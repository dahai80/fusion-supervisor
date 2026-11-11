pub mod policy;
pub mod runner;

use crate::alert::Alerter;
use crate::compose::{ComposeBackend, DockerCompose};
use crate::config::Config;
use crate::dep_map;
use crate::manifest::{BootTier, Layer, ServiceDef, ServiceManifest};
use crate::probe::{ProbeResult, ProbeSpec, probe};
use crate::probe_map;
use crate::store::{HistoryStore, PinMode};
use anyhow::Result;
// issue #60: CommandExt::process_group — 服务子进程独立进程组, 隔离 daemon 重载的组信号
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use tokio::sync::Semaphore;

// 签名: (repo_dir, cmd, env_overrides)。env_overrides 为注入连接 env (issue #8)。
pub type ArcStartFn = Arc<dyn Fn(&str, &str, &[(String, String)]) -> Result<()> + Send + Sync>;
pub type ArcProbeFn = Arc<dyn Fn(&ProbeSpec) -> ProbeResult + Send + Sync>;

// issue #122: 可移交的 exec 作业 — prepare (锁内) 产出, 调用方在核心锁外 run(),
// 再拿锁 finish。exec (start.sh / go build 等) 可达数分钟, 绝不能持锁等待。
pub struct StartJob {
    pub service: String,
    pub verb: &'static str,
    pub env: Vec<(String, String)>,
    pub timeout: u64,
    pub start_fn: ArcStartFn,
}

impl StartJob {
    // 锁外执行 — 与旧内联 exec 同语义 (同步闭包 + 超时包裹)。
    pub async fn run(&self) -> Result<()> {
        let start_fn = self.start_fn.clone();
        let svc = self.service.clone();
        let verb = self.verb;
        let env = self.env.clone();
        let timeout = self.timeout;
        let cmd = async move { start_fn(&svc, verb, &env) };
        match tokio::time::timeout(std::time::Duration::from_secs(timeout), cmd).await {
            Ok(r) => r,
            Err(_) => {
                tracing::warn!(svc = %self.service, verb, "exec timeout {timeout}s");
                anyhow::bail!("{verb} timeout {timeout}s")
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StatusEntry {
    pub name: String,
    pub state: String, // State 的字符串形式, 兼容容器平面 (Failed/Healthy/...)
    pub port: u16,
    pub plane: String, // "native" | "compose"
    // issue #112: 最近一次 restart 类事件 ("kind detail"), 审计 "谁拉起的"。
    // None = 无记录 (从没重启过/手工事件缺), 序列化时省略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_restart: Option<String>,
}

// PRD app-tiering: /api/v1/boot 行。pinned=None 表示无个人覆盖 (走注册表 boot 字段)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BootEntry {
    pub name: String,
    pub tier: &'static str,
    pub pinned: Option<String>,
    pub state: String,
    pub port: u16,
}

// issue #90: 当前 unix 秒 — up 路径没有 tick 的注入时钟, 重试排期只需
// 真实时刻 (测试对状态机断言走 tick 的 now_ts 注入, 不受影响)。
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct SupervisorCore {
    pub runners: Vec<runner::ServiceRunner>,
    // issue #37: Arc 化 — Alerter 的事件落库 sink 需要持有 store 的克隆
    // (告警 chokepoint 统一写 event 表, native + compose 共用)。
    pub store: Arc<HistoryStore>,
    pub alerter: Alerter,
    pub cfg: Config,
    // 注入的 start 函数 (生产=bash start.sh, 测试=fake)。签名: (repo_dir, cmd) -> Result
    start_fn: ArcStartFn,
    probe_fn: ArcProbeFn,
    // compose 平面 (可选)。enabled=false 时为 None, 保留仅 native 行为。
    compose: Option<Box<dyn ComposeBackend>>,
    // compose 容器告警节流 (issue #6): 防 flapping 容器每 tick 刷告警。
    compose_tracker: crate::compose_tracker::ComposeTracker,
    // issue #10: 上次备份时间戳 (调度判定用)。0=从未备份 → 首次 tick 即到期。
    // pub: #113 detached 语义的集成测试直接断言 (tests/test_backup_detach.rs)。
    pub last_backup_ts: u64,
    // issue #103: 备份目标从未配置的一次性 WARN 锁存 (调度路径静默, 不逐 tick 刷)。
    backup_disabled_notified: bool,
    // issue #37: 上次 retention sweep 时间戳 (调度判定用, 语义同 last_backup_ts)。
    last_prune_ts: u64,
    // issue #37: 累计清除的 history.db 行数 (/metrics fusion_sv_events_pruned_total)
    pub events_pruned_total: u64,
    // PRD app-tiering: pin 覆盖内存缓存 (boot_auto 零查询); 真源在 store.boot_pin 表
    boot_pins: std::collections::HashMap<String, PinMode>,
}

impl SupervisorCore {
    // 生产构造: 真 bash start.sh + 真 probe。probe_map 填 spec + restart_cmd。
    pub fn new(defs: Vec<ServiceDef>, cfg: Config) -> Result<Self> {
        let store = Arc::new(HistoryStore::open(&cfg.db_path)?);
        let alerter = Alerter::new(cfg.alert_dedup_sec, cfg.alert_url.clone())
            .with_event_sink(make_event_sink(store.clone()));
        let env_file = cfg.compose.env_file.clone();
        let compose_on = cfg.compose.enabled;
        // issue #96: 端口快照供 daemon_stop_cmd 孤儿兜底查询
        crate::manifest::register_ports(&defs);
        let runners: Vec<_> = defs
            .into_iter()
            .filter(|d| d.managed)
            .map(|d| {
                let spec = probe_map::probe_spec_for(&d.name, d.port);
                let cmd = probe_map::restart_cmd_for(&d.name).to_string();
                let name = d.name.clone();
                let env_file = env_file.clone();
                let policy = policy::RestartPolicy::new(
                    cfg.restart_window_sec,
                    cfg.restart_max,
                    cfg.backoff_max_sec,
                );
                runner::ServiceRunner::new(
                    d,
                    &cmd,
                    move || real_probe_spec(&spec),
                    move |c: &str| {
                        // issue #29: crash 自动重启路径也注入 shell 注册 env。
                        // issue #78: 同时注入 sidecar 连接 env (不 gate compose 开关,
                        // 与 up_native/up_one 一致 — 否则崩溃重启的服务又静默
                        // 落回 SQLite)。解析失败仅 ERROR + 已解析部分继续 (不 bail):
                        // 服务真起不来时探活失败告警兜底; 首次启动路径 up_native
                        // 才是严格 fail-closed。
                        let mut env: Vec<(String, String)> =
                            match crate::env_map::shell_env_for(&name, &env_file) {
                                Ok(m) => m.into_iter().collect(),
                                Err(e) => {
                                    tracing::error!(svc = %name, err = %e, "shell env 解析失败, 重启以空 env 继续");
                                    Vec::new()
                                }
                            };
                        match crate::env_map::native_env_for(&name, &env_file, compose_on) {
                            Ok(m) => env.extend(m),
                            Err(e) => {
                                tracing::error!(svc = %name, err = %e, "sidecar env 解析失败, 重启以剩余 env 继续");
                            }
                        }
                        real_start_pair(&name, c, &env)
                    },
                    policy,
                )
            })
            .collect();
        // issue #126: 仅生产构造器注入启动宽限窗口; 测试构造器保持 0 (行为不变)
        let grace = cfg.startup_grace_secs;
        let runners: Vec<_> = runners
            .into_iter()
            .map(|mut r| {
                r.startup_grace_secs = grace;
                r
            })
            .collect();
        let boot_pins = load_boot_pins(&store);
        let compose = build_compose(&cfg);
        let compose_tracker = crate::compose_tracker::ComposeTracker::new(
            cfg.alert_dedup_sec,
            cfg.container_crash_escalation,
        );
        // issue #113: 恢复上次备份时间戳 — reload 不再重触发全量备份
        let last_backup_ts = store.get_meta("last_backup_ts").unwrap_or(0);
        Ok(Self {
            runners,
            store,
            alerter,
            cfg,
            start_fn: Arc::new(real_start_pair),
            probe_fn: Arc::new(real_probe_spec),
            compose,
            compose_tracker,
            // issue #113: 恢复上次备份时间戳 — reload 不再重触发全量备份
            last_backup_ts,
            backup_disabled_notified: false,
            last_prune_ts: 0,
            events_pruned_total: 0,
            boot_pins,
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
        let store = Arc::new(HistoryStore::open(&db_path).unwrap());
        let boot_pins = load_boot_pins(&store);
        // issue #37: 测试构造同样注入事件 sink — native 告警落库行为与生产一致 (可测)
        let alerter = Alerter::new(cfg.alert_dedup_sec, cfg.alert_url.clone())
            .with_event_sink(make_event_sink(store.clone()));
        let start_fn = Arc::new(start_fn);
        let probe_fn = Arc::new(probe_fn);
        let runners: Vec<_> = defs
            .into_iter()
            .filter(|d| d.managed)
            .map(|d| {
                let pf = probe_fn.clone();
                // issue #96: runner 内部 start 闭包接注入 start_fn (verb 转发) —
                // invoke_start 在测试中可观测 (此前 no-op, restart/rollout 的
                // start 半段测试不可见)
                let sf = start_fn.clone();
                let name = d.name.clone();
                runner::ServiceRunner::new(
                    d,
                    "restart",
                    move || pf(&ProbeSpec::tcp(0)),
                    move |verb: &str| sf(&name, verb, &[]),
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
        // issue #113: 恢复上次备份时间戳 — reload 不再重触发全量备份
        let last_backup_ts = store.get_meta("last_backup_ts").unwrap_or(0);
        Self {
            runners,
            store,
            alerter,
            cfg,
            start_fn,
            probe_fn,
            compose,
            compose_tracker,
            // issue #113: 恢复上次备份时间戳 — reload 不再重触发全量备份
            last_backup_ts,
            backup_disabled_notified: false,
            last_prune_ts: 0,
            events_pruned_total: 0,
            boot_pins,
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
        self.up_native_where(&|_| true).await
    }

    // PRD app-tiering: 带 boot 集过滤的 native up — keep=true 的服务参与启动, 其余跳过。
    // up_all 走全真谓词; up_boot_set / boot_group 走成员谓词。复用既有层序+并发+探活逻辑。
    async fn up_native_where(
        &mut self,
        keep: &(dyn Fn(&ServiceDef) -> bool + Send + Sync),
    ) -> Result<()> {
        let sorted = ServiceManifest::sort_by_layer(
            self.runners
                .iter()
                .map(|r| r.def.clone())
                .filter(|d| keep(d))
                .collect(),
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
                // 依赖门: compose 启用时, 该 native 服务依赖的 sidecar 容器须 Healthy 才起。
                // 不靠 layer 隐式顺序; 显式校验, gate 未就绪跳过启动 (不标 Failed, 依赖问题非服务崩溃)。
                if !self.dep_gate_ok(&d.name).await {
                    if let Some(r) = self.runners.iter_mut().find(|r| r.def.name == d.name) {
                        // issue #90: 清残留重试排期 (避免旧 restart_after 触发陈旧重启)
                        r.mark_stopped();
                    }
                    continue;
                }
                let sem = sem.clone();
                let start_fn = self.start_fn.clone();
                let probe_fn = self.probe_fn.clone();
                let timeout = self.cfg.start_timeout_sec;
                let env_file = self.cfg.compose.env_file.clone();
                let compose_on = self.compose_enabled();
                // 返回 (服务名, 是否跳过=已健康, 启动结果, 是否可重试)
                // 可重试 = 真执行失败/超时 (依赖可能后到); env fail-closed 属配置
                // 问题, 重试无救, 仍标终态 Failed (issue #90)
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    // 先探活: 已健康跳过 (R11: 用注入 probe_fn, 非真 probe)
                    let spec = ProbeSpec::tcp(d.port);
                    if matches!((probe_fn)(&spec), ProbeResult::Healthy { .. }) {
                        tracing::info!(svc = %d.name, "skip start (already up)");
                        return (d.name, true, Ok(()), false);
                    }
                    // env 注入两类, 均解析失败 fail-closed 拒绝启动 (均不 gate 在
                    // compose 开关上):
                    // - issue #29: shell 注册 env
                    // - issue #8/#78: sidecar 连接 env — 此前 gate 在 compose.enabled
                    //   上, 默认 off 时 cowork 静默跑 SQLite / rag 静默跑 :memory:
                    let mut env_overrides: Vec<(String, String)> = Vec::new();
                    if crate::env_map::needs_shell_env(&d.name) {
                        match crate::env_map::shell_env_for(&d.name, &env_file) {
                            Ok(m) => env_overrides.extend(m),
                            Err(e) => {
                                tracing::error!(svc = %d.name, err = %e, "shell env 解析失败, fail-closed 拒绝启动");
                                return (d.name, false, Err(e), false);
                            }
                        }
                    }
                    if crate::env_map::needs_injection(&d.name) {
                        match crate::env_map::native_env_for(&d.name, &env_file, compose_on) {
                            Ok(m) => env_overrides.extend(m),
                            Err(e) => {
                                tracing::error!(svc = %d.name, err = %e, "env 注入解析失败, fail-closed 拒绝启动");
                                return (d.name, false, Err(e), false);
                            }
                        }
                    }
                    tracing::info!(svc = %d.name, repo = %d.repo_dir, env_n = env_overrides.len(), "start.sh start");
                    let cmd = async { (start_fn)(&d.name, "start", &env_overrides) };
                    match tokio::time::timeout(std::time::Duration::from_secs(timeout), cmd).await {
                        Ok(Ok(())) => (d.name, false, Ok(()), false),
                        Ok(Err(e)) => {
                            tracing::warn!(svc = %d.name, err = %e, "start fail");
                            (d.name, false, Err(e), true)
                        }
                        Err(_) => {
                            let e = anyhow::anyhow!("start timeout {timeout}s");
                            tracing::warn!(svc = %d.name, "start timeout {timeout}s");
                            (d.name, false, Err(e), true)
                        }
                    }
                }));
            }
            for h in handles {
                if let Ok((name, skipped_healthy, start_res, retryable)) = h.await
                    && let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name)
                {
                    if skipped_healthy {
                        r.state = runner::State::Healthy;
                    } else if start_res.is_ok() {
                        r.state = runner::State::Starting;
                        // issue #126: 拉起成功记启动宽限截止
                        r.set_boot_grace(now_unix());
                    } else {
                        let detail = start_res
                            .err()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "start failed".into());
                        if retryable {
                            // issue #90: 启动失败进 tick 退避重试流 (依赖未就绪常见),
                            // 不再直接标终态 Failed — 此前开机 PG 未起时 gateway/
                            // identity 死等人工。exit-78 配置致命仍在 mark_start_failed
                            // 内部标 Failed (重试无救)。
                            tracing::error!(svc = %name, %detail, "up_native: 启动失败, 排期重试");
                            let now_ts = now_unix();
                            let retry = r.mark_start_failed(&detail, now_ts);
                            if retry {
                                tracing::info!(svc = %name, "已排期启动失败重试");
                            }
                        } else {
                            // env fail-closed = 配置问题, 重试不会自愈, 保持终态 Failed
                            tracing::error!(svc = %name, %detail, "up_native: env 解析失败, 标 Failed (配置问题)");
                            r.state = runner::State::Failed;
                        }
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
        self.down_native_where(&|_| true).await
    }

    // PRD app-tiering: 带 boot 集过滤的 native down (分组批量停用)。
    async fn down_native_where(
        &mut self,
        keep: &(dyn Fn(&ServiceDef) -> bool + Send + Sync),
    ) -> Result<()> {
        let mut defs: Vec<ServiceDef> = self
            .runners
            .iter()
            .map(|r| r.def.clone())
            .filter(|d| keep(d))
            .collect();
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
                    if let Err(e) = (start_fn)(&d.name, "stop", &[]) {
                        tracing::warn!(svc = %d.name, err = %e, "stop fail");
                    }
                    d.name
                }));
            }
            for h in handles {
                if let Ok(name) = h.await
                    && let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name)
                {
                    // issue #90: 手动停止清掉待执行的重试排期
                    r.mark_stopped();
                }
            }
        }
        Ok(())
    }

    // issue #31: 单服务 up/down。RPC up/down 的 service 参数此前被忽略 —
    // `fusion-sv down <svc>` 实际跑 down_all 全停, 与 CLI 文档 `down [svc]` 不符且危险。
    // 语义与 up_native/down_native 单条目一致: 探活跳过已健康 / env 注入 fail-closed / 状态机同款。
    // issue #122: exec 移出核心锁。RPC 单服务 up/down/restart 期间 (start.sh
    // 里 go build 链接可达数分钟) 曾持锁阻塞全部 RPC/HTTP (2026-11-11 实证)。
    // 协议: prepare (锁内, 无 exec) → StartJob::run (锁外) → finish (锁内)。

    pub async fn up_one(&mut self, name: &str) -> Result<()> {
        match self.prepare_up_one(name).await? {
            None => Ok(()),
            Some(job) => {
                let res = job.run().await;
                self.finish_up_one(name, res).await
            }
        }
    }

    // 阶段 1 (锁内): 依赖门 + 探活跳过 + env 解析, 不 exec。None = 已健康跳过。
    pub async fn prepare_up_one(&mut self, name: &str) -> Result<Option<StartJob>> {
        let d = self
            .runners
            .iter()
            .find(|r| r.def.name == name)
            .map(|r| r.def.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown service: {name}"))?;
        if !self.dep_gate_ok(&d.name).await {
            // 与 up_native 一致: 依赖问题非崩溃, 标 Stopped 跳过
            if let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name) {
                r.mark_stopped();
            }
            anyhow::bail!("dep gate 未就绪, 跳过启动: {}", d.name);
        }
        // 先探活: 已健康跳过 (避双启)
        let spec = ProbeSpec::tcp(d.port);
        if matches!((self.probe_fn)(&spec), ProbeResult::Healthy { .. }) {
            if let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name) {
                r.state = runner::State::Healthy;
            }
            tracing::info!(svc = %d.name, "skip start (already up)");
            return Ok(None);
        }
        // env 注入: shell 注册 + sidecar 连接, 均不 gate 在 compose 开关上
        // (issue #78: 默认 off 时也要注入, 否则 cowork 静默跑 SQLite), 均 fail-closed
        let env_file = self.cfg.compose.env_file.clone();
        let mut env_overrides: Vec<(String, String)> = Vec::new();
        if crate::env_map::needs_shell_env(&d.name) {
            let m = crate::env_map::shell_env_for(&d.name, &env_file).map_err(|e| {
                tracing::error!(svc = %d.name, err = %e, "shell env 解析失败, fail-closed 拒绝启动");
                e
            })?;
            env_overrides.extend(m);
        }
        if crate::env_map::needs_injection(&d.name) {
            let m = crate::env_map::native_env_for(&d.name, &env_file, self.compose_enabled())
                .map_err(|e| {
                tracing::error!(svc = %d.name, err = %e, "env 注入解析失败, fail-closed 拒绝启动");
                e
            })?;
            env_overrides.extend(m);
        }
        tracing::info!(svc = %d.name, repo = %d.repo_dir, env_n = env_overrides.len(), "start.sh start (single)");
        Ok(Some(StartJob {
            service: d.name.clone(),
            verb: "start",
            env: env_overrides,
            timeout: self.cfg.start_timeout_sec,
            start_fn: self.start_fn.clone(),
        }))
    }

