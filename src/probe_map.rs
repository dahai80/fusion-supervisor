use crate::probe::ProbeSpec;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

// 服务名 → ProbeSpec。路径由扫描 41 服务确探活路径得 (spec §4.3)。
// 默认 HTTP GET /health; 例外覆盖。UDS 服务 (speech/memory) 用 uds。
fn build_table() -> HashMap<&'static str, ProbeSpec> {
    let mut m = HashMap::new();
    // core
    m.insert("fusion-mlx", ProbeSpec::http(11434, "/healthz"));
    m.insert("fusion-gateway", ProbeSpec::http(11432, "/healthz"));
    // platform
    m.insert("fusion-rag", ProbeSpec::http(11436, "/health"));
    // issue #34: 实际公开健康口是 /api/v1/system/health (auth.py PUBLIC_PATHS), 根 /health 401
    m.insert(
        "fusion-model-hub",
        ProbeSpec::http(11444, "/api/v1/system/health"),
    );
    // issue #34: fm-server HTTP 面 /healthz (socket 已迁 ~/.fusion-memory/, HTTP 探活更稳)
    m.insert("fusion-memory", ProbeSpec::http(11435, "/healthz"));
    // app
    // issue #34: projectApiServer 全端点鉴权 (per-instance token), HTTP 探活恒 401 → TCP
    m.insert("fusion-code", ProbeSpec::tcp(11441));
    m.insert("fusion-doc", ProbeSpec::http(11449, "/health"));
    m.insert("fusion-cowork", ProbeSpec::http(11438, "/health"));
    // issue #34: cowork-ws = `fusion-cowork remote serve` WebSocket, 无 /health → TCP
    m.insert("fusion-cowork-ws", ProbeSpec::tcp(11439));
    // issue #34: fusion-projects 为 UDS 服务 (sock /tmp/fusion-project-svc.sock),
    // registry 11440 为死值, TCP 探活恒不可达 → UDS 探活
    m.insert(
        "fusion-projects",
        ProbeSpec::uds("/tmp/fusion-project-svc.sock"),
    );
    // domain
    // issue #34: 路由挂 /api/v1 前缀 (api/app.py include_router), 根 /health 404
    m.insert("fusion-health", ProbeSpec::http(11469, "/api/v1/health"));
    // issue #34: APIKeyMiddleware 豁免口为 /api/v1/ready (503=依赖未就绪, 真实状态)
    m.insert("fusion-finance", ProbeSpec::http(11466, "/api/v1/ready"));
    // issue #34: k12 健康口 /api/health (backend=ok 才算全通)
    m.insert("fusion-k12-teacher", ProbeSpec::http(11448, "/api/health"));
    // issue #34: bench 公开健康口 /api/v1/system/health (auth 豁免表)
    m.insert(
        "fusion-bench",
        ProbeSpec::http(11467, "/api/v1/system/health"),
    );
    // issue #34: modelization REST /health (start.sh 同口)
    m.insert(
        "fusion-code-modenization",
        ProbeSpec::http(11459, "/health"),
    );
    // issue #34: speech daemon socket 已迁 ~/.fusion-speech/run/ (上游 config.py),
    // HTTP /health 面一直存在, 改 HTTP 探活
    m.insert("fusion-speech", ProbeSpec::http(11465, "/health"));
    // issue #29: shell-ecosystem 模块服务 (实探 /health 均已验证)
    m.insert("fusion-cowork-web", ProbeSpec::http(11461, "/health"));
    m.insert("fusion-trainer-bridge", ProbeSpec::http(8790, "/health"));
    m.insert(
        "fusion-plugins-ecosystem-console",
        ProbeSpec::http(8795, "/health"),
    );
    m.insert("fusion-agent-studio", ProbeSpec::http(11442, "/health"));
    // issue #31: MCP server (sse) 无 /health 端点, TCP 端口探活
    m.insert("fusion-plugins-ecosystem", ProbeSpec::tcp(8765));
    // issue #33: fs-serve /health (deploy/start.sh, :11463)
    m.insert("fusion-store", ProbeSpec::http(11463, "/health"));
    // issue #33: fsb uvicorn /health (:11490)
    m.insert("fusion-smallbusiness", ProbeSpec::http(11490, "/health"));
    // issue #33: 仿真 gRPC 服务无 HTTP 面, TCP 端口探活 (:11447)
    m.insert("fusion-simulation", ProbeSpec::tcp(11447));
    // 其余服务兜底: ProbeSpec::tcp(port) 在 probe_spec_for 内补
    m
}

static TABLE: OnceLock<HashMap<&'static str, ProbeSpec>> = OnceLock::new();

pub fn probe_spec_for(name: &str, port: u16) -> ProbeSpec {
    let table = TABLE.get_or_init(build_table);
    if let Some(spec) = table.get(name) {
        return spec.clone();
    }
    // 兜底: TCP 端口通 (最弱探活, 路径未确)
    tracing::warn!(svc = name, port, "probe_map 未确路径, 退化 TCP 探活");
    ProbeSpec::tcp(port)
}

// fusion-store (issue #33 起 daemonize 直跑) 无 restart 语义 → 退化为 stop+start。
// issue #29: start_map 覆盖服务 (trainer-bridge 脚本无 restart 子命令;
// daemonize 进程无 restart 语义) 一律 stop+start。
pub fn restart_cmd_for(name: &str) -> &'static str {
    if name == "fusion-store" || crate::start_map::start_spec_for(name).is_some() {
        "stop+start"
    } else {
        "restart"
    }
}

