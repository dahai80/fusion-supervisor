use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    // All fields have serde defaults: config files may carry only overrides (docs promise
    // "all fields have defaults"; fields without defaults used to make some config files
    // fail to parse outright — contradicting the docs).
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
    // issue #6: compose container failing this many times in a row → escalated alert (ContainerCrashLoop)
    #[serde(default = "default_container_crash_escalation")]
    pub container_crash_escalation: u32,
    // issue #9: drain grace period (seconds). On expiry force Stopped (even if in-flight work remains).
    #[serde(default = "default_drain_grace_sec")]
    pub drain_grace_sec: u64,
    // issue #9: rollout prewarm timeout (seconds). If probes do not reach Healthy within this window after start → alert + leave Stopped.
    #[serde(default = "default_rollout_prewarm_timeout_sec")]
    pub rollout_prewarm_timeout_sec: u64,
    // issue #10: backup schedule interval (seconds). Checked by the daemon tick loop, triggers backup when due. Default 86400 (daily).
    #[serde(default = "default_backup_schedule_sec")]
    pub backup_schedule_sec: u64,
    // issue #10: backup retention count. After a new backup, prune old backups beyond N. Default 7.
    #[serde(default = "default_backup_retention")]
    pub backup_retention: u32,
    // issue #132: weekly backup retention — on top of daily retention (backup_retention), keep the
    // newest N backups per natural week among prune candidates. 0 = disabled (old flat behavior). Default 0.
    #[serde(default)]
    pub backup_weekly_retention: u32,
    // issue #37: history.db health-history/event retention days. The daemon periodically sweeps expired rows.
    #[serde(default = "default_history_retention_days")]
    pub history_retention_days: u32,
    // issue #37: sweep schedule interval (seconds). Runs cleanup_old once when due; the timestamp is
    // updated even on failure (avoids a per-tick retry storm, same semantics as the backup schedule).
    #[serde(default = "default_prune_schedule_sec")]
    pub prune_schedule_sec: u64,
    #[serde(default = "default_registry_path")]
    pub registry_path: PathBuf,
    // issue #145: systemd-managed service table (service name → systemd unit).
    // On VPS deployments systemd is the lifecycle authority — supervisor start/stop/restart
    // drives the unit via systemctl instead of start.sh (start.sh pidfiles do not work for
    // systemd-spawned processes; stop cannot kill the real process → fake stop + port-in-use
    // bail). Mac does not configure this (default empty table, everything goes through
    // start.sh / start_map). Unit names get fail-closed character validation.
    #[serde(default)]
    pub systemd_units: HashMap<String, String>,
    #[serde(default)]
    pub compose: ComposeConfig,
    // issue #22: listen address for the Prometheus /metrics HTTP endpoint.
    // Some → endpoint enabled; None → no HTTP server (default Some("127.0.0.1:9100")).
    // Loopback-only binding is a security boundary (not exposed externally, no auth).
    #[serde(default = "default_metrics_addr")]
    pub metrics_addr: Option<String>,
    // issue #25: web console static asset directory (web/dist).
    // Some → the HTTP listener also serves SPA static files; None → not served (default).
    #[serde(default)]
    pub web_dir: Option<String>,
    // issue #29: run one up_all automatically at daemon start (probe-first skips already-healthy, no double start).
    // After a host reboot, launchd/operator starts the daemon → full fleet self-heals in layer order.
    // Default false (dev-machine manual `fusion-sv up` behavior unchanged).
    #[serde(default)]
    pub auto_up_on_start: bool,
    // issue #77: pg_dumpall binary override for the native backup plane (effective when compose.enabled=false).
    // None → resolve "pg_dumpall" from PATH. Version must match the target server (mind brew multi-version coexistence).
    #[serde(default)]
    pub pg_dump_bin: Option<String>,
    // issue #77: native brew PostgreSQL connection parameters (authoritative instance,
    // see architecture/fusion-db-conventions-1030.md §1). The username goes through the FUSION_PG_USER env;
    // when unset, -U is omitted (pg_dumpall defaults to the OS user, which under brew trust auth is the installing user).
    #[serde(default = "default_native_pg_host")]
    pub native_pg_host: String,
    #[serde(default = "default_native_pg_port")]
    pub native_pg_port: u16,
    // issue #80: extra PATH entries appended to the daemon's own PATH (colon-separated). The launchd
    // default PATH lacks /opt/homebrew/bin — node/brew-based start.sh scripts all fail `command -v node`
    // when started by the daemon. At daemon start, missing entries are appended to the tail of its own
    // PATH and inherited by all child processes (start.sh / daemonize); an empty string disables this
    // (keeps the raw launchd PATH).
    #[serde(default = "default_native_extra_path")]
    pub native_extra_path: String,
    // issue #126: startup grace window (seconds). After a successful exec start, probe failures within
    // the grace window do not count toward fail_streak / do not schedule restarts (slow-start daemons
    // need 30-60s to bind their port; the old logic burned consecutive-failure counts during the startup
    // window → due restarts killed healthy processes still starting). 0 = disabled.
    #[serde(default = "default_startup_grace_secs")]
    pub startup_grace_secs: u64,
    // issue #137: scheduled job table (one-shot jobs such as fusion-ops retention/cluster).
    // The daemon tick loop checks due jobs and runs them detached; the cursor persists in meta job_last:<name>.
    #[serde(default)]
    pub jobs: Vec<JobConfig>,
    // issue #168: daemon log size rotation (copy-truncate).
    // log_rotate_max_mb: rotate a file once it exceeds this many MB; 0 = disabled (default 100).
    // log_rotate_keep: number of backup copies kept <file>.1 .. <file>.N (default 3).
    // log_rotate_paths: rotation targets — defaults to the two launchd StandardOut/ErrPath daemon
    // logs (resolved relative to FUSION_ROOT, see log_rotate::resolve_rotate_path).
    #[serde(default = "default_log_rotate_max_mb")]
    pub log_rotate_max_mb: u64,
    #[serde(default = "default_log_rotate_keep")]
    pub log_rotate_keep: u32,
    #[serde(default = "default_log_rotate_paths")]
    pub log_rotate_paths: Vec<String>,
}

