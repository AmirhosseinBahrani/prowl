//! Stopping reviews: a running one is stopped where it is, a waiting one never runs.

mod common;

use axum::http::Method;
use common::*;
use prowl_e2e::ClaudeResponse;

fn slow(review: serde_json::Value) -> ClaudeResponse {
    ClaudeResponse {
        delay_ms: 30_000,
        ..ClaudeResponse::review(&review)
    }
}

#[tokio::test]
async fn a_running_review_is_stopped_and_posts_nothing() {
    let (h, _) = setup(&[7]).await;
    h.claude.respond_with(vec![slow(calm_review())]).unwrap();
    h.client().poll_now().await.unwrap();
    let running = wait_for_job(&h, |j| j.status == "running").await;
    // Claude has been started.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while h.claude.invocations().unwrap().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "claude never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let cancelled = h.client().cancel_job(running.id).await.unwrap();

    assert_eq!(cancelled, prowld::Cancelled::Running);
    let job = wait_for_job(&h, |j| j.id == running.id && j.status == "cancelled").await;
    assert_eq!(job.error.as_deref(), Some("cancelled by you"));
    assert!(
        h.github
            .requests_to(Method::POST, &format!("/repos/{REPO}/pulls/7/reviews"))
            .is_empty()
            && h.github
                .requests_to(Method::POST, &format!("/repos/{REPO}/issues/7/comments"))
                .is_empty(),
        "nothing was posted"
    );
    let transcript = h.client().transcript(running.id).await.unwrap();
    assert_eq!(transcript.last().unwrap().kind, "error");
    let worktrees = h.daemon().home().join("wt").join(running.id.to_string());
    assert!(!worktrees.exists(), "its checkout is removed");
    // Finished jobs can't be cancelled.
    let err = h
        .client()
        .cancel_job(running.id)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("409"), "{err}");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_waiting_review_never_runs() {
    let (mut h, _) = setup(&[7, 8]).await;
    h.restart_with(|c| c.review_concurrency = 1).await.unwrap();
    h.claude.respond_with(vec![slow(calm_review())]).unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, |j| j.status == "running").await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while h.claude.invocations().unwrap().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "claude never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let waiting = h
        .client()
        .jobs()
        .await
        .unwrap()
        .into_iter()
        .find(|j| j.status == "queued")
        .expect("one review waits for the other");

    let cancelled = h.client().cancel_job(waiting.id).await.unwrap();

    assert_eq!(cancelled, prowld::Cancelled::Waiting);
    let job = h
        .client()
        .jobs()
        .await
        .unwrap()
        .into_iter()
        .find(|j| j.id == waiting.id)
        .unwrap();
    assert_eq!(job.status, "cancelled");
    assert_eq!(
        h.claude.invocations().unwrap().len(),
        1,
        "only the first one ran"
    );
    h.shutdown().await.unwrap();
}
