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

#[derive(Debug, Clone)]
pub struct ServiceDef {
    pub name: String,
    pub port: u16,
    pub repo_dir: String,
    pub purpose: String,
    pub fixed: bool,
    // issue #31: supervise: false 条目 (dev-only/k8s artifact/未建) 不进 runner,
    // up/down/tick/status 全跳过 — 注册表噪声与真实故障分离。
    pub managed: bool,
    pub layer: Layer,
}

#[derive(Deserialize)]
struct RegistryFile {
    // 保留 YAML 源序: 同层同 fixed 时按注册表书写顺序启动 (mlx 先于 gateway)。
    // BTreeMap 按键名字母序会颠倒核心基座顺序, 故用 IndexMap。
    services: indexmap::IndexMap<String, ServiceEntry>,
}

#[derive(Deserialize)]
struct ServiceEntry {
    port: u16,
    purpose: String,
    #[serde(default)]
    fixed: bool,
    // issue #31: 显式豁免标记。默认 true (可托管); false = 不建 runner。
    #[serde(default = "default_supervise")]
    supervise: bool,
}

fn default_supervise() -> bool {
    true
}

// registry key → 本地 repo 目录。绝大多数 key==dir, 例外在此。
// sub-service key (fusion-cowork-ws) → 父 repo (fusion-cowork)。
pub fn name_to_dir(name: &str) -> &str {
    // 已知拼写差异例外 + business 平台目录前缀 (issue #45: registry key 无 fusion-
    // 前缀, 实际 repo 在 monorepo 根 business/ 子目录下, 默认映射 ~/fusion/<name> 不存在)
    static EXCEPTIONS: &[(&str, &str)] = &[
        ("fusion-code-modelization", "fusion-code-modenization"),
        ("ai-family", "business/ai-family"),
        // ai-fit: registry key + actual dir are both ai-fit (issue: boot
        // auto-start failed because the old 'fit-ai' exception pointed to a
        // business/fit-ai directory that never existed)
        ("ai-fit", "business/ai-fit"),
        ("ai-speech-exam", "business/ai-speech-exam"),
        ("ai-apparelmockup", "business/ai-apparelmockup"),
        // issue #55: 2026-10-27/28 端口治理批量登记的 business 条目 — 默认映射
        // ~/fusion/<key> 不存在, 全部落在 business/ 子目录 (charactervoice 双条目
        // 与 customer-insight 双条目各自共享同一 repo; ai-OperaBase 目录大小写特殊)。
        ("ai-ads-videobuilder", "business/ai-ads-videobuilder"),
        ("ai-doc-automation", "business/ai-doc-automation"),
        ("ai-information-search", "business/ai-information-search"),
        ("ai-OperaBase", "business/ai-OperaBase"),
        ("ai-charactervoice", "business/ai-charactervoice"),
        ("ai-charactervoice-session", "business/ai-charactervoice"),
        ("customer-insight", "business/ai-customer-insight"),
        ("customer-insight-api", "business/ai-customer-insight"),
        ("ai-apparelmockup-web", "business/ai-apparelmockup"),
    ];
    for (key, dir) in EXCEPTIONS {
        if name == *key {
            return dir;
        }
    }
    // 子服务 key: fusion-cowork-ws / fusion-security-svc / fusion-multi-node-agent 等 → 父 repo
    for parent in &[
        "fusion-cowork",
        "fusion-security",
        "fusion-multi-node",
        "fusion-agent-studio",
        "fusion-simulation",
        "fusion-bench",
        // issue #29: fusion-trainer-bridge → fusion-trainer;
        // fusion-plugins-ecosystem-console → fusion-plugins-ecosystem
        "fusion-trainer",
        "fusion-plugins-ecosystem",
    ] {
        if name.starts_with(parent) && name.len() > parent.len() {
            return parent;
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

impl ServiceManifest {
    pub fn load(path: &Path) -> Result<Vec<ServiceDef>> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读注册表失败: {}", path.display()))?;
        let reg: RegistryFile = serde_yaml::from_str(&text)
            .with_context(|| format!("解析注册表失败: {}", path.display()))?;
        let defs = reg
            .services
            .into_iter()
            .map(|(name, entry)| {
                if !entry.supervise {
                    tracing::info!(svc = %name, "registry 标记 supervise: false, 跳过托管 (dev-only/external/planned)");
                }
                ServiceDef {
                    repo_dir: name_to_dir(&name).to_string(),
                    layer: layer_of(&name),
                    name,
                    port: entry.port,
                    purpose: entry.purpose,
                    fixed: entry.fixed,
                    managed: entry.supervise,
                }
            })
            .collect();
        Ok(defs)
    }

    pub fn sort_by_layer(mut defs: Vec<ServiceDef>) -> Vec<ServiceDef> {
        defs.sort_by(|a, b| {
            // fixed 强制最先, 然后 layer
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

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_REGISTRY: &str = r#"
services:
  fusion-mlx:
    port: 11434
    purpose: 本地推理引擎
    fixed: true
  fusion-gateway:
    port: 11432
    purpose: 网关
    fixed: true
  fusion-rag:
    port: 11436
    purpose: 向量知识库
    fixed: false
  fusion-code-modelization:
    port: 11459
    purpose: 代码建模
    fixed: false
  fusion-multi-node:
    port: 11452
    purpose: 集群 Master
    fixed: false
  fusion-web-shell:
    port: 11430
    purpose: 样例豁免条目
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

    // issue #31: supervise 标记 — 默认 true, 显式 false 豁免
    #[test]
    fn test_supervise_flag_default_and_exemption() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), FAKE_REGISTRY).unwrap();
        let defs = ServiceManifest::load(tmp.path()).unwrap();
        let rag = defs.iter().find(|d| d.name == "fusion-rag").unwrap();
        assert!(rag.managed, "未标 supervise 的条目默认可托管");
        let web_shell = defs.iter().find(|d| d.name == "fusion-web-shell").unwrap();
        assert!(!web_shell.managed, "supervise: false 应豁免");
    }

    #[test]
    fn test_name_to_dir_exception() {
        assert_eq!(
            name_to_dir("fusion-code-modelization"),
            "fusion-code-modenization"
        );
        assert_eq!(name_to_dir("fusion-mlx"), "fusion-mlx");
        assert_eq!(name_to_dir("fusion-cowork-ws"), "fusion-cowork");
    }

    // issue #45: business 平台服务 → business/ 子目录
    #[test]
    fn test_name_to_dir_business_plane() {
        assert_eq!(name_to_dir("ai-family"), "business/ai-family");
        assert_eq!(name_to_dir("ai-fit"), "business/ai-fit");
        assert_eq!(name_to_dir("ai-speech-exam"), "business/ai-speech-exam");
        assert_eq!(name_to_dir("ai-apparelmockup"), "business/ai-apparelmockup");
        // issue #55: 2026-10-27/28 批量登记条目 (含大小写特殊与共享 repo 双条目)
        assert_eq!(
            name_to_dir("ai-OperaBase"),
            "business/ai-OperaBase",
            "目录大小写特殊"
        );
        assert_eq!(
            name_to_dir("ai-charactervoice-session"),
            "business/ai-charactervoice",
            "session 与 api 共享 repo"
        );
        assert_eq!(
            name_to_dir("customer-insight-api"),
            "business/ai-customer-insight",
            "api 与 web 共享 repo"
        );
        assert_eq!(
            name_to_dir("ai-ads-videobuilder"),
            "business/ai-ads-videobuilder"
        );
    }

    // issue #29: shell-ecosystem 子服务 → 父 repo; 父名本身 → 自身
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

    // issue #29: 四个 shell 模块服务应落 App 层 (gateway 在 Core 层先起,
    // 满足 "gateway 先于模块" 的注册依赖顺序)
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
}
