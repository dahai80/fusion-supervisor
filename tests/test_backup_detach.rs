// issue #113: spawn_backup detached — scheduling/RPC no longer hold the core lock across the whole dump await;
// last_backup_ts advances immediately + meta persisted; failure events land in the event table via an independent Alerter.
use fusion_supervisor::manifest::{Layer, ServiceDef};
use fusion_supervisor::supervisor::SupervisorCore;
use std::sync::{Mutex, OnceLock};

// Reentrant env test lock (same as #134: thread-local depth + outermost layer holds a std Mutex),
// EnvGuard and test body share one lock domain, set/restore windows are atomic vs other env tests.
thread_local! {
    static ENV_LOCK_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
struct EnvLockGuard {
    _inner: Option<std::sync::MutexGuard<'static, ()>>,
}

impl Drop for EnvLockGuard {
    fn drop(&mut self) {
        ENV_LOCK_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

fn env_lock() -> EnvLockGuard {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    let outermost = ENV_LOCK_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v == 0
    });
    EnvLockGuard {
        _inner: outermost.then(|| L.get_or_init(|| Mutex::new(())).lock().unwrap()),
    }
}

struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
    _lock: EnvLockGuard,
}

impl EnvGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let lock = env_lock();
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, val) };
        Self {
            key,
            prev,
            _lock: lock,
        }
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

// dump bin points at a nonexistent path → spawn fail → BackupFailed event (the fail-closed
// path conveniently proves the detached task actually ran and persisted).
fn cfg(db_path: &std::path::Path) -> fusion_supervisor::config::Config {
    fusion_supervisor::config::Config {
        db_path: db_path.to_path_buf(),
        pg_dump_bin: Some("/nonexistent/pg_dumpall".into()),
        ..fusion_supervisor::config::Config::default()
    }
}

#[tokio::test]
// env_lock (std Mutex) must be held across await — FUSION_BACKUP_TARGET is a global env var,
// read by the spawned detached task while we hold the lock; no concurrent contention in a single-threaded tokio test.
#[allow(clippy::await_holding_lock)]
async fn test_spawn_backup_detached_and_persisted() {
    let _g = env_lock();
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
    assert_eq!(core.last_backup_ts, 0, "fresh db has no meta → 0");
    let now: u64 = 1_700_000_000;
    core.spawn_backup(now);
    // Under the lock only construct + spawn — caller returns immediately, timestamp already advanced
    assert_eq!(core.last_backup_ts, now, "advanced immediately after spawn");
    assert_eq!(
        core.store.get_meta("last_backup_ts"),
        Some(now),
        "meta persisted (reload does not re-trigger)"
    );
    // detached task finished → BackupFailed event lands in the event table (fake fail path)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let evs = core.store.recent_events("backup", 10).unwrap();
        if evs.iter().any(|e| e.kind == "BackupFailed") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "BackupFailed event not persisted within 10s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn test_last_backup_ts_loaded_from_meta() {
    let _g = env_lock();
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("history.db");
    // new_with_fns constructor uses its own temp db (not cfg.db_path); the constructor's meta-restore
    // behavior is verified at the store layer: reopening the same db must read back the set value
    // (the constructor's single get_meta line depends on this semantics)
    {
        let store = fusion_supervisor::store::HistoryStore::open(&db).unwrap();
        store.set_meta("last_backup_ts", 1_700_000_100);
    }
    // Reopen (simulating reload) → meta persisted
    let store = fusion_supervisor::store::HistoryStore::open(&db).unwrap();
    assert_eq!(
        store.get_meta("last_backup_ts"),
        Some(1_700_000_100),
        "reload should restore last_backup_ts from meta"
    );
}

// issue #113: schedule due → check_backup_schedule triggers spawn_backup (returns true immediately,
// timestamp advanced, no longer holds the lock waiting for the dump)
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn test_check_backup_schedule_triggers_spawn() {
    let _g = env_lock();
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
    assert!(
        core.check_backup_schedule(1_000).await,
        "last=0 → due, triggers"
    );
    assert_eq!(core.last_backup_ts, 1_000);
    // Already advanced, no re-trigger at the same instant
    assert!(!core.check_backup_schedule(1_001).await);
}
