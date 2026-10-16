//! Writing reviews to GitHub. Everything PRowl posts says it is automated and carries a
//! hidden marker naming the PR and commit, so later runs can find and update it.

use crate::decider::{Decision, Reason};
use crate::diff_lines::DiffLines;
use crate::github::{GitHub, GitHubError, InlineComment, NewReview, Submitted};
use crate::review::{Finding, ReviewOutput};

pub const LABEL_AGENT_REVIEWED: &str = "prowl:agent-reviewed";

/// What a published review is about.
pub(crate) struct Target<'a> {
    pub repo: &'a str,
    pub number: u64,
    pub head_sha: &'a str,
    pub reviewer: &'a str,
    pub policy_scope: &'a str,
    pub policy_version_id: i64,
}

pub(crate) fn marker(target: &Target<'_>) -> String {
    format!(
        "<!-- prowl:review pr={}#{} sha={} -->",
        target.repo, target.number, target.head_sha
    )
}

/// Submits an AGENT_ONLY review: a COMMENT review (never an approval) with inline notes on
/// diff lines, then labels the PR.
pub(crate) async fn agent_only(
    github: &GitHub,
    token: &str,
    target: &Target<'_>,
    review: &ReviewOutput,
) -> Result<Submitted, GitHubError> {
    let mut diff = DiffLines::default();
    for file in github.pull_files(token, target.repo, target.number).await? {
        if let Some(patch) = &file.patch {
            diff.add_patch(&file.filename, patch);
        }
    }
    let (inline, elsewhere): (Vec<&Finding>, Vec<&Finding>) = review
        .findings
        .iter()
        .partition(|f| f.line.is_some_and(|line| diff.contains(&f.file, line)));
    let body = agent_only_body(target, review, &elsewhere);
    let comments = inline
        .into_iter()
        .map(|f| InlineComment {
            path: f.file.clone(),
            line: f.line.expect("partitioned on line"),
            side: "RIGHT",
            body: finding_comment(f),
        })
        .collect();
    let submitted = github
        .create_review(
            token,
            target.repo,
            target.number,
            &NewReview {
                commit_id: target.head_sha,
                body: &body,
                event: "COMMENT",
                comments,
            },
        )
        .await?;
    github
        .ensure_label(
            token,
            target.repo,
            LABEL_AGENT_REVIEWED,
            "0e8a16",
            "Reviewed by the PRowl agent; no human review required by policy",
        )
        .await?;
    github
        .add_labels(token, target.repo, target.number, &[LABEL_AGENT_REVIEWED])
        .await?;
    Ok(submitted)
}

pub(crate) fn agent_only_body(
    target: &Target<'_>,
    review: &ReviewOutput,
    other_findings: &[&Finding],
) -> String {
    let mut body = String::new();
    body.push_str("### 🤖 PRowl automated review\n\n");
    body.push_str(&format!(
        "> Written by an AI agent (Claude, run by PRowl) on behalf of @{}. This is not a human review.\n\n",
        target.reviewer
    ));
    body.push_str(&format!("{}\n\n", review.summary.trim()));
    body.push_str(&format!(
        "**Severity:** {} · **Risk:** {}/100 · **Decision:** no human review required by policy\n\n",
        severity_word(review),
        review.risk_score
    ));
    if !review.quick_notes.is_empty() {
        body.push_str("**Quick notes**\n");
        for note in &review.quick_notes {
            body.push_str(&format!("- {}\n", note.trim()));
        }
        body.push('\n');
    }
    if !other_findings.is_empty() {
        body.push_str("**Findings**\n");
        for f in other_findings {
            let location = match f.line {
                Some(line) => format!("{}:{line}", f.file),
                None => f.file.clone(),
            };
            body.push_str(&format!(
                "- **{}** `{location}` — {}: {}\n",
                severity_name(f),
                f.title.trim(),
                f.body.trim()
            ));
            if let Some(s) = f.suggestion.as_deref().filter(|s| !s.trim().is_empty()) {
                body.push_str(&format!("  - Suggestion: {}\n", s.trim()));
            }
        }
        body.push('\n');
    }
    body.push_str(&format!(
        "<sub>Commit {} · policy {} v{}</sub>\n{}",
        short(target.head_sha),
        target.policy_scope,
        target.policy_version_id,
        marker(target)
    ));
    body
}

fn finding_comment(f: &Finding) -> String {
    let mut body = format!(
        "**{}** — {}\n\n{}",
        severity_name(f),
        f.title.trim(),
        f.body.trim()
    );
    if let Some(s) = f.suggestion.as_deref().filter(|s| !s.trim().is_empty()) {
        body.push_str(&format!("\n\n**Suggestion:** {}", s.trim()));
    }
    body.push_str("\n\n<sub>🤖 PRowl automated review</sub>");
    body
}

fn severity_word(review: &ReviewOutput) -> String {
    serde_json::to_value(review.severity)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn severity_name(f: &Finding) -> String {
    serde_json::to_value(f.severity)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// One line per reason, for logs and later the human-review banner.
pub(crate) fn describe(decision: Decision, reasons: &[Reason]) -> String {
    if decision == Decision::AgentOnly {
        return "no human review required by policy".into();
    }
    reasons
        .iter()
        .map(|r| match r {
            Reason::InvalidOutput { .. } => "the agent's review was unusable".to_owned(),
            Reason::PolicyRule { id, .. } => format!("matched [{id}]"),
            Reason::UnknownPolicyRule { id } => format!("cited unknown rule [{id}]"),
            Reason::CriticalSection => "touches critical code".to_owned(),
            Reason::Severity { severity } => format!(
                "severity {}",
                serde_json::to_value(severity)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            ),
            Reason::RiskScore { score, threshold } => format!("risk {score} ≥ {threshold}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}
