// compose 容器告警节流 (issue #6): 防止 flapping 容器每 tick 刷 ServiceDown 告警。
// 纯逻辑, 注入 u64 时间戳 (确定性测试, 同 policy.rs 模式)。tick_compose 调 observe() 取决策再发告警。
use std::collections::HashMap;

// 单容器上一轮快照: supervisor_state + docker 原始 state + 上次告警 ts + 连续失败计数 + 已升级标记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRecord {
    pub state: &'static str,
    pub raw_state: String,
    pub last_alert_ts: u64,
    pub fail_count: u32,
    pub escalated: bool,
}

// observe() 返回的本轮应发告警决策 (0..2)。tick_compose 据此调 alerter.alert。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeAlert {
    // 首次失败 / 去抖窗外复发
    Down,
    // 失败期间 docker state 变化 (restarting→exited 等) = 新事件
    StateChange,
    // 连续失败达阈值, 升级告警 (仅一次, 恢复后重置)
    CrashLoop,
    // 失败→健康 恢复
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

    // 观察一容器当前状态, 返回本轮应发告警决策 + 更新内部快照。
    // state = supervisor_state() ("Healthy"/"Failed"/...), raw_state = docker state。
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
                // 首见即失败
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
                // 首见非失败, 仅记快照, 不告警
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
                // 发 ServiceDown 条件: docker state 变化 (新事件) 或 去抖窗外复发
                if state_changed || window_elapsed {
                    out.push(ComposeAlert::Down);
                }
                // 升级: 连续失败达阈值且未升级过
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
                // 失败→健康: 发 ServiceBack 一次, 重置计数
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
                // 其他态 (Starting/Restarting) 仅更新快照, 不告警
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

    // 移除 ps 中已消失的容器 (容器被删/重命名), 避免陈旧快照干扰。
    pub fn reap(&mut self, live: &[String]) {
        self.records.retain(|k, _| live.iter().any(|l| l == k));
    }

    // 测试/诊断: 取一容器快照。
    pub fn record(&self, name: &str) -> Option<&ContainerRecord> {
        self.records.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 同容器 exited 连续 5 tick (去抖窗内) → 仅首发 Down, 其余抑制
    #[test]
    fn test_dedup_repeated_failed_within_window() {
        // escalation 设高, 此测试只验证去抖 (不混入 CrashLoop)
        let mut t = ComposeTracker::new(300, 50);
        let d0 = t.observe("redis", "exited", "Failed", 1000);
        assert_eq!(d0, vec![ComposeAlert::Down]);
        for ts in [1002, 1004, 1006, 1008] {
            let d = t.observe("redis", "exited", "Failed", ts);
            assert!(
                d.is_empty(),
                "去抖窗内同 state 重复失败应抑制, ts={ts} 得 {:?}",
                d
            );
        }
        assert_eq!(t.record("redis").unwrap().fail_count, 5);
    }

    // docker state 变化 (restarting→exited) → 新告警, 即使在去抖窗内
    #[test]
    fn test_state_change_re_alerts_within_window() {
        let mut t = ComposeTracker::new(300, 10);
        t.observe("redis", "restarting", "Failed", 1000);
        let d = t.observe("redis", "exited", "Failed", 1002);
        assert!(
            d.contains(&ComposeAlert::Down),
            "state 变化应重新告警, 得 {:?}",
            d
        );
    }

    // 失败→健康 → ServiceBack 一次
    #[test]
    fn test_service_back_on_recovery() {
        let mut t = ComposeTracker::new(300, 10);
        t.observe("redis", "exited", "Failed", 1000);
        let d = t.observe("redis", "running", "Healthy", 1010);
        assert_eq!(d, vec![ComposeAlert::Back]);
        // 再 tick 健康 → 不再发 Back
        let d2 = t.observe("redis", "running", "Healthy", 1020);
        assert!(d2.is_empty(), "持续健康不应再发 Back");
        assert_eq!(t.record("redis").unwrap().fail_count, 0);
    }

    // 连续失败达阈值 → CrashLoop 升级 (仅一次), 恢复后重置可再升级
    #[test]
    fn test_escalation_after_threshold() {
        let mut t = ComposeTracker::new(300, 3);
        // tick1: Down, fail_count=1
        let d1 = t.observe("redis", "exited", "Failed", 1000);
        assert_eq!(d1, vec![ComposeAlert::Down]);
        // tick2: fail_count=2, 未达阈值
        let d2 = t.observe("redis", "exited", "Failed", 1002);
        assert!(d2.is_empty());
        // tick3: fail_count=3 达阈值 → CrashLoop
        let d3 = t.observe("redis", "exited", "Failed", 1004);
        assert!(
            d3.contains(&ComposeAlert::CrashLoop),
            "达阈值应升级, 得 {:?}",
            d3
        );
        assert!(t.record("redis").unwrap().escalated);
        // tick4: 仍失败, 不再升级
        let d4 = t.observe("redis", "exited", "Failed", 1006);
        assert!(
            !d4.contains(&ComposeAlert::CrashLoop),
            "已升级不应重复, 得 {:?}",
            d4
        );
        // 恢复 → 重置
        t.observe("redis", "running", "Healthy", 1010);
        assert!(!t.record("redis").unwrap().escalated);
    }

    // 去抖窗外复发 → 重新发 Down
    #[test]
    fn test_re_alert_after_window() {
        let mut t = ComposeTracker::new(300, 50);
        t.observe("redis", "exited", "Failed", 1000);
        // 窗内抑制
        assert!(t.observe("redis", "exited", "Failed", 1100).is_empty());
        // 窗外复发 → Down
        let d = t.observe("redis", "exited", "Failed", 1400);
        assert!(d.contains(&ComposeAlert::Down), "窗外应复发, 得 {:?}", d);
    }

    // 容器消失 → reap 清除快照
    #[test]
    fn test_reap_removes_vanished() {
        let mut t = ComposeTracker::new(300, 5);
        t.observe("redis", "exited", "Failed", 1000);
        t.observe("postgres", "running", "Healthy", 1000);
        t.reap(&["redis".to_string()]);
        assert!(t.record("redis").is_some());
        assert!(t.record("postgres").is_none(), "消失容器应被 reap");
    }
}
