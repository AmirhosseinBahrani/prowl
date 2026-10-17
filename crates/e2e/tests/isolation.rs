//! What a review's Claude run can reach: no secrets from the daemon's environment, and no
//! symlinks out of the checkout. (Its own test binary: it sets a process environment variable.)

mod common;

use common::*;
use prowl_e2e::ClaudeResponse;

#[cfg(unix)]
#[tokio::test]
async fn claude_sees_no_daemon_secrets_and_no_symlinks_out_of_the_checkout() {
    // SAFETY: this test binary runs this one test; nothing else reads the environment.
    unsafe { std::env::set_var("PROWL_TEST_DEPLOY_TOKEN", "do-not-leak") };
    let (h, _) = setup(&[]).await;
    let outside = h.daemon().home().join("secret.txt");
    std::fs::write(&outside, "TOP SECRET").unwrap();
    let commits = h
        .git
        .create_pr(
            REPO,
            7,
            &[("README.md", "hello\n")],
            &[("notes.txt", &format!("symlink:{}", outside.display()))],
        )
        .unwrap();
    serve_prs(&h, &[(7, &commits)]);
    h.claude
        .respond_with(vec![ClaudeResponse::review(&calm_review())])
        .unwrap();

    h.client().poll_now().await.unwrap();
    wait_for_job(&h, finished("done")).await;

    let run = &h.claude.invocations().unwrap()[0];
    assert!(
        run.files.contains(&"notes.txt".to_owned()),
        "{:?}",
        run.files
    );
    assert!(
        run.symlinks.is_empty(),
        "no symlinks in the checkout: {:?}",
        run.symlinks
    );
    assert!(run.env.iter().any(|v| v == "PATH"), "the basics are there");
    assert!(
        !run.env.iter().any(|v| v == "PROWL_TEST_DEPLOY_TOKEN"),
        "the daemon's secrets are not: {:?}",
        run.env
    );
    h.shutdown().await.unwrap();
}
