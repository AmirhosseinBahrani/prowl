use std::convert::Infallible;
use std::time::Instant;

use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

use crate::auth::{Auth, AuthError, AuthState};
use crate::events::EventBus;
use crate::jobs::{Job, Jobs};
use crate::policy::PolicyStore;
use crate::poller::{PollSummary, Poller};
use crate::repos::Repos;

#[derive(Clone)]
pub(crate) struct AppState {
    pub auth: Auth,
    pub policies: PolicyStore,
    pub repos: Repos,
    pub jobs: Jobs,
    pub poller: Poller,
    pub token: String,
    pub events: EventBus,
    pub started: Instant,
    pub started_at_ms: i64,
    pub shutdown: CancellationToken,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub version: String,
    pub pid: u32,
    pub started_at_ms: i64,
    pub uptime_secs: u64,
    pub github: AuthState,
}

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/events", get(events))
        .route("/v1/auth", get(auth_state).delete(disconnect))
        .route("/v1/auth/device", post(start_device_flow))
        .route("/v1/poll", post(poll_now))
        .route("/v1/jobs", get(list_jobs))
        .merge(crate::policy_api::routes())
        .merge(crate::repos_api::routes())
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .with_state(state)
}

async fn require_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(t) if constant_time_eq(t.as_bytes(), state.token.as_bytes()) => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "missing or invalid API token").into_response(),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn status(State(state): State<AppState>) -> Json<Status> {
    Json(Status {
        version: env!("CARGO_PKG_VERSION").into(),
        pid: std::process::id(),
        started_at_ms: state.started_at_ms,
        uptime_secs: state.started.elapsed().as_secs(),
        github: state.auth.state().await,
    })
}

/// Runs a poll immediately (also used by the UI's "check now").
async fn poll_now(State(state): State<AppState>) -> Json<PollSummary> {
    Json(state.poller.poll_once().await)
}

async fn list_jobs(State(state): State<AppState>) -> Result<Json<Vec<Job>>, ApiError> {
    Ok(Json(state.jobs.list().await.map_err(ApiError::internal)?))
}

async fn auth_state(State(state): State<AppState>) -> Json<AuthState> {
    Json(state.auth.state().await)
}

async fn start_device_flow(State(state): State<AppState>) -> Result<Json<AuthState>, ApiError> {
    match state.auth.start_device_flow().await {
        Ok(pending) => Ok(Json(pending)),
        Err(e @ AuthError::NoClientId) => {
            Err(ApiError(StatusCode::PRECONDITION_FAILED, e.to_string()))
        }
        Err(e @ AuthError::GitHub(_)) => Err(ApiError(StatusCode::BAD_GATEWAY, e.to_string())),
        Err(AuthError::Internal(e)) => Err(ApiError::internal(e)),
    }
}

async fn disconnect(State(state): State<AppState>) -> Result<Json<AuthState>, ApiError> {
    state.auth.disconnect().await.map_err(ApiError::internal)?;
    Ok(Json(state.auth.state().await))
}

/// An error answered as `{"error": "..."}` with the given status.
pub(crate) struct ApiError(pub StatusCode, pub String);

impl ApiError {
    pub(crate) fn internal(e: anyhow::Error) -> Self {
        tracing::error!(error = %format!("{e:#}"), "request failed");
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    since: i64,
}

/// Replays stored events after `since`, then streams new ones live.
async fn events(
    State(state): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> Result<Sse<impl Stream<Item = Result<sse::Event, Infallible>>>, StatusCode> {
    // Subscribe before reading history so nothing emitted in between is lost.
    let mut live = state.events.subscribe();
    let backlog = state
        .events
        .since(q.since)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let bus = state.events.clone();

    let stream = async_stream::stream! {
        let mut last = q.since;
        for event in backlog {
            last = event.id;
            yield event;
        }
        loop {
            match live.recv().await {
                Ok(event) if event.id > last => {
                    last = event.id;
                    yield event;
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {
                    // Fell behind the channel: catch up from the database.
                    if let Ok(missed) = bus.since(last).await {
                        for event in missed {
                            last = event.id;
                            yield event;
                        }
                    }
                }
                Err(RecvError::Closed) => break,
            }
        }
    }
    .map(|event| {
        Ok(sse::Event::default()
            .id(event.id.to_string())
            .event(event.kind.clone())
            .data(serde_json::to_string(&event).unwrap_or_default()))
    })
    .take_until(state.shutdown.cancelled_owned());

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
