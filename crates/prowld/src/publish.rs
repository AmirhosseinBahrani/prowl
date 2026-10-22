//! Writing reviews to GitHub. Everything PRowl posts says it is automated and carries a
//! hidden marker naming the PR and commit, so later runs can find and update it.

use crate::decider::{Decision, Reason};
use crate::diff_lines::DiffLines;
use crate::github::{GitHub, GitHubError, InlineComment, NewReview, ReviewThread, Submitted};
use crate::publications::Prior;
use crate::review::{Finding, ReviewOutput};

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
/// agent verdicts are on (see [`agent_event`]).
pub(crate) async fn agent_only(
    github: &GitHub,
    token: &str,
    target: &Target<'_>,
    review: &ReviewOutput,
) -> Result<Submitted, GitHubError> {
    let split = split_findings(github, token, target, review, Covered::Any).await?;
    let body = agent_only_body(target, review, &split.elsewhere, split.already);
    let comments = threads(&split.inline);
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

/// Which existing threads count as already raising a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Covered {
    /// Any thread, open, answered or resolved: it was said, and a reply or resolution settles it.
    Any,
    /// Only open ones — a verification re-raises what was resolved without being fixed.
    Open,
}

/// A review's findings, sorted for posting.
struct Split<'r> {
    /// On lines the PR changed and not raised before: they become review threads.
    inline: Vec<&'r Finding>,
    /// Elsewhere and not raised before: listed in the review's body.
    elsewhere: Vec<&'r Finding>,
    /// How many were left out because a thread already raises them.
    already: usize,
    /// How many were on changed lines before leaving those out.
    inline_total: usize,
}

/// Sorts a review's findings into threads to post and the rest, leaving out what a thread on
/// the PR already raises (by anyone).
async fn split_findings<'r>(
    github: &GitHub,
    token: &str,
    target: &Target<'_>,
    review: &'r ReviewOutput,
    covered: Covered,
) -> Result<Split<'r>, GitHubError> {
    let mut diff = DiffLines::default();
    for file in github.pull_files(token, target.repo, target.number).await? {
        if let Some(patch) = &file.patch {
            diff.add_patch(&file.filename, patch);
        }
    }
    let existing = if review.findings.is_empty() {
        Vec::new()
    } else {
        github
            .review_threads(token, target.repo, target.number)
            .await?
    };
    let mut split = Split {
        inline: Vec::new(),
        elsewhere: Vec::new(),
        already: 0,
        inline_total: 0,
    };
    for f in &review.findings {
        let on_diff = f.line.is_some_and(|line| diff.contains(&f.file, line));
        split.inline_total += usize::from(on_diff);
        if already_raised(f, &existing, covered) {
            split.already += 1;
        } else if on_diff {
            split.inline.push(f);
        } else {
            split.elsewhere.push(f);
        }
    }
    Ok(split)
}

/// How far apart (in lines) a finding and a thread about the same issue can be: the code may
/// have moved since the thread was opened.
const NEARBY_LINES: u32 = 5;

