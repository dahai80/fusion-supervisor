#![allow(dead_code)]

use crate::rpc::schema::{make_error, make_result, parse_method, Method, RpcRequest, RpcResponse};
use crate::supervisor::SupervisorCore;
use anyhow::Result;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

pub async fn serve(
    core: std::sync::Arc<tokio::sync::Mutex<SupervisorCore>>,
    socket_path: &str,
    token: Option<String>,
) -> Result<()> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    tracing::info!(sock = %socket_path, "rpc server listening (0600)");
    loop {
        let (stream, _) = listener.accept().await?;
        let core = core.clone();
        let token = token.clone();
        tokio::spawn(handle_conn(stream, core, token));
    }
}

async fn handle_conn(
    stream: tokio::net::UnixStream,
    core: std::sync::Arc<tokio::sync::Mutex<SupervisorCore>>,
    token: Option<String>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let resp = match serde_json::from_str::<RpcRequest>(&line) {
            Ok(req) => dispatch(req, &core, &token).await,
            Err(e) => make_error(-32700, &format!("parse error: {e}"), 0),
        };
        let mut out = serde_json::to_string(&resp).unwrap_or_else(|_| r#"{"error":{"code":-32603}}"#.into());
        out.push('\n');
        if writer.write_all(out.as_bytes()).await.is_err() { break; }
    }
}

async fn dispatch(
    req: RpcRequest,
    core: &std::sync::Arc<tokio::sync::Mutex<SupervisorCore>>,
    token: &Option<String>,
) -> RpcResponse {
    if let Some(expected) = token {
        let got = req.params.get("token").and_then(|v| v.as_str()).unwrap_or("");
        if got != expected {
            return make_error(-32001, "unauthorized", req.id);
        }
    }
    match parse_method(&req.method) {
        None => make_error(-32601, &format!("method not found: {}", req.method), req.id),
        Some(Method::Ping) => make_result(json!("pong"), req.id),
        Some(Method::Status) => {
            let c = core.lock().await;
            let entries: Vec<_> = c.status_all().into_iter().map(|e| json!({
                "name": e.name, "state": format!("{:?}", e.state), "port": e.port,
            })).collect();
            make_result(json!(entries), req.id)
        }
        Some(Method::Up) => {
            let mut c = core.lock().await;
            match c.up_all().await { Ok(()) => make_result(json!("ok"), req.id), Err(e) => make_error(-32000, &e.to_string(), req.id) }
        }
        Some(Method::Down) => {
            let mut c = core.lock().await;
            match c.down_all().await { Ok(()) => make_result(json!("ok"), req.id), Err(e) => make_error(-32000, &e.to_string(), req.id) }
        }
        Some(Method::Restart | Method::Top | Method::Shutdown) => {
            make_error(-32601, "not implemented yet", req.id)
        }
    }
}