fn default_log_rotate_max_mb() -> u64 {
    100
}

fn default_log_rotate_keep() -> u32 {
    3
}

fn default_log_rotate_paths() -> Vec<String> {
    vec![
        "fusion-supervisor/logs/daemon.launchd.log".to_string(),
        "fusion-supervisor/logs/daemon.launchd.err.log".to_string(),
    ]
}

// issue #137: a single scheduled job. Exactly one of daily_at / every_sec must be set (validated in load).
#[derive(Debug, Clone, Deserialize)]
pub struct JobConfig {
    pub name: String,
    pub bin: String,
    #[serde(default)]
    pub args: Vec<String>,
    // Run once daily at local HH:MM (retention-style nightly jobs)
    #[serde(default)]
    pub daily_at: Option<String>,
    // Fixed interval in seconds (cluster-style periodic jobs)
    #[serde(default)]
    pub every_sec: Option<u64>,
    // Execution timeout; killed when reached. Default 900 (upstream job binaries carry their own 10min ctx).
    #[serde(default = "default_job_timeout_sec")]
    pub timeout_sec: u64,
}

fn default_job_timeout_sec() -> u64 {
    900
}

// issue #137: jobs config validation — name non-empty and unique; exactly one of daily_at / every_sec;
// daily_at format HH:MM (0-23:0-59). Failure = config fatal (daemon refuses to start, fail-closed).
fn validate_jobs(jobs: &[JobConfig]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for j in jobs {
        anyhow::ensure!(!j.name.trim().is_empty(), "job name must not be empty");
        anyhow::ensure!(
            seen.insert(j.name.clone()),
            "duplicate job name: {}",
            j.name
        );
        anyhow::ensure!(
            j.daily_at.is_some() ^ j.every_sec.is_some(),
            "job {} must set exactly one of daily_at / every_sec",
            j.name
        );
        if let Some(hm) = &j.daily_at {
            let bad = hm
                .split_once(':')
                .and_then(|(h, m)| {
                    Some(h.trim().parse::<u32>().ok()? <= 23 && m.trim().parse::<u32>().ok()? <= 59)
                })
                .unwrap_or(false);
            anyhow::ensure!(bad, "job {} daily_at must be HH:MM (00:00-23:59)", j.name);
        }
    }
    Ok(())
}

