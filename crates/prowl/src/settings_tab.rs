//! The Settings tab: every daemon setting, edited in place and applied live.

use crossterm::event::{KeyCode, KeyEvent};
use prowld::Settings;
use serde_json::{Value, json};

use crate::app::Cmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    /// A number or nothing (empty input clears it).
    OptFloat,
    OptText,
    Bool,
    /// Cycles through these JSON values.
    Choice(&'static [&'static str]),
}

pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: Kind,
}

pub const FIELDS: &[Field] = &[
    Field {
        key: "poll_interval_secs",
        label: "Check GitHub every (seconds)",
        kind: Kind::Int,
    },
    Field {
        key: "review_concurrency",
        label: "Reviews at the same time",
        kind: Kind::Int,
    },
    Field {
        key: "claude_model",
        label: "Claude model (empty: the CLI's default)",
        kind: Kind::OptText,
    },
    Field {
        key: "review_timeout_secs",
        label: "Stop a review after (seconds)",
        kind: Kind::Int,
    },
    Field {
        key: "review_budget_usd",
        label: "Max spend per review (USD)",
        kind: Kind::OptFloat,
    },
    Field {
        key: "daily_budget_usd",
        label: "Daily budget (USD)",
        kind: Kind::OptFloat,
    },
    Field {
        key: "budget_day",
        label: "Budget day starts at midnight",
        kind: Kind::Choice(&["utc", "local"]),
    },
    Field {
        key: "risk_threshold",
        label: "Risk score that needs a human (1–100)",
        kind: Kind::Int,
    },
    Field {
        key: "max_changed_lines",
        label: "Larger PRs skip the agent (lines)",
        kind: Kind::Int,
    },
    Field {
        key: "max_changed_files",
        label: "Larger PRs skip the agent (files)",
        kind: Kind::Int,
    },
    Field {
        key: "sleep_mode",
        label: "Keep the machine awake",
        kind: Kind::Choice(&["default", "always", "ac_only", "during_reviews"]),
    },
    Field {
        key: "skip_drafts",
        label: "Skip draft PRs",
        kind: Kind::Bool,
    },
    Field {
        key: "include_team_requests",
        label: "Review requests made to my teams",
        kind: Kind::Bool,
    },
    Field {
        key: "agent_verdicts",
        label: "Agent approves / requests changes on low-risk PRs",
        kind: Kind::Bool,
    },
    Field {
        key: "notifications",
        label: "Desktop notifications",
        kind: Kind::Bool,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Editing {
    /// Editing field `index` as text.
    Field { index: usize, text: String },
    /// Typing a new file rule: `owner/repo glob`.
    NewRule { text: String },
}

#[derive(Debug, Clone, Default)]
pub struct SettingsTab {
    pub settings: Option<Settings>,
    /// Rows: the fields, then one per file rule.
    pub selected: usize,
    pub editing: Option<Editing>,
    pub error: Option<String>,
    pub saved: Option<String>,
}

impl SettingsTab {
    pub fn captures_keys(&self) -> bool {
        self.editing.is_some()
    }

    /// File rules flattened to `(repo, pattern)` rows.
    pub fn rules(&self) -> Vec<(String, String)> {
        self.settings
            .as_ref()
            .map(|s| {
                s.file_rules
                    .iter()
                    .flat_map(|(repo, patterns)| {
                        patterns.iter().map(move |p| (repo.clone(), p.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn value(&self, key: &str) -> Value {
        self.settings
            .as_ref()
            .and_then(|s| serde_json::to_value(s).ok())
            .and_then(|v| v.get(key).cloned())
            .unwrap_or(Value::Null)
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if let Some(editing) = &mut self.editing {
            let text = match editing {
                Editing::Field { text, .. } | Editing::NewRule { text } => text,
            };
            match key.code {
                KeyCode::Esc => self.editing = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) => text.push(c),
                KeyCode::Enter => {
                    let editing = self.editing.take().expect("editing is set");
                    return self.submit(editing);
                }
                _ => {}
            }
            return vec![];
        }
        self.error = None;
        self.saved = None;
        let rows = FIELDS.len() + self.rules().len();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') if self.selected + 1 < rows => self.selected += 1,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('a') => {
                self.editing = Some(Editing::NewRule {
                    text: String::new(),
                })
            }
            KeyCode::Char('x') | KeyCode::Delete if self.selected >= FIELDS.len() => {
                return self.remove_rule(self.selected - FIELDS.len());
            }
            KeyCode::Enter | KeyCode::Char(' ') if self.selected < FIELDS.len() => {
                let field = &FIELDS[self.selected];
                let current = self.value(field.key);
                match field.kind {
                    Kind::Bool => {
                        return save(json!({ field.key: !current.as_bool().unwrap_or(false) }));
                    }
                    Kind::Choice(options) => {
                        let now = current.as_str().unwrap_or("default");
                        let i = options.iter().position(|o| *o == now).unwrap_or(0);
                        let next = options[(i + 1) % options.len()];
                        let value = if next == "default" {
                            Value::Null
                        } else {
                            json!(next)
                        };
                        return save(json!({ field.key: value }));
                    }
                    _ => {
                        let text = match current {
                            Value::Null => String::new(),
                            Value::String(s) => s,
                            other => other.to_string(),
                        };
                        self.editing = Some(Editing::Field {
                            index: self.selected,
                            text,
                        });
                    }
                }
            }
            _ => {}
        }
        vec![]
    }

    fn submit(&mut self, editing: Editing) -> Vec<Cmd> {
        match editing {
            Editing::Field { index, text } => {
                let field = &FIELDS[index];
                let text = text.trim();
                let value = match field.kind {
                    Kind::Int => match text.parse::<u64>() {
                        Ok(n) => json!(n),
                        Err(_) => {
                            return self.fail(format!("{} must be a whole number", field.label));
                        }
                    },
                    Kind::OptFloat if text.is_empty() => Value::Null,
                    Kind::OptFloat => match text.trim_start_matches('$').parse::<f64>() {
                        Ok(n) => json!(n),
                        Err(_) => {
                            return self.fail(format!(
                                "{} must be a number (or empty for none)",
                                field.label
                            ));
                        }
                    },
                    Kind::OptText if text.is_empty() => Value::Null,
                    _ => json!(text),
                };
                save(json!({ field.key: value }))
            }
            Editing::NewRule { text } => {
                let mut parts = text.split_whitespace();
                let (Some(repo), Some(pattern), None) = (parts.next(), parts.next(), parts.next())
                else {
                    return self.fail(
                        "Type the repository and a pattern, e.g. acme/api migrations/**".into(),
                    );
                };
                let mut rules = self
                    .settings
                    .as_ref()
                    .map(|s| s.file_rules.clone())
                    .unwrap_or_default();
                rules
                    .entry(repo.to_owned())
                    .or_default()
                    .push(pattern.to_owned());
                save(json!({ "file_rules": rules }))
            }
        }
    }

    fn remove_rule(&mut self, index: usize) -> Vec<Cmd> {
        let Some((repo, pattern)) = self.rules().get(index).cloned() else {
            return vec![];
        };
        let mut rules = self
            .settings
            .as_ref()
            .map(|s| s.file_rules.clone())
            .unwrap_or_default();
        if let Some(patterns) = rules.get_mut(&repo) {
            patterns.retain(|p| *p != pattern);
            if patterns.is_empty() {
                rules.remove(&repo);
            }
        }
        save(json!({ "file_rules": rules }))
    }

    fn fail(&mut self, message: String) -> Vec<Cmd> {
        self.error = Some(message);
        vec![]
    }

    pub fn on_saved(&mut self, result: Result<Settings, String>) {
        match result {
            Ok(settings) => {
                self.settings = Some(settings);
                self.saved = Some("Saved: it applies right away.".into());
                let rows = FIELDS.len() + self.rules().len();
                self.selected = self.selected.min(rows.saturating_sub(1));
            }
            Err(e) => self.error = Some(e),
        }
    }
}

fn save(patch: Value) -> Vec<Cmd> {
    vec![Cmd::SaveSettings { patch }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn tab() -> SettingsTab {
        let settings: Settings = serde_json::from_value(json!({
            "poll_interval_secs": 60, "review_concurrency": 2, "claude_model": null,
            "review_timeout_secs": 900, "review_budget_usd": null, "daily_budget_usd": null,
            "budget_day": "utc", "risk_threshold": 60, "max_changed_lines": 70000,
            "max_changed_files": 350, "sleep_mode": null, "skip_drafts": true,
            "include_team_requests": false, "notifications": true,
            "file_rules": { "acme/api": ["migrations/**"] }
        }))
        .unwrap();
        SettingsTab {
            settings: Some(settings),
            ..Default::default()
        }
    }

    fn select(tab: &mut SettingsTab, key_name: &str) {
        tab.selected = FIELDS.iter().position(|f| f.key == key_name).unwrap();
    }

    #[test]
    fn booleans_toggle_and_choices_cycle() {
        let mut t = tab();
        select(&mut t, "skip_drafts");
        assert_eq!(
            t.on_key(key(KeyCode::Enter)),
            [Cmd::SaveSettings {
                patch: json!({ "skip_drafts": false })
            }]
        );
        select(&mut t, "sleep_mode");
        assert_eq!(
            t.on_key(key(KeyCode::Enter)),
            [Cmd::SaveSettings {
                patch: json!({ "sleep_mode": "always" })
            }]
        );
    }

    #[test]
    fn numbers_are_checked_and_empty_clears_optional_ones() {
        let mut t = tab();
        select(&mut t, "risk_threshold");
        t.on_key(key(KeyCode::Enter));
        assert_eq!(
            t.editing,
            Some(Editing::Field {
                index: t.selected,
                text: "60".into()
            })
        );
        t.on_key(key(KeyCode::Backspace));
        t.on_key(key(KeyCode::Char('x')));
        assert!(t.on_key(key(KeyCode::Enter)).is_empty());
        assert!(t.error.is_some());

        select(&mut t, "daily_budget_usd");
        t.on_key(key(KeyCode::Enter));
        for c in "$2.5".chars() {
            t.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            t.on_key(key(KeyCode::Enter)),
            [Cmd::SaveSettings {
                patch: json!({ "daily_budget_usd": 2.5 })
            }]
        );
    }

    #[test]
    fn file_rules_are_added_and_removed() {
        let mut t = tab();
        t.on_key(key(KeyCode::Char('a')));
        for c in "acme/api **/*.sql".chars() {
            t.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            t.on_key(key(KeyCode::Enter)),
            [Cmd::SaveSettings {
                patch: json!({ "file_rules": { "acme/api": ["migrations/**", "**/*.sql"] } })
            }]
        );
        t.selected = FIELDS.len();
        assert_eq!(
            t.on_key(key(KeyCode::Char('x'))),
            [Cmd::SaveSettings {
                patch: json!({ "file_rules": {} })
            }]
        );
    }
}
