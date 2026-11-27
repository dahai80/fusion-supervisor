// issue #22: pure-logic rendering for the Prometheus /metrics endpoint.
// Generates the Prometheus text format (text exposition format v0.0.4) from SupervisorCore
// in-memory state. Pure function, no locking, no DB; the caller (HTTP handler) locks
// the core and passes the reference.
// Lock discipline: handler lock().await → render(&c) → release immediately, short hold like RPC.

use crate::supervisor::SupervisorCore;
use crate::supervisor::runner::State;

// State → 1 if Healthy else 0 (for the healthy gauge).
fn is_healthy(s: State) -> u32 {
    match s {
        State::Healthy => 1,
        _ => 0,
    }
}

// State → Prometheus label-safe string (all lowercase, no spaces).
fn state_label(s: State) -> &'static str {
    match s {
        State::Stopped => "stopped",
        State::Starting => "starting",
        State::Healthy => "healthy",
        State::Unhealthy => "unhealthy",
        State::Restarting => "restarting",
        State::Failed => "failed",
        State::Draining => "draining",
    }
}

// Render the Prometheus text format. Core in-memory state → string.
// Metrics:
//   fusion_sv_up 1                              — daemon alive (always 1, endpoint responding = alive)
//   fusion_sv_services_total{plane="native"}    — total native services
//   fusion_sv_services_total{plane="compose"}   — total compose containers
//   fusion_sv_service_healthy{service,port,plane} — 1=Healthy, 0=other states
//   fusion_sv_service_state{service,port,plane,state} — 1=current state (only the current state is 1 per service)
//   fusion_sv_crash_count{service}              — crash count within the sliding window (native only)
//   fusion_sv_alerts_total                      — actually emitted post-dedup alerts, cumulative (issue #37)
//   fusion_sv_events_pruned_total               — rows pruned by retention sweeps, cumulative (issue #37)
pub fn render(core: &SupervisorCore) -> String {
    let mut out = String::with_capacity(4096);
    // daemon up (endpoint responding = alive)
    out.push_str("# HELP fusion_sv_up 1 if the supervisor daemon is responding.\n");
    out.push_str("# TYPE fusion_sv_up gauge\n");
    out.push_str("fusion_sv_up 1\n");

    // total native services
    let native_n = core.runners.len();
    out.push_str("# HELP fusion_sv_services_total Number of supervised services by plane.\n");
    out.push_str("# TYPE fusion_sv_services_total gauge\n");
    out.push_str(&format!(
        "fusion_sv_services_total{{plane=\"native\"}} {native_n}\n"
    ));

    // fetch compose rows once — each status_all call is a blocking docker ps subprocess,
    // must not be called repeatedly (caller holds the core lock; double subprocesses double the lock hold time).
    let compose_rows: Vec<_> = if core.compose_enabled() {
        core.status_all()
            .into_iter()
            .filter(|e| e.plane == "compose")
            .collect()
    } else {
        Vec::new()
    };
    let compose_n = compose_rows.len();
    out.push_str(&format!(
        "fusion_sv_services_total{{plane=\"compose\"}} {compose_n}\n"
    ));

    // issue #37: alert + retention sweep cumulative counters (monotonic within the daemon lifetime)
    out.push_str(
        "# HELP fusion_sv_alerts_total Alerts actually emitted (post-dedup), cumulative.\n",
    );
    out.push_str("# TYPE fusion_sv_alerts_total counter\n");
    out.push_str(&format!(
        "fusion_sv_alerts_total {}\n",
        core.alerter.alerts_emitted()
    ));
    // issue #142: rate-limit suppression counter — the getter previously had no consumer, invisible when debugging
    out.push_str(
        "# HELP fusion_sv_alerts_rate_limited_total Alerts suppressed by the per (service, kind) rate window, cumulative.\n",
    );
    out.push_str("# TYPE fusion_sv_alerts_rate_limited_total counter\n");
    out.push_str(&format!(
        "fusion_sv_alerts_rate_limited_total {}\n",
        core.alerter.alerts_rate_limited()
    ));
    out.push_str("# HELP fusion_sv_events_pruned_total History rows pruned by retention sweeps, cumulative.\n");
    out.push_str("# TYPE fusion_sv_events_pruned_total counter\n");
    out.push_str(&format!(
        "fusion_sv_events_pruned_total {}\n",
        core.events_pruned_total
    ));

    // per-service healthy gauge + state gauge + crash_count
    out.push_str("# HELP fusion_sv_service_healthy 1 if the service is Healthy, 0 otherwise.\n");
    out.push_str("# TYPE fusion_sv_service_healthy gauge\n");
    out.push_str(
        "# HELP fusion_sv_service_state 1 if the service is in the labeled state, 0 otherwise.\n",
    );
    out.push_str("# TYPE fusion_sv_service_state gauge\n");
    out.push_str("# HELP fusion_sv_crash_count Crash count within the sliding restart window.\n");
    out.push_str("# TYPE fusion_sv_crash_count gauge\n");

    for r in &core.runners {
        let name = &r.def.name;
        let port = r.def.port;
        let st = r.state;
        let stl = state_label(st);
        out.push_str(&format!(
            "fusion_sv_service_healthy{{service=\"{name}\",port=\"{port}\",plane=\"native\"}} {}\n",
            is_healthy(st)
        ));
        out.push_str(&format!(
            "fusion_sv_service_state{{service=\"{name}\",port=\"{port}\",plane=\"native\",state=\"{stl}\"}} 1\n"
        ));
        out.push_str(&format!(
            "fusion_sv_crash_count{{service=\"{name}\"}} {}\n",
            r.policy.crash_count()
        ));
    }

    // compose containers: only healthy + state gauge (no crash_count for containers)
    for e in &compose_rows {
        let name = &e.name;
        let port = e.port;
        let stl = e.state.to_lowercase();
        let h = if e.state == "Healthy" { 1 } else { 0 };
        out.push_str(&format!(
                "fusion_sv_service_healthy{{service=\"{name}\",port=\"{port}\",plane=\"compose\"}} {h}\n"
            ));
        out.push_str(&format!(
                "fusion_sv_service_state{{service=\"{name}\",port=\"{port}\",plane=\"compose\",state=\"{stl}\"}} 1\n"
            ));
    }

    out
}

