pub mod policy;
pub mod runner;

use crate::alert::Alerter;
use crate::config::Config;
use crate::manifest::{Layer, ServiceDef, ServiceManifest};
use crate::probe::{ProbeResult, ProbeSpec};
use crate::store::HistoryStore;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Semaphore;

pub struct StatusEntry {
    pub name: String,
    pub state: runner::State,
    pub port: u16,
}

pub struct SupervisorCore {
    pub runners: Vec<runner::ServiceRunner>,
    pub store: HistoryStore,
    pub alerter: Alerter,
    pub cfg: Config,
    // 注入的 start 函数 (生产=bash start.sh, 测试=fake)。签名: (repo_dir, cmd) -> Result
    start_fn: Arc<dyn Fn(&str, &str) -> Result<()> + Send + Sync>,
    probe_fn: Arc<dyn Fn(&ProbeSpec) -> ProbeResult + Send + Sync>,
}

impl SupervisorCore {
    // 生产构造: 真 bash start.sh + 真 probe。probe_map 在 Task 11 填。
    pub fn new(defs: Vec<ServiceDef>, cfg: Config) -> Result<Self> {
        let store = HistoryStore::open(&cfg.db_path)?;
        let alerter = Alerter::new(cfg.alert_dedup_sec, cfg.alert_url.clone());
        let runners: Vec<_> = defs
            .into_iter()
            .map(|d| {
                runner::ServiceRunner::new(
                    d,
                    "restart",
                    real_probe,
                    real_start,
                    policy::RestartPolicy::new(
                        cfg.restart_window_sec,
                        cfg.restart_max,
                        cfg.backoff_max_sec,
                    ),
                )
            })
            .collect();
        Ok(Self {
            runners,
            store,
            alerter,
            cfg,
            start_fn: Arc::new(real_start_pair),
            probe_fn: Arc::new(real_probe_spec),
        })
    }

    // 测试构造: 注入 fake probe + fake start。defs 不经 layer 排序 (测试自带顺序)。
    #[cfg(test)]
    pub fn new_with_fns(
        defs: Vec<ServiceDef>,
        cfg: Config,
        probe_fn: impl Fn(&ProbeSpec) -> ProbeResult + Send + Sync + 'static,
        start_fn: impl Fn(&str, &str) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        let store = HistoryStore::open(tempfile::NamedTempFile::new().unwrap().path()).unwrap();
        let alerter = Alerter::new(cfg.alert_dedup_sec, cfg.alert_url.clone());
        let start_fn = Arc::new(start_fn);
        let probe_fn = Arc::new(probe_fn);
        let runners: Vec<_> = defs
            .into_iter()
            .map(|d| {
                let pf = probe_fn.clone();
                runner::ServiceRunner::new(
                    d,
                    "restart",
                    move || pf(&ProbeSpec::tcp(0)),
                    |_: &str| Ok(()),
                    policy::RestartPolicy::new(
                        cfg.restart_window_sec,
                        cfg.restart_max,
                        cfg.backoff_max_sec,
                    ),
                )
            })
            .collect();
        Self { runners, store, alerter, cfg, start_fn, probe_fn }
    }

    // up: 按 layer 序, 层内并发 cap。先探活, 已健康跳过 (避双启)。
    pub async fn up_all(&mut self) -> Result<()> {
        let sorted = ServiceManifest::sort_by_layer(
            self.runners.iter().map(|r| r.def.clone()).collect(),
        );
        let sem = Arc::new(Semaphore::new(self.cfg.max_concurrent_start));
        // 分层桶: 同 layer 内并发, 层间串行
        let mut by_layer: std::collections::BTreeMap<Layer, Vec<ServiceDef>> =
            std::collections::BTreeMap::new();
        for d in sorted {
            by_layer.entry(d.layer).or_default().push(d);
        }
        for (_layer, batch) in by_layer {
            let mut handles = Vec::new();
            for d in batch {
                let sem = sem.clone();
                let start_fn = self.start_fn.clone();
                let probe_fn = self.probe_fn.clone();
                let timeout = self.cfg.start_timeout_sec;
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    // 先探活: 已健康跳过 (R11: 用注入 probe_fn, 非真 probe)
                    let spec = ProbeSpec::tcp(d.port);
                    if matches!((probe_fn)(&spec), ProbeResult::Healthy { .. }) {
                        tracing::info!(svc = %d.name, "skip start (already up)");
                        return;
                    }
                    tracing::info!(svc = %d.name, repo = %d.repo_dir, "start.sh start");
                    let cmd = async { (start_fn)(&d.repo_dir, "start") };
                    match tokio::time::timeout(std::time::Duration::from_secs(timeout), cmd).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!(svc = %d.name, err = %e, "start fail"),
                        Err(_) => tracing::warn!(svc = %d.name, "start timeout {timeout}s"),
                    }
                }));
            }
            for h in handles {
                let _ = h.await;
            }
        }
        Ok(())
    }

    // down: 逆序 (cluster→domain→app→platform→core)。
    pub async fn down_all(&mut self) -> Result<()> {
        let mut defs: Vec<ServiceDef> = self.runners.iter().map(|r| r.def.clone()).collect();
        defs = ServiceManifest::sort_by_layer(defs);
        defs.reverse(); // 逆序停
        let sem = Arc::new(Semaphore::new(self.cfg.max_concurrent_start));
        let mut by_layer: std::collections::BTreeMap<Layer, Vec<ServiceDef>> =
            std::collections::BTreeMap::new();
        for d in defs {
            by_layer.entry(d.layer).or_default().push(d);
        }
        // R12: BTreeMap 默认按键升序 (Core 先), 故 .rev() 取降序 (Cluster→...→Core)
        for (_layer, batch) in by_layer.into_iter().rev() {
            let mut handles = Vec::new();
            for d in batch {
                let sem = sem.clone();
                let start_fn = self.start_fn.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    tracing::info!(svc = %d.name, repo = %d.repo_dir, "start.sh stop");
                    if let Err(e) = (start_fn)(&d.repo_dir, "stop") {
                        tracing::warn!(svc = %d.name, err = %e, "stop fail");
                    }
                }));
            }
            for h in handles {
                let _ = h.await;
            }
        }
        Ok(())
    }

    pub fn status_all(&self) -> Vec<StatusEntry> {
        self.runners
            .iter()
            .map(|r| StatusEntry {
                name: r.def.name.clone(),
                state: r.state,
                port: r.def.port,
            })
            .collect()
    }
}

