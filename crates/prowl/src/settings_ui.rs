//! Rendering for the Settings tab.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use serde_json::Value;

use crate::settings_tab::{Editing, FIELDS, Kind, SettingsTab};

pub fn draw(tab: &SettingsTab, frame: &mut Frame, area: Rect) {
    let [body, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
    if tab.settings.is_none() {
        frame.render_widget(Paragraph::new("  Loading…").dim(), body);
        return;
    }
    let mut items: Vec<ListItem> = FIELDS
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let shown = match &tab.editing {
                Some(Editing::Field { index, text }) if *index == i => {
                    Span::raw(format!("{text}▏")).cyan()
                }
                _ => Span::raw(display(field.kind, &tab.value(field.key))),
            };
            ListItem::new(Line::from(vec![
                format!("  {:<42}", field.label).into(),
                shown,
            ]))
        })
        .collect();
    items.push(ListItem::new(Line::from("")));
    items.push(ListItem::new(
        Line::from("  File rules: changes to matching files always need a human").bold(),
    ));
    let rules = tab.rules();
    if rules.is_empty() {
        items.push(ListItem::new(
            Line::from("    none — press a to add one").dim(),
        ));
    }
    for (repo, pattern) in &rules {
        items.push(ListItem::new(Line::from(format!("    {repo}  {pattern}"))));
    }
    // Selection index → list row (two header rows sit between fields and rules).
    let row = if tab.selected < FIELDS.len() {
        tab.selected
    } else {
        tab.selected + 2 + usize::from(rules.is_empty())
    };
    let mut state = ListState::default().with_selected(Some(row));
    frame.render_stateful_widget(
        List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        body,
        &mut state,
    );

    let line = if let Some(Editing::NewRule { text }) = &tab.editing {
        Line::from(vec![
            " new rule (owner/repo pattern): ".bold(),
            Span::raw(format!("{text}▏")),
            "   enter save · esc cancel".dim(),
        ])
    } else if let Some(error) = &tab.error {
        Line::from(format!(" {error}")).red()
    } else if let Some(saved) = &tab.saved {
        Line::from(format!(" {saved}")).green()
    } else if tab.editing.is_some() {
        Line::from(" enter save · esc cancel".dim())
    } else {
        Line::from(" ↑↓ select · [enter] change · [a] add file rule · [x] remove file rule".dim())
    };
    frame.render_widget(line, footer);
}

fn display(kind: Kind, value: &Value) -> String {
    match (kind, value) {
        (Kind::Bool, Value::Bool(b)) => if *b { "on" } else { "off" }.into(),
        (Kind::OptFloat, Value::Null) => "none".into(),
        (Kind::OptFloat, Value::Number(n)) => format!("${}", n),
        (Kind::OptText, Value::Null) => "default".into(),
        (Kind::Choice(_), Value::Null) => "default".into(),
        (_, Value::String(s)) => s.replace('_', " "),
        (_, other) => other.to_string(),
    }
}
