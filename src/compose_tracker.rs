// Compose container alert throttling (issue #6): keeps flapping containers from
// re-emitting ServiceDown alerts every tick. Pure logic with injected u64 timestamps
// (deterministic tests, same pattern as policy.rs). tick_compose calls observe() and
// emits the returned decisions as alerts.
use std::collections::HashMap;

// Per-container snapshot from the previous round: supervisor_state + raw docker state +
// last alert ts + consecutive failure count + escalation latch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRecord {
    pub state: &'static str,
    pub raw_state: String,
    pub last_alert_ts: u64,
    pub fail_count: u32,
    pub escalated: bool,
}

// Alert decisions observe() returns for this round (0..2). tick_compose calls alerter.alert with them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeAlert {
    // First failure / recurrence outside the dedup window
    Down,
    // Docker state changed while failing (restarting→exited etc.) = new event
    StateChange,
    // Consecutive failures reached the threshold; escalate (once, reset on recovery)
    CrashLoop,
    // Failed → Healthy recovery
    Back,
}

pub struct ComposeTracker {
    dedup_sec: u64,
    escalation: u32,
    records: HashMap<String, ContainerRecord>,
}

impl ComposeTracker {
    pub fn new(dedup_sec: u64, escalation: u32) -> Self {
        Self {
            dedup_sec,
            escalation,
            records: HashMap::new(),
        }
    }

    // Observe a container's current state; returns this round's alert decisions and
    // updates the internal snapshot.
    // state = supervisor_state() ("Healthy"/"Failed"/...), raw_state = docker state.
    pub fn observe(
        &mut self,
        name: &str,
        raw_state: &str,
        state: &'static str,
        now_ts: u64,
    ) -> Vec<ComposeAlert> {
        let mut out = Vec::new();
        let prev = self.records.get(name).cloned();
        match (prev.as_ref(), state) {
            (None, "Failed") => {
                // first sighting is already failing
                out.push(ComposeAlert::Down);
                let fail_count = 1;
                let mut escalated = false;
                if fail_count >= self.escalation {
                    out.push(ComposeAlert::CrashLoop);
                    escalated = true;
                }
                self.records.insert(
                    name.to_string(),
                    ContainerRecord {
                        state,
                        raw_state: raw_state.to_string(),
                        last_alert_ts: now_ts,
                        fail_count,
                        escalated,
                    },
                );
            }
            (None, "Healthy") | (None, _) => {
                // first sighting not failing; record snapshot only, no alert
                self.records.insert(
                    name.to_string(),
                    ContainerRecord {
                        state,
                        raw_state: raw_state.to_string(),
                        last_alert_ts: 0,
                        fail_count: 0,
                        escalated: false,
                    },
                );
            }
            (Some(p), "Failed") => {
                let fail_count = p.fail_count + 1;
                let state_changed = p.raw_state != raw_state;
                let window_elapsed = now_ts.saturating_sub(p.last_alert_ts) > self.dedup_sec;
                // Emit ServiceDown when: docker state changed (new event) or recurrence outside the dedup window
                if state_changed || window_elapsed {
                    out.push(ComposeAlert::Down);
                }
                // Escalate: consecutive failures reached the threshold and not escalated yet
                let mut escalated = p.escalated;
                if fail_count >= self.escalation && !escalated {
                    out.push(ComposeAlert::CrashLoop);
                    escalated = true;
                }
                let last_alert_ts = if state_changed || window_elapsed {
                    now_ts
                } else {
                    p.last_alert_ts
                };
                self.records.insert(
                    name.to_string(),
                    ContainerRecord {
                        state,
                        raw_state: raw_state.to_string(),
                        last_alert_ts,
                        fail_count,
                        escalated,
                    },
                );
            }
            (Some(p), "Healthy") => {
                // Failed → Healthy: emit ServiceBack once, reset counters
                if p.state == "Failed" {
                    out.push(ComposeAlert::Back);
                }
                self.records.insert(
                    name.to_string(),
                    ContainerRecord {
                        state,
                        raw_state: raw_state.to_string(),
                        last_alert_ts: 0,
                        fail_count: 0,
                        escalated: false,
                    },
                );
            }
            (Some(_), _) => {
                // Other states (Starting/Restarting): update snapshot only, no alert
                self.records.insert(
                    name.to_string(),
                    ContainerRecord {
                        state,
                        raw_state: raw_state.to_string(),
                        last_alert_ts: 0,
                        fail_count: 0,
                        escalated: false,
                    },
                );
            }
        }
        out
    }

