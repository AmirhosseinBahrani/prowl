//! Rendering for the Feed and History tabs.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Table, TableState, Wrap};

use crate::app::StatusBar;
use crate::feed_tab::{FeedTab, Tone, clock, describe, duration, subject};
use crate::history_tab::{HistoryTab, decision_label};

pub fn draw_feed(feed: &FeedTab, frame: &mut Frame, area: Rect) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let state = if feed.paused {
        "paused".yellow().bold()
    } else {
        "following".green()
    };
    let mut head = vec![" ".into(), state, "  times in UTC".dim()];
    if !feed.filter.is_empty() {
        head.push(format!("  filter: {}", feed.filter).cyan());
    }
    frame.render_widget(Line::from(head), header);

    let visible = feed.visible();
    let height = body.height as usize;
    let end = visible.len().saturating_sub(feed.scroll.min(visible.len()));
    let start = end.saturating_sub(height);
    let lines: Vec<Line> = visible[start..end]
        .iter()
        .map(|event| {
            let (tone, text) = describe(event);
            let style = match tone {
                Tone::Normal => Style::default(),
                Tone::Good => Style::default().fg(Color::Green),
                Tone::Warn => Style::default().fg(Color::Yellow),
                Tone::Bad => Style::default().fg(Color::Red),
                Tone::Dim => Style::default().add_modifier(Modifier::DIM),
            };
            let who = subject(event)
                .map(|s| format!("{s:<20} "))
                .unwrap_or_else(|| " ".repeat(21));
            Line::from(vec![
                format!(" {} ", clock(event.at_ms)).dim(),
                Span::raw(who).bold(),
                Span::styled(text, style),
            ])
        })
        .collect();
    if lines.is_empty() {
        let text = if feed.loaded {
            "  Nothing yet."
        } else {
            "  Loading recent activity…"
        };
        frame.render_widget(Paragraph::new(text).dim(), body);
    } else {
        frame.render_widget(Paragraph::new(lines), body);
    }
    let foot = match &feed.editing {
        Some(text) => Line::from(vec![
            " filter: ".bold(),
            Span::raw(format!("{text}▏")),
            "   enter apply · esc cancel".dim(),
        ]),
        None => Line::from(
            format!(
                " [space] {} · ↑↓ scroll · [f] filter · [c] clear · [p] {} polls",
                if feed.paused { "follow" } else { "pause" },
                if feed.show_polls { "hide" } else { "show" }
            )
            .dim(),
        ),
    };
    frame.render_widget(foot, footer);
}

pub fn draw_history(history: &HistoryTab, status: &StatusBar, frame: &mut Frame, area: Rect) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let mut head = vec![" ".into()];
    if let Some(costs) = &status.costs {
        head.push(match costs.daily_budget_usd {
            Some(b) => format!("Today: ${:.2} of ${b:.2}", costs.today_usd).bold(),
            None => format!("Today: ${:.2}", costs.today_usd).bold(),
        });
        head.push(format!(" · {} runs  (UTC day)", costs.reviews_today).dim());
    }
    if !history.filter.is_empty() {
        head.push(format!("  filter: {}", history.filter).cyan());
    }
    frame.render_widget(Line::from(head), header);

    let visible = history.visible();
    if history.detail
        && let Some(review) = visible.get(history.selected)
    {
        frame.render_widget(
            Paragraph::new(detail(review)).wrap(Wrap { trim: false }),
            body,
        );
    } else if visible.is_empty() {
        let text = if history.loaded {
            "  No reviews yet."
        } else {
            "  Loading…"
        };
        frame.render_widget(Paragraph::new(text).dim(), body);
    } else {
        let rows = visible.iter().map(|r| {
            let decision = match decision_label(r) {
                "agent" => Span::raw("agent").green(),
                "human" => Span::raw("human").yellow(),
                other => Span::raw(other),
            };
            let severity = r
                .output
                .as_ref()
                .map(|o| format!("{:?}", o.severity).to_lowercase())
                .unwrap_or_else(|| "unusable".into());
            let rules: Vec<String> = r
                .decision_reasons
                .iter()
                .filter_map(|reason| match reason {
                    prowld::Reason::PolicyRule { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            Row::new(vec![
                Line::from(crate::feed_tab::clock(r.created_at_ms)),
                Line::from(format!("{}#{}", r.repo, r.pr_number)),
                Line::from(r.title.clone()),
                Line::from(decision),
                Line::from(severity),
                Line::from(rules.join(",")),
                Line::from(r.cost_usd.map(|c| format!("${c:.2}")).unwrap_or_default()),
                Line::from(duration(r.duration_ms)),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(8),
                Constraint::Length(18),
                Constraint::Min(10),
                Constraint::Length(6),
                Constraint::Length(9),
                Constraint::Length(12),
                Constraint::Length(6),
                Constraint::Length(6),
            ],
        )
        .header(
            Row::new([
                "time", "PR", "title", "who", "severity", "rules", "cost", "took",
            ])
            .bold(),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = TableState::default().with_selected(Some(history.selected));
        frame.render_stateful_widget(table, body, &mut state);
    }
    let foot = match &history.editing {
        Some(text) => Line::from(vec![
            " filter: ".bold(),
            Span::raw(format!("{text}▏")),
            "   enter apply · esc cancel".dim(),
        ]),
        None => Line::from(
            " ↑↓ select · [enter] details · [o] open on GitHub · [f] filter · [c] clear".dim(),
        ),
    };
    frame.render_widget(foot, footer);
}

fn detail(review: &prowld::Review) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!(
            " {}#{} — {}",
            review.repo, review.pr_number, review.title
        ))
        .bold(),
        Line::from(format!(
            " policy {} v{} · {} · took {}",
            review.policy_scope,
            review.policy_version_id,
            review
                .cost_usd
                .map(|c| format!("${c:.2}"))
                .unwrap_or_else(|| "no cost".into()),
            duration(review.duration_ms)
        ))
        .dim(),
        Line::from(""),
    ];
    match review.decision {
        Some(prowld::Decision::AgentOnly) => {
            lines.push(Line::from(" The agent's review was enough.").green())
        }
        Some(prowld::Decision::NeedsHuman) => {
            lines.push(Line::from(" Needed a human review:").yellow())
        }
        None => {}
    }
    for reason in &review.decision_reasons {
        lines.push(Line::from(format!(
            "   • {}",
            crate::policy_ui::reason_text(reason)
        )));
    }
    if let Some(output) = &review.output {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            " Summary: ".bold(),
            Span::raw(output.summary.clone()),
        ]));
        for note in &output.quick_notes {
            lines.push(Line::from(format!("   - {note}")));
        }
        for f in &output.findings {
            let at = f.line.map(|l| format!(":{l}")).unwrap_or_default();
            lines.push(Line::from(
                format!("   [{:?}] {}{at} {}", f.severity, f.file, f.title).to_lowercase(),
            ));
        }
    } else if let Some(why) = &review.invalid_reason {
        lines.push(Line::from(format!(" No usable review: {why}")));
    }
    if let Some(url) = &review.published_url {
        lines.push(Line::from(""));
        lines.push(Line::from(format!(" Posted: {url}")).dim());
    }
    lines.push(Line::from(""));
    lines.push(Line::from(" [esc] back").dim());
    lines
}
