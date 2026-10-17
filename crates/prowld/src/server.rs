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
use crate::power::{Power, PowerStatus};
use crate::queue::Queue;
use crate::repos::Repos;
use crate::reviews::{Review, Reviews};

#[derive(Clone)]
pub(crate) struct AppState {
    pub auth: Auth,
    pub policies: PolicyStore,
    pub repos: Repos,
    pub jobs: Jobs,
    pub reviews: Reviews,
    pub transcripts: crate::transcripts::Transcripts,
    pub updates: crate::updates::Updates,
    pub settings: crate::settings::SettingsStore,
    pub power: Power,
    pub claude: std::sync::Arc<crate::claude::Claude>,
    pub claude_env: Vec<(String, String)>,
    pub worker: crate::worker::Worker,
    pub queue: Queue,
    pub poller: Poller,
    pub token: String,
    pub events: EventBus,
    pub started: Instant,
    pub started_at_ms: i64,
    pub shutdown: CancellationToken,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Status {
    pub version: String,
    pub pid: u32,
    pub started_at_ms: i64,
    pub uptime_secs: u64,
    pub github: AuthState,
    /// The latest release, once checked.
    #[serde(default)]
    pub update: Option<crate::updates::UpdateInfo>,
}

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/events", get(events))
        .route("/v1/events/recent", get(recent_events))
        .route("/v1/auth", get(auth_state).delete(disconnect))
        .route("/v1/auth/device", post(start_device_flow))
        .route("/v1/auth/token", post(sign_in_with_token))
        .route("/v1/poll", post(poll_now))
        .route("/v1/jobs", get(list_jobs))
        .route("/v1/jobs/{id}/transcript", get(job_transcript))
        .route("/v1/reviews", get(list_reviews))
        .route("/v1/power", get(power_status))
        .route("/v1/costs", get(costs))
        .route("/v1/settings", get(get_settings).put(put_settings))
        .route("/v1/doctor", get(doctor))
        .route("/v1/dry-run", post(dry_run))
        .merge(crate::policy_api::routes())
        .merge(crate::repos_api::routes())
        .merge(crate::queue_api::routes())
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
        update: state.updates.get(),
    })
}

/// Runs a poll immediately (also used by the UI's "check now").
async fn poll_now(State(state): State<AppState>) -> Json<PollSummary> {
    Json(state.poller.poll_once().await)
}

async fn list_jobs(State(state): State<AppState>) -> Result<Json<Vec<Job>>, ApiError> {
    Ok(Json(state.jobs.list().await.map_err(ApiError::internal)?))
}

async fn job_transcript(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<Vec<crate::transcripts::TranscriptEntry>>, ApiError> {
    Ok(Json(
        state
            .transcripts
            .list(id)
            .await
            .map_err(ApiError::internal)?,
    ))
}

#[derive(Deserialize)]
struct ReviewsQuery {
    job_id: Option<i64>,
}

async fn list_reviews(
    State(state): State<AppState>,
    Query(q): Query<ReviewsQuery>,
) -> Result<Json<Vec<Review>>, ApiError> {
    Ok(Json(
        state
            .reviews
            .list(q.job_id)
            .await
            .map_err(ApiError::internal)?,
    ))
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Costs {
    /// Claude spend since midnight UTC.
    pub today_usd: f64,
    pub reviews_today: i64,
    pub daily_budget_usd: Option<f64>,
    /// When "today" ends and the budget resets.
    pub resets_at_ms: i64,
}

async fn get_settings(State(state): State<AppState>) -> Json<crate::settings::Settings> {
    Json(state.settings.get())
}

/// Changes any subset of the settings; they apply without a restart.
async fn put_settings(
    State(state): State<AppState>,
    Json(patch): Json<serde_json::Value>,
) -> Result<Json<crate::settings::Settings>, ApiError> {
    use crate::settings::SettingsError;
    match state.settings.update(&patch).await {
        Ok(settings) => Ok(Json(settings)),
        Err(SettingsError::Invalid(message)) => {
            Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, message))
        }
        Err(SettingsError::Internal(e)) => Err(ApiError::internal(e)),
    }
}

