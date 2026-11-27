use crate::alert::{Alert, AlertKind, Alerter};
use crate::manifest::ServiceDef;
use crate::probe::ProbeResult;
use crate::store::HistoryStore;
use crate::supervisor::policy::RestartPolicy;
use anyhow::Result;

pub type ProbeFn = std::sync::Arc<dyn Fn() -> ProbeResult + Send + Sync>;
pub type StartFn = std::sync::Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;

// issue #122 phase-2: run the start closure per action mode (restart = one command; otherwise stop+start).
pub(crate) fn run_start_verbs(f: &StartFn, is_restart: bool) -> Result<()> {
    if is_restart {
        f("restart")
    } else {
        f("stop").and_then(|_| f("start"))
    }
}

// issue #124: per-service result of the lock-free exec. Adopted = the pre-exec probe found a
// healthy instance already answering on the port (the operator's manual start.sh restart won
// the race, or the service self-healed during slow startup) — skip the spawn and adopt the
// external instance, preventing a double-process port collision (caller 502).
pub enum ExecOutcome {
    Ran(Result<()>),
    Adopted,
}

// issue #47: config-fatal exit code (sysexits.h EX_CONFIG). fusion-gateway contract:
// 78 = config is broken and self-healing failed (restart cannot help, needs manual config fix), other exit codes = ordinary crash.
pub(crate) const EXIT_CONFIG: i32 = 78;
// issue #47: fast-fail window (sec) — probe unreachable again within the window after a successful start = fast-exit
const FAST_FAIL_WINDOW_SEC: u64 = 10;
// issue #47: consecutive fast-fail circuit-breaker threshold (reaching it → alert + escalated backoff)
const FAST_FAIL_STREAK: u32 = 3;
// issue #47: config-fatal cap (hitting it marks Failed, manual fusion-sv restart resets)
const CONFIG_FAIL_CAP: u32 = 5;
// issue #90: start-failure retry cap — services that never started successfully retry
// with the escalated backoff (10→60→300s); N consecutive failures still not up → Failed (manual dependency/config fix).
// N=10 covers ~41 minutes (the typical boot-time launchd concurrency race window) and does not
// consume crash-window quota (a service that never started is not a crash-loop).
pub(crate) const START_FAIL_CAP: u32 = 10;
// issue #79: consecutive-failure restart threshold — only N consecutive probe failures enter the restart flow.
// Busy services (fusion-mlx high GIL load) must not escalate to restart on a single /healthz timeout blip;
// any successful probe in between resets the counter, so jitter never escalates. A genuine outage
// still restarts promptly after 3 consecutive failures (poll_unhealthy_sec=2 ≈ 6s).
pub(crate) const RESTART_FAIL_STREAK: u32 = 3;
// issue #153: split Timeout from ConnectionRefused — under heavy inference load (fusion-mlx
// 27B decode GIL saturation), /healthz can time out repeatedly while the process is healthy; the
// 3-failure threshold would kill a healthy engine (SIGTERM'ing in-flight generations, three proven
// cases on 2026-11-24). Timeout means "slow to respond / transiently unreachable", not
// "process dead" (a true death gives ConnectionRefused first); it takes
// TIMEOUT_RESTART_STREAK consecutive timeouts to schedule a restart, and any success in between resets.
// 15 consecutive failures × (2s tick + ≤10s probe timeout) ≈ ≥2.5min of uninterrupted starvation before
// escalating — enough to span a full long generation; a genuinely hung service is reclaimed within minutes. The
// start-retry period (start_retry) is exempt: the process not listening yet is expected, keep
// RESTART_FAIL_STREAK for fast re-pull.
pub(crate) const TIMEOUT_RESTART_STREAK: u32 = 15;

// issue #47: circuit-breaker backoff sequence 10s → 60s → 300s (capped). Unlike the ordinary crash 1→2→4→…:
// for config-fatal / fast-fail, quick retries just burn process startup; stretch the interval and wait for manual repair.
fn escalated_delay(n: u32) -> u64 {
    match n {
        0 | 1 => 10,
        2 => 60,
        _ => 300,
    }
}

// issue #90: start-failure retry curve 5s → 15s → 60s → 300s (capped). Kept separate from
// escalated_delay (circuit-breaker): boot-time dependency races (PG stale pid / launchd late start)
// usually self-heal within a minute; the first two quick re-pulls keep the core outage window to
// seconds, then escalation prevents long-term failures from burning start.sh.
// Production evidence 2026-11-02: gateway dependency race lasted 13.5min; the old escalated curve left a 5min outage per attempt.
fn start_retry_delay(n: u32) -> u64 {
    match n {
        0 | 1 => 5,
        2 => 15,
        3 => 60,
        _ => 300,
    }
}

// issue #47: extract the exit code from exec_* failure messages (convention format "exit code=<n>",
// see the bail! format in supervisor/mod.rs — changing one requires syncing the other).
fn exit_code_of(err_msg: &str) -> Option<i32> {
    let rest = err_msg.split("exit code=").nth(1)?;
    let digits: &str = rest
        .split(|c: char| !(c.is_ascii_digit() || c == '-'))
        .next()
        .unwrap_or("");
    digits.parse().ok()
}

// issue #111: PID liveness — `kill -0` semantics (no kill, probe only). kill unavailable → true
// (pass-open, falls back to pre-fix behavior; don't misjudge an outage just because the tool is missing).
fn process_alive(pid: i32) -> bool {
    match std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
    {
        Ok(s) => s.success(),
        Err(e) => {
            tracing::warn!(pid, err = %e, "kill -0 unavailable, treat process as alive (pass-open)");
            true
        }
    }
}

// Read pid from pidfile (missing/invalid → None). Standalone free fn for unit testing.
fn read_pidfile(path: &std::path::Path) -> Option<i32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().parse::<i32>().ok()
}

// issue #111: pidfile → death determination. Missing/invalid/process still alive → None;
// pid dead → Some(ProcessExit). Pure function (path injected) for unit testing.
fn dead_pid_at(path: &std::path::Path) -> Option<crate::probe::UnhealthyReason> {
    let pid = read_pidfile(path)?;
    if process_alive(pid) {
        None
    } else {
        Some(crate::probe::UnhealthyReason::ProcessExit)
    }
}

// issue #86: detect whether an operator is currently running start.sh for this service (any subcommand).
// Port probing cannot distinguish "the operator's do_restart stop window" from "a real outage" — both
// give ConnectionRefused. If the supervisor auto-restarts ahead of an operator's restart:
// 1) it races the operator's do_start and double-spawns the process; 2) the supervisor's exec only carries the
// daemon env snapshot (env_n=0), so the operator's inline env overrides are silently dropped (2026-10-31 fusion-mlx
// FUSION_MLX_HEAVY_ADMISSION_LINE=0.99 proven lost).
// Two-phase detection:
//   Phase 1: pgrep -f "<repo_dir>/start[.]sh" — cmdline contains the repo-prefixed path
//   (absolute invocation / `bash fusion-mlx/start.sh` relative to the monorepo root).
//   Phase 2 (2026-10-31 production patch): operator cd's into the repo and runs `./start.sh restart` —
//   cmdline is only "./start.sh" with no repo prefix, which phase 1 misses (fusion-mlx
//   env_n=0 premature restart, proven). Collect candidate processes by basename, read each cwd with lsof, compare
//   against FUSION_ROOT/<repo_dir>.
// Self-match guard: the pattern uses the [.] bracket form; a concurrent pgrep over the same repo has the literal
// "start[.]sh" in its cmdline, and the regex matches only "start.sh", not "start[.]sh", eliminating pgrep cross-matching
// (7 false positives seen under parallel test runs); the supervisor's own exec runs synchronously inside the core lock,
// so tick never concurrently sees its own start.sh process. pgrep/lsof unavailable → false
// (pass-open, behavior falls back to pre-fix).
// Mentioned ≠ executed (#136 follow-up): candidates hit by pgrep must still pass the is_shell_process
// recheck — only a shell binary counts as truly running the script, excluding unrelated processes whose
// cmdline merely carries the script path text (proven by a parallel agent's claude process).
fn pgrep_pids(pat: &str) -> Option<Vec<String>> {
    match std::process::Command::new("pgrep")
        .arg("-f")
        .arg(pat)
        .output()
    {
        Ok(o) if o.status.success() => Some(
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect(),
        ),
        Ok(_) => Some(Vec::new()),
        Err(e) => {
            tracing::warn!(err = %e, "pgrep unavailable, assume no operator script running (pass-open)");
            None
        }
    }
}

