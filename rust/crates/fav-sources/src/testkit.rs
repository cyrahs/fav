//! Test doubles shared by the source tests: a local HTTP server answering through a
//! closure (httpx's `MockTransport`), and notifiers that record or fail.

use std::sync::{Arc, Mutex};

use axum::response::IntoResponse;
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Resolution};

use crate::kit::Notifier;

/// One request the fake server received.
#[derive(Debug, Clone)]
pub(crate) struct Received {
    pub(crate) method: String,
    pub(crate) path: String,
    /// The query string as sent, without the `?`.
    pub(crate) raw_query: Option<String>,
    pub(crate) query: Vec<(String, String)>,
    pub(crate) headers: axum::http::HeaderMap,
    pub(crate) body: Vec<u8>,
}

impl Received {
    /// The first value of query parameter `name`.
    pub(crate) fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// The path and query string, as a URL ends.
    pub(crate) fn path_and_query(&self) -> String {
        match &self.raw_query {
            Some(query) => format!("{}?{query}", self.path),
            None => self.path.clone(),
        }
    }

    pub(crate) fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body read as a form, like `parse_qsl(body, keep_blank_values=True)`.
    pub(crate) fn form(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(&self.body).into_owned().collect()
    }
}

/// What the fake server answers.
#[derive(Debug, Clone)]
pub(crate) struct Answer {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl Answer {
    /// An extra response header, such as `Set-Cookie` (repeatable).
    pub(crate) fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub(crate) fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.as_bytes().to_vec(),
            headers: Vec::new(),
        }
    }

    pub(crate) fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.as_bytes().to_vec(),
            headers: Vec::new(),
        }
    }

    pub(crate) fn bytes(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            content_type: "application/octet-stream",
            body: body.to_vec(),
            headers: Vec::new(),
        }
    }

    pub(crate) fn html(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
            headers: Vec::new(),
        }
    }
}

type Handler = Arc<dyn Fn(&Received) -> Answer + Send + Sync>;

#[derive(Clone)]
struct State {
    handler: Handler,
    received: Arc<Mutex<Vec<Received>>>,
    panics: Arc<Mutex<Vec<String>>>,
}

/// A server on 127.0.0.1 that answers every request through one closure.
#[derive(Debug)]
pub(crate) struct FakeServer {
    pub(crate) base_url: String,
    received: Arc<Mutex<Vec<Received>>>,
    /// Handler panics (an assertion about a request), raised again when the server is dropped:
    /// the server task would otherwise swallow them.
    panics: Arc<Mutex<Vec<String>>>,
}

impl FakeServer {
    pub(crate) async fn start(handler: impl Fn(&Received) -> Answer + Send + Sync + 'static) -> Self {
        async fn handle(
            axum::extract::State(state): axum::extract::State<State>,
            request: axum::extract::Request,
        ) -> axum::response::Response {
            let query = request
                .uri()
                .query()
                .map(|query| url::form_urlencoded::parse(query.as_bytes()).into_owned().collect())
                .unwrap_or_default();
            let method = request.method().as_str().to_owned();
            let path = request.uri().path().to_owned();
            let raw_query = request.uri().query().map(str::to_owned);
            let headers = request.headers().clone();
            let body = axum::body::to_bytes(request.into_body(), usize::MAX)
                .await
                .map(|bytes| bytes.to_vec())
                .unwrap_or_default();
            let received = Received {
                method,
                path,
                raw_query,
                query,
                headers,
                body,
            };
            state.received.lock().expect("fake server state").push(received.clone());
            let handler = Arc::clone(&state.handler);
            let answer = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&received))) {
                Ok(answer) => answer,
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                        .unwrap_or_default();
                    state.panics.lock().expect("fake server state").push(message);
                    Answer::text(500, "the fake server's handler panicked")
                }
            };
            let status = axum::http::StatusCode::from_u16(answer.status).expect("status");
            let mut response = (status, [(axum::http::header::CONTENT_TYPE, answer.content_type)], answer.body).into_response();
            for (name, value) in &answer.headers {
                let name = axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name");
                let value = axum::http::HeaderValue::from_str(value).expect("header value");
                response.headers_mut().append(name, value);
            }
            response
        }

        let received: Arc<Mutex<Vec<Received>>> = Arc::default();
        let panics: Arc<Mutex<Vec<String>>> = Arc::default();
        let state = State {
            handler: Arc::new(handler),
            received: Arc::clone(&received),
            panics: Arc::clone(&panics),
        };
        let app = axum::Router::new().fallback(handle).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base_url: format!("http://{address}"),
            received,
            panics,
        }
    }

    pub(crate) fn received(&self) -> Vec<Received> {
        self.received.lock().expect("fake server state").clone()
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        let panics = std::mem::take(&mut *self.panics.lock().expect("fake server state"));
        if !std::thread::panicking() {
            assert!(panics.is_empty(), "the fake server's handler panicked: {panics:?}");
        }
    }
}

/// A notifier that keeps what it was given, or refuses enqueues or resolves.
#[derive(Debug, Default)]
pub(crate) struct RecordingNotifier {
    sent: Mutex<Vec<NewNotification>>,
    resolved: Mutex<Vec<(String, Resolution)>>,
    fail: bool,
    fail_resolve: bool,
}

impl RecordingNotifier {
    /// Every enqueue fails.
    pub(crate) fn failing() -> Self {
        Self {
            fail: true,
            ..Self::default()
        }
    }

    /// Every resolve fails.
    pub(crate) fn failing_resolve() -> Self {
        Self {
            fail_resolve: true,
            ..Self::default()
        }
    }

    pub(crate) fn sent(&self) -> Vec<NewNotification> {
        self.sent.lock().expect("notifier state").clone()
    }

    pub(crate) fn resolved(&self) -> Vec<(String, Resolution)> {
        self.resolved.lock().expect("notifier state").clone()
    }
}

impl Notifier for RecordingNotifier {
    fn enqueue(&self, notification: NewNotification) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            if self.fail {
                return Err("notify failed".to_owned());
            }
            self.sent.lock().expect("notifier state").push(notification);
            Ok(())
        })
    }

    fn resolve(&self, dedupe_key: String, resolution: Resolution) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            if self.fail_resolve {
                return Err("resolve failed".to_owned());
            }
            self.resolved.lock().expect("notifier state").push((dedupe_key, resolution));
            Ok(())
        })
    }
}