    // 阶段 3 (锁内): exec 结果落状态机。
    pub async fn finish_up_one(&mut self, name: &str, res: Result<()>) -> Result<()> {
        let r = self
            .runners
            .iter_mut()
            .find(|r| r.def.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown service: {name}"))?;
        match res {
            Ok(()) => {
                r.state = runner::State::Starting;
                // issue #126: 拉起成功记启动宽限截止 — 窗口内探活失败不计失败
                // 不排重启 (慢启动 daemon 绑定口需 30-60s)
                r.set_boot_grace(now_unix());
                Ok(())
            }
            Err(e) => {
                tracing::warn!(svc = %name, err = %e, "start fail (exec 锁外返回)");
                // issue #90: 同 up_native — 排期退避重试而非终态 Failed
                r.mark_start_failed(&e.to_string(), now_unix());
                Err(e)
            }
        }
    }

    pub async fn down_one(&mut self, name: &str) -> Result<()> {
        let job = self.prepare_down_one(name).await?;
        let res = job.run().await;
        self.finish_down_one(name, res).await
    }

    // 阶段 1 (锁内): 校验 + 组 stop 作业 (不 exec)。
    pub async fn prepare_down_one(&mut self, name: &str) -> Result<StartJob> {
        let d = self
            .runners
            .iter()
            .find(|r| r.def.name == name)
            .map(|r| r.def.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown service: {name}"))?;
        tracing::info!(svc = %d.name, repo = %d.repo_dir, "start.sh stop (single)");
        Ok(StartJob {
            service: d.name.clone(),
            verb: "stop",
            env: Vec::new(),
            timeout: self.cfg.start_timeout_sec,
            start_fn: self.start_fn.clone(),
        })
    }

    // 阶段 3 (锁内): stop 结果落状态机 (stop fail 不拒 — pidfile 缺失等 no-op)。
    pub async fn finish_down_one(&mut self, name: &str, res: Result<()>) -> Result<()> {
        if let Err(e) = res.as_ref() {
            tracing::warn!(svc = %name, err = %e, "stop fail (exec 锁外返回)");
        }
        if let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name) {
            // issue #90: 手动 down 清掉待执行的启动重试排期
            r.mark_stopped();
        }
        Ok(())
    }

    // ===== issue #96: restart/rollout 必须真正替换进程 =====
    // 此前 RPC restart 与 rollout 都只 invoke_start (从不 stop): 旧监听器存活,
    // 新进程撞注册端口死掉, 探活命中旧进程 → "ok"/Healthy 但代码从未被替换。

    // stop 命令 + 端口探活验证 — 旧进程未死则 bail (调用方不得返回 ok)。
    // stop 命令失败不 bail (pidfile 缺失等 no-op 场景), 由探活验证兜底;
    // daemon spec 的孤儿监听由 daemon_stop_cmd 的 port 锚定 lsof 补杀处理。
    async fn stop_and_verify(&mut self, name: &str, run_stop: bool) -> Result<()> {
        // issue #122: restart 路径的 stop exec 已在锁外作业执行, 这里只验证
        if run_stop && let Err(e) = (self.start_fn)(name, "stop", &[]) {
            tracing::warn!(svc = %name, err = %e, "stop 命令失败 (由探活验证兜底)");
        }
        let port = self
            .runners
            .iter()
            .find(|r| r.def.name == name)
            .map(|r| r.def.port)
            .unwrap_or(0);
        let spec = ProbeSpec::tcp(port);
        // 20×250ms — daemon_stop_cmd 已在 shell 内等进程退出, 此处是脚本型
        // stop 异步退出 / 孤儿兜底生效的最后确认
        for i in 0..20 {
            if !matches!((self.probe_fn)(&spec), ProbeResult::Healthy { .. }) {
                if i > 0 {
                    tracing::info!(svc = %name, waits = i, "stop 确认: 端口已释放");
                }
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        anyhow::bail!("stop 后端口仍被占用 (旧进程未被替换): {name}")
    }

    // 人工 restart: stop 旧进程 (验证端口释放) → start 新进程。返回 pidfile
    // 记录的旧→新 PID (None = pidfile 缺失/非法)。复位熔断与启动失败重试
    // (原 RPC restart 内联语义, 移入此处与真实 stop+start 流程绑定)。
    // issue #122: RPC 路径改走三段 (prepare → 锁外 exec → finish), 此编排器
    // 仅供内部/测试 — 持锁等价旧语义。
    pub async fn restart_one(&mut self, name: &str) -> Result<(Option<u32>, Option<u32>)> {
        let (old_pid, stop_job) = self.prepare_restart(name).await?;
        let stop_res = stop_job.run().await;
        let jobs = self.restart_after_stop(name, stop_res).await?;
        let mut res = Ok(());
        for job in &jobs {
            if let Err(e) = job.run().await {
                res = Err(e);
                break;
            }
        }
        self.finish_restart(name, old_pid, res).await
    }

    // 阶段 1 (锁内): 复位计数/排期 + 组 stop 作业 (不 exec)。
    pub async fn prepare_restart(&mut self, name: &str) -> Result<(Option<u32>, StartJob)> {
        if !self.runners.iter().any(|r| r.def.name == name) {
            anyhow::bail!("unknown service: {name}");
        }
        let old_pid = read_pidfile_pid(name);
        if let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name) {
            r.policy.reset();
            r.start_fail_streak = 0;
            r.start_retry = false;
            // issue #121: 清掉崩溃路径排定的 restart_after — 否则人工 restart 刚
            // 替换进程, 到期的 tick 又 spawn 一次, 双实例抢端口后败者秒死 (实测
            // old_pid=40281 → 40s 后 new_pid=42476 的连环重启)。
            r.restart_after = None;
            r.state = runner::State::Stopped;
        }
        tracing::info!(svc = %name, old_pid, "restart: 停旧进程");
        Ok((
            old_pid,
            StartJob {
                service: name.to_string(),
                verb: "stop",
                env: Vec::new(),
                timeout: self.cfg.start_timeout_sec,
                start_fn: self.start_fn.clone(),
            },
        ))
    }

    // 阶段 2 (锁内): stop 结果核验 (探活确认端口释放, 有界等待无长 exec) +
    // 组 start 作业。非 restart 型服务 (Daemon spec) 语义 = stop → start 两次 exec。
    pub async fn restart_after_stop(
        &mut self,
        name: &str,
        stop_res: Result<()>,
    ) -> Result<Vec<StartJob>> {
        if let Err(e) = stop_res.as_ref() {
            tracing::warn!(svc = %name, err = %e, "restart: stop 命令失败 (由探活验证兜底)");
        }
        self.stop_and_verify(name, false).await?;
        tracing::info!(svc = %name, "restart: 旧进程已停, 拉起新进程");
        let r = self
            .runners
            .iter()
            .find(|r| r.def.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown service: {name}"))?;
        // 与 runner invoke_start 闭包同语义: shell 注册 env 解析, 失败仅 ERROR +
        // 空 env 继续 (restart 型 exec 需要共享 venv / key 注入, #34/#80)
        let env_file = self.cfg.compose.env_file.clone();
        let env: Vec<(String, String)> = match crate::env_map::shell_env_for(name, &env_file) {
            Ok(m) => m.into_iter().collect(),
            Err(e) => {
                tracing::error!(svc = %name, err = %e, "shell env 解析失败, restart 以空 env 继续");
                Vec::new()
            }
        };
        let timeout = self.cfg.start_timeout_sec;
        let start_fn = self.start_fn.clone();
        let mut jobs = Vec::new();
        if r.restart_cmd == "restart" {
            jobs.push(StartJob {
                service: name.to_string(),
                verb: "restart",
                env,
                timeout,
                start_fn,
            });
        } else {
            jobs.push(StartJob {
                service: name.to_string(),
                verb: "start",
                env,
                timeout,
                start_fn: start_fn.clone(),
            });
        }
        Ok(jobs)
    }

    // 阶段 3 (锁内): start 结果落状态机 + pid 替换证据。
    pub async fn finish_restart(
        &mut self,
        name: &str,
        old_pid: Option<u32>,
        res: Result<()>,
    ) -> Result<(Option<u32>, Option<u32>)> {
        let r = self
            .runners
            .iter_mut()
            .find(|r| r.def.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown service: {name}"))?;
        res?;
        r.state = runner::State::Starting;
        // issue #126: restart 拉起成功记启动宽限截止 (慢启动 daemon 绑口窗口)
        r.set_boot_grace(now_unix());
        let new_pid = read_pidfile_pid(name);
        tracing::info!(svc = %name, old_pid, new_pid, "restart: start 已发起 (pid 替换)");
        Ok((old_pid, new_pid))
    }

    // ===== PRD app-tiering (issue #62): boot 分级启动 API =====

    // 生效优先级: pin 覆盖 > 注册表 boot 字段。boot_pins 内存缓存 (构造时从 store 加载),
    // set_boot_pin 时同步更新 — boot_auto 零查询, tick/RPC 热路径不碰 SQLite。
    pub fn boot_auto(&self, d: &ServiceDef) -> bool {
        match self.boot_pins.get(&d.name) {
            Some(PinMode::Auto) => true,
            Some(PinMode::Manual) => false,
            None => d.boot_tier == BootTier::T0,
        }
    }

    // 开机集 = T0 ∪ pin=auto。auto_up_on_start 语义收窄为只拉这批 (issue #62)。
    // 复用 up_native_where: 层序 + 并发 cap + 探活跳过已健康, 行为与 up_all 一致。
    pub async fn up_boot_set(&mut self) -> Result<()> {
        // 先物化成员集再进 up_native_where — 闭包只捕获 owned set,
        // 不借 &self.boot_pins (与循环内 &mut self 借用冲突)。
        let members: std::collections::HashSet<String> = self
            .runners
            .iter()
            .filter(|r| self.boot_auto(&r.def))
            .map(|r| r.def.name.clone())
            .collect();
        tracing::info!(n = members.len(), "boot set: T0 + pin=auto");
        self.up_native_where(&|d| members.contains(&d.name)).await
    }

    // 分组批量 up/down (面板 "一键拉起/全部停止")。复用逐服务路径,
    // 返回部分失败汇总 (验收: group 部分失败可观测, 不因单个失败中断整批)。
    pub async fn boot_group(&mut self, group: &str, action: &str) -> Result<serde_json::Value> {
        if action != "up" && action != "down" {
            anyhow::bail!("unknown boot action: {action}");
        }
        let keep: Box<dyn Fn(&ServiceDef) -> bool + Send + Sync> = match group {
            "T0" => Box::new(|d: &ServiceDef| d.boot_tier == BootTier::T0),
            "T1" => Box::new(|d: &ServiceDef| d.boot_tier == BootTier::T1),
            // T2 = 按需层 (注册表无 boot 字段的缺省 tier), tray "APP · 按需" 面板
            // 一键启动/停止走这里 — 此前缺该 arm, tray 发 T2 直接 bail。
            "T2" => Box::new(|d: &ServiceDef| d.boot_tier == BootTier::T2),
            // issue #106: app = T1 ∪ T2 联合组 — tray APP 面板一键启停走这里。
            // #91 把 shell 常驻后端迁到 T1 后, 用户感知的 "app fleet" 是 T1∪T2;
            // 只发 T2 会把 8 个 T1 (cowork/agent-studio/plugins 等) 留在运行态,
            // 操作员观感即 "一键停止没反应"。T0 永不参与。
            "app" => Box::new(|d: &ServiceDef| d.boot_tier != BootTier::T0),
            "pinned" => {
                // 克隆 pin 快照进闭包 — 不借 self
                let pins: Vec<String> = self.boot_pins.keys().cloned().collect();
                Box::new(move |d: &ServiceDef| pins.contains(&d.name))
            }
            other => anyhow::bail!("unknown boot group: {other}"),
        };
        if action == "up" {
            self.up_native_where(&*keep).await?;
        } else {
            self.down_native_where(&*keep).await?;
        }
        let mut started = 0u32;
        let mut already_up = 0u32;
        let mut stopped = 0u32;
        let mut failed = 0u32;
        let mut skipped = 0u32;
        // issue #90: 启动失败已排期重试 (不再直接 Failed)
        let mut retrying = 0u32;
        for r in self.runners.iter() {
            if !keep(&r.def) {
                continue;
            }
            use runner::State as S;
            match r.state {
                S::Starting | S::Restarting => started += 1,
                S::Healthy | S::Draining => already_up += 1,
                S::Stopped if action == "down" => stopped += 1,
                S::Stopped if action == "up" => skipped += 1, // dep-gate 跳过等
                S::Unhealthy => retrying += 1,
                S::Failed => failed += 1,
                _ => skipped += 1,
            }
        }
        tracing::info!(
            group,
            action,
            started,
            already_up,
            stopped,
            failed,
            skipped,
            retrying,
            "boot group done"
        );
        Ok(serde_json::json!({
            "group": group,
            "action": action,
            "started": started,
            "already_up": already_up,
            "stopped": stopped,
            "failed": failed,
            "skipped": skipped,
            "retrying": retrying,
        }))
    }

    // /api/v1/boot 冷启动全量 (只读, 无 token — 与 /api/v1/status 对齐)。
    // tier 分组序 (T0→T1→T2), 同 tier 内稳定排序保持注册表书写顺序。
    pub fn boot_status(&self) -> Vec<BootEntry> {
        let mut out: Vec<BootEntry> = self
            .runners
            .iter()
            .map(|r| BootEntry {
                name: r.def.name.clone(),
                tier: r.def.boot_tier.as_str(),
                pinned: self
                    .boot_pins
                    .get(&r.def.name)
                    .map(|m| m.as_str().to_string()),
                state: format!("{:?}", r.state),
                port: r.def.port,
            })
            .collect();
        out.sort_by_key(|e| match e.tier {
            "T0" => 0,
            "T1" => 1,
            _ => 2,
        });
        out
    }

    // 写/清个人覆盖: 真源 store.boot_pin 表 (重启不丢, 验收 §5.4), 内存缓存同步。
    pub fn set_boot_pin(&mut self, service: &str, mode: Option<PinMode>) -> Result<()> {
        if !self.runners.iter().any(|r| r.def.name == service) {
            anyhow::bail!("unknown service: {service}");
        }
        self.store.set_boot_pin(service, mode)?;
        match mode {
            Some(m) => {
                self.boot_pins.insert(service.to_string(), m);
                tracing::info!(svc = %service, mode = m.as_str(), "boot pin set");
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if let Err(e) = self.store.record_event(
                    service,
                    "BootPin",
                    &format!("pin mode={}", m.as_str()),
                    ts,
                    "native",
                    "info",
                ) {
                    tracing::warn!(svc = %service, err = %e, "BootPin 事件写库失败 (非致命)");
                }
            }
            None => {
                self.boot_pins.remove(service);
                tracing::info!(svc = %service, "boot pin 清除 (回到注册表 boot 语义)");
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if let Err(e) =
                    self.store
                        .record_event(service, "BootPin", "pin cleared", ts, "native", "info")
                {
                    tracing::warn!(svc = %service, err = %e, "BootPin 事件写库失败 (非致命)");
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
                last_restart: None,
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
                            last_restart: None,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(err = %e, "compose ps 失败 (状态表缺容器行)");
                }
            }
        }
        // issue #112: status 附带最近一次 restart 类事件 — 事后可答
        // "这服务最近 24h 谁拉起的" (AutoRestart/ManualStart/ManualStop)。
        let restarts = self.store.last_restart_map();
        for e in &mut out {
            e.last_restart = restarts.get(&e.name).cloned();
        }
        out
    }

    // 单轮监督: native runner tick + compose 容器健康探测。
    // 容器: ps → exited/unhealthy → 标 Failed + 告警 (同 native 宕机路径)。
    // compose restart: unless-stopped 已处理自动重启, supervisor 仅报告状态 + 告警。
    pub async fn tick_all(&mut self, now_ts: u64) {
        // issue #123: 探活并发预跑 — tick 全程持核心锁, 串行探活在降级 fleet 下
        // (N 个宕机服务 × 2s 超时) 把锁扣住 N×2s, RPC/HTTP 全堵 (2026-11-11 实测:
        // 睡眠唤醒后 10+ 服务 Timeout, /api/v1/status 连续 5s+ 无响应)。
        // 并发预跑整轮压到 max(2s) 量级, 锁内只做状态机推进。
        self.preflight_probes().await;
        for r in &mut self.runners {
            if let Err(e) = r.tick(now_ts, &self.store, &mut self.alerter).await {
                tracing::warn!(svc = %r.def.name, err = %e, "tick fail (继续下一个)");
            }
        }
        self.tick_compose(now_ts).await;
    }

    // issue #123: 全 fleet 探活并发预跑 (spawn_blocking + JoinSet), 结果灌进
    // runner.cached_probe, 本轮 tick 直接取用。单 runner 失败 (panic/join) 回落
    // 现场探 (cached_probe 保持 None)。
    async fn preflight_probes(&mut self) {
        let jobs: Vec<(String, runner::ProbeFn)> = self
            .runners
            .iter()
            .map(|r| (r.def.name.clone(), r.probe_fn_clone()))
            .collect();
        let mut set = tokio::task::JoinSet::new();
        for (name, f) in jobs {
            set.spawn_blocking(move || (name, f()));
        }
        while let Some(joined) = set.join_next().await {
            if let Ok((name, result)) = joined
                && let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name)
            {
                r.cached_probe = Some(result);
            }
        }
    }

    // issue #122 phase-2: 收走 tick 挂起的到点重启作业 (锁内, 不 exec)。
    // 调用方在核心锁外经 run_pending_execs 并发执行, 再 finish_pending_execs 落账。
    pub fn collect_pending_execs(
        &mut self,
    ) -> Vec<(String, bool, runner::StartFn, runner::ProbeFn)> {
        let mut out = Vec::new();
        for r in &mut self.runners {
            if let Some(is_restart) = r.take_pending_exec() {
                r.exec_in_flight = true;
                out.push((
                    r.def.name.clone(),
                    is_restart,
                    r.start_fn_clone(),
                    r.probe_fn_clone(),
                ));
            }
        }
        if !out.is_empty() {
            tracing::info!(n = out.len(), "tick 到点重启: exec 移出核心锁");
        }
        out
    }

    // issue #122 phase-2: 锁外执行挂起重启作业 (并发), 返回逐服务结果。
    // issue #124: exec 前置探活 — 操作员手工 start.sh restart 可能已把服务拉起
    // (慢启动窗口内 #86 闸门已放行), 再拉就是双进程撞口 (调用方 502)。
    // 已 Healthy → 采纳外部实例, 跳过 exec。
    pub async fn run_pending_execs(
        execs: Vec<(String, bool, runner::StartFn, runner::ProbeFn)>,
    ) -> Vec<(String, runner::ExecOutcome)> {
        let mut set = tokio::task::JoinSet::new();
        for (name, is_restart, f, probe) in execs {
            set.spawn_blocking(move || {
                if matches!(probe(), crate::probe::ProbeResult::Healthy { .. }) {
                    return (name, runner::ExecOutcome::Adopted);
                }
                let res = runner::run_start_verbs(&f, is_restart);
                (name, runner::ExecOutcome::Ran(res))
            });
        }
        let mut out = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(pair) => out.push(pair),
                Err(e) => tracing::error!(err = %e, "pending exec join fail (该服务本轮回落)"),
            }
        }
        out
    }

    // issue #122 phase-2: 重新拿锁, 逐服务落账 exec 结果 (状态机/告警/pid 审计)。
    pub async fn finish_pending_execs(
        &mut self,
        now_ts: u64,
        results: Vec<(String, runner::ExecOutcome)>,
    ) {
        for (name, outcome) in results {
            if let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name) {
                match outcome {
                    runner::ExecOutcome::Adopted => {
                        r.apply_adopted(now_ts, &mut self.alerter).await
                    }
                    runner::ExecOutcome::Ran(res) => {
                        r.apply_exec_result(now_ts, &mut self.alerter, res).await
                    }
                }
            }
        }
    }

