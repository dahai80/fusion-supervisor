use std::collections::VecDeque;

// 移植 fusion-mlx _run_with_watchdog: 300s 滑窗, 5 次封顶, 退避 1→2→4→8→16→30 上限。
// 纯逻辑: 时间以 u64(unix sec) 注入, 不碰 SystemTime, 便于确定性单测。
pub struct RestartPolicy {
    window_sec: u64,
    max: u32,
    backoff_max: u64,
    crashes: VecDeque<u64>,
}

impl RestartPolicy {
    pub fn new(window_sec: u64, max: u32, backoff_max: u64) -> Self {
        Self {
            window_sec,
            max,
            backoff_max,
            crashes: VecDeque::new(),
        }
    }

    pub fn crash_count(&self) -> usize {
        self.crashes.len()
    }

    pub fn is_capped(&self) -> bool {
        self.crashes.len() > self.max as usize
    }

    // 记一次 crash (ts=unix sec). 返回 true = 已达封顶, 调用方应停重启+告警。
    // 先滑窗 (剔除 ts-window_sec 之前的), 再 push, 再判封顶。
    pub fn record_crash(&mut self, ts: u64) -> bool {
        let cutoff = ts.saturating_sub(self.window_sec);
        while let Some(&front) = self.crashes.front() {
            if front < cutoff {
                self.crashes.pop_front();
            } else {
                break;
            }
        }
        self.crashes.push_back(ts);
        tracing::debug!(
            crashes = self.crashes.len(),
            window = self.window_sec,
            "record_crash ts={ts}"
        );
        self.is_capped()
    }

    // 下次退避秒数: 2^(n-1), 上限 backoff_max; 封顶后恒 backoff_max (用于告警显示)。
    pub fn next_delay(&self) -> u64 {
        let n = self.crashes.len() as u32;
        if n == 0 {
            return 1;
        }
        let mut delay = 1u64;
        for _ in 0..(n.saturating_sub(1)) {
            delay = delay.saturating_mul(2);
            if delay >= self.backoff_max {
                return self.backoff_max;
            }
        }
        delay.min(self.backoff_max)
    }

    pub fn reset(&mut self) {
        self.crashes.clear();
        tracing::info!("restart policy reset");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> (u64, u32, u64) {
        (300, 5, 30)
    }

    #[test]
    fn test_backoff_sequence_capped() {
        let (w, m, bmax) = cfg();
        let mut p = RestartPolicy::new(w, m, bmax);
        let ts = 1000u64;
        // 每次重启前先记 crash, 再取 delay
        // crash 1: delay 1
        assert!(!p.record_crash(ts));
        assert_eq!(p.next_delay(), 1);
        // crash 2: 2, 3:4, 4:8, 5:16
        assert!(!p.record_crash(ts + 1));
        assert_eq!(p.next_delay(), 2);
        assert!(!p.record_crash(ts + 2));
        assert_eq!(p.next_delay(), 4);
        assert!(!p.record_crash(ts + 3));
        assert_eq!(p.next_delay(), 8);
        assert!(!p.record_crash(ts + 4));
        assert_eq!(p.next_delay(), 16);
        // crash 6: cap hit (max=5), delay 不再增长, 返回 true
        assert!(p.record_crash(ts + 5));
        // cap hit 后 next_delay 应返回 backoff_max
        assert_eq!(p.next_delay(), 30);
    }

    #[test]
    fn test_window_rolls_old_crashes_out() {
        let (w, m, bmax) = cfg();
        let mut p = RestartPolicy::new(w, m, bmax);
        // 5 crash 在 t=1000..1004
        for i in 0..5 {
            assert!(!p.record_crash(1000 + i));
        }
        assert_eq!(p.crash_count(), 5);
        // 过 301s, 老的全滑出窗
        assert!(!p.record_crash(1000 + 5 + 301));
        assert_eq!(p.crash_count(), 1);
        // delay 回到 1 (计数重置)
        assert_eq!(p.next_delay(), 1);
    }

    #[test]
    fn test_reset_after_cap() {
        let (w, m, bmax) = cfg();
        let mut p = RestartPolicy::new(w, m, bmax);
        for i in 0..6 {
            p.record_crash(1000 + i);
        }
        assert!(p.is_capped());
        p.reset();
        assert!(!p.is_capped());
        assert_eq!(p.crash_count(), 0);
        assert_eq!(p.next_delay(), 1);
    }

    #[test]
    fn test_window_edge_keep_within() {
        let (w, m, bmax) = cfg();
        let mut p = RestartPolicy::new(w, m, bmax);
        p.record_crash(1000);
        // 恰好 300s 边界: 仍算在窗内 (<= window)
        assert!(!p.record_crash(1300));
        assert_eq!(p.crash_count(), 2);
        // 301s: 第一条滑出
        assert!(!p.record_crash(1301));
        assert_eq!(p.crash_count(), 2);
    }
}
