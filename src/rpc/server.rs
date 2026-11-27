use crate::rpc::schema::{Method, RpcRequest, RpcResponse, make_error, make_result, parse_method};
use crate::supervisor::SupervisorCore;
use anyhow::Result;
use serde_json::json;
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;

// issue #20: shutdown signal (RPC Shutdown arm fires → run_daemon select receives → graceful exit).
// UDS face is unix-only — windows has no UnixStream (the daemon uses the HTTP API face, gated in main.rs).
#[cfg(unix)]
pub async fn serve(
    core: std::sync::Arc<tokio::sync::Mutex<SupervisorCore>>,
    socket_path: &str,
    token: Option<String>,
) -> Result<()> {
    serve_with_shutdown(
        core,
        socket_path,
        token,
        std::sync::Arc::new(tokio::sync::Mutex::new(None)),
    )
    .await
}

// serve with a shutdown tx: the Shutdown arm sends the signal, run_daemon watches rx.
// issue #25: tx is now a shared Arc — the HTTP face (http_server) and the UDS face share the same signal.
// issue #114: bind moved up to the run_daemon entry — bind failure = fatal (exit 1,
// launchd KeepAlive retries), no more faceless idling tick (proven by orphan-daemon co-running).
#[cfg(unix)]
pub fn bind_uds(socket_path: &str) -> Result<UnixListener> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(
        socket_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    Ok(listener)
}

#[cfg(unix)]
pub async fn serve_with_shutdown(
    core: std::sync::Arc<tokio::sync::Mutex<SupervisorCore>>,
    socket_path: &str,
    token: Option<String>,
    shutdown_tx: std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
) -> Result<()> {
    let listener = bind_uds(socket_path)?;
    serve_with_shutdown_on(core, listener, token, shutdown_tx).await
}

#[cfg(unix)]
pub async fn serve_with_shutdown_on(
    core: std::sync::Arc<tokio::sync::Mutex<SupervisorCore>>,
    listener: UnixListener,
    token: Option<String>,
    shutdown_tx: std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
) -> Result<()> {
    tracing::info!("rpc server listening (0600)");
    loop {
        let (stream, _) = listener.accept().await?;
        let core = core.clone();
        let token = token.clone();
        let tx = shutdown_tx.clone();
        tokio::spawn(handle_conn(stream, core, token, tx));
    }
}

#[cfg(unix)]
async fn handle_conn(
    stream: tokio::net::UnixStream,
    core: std::sync::Arc<tokio::sync::Mutex<SupervisorCore>>,
    token: Option<String>,
    shutdown_tx: std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let resp = match serde_json::from_str::<RpcRequest>(&line) {
            Ok(req) => dispatch(req, &core, &token, &shutdown_tx).await,
            Err(e) => make_error(-32700, &format!("parse error: {e}"), 0),
        };
        let mut out =
            serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":{"code":-32603}}"#.into());
        out.push('\n');
        if writer.write_all(out.as_bytes()).await.is_err() {
            break;
        }
    }
}

