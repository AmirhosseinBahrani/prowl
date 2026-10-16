mod common;

use axum::http::{Method, StatusCode};
use common::*;
use prowl_e2e::{ClaudeResponse, Harness};
use prowld::client::ClientError;
use serde_json::{Value, json};

fn high_risk(severity: &str, risk: u8) -> Value {
    let mut review = valid_review();
    review["severity"] = severity.into();
    review["risk_score"] = risk.into();
    review
}

async fn queued(numbers: &[u64], review: &Value) -> Harness {
    let (h, _) = setup(numbers).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(review)])
        .unwrap();
    h.client().poll_now().await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while h.client().queue().await.unwrap().len() < numbers.len() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "jobs: {:?}",
            h.client().jobs().await.unwrap()
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    h
}

fn status_of(err: ClientError) -> u16 {
    match err {
        ClientError::Status { status, .. } => status.as_u16(),
        other => panic!("expected an HTTP status error, got {other:?}"),
    }
}

#[tokio::test]
async fn needs_human_posts_a_waiting_for_approval_comment_not_a_review() {
    let h = queued(&[7], &valid_review()).await;

    assert!(
        h.github
            .requests_to(Method::POST, &format!("/repos/{REPO}/pulls/7/reviews"))
            .is_empty(),
        "a review would clear the user's review request"
    );
    let comments = h
        .github
        .requests_to(Method::POST, &format!("/repos/{REPO}/issues/7/comments"));
    assert_eq!(comments.len(), 1);
    let body = comments[0].body.as_ref().unwrap()["body"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(body.contains("Waiting for human approval"));
    assert!(body.contains("Matched **[AUTH]** — src/login.rs:1"));
    assert!(body.contains("Touches code the policy treats as critical"));
    assert!(body.contains("Adds a login rate limit."));
    assert!(body.contains("No limit on retries"), "findings are listed");
    assert!(body.contains("<!-- prowl:review pr=acme/api#7 sha="));
    let labelled = h
        .github
        .requests_to(Method::POST, &format!("/repos/{REPO}/issues/7/labels"));
    assert_eq!(
        labelled[0].body.as_ref().unwrap()["labels"],
        json!(["prowl:needs-human"])
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_queue_lists_the_pr_with_its_notes_and_diff() {
    let h = queued(&[7], &valid_review()).await;
    h.github.route(
        Method::GET,
        &format!("/repos/{REPO}/pulls/7"),
        StatusCode::OK,
        json!("diff --git a/src/login_7.rs b/src/login_7.rs\n+fn login() {}\n"),
    );

    let queue = h.client().queue().await.unwrap();
    assert_eq!(queue.len(), 1);
    let item = &queue[0];
    assert_eq!((item.repo.as_str(), item.pr_number), (REPO, 7));
    assert_eq!(item.title, "Rate limit login #7");
    assert_eq!(item.summary.as_deref(), Some("Adds a login rate limit."));
    assert_eq!(item.quick_notes, ["Limit is per IP"]);
    assert_eq!(item.findings.len(), 1);
    assert!(
        item.reasons
            .iter()
            .any(|r| matches!(r, prowld::Reason::PolicyRule { id, .. } if id == "AUTH"))
    );
    assert_eq!(
        item.comment_url.as_deref(),
        Some("https://github.com/acme/api/pull/7#issuecomment-707")
    );

    let detail = h.client().queue_item(REPO, 7).await.unwrap();
    assert!(detail.diff.contains("fn login"));
    let diff_request = h
        .github
        .requests_to(Method::GET, &format!("/repos/{REPO}/pulls/7"))
        .pop()
        .unwrap();
    assert_eq!(
        diff_request.headers["accept"],
        "application/vnd.github.diff"
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_riskiest_pr_comes_first() {
    let (h, _) = setup(&[7, 8, 9]).await;
    h.claude
        .respond_with(vec![
            ClaudeResponse::review(&high_risk("medium", 70)),
            ClaudeResponse::review(&high_risk("critical", 95)),
            ClaudeResponse::answer("not json at all"),
        ])
        .unwrap();
    // One at a time, so responses map to PRs in order.
    let mut h = h;
    h.restart_with(|c| c.review_concurrency = 1).await.unwrap();
    h.client().poll_now().await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while h.client().queue().await.unwrap().len() < 3 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let order: Vec<u64> = h
        .client()
        .queue()
        .await
        .unwrap()
        .iter()
        .map(|i| i.pr_number)
        .collect();
    // #9's review was unusable (repair also failed), so it's the most urgent; then critical; then medium.
    assert_eq!(order, [9, 8, 7]);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn approving_in_the_app_submits_the_users_review_and_relabels() {
    let h = queued(&[7], &valid_review()).await;
    let head = h.client().queue().await.unwrap()[0].head_sha.clone();

    let item = h
        .client()
        .approve(REPO, 7, "Looks good to me")
        .await
        .unwrap();

    assert_eq!(item.status, "approved");
    assert!(h.client().queue().await.unwrap().is_empty());
    let reviews = h
        .github
        .requests_to(Method::POST, &format!("/repos/{REPO}/pulls/7/reviews"));
    assert_eq!(reviews.len(), 1, "only the user's explicit approval");
    let body = reviews[0].body.as_ref().unwrap();
    assert_eq!(body["event"], "APPROVE");
    assert_eq!(body["commit_id"], head);
    assert_eq!(body["body"], "Looks good to me");
    let removed = h.github.requests_to(
        Method::DELETE,
        &format!("/repos/{REPO}/issues/7/labels/prowl%3Aneeds-human"),
    );
    assert_eq!(removed.len(), 1);
    let added = h
        .github
        .requests_to(Method::POST, &format!("/repos/{REPO}/issues/7/labels"));
    assert_eq!(
        added.last().unwrap().body.as_ref().unwrap()["labels"],
        json!(["prowl:human-approved"])
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn requesting_changes_needs_a_comment() {
    let h = queued(&[7], &valid_review()).await;

    assert_eq!(
        status_of(h.client().request_changes(REPO, 7, "  ").await.unwrap_err()),
        422
    );
    let item = h
        .client()
        .request_changes(REPO, 7, "Please add tests")
        .await
        .unwrap();

    assert_eq!(item.status, "changes_requested");
    let reviews = h
        .github
        .requests_to(Method::POST, &format!("/repos/{REPO}/pulls/7/reviews"));
    assert_eq!(
        reviews[0].body.as_ref().unwrap()["event"],
        "REQUEST_CHANGES"
    );
    let added = h
        .github
        .requests_to(Method::POST, &format!("/repos/{REPO}/issues/7/labels"));
    assert_eq!(
        added.last().unwrap().body.as_ref().unwrap()["labels"],
        json!(["prowl:changes-requested"])
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn dismissing_touches_nothing_on_github() {
    let h = queued(&[7], &valid_review()).await;
    let writes_before = h.github.writes().len();

    let item = h.client().dismiss(REPO, 7).await.unwrap();

    assert_eq!(item.status, "dismissed");
    assert!(h.client().queue().await.unwrap().is_empty());
    assert_eq!(h.github.writes().len(), writes_before);
    assert_eq!(
        status_of(h.client().dismiss(REPO, 7).await.unwrap_err()),
        404
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_review_submitted_on_github_is_noticed_on_the_next_poll() {
    let h = queued(&[7], &valid_review()).await;
    h.github.route(
        Method::GET,
        &format!("/repos/{REPO}/pulls/7/reviews"),
        StatusCode::OK,
        json!([
            { "user": { "login": "someone-else" }, "state": "APPROVED", "submitted_at": "2099-01-01T00:00:00Z" },
            { "user": { "login": "octo" }, "state": "APPROVED", "submitted_at": "2099-01-01T00:00:00Z" }
        ]),
    );

    h.client().poll_now().await.unwrap();

    assert!(h.client().queue().await.unwrap().is_empty());
    let added = h
        .github
        .requests_to(Method::POST, &format!("/repos/{REPO}/issues/7/labels"));
    assert_eq!(
        added.last().unwrap().body.as_ref().unwrap()["labels"],
        json!(["prowl:human-approved"])
    );
    let resolved = events(&h)
        .await
        .into_iter()
        .find(|e| e.kind == "queue.resolved")
        .unwrap();
    assert_eq!(resolved.payload["via"], "github");
    assert_eq!(resolved.payload["status"], "approved");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn reviews_from_before_the_pr_was_queued_dont_count() {
    let h = queued(&[7], &valid_review()).await;
    h.github.route(
        Method::GET,
        &format!("/repos/{REPO}/pulls/7/reviews"),
        StatusCode::OK,
        json!([{ "user": { "login": "octo" }, "state": "APPROVED", "submitted_at": "2001-01-01T00:00:00Z" }]),
    );

    h.client().poll_now().await.unwrap();

    assert_eq!(h.client().queue().await.unwrap().len(), 1);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_closed_pr_leaves_the_queue() {
    let (h, commits) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&valid_review())])
        .unwrap();
    h.client().poll_now().await.unwrap();
    while h.client().queue().await.unwrap().is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let mut closed = pr_json(7, &commits[0], &["octo"]);
    closed["state"] = "closed".into();
    h.github.route(
        Method::GET,
        &format!("/repos/{REPO}/pulls/7"),
        StatusCode::OK,
        closed,
    );

    h.client().poll_now().await.unwrap();

    assert!(h.client().queue().await.unwrap().is_empty());
    h.shutdown().await.unwrap();
}
