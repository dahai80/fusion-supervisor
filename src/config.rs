use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    // 全字段 serde default: 配置文件可只写覆盖项 (文档承诺 "all fields have defaults",
    // 之前无 default 的字段会让部分配置文件直接 parse 失败 — 与文档矛盾)。
    #[serde(default = "default_socket_path")]
    pub socket_path: String,
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
    #[serde(default = "default_poll_healthy_sec")]
    pub poll_healthy_sec: u64,
    #[serde(default = "default_poll_unhealthy_sec")]
    pub poll_unhealthy_sec: u64,
    #[serde(default = "default_max_concurrent_start")]
    pub max_concurrent_start: usize,
    #[serde(default = "default_start_timeout_sec")]
    pub start_timeout_sec: u64,
    #[serde(default = "default_restart_window_sec")]
    pub restart_window_sec: u64,
    #[serde(default = "default_restart_max")]
    pub restart_max: u32,
    #[serde(default = "default_backoff_max_sec")]
    pub backoff_max_sec: u64,
    #[serde(default)]
    pub alert_url: String,
    #[serde(default = "default_alert_dedup_sec")]
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
    // issue #37: history.db 健康史/事件保留天数。daemon 周期 sweep 清除过期行。
    #[serde(default = "default_history_retention_days")]
    pub history_retention_days: u32,
    // issue #37: sweep 调度间隔 (秒)。到期执行一次 cleanup_old; 失败也更新时间戳
    // (避免每 tick 重试风暴, 同 backup 调度语义)。
    #[serde(default = "default_prune_schedule_sec")]
    pub prune_schedule_sec: u64,
    #[serde(default = "default_registry_path")]
    pub registry_path: PathBuf,
    #[serde(default)]
    pub compose: ComposeConfig,
    // issue #22: Prometheus /metrics HTTP 端点监听地址。
    // Some → 启用端点; None → 不启 HTTP server (默认 Some("127.0.0.1:9100"))。
    // 仅 localhost 绑定是安全边界 (不对外暴露, 不加 auth)。
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: Option<String>,
    // issue #25: web 控制台静态产物目录 (web/dist)。
    // Some → HTTP listener 同时服务 SPA 静态文件; None → 不服务 (默认)。
    #[serde(default)]
    pub web_dir: Option<String>,
    // issue #29: daemon 启动时自动执行一次 up_all (探活跳过已健康, 不双启)。
    // 主机重启后 launchd/操作员拉起 daemon → 全量服务按 layer 序自愈。
    // 默认 false (开发机手动 fusion-sv up 的行为不变)。
    #[serde(default)]
    pub auto_up_on_start: bool,
    // issue #77: native 备份平面 (compose.enabled=false 时生效) 的 pg_dumpall 二进制覆盖。
    // None → 从 PATH 解析 "pg_dumpall"。版本须与目标 server 匹配 (brew 多版本共存时注意)。
    #[serde(default)]
    pub pg_dump_bin: Option<String>,
    // issue #77: native brew PostgreSQL 连接参数 (authoritative 实例,
    // 见 architecture/fusion-db-conventions-1030.md §1)。用户名走 FUSION_PG_USER env,
    // 未设则省略 -U (pg_dumpall 默认 OS 用户, brew trust auth 下即安装用户)。
    #[serde(default = "default_native_pg_host")]
    pub native_pg_host: String,
    #[serde(default = "default_native_pg_port")]
    pub native_pg_port: u16,
    // issue #80: daemon 自身 PATH 的追加项 (冒号分隔)。launchd 默认 PATH 不含
    // /opt/homebrew/bin — node/brew 系 start.sh 在 daemon 拉起时 `command -v node`
    // 全挂。daemon 启动时把缺失项追加到自身 PATH 尾部, 子进程 (start.sh /
    // daemonize) 全部继承; 空串禁用 (保持 launchd 原始 PATH)。
    #[serde(default = "default_native_extra_path")]
    pub native_extra_path: String,
}

// compose 平面配置。enabled=false (默认) → 仅 native, 保留单节点开发行为。
// 注意不能 derive(Default): derive 给 PathBuf 的默认是空串, 而 serde default = "..."
// 函数只在 toml 显式缺 key 时生效 — derive(Default) 会让无 [compose] 段的配置文件
// 拿到空 env_file/sidecar_file, 破坏 .env 解析。手动 impl 走真正的默认值。
#[derive(Debug, Clone, Deserialize)]
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

