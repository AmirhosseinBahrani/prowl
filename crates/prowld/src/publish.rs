//! Writing reviews to GitHub. Everything PRowl posts says it is automated and carries a
//! hidden marker naming the PR and commit, so later runs can find and update it.

use crate::decider::{Decision, Reason};
use crate::diff_lines::DiffLines;
use crate::github::{GitHub, GitHubError, InlineComment, NewReview, Submitted};
use crate::publications::Prior;
use crate::review::{Finding, ReviewOutput};

pub const LABEL_AGENT_REVIEWED: &str = "prowl:agent-reviewed";
pub const LABEL_NEEDS_HUMAN: &str = "prowl:needs-human";
pub const LABEL_HUMAN_APPROVED: &str = "prowl:human-approved";
pub const LABEL_CHANGES_REQUESTED: &str = "prowl:changes-requested";

/// `(name, colour, description)` for every label PRowl manages.
pub(crate) fn label_spec(name: &str) -> (&'static str, &'static str) {
    match name {
        LABEL_AGENT_REVIEWED => (
            "0e8a16",
            "Reviewed by the PRowl agent; no human review required by policy",
        ),
        LABEL_NEEDS_HUMAN => ("d93f0b", "PRowl: waiting for a human review before merging"),
        LABEL_HUMAN_APPROVED => ("1d76db", "PRowl: approved by a human reviewer"),
        LABEL_CHANGES_REQUESTED => ("e99695", "PRowl: a human reviewer requested changes"),
        _ => ("ededed", "PRowl"),
    }
}

/// Makes sure `label` exists in the repository and is on the PR.
pub(crate) async fn add_label(
    github: &GitHub,
    token: &str,
    repo: &str,
    number: u64,
    label: &str,
) -> Result<(), GitHubError> {
    let (color, description) = label_spec(label);
    github
        .ensure_label(token, repo, label, color, description)
        .await?;
    github.add_labels(token, repo, number, &[label]).await
}

/// What a published review is about.
pub(crate) struct Target<'a> {
    pub repo: &'a str,
    pub number: u64,
    pub head_sha: &'a str,
    pub reviewer: &'a str,
    pub policy_scope: &'a str,
    pub policy_version_id: i64,
    /// Post Claude's recommendation as the verdict of agent-only reviews.
    pub agent_verdicts: bool,
    /// The job, for runs the user asked for: a commit can then be reviewed more than once, and
    /// each run's posts are told apart by it.
    pub run: Option<i64>,
}

/// The review event for an agent-only review: Claude's recommendation when agent verdicts are
/// on, otherwise always a plain comment.
pub(crate) fn agent_event(target: &Target<'_>, review: &ReviewOutput) -> &'static str {
    use crate::review::Recommendation;
    match (target.agent_verdicts, review.recommendation) {
        (true, Recommendation::Approve) => "APPROVE",
        (true, Recommendation::RequestChanges) => "REQUEST_CHANGES",
        _ => "COMMENT",
    }
}

pub(crate) fn marker(target: &Target<'_>) -> String {
    let run = target
        .run
        .map(|id| format!(" run={id}"))
        .unwrap_or_default();
    format!(
        "<!-- prowl:review pr={}#{} sha={}{run} -->",
        target.repo, target.number, target.head_sha
    )
}

/// Submits an AGENT_ONLY review with inline notes on diff lines. It is a COMMENT review unless
/// agent verdicts are on (see [`agent_event`]). The caller labels the PR.
pub(crate) async fn agent_only(
    github: &GitHub,
    token: &str,
    target: &Target<'_>,
    review: &ReviewOutput,
) -> Result<Submitted, GitHubError> {
    let (inline, elsewhere) = split_findings(github, token, target, review).await?;
    let body = agent_only_body(target, review, &elsewhere);
    let comments = threads(&inline);
    let submitted = github
        .create_review(
            token,
            target.repo,
            target.number,
            &NewReview {
                commit_id: target.head_sha,
                body: &body,
                event: agent_event(target, review),
                comments,
            },
        )
        .await?;
    Ok(submitted)
}

/// Findings on lines the PR changed (they can be review threads) and the rest.
async fn split_findings<'r>(
    github: &GitHub,
    token: &str,
    target: &Target<'_>,
    review: &'r ReviewOutput,
) -> Result<(Vec<&'r Finding>, Vec<&'r Finding>), GitHubError> {
    let mut diff = DiffLines::default();
    for file in github.pull_files(token, target.repo, target.number).await? {
        if let Some(patch) = &file.patch {
            diff.add_patch(&file.filename, patch);
        }
    }
    Ok(review
        .findings
        .iter()
        .partition(|f| f.line.is_some_and(|line| diff.contains(&f.file, line))))
}

