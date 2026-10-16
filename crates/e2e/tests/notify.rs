mod common;

use std::time::Duration;

use common::*;
use prowl_e2e::ClaudeResponse;

async fn wait_for_notification(h: &prowl_e2e::Harness, title: &str) -> (String, String) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(n) = h
            .notifications
            .sent
            .lock()
            .unwrap()
            .iter()
            .find(|(t, _)| t == title)
        {
            return n.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no {title:?} notification: {:?}",
            h.notifications.sent.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_pr_entering_the_queue_is_announced_on_the_desktop() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&valid_review())])
        .unwrap();

    h.client().poll_now().await.unwrap();

    let (_, body) = wait_for_notification(&h, "A pull request needs your review").await;
    assert_eq!(body, "acme/api#7 — Rate limit login #7");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn agent_handled_prs_are_not_announced_but_failures_are() {
    let (mut h, _) = setup(&[7, 8]).await;
    h.restart_with(|c| {
        c.max_attempts = 1;
        c.review_concurrency = 1;
    })
    .await
    .unwrap();
    h.claude
        .respond_with(vec![
            ClaudeResponse::review(&calm_review()),
            ClaudeResponse {
                exit_code: 1,
                stderr: "overloaded".into(),
                ..Default::default()
            },
        ])
        .unwrap();

    h.client().poll_now().await.unwrap();

    let (_, body) = wait_for_notification(&h, "PRowl couldn't review acme/api#8").await;
    assert!(body.contains("overloaded"));
    let sent = h.notifications.sent.lock().unwrap().clone();
    assert!(
        !sent.iter().any(|(t, _)| t.contains("needs your review")),
        "{sent:?}"
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn notifications_can_be_turned_off() {
    let (mut h, _) = setup(&[7]).await;
    h.restart_with(|c| c.notifications = false).await.unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse::review(&valid_review())])
        .unwrap();

    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(h.notifications.sent.lock().unwrap().is_empty());
    h.shutdown().await.unwrap();
}
