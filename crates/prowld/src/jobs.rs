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
    /// `review` (found by polling), `rerun` (asked for) or `verify`.
    #[serde(default = "review_kind")]
    pub kind: String,
}

fn review_kind() -> String {
    "review".into()
}

pub mod kinds {
    pub const REVIEW: &str = "review";
    pub const RERUN: &str = "rerun";
    pub const VERIFY: &str = "verify";
}

impl Job {
    /// Asked for by the user rather than found by polling: it runs even if PRowl is no longer
    /// a requested reviewer (posting a review clears the request).
    pub fn manual(&self) -> bool {
        self.kind != kinds::REVIEW
    }
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
    String,
);

fn from_row(r: JobRow) -> Job {
    Job {
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
        kind: r.11,
    }
}

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
             ON CONFLICT (repo, pr_number, head_sha) WHERE kind = 'review' DO NOTHING",
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

    /// Queues a job the user asked for (`kinds::RERUN` or `kinds::VERIFY`), even if this
    /// commit was reviewed before. Returns the new job's id.
    pub(crate) async fn enqueue_manual(&self, job: &NewJob<'_>, kind: &str) -> anyhow::Result<i64> {
        let now = events::now_ms();
        Ok(sqlx::query(
            "INSERT INTO jobs (repo, pr_number, head_sha, title, author, html_url,
                               next_run_at_ms, created_at_ms, updated_at_ms, kind)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(kind)
        .execute(&self.db)
        .await?
        .last_insert_rowid())
    }

    /// Whether a job of `kind` exists for this exact commit, in any state.
    pub(crate) async fn exists_of_kind(
        &self,
        repo: &str,
        pr_number: u64,
        head_sha: &str,
        kind: &str,
    ) -> anyhow::Result<bool> {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM jobs WHERE repo = ? AND pr_number = ? AND head_sha = ? AND kind = ?",
        )
        .bind(repo)
        .bind(pr_number as i64)
        .bind(head_sha)
        .bind(kind)
        .fetch_optional(&self.db)
        .await?;
        Ok(found.is_some())
    }

    /// When the newest job for this pull request was queued, and whether any of its jobs is
    /// still to run. `None` when PRowl never queued one.
    pub(crate) async fn latest_for_pr(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> anyhow::Result<Option<(i64, bool)>> {
        let (latest, waiting): (Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT MAX(created_at_ms), MAX(status IN ('queued', 'running', 'interrupted'))
             FROM jobs WHERE repo = ? AND pr_number = ?",
        )
        .bind(repo)
        .bind(pr_number as i64)
        .fetch_one(&self.db)
        .await?;
        Ok(latest.map(|at| (at, waiting == Some(1))))
    }

    /// Pull requests whose latest PRowl result was agent-only and posted as a review — the
    /// ones whose findings' threads PRowl watches.
    pub(crate) async fn agent_reviewed_prs(&self) -> anyhow::Result<Vec<(String, u64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT repo, pr_number FROM pr_publications
             WHERE last_decision = 'AGENT_ONLY' AND agent_review_id IS NOT NULL
             ORDER BY updated_at_ms DESC LIMIT 200",
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(|(r, n)| (r, n as u64)).collect())
    }

    /// Cancels a job that hasn't started. Returns whether it was waiting.
    pub(crate) async fn cancel_waiting(&self, id: i64, reason: &str) -> anyhow::Result<bool> {
        let done = sqlx::query(
            "UPDATE jobs SET status = 'cancelled', error = ?, updated_at_ms = ?
             WHERE id = ? AND status IN ('queued', 'interrupted')",
        )
        .bind(reason)
        .bind(events::now_ms())
        .bind(id)
        .execute(&self.db)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    pub(crate) async fn get(&self, id: i64) -> anyhow::Result<Option<Job>> {
        let row: Option<JobRow> = sqlx::query_as(
            "SELECT id, repo, pr_number, head_sha, title, author, html_url, status, attempts,
                    error, created_at_ms, kind
             FROM jobs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(from_row))
    }

    pub(crate) async fn exists(
        &self,
        repo: &str,
        pr_number: u64,
        head_sha: &str,
    ) -> anyhow::Result<bool> {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM jobs WHERE repo = ? AND pr_number = ? AND head_sha = ? AND kind = 'review'",
        )
        .bind(repo)
        .bind(pr_number as i64)
        .bind(head_sha)
        .fetch_optional(&self.db)
        .await?;
        Ok(found.is_some())
    }

    /// Atomically takes the oldest runnable job and marks it running.
    pub(crate) async fn claim_next(&self) -> anyhow::Result<Option<Job>> {
        let now = events::now_ms();
        let row: Option<JobRow> = sqlx::query_as(
            "UPDATE jobs SET status = 'running', attempts = attempts + 1, updated_at_ms = ?
             WHERE id = (SELECT id FROM jobs WHERE status IN ('queued', 'interrupted')
                         AND next_run_at_ms <= ? ORDER BY id LIMIT 1)
             RETURNING id, repo, pr_number, head_sha, title, author, html_url, status, attempts,
                       error, created_at_ms, kind",
        )
        .bind(now)
        .bind(now)
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(from_row))
    }

    pub(crate) async fn finish(
        &self,
        id: i64,
        status: &str,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE jobs SET status = ?, error = ?, updated_at_ms = ? WHERE id = ?")
            .bind(status)
            .bind(error)
            .bind(events::now_ms())
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Puts a running job back in the queue to try again after `delay`.
    pub(crate) async fn defer(
        &self,
        id: i64,
        delay: std::time::Duration,
        reason: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE jobs SET status = 'queued', error = ?, next_run_at_ms = ?, updated_at_ms = ?
             WHERE id = ?",
        )
        .bind(reason)
        .bind(events::now_ms() + delay.as_millis() as i64)
        .bind(events::now_ms())
        .bind(id)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Like [`defer`](Self::defer), without counting the run as an attempt.
    pub(crate) async fn defer_without_attempt(
        &self,
        id: i64,
        delay: std::time::Duration,
        reason: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE jobs SET attempts = MAX(attempts - 1, 0) WHERE id = ?")
            .bind(id)
            .execute(&self.db)
            .await?;
        self.defer(id, delay, reason).await
    }

    /// Jobs left running by a crash or shutdown become runnable again.
    pub(crate) async fn mark_interrupted(&self) -> anyhow::Result<u64> {
        Ok(sqlx::query(
            "UPDATE jobs SET status = 'interrupted', updated_at_ms = ? WHERE status = 'running'",
        )
        .bind(events::now_ms())
        .execute(&self.db)
        .await?
        .rows_affected())
    }

    pub(crate) async fn list(&self) -> anyhow::Result<Vec<Job>> {
        let rows: Vec<JobRow> = sqlx::query_as(
            "SELECT id, repo, pr_number, head_sha, title, author, html_url, status, attempts,
                        error, created_at_ms, kind
                 FROM jobs ORDER BY id DESC LIMIT 500",
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(from_row).collect())
    }
}
