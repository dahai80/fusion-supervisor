// Dependency gate: native service → compose sidecar container service names it depends on.
// up_native explicitly validates each dependency container supervisor_state == Healthy
// before starting; does not rely on implicit layer ordering.
// When compose.enabled=false the caller skips the gate (no compose backend, nothing to wait on).
use std::collections::HashMap;
use std::sync::OnceLock;

// Native service name → dependent compose container service names.
// Only lists services that depend on sidecars; native services not in this
// table have no gate (existing behavior preserved).
fn build_table() -> HashMap<&'static str, Vec<&'static str>> {
    let mut m = HashMap::new();
    // app layer: identity depends on postgres
    m.insert("fusion-identity", vec!["postgres"]);
    // platform layer: rag depends on qdrant
    m.insert("fusion-rag", vec!["qdrant"]);
    // app layer: cowork depends on postgres + redis
    m.insert("fusion-cowork", vec!["postgres", "redis"]);
    // platform layer: model-hub depends on postgres + redis
    m.insert("fusion-model-hub", vec!["postgres", "redis"]);
    m
}

static TABLE: OnceLock<HashMap<&'static str, Vec<&'static str>>> = OnceLock::new();

// Returns the compose container service names this native service depends on.
// Returns an empty slice when there are no dependencies.
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
