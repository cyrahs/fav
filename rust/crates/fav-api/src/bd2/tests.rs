//! The BD2 routes and the Live2D view overrides against Python.
//!
//! `rust/goldens/bd2/api.json` replays requests through the real FastAPI app and
//! `BD2Library` over a fixture tree (its files are in the golden), with an in-memory
//! override store; the replay here builds the same tree and answers from the same kind of
//! store. `rust/goldens/bd2/live2d_overrides.json` pins the model ids and the overlay. The
//! tests after the replays port `tests/test_bd2_api.py`; the PostgreSQL store is checked
//! against a real database.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone as _, Utc};
use fav_core::golden;
use fav_core::pyjson::{self, Dict, Value};
use fav_runtime::BoxFuture;

use super::Bd2Library;
use crate::live2d_overrides::{self, Live2DViewOverrideStore, OverrideKey, OverrideRow, OverrideView, PostgresLive2DViewOverrideStore};
use crate::tests::{Answer, Harness, TOKEN};

type Key = (String, i64, String, String);

/// The tests' in-memory store: a new row is stamped `created_at`, every write `updated_at`
/// (the Nikke tests use it too).
pub(crate) struct FakeStore {
    rows: Mutex<BTreeMap<Key, OverrideRow>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl FakeStore {
    pub(crate) fn new(created_at: DateTime<Utc>, updated_at: DateTime<Utc>) -> Self {
        Self {
            rows: Mutex::new(BTreeMap::new()),
            created_at,
            updated_at,
        }
    }

    fn key(key: &OverrideKey) -> Key {
        (key.source.clone(), key.content_id, key.model_id.clone(), key.profile.clone())
    }
}

fn vector((x, y): (f64, f64)) -> Value {
    let mut dict = Dict::new();
    dict.insert("x", Value::Float(x));
    dict.insert("y", Value::Float(y));
    Value::Dict(dict)
}

impl Live2DViewOverrideStore for FakeStore {
    fn list_for_character<'a>(&'a self, source: &'a str, content_id: i64) -> BoxFuture<'a, Result<Vec<OverrideRow>, String>> {
        Box::pin(async move {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|(key, _)| key.0 == source && key.1 == content_id)
                .map(|(_, row)| row.clone())
                .collect())
        })
    }

    fn get<'a>(&'a self, key: &'a OverrideKey) -> BoxFuture<'a, Result<Option<OverrideRow>, String>> {
        Box::pin(async move { Ok(self.rows.lock().unwrap().get(&Self::key(key)).cloned()) })
    }

    fn upsert<'a>(&'a self, key: &'a OverrideKey, view: &'a OverrideView) -> BoxFuture<'a, Result<OverrideRow, String>> {
        Box::pin(async move {
            let mut rows = self.rows.lock().unwrap();
            let created_at = rows.get(&Self::key(key)).map_or(self.created_at, |row| row.created_at);
            let row = OverrideRow {
                source: key.source.clone(),
                content_id: key.content_id,
                model_id: key.model_id.clone(),
                profile: key.profile.clone(),
                position: vector(view.position),
                scale: view.scale,
                background_position: view.background_position.map(vector),
                background_scale: view.background_scale,
                created_at,
                updated_at: self.updated_at,
            };
            rows.insert(Self::key(key), row.clone());
            Ok(row)
        })
    }

    fn delete<'a>(&'a self, key: &'a OverrideKey) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move { Ok(self.rows.lock().unwrap().remove(&Self::key(key)).is_some()) })
    }
}

fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, second)
        .single()
        .expect("a valid time")
}

async fn harness(root: &Path, store: FakeStore) -> Harness {
    let mut harness = Harness::new().await;
    harness.state.bd2 = tokio::sync::OnceCell::new_with(Some(Arc::new(Bd2Library::new(root))));
    harness.state.live2d_overrides = Arc::new(store);
    harness
}

fn write_files(root: &Path, files: &Value) {
    for (relative, text) in files.as_dict().expect("files").iter() {
        let path = root.join(fav_runtime::pystr::str_key(relative));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, golden::string(text)).unwrap();
    }
}

/// The headers Python's replay records (`_RESPONSE_HEADERS`).
const RECORDED_HEADERS: [&str; 4] = ["content-type", "cache-control", "etag", "allow"];

