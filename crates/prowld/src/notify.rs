//! Desktop notifications, sent by the daemon itself so the user hears about PRs that need them
//! even with no UI open. On machines without a desktop session they are silently skipped.

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

use crate::events::{Event, EventBus};

pub trait Notifier: Send + Sync + 'static {
    fn notify(&self, title: &str, body: &str) -> anyhow::Result<()>;
}

/// A [`Notifier`] handed to [`Config`](crate::Config), e.g. a recorder in tests.
#[derive(Clone)]
pub struct NotifierHook(pub Arc<dyn Notifier>);

impl std::fmt::Debug for NotifierHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NotifierHook(..)")
    }
}

/// The OS notification service (Notification Center, D-Bus, Windows toasts).
pub(crate) struct Desktop;

impl Notifier for Desktop {
    fn notify(&self, title: &str, body: &str) -> anyhow::Result<()> {
        notify_rust::Notification::new()
            .appname("PRowl")
            .summary(title)
            .body(body)
            .show()?;
        Ok(())
    }
}

/// What to tell the user about an event, if anything.
pub(crate) fn message(event: &Event) -> Option<(String, String)> {
    let p = &event.payload;
    let s = |key: &str| p.get(key).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let pr = || match p.get("pr").and_then(|v| v.as_u64()) {
        Some(n) => format!("{}#{n}", s("repo")),
        None => s("repo"),
    };
    match event.kind.as_str() {
        "queue.added" => Some((
            "A pull request needs your review".into(),
            match s("title") {
                t if t.is_empty() => pr(),
                t => format!("{} — {t}", pr()),
            },
        )),
        "review.failed" => Some((format!("PRowl couldn't review {}", pr()), s("error"))),
        "auth.reconnect_required" => Some((
            "PRowl needs you to reconnect GitHub".into(),
            "GitHub no longer accepts PRowl's token. Open prowl to reconnect.".into(),
        )),
        "budget.reached" => Some((
            "PRowl reached today's Claude budget".into(),
            "New reviews will start again tomorrow.".into(),
        )),
        _ => None,
    }
}

pub(crate) fn spawn(
    events: &EventBus,
    notifier: Arc<dyn Notifier>,
    settings: crate::settings::SettingsStore,
    shutdown: CancellationToken,
) {
    let mut rx = events.subscribe();
    tokio::spawn(async move {
        let mut warned = false;
        loop {
            let event = tokio::select! {
                event = rx.recv() => event,
                () = shutdown.cancelled() => return,
            };
            let event = match event {
                Ok(event) => event,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            if !settings.get().notifications {
                continue;
            }
            let Some((title, body)) = message(&event) else {
                continue;
            };
            let notifier = notifier.clone();
            let result = tokio::task::spawn_blocking(move || notifier.notify(&title, &body)).await;
            if let Ok(Err(e)) = result
                && !warned
            {
                // Typical on headless servers; say so once.
                warned = true;
                tracing::info!(error = %e, "desktop notifications unavailable");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_additions_name_the_pr() {
        let event = Event {
            id: 1,
            kind: "queue.added".into(),
            payload: serde_json::json!({ "repo": "acme/api", "pr": 7, "title": "Rate limit login" }),
            at_ms: 0,
        };
        assert_eq!(
            message(&event),
            Some((
                "A pull request needs your review".into(),
                "acme/api#7 — Rate limit login".into()
            ))
        );
        let quiet = Event {
            kind: "review.started".into(),
            ..event
        };
        assert_eq!(message(&quiet), None);
    }
}
