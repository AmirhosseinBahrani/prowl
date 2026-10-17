//! Rendering for the Reviews tab.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Row, Table, TableState, Wrap};
use serde_json::Value;

use crate::feed_tab::{clock, duration};
use crate::reviews_tab::{OpenReview, ReviewsTab};
use crate::theme;

/// Lines of a tool's output shown under the call.
const RESULT_PREVIEW: usize = 6;

pub fn draw(tab: &ReviewsTab, frame: &mut Frame, area: Rect) {
    let [body, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
    if let Some(open) = &tab.open {
        draw_open(tab, open, frame, body);
        frame.render_widget(
            footer_line(
                tab,
                " ↑↓ scroll · [space/p] page · [g/G] top/end · [o] open · [R] re-review · [x] cancel · [esc] back",
            ),
            footer,
        );
        return;
    }
    let jobs = tab.ordered();
    if jobs.is_empty() {
        let text = if tab.loaded {
            "  No reviews yet. PRowl lists them here as soon as a pull request is picked up."
        } else {
            "  Loading…"
        };
        frame.render_widget(Paragraph::new(text).dim(), body);
    } else {
        let rows = jobs.iter().map(|job| {
            let review = tab.review_of(job.id);
            Row::new(vec![
                Line::from(status_badge(&job.status)),
                Line::from(theme::pr_ref(format!("{}#{}", job.repo, job.pr_number))),
                Line::from(job.title.clone()),
                Line::from(outcome(review)),
                Line::from(
                    review
                        .and_then(|r| r.cost_usd)
                        .map(|c| format!("${c:.2}"))
                        .unwrap_or_default(),
                ),
                Line::from(theme::time(clock(job.created_at_ms))),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(12),
                Constraint::Length(18),
                Constraint::Min(10),
                Constraint::Length(16),
                Constraint::Length(6),
                Constraint::Length(8),
            ],
        )
        .header(Row::new(["status", "PR", "title", "outcome", "cost", "queued"]).bold())
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = TableState::default().with_selected(Some(tab.selected));
        frame.render_stateful_widget(table, body, &mut state);
    }
    frame.render_widget(
        footer_line(
            tab,
            " ↑↓ select · [enter] conversation & result · [o] open · [R] re-review · [x] cancel · [r] refresh",
        ),
        footer,
    );
}

/// The key hints, or the "cancel this review?" question while it waits for an answer.
fn footer_line(tab: &ReviewsTab, hints: &str) -> Line<'static> {
    match tab.confirm_cancel {
        Some(_) => Line::from(vec![
            " Stop this review? Claude is stopped and nothing more is posted. "
                .bold()
                .yellow(),
            "[y] yes  any other key: no".into(),
        ]),
        None => theme::hints(hints),
    }
}

fn draw_open(tab: &ReviewsTab, open: &OpenReview, frame: &mut Frame, area: Rect) {
    let mut lines = header_lines(tab, open.job_id);
    lines.push(Line::from(""));
    // The findings first, each in its own box; the conversation below.
    match current_findings(tab, open) {
        Some(findings) => {
            lines.push(section(&format!("Findings ({})", findings.len())));
            if findings.is_empty() {
                lines.push(Line::from("  No findings.").dim());
            }
            for finding in &findings {
                lines.extend(finding_box(finding, area.width));
            }
        }
        None if open.loaded => {
            lines.push(section("Findings"));
            lines.push(Line::from("  None yet — they appear when Claude answers.").dim());
        }
        None => {}
    }
    lines.push(Line::from(""));
    lines.push(section("Conversation & context"));
    if !open.loaded {
        lines.push(Line::from("  Loading…").dim());
    } else if open.transcript.is_empty() {
        let running = tab
            .job(open.job_id)
            .is_some_and(|j| matches!(j.status.as_str(), "queued" | "interrupted"));
        lines.push(
            Line::from(if running {
                "  Waiting to start. The conversation appears here live."
            } else {
                "  No conversation was recorded for this review (it ran before PRowl kept them, \
                 or it never reached Claude)."
            })
            .dim(),
        );
    } else {
        lines.extend(transcript_lines(&open.transcript));
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(area.width);
    let max = total.saturating_sub(area.height as usize);
    open.max_scroll.set(max);
    open.page.set(area.height.saturating_sub(1) as usize);
    let scroll = if open.follow {
        max
    } else {
        open.scroll.min(max)
    };
    frame.render_widget(
        paragraph.scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        area,
    );
}

/// The findings to show: the stored review's, or while it runs, Claude's latest valid answer.
fn current_findings(tab: &ReviewsTab, open: &OpenReview) -> Option<Vec<prowld::review::Finding>> {
    if let Some(output) = tab.review_of(open.job_id).and_then(|r| r.output.as_ref()) {
        return Some(output.findings.clone());
    }
    open.transcript
        .iter()
        .rev()
        .filter(|e| e.kind == "result" && e.content["valid"] == Value::Bool(true))
        .find_map(|e| serde_json::from_value(e.content["output"]["findings"].clone()).ok())
}

/// One finding drawn as a box `width` columns wide: severity and place on the top edge, then
/// the title, the explanation and the suggestion, wrapped to fit.
pub fn finding_box(finding: &prowld::review::Finding, width: u16) -> Vec<Line<'static>> {
    let severity = format!("{:?}", finding.severity).to_lowercase();
    let place = match finding.line {
        Some(line) => format!("{}:{line}", finding.file),
        None => finding.file.clone(),
    };
    let mut rows = vec![crate::boxes::Row::bold(finding.title.trim())];
    rows.extend(finding.body.trim().lines().map(crate::boxes::Row::plain));
    if let Some(suggestion) = finding
        .suggestion
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        rows.push(crate::boxes::Row::styled(
            format!("Suggestion: {}", suggestion.trim()),
            Style::default().fg(theme::GOOD),
        ));
    }
    crate::boxes::text_box(
        &format!("{} · {place}", severity.to_uppercase()),
        crate::boxes::severity_color(&severity),
        &rows,
        width,
    )
}

/// The PR, the job's state and, once decided, the outcome.
pub fn header_lines(tab: &ReviewsTab, job_id: i64) -> Vec<Line<'static>> {
    let Some(job) = tab.job(job_id) else {
        return vec![Line::from(" This review is no longer listed.").dim()];
    };
    let mut lines = vec![
        Line::from(vec![
            Span::raw(" "),
            theme::pr_ref(format!("{}#{}", job.repo, job.pr_number)),
            Span::raw(format!(" — {}", job.title)).bold(),
            Span::raw(format!("  by @{}", job.author)).dim(),
        ]),
        Line::from(vec![
            Span::raw(" "),
            status_badge(&job.status),
            Span::raw(format!(
                "  attempt {} · commit {}",
                job.attempts,
                job.head_sha.chars().take(7).collect::<String>()
            ))
            .dim(),
        ]),
    ];
    if let Some(error) = &job.error {
        lines.push(Line::from(format!(" {error}")).red());
    }
    if let Some(review) = tab.review_of(job_id) {
        let mut parts = vec![Span::raw(" "), outcome(Some(review))];
        if let Some(output) = &review.output {
            parts.push(Span::raw(" · "));
            parts.push(theme::severity_badge(
                &format!("{:?}", output.severity).to_lowercase(),
            ));
            parts.push(Span::raw(format!(" · risk {}", output.risk_score)));
        }
        if let Some(cost) = review.cost_usd {
            parts.push(Span::raw(format!(" · ${cost:.2}")));
        }
        parts.push(Span::raw(format!(
            " · took {}",
            duration(review.duration_ms)
        )));
        lines.push(Line::from(parts));
        if let Some(url) = &review.published_url {
            lines.push(Line::from(vec![
                Span::raw(" posted: "),
                Span::raw(url.clone()).underlined(),
            ]));
        }
    }
    lines
}

