// issue #137: scheduled job dispatch for fusion-ops (driven by config [[jobs]], same semantics as backup scheduling).
// This file holds only pure logic (due check / daily time conversion); the executor lives in
// supervisor/mod.rs (spawn_job_task, same detached blocking pool as spawn_backup_task).
// Design tradeoff: fusion-ops is a one-shot Go binary (exits after a single run), not a fit for the
// probe/restart state machine (the issue's original "T2 tier" wiring would leave the probe permanently
// Down) — scheduling hangs off the supervisor tick loop, the meta cursor job_last:<name> is persisted,
// spawn advances it immediately (failures are not retried until the next due time, same as backup).
use crate::config::JobConfig;

// Due check (pure function, injectable timestamps for testability).
// daily_at: today's local HH:MM run time R — due when last < R <= now (once per day).
// every_sec: last==0 (never ran, due on daemon first start) or now-last >= interval.
pub fn job_due(now_ts: u64, last_ts: u64, job: &JobConfig) -> bool {
    match (&job.daily_at, job.every_sec) {
        (Some(hm), _) => match parse_hm(hm) {
            Some((h, m)) => match daily_run_ts(now_ts, h, m) {
                // run already passed (run <= now) and last predates run → due;
                // run not yet reached doesn't count (today's time hasn't come)
                Some(run) => run <= now_ts && last_ts < run,
                None => false,
            },
            None => false,
        },
        (None, Some(sec)) => last_ts == 0 || now_ts.saturating_sub(last_ts) >= sec,
        _ => false,
    }
}

// "HH:MM" → (h, m). Validation already happens at config load; this defensively returns None.
fn parse_hm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

// Unix seconds for today's local-timezone HH:MM. Strip now's intra-day offset to get local
// midnight, then add the offset.
// DST drift is acceptable (hour-scale, the daily auto catch-up semantics are unaffected).
// libc localtime_r (unix) / localtime_s (windows): the daemon has no chrono dependency, not adding one for this.
// Other targets have no local-timezone libc surface → None (daily_at unavailable, every_sec unaffected).
pub fn daily_run_ts(now_ts: u64, hour: u32, minute: u32) -> Option<u64> {
    let secs = now_ts as i64;
    #[cfg(unix)]
    {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            return None;
        }
        let sec_of_day = (tm.tm_hour as i64) * 3600 + (tm.tm_min as i64) * 60 + tm.tm_sec as i64;
        let local_midnight = secs - sec_of_day;
        Some((local_midnight + (hour as i64) * 3600 + (minute as i64) * 60) as u64)
    }
    #[cfg(windows)]
    {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // windows libc signature: localtime_s(tmDest, sourceTime) — argument order reversed vs unix
        if unsafe { libc::localtime_s(&mut tm, &t) } != 0 {
            return None;
        }
        let sec_of_day = (tm.tm_hour as i64) * 3600 + (tm.tm_min as i64) * 60 + tm.tm_sec as i64;
        let local_midnight = secs - sec_of_day;
        Some((local_midnight + (hour as i64) * 3600 + (minute as i64) * 60) as u64)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (hour, minute);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(at: Option<&str>, sec: Option<u64>) -> JobConfig {
        JobConfig {
            name: "j".into(),
            bin: "b".into(),
            args: Vec::new(),
            daily_at: at.map(String::from),
            every_sec: sec,
            timeout_sec: 60,
        }
    }

    fn daily(at: &str) -> JobConfig {
        job(Some(at), None)
    }

    fn every(sec: u64) -> JobConfig {
        job(None, Some(sec))
    }

    #[test]
    fn test_every_sec_first_run_and_interval() {
        let j = every(900);
        assert!(job_due(1_000, 0, &j), "never ran → due");
        assert!(!job_due(1_000, 500, &j), "interval not elapsed");
        assert!(job_due(1_400, 500, &j), "now-last >= 900 → due");
    }

    #[test]
    fn test_daily_due_after_run_time() {
        let j = daily("03:30");
        // last=0 → due once this morning's 03:30 has passed (daemon first-start catch-up)
        let now = 1_800_000_000;
        let run = daily_run_ts(now, 3, 30).unwrap();
        assert!(run < now);
        assert!(job_due(now, 0, &j));
        // last falls after this morning's run → no longer due
        assert!(!job_due(now, run + 1, &j));
        // last falls before the run (yesterday's) → due
        assert!(job_due(now, run - 1, &j));
    }

    #[test]
    fn test_daily_before_run_time_not_due() {
        // Construct "run not yet reached": now = before today's 23:59 run time — take the real now,
        // derive today's midnight, and assert both semantic branches for any point within the prior
        // day (timezone-dependent; branch on daily_run_ts's own result).
        let real = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let j = daily("23:59");
        let run = daily_run_ts(real, 23, 59).unwrap();
        if run > real {
            assert!(!job_due(real, 0, &j), "run not reached → not due");
        } else {
            assert!(job_due(real, 0, &j), "run already passed → due");
        }
    }

    #[test]
    fn test_invalid_hm_not_due() {
        let j = daily("25:00");
        assert!(!job_due(1_000, 0, &j), "invalid time defensively not due");
    }

    #[test]
    fn test_daily_run_ts_consistent() {
        let now = 1_800_000_000;
        let r1 = daily_run_ts(now, 3, 30).unwrap();
        let r2 = daily_run_ts(now, 3, 30).unwrap();
        assert_eq!(r1, r2);
        assert_eq!(r1 % 60, 0, "whole minute");
        // difference between two times on the same day is constant
        let r3 = daily_run_ts(now, 4, 30).unwrap();
        assert_eq!(r3 - r1, 3600);
    }
}
