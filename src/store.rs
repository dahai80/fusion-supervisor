use anyhow::{Context, Result};
use rusqlite::{Connection, params};

// Connection 是 Send 但 !Sync (内含 RefCell)。tick (async) 持 &HistoryStore 跨 .await,
// 需 &HistoryStore: Send → HistoryStore: Sync。包 std::sync::Mutex 使其 Send+Sync,
// 解锁 daemon tick 循环的 tokio::spawn (C1)。SQLite 连接本就单线程访问, Mutex 显式化。
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
                 plane TEXT NOT NULL DEFAULT 'native'
             );
             CREATE INDEX IF NOT EXISTS idx_health_svc ON health_check(service, ts);
             CREATE INDEX IF NOT EXISTS idx_event_svc ON event(service, ts);",
        )?;
        // 迁移: 旧库表无 plane 列 → ALTER 补列 (幂等, SQLite 无 ADD COLUMN IF NOT EXISTS,
        // 靠错误吞掉重复列)。旧行默认 'native' (DEFAULT 子句)。
        migrate_add_plane(&conn)?;
        tracing::info!(db = %path.display(), "history store open (WAL)");
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
        })
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
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO event(ts, service, kind, detail, plane) VALUES (?,?,?,?,?)",
            params![ts as i64, service, kind, detail, plane],
        )?;
        Ok(())
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
        tracing::info!(cutoff, deleted_health = h, deleted_event = e, "cleanup old");
        Ok(h + e)
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
            "SELECT ts, service, kind, detail, plane FROM (
                 SELECT ts, service, kind, detail, plane FROM event
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
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

// 迁移: 旧库表无 plane 列 → ALTER ADD COLUMN。SQLite 无 IF NOT EXISTS,
// 靠吞 "duplicate column name" 错误实现幂等。旧行 DEFAULT 'native'。
fn migrate_add_plane(conn: &Connection) -> Result<()> {
    for table in ["health_check", "event"] {
        let sql = format!("ALTER TABLE {table} ADD COLUMN plane TEXT NOT NULL DEFAULT 'native'");
        match conn.execute(&sql, []) {
            Ok(_) => {
                tracing::info!(%table, "migrate: add plane column");
            }
            // SQLite 无 ADD COLUMN IF NOT EXISTS, 重复列报 "duplicate column name" → 吞掉
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column") =>
            {
                tracing::debug!(%table, "migrate: plane column 已存在, 跳过");
            }
            Err(e) => return Err(e.into()),
        }
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
        s.record_event("fusion-rag", "ServiceDown", "conn refused", 5000, "native")
            .unwrap();
        s.record_event("fusion-rag", "ServiceBack", "recovered", 5300, "native")
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

    // compose plane 写入与 native 并存, 查询应正确返回各自 plane
    #[test]
    fn test_history_query_merges_planes() {
        let s = open_mem();
        s.record_health("redis", "healthy", 5, 100, "compose")
            .unwrap();
        s.record_health("redis", "unhealthy", 0, 200, "compose")
            .unwrap();
        s.record_health("fusion-mlx", "healthy", 3, 150, "native")
            .unwrap();
        // compose 容器健康行
        let compose_rows = s.recent_health("redis", 10).unwrap();
        assert_eq!(compose_rows.len(), 2);
        assert_eq!(compose_rows[0].plane, "compose");
        // native 健康行
        let native_rows = s.recent_health("fusion-mlx", 10).unwrap();
        assert_eq!(native_rows.len(), 1);
        assert_eq!(native_rows[0].plane, "native");
        // event 跨 plane
        s.record_event("redis", "ContainerCrashLoop", "exited", 300, "compose")
            .unwrap();
        s.record_event("fusion-mlx", "ServiceDown", "refused", 310, "native")
            .unwrap();
        let ev_compose = s.recent_events("redis", 10).unwrap();
        assert_eq!(ev_compose[0].plane, "compose");
        let ev_native = s.recent_events("fusion-mlx", 10).unwrap();
        assert_eq!(ev_native[0].plane, "native");
    }

    // 旧库 (无 plane 列) 打开后迁移应补列, 旧行默认 native
    #[test]
    fn test_migration_defaults_existing_rows_native() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        // 先建无 plane 列的旧库并写一行
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
        // 重新 open 触发迁移
        let s = HistoryStore::open(path).unwrap();
        let rows = s.recent_health("fusion-mlx", 10).unwrap();
        assert_eq!(rows.len(), 1, "旧行应保留");
        assert_eq!(rows[0].plane, "native", "旧行 plane 默认 native");
        assert_eq!(rows[0].latency_ms, 7);
        let ev = s.recent_events("fusion-rag", 10).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].plane, "native", "旧 event 行 plane 默认 native");
        // 迁移后再写新行, plane 显式 compose
        s.record_health("redis", "healthy", 1, 2000, "compose")
            .unwrap();
        let new_rows = s.recent_health("redis", 10).unwrap();
        assert_eq!(new_rows[0].plane, "compose");
    }
}
