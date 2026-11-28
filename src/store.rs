use anyhow::{Context, Result};
use rusqlite::{Connection, params};

// Connection is Send but !Sync (contains a RefCell). The tick loop (async) holds
// &HistoryStore across .await, which requires &HistoryStore: Send → HistoryStore: Sync.
// Wrap in std::sync::Mutex to make it Send+Sync, unlocking the tokio::spawn in the
// daemon tick loop (C1). SQLite connections are inherently single-threaded; the
// Mutex makes that explicit.
pub struct HistoryStore {
    pub conn: std::sync::Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct HealthRow {
    pub ts: u64,
    pub service: String,
    pub status: String,
    pub latency_ms: u64,
    pub plane: String,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub ts: u64,
    pub service: String,
    pub kind: String,
    pub detail: String,
    pub plane: String,
    // issue #37: event severity (warn/error/info), defaults to warn on old-DB migration
    pub severity: String,
}

// issue #12: one probed tripwire window. None latency = probe error (refused/timeout).
#[derive(Debug, Clone)]
pub struct TripwireRow {
    pub ts: u64,
    pub gateway_ms: Option<u64>,
    pub identity_ms: Option<u64>,
    pub ci_active: bool,
    pub breach: bool,
}

// PRD app-tiering: per-service boot override. Pin override > registry boot: field;
// stored in local SQLite, survives restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PinMode {
    Auto,
    Manual,
}

impl PinMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PinMode::Auto => "auto",
            PinMode::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<PinMode> {
        match s {
            "auto" => Some(PinMode::Auto),
            "manual" => Some(PinMode::Manual),
            _ => None,
        }
    }
}

