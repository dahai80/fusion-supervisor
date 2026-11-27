use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    Core,
    Platform,
    App,
    Domain,
    Cluster,
}

// PRD fusion-supervisor-app-tiering-1029: boot tiering, orthogonal to Layer —
// Layer governs start order, Tier governs whether the service auto-starts on
// boot. Default T2 (new services do not grab boot resources).
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum BootTier {
    T0,
    T1,
    #[default]
    T2,
}

impl BootTier {
    pub fn as_str(self) -> &'static str {
        match self {
            BootTier::T0 => "T0",
            BootTier::T1 => "T1",
            BootTier::T2 => "T2",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServiceDef {
    pub name: String,
    pub port: u16,
    pub repo_dir: String,
    pub purpose: String,
    pub fixed: bool,
    // issue #31: supervise: false entries (dev-only/k8s artifact/not built) get
    // no runner, up/down/tick/status all skip them — separating registry noise
    // from real failures.
    pub managed: bool,
    pub layer: Layer,
    // PRD app-tiering: boot tier, default T2; supervise:false exempt entries are
    // still labeled but excluded from tier operations
    pub boot_tier: BootTier,
}

#[derive(Deserialize)]
struct RegistryFile {
    // Preserve YAML source order: with the same layer and fixed flag, services
    // start in registry written order (mlx before gateway).
    // BTreeMap's alphabetical key sort would invert the core base order, hence
    // IndexMap.
    services: indexmap::IndexMap<String, ServiceEntry>,
}

#[derive(Deserialize)]
struct ServiceEntry {
    port: u16,
    purpose: String,
    #[serde(default)]
    fixed: bool,
    // issue #31: explicit exemption flag. Default true (managed); false = no runner.
    #[serde(default = "default_supervise")]
    supervise: bool,
    // PRD app-tiering: default T2 (newly registered services do not grab boot
    // resources by default)
    #[serde(default)]
    boot: BootTier,
}

fn default_supervise() -> bool {
    true
}

// Registry key -> local repo dir. Most keys equal their dir; exceptions here.
// Sub-service key (fusion-cowork-ws) -> parent repo (fusion-cowork).
pub fn name_to_dir(name: &str) -> &str {
    // Known spelling-difference exceptions + business-plane directory prefixes
    // (issue #45: registry keys lack the fusion- prefix; the actual repos live
    // under the monorepo root's business/ subdirectory, so the default
    // ~/fusion/<name> mapping does not exist)
    static EXCEPTIONS: &[(&str, &str)] = &[
        ("fusion-code-modelization", "fusion-code-modenization"),
        // issue #57: registry key singular, actual on-disk repo dir plural
        // fusion-multi-nodes (active v0.20.2, start.sh master 127.0.0.1:11452);
        // the default-mapped dir does not exist -> start path fail-closed,
        // long-term Failed
        ("fusion-multi-node", "fusion-multi-nodes"),
        // registry key without s, on-disk repo dir with s (fusion-models-hub,
        // active v1.2.10, start.sh 11444) — the old default mapping pointed at the
        // nonexistent fusion-model-hub/, start path fail-closed, the service could
        // only be started manually (the 2026-10-31 11444 orphan process was such a
        // manual leftover)
        ("fusion-model-hub", "fusion-models-hub"),
        ("ai-family", "business/ai-family"),
        // ai-fit: registry key + actual dir are both ai-fit (issue: boot
        // auto-start failed because the old 'fit-ai' exception pointed to a
        // business/fit-ai directory that never existed)
        ("ai-fit", "business/ai-fit"),
        ("ai-speech-exam", "business/ai-speech-exam"),
        ("ai-apparelmockup", "business/ai-apparelmockup"),
        // issue #55: business entries batch-registered in the 2026-10-27/28 port
        // governance sweep — the default ~/fusion/<key> mapping does not exist;
        // they all live under the business/ subdirectory (the two charactervoice
        // entries and the two customer-insight entries each share one repo;
        // ai-OperaBase has a case-unusual directory name).
        ("ai-ads-videobuilder", "business/ai-ads-videobuilder"),
        ("ai-doc-automation", "business/ai-doc-automation"),
        ("ai-information-search", "business/ai-information-search"),
        ("ai-OperaBase", "business/ai-OperaBase"),
        ("ai-charactervoice", "business/ai-charactervoice"),
        ("ai-charactervoice-session", "business/ai-charactervoice"),
        ("customer-insight", "business/ai-customer-insight"),
        ("customer-insight-api", "business/ai-customer-insight"),
        ("ai-apparelmockup-web", "business/ai-apparelmockup"),
        // issue #83: AI try-on backend (11515), repo at business/ai-trysuite;
        // the root start.sh is a wrapper whose exec delegates to server/start.sh
        ("ai-trysuite", "business/ai-trysuite"),
        // issue #88: two business entries found by the post-venv-rebuild fleet
        // audit — the default ~/fusion/<key> mapping does not exist, start
        // fail-closed (script missing); registry keys lack the fusion- prefix,
        // repos live under the business/ subdirectory
        ("ai-assistant-agent", "business/ai-assistant-agent"),
        ("ai-lead-qualifier", "business/ai-lead-qualifier"),
        // issue #94: same class as #88 — registry key without prefix, repo under
        // the business/ subdirectory, default ~/fusion/<key> mapping does not
        // exist, start fail-closed "script missing"
        ("ai-booking-agent", "business/ai-booking-agent"),
    ];
    for (key, dir) in EXCEPTIONS {
        if name == *key {
            return dir;
        }
    }
    // Sub-service keys: fusion-cowork-ws / fusion-security-svc /
    // fusion-multi-node-agent etc -> parent repo.
    // (prefix, dir) pairs: prefix matches the sub-service key, dir is the parent
    // repo's actual directory —
    // issue #57: supervise:false entries like fusion-multi-node-agent, if resolved
    // by logs/start_sh_path, must land in the real fusion-multi-nodes directory,
    // not the nonexistent singular one
    for (parent, dir) in &[
        ("fusion-cowork", "fusion-cowork"),
        ("fusion-security", "fusion-security"),
        // issue #57: parent repo dir is plural, prefix still matches the singular
        // registry key
        ("fusion-multi-node", "fusion-multi-nodes"),
        ("fusion-agent-studio", "fusion-agent-studio"),
        ("fusion-simulation", "fusion-simulation"),
        ("fusion-bench", "fusion-bench"),
        // issue #29: fusion-trainer-bridge -> fusion-trainer;
        // fusion-plugins-ecosystem-console -> fusion-plugins-ecosystem
        ("fusion-trainer", "fusion-trainer"),
        ("fusion-plugins-ecosystem", "fusion-plugins-ecosystem"),
        // osagent issue#6: the registry only has the fusion-osagent-metrics
        // sub-service key; the on-disk repo is fusion-osagent (no start.sh, see
        // the start_map Daemon spec for startup)
        ("fusion-osagent", "fusion-osagent"),
        // issue #81: fusion-office-api sub-service -> parent repo fusion-office.
        // The root start.sh only manages the 11485 shell; the api lifecycle script
        // lives in services/office-api/ (started via the start_map Script spec);
        // name_to_dir's default mapping points at the nonexistent
        // fusion-office-api/ directory (logs/start_sh_path fallback fail-closed)
        ("fusion-office", "fusion-office"),
        // issue #147: fusion-drama-backend sub-service -> parent repo fusion-drama.
        // The registry key is the sub-service (supervise turned on by
        // fusion-drama#14); the on-disk repo is fusion-drama (root start.sh
        // start|stop|restart|status|log + pidfile + /api/v1/health wait); the
        // default mapping points at the nonexistent fusion-drama-backend/ directory
        ("fusion-drama", "fusion-drama"),
    ] {
        if name.starts_with(parent) && name.len() > parent.len() {
            return dir;
        }
    }
    name
}

pub fn layer_of(name: &str) -> Layer {
    let core = ["fusion-mlx", "fusion-gateway"];
    let platform = [
        "fusion-rag",
        "fusion-model-hub",
        "fusion-artifacts-engine",
        "fusion-plugins-ecosystem",
        "fusion-gateway-deploy",
    ];
    let cluster_prefix = "fusion-multi-node";
    if core.contains(&name) {
        Layer::Core
    } else if platform.contains(&name) {
        Layer::Platform
    } else if name.starts_with(cluster_prefix) {
        Layer::Cluster
    } else if is_domain(name) {
        Layer::Domain
    } else {
        Layer::App
    }
}

fn is_domain(name: &str) -> bool {
    let domain = [
        "fusion-health",
        "fusion-science",
        "fusion-finance",
        "fusion-k12-teacher",
        "fusion-simulation",
        "fusion-gupiao",
        "fusion-smallbusiness",
        "fusion-store",
        "fusion-bench",
    ];
    domain.iter().any(|d| name == *d || name.starts_with(d))
}

pub struct ServiceManifest;

// issue #96: service-name -> registered-port snapshot (populated by new() at
// daemon start). daemon_stop_cmd's port-anchored orphan fallback (lsof kill)
// needs the port, but exec paths (free fns) cannot reach ServiceDef — hence the
// dep_map/probe_map-style OnceLock global table.
static PORT_MAP: std::sync::OnceLock<std::collections::HashMap<String, u16>> =
    std::sync::OnceLock::new();

pub fn register_ports(defs: &[ServiceDef]) {
    let m: std::collections::HashMap<String, u16> =
        defs.iter().map(|d| (d.name.clone(), d.port)).collect();
    let _ = PORT_MAP.set(m);
}

pub fn registered_port(svc: &str) -> Option<u16> {
    PORT_MAP.get().and_then(|m| m.get(svc).copied())
}

impl ServiceManifest {
    pub fn load(path: &Path) -> Result<Vec<ServiceDef>> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read registry: {}", path.display()))?;
        let reg: RegistryFile = serde_yaml::from_str(&text)
            .with_context(|| format!("failed to parse registry: {}", path.display()))?;
        let defs = reg
            .services
            .into_iter()
            .map(|(name, entry)| {
                if !entry.supervise {
                    tracing::info!(svc = %name, "registry marked supervise: false, skipping management (dev-only/external/planned)");
                }
                ServiceDef {
                    repo_dir: name_to_dir(&name).to_string(),
                    layer: layer_of(&name),
                    name,
                    port: entry.port,
                    purpose: entry.purpose,
                    fixed: entry.fixed,
                    managed: entry.supervise,
                    boot_tier: entry.boot,
                }
            })
            .collect();
        Ok(defs)
    }

    pub fn sort_by_layer(mut defs: Vec<ServiceDef>) -> Vec<ServiceDef> {
        defs.sort_by(|a, b| {
            // fixed forces first, then layer
            match (a.fixed, b.fixed) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => (),
            }
            a.layer.cmp(&b.layer)
        });
        defs
    }
}

