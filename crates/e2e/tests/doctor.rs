use prowl_e2e::Harness;

#[tokio::test]
async fn reports_a_usable_claude_and_git_without_leaking_the_account() {
    let h = Harness::start().await.unwrap();

    let doctor = h.client().doctor().await.unwrap();

    assert!(doctor.ok(), "{doctor:?}");
    assert!(doctor.claude.tool.found);
    assert_eq!(doctor.claude.logged_in, Some(true));
    assert!(
        doctor
            .claude
            .tool
            .version
            .as_deref()
            .unwrap()
            .contains("2.1.283")
    );
    assert!(doctor.git.found);
    let raw = h.client().get_raw("/v1/doctor", None).await.unwrap();
    assert_eq!(raw.0.as_u16(), 401, "needs the token like everything else");
    let json = serde_json::to_string(&doctor).unwrap();
    assert!(
        !json.contains("private@example.com"),
        "email must not be exposed"
    );
    assert!(
        h.claude.invocations().unwrap().is_empty(),
        "health checks aren't reviews"
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn explains_a_logged_out_or_missing_claude_cli() {
    let mut h = Harness::start().await.unwrap();
    h.claude.set_logged_in(false).unwrap();

    let doctor = h.client().doctor().await.unwrap();
    assert!(!doctor.ok());
    assert!(
        doctor
            .claude
            .tool
            .problem
            .as_deref()
            .unwrap()
            .contains("claude auth login")
    );

    h.restart_with(|c| c.claude_bin = Some("/nonexistent/claude".into()))
        .await
        .unwrap();
    let doctor = h.client().doctor().await.unwrap();
    assert!(!doctor.claude.tool.found);
    assert!(
        doctor
            .claude
            .tool
            .problem
            .as_deref()
            .unwrap()
            .contains("not found")
    );
    h.shutdown().await.unwrap();
}
