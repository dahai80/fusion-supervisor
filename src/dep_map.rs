// 依赖门: native 服务 → 其依赖的 compose sidecar 容器服务名。
// up_native 启动前显式校验依赖容器 supervisor_state == Healthy, 不靠 layer 隐式顺序。
// compose.enabled=false 时调用方跳过门 (无 compose 后端, 无依赖可等)。
use std::collections::HashMap;
use std::sync::OnceLock;

// native 服务名 → 依赖的 compose 容器服务名列表。
// 仅列依赖 sidecar 的服务; 不在此表的 native 服务无门 (保持原行为)。
fn build_table() -> HashMap<&'static str, Vec<&'static str>> {
    let mut m = HashMap::new();
    // app 层: identity 依赖 postgres
    m.insert("fusion-identity", vec!["postgres"]);
    // platform 层: rag 依赖 qdrant
    m.insert("fusion-rag", vec!["qdrant"]);
    // app 层: cowork 依赖 postgres + redis
    m.insert("fusion-cowork", vec!["postgres", "redis"]);
    // platform 层: model-hub 依赖 postgres + redis
    m.insert("fusion-model-hub", vec!["postgres", "redis"]);
    m
}

static TABLE: OnceLock<HashMap<&'static str, Vec<&'static str>>> = OnceLock::new();

// 返回该 native 服务依赖的 compose 容器服务名。无依赖返回空切片。
pub fn deps_for(name: &str) -> &'static [&'static str] {
    let table = TABLE.get_or_init(build_table);
    table.get(name).map(|v| v.as_slice()).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_deps_postgres() {
        assert_eq!(deps_for("fusion-identity"), &["postgres"]);
    }

    #[test]
    fn test_rag_deps_qdrant() {
        assert_eq!(deps_for("fusion-rag"), &["qdrant"]);
    }

    #[test]
    fn test_cowork_deps_postgres_redis() {
        assert_eq!(deps_for("fusion-cowork"), &["postgres", "redis"]);
    }

    #[test]
    fn test_unknown_service_no_deps() {
        assert!(deps_for("fusion-mlx").is_empty());
        assert!(deps_for("fusion-fake").is_empty());
    }
}