async fn replay(harness: &Harness, exchanges: &[Value]) {
    for exchange in exchanges {
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
        let context = format!("{method} {path} {body:?}");
        assert_eq!(
            i128::from(answer.status),
            golden::int(golden::get(expected, "status")),
            "{context}: {}",
            answer.text()
        );
        let expected_headers = golden::get(expected, "headers").as_dict().expect("headers");
        for header in RECORDED_HEADERS {
            assert_eq!(
                answer.header(header),
                expected_headers.get(header).and_then(Value::as_str),
                "{context}: header {header}"
            );
        }
        match expected.get("json") {
            Some(json) => assert_eq!(&answer.json(), json, "{context}"),
            None => assert_eq!(answer.text(), golden::string(golden::get(expected, "text")), "{context}"),
        }
    }
}

#[tokio::test]
async fn api_replays_python() {
    let document = golden::load("bd2/api.json");
    let case = &golden::cases(&document)[0];
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bd2");
    write_files(&root, golden::get(case, "files"));
    let store = FakeStore::new(
        utc(2026, 5, 1, 0, 0, 0) + chrono::Duration::microseconds(500_000),
        utc(2026, 5, 2, 3, 4, 5),
    );
    let harness = harness(&root, store).await;
    replay(&harness, golden::list(golden::get(case, "exchanges"))).await;
    // Manifests whose fields fail the response models: the summary cache is rebuilt and
    // every route that validates them answers 500.
    write_files(&root, golden::get(case, "hostile_files"));
    replay(&harness, golden::list(golden::get(case, "hostile_exchanges"))).await;
    harness.finish().await;
}

#[test]
fn model_ids_and_overlay_match_python() {
    let document = golden::load("bd2/live2d_overrides.json");
    for case in golden::cases(&document) {
        let name = golden::string(golden::get(case, "fn"));
        let args = golden::list(golden::get(case, "args"));
        let expected = golden::get(case, "result");
        let error = |message: String| {
            let mut dict = Dict::new();
            dict.insert("error", Value::Str(format!("ValueError: {message}")));
            Value::Dict(dict)
        };
        let result = match name.as_str() {
            "live2d_model_id" => Value::Str(live2d_overrides::live2d_model_id(args[0].as_dict().expect("a model"))),
            "apply_live2d_view_overrides" => {
                let mut character = args[0].as_dict().expect("a character").clone();
                live2d_overrides::apply_live2d_view_overrides(&mut character, golden::list(&args[1]));
                Value::Dict(character)
            }
            "validate_live2d_source" => live2d_overrides::validate_live2d_source(&golden::string(&args[0])).map_or_else(error, Value::Str),
            "validate_live2d_profile" => {
                live2d_overrides::validate_live2d_profile(&golden::string(&args[0])).map_or_else(error, Value::Str)
            }
            other => panic!("unknown function {other}"),
        };
        assert_eq!(&result, expected, "{name} {args:?}");
    }
}

// ---------- tests/test_bd2_api.py ----------

const STATIC_CHARACTER_PREFIX: &str = "/static/bd2/101%20-%20Test%20BD2";

fn parse(text: &str) -> Value {
    pyjson::loads(text).expect("JSON")
}

fn write_json(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, pyjson::dumps(value, &pyjson::DumpOptions::plain()).unwrap()).unwrap();
}