    // 测试/单机便捷入口: tick + 到期 exec + 落账 (持 &mut 全程 — 测试无并发,
    // 无锁可让)。生产 daemon 路径在 main.rs: tick_all (锁内) → collect → 锁外
    // run_pending_execs → finish (重新拿锁)。
    pub async fn drive(&mut self, now_ts: u64) {
        self.tick_all(now_ts).await;
        let execs = self.collect_pending_execs();
        if execs.is_empty() {
            return;
        }
        let results = Self::run_pending_execs(execs).await;
        self.finish_pending_execs(now_ts, results).await;
    }

    // issue #107 D2: daemon 冷启动状态对账。launchd 崩溃重启 (如系统睡眠中
    // 进程死亡) 后, 托管服务的 nohup 进程往往还活着, 但 runner 全部初始为
    // Stopped — 首个 tick 前的 status 会谎报 Stopped, 操作员据此手动 restart
    // 会杀掉健康进程 (实测事故 2026-11-06: 杀死 in-flight workflow)。
    // 启动时先探活一轮: probe 通过 → 直接采纳为 Healthy (复位计数/排期),
    // status 从第一个请求起就与真实探活一致; probe 失败 → 维持 Stopped
    // (真死的服务交由 auto_up / 手动 up 正常拉起)。返回采纳数 (供日志)。
    pub async fn reconcile_on_boot(&mut self) -> usize {
        use runner::State as S;
        let mut adopted = 0usize;
        // issue #111: 常驻档 (T0/T1) 服务在对账时发现死亡 → 落审计事件。
        // 这类服务预期一直在跑, daemon 缺席窗口 (睡眠/崩溃) 内的死因对
        // 事后追溯不可见 — 不记就是 #111 那种 "事件表直接跳行" 的空洞。
        // T2 按需服务不记 (不运行是常态)。记录不改状态 (仍 Stopped,
        // auto_up 只拉 boot 集合 — 与 #107 语义一致)。
        let mut dead_resident: Vec<String> = Vec::new();
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for r in &mut self.runners {
            if !matches!(r.probe_once(), crate::probe::ProbeResult::Healthy { .. }) {
                if r.def.boot_tier != crate::manifest::BootTier::T2 {
                    dead_resident.push(r.def.name.clone());
                }
                continue;
            }
            if !matches!(r.state, S::Healthy) {
                tracing::info!(svc = %r.def.name, "boot reconcile: live process adopted as Healthy");
                adopted += 1;
            }
            r.state = S::Healthy;
            r.fail_streak = 0;
            r.restart_after = None;
            r.start_retry = false;
            r.start_fail_streak = 0;
            r.policy.reset();
        }
        for name in &dead_resident {
            let _ = self
                .alerter
                .alert(crate::alert::Alert {
                    service: name.clone(),
                    kind: crate::alert::AlertKind::ServiceDown,
                    detail: "boot reconcile: found dead at daemon start, left Stopped (resident tier, not auto-started)".into(),
                    ts: now_ts,
                })
                .await;
        }
        if adopted > 0 {
            tracing::info!(adopted, "boot reconciliation done");
        }
        adopted
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
                let _ = event_kind; // 落库统一走 Alerter 事件 sink (issue #37), 此处仅告警
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

    // 依赖门: compose 启用时, 该 native 服务依赖的 sidecar 容器须全部 Healthy。
    // compose 未启用 → 无门 (无依赖可等, 保留单节点开发行为)。
    // 服务无依赖 (dep_map 未列) → 通行。
    // gate 未就绪 → 返回 false (调用方跳过启动 + 记录, 不标 Failed)。
    async fn dep_gate_ok(&mut self, name: &str) -> bool {
        let deps = dep_map::deps_for(name);
        if deps.is_empty() {
            return true;
        }
        let Some(cb) = &self.compose else {
            return true; // compose 未启用, 无门
        };
        let cs = match cb.ps() {
            Ok(cs) => cs,
            Err(e) => {
                tracing::warn!(svc = name, err = %e, "dep_gate ps 失败, 放行 (避免误阻断)");
                return true;
            }
        };
        for dep in deps {
            let found = cs.iter().find(|c| c.service == *dep);
            match found {
                Some(c) if c.supervisor_state() == "Healthy" => {
                    tracing::debug!(svc = name, dep, "dep_gate 通过");
                }
                _ => {
                    tracing::warn!(
                        svc = name,
                        dep,
                        "dep_gate 未就绪, 跳过启动 (依赖非 Healthy)"
                    );
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let _ = self
                        .alerter
                        .alert(crate::alert::Alert {
                            service: name.to_string(),
                            kind: crate::alert::AlertKind::ServiceDown,
                            detail: format!("blocked on sidecar {dep} not healthy (dep_gate)"),
                            ts,
                        })
                        .await;
                    return false;
                }
            }
        }
        true
    }

    // issue #9: 摘单服务流量。标 Draining + 设宽限期。
    // 返回 Err 若服务不存在或状态不可 drain。
    // 边界: 仅管本地生命周期 (停止接新工作), 流量切换归 fusion-gateway。
    pub fn drain(&mut self, service: &str, now_ts: u64) -> Result<()> {
        let r = self
            .runners
            .iter_mut()
            .find(|r| r.def.name == service)
            .ok_or_else(|| anyhow::anyhow!("unknown service: {service}"))?;
        r.begin_drain(self.cfg.drain_grace_sec, now_ts)?;
        Ok(())
    }

    // issue #9: 摘全部 native 服务流量 (集群 drain 的本地部分, 边界归 fusion-multi-node)。
    pub fn drain_all(&mut self, now_ts: u64) -> Result<()> {
        let mut errs = Vec::new();
        for r in &mut self.runners {
            if let Err(e) = r.begin_drain(self.cfg.drain_grace_sec, now_ts) {
                tracing::warn!(svc = %r.def.name, err = %e, "drain_all 跳过 (状态不可 drain)");
                errs.push(format!("{}: {e}", r.def.name));
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("drain_all 部分失败: {}", errs.join("; "))
        }
    }

    // issue #9: 取消单服务 drain (回流量)。仅 Draining 可取消。
    pub fn cancel_drain(&mut self, service: &str) -> Result<()> {
        let r = self
            .runners
            .iter_mut()
            .find(|r| r.def.name == service)
            .ok_or_else(|| anyhow::anyhow!("unknown service: {service}"))?;
        r.cancel_drain()?;
        Ok(())
    }

    // issue #9: 零停机换版 — drain→stop→start→prewarm(Healthy gate)→清 drain 标记。
    // prewarm: 循环探活, 达 Healthy 立即返回成功并清 drain (换版完成)。
    // 超时未达 Healthy → 告警 RolloutPrewarmFailed + 留 Stopped (不回滚, 由调用方决策)。
    // now_ts=起始时间戳; grace=drain 宽限; prewarm_timeout=探活超时。
    pub async fn rollout(
        &mut self,
        service: &str,
        now_ts: u64,
        grace: u64,
        prewarm_timeout: u64,
    ) -> Result<()> {
        {
            let r = self
                .runners
                .iter_mut()
                .find(|r| r.def.name == service)
                .ok_or_else(|| anyhow::anyhow!("unknown service: {service}"))?;
            // 1. drain (标 Draining + 宽限)
            r.begin_drain(grace, now_ts)?;
            tracing::info!(svc = %service, grace, "rollout: drain 开始");
            // 2. 等宽限期过 (drain 完成) — 实际生产由 tick 推进 Stopped, 此处直接标 Stopped 走 start
            r.state = runner::State::Stopped;
            r.drain_grace_until = None;
        }
        tracing::info!(svc = %service, "rollout: drain 完成, 停服务");
        // 2.5 issue #96: 真正停掉旧进程并验证端口释放 — 此前 rollout 从不 stop,
        // 新进程撞注册端口死掉, prewarm 探活命中旧监听 → "ok" 但代码从未被替换
        let old_pid = read_pidfile_pid(service);
        self.stop_and_verify(service, true).await?;
        tracing::info!(svc = %service, old_pid, "rollout: 旧进程已停, 拉起新进程");
        // 3. start (复用 runner.invoke_start, 1-arg start.sh 读自身 .env)
        let r = self
            .runners
            .iter_mut()
            .find(|r| r.def.name == service)
            .ok_or_else(|| anyhow::anyhow!("unknown service: {service}"))?;
        if let Err(e) = r.invoke_start() {
            tracing::error!(svc = %service, err = %e, "rollout: start 失败");
            let _ = self
                .alerter
                .alert(crate::alert::Alert {
                    service: service.to_string(),
                    kind: crate::alert::AlertKind::RolloutPrewarmFailed,
                    detail: format!("start fail: {e}"),
                    ts: now_ts,
                })
                .await;
            anyhow::bail!("rollout start fail: {e}");
        }
        r.state = runner::State::Starting;
        // issue #126: rollout 拉起成功记启动宽限截止
        r.set_boot_grace(now_ts);
        tracing::info!(svc = %service, "rollout: start 完成, prewarm 等健康");
        // 4. prewarm: 探活循环, 达 Healthy 即成功
        // issue #96 伴生缺陷: 此前循环以 t+=1 模拟时间推进且无 sleep — 60 次
        // 探活微秒级耗尽 "60s" 预算, start 未完成即误判超时 (旧实现靠探活
        // 立即命中旧监听掩盖)。改真 sleep 1s/轮, 预算=墙钟秒数 (Instant 计时,
        // 不与注入 now_ts 混用 — 测试的 now_ts 是模拟值)。
        let prewarm_start = std::time::Instant::now();
        let mut t;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            t = now_unix();
            if prewarm_start.elapsed() >= std::time::Duration::from_secs(prewarm_timeout) {
                tracing::error!(svc = %service, prewarm_timeout, "rollout: prewarm 超时未达 Healthy");
                r.state = runner::State::Stopped;
                let _ = self
                    .alerter
                    .alert(crate::alert::Alert {
                        service: service.to_string(),
                        kind: crate::alert::AlertKind::RolloutPrewarmFailed,
                        detail: format!("prewarm timeout {prewarm_timeout}s, not healthy"),
                        ts: t,
                    })
                    .await;
                anyhow::bail!(
                    "rollout prewarm timeout: {service} not healthy in {prewarm_timeout}s"
                );
            }
            let result = r.probe_once();
            if matches!(result, ProbeResult::Healthy { .. }) {
                r.state = runner::State::Healthy;
                tracing::info!(svc = %service, "rollout: prewarm 达 Healthy, 换版完成");
                return Ok(());
            }
        }
    }

    // issue #10: 执行一次备份 (Postgres + Qdrant → FUSION_BACKUP_TARGET)。
    // issue #113: 改为 detached task — 旧实现持核心 Mutex await 全程
    // (2GB dump ~15min), 期间 /api/v1/status /metrics /up 全部无响应
    // (HTTP_STILL_BUSY 实证 2026-11-09)。BackupRunner 在锁内构造 (便宜),
    // 运行移交 spawn; last_backup_ts 立即推进 + 持久化 (旧语义: 失败也推进,
    // 防重试风暴), 失败由 BackupFailed 告警兜底。
    pub fn spawn_backup(&mut self, now_ts: u64) {
        tracing::info!("backup 开始 (issue #10, detached)");
        let runner = crate::backup::BackupRunner::new(&self.cfg);
        spawn_backup_task(
            runner,
            self.store.clone(),
            self.cfg.alert_dedup_sec,
            self.cfg.alert_url.clone(),
            now_ts,
        );
        self.last_backup_ts = now_ts;
        self.store.set_meta("last_backup_ts", now_ts);
    }

    // issue #10: 调度检查。到期 (now >= last + schedule) → 触发 backup。
    // 在 daemon tick 循环内调用。返回是否触发了备份。
    // issue #113: 触发改走 spawn_backup (detached, 不持锁)。
    pub async fn check_backup_schedule(&mut self, now_ts: u64) -> bool {
        if !crate::backup::backup_due(now_ts, self.last_backup_ts, self.cfg.backup_schedule_sec) {
            return false;
        }
        // issue #103: FUSION_BACKUP_TARGET never configured = unconfigured
        // optional feature, not a failure. One WARN (latched), then silence —
        // no per-cycle ERROR spam, no BackupFailed alert (nothing attempted).
        // Manual `fusion-sv backup` / RPC backup with no target still fails
        // loudly in the spawned task (operator explicitly asked for it).
        if crate::backup::resolve_target().is_none() {
            if !self.backup_disabled_notified {
                tracing::warn!(
                    "backups disabled: FUSION_BACKUP_TARGET not set (see .env.example) — set it to enable scheduled backups"
                );
                self.backup_disabled_notified = true;
            }
            self.last_backup_ts = now_ts;
            return false;
        }
        tracing::info!(
            now_ts,
            schedule = self.cfg.backup_schedule_sec,
            "backup 调度到期, 触发"
        );
        self.spawn_backup(now_ts);
        true
    }
    // issue #37: history.db retention sweep 调度 (语义同 backup 调度)。
    // 到期执行 cleanup_old (health_check + event 两表按 history_retention_days 裁剪)。
    // 失败也更新时间戳 — 避免每 tick 重试风暴, 下轮 sweep 再试。
    // daemon tick 循环内调用。返回是否执行了 sweep。
    pub fn check_retention(&mut self, now_ts: u64) -> bool {
        if !crate::backup::backup_due(now_ts, self.last_prune_ts, self.cfg.prune_schedule_sec) {
            return false;
        }
        tracing::info!(
            now_ts,
            retention_days = self.cfg.history_retention_days,
            "retention sweep 到期, 执行"
        );
        match self
            .store
            .cleanup_old(now_ts, self.cfg.history_retention_days)
        {
            Ok(n) => {
                tracing::info!(pruned = n, "retention sweep 完成");
                self.events_pruned_total += n as u64;
            }
            Err(e) => {
                tracing::error!(err = %e, "retention sweep 失败 (下轮再试)");
            }
        }
        self.last_prune_ts = now_ts;
        true
    }
}

// issue #113: 备份任务 detached 执行 — 核心锁内只构造 runner (便宜),
// 运行移交 tokio::spawn; BackupFailed 不去抖 → 独立 Alerter 状态安全。
// store Arc 共享 (HistoryStore: Sync), 事件落库走同一 sink。
fn spawn_backup_task(
    mut runner: crate::backup::BackupRunner,
    store: Arc<HistoryStore>,
    alert_dedup_sec: u64,
    alert_url: String,
    now_ts: u64,
) {
    // #116: dump/qdrant/prune 全是同步阻塞 (std::process + std::io::copy 流过
    // 2.2GB, 分钟级) — 直接 tokio::spawn 跑在 worker 线程会把 worker 钉死,
    // runtime 饿死 → RPC 全黑 (2026-11-09 实证 curl 000 全程)。落 blocking
    // 池线程 + 内嵌 current_thread runtime 跑 async 段 (alert webhook reqwest /
    // store await), tokio worker 零阻塞。
    tokio::task::spawn_blocking(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(err = %e, "backup 内嵌 runtime 构建失败 (备份未运行)");
                return;
            }
        };
        rt.block_on(async move {
            let mut alerter = crate::alert::Alerter::new(alert_dedup_sec, alert_url)
                .with_event_sink(make_event_sink(store.clone()));
            match runner.run(now_ts, &store, &mut alerter).await {
                Ok(()) => tracing::info!("backup 完成 (detached)"),
                Err(e) => tracing::error!(err = %e, "backup 失败 (fail-closed 已告警)"),
            }
        });
    });
}

