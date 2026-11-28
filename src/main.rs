use clap::{Parser, Subcommand};
use fusion_supervisor::cli;
use fusion_supervisor::config::{self, Config};
use fusion_supervisor::manifest;
#[cfg(unix)]
use fusion_supervisor::rpc;
use fusion_supervisor::rpc::client::RpcClient;
use fusion_supervisor::rpc::schema::RpcRequest;
use fusion_supervisor::supervisor;
use fusion_supervisor::supervisor::SupervisorCore;
use serde_json::Value;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "fusion-sv", version, about = "Fusion service supervisor")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Daemon,
    Up {
        service: Option<String>,
    },
    Down {
        service: Option<String>,
    },
    Status {
        /// Optional service name filter (issue #96: `status <svc>` used to be rejected)
        service: Option<String>,
    },
    Restart {
        service: String,
    },
    Logs {
        service: String,
    },
    Top,
    /// Ping the daemon (alive check)
    Ping,
    /// Drain traffic from a service (or all if no service given) — issue #9
    Drain {
        service: Option<String>,
    },
    /// Zero-downtime rollout a service: drain→stop→start→prewarm — issue #9
    Rollout {
        service: String,
    },
    /// Run a one-shot backup (Postgres + Qdrant → FUSION_BACKUP_TARGET) — issue #10
    Backup,
    /// Gracefully shut down the daemon (remote shutdown via RPC) — issue #20
    Shutdown,
    /// Run the login-latency tripwire loop standalone (issue #12): same probes, alerts and
    /// history writes as the daemon-spawned loop, without the supervision tick or the daemon
    /// instance lock. For drills and launchd/cron-hosted operation.
    Tripwire,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // issue #31: default to info when RUST_LOG is unset — operators must see
    // startup warnings (e.g. the FUSION_ROOT hint); default error would swallow them.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let cli = Cli::parse();
    let cfg = Config::load()?;
    match cli.command {
        None | Some(Commands::Daemon) => run_daemon(cfg).await,
        Some(Commands::Status { service }) => cli_status(cfg, service).await,
        Some(Commands::Top) => cli_top(cfg).await,
        // issue #41: the [SERVICE] arg must route to the single-service RPC (up_one/down_one);
        // `{ .. }` used to drop the arg making `down <svc>` stop the whole fleet. Routed via
        // cli::up_down_target (testable).
        Some(Commands::Up { service }) => match cli::up_down_target(service) {
            Some(s) => cli_call_param(cfg, "up", s).await,
            None => cli_call(cfg, "up").await,
        },
        Some(Commands::Down { service }) => match cli::up_down_target(service) {
            Some(s) => cli_call_param(cfg, "down", s).await,
            None => cli_call(cfg, "down").await,
        },
        Some(Commands::Restart { service }) => cli_call_param(cfg, "restart", service).await,
        Some(Commands::Logs { service }) => cli_logs(cfg, service).await,
        Some(Commands::Ping) => cli_ping(cfg).await,
        Some(Commands::Drain { service }) => match service {
            Some(s) => cli_call_param(cfg, "drain", s).await,
            None => cli_call(cfg, "drain").await,
        },
        Some(Commands::Rollout { service }) => cli_call_param(cfg, "rollout", service).await,
        Some(Commands::Backup) => cli_call(cfg, "backup").await,
        Some(Commands::Shutdown) => cli_call(cfg, "shutdown").await,
        Some(Commands::Tripwire) => run_tripwire(cfg).await,
    }
}

// issue #12: standalone tripwire loop — drills and hosts where the tripwire runs without the
// supervision daemon. Never returns (Ctrl-C exits); each sample is logged and persisted.
async fn run_tripwire(cfg: Config) -> anyhow::Result<()> {
    if !cfg.tripwire_enabled {
        anyhow::bail!(
            "tripwire_enabled = false in config; enable it or point FUSION_SV_CONFIG at a config that does"
        );
    }
    let store = Arc::new(fusion_supervisor::store::HistoryStore::open(&cfg.db_path)?);
    tracing::info!(db = %cfg.db_path.display(), "standalone tripwire starting");
    fusion_supervisor::tripwire::run_tripwire_loop(
        fusion_supervisor::tripwire::TripwireConfig::from_config(&cfg),
        store,
        cfg.alert_url.clone(),
        cfg.alert_dedup_sec,
    )
    .await;
    Ok(())
}