/// One review thread per finding, on its line.
fn threads(findings: &[&Finding]) -> Vec<InlineComment> {
    findings
        .iter()
        .map(|f| InlineComment {
            path: f.file.clone(),
            line: f
                .line
                .expect("only findings on changed lines become threads"),
            side: "RIGHT",
            body: finding_comment(f),
        })
        .collect()
}

/// Posts the findings of a review that needs a human as a COMMENT review with one thread per
/// finding, reusing one an interrupted attempt already posted. `None` when no finding is on a
/// changed line.
async fn post_findings_threads(
    github: &GitHub,
    token: &str,
    target: &Target<'_>,
    inline: &[&Finding],
) -> Result<Option<Submitted>, GitHubError> {
    if inline.is_empty() {
        return Ok(None);
    }
    let already = github
        .list_reviews(token, target.repo, target.number)
        .await?
        .into_iter()
        .find(|r| {
            authored_by(r.user.as_ref(), target.reviewer)
                && r.body
                    .as_deref()
                    .is_some_and(|b| b.contains(&marker(target)))
        });
    if let Some(r) = already {
        return Ok(Some(Submitted {
            id: r.id,
            html_url: r.html_url.unwrap_or_default(),
            body: r.body,
            state: Some(r.state),
        }));
    }
    let body = format!(
        "### 🔎 PRowl findings\n\n> Written by an AI agent (Claude, run by PRowl) on behalf of @{}. \
         This pull request **needs a human review** (see PRowl's waiting-for-approval comment). \
         Each finding below is its own thread — resolve it once it's addressed.\n\n\
         <sub>Commit {} · policy {} v{}</sub>\n{}",
        target.reviewer,
        short(target.head_sha),
        target.policy_scope,
        target.policy_version_id,
        marker(target)
    );
    github
        .create_review(
            token,
            target.repo,
            target.number,
            &NewReview {
                commit_id: target.head_sha,
                body: &body,
                event: "COMMENT",
                comments: threads(inline),
            },
        )
        .await
        .map(Some)
}

/// Whether a verification found nothing left to fix (trivial notes don't count).
pub(crate) fn verification_passed(output: &ReviewOutput) -> bool {
    output
        .findings
        .iter()
        .all(|f| f.severity == crate::review::Severity::Trivial)
}

/// Posts the result of a verification run as PRowl's review of this commit: an approval when
/// everything was addressed, otherwise a COMMENT review with a thread per remaining issue.
pub(crate) async fn publish_verification(
    github: &GitHub,
    token: &str,
    target: &Target<'_>,
    output: Result<&ReviewOutput, &str>,
    checked: usize,
) -> Result<Submitted, GitHubError> {
    let footer = format!(
        "<sub>Commit {} · policy {} v{}</sub>\n{}",
        short(target.head_sha),
        target.policy_scope,
        target.policy_version_id,
        marker(target)
    );
    let intro = format!(
        "> Written by an AI agent (Claude, run by PRowl) on behalf of @{}, after the review \
         threads were resolved: it checked the {checked} earlier finding(s) against the current \
         code and looked for new issues.\n\n",
        target.reviewer
    );
    let (event, body, comments) = match output {
        Ok(output) if verification_passed(output) => (
            "APPROVE",
            format!(
                "### ✅ PRowl verification passed\n\n{intro}{}\n\nEverything was addressed and \
                 nothing new turned up, so this pull request is approved.\n\n{footer}",
                output.summary.trim()
            ),
            Vec::new(),
        ),
        Ok(output) => {
            let (inline, elsewhere) = split_findings(github, token, target, output).await?;
            let mut body = format!(
                "### 🔁 PRowl verification: {} issue(s) remain\n\n{intro}{}\n\n",
                output.findings.len(),
                output.summary.trim()
            );
            if !inline.is_empty() {
                body.push_str(&format!(
                    "**{} posted as threads** on the changed lines — resolve them once they're \
                     fixed and PRowl checks again.\n\n",
                    inline.len()
                ));
            }
            push_findings(&mut body, &elsewhere);
            body.push_str(&footer);
            ("COMMENT", body, threads(&inline))
        }
        Err(problem) => (
            "COMMENT",
            format!(
                "### ⚠️ PRowl verification couldn't finish\n\n{intro}The agent's answer was \
                 unusable ({problem}), so nothing was approved. Ask for another check or review it \
                 yourself.\n\n{footer}"
            ),
            Vec::new(),
        ),
    };
    github
        .create_review(
            token,
            target.repo,
            target.number,
            &NewReview {
                commit_id: target.head_sha,
                body: &body,
                event,
                comments,
            },
        )
        .await
}

