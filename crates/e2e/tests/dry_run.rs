mod common;

use axum::http::{Method, StatusCode};
use common::*;
use prowl_e2e::ClaudeResponse;
use prowld::Decision;
use prowld::policy::Scope;

#[tokio::test]
async fn a_dry_run_reviews_without_touching_github_or_the_queue() {
    let (h, _) = setup(&[]).await;
    let commits = h
        .git
        .create_pr(
            REPO,
            7,
            &[("README.md", "# api\n")],
            &[("src/login_7.rs", "fn login() {}\n")],
        )
        .unwrap();
    // The PR exists but isn't requesting anyone: dry runs work on any PR.
    h.github.route(
        Method::GET,
        "/repos/acme/api/pulls/7",
        StatusCode::OK,
        pr_json(7, &commits, &[]),
    );
    h.claude
        .respond_with(vec![ClaudeResponse::review(&valid_review())])
        .unwrap();
    let writes_before = h.github.writes().len();

    let result = h.client().dry_run(REPO, 7).await.unwrap();

    assert_eq!(result.decision, Decision::NeedsHuman);
    assert!(
        result
            .review
            .unwrap()
            .matched_policy_rules
            .iter()
            .any(|r| r.id == "AUTH")
    );
    assert_eq!(result.policy_scope, "global");
    assert_eq!(
        h.github.writes().len(),
        writes_before,
        "nothing written to GitHub"
    );
    assert!(h.client().jobs().await.unwrap().is_empty());
    assert!(h.client().queue().await.unwrap().is_empty());
    assert!(h.client().reviews(None).await.unwrap().is_empty());
    assert_eq!(h.claude.invocations().unwrap().len(), 1);
    // ...but it isn't free: it counts towards today's budget.
    assert_eq!(h.client().costs().await.unwrap().today_usd, 0.05);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_dry_run_uses_the_repos_override_policy() {
    let (h, _) = setup(&[]).await;
    let commits = h
        .git
        .create_pr(REPO, 7, &[("a", "a")], &[("b", "b")])
        .unwrap();
    h.github.route(
        Method::GET,
        "/repos/acme/api/pulls/7",
        StatusCode::OK,
        pr_json(7, &commits, &[]),
    );
    let strict = "## ALWAYS require human review\n- [ALL] everything\n";
    h.client()
        .save_policy(&Scope::parse_repo(REPO).unwrap(), strict)
        .await
        .unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();

    let result = h.client().dry_run(REPO, 7).await.unwrap();

    assert_eq!(result.policy_scope, REPO);
    assert!(
        h.claude.invocations().unwrap()[0]
            .stdin
            .contains("[ALL] everything")
    );
    assert_eq!(
        result.decision,
        Decision::AgentOnly,
        "calm review, no rule matched"
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn policies_are_listed_with_their_files() {
    let h = prowl_e2e::Harness::start().await.unwrap();
    h.client()
        .save_policy(
            &Scope::parse_repo("acme/payments").unwrap(),
            "## ALWAYS\n- [MONEY] money\n",
        )
        .await
        .unwrap();

    let list = h.client().policies().await.unwrap();

    let scopes: Vec<_> = list.iter().map(|p| p.scope.as_str()).collect();
    assert_eq!(scopes, ["global", "acme/payments"]);
    assert_eq!(list[1].rules, ["MONEY"]);
    for policy in &list {
        assert!(
            std::path::Path::new(&policy.path).exists(),
            "{}",
            policy.path
        );
    }
    h.shutdown().await.unwrap();
}
