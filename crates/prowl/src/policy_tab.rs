//! The Policy tab: view the global policy and overrides, edit them in $EDITOR, browse and
//! restore versions, create or delete overrides, and dry-run a PR.

use crossterm::event::{KeyCode, KeyEvent};
use prowld::{DryRun, EffectivePolicy, PolicySummary, PolicyVersion};

use crate::app::Cmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    NewOverride,
    DryRun,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub kind: InputKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    Delete { scope: String },
    Restore { scope: String, version_id: i64 },
}

#[derive(Debug, Clone)]
pub enum DryRunState {
    Running {
        target: String,
    },
    Done {
        target: String,
        result: Box<Result<DryRun, String>>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct PolicyTab {
    pub list: Vec<PolicySummary>,
    pub selected: usize,
    pub content: Option<EffectivePolicy>,
    /// `Some` while browsing history.
    pub versions: Option<Vec<PolicyVersion>>,
    pub version_selected: usize,
    pub input: Option<Input>,
    pub confirm: Option<Confirm>,
    pub dry_run: Option<DryRunState>,
}

impl PolicyTab {
    pub fn selected_scope(&self) -> Option<String> {
        self.list.get(self.selected).map(|p| p.scope.clone())
    }

    /// Whether keys should go here before global shortcuts (typing, confirming).
    pub fn captures_keys(&self) -> bool {
        self.input.is_some() || self.confirm.is_some()
    }

    pub fn on_policies(&mut self, list: Vec<PolicySummary>) -> Vec<Cmd> {
        let previous = self.selected_scope();
        self.list = list;
        self.selected = previous
            .and_then(|s| self.list.iter().position(|p| p.scope == s))
            .unwrap_or(0);
        self.reload_selected()
    }

    pub fn reload_selected(&mut self) -> Vec<Cmd> {
        let Some(scope) = self.selected_scope() else {
            return vec![];
        };
        let mut cmds = vec![Cmd::LoadPolicy {
            scope: scope.clone(),
        }];
        if self.versions.is_some() {
            cmds.push(Cmd::LoadVersions { scope });
        }
        cmds
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if let Some(confirm) = self.confirm.take() {
            return match key.code {
                KeyCode::Char('y') => match confirm {
                    Confirm::Delete { scope } => vec![Cmd::DeleteOverride { scope }],
                    Confirm::Restore { scope, version_id } => {
                        vec![Cmd::RestorePolicy { scope, version_id }]
                    }
                },
                _ => vec![],
            };
        }
        if let Some(input) = &mut self.input {
            match key.code {
                KeyCode::Esc => self.input = None,
                KeyCode::Backspace => {
                    input.text.pop();
                }
                KeyCode::Char(c) => input.text.push(c),
                KeyCode::Enter => {
                    let Input { kind, text } = self.input.take().expect("input is set");
                    let text = text.trim().to_owned();
                    return match kind {
                        InputKind::NewOverride => vec![Cmd::CreateOverride { repo: text }],
                        InputKind::DryRun => match parse_pr_ref(&text) {
                            Some((repo, pr)) => {
                                self.dry_run = Some(DryRunState::Running { target: text });
                                vec![Cmd::DryRun { repo, pr }]
                            }
                            None => {
                                self.dry_run = Some(DryRunState::Done {
                                    target: text,
                                    result: Box::new(Err(
                                        "Expected owner/repo#number, e.g. acme/api#42".into(),
                                    )),
                                });
                                vec![]
                            }
                        },
                    };
                }
                _ => {}
            }
            return vec![];
        }
        if self.dry_run.is_some() && key.code == KeyCode::Esc {
            self.dry_run = None;
            return vec![];
        }
        if let Some(versions) = &self.versions {
            match key.code {
                KeyCode::Esc | KeyCode::Char('h') => {
                    self.versions = None;
                    return vec![];
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if self.version_selected + 1 < versions.len() {
                        self.version_selected += 1;
                    }
                    return vec![];
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.version_selected = self.version_selected.saturating_sub(1);
                    return vec![];
                }
                KeyCode::Char('r') => {
                    if let (Some(scope), Some(v)) =
                        (self.selected_scope(), versions.get(self.version_selected))
                        && v.source != "delete"
                    {
                        self.confirm = Some(Confirm::Restore {
                            scope,
                            version_id: v.id,
                        });
                    }
                    return vec![];
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.list.len() {
                    self.selected += 1;
                    return self.reload_selected();
                }
                vec![]
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                    return self.reload_selected();
                }
                vec![]
            }
            KeyCode::Char('e') => self
                .selected_scope()
                .map(|scope| vec![Cmd::EditPolicy { scope }])
                .unwrap_or_default(),
            KeyCode::Char('h') => {
                self.version_selected = 0;
                self.versions = Some(Vec::new());
                self.selected_scope()
                    .map(|scope| vec![Cmd::LoadVersions { scope }])
                    .unwrap_or_default()
            }
            KeyCode::Char('n') => {
                self.input = Some(Input {
                    kind: InputKind::NewOverride,
                    text: String::new(),
                });
                vec![]
            }
            KeyCode::Char('t') => {
                self.input = Some(Input {
                    kind: InputKind::DryRun,
                    text: String::new(),
                });
                vec![]
            }
            // Learn from reviewers' comments: this repository's, or every watched one's.
            KeyCode::Char('L') => vec![Cmd::LearnPolicy {
                repo: self.selected_scope().filter(|s| s != "global"),
            }],
            KeyCode::Char('x') => {
                if let Some(scope) = self.selected_scope().filter(|s| s != "global") {
                    self.confirm = Some(Confirm::Delete { scope });
                }
                vec![]
            }
            _ => vec![],
        }
    }
}

/// `owner/repo#123` → (`owner/repo`, 123).
pub fn parse_pr_ref(text: &str) -> Option<(String, u64)> {
    let (repo, number) = text.split_once('#')?;
    let (owner, name) = repo.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some((repo.to_owned(), number.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn summary(scope: &str) -> PolicySummary {
        PolicySummary {
            scope: scope.into(),
            version_id: 1,
            rules: vec![],
            path: format!("/p/{scope}.md"),
        }
    }

    #[test]
    fn pr_refs_parse() {
        assert_eq!(parse_pr_ref("acme/api#42"), Some(("acme/api".into(), 42)));
        for bad in ["acme/api", "acme#1", "/api#1", "a/b/c#1", "acme/api#x"] {
            assert_eq!(parse_pr_ref(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_global_policy_cannot_be_deleted_and_overrides_ask_first() {
        let mut tab = PolicyTab::default();
        tab.on_policies(vec![summary("global"), summary("acme/api")]);
        tab.on_key(key(KeyCode::Char('x')));
        assert!(tab.confirm.is_none(), "global can't be deleted");

        tab.on_key(key(KeyCode::Down));
        tab.on_key(key(KeyCode::Char('x')));
        assert_eq!(
            tab.confirm,
            Some(Confirm::Delete {
                scope: "acme/api".into()
            })
        );
        assert!(
            tab.on_key(key(KeyCode::Char('n'))).is_empty(),
            "anything but y cancels"
        );
        tab.on_key(key(KeyCode::Char('x')));
        assert_eq!(
            tab.on_key(key(KeyCode::Char('y'))),
            [Cmd::DeleteOverride {
                scope: "acme/api".into()
            }]
        );
    }

    #[test]
    fn dry_run_input_is_validated() {
        let mut tab = PolicyTab::default();
        tab.on_key(key(KeyCode::Char('t')));
        for c in "acme/api#7".chars() {
            tab.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            tab.on_key(key(KeyCode::Enter)),
            [Cmd::DryRun {
                repo: "acme/api".into(),
                pr: 7
            }]
        );

        tab.on_key(key(KeyCode::Char('t')));
        tab.on_key(key(KeyCode::Char('?')));
        assert!(tab.on_key(key(KeyCode::Enter)).is_empty());
        assert!(matches!(&tab.dry_run, Some(DryRunState::Done { result, .. }) if result.is_err()));
    }
}
