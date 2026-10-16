use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use prowld::{ApiClient, AuthState, Config};
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
    /// Connect a GitHub account through the running daemon (device flow).
    Login,
    /// Disconnect the GitHub account and delete its stored token.
    Logout,
    /// Manage the per-user background service that keeps prowld running.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
        /// Service name, to run more than one install side by side.
        #[arg(long, default_value = prowld::service::DEFAULT_NAME, global = true)]
        name: String,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install and start the service; it then starts at every login.
    Install,
    /// Stop and remove the service.
    Uninstall,
    /// Show whether the service is installed and running.
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
        Command::Login => login(&ApiClient::for_home(&home)?).await,
        Command::Service { action, name } => {
            use prowld::service::{self, ServiceSpec};
            let spec = ServiceSpec::beside_current_exe(&name, &home)?;
            let status = match action {
                ServiceAction::Install => service::install(&spec)?,
                ServiceAction::Uninstall => service::uninstall(&spec)?,
                ServiceAction::Status => service::status(&spec)?,
            };
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(())
        }
        Command::Logout => {
            ApiClient::for_home(&home)?.disconnect().await?;
            println!("GitHub account disconnected.");
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

async fn login(client: &ApiClient) -> anyhow::Result<()> {
    if let AuthState::Pending {
        user_code,
        verification_uri,
        ..
    } = client.start_login().await?
    {
        println!("Open {verification_uri} and enter the code: {user_code}");
    }
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        match client.auth_state().await? {
            AuthState::Pending { .. } => {}
            AuthState::Connected { login, .. } => {
                println!("Connected as {login}.");
                return Ok(());
            }
            AuthState::Failed { reason } => anyhow::bail!("GitHub login failed: {reason}"),
            other => anyhow::bail!("GitHub login ended unexpectedly: {other:?}"),
        }
    }
}
