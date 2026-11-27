// issue #22 / #26 / #25: supervisor HTTP face (same listener as metrics).
// Routes:
//   GET  /metrics                Prometheus text (issue #22)
//   GET  /health                 {"ok":true} (shell module health probe)
//   GET  /api/v1/status          read-only JSON status, mirrors UDS status (issue #26)
//   GET  /api/v1/history?service=&limit=   per-service health history (issue #25)
//   GET  /api/v1/events?limit=            global event stream, alerts-list data source (issue #25)
//   GET  /api/v1/logs?service=            log tail (reuses RPC dispatch)
//   GET  /api/v1/boot                     boot tier view (issue #62, read-only, no token)
//   POST /api/v1/boot/pin|boot/group      pin override / group batch (issue #62, token)
//   POST /api/v1/{up,down,restart,drain,rollout,backup,shutdown}
//                                orchestration ops — reuse UDS dispatch, same token semantics
//                                (issue #25; Authorization: Bearer / x-token)
//   POST /api/v1/alerts/ingest   external alert ingestion (issue #139, token; event persisted +
//                                optional webhook forward, dedup_key optional)
//   GET  other paths             web console static files (web_dir config, issue #25)
// Security boundary: listener binds loopback (metrics_addr, default 127.0.0.1:9100);
// mutation token validation is identical to UDS (FUSION_SV_TOKEN).
// Lock discipline: short lock hold (lock → render → release), never hold across await.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use crate::supervisor::SupervisorCore;

// Read request cap: request line + headers + JSON body (orchestration params are small).
const MAX_REQUEST_BYTES: usize = 8192;

pub type SharedShutdownTx = Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>;

// issue #114: bind moved up to the run_daemon entry — bind failure = fatal, no more faceless idle loop.
pub async fn bind_tcp(addr: &str) -> anyhow::Result<tokio::net::TcpListener> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr, "http server listening (metrics + status + web)");
    Ok(listener)
}

pub async fn serve(
    core: Arc<tokio::sync::Mutex<SupervisorCore>>,
    addr: &str,
    token: Option<String>,
    web_dir: Option<String>,
    shutdown_tx: SharedShutdownTx,
) -> anyhow::Result<()> {
    let listener = bind_tcp(addr).await?;
    serve_on(core, listener, token, web_dir, shutdown_tx).await
}

