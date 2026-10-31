use crate::rpc::schema::{Method, RpcRequest, RpcResponse, make_error, make_result, parse_method};
use crate::supervisor::SupervisorCore;
use anyhow::Result;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

// issue #20: shutdown 信号 (RPC Shutdown arm 触发 → run_daemon select 接收 → 优雅退出)。
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

// 带 shutdown tx 的 serve: Shutdown arm 发送信号, run_daemon 监听 rx。
// issue #25: tx 改为共享 Arc — HTTP 面 (http_server) 与 UDS 面共用同一信号。
pub async fn serve_with_shutdown(
    core: std::sync::Arc<tokio::sync::Mutex<SupervisorCore>>,
    socket_path: &str,
    token: Option<String>,
    shutdown_tx: std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
) -> Result<()> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(
        socket_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    tracing::info!(sock = %socket_path, "rpc server listening (0600)");
    loop {
        let (stream, _) = listener.accept().await?;
        let core = core.clone();
        let token = token.clone();
        let tx = shutdown_tx.clone();
        tokio::spawn(handle_conn(stream, core, token, tx));
    }
}

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
                    })
                })
                .collect();
            make_result(json!(entries), req.id)
        }
        Some(Method::Up) => {
            // issue #31: service 参数此前被忽略 (up <svc> 实际跑全量 up_all)。
            // 有 service → 单服务 up_one; 无 → 全量 up_all。
            // 2026-10-31: up 与 down 对称记录来源 — 此前 01:05 全 fleet 启动波
            // 在日志/事件表零痕迹, 无法定位触发方 (down 侧 #82 已有同款)。
            let origin = origin_of(&req.params, "uds client (origin not provided)");
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut c = core.lock().await;
            let res = match req.params.get("service").and_then(|v| v.as_str()) {
                Some(svc) if !svc.is_empty() => {
                    tracing::info!(svc = %svc, origin = %origin, "manual start (operator intent)");
                    if let Err(e) =
                        c.store
                            .record_event(svc, "ManualStart", &origin, now_ts, "native", "info")
                    {
                        tracing::warn!(err = %e, "ManualStart event write fail");
                    }
                    c.up_one(svc).await
                }
                _ => {
                    // 醒目 banner: 全量启动波与 stop-all 对称, 逐行日志易漏看
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
            // issue #31: 同 Up — service 参数此前被忽略, down <svc> 会全停, 危险。
            // issue #82: 停止是操作员意图, 必须记录来源 (CLI argv / HTTP 路由+对端 /
            // UDS 直连) 并与探活失败区分 — stop-all 全停波打醒目 banner + 事件表。
            let origin = origin_of(&req.params, "uds client (origin not provided)");
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut c = core.lock().await;
            let res = match req.params.get("service").and_then(|v| v.as_str()) {
                Some(svc) if !svc.is_empty() => {
                    tracing::warn!(svc = %svc, origin = %origin, "manual stop (operator intent)");
                    if let Err(e) =
                        c.store
                            .record_event(svc, "ManualStop", &origin, now_ts, "native", "warn")
                    {
                        tracing::warn!(err = %e, "ManualStop event write fail");
                    }
                    c.down_one(svc).await
                }
                _ => {
                    // 醒目 banner: 全停波普通逐行日志易漏看 (2026-10-30 22:53 事故)
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
                    c.down_all().await
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
            let mut c = core.lock().await;
            let runner = match c.runners.iter_mut().find(|r| r.def.name == svc) {
                Some(r) => r,
                None => return make_error(-32602, &format!("unknown service: {svc}"), req.id),
            };
            runner.policy.reset();
            runner.state = crate::supervisor::runner::State::Stopped;
            match runner.invoke_start() {
                Ok(()) => {
                    runner.state = crate::supervisor::runner::State::Starting;
                    tracing::info!(svc = %svc, "manual restart ok");
                    make_result(json!("ok"), req.id)
                }
                Err(e) => {
                    tracing::warn!(svc = %svc, err = %e, "manual restart start.sh fail");
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
            // compose 容器优先: 走 compose logs
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
            // native: 读每服务日志文件 tail 50
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
            // issue #9: drain 单服务 (params.service) 或全部 (无 service)。
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
            // issue #9: 零停机换版单服务 (drain→stop→start→prewarm→清 drain)。
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
        // issue #20: Top 是客户端命令 (cli_top 循环 cli_status), 无需 server RPC。
        Some(Method::Top) => make_error(
            -32601,
            "top is a client-side command, run `fusion-sv top` directly",
            req.id,
        ),
        // issue #20: 优雅远程关停 daemon。发送 shutdown 信号 → run_daemon select 接收 → 退出。
        Some(Method::Shutdown) => {
            let mut guard = shutdown_tx.lock().await;
            let usable = guard.as_ref().map(|tx| !tx.is_closed()).unwrap_or(false);
            if usable {
                // 取出 tx 发送; 置 None 防重复触发 (幂等: 再次 shutdown 返回 unavailable)。
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
            // issue #10: 一次性备份 (Postgres + Qdrant → FUSION_BACKUP_TARGET)。
            // fail-closed: target 未设 → Err + BackupFailed 告警。
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut c = core.lock().await;
            match c.run_backup(now_ts).await {
                Ok(()) => {
                    tracing::info!("backup ok (rpc)");
                    make_result(json!("ok"), req.id)
                }
                Err(e) => {
                    tracing::warn!(err = %e, "backup fail (rpc, fail-closed 已告警)");
                    make_error(-32000, &format!("backup fail: {e}"), req.id)
                }
            }
        }
        // PRD app-tiering (issue #62): boot 分级启动面
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
            // issue #82: 分组停止波也是操作员意图, 日志带来源
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

// issue #25: HTTP 面复用 UDS dispatch — 同一 token 语义、同一编排逻辑,
// 不在 HTTP 层重写 up/down/restart/drain。
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

// issue #82: 停止命令来源解析 — 客户端显式带 origin (CLI argv / HTTP 路由) 优先,
// 缺失回退调用方面默认值 (UDS 直连无法取对端进程, 标记为未知来源)。
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
        // 空串视为缺失
        assert_eq!(
            origin_of(&serde_json::json!({"origin": ""}), "uds client"),
            "uds client"
        );
    }
}
