//! Text in a box: a coloured frame with a title on its top edge, wrapped to fit. Used for a
//! review's findings and for the threads and comments PRowl posted.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme;

/// One paragraph inside a box, wrapped to its width.
#[derive(Debug, Clone)]
pub struct Row {
    pub text: String,
    pub style: Style,
}

impl Row {
    pub fn plain(text: impl Into<String>) -> Self {
        Self::styled(text, Style::default())
    }

    pub fn bold(text: impl Into<String>) -> Self {
        Self::styled(text, Style::default().add_modifier(Modifier::BOLD))
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// The frame colour for a severity (critical is red, not the badge's white-on-red).
pub fn severity_color(severity: &str) -> Color {
    match severity {
        "critical" => theme::BAD,
        other => theme::severity(other).fg.unwrap_or(theme::ACCENT),
    }
}

/// A box `width` columns wide with `title` on its top edge and `rows` inside.
pub fn text_box(title: &str, color: Color, rows: &[Row], width: u16) -> Vec<Line<'static>> {
    let edge = Style::default().fg(color);
    let inner = (width as usize).saturating_sub(6).max(10);
    let label: String = format!(" {title} ").chars().take(inner).collect();
    let label_width = Span::raw(label.as_str()).width();
    let mut lines = vec![Line::from(vec![
        Span::styled(" ┌─", edge),
        Span::styled(label, edge.add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{}┐", "─".repeat((inner + 1).saturating_sub(label_width))),
            edge,
        ),
    ])];
    for row in rows {
        for text in wrap(&row.text, inner) {
            let pad = inner.saturating_sub(Span::raw(text.as_str()).width());
            lines.push(Line::from(vec![
                Span::styled(" │ ", edge),
                Span::styled(text, row.style),
                Span::raw(" ".repeat(pad)),
                Span::styled(" │", edge),
            ]));
        }
    }
    lines.push(Line::from(Span::styled(
        format!(" └{}┘", "─".repeat(inner + 2)),
        edge,
    )));
    lines
}

/// Word-wraps `text` to `width` columns, breaking words longer than a line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_owned();
        while Span::raw(word.as_str()).width() > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            let cut: String = word.chars().take(width).collect();
            word = word.chars().skip(width).collect();
            lines.push(cut);
        }
        let needed = Span::raw(current.as_str()).width()
            + usize::from(!current.is_empty())
            + Span::raw(word.as_str()).width();
        if needed > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&word);
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}