// issue #37: Alerter 事件落库 sink — 每次真实发出的告警 (去抖通过) 写 event 表。
// compose 告警 service 带 "compose:" 前缀 → 落库剥离前缀 + plane=compose
// (语义同旧 tick_compose 显式写入, issue #7)。
fn make_event_sink(store: Arc<HistoryStore>) -> crate::alert::EventSink {
    Arc::new(
        move |service: &str, kind: &str, detail: &str, ts: u64, _plane: &str, severity: &str| {
            let (svc, plane) = match service.strip_prefix("compose:") {
                Some(s) => (s, "compose"),
                None => (service, "native"),
            };
            if let Err(e) = store.record_event(svc, kind, detail, ts, plane, severity) {
                tracing::warn!(svc = svc, err = %e, "alert event write fail (非致命)");
            }
        },
    )
}

// PRD app-tiering: pin 覆盖从 store 加载 (重启不丢, 验收 §5.4)。失败降级空缓存
// (pin 表损坏不杀 daemon, 行为同告警兜底哲学, tracing warn 留痕)。
fn load_boot_pins(store: &Arc<HistoryStore>) -> std::collections::HashMap<String, PinMode> {
    store
        .all_boot_pins()
        .unwrap_or_else(|e| {
            tracing::warn!(err = %e, "boot pins 读取失败, 以空覆盖继续");
            Vec::new()
        })
        .into_iter()
        .collect()
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

// 真启动调用 (生产)。svc=服务名: start_map 有覆盖 (issue #29) 走覆盖命令,
// 否则默认 bash {repo}/start.sh {cmd}。stderr → 每服务日志。
// env_overrides: 注入 env (issue #8 sidecar 连接 + issue #29 shell 注册)。
// issue #96: 读服务 pidfile 记录的 PID (缺失/非法 → None) — restart/rollout
// 输出旧→新 PID 替换证据。
fn read_pidfile_pid(svc: &str) -> Option<u32> {
    let s = std::fs::read_to_string(crate::start_map::pid_path(svc)).ok()?;
    s.trim().parse::<u32>().ok()
}

fn real_start_pair(svc: &str, cmd: &str, env_overrides: &[(String, String)]) -> Result<()> {
    match crate::start_map::start_spec_for(svc) {
        Some(spec) => exec_start_spec(svc, &spec, cmd, env_overrides),
        None => {
            let repo_dir = crate::manifest::name_to_dir(svc);
            let sh = probe_map::start_sh_path(repo_dir);
            let r = exec_bash_script(svc, &sh, cmd, env_overrides);
            // 防 idle sleep 陪护 (gupiao 过渡补丁): start/restart 成功后挂
            // caffeinate -i -w <pid> 断言, 服务退出自动释放
            if r.is_ok()
                && matches!(cmd, "start" | "restart")
                && let Some(pidfile) = crate::start_map::caffeinate_pidfile(svc)
            {
                spawn_caffeinate(svc, pidfile);
            }
            r
        }
    }
}

// caffeinate 陪护: 读服务 pidfile, `sh -c 'caffeinate -i -w <pid> & ...'` 方式
// 孤儿化 — sh 即刻退出 (status() 回收防僵尸), caffeinate 重父到 launchd。
// pidfile 缺失/内容非数字 (start.sh 尚未写好) → WARN 跳过, 下次重启再挂。
fn spawn_caffeinate(svc: &str, pidfile: &str) {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = pidfile.replace('~', &home);
    let pid = match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            tracing::warn!(svc, pidfile = %path, err = %e, "caffeinate 陪护跳过: pidfile 不可读");
            return;
        }
    };
    if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
        tracing::warn!(svc, pidfile = %path, "caffeinate 陪护跳过: pidfile 内容非法");
        return;
    }
    let cmdline = format!("caffeinate -i -w {pid} >/dev/null 2>&1 &");
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmdline)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(_) => {
            tracing::info!(svc, pid = %pid, "caffeinate 断言已挂 (防 idle sleep, 服务退出自动释放)")
        }
        Err(e) => tracing::warn!(svc, err = %e, "caffeinate 陪护拉起失败"),
    }
}

