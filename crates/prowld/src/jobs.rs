//! The durable review queue.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::events;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: i64,
    pub repo: String,
    pub pr_number: u64,
    pub head_sha: String,
    pub title: String,
    pub author: String,
    pub html_url: String,
    pub status: String,
    pub attempts: i64,
    pub error: Option<String>,
    pub created_at_ms: i64,
}

/// A pull request at a specific commit, ready to be queued.
pub(crate) struct NewJob<'a> {
    pub repo: &'a str,
    pub pr_number: u64,
    pub head_sha: &'a str,
    pub title: &'a str,
    pub author: &'a str,
    pub html_url: &'a str,
}

type JobRow = (
    i64,
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<String>,
    i64,
);

#[derive(Clone)]
pub(crate) struct Jobs {
    db: SqlitePool,
}

impl Jobs {
    pub(crate) fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    /// Queues a job unless one already exists for this exact commit. Returns the new job's id.
    pub(crate) async fn enqueue(&self, job: &NewJob<'_>) -> anyhow::Result<Option<i64>> {
        let now = events::now_ms();
        let result = sqlx::query(
            "INSERT INTO jobs (repo, pr_number, head_sha, title, author, html_url,
                               next_run_at_ms, created_at_ms, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (repo, pr_number, head_sha) DO NOTHING",
        )
        .bind(job.repo)
        .bind(job.pr_number as i64)
        .bind(job.head_sha)
        .bind(job.title)
        .bind(job.author)
        .bind(job.html_url)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&self.db)
        .await?;
        Ok((result.rows_affected() == 1).then(|| result.last_insert_rowid()))
    }

    pub(crate) async fn exists(
        &self,
        repo: &str,
        pr_number: u64,
        head_sha: &str,
    ) -> anyhow::Result<bool> {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM jobs WHERE repo = ? AND pr_number = ? AND head_sha = ?",
        )
        .bind(repo)
        .bind(pr_number as i64)
        .bind(head_sha)
        .fetch_optional(&self.db)
        .await?;
        Ok(found.is_some())
    }

    pub(crate) async fn list(&self) -> anyhow::Result<Vec<Job>> {
        let rows: Vec<JobRow> = sqlx::query_as(
            "SELECT id, repo, pr_number, head_sha, title, author, html_url, status, attempts,
                        error, created_at_ms
                 FROM jobs ORDER BY id DESC LIMIT 500",
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| Job {
                id: r.0,
                repo: r.1,
                pr_number: r.2 as u64,
                head_sha: r.3,
                title: r.4,
                author: r.5,
                html_url: r.6,
                status: r.7,
                attempts: r.8,
                error: r.9,
                created_at_ms: r.10,
            })
            .collect())
    }
}
