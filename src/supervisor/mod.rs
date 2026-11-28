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
// issue #60: CommandExt::process_group — service child processes get their own process group, isolating daemon-reload group signals
// (unix only; windows targets lack this API — isolation falls to CREATE_NEW_PROCESS_GROUP semantics there, simply skipped).
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use tokio::sync::Semaphore;

// Signature: (repo_dir, cmd, env_overrides). env_overrides carries injected connection env (issue #8).
pub type ArcStartFn = Arc<dyn Fn(&str, &str, &[(String, String)]) -> Result<()> + Send + Sync>;
pub type ArcProbeFn = Arc<dyn Fn(&ProbeSpec) -> ProbeResult + Send + Sync>;

// issue #122: hand-off-able exec job — produced by prepare (under the lock), the caller runs it
// outside the core lock, then re-acquires the lock to finish. exec (start.sh / go build etc.) can take minutes — never wait holding the lock.
pub struct StartJob {
    pub service: String,
    pub verb: &'static str,
    pub env: Vec<(String, String)>,
    pub timeout: u64,
    pub start_fn: ArcStartFn,
}

impl StartJob {
    // Runs outside the lock — same semantics as the old inline exec (sync closure + timeout wrapper).
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
    pub state: String, // String form of State, covers the compose plane (Failed/Healthy/...)
    pub port: u16,
    pub plane: String, // "native" | "compose"
    // issue #112: most recent restart-class event ("kind detail"), audits "who started it".
    // None = no record (never restarted / manual event missing), omitted when serializing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_restart: Option<String>,
}

// PRD app-tiering: /api/v1/boot row. pinned=None means no personal override (registry boot field applies).
#[derive(Debug, Clone, serde::Serialize)]
pub struct BootEntry {
    pub name: String,
    pub tier: &'static str,
    pub pinned: Option<String>,
    pub state: String,
    pub port: u16,
}

// issue #90: current unix seconds — the up path has no tick's injected clock, retry scheduling
// only needs the real time (state-machine test assertions go through tick's injected now_ts, unaffected).
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct SupervisorCore {
    pub runners: Vec<runner::ServiceRunner>,
    // issue #37: Arc — the Alerter's event-persist sink needs to hold a clone of the store
    // (alert chokepoint writes the event table uniformly, shared by native + compose).
    pub store: Arc<HistoryStore>,
    pub alerter: Alerter,
    pub cfg: Config,
    // Injected start function (prod = bash start.sh, test = fake). Signature: (repo_dir, cmd) -> Result
    start_fn: ArcStartFn,
    probe_fn: ArcProbeFn,
    // compose plane (optional). None when enabled=false, preserving native-only behavior.
    compose: Option<Box<dyn ComposeBackend>>,
    // compose container alert throttle (issue #6): keeps flapping containers from spamming alerts every tick.
    compose_tracker: crate::compose_tracker::ComposeTracker,
    // issue #10: last backup timestamp (for scheduling decisions). 0 = never backed up → due on first tick.
    // pub: integration tests assert the #113 detached semantics directly (tests/test_backup_detach.rs).
    pub last_backup_ts: u64,
    // issue #103: one-shot WARN latch for never-configured backup target (scheduling path stays silent, no per-tick spam).
    backup_disabled_notified: bool,
    // issue #37: last retention sweep timestamp (for scheduling, same semantics as last_backup_ts).
    last_prune_ts: u64,
    // issue #37: total history.db rows pruned (/metrics fusion_sv_events_pruned_total)
    pub events_pruned_total: u64,
    // PRD app-tiering: in-memory pin override cache (boot_auto does zero queries); source of truth is store.boot_pin
    boot_pins: std::collections::HashMap<String, PinMode>,
    // issue #137: job cursors (job name → last fired ts). In-memory mirror of meta job_last:<name>;
    // advanced at spawn time (no retry until next due on failure, same semantics as backup).
    job_last: std::collections::HashMap<String, u64>,
    // issue #139: external alert ingest dedup gate (caller dedup_key → last admitted ts).
    // pub: the http_server ingest route operates it directly (exposed alongside store/alerter).
    pub ingest_gate: crate::ingest::IngestGate,
    // Whether any compose container was Failed last tick (used by poll_interval). ps() is a blocking
    // docker subprocess; re-querying per tick inside poll_interval would double-spawn under the lock
    // (tick_compose already queried once) — reuse the last result; one tick of staleness is acceptable.
    compose_any_failed: bool,
}