// issue #51: 子进程 stderr 直接落服务日志文件 — 不再走 pipe + tail 线程。
// 旧方案 (issue #47) 的 join 依赖 pipe EOF, 但 start.sh 后台化的服务进程 /
// uvicorn worker 会继承 pipe 写端不关闭, child.wait() 返回后 join 永久阻塞,
// up_all 持有核心 Mutex 饿死全部 RPC/HTTP (生产事故 2026-10-27, 见 issue #51)。
// 失败 tail 改为 wait 后从日志文件回读末尾 N 行, 格式契约 "exit code=<n>" 不变。
const STDERR_TAIL_LINES: usize = 20;

// issue #121: 自动重启前把上一轮服务日志换到 <name>.log.1 (保留一代)。
// 旧实现 truncate 覆盖 — 进程在两次 spawn 之间死掉 (ProcessExit, 外部 TERM/
// 内部 should_exit) 时死因证据随重启蒸发, 只能盲查。rotate 失败不阻断启动。
fn rotate_svc_log(log_path: &std::path::Path) {
    let non_empty = std::fs::metadata(log_path)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if !non_empty {
        return;
    }
    let mut rotated = log_path.as_os_str().to_os_string();
    rotated.push(".1");
    if let Err(e) = std::fs::rename(log_path, std::path::Path::new(&rotated)) {
        tracing::debug!(log = %log_path.display(), err = %e, "svc log rotate fail");
    }
}

// 子进程 stderr 重定向目标: 服务日志文件 (换代后 truncate 打开; 打不开退化为丢弃)。
fn open_svc_stderr(svc: &str, log_path: &std::path::Path) -> std::process::Stdio {
    rotate_svc_log(log_path);
    match std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(log_path)
    {
        Ok(f) => f.into(),
        Err(e) => {
            tracing::warn!(svc, err = %e, log = %log_path.display(), "open svc log fail, stderr 丢弃");
            std::process::Stdio::null()
        }
    }
}

// 从服务日志文件回读末尾 N 行 (失败根因 tail; 文件缺失/读失败返回空串)。
fn read_log_tail(log_path: &std::path::Path) -> String {
    let bytes = std::fs::read(log_path).unwrap_or_default();
    let content = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(STDERR_TAIL_LINES);
    lines[start..].join("\n")
}

// issue #29: 覆盖服务的启动分派。
// Script: bash <script> <cmd> (trainer-bridge 自带 start/stop)。
// Daemon: start → nohup 后台 + pidfile; stop → pidfile kill; restart 上游已退化为 stop+start。
fn exec_start_spec(
    svc: &str,
    spec: &crate::start_map::StartSpec,
    cmd: &str,
    env_overrides: &[(String, String)],
) -> Result<()> {
    match spec {
        crate::start_map::StartSpec::Script(sh) => exec_bash_script(svc, sh, cmd, env_overrides),
        crate::start_map::StartSpec::Daemon { .. } => {
            let shell_cmd = match cmd {
                "start" => crate::start_map::daemon_start_cmd(spec, svc)
                    .ok_or_else(|| anyhow::anyhow!("daemon spec 缺 start 命令: {svc}"))?,
                "stop" => {
                    crate::start_map::daemon_stop_cmd(svc, crate::manifest::registered_port(svc))
                }
                other => {
                    anyhow::bail!("daemon spec 不支持 {other} (restart 应为 stop+start): {svc}");
                }
            };
            tracing::info!(svc, cmd, shell = %shell_cmd, env_n = env_overrides.len(), "exec start_map daemon cmd");
            let mut command = std::process::Command::new("bash");
            command.arg("-c").arg(&shell_cmd);
            for (k, v) in env_overrides {
                command.env(k, v);
            }
            // issue #39/#40: 剥离钉死快照的 env (如 fusion-mlx 的 FUSION_MLX_API_KEY),
            // 让服务自己的 start.sh/配置现场解析。
            for k in crate::env_map::strip_env_for(svc) {
                command.env_remove(k);
            }
            // issue #60: 子进程放独立进程组 — daemon (launchd) 重载 bootout 时
            // 信号按进程组广播, 继承 daemon 组会把整个托管 fleet 连带杀掉。
            command.process_group(0);
            // issue #51: stderr 落日志文件, 失败 tail 从文件回读 (无 pipe/无 join,
            // 后台化孙进程持有 fd 不再阻塞 exec — 见 open_svc_stderr 注释)。
            let log_path = crate::start_map::log_path(svc);
            std::fs::create_dir_all(log_path.parent().unwrap()).ok();
            let mut child = command
                .stdout(std::process::Stdio::null())
                .stderr(open_svc_stderr(svc, &log_path))
                .spawn()
                .map_err(|e| anyhow::anyhow!("spawn daemon cmd: {e}"))?;
            let status = child
                .wait()
                .map_err(|e| anyhow::anyhow!("wait daemon cmd: {e}"))?;
            if !status.success() {
                let tail = read_log_tail(&log_path);
                // 约定格式 "exit code=<n>" — runner.exit_code_of 依赖此格式 (改一处必须同步另一处)
                match status.code() {
                    Some(c) => anyhow::bail!("daemon cmd {cmd} exit code={c}: {tail}"),
                    None => anyhow::bail!("daemon cmd {cmd} killed by signal: {tail}"),
                }
            }
            Ok(())
        }
    }
}

