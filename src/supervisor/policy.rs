use std::collections::VecDeque;

// Ported from fusion-mlx _run_with_watchdog: 300s sliding window, 5-crash cap, backoff 1→2→4→8→16→30 max.
// Pure logic: time injected as u64 (unix sec), never touches SystemTime, for deterministic unit tests.
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

    // Record one crash (ts=unix sec). Returns true = cap reached, caller should stop restarting + alert.
    // Slide window first (drop entries older than ts-window_sec), then push, then check cap.
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

    // Next backoff seconds: 2^(n-1), capped at backoff_max; always backoff_max after cap (for alert display).
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
        // Record crash before each restart, then take delay
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
        // crash 6: cap hit (max=5), delay stops growing, returns true
        assert!(p.record_crash(ts + 5));
        // after cap hit, next_delay should return backoff_max
        assert_eq!(p.next_delay(), 30);
    }

    #[test]
    fn test_window_rolls_old_crashes_out() {
        let (w, m, bmax) = cfg();
        let mut p = RestartPolicy::new(w, m, bmax);
        // 5 crashes at t=1000..1004
        for i in 0..5 {
            assert!(!p.record_crash(1000 + i));
        }
        assert_eq!(p.crash_count(), 5);
        // After 301s, all old crashes slide out of the window
        assert!(!p.record_crash(1000 + 5 + 301));
        assert_eq!(p.crash_count(), 1);
        // delay back to 1 (count reset)
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
        // Exactly 300s boundary: still counts as in-window (<= window)
        assert!(!p.record_crash(1300));
        assert_eq!(p.crash_count(), 2);
        // 301s: first entry slides out
        assert!(!p.record_crash(1301));
        assert_eq!(p.crash_count(), 2);
    }
}