/// Whether a thread already raises this finding: the reviewer said so, or a thread on the same
/// file near the same line opens with the finding's title (PRowl's earlier post of it).
fn already_raised(f: &Finding, threads: &[ReviewThread], covered: Covered) -> bool {
    if f.already_raised {
        return true;
    }
    let title = f.title.trim();
    if title.is_empty() {
        return false;
    }
    threads.iter().any(|t| {
        let counts = covered == Covered::Any || !t.resolved;
        let near = match (f.line, t.line) {
            (Some(a), Some(b)) => a.abs_diff(b) <= NEARBY_LINES,
            // Outdated threads lose their line; findings off the diff have none.
            _ => true,
        };
        counts && near && t.path == f.file && t.body.contains(title)
    })
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
    split: &Split<'_>,
) -> Result<Option<Submitted>, GitHubError> {
    if split.inline_total == 0 {
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
    // Everything on the changed lines was raised before.
    if split.inline.is_empty() {
        return Ok(None);
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
                comments: threads(&split.inline),
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
            let split = split_findings(github, token, target, output, Covered::Open).await?;
            let mut body = format!(
                "### 🔁 PRowl verification: {} issue(s) remain\n\n{intro}{}\n\n",
                output.findings.len(),
                output.summary.trim()
            );
            if !split.inline.is_empty() {
                body.push_str(&format!(
                    "**{} posted as threads** on the changed lines — resolve them once they're \
                     fixed and PRowl checks again.\n\n",
                    split.inline.len()
                ));
            }
            push_already(&mut body, split.already);
            push_findings(&mut body, &split.elsewhere);
            body.push_str(&footer);
            ("COMMENT", body, threads(&split.inline))
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
///   findings. A previous PRowl review is marked superseded.
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
            let split = match review {
                Ok(review) => split_findings(github, token, target, review, Covered::Any).await?,
                Err(_) => Split {
                    inline: Vec::new(),
                    elsewhere: Vec::new(),
                    already: 0,
                    inline_total: 0,
                },
            };
            let findings_review = post_findings_threads(github, token, target, &split).await?;
            let threads_note = findings_review
                .as_ref()
                .map(|r| (split.inline.len(), r.html_url.as_str()));
            let body = needs_human_body(
                target,
                review.ok(),
                reasons,
                &split.elsewhere,
                threads_note,
                split.already,
            );
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
    already: usize,
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
            if let Some((count, url)) = threads.filter(|(count, _)| *count > 0) {
                body.push_str(&format!(
                    "**Findings:** {count} posted as [review threads]({url}) on the changed lines.\n\n"
                ));
            }
            push_already(&mut body, already);
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
        Reason::PromptInjection { location, excerpt } => format!(
            "**Possible prompt injection** in {location}: `{}` — text that tries to steer the AI \
             reviewer, so its review can't stand on its own",
            quote(excerpt)
        ),
        Reason::HiddenCharacters { location, count } => format!(
            "**{count} hidden or reordering Unicode character(s)** in {location} — the code may \
             not do what it appears to"
        ),
        Reason::SecretRedacted { count } => format!(
            "The agent's review contained {count} credential-like string(s); they were removed \
             before posting"
        ),
    }
}

/// Attacker-written text, made safe to show inside an inline code span: one line, no
/// backticks or control characters, bounded.
fn quote(text: &str) -> String {
    text.chars()
        .map(|c| if c == '`' { '\'' } else { c })
        .filter(|c| !c.is_control())
        .take(160)
        .collect::<String>()
        .trim()
        .to_owned()
}

pub(crate) fn agent_only_body(
    target: &Target<'_>,
    review: &ReviewOutput,
    other_findings: &[&Finding],
    already: usize,
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
    push_already(&mut body, already);
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

/// Notes the findings left out because a thread on the PR already raises them.
fn push_already(body: &mut String, already: usize) {
    if already > 0 {
        body.push_str(&format!(
            "**Already raised:** {already} finding(s) are covered by existing review threads and \
             weren't posted again.\n\n"
        ));
    }
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
            Reason::PromptInjection { location, .. } => {
                format!("possible prompt injection in {location}")
            }
            Reason::HiddenCharacters { location, .. } => format!("hidden characters in {location}"),
            Reason::SecretRedacted { .. } => "credentials removed from the review".to_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::Severity;

    fn finding(line: Option<u32>, title: &str) -> Finding {
        Finding {
            file: "src/a.rs".into(),
            line,
            severity: Severity::High,
            title: title.into(),
            body: "body".into(),
            suggestion: None,
            already_raised: false,
        }
    }

    fn thread(line: Option<u32>, resolved: bool, body: &str) -> ReviewThread {
        ReviewThread {
            path: "src/a.rs".into(),
            line,
            resolved,
            outdated: line.is_none(),
            author: "octo".into(),
            body: body.into(),
            comments: vec![],
            resolved_by: None,
        }
    }

    #[test]
    fn a_finding_posted_before_near_the_same_line_is_already_raised() {
        let posted = thread(Some(10), false, "**high** — Token logged\n\nbody");
        assert!(already_raised(
            &finding(Some(12), "Token logged"),
            std::slice::from_ref(&posted),
            Covered::Any
        ));
        assert!(!already_raised(
            &finding(Some(40), "Token logged"),
            std::slice::from_ref(&posted),
            Covered::Any
        ));
        assert!(!already_raised(
            &finding(Some(10), "Unchecked unwrap"),
            &[posted],
            Covered::Any
        ));
        let outdated = thread(None, false, "**high** — Token logged");
        assert!(already_raised(
            &finding(Some(3), "Token logged"),
            &[outdated],
            Covered::Any
        ));
    }

    #[test]
    fn a_verification_raises_again_what_was_resolved_but_not_what_is_open() {
        let f = finding(Some(10), "Token logged");
        let resolved = thread(Some(10), true, "**high** — Token logged");
        let open = thread(Some(10), false, "**high** — Token logged");
        assert!(already_raised(
            &f,
            std::slice::from_ref(&resolved),
            Covered::Any
        ));
        assert!(!already_raised(&f, &[resolved], Covered::Open));
        assert!(already_raised(&f, &[open], Covered::Open));
    }

    #[test]
    fn the_reviewer_can_say_a_thread_already_covers_a_finding() {
        let mut f = finding(Some(10), "Missing check");
        f.already_raised = true;
        assert!(already_raised(&f, &[], Covered::Any));
    }
}