fn default_startup_grace_secs() -> u64 {
    90
}

// Compose-plane config. enabled=false (default) → native only, preserving single-node dev behavior.
// Note: cannot derive(Default) — derive gives PathBuf an empty-string default, and serde default = "..."
// functions only apply when the toml key is explicitly absent, so derive(Default) would hand config files
// without a [compose] section empty env_file/sidecar_file, breaking .env parsing. The manual impl uses real defaults.
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
// issue #22: metrics binds localhost:9100 by default (not exposed, no auth).
fn default_metrics_addr() -> Option<String> {
    Some("127.0.0.1:9100".to_string())
}
// issue #77: native brew PG default connection parameters (authoritative instance).
fn default_native_pg_host() -> String {
    "127.0.0.1".to_string()
}
fn default_native_pg_port() -> u16 {
    5432
}
// issue #80: Homebrew bin on both Apple Silicon (/opt/homebrew) and Intel (/usr/local).
fn default_native_extra_path() -> String {
    "/opt/homebrew/bin:/usr/local/bin".to_string()
}

// issue #80: append PATH entries from extra to the tail of cur — existing entries are skipped (order-preserving
// dedup, append-only keeps existing resolution priority), empty/whitespace entries ignored. Pure function for unit tests.
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
            backup_weekly_retention: 0,
            history_retention_days: 7,
            prune_schedule_sec: 3600,
            log_rotate_max_mb: default_log_rotate_max_mb(),
            log_rotate_keep: default_log_rotate_keep(),
            log_rotate_paths: default_log_rotate_paths(),
            registry_path: PathBuf::from("architecture/port-registry.yaml"),
            systemd_units: HashMap::new(),
            compose: ComposeConfig::default(),
            metrics_addr: default_metrics_addr(),
            web_dir: None,
            auto_up_on_start: false,
            pg_dump_bin: None,
            native_pg_host: default_native_pg_host(),
            native_pg_port: default_native_pg_port(),
            native_extra_path: default_native_extra_path(),
            startup_grace_secs: default_startup_grace_secs(),
            jobs: Vec::new(),
        }
    }
}

// issue #136: deployment-convention config path (docs/backup-restore.md VPS systemd deploy;
// the daemon unit's FUSION_SV_CONFIG points here). The CLI adopts it when FUSION_SV_CONFIG is unset
// and the default path is missing, avoiding a root-run CLI silently using defaults (retention etc.).
const DEPLOY_CONFIG_PATH: &str = "/opt/fusion-sv/config.toml";

// Pure function: resolve the config path. explicit (FUSION_SV_CONFIG) always wins; when unset the
// default is ~/.fusion-sv/config.toml, but if the default is missing and the deploy-convention path
// exists, the deploy config is adopted (adopted=true). The caller probes both exists flags inline, easing test injection.
fn resolve_config_path(
    explicit: Option<String>,
    default_exists: bool,
    deploy_exists: bool,
) -> (PathBuf, bool) {
    if let Some(p) = explicit {
        return (PathBuf::from(p), false);
    }
    let default = PathBuf::from("~/.fusion-sv/config.toml");
    if !default_exists && deploy_exists {
        return (PathBuf::from(DEPLOY_CONFIG_PATH), true);
    }
    (default, false)
}

