use std::time::Duration;

use prowl_e2e::Harness;
use prowld::client::ClientError;
use prowld::policy::{DEFAULT_POLICY, Scope};
use prowld::{ApiClient, PolicyVersion};

const STRICT: &str = "## ALWAYS require human review\n- [EVERYTHING] Every change\n";

fn repo(name: &str) -> Scope {
    Scope::parse_repo(name).unwrap()
}

fn status_of(err: ClientError) -> u16 {
    match err {
        ClientError::Status { status, .. } => status.as_u16(),
        other => panic!("expected an HTTP status error, got {other:?}"),
    }
}

async fn wait_for_versions(
    client: &ApiClient,
    scope: &Scope,
    done: impl Fn(&[PolicyVersion]) -> bool,
) -> Vec<PolicyVersion> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let versions = client.policy_versions(scope).await.unwrap();
        if done(&versions) {
            return versions;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out; versions {versions:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn first_run_writes_the_default_policy() {
    let mut h = Harness::start().await.unwrap();

    let policy = h.client().effective_policy(None).await.unwrap();

    assert_eq!(policy.scope, "global");
    assert_eq!(policy.content, DEFAULT_POLICY);
    let ids: Vec<_> = policy.rules.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["AUTH", "MONEY", "DATA", "INFRA", "SECRETS", "DEPS"]);
    let file = h.daemon().home().join("policies").join("global.md");
    assert_eq!(std::fs::read_to_string(file).unwrap(), DEFAULT_POLICY);

    h.restart().await.unwrap();
    let versions = h.client().policy_versions(&Scope::Global).await.unwrap();
    assert_eq!(versions.len(), 1, "restart must not add versions");
    assert_eq!(versions[0].source, "default");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn saves_are_versioned_and_restorable() {
    let h = Harness::start().await.unwrap();
    let client = h.client();
    let original = client.policy(&Scope::Global).await.unwrap();

    let saved = client.save_policy(&Scope::Global, STRICT).await.unwrap();
    assert_eq!(saved.rules.len(), 1);
    assert_eq!(saved.rules[0].id, "EVERYTHING");
    assert_eq!(client.effective_policy(None).await.unwrap().content, STRICT);

    let restored = client
        .restore_policy(&Scope::Global, original.version_id)
        .await
        .unwrap();
    assert_eq!(restored.content, DEFAULT_POLICY);
    assert!(restored.version_id > saved.version_id);

    let sources: Vec<_> = client
        .policy_versions(&Scope::Global)
        .await
        .unwrap()
        .into_iter()
        .map(|v| v.source)
        .collect();
    assert_eq!(sources, ["restore", "api", "default"]);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn repo_override_wins_for_that_repo_only() {
    let h = Harness::start().await.unwrap();
    let client = h.client();

    client
        .save_policy(&repo("acme/payments"), STRICT)
        .await
        .unwrap();

    let payments = client
        .effective_policy(Some("acme/payments"))
        .await
        .unwrap();
    assert_eq!(payments.scope, "acme/payments");
    assert_eq!(payments.content, STRICT);
    let docs = client.effective_policy(Some("acme/docs")).await.unwrap();
    assert_eq!(docs.scope, "global");

    client.delete_policy(&repo("acme/payments")).await.unwrap();
    // Deleting again is fine (idempotent); deleting one that never existed is not.
    client.delete_policy(&repo("acme/payments")).await.unwrap();
    assert_eq!(
        status_of(client.delete_policy(&repo("acme/never")).await.unwrap_err()),
        404
    );
    let after = client
        .effective_policy(Some("acme/payments"))
        .await
        .unwrap();
    assert_eq!(after.scope, "global");
    assert_eq!(
        status_of(client.policy(&repo("acme/payments")).await.unwrap_err()),
        404
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn edits_made_in_an_editor_are_picked_up_live() {
    let h = Harness::start().await.unwrap();
    let file = h.daemon().home().join("policies").join("global.md");

    std::fs::write(&file, STRICT).unwrap();

    let versions = wait_for_versions(&h.client(), &Scope::Global, |v| v[0].source == "file").await;
    assert_eq!(versions[0].content, STRICT);
    assert_eq!(
        h.client().effective_policy(None).await.unwrap().rules[0].id,
        "EVERYTHING"
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn new_override_files_are_discovered() {
    let h = Harness::start().await.unwrap();
    let dir = h
        .daemon()
        .home()
        .join("policies")
        .join("repos")
        .join("acme");
    std::fs::create_dir_all(&dir).unwrap();

    std::fs::write(dir.join("api.md"), STRICT).unwrap();

    wait_for_versions(&h.client(), &repo("acme/api"), |v| !v.is_empty()).await;
    assert_eq!(
        h.client()
            .effective_policy(Some("acme/api"))
            .await
            .unwrap()
            .scope,
        "acme/api"
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn edits_made_while_stopped_are_recorded_on_start() {
    let mut h = Harness::start().await.unwrap();
    let file = h.daemon().home().join("policies").join("global.md");

    h.shutdown_daemon_only().await.unwrap();
    std::fs::write(&file, STRICT).unwrap();
    h.restart().await.unwrap();

    let versions = h.client().policy_versions(&Scope::Global).await.unwrap();
    assert_eq!(versions[0].source, "file");
    assert_eq!(versions[0].content, STRICT);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleted_global_policy_is_restored() {
    let h = Harness::start().await.unwrap();
    let client = h.client();
    client.save_policy(&Scope::Global, STRICT).await.unwrap();
    let file = h.daemon().home().join("policies").join("global.md");

    std::fs::remove_file(&file).unwrap();

    let policy = client.effective_policy(None).await.unwrap();
    assert_eq!(policy.content, STRICT);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), STRICT);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn bad_input_is_rejected() {
    let h = Harness::start().await.unwrap();
    let client = h.client();

    assert_eq!(
        status_of(
            client
                .effective_policy(Some("not-a-repo"))
                .await
                .unwrap_err()
        ),
        422
    );
    assert_eq!(
        status_of(
            client
                .save_policy(&Scope::Global, "   \n")
                .await
                .unwrap_err()
        ),
        422
    );
    assert_eq!(
        status_of(
            client
                .save_policy(&Scope::Global, &"x".repeat(70 * 1024))
                .await
                .unwrap_err()
        ),
        422
    );
    assert_eq!(
        status_of(
            client
                .restore_policy(&Scope::Global, 9999)
                .await
                .unwrap_err()
        ),
        404
    );
    let escaped = client
        .get_raw("/v1/policies/repos/../secrets", Some("x"))
        .await
        .unwrap()
        .0;
    assert_ne!(escaped.as_u16(), 200);
    h.shutdown().await.unwrap();
}
