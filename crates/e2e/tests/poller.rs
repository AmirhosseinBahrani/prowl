use std::time::Duration;

use axum::http::{Method, StatusCode};
use futures::StreamExt;
use prowl_e2e::Harness;
use prowld::{Event, RepoQuery};
use serde_json::{Value, json};

const WATCHED: &str = "acme/api";

fn repo(id: i64, full_name: &str) -> Value {
    let (owner, name) = full_name.split_once('/').unwrap();
    json!({
        "id": id, "full_name": full_name, "name": name, "owner": { "login": owner },
        "private": false, "archived": false, "default_branch": "main",
        "html_url": format!("https://github.com/{full_name}"),
    })
}

struct Pr {
    repo: &'static str,
    number: u64,
    sha: &'static str,
    author: &'static str,
    reviewers: Vec<&'static str>,
    teams: Vec<&'static str>,
    draft: bool,
    state: &'static str,
}

impl Pr {
    fn requested(number: u64) -> Self {
        Self {
            repo: WATCHED,
            number,
            sha: "abc123",
            author: "alice",
            reviewers: vec!["octo"],
            teams: vec![],
            draft: false,
            state: "open",
        }
    }

    fn json(&self) -> Value {
        json!({
            "number": self.number,
            "state": self.state,
            "draft": self.draft,
            "title": format!("PR {}", self.number),
            "html_url": format!("https://github.com/{}/pull/{}", self.repo, self.number),
            "user": { "login": self.author },
            "head": { "sha": self.sha },
            "requested_reviewers": self.reviewers.iter().map(|l| json!({ "login": l })).collect::<Vec<_>>(),
            "requested_teams": self.teams.iter().map(|s| json!({ "slug": s })).collect::<Vec<_>>(),
        })
    }
}

/// Serves `prs` from the review-request search and their detail endpoints.
fn serve(h: &Harness, prs: &[Pr]) {
    let items: Vec<Value> = prs
        .iter()
        .map(|p| {
            json!({
                "number": p.number,
                "repository_url": format!("https://api.github.com/repos/{}", p.repo),
            })
        })
        .collect();
    h.github.route(
        Method::GET,
        "/search/issues",
        StatusCode::OK,
        json!({ "total_count": items.len(), "items": items }),
    );
    for pr in prs {
        h.github.route(
            Method::GET,
            &format!("/repos/{}/pulls/{}", pr.repo, pr.number),
            StatusCode::OK,
            pr.json(),
        );
    }
}

/// Connected as `octo`, watching `acme/api`.
async fn watching() -> Harness {
    let h = Harness::start().await.unwrap();
    h.connect_github("octo").await.unwrap();
    h.github.route(
        Method::GET,
        "/user/repos",
        StatusCode::OK,
        json!([repo(1, WATCHED), repo(2, "acme/ignored")]),
    );
    h.client().repos(&RepoQuery::default()).await.unwrap();
    h.client().watch(WATCHED).await.unwrap();
    h
}

async fn events_of(h: &Harness, kind: &str) -> Vec<Event> {
    let mut stream = Box::pin(h.client().events(0).await.unwrap());
    let mut found = Vec::new();
    // Drain what's stored; stop once the stream goes quiet.
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(200), stream.next()).await
    {
        let event = event.unwrap();
        if event.kind == kind {
            found.push(event);
        }
    }
    found
}