impl SupervisorCore {
    // Production constructor: real bash start.sh + real probe. probe_map supplies spec + restart_cmd.
    pub fn new(defs: Vec<ServiceDef>, cfg: Config) -> Result<Self> {
        let store = Arc::new(HistoryStore::open(&cfg.db_path)?);
        let alerter = Alerter::new(cfg.alert_dedup_sec, cfg.alert_url.clone())
            .with_event_sink(make_event_sink(store.clone()));
        let env_file = cfg.compose.env_file.clone();
        let compose_on = cfg.compose.enabled;
        // issue #96: port snapshot for the daemon_stop_cmd orphan-cleanup lookup
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
                        // issue #29: the crash auto-restart path also injects shell registration env.
                        // issue #78: also inject sidecar connection env (not gated on the compose
                        // switch, matching up_native/up_one — otherwise a crash-restarted service
                        // silently falls back to SQLite again). Parse failure logs ERROR only and
                        // continues with what parsed (no bail): probe-failure alerts cover a service
                        // that truly cannot start; only the first-start path up_native is strictly fail-closed.
                        let mut env: Vec<(String, String)> =
                            match crate::env_map::shell_env_for(&name, &env_file) {
                                Ok(m) => m.into_iter().collect(),
                                Err(e) => {
                                    tracing::error!(svc = %name, err = %e, "shell env parse failed, restarting with empty env");
                                    Vec::new()
                                }
                            };
                        match crate::env_map::native_env_for(&name, &env_file, compose_on) {
                            Ok(m) => env.extend(m),
                            Err(e) => {
                                tracing::error!(svc = %name, err = %e, "sidecar env parse failed, restarting with remaining env");
                            }
                        }
                        // issue #125: per-service overlay applied last (same key overrides)
                        env.extend(crate::env_map::overlay_env_for(&name));
                        real_start_pair(&name, c, &env)
                    },
                    policy,
                )
            })
            .collect();
        // issue #126: only the production constructor injects the startup grace window; test constructors keep 0 (behavior unchanged)
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
        // issue #113: restore last backup timestamp — reload no longer re-triggers a full backup
        let last_backup_ts = store.get_meta("last_backup_ts").unwrap_or(0);
        // issue #137: restore job cursors — reload does not re-fire all jobs
        let job_last = load_job_last(&store, &cfg.jobs);
        Ok(Self {
            runners,
            store,
            alerter,
            cfg,
            start_fn: Arc::new(real_start_pair),
            probe_fn: Arc::new(real_probe_spec),
            compose,
            compose_tracker,
            // issue #113: restore last backup timestamp — reload no longer re-triggers a full backup
            last_backup_ts,
            backup_disabled_notified: false,
            last_prune_ts: 0,
            events_pruned_total: 0,
            boot_pins,
            job_last,
            // issue #139: start with an empty gate (dedup cursor not persisted — window resets after process restart, acceptable)
            ingest_gate: crate::ingest::IngestGate::default(),
            compose_any_failed: false,
        })
    }

    // Test constructor: injects fake probe + fake start. defs skip layer sorting (tests supply their own order).
    // compose optionally injected (None = native-only, Some = test compose orchestration).
    pub fn new_with_fns(
        defs: Vec<ServiceDef>,
        cfg: Config,
        probe_fn: impl Fn(&ProbeSpec) -> ProbeResult + Send + Sync + 'static,
        start_fn: impl Fn(&str, &str, &[(String, String)]) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self::new_with_fns_and_compose(defs, cfg, probe_fn, start_fn, None)
    }

    // Test constructor + compose injection.
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
        // issue #37: the test constructor also injects the event sink — native alert persistence matches prod (testable)
        let alerter = Alerter::new(cfg.alert_dedup_sec, cfg.alert_url.clone())
            .with_event_sink(make_event_sink(store.clone()));
        let start_fn = Arc::new(start_fn);
        let probe_fn = Arc::new(probe_fn);
        let runners: Vec<_> = defs
            .into_iter()
            .filter(|d| d.managed)
            .map(|d| {
                let pf = probe_fn.clone();
                // issue #96: the runner's internal start closure uses the injected start_fn (verb forwarded) —
                // invoke_start is observable in tests (previously a no-op, leaving the
                // start half of restart/rollout invisible to tests)
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
        // issue #113: restore last backup timestamp — reload no longer re-triggers a full backup
        let last_backup_ts = store.get_meta("last_backup_ts").unwrap_or(0);
        // issue #137: restore job cursors (same as the prod constructor, behavior identical)
        let job_last = load_job_last(&store, &cfg.jobs);
        Self {
            runners,
            store,
            alerter,
            cfg,
            start_fn,
            probe_fn,
            compose,
            compose_tracker,
            // issue #113: restore last backup timestamp — reload no longer re-triggers a full backup
            last_backup_ts,
            backup_disabled_notified: false,
            last_prune_ts: 0,
            events_pruned_total: 0,
            boot_pins,
            job_last,
            // issue #139: start with an empty gate (dedup cursor not persisted — window resets after process restart, acceptable)
            ingest_gate: crate::ingest::IngestGate::default(),
            compose_any_failed: false,
        }
    }

    // up: compose plane first (sidecar), native in layer order, business containers last.
    // enabled=false skips compose, native only (preserves single-node dev behavior).
    pub async fn up_all(&mut self) -> Result<()> {
        // 1. compose sidecar first (traefik/redis/postgres/qdrant), wait for healthy
        if let Some(cb) = &self.compose {
            tracing::info!("up: compose sidecar plane first");
            cb.up_sidecar()?;
        }
        // 2. native in layer order (core→...→cluster), concurrency cap within layer, probe-skip already healthy
        self.up_native().await?;
        // 3. compose business containers (model-hub/cowork), depend on sidecar health
        if let Some(cb) = &self.compose {
            tracing::info!("up: compose business containers starting (depend on sidecar)");
            cb.up_business()?;
        }
        Ok(())
    }

    // native up: layer order, concurrency cap within layer. Probe first, skip already healthy (avoid double start).
    async fn up_native(&mut self) -> Result<()> {
        self.up_native_where(&|_| true).await
    }

    // PRD app-tiering: native up with a boot-set filter — services with keep=true participate, the rest are skipped.
    // up_all uses an always-true predicate; up_boot_set / boot_group use membership predicates. Reuses existing layer order + concurrency + probe-skip logic.
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
        // Layer buckets: concurrent within a layer, serial across layers
        let mut by_layer: std::collections::BTreeMap<Layer, Vec<ServiceDef>> =
            std::collections::BTreeMap::new();
        for d in sorted {
            by_layer.entry(d.layer).or_default().push(d);
        }
        for (_layer, batch) in by_layer {
            let mut handles = Vec::new();
            for d in batch {
                // Dep gate: when compose is enabled, the sidecar containers this native service depends on must be Healthy before it starts.
                // Not relying on implicit layer order; explicit check — an unready gate skips startup (not marked Failed, a dependency problem is not a service crash).
                if !self.dep_gate_ok(&d.name).await {
                    if let Some(r) = self.runners.iter_mut().find(|r| r.def.name == d.name) {
                        // issue #90: clear leftover retry schedule (avoid a stale restart_after triggering an outdated restart)
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
                // Returns (service name, skipped=already healthy, start result, retryable)
                // retryable = real exec failure/timeout (deps may arrive late); env fail-closed is a
                // config problem no retry can fix — still marked terminal Failed (issue #90)
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    // Probe first: skip already healthy (R11: uses the injected probe_fn, not the real probe)
                    let spec = ProbeSpec::tcp(d.port);
                    if matches!((probe_fn)(&spec), ProbeResult::Healthy { .. }) {
                        tracing::info!(svc = %d.name, "skip start (already up)");
                        return (d.name, true, Ok(()), false);
                    }
                    // Two env injection classes, both fail-closed on parse error (neither gated on
                    // the compose switch):
                    // - issue #29: shell registration env
                    // - issue #8/#78: sidecar connection env — previously gated on compose.enabled;
                    //   with the default off, cowork silently ran SQLite / rag silently ran :memory:
                    let mut env_overrides: Vec<(String, String)> = Vec::new();
                    if crate::env_map::needs_shell_env(&d.name) {
                        match crate::env_map::shell_env_for(&d.name, &env_file) {
                            Ok(m) => env_overrides.extend(m),
                            Err(e) => {
                                tracing::error!(svc = %d.name, err = %e, "shell env parse failed, fail-closed refusing to start");
                                return (d.name, false, Err(e), false);
                            }
                        }
                    }
                    if crate::env_map::needs_injection(&d.name) {
                        match crate::env_map::native_env_for(&d.name, &env_file, compose_on) {
                            Ok(m) => env_overrides.extend(m),
                            Err(e) => {
                                tracing::error!(svc = %d.name, err = %e, "env injection parse failed, fail-closed refusing to start");
                                return (d.name, false, Err(e), false);
                            }
                        }
                    }
                    // issue #125: per-service overlay applied last (same key overrides)
                    env_overrides.extend(crate::env_map::overlay_env_for(&d.name));
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
                        // Adopt already-healthy: reset the full counter set (same semantics as runner #124 recovery) —
                        // flipping only the state without resetting would leave Failed runners / ones with circuit
                        // counts carrying stale crash windows, tripping the cap again at the next outage
                        r.state = runner::State::Healthy;
                        r.fail_streak = 0;
                        r.restart_after = None;
                        r.start_retry = false;
                        r.start_fail_streak = 0;
                        r.policy.reset();
                    } else if start_res.is_ok() {
                        r.state = runner::State::Starting;
                        // issue #126: on successful start, record the boot grace deadline
                        r.set_boot_grace(now_unix());
                    } else {
                        let detail = start_res
                            .err()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "start failed".into());
                        if retryable {
                            // issue #90: start failures enter the tick backoff-retry flow (missing deps are common),
                            // no longer directly marked terminal Failed — previously gateway/
                            // identity waited forever for a human when PG was not up at boot. exit-78
                            // config-fatal is still marked Failed inside mark_start_failed (no retry can fix it).
                            tracing::error!(svc = %name, %detail, "up_native: start failed, retry scheduled");
                            let now_ts = now_unix();
                            let retry = r.mark_start_failed(&detail, now_ts);
                            if retry {
                                tracing::info!(svc = %name, "start-fail retry scheduled");
                            }
                        } else {
                            // env fail-closed = config problem, retry cannot self-heal, stays terminal Failed
                            tracing::error!(svc = %name, %detail, "up_native: env parse failed, marked Failed (config problem)");
                            r.state = runner::State::Failed;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // down: business containers first → native reverse order (cluster→...→core) → sidecar last.
    // compose down stops business+sidecar in one shot, so two steps: stop business first, then down everything after native stops.
    pub async fn down_all(&mut self) -> Result<()> {
        // 1. compose down everything (business + sidecar together; containers have no native deps, safe to stop first)
        if let Some(cb) = &self.compose {
            tracing::info!("down: compose plane stopping (business + sidecar)");
            cb.down()?;
        }
        // 2. native stop in reverse order
        self.down_native().await?;
        Ok(())
    }

    // native down: reverse order (cluster→domain→app→platform→core).
    async fn down_native(&mut self) -> Result<()> {
        self.down_native_where(&|_| true).await
    }

    // PRD app-tiering: native down with a boot-set filter (group bulk stop).
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
        defs.reverse(); // stop in reverse order
        let sem = Arc::new(Semaphore::new(self.cfg.max_concurrent_start));
        let mut by_layer: std::collections::BTreeMap<Layer, Vec<ServiceDef>> =
            std::collections::BTreeMap::new();
        for d in defs {
            by_layer.entry(d.layer).or_default().push(d);
        }
        // R12: BTreeMap defaults to ascending key order (Core first), so .rev() walks descending (Cluster→...→Core)
        for (_layer, batch) in by_layer.into_iter().rev() {
            let mut handles = Vec::new();
            for d in batch {
                let sem = sem.clone();
                let start_fn = self.start_fn.clone();
                // Returns the service name; after join, runner.state = Stopped
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
                    // issue #90: manual stop clears the pending retry schedule
                    r.mark_stopped();
                }
            }
        }
        Ok(())
    }

    // issue #31: single-service up/down. The RPC up/down service param was previously ignored —
    // `fusion-sv down <svc>` actually ran down_all (full stop), contradicting the CLI doc `down [svc]` and dangerous.
    // Semantics match the up_native/down_native single entry: probe-skip healthy / fail-closed env injection / same state machine.
    // issue #122: exec moved out of the core lock. During single-service RPC up/down/restart
    // (go build linking inside start.sh can take minutes) the lock blocked all RPC/HTTP (observed 2026-11-11).
    // Protocol: prepare (under lock, no exec) → StartJob::run (outside lock) → finish (under lock).

    pub async fn up_one(&mut self, name: &str) -> Result<()> {
        match self.prepare_up_one(name).await? {
            None => Ok(()),
            Some(job) => {
                let res = job.run().await;
                self.finish_up_one(name, res).await
            }
        }
    }

    // Phase 1 (under lock): dep gate + probe skip + env resolution, no exec. None = skipped (already healthy).
    pub async fn prepare_up_one(&mut self, name: &str) -> Result<Option<StartJob>> {
        let d = self
            .runners
            .iter()
            .find(|r| r.def.name == name)
            .map(|r| r.def.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown service: {name}"))?;
        if !self.dep_gate_ok(&d.name).await {
            // Same as up_native: a dependency problem is not a crash, mark Stopped and skip
            if let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name) {
                r.mark_stopped();
            }
            anyhow::bail!("dep gate not ready, skipping start: {}", d.name);
        }
        // Probe first: skip already healthy (avoid double start)
        let spec = ProbeSpec::tcp(d.port);
        if matches!((self.probe_fn)(&spec), ProbeResult::Healthy { .. }) {
            if let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name) {
                r.state = runner::State::Healthy;
            }
            tracing::info!(svc = %d.name, "skip start (already up)");
            return Ok(None);
        }
        // env injection: shell registration + sidecar connection, neither gated on the compose
        // switch (issue #78: inject even when off by default, otherwise cowork silently runs SQLite), both fail-closed
        let env_file = self.cfg.compose.env_file.clone();
        let mut env_overrides: Vec<(String, String)> = Vec::new();
        if crate::env_map::needs_shell_env(&d.name) {
            let m = crate::env_map::shell_env_for(&d.name, &env_file).map_err(|e| {
                tracing::error!(svc = %d.name, err = %e, "shell env parse failed, fail-closed refusing to start");
                e
            })?;
            env_overrides.extend(m);
        }
        if crate::env_map::needs_injection(&d.name) {
            let m = crate::env_map::native_env_for(&d.name, &env_file, self.compose_enabled())
                .map_err(|e| {
                tracing::error!(svc = %d.name, err = %e, "env injection parse failed, fail-closed refusing to start");
                e
            })?;
            env_overrides.extend(m);
        }
        // issue #125: per-service overlay applied last (same key overrides)
        env_overrides.extend(crate::env_map::overlay_env_for(&d.name));
        tracing::info!(svc = %d.name, repo = %d.repo_dir, env_n = env_overrides.len(), "start.sh start (single)");
        Ok(Some(StartJob {
            service: d.name.clone(),
            verb: "start",
            env: env_overrides,
            timeout: self.cfg.start_timeout_sec,
            start_fn: self.start_fn.clone(),
        }))
    }

    // Phase 3 (under lock): apply the exec result to the state machine.
    pub async fn finish_up_one(&mut self, name: &str, res: Result<()>) -> Result<()> {
        let r = self
            .runners
            .iter_mut()
            .find(|r| r.def.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown service: {name}"))?;
        match res {
            Ok(()) => {
                r.state = runner::State::Starting;
                // On successful start clear leftover restart schedule / circuit counters — without
                // clearing, a restart_after left by a prior start-fail/crash flow would
                // bypass backoff and restart immediately at the next outage
                r.restart_after = None;
                r.start_retry = false;
                r.start_fail_streak = 0;
                // issue #126: on successful start record the boot grace deadline — probe failures inside the
                // window count no failures and schedule no restarts (slow-start daemons need 30-60s to bind)
                r.set_boot_grace(now_unix());
                Ok(())
            }
            Err(e) => {
                tracing::warn!(svc = %name, err = %e, "start fail (exec returned outside the lock)");
                // issue #90: same as up_native — schedule a backoff retry instead of terminal Failed
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

    // Phase 1 (under lock): validate + assemble the stop job (no exec).
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

    // Phase 3 (under lock): apply the stop result to the state machine (stop fail does not reject — pidfile missing etc. are no-ops).
    pub async fn finish_down_one(&mut self, name: &str, res: Result<()>) -> Result<()> {
        if let Err(e) = res.as_ref() {
            tracing::warn!(svc = %name, err = %e, "stop fail (exec returned outside the lock)");
        }
        if let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name) {
            // issue #90: manual down clears the pending start-retry schedule
            r.mark_stopped();
        }
        Ok(())
    }

    // ===== issue #96: restart/rollout must truly replace the process =====
    // Previously both RPC restart and rollout only invoked start (never stop): the old listener survived,
    // the new process died colliding with the registered port, the probe hit the old process → "ok"/Healthy but the code was never replaced.

    // stop command + port probe verification — if the port is still held within the window, re-issue stop (issue #134:
    // script-type stop's orphan cleanup / async exit is occasionally slower than the first verification pass; only fail
    // after bounded retries still find the port held, with the holder PID in the failure message). The caller must not get ok.
    // stop command failure does not bail (pidfile-missing etc. no-op scenarios); probe verification covers it;
    // orphan listeners of daemon specs are handled by daemon_stop_cmd's port-anchored lsof cleanup.
    async fn stop_and_verify(&mut self, name: &str, run_stop: bool) -> Result<()> {
        // issue #122: the restart path's stop exec already ran as an outside-lock job; the re-issue
        // here is the stop verb itself (TERM→grace→KILL, bounded seconds, not a long build script) —
        // acceptable under the lock, same magnitude as #123's 5s pre-run window.
        if run_stop && let Err(e) = (self.start_fn)(name, "stop", &[]) {
            tracing::warn!(svc = %name, err = %e, "stop command failed (covered by probe verification)");
        }
        let port = self
            .runners
            .iter()
            .find(|r| r.def.name == name)
            .map(|r| r.def.port)
            .unwrap_or(0);
        let spec = ProbeSpec::tcp(port);
        // 30×250ms (7.5s) — daemon_stop_cmd already waits for process exit inside the shell; this is the
        // final confirmation for script-type stop's async exit / orphan cleanup; re-issue stop at i=8 and
        // i=20 if the port is still held (at most 2 re-issues).
        for i in 0..30 {
            if !matches!((self.probe_fn)(&spec), ProbeResult::Healthy { .. }) {
                if i > 0 {
                    tracing::info!(svc = %name, waits = i, "stop confirmed: port released");
                }
                return Ok(());
            }
            if i == 8 || i == 20 {
                tracing::warn!(svc = %name, round = i, "port still held, re-issuing stop command");
                if let Err(e) = (self.start_fn)(name, "stop", &[]) {
                    tracing::warn!(svc = %name, err = %e, "stop re-issue failed (continuing probe verification)");
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        let holder = port_holder_pid(port)
            .map(|p| format!(", pid={p}"))
            .unwrap_or_default();
        anyhow::bail!(
            "port still held after stop (old process not replaced, stop re-issue ineffective): {name}:{port}{holder}"
        )
    }

    // Manual restart: stop the old process (verify port release) → start the new one. Returns the
    // pidfile-recorded old→new PIDs (None = pidfile missing/invalid). Resets circuit counters and start-fail
    // retries (the original inline RPC restart semantics, moved here and bound to the real stop+start flow).
    // issue #122: the RPC path now goes through the three phases (prepare → outside-lock exec → finish); this
    // orchestrator is for internal/test use only — holding the lock matches the old semantics.
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

    // Phase 1 (under lock): reset counters/schedule + assemble the stop job (no exec).
    pub async fn prepare_restart(&mut self, name: &str) -> Result<(Option<u32>, StartJob)> {
        if !self.runners.iter().any(|r| r.def.name == name) {
            anyhow::bail!("unknown service: {name}");
        }
        let old_pid = read_pidfile_pid(name);
        if let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name) {
            r.policy.reset();
            r.start_fail_streak = 0;
            r.start_retry = false;
            // issue #121: clear the crash path's scheduled restart_after — otherwise a manual restart just
            // replaced the process, the due tick spawns another, and the double instance fights over the port
            // with the loser dying instantly (observed chain restarts old_pid=40281 → new_pid=42476 40s later).
            r.restart_after = None;
            r.state = runner::State::Stopped;
        }
        tracing::info!(svc = %name, old_pid, "restart: stopping old process");
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

    // Phase 2 (under lock): verify the stop result (probe confirms port release, bounded wait, no long exec) +
    // assemble the start job. For non-restart services (Daemon spec) the semantics = two execs: stop → start.
    pub async fn restart_after_stop(
        &mut self,
        name: &str,
        stop_res: Result<()>,
    ) -> Result<Vec<StartJob>> {
        if let Err(e) = stop_res.as_ref() {
            tracing::warn!(svc = %name, err = %e, "restart: stop command failed (covered by probe verification)");
        }
        self.stop_and_verify(name, false).await?;
        tracing::info!(svc = %name, "restart: old process stopped, starting new process");
        let r = self
            .runners
            .iter()
            .find(|r| r.def.name == name)
            .ok_or_else(|| anyhow::anyhow!("unknown service: {name}"))?;
        // Same semantics as the runner invoke_start closure: shell registration env resolution; on failure
        // only ERROR + continue with empty env (restart-type exec needs the shared venv / key injection, #34/#80)
        let env_file = self.cfg.compose.env_file.clone();
        let env: Vec<(String, String)> = match crate::env_map::shell_env_for(name, &env_file) {
            Ok(m) => m.into_iter().collect(),
            Err(e) => {
                tracing::error!(svc = %name, err = %e, "shell env parse failed, restart continues with empty env");
                Vec::new()
            }
        };
        // issue #125: per-service overlay applied last (same key overrides)
        let mut env = env;
        env.extend(crate::env_map::overlay_env_for(name));
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

    // Phase 3 (under lock): apply the start result to the state machine + pid replacement evidence.
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
        // issue #126: on restart success record the boot grace deadline (slow-start daemon bind window)
        r.set_boot_grace(now_unix());
        let new_pid = read_pidfile_pid(name);
        tracing::info!(svc = %name, old_pid, new_pid, "restart: start issued (pid replacement)");
        Ok((old_pid, new_pid))
    }

    // ===== PRD app-tiering (issue #62): boot tiering start API =====

    // Precedence: pin override > registry boot field. boot_pins in-memory cache (loaded from the store at
    // construction), updated in sync at set_boot_pin — boot_auto does zero queries, hot tick/RPC paths never touch SQLite.
    pub fn boot_auto(&self, d: &ServiceDef) -> bool {
        match self.boot_pins.get(&d.name) {
            Some(PinMode::Auto) => true,
            Some(PinMode::Manual) => false,
            None => d.boot_tier == BootTier::T0,
        }
    }

    // Boot set = T0 ∪ pin=auto. auto_up_on_start is narrowed to only bring up this set (issue #62).
    // Reuses up_native_where: layer order + concurrency cap + probe-skip healthy, same behavior as up_all.
    pub async fn up_boot_set(&mut self) -> Result<()> {
        // Materialize the member set before entering up_native_where — the closure captures only the owned set,
        // never borrowing &self.boot_pins (conflicts with the &mut self borrow inside the loop).
        let members: std::collections::HashSet<String> = self
            .runners
            .iter()
            .filter(|r| self.boot_auto(&r.def))
            .map(|r| r.def.name.clone())
            .collect();
        tracing::info!(n = members.len(), "boot set: T0 + pin=auto");
        self.up_native_where(&|d| members.contains(&d.name)).await
    }

    // Group bulk up/down (panel "start all / stop all"). Reuses per-service paths,
    // returns a partial-failure summary (acceptance: group partial failures observable, one failure never aborts the batch).
    pub async fn boot_group(&mut self, group: &str, action: &str) -> Result<serde_json::Value> {
        if action != "up" && action != "down" {
            anyhow::bail!("unknown boot action: {action}");
        }
        let keep: Box<dyn Fn(&ServiceDef) -> bool + Send + Sync> = match group {
            "T0" => Box::new(|d: &ServiceDef| d.boot_tier == BootTier::T0),
            "T1" => Box::new(|d: &ServiceDef| d.boot_tier == BootTier::T1),
            // T2 = on-demand tier (default tier for registry entries without a boot field); the tray "APP · on-demand"
            // panel one-click start/stop goes here — this arm was previously missing and tray T2 requests bailed outright.
            "T2" => Box::new(|d: &ServiceDef| d.boot_tier == BootTier::T2),
            // issue #106: app = T1 ∪ T2 union group — the tray APP panel one-click start/stop goes here.
            // After #91 moved shell resident backends to T1, the user-perceived "app fleet" is T1∪T2;
            // sending only T2 would leave 8 T1 services (cowork/agent-studio/plugins etc.) running,
            // which operators perceive as "one-click stop does nothing". T0 never participates.
            "app" => Box::new(|d: &ServiceDef| d.boot_tier != BootTier::T0),
            "pinned" => {
                // Clone the pin snapshot into the closure — never borrow self
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
        // issue #90: start failures have a retry scheduled (no longer directly Failed)
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
                S::Stopped if action == "up" => skipped += 1, // dep-gate skips etc.
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

    // /api/v1/boot cold-start full listing (read-only, no token — aligned with /api/v1/status).
    // Tier group order (T0→T1→T2), stable sort within a tier preserves registry file order.
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

    // Write/clear personal overrides: source of truth is the store.boot_pin table (survives restart, acceptance §5.4); memory cache kept in sync.
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
                    tracing::warn!(svc = %service, err = %e, "BootPin event write fail (non-fatal)");
                }
            }
            None => {
                self.boot_pins.remove(service);
                tracing::info!(svc = %service, "boot pin cleared (back to registry boot semantics)");
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if let Err(e) =
                    self.store
                        .record_event(service, "BootPin", "pin cleared", ts, "native", "info")
                {
                    tracing::warn!(svc = %service, err = %e, "BootPin event write fail (non-fatal)");
                }
            }
        }
        Ok(())
    }

    // status: native entries + compose container entries merged into one table.
    // Containers get plane="compose", state from the docker ps health mapping.
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
                    tracing::warn!(err = %e, "compose ps failed (status table missing container rows)");
                }
            }
        }
        // issue #112: status carries the most recent restart-class event — afterwards answers
        // "who started this service in the last 24h" (AutoRestart/ManualStart/ManualStop).
        let restarts = self.store.last_restart_map();
        for e in &mut out {
            e.last_restart = restarts.get(&e.name).cloned();
        }
        out
    }

    // One supervision round: native runner tick + compose container health probing.
    // Containers: ps → exited/unhealthy → mark Failed + alert (same as the native down path).
    // compose restart: unless-stopped already handles auto-restart; supervisor only reports state + alerts.
    pub async fn tick_all(&mut self, now_ts: u64) {
        // issue #123: concurrent probe pre-run — tick holds the core lock throughout; serial probing on a
        // degraded fleet (N down services × 2s timeout) pins the lock for N×2s, blocking all RPC/HTTP
        // (measured 2026-11-11: after wake from sleep 10+ services timed out, /api/v1/status unresponsive 5s+).
        // The concurrent pre-run brings a full round down to ~max(2s); under the lock only the state machine advances.
        self.preflight_probes().await;
        for r in &mut self.runners {
            if let Err(e) = r.tick(now_ts, &self.store, &mut self.alerter).await {
                tracing::warn!(svc = %r.def.name, err = %e, "tick fail (continuing to the next)");
            }
        }
        self.tick_compose(now_ts).await;
    }

    // issue #123: fleet-wide concurrent probe pre-run (spawn_blocking + JoinSet); results feed
    // runner.cached_probe, which this tick round consumes directly. A single runner failure (panic/join)
    // falls back to probing inline (cached_probe stays None).
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

    // issue #122 phase-2: collect tick-scheduled due restart jobs (under lock, no exec).
    // The caller runs them concurrently outside the core lock via run_pending_execs, then settles results via finish_pending_execs.
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
            tracing::info!(
                n = out.len(),
                "tick due restarts: exec moved out of the core lock"
            );
        }
        out
    }

    // issue #122 phase-2: run pending restart jobs outside the lock (concurrently), returning per-service results.
    // issue #124: probe before exec — an operator's manual start.sh restart may have already brought the service up
    // (the #86 gate let it through during the slow-start window); starting again means two processes fighting over the port (caller gets 502).
    // Already Healthy → adopt the external instance, skip exec.
    pub async fn run_pending_execs(
        execs: Vec<(String, bool, runner::StartFn, runner::ProbeFn)>,
    ) -> Vec<(String, runner::ExecOutcome)> {
        let mut set = tokio::task::JoinSet::new();
        for (name, is_restart, f, probe) in execs {
            set.spawn_blocking(move || {
                // Panic isolation — if a start closure panic bubbles up as a JoinError, the service name is lost
                // and exec_in_flight can never be cleared → supervision of that service halts permanently (tick early-returns).
                // Catch and convert to Ran(Err); finish settles it and resets the gate normally.
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if matches!(probe(), crate::probe::ProbeResult::Healthy { .. }) {
                        return (name.clone(), runner::ExecOutcome::Adopted);
                    }
                    let res = runner::run_start_verbs(&f, is_restart);
                    (name.clone(), runner::ExecOutcome::Ran(res))
                })) {
                    Ok(pair) => pair,
                    Err(_) => (
                        name,
                        runner::ExecOutcome::Ran(Err(anyhow::anyhow!(
                            "start exec panicked (caught)"
                        ))),
                    ),
                }
            });
        }
        let mut out = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(pair) => out.push(pair),
                Err(e) => {
                    tracing::error!(err = %e, "pending exec join fail (that service falls back this round)")
                }
            }
        }
        out
    }

    // issue #122 phase-2: re-acquire the lock and settle exec results per service (state machine/alerts/pid audit).
    pub async fn finish_pending_execs(
        &mut self,
        now_ts: u64,
        results: Vec<(String, runner::ExecOutcome)>,
    ) {
        for (name, outcome) in results {
            if let Some(r) = self.runners.iter_mut().find(|r| r.def.name == name) {
                // issue #149: the operator downed this service while exec was in flight — the in-flight result
                // must be discarded, never settled (apply_adopted/apply_exec_result would flip Stopped
                // back to Healthy/Starting, silently undoing the ManualStop).
                if r.state == runner::State::Stopped {
                    r.exec_in_flight = false;
                    tracing::info!(
                        svc = %name,
                        "in-flight restart exec discarded: service was downed during exec"
                    );
                    continue;
                }
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

    // Test/single-machine convenience entry: tick + due exec + settle (holds &mut throughout — tests have
    // no concurrency, no lock to yield). The production daemon path lives in main.rs: tick_all (under lock) → collect →
    // run_pending_execs (outside lock) → finish (re-acquire lock).
    pub async fn drive(&mut self, now_ts: u64) {
        self.tick_all(now_ts).await;
        let execs = self.collect_pending_execs();
        if execs.is_empty() {
            return;
        }
        let results = Self::run_pending_execs(execs).await;
        self.finish_pending_execs(now_ts, results).await;
    }

    // issue #107 D2: daemon cold-start state reconciliation. After a launchd crash restart (e.g. the process
    // died mid-sleep), the managed services' nohup processes are often still alive, but all runners initialize to
    // Stopped — status before the first tick would falsely report Stopped, and an operator-triggered restart based
    // on it would kill healthy processes (observed incident 2026-11-06: killed an in-flight workflow).
    // Probe once at startup: probe passes → adopt as Healthy directly (reset counters/schedule), so
    // status matches real probing from the very first request; probe fails → stay Stopped
    // (truly dead services are brought up normally via auto_up / manual up). Returns the adoption count (for logging).
    pub async fn reconcile_on_boot(&mut self) -> usize {
        use runner::State as S;
        let mut adopted = 0usize;
        // issue #111: resident-tier (T0/T1) services found dead during reconciliation → record an audit event.
        // These services are expected to run continuously; their cause of death inside a daemon absence window
        // (sleep/crash) is invisible to hindsight — not recording is exactly #111's "event table skips a line" hole.
        // T2 on-demand services are not recorded (not running is their norm). Recording does not change state
        // (still Stopped; auto_up only starts the boot set — consistent with #107 semantics).
        let mut dead_resident: Vec<String> = Vec::new();
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // issue #157: operator intent persists across daemon restarts — all runners initialize Stopped;
        // a service whose most recent restart-class event in the event table is ManualStop was deliberately
        // stopped by the operator (core-only resident policy) and does not count as "dead" — no reconcile alert,
        // and a live leftover listener is not adopted back to Healthy (#149 semantics hold on the reconcile path too).
        // An empty event table (fresh install) counts as not operator-stopped; T0/T1 deaths still record audit events.
        let operator_stopped: std::collections::HashSet<String> = self
            .store
            .last_restart_map()
            .into_iter()
            .filter(|(_, v)| v.starts_with("ManualStop"))
            .map(|(k, _)| k)
            .collect();
        for r in &mut self.runners {
            if !matches!(r.probe_once(), crate::probe::ProbeResult::Healthy { .. }) {
                if r.def.boot_tier != crate::manifest::BootTier::T2
                    && !operator_stopped.contains(&r.def.name)
                {
                    dead_resident.push(r.def.name.clone());
                } else if r.def.boot_tier != crate::manifest::BootTier::T2 {
                    tracing::info!(
                        svc = %r.def.name,
                        "boot reconcile: resident service dead but operator-downed, no alert"
                    );
                }
                continue;
            }
            if operator_stopped.contains(&r.def.name) {
                tracing::info!(
                    svc = %r.def.name,
                    "boot reconcile: live listener but operator-downed, keep Stopped"
                );
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

    // compose container supervision: probe each container, alert on down (throttled via compose_tracker, issue #6).
    // The tracker is pure logic producing decisions; this emits alerts per decision. Containers are not restarted by the supervisor (Docker owns that).
    async fn tick_compose(&mut self, now_ts: u64) {
        let Some(cb) = &self.compose else {
            return;
        };
        let cs = match cb.ps() {
            Ok(cs) => cs,
            Err(e) => {
                tracing::warn!(err = %e, "compose ps failed (skipping container supervision this round)");
                return;
            }
        };
        self.compose_any_failed = cs.iter().any(|c| c.supervisor_state() == "Failed");
        let live: Vec<String> = cs.iter().map(|c| c.service.clone()).collect();
        for c in &cs {
            let st = c.supervisor_state();
            // Write a health row per container per tick with plane=compose (issue #7: containers enter history.db)
            if let Err(e) = self
                .store
                .record_health(&c.service, st, 0, now_ts, "compose")
            {
                tracing::warn!(svc = %c.service, err = %e, "compose health write fail (non-fatal)");
            }
            let decisions = self
                .compose_tracker
                .observe(&c.service, &c.state, st, now_ts);
            for d in decisions {
                let (kind, detail) = match d {
                    crate::compose_tracker::ComposeAlert::Down => (
                        crate::alert::AlertKind::ServiceDown,
                        format!(
                            "compose container {} state={} health={}",
                            c.name, c.state, c.health
                        ),
                    ),
                    crate::compose_tracker::ComposeAlert::StateChange => (
                        crate::alert::AlertKind::ContainerStateChange,
                        format!(
                            "compose container {} state changed to {} health={}",
                            c.name, c.state, c.health
                        ),
                    ),
                    crate::compose_tracker::ComposeAlert::CrashLoop => (
                        crate::alert::AlertKind::ContainerCrashLoop,
                        format!(
                            "compose container {} crash-loop (consecutive failures >= threshold)",
                            c.name
                        ),
                    ),
                    crate::compose_tracker::ComposeAlert::Back => (
                        crate::alert::AlertKind::ServiceBack,
                        format!("compose container {} recovered", c.name),
                    ),
                };
                tracing::info!(svc = %c.service, kind = kind.label(), "compose tick alert decision");
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
        // Drop snapshots for vanished containers
        self.compose_tracker.reap(&live);
    }

    // Probe interval: use poll_unhealthy_sec when any service is down (native or compose container), else poll_healthy_sec
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
        // compose containers in Failed also count as down (reuse last round's tick_compose ps snapshot —
        // a live re-query is a blocking docker subprocess, and it was already queried once per tick in tick_compose)
        if self.compose_any_failed {
            return self.cfg.poll_unhealthy_sec;
        }
        self.cfg.poll_healthy_sec
    }

    // logs dispatch: compose container service name → compose logs; else None (native goes through cli_logs)
    pub fn compose_logs(&self, service: &str) -> Option<Result<String>> {
        let cb = self.compose.as_ref()?;
        // Only route to compose logs when the service is in the compose container list
        match cb.ps() {
            Ok(cs) if cs.iter().any(|c| c.service == service) => Some(cb.logs(service)),
            _ => None,
        }
    }

    // Whether compose is enabled (for tests + CLI)
    pub fn compose_enabled(&self) -> bool {
        self.compose.is_some()
    }

    // Dependency gate: when compose is enabled, the sidecar containers this native service depends on must all be Healthy.
    // compose disabled → no gate (nothing to wait on, preserves single-node dev behavior).
    // Service with no mapped deps (not in dep_map) → pass through.
    // Gate not ready → return false (caller skips start + records, does not mark Failed).
    async fn dep_gate_ok(&mut self, name: &str) -> bool {
        let deps = dep_map::deps_for(name);
        if deps.is_empty() {
            return true;
        }
        let Some(cb) = &self.compose else {
            return true; // compose disabled, no gate
        };
        let cs = match cb.ps() {
            Ok(cs) => cs,
            Err(e) => {
                tracing::warn!(svc = name, err = %e, "dep_gate ps failed, pass open (avoid false blocking)");
                return true;
            }
        };
        for dep in deps {
            let found = cs.iter().find(|c| c.service == *dep);
            match found {
                Some(c) if c.supervisor_state() == "Healthy" => {
                    tracing::debug!(svc = name, dep, "dep_gate passed");
                }
                _ => {
                    tracing::warn!(
                        svc = name,
                        dep,
                        "dep_gate not ready, skipping start (dependency not Healthy)"
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

    // issue #9: drain one service's traffic. Marks Draining + sets the grace period.
    // Returns Err if the service does not exist or its state is not drainable.
    // Boundary: local lifecycle only (stop accepting new work); traffic cut-over belongs to fusion-gateway.
    pub fn drain(&mut self, service: &str, now_ts: u64) -> Result<()> {
        let r = self
            .runners
            .iter_mut()
            .find(|r| r.def.name == service)
            .ok_or_else(|| anyhow::anyhow!("unknown service: {service}"))?;
        r.begin_drain(self.cfg.drain_grace_sec, now_ts)?;
        Ok(())
    }

    // issue #9: drain all native services' traffic (local part of a cluster drain; cluster boundary belongs to fusion-multi-node).
    pub fn drain_all(&mut self, now_ts: u64) -> Result<()> {
        let mut errs = Vec::new();
        for r in &mut self.runners {
            if let Err(e) = r.begin_drain(self.cfg.drain_grace_sec, now_ts) {
                tracing::warn!(svc = %r.def.name, err = %e, "drain_all skip (state not drainable)");
                errs.push(format!("{}: {e}", r.def.name));
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("drain_all partially failed: {}", errs.join("; "))
        }
    }

    // issue #9: cancel one service's drain (restore traffic). Only Draining can be cancelled.
    pub fn cancel_drain(&mut self, service: &str) -> Result<()> {
        let r = self
            .runners
            .iter_mut()
            .find(|r| r.def.name == service)
            .ok_or_else(|| anyhow::anyhow!("unknown service: {service}"))?;
        r.cancel_drain()?;
        Ok(())
    }

    // issue #9: zero-downtime rollout — drain→stop→start→prewarm(Healthy gate)→clear drain flags.
    // prewarm: probe loop; reaching Healthy returns success immediately and clears drain (rollout complete).
    // Timeout without Healthy → RolloutPrewarmFailed alert + stays Stopped (no rollback, caller decides).
    // now_ts=start timestamp; grace=drain grace; prewarm_timeout=probe timeout.
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
            // 1. drain (mark Draining + grace)
            r.begin_drain(grace, now_ts)?;
            tracing::info!(svc = %service, grace, "rollout: drain started");
            // 2. Wait out the grace period (drain completes) — in production the tick advances to Stopped; here mark Stopped directly and proceed to start
            r.state = runner::State::Stopped;
            r.drain_grace_until = None;
        }
        tracing::info!(svc = %service, "rollout: drain done, stopping service");
        // 2.5 issue #96: actually stop the old process and verify port release — rollout previously never stopped,
        // the new process died colliding with the registered port, and prewarm probes hit the old listener → "ok" but code never replaced
        let old_pid = read_pidfile_pid(service);
        self.stop_and_verify(service, true).await?;
        tracing::info!(svc = %service, old_pid, "rollout: old process stopped, starting new process");
        // 3. start (reuse runner.invoke_start; 1-arg start.sh reads its own .env)
        let r = self
            .runners
            .iter_mut()
            .find(|r| r.def.name == service)
            .ok_or_else(|| anyhow::anyhow!("unknown service: {service}"))?;
        if let Err(e) = r.invoke_start() {
            tracing::error!(svc = %service, err = %e, "rollout: start failed");
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
        // issue #126: on rollout start success record the boot grace deadline
        r.set_boot_grace(now_ts);
        tracing::info!(svc = %service, "rollout: start done, prewarm waiting for health");
        // 4. prewarm: probe loop; reaching Healthy = success
        // issue #96 companion defect: the loop previously advanced time via t+=1 with no sleep — 60 probes
        // exhausted the "60s" budget in microseconds and misjudged a timeout before start even finished
        // (the old implementation masked this by probing straight onto the old listener). Now a real 1s sleep
        // per round, budget = wall-clock seconds (Instant timing, not mixed with injected now_ts — the tests' now_ts is simulated).
        let prewarm_start = std::time::Instant::now();
        let mut t;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            t = now_unix();
            if prewarm_start.elapsed() >= std::time::Duration::from_secs(prewarm_timeout) {
                tracing::error!(svc = %service, prewarm_timeout, "rollout: prewarm timed out without reaching Healthy");
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
                tracing::info!(svc = %service, "rollout: prewarm reached Healthy, rollout complete");
                return Ok(());
            }
        }
    }

    // issue #10: run one backup (Postgres + Qdrant → FUSION_BACKUP_TARGET).
    // issue #113: converted to a detached task — the old implementation held the core Mutex for the whole
    // await (2GB dump ~15min), leaving /api/v1/status /metrics /up unresponsive throughout
    // (HTTP_STILL_BUSY observed 2026-11-09). BackupRunner is constructed under the lock (cheap),
    // then the run is handed to spawn; last_backup_ts advances immediately + persists (old semantics: advances even
    // on failure, preventing retry storms), failures fall back to the BackupFailed alert.
    pub fn spawn_backup(&mut self, now_ts: u64) {
        tracing::info!("backup started (issue #10, detached)");
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

    // issue #10: schedule check. Due (now >= last + schedule) → trigger backup.
    // Called inside the daemon tick loop. Returns whether a backup was triggered.
    // issue #113: triggering goes through spawn_backup (detached, no lock held).
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
            "backup schedule due, triggering"
        );
        self.spawn_backup(now_ts);
        true
    }
    // issue #37: history.db retention sweep schedule (same semantics as the backup schedule).
    // On due, runs cleanup_old (trims health_check + event tables by history_retention_days).
    // The timestamp advances even on failure — avoids a per-tick retry storm; next sweep retries.
    // Called inside the daemon tick loop. Returns whether a sweep ran.
    pub fn check_retention(&mut self, now_ts: u64) -> bool {
        if !crate::backup::backup_due(now_ts, self.last_prune_ts, self.cfg.prune_schedule_sec) {
            return false;
        }
        tracing::info!(
            now_ts,
            retention_days = self.cfg.history_retention_days,
            "retention sweep due, running"
        );
        match self
            .store
            .cleanup_old(now_ts, self.cfg.history_retention_days)
        {
            Ok(n) => {
                tracing::info!(pruned = n, "retention sweep done");
                self.events_pruned_total += n as u64;
            }
            Err(e) => {
                tracing::error!(err = %e, "retention sweep failed (retry next round)");
            }
        }
        self.last_prune_ts = now_ts;
        true
    }

    // issue #137: scheduled job dispatch (inside the daemon tick loop, called under the core lock — cheap ops:
    // due check + cursor advance + detached spawn, no exec).
    // Returns the number of jobs triggered this tick (observable in tests).
    pub fn dispatch_due_jobs(&mut self, now_ts: u64) -> usize {
        if self.cfg.jobs.is_empty() {
            return 0;
        }
        let mut fired = 0;
        for job in self.cfg.jobs.clone() {
            let last = self.job_last.get(&job.name).copied().unwrap_or(0);
            if !crate::jobs::job_due(now_ts, last, &job) {
                continue;
            }
            tracing::info!(job = %job.name, now_ts, "job schedule due, triggering");
            // Cursor advances immediately + persists — spawn counts as run, failures do not retry until the next due
            // (same as backup semantics, preventing failure storms)
            self.job_last.insert(job.name.clone(), now_ts);
            self.store
                .set_meta(&format!("job_last:{}", job.name), now_ts);
            spawn_job_task(
                job,
                self.store.clone(),
                self.cfg.alert_dedup_sec,
                self.cfg.alert_url.clone(),
                now_ts,
            );
            fired += 1;
        }
        fired
    }
}

// issue #137: job cursors restored from meta (config present but meta missing → 0, due on first start).
fn load_job_last(
    store: &Arc<HistoryStore>,
    jobs: &[crate::config::JobConfig],
) -> std::collections::HashMap<String, u64> {
    jobs.iter()
        .map(|j| {
            let last = store.get_meta(&format!("job_last:{}", j.name)).unwrap_or(0);
            (j.name.clone(), last)
        })
        .collect()
}

// issue #137: job execution detached — exec is synchronously blocking (std::process + try_wait polling,
// up to timeout minutes), same pattern as spawn_backup_task: blocking pool + embedded
// current_thread runtime for the async section (alert webhook), tokio workers untouched.
// Event persistence: success info / failure error + JobFailed alert (independent Alerter, no dedup).
// stdout/stderr append to ~/.fusion-sv/logs/<job>.log (job logs share the service log directory).
fn spawn_job_task(
    job: crate::config::JobConfig,
    store: Arc<HistoryStore>,
    alert_dedup_sec: u64,
    alert_url: String,
    now_ts: u64,
) {
    tokio::task::spawn_blocking(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(job = %job.name, err = %e, "job embedded runtime build failed (task not run)");
                return;
            }
        };
        rt.block_on(async move { run_job(job, store, alert_dedup_sec, alert_url, now_ts).await });
    });
}

async fn run_job(
    job: crate::config::JobConfig,
    store: Arc<HistoryStore>,
    alert_dedup_sec: u64,
    alert_url: String,
    now_ts: u64,
) {
    // bin resolution: containing '/' resolves relative to FUSION_ROOT (same semantics as start_map::resolve_bin);
    // bare names prefer the shared venv bin, falling back to PATH.
    let bin = crate::start_map::resolve_bin(&job.bin);
    let log = crate::start_map::log_path(&format!("job-{}", job.name));
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // per-job env overlay (#125 reuse) — FUSION_OPS_DATABASE_URL etc. go through
    // ~/.fusion-sv/env/<job>.env; values are never logged.
    let overlay = crate::env_map::overlay_env_for(&job.name);
    tracing::info!(
        job = %job.name,
        timeout_sec = job.timeout_sec,
        env_n = overlay.len(),
        log = %log.display(),
        "job started"
    );
    let log_f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log);
    let (stdout_f, stderr_f) = match log_f {
        Ok(f) => {
            let out = f.try_clone();
            match out {
                Ok(o) => (Some(f), Some(o)),
                Err(e) => {
                    tracing::warn!(job = %job.name, err = %e, "job log handle clone failed, falling back to null");
                    (None, None)
                }
            }
        }
        Err(e) => {
            tracing::warn!(job = %job.name, err = %e, "job log open failed, falling back to null");
            (None, None)
        }
    };
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(&job.args)
        .stdout(stdout_f.map_or_else(std::process::Stdio::null, std::process::Stdio::from))
        .stderr(stderr_f.map_or_else(std::process::Stdio::null, std::process::Stdio::from));
    for (k, v) in overlay {
        cmd.env(k, v);
    }
    let spawn = cmd.spawn();
    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            let detail = format!("spawn fail: {e}");
            tracing::error!(job = %job.name, "{}", detail);
            record_job_outcome(
                &store,
                alert_dedup_sec,
                alert_url,
                &job,
                &detail,
                now_ts,
                false,
            )
            .await;
            return;
        }
    };
    // Poll try_wait until timeout → kill (500ms granularity; polling overhead is negligible for minute-scale jobs)
    let deadline = now_ts + job.timeout_sec;
    let mut status = None;
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => {
                let now = now_unix();
                if now >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) => {
                let detail = format!("wait fail: {e}");
                tracing::error!(job = %job.name, "{}", detail);
                record_job_outcome(
                    &store,
                    alert_dedup_sec,
                    alert_url,
                    &job,
                    &detail,
                    now_ts,
                    false,
                )
                .await;
                return;
            }
        }
    }
    let ok = !timed_out && status.map(|s| s.success()).unwrap_or(false);
    let detail = if timed_out {
        format!(
            "timeout after {}s, killed (log: {})",
            job.timeout_sec,
            log.display()
        )
    } else {
        format!(
            "exit code={} (log: {})",
            status
                .map(|s| s.code().unwrap_or(-1).to_string())
                .unwrap_or_else(|| "unknown".into()),
            log.display()
        )
    };
    if ok {
        tracing::info!(job = %job.name, "{}", detail);
    } else {
        tracing::error!(job = %job.name, "{}", detail);
    }
    record_job_outcome(
        &store,
        alert_dedup_sec,
        alert_url,
        &job,
        &detail,
        now_ts,
        ok,
    )
    .await;
}

// Record the job outcome into the event table (kind=Job); failures also emit a JobFailed alert (escalated ERROR, no dedup).
async fn record_job_outcome(
    store: &Arc<HistoryStore>,
    alert_dedup_sec: u64,
    alert_url: String,
    job: &crate::config::JobConfig,
    detail: &str,
    ts: u64,
    ok: bool,
) {
    if let Err(e) = store.record_event(
        &job.name,
        "Job",
        detail,
        ts,
        "native",
        if ok { "info" } else { "error" },
    ) {
        tracing::warn!(job = %job.name, err = %e, "job event write fail (non-fatal)");
    }
    if ok {
        return;
    }
    let mut alerter = crate::alert::Alerter::new(alert_dedup_sec, alert_url)
        .with_event_sink(make_event_sink(store.clone()));
    if let Err(e) = alerter
        .alert(crate::alert::Alert {
            service: job.name.clone(),
            kind: crate::alert::AlertKind::JobFailed,
            detail: detail.to_string(),
            ts,
        })
        .await
    {
        tracing::warn!(job = %job.name, err = %e, "JobFailed alert emit fail (non-fatal)");
    }
}

// issue #113: backup runs detached — under the core lock only the runner is constructed (cheap),
// then the run is handed to tokio::spawn; BackupFailed is not deduped → an independent Alerter keeps state safe.
// The store Arc is shared (HistoryStore: Sync); event persistence goes through the same sink.
fn spawn_backup_task(
    mut runner: crate::backup::BackupRunner,
    store: Arc<HistoryStore>,
    alert_dedup_sec: u64,
    alert_url: String,
    now_ts: u64,
) {
    // #116: dump/qdrant/prune are all synchronously blocking (std::process + std::io::copy streaming
    // 2.2GB, minutes-long) — running them via tokio::spawn on a worker thread would pin the worker
    // and starve the runtime → all RPC black-holed (observed 2026-11-09: curl 000 throughout). Use a blocking
    // pool thread + embedded current_thread runtime for the async section (alert webhook reqwest /
    // store await), leaving tokio workers unblocked.
    tokio::task::spawn_blocking(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(err = %e, "backup embedded runtime build failed (backup not run)");
                return;
            }
        };
        rt.block_on(async move {
            let mut alerter = crate::alert::Alerter::new(alert_dedup_sec, alert_url)
                .with_event_sink(make_event_sink(store.clone()));
            match runner.run(now_ts, &store, &mut alerter).await {
                Ok(()) => tracing::info!("backup done (detached)"),
                Err(e) => tracing::error!(err = %e, "backup failed (fail-closed already alerted)"),
            }
        });
    });
}