// issue #114: single-instance guard — the file lock is held until process exit (flock,
// released by the kernel on exit, no residue on crash). Lock file: ~/.fusion-sv/daemon.lock.
// Two daemons running = duplicated ticks/backups/alerts (2026-11-09 incident: an orphan
// daemon overwrote the day's backup).
fn acquire_daemon_lock() -> Option<std::fs::File> {
    // FUSION_SV_LOCK_PATH overrides the lock path (test isolation, never touches the real daemon.lock)
    let path = match std::env::var("FUSION_SV_LOCK_PATH") {
        Ok(v) if !v.is_empty() => std::path::PathBuf::from(v),
        _ => {
            let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).ok()?;
            let dir = std::path::Path::new(&home).join(".fusion-sv");
            if !dir.exists() {
                std::fs::create_dir_all(&dir).ok();
            }
            dir.join("daemon.lock")
        }
    };
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .open(&path)
        .map_err(|e| {
            tracing::error!(path = %path.display(), err = %e, "daemon lock file create fail");
            e
        })
        .ok()?;
    match file.try_lock() {
        Ok(()) => {
            tracing::info!(path = %path.display(), "daemon instance lock acquired");
            Some(file)
        }
        Err(e) => {
            tracing::error!(path = %path.display(), err = %e, "another fusion-sv daemon already holds the instance lock");
            None
        }
    }
}

