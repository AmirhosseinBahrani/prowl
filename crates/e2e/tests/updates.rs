//! The daemon checks for a newer PRowl release and reports it.

use std::time::Duration;

use axum::http::{Method, StatusCode};
use prowl_e2e::Harness;
use prowld::ApiClient;
use serde_json::json;

const LATEST: &str = "/repos/acme/prowl/releases/latest";

async fn with_release(tag: &str) -> Harness {
    let mut h = Harness::start().await.unwrap();
    h.connect_github("octo").await.unwrap();
    h.github.route(
        Method::GET,
        LATEST,
        StatusCode::OK,
        json!({ "tag_name": tag, "html_url": format!("https://github.com/acme/prowl/releases/tag/{tag}") }),
    );
    h.restart_with(|c| {
        c.update_repo = Some("acme/prowl".into());
        c.update_check_retry = Duration::from_millis(50);
    })
    .await
    .unwrap();
    h
}

async fn update_info(client: &ApiClient) -> prowld::updates::UpdateInfo {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(update) = client.status().await.unwrap().update {
            return update;
        }
        assert!(tokio::time::Instant::now() < deadline, "never checked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_newer_release_is_reported_and_announced_once() {
    let h = with_release("v99.0.0").await;

    let update = update_info(&h.client()).await;

    assert!(update.available);
    assert_eq!(update.latest, "v99.0.0");
    assert_eq!(update.current, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        update.url,
        "https://github.com/acme/prowl/releases/tag/v99.0.0"
    );
    // The event follows the status update closely; give it a moment, then make sure it's alone.
    let announced = || async {
        h.client()
            .recent_events(500)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "update.available")
            .count()
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while announced().await == 0 {
        assert!(tokio::time::Instant::now() < deadline, "never announced");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(announced().await, 1);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_current_release_is_not_an_update() {
    let h = with_release(&format!("v{}", env!("CARGO_PKG_VERSION"))).await;

    let update = update_info(&h.client()).await;

    assert!(!update.available);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn without_a_release_nothing_is_reported() {
    let mut h = Harness::start().await.unwrap();
    h.connect_github("octo").await.unwrap();
    h.restart_with(|c| {
        c.update_repo = Some("acme/prowl".into());
        c.update_check_retry = Duration::from_millis(50);
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        !h.github.requests_to(Method::GET, LATEST).is_empty(),
        "it checked"
    );
    assert_eq!(h.client().status().await.unwrap().update, None);
    h.shutdown().await.unwrap();
}
