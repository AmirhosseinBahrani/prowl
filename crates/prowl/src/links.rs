//! Clickable links: after each draw, URLs on screen are re-written wrapped in OSC 8
//! hyperlink escapes, which most terminals turn into clickable links. The text and position
//! stay the same, so ratatui's view of the screen is unchanged.

use std::io::Write;

use ratatui::buffer::Buffer;

/// A URL on screen: its first cell and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub x: u16,
    pub y: u16,
    pub url: String,
}

/// Every `https://…` / `http://…` run on screen (ends at whitespace, a box-drawing character or
/// common trailing punctuation).
pub fn find_links(buffer: &Buffer) -> Vec<Link> {
    let area = buffer.area;
    let mut links = Vec::new();
    for y in area.top()..area.bottom() {
        let cells: Vec<&str> = (area.left()..area.right())
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        let row: String = cells.concat();
        // Map byte offsets in `row` back to cell columns (cells can hold multi-byte symbols).
        let mut starts = Vec::with_capacity(cells.len());
        let mut offset = 0;
        for cell in &cells {
            starts.push(offset);
            offset += cell.len();
        }
        let mut search = 0;
        while let Some(found) = ["https://", "http://"]
            .iter()
            .filter_map(|scheme| row[search..].find(scheme).map(|i| search + i))
            .min()
        {
            let tail = &row[found..];
            let len = tail
                .find(|c: char| {
                    c.is_whitespace() || ('\u{2500}'..='\u{257f}').contains(&c) || c == '▏'
                })
                .unwrap_or(tail.len());
            let url = tail[..len].trim_end_matches(['.', ',', ')', ';', ':', '"', '\'']);
            if url.len() > "https://".len()
                && let Some(column) = starts.iter().position(|&s| s == found)
            {
                links.push(Link {
                    x: area.left() + column as u16,
                    y,
                    url: url.to_owned(),
                });
            }
            search = found + len.max(1);
        }
    }
    links
}

/// Writes the links over themselves as OSC 8 hyperlinks.
pub fn write_hyperlinks(out: &mut impl Write, links: &[Link]) -> std::io::Result<()> {
    for link in links {
        // Only plain URL characters go inside the escape sequence.
        if link.url.chars().any(|c| c.is_control()) {
            continue;
        }
        crossterm::queue!(out, crossterm::cursor::MoveTo(link.x, link.y))?;
        write!(out, "\x1b]8;;{0}\x1b\\{0}\x1b]8;;\x1b\\", link.url)?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn buffer(lines: &[&str]) -> Buffer {
        Buffer::with_lines(lines.iter().copied())
    }

    #[test]
    fn finds_urls_with_their_positions() {
        let b = buffer(&[
            " Open https://github.com/login/device now   ",
            "│ Posted: https://github.com/a/b/pull/7#x.  │",
            " nothing here                                ",
        ]);
        assert_eq!(
            find_links(&b),
            [
                Link {
                    x: 6,
                    y: 0,
                    url: "https://github.com/login/device".into()
                },
                Link {
                    x: 10,
                    y: 1,
                    url: "https://github.com/a/b/pull/7#x".into()
                },
            ]
        );
        assert!(find_links(&Buffer::empty(Rect::new(0, 0, 5, 1))).is_empty());
    }

    #[test]
    fn writes_osc8_sequences() {
        let mut out = Vec::new();
        write_hyperlinks(
            &mut out,
            &[Link {
                x: 1,
                y: 2,
                url: "https://example.com".into(),
            }],
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("\x1b]8;;https://example.com\x1b\\https://example.com\x1b]8;;\x1b\\")
        );
    }
}
