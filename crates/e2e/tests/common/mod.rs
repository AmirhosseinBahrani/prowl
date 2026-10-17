//! Helpers shared by the review and publish tests.
#![allow(dead_code)]

use std::time::Duration;

use axum::http::{Method, StatusCode};
use futures::StreamExt;
use prowl_e2e::{Harness, PrCommits};
use prowld::{Event, Job, RepoQuery};
use serde_json::{Value, json};

pub const REPO: &str = "acme/api";

pub fn valid_review() -> Value {
    json!({
        "summary": "Adds a login rate limit.",
        "quick_notes": ["Limit is per IP"],
        "risk_score": 35,
        "severity": "medium",
        "touches_critical_sections": true,
        "matched_policy_rules": [{ "id": "AUTH", "evidence": "src/login.rs:1" }],
        "findings": [{
            "file": "src/login.rs", "line": 1, "severity": "medium",
            "title": "No limit on retries", "body": "Retries bypass the limiter.", "suggestion": null
        }],
        "recommendation": "comment"
    })
}

pub fn pr_json(number: u64, commits: &PrCommits, reviewers: &[&str]) -> Value {
    json!({
        "number": number,
        "state": "open",
        "draft": false,
        "title": format!("Rate limit login #{number}"),
        "html_url": format!("https://github.com/{REPO}/pull/{number}"),
        "user": { "login": "alice" },
        "body": "Adds a rate limit to login attempts.",
        "head": { "sha": commits.head_sha, "ref": "feature" },
        "base": { "sha": commits.base_sha, "ref": "main" },
        "additions": 3, "deletions": 0, "changed_files": 1,
        "labels": [{ "name": "security" }],
        "requested_reviewers": reviewers.iter().map(|l| json!({ "login": l })).collect::<Vec<_>>(),
        "requested_teams": [],
    })
}

/// Connected as `octo`, watching `acme/api`, with PRs `numbers` requesting octo's review.
pub async fn setup(numbers: &[u64]) -> (Harness, Vec<PrCommits>) {
    let h = Harness::start().await.unwrap();
    h.connect_github("octo").await.unwrap();
    h.github.route(
        Method::GET,
        "/user/repos",
        StatusCode::OK,
        json!([{
            "id": 1, "full_name": REPO, "name": "api", "owner": { "login": "acme" },
            "private": false, "archived": false, "default_branch": "main",
            "html_url": "https://github.com/acme/api"
        }]),
    );
    h.client().repos(&RepoQuery::default()).await.unwrap();
    h.client().watch(REPO).await.unwrap();

    let mut all = Vec::new();
    for &number in numbers {
        let commits = h
            .git
            .create_pr(
                REPO,
                number,
                &[("README.md", "# api\n")],
                &[(&format!("src/login_{number}.rs"), "fn login() {}\n")],
            )
            .unwrap();
        h.github.route(
            Method::GET,
            &format!("/repos/{REPO}/pulls/{number}"),
            StatusCode::OK,
            pr_json(number, &commits, &["octo"]),
        );
        accept_writes(&h, number);
        all.push(commits);
    }
    let refs: Vec<(u64, &PrCommits)> = numbers.iter().copied().zip(all.iter()).collect();
    serve_search(&h, &refs);
    (h, all)
}

/// Serves already-created PRs (requesting octo's review) from search and detail endpoints.
pub fn serve_prs(h: &Harness, prs: &[(u64, &PrCommits)]) {
    for (number, commits) in prs {
        h.github.route(
            Method::GET,
            &format!("/repos/{REPO}/pulls/{number}"),
            StatusCode::OK,
            pr_json(*number, commits, &["octo"]),
        );
        accept_writes(h, *number);
    }
    serve_search(h, prs);
}