async fn run_daemon(cfg: Config) -> anyhow::Result<()> {
    tracing::info!("fusion-sv daemon starting");
    // issue #114: guard runs before everything — exit immediately when the lock is
    // unavailable (launchd KeepAlive will retry, but retries keep failing while the
    // holder lives; silent double-running can no longer happen).
    let _daemon_lock = match acquire_daemon_lock() {
        Some(f) => f,
        None => anyhow::bail!("fusion-sv daemon already running (instance lock held)"),
    };
    // issue #145: feed the systemd-managed table declared in config into start_map (OnceLock, once;
    // CLI one-shot does not register — all lifecycle commands go through daemon RPC).
    fusion_supervisor::start_map::register_systemd_units(cfg.systemd_units.clone());
    if !cfg.systemd_units.is_empty() {
        tracing::info!(
            units = cfg.systemd_units.len(),
            "systemd-managed services registered (start/stop/restart via systemctl)"
        );
    }
    // issue #80: launchd's default PATH lacks Homebrew bin — node/brew-based start.sh
    // scripts all fail `command -v node` when spawned by the daemon. At startup, append
    // missing entries from native_extra_path to the daemon's own PATH; all child processes
    // (exec_bash_script / exec_start_spec) inherit daemon env, so a single injection point
    // covers every exec path. Empty string disables (keeps launchd's original PATH).
    if !cfg.native_extra_path.trim().is_empty() {
        let cur = std::env::var("PATH").unwrap_or_default();
        let merged = config::append_path_entries(&cur, &cfg.native_extra_path);
        if merged != cur {
            tracing::info!(
                extra = %cfg.native_extra_path,
                "daemon PATH extended with Homebrew dirs (inherited by children)"
            );
            // SAFETY: run_daemon entry point; no thread concurrently reading PATH has been spawned yet.
            unsafe { std::env::set_var("PATH", merged) };
        }
    }
    // issue #31: warn once when FUSION_ROOT is unset and cwd does not look like the monorepo root.
    // start.sh path resolution prefers env over cwd; with a wrong cwd up_native fails closed across the fleet.
    if std::env::var("FUSION_ROOT").is_err() {
        let cwd = std::env::current_dir().unwrap_or_default();
        if !cwd.join("architecture/port-registry.yaml").exists() {
            tracing::warn!(
                cwd = %cwd.display(),
                "FUSION_ROOT is unset and the current directory is not the fusion monorepo root — default start.sh paths will fail to resolve. Start with FUSION_ROOT=<monorepo root> fusion-sv daemon"
            );
        }
    }
    // issue #102: absent registry degrades to an empty managed set (one WARN
    // inside load_or_empty) — never blocks boot on the minimal-download profile.
    // issue #114: bind surfaces first — UDS/HTTP bind failure is fatal (exit 1), no more
    // faceless idle ticking (the old path bound inside the serve task and only logged the
    // failure, leaving a zombie daemon running alongside the lock-holding one).
    // UDS face is unix-only — windows has no UnixStream; the daemon degrades to a pure HTTP API face (CLI uses HTTP).
    #[cfg(unix)]
    let uds_listener = rpc::server::bind_uds(&cfg.socket_path)
        .map_err(|e| anyhow::anyhow!("uds bind fail (fatal) {}: {e}", cfg.socket_path))?;
    #[cfg(not(unix))]
    let uds_listener = {
        tracing::warn!(
            "uds face unavailable on this platform; CLI subcommands must use the HTTP API"
        );
        ()
    };
    let tcp_listener = match &cfg.metrics_addr {
        Some(addr) => Some(
            fusion_supervisor::http_server::bind_tcp(addr)
                .await
                .map_err(|e| anyhow::anyhow!("http bind fail (fatal) {addr}: {e}"))?,
        ),
        None => None,
    };
    let defs = manifest::load_or_empty(&cfg.registry_path);
    let core = supervisor::SupervisorCore::new(defs, cfg.clone())?;
    // issue #12: clone the history store before the core moves under the daemon Mutex —
    // the tripwire loop shares it (HistoryStore: Sync) without ever taking the core lock.
    let tripwire_store = core.store.clone();
    let mut core = core;
    // issue #107 D2: reconcile before opening RPC — surviving processes (after launchd
    // crash-restart) are no longer misreported as Stopped by status. One probe round is
    // enough; the block is far shorter than the first tick's wait window.
    let adopted = core.reconcile_on_boot().await;
    tracing::info!(adopted, "boot reconcile complete");
    let core = Arc::new(tokio::sync::Mutex::new(core));
    let token = std::env::var("FUSION_SV_TOKEN").ok();
    let core_rpc = core.clone();
    // issue #20: shutdown signal channel (RPC Shutdown → run_daemon select exit).
    // issue #25: shared Arc — the same signal for both the UDS and HTTP faces.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown_shared = Arc::new(tokio::sync::Mutex::new(Some(shutdown_tx)));
    let shutdown_for_rpc = shutdown_shared.clone();
    #[cfg(unix)]
    tokio::spawn(async move {
        // issue #114: listener already bound at entry (failure was fatal); serve only accepts.
        if let Err(e) =
            rpc::server::serve_with_shutdown_on(core_rpc, uds_listener, token, shutdown_for_rpc)
                .await
        {
            tracing::error!(err = %e, "rpc serve exit");
        }
    });
    #[cfg(not(unix))]
    {
        let _ = (core_rpc, uds_listener, shutdown_for_rpc, token);
    }
    tracing::info!("daemon ready (socket {})", cfg.socket_path);
    // Supervision tick loop: periodic probing + automatic crash restart (C1 fix)
    // Lock discipline: lock to read poll_interval → unlock → sleep → re-lock for tick_all.
    // Never hold the lock across a sleep (it would block RPC status/up/down).
    let core_tick = core.clone();
    // issue #168: log rotation params snapshot (read-only in the tick; changes only via config reload)
    let rotate_paths = cfg.log_rotate_paths.clone();
    let rotate_max_mb = cfg.log_rotate_max_mb;
    let rotate_keep = cfg.log_rotate_keep;
    let tick_handle = tokio::spawn(async move {
        // issue #107 D1: sleep/suspend diagnostics — a gap between two ticks far exceeding
        // poll_interval is treated as a system wake (if the daemon died during sleep, the
        // absence of this log corroborates a hard kill like SIGKILL).
        let mut last_tick_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        loop {
            let interval = {
                let c = core_tick.lock().await;
                c.poll_interval()
            };
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let gap = now.saturating_sub(last_tick_ts);
            if gap > interval * 6 + 60 {
                tracing::warn!(
                    gap_sec = gap,
                    expected_sec = interval,
                    "time jump detected — system sleep/suspend suspected"
                );
            }
            last_tick_ts = now;
            // Per-round tick panic isolation: a panic kills the whole tick task — RPC/HTTP
            // stay alive but supervision silently stalls (probing/self-healing/alerting all
            // stop), unacceptable for a resident component. Inner spawn isolates it (JoinError
            // captures the panic); on crash, log ERROR and continue next tick.
            // Lock discipline unchanged: the tick body takes the lock, the outer sleep is lock-free.
            let core_inner = core_tick.clone();
            // issue #168: rotation params cloned into the 'static tick task (Vec is not Copy;
            // cloning the two-element strings once per tick is negligible)
            let rotate_paths = rotate_paths.clone();
            let tick_round = tokio::spawn(async move {
                // issue #122 phase-2: due restart execs scheduled by the tick run concurrently outside
                // the core lock — start.sh (go build etc.) can take minutes; holding the lock would
                // block RPC/HTTP status.
                let execs = {
                    let mut c = core_inner.lock().await;
                    c.tick_all(now).await;
                    // issue #10/#113: backup schedule check (due → spawn_backup, fail-closed already alerted)
                    c.check_backup_schedule(now).await;
                    // issue #37: retention sweep schedule check (due → prune expired history.db rows)
                    c.check_retention(now);
                    // issue #137: scheduled jobs (due → detached exec, cursor persisted)
                    c.dispatch_due_jobs(now);
                    c.collect_pending_execs()
                };
                if !execs.is_empty() {
                    let results = SupervisorCore::run_pending_execs(execs).await;
                    let mut c = core_inner.lock().await;
                    c.finish_pending_execs(now, results).await;
                }
                // issue #168: log size rotation check (outside the lock — pays the copy cost only at
                // the threshold, otherwise just a metadata stat; check_paths logs a WARN per failing
                // file internally and continues)
                fusion_supervisor::log_rotate::check_paths(
                    &rotate_paths,
                    rotate_max_mb,
                    rotate_keep,
                );
            });
            if let Err(e) = tick_round.await {
                tracing::error!(err = %e, "tick round panicked (self-healing, continuing)");
            }
        }
    });
    // issue #12: login-latency tripwire — resident loop detached from the core lock (same
    // detached pattern as the backup task): probes the gateway/identity front doors only during
    // CI-runner activity windows, alerts through the standard alert channel, samples into
    // history.db tripwire_sample. Own Alerter instance; event persistence via the shared sink.
    let tripwire_handle = if cfg.tripwire_enabled {
        Some(tokio::spawn(
            fusion_supervisor::tripwire::run_tripwire_loop(
                fusion_supervisor::tripwire::TripwireConfig::from_config(&cfg),
                tripwire_store,
                cfg.alert_url.clone(),
                cfg.alert_dedup_sec,
            ),
        ))
    } else {
        None
    };
    // issue #22/#26/#25: HTTP face (metrics + read-only status/history/events + web static
    // console + token-checked orchestration ops), third spawn task, shared via core.clone().
    // issue #114: listener already bound at entry (failure was fatal); serve only accepts.
    let http_handle = if let Some(listener) = tcp_listener {
        let core_http = core.clone();
        let token_http = std::env::var("FUSION_SV_TOKEN").ok();
        let web_dir = cfg.web_dir.clone();
        let shutdown_for_http = shutdown_shared.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = fusion_supervisor::http_server::serve_on(
                core_http,
                listener,
                token_http,
                web_dir,
                shutdown_for_http,
            )
            .await
            {
                tracing::error!(err = %e, "http server exit");
            }
        }))
    } else {
        None
    };
    // issue #29: auto-start at daemon boot (probe skips already-healthy services, no double start).
    // PRD app-tiering (issue #62) narrowed semantics: auto_up only starts T0 ∪ pin=auto,
    // no longer a full up_all (a 40+ service boot storm exhausted memory). up_all is reserved
    // for explicit Web/CLI operations. Holding the lock across up_boot_set matches RPC up
    // behavior; default false keeps manual dev-machine behavior unchanged.
    if cfg.auto_up_on_start {
        let core_boot = core.clone();
        tokio::spawn(async move {
            tracing::info!("auto_up_on_start: up_boot_set at boot (T0 ∪ pin=auto, layer order)");
            let mut c = core_boot.lock().await;
            if let Err(e) = c.up_boot_set().await {
                tracing::error!(err = %e, "auto_up_on_start up_boot_set partially failed (tick backstop)");
            }
        });
    }
    // issue #20: graceful exit on either ctrl-c or the RPC Shutdown signal.
    // issue #107 D1: SIGTERM (launchd bootout) also exits gracefully with a log trace — if a
    // silent death during sleep was not signal-caused, the missing shutdown line rules out
    // normal termination paths.
    // SIGTERM face is unix-only — on windows this arm degrades to a never-ready pending
    // (select then only watches ctrl-c + rpc).
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| anyhow::anyhow!("SIGTERM handler install fail: {e}"))?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("daemon shutdown (ctrl-c)");
        }
        _ = async {
            #[cfg(unix)]
            { sigterm.recv().await }
            #[cfg(not(unix))]
            { std::future::pending::<()>().await }
        } => {
            tracing::info!("daemon shutdown (SIGTERM)");
        }
        _ = &mut shutdown_rx => {
            tracing::info!("daemon shutdown (rpc)");
        }
    }
    tick_handle.abort();
    if let Some(h) = tripwire_handle {
        h.abort();
    }
    if let Some(h) = http_handle {
        h.abort();
    }
    Ok(())
}