// 生产 probe/start 实现 (Task 11 接 probe_map 后细化)
fn real_probe() -> ProbeResult {
    ProbeResult::Unhealthy { reason: crate::probe::UnhealthyReason::ConnectionRefused }
}
fn real_start(_cmd: &str) -> Result<()> {
    Ok(())
}
fn real_start_pair(_repo: &str, _cmd: &str) -> Result<()> {
    tracing::warn!("real start.sh not wired (Task 11)");
    Ok(())
}
fn real_probe_spec(_s: &ProbeSpec) -> ProbeResult {
    ProbeResult::Unhealthy { reason: crate::probe::UnhealthyReason::ConnectionRefused }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Layer, ServiceDef};
    use crate::probe::{ProbeResult, ProbeSpec};
    use std::sync::{Arc, Mutex};

    fn def(name: &str, layer: Layer, port: u16) -> ServiceDef {
        ServiceDef {
            name: name.into(), port,
            repo_dir: name.into(), purpose: "x".into(), fixed: false, layer,
        }
    }

    fn fake_cfg() -> Config {
        Config { max_concurrent_start: 2, start_timeout_sec: 5, ..Config::default() }
    }

    #[tokio::test]
    async fn test_up_all_skips_already_healthy() {
        // 所有服务探活即健康 → 不调 start.sh
        let defs = vec![def("fusion-mlx", Layer::Core, 11434), def("fusion-rag", Layer::App, 11436)];
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_p = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs, fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 }, // 探活恒健康
            move |svc, cmd| { calls_p.lock().unwrap().push(format!("{svc}/{cmd}")); Ok(()) },
        );
        core.up_all().await.unwrap();
        assert!(calls.lock().unwrap().is_empty(), "已健康应跳过不调 start.sh");
    }

    #[tokio::test]
    async fn test_up_all_starts_unhealthy_in_layer_order() {
        // 全不健康 → 按 layer 序起, 记录顺序
        let defs = vec![
            def("fusion-rag", Layer::App, 11436),
            def("fusion-mlx", Layer::Core, 11434),
        ];
        let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let order_p = order.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs, fake_cfg(),
            |_| ProbeResult::Unhealthy { reason: crate::probe::UnhealthyReason::ConnectionRefused },
            move |svc, _cmd| {
                order_p.lock().unwrap().push(svc.to_string());
                Ok(())
            },
        );
        core.up_all().await.unwrap();
        let o = order.lock().unwrap();
        assert_eq!(o[0], "fusion-mlx", "core 层先起");
        assert_eq!(o[1], "fusion-rag", "app 层后起");
    }

    #[tokio::test]
    async fn test_down_all_reverse_order() {
        let defs = vec![
            def("fusion-mlx", Layer::Core, 11434),
            def("fusion-rag", Layer::App, 11436),
        ];
        let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let order_p = order.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs, fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            move |svc, _cmd| { order_p.lock().unwrap().push(svc.to_string()); Ok(()) },
        );
        core.down_all().await.unwrap();
        let o = order.lock().unwrap();
        assert_eq!(o[0], "fusion-rag", "逆序: app 先停");
        assert_eq!(o[1], "fusion-mlx", "core 最后停");
    }

    #[tokio::test]
    async fn test_concurrency_cap_not_exceeded() {
        // 4 服务同层, cap=2 → 任意时刻最多 2 并发 start
        let defs: Vec<ServiceDef> = (0..4)
            .map(|i| def(&format!("fusion-s{}", i), Layer::App, 11480 + i))
            .collect();
        let inflight: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
        let max_seen: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
        let inflight_p = inflight.clone();
        let max_p = max_seen.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs, fake_cfg(),
            |_| ProbeResult::Unhealthy { reason: crate::probe::UnhealthyReason::ConnectionRefused },
            move |_svc, _cmd| {
                let mut cur = inflight_p.lock().unwrap();
                *cur += 1;
                let mut mx = max_p.lock().unwrap();
                if *cur > *mx { *mx = *cur; }
                std::thread::sleep(std::time::Duration::from_millis(20));
                *cur -= 1;
                Ok(())
            },
        );
        core.up_all().await.unwrap();
        let mx = *max_seen.lock().unwrap();
        assert!(mx <= 2, "并发上限 2, 实测峰值 {mx}");
    }
}
