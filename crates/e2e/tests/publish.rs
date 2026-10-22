mod common;

use axum::http::Method;
use common::*;
use prowl_e2e::{ClaudeResponse, Harness};
use prowld::{Decision, Reason};
use serde_json::{Value, json};

/// A review that should stay with the agent: low risk, no policy rules, nothing critical.
fn calm_review_with_findings() -> Value {
    json!({
        "summary": "Renames a helper and updates its callers.",
        "quick_notes": ["Pure rename", "No behaviour change"],
        "risk_score": 10,
        "severity": "low",
        "touches_critical_sections": false,
        "matched_policy_rules": [],
        "findings": [
            {
                "file": "src/login_7.rs", "line": 1, "severity": "low",
                "title": "Name could be clearer", "body": "`login` hides that it validates.",
                "suggestion": "Call it `validate_login`."
            },
            {
                "file": "README.md", "line": 40, "severity": "trivial",
                "title": "Docs mention the old name", "body": "Update the README.", "suggestion": null
            }
        ],
        "recommendation": "comment"
    })
}

async fn reviewed(h: &Harness, review: &Value) -> prowld::Review {
    h.claude
        .respond_with(vec![ClaudeResponse::review(review)])
        .unwrap();
    h.client().poll_now().await.unwrap();
    let job = wait_for_job(h, |j| j.status == "done" || j.status == "failed").await;
    assert_eq!(job.status, "done", "{:?}", job.error);
    h.client().reviews(Some(job.id)).await.unwrap().remove(0)
}

