//! Thin client for the GitHub endpoints PRowl uses.

use std::time::Duration;

use reqwest::{StatusCode, header};
use serde::Deserialize;

const USER_AGENT: &str = concat!("prowl/", env!("CARGO_PKG_VERSION"));
pub(crate) const SCOPES: &str = "repo read:org";

#[derive(Debug, thiserror::Error)]
pub(crate) enum GitHubError {
    #[error("GitHub rejected the token")]
    Unauthorized,
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
        let response = self
            .http
            .get(format!("{}/user", self.api_url))
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(token)
            .send()
            .await?;
        Ok(ok(response).await?.json().await?)
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
