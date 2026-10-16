mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use prowl_e2e::ClaudeResponse;
use serde_json::json;

const REVIEWS: &str = "/repos/acme/api/pulls/7/reviews";

fn posted_review() -> serde_json::Value {
    json!({ "id": 907, "html_url": "https://github.com/acme/api/pull/7#pullrequestreview-907" })
}

#[tokio::test]
async fn a_github_5xx_is_retried_without_running_claude_again() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();
    h.github.route_seq(
        Method::POST,
        REVIEWS,
        vec![
            (StatusCode::BAD_GATEWAY, json!({ "message": "Bad Gateway" })),
            (StatusCode::OK, posted_review()),
        ],
    );

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;

    assert_eq!(job.attempts, 2);
    assert_eq!(
        h.claude.invocations().unwrap().len(),
        1,
        "the stored review was reused"
    );
    assert_eq!(h.github.requests_to(Method::POST, REVIEWS).len(), 2);
    assert!(events(&h).await.iter().any(|e| e.kind == "review.retrying"));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_review_already_posted_by_an_interrupted_attempt_is_not_posted_again() {
    let (h, commits) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();
    // The review gets posted, then labelling fails: as if PRowl died right after posting.
    h.github.route_seq(
        Method::POST,
        "/repos/acme/api/issues/7/labels",
        vec![
            (StatusCode::INTERNAL_SERVER_ERROR, json!({})),
            (StatusCode::OK, json!([])),
        ],
    );
    let marker = format!(
        "<!-- prowl:review pr=acme/api#7 sha={} -->",
        commits[0].head_sha
    );
    h.github.route_seq(
        Method::GET,
        REVIEWS,
        vec![
            (StatusCode::OK, json!([])),
            (
                StatusCode::OK,
                json!([{
                    "id": 907, "user": { "login": "octo" }, "state": "COMMENTED",
                    "body": format!("### 🤖 PRowl automated review\n…\n{marker}"),
                    "html_url": "https://github.com/acme/api/pull/7#pullrequestreview-907"
                }]),
            ),
        ],
    );

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;

    assert_eq!(job.attempts, 2);
    assert_eq!(
        h.github.requests_to(Method::POST, REVIEWS).len(),
        1,
        "posted exactly once"
    );
    assert_eq!(h.claude.invocations().unwrap().len(), 1);
    let review = &h.client().reviews(Some(job.id)).await.unwrap()[0];
    assert_eq!(
        review.published_url.as_deref(),
        Some("https://github.com/acme/api/pull/7#pullrequestreview-907")
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_restart_mid_review_still_posts_exactly_once() {
    let (mut h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse {
            delay_ms: 2000,
            ..ClaudeResponse::review(&valid_review())
        }])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, |j| j.status == "running").await;

    h.restart().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;

    assert_eq!(
        h.github
            .requests_to(Method::POST, "/repos/acme/api/issues/7/comments")
            .len(),
        1
    );
    assert_eq!(h.client().reviews(Some(job.id)).await.unwrap().len(), 1);
    assert_eq!(h.client().queue().await.unwrap().len(), 1);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn leftover_checkouts_are_removed_at_startup() {
    let (mut h, _) = setup(&[]).await;
    let home = h.daemon().home().to_path_buf();
    for dir in ["wt/999", "review/999"] {
        std::fs::create_dir_all(home.join(dir)).unwrap();
        std::fs::write(home.join(dir).join("leftover"), "x").unwrap();
    }

    h.restart().await.unwrap();

    assert!(!home.join("wt/999").exists());
    assert!(!home.join("review/999").exists());
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_git_locks_and_half_made_clones_dont_wedge_reviews() {
    let (mut h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();
    let home = h.daemon().home().to_path_buf();
    // A clone whose setup was killed midway, and a lock left by a killed git process.
    let leftover = home.join("git/acme/api.git.init-1");
    std::fs::create_dir_all(&leftover).unwrap();
    h.shutdown_daemon_only().await.unwrap();
    std::fs::create_dir_all(home.join("git/acme")).unwrap();
    std::process::Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(home.join("git/acme/api.git"))
        .status()
        .unwrap();
    std::fs::write(home.join("git/acme/api.git/HEAD.lock"), "").unwrap();
    h.restart().await.unwrap();
    assert!(!home.join("git/acme/api.git/HEAD.lock").exists());

    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_review_that_keeps_failing_gives_up_after_the_attempt_limit() {
    let (mut h, _) = setup(&[7]).await;
    h.restart_with(|c| c.max_attempts = 3).await.unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse {
            exit_code: 1,
            stderr: "API overloaded".into(),
            ..Default::default()
        }])
        .unwrap();

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("failed")).await;

    assert_eq!(job.attempts, 3);
    assert_eq!(h.claude.invocations().unwrap().len(), 3);
    assert!(job.error.as_deref().unwrap().contains("API overloaded"));
    let retries = events(&h)
        .await
        .into_iter()
        .filter(|e| e.kind == "review.retrying")
        .count();
    assert_eq!(retries, 2);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_permanent_github_error_is_not_retried() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();
    h.github.route(
        Method::POST,
        REVIEWS,
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({ "message": "Validation Failed" }),
    );

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("failed")).await;

    assert_eq!(job.attempts, 1);
    assert!(job.error.as_deref().unwrap().contains("422"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.github.requests_to(Method::POST, REVIEWS).len(), 1);
    h.shutdown().await.unwrap();
}
