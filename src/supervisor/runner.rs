use crate::alert::{Alert, AlertKind, Alerter};
use crate::manifest::ServiceDef;
use crate::probe::ProbeResult;
use crate::store::HistoryStore;
use crate::supervisor::policy::RestartPolicy;
use anyhow::Result;

pub type ProbeFn = Box<dyn Fn() -> ProbeResult + Send + Sync>;
pub type StartFn = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;

// 状态机: Stopped→Starting→Healthy→(Unhealthy)→Restarting→Healthy|Failed
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum State {
    Stopped,
    Starting,
    Healthy,
    Unhealthy,
    Restarting,
    Failed,
}

pub struct ServiceRunner {
    pub def: ServiceDef,
    pub state: State,
    pub policy: RestartPolicy,
    pub restart_cmd: String, // "restart" | "stop+start"
    probe_fn: ProbeFn,
    start_fn: StartFn,
}

impl ServiceRunner {
    pub fn new(
        def: ServiceDef,
        restart_cmd: &str,
        probe_fn: impl Fn() -> ProbeResult + Send + Sync + 'static,
        start_fn: impl Fn(&str) -> Result<()> + Send + Sync + 'static,
        policy: RestartPolicy,
    ) -> Self {
        Self {
            def,
            state: State::Stopped,
            policy,
            restart_cmd: restart_cmd.to_string(),
            probe_fn: Box::new(probe_fn),
            start_fn: Box::new(start_fn),
        }
    }

    // 单步监督。now_ts=unix sec (注入, 确定性)。
    pub async fn tick(
        &mut self,
        now_ts: u64,
        store: &HistoryStore,
        alerter: &mut Alerter,
    ) -> Result<()> {
        let result = (self.probe_fn)();
        self.persist_health(&result, now_ts, store);
        match result {
            ProbeResult::Healthy { latency_ms } => {
                let was_down = matches!(self.state, State::Unhealthy | State::Restarting);
                self.state = State::Healthy;
                if was_down {
                    tracing::info!(svc = %self.def.name, "recovered");
                    let _ = alerter
                        .alert(Alert {
                            service: self.def.name.clone(),
                            kind: AlertKind::ServiceBack,
                            detail: format!("latency {}ms", latency_ms),
                            ts: now_ts,
                        })
                        .await;
                    self.policy.reset();
                }
            }
            ProbeResult::Unhealthy { reason } => {
                self.handle_unhealthy(reason, now_ts, alerter).await;
            }
        }
        Ok(())
    }

    fn persist_health(&self, result: &ProbeResult, now_ts: u64, store: &HistoryStore) {
        let (status, latency) = match result {
            ProbeResult::Healthy { latency_ms } => ("healthy", *latency_ms),
            ProbeResult::Unhealthy { .. } => ("unhealthy", 0),
        };
        if let Err(e) = store.record_health(&self.def.name, status, latency, now_ts) {
            tracing::warn!(svc = %self.def.name, err = %e, "health write fail (非致命, 继续监督)");
        }
    }

    async fn handle_unhealthy(
        &mut self,
        reason: crate::probe::UnhealthyReason,
        now_ts: u64,
        alerter: &mut Alerter,
    ) {
        // 防风暴: BadPath 不重启不记 crash, 只告警路径错
        if !reason.restart_eligible() {
            tracing::warn!(
                svc = %self.def.name,
                reason = ?reason,
                "探活不可达但非宕机信号, 不重启 (查 probe_map 路径)"
            );
            self.state = State::Unhealthy;
            let _ = alerter
                .alert(Alert {
                    service: self.def.name.clone(),
                    kind: AlertKind::RestartFailed,
                    detail: format!("probe path suspect: {:?}", reason),
                    ts: now_ts,
                })
                .await;
            return;
        }
        // 真宕机信号 (ConnectionRefused/Timeout)
        match self.state {
            State::Healthy | State::Starting => {
                tracing::warn!(svc = %self.def.name, reason = ?reason, "服务宕机");
                self.state = State::Unhealthy;
                let _ = alerter
                    .alert(Alert {
                        service: self.def.name.clone(),
                        kind: AlertKind::ServiceDown,
                        detail: format!("{:?}", reason),
                        ts: now_ts,
                    })
                    .await;
                // 首次宕机不立即重启, 等下一 tick (避免抖动误判)
            }
            State::Unhealthy | State::Restarting => {
                let capped = self.policy.record_crash(now_ts);
                if capped {
                    tracing::error!(svc = %self.def.name, "crash-window 封顶, 标 Failed 停重启");
                    self.state = State::Failed;
                    let _ = alerter
                        .alert(Alert {
                            service: self.def.name.clone(),
                            kind: AlertKind::RestartCapHit,
                            detail: format!("cap {} hit", self.policy.crash_count()),
                            ts: now_ts,
                        })
                        .await;
                    return;
                }
                let delay = self.policy.next_delay();
                tracing::info!(svc = %self.def.name, delay, "退避后重启");
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                if let Err(e) = self.invoke_start() {
                    tracing::warn!(svc = %self.def.name, err = %e, "start.sh fail");
                    let _ = alerter
                        .alert(Alert {
                            service: self.def.name.clone(),
                            kind: AlertKind::RestartFailed,
                            detail: e.to_string(),
                            ts: now_ts,
                        })
                        .await;
                } else {
                    self.state = State::Restarting;
                }
            }
            State::Failed | State::Stopped => {
                // 不动作: Failed 需人工 fusion-sv restart <svc> 复位
            }
        }
    }

