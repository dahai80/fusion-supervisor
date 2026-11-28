use crate::alert::{Alert, AlertKind, Alerter};
use crate::store::HistoryStore;
use std::sync::Arc;
use std::time::Duration;

// issue #12: login-latency tripwire — automates the MANUAL policy in ~/actions-runner/RUNNERS.md
// (fusion#8 login-outage postmortem). During CI-runner activity windows, probe the gateway/identity
// front doors; latency above the threshold for N consecutive samples fires ONE alert per breach
// episode through the standard alert channel (Alerter → webhook + event table). Every probed sample
// lands in the history.db tripwire_sample table for postmortem latency graphs.
//
// Integration choice: a resident loop task spawned by run_daemon, detached from the core lock
// (same pattern as the detached backup task — HTTP probes with a 5s timeout must never hold the
// core lock). `fusion-sv tripwire` runs the same loop standalone for drills / launchd hosting.

// Issue-default values (mirrored in config.rs serde defaults; kept here for the loop's own docs).
pub const DEFAULT_WINDOW_SEC: u64 = 30;
pub const DEFAULT_THRESHOLD_MS: u64 = 2000;
pub const DEFAULT_CONSECUTIVE: u32 = 3;
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;
pub const DEFAULT_GATEWAY_URL: &str = "http://127.0.0.1:11432/healthz";
pub const DEFAULT_IDENTITY_URL: &str = "http://127.0.0.1:11470/healthz";
pub const DEFAULT_CI_PROCESS: &str = "Runner.Listener";

// Alert service name in the event table / webhook payloads (not a managed service — the
// tripwire watches the login chain as a whole, gateway + identity).
pub const TRIPWIRE_SERVICE: &str = "login-tripwire";

#[derive(Debug, Clone)]
pub struct TripwireConfig {
    pub window_sec: u64,
    pub threshold_ms: u64,
    pub consecutive: u32,
    pub timeout_ms: u64,
    pub gateway_url: String,
    pub identity_url: String,
    pub ci_process: String,
}

impl TripwireConfig {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            window_sec: cfg.tripwire_window_sec,
            threshold_ms: cfg.tripwire_threshold_ms,
            consecutive: cfg.tripwire_consecutive,
            timeout_ms: cfg.tripwire_timeout_ms,
            gateway_url: cfg.tripwire_gateway_url.clone(),
            identity_url: cfg.tripwire_identity_url.clone(),
            ci_process: cfg.tripwire_ci_process.clone(),
        }
    }
}

// Episode state machine state. One TripwireState per loop instance; a daemon restart
// resets it (fresh state after restart = at most one extra alert after a restart mid-episode).
#[derive(Debug, Default)]
pub struct TripwireState {
    consecutive_breaches: u32,
    // true from the moment the episode alert fires until the first clean sample —
    // guarantees one alert per breach episode, not one per sample.
    episode_active: bool,
    #[allow(dead_code)]
    last_check_ts: u64,
    // previous window's CI activity (drives the once-per-transition idle/active log line;
    // None until the first check so the starting state is logged too)
    ci_was_active: Option<bool>,
}

