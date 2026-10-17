//! The Feed tab: daemon events as readable one-liners, live.

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent};
use prowld::Event;

use crate::app::Cmd;

const KEEP: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Good,
    Warn,
    Bad,
    Dim,
}

#[derive(Debug, Clone, Default)]
pub struct FeedTab {
    pub events: VecDeque<Event>,
    pub loaded: bool,
    pub filter: String,
    /// `Some` while typing a filter.
    pub editing: Option<String>,
    pub paused: bool,
    /// Lines scrolled up from the newest, while paused.
    pub scroll: usize,
    /// Routine "polled GitHub" lines are hidden unless asked for.
    pub show_polls: bool,
}

impl FeedTab {
    pub fn captures_keys(&self) -> bool {
        self.editing.is_some()
    }

    /// Merges the backlog fetched on first view with whatever arrived live meanwhile.
    pub fn on_recent(&mut self, recent: Vec<Event>) {
        let live: Vec<Event> = self.events.drain(..).collect();
        let last_recent = recent.last().map_or(0, |e| e.id);
        self.events.extend(recent);
        self.events
            .extend(live.into_iter().filter(|e| e.id > last_recent));
        self.trim();
        self.loaded = true;
    }

    pub fn push(&mut self, event: Event) {
        if self.events.back().is_some_and(|last| last.id >= event.id) {
            return;
        }
        self.events.push_back(event);
        if self.paused {
            // Keep the view still while the user reads.
            self.scroll += 1;
        }
        self.trim();
    }

    fn trim(&mut self) {
        while self.events.len() > KEEP {
            self.events.pop_front();
        }
    }

