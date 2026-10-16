mod common;

use axum::http::{Method, StatusCode};
use common::*;
use prowl_e2e::ClaudeResponse;
use prowld::{Decision, Reason};

#[tokio::test]
async fn reviews_stop_starting_once_the_daily_budget_is_spent() {
    let (mut h, _) = setup(&[7, 8]).await;
    // Each fake run costs $0.05, so one review uses the whole budget.
    h.restart_with(|c| {
        c.daily_budget_usd = Some(0.05);
        c.review_concurrency = 1;
    })
    .await
    .unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();

    h.client().poll_now().await.unwrap();
    wait_for_job(&h, |j| j.pr_number == 7 && j.status == "done").await;
    let waiting = wait_for_job(&h, |j| {
        j.pr_number == 8 && j.error.as_deref() == Some("daily Claude budget reached")
    })
    .await;

    assert_eq!(waiting.status, "queued");
    assert_eq!(h.claude.invocations().unwrap().len(), 1);
    let reached: Vec<_> = events(&h)
        .await
        .into_iter()
        .filter(|e| e.kind == "budget.reached")
        .collect();
    assert_eq!(reached.len(), 1);
    assert_eq!(reached[0].payload["daily_budget_usd"], 0.05);
    let costs = h.client().costs().await.unwrap();
    assert_eq!((costs.today_usd, costs.reviews_today), (0.05, 1));
    assert_eq!(costs.daily_budget_usd, Some(0.05));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn each_run_is_capped_at_what_is_left_of_the_budget() {
    let (mut h, _) = setup(&[7]).await;
    h.restart_with(|c| {
        c.daily_budget_usd = Some(2.0);
        c.review_budget_usd = Some(0.75);
    })
    .await
    .unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();

    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;

    // The smaller of the per-review cap and what's left today.
    assert_eq!(
        h.claude.invocations().unwrap()[0].arg_after("--max-budget-usd"),
        Some("0.75")
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_oversized_pr_goes_to_a_human_without_running_claude() {
    let (h, commits) = setup(&[7]).await;
    let mut huge = pr_json(7, &commits[0], &["octo"]);
    huge["additions"] = 4000.into();
    huge["deletions"] = 500.into();
    huge["changed_files"] = 42.into();
    h.github
        .route(Method::GET, "/repos/acme/api/pulls/7", StatusCode::OK, huge);

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;

    assert!(h.claude.invocations().unwrap().is_empty(), "no Claude run");
    let review = &h.client().reviews(Some(job.id)).await.unwrap()[0];
    assert_eq!(review.decision, Some(Decision::NeedsHuman));
    assert_eq!(
        review.decision_reasons,
        [Reason::TooLarge {
            changed_lines: 4500,
            changed_files: 42,
            max_lines: 3000,
            max_files: 100
        }]
    );
    let comment = &h
        .github
        .requests_to(Method::POST, "/repos/acme/api/issues/7/comments")[0];
    let body = comment.body.as_ref().unwrap()["body"].as_str().unwrap();
    assert!(body.contains("Too large for an automated review: 4500 changed lines in 42 files"));
    assert!(body.contains("PRowl did not run the agent"));
    assert_eq!(h.client().queue().await.unwrap().len(), 1);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn costs_are_reported_per_review_and_per_day() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;

    assert_eq!(
        h.client().reviews(Some(job.id)).await.unwrap()[0].cost_usd,
        Some(0.05)
    );
    let costs = h.client().costs().await.unwrap();
    assert_eq!(costs.today_usd, 0.05);
    assert_eq!(costs.daily_budget_usd, None);
    assert!(costs.resets_at_ms > prowl_now_ms());
    h.shutdown().await.unwrap();
}

fn prowl_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
