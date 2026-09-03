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
    /// Gracefully shut down the daemon (remote shutdown via RPC) — issue #20
    Shutdown,
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
        Some(Commands::Shutdown) => cli_call(cfg, "shutdown").await,
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
    // issue #20: shutdown 信号通道 (RPC Shutdown → run_daemon select 退出)。
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        if let Err(e) =
            rpc::server::serve_with_shutdown(core_rpc, &socket_path, token, Some(shutdown_tx)).await
        {
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
    // issue #22: Prometheus /metrics HTTP 端点 (可选)。
    // 第三个 spawn task, 共享 core.clone()。hand-rolled HTTP (无新依赖)。
    // 锁纪律: handler lock().await → metrics::render(&c) → 立即释放, 短持锁同 RPC。
    let metrics_handle = if let Some(addr) = &cfg.metrics_addr {
        let core_metrics = core.clone();
        let addr = addr.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = serve_metrics(core_metrics, &addr).await {
                tracing::error!(err = %e, "metrics server exit");
            }
        }))
    } else {
        None
    };
    // issue #20: ctrl-c 或 RPC Shutdown 信号任一触发优雅退出。
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("daemon shutdown (ctrl-c)");
        }
        _ = &mut shutdown_rx => {
            tracing::info!("daemon shutdown (rpc)");
        }
    }
    tick_handle.abort();
    if let Some(h) = metrics_handle {
        h.abort();
    }
    Ok(())
}

// issue #22: hand-rolled Prometheus /metrics HTTP server (无 hyper/axum 依赖)。
// TcpListener accept 循环: 每连接读请求行, 仅响应 GET /metrics (200 text/plain),
// 其他路径 404。锁纪律: 短持锁 (lock → render → release), 绝不持锁跨 await。
async fn serve_metrics(
    core: Arc<tokio::sync::Mutex<supervisor::SupervisorCore>>,
    addr: &str,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr, "metrics server listening");
    loop {
        match listener.accept().await {
            Ok((mut stream, peer)) => {
                let core = core.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_metrics_conn(&mut stream, &core).await {
                        tracing::debug!(peer = %peer, err = %e, "metrics conn error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(err = %e, "metrics accept error");
            }
        }
    }
}

async fn handle_metrics_conn(
    stream: &mut tokio::net::TcpStream,
    core: &Arc<tokio::sync::Mutex<supervisor::SupervisorCore>>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // 读请求 (最多 4KB, 足够 HTTP 请求行 + 头)。
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    // 仅响应 GET /metrics, 其他 → 404。
    let is_metrics =
        first_line.starts_with("GET /metrics") || first_line.starts_with("GET /metrics?");
    if !is_metrics {
        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        stream.write_all(resp.as_bytes()).await?;
        return Ok(());
    }
    // 短持锁: lock → render → 释放 (render 纯函数, 无 await)。
    let body = {
        let c = core.lock().await;
        fusion_supervisor::metrics::render(&c)
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).await?;
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