// Injected latency probe: url + timeout → measured millis, None = probe error
// (refused/timeout — a front door that does not answer is a breach by definition).
pub type LatencyFn = Arc<
    dyn Fn(
            String,
            Duration,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<u64>> + Send>>
        + Send
        + Sync,
>;

// Injected CI-activity detection (prod = pgrep; tests inject synthetic results).
pub type CiActiveFn = Arc<dyn Fn() -> bool + Send + Sync>;

// Injected unix-seconds clock (prod = SystemTime; tests inject a counter so per-window
// samples land on distinct ts rows even at window_sec = 0).
pub type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

// Pure breach rule: either endpoint over threshold = breach sample. None (probe error)
// counts as a breach — an unanswering login chain is exactly what this tripwire guards.
pub fn sample_breach(
    cfg: &TripwireConfig,
    gateway_ms: Option<u64>,
    identity_ms: Option<u64>,
) -> bool {
    let over = |v: Option<u64>| match v {
        Some(ms) => ms > cfg.threshold_ms,
        None => true,
    };
    over(gateway_ms) || over(identity_ms)
}

// Pure episode state machine. Returns true exactly when an alert must fire:
// - clean sample → counter reset + episode closed, never fires;
// - breach → counter grows; on reaching cfg.consecutive (and not already alerted in this
//   episode) fires once, resets the counter and latches the episode until the next clean sample.
pub fn evaluate_breach(state: &mut TripwireState, cfg: &TripwireConfig, breach: bool) -> bool {
    if !breach {
        state.consecutive_breaches = 0;
        state.episode_active = false;
        return false;
    }
    state.consecutive_breaches += 1;
    if state.episode_active {
        return false;
    }
    if state.consecutive_breaches >= cfg.consecutive {
        state.episode_active = true;
        state.consecutive_breaches = 0;
        return true;
    }
    false
}

// Production latency probe: time-to-response of a GET (any HTTP status counts — identity's
// /healthz answers 401 unauthenticated, the response still measures the auth chain front door).
pub async fn probe_latency(url: &str, timeout: Duration) -> Option<u64> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_default();
    let start = std::time::Instant::now();
    match client.get(url).send().await {
        Ok(_) => Some(start.elapsed().as_millis() as u64),
        Err(e) => {
            tracing::warn!(url, err = %e, "tripwire probe failed (counted as breach)");
            None
        }
    }
}

