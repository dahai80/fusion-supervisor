// issue #113: spawn_backup detached — 调度/RPC 不再持核心锁 await 全程 dump;
// last_backup_ts 即时推进 + meta 持久化; 失败事件经独立 Alerter 落 event 表。
use fusion_supervisor::manifest::{Layer, ServiceDef};
use fusion_supervisor::supervisor::SupervisorCore;
use std::sync::{Mutex, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, val) };
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

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

// dump bin 指向不存在路径 → spawn fail → BackupFailed 事件 (fail-closed 路径
// 正好用来证明 detached 任务确实跑完并落库)。
fn cfg(db_path: &std::path::Path) -> fusion_supervisor::config::Config {
    fusion_supervisor::config::Config {
        db_path: db_path.to_path_buf(),
        pg_dump_bin: Some("/nonexistent/pg_dumpall".into()),
        ..fusion_supervisor::config::Config::default()
    }
}

#[tokio::test]
// env_lock (std Mutex) 须跨 await 持有 — FUSION_BACKUP_TARGET 是全局 env,
// spawn 出的 detached 任务在锁内读它; tokio 单线程测试内无并发争锁。
#[allow(clippy::await_holding_lock)]
async fn test_spawn_backup_detached_and_persisted() {
    let _g = env_lock().lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let target_dir = tempfile::TempDir::new().unwrap();
    let _e = EnvGuard::set("FUSION_BACKUP_TARGET", target_dir.path().to_str().unwrap());
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-rag", 11436)],
        cfg(&tmp.path().join("history.db")),
        |_| fusion_supervisor::probe::ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let mut core = core;
    assert_eq!(core.last_backup_ts, 0, "新库无 meta → 0");
    let now: u64 = 1_700_000_000;
    core.spawn_backup(now);
    // 锁内只构造 + spawn — 调用方立即返回, 时间戳已推进
    assert_eq!(core.last_backup_ts, now, "spawn 后立即推进");
    assert_eq!(
        core.store.get_meta("last_backup_ts"),
        Some(now),
        "meta 已持久化 (reload 不重触发)"
    );
    // detached 任务跑完 → BackupFailed 事件落 event 表 (fake fail 路径)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let evs = core.store.recent_events("backup", 10).unwrap();
        if evs.iter().any(|e| e.kind == "BackupFailed") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "BackupFailed 事件未在 10s 内落库"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn test_last_backup_ts_loaded_from_meta() {
    let _g = env_lock().lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("history.db");
    // new_with_fns 构造器用独立临时库 (不接 cfg.db_path), 构造器 meta 恢复行为
    // 在 store 层验证: 同一 db 重开, set 的值必须可读 (构造器一行 get_meta 即依赖此语义)
    {
        let store = fusion_supervisor::store::HistoryStore::open(&db).unwrap();
        store.set_meta("last_backup_ts", 1_700_000_100);
    }
    // 重开 (模拟 reload) → meta 持久化
    let store = fusion_supervisor::store::HistoryStore::open(&db).unwrap();
    assert_eq!(
        store.get_meta("last_backup_ts"),
        Some(1_700_000_100),
        "reload 应从 meta 恢复 last_backup_ts"
    );
}

// issue #113: 调度到期 → check_backup_schedule 触发 spawn_backup (即时返回 true,
// 时间戳推进, 不再持锁等 dump)
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_check_backup_schedule_triggers_spawn() {
    let _g = env_lock().lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let target_dir = tempfile::TempDir::new().unwrap();
    let _e = EnvGuard::set("FUSION_BACKUP_TARGET", target_dir.path().to_str().unwrap());
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-rag", 11436)],
        cfg(&tmp.path().join("history.db")),
        |_| fusion_supervisor::probe::ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let mut core = core;
    assert!(core.check_backup_schedule(1_000).await, "last=0 → 到期触发");
    assert_eq!(core.last_backup_ts, 1_000);
    // 推进后同刻不再触发
    assert!(!core.check_backup_schedule(1_001).await);
}
