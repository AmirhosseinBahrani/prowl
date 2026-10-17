//! The transcript of each review run: the context Claude was given, its messages and tool calls,
//! the result and what was posted — so the user can see exactly what happened.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::events;

/// Longest text kept for one entry; tool results (whole files) are cut to this.
const MAX_TEXT: usize = 16_000;

pub mod kind {
    /// The first message and everything around it: `prompt`, `model`, `material_files`, policy.
    pub const CONTEXT: &str = "context";
    /// A follow-up asking Claude to fix an unusable answer: `prompt`.
    pub const REPAIR: &str = "repair";
    /// Claude's text: `text`.
    pub const ASSISTANT: &str = "assistant";
    /// A tool Claude called: `id`, `name`, `input`.
    pub const TOOL_USE: &str = "tool_use";
    /// What the tool returned: `id`, `content`, `is_error`, `truncated`.
    pub const TOOL_RESULT: &str = "tool_result";
    /// The end of a Claude run: `is_error`, `cost_usd`, `duration_ms`, `output` (when valid),
    /// `invalid_reason`.
    pub const RESULT: &str = "result";
    /// PRowl's decision: `decision`, `reasons`.
    pub const DECISION: &str = "decision";
    /// What went to GitHub: `as` (`review` / `comment`), `event`, `url`, `body`.
    pub const PUBLISHED: &str = "published";
    /// The run failed: `message`.
    pub const ERROR: &str = "error";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub id: i64,
    pub job_id: i64,
    /// Which try of the job this belongs to (retries start a new attempt).
    pub attempt: i64,
    pub at_ms: i64,
    pub kind: String,
    pub content: serde_json::Value,
}

#[derive(Clone)]
pub(crate) struct Transcripts {
    db: SqlitePool,
}

impl Transcripts {
    pub(crate) fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    /// Records one entry. Failures are logged, never fatal: a transcript is for looking at.
    pub(crate) async fn append(
        &self,
        job_id: i64,
        attempt: i64,
        kind: &str,
        mut content: serde_json::Value,
    ) {
        truncate_strings(&mut content);
        let result = sqlx::query(
            "INSERT INTO review_transcripts (job_id, attempt, at_ms, kind, content)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(job_id)
        .bind(attempt)
        .bind(events::now_ms())
        .bind(kind)
        .bind(content.to_string())
        .execute(&self.db)
        .await;
        if let Err(e) = result {
            tracing::warn!(job = job_id, error = %e, "could not record transcript entry");
        }
    }

    /// The whole transcript of a job, oldest first.
    pub(crate) async fn list(&self, job_id: i64) -> anyhow::Result<Vec<TranscriptEntry>> {
        let rows: Vec<(i64, i64, i64, i64, String, String)> = sqlx::query_as(
            "SELECT id, job_id, attempt, at_ms, kind, content FROM review_transcripts
             WHERE job_id = ? ORDER BY id",
        )
        .bind(job_id)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, job_id, attempt, at_ms, kind, content)| TranscriptEntry {
                    id,
                    job_id,
                    attempt,
                    at_ms,
                    kind,
                    content: serde_json::from_str(&content).unwrap_or(serde_json::Value::Null),
                },
            )
            .collect())
    }
}

/// Cuts every long string in `value`, marking it so the reader knows.
fn truncate_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) if s.len() > MAX_TEXT => {
            let mut end = MAX_TEXT;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            let dropped = s.len() - end;
            s.truncate(end);
            s.push_str(&format!("\n… ({dropped} more bytes not kept)"));
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(truncate_strings),
        serde_json::Value::Object(map) => map.values_mut().for_each(truncate_strings),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        let mut value = serde_json::json!({ "content": "é".repeat(MAX_TEXT), "short": "ok" });
        truncate_strings(&mut value);
        let content = value["content"].as_str().unwrap();
        assert!(content.len() < MAX_TEXT + 64);
        assert!(content.ends_with("more bytes not kept)"));
        assert_eq!(value["short"], "ok");
    }
}
