use fusion_supervisor::alert::Alerter;
use fusion_supervisor::manifest::{Layer, ServiceDef};
use fusion_supervisor::probe::{ProbeResult, UnhealthyReason};
use fusion_supervisor::store::HistoryStore;
use fusion_supervisor::supervisor::policy::RestartPolicy;
use fusion_supervisor::supervisor::runner::{ServiceRunner, State};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn test_badpath_never_restarts() {
    let def = ServiceDef {
        name: "fusion-rag".into(),
        port: 11436,
        repo_dir: "fusion-rag".into(),
        purpose: "x".into(),
        fixed: false,
        layer: Layer::App,
    };
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let cc = calls.clone();
    let mut r = ServiceRunner::new(
        def,
        "restart",
        || ProbeResult::Unhealthy {
            reason: UnhealthyReason::BadPath(404),
        },
        move |c| {
            cc.lock().unwrap().push(c.into());
            Ok(())
        },
        RestartPolicy::new(300, 5, 30),
    );
    r.state = State::Healthy;
    let mut path = std::env::temp_dir();
    path.push(format!("fusion-sv-badpath-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = HistoryStore::open(&path).unwrap();
    let mut al = Alerter::new(300, String::new());
    for ts in 1000..1010u64 {
        r.tick(ts, &store, &mut al).await.unwrap();
    }
    assert!(
        calls.lock().unwrap().is_empty(),
        "BadPath 绝不调 start.sh (防风暴)"
    );
    assert_eq!(r.policy.crash_count(), 0, "BadPath 不记 crash");
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
