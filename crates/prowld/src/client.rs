//! Client for the daemon's Local API, used by the CLI, the desktop app and tests.

use std::path::Path;

use bytes::Bytes;
use futures::Stream;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode, header};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;

use crate::events::Event;
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

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let (status, body) = self.get_raw(path, Some(&self.token)).await?;
        if !status.is_success() {
            return Err(ClientError::Status {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        Ok(serde_json::from_slice(&body)?)
    }

    /// Low-level GET with an explicit token (or none), returning status and body.
    pub async fn get_raw(
        &self,
        path: &str,
        token: Option<&str>,
    ) -> Result<(StatusCode, Bytes), ClientError> {
        let response = self.send(path, token).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        Ok((status, body))
    }

    /// Streams events with id greater than `since`: stored ones first, then live ones.
    pub async fn events(
        &self,
        since: i64,
    ) -> Result<impl Stream<Item = Result<Event, ClientError>> + Send + 'static, ClientError> {
        let response = self
            .send(&format!("/v1/events?since={since}"), Some(&self.token))
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.into_body().collect().await?.to_bytes();
            return Err(ClientError::Status {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        Ok(sse_events(response.into_body()))
    }

    async fn send(
        &self,
        path: &str,
        token: Option<&str>,
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

        let mut request = Request::get(path).header(header::HOST, "prowld");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = request
            .body(Empty::<Bytes>::new())
            .expect("static request parts are valid");
        Ok(sender.send_request(request).await?)
    }
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
