//! After the review threads are resolved, a verification run checks the fixes and approves.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use prowl_e2e::{ClaudeResponse, Harness};
use serde_json::{Value, json};

const REVIEWS: &str = "/repos/acme/api/pulls/7/reviews";

/// GitHub's GraphQL answer listing PRowl's thread on src/login_7.rs:1.
fn threads(resolved: bool) -> Value {
    json!({ "data": { "repository": { "pullRequest": { "reviewThreads": {
        "pageInfo": { "hasNextPage": false, "endCursor": null },
        "nodes": [{
            "isResolved": resolved, "isOutdated": false, "path": "src/login_7.rs", "line": 1,
            "comments": { "nodes": [{
                "author": { "login": "octo" },
                "body": "**High** — Token logged\n\n<sub>🤖 PRowl automated review</sub>"
            }] }
        }]
    } } } } })
}

/// A review whose one finding is on a line the PR changed, so it becomes a thread.
fn with_threaded_finding(mut review: Value) -> Value {
    review["findings"] = json!([{
        "file": "src/login_7.rs", "line": 1, "severity": "high", "title": "Token logged",
        "body": "The session token goes to the log.", "suggestion": "Redact it."
    }]);
    review
}

async fn job_done(h: &Harness, id: i64) -> prowld::Job {
    wait_for_job(h, |j| {
        j.id == id && !matches!(j.status.as_str(), "queued" | "running")
    })
    .await
}

fn last_review(h: &Harness) -> Value {
    h.github
        .requests_to(Method::POST, REVIEWS)
        .pop()
        .unwrap()
        .body
        .unwrap()
}