fn write_asset(path: &Path, body: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn obj(pairs: &[(&str, Value)]) -> Value {
    let mut dict = Dict::new();
    for (key, value) in pairs {
        dict.insert(*key, value.clone());
    }
    Value::Dict(dict)
}

/// `{**base, **extra}`.
fn merged(base: &Value, extra: &[(&str, Value)]) -> Value {
    let mut dict = base.as_dict().expect("a dictionary").clone();
    for (key, value) in extra {
        dict.insert(*key, value.clone());
    }
    Value::Dict(dict)
}

fn asset(path: &str, body: &[u8], kind: &str, context: Value, sha256: &str, content_type: &str) -> Value {
    let name = path.rsplit('/').next().unwrap_or(path);
    obj(&[
        ("url", Value::Str(format!("https://cdn.example.test/{name}"))),
        ("kind", Value::from(kind)),
        ("local_path", Value::from(path)),
        ("sha256", Value::from(sha256)),
        ("content_type", Value::from(content_type)),
        ("size", Value::from(i64::try_from(body.len()).unwrap())),
        ("status", Value::from("downloaded")),
        ("error", Value::from("")),
        ("contexts", Value::List(vec![context])),
    ])
}

fn style_context(row_index: i64, label: &str, field: &str, kind_role: &str) -> Value {
    obj(&[
        ("section", Value::from("style")),
        ("style_index", Value::from(0_i64)),
        ("style_name", Value::from("style-a")),
        ("costume_title", Value::from("Office Worker")),
        ("costume_category", Value::from("Support")),
        ("row_index", Value::from(row_index)),
        ("column_index", Value::from(1_i64)),
        ("label", Value::from(label)),
        ("field", Value::from(field)),
        ("is_art_row", Value::Bool(!field.is_empty())),
        ("column_name", Value::from("Office Worker")),
        ("column_category", Value::from("Support")),
        ("column_role", Value::from(kind_role)),
    ])
}

/// `_create_bd2_fixture(root)`.
#[allow(clippy::too_many_lines)] // The Python fixture, field for field.
fn create_fixture(root: &Path) {
    let character_root = root.join("101 - Test BD2");
    let files: Vec<(&str, &[u8])> = vec![
        ("assets/images/icon.png", b"icon"),
        ("assets/images/sprite.png", b"sprite"),
        ("assets/images/other-portrait.png", b"other-portrait"),
        ("assets/images/portrait.png", b"portrait"),
        ("assets/images/full.png", b"full"),
        ("assets/videos/skill.mp4", b"video"),
        ("assets/audio/voice.mp3", b"voice"),
        ("assets/live2d/model-a/model.atlas", b"atlas"),
        ("assets/live2d/model-a/model.json", br#"{"skeleton":{"spine":"4.0.64"}}"#),
        ("assets/live2d/model-a/model.png", b"texture"),
        ("assets/live2d/model-a/background.png", b"background"),
    ];
    for (path, body) in &files {
        write_asset(&character_root.join(path), body);
    }
    let body = |path: &str| files.iter().find(|(name, _)| *name == path).map(|(_, body)| *body).unwrap();
    let live2d_context = merged(
        &style_context(6, "Standing Live2D", "standing_live2d", "Live2D file"),
        &[("live2d_key", Value::from("model-a"))],
    );
    let octet = "application/octet-stream";
    let a = "a".repeat(64);
    let assets = vec![
        asset(
            "assets/images/icon.png",
            body("assets/images/icon.png"),
            "image",
            obj(&[
                ("section", Value::from("entry_tree")),
                ("field", Value::from("icon")),
                ("label", Value::from("icon")),
            ]),
            &"b".repeat(64),
            "image/png",
        ),
        asset(
            "assets/images/sprite.png",
            body("assets/images/sprite.png"),
            "image",
            style_context(2, "Costume sprite", "costume_sprite", "Costume sprite"),
            &a,
            "image/png",
        ),
        asset(
            "assets/images/other-portrait.png",
            body("assets/images/other-portrait.png"),
            "image",
            merged(
                &style_context(3, "Costume portrait", "costume_portrait", "Costume portrait"),
                &[
                    ("column_name", Value::from("Other Costume")),
                    ("column_category", Value::from("Attacker")),
                    ("costume_title", Value::from("Other Costume")),
                    ("costume_category", Value::from("Attacker")),
                ],
            ),
            &a,
            "image/png",
        ),
        asset(
            "assets/images/portrait.png",
            body("assets/images/portrait.png"),
            "image",
            style_context(3, "Costume portrait", "costume_portrait", "Costume portrait"),
            &a,
            "image/png",
        ),
        asset(
            "assets/images/full.png",
            body("assets/images/full.png"),
            "image",
            style_context(4, "Full costume portrait", "costume_full_portrait", "Full costume portrait"),
            &a,
            "image/png",
        ),
        asset(
            "assets/videos/skill.mp4",
            body("assets/videos/skill.mp4"),
            "video",
            style_context(8, "Skill animation", "skill_live2d_1", "Skill animation"),
            &a,
            "video/mp4",
        ),
        asset(
            "assets/audio/voice.mp3",
            body("assets/audio/voice.mp3"),
            "audio",
            merged(&style_context(16, "Voice", "", "Voice"), &[("field", Value::from(""))]),
            &a,
            "audio/mpeg",
        ),
        asset(
            "assets/live2d/model-a/model.atlas",
            body("assets/live2d/model-a/model.atlas"),
            "live2d_atlas",
            merged(&live2d_context, &[("live2d_field", Value::from("atlas"))]),
            &a,
            octet,
        ),
        asset(
            "assets/live2d/model-a/model.json",
            body("assets/live2d/model-a/model.json"),
            "live2d_json",
            merged(&live2d_context, &[("live2d_field", Value::from("json"))]),
            &a,
            octet,
        ),
        asset(
            "assets/live2d/model-a/model.png",
            body("assets/live2d/model-a/model.png"),
            "live2d_texture",
            merged(&live2d_context, &[("live2d_field", Value::from("atlas_texture"))]),
            &a,
            "image/png",
        ),
        asset(
            "assets/live2d/model-a/background.png",
            body("assets/live2d/model-a/background.png"),
            "live2d_background",
            merged(&live2d_context, &[("live2d_field", Value::from("bg"))]),
            &a,
            "image/png",
        ),
    ];
    let base_info = parse(
        r#"{"Rarity": [{"row_index": 1, "values": [{"type": "text", "value": "5-star"}]}],
            "Class": [{"row_index": 2, "values": [{"type": "text", "value": "Support"}]}]}"#,
    );
    let costume = parse(
        r#"{"style_index": 0, "style_name": "style-a", "title": "Office Worker", "category": "Support",
            "rows": [{"row_index": 0, "label": "Costume name", "cells": [{"type": "text", "value": "Office Worker"}]}]}"#,
    );
    let live2d_models = Value::List(vec![merged(
        &live2d_context,
        &[
            ("key", Value::from("bound-live2d")),
            ("stable_id", Value::from("stable-live2d")),
            ("animation", Value::from("idle")),
            ("skin", Value::from("office")),
            ("limit_age", Value::Bool(false)),
            ("source", Value::from("bd2_l2d_viewer")),
            ("variant", Value::from("censored")),
            ("supplement_reason", Value::from("censored_variant")),
            ("viewer_entry_id", Value::from("101101_c")),
            ("viewer_stem", Value::from("char101101_c")),
            ("source_page_url", Value::from("https://jelosus2.github.io/BD2-L2D-Viewer/")),
            ("position", parse(r#"{"pc": {"x": 1}}"#)),
            ("bg_position", parse(r#"{"pc": {"x": 2}}"#)),
            (
                "urls",
                parse(r#"{"atlas": "https://cdn.example.test/model.atlas", "bg": "https://cdn.example.test/background.png"}"#),
            ),
        ],
    )]);
    let tree_row = parse(r#"{"content_id": 101, "name": "Test BD2", "_bd2_group_name": "5-star"}"#);
    let manifest = obj(&[
        ("source_url", Value::from("https://www.gamekee.com/zsca2/tj/101.html")),
        ("detail_api_url", Value::from("https://www.gamekee.com/v1/content/detail/101")),
        ("fetched_at", Value::from("2026-05-01T00:00:00+00:00")),
        ("content_id", Value::from(101_i64)),
        ("title", Value::from("Test BD2")),
        ("updated_at", Value::from(123_i64)),
        ("tree_row", tree_row.clone()),
        ("base_info", base_info.clone()),
        ("content_summary", obj(&[("costumes", Value::List(vec![costume.clone()]))])),
        ("costumes", Value::List(vec![costume.clone()])),
        ("live2d_models", live2d_models.clone()),
        ("assets", Value::List(assets)),
        (
            "asset_counts",
            parse(r#"{"image": 5, "video": 1, "audio": 1, "live2d_atlas": 1, "live2d_texture": 1}"#),
        ),
    ]);
    let character = obj(&[
        ("content_id", Value::from(101_i64)),
        ("title", Value::from("Test BD2")),
        ("tree_row", tree_row),
        ("base_info", base_info),
        ("costumes", Value::List(vec![costume])),
        ("live2d_models", live2d_models),
    ]);
    write_json(&character_root.join("manifest.json"), &manifest);
    write_json(&character_root.join("character.json"), &character);
    write_json(
        &root.join("_blobs").join("manifest.json"),
        &parse(r#"{"content_id": 999, "title": "Ignored"}"#),
    );
}

async fn fixture_harness(root: &Path) -> Harness {
    create_fixture(root);
    let now = utc(2026, 5, 1, 0, 0, 0);
    harness(root, FakeStore::new(now, now)).await
}

fn auth() -> String {
    format!("Bearer {TOKEN}")
}

async fn get(harness: &Harness, target: &str) -> Answer {
    harness.request("GET", target, &[("Authorization", &auth())], None).await
}

async fn send(harness: &Harness, method: &str, target: &str, body: &str) -> Answer {
    harness
        .request(
            method,
            target,
            &[("Authorization", &auth()), ("content-type", "application/json")],
            Some(body),
        )
        .await
}

fn at<'a>(value: &'a Value, path: &[&str]) -> &'a Value {
    path.iter().fold(value, |value, key| match key.parse::<usize>() {
        Ok(index) => &golden::list(value)[index],
        Err(_) => golden::get(value, key),
    })
}

fn text_at(value: &Value, path: &[&str]) -> String {
    golden::string(at(value, path))
}

// py: tests/test_bd2_api.py::test_list_bd2_characters_returns_lightweight_summaries
#[tokio::test]
async fn list_bd2_characters_returns_lightweight_summaries() {
    let tmp = tempfile::tempdir().unwrap();
    let harness = fixture_harness(&tmp.path().join("bd2")).await;

    let answer = get(&harness, "/api/v2/bd2/characters?q=support&limit=1").await;
    assert_eq!(answer.status, 200);
    let payload = answer.json();
    assert_eq!(golden::int(golden::get(&payload, "total")), 1);
    let item = at(&payload, &["items", "0"]);
    assert_eq!(golden::int(golden::get(item, "content_id")), 101);
    assert_eq!(text_at(item, &["title"]), "Test BD2");
    assert_eq!(text_at(item, &["tags", "rarity_group"]), "5-star");
    assert_eq!(
        text_at(item, &["icon", "url"]),
        format!("{STATIC_CHARACTER_PREFIX}/assets/images/icon.png?v={}", "b".repeat(64))
    );
    assert_eq!(text_at(item, &["portrait", "path"]), "assets/images/portrait.png");
    assert_eq!(golden::int(golden::get(item, "costume_count")), 1);
    assert_eq!(golden::int(golden::get(item, "live2d_model_count")), 1);
    assert!(golden::list(golden::get(item, "search_terms")).contains(&Value::from("Office Worker")));

    let costume = get(&harness, "/api/v2/bd2/characters?q=office%20worker").await;
    assert_eq!(costume.status, 200);
    assert_eq!(golden::int(golden::get(&costume.json(), "total")), 1);
    harness.finish().await;
}

/// Overwrite a file with other bytes of the same length and keep its mtime, so its
/// signature entry does not change.
fn scramble_keeping_signature(path: &Path) {
    let modified = std::fs::metadata(path).unwrap().modified().unwrap();
    let length = std::fs::metadata(path).unwrap().len();
    std::fs::write(path, "x".repeat(usize::try_from(length).unwrap())).unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

// py: tests/test_bd2_api.py::test_bd2_library_loads_summary_from_disk_cache_without_rebuilding
#[test]
fn bd2_library_loads_summary_from_disk_cache_without_rebuilding() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bd2");
    create_fixture(&root);
    let expected = Bd2Library::new(&root).list_characters().unwrap();
    assert_eq!(expected.len(), 1);
    assert!(root.join("_api").join("summary-cache.json").is_file());
    // A rebuild would now find no readable manifest; the cache answers instead.
    scramble_keeping_signature(&root.join("101 - Test BD2").join("manifest.json"));
    // Python's `==` on dictionaries ignores their order; the cache file is sorted.
    let sorted = |summaries: Vec<Dict>| {
        pyjson::dumps(
            &Value::List(summaries.into_iter().map(Value::Dict).collect()),
            &pyjson::DumpOptions::hash(),
        )
        .unwrap()
    };
    assert_eq!(sorted(Bd2Library::new(&root).list_characters().unwrap()), sorted(expected));
}

// py: tests/test_bd2_api.py::test_list_bd2_sidebar_characters_returns_sorted_light_payload
#[tokio::test]
async fn list_bd2_sidebar_characters_returns_sorted_light_payload() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bd2");
    create_fixture(&root);
    let recent_root = root.join("202 - Recent BD2");
    write_asset(&recent_root.join("assets/images/recent-icon.png"), b"recent-icon");
    let mut recent = parse(
        r#"{"source_url": "https://www.gamekee.com/zsca2/tj/202.html", "fetched_at": "2026-05-01T00:00:01+00:00",
            "content_id": 202, "title": "Recent BD2", "updated_at": 999,
            "tree_row": {"content_id": 202, "_bd2_group_name": "4-star"}, "base_info": {}, "costumes": [],
            "live2d_models": []}"#,
    );
    recent = merged(
        &recent,
        &[(
            "assets",
            Value::List(vec![asset(
                "assets/images/recent-icon.png",
                b"recent-icon",
                "image",
                parse(r#"{"section": "entry_tree", "field": "icon", "label": "icon"}"#),
                &"c".repeat(64),
                "image/png",
            )]),
        )],
    );
    write_json(&recent_root.join("manifest.json"), &recent);
    let now = utc(2026, 5, 1, 0, 0, 0);
    let harness = harness(&root, FakeStore::new(now, now)).await;

    let answer = get(&harness, "/api/v2/bd2/sidebar/characters").await;
    let query = get(&harness, "/api/v2/bd2/sidebar/characters?q=test").await;
    let costume_query = get(&harness, "/api/v2/bd2/sidebar/characters?q=office%20worker").await;
    let etag = answer.header("etag").unwrap().to_owned();
    let cached = harness
        .request(
            "GET",
            "/api/v2/bd2/sidebar/characters",
            &[("Authorization", &auth()), ("If-None-Match", &etag)],
            None,
        )
        .await;

    assert_eq!(answer.status, 200);
    assert_eq!(answer.header("cache-control"), Some("public, max-age=300"));
    let payload = answer.json();
    assert_eq!(golden::int(golden::get(&payload, "total")), 2);
    let ids: Vec<i128> = golden::list(golden::get(&payload, "items"))
        .iter()
        .map(|item| golden::int(golden::get(item, "content_id")))
        .collect();
    assert_eq!(ids, [202, 101]);
    assert_eq!(
        at(&payload, &["items", "0", "icon"]),
        &obj(&[
            (
                "url",
                Value::Str(format!(
                    "/static/bd2/202%20-%20Recent%20BD2/assets/images/recent-icon.png?v={}",
                    "c".repeat(64)
                ))
            ),
            ("available", Value::Bool(true)),
            ("sha256", Value::Str("c".repeat(64))),
            ("content_type", Value::from("image/png")),
        ])
    );
    for filtered in [&query, &costume_query] {
        assert_eq!(filtered.status, 200);
        assert_eq!(golden::int(golden::get(&filtered.json(), "total")), 1);
        assert_eq!(golden::int(at(&filtered.json(), &["items", "0", "content_id"])), 101);
    }
    assert_eq!(cached.status, 304);
    assert!(cached.body.is_empty());
    assert_eq!(cached.header("etag"), Some(etag.as_str()));
    harness.finish().await;
}

// py: tests/test_bd2_api.py::test_get_bd2_character_returns_costumes_assets_and_live2d_refs
#[tokio::test]
async fn get_bd2_character_returns_costumes_assets_and_live2d_refs() {
    let tmp = tempfile::tempdir().unwrap();
    let harness = fixture_harness(&tmp.path().join("bd2")).await;

    let answer = get(&harness, "/api/v2/bd2/characters/101").await;
    assert_eq!(answer.status, 200);
    let payload = answer.json();
    assert_eq!(text_at(&payload, &["base_info", "Class", "0", "values", "0", "value"]), "Support");
    assert_eq!(golden::list(golden::get(&payload, "assets")).len(), 11);
    let costume = at(&payload, &["costumes", "0"]);
    assert_eq!(text_at(costume, &["title"]), "Office Worker");
    assert_eq!(text_at(costume, &["sprite", "path"]), "assets/images/sprite.png");
    assert_eq!(text_at(costume, &["portrait", "path"]), "assets/images/portrait.png");
    assert_eq!(text_at(costume, &["full_portrait", "path"]), "assets/images/full.png");
    assert!(
        golden::list(golden::get(costume, "gallery"))
            .iter()
            .any(|asset| text_at(asset, &["path"]) == "assets/images/other-portrait.png")
    );
    assert_eq!(text_at(costume, &["videos", "0", "path"]), "assets/videos/skill.mp4");
    assert_eq!(text_at(costume, &["audio", "0", "path"]), "assets/audio/voice.mp3");
    let model = at(costume, &["live2d_models", "0"]);
    for (key, expected) in [
        ("live2d_key", "model-a"),
        ("source", "bd2_l2d_viewer"),
        ("variant", "censored"),
        ("supplement_reason", "censored_variant"),
        ("viewer_entry_id", "101101_c"),
        ("viewer_stem", "char101101_c"),
        ("source_page_url", "https://jelosus2.github.io/BD2-L2D-Viewer/"),
        ("model_id", "stable-live2d"),
        ("spine_version", "4.0.64"),
    ] {
        assert_eq!(text_at(model, &[key]), expected, "{key}");
    }
    assert_eq!(golden::get(model, "view_overrides"), &Value::Dict(Dict::new()));
    assert_eq!(text_at(model, &["assets", "atlas", "path"]), "assets/live2d/model-a/model.atlas");
    assert_eq!(
        text_at(model, &["assets", "atlas", "url"]),
        format!("{STATIC_CHARACTER_PREFIX}/assets/live2d/model-a/model.atlas?v={}", "a".repeat(64))
    );
    assert_eq!(text_at(model, &["assets", "json", "path"]), "assets/live2d/model-a/model.json");
    assert_eq!(
        text_at(model, &["assets", "textures", "0", "path"]),
        "assets/live2d/model-a/model.png"
    );
    assert_eq!(
        text_at(model, &["assets", "background", "path"]),
        "assets/live2d/model-a/background.png"
    );
    harness.finish().await;
}

// py: tests/test_bd2_api.py::test_get_bd2_asset_endpoint_is_removed
#[tokio::test]
async fn get_bd2_asset_endpoint_is_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let harness = fixture_harness(&tmp.path().join("bd2")).await;
    let answer = get(&harness, "/api/v2/bd2/assets/101/assets/images/icon.png").await;
    assert_eq!(answer.status, 404);
    assert_eq!(answer.error_code(), "not_found");
    harness.finish().await;
}

const OVERRIDE: &str = "/api/v2/bd2/characters/101/live2d-models/stable-live2d/view-overrides/default";

// py: tests/test_bd2_api.py::test_bd2_live2d_view_override_crud_and_detail_overlay
#[tokio::test]
async fn bd2_live2d_view_override_crud_and_detail_overlay() {
    let tmp = tempfile::tempdir().unwrap();
    let harness = fixture_harness(&tmp.path().join("bd2")).await;

    let created = send(
        &harness,
        "PUT",
        OVERRIDE,
        r#"{"position": {"x": 120.5, "y": -32}, "scale": 1.25, "background_position": {"x": 0, "y": 4}, "background_scale": 1.1}"#,
    )
    .await;
    let fetched = get(&harness, OVERRIDE).await;
    let detail = get(&harness, "/api/v2/bd2/characters/101").await;
    let deleted = harness.request("DELETE", OVERRIDE, &[("Authorization", &auth())], None).await;
    let missing = get(&harness, OVERRIDE).await;

    assert_eq!(created.status, 200);
    assert_eq!(
        created.json(),
        parse(
            r#"{"position": {"x": 120.5, "y": -32.0}, "scale": 1.25, "background_position": {"x": 0.0, "y": 4.0},
                "background_scale": 1.1, "created_at": "2026-05-01T00:00:00Z", "updated_at": "2026-05-01T00:00:00Z",
                "source": "bd2", "content_id": 101, "model_id": "stable-live2d", "profile": "default"}"#
        )
    );
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.json(), created.json());
    assert_eq!(detail.status, 200);
    let model = at(&detail.json(), &["costumes", "0", "live2d_models", "0"]).clone();
    assert_eq!(text_at(&model, &["model_id"]), "stable-live2d");
    assert_eq!(
        at(&model, &["view_overrides", "default", "position"]),
        &parse(r#"{"x": 120.5, "y": -32.0}"#)
    );
    assert_eq!(at(&model, &["view_overrides", "default", "scale"]), &Value::Float(1.25));
    assert_eq!(deleted.status, 204);
    assert!(deleted.body.is_empty());
    assert_eq!(missing.status, 404);
    assert_eq!(missing.error_code(), "live2d_view_override_not_found");
    harness.finish().await;
}

// py: tests/test_bd2_api.py::test_bd2_live2d_view_override_rejects_unknown_model
#[tokio::test]
async fn bd2_live2d_view_override_rejects_unknown_model() {
    let tmp = tempfile::tempdir().unwrap();
    let harness = fixture_harness(&tmp.path().join("bd2")).await;
    let answer = send(
        &harness,
        "PUT",
        "/api/v2/bd2/characters/101/live2d-models/missing-model/view-overrides/default",
        r#"{"position": {"x": 1, "y": 2}, "scale": 1}"#,
    )
    .await;
    assert_eq!(answer.status, 404);
    assert_eq!(answer.error_code(), "bd2_live2d_model_not_found");
    harness.finish().await;
}

// py: tests/test_bd2_api.py::test_bd2_library_skips_symlinked_character_dirs_outside_root
#[test]
fn bd2_library_skips_symlinked_character_dirs_outside_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bd2");
    let external = tmp.path().join("external-character");
    write_json(
        &external.join("manifest.json"),
        &parse(r#"{"content_id": 303, "title": "External BD2"}"#),
    );
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&external, root.join("303 - External BD2")).unwrap();
    assert_eq!(Bd2Library::new(&root).list_characters().unwrap(), Vec::<Dict>::new());
}

// py: tests/test_bd2_api.py::test_bd2_library_prefers_newest_duplicate_content_id_manifest
#[test]
fn bd2_library_prefers_newest_duplicate_content_id_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bd2");
    write_json(
        &root.join("101 - Old BD2").join("manifest.json"),
        &parse(r#"{"content_id": 101, "title": "Old BD2", "fetched_at": "2026-05-01T00:00:00+00:00"}"#),
    );
    write_json(
        &root.join("101 - New BD2").join("manifest.json"),
        &parse(r#"{"content_id": 101, "title": "New BD2", "fetched_at": "2026-05-01T00:01:00+00:00"}"#),
    );
    let library = Bd2Library::new(&root);
    let titles: Vec<String> = library
        .list_characters()
        .unwrap()
        .iter()
        .map(|character| golden::string(character.get("title").unwrap()))
        .collect();
    assert_eq!(titles, ["New BD2"]);
    assert_eq!(golden::string(library.get_character(101).unwrap().get("title").unwrap()), "New BD2");
}

// py: tests/test_bd2.py::test_api_job_enum_includes_bd2
#[test]
fn api_job_enum_includes_bd2() {
    assert!(crate::handlers::JOB_REQUEST_TARGETS.contains(&"bd2"));
}

// ---------- the PostgreSQL store ----------

#[tokio::test]
async fn the_postgres_store_keeps_created_at_and_round_trips_positions() {
    let harness = Harness::new().await;
    let store = PostgresLive2DViewOverrideStore::new(harness.state.db.clone());
    let key = OverrideKey {
        source: "bd2".to_owned(),
        content_id: 101,
        model_id: "stable-live2d".to_owned(),
        profile: "default".to_owned(),
    };
    assert_eq!(store.get(&key).await.unwrap(), None);
    let first = store
        .upsert(
            &key,
            &OverrideView {
                position: (120.5, -32.0),
                scale: 1.25,
                background_position: Some((0.0, 4.0)),
                background_scale: Some(1.1),
            },
        )
        .await
        .unwrap();
    assert_eq!(first.position, parse(r#"{"x": 120.5, "y": -32.0}"#));
    assert_eq!(first.background_position, Some(parse(r#"{"x": 0.0, "y": 4.0}"#)));
    let second = store
        .upsert(
            &key,
            &OverrideView {
                position: (1.0, 2.0),
                scale: 2.0,
                background_position: None,
                background_scale: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(second.created_at, first.created_at);
    assert_eq!(second.background_position, None);
    let other = OverrideKey {
        profile: "mobile".to_owned(),
        ..key.clone()
    };
    store
        .upsert(
            &other,
            &OverrideView {
                position: (0.0, 0.0),
                scale: 0.5,
                background_position: None,
                background_scale: None,
            },
        )
        .await
        .unwrap();
    let listed: Vec<String> = store
        .list_for_character("bd2", 101)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.profile)
        .collect();
    assert_eq!(listed, ["default", "mobile"]);
    assert_eq!(store.get(&key).await.unwrap(), Some(second));
    assert!(store.delete(&key).await.unwrap());
    assert!(!store.delete(&key).await.unwrap());
    assert!(store.list_for_character("nikke", 101).await.unwrap().is_empty());
    harness.finish().await;
}
