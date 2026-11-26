// issue #139: 外部告警摄入 — 外部生产者 (fusion-ops 探活等) 经
// POST /api/v1/alerts/ingest 把告警写进 history.db 事件流, 可选经 Alerter
// webhook 转发。纯逻辑在此 (payload 解析 + dedup 判定), HTTP 编排在 http_server。
use serde_json::Value;

// 摄入请求体: {"source":"fusion-ops","kind":"fail","severity":"error","detail":...,
// "dedup_key":"optional"}。severity 非法值回落 "warn"; source/kind 缺失 = 拒绝。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestPayload {
    pub source: String,
    pub kind: String,
    pub severity: String,
    pub detail: String,
    pub dedup_key: Option<String>,
}

impl IngestPayload {
    pub fn parse(body: &str) -> Result<IngestPayload, String> {
        let v: Value = serde_json::from_str(body).map_err(|e| format!("body must be JSON: {e}"))?;
        let source = v
            .get("source")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let kind = v
            .get("kind")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if source.is_empty() || kind.is_empty() {
            return Err("source and kind are required".to_string());
        }
        let severity = match v.get("severity").and_then(|x| x.as_str()) {
            Some(s @ ("info" | "warn" | "error")) => s.to_string(),
            // 缺省/非法值回落 warn (与 event 表旧行默认一致)
            _ => "warn".to_string(),
        };
        let detail = v
            .get("detail")
            .map(|x| match x {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        let dedup_key = v
            .get("dedup_key")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        Ok(IngestPayload {
            source,
            kind,
            severity,
            detail,
            dedup_key,
        })
    }
}

// dedup 闸门: 调用方提供 dedup_key 才参与去重 (同 key 在窗口内只记一次),
// 缺省 = 每次都放行。窗口复用 cfg.alert_dedup_sec (默认 300s, 与 ServiceDown 一致)。
#[derive(Default)]
pub struct IngestGate {
    last: std::collections::HashMap<String, u64>,
}

impl IngestGate {
    // true = 放行 (记事件), false = 窗口内重复 (跳过)。放行时记录时间戳。
    pub fn observe(&mut self, key: Option<&str>, ts: u64, window_sec: u64) -> bool {
        let Some(key) = key else {
            return true;
        };
        match self.last.get(key) {
            Some(&last) if ts.saturating_sub(last) <= window_sec => false,
            _ => {
                self.last.insert(key.to_string(), ts);
                true
            }
        }
    }

    // 事件落库失败时回退游标 — observe 先推进、record 后失败的话, 合法重发
    // 会被白耗的窗口吞掉 (200 deduped 但事件从未落库)。
    pub fn rollback(&mut self, key: Option<&str>) {
        if let Some(key) = key {
            self.last.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_payload() {
        let p = IngestPayload::parse(
            r#"{"source":"fusion-ops","kind":"fail","severity":"error","detail":"probe down","dedup_key":"ops:11400"}"#,
        )
        .unwrap();
        assert_eq!(p.source, "fusion-ops");
        assert_eq!(p.kind, "fail");
        assert_eq!(p.severity, "error");
        assert_eq!(p.detail, "probe down");
        assert_eq!(p.dedup_key.as_deref(), Some("ops:11400"));
    }

    #[test]
    fn test_parse_defaults_and_clamps() {
        // severity 缺省/非法回落 warn; detail 缺省空; dedup_key 缺省 None
        let p = IngestPayload::parse(r#"{"source":"s","kind":"k","severity":"bogus"}"#).unwrap();
        assert_eq!(p.severity, "warn");
        assert_eq!(p.detail, "");
        assert!(p.dedup_key.is_none());
        // detail 非字符串 (对象) 序列化进 detail, 不拒收
        let p = IngestPayload::parse(r#"{"source":"s","kind":"k","detail":{"a":1}}"#).unwrap();
        assert_eq!(p.detail, r#"{"a":1}"#);
    }

    #[test]
    fn test_parse_rejects_missing_source_or_kind() {
        assert!(IngestPayload::parse(r#"{"kind":"k"}"#).is_err());
        assert!(IngestPayload::parse(r#"{"source":"s"}"#).is_err());
        assert!(IngestPayload::parse("not json").is_err());
        // 空串/空白同样拒绝
        assert!(IngestPayload::parse(r#"{"source":"  ","kind":"k"}"#).is_err());
    }

    #[test]
    fn test_gate_no_key_always_passes() {
        let mut g = IngestGate::default();
        assert!(g.observe(None, 100, 300));
        assert!(g.observe(None, 101, 300));
    }

    #[test]
    fn test_gate_dedup_within_window_then_expires() {
        let mut g = IngestGate::default();
        assert!(g.observe(Some("k"), 100, 300));
        assert!(!g.observe(Some("k"), 400, 300)); // 窗口内 (<=)
        assert!(!g.observe(Some("k"), 399, 300));
        assert!(g.observe(Some("k"), 401, 300)); // 过窗放行并刷新游标
        assert!(!g.observe(Some("k"), 500, 300));
        // 不同 key 互不影响
        assert!(g.observe(Some("k2"), 500, 300));
    }
}
