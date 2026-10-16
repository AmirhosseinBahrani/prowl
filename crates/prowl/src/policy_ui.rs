//! Rendering for the Policy tab.

use prowld::Reason;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use similar::{ChangeTag, TextDiff};

use crate::policy_tab::{Confirm, DryRunState, InputKind, PolicyTab};

pub fn draw(tab: &PolicyTab, frame: &mut Frame, area: Rect) {
    let [main, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
    let [left, right] =
        Layout::horizontal([Constraint::Length(28), Constraint::Min(20)]).areas(main);

    let items: Vec<ListItem> = tab
        .list
        .iter()
        .map(|p| {
            ListItem::new(Line::from(vec![
                Span::raw(p.scope.clone()),
                format!("  {} rules", p.rules.len()).dim(),
            ]))
        })
        .collect();
    let mut state =
        ListState::default().with_selected((!tab.list.is_empty()).then_some(tab.selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::RIGHT).title(" policies "))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        left,
        &mut state,
    );

    if let Some(dry) = &tab.dry_run {
        dry_run(dry, frame, right);
    } else if let Some(versions) = &tab.versions {
        history(tab, versions, frame, right);
    } else {
        content(tab, frame, right);
    }

    let footer_line = if let Some(input) = &tab.input {
        let prompt = match input.kind {
            InputKind::NewOverride => " override for repository (owner/repo): ",
            InputKind::DryRun => " dry-run which PR (owner/repo#number): ",
        };
        Line::from(vec![
            prompt.bold(),
            Span::raw(format!("{}▏", input.text)),
            "   enter ok · esc cancel".dim(),
        ])
    } else if let Some(confirm) = &tab.confirm {
        let question = match confirm {
            Confirm::Delete { scope } => {
                format!(" Delete the override for {scope}? It falls back to the global policy. ")
            }
            Confirm::Restore { version_id, .. } => format!(" Restore version {version_id}? "),
        };
        Line::from(vec![
            question.bold().yellow(),
            "[y] yes  any other key: no".into(),
        ])
    } else if tab.versions.is_some() {
        Line::from(" ↑↓ version · [r] restore · [esc] back".dim())
    } else {
        Line::from(
            " [e] edit · [h] history · [n] new override · [x] delete override · [t] dry-run a PR"
                .dim(),
        )
    };
    frame.render_widget(footer_line, footer);
}

