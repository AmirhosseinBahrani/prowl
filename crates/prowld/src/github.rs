//! Thin client for the GitHub endpoints PRowl uses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;

use reqwest::{StatusCode, header};
use serde::Deserialize;

const USER_AGENT: &str = concat!("prowl/", env!("CARGO_PKG_VERSION"));
pub(crate) const SCOPES: &str = "repo read:org";

#[derive(Debug, thiserror::Error)]
pub(crate) enum GitHubError {
    #[error("GitHub rejected the token")]
    Unauthorized,
    #[error("GitHub rate limit reached; resets at {reset_at} (unix seconds)")]
    RateLimited { reset_at: u64 },
    #[error("GitHub returned {status}: {body}")]
    Status { status: StatusCode, body: String },
    #[error("GitHub request failed: {0}")]
    Transport(#[from] reqwest::Error),
}

#[derive(Clone)]
pub(crate) struct GitHub {
    http: reqwest::Client,
    api_url: String,
    web_url: String,
    /// ETag and body of the last 200 per URL, for conditional requests (a 304 is free).
    etags: Arc<Mutex<HashMap<String, (String, Bytes)>>>,
    rate: Arc<Mutex<RateLimit>>,
}

/// The API rate limit as last reported by GitHub.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RateLimit {
    pub remaining: Option<u64>,
    /// Unix seconds.
    pub reset_at: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// One answer from the device-flow token endpoint.
#[derive(Debug, PartialEq)]
pub(crate) enum TokenPoll {
    Granted(String),
    Pending,
    SlowDown { interval: u64 },
    Expired,
    Denied,
    Failed(String),
}

/// A repository as GitHub's REST API returns it (the fields PRowl uses).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ApiRepo {
    pub id: i64,
    pub full_name: String,
    pub name: String,
    pub owner: ApiOwner,
    pub private: bool,
    pub archived: bool,
    pub default_branch: String,
    pub html_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ApiOwner {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchResults {
    items: Vec<SearchItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchItem {
    number: u64,
    repository_url: String,
}

/// An open pull request found by the review-request search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrRef {
    pub repo: String,
    pub number: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PullRequest {
    pub number: u64,
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    pub title: String,
    pub html_url: String,
    pub user: ApiOwner,
    pub head: GitRef,
    #[serde(default)]
    pub requested_reviewers: Vec<ApiOwner>,
    #[serde(default)]
    pub requested_teams: Vec<TeamRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GitRef {
    pub sha: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TeamRef {
    pub slug: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Team {
    pub slug: String,
    pub organization: ApiOwner,
}

const PER_PAGE: usize = 100;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct User {
    pub login: String,
    pub name: Option<String>,
}

impl GitHub {
    pub(crate) fn new(api_url: &str, web_url: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            api_url: api_url.trim_end_matches('/').to_owned(),
            web_url: web_url.trim_end_matches('/').to_owned(),
            etags: Arc::default(),
            rate: Arc::default(),
        })
    }

    pub(crate) async fn request_device_code(
        &self,
        client_id: &str,
    ) -> Result<DeviceCode, GitHubError> {
        let response = self
            .http
            .post(format!("{}/login/device/code", self.web_url))
            .header(header::ACCEPT, "application/json")
            .form(&[("client_id", client_id), ("scope", SCOPES)])
            .send()
            .await?;
        Ok(ok(response).await?.json().await?)
    }

    pub(crate) async fn poll_device_token(
        &self,
        client_id: &str,
        device_code: &str,
    ) -> Result<TokenPoll, GitHubError> {
        #[derive(Deserialize)]
        struct Answer {
            access_token: Option<String>,
            error: Option<String>,
            interval: Option<u64>,
        }
        let response = self
            .http
            .post(format!("{}/login/oauth/access_token", self.web_url))
            .header(header::ACCEPT, "application/json")
            .form(&[
                ("client_id", client_id),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;
        let answer: Answer = ok(response).await?.json().await?;
        Ok(match (answer.access_token, answer.error.as_deref()) {
            (Some(token), _) => TokenPoll::Granted(token),
            (None, Some("authorization_pending")) => TokenPoll::Pending,
            (None, Some("slow_down")) => TokenPoll::SlowDown {
                interval: answer.interval.unwrap_or(10),
            },
            (None, Some("expired_token")) => TokenPoll::Expired,
            (None, Some("access_denied")) => TokenPoll::Denied,
            (None, other) => TokenPoll::Failed(other.unwrap_or("no token in response").to_owned()),
        })
    }

    pub(crate) async fn current_user(&self, token: &str) -> Result<User, GitHubError> {
        self.get_json(token, "/user", &[]).await
    }

    /// Every repository the account owns, collaborates on, or can see through an organisation.
    pub(crate) async fn list_repos(&self, token: &str) -> Result<Vec<ApiRepo>, GitHubError> {
        let mut repos = Vec::new();
        for page in 1.. {
            let page = page.to_string();
            let batch: Vec<ApiRepo> = self
                .get_json(
                    token,
                    "/user/repos",
                    &[
                        ("affiliation", "owner,collaborator,organization_member"),
                        ("sort", "full_name"),
                        ("per_page", &PER_PAGE.to_string()),
                        ("page", &page),
                    ],
                )
                .await?;
            let last = batch.len() < PER_PAGE;
            repos.extend(batch);
            if last {
                break;
            }
        }
        Ok(repos)
    }

    /// One repository, or `None` if it doesn't exist or the account can't see it.
    pub(crate) async fn get_repo(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
    ) -> Result<Option<ApiRepo>, GitHubError> {
        match self
            .get_json(token, &format!("/repos/{owner}/{repo}"), &[])
            .await
        {
            Ok(repo) => Ok(Some(repo)),
            Err(GitHubError::Status { status, .. }) if status == StatusCode::NOT_FOUND => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Open PRs where the account is a requested reviewer: directly, or also through a team.
    pub(crate) async fn review_requests(
        &self,
        token: &str,
        include_teams: bool,
    ) -> Result<Vec<PrRef>, GitHubError> {
        let qualifier = if include_teams {
            "review-requested:@me"
        } else {
            "user-review-requested:@me"
        };
        let q = format!("is:pr is:open archived:false {qualifier}");
        let mut found = Vec::new();
        for page in 1..=10 {
            let page = page.to_string();
            let results: SearchResults = self
                .get_json(
                    token,
                    "/search/issues",
                    &[
                        ("q", &q),
                        ("per_page", &PER_PAGE.to_string()),
                        ("page", &page),
                    ],
                )
                .await?;
            let last = results.items.len() < PER_PAGE;
            found.extend(results.items.into_iter().filter_map(|item| {
                let (_, repo) = item.repository_url.split_once("/repos/")?;
                Some(PrRef {
                    repo: repo.to_owned(),
                    number: item.number,
                })
            }));
            if last {
                break;
            }
        }
        Ok(found)
    }

    pub(crate) async fn pull_request(
        &self,
        token: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest, GitHubError> {
        self.get_json(token, &format!("/repos/{repo}/pulls/{number}"), &[])
            .await
    }

    /// Teams the account belongs to, across organisations.
    pub(crate) async fn my_teams(&self, token: &str) -> Result<Vec<Team>, GitHubError> {
        let mut teams = Vec::new();
        for page in 1.. {
            let page = page.to_string();
            let batch: Vec<Team> = self
                .get_json(
                    token,
                    "/user/teams",
                    &[("per_page", &PER_PAGE.to_string()), ("page", &page)],
                )
                .await?;
            let last = batch.len() < PER_PAGE;
            teams.extend(batch);
            if last {
                break;
            }
        }
        Ok(teams)
    }

    pub(crate) fn rate_limit(&self) -> RateLimit {
        *self.rate.lock().unwrap()
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        token: &str,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, GitHubError> {
        let request = self
            .http
            .get(format!("{}{path}", self.api_url))
            .query(query)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(token)
            .build()?;
        let key = format!("{token}\n{}", request.url());
        let cached = self.etags.lock().unwrap().get(&key).cloned();
        let mut request = request;
        if let Some((etag, _)) = &cached
            && let Ok(value) = header::HeaderValue::from_str(etag)
        {
            request.headers_mut().insert(header::IF_NONE_MATCH, value);
        }

        let response = self.http.execute(request).await?;
        self.record_rate_limit(&response);
        if response.status() == StatusCode::NOT_MODIFIED
            && let Some((_, body)) = cached
        {
            return serde_json::from_slice(&body).map_err(|e| GitHubError::Status {
                status: StatusCode::NOT_MODIFIED,
                body: format!("cached body unreadable: {e}"),
            });
        }
        let response = self.check(response).await?;
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().await?;
        let parsed = serde_json::from_slice(&body).map_err(|e| GitHubError::Status {
            status: StatusCode::OK,
            body: format!("unexpected response: {e}"),
        })?;
        if let Some(etag) = etag {
            self.etags.lock().unwrap().insert(key, (etag, body));
        }
        Ok(parsed)
    }

    fn record_rate_limit(&self, response: &reqwest::Response) {
        let number = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
        };
        if let Some(remaining) = number("x-ratelimit-remaining") {
            *self.rate.lock().unwrap() = RateLimit {
                remaining: Some(remaining),
                reset_at: number("x-ratelimit-reset"),
            };
        }
    }

    /// Like [`ok`], but recognises GitHub's rate-limit responses.
    async fn check(&self, response: reqwest::Response) -> Result<reqwest::Response, GitHubError> {
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS
            || (status == StatusCode::FORBIDDEN && self.rate_limit().remaining == Some(0))
        {
            let reset_at = self.rate_limit().reset_at.unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs())
                    + 60
            });
            return Err(GitHubError::RateLimited { reset_at });
        }
        ok(response).await
    }
}

async fn ok(response: reqwest::Response) -> Result<reqwest::Response, GitHubError> {
    match response.status() {
        s if s.is_success() => Ok(response),
        StatusCode::UNAUTHORIZED => Err(GitHubError::Unauthorized),
        status => Err(GitHubError::Status {
            status,
            body: response.text().await.unwrap_or_default(),
        }),
    }
}
