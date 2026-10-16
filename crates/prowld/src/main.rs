use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use prowld::{ApiClient, Config};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "prowld", version, about = "PRowl background daemon")]
struct Cli {
    /// Directory holding PRowl's database, token and socket.
    #[arg(long, env = "PROWL_HOME", global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground (default).
    Run,
    /// Ask a running daemon for its status.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let home = match cli.home {
        Some(home) => home,
        None => prowld::config::default_home()?,
    };

    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(Config::new(home)).await,
        Command::Status => {
            let status = ApiClient::for_home(&home)?.status().await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(())
        }
    }
}

async fn run(config: Config) -> anyhow::Result<()> {
    let daemon = prowld::start(config).await?;
    let stop = daemon.shutdown_token();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutting down");
            stop.cancel();
        }
    });
    daemon.wait().await.context("prowld stopped with an error")
}