    fn invoke_start(&self) -> Result<()> {
        if self.restart_cmd == "restart" {
            (self.start_fn)("restart")
        } else {
            (self.start_fn)("stop")?;
            (self.start_fn)("start")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Layer;
    use crate::probe::{ProbeResult, UnhealthyReason};
    use std::sync::{Arc, Mutex};

    fn def() -> ServiceDef {
        ServiceDef {
            name: "fusion-rag".into(),
            port: 11436,
            repo_dir: "fusion-rag".into(),
            purpose: "kb".into(),
            fixed: false,
            layer: Layer::App,
        }
    }

    // 可变探活结果 + 记录 start.sh 调用序列, 供断言
    #[allow(clippy::type_complexity)]
    fn harness(
        probe_results: ProbeResult,
    ) -> (
        Arc<Mutex<Vec<String>>>,
        Arc<Mutex<ProbeResult>>,
        impl Fn() -> ProbeResult + Send + Sync,
        impl Fn(&str) -> Result<()> + Send + Sync,
    ) {
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let probe_cell: Arc<Mutex<ProbeResult>> = Arc::new(Mutex::new(probe_results));
        let calls_p = calls.clone();
        let probe_p = probe_cell.clone();
        let probe_fn = move || probe_p.lock().unwrap().clone();
        let start_fn = move |cmd: &str| -> Result<()> {
            calls_p.lock().unwrap().push(cmd.to_string());
            Ok(())
        };
        (calls, probe_cell, probe_fn, start_fn)
    }

    fn store() -> HistoryStore {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        HistoryStore::open(tmp.path()).unwrap()
    }

    #[tokio::test]
    async fn test_healthy_stays_healthy() {
        let (calls, _, pf, sf) = harness(ProbeResult::Healthy { latency_ms: 5 });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Healthy));
        assert!(calls.lock().unwrap().is_empty()); // 不调 start.sh
    }

    #[tokio::test]
    async fn test_healthy_to_unhealthy_emits_down_no_restart_yet() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        // 首次宕机: 标 Unhealthy + 告警, 但尚未重启 (重启在下一 tick)
        assert!(matches!(r.state, State::Unhealthy));
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(r.policy.crash_count(), 0);
    }

    #[tokio::test]
    async fn test_unhealthy_records_crash_and_restarts() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy; // 已标 down, 这次进重启流
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Restarting));
        assert_eq!(r.policy.crash_count(), 1);
        assert_eq!(*calls.lock().unwrap(), vec!["restart".to_string()]);
    }

    #[tokio::test]
    async fn test_cap_hit_marks_failed_no_restart() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        // 预填 5 次 crash, 这次第 6 次封顶 (R10: policy is_capped 用 > max, max=5 → 第 6 次封顶)
        for i in 0..5 {
            r.policy.record_crash(900 + i);
        }
        r.state = State::Unhealthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Failed));
        assert!(calls.lock().unwrap().is_empty()); // 封顶不重启
    }

    #[tokio::test]
    async fn test_badpath_no_restart_no_crash() {
        // 防风暴核心: BadPath(404) 不重启, 不记 crash
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::BadPath(404),
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Unhealthy));
        assert!(calls.lock().unwrap().is_empty()); // 不调 start.sh
        assert_eq!(r.policy.crash_count(), 0); // 不记 crash
        // 再 tick 一次 (state=Unhealthy): BadPath 仍不重启
        r.tick(1001, &s, &mut al).await.unwrap();
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(r.policy.crash_count(), 0);
    }

    #[tokio::test]
    async fn test_recovery_emits_back() {
        let (calls, probe_cell, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // 改探活为健康, tick 应恢复
        *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 8 };
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Healthy));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_stop_plus_start_when_no_restart_cmd() {
        // restart_cmd="stop+start" (fusion-store 无 restart)
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "stop+start", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["stop".to_string(), "start".to_string()]
        );
    }
}