pub async fn serve_on(
    core: Arc<tokio::sync::Mutex<SupervisorCore>>,
    listener: tokio::net::TcpListener,
    token: Option<String>,
    web_dir: Option<String>,
    shutdown_tx: SharedShutdownTx,
) -> anyhow::Result<()> {
    let web_dir = web_dir.map(PathBuf::from);
    loop {
        match listener.accept().await {
            Ok((mut stream, peer)) => {
                let core = core.clone();
                let token = token.clone();
                let web_dir = web_dir.clone();
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(
                        &mut stream,
                        &peer.to_string(),
                        &core,
                        &token,
                        &web_dir,
                        &shutdown_tx,
                    )
                    .await
                    {
                        tracing::debug!(peer = %peer, err = %e, "http conn error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(err = %e, "http accept error");
            }
        }
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    // Authorization: Bearer <t> or x-token: <t>
    token: String,
    body: String,
}

fn parse_request(raw: &str) -> Request {
    let mut lines = raw.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut token = String::new();
    let mut body = String::new();
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("authorization: bearer ") {
            // rest is the part after the prefix — slice the original line at the same position.
            let start = line.len() - rest.len();
            token = line[start..].trim().to_string();
        } else if lower.starts_with("x-token:") {
            token = line[8..].trim().to_string();
        }
    }
    if let Some((_, b)) = raw.split_once("\r\n\r\n") {
        body = b.to_string();
    }
    Request {
        method,
        path,
        query,
        token,
        body,
    }
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

pub async fn handle_conn(
    stream: &mut tokio::net::TcpStream,
    peer: &str,
    core: &Arc<tokio::sync::Mutex<SupervisorCore>>,
    token: &Option<String>,
    web_dir: &Option<PathBuf>,
    shutdown_tx: &SharedShutdownTx,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0u8; MAX_REQUEST_BYTES];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let req = parse_request(&String::from_utf8_lossy(&buf[..n]));

    let resp = route(&req, peer, core, token, web_dir, shutdown_tx).await;
    stream.write_all(resp.as_bytes()).await?;
    Ok(())
}

async fn route(
    req: &Request,
    peer: &str,
    core: &Arc<tokio::sync::Mutex<SupervisorCore>>,
    token: &Option<String>,
    web_dir: &Option<PathBuf>,
    shutdown_tx: &SharedShutdownTx,
) -> String {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/metrics") => {
            let body = {
                let c = core.lock().await;
                crate::metrics::render(&c)
            };
            http_text("200 OK", "text/plain; version=0.0.4; charset=utf-8", &body)
        }
        ("GET", "/health") => http_text("200 OK", "application/json", "{\"ok\":true}"),
        ("GET", "/api/v1/status") => {
            let body = {
                let c = core.lock().await;
                crate::metrics::render_status_json(&c)
            };
            http_text("200 OK", "application/json", &body)
        }
        // PRD app-tiering (issue #62): boot tier view — GET is read-only without token
        // (aligned with status); POST pin/group goes through dispatch_http token validation.
        ("GET", "/api/v1/boot") => {
            let body = {
                let c = core.lock().await;
                serde_json::to_string(&c.boot_status()).unwrap_or_else(|_| "[]".into())
            };
            http_text("200 OK", "application/json", &body)
        }
        ("POST", "/api/v1/boot/pin") => {
            // issue #82: boot-plane requests carry origin too
            let Some(mut params) = boot_post_params(&req.body, &req.token) else {
                return http_text(
                    "400 Bad Request",
                    "application/json",
                    "{\"error\":\"body must be JSON\"}",
                );
            };
            params["origin"] = json!(format!("http POST /api/v1/boot/pin from {peer}"));
            let resp =
                crate::rpc::server::dispatch_http("boot.pin", params, core, token, shutdown_tx)
                    .await;
            let status = if resp.error.is_some() {
                "500 Internal Server Error"
            } else {
                "200 OK"
            };
            http_text(
                status,
                "application/json",
                &serde_json::to_string(&resp).unwrap_or_default(),
            )
        }
        ("POST", "/api/v1/boot/group") => {
            // issue #82: group stop waves are operator intent too; origin kept traceable
            let Some(mut params) = boot_post_params(&req.body, &req.token) else {
                return http_text(
                    "400 Bad Request",
                    "application/json",
                    "{\"error\":\"body must be JSON\"}",
                );
            };
            params["origin"] = json!(format!("http POST /api/v1/boot/group from {peer}"));
            let resp =
                crate::rpc::server::dispatch_http("boot.group", params, core, token, shutdown_tx)
                    .await;
            let status = if resp.error.is_some() {
                "500 Internal Server Error"
            } else {
                "200 OK"
            };
            http_text(
                status,
                "application/json",
                &serde_json::to_string(&resp).unwrap_or_default(),
            )
        }
        ("GET", "/api/v1/history") => {
            let service = query_param(&req.query, "service").unwrap_or_default();
            if service.is_empty() {
                return http_text(
                    "400 Bad Request",
                    "application/json",
                    "{\"error\":\"service is required\"}",
                );
            }
            let limit: u32 = query_param(&req.query, "limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(120)
                .min(1000);
            let body = {
                let c = core.lock().await;
                match c.store.recent_health(&service, limit) {
                    Ok(rows) => {
                        let items: Vec<_> = rows
                            .iter()
                            .map(|r| {
                                json!({
                                    "ts": r.ts, "service": r.service,
                                    "status": r.status, "latency_ms": r.latency_ms,
                                    "plane": r.plane,
                                })
                            })
                            .collect();
                        json!({"ok": true, "history": items}).to_string()
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "history query fail");
                        json!({"ok": false, "error": e.to_string()}).to_string()
                    }
                }
            };
            http_text("200 OK", "application/json", &body)
        }
        ("GET", "/api/v1/events") => {
            let limit: u32 = query_param(&req.query, "limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(100)
                .min(1000);
            let body = {
                let c = core.lock().await;
                match c.store.recent_events_all(limit) {
                    Ok(rows) => {
                        let items: Vec<_> = rows
                            .iter()
                            .map(|r| {
                                json!({
                                    "ts": r.ts, "service": r.service,
                                    "kind": r.kind, "detail": r.detail,
                                    "plane": r.plane, "severity": r.severity,
                                })
                            })
                            .collect();
                        json!({"ok": true, "events": items}).to_string()
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "events query fail");
                        json!({"ok": false, "error": e.to_string()}).to_string()
                    }
                }
            };
            http_text("200 OK", "application/json", &body)
        }
        // issue #139: external alert ingestion — token semantics match orchestration ops
        // (must match when configured; 401 does not record an event). Events go straight
        // into history.db (plane="external"), optionally forwarded via an Alerter webhook
        // (separate instance without a sink, so events are not double-recorded).
        // Under the lock: dedup + persist only.
        ("POST", "/api/v1/alerts/ingest") => {
            if let Some(expected) = token
                && expected != &req.token
            {
                return http_text(
                    "401 Unauthorized",
                    "application/json",
                    "{\"error\":\"invalid token\"}",
                );
            }
            let payload = match crate::ingest::IngestPayload::parse(&req.body) {
                Ok(p) => p,
                Err(e) => {
                    return http_text(
                        "400 Bad Request",
                        "application/json",
                        &json!({"error": e}).to_string(),
                    );
                }
            };
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let (passed, alert_url, dedup_sec) = {
                let mut c = core.lock().await;
                let window = c.cfg.alert_dedup_sec;
                let ok = c
                    .ingest_gate
                    .observe(payload.dedup_key.as_deref(), ts, window);
                if ok
                    && let Err(e) = c.store.record_event(
                        &payload.source,
                        &payload.kind,
                        &payload.detail,
                        ts,
                        "external",
                        &payload.severity,
                    )
                {
                    // roll back the dedup cursor — a wasted window would swallow legitimate retries
                    c.ingest_gate.rollback(payload.dedup_key.as_deref());
                    tracing::warn!(err = %e, "ingest event record fail");
                    return http_text(
                        "500 Internal Server Error",
                        "application/json",
                        "{\"error\":\"record fail\"}",
                    );
                }
                (ok, c.cfg.alert_url.clone(), c.cfg.alert_dedup_sec)
            };
            tracing::info!(
                source = %payload.source,
                kind = %payload.kind,
                severity = %payload.severity,
                passed,
                "external alert ingested"
            );
            if !passed {
                return http_text(
                    "200 OK",
                    "application/json",
                    "{\"ok\":true,\"deduped\":true}",
                );
            }
            // webhook forward is fire-and-forget outside the lock (no network I/O under the lock)
            if !alert_url.is_empty() {
                let alert = crate::alert::Alert {
                    service: payload.source.clone(),
                    kind: crate::alert::AlertKind::External,
                    detail: format!("[{}] {}", payload.kind, payload.detail),
                    ts,
                };
                tokio::spawn(async move {
                    let mut alerter = crate::alert::Alerter::new(dedup_sec, alert_url);
                    if let Err(e) = alerter.alert(alert).await {
                        tracing::warn!(err = %e, "ingest webhook forward fail");
                    }
                });
            }
            http_text("200 OK", "application/json", "{\"ok\":true}")
        }
        // Orchestration ops (POST) and logs (GET): reuse UDS dispatch, same token semantics.
        (m, p) if p.starts_with("/api/v1/") => {
            let method = p.trim_start_matches("/api/v1/");
            let is_get_rpc = m == "GET" && (method == "logs" || method == "status");
            if !(m == "POST" || is_get_rpc) {
                return http_text("404 Not Found", "text/plain", "");
            }
            // HTTP credentials (Bearer/x-token) → RPC params.token (validated by dispatch).
            let mut params: serde_json::Value = if req.body.trim().is_empty() {
                json!({})
            } else {
                match serde_json::from_str(&req.body) {
                    Ok(v) => v,
                    Err(_) => {
                        return http_text(
                            "400 Bad Request",
                            "application/json",
                            "{\"error\":\"body must be JSON\"}",
                        );
                    }
                }
            };
            // Merge GET query params into params (logs?service=x).
            if m == "GET" {
                for kv in req.query.split('&').filter(|s| !s.is_empty()) {
                    if let Some((k, v)) = kv.split_once('=') {
                        params[k] = json!(v);
                    }
                }
            }
            if !req.token.is_empty() {
                params["token"] = json!(req.token);
            }
            // issue #82: HTTP orchestration ops keep a traceable origin (method+path+peer);
            // issue #173: enrich the peer with the caller identity (lsof resolves the
            // ephemeral port owner's pid+process name, best-effort — a bare port number is
            // not attributable after the fact; two mistaken fusion-mlx bench kills proved it).
            // Do not overwrite an origin supplied in the request body.
            if params
                .get("origin")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty()
            {
                params["origin"] =
                    json!(format!("http {} {} from {}", m, p, caller_identity(peer)));
            }
            let resp =
                crate::rpc::server::dispatch_http(method, params, core, token, shutdown_tx).await;
            let status = if resp.error.is_some() {
                "500 Internal Server Error"
            } else {
                "200 OK"
            };
            http_text(
                status,
                "application/json",
                &serde_json::to_string(&resp).unwrap_or_default(),
            )
        }
        // Remaining GETs: web console static files (when web_dir is configured).
        ("GET", _) | ("HEAD", _) => match web_dir {
            Some(dir) => serve_static(dir, &req.path).await,
            None => http_text("404 Not Found", "text/plain", ""),
        },
        _ => http_text("404 Not Found", "text/plain", ""),
    }
}

