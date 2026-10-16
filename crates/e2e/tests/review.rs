mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use prowl_e2e::ClaudeResponse;
use prowld::policy::Scope;

#[tokio::test]
async fn a_queued_pr_is_reviewed_with_the_policy_in_a_read_only_checkout() {
    let (h, commits) = setup(&[7]).await;
    h.claude
        .respond_with(vec![ClaudeResponse::review(&valid_review())])
        .unwrap();
    let policy = h.client().effective_policy(Some(REPO)).await.unwrap();

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;

    let reviews = h.client().reviews(Some(job.id)).await.unwrap();
    assert_eq!(reviews.len(), 1);
    let review = &reviews[0];
    assert!(review.valid);
    assert_eq!(review.policy_version_id, policy.version_id);
    assert_eq!(review.cost_usd, Some(0.05));
    let output = review.output.as_ref().unwrap();
    assert_eq!(output.matched_policy_rules[0].id, "AUTH");

    let calls = h.claude.invocations().unwrap();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    // Ran inside a checkout of the PR head.
    assert_eq!(call.git_head.as_deref(), Some(commits[0].head_sha.as_str()));
    assert!(call.files.contains(&"src".to_owned()), "{:?}", call.files);
    // The first message carries the policy verbatim, then the PR, then the request.
    let prompt = &call.stdin;
    assert!(prompt.contains(policy.content.trim_end()));
    let order: Vec<usize> = [
        "<role>",
        "<review_policy",
        "<pr>",
        "<pr_description",
        "<request>",
    ]
    .iter()
    .map(|t| prompt.find(t).unwrap())
    .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]));
    // The diff and commit list live outside the checkout, in a directory Claude may read.
    let material = call.arg_after("--add-dir").expect("--add-dir");
    assert!(!std::path::Path::new(material).starts_with(&call.cwd));
    // The prompt uses forward slashes on every OS.
    assert!(prompt.contains(&format!("{}/pr.diff", material.replace('\\', "/"))));
    assert!(
        !call.files.iter().any(|f| f.starts_with(".prowl")),
        "nothing written into the checkout: {:?}",
        call.files
    );
    assert!(
        call.review_diff
            .as_deref()
            .unwrap()
            .contains("+fn login() {}")
    );
    assert!(call.review_commits.as_deref().unwrap().contains("PR 7"));
    assert_eq!(
        prompt.matches("</pr_description>").count(),
        1,
        "description can't escape"
    );
    // Read-only, no project settings, no MCP, nothing outside the allow-list.
    assert!(call.args.contains(&"--restricted".to_owned()));
    assert_eq!(call.arg_after("--permission-mode"), Some("dontAsk"));
    assert!(call.args.contains(&"--strict-mcp-config".to_owned()));
    assert_eq!(
        call.arg_after("--tools"),
        Some("Read,Grep,Glob"),
        "no shell"
    );
    assert_eq!(call.args_after("--allowedTools"), ["Read", "Grep", "Glob"]);
    for denied in ["Bash", "Write", "Edit", "WebFetch", "WebSearch"] {
        assert!(
            call.args_after("--disallowedTools").contains(&denied),
            "{denied} not denied"
        );
    }
    assert!(call.arg_after("--json-schema").is_some());

    assert_eq!(worktrees_left(&h), 0, "worktree removed after the review");
    let kinds: Vec<String> = events(&h).await.into_iter().map(|e| e.kind).collect();
    for kind in ["review.started", "review.output", "review.finished"] {
        assert!(
            kinds.iter().any(|k| k == kind),
            "{kind} missing from {kinds:?}"
        );
    }
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_request_withdrawn_while_queued_is_cancelled_without_claude() {
    let (mut h, commits) = setup(&[7, 8]).await;
    // One slot, held by a slow review of #7, so #8 waits in the queue.
    h.restart_with(|c| c.review_concurrency = 1).await.unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse {
            delay_ms: 1500,
            ..ClaudeResponse::review(&valid_review())
        }])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, |j| j.pr_number == 7 && j.status == "running").await;

    h.github.route(
        Method::GET,
        &format!("/repos/{REPO}/pulls/8"),
        StatusCode::OK,
        pr_json(8, &commits[1], &[]),
    );
    let cancelled = wait_for_job(&h, |j| j.pr_number == 8 && j.status == "cancelled").await;

    assert_eq!(cancelled.error.as_deref(), Some("not_requested"));
    wait_for_job(&h, |j| j.pr_number == 7 && j.status == "done").await;
    let calls = h.claude.invocations().unwrap();
    assert_eq!(calls.len(), 1, "Claude ran only for #7");
    assert_eq!(
        calls[0].git_head.as_deref(),
        Some(commits[0].head_sha.as_str())
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_newer_commit_supersedes_the_queued_one() {
    let (mut h, commits) = setup(&[7, 8]).await;
    h.restart_with(|c| c.review_concurrency = 1).await.unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse {
            delay_ms: 1500,
            ..ClaudeResponse::review(&valid_review())
        }])
        .unwrap();
    h.client().poll_now().await.unwrap();
    wait_for_job(&h, |j| j.pr_number == 7 && j.status == "running").await;

    let mut pushed = pr_json(8, &commits[1], &["octo"]);
    pushed["head"]["sha"] = "f00dfeed".into();
    h.github.route(
        Method::GET,
        &format!("/repos/{REPO}/pulls/8"),
        StatusCode::OK,
        pushed,
    );

    let job = wait_for_job(&h, |j| j.pr_number == 8 && j.status == "cancelled").await;
    assert_eq!(job.error.as_deref(), Some("superseded_by_newer_commit"));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_invalid_answer_gets_one_repair_attempt() {
    let (h, _) = setup(&[7]).await;
    h.claude
        .respond_with(vec![
            ClaudeResponse::answer("Looks fine to me!"),
            ClaudeResponse::review(&valid_review()),
        ])
        .unwrap();

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;

    let calls = h.claude.invocations().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].arg_after("--resume"), Some("session-1"));
    assert!(calls[1].stdin.contains("could not be used"));
    let review = &h.client().reviews(Some(job.id)).await.unwrap()[0];
    assert!(review.valid);
    assert_eq!(review.cost_usd, Some(0.1), "cost of both runs");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn output_still_invalid_after_repair_is_stored_as_invalid() {
    let (h, _) = setup(&[7]).await;
    let mut bad = valid_review();
    bad["severity"] = "catastrophic".into();
    h.claude
        .respond_with(vec![ClaudeResponse::review(&bad)])
        .unwrap();

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;

    assert_eq!(h.claude.invocations().unwrap().len(), 2);
    let review = &h.client().reviews(Some(job.id)).await.unwrap()[0];
    assert!(!review.valid);
    assert!(review.output.is_none());
    assert!(review.invalid_reason.as_deref().unwrap().contains("schema"));
    assert!(review.raw_answer.contains("catastrophic"));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_hanging_run_is_stopped_at_the_timeout() {
    let (mut h, _) = setup(&[7]).await;
    h.restart_with(|c| c.review_timeout = Duration::from_millis(500))
        .await
        .unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse {
            delay_ms: 30_000,
            ..ClaudeResponse::review(&valid_review())
        }])
        .unwrap();

    h.client().poll_now().await.unwrap();
    let started = std::time::Instant::now();
    let job = wait_for_job(&h, finished("failed")).await;

    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        job.error.as_deref().unwrap().contains("did not finish"),
        "{:?}",
        job.error
    );
    assert_eq!(worktrees_left(&h), 0);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrency_is_bounded() {
    let (mut h, _) = setup(&[7, 8]).await;
    h.restart_with(|c| c.review_concurrency = 1).await.unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse {
            delay_ms: 400,
            ..ClaudeResponse::review(&valid_review())
        }])
        .unwrap();

    h.client().poll_now().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while h
        .client()
        .jobs()
        .await
        .unwrap()
        .iter()
        .any(|j| j.status != "done")
    {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

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
async fn a_missing_claude_cli_fails_the_job_clearly() {
    let (mut h, _) = setup(&[7]).await;
    h.restart_with(|c| c.claude_bin = Some("/nonexistent/claude".into()))
        .await
        .unwrap();

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("failed")).await;

    assert!(
        job.error
            .as_deref()
            .unwrap()
            .contains("claude CLI was not found")
    );
    assert!(events(&h).await.iter().any(|e| e.kind == "claude.missing"));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_repo_override_policy_is_the_one_claude_sees() {
    let (h, _) = setup(&[7]).await;
    let strict = "## ALWAYS require human review\n- [EVERYTHING] Every change in acme/api\n";
    let saved = h
        .client()
        .save_policy(&Scope::parse_repo(REPO).unwrap(), strict)
        .await
        .unwrap();
    h.claude
        .respond_with(vec![ClaudeResponse::review(&valid_review())])
        .unwrap();

    h.client().poll_now().await.unwrap();
    let job = wait_for_job(&h, finished("done")).await;

    assert!(
        h.claude.invocations().unwrap()[0]
            .stdin
            .contains("[EVERYTHING] Every change in acme/api")
    );
    let review = &h.client().reviews(Some(job.id)).await.unwrap()[0];
    assert_eq!(review.policy_scope, REPO);
    assert_eq!(review.policy_version_id, saved.version_id);
    h.shutdown().await.unwrap();
}

/// A PR controls every file in its checkout, including symlinks. PRowl must never write
/// through them; it writes nothing into the checkout at all.
#[cfg(unix)]
#[tokio::test]
async fn symlinks_in_the_pr_cannot_redirect_prowls_writes() {
    let (h, _) = setup(&[]).await;
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().display().to_string();
    let commits = h
        .git
        .create_pr(
            REPO,
            9,
            &[("README.md", "# api\n")],
            &[
                (".prowl-review", &format!("symlink:{target}")),
                ("pr.diff", &format!("symlink:{target}/pwned")),
                ("src/x.rs", "fn x() {}\n"),
            ],
        )
        .unwrap();
    serve_prs(&h, &[(9, &commits)]);
    h.claude
        .respond_with(vec![ClaudeResponse::review(&valid_review())])
        .unwrap();

    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;

    let written: Vec<_> = std::fs::read_dir(outside.path()).unwrap().collect();
    assert!(
        written.is_empty(),
        "PRowl wrote through a PR symlink: {written:?}"
    );
    h.shutdown().await.unwrap();
}
