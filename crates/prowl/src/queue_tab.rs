//! The Queue tab: PRs waiting for the user's own review.

use crossterm::event::{KeyCode, KeyEvent};
use prowld::{QueueDetail, QueueItem};

use crate::app::Cmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    Approve,
    RequestChanges,
    Dismiss,
}

impl QueueAction {
    pub fn past_tense(self) -> &'static str {
        match self {
            QueueAction::Approve => "Approved",
            QueueAction::RequestChanges => "Requested changes on",
            QueueAction::Dismiss => "Dismissed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentInput {
    pub action: QueueAction,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct QueueTab {
    pub items: Vec<QueueItem>,
    pub loaded: bool,
    pub selected: usize,
    /// The PR opened in detail view, with its diff.
    pub detail: Option<QueueDetail>,
    pub diff_scroll: usize,
    pub input: Option<CommentInput>,
    pub confirm_dismiss: bool,
    /// An action is being sent to GitHub.
    pub busy: bool,
    pub error: Option<String>,
}

impl QueueTab {
    pub fn captures_keys(&self) -> bool {
        self.input.is_some() || self.confirm_dismiss
    }

    pub fn current(&self) -> Option<&QueueItem> {
        match &self.detail {
            Some(detail) => Some(&detail.item),
            None => self.items.get(self.selected),
        }
    }

    pub fn on_items(&mut self, items: Vec<QueueItem>) {
        let keep = self
            .items
            .get(self.selected)
            .map(|i| (i.repo.clone(), i.pr_number));
        self.items = items;
        self.loaded = true;
        self.selected = keep
            .and_then(|(repo, n)| {
                self.items
                    .iter()
                    .position(|i| i.repo == repo && i.pr_number == n)
            })
            .unwrap_or(0)
            .min(self.items.len().saturating_sub(1));
        // A PR resolved elsewhere (e.g. on GitHub) closes its detail view.
        if let Some(open) = &self.detail
            && !self
                .items
                .iter()
                .any(|i| i.repo == open.item.repo && i.pr_number == open.item.pr_number)
        {
            self.detail = None;
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if self.confirm_dismiss {
            self.confirm_dismiss = false;
            if key.code == KeyCode::Char('y') {
                return self.resolve(QueueAction::Dismiss, String::new());
            }
            return vec![];
        }
        if let Some(input) = &mut self.input {
            match key.code {
                KeyCode::Esc => self.input = None,
                KeyCode::Backspace => {
                    input.text.pop();
                }
                KeyCode::Char(c) => input.text.push(c),
                KeyCode::Enter => {
                    let CommentInput { action, text } = self.input.take().expect("input is set");
                    if action == QueueAction::RequestChanges && text.trim().is_empty() {
                        self.error =
                            Some("Say what needs to change: a comment is required.".into());
                        self.input = Some(CommentInput { action, text });
                        return vec![];
                    }
                    return self.resolve(action, text.trim().to_owned());
                }
                _ => {}
            }
            return vec![];
        }
        self.error = None;
        match key.code {
            KeyCode::Char('a') if self.current().is_some() => {
                self.input = Some(CommentInput {
                    action: QueueAction::Approve,
                    text: String::new(),
                });
            }
            KeyCode::Char('r') if self.current().is_some() => {
                self.input = Some(CommentInput {
                    action: QueueAction::RequestChanges,
                    text: String::new(),
                });
            }
            KeyCode::Char('d') if self.current().is_some() => self.confirm_dismiss = true,
            KeyCode::Char('o') => {
                if let Some(item) = self.current() {
                    return vec![Cmd::OpenUrl {
                        url: item.html_url.clone(),
                    }];
                }
            }
            KeyCode::Char('R') => {
                if let Some(item) = self.current() {
                    return vec![Cmd::Rerun {
                        repo: item.repo.clone(),
                        pr: item.pr_number,
                    }];
                }
            }
            KeyCode::Char('v') => {
                if let Some(item) = self.current() {
                    return vec![Cmd::Verify {
                        repo: item.repo.clone(),
                        pr: item.pr_number,
                    }];
                }
            }
            KeyCode::Esc => self.detail = None,
            KeyCode::Enter if self.detail.is_none() => {
                if let Some(item) = self.items.get(self.selected) {
                    return vec![Cmd::LoadQueueItem {
                        repo: item.repo.clone(),
                        number: item.pr_number,
                    }];
                }
            }
            _ if self.detail.is_some() => self.scroll_diff(key.code),
            KeyCode::Down | KeyCode::Char('j') if self.selected + 1 < self.items.len() => {
                self.selected += 1
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            _ => {}
        }
        vec![]
    }

    fn scroll_diff(&mut self, code: KeyCode) {
        let Some(detail) = &self.detail else { return };
        let lines: Vec<&str> = detail.diff.lines().collect();
        let last = lines.len().saturating_sub(1);
        self.diff_scroll = match code {
            KeyCode::Down | KeyCode::Char('j') => self.diff_scroll + 1,
            KeyCode::Up | KeyCode::Char('k') => self.diff_scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.diff_scroll + 20,
            KeyCode::PageUp => self.diff_scroll.saturating_sub(20),
            KeyCode::Char(']') => lines
                .iter()
                .enumerate()
                .skip(self.diff_scroll + 1)
                .find(|(_, l)| l.starts_with("diff --git"))
                .map_or(self.diff_scroll, |(i, _)| i),
            KeyCode::Char('[') => lines
                .iter()
                .enumerate()
                .take(self.diff_scroll)
                .rev()
                .find(|(_, l)| l.starts_with("diff --git"))
                .map_or(0, |(i, _)| i),
            _ => self.diff_scroll,
        }
        .min(last);
    }

    fn resolve(&mut self, action: QueueAction, comment: String) -> Vec<Cmd> {
        let Some(item) = self.current() else {
            return vec![];
        };
        let cmd = Cmd::Resolve {
            repo: item.repo.clone(),
            number: item.pr_number,
            action,
            comment,
        };
        self.busy = true;
        vec![cmd]
    }

    pub fn on_resolved(&mut self, repo: &str, number: u64) {
        self.busy = false;
        self.items
            .retain(|i| !(i.repo == repo && i.pr_number == number));
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        self.detail = None;
        self.diff_scroll = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn item(n: u64) -> QueueItem {
        QueueItem {
            repo: "acme/api".into(),
            pr_number: n,
            title: format!("PR {n}"),
            author: "alice".into(),
            html_url: format!("https://github.com/acme/api/pull/{n}"),
            head_sha: "abc".into(),
            review_id: 1,
            status: "pending".into(),
            queued_at_ms: 0,
            severity: None,
            risk_score: None,
            summary: None,
            quick_notes: vec![],
            findings: vec![],
            reasons: vec![],
            comment_url: None,
        }
    }

    #[test]
    fn requesting_changes_insists_on_a_comment() {
        let mut tab = QueueTab::default();
        tab.on_items(vec![item(7)]);
        tab.on_key(key(KeyCode::Char('r')));
        assert!(tab.on_key(key(KeyCode::Enter)).is_empty());
        assert!(tab.error.is_some() && tab.input.is_some());
        for c in "add tests".chars() {
            tab.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            tab.on_key(key(KeyCode::Enter)),
            [Cmd::Resolve {
                repo: "acme/api".into(),
                number: 7,
                action: QueueAction::RequestChanges,
                comment: "add tests".into()
            }]
        );
    }

    #[test]
    fn dismissing_asks_first_and_approval_comments_are_optional() {
        let mut tab = QueueTab::default();
        tab.on_items(vec![item(7)]);
        tab.on_key(key(KeyCode::Char('d')));
        assert!(
            tab.on_key(key(KeyCode::Char('n'))).is_empty(),
            "anything but y cancels"
        );
        tab.on_key(key(KeyCode::Char('a')));
        assert!(matches!(
            &tab.on_key(key(KeyCode::Enter))[..],
            [Cmd::Resolve { action: QueueAction::Approve, comment, .. }] if comment.is_empty()
        ));
    }

    #[test]
    fn a_pr_resolved_elsewhere_closes_its_detail() {
        let mut tab = QueueTab::default();
        tab.on_items(vec![item(7), item(8)]);
        tab.detail = Some(QueueDetail {
            item: item(7),
            diff: String::new(),
        });
        tab.on_items(vec![item(8)]);
        assert!(tab.detail.is_none());
        assert_eq!(tab.items[tab.selected].pr_number, 8);
    }

    #[test]
    fn file_jumps_in_the_diff() {
        let mut tab = QueueTab::default();
        let diff = "diff --git a/x b/x\n+1\n+2\ndiff --git a/y b/y\n+3\n";
        tab.detail = Some(QueueDetail {
            item: item(7),
            diff: diff.into(),
        });
        tab.on_key(key(KeyCode::Char(']')));
        assert_eq!(tab.diff_scroll, 3);
        tab.on_key(key(KeyCode::Char('[')));
        assert_eq!(tab.diff_scroll, 0);
    }
}
