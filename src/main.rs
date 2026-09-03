use clap::{Parser, Subcommand};
use fusion_supervisor::cli;
use fusion_supervisor::config::Config;
use fusion_supervisor::manifest;
use fusion_supervisor::rpc;
use fusion_supervisor::rpc::client::RpcClient;
use fusion_supervisor::rpc::schema::RpcRequest;
use fusion_supervisor::supervisor;
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
    Status,
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let cfg = Config::load()?;
    match cli.command {
        None | Some(Commands::Daemon) => run_daemon(cfg).await,
        Some(Commands::Status) => cli_status(cfg).await,
        Some(Commands::Top) => cli_top(cfg).await,
        Some(Commands::Up { .. }) => cli_call(cfg, "up").await,
        Some(Commands::Down { .. }) => cli_call(cfg, "down").await,
        Some(Commands::Restart { service }) => cli_call_param(cfg, "restart", service).await,
        Some(Commands::Logs { service }) => cli_logs(cfg, service).await,
        Some(Commands::Ping) => cli_ping(cfg).await,
        Some(Commands::Drain { service }) => match service {
            Some(s) => cli_call_param(cfg, "drain", s).await,
            None => cli_call(cfg, "drain").await,
        },
        Some(Commands::Rollout { service }) => cli_call_param(cfg, "rollout", service).await,
        Some(Commands::Backup) => cli_call(cfg, "backup").await,
    }
}

async fn run_daemon(cfg: Config) -> anyhow::Result<()> {
    tracing::info!("fusion-sv daemon starting");
    let defs = manifest::ServiceManifest::load(&cfg.registry_path)?;
    tracing::info!(services = defs.len(), "registry loaded");
    let core = supervisor::SupervisorCore::new(defs, cfg.clone())?;
    let core = Arc::new(tokio::sync::Mutex::new(core));
    let token = std::env::var("FUSION_SV_TOKEN").ok();
    let core_rpc = core.clone();
    let socket_path = cfg.socket_path.clone();
    tokio::spawn(async move {
        if let Err(e) = rpc::server::serve(core_rpc, &socket_path, token).await {
            tracing::error!(err = %e, "rpc serve exit");
        }
    });
    tracing::info!("daemon ready (socket {})", cfg.socket_path);
    // 监督 tick 循环: 周期探活 + 宕机自动重启 (C1 修复)
    // 锁纪律: 取锁读 poll_interval → 释放 → sleep → 再取锁 tick_all。
    // 绝不持锁跨 sleep (否则阻塞 RPC status/up/down)。
    let core_tick = core.clone();
    let tick_handle = tokio::spawn(async move {
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
            let mut c = core_tick.lock().await;
            c.tick_all(now).await;
            // issue #10: 备份调度检查 (到期触发 run_backup, fail-closed 已告警)
            c.check_backup_schedule(now).await;
        }
    });
    tokio::signal::ctrl_c().await?;
    tracing::info!("daemon shutdown (ctrl-c)");
    tick_handle.abort();
    Ok(())
}

async fn cli_status(cfg: Config) -> anyhow::Result<()> {
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
    let entries: Vec<supervisor::StatusEntry> =
        serde_json::from_value(resp.result.unwrap_or(Value::Array(vec![]))).unwrap_or_default();
    cli::print_status(&entries);
    Ok(())
}

async fn cli_top(cfg: Config) -> anyhow::Result<()> {
    loop {
        // M6: 先清屏再打印, 避免首帧滞留 1s 再空白
        print!("\x1b[2J\x1b[H");
        if let Err(e) = cli_status(cfg.clone()).await {
            cli::print_error(&e.to_string());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn cli_call(cfg: Config, method: &str) -> anyhow::Result<()> {
    let mut client = connect_client(&cfg).await?;
    let req = RpcRequest {
        jsonrpc: "2.0".into(),
        method: method.into(),
        params: Value::Null,
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
        params: serde_json::json!({"service": service}),
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
