use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

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
    pub registry_path: PathBuf,
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
            registry_path: PathBuf::from("architecture/port-registry.yaml"),
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
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&expanded)?;
        let cfg: Config = toml::from_str(&text)?;
        Ok(cfg)
    }
}

fn expand_tilde(p: &PathBuf) -> PathBuf {
    if let Some(s) = p.to_str() {
        if s.starts_with("~") {
            if let Some(home) = dirs_or_home() {
                return PathBuf::from(s.replacen("~", &home, 1));
            }
        }
    }
    p.clone()
}

fn dirs_or_home() -> Option<String> {
    std::env::var("HOME").ok()
}
