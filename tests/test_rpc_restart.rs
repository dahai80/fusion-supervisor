// C2: Method::Restart RPC — since issue #96 a real stop (with port-release verification) + start flow
// (restart_one): success returns {"ok":true,"old_pid":..,"new_pid":..}, state →
// Starting, policy reset. Unknown service → -32602. Empty/missing → -32602.
// Injected probe sequence Healthy→Unhealthy: a pre-stop probe is unnecessary (restart does not
// probe-skip), but stop verification needs one non-Healthy to pass.
use fusion_supervisor::manifest::{Layer, ServiceDef};
use fusion_supervisor::probe::ProbeResult;
use fusion_supervisor::rpc::client::RpcClient;
use fusion_supervisor::rpc::schema::{RpcRequest, RpcResponse};
use fusion_supervisor::supervisor::{SupervisorCore, runner::State};
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

async fn restart_call(client: &mut RpcClient, params: Value, id: i64) -> RpcResponse {
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        method: "restart".into(),
        params,
        id,
    };
    client.call(req).await.unwrap()
}

#[tokio::test]
async fn test_rpc_restart_known_service_returns_ok_and_sets_starting() {
    // issue #96: probe sequence Healthy(pre-stop)→Unhealthy(stop verification passes) — always-Healthy
    // would bail because the old process never died (see stop_and_verify)
    let q: Arc<std::sync::Mutex<std::collections::VecDeque<ProbeResult>>> =
        Arc::new(std::sync::Mutex::new(
            [
                ProbeResult::Healthy { latency_ms: 1 },
                ProbeResult::Unhealthy {
                    reason: fusion_supervisor::probe::UnhealthyReason::ConnectionRefused,
                },
            ]
            .into_iter()
            .collect(),
        ));
    let qc = q.clone();
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-rag", 11436)],
        cfg(),
        move |_: &fusion_supervisor::probe::ProbeSpec| {
            qc.lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ProbeResult::Healthy { latency_ms: 1 })
        },
        |_, _, _| Ok(()),
    );
    // preset: Unhealthy + crash_count=2, restart should reset policy and trigger a restart
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    {
        let mut c = core_arc.lock().await;
        c.runners[0].state = State::Unhealthy;
        c.runners[0].policy.record_crash(100);
        c.runners[0].policy.record_crash(101);
        assert_eq!(c.runners[0].policy.crash_count(), 2);
    }
    let sock = format!("/tmp/fusion-sv-restart-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&sock);
    let core_serve = core_arc.clone();
    let sock_clone = sock.clone();
    let server = tokio::spawn(async move {
        let _ = fusion_supervisor::rpc::server::serve(core_serve, &sock_clone, None).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut client = RpcClient::connect(&sock).await.unwrap();
    let resp = restart_call(&mut client, json!({"service": "fusion-rag"}), 1).await;
    // issue #96: result carries pid replacement evidence (no pidfile in tests → null)
    assert_eq!(
        resp.result,
        Some(json!({"ok": true, "old_pid": null, "new_pid": null})),
        "restart should return ok + pid fields"
    );
    assert!(resp.error.is_none(), "should have no error");
    // verify state machine: restart should stop → verify port release → invoke_start Ok → Starting
    // and reset policy (crash_count back to 0)
    {
        let c = core_arc.lock().await;
        assert!(
            matches!(c.runners[0].state, State::Starting),
            "should be Starting after restart, got {:?}",
            c.runners[0].state
        );
        assert_eq!(
            c.runners[0].policy.crash_count(),
            0,
            "policy should be reset"
        );
    }
    server.abort();
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn test_rpc_restart_unknown_service_returns_32602() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-rag", 11436)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let sock = format!("/tmp/fusion-sv-restart-unknown-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&sock);
    let core_serve = core_arc.clone();
    let sock_clone = sock.clone();
    let server = tokio::spawn(async move {
        let _ = fusion_supervisor::rpc::server::serve(core_serve, &sock_clone, None).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut client = RpcClient::connect(&sock).await.unwrap();
    let resp = restart_call(&mut client, json!({"service": "no-such-svc"}), 2).await;
    assert_eq!(
        resp.error.map(|e| e.code),
        Some(-32602),
        "unknown service should return -32602"
    );
    assert!(resp.result.is_none());
    server.abort();
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn test_rpc_restart_empty_or_missing_service_returns_32602() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-rag", 11436)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let sock = format!("/tmp/fusion-sv-restart-empty-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&sock);
    let core_serve = core_arc.clone();
    let sock_clone = sock.clone();
    let server = tokio::spawn(async move {
        let _ = fusion_supervisor::rpc::server::serve(core_serve, &sock_clone, None).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut client = RpcClient::connect(&sock).await.unwrap();
    // empty-string service
    let resp = restart_call(&mut client, json!({"service": ""}), 3).await;
    assert_eq!(
        resp.error.map(|e| e.code),
        Some(-32602),
        "empty service should return -32602"
    );
    // missing service field
    let resp = restart_call(&mut client, json!({}), 4).await;
    assert_eq!(
        resp.error.map(|e| e.code),
        Some(-32602),
        "missing service should return -32602"
    );
    server.abort();
    let _ = std::fs::remove_file(&sock);
}
