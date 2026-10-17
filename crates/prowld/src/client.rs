//! Client for the daemon's Local API, used by the CLI, the desktop app and tests.

use std::path::Path;

use bytes::Bytes;
use futures::Stream;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode, header};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;

use crate::auth::AuthState;
use crate::events::Event;
use crate::jobs::Job;
use crate::policy::{EffectivePolicy, PolicySummary, PolicyVersion, Scope};
use crate::poller::PollSummary;
use crate::power::PowerStatus;
use crate::queue::{QueueDetail, QueueItem};
use crate::repos::{Repo, RepoPage, RepoQuery};
use crate::reviews::Review;
use crate::server::{Costs, Status};
use crate::transport::{self, Endpoint};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach prowld at {endpoint}: {source}")]
    Unreachable {
        endpoint: Endpoint,
        source: std::io::Error,
    },
    #[error("HTTP error: {0}")]
    Http(#[from] hyper::Error),
    #[error("prowld returned {status}: {body}")]
    Status { status: StatusCode, body: String },
    #[error("invalid response: {0}")]
    Decode(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct ApiClient {
    endpoint: Endpoint,
    token: String,
}

impl ApiClient {
    pub fn new(endpoint: Endpoint, token: impl Into<String>) -> Self {
        Self {
            endpoint,
            token: token.into(),
        }
    }

    /// Connects to the daemon that owns `home`, reading its API token from disk.
    pub fn for_home(home: &Path) -> anyhow::Result<Self> {
        Ok(Self::new(
            Endpoint::for_home(home),
            crate::config::read_token(home)?,
        ))
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub async fn status(&self) -> Result<Status, ClientError> {
        self.get_json("/v1/status").await
    }

    pub async fn auth_state(&self) -> Result<AuthState, ClientError> {
        self.get_json("/v1/auth").await
    }

    /// Starts a GitHub device-flow login; the returned state holds the code to show the user.
    pub async fn start_login(&self) -> Result<AuthState, ClientError> {
        self.call_json(Method::POST, "/v1/auth/device").await
    }

    /// Connects with a personal access token; GitHub must accept it.
    pub async fn sign_in_with_token(&self, token: &str) -> Result<AuthState, ClientError> {
        let body = serde_json::json!({ "token": token });
        self.call_json_body(Method::POST, "/v1/auth/token", Some(body))
            .await
    }

    pub async fn disconnect(&self) -> Result<AuthState, ClientError> {
        self.call_json(Method::DELETE, "/v1/auth").await
    }

    /// The policy that applies to `repo` (`owner/repo`), or the global one.
    pub async fn effective_policy(
        &self,
        repo: Option<&str>,
    ) -> Result<EffectivePolicy, ClientError> {
        match repo {
            Some(repo) => self.get_json(&format!("/v1/policy?repo={repo}")).await,
            None => self.get_json("/v1/policy").await,
        }
    }

    /// The global policy and every per-repository override, with their files.
    pub async fn policies(&self) -> Result<Vec<PolicySummary>, ClientError> {
        self.get_json("/v1/policies").await
    }

    /// Reviews `repo#pr` as a real run would, publishing nothing.
    pub async fn dry_run(&self, repo: &str, pr: u64) -> Result<crate::worker::DryRun, ClientError> {
        let body = serde_json::json!({ "repo": repo, "pr": pr });
        self.call_json_body(Method::POST, "/v1/dry-run", Some(body))
            .await
    }

    pub async fn policy(&self, scope: &Scope) -> Result<EffectivePolicy, ClientError> {
        self.get_json(&policy_path(scope)).await
    }

    pub async fn save_policy(
        &self,
        scope: &Scope,
        content: &str,
    ) -> Result<EffectivePolicy, ClientError> {
        let body = serde_json::json!({ "content": content });
        self.call_json_body(Method::PUT, &policy_path(scope), Some(body))
            .await
    }

    pub async fn delete_policy(&self, scope: &Scope) -> Result<(), ClientError> {
        let response = self
            .send(Method::DELETE, &policy_path(scope), Some(&self.token), None)
            .await?;
        success_body(response).await.map(drop)
    }

    pub async fn policy_versions(&self, scope: &Scope) -> Result<Vec<PolicyVersion>, ClientError> {
        self.get_json(&format!("{}/versions", policy_path(scope)))
            .await
    }

    pub async fn restore_policy(
        &self,
        scope: &Scope,
        version_id: i64,
    ) -> Result<EffectivePolicy, ClientError> {
        let path = format!("{}/versions/{version_id}/restore", policy_path(scope));
        self.call_json(Method::POST, &path).await
    }

    /// Repositories the connected account can see, filtered and paginated.
    pub async fn repos(&self, query: &RepoQuery) -> Result<RepoPage, ClientError> {
        let mut params = Vec::new();
        if let Some(q) = &query.q {
            params.push(format!("q={}", percent_encode(q)));
        }
        if let Some(page) = query.page {
            params.push(format!("page={page}"));
        }
        if let Some(per_page) = query.per_page {
            params.push(format!("per_page={per_page}"));
        }
        if query.refresh {
            params.push("refresh=true".into());
        }
        self.get_json(&format!("/v1/repos?{}", params.join("&")))
            .await
    }

    /// Polls GitHub for review requests now and returns what happened.
    /// Checks that `repo#pr`'s findings were addressed and approves it if so; returns the job.
    pub async fn verify(&self, repo: &str, pr: u64) -> Result<Job, ClientError> {
        let body = serde_json::json!({ "repo": repo, "pr": pr });
        self.call_json_body(Method::POST, "/v1/verify", Some(body))
            .await
    }

    /// The pull request's review threads and whether each is resolved.
    pub async fn threads(
        &self,
        repo: &str,
        pr: u64,
    ) -> Result<Vec<crate::ReviewThread>, ClientError> {
        self.get_json(&format!("/v1/threads?repo={repo}&pr={pr}"))
            .await
    }

    /// Reviews `repo#pr` again at its current commit; returns the queued job.
    pub async fn rerun(&self, repo: &str, pr: u64) -> Result<Job, ClientError> {
        let body = serde_json::json!({ "repo": repo, "pr": pr });
        self.call_json_body(Method::POST, "/v1/rerun", Some(body))
            .await
    }

    pub async fn poll_now(&self) -> Result<PollSummary, ClientError> {
        self.call_json(Method::POST, "/v1/poll").await
    }

    pub async fn reviews(&self, job_id: Option<i64>) -> Result<Vec<Review>, ClientError> {
        match job_id {
            Some(id) => self.get_json(&format!("/v1/reviews?job_id={id}")).await,
            None => self.get_json("/v1/reviews").await,
        }
    }

    /// PRs waiting for the user's review, riskiest first.
    pub async fn queue(&self) -> Result<Vec<QueueItem>, ClientError> {
        self.get_json("/v1/queue").await
    }

    /// One queued PR with its diff.
    pub async fn queue_item(&self, repo: &str, number: u64) -> Result<QueueDetail, ClientError> {
        self.get_json(&format!("/v1/queue/{repo}/{number}")).await
    }

    /// Approves the PR on GitHub as the user and removes it from the queue.
    pub async fn approve(
        &self,
        repo: &str,
        number: u64,
        comment: &str,
    ) -> Result<QueueItem, ClientError> {
        let body = serde_json::json!({ "comment": comment });
        self.call_json_body(
            Method::POST,
            &format!("/v1/queue/{repo}/{number}/approve"),
            Some(body),
        )
        .await
    }

    pub async fn request_changes(
        &self,
        repo: &str,
        number: u64,
        comment: &str,
    ) -> Result<QueueItem, ClientError> {
        let body = serde_json::json!({ "comment": comment });
        self.call_json_body(
            Method::POST,
            &format!("/v1/queue/{repo}/{number}/request-changes"),
            Some(body),
        )
        .await
    }

    /// Removes the PR from the queue without doing anything on GitHub.
    pub async fn dismiss(&self, repo: &str, number: u64) -> Result<QueueItem, ClientError> {
        self.call_json(Method::POST, &format!("/v1/queue/{repo}/{number}/dismiss"))
            .await
    }

    pub async fn settings(&self) -> Result<crate::settings::Settings, ClientError> {
        self.get_json("/v1/settings").await
    }

    /// Changes the given settings (a JSON object with any subset of fields).
    pub async fn update_settings(
        &self,
        patch: serde_json::Value,
    ) -> Result<crate::settings::Settings, ClientError> {
        self.call_json_body(Method::PUT, "/v1/settings", Some(patch))
            .await
    }

    /// Today's Claude spend against the daily budget.
    pub async fn costs(&self) -> Result<Costs, ClientError> {
        self.get_json("/v1/costs").await
    }

    /// Whether the claude CLI and git are usable by the daemon.
    pub async fn doctor(&self) -> Result<crate::doctor::Doctor, ClientError> {
        self.get_json("/v1/doctor").await
    }

    pub async fn power(&self) -> Result<PowerStatus, ClientError> {
        self.get_json("/v1/power").await
    }

    pub async fn jobs(&self) -> Result<Vec<Job>, ClientError> {
        self.get_json("/v1/jobs").await
    }

    /// Everything a job's review runs saw and did, oldest first.
    pub async fn transcript(
        &self,
        job_id: i64,
    ) -> Result<Vec<crate::transcripts::TranscriptEntry>, ClientError> {
        self.get_json(&format!("/v1/jobs/{job_id}/transcript"))
            .await
    }

    pub async fn watched_repos(&self) -> Result<Vec<Repo>, ClientError> {
        self.get_json("/v1/repos/watched").await
    }

    pub async fn watch(&self, full_name: &str) -> Result<Repo, ClientError> {
        self.call_json(Method::PUT, &format!("/v1/repos/{full_name}/watch"))
            .await
    }

    pub async fn unwatch(&self, full_name: &str) -> Result<Repo, ClientError> {
        self.call_json(Method::DELETE, &format!("/v1/repos/{full_name}/watch"))
            .await
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        self.call_json(Method::GET, path).await
    }

    async fn call_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
    ) -> Result<T, ClientError> {
        self.call_json_body(method, path, None).await
    }

    async fn call_json_body<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, ClientError> {
        let response = self.send(method, path, Some(&self.token), body).await?;
        let body = success_body(response).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    /// Low-level GET with an explicit token (or none), returning status and body.
    pub async fn get_raw(
        &self,
        path: &str,
        token: Option<&str>,
    ) -> Result<(StatusCode, Bytes), ClientError> {
        let response = self.send(Method::GET, path, token, None).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        Ok((status, body))
    }

    /// Streams events with id greater than `since`: stored ones first, then live ones.
    pub async fn events(
        &self,
        since: i64,
    ) -> Result<impl Stream<Item = Result<Event, ClientError>> + Send + 'static, ClientError> {
        self.event_stream(format!("/v1/events?since={since}")).await
    }

    /// The latest `limit` events, oldest first.
    pub async fn recent_events(&self, limit: usize) -> Result<Vec<Event>, ClientError> {
        self.get_json(&format!("/v1/events/recent?limit={limit}"))
            .await
    }

    /// Streams only events emitted from now on.
    pub async fn live_events(
        &self,
    ) -> Result<impl Stream<Item = Result<Event, ClientError>> + Send + 'static, ClientError> {
        self.event_stream("/v1/events?live=true".to_owned()).await
    }

    async fn event_stream(
        &self,
        path: String,
    ) -> Result<impl Stream<Item = Result<Event, ClientError>> + Send + 'static, ClientError> {
        let response = self
            .send(Method::GET, &path, Some(&self.token), None)
            .await?;
        if !response.status().is_success() {
            return Err(success_body(response).await.unwrap_err());
        }
        Ok(sse_events(response.into_body()))
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> Result<Response<Incoming>, ClientError> {
        let io = transport::connect(&self.endpoint).await.map_err(|source| {
            ClientError::Unreachable {
                endpoint: self.endpoint.clone(),
                source,
            }
        })?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "prowld");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = match body {
            Some(json) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Bytes::from(json.to_string())
            }
            None => Bytes::new(),
        };
        let request = request
            .body(Full::new(body))
            .expect("static request parts are valid");
        Ok(sender.send_request(request).await?)
    }
}