// Read a process cwd with lsof: `lsof -a -p <pid> -d cwd -Fn` outputs two lines, p<pid> / n<path>.
// Returns the canonicalized path (/var-style symlink dirs resolve to /private/... on macOS).
fn proc_cwd(pid: &str) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("lsof")
        .args(["-a", "-p", pid, "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('n') {
            return std::fs::canonicalize(p).ok().or_else(|| Some(p.into()));
        }
    }
    None
}

// issue #136 follow-up: pgrep -f matches the full cmdline text, so any process "mentioning" the
// script path hits — proven: a parallel agent's claude process carried the entire monorepo
// CLAUDE.md in its cmdline (including "fusion-mlx/start.sh"), misjudged the operator script as alive,
// and the auto-restart was skipped (proven by the test_issue90 circuit-breaker test; production would hit it too). start.sh is a bash script,
// so a real invoker's comm is necessarily a shell; agent binaries (claude/node) are excluded.
fn is_shell_process(pid: &str) -> bool {
    let out = match std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", pid])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let comm = String::from_utf8_lossy(&out.stdout).trim().to_string();
    matches!(
        std::path::Path::new(&comm)
            .file_name()
            .and_then(|s| s.to_str()),
        Some("bash") | Some("sh") | Some("zsh") | Some("dash") | Some("ksh")
    )
}

fn start_sh_process_alive(repo_dir: &str) -> bool {
    // Phase 1: cmdline carries the path prefix (candidates must pass the shell comm recheck, see is_shell_process)
    let pat = format!("{repo_dir}/start[.]sh");
    if pgrep_pids(&pat).is_some_and(|pids| pids.iter().any(|p| is_shell_process(p))) {
        return true;
    }
    // Phase 2: relative cwd invocation (./start.sh), attribute ownership by cwd
    let Some(pids) = pgrep_pids("start[.]sh") else {
        return false;
    };
    let root = std::env::var("FUSION_ROOT").unwrap_or_else(|_| ".".to_string());
    let want = std::path::Path::new(&root).join(repo_dir);
    let want = std::fs::canonicalize(&want).unwrap_or(want);
    for pid in pids {
        if !is_shell_process(&pid) {
            continue;
        }
        if let Some(cwd) = proc_cwd(&pid)
            && cwd == want
        {
            return true;
        }
    }
    false
}

// State machine: Stopped→Starting→Healthy→(Unhealthy)→Restarting→Healthy|Failed
//         Healthy→Draining→Stopped (drain completed) | Healthy→Draining→Healthy (drain canceled)
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum State {
    Stopped,
    Starting,
    Healthy,
    Unhealthy,
    Restarting,
    Failed,
    // issue #9: draining traffic away. Probes still run, but tick does not restart (a draining service exiting is expected).
    // Grace expiry or probe unreachable (in-flight finished / service stopped answering) → Stopped.
    Draining,
}