// issue #37: Alerter event sink — every alert actually emitted (dedup passed) is written to the event table.
// compose alerts carry a "compose:" service prefix → the prefix is stripped on write + plane=compose
// (same semantics as the old explicit tick_compose writes, issue #7).
pub(crate) fn make_event_sink(store: Arc<HistoryStore>) -> crate::alert::EventSink {
    Arc::new(
        move |service: &str, kind: &str, detail: &str, ts: u64, _plane: &str, severity: &str| {
            let (svc, plane) = match service.strip_prefix("compose:") {
                Some(s) => (s, "compose"),
                None => (service, "native"),
            };
            if let Err(e) = store.record_event(svc, kind, detail, ts, plane, severity) {
                tracing::warn!(svc = svc, err = %e, "alert event write fail (non-fatal)");
            }
        },
    )
}

// PRD app-tiering: pin overrides load from the store (survive restart, acceptance §5.4). On failure degrade to an
// empty cache (a corrupt pin table must not kill the daemon; same philosophy as alert fallbacks, tracing warn leaves a trace).
fn load_boot_pins(store: &Arc<HistoryStore>) -> std::collections::HashMap<String, PinMode> {
    store
        .all_boot_pins()
        .unwrap_or_else(|e| {
            tracing::warn!(err = %e, "boot pins read failed, continuing with empty overrides");
            Vec::new()
        })
        .into_iter()
        .collect()
}