impl Config {
    pub fn load() -> Result<Self> {
        let explicit = std::env::var("FUSION_SV_CONFIG").ok();
        let default_expanded = expand_tilde(&PathBuf::from("~/.fusion-sv/config.toml"));
        let deploy_path = PathBuf::from(DEPLOY_CONFIG_PATH);
        let (path, adopted) =
            resolve_config_path(explicit, default_expanded.exists(), deploy_path.exists());
        let expanded = expand_tilde(&path);
        // issue #136: FUSION_SV_CONFIG unset and default + deploy configs coexisting → WARN.
        // The two configs may carry different retention/schedule settings; CLI and daemon diverge silently
        // (observed on VPS: /root/.fusion-sv/config.toml ≠ /opt/fusion-sv/config.toml).
        if !adopted
            && std::env::var("FUSION_SV_CONFIG").is_err()
            && deploy_path.exists()
            && deploy_path != expanded
        {
            tracing::warn!(
                cli_config = %expanded.display(),
                daemon_config = DEPLOY_CONFIG_PATH,
                "CLI default config coexists with daemon deploy config; settings may diverge (set FUSION_SV_CONFIG to align one-shot CLI runs)"
            );
        }
        if !expanded.exists() {
            // issue #134: CLI and daemon may each hold their own config (daemon uses the systemd
            // FUSION_SV_CONFIG, CLI defaults to ~/.fusion-sv/config.toml) — when falling back to
            // defaults, upgrade to WARN with an alignment hint so "CLI silently runs the wrong config" is attributable.
            tracing::warn!(
                path = %expanded.display(),
                "config not found, using defaults (if a daemon runs with FUSION_SV_CONFIG set, set the same variable for one-shot CLI runs)"
            );
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
        // issue #25: expand ~ in web_dir too (consistent with db_path).
        if let Some(dir) = &cfg.web_dir {
            cfg.web_dir = Some(expand_tilde(Path::new(dir)).to_string_lossy().into_owned());
        }
        // issue #22: empty string → None (disable the metrics endpoint).
        if let Some(addr) = &cfg.metrics_addr
            && addr.trim().is_empty()
        {
            cfg.metrics_addr = None;
        }
        // issue #25: empty string → None (do not serve the web static face).
        if let Some(dir) = &cfg.web_dir
            && dir.trim().is_empty()
        {
            cfg.web_dir = None;
        }
        validate_jobs(&cfg.jobs)?;
        // issue #145: systemd unit name fail-closed validation (prevents shell metacharacter injection into systemctl args)
        for (svc, unit) in &cfg.systemd_units {
            anyhow::ensure!(
                !svc.trim().is_empty() && !unit.trim().is_empty(),
                "systemd_units entries must be non-empty (got {svc:?} -> {unit:?})"
            );
            anyhow::ensure!(
                unit.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-._@".contains(c)),
                "systemd unit {unit:?} for {svc} has invalid characters (allowed: alnum - . _ @)"
            );
        }
        Ok(cfg)
    }
}

pub(crate) fn expand_tilde(p: &Path) -> PathBuf {
    // Only expand "~" and "~/.."; "~user"/"~suffix"-style paths like ~backup are left untouched
    if let Some(s) = p.to_str()
        && (s == "~" || s.starts_with("~/"))
        && let Some(home) = dirs_or_home()
    {
        return PathBuf::from(s.replacen("~", &home, 1));
    }
    p.to_path_buf()
}

// issue #31: a relative env_file resolves preferentially against the monorepo root when FUSION_ROOT is set
// and the file exists there. The README guides starting the daemon from the monorepo root (start.sh path
// resolution depends on FUSION_ROOT); if .env is still looked up relative to CWD, a secret file left in
// fusion-supervisor/ would not be found → shell env fail-closed refuses to start.
// Explicit absolute paths / ~-expanded paths are unaffected; fall back to CWD when absent under FUSION_ROOT (old behavior).
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

