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
}

// registry key → 本地 repo 目录。绝大多数 key==dir, 例外在此。
// sub-service key (fusion-cowork-ws) → 父 repo (fusion-cowork)。
pub fn name_to_dir(name: &str) -> &str {
    // 已知拼写差异例外
    static EXCEPTIONS: &[(&str, &str)] =
        &[("fusion-code-modelization", "fusion-code-modenization")];
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
            .map(|(name, entry)| ServiceDef {
                repo_dir: name_to_dir(&name).to_string(),
                layer: layer_of(&name),
                name,
                port: entry.port,
                purpose: entry.purpose,
                fixed: entry.fixed,
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
reserved: []
external_tools: []
"#;

    #[test]
    fn test_parse_count_and_ports() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), FAKE_REGISTRY).unwrap();
        let defs = ServiceManifest::load(tmp.path()).unwrap();
        assert_eq!(defs.len(), 5);
        let mlx = defs.iter().find(|d| d.name == "fusion-mlx").unwrap();
        assert_eq!(mlx.port, 11434);
        assert!(mlx.fixed);
        let rag = defs.iter().find(|d| d.name == "fusion-rag").unwrap();
        assert_eq!(rag.port, 11436);
        assert!(!rag.fixed);
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