pub fn start_sh_path(repo_dir: &str) -> PathBuf {
    let root = std::env::var("FUSION_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    root.join(repo_dir).join("start.sh")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::ProbeKind;

    #[test]
    fn test_probe_spec_known_http() {
        let s = probe_spec_for("fusion-mlx", 11434);
        assert!(matches!(s.kind, ProbeKind::Http));
        assert_eq!(s.path.as_deref(), Some("/healthz"));
    }

    #[test]
    fn test_probe_spec_known_uds() {
        // issue #34: memory 改 HTTP 后, UDS 探活以 fusion-projects 为代表
        let s = probe_spec_for("fusion-projects", 11440);
        assert!(matches!(s.kind, ProbeKind::Uds));
    }

    #[test]
    fn test_probe_spec_fallback_tcp() {
        let s = probe_spec_for("fusion-fake-unknown", 11499);
        assert!(matches!(s.kind, ProbeKind::Tcp));
    }

    #[test]
    fn test_restart_cmd_store_special() {
        assert_eq!(restart_cmd_for("fusion-store"), "stop+start");
        assert_eq!(restart_cmd_for("fusion-mlx"), "restart");
    }

    // issue #29: start_map 覆盖服务 → stop+start (无 restart 语义)
    #[test]
    fn test_restart_cmd_start_map_overrides() {
        assert_eq!(restart_cmd_for("fusion-trainer-bridge"), "stop+start");
        assert_eq!(restart_cmd_for("fusion-cowork-web"), "stop+start");
        assert_eq!(
            restart_cmd_for("fusion-plugins-ecosystem-console"),
            "stop+start"
        );
    }

    // issue #31: MCP server 无 /health, TCP 探活
    #[test]
    fn test_plugins_ecosystem_tcp_probe() {
        let s = probe_spec_for("fusion-plugins-ecosystem", 8765);
        assert!(matches!(s.kind, ProbeKind::Tcp));
    }

    // issue #33: 新托管服务探活 — store/fsb HTTP /health, simulation gRPC TCP
    #[test]
    fn test_issue33_probe_specs() {
        for (name, port) in [("fusion-store", 11463), ("fusion-smallbusiness", 11490)] {
            let s = probe_spec_for(name, port);
            assert!(matches!(s.kind, ProbeKind::Http), "{name} 应 HTTP 探活");
            assert_eq!(
                s.path.as_deref(),
                Some("/health"),
                "{name} 路径应为 /health"
            );
        }
        let s = probe_spec_for("fusion-simulation", 11447);
        assert!(matches!(s.kind, ProbeKind::Tcp), "simulation 应 TCP 探活");
    }

    // issue #33: 三个新托管服务均为 start_map 覆盖 → restart 退化为 stop+start
    #[test]
    fn test_issue33_restart_cmd_stop_start() {
        for name in ["fusion-store", "fusion-smallbusiness", "fusion-simulation"] {
            assert_eq!(restart_cmd_for(name), "stop+start", "{name}");
        }
    }

    // issue #34: 探活路径对齐上游实况 — model-hub/health/finance 换真实路径,
    // speech/memory 改 HTTP, projects 改 UDS, code 恒 401 退 TCP
    #[test]
    fn test_issue34_probe_path_fixes() {
        let s = probe_spec_for("fusion-model-hub", 11444);
        assert_eq!(s.path.as_deref(), Some("/api/v1/system/health"));

        let s = probe_spec_for("fusion-health", 11469);
        assert_eq!(s.path.as_deref(), Some("/api/v1/health"));

        let s = probe_spec_for("fusion-finance", 11466);
        assert_eq!(s.path.as_deref(), Some("/api/v1/ready"));

        for name in ["fusion-speech", "fusion-memory"] {
            let s = probe_spec_for(name, 1);
            assert!(matches!(s.kind, ProbeKind::Http), "{name} 应 HTTP 探活");
        }
        let s = probe_spec_for("fusion-speech", 11465);
        assert_eq!(s.path.as_deref(), Some("/health"));
        let s = probe_spec_for("fusion-memory", 11435);
        assert_eq!(s.path.as_deref(), Some("/healthz"));

        let s = probe_spec_for("fusion-projects", 11440);
        assert!(matches!(s.kind, ProbeKind::Uds), "projects 应 UDS 探活");

        let s = probe_spec_for("fusion-code", 11441);
        assert!(matches!(s.kind, ProbeKind::Tcp), "code 恒 401 应 TCP 探活");

        let s = probe_spec_for("fusion-cowork-ws", 11439);
        assert!(matches!(s.kind, ProbeKind::Tcp));

        // issue #34 补充: bench/k12/modenization 换真实路径
        let s = probe_spec_for("fusion-bench", 11467);
        assert_eq!(s.path.as_deref(), Some("/api/v1/system/health"));
        let s = probe_spec_for("fusion-k12-teacher", 11448);
        assert_eq!(s.path.as_deref(), Some("/api/health"));
        let s = probe_spec_for("fusion-code-modenization", 11459);
        assert_eq!(s.path.as_deref(), Some("/health"));
    }

    #[test]
    fn test_shell_ecosystem_probe_specs() {
        for (name, port) in [
            ("fusion-cowork-web", 11461),
            ("fusion-trainer-bridge", 8790),
            ("fusion-plugins-ecosystem-console", 8795),
            ("fusion-agent-studio", 11442),
        ] {
            let s = probe_spec_for(name, port);
            assert!(matches!(s.kind, ProbeKind::Http), "{name} 应 HTTP 探活");
            assert_eq!(
                s.path.as_deref(),
                Some("/health"),
                "{name} 路径应为 /health"
            );
        }
    }
}
