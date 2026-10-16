use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::auth::Auth;
use crate::claude::{Claude, ClaudeSettings};
use crate::client::ApiClient;
use crate::config::{self, Config};
use crate::credentials::CredentialStore;
use crate::decider::DecisionSettings;
use crate::events::{self, EventBus};
use crate::github::GitHub;
use crate::jobs::Jobs;
use crate::policy::PolicyStore;
use crate::poller::{PollSettings, Poller};
use crate::publications::Publications;
use crate::queue::Queue;
use crate::repos::Repos;
use crate::reviews::Reviews;
use crate::server::{self, AppState};
use crate::store;
use crate::transport::{self, Endpoint};
use crate::worker::{RetrySettings, Worker, WorkerParts};
use crate::workspace::Workspace;

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
    let events = EventBus::new(db.clone());

    let endpoint = Endpoint::for_home(&config.home);
    let listener = transport::bind(&endpoint)
        .await
        .with_context(|| format!("binding Local API on {endpoint}"))?;

    let shutdown = CancellationToken::new();
    let github = GitHub::new(&config.github_api_url, &config.github_web_url)?;
    let credentials = CredentialStore::open(config.credentials, config.home.clone()).await;
    let auth = Auth::load(
        github.clone(),
        config.github_client_id.clone(),
        credentials,
        db.clone(),
        events.clone(),
        shutdown.clone(),
    )
    .await?;

    let policies = PolicyStore::open(&config.home, db.clone(), events.clone())
        .await
        .context("opening policy store")?;
    spawn_policy_sync(
        policies.clone(),
        config.policy_sync_interval,
        shutdown.clone(),
    );

    let repos = Repos::new(db.clone(), github.clone(), auth.clone(), events.clone());
    let jobs = Jobs::new(db.clone());
    let queue = Queue::new(db.clone(), github.clone(), auth.clone(), events.clone());
    let poller = Poller::new(
        github.clone(),
        auth.clone(),
        repos.clone(),
        jobs.clone(),
        events.clone(),
        queue.clone(),
        PollSettings {
            include_team_requests: config.include_team_requests,
            skip_drafts: config.skip_drafts,
        },
    );
    poller.spawn(config.poll_interval, shutdown.clone());

    // Anything left running by a crash or shutdown goes back in the queue, with a clean checkout.
    let interrupted = jobs.mark_interrupted().await?;
    let workspace = Workspace::new(&config.home, &config.git_base_url);
    workspace.recover().await;
    if interrupted > 0 {
        tracing::info!(
            interrupted,
            "re-queued reviews interrupted by the last shutdown"
        );
    }
    let reviews = Reviews::new(db.clone());
    Worker::new(WorkerParts {
        jobs: jobs.clone(),
        reviews: reviews.clone(),
        poller: poller.clone(),
        auth: auth.clone(),
        policies: policies.clone(),
        workspace,
        claude: Claude::new(ClaudeSettings {
            bin: config.claude_bin.clone(),
            env: config.claude_env.clone(),
            model: config.claude_model.clone(),
            timeout: config.review_timeout,
            max_budget_usd: config.review_budget_usd,
        }),
        events: events.clone(),
        github: github.clone(),
        queue: queue.clone(),
        publications: Publications::new(db.clone()),
        decision: DecisionSettings {
            risk_threshold: config.risk_threshold,
        },
        retry: RetrySettings {
            base: config.retry_base,
            max_attempts: config.max_attempts,
        },
        concurrency: config.review_concurrency,
    })
    .spawn(shutdown.clone());

    let state = AppState {
        auth: auth.clone(),
        policies,
        repos,
        jobs,
        reviews,
        queue,
        poller,
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
    // Don't block startup on the network; a rejected token flips the state when GitHub answers.
    tokio::spawn(async move {
        if let Err(e) = auth.validate().await {
            tracing::warn!(error = %e, "GitHub token validation failed");
        }
    });
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

/// Picks up policy edits made in an external editor while the daemon runs.
fn spawn_policy_sync(
    policies: PolicyStore,
    every: std::time::Duration,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                () = shutdown.cancelled() => return,
            }
            if let Err(e) = policies.sync_from_disk().await {
                tracing::warn!(error = %e, "checking policy files failed");
            }
        }
    });
}
