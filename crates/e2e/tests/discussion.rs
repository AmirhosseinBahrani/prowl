//! Every review sees what was said on the pull request so far, doesn't post again what a thread
//! already raises, and a review requested again is run again.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use prowl_e2e::ClaudeResponse;
use serde_json::{Value, json};

const REVIEWS: &str = "/repos/acme/api/pulls/7/reviews";

/// GitHub's GraphQL answer: one review thread on src/login_7.rs:1 and, when `requested_at` is
/// set, a review request for octo at that time.
fn pull_request(resolved: bool, replies: &[&str], requested_at: Option<&str>) -> Value {
    let mut comments = vec![json!({
        "author": { "login": "octo" },
        "body": "**high** — Token logged\n\nThe session token goes to the log."
    })];
    comments.extend(
        replies
            .iter()
            .map(|r| json!({ "author": { "login": "alice" }, "body": r })),
    );
    let requests: Vec<Value> = requested_at
        .map(|at| json!({ "createdAt": at, "requestedReviewer": { "login": "octo" } }))
        .into_iter()
        .collect();
    json!({ "data": { "repository": { "pullRequest": {
        "reviewThreads": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "nodes": [{
                "isResolved": resolved, "isOutdated": false, "path": "src/login_7.rs", "line": 1,
                "comments": { "nodes": comments }
            }]
        },
        "timelineItems": { "nodes": requests }
    } } } })
}

/// A review the agent may publish alone, with one finding on a changed line.
fn calm_with_finding(title: &str, already_raised: bool) -> Value {
    let mut review = calm_review();
    review["findings"] = json!([{
        "file": "src/login_7.rs", "line": 1, "severity": "low", "title": title,
        "body": "Details.", "suggestion": null, "already_raised": already_raised
    }]);
    review
}

fn last_review(h: &prowl_e2e::Harness) -> Value {
    h.github
        .requests_to(Method::POST, REVIEWS)
        .pop()
        .unwrap()
        .body
        .unwrap()
}

#[tokio::test]
async fn the_review_sees_the_threads_and_the_conversation() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();
    h.github.route(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        pull_request(false, &["It's redacted further down"], None),
    );
    h.github.route(
        Method::GET,
        "/repos/acme/api/issues/7/comments",
        StatusCode::OK,
        json!([{ "id": 1, "user": { "login": "bob" }, "body": "Please keep the cache",
                 "html_url": "https://github.com/acme/api/pull/7#issuecomment-1" }]),
    );
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;

    let asked = &h.claude.invocations().unwrap()[0].stdin;
    assert!(asked.contains("<existing_discussion"), "{asked}");
    assert!(asked.contains("Token logged") && asked.contains("\"open\""));
    assert!(asked.contains("It's redacted further down"));
    assert!(asked.contains("Please keep the cache"));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_finding_a_thread_already_raises_is_not_posted_again() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_with_finding(
            "Token logged",
            false,
        ))])
        .unwrap();
    h.github.route(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        pull_request(true, &[], None),
    );
    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;
    assert_eq!(job.status, "done", "{:?}", job.error);

    let review = last_review(&h);
    assert_eq!(review["comments"], json!([]), "no duplicate thread");
    assert!(
        review["body"].as_str().unwrap().contains("Already raised"),
        "{review}"
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_reviewer_can_mark_a_finding_as_already_raised() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_with_finding(
            "Session token leaks",
            true,
        ))])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;

    assert_eq!(last_review(&h)["comments"], json!([]));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_new_finding_is_still_posted() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_with_finding(
            "Unchecked unwrap",
            false,
        ))])
        .unwrap();
    h.github.route(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        pull_request(false, &[], None),
    );
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;

    let comments = last_review(&h)["comments"].clone();
    assert_eq!(comments.as_array().unwrap().len(), 1, "{comments}");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_review_requested_again_is_run_again_with_the_discussion_as_it_is_now() {
    let (mut h, _) = setup(&[7]).await;
    // The fake GitHub shares the test's clock.
    h.restart_with(|c| c.rerequest_margin = std::time::Duration::ZERO)
        .await
        .unwrap();
    h.claude
        .respond_with(vec![
            ClaudeResponse::review(&calm_review()),
            ClaudeResponse::review(&calm_review()),
        ])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 1).await;

    // Nothing new yet: polling again changes nothing.
    h.client().poll_now().await.unwrap();
    assert_eq!(h.client().jobs().await.unwrap().len(), 1);

    // The author answers the thread and requests the review again, on the same commit.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    h.github.route(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        pull_request(false, &["Fixed by redacting it"], Some(&now)),
    );
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 2).await;

    let jobs = h.client().jobs().await.unwrap();
    assert_eq!(jobs.len(), 2);
    assert!(jobs.iter().any(|j| j.kind == "rerun"), "{jobs:?}");
    let asked = &h.claude.invocations().unwrap()[1].stdin;
    assert!(asked.contains("Fixed by redacting it"), "{asked}");

    // Once re-reviewed, the same request doesn't queue it a third time.
    h.client().poll_now().await.unwrap();
    assert_eq!(h.client().jobs().await.unwrap().len(), 2);
    h.shutdown().await.unwrap();
}