/// The whole transcript as readable lines.
pub fn transcript_lines(entries: &[prowld::transcripts::TranscriptEntry]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let attempts = entries.iter().map(|e| e.attempt).max().unwrap_or(1);
    let mut attempt = None;
    for entry in entries {
        if attempts > 1 && attempt != Some(entry.attempt) {
            attempt = Some(entry.attempt);
            lines.push(section(&format!("Attempt {}", entry.attempt)));
        }
        let c = &entry.content;
        let at = theme::time(clock(entry.at_ms));
        match entry.kind.as_str() {
            "context" => {
                lines.push(section("Context given to Claude"));
                lines.push(Line::from(format!(
                    " policy {} v{} · model {} · commit {} · base {}",
                    text(c, "policy_scope"),
                    c["policy_version_id"],
                    c["model"].as_str().unwrap_or("CLI default"),
                    text(c, "head_sha").chars().take(7).collect::<String>(),
                    text(c, "base"),
                )));
                let files: Vec<&str> = c["material_files"]
                    .as_array()
                    .map(|f| f.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                let tools: Vec<&str> = c["tools"]
                    .as_array()
                    .map(|t| t.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                lines.push(Line::from(format!(
                    " prepared files: {} · tools: {} (read-only)",
                    files.join(", "),
                    tools.join(", ")
                )));
                lines.push(Line::from(" first message:").bold());
                quote(&mut lines, &text(c, "prompt"));
            }
            "repair" => {
                lines.push(section("PRowl asked Claude to fix its answer"));
                quote(&mut lines, &text(c, "prompt"));
            }
            "assistant" => {
                let body = text(c, "text");
                let mut first = true;
                for line in body.lines() {
                    if first {
                        lines.push(Line::from(vec![
                            at.clone(),
                            Span::raw(" Claude: ").bold(),
                            Span::raw(line.to_owned()),
                        ]));
                        first = false;
                    } else {
                        lines.push(Line::from(format!("          {line}")));
                    }
                }
            }
            "tool_use" => {
                lines.push(Line::from(vec![
                    at,
                    Span::raw(" → "),
                    theme::key(&text(c, "name")),
                    Span::raw(format!(" {}", tool_target(&c["input"]))),
                ]));
            }
            "tool_result" => {
                let body = text(c, "content");
                let all: Vec<&str> = body.lines().collect();
                let style = if c["is_error"] == Value::Bool(true) {
                    Style::default().fg(theme::BAD)
                } else {
                    Style::default().fg(theme::MUTED)
                };
                for line in all.iter().take(RESULT_PREVIEW) {
                    lines.push(Line::from(Span::styled(
                        format!("            ↳ {line}"),
                        style,
                    )));
                }
                if all.len() > RESULT_PREVIEW {
                    lines.push(
                        Line::from(format!(
                            "            … {} more lines",
                            all.len() - RESULT_PREVIEW
                        ))
                        .dim(),
                    );
                }
            }
            "result" => {
                let mut head = vec![Span::raw(" ")];
                if c["valid"] == Value::Bool(true) {
                    head.push(Span::raw("valid answer").fg(theme::GOOD));
                } else {
                    head.push(Span::raw("unusable answer").fg(theme::BAD));
                }
                if let Some(cost) = c["cost_usd"].as_f64() {
                    head.push(Span::raw(format!(" · ${cost:.2}")));
                }
                if let Some(ms) = c["duration_ms"].as_i64() {
                    head.push(Span::raw(format!(" · {}", duration(ms))));
                }
                lines.push(section("Claude's answer"));
                lines.push(Line::from(head));
                if c["valid"] == Value::Bool(true) {
                    review_lines(&mut lines, &c["output"]);
                } else {
                    lines.push(Line::from(format!(" {}", text(c, "invalid_reason"))).red());
                    quote(&mut lines, &text(c, "text"));
                }
            }
            "decision" => {
                lines.push(section("PRowl's decision"));
                let agent = text(c, "decision") == "AGENT_ONLY";
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    theme::decision(if agent { "agent" } else { "human" }),
                    Span::raw(format!(" {}", text(c, "summary"))),
                ]));
            }
            "published" => {
                lines.push(section("Posted to GitHub"));
                let what = match (text(c, "as").as_str(), text(c, "state").as_str()) {
                    ("review", "APPROVED") => "approval review".to_owned(),
                    ("review", "CHANGES_REQUESTED") => "changes-requested review".to_owned(),
                    ("review", _) => "comment review".to_owned(),
                    _ => "waiting-for-approval comment".to_owned(),
                };
                lines.push(Line::from(vec![
                    Span::raw(format!(" {what}: ")),
                    Span::raw(text(c, "url")).underlined(),
                ]));
                quote(&mut lines, &text(c, "body"));
            }
            "error" => {
                lines.push(section("Failed"));
                lines.push(Line::from(format!(" {}", text(c, "message"))).red());
            }
            other => lines.push(Line::from(format!(" {other}: {c}")).dim()),
        }
    }
    lines
}

