//! The connected GitHub account: device-flow login, token validation and disconnect.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::credentials::CredentialStore;
use crate::events::{self, EventBus};
use crate::github::{GitHub, GitHubError, TokenPoll};

pub mod kind {
    pub const PENDING: &str = "auth.pending";
    pub const CONNECTED: &str = "auth.connected";
    pub const FAILED: &str = "auth.failed";
    pub const DISCONNECTED: &str = "auth.disconnected";
    pub const RECONNECT_REQUIRED: &str = "auth.reconnect_required";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AuthState {
    Disconnected,
    /// Waiting for the user to enter `user_code` at `verification_uri`.
    Pending {
        user_code: String,
        verification_uri: String,
        expires_at_ms: i64,
    },
    Connected {
        login: String,
        name: Option<String>,
    },
    /// A token is stored but GitHub no longer accepts it.
    ReconnectRequired {
        login: String,
    },
    Failed {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(
        "no GitHub OAuth client id configured; set PROWL_GITHUB_CLIENT_ID to the PRowl OAuth App's client id"
    )]
    NoClientId,
    #[error("could not start GitHub login: {0}")]
    GitHub(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Clone)]
pub(crate) struct Auth {
    inner: Arc<Inner>,
}

struct Inner {
    github: GitHub,
    client_id: Option<String>,
    credentials: CredentialStore,
    db: SqlitePool,
    events: EventBus,
    state: Mutex<AuthState>,
    /// Cancels the device-flow poll currently in flight, if any.
    flow: Mutex<Option<CancellationToken>>,
    shutdown: CancellationToken,
}

impl Auth {
    pub(crate) async fn load(
        github: GitHub,
        client_id: Option<String>,
        credentials: CredentialStore,
        db: SqlitePool,
        events: EventBus,
        shutdown: CancellationToken,
    ) -> anyhow::Result<Self> {
        let account: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT login, name FROM account WHERE id = 1")
                .fetch_optional(&db)
                .await?;
        let has_token = credentials.get().await?.is_some();
        let state = match (account, has_token) {
            (Some((login, name)), true) => AuthState::Connected { login, name },
            (Some((login, _)), false) => AuthState::ReconnectRequired { login },
            (None, _) => AuthState::Disconnected,
        };
        tracing::info!(
            store = credentials.describe(),
            ?state,
            "GitHub account loaded"
        );
        Ok(Self {
            inner: Arc::new(Inner {
                github,
                client_id,
                credentials,
                db,
                events,
                state: Mutex::new(state),
                flow: Mutex::new(None),
                shutdown,
            }),
        })
    }

    pub(crate) async fn state(&self) -> AuthState {
        self.inner.state.lock().await.clone()
    }

    /// The stored token, if the account is usable.
    #[allow(dead_code)] // used by the poller (ticket 04)
    pub(crate) async fn token(&self) -> Option<String> {
        match *self.inner.state.lock().await {
            AuthState::Connected { .. } => self.inner.credentials.get().await.ok().flatten(),
            _ => None,
        }
    }

    /// Checks the stored token against GitHub. Network errors leave the state unchanged.
    pub(crate) async fn validate(&self) -> anyhow::Result<()> {
        let Some(token) = self.inner.credentials.get().await? else {
            return Ok(());
        };
        match self.inner.github.current_user(&token).await {
            Ok(user) => self.save_account(&user.login, user.name.as_deref()).await,
            Err(GitHubError::Unauthorized) => self.token_rejected().await,
            Err(e) => {
                tracing::warn!(error = %e, "could not validate GitHub token; will retry later");
                Ok(())
            }
        }
    }

    /// Marks the account as needing reconnection after GitHub rejected its token.
    pub(crate) async fn token_rejected(&self) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().await;
        let login = match &*state {
            AuthState::Connected { login, .. } | AuthState::ReconnectRequired { login } => {
                login.clone()
            }
            _ => return Ok(()),
        };
        if matches!(*state, AuthState::ReconnectRequired { .. }) {
            return Ok(());
        }
        *state = AuthState::ReconnectRequired {
            login: login.clone(),
        };
        drop(state);
        self.inner
            .events
            .emit(
                kind::RECONNECT_REQUIRED,
                serde_json::json!({ "login": login }),
            )
            .await?;
        Ok(())
    }

