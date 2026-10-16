//! The History tab: past reviews, with detail and a link to the PR.

use crossterm::event::{KeyCode, KeyEvent};
use prowld::Review;

use crate::app::Cmd;

#[derive(Debug, Clone, Default)]
pub struct HistoryTab {
    pub reviews: Vec<Review>,
    pub loaded: bool,
    pub selected: usize,
    pub detail: bool,
    pub filter: String,
    pub editing: Option<String>,
}

impl HistoryTab {
    pub fn captures_keys(&self) -> bool {
        self.editing.is_some()
    }

    pub fn on_reviews(&mut self, reviews: Vec<Review>) {
        let keep = self.visible().get(self.selected).map(|r| r.id);
        self.reviews = reviews;
        self.loaded = true;
        self.selected = keep
            .and_then(|id| self.visible().iter().position(|r| r.id == id))
            .unwrap_or(0);
    }

    pub fn visible(&self) -> Vec<&Review> {
        let needle = self.filter.to_lowercase();
        self.reviews
            .iter()
            .filter(|r| {
                needle.is_empty()
                    || format!("{}#{}", r.repo, r.pr_number)
                        .to_lowercase()
                        .contains(&needle)
                    || r.title.to_lowercase().contains(&needle)
                    || decision_label(r).to_lowercase().contains(&needle)
            })
            .collect()
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if let Some(text) = &mut self.editing {
            match key.code {
                KeyCode::Esc => self.editing = None,
                KeyCode::Enter => {
                    self.filter = self.editing.take().unwrap_or_default().trim().to_owned();
                    self.selected = 0;
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) => text.push(c),
                _ => {}
            }
            return vec![];
        }
        let len = self.visible().len();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') if self.selected + 1 < len => self.selected += 1,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Enter => self.detail = !self.detail,
            KeyCode::Esc => self.detail = false,
            KeyCode::Char('f') | KeyCode::Char('/') => self.editing = Some(self.filter.clone()),
            KeyCode::Char('c') => {
                self.filter.clear();
                self.selected = 0;
            }
            KeyCode::Char('o') => {
                if let Some(review) = self.visible().get(self.selected) {
                    let url = review
                        .published_url
                        .clone()
                        .filter(|u| !u.is_empty())
                        .unwrap_or_else(|| review.html_url.clone());
                    if !url.is_empty() {
                        return vec![Cmd::OpenUrl { url }];
                    }
                }
            }
            _ => {}
        }
        vec![]
    }
}

pub fn decision_label(review: &Review) -> &'static str {
    match review.decision {
        Some(prowld::Decision::AgentOnly) => "agent",
        Some(prowld::Decision::NeedsHuman) => "human",
        None => "—",
    }
}
