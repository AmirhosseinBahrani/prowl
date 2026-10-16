//! Local API routes for repositories and watch selection.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};

use crate::policy::Scope;
use crate::repos::{Repo, RepoError, RepoPage, RepoQuery};
use crate::server::{ApiError, AppState};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/repos", get(list))
        .route("/v1/repos/watched", get(watched))
        .route("/v1/repos/{owner}/{repo}/watch", put(watch).delete(unwatch))
}

impl From<RepoError> for ApiError {
    fn from(e: RepoError) -> Self {
        let status = match &e {
            RepoError::NotConnected => StatusCode::CONFLICT,
            RepoError::NotFound(_) => StatusCode::NOT_FOUND,
            RepoError::Archived(_) => StatusCode::UNPROCESSABLE_ENTITY,
            RepoError::GitHub(_) => StatusCode::BAD_GATEWAY,
            RepoError::Internal(e) => return ApiError::internal(anyhow::anyhow!("{e:#}")),
        };
        ApiError(status, e.to_string())
    }
}

async fn list(
    State(s): State<AppState>,
    Query(q): Query<RepoQuery>,
) -> Result<Json<RepoPage>, ApiError> {
    Ok(Json(s.repos.list(&q).await?))
}

async fn watched(State(s): State<AppState>) -> Result<Json<Vec<Repo>>, ApiError> {
    Ok(Json(s.repos.watched().await?))
}

async fn watch(
    State(s): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<Repo>, ApiError> {
    set(&s, &owner, &repo, true).await
}

async fn unwatch(
    State(s): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Json<Repo>, ApiError> {
    set(&s, &owner, &repo, false).await
}

async fn set(s: &AppState, owner: &str, repo: &str, watched: bool) -> Result<Json<Repo>, ApiError> {
    // Names go into GitHub URLs; reject anything that isn't a plain owner/repo.
    Scope::repo(owner, repo)?;
    Ok(Json(s.repos.set_watched(owner, repo, watched).await?))
}