// Build the compose backend from config. enabled=false → None (native only).
// Missing files alert but still return None (does not block native startup; the operator creates the files per the hint).
fn build_compose(cfg: &Config) -> Option<Box<dyn ComposeBackend>> {
    if !cfg.compose.enabled {
        return None;
    }
    if !cfg.compose.sidecar_file.exists() {
        tracing::warn!(
            path = %cfg.compose.sidecar_file.display(),
            "compose.enabled=true but sidecar file missing, falling back to native-only"
        );
        return None;
    }
    Some(Box::new(DockerCompose::new(
        cfg.compose.sidecar_file.clone(),
        cfg.compose.business_file.clone(),
        cfg.compose.env_file.clone(),
    )))
}

// Real start invocation (production). svc = service name: start_map overrides (issue #29) take the override command,
// else the default is bash {repo}/start.sh {cmd}. stderr → per-service log.
// env_overrides: env to inject (issue #8 sidecar connections + issue #29 shell registration).
// issue #96: read the PID recorded in the service pidfile (missing/invalid → None) — restart/rollout
// report old→new PID replacement evidence.
fn read_pidfile_pid(svc: &str) -> Option<u32> {
    let s = std::fs::read_to_string(crate::start_map::pid_path(svc)).ok()?;
    s.trim().parse::<u32>().ok()
}

