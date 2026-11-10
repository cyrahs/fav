use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fav_core::pyjson::{self, Dict, Value};
use fav_core::settings::Settings;
use fav_core::validate::PyPath;
use fav_db::Db;
use fav_db::testdb::TestDatabase;
use fav_net::retry::RecordingSleeper;
use serde_json::json;
use sqlx::Row as _;

use super::*;
use crate::content_discovery::CharacterSnapshot;
use crate::nikke::tests::dict;
use crate::nikke::{extract_resources, merge_live2d_layer_captures};
use crate::nikke_layer_metadata::{RUNTIME_ANIMATION_CAPTURE_SCHEMA, layer_capture_manifest_summary, live2d_layer_fingerprint, sha256_hex};
use crate::testkit::RecordingNotifier;
use crate::testutil::{FakeWeb, Reply, Seen};

const CONTENT_ID: i128 = 711_133;
const CDN: &str = "http://cdn.example.com";

// ---------- fakes ----------

type CaptureFn = Box<dyn Fn(&RuntimeCaptureRequest) -> Result<Dict, PyError> + Send + Sync>;

/// A runtime capturer that answers from a closure and remembers each request, with how
/// many requests the fake web had served by then.
struct FakeCapturer {
    web: FakeWeb,
    respond: CaptureFn,
    calls: Mutex<Vec<(RuntimeCaptureRequest, usize, Option<Dict>)>>,
}

impl FakeCapturer {
    fn new(web: &FakeWeb, respond: impl Fn(&RuntimeCaptureRequest) -> Result<Dict, PyError> + Send + Sync + 'static) -> Self {
        Self {
            web: web.clone(),
            respond: Box::new(respond),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The tests that must not reach the browser: a call fails and is recorded.
    fn unexpected(web: &FakeWeb) -> Self {
        Self::new(web, |_| Err(PyError::new("AssertionError", "unexpected runtime capture")))
    }

    fn calls(&self) -> Vec<(RuntimeCaptureRequest, usize, Option<Dict>)> {
        self.calls.lock().expect("calls").clone()
    }
}

impl RuntimeCapturer for FakeCapturer {
    fn capture(&self, request: RuntimeCaptureRequest) -> BoxFuture<'_, Result<Dict, PyError>> {
        Box::pin(async move {
            let seen = self.web.seen().len();
            let result = (self.respond)(&request);
            self.calls
                .lock()
                .expect("calls")
                .push((request, seen, result.as_ref().ok().cloned()));
            result
        })
    }
}

struct Harness {
    nikke: Nikke,
    web: FakeWeb,
    database: Option<TestDatabase>,
    tmp: tempfile::TempDir,
    notifier: Arc<RecordingNotifier>,
}

impl Harness {
    async fn new(web: FakeWeb, configure: impl FnOnce(&mut settings::Nikke)) -> Self {
        let database = crate::testutil::database().await;
        let db = database.db().clone();
        Self::build(web, db, Some(database), configure)
    }

    /// No database behind it, for tests that never reach one.
    fn offline(web: FakeWeb) -> Self {
        let db = Db::connect_lazy("postgresql://tests@127.0.0.1:1/unused").expect("lazy pool");
        Self::build(web, db, None, |_| {})
    }

    fn build(web: FakeWeb, db: Db, database: Option<TestDatabase>, configure: impl FnOnce(&mut settings::Nikke)) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Settings::default().web.nikke;
        cfg.path = PyPath::new(tmp.path().to_str().expect("UTF-8 path"));
        configure(&mut cfg);
        let options = ClientOptions {
            proxy: Some(web.proxy.clone()),
            timeout: Duration::from_secs(5),
            ..client_options()
        };
        let notifier = Arc::new(RecordingNotifier::default());
        let mut nikke = Nikke::new(
            cfg,
            HttpClient::new(&options).expect("client"),
            db,
            Arc::clone(&notifier) as Arc<dyn Notifier>,
        );
        nikke.base_url = "http://www.gamekee.com".to_owned();
        nikke.sleeper = RecordingSleeper::new().sleeper();
        nikke.pacer = RecordingSleeper::new().sleeper();
        nikke.jitter = || 0.0;
        nikke.capturer = Arc::new(FakeCapturer::unexpected(&web));
        Self {
            nikke,
            web,
            database,
            tmp,
            notifier,
        }
    }

    fn with_capturer(mut self, capturer: &Arc<FakeCapturer>) -> Self {
        self.nikke.capturer = Arc::clone(capturer) as Arc<dyn RuntimeCapturer>;
        self
    }

    fn db(&self) -> &Db {
        self.database.as_ref().expect("a database").db()
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().join("711133 - Test")
    }

    async fn completed(&self, content_id: i128) -> bool {
        let rows = crate::execute(
            self.db(),
            "SELECT completed_at IS NOT NULL AS done FROM nikke_pages WHERE content_id = $1;",
            vec![Param::from(i64::try_from(content_id).expect("bigint"))],
        )
        .await
        .expect("query");
        rows.first().is_some_and(|row| row.try_get::<bool, _>("done").expect("done"))
    }

    /// What the page went through, in order, as `_install_crawl_page_fakes` records it:
    /// the atlas downloads are `expand`, the other files `process`, the capture call
    /// `capture`, and the completed page `complete`.
    async fn events(&self, capture_at: Option<usize>) -> Vec<&'static str> {
        let mut events: Vec<&'static str> = Vec::new();
        let mut push = |event: &'static str| {
            if events.last() != Some(&event) {
                events.push(event);
            }
        };
        let urls = self.web.urls();
        for (index, url) in urls.iter().enumerate() {
            if capture_at == Some(index) {
                push("capture");
            }
            if url.contains("/v1/") {
                continue;
            }
            push(if Path::new(url).extension().is_some_and(|ext| ext == "atlas") {
                "expand"
            } else {
                "process"
            });
        }
        if capture_at == Some(urls.len()) {
            push("capture");
        }
        if self.completed(CONTENT_ID).await {
            events.push("complete");
        }
        events
    }

    async fn finish(self) {
        if let Some(database) = self.database {
            database.drop_database().await;
        }
    }
}

// ---------- fixtures ----------

fn live2d_cell(key: &str, live2d_key: &str) -> serde_json::Value {
    json!({
        "type": "live2d",
        "key": key,
        "value": {
            "live2dKey": live2d_key,
            "atlas": format!("{CDN}/{live2d_key}/model.atlas"),
            "skel": format!("{CDN}/{live2d_key}/model.skel"),
        },
    })
}

/// `_runtime_content_json`: one skin drawn from two stacked full-body models.
fn runtime_content_json() -> serde_json::Value {
    json!({
        "styleData": [
            {
                "name": "default",
                "data": [[{"type": "text", "value": "Model"}, live2d_cell("main-cell", "main"), live2d_cell("back-cell", "back")]],
            },
        ],
    })
}

