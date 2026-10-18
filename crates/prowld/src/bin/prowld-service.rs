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

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");
    // Listen for a quit from the start: startup can take a while (the OS keychain may wait for
    // permission), and a quit then must still be a clean exit, or the service manager restarts
    // it as if it had crashed.
    let quit = tokio_util::sync::CancellationToken::new();
    watch_for_quit(quit.clone())?;
    let daemon = tokio::select! {
        started = prowld::start(prowld::Config::new(home)) => match started {
            Ok(daemon) => daemon,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "prowld failed to start");
                return Err(e);
            }
        },
        () = quit.cancelled() => {
            tracing::info!("asked to quit while starting; stopped");
            return Ok(());
        }
    };
    let stop = daemon.shutdown_token();
    tokio::spawn(async move {
        quit.cancelled().await;
        tracing::info!("asked to quit; stopping (the service won't restart it until next login)");
        stop.cancel();
    });
    let result = daemon.wait().await;
    if let Err(e) = &result {
        tracing::error!(error = %format!("{e:#}"), "prowld stopped with an error");
    }
    result
}

/// Cancels `quit` on Ctrl-C, and on Unix on SIGTERM — what launchd and systemd send to stop a
/// service, and what a plain `kill` or Activity Monitor's "Quit" sends.
fn watch_for_quit(quit: tokio_util::sync::CancellationToken) -> anyhow::Result<()> {
    let on_ctrl_c = quit.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    });
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::spawn(async move {
            term.recv().await;
            quit.cancel();
        });
    }
    Ok(())
}
