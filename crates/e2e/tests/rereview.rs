mod common;

use axum::http::{Method, StatusCode};
use common::*;
use prowl_e2e::ClaudeResponse;
use serde_json::json;

const COMMENT: &str = "/repos/acme/api/issues/7/comments";
const EDIT_COMMENT: &str = "/repos/acme/api/issues/comments/707";
const REVIEWS: &str = "/repos/acme/api/pulls/7/reviews";
const EDIT_REVIEW: &str = "/repos/acme/api/pulls/7/reviews/907";

#[tokio::test]
async fn a_new_push_is_reviewed_again_and_the_comment_is_edited_not_duplicated() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&valid_review())])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 1).await;

    let v2 = push_new_commit(&h, 7, &["octo"]);
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 2).await;

    assert_eq!(h.claude.invocations().unwrap().len(), 2);
    assert_eq!(
        h.github.requests_to(Method::POST, COMMENT).len(),
        1,
        "one comment..."
    );
    let edits = h.github.requests_to(Method::PATCH, EDIT_COMMENT);
    assert_eq!(edits.len(), 1, "...edited for the new commit");
    let body = edits[0].body.as_ref().unwrap()["body"].as_str().unwrap();
    assert!(body.contains(&format!("sha={}", v2.head_sha)));
    let queue = h.client().queue().await.unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].head_sha, v2.head_sha);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_new_push_without_a_review_request_is_not_reviewed() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 1).await;

    // The agent's review cleared the request; nobody re-requested it.
    push_new_commit(&h, 7, &[]);
    h.client().poll_now().await.unwrap();

    assert_eq!(h.claude.invocations().unwrap().len(), 1);
    let gated = events(&h)
        .await
        .into_iter()
        .filter(|e| e.kind == "pr.gated_out")
        .collect::<Vec<_>>();
    assert!(
        gated
            .iter()
            .any(|e| e.payload["pr"] == 7 && e.payload["reason"] == "not_requested")
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn needs_human_then_agent_only_resolves_the_comment_and_leaves_the_queue() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![
            ClaudeResponse::review(&valid_review()),
            ClaudeResponse::review(&calm_review()),
        ])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 1).await;
    assert_eq!(h.client().queue().await.unwrap().len(), 1);

    push_new_commit(&h, 7, &["octo"]);
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 2).await;

    assert_eq!(
        h.github.requests_to(Method::POST, REVIEWS).len(),
        1,
        "agent review posted"
    );
    let resolved = h.github.requests_to(Method::PATCH, EDIT_COMMENT);
    assert!(
        resolved[0].body.as_ref().unwrap()["body"]
            .as_str()
            .unwrap()
            .contains("No longer waiting")
    );
    assert_eq!(
        h.github
            .requests_to(
                Method::DELETE,
                "/repos/acme/api/issues/7/labels/prowl%3Aneeds-human"
            )
            .len(),
        1
    );
    assert!(h.client().queue().await.unwrap().is_empty());
    let left = events(&h)
        .await
        .into_iter()
        .find(|e| e.kind == "queue.resolved")
        .unwrap();
    assert_eq!(left.payload["via"], "superseded");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn agent_only_then_needs_human_supersedes_the_review_and_queues_the_pr() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![
            ClaudeResponse::review(&calm_review()),
            ClaudeResponse::review(&valid_review()),
        ])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 1).await;

    push_new_commit(&h, 7, &["octo"]); // re-requested by the author
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 2).await;

    assert_eq!(h.github.requests_to(Method::POST, COMMENT).len(), 1);
    let superseded = h.github.requests_to(Method::PUT, EDIT_REVIEW);
    assert_eq!(superseded.len(), 1);
    assert!(
        superseded[0].body.as_ref().unwrap()["body"]
            .as_str()
            .unwrap()
            .contains("superseded")
    );
    assert_eq!(
        h.github
            .requests_to(
                Method::DELETE,
                "/repos/acme/api/issues/7/labels/prowl%3Aagent-reviewed"
            )
            .len(),
        1
    );
    assert_eq!(h.client().queue().await.unwrap().len(), 1);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_second_agent_review_marks_the_first_superseded() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 1).await;

    push_new_commit(&h, 7, &["octo"]);
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 2).await;

    assert_eq!(
        h.github.requests_to(Method::POST, REVIEWS).len(),
        2,
        "inline notes need a new review"
    );
    assert_eq!(h.github.requests_to(Method::PUT, EDIT_REVIEW).len(), 1);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_deleted_comment_is_posted_again() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&valid_review())])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 1).await;
    h.github.route(
        Method::PATCH,
        EDIT_COMMENT,
        StatusCode::NOT_FOUND,
        json!({ "message": "Not Found" }),
    );

    push_new_commit(&h, 7, &["octo"]);
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 2).await;

    assert_eq!(h.github.requests_to(Method::POST, COMMENT).len(), 2);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_agent_approval_is_withdrawn_when_a_newer_commit_needs_a_human() {
    let (mut h, _) = setup(&[7]).await;
    h.restart_with(|c| c.agent_verdicts = true).await.unwrap();
    let mut approve = calm_review();
    approve["recommendation"] = "approve".into();
    h.claude
        .respond_with(vec![
            ClaudeResponse::review(&approve),
            ClaudeResponse::review(&valid_review()),
        ])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 1).await;
    // GitHub now lists PRowl's approval.
    h.github.route(
        Method::GET,
        REVIEWS,
        StatusCode::OK,
        json!([{ "id": 907, "state": "APPROVED", "user": { "login": "octo" }, "body": "earlier" }]),
    );
    h.github.route(
        Method::PUT,
        "/repos/acme/api/pulls/7/reviews/907/dismissals",
        StatusCode::OK,
        json!({ "id": 907, "state": "DISMISSED" }),
    );

    push_new_commit(&h, 7, &["octo"]);
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 2).await;

    let dismissed = h.github.requests_to(
        Method::PUT,
        "/repos/acme/api/pulls/7/reviews/907/dismissals",
    );
    assert_eq!(dismissed.len(), 1, "the stale approval is withdrawn");
    assert_eq!(dismissed[0].body.as_ref().unwrap()["event"], "DISMISS");
    assert_eq!(h.client().queue().await.unwrap().len(), 1);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_comment_review_is_never_dismissed() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![
            ClaudeResponse::review(&calm_review()),
            ClaudeResponse::review(&valid_review()),
        ])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 1).await;
    h.github.route(
        Method::GET,
        REVIEWS,
        StatusCode::OK,
        json!([{ "id": 907, "state": "COMMENTED", "user": { "login": "octo" }, "body": "earlier" }]),
    );

    push_new_commit(&h, 7, &["octo"]);
    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 2).await;

    assert!(
        h.github
            .requests_to(
                Method::PUT,
                "/repos/acme/api/pulls/7/reviews/907/dismissals"
            )
            .is_empty()
    );
    h.shutdown().await.unwrap();
}