/// `_runtime_reverse_bind`, as GameKee's `entry_data_bind` (stable id to cell key).
fn entry_data_bind() -> serde_json::Value {
    json!({"stable-main": "main-cell", "stable-back": "back-cell", "extra-main": "extra-main-cell", "extra-back": "extra-back-cell"})
}

fn reverse_bind() -> ReverseBind {
    reverse_bind_map(&dict(&json!({"entry_data_bind": entry_data_bind()})))
}

fn list_row() -> Dict {
    dict(&json!({"title": "Test"}))
}

/// The models `extract_resources` finds on the page, before the atlas textures are added.
fn page_models(content_json: &serde_json::Value) -> Vec<Dict> {
    extract_resources(&dict(content_json), Some(&list_row()), &reverse_bind()).1
}

fn runtime_animations() -> serde_json::Value {
    json!({
        "idle": {"animation": "idle", "enabled": true, "loop": true,
                 "match_method": "gamekee-runtime-player", "match_confidence": "high"},
        "click": {"animation": "action", "enabled": true, "loop": false,
                  "match_method": "gamekee-runtime-player-event", "match_confidence": "high", "duration_ms": 1200},
    })
}

/// `_runtime_capture_payload`.
fn runtime_capture_payload(content_id: i128, models: &[Dict]) -> Dict {
    let refs: Vec<&Dict> = models.iter().collect();
    dict(&json!({
        "content_id": content_id,
        "status": "success",
        "layer_capture_schema": RUNTIME_ANIMATION_CAPTURE_SCHEMA,
        "fingerprint": live2d_layer_fingerprint(content_id, &refs),
        "layers": [
            {"stable_id": "stable-main", "live2d_key": "main", "layer_order": 2, "source_z_index": 9,
             "source_layer_index": 0, "is_primary": true, "layer_match_confidence": "high",
             "runtime_animations": runtime_animations()},
            {"stable_id": "stable-back", "live2d_key": "back", "layer_order": 1, "source_z_index": 8,
             "source_layer_index": 1, "is_primary": false, "layer_match_confidence": "high",
             "runtime_animations": runtime_animations()},
        ],
    }))
}

/// `_capture_hash`.
fn capture_hash(payload: &Dict) -> String {
    sha256_hex(json_dumps(&Value::Dict(payload.clone())).as_bytes())
}

fn detail_response(content_id: i128, content_json: &serde_json::Value, title: &str) -> serde_json::Value {
    json!({
        "code": 0,
        "data": {
            "title": title,
            "entry_id": content_id,
            "updated_at": "2026-07-07T00:00:00+00:00",
            "content_json": serde_json::to_string(content_json).expect("JSON"),
            "entry_data_bind": entry_data_bind(),
        },
    })
}

/// GameKee and its CDN: the detail API answers for `pages`, every atlas names one
/// `expanded.png` texture, and every other file is its own URL's bytes.
fn gamekee(
    pages: Vec<(i128, serde_json::Value)>,
    list: serde_json::Value,
    files: impl Fn(&Seen) -> Option<Reply> + Send + Sync + 'static,
) -> impl Fn(&Seen) -> Reply + Send + Sync + 'static {
    move |seen| {
        if let Some(reply) = files(seen) {
            return reply;
        }
        if seen.host() == "www.gamekee.com" {
            if seen.path() == "/v1/entry/tj-list" {
                return Reply::json(200, &list);
            }
            for (content_id, response) in &pages {
                if seen.path() == format!("/v1/content/detail/{content_id}") {
                    return Reply::json(200, response);
                }
            }
            return Reply::status(404);
        }
        if Path::new(seen.path()).extension().is_some_and(|ext| ext == "atlas") {
            return Reply::bytes(200, "expanded.png\nsize: 1,1\n");
        }
        Reply::bytes(200, seen.url.clone())
    }
}

async fn page_web(content_json: &serde_json::Value) -> FakeWeb {
    let pages = vec![(CONTENT_ID, detail_response(CONTENT_ID, content_json, "Test"))];
    FakeWeb::start(gamekee(pages, json!({"code": 0, "data": []}), |_| None)).await
}

fn write_text(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn write_json_value(path: &Path, value: &Value) {
    write_text(path, &pyjson::dumps(value, &pyjson::DumpOptions::python_default()).expect("dumps"));
}

fn read_document(path: &Path) -> Dict {
    match pyjson::loads(&std::fs::read_to_string(path).expect("read")).expect("JSON") {
        Value::Dict(document) => document,
        other => panic!("not an object: {other:?}"),
    }
}

fn get<'a>(dict: &'a Dict, key: &str) -> &'a Value {
    dict.get(key).unwrap_or_else(|| panic!("missing {key} in {dict:?}"))
}

fn sub<'a>(dict: &'a Dict, key: &str) -> &'a Dict {
    match get(dict, key) {
        Value::Dict(inner) => inner,
        other => panic!("{key} is not a dict: {other:?}"),
    }
}

fn models_of(document: &Dict) -> Vec<&Dict> {
    match get(document, "live2d_models") {
        Value::List(models) => models
            .iter()
            .map(|model| match model {
                Value::Dict(model) => model,
                other => panic!("not a dict: {other:?}"),
            })
            .collect(),
        other => panic!("not a list: {other:?}"),
    }
}

fn s(text: &str) -> Value {
    Value::from(text)
}

async fn crawl(harness: &Harness, allow_runtime_capture: Option<bool>) -> PathBuf {
    harness
        .nikke
        .crawl_page(Some(&list_row()), CONTENT_ID, false, allow_runtime_capture)
        .await
        .expect("the page crawls")
}

// ---------- the detail's content ----------

// py: tests/test_nikke.py::test_load_content_json_uses_content_cdn_when_inline_content_is_empty
#[tokio::test]
async fn load_content_json_uses_content_cdn_when_inline_content_is_empty() {
    let web = FakeWeb::start(|_| {
        Reply::json(
            200,
            &json!({"content": serde_json::to_string(&json!({"styleData": [{"name": "default", "data": []}]})).expect("JSON")}),
        )
    })
    .await;
    let harness = Harness::offline(web);
    // GameKee's protocol-relative CDN links are read over HTTPS.
    let relative = dict(&json!({"content_json": "", "content_cdn": "//api-cdn.example.test/content/1.json"}));
    assert_eq!(
        cdn_json_url(&relative, "content_cdn"),
        "https://api-cdn.example.test/content/1.json"
    );

    let detail = dict(&json!({"content_json": "", "content_cdn": "http://api-cdn.example.test/content/1.json"}));
    let content_json = harness.nikke.load_content_json(&detail, 1).await.expect("content");

    assert_eq!(content_json, dict(&json!({"styleData": [{"name": "default", "data": []}]})));
    assert_eq!(harness.web.urls(), ["http://api-cdn.example.test/content/1.json"]);
}