/// Severity, risk, rules and findings of a valid answer.
fn review_lines(lines: &mut Vec<Line<'static>>, output: &Value) {
    lines.push(Line::from(format!(" {}", text(output, "summary"))));
    lines.push(Line::from(vec![
        Span::raw(" "),
        theme::severity_badge(&text(output, "severity")),
        Span::raw(format!(
            " · risk {} · recommends {}",
            output["risk_score"],
            text(output, "recommendation").replace('_', " ")
        )),
    ]));
    for rule in output["matched_policy_rules"]
        .as_array()
        .into_iter()
        .flatten()
    {
        lines.push(Line::from(format!(
            " matched [{}] {}",
            text(rule, "id"),
            text(rule, "evidence")
        )));
    }
    for note in output["quick_notes"].as_array().into_iter().flatten() {
        lines.push(Line::from(format!(
            " - {}",
            note.as_str().unwrap_or_default()
        )));
    }
    for finding in output["findings"].as_array().into_iter().flatten() {
        let at = finding["line"]
            .as_u64()
            .map(|l| format!(":{l}"))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::raw(" "),
            theme::severity_badge(&text(finding, "severity")),
            Span::raw(format!(
                " {}{at} {}",
                text(finding, "file"),
                text(finding, "title")
            )),
        ]));
    }
}

