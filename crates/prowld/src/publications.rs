//! What PRowl has already posted on each pull request.

use sqlx::SqlitePool;

use crate::decider::Decision;
use crate::events;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Prior {
    pub comment: Option<(i64, String)>,
    pub agent_review: Option<(i64, String)>,
    pub last_decision: Option<Decision>,
}

#[derive(Clone)]
pub(crate) struct Publications {
    db: SqlitePool,
}

type Row = (
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
);

impl Publications {
    pub(crate) fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub(crate) async fn prior(&self, repo: &str, pr_number: u64) -> anyhow::Result<Prior> {
        let row: Option<Row> = sqlx::query_as(
            "SELECT comment_id, comment_url, agent_review_id, agent_review_url, last_decision
             FROM pr_publications WHERE repo = ? AND pr_number = ?",
        )
        .bind(repo)
        .bind(pr_number as i64)
        .fetch_optional(&self.db)
        .await?;
        let Some((cid, curl, rid, rurl, decision)) = row else {
            return Ok(Prior::default());
        };
        Ok(Prior {
            comment: cid.zip(curl),
            agent_review: rid.zip(rurl),
            last_decision: decision
                .map(|d| serde_json::from_value(d.into()))
                .transpose()?,
        })
    }

    pub(crate) async fn save(
        &self,
        repo: &str,
        pr_number: u64,
        head_sha: &str,
        decision: Decision,
        prior: &Prior,
    ) -> anyhow::Result<()> {
        let decision = serde_json::to_value(decision)?;
        sqlx::query(
            "INSERT INTO pr_publications (repo, pr_number, comment_id, comment_url, agent_review_id,
                                          agent_review_url, last_decision, last_head_sha, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (repo, pr_number) DO UPDATE SET
                comment_id = excluded.comment_id, comment_url = excluded.comment_url,
                agent_review_id = excluded.agent_review_id, agent_review_url = excluded.agent_review_url,
                last_decision = excluded.last_decision, last_head_sha = excluded.last_head_sha,
                updated_at_ms = excluded.updated_at_ms",
        )
        .bind(repo)
        .bind(pr_number as i64)
        .bind(prior.comment.as_ref().map(|c| c.0))
        .bind(prior.comment.as_ref().map(|c| c.1.as_str()))
        .bind(prior.agent_review.as_ref().map(|r| r.0))
        .bind(prior.agent_review.as_ref().map(|r| r.1.as_str()))
        .bind(decision.as_str())
        .bind(head_sha)
        .bind(events::now_ms())
        .execute(&self.db)
        .await?;
        Ok(())
    }
}