    // Drop snapshots for containers gone from ps (deleted/renamed) so stale records don't interfere.
    pub fn reap(&mut self, live: &[String]) {
        self.records.retain(|k, _| live.iter().any(|l| l == k));
    }

    // Test/diagnostic: fetch one container's snapshot.
    pub fn record(&self, name: &str) -> Option<&ContainerRecord> {
        self.records.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Same container exited for 5 consecutive ticks (within the dedup window) → only the first Down, rest suppressed
    #[test]
    fn test_dedup_repeated_failed_within_window() {
        // escalation set high; this test only exercises dedup (no CrashLoop mixed in)
        let mut t = ComposeTracker::new(300, 50);
        let d0 = t.observe("redis", "exited", "Failed", 1000);
        assert_eq!(d0, vec![ComposeAlert::Down]);
        for ts in [1002, 1004, 1006, 1008] {
            let d = t.observe("redis", "exited", "Failed", ts);
            assert!(
                d.is_empty(),
                "repeated failure with same state within dedup window should be suppressed, ts={ts} got {:?}",
                d
            );
        }
        assert_eq!(t.record("redis").unwrap().fail_count, 5);
    }

    // Docker state change (restarting→exited) → new alert, even within the dedup window
    #[test]
    fn test_state_change_re_alerts_within_window() {
        let mut t = ComposeTracker::new(300, 10);
        t.observe("redis", "restarting", "Failed", 1000);
        let d = t.observe("redis", "exited", "Failed", 1002);
        assert!(
            d.contains(&ComposeAlert::Down),
            "state change should re-alert, got {:?}",
            d
        );
    }

    // Failed → Healthy → ServiceBack once
    #[test]
    fn test_service_back_on_recovery() {
        let mut t = ComposeTracker::new(300, 10);
        t.observe("redis", "exited", "Failed", 1000);
        let d = t.observe("redis", "running", "Healthy", 1010);
        assert_eq!(d, vec![ComposeAlert::Back]);
        // another healthy tick → no more Back
        let d2 = t.observe("redis", "running", "Healthy", 1020);
        assert!(d2.is_empty(), "sustained health should not re-emit Back");
        assert_eq!(t.record("redis").unwrap().fail_count, 0);
    }

    // Consecutive failures reach the threshold → CrashLoop escalation (once), reset on recovery allows re-escalation
    #[test]
    fn test_escalation_after_threshold() {
        let mut t = ComposeTracker::new(300, 3);
        // tick1: Down, fail_count=1
        let d1 = t.observe("redis", "exited", "Failed", 1000);
        assert_eq!(d1, vec![ComposeAlert::Down]);
        // tick2: fail_count=2, below threshold
        let d2 = t.observe("redis", "exited", "Failed", 1002);
        assert!(d2.is_empty());
        // tick3: fail_count=3 reaches threshold → CrashLoop
        let d3 = t.observe("redis", "exited", "Failed", 1004);
        assert!(
            d3.contains(&ComposeAlert::CrashLoop),
            "reaching threshold should escalate, got {:?}",
            d3
        );
        assert!(t.record("redis").unwrap().escalated);
        // tick4: still failing, no re-escalation
        let d4 = t.observe("redis", "exited", "Failed", 1006);
        assert!(
            !d4.contains(&ComposeAlert::CrashLoop),
            "already escalated should not repeat, got {:?}",
            d4
        );
        // recovery → reset
        t.observe("redis", "running", "Healthy", 1010);
        assert!(!t.record("redis").unwrap().escalated);
    }

    // Recurrence outside the dedup window → Down again
    #[test]
    fn test_re_alert_after_window() {
        let mut t = ComposeTracker::new(300, 50);
        t.observe("redis", "exited", "Failed", 1000);
        // within window suppressed
        assert!(t.observe("redis", "exited", "Failed", 1100).is_empty());
        // outside window recurs → Down
        let d = t.observe("redis", "exited", "Failed", 1400);
        assert!(
            d.contains(&ComposeAlert::Down),
            "should recur outside the window, got {:?}",
            d
        );
    }

    // Container vanishes → reap clears its snapshot
    #[test]
    fn test_reap_removes_vanished() {
        let mut t = ComposeTracker::new(300, 5);
        t.observe("redis", "exited", "Failed", 1000);
        t.observe("postgres", "running", "Healthy", 1000);
        t.reap(&["redis".to_string()]);
        assert!(t.record("redis").is_some());
        assert!(
            t.record("postgres").is_none(),
            "vanished container should be reaped"
        );
    }
}
