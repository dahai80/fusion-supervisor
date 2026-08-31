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
    let mut store = HistoryStore::open(&path).unwrap();
    let mut al = Alerter::new(300, String::new());
    // 健康保持
    r.tick(1000, &mut store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Healthy));
    // 宕机: 首次标 Unhealthy, 不重启
    *probe_cell.lock().unwrap() =
        ProbeResult::Unhealthy { reason: UnhealthyReason::ConnectionRefused };
    r.tick(1001, &mut store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Unhealthy));
    // 再次宕机: 进重启流
    r.tick(1002, &mut store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Restarting));
    assert_eq!(calls.lock().unwrap().len(), 1);
    // 恢复
    *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 5 };
    r.tick(1003, &mut store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Healthy));
    cleanup_db(&path);
}

fn cleanup_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        let _ = std::fs::remove_file(parent.join(format!("{stem}.db-wal")));
        let _ = std::fs::remove_file(parent.join(format!("{stem}.db-shm")));
    }
}