fn http_text(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

// issue #173: peer port → caller identity (pid + process name). lsof resolves the TCP port
// (a local ephemeral port is unique enough); failure/timeout/no match = return the peer
// address only (best-effort, never reject a request because attribution failed).
// unix only; windows has no lsof and returns the peer address directly.
fn caller_identity(peer: &str) -> String {
    #[cfg(unix)]
    {
        let port = match peer.rsplit(':').next().unwrap_or("").parse::<u32>() {
            Ok(p) => p,
            Err(_) => return peer.to_string(),
        };
        let lsof = std::process::Command::new("lsof")
            .args(["-t", &format!("-iTCP:{port}")])
            .output();
        let Ok(out) = lsof else {
            return peer.to_string();
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let pid_str = stdout.lines().next().unwrap_or("").trim();
        let Ok(pid) = pid_str.parse::<i32>() else {
            return peer.to_string();
        };
        let comm = std::process::Command::new("ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if comm.is_empty() {
            format!("{peer} (pid {pid})")
        } else {
            format!("{peer} (pid {pid} {comm})")
        }
    }
    #[cfg(not(unix))]
    {
        let _ = peer;
        peer.to_string()
    }
}

// boot POST endpoint params: JSON body + Bearer/x-token → params.token (same semantics as
// the catch-all). A non-JSON body returns None → caller replies 400 (consistent with the
// catch-all POST validation, not silently treated as empty params).
fn boot_post_params(body: &str, token: &str) -> Option<serde_json::Value> {
    let mut params: serde_json::Value = if body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(body).ok()?
    };
    if !token.is_empty() {
        params["token"] = json!(token);
    }
    Some(params)
}

// Static files: "/" → index.html; other paths resolved as-is (reject ..); no configured dir → 404.
async fn serve_static(web_dir: &Path, path: &str) -> String {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    // Path traversal guard: any component equal to .. or an absolute path → 404.
    if rel.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return http_text("404 Not Found", "text/plain", "");
    }
    let full = web_dir.join(rel);
    match tokio::fs::read(&full).await {
        Ok(bytes) => {
            let ct = content_type_for(rel);
            let body = String::from_utf8_lossy(&bytes).into_owned();
            // Binary files (fonts/images) would be corrupted by lossy conversion — text only;
            // dist fonts are woff2, base64-inlined in CSS, no standalone binary assets.
            if bytes.contains(&0) {
                return http_text("404 Not Found", "text/plain", "");
            }
            http_text("200 OK", ct, &body)
        }
        Err(_) => http_text("404 Not Found", "text/plain", ""),
    }
}

fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // issue #173: attribution is best-effort — this process's own listening socket can
    // always be lsof-resolved to its own pid; on failure fall back to the bare peer
    // address (never reject the request).
    #[cfg(unix)]
    #[test]
    fn test_caller_identity_resolves_own_pid() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let id = caller_identity(&format!("127.0.0.1:{port}"));
        assert!(
            id.contains(&format!("(pid {}", std::process::id())),
            "unexpected identity: {id}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_caller_identity_falls_back_without_socket() {
        // When no process holds the port (the likely case), lsof is empty → return peer as-is
        let id = caller_identity("127.0.0.1:63999");
        assert!(id == "127.0.0.1:63999" || id.contains("(pid"));
    }

    #[test]
    fn test_caller_identity_malformed_peer() {
        assert_eq!(caller_identity("garbage"), "garbage");
    }

    #[test]
    fn test_parse_request_bearer_token_and_body() {
        let raw = "POST /api/v1/down HTTP/1.1\r\nAuthorization: Bearer sekret\r\nContent-Type: application/json\r\n\r\n{\"service\":\"a\"}";
        let req = parse_request(raw);
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/api/v1/down");
        assert_eq!(req.token, "sekret");
        assert_eq!(req.body, "{\"service\":\"a\"}");
    }

    #[test]
    fn test_parse_request_query_and_xtoken() {
        let raw = "GET /api/v1/history?service=mlx&limit=5 HTTP/1.1\r\nx-token: t1\r\n\r\n";
        let req = parse_request(raw);
        assert_eq!(req.path, "/api/v1/history");
        assert_eq!(req.query, "service=mlx&limit=5");
        assert_eq!(req.token, "t1");
        assert_eq!(query_param(&req.query, "service").as_deref(), Some("mlx"));
        assert_eq!(query_param(&req.query, "limit").as_deref(), Some("5"));
    }

    #[test]
    fn test_static_path_traversal_rejected() {
        // Reject ".." and empty segments (absolute path splicing).
        assert!(rel_has_traversal("/../etc/passwd"));
        assert!(rel_has_traversal("/a/../../etc"));
        assert!(!rel_has_traversal("/assets/index.js"));
    }

    fn rel_has_traversal(path: &str) -> bool {
        let rel = path.trim_start_matches('/');
        rel.split('/').any(|seg| seg == ".." || seg.is_empty())
    }
}