async fn costs(State(state): State<AppState>) -> Result<Json<Costs>, ApiError> {
    let settings = state.settings.get();
    let day = settings.day_start_ms(crate::events::now_ms());
    let (today_usd, reviews_today) = state
        .reviews
        .spent_since(day)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(Costs {
        today_usd,
        reviews_today,
        daily_budget_usd: settings.daily_budget_usd,
        resets_at_ms: settings.day_start_ms(day + 86_400_000 + 3_600_000),
    }))
}

#[derive(Deserialize)]
struct DryRunRequest {
    repo: String,
    pr: u64,
}

/// Reviews a PR without publishing anything (to try out a policy change).
async fn dry_run(
    State(state): State<AppState>,
    Json(req): Json<DryRunRequest>,
) -> Result<Json<crate::worker::DryRun>, ApiError> {
    state
        .worker
        .dry_run(&req.repo, req.pr)
        .await
        .map(Json)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))
}

async fn doctor(State(state): State<AppState>) -> Json<crate::doctor::Doctor> {
    Json(crate::doctor::check(&state.claude, &state.claude_env).await)
}

async fn power_status(State(state): State<AppState>) -> Json<PowerStatus> {
    Json(state.power.status())
}

async fn auth_state(State(state): State<AppState>) -> Json<AuthState> {
    Json(state.auth.state().await)
}

async fn start_device_flow(State(state): State<AppState>) -> Result<Json<AuthState>, ApiError> {
    match state.auth.start_device_flow().await {
        Ok(pending) => Ok(Json(pending)),
        Err(e) => Err(auth_error(e)),
    }
}

#[derive(serde::Deserialize)]
struct TokenSignIn {
    token: String,
}

async fn sign_in_with_token(
    State(state): State<AppState>,
    Json(body): Json<TokenSignIn>,
) -> Result<Json<AuthState>, ApiError> {
    state
        .auth
        .sign_in_with_token(&body.token)
        .await
        .map(Json)
        .map_err(auth_error)
}

fn auth_error(e: AuthError) -> ApiError {
    match e {
        AuthError::NoClientId => ApiError(StatusCode::PRECONDITION_FAILED, e.to_string()),
        AuthError::InvalidToken => ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
        AuthError::GitHub(_) => ApiError(StatusCode::BAD_GATEWAY, e.to_string()),
        AuthError::Internal(e) => ApiError::internal(e),
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
    /// Skip stored events and stream only new ones.
    #[serde(default)]
    live: bool,
}

#[derive(Deserialize)]
struct RecentQuery {
    #[serde(default = "default_recent")]
    limit: i64,
}

fn default_recent() -> i64 {
    200
}

async fn recent_events(
    State(state): State<AppState>,
    Query(q): Query<RecentQuery>,
) -> Result<Json<Vec<crate::events::Event>>, ApiError> {
    let limit = q.limit.clamp(1, 1000);
    Ok(Json(
        state
            .events
            .recent(limit)
            .await
            .map_err(ApiError::internal)?,
    ))
}

/// Replays stored events after `since`, then streams new ones live.
async fn events(
    State(state): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> Result<Sse<impl Stream<Item = Result<sse::Event, Infallible>>>, StatusCode> {
    // Subscribe before reading history so nothing emitted in between is lost.
    let mut live = state.events.subscribe();
    let (since, backlog) = if q.live {
        let latest = state
            .events
            .latest_id()
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        (latest, Vec::new())
    } else {
        let backlog = state
            .events
            .since(q.since)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        (q.since, backlog)
    };
    let bus = state.events.clone();

    let stream = async_stream::stream! {
        let mut last = since;
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
