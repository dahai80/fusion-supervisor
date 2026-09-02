// C1: integration-level proof that tick_all drives the runner state machine
// through SupervisorCore (not just ServiceRunner). Unhealthy+ConnectionRefused
// → tick_all records crash + restarts → flip probe to Healthy → tick_all → Healthy.
use fusion_supervisor::manifest::{Layer, ServiceDef};
use fusion_supervisor::probe::{ProbeResult, ProbeSpec, UnhealthyReason};
use fusion_supervisor::supervisor::{SupervisorCore, runner::State};
use std::sync::{Arc, Mutex};

fn def(name: &str, port: u16) -> ServiceDef {
    ServiceDef {
        name: name.into(),
        port,
        repo_dir: name.into(),
        purpose: "x".into(),
        fixed: false,
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

#[tokio::test]
async fn test_tick_all_recovers_unhealthy_to_healthy() {
    let probe_cell: Arc<Mutex<ProbeResult>> = Arc::new(Mutex::new(ProbeResult::Unhealthy {
        reason: UnhealthyReason::ConnectionRefused,
    }));
    let pc = probe_cell.clone();
    let mut core = SupervisorCore::new_with_fns(
        vec![def("fusion-rag", 11436)],
        cfg(),
        move |_: &ProbeSpec| pc.lock().unwrap().clone(),
        |_, _, _| Ok(()),
    );
    // 起始 Unhealthy: 首 tick_all 进重启流 → Restarting
    core.runners[0].state = State::Unhealthy;
    core.tick_all(1000).await;
    assert!(
        matches!(core.runners[0].state, State::Restarting),
        "tick_all 后应 Restarting, 实测 {:?}",
        core.runners[0].state
    );
    // 翻探活为健康: tick_all 应恢复 Healthy
    *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 2 };
    core.tick_all(1001).await;
    assert!(
        matches!(core.runners[0].state, State::Healthy),
        "恢复后 tick_all 应 Healthy, 实测 {:?}",
        core.runners[0].state
    );
}

#[tokio::test]
async fn test_tick_all_skips_stopped_runner() {
    // Stopped 态 runner 在 tick 内 no-op (runner.rs:163), tick_all 不应改其状态
    let mut core = SupervisorCore::new_with_fns(
        vec![def("fusion-rag", 11436)],
        cfg(),
        |_| ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        },
        |_, _, _| Ok(()),
    );
    core.runners[0].state = State::Stopped;
    core.tick_all(2000).await;
    assert!(
        matches!(core.runners[0].state, State::Stopped),
        "Stopped 态 tick_all 应 no-op, 实测 {:?}",
        core.runners[0].state
    );
}