/// Publishes a decided review, reusing what PRowl posted on this PR before (`prior`, updated
/// in place to describe what is now posted):
/// - AGENT_ONLY: a new COMMENT review (inline notes need the new commit). The previous agent
///   review is marked superseded, and a waiting-for-approval comment is marked resolved.
/// - NEEDS_HUMAN: findings on changed lines as a COMMENT review with a thread each, then the
///   waiting-for-approval comment (edited if it exists) with the reasons and the remaining
///   findings. A previous PRowl review is marked superseded and its label removed.
pub(crate) async fn publish_result(
    github: &GitHub,
    token: &str,
    target: &Target<'_>,
    decision: Decision,
    review: Result<&ReviewOutput, &str>,
    reasons: &[Reason],
    prior: &mut Prior,
) -> Result<Submitted, GitHubError> {
    match (decision, review) {
        (Decision::AgentOnly, Ok(review)) => {
            let existing = github
                .list_reviews(token, target.repo, target.number)
                .await?;
            // An interrupted earlier attempt may already have posted this exact review.
            let already = existing
                .iter()
                .find(|r| {
                    // Anyone can paste a marker into a review; only ours count.
                    authored_by(r.user.as_ref(), target.reviewer)
                        && r.body
                            .as_deref()
                            .is_some_and(|b| b.contains(&marker(target)))
                })
                .cloned();
            let posted = match already {
                Some(r) => Submitted {
                    id: r.id,
                    html_url: r.html_url.unwrap_or_default(),
                    body: r.body,
                    state: Some(r.state),
                },
                None => agent_only(github, token, target, review).await?,
            };
            add_label(
                github,
                token,
                target.repo,
                target.number,
                LABEL_AGENT_REVIEWED,
            )
            .await?;
            if let Some((old, _)) = prior.agent_review.take() {
                let body = superseded_body(target, &posted.html_url);
                ignore_missing(
                    github
                        .update_review(token, target.repo, target.number, old, &body)
                        .await,
                )?;
                withdraw_verdict(github, token, target, &existing, old, &posted.html_url).await?;
            }
            if let Some((comment, _)) = &prior.comment {
                let body = resolved_body(target, &posted.html_url);
                ignore_missing(
                    github
                        .update_issue_comment(token, target.repo, *comment, &body)
                        .await,
                )?;
                github
                    .remove_label(token, target.repo, target.number, LABEL_NEEDS_HUMAN)
                    .await?;
            }
            prior.agent_review = Some((posted.id, posted.html_url.clone()));
            Ok(posted)
        }
        _ => {
            if prior.comment.is_none() {
                // Our record is gone (e.g. a crash right after posting): find it by its marker.
                let pr_marker = format!("<!-- prowl:review pr={}#{} ", target.repo, target.number);
                prior.comment = github
                    .list_issue_comments(token, target.repo, target.number)
                    .await?
                    .into_iter()
                    .find(|c| {
                        authored_by(c.user.as_ref(), target.reviewer)
                            && c.body.as_deref().is_some_and(|b| {
                                b.contains(&pr_marker) && b.contains("Waiting for human approval")
                            })
                    })
                    .map(|c| (c.id, c.html_url));
            }
            let (inline, elsewhere) = match review {
                Ok(review) => split_findings(github, token, target, review).await?,
                Err(_) => (Vec::new(), Vec::new()),
            };
            let findings_review = post_findings_threads(github, token, target, &inline).await?;
            let threads_note = findings_review
                .as_ref()
                .map(|r| (inline.len(), r.html_url.as_str()));
            let body = needs_human_body(target, review.ok(), reasons, &elsewhere, threads_note);
            let existing = match &prior.comment {
                Some((id, _)) => ignore_missing(
                    github
                        .update_issue_comment(token, target.repo, *id, &body)
                        .await,
                )?,
                None => None,
            };
            let comment = match existing {
                Some(updated) => updated,
                // First time, or someone deleted PRowl's comment.
                None => {
                    github
                        .create_issue_comment(token, target.repo, target.number, &body)
                        .await?
                }
            };
            add_label(github, token, target.repo, target.number, LABEL_NEEDS_HUMAN).await?;
            if let Some((old, _)) = prior
                .agent_review
                .take()
                .filter(|(old, _)| Some(*old) != findings_review.as_ref().map(|r| r.id))
            {
                let body = superseded_body(target, &comment.html_url);
                ignore_missing(
                    github
                        .update_review(token, target.repo, target.number, old, &body)
                        .await,
                )?;
                // An earlier agent approval must not outlive a result that needs a human.
                let existing = github
                    .list_reviews(token, target.repo, target.number)
                    .await?;
                withdraw_verdict(github, token, target, &existing, old, &comment.html_url).await?;
                github
                    .remove_label(token, target.repo, target.number, LABEL_AGENT_REVIEWED)
                    .await?;
            }
            prior.comment = Some((comment.id, comment.html_url.clone()));
            // The findings review is PRowl's review of this commit; a later one supersedes it.
            prior.agent_review = findings_review.map(|r| (r.id, r.html_url));
            Ok(comment)
        }
    }
}

