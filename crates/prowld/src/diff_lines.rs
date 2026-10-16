//! Which lines of a PR's new files can carry inline review comments: GitHub only accepts
//! comments on lines that appear in the diff (added or context lines, on the RIGHT side).

use std::collections::{HashMap, HashSet};

/// Commentable RIGHT-side line numbers per file path.
#[derive(Debug, Default, Clone)]
pub(crate) struct DiffLines(HashMap<String, HashSet<u32>>);

impl DiffLines {
    pub(crate) fn add_patch(&mut self, path: &str, patch: &str) {
        let lines = self.0.entry(path.to_owned()).or_default();
        let mut next: Option<u32> = None;
        for line in patch.lines() {
            if let Some(header) = line.strip_prefix("@@ ") {
                next = parse_new_start(header);
                continue;
            }
            let Some(n) = next else { continue };
            match line.as_bytes().first() {
                Some(b'+') | Some(b' ') => {
                    lines.insert(n);
                    next = Some(n + 1);
                }
                // Removed lines exist only on the LEFT side.
                Some(b'-') => {}
                // "\ No newline at end of file" and anything unexpected.
                _ => {}
            }
        }
    }

    pub(crate) fn contains(&self, path: &str, line: u32) -> bool {
        self.0.get(path).is_some_and(|lines| lines.contains(&line))
    }
}

/// From `-a,b +c,d @@ ...`, the `c`.
fn parse_new_start(header: &str) -> Option<u32> {
    let plus = header
        .split_whitespace()
        .find(|part| part.starts_with('+'))?;
    plus[1..].split(',').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_and_context_lines_are_commentable_removed_ones_are_not() {
        let patch = "@@ -1,4 +1,5 @@ fn main\n context\n-removed\n+added one\n+added two\n context\n@@ -20,2 +21,2 @@\n-old\n+new\n same\n\\ No newline at end of file";
        let mut diff = DiffLines::default();
        diff.add_patch("src/main.rs", patch);

        for line in [1, 2, 3, 4, 21, 22] {
            assert!(diff.contains("src/main.rs", line), "line {line}");
        }
        for line in [5, 6, 20, 23] {
            assert!(!diff.contains("src/main.rs", line), "line {line}");
        }
        assert!(!diff.contains("other.rs", 1));
    }

    #[test]
    fn new_files_start_at_line_one() {
        let mut diff = DiffLines::default();
        diff.add_patch("new.rs", "@@ -0,0 +1,2 @@\n+a\n+b");
        assert!(diff.contains("new.rs", 1) && diff.contains("new.rs", 2));
    }
}