impl Default for ComposeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sidecar_file: default_sidecar(),
            business_file: default_business(),
            env_file: default_env(),
        }
    }
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
fn default_history_retention_days() -> u32 {
    7
}
fn default_prune_schedule_sec() -> u64 {
    3600
}
fn default_socket_path() -> String {
    "/tmp/fusion-sv.sock".to_string()
}
fn default_db_path() -> PathBuf {
    PathBuf::from("~/.fusion-sv/history.db")
}
fn default_poll_healthy_sec() -> u64 {
    15
}
fn default_poll_unhealthy_sec() -> u64 {
    2
}
fn default_max_concurrent_start() -> usize {
    4
}
fn default_start_timeout_sec() -> u64 {
    30
}
fn default_restart_window_sec() -> u64 {
    300
}
fn default_restart_max() -> u32 {
    5
}
fn default_backoff_max_sec() -> u64 {
    30
}
fn default_alert_dedup_sec() -> u64 {
    300
}
fn default_registry_path() -> PathBuf {
    PathBuf::from("architecture/port-registry.yaml")
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
// issue #22: metrics 默认绑 localhost:9100 (不对外, 无 auth)。
fn default_metrics_addr() -> Option<String> {
    Some("127.0.0.1:9100".to_string())
}
// issue #77: native brew PG 默认连接参数 (authoritative 实例)。
fn default_native_pg_host() -> String {
    "127.0.0.1".to_string()
}
fn default_native_pg_port() -> u16 {
    5432
}
// issue #80: Apple Silicon (/opt/homebrew) + Intel (/usr/local) 两处 Homebrew bin。
fn default_native_extra_path() -> String {
    "/opt/homebrew/bin:/usr/local/bin".to_string()
}

// issue #80: 把 extra 中的 PATH 项追加到 cur 尾部 — 已存在的项跳过 (保序去重,
// 追加-only 不改变既有解析优先级), 空项/空白项忽略。纯函数便于单测。
pub fn append_path_entries(cur: &str, extra: &str) -> String {
    let mut entries: Vec<&str> = cur.split(':').filter(|s| !s.is_empty()).collect();
    for e in extra.split(':') {
        let e = e.trim();
        if e.is_empty() || entries.contains(&e) {
            continue;
        }
        entries.push(e);
    }
    entries.join(":")
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
            history_retention_days: 7,
            prune_schedule_sec: 3600,
            registry_path: PathBuf::from("architecture/port-registry.yaml"),
            compose: ComposeConfig::default(),
            metrics_addr: default_metrics_addr(),
            web_dir: None,
            auto_up_on_start: false,
            pg_dump_bin: None,
            native_pg_host: default_native_pg_host(),
            native_pg_port: default_native_pg_port(),
            native_extra_path: default_native_extra_path(),
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
            cfg.compose.env_file = resolve_env_file(&cfg.compose.env_file);
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(&expanded)?;
        let mut cfg: Config = toml::from_str(&text)?;
        cfg.db_path = expand_tilde(&cfg.db_path);
        cfg.registry_path = expand_tilde(&cfg.registry_path);
        cfg.compose.sidecar_file = expand_tilde(&cfg.compose.sidecar_file);
        cfg.compose.business_file = expand_tilde(&cfg.compose.business_file);
        cfg.compose.env_file = expand_tilde(&cfg.compose.env_file);
        cfg.compose.env_file = resolve_env_file(&cfg.compose.env_file);
        // issue #25: web_dir 同样展开 ~ (与 db_path 一致)。
        if let Some(dir) = &cfg.web_dir {
            cfg.web_dir = Some(expand_tilde(Path::new(dir)).to_string_lossy().into_owned());
        }
        // issue #22: 空字符串 → None (禁用 metrics 端点)。
        if let Some(addr) = &cfg.metrics_addr
            && addr.trim().is_empty()
        {
            cfg.metrics_addr = None;
        }
        // issue #25: 空字符串 → None (不服务 web 静态面)。
        if let Some(dir) = &cfg.web_dir
            && dir.trim().is_empty()
        {
            cfg.web_dir = None;
        }
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

// issue #31: 相对 env_file 在 FUSION_ROOT 设置且该处存在时优先相对 monorepo 根解析。
// README 引导从 monorepo 根启动 daemon (start.sh 路径解析依赖 FUSION_ROOT), 若 .env
// 仍按 CWD 找, 密钥文件留在 fusion-supervisor/ 会读不到 → shell env fail-closed 拒启动。
// 显式绝对路径 / ~ 展开后不受影响; FUSION_ROOT 下不存在则回退 CWD (旧行为)。
fn resolve_env_file(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    if let Ok(root) = std::env::var("FUSION_ROOT") {
        let candidate = PathBuf::from(root).join(p);
        if candidate.exists() {
            return candidate;
        }
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

    // 部分配置文件 (只写覆盖项) 应可加载, 其余字段取默认 —
    // 文档承诺 "all fields have defaults", 之前无 serde default 的字段直接 parse 失败。
    #[test]
    fn test_partial_config_file_uses_defaults() {
        let path = std::env::temp_dir().join("fusion-sv-config-partial-test.toml");
        std::fs::write(&path, "auto_up_on_start = true\n").unwrap();
        let _guard = EnvGuard::set("FUSION_SV_CONFIG", path.to_str().unwrap());
        // issue #31: env 全局共享, config 测试也须与其它 env 测试共用锁串行
        let _g0 = crate::env_map::tests::env_lock().lock().unwrap();
        // 钉住 FUSION_ROOT 到无 .env 的临时目录, 断言不受宿主环境影响
        let empty_root = std::env::temp_dir().join("fusion-sv-config-test-root");
        std::fs::create_dir_all(&empty_root).unwrap();
        let _guard2 = EnvGuard::set("FUSION_ROOT", empty_root.to_str().unwrap());
        let cfg = Config::load().expect("部分配置应加载成功");
        assert!(cfg.auto_up_on_start, "覆盖项应生效");
        assert_eq!(cfg.socket_path, "/tmp/fusion-sv.sock");
        assert_eq!(cfg.poll_healthy_sec, 15);
        assert_eq!(cfg.max_concurrent_start, 4);
        assert!(!cfg.compose.enabled);
        // 回归: 无 [compose] 段时 env_file 应为 ".env" 而非 derive(Default) 的空串
        assert_eq!(cfg.compose.env_file, PathBuf::from(".env"));
        assert_eq!(
            cfg.compose.sidecar_file,
            PathBuf::from("docker-compose.sidecar.yml")
        );
        let _ = std::fs::remove_file(&path);
    }

    // issue #31: 相对 env_file 优先解析到 FUSION_ROOT 下 (存在时), 否则回退 CWD
    #[test]
    fn test_resolve_env_file_fusion_root_priority() {
        // issue #31: 同上, env 锁串行
        let _g = crate::env_map::tests::env_lock().lock().unwrap();
        let root = std::env::temp_dir().join("fusion-sv-envroot-test");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".env"), "K=V\n").unwrap();
        let _guard = EnvGuard::set("FUSION_ROOT", root.to_str().unwrap());
        assert_eq!(
            resolve_env_file(Path::new(".env")),
            root.join(".env"),
            "FUSION_ROOT 下存在 .env 时应优先解析到根"
        );
        assert_eq!(
            resolve_env_file(Path::new("other.env")),
            PathBuf::from("other.env"),
            "FUSION_ROOT 下不存在时回退 CWD 相对路径"
        );
        let abs = root.join("abs.env");
        assert_eq!(resolve_env_file(&abs), abs, "绝对路径原样返回");
        let _ = std::fs::remove_dir_all(&root);
    }

    // issue #80: 缺失项追加到尾部 (追加-only, 不改变既有解析顺序)
    #[test]
    fn test_append_path_entries_appends_missing() {
        assert_eq!(
            append_path_entries("/usr/bin:/bin", "/opt/homebrew/bin:/usr/local/bin"),
            "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin"
        );
    }

    // issue #80: 已存在的项跳过 — launchd 环境已含 Homebrew bin 时幂等
    #[test]
    fn test_append_path_entries_dedups_existing() {
        assert_eq!(
            append_path_entries(
                "/opt/homebrew/bin:/usr/bin:/bin",
                "/opt/homebrew/bin:/usr/local/bin"
            ),
            "/opt/homebrew/bin:/usr/bin:/bin:/usr/local/bin"
        );
        // 全部已存在 → 不变
        assert_eq!(append_path_entries("/a:/b", "/a:/b"), "/a:/b");
    }

    // issue #80: extra 为空串 (禁用) / 空项 / 空白项 → cur 原样 (仅去空项)
    #[test]
    fn test_append_path_entries_empty_and_trim() {
        assert_eq!(append_path_entries("/usr/bin:/bin", ""), "/usr/bin:/bin");
        assert_eq!(
            append_path_entries("/usr/bin:::/bin", "  "),
            "/usr/bin:/bin"
        );
        // 空白项 trim 后追加
        assert_eq!(
            append_path_entries("/usr/bin", "  /opt/homebrew/bin  "),
            "/usr/bin:/opt/homebrew/bin"
        );
    }

    // issue #80: cur 为空 (PATH 未设) → 仅剩 extra 的有效项
    #[test]
    fn test_append_path_entries_empty_cur() {
        assert_eq!(
            append_path_entries("", "/opt/homebrew/bin:/usr/local/bin"),
            "/opt/homebrew/bin:/usr/local/bin"
        );
        assert_eq!(append_path_entries("", ""), "");
    }
}
