use anyhow::{Context, Result};
use rusqlite::{Connection, params};

pub struct HistoryStore {
    pub conn: Connection,
}

#[derive(Debug, Clone)]
pub struct HealthRow {
    pub ts: u64,
    pub service: String,
    pub status: String,
    pub latency_ms: u64,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub ts: u64,
    pub service: String,
    pub kind: String,
    pub detail: String,
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
                 latency_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS event (
                 ts INTEGER NOT NULL,
                 service TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 detail TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_health_svc ON health_check(service, ts);
             CREATE INDEX IF NOT EXISTS idx_event_svc ON event(service, ts);",
        )?;
        tracing::info!(db = %path.display(), "history store open (WAL)");
        Ok(Self { conn })
    }

    pub fn record_health(
        &self,
        service: &str,
        status: &str,
        latency_ms: u64,
        ts: u64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO health_check(ts, service, status, latency_ms) VALUES (?,?,?,?)",
            params![ts as i64, service, status, latency_ms as i64],
        )?;
        Ok(())
    }

    pub fn record_event(&self, service: &str, kind: &str, detail: &str, ts: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO event(ts, service, kind, detail) VALUES (?,?,?,?)",
            params![ts as i64, service, kind, detail],
        )?;
        Ok(())
    }

    pub fn cleanup_old(&self, now_ts: u64, retain_days: u32) -> Result<usize> {
        let cutoff = now_ts.saturating_sub((retain_days as u64) * 24 * 3600);
        let h = self.conn.execute(
            "DELETE FROM health_check WHERE ts <= ?",
            params![cutoff as i64],
        )?;
        let e = self
            .conn
            .execute("DELETE FROM event WHERE ts <= ?", params![cutoff as i64])?;
        tracing::info!(cutoff, deleted_health = h, deleted_event = e, "cleanup old");
        Ok(h + e)
    }

    pub fn recent_health(&self, service: &str, limit: u32) -> Result<Vec<HealthRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, service, status, latency_ms FROM (
                 SELECT ts, service, status, latency_ms FROM health_check
                 WHERE service = ? ORDER BY ts DESC LIMIT ?
             ) ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![service, limit], |r| {
            Ok(HealthRow {
                ts: r.get::<_, i64>(0)? as u64,
                service: r.get(1)?,
                status: r.get(2)?,
                latency_ms: r.get::<_, i64>(3)? as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn recent_events(&self, service: &str, limit: u32) -> Result<Vec<EventRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, service, kind, detail FROM (
                 SELECT ts, service, kind, detail FROM event
                 WHERE service = ? ORDER BY ts DESC LIMIT ?
             ) ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![service, limit], |r| {
            Ok(EventRow {
                ts: r.get::<_, i64>(0)? as u64,
                service: r.get(1)?,
                kind: r.get(2)?,
                detail: r.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
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
        s.record_health("fusion-mlx", "healthy", 12, 1000).unwrap();
        s.record_health("fusion-mlx", "unhealthy", 0, 2000).unwrap();
        let rows = s.recent_health("fusion-mlx", 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].status, "healthy");
        assert_eq!(rows[0].latency_ms, 12);
        assert_eq!(rows[1].status, "unhealthy");
    }

    #[test]
    fn test_record_and_query_event() {
        let s = open_mem();
        s.record_event("fusion-rag", "ServiceDown", "conn refused", 5000)
            .unwrap();
        s.record_event("fusion-rag", "ServiceBack", "recovered", 5300)
            .unwrap();
        let rows = s.recent_events("fusion-rag", 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "ServiceDown");
        assert_eq!(rows[1].kind, "ServiceBack");
    }

    #[test]
    fn test_cleanup_old_deletes_7d() {
        let s = open_mem();
        let now = 1_000_000u64;
        let old = now - (8 * 24 * 3600);
        s.record_health("fusion-mlx", "healthy", 1, old).unwrap();
        s.record_health("fusion-mlx", "healthy", 2, now).unwrap();
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
        s.record_health("fusion-mlx", "healthy", 9, boundary)
            .unwrap();
        let deleted = s.cleanup_old(now, 7).unwrap();
        assert_eq!(deleted, 1);
        let rows = s.recent_health("fusion-mlx", 10).unwrap();
        assert_eq!(rows.len(), 0);
    }
}
