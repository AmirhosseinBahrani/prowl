//! Colours and text styles used across the TUI, in one place. Named ANSI colours are used so
//! the palette follows the terminal's own colour scheme. With `NO_COLOR` set, colours are
//! stripped after drawing (bold/dim/reverse stay), see [`strip_colors`].

use std::sync::OnceLock;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};

pub const ACCENT: Color = Color::Cyan;
pub const KEY: Color = Color::Yellow;
pub const MUTED: Color = Color::DarkGray;
pub const GOOD: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const BAD: Color = Color::Red;
pub const MONEY: Color = Color::Magenta;

pub fn no_color() -> bool {
    static NO_COLOR: OnceLock<bool> = OnceLock::new();
    *NO_COLOR.get_or_init(|| std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()))
}

/// Removes foreground and background colours from the drawn screen (for `NO_COLOR`).
pub fn strip_colors(buffer: &mut Buffer) {
    for cell in buffer.content.iter_mut() {
        cell.set_fg(Color::Reset);
        cell.set_bg(Color::Reset);
    }
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn border() -> Style {
    Style::default().fg(MUTED)
}

/// The "PRowl" badge in the title bar.
pub fn badge(text: &str) -> Span<'static> {
    Span::styled(
        text.to_owned(),
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
    )
}

/// A key name in a hint, e.g. `[enter] `.
pub fn key(text: &str) -> Span<'static> {
    Span::styled(
        text.to_owned(),
        Style::default().fg(KEY).add_modifier(Modifier::BOLD),
    )
}

/// A hint line such as `" [e] edit · [h] history"`: keys in the key colour, the rest dim.
pub fn hints(text: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let Some(len) = rest[start..].find(']') else {
            break;
        };
        if start > 0 {
            spans.push(Span::styled(
                rest[..start].to_owned(),
                Style::default().fg(MUTED),
            ));
        }
        spans.push(key(&rest[start..=start + len]));
        rest = &rest[start + len + 1..];
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_owned(), Style::default().fg(MUTED)));
    }
    Line::from(spans)
}

/// Colour for a severity name (`critical`, `high`, …, or `unusable`).
pub fn severity(name: &str) -> Style {
    match name {
        "critical" => Style::default()
            .fg(Color::White)
            .bg(BAD)
            .add_modifier(Modifier::BOLD),
        "high" => Style::default().fg(BAD).add_modifier(Modifier::BOLD),
        "medium" => Style::default().fg(WARN),
        "low" => Style::default().fg(Color::Blue),
        "trivial" => Style::default().fg(MUTED),
        "unusable" => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        _ => Style::default(),
    }
}

/// A severity as a padded badge, e.g. ` high `.
pub fn severity_badge(name: &str) -> Span<'static> {
    Span::styled(format!(" {name} "), severity(name))
}

/// Who decided: `agent` (green) or `human` (yellow).
pub fn decision(label: &str) -> Span<'static> {
    match label {
        "agent" => label.to_owned().green().bold(),
        "human" => label.to_owned().yellow().bold(),
        other => Span::raw(other.to_owned()),
    }
}

pub fn pr_ref(text: String) -> Span<'static> {
    Span::styled(text, Style::default().fg(ACCENT))
}

pub fn time(text: String) -> Span<'static> {
    Span::styled(text, Style::default().fg(MUTED))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_colour_keys_and_dim_the_rest() {
        let line = hints(" [e] edit · [h] history");
        let parts: Vec<(&str, Option<Color>)> = line
            .spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style.fg))
            .collect();
        assert_eq!(
            parts,
            [
                (" ", Some(MUTED)),
                ("[e]", Some(KEY)),
                (" edit · ", Some(MUTED)),
                ("[h]", Some(KEY)),
                (" history", Some(MUTED)),
            ]
        );
    }
}
