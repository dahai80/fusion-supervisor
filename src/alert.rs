use std::collections::HashMap;
use std::sync::Arc;

// issue #37: 事件落库 sink — 每次真实发出的告警同步写 history.db event 表。
// (service, kind, detail, ts, plane, severity) 六元组, 由构造方闭包决定落库细节。
pub type EventSink = Arc<dyn Fn(&str, &str, &str, u64, &str, &str) + Send + Sync>;

// 告警: ServiceDown 去抖 (5min/服务), 其余每次发。sink = tracing WARN + 可选 webhook。
// webhook 投递失败重试 3 次退回仅日志, 不影响监督 (spec §6.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    ServiceDown,
    ServiceBack,
    RestartFailed,
    RestartCapHit,
    // compose 容器失败期间 docker state 变化 (新事件, 不去抖)
    ContainerStateChange,
    // compose 容器连续失败达阈值升级 (crash-loop, 不去抖)
    ContainerCrashLoop,
    // issue #9: 服务 drain 完成 (摘流量后停止, 正常事件)
    ServiceDrained,
    // issue #9: rollout prewarm 失败 (启动后超时未达 Healthy)
    RolloutPrewarmFailed,
    // issue #10: 备份失败 (目标未设/不可写/docker exec 失败)
    BackupFailed,
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

pub struct Alerter {
    dedup_sec: u64,
    last_down: HashMap<String, u64>,
    webhook_url: String,
    // issue #37: 告警 → event 表的落库 sink (可选; 未设置则不落库, 测试用)
    event_sink: Option<EventSink>,
    // issue #37: 真实发出 (去抖后) 的告警累计数 (/metrics fusion_sv_alerts_total)
    alerts_emitted: u64,
}

impl Alerter {
    pub fn new(dedup_sec: u64, webhook_url: String) -> Self {
        Self {
            dedup_sec,
            last_down: HashMap::new(),
            webhook_url,
            event_sink: None,
            alerts_emitted: 0,
        }
    }

    // issue #37: 注入事件落库 sink (builder 风格, 生产构造处调用)
    pub fn with_event_sink(mut self, sink: EventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    pub fn alerts_emitted(&self) -> u64 {
        self.alerts_emitted
    }

    // issue #37: kind → severity 映射 (crash-loop 升级=error, 恢复/正常事件=info, 其余 warn)
    fn severity_of(kind: &AlertKind) -> &'static str {
        match kind {
            AlertKind::ContainerCrashLoop => "error",
            AlertKind::ServiceBack | AlertKind::ServiceDrained => "info",
            _ => "warn",
        }
    }

    // 纯判定: ServiceDown 在去抖窗内 = 抑制, 其余恒放行。
    pub fn should_emit(&self, service: &str, kind: &AlertKind, ts: u64) -> bool {
        if *kind != AlertKind::ServiceDown {
            return true;
        }
        match self.last_down.get(service) {
            None => true,
            Some(&last) => ts.saturating_sub(last) > self.dedup_sec,
        }
    }

    // 记录已发 (供测试与 alert() 内部调用)。
    pub fn record_emitted(&mut self, service: &str, kind: &AlertKind, ts: u64) {
        if *kind == AlertKind::ServiceDown {
            self.last_down.insert(service.to_string(), ts);
        }
    }

    pub async fn alert(&mut self, alert: Alert) -> anyhow::Result<()> {
        if !self.should_emit(&alert.service, &alert.kind, alert.ts) {
            tracing::debug!(svc = %alert.service, kind = alert.kind.label(), "alert dedup skip");
            return Ok(());
        }
        // CrashLoop 升级用 ERROR, 其余 WARN (便于运维筛 crash-loop)
        let is_escalation = matches!(alert.kind, AlertKind::ContainerCrashLoop);
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
        // issue #37: 真实发出的告警 → 计数 + 事件落库 (dedup 通过才到这, 不重复记录)
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
            tracing::warn!(err = %e, "webhook send fail (退回仅日志, 不影响监督)");
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
        // 记录已发
        a.record_emitted("fusion-rag", &AlertKind::ServiceDown, 1000);
        // 5min 内不重发
        assert!(!a.should_emit("fusion-rag", &AlertKind::ServiceDown, 1000 + 299));
        // 300s 边界: 仍抑制 (<= window)
        assert!(!a.should_emit("fusion-rag", &AlertKind::ServiceDown, 1000 + 300));
        // 301s: 释放
        assert!(a.should_emit("fusion-rag", &AlertKind::ServiceDown, 1000 + 301));
    }

    #[test]
    fn test_dedup_per_service_isolated() {
        let mut a = Alerter::new(300, String::new());
        a.record_emitted("fusion-rag", &AlertKind::ServiceDown, 1000);
        // 另一服务不受影响
        assert!(a.should_emit("fusion-mlx", &AlertKind::ServiceDown, 1000 + 100));
    }

    #[test]
    fn test_other_kinds_always_emit() {
        let mut a = Alerter::new(300, String::new());
        // ServiceBack / RestartFailed / RestartCapHit 不去抖, 每次都发
        assert!(a.should_emit("fusion-rag", &AlertKind::ServiceBack, 1000));
        a.record_emitted("fusion-rag", &AlertKind::ServiceBack, 1000);
        assert!(a.should_emit("fusion-rag", &AlertKind::ServiceBack, 1001));

        assert!(a.should_emit("fusion-rag", &AlertKind::RestartFailed, 1000));
        assert!(a.should_emit("fusion-rag", &AlertKind::RestartCapHit, 1000));
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
    }
}