// py: tests/test_nikke.py::test_load_content_json_rejects_empty_inline_content_without_cdn
#[tokio::test]
async fn load_content_json_rejects_empty_inline_content_without_cdn() {
    let harness = Harness::offline(FakeWeb::start(|_| Reply::status(500)).await);

    let error = harness
        .nikke
        .load_content_json(&dict(&json!({"content_json": ""})), 1)
        .await
        .expect_err("no content");

    assert_eq!(error.class_name(), "RuntimeError");
    assert!(error.message().contains("content_json is empty"), "{}", error.message());
    assert!(harness.web.urls().is_empty());
}

// py: tests/test_nikke.py::test_load_reverse_bind_uses_entry_data_bind_cdn
#[tokio::test]
async fn load_reverse_bind_uses_entry_data_bind_cdn() {
    let web = FakeWeb::start(|_| Reply::json(200, &json!({"entry_data_bind": {"stable-image": "dynamic-image"}}))).await;
    let harness = Harness::offline(web);

    let detail = dict(&json!({"entry_data_bind_cdn": "http://api-cdn.example.test/entry_binding/entry_bind_data.json"}));
    let reverse_bind = harness.nikke.load_reverse_bind(&detail, 1).await.expect("reverse bind");

    assert_eq!(
        reverse_bind,
        ReverseBind::from([("dynamic-image".to_owned(), "stable-image".to_owned())])
    );
}

// ---------- layer captures during a page crawl ----------

