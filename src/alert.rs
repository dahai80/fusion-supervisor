use std::collections::HashMap;
use std::sync::Arc;

// issue #37: event persistence sink — every actually-emitted alert is synchronously
// written to the history.db event table.
// The (service, kind, detail, ts, plane, severity) tuple decides persistence details.
pub type EventSink = Arc<dyn Fn(&str, &str, &str, u64, &str, &str) + Send + Sync>;

// Alerts: ServiceDown is deduped (5min per service), the rest emit every time.
// sink = tracing WARN + optional webhook.
// Webhook delivery retries 3 times on failure then falls back to log-only,
// never affecting supervision (spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    ServiceDown,
    ServiceBack,
    RestartFailed,
    RestartCapHit,
    // compose container docker state changed while failing (new event, not deduped)
    ContainerStateChange,
    // compose container consecutive failures reached escalation threshold (crash-loop, not deduped)
    ContainerCrashLoop,
    // issue #9: service drain completed (stopped after traffic removal, normal event)
    ServiceDrained,
    // issue #9: rollout prewarm failed (did not reach Healthy within timeout after start)
    RolloutPrewarmFailed,
    // issue #10: backup failed (target unset/unwritable/docker exec failure)
    BackupFailed,
    // issue #47: config-fatal exit (exit 78, EX_CONFIG) — restart cannot help, needs manual config fix
    ServiceConfigFatal,
    // issue #47: fast-fail circuit breaker (dies seconds after a successful start, repeatedly)
    FastFailLoop,
    // issue #112: crash/start-retry backoff flow actually restarted the service — audit event,
    // not deduped. detail carries old/new pid; see the adjacent ServiceDown row for the trigger.
    AutoRestart,
    // issue #137: scheduled job execution failed (nonzero exit / timeout kill / spawn failure). Not deduped.
    JobFailed,
    // issue #139: kind label when externally-ingested alerts are re-emitted via webhook (the event itself is
    // persisted directly by the ingest path; only re-emitted here, sink always empty — no duplicate events).
    External,
    // issue #12: login-latency tripwire — gateway/identity latency above threshold for N consecutive
    // samples during a CI-runner activity window. Episode-latched at the source (one alert per breach
    // episode), not deduped here.
    LoginLatencyBreach,
}