// bash <script> <cmd>, stderr → 每服务日志 (~/.fusion-sv/logs/<svc>.log)。
fn exec_bash_script(
    svc: &str,
    sh: &std::path::Path,
    cmd: &str,
    env_overrides: &[(String, String)],
) -> Result<()> {
    if !sh.exists() {
        anyhow::bail!("脚本不存在: {}", sh.display());
    }
    std::fs::create_dir_all(crate::start_map::log_path(svc).parent().unwrap()).ok();
    let svc_log = crate::start_map::log_path(svc);
    tracing::info!(sh = %sh.display(), cmd, log = %svc_log.display(), env_n = env_overrides.len(), "exec start script");
    let mut command = std::process::Command::new("bash");
    command.arg(sh).arg(cmd);
    // 注入 env (覆盖/新增), 继承父进程其余 env
    for (k, v) in env_overrides {
        command.env(k, v);
    }
    // issue #39/#40: 剥离钉死快照的 env (如 fusion-mlx 的 FUSION_MLX_API_KEY),
    // 让服务自己的 start.sh 现场从 settings.json 解析, 防 supervisor 重启后旧 key 401 风暴。
    for k in crate::env_map::strip_env_for(svc) {
        command.env_remove(k);
    }
    // issue #60: 子进程放独立进程组 — daemon (launchd) 重载 bootout 时
    // 信号按进程组广播, 继承 daemon 组会把整个托管 fleet 连带杀掉。
    command.process_group(0);
    // issue #51: stderr 落日志文件, 失败 tail 从文件回读 (无 pipe/无 join —
    // 后台化孙进程持有 pipe 写端曾把 exec 永久挂死, 见 open_svc_stderr 注释)。
    let mut child = command
        .stdout(std::process::Stdio::null())
        .stderr(open_svc_stderr(svc, &svc_log))
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn start script: {e}"))?;
    let status = child
        .wait()
        .map_err(|e| anyhow::anyhow!("wait start script: {e}"))?;
    if !status.success() {
        let tail = read_log_tail(&svc_log);
        // 约定格式 "exit code=<n>" — runner.exit_code_of 依赖此格式 (改一处必须同步另一处)
        match status.code() {
            Some(c) => anyhow::bail!("script {cmd} exit code={c}: {tail}"),
            None => anyhow::bail!("script {cmd} killed by signal: {tail}"),
        }
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
    use std::sync::{Arc, Mutex};

    // env 注入测试共用 env_map::tests::env_lock (单一全局锁, 避免双锁竞态)。
    use crate::env_map::tests::env_lock;

    fn def(name: &str, layer: Layer, port: u16) -> ServiceDef {
        ServiceDef {
            name: name.into(),
            port,
            repo_dir: name.into(),
            purpose: "x".into(),
            fixed: false,
            managed: true,
            layer,
            boot_tier: BootTier::T2,
        }
    }

    fn def_tier(name: &str, layer: Layer, port: u16, tier: BootTier) -> ServiceDef {
        let mut d = def(name, layer, port);
        d.boot_tier = tier;
        d
    }

    fn fake_cfg() -> Config {
        Config {
            max_concurrent_start: 2,
            start_timeout_sec: 5,
            ..Config::default()
        }
    }

    // issue #39/#40: exec_bash_script 剥离 strip_env_for 列出的 env —
    // fusion-mlx 子进程不得继承 daemon 的 FUSION_MLX_API_KEY 快照 (防旧 key 401),
    // 其它服务照常继承。
    #[test]
    fn test_exec_bash_script_strips_pinned_env() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var("FUSION_MLX_API_KEY").ok();
        unsafe { std::env::set_var("FUSION_MLX_API_KEY", "stale-snapshot-key") };
        // 脚本: env 存在即 fail
        let dir = tempfile::tempdir().unwrap();
        let sh = dir.path().join("probe.sh");
        std::fs::write(
            &sh,
            "#!/bin/bash\nif [ -n \"$FUSION_MLX_API_KEY\" ]; then exit 1; fi\nexit 0\n",
        )
        .unwrap();
        // fusion-mlx: 剥离后脚本看不到 var → Ok
        assert!(
            exec_bash_script("fusion-mlx", &sh, "start", &[]).is_ok(),
            "fusion-mlx 子进程应剥离 FUSION_MLX_API_KEY"
        );
        // 其它服务: 继承 var → 脚本 fail
        assert!(
            exec_bash_script("fusion-rag", &sh, "start", &[]).is_err(),
            "其它服务照常继承 daemon env"
        );
        // 恢复进程 env
        match prev {
            Some(v) => unsafe { std::env::set_var("FUSION_MLX_API_KEY", v) },
            None => unsafe { std::env::remove_var("FUSION_MLX_API_KEY") },
        }
    }

    // issue #47: exec_bash_script 失败信息携带退出码 + stderr tail (根因回传给告警/event)
    #[test]
    fn test_exec_bash_script_captures_exit_code_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let sh = dir.path().join("fail.sh");
        std::fs::write(
            &sh,
            "#!/bin/bash\necho 'yaml: unmarshal errors: bad config' >&2\nexit 78\n",
        )
        .unwrap();
        let err = exec_bash_script("ut-tail-capture", &sh, "start", &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exit code=78"), "应携带约定格式退出码: {msg}");
        assert!(
            msg.contains("yaml: unmarshal errors: bad config"),
            "应携带 stderr tail: {msg}"
        );
        // 日志文件也应收到同一 stderr (tail 线程写盘)
        let log = crate::start_map::log_path("ut-tail-capture");
        let log_content = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(log_content.contains("yaml: unmarshal errors: bad config"));
        // 清理测试过程文件
        let _ = std::fs::remove_file(&log);
    }

    // issue #121: 服务日志换代 — 自动重启前保留上一轮 stderr 到 .log.1,
    // ProcessExit (外部 TERM / 内部 should_exit) 死因证据不再随重启蒸发。
    #[test]
    fn test_issue121_rotate_svc_log_keeps_previous_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("ut-svc.log");
        std::fs::write(&log, "old run: Shutting down\n").unwrap();
        rotate_svc_log(&log);
        assert!(!log.exists(), "rotate 后主文件让位给新一轮");
        let rotated = tmp.path().join("ut-svc.log.1");
        assert_eq!(
            std::fs::read_to_string(&rotated).unwrap(),
            "old run: Shutting down\n"
        );
        // 新一轮结束后再 rotate: .1 覆盖为最近一代
        std::fs::write(&log, "new run\n").unwrap();
        rotate_svc_log(&log);
        assert_eq!(std::fs::read_to_string(&rotated).unwrap(), "new run\n");
        // 空文件不换代 (省无谓 rename)
        std::fs::write(&log, "").unwrap();
        rotate_svc_log(&log);
        assert!(log.exists(), "空文件不 rotate");
    }

    // issue #51 回归: start.sh 后台化的孙进程继承 stderr 不关闭 — exec 不得等它。
    // 旧 pipe+join 实现会阻塞到 sleep 退出 (60s), 这里断言 <10s 内返回。
    #[test]
    fn test_exec_returns_despite_lingering_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let sh = dir.path().join("linger.sh");
        std::fs::write(&sh, "#!/bin/bash\nsleep 60 &\nexit 0\n").unwrap();
        let start = std::time::Instant::now();
        exec_bash_script("ut-linger", &sh, "start", &[])
            .expect("exec 应在孙进程仍持有 stderr 时返回");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "不得等待 pipe EOF (旧实现在此挂死, issue #51)"
        );
        // 清理测试过程文件 (孙进程 sleep 已被 tempdir 之外清理策略兜底: 60s 后自然退出)
        let _ = std::fs::remove_file(crate::start_map::log_path("ut-linger"));
    }

    // caffeinate 陪护: pidfile 指向活进程 → 断言挂上; 进程死 → caffeinate 退出
    // (绝对路径直传, 不走 ~ 展开 — 避免 HOME 全局 env 竞态)
    #[test]
    fn test_spawn_caffeinate_attaches_and_releases() {
        let dir = tempfile::tempdir().unwrap();
        // 陪护目标: sleep 进程
        let mut target = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = target.id().to_string();
        let pidfile = dir.path().join("run.pid");
        std::fs::write(&pidfile, &pid).unwrap();
        spawn_caffeinate("ut-caffeinate", pidfile.to_str().unwrap());
        // caffeinate 进程应存在且 -w 指向 target pid
        let mut attached = false;
        for _ in 0..40 {
            let out = std::process::Command::new("pgrep")
                .args(["-f", &format!("caffeinate -i -w {pid}")])
                .output()
                .unwrap();
            if out.status.success() && !out.stdout.is_empty() {
                attached = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(attached, "caffeinate 断言应挂到目标 pid");
        // 目标死亡 → caffeinate 自动退出 (-w 语义)
        let _ = target.kill();
        let _ = target.wait();
        let mut released = false;
        for _ in 0..60 {
            let out = std::process::Command::new("pgrep")
                .args(["-f", &format!("caffeinate -i -w {pid}")])
                .output()
                .unwrap();
            if !out.status.success() || out.stdout.is_empty() {
                released = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(released, "目标进程死后 caffeinate 应自动退出");
    }

    // caffeinate 陪护: pidfile 缺失 → 跳过不 panic
    #[test]
    fn test_spawn_caffeinate_skips_bad_pidfile() {
        spawn_caffeinate("ut-caffeinate", "/nonexistent-ut-xyz/run.pid");
    }

    // issue #60 回归: exec 启动的子进程必须落在独立进程组 — launchd bootout 按
    // daemon 进程组广播 SIGTERM, 继承同组会把整个托管 fleet 连带杀掉。
    // 断言: exec_bash_script 起的 bash pgid ≠ 测试进程 pgid; 普通继承 spawn 则 ==。
    #[test]
    fn test_exec_child_gets_own_process_group() {
        let our_pgid = {
            let out = std::process::Command::new("bash")
                .arg("-c")
                .arg("ps -o pgid= -p $$ | tr -d ' '")
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let dir = tempfile::tempdir().unwrap();
        let pgid_file = dir.path().join("child-pgid");
        let sh = dir.path().join("pgid.sh");
        std::fs::write(
            &sh,
            format!(
                "#!/bin/bash\nps -o pgid= -p $$ | tr -d ' ' > {}\n",
                pgid_file.display()
            ),
        )
        .unwrap();
        exec_bash_script("ut-pgid", &sh, "start", &[]).unwrap();
        let child_pgid = std::fs::read_to_string(&pgid_file)
            .expect("子进程应写出自身 pgid")
            .trim()
            .to_string();
        assert!(!child_pgid.is_empty(), "pgid 输出不得为空");
        assert_ne!(
            child_pgid, our_pgid,
            "exec 子进程应独立进程组 (issue #60), 实际与 daemon 同组"
        );
        // 清理测试过程文件
        let _ = std::fs::remove_file(crate::start_map::log_path("ut-pgid"));
    }

    // issue #31: up_one/down_one — service 级操作只动目标服务
    #[tokio::test]
    async fn test_up_one_down_one_single_service() {
        // Pin FUSION_CLIENT_KEY: issue #85 inverts client-key injection onto every
        // managed service, so the up path resolves it fail-closed. CI runners have
        // no ~/fusion/fusion-gateway/config.yaml fallback (local dev does), which
        // made these tests environment-dependent. Same fix as the trainer-bridge
        // tests in env_map.rs.
        let _ck = crate::env_map::tests::EnvGuard::set("FUSION_CLIENT_KEY", "ck-up-test");
        let defs = vec![
            def("fusion-mlx", Layer::Core, 11434),
            def("fusion-rag", Layer::App, 11436),
        ];
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_p = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |svc, cmd, _env| {
                calls_p.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        core.up_one("fusion-rag").await.unwrap();
        let got: Vec<String> = calls.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["fusion-rag/start"],
            "up_one 只应启动目标服务: {got:?}"
        );
        assert!(matches!(core.runners[1].state, runner::State::Starting));

        calls.lock().unwrap().clear();
        core.down_one("fusion-rag").await.unwrap();
        let got: Vec<String> = calls.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["fusion-rag/stop"],
            "down_one 只应停目标服务: {got:?}"
        );
        assert!(matches!(core.runners[1].state, runner::State::Stopped));

        // 未知服务 → Err
        assert!(core.up_one("fusion-nope").await.is_err());
        assert!(core.down_one("fusion-nope").await.is_err());
    }

    // issue #31: up_one 已健康跳过 (与 up_native 避双启一致)
    #[tokio::test]
    async fn test_up_one_skips_healthy() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_p = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            move |svc, cmd, _env| {
                calls_p.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        core.up_one("fusion-mlx").await.unwrap();
        assert!(calls.lock().unwrap().is_empty(), "已健康应跳过 start");
        assert!(matches!(core.runners[0].state, runner::State::Healthy));
    }

    // issue #31: supervise: false 条目不建 runner — up/down/tick/status 全跳过
    #[tokio::test]
    async fn test_unmanaged_services_excluded_from_runners() {
        let mut d1 = def("fusion-mlx", Layer::Core, 11434);
        d1.managed = true;
        let mut d2 = def("fusion-web-shell", Layer::App, 11430);
        d2.managed = false;
        let mut d3 = def("fusion-gateway-deploy", Layer::Platform, 8100);
        d3.managed = false;
        let core = SupervisorCore::new_with_fns(
            vec![d1, d2, d3],
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        assert_eq!(core.runners.len(), 1, "豁免条目不应建 runner");
        assert_eq!(core.runners[0].def.name, "fusion-mlx");
        let status = core.status_all();
        assert_eq!(status.len(), 1, "status 不含豁免条目");
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
        // Pin FUSION_CLIENT_KEY: issue #85 inverts client-key injection onto every
        // managed service, so the up path resolves it fail-closed. CI runners have
        // no ~/fusion/fusion-gateway/config.yaml fallback (local dev does), which
        // made these tests environment-dependent. Same fix as the trainer-bridge
        // tests in env_map.rs.
        let _ck = crate::env_map::tests::EnvGuard::set("FUSION_CLIENT_KEY", "ck-up-test");
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
        // 起始: 已 Unhealthy。issue #31: 退避不再持锁 sleep —
        // 首 tick 排期, 到点 tick (delay=1s) 重启 → Restarting
        core.runners[0].state = runner::State::Unhealthy;
        // issue #79: 预置连续失败计数 (直接从 Unhealthy 起测)
        core.runners[0].fail_streak = 2;
        core.drive(1000).await;
        assert!(
            matches!(core.runners[0].state, runner::State::Unhealthy),
            "排期 tick 不应重启"
        );
        core.drive(1001).await;
        assert!(
            matches!(core.runners[0].state, runner::State::Restarting),
            "tick_all 后应 Restarting"
        );
        // 翻探活为健康 → tick_all 应恢复 Healthy
        *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 3 };
        core.drive(1001).await;
        assert!(
            matches!(core.runners[0].state, runner::State::Healthy),
            "探活恢复后 tick_all 应 Healthy"
        );
    }

    // issue #112: 自动恢复拉起成功后必须落 AutoRestart 审计事件 (带 old/new pid),
    // 且 status_all 的 last_restart 列能查到它 — 事后可答 "这服务谁拉起的"
    #[tokio::test]
    async fn test_issue112_autorestart_event_and_status_last_restart() {
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
        core.runners[0].state = runner::State::Unhealthy;
        core.runners[0].fail_streak = 2;
        core.drive(1000).await;
        core.drive(1001).await; // 到点重启, invoke_start Ok → AutoRestart 事件
        // 事件落库: kind=AutoRestart, detail 带 pid 字段 (测试无 pidfile → "?")
        let (kind, detail): (String, String) = core
            .store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT kind, detail FROM event WHERE kind='AutoRestart' ORDER BY ts DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("AutoRestart 事件应已落库");
        assert!(detail.contains("old_pid="), "detail 应带 old_pid: {detail}");
        assert!(detail.contains("new_pid="), "detail 应带 new_pid: {detail}");
        // 翻探活为健康 → 恢复
        *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 3 };
        core.drive(1001).await;
        assert!(matches!(core.runners[0].state, runner::State::Healthy));
        // status enrichment: last_restart 列 = "AutoRestart ..."
        let status = core.status_all();
        let row = status.iter().find(|e| e.name == "fusion-rag").unwrap();
        let lr = row.last_restart.as_deref().expect("last_restart 应有值");
        assert!(lr.starts_with("AutoRestart"), "got: {lr}");
        let _ = kind;
    }

    // issue #111: 探活通过但 pidfile 进程已死 (陈旧监听器占口) — Daemon spec
    // 服务须转 Unhealthy (走重启流清掉陈旧监听); 活 pid / 无 pidfile 不误判
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_issue111_probe_pass_dead_pid_detected() {
        use crate::env_map::tests::env_lock;
        struct HomeGuard {
            prev: Option<String>,
        }
        impl HomeGuard {
            fn set(val: &std::path::Path) -> Self {
                let prev = std::env::var("HOME").ok();
                // SAFETY: env_lock 串行化, 此测试期间无并发读 HOME 的被测代码
                unsafe { std::env::set_var("HOME", val) };
                Self { prev }
            }
        }
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                // SAFETY: 同上
                match &self.prev {
                    Some(v) => unsafe { std::env::set_var("HOME", v) },
                    None => unsafe { std::env::remove_var("HOME") },
                }
            }
        }
        let _g0 = env_lock().lock().unwrap();
        let home = std::env::temp_dir().join(format!("fusion-sv-111-home-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".fusion-sv/pids")).unwrap();
        let _g1 = HomeGuard::set(&home);
        let pidfile = home.join(".fusion-sv/pids/fusion-cowork-web.pid");

        // 场景 1: 死 pid — i32::MAX 永不存在 (macOS pid 上限 ~99998), 免
        // 并行测试杀子进程后 pid 被即时复用的竞态
        std::fs::write(&pidfile, i32::MAX.to_string()).unwrap();
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-cowork-web", Layer::App, 11461)],
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_svc, _cmd, _env| Ok(()),
        );
        core.runners[0].state = runner::State::Healthy;
        core.drive(1000).await;
        assert!(
            matches!(core.runners[0].state, runner::State::Unhealthy),
            "探活通过但 pid 已死应转 Unhealthy"
        );
        drop(core);

        // 场景 2: 活 pid → 保持 Healthy
        let mut live = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        std::fs::write(&pidfile, live.id().to_string()).unwrap();
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-cowork-web", Layer::App, 11461)],
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_svc, _cmd, _env| Ok(()),
        );
        core.runners[0].state = runner::State::Healthy;
        core.drive(1000).await;
        assert!(matches!(core.runners[0].state, runner::State::Healthy));
        drop(core);

        // 场景 3: pidfile 缺失 → 不误判, 保持 Healthy
        let _ = std::fs::remove_file(&pidfile);
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-cowork-web", Layer::App, 11461)],
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_svc, _cmd, _env| Ok(()),
        );
        core.runners[0].state = runner::State::Healthy;
        core.drive(1000).await;
        assert!(matches!(core.runners[0].state, runner::State::Healthy));
        live.kill().unwrap();
        live.wait().unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    // issue #112: boot 对账发现常驻档 (非 T2) 服务死亡 — 落 ServiceDown 事件
    // "found dead at daemon start", 事后告警流可见, 不再静默维持 Stopped
    #[tokio::test]
    async fn test_issue112_reconcile_dead_resident_event() {
        let defs = vec![
            def_tier("fusion-cowork-web", Layer::App, 11461, BootTier::T1),
            def_tier("fusion-health", Layer::Domain, 11471, BootTier::T2),
        ];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_: &crate::probe::ProbeSpec| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            |_svc, _cmd, _env| Ok(()),
        );
        let adopted = core.reconcile_on_boot().await;
        assert_eq!(adopted, 0);
        // T1 死亡有事件, T2 (非常驻档) 无事件
        let n: i64 = core
            .store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM event WHERE kind='ServiceDown' AND detail LIKE 'boot reconcile%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "仅常驻档 (T1) 死亡落 reconcile 事件");
        let svc: String = core
            .store
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT service FROM event WHERE kind='ServiceDown' AND detail LIKE 'boot reconcile%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(svc, "fusion-cowork-web");
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
        // Pin FUSION_CLIENT_KEY: issue #85 inverts client-key injection onto every
        // managed service, so the up path resolves it fail-closed. CI runners have
        // no ~/fusion/fusion-gateway/config.yaml fallback (local dev does), which
        // made these tests environment-dependent. Same fix as the trainer-bridge
        // tests in env_map.rs.
        let _ck = crate::env_map::tests::EnvGuard::set("FUSION_CLIENT_KEY", "ck-up-test");
        let defs = vec![
            def("fusion-mlx", Layer::Core, 11434),
            def("fusion-rag", Layer::Platform, 11436),
        ];
        let native_order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let no_p = native_order.clone();
        let fake = Box::new(FakeCompose::new());
        // rag 依赖 qdrant (dep_map), 预置 qdrant Healthy 满足 dep-gate
        fake.set_containers(vec![mk_container("qdrant", "running", "healthy", 6333)]);
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
        core.drive(1000).await;
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

    // ---- dep-gate (issue #5) 集成测试 ----

    // dep_gate: compose 启用且依赖容器 Healthy → native 服务正常起
    #[tokio::test]
    async fn test_up_native_gates_on_sidecar_healthy() {
        // Pin FUSION_CLIENT_KEY: issue #85 inverts client-key injection onto every
        // managed service, so the up path resolves it fail-closed. CI runners have
        // no ~/fusion/fusion-gateway/config.yaml fallback (local dev does), which
        // made these tests environment-dependent. Same fix as the trainer-bridge
        // tests in env_map.rs.
        let _ck = crate::env_map::tests::EnvGuard::set("FUSION_CLIENT_KEY", "ck-up-test");
        let defs = vec![
            def("fusion-mlx", Layer::Core, 11434),
            def("fusion-rag", Layer::Platform, 11436), // 依赖 qdrant
        ];
        let native_started: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let ns_p = native_started.clone();
        let fake = Box::new(FakeCompose::new());
        fake.set_containers(vec![mk_container("qdrant", "running", "healthy", 6333)]);
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |svc, _cmd, _env| {
                ns_p.lock().unwrap().push(svc.to_string());
                Ok(())
            },
            Some(fake),
        );
        core.up_native().await.unwrap();
        let started = native_started.lock().unwrap();
        assert!(
            started.contains(&"fusion-rag".to_string()),
            "依赖 qdrant Healthy 时 rag 应启动"
        );
    }

    // dep_gate: 依赖容器未就绪 → 跳过该 native 服务启动 (标 Stopped, 不 Failed)
    #[tokio::test]
    async fn test_up_native_gate_skips_when_dep_unhealthy() {
        let defs = vec![
            def("fusion-mlx", Layer::Core, 11434),
            def("fusion-rag", Layer::Platform, 11436), // 依赖 qdrant
        ];
        let native_started: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let ns_p = native_started.clone();
        let fake = Box::new(FakeCompose::new());
        // qdrant exited (非 Healthy) → gate 未就绪
        fake.set_containers(vec![mk_container("qdrant", "exited", "", 0)]);
        let mut core = SupervisorCore::new_with_fns_and_compose(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |svc, _cmd, _env| {
                ns_p.lock().unwrap().push(svc.to_string());
                Ok(())
            },
            Some(fake),
        );
        core.up_native().await.unwrap();
        let started = native_started.lock().unwrap();
        assert!(
            !started.contains(&"fusion-rag".to_string()),
            "qdrant 非 Healthy 时 rag 应被 gate 跳过, 不启动"
        );
        // rag runner 应标 Stopped (非 Failed, 依赖问题非崩溃)
        let rag = core
            .runners
            .iter()
            .find(|r| r.def.name == "fusion-rag")
            .unwrap();
        assert!(
            matches!(rag.state, runner::State::Stopped),
            "gate 跳过应标 Stopped, 实际: {:?}",
            rag.state
        );
        // mlx 无依赖 → 照常起
        assert!(
            started.contains(&"fusion-mlx".to_string()),
            "无依赖服务不受 gate 影响"
        );
    }

    // dep_gate: compose 未启用 → 无门, 所有 native 正常起
    #[tokio::test]
    async fn test_up_native_no_gate_when_compose_disabled() {
        // Pin FUSION_CLIENT_KEY: issue #85 inverts client-key injection onto every
        // managed service, so the up path resolves it fail-closed. CI runners have
        // no ~/fusion/fusion-gateway/config.yaml fallback (local dev does), which
        // made these tests environment-dependent. Same fix as the trainer-bridge
        // tests in env_map.rs.
        let _ck = crate::env_map::tests::EnvGuard::set("FUSION_CLIENT_KEY", "ck-up-test");
        let defs = vec![
            def("fusion-mlx", Layer::Core, 11434),
            def("fusion-rag", Layer::Platform, 11436), // 若有门会被拦 (无 qdrant 容器)
        ];
        let native_started: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let ns_p = native_started.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |svc, _cmd, _env| {
                ns_p.lock().unwrap().push(svc.to_string());
                Ok(())
            },
        );
        assert!(!core.compose_enabled());
        core.up_native().await.unwrap();
        let started = native_started.lock().unwrap();
        assert!(
            started.contains(&"fusion-rag".to_string()),
            "compose 未启用时无 dep-gate, rag 应正常启动"
        );
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
            core.drive(ts).await;
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
        core.drive(1000).await;
        assert_eq!(
            core.compose_tracker.record("redis").unwrap().raw_state,
            "restarting"
        );
        // 切到 exited (经 Arc<Mutex> 内部可变, 无需借 core.compose)
        *containers.lock().unwrap() = vec![mk_container("redis", "exited", "", 6379)];
        core.drive(1002).await;
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
        core.drive(1000).await;
        assert_eq!(
            core.compose_tracker.record("redis").unwrap().state,
            "Failed"
        );
        // 恢复 (经 Arc<Mutex> 内部可变)
        *containers.lock().unwrap() = vec![mk_container("redis", "running", "healthy", 6379)];
        core.drive(1010).await;
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
            core.drive(ts).await;
        }
        let rec = core.compose_tracker.record("redis").unwrap();
        assert!(rec.escalated, "达阈值应升级标记");
        assert_eq!(rec.fail_count, 5);
    }

    // ---- compose 容器持久化进 history.db (issue #7) ----

    // issue #37: native 告警经 Alerter 事件 sink 落库 — kind/plane/severity 正确,
    // 去抖窗内被抑制的告警不重复落库, alerts_emitted 计数只记真实发出。
    #[tokio::test]
    async fn test_native_alert_persists_event() {
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            |_, _, _| Ok(()),
        );
        core.runners[0].state = runner::State::Healthy;
        core.drive(1000).await;
        let ev = core.store.recent_events("fusion-mlx", 10).unwrap();
        assert_eq!(ev.len(), 1, "首次宕机 ServiceDown 应落库");
        assert_eq!(ev[0].kind, "ServiceDown");
        assert_eq!(ev[0].plane, "native");
        assert_eq!(ev[0].severity, "warn");
        assert_eq!(core.alerter.alerts_emitted(), 1);
        // 去抖窗内第二次 ServiceDown: 不落库不计数 (tick 只排期重启, 不发新告警)
        core.drive(1001).await;
        let ev2 = core.store.recent_events("fusion-mlx", 10).unwrap();
        assert_eq!(ev2.len(), 1, "无新告警不应新增 event 行");
        assert_eq!(core.alerter.alerts_emitted(), 1);
    }

    // issue #37: retention sweep 调度 — 到期裁剪过期行并计数, 未到期不执行。
    #[tokio::test]
    async fn test_check_retention_prunes_and_counts() {
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        let now = 1_000_000u64;
        let old = now - (8 * 24 * 3600);
        core.store
            .record_health("fusion-mlx", "healthy", 1, old, "native")
            .unwrap();
        core.store
            .record_event("fusion-mlx", "ServiceDown", "old", old, "native", "warn")
            .unwrap();
        core.store
            .record_health("fusion-mlx", "healthy", 2, now, "native")
            .unwrap();
        assert!(core.check_retention(now), "last_prune_ts=0 → 首次即到期");
        assert_eq!(core.events_pruned_total, 2, "旧 health 行 + 旧 event 行");
        assert_eq!(
            core.store.recent_health("fusion-mlx", 10).unwrap().len(),
            1,
            "近期行保留"
        );
        // 调度间隔内未到期 → 不重复执行
        assert!(!core.check_retention(now + 60));
        assert_eq!(core.events_pruned_total, 2);
    }

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
        core.drive(1000).await;
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
        core.drive(1000).await;
        // 恢复
        *containers.lock().unwrap() = vec![mk_container("redis", "running", "healthy", 6379)];
        core.drive(1010).await;
        let ev = core.store.recent_events("redis", 10).unwrap();
        let has_back = ev
            .iter()
            .any(|e| e.kind == "ServiceBack" && e.plane == "compose");
        assert!(has_back, "恢复应写 ServiceBack 事件 plane=compose");
    }

    // issue #78: compose disabled (单节点开发) 也要注入 sidecar 连接 env —
    // 此前 gate 在 compose.enabled 上, 默认 off 时 cowork 静默跑 SQLite。
    // .env 有 FUSION_PG_PASSWORD → identity 收到 DATABASE_URL。
    // env_lock 跨 await: 测试体短, 仅串行化进程 env 读写, 不致 executor 饥饿。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_up_native_injects_env_when_compose_disabled() {
        // Pin FUSION_CLIENT_KEY: issue #85 inverts client-key injection onto every
        // managed service, so the up path resolves it fail-closed. CI runners have
        // no ~/fusion/fusion-gateway/config.yaml fallback (local dev does), which
        // made these tests environment-dependent. Same fix as the trainer-bridge
        // tests in env_map.rs.
        let _ck = crate::env_map::tests::EnvGuard::set("FUSION_CLIENT_KEY", "ck-up-test");
        let _g = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let env_f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(env_f.path(), "FUSION_PG_PASSWORD=pw78\n").unwrap();
        let mut cfg = fake_cfg(); // compose.enabled=false (默认)
        cfg.compose.env_file = env_f.path().to_path_buf();
        let defs = vec![def("fusion-identity", Layer::App, 11450)];
        let received_env: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let re_p = received_env.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            cfg,
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
        let env = received_env.lock().unwrap();
        let db_url = env
            .iter()
            .find(|(k, _)| k == "DATABASE_URL")
            .expect("compose off 也应注入 DATABASE_URL (issue #78)");
        assert!(
            db_url.1.contains(":pw78@localhost:5432/"),
            "got {}",
            db_url.1
        );
        assert!(
            matches!(core.runners[0].state, runner::State::Starting),
            "注入成功服务应 Starting"
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
        // dep-gate (#5): fusion-identity 依赖 postgres, 需 postgres Healthy 才过门,
        // 过门后才走 env 注入 (fail-closed 路径)。无此容器 dep-gate 先跳过 → Stopped。
        fake.set_containers(vec![mk_container("postgres", "running", "healthy", 5432)]);
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

    // ---- issue #90: 启动失败退避重试 ----

    // start 失败 (非 exit-78) 应排期重试而非标终态 Failed, 且不占 crash-window 名额
    #[tokio::test]
    async fn test_issue90_start_fail_schedules_retry() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            |_, _, _| Err(anyhow::anyhow!("script not found")),
        );
        core.up_all().await.unwrap();
        let r = &core.runners[0];
        assert!(
            matches!(r.state, runner::State::Unhealthy),
            "启动失败应 Unhealthy 等重试, 非终态 Failed"
        );
        assert!(r.restart_after.is_some(), "应排期重试");
        assert!(r.last_start_ts.is_none(), "从未成功启动");
        assert_eq!(r.policy.crash_count(), 0, "不占 crash-window 名额");
    }

    // exit-78 配置致命: 重试无救, 直接 Failed
    #[tokio::test]
    async fn test_issue90_start_fail_exit78_terminal() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            |_, _, _| Err(anyhow::anyhow!("exit code=78: config fatal")),
        );
        core.up_all().await.unwrap();
        let r = &core.runners[0];
        assert!(
            matches!(r.state, runner::State::Failed),
            "exit 78 应终态 Failed"
        );
        assert!(r.restart_after.is_none(), "配置致命不排期重试");
    }

    // 重试封顶: start_fail_streak 达 START_FAIL_CAP → Failed + RestartCapHit 告警
    #[tokio::test]
    async fn test_issue90_start_fail_cap_marks_failed() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            |_, _, _| Err(anyhow::anyhow!("still down")),
        );
        {
            let r = &mut core.runners[0];
            r.state = runner::State::Unhealthy;
            r.fail_streak = runner::RESTART_FAIL_STREAK;
            r.start_retry = true;
            r.start_fail_streak = runner::START_FAIL_CAP - 1;
            r.restart_after = Some(1000);
        }
        core.drive(1001).await;
        let r = &core.runners[0];
        assert!(matches!(r.state, runner::State::Failed), "达 cap 应 Failed");
        assert_eq!(r.start_fail_streak, runner::START_FAIL_CAP);
        let ev = core.store.recent_events("fusion-mlx", 10).unwrap();
        assert!(
            ev.iter()
                .any(|e| e.kind == "RestartCapHit" && e.detail.contains("start-fail cap")),
            "应记 start-fail cap 事件"
        );
    }

    // 手动 down 应清掉待执行重试排期与计数
    #[tokio::test]
    async fn test_issue90_down_clears_retry_schedule() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            |_, _, _| Err(anyhow::anyhow!("script not found")),
        );
        core.up_all().await.unwrap();
        assert!(core.runners[0].restart_after.is_some());
        core.down_one("fusion-mlx").await.unwrap();
        let r = &core.runners[0];
        assert!(matches!(r.state, runner::State::Stopped));
        assert!(r.restart_after.is_none(), "down 应清排期");
        assert_eq!(r.start_fail_streak, 0, "down 应清计数");
    }

    // ---- issue #9: drain / drain_all / rollout ----

    // drain 单服务: Healthy → Draining
    #[tokio::test]
    async fn test_drain_single_service() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        core.runners[0].state = runner::State::Healthy;
        core.drain("fusion-mlx", 1000).unwrap();
        assert!(matches!(core.runners[0].state, runner::State::Draining));
        assert_eq!(core.runners[0].drain_grace_until, Some(1030)); // grace=30 默认
    }

    // drain 未知服务 → Err
    #[tokio::test]
    async fn test_drain_unknown_service_err() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        assert!(core.drain("nope", 1000).is_err());
    }

    // drain_all: 全部 Healthy → 全 Draining
    #[tokio::test]
    async fn test_drain_all_services() {
        let defs = vec![
            def("fusion-mlx", Layer::Core, 11434),
            def("fusion-rag", Layer::App, 11436),
        ];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        for r in &mut core.runners {
            r.state = runner::State::Healthy;
        }
        core.drain_all(1000).unwrap();
        assert!(
            core.runners
                .iter()
                .all(|r| matches!(r.state, runner::State::Draining))
        );
    }

    // cancel_drain: Draining → Healthy
    #[tokio::test]
    async fn test_cancel_drain() {
        let defs = vec![def("fusion-mlx", Layer::Core, 11434)];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        core.runners[0].state = runner::State::Healthy;
        core.drain("fusion-mlx", 1000).unwrap();
        assert!(matches!(core.runners[0].state, runner::State::Draining));
        core.cancel_drain("fusion-mlx").unwrap();
        assert!(matches!(core.runners[0].state, runner::State::Healthy));
        assert!(core.runners[0].drain_grace_until.is_none());
    }

    // rollout 全周期: drain→stop→start→prewarm(Healthy)→Healthy
    // issue #96: 探活序列 = Healthy(停前) → Unhealthy(stop 验证通过) →
    // Healthy(prewarm); start_fn 收到 stop+start 两个动词
    #[tokio::test]
    async fn test_rollout_full_cycle() {
        let probe_q: Arc<Mutex<std::collections::VecDeque<ProbeResult>>> = Arc::new(Mutex::new(
            [
                ProbeResult::Healthy { latency_ms: 2 },
                ProbeResult::Unhealthy {
                    reason: crate::probe::UnhealthyReason::ConnectionRefused,
                },
                ProbeResult::Healthy { latency_ms: 3 },
            ]
            .into_iter()
            .collect(),
        ));
        let pq = probe_q.clone();
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cp = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            move |_: &ProbeSpec| {
                pq.lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(ProbeResult::Healthy { latency_ms: 3 })
            },
            move |svc, cmd, _env| {
                cp.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        core.runners[0].state = runner::State::Healthy;
        // rollout: grace=5, prewarm_timeout=10
        core.rollout("fusion-mlx", 1000, 5, 10).await.unwrap();
        assert!(
            matches!(core.runners[0].state, runner::State::Healthy),
            "rollout 成功后应 Healthy"
        );
        assert!(core.runners[0].drain_grace_until.is_none());
        // issue #96: rollout 必须先 stop 旧进程再 start
        let got = calls.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["fusion-mlx/stop", "fusion-mlx/restart"],
            "rollout 应真正 stop + invoke_start"
        );
    }

    // rollout prewarm 失败: 探活恒不健康 → 超时 → Err + 留 Stopped + 告警
    #[tokio::test]
    async fn test_rollout_prewarm_fail_alerts() {
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            |_, _, _| Ok(()),
        );
        core.runners[0].state = runner::State::Healthy;
        // prewarm_timeout=3 → 很快超时
        let res = core.rollout("fusion-mlx", 1000, 5, 3).await;
        assert!(res.is_err(), "prewarm 超时应返回 Err");
        assert!(
            matches!(core.runners[0].state, runner::State::Stopped),
            "prewarm 失败应留 Stopped (不回滚)"
        );
    }

    // ===== issue #96: restart/rollout 必须真正替换进程 =====

    // restart_one: stop → 端口释放 (探活转不健康) → start, 动词顺序 stop+start
    #[tokio::test]
    async fn test_issue96_restart_stops_then_starts() {
        let probe_q: Arc<Mutex<std::collections::VecDeque<ProbeResult>>> = Arc::new(Mutex::new(
            [
                ProbeResult::Healthy { latency_ms: 1 },
                ProbeResult::Unhealthy {
                    reason: crate::probe::UnhealthyReason::ConnectionRefused,
                },
            ]
            .into_iter()
            .collect(),
        ));
        let pq = probe_q.clone();
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cp = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            move |_: &ProbeSpec| {
                pq.lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(ProbeResult::Healthy { latency_ms: 1 })
            },
            move |svc, cmd, _env| {
                cp.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        core.runners[0].state = runner::State::Healthy;
        let (old_pid, new_pid) = core.restart_one("fusion-mlx").await.unwrap();
        assert_eq!(old_pid, None, "测试环境无 pidfile");
        assert_eq!(new_pid, None);
        let got = calls.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["fusion-mlx/stop", "fusion-mlx/restart"],
            "restart 应真正 stop + invoke_start"
        );
        assert!(matches!(core.runners[0].state, runner::State::Starting));
    }

    // issue #121: 人工 restart 必须清掉崩溃路径排定的 restart_after (单飞不变量) —
    // 否则到期 tick 再 spawn 一次, 双实例抢端口败者秒死 (cowork-web 40281→42476 实证)。
    #[tokio::test]
    async fn test_issue121_restart_clears_scheduled_crash_restart() {
        let probe_q: Arc<Mutex<std::collections::VecDeque<ProbeResult>>> = Arc::new(Mutex::new(
            [
                ProbeResult::Healthy { latency_ms: 1 },
                ProbeResult::Unhealthy {
                    reason: crate::probe::UnhealthyReason::ConnectionRefused,
                },
            ]
            .into_iter()
            .collect(),
        ));
        let pq = probe_q.clone();
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            move |_: &ProbeSpec| {
                pq.lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(ProbeResult::Unhealthy {
                        reason: crate::probe::UnhealthyReason::ConnectionRefused,
                    })
            },
            |_, _, _| Ok(()),
        );
        core.runners[0].state = runner::State::Unhealthy;
        core.runners[0].restart_after = Some(9999);
        core.restart_one("fusion-mlx").await.unwrap();
        assert!(
            core.runners[0].restart_after.is_none(),
            "restart 必须清掉排定的崩溃重启 (单飞)"
        );
    }

    // restart_one: stop 无效 (端口恒 Healthy) → bail, 绝不 start、绝不返回 ok
    #[tokio::test]
    async fn test_issue96_restart_bails_when_stop_ineffective() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cp = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            move |svc, cmd, _env| {
                cp.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        core.runners[0].state = runner::State::Healthy;
        let res = core.restart_one("fusion-mlx").await;
        assert!(res.is_err(), "stop 无效应 bail");
        let got = calls.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["fusion-mlx/stop"],
            "旧进程未死不得 start (防双启/防假替换)"
        );
        assert!(matches!(core.runners[0].state, runner::State::Stopped));
    }

    // restart_one: 未知服务 bail
    #[tokio::test]
    async fn test_issue96_restart_unknown_service() {
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        let res = core.restart_one("no-such-svc").await;
        assert!(res.is_err());
    }

    // ===== PRD app-tiering (issue #62) boot API 单测 =====

    // up_boot_set 只拉 T0 + pin=auto — T1/T2 无 pin 不参与
    #[tokio::test]
    async fn test_up_boot_set_only_t0_and_pin_auto() {
        // Pin FUSION_CLIENT_KEY: issue #85 inverts client-key injection onto every
        // managed service, so the up path resolves it fail-closed. CI runners have
        // no ~/fusion/fusion-gateway/config.yaml fallback (local dev does), which
        // made these tests environment-dependent. Same fix as the trainer-bridge
        // tests in env_map.rs.
        let _ck = crate::env_map::tests::EnvGuard::set("FUSION_CLIENT_KEY", "ck-up-test");
        let defs = vec![
            def_tier("fusion-mlx", Layer::Core, 11434, BootTier::T0),
            def_tier("fusion-gateway", Layer::Core, 11432, BootTier::T0),
            def_tier("fusion-rag", Layer::Platform, 11436, BootTier::T1),
            def("fusion-health", Layer::Domain, 11471),
        ];
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_p = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |svc, cmd, _env| {
                calls_p.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        // pin fusion-rag 成 auto — 应进入 boot 集
        core.set_boot_pin("fusion-rag", Some(PinMode::Auto))
            .unwrap();
        core.up_boot_set().await.unwrap();
        let got: Vec<String> = calls.lock().unwrap().clone();
        // 顺序: 层序 (fixed/layer) — mlx, gateway (Core), rag (Platform)
        assert_eq!(
            got,
            vec![
                "fusion-mlx/start",
                "fusion-gateway/start",
                "fusion-rag/start"
            ]
        );
        // health (T2, 无 pin) 不参与
        assert!(!got.iter().any(|s| s.starts_with("fusion-health")));
    }

    // boot_group T1 up: 部分失败汇总正确
    #[tokio::test]
    async fn test_boot_group_t1_up_partial_summary() {
        // Pin FUSION_CLIENT_KEY: issue #85 inverts client-key injection onto every
        // managed service, so the up path resolves it fail-closed. CI runners have
        // no ~/fusion/fusion-gateway/config.yaml fallback (local dev does), which
        // made these tests environment-dependent. Same fix as the trainer-bridge
        // tests in env_map.rs.
        let _ck = crate::env_map::tests::EnvGuard::set("FUSION_CLIENT_KEY", "ck-up-test");
        let defs = vec![
            def_tier("fusion-rag", Layer::Platform, 11436, BootTier::T1),
            def_tier("fusion-cowork", Layer::App, 11460, BootTier::T1),
        ];
        let fail = Arc::new(Mutex::new(vec!["fusion-cowork".to_string()]));
        let fail_p = fail.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |svc, _cmd, _env| {
                if fail_p.lock().unwrap().contains(&svc.to_string()) {
                    anyhow::bail!("boom")
                } else {
                    Ok(())
                }
            },
        );
        let res = core.boot_group("T1", "up").await.unwrap();
        assert_eq!(res["started"], 1, "rag 起了");
        assert_eq!(res["failed"], 0, "issue #90: 启动失败不再直接 Failed");
        assert_eq!(res["retrying"], 1, "cowork start 失败应排期重试");
        assert_eq!(res["group"], "T1");
        assert_eq!(res["action"], "up");
    }

    // pin 覆盖注册表: T0 unpin 成 manual 后不进 boot 集
    #[tokio::test]
    async fn test_boot_pin_overrides_registry() {
        let defs = vec![def_tier("fusion-mlx", Layer::Core, 11434, BootTier::T0)];
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_p = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            move |svc, cmd, _env| {
                calls_p.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        // T0 unpin 成 manual — 不再进 boot 集
        core.set_boot_pin("fusion-mlx", Some(PinMode::Manual))
            .unwrap();
        assert!(!core.boot_auto(&core.runners[0].def));
        assert_eq!(calls.lock().unwrap().len(), 0);
    }

    // boot_status: tier 分组序 + pinned 字段
    #[tokio::test]
    async fn test_boot_status_entries() {
        let defs = vec![
            def_tier("fusion-rag", Layer::Platform, 11436, BootTier::T1),
            def_tier("fusion-mlx", Layer::Core, 11434, BootTier::T0),
            def("fusion-health", Layer::Domain, 11471),
        ];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_svc, _cmd, _env| Ok(()),
        );
        core.set_boot_pin("fusion-health", Some(PinMode::Auto))
            .unwrap();
        let rows = core.boot_status();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "fusion-mlx");
        assert_eq!(rows[0].tier, "T0");
        assert_eq!(rows[0].pinned, None);
        assert_eq!(rows[1].name, "fusion-rag");
        assert_eq!(rows[1].tier, "T1");
        assert_eq!(rows[2].name, "fusion-health");
        assert_eq!(rows[2].pinned.as_deref(), Some("auto"));
        // 状态字符串非空
        assert!(!rows[0].state.is_empty());
    }

    // T2 分组 (APP · 按需面板): down 批量停 T2 服务, 不碰 T0
    #[tokio::test]
    async fn test_boot_group_t2_down_skips_core() {
        let defs = vec![
            def_tier("fusion-mlx", Layer::Core, 11434, BootTier::T0),
            def_tier("fusion-health", Layer::Domain, 11471, BootTier::T2),
            def_tier("fusion-finance", Layer::Domain, 11466, BootTier::T2),
        ];
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_p = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            move |svc, cmd, _env| {
                calls_p.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        let res = core.boot_group("T2", "down").await.unwrap();
        let calls = calls.lock().unwrap().clone();
        assert!(calls.contains(&"fusion-health/stop".to_string()));
        assert!(calls.contains(&"fusion-finance/stop".to_string()));
        assert!(
            !calls.iter().any(|c| c.starts_with("fusion-mlx/")),
            "T0 不动"
        );
        assert_eq!(res["group"], "T2");
        assert_eq!(res["action"], "down");
    }

    // issue #106: app 联合组 (T1∪T2) — down 停两层, 不碰 T0
    #[tokio::test]
    async fn test_boot_group_app_down_covers_t1_and_t2() {
        let defs = vec![
            def_tier("fusion-mlx", Layer::Core, 11434, BootTier::T0),
            def_tier("fusion-cowork", Layer::App, 11460, BootTier::T1),
            def_tier("fusion-health", Layer::Domain, 11471, BootTier::T2),
        ];
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_p = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            move |svc, cmd, _env| {
                calls_p.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        let res = core.boot_group("app", "down").await.unwrap();
        let calls = calls.lock().unwrap().clone();
        assert!(calls.contains(&"fusion-cowork/stop".to_string()), "T1 停");
        assert!(calls.contains(&"fusion-health/stop".to_string()), "T2 停");
        assert!(
            !calls.iter().any(|c| c.starts_with("fusion-mlx/")),
            "T0 不动"
        );
        assert_eq!(res["group"], "app");
        assert_eq!(res["action"], "down");
    }

    // issue #107 D2: 冷启动对账 — 探活通过的幸存进程采纳为 Healthy (复位计数),
    // 探活失败的维持 Stopped; 幂等, 第二轮无新增
    #[tokio::test]
    async fn test_reconcile_on_boot_adopts_live_services() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let defs = vec![
            def("fusion-cowork-web", Layer::App, 11461),
            def("fusion-health", Layer::Domain, 11471),
        ];
        let n = Arc::new(AtomicUsize::new(0));
        let n_p = n.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            move |_spec: &crate::probe::ProbeSpec| {
                // 第 1 个 runner (cowork-web) 探活通过, 第 2 个 (health) 拒连
                if n_p.fetch_add(1, Ordering::SeqCst) == 0 {
                    ProbeResult::Healthy { latency_ms: 1 }
                } else {
                    ProbeResult::Unhealthy {
                        reason: crate::probe::UnhealthyReason::ConnectionRefused,
                    }
                }
            },
            |_svc, _cmd, _env| Ok(()),
        );
        let adopted = core.reconcile_on_boot().await;
        assert_eq!(adopted, 1, "仅探活通过的服务被采纳");
        assert!(matches!(core.runners[0].state, runner::State::Healthy));
        assert!(matches!(core.runners[1].state, runner::State::Stopped));
        // 幂等: 已 Healthy 的服务第二轮不计入
        let adopted2 = core.reconcile_on_boot().await;
        assert_eq!(adopted2, 0);
    }

    // issue #110 验收: 管理的子进程被外部击杀后 daemon 重启 — 对账必须
    // adopt-or-retire 而不双启。探活拒连 (进程已死) → 保持 Stopped、不复位
    // 不排期重启 (tick 对 Stopped 不重启), 不会出现旧实例 + 新实例并存。
    #[tokio::test]
    async fn test_issue110_reconcile_dead_service_retired_no_double_start() {
        let defs = vec![
            def("fusion-cowork-web", Layer::App, 11461),
            def("fusion-health", Layer::Domain, 11471),
        ];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |spec: &crate::probe::ProbeSpec| {
                // 两个目标都已死 (外部 kill): 全部拒连
                let _ = spec;
                ProbeResult::Unhealthy {
                    reason: crate::probe::UnhealthyReason::ConnectionRefused,
                }
            },
            |_svc, _cmd, _env| Ok(()),
        );
        let adopted = core.reconcile_on_boot().await;
        assert_eq!(adopted, 0, "死服务不得被采纳");
        for r in &core.runners {
            assert!(matches!(r.state, runner::State::Stopped));
            assert!(r.restart_after.is_none(), "不得排期重启 (会双启)");
            assert!(!r.start_retry);
            assert_eq!(r.fail_streak, 0);
        }
        // 复核: reconcile 不触发任何 start 调用 (start_fn 未被记录到崩溃窗口)
        for r in &core.runners {
            assert_eq!(r.policy.crash_count(), 0, "对账不算 crash");
        }
    }

    // 非法 group/action 直接 bail, 不动任何服务
    #[tokio::test]
    async fn test_boot_group_invalid_args() {
        let defs = vec![def_tier("fusion-rag", Layer::Platform, 11436, BootTier::T1)];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_svc, _cmd, _env| Ok(()),
        );
        assert!(core.boot_group("T9", "up").await.is_err());
        assert!(core.boot_group("T0", "sideways").await.is_err());
        assert!(core.set_boot_pin("nope", Some(PinMode::Auto)).is_err());
    }

    // load_boot_pins: 从 store 重载 pin (重启不丢语义, 存取持久化由 store.rs 测)
    #[test]
    fn test_load_boot_pins_reads_store() {
        use crate::store::HistoryStore;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let s = HistoryStore::open(tmp.path()).unwrap();
        s.set_boot_pin("fusion-rag", Some(PinMode::Auto)).unwrap();
        s.set_boot_pin("fusion-health", Some(PinMode::Manual))
            .unwrap();
        let pins = load_boot_pins(&Arc::new(s));
        assert_eq!(pins.get("fusion-rag"), Some(&PinMode::Auto));
        assert_eq!(pins.get("fusion-health"), Some(&PinMode::Manual));
    }
}