// py: tests/test_nikke.py::test_crawl_page_reuses_previous_layer_capture_before_manifest_write
#[tokio::test]
async fn crawl_page_reuses_previous_layer_capture_before_manifest_write() {
    let content_json = runtime_content_json();
    let web = page_web(&content_json).await;
    let harness = Harness::new(web, |cfg| {
        cfg.runtime_capture_enabled = false;
        cfg.runtime_capture_force_refresh = false;
        cfg.runtime_capture_timeout_seconds = 60.0;
    })
    .await;
    let previous_capture = runtime_capture_payload(CONTENT_ID, &page_models(&content_json));
    let raw_capture_path = harness.root().join("raw/live2d-layer-capture.json");
    write_json_value(&raw_capture_path, &Value::Dict(previous_capture.clone()));

    let root = crawl(&harness, None).await;

    let saved_manifest = read_document(&root.join("manifest.json"));
    let saved_character = read_document(&root.join("character.json"));
    let saved_capture = read_document(&raw_capture_path);
    let layers: Vec<(Value, Value, Value)> = models_of(&saved_manifest)
        .iter()
        .map(|model| {
            (
                get(model, "stable_id").clone(),
                model.get("layer_order").cloned().unwrap_or(Value::Null),
                model.get("is_primary").cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
    assert_eq!(
        layers,
        [
            (s("stable-main"), Value::Int(2), Value::Bool(true)),
            (s("stable-back"), Value::Int(1), Value::Bool(false)),
        ]
    );
    assert_eq!(get(&saved_character, "live2d_models"), get(&saved_manifest, "live2d_models"));
    assert_eq!(saved_capture, previous_capture);
    assert_eq!(get(&saved_capture, "status"), &s("success"));
    let summary = sub(&saved_manifest, "live2d_layer_capture");
    assert_eq!(get(summary, "status"), &s("success"));
    assert_eq!(get(summary, "action"), &s("reused"));
    assert_eq!(get(summary, "fingerprint"), get(&saved_capture, "fingerprint"));
    assert_eq!(get(summary, "capture_hash"), &Value::Str(capture_hash(&saved_capture)));
    assert!(get(summary, "attempted_at").is_truthy());
    assert_eq!(
        get(&saved_character, "live2d_layer_capture"),
        get(&saved_manifest, "live2d_layer_capture")
    );
    assert_eq!(harness.events(None).await, ["expand", "process", "complete"]);
    harness.finish().await;
}

// py: tests/test_nikke.py::test_crawl_page_captures_runtime_layers_when_enabled
#[tokio::test]
async fn crawl_page_captures_runtime_layers_when_enabled() {
    let content_json = runtime_content_json();
    let web = page_web(&content_json).await;
    let capturer = Arc::new(FakeCapturer::new(&web, |request| {
        for model in &request.models {
            match model.get("urls") {
                Some(Value::Dict(urls)) => assert!(urls.contains_key("atlas_textures"), "{urls:?}"),
                other => panic!("no urls: {other:?}"),
            }
        }
        let mut payload = runtime_capture_payload(request.content_id, &request.models);
        payload.insert(
            "runtime",
            Value::Dict(dict(&json!({
                "requested_live2d_keys": ["main", "back"],
                "matched_live2d_keys": ["main", "back"],
                "container_count": 2,
                "raw_browser_state": {"path": "/var/lib/chromium-profile"},
            }))),
        );
        payload.insert(
            "warnings",
            Value::from_serde(&json!(["safe warning", {"raw": "object"}, "path /var/lib/chromium-profile"])),
        );
        if let Some(Value::List(layers)) = payload.get_mut("layers")
            && let Some(Value::Dict(first)) = layers.first_mut()
        {
            first.insert(
                "raw_container",
                Value::Dict(dict(&json!({"class_name": "spine-player-container", "style": "z-index: 9"}))),
            );
        }
        Ok(payload)
    }));
    let harness = Harness::new(web, |cfg| {
        cfg.runtime_capture_enabled = true;
        cfg.runtime_capture_force_refresh = false;
        cfg.runtime_capture_timeout_seconds = 12.5;
    })
    .await
    .with_capturer(&capturer);

    let root = crawl(&harness, None).await;

    let saved_manifest = read_document(&root.join("manifest.json"));
    let saved_character = read_document(&root.join("character.json"));
    let saved_capture = read_document(&root.join("raw/live2d-layer-capture.json"));
    let calls = capturer.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.timeout_ms, 12_500);
    // Dict equality, as Python's `==`: the file is written with sorted keys.
    let returned = calls[0].2.clone().expect("the capture succeeded");
    assert!(py_eq(&Value::Dict(saved_capture.clone()), &Value::Dict(returned)));
    let models = models_of(&saved_manifest);
    assert_eq!(get(models[0], "layer_order"), &Value::Int(2));
    assert_eq!(get(models[1], "layer_order"), &Value::Int(1));
    assert_eq!(get(&saved_character, "live2d_models"), get(&saved_manifest, "live2d_models"));
    let Value::List(saved_layers) = get(&saved_capture, "layers") else {
        panic!("layers")
    };
    assert_eq!(saved_layers.len(), 2);
    let Value::Dict(first_layer) = &saved_layers[0] else {
        panic!("layer")
    };
    assert_eq!(get(sub(first_layer, "raw_container"), "class_name"), &s("spine-player-container"));
    assert_eq!(get(&saved_capture, "status"), &s("success"));
    let summary = sub(&saved_manifest, "live2d_layer_capture");
    assert_eq!(get(summary, "status"), &s("success"));
    assert_eq!(get(summary, "action"), &s("captured"));
    assert_eq!(get(summary, "fingerprint"), get(&saved_capture, "fingerprint"));
    assert_eq!(get(summary, "capture_hash"), &Value::Str(capture_hash(&saved_capture)));
    let Value::List(summary_layers) = get(summary, "layers") else {
        panic!("layers")
    };
    assert!(summary_layers.iter().all(|layer| match layer {
        Value::Dict(layer) => !layer.contains_key("raw_container"),
        _ => false,
    }));
    assert!(py_eq(
        get(summary, "runtime"),
        &Value::from_serde(&json!({
            "requested_live2d_keys": ["main", "back"],
            "matched_live2d_keys": ["main", "back"],
            "container_count": 2,
        }))
    ));
    assert_eq!(
        get(summary, "warnings"),
        &Value::from_serde(&json!(["safe warning", "path <path>"]))
    );
    assert_eq!(
        get(&saved_character, "live2d_layer_capture"),
        get(&saved_manifest, "live2d_layer_capture")
    );
    assert_eq!(harness.events(Some(calls[0].1)).await, ["expand", "capture", "process", "complete"]);
    harness.finish().await;
}

// py: tests/test_nikke.py::test_crawl_page_writes_failed_layer_capture_summary_without_blocking_manifest
#[tokio::test]
async fn crawl_page_writes_failed_layer_capture_summary_without_blocking_manifest() {
    let content_json = runtime_content_json();
    let web = page_web(&content_json).await;
    let capturer = Arc::new(FakeCapturer::new(&web, |_| {
        Err(PyError::new(
            "RuntimeError",
            "browser unavailable at /var/lib/chromium-profile\nwith details",
        ))
    }));
    let harness = Harness::new(web, |cfg| cfg.runtime_capture_enabled = true)
        .await
        .with_capturer(&capturer);

    let root = crawl(&harness, None).await;

    let saved_manifest = read_document(&root.join("manifest.json"));
    let saved_character = read_document(&root.join("character.json"));
    let summary = sub(&saved_manifest, "live2d_layer_capture");
    assert_eq!(get(summary, "status"), &s("failed"));
    assert_eq!(get(summary, "reason"), &s("runtime_capture_failed"));
    assert_eq!(get(summary, "retryable"), &Value::Bool(true));
    assert_eq!(get(summary, "error_class"), &s("RuntimeError"));
    assert_eq!(get(summary, "error_message"), &s("browser unavailable at <path> with details"));
    assert!(get(summary, "fingerprint").is_truthy());
    assert_eq!(
        get(&saved_character, "live2d_layer_capture"),
        get(&saved_manifest, "live2d_layer_capture")
    );
    assert!(!root.join("raw/live2d-layer-capture.json").exists());
    assert!(models_of(&saved_manifest).iter().all(|model| !model.contains_key("layer_order")));
    let calls = capturer.calls();
    assert_eq!(harness.events(Some(calls[0].1)).await, ["expand", "capture", "process", "complete"]);
    harness.finish().await;
}

// py: tests/test_nikke.py::test_crawl_page_writes_skipped_layer_capture_summary_when_runtime_disabled
#[tokio::test]
async fn crawl_page_writes_skipped_layer_capture_summary_when_runtime_disabled() {
    let web = page_web(&runtime_content_json()).await;
    let harness = Harness::new(web, |cfg| cfg.runtime_capture_enabled = false).await;

    let root = crawl(&harness, None).await;

    let saved_manifest = read_document(&root.join("manifest.json"));
    let saved_character = read_document(&root.join("character.json"));
    let summary = sub(&saved_manifest, "live2d_layer_capture");
    assert_eq!(get(summary, "status"), &s("skipped"));
    assert_eq!(get(summary, "reason"), &s("runtime_capture_disabled"));
    assert_eq!(get(summary, "retryable"), &Value::Bool(false));
    assert!(get(summary, "fingerprint").is_truthy());
    assert_eq!(
        get(&saved_character, "live2d_layer_capture"),
        get(&saved_manifest, "live2d_layer_capture")
    );
    assert_eq!(get(&saved_character, "live2d_models"), get(&saved_manifest, "live2d_models"));
    assert!(!root.join("raw/live2d-layer-capture.json").exists());
    assert_eq!(harness.events(None).await, ["expand", "process", "complete"]);
    harness.finish().await;
}

// py: tests/test_nikke.py::test_crawl_page_does_not_backfill_raw_capture_when_runtime_not_allowed
#[tokio::test]
async fn crawl_page_does_not_backfill_raw_capture_when_runtime_not_allowed() {
    let content_json = runtime_content_json();
    let web = page_web(&content_json).await;
    let capturer = Arc::new(FakeCapturer::unexpected(&web));
    let harness = Harness::new(web, |cfg| cfg.runtime_capture_enabled = true)
        .await
        .with_capturer(&capturer);
    let previous_capture = runtime_capture_payload(CONTENT_ID, &page_models(&content_json));
    let raw_capture_path = harness.root().join("raw/live2d-layer-capture.json");
    write_json_value(&raw_capture_path, &Value::Dict(previous_capture.clone()));

    let root = crawl(&harness, Some(false)).await;

    let saved_manifest = read_document(&root.join("manifest.json"));
    let summary = sub(&saved_manifest, "live2d_layer_capture");
    assert_eq!(get(summary, "status"), &s("skipped"));
    assert_eq!(get(summary, "reason"), &s("runtime_capture_not_allowed"));
    assert!(models_of(&saved_manifest).iter().all(|model| !model.contains_key("layer_order")));
    assert_eq!(read_document(&raw_capture_path), previous_capture);
    assert!(capturer.calls().is_empty());
    assert_eq!(harness.events(None).await, ["expand", "process", "complete"]);
    harness.finish().await;
}

// py: tests/test_nikke.py::test_crawl_page_preserves_committed_manifest_layer_metadata_when_runtime_not_allowed
#[tokio::test]
async fn crawl_page_preserves_committed_manifest_layer_metadata_when_runtime_not_allowed() {
    let content_json = runtime_content_json();
    let web = page_web(&content_json).await;
    let capturer = Arc::new(FakeCapturer::unexpected(&web));
    let harness = Harness::new(web, |cfg| cfg.runtime_capture_enabled = true)
        .await
        .with_capturer(&capturer);
    let mut previous_models = page_models(&content_json);
    let previous_capture = runtime_capture_payload(CONTENT_ID, &previous_models);
    let merge_report = merge_live2d_layer_captures(&mut previous_models, &previous_capture, Some(CONTENT_ID), false);
    let previous_summary = layer_capture_manifest_summary(&previous_capture, &merge_report);
    let mut manifest = dict(&json!({"content_id": CONTENT_ID, "title": "Test"}));
    manifest.insert("live2d_models", Value::List(previous_models.into_iter().map(Value::Dict).collect()));
    manifest.insert("live2d_layer_capture", Value::Dict(previous_summary));
    write_json_value(&harness.root().join("manifest.json"), &Value::Dict(manifest));

    let root = crawl(&harness, Some(false)).await;

    let saved_manifest = read_document(&root.join("manifest.json"));
    let saved_character = read_document(&root.join("character.json"));
    let models = models_of(&saved_manifest);
    assert_eq!(get(models[0], "layer_order"), &Value::Int(2));
    assert_eq!(get(models[1], "is_primary"), &Value::Bool(false));
    assert_eq!(get(&saved_character, "live2d_models"), get(&saved_manifest, "live2d_models"));
    let summary = sub(&saved_manifest, "live2d_layer_capture");
    assert_eq!(get(summary, "status"), &s("success"));
    assert_eq!(get(summary, "action"), &s("preserved"));
    assert_eq!(get(summary, "reason"), &s("runtime_capture_not_allowed"));
    assert!(!root.join("raw/live2d-layer-capture.json").exists());
    assert!(capturer.calls().is_empty());
    assert_eq!(harness.events(None).await, ["expand", "process", "complete"]);
    harness.finish().await;
}

// py: tests/test_nikke.py::test_crawl_page_ignores_corrupt_previous_layer_capture_without_blocking_manifest
#[tokio::test]
async fn crawl_page_ignores_corrupt_previous_layer_capture_without_blocking_manifest() {
    let web = page_web(&runtime_content_json()).await;
    let harness = Harness::new(web, |cfg| cfg.runtime_capture_enabled = false).await;
    let raw_capture_path = harness.root().join("raw/live2d-layer-capture.json");
    write_text(&raw_capture_path, "{not-json");

    let root = crawl(&harness, None).await;

    let saved_manifest = read_document(&root.join("manifest.json"));
    let summary = sub(&saved_manifest, "live2d_layer_capture");
    assert_eq!(get(summary, "status"), &s("skipped"));
    assert_eq!(get(summary, "reason"), &s("runtime_capture_disabled"));
    assert_eq!(get(summary, "previous_capture_error_class"), &s("JSONDecodeError"));
    assert_eq!(std::fs::read_to_string(&raw_capture_path).expect("read"), "{not-json");
    assert_eq!(harness.events(None).await, ["expand", "process", "complete"]);
    harness.finish().await;
}

fn previous_summary(content_json: &serde_json::Value) -> Dict {
    let mut models = page_models(content_json);
    let previous_capture = runtime_capture_payload(CONTENT_ID, &models);
    let merge_report = merge_live2d_layer_captures(&mut models, &previous_capture, Some(CONTENT_ID), true);
    layer_capture_manifest_summary(&previous_capture, &merge_report)
}

// py: tests/test_nikke.py::test_crawl_page_falls_back_to_manifest_summary_when_raw_capture_is_corrupt
#[tokio::test]
async fn crawl_page_falls_back_to_manifest_summary_when_raw_capture_is_corrupt() {
    let content_json = runtime_content_json();
    let web = page_web(&content_json).await;
    let harness = Harness::new(web, |cfg| cfg.runtime_capture_enabled = false).await;
    let previous = previous_summary(&content_json);
    let raw_capture_path = harness.root().join("raw/live2d-layer-capture.json");
    write_text(&raw_capture_path, "{not-json");
    let mut manifest = Dict::new();
    manifest.insert("live2d_layer_capture", Value::Dict(previous.clone()));
    write_json_value(&harness.root().join("manifest.json"), &Value::Dict(manifest));

    let root = crawl(&harness, None).await;

    let saved_manifest = read_document(&root.join("manifest.json"));
    let models = models_of(&saved_manifest);
    assert_eq!(get(models[0], "layer_order"), &Value::Int(2));
    assert_eq!(get(models[1], "layer_order"), &Value::Int(1));
    let summary = sub(&saved_manifest, "live2d_layer_capture");
    assert_eq!(get(summary, "status"), &s("success"));
    assert_eq!(get(summary, "action"), &s("reused"));
    assert_eq!(get(summary, "capture_hash"), get(&previous, "capture_hash"));
    assert_eq!(get(summary, "previous_capture_error_class"), &s("JSONDecodeError"));
    assert_eq!(std::fs::read_to_string(&raw_capture_path).expect("read"), "{not-json");
    assert_eq!(harness.events(None).await, ["expand", "process", "complete"]);
    harness.finish().await;
}

// py: tests/test_nikke.py::test_crawl_page_reuses_manifest_layer_summary_without_writing_raw_capture
#[tokio::test]
async fn crawl_page_reuses_manifest_layer_summary_without_writing_raw_capture() {
    let content_json = runtime_content_json();
    let web = page_web(&content_json).await;
    let harness = Harness::new(web, |cfg| cfg.runtime_capture_enabled = false).await;
    let previous = previous_summary(&content_json);
    let mut manifest = Dict::new();
    manifest.insert("live2d_layer_capture", Value::Dict(previous.clone()));
    write_json_value(&harness.root().join("manifest.json"), &Value::Dict(manifest));

    let root = crawl(&harness, None).await;

    let saved_manifest = read_document(&root.join("manifest.json"));
    let summary = sub(&saved_manifest, "live2d_layer_capture");
    assert_eq!(get(summary, "status"), &s("success"));
    assert_eq!(get(summary, "action"), &s("reused"));
    assert_eq!(get(summary, "capture_hash"), get(&previous, "capture_hash"));
    assert!(!root.join("raw/live2d-layer-capture.json").exists());
    assert_eq!(harness.events(None).await, ["expand", "process", "complete"]);
    harness.finish().await;
}

// py: tests/test_nikke.py::test_crawl_page_writes_no_multi_full_layer_capture_summary
#[tokio::test]
async fn crawl_page_writes_no_multi_full_layer_capture_summary() {
    let mut content_json = runtime_content_json();
    content_json["styleData"][0]["data"][0].as_array_mut().expect("row").truncate(2);
    let web = page_web(&content_json).await;
    let capturer = Arc::new(FakeCapturer::unexpected(&web));
    let harness = Harness::new(web, |cfg| cfg.runtime_capture_enabled = true)
        .await
        .with_capturer(&capturer);

    let root = crawl(&harness, None).await;

    let saved_manifest = read_document(&root.join("manifest.json"));
    let saved_character = read_document(&root.join("character.json"));
    let summary = sub(&saved_manifest, "live2d_layer_capture");
    assert_eq!(get(summary, "status"), &s("skipped"));
    assert_eq!(get(summary, "reason"), &s("no_multi_full_groups"));
    assert_eq!(get(summary, "retryable"), &Value::Bool(false));
    assert_eq!(
        get(&saved_character, "live2d_layer_capture"),
        get(&saved_manifest, "live2d_layer_capture")
    );
    assert!(!root.join("raw/live2d-layer-capture.json").exists());
    assert!(capturer.calls().is_empty());
    assert_eq!(harness.events(None).await, ["expand", "process", "complete"]);
    harness.finish().await;
}

// ---------- the scheduled run ----------

// py: tests/test_nikke.py::test_update_disables_runtime_capture_for_regular_crawler_cron
#[tokio::test]
async fn update_disables_runtime_capture_for_regular_crawler_cron() {
    let pages = vec![(CONTENT_ID, detail_response(CONTENT_ID, &runtime_content_json(), "Test"))];
    let list = json!({"code": 0, "data": [{"nikke": {"content_id": CONTENT_ID, "title": "Test"}}]});
    let web = FakeWeb::start(gamekee(pages, list, |_| None)).await;
    let capturer = Arc::new(FakeCapturer::unexpected(&web));
    let harness = Harness::new(web, |cfg| cfg.runtime_capture_enabled = true)
        .await
        .with_capturer(&capturer);

    harness.nikke.update().await.expect("the run succeeds");

    assert!(capturer.calls().is_empty());
    let saved_manifest = read_document(&harness.root().join("manifest.json"));
    assert_eq!(
        get(sub(&saved_manifest, "live2d_layer_capture"), "reason"),
        &s("runtime_capture_not_allowed")
    );
    harness.finish().await;
}

fn image_page(content_id: i128, title: &str, image: &str) -> serde_json::Value {
    let content_json = json!({"baseData": [[{"type": "text", "value": "头像"}, {"type": "image", "key": "img", "value": image}]]});
    let mut response = detail_response(content_id, &content_json, title);
    response["data"]["entry_data_bind"] = json!({"stable-image": "img"});
    response
}

// py: tests/test_nikke.py::test_update_crawls_every_page_and_reports_the_failed_ones
#[tokio::test]
async fn update_crawls_every_page_and_reports_the_failed_ones() {
    let broken_id: i128 = 691_899;
    let pages = vec![
        (broken_id, image_page(broken_id, "A", &format!("{CDN}/broken.png"))),
        (CONTENT_ID, image_page(CONTENT_ID, "B", &format!("{CDN}/fine.png"))),
    ];
    let list = json!({"code": 0, "data": [
        {"nikke": {"content_id": broken_id, "title": "A"}},
        {"nikke": {"content_id": CONTENT_ID, "title": "B"}},
    ]});
    let web = FakeWeb::start(gamekee(pages, list, |seen| {
        (seen.path() == "/broken.png").then(|| Reply::status(500))
    }))
    .await;
    let harness = Harness::new(web, |_| {}).await;

    let error = harness.nikke.update().await.expect_err("one page failed");

    assert_eq!(error.class_name(), "CrawlRunError");
    assert_eq!(error.message(), "1 Nikke page(s) failed: 691899");
    let details: Vec<String> = harness
        .web
        .urls()
        .into_iter()
        .filter(|url| url.contains("/v1/content/detail/"))
        .collect();
    assert_eq!(
        details,
        [
            "http://www.gamekee.com/v1/content/detail/691899",
            "http://www.gamekee.com/v1/content/detail/711133",
        ]
    );
    assert!(!harness.completed(broken_id).await);
    assert!(harness.completed(CONTENT_ID).await);
    harness.finish().await;
}

// ---------- the blob store ----------

fn fail_link(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("no links here"))
}

// py: tests/test_nikke.py::test_materialize_blob_falls_back_to_copy_when_hardlink_fails
#[test]
fn materialize_blob_falls_back_to_copy_when_hardlink_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let blob = tmp.path().join("blob");
    let data = b"nikke-asset";
    std::fs::write(&blob, data).expect("write");
    let destination = tmp.path().join("character/assets/images/file.bin");

    materialize_blob(&blob, &destination, &sha256_hex(data), 11, fail_link).expect("materialized");

    assert_eq!(std::fs::read(&destination).expect("read"), data);
}