#[tokio::test]
async fn a_requested_review_is_queued_exactly_once() {
    let mut h = watching().await;
    serve(&h, &[Pr::requested(7)]);

    let first = h.client().poll_now().await.unwrap();
    let second = h.client().poll_now().await.unwrap();
    h.restart().await.unwrap();
    let third = h.client().poll_now().await.unwrap();

    assert_eq!((first.candidates, first.enqueued), (1, 1));
    assert_eq!(second.enqueued, 0);
    assert_eq!(third.enqueued, 0);
    let jobs = h.client().jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        (
            jobs[0].repo.as_str(),
            jobs[0].pr_number,
            jobs[0].head_sha.as_str(),
            jobs[0].status.as_str()
        ),
        (WATCHED, 7, "abc123", "queued")
    );
    assert_eq!(events_of(&h, "pr.enqueued").await.len(), 1);
    assert!(
        h.claude.invocations().unwrap().is_empty(),
        "polling must never run Claude"
    );
    assert!(
        h.github
            .writes()
            .iter()
            .all(|w| w.path.starts_with("/login/"))
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn each_gate_check_reports_its_reason_once() {
    let h = watching().await;
    serve(
        &h,
        &[
            Pr {
                draft: true,
                ..Pr::requested(1)
            },
            Pr {
                author: "octo",
                ..Pr::requested(2)
            },
            Pr {
                repo: "acme/ignored",
                ..Pr::requested(3)
            },
            Pr {
                state: "closed",
                ..Pr::requested(4)
            },
            Pr {
                reviewers: vec!["someone-else"],
                ..Pr::requested(5)
            },
        ],
    );

    let summary = h.client().poll_now().await.unwrap();
    h.client().poll_now().await.unwrap();

    assert_eq!(
        (summary.candidates, summary.enqueued, summary.gated_out),
        (5, 0, 5)
    );
    assert!(h.client().jobs().await.unwrap().is_empty());
    let mut reasons: Vec<(u64, String)> = events_of(&h, "pr.gated_out")
        .await
        .into_iter()
        .map(|e| {
            (
                e.payload["pr"].as_u64().unwrap(),
                e.payload["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    reasons.sort();
    assert_eq!(
        reasons,
        [
            (1, "draft".to_owned()),
            (2, "own_pull_request".to_owned()),
            (3, "repo_not_watched".to_owned()),
            (4, "not_open".to_owned()),
            (5, "not_requested".to_owned()),
        ],
        "each reason reported once despite two polls"
    );
    // The unwatched repo's PR was never fetched.
    assert!(
        h.github
            .requests_to(Method::GET, "/repos/acme/ignored/pulls/3")
            .is_empty()
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn team_requests_count_only_when_enabled() {
    let mut h = watching().await;
    serve(
        &h,
        &[Pr {
            reviewers: vec![],
            teams: vec!["backend"],
            ..Pr::requested(9)
        }],
    );
    h.github.route(
        Method::GET,
        "/user/teams",
        StatusCode::OK,
        json!([{ "slug": "backend", "organization": { "login": "acme" } }]),
    );

    h.client().poll_now().await.unwrap();
    assert!(h.client().jobs().await.unwrap().is_empty());
    let search = h.github.requests_to(Method::GET, "/search/issues");
    assert!(
        search[0]
            .query
            .as_deref()
            .unwrap()
            .contains("user-review-requested%3A%40me")
    );

    h.restart_with(|c| c.include_team_requests = true)
        .await
        .unwrap();
    h.client().poll_now().await.unwrap();

    assert_eq!(h.client().jobs().await.unwrap().len(), 1);
    let search = h.github.requests_to(Method::GET, "/search/issues");
    let last = search.last().unwrap().query.as_deref().unwrap();
    assert!(last.contains("+review-requested%3A%40me"), "{last}");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn repeat_polls_use_conditional_requests() {
    let h = watching().await;
    serve(&h, &[Pr::requested(7)]);

    h.client().poll_now().await.unwrap();
    h.client().poll_now().await.unwrap();

    let detail = h.github.requests_to(Method::GET, "/repos/acme/api/pulls/7");
    assert_eq!(detail.len(), 2);
    assert!(!detail[0].headers.contains_key("if-none-match"));
    assert!(detail[1].headers.contains_key("if-none-match"));
    assert_eq!(detail[1].status, 304);
    assert_eq!(h.client().jobs().await.unwrap().len(), 1);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn polling_pauses_when_the_rate_limit_runs_low() {
    let h = watching().await;
    serve(&h, &[Pr::requested(7)]);
    let reset_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    h.github.set_rate_limit(5, reset_at);
    h.client().poll_now().await.unwrap();
    let before = h.github.requests().len();

    let summary = h.client().poll_now().await.unwrap();

    assert_eq!(summary.skipped.as_deref(), Some("rate_limited"));
    assert_eq!(
        h.github.requests().len(),
        before,
        "no requests while rate limited"
    );
    let limited = events_of(&h, "poll.rate_limited").await;
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].payload["reset_at"], reset_at);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn nothing_is_requested_without_an_account_or_watched_repos() {
    let h = Harness::start().await.unwrap();
    assert_eq!(
        h.client().poll_now().await.unwrap().skipped.as_deref(),
        Some("not_connected")
    );
    h.connect_github("octo").await.unwrap();
    let before = h.github.requests().len();

    let summary = h.client().poll_now().await.unwrap();

    assert_eq!(summary.skipped.as_deref(), Some("no_watched_repos"));
    assert_eq!(h.github.requests().len(), before);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_scheduler_polls_on_its_own() {
    let mut h = watching().await;
    serve(&h, &[Pr::requested(7)]);

    h.restart_with(|c| c.poll_interval = Duration::from_millis(50))
        .await
        .unwrap();

    // The first tick fires at startup...
    wait_for_jobs(&h, 1).await;
    // ...and later ones keep coming on the interval.
    serve(&h, &[Pr::requested(7), Pr::requested(8)]);
    wait_for_jobs(&h, 2).await;
    h.shutdown().await.unwrap();
}

async fn wait_for_jobs(h: &Harness, n: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while h.client().jobs().await.unwrap().len() < n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no scheduled poll happened"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
