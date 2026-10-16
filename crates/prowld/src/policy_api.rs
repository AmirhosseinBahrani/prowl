//! Local API routes for the Review Policy.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::policy::{EffectivePolicy, PolicyError, PolicySummary, PolicyVersion, Scope};
use crate::server::{ApiError, AppState};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/policy", get(effective))
        .route("/v1/policies", get(list))
        .route("/v1/policies/global", get(get_global).put(put_global))
        .route("/v1/policies/global/versions", get(global_versions))
        .route(
            "/v1/policies/global/versions/{id}/restore",
            post(restore_global),
        )
        .route(
            "/v1/policies/repos/{owner}/{repo}",
            get(get_repo).put(put_repo).delete(delete_repo),
        )
        .route(
            "/v1/policies/repos/{owner}/{repo}/versions",
            get(repo_versions),
        )
        .route(
            "/v1/policies/repos/{owner}/{repo}/versions/{id}/restore",
            post(restore_repo),
        )
}

#[derive(Deserialize)]
struct EffectiveQuery {
    repo: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PolicyBody {
    content: String,
}

impl From<PolicyError> for ApiError {
    fn from(e: PolicyError) -> Self {
        let status = match &e {
            PolicyError::InvalidRepo(_) | PolicyError::Empty | PolicyError::TooLarge => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            PolicyError::NotFound(_) | PolicyError::UnknownVersion(_) => StatusCode::NOT_FOUND,
            PolicyError::Internal(e) => return ApiError::internal(anyhow::anyhow!("{e:#}")),
        };
        ApiError(status, e.to_string())
    }
}

type Result<T> = std::result::Result<Json<T>, ApiError>;

async fn effective(
    State(s): State<AppState>,
    Query(q): Query<EffectiveQuery>,
) -> Result<EffectivePolicy> {
    let repo = q.repo.as_deref().map(Scope::parse_repo).transpose()?;
    Ok(Json(s.policies.effective(repo.as_ref()).await?))
}

async fn list(State(s): State<AppState>) -> Result<Vec<PolicySummary>> {
    Ok(Json(s.policies.list().await?))
}

async fn get_scope(s: &AppState, scope: Scope) -> Result<EffectivePolicy> {
    let key = scope.key();
    s.policies
        .current(&scope)
        .await?
        .map(Json)
        .ok_or_else(|| PolicyError::NotFound(key).into())
}

async fn get_global(State(s): State<AppState>) -> Result<EffectivePolicy> {
    get_scope(&s, Scope::Global).await
}

async fn put_global(
    State(s): State<AppState>,
    Json(b): Json<PolicyBody>,
) -> Result<EffectivePolicy> {
    Ok(Json(s.policies.save(&Scope::Global, &b.content).await?))
}

async fn global_versions(State(s): State<AppState>) -> Result<Vec<PolicyVersion>> {
    Ok(Json(s.policies.versions(&Scope::Global).await?))
}

async fn restore_global(State(s): State<AppState>, Path(id): Path<i64>) -> Result<EffectivePolicy> {
    Ok(Json(s.policies.restore(&Scope::Global, id).await?))
}

async fn get_repo(
    State(s): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<EffectivePolicy> {
    get_scope(&s, Scope::repo(&owner, &repo)?).await
}

async fn put_repo(
    State(s): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Json(b): Json<PolicyBody>,
) -> Result<EffectivePolicy> {
    Ok(Json(
        s.policies
            .save(&Scope::repo(&owner, &repo)?, &b.content)
            .await?,
    ))
}

async fn delete_repo(
    State(s): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> std::result::Result<StatusCode, ApiError> {
    s.policies.delete(&Scope::repo(&owner, &repo)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn repo_versions(
    State(s): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
) -> Result<Vec<PolicyVersion>> {
    Ok(Json(
        s.policies.versions(&Scope::repo(&owner, &repo)?).await?,
    ))
}

async fn restore_repo(
    State(s): State<AppState>,
    Path((owner, repo, id)): Path<(String, String, i64)>,
) -> Result<EffectivePolicy> {
    Ok(Json(
        s.policies.restore(&Scope::repo(&owner, &repo)?, id).await?,
    ))
}