    /// Events that pass the filter and the poll toggle, oldest first.
    pub fn visible(&self) -> Vec<&Event> {
        let needle = self.filter.to_lowercase();
        self.events
            .iter()
            .filter(|e| self.show_polls || e.kind != "poll.completed")
            .filter(|e| {
                needle.is_empty()
                    || e.kind.to_lowercase().contains(&needle)
                    || describe(e).1.to_lowercase().contains(&needle)
                    || subject(e).is_some_and(|s| s.to_lowercase().contains(&needle))
            })
            .collect()
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if let Some(text) = &mut self.editing {
            match key.code {
                KeyCode::Esc => self.editing = None,
                KeyCode::Enter => {
                    self.filter = self.editing.take().unwrap_or_default().trim().to_owned();
                    self.scroll = 0;
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) => text.push(c),
                _ => {}
            }
            return vec![];
        }
        match key.code {
            KeyCode::Char(' ') => {
                self.paused = !self.paused;
                if !self.paused {
                    self.scroll = 0;
                }
            }
            KeyCode::Char('f') | KeyCode::Char('/') => self.editing = Some(self.filter.clone()),
            KeyCode::Char('c') => {
                self.filter.clear();
                self.scroll = 0;
            }
            KeyCode::Char('p') => self.show_polls = !self.show_polls,
            KeyCode::Up | KeyCode::Char('k') => {
                self.paused = true;
                self.scroll = (self.scroll + 1).min(self.visible().len().saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.paused = false;
                self.scroll = 0;
            }
            _ => {}
        }
        vec![]
    }
}

/// `owner/repo#n` for events about a PR.
pub fn subject(event: &Event) -> Option<String> {
    let repo = event.payload.get("repo")?.as_str()?;
    match event.payload.get("pr").and_then(|p| p.as_u64()) {
        Some(n) => Some(format!("{repo}#{n}")),
        None => Some(repo.to_owned()),
    }
}

/// A readable line for an event, and how to colour it.
pub fn describe(event: &Event) -> (Tone, String) {
    let p = &event.payload;
    let s = |key: &str| p.get(key).and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let n = |key: &str| p.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let words = |key: &str| s(key).replace('_', " ");
    use Tone::*;
    match event.kind.as_str() {
        "daemon.started" => (Dim, format!("prowld {} started", s("version"))),
        "update.available" => (
            Warn,
            format!(
                "PRowl {} is available (you have {}); rerun the installer to update: {}",
                s("latest"),
                s("current"),
                s("url")
            ),
        ),
        "auth.connected" => (Good, format!("connected to GitHub as @{}", s("login"))),
        "auth.pending" => (
            Normal,
            format!("waiting for GitHub login (code {})", s("user_code")),
        ),
        "auth.failed" => (Bad, format!("GitHub login failed: {}", s("reason"))),
        "auth.disconnected" => (Warn, "GitHub account disconnected".into()),
        "auth.reconnect_required" => (
            Bad,
            "GitHub rejected the token; reconnect in the app".into(),
        ),
        "repo.watched" => (Normal, "now watching this repository".into()),
        "repo.unwatched" => (Normal, "stopped watching this repository".into()),
        "poll.completed" => (
            Dim,
            format!(
                "checked GitHub: {} review requests, {} newly queued",
                n("candidates"),
                n("enqueued")
            ),
        ),
        "poll.failed" => (Bad, format!("checking GitHub failed: {}", s("error"))),
        "poll.rate_limited" => {
            let limit = match s("resource").as_str() {
                "" | "core" => "GitHub API limit".to_owned(),
                resource => format!("GitHub {resource} limit"),
            };
            let resets = event.payload["reset_at"]
                .as_i64()
                .map(|at| format!(" (in {})", duration(at * 1000 - event.at_ms)))
                .unwrap_or_default();
            (
                Warn,
                format!("{limit} nearly used up; pausing until it resets{resets}"),
            )
        }
        "pr.enqueued" => (Normal, format!("queued for review: {}", s("title"))),
        "pr.gated_out" => (Dim, format!("not reviewing: {}", words("reason"))),
        "review.started" => (Normal, "reviewing with Claude…".into()),
        "review.tool" => (
            Dim,
            format!("  {} {}", s("tool").to_lowercase(), s("target")),
        ),
        "review.output" => (
            Dim,
            format!("  {}", s("text").lines().next().unwrap_or("").trim()),
        ),
        "review.finished" if p.get("valid").and_then(|v| v.as_bool()) == Some(true) => (
            Normal,
            format!(
                "review done: {} severity, risk {}{}",
                s("severity"),
                n("risk_score"),
                cost(p.get("cost_usd"))
            ),
        ),
        "review.finished" => (
            Warn,
            format!(
                "the review was unusable: {}{}",
                s("invalid_reason"),
                cost(p.get("cost_usd"))
            ),
        ),
        "review.decided" if s("decision") == "AGENT_ONLY" => {
            (Good, "the agent's review is enough".into())
        }
        "review.decided" => (Warn, format!("needs a human: {}", s("summary"))),
        "review.published" => (Good, format!("posted {}", s("url"))),
        "review.retrying" => (
            Warn,
            format!("will retry (attempt {}): {}", n("attempt"), s("error")),
        ),
        "review.failed" => (Bad, format!("review failed: {}", s("error"))),
        "job.cancelled" => (Dim, format!("review cancelled: {}", words("reason"))),
        "claude.missing" => (Bad, "the claude CLI was not found".into()),
        "queue.added" => (Warn, "waiting for your review".into()),
        "queue.resolved" => (
            Good,
            format!("left your queue: {} (via {})", words("status"), s("via")),
        ),
        "power.awake_held" => (
            Dim,
            format!("keeping the machine awake ({})", words("reason")),
        ),
        "power.awake_released" => (
            Dim,
            format!("letting the machine sleep ({})", words("reason")),
        ),
        "power.woke" => {
            let ms = p.get("asleep_to_ms").and_then(|v| v.as_i64()).unwrap_or(0)
                - p.get("asleep_from_ms")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
            (
                Warn,
                format!(
                    "the machine slept for {}; caught up on {} PRs",
                    duration(ms),
                    n("caught_up")
                ),
            )
        }
        "power.lid_closed" => (Warn, s("message")),
        "power.unavailable" => (
            Warn,
            format!("can't keep the machine awake: {}", s("error")),
        ),
        "budget.reached" => (
            Warn,
            format!(
                "daily Claude budget reached (${:.2} of ${:.2}); reviews resume tomorrow",
                p.get("spent_usd").and_then(|v| v.as_f64()).unwrap_or(0.0),
                p.get("daily_budget_usd")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
            ),
        ),
        "policy.changed" => (
            Normal,
            format!(
                "policy {} updated to v{} ({})",
                s("scope"),
                n("version_id"),
                s("source")
            ),
        ),
        other => (Dim, other.to_owned()),
    }
}

fn cost(value: Option<&serde_json::Value>) -> String {
    value
        .and_then(|v| v.as_f64())
        .map(|c| format!(", ${c:.2}"))
        .unwrap_or_default()
}

pub fn duration(ms: i64) -> String {
    let mins = ms / 60_000;
    match mins {
        0 => format!("{}s", (ms / 1000).max(0)),
        1..60 => format!("{mins}m"),
        _ => format!("{}h {}m", mins / 60, mins % 60),
    }
}

/// `HH:MM:SS` (UTC) for a Unix-milliseconds timestamp.
pub fn clock(at_ms: i64) -> String {
    let secs = at_ms.div_euclid(1000).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[test]
    fn rate_limit_pauses_name_the_limit_and_when_it_resets() {
        let event = |payload: serde_json::Value| Event {
            id: 1,
            kind: "poll.rate_limited".into(),
            payload,
            at_ms: 1_000_000,
        };
        let (_, text) = describe(&event(serde_json::json!({
            "remaining": 2, "reset_at": 1_045, "resource": "search"
        })));
        assert_eq!(
            text,
            "GitHub search limit nearly used up; pausing until it resets (in 45s)"
        );
        let (_, text) = describe(&event(
            serde_json::json!({ "remaining": 10, "reset_at": 4_600 }),
        ));
        assert_eq!(
            text,
            "GitHub API limit nearly used up; pausing until it resets (in 1h 0m)"
        );
    }

    fn event(id: i64, kind: &str, payload: serde_json::Value) -> Event {
        Event {
            id,
            kind: kind.into(),
            payload,
            at_ms: 0,
        }
    }

    #[test]
    fn backlog_and_live_events_merge_without_duplicates() {
        let mut feed = FeedTab::default();
        feed.push(event(5, "queue.added", serde_json::json!({})));
        feed.on_recent(vec![
            event(4, "repo.watched", serde_json::json!({})),
            event(5, "queue.added", serde_json::json!({})),
        ]);
        feed.push(event(5, "queue.added", serde_json::json!({})));
        feed.push(event(6, "queue.resolved", serde_json::json!({})));
        let ids: Vec<i64> = feed.events.iter().map(|e| e.id).collect();
        assert_eq!(ids, [4, 5, 6]);
    }

    #[test]
    fn polls_are_hidden_until_asked_and_filters_match_prs() {
        let mut feed = FeedTab::default();
        feed.on_recent(vec![
            event(
                1,
                "poll.completed",
                serde_json::json!({ "candidates": 1, "enqueued": 0 }),
            ),
            event(
                2,
                "pr.enqueued",
                serde_json::json!({ "repo": "acme/api", "pr": 7, "title": "x" }),
            ),
            event(
                3,
                "pr.enqueued",
                serde_json::json!({ "repo": "acme/web", "pr": 9, "title": "y" }),
            ),
        ]);
        assert_eq!(feed.visible().len(), 2);
        feed.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(feed.visible().len(), 3);
        feed.filter = "api#7".into();
        assert_eq!(feed.visible().len(), 1);
    }

    #[test]
    fn readable_descriptions() {
        let e = event(
            1,
            "pr.gated_out",
            serde_json::json!({ "reason": "own_pull_request" }),
        );
        assert_eq!(describe(&e).1, "not reviewing: own pull request");
        assert_eq!(clock(3_723_000), "01:02:03");
        assert_eq!(duration(8 * 3600 * 1000 + 5 * 60_000), "8h 5m");
    }
}
