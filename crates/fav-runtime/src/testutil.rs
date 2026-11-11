//! Shared helpers for this crate's tests.

use fav_db::testdb::TestDatabase;

/// A temporary database with fav's schema applied.
pub(crate) async fn database() -> TestDatabase {
    let database = TestDatabase::create().await;
    database.db().ensure_schema().await.expect("schema applies");
    database
}

/// Run raw SQL that needs no parameters (fixtures).
pub(crate) async fn execute(database: &TestDatabase, sql: &'static str) {
    sqlx::raw_sql(sql).execute(database.db().pool()).await.expect("fixture SQL runs");
}

/// A golden document read with `serde_json`, for comparing against what was sent over HTTP.
pub(crate) fn golden_json(relative: &str) -> serde_json::Value {
    let path = fav_core::golden::dir().join(relative);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("cannot read golden {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("golden {relative} is not JSON: {error}"))
}

/// One scripted Bot API answer.
#[derive(Debug, Clone)]
pub(crate) struct Answer {
    pub status: u16,
    pub body: AnswerBody,
}

#[derive(Debug, Clone)]
pub(crate) enum AnswerBody {
    Json(serde_json::Value),
    Text(String),
}

impl Answer {
    pub(crate) fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            body: AnswerBody::Json(body),
        }
    }

    pub(crate) fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            body: AnswerBody::Text(body.to_owned()),
        }
    }

    /// The golden form: `{"status": 200, "json": {...}}` or `{"status": 502, "text": "..."}`.
    pub(crate) fn from_golden(value: &serde_json::Value) -> Self {
        let status = u16::try_from(value["status"].as_u64().expect("status")).expect("u16 status");
        match value.get("json") {
            Some(body) => Self::json(status, body.clone()),
            None => Self::text(status, value["text"].as_str().expect("text body")),
        }
    }
}

