use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::broadcast;

/// Something the daemon did, persisted and streamed to UI clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub at_ms: i64,
}

pub mod kind {
    pub const DAEMON_STARTED: &str = "daemon.started";
}

#[derive(Clone)]
pub(crate) struct EventBus {
    db: SqlitePool,
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub(crate) fn new(db: SqlitePool) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { db, tx }
    }

    pub(crate) async fn emit(
        &self,
        kind: &str,
        payload: serde_json::Value,
    ) -> anyhow::Result<Event> {
        let at_ms = now_ms();
        let id = sqlx::query("INSERT INTO events (kind, payload, at_ms) VALUES (?, ?, ?)")
            .bind(kind)
            .bind(payload.to_string())
            .bind(at_ms)
            .execute(&self.db)
            .await?
            .last_insert_rowid();
        let event = Event {
            id,
            kind: kind.to_owned(),
            payload,
            at_ms,
        };
        // No subscribers is fine; persisted events are replayed on connect.
        let _ = self.tx.send(event.clone());
        Ok(event)
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// The most recent `limit` events, oldest first.
    pub(crate) async fn recent(&self, limit: i64) -> anyhow::Result<Vec<Event>> {
        let latest = self.latest_id().await?;
        let mut events = self.since((latest - limit).max(0)).await?;
        events.truncate(limit as usize);
        Ok(events)
    }

    pub(crate) async fn latest_id(&self) -> anyhow::Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM events")
                .fetch_one(&self.db)
                .await?,
        )
    }

    pub(crate) async fn since(&self, after_id: i64) -> anyhow::Result<Vec<Event>> {
        let rows: Vec<(i64, String, String, i64)> = sqlx::query_as(
            "SELECT id, kind, payload, at_ms FROM events WHERE id > ? ORDER BY id LIMIT 1000",
        )
        .bind(after_id)
        .fetch_all(&self.db)
        .await?;
        rows.into_iter()
            .map(|(id, kind, payload, at_ms)| {
                Ok(Event {
                    id,
                    kind,
                    payload: serde_json::from_str(&payload)?,
                    at_ms,
                })
            })
            .collect()
    }
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}
