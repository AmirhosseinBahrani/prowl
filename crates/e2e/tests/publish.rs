mod common;

use axum::http::Method;
use common::*;
use prowl_e2e::{ClaudeResponse, Harness};
use prowld::{Decision, Reason};
use serde_json::{Value, json};

/// A review that should stay with the agent: low risk, no policy rules, nothing critical.
fn calm_review() -> Value {
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

    let review = reviewed(&h, &calm_review()).await;

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

    let created = h
        .github
        .requests_to(Method::POST, &format!("/repos/{REPO}/labels"));
    assert_eq!(
        created[0].body.as_ref().unwrap()["name"],
        "prowl:agent-reviewed"
    );
    let labelled = h
        .github
        .requests_to(Method::POST, &format!("/repos/{REPO}/issues/7/labels"));
    assert_eq!(
        labelled[0].body.as_ref().unwrap()["labels"],
        json!(["prowl:agent-reviewed"])
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn prowl_never_approves() {
    let (h, _) = setup(&[7]).await;
    let mut eager = calm_review();
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
    // Published as a PR comment (ticket 08), never as a review.
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

    let review = reviewed(&h, &calm_review()).await;

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

    reviewed(&h, &calm_review()).await;

    let all = events(&h).await;
    let decided = all
        .iter()
        .find(|e| e.kind == "review.decided")
        .expect("review.decided");
    assert_eq!(decided.payload["decision"], "AGENT_ONLY");
    assert!(all.iter().any(|e| e.kind == "review.published"));
    h.shutdown().await.unwrap();
}
