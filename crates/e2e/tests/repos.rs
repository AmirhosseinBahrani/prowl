use axum::http::{Method, StatusCode};
use futures::StreamExt;
use prowl_e2e::Harness;
use prowld::RepoQuery;
use prowld::client::ClientError;
use serde_json::{Value, json};

fn repo(id: i64, full_name: &str, archived: bool) -> Value {
    let (owner, name) = full_name.split_once('/').unwrap();
    json!({
        "id": id,
        "full_name": full_name,
        "name": name,
        "owner": { "login": owner },
        "private": id % 2 == 0,
        "archived": archived,
        "default_branch": "main",
        "html_url": format!("https://github.com/{full_name}"),
    })
}

fn serve_repos(h: &Harness, repos: Vec<Value>) {
    h.github.route(
        Method::GET,
        "/user/repos",
        StatusCode::OK,
        Value::Array(repos),
    );
}

async fn connected() -> Harness {
    let h = Harness::start().await.unwrap();
    h.connect_github("octo").await.unwrap();
    h
}

fn status_of(err: ClientError) -> u16 {
    match err {
        ClientError::Status { status, .. } => status.as_u16(),
        other => panic!("expected an HTTP status error, got {other:?}"),
    }
}

fn query(q: &str) -> RepoQuery {
    RepoQuery {
        q: Some(q.into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn lists_personal_and_org_repositories() {
    let h = connected().await;
    serve_repos(
        &h,
        vec![
            repo(1, "octo/dotfiles", false),
            repo(2, "acme-org/payments", false),
            repo(3, "acme-org/legacy", true),
        ],
    );

    let page = h.client().repos(&RepoQuery::default()).await.unwrap();

    let names: Vec<_> = page.repos.iter().map(|r| r.full_name.as_str()).collect();
    assert_eq!(
        names,
        ["acme-org/legacy", "acme-org/payments", "octo/dotfiles"]
    );
    assert_eq!(page.total, 3);
    assert!(page.repos.iter().all(|r| !r.watched));
    assert!(page.repos[0].archived);
    let request = &h.github.requests_to(Method::GET, "/user/repos")[0];
    assert!(
        request
            .query
            .as_deref()
            .unwrap()
            .contains("affiliation=owner%2Ccollaborator%2Corganization_member")
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn follows_github_pagination() {
    let h = connected().await;
    let first: Vec<_> = (0..100)
        .map(|i| repo(i + 1, &format!("octo/repo-{i:03}"), false))
        .collect();
    h.github.route_when(
        Method::GET,
        "/user/repos",
        "page=1",
        StatusCode::OK,
        Value::Array(first),
    );
    h.github.route_when(
        Method::GET,
        "/user/repos",
        "page=2",
        StatusCode::OK,
        json!([repo(1000, "octo/zzz-last", false)]),
    );

    let page = h.client().repos(&RepoQuery::default()).await.unwrap();

    assert_eq!(page.total, 101);
    assert_eq!(h.github.requests_to(Method::GET, "/user/repos").len(), 2);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn search_and_pages_are_applied() {
    let h = connected().await;
    serve_repos(
        &h,
        (1..=30)
            .map(|i| repo(i, &format!("acme-org/svc-{i:02}"), false))
            .chain([repo(99, "octo/dotfiles", false)])
            .collect(),
    );
    let client = h.client();

    let found = client.repos(&query("DOTF")).await.unwrap();
    assert_eq!(found.total, 1);
    assert_eq!(found.repos[0].full_name, "octo/dotfiles");

    let second = client
        .repos(&RepoQuery {
            q: Some("svc".into()),
            page: Some(2),
            per_page: Some(10),
            refresh: false,
        })
        .await
        .unwrap();
    assert_eq!(second.total, 30);
    assert_eq!(second.repos.len(), 10);
    assert_eq!(second.repos[0].full_name, "acme-org/svc-11");
    // Served from the cache after the first listing.
    assert_eq!(h.github.requests_to(Method::GET, "/user/repos").len(), 1);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn watching_persists_across_restarts() {
    let mut h = connected().await;
    serve_repos(
        &h,
        vec![
            repo(1, "octo/dotfiles", false),
            repo(2, "acme-org/payments", false),
        ],
    );
    h.client().repos(&RepoQuery::default()).await.unwrap();
    let mut events = Box::pin(h.client().events(0).await.unwrap());

    let watched = h.client().watch("acme-org/payments").await.unwrap();
    assert!(watched.watched);
    loop {
        let event = events.next().await.unwrap().unwrap();
        if event.kind == "repo.watched" {
            assert_eq!(event.payload["repo"], "acme-org/payments");
            break;
        }
    }
    drop(events);

    h.restart().await.unwrap();
    let names: Vec<_> = h
        .client()
        .watched_repos()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.full_name)
        .collect();
    assert_eq!(names, ["acme-org/payments"]);

    h.client().unwatch("acme-org/payments").await.unwrap();
    assert!(h.client().watched_repos().await.unwrap().is_empty());
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn archived_repositories_cannot_be_watched() {
    let h = connected().await;
    serve_repos(&h, vec![repo(3, "acme-org/legacy", true)]);
    h.client().repos(&RepoQuery::default()).await.unwrap();

    let err = h.client().watch("acme-org/legacy").await.unwrap_err();

    assert_eq!(status_of(err), 422);
    assert!(h.client().watched_repos().await.unwrap().is_empty());
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn repositories_outside_the_cache_are_checked_on_github() {
    let h = connected().await;
    serve_repos(&h, vec![]);
    h.github.route(
        Method::GET,
        "/repos/acme-org/brand-new",
        StatusCode::OK,
        repo(7, "acme-org/brand-new", false),
    );

    assert!(
        h.client()
            .watch("acme-org/brand-new")
            .await
            .unwrap()
            .watched
    );
    assert_eq!(
        status_of(h.client().watch("acme-org/missing").await.unwrap_err()),
        404
    );
    assert_eq!(
        status_of(h.client().watch("acme-org/..").await.unwrap_err()),
        422
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn refresh_drops_vanished_repos_but_keeps_watched_ones() {
    let h = connected().await;
    serve_repos(
        &h,
        vec![repo(1, "octo/old", false), repo(2, "octo/watched", false)],
    );
    let client = h.client();
    client.repos(&RepoQuery::default()).await.unwrap();
    client.watch("octo/watched").await.unwrap();

    serve_repos(&h, vec![repo(3, "octo/new", false)]);
    let page = client
        .repos(&RepoQuery {
            refresh: true,
            ..Default::default()
        })
        .await
        .unwrap();

    let names: Vec<_> = page.repos.iter().map(|r| r.full_name.as_str()).collect();
    assert_eq!(names, ["octo/new", "octo/watched"]);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn listing_requires_a_connected_account() {
    let h = Harness::start().await.unwrap();

    let err = h.client().repos(&RepoQuery::default()).await.unwrap_err();

    assert_eq!(status_of(err), 409);
    assert!(h.github.requests().is_empty());
    h.shutdown().await.unwrap();
}