impl HistoryStore {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).ok();
        }
        let conn =
            Connection::open(path).with_context(|| format!("open sqlite {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS health_check (
                 ts INTEGER NOT NULL,
                 service TEXT NOT NULL,
                 status TEXT NOT NULL,
                 latency_ms INTEGER NOT NULL,
                 plane TEXT NOT NULL DEFAULT 'native'
             );
             CREATE TABLE IF NOT EXISTS event (
                 ts INTEGER NOT NULL,
                 service TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 detail TEXT NOT NULL,
                 plane TEXT NOT NULL DEFAULT 'native',
                 severity TEXT NOT NULL DEFAULT 'warn'
             );
             -- issue #12: login-latency tripwire samples — one row per probed window
             -- (CI-active windows only). Postmortem latency graphs read this table.
             CREATE TABLE IF NOT EXISTS tripwire_sample (
                 ts INTEGER PRIMARY KEY,
                 gateway_ms INTEGER,
                 identity_ms INTEGER,
                 ci_active INTEGER NOT NULL,
                 breach INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS boot_pin (
                 service TEXT PRIMARY KEY,
                 mode TEXT NOT NULL
             );
             -- issue #113: persist scheduling metadata (last_backup_ts etc.) across restarts.
             CREATE TABLE IF NOT EXISTS meta (
                 key TEXT PRIMARY KEY,
                 value INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_health_svc ON health_check(service, ts);
             CREATE INDEX IF NOT EXISTS idx_event_svc ON event(service, ts);
             CREATE INDEX IF NOT EXISTS idx_tripwire_ts ON tripwire_sample(ts);",
        )?;
        // Migration: old DBs lack the plane column → ALTER ADD COLUMN (idempotent; SQLite
        // has no ADD COLUMN IF NOT EXISTS, so we swallow the duplicate-column error).
        // Existing rows default to 'native' via the DEFAULT clause.
        migrate_add_plane(&conn)?;
        // issue #37: old DBs' event table lacks the severity column → same approach,
        // existing rows default to 'warn'.
        migrate_add_severity(&conn)?;
        tracing::info!(db = %path.display(), "history store open (WAL)");
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
        })
    }

    // issue #113: meta read/write — restore last_backup_ts and other scheduling cursors
    // after a daemon restart.
    pub fn get_meta(&self, key: &str) -> Option<u64> {
        let conn = self.conn.lock().unwrap();
        match conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get::<_, i64>(0),
        ) {
            Ok(v) => Some(v.max(0) as u64),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => {
                tracing::warn!(key, err = %e, "meta read failed");
                None
            }
        }
    }

    pub fn set_meta(&self, key: &str, value: u64) {
        let res = self.conn.lock().unwrap().execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value as i64],
        );
        if let Err(e) = res {
            tracing::warn!(key, err = %e, "meta write failed");
        }
    }

    pub fn record_health(
        &self,
        service: &str,
        status: &str,
        latency_ms: u64,
        ts: u64,
        plane: &str,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO health_check(ts, service, status, latency_ms, plane) VALUES (?,?,?,?,?)",
            params![ts as i64, service, status, latency_ms as i64, plane],
        )?;
        Ok(())
    }

    pub fn record_event(
        &self,
        service: &str,
        kind: &str,
        detail: &str,
        ts: u64,
        plane: &str,
        severity: &str,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO event(ts, service, kind, detail, plane, severity) VALUES (?,?,?,?,?,?)",
            params![ts as i64, service, kind, detail, plane, severity],
        )?;
        Ok(())
    }

    // issue #12: persist one tripwire sample (NULL = probe error for either endpoint).
    pub fn record_tripwire_sample(
        &self,
        ts: u64,
        gateway_ms: Option<u64>,
        identity_ms: Option<u64>,
        ci_active: bool,
        breach: bool,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT OR REPLACE INTO tripwire_sample(ts, gateway_ms, identity_ms, ci_active, breach)
             VALUES (?,?,?,?,?)",
            params![
                ts as i64,
                gateway_ms.map(|v| v as i64),
                identity_ms.map(|v| v as i64),
                ci_active as i64,
                breach as i64
            ],
        )?;
        Ok(())
    }

    // issue #12: recent tripwire samples, ascending ts — postmortem latency graph data source.
    pub fn recent_tripwire_samples(&self, limit: u32) -> Result<Vec<TripwireRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ts, gateway_ms, identity_ms, ci_active, breach FROM (
                 SELECT ts, gateway_ms, identity_ms, ci_active, breach FROM tripwire_sample
                 ORDER BY ts DESC LIMIT ?
             ) ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(TripwireRow {
                ts: r.get::<_, i64>(0)? as u64,
                gateway_ms: r.get::<_, Option<i64>>(1)?.map(|v| v.max(0) as u64),
                identity_ms: r.get::<_, Option<i64>>(2)?.map(|v| v.max(0) as u64),
                ci_active: r.get::<_, i64>(3)? != 0,
                breach: r.get::<_, i64>(4)? != 0,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    // issue #112: most recent restart-kind event per service (AutoRestart/ManualStart/ManualStop).
    // Data source for the status table's last_restart column — answers "who last started/stopped
    // this service" after the fact. Detail truncated to 80 chars to keep table rows short;
    // services without a record are absent from the map.
    pub fn last_restart_map(&self) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        let conn = self.conn.lock().unwrap();
        let res = conn.prepare(
            "SELECT service, kind || ' ' || substr(detail, 1, 80), MAX(ts) FROM event
             WHERE kind IN ('AutoRestart','ManualStart','ManualStop')
             GROUP BY service",
        );
        let mut stmt = match res {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(err = %e, "last_restart_map query fail, return empty");
                return out;
            }
        };
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
        if let Ok(rows) = rows {
            for row in rows.flatten() {
                out.insert(row.0, row.1);
            }
        }
        out
    }

    pub fn cleanup_old(&self, now_ts: u64, retain_days: u32) -> Result<usize> {
        let cutoff = now_ts.saturating_sub((retain_days as u64) * 24 * 3600);
        let h = self.conn.lock().unwrap().execute(
            "DELETE FROM health_check WHERE ts <= ?",
            params![cutoff as i64],
        )?;
        let e = self
            .conn
            .lock()
            .unwrap()
            .execute("DELETE FROM event WHERE ts <= ?", params![cutoff as i64])?;
        // issue #12: tripwire samples follow the same retention window
        let t = self.conn.lock().unwrap().execute(
            "DELETE FROM tripwire_sample WHERE ts <= ?",
            params![cutoff as i64],
        )?;
        tracing::info!(
            cutoff,
            deleted_health = h,
            deleted_event = e,
            deleted_tripwire = t,
            "cleanup old"
        );
        Ok(h + e + t)
    }

    pub fn recent_health(&self, service: &str, limit: u32) -> Result<Vec<HealthRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ts, service, status, latency_ms, plane FROM (
                 SELECT ts, service, status, latency_ms, plane FROM health_check
                 WHERE service = ? ORDER BY ts DESC LIMIT ?
             ) ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![service, limit], |r| {
            Ok(HealthRow {
                ts: r.get::<_, i64>(0)? as u64,
                service: r.get(1)?,
                status: r.get(2)?,
                latency_ms: r.get::<_, i64>(3)? as u64,
                plane: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn recent_events(&self, service: &str, limit: u32) -> Result<Vec<EventRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ts, service, kind, detail, plane, severity FROM (
                 SELECT ts, service, kind, detail, plane, severity FROM event
                 WHERE service = ? ORDER BY ts DESC LIMIT ?
             ) ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![service, limit], |r| {
            Ok(EventRow {
                ts: r.get::<_, i64>(0)? as u64,
                service: r.get(1)?,
                kind: r.get(2)?,
                detail: r.get(3)?,
                plane: r.get(4)?,
                severity: r.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    // PRD app-tiering: write/clear a boot pin override. None = clear (fall back to the
    // registry boot: field semantics).
    pub fn set_boot_pin(&self, service: &str, mode: Option<PinMode>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        match mode {
            Some(m) => {
                conn.execute(
                    "INSERT INTO boot_pin(service, mode) VALUES (?, ?)
                     ON CONFLICT(service) DO UPDATE SET mode = excluded.mode",
                    params![service, m.as_str()],
                )?;
            }
            None => {
                conn.execute("DELETE FROM boot_pin WHERE service = ?", params![service])?;
            }
        }
        Ok(())
    }

    pub fn get_boot_pin(&self, service: &str) -> Result<Option<PinMode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT mode FROM boot_pin WHERE service = ?")?;
        let mut rows = stmt.query_map(params![service], |r| r.get::<_, String>(0))?;
        match rows.next() {
            Some(Ok(mode)) => Ok(PinMode::parse(&mode)),
            _ => Ok(None),
        }
    }

    pub fn all_boot_pins(&self) -> Result<Vec<(String, PinMode)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT service, mode FROM boot_pin")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(svc, mode)| PinMode::parse(&mode).map(|m| (svc, m)))
            .collect())
    }

    // issue #25: global event stream (data source for the web alerts list) — recent
    // events across services, returned in ascending time order.
    pub fn recent_events_all(&self, limit: u32) -> Result<Vec<EventRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ts, service, kind, detail, plane, severity FROM (
                 SELECT ts, service, kind, detail, plane, severity FROM event
                 ORDER BY ts DESC LIMIT ?
             ) ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(EventRow {
                ts: r.get::<_, i64>(0)? as u64,
                service: r.get(1)?,
                kind: r.get(2)?,
                detail: r.get(3)?,
                plane: r.get(4)?,
                severity: r.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

// Migration: old DBs lack the plane column → ALTER ADD COLUMN. SQLite has no
// IF NOT EXISTS for columns; idempotency comes from swallowing the
// "duplicate column name" error. Existing rows get DEFAULT 'native'.
fn migrate_add_plane(conn: &Connection) -> Result<()> {
    for table in ["health_check", "event"] {
        let sql = format!("ALTER TABLE {table} ADD COLUMN plane TEXT NOT NULL DEFAULT 'native'");
        match conn.execute(&sql, []) {
            Ok(_) => {
                tracing::info!(%table, "migrate: add plane column");
            }
            // SQLite has no ADD COLUMN IF NOT EXISTS; a duplicate column reports
            // "duplicate column name" → swallow it
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column") =>
            {
                tracing::debug!(%table, "migrate: plane column already exists, skipping");
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

// issue #37: old DBs' event table lacks the severity column → ALTER ADD COLUMN,
// same pattern as migrate_add_plane. Existing rows default to 'warn' (DEFAULT clause).
fn migrate_add_severity(conn: &Connection) -> Result<()> {
    match conn.execute(
        "ALTER TABLE event ADD COLUMN severity TEXT NOT NULL DEFAULT 'warn'",
        [],
    ) {
        Ok(_) => {
            tracing::info!("migrate: add severity column");
        }
        Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("duplicate column") => {
            tracing::debug!("migrate: severity column already exists, skipping");
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_mem() -> HistoryStore {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        HistoryStore::open(tmp.path()).unwrap()
    }

    // issue #112: last_restart_map — only restart-kind kinds, newest entry per service,
    // detail truncated, other kinds excluded from the map
    #[test]
    fn test_last_restart_map() {
        let s = open_mem();
        s.record_event("svc-a", "ServiceDown", "down", 100, "native", "warn")
            .unwrap();
        s.record_event("svc-a", "AutoRestart", "auto-recovery restart ok old_pid=1 new_pid=2 and some very long detail that should be truncated at eighty characters boundary here!!!!", 200, "native", "info")
            .unwrap();
        s.record_event("svc-a", "ManualStop", "http down", 150, "native", "info")
            .unwrap();
        s.record_event("svc-b", "ManualStart", "cli up", 300, "native", "info")
            .unwrap();
        let m = s.last_restart_map();
        assert_eq!(m.len(), 2);
        // svc-a picks the MAX(ts)=200 AutoRestart row, detail truncated at 80 chars
        let a = &m["svc-a"];
        assert!(
            a.starts_with("AutoRestart auto-recovery restart ok"),
            "got: {a}"
        );
        assert!(
            a.contains("new_pid=2"),
            "expected the ts=200 AutoRestart row: {a}"
        );
        assert!(
            a.len() <= 11 + 1 + 80,
            "detail should truncate at 80: len={}",
            a.len()
        );
        assert_eq!(m["svc-b"], "ManualStart cli up");
        // empty DB → empty map
        let s2 = open_mem();
        assert!(s2.last_restart_map().is_empty());
    }

    // issue #113: meta read/write + persistence across reopen — reload restores
    // last_backup_ts without re-triggering a backup
    #[test]
    fn test_meta_roundtrip_and_persist() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let s = HistoryStore::open(tmp.path()).unwrap();
        assert_eq!(s.get_meta("last_backup_ts"), None);
        s.set_meta("last_backup_ts", 123456);
        assert_eq!(s.get_meta("last_backup_ts"), Some(123456));
        // overwrite
        s.set_meta("last_backup_ts", 234567);
        assert_eq!(s.get_meta("last_backup_ts"), Some(234567));
        // reopen (same file) → value persists
        drop(s);
        let s2 = HistoryStore::open(tmp.path()).unwrap();
        assert_eq!(s2.get_meta("last_backup_ts"), Some(234567));
        // unrelated keys stay isolated
        assert_eq!(s2.get_meta("other"), None);
    }

    #[test]
    fn test_create_tables() {
        let s = open_mem();
        let n: i64 = s
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('health_check','event')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn test_record_and_query_health() {
        let s = open_mem();
        s.record_health("fusion-mlx", "healthy", 12, 1000, "native")
            .unwrap();
        s.record_health("fusion-mlx", "unhealthy", 0, 2000, "native")
            .unwrap();
        let rows = s.recent_health("fusion-mlx", 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].status, "healthy");
        assert_eq!(rows[0].latency_ms, 12);
        assert_eq!(rows[0].plane, "native");
        assert_eq!(rows[1].status, "unhealthy");
    }

    #[test]
    fn test_record_and_query_event() {
        let s = open_mem();
        s.record_event(
            "fusion-rag",
            "ServiceDown",
            "conn refused",
            5000,
            "native",
            "warn",
        )
        .unwrap();
        s.record_event(
            "fusion-rag",
            "ServiceBack",
            "recovered",
            5300,
            "native",
            "info",
        )
        .unwrap();
        let rows = s.recent_events("fusion-rag", 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "ServiceDown");
        assert_eq!(rows[0].plane, "native");
        assert_eq!(rows[1].kind, "ServiceBack");
    }

    #[test]
    fn test_cleanup_old_deletes_7d() {
        let s = open_mem();
        let now = 1_000_000u64;
        let old = now - (8 * 24 * 3600);
        s.record_health("fusion-mlx", "healthy", 1, old, "native")
            .unwrap();
        s.record_health("fusion-mlx", "healthy", 2, now, "native")
            .unwrap();
        let deleted = s.cleanup_old(now, 7).unwrap();
        assert_eq!(deleted, 1);
        let rows = s.recent_health("fusion-mlx", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].latency_ms, 2);
    }

    // issue #37: expired event rows are also swept, recent rows retained (both
    // health and event tables are pruned)
    #[test]
    fn test_cleanup_old_prunes_events_too() {
        let s = open_mem();
        let now = 1_000_000u64;
        let old = now - (8 * 24 * 3600);
        s.record_event("fusion-rag", "ServiceDown", "old", old, "native", "warn")
            .unwrap();
        s.record_event("fusion-rag", "ServiceDown", "new", now, "native", "warn")
            .unwrap();
        s.record_health("fusion-rag", "healthy", 1, old, "native")
            .unwrap();
        let deleted = s.cleanup_old(now, 7).unwrap();
        assert_eq!(deleted, 2, "old health row + old event row");
        let ev = s.recent_events("fusion-rag", 10).unwrap();
        assert_eq!(ev.len(), 1, "recent event row retained");
        assert_eq!(ev[0].detail, "new");
    }

    // issue #12: tripwire sample roundtrip (NULL latencies, breach/ci flags, ascending order)
    // + retention sweep covers the table
    #[test]
    fn test_tripwire_sample_roundtrip_and_cleanup() {
        let now = 1_000_000u64;
        let base = now - 1000;
        let s = open_mem();
        s.record_tripwire_sample(base, Some(16500), Some(4200), true, true)
            .unwrap();
        s.record_tripwire_sample(base + 30, None, Some(180), true, true)
            .unwrap();
        s.record_tripwire_sample(base + 60, Some(7), Some(3), true, false)
            .unwrap();
        let rows = s.recent_tripwire_samples(10).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].ts, base);
        assert_eq!(rows[0].gateway_ms, Some(16500));
        assert_eq!(rows[0].identity_ms, Some(4200));
        assert!(rows[0].breach && rows[0].ci_active);
        assert_eq!(rows[1].gateway_ms, None, "probe error stored as NULL");
        assert!(!rows[2].breach);
        // replace on same ts (INSERT OR REPLACE — one row per window)
        s.record_tripwire_sample(base + 60, Some(9), Some(9), true, true)
            .unwrap();
        assert_eq!(s.recent_tripwire_samples(10).unwrap().len(), 3);
        // retention sweep prunes old tripwire rows too
        let old = now - (8 * 24 * 3600);
        s.record_tripwire_sample(old, Some(1), Some(1), true, false)
            .unwrap();
        let deleted = s.cleanup_old(now, 7).unwrap();
        assert_eq!(deleted, 1, "old tripwire row pruned");
        let kept = s.recent_tripwire_samples(10).unwrap();
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].ts, base);
    }

    #[test]
    fn test_cleanup_keeps_boundary() {
        let s = open_mem();
        let now = 1_000_000u64;
        let boundary = now - (7 * 24 * 3600);
        s.record_health("fusion-mlx", "healthy", 9, boundary, "native")
            .unwrap();
        let deleted = s.cleanup_old(now, 7).unwrap();
        assert_eq!(deleted, 1);
        let rows = s.recent_health("fusion-mlx", 10).unwrap();
        assert_eq!(rows.len(), 0);
    }

    // compose-plane writes coexist with native; queries must return each plane correctly
    #[test]
    fn test_history_query_merges_planes() {
        let s = open_mem();
        s.record_health("redis", "healthy", 5, 100, "compose")
            .unwrap();
        s.record_health("redis", "unhealthy", 0, 200, "compose")
            .unwrap();
        s.record_health("fusion-mlx", "healthy", 3, 150, "native")
            .unwrap();
        // compose container health rows
        let compose_rows = s.recent_health("redis", 10).unwrap();
        assert_eq!(compose_rows.len(), 2);
        assert_eq!(compose_rows[0].plane, "compose");
        // native health rows
        let native_rows = s.recent_health("fusion-mlx", 10).unwrap();
        assert_eq!(native_rows.len(), 1);
        assert_eq!(native_rows[0].plane, "native");
        // events across planes
        s.record_event(
            "redis",
            "ContainerCrashLoop",
            "exited",
            300,
            "compose",
            "error",
        )
        .unwrap();
        s.record_event(
            "fusion-mlx",
            "ServiceDown",
            "refused",
            310,
            "native",
            "warn",
        )
        .unwrap();
        let ev_compose = s.recent_events("redis", 10).unwrap();
        assert_eq!(ev_compose[0].plane, "compose");
        let ev_native = s.recent_events("fusion-mlx", 10).unwrap();
        assert_eq!(ev_native[0].plane, "native");
    }

    // opening an old DB (no plane column) should migrate and add the column, existing rows default to native
    #[test]
    fn test_migration_defaults_existing_rows_native() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        // build an old-style DB without the plane column and insert one row
        {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "CREATE TABLE health_check (
                     ts INTEGER NOT NULL,
                     service TEXT NOT NULL,
                     status TEXT NOT NULL,
                     latency_ms INTEGER NOT NULL
                 );
                 CREATE TABLE event (
                     ts INTEGER NOT NULL,
                     service TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     detail TEXT NOT NULL
                 );
                 INSERT INTO health_check(ts, service, status, latency_ms)
                     VALUES (1000, 'fusion-mlx', 'healthy', 7);
                 INSERT INTO event(ts, service, kind, detail)
                     VALUES (1100, 'fusion-rag', 'ServiceDown', 'old');",
            )
            .unwrap();
        }
        // reopen to trigger the migration
        let s = HistoryStore::open(path).unwrap();
        let rows = s.recent_health("fusion-mlx", 10).unwrap();
        assert_eq!(rows.len(), 1, "existing row retained");
        assert_eq!(
            rows[0].plane, "native",
            "existing row plane defaults to native"
        );
        assert_eq!(rows[0].latency_ms, 7);
        let ev = s.recent_events("fusion-rag", 10).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(
            ev[0].plane, "native",
            "existing event row plane defaults to native"
        );
        assert_eq!(
            ev[0].severity, "warn",
            "existing event row severity defaults to warn"
        );
        // after migration, write a new row with an explicit compose plane
        s.record_health("redis", "healthy", 1, 2000, "compose")
            .unwrap();
        let new_rows = s.recent_health("redis", 10).unwrap();
        assert_eq!(new_rows[0].plane, "compose");
    }

    // PRD app-tiering: boot_pin access — set/get/overwrite/clear/all
    #[test]
    fn test_boot_pin_set_get_clear() {
        let s = open_mem();
        assert_eq!(s.get_boot_pin("fusion-rag").unwrap(), None, "no pin record");
        s.set_boot_pin("fusion-rag", Some(PinMode::Auto)).unwrap();
        assert_eq!(s.get_boot_pin("fusion-rag").unwrap(), Some(PinMode::Auto));
        // overwrite
        s.set_boot_pin("fusion-rag", Some(PinMode::Manual)).unwrap();
        assert_eq!(s.get_boot_pin("fusion-rag").unwrap(), Some(PinMode::Manual));
        // clear = fall back to registry semantics
        s.set_boot_pin("fusion-rag", None).unwrap();
        assert_eq!(s.get_boot_pin("fusion-rag").unwrap(), None);
    }

    #[test]
    fn test_boot_pin_all_and_persistence() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let s = HistoryStore::open(tmp.path()).unwrap();
            s.set_boot_pin("fusion-rag", Some(PinMode::Auto)).unwrap();
            s.set_boot_pin("fusion-health", Some(PinMode::Manual))
                .unwrap();
        }
        // reopen (simulates a daemon restart) — pins survive (acceptance §5.4)
        let s = HistoryStore::open(tmp.path()).unwrap();
        let mut pins = s.all_boot_pins().unwrap();
        pins.sort();
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0], ("fusion-health".into(), PinMode::Manual));
        assert_eq!(pins[1], ("fusion-rag".into(), PinMode::Auto));
    }

    #[test]
    fn test_pin_mode_parse() {
        assert_eq!(PinMode::parse("auto"), Some(PinMode::Auto));
        assert_eq!(PinMode::parse("manual"), Some(PinMode::Manual));
        assert_eq!(PinMode::parse("bogus"), None);
        assert_eq!(PinMode::Auto.as_str(), "auto");
        assert_eq!(PinMode::Manual.as_str(), "manual");
    }
}