impl AlertKind {
    pub fn label(&self) -> &'static str {
        match self {
            AlertKind::ServiceDown => "ServiceDown",
            AlertKind::ServiceBack => "ServiceBack",
            AlertKind::RestartFailed => "RestartFailed",
            AlertKind::RestartCapHit => "RestartCapHit",
            AlertKind::ContainerStateChange => "ContainerStateChange",
            AlertKind::ContainerCrashLoop => "ContainerCrashLoop",
            AlertKind::ServiceDrained => "ServiceDrained",
            AlertKind::RolloutPrewarmFailed => "RolloutPrewarmFailed",
            AlertKind::BackupFailed => "BackupFailed",
            AlertKind::ServiceConfigFatal => "ServiceConfigFatal",
            AlertKind::FastFailLoop => "FastFailLoop",
            AlertKind::AutoRestart => "AutoRestart",
            AlertKind::JobFailed => "JobFailed",
            AlertKind::External => "External",
            AlertKind::LoginLatencyBreach => "LoginLatencyBreach",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Alert {
    pub service: String,
    pub kind: AlertKind,
    pub detail: String,
    pub ts: u64,
}

// issue #142: per (service, kind) sliding-window rate limit — max 10 per key within 60s,
// beyond that suppressed. Defense in depth: a state-machine bug must not flood the
// webhook / event table (ServiceDown dedup only guards one kind; the rest rely on
// correct edge triggering at the source).
const RATE_WINDOW_SEC: u64 = 60;
const RATE_MAX_PER_WINDOW: usize = 10;

pub struct Alerter {
    dedup_sec: u64,
    last_down: HashMap<String, u64>,
    // issue #142: (service, kind) → emitted timestamps within the window (ascending)
    rate_window: HashMap<(String, &'static str), Vec<u64>>,
    webhook_url: String,
    // issue #37: alert → event table persistence sink (optional; unset = no persistence, tests)
    event_sink: Option<EventSink>,
    // issue #37: actually emitted (post-dedup) alert count (/metrics fusion_sv_alerts_total)
    alerts_emitted: u64,
    // issue #142: alerts suppressed by rate limiting (debugging visibility)
    alerts_rate_limited: u64,
}

impl Alerter {
    pub fn new(dedup_sec: u64, webhook_url: String) -> Self {
        Self {
            dedup_sec,
            last_down: HashMap::new(),
            rate_window: HashMap::new(),
            webhook_url,
            event_sink: None,
            alerts_emitted: 0,
            alerts_rate_limited: 0,
        }
    }

    // issue #37: inject the event persistence sink (builder style, called at the production construction site)
    pub fn with_event_sink(mut self, sink: EventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    pub fn alerts_emitted(&self) -> u64 {
        self.alerts_emitted
    }

    // issue #142: alerts suppressed by rate limiting, cumulative
    pub fn alerts_rate_limited(&self) -> u64 {
        self.alerts_rate_limited
    }

    // issue #142: pure check — emitted count for the same (service, kind) window is below the cap
    fn rate_ok(&self, service: &str, kind: &AlertKind, ts: u64) -> bool {
        match self.rate_window.get(&(service.to_string(), kind.label())) {
            None => true,
            Some(stamps) => {
                stamps
                    .iter()
                    .filter(|&&t| ts.saturating_sub(t) < RATE_WINDOW_SEC)
                    .count()
                    < RATE_MAX_PER_WINDOW
            }
        }
    }

    // issue #37: kind → severity mapping (crash-loop escalation=error, recovery/normal events=info, rest warn)
    fn severity_of(kind: &AlertKind) -> &'static str {
        match kind {
            AlertKind::ContainerCrashLoop => "error",
            // issue #47: config-fatal/fast-fail circuit breaker are escalation events; #137 job failure likewise
            AlertKind::ServiceConfigFatal | AlertKind::FastFailLoop | AlertKind::JobFailed => {
                "error"
            }
            AlertKind::ServiceBack | AlertKind::ServiceDrained | AlertKind::AutoRestart => "info",
            _ => "warn",
        }
    }

    // Pure check: ServiceDown within the dedup window = suppressed; additionally layered
    // with the per (service, kind) rate window.
    pub fn should_emit(&self, service: &str, kind: &AlertKind, ts: u64) -> bool {
        if *kind == AlertKind::ServiceDown {
            let dedup_passed = match self.last_down.get(service) {
                None => true,
                Some(&last) => ts.saturating_sub(last) > self.dedup_sec,
            };
            if !dedup_passed {
                return false;
            }
        }
        self.rate_ok(service, kind, ts)
    }

    // Record as emitted (used by tests and internally by alert()).
    pub fn record_emitted(&mut self, service: &str, kind: &AlertKind, ts: u64) {
        if *kind == AlertKind::ServiceDown {
            self.last_down.insert(service.to_string(), ts);
        }
        let stamps = self
            .rate_window
            .entry((service.to_string(), kind.label()))
            .or_default();
        stamps.push(ts);
        stamps.retain(|&t| ts.saturating_sub(t) < RATE_WINDOW_SEC);
    }

    pub async fn alert(&mut self, alert: Alert) -> anyhow::Result<()> {
        if !self.should_emit(&alert.service, &alert.kind, alert.ts) {
            if self.rate_ok(&alert.service, &alert.kind, alert.ts) {
                tracing::debug!(svc = %alert.service, kind = alert.kind.label(), "alert dedup skip");
            } else {
                // issue #142: rate-limit suppression — single WARN for visibility (debugging clue),
                // never reaches webhook/event table
                self.alerts_rate_limited += 1;
                tracing::warn!(
                    svc = %alert.service,
                    kind = alert.kind.label(),
                    "alert rate limited ({} per {}s window cap)",
                    RATE_MAX_PER_WINDOW,
                    RATE_WINDOW_SEC
                );
            }
            return Ok(());
        }
        // CrashLoop / config-fatal / fast-fail circuit breaker escalate to ERROR, the rest WARN (ops filters escalation events)
        let is_escalation = matches!(
            alert.kind,
            AlertKind::ContainerCrashLoop
                | AlertKind::ServiceConfigFatal
                | AlertKind::FastFailLoop
                | AlertKind::JobFailed
        );
        if is_escalation {
            tracing::error!(
                svc = %alert.service,
                kind = alert.kind.label(),
                detail = %alert.detail,
                "ALERT (crash-loop escalation)"
            );
        } else {
            tracing::warn!(
                svc = %alert.service,
                kind = alert.kind.label(),
                detail = %alert.detail,
                "ALERT"
            );
        }
        self.record_emitted(&alert.service, &alert.kind, alert.ts);
        // issue #37: actually emitted alert → count + event persistence (only reached after dedup passes, no duplicates)
        self.alerts_emitted += 1;
        if let Some(sink) = &self.event_sink {
            sink(
                &alert.service,
                alert.kind.label(),
                &alert.detail,
                alert.ts,
                "native",
                Self::severity_of(&alert.kind),
            );
        }
        if !self.webhook_url.is_empty()
            && let Err(e) = self.send_webhook(&alert).await
        {
            tracing::warn!(err = %e, "webhook send fail (falling back to log-only, supervision unaffected)");
        }
        Ok(())
    }

    async fn send_webhook(&self, alert: &Alert) -> anyhow::Result<()> {
        let body = serde_json::json!({
            "service": alert.service,
            "kind": alert.kind.label(),
            "detail": alert.detail,
            "ts": alert.ts,
        });
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap_or_default();
        let mut last_err = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                // backoff between retries — old implementation fired 3 times with zero gap,
                // hitting a dead endpoint with an instant triple burst
                tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
            }
            match client.post(&self.webhook_url).json(&body).send().await {
                Ok(r) if r.status().is_success() => {
                    tracing::debug!(attempt, "webhook sent");
                    return Ok(());
                }
                Ok(r) => {
                    last_err = Some(format!("status {}", r.status()));
                }
                Err(e) => last_err = Some(e.to_string()),
            }
        }
        anyhow::bail!(last_err.unwrap_or_else(|| "webhook unknown fail".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup_down_within_window() {
        let mut a = Alerter::new(300, String::new());
        assert!(a.should_emit("fusion-rag", &AlertKind::ServiceDown, 1000));
        // record as emitted
        a.record_emitted("fusion-rag", &AlertKind::ServiceDown, 1000);
        // no re-emit within 5min
        assert!(!a.should_emit("fusion-rag", &AlertKind::ServiceDown, 1000 + 299));
        // 300s boundary: still suppressed (<= window)
        assert!(!a.should_emit("fusion-rag", &AlertKind::ServiceDown, 1000 + 300));
        // 301s: released
        assert!(a.should_emit("fusion-rag", &AlertKind::ServiceDown, 1000 + 301));
    }

    #[test]
    fn test_dedup_per_service_isolated() {
        let mut a = Alerter::new(300, String::new());
        a.record_emitted("fusion-rag", &AlertKind::ServiceDown, 1000);
        // another service is unaffected
        assert!(a.should_emit("fusion-mlx", &AlertKind::ServiceDown, 1000 + 100));
    }

    #[test]
    fn test_other_kinds_always_emit() {
        let mut a = Alerter::new(300, String::new());
        // ServiceBack / RestartFailed / RestartCapHit are not deduped, emit every time
        assert!(a.should_emit("fusion-rag", &AlertKind::ServiceBack, 1000));
        a.record_emitted("fusion-rag", &AlertKind::ServiceBack, 1000);
        assert!(a.should_emit("fusion-rag", &AlertKind::ServiceBack, 1001));

        assert!(a.should_emit("fusion-rag", &AlertKind::RestartFailed, 1000));
        assert!(a.should_emit("fusion-rag", &AlertKind::RestartCapHit, 1000));
    }

    // issue #142: per (service, kind) rate window — more than 10 per key within 60s suppressed
    #[test]
    fn test_issue142_rate_limit_per_service_kind() {
        let mut a = Alerter::new(300, String::new());
        for i in 0..10u64 {
            assert!(a.should_emit("svc", &AlertKind::ServiceBack, 1000 + i));
            a.record_emitted("svc", &AlertKind::ServiceBack, 1000 + i);
        }
        // 11th emit (within 60s window) suppressed
        assert!(!a.should_emit("svc", &AlertKind::ServiceBack, 1009));
        // other kinds / other services unaffected
        assert!(a.should_emit("svc", &AlertKind::ServiceDown, 1009));
        assert!(a.should_emit("svc2", &AlertKind::ServiceBack, 1009));
        // released after the window slides past
        assert!(a.should_emit("svc", &AlertKind::ServiceBack, 1000 + RATE_WINDOW_SEC));
    }

    #[test]
    fn test_alert_kind_display() {
        assert_eq!(AlertKind::ServiceDown.label(), "ServiceDown");
        assert_eq!(AlertKind::ServiceBack.label(), "ServiceBack");
        assert_eq!(AlertKind::RestartFailed.label(), "RestartFailed");
        assert_eq!(AlertKind::RestartCapHit.label(), "RestartCapHit");
        assert_eq!(
            AlertKind::ContainerStateChange.label(),
            "ContainerStateChange"
        );
        assert_eq!(AlertKind::ContainerCrashLoop.label(), "ContainerCrashLoop");
        // issue #47: new escalation alert kinds
        assert_eq!(AlertKind::ServiceConfigFatal.label(), "ServiceConfigFatal");
        assert_eq!(AlertKind::FastFailLoop.label(), "FastFailLoop");
    }
}