pub struct ServiceRunner {
    pub def: ServiceDef,
    pub state: State,
    pub policy: RestartPolicy,
    pub restart_cmd: String, // "restart" | "stop+start"
    // issue #9: drain grace deadline (unix sec). None = not draining or no grace.
    pub drain_grace_until: Option<u64>,
    // issue #31: backoff restart deadline (unix sec). None = no restart pending.
    // Previously the backoff slept while holding the lock — several failing services would hold the
    // SupervisorCore Mutex for tens of seconds, starving RPC/HTTP status. Instead record the deadline and return this tick,
    // later ticks actually invoke_start once due (poll_unhealthy_sec=2s, skew ≤2s).
    pub restart_after: Option<u64>,
    // issue #47: last successful invoke_start time (unix sec). None = never started successfully.
    pub(crate) last_start_ts: Option<u64>,
    // issue #47: consecutive fast-fail count — probe unreachable within FAST_FAIL_WINDOW_SEC after a successful start.
    fast_fail_streak: u32,
    // issue #47: fast-fail circuit-breaker alert latch (alert once at threshold, reset on Healthy recovery, no spam)
    fast_fail_alerted: bool,
    // issue #47: config-fatal (exit 78) consecutive count — reaching CONFIG_FAIL_CAP marks Failed.
    config_fail_streak: u32,
    // issue #79: consecutive probe-failure count — reaching RESTART_FAIL_STREAK enters the restart flow.
    // Any successful probe resets it (jitter does not escalate). pub for integration-test presetting.
    pub fail_streak: u32,
    // issue #90: consecutive start-failure count — the start-failure retry flow backs off escalated,
    // reaching START_FAIL_CAP marks Failed. Reset by start success / Healthy / down.
    pub(crate) start_fail_streak: u32,
    // issue #90: whether in the "up-path start-failure retry" flow. Set only by mark_start_failed;
    // cannot be judged from last_start_ts — a manually started service also has None there,
    // and its crash should go through normal crash recovery, not the start-retry cap.
    pub(crate) start_retry: bool,
    // issue #86: operator start.sh race alert latch — WARN once on the first skipped auto-restart
    // (including the inline env loss note), silent while skipping persists; reset on Healthy recovery.
    operator_race_notified: bool,
    probe_fn: ProbeFn,
    start_fn: StartFn,
    // issue #123: pre-run probe cache — filled by tick_all's concurrent preflight, taken by tick (None = probe live).
    // Avoids lock-held serial probes pinning the lock for N×2s on a degraded fleet.
    pub(crate) cached_probe: Option<ProbeResult>,
    // issue #122 phase-2: due crash-restart exec moved out of the core lock. tick's due branch only parks
    // the job (records old_pid + action mode), collect_pending_execs takes it for lock-free execution, finish backfills.
    // Some(true) = restart command, Some(false) = stop+start; exec_in_flight prevents tick re-entry
    // from re-pulling during the execution window.
    pub(crate) pending_exec: Option<bool>,
    pending_old_pid: Option<i32>,
    pub(crate) exec_in_flight: bool,
    // issue #126: startup grace window length (sec). 0 = disabled (test default); production new() fills it from
    // cfg.startup_grace_secs. The grace deadline is recorded into boot_grace_until when exec succeeds.
    pub startup_grace_secs: u64,
    // issue #126: startup grace deadline (unix sec). Some(t) with now < t → probe failures don't count
    // streaks and don't schedule restarts (a slow-starting daemon needs 30-60s to bind its port; it must
    // not have the crash cap burned during the startup window and be killed while starting). Cleared on Healthy recovery.
    boot_grace_until: Option<u64>,
    // issue #142: stale pidfile episode latch — the "probe passed but pidfile process dead" case flips
    // state only once. A self-healing service (its own scheduler restarts with a new PID) has a perpetually
    // stale pidfile; without the latch every tick would do: Healthy+dead pid → Unhealthy (ServiceDown is
    // deduped) → next tick probe recovers → ServiceBack (not deduped) → endless flapping alert storm.
    stale_pid_active: bool,
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
            drain_grace_until: None,
            restart_after: None,
            last_start_ts: None,
            operator_race_notified: false,
            fast_fail_streak: 0,
            fast_fail_alerted: false,
            config_fail_streak: 0,
            fail_streak: 0,
            start_fail_streak: 0,
            start_retry: false,
            cached_probe: None,
            pending_exec: None,
            pending_old_pid: None,
            exec_in_flight: false,
            startup_grace_secs: 0,
            boot_grace_until: None,
            stale_pid_active: false,
            probe_fn: std::sync::Arc::new(probe_fn),
            start_fn: std::sync::Arc::new(start_fn),
        }
    }

    // One supervision step. now_ts = unix sec (injected, deterministic).
    pub async fn tick(
        &mut self,
        now_ts: u64,
        store: &HistoryStore,
        alerter: &mut Alerter,
    ) -> Result<()> {
        // issue #122 phase-2: crash-recovery exec in flight outside the lock — this tick fully yields (no probing,
        // no streak counting, same semantics as the old lock-held tick blocking during exec, prevents double pull).
        if self.exec_in_flight {
            return Ok(());
        }
        // issue #123: prefer the preflight cache (concurrent probe result); once taken, this tick does not probe live again
        let result = self
            .cached_probe
            .take()
            .unwrap_or_else(|| (self.probe_fn)());
        self.persist_health(&result, now_ts, store);
        // issue #9: draining services do not enter the restart flow. Probes still record health.
        // Probe unreachable (in-flight finished / service stopped answering) or grace expired → Stopped + alert.
        if self.state == State::Draining {
            let grace_expired = self.drain_grace_until.map(|t| now_ts >= t).unwrap_or(true);
            let drained = !matches!(result, ProbeResult::Healthy { .. }) || grace_expired;
            if drained {
                tracing::info!(svc = %self.def.name, grace_expired, "drain completed → Stopped");
                self.state = State::Stopped;
                self.drain_grace_until = None;
                let _ = alerter
                    .alert(Alert {
                        service: self.def.name.clone(),
                        kind: AlertKind::ServiceDrained,
                        detail: format!("drain completed (grace_expired={grace_expired})",),
                        ts: now_ts,
                    })
                    .await;
            }
            // while draining, stay Draining — no restart, no ServiceDown alert
            return Ok(());
        }
        match result {
            ProbeResult::Healthy { latency_ms } => {
                // issue #111: probe-pass ≠ process-alive — Daemon-managed services (supervisor
                // owns their pidfile) get a PID liveness recheck when the probe passes; a dead pidfile pid
                // while the port still answers = stale listener holding the port (orphan/port collision), routed as a
                // genuine-outage ProcessExit into auto-restart (#96 port backstop + #110 ownership verification clear the stale listener).
                // issue #124: Failed state + probe recovered (an external instance took over the port) →
                // take the same recovery path to clear Failed — the service is plainly healthy; the status table must not stay red forever.
                if matches!(self.state, State::Healthy | State::Failed) {
                    if let Some(reason) = self.process_exit_if_dead() {
                        // issue #142: flip only once per episode. Latch already set = a ProcessExit
                        // was already reported and the probe recovered afterwards — a self-healing service (its
                        // own scheduler rotates PIDs) has a perpetually stale pidfile, and flipping again only creates a
                        // Down/Back flapping storm (ServiceDown is deduped, ServiceBack is not).
                        // Actual stale-listener cleanup depends on probe failure going through the normal restart flow, unaffected.
                        if self.stale_pid_active {
                            tracing::debug!(
                                svc = %self.def.name,
                                "stale pidfile episode already reported, keep healthy"
                            );
                        } else {
                            self.stale_pid_active = true;
                            tracing::warn!(
                                svc = %self.def.name,
                                "probe passed but pidfile process is dead (stale port holder suspected)"
                            );
                            self.handle_unhealthy(reason, now_ts, alerter).await;
                        }
                        return Ok(());
                    }
                    // pidfile process alive — stale episode is over, clear the latch
                    self.stale_pid_active = false;
                }
                // issue #149: Stopped (operator downed) + probe passed = a leftover process the down's stop
                // failed to kill (stale listener) — must never flip back to Healthy, otherwise
                // the operator's down is silently undone and the next probe failure walks the full restart flow again
                // (2026-11-23 fusion-site/cowork-web revived 9 minutes after down, proven).
                // The leftover process is cleaned up manually by the operator; the state stays Stopped.
                if matches!(self.state, State::Stopped) {
                    tracing::info!(
                        svc = %self.def.name,
                        "probe passed but service is Stopped (operator downed) — leftover listener, keep Stopped"
                    );
                    return Ok(());
                }
                // issue #124: Failed + probe Healthy = an external instance took over (the new process the
                // operator's manual restart spawned), flip state back to Healthy + full reset — no longer stuck
                // in Failed waiting for a manual fusion-sv restart.
                let was_down = matches!(
                    self.state,
                    State::Unhealthy | State::Restarting | State::Failed
                );
                self.state = State::Healthy;
                // issue #126: probe has recovered — the startup grace window has served its purpose, clear it.
                self.boot_grace_until = None;
                // issue #79: any successful probe resets the consecutive-failure count (jitter does not escalate).
                self.fail_streak = 0;
                if was_down {
                    // issue #79: on recovery, clear the stale schedule left by the previous failure round,
                    // otherwise the next entry into Unhealthy would hit the expired due and restart immediately.
                    self.restart_after = None;
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
                    // issue #47: recovery resets the circuit-breaker counters (fast-fail / config-fatal)
                    self.fast_fail_streak = 0;
                    self.fast_fail_alerted = false;
                    self.config_fail_streak = 0;
                    // issue #90: start-failure retry counters reset with recovery
                    self.start_fail_streak = 0;
                    self.start_retry = false;
                    // issue #86: recovery resets the operator-race alert latch
                    self.operator_race_notified = false;
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
        if let Err(e) = store.record_health(&self.def.name, status, latency, now_ts, "native") {
            tracing::warn!(svc = %self.def.name, err = %e, "health write fail (non-fatal, keep supervising)");
        }
    }

    async fn handle_unhealthy(
        &mut self,
        reason: crate::probe::UnhealthyReason,
        now_ts: u64,
        alerter: &mut Alerter,
    ) {
        // issue #126: startup grace window — a slow-starting daemon just spawned by exec (needs
        // 30-60s to bind its port) will inevitably fail probes; the old logic burned fail_streak within the startup window
        // and restarted on schedule, killing a healthy process that was still starting (2026-11-11 agent-studio, 28
        // AutoRestarts + RestartCapHit mid-boot, proven). Within the grace window, probe failures don't count
        // streaks / don't schedule restarts / don't alert; after the window passes, normal supervision resumes.
        if let Some(until) = self.boot_grace_until
            && now_ts < until
            && matches!(
                self.state,
                State::Starting | State::Unhealthy | State::Restarting
            )
        {
            tracing::info!(
                svc = %self.def.name,
                remaining_sec = until - now_ts,
                reason = ?reason,
                "startup grace: probe not up yet, failure not counted (waiting for daemon startup to finish)"
            );
            return;
        }
        // Anti-storm: BadPath neither restarts nor records a crash, only alerts on the wrong path
        if !reason.restart_eligible() {
            tracing::warn!(
                svc = %self.def.name,
                reason = ?reason,
                "probe unreachable but not an outage signal, no restart (check the probe_map path)"
            );
            // issue #49: the same root cause alerting every tick (2s) floods the logs — alert only on the first
            // transition into Unhealthy, stay silent while Unhealthy persists; after recovering to Healthy,
            // a new failure alerts again (the state machine reset lifts the silence).
            // Stopped/Failed/Draining probe failures do not change state — a manually stopped service
            // (down_one) must not be flipped back to Unhealthy red by the next tick.
            if matches!(self.state, State::Stopped | State::Failed | State::Draining) {
                return;
            }
            let first_transition = !matches!(self.state, State::Unhealthy);
            self.state = State::Unhealthy;
            if first_transition {
                let _ = alerter
                    .alert(Alert {
                        service: self.def.name.clone(),
                        kind: AlertKind::RestartFailed,
                        detail: format!("probe path suspect: {:?}", reason),
                        ts: now_ts,
                    })
                    .await;
            }
            return;
        }
        // Genuine outage signals (ConnectionRefused/Timeout)
        // issue #79: manual states (Failed/Stopped) don't count; all others increment the consecutive-failure count,
        // reaching RESTART_FAIL_STREAK enters the restart flow.
        if !matches!(self.state, State::Failed | State::Stopped) {
            self.fail_streak += 1;
        }
        match self.state {
            State::Healthy | State::Starting => {
                tracing::warn!(svc = %self.def.name, reason = ?reason, "service down");
                self.state = State::Unhealthy;
                let _ = alerter
                    .alert(Alert {
                        service: self.def.name.clone(),
                        kind: AlertKind::ServiceDown,
                        detail: format!("{:?}", reason),
                        ts: now_ts,
                    })
                    .await;
                // First down: don't restart immediately, wait for the next tick (avoids flap misjudgment)
            }
            State::Unhealthy | State::Restarting => {
                // issue #79: below the consecutive-failure threshold, no restart is scheduled — busy services (fusion-mlx
                // high GIL load) reset the probe jitter count on any success in between, never escalating;
                // a genuine outage is scheduled after 3 consecutive failures (poll 2s ≈ 6s), still timely.
                // issue #153: Timeout takes a higher threshold of its own — a timeout means "transiently
                // unreachable", not "process dead" (a true death gives ConnectionRefused first); during long
                // generations with GIL saturation /healthz can time out for tens of seconds while the engine is healthy.
                let streak_needed = if matches!(reason, crate::probe::UnhealthyReason::Timeout)
                    && !self.start_retry
                {
                    TIMEOUT_RESTART_STREAK
                } else {
                    RESTART_FAIL_STREAK
                };
                if self.fail_streak < streak_needed {
                    tracing::info!(
                        svc = %self.def.name,
                        streak = self.fail_streak,
                        needed = streak_needed,
                        "consecutive failures below threshold, restart not scheduled yet"
                    );
                    return;
                }
                // issue #31: backoff does not sleep holding the lock (the old implementation pinned the lock for tens of seconds, starving status/metrics);
                // instead record the deadline and return this tick; the due tick actually restarts.
                // Crash accounting semantics match the old version: each actual restart attempt records 1 crash.
                match self.restart_after {
                    None => {
                        // issue #90: the start-failure retry flow uses the escalated backoff (10→60→300);
                        // policy's 1→2→4 sequence would burn the 5-crash quota in ~31s
                        // (pure waste while a dependency isn't ready).
                        let delay = if self.config_fail_streak > 0 {
                            // issue #47: while the circuit breaker is active, backoff follows the 10→60→300 sequence,
                            // not the ordinary crash 1→2→4 fast sequence (burning process startup).
                            escalated_delay(self.config_fail_streak)
                        } else if self.start_retry {
                            start_retry_delay(self.start_fail_streak.max(1))
                        } else if self.fast_fail_streak >= FAST_FAIL_STREAK {
                            escalated_delay(self.fast_fail_streak)
                        } else {
                            self.policy.next_delay()
                        };
                        self.restart_after = Some(now_ts + delay);
                        tracing::info!(svc = %self.def.name, delay, "restart after backoff (scheduled)");
                    }
                    Some(due) if now_ts < due => {
                        // not due yet, wait for later ticks
                    }
                    Some(_) => {
                        // issue #86: operator start.sh in progress → skip this round's auto-restart.
                        // Port probing cannot distinguish an operator do_stop window from a real outage; jumping the gun would
                        // double-spawn the process, and the exec only carries the daemon env snapshot (env_n=0), so the operator's
                        // inline env overrides are silently dropped. Keep restart_after (recheck next tick),
                        // no crash recorded — an operator action is not a crash. If the probe recovers after the script
                        // exits, it self-heals; if still down, the next round restarts normally.
                        if start_sh_process_alive(&self.def.repo_dir) {
                            if !self.operator_race_notified {
                                self.operator_race_notified = true;
                                tracing::warn!(
                                    svc = %self.def.name,
                                    repo = %self.def.repo_dir,
                                    "operator start.sh in progress, skipping auto-restart (avoids race double-spawn + \\
                                     inline env being overridden by the daemon env snapshot; during an operator restart, \\
                                     inline env overrides only apply to the operator's own exec)"
                                );
                            }
                            return;
                        }
                        self.restart_after = None;
                        // issue #47: in an exit-78 config-fatal retry — consumes neither crash-window
                        // nor start-fail quota; the invoke_start failure branch below keeps counting
                        // config_fail_streak
                        if self.config_fail_streak > 0 {
                            // no-op: quota accounting is left to the failure branch below
                        } else if self.start_retry {
                            // issue #90: the start-failure retry flow consumes no crash-window quota
                            // (it is not a crash-loop, it is waiting on a dependency); it uses the independent
                            // start_fail_streak cap — the typical recovery window of a boot-time dependency race can reach tens of minutes.
                            self.start_fail_streak += 1;
                            if self.start_fail_streak >= START_FAIL_CAP {
                                tracing::error!(
                                    svc = %self.def.name,
                                    streak = self.start_fail_streak,
                                    "start-failure retry cap hit, marked Failed, restarts stopped (check dependencies/config)"
                                );
                                self.state = State::Failed;
                                let _ = alerter
                                    .alert(Alert {
                                        service: self.def.name.clone(),
                                        kind: AlertKind::RestartCapHit,
                                        detail: format!(
                                            "start-fail cap {} hit",
                                            self.start_fail_streak
                                        ),
                                        ts: now_ts,
                                    })
                                    .await;
                                return;
                            }
                        } else {
                            let capped = self.policy.record_crash(now_ts);
                            if capped {
                                tracing::error!(svc = %self.def.name, "crash-window cap hit, marked Failed, restarts stopped");
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
                        }
                        // issue #47: fast-fail circuit breaker — probe unreachable within the window after the last successful start.
                        // Reaching the threshold consecutively → circuit-breaker alert (latched, reset on recovery), backoff escalates 10→60→300 afterwards.
                        if let Some(t0) = self.last_start_ts
                            && now_ts.saturating_sub(t0) < FAST_FAIL_WINDOW_SEC
                        {
                            self.fast_fail_streak += 1;
                            if self.fast_fail_streak == FAST_FAIL_STREAK && !self.fast_fail_alerted
                            {
                                self.fast_fail_alerted = true;
                                tracing::error!(
                                    svc = %self.def.name,
                                    streak = self.fast_fail_streak,
                                    window_sec = FAST_FAIL_WINDOW_SEC,
                                    "fast-fail circuit breaker (down seconds after start), backoff escalated"
                                );
                                let _ = alerter
                                    .alert(Alert {
                                        service: self.def.name.clone(),
                                        kind: AlertKind::FastFailLoop,
                                        detail: format!(
                                            "{} consecutive fast-exits (<{}s) after start",
                                            self.fast_fail_streak, FAST_FAIL_WINDOW_SEC
                                        ),
                                        ts: now_ts,
                                    })
                                    .await;
                            }
                        }
                        // issue #112: audit — grab the old pid before the auto-recovery actually pulls,
                        // and record the AutoRestart event on success (old/new pid traceable).
                        // issue #122 phase-2: exec runs outside the lock — only park the job here,
                        // collect_pending_execs takes it, finish_pending_execs books the outcome.
                        self.pending_old_pid = self.pidfile_pid();
                        self.pending_exec = Some(self.restart_cmd == "restart");
                        self.state = State::Restarting;
                    }
                }
            }
            State::Failed | State::Stopped => {
                // no action: Failed requires a manual fusion-sv restart <svc> to reset
            }
            State::Draining => {
                // Unreachable: tick() returns early for Draining before handle_unhealthy.
                // Defensive fallback (compiler-exhaustive match), theoretically never executed.
                tracing::warn!(
                    svc = %self.def.name,
                    "handle_unhealthy reached Draining (should not happen, already intercepted by tick)"
                );
            }
        }
    }

    // issue #111: only Daemon specs (supervisor-owned pidfile) participate in the PID recheck.
    // Missing/unreadable/invalid pidfile → None (no misjudgment); the pid's process still alive → None
    // (including pid reuse — even if the reuser is not this service, pass; never punish a healthy probe).
    fn process_exit_if_dead(&self) -> Option<crate::probe::UnhealthyReason> {
        match crate::start_map::start_spec_for(&self.def.name) {
            Some(crate::start_map::StartSpec::Daemon { .. }) => {}
            _ => return None,
        }
        dead_pid_at(&crate::start_map::pid_path(&self.def.name))
    }

    // Read pid from pidfile (missing/invalid → None)
    fn pidfile_pid(&self) -> Option<i32> {
        read_pidfile(&crate::start_map::pid_path(&self.def.name))
    }

    // issue #122 phase-2: take the parked due-restart job (for lock-free execution).
    pub(crate) fn take_pending_exec(&mut self) -> Option<bool> {
        self.pending_exec.take()
    }

    // issue #123: clone the start closure (for concurrent preflight / lock-free exec).
    pub(crate) fn start_fn_clone(&self) -> StartFn {
        self.start_fn.clone()
    }

    // issue #122 phase-2: book the outcome after the lock-free exec completes (the original due branch's Ok/Err handling).
    pub(crate) async fn apply_exec_result(
        &mut self,
        now_ts: u64,
        alerter: &mut Alerter,
        res: Result<()>,
    ) {
        self.exec_in_flight = false;
        let old_pid = self.pending_old_pid.take();
        match res {
            Err(e) => {
                let detail = e.to_string();
                // issue #47: exit 78 (EX_CONFIG) = config-fatal, restart cannot help —
                // don't burn quick retries, backoff follows 10→60→300; reaching the cap consecutively marks Failed.
                if exit_code_of(&detail) == Some(EXIT_CONFIG) {
                    self.config_fail_streak += 1;
                    self.last_start_ts = None;
                    tracing::error!(
                        svc = %self.def.name,
                        streak = self.config_fail_streak,
                        "config-fatal (exit 78), restart cannot help, config must be fixed"
                    );
                    let _ = alerter
                        .alert(Alert {
                            service: self.def.name.clone(),
                            kind: AlertKind::ServiceConfigFatal,
                            detail,
                            ts: now_ts,
                        })
                        .await;
                    if self.config_fail_streak >= CONFIG_FAIL_CAP {
                        self.state = State::Failed;
                        tracing::error!(svc = %self.def.name, "config-fatal cap hit, marked Failed, restarts stopped");
                        return;
                    }
                    self.restart_after = Some(now_ts + escalated_delay(self.config_fail_streak));
                } else {
                    tracing::warn!(svc = %self.def.name, err = %detail, "start.sh fail");
                    let _ = alerter
                        .alert(Alert {
                            service: self.def.name.clone(),
                            kind: AlertKind::RestartFailed,
                            detail,
                            ts: now_ts,
                        })
                        .await;
                }
                self.state = State::Unhealthy;
            }
            Ok(()) => {
                self.last_start_ts = Some(now_ts);
                // issue #126: exec pulled successfully → record the startup grace deadline. Probe failures within the
                // window count no failures and schedule no restarts (a slow-starting daemon needs 30-60s to bind its port).
                self.set_boot_grace(now_ts);
                // issue #112: auto recovery (crash / start-retry backoff flow)
                // actually pulled successfully → AutoRestart audit event (not deduped, info).
                // See the adjacent ServiceDown event line for the trigger reason.
                let new_pid = self.pidfile_pid();
                let _ = alerter
                    .alert(Alert {
                        service: self.def.name.clone(),
                        kind: AlertKind::AutoRestart,
                        detail: format!(
                            "auto-recovery restart ok old_pid={} new_pid={}",
                            old_pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
                            new_pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
                        ),
                        ts: now_ts,
                    })
                    .await;
                // issue #90: start script success exits the start-retry flow —
                // later failures fall back to crash / fast-fail semantics, counters reset
                self.start_fail_streak = 0;
                self.start_retry = false;
                self.state = State::Restarting;
            }
        }
    }

    // issue #124: the pre-exec probe found Healthy — an external instance (the operator's manual start.sh
    // restart) has taken over the port, adopt it instead of pulling. Takes the recovery path: full counter reset + ServiceBack
    // alert; no AutoRestart emitted (we pulled nothing).
    // issue #126: after a successful exec pull, record the startup grace deadline (when startup_grace_secs>0).
    pub fn set_boot_grace(&mut self, now_ts: u64) {
        if self.startup_grace_secs > 0 {
            self.boot_grace_until = Some(now_ts + self.startup_grace_secs);
        }
    }

    pub(crate) async fn apply_adopted(&mut self, now_ts: u64, alerter: &mut Alerter) {
        self.exec_in_flight = false;
        self.pending_old_pid = None;
        self.restart_after = None;
        self.boot_grace_until = None;
        self.fail_streak = 0;
        self.start_fail_streak = 0;
        self.start_retry = false;
        self.fast_fail_streak = 0;
        self.fast_fail_alerted = false;
        self.config_fail_streak = 0;
        self.operator_race_notified = false;
        self.policy.reset();
        self.state = State::Healthy;
        tracing::info!(
            svc = %self.def.name,
            "pre-exec probe already Healthy, adopted the external instance (respawn skipped to prevent a double-process port collision)"
        );
        let _ = alerter
            .alert(Alert {
                service: self.def.name.clone(),
                kind: AlertKind::ServiceBack,
                detail: "adopted externally started instance (operator restart won the race); respawn skipped".to_string(),
                ts: now_ts,
            })
            .await;
    }

    // issue #122 phase-2: runner-level synchronous finish (test/single-node path) — executes the parked
    // job under the lock and books the outcome. The production path uses collect_pending_execs (lock-free) + finish_pending_execs.
    // issue #124: same semantics as the production run_pending_execs — probe before exec; if already Healthy,
    // adopt and skip the pull.
    pub async fn complete_pending_exec(&mut self, now_ts: u64, alerter: &mut Alerter) {
        let Some(is_restart) = self.pending_exec.take() else {
            return;
        };
        if matches!((self.probe_fn)(), ProbeResult::Healthy { .. }) {
            self.apply_adopted(now_ts, alerter).await;
            return;
        }
        let res = run_start_verbs(&self.start_fn, is_restart);
        self.apply_exec_result(now_ts, alerter, res).await;
    }

    // issue #90: register an up-path start failure. Non-exit-78 start failures are mostly an environment not ready
    // (boot-time dependencies not yet listening, etc.), entering the backoff retry flow (state=Unhealthy + restart_after)
    // rather than terminal Failed — later tick crash-recovery retries with the escalated backoff.
    // exit-78 = config-fatal, retries cannot help, stay Failed (issue #47 semantics).
    // Returns true if a retry has been scheduled.
    pub(crate) fn mark_start_failed(&mut self, err_msg: &str, now_ts: u64) -> bool {
        if exit_code_of(err_msg) == Some(EXIT_CONFIG) {
            tracing::error!(svc = %self.def.name, "start exited 78 config-fatal, marked Failed (retries cannot help)");
            self.state = State::Failed;
            return false;
        }
        self.state = State::Unhealthy;
        // Fill the failure streak directly so the next tick enters the scheduling branch
        self.fail_streak = RESTART_FAIL_STREAK;
        self.start_retry = true;
        let delay = start_retry_delay(self.start_fail_streak.max(1));
        self.restart_after = Some(now_ts + delay);
        tracing::warn!(
            svc = %self.def.name,
            delay,
            err = %err_msg,
            "start failed, retry scheduled (usually a dependency not ready; reaching the cap marks Failed)"
        );
        true
    }

    // issue #90: manual stop clears the pending retry schedule and start-failure counters,
    // otherwise a leftover restart_after after down would fire a stale restart in the window before the next up.
    // issue #149: also discard any pending_exec not yet taken — after an operator down,
    // due-restart jobs scheduled during a storm must not execute (otherwise the down is silently undone).
    // Jobs already taken (exec_in_flight) are rejected from booking by the
    // Stopped guard in finish_pending_execs.
    pub(crate) fn mark_stopped(&mut self) {
        self.state = State::Stopped;
        self.restart_after = None;
        self.pending_exec = None;
        self.start_fail_streak = 0;
        self.start_retry = false;
    }

    pub(crate) fn invoke_start(&self) -> Result<()> {
        if self.restart_cmd == "restart" {
            (self.start_fn)("restart")
        } else {
            (self.start_fn)("stop")?;
            (self.start_fn)("start")
        }
    }

    // issue #9: single probe (for prewarm). Reuses the injected probe_fn.
    pub fn probe_once(&self) -> ProbeResult {
        (self.probe_fn)()
    }

    // issue #123: clone the probe closure (for tick_all's concurrent preflight).
    pub(crate) fn probe_fn_clone(&self) -> ProbeFn {
        self.probe_fn.clone()
    }

    // issue #9: enter drain (remove traffic). grace_sec = grace period (on expiry, forced to Stopped).
    // Only Healthy/Starting may drain; other states return Err (avoids mislabeling).
    pub fn begin_drain(&mut self, grace_sec: u64, now_ts: u64) -> Result<()> {
        if !matches!(
            self.state,
            State::Healthy | State::Starting | State::Unhealthy
        ) {
            anyhow::bail!(
                "drain requires Healthy/Starting/Unhealthy, got {:?} (svc {})",
                self.state,
                self.def.name
            );
        }
        self.state = State::Draining;
        self.drain_grace_until = Some(now_ts + grace_sec);
        tracing::info!(svc = %self.def.name, grace_sec, "drain started → Draining");
        Ok(())
    }

    // issue #9: cancel the drain (restore traffic). Only Draining may cancel.
    pub fn cancel_drain(&mut self) -> Result<()> {
        if self.state != State::Draining {
            anyhow::bail!(
                "cancel_drain requires Draining, got {:?} (svc {})",
                self.state,
                self.def.name
            );
        }
        self.state = State::Healthy;
        self.drain_grace_until = None;
        tracing::info!(svc = %self.def.name, "drain canceled → Healthy");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Layer;
    use crate::probe::{ProbeResult, UnhealthyReason};
    use std::sync::{Arc, Mutex};

    // issue #111: pidfile parsing — missing/invalid/blank/normal
    #[test]
    fn test_read_pidfile_variants() {
        let dir =
            std::env::temp_dir().join(format!("fusion-sv-runner-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // missing
        assert_eq!(read_pidfile(&dir.join("missing.pid")), None);
        // invalid content
        std::fs::write(dir.join("bad.pid"), "not-a-pid\n").unwrap();
        assert_eq!(read_pidfile(&dir.join("bad.pid")), None);
        // empty file
        std::fs::write(dir.join("empty.pid"), "").unwrap();
        assert_eq!(read_pidfile(&dir.join("empty.pid")), None);
        // normal (trailing newline + whitespace)
        std::fs::write(dir.join("ok.pid"), " 4242 \n").unwrap();
        assert_eq!(read_pidfile(&dir.join("ok.pid")), Some(4242));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // issue #111: dead_pid_at — live process → None, dead process → Some(ProcessExit)
    #[test]
    fn test_dead_pid_at_liveness() {
        let dir =
            std::env::temp_dir().join(format!("fusion-sv-runner-dead-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("svc.pid");
        // real child process: alive → None
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        std::fs::write(&pidfile, child.id().to_string()).unwrap();
        assert_eq!(dead_pid_at(&pidfile), None);
        child.kill().unwrap();
        child.wait().unwrap();
        // dead pid uses i32::MAX — macOS pid ceiling is ~99998, this value never exists, avoiding the race
        // where a parallel test's killed child pid is instantly reused (matches production semantics: pid reuse passes)
        std::fs::write(&pidfile, i32::MAX.to_string()).unwrap();
        assert_eq!(
            dead_pid_at(&pidfile),
            Some(crate::probe::UnhealthyReason::ProcessExit)
        );
        // missing pidfile → None (no misjudgment)
        let _ = std::fs::remove_file(&pidfile);
        assert_eq!(dead_pid_at(&pidfile), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // issue #142: stale pidfile episode latch — when the probe always passes and the pidfile process is always dead
    // (a self-healing service whose own scheduler rotates PIDs), 10 consecutive healthy probes produce only 1 ServiceBack
    // (1 Down + 1 Back), no longer flipping every tick into an alert storm.
    #[tokio::test]
    async fn test_issue142_stale_pidfile_single_back_per_episode() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = crate::env_map::tests::EnvGuard::set("HOME", tmp.path().to_str().unwrap());
        let pids = tmp.path().join(".fusion-sv/pids");
        std::fs::create_dir_all(&pids).unwrap();
        // Only Daemon-spec services get the PID recheck (fusion-cowork-web is in the start_map table);
        // the pid uses i32::MAX — never exists within the macOS pid ceiling, race-free (same as test_dead_pid_at)
        let pidfile = pids.join("fusion-cowork-web.pid");
        std::fs::write(&pidfile, i32::MAX.to_string()).unwrap();

        let (_, _, pf, sf) = harness(ProbeResult::Healthy { latency_ms: 5 });
        let mut d = def();
        d.name = "fusion-cowork-web".into();
        let mut r = ServiceRunner::new(d, "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());

        // tick1: Healthy + dead pid → ProcessExit flips to Unhealthy (Down, no restart)
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Unhealthy));
        // tick2: probe recovers → 1 ServiceBack
        r.tick(1005, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Healthy));
        // tick3..12: pidfile still dead but latch set — stays Healthy, zero new alerts
        for i in 0..10u64 {
            r.tick(1010 + i * 5, &s, &mut al).await.unwrap();
            assert!(matches!(r.state, State::Healthy), "tick {}", i);
        }
        assert_eq!(al.alerts_emitted(), 2, "exactly 1 Down + 1 Back");

        // pid alive again → episode ends, latch cleared; dies again → a new episode flips once more
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        std::fs::write(&pidfile, child.id().to_string()).unwrap();
        r.tick(2000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Healthy));
        child.kill().unwrap();
        child.wait().unwrap();
        std::fs::write(&pidfile, i32::MAX.to_string()).unwrap();
        r.tick(2005, &s, &mut al).await.unwrap();
        assert!(
            matches!(r.state, State::Unhealthy),
            "new episode flips again"
        );
        r.tick(2010, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Healthy));
        // cumulative: 2 Down (was the second one suppressed within the dedup window? the 1000→2005 gap exceeds 300s, dedup released) + 2 Back
        assert_eq!(al.alerts_emitted(), 4);
    }

    // issue #90: start-retry curve — first two quick re-pulls (5/15s), then 60/300 escalation
    #[test]
    fn test_start_retry_delay_curve() {
        assert_eq!(start_retry_delay(0), 5);
        assert_eq!(start_retry_delay(1), 5);
        assert_eq!(start_retry_delay(2), 15);
        assert_eq!(start_retry_delay(3), 60);
        assert_eq!(start_retry_delay(4), 300);
        assert_eq!(start_retry_delay(9), 300);
    }

    fn def() -> ServiceDef {
        ServiceDef {
            name: "fusion-rag".into(),
            port: 11436,
            repo_dir: "fusion-rag".into(),
            purpose: "kb".into(),
            fixed: false,
            managed: true,
            layer: Layer::App,
            boot_tier: crate::manifest::BootTier::T2,
        }
    }

    // Mutable probe result + records the start.sh invocation sequence, for assertions
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
        assert!(calls.lock().unwrap().is_empty()); // start.sh not called
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
        // first down: marked Unhealthy + alert, but no restart yet (restart happens next tick)
        assert!(matches!(r.state, State::Unhealthy));
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(r.policy.crash_count(), 0);
    }

    #[tokio::test]
    async fn test_stopped_stays_stopped_on_probe_fail() {
        // a manually downed service (Stopped) must not be flipped back to Unhealthy on probe failure (otherwise the tray turns red)
        let (_, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::UdsConnectFailed,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Stopped;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        r.tick(1002, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Stopped));
    }

    #[tokio::test]
    async fn test_issue149_stopped_stays_stopped_on_probe_success() {
        // issue #149: Stopped + probe passed = a stale listener the down failed to kill,
        // must never flip back to Healthy (otherwise the operator's down is silently undone and the storm restart revives it)
        let (_, _, pf, sf) = harness(ProbeResult::Healthy { latency_ms: 5 });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Stopped;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        r.tick(1002, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Stopped));
        assert_eq!(r.policy.crash_count(), 0);
    }

    #[tokio::test]
    async fn test_unhealthy_records_crash_and_restarts() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: preset the consecutive-failure count to simulate an already-failed intermediate state (test directly from Unhealthy)
        r.fail_streak = 2; // already marked down, this tick enters the restart flow
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // issue #31: backoff no longer sleeps holding the lock — the first tick schedules, not due yet, no restart
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(
            matches!(r.state, State::Unhealthy),
            "scheduling tick must not restart"
        );
        assert_eq!(r.policy.crash_count(), 0);
        assert!(calls.lock().unwrap().is_empty());
        // due tick (first backoff delay=1s): actually restarts + records 1 crash
        r.tick(1001, &s, &mut al).await.unwrap();
        r.complete_pending_exec(1001, &mut al).await;
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
        // prefill 5 crashes; this is the 6th that caps (R10: policy is_capped uses > max, max=5 → the 6th caps)
        for i in 0..5 {
            r.policy.record_crash(900 + i);
        }
        r.state = State::Unhealthy;
        // issue #79: preset the consecutive-failure count to simulate an already-failed intermediate state (test directly from Unhealthy)
        r.fail_streak = 2;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // issue #31: the first tick schedules (5 crashes → backoff 30s); the due tick caps → Failed, no restart
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(
            matches!(r.state, State::Unhealthy),
            "scheduling tick must not restart"
        );
        r.tick(1031, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Failed));
        assert!(calls.lock().unwrap().is_empty()); // capped, no restart
    }

    #[tokio::test]
    async fn test_badpath_no_restart_no_crash() {
        // anti-storm core: BadPath(404) neither restarts nor records a crash
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::BadPath(404),
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Unhealthy));
        assert!(calls.lock().unwrap().is_empty()); // start.sh not called
        assert_eq!(r.policy.crash_count(), 0); // no crash recorded
        // tick once more (state=Unhealthy): BadPath still does not restart
        r.tick(1001, &s, &mut al).await.unwrap();
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(r.policy.crash_count(), 0);
    }

    // issue #49: the RestartFailed alert for non-restart-type probe failures (BadPath/BadStatus) fires only
    // on the first transition into Unhealthy — while Unhealthy persists, it must not spam every tick (2s)
    #[tokio::test]
    async fn test_badpath_alert_only_on_transition() {
        let (_, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::BadStatus(500),
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(0, String::new());
        r.tick(1000, &s, &mut al).await.unwrap(); // transition into Unhealthy → 1 alert
        assert_eq!(al.alerts_emitted(), 1);
        r.tick(1002, &s, &mut al).await.unwrap(); // Unhealthy persists → silent
        r.tick(1004, &s, &mut al).await.unwrap();
        assert_eq!(
            al.alerts_emitted(),
            1,
            "persistent Unhealthy must not alert repeatedly"
        );
    }

    #[tokio::test]
    async fn test_recovery_emits_back() {
        let (calls, probe_cell, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: preset the consecutive-failure count to simulate an already-failed intermediate state (test directly from Unhealthy)
        r.fail_streak = 2;
        // flip the probe to healthy; the tick should recover
        *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 8 };
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Healthy));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_stop_plus_start_when_no_restart_cmd() {
        // restart_cmd="stop+start" (fusion-store has no restart)
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "stop+start", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: preset the consecutive-failure count to simulate an already-failed intermediate state (test directly from Unhealthy)
        r.fail_streak = 2;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // issue #31: the first tick schedules (delay=1s); the due tick executes stop+start
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(
            calls.lock().unwrap().is_empty(),
            "scheduling tick must not restart"
        );
        r.tick(1001, &s, &mut al).await.unwrap();
        r.complete_pending_exec(1001, &mut al).await;
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["stop".to_string(), "start".to_string()]
        );
    }

    // issue #9: drain → Draining → after grace expiry, tick → Stopped, no restart
    #[tokio::test]
    async fn test_drain_transitions_to_draining_then_stopped() {
        let (calls, _, pf, sf) = harness(ProbeResult::Healthy { latency_ms: 5 });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        // enter drain, grace=10s
        r.begin_drain(10, 1000).unwrap();
        assert!(matches!(r.state, State::Draining));
        assert_eq!(r.drain_grace_until, Some(1010));
        // grace not expired + probe healthy → stays Draining, no restart
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1005, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Draining));
        assert!(calls.lock().unwrap().is_empty());
        // grace expired → Stopped, still no restart
        r.tick(1011, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Stopped));
        assert!(r.drain_grace_until.is_none());
        assert!(calls.lock().unwrap().is_empty());
    }

    // issue #9: draining service probe unreachable → tick does not restart, transitions to Stopped
    #[tokio::test]
    async fn test_tick_skips_restart_for_draining() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        r.begin_drain(10, 1000).unwrap();
        // probe unreachable + grace not expired → still transitions to Stopped (in-flight finished, service stopped answering)
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1001, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Stopped));
        // key point: start.sh not called (draining does not enter the restart flow)
        assert!(calls.lock().unwrap().is_empty());
    }

    // issue #9: calling begin_drain from a non-Healthy/Starting/Unhealthy state → Err
    #[tokio::test]
    async fn test_begin_drain_rejects_invalid_state() {
        let (calls, _, pf, sf) = harness(ProbeResult::Healthy { latency_ms: 5 });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Failed;
        assert!(r.begin_drain(10, 1000).is_err());
        assert!(matches!(r.state, State::Failed));
        let _ = calls;
    }

    // issue #9: cancel_drain is only allowed when Draining, otherwise Err
    #[tokio::test]
    async fn test_cancel_drain_only_when_draining() {
        let (calls, probe_cell, pf, sf) = harness(ProbeResult::Healthy { latency_ms: 5 });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        // not Draining → Err
        assert!(r.cancel_drain().is_err());
        // enter drain then cancel → Healthy
        r.begin_drain(10, 1000).unwrap();
        assert!(matches!(r.state, State::Draining));
        r.cancel_drain().unwrap();
        assert!(matches!(r.state, State::Healthy));
        assert!(r.drain_grace_until.is_none());
        let _ = (calls, probe_cell);
    }

    // issue #47: exit-code parsing from exec failure messages (convention format "exit code=<n>")
    #[test]
    fn test_exit_code_of() {
        assert_eq!(
            exit_code_of("script restart exit code=78: yaml: bad config"),
            Some(78)
        );
        assert_eq!(exit_code_of("daemon cmd start exit code=1: boom"), Some(1));
        assert_eq!(exit_code_of("script restart killed by signal: x"), None);
        assert_eq!(exit_code_of("spawn fail: no such file"), None);
        assert_eq!(exit_code_of("exit code=78"), Some(78));
        assert_eq!(exit_code_of("exit code=-1: segv"), Some(-1));
    }

    // issue #47: config-fatal (exit 78) backoff escalates 10→60→300, not the 1→2→4 fast sequence
    #[tokio::test]
    async fn test_config_fatal_exit_78_escalates_backoff() {
        let (_, _, pf, _) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let sf = |_cmd: &str| -> Result<()> {
            Err(anyhow::anyhow!(
                "script restart exit code=78: yaml: bad config"
            ))
        };
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: preset the consecutive-failure count to simulate an already-failed intermediate state (test directly from Unhealthy)
        r.fail_streak = 2;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // scheduling tick (streak=0 → ordinary backoff 1s)
        r.tick(1000, &s, &mut al).await.unwrap();
        assert_eq!(r.restart_after, Some(1001));
        // due: start fails with exit 78 → streak=1, backoff 10s
        r.tick(1001, &s, &mut al).await.unwrap();
        r.complete_pending_exec(1001, &mut al).await;
        assert_eq!(r.config_fail_streak, 1);
        assert_eq!(r.restart_after, Some(1011));
        // due: streak=2, backoff 60s
        r.tick(1011, &s, &mut al).await.unwrap();
        r.complete_pending_exec(1011, &mut al).await;
        assert_eq!(r.config_fail_streak, 2);
        assert_eq!(r.restart_after, Some(1071));
        // due: streak=3, backoff 300s (capped)
        r.tick(1071, &s, &mut al).await.unwrap();
        r.complete_pending_exec(1071, &mut al).await;
        assert_eq!(r.config_fail_streak, 3);
        assert_eq!(r.restart_after, Some(1371));
        // state still Unhealthy (below the cap, not marked Failed)
        assert!(matches!(r.state, State::Unhealthy));
    }

    // issue #47: 5 consecutive exit 78 → marked Failed, restarts stopped
    #[tokio::test]
    async fn test_config_fatal_cap_marks_failed() {
        let (_, _, pf, _) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let sf = |_cmd: &str| -> Result<()> {
            Err(anyhow::anyhow!(
                "script restart exit code=78: yaml: bad config"
            ))
        };
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: preset the consecutive-failure count to simulate an already-failed intermediate state (test directly from Unhealthy)
        r.fail_streak = 2;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        let mut now = 1000u64;
        // scheduling tick, then loop "due → failure streak+1 → schedule escalated" until Failed
        r.tick(now, &s, &mut al).await.unwrap(); // schedule 1s
        now += 1;
        loop {
            r.tick(now, &s, &mut al).await.unwrap(); // failure → streak+1
            r.complete_pending_exec(now, &mut al).await;
            if matches!(r.state, State::Failed) {
                break;
            }
            now = r.restart_after.unwrap(); // jump to the backoff deadline
        }
        assert_eq!(r.config_fail_streak, 5);
        assert!(
            matches!(r.state, State::Failed),
            "5 consecutive exit 78 should mark Failed"
        );
    }

    // issue #47: fast-fail circuit breaker — down within the window after a successful start; 3 consecutive trips the breaker + backoff escalates
    #[tokio::test]
    async fn test_fast_fail_circuit_breaker() {
        let (_, _, pf, _) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let sf = |_cmd: &str| -> Result<()> { Ok(()) };
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: preset the consecutive-failure count to simulate an already-failed intermediate state (test directly from Unhealthy)
        r.fail_streak = 2;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // schedule 1s → due, start succeeds (last_start_ts=1001)
        r.tick(1000, &s, &mut al).await.unwrap();
        r.tick(1001, &s, &mut al).await.unwrap();
        r.complete_pending_exec(1001, &mut al).await;
        assert_eq!(r.last_start_ts, Some(1001));
        assert_eq!(r.fast_fail_streak, 0);
        // round 1: schedule (backoff 2s) → due at 1004, 3s since last start < 10s → streak=1
        r.tick(1002, &s, &mut al).await.unwrap();
        r.tick(1004, &s, &mut al).await.unwrap();
        r.complete_pending_exec(1004, &mut al).await;
        assert_eq!(r.fast_fail_streak, 1);
        // round 2: backoff 4s → due at 1009, 5s since last start → streak=2
        r.tick(1005, &s, &mut al).await.unwrap();
        r.tick(1009, &s, &mut al).await.unwrap();
        r.complete_pending_exec(1009, &mut al).await;
        assert_eq!(r.fast_fail_streak, 2);
        // round 3: backoff 8s → due at 1018, 9s since last start → streak=3, breaker latch set
        r.tick(1010, &s, &mut al).await.unwrap();
        r.tick(1018, &s, &mut al).await.unwrap();
        r.complete_pending_exec(1018, &mut al).await;
        assert_eq!(r.fast_fail_streak, 3);
        assert!(
            r.fast_fail_alerted,
            "reaching the threshold should set the breaker latch"
        );
        // next round's backoff escalates: escalated_delay(3)=300s
        r.tick(1019, &s, &mut al).await.unwrap();
        assert_eq!(
            r.restart_after,
            Some(1319),
            "after tripping, backoff should hit the 300s cap"
        );
    }

    // issue #47: recovery to Healthy resets the circuit-breaker counters
    #[tokio::test]
    async fn test_recovery_resets_circuit_breakers() {
        let (_, probe_cell, pf, sf) = harness(ProbeResult::Healthy { latency_ms: 5 });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Unhealthy;
        // issue #79: preset the consecutive-failure count to simulate an already-failed intermediate state (test directly from Unhealthy)
        r.fail_streak = 2;
        r.fast_fail_streak = 3;
        r.fast_fail_alerted = true;
        r.config_fail_streak = 2;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Healthy));
        assert_eq!(r.fast_fail_streak, 0);
        assert!(!r.fast_fail_alerted);
        assert_eq!(r.config_fail_streak, 0);
        assert_eq!(
            r.fail_streak, 0,
            "recovery should reset the consecutive-failure count"
        );
        let _ = probe_cell;
    }

    // issue #79: below the consecutive-failure threshold (3), no restart is scheduled — busy services' probe jitter does not escalate.
    // issue #153: uses ConnectionRefused (a true-death signal); the Timeout split is covered by
    // mod.rs test_issue153_timeout_streak_gates_restart (threshold 15).
    #[tokio::test]
    async fn test_issue79_streak_gate_no_restart_below_threshold() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // tick 1: streak=1 → Unhealthy + ServiceDown, not scheduled
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Unhealthy));
        assert!(r.restart_after.is_none(), "streak=1 must not schedule");
        assert_eq!(al.alerts_emitted(), 1);
        // tick 2: streak=2 → still not scheduled
        r.tick(1002, &s, &mut al).await.unwrap();
        assert!(r.restart_after.is_none(), "streak=2 must not schedule");
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(r.policy.crash_count(), 0, "no restart must record no crash");
        // tick 3: streak=3 reaches the threshold → schedule (first backoff 1s)
        r.tick(1004, &s, &mut al).await.unwrap();
        assert_eq!(
            r.restart_after,
            Some(1005),
            "3 consecutive failures should schedule a restart"
        );
        assert!(
            calls.lock().unwrap().is_empty(),
            "scheduling tick must not restart"
        );
    }

    // issue #86: operator start.sh process detection — real pgrep path
    #[test]
    fn test_start_sh_process_alive_detection() {
        // no process → false
        assert!(
            !start_sh_process_alive("no-such-repo-xyz-987"),
            "no operator script should not be detected"
        );
        // simulate the operator script: a bash process whose cmdline contains "<repo>/start.sh"
        // (bash -c's $0 argument enters cmdline, pgrep -f can match it)
        // compound command guards against bash's exec optimization (a single command execs into sleep, losing argv)
        let mut child = std::process::Command::new("bash")
            .arg("-c")
            .arg("sleep 3; true")
            .arg("race-probe-repo-xyz/start.sh")
            .spawn()
            .unwrap();
        let mut seen = false;
        for _ in 0..40 {
            if start_sh_process_alive("race-probe-repo-xyz") {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert!(seen, "a live operator script process should be detected");
        // after the process exits, no longer detected (pgrep has millisecond-level lag; poll until cleared)
        let mut cleared = false;
        for _ in 0..60 {
            if !start_sh_process_alive("race-probe-repo-xyz") {
                cleared = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            cleared,
            "after the script exits, it should no longer be detected"
        );
    }

    // issue #136 follow-up: pgrep candidates must pass the shell comm recheck — a bash process true,
    // a non-shell binary (sleep) false (mitigates false positives from agent processes whose cmdline
    // carries the script path text; see the is_shell_process comment).
    #[test]
    fn test_issue136_shell_comm_verification() {
        let mut bash = std::process::Command::new("bash")
            // compound command guards against bash's exec optimization: a single command gets exec-replaced by sleep, comm becomes sleep
            .args(["-c", "sleep 30; true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn bash");
        let mut other = std::process::Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let bash_res = is_shell_process(&bash.id().to_string());
        let other_res = is_shell_process(&other.id().to_string());
        let _ = bash.kill();
        let _ = other.kill();
        let _ = bash.wait();
        let _ = other.wait();
        assert!(bash_res, "a bash process should be recognized as a shell");
        assert!(
            !other_res,
            "a sleep process should not be recognized as a shell"
        );
    }

    // issue #86 production patch: an operator who cd's into the repo and uses ./start.sh (cmdline without the repo
    // prefix) must also be detected — phase 2 cwd attribution path
    #[test]
    fn test_start_sh_alive_relative_cwd_invocation() {
        struct EnvGuard {
            key: &'static str,
            prev: Option<String>,
        }
        impl EnvGuard {
            fn set(key: &'static str, val: &str) -> Self {
                let prev = std::env::var(key).ok();
                // SAFETY: only this test reads/writes this key; restored in Drop, no leak on panic.
                unsafe { std::env::set_var(key, val) };
                Self { key, prev }
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: same as above.
                match &self.prev {
                    Some(v) => unsafe { std::env::set_var(self.key, v) },
                    None => unsafe { std::env::remove_var(self.key) },
                }
            }
        }
        let base = std::env::temp_dir().join("race-cwd-repo-base-xyz");
        let repo = base.join("fake-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let _guard = EnvGuard::set("FUSION_ROOT", base.to_str().unwrap());
        // simulate the operator using ./start.sh inside the repo dir — cmdline has no repo prefix,
        // compound command guards against bash's exec optimization dropping argv
        let mut child = std::process::Command::new("bash")
            .arg("-c")
            .arg("sleep 3; true")
            .arg("./start.sh")
            .current_dir(&repo)
            .spawn()
            .unwrap();
        let mut seen = false;
        for _ in 0..60 {
            if start_sh_process_alive("fake-repo") {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        // clean up the temp directory (keep only final artifacts beyond test outputs)
        let _ = std::fs::remove_dir_all(&base);
        assert!(
            seen,
            "a cwd-relative ./start.sh invocation should be detected by phase 2"
        );
    }

    // issue #86: when the operator's start.sh is running at a due restart → skip this round (no invoke_start,
    // no crash recorded, restart_after retained); the next round restarts normally after the script exits.
    #[tokio::test]
    async fn test_operator_start_sh_running_skips_auto_restart() {
        let (calls, _, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        });
        let mut d = def();
        d.repo_dir = "race-flow-repo-xyz".into();
        let mut r = ServiceRunner::new(d, "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let mut child = std::process::Command::new("bash")
            .arg("-c")
            .arg("sleep 2; true")
            .arg("race-flow-repo-xyz/start.sh")
            .spawn()
            .unwrap();
        // wait until pgrep can see it (bash spawn is millisecond-scale)
        for _ in 0..40 {
            if start_sh_process_alive("race-flow-repo-xyz") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // preset streak: tick1 (Healthy→Unhealthy records the outage) → tick2 (streak reaches the threshold, scheduled,
        // first backoff 1s, due=1002) → tick3 due, should be held back by the operator script gate
        r.fail_streak = 2;
        r.tick(1000, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Unhealthy));
        r.tick(1001, &s, &mut al).await.unwrap();
        assert_eq!(
            r.restart_after,
            Some(1002),
            "3 consecutive failures should schedule a restart"
        );
        r.tick(1002, &s, &mut al).await.unwrap();
        // key points: no invoke_start, no crash recorded, schedule retained (rechecked next tick)
        assert!(
            calls.lock().unwrap().is_empty(),
            "must not jump the gun on auto-restart while the operator script is running"
        );
        assert_eq!(
            r.policy.crash_count(),
            0,
            "an operator action is not a crash, must not record one"
        );
        assert!(
            r.restart_after.is_some(),
            "the schedule should be retained and rechecked next tick"
        );
        assert!(
            r.operator_race_notified,
            "the first skip should set the alert latch"
        );
        // wait for the operator script to exit (sleep 2 elapses) → the next scheduled round restarts normally when due
        let _ = child.wait();
        let mut gone = false;
        for _ in 0..60 {
            if !start_sh_process_alive("race-flow-repo-xyz") {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(gone, "the operator script should have exited");
        r.tick(1002, &s, &mut al).await.unwrap();
        r.tick(1003, &s, &mut al).await.unwrap();
        r.complete_pending_exec(1003, &mut al).await;
        assert!(
            !calls.lock().unwrap().is_empty(),
            "auto-restart should resume normally after the script exits"
        );
    }

    // issue #79: in flapping (fail-fail-recover-fail-fail), a successful probe in between resets the count, never escalating
    #[tokio::test]
    async fn test_issue79_flap_recovery_resets_streak() {
        let (calls, probe_cell, pf, sf) = harness(ProbeResult::Unhealthy {
            reason: UnhealthyReason::Timeout,
        });
        let mut r = ServiceRunner::new(def(), "restart", pf, sf, RestartPolicy::new(300, 5, 30));
        r.state = State::Healthy;
        let s = store();
        let mut al = Alerter::new(300, String::new());
        // two failures (streak 1→2)
        r.tick(1000, &s, &mut al).await.unwrap();
        r.tick(1002, &s, &mut al).await.unwrap();
        assert_eq!(r.fail_streak, 2);
        // recovery — resets the count + clears the stale schedule
        *probe_cell.lock().unwrap() = ProbeResult::Healthy { latency_ms: 8 };
        r.tick(1004, &s, &mut al).await.unwrap();
        assert!(matches!(r.state, State::Healthy));
        assert_eq!(r.fail_streak, 0);
        // two more failures — streak counts from 0 again, not scheduled
        *probe_cell.lock().unwrap() = ProbeResult::Unhealthy {
            reason: UnhealthyReason::Timeout,
        };
        r.tick(1006, &s, &mut al).await.unwrap();
        r.tick(1008, &s, &mut al).await.unwrap();
        assert_eq!(r.fail_streak, 2);
        assert!(
            r.restart_after.is_none(),
            "flapping must not escalate to scheduling"
        );
        assert!(
            calls.lock().unwrap().is_empty(),
            "flapping must not restart"
        );
        assert_eq!(r.policy.crash_count(), 0);
    }
}
