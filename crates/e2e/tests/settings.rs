mod common;

use std::time::Duration;

use axum::http::Method;
use common::*;
use prowl_e2e::{ClaudeResponse, Harness};
use prowld::client::ClientError;
use prowld::{Decision, Reason, SleepMode};
use serde_json::json;

fn status_of(err: ClientError) -> u16 {
    match err {
        ClientError::Status { status, .. } => status.as_u16(),
        other => panic!("expected an HTTP status error, got {other:?}"),
    }
}

#[tokio::test]
async fn settings_start_from_the_config_persist_and_validate() {
    let mut h = Harness::start().await.unwrap();
    let defaults = h.client().settings().await.unwrap();
    assert_eq!(defaults.risk_threshold, 60);
    assert_eq!(defaults.review_concurrency, 2);

    let updated = h
        .client()
        .update_settings(json!({ "risk_threshold": 35, "skip_drafts": false }))
        .await
        .unwrap();
    assert_eq!((updated.risk_threshold, updated.skip_drafts), (35, false));
    assert_eq!(updated.review_concurrency, 2, "other settings untouched");

    h.restart().await.unwrap();
    let after = h.client().settings().await.unwrap();
    assert_eq!((after.risk_threshold, after.skip_drafts), (35, false));

    for bad in [
        json!({ "risk_threshold": 0 }),
        json!({ "review_concurrency": 100 }),
        json!({ "file_rules": { "acme/api": ["[broken"] } }),
        json!({ "nope": 1 }),
    ] {
        let err = h.client().update_settings(bad.clone()).await.unwrap_err();
        assert_eq!(status_of(err), 422, "{bad}");
    }
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrency_changes_apply_without_a_restart() {
    let (h, _) = setup(&[7, 8]).await;
    h.client()
        .update_settings(json!({ "review_concurrency": 1 }))
        .await
        .unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse {
            delay_ms: 400,
            ..ClaudeResponse::review(&calm_review())
        }])
        .unwrap();

    h.client().poll_now().await.unwrap();
    wait_for_reviews(&h, 7, 1).await;
    wait_for_reviews(&h, 8, 1).await;

    let mut running = 0;
    for event in events(&h).await {
        match event.kind.as_str() {
            "review.started" => {
                running += 1;
                assert!(running <= 1, "two reviews ran at once");
            }
            "review.finished" => running -= 1,
            _ => {}
        }
    }
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_new_daily_budget_applies_immediately() {
    let (h, _) = setup(&[7, 8]).await;
    h.client()
        .update_settings(json!({ "daily_budget_usd": 0.05, "review_concurrency": 1 }))
        .await
        .unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();

    h.client().poll_now().await.unwrap();

    let waiting = wait_for_job(&h, |j| {
        j.error.as_deref() == Some("daily Claude budget reached")
    })
    .await;
    assert_eq!(waiting.status, "queued");
    assert_eq!(
        h.client().costs().await.unwrap().daily_budget_usd,
        Some(0.05)
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn sleep_mode_and_notifications_follow_the_settings() {
    let h = Harness::start().await.unwrap();
    let deadline = || tokio::time::Instant::now() + Duration::from_secs(5);
    let until = deadline();
    while !h.power.is_held() {
        assert!(
            tokio::time::Instant::now() < until,
            "ac_only on AC should hold"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    h.client()
        .update_settings(json!({ "sleep_mode": "during_reviews" }))
        .await
        .unwrap();

    let until = deadline();
    while h.power.is_held() {
        assert!(
            tokio::time::Instant::now() < until,
            "idle during_reviews should release"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        h.client().power().await.unwrap().mode,
        SleepMode::DuringReviews
    );
    h.shutdown().await.unwrap();

    let (h, _) = setup(&[7]).await;
    h.client()
        .update_settings(json!({ "notifications": false }))
        .await
        .unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse::review(&valid_review())])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.notifications.sent.lock().unwrap().is_empty());
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_shorter_poll_interval_takes_effect_without_a_restart() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();
    // The harness polls hourly; 10s is the shortest interval allowed.
    h.client()
        .update_settings(json!({ "poll_interval_secs": 10 }))
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while h.client().jobs().await.unwrap().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no poll after the interval changed"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_file_rule_forces_a_human_even_when_the_agent_is_calm() {
    let (h, _) = setup(&[7]).await;
    h.client()
        .update_settings(json!({ "file_rules": { "acme/api": ["docs/**", "src/login_*.rs"] } }))
        .await
        .unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;

    let review = &h.client().reviews(Some(job.id)).await.unwrap()[0];
    assert_eq!(review.decision, Some(Decision::NeedsHuman));
    assert_eq!(
        review.decision_reasons[0],
        Reason::FilePattern {
            pattern: "src/login_*.rs".into(),
            file: "src/login_7.rs".into()
        }
    );
    assert_eq!(
        h.claude.invocations().unwrap().len(),
        1,
        "Claude still writes the notes"
    );
    let comment = &h
        .github
        .requests_to(Method::POST, "/repos/acme/api/issues/7/comments")[0];
    assert!(
        comment.body.as_ref().unwrap()["body"]
            .as_str()
            .unwrap()
            .contains("matches the safety rule `src/login_*.rs`")
    );
    h.shutdown().await.unwrap();
}
