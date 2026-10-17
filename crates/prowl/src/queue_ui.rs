//! Rendering for the Queue tab.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Table, TableState, Wrap};

use crate::feed_tab::duration;
use crate::policy_ui::reason_text;
use crate::queue_tab::{QueueAction, QueueTab};

pub fn draw(tab: &QueueTab, frame: &mut Frame, area: Rect) {
    let [body, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
    if let Some(detail) = &tab.detail {
        draw_detail(tab, detail, frame, body);
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
    if tab.detail.is_some() {
        crate::theme::hints(
            " [a] approve · [r] request changes · [d] dismiss · [o] open · [R] re-review · ↑↓ ] [ · [esc] back",
        )
    } else {
        crate::theme::hints(
            " [enter] open · [a] approve · [r] request changes · [d] dismiss · [o] open on GitHub · [R] re-review · [v] verify",
        )
    }
}

fn draw_detail(tab: &QueueTab, detail: &prowld::QueueDetail, frame: &mut Frame, area: Rect) {
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
    for f in &item.findings {
        let at = f.line.map(|l| format!(":{l}")).unwrap_or_default();
        info.push(
            Line::from(format!("   [{:?}] {}{at} {}", f.severity, f.file, f.title).to_lowercase())
                .dim(),
        );
    }
    let info_height = (info.len() as u16 + 1).min(area.height / 2);
    let [top, bottom] =
        Layout::vertical([Constraint::Length(info_height), Constraint::Min(3)]).areas(area);
    frame.render_widget(Paragraph::new(info).wrap(Wrap { trim: false }), top);

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