// py: tests/test_nikke.py::test_register_temp_blob_reuses_existing_hash_blob
#[tokio::test]
async fn register_temp_blob_reuses_existing_hash_blob() {
    let data = b"same-content";
    let web = FakeWeb::start(move |_| Reply::bytes(200, data.to_vec())).await;
    let harness = Harness::new(web, |_| {}).await;
    let sha256 = sha256_hex(data);
    let existing_relative = format!("_blobs/sha256/{}/{sha256}", &sha256[..2]);
    let existing = harness.tmp.path().join(&existing_relative);
    write_text(&existing, "same-content");
    crate::execute(
        harness.db(),
        "INSERT INTO nikke_blobs (sha256, size, content_type, blob_path, created_at) VALUES ($1, $2, 'image/png', $3, NOW());",
        vec![Param::from(sha256.clone()), Param::from(12_i64), Param::from(existing_relative)],
    )
    .await
    .expect("insert");
    let asset = Asset::new(format!("{CDN}/same.png"), "image");
    let temp = harness
        .nikke
        .download_asset_to_temp(&asset)
        .await
        .map_err(|failure| failure.error)
        .expect("downloaded");
    let temp_path = temp.file.temp_path().to_path_buf();
    assert!(temp_path.exists());

    let blob = harness.nikke.register_temp_blob(temp).await.expect("registered");

    assert_eq!(blob.path, existing);
    assert!(!temp_path.exists());
    let rows = crate::execute(
        harness.db(),
        "SELECT last_verified_at IS NOT NULL AS verified FROM nikke_blobs WHERE sha256 = $1;",
        vec![Param::from(sha256)],
    )
    .await
    .expect("query");
    assert!(rows[0].try_get::<bool, _>("verified").expect("verified"));
    harness.finish().await;
}

