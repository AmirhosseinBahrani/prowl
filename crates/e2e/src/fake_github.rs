use std::collections::HashMap;
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
    pub body: Option<serde_json::Value>,
}

#[derive(Default)]
struct Inner {
    routes: HashMap<(Method, String), (StatusCode, serde_json::Value)>,
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
        let inner = Arc::new(Mutex::new(Inner::default()));
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
        self.inner
            .lock()
            .unwrap()
            .routes
            .insert((method, path.to_owned()), (status, body));
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.inner.lock().unwrap().requests.clone()
    }

    /// Requests that would change GitHub state (anything but GET/HEAD).
    pub fn writes(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.method != Method::GET && r.method != Method::HEAD)
            .collect()
    }
}

async fn handle(State(inner): State<Arc<Mutex<Inner>>>, req: Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().map(str::to_owned);
    let bytes: Bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let body = serde_json::from_slice(&bytes).ok();

    let mut inner = inner.lock().unwrap();
    inner.requests.push(RecordedRequest {
        method: method.clone(),
        path: path.clone(),
        query,
        body,
    });
    match inner.routes.get(&(method, path)) {
        Some((status, body)) => (*status, axum::Json(body.clone())).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "message": "Not Found" })),
        )
            .into_response(),
    }
}