async fn dispatch(
    req: RpcRequest,
    core: &std::sync::Arc<tokio::sync::Mutex<SupervisorCore>>,
    token: &Option<String>,
    shutdown_tx: &std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
) -> RpcResponse {
    if let Some(expected) = token {
        let got = req
            .params
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if got != expected {
            return make_error(-32001, "unauthorized", req.id);
        }
    }
    match parse_method(&req.method) {
        None => make_error(-32601, &format!("method not found: {}", req.method), req.id),
        Some(Method::Ping) => make_result(json!("pong"), req.id),
        Some(Method::Status) => {
            let c = core.lock().await;
            let entries: Vec<_> = c
                .status_all()
                .into_iter()
                .map(|e| {
                    json!({
                        "name": e.name, "state": e.state, "port": e.port, "plane": e.plane,
                        // issue #112: most recent restart-class event (CLI status table column)
                        "last_restart": e.last_restart,
                    })
                })
                .collect();
            make_result(json!(entries), req.id)
        }
        Some(Method::Up) => {
            // issue #31: the service argument was previously ignored (up <svc> actually ran the fleet-wide up_all).
            // With service → single-service up_one; without → fleet-wide up_all.
            // 2026-10-31: up records its origin symmetrically with down — the previous 01:05 fleet-wide
            // start wave left zero trace in logs/event table, trigger unattributable (down already had
            // the same via #82).
            let origin = origin_of(&req.params, "uds client (origin not provided)");
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let svc_param = req
                .params
                .get("service")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            if let Some(svc) = svc_param {
                // issue #122: single-service exec outside the lock — prepare (locked) → run (unlocked) →
                // finish (locked). The old implementation held the lock waiting for start.sh (go build can
                // take minutes), queueing all RPC/HTTP (observed 2026-11-11).
                let job = {
                    let mut c = core.lock().await;
                    tracing::info!(svc = %svc, origin = %origin, "manual start (operator intent)");
                    if let Err(e) =
                        c.store
                            .record_event(&svc, "ManualStart", &origin, now_ts, "native", "info")
                    {
                        tracing::warn!(err = %e, "ManualStart event write fail");
                    }
                    match c.prepare_up_one(&svc).await {
                        Err(e) => return make_error(-32000, &e.to_string(), req.id),
                        Ok(None) => return make_result(json!("ok"), req.id),
                        Ok(Some(job)) => job,
                    }
                };
                let res = job.run().await;
                let finish = {
                    let mut c = core.lock().await;
                    c.finish_up_one(&svc, res).await
                };
                return match finish {
                    Ok(()) => make_result(json!("ok"), req.id),
                    Err(e) => make_error(-32000, &e.to_string(), req.id),
                };
            }
            // fleet-wide START-ALL (lock semantics unchanged — explicit whole-fleet operation, with banner)
            let mut c = core.lock().await;
            let res = {
                {
                    // prominent banner: fleet-wide start waves mirror stop-all; per-line logs are easy to miss
                    tracing::warn!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
                    tracing::warn!("!! START-ALL requested — entire fleet will be started");
                    tracing::warn!("!! origin: {origin}");
                    tracing::warn!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
                    if let Err(e) = c
                        .store
                        .record_event("*", "StartAll", &origin, now_ts, "native", "info")
                    {
                        tracing::warn!(err = %e, "StartAll event write fail");
                    }
                    c.up_all().await
                }
            };
            match res {
                Ok(()) => make_result(json!("ok"), req.id),
                Err(e) => make_error(-32000, &e.to_string(), req.id),
            }
        }
        Some(Method::Down) => {
            // issue #31: same as Up — the service argument was previously ignored, so down <svc> stopped
            // everything, dangerous.
            // issue #82: stopping is operator intent, the origin must be recorded (CLI argv / HTTP route+peer /
            // UDS direct) and distinguished from probe failures — the stop-all wave gets a prominent banner + event rows.
            let origin = origin_of(&req.params, "uds client (origin not provided)");
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let svc_param = req
                .params
                .get("service")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            if let Some(svc) = svc_param {
                // issue #122: single-service stop exec outside the lock (same as the Up arm)
                let job = {
                    let mut c = core.lock().await;
                    tracing::warn!(svc = %svc, origin = %origin, "manual stop (operator intent)");
                    if let Err(e) =
                        c.store
                            .record_event(&svc, "ManualStop", &origin, now_ts, "native", "warn")
                    {
                        tracing::warn!(err = %e, "ManualStop event write fail");
                    }
                    match c.prepare_down_one(&svc).await {
                        Err(e) => return make_error(-32000, &e.to_string(), req.id),
                        Ok(job) => job,
                    }
                };
                let res = job.run().await;
                let finish = {
                    let mut c = core.lock().await;
                    c.finish_down_one(&svc, res).await
                };
                return match finish {
                    Ok(()) => make_result(json!("ok"), req.id),
                    Err(e) => make_error(-32000, &e.to_string(), req.id),
                };
            }
            // fleet-wide STOP-ALL (lock semantics unchanged — explicit whole-fleet operation, with banner)
            let mut c = core.lock().await;
            let res = {
                {
                    // prominent banner: fleet-wide stop waves are easy to miss in per-line logs (2026-10-30 22:53 incident)
                    tracing::warn!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
                    tracing::warn!("!! STOP-ALL requested — entire fleet will be stopped");
                    tracing::warn!("!! fleet stays DOWN until manually restarted");
                    tracing::warn!("!! origin: {origin}");
                    tracing::warn!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!");
                    if let Err(e) = c
                        .store
                        .record_event("*", "StopAll", &origin, now_ts, "native", "warn")
                    {
                        tracing::warn!(err = %e, "StopAll event write fail");
                    }
                    // issue #128: per-service ManualStop on stop-all — previously only one StopAll("*")
                    // aggregate event was written, leaving stale last_restart notes per service (e.g. AutoRestart
                    // "restart ok"), making fleet-drift stop attribution impossible
                    let was_running: Vec<String> = c
                        .runners
                        .iter()
                        .filter(|r| !matches!(r.state, crate::supervisor::runner::State::Stopped))
                        .map(|r| r.def.name.clone())
                        .collect();
                    let res = c.down_all().await;
                    for svc in &was_running {
                        if let Err(e) = c.store.record_event(
                            svc,
                            "ManualStop",
                            &origin,
                            now_ts,
                            "native",
                            "warn",
                        ) {
                            tracing::warn!(svc = %svc, err = %e, "ManualStop event write fail");
                        }
                    }
                    res
                }
            };
            match res {
                Ok(()) => make_result(json!("ok"), req.id),
                Err(e) => make_error(-32000, &e.to_string(), req.id),
            }
        }
        Some(Method::Restart) => {
            let svc = match req.params.get("service").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return make_error(-32602, "invalid params: service required", req.id),
            };
            // issue #96: restart must stop the old process and verify port release before starting —
            // previously only invoke_start ran, leaving the old listener alive while the new process died
            // on the port collision; the probe hit the old process and returned "ok" without replacing code.
            // The result carries old/new pidfile PIDs as replacement evidence.
            // issue #122: stop/start exec all outside the lock; only verify (bounded probing) holds it.
            let (old_pid, stop_job) = {
                let mut c = core.lock().await;
                match c.prepare_restart(&svc).await {
                    Err(e) => {
                        let code = if e.to_string().starts_with("unknown service") {
                            -32602
                        } else {
                            -32000
                        };
                        return make_error(code, &e.to_string(), req.id);
                    }
                    Ok(v) => v,
                }
            };
            let stop_res = stop_job.run().await;
            let jobs = {
                let mut c = core.lock().await;
                match c.restart_after_stop(&svc, stop_res).await {
                    Err(e) => {
                        tracing::warn!(svc = %svc, err = %e, "manual restart fail (verify)");
                        return make_error(-32000, &format!("restart fail: {e}"), req.id);
                    }
                    Ok(jobs) => jobs,
                }
            };
            let mut start_res = Ok(());
            for job in &jobs {
                if let Err(e) = job.run().await {
                    start_res = Err(e);
                    break;
                }
            }
            let out = {
                let mut c = core.lock().await;
                c.finish_restart(&svc, old_pid, start_res).await
            };
            match out {
                Ok((old_pid, new_pid)) => {
                    tracing::info!(svc = %svc, old_pid, new_pid, "manual restart ok");
                    make_result(
                        json!({"ok": true, "old_pid": old_pid, "new_pid": new_pid}),
                        req.id,
                    )
                }
                Err(e) => {
                    tracing::warn!(svc = %svc, err = %e, "manual restart fail");
                    make_error(-32000, &format!("restart fail: {e}"), req.id)
                }
            }
        }
        Some(Method::Logs) => {
            let svc = match req.params.get("service").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return make_error(-32602, "invalid params: service required", req.id),
            };
            let c = core.lock().await;
            // compose containers first: use compose logs
            if let Some(res) = c.compose_logs(&svc) {
                match res {
                    Ok(text) => {
                        return make_result(json!({"lines": text, "plane": "compose"}), req.id);
                    }
                    Err(e) => {
                        return make_error(-32000, &format!("compose logs fail: {e}"), req.id);
                    }
                }
            }
            // native: tail 50 lines of the per-service log file
            let log_dir = std::env::var("HOME")
                .map(|h| format!("{h}/.fusion-sv/logs"))
                .unwrap_or_else(|_| "/tmp/fusion-sv-logs".into());
            let dir = crate::manifest::name_to_dir(&svc);
            let path = std::path::Path::new(&log_dir).join(format!("{dir}.log"));
            if !path.exists() {
                return make_error(-32000, &format!("no log for {svc}"), req.id);
            }
            let out = match std::process::Command::new("tail")
                .args(["-n", "50"])
                .arg(&path)
                .output()
            {
                Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
                Err(e) => return make_error(-32000, &format!("tail fail: {e}"), req.id),
            };
            make_result(json!({"lines": out, "plane": "native"}), req.id)
        }
        Some(Method::Drain) => {
            // issue #9: drain a single service (params.service) or all (no service).
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut c = core.lock().await;
            match req.params.get("service").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => match c.drain(s, now_ts) {
                    Ok(()) => make_result(json!("ok"), req.id),
                    Err(e) => make_error(-32000, &e.to_string(), req.id),
                },
                _ => match c.drain_all(now_ts) {
                    Ok(()) => make_result(json!("ok"), req.id),
                    Err(e) => make_error(-32000, &e.to_string(), req.id),
                },
            }
        }
        Some(Method::Rollout) => {
            // issue #9: zero-downtime rollout of a single service (drain→stop→start→prewarm→clear drain).
            let svc = match req.params.get("service").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return make_error(-32602, "invalid params: service required", req.id),
            };
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut c = core.lock().await;
            let grace = c.cfg.drain_grace_sec;
            let prewarm_timeout = c.cfg.rollout_prewarm_timeout_sec;
            match c.rollout(&svc, now_ts, grace, prewarm_timeout).await {
                Ok(()) => {
                    tracing::info!(svc = %svc, "rollout ok (prewarm healthy)");
                    make_result(json!("ok"), req.id)
                }
                Err(e) => {
                    tracing::warn!(svc = %svc, err = %e, "rollout fail");
                    make_error(-32000, &format!("rollout fail: {e}"), req.id)
                }
            }
        }
        // issue #20: Top is a client-side command (cli_top loops cli_status), no server RPC needed.
        Some(Method::Top) => make_error(
            -32601,
            "top is a client-side command, run `fusion-sv top` directly",
            req.id,
        ),
        // issue #20: graceful remote daemon shutdown. Send the shutdown signal → run_daemon select receives → exit.
        Some(Method::Shutdown) => {
            let mut guard = shutdown_tx.lock().await;
            let usable = guard.as_ref().map(|tx| !tx.is_closed()).unwrap_or(false);
            if usable {
                // take the tx and send; set None to prevent re-triggering (idempotent: a second shutdown returns unavailable).
                if let Some(tx) = guard.take()
                    && tx.send(()).is_err()
                {
                    tracing::warn!("shutdown signal recv already dropped");
                }
                tracing::info!("shutdown requested via rpc");
                make_result(json!("ok"), req.id)
            } else {
                make_error(-32000, "shutdown channel unavailable", req.id)
            }
        }
        Some(Method::Backup) => {
            // issue #10: one-shot backup (Postgres + Qdrant → FUSION_BACKUP_TARGET).
            // issue #113: switched to fire-and-forget — the old synchronous implementation held the lock
            // ~15min, timing out the RPC client and leaving the whole API unresponsive. Spawns a detached
            // task and returns immediately; outcomes land in the event table + alerts (BackupFailed),
            // completion in the INFO log.
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut c = core.lock().await;
            c.spawn_backup(now_ts);
            tracing::info!("backup started (rpc, detached)");
            make_result(json!({"ok": true, "started": true}), req.id)
        }
        // PRD app-tiering (issue #62): boot-tier startup face
        Some(Method::Boot) => {
            let c = core.lock().await;
            make_result(json!(c.boot_status()), req.id)
        }
        Some(Method::BootPin) => {
            let svc = match req.params.get("service").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return make_error(-32602, "invalid params: service required", req.id),
            };
            let mode = match req.params.get("mode").and_then(|v| v.as_str()) {
                Some("auto") => Some(crate::store::PinMode::Auto),
                Some("manual") => Some(crate::store::PinMode::Manual),
                Some("clear") => None,
                _ => {
                    return make_error(
                        -32602,
                        "invalid params: mode must be auto|manual|clear",
                        req.id,
                    );
                }
            };
            let mut c = core.lock().await;
            match c.set_boot_pin(&svc, mode) {
                Ok(()) => {
                    tracing::info!(svc = %svc, "boot pin updated (rpc)");
                    make_result(json!("ok"), req.id)
                }
                Err(e) => make_error(-32000, &e.to_string(), req.id),
            }
        }
        Some(Method::BootGroup) => {
            let group = match req.params.get("group").and_then(|v| v.as_str()) {
                Some(g) => g.to_string(),
                _ => return make_error(-32602, "invalid params: group required", req.id),
            };
            let action = match req.params.get("action").and_then(|v| v.as_str()) {
                Some(a) => a.to_string(),
                _ => return make_error(-32602, "invalid params: action required", req.id),
            };
            let mut c = core.lock().await;
            // issue #82: group stop waves are also operator intent, log with origin
            let origin = origin_of(&req.params, "uds client (origin not provided)");
            match c.boot_group(&group, &action).await {
                Ok(summary) => {
                    tracing::info!(group = %group, action = %action, origin = %origin, "boot group done (rpc)");
                    make_result(summary, req.id)
                }
                Err(e) => make_error(-32000, &e.to_string(), req.id),
            }
        }
    }
}