/// Percent-encodes a query-string value.
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn policy_path(scope: &Scope) -> String {
    match scope {
        Scope::Global => "/v1/policies/global".into(),
        Scope::Repo { owner, repo } => format!("/v1/policies/repos/{owner}/{repo}"),
    }
}

/// Collects the body of a 2xx response, or turns anything else into `ClientError::Status`,
/// preferring the daemon's `{"error": "..."}` message.
async fn success_body(response: Response<Incoming>) -> Result<Bytes, ClientError> {
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    if status.is_success() {
        return Ok(body);
    }
    let message = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
    Err(ClientError::Status {
        status,
        body: message,
    })
}

/// Parses a server-sent-events body into `Event`s, ignoring keep-alive comments.
fn sse_events(body: Incoming) -> impl Stream<Item = Result<Event, ClientError>> + Send + 'static {
    async_stream::stream! {
        let mut body = body;
        let mut buf = String::new();
        while let Some(frame) = body.frame().await {
            let frame = match frame {
                Ok(frame) => frame,
                Err(e) => {
                    yield Err(ClientError::Http(e));
                    return;
                }
            };
            let Ok(data) = frame.into_data() else { continue };
            buf.push_str(&String::from_utf8_lossy(&data));
            while let Some(end) = buf.find("\n\n") {
                let block: String = buf.drain(..end + 2).collect();
                let data: Vec<&str> = block
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect();
                if !data.is_empty() {
                    yield serde_json::from_str(&data.join("\n")).map_err(ClientError::from);
                }
            }
        }
    }
}