async fn asset_status(db: &Db, url: &str) -> (String, i32, String) {
    let rows = crate::execute(
        db,
        "SELECT status, failed_count, last_error FROM nikke_assets WHERE url = $1;",
        vec![Param::from(url)],
    )
    .await
    .expect("query");
    let row = rows.first().expect("an asset row");
    (
        row.try_get("status").expect("status"),
        row.try_get("failed_count").expect("failed_count"),
        row.try_get("last_error").expect("last_error"),
    )
}

// py: tests/test_nikke.py::test_completed_blob_for_url_marks_missing_when_db_blob_is_gone
#[tokio::test]
async fn completed_blob_for_url_marks_missing_when_db_blob_is_gone() {
    let harness = Harness::new(FakeWeb::start(|_| Reply::status(500)).await, |_| {}).await;
    let url = format!("{CDN}/missing.png");
    let sha256 = "a".repeat(64);
    for (sql, params) in [
        (
            "INSERT INTO nikke_blobs (sha256, size, content_type, blob_path, created_at) VALUES ($1, 10, 'image/png', $2, NOW());",
            vec![Param::from(sha256.clone()), Param::from(format!("_blobs/sha256/aa/{sha256}"))],
        ),
        (
            "INSERT INTO nikke_assets (url, normalized_url, sha256, size, content_type, status, first_seen_at, last_seen_at) \
             VALUES ($1, $1, $2, 10, 'image/png', 'downloaded', NOW(), NOW());",
            vec![Param::from(url.clone()), Param::from(sha256.clone())],
        ),
    ] {
        crate::execute(harness.db(), sql, params).await.expect("insert");
    }

    let blob = harness.nikke.completed_blob_for_url(&url).await.expect("query");

    assert!(blob.is_none());
    assert_eq!(asset_status(harness.db(), &url).await.0, "missing");
    harness.finish().await;
}

