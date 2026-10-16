use std::time::Duration;

use axum::http::{Method, StatusCode};
use futures::StreamExt;
use prowl_e2e::{ClaudeResponse, Harness};

#[tokio::test]
async fn status_reports_version_and_uptime() {
    let h = Harness::start().await.unwrap();

    let status = h.client().status().await.unwrap();

    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(status.pid, std::process::id());
    assert!(status.started_at_ms > 0);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn requests_without_a_valid_token_are_rejected() {
    let h = Harness::start().await.unwrap();
    let client = h.client();

    let (missing, _) = client.get_raw("/v1/status", None).await.unwrap();
    let (wrong, _) = client.get_raw("/v1/status", Some("nope")).await.unwrap();

    assert_eq!(missing, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong, StatusCode::UNAUTHORIZED);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn event_stream_delivers_daemon_started() {
    let h = Harness::start().await.unwrap();

    let mut events = Box::pin(h.client().events(0).await.unwrap());
    let first = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("an event within 5s")
        .expect("stream open")
        .unwrap();

    assert_eq!(first.kind, "daemon.started");
    drop(events);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn events_persist_across_restarts() {
    let mut h = Harness::start().await.unwrap();
    h.restart().await.unwrap();

    let mut events = Box::pin(h.client().events(0).await.unwrap());
    let mut started = 0;
    while started < 2 {
        let event = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .expect("an event within 5s")
            .expect("stream open")
            .unwrap();
        if event.kind == "daemon.started" {
            started += 1;
        }
    }
    drop(events);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_second_daemon_cannot_take_over_a_running_home() {
    let h = Harness::start().await.unwrap();

    let err = prowld::start(prowld::Config::new(h.daemon().home()))
        .await
        .err()
        .expect("second start must fail");

    assert!(format!("{err:#}").contains("already listening"), "{err:#}");
    h.client()
        .status()
        .await
        .expect("first daemon still serving");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_daemon_never_invokes_claude_or_writes_to_github() {
    let h = Harness::start().await.unwrap();
    h.client().status().await.unwrap();

    assert!(h.claude.invocations().unwrap().is_empty());
    assert!(h.github.writes().is_empty());
    h.shutdown().await.unwrap();
}

/// Checks the fakes themselves, so later tests can trust them.
#[tokio::test]
async fn fakes_behave_as_scripted() {
    let h = Harness::start().await.unwrap();

    h.github.route(
        Method::GET,
        "/user",
        StatusCode::OK,
        serde_json::json!({ "login": "octo" }),
    );
    let body: serde_json::Value = http_get(&format!("{}/user", h.github.url())).await;
    assert_eq!(body["login"], "octo");

    h.claude
        .respond_with(vec![ClaudeResponse {
            stdout: "{\"ok\":true}".into(),
            ..Default::default()
        }])
        .unwrap();
    let output = tokio::process::Command::new(h.claude.bin())
        .args(["-p", "--output-format", "json"])
        .envs(h.claude.env())
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "{\"ok\":true}");

    let calls = h.claude.invocations().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].args, ["-p", "--output-format", "json"]);
    h.shutdown().await.unwrap();
}

/// Minimal HTTP GET over TCP so the harness tests need no HTTP client dependency.
async fn http_get(url: &str) -> serde_json::Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
    let request = format!("GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let body = response.split_once("\r\n\r\n").unwrap().1;
    serde_json::from_str(body).unwrap()
}