    // RAII guard: sets an env var, restores the original on Drop (Drop also runs on panic, so other tests are not polluted).
    // Since edition 2024, set_var/remove_var are unsafe, hence the unsafe blocks.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
        // Holds the env test lock (#134 CI flake): set/restore is mutually exclusive with other env tests throughout.
        _lock: crate::env_map::tests::EnvLockGuard,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let lock = crate::env_map::tests::env_lock();
            let prev = std::env::var(key).ok();
            // SAFETY: env is set within a single test thread; no code concurrently reads this key here.
            unsafe { std::env::set_var(key, val) };
            Self {
                key,
                prev,
                _lock: lock,
            }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: same as above, restoring env within the test.
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    // I1: in the no-config-file branch, load() should expand ~ in db_path to $HOME,
    // never leaving a literal "~" directory. Regression: load used to return Default with "~/.fusion-sv/history.db".
    #[test]
    fn test_load_expands_tilde_in_db_path() {
        let nonexistent = std::env::temp_dir().join("fusion-sv-config-noexist-test.toml");
        let _ = std::fs::remove_file(&nonexistent);
        let _guard = EnvGuard::set("FUSION_SV_CONFIG", nonexistent.to_str().unwrap());
        let cfg = Config::load().expect("load should succeed (no file, defaults used)");
        let home = std::env::var("HOME").expect("HOME should exist");
        assert!(
            !cfg.db_path.starts_with("~"),
            "db_path must not keep a literal ~: {:?}",
            cfg.db_path
        );
        assert!(
            cfg.db_path.starts_with(&home),
            "db_path should start with $HOME: {:?} vs {}",
            cfg.db_path,
            home
        );
        assert!(
            !cfg.registry_path.starts_with("~") || cfg.registry_path.starts_with(&home),
            "registry_path must not keep a literal ~: {:?}",
            cfg.registry_path
        );
        let _ = std::fs::remove_file(&nonexistent);
    }

