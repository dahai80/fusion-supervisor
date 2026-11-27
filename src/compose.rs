// Compose plane: wraps docker compose commands, parses ps status, and feeds SupervisorCore
// for unified orchestration.
// Design: the ComposeBackend trait abstracts the backend for test injection (same
// convention as ArcStartFn/ArcProbeFn).
// Production = real docker compose; tests = fake (no docker calls).
use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

// Container status: Service+Health from docker compose ps mapped to supervisor State.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerStatus {
    pub name: String,
    pub service: String,
    pub state: String,           // running / exited / restarting ...
    pub health: String,          // healthy / starting / unhealthy / "" (no healthcheck)
    pub publishers: Vec<String>, // port mappings ["0.0.0.0:5432->5432/tcp"]
}

impl ContainerStatus {
    // Maps to supervisor::runner::State semantics (does not depend on the runner module;
    // returns strings to avoid a dependency cycle)
    pub fn supervisor_state(&self) -> &'static str {
        if self.state == "exited" || self.state == "dead" {
            return "Failed";
        }
        if self.state == "restarting" {
            return "Restarting";
        }
        if self.state != "running" {
            return "Starting";
        }
        // running: consult the healthcheck
        match self.health.as_str() {
            "healthy" => "Healthy",
            "unhealthy" => "Failed",
            "starting" => "Starting",
            "" => "Healthy", // no healthcheck but running is treated as healthy
            _ => "Starting",
        }
    }

    // Port: container-side port number of the first publisher, 0 if none.
    // Handles both "0.0.0.0:5432->5432/tcp" (normalized) and "0.0.0.0:5432:5432/tcp" (raw).
    pub fn port(&self) -> u16 {
        for p in &self.publishers {
            // take the part before /, then the container port after the last :
            let host_part = p.split('/').next().unwrap_or(p);
            let port_str = if let Some(after) = host_part.split("->").nth(1) {
                after.split(':').next_back().unwrap_or(after)
            } else {
                host_part.split(':').next_back().unwrap_or(host_part)
            };
            if let Ok(n) = port_str.parse::<u16>() {
                return n;
            }
        }
        0
    }
}

// Compose backend trait: production = real docker, tests = fake.
pub trait ComposeBackend: Send + Sync {
    // up the sidecar plane (traefik/redis/postgres/qdrant), wait for health
    fn up_sidecar(&self) -> Result<()>;
    // up business containers (model-hub/cowork)
    fn up_business(&self) -> Result<()>;
    // down everything (business + sidecar)
    fn down(&self) -> Result<()>;
    // ps: list all container statuses
    fn ps(&self) -> Result<Vec<ContainerStatus>>;
    // logs --tail 50 for one service
    fn logs(&self, service: &str) -> Result<String>;
    // fail-closed env check: reject on missing secrets
    fn check_env(&self) -> Result<()>;
}

// Production backend: real docker compose.
pub struct DockerCompose {
    pub sidecar_file: PathBuf,
    pub business_file: PathBuf,
    pub env_file: PathBuf,
}

impl DockerCompose {
    pub fn new(sidecar: PathBuf, business: PathBuf, env: PathBuf) -> Self {
        Self {
            sidecar_file: sidecar,
            business_file: business,
            env_file: env,
        }
    }

    fn base_cmd(&self) -> Vec<String> {
        vec![
            "compose".into(),
            "--env-file".into(),
            self.env_file.to_string_lossy().into(),
            "-f".into(),
            self.sidecar_file.to_string_lossy().into(),
            "-f".into(),
            self.business_file.to_string_lossy().into(),
        ]
    }

    fn run(&self, args: &[&str]) -> Result<std::process::Output> {
        let mut full = self.base_cmd();
        for a in args {
            full.push(a.to_string());
        }
        tracing::info!(cmd = ?full, "exec docker compose");
        let out = std::process::Command::new("docker")
            .args(&full[1..])
            .output()
            .map_err(|e| anyhow::anyhow!("spawn docker compose: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!(
                "docker compose {} failed: {}",
                args.join(" "),
                stderr.trim()
            );
        }
        Ok(out)
    }
}

impl ComposeBackend for DockerCompose {
    fn up_sidecar(&self) -> Result<()> {
        self.check_env()?;
        // sidecar services first; --wait for healthchecks to pass
        self.run(&[
            "up", "-d", "--wait", "traefik", "redis", "postgres", "qdrant",
        ])?;
        tracing::info!("sidecar plane up (traefik/redis/postgres/qdrant healthy)");
        Ok(())
    }

    fn up_business(&self) -> Result<()> {
        self.run(&["up", "-d", "--wait", "fusion-model-hub", "fusion-cowork"])?;
        tracing::info!("business containers up (model-hub/cowork healthy)");
        Ok(())
    }