// Production CI-activity detection: any process whose full command line matches the pattern
// (GitHub Actions self-hosted runner: Runner.Listener). pgrep failure → treated as idle (fail-safe:
// no probes, no alerts from a broken detector) with a WARN for visibility.
pub fn ci_active_by_pgrep(pattern: &str) -> bool {
    match std::process::Command::new("pgrep")
        .arg("-f")
        .arg(pattern)
        .output()
    {
        Ok(out) => out.status.success() && !out.stdout.is_empty(),
        Err(e) => {
            tracing::warn!(pattern, err = %e, "tripwire pgrep failed (treated as idle)");
            false
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ms_txt(v: Option<u64>) -> String {
    match v {
        Some(ms) => format!("{ms}ms"),
        None => "err".to_string(),
    }
}

// One tripwire check: record the sample, advance the episode state machine, fire the alert
// when the episode threshold is crossed. Unit-test entry point (mem store + injected values).
#[allow(clippy::too_many_arguments)]
pub async fn tripwire_once(
    state: &mut TripwireState,
    cfg: &TripwireConfig,
    store: &HistoryStore,
    alerter: &mut Alerter,
    ts: u64,
    gateway_ms: Option<u64>,
    identity_ms: Option<u64>,
    ci_active: bool,
) {
    let breach = sample_breach(cfg, gateway_ms, identity_ms);
    if let Err(e) = store.record_tripwire_sample(ts, gateway_ms, identity_ms, ci_active, breach) {
        tracing::warn!(err = %e, "tripwire sample write fail (non-fatal)");
    }
    tracing::info!(
        ts,
        gateway_ms = ms_txt(gateway_ms),
        identity_ms = ms_txt(identity_ms),
        ci_active,
        breach,
        "tripwire sample"
    );
    if !evaluate_breach(state, cfg, breach) {
        return;
    }
    let detail = format!(
        "login latency breach: gateway={} identity={} threshold={}ms for {} consecutive samples (CI active); policy ~/actions-runner/RUNNERS.md: inspect newest CI job",
        ms_txt(gateway_ms),
        ms_txt(identity_ms),
        cfg.threshold_ms,
        cfg.consecutive
    );
    tracing::warn!(svc = TRIPWIRE_SERVICE, detail = %detail, "tripwire breach episode");
    if let Err(e) = alerter
        .alert(Alert {
            service: TRIPWIRE_SERVICE.to_string(),
            kind: AlertKind::LoginLatencyBreach,
            detail,
            ts,
        })
        .await
    {
        tracing::warn!(err = %e, "tripwire alert emit fail (non-fatal)");
    }
}

// Production loop: real pgrep CI detection + real HTTP latency probes.
pub async fn run_tripwire_loop(
    cfg: TripwireConfig,
    store: Arc<HistoryStore>,
    alert_url: String,
    alert_dedup_sec: u64,
) {
    let ci_fn: CiActiveFn = {
        let pattern = cfg.ci_process.clone();
        Arc::new(move || ci_active_by_pgrep(&pattern))
    };
    let probe_fn: LatencyFn =
        Arc::new(|url, timeout| Box::pin(async move { probe_latency(&url, timeout).await }));
    let alerter = Alerter::new(alert_dedup_sec, alert_url)
        .with_event_sink(crate::supervisor::make_event_sink(store.clone()));
    let now_fn: NowFn = Arc::new(unix_now);
    run_tripwire_loop_with(cfg, store, alerter, ci_fn, probe_fn, now_fn).await;
}

// Loop body with injected detection/probing/clock (unit tests drive this directly).
pub async fn run_tripwire_loop_with(
    cfg: TripwireConfig,
    store: Arc<HistoryStore>,
    mut alerter: Alerter,
    ci_fn: CiActiveFn,
    probe_fn: LatencyFn,
    now_fn: NowFn,
) {
    tracing::info!(
        window_sec = cfg.window_sec,
        threshold_ms = cfg.threshold_ms,
        consecutive = cfg.consecutive,
        timeout_ms = cfg.timeout_ms,
        gateway = %cfg.gateway_url,
        identity = %cfg.identity_url,
        ci_process = %cfg.ci_process,
        "login-latency tripwire loop started"
    );
    let mut state = TripwireState::default();
    let timeout = Duration::from_millis(cfg.timeout_ms);
    loop {
        tokio::time::sleep(Duration::from_secs(cfg.window_sec)).await;
        let now = (now_fn)();
        let ci = (ci_fn)();
        // Log once per window transition (and once at startup), not once per idle window.
        if state.ci_was_active != Some(ci) {
            if ci {
                tracing::info!("tripwire: CI runner activity detected, probing enabled");
            } else {
                tracing::info!("tripwire: CI runner idle, probing disabled");
            }
            state.ci_was_active = Some(ci);
        }
        state.last_check_ts = now;
        if !ci {
            continue;
        }
        let gateway_ms = (probe_fn)(cfg.gateway_url.clone(), timeout).await;
        let identity_ms = (probe_fn)(cfg.identity_url.clone(), timeout).await;
        tripwire_once(
            &mut state,
            &cfg,
            &store,
            &mut alerter,
            now,
            gateway_ms,
            identity_ms,
            ci,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TripwireConfig {
        TripwireConfig {
            window_sec: DEFAULT_WINDOW_SEC,
            threshold_ms: DEFAULT_THRESHOLD_MS,
            consecutive: DEFAULT_CONSECUTIVE,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            gateway_url: DEFAULT_GATEWAY_URL.to_string(),
            identity_url: DEFAULT_IDENTITY_URL.to_string(),
            ci_process: DEFAULT_CI_PROCESS.to_string(),
        }
    }

    fn open_mem() -> HistoryStore {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        HistoryStore::open(tmp.path()).unwrap()
    }

    #[test]
    fn test_sample_breach_threshold_and_error() {
        let c = cfg();
        // healthy round: both endpoints fast
        assert!(!sample_breach(&c, Some(50), Some(180)));
        // exactly at threshold = not a breach (strictly greater)
        assert!(!sample_breach(&c, Some(2000), Some(180)));
        // either endpoint over threshold = breach
        assert!(sample_breach(&c, Some(2001), Some(180)));
        assert!(sample_breach(&c, Some(50), Some(5000)));
        // probe error (None) = breach
        assert!(sample_breach(&c, None, Some(180)));
        assert!(sample_breach(&c, Some(50), None));
    }

    #[test]
    fn test_evaluate_breach_fires_once_per_episode() {
        let mut st = TripwireState::default();
        let c = cfg();
        assert!(!evaluate_breach(&mut st, &c, true), "breach 1: not yet");
        assert!(!evaluate_breach(&mut st, &c, true), "breach 2: not yet");
        assert!(
            evaluate_breach(&mut st, &c, true),
            "breach 3: episode fires"
        );
        // sustained breach: latched, never re-fires within the episode
        assert!(!evaluate_breach(&mut st, &c, true), "breach 4: latched");
        assert!(!evaluate_breach(&mut st, &c, true), "breach 5: latched");
        // clean sample closes the episode and resets the counter
        assert!(!evaluate_breach(&mut st, &c, false), "clean sample");
        // new episode needs a fresh consecutive run
        assert!(!evaluate_breach(&mut st, &c, true), "new episode breach 1");
        assert!(!evaluate_breach(&mut st, &c, true), "new episode breach 2");
        assert!(
            evaluate_breach(&mut st, &c, true),
            "new episode breach 3 fires"
        );
    }

    #[test]
    fn test_evaluate_breach_intermittent_resets() {
        let mut st = TripwireState::default();
        let c = cfg();
        // 2 breaches, 1 clean, 2 breaches → never 3 consecutive → no fire
        assert!(!evaluate_breach(&mut st, &c, true));
        assert!(!evaluate_breach(&mut st, &c, true));
        assert!(!evaluate_breach(&mut st, &c, false));
        assert!(!evaluate_breach(&mut st, &c, true));
        assert!(!evaluate_breach(&mut st, &c, true));
        assert!(evaluate_breach(&mut st, &c, true), "third in a row fires");
    }

    // tripwire_once: records every sample, fires exactly one alert on the 3rd consecutive
    // breach, persists the event row, latches under sustained breach.
    #[tokio::test]
    async fn test_tripwire_once_records_and_alerts() {
        let store = open_mem();
        let mut alerter = Alerter::new(300, String::new());
        let mut st = TripwireState::default();
        let c = cfg();
        tripwire_once(
            &mut st,
            &c,
            &store,
            &mut alerter,
            1000,
            Some(3000),
            Some(100),
            true,
        )
        .await;
        tripwire_once(
            &mut st,
            &c,
            &store,
            &mut alerter,
            1030,
            Some(4000),
            None,
            true,
        )
        .await;
        assert_eq!(alerter.alerts_emitted(), 0, "no alert before 3 consecutive");
        tripwire_once(
            &mut st,
            &c,
            &store,
            &mut alerter,
            1060,
            Some(2500),
            Some(6000),
            true,
        )
        .await;
        assert_eq!(
            alerter.alerts_emitted(),
            1,
            "3rd consecutive breach fires once"
        );
        // sustained breach: sample recorded, no second alert
        tripwire_once(
            &mut st,
            &c,
            &store,
            &mut alerter,
            1090,
            Some(9999),
            Some(9999),
            true,
        )
        .await;
        assert_eq!(
            alerter.alerts_emitted(),
            1,
            "episode latch: no second alert"
        );
        // clean sample closes the episode
        tripwire_once(
            &mut st,
            &c,
            &store,
            &mut alerter,
            1120,
            Some(10),
            Some(20),
            true,
        )
        .await;
        tripwire_once(
            &mut st,
            &c,
            &store,
            &mut alerter,
            1150,
            Some(5000),
            Some(5000),
            true,
        )
        .await;
        tripwire_once(
            &mut st,
            &c,
            &store,
            &mut alerter,
            1180,
            Some(5000),
            Some(5000),
            true,
        )
        .await;
        tripwire_once(
            &mut st,
            &c,
            &store,
            &mut alerter,
            1210,
            Some(5000),
            Some(5000),
            true,
        )
        .await;
        assert_eq!(
            alerter.alerts_emitted(),
            2,
            "second episode fires after recovery"
        );
        // every sample landed in the history table with the right breach flags
        // (3 breach + 1 sustained + 1 clean + 3 new-episode = 8 samples)
        let rows = store.recent_tripwire_samples(100).unwrap();
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[0].ts, 1000);
        assert_eq!(rows[0].gateway_ms, Some(3000));
        assert_eq!(rows[0].identity_ms, Some(100));
        assert!(rows[0].breach);
        assert!(rows[0].ci_active);
        assert!(!rows[4].breach, "clean sample flagged false");
        assert_eq!(rows[7].ts, 1210);
        // the fired alert persisted through the event sink path (Alerter without sink →
        // assert via the emitted count above; event-table assertion in the sink test below)
        let _ = alerter.alerts_rate_limited();
    }

    // Full alert-chain check: the tripwire's own sink writes the LoginLatencyBreach event row.
    #[tokio::test]
    async fn test_tripwire_alert_persists_event_row() {
        let store = std::sync::Arc::new(open_mem());
        let mut alerter = Alerter::new(300, String::new())
            .with_event_sink(crate::supervisor::make_event_sink(store.clone()));
        let mut st = TripwireState::default();
        let c = cfg();
        for ts in [1000, 1030, 1060] {
            tripwire_once(
                &mut st,
                &c,
                &store,
                &mut alerter,
                ts,
                Some(9000),
                Some(9000),
                true,
            )
            .await;
        }
        let ev = store.recent_events(TRIPWIRE_SERVICE, 10).unwrap();
        assert_eq!(ev.len(), 1, "one event row per episode");
        assert_eq!(ev[0].kind, "LoginLatencyBreach");
        assert_eq!(ev[0].severity, "warn");
        assert!(ev[0].detail.contains("gateway=9000ms"));
    }

    // Idle loop: no CI activity → no probing, no samples, no alerts (loop spins with window 0).
    #[tokio::test]
    async fn test_loop_idle_records_nothing() {
        let store = std::sync::Arc::new(open_mem());
        let c = TripwireConfig {
            window_sec: 0,
            ..cfg()
        };
        let alerter = Alerter::new(300, String::new());
        let ci_fn: CiActiveFn = Arc::new(|| false);
        let probe_fn: LatencyFn = Arc::new(|_, _| Box::pin(async { Some(1) }));
        let now_fn: NowFn = Arc::new(|| 1000);
        let handle = tokio::spawn(run_tripwire_loop_with(
            c,
            store.clone(),
            alerter,
            ci_fn,
            probe_fn,
            now_fn,
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        assert!(
            store.recent_tripwire_samples(10).unwrap().is_empty(),
            "idle windows must not record samples"
        );
    }

    // Active loop with a deterministic breaching probe: 3 windows later exactly one alert,
    // idle transitions log once (asserted indirectly: samples exist and count is exact).
    #[tokio::test]
    async fn test_loop_active_breach_fires_one_alert() {
        let store = std::sync::Arc::new(open_mem());
        let c = TripwireConfig {
            window_sec: 0,
            ..cfg()
        };
        let alerter = Alerter::new(300, String::new())
            .with_event_sink(crate::supervisor::make_event_sink(store.clone()));
        let ci_fn: CiActiveFn = Arc::new(|| true);
        // deterministic breach: gateway error (None) every window
        let probe_fn: LatencyFn = Arc::new(|url, _| {
            Box::pin(async move {
                if url == DEFAULT_GATEWAY_URL {
                    None
                } else {
                    Some(10)
                }
            })
        });
        // monotonic counter clock: distinct ts per window even at window_sec = 0
        let clock = std::sync::atomic::AtomicU64::new(1000);
        let now_fn: NowFn =
            Arc::new(move || clock.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let handle = tokio::spawn(run_tripwire_loop_with(
            c,
            store.clone(),
            alerter,
            ci_fn,
            probe_fn,
            now_fn,
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();
        let rows = store.recent_tripwire_samples(100).unwrap();
        assert!(
            rows.len() >= 3,
            "active windows probe every window (got {})",
            rows.len()
        );
        assert!(rows.iter().all(|r| r.breach), "all windows breach");
        assert_eq!(
            store.recent_events(TRIPWIRE_SERVICE, 100).unwrap().len(),
            1,
            "exactly one alert for the whole sustained-breach episode"
        );
    }

    // CI detection: real pgrep against a spawned marker process (no live services involved).
    // The marker rides in the `tail -f <file>` command line — shell `-c` strings are unreliable
    // (sh execs simple commands and drops the marker from argv).
    #[test]
    fn test_ci_active_by_pgrep() {
        let dir = tempfile::TempDir::new().unwrap();
        let marker_path = dir.path().join("fusion12-tripwire-test-marker");
        std::fs::write(&marker_path, b"marker").unwrap();
        let pattern = marker_path.to_str().unwrap().to_string();
        assert!(
            !ci_active_by_pgrep(&pattern),
            "marker process not running yet"
        );
        let mut child = std::process::Command::new("tail")
            .arg("-f")
            .arg(&marker_path)
            .spawn()
            .unwrap();
        // give the spawn a moment to appear in the process table
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            ci_active_by_pgrep(&pattern),
            "marker process must be detected"
        );
        assert!(!ci_active_by_pgrep("fusion12-no-such-marker-xyz"));
        let _ = child.kill();
        let _ = child.wait();
    }
}
