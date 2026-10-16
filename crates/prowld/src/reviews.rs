//! Stored reviews.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::decider::{Decision, Reason};
use crate::events;
use crate::review::ReviewOutput;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    pub id: i64,
    pub job_id: i64,
    pub policy_scope: String,
    pub policy_version_id: i64,
    pub valid: bool,
    pub output: Option<ReviewOutput>,
    pub raw_answer: String,
    pub invalid_reason: Option<String>,
    pub cost_usd: Option<f64>,
    pub duration_ms: i64,
    pub created_at_ms: i64,
    /// `None` until decided.
    pub decision: Option<Decision>,
    #[serde(default)]
    pub decision_reasons: Vec<Reason>,
    pub published_url: Option<String>,
}

pub(crate) struct NewReview<'a> {
    pub job_id: i64,
    pub policy_scope: &'a str,
    pub policy_version_id: i64,
    pub output: Result<&'a ReviewOutput, &'a str>,
    pub raw_answer: &'a str,
    pub cost_usd: Option<f64>,
    pub duration_ms: i64,
}

type ReviewRow = (
    i64,
    i64,
    String,
    i64,
    bool,
    Option<String>,
    String,
    Option<String>,
    Option<f64>,
    i64,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
);

#[derive(Clone)]
pub(crate) struct Reviews {
    db: SqlitePool,
}

impl Reviews {
    pub(crate) fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub(crate) async fn insert(&self, review: &NewReview<'_>) -> anyhow::Result<i64> {
        let (valid, output, reason) = match review.output {
            Ok(output) => (true, Some(serde_json::to_string(output)?), None),
            Err(reason) => (false, None, Some(reason)),
        };
        Ok(sqlx::query(
            "INSERT INTO reviews (job_id, policy_scope, policy_version_id, valid, output, raw_answer,
                                  invalid_reason, cost_usd, duration_ms, created_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(review.job_id)
        .bind(review.policy_scope)
        .bind(review.policy_version_id)
        .bind(valid)
        .bind(output)
        .bind(review.raw_answer)
        .bind(reason)
        .bind(review.cost_usd)
        .bind(review.duration_ms)
        .bind(events::now_ms())
        .execute(&self.db)
        .await?
        .last_insert_rowid())
    }

    pub(crate) async fn set_decision(
        &self,
        id: i64,
        decision: Decision,
        reasons: &[Reason],
    ) -> anyhow::Result<()> {
        let decision = serde_json::to_value(decision)?;
        sqlx::query("UPDATE reviews SET decision = ?, decision_reasons = ? WHERE id = ?")
            .bind(decision.as_str())
            .bind(serde_json::to_string(reasons)?)
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub(crate) async fn set_published(
        &self,
        id: i64,
        github_review_id: i64,
        url: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE reviews SET github_review_id = ?, published_url = ?, published_at_ms = ? WHERE id = ?",
        )
        .bind(github_review_id)
        .bind(url)
        .bind(events::now_ms())
        .bind(id)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Claude spend recorded since `since_ms` (reviews and dry runs), and how many runs.
    pub(crate) async fn spent_since(&self, since_ms: i64) -> anyhow::Result<(f64, i64)> {
        Ok(sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0.0), COUNT(*) FROM (
                SELECT cost_usd FROM reviews WHERE created_at_ms >= ? AND cost_usd IS NOT NULL
                UNION ALL
                SELECT cost_usd FROM dry_runs WHERE created_at_ms >= ? AND cost_usd IS NOT NULL
             )",
        )
        .bind(since_ms)
        .bind(since_ms)
        .fetch_one(&self.db)
        .await?)
    }

    pub(crate) async fn record_dry_run(
        &self,
        repo: &str,
        pr_number: u64,
        cost_usd: Option<f64>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO dry_runs (repo, pr_number, cost_usd, created_at_ms) VALUES (?, ?, ?, ?)",
        )
        .bind(repo)
        .bind(pr_number as i64)
        .bind(cost_usd)
        .bind(events::now_ms())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub(crate) async fn list(&self, job_id: Option<i64>) -> anyhow::Result<Vec<Review>> {
        let rows: Vec<ReviewRow> = sqlx::query_as(
            "SELECT id, job_id, policy_scope, policy_version_id, valid, output, raw_answer,
                    invalid_reason, cost_usd, duration_ms, created_at_ms, decision,
                    decision_reasons, published_url
             FROM reviews WHERE (? IS NULL OR job_id = ?) ORDER BY id DESC LIMIT 500",
        )
        .bind(job_id)
        .bind(job_id)
        .fetch_all(&self.db)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(Review {
                    id: r.0,
                    job_id: r.1,
                    policy_scope: r.2,
                    policy_version_id: r.3,
                    valid: r.4,
                    output: r.5.as_deref().map(serde_json::from_str).transpose()?,
                    raw_answer: r.6,
                    invalid_reason: r.7,
                    cost_usd: r.8,
                    duration_ms: r.9,
                    created_at_ms: r.10,
                    decision: r
                        .11
                        .as_deref()
                        .map(|d| serde_json::from_value(d.into()))
                        .transpose()?,
                    decision_reasons: r
                        .12
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?
                        .unwrap_or_default(),
                    published_url: r.13,
                })
            })
            .collect()
    }
}
