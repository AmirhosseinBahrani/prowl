//! Rendering for the Queue tab.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Table, TableState, Wrap};

use crate::feed_tab::duration;
use crate::policy_ui::reason_text;
use crate::queue_tab::{QueueAction, QueueTab};

/// `me` is the connected account, whose threads are PRowl's.
pub fn draw(tab: &QueueTab, me: Option<&str>, frame: &mut Frame, area: Rect) {
    let [body, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
    if let Some(detail) = &tab.detail {
        draw_detail(tab, detail, me, frame, body);
    } else if tab.items.is_empty() {
        let text = if tab.loaded {
            "  Nothing is waiting for you. PRowl will notify you when a PR needs your review."
        } else {
            "  Loading…"
        };
        frame.render_widget(Paragraph::new(text).dim(), body);
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let rows = tab.items.iter().map(|item| {
            let severity = match item.severity {
                Some(s) => format!("{s:?}").to_lowercase(),
                None => "unusable".into(),
            };

            let why: Vec<String> = item.reasons.iter().map(short_reason).collect();
            Row::new(vec![
                Line::from(crate::theme::severity_badge(&severity)),
                Line::from(crate::theme::pr_ref(format!(
                    "{}#{}",
                    item.repo, item.pr_number
                ))),
                Line::from(item.title.clone()),
                Line::from(why.join(", ")),
                Line::from(duration(now - item.queued_at_ms)),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Length(18),
                Constraint::Min(12),
                Constraint::Length(26),
                Constraint::Length(7),
            ],
        )
        .header(Row::new(["severity", "PR", "title", "why", "waiting"]).bold())
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = TableState::default().with_selected(Some(tab.selected));
        frame.render_stateful_widget(table, body, &mut state);
    }
    frame.render_widget(footer_line(tab), footer);
}

fn footer_line(tab: &QueueTab) -> Line<'static> {
    if let Some(input) = &tab.input {
        let prompt = match input.action {
            QueueAction::Approve => " approve — optional comment: ",
            QueueAction::RequestChanges => " request changes — what needs to change: ",
            QueueAction::Dismiss => "",
        };
        let mut spans = vec![prompt.bold(), Span::raw(format!("{}▏", input.text))];
        match &tab.error {
            Some(error) => spans.push(format!("   {error}").red()),
            None => spans.push("   enter send · esc cancel".dim()),
        }
        return Line::from(spans);
    }
    if tab.confirm_dismiss {
        return Line::from(vec![
            " Remove it from your queue without reviewing on GitHub? "
                .bold()
                .yellow(),
            "[y] yes  any other key: no".into(),
        ]);
    }
    if tab.busy {
        return Line::from(" sending to GitHub…".dim());
    }
    if let Some(error) = &tab.error {
        return Line::from(format!(" {error}")).red();
    }
    if tab.detail.is_some() && tab.show_diff {
        crate::theme::hints(
            " [a] approve · [r] request changes · [d] dismiss · [v] verify · ↑↓ ] [ · [D] threads · [esc] back",
        )
    } else if tab.detail.is_some() {
        crate::theme::hints(
            " [a] approve · [r] request changes · [d] dismiss · [v] verify · [R] re-review · ↑↓ · [D] diff · [esc] back",
        )
    } else {
        crate::theme::hints(
            " [enter] open · [a] approve · [r] request changes · [d] dismiss · [o] open on GitHub · [R] re-review · [v] verify",
        )
    }
}

fn draw_detail(
    tab: &QueueTab,
    detail: &prowld::QueueDetail,
    me: Option<&str>,
    frame: &mut Frame,
    area: Rect,
) {
    let item = &detail.item;
    let mut info = vec![
        Line::from(format!(
            " {}#{} — {}  (by @{})",
            item.repo, item.pr_number, item.title, item.author
        ))
        .bold(),
        Line::from(" Waiting for you because:").yellow(),
    ];
    for reason in &item.reasons {
        info.push(Line::from(format!("   • {}", reason_text(reason))));
    }
    if let Some(summary) = &item.summary {
        info.push(Line::from(vec![
            " Agent's summary: ".bold(),
            Span::raw(summary.clone()),
        ]));
    }
    for note in &item.quick_notes {
        info.push(Line::from(format!("   - {note}")));
    }
    let info_height = (info.len() as u16 + 1).min(area.height / 2);
    let [top, bottom] =
        Layout::vertical([Constraint::Length(info_height), Constraint::Min(3)]).areas(area);
    frame.render_widget(Paragraph::new(info).wrap(Wrap { trim: false }), top);

    if !tab.show_diff {
        let lines = posted_lines(tab, detail, me, bottom.width);
        let max = lines.len().saturating_sub(bottom.height as usize);
        tab.boxes_max.set(max);
        let scroll = tab.boxes_scroll.min(max);
        frame.render_widget(
            Paragraph::new(lines).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
            bottom,
        );
        return;
    }
    let lines: Vec<Line> = detail
        .diff
        .lines()
        .skip(tab.diff_scroll)
        .take(bottom.height as usize)
        .map(|line| {
            let style = if line.starts_with("diff --git") {
                crate::theme::accent()
            } else if line.starts_with("@@") {
                Style::default().fg(Color::Magenta)
            } else if line.starts_with('+') && !line.starts_with("+++") {
                Style::default().fg(Color::Green)
            } else if line.starts_with('-') && !line.starts_with("---") {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            Line::from(Span::styled(line.to_owned(), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), bottom);
}

fn short_reason(reason: &prowld::Reason) -> String {
    use prowld::Reason::*;
    match reason {
        PolicyRule { id, .. } => format!("[{id}]"),
        UnknownPolicyRule { id } => format!("[{id}]?"),
        CriticalSection => "critical code".into(),
        Severity { .. } => "severity".into(),
        RiskScore { score, .. } => format!("risk {score}"),
        InvalidOutput { .. } => "unusable review".into(),
        TooLarge { .. } => "too large".into(),
        FilePattern { .. } => "file rule".into(),
        PromptInjection { .. } => "⚠ injection".into(),
        HiddenCharacters { .. } => "⚠ hidden chars".into(),
        SecretRedacted { .. } => "⚠ secret".into(),
    }
}

/// What PRowl posted on the PR, as boxes: each review thread it opened (with whether it's
/// resolved, and the replies), then the findings that live in its comment instead.
pub fn posted_lines(
    tab: &QueueTab,
    detail: &prowld::QueueDetail,
    me: Option<&str>,
    width: u16,
) -> Vec<Line<'static>> {
    use crate::boxes::{Row as BoxRow, text_box};
    let heading = |text: String| Line::from(Span::styled(text, crate::theme::accent()));
    let mut lines = Vec::new();
    let mut threaded: Vec<(String, Option<u32>)> = Vec::new();
    match &tab.threads {
        None => lines.push(Line::from(" Loading PRowl's threads…").dim()),
        Some(Err(e)) => lines.push(Line::from(format!(" Couldn't load the threads: {e}")).red()),
        Some(Ok(threads)) => {
            let mine: Vec<&prowld::ReviewThread> = threads
                .iter()
                .filter(|t| {
                    me.is_some_and(|me| t.author.eq_ignore_ascii_case(me))
                        && t.body.contains("PRowl automated review")
                })
                .collect();
            let resolved = mine.iter().filter(|t| t.resolved).count();
            lines.push(heading(format!(
                " ── Threads PRowl opened ({}, {resolved} resolved) ──",
                mine.len()
            )));
            if mine.is_empty() {
                lines.push(Line::from("  None on this commit.").dim());
            }
            for thread in mine {
                threaded.push((thread.path.clone(), thread.line));
                let place = match thread.line {
                    Some(line) => format!("{}:{line}", thread.path),
                    None => thread.path.clone(),
                };
                let (state, color) = match (thread.resolved, thread.outdated) {
                    (true, _) => ("✓ resolved", crate::theme::GOOD),
                    (false, true) => ("○ open · outdated", crate::theme::MUTED),
                    (false, false) => ("○ open", crate::theme::WARN),
                };
                let mut rows: Vec<BoxRow> = readable(&thread.body)
                    .into_iter()
                    .enumerate()
                    .map(|(i, line)| {
                        if i == 0 {
                            BoxRow::bold(line)
                        } else {
                            BoxRow::plain(line)
                        }
                    })
                    .collect();
                for reply in thread.comments.iter().skip(1) {
                    let text = readable(&reply.body).join(" ");
                    rows.push(BoxRow::styled(
                        format!("↳ @{}: {text}", reply.author),
                        Style::default().fg(crate::theme::ACCENT),
                    ));
                }
                lines.extend(text_box(&format!("{state} · {place}"), color, &rows, width));
            }
        }
    }
    // Findings with no thread of their own are in PRowl's waiting-for-approval comment.
    let elsewhere: Vec<&prowld::review::Finding> = detail
        .item
        .findings
        .iter()
        .filter(|f| {
            !threaded
                .iter()
                .any(|(path, line)| *path == f.file && *line == f.line)
        })
        .collect();
    if !elsewhere.is_empty() {
        lines.push(Line::from(""));
        lines.push(heading(format!(
            " ── In PRowl's comment ({}) ──",
            elsewhere.len()
        )));
        for finding in elsewhere {
            lines.extend(crate::reviews_ui::finding_box(finding, width));
        }
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from(" [D] show the diff · ↑↓ scroll · [o] open the PR to reply or resolve").dim(),
    );
    lines
}

/// A GitHub comment as plain lines: no PRowl footer, no Markdown emphasis.
fn readable(body: &str) -> Vec<String> {
    body.lines()
        .filter(|l| !l.trim_start().starts_with("<sub>") && !l.trim().is_empty())
        .map(|l| l.replace("**", "").trim().to_owned())
        .collect()
}
