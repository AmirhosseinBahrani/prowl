use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::client::ApiClient;
use crate::config::{self, Config};
use crate::events::{self, EventBus};
use crate::server::{self, AppState};
use crate::store;
use crate::transport::{self, Endpoint};

/// A daemon running inside the current process.
pub struct RunningDaemon {
    config: Config,
    endpoint: Endpoint,
    token: String,
    shutdown: CancellationToken,
    task: JoinHandle<anyhow::Result<()>>,
}

impl RunningDaemon {
    pub fn home(&self) -> &Path {
        &self.config.home
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn client(&self) -> ApiClient {
        ApiClient::new(self.endpoint.clone(), self.token.clone())
    }

    /// Stops serving, closes open event streams and waits for the server to finish.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.shutdown.cancel();
        self.task.await.context("daemon task panicked")?
    }

    /// Resolves when the daemon stops on its own (e.g. the server failed).
    pub async fn wait(self) -> anyhow::Result<()> {
        self.task.await.context("daemon task panicked")?
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }
}

pub async fn start(config: Config) -> anyhow::Result<RunningDaemon> {
    std::fs::create_dir_all(&config.home)
        .with_context(|| format!("creating PRowl home {}", config.home.display()))?;
    let token = config::ensure_token(&config.token_path())?;
    let db = store::open(&config.db_path())
        .await
        .context("opening database")?;
    let events = EventBus::new(db);

    let endpoint = Endpoint::for_home(&config.home);
    let listener = transport::bind(&endpoint)
        .await
        .with_context(|| format!("binding Local API on {endpoint}"))?;

    let shutdown = CancellationToken::new();
    let state = AppState {
        token: token.clone(),
        events: events.clone(),
        started: Instant::now(),
        started_at_ms: events::now_ms(),
        shutdown: shutdown.clone(),
    };
    let app = server::router(state);

    let task = {
        let shutdown = shutdown.clone();
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            let result = serve(listener, app, shutdown).await;
            transport::cleanup(&endpoint);
            result
        })
    };

    events
        .emit(
            events::kind::DAEMON_STARTED,
            serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "endpoint": endpoint.as_str() }),
        )
        .await?;
    tracing::info!(%endpoint, home = %config.home.display(), "prowld ready");

    Ok(RunningDaemon {
        config,
        endpoint,
        token,
        shutdown,
        task,
    })
}

async fn serve(
    listener: transport::Listener,
    app: axum::Router,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("Local API server failed")
}