    /// Starts a device flow and polls GitHub in the background until it resolves.
    pub(crate) async fn start_device_flow(&self) -> Result<AuthState, AuthError> {
        let client_id = self.inner.client_id.clone().ok_or(AuthError::NoClientId)?;
        let code = self
            .inner
            .github
            .request_device_code(&client_id)
            .await
            .map_err(|e| AuthError::GitHub(e.to_string()))?;

        let cancel = self.inner.shutdown.child_token();
        if let Some(previous) = self.inner.flow.lock().await.replace(cancel.clone()) {
            previous.cancel();
        }

        let pending = AuthState::Pending {
            user_code: code.user_code.clone(),
            verification_uri: code.verification_uri.clone(),
            expires_at_ms: events::now_ms() + code.expires_in as i64 * 1000,
        };
        *self.inner.state.lock().await = pending.clone();
        self.inner
            .events
            .emit(
                kind::PENDING,
                serde_json::to_value(&pending).map_err(anyhow::Error::from)?,
            )
            .await?;

        let this = self.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(code.expires_in);
            let outcome = tokio::select! {
                outcome = this.poll_until_resolved(&client_id, &code.device_code, code.interval, deadline) => outcome,
                () = cancel.cancelled() => return,
            };
            if let Err(e) = this.finish_flow(outcome).await {
                tracing::error!(error = %e, "finishing GitHub login failed");
            }
        });
        Ok(pending)
    }

    async fn poll_until_resolved(
        &self,
        client_id: &str,
        device_code: &str,
        interval: u64,
        deadline: tokio::time::Instant,
    ) -> Result<String, String> {
        let mut interval = interval;
        loop {
            // GitHub asks for at least `interval` seconds between polls.
            let wait = if interval == 0 {
                Duration::from_millis(20)
            } else {
                Duration::from_secs(interval)
            };
            tokio::time::sleep(wait).await;
            if tokio::time::Instant::now() >= deadline {
                return Err("the login code expired".into());
            }
            match self
                .inner
                .github
                .poll_device_token(client_id, device_code)
                .await
            {
                Ok(TokenPoll::Granted(token)) => return Ok(token),
                Ok(TokenPoll::Pending) => {}
                Ok(TokenPoll::SlowDown { interval: next }) => interval = next,
                Ok(TokenPoll::Expired) => return Err("the login code expired".into()),
                Ok(TokenPoll::Denied) => return Err("access was denied on GitHub".into()),
                Ok(TokenPoll::Failed(reason)) => return Err(reason),
                Err(e) => tracing::warn!(error = %e, "device-flow poll failed; retrying"),
            }
        }
    }

    async fn finish_flow(&self, outcome: Result<String, String>) -> anyhow::Result<()> {
        let reason = match outcome {
            Ok(token) => match self.inner.github.current_user(&token).await {
                Ok(user) => {
                    self.inner.credentials.set(token).await?;
                    return self.save_account(&user.login, user.name.as_deref()).await;
                }
                Err(e) => format!("could not read the GitHub account: {e}"),
            },
            Err(reason) => reason,
        };
        *self.inner.state.lock().await = AuthState::Failed {
            reason: reason.clone(),
        };
        self.inner
            .events
            .emit(kind::FAILED, serde_json::json!({ "reason": reason }))
            .await?;
        Ok(())
    }

    async fn save_account(&self, login: &str, name: Option<&str>) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO account (id, login, name, connected_at_ms) VALUES (1, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET login = excluded.login, name = excluded.name",
        )
        .bind(login)
        .bind(name)
        .bind(events::now_ms())
        .execute(&self.inner.db)
        .await?;
        let connected = AuthState::Connected {
            login: login.to_owned(),
            name: name.map(str::to_owned),
        };
        let changed = {
            let mut state = self.inner.state.lock().await;
            let changed = *state != connected;
            *state = connected.clone();
            changed
        };
        if changed {
            self.inner
                .events
                .emit(kind::CONNECTED, serde_json::to_value(&connected)?)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn disconnect(&self) -> anyhow::Result<()> {
        if let Some(flow) = self.inner.flow.lock().await.take() {
            flow.cancel();
        }
        self.inner.credentials.delete().await?;
        sqlx::query("DELETE FROM account")
            .execute(&self.inner.db)
            .await?;
        *self.inner.state.lock().await = AuthState::Disconnected;
        self.inner
            .events
            .emit(kind::DISCONNECTED, serde_json::json!({}))
            .await?;
        Ok(())
    }
}