// issue #26: pure-logic rendering for the read-only HTTP status face (GET /api/v1/status).
// Mirrors the UDS RPC `status` method: per-service {name, state, port, plane} JSON array.
// Data source for the browser-reachable ops panel; start/stop mutations remain UDS RPC only.
// Pure function, no locking; the caller (HTTP handler) locks the core and passes the reference, same lock discipline as render.
pub fn render_status_json(core: &SupervisorCore) -> String {
    let entries: Vec<serde_json::Value> = core
        .status_all()
        .into_iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name, "state": e.state, "port": e.port, "plane": e.plane,
                // issue #112: most recent restart-class event (mirrors the UDS status output)
                "last_restart": e.last_restart,
            })
        })
        .collect();
    serde_json::to_string(&serde_json::json!({
        "ok": true,
        "services": entries,
    }))
    .unwrap_or_else(|_| "{\"ok\":false,\"services\":[]}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Layer, ServiceDef};
    use crate::probe::ProbeResult;
    use crate::supervisor::SupervisorCore;

    fn def(name: &str, layer: Layer, port: u16) -> ServiceDef {
        ServiceDef {
            name: name.into(),
            port,
            repo_dir: name.into(),
            purpose: "x".into(),
            fixed: false,
            managed: true,
            layer,
            boot_tier: crate::manifest::BootTier::T2,
        }
    }

    fn cfg() -> crate::config::Config {
        crate::config::Config {
            max_concurrent_start: 2,
            start_timeout_sec: 5,
            ..crate::config::Config::default()
        }
    }

    #[test]
    fn test_render_has_daemon_up_and_counts() {
        let core = SupervisorCore::new_with_fns(
            vec![
                def("fusion-mlx", Layer::Core, 11434),
                def("fusion-rag", Layer::App, 11436),
            ],
            cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        let out = render(&core);
        assert!(
            out.contains("fusion_sv_up 1"),
            "should have the daemon up metric"
        );
        assert!(
            out.contains("fusion_sv_services_total{plane=\"native\"} 2"),
            "native total should be 2"
        );
        assert!(
            out.contains("fusion_sv_services_total{plane=\"compose\"} 0"),
            "compose disabled should be 0"
        );
    }

    #[test]
    fn test_render_native_healthy_and_state_gauges() {
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        core.runners[0].state = State::Healthy;
        let out = render(&core);
        assert!(
            out.contains(
                "fusion_sv_service_healthy{service=\"fusion-mlx\",port=\"11434\",plane=\"native\"} 1"
            ),
            "Healthy service should have healthy=1"
        );
        assert!(
            out.contains("state=\"healthy\"} 1"),
            "should have a state=healthy gauge"
        );
        assert!(
            out.contains("fusion_sv_crash_count{service=\"fusion-mlx\"} 0"),
            "should have crash_count=0"
        );
    }

    #[test]
    fn test_render_failed_service_healthy_zero() {
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        core.runners[0].state = State::Failed;
        // record 2 crashes
        core.runners[0].policy.record_crash(100);
        core.runners[0].policy.record_crash(101);
        let out = render(&core);
        assert!(
            out.contains(
                "fusion_sv_service_healthy{service=\"fusion-mlx\",port=\"11434\",plane=\"native\"} 0"
            ),
            "Failed service should have healthy=0"
        );
        assert!(
            out.contains("state=\"failed\"} 1"),
            "should have a state=failed gauge"
        );
        assert!(
            out.contains("fusion_sv_crash_count{service=\"fusion-mlx\"} 2"),
            "crash_count should be 2"
        );
    }

    // issue #37: alert + retention sweep counters
    #[test]
    fn test_render_alerts_and_pruned_counters() {
        let mut core = SupervisorCore::new_with_fns(
            vec![def("fusion-mlx", Layer::Core, 11434)],
            cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        core.events_pruned_total = 5;
        let out = render(&core);
        assert!(
            out.contains("fusion_sv_alerts_total 0"),
            "initial alert count should be 0: {out}"
        );
        assert!(
            out.contains("fusion_sv_events_pruned_total 5"),
            "sweep count should be 5: {out}"
        );
    }

    #[test]
    fn test_render_status_json_mirrors_status_rpc() {
        // issue #26: read-only status face = field mirror of the UDS status RPC (name/state/port/plane)
        let mut core = SupervisorCore::new_with_fns(
            vec![
                def("fusion-mlx", Layer::Core, 11434),
                def("fusion-agent-studio", Layer::App, 11442),
            ],
            cfg(),
            |_| ProbeResult::Healthy { latency_ms: 1 },
            |_, _, _| Ok(()),
        );
        core.runners[0].state = State::Healthy;
        core.runners[1].state = State::Stopped;
        let out = render_status_json(&core);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], serde_json::json!(true));
        let svcs = v["services"].as_array().unwrap();
        assert_eq!(svcs.len(), 2);
        assert_eq!(svcs[0]["name"], "fusion-mlx");
        assert_eq!(svcs[0]["state"], "Healthy");
        assert_eq!(svcs[0]["port"], 11434);
        assert_eq!(svcs[0]["plane"], "native");
        assert_eq!(svcs[1]["state"], "Stopped");
        assert_eq!(svcs[1]["port"], 11442);
    }

    #[test]
    fn test_render_all_states_have_labels() {
        let states = [
            State::Stopped,
            State::Starting,
            State::Healthy,
            State::Unhealthy,
            State::Restarting,
            State::Failed,
            State::Draining,
        ];
        for st in states {
            let mut core = SupervisorCore::new_with_fns(
                vec![def("fusion-mlx", Layer::Core, 11434)],
                cfg(),
                |_| ProbeResult::Healthy { latency_ms: 1 },
                |_, _, _| Ok(()),
            );
            core.runners[0].state = st;
            let out = render(&core);
            let label = state_label(st);
            assert!(
                out.contains(&format!("state=\"{label}\"}} 1")),
                "state {st:?} should have a state={label} gauge"
            );
        }
    }
}