fn assets_of(items: &[(&str, &str)]) -> Assets {
    let mut assets = Assets::default();
    for (url, kind) in items {
        assets.entry((*url).to_owned(), kind);
    }
    assets
}

// py: tests/test_nikke.py::test_process_assets_raises_when_any_asset_failed
#[tokio::test]
async fn process_assets_raises_when_any_asset_failed() {
    let web = FakeWeb::start(|seen| {
        if seen.path() == "/b.png" {
            Reply::status(500)
        } else {
            Reply::bytes(200, "a-bytes")
        }
    })
    .await;
    let harness = Harness::new(web, |_| {}).await;
    let mut assets = assets_of(&[("http://cdn.example.com/a.png", "image"), ("http://cdn.example.com/b.png", "image")]);
    assign_asset_paths(&mut assets, 1).expect("paths");
    let root = harness.tmp.path().join("page");

    let error = harness
        .nikke
        .process_assets(&root, &mut assets)
        .await
        .expect_err("one asset failed");

    assert_eq!(error.class_name(), "AssetProcessingError");
    assert!(error.message().starts_with("1 Nikke assets failed"), "{}", error.message());
    assert_eq!(assets.get("http://cdn.example.com/a.png", "image").expect("a").status, "downloaded");
    assert_eq!(assets.get("http://cdn.example.com/b.png", "image").expect("b").status, "failed");
    harness.finish().await;
}

// py: tests/test_nikke.py::test_process_assets_saves_page_when_gamekee_does_not_host_an_asset
#[tokio::test]
async fn process_assets_saves_page_when_gamekee_does_not_host_an_asset() {
    let web = FakeWeb::start(|seen| {
        if seen.path() == "/b.png" {
            Reply::status(404)
        } else {
            Reply::bytes(200, "a-bytes")
        }
    })
    .await;
    let harness = Harness::new(web, |_| {}).await;
    let mut assets = assets_of(&[
        ("http://cdn.example.com/a.png", "image"),
        ("http://cdn.example.com/b.png", "live2d_texture"),
    ]);
    assign_asset_paths(&mut assets, 1).expect("paths");
    let root = harness.tmp.path().join("page");

    harness.nikke.process_assets(&root, &mut assets).await.expect("the page is saved");

    assert_eq!(
        assets.get("http://cdn.example.com/b.png", "live2d_texture").expect("b").status,
        "missing"
    );
    harness.finish().await;
}

// py: tests/test_nikke.py::test_process_asset_marks_only_not_found_as_missing
#[tokio::test]
async fn process_asset_marks_only_not_found_as_missing() {
    for (status_code, expected) in [(404_u16, "missing"), (500, "failed"), (567, "failed")] {
        let web = FakeWeb::start(move |_| Reply::status(status_code)).await;
        let harness = Harness::new(web, |_| {}).await;
        let url = format!("{CDN}/FavoriteItemScene_c192_00.png");
        let mut asset = Asset::new(url.clone(), "live2d_texture");
        asset.local_path = "assets/live2d/FavoriteItemScene_c192_00.png".to_owned();

        let ok = harness.nikke.process_asset(harness.tmp.path(), &mut asset).await.expect("recorded");

        assert!(!ok);
        assert_eq!(asset.status, expected, "HTTP {status_code}");
        // Recorded as failed either way, so the next run asks GameKee for the file again.
        let (status, failed_count, _) = asset_status(harness.db(), &url).await;
        assert_eq!((status.as_str(), failed_count), ("failed", 1), "HTTP {status_code}");
        harness.finish().await;
    }
}

// py: tests/test_nikke.py::test_process_assets_limits_db_backed_asset_processing
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn process_assets_limits_db_backed_asset_processing() {
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let (counter, peak) = (Arc::clone(&active), Arc::clone(&max_active));
    let web = FakeWeb::start(move |seen| {
        let now = counter.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(30));
        counter.fetch_sub(1, Ordering::SeqCst);
        Reply::bytes(200, seen.url.clone())
    })
    .await;
    let mut harness = Harness::new(web, |_| {}).await;
    // Lift the CDN's own limit of 3, so the peak measures the asset-group limit alone.
    harness.nikke.cdn_semaphore = tokio::sync::Semaphore::new(12);
    let urls: Vec<String> = (0..12).map(|index| format!("{CDN}/{index}.png")).collect();
    let items: Vec<(&str, &str)> = urls.iter().map(|url| (url.as_str(), "image")).collect();
    let mut assets = assets_of(&items);
    assign_asset_paths(&mut assets, 1).expect("paths");

    harness
        .nikke
        .process_assets(&harness.tmp.path().join("page"), &mut assets)
        .await
        .expect("processed");

    assert!(assets.iter().all(|asset| asset.status == "downloaded"));
    assert_eq!(max_active.load(Ordering::SeqCst), ASSET_PROCESS_CONCURRENCY);
    harness.finish().await;
}

// ---------- page rows ----------

// py: tests/test_nikke.py::test_upsert_page_fetch_start_clears_completed_at
#[tokio::test]
async fn upsert_page_fetch_start_clears_completed_at() {
    let harness = Harness::new(FakeWeb::start(|_| Reply::status(500)).await, |_| {}).await;
    crate::execute(
        harness.db(),
        "INSERT INTO nikke_pages (content_id, name, completed_at) VALUES (1, 'Nikke', NOW());",
        Vec::new(),
    )
    .await
    .expect("insert");
    assert!(harness.completed(1).await);

    harness
        .nikke
        .upsert_page_fetch_start(
            1,
            "Nikke",
            Some(&Dict::new()),
            &dict(&json!({"data": {"title": "Nikke"}})),
            &harness.tmp.path().join("manifest.json"),
            "abc",
        )
        .await
        .expect("upserted");

    assert!(!harness.completed(1).await);
    harness.finish().await;
}

async fn page_asset_urls(db: &Db) -> Vec<String> {
    crate::execute(
        db,
        "SELECT url FROM nikke_page_assets WHERE content_id = 1 ORDER BY url;",
        Vec::new(),
    )
    .await
    .expect("query")
    .iter()
    .map(|row| row.try_get("url").expect("url"))
    .collect()
}

