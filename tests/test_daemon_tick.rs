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
    // Start Unhealthy. issue #31: backoff became scheduling — the first tick_all schedules without restarting,
    // the due tick_all (delay=1s) enters the restart flow → Restarting
    // issue #79: preset the consecutive-failure count (testing straight from Unhealthy)
    core.runners[0].state = State::Unhealthy;
    core.runners[0].fail_streak = 2;
    core.drive(1000).await;
    assert!(
        matches!(core.runners[0].state, State::Unhealthy),
        "scheduled tick must not restart, got {:?}",
        core.runners[0].state
    );
    core.drive(1001).await;
    assert!(
        matches!(core.runners[0].state, State::Restarting),
        "should be Restarting after tick_all, got {:?}",
        core.runners[0].state
    );
    // Flip the probe to healthy: tick_all should recover to Healthy
    *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 2 };
    core.drive(1001).await;
    assert!(
        matches!(core.runners[0].state, State::Healthy),
        "tick_all should be Healthy after recovery, got {:?}",
        core.runners[0].state
    );
}

#[tokio::test]
async fn test_tick_all_skips_stopped_runner() {
    // A Stopped runner is a no-op inside tick (runner.rs:163); tick_all must not change its state
    let mut core = SupervisorCore::new_with_fns(
        vec![def("fusion-rag", 11436)],
        cfg(),
        |_| ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        },
        |_, _, _| Ok(()),
    );
    core.runners[0].state = State::Stopped;
    core.drive(2000).await;
    assert!(
        matches!(core.runners[0].state, State::Stopped),
        "tick_all should be a no-op on a Stopped runner, got {:?}",
        core.runners[0].state
    );
}
