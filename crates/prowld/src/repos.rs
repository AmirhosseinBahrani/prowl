//! Repositories the connected account can see, and which of them PRowl watches.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::auth::Auth;
use crate::events::{self, EventBus};
use crate::github::{ApiRepo, GitHub, GitHubError};

pub mod kind {
    pub const WATCHED: &str = "repo.watched";
    pub const UNWATCHED: &str = "repo.unwatched";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub id: i64,
    pub full_name: String,
    pub owner: String,
    pub name: String,
    pub private: bool,
    pub archived: bool,
    pub default_branch: String,
    pub html_url: String,
    pub watched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPage {
    /// Matching repositories in total, across all pages.
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
    pub repos: Vec<Repo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RepoQuery {
    /// Case-insensitive substring of `owner/name`.
    pub q: Option<String>,
    /// 1-based.
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    /// Re-fetch the list from GitHub instead of using the cached copy.
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("connect a GitHub account first")]
    NotConnected,
    #[error("{0} not found, or the connected account cannot see it")]
    NotFound(String),
    #[error("{0} is archived; archived repositories cannot be watched")]
    Archived(String),
    #[error("GitHub request failed: {0}")]
    GitHub(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for RepoError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.into())
    }
}

#[derive(Clone)]
pub(crate) struct Repos {
    db: SqlitePool,
    github: GitHub,
    auth: Auth,
    events: EventBus,
}

type Row = (
    i64,
    String,
    String,
    String,
    bool,
    bool,
    String,
    String,
    bool,
);
const COLUMNS: &str =
    "id, full_name, owner, name, private, archived, default_branch, html_url, watched";

fn from_row(r: Row) -> Repo {
    Repo {
        id: r.0,
        full_name: r.1,
        owner: r.2,
        name: r.3,
        private: r.4,
        archived: r.5,
        default_branch: r.6,
        html_url: r.7,
        watched: r.8,
    }
}

impl Repos {
    pub(crate) fn new(db: SqlitePool, github: GitHub, auth: Auth, events: EventBus) -> Self {
        Self {
            db,
            github,
            auth,
            events,
        }
    }

    pub(crate) async fn list(&self, query: &RepoQuery) -> Result<RepoPage, RepoError> {
        let cached: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repos")
            .fetch_one(&self.db)
            .await?;
        if query.refresh || cached == 0 {
            self.refresh().await?;
        }
        let needle = query.q.as_deref().unwrap_or("").to_lowercase();
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM repos WHERE instr(lower(full_name), ?) > 0
             ORDER BY lower(full_name)"
        ))
        .bind(&needle)
        .fetch_all(&self.db)
        .await?;

        let per_page = query.per_page.unwrap_or(50).clamp(1, 200);
        let page = query.page.unwrap_or(1).max(1);
        let total = rows.len();
        let repos = rows
            .into_iter()
            .skip((page - 1) * per_page)
            .take(per_page)
            .map(from_row)
            .collect();
        Ok(RepoPage {
            total,
            page,
            per_page,
            repos,
        })
    }

    pub(crate) async fn watched(&self) -> Result<Vec<Repo>, RepoError> {
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM repos WHERE watched = 1 ORDER BY lower(full_name)"
        ))
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(from_row).collect())
    }

    pub(crate) async fn set_watched(
        &self,
        owner: &str,
        name: &str,
        watched: bool,
    ) -> Result<Repo, RepoError> {
        let full_name = format!("{owner}/{name}");
        let repo = match self.find(&full_name).await? {
            Some(repo) => repo,
            None => {
                // Not in the cached list (e.g. created since): ask GitHub directly.
                let token = self.token().await?;
                let found = self
                    .github
                    .get_repo(&token, owner, name)
                    .await
                    .map_err(|e| self.github_error(e))?
                    .ok_or_else(|| RepoError::NotFound(full_name.clone()))?;
                self.upsert(&[found]).await?;
                self.find(&full_name)
                    .await?
                    .ok_or_else(|| RepoError::NotFound(full_name.clone()))?
            }
        };
        if watched && repo.archived {
            return Err(RepoError::Archived(repo.full_name));
        }
        if repo.watched == watched {
            return Ok(repo);
        }
        sqlx::query("UPDATE repos SET watched = ? WHERE id = ?")
            .bind(watched)
            .bind(repo.id)
            .execute(&self.db)
            .await?;
        self.events
            .emit(
                if watched {
                    kind::WATCHED
                } else {
                    kind::UNWATCHED
                },
                serde_json::json!({ "repo": repo.full_name }),
            )
            .await?;
        Ok(Repo { watched, ..repo })
    }

    /// Replaces the cached list with GitHub's, keeping watch flags. Repositories that are gone
    /// are dropped unless watched (they stay, so the user notices instead of silently losing them).
    async fn refresh(&self) -> Result<(), RepoError> {
        let token = self.token().await?;
        let repos = self
            .github
            .list_repos(&token)
            .await
            .map_err(|e| self.github_error(e))?;
        let started = events::now_ms();
        self.upsert(&repos).await?;
        sqlx::query("DELETE FROM repos WHERE seen_at_ms < ? AND watched = 0")
            .bind(started)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    async fn upsert(&self, repos: &[ApiRepo]) -> Result<(), RepoError> {
        let now = events::now_ms();
        let mut tx = self.db.begin().await?;
        for r in repos {
            sqlx::query(
                "INSERT INTO repos (id, full_name, owner, name, private, archived, default_branch, html_url, seen_at_ms)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (id) DO UPDATE SET
                    full_name = excluded.full_name, owner = excluded.owner, name = excluded.name,
                    private = excluded.private, archived = excluded.archived,
                    default_branch = excluded.default_branch, html_url = excluded.html_url,
                    seen_at_ms = excluded.seen_at_ms",
            )
            .bind(r.id)
            .bind(&r.full_name)
            .bind(&r.owner.login)
            .bind(&r.name)
            .bind(r.private)
            .bind(r.archived)
            .bind(&r.default_branch)
            .bind(&r.html_url)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn find(&self, full_name: &str) -> Result<Option<Repo>, RepoError> {
        let row: Option<Row> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM repos WHERE lower(full_name) = lower(?)"
        ))
        .bind(full_name)
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(from_row))
    }

    async fn token(&self) -> Result<String, RepoError> {
        self.auth.token().await.ok_or(RepoError::NotConnected)
    }

    fn github_error(&self, e: GitHubError) -> RepoError {
        if let GitHubError::Unauthorized = e {
            let auth = self.auth.clone();
            tokio::spawn(async move {
                let _ = auth.token_rejected().await;
            });
            return RepoError::NotConnected;
        }
        RepoError::GitHub(e.to_string())
    }
}