// py: tests/test_nikke.py::test_replace_page_assets_and_complete_uses_single_transaction
#[tokio::test]
async fn replace_page_assets_and_complete_uses_single_transaction() {
    let harness = Harness::new(FakeWeb::start(|_| Reply::status(500)).await, |_| {}).await;
    for sql in [
        "INSERT INTO nikke_pages (content_id, name) VALUES (1, 'Nikke');",
        "INSERT INTO nikke_assets (url, normalized_url) VALUES ('http://cdn.example.com/old.png', 'http://cdn.example.com/old.png');",
        "INSERT INTO nikke_page_assets (content_id, url, kind, context_hash, local_path, original_filename) \
         VALUES (1, 'http://cdn.example.com/old.png', 'image', 'x', 'assets/images/old.png', 'old.png');",
    ] {
        crate::execute(harness.db(), sql, Vec::new()).await.expect("seed");
    }
    let mut assets = Assets::default();
    let asset = assets.entry("http://cdn.example.com/a.png".to_owned(), "image");
    asset.local_path = "assets/images/a.png".to_owned();
    asset.contexts = vec![dict(&json!({"field": "icon"}))];

    // The new asset has no `nikke_assets` row yet, so its insert fails, and with it the
    // delete before it and the completion after it: one transaction.
    harness
        .nikke
        .replace_page_assets_and_mark_completed(1, &assets)
        .await
        .expect_err("the insert fails");
    assert_eq!(page_asset_urls(harness.db()).await, ["http://cdn.example.com/old.png"]);
    assert!(!harness.completed(1).await);

    crate::execute(
        harness.db(),
        "INSERT INTO nikke_assets (url, normalized_url) VALUES ('http://cdn.example.com/a.png', 'http://cdn.example.com/a.png');",
        Vec::new(),
    )
    .await
    .expect("seed");
    harness
        .nikke
        .replace_page_assets_and_mark_completed(1, &assets)
        .await
        .expect("replaced");

    assert_eq!(page_asset_urls(harness.db()).await, ["http://cdn.example.com/a.png"]);
    assert!(harness.completed(1).await);
    harness.finish().await;
}

// py: tests/test_nikke.py::test_expand_atlas_textures_uses_blob_pipeline_not_direct_get
#[tokio::test]
async fn expand_atlas_textures_uses_blob_pipeline_not_direct_get() {
    let web = FakeWeb::start(|_| Reply::bytes(200, "texture.png\nsize: 1,1\n")).await;
    let harness = Harness::new(web, |_| {}).await;
    let atlas_url = "http://cdn.example.com/model.atlas";
    let mut assets = Assets::default();
    let atlas = assets.entry(atlas_url.to_owned(), "live2d_atlas");
    atlas.local_path = "assets/live2d/model/model.atlas".to_owned();
    atlas.contexts = vec![dict(&json!({"live2d_key": "model"}))];
    let mut models = vec![dict(&json!({"urls": {"atlas": atlas_url}, "live2d_key": "model"}))];

    harness
        .nikke
        .expand_atlas_textures(harness.tmp.path(), &mut assets, &mut models)
        .await
        .expect("expanded");

    // The atlas went through the blob pipeline once and was read back from the blob.
    assert_eq!(harness.web.urls(), [atlas_url]);
    assert_eq!(assets.get(atlas_url, "live2d_atlas").expect("atlas").status, "downloaded");
    assert!(assets.get("http://cdn.example.com/texture.png", "live2d_texture").is_some());
    assert_eq!(
        sub(&models[0], "urls").get("atlas_textures"),
        Some(&Value::from_serde(&json!(["http://cdn.example.com/texture.png"])))
    );
    harness.finish().await;
}

// ---------- discovery notifications ----------

fn rapi(skins: &serde_json::Value) -> Dict {
    dict(&json!({"content_id": 101, "title": "Rapi", "skins": skins}))
}

// py: tests/test_game_content_notifications.py::test_nikke_new_character_notification_includes_character_and_skin_names
#[tokio::test]
async fn nikke_new_character_notification_includes_character_and_skin_names() {
    let harness = Harness::offline(FakeWeb::start(|_| Reply::status(500)).await);
    harness.nikke.notify_discoveries.store(true, Ordering::SeqCst);
    let character = rapi(&json!([{"name": "default", "title": "Default"}, {"name": "summer", "title": "Summer Vacation"}]));

    harness
        .nikke
        .notify_content_discovery(
            &CharacterSnapshot {
                exists: false,
                character: None,
            },
            &character,
        )
        .await;

    let sent = harness.notifier.sent();
    assert_eq!(sent.len(), 1);
    let notification = &sent[0];
    assert_eq!(notification.kind, "content_discovered");
    assert_eq!(notification.source, "nikke");
    assert_eq!(notification.header, "Nikke");
    assert_eq!(notification.title, "New character: Rapi");
    assert_eq!(notification.body, "Character: Rapi\nSkin: Default, Summer Vacation");
    assert_eq!(notification.link_url, "https://www.gamekee.com/nikke/tj/101.html");
    assert_eq!(
        notification.payload,
        Some(dict(&json!({
            "content_id": 101,
            "character_name": "Rapi",
            "skin_names": ["Default", "Summer Vacation"],
            "discovery": "character",
        })))
    );
    assert_eq!(
        *notification,
        NewNotification {
            kind: notification.kind.clone(),
            source: notification.source.clone(),
            header: notification.header.clone(),
            title: notification.title.clone(),
            body: notification.body.clone(),
            link_url: notification.link_url.clone(),
            payload: notification.payload.clone(),
            ..NewNotification::default()
        }
    );
}

// py: tests/test_game_content_notifications.py::test_nikke_existing_character_only_notifies_for_new_skin
#[tokio::test]
async fn nikke_existing_character_only_notifies_for_new_skin() {
    let harness = Harness::offline(FakeWeb::start(|_| Reply::status(500)).await);
    harness.nikke.notify_discoveries.store(true, Ordering::SeqCst);
    let previous = CharacterSnapshot {
        exists: true,
        character: Some(dict(&json!({"skins": [{"name": "default", "title": "Default"}]}))),
    };
    let character = rapi(&json!([
        {"name": "default", "title": "Default (updated translation)"},
        {"name": "summer", "title": "Summer Vacation"},
    ]));

    harness.nikke.notify_content_discovery(&previous, &character).await;

    let sent = harness.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].header, "Nikke");
    assert_eq!(sent[0].title, "New skin: Rapi");
    assert_eq!(sent[0].body, "Character: Rapi\nSkin: Summer Vacation");
    assert_eq!(
        sent[0].payload.as_ref().and_then(|payload| payload.get("skin_names")),
        Some(&Value::from_serde(&json!(["Summer Vacation"])))
    );
}

/// The Nikke side of
/// `test_discovery_notifications_are_suppressed_without_an_existing_archive_baseline`
/// (BD2's test is WP10's): a run over an empty archive announces nothing.
#[tokio::test]
async fn nikke_discovery_notifications_are_suppressed_without_an_existing_archive_baseline() {
    let harness = Harness::offline(FakeWeb::start(|_| Reply::status(500)).await);

    harness
        .nikke
        .notify_content_discovery(
            &CharacterSnapshot {
                exists: false,
                character: None,
            },
            &rapi(&json!([{"name": "default", "title": "Default"}])),
        )
        .await;

    assert!(harness.notifier.sent().is_empty());
}
