//! Local API routes for the Human Review Queue.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::policy::Scope;
use crate::queue::{QueueDetail, QueueError, QueueItem, Resolution};
use crate::server::{ApiError, AppState};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/queue", get(list))
        .route("/v1/queue/{owner}/{repo}/{number}", get(detail))
        .route("/v1/queue/{owner}/{repo}/{number}/approve", post(approve))
        .route(
            "/v1/queue/{owner}/{repo}/{number}/request-changes",
            post(request_changes),
        )
        .route("/v1/queue/{owner}/{repo}/{number}/dismiss", post(dismiss))
}

impl From<QueueError> for ApiError {
    fn from(e: QueueError) -> Self {
        let status = match &e {
            QueueError::NotQueued(_) => StatusCode::NOT_FOUND,
            QueueError::NotConnected => StatusCode::CONFLICT,
            QueueError::CommentRequired => StatusCode::UNPROCESSABLE_ENTITY,
            QueueError::GitHub(_) => StatusCode::BAD_GATEWAY,
            QueueError::Internal(e) => return ApiError::internal(anyhow::anyhow!("{e:#}")),
        };
        ApiError(status, e.to_string())
    }
}

#[derive(Deserialize, Default)]
struct Comment {
    #[serde(default)]
    comment: String,
}

type Pr = Path<(String, String, u64)>;

fn repo_name(owner: &str, repo: &str) -> Result<String, ApiError> {
    Scope::repo(owner, repo)?;
    Ok(format!("{owner}/{repo}"))
}

async fn list(State(s): State<AppState>) -> Result<Json<Vec<QueueItem>>, ApiError> {
    Ok(Json(s.queue.pending().await?))
}

async fn detail(
    State(s): State<AppState>,
    Path((owner, repo, n)): Pr,
) -> Result<Json<QueueDetail>, ApiError> {
    Ok(Json(s.queue.detail(&repo_name(&owner, &repo)?, n).await?))
}

async fn resolve(
    s: &AppState,
    owner: &str,
    repo: &str,
    n: u64,
    how: Resolution,
    comment: &str,
) -> Result<Json<QueueItem>, ApiError> {
    Ok(Json(
        s.queue
            .resolve(&repo_name(owner, repo)?, n, how, comment)
            .await?,
    ))
}

async fn approve(
    State(s): State<AppState>,
    Path((owner, repo, n)): Pr,
    body: Option<Json<Comment>>,
) -> Result<Json<QueueItem>, ApiError> {
    let comment = body.map(|b| b.0.comment).unwrap_or_default();
    resolve(&s, &owner, &repo, n, Resolution::Approve, &comment).await
}

async fn request_changes(
    State(s): State<AppState>,
    Path((owner, repo, n)): Pr,
    body: Option<Json<Comment>>,
) -> Result<Json<QueueItem>, ApiError> {
    let comment = body.map(|b| b.0.comment).unwrap_or_default();
    resolve(&s, &owner, &repo, n, Resolution::RequestChanges, &comment).await
}

async fn dismiss(
    State(s): State<AppState>,
    Path((owner, repo, n)): Pr,
) -> Result<Json<QueueItem>, ApiError> {
    resolve(&s, &owner, &repo, n, Resolution::Dismiss, "").await
}
