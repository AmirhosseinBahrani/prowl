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
use crate::policy::{EffectivePolicy, PolicyVersion, Scope};
use crate::server::Status;
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
        let path = format!("/v1/events?since={since}");
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