    fn down(&self) -> Result<()> {
        // down tolerates partial absence (orphan cleanup)
        let mut full = self.base_cmd();
        full.push("down".into());
        full.push("--remove-orphans".into());
        let out = std::process::Command::new("docker")
            .args(&full[1..])
            .output()
            .map_err(|e| anyhow::anyhow!("spawn docker compose down: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // down failures usually mean containers already stopped; warn only, not fatal
            tracing::warn!(stderr = %stderr.trim(), "compose down exited non-zero (tolerated)");
        }
        Ok(())
    }

    fn ps(&self) -> Result<Vec<ContainerStatus>> {
        let out = self.run(&["ps", "--format", "json", "--all"])?;
        let text = String::from_utf8_lossy(&out.stdout);
        parse_compose_ps(&text)
    }

    fn logs(&self, service: &str) -> Result<String> {
        let out = self.run(&["logs", "--tail", "50", service])?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn check_env(&self) -> Result<()> {
        // fail-closed: secrets required; reject and point at .env.example when missing
        // docker --env-file injects variables from the file, so check both env and env_file
        // config Phase 3 (issue #74): client key uses the canonical name FUSION_CLIENT_KEY;
        // legacy FUSION_MLX_API_KEY compatibility lives in the fusion-core alias chain,
        // this repo no longer references it
        let need = ["FUSION_PG_PASSWORD", "FUSION_CLIENT_KEY"];
        for k in need {
            if std::env::var(k).is_err() && !key_in_env_file(&self.env_file, k) {
                anyhow::bail!(
                    "missing {k} — compose fail-closed guard. Fill .env (see .env.example) or export {k}"
                );
            }
        }
        Ok(())
    }
}

// Check whether a key is defined in the env_file (docker --env-file injects it)
fn key_in_env_file(path: &std::path::Path, key: &str) -> bool {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    text.lines().any(|line| {
        let line = line.trim();
        !line.is_empty()
            && !line.starts_with('#')
            && line
                .split('=')
                .next()
                .map(|k| k.trim() == key)
                .unwrap_or(false)
    })
}

// Parse docker compose ps --format json output.
// compose v2 output: a multi-line JSON array (one object per line) or a single JSON
// array; both are handled.
pub fn parse_compose_ps(text: &str) -> Result<Vec<ContainerStatus>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }
    // try a whole-document JSON array (newer compose v2)
    if trimmed.starts_with('[') {
        let arr: Vec<RawContainer> = serde_json::from_str(trimmed)
            .map_err(|e| anyhow::anyhow!("ps JSON array parse failed: {e}"))?;
        return Ok(arr.into_iter().map(RawContainer::into_status).collect());
    }
    // line-by-line JSON objects (older compose v2 / ndjson)
    let mut out = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let raw: RawContainer = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("ps JSON line parse failed: {e}"))?;
        out.push(raw.into_status());
    }
    Ok(out)
}

// Raw docker compose ps --format json fields (compose v2 = PascalCase: Name/Service/State/Health/Publishers)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawContainer {
    #[serde(default)]
    name: String,
    #[serde(default)]
    service: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    health: String,
    #[serde(default)]
    publishers: Vec<RawPublisher>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawPublisher {
    #[serde(default, alias = "URL")]
    url: String, // "0.0.0.0:5432:5432/tcp" or published_url
}

impl RawContainer {
    fn into_status(self) -> ContainerStatus {
        // publishers.url format "0.0.0.0:5432:5432/tcp" → normalize to "0.0.0.0:5432->5432/tcp"
        let pubs: Vec<String> = self
            .publishers
            .into_iter()
            .filter(|p| !p.url.is_empty())
            .map(|p| normalize_publisher(&p.url))
            .collect();
        ContainerStatus {
            name: self.name,
            service: self.service,
            state: self.state,
            health: self.health,
            publishers: pubs,
        }
    }
}

// "0.0.0.0:5432:5432/tcp" → "0.0.0.0:5432->5432/tcp"
fn normalize_publisher(url: &str) -> String {
    // already in -> form: return as-is
    if url.contains("->") {
        return url.to_string();
    }
    // host:port:container/tcp → host:port->container/tcp
    let path = url.split('/').next().unwrap_or(url);
    let rest = if let Some(idx) = url.find('/') {
        &url[idx..]
    } else {
        ""
    };
    let parts: Vec<&str> = path.split(':').collect();
    if parts.len() == 3 {
        format!("{}:{}->{}{}", parts[0], parts[1], parts[2], rest)
    } else {
        url.to_string()
    }
}

