use fusion_supervisor::alert::Alerter;
use fusion_supervisor::manifest::{Layer, ServiceDef};
use fusion_supervisor::probe::{ProbeResult, UnhealthyReason};
use fusion_supervisor::store::HistoryStore;
use fusion_supervisor::supervisor::policy::RestartPolicy;
use fusion_supervisor::supervisor::runner::{ServiceRunner, State};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn test_lifecycle_crash_then_recover() {
    let def = ServiceDef {
        name: "fusion-rag".into(),
        port: 11436,
        repo_dir: "fusion-rag".into(),
        purpose: "x".into(),
        fixed: false,
        managed: true,
        boot_tier: fusion_supervisor::manifest::BootTier::T2,
        layer: Layer::App,
    };
    let probe_cell = Arc::new(Mutex::new(ProbeResult::Healthy { latency_ms: 1 }));
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let pc = probe_cell.clone();
    let cc = calls.clone();
    let mut r = ServiceRunner::new(
        def,
        "restart",
        move || pc.lock().unwrap().clone(),
        move |c| {
            cc.lock().unwrap().push(c.into());
            Ok(())
        },
        RestartPolicy::new(300, 5, 30),
    );
    r.state = State::Healthy;
    let mut path = std::env::temp_dir();
    path.push(format!("fusion-sv-lifecycle-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = HistoryStore::open(&path).unwrap();
    let mut al = Alerter::new(300, String::new());
    // stays healthy
    r.tick(1000, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Healthy));
    // outage: first down marks Unhealthy, no restart
    *probe_cell.lock().unwrap() = ProbeResult::Unhealthy {
        reason: UnhealthyReason::ConnectionRefused,
    };
    r.tick(1001, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Unhealthy));
    // issue #31: backoff becomes scheduling — the next down tick schedules; the due tick (delay=1s) restarts
    // issue #79: consecutive-failure count — tick1001 streak=1, tick1002 streak=2 (below threshold 3, no schedule),
    // tick1003 streak=3 schedules, tick1004 due finally enters Restarting
    r.tick(1002, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Unhealthy));
    assert_eq!(calls.lock().unwrap().len(), 0);
    r.tick(1003, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Unhealthy));
    r.tick(1004, &store, &mut al).await.unwrap();
    // issue #122 phase-2: the due tick only parks the exec (lock-free execution); Restarting after the synchronous finish
    r.complete_pending_exec(1004, &mut al).await;
    assert!(matches!(r.state, State::Restarting));
    assert_eq!(calls.lock().unwrap().len(), 1);
    // recovery
    *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 5 };
    r.tick(1004, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Healthy));
    cleanup_db(&path);
}

#[tokio::test]
async fn test_issue124_failed_recovers_on_healthy_probe() {
    // issue #124: Failed state + probe recovers healthy (the operator's manual restart spawned a new instance)
    // → state flips back to Healthy, no longer stuck in Failed awaiting a manual reset.
    let (mut r, probe_cell, _calls, path) = failed_runner("a").await;
    let store = HistoryStore::open(&path).unwrap();
    let mut al = Alerter::new(300, String::new());
    *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 3 };
    r.tick(1100, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Healthy), "actual {:?}", r.state);
    cleanup_db(&path);
}

