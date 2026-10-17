//! Reviewing a pull request again on request.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use prowl_e2e::ClaudeResponse;

#[tokio::test]
async fn a_reviewed_pr_is_reviewed_again_on_request() {
    let (h, commits) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();
    h.client().poll_now().await.unwrap();
    let first = wait_for_job(&h, finished("done")).await;
    // Posting the review cleared the review request, as GitHub does.
    h.github.route(
        Method::GET,
        "/repos/acme/api/pulls/7",
        StatusCode::OK,
        pr_json(7, &commits[0], &[]),
    );

    let rerun = h.client().rerun("acme/api", 7).await.unwrap();

    assert_eq!(rerun.kind, "rerun");
    assert_eq!(
        rerun.head_sha, first.head_sha,
        "same commit, reviewed again"
    );
    let done = wait_for_job(&h, |j| {
        j.id == rerun.id && j.status != "queued" && j.status != "running"
    })
    .await;
    assert_eq!(done.status, "done", "{:?}", done.error);
    assert_eq!(h.claude.invocations().unwrap().len(), 2);
    let posted = h
        .github
        .requests_to(Method::POST, "/repos/acme/api/pulls/7/reviews");
    assert_eq!(posted.len(), 2, "a second review was posted");
    let body = posted[1].body.as_ref().unwrap()["body"].as_str().unwrap();
    assert!(body.contains(&format!("run={} -->", rerun.id)), "{body}");
    // Polling doesn't queue the commit again on its own.
    h.client().poll_now().await.unwrap();
    assert_eq!(h.client().jobs().await.unwrap().len(), 2);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_closed_pr_is_not_reviewed_again() {
    let (h, commits) = setup(&[7]).await;
    let mut closed = pr_json(7, &commits[0], &["octo"]);
    closed["state"] = "closed".into();
    h.github.route(
        Method::GET,
        "/repos/acme/api/pulls/7",
        StatusCode::OK,
        closed,
    );

    let err = h
        .client()
        .rerun("acme/api", 7)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("409") && err.contains("isn't open"), "{err}");
    assert!(h.client().jobs().await.unwrap().is_empty());
    h.shutdown().await.unwrap();
}