    // issue #145: systemd_units validation — a valid unit passes, shell metacharacters fail closed
    #[test]
    fn test_issue145_systemd_units_validation() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let _g =
            crate::env_map::tests::EnvGuard::set("FUSION_SV_CONFIG", tmp.path().to_str().unwrap());
        std::fs::write(
            tmp.path(),
            "[systemd_units]\n\"fusion-identity\" = \"fusion-identity.service\"\n",
        )
        .unwrap();
        let cfg = Config::load().unwrap();
        assert_eq!(
            cfg.systemd_units.get("fusion-identity").map(String::as_str),
            Some("fusion-identity.service")
        );
        // shell metacharacter injection rejected
        std::fs::write(tmp.path(), "[systemd_units]\n\"svc\" = \"evil; reboot\"\n").unwrap();
        assert!(Config::load().is_err());
    }

    // issue #136: config path resolution pure function — explicit always wins.
    #[test]
    fn test_issue136_resolve_path_explicit_wins() {
        let (path, adopted) = resolve_config_path(Some("/custom/sv.toml".into()), true, true);
        assert_eq!(path, PathBuf::from("/custom/sv.toml"));
        assert!(!adopted);
    }

    // issue #136: default config exists → use the default, do not adopt the deploy config.
    #[test]
    fn test_issue136_resolve_path_default_present() {
        let (path, adopted) = resolve_config_path(None, true, true);
        assert_eq!(path, PathBuf::from("~/.fusion-sv/config.toml"));
        assert!(!adopted);
    }

    // issue #136: default missing + deploy config exists (VPS root running CLI) → adopt the deploy config.
    #[test]
    fn test_issue136_resolve_path_adopts_deploy() {
        let (path, adopted) = resolve_config_path(None, false, true);
        assert_eq!(path, PathBuf::from(DEPLOY_CONFIG_PATH));
        assert!(adopted);
    }

    // issue #136: both missing → fall back to the default path (the missing branch is WARNed by the caller, #134).
    #[test]
    fn test_issue136_resolve_path_neither_exists() {
        let (path, adopted) = resolve_config_path(None, false, false);
        assert_eq!(path, PathBuf::from("~/.fusion-sv/config.toml"));
        assert!(!adopted);
    }

    // A partial config file (only overrides) should load, remaining fields take defaults —
    // the docs promise "all fields have defaults"; fields without serde defaults used to fail parsing outright.
    #[test]
    fn test_partial_config_file_uses_defaults() {
        let path = std::env::temp_dir().join("fusion-sv-config-partial-test.toml");
        std::fs::write(&path, "auto_up_on_start = true\n").unwrap();
        let _guard = EnvGuard::set("FUSION_SV_CONFIG", path.to_str().unwrap());
        // issue #31: env is globally shared, config tests must also share the lock with other env tests to serialize
        let _g0 = crate::env_map::tests::env_lock();
        // Pin FUSION_ROOT to a temp dir without .env, asserting independence from the host environment
        let empty_root = std::env::temp_dir().join("fusion-sv-config-test-root");
        std::fs::create_dir_all(&empty_root).unwrap();
        let _guard2 = EnvGuard::set("FUSION_ROOT", empty_root.to_str().unwrap());
        let cfg = Config::load().expect("partial config should load");
        assert!(cfg.auto_up_on_start, "override should take effect");
        assert_eq!(cfg.socket_path, "/tmp/fusion-sv.sock");
        assert_eq!(cfg.poll_healthy_sec, 15);
        assert_eq!(cfg.max_concurrent_start, 4);
        assert!(!cfg.compose.enabled);
        // Regression: without a [compose] section, env_file should be ".env", not derive(Default)'s empty string
        assert_eq!(cfg.compose.env_file, PathBuf::from(".env"));
        assert_eq!(
            cfg.compose.sidecar_file,
            PathBuf::from("docker-compose.sidecar.yml")
        );
        let _ = std::fs::remove_file(&path);
    }

    // issue #31: relative env_file resolves preferentially under FUSION_ROOT (when present), else falls back to CWD
    #[test]
    fn test_resolve_env_file_fusion_root_priority() {
        // issue #31: as above, env lock serializes
        let _g = crate::env_map::tests::env_lock();
        let root = std::env::temp_dir().join("fusion-sv-envroot-test");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".env"), "K=V\n").unwrap();
        let _guard = EnvGuard::set("FUSION_ROOT", root.to_str().unwrap());
        assert_eq!(
            resolve_env_file(Path::new(".env")),
            root.join(".env"),
            "resolves to the root when .env exists under FUSION_ROOT"
        );
        assert_eq!(
            resolve_env_file(Path::new("other.env")),
            PathBuf::from("other.env"),
            "falls back to the CWD-relative path when absent under FUSION_ROOT"
        );
        let abs = root.join("abs.env");
        assert_eq!(
            resolve_env_file(&abs),
            abs,
            "absolute paths are returned as-is"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // issue #80: missing entries appended to the tail (append-only, does not change existing resolution order)
    #[test]
    fn test_append_path_entries_appends_missing() {
        assert_eq!(
            append_path_entries("/usr/bin:/bin", "/opt/homebrew/bin:/usr/local/bin"),
            "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin"
        );
    }

    // issue #80: existing entries skipped — idempotent when the launchd environment already has Homebrew bin
    #[test]
    fn test_append_path_entries_dedups_existing() {
        assert_eq!(
            append_path_entries(
                "/opt/homebrew/bin:/usr/bin:/bin",
                "/opt/homebrew/bin:/usr/local/bin"
            ),
            "/opt/homebrew/bin:/usr/bin:/bin:/usr/local/bin"
        );
        // all already present → unchanged
        assert_eq!(append_path_entries("/a:/b", "/a:/b"), "/a:/b");
    }

    // issue #80: extra empty string (disabled) / empty / whitespace entries → cur as-is (only empty entries removed)
    #[test]
    fn test_append_path_entries_empty_and_trim() {
        assert_eq!(append_path_entries("/usr/bin:/bin", ""), "/usr/bin:/bin");
        assert_eq!(
            append_path_entries("/usr/bin:::/bin", "  "),
            "/usr/bin:/bin"
        );
        // whitespace entries are trimmed then appended
        assert_eq!(
            append_path_entries("/usr/bin", "  /opt/homebrew/bin  "),
            "/usr/bin:/opt/homebrew/bin"
        );
    }

    // issue #80: cur empty (PATH unset) → only extra's valid entries remain
    #[test]
    fn test_append_path_entries_empty_cur() {
        assert_eq!(
            append_path_entries("", "/opt/homebrew/bin:/usr/local/bin"),
            "/opt/homebrew/bin:/usr/local/bin"
        );
        assert_eq!(append_path_entries("", ""), "");
    }
}