// issue #134: read-only query for the PID of the listener holding a port (lsof, best-effort). Only for attributing
// stop-verification failure messages — never used to kill (kill-safety #110: no pattern-kill path).
// lsof missing / port 0 / unregistered → None.
fn port_holder_pid(port: u16) -> Option<String> {
    if port == 0 {
        return None;
    }
    let out = std::process::Command::new("lsof")
        .args(["-ti", &format!("tcp:{port}"), "-sTCP:LISTEN"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let first = s.lines().map(str::trim).find(|l| !l.is_empty())?;
    (!first.is_empty()).then(|| first.to_string())
}

fn real_start_pair(svc: &str, cmd: &str, env_overrides: &[(String, String)]) -> Result<()> {
    match crate::start_map::start_spec_for(svc) {
        Some(spec) => exec_start_spec(svc, &spec, cmd, env_overrides),
        None => {
            let repo_dir = crate::manifest::name_to_dir(svc);
            let sh = probe_map::start_sh_path(repo_dir);
            let r = exec_bash_script(svc, &sh, cmd, env_overrides);
            // Anti-idle-sleep companion (gupiao transitional patch): after start/restart success attach
            // a caffeinate -i -w <pid> assertion, released automatically when the service exits
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

// caffeinate companion: reads the service pidfile, orphaned via `sh -c 'caffeinate -i -w <pid> & ...'` —
// sh exits immediately (status() reaps it to prevent zombies), caffeinate re-parents to launchd.
// pidfile missing / non-numeric content (start.sh has not written it yet) → WARN + skip, attach again on the next restart.
fn spawn_caffeinate(svc: &str, pidfile: &str) {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = pidfile.replace('~', &home);
    let pid = match std::fs::read_to_string(&path) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            tracing::warn!(svc, pidfile = %path, err = %e, "caffeinate companion skipped: pidfile unreadable");
            return;
        }
    };
    if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
        tracing::warn!(svc, pidfile = %path, "caffeinate companion skipped: pidfile content invalid");
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
            tracing::info!(svc, pid = %pid, "caffeinate assertion attached (anti idle sleep, released when the service exits)")
        }
        Err(e) => tracing::warn!(svc, err = %e, "caffeinate companion spawn failed"),
    }
}

// issue #51: child stderr goes straight to the per-service log file — no more pipe + tail thread.
// The old scheme (issue #47) joined on pipe EOF, but service processes backgrounded by start.sh /
// uvicorn workers inherit the pipe write end without closing it, so join blocks forever after child.wait() returns,
// with up_all holding the core Mutex starving all RPC/HTTP (production incident 2026-10-27, see issue #51).
// The failure tail now reads the last N lines from the log file after wait; the "exit code=<n>" format contract is unchanged.
const STDERR_TAIL_LINES: usize = 20;

// issue #121: before auto-restart, rotate the previous service log to <name>.log.1 (keep one generation).
// The old implementation truncated in place — when a process died between two spawns (ProcessExit, external TERM /
// internal should_exit), the cause-of-death evidence evaporated with the restart, leaving only blind debugging. A rotate failure does not block startup.
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

// Child stderr redirect target: the service log file (truncated open after rotation; if unopenable, degrade to discard).
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
            tracing::warn!(svc, err = %e, log = %log_path.display(), "open svc log fail, stderr discarded");
            std::process::Stdio::null()
        }
    }
}

// Read back the last N lines of the service log file (failure root-cause tail; returns an empty string if the file is missing or unreadable).
fn read_log_tail(log_path: &std::path::Path) -> String {
    let bytes = std::fs::read(log_path).unwrap_or_default();
    let content = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(STDERR_TAIL_LINES);
    lines[start..].join("\n")
}

// issue #60: service child processes get their own process group (unix). windows has no such API; children keep the default group.
#[cfg(unix)]
fn set_service_process_group(command: &mut std::process::Command) {
    command.process_group(0);
}
#[cfg(not(unix))]
fn set_service_process_group(_command: &mut std::process::Command) {}

// issue #29: start dispatch for override services.
// Script: bash <script> <cmd> (trainer-bridge ships its own start/stop).
// Daemon: start → nohup background + pidfile; stop → pidfile kill; restart upstream has degenerated to stop+start.
fn exec_start_spec(
    svc: &str,
    spec: &crate::start_map::StartSpec,
    cmd: &str,
    env_overrides: &[(String, String)],
) -> Result<()> {
    match spec {
        crate::start_map::StartSpec::Script(sh) => exec_bash_script(svc, sh, cmd, env_overrides),
        crate::start_map::StartSpec::Daemon { .. }
        | crate::start_map::StartSpec::Systemd { .. } => {
            // issue #145: the Systemd spec shares the same spawn/exit-code path as Daemon —
            // the only difference is command construction (systemctl returns synchronously, exiting only
            // after the unit has stopped, releasing the port; see the config comment for why the start.sh
            // pidfile path fails against systemd processes).
            let shell_cmd = match spec {
                crate::start_map::StartSpec::Daemon { .. } => match cmd {
                    "start" => crate::start_map::daemon_start_cmd(spec, svc).ok_or_else(|| {
                        anyhow::anyhow!("daemon spec missing start command: {svc}")
                    })?,
                    "stop" => crate::start_map::daemon_stop_cmd(
                        svc,
                        crate::manifest::registered_port(svc),
                    ),
                    other => {
                        anyhow::bail!(
                            "daemon spec does not support {other} (restart should be stop+start): {svc}"
                        );
                    }
                },
                crate::start_map::StartSpec::Systemd { unit } => {
                    crate::start_map::systemd_shell_cmd(unit, cmd)?
                }
                _ => unreachable!("outer match already excluded Script"),
            };
            tracing::info!(svc, cmd, shell = %shell_cmd, env_n = env_overrides.len(), "exec start_map daemon cmd");
            let mut command = std::process::Command::new("bash");
            command.arg("-c").arg(&shell_cmd);
            for (k, v) in env_overrides {
                command.env(k, v);
            }
            // issue #39/#40: strip env vars pinned by stale snapshots (e.g. fusion-mlx's FUSION_MLX_API_KEY),
            // letting the service's own start.sh/config resolve them live.
            for k in crate::env_map::strip_env_for(svc) {
                command.env_remove(k);
            }
            // issue #60: put the child in its own process group — when the daemon (launchd) reloads via bootout
            // signals are broadcast per process group; inheriting the daemon group would take down the whole managed fleet.
            set_service_process_group(&mut command);
            // issue #51: stderr to the log file, failure tail read back from the file (no pipe / no join,
            // backgrounded grandchildren holding the fd no longer block exec — see the open_svc_stderr comment).
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
                // Contract format "exit code=<n>" — runner.exit_code_of depends on it (changing one side requires syncing the other)
                match status.code() {
                    Some(c) => anyhow::bail!("daemon cmd {cmd} exit code={c}: {tail}"),
                    None => anyhow::bail!("daemon cmd {cmd} killed by signal: {tail}"),
                }
            }
            Ok(())
        }
    }
}

// bash <script> <cmd>, stderr → per-service log (~/.fusion-sv/logs/<svc>.log).
fn exec_bash_script(
    svc: &str,
    sh: &std::path::Path,
    cmd: &str,
    env_overrides: &[(String, String)],
) -> Result<()> {
    if !sh.exists() {
        anyhow::bail!("script not found: {}", sh.display());
    }
    std::fs::create_dir_all(crate::start_map::log_path(svc).parent().unwrap()).ok();
    let svc_log = crate::start_map::log_path(svc);
    tracing::info!(sh = %sh.display(), cmd, log = %svc_log.display(), env_n = env_overrides.len(), "exec start script");
    let mut command = std::process::Command::new("bash");
    command.arg(sh).arg(cmd);
    // Inject env (override/add), inheriting the parent's remaining env
    for (k, v) in env_overrides {
        command.env(k, v);
    }
    // issue #39/#40: strip env vars pinned by stale snapshots (e.g. fusion-mlx's FUSION_MLX_API_KEY),
    // letting the service's own start.sh resolve them live from settings.json, preventing a stale-key 401 storm after supervisor restarts.
    for k in crate::env_map::strip_env_for(svc) {
        command.env_remove(k);
    }
    // issue #60: put the child in its own process group — when the daemon (launchd) reloads via bootout
    // signals are broadcast per process group; inheriting the daemon group would take down the whole managed fleet.
    set_service_process_group(&mut command);
    // issue #51: stderr to the log file, failure tail read back from the file (no pipe / no join —
    // a backgrounded grandchild holding the pipe write end once hung exec permanently, see the open_svc_stderr comment).
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
        // Contract format "exit code=<n>" — runner.exit_code_of depends on it (changing one side requires syncing the other)
        match status.code() {
            Some(c) => anyhow::bail!("script {cmd} exit code={c}: {tail}"),
            None => anyhow::bail!("script {cmd} killed by signal: {tail}"),
        }
    }
    Ok(())
}

