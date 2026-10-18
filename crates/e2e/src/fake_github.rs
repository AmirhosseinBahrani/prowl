use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    /// The body parsed as JSON, if it was JSON.
    pub body: Option<serde_json::Value>,
    /// The body as sent (e.g. a form-encoded OAuth request).
    pub raw_body: String,
    /// Request headers, names lower-cased.
    pub headers: HashMap<String, String>,
    /// The status the fake answered with.
    pub status: u16,
}

impl RecordedRequest {
    /// A field from a form-encoded body.
    pub fn form_field(&self, name: &str) -> Option<String> {
        self.raw_body.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == name).then(|| v.replace('+', " ").replace("%3A", ":").replace("%20", " "))
        })
    }
}

#[derive(Default)]
struct Inner {
    /// Responses per route, served in order; the last one repeats.
    routes: HashMap<(Method, String), VecDeque<(StatusCode, serde_json::Value)>>,
    /// Routes that also require a query-string fragment; checked before `routes`.
    query_routes: Vec<(Method, String, String, (StatusCode, serde_json::Value))>,
    /// `(remaining, reset_at)` sent as X-RateLimit headers on every response.
    rate_limit: Option<(u64, u64)>,
    /// `(limit, remaining, reset_at)` for `/search/*`, reported as the `search` resource.
    search_rate_limit: Option<(u64, u64, u64)>,
    /// Unified diffs served for `Accept: application/vnd.github.diff`, by path.
    diffs: HashMap<String, String>,
    requests: Vec<RecordedRequest>,
}

/// An HTTP server standing in for api.github.com: scripted responses, every request recorded.
#[derive(Clone)]
pub struct FakeGitHub {
    url: String,
    inner: Arc<Mutex<Inner>>,
}

impl FakeGitHub {
    pub async fn start() -> anyhow::Result<Self> {
        let mut state = Inner::default();
        // A pull request nobody has discussed: no review threads, no review requests.
        state.routes.insert(
            (Method::POST, "/graphql".to_owned()),
            vec![(StatusCode::OK, empty_pull_request())].into(),
        );
        let inner = Arc::new(Mutex::new(state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let app = Router::new().fallback(handle).with_state(inner.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { url, inner })
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// Scripts the response for `method path` (path without query string).
    pub fn route(&self, method: Method, path: &str, status: StatusCode, body: serde_json::Value) {
        self.route_seq(method, path, vec![(status, body)]);
    }

    /// Scripts successive responses for `method path`; the last one repeats.
    pub fn route_seq(
        &self,
        method: Method,
        path: &str,
        responses: Vec<(StatusCode, serde_json::Value)>,
    ) {
        assert!(!responses.is_empty(), "a route needs at least one response");
        self.inner
            .lock()
            .unwrap()
            .routes
            .insert((method, path.to_owned()), responses.into());
    }

    /// Scripts the response for `method path` when the query string contains `query`
    /// (e.g. `page=2`). Takes precedence over [`route`](Self::route).
    pub fn route_when(
        &self,
        method: Method,
        path: &str,
        query: &str,
        status: StatusCode,
        body: serde_json::Value,
    ) {
        self.inner.lock().unwrap().query_routes.push((
            method,
            path.to_owned(),
            query.to_owned(),
            (status, body),
        ));
    }

    /// Serves `diff` for `GET path` when the client asks for a diff.
    pub fn route_diff(&self, path: &str, diff: &str) {
        self.inner
            .lock()
            .unwrap()
            .diffs
            .insert(path.to_owned(), diff.to_owned());
    }

    /// Reports this rate limit on every response from now on.
    pub fn set_rate_limit(&self, remaining: u64, reset_at: u64) {
        self.inner.lock().unwrap().rate_limit = Some((remaining, reset_at));
    }

    /// Reports this limit, as GitHub's separate `search` resource, on search responses.
    pub fn set_search_rate_limit(&self, limit: u64, remaining: u64, reset_at: u64) {
        self.inner.lock().unwrap().search_rate_limit = Some((limit, remaining, reset_at));
    }

    pub fn requests_to(&self, method: Method, path: &str) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == method && r.path == path)
            .collect()
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.inner.lock().unwrap().requests.clone()
    }

    /// Requests that would change GitHub state (anything but GET/HEAD).
    pub fn writes(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.method != Method::GET && r.method != Method::HEAD)
            // GraphQL queries are POSTs that only read.
            .filter(|r| r.path != "/graphql" || r.raw_body.contains("mutation"))
            .collect()
    }
}

async fn handle(State(inner): State<Arc<Mutex<Inner>>>, req: Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().map(str::to_owned);
    let headers: HashMap<String, String> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect();
    let bytes: Bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default();

    let is_search = path.starts_with("/search/");
    let mut inner = inner.lock().unwrap();
    if headers.get("accept").map(String::as_str) == Some("application/vnd.github.diff")
        && let Some(diff) = inner.diffs.get(&path).cloned()
    {
        inner.requests.push(RecordedRequest {
            method,
            path,
            query,
            body: None,
            raw_body: String::new(),
            headers,
            status: 200,
        });
        return (StatusCode::OK, diff).into_response();
    }
    let query_string = query.clone().unwrap_or_default();
    let scripted = inner
        .query_routes
        .iter()
        .find(|(m, p, q, _)| {
            *m == method && *p == path && query_string.split('&').any(|part| part == q)
        })
        .map(|(_, _, _, response)| response.clone())
        .or_else(|| {
            let queue = inner.routes.get_mut(&(method.clone(), path.clone()))?;
            Some(if queue.len() > 1 {
                queue.pop_front().expect("non-empty")
            } else {
                queue[0].clone()
            })
        });
    let (status, body) = scripted.unwrap_or((
        StatusCode::NOT_FOUND,
        serde_json::json!({ "message": "Not Found" }),
    ));

    // Real GitHub sends ETags and answers a matching If-None-Match with an empty 304.
    let etag = format!("\"{:016x}\"", fnv(body.to_string().as_bytes()));
    let not_modified = status == StatusCode::OK && headers.get("if-none-match") == Some(&etag);
    let status = if not_modified {
        StatusCode::NOT_MODIFIED
    } else {
        status
    };

    inner.requests.push(RecordedRequest {
        method,
        path,
        query,
        body: serde_json::from_slice(&bytes).ok(),
        raw_body: String::from_utf8_lossy(&bytes).into_owned(),
        headers,
        status: status.as_u16(),
    });

    let mut response = if not_modified {
        status.into_response()
    } else {
        (status, axum::Json(body)).into_response()
    };
    let response_headers = response.headers_mut();
    response_headers.insert("etag", etag.parse().expect("valid header"));
    match inner.search_rate_limit {
        Some((limit, remaining, reset_at)) if is_search => {
            response_headers.insert("x-ratelimit-resource", "search".parse().expect("valid"));
            response_headers.insert("x-ratelimit-limit", limit.into());
            response_headers.insert("x-ratelimit-remaining", remaining.into());
            response_headers.insert("x-ratelimit-reset", reset_at.into());
        }
        _ => {
            if let Some((remaining, reset_at)) = inner.rate_limit {
                response_headers.insert("x-ratelimit-remaining", remaining.into());
                response_headers.insert("x-ratelimit-reset", reset_at.into());
            }
        }
    }
    response
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// GitHub's GraphQL answer for a pull request without review threads or timeline events.
pub fn empty_pull_request() -> serde_json::Value {
    serde_json::json!({ "data": { "repository": { "pullRequest": {
        "reviewThreads": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
        "timelineItems": { "nodes": [] }
    } } } })
}
