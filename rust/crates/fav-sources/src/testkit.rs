//! Test doubles shared by the source tests: a local HTTP server answering through a
//! closure (httpx's `MockTransport`), and notifiers that record or fail.

use std::sync::{Arc, Mutex};

use axum::response::IntoResponse;
use fav_runtime::BoxFuture;
use fav_runtime::notifications::NewNotification;

use crate::kit::Notifier;

/// One request the fake server received.
#[derive(Debug, Clone)]
pub(crate) struct Received {
    pub(crate) path: String,
    pub(crate) query: Vec<(String, String)>,
    pub(crate) headers: axum::http::HeaderMap,
}

impl Received {
    /// The first value of query parameter `name`.
    pub(crate) fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// What the fake server answers.
#[derive(Debug, Clone)]
pub(crate) struct Answer {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Answer {
    pub(crate) fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.as_bytes().to_vec(),
        }
    }

    pub(crate) fn bytes(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            content_type: "application/octet-stream",
            body: body.to_vec(),
        }
    }

    pub(crate) fn html(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }
}

type Handler = Arc<dyn Fn(&Received) -> Answer + Send + Sync>;

#[derive(Clone)]
struct State {
    handler: Handler,
    received: Arc<Mutex<Vec<Received>>>,
}

/// A server on 127.0.0.1 that answers every request through one closure.
#[derive(Debug)]
pub(crate) struct FakeServer {
    pub(crate) base_url: String,
    received: Arc<Mutex<Vec<Received>>>,
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
            let received = Received {
                path: request.uri().path().to_owned(),
                query,
                headers: request.headers().clone(),
            };
            let answer = (state.handler)(&received);
            state.received.lock().expect("fake server state").push(received);
            let status = axum::http::StatusCode::from_u16(answer.status).expect("status");
            (status, [(axum::http::header::CONTENT_TYPE, answer.content_type)], answer.body).into_response()
        }

        let received: Arc<Mutex<Vec<Received>>> = Arc::default();
        let state = State {
            handler: Arc::new(handler),
            received: Arc::clone(&received),
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
        }
    }

    pub(crate) fn received(&self) -> Vec<Received> {
        self.received.lock().expect("fake server state").clone()
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

/// A notifier that keeps what it was given, or refuses everything.
#[derive(Debug, Default)]
pub(crate) struct RecordingNotifier {
    sent: Mutex<Vec<NewNotification>>,
    fail: bool,
}

impl RecordingNotifier {
    pub(crate) fn failing() -> Self {
        Self {
            sent: Mutex::default(),
            fail: true,
        }
    }

    pub(crate) fn sent(&self) -> Vec<NewNotification> {
        self.sent.lock().expect("notifier state").clone()
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
}
