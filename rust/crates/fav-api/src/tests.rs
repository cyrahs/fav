//! The API against a real PostgreSQL (`FAV_TEST_DATABASE_URL`), checked against Python.
//!
//! `rust/goldens/api/requests.json` replays requests through the real FastAPI app; each
//! scenario's stored state is seeded into a fresh database here and every exchange must
//! come back the same: status, the headers Python's replay recorded, and the body. The
//! tests after the replay port `tests/test_api_server.py` and
//! `tests/test_credentials_api.py`, and cover what only the Rust side can (the front end
//! files, the archive tables).

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use chrono::{DateTime, TimeZone as _, Utc};
use fav_core::golden;
use fav_core::pyjson::{self, DumpOptions, Value};
use fav_db::SettingsStore;
use fav_db::testdb::TestDatabase;
use fav_runtime::site_sessions::{self, StoredCookie};
use fav_runtime::telegram_bot::BotClient;
use percent_encoding::percent_decode_str;

use crate::router::{self, Request};
use crate::{ApiState, Clock, cors, credentials, openapi};

const TOKEN: &str = "token-for-tests";
const CLIENT_TOKEN: &str = "rsshub-token";

fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 2, 0, 0, 0).single().expect("a valid time")
}

fn fixed_clock() -> Clock {
    Arc::new(fixed_now)
}

fn digest(token: &str) -> String {
    credentials::sha256_hex(token)
}

/// One answer, as a test sees it.
#[derive(Debug)]
struct Answer {
    status: u16,
    headers: HeaderMap,
    body: Bytes,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    fn json(&self) -> Value {
        pyjson::loads(std::str::from_utf8(&self.body).expect("UTF-8")).unwrap_or_else(|error| {
            panic!("not JSON ({error:?}): {:?}", String::from_utf8_lossy(&self.body));
        })
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn error_code(&self) -> String {
        golden::string(golden::get(golden::get(&self.json(), "error"), "code"))
    }
}

/// A fresh database and the API state over it.
struct Harness {
    database: TestDatabase,
    state: ApiState,
}

impl Harness {
    async fn new() -> Self {
        let database = TestDatabase::create().await;
        database.db().ensure_schema().await.expect("the schema");
        let db = database.db().clone();
        let state = ApiState {
            token: TOKEN.to_owned(),
            settings: Arc::new(SettingsStore::new(db.clone())),
            db,
            // Nothing here reaches it unless a test points it at its own fake.
            bot: BotClient::with_base_url("http://127.0.0.1:9").expect("a client"),
            credential_clients: Vec::new(),
            cors: None,
            web_dist: None,
            clock: fixed_clock(),
        };
        Self { database, state }
    }

    async fn finish(self) {
        self.database.drop_database().await;
    }

    async fn execute(&self, sql: &str, binds: &[&str]) {
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()));
        for bind in binds {
            query = query.bind(*bind);
        }
        query
            .execute(self.state.db.pool())
            .await
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
    }

    async fn store_settings(&self, section: &str, value: &Value) {
        let text = pyjson::dumps(value, &DumpOptions::python_default()).expect("dumpable");
        self.execute(
            "INSERT INTO app_settings (section, value) VALUES ($1, $2::jsonb) ON CONFLICT (section) DO UPDATE SET value = EXCLUDED.value;",
            &[section, &text],
        )
        .await;
        self.state.settings.invalidate_cache();
    }

    async fn store_control_request(&self, row: &Value) {
        let text = |key: &str| row.get(key).map(golden::string);
        let id = golden::int(golden::get(row, "id")).to_string();
        let sql = "INSERT INTO control_requests (id, kind, target, status, requested_at, started_at, finished_at, result, error)
                   VALUES ($1::bigint, $2, $3, $4, $5::timestamptz, $6::timestamptz, $7::timestamptz, $8, $9);";
        let mut query = sqlx::query(sql)
            .bind(id)
            .bind(text("kind").unwrap_or_else(|| "trigger_job".to_owned()))
            .bind(text("target"))
            .bind(text("status"))
            .bind(text("requested_at"))
            .bind(text("started_at"))
            .bind(text("finished_at"));
        query = query
            .bind(text("result").unwrap_or_default())
            .bind(text("error").unwrap_or_default());
        query.execute(self.state.db.pool()).await.expect("a control request");
        self.execute(
            "SELECT setval(pg_get_serial_sequence('control_requests', 'id'), (SELECT MAX(id) FROM control_requests));",
            &[],
        )
        .await;
    }

    async fn store_site_session(&self, row: &Value) {
        let cookies: Vec<StoredCookie> = row
            .get("cookies")
            .map(golden::list)
            .unwrap_or_default()
            .iter()
            .map(|cookie| StoredCookie {
                name: golden::string(golden::get(cookie, "name")),
                value: golden::string(golden::get(cookie, "value")),
                domain: golden::string(golden::get(cookie, "domain")),
                path: golden::string(golden::get(cookie, "path")),
                expires: match golden::get(cookie, "expires") {
                    Value::Float(value) => Some(*value),
                    #[allow(clippy::cast_precision_loss)] // Unix seconds, far below 2^53.
                    Value::Int(value) => Some(*value as f64),
                    _ => None,
                },
            })
            .collect();
        let extra = row.get("extra").map_or_else(
            || "{}".to_owned(),
            |extra| pyjson::dumps(extra, &DumpOptions::python_default()).expect("dumpable"),
        );
        let text = |key: &str| row.get(key).map(golden::string);
        sqlx::query(
            "INSERT INTO site_sessions (site, cookies, account, status, message, expires_at, signed_in_at, checked_at, extra)
             VALUES ($1, $2, $3, $4, $5, $6::timestamptz, $7::timestamptz, $8::timestamptz, $9);",
        )
        .bind(text("site"))
        .bind(site_sessions::encode_cookies(&cookies))
        .bind(text("account").unwrap_or_default())
        .bind(text("status").unwrap_or_default())
        .bind(text("message").unwrap_or_default())
        .bind(text("expires_at"))
        .bind(text("signed_in_at"))
        .bind(text("checked_at"))
        .bind(extra)
        .execute(self.state.db.pool())
        .await
        .expect("a site session");
    }

    /// Seed a golden scenario's `setup`.
    async fn seed(&mut self, setup: &Value) {
        if let Some(Value::Dict(sections)) = setup.get("settings") {
            for (section, value) in sections.iter() {
                self.store_settings(&fav_runtime::pystr::str_key(section), value).await;
            }
        }
        for row in setup.get("control_requests").map(golden::list).unwrap_or_default() {
            self.store_control_request(row).await;
        }
        for row in setup.get("site_sessions").map(golden::list).unwrap_or_default() {
            self.store_site_session(row).await;
        }
        if let Some(raw) = setup.get("credential_clients") {
            self.state.credential_clients = credentials::parse_clients(&golden::string(raw)).expect("valid clients");
        }
        if let Some(origins) = setup.get("cors_origins") {
            let origins = golden::list(origins).iter().map(golden::string).collect();
            let credentials = setup.get("cors_allow_credentials").is_some_and(Value::is_truthy);
            self.state.cors = cors::Cors::new(origins, credentials);
        }
    }

    async fn request(&self, method: &str, target: &str, headers: &[(&str, &str)], body: Option<&str>) -> Answer {
        let (path, query_string) = target.split_once('?').unwrap_or((target, ""));
        let mut header_map = HeaderMap::new();
        header_map.insert("host", HeaderValue::from_static("testserver"));
        for (name, value) in headers {
            header_map.append(
                HeaderName::from_bytes(name.to_lowercase().as_bytes()).expect("a header name"),
                HeaderValue::from_bytes(value.as_bytes()).expect("a header value"),
            );
        }
        let request = Request {
            method: method.to_owned(),
            path: percent_decode_str(path).decode_utf8_lossy().into_owned(),
            query: form_urlencoded::parse(query_string.as_bytes()).into_owned().collect(),
            query_string: query_string.to_owned(),
            headers: header_map,
            body: Bytes::from(body.unwrap_or_default().to_owned()),
        };
        let response = router::handle(&self.state, request).await;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("a body");
        Answer { status, headers, body }
    }

    async fn get(&self, target: &str) -> Answer {
        self.request("GET", target, &[("Authorization", &format!("Bearer {TOKEN}"))], None)
            .await
    }

    async fn send_json(&self, method: &str, target: &str, body: &str) -> Answer {
        let authorization = format!("Bearer {TOKEN}");
        self.request(
            method,
            target,
            &[("Authorization", &authorization), ("content-type", "application/json")],
            Some(body),
        )
        .await
    }
}

