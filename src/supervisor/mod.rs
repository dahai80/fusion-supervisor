pub mod policy;
pub mod runner;

use crate::alert::Alerter;
use crate::compose::{ComposeBackend, DockerCompose};
use crate::config::Config;
use crate::manifest::{Layer, ServiceDef, ServiceManifest};
use crate::probe::{ProbeResult, ProbeSpec, probe};
use crate::probe_map;
use crate::store::HistoryStore;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Semaphore;

// 签名: (repo_dir, cmd, env_overrides)。env_overrides 为注入连接 env (issue #8)。
pub type ArcStartFn = Arc<dyn Fn(&str, &str, &[(String, String)]) -> Result<()> + Send + Sync>;
pub type ArcProbeFn = Arc<dyn Fn(&ProbeSpec) -> ProbeResult + Send + Sync>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StatusEntry {
    pub name: String,
    pub state: String, // State 的字符串形式, 兼容容器平面 (Failed/Healthy/...)
    pub port: u16,
    pub plane: String, // "native" | "compose"
}

pub struct SupervisorCore {
    pub runners: Vec<runner::ServiceRunner>,
    pub store: HistoryStore,
    pub alerter: Alerter,
    pub cfg: Config,
    // 注入的 start 函数 (生产=bash start.sh, 测试=fake)。签名: (repo_dir, cmd) -> Result
    start_fn: ArcStartFn,
    probe_fn: ArcProbeFn,
    // compose 平面 (可选)。enabled=false 时为 None, 保留仅 native 行为。
    compose: Option<Box<dyn ComposeBackend>>,
    // compose 容器告警节流 (issue #6): 防 flapping 容器每 tick 刷告警。
    compose_tracker: crate::compose_tracker::ComposeTracker,
}

impl SupervisorCore {
    // 生产构造: 真 bash start.sh + 真 probe。probe_map 填 spec + restart_cmd。
    pub fn new(defs: Vec<ServiceDef>, cfg: Config) -> Result<Self> {
        let store = HistoryStore::open(&cfg.db_path)?;
        let alerter = Alerter::new(cfg.alert_dedup_sec, cfg.alert_url.clone());
        let runners: Vec<_> = defs
            .into_iter()
            .map(|d| {
                let spec = probe_map::probe_spec_for(&d.name, d.port);
                let cmd = probe_map::restart_cmd_for(&d.name).to_string();
                let repo_dir = d.repo_dir.clone();
                let policy = policy::RestartPolicy::new(
                    cfg.restart_window_sec,
                    cfg.restart_max,
                    cfg.backoff_max_sec,
                );
                runner::ServiceRunner::new(
                    d,
                    &cmd,
                    move || real_probe_spec(&spec),
                    move |c: &str| real_start_pair(&repo_dir, c, &[]),
                    policy,
                )
            })
            .collect();
        let compose = build_compose(&cfg);
        let compose_tracker = crate::compose_tracker::ComposeTracker::new(
            cfg.alert_dedup_sec,
            cfg.container_crash_escalation,
        );
        Ok(Self {
            runners,
            store,
            alerter,
            cfg,
            start_fn: Arc::new(real_start_pair),
            probe_fn: Arc::new(real_probe_spec),
            compose,
            compose_tracker,
        })
    }

    // 测试构造: 注入 fake probe + fake start。defs 不经 layer 排序 (测试自带顺序)。
    // compose 可选注入 (None=仅 native, Some=测试 compose 编排)。
    pub fn new_with_fns(
        defs: Vec<ServiceDef>,
        cfg: Config,
        probe_fn: impl Fn(&ProbeSpec) -> ProbeResult + Send + Sync + 'static,
        start_fn: impl Fn(&str, &str, &[(String, String)]) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self::new_with_fns_and_compose(defs, cfg, probe_fn, start_fn, None)
    }

