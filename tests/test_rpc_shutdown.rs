// issue #20: Method::Shutdown RPC — sends the shutdown signal, returns "ok", rx receives the signal.
// Top is a client-side command, the server arm returns -32601 with an explanation.
use fusion_supervisor::manifest::{Layer, ServiceDef};
use fusion_supervisor::probe::ProbeResult;
use fusion_supervisor::rpc::client::RpcClient;
use fusion_supervisor::rpc::schema::{RpcRequest, RpcResponse};
use fusion_supervisor::supervisor::SupervisorCore;
use serde_json::{Value, json};
use std::sync::Arc;

fn def(name: &str, port: u16) -> ServiceDef {
    ServiceDef {
        name: name.into(),
        port,
        repo_dir: name.into(),
        purpose: "x".into(),
        fixed: false,
        managed: true,
        boot_tier: fusion_supervisor::manifest::BootTier::T2,
        layer: Layer::App,
    }
}

fn cfg() -> fusion_supervisor::config::Config {
    fusion_supervisor::config::Config {
        max_concurrent_start: 2,
        start_timeout_sec: 5,
        ..fusion_supervisor::config::Config::default()
    }
}

async fn shutdown_call(client: &mut RpcClient, id: i64) -> RpcResponse {
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        method: "shutdown".into(),
        params: Value::Null,
        id,
    };
    client.call(req).await.unwrap()
}

async fn top_call(client: &mut RpcClient, id: i64) -> RpcResponse {
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        method: "top".into(),
        params: Value::Null,
        id,
    };
    client.call(req).await.unwrap()
}

#[tokio::test]
async fn test_rpc_shutdown_sends_signal_and_returns_ok() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-rag", 11436)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let sock = format!("/tmp/fusion-sv-shutdown-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&sock);
    let core_serve = core_arc.clone();
    let sock_clone = sock.clone();
    let server = tokio::spawn(async move {
        let _ = fusion_supervisor::rpc::server::serve_with_shutdown(
            core_serve,
            &sock_clone,
            None,
            Arc::new(tokio::sync::Mutex::new(Some(tx))),
        )
        .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut client = RpcClient::connect(&sock).await.unwrap();
    let resp = shutdown_call(&mut client, 1).await;
    assert_eq!(resp.result, Some(json!("ok")), "shutdown should return ok");
    assert!(resp.error.is_none(), "should have no error");
    // the signal should reach rx
    rx.await.expect("shutdown signal not received");
    server.abort();
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn test_rpc_shutdown_idempotent_second_call_unavailable() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-rag", 11436)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let (tx, _rx) = tokio::sync::oneshot::channel::<()>();
    let sock = format!("/tmp/fusion-sv-shutdown-idem-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&sock);
    let core_serve = core_arc.clone();
    let sock_clone = sock.clone();
    let server = tokio::spawn(async move {
        let _ = fusion_supervisor::rpc::server::serve_with_shutdown(
            core_serve,
            &sock_clone,
            None,
            Arc::new(tokio::sync::Mutex::new(Some(tx))),
        )
        .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut client = RpcClient::connect(&sock).await.unwrap();
    // first call takes the tx and sends
    let resp1 = shutdown_call(&mut client, 1).await;
    assert_eq!(resp1.result, Some(json!("ok")));
    // second call: tx already taken → unavailable
    let resp2 = shutdown_call(&mut client, 2).await;
    assert_eq!(
        resp2.error.map(|e| e.code),
        Some(-32000),
        "repeat shutdown should return -32000 unavailable"
    );
    server.abort();
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn test_rpc_top_returns_32601_client_side_message() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-rag", 11436)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let sock = format!("/tmp/fusion-sv-top-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&sock);
    let core_serve = core_arc.clone();
    let sock_clone = sock.clone();
    let server = tokio::spawn(async move {
        let _ = fusion_supervisor::rpc::server::serve_with_shutdown(
            core_serve,
            &sock_clone,
            None,
            Arc::new(tokio::sync::Mutex::new(None)),
        )
        .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut client = RpcClient::connect(&sock).await.unwrap();
    let resp = top_call(&mut client, 1).await;
    assert_eq!(
        resp.error.as_ref().map(|e| e.code),
        Some(-32601),
        "top should return -32601 (client-side command)"
    );
    assert!(
        resp.error
            .as_ref()
            .map(|e| e.message.contains("client-side"))
            .unwrap_or(false),
        "error message should mention client-side"
    );
    server.abort();
    let _ = std::fs::remove_file(&sock);
}
