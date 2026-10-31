use clap::{Parser, Subcommand};
use fusion_supervisor::cli;
use fusion_supervisor::config::{self, Config};
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
    // issue #31: RUST_LOG 未设时默认 info — 操作员须能看到启动告警
    // (如 FUSION_ROOT 未设提示), 默认 error 会静默吞掉。
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let cli = Cli::parse();
    let cfg = Config::load()?;
    match cli.command {
        None | Some(Commands::Daemon) => run_daemon(cfg).await,
        Some(Commands::Status) => cli_status(cfg).await,
        Some(Commands::Top) => cli_top(cfg).await,
        // issue #41: [SERVICE] 参数必须路由到单服务 RPC (up_one/down_one),
        // 此前 `{ .. }` 丢弃参数致 `down <svc>` 全停。经 cli::up_down_target 路由 (可测)。
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
    }
}

async fn run_daemon(cfg: Config) -> anyhow::Result<()> {
    tracing::info!("fusion-sv daemon starting");
    // issue #80: launchd 默认 PATH 不含 Homebrew bin — node/brew 系 start.sh 在
    // daemon 拉起时 `command -v node` 全挂。启动时把 native_extra_path 中缺失的
    // 项追加到 daemon 自身 PATH 尾部; 所有子进程 (exec_bash_script /
    // exec_start_spec) 继承 daemon env, 单一注入点覆盖全部 exec 路径。
    // 空串禁用 (保持 launchd 原始 PATH 行为)。
    if !cfg.native_extra_path.trim().is_empty() {
        let cur = std::env::var("PATH").unwrap_or_default();
        let merged = config::append_path_entries(&cur, &cfg.native_extra_path);
        if merged != cur {
            tracing::info!(
                extra = %cfg.native_extra_path,
                "daemon PATH 追加 Homebrew 目录 (子进程继承)"
            );
            // SAFETY: run_daemon 入口, 尚未 spawn 任何并发读 PATH 的线程。
            unsafe { std::env::set_var("PATH", merged) };
        }
    }
    // issue #31: FUSION_ROOT 未设且 cwd 不像 monorepo 根时一次告警。
    // start.sh 路径解析 env > cwd, cwd 错误时 up_native 会全量 fail-closed。
    if std::env::var("FUSION_ROOT").is_err() {
        let cwd = std::env::current_dir().unwrap_or_default();
        if !cwd.join("architecture/port-registry.yaml").exists() {
            tracing::warn!(
                cwd = %cwd.display(),
                "FUSION_ROOT 未设置且当前目录不是 fusion monorepo 根 — 默认 start.sh 路径将解析失败。请 FUSION_ROOT=<monorepo 根> fusion-sv daemon 启动"
            );
        }
    }
    let defs = manifest::ServiceManifest::load(&cfg.registry_path)?;
    tracing::info!(services = defs.len(), "registry loaded");
    let core = supervisor::SupervisorCore::new(defs, cfg.clone())?;
    let core = Arc::new(tokio::sync::Mutex::new(core));
    let token = std::env::var("FUSION_SV_TOKEN").ok();
    let core_rpc = core.clone();
    let socket_path = cfg.socket_path.clone();
    // issue #20: shutdown 信号通道 (RPC Shutdown → run_daemon select 退出)。
    // issue #25: 共享 Arc — UDS 与 HTTP 两个面同一信号。
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown_shared = Arc::new(tokio::sync::Mutex::new(Some(shutdown_tx)));
    let shutdown_for_rpc = shutdown_shared.clone();
    tokio::spawn(async move {
        if let Err(e) =
            rpc::server::serve_with_shutdown(core_rpc, &socket_path, token, shutdown_for_rpc).await
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
            // tick 单轮 panic 隔离: panic 会杀掉整个 tick task — RPC/HTTP 仍活
            // 但监督静默停摆 (探活/自愈/告警全停), 常驻组件不可接受。inner spawn
            // 隔离 (JoinError 捕获 panic), 崩了记 ERROR 下一 tick 续跑。
            // 锁纪律不变: tick 体内取锁, 外层 sleep 无锁。
            let core_inner = core_tick.clone();
            let tick_round = tokio::spawn(async move {
                let mut c = core_inner.lock().await;
                c.tick_all(now).await;
                // issue #10: 备份调度检查 (到期触发 run_backup, fail-closed 已告警)
                c.check_backup_schedule(now).await;
                // issue #37: retention sweep 调度检查 (到期裁剪 history.db 过期行)
                c.check_retention(now);
            });
            if let Err(e) = tick_round.await {
                tracing::error!(err = %e, "tick 单轮 panic (自愈续跑)");
            }
        }
    });
    // issue #22/#26/#25: HTTP 面 (metrics + 只读 status/history/events + web
    // 静态控制台 + token 校验编排操作), 第三个 spawn task, 共享 core.clone()。
    let http_handle = if let Some(addr) = &cfg.metrics_addr {
        let core_http = core.clone();
        let addr = addr.clone();
        let token_http = std::env::var("FUSION_SV_TOKEN").ok();
        let web_dir = cfg.web_dir.clone();
        let shutdown_for_http = shutdown_shared.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = fusion_supervisor::http_server::serve(
                core_http,
                &addr,
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
    // issue #29: daemon 启动即自动拉起 (探活跳过已健康, 不双启)。
    // PRD app-tiering (issue #62) 语义收窄: auto_up 只拉 T0 ∪ pin=auto,
    // 不再 up_all 全量 (40+ 服务启动风暴挤爆内存)。up_all 保留给 Web/CLI 显式操作。
    // 持锁跨 up_boot_set 与 RPC up 行为一致; 默认 false, 开发机手动行为不变。
    if cfg.auto_up_on_start {
        let core_boot = core.clone();
        tokio::spawn(async move {
            tracing::info!("auto_up_on_start: 启动即 up_boot_set (T0 ∪ pin=auto, layer 序)");
            let mut c = core_boot.lock().await;
            if let Err(e) = c.up_boot_set().await {
                tracing::error!(err = %e, "auto_up_on_start up_boot_set 部分失败 (tick 兜底)");
            }
        });
    }
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
    if let Some(h) = http_handle {
        h.abort();
    }
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

// issue #82: CLI 请求带来源 — daemon 侧停止/停止全部操作可追溯发起进程
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