async fn cli_status(cfg: Config, service: Option<String>) -> anyhow::Result<()> {
    let mut client = connect_client(&cfg).await?;
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        method: "status".into(),
        params: Value::Null,
        id: 1,
    };
    let resp = client.call(req).await?;
    if let Some(err) = resp.error {
        cli::print_error(&err.message);
        std::process::exit(1);
    }
    let mut entries: Vec<supervisor::StatusEntry> =
        serde_json::from_value(resp.result.unwrap_or(Value::Array(vec![]))).unwrap_or_default();
    // issue #96: `status <svc>` client-side filtering (the RPC remains the full status face)
    if let Some(f) = &service {
        entries.retain(|e| &e.name == f);
        if entries.is_empty() {
            cli::print_error(&format!("no matching service: {f}"));
            std::process::exit(1);
        }
    }
    cli::print_status(&entries);
    Ok(())
}

async fn cli_top(cfg: Config) -> anyhow::Result<()> {
    loop {
        // M6: clear the screen before printing, avoiding a 1s stale first frame
        print!("\x1b[2J\x1b[H");
        if let Err(e) = cli_status(cfg.clone(), None).await {
            cli::print_error(&e.to_string());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

// issue #82: CLI requests carry an origin — stop/stop-all operations on the daemon side are attributable to the originating process
fn cli_origin() -> String {
    let argv = std::env::args().collect::<Vec<_>>().join(" ");
    format!("cli pid={} argv: {}", std::process::id(), argv)
}

async fn cli_call(cfg: Config, method: &str) -> anyhow::Result<()> {
    let mut client = connect_client(&cfg).await?;
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        method: method.into(),
        params: serde_json::json!({"origin": cli_origin()}),
        id: 1,
    };
    let resp = client.call(req).await?;
    if let Some(err) = resp.error {
        cli::print_error(&err.message);
        std::process::exit(1);
    }
    cli::print_ok(
        &resp
            .result
            .map(|v| v.to_string())
            .unwrap_or_else(|| "ok".into()),
    );
    Ok(())
}

async fn cli_call_param(cfg: Config, method: &str, service: String) -> anyhow::Result<()> {
    let mut client = connect_client(&cfg).await?;
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        method: method.into(),
        params: serde_json::json!({"service": service, "origin": cli_origin()}),
        id: 1,
    };
    let resp = client.call(req).await?;
    if let Some(err) = resp.error {
        cli::print_error(&err.message);
        std::process::exit(1);
    }
    cli::print_ok(
        &resp
            .result
            .map(|v| v.to_string())
            .unwrap_or_else(|| "ok".into()),
    );
    Ok(())
}

async fn connect_client(cfg: &Config) -> anyhow::Result<RpcClient> {
    match RpcClient::connect(&cfg.socket_path).await {
        Ok(c) => Ok(c),
        Err(e) => {
            cli::print_error(&format!("{e}"));
            std::process::exit(3);
        }
    }
}

async fn cli_logs(cfg: Config, service: String) -> anyhow::Result<()> {
    let mut client = connect_client(&cfg).await?;
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        method: "logs".into(),
        params: serde_json::json!({"service": service}),
        id: 1,
    };
    let resp = client.call(req).await?;
    if let Some(err) = resp.error {
        cli::print_error(&err.message);
        std::process::exit(1);
    }
    let obj = resp.result.unwrap_or(serde_json::Value::Null);
    let lines = obj.get("lines").and_then(|v| v.as_str()).unwrap_or("");
    let plane = obj
        .get("plane")
        .and_then(|v| v.as_str())
        .unwrap_or("native");
    tracing::info!(svc = %service, plane, "tail logs");
    print!("{lines}");
    Ok(())
}