/// The headers Python's replay records (`_RESPONSE_HEADERS` and every
/// `access-control-*`).
const RECORDED_HEADERS: [&str; 6] = ["content-type", "www-authenticate", "allow", "cache-control", "location", "vary"];

fn without(value: &Value, keys: &[String]) -> Value {
    match value {
        Value::Dict(dict) => {
            let mut dict = dict.clone();
            for key in keys {
                dict.remove(key);
            }
            Value::Dict(dict)
        }
        other => other.clone(),
    }
}

#[tokio::test]
async fn requests_replay_python() {
    let document = golden::load("api/requests.json");
    for case in golden::cases(&document) {
        let name = golden::string(golden::get(case, "name"));
        let mut harness = Harness::new().await;
        harness.seed(golden::get(case, "setup")).await;
        for exchange in golden::list(golden::get(case, "exchanges")) {
            let request = golden::get(exchange, "request");
            let expected = golden::get(exchange, "response");
            let headers: Vec<(String, String)> = golden::get(request, "headers")
                .as_dict()
                .expect("headers")
                .iter()
                .map(|(key, value)| (fav_runtime::pystr::str_key(key), golden::string(value)))
                .collect();
            let header_refs: Vec<(&str, &str)> = headers.iter().map(|(key, value)| (key.as_str(), value.as_str())).collect();
            let method = golden::string(golden::get(request, "method"));
            let path = golden::string(golden::get(request, "path"));
            let body = request.get("body").map(golden::string);
            let answer = harness.request(&method, &path, &header_refs, body.as_deref()).await;
            let context = format!("[{name}] {method} {path} {headers:?} {body:?}");

            assert_eq!(
                i128::from(answer.status),
                golden::int(golden::get(expected, "status")),
                "{context}: {}",
                answer.text()
            );
            let expected_headers = golden::get(expected, "headers").as_dict().expect("headers");
            let mut names: Vec<String> = RECORDED_HEADERS.iter().map(|name| (*name).to_owned()).collect();
            names.extend(
                answer
                    .headers
                    .keys()
                    .map(ToString::to_string)
                    .filter(|name| name.starts_with("access-control-")),
            );
            names.extend(expected_headers.iter().map(|(key, _)| fav_runtime::pystr::str_key(key)));
            names.sort();
            names.dedup();
            for header in names {
                assert_eq!(
                    answer.header(&header),
                    expected_headers.get(&header).and_then(Value::as_str),
                    "{context}: header {header}"
                );
            }
            match expected.get("json") {
                Some(json) => {
                    let volatile: Vec<String> = expected
                        .get("volatile")
                        .map(golden::list)
                        .unwrap_or_default()
                        .iter()
                        .map(golden::string)
                        .collect();
                    assert_eq!(without(&answer.json(), &volatile), without(json, &volatile), "{context}");
                    if !volatile.is_empty() {
                        // Python's fake store stamps every request with its fixed clock; pin
                        // the row PostgreSQL stamped so later reads compare exactly.
                        let id = golden::int(golden::get(&answer.json(), "id")).to_string();
                        let requested_at = golden::string(golden::get(json, "requested_at"));
                        harness
                            .execute(
                                "UPDATE control_requests SET requested_at = $2::timestamptz WHERE id = $1::bigint;",
                                &[&id, &requested_at],
                            )
                            .await;
                    }
                }
                None => assert_eq!(answer.text(), golden::string(golden::get(expected, "text")), "{context}"),
            }
        }
        harness.finish().await;
    }
}