/// What Claude pointed a tool at: the file, or the pattern and where.
fn tool_target(input: &Value) -> String {
    match (
        input["file_path"].as_str(),
        input["pattern"].as_str(),
        input["path"].as_str(),
    ) {
        (Some(file), _, _) => file.to_owned(),
        (None, Some(pattern), Some(path)) => format!("{pattern:?} in {path}"),
        (None, Some(pattern), None) => format!("{pattern:?}"),
        (None, None, Some(path)) => path.to_owned(),
        _ => input.to_string(),
    }
}

fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(format!(" ── {title} ──"), theme::accent()))
}

/// Long text, indented and dimmed so it reads as quoted material.
fn quote(lines: &mut Vec<Line<'static>>, body: &str) {
    for line in body.lines() {
        lines.push(Line::from(format!("   │ {line}")).dim());
    }
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

fn status_badge(status: &str) -> Span<'static> {
    let (label, color) = match status {
        "running" => ("● reviewing", theme::ACCENT),
        "queued" | "interrupted" => ("◌ waiting", theme::WARN),
        "done" => ("✓ done", theme::GOOD),
        "failed" => ("✗ failed", theme::BAD),
        "cancelled" => ("– cancelled", theme::MUTED),
        other => return Span::raw(other.to_owned()).dim(),
    };
    Span::styled(label, Style::default().fg(color))
}