async fn cli_ping(cfg: Config) -> anyhow::Result<()> {
    let mut client = connect_client(&cfg).await?;
    match client.ping().await {
        Ok(ok) => {
            cli::print_pong(ok);
            Ok(())
        }
        Err(e) => {
            cli::print_error(&format!("{e}"));
            std::process::exit(1);
        }
    }
}

// issue #114: single-instance guard testability — FUSION_SV_LOCK_PATH points at a temp file,
// never touching the real ~/.fusion-sv/daemon.lock (a production daemon may be holding it).
#[cfg(test)]
mod daemon_lock_tests {
    use super::*;

    #[test]
    fn test_daemon_lock_exclusive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let lock_path = tmp.path().join("test-daemon.lock");
        // SAFETY: the test process uses this env single-threaded, and this mod is the only env test in main.rs
        unsafe { std::env::set_var("FUSION_SV_LOCK_PATH", &lock_path) };
        let f1 = acquire_daemon_lock().expect("first acquire should succeed");
        assert!(
            acquire_daemon_lock().is_none(),
            "second acquire must fail (lock held)"
        );
        drop(f1);
        // flock is released with the fd → can be acquired again
        assert!(
            acquire_daemon_lock().is_some(),
            "release then reacquire should succeed"
        );
        unsafe { std::env::remove_var("FUSION_SV_LOCK_PATH") };
    }
}
