use crate::probe::ProbeSpec;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

// 服务名 → ProbeSpec。路径由扫描 44 服务确探活路径得 (spec §4.3)。
// 默认 HTTP GET /health; 例外覆盖。UDS 服务 (speech/memory) 用 uds。
fn build_table() -> HashMap<&'static str, ProbeSpec> {
    let mut m = HashMap::new();
    // core
    m.insert("fusion-mlx", ProbeSpec::http(11434, "/healthz"));
    m.insert("fusion-gateway", ProbeSpec::http(11432, "/healthz"));
    // platform
    m.insert("fusion-rag", ProbeSpec::http(11436, "/health"));
    m.insert("fusion-model-hub", ProbeSpec::http(11444, "/health"));
    m.insert("fusion-memory", ProbeSpec::uds("/tmp/fusion-memory.sock"));
    // app
    m.insert("fusion-code", ProbeSpec::http(11441, "/health"));
    m.insert("fusion-doc", ProbeSpec::http(11449, "/health"));
    m.insert("fusion-cowork", ProbeSpec::http(11438, "/health"));
    // domain
    m.insert("fusion-health", ProbeSpec::http(11469, "/health"));
    m.insert("fusion-finance", ProbeSpec::http(11466, "/health"));
    m.insert("fusion-k12-teacher", ProbeSpec::http(11448, "/health"));
    // speech = UDS daemon
    m.insert("fusion-speech", ProbeSpec::uds("/tmp/fusion-speech.sock"));
    // 其余 44 服务兜底: ProbeSpec::tcp(port) 在 probe_spec_for 内补
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

// fusion-store deploy/start.sh 仅 start, 无 restart → 退化为 stop+start
pub fn restart_cmd_for(name: &str) -> &'static str {
    if name == "fusion-store" {
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
        let s = probe_spec_for("fusion-memory", 11435);
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
}