/// Dismisses PRowl's earlier agent review `old` if it approved or requested changes, so a stale
/// verdict doesn't keep counting. Needs write access; without it the verdict stays, marked
/// superseded, and a warning is logged.
async fn withdraw_verdict(
    github: &GitHub,
    token: &str,
    target: &Target<'_>,
    reviews: &[crate::github::SubmittedReview],
    old: i64,
    replaced_by: &str,
) -> Result<(), GitHubError> {
    let Some(review) = reviews.iter().find(|r| r.id == old) else {
        return Ok(());
    };
    if !authored_by(review.user.as_ref(), target.reviewer)
        || !matches!(review.state.as_str(), "APPROVED" | "CHANGES_REQUESTED")
    {
        return Ok(());
    }
    let message = format!("Superseded by PRowl's review of a newer commit: {replaced_by}");
    match github
        .dismiss_review(token, target.repo, target.number, old, &message)
        .await
    {
        Ok(()) => Ok(()),
        Err(GitHubError::Status { status, body }) => {
            tracing::warn!(
                repo = target.repo,
                pr = target.number,
                %status,
                body,
                "could not dismiss PRowl's earlier verdict"
            );
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Whether a GitHub post was written by `login` — the account PRowl posts as.
fn authored_by(user: Option<&crate::github::ApiOwner>, login: &str) -> bool {
    user.is_some_and(|u| u.login.eq_ignore_ascii_case(login))
}

/// Treats "it's gone" (someone deleted it) as nothing to update.
fn ignore_missing<T>(result: Result<T, GitHubError>) -> Result<Option<T>, GitHubError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(GitHubError::Status { status, .. }) if status == reqwest::StatusCode::NOT_FOUND => {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

fn superseded_body(target: &Target<'_>, latest_url: &str) -> String {
    format!(
        "### 🤖 PRowl automated review — superseded\n\n\
         This review is out of date: PRowl reviewed a newer commit ({}). \
         [See the latest result]({latest_url}).\n{}",
        short(target.head_sha),
        marker(target)
    )
}

fn resolved_body(target: &Target<'_>, review_url: &str) -> String {
    format!(
        "### ✅ No longer waiting for human approval\n\n\
         PRowl reviewed commit {} and, under the review policy, it no longer needs a human \
         review. [See the agent's review]({review_url}).\n{}",
        short(target.head_sha),
        marker(target)
    )
}

/// The waiting-for-approval comment. `listed` are the findings written out here; with
/// `threads` = `(count, review url)`, the others were posted as review threads.
pub(crate) fn needs_human_body(
    target: &Target<'_>,
    review: Option<&ReviewOutput>,
    reasons: &[Reason],
    listed: &[&Finding],
    threads: Option<(usize, &str)>,
) -> String {
    let mut body = String::new();
    body.push_str(
        "### ⏳ Waiting for human approval

",
    );
    body.push_str(&format!(
        "> 🤖 PRowl's AI agent reviewed this pull request on behalf of @{}. Under the review \
         policy it **needs a human review before merging**:\n",
        target.reviewer
    ));
    for reason in reasons {
        body.push_str(&format!("> - {}\n", reason_line(reason)));
    }
    body.push('\n');
    match review {
        Some(review) => {
            body.push_str(&format!(
                "**Agent's summary:** {}\n\n",
                review.summary.trim()
            ));
            body.push_str(&format!(
                "**Severity:** {} · **Risk:** {}/100\n\n",
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
            if let Some((count, url)) = threads {
                body.push_str(&format!(
                    "**Findings:** {count} posted as [review threads]({url}) on the changed lines.\n\n"
                ));
            }
            push_findings(&mut body, listed);
        }
        None if reasons.iter().any(|r| matches!(r, Reason::TooLarge { .. })) => body.push_str(
            "PRowl did not run the agent on this pull request, so there are no automated notes.\n\n",
        ),
        None => body.push_str(
            "The agent could not produce a usable review, so there are no automated notes.\n\n",
        ),
    }
    body.push_str(&format!(
        "<sub>Commit {} · policy {} v{} · this comment is automated; the approval must come from a person.</sub>\n{}",
        short(target.head_sha),
        target.policy_scope,
        target.policy_version_id,
        marker(target)
    ));
    body
}

fn reason_line(reason: &Reason) -> String {
    match reason {
        Reason::InvalidOutput { detail } => format!("The automated review was unusable ({detail})"),
        Reason::PolicyRule { id, evidence } if evidence.trim().is_empty() => {
            format!("Matched **[{id}]**")
        }
        Reason::PolicyRule { id, evidence } => format!("Matched **[{id}]** — {}", evidence.trim()),
        Reason::UnknownPolicyRule { id } => format!("Cited **[{id}]**, which is not in the policy"),
        Reason::CriticalSection => "Touches code the policy treats as critical".to_owned(),
        Reason::Severity { severity } => format!(
            "Severity **{}**",
            serde_json::to_value(severity)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        ),
        Reason::FilePattern { pattern, file } => {
            format!("Changes `{file}`, which matches the safety rule `{pattern}`")
        }
        Reason::TooLarge {
            changed_lines,
            changed_files,
            max_lines,
            max_files,
        } => format!(
            "Too large for an automated review: {changed_lines} changed lines in {changed_files} files \
             (limits: {max_lines} lines, {max_files} files)"
        ),
        Reason::RiskScore { score, threshold } => {
            format!("Risk score **{score}** (threshold {threshold})")
        }
    }
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
        "**Severity:** {} · **Risk:** {}/100 · **Decision:** no human review required by policy{}\n\n",
        severity_word(review),
        review.risk_score,
        match agent_event(target, review) {
            "APPROVE" => " · **Verdict:** approved by the agent",
            "REQUEST_CHANGES" => " · **Verdict:** changes requested by the agent",
            _ => "",
        }
    ));
    if !review.quick_notes.is_empty() {
        body.push_str("**Quick notes**\n");
        for note in &review.quick_notes {
            body.push_str(&format!("- {}\n", note.trim()));
        }
        body.push('\n');
    }
    push_findings(&mut body, other_findings);
    body.push_str(&format!(
        "<sub>Commit {} · policy {} v{}</sub>\n{}",
        short(target.head_sha),
        target.policy_scope,
        target.policy_version_id,
        marker(target)
    ));
    body
}

fn push_findings(body: &mut String, findings: &[&Finding]) {
    if findings.is_empty() {
        return;
    }
    body.push_str("**Findings**\n");
    for f in findings {
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
            Reason::FilePattern { pattern, .. } => format!("file rule {pattern}"),
            Reason::TooLarge {
                changed_lines,
                changed_files,
                ..
            } => {
                format!("too large for the agent ({changed_lines} lines, {changed_files} files)")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}
