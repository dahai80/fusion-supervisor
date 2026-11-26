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
    // issue #47: 配置致命退出 (exit 78, EX_CONFIG) — 重启无救, 需人工修配置
    ServiceConfigFatal,
    // issue #47: 快速失败熔断 (start 成功后秒级又宕, 连续达阈值)
    FastFailLoop,
    // issue #112: 自动恢复 (crash/start-retry 退避流) 实际拉起成功 — 审计事件,
    // 不去抖。detail 携带 old/new pid; 触发原因见紧邻的 ServiceDown 行。
    AutoRestart,
    // issue #137: 定时任务执行失败 (非零退出 / 超时被杀 / spawn 失败)。不去抖。
    JobFailed,
    // issue #139: 外部摄入告警经 webhook 转发时的 kind 标签 (事件本身由
    // ingest 路径直接落库, 此处仅 re-emit, sink 恒空 — 不重复记事件)。
    External,
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

// issue #142: 每 (service, kind) 滑动窗口限流 — 同 key 60s 内最多 10 条, 超出
// 抑制。防御纵深: 状态机 bug 时不让 webhook / event 表被风暴打满
// (ServiceDown 去抖只护一种 kind, 其余 kind 依赖源头边沿触发正确)。
const RATE_WINDOW_SEC: u64 = 60;
const RATE_MAX_PER_WINDOW: usize = 10;

pub struct Alerter {
    dedup_sec: u64,
    last_down: HashMap<String, u64>,
    // issue #142: (service, kind) → 窗口内已发时间戳 (升序)
    rate_window: HashMap<(String, &'static str), Vec<u64>>,
    webhook_url: String,
    // issue #37: 告警 → event 表的落库 sink (可选; 未设置则不落库, 测试用)
    event_sink: Option<EventSink>,
    // issue #37: 真实发出 (去抖后) 的告警累计数 (/metrics fusion_sv_alerts_total)
    alerts_emitted: u64,
    // issue #142: 被限流抑制的告警累计数 (排障可见性)
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

    // issue #37: 注入事件落库 sink (builder 风格, 生产构造处调用)
    pub fn with_event_sink(mut self, sink: EventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    pub fn alerts_emitted(&self) -> u64 {
        self.alerts_emitted
    }

    // issue #142: 被限流抑制的告警累计数
    pub fn alerts_rate_limited(&self) -> u64 {
        self.alerts_rate_limited
    }

    // issue #142: 纯判定 — 同 (service, kind) 窗口内已发条数未达上限
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

    // issue #37: kind → severity 映射 (crash-loop 升级=error, 恢复/正常事件=info, 其余 warn)
    fn severity_of(kind: &AlertKind) -> &'static str {
        match kind {
            AlertKind::ContainerCrashLoop => "error",
            // issue #47: 配置致命/快速失败熔断均为升级事件; #137 任务失败同升级
            AlertKind::ServiceConfigFatal | AlertKind::FastFailLoop | AlertKind::JobFailed => {
                "error"
            }
            AlertKind::ServiceBack | AlertKind::ServiceDrained | AlertKind::AutoRestart => "info",
            _ => "warn",
        }
    }

    // 纯判定: ServiceDown 在去抖窗内 = 抑制; 再叠加每 (service, kind) 限流窗。
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

    // 记录已发 (供测试与 alert() 内部调用)。
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
                // issue #142: 限流抑制 — WARN 一次可见 (排障线索), 不进 webhook/事件表
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
        // CrashLoop / config-fatal / 快速失败熔断升级用 ERROR, 其余 WARN (便于运维筛升级事件)
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
            if attempt > 0 {
                // 重试间加退避 — 旧实现 3 连发零间隔, 对端死亡时瞬时 3 连击
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

    // issue #142: 每 (service, kind) 限流窗 — 同 key 60s 内超 10 条抑制
    #[test]
    fn test_issue142_rate_limit_per_service_kind() {
        let mut a = Alerter::new(300, String::new());
        for i in 0..10u64 {
            assert!(a.should_emit("svc", &AlertKind::ServiceBack, 1000 + i));
            a.record_emitted("svc", &AlertKind::ServiceBack, 1000 + i);
        }
        // 第 11 条 (60s 窗内) 抑制
        assert!(!a.should_emit("svc", &AlertKind::ServiceBack, 1009));
        // 其他 kind / 其他服务不受影响
        assert!(a.should_emit("svc", &AlertKind::ServiceDown, 1009));
        assert!(a.should_emit("svc2", &AlertKind::ServiceBack, 1009));
        // 窗口滑出后释放
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
        // issue #47: 新增升级告警 kind
        assert_eq!(AlertKind::ServiceConfigFatal.label(), "ServiceConfigFatal");
        assert_eq!(AlertKind::FastFailLoop.label(), "FastFailLoop");
    }
}