// issue #25: the HTTP face reuses the UDS dispatch — same token semantics, same orchestration logic,
// no rewriting of up/down/restart/drain at the HTTP layer.
pub async fn dispatch_http(
    method: &str,
    params: serde_json::Value,
    core: &std::sync::Arc<tokio::sync::Mutex<SupervisorCore>>,
    token: &Option<String>,
    shutdown_tx: &std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
) -> RpcResponse {
    dispatch(
        RpcRequest {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
            id: 1,
        },
        core,
        token,
        shutdown_tx,
    )
    .await
}

// issue #82: stop-command origin resolution — an explicit client origin (CLI argv / HTTP route) wins,
// missing falls back to the caller-face default (UDS direct connections cannot read the peer process, marked as unknown origin).
fn origin_of(params: &serde_json::Value, fallback: &str) -> String {
    params
        .get("origin")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::origin_of;

    #[test]
    fn test_origin_of_prefers_explicit() {
        let params = serde_json::json!({"origin": "cli pid=1 argv: fusion-sv down"});
        assert_eq!(
            origin_of(&params, "uds client"),
            "cli pid=1 argv: fusion-sv down"
        );
    }

    #[test]
    fn test_origin_of_falls_back() {
        assert_eq!(
            origin_of(&serde_json::json!({}), "uds client"),
            "uds client"
        );
        assert_eq!(
            origin_of(&serde_json::Value::Null, "uds client"),
            "uds client"
        );
        // empty string counts as missing
        assert_eq!(
            origin_of(&serde_json::json!({"origin": ""}), "uds client"),
            "uds client"
        );
    }
}
