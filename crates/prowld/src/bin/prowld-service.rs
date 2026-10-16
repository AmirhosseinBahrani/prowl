//! The daemon as run by the background service: no console window on Windows, logs to a file.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Context;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let home = match (args.next(), args.next()) {
        (Some(flag), Some(home)) if flag == "--home" => PathBuf::from(home),
        _ => prowld::config::default_home()?,
    };

    let logs = home.join("logs");
    std::fs::create_dir_all(&logs)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("prowld-service.log"))
        .context("opening the service log")?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(false)
        .with_writer(Mutex::new(log))
        .init();

    let daemon = match prowld::start(prowld::Config::new(home)).await {
        Ok(daemon) => daemon,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "prowld failed to start");
            return Err(e);
        }
    };
    let stop = daemon.shutdown_token();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            stop.cancel();
        }
    });
    #[cfg(unix)]
    {
        // launchd and systemd stop services with SIGTERM.
        let stop = daemon.shutdown_token();
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::spawn(async move {
            term.recv().await;
            stop.cancel();
        });
    }
    let result = daemon.wait().await;
    if let Err(e) = &result {
        tracing::error!(error = %format!("{e:#}"), "prowld stopped with an error");
    }
    result
}
