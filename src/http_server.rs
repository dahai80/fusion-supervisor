// issue #22 / #26 / #25: supervisor HTTP 面 (与 metrics 同一 listener)。
// 路由:
//   GET  /metrics                Prometheus 文本 (issue #22)
//   GET  /health                 {"ok":true} (shell 模块健康探针)
//   GET  /api/v1/status          只读 JSON 状态面, 镜像 UDS status (issue #26)
//   GET  /api/v1/history?service=&limit=   单服务健康历史 (issue #25)
//   GET  /api/v1/events?limit=            全局事件流, 告警列表数据源 (issue #25)
//   GET  /api/v1/logs?service=            日志 tail (复用 RPC dispatch)
//   POST /api/v1/{up,down,restart,drain,rollout,backup,shutdown}
//                                编排操作 — 复用 UDS dispatch, 同一 token 语义
//                                (issue #25; Authorization: Bearer / x-token)
//   GET  其余路径                web 控制台静态文件 (web_dir 配置, issue #25)
// 安全边界: listener 绑 loopback (metrics_addr, 默认 127.0.0.1:9100);
// 变更操作 token 校验与 UDS 完全一致 (FUSION_SV_TOKEN)。
// 锁纪律: 短持锁 (lock → render → release), 绝不持锁跨 await。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use crate::supervisor::SupervisorCore;

// 读请求上限: 请求行 + 头 + JSON body (编排操作参数很小)。
const MAX_REQUEST_BYTES: usize = 8192;

pub type SharedShutdownTx = Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>;

pub async fn serve(
    core: Arc<tokio::sync::Mutex<SupervisorCore>>,
    addr: &str,
    token: Option<String>,
    web_dir: Option<String>,
    shutdown_tx: SharedShutdownTx,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr, "http server listening (metrics + status + web)");
    let web_dir = web_dir.map(PathBuf::from);
    loop {
        match listener.accept().await {
            Ok((mut stream, peer)) => {
                let core = core.clone();
                let token = token.clone();
                let web_dir = web_dir.clone();
                let shutdown_tx = shutdown_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_conn(&mut stream, &core, &token, &web_dir, &shutdown_tx).await
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
    // Authorization: Bearer <t> 或 x-token: <t>
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
            // rest 是 prefix 之后的部分 — 原始 line 同位置切片。
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

    let resp = route(&req, core, token, web_dir, shutdown_tx).await;
    stream.write_all(resp.as_bytes()).await?;
    Ok(())
}

async fn route(
    req: &Request,
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
        // 编排操作 (POST) 与日志 (GET): 复用 UDS dispatch, token 语义一致。
        (m, p) if p.starts_with("/api/v1/") => {
            let method = p.trim_start_matches("/api/v1/");
            let is_get_rpc = m == "GET" && (method == "logs" || method == "status");
            if !(m == "POST" || is_get_rpc) {
                return http_text("404 Not Found", "text/plain", "");
            }
            // HTTP 凭证 (Bearer/x-token) → RPC params.token (dispatch 校验)。
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
            // GET 查询参数并入 params (logs?service=x)。
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
        // 其余 GET: web 控制台静态文件 (web_dir 配置时)。
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

// 静态文件: "/" → index.html; 其余按路径取 (拒绝 ..); 未配置目录 → 404。
async fn serve_static(web_dir: &Path, path: &str) -> String {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    // 路径穿越防护: 任一组件为 .. 或绝对路径 → 404。
    if rel.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return http_text("404 Not Found", "text/plain", "");
    }
    let full = web_dir.join(rel);
    match tokio::fs::read(&full).await {
        Ok(bytes) => {
            let ct = content_type_for(rel);
            let body = String::from_utf8_lossy(&bytes).into_owned();
            // 二进制 (字体/图片) 经 lossy 转换会损坏 — 仅文本走此路径;
            // dist 内字体为 woff2, base64 内联于 CSS, 无独立二进制资源。
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
        // 拒绝 ".." 与空段 (绝对路径拼接)。
        assert!(rel_has_traversal("/../etc/passwd"));
        assert!(rel_has_traversal("/a/../../etc"));
        assert!(!rel_has_traversal("/assets/index.js"));
    }

    fn rel_has_traversal(path: &str) -> bool {
        let rel = path.trim_start_matches('/');
        rel.split('/').any(|seg| seg == ".." || seg.is_empty())
    }
}