// ---------- documents ----------

#[tokio::test]
async fn openapi_matches_python_for_every_ported_path() {
    let document = golden::load("api/openapi.json");
    let python: serde_json::Value =
        serde_json::from_str(&pyjson::dumps(&golden::cases(&document)[0], &DumpOptions::compact()).expect("dumpable")).expect("JSON");
    let rust = openapi::document_json();

    assert_eq!(rust["openapi"], python["openapi"]);
    assert_eq!(rust["info"], python["info"]);
    let python_paths = python["paths"].as_object().expect("paths");
    let rust_paths = rust["paths"].as_object().expect("paths");
    for (path, item) in python_paths {
        match rust_paths.get(path) {
            Some(ours) => assert_eq!(ours, item, "{path}"),
            None => assert!(
                openapi::PENDING.iter().any(|(pending, _)| pending == path),
                "{path} is neither served nor pending"
            ),
        }
    }
    for path in rust_paths.keys() {
        assert!(python_paths.contains_key(path), "{path} is not in Python's document");
        assert!(
            !openapi::PENDING.iter().any(|(pending, _)| pending == path),
            "{path} is served and pending"
        );
    }
    // Every schema the served paths reach is Python's, and nothing else is listed.
    let mut referenced = std::collections::BTreeSet::new();
    openapi::referenced_schemas(&rust["paths"], &mut referenced);
    let rust_schemas = rust["components"]["schemas"].as_object().expect("schemas");
    loop {
        let before = referenced.len();
        for name in referenced.clone() {
            openapi::referenced_schemas(&rust_schemas[&name], &mut referenced);
        }
        if referenced.len() == before {
            break;
        }
    }
    let names: Vec<&String> = rust_schemas.keys().collect();
    assert_eq!(names, referenced.iter().collect::<Vec<_>>());
    for (name, schema) in rust_schemas {
        assert_eq!(schema, &python["components"]["schemas"][name], "{name}");
    }
    let harness = Harness::new().await;
    let answer = harness.request("GET", "/openapi.json", &[], None).await;
    assert_eq!(answer.status, 200);
    let served: serde_json::Value = serde_json::from_slice(&answer.body).expect("JSON");
    assert_eq!(served, rust);

    // `==` on JSON objects ignores key order; Swagger UI shows paths, operations,
    // properties and responses in document order, so those orders must be Python's.
    let mut python_orders = Vec::new();
    name_orders(&python, "", false, &mut python_orders);
    let python_orders: std::collections::BTreeMap<String, Vec<String>> = python_orders.into_iter().collect();
    let mut served_orders = Vec::new();
    name_orders(&served, "", false, &mut served_orders);
    for (pointer, keys) in served_orders {
        let expected: Vec<String> = python_orders[&pointer].iter().filter(|key| keys.contains(key)).cloned().collect();
        assert_eq!(keys, expected, "key order at {pointer}");
    }
    harness.finish().await;
}

