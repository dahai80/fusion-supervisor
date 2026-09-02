// compose 平面: 包装 docker compose 命令, 解析 ps 状态, 提供给 SupervisorCore 统一编排。
// 设计: trait 抽象 ComposeBackend 便于测试注入 (同 ArcStartFn/ArcProbeFn 约定)。
// 生产=真 docker compose; 测试=fake (不调 docker)。
use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

// 容器状态: 来自 docker compose ps 的 Service+Health 映射到 supervisor State。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContainerStatus {
    pub name: String,
    pub service: String,
    pub state: String,           // running / exited / restarting ...
    pub health: String,          // healthy / starting / unhealthy / "" (无 healthcheck)
    pub publishers: Vec<String>, // 端口映射 ["0.0.0.0:5432->5432/tcp"]
}

impl ContainerStatus {
    // 映射到 supervisor::runner::State 语义 (不依赖 runner 模块, 返回字符串避免环)
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
        // running: 看 healthcheck
        match self.health.as_str() {
            "healthy" => "Healthy",
            "unhealthy" => "Failed",
            "starting" => "Starting",
            "" => "Healthy", // 无 healthcheck 但 running 视为健康
            _ => "Starting",
        }
    }

    // 端口: 取第一个 publisher 的容器端口数字, 无则 0。
    // 兼容 "0.0.0.0:5432->5432/tcp" (规整) 和 "0.0.0.0:5432:5432/tcp" (原始)。
    pub fn port(&self) -> u16 {
        for p in &self.publishers {
            // 取 / 前部分, 再取最后一个 : 后的容器端口
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

// compose 后端 trait: 生产真 docker, 测试 fake。
pub trait ComposeBackend: Send + Sync {
    // up sidecar 平面 (traefik/redis/postgres/qdrant), 等健康
    fn up_sidecar(&self) -> Result<()>;
    // up business 容器 (model-hub/cowork)
    fn up_business(&self) -> Result<()>;
    // down 全部 (business + sidecar)
    fn down(&self) -> Result<()>;
    // ps: 列出所有容器状态
    fn ps(&self) -> Result<Vec<ContainerStatus>>;
    // logs --tail 50 单服务
    fn logs(&self, service: &str) -> Result<String>;
    // fail-closed env 检查: 缺密码拒绝
    fn check_env(&self) -> Result<()>;
}

// 生产后端: 真 docker compose。
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
            anyhow::bail!("docker compose {} 失败: {}", args.join(" "), stderr.trim());
        }
        Ok(out)
    }
}

impl ComposeBackend for DockerCompose {
    fn up_sidecar(&self) -> Result<()> {
        self.check_env()?;
        // sidecar 服务先起, --wait 等健康检查通过
        self.run(&[
            "up", "-d", "--wait", "traefik", "redis", "postgres", "qdrant",
        ])?;
        tracing::info!("sidecar 平面已起 (traefik/redis/postgres/qdrant 健康)");
        Ok(())
    }

    fn up_business(&self) -> Result<()> {
        self.run(&["up", "-d", "--wait", "fusion-model-hub", "fusion-cowork"])?;
        tracing::info!("business 容器已起 (model-hub/cowork 健康)");
        Ok(())
    }

    fn down(&self) -> Result<()> {
        // down 容忍部分缺失 (orphan 清理)
        let mut full = self.base_cmd();
        full.push("down".into());
        full.push("--remove-orphans".into());
        let out = std::process::Command::new("docker")
            .args(&full[1..])
            .output()
            .map_err(|e| anyhow::anyhow!("spawn docker compose down: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // down 失败多为容器已停, 仅告警不致命
            tracing::warn!(stderr = %stderr.trim(), "compose down 非零退出 (容许)");
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
        // fail-closed: 必需密码, 缺则拒绝并指 .env.example
        // docker --env-file 会注入文件内变量, 故查 env + env_file 两处
        let need = ["FUSION_PG_PASSWORD", "FUSION_MLX_API_KEY"];
        for k in need {
            if std::env::var(k).is_err() && !key_in_env_file(&self.env_file, k) {
                anyhow::bail!(
                    "缺少 {k} — compose fail-closed 守卫。填 .env (参考 .env.example) 或 export {k}"
                );
            }
        }
        Ok(())
    }
}

// 检查 env_file 中是否定义了某 key (docker --env-file 会注入)
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

// 解析 docker compose ps --format json 输出。
// compose v2 输出: 多行 JSON 数组 (每行一个对象) 或单个 JSON 数组, 兼容两种。
pub fn parse_compose_ps(text: &str) -> Result<Vec<ContainerStatus>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }
    // 尝试整体 JSON 数组 (compose v2 新版)
    if trimmed.starts_with('[') {
        let arr: Vec<RawContainer> = serde_json::from_str(trimmed)
            .map_err(|e| anyhow::anyhow!("ps JSON 数组解析失败: {e}"))?;
        return Ok(arr.into_iter().map(RawContainer::into_status).collect());
    }
    // 逐行 JSON 对象 (compose v2 旧版/ndjson)
    let mut out = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let raw: RawContainer =
            serde_json::from_str(line).map_err(|e| anyhow::anyhow!("ps JSON 行解析失败: {e}"))?;
        out.push(raw.into_status());
    }
    Ok(out)
}

// docker compose ps --format json 原始字段 (compose v2 = PascalCase: Name/Service/State/Health/Publishers)
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
    url: String, // "0.0.0.0:5432:5432/tcp" 或 published_url
}

impl RawContainer {
    fn into_status(self) -> ContainerStatus {
        // publishers.url 格式 "0.0.0.0:5432:5432/tcp" → 规整为 "0.0.0.0:5432->5432/tcp"
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
    // 已是 -> 格式直接返回
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

// 测试支持: FakeCompose 暴露给 supervisor 集成测试 (仅 cfg test)
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

    // compose v2 单 JSON 数组格式
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

    // ndjson 逐行格式
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