// Test support: FakeCompose exposed to supervisor integration tests (cfg test only)
#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::sync::{Arc, Mutex};

    pub struct FakeCompose {
        pub calls: Arc<Mutex<Vec<String>>>,
        pub containers: Arc<Mutex<Vec<ContainerStatus>>>,
    }

    impl Default for FakeCompose {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakeCompose {
        pub fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                containers: Arc::new(Mutex::new(Vec::new())),
            }
        }
        pub fn set_containers(&self, cs: Vec<ContainerStatus>) {
            *self.containers.lock().unwrap() = cs;
        }
    }

    impl ComposeBackend for FakeCompose {
        fn up_sidecar(&self) -> Result<()> {
            self.calls.lock().unwrap().push("up_sidecar".into());
            Ok(())
        }
        fn up_business(&self) -> Result<()> {
            self.calls.lock().unwrap().push("up_business".into());
            Ok(())
        }
        fn down(&self) -> Result<()> {
            self.calls.lock().unwrap().push("down".into());
            Ok(())
        }
        fn ps(&self) -> Result<Vec<ContainerStatus>> {
            Ok(self.containers.lock().unwrap().clone())
        }
        fn logs(&self, service: &str) -> Result<String> {
            self.calls.lock().unwrap().push(format!("logs:{service}"));
            Ok(format!("fake logs for {service}"))
        }
        fn check_env(&self) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // compose v2 single JSON array format
    #[test]
    fn test_parse_ps_json_array() {
        let json = r#"[
          {"Name":"sv-postgres-1","Service":"postgres","State":"running","Health":"healthy","Publishers":[{"URL":"0.0.0.0:5432:5432/tcp"}]},
          {"Name":"sv-redis-1","Service":"redis","State":"running","Health":"healthy","Publishers":[{"URL":"0.0.0.0:6379:6379/tcp"}]}
        ]"#;
        let st = parse_compose_ps(json).unwrap();
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].service, "postgres");
        assert_eq!(st[0].supervisor_state(), "Healthy");
        assert_eq!(st[0].port(), 5432);
        assert_eq!(st[1].port(), 6379);
        assert!(st[1].publishers[0].contains("->"));
    }

    // ndjson line-by-line format
    #[test]
    fn test_parse_ps_ndjson() {
        let json = "{\"Name\":\"sv-traefik-1\",\"Service\":\"traefik\",\"State\":\"running\",\"Health\":\"healthy\",\"Publishers\":[]}\n{\"Name\":\"sv-qdrant-1\",\"Service\":\"qdrant\",\"State\":\"exited\",\"Health\":\"\",\"Publishers\":[]}\n";
        let st = parse_compose_ps(json).unwrap();
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].supervisor_state(), "Healthy");
        assert_eq!(st[1].supervisor_state(), "Failed");
        assert_eq!(st[1].port(), 0);
    }

    #[test]
    fn test_parse_ps_empty() {
        assert!(parse_compose_ps("").unwrap().is_empty());
        assert!(parse_compose_ps("  \n  ").unwrap().is_empty());
    }

    #[test]
    fn test_state_mapping() {
        let mk = |state: &str, health: &str| ContainerStatus {
            name: "x".into(),
            service: "x".into(),
            state: state.into(),
            health: health.into(),
            publishers: vec![],
        };
        assert_eq!(mk("running", "healthy").supervisor_state(), "Healthy");
        assert_eq!(mk("running", "unhealthy").supervisor_state(), "Failed");
        assert_eq!(mk("running", "starting").supervisor_state(), "Starting");
        assert_eq!(mk("running", "").supervisor_state(), "Healthy");
        assert_eq!(mk("exited", "").supervisor_state(), "Failed");
        assert_eq!(mk("dead", "").supervisor_state(), "Failed");
        assert_eq!(mk("restarting", "").supervisor_state(), "Restarting");
        assert_eq!(mk("created", "").supervisor_state(), "Starting");
    }

    #[test]
    fn test_port_extraction() {
        let mk = |pubs: Vec<&str>| ContainerStatus {
            name: "x".into(),
            service: "x".into(),
            state: "running".into(),
            health: "healthy".into(),
            publishers: pubs.into_iter().map(String::from).collect(),
        };
        assert_eq!(mk(vec!["0.0.0.0:5432->5432/tcp"]).port(), 5432);
        assert_eq!(mk(vec!["0.0.0.0:8080:8080/tcp"]).port(), 8080);
        assert_eq!(mk(vec![]).port(), 0);
    }

    #[test]
    fn test_normalize_publisher() {
        assert_eq!(
            normalize_publisher("0.0.0.0:5432:5432/tcp"),
            "0.0.0.0:5432->5432/tcp"
        );
        assert_eq!(
            normalize_publisher("0.0.0.0:5432->5432/tcp"),
            "0.0.0.0:5432->5432/tcp"
        );
    }
}
