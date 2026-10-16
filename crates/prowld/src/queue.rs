//! The Human Review Queue: pull requests the policy says a person must review.
//! A PR leaves the queue when the user approves or requests changes — in the app or directly
//! on GitHub — or when it is closed.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::auth::{Auth, AuthState};
use crate::decider::Reason;
use crate::events::{self, EventBus};
use crate::github::{GitHub, GitHubError, HumanVerdict};
use crate::publish::{self, LABEL_CHANGES_REQUESTED, LABEL_HUMAN_APPROVED, LABEL_NEEDS_HUMAN};
use crate::review::{Finding, ReviewOutput, Severity};

pub mod kind {
    pub const ADDED: &str = "queue.added";
    pub const RESOLVED: &str = "queue.resolved";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    pub repo: String,
    pub pr_number: u64,
    pub title: String,
    pub author: String,
    pub html_url: String,
    pub head_sha: String,
    pub review_id: i64,
    /// `pending`, `approved`, `changes_requested`, `dismissed` or `closed`.
    pub status: String,
    pub queued_at_ms: i64,
    /// `None` when the agent's output was unusable.
    pub severity: Option<Severity>,
    pub risk_score: Option<u8>,
    pub summary: Option<String>,
    pub quick_notes: Vec<String>,
    pub findings: Vec<Finding>,
    pub reasons: Vec<Reason>,
    /// The awaiting-approval comment PRowl posted.
    pub comment_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueDetail {
    #[serde(flatten)]
    pub item: QueueItem,
    pub diff: String,
}

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("{0} is not waiting for a human review")]
    NotQueued(String),
    #[error("connect a GitHub account first")]
    NotConnected,
    #[error("a comment is required when requesting changes")]
    CommentRequired,
    #[error("GitHub request failed: {0}")]
    GitHub(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for QueueError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl From<GitHubError> for QueueError {
    fn from(e: GitHubError) -> Self {
        match e {
            GitHubError::Unauthorized => Self::NotConnected,
            other => Self::GitHub(other.to_string()),
        }
    }
}

/// What the user decided in the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Approve,
    RequestChanges,
    Dismiss,
}

#[derive(Clone)]
pub(crate) struct Queue {
    db: SqlitePool,
    github: GitHub,
    auth: Auth,
    events: EventBus,
}

type ItemRow = (
    String,
    i64,
    String,
    String,
    String,
    String,
    i64,
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
);

impl Queue {
    pub(crate) fn new(db: SqlitePool, github: GitHub, auth: Auth, events: EventBus) -> Self {
        Self {
            db,
            github,
            auth,
            events,
        }
    }

