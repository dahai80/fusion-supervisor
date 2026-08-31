mod config;
mod manifest;

use clap::{Parser, Subcommand};
use config::Config;

#[derive(Parser)]
#[command(name = "fusion-sv", version, about = "Fusion service supervisor")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Daemon,
    Up { service: Option<String> },
    Down { service: Option<String> },
    Status,
    Restart { service: String },
    Logs { service: String },
    Top,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let _cfg = Config::load()?;
    match cli.command {
        None => tracing::info!("daemon 模式 (占位)"),
        Some(Commands::Daemon) => tracing::info!("daemon (占位)"),
        Some(Commands::Up { .. }) => tracing::info!("up (占位)"),
        Some(Commands::Down { .. }) => tracing::info!("down (占位)"),
        Some(Commands::Status) => tracing::info!("status (占位)"),
        Some(Commands::Restart { .. }) => tracing::info!("restart (占位)"),
        Some(Commands::Logs { .. }) => tracing::info!("logs (占位)"),
        Some(Commands::Top) => tracing::info!("top (占位)"),
    }
    Ok(())
}
