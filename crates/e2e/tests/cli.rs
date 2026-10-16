mod common;

use axum::http::Method;
use common::*;
use prowl::queue_tab::QueueAction;
use prowl_e2e::ClaudeResponse;

async fn queued() -> prowl_e2e::Harness {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&valid_review())])
        .unwrap();
    h.client().poll_now().await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while h.client().queue().await.unwrap().is_empty() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    h
}

#[tokio::test]
async fn prowl_queue_lists_what_waits_for_you() {
    let h = queued().await;

    let out = prowl::cli::queue(&h.client()).await.unwrap();

    assert!(
        out.starts_with("acme/api#7  [medium]  Rate limit login #7\n"),
        "{out}"
    );
    assert!(out.contains("matched [AUTH] src/login.rs:1"));
    assert!(out.contains("https://github.com/acme/api/pull/7"));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn prowl_approve_and_request_changes_act_on_github() {
    let h = queued().await;
    let client = h.client();

    let err = prowl::cli::resolve(&client, "acme/api#7", QueueAction::RequestChanges, " ")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("-m"));
    assert!(
        prowl::cli::resolve(&client, "acme/api", QueueAction::Approve, "")
            .await
            .is_err()
    );

    let out = prowl::cli::resolve(&client, "acme/api#7", QueueAction::Approve, "ship it")
        .await
        .unwrap();

    assert_eq!(out, "Approved acme/api#7.\n");
    let review = &h
        .github
        .requests_to(Method::POST, "/repos/acme/api/pulls/7/reviews")[0];
    assert_eq!(review.body.as_ref().unwrap()["event"], "APPROVE");
    assert_eq!(review.body.as_ref().unwrap()["body"], "ship it");
    assert_eq!(
        prowl::cli::queue(&client).await.unwrap(),
        "Nothing is waiting for your review.\n"
    );
    h.shutdown().await.unwrap();
}