#[tokio::test]
async fn test_issue124_adopt_skips_respawn_when_port_served() {
    // issue #124: pre-exec probe of a due restart already Healthy (an external instance took over the port)
    // → skip the pull (start closure called zero times), adopt the external instance and flip back to Healthy.
    let def = ServiceDef {
        name: "fusion-agent-studio".into(),
        port: 11442,
        repo_dir: "fusion-agent-studio".into(),
        purpose: "x".into(),
        fixed: false,
        managed: true,
        boot_tier: fusion_supervisor::manifest::BootTier::T2,
        layer: Layer::App,
    };
    let probe_cell = Arc::new(Mutex::new(ProbeResult::Unhealthy {
        reason: UnhealthyReason::ConnectionRefused,
    }));
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let pc = probe_cell.clone();
    let cc = calls.clone();
    let mut r = ServiceRunner::new(
        def,
        "restart",
        move || pc.lock().unwrap().clone(),
        move |c| {
            cc.lock().unwrap().push(c.into());
            Ok(())
        },
        RestartPolicy::new(300, 5, 30),
    );
    r.state = State::Healthy;
    let path = std::env::temp_dir().join(format!("fusion-sv-i124-c-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = HistoryStore::open(&path).unwrap();
    let mut al = Alerter::new(300, String::new());
    // 3 consecutive failures schedule + the due tick parks the exec (cap=5 not reached)
    r.tick(1000, &store, &mut al).await.unwrap();
    r.tick(1001, &store, &mut al).await.unwrap();
    r.tick(1002, &store, &mut al).await.unwrap();
    r.tick(1003, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Restarting), "actual {:?}", r.state);
    // the operator's start.sh brought the service up within the backoff window — probe recovers healthy
    *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 2 };
    r.complete_pending_exec(1003, &mut al).await;
    assert!(
        calls.lock().unwrap().is_empty(),
        "the adopt path must not pull (double-process port collision), actual {:?}",
        calls.lock().unwrap()
    );
    assert!(matches!(r.state, State::Healthy), "actual {:?}", r.state);
    cleanup_db(&path);
}

#[tokio::test]
async fn test_issue126_grace_defers_restart_during_window() {
    // issue #126: within the startup grace window, probe failures count no fail_streak / schedule no restart,
    // a slow-starting daemon (30-60s to bind its port) is no longer killed by auto-restart.
    let def = ServiceDef {
        name: "fusion-agent-studio".into(),
        port: 11442,
        repo_dir: "fusion-test-no-such-repo".into(),
        purpose: "x".into(),
        fixed: false,
        managed: true,
        boot_tier: fusion_supervisor::manifest::BootTier::T2,
        layer: Layer::App,
    };
    let probe_cell = Arc::new(Mutex::new(ProbeResult::Unhealthy {
        reason: UnhealthyReason::ConnectionRefused,
    }));
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let pc = probe_cell.clone();
    let cc = calls.clone();
    let mut r = ServiceRunner::new(
        def,
        "restart",
        move || pc.lock().unwrap().clone(),
        move |c| {
            cc.lock().unwrap().push(c.into());
            Ok(())
        },
        RestartPolicy::new(300, 5, 30),
    );
    r.state = State::Starting;
    r.startup_grace_secs = 60;
    r.set_boot_grace(1000);
    let path = std::env::temp_dir().join(format!("fusion-sv-i126-a-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = HistoryStore::open(&path).unwrap();
    let mut al = Alerter::new(300, String::new());
    // consecutive failures within the grace window: state unchanged, no crash accounting, no exec scheduling
    r.tick(1001, &store, &mut al).await.unwrap();
    r.tick(1010, &store, &mut al).await.unwrap();
    r.tick(1050, &store, &mut al).await.unwrap();
    assert!(
        matches!(r.state, State::Starting),
        "state must not transition within the grace window, actual {:?}",
        r.state
    );
    assert!(calls.lock().unwrap().is_empty());
    // after grace expiry, normal flow: the first failure marks Unhealthy
    r.tick(1061, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Unhealthy), "actual {:?}", r.state);
    cleanup_db(&path);
}

#[tokio::test]
async fn test_issue126_grace_expires_normal_flow_resumes() {
    // issue #126: grace expiry + probe recovers healthy → flips back to Healthy and the grace window clears,
    // a later outage walks the normal recovery flow (unaffected by grace leftovers).
    let def = ServiceDef {
        name: "fusion-agent-studio".into(),
        port: 11442,
        repo_dir: "fusion-test-no-such-repo".into(),
        purpose: "x".into(),
        fixed: false,
        managed: true,
        boot_tier: fusion_supervisor::manifest::BootTier::T2,
        layer: Layer::App,
    };
    let probe_cell = Arc::new(Mutex::new(ProbeResult::Unhealthy {
        reason: UnhealthyReason::ConnectionRefused,
    }));
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let pc = probe_cell.clone();
    let cc = calls.clone();
    let mut r = ServiceRunner::new(
        def,
        "restart",
        move || pc.lock().unwrap().clone(),
        move |c| {
            cc.lock().unwrap().push(c.into());
            Ok(())
        },
        RestartPolicy::new(300, 5, 30),
    );
    r.state = State::Starting;
    r.startup_grace_secs = 30;
    r.set_boot_grace(1000);
    let path = std::env::temp_dir().join(format!("fusion-sv-i126-b-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = HistoryStore::open(&path).unwrap();
    let mut al = Alerter::new(300, String::new());
    // failures within the grace window are swallowed
    r.tick(1010, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Starting));
    // the daemon finishes startup, probe recovers healthy
    *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 2 };
    r.tick(1020, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Healthy), "actual {:?}", r.state);
    // another outage: normal flow (first Unhealthy, scheduled restart after 3 consecutive failures)
    *probe_cell.lock().unwrap() = ProbeResult::Unhealthy {
        reason: UnhealthyReason::ConnectionRefused,
    };
    r.tick(1030, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Unhealthy), "actual {:?}", r.state);
    r.tick(1031, &store, &mut al).await.unwrap();
    r.tick(1032, &store, &mut al).await.unwrap();
    r.tick(1033, &store, &mut al).await.unwrap();
    r.complete_pending_exec(1033, &mut al).await;
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "after the grace clears, restart should execute normally, actual {:?}",
        calls.lock().unwrap()
    );
    cleanup_db(&path);
}

