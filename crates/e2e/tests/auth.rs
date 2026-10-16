use std::time::Duration;

use axum::http::{Method, StatusCode};
use prowl_e2e::{Harness, TEST_CLIENT_ID};
use prowld::{ApiClient, AuthState};
use serde_json::json;

fn script_device_flow(h: &Harness, token_answers: Vec<serde_json::Value>) {
    h.github.route(
        Method::POST,
        "/login/device/code",
        StatusCode::OK,
        json!({
            "device_code": "dev-123",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 0
        }),
    );
    h.github.route_seq(
        Method::POST,
        "/login/oauth/access_token",
        token_answers
            .into_iter()
            .map(|a| (StatusCode::OK, a))
            .collect(),
    );
    h.github.route(
        Method::GET,
        "/user",
        StatusCode::OK,
        json!({ "login": "octo", "name": "Octo Cat" }),
    );
}

async fn wait_for(client: &ApiClient, done: impl Fn(&AuthState) -> bool) -> AuthState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let state = client.auth_state().await.unwrap();
        if done(&state) {
            return state;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out; last state {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn connect(h: &Harness) {
    script_device_flow(
        h,
        vec![
            json!({ "error": "authorization_pending" }),
            json!({ "access_token": "gho_secret", "token_type": "bearer" }),
        ],
    );
    h.client().start_login().await.unwrap();
    wait_for(&h.client(), |s| matches!(s, AuthState::Connected { .. })).await;
}

#[tokio::test]
async fn device_flow_connects_the_account() {
    let h = Harness::start().await.unwrap();
    script_device_flow(
        &h,
        vec![
            json!({ "error": "authorization_pending" }),
            json!({ "error": "authorization_pending" }),
            json!({ "access_token": "gho_secret", "token_type": "bearer" }),
        ],
    );

    let pending = h.client().start_login().await.unwrap();
    assert!(matches!(
        &pending,
        AuthState::Pending { user_code, verification_uri, .. }
            if user_code == "ABCD-1234" && verification_uri == "https://github.com/login/device"
    ));

    let connected = wait_for(&h.client(), |s| matches!(s, AuthState::Connected { .. })).await;
    assert_eq!(
        connected,
        AuthState::Connected {
            login: "octo".into(),
            name: Some("Octo Cat".into())
        }
    );
    assert_eq!(h.client().status().await.unwrap().github, connected);

    let code_request = &h.github.requests_to(Method::POST, "/login/device/code")[0];
    assert_eq!(
        code_request.form_field("client_id").as_deref(),
        Some(TEST_CLIENT_ID)
    );
    assert_eq!(
        code_request.form_field("scope").as_deref(),
        Some("repo read:org")
    );
    assert_eq!(
        h.github
            .requests_to(Method::POST, "/login/oauth/access_token")
            .len(),
        3
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn connection_survives_a_restart() {
    let mut h = Harness::start().await.unwrap();
    connect(&h).await;

    h.restart().await.unwrap();

    assert!(matches!(
        h.client().auth_state().await.unwrap(),
        AuthState::Connected { login, .. } if login == "octo"
    ));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn disconnect_forgets_the_account_and_token() {
    let mut h = Harness::start().await.unwrap();
    connect(&h).await;
    let token_file = h.daemon().home().join("github-token");
    assert!(token_file.exists());

    let state = h.client().disconnect().await.unwrap();

    assert_eq!(state, AuthState::Disconnected);
    assert!(!token_file.exists());
    h.restart().await.unwrap();
    assert_eq!(
        h.client().auth_state().await.unwrap(),
        AuthState::Disconnected
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn revoked_token_requires_reconnect() {
    let mut h = Harness::start().await.unwrap();
    connect(&h).await;
    h.github.route(
        Method::GET,
        "/user",
        StatusCode::UNAUTHORIZED,
        json!({ "message": "Bad credentials" }),
    );

    h.restart().await.unwrap();

    let state = wait_for(&h.client(), |s| {
        matches!(s, AuthState::ReconnectRequired { .. })
    })
    .await;
    assert_eq!(
        state,
        AuthState::ReconnectRequired {
            login: "octo".into()
        }
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn denied_login_is_reported() {
    let h = Harness::start().await.unwrap();
    script_device_flow(&h, vec![json!({ "error": "access_denied" })]);

    h.client().start_login().await.unwrap();

    let state = wait_for(&h.client(), |s| matches!(s, AuthState::Failed { .. })).await;
    assert!(matches!(state, AuthState::Failed { reason } if reason.contains("denied")));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn login_without_a_client_id_explains_what_to_set() {
    let mut h = Harness::start().await.unwrap();
    h.restart_with(|c| c.github_client_id = None).await.unwrap();

    let err = h.client().start_login().await.unwrap_err();

    let message = err.to_string();
    assert!(message.contains("412"), "{message}");
    assert!(message.contains("PROWL_GITHUB_CLIENT_ID"), "{message}");
    assert!(h.github.requests().is_empty());
    h.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn token_file_fallback_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::start().await.unwrap();
    connect(&h).await;

    let mode = std::fs::metadata(h.daemon().home().join("github-token"))
        .unwrap()
        .permissions()
        .mode();

    assert_eq!(mode & 0o777, 0o600);
    h.shutdown().await.unwrap();
}