    // 测试构造 + compose 注入。
    pub fn new_with_fns_and_compose(
        defs: Vec<ServiceDef>,
        cfg: Config,
        probe_fn: impl Fn(&ProbeSpec) -> ProbeResult + Send + Sync + 'static,
        start_fn: impl Fn(&str, &str, &[(String, String)]) -> Result<()> + Send + Sync + 'static,
        compose: Option<Box<dyn ComposeBackend>>,
    ) -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let db_path =
            std::env::temp_dir().join(format!("fusion-sv-test-{}-{}.db", std::process::id(), seq));
        let store = HistoryStore::open(&db_path).unwrap();
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
        let compose_tracker = crate::compose_tracker::ComposeTracker::new(
            cfg.alert_dedup_sec,
            cfg.container_crash_escalation,
        );
        Self {
            runners,
            store,
            alerter,
            cfg,
            start_fn,
            probe_fn,
            compose,
            compose_tracker,
        }
    }

    // up: compose 平面先 (sidecar), native 按 layer 序, business 容器最后。
    // enabled=false 跳过 compose, 仅 native (保留单节点开发行为)。
    pub async fn up_all(&mut self) -> Result<()> {
        // 1. compose sidecar 先 (traefik/redis/postgres/qdrant), 等健康
        if let Some(cb) = &self.compose {
            tracing::info!("up: compose sidecar 平面先起");
            cb.up_sidecar()?;
        }
        // 2. native 按 layer 序 (core→...→cluster), 层内并发 cap, 探活跳过已健康
        self.up_native().await?;
        // 3. compose business 容器 (model-hub/cowork), 依赖 sidecar 健康
        if let Some(cb) = &self.compose {
            tracing::info!("up: compose business 容器起 (依赖 sidecar)");
            cb.up_business()?;
        }
        Ok(())
    }

    // native up: 按 layer 序, 层内并发 cap。先探活, 已健康跳过 (避双启)。
    async fn up_native(&mut self) -> Result<()> {
        let sorted =
            ServiceManifest::sort_by_layer(self.runners.iter().map(|r| r.def.clone()).collect());
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
                let compose_enabled = self.cfg.compose.enabled;
                let env_file = self.cfg.compose.env_file.clone();
                // 返回 (服务名, 是否跳过=已健康, 启动结果), join 后据其写 runner.state
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    // 先探活: 已健康跳过 (R11: 用注入 probe_fn, 非真 probe)
                    let spec = ProbeSpec::tcp(d.port);
                    if matches!((probe_fn)(&spec), ProbeResult::Healthy { .. }) {
                        tracing::info!(svc = %d.name, "skip start (already up)");
                        return (d.name, true, Ok(()));
                    }
                    // issue #8: compose enabled 时, 依赖 sidecar 的服务注入连接 env。
                    // fail-closed: 解析失败 → 拒绝启动 (非静默降级)。
                    let env_overrides: Vec<(String, String)> = if compose_enabled
                        && crate::env_map::needs_injection(&d.name)
                    {
                        match crate::env_map::native_env_for(&d.name, &env_file) {
                            Ok(m) => m.into_iter().collect(),
                            Err(e) => {
                                tracing::error!(svc = %d.name, err = %e, "env 注入解析失败, fail-closed 拒绝启动");
                                return (d.name, false, Err(e));
                            }
                        }
                    } else {
                        Vec::new()
                    };
                    tracing::info!(svc = %d.name, repo = %d.repo_dir, env_n = env_overrides.len(), "start.sh start");
                    let cmd = async { (start_fn)(&d.repo_dir, "start", &env_overrides) };
                    match tokio::time::timeout(std::time::Duration::from_secs(timeout), cmd).await {
                        Ok(Ok(())) => (d.name, false, Ok(())),
                        Ok(Err(e)) => {
                            tracing::warn!(svc = %d.name, err = %e, "start fail");
                            (d.name, false, Err(e))
                        }
                        Err(_) => {
                            let e = anyhow::anyhow!("start timeout {timeout}s");
                            tracing::warn!(svc = %d.name, "start timeout {timeout}s");
                            (d.name, false, Err(e))
                        }
                    }
                }));
            }
            for h in handles {
                if let Ok((name, skipped_healthy, start_res)) = h.await
                    && let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name)
                {
                    if skipped_healthy {
                        r.state = runner::State::Healthy;
                    } else if start_res.is_ok() {
                        r.state = runner::State::Starting;
                    } else {
                        // fail-closed (env 注入失败) 或 start.sh 失败 → 标 Failed。
                        // 下一 tick 的 handle_unhealthy/探活会补告警; 此处仅记 ERROR。
                        let detail = start_res
                            .err()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "start failed".into());
                        tracing::error!(svc = %name, %detail, "up_native: 服务未启动 (fail-closed 或 start 失败)");
                        r.state = runner::State::Failed;
                    }
                }
            }
        }
        Ok(())
    }

    // down: business 容器先 → native 逆序 (cluster→...→core) → sidecar 最后。
    // compose down 一次性停 business+sidecar, 故分两步: 先停 business, native 停后再 down 全部。
    pub async fn down_all(&mut self) -> Result<()> {
        // 1. compose 全停 (business + sidecar 一起; 容器无 native 依赖, 先停安全)
        if let Some(cb) = &self.compose {
            tracing::info!("down: compose 平面停 (business + sidecar)");
            cb.down()?;
        }
        // 2. native 逆序停
        self.down_native().await?;
        Ok(())
    }

    // native down: 逆序 (cluster→domain→app→platform→core)。
    async fn down_native(&mut self) -> Result<()> {
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
                // 返回服务名, join 后写 runner.state = Stopped
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    tracing::info!(svc = %d.name, repo = %d.repo_dir, "start.sh stop");
                    if let Err(e) = (start_fn)(&d.repo_dir, "stop", &[]) {
                        tracing::warn!(svc = %d.name, err = %e, "stop fail");
                    }
                    d.name
                }));
            }
            for h in handles {
                if let Ok(name) = h.await
                    && let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name)
                {
                    r.state = runner::State::Stopped;
                }
            }
        }
        Ok(())
    }

    // status: native entries + compose 容器 entries, 合并一张表。
    // 容器 plane="compose", state 来自 docker ps 健康映射。
    pub fn status_all(&self) -> Vec<StatusEntry> {
        let mut out: Vec<StatusEntry> = self
            .runners
            .iter()
            .map(|r| StatusEntry {
                name: r.def.name.clone(),
                state: format!("{:?}", r.state),
                port: r.def.port,
                plane: "native".into(),
            })
            .collect();
        if let Some(cb) = &self.compose {
            match cb.ps() {
                Ok(cs) => {
                    for c in cs {
                        out.push(StatusEntry {
                            name: c.service.clone(),
                            state: c.supervisor_state().to_string(),
                            port: c.port(),
                            plane: "compose".into(),
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(err = %e, "compose ps 失败 (状态表缺容器行)");
                }
            }
        }
        out
    }

    // 单轮监督: native runner tick + compose 容器健康探测。
    // 容器: ps → exited/unhealthy → 标 Failed + 告警 (同 native 宕机路径)。
    // compose restart: unless-stopped 已处理自动重启, supervisor 仅报告状态 + 告警。
    pub async fn tick_all(&mut self, now_ts: u64) {
        for r in &mut self.runners {
            if let Err(e) = r.tick(now_ts, &self.store, &mut self.alerter).await {
                tracing::warn!(svc = %r.def.name, err = %e, "tick fail (继续下一个)");
            }
        }
        self.tick_compose(now_ts).await;
    }

    // compose 容器监督: 探活每个容器, 宕机告警 (经 compose_tracker 节流, issue #6)。
    // tracker 纯逻辑给决策, 此处据决策发告警。容器不被 supervisor 重启 (Docker 管)。
    async fn tick_compose(&mut self, now_ts: u64) {
        let Some(cb) = &self.compose else {
            return;
        };
        let cs = match cb.ps() {
            Ok(cs) => cs,
            Err(e) => {
                tracing::warn!(err = %e, "compose ps 失败 (跳过容器监督本轮)");
                return;
            }
        };
        let live: Vec<String> = cs.iter().map(|c| c.service.clone()).collect();
        for c in &cs {
            let st = c.supervisor_state();
            // 每容器每 tick 写健康行 plane=compose (issue #7: 容器进 history.db)
            if let Err(e) = self
                .store
                .record_health(&c.service, st, 0, now_ts, "compose")
            {
                tracing::warn!(svc = %c.service, err = %e, "compose health write fail (非致命)");
            }
            let decisions = self
                .compose_tracker
                .observe(&c.service, &c.state, st, now_ts);
            for d in decisions {
                let (kind, detail, event_kind) = match d {
                    crate::compose_tracker::ComposeAlert::Down => (
                        crate::alert::AlertKind::ServiceDown,
                        format!(
                            "compose container {} state={} health={}",
                            c.name, c.state, c.health
                        ),
                        "ServiceDown",
                    ),
                    crate::compose_tracker::ComposeAlert::StateChange => (
                        crate::alert::AlertKind::ContainerStateChange,
                        format!(
                            "compose container {} state changed to {} health={}",
                            c.name, c.state, c.health
                        ),
                        "ContainerStateChange",
                    ),
                    crate::compose_tracker::ComposeAlert::CrashLoop => (
                        crate::alert::AlertKind::ContainerCrashLoop,
                        format!(
                            "compose container {} crash-loop (consecutive failures >= threshold)",
                            c.name
                        ),
                        "ContainerCrashLoop",
                    ),
                    crate::compose_tracker::ComposeAlert::Back => (
                        crate::alert::AlertKind::ServiceBack,
                        format!("compose container {} recovered", c.name),
                        "ServiceBack",
                    ),
                };
                tracing::info!(svc = %c.service, kind = kind.label(), "compose tick 告警决策");
                // 持久化事件行 plane=compose (issue #7)
                if let Err(e) = self
                    .store
                    .record_event(&c.service, event_kind, &detail, now_ts, "compose")
                {
                    tracing::warn!(svc = %c.service, err = %e, "compose event write fail (非致命)");
                }
                let _ = self
                    .alerter
                    .alert(crate::alert::Alert {
                        service: format!("compose:{}", c.service),
                        kind,
                        detail,
                        ts: now_ts,
                    })
                    .await;
            }
        }
        // 清除已消失容器快照
        self.compose_tracker.reap(&live);
    }

    // 探活间隔: 有 down 态服务 (native 或 compose 容器) 用 poll_unhealthy_sec, 否则 poll_healthy_sec
    pub fn poll_interval(&self) -> u64 {
        let any_down = self.runners.iter().any(|r| {
            matches!(
                r.state,
                runner::State::Unhealthy | runner::State::Restarting | runner::State::Failed
            )
        });
        if any_down {
            return self.cfg.poll_unhealthy_sec;
        }
        // compose 容器有 Failed 也算 down
        if let Some(cb) = &self.compose
            && let Ok(cs) = cb.ps()
            && cs.iter().any(|c| c.supervisor_state() == "Failed")
        {
            return self.cfg.poll_unhealthy_sec;
        }
        self.cfg.poll_healthy_sec
    }

    // logs 分派: compose 容器服务名 → compose logs; 否则 None (native 走 cli_logs)
    pub fn compose_logs(&self, service: &str) -> Option<Result<String>> {
        let cb = self.compose.as_ref()?;
        // 仅当该服务在 compose 容器列表中才走 compose logs
        match cb.ps() {
            Ok(cs) if cs.iter().any(|c| c.service == service) => Some(cb.logs(service)),
            _ => None,
        }
    }

    // compose 是否启用 (测试 + cli 用)
    pub fn compose_enabled(&self) -> bool {
        self.compose.is_some()
    }
}

// 据配置构建 compose 后端。enabled=false → None (仅 native)。
// 文件不存在时告警但返回 None (不阻塞 native 启动, 操作员按提示补文件)。
fn build_compose(cfg: &Config) -> Option<Box<dyn ComposeBackend>> {
    if !cfg.compose.enabled {
        return None;
    }
    if !cfg.compose.sidecar_file.exists() {
        tracing::warn!(
            path = %cfg.compose.sidecar_file.display(),
            "compose.enabled=true 但 sidecar 文件不存在, 回退仅 native"
        );
        return None;
    }
    Some(Box::new(DockerCompose::new(
        cfg.compose.sidecar_file.clone(),
        cfg.compose.business_file.clone(),
        cfg.compose.env_file.clone(),
    )))
}

// 真 start.sh 调用: bash {repo}/start.sh {cmd}, stderr → 每服务日志。
// env_overrides: 注入连接 env (issue #8, compose enabled 时依赖 sidecar 的服务)。
fn real_start_pair(repo_dir: &str, cmd: &str, env_overrides: &[(String, String)]) -> Result<()> {
    let sh = probe_map::start_sh_path(repo_dir);
    if !sh.exists() {
        anyhow::bail!("start.sh 不存在: {}", sh.display());
    }
    let log_dir = std::env::var("HOME")
        .map(|h| format!("{h}/.fusion-sv/logs"))
        .unwrap_or_else(|_| "/tmp/fusion-sv-logs".into());
    std::fs::create_dir_all(&log_dir).ok();
    let svc_log = format!("{log_dir}/{repo_dir}.log");
    tracing::info!(sh = %sh.display(), cmd, log = %svc_log, env_n = env_overrides.len(), "exec start.sh");
    let stderr: std::process::Stdio = match std::fs::File::create(&svc_log) {
        Ok(f) => f.into(),
        Err(e) => {
            tracing::warn!(err = %e, log = %svc_log, "open svc log fail, stderr → null");
            std::process::Stdio::null()
        }
    };
    let mut command = std::process::Command::new("bash");
    command.arg(&sh).arg(cmd);
    // 注入连接 env (覆盖/新增), 继承父进程其余 env
    for (k, v) in env_overrides {
        command.env(k, v);
    }
    let status = command
        .stdout(std::process::Stdio::null())
        .stderr(stderr)
        .status()
        .map_err(|e| anyhow::anyhow!("spawn start.sh: {e}"))?;
    if !status.success() {
        anyhow::bail!("start.sh {cmd} exit {:?}", status.code());
    }
    Ok(())
}

// 同步壳: tokio runtime 内 block_on probe。ServiceRunner.probe_fn 是同步闭包。
fn real_probe_spec(s: &ProbeSpec) -> ProbeResult {
    let s = s.clone();
    tokio::task::block_in_place(|| {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(probe(&s))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Layer, ServiceDef};
    use crate::probe::ProbeResult;
    use std::sync::{Arc, Mutex, OnceLock};

    // 进程 env 全局共享, env 注入测试需串行避免互相污染。
    fn env_lock() -> &'static Mutex<()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    fn def(name: &str, layer: Layer, port: u16) -> ServiceDef {
        ServiceDef {
            name: name.into(),
            port,
            repo_dir: name.into(),
            purpose: "x".into(),
            fixed: false,
            layer,
        }
    }

    fn fake_cfg() -> Config {
        Config {
            max_concurrent_start: 2,
            start_timeout_sec: 5,
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn test_up_all_skips_already_healthy() {
        // 所有服务探活即健康 → 不调 start.sh
        let defs = vec![
            def("fusion-mlx", Layer::Core, 11434),
            def("fusion-rag", Layer::App, 11436),
        ];
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_p = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 }, // 探活恒健康
            move |svc, cmd, _env| {
                calls_p.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        core.up_all().await.unwrap();
        assert!(
            calls.lock().unwrap().is_empty(),
            "已健康应跳过不调 start.sh"
        );
        // I4: 跳过=已健康 的 runner 应标 Healthy
        for r in &core.runners {
            assert!(
                matches!(r.state, runner::State::Healthy),
                "skip 后应 Healthy"
            );
        }
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
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |svc, _cmd, _env| {
                order_p.lock().unwrap().push(svc.to_string());
                Ok(())
            },
        );
        core.up_all().await.unwrap();
        let o = order.lock().unwrap();
        assert_eq!(o[0], "fusion-mlx", "core 层先起");
        assert_eq!(o[1], "fusion-rag", "app 层后起");
        // I4: 启动过的 runner 应标 Starting
        for r in &core.runners {
            assert!(
                matches!(r.state, runner::State::Starting),
                "启动后应 Starting"
            );
        }
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
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            move |svc, _cmd, _env| {
                order_p.lock().unwrap().push(svc.to_string());
                Ok(())
            },
        );
        core.down_all().await.unwrap();
        let o = order.lock().unwrap();
        assert_eq!(o[0], "fusion-rag", "逆序: app 先停");
        assert_eq!(o[1], "fusion-mlx", "core 最后停");
        // I5: 停止过的 runner 应标 Stopped
        for r in &core.runners {
            assert!(
                matches!(r.state, runner::State::Stopped),
                "停止后应 Stopped"
            );
        }
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
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |_svc, _cmd, _env| {
                let mut cur = inflight_p.lock().unwrap();
                *cur += 1;
                let mut mx = max_p.lock().unwrap();
                if *cur > *mx {
                    *mx = *cur;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                *cur -= 1;
                Ok(())
            },
        );
        core.up_all().await.unwrap();
        let mx = *max_seen.lock().unwrap();
        assert!(mx <= 2, "并发上限 2, 实测峰值 {mx}");
    }

    // C1: tick_all 驱动状态机 — Unhealthy+ConnectionRefused → 记 crash + 重启 →
    // 翻探活为 Healthy → tick → 恢复 Healthy。证明 tick_all 接通监督回路。
    #[tokio::test]
    async fn test_tick_all_drives_recovery() {
        let probe_cell: Arc<Mutex<ProbeResult>> = Arc::new(Mutex::new(ProbeResult::Unhealthy {
            reason: crate::probe::UnhealthyReason::ConnectionRefused,
        }));
        let pc = probe_cell.clone();
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-rag", Layer::App, 11436)],
            fake_cfg(),
            move |_: &ProbeSpec| pc.lock().unwrap().clone(),
            |_svc, _cmd, _env| Ok(()),
        );
        // 起始: 已 Unhealthy (首 tick 进重启流 → Restarting)
        core.runners[0].state = runner::State::Unhealthy;
        core.tick_all(1000).await;
        assert!(
            matches!(core.runners[0].state, runner::State::Restarting),
            "tick_all 后应 Restarting"
        );
        // 翻探活为健康 → tick_all 应恢复 Healthy
        *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 3 };
        core.tick_all(1001).await;
        assert!(
            matches!(core.runners[0].state, runner::State::Healthy),
            "探活恢复后 tick_all 应 Healthy"
        );
    }

    // C1: poll_interval — 有 down 态返回 poll_unhealthy_sec, 否则 poll_healthy_sec
    #[test]
    fn test_poll_interval_switches_on_down_state() {
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-rag", Layer::App, 11436)],
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        // 全 Healthy → poll_healthy_sec
        core.runners[0].state = runner::State::Healthy;
        assert_eq!(core.poll_interval(), core.cfg.poll_healthy_sec);
        // 有 Unhealthy → poll_unhealthy_sec
        core.runners[0].state = runner::State::Unhealthy;
        assert_eq!(core.poll_interval(), core.cfg.poll_unhealthy_sec);
        // Failed 也算 down
        core.runners[0].state = runner::State::Failed;
        assert_eq!(core.poll_interval(), core.cfg.poll_unhealthy_sec);
    }

    // ---- compose 平面集成测试 ----
    use crate::compose::{ContainerStatus, test_support::FakeCompose};

    fn mk_container(service: &str, state: &str, health: &str, port: u16) -> ContainerStatus {
        ContainerStatus {
            name: format!("sv-{service}-1"),
            service: service.into(),
            state: state.into(),
            health: health.into(),
            publishers: if port > 0 {
                vec![format!("0.0.0.0:{port}->{port}/tcp")]
            } else {
                vec![]
            },
        }
    }

    // up_all: compose 启用时顺序 = sidecar → native → business
    #[tokio::test]
    async fn test_up_all_compose_order_sidecar_native_business() {
        let defs = vec![
            def("fusion-mlx", Layer::Core, 11434),
            def("fusion-rag", Layer::Platform, 11436),
        ];
        let native_order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let no_p = native_order.clone();
        let fake = Box::new(FakeCompose::new());
        let calls_ref = fake.calls.clone();
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |svc, _cmd, _env| {
                no_p.lock().unwrap().push(svc.to_string());
                Ok(())
            },
            Some(fake),
        );
        core.up_all().await.unwrap();
        let calls = calls_ref.lock().unwrap();
        // 顺序: up_sidecar 先, up_business 最后
        assert_eq!(calls[0], "up_sidecar", "sidecar 先起");
        assert_eq!(calls[calls.len() - 1], "up_business", "business 最后起");
        // native 在中间: core 先于 platform
        let no = native_order.lock().unwrap();
        let mlx_i = no.iter().position(|n| n == "fusion-mlx").unwrap();
        let rag_i = no.iter().position(|n| n == "fusion-rag").unwrap();
        assert!(mlx_i < rag_i, "native core 先于 platform");
        // sidecar 在 native 之前
        let sidecar_i = calls.iter().position(|c| c == "up_sidecar").unwrap();
        let business_i = calls.iter().position(|c| c == "up_business").unwrap();
        assert!(sidecar_i < business_i);
    }

    // compose 未启用 (None): up_all 不调任何 compose 方法, 仅 native
    #[tokio::test]
    async fn test_up_all_no_compose_native_only() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let called: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cp = called.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |svc, cmd, _env| {
                cp.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        assert!(!core.compose_enabled(), "默认 compose 未启用");
        core.up_all().await.unwrap();
        let c = called.lock().unwrap();
        assert!(
            c.iter().all(|x| x.contains("start")),
            "仅 native start 调用"
        );
    }

    // down_all: compose 启用时 compose down 先, native 逆序后
    #[tokio::test]
    async fn test_down_all_compose_then_native_reverse() {
        let defs = vec![
            def("fusion-mlx", Layer::Core, 11434),
            def("fusion-rag", Layer::Platform, 11436),
        ];
        let native_order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let no_p = native_order.clone();
        let fake = Box::new(FakeCompose::new());
        let calls_ref = fake.calls.clone();
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            move |svc, _cmd, _env| {
                no_p.lock().unwrap().push(svc.to_string());
                Ok(())
            },
            Some(fake),
        );
        core.down_all().await.unwrap();
        let calls = calls_ref.lock().unwrap();
        assert_eq!(calls[0], "down", "compose down 先");
        // native 逆序: platform 先停, core 后停
        let no = native_order.lock().unwrap();
        let rag_i = no.iter().position(|n| n == "fusion-rag").unwrap();
        let mlx_i = no.iter().position(|n| n == "fusion-mlx").unwrap();
        assert!(rag_i < mlx_i, "native 逆序: platform 先停, core 后停");
    }

    // status_all: compose 启用时合并 native + 容器行
    #[test]
    fn test_status_all_merges_compose_rows() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let fake = Box::new(FakeCompose::new());
        fake.set_containers(vec![
            mk_container("postgres", "running", "healthy", 5432),
            mk_container("qdrant", "exited", "", 0),
        ]);
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
            Some(fake),
        );
        core.runners[0].state = runner::State::Healthy;
        let st = core.status_all();
        // 1 native + 2 compose
        assert_eq!(st.len(), 3);
        let native = st.iter().find(|e| e.plane == "native").unwrap();
        assert_eq!(native.name, "fusion-mlx");
        let pg = st.iter().find(|e| e.name == "postgres").unwrap();
        assert_eq!(pg.plane, "compose");
        assert_eq!(pg.state, "Healthy");
        assert_eq!(pg.port, 5432);
        let qd = st.iter().find(|e| e.name == "qdrant").unwrap();
        assert_eq!(qd.state, "Failed");
    }

    // tick_all: compose 容器 exited → ServiceDown 告警
    #[tokio::test]
    async fn test_tick_all_compose_exited_alerts() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let fake = Box::new(FakeCompose::new());
        fake.set_containers(vec![
            mk_container("postgres", "running", "healthy", 5432),
            mk_container("redis", "exited", "", 6379),
        ]);
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
            Some(fake),
        );
        core.runners[0].state = runner::State::Healthy;
        core.tick_all(1000).await;
        // 告警经由 tracing WARN + 可选 webhook; 此处验证不 panic + 状态表见 Failed
        let st = core.status_all();
        let redis = st.iter().find(|e| e.name == "redis").unwrap();
        assert_eq!(redis.state, "Failed", "exited 容器应标 Failed");
    }

    // poll_interval: compose 容器 Failed → poll_unhealthy_sec
    #[test]
    fn test_poll_interval_compose_failed() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let fake = Box::new(FakeCompose::new());
        fake.set_containers(vec![mk_container("postgres", "exited", "", 5432)]);
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
            Some(fake),
        );
        core.runners[0].state = runner::State::Healthy;
        // native 全 Healthy 但 compose 容器 Failed → poll_unhealthy_sec
        assert_eq!(core.poll_interval(), core.cfg.poll_unhealthy_sec);
    }

    // compose_logs: compose 服务走 compose logs, native 服务返回 None
    #[test]
    fn test_compose_logs_dispatch() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let fake = Box::new(FakeCompose::new());
        fake.set_containers(vec![mk_container("postgres", "running", "healthy", 5432)]);
        let core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
            Some(fake),
        );
        // compose 服务 → Some
        assert!(core.compose_logs("postgres").is_some());
        // native 服务 → None (走 native 日志路径)
        assert!(core.compose_logs("fusion-mlx").is_none());
        // compose 未启用 → None
        let core2 = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        assert!(core2.compose_logs("postgres").is_none());
    }

    // ---- compose 容器告警节流 (issue #6) 集成测试 ----
    // 4 场景经 tick_compose 驱动 compose_tracker, 验证快照/决策不 panic 且状态正确。

    // 同容器 exited 连续 5 tick (去抖窗内) → fail_count 累计, 仅首 tick 记 last_alert
    #[tokio::test]
    async fn test_tick_compose_dedup_repeated_failed() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let fake = Box::new(FakeCompose::new());
        fake.set_containers(vec![mk_container("redis", "exited", "", 6379)]);
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
            Some(fake),
        );
        core.runners[0].state = runner::State::Healthy;
        // 5 tick, 去抖窗内 (默认 alert_dedup_sec=300, escalation=5)
        for ts in [1000u64, 1002, 1004, 1006, 1008] {
            core.tick_all(ts).await;
        }
        let rec = core.compose_tracker.record("redis").unwrap();
        assert_eq!(rec.fail_count, 5, "连续 5 tick 失败应累计 5");
        assert_eq!(rec.state, "Failed");
    }

    // docker state 变化 (restarting→exited) → tracker 记录 raw_state 变化
    #[tokio::test]
    async fn test_tick_compose_state_change_re_alerts() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let fake = Box::new(FakeCompose::new());
        fake.set_containers(vec![mk_container("redis", "restarting", "", 6379)]);
        let containers = fake.containers.clone();
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
            Some(fake),
        );
        core.runners[0].state = runner::State::Healthy;
        core.tick_all(1000).await;
        assert_eq!(
            core.compose_tracker.record("redis").unwrap().raw_state,
            "restarting"
        );
        // 切到 exited (经 Arc<Mutex> 内部可变, 无需借 core.compose)
        *containers.lock().unwrap() = vec![mk_container("redis", "exited", "", 6379)];
        core.tick_all(1002).await;
        assert_eq!(
            core.compose_tracker.record("redis").unwrap().raw_state,
            "exited",
            "state 变化应更新快照"
        );
    }

    // 失败→健康 → tracker 重置 fail_count, escalated 清零
    #[tokio::test]
    async fn test_tick_compose_service_back() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let fake = Box::new(FakeCompose::new());
        fake.set_containers(vec![mk_container("redis", "exited", "", 6379)]);
        let containers = fake.containers.clone();
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
            Some(fake),
        );
        core.runners[0].state = runner::State::Healthy;
        core.tick_all(1000).await;
        assert_eq!(
            core.compose_tracker.record("redis").unwrap().state,
            "Failed"
        );
        // 恢复 (经 Arc<Mutex> 内部可变)
        *containers.lock().unwrap() = vec![mk_container("redis", "running", "healthy", 6379)];
        core.tick_all(1010).await;
        let rec = core.compose_tracker.record("redis").unwrap();
        assert_eq!(rec.state, "Healthy", "恢复应转 Healthy");
        assert_eq!(rec.fail_count, 0, "恢复应重置 fail_count");
    }

    // 连续失败达阈值 → escalated=true (CrashLoop 升级)
    #[tokio::test]
    async fn test_tick_compose_escalation() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let fake = Box::new(FakeCompose::new());
        fake.set_containers(vec![mk_container("redis", "exited", "", 6379)]);
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
            Some(fake),
        );
        core.runners[0].state = runner::State::Healthy;
        // escalation=5 (fake_cfg 默认), 跑 5 tick
        for ts in [1000u64, 1002, 1004, 1006, 1008] {
            core.tick_all(ts).await;
        }
        let rec = core.compose_tracker.record("redis").unwrap();
        assert!(rec.escalated, "达阈值应升级标记");
        assert_eq!(rec.fail_count, 5);
    }

    // ---- compose 容器持久化进 history.db (issue #7) ----

    // 容器 exited → tick_compose 应写 event 行 plane=compose, kind=ServiceDown
    #[tokio::test]
    async fn test_tick_compose_persists_down_event() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let fake = Box::new(FakeCompose::new());
        fake.set_containers(vec![mk_container("redis", "exited", "", 6379)]);
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
            Some(fake),
        );
        core.runners[0].state = runner::State::Healthy;
        core.tick_all(1000).await;
        let ev = core.store.recent_events("redis", 10).unwrap();
        assert!(!ev.is_empty(), "exited 容器应写 event 行");
        assert_eq!(ev[0].kind, "ServiceDown");
        assert_eq!(ev[0].plane, "compose");
        // 健康行也应存在 plane=compose
        let h = core.store.recent_health("redis", 10).unwrap();
        assert!(!h.is_empty(), "应写 compose 健康行");
        assert_eq!(h[0].plane, "compose");
    }

    // 容器 exited→running → tick_compose 应写 Back 事件 plane=compose
    #[tokio::test]
    async fn test_tick_compose_persists_back_event() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let fake = Box::new(FakeCompose::new());
        fake.set_containers(vec![mk_container("redis", "exited", "", 6379)]);
        let containers = fake.containers.clone();
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
            Some(fake),
        );
        core.runners[0].state = runner::State::Healthy;
        core.tick_all(1000).await;
        // 恢复
        *containers.lock().unwrap() = vec![mk_container("redis", "running", "healthy", 6379)];
        core.tick_all(1010).await;
        let ev = core.store.recent_events("redis", 10).unwrap();
        let has_back = ev
            .iter()
            .any(|e| e.kind == "ServiceBack" && e.plane == "compose");
        assert!(has_back, "恢复应写 ServiceBack 事件 plane=compose");
    }

    // issue #8: compose disabled (单节点开发) → 依赖 sidecar 的服务照常启动, 无 env 注入。
    // start_fn 收到空 env_overrides (第三参为空切片)。
    // env_lock 跨 await: 测试体短, 仅串行化进程 env 读写, 不致 executor 饥饿。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_up_native_no_env_injection_when_compose_disabled() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let defs = vec![def("fusion-identity", Layer::App, 11450)];
        let received_env: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let re_p = received_env.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(), // compose.enabled=false (默认)
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |_svc, _cmd, env: &[(String, String)]| {
                re_p.lock().unwrap().extend(env.iter().cloned());
                Ok(())
            },
        );
        assert!(!core.compose_enabled(), "compose 应未启用");
        core.up_all().await.unwrap();
        assert!(
            received_env.lock().unwrap().is_empty(),
            "compose disabled 时不应注入连接 env"
        );
        assert!(
            matches!(core.runners[0].state, runner::State::Starting),
            "无注入时服务应正常 Starting"
        );
    }

    // issue #8: compose enabled + 依赖 sidecar 的服务 + .env 缺 FUSION_PG_PASSWORD
    // → fail-closed: 拒绝启动, 不调 start.sh, runner 标 Failed。
    // env_lock 跨 await: 测试体短, 仅串行化进程 env 读写, 不致 executor 饥饿。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_up_native_fail_closed_when_pg_password_missing() {
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        // .env 只有 USER 无 PASSWORD → DATABASE_URL 构造不出
        let env_f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(env_f.path(), "FUSION_PG_USER=fusion\n").unwrap();
        let mut cfg = fake_cfg();
        cfg.compose.enabled = true;
        cfg.compose.env_file = env_f.path().to_path_buf();
        let defs = vec![def("fusion-identity", Layer::App, 11450)];
        let started: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let st_p = started.clone();
        let fake = Box::new(FakeCompose::new());
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            cfg,
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |_svc, _cmd, _env| {
                *st_p.lock().unwrap() = true;
                Ok(())
            },
            Some(fake),
        );
        assert!(core.compose_enabled(), "compose 应启用");
        core.up_all().await.unwrap();
        assert!(!*started.lock().unwrap(), "fail-closed 应拒绝调用 start.sh");
        assert!(
            matches!(core.runners[0].state, runner::State::Failed),
            "env 注入失败应标 Failed (非静默降级)"
        );
    }
}