    /// Puts a PR in the queue (or refreshes its entry with a newer review).
    pub(crate) async fn add(
        &self,
        repo: &str,
        pr_number: u64,
        head_sha: &str,
        review_id: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO human_queue (repo, pr_number, head_sha, review_id, status, queued_at_ms)
             VALUES (?, ?, ?, ?, 'pending', ?)
             ON CONFLICT (repo, pr_number) DO UPDATE SET
                head_sha = excluded.head_sha, review_id = excluded.review_id, status = 'pending',
                resolved_via = NULL, resolved_at_ms = NULL, queued_at_ms = excluded.queued_at_ms",
        )
        .bind(repo)
        .bind(pr_number as i64)
        .bind(head_sha)
        .bind(review_id)
        .bind(events::now_ms())
        .execute(&self.db)
        .await?;
        self.events
            .emit(
                kind::ADDED,
                serde_json::json!({ "repo": repo, "pr": pr_number, "review_id": review_id }),
            )
            .await?;
        Ok(())
    }

    /// Pending items, riskiest first: unusable reviews, then by severity, then oldest.
    pub(crate) async fn pending(&self) -> Result<Vec<QueueItem>, QueueError> {
        let mut items = self.items("WHERE q.status = 'pending'", None).await?;
        items.sort_by_key(|i| {
            let rank = match i.severity {
                None => 5,
                Some(s) => s as u8,
            };
            (std::cmp::Reverse(rank), i.queued_at_ms)
        });
        Ok(items)
    }

    pub(crate) async fn detail(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> Result<QueueDetail, QueueError> {
        let item = self.pending_item(repo, pr_number).await?;
        let token = self.token().await?;
        let diff = self.github.pull_diff(&token, repo, pr_number).await?;
        Ok(QueueDetail { item, diff })
    }

    /// Applies the user's decision from the app. Approve and request-changes submit the
    /// user's own review on GitHub; dismiss only removes the PR from the queue.
    pub(crate) async fn resolve(
        &self,
        repo: &str,
        pr_number: u64,
        resolution: Resolution,
        comment: &str,
    ) -> Result<QueueItem, QueueError> {
        let item = self.pending_item(repo, pr_number).await?;
        let status = match resolution {
            Resolution::Dismiss => "dismissed",
            Resolution::Approve => "approved",
            Resolution::RequestChanges => {
                if comment.trim().is_empty() {
                    return Err(QueueError::CommentRequired);
                }
                "changes_requested"
            }
        };
        if resolution != Resolution::Dismiss {
            let token = self.token().await?;
            let verdict = if resolution == Resolution::Approve {
                HumanVerdict::Approve
            } else {
                HumanVerdict::RequestChanges
            };
            self.github
                .submit_human_review(&token, repo, pr_number, &item.head_sha, verdict, comment)
                .await?;
            self.relabel(&token, repo, pr_number, status).await?;
        }
        self.mark(repo, pr_number, status, "app").await?;
        Ok(QueueItem {
            status: status.to_owned(),
            ..item
        })
    }

    /// Resolves queued PRs the user reviewed on GitHub directly, or that were closed.
    pub(crate) async fn sync_with_github(&self) -> Result<(), QueueError> {
        let AuthState::Connected { login, .. } = self.auth.state().await else {
            return Ok(());
        };
        let pending = self.items("WHERE q.status = 'pending'", None).await?;
        if pending.is_empty() {
            return Ok(());
        }
        let token = self.token().await?;
        for item in pending {
            let pr = self
                .github
                .pull_request(&token, &item.repo, item.pr_number)
                .await?;
            if pr.state != "open" {
                self.mark(&item.repo, item.pr_number, "closed", "github")
                    .await?;
                continue;
            }
            let reviews = self
                .github
                .list_reviews(&token, &item.repo, item.pr_number)
                .await?;
            let human = reviews.iter().rev().find(|r| {
                r.user
                    .as_ref()
                    .is_some_and(|u| u.login.eq_ignore_ascii_case(&login))
                    && matches!(r.state.as_str(), "APPROVED" | "CHANGES_REQUESTED")
                    && r.submitted_at
                        .as_deref()
                        .and_then(parse_rfc3339_ms)
                        .is_some_and(|at| at >= item.queued_at_ms - 1000)
            });
            if let Some(review) = human {
                let status = if review.state == "APPROVED" {
                    "approved"
                } else {
                    "changes_requested"
                };
                self.relabel(&token, &item.repo, item.pr_number, status)
                    .await?;
                self.mark(&item.repo, item.pr_number, status, "github")
                    .await?;
            }
        }
        Ok(())
    }

    async fn relabel(
        &self,
        token: &str,
        repo: &str,
        pr_number: u64,
        status: &str,
    ) -> Result<(), QueueError> {
        self.github
            .remove_label(token, repo, pr_number, LABEL_NEEDS_HUMAN)
            .await?;
        let label = if status == "approved" {
            LABEL_HUMAN_APPROVED
        } else {
            LABEL_CHANGES_REQUESTED
        };
        publish::add_label(&self.github, token, repo, pr_number, label).await?;
        Ok(())
    }

    async fn mark(
        &self,
        repo: &str,
        pr_number: u64,
        status: &str,
        via: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE human_queue SET status = ?, resolved_via = ?, resolved_at_ms = ?
             WHERE repo = ? AND pr_number = ?",
        )
        .bind(status)
        .bind(via)
        .bind(events::now_ms())
        .bind(repo)
        .bind(pr_number as i64)
        .execute(&self.db)
        .await?;
        self.events
            .emit(
                kind::RESOLVED,
                serde_json::json!({ "repo": repo, "pr": pr_number, "status": status, "via": via }),
            )
            .await?;
        Ok(())
    }

    async fn pending_item(&self, repo: &str, pr_number: u64) -> Result<QueueItem, QueueError> {
        self.items(
            "WHERE q.status = 'pending' AND lower(q.repo) = lower(?) AND q.pr_number = ?",
            Some((repo, pr_number)),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| QueueError::NotQueued(format!("{repo}#{pr_number}")))
    }

    async fn items(
        &self,
        filter: &str,
        key: Option<(&str, u64)>,
    ) -> Result<Vec<QueueItem>, QueueError> {
        let sql = format!(
            "SELECT q.repo, q.pr_number, j.title, j.author, j.html_url, q.head_sha, q.review_id,
                    q.status, q.queued_at_ms, r.output, r.decision_reasons, r.published_url
             FROM human_queue q
             JOIN reviews r ON r.id = q.review_id
             JOIN jobs j ON j.id = r.job_id
             {filter}"
        );
        let mut query = sqlx::query_as::<_, ItemRow>(&sql);
        if let Some((repo, number)) = key {
            query = query.bind(repo).bind(number as i64);
        }
        let rows = query.fetch_all(&self.db).await?;
        rows.into_iter()
            .map(|r| {
                let output: Option<ReviewOutput> =
                    r.9.as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(anyhow::Error::from)?;
                let reasons: Vec<Reason> =
                    r.10.as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(anyhow::Error::from)?
                        .unwrap_or_default();
                Ok(QueueItem {
                    repo: r.0,
                    pr_number: r.1 as u64,
                    title: r.2,
                    author: r.3,
                    html_url: r.4,
                    head_sha: r.5,
                    review_id: r.6,
                    status: r.7,
                    queued_at_ms: r.8,
                    severity: output.as_ref().map(|o| o.severity),
                    risk_score: output.as_ref().map(|o| o.risk_score),
                    summary: output.as_ref().map(|o| o.summary.clone()),
                    quick_notes: output
                        .as_ref()
                        .map(|o| o.quick_notes.clone())
                        .unwrap_or_default(),
                    findings: output.map(|o| o.findings).unwrap_or_default(),
                    reasons,
                    comment_url: r.11,
                })
            })
            .collect()
    }

    async fn token(&self) -> Result<String, QueueError> {
        self.auth.token().await.ok_or(QueueError::NotConnected)
    }
}

/// `2026-10-16T15:04:05Z` (GitHub's format) as Unix milliseconds.
fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let (date, time) = s.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let time = time.split(['+', '.']).next()?;
    let mut t = time.split(':').map(|p| p.parse::<i64>());
    let (hh, mm, ss) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    // Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400) + hh * 3600 + mm * 60 + ss) * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_timestamps() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2000-03-01T00:00:01Z"),
            Some(951_868_801_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-10-16T15:04:05Z"),
            Some(1_792_163_045_000)
        );
        assert_eq!(parse_rfc3339_ms("garbage"), None);
    }
}
