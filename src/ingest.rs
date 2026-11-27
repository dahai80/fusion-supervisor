// issue #139: external alert ingestion — external producers (fusion-ops probes etc.) POST
// to /api/v1/alerts/ingest to write alerts into the history.db event stream, optionally
// forwarded via an Alerter webhook. Pure logic lives here (payload parsing + dedup);
// HTTP orchestration is in http_server.
use serde_json::Value;

// Ingest request body: {"source":"fusion-ops","kind":"fail","severity":"error","detail":...,
// "dedup_key":"optional"}. Invalid severity falls back to "warn"; missing source/kind = reject.
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
            // missing/invalid falls back to warn (matches the old event table row default)
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

// Dedup gate: only participates when the caller supplies a dedup_key (same key recorded
// once per window); no key = always pass. The window reuses cfg.alert_dedup_sec
// (default 300s, same as ServiceDown).
#[derive(Default)]
pub struct IngestGate {
    last: std::collections::HashMap<String, u64>,
}

impl IngestGate {
    // true = pass (record event), false = duplicate within window (skip). Passing records the timestamp.
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

    // Roll back the cursor when event persistence fails — if observe advances first and
    // the record then fails, legitimate retries would be swallowed by the wasted window
    // (200 deduped but the event was never persisted).
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
        // severity missing/invalid falls back to warn; detail defaults to empty; dedup_key defaults to None
        let p = IngestPayload::parse(r#"{"source":"s","kind":"k","severity":"bogus"}"#).unwrap();
        assert_eq!(p.severity, "warn");
        assert_eq!(p.detail, "");
        assert!(p.dedup_key.is_none());
        // non-string detail (object) is serialized into detail, not rejected
        let p = IngestPayload::parse(r#"{"source":"s","kind":"k","detail":{"a":1}}"#).unwrap();
        assert_eq!(p.detail, r#"{"a":1}"#);
    }

    #[test]
    fn test_parse_rejects_missing_source_or_kind() {
        assert!(IngestPayload::parse(r#"{"kind":"k"}"#).is_err());
        assert!(IngestPayload::parse(r#"{"source":"s"}"#).is_err());
        assert!(IngestPayload::parse("not json").is_err());
        // empty/whitespace-only is rejected too
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
        assert!(!g.observe(Some("k"), 400, 300)); // within window (<=)
        assert!(!g.observe(Some("k"), 399, 300));
        assert!(g.observe(Some("k"), 401, 300)); // past window passes and refreshes the cursor
        assert!(!g.observe(Some("k"), 500, 300));
        // different keys do not interfere
        assert!(g.observe(Some("k2"), 500, 300));
    }
}
