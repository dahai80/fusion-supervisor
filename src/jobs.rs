// issue #137: fusion-ops 定时任务调度 (config [[jobs]] 驱动, 备份调度同款语义)。
// 本文件只放纯逻辑 (到期判定 / 每日时刻换算), 执行器在 supervisor/mod.rs
// (spawn_job_task, 与 spawn_backup_task 同款 detached blocking 池)。
// 设计取舍: fusion-ops 是一次性 Go 二进制 (单次跑完即退), 不适合 probe/restart
// 状态机 (issue 原话 "T2 tier" 按启动档位接入会让探活恒 Down) — 调度挂 supervisor
// tick 循环, meta 游标 job_last:<name> 持久化, spawn 即推进 (失败不重试到下个到期,
// 同 backup 语义)。
use crate::config::JobConfig;

// 到期判定 (纯函数, 注入时间戳可测)。
// daily_at: 今日本地 HH:MM 的 run 时刻 R — last < R ≤ now 即到期 (跨日一次)。
// every_sec: last==0 (从未跑过, daemon 首启即到期) 或 now-last >= 间隔。
pub fn job_due(now_ts: u64, last_ts: u64, job: &JobConfig) -> bool {
    match (&job.daily_at, job.every_sec) {
        (Some(hm), _) => match parse_hm(hm) {
            Some((h, m)) => match daily_run_ts(now_ts, h, m) {
                // run 已过 (run <= now) 且 last 早于 run → 到期;
                // run 未到不算 (今日还没到点)
                Some(run) => run <= now_ts && last_ts < run,
                None => false,
            },
            None => false,
        },
        (None, Some(sec)) => last_ts == 0 || now_ts.saturating_sub(last_ts) >= sec,
        _ => false,
    }
}

// "HH:MM" → (h, m)。校验在 config load 已做, 这里防御性返回 None。
fn parse_hm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

// 今日本地时区 HH:MM 对应的 unix 秒。now 本地秒内偏移去掉得本地零点, 再加偏移。
// DST 偏差可接受 (一小时级, 每天 auto 补跑语义不受影响)。
// libc localtime_r (unix) / localtime_s (windows): daemon 无 chrono 依赖, 不为此引入。
// 其他目标无本地时区 libc 面 → None (daily_at 不可用, every_sec 不受影响)。
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
        // windows libc 签名: localtime_s(tmDest, sourceTime) — 参数序与 unix 相反
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
        assert!(job_due(1_000, 0, &j), "从未跑过 → 到期");
        assert!(!job_due(1_000, 500, &j), "间隔未满");
        assert!(job_due(1_400, 500, &j), "now-last >= 900 → 到期");
    }

    #[test]
    fn test_daily_due_after_run_time() {
        let j = daily("03:30");
        // last=0 → 今早 03:30 已过即到期 (daemon 首启补跑)
        let now = 1_800_000_000;
        let run = daily_run_ts(now, 3, 30).unwrap();
        assert!(run < now);
        assert!(job_due(now, 0, &j));
        // last 落在今早 run 之后 → 不再到期
        assert!(!job_due(now, run + 1, &j));
        // last 落在 run 之前 (昨天的) → 到期
        assert!(job_due(now, run - 1, &j));
    }

    #[test]
    fn test_daily_before_run_time_not_due() {
        // 构造 "run 未到": now = 今天的 23:59 run 时刻之前 — 取真实 now,
        // 反推今日零点再往前推一天内的任意点断言两侧语义分支 (本地时区相关,
        // 用 daily_run_ts 自身结果分流)。
        let real = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let j = daily("23:59");
        let run = daily_run_ts(real, 23, 59).unwrap();
        if run > real {
            assert!(!job_due(real, 0, &j), "run 未到 → 不到期");
        } else {
            assert!(job_due(real, 0, &j), "run 已过 → 到期");
        }
    }

    #[test]
    fn test_invalid_hm_not_due() {
        let j = daily("25:00");
        assert!(!job_due(1_000, 0, &j), "非法时刻防御性不到期");
    }

    #[test]
    fn test_daily_run_ts_consistent() {
        let now = 1_800_000_000;
        let r1 = daily_run_ts(now, 3, 30).unwrap();
        let r2 = daily_run_ts(now, 3, 30).unwrap();
        assert_eq!(r1, r2);
        assert_eq!(r1 % 60, 0, "整分");
        // 同一天两个时刻差值恒定
        let r3 = daily_run_ts(now, 4, 30).unwrap();
        assert_eq!(r3 - r1, 3600);
    }
}
