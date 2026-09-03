// issue #22: Prometheus /metrics 端点纯逻辑渲染。
// 从 SupervisorCore 内存态生成 Prometheus 文本格式 (text exposition format v0.0.4)。
// 纯函数, 不持锁不查 DB, 调用方 (HTTP handler) 负责 lock core 后传引用。
// 锁纪律: handler lock().await → render(&c) → 立即释放, 短持锁同 RPC。

use crate::supervisor::SupervisorCore;
use crate::supervisor::runner::State;

// State → 1 if Healthy else 0 (供 healthy gauge)。
fn is_healthy(s: State) -> u32 {
    match s {
        State::Healthy => 1,
        _ => 0,
    }
}

// State → Prometheus label-safe 字符串 (全小写, 无空格)。
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

// 渲染 Prometheus 文本格式。core 内存态 → 字符串。
// 指标:
//   fusion_sv_up 1                              — daemon 活着 (恒 1, 端点能响应即活)
//   fusion_sv_services_total{plane="native"}    — native 服务总数
//   fusion_sv_services_total{plane="compose"}   — compose 容器总数
//   fusion_sv_service_healthy{service,port,plane} — 1=Healthy, 0=其他态
//   fusion_sv_service_state{service,port,plane,state} — 1=当前态 (每服务仅其当前态为 1)
//   fusion_sv_crash_count{service}              — 滑动窗内崩溃次数 (native only)
pub fn render(core: &SupervisorCore) -> String {
    let mut out = String::with_capacity(4096);
    // daemon up (端点能响应即活)
    out.push_str("# HELP fusion_sv_up 1 if the supervisor daemon is responding.\n");
    out.push_str("# TYPE fusion_sv_up gauge\n");
    out.push_str("fusion_sv_up 1\n");

    // native 服务总数
    let native_n = core.runners.len();
    out.push_str("# HELP fusion_sv_services_total Number of supervised services by plane.\n");
    out.push_str("# TYPE fusion_sv_services_total gauge\n");
    out.push_str(&format!(
        "fusion_sv_services_total{{plane=\"native\"}} {native_n}\n"
    ));

    // compose 容器总数 (若启用, 从 ps 快照; 失败则 0)
    let compose_n = if core.compose_enabled() {
        core.status_all()
            .iter()
            .filter(|e| e.plane == "compose")
            .count()
    } else {
        0
    };
    out.push_str(&format!(
        "fusion_sv_services_total{{plane=\"compose\"}} {compose_n}\n"
    ));

    // 每服务 healthy gauge + state gauge + crash_count
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

    // compose 容器: 仅 healthy + state gauge (compose 容器无 crash_count)
    if core.compose_enabled() {
        for e in core.status_all().iter().filter(|e| e.plane == "compose") {
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
    }

    out
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
            layer,
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
        assert!(out.contains("fusion_sv_up 1"), "应有 daemon up 指标");
        assert!(
            out.contains("fusion_sv_services_total{plane=\"native\"} 2"),
            "native 总数应为 2"
        );
        assert!(
            out.contains("fusion_sv_services_total{plane=\"compose\"} 0"),
            "compose 未启用应为 0"
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
            "Healthy 服务应 healthy=1"
        );
        assert!(
            out.contains("state=\"healthy\"} 1"),
            "应有 state=healthy gauge"
        );
        assert!(
            out.contains("fusion_sv_crash_count{service=\"fusion-mlx\"} 0"),
            "应有 crash_count=0"
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
        // 记 2 次崩溃
        core.runners[0].policy.record_crash(100);
        core.runners[0].policy.record_crash(101);
        let out = render(&core);
        assert!(
            out.contains(
                "fusion_sv_service_healthy{service=\"fusion-mlx\",port=\"11434\",plane=\"native\"} 0"
            ),
            "Failed 服务应 healthy=0"
        );
        assert!(
            out.contains("state=\"failed\"} 1"),
            "应有 state=failed gauge"
        );
        assert!(
            out.contains("fusion_sv_crash_count{service=\"fusion-mlx\"} 2"),
            "crash_count 应为 2"
        );
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
                "状态 {st:?} 应有 state={label} gauge"
            );
        }
    }
}