/// Who the review was left to, from the stored decision.
fn outcome(review: Option<&prowld::Review>) -> Span<'static> {
    match review.and_then(|r| r.decision) {
        Some(prowld::Decision::AgentOnly) => theme::decision("agent"),
        Some(prowld::Decision::NeedsHuman) => theme::decision("human"),
        None if review.is_some() => Span::raw("deciding…").dim(),
        None => Span::raw(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prowld::transcripts::TranscriptEntry;
    use serde_json::json;

    fn entry(kind: &str, content: Value) -> TranscriptEntry {
        TranscriptEntry {
            id: 0,
            job_id: 1,
            attempt: 1,
            at_ms: 0,
            kind: kind.into(),
            content,
        }
    }

    fn plain(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_transcript_reads_from_context_to_what_was_posted() {
        let text = plain(&transcript_lines(&[
            entry(
                "context",
                json!({ "prompt": "<role>\nYou are PRowl", "policy_scope": "global",
                        "policy_version_id": 3, "head_sha": "abcdef123", "base": "main@1",
                        "material_files": ["commits.txt", "pr.diff"], "tools": ["Read"] }),
            ),
            entry("assistant", json!({ "text": "Let me read the diff." })),
            entry(
                "tool_use",
                json!({ "name": "Grep", "input": { "pattern": "unsafe", "path": "src" } }),
            ),
            entry(
                "tool_result",
                json!({ "content": "1\n2\n3\n4\n5\n6\n7\n8", "is_error": false }),
            ),
            entry(
                "result",
                json!({ "valid": true, "cost_usd": 0.07, "duration_ms": 4200, "output": {
                    "summary": "Small fix.", "severity": "low", "risk_score": 10,
                    "recommendation": "approve", "matched_policy_rules": [],
                    "quick_notes": [], "findings": [{ "severity": "low", "file": "a.rs",
                    "line": 3, "title": "Typo" }] } }),
            ),
            entry(
                "decision",
                json!({ "decision": "AGENT_ONLY", "summary": "no human review required" }),
            ),
            entry(
                "published",
                json!({ "as": "review", "state": "APPROVED",
                        "url": "https://github.com/acme/api/pull/7#r1", "body": "Looks good" }),
            ),
        ]));
        for expected in [
            "── Context given to Claude ──",
            "policy global v3 · model CLI default · commit abcdef1 · base main@1",
            "prepared files: commits.txt, pr.diff · tools: Read (read-only)",
            "│ <role>",
            "Claude: Let me read the diff.",
            "→ Grep \"unsafe\" in src",
            "↳ 6",
            "… 2 more lines",
            "valid answer · $0.07 · 4s",
            "recommends approve",
            "a.rs:3 Typo",
            "no human review required",
            "approval review: https://github.com/acme/api/pull/7#r1",
            "│ Looks good",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        assert!(
            !text.contains("↳ 7"),
            "only the first lines of a tool result"
        );
    }

    #[test]
    fn each_finding_is_a_closed_box_of_even_width() {
        let finding: prowld::review::Finding = serde_json::from_value(json!({
            "file": "src/login.rs", "line": 12, "severity": "high",
            "title": "Token logged",
            "body": "The session token is written to the debug log on every request, which \
                     anyone with log access can read. averyveryveryveryveryveryverylongidentifier",
            "suggestion": "Redact it before logging."
        }))
        .unwrap();

        let lines = finding_box(&finding, 50);

        let widths: Vec<usize> = lines.iter().map(Line::width).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        let text = plain(&lines);
        assert!(text.starts_with(" ┌─ HIGH · src/login.rs:12 ─"), "{text}");
        assert!(text.contains("│ Token logged"));
        assert!(text.contains("Suggestion: Redact it before logging."));
        assert!(text.lines().last().unwrap().starts_with(" └"));
        assert!(text.lines().count() > 5, "the explanation wraps:\n{text}");
    }

    #[test]
    fn retries_are_labelled_by_attempt() {
        let mut second = entry("error", json!({ "message": "claude timed out" }));
        second.attempt = 2;
        let text = plain(&transcript_lines(&[
            entry("error", json!({ "message": "network down" })),
            second,
        ]));
        assert!(text.contains("── Attempt 1 ──"));
        assert!(text.contains("── Attempt 2 ──"));
        assert!(text.contains("claude timed out"));
    }
}