/// The key order of every object whose keys are names rather than JSON Schema keywords
/// (paths, a path's operations, properties, responses, media types, schemas).
fn name_orders(value: &serde_json::Value, pointer: &str, names: bool, out: &mut Vec<(String, Vec<String>)>) {
    match value {
        serde_json::Value::Object(map) => {
            if names {
                out.push((pointer.to_owned(), map.keys().cloned().collect()));
            }
            for (key, item) in map {
                // Under a name map the keys are names, so only a path's operations are
                // another name map; elsewhere these keywords hold one.
                let child_names = if names {
                    pointer == "/paths"
                } else {
                    matches!(key.as_str(), "paths" | "properties" | "responses" | "content" | "schemas")
                };
                name_orders(item, &format!("{pointer}/{key}"), child_names, out);
            }
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                name_orders(item, &format!("{pointer}/{index}"), false, out);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn documentation_pages_are_fastapis() {
    let harness = Harness::new().await;
    let document = golden::load("api/docs.json");
    for case in golden::cases(&document) {
        let path = golden::string(golden::get(case, "path"));
        let answer = harness.request("GET", &path, &[], None).await;
        assert_eq!(i128::from(answer.status), golden::int(golden::get(case, "status")), "{path}");
        assert_eq!(
            answer.header("content-type"),
            Some(golden::string(golden::get(case, "content_type")).as_str())
        );
        assert_eq!(answer.text(), golden::string(golden::get(case, "body")), "{path}");
        assert_eq!(harness.request("HEAD", &path, &[], None).await.status, 200, "{path}");
    }
    harness.finish().await;
}

// ---------- tests/test_api_server.py ----------

// py: tests/test_api_server.py::test_health_endpoint_returns_ok_without_authorization
#[tokio::test]
async fn health_endpoint_returns_ok_without_authorization() {
    let harness = Harness::new().await;
    let answer = harness.request("GET", "/healthz", &[], None).await;
    assert_eq!(answer.status, 200);
    assert_eq!(
        answer.json(),
        pyjson::loads(r#"{"status": "ok", "generated_at": "2026-03-02T00:00:00Z"}"#).unwrap()
    );
    harness.finish().await;
}

/// A fake Bot API answering every call with one status and body.
async fn fake_bot_api(status: u16, body: &'static str) -> String {
    let app = axum::Router::new().fallback(move || async move {
        let mut response = axum::response::Response::new(axum::body::Body::from(body));
        *response.status_mut() = axum::http::StatusCode::from_u16(status).expect("a status");
        response
            .headers_mut()
            .insert("content-type", HeaderValue::from_static("application/json"));
        response
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("an address");
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{address}")
}

async fn configured_bot(harness: &mut Harness, status: u16, body: &'static str) {
    let telegram = pyjson::loads(r#"{"enabled": true, "bot_token": "123:abc", "chat_id": "-100"}"#).unwrap();
    harness.store_settings("notifications.telegram", &telegram).await;
    harness.state.bot = BotClient::with_base_url(&fake_bot_api(status, body).await).expect("a client");
}

// py: tests/test_api_server.py::test_telegram_notification_test_endpoint_returns_delivery_result
#[tokio::test]
async fn telegram_notification_test_endpoint_returns_delivery_result() {
    let mut harness = Harness::new().await;
    configured_bot(&mut harness, 200, r#"{"ok": true, "result": {"message_id": 77}}"#).await;
    let answer = harness.send_json("POST", "/api/v2/notifications/telegram/test", "{}").await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    // The real sender reports no warnings for a plain message; Python's test injects one.
    assert_eq!(
        answer.json(),
        pyjson::loads(r#"{"status": "delivered", "message_id": 77, "warnings": []}"#).unwrap()
    );
    harness.finish().await;
}

// py: tests/test_api_server.py::test_telegram_notification_test_endpoint_returns_sanitized_delivery_error
#[tokio::test]
async fn telegram_notification_test_endpoint_returns_sanitized_delivery_error() {
    let mut harness = Harness::new().await;
    configured_bot(
        &mut harness,
        401,
        r#"{"ok": false, "error_code": 401, "description": "Unauthorized"}"#,
    )
    .await;
    let answer = harness.send_json("POST", "/api/v2/notifications/telegram/test", "{}").await;
    assert_eq!(answer.status, 502);
    assert_eq!(
        *golden::get(&answer.json(), "error"),
        pyjson::loads(
            r#"{"code": "telegram_delivery_failed", "message": "Telegram Bot API responded with error 401: Unauthorized", "details": null}"#
        )
        .unwrap()
    );
    // The bot token never reaches the answer.
    assert!(!answer.text().contains("123:abc"));
    harness.finish().await;
}

// py: tests/test_api_server.py::test_configured_cors_allows_frontend_origin_preflight
#[tokio::test]
async fn configured_cors_allows_frontend_origin_preflight() {
    let mut harness = Harness::new().await;
    harness.state.cors = cors::Cors::new(vec!["https://game-view.s117.me".to_owned()], true);
    let answer = harness
        .request(
            "OPTIONS",
            "/api/v2/jobs",
            &[
                ("Origin", "https://game-view.s117.me"),
                ("Access-Control-Request-Method", "PUT"),
                ("Access-Control-Request-Headers", "Authorization"),
            ],
            None,
        )
        .await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.header("access-control-allow-origin"), Some("https://game-view.s117.me"));
    assert_eq!(answer.header("access-control-allow-credentials"), Some("true"));
    assert!(
        answer
            .header("access-control-allow-headers")
            .unwrap_or_default()
            .contains("Authorization")
    );
    harness.finish().await;
}

// py: tests/test_api_server.py::test_jobs_endpoint_requires_authorization_header
#[tokio::test]
async fn jobs_endpoint_requires_authorization_header() {
    let harness = Harness::new().await;
    let answer = harness.request("GET", "/api/v2/jobs", &[], None).await;
    assert_eq!(answer.status, 401);
    assert_eq!(answer.header("www-authenticate"), Some(r#"Bearer realm="fav-api""#));
    assert_eq!(
        answer.json(),
        pyjson::loads(r#"{"error": {"code": "missing_authorization", "message": "Authorization header is required.", "details": null}}"#)
            .unwrap()
    );
    harness.finish().await;
}

// py: tests/test_api_server.py::test_jobs_endpoint_rejects_invalid_token
#[tokio::test]
async fn jobs_endpoint_rejects_invalid_token() {
    let harness = Harness::new().await;
    let answer = harness
        .request("GET", "/api/v2/jobs", &[("Authorization", "Bearer wrong-token")], None)
        .await;
    assert_eq!(answer.status, 403);
    assert_eq!(
        answer.json(),
        pyjson::loads(r#"{"error": {"code": "invalid_token", "message": "Bearer token is invalid.", "details": null}}"#).unwrap()
    );
    harness.finish().await;
}

// py: tests/test_api_server.py::test_jobs_endpoint_returns_registered_jobs
#[tokio::test]
async fn jobs_endpoint_returns_registered_jobs() {
    let harness = Harness::new().await;
    let bilibili = pyjson::loads(
        r#"{"enabled": true, "cron": "*/30 * * * *", "accounts": [{"name": "main", "favorites": [{"fav_id": 1, "path": "collection/bilibili/a"}]}]}"#,
    )
    .unwrap();
    harness.store_settings("web.bilibili", &bilibili).await;
    harness
        .store_settings("web.telegram", &pyjson::loads(r#"{"enabled": false, "notify": false}"#).unwrap())
        .await;
    let answer = harness.get("/api/v2/jobs").await;
    assert_eq!(answer.status, 200);
    let body = answer.json();
    let items = golden::list(golden::get(&body, "items"));
    // Every job is listed, whether its source is ported yet or not (DIVERGENCES.md D17).
    assert_eq!(items.len(), 13);
    assert_eq!(golden::int(golden::get(&body, "total")), 13);
    assert_eq!(
        items[0],
        pyjson::loads(
            r#"{"key": "bilibili", "name": "Bilibili", "enabled": true, "notify": true, "cron": "*/30 * * * *", "section": "web.bilibili", "missing_fields": []}"#
        )
        .unwrap()
    );
    let telegram = items
        .iter()
        .find(|item| golden::string(golden::get(item, "key")) == "telegram")
        .expect("telegram");
    assert_eq!(golden::get(telegram, "name"), &Value::from("Telegram"));
    assert_eq!(golden::get(telegram, "enabled"), &Value::Bool(false));
    assert_eq!(golden::get(telegram, "notify"), &Value::Bool(false));
    assert_eq!(golden::get(telegram, "cron"), &Value::from("*/30 * * * *"));
    assert_eq!(golden::get(telegram, "section"), &Value::from("web.telegram"));
    harness.finish().await;
}

fn assert_new_request(answer: &Answer, target: &str) {
    assert_eq!(answer.status, 202, "{}", answer.text());
    let body = answer.json();
    assert_eq!(
        without(&body, &["requested_at".to_owned()]),
        pyjson::loads(&format!(
            r#"{{"id": 1, "target": "{target}", "kind": "trigger_job", "status": "pending", "started_at": null, "finished_at": null, "result": "", "error": ""}}"#
        ))
        .unwrap()
    );
    let requested_at = golden::string(golden::get(&body, "requested_at"));
    assert!(requested_at.ends_with('Z') && requested_at.len() == 20, "{requested_at}");
}

// py: tests/test_api_server.py::test_create_job_request_accepts_disabled_target
#[tokio::test]
async fn create_job_request_accepts_disabled_target() {
    let harness = Harness::new().await;
    let answer = harness.send_json("POST", "/api/v2/job-requests", r#"{"target": "telegram"}"#).await;
    assert_new_request(&answer, "telegram");
    harness.finish().await;
}

// py: tests/test_api_server.py::test_create_job_request_accepts_azurlane_target
#[tokio::test]
async fn create_job_request_accepts_azurlane_target() {
    let harness = Harness::new().await;
    let answer = harness.send_json("POST", "/api/v2/job-requests", r#"{"target": "azurlane"}"#).await;
    assert_new_request(&answer, "azurlane");
    harness.finish().await;
}

// py: tests/test_api_server.py::test_create_job_request_rejects_invalid_target_as_validation_error
#[tokio::test]
async fn create_job_request_rejects_invalid_target_as_validation_error() {
    let harness = Harness::new().await;
    let answer = harness
        .send_json("POST", "/api/v2/job-requests", r#"{"target": "not-a-source"}"#)
        .await;
    assert_eq!(answer.status, 422);
    assert_eq!(answer.error_code(), "validation_error");
    harness.finish().await;
}

async fn seed_requests(harness: &Harness, rows: &[(i64, &str, &str)]) {
    for (id, status, kind) in rows {
        let row = pyjson::loads(&format!(
            r#"{{"id": {id}, "kind": "{kind}", "target": "bilibili", "status": "{status}", "requested_at": "2026-03-01T00:00:{:02}+00:00"}}"#,
            id % 60
        ))
        .unwrap();
        harness.store_control_request(&row).await;
    }
}

fn statuses(answer: &Answer) -> Vec<String> {
    golden::list(golden::get(&answer.json(), "items"))
        .iter()
        .map(|item| golden::string(golden::get(item, "status")))
        .collect()
}

// py: tests/test_api_server.py::test_list_job_requests_accepts_repeated_status_params
#[tokio::test]
async fn list_job_requests_accepts_repeated_status_params() {
    let harness = Harness::new().await;
    seed_requests(
        &harness,
        &[
            (1, "failed", "trigger_job"),
            (2, "rejected", "trigger_job"),
            (3, "pending", "trigger_job"),
        ],
    )
    .await;
    let answer = harness.get("/api/v2/job-requests?status=failed&status=rejected&limit=10").await;
    assert_eq!(answer.status, 200);
    assert_eq!(statuses(&answer), ["rejected", "failed"]);
    let answer = harness.get("/api/v2/job-requests?status=failed&limit=10").await;
    assert_eq!(statuses(&answer), ["failed"]);
    harness.finish().await;
}

// py: tests/test_api_server.py::test_list_job_requests_without_status_passes_none
#[tokio::test]
async fn list_job_requests_without_status_passes_none() {
    let harness = Harness::new().await;
    let rows: Vec<(i64, &str, &str)> = (1..=55)
        .map(|id| {
            (
                id,
                ["pending", "failed", "succeeded"][usize::try_from(id % 3).unwrap()],
                "trigger_job",
            )
        })
        .collect();
    seed_requests(&harness, &rows).await;
    let answer = harness.get("/api/v2/job-requests").await;
    assert_eq!(answer.status, 200);
    // No status filter, and the default limit of 50.
    let found = statuses(&answer);
    assert_eq!(found.len(), 50);
    for status in ["pending", "failed", "succeeded"] {
        assert!(found.iter().any(|found| found == status), "{status}");
    }
    harness.finish().await;
}

// py: tests/test_api_server.py::test_list_job_requests_rejects_unknown_status
#[tokio::test]
async fn list_job_requests_rejects_unknown_status() {
    let harness = Harness::new().await;
    let answer = harness.get("/api/v2/job-requests?status=bogus").await;
    assert_eq!(answer.status, 422);
    assert_eq!(answer.error_code(), "validation_error");
    harness.finish().await;
}

// py: tests/test_api_server.py::test_list_job_requests_serializes_scheduled_kind
#[tokio::test]
async fn list_job_requests_serializes_scheduled_kind() {
    let harness = Harness::new().await;
    seed_requests(&harness, &[(1, "succeeded", "scheduled_job")]).await;
    let answer = harness.get("/api/v2/job-requests").await;
    assert_eq!(answer.status, 200);
    let body = answer.json();
    assert_eq!(
        golden::get(&golden::list(golden::get(&body, "items"))[0], "kind"),
        &Value::from("scheduled_job")
    );
    harness.finish().await;
}

// py: tests/test_api_server.py::test_get_job_request_returns_status_payload
#[tokio::test]
async fn get_job_request_returns_status_payload() {
    let harness = Harness::new().await;
    for (id, status) in [
        (95, "pending"),
        (96, "running"),
        (97, "succeeded"),
        (98, "failed"),
        (99, "rejected"),
    ] {
        seed_requests(&harness, &[(id, status, "trigger_job")]).await;
        let answer = harness.get(&format!("/api/v2/job-requests/{id}")).await;
        assert_eq!(answer.status, 200);
        let body = answer.json();
        assert_eq!(golden::int(golden::get(&body, "id")), i128::from(id));
        assert_eq!(golden::get(&body, "status"), &Value::from(status));
    }
    harness.finish().await;
}

// py: tests/test_api_server.py::test_notifications_endpoints_are_removed
#[tokio::test]
async fn notifications_endpoints_are_removed() {
    let harness = Harness::new().await;
    assert_eq!(harness.get("/api/v2/notifications").await.status, 404);
    assert_eq!(
        harness
            .send_json("POST", "/api/v2/notifications/ack", r#"{"ids": [1]}"#)
            .await
            .status,
        404
    );
    harness.finish().await;
}

async fn stored_section(harness: &Harness, section: &str) -> Option<Value> {
    let text: Option<String> = sqlx::query_scalar("SELECT value::text FROM app_settings WHERE section = $1;")
        .bind(section)
        .fetch_optional(harness.state.db.pool())
        .await
        .expect("a query");
    text.map(|text| pyjson::loads(&text).expect("JSON"))
}

// py: tests/test_api_server.py::test_update_settings_section_rejects_enabling_an_incomplete_source
#[tokio::test]
async fn update_settings_section_rejects_enabling_an_incomplete_source() {
    let harness = Harness::new().await;
    // Bilibili needs at least one account before it can run.
    let answer = harness
        .send_json("PUT", "/api/v2/settings/web.bilibili", r#"{"cron": "0 * * * *", "enabled": true}"#)
        .await;
    assert_eq!(answer.status, 422);
    assert_eq!(
        *golden::get(&answer.json(), "error"),
        pyjson::loads(
            r#"{"code": "incomplete_settings", "message": "Cannot enable a source while its configuration is incomplete.", "details": {"missing_fields": ["accounts"]}}"#
        )
        .unwrap()
    );
    // The rejected payload must not have been persisted.
    assert_eq!(stored_section(&harness, "web.bilibili").await, None);
    harness.finish().await;
}

// py: tests/test_api_server.py::test_update_settings_section_allows_an_incomplete_source_while_disabled
#[tokio::test]
async fn update_settings_section_allows_an_incomplete_source_while_disabled() {
    let harness = Harness::new().await;
    let answer = harness
        .send_json("PUT", "/api/v2/settings/web.bilibili", r#"{"cron": "0 * * * *", "enabled": false}"#)
        .await;
    assert_eq!(answer.status, 200);
    assert_eq!(
        golden::get(&answer.json(), "missing_fields"),
        &pyjson::loads(r#"["accounts"]"#).unwrap()
    );
    assert!(stored_section(&harness, "web.bilibili").await.is_some());
    harness.finish().await;
}

// py: tests/test_api_server.py::test_update_settings_section_accepts_enabling_a_complete_source
#[tokio::test]
async fn update_settings_section_accepts_enabling_a_complete_source() {
    let harness = Harness::new().await;
    let answer = harness
        .send_json(
            "PUT",
            "/api/v2/settings/web.bilibili",
            r#"{"cron": "0 * * * *", "enabled": true, "accounts": [{"name": "main", "favorites": [{"fav_id": 99, "path": "collection/bilibili/fav"}]}]}"#,
        )
        .await;
    assert_eq!(answer.status, 200);
    assert_eq!(golden::get(&answer.json(), "missing_fields"), &Value::List(Vec::new()));
    let stored = stored_section(&harness, "web.bilibili").await.expect("saved");
    assert_eq!(golden::get(&stored, "enabled"), &Value::Bool(true));
    harness.finish().await;
}

// ---------- tests/test_credentials_api.py ----------

async fn credentials_harness() -> Harness {
    let mut harness = Harness::new().await;
    let sessions = pyjson::loads(
        r#"[
            {"site": "bilibili-main", "cookies": [{"name": "SESSDATA", "value": "s", "domain": ".bilibili.com", "path": "/", "expires": 2000000000.0}],
             "account": "某人 (42)", "status": "valid", "signed_in_at": "2026-11-03T00:00:00+00:00", "extra": {"refresh_token": "never-served"}},
            {"site": "rednote", "cookies": [{"name": "web_session", "value": "in-profile", "domain": ".xiaohongshu.com", "path": "/", "expires": null}],
             "account": "某人 (42)", "status": "valid"}
        ]"#,
    )
    .unwrap();
    for row in golden::list(&sessions) {
        harness.store_site_session(row).await;
    }
    harness.state.credential_clients =
        credentials::parse_clients(&format!("rsshub:{}:bilibili-main,rednote,pixiv", digest(CLIENT_TOKEN))).unwrap();
    harness
}

fn client_headers() -> Vec<(&'static str, String)> {
    vec![("Authorization", format!("Bearer {CLIENT_TOKEN}"))]
}

async fn client_get(harness: &Harness, target: &str) -> Answer {
    let headers = client_headers();
    let refs: Vec<(&str, &str)> = headers.iter().map(|(name, value)| (*name, value.as_str())).collect();
    harness.request("GET", target, &refs, None).await
}

// py: tests/test_credentials_api.py::test_a_client_reads_the_cookies_of_its_site
#[tokio::test]
async fn a_client_reads_the_cookies_of_its_site() {
    let harness = credentials_harness().await;
    let answer = client_get(&harness, "/api/v2/credentials/bilibili-main").await;
    assert_eq!(answer.status, 200);
    assert_eq!(answer.header("cache-control"), Some("no-store"));
    let body = answer.json();
    assert_eq!(golden::get(&body, "account"), &Value::from("某人 (42)"));
    assert_eq!(
        *golden::get(&body, "cookies"),
        pyjson::loads(r#"[{"name": "SESSDATA", "value": "s", "domain": ".bilibili.com", "path": "/", "expires": 2000000000.0}]"#).unwrap()
    );
    assert!(!answer.text().contains("never-served"));
    harness.finish().await;
}

// py: tests/test_credentials_api.py::test_the_api_token_is_not_a_credential_client
#[tokio::test]
async fn the_api_token_is_not_a_credential_client() {
    let harness = credentials_harness().await;
    assert_eq!(
        harness.request("GET", "/api/v2/credentials/bilibili-main", &[], None).await.status,
        401
    );
    assert_eq!(
        harness
            .request(
                "GET",
                "/api/v2/credentials/bilibili-main",
                &[("Authorization", "Bearer other")],
                None
            )
            .await
            .status,
        403
    );
    assert_eq!(harness.get("/api/v2/credentials/bilibili-main").await.status, 403);
    harness.finish().await;
}

// py: tests/test_credentials_api.py::test_a_client_is_limited_to_its_sites
#[tokio::test]
async fn a_client_is_limited_to_its_sites() {
    let harness = credentials_harness().await;
    let answer = client_get(&harness, "/api/v2/credentials/twitter").await;
    assert_eq!(answer.status, 403);
    assert_eq!(answer.error_code(), "site_not_allowed");
    harness.finish().await;
}

// py: tests/test_credentials_api.py::test_sessions_without_values_or_rows_are_not_found
#[tokio::test]
async fn sessions_without_values_or_rows_are_not_found() {
    let harness = credentials_harness().await;
    assert_eq!(client_get(&harness, "/api/v2/credentials/rednote").await.status, 404);
    assert_eq!(client_get(&harness, "/api/v2/credentials/pixiv").await.status, 404);
    harness.finish().await;
}

// ---------- the archive tables ----------

/// The golden rows go into the real tables; the API answers Python's serialization of
/// each, newest first, and counts and filters them in PostgreSQL.
#[tokio::test]
async fn archive_lists_the_real_tables() {
    let harness = Harness::new().await;
    let document = golden::load("api/archive.json");
    let mut expected: std::collections::BTreeMap<String, Vec<(String, Value)>> = std::collections::BTreeMap::new();
    for case in &golden::cases(&document)[1..] {
        let Some(source_key) = case.get("source") else { continue };
        let source_key = golden::string(source_key);
        let source = crate::archive::source(&source_key).expect("a source");
        let row = golden::get(case, "row").as_dict().expect("a row");
        let columns: Vec<String> = row
            .iter()
            .map(|(column, _)| format!("\"{}\"", fav_runtime::pystr::str_key(column)))
            .collect();
        let values: Vec<String> = row
            .iter()
            .map(|(_, value)| match value.get("timestamp") {
                Some(timestamp) => format!("'{}'::timestamp", golden::string(timestamp)),
                None => match value {
                    Value::Null => "NULL".to_owned(),
                    Value::Str(text) => format!("'{}'", text.replace('\'', "''")),
                    other => pyjson::dumps(other, &DumpOptions::compact()).expect("a number"),
                },
            })
            .collect();
        let mut extra_columns = String::new();
        let mut extra_values = String::new();
        if source.table == "wechat_inbox" || source.table == "wechat_article" || source.table == "wechat_channel_video" {
            extra_columns.push_str(", talker");
            extra_values.push_str(", ''");
        }
        harness
            .execute(
                &format!(
                    "INSERT INTO {} ({}{extra_columns}) VALUES ({}{extra_values});",
                    source.table,
                    columns.join(", "),
                    values.join(", ")
                ),
                &[],
            )
            .await;
        let created_at = golden::get(golden::get(case, "item"), "created_at");
        expected
            .entry(source_key)
            .or_default()
            .push((created_at.as_str().unwrap_or("").to_owned(), golden::get(case, "item").clone()));
    }

    for (source_key, mut items) in expected {
        // Newest first, missing times last (each source's golden ids already descend with time).
        items.sort_by(|left, right| match (left.0.is_empty(), right.0.is_empty()) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => right.0.cmp(&left.0),
        });
        let answer = harness.get(&format!("/api/v2/archive/items?source={source_key}")).await;
        assert_eq!(answer.status, 200, "{source_key}: {}", answer.text());
        let body = answer.json();
        let got = golden::list(golden::get(&body, "items")).to_vec();
        let want: Vec<Value> = items.into_iter().map(|(_, item)| item).collect();
        assert_eq!(got, want, "{source_key}");
        assert_eq!(golden::int(golden::get(&body, "total")), i128::try_from(want.len()).unwrap());
        assert_eq!(golden::int(golden::get(&body, "limit")), 50);
        assert_eq!(golden::int(golden::get(&body, "offset")), 0);
    }

    // Search is a case-insensitive substring of the search columns; paging is bounded.
    let answer = harness.get("/api/v2/archive/items?source=bilibili&q=%E6%A0%87").await;
    assert_eq!(golden::int(golden::get(&answer.json(), "total")), 1);
    let answer = harness.get("/api/v2/archive/items?source=bilibili&q=bv2YY").await;
    assert_eq!(golden::int(golden::get(&answer.json(), "total")), 1);
    let answer = harness.get("/api/v2/archive/items?source=bilibili&q=%20%20").await;
    assert_eq!(golden::int(golden::get(&answer.json(), "total")), 2);
    let answer = harness.get("/api/v2/archive/items?source=bilibili&limit=1&offset=1").await;
    let body = answer.json();
    assert_eq!(golden::list(golden::get(&body, "items")).len(), 1);
    assert_eq!(
        golden::get(&golden::list(golden::get(&body, "items"))[0], "id"),
        &Value::from("BV2yy")
    );
    let answer = harness
        .get("/api/v2/archive/items?source=bilibili&offset=99999999999999999999")
        .await;
    assert_eq!(answer.status, 200);
    assert_eq!(golden::list(golden::get(&answer.json(), "items")).len(), 0);

    let answer = harness.get("/api/v2/archive/sources").await;
    assert_eq!(answer.status, 200);
    let body = answer.json();
    let bilibili = &golden::list(golden::get(&body, "items"))[0];
    assert_eq!(
        *bilibili,
        pyjson::loads(r#"{"source": "bilibili", "name": "Bilibili", "total": 2, "latest_at": "2026-03-01T12:00:00"}"#).unwrap()
    );
    assert_eq!(golden::int(golden::get(&body, "total")), 11);
    harness.finish().await;
}

// ---------- site sessions ----------

#[tokio::test]
async fn site_sessions_never_show_cookie_values() {
    let harness = credentials_harness().await;
    let answer = harness.get("/api/v2/site-sessions").await;
    assert_eq!(answer.status, 200);
    assert!(!answer.text().contains("\"s\""));
    assert!(!answer.text().contains("never-served"));
    assert_eq!(golden::get(&answer.json(), "login"), &Value::Null);
    harness.finish().await;
}

// ---------- the front end ----------

fn front_end() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a directory");
    std::fs::create_dir_all(dir.path().join("assets")).unwrap();
    std::fs::write(dir.path().join("index.html"), "<!doctype html><title>fav</title>").unwrap();
    std::fs::write(dir.path().join("assets/index-abc.js"), "console.log(1)").unwrap();
    std::fs::write(dir.path().join("favicon.ico"), [0u8, 1, 2]).unwrap();
    std::fs::write(dir.path().join("secret.txt"), "outside assets").unwrap();
    dir
}

#[tokio::test]
async fn the_front_end_is_served_with_its_deep_links() {
    let mut harness = Harness::new().await;
    let dist = front_end();
    harness.state.web_dist = Some(PathBuf::from(dist.path()));

    let index = harness.request("GET", "/", &[], None).await;
    assert_eq!(index.status, 200);
    assert_eq!(index.header("content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(index.text(), "<!doctype html><title>fav</title>");
    // A deep link survives a refresh; HEAD too.
    assert_eq!(harness.request("GET", "/records/bilibili", &[], None).await.text(), index.text());
    assert_eq!(harness.request("HEAD", "/logins", &[], None).await.status, 200);
    // The backend's paths stay JSON errors.
    for path in ["/api/v2/nope", "/mcp", "/readyz", "/docs/nope", "/healthz/x"] {
        let answer = harness.request("GET", path, &[], None).await;
        assert_eq!(answer.status, 404, "{path}");
        assert_eq!(answer.error_code(), "not_found", "{path}");
    }
    // Only GET and HEAD get the page.
    assert_eq!(harness.request("POST", "/records", &[], None).await.status, 404);

    let script = harness.request("GET", "/assets/index-abc.js", &[], None).await;
    assert_eq!(script.status, 200);
    assert_eq!(script.header("content-type"), Some("text/javascript; charset=utf-8"));
    assert_eq!(script.text(), "console.log(1)");
    // A missing asset or one outside the directory is the page, as Starlette's 404 is.
    assert_eq!(harness.request("GET", "/assets/missing.js", &[], None).await.text(), index.text());
    assert_eq!(
        harness.request("GET", "/assets/../secret.txt", &[], None).await.text(),
        index.text()
    );
    assert_eq!(
        harness.request("GET", "/assets/%2E%2E/secret.txt", &[], None).await.text(),
        index.text()
    );
    // Dot segments inside the directory are resolved, as `os.path.normpath` does.
    let dotted = harness.request("GET", "/assets/x/../index-abc.js", &[], None).await;
    assert_eq!((dotted.status, dotted.text()), (200, "console.log(1)".to_owned()));
    let posted = harness.request("POST", "/assets/index-abc.js", &[], None).await;
    assert_eq!(posted.status, 405);
    assert_eq!(posted.header("allow"), None);
    // `/assets` itself redirects to the mount.
    let bare = harness.request("GET", "/assets", &[], None).await;
    assert_eq!(bare.status, 307);
    assert_eq!(bare.header("location"), Some("http://testserver/assets/"));

    let icon = harness.request("GET", "/favicon.ico", &[], None).await;
    assert_eq!(icon.status, 200);
    assert_eq!(icon.body.as_ref(), [0u8, 1, 2]);
    let head = harness.request("HEAD", "/favicon.ico", &[], None).await;
    assert_eq!(head.status, 405);
    assert_eq!(head.header("allow"), Some("GET"));
    // An icon the build does not have falls back to the page.
    assert_eq!(harness.request("GET", "/icon.svg", &[], None).await.text(), index.text());
    harness.finish().await;
}

#[tokio::test]
async fn without_a_front_end_unknown_paths_are_json() {
    let harness = Harness::new().await;
    let answer = harness.request("GET", "/records", &[], None).await;
    assert_eq!(answer.status, 404);
    assert_eq!(answer.error_code(), "not_found");
    assert_eq!(harness.request("GET", "/assets/index.js", &[], None).await.status, 404);
    harness.finish().await;
}

#[tokio::test]
async fn non_ascii_tokens_are_refused_not_crashed() {
    let harness = Harness::new().await;
    let answer = harness
        .request("GET", "/api/v2/jobs", &[("Authorization", "Bearer tökén")], None)
        .await;
    assert_eq!(answer.status, 403);
    harness.finish().await;
}

// ---------- shutdown ----------

#[tokio::test]
async fn a_stalled_connection_cannot_hold_the_server_after_a_stop() {
    use tokio::io::AsyncWriteExt as _;

    let Harness { database, state } = Harness::new().await;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.expect("a port");
    let address = listener.local_addr().expect("an address");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let grace = std::time::Duration::from_millis(300);
    let server = tokio::spawn(crate::serve_listener(
        listener,
        Arc::new(state),
        async move {
            let _ = stopped.await;
        },
        grace,
    ));

    // A request whose headers never end keeps the connection busy forever.
    let mut client = tokio::net::TcpStream::connect(address).await.expect("connected");
    client.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n").await.expect("written");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let stopped_at = std::time::Instant::now();
    stop.send(()).expect("the server is waiting");
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("the server stops within its grace period")
        .expect("the task ran");
    assert!(result.is_ok(), "{result:?}");
    assert!(stopped_at.elapsed() >= grace, "it waited for the connection first");
    drop(client);
    database.drop_database().await;
}