/// One request the fake Bot API received.
#[derive(Debug, Clone)]
pub(crate) struct Received {
    pub path: String,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Received {
    /// The Bot API method: the last path segment.
    pub(crate) fn method(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or_default()
    }

    pub(crate) fn json(&self) -> serde_json::Value {
        assert!(self.content_type.starts_with("application/json"), "not JSON: {}", self.content_type);
        serde_json::from_slice(&self.body).expect("JSON body")
    }

    /// The golden form of the request, as `notifications.py`'s `_recorded_request` writes it.
    pub(crate) fn recorded(&self) -> serde_json::Value {
        let mut entry = serde_json::Map::new();
        entry.insert("method".to_owned(), self.method().into());
        entry.insert("path".to_owned(), self.path.clone().into());
        if self.content_type.starts_with("application/json") {
            entry.insert("json".to_owned(), self.json());
            return serde_json::Value::Object(entry);
        }
        let (fields, files) = self.multipart();
        entry.insert("form".to_owned(), serde_json::Value::Object(fields));
        entry.insert("files".to_owned(), serde_json::Value::Array(files));
        serde_json::Value::Object(entry)
    }

    /// Form fields and file parts (field, filename, content type, size) of a multipart body.
    pub(crate) fn multipart(&self) -> (serde_json::Map<String, serde_json::Value>, Vec<serde_json::Value>) {
        let boundary = self.content_type.split("boundary=").nth(1).expect("multipart boundary");
        let delimiter = format!("--{boundary}").into_bytes();
        let mut fields = serde_json::Map::new();
        let mut files = Vec::new();
        let parts = split_bytes(&self.body, &delimiter);
        for part in &parts[1..parts.len() - 1] {
            let part = trim_crlf(part);
            let split = find_bytes(part, b"\r\n\r\n").expect("part head");
            let head = String::from_utf8_lossy(&part[..split]).into_owned();
            let body = &part[split + 4..];
            let header = |name: &str| {
                head.split("\r\n")
                    .find(|line| line.to_ascii_lowercase().starts_with(name))
                    .map(|line| line.split_once(':').map_or("", |(_, value)| value.trim()).to_owned())
            };
            let disposition = header("content-disposition").expect("disposition");
            let quoted = |key: &str| {
                disposition
                    .split(&format!("{key}=\""))
                    .nth(1)
                    .map(|rest| rest.split('"').next().unwrap_or_default().to_owned())
            };
            let name = quoted("name").expect("part name");
            if let Some(filename) = quoted("filename") {
                files.push(serde_json::json!({
                    "field": name,
                    "filename": filename,
                    "content_type": header("content-type").unwrap_or_default(),
                    "size": body.len(),
                }));
            } else {
                fields.insert(name, String::from_utf8(body.to_vec()).expect("text field").into());
            }
        }
        (fields, files)
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn split_bytes<'a>(mut haystack: &'a [u8], needle: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    while let Some(index) = find_bytes(haystack, needle) {
        parts.push(&haystack[..index]);
        haystack = &haystack[index + needle.len()..];
    }
    parts.push(haystack);
    parts
}

/// Python's `bytes.strip(b'\r\n')`.
fn trim_crlf(mut bytes: &[u8]) -> &[u8] {
    while let [b'\r' | b'\n', rest @ ..] = bytes {
        bytes = rest;
    }
    while let [rest @ .., b'\r' | b'\n'] = bytes {
        bytes = rest;
    }
    bytes
}

#[derive(Default)]
struct FakeState {
    received: Vec<Received>,
    /// Scripted answers per method; the last one repeats.
    programmes: std::collections::HashMap<String, Vec<Answer>>,
}

/// A fake Telegram Bot API on a loopback port. Unscripted methods succeed: pin and unpin
/// with `result: true`, everything else with `message_id` 99.
#[derive(Clone)]
pub(crate) struct FakeBotApi {
    pub base_url: String,
    state: std::sync::Arc<std::sync::Mutex<FakeState>>,
}

impl FakeBotApi {
    pub(crate) async fn start() -> Self {
        use axum::extract::{Request, State};
        use axum::response::IntoResponse as _;

        type Shared = std::sync::Arc<std::sync::Mutex<FakeState>>;
        async fn handle(State(state): State<Shared>, request: Request) -> axum::response::Response {
            let path = request.uri().path().to_owned();
            let content_type = request
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let body = axum::body::to_bytes(request.into_body(), usize::MAX).await.expect("body").to_vec();
            let received = Received { path, content_type, body };
            let method = received.method().to_owned();
            let answer = {
                let mut state = state.lock().expect("fake state");
                state.received.push(received);
                match state.programmes.get_mut(&method) {
                    Some(answers) if answers.len() > 1 => Some(answers.remove(0)),
                    Some(answers) => answers.first().cloned(),
                    None => None,
                }
            };
            let answer = answer.unwrap_or_else(|| {
                if method == "pinChatMessage" || method == "unpinChatMessage" {
                    Answer::json(200, serde_json::json!({"ok": true, "result": true}))
                } else {
                    Answer::json(200, serde_json::json!({"ok": true, "result": {"message_id": 99}}))
                }
            });
            let status = axum::http::StatusCode::from_u16(answer.status).expect("status");
            match answer.body {
                AnswerBody::Json(body) => (status, axum::Json(body)).into_response(),
                AnswerBody::Text(body) => (status, body).into_response(),
            }
        }

        let state: Shared = std::sync::Arc::default();
        let app = axum::Router::new().fallback(handle).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            base_url: format!("http://{address}"),
            state,
        }
    }

    /// Answer `method` with `answers` in order, the last one repeating.
    pub(crate) fn script(&self, method: &str, answers: Vec<Answer>) {
        self.state.lock().expect("fake state").programmes.insert(method.to_owned(), answers);
    }

    pub(crate) fn received(&self) -> Vec<Received> {
        self.state.lock().expect("fake state").received.clone()
    }

    pub(crate) fn methods(&self) -> Vec<String> {
        self.received().iter().map(|request| request.method().to_owned()).collect()
    }

    /// A client for this server that tells images apart with Python's built-in table only.
    pub(crate) fn client(&self) -> crate::telegram_bot::BotClient {
        crate::telegram_bot::BotClient::with_base_url(&self.base_url)
            .expect("client")
            .with_mime_types(fav_core::mimetypes::MimeTypes::defaults())
    }
}