#[tokio::test]
async fn an_agent_only_review_is_posted_as_a_comment_review() {
    let (h, commits) = setup(&[7]).await;

    let review = reviewed(&h, &calm_review_with_findings()).await;

    assert_eq!(review.decision, Some(Decision::AgentOnly));
    assert!(review.decision_reasons.is_empty());
    assert_eq!(
        review.published_url.as_deref(),
        Some("https://github.com/acme/api/pull/7#pullrequestreview-907")
    );

    let posted = h
        .github
        .requests_to(Method::POST, &format!("/repos/{REPO}/pulls/7/reviews"));
    assert_eq!(posted.len(), 1, "exactly one review");
    let body = posted[0].body.as_ref().unwrap();
    assert_eq!(body["event"], "COMMENT");
    assert_eq!(body["commit_id"], commits[0].head_sha);
    let text = body["body"].as_str().unwrap();
    assert!(text.contains("PRowl automated review"));
    assert!(text.contains("not a human review"));
    assert!(text.contains("Pure rename"));
    assert!(text.contains(&format!(
        "<!-- prowl:review pr={REPO}#7 sha={} -->",
        commits[0].head_sha
    )));
    // The finding on a diff line is inline; the other one is in the body.
    let comments = body["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["path"], "src/login_7.rs");
    assert_eq!(comments[0]["line"], 1);
    assert_eq!(comments[0]["side"], "RIGHT");
    assert!(
        comments[0]["body"]
            .as_str()
            .unwrap()
            .contains("validate_login")
    );
    assert!(text.contains("`README.md:40`"));

    assert_eq!(body["event"], "COMMENT");
    assert_no_label_writes(&h);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn without_agent_verdicts_prowl_never_approves() {
    let (h, _) = setup(&[7]).await;
    let mut eager = calm_review_with_findings();
    eager["recommendation"] = "approve".into();

    reviewed(&h, &eager).await;

    for write in h.github.writes() {
        let event = write
            .body
            .as_ref()
            .and_then(|b| b.get("event"))
            .and_then(|e| e.as_str());
        assert_ne!(event, Some("APPROVE"), "{} {}", write.method, write.path);
    }
    h.shutdown().await.unwrap();
}

fn posted_review(h: &Harness) -> Value {
    h.github
        .requests_to(Method::POST, &format!("/repos/{REPO}/pulls/7/reviews"))
        .remove(0)
        .body
        .unwrap()
}

#[tokio::test]
async fn with_agent_verdicts_a_low_risk_pr_is_approved() {
    let (mut h, _) = setup(&[7]).await;
    h.restart_with(|c| c.agent_verdicts = true).await.unwrap();
    let mut eager = calm_review_with_findings();
    eager["recommendation"] = "approve".into();

    let review = reviewed(&h, &eager).await;

    assert_eq!(review.decision, Some(Decision::AgentOnly));
    let posted = posted_review(&h);
    assert_eq!(posted["event"], "APPROVE");
    assert!(
        posted["body"]
            .as_str()
            .unwrap()
            .contains("approved by the agent")
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn with_agent_verdicts_changes_are_requested_when_claude_recommends_it() {
    let (mut h, _) = setup(&[7]).await;
    h.restart_with(|c| c.agent_verdicts = true).await.unwrap();
    let mut picky = calm_review_with_findings();
    picky["recommendation"] = "request_changes".into();

    reviewed(&h, &picky).await;

    let posted = posted_review(&h);
    assert_eq!(posted["event"], "REQUEST_CHANGES");
    assert!(
        posted["body"]
            .as_str()
            .unwrap()
            .contains("changes requested by the agent")
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn agent_verdicts_never_approve_a_pr_that_needs_a_human() {
    let (mut h, _) = setup(&[7]).await;
    h.restart_with(|c| c.agent_verdicts = true).await.unwrap();
    let mut risky = valid_review();
    risky["recommendation"] = "approve".into();

    let review = reviewed(&h, &risky).await;

    assert_eq!(review.decision, Some(Decision::NeedsHuman));
    for write in h.github.writes() {
        let event = write.body.as_ref().and_then(|b| b.get("event"));
        assert_ne!(
            event.and_then(|e| e.as_str()),
            Some("APPROVE"),
            "{} {}",
            write.method,
            write.path
        );
    }
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn findings_that_need_a_human_are_posted_as_threads() {
    let (h, _) = setup(&[7]).await;
    let mut risky = valid_review();
    // On a line the PR changed (the fake diff adds src/login_7.rs:1), plus one elsewhere.
    risky["findings"] = json!([
        { "file": "src/login_7.rs", "line": 1, "severity": "high", "title": "Token logged",
          "body": "The session token goes to the log.", "suggestion": "Redact it." },
        { "file": "docs/auth.md", "line": null, "severity": "low", "title": "Docs outdated",
          "body": "Mentions the old flow.", "suggestion": null }
    ]);

    let review = reviewed(&h, &risky).await;

    assert_eq!(review.decision, Some(Decision::NeedsHuman));
    let posted = h
        .github
        .requests_to(Method::POST, &format!("/repos/{REPO}/pulls/7/reviews"));
    assert_eq!(posted.len(), 1, "one review holding the threads");
    let threads = posted[0].body.as_ref().unwrap();
    assert_eq!(
        threads["event"], "COMMENT",
        "never a verdict for a PR that needs a human"
    );
    let comments = threads["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["path"], "src/login_7.rs");
    assert_eq!(comments[0]["line"], 1);
    let thread = comments[0]["body"].as_str().unwrap();
    assert!(
        thread.contains("Token logged") && thread.contains("Redact it."),
        "{thread}"
    );

    let comment = h
        .github
        .requests_to(Method::POST, &format!("/repos/{REPO}/issues/7/comments"))
        .remove(0);
    let text = comment.body.as_ref().unwrap()["body"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.contains("Waiting for human approval"));
    assert!(
        text.contains(
            "1 posted as [review threads](https://github.com/acme/api/pull/7#pullrequestreview-907)"
        ),
        "{text}"
    );
    assert!(
        !text.contains("Token logged"),
        "threaded findings aren't repeated:\n{text}"
    );
    assert!(
        text.contains("Docs outdated"),
        "findings off the diff stay in the comment"
    );
    assert_eq!(
        review.published_url.as_deref(),
        Some("https://github.com/acme/api/pull/7#issuecomment-707")
    );
    assert_eq!(h.client().queue().await.unwrap().len(), 1);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_matched_policy_rule_needs_a_human_and_submits_no_review() {
    let (h, _) = setup(&[7]).await;

    let review = reviewed(&h, &valid_review()).await;

    assert_eq!(review.decision, Some(Decision::NeedsHuman));
    assert!(review.decision_reasons.contains(&Reason::PolicyRule {
        id: "AUTH".into(),
        evidence: "src/login.rs:1".into()
    }));
    assert!(review.decision_reasons.contains(&Reason::CriticalSection));
    assert!(
        h.github
            .requests_to(Method::POST, &format!("/repos/{REPO}/pulls/7/reviews"))
            .is_empty()
    );
    // Its finding isn't on a changed line, so there are no threads: just the PR comment.
    assert_eq!(
        review.published_url.as_deref(),
        Some("https://github.com/acme/api/pull/7#issuecomment-707")
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn unusable_output_needs_a_human() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::answer("LGTM")])
        .unwrap();

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;

    let review = &h.client().reviews(Some(job.id)).await.unwrap()[0];
    assert_eq!(review.decision, Some(Decision::NeedsHuman));
    assert!(matches!(
        review.decision_reasons[0],
        Reason::InvalidOutput { .. }
    ));
    assert!(
        h.github
            .requests_to(Method::POST, &format!("/repos/{REPO}/pulls/7/reviews"))
            .is_empty()
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_risk_threshold_is_configurable() {
    let (mut h, _) = setup(&[7]).await;
    h.restart_with(|c| c.risk_threshold = 10).await.unwrap();

    let review = reviewed(&h, &calm_review_with_findings()).await;

    assert_eq!(review.decision, Some(Decision::NeedsHuman));
    assert_eq!(
        review.decision_reasons,
        [Reason::RiskScore {
            score: 10,
            threshold: 10
        }]
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn decisions_are_announced() {
    let (h, _) = setup(&[7]).await;

    reviewed(&h, &calm_review_with_findings()).await;

    let all = events(&h).await;
    let decided = all
        .iter()
        .find(|e| e.kind == "review.decided")
        .expect("review.decided");
    assert_eq!(decided.payload["decision"], "AGENT_ONLY");
    assert!(all.iter().any(|e| e.kind == "review.published"));
    h.shutdown().await.unwrap();
}