// issue #124 test scaffold: a runner already in the Failed state (crash-window cap=1 capping path).
// The suffix distinguishes the two tests' db files (running concurrently in the same process, a shared name hits database is locked).
async fn failed_runner(
    suffix: &str,
) -> (
    ServiceRunner,
    Arc<Mutex<ProbeResult>>,
    Arc<Mutex<Vec<String>>>,
    std::path::PathBuf,
) {
    let def = ServiceDef {
        name: "fusion-agent-studio".into(),
        port: 11442,
        // fake repo dir — the operator race gate (issue #86) pgreps real start.sh on the machine;
        // a real repo name could collide with a running script and make the test flaky
        repo_dir: "fusion-test-no-such-repo".into(),
        purpose: "x".into(),
        fixed: false,
        managed: true,
        boot_tier: fusion_supervisor::manifest::BootTier::T2,
        layer: Layer::App,
    };
    let probe_cell = Arc::new(Mutex::new(ProbeResult::Unhealthy {
        reason: UnhealthyReason::ConnectionRefused,
    }));
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let pc = probe_cell.clone();
    let cc = calls.clone();
    let mut r = ServiceRunner::new(
        def,
        "restart",
        move || pc.lock().unwrap().clone(),
        move |c| {
            cc.lock().unwrap().push(c.into());
            Ok(())
        },
        RestartPolicy::new(300, 1, 30),
    );
    r.state = State::Healthy;
    // take the real path into Failed: first crash round (cap=1 not reached) + second crash round caps
    let path =
        std::env::temp_dir().join(format!("fusion-sv-i124-{suffix}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = HistoryStore::open(&path).unwrap();
    let mut al = Alerter::new(300, String::new());
    r.tick(1000, &store, &mut al).await.unwrap();
    r.tick(1001, &store, &mut al).await.unwrap();
    r.tick(1002, &store, &mut al).await.unwrap();
    r.tick(1003, &store, &mut al).await.unwrap();
    r.complete_pending_exec(1003, &mut al).await;
    r.tick(1004, &store, &mut al).await.unwrap();
    r.tick(1005, &store, &mut al).await.unwrap();
    r.tick(1006, &store, &mut al).await.unwrap();
    r.tick(1007, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Failed), "actual {:?}", r.state);
    (r, probe_cell, calls, path)
}

fn cleanup_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        let _ = std::fs::remove_file(parent.join(format!("{stem}.db-wal")));
        let _ = std::fs::remove_file(parent.join(format!("{stem}.db-shm")));
    }
}