// Sync wrapper: block_on probe inside the tokio runtime. ServiceRunner.probe_fn is a sync closure.
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

    // env-injection tests share env_map::tests::env_lock (a single global lock, avoiding double-lock races).
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

    // issue #39/#40: exec_bash_script strips the env vars listed by strip_env_for —
    // the fusion-mlx child must not inherit the daemon's FUSION_MLX_API_KEY snapshot (preventing stale-key 401s),
    // other services inherit as usual.
    #[test]
    fn test_exec_bash_script_strips_pinned_env() {
        let _guard = env_lock();
        let prev = std::env::var("FUSION_MLX_API_KEY").ok();
        unsafe { std::env::set_var("FUSION_MLX_API_KEY", "stale-snapshot-key") };
        // Script: fails if the env var exists
        let dir = tempfile::tempdir().unwrap();
        let sh = dir.path().join("probe.sh");
        std::fs::write(
            &sh,
            "#!/bin/bash\nif [ -n \"$FUSION_MLX_API_KEY\" ]; then exit 1; fi\nexit 0\n",
        )
        .unwrap();
        // fusion-mlx: after the strip the script cannot see the var → Ok
        assert!(
            exec_bash_script("fusion-mlx", &sh, "start", &[]).is_ok(),
            "fusion-mlx child should have FUSION_MLX_API_KEY stripped"
        );
        // Other services: inherit the var → script fails
        assert!(
            exec_bash_script("fusion-rag", &sh, "start", &[]).is_err(),
            "other services inherit daemon env as usual"
        );
        // Restore the process env
        match prev {
            Some(v) => unsafe { std::env::set_var("FUSION_MLX_API_KEY", v) },
            None => unsafe { std::env::remove_var("FUSION_MLX_API_KEY") },
        }
    }

    // issue #47: exec_bash_script failure messages carry the exit code + stderr tail (root cause fed back to alerts/events)
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
        assert!(
            msg.contains("exit code=78"),
            "should carry the contract exit code: {msg}"
        );
        assert!(
            msg.contains("yaml: unmarshal errors: bad config"),
            "should carry the stderr tail: {msg}"
        );
        // The log file should also receive the same stderr (the tail thread writes it)
        let log = crate::start_map::log_path("ut-tail-capture");
        let log_content = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(log_content.contains("yaml: unmarshal errors: bad config"));
        // Clean up test artifact files
        let _ = std::fs::remove_file(&log);
    }

    // issue #121: service log rotation — keep the previous round's stderr in .log.1 before auto-restart,
    // so ProcessExit (external TERM / internal should_exit) cause-of-death evidence no longer evaporates on restart.
    #[test]
    fn test_issue121_rotate_svc_log_keeps_previous_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("ut-svc.log");
        std::fs::write(&log, "old run: Shutting down\n").unwrap();
        rotate_svc_log(&log);
        assert!(
            !log.exists(),
            "after rotate the main file yields to the new round"
        );
        let rotated = tmp.path().join("ut-svc.log.1");
        assert_eq!(
            std::fs::read_to_string(&rotated).unwrap(),
            "old run: Shutting down\n"
        );
        // Rotate again after a new round finishes: .1 is overwritten with the latest generation
        std::fs::write(&log, "new run\n").unwrap();
        rotate_svc_log(&log);
        assert_eq!(std::fs::read_to_string(&rotated).unwrap(), "new run\n");
        // Empty files are not rotated (skips a pointless rename)
        std::fs::write(&log, "").unwrap();
        rotate_svc_log(&log);
        assert!(log.exists(), "empty file is not rotated");
    }

    // issue #51 regression: a grandchild backgrounded by start.sh inherits stderr without closing it — exec must not wait for it.
    // The old pipe+join implementation would block until the sleep exits (60s); assert a return within <10s.
    #[test]
    fn test_exec_returns_despite_lingering_grandchild() {
        let dir = tempfile::tempdir().unwrap();
        let sh = dir.path().join("linger.sh");
        std::fs::write(&sh, "#!/bin/bash\nsleep 60 &\nexit 0\n").unwrap();
        let start = std::time::Instant::now();
        exec_bash_script("ut-linger", &sh, "start", &[])
            .expect("exec should return while the grandchild still holds stderr");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "must not wait for pipe EOF (the old implementation hung here, issue #51)"
        );
        // Clean up test artifact files (the grandchild sleep is covered by the cleanup policy outside tempdir: it exits naturally after 60s)
        let _ = std::fs::remove_file(crate::start_map::log_path("ut-linger"));
    }

    // caffeinate companion: a pidfile pointing at a live process → assertion attaches; process dies → caffeinate exits
    // (absolute path passed directly, no ~ expansion — avoids the HOME global env race)
    #[test]
    fn test_spawn_caffeinate_attaches_and_releases() {
        let dir = tempfile::tempdir().unwrap();
        // Companion target: a sleep process
        let mut target = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = target.id().to_string();
        let pidfile = dir.path().join("run.pid");
        std::fs::write(&pidfile, &pid).unwrap();
        spawn_caffeinate("ut-caffeinate", pidfile.to_str().unwrap());
        // The caffeinate process should exist and -w should point at the target pid
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
        assert!(
            attached,
            "caffeinate assertion should attach to the target pid"
        );
        // Target dies → caffeinate exits automatically (-w semantics)
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
        assert!(
            released,
            "caffeinate should exit automatically after the target process dies"
        );
    }

    // caffeinate companion: pidfile missing → skip without panicking
    #[test]
    fn test_spawn_caffeinate_skips_bad_pidfile() {
        spawn_caffeinate("ut-caffeinate", "/nonexistent-ut-xyz/run.pid");
    }

    // issue #145: Systemd spec exec — a systemctl shim verifies command pass-through +
    // non-zero exit codes surfacing (stop failures are no longer swallowed into fake stops).
    #[test]
    fn test_issue145_systemd_exec_start_stop_and_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let shim = tmp.path().join("systemctl");
        std::fs::write(
            &shim,
            "#!/bin/bash\necho \"$1 $2\" >> \"$FUSION_TEST_SHIM_LOG\"\nexit ${FUSION_TEST_SHIM_RC:-0}\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = tmp.path().join("shim.log");
        let _g1 = crate::env_map::tests::EnvGuard::set(
            "PATH",
            &format!(
                "{}:{}",
                tmp.path().display(),
                std::env::var("PATH").unwrap()
            ),
        );
        let _g2 =
            crate::env_map::tests::EnvGuard::set("FUSION_TEST_SHIM_LOG", log.to_str().unwrap());
        let spec = crate::start_map::StartSpec::Systemd {
            unit: "ut-svc.service".to_string(),
        };
        // start / stop both pass through to systemctl
        exec_start_spec("ut-svc", &spec, "start", &[]).unwrap();
        exec_start_spec("ut-svc", &spec, "stop", &[]).unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls, "start ut-svc.service\nstop ut-svc.service\n");
        // systemctl failure → Err carries the exit code (runner.exit_code_of contract format)
        let _g3 = crate::env_map::tests::EnvGuard::set("FUSION_TEST_SHIM_RC", "1");
        let err = exec_start_spec("ut-svc", &spec, "stop", &[]).unwrap_err();
        assert!(err.to_string().contains("exit code=1"), "{err}");
        // Unknown verbs are rejected
        let _g4 = crate::env_map::tests::EnvGuard::set("FUSION_TEST_SHIM_RC", "0");
        assert!(exec_start_spec("ut-svc", &spec, "restart", &[]).is_err());
        let _ = std::fs::remove_file(crate::start_map::log_path("ut-svc"));
    }

    // issue #60 regression: child processes started via exec must land in their own process group — launchd bootout
    // broadcasts SIGTERM per daemon process group; inheriting the same group would take down the whole managed fleet.
    // Assert: the bash started by exec_bash_script has pgid ≠ the test process pgid; a plain inheriting spawn has ==.
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
            .expect("child should write out its own pgid")
            .trim()
            .to_string();
        assert!(!child_pgid.is_empty(), "pgid output must not be empty");
        assert_ne!(
            child_pgid, our_pgid,
            "exec child should be in its own process group (issue #60), but shares the daemon's group"
        );
        // Clean up test artifact files
        let _ = std::fs::remove_file(crate::start_map::log_path("ut-pgid"));
    }

    // issue #31: up_one/down_one — service-level ops touch only the target service
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
            "up_one should start only the target service: {got:?}"
        );
        assert!(matches!(core.runners[1].state, runner::State::Starting));

        calls.lock().unwrap().clear();
        core.down_one("fusion-rag").await.unwrap();
        let got: Vec<String> = calls.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["fusion-rag/stop"],
            "down_one should stop only the target service: {got:?}"
        );
        assert!(matches!(core.runners[1].state, runner::State::Stopped));

        // Unknown service → Err
        assert!(core.up_one("fusion-nope").await.is_err());
        assert!(core.down_one("fusion-nope").await.is_err());
    }

    // issue #31: up_one skips already-healthy (consistent with up_native's double-start avoidance)
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
        assert!(
            calls.lock().unwrap().is_empty(),
            "already healthy should skip start"
        );
        assert!(matches!(core.runners[0].state, runner::State::Healthy));
    }

    // issue #31: supervise: false entries get no runner — up/down/tick/status all skip them
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
        assert_eq!(
            core.runners.len(),
            1,
            "exempt entries must not create runners"
        );
        assert_eq!(core.runners[0].def.name, "fusion-mlx");
        let status = core.status_all();
        assert_eq!(status.len(), 1, "status must not include exempt entries");
    }

    #[tokio::test]
    async fn test_up_all_skips_already_healthy() {
        // All services healthy on probe → start.sh is not called
        let defs = vec![
            def("fusion-mlx", Layer::Core, 11434),
            def("fusion-rag", Layer::App, 11436),
        ];
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_p = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 }, // probe always healthy
            move |svc, cmd, _env| {
                calls_p.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        core.up_all().await.unwrap();
        assert!(
            calls.lock().unwrap().is_empty(),
            "already healthy should skip and not call start.sh"
        );
        // I4: runners skipped as already-healthy should be marked Healthy
        for r in &core.runners {
            assert!(
                matches!(r.state, runner::State::Healthy),
                "should be Healthy after skip"
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
        // All unhealthy → start in layer order, recording the order
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
        assert_eq!(o[0], "fusion-mlx", "core layer starts first");
        assert_eq!(o[1], "fusion-rag", "app layer starts after");
        // I4: runners that were started should be marked Starting
        for r in &core.runners {
            assert!(
                matches!(r.state, runner::State::Starting),
                "should be Starting after start"
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
        assert_eq!(o[0], "fusion-rag", "reverse order: app stops first");
        assert_eq!(o[1], "fusion-mlx", "core stops last");
        // I5: runners stopped by down_all should be marked Stopped
        for r in &core.runners {
            assert!(
                matches!(r.state, runner::State::Stopped),
                "should be Stopped after stop"
            );
        }
    }

    #[tokio::test]
    async fn test_concurrency_cap_not_exceeded() {
        // 4 services in one layer, cap=2 → at most 2 concurrent starts at any moment
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
        assert!(mx <= 2, "concurrency cap 2, observed peak {mx}");
    }

    // C1: tick_all drives the state machine — Unhealthy+ConnectionRefused → record crash + restart →
    // flip the probe to Healthy → tick → back to Healthy. Proves tick_all completes the supervision loop.
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
        // Start: already Unhealthy. issue #31: backoff no longer sleeps under the lock —
        // the first tick schedules, the due tick (delay=1s) restarts → Restarting
        core.runners[0].state = runner::State::Unhealthy;
        // issue #79: preset the consecutive-failure count (testing straight from Unhealthy)
        core.runners[0].fail_streak = 2;
        core.drive(1000).await;
        assert!(
            matches!(core.runners[0].state, runner::State::Unhealthy),
            "scheduled tick must not restart"
        );
        core.drive(1001).await;
        assert!(
            matches!(core.runners[0].state, runner::State::Restarting),
            "should be Restarting after tick_all"
        );
        // Flip the probe to healthy → tick_all should recover to Healthy
        *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 3 };
        core.drive(1001).await;
        assert!(
            matches!(core.runners[0].state, runner::State::Healthy),
            "tick_all should be Healthy after the probe recovers"
        );
    }

    // issue #153: Timeout uses a higher restart threshold — 3 consecutive timeouts do not schedule a restart
    // (healthy engines were being killed during high-load GIL saturation); only reaching TIMEOUT_RESTART_STREAK escalates.
    #[tokio::test]
    async fn test_issue153_timeout_streak_gates_restart() {
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::Timeout,
            },
            |_svc, _cmd, _env| Ok(()),
        );
        // Use a fake repo_dir — if the real machine happens to have a start.sh with the same name running
        // (the #86 operator-race gate's pgrep -f match), the due-restart would be skipped and the test
        // becomes an environment-sensitive false failure
        core.runners[0].def.repo_dir = "test-no-such-repo-153".into();
        core.runners[0].state = runner::State::Unhealthy;
        core.runners[0].fail_streak = 2;
        // streak 3..=TIMEOUT_RESTART_STREAK-1: consecutive timeouts far beyond the old 3-streak threshold, no restart scheduled
        for _ in 3..runner::TIMEOUT_RESTART_STREAK {
            core.drive(2000).await;
            assert!(
                core.runners[0].restart_after.is_none(),
                "Timeout streak below {} must not schedule a restart",
                runner::TIMEOUT_RESTART_STREAK
            );
            assert!(matches!(core.runners[0].state, runner::State::Unhealthy));
        }
        // Threshold reached → schedule; due tick → actual restart
        core.drive(2000).await;
        assert!(
            core.runners[0].restart_after.is_some(),
            "Timeout reaching the threshold should schedule a restart"
        );
        core.drive(9_999_999).await;
        assert!(matches!(core.runners[0].state, runner::State::Restarting));
    }

    // issue #153: ConnectionRefused still uses the RESTART_FAIL_STREAK=3 fast restart
    // (the true-death signal is not slowed by the Timeout triage).
    #[tokio::test]
    async fn test_issue153_refused_keeps_fast_restart() {
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-rag", Layer::App, 11436)],
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            |_svc, _cmd, _env| Ok(()),
        );
        // Use a fake repo_dir — if the real machine happens to have a start.sh with the same name running
        // (the #86 operator-race gate's pgrep -f match), the due-restart would be skipped and the test
        // becomes an environment-sensitive false failure
        core.runners[0].def.repo_dir = "test-no-such-repo-153".into();
        core.runners[0].state = runner::State::Unhealthy;
        core.runners[0].fail_streak = 2;
        core.drive(1000).await;
        assert!(
            core.runners[0].restart_after.is_some(),
            "ConnectionRefused 3-streak should schedule a restart"
        );
        core.drive(999_999).await;
        assert!(matches!(core.runners[0].state, runner::State::Restarting));
    }

    // issue #153: during start_retry, Timeout does not escalate the threshold — a process not yet
    // listening is expected; keep the 3-streak fast relaunch, otherwise start-phase retries slow down by an order of magnitude.
    #[tokio::test]
    async fn test_issue153_timeout_during_start_retry_keeps_fast_threshold() {
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::Timeout,
            },
            |_svc, _cmd, _env| Ok(()),
        );
        // Use a fake repo_dir — if the real machine happens to have a start.sh with the same name running
        // (the #86 operator-race gate's pgrep -f match), the due-restart would be skipped and the test
        // becomes an environment-sensitive false failure
        core.runners[0].def.repo_dir = "test-no-such-repo-153".into();
        core.runners[0].state = runner::State::Unhealthy;
        core.runners[0].fail_streak = 2;
        core.runners[0].start_retry = true;
        core.drive(1000).await;
        assert!(
            core.runners[0].restart_after.is_some(),
            "Timeout during start_retry should keep the fast threshold and schedule a restart"
        );
    }

    // issue #112: after a successful auto-recovery start, an AutoRestart audit event must be recorded (with old/new pid),
    // and status_all's last_restart column must find it — afterwards answers "who started this service"
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
        // Use a fake repo_dir — if the real machine happens to have a start.sh with the same name running
        // (the #86 operator-race gate's pgrep -f match), the due-restart would be skipped and the test
        // becomes an environment-sensitive false failure
        core.runners[0].def.repo_dir = "test-no-such-repo-153".into();
        core.runners[0].state = runner::State::Unhealthy;
        core.runners[0].fail_streak = 2;
        core.drive(1000).await;
        core.drive(1001).await; // due restart, invoke_start Ok → AutoRestart event
        // Event persisted: kind=AutoRestart, detail carries pid fields (tests have no pidfile → "?")
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
            .expect("AutoRestart event should be persisted");
        assert!(
            detail.contains("old_pid="),
            "detail should carry old_pid: {detail}"
        );
        assert!(
            detail.contains("new_pid="),
            "detail should carry new_pid: {detail}"
        );
        // Flip the probe to healthy → recovery
        *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 3 };
        core.drive(1001).await;
        assert!(matches!(core.runners[0].state, runner::State::Healthy));
        // status enrichment: last_restart column = "AutoRestart ..."
        let status = core.status_all();
        let row = status.iter().find(|e| e.name == "fusion-rag").unwrap();
        let lr = row
            .last_restart
            .as_deref()
            .expect("last_restart should have a value");
        assert!(lr.starts_with("AutoRestart"), "got: {lr}");
        let _ = kind;
    }

    // issue #111: probe passes but the pidfile process is dead (a stale listener holds the port) — Daemon spec
    // services must flip to Unhealthy (entering the restart flow to clear the stale listener); live pid / no pidfile are not misjudged
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
                // SAFETY: env_lock serializes; no code under test reads HOME concurrently during this test
                unsafe { std::env::set_var("HOME", val) };
                Self { prev }
            }
        }
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                // SAFETY: same as above
                match &self.prev {
                    Some(v) => unsafe { std::env::set_var("HOME", v) },
                    None => unsafe { std::env::remove_var("HOME") },
                }
            }
        }
        let _g0 = env_lock();
        let home = std::env::temp_dir().join(format!("fusion-sv-111-home-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".fusion-sv/pids")).unwrap();
        let _g1 = HomeGuard::set(&home);
        let pidfile = home.join(".fusion-sv/pids/fusion-cowork-web.pid");

        // Scenario 1: dead pid — i32::MAX never exists (macOS pid cap ~99998), avoiding the race where a
        // parallel test kills a child process and the pid is instantly reused
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
            "probe passing with a dead pid should flip to Unhealthy"
        );
        drop(core);

        // Scenario 2: live pid → stays Healthy
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

        // Scenario 3: pidfile missing → no misjudgment, stays Healthy
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

    // issue #112: boot reconciliation finds a resident-tier (non-T2) service dead — records a ServiceDown event
    // "found dead at daemon start", visible in the alert stream afterwards, no longer silently staying Stopped
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
        // T1 death has an event, T2 (non-resident tier) does not
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
        assert_eq!(
            n, 1,
            "only a resident-tier (T1) death records a reconcile event"
        );
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

    // issue #157: for a resident-tier service the operator downed (the event table's most recent restart-class
    // event = ManualStop), reconcile after a daemon restart no longer emits the "found dead" alert — deliberately
    // stopped does not count as dead; services without a ManualStop record still alert (#111 semantics unchanged).
    #[tokio::test]
    async fn test_issue157_reconcile_skips_operator_stopped_alert() {
        let defs = vec![
            def_tier("fusion-rag", Layer::App, 11436, BootTier::T1),
            def_tier("fusion-model-hub", Layer::Platform, 11444, BootTier::T1),
        ];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_: &crate::probe::ProbeSpec| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            |_svc, _cmd, _env| Ok(()),
        );
        // fusion-rag: downed by the operator (ManualStop event); fusion-model-hub: no record
        core.store
            .record_event(
                "fusion-rag",
                "ManualStop",
                "cli pid=1 argv: fusion-sv down fusion-rag",
                100,
                "native",
                "warn",
            )
            .unwrap();
        core.reconcile_on_boot().await;
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
        assert_eq!(
            svc, "fusion-model-hub",
            "only a resident-tier service not operator-stopped (no ManualStop record) emits a reconcile alert"
        );
    }

    // issue #157/#149: if an operator-downed service left a live listener behind and the daemon restarts,
    // reconcile must not adopt it back to Healthy — stays Stopped (#149 down-sticky across restarts).
    #[tokio::test]
    async fn test_issue157_reconcile_keeps_operator_stopped_service_down() {
        let defs = vec![def_tier(
            "fusion-cowork-web",
            Layer::App,
            11461,
            BootTier::T1,
        )];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_: &crate::probe::ProbeSpec| ProbeResult::Healthy { latency_ms: 1 },
            |_svc, _cmd, _env| Ok(()),
        );
        core.store
            .record_event(
                "fusion-cowork-web",
                "ManualStop",
                "cli pid=1 argv: fusion-sv down fusion-cowork-web",
                100,
                "native",
                "warn",
            )
            .unwrap();
        let adopted = core.reconcile_on_boot().await;
        assert_eq!(
            adopted, 0,
            "leftover listener of an operator-downed service must not be adopted"
        );
        assert!(matches!(core.runners[0].state, runner::State::Stopped));
    }

    // C1: poll_interval — returns poll_unhealthy_sec when anything is down, else poll_healthy_sec
    #[test]
    fn test_poll_interval_switches_on_down_state() {
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-rag", Layer::App, 11436)],
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        // All Healthy → poll_healthy_sec
        core.runners[0].state = runner::State::Healthy;
        assert_eq!(core.poll_interval(), core.cfg.poll_healthy_sec);
        // Any Unhealthy → poll_unhealthy_sec
        core.runners[0].state = runner::State::Unhealthy;
        assert_eq!(core.poll_interval(), core.cfg.poll_unhealthy_sec);
        // Failed also counts as down
        core.runners[0].state = runner::State::Failed;
        assert_eq!(core.poll_interval(), core.cfg.poll_unhealthy_sec);
    }

    // ---- compose plane integration tests ----
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

    // up_all: with compose enabled the order is sidecar → native → business
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
        // rag depends on qdrant (dep_map); preset qdrant Healthy to satisfy the dep-gate
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
        // Order: up_sidecar first, up_business last
        assert_eq!(calls[0], "up_sidecar", "sidecar starts first");
        assert_eq!(
            calls[calls.len() - 1],
            "up_business",
            "business starts last"
        );
        // native in the middle: core before platform
        let no = native_order.lock().unwrap();
        let mlx_i = no.iter().position(|n| n == "fusion-mlx").unwrap();
        let rag_i = no.iter().position(|n| n == "fusion-rag").unwrap();
        assert!(mlx_i < rag_i, "native core before platform");
        // sidecar before native
        let sidecar_i = calls.iter().position(|c| c == "up_sidecar").unwrap();
        let business_i = calls.iter().position(|c| c == "up_business").unwrap();
        assert!(sidecar_i < business_i);
    }

    // compose disabled (None): up_all calls no compose methods, native only
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
        assert!(
            !core.compose_enabled(),
            "compose should be disabled by default"
        );
        core.up_all().await.unwrap();
        let c = called.lock().unwrap();
        assert!(
            c.iter().all(|x| x.contains("start")),
            "only native start calls"
        );
    }

    // down_all: with compose enabled, compose down first, then native in reverse order
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
        assert_eq!(calls[0], "down", "compose down first");
        // native reverse order: platform stops first, core stops after
        let no = native_order.lock().unwrap();
        let rag_i = no.iter().position(|n| n == "fusion-rag").unwrap();
        let mlx_i = no.iter().position(|n| n == "fusion-mlx").unwrap();
        assert!(
            rag_i < mlx_i,
            "native reverse order: platform stops first, core stops after"
        );
    }

    // status_all: with compose enabled, native + container rows are merged
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
        // 1 native + 2 compose rows
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

    // tick_all: a compose container exited → ServiceDown alert
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
        // Alerts go via tracing WARN + optional webhook; here verify no panic + the status table shows Failed
        let st = core.status_all();
        let redis = st.iter().find(|e| e.name == "redis").unwrap();
        assert_eq!(
            redis.state, "Failed",
            "an exited container should be marked Failed"
        );
    }

    // poll_interval: a compose container Failed → poll_unhealthy_sec
    #[tokio::test]
    async fn test_poll_interval_compose_failed() {
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
        // poll_interval reuses last round's tick_compose ps snapshot (no live re-query)
        core.tick_compose(1000).await;
        assert!(core.compose_any_failed);
        // native all Healthy but a compose container Failed → poll_unhealthy_sec
        assert_eq!(core.poll_interval(), core.cfg.poll_unhealthy_sec);
    }

    // compose_logs: compose services go through compose logs, native services return None
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
        // compose service → Some
        assert!(core.compose_logs("postgres").is_some());
        // native service → None (goes through the native log path)
        assert!(core.compose_logs("fusion-mlx").is_none());
        // compose disabled → None
        let core2 = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        assert!(core2.compose_logs("postgres").is_none());
    }

    // ---- dep-gate (issue #5) integration tests ----

    // dep_gate: compose enabled and the dependency container Healthy → the native service starts normally
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
            def("fusion-rag", Layer::Platform, 11436), // depends on qdrant
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
            "rag should start when qdrant is Healthy"
        );
    }

    // dep_gate: dependency container not ready → skip starting that native service (mark Stopped, not Failed)
    #[tokio::test]
    async fn test_up_native_gate_skips_when_dep_unhealthy() {
        let defs = vec![
            def("fusion-mlx", Layer::Core, 11434),
            def("fusion-rag", Layer::Platform, 11436), // depends on qdrant
        ];
        let native_started: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let ns_p = native_started.clone();
        let fake = Box::new(FakeCompose::new());
        // qdrant exited (not Healthy) → gate not ready
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
            "rag should be gate-skipped when qdrant is not Healthy, not started"
        );
        // The rag runner should be marked Stopped (not Failed; a dependency problem is not a crash)
        let rag = core
            .runners
            .iter()
            .find(|r| r.def.name == "fusion-rag")
            .unwrap();
        assert!(
            matches!(rag.state, runner::State::Stopped),
            "gate skip should be marked Stopped, got: {:?}",
            rag.state
        );
        // mlx has no deps → starts as usual
        assert!(
            started.contains(&"fusion-mlx".to_string()),
            "services without deps are unaffected by the gate"
        );
    }

    // dep_gate: compose disabled → no gate, all native services start normally
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
            def("fusion-rag", Layer::Platform, 11436), // would be blocked if a gate existed (no qdrant container)
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
            "with compose disabled there is no dep-gate, rag should start normally"
        );
    }

    // ---- compose container alert throttling (issue #6) integration tests ----
    // 4 scenarios driven through tick_compose → compose_tracker; verify snapshots/decisions do not panic and state is correct.

    // The same container exited for 5 consecutive ticks (inside the dedup window) → fail_count accumulates, only the first tick sets last_alert
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
        // 5 ticks, inside the dedup window (default alert_dedup_sec=300, escalation=5)
        for ts in [1000u64, 1002, 1004, 1006, 1008] {
            core.drive(ts).await;
        }
        let rec = core.compose_tracker.record("redis").unwrap();
        assert_eq!(
            rec.fail_count, 5,
            "5 consecutive failed ticks should accumulate to 5"
        );
        assert_eq!(rec.state, "Failed");
    }

    // docker state change (restarting→exited) → the tracker records the raw_state change
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
        // Switch to exited (via Arc<Mutex> interior mutability, no core.compose borrow needed)
        *containers.lock().unwrap() = vec![mk_container("redis", "exited", "", 6379)];
        core.drive(1002).await;
        assert_eq!(
            core.compose_tracker.record("redis").unwrap().raw_state,
            "exited",
            "state change should update the snapshot"
        );
    }

    // failure→healthy → the tracker resets fail_count and clears escalated
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
        // Recovery (via Arc<Mutex> interior mutability)
        *containers.lock().unwrap() = vec![mk_container("redis", "running", "healthy", 6379)];
        core.drive(1010).await;
        let rec = core.compose_tracker.record("redis").unwrap();
        assert_eq!(rec.state, "Healthy", "recovery should flip to Healthy");
        assert_eq!(rec.fail_count, 0, "recovery should reset fail_count");
    }

    // consecutive failures reach the threshold → escalated=true (CrashLoop escalation)
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
        // escalation=5 (fake_cfg default), run 5 ticks
        for ts in [1000u64, 1002, 1004, 1006, 1008] {
            core.drive(ts).await;
        }
        let rec = core.compose_tracker.record("redis").unwrap();
        assert!(
            rec.escalated,
            "reaching the threshold should set the escalation flag"
        );
        assert_eq!(rec.fail_count, 5);
    }

    // ---- compose containers persisted into history.db (issue #7) ----

    // issue #37: native alerts persist via the Alerter event sink — kind/plane/severity correct,
    // alerts suppressed inside the dedup window are not re-persisted, and alerts_emitted counts only real emissions.
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
        assert_eq!(ev.len(), 1, "the first ServiceDown should be persisted");
        assert_eq!(ev[0].kind, "ServiceDown");
        assert_eq!(ev[0].plane, "native");
        assert_eq!(ev[0].severity, "warn");
        assert_eq!(core.alerter.alerts_emitted(), 1);
        // Second ServiceDown inside the dedup window: not persisted, not counted (the tick only schedules a restart, no new alert)
        core.drive(1001).await;
        let ev2 = core.store.recent_events("fusion-mlx", 10).unwrap();
        assert_eq!(ev2.len(), 1, "no new alert should not add an event row");
        assert_eq!(core.alerter.alerts_emitted(), 1);
    }

    // issue #37: retention sweep schedule — on due, prunes expired rows and counts; before due, does nothing.
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
        assert!(
            core.check_retention(now),
            "last_prune_ts=0 → due on the first check"
        );
        assert_eq!(
            core.events_pruned_total, 2,
            "old health row + old event row"
        );
        assert_eq!(
            core.store.recent_health("fusion-mlx", 10).unwrap().len(),
            1,
            "recent rows are kept"
        );
        // Inside the schedule interval, not due → does not run again
        assert!(!core.check_retention(now + 60));
        assert_eq!(core.events_pruned_total, 2);
    }

    // Container exited → tick_compose should write an event row with plane=compose, kind=ServiceDown
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
        assert!(
            !ev.is_empty(),
            "an exited container should write an event row"
        );
        assert_eq!(ev[0].kind, "ServiceDown");
        assert_eq!(ev[0].plane, "compose");
        // A health row should also exist with plane=compose
        let h = core.store.recent_health("redis", 10).unwrap();
        assert!(!h.is_empty(), "a compose health row should be written");
        assert_eq!(h[0].plane, "compose");
    }

    // Container exited→running → tick_compose should write a Back event with plane=compose
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
        // Recovery
        *containers.lock().unwrap() = vec![mk_container("redis", "running", "healthy", 6379)];
        core.drive(1010).await;
        let ev = core.store.recent_events("redis", 10).unwrap();
        let has_back = ev
            .iter()
            .any(|e| e.kind == "ServiceBack" && e.plane == "compose");
        assert!(
            has_back,
            "recovery should write a ServiceBack event with plane=compose"
        );
    }

    // issue #78: even with compose disabled (single-node dev), sidecar connection env must be injected —
    // the gate previously sat on compose.enabled, so with the default off, cowork silently ran SQLite.
    // .env has FUSION_PG_PASSWORD → identity receives DATABASE_URL.
    // env_lock across await: the test body is short, only serializing process env reads/writes, not starving the executor.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_up_native_injects_env_when_compose_disabled() {
        // Pin FUSION_CLIENT_KEY: issue #85 inverts client-key injection onto every
        // managed service, so the up path resolves it fail-closed. CI runners have
        // no ~/fusion/fusion-gateway/config.yaml fallback (local dev does), which
        // made these tests environment-dependent. Same fix as the trainer-bridge
        // tests in env_map.rs.
        let _ck = crate::env_map::tests::EnvGuard::set("FUSION_CLIENT_KEY", "ck-up-test");
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        let env_f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(env_f.path(), "FUSION_PG_PASSWORD=pw78\n").unwrap();
        let mut cfg = fake_cfg(); // compose.enabled=false (default)
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
        assert!(!core.compose_enabled(), "compose should not be enabled");
        core.up_all().await.unwrap();
        let env = received_env.lock().unwrap();
        let db_url = env
            .iter()
            .find(|(k, _)| k == "DATABASE_URL")
            .expect("DATABASE_URL should be injected even with compose off (issue #78)");
        assert!(
            db_url.1.contains(":pw78@localhost:5432/"),
            "got {}",
            db_url.1
        );
        assert!(
            matches!(core.runners[0].state, runner::State::Starting),
            "a successfully injected service should be Starting"
        );
    }

    // issue #125: per-service env overlay (`~/.fusion-sv/env/<svc>.env`) injected on the up path.
    // HOME is redirected to a temp dir; the overlay file carries FUSION_LLM_TIMEOUT → start_fn receives it.
    // env_lock across await: the test body is short, only serializing process env reads/writes, not starving the executor.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_issue125_overlay_env_reaches_start_fn() {
        let _ck = crate::env_map::tests::EnvGuard::set("FUSION_CLIENT_KEY", "ck-ovl-test");
        let _g = env_lock();
        let home = tempfile::tempdir().unwrap();
        let ovl_dir = home.path().join(".fusion-sv/env");
        std::fs::create_dir_all(&ovl_dir).unwrap();
        std::fs::write(
            ovl_dir.join("fusion-identity.env"),
            "FUSION_LLM_TIMEOUT=120\n",
        )
        .unwrap();
        let _home = crate::env_map::tests::EnvGuard::set("HOME", home.path().to_str().unwrap());
        let mut cfg = fake_cfg();
        cfg.compose.env_file = std::env::temp_dir().join("fusion-sv-no-such-env-file");
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
        core.up_all().await.unwrap();
        let env = received_env.lock().unwrap();
        assert!(
            env.iter()
                .any(|(k, v)| k == "FUSION_LLM_TIMEOUT" && v == "120"),
            "overlay env should reach start_fn, got {:?}",
            *env
        );
    }

    // issue #8: compose enabled + a service depending on sidecars + .env missing FUSION_PG_PASSWORD
    // → fail-closed: refuse to start, do not call start.sh, mark the runner Failed.
    // env_lock across await: the test body is short, only serializing process env reads/writes, not starving the executor.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn test_up_native_fail_closed_when_pg_password_missing() {
        let _g = env_lock();
        unsafe { std::env::remove_var("FUSION_PG_PASSWORD") };
        // .env has only USER, no PASSWORD → DATABASE_URL cannot be constructed
        let env_f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(env_f.path(), "FUSION_PG_USER=fusion\n").unwrap();
        let mut cfg = fake_cfg();
        cfg.compose.enabled = true;
        cfg.compose.env_file = env_f.path().to_path_buf();
        let defs = vec![def("fusion-identity", Layer::App, 11450)];
        let started: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let st_p = started.clone();
        let fake = Box::new(FakeCompose::new());
        // dep-gate (#5): fusion-identity depends on postgres, which must be Healthy to pass the gate;
        // env injection (the fail-closed path) happens only after passing. Without this container the
        // dep-gate skips first → Stopped.
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
        assert!(core.compose_enabled(), "compose should be enabled");
        core.up_all().await.unwrap();
        assert!(
            !*started.lock().unwrap(),
            "fail-closed should refuse to call start.sh"
        );
        assert!(
            matches!(core.runners[0].state, runner::State::Failed),
            "env injection failure should be marked Failed (no silent degradation)"
        );
    }

    // ---- issue #90: start-failure backoff retry ----

    // A start failure (non exit-78) should schedule a retry instead of the terminal Failed state, without consuming a crash-window slot
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
            "a start failure should be Unhealthy awaiting retry, not terminal Failed"
        );
        assert!(r.restart_after.is_some(), "a retry should be scheduled");
        assert!(r.last_start_ts.is_none(), "never successfully started");
        assert_eq!(
            r.policy.crash_count(),
            0,
            "does not consume a crash-window slot"
        );
    }

    // exit-78 config-fatal: retry cannot help, straight to Failed
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
            "exit 78 should be terminal Failed"
        );
        assert!(
            r.restart_after.is_none(),
            "config-fatal does not schedule retries"
        );
    }

    // Retry cap: start_fail_streak reaching START_FAIL_CAP → Failed + RestartCapHit alert
    // repo_dir uses a scaffold (same #86 test convention) — a real "fusion-mlx" would collide with a
    // start.sh process running on this machine (pgrep stage-1 hit), the operator-race gate would wrongly
    // skip the restart and the test would flake with the environment
    #[tokio::test]
    async fn test_issue90_start_fail_cap_marks_failed() {
        let mut d = def("fusion-mlx", Layer::Core, 11434);
        d.repo_dir = "scaffold-issue90-cap".into();
        let defs = vec![d];
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
        assert!(
            matches!(r.state, runner::State::Failed),
            "reaching the cap should be Failed"
        );
        assert_eq!(r.start_fail_streak, runner::START_FAIL_CAP);
        let ev = core.store.recent_events("fusion-mlx", 10).unwrap();
        assert!(
            ev.iter()
                .any(|e| e.kind == "RestartCapHit" && e.detail.contains("start-fail cap")),
            "a start-fail cap event should be recorded"
        );
    }

    // A manual down should clear the pending retry schedule and counters
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
        assert!(r.restart_after.is_none(), "down should clear the schedule");
        assert_eq!(r.start_fail_streak, 0, "down should clear the counter");
    }

    // ---- issue #9: drain / drain_all / rollout ----

    // drain a single service: Healthy → Draining
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
        assert_eq!(core.runners[0].drain_grace_until, Some(1030)); // grace=30 default
    }

    // drain an unknown service → Err
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

    // drain_all: all Healthy → all Draining
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

    // rollout full cycle: drain→stop→start→prewarm(Healthy)→Healthy
    // issue #96: probe sequence = Healthy(before stop) → Unhealthy(stop verified) →
    // Healthy(prewarm); start_fn receives both stop and start verbs
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
            "should be Healthy after a successful rollout"
        );
        assert!(core.runners[0].drain_grace_until.is_none());
        // issue #96: rollout must stop the old process before starting
        let got = calls.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["fusion-mlx/stop", "fusion-mlx/restart"],
            "rollout should really stop + invoke_start"
        );
    }

    // rollout prewarm failure: probes always unhealthy → timeout → Err + stays Stopped + alert
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
        // prewarm_timeout=3 → times out quickly
        let res = core.rollout("fusion-mlx", 1000, 5, 3).await;
        assert!(res.is_err(), "prewarm timeout should return Err");
        assert!(
            matches!(core.runners[0].state, runner::State::Stopped),
            "prewarm failure should stay Stopped (no rollback)"
        );
    }

    // ===== issue #96: restart/rollout must really replace the process =====

    // restart_one: stop → port released (probe flips unhealthy) → start, verb order stop+start
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
        assert_eq!(old_pid, None, "no pidfile in the test environment");
        assert_eq!(new_pid, None);
        let got = calls.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["fusion-mlx/stop", "fusion-mlx/restart"],
            "restart should really stop + invoke_start"
        );
        assert!(matches!(core.runners[0].state, runner::State::Starting));
    }

    // issue #121: a manual restart must clear the crash-path-scheduled restart_after (single-flight invariant) —
    // otherwise the due tick spawns another instance and the port-race loser dies instantly (observed cowork-web 40281→42476).
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
            "restart must clear the scheduled crash restart (single-flight)"
        );
    }

    // restart_one: stop ineffective (port stays Healthy) → re-issue stop ×2 inside the window (issue #134);
    // still held → bail, never start, never return ok
    #[tokio::test(start_paused = true)]
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
        assert!(res.is_err(), "an ineffective stop should bail");
        let got = calls.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["fusion-mlx/stop", "fusion-mlx/stop", "fusion-mlx/stop"],
            "old process not dead: first stop + 2 re-issues inside the window, no start (prevents double-start / fake replacement)"
        );
        assert!(matches!(core.runners[0].state, runner::State::Stopped));
    }

    // issue #134: a script-type stop is slower than the first verification — after the re-issued stop releases the port,
    // the restart completes normally (tolerant-window semantics)
    #[tokio::test(start_paused = true)]
    async fn test_issue134_stop_reissue_recovers() {
        // i=0..8 probes Healthy (9 times), stop re-issued at i=8, port released from i=9
        let probe_n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pn = probe_n.clone();
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cp = calls.clone();
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            fake_cfg(),
            move |_: &ProbeSpec| {
                let n = pn.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n < 9 {
                    ProbeResult::Healthy { latency_ms: 1 }
                } else {
                    ProbeResult::Unhealthy {
                        reason: crate::probe::UnhealthyReason::ConnectionRefused,
                    }
                }
            },
            move |svc, cmd, _env| {
                cp.lock().unwrap().push(format!("{svc}/{cmd}"));
                Ok(())
            },
        );
        core.runners[0].state = runner::State::Healthy;
        core.restart_one("fusion-mlx").await.unwrap();
        let got = calls.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["fusion-mlx/stop", "fusion-mlx/stop", "fusion-mlx/restart"],
            "after the re-issued stop releases the port → the new process starts normally"
        );
    }

    // issue #134: port 0 (unregistered / probe has no port) skips the lsof query; the port-holder PID query returns None
    #[test]
    fn test_issue134_port_holder_pid_zero_port() {
        assert_eq!(port_holder_pid(0), None);
    }

    // restart_one: unknown service bails
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

    // ===== PRD app-tiering (issue #62) boot API unit tests =====

    // up_boot_set starts only T0 + pin=auto — T1/T2 without a pin do not participate
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
        // pin fusion-rag to auto — should join the boot set
        core.set_boot_pin("fusion-rag", Some(PinMode::Auto))
            .unwrap();
        core.up_boot_set().await.unwrap();
        let got: Vec<String> = calls.lock().unwrap().clone();
        // Order: layer order (fixed/layer) — mlx, gateway (Core), rag (Platform)
        assert_eq!(
            got,
            vec![
                "fusion-mlx/start",
                "fusion-gateway/start",
                "fusion-rag/start"
            ]
        );
        // health (T2, no pin) does not participate
        assert!(!got.iter().any(|s| s.starts_with("fusion-health")));
    }

    // boot_group T1 up: partial-failure summary is correct
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
        assert_eq!(res["started"], 1, "rag started");
        assert_eq!(
            res["failed"], 0,
            "issue #90: start failures no longer go straight to Failed"
        );
        assert_eq!(
            res["retrying"], 1,
            "cowork start failure should schedule a retry"
        );
        assert_eq!(res["group"], "T1");
        assert_eq!(res["action"], "up");
    }

    // pin overrides the registry: a T0 unpinned to manual no longer joins the boot set
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
        // T0 unpinned to manual — no longer joins the boot set
        core.set_boot_pin("fusion-mlx", Some(PinMode::Manual))
            .unwrap();
        assert!(!core.boot_auto(&core.runners[0].def));
        assert_eq!(calls.lock().unwrap().len(), 0);
    }

    // boot_status: tier group order + pinned field
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
        // The state string is non-empty
        assert!(!rows[0].state.is_empty());
    }

    // T2 group (APP · on-demand panel): down batch-stops T2 services, never touches T0
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
            "T0 untouched"
        );
        assert_eq!(res["group"], "T2");
        assert_eq!(res["action"], "down");
    }

    // issue #106: app joint group (T1∪T2) — down stops both tiers, never touches T0
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
        assert!(
            calls.contains(&"fusion-cowork/stop".to_string()),
            "T1 stopped"
        );
        assert!(
            calls.contains(&"fusion-health/stop".to_string()),
            "T2 stopped"
        );
        assert!(
            !calls.iter().any(|c| c.starts_with("fusion-mlx/")),
            "T0 untouched"
        );
        assert_eq!(res["group"], "app");
        assert_eq!(res["action"], "down");
    }

    // issue #107 D2: cold-start reconciliation — surviving processes that pass probing are adopted as Healthy (counters reset),
    // those failing probing stay Stopped; idempotent, a second round adds nothing
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
                // The 1st runner (cowork-web) passes probing, the 2nd (health) refuses connection
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
        assert_eq!(adopted, 1, "only services passing probing are adopted");
        assert!(matches!(core.runners[0].state, runner::State::Healthy));
        assert!(matches!(core.runners[1].state, runner::State::Stopped));
        // Idempotent: already-Healthy services are not counted in the second round
        let adopted2 = core.reconcile_on_boot().await;
        assert_eq!(adopted2, 0);
    }

    // issue #110 acceptance: a managed child killed externally, then the daemon restarts — reconciliation must
    // adopt-or-retire without double-starting. Probe refuses connection (process dead) → stays Stopped, no reset,
    // no restart scheduled (tick does not restart Stopped), so an old + new instance can never coexist.
    // issue #149: the operator downs a service while exec is in flight — the in-flight restart result must be
    // discarded, never flipping Stopped back to Healthy/Starting (silently undoing the down).
    #[tokio::test]
    async fn test_issue149_down_during_inflight_exec_discards_result() {
        let defs = vec![def("fusion-site", Layer::App, 11481)];
        let mut core = SupervisorCore::new_with_fns(
            defs,
            fake_cfg(),
            |_| ProbeResult::Unhealthy {
                reason: crate::probe::UnhealthyReason::ConnectionRefused,
            },
            |_svc, _cmd, _env| Ok(()),
        );
        // A due restart job was collected during the storm (exec in flight outside the lock)
        core.runners[0].state = runner::State::Restarting;
        core.runners[0].pending_exec = Some(true);
        core.runners[0].exec_in_flight = true;
        let execs = core.collect_pending_execs();
        assert_eq!(execs.len(), 1);
        // The operator downs it during the in-flight exec window (ManualStop)
        core.runners[0].mark_stopped();
        assert!(matches!(core.runners[0].state, runner::State::Stopped));
        // exec completes and its result is settled — it must be discarded, the state stays Stopped
        let results = vec![("fusion-site".to_string(), runner::ExecOutcome::Ran(Ok(())))];
        core.finish_pending_execs(2000, results).await;
        assert!(matches!(core.runners[0].state, runner::State::Stopped));
        assert!(!core.runners[0].exec_in_flight);
    }

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
                // Both targets already dead (external kill): all refuse connection
                let _ = spec;
                ProbeResult::Unhealthy {
                    reason: crate::probe::UnhealthyReason::ConnectionRefused,
                }
            },
            |_svc, _cmd, _env| Ok(()),
        );
        let adopted = core.reconcile_on_boot().await;
        assert_eq!(adopted, 0, "a dead service must not be adopted");
        for r in &core.runners {
            assert!(matches!(r.state, runner::State::Stopped));
            assert!(
                r.restart_after.is_none(),
                "no restart must be scheduled (would double-start)"
            );
            assert!(!r.start_retry);
            assert_eq!(r.fail_streak, 0);
        }
        // Double-check: reconcile triggers no start calls (start_fn never recorded into the crash window)
        for r in &core.runners {
            assert_eq!(r.policy.crash_count(), 0, "reconciliation is not a crash");
        }
    }

    // Invalid group/action bails immediately, touching no services
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

    // load_boot_pins: reload pins from the store (semantics survive restart; persistence itself is tested in store.rs)
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
