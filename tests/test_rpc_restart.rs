// C2: Method::Restart RPC — issue #96 起为真实 stop(验证端口释放)+start 流程
// (restart_one): 成功返回 {"ok":true,"old_pid":..,"new_pid":..}, state →
// Starting, policy 复位。Unknown service → -32602. Empty/missing → -32602。
// 探活注入序列 Healthy→Unhealthy: 停前探活不必 (restart 不探活跳过), stop
// 验证需要一次非 Healthy 才放行。
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
    // issue #96: 探活序列 Healthy(停前)→Unhealthy(stop 验证通过) — 恒 Healthy
    // 会因旧进程未死而 bail (见 stop_and_verify)
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
    // 预置: Unhealthy + crash_count=2, restart 应复位 policy 并起重启
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
    // issue #96: 结果携带 pid 替换证据 (测试环境无 pidfile → null)
    assert_eq!(
        resp.result,
        Some(json!({"ok": true, "old_pid": null, "new_pid": null})),
        "restart 应返回 ok + pid 字段"
    );
    assert!(resp.error.is_none(), "不应有 error");
    // 验证状态机: restart 应 stop → 验证端口释放 → invoke_start Ok → Starting
    // 并复位 policy (crash_count 归 0)
    {
        let c = core_arc.lock().await;
        assert!(
            matches!(c.runners[0].state, State::Starting),
            "restart 后应 Starting, 实测 {:?}",
            c.runners[0].state
        );
        assert_eq!(c.runners[0].policy.crash_count(), 0, "policy 应复位");
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
        "未知服务应 -32602"
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
    // 空字符串 service
    let resp = restart_call(&mut client, json!({"service": ""}), 3).await;
    assert_eq!(
        resp.error.map(|e| e.code),
        Some(-32602),
        "空 service 应 -32602"
    );
    // 缺 service 字段
    let resp = restart_call(&mut client, json!({}), 4).await;
    assert_eq!(
        resp.error.map(|e| e.code),
        Some(-32602),
        "缺 service 应 -32602"
    );
    server.abort();
    let _ = std::fs::remove_file(&sock);
}