#[tokio::test]
async fn verifying_a_queued_pr_approves_it_when_everything_was_fixed() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![
            ClaudeResponse::review(&with_threaded_finding(valid_review())),
            ClaudeResponse::review(&calm_review()),
        ])
        .unwrap();
    h.github
        .route(Method::POST, "/graphql", StatusCode::OK, threads(true));
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;
    assert_eq!(h.client().queue().await.unwrap().len(), 1);

    let job = h.client().verify("acme/api", 7).await.unwrap();

    assert_eq!(job.kind, "verify");
    let done = job_done(&h, job.id).await;
    assert_eq!(done.status, "done", "{:?}", done.error);
    let asked = &h.claude.invocations().unwrap()[1].stdin;
    assert!(asked.contains("<previous_findings"), "{asked}");
    assert!(
        asked.contains("Token logged") && asked.contains("\"resolved\""),
        "{asked}"
    );
    let approval = last_review(&h);
    assert_eq!(approval["event"], "APPROVE");
    assert!(
        approval["body"]
            .as_str()
            .unwrap()
            .contains("PRowl verification passed")
    );
    assert!(
        h.client().queue().await.unwrap().is_empty(),
        "it leaves the queue"
    );
    let labelled = h
        .github
        .requests_to(Method::POST, "/repos/acme/api/issues/7/labels");
    assert_eq!(
        labelled.last().unwrap().body.as_ref().unwrap()["labels"],
        json!(["prowl:human-approved"])
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_verification_that_finds_problems_opens_new_threads_and_approves_nothing() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![
            ClaudeResponse::review(&with_threaded_finding(valid_review())),
            ClaudeResponse::review(&with_threaded_finding(calm_review())),
        ])
        .unwrap();
    h.github
        .route(Method::POST, "/graphql", StatusCode::OK, threads(true));
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;

    let job = h.client().verify("acme/api", 7).await.unwrap();
    job_done(&h, job.id).await;

    let review = last_review(&h);
    assert_eq!(review["event"], "COMMENT");
    assert!(
        review["body"]
            .as_str()
            .unwrap()
            .contains("1 issue(s) remain")
    );
    assert_eq!(review["comments"][0]["path"], "src/login_7.rs");
    assert!(
        h.github
            .writes()
            .iter()
            .all(|w| w.body.as_ref().and_then(|b| b.get("event")) != Some(&json!("APPROVE")))
    );
    assert_eq!(
        h.client().queue().await.unwrap().len(),
        1,
        "still waits for a human"
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn with_agent_verdicts_resolved_threads_are_verified_and_approved_on_their_own() {
    let (mut h, _) = setup(&[7]).await;
    h.restart_with(|c| c.agent_verdicts = true).await.unwrap();
    h.claude
        .respond_with(vec![
            ClaudeResponse::review(&with_threaded_finding(calm_review())),
            ClaudeResponse::review(&calm_review()),
        ])
        .unwrap();
    h.github
        .route(Method::POST, "/graphql", StatusCode::OK, threads(false));
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;
    assert_eq!(
        last_review(&h)["event"],
        "COMMENT",
        "a finding: no approval yet"
    );

    // Threads still open: nothing happens.
    h.client().poll_now().await.unwrap();
    assert_eq!(h.client().jobs().await.unwrap().len(), 1);

    // The author resolves the thread.
    h.github
        .route(Method::POST, "/graphql", StatusCode::OK, threads(true));
    h.client().poll_now().await.unwrap();
    let verify = h
        .client()
        .jobs()
        .await
        .unwrap()
        .into_iter()
        .find(|j| j.kind == "verify")
        .expect("a verification was queued");
    let done = job_done(&h, verify.id).await;
    assert_eq!(done.status, "done", "{:?}", done.error);
    assert_eq!(last_review(&h)["event"], "APPROVE");

    // Once per commit.
    h.client().poll_now().await.unwrap();
    assert_eq!(h.client().jobs().await.unwrap().len(), 2);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn without_agent_verdicts_nothing_is_verified_on_its_own() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&with_threaded_finding(
            calm_review(),
        ))])
        .unwrap();
    h.github
        .route(Method::POST, "/graphql", StatusCode::OK, threads(true));
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;
    let thread_checks = || {
        h.github
            .requests_to(Method::POST, "/graphql")
            .iter()
            .filter(|r| r.raw_body.contains("reviewThreads"))
            .count()
    };
    let before = thread_checks();

    h.client().poll_now().await.unwrap();

    assert_eq!(h.client().jobs().await.unwrap().len(), 1);
    assert_eq!(thread_checks(), before, "no thread states polled");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn fixes_that_need_a_human_are_not_approved_by_verification() {
    let (mut h, _) = setup(&[7]).await;
    h.restart_with(|c| c.agent_verdicts = true).await.unwrap();
    // The fix touches authentication: the verification matches the AUTH rule.
    let mut touches_auth = calm_review();
    touches_auth["matched_policy_rules"] =
        json!([{ "id": "AUTH", "evidence": "src/login_7.rs:1" }]);
    touches_auth["recommendation"] = "approve".into();
    h.claude
        .respond_with(vec![
            ClaudeResponse::review(&with_threaded_finding(calm_review())),
            ClaudeResponse::review(&touches_auth),
        ])
        .unwrap();
    h.github
        .route(Method::POST, "/graphql", StatusCode::OK, threads(true));
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;

    h.client().poll_now().await.unwrap();
    let verify = h
        .client()
        .jobs()
        .await
        .unwrap()
        .into_iter()
        .find(|j| j.kind == "verify")
        .expect("a verification was queued");
    job_done(&h, verify.id).await;

    assert!(
        h.github
            .writes()
            .iter()
            .all(|w| w.body.as_ref().and_then(|b| b.get("event")) != Some(&json!("APPROVE"))),
        "never approved"
    );
    let comment = h
        .github
        .requests_to(Method::POST, "/repos/acme/api/issues/7/comments")
        .pop()
        .expect("a waiting-for-approval comment");
    let text = comment.body.as_ref().unwrap()["body"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        text.contains("Waiting for human approval") && text.contains("AUTH"),
        "{text}"
    );
    assert_eq!(
        h.client().queue().await.unwrap().len(),
        1,
        "it waits for a human"
    );
    h.shutdown().await.unwrap();
}