// issue #102: absent registry must degrade, never block boot (core-profile
// contract, fusion-gateway#498). Minimal-download machines clone only the five
// core repos — no architecture/ — and still need a live daemon for status/up
// against whatever runs. One WARN with remediation hint, then an empty managed
// set: managed start/stop is simply unavailable, nothing is spawned.
pub fn load_or_empty(path: &Path) -> Vec<ServiceDef> {
    match ServiceManifest::load(path) {
        Ok(d) => {
            tracing::info!(services = d.len(), "registry loaded");
            d
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                err = %e,
                "registry not found — fleet management disabled (start/stop unavailable); clone the monorepo root or point registry_path at a valid port-registry.yaml to enable"
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_REGISTRY: &str = r#"
services:
  fusion-mlx:
    port: 11434
    purpose: local inference engine
    fixed: true
  fusion-gateway:
    port: 11432
    purpose: gateway
    fixed: true
  fusion-rag:
    port: 11436
    purpose: vector knowledge base
    fixed: false
  fusion-code-modelization:
    port: 11459
    purpose: code modelization
    fixed: false
  fusion-multi-node:
    port: 11452
    purpose: cluster master
    fixed: false
  fusion-web-shell:
    port: 11430
    purpose: sample exempt entry
    fixed: false
    supervise: false
reserved: []
external_tools: []
"#;

    #[test]
    fn test_parse_count_and_ports() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), FAKE_REGISTRY).unwrap();
        let defs = ServiceManifest::load(tmp.path()).unwrap();
        assert_eq!(defs.len(), 6);
        let mlx = defs.iter().find(|d| d.name == "fusion-mlx").unwrap();
        assert_eq!(mlx.port, 11434);
        assert!(mlx.fixed);
        let rag = defs.iter().find(|d| d.name == "fusion-rag").unwrap();
        assert_eq!(rag.port, 11436);
        assert!(!rag.fixed);
    }

    // issue #102: absent registry degrades to an empty managed set (never
    // blocks daemon boot); a valid registry loads through the same entrypoint.
    #[test]
    fn test_load_or_empty_degrades_on_missing_registry() {
        let missing =
            std::env::temp_dir().join(format!("no-such-registry-{}.yaml", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        assert!(load_or_empty(&missing).is_empty());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), FAKE_REGISTRY).unwrap();
        assert_eq!(load_or_empty(tmp.path()).len(), 6);
    }

    // issue #31: supervise flag — default true, explicit false exempts
    #[test]
    fn test_supervise_flag_default_and_exemption() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), FAKE_REGISTRY).unwrap();
        let defs = ServiceManifest::load(tmp.path()).unwrap();
        let rag = defs.iter().find(|d| d.name == "fusion-rag").unwrap();
        assert!(rag.managed, "entries without supervise default to managed");
        let web_shell = defs.iter().find(|d| d.name == "fusion-web-shell").unwrap();
        assert!(!web_shell.managed, "supervise: false should exempt");
    }

    // issue #91: boot tier parsing — explicit T1 takes effect, default T2
    // (menu-resident services need boot autostart)
    #[test]
    fn test_boot_tier_parse_default_and_explicit() {
        const REG: &str = r#"
services:
  fusion-mlx:
    port: 11434
    purpose: inference
    fixed: true
    boot: T0
  fusion-trainer-bridge:
    port: 8790
    purpose: Trainer bridge
    boot: T1
  fusion-health:
    port: 11471
    purpose: health assistant
reserved: []
external_tools: []
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), REG).unwrap();
        let defs = ServiceManifest::load(tmp.path()).unwrap();
        let mlx = defs.iter().find(|d| d.name == "fusion-mlx").unwrap();
        assert_eq!(mlx.boot_tier, BootTier::T0);
        let bridge = defs
            .iter()
            .find(|d| d.name == "fusion-trainer-bridge")
            .unwrap();
        assert_eq!(
            bridge.boot_tier,
            BootTier::T1,
            "explicit boot: T1 should take effect"
        );
        let health = defs.iter().find(|d| d.name == "fusion-health").unwrap();
        assert_eq!(health.boot_tier, BootTier::T2, "default should be T2");
    }

    #[test]
    fn test_name_to_dir_exception() {
        assert_eq!(
            name_to_dir("fusion-code-modelization"),
            "fusion-code-modenization"
        );
        assert_eq!(name_to_dir("fusion-mlx"), "fusion-mlx");
        assert_eq!(name_to_dir("fusion-cowork-ws"), "fusion-cowork");
        // issue #81: office-api lands in parent repo fusion-office (default
        // mapped directory does not exist)
        assert_eq!(name_to_dir("fusion-office-api"), "fusion-office");
    }

    // issue #45: business-plane services -> business/ subdirectory
    #[test]
    fn test_name_to_dir_business_plane() {
        assert_eq!(name_to_dir("ai-family"), "business/ai-family");
        assert_eq!(name_to_dir("ai-fit"), "business/ai-fit");
        assert_eq!(name_to_dir("ai-speech-exam"), "business/ai-speech-exam");
        assert_eq!(name_to_dir("ai-apparelmockup"), "business/ai-apparelmockup");
        // issue #55: batch-registered entries from 2026-10-27/28 (including the
        // case-unusual dir and shared-repo dual entries)
        assert_eq!(
            name_to_dir("ai-OperaBase"),
            "business/ai-OperaBase",
            "directory name has unusual casing"
        );
        assert_eq!(
            name_to_dir("ai-charactervoice-session"),
            "business/ai-charactervoice",
            "session and api share one repo"
        );
        assert_eq!(
            name_to_dir("customer-insight-api"),
            "business/ai-customer-insight",
            "api and web share one repo"
        );
        assert_eq!(
            name_to_dir("ai-ads-videobuilder"),
            "business/ai-ads-videobuilder"
        );
        // issue #83: AI try-on backend; the root start.sh is a wrapper delegating
        // to server/start.sh
        assert_eq!(name_to_dir("ai-trysuite"), "business/ai-trysuite");
    }

    // issue #29: shell-ecosystem sub-services -> parent repo; a parent name itself
    // -> itself
    #[test]
    fn test_name_to_dir_shell_ecosystem() {
        assert_eq!(name_to_dir("fusion-trainer-bridge"), "fusion-trainer");
        assert_eq!(
            name_to_dir("fusion-plugins-ecosystem-console"),
            "fusion-plugins-ecosystem"
        );
        assert_eq!(name_to_dir("fusion-cowork-web"), "fusion-cowork");
        assert_eq!(
            name_to_dir("fusion-plugins-ecosystem"),
            "fusion-plugins-ecosystem"
        );
    }

    // issue #57: registry key fusion-multi-node is singular, the on-disk repo dir
    // is plural (fusion-multi-nodes) — the bare key goes through EXCEPTIONS, the
    // sub-service keys through the prefix loop; both must land in the real plural
    // directory (otherwise the start path fail-closes as long-term Failed)
    #[test]
    fn test_name_to_dir_multi_node_plural_dir() {
        assert_eq!(name_to_dir("fusion-multi-node"), "fusion-multi-nodes");
        assert_eq!(name_to_dir("fusion-multi-node-agent"), "fusion-multi-nodes");
        assert_eq!(name_to_dir("fusion-multi-node-mcp"), "fusion-multi-nodes");
    }

    // fusion-model-hub: registry key without s, on-disk repo dir with s — the
    // default mapping points at the nonexistent fusion-model-hub/, start path
    // fail-closed
    #[test]
    fn test_name_to_dir_model_hub_plural_dir() {
        assert_eq!(name_to_dir("fusion-model-hub"), "fusion-models-hub");
    }

    // issue #88: two business entries found by the fleet audit — the default
    // mapping points at the nonexistent root <key>/ dirs; the repos actually live
    // under the business/ subdirectory
    #[test]
    fn test_name_to_dir_issue88_business_entries() {
        assert_eq!(
            name_to_dir("ai-assistant-agent"),
            "business/ai-assistant-agent"
        );
        assert_eq!(
            name_to_dir("ai-lead-qualifier"),
            "business/ai-lead-qualifier"
        );
        // issue #94: booking agent, same class (default-mapped start fail-closed
        // "script missing")
        assert_eq!(name_to_dir("ai-booking-agent"), "business/ai-booking-agent");
    }

    // osagent issue#6: the fusion-osagent-metrics sub-service key lands in the
    // real repo directory fusion-osagent (start path no longer points at the
    // nonexistent fusion-osagent-metrics/)
    #[test]
    fn test_name_to_dir_osagent_metrics_parent() {
        assert_eq!(name_to_dir("fusion-osagent-metrics"), "fusion-osagent");
    }

    // issue #147: fusion-drama-backend sub-service key -> parent repo
    // fusion-drama (start path no longer points at the nonexistent
    // fusion-drama-backend/ directory)
    #[test]
    fn test_name_to_dir_drama_backend_parent() {
        assert_eq!(name_to_dir("fusion-drama-backend"), "fusion-drama");
    }

    // issue #29: the four shell module services should land in the App layer
    // (gateway starts first in the Core layer, satisfying the "gateway before
    // modules" registration order dependency)
    #[test]
    fn test_shell_ecosystem_layer_app_after_core() {
        assert!(matches!(layer_of("fusion-cowork-web"), Layer::App));
        assert!(matches!(layer_of("fusion-trainer-bridge"), Layer::App));
        assert!(matches!(
            layer_of("fusion-plugins-ecosystem-console"),
            Layer::App
        ));
        assert!(matches!(layer_of("fusion-agent-studio"), Layer::App));
        assert!(matches!(layer_of("fusion-gateway"), Layer::Core));
    }

    #[test]
    fn test_layer_of() {
        assert!(matches!(layer_of("fusion-mlx"), Layer::Core));
        assert!(matches!(layer_of("fusion-gateway"), Layer::Core));
        assert!(matches!(layer_of("fusion-rag"), Layer::Platform));
        assert!(matches!(layer_of("fusion-code-modelization"), Layer::App));
        assert!(matches!(layer_of("fusion-multi-node"), Layer::Cluster));
        assert!(matches!(layer_of("fusion-health"), Layer::Domain));
    }

    #[test]
    fn test_layer_sort_order() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), FAKE_REGISTRY).unwrap();
        let defs = ServiceManifest::load(tmp.path()).unwrap();
        let sorted = ServiceManifest::sort_by_layer(defs);
        let names: Vec<_> = sorted.iter().map(|d| d.name.as_str()).collect();
        // core first (fixed), then platform, app, cluster
        assert_eq!(names[0], "fusion-mlx");
        assert_eq!(names[1], "fusion-gateway");
        assert!(
            names.iter().position(|n| *n == "fusion-rag").unwrap()
                < names
                    .iter()
                    .position(|n| *n == "fusion-code-modelization")
                    .unwrap()
        );
        assert!(
            names
                .iter()
                .position(|n| *n == "fusion-code-modelization")
                .unwrap()
                < names
                    .iter()
                    .position(|n| *n == "fusion-multi-node")
                    .unwrap()
        );
    }

    // PRD app-tiering: boot field parsing — default T2, explicit T0/T1 take
    // effect, illegal values error
    #[test]
    fn test_boot_tier_default_t2() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), FAKE_REGISTRY).unwrap();
        let defs = ServiceManifest::load(tmp.path()).unwrap();
        for name in ["fusion-mlx", "fusion-rag", "fusion-multi-node"] {
            let d = defs.iter().find(|d| d.name == name).unwrap();
            assert_eq!(
                d.boot_tier,
                BootTier::T2,
                "{name} without a boot field should default to T2"
            );
        }
    }

    #[test]
    fn test_boot_tier_explicit_parse() {
        let yaml = r#"
services:
  fusion-mlx:
    port: 11434
    purpose: inference
    fixed: true
    boot: T0
  fusion-rag:
    port: 11436
    purpose: knowledge base
    boot: T1
  fusion-health:
    port: 11471
    purpose: health
    boot: T2
reserved: []
external_tools: []
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), yaml).unwrap();
        let defs = ServiceManifest::load(tmp.path()).unwrap();
        let mlx = defs.iter().find(|d| d.name == "fusion-mlx").unwrap();
        assert_eq!(mlx.boot_tier, BootTier::T0);
        let rag = defs.iter().find(|d| d.name == "fusion-rag").unwrap();
        assert_eq!(rag.boot_tier, BootTier::T1);
        let health = defs.iter().find(|d| d.name == "fusion-health").unwrap();
        assert_eq!(health.boot_tier, BootTier::T2);
    }

    #[test]
    fn test_boot_tier_illegal_value_rejected() {
        let yaml = r#"
services:
  fusion-mlx:
    port: 11434
    purpose: inference
    boot: T9
reserved: []
external_tools: []
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), yaml).unwrap();
        assert!(
            ServiceManifest::load(tmp.path()).is_err(),
            "T9 is an illegal enum value and should fail to parse"
        );
    }

    #[test]
    fn test_boot_tier_as_str() {
        assert_eq!(BootTier::T0.as_str(), "T0");
        assert_eq!(BootTier::T1.as_str(), "T1");
        assert_eq!(BootTier::T2.as_str(), "T2");
    }
}
