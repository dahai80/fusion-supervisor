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
    // 健康保持
    r.tick(1000, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Healthy));
    // 宕机: 首次标 Unhealthy, 不重启
    *probe_cell.lock().unwrap() = ProbeResult::Unhealthy {
        reason: UnhealthyReason::ConnectionRefused,
    };
    r.tick(1001, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Unhealthy));
    // issue #31: 退避改为排期 — 再次宕机 tick 排期, 到点 tick (delay=1s) 重启
    // issue #79: 连续失败计数 — tick1001 streak=1, tick1002 streak=2 (未达阈值 3 不排),
    // tick1003 streak=3 排期, tick1004 到点才进 Restarting
    r.tick(1002, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Unhealthy));
    assert_eq!(calls.lock().unwrap().len(), 0);
    r.tick(1003, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Unhealthy));
    r.tick(1004, &store, &mut al).await.unwrap();
    // issue #122 phase-2: 到点 tick 只挂起 exec (锁外执行), 同步收尾后 Restarting
    r.complete_pending_exec(1004, &mut al).await;
    assert!(matches!(r.state, State::Restarting));
    assert_eq!(calls.lock().unwrap().len(), 1);
    // 恢复
    *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 5 };
    r.tick(1004, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Healthy));
    cleanup_db(&path);
}

#[tokio::test]
async fn test_issue124_failed_recovers_on_healthy_probe() {
    // issue #124: Failed 态 + 探活恢复健康 (操作员手工 restart 拉起了新实例)
    // → 状态翻回 Healthy, 不再永久卡 Failed 等人工复位。
    let (mut r, probe_cell, _calls, path) = failed_runner("a").await;
    let store = HistoryStore::open(&path).unwrap();
    let mut al = Alerter::new(300, String::new());
    *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 3 };
    r.tick(1100, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Healthy), "实际 {:?}", r.state);
    cleanup_db(&path);
}

#[tokio::test]
async fn test_issue124_adopt_skips_respawn_when_port_served() {
    // issue #124: 到点重启 exec 前置探活已 Healthy (外部实例已接管端口)
    // → 跳过拉起 (start 闭包零调用), 采纳外部实例翻回 Healthy。
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
    // 3 连败排期 + 到点 tick 挂起 exec (cap=5 未封顶)
    r.tick(1000, &store, &mut al).await.unwrap();
    r.tick(1001, &store, &mut al).await.unwrap();
    r.tick(1002, &store, &mut al).await.unwrap();
    r.tick(1003, &store, &mut al).await.unwrap();
    assert!(matches!(r.state, State::Restarting), "实际 {:?}", r.state);
    // 操作员的 start.sh 在退避窗口内把服务拉起来了 — 探活恢复健康
    *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 2 };
    r.complete_pending_exec(1003, &mut al).await;
    assert!(
        calls.lock().unwrap().is_empty(),
        "采纳路径不得拉起 (双进程撞口), 实际 {:?}",
        calls.lock().unwrap()
    );
    assert!(matches!(r.state, State::Healthy), "实际 {:?}", r.state);
    cleanup_db(&path);
}

// issue #124 测试脚手架: 已达 Failed 态的 runner (crash-window cap=1 封顶路径)。
// 后缀区分两测试的 db 文件 (同进程并发跑, 共名会 database is locked)。
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
        // 假 repo 目录 — 操作员竞态闸门 (issue #86) 会 pgrep 真机 start.sh,
        // 用真实 repo 名可能撞上在跑的脚本导致测试不确定
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
    // 走真实路径进 Failed: 第一轮 crash (cap=1 未封顶) + 第二轮 crash 封顶
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
    assert!(matches!(r.state, State::Failed), "实际 {:?}", r.state);
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