/// Answers everything PRowl writes for PR `n` the way GitHub does.
pub fn accept_writes(h: &Harness, n: u64) {
    h.github.route(
        Method::GET,
        &format!("/repos/{REPO}/pulls/{n}/files"),
        StatusCode::OK,
        json!([{ "filename": format!("src/login_{n}.rs"), "patch": "@@ -0,0 +1 @@\n+fn login() {}" }]),
    );
    h.github.route(
        Method::POST,
        &format!("/repos/{REPO}/pulls/{n}/reviews"),
        StatusCode::OK,
        json!({ "id": 900 + n, "html_url": format!("https://github.com/{REPO}/pull/{n}#pullrequestreview-{}", 900 + n) }),
    );
    h.github.route(
        Method::GET,
        &format!("/repos/{REPO}/pulls/{n}/reviews"),
        StatusCode::OK,
        json!([]),
    );
    h.github.route(
        Method::POST,
        &format!("/repos/{REPO}/issues/{n}/comments"),
        StatusCode::CREATED,
        json!({ "id": 700 + n, "html_url": format!("https://github.com/{REPO}/pull/{n}#issuecomment-{}", 700 + n) }),
    );
    h.github.route(
        Method::GET,
        &format!("/repos/{REPO}/issues/{n}/comments"),
        StatusCode::OK,
        json!([]),
    );
    h.github.route(
        Method::PATCH,
        &format!("/repos/{REPO}/issues/comments/{}", 700 + n),
        StatusCode::OK,
        json!({ "id": 700 + n, "html_url": format!("https://github.com/{REPO}/pull/{n}#issuecomment-{}", 700 + n) }),
    );
    h.github.route(
        Method::PUT,
        &format!("/repos/{REPO}/pulls/{n}/reviews/{}", 900 + n),
        StatusCode::OK,
        json!({ "id": 900 + n, "html_url": format!("https://github.com/{REPO}/pull/{n}#pullrequestreview-{}", 900 + n) }),
    );
    h.github.route(
        Method::POST,
        &format!("/repos/{REPO}/labels"),
        StatusCode::CREATED,
        json!({}),
    );
    h.github.route(
        Method::POST,
        &format!("/repos/{REPO}/issues/{n}/labels"),
        StatusCode::OK,
        json!([]),
    );
    for label in ["prowl%3Aneeds-human", "prowl%3Aagent-reviewed"] {
        h.github.route(
            Method::DELETE,
            &format!("/repos/{REPO}/issues/{n}/labels/{label}"),
            StatusCode::OK,
            json!([]),
        );
    }
}

fn serve_search(h: &Harness, prs: &[(u64, &PrCommits)]) {
    let numbers: Vec<u64> = prs.iter().map(|(n, _)| *n).collect();
    let items: Vec<Value> = numbers
        .iter()
        .map(|n| json!({ "number": n, "repository_url": format!("https://api.github.com/repos/{REPO}") }))
        .collect();
    h.github.route(
        Method::GET,
        "/search/issues",
        StatusCode::OK,
        json!({ "total_count": items.len(), "items": items }),
    );
}

pub async fn wait_for_job(h: &Harness, done: impl Fn(&Job) -> bool) -> Job {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(job) = h
            .client()
            .jobs()
            .await
            .unwrap()
            .into_iter()
            .find(|j| done(j))
        {
            return job;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "job did not reach the expected state: {:?}",
            h.client().jobs().await.unwrap()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn finished(status: &'static str) -> impl Fn(&Job) -> bool {
    move |j| j.status == status
}

pub async fn events(h: &Harness) -> Vec<Event> {
    let mut stream = Box::pin(h.client().events(0).await.unwrap());
    let mut all = Vec::new();
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(200), stream.next()).await
    {
        all.push(event.unwrap());
    }
    all
}

pub fn worktrees_left(h: &Harness) -> usize {
    std::fs::read_dir(h.daemon().home().join("wt"))
        .map(|d| d.count())
        .unwrap_or(0)
}

/// A review the policy lets the agent handle alone.
pub fn calm_review() -> Value {
    json!({
        "summary": "Renames a helper.",
        "quick_notes": ["Pure rename"],
        "risk_score": 10,
        "severity": "low",
        "touches_critical_sections": false,
        "matched_policy_rules": [],
        "findings": [],
        "recommendation": "comment"
    })
}

/// Pushes a new commit to PR `n` (a new head SHA) and serves it, requesting `reviewers`.
pub fn push_new_commit(h: &Harness, n: u64, reviewers: &[&str]) -> PrCommits {
    let commits = h
        .git
        .create_pr(
            REPO,
            n,
            &[("README.md", "# api\n")],
            &[(&format!("src/login_{n}.rs"), "fn login() { /* v2 */ }\n")],
        )
        .unwrap();
    h.github.route(
        Method::GET,
        &format!("/repos/{REPO}/pulls/{n}"),
        StatusCode::OK,
        pr_json(n, &commits, reviewers),
    );
    commits
}

/// Waits until `count` jobs for PR `n` are done.
pub async fn wait_for_reviews(h: &Harness, n: u64, count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let done = h
            .client()
            .jobs()
            .await
            .unwrap()
            .iter()
            .filter(|j| j.pr_number == n && j.status == "done")
            .count();
        if done >= count {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "jobs: {:?}",
            h.client().jobs().await.unwrap()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
