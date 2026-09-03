use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub socket_path: String,
    pub db_path: PathBuf,
    pub poll_healthy_sec: u64,
    pub poll_unhealthy_sec: u64,
    pub max_concurrent_start: usize,
    pub start_timeout_sec: u64,
    pub restart_window_sec: u64,
    pub restart_max: u32,
    pub backoff_max_sec: u64,
    pub alert_url: String,
    pub alert_dedup_sec: u64,
    // compose 容器连续失败达此次数 → 升级告警 (ContainerCrashLoop, issue #6)
    #[serde(default = "default_container_crash_escalation")]
    pub container_crash_escalation: u32,
    // issue #9: drain 宽限期 (秒)。到期强制 Stopped (in-flight 未完也停)。
    #[serde(default = "default_drain_grace_sec")]
    pub drain_grace_sec: u64,
    // issue #9: rollout prewarm 超时 (秒)。启动后探活在此时间内未达 Healthy → 告警 + 留 Stopped。
    #[serde(default = "default_rollout_prewarm_timeout_sec")]
    pub rollout_prewarm_timeout_sec: u64,
    // issue #10: 备份调度间隔 (秒)。daemon tick 循环检查, 到期触发 backup。默认 86400 (每日)。
    #[serde(default = "default_backup_schedule_sec")]
    pub backup_schedule_sec: u64,
    // issue #10: 备份保留份数。新备份后裁剪超出 N 份的旧备份, 默认 7。
    #[serde(default = "default_backup_retention")]
    pub backup_retention: u32,
    pub registry_path: PathBuf,
    #[serde(default)]
    pub compose: ComposeConfig,
}

// compose 平面配置。enabled=false (默认) → 仅 native, 保留单节点开发行为。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ComposeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_sidecar")]
    pub sidecar_file: PathBuf,
    #[serde(default = "default_business")]
    pub business_file: PathBuf,
    #[serde(default = "default_env")]
    pub env_file: PathBuf,
}

fn default_sidecar() -> PathBuf {
    PathBuf::from("docker-compose.sidecar.yml")
}
fn default_business() -> PathBuf {
    PathBuf::from("docker-compose.business.yml")
}
fn default_env() -> PathBuf {
    PathBuf::from(".env")
}
fn default_container_crash_escalation() -> u32 {
    5
}
fn default_drain_grace_sec() -> u64 {
    30
}
fn default_rollout_prewarm_timeout_sec() -> u64 {
    60
}
fn default_backup_schedule_sec() -> u64 {
    86400
}
fn default_backup_retention() -> u32 {
    7
}

impl Default for Config {
    fn default() -> Self {
        Self {
            socket_path: "/tmp/fusion-sv.sock".to_string(),
            db_path: PathBuf::from("~/.fusion-sv/history.db"),
            poll_healthy_sec: 15,
            poll_unhealthy_sec: 2,
            max_concurrent_start: 4,
            start_timeout_sec: 30,
            restart_window_sec: 300,
            restart_max: 5,
            backoff_max_sec: 30,
            alert_url: String::new(),
            alert_dedup_sec: 300,
            container_crash_escalation: 5,
            drain_grace_sec: 30,
            rollout_prewarm_timeout_sec: 60,
            backup_schedule_sec: 86400,
            backup_retention: 7,
            registry_path: PathBuf::from("architecture/port-registry.yaml"),
            compose: ComposeConfig::default(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = std::env::var("FUSION_SV_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("~/.fusion-sv/config.toml"));
        let expanded = expand_tilde(&path);
        if !expanded.exists() {
            tracing::info!("config 不存在, 用默认值: {}", expanded.display());
            let mut cfg = Config::default();
            cfg.db_path = expand_tilde(&cfg.db_path);
            cfg.registry_path = expand_tilde(&cfg.registry_path);
            cfg.compose = ComposeConfig::default();
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(&expanded)?;
        let mut cfg: Config = toml::from_str(&text)?;
        cfg.db_path = expand_tilde(&cfg.db_path);
        cfg.registry_path = expand_tilde(&cfg.registry_path);
        cfg.compose.sidecar_file = expand_tilde(&cfg.compose.sidecar_file);
        cfg.compose.business_file = expand_tilde(&cfg.compose.business_file);
        cfg.compose.env_file = expand_tilde(&cfg.compose.env_file);
        Ok(cfg)
    }
}

fn expand_tilde(p: &Path) -> PathBuf {
    if let Some(s) = p.to_str()
        && s.starts_with("~")
        && let Some(home) = dirs_or_home()
    {
        return PathBuf::from(s.replacen("~", &home, 1));
    }
    p.to_path_buf()
}

fn dirs_or_home() -> Option<String> {
    std::env::var("HOME").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // RAII 守卫: 设 env, Drop 时恢复原值 (panic 也会跑 Drop, 不污染其他测试)。
    // edition 2024 起 set_var/remove_var 为 unsafe, 故包 unsafe 块。
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: 测试单线程内设 env, 此处无并发读此 key 的代码。
            unsafe { std::env::set_var(key, val) };
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: 同上, 测试内恢复 env。
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    // I1: load() 在无配置文件分支应展开 db_path 的 ~ 为 $HOME,
    // 不留字面 "~" 目录。回归: 之前 load 返回 Default 带 "~/.fusion-sv/history.db"。
    #[test]
    fn test_load_expands_tilde_in_db_path() {
        let nonexistent = std::env::temp_dir().join("fusion-sv-config-noexist-test.toml");
        let _ = std::fs::remove_file(&nonexistent);
        let _guard = EnvGuard::set("FUSION_SV_CONFIG", nonexistent.to_str().unwrap());
        let cfg = Config::load().expect("load 应成功 (无文件用默认)");
        let home = std::env::var("HOME").expect("HOME 应存在");
        assert!(
            !cfg.db_path.starts_with("~"),
            "db_path 不应保留字面 ~: {:?}",
            cfg.db_path
        );
        assert!(
            cfg.db_path.starts_with(&home),
            "db_path 应以 $HOME 开头: {:?} vs {}",
            cfg.db_path,
            home
        );
        assert!(
            !cfg.registry_path.starts_with("~") || cfg.registry_path.starts_with(&home),
            "registry_path 不应保留字面 ~: {:?}",
            cfg.registry_path
        );
        let _ = std::fs::remove_file(&nonexistent);
    }
}