fn content(tab: &PolicyTab, frame: &mut Frame, area: Rect) {
    let Some(policy) = &tab.content else {
        frame.render_widget(Paragraph::new(" Loading…").dim(), area);
        return;
    };
    let rule_ids: Vec<&str> = policy.rules.iter().map(|r| r.id.as_str()).collect();
    let lines: Vec<Line> = policy
        .content
        .lines()
        .map(|line| {
            let is_rule = rule_ids.iter().any(|id| line.contains(&format!("[{id}]")));
            if line.starts_with('#') {
                Line::from(line.to_owned()).bold()
            } else if is_rule {
                Line::from(line.to_owned()).yellow()
            } else {
                Line::from(line.to_owned())
            }
        })
        .collect();
    let title = format!(" {} · version {} ", policy.scope, policy.version_id);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn history(tab: &PolicyTab, versions: &[prowld::PolicyVersion], frame: &mut Frame, area: Rect) {
    let [list, diff] = Layout::vertical([Constraint::Length(8), Constraint::Min(3)]).areas(area);
    let items: Vec<ListItem> = versions
        .iter()
        .map(|v| ListItem::new(format!("v{}  {}  {}", v.id, v.source, age(v.created_at_ms))))
        .collect();
    let mut state =
        ListState::default().with_selected((!versions.is_empty()).then_some(tab.version_selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .title(" versions (newest first) "),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        list,
        &mut state,
    );
    let (Some(selected), Some(current)) = (versions.get(tab.version_selected), &tab.content) else {
        return;
    };
    let mut lines = vec![
        Line::from(format!(
            " changes from v{} to the current version:",
            selected.id
        ))
        .dim(),
    ];
    let text_diff = TextDiff::from_lines(selected.content.as_str(), current.content.as_str());
    let mut changed = false;
    for change in text_diff.iter_all_changes() {
        let text = change.to_string_lossy().trim_end_matches('\n').to_owned();
        match change.tag() {
            ChangeTag::Delete => {
                changed = true;
                lines.push(Line::from(format!("- {text}")).red());
            }
            ChangeTag::Insert => {
                changed = true;
                lines.push(Line::from(format!("+ {text}")).green());
            }
            ChangeTag::Equal => {}
        }
    }
    if !changed {
        lines.push(Line::from(" (identical)").dim());
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), diff);
}

fn dry_run(state: &DryRunState, frame: &mut Frame, area: Rect) {
    let mut lines = Vec::new();
    match state {
        DryRunState::Running { target } => {
            lines.push(Line::from(format!(
                " Dry run of {target}: reviewing with Claude… (nothing will be posted)"
            )));
        }
        DryRunState::Done { target, result } if result.is_err() => {
            let Err(e) = result.as_ref() else {
                unreachable!()
            };
            lines.push(
                Line::from(format!(" Dry run of {target} failed"))
                    .red()
                    .bold(),
            );
            lines.push(Line::from(format!(" {e}")));
        }
        DryRunState::Done { target, result } => {
            let Ok(r) = result.as_ref() else {
                unreachable!()
            };
            let verdict = match r.decision {
                prowld::Decision::AgentOnly => " → the agent's review would be posted on its own"
                    .green()
                    .bold(),
                prowld::Decision::NeedsHuman => {
                    " → this PR would wait for a human review".yellow().bold()
                }
            };
            lines.push(Line::from(format!(
                " Dry run of {target} (policy {} v{})",
                r.policy_scope, r.policy_version_id
            )));
            lines.push(Line::from(verdict));
            for reason in &r.reasons {
                lines.push(Line::from(format!("   • {}", reason_text(reason))));
            }
            lines.push(Line::from(""));
            if let Some(review) = &r.review {
                lines.push(Line::from(vec![
                    " Summary: ".bold(),
                    Span::raw(review.summary.clone()),
                ]));
                for note in &review.quick_notes {
                    lines.push(Line::from(format!("   - {note}")));
                }
                for finding in &review.findings {
                    lines.push(
                        Line::from(format!(
                            "   [{:?}] {}: {}",
                            finding.severity, finding.file, finding.title
                        ))
                        .dim(),
                    );
                }
            } else if let Some(why) = &r.invalid_reason {
                lines.push(Line::from(format!(" No usable review: {why}")));
            }
            if let Some(cost) = r.cost_usd {
                lines.push(Line::from(format!(" Cost: ${cost:.2}")).dim());
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(" [esc] close").dim());
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

pub fn reason_text(reason: &Reason) -> String {
    match reason {
        Reason::InvalidOutput { detail } => format!("the automated review was unusable ({detail})"),
        Reason::PolicyRule { id, evidence } => format!("matched [{id}] {evidence}"),
        Reason::UnknownPolicyRule { id } => format!("cited [{id}], which isn't in the policy"),
        Reason::CriticalSection => "touches critical code".into(),
        Reason::Severity { severity } => format!("severity {severity:?}").to_lowercase(),
        Reason::RiskScore { score, threshold } => {
            format!("risk score {score} (threshold {threshold})")
        }
        Reason::FilePattern { pattern, file } => format!("changes {file} (safety rule {pattern})"),
        Reason::TooLarge {
            changed_lines,
            changed_files,
            ..
        } => {
            format!("too large for the agent ({changed_lines} lines, {changed_files} files)")
        }
    }
}

fn age(at_ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let secs = ((now - at_ms) / 1000).max(0);
    match secs {
        0..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}
