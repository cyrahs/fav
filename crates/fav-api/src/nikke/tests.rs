use std::path::Path;

use fav_core::golden;
use fav_core::pyjson::{self, Value};
use serde_json::json;

use super::{NikkeLibrary, py_set_order, sort_date, sort_timestamp, sorted_skin_indexes};
use crate::tests::Harness;

const AUTH: (&str, &str) = ("Authorization", "Bearer token-for-tests");
/// The headers `805f5c0:rust/tools/goldens/areas/nikke.py` records.
const RECORDED_HEADERS: [&str; 3] = ["content-type", "cache-control", "etag"];

fn value(json: &serde_json::Value) -> Value {
    Value::from_serde(json)
}

/// Point `web.nikke.path` at `root`.
async fn configure(harness: &Harness, root: &Path) {
    let settings = value(&json!({"path": root.to_str().expect("UTF-8 path")}));
    harness.store_settings("web.nikke", &settings).await;
}

// ---------- the golden replay ----------

// The requests of `goldens/nikke/api.json` against the same files, through the
// router, answer what FastAPI answered.
#[tokio::test]
async fn api_replays_python() {
    let document = golden::load("nikke/api.json");
    for case in golden::cases(&document) {
        let name = golden::string(golden::get(case, "name"));
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("nikke");
        let files = golden::list(golden::get(case, "files"));
        if !files.is_empty() {
            std::fs::create_dir(&root).expect("root");
        }
        for file in files {
            let path = root.join(golden::string(golden::get(file, "path")));
            std::fs::create_dir_all(path.parent().expect("parent")).expect("directories");
            std::fs::write(&path, golden::string(golden::get(file, "text"))).expect("file");
        }
        let harness = Harness::new().await;
        configure(&harness, &root).await;
        let mut etag = String::new();
        for exchange in golden::list(golden::get(case, "exchanges")) {
            let request = golden::get(exchange, "request");
            let expected = golden::get(exchange, "response");
            let headers: Vec<(String, String)> = golden::get(request, "headers")
                .as_dict()
                .expect("headers")
                .iter()
                .map(|(key, header)| {
                    let header = golden::string(header);
                    (
                        fav_runtime::pystr::str_key(key),
                        if header == "$etag" { etag.clone() } else { header },
                    )
                })
                .collect();
            let header_refs: Vec<(&str, &str)> = headers.iter().map(|(key, header)| (key.as_str(), header.as_str())).collect();
            let method = golden::string(golden::get(request, "method"));
            let path = golden::string(golden::get(request, "path"));
            let answer = harness.request(&method, &path, &header_refs, None).await;
            let context = format!("[{name}] {method} {path} {headers:?}");
            if let Some(tag) = answer.header("etag") {
                tag.clone_into(&mut etag);
            }

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
        harness.finish().await;
    }
}

#[test]
fn skin_index_order_matches_python() {
    let document = golden::load("nikke/helpers.json");
    for case in golden::list(golden::get(&golden::cases(&document)[0], "skin_index_order")) {
        let models: Vec<Value> = golden::list(golden::get(case, "values"))
            .iter()
            .map(|skin_index| {
                let mut model = pyjson::Dict::new();
                model.insert("skin_index", skin_index.clone());
                Value::Dict(model)
            })
            .collect();
        let order: Vec<Value> = sorted_skin_indexes(&models)
            .into_iter()
            .map(|index| index.map_or(Value::Null, Value::Int))
            .collect();
        assert_eq!(&Value::List(order), golden::get(case, "order"), "{:?}", golden::get(case, "values"));
    }
}

#[test]
fn set_order_is_the_table_order() {
    // `None` hashes to 0xFCA86420, slot 0 of a table of 8, so it comes before 3 (slot 3)
    // whatever the insertion order; duplicates are dropped.
    assert_eq!(py_set_order([Some(3), Some(3), None, None]), vec![None, Some(3)]);
    // Five values fill a table of 8 past 3/5: it grows to 32 and the order follows the
    // new slots (8 and 16 no longer share slot 0).
    assert_eq!(
        py_set_order([Some(16), Some(8), Some(1), Some(2), Some(3)]),
        vec![Some(1), Some(2), Some(3), Some(8), Some(16)]
    );
}

#[test]
fn sidebar_sort_keys_match_python() {
    let document = golden::load("nikke/helpers.json");
    let keys = &golden::cases(&document)[1];
    for case in golden::list(golden::get(keys, "timestamps")) {
        let Value::Float(expected) = golden::get(case, "key") else {
            panic!("a float key: {case:?}");
        };
        let actual = sort_timestamp(golden::get(case, "value"));
        assert!(actual.to_bits() == expected.to_bits(), "{case:?}: {actual}");
    }
    for case in golden::list(golden::get(keys, "dates")) {
        assert_eq!(
            i128::from(sort_date(golden::get(case, "value"))),
            golden::int(golden::get(case, "key")),
            "{case:?}"
        );
    }
}

// py: tests/test_nikke.py::test_api_job_enum_includes_nikke
#[test]
fn api_job_enum_includes_nikke() {
    assert!(crate::handlers::JOB_REQUEST_TARGETS.contains(&"nikke"));
}

// ---------- tests/test_nikke_api.py ----------

const STATIC_CHARACTER_PREFIX: &str = "/static/nikke/101%20-%20Test%20NIKKE";

fn write_json(path: &Path, payload: &serde_json::Value) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("directories");
    std::fs::write(path, serde_json::to_string(payload).expect("JSON")).expect("write");
}

fn write_asset(path: &Path, body: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("directories");
    std::fs::write(path, body).expect("write");
}

/// `_asset`.
fn asset(path: &str, label: &str, body: &[u8], kind: &str, extra_context: &serde_json::Value) -> serde_json::Value {
    let row_index = match label {
        "时装图（切换）" => 2,
        "时装立绘" => 8,
        "SD模型" => 9,
        "爆裂动画" => 10,
        _ => 0,
    };
    let mut context = json!({"section": "style", "row_index": row_index, "label": label, "skin_index": 0});
    if let (Some(context), Some(extra)) = (context.as_object_mut(), extra_context.as_object()) {
        context.extend(extra.clone());
    }
    let name = Path::new(path).file_name().expect("name").to_string_lossy().into_owned();
    json!({
        "url": format!("https://cdn.example.test/{name}"),
        "kind": kind,
        "local_path": path,
        "sha256": "a".repeat(64),
        "content_type": if kind == "image" { "image/png" } else { "application/octet-stream" },
        "size": body.len(),
        "status": "downloaded",
        "error": "",
        "contexts": [context],
    })
}

/// `_runtime_animations`.
fn runtime_animations(prefix: &str) -> serde_json::Value {
    json!({
        "idle": {
            "animation": format!("{prefix}-idle"),
            "enabled": true,
            "loop": true,
            "match_method": "gamekee-runtime-player",
            "match_confidence": "high",
        },
        "click": {
            "animation": format!("{prefix}-click"),
            "enabled": true,
            "loop": false,
            "match_method": "gamekee-runtime-player-event",
            "match_confidence": "high",
            "duration_ms": 1200,
        },
    })
}

/// `_create_nikke_fixture`: one character with images, a Live2D model and a voice line.
#[allow(clippy::too_many_lines)] // The Python fixture, field for field.
fn create_nikke_fixture(root: &Path) {
    let character_root = root.join("101 - Test NIKKE");
    let files: [(&str, &[u8]); 8] = [
        ("assets/images/icon.png", b"icon"),
        ("assets/images/thumb.webp", b"thumb"),
        ("assets/images/portrait.png", b"portrait"),
        ("assets/images/sd.gif", b"sd"),
        ("assets/images/burst.gif", b"burst"),
        ("assets/live2d/model-a/model.atlas", b"atlas"),
        ("assets/live2d/model-a/model.skel", b"hash 4.1.24 skel"),
        ("assets/live2d/model-a/model.png", b"texture"),
    ];
    for (relative_path, body) in files {
        write_asset(&character_root.join(relative_path), body);
    }
    let body = |path: &str| files.iter().find(|(name, _)| *name == path).expect("a fixture file").1;
    let none = json!({});
    let live2d = |path: &str, kind: &str, field: &str| {
        asset(
            path,
            "live2d（full）",
            body(path),
            kind,
            &json!({"live2d_key": "model-a", "live2d_field": field}),
        )
    };
    let assets = json!([
        {
            "url": "https://cdn.example.test/icon.png",
            "kind": "image",
            "local_path": "assets/images/icon.png",
            "sha256": "b".repeat(64),
            "content_type": "image/png",
            "size": 4,
            "status": "downloaded",
            "error": "",
            "contexts": [
                {"section": "tj_list", "field": "icon", "label": "icon"},
                {"section": "base", "row_index": 15, "label": "部队成员1"},
            ],
        },
        asset("assets/images/thumb.webp", "时装图（切换）", body("assets/images/thumb.webp"), "image", &none),
        asset("assets/images/portrait.png", "时装立绘", body("assets/images/portrait.png"), "image", &none),
        asset("assets/images/sd.gif", "SD模型", body("assets/images/sd.gif"), "image", &none),
        asset("assets/images/burst.gif", "爆裂动画", body("assets/images/burst.gif"), "image", &none),
        live2d("assets/live2d/model-a/model.atlas", "live2d_atlas", "atlas"),
        live2d("assets/live2d/model-a/model.skel", "live2d_skel", "skel"),
        live2d("assets/live2d/model-a/model.png", "live2d_texture", "atlas_texture"),
    ]);
    let text = |row_index: i64, text: &str| json!([{"row_index": row_index, "values": [{"type": "text", "value": text}]}]);
    let base_info = json!({
        "稀有度": text(1, "SSR"),
        "企业": text(5, "Elysion"),
        "属性": text(8, "Fire"),
        "职业": text(9, "Attacker"),
        "武器": text(10, "Rifle"),
        "CV": text(2, "Test CV"),
        "实装日期": text(20, "2024/01/01"),
    });
    let skins = json!([{
        "skin_index": 0,
        "name": "Skin 1",
        "title": "Default",
        "series": "/",
        "obtain": "/",
        "is_collection_skin": false,
        "rows": [
            {"row_index": 0, "label": "时装名称", "cells": [{"type": "text", "value": "Default"}]},
            {
                "row_index": 12,
                "label": "Greeting",
                "cells": [
                    {"type": "text", "value": "Greeting"},
                    {"type": "text", "value": "Hello, Commander."},
                    {"type": "audio", "value": "//cdn.example.test/voice.mp3"},
                ],
            },
        ],
    }]);
    let live2d_models = json!([{
        "label": "live2d（full）",
        "section": "style",
        "row_index": 5,
        "skin_index": 0,
        "skin_name": "Skin 1",
        "skin_title": "Default",
        "live2d_key": "model-a",
        "position": {"pc": {"large": {"x": 1}}},
        "layer_order": 2,
        "source_z_index": 9,
        "source_layer_index": 0,
        "is_primary": true,
        "layer_match_method": "gamekee-player-init",
        "layer_match_confidence": "high",
        "urls": {"atlas": "https://cdn.example.test/model.atlas"},
    }]);
    let tj_list = json!({"content_id": 101, "name": "Test NIKKE", "level": "SSR", "qy": "Elysion"});
    let layer_capture = json!({
        "status": "skipped",
        "reason": "no_multi_full_groups",
        "attempted_at": "2026-04-30T00:00:01+00:00",
        "retryable": false,
    });
    let manifest = json!({
        "source_url": "https://www.gamekee.com/nikke/tj/101.html",
        "fetched_at": "2026-04-30T00:00:00+00:00",
        "content_id": 101,
        "title": "Test NIKKE",
        "updated_at": 123,
        "tj_list": tj_list,
        "base_info": base_info,
        "content_summary": {"skins": skins},
        "live2d_models": live2d_models,
        "assets": assets,
        "asset_counts": {"image": 5, "live2d_atlas": 1, "live2d_skel": 1, "live2d_texture": 1},
        "live2d_layer_capture": layer_capture,
    });
    let character = json!({
        "content_id": 101,
        "title": "Test NIKKE",
        "tj_list": tj_list,
        "base_info": base_info,
        "skins": skins,
        "live2d_models": live2d_models,
        "live2d_layer_capture": layer_capture,
    });
    write_json(&character_root.join("manifest.json"), &manifest);
    write_json(&character_root.join("character.json"), &character);
    write_json(
        &root.join("_blobs").join("manifest.json"),
        &json!({"content_id": 999, "title": "Ignored"}),
    );
}

async fn get(harness: &Harness, path: &str) -> crate::tests::Answer {
    harness.request("GET", path, &[AUTH], None).await
}

fn field<'a>(value: &'a Value, path: &[&str]) -> &'a Value {
    path.iter().fold(value, |value, key| match key.parse::<usize>() {
        Ok(index) => &golden::list(value)[index],
        Err(_) => golden::get(value, key),
    })
}

fn text_at(value: &Value, path: &[&str]) -> String {
    golden::string(field(value, path))
}

// py: tests/test_nikke_api.py::test_list_nikke_characters_returns_lightweight_summaries
#[tokio::test]
async fn list_nikke_characters_returns_lightweight_summaries() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("nikke");
    create_nikke_fixture(&root);
    let harness = Harness::new().await;
    configure(&harness, &root).await;

    let answer = get(&harness, "/api/v2/nikke/characters?q=elysion&limit=1").await;

    assert_eq!(answer.status, 200);
    let payload = answer.json();
    assert_eq!(field(&payload, &["total"]), &Value::Int(1));
    assert_eq!(field(&payload, &["limit"]), &Value::Int(1));
    assert_eq!(field(&payload, &["offset"]), &Value::Int(0));
    let item = field(&payload, &["items", "0"]);
    assert_eq!(field(item, &["content_id"]), &Value::Int(101));
    assert_eq!(text_at(item, &["title"]), "Test NIKKE");
    assert_eq!(text_at(item, &["tags", "rarity"]), "SSR");
    assert_eq!(text_at(item, &["tags", "company"]), "Elysion");
    assert_eq!(
        text_at(item, &["icon", "url"]),
        format!("{STATIC_CHARACTER_PREFIX}/assets/images/icon.png?v={}", "b".repeat(64))
    );
    assert_eq!(text_at(item, &["portrait", "path"]), "assets/images/portrait.png");
    assert_eq!(field(item, &["skin_count"]), &Value::Int(1));
    assert_eq!(field(item, &["live2d_model_count"]), &Value::Int(1));
    harness.finish().await;
}

// py: tests/test_nikke_api.py::test_nikke_library_loads_summary_from_disk_cache_without_rebuilding
#[test]
fn nikke_library_loads_summary_from_disk_cache_without_rebuilding() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("nikke");
    create_nikke_fixture(&root);
    let path = fav_core::validate::PyPath::new(root.to_str().expect("UTF-8 path"));
    let expected = NikkeLibrary::new(path.clone()).list_characters();
    let cache_path = root.join("_api").join("summary-cache.json");
    assert!(cache_path.is_file());

    // Python fails any manifest read; here the cached title is changed, so a summary
    // rebuilt from the manifest would differ from the one a fresh library returns.
    let cache = std::fs::read_to_string(&cache_path).expect("cache");
    assert!(cache.contains(r#""title":"Test NIKKE""#));
    std::fs::write(&cache_path, cache.replace(r#""title":"Test NIKKE""#, r#""title":"From Cache""#)).expect("cache");
    let cached = NikkeLibrary::new(path).list_characters();

    assert_eq!(cached.len(), expected.len());
    assert_eq!(cached[0].get("title"), Some(&Value::from("From Cache")));
    let mut restored = cached[0].clone();
    restored.insert("title", Value::from("Test NIKKE"));
    assert!(fav_sources::nikke_layer_metadata::py_eq(
        &Value::Dict(restored),
        &Value::Dict(expected[0].clone())
    ));
}

// py: tests/test_nikke_api.py::test_list_nikke_sidebar_characters_returns_sorted_light_payload
#[tokio::test]
async fn list_nikke_sidebar_characters_returns_sorted_light_payload() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("nikke");
    create_nikke_fixture(&root);
    let recent_root = root.join("202 - Recent NIKKE");
    let recent_icon_path = "assets/images/recent-icon.png";
    write_asset(&recent_root.join(recent_icon_path), b"recent-icon");
    write_json(
        &recent_root.join("manifest.json"),
        &json!({
            "source_url": "https://www.gamekee.com/nikke/tj/202.html",
            "fetched_at": "2026-04-30T00:00:01+00:00",
            "content_id": 202,
            "title": "Recent NIKKE",
            "updated_at": 999,
            "tj_list": {"content_id": 202, "name": "Recent NIKKE", "level": "SSR", "qy": "Missilis"},
            "base_info": {
                "稀有度": [{"row_index": 1, "values": [{"type": "text", "value": "SSR"}]}],
                "企业": [{"row_index": 5, "values": [{"type": "text", "value": "Missilis"}]}],
                "实装日期": [{"row_index": 20, "values": [{"type": "text", "value": "2025/02/03"}]}],
            },
            "content_summary": {"skins": []},
            "live2d_models": [],
            "assets": [{
                "url": "https://cdn.example.test/recent-icon.png",
                "kind": "image",
                "local_path": recent_icon_path,
                "sha256": "c".repeat(64),
                "content_type": "image/png",
                "size": 11,
                "status": "downloaded",
                "error": "",
                "contexts": [{"section": "tj_list", "field": "icon", "label": "icon"}],
            }],
        }),
    );
    let harness = Harness::new().await;
    configure(&harness, &root).await;

    let answer = get(&harness, "/api/v2/nikke/sidebar/characters").await;
    let query_answer = get(&harness, "/api/v2/nikke/sidebar/characters?q=elysion").await;
    let etag = answer.header("etag").expect("an ETag").to_owned();
    let cached_answer = harness
        .request("GET", "/api/v2/nikke/sidebar/characters", &[AUTH, ("If-None-Match", &etag)], None)
        .await;

    assert_eq!(answer.status, 200);
    assert_eq!(answer.header("cache-control"), Some("public, max-age=300"));
    assert!(etag.starts_with('"'));
    let payload = answer.json();
    let keys = |value: &Value| -> Vec<String> {
        value
            .as_dict()
            .expect("an object")
            .iter()
            .map(|(key, _)| fav_runtime::pystr::str_key(key))
            .collect()
    };
    assert_eq!(keys(&payload), ["items", "total"]);
    assert_eq!(field(&payload, &["total"]), &Value::Int(2));
    let ids: Vec<i128> = golden::list(field(&payload, &["items"]))
        .iter()
        .map(|item| golden::int(field(item, &["content_id"])))
        .collect();
    assert_eq!(ids, [202, 101]);
    let item = field(&payload, &["items", "0"]);
    assert_eq!(
        keys(item),
        ["content_id", "title", "icon", "implemented_at", "updated_at", "fetched_at"]
    );
    assert_eq!(text_at(item, &["title"]), "Recent NIKKE");
    assert_eq!(text_at(item, &["implemented_at"]), "2025/02/03");
    assert_eq!(field(item, &["updated_at"]), &Value::Int(999));
    assert_eq!(text_at(item, &["fetched_at"]), "2026-04-30T00:00:01+00:00");
    assert_eq!(
        field(item, &["icon"]),
        &value(&json!({
            "url": format!("/static/nikke/202%20-%20Recent%20NIKKE/assets/images/recent-icon.png?v={}", "c".repeat(64)),
            "available": true,
            "sha256": "c".repeat(64),
            "content_type": "image/png",
        }))
    );
    assert_eq!(query_answer.status, 200);
    assert_eq!(field(&query_answer.json(), &["total"]), &Value::Int(1));
    assert_eq!(field(&query_answer.json(), &["items", "0", "content_id"]), &Value::Int(101));
    assert_eq!(cached_answer.status, 304);
    assert!(cached_answer.body.is_empty());
    assert_eq!(cached_answer.header("cache-control"), Some("public, max-age=300"));
    assert_eq!(cached_answer.header("etag"), Some(etag.as_str()));
    harness.finish().await;
}

// py: tests/test_nikke_api.py::test_get_nikke_character_returns_skins_assets_and_live2d_refs
#[tokio::test]
async fn get_nikke_character_returns_skins_assets_and_live2d_refs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("nikke");
    create_nikke_fixture(&root);
    let harness = Harness::new().await;
    configure(&harness, &root).await;

    let answer = get(&harness, "/api/v2/nikke/characters/101").await;

    assert_eq!(answer.status, 200);
    let payload = answer.json();
    assert_eq!(text_at(&payload, &["base_info", "CV", "0", "values", "0", "value"]), "Test CV");
    assert_eq!(golden::list(field(&payload, &["assets"])).len(), 8);
    let empty = Value::List(Vec::new());
    for scope in [&payload, field(&payload, &["skins", "0"])] {
        assert_eq!(field(scope, &["layer_metadata_required"]), &Value::Bool(false));
        assert_eq!(text_at(scope, &["layer_metadata_status"]), "complete");
        assert_eq!(field(scope, &["layer_metadata_issues"]), &empty);
        assert_eq!(field(scope, &["runtime_animation_metadata_required"]), &Value::Bool(false));
        assert_eq!(text_at(scope, &["runtime_animation_metadata_status"]), "complete");
        assert_eq!(field(scope, &["runtime_animation_metadata_issues"]), &empty);
    }
    assert_eq!(text_at(&payload, &["layer_metadata_capture", "status"]), "skipped");
    assert_eq!(text_at(&payload, &["layer_metadata_capture", "reason"]), "no_multi_full_groups");
    let skin = field(&payload, &["skins", "0"]);
    assert_eq!(text_at(skin, &["title"]), "Default");
    assert_eq!(
        field(skin, &["layer_metadata_capture"]),
        field(&payload, &["layer_metadata_capture"])
    );
    assert_eq!(text_at(skin, &["thumbnail", "path"]), "assets/images/thumb.webp");
    assert_eq!(text_at(skin, &["sd_model", "path"]), "assets/images/sd.gif");
    assert_eq!(text_at(skin, &["burst_animation", "path"]), "assets/images/burst.gif");
    let gallery: Vec<String> = golden::list(field(skin, &["gallery"]))
        .iter()
        .map(|asset| text_at(asset, &["label"]))
        .collect();
    assert_eq!(gallery, ["时装图（切换）", "时装立绘", "SD模型", "爆裂动画"]);
    assert_eq!(
        field(skin, &["voice_lines"]),
        &value(&json!([{"label": "Greeting", "text": "Hello, Commander.", "source_url": "https://cdn.example.test/voice.mp3"}]))
    );
    let model = field(skin, &["live2d_models", "0"]);
    assert_eq!(text_at(model, &["live2d_key"]), "model-a");
    assert_eq!(text_at(model, &["model_id"]), "model-a");
    assert_eq!(field(model, &["layer_order"]), &Value::Int(2));
    assert_eq!(field(model, &["source_z_index"]), &Value::Int(9));
    assert_eq!(field(model, &["source_layer_index"]), &Value::Int(0));
    assert_eq!(field(model, &["is_primary"]), &Value::Bool(true));
    assert_eq!(text_at(model, &["layer_match_method"]), "gamekee-player-init");
    assert_eq!(text_at(model, &["layer_match_confidence"]), "high");
    assert_eq!(field(model, &["runtime_animations"]), &Value::Null);
    assert_eq!(field(model, &["view_overrides"]), &value(&json!({})));
    assert_eq!(text_at(model, &["assets", "atlas", "path"]), "assets/live2d/model-a/model.atlas");
    assert_eq!(
        text_at(model, &["assets", "atlas", "url"]),
        format!("{STATIC_CHARACTER_PREFIX}/assets/live2d/model-a/model.atlas?v={}", "a".repeat(64))
    );
    assert_eq!(text_at(model, &["assets", "skel", "path"]), "assets/live2d/model-a/model.skel");
    assert_eq!(
        text_at(model, &["assets", "textures", "0", "path"]),
        "assets/live2d/model-a/model.png"
    );
    assert_eq!(text_at(model, &["spine_version"]), "4.1.24");
    harness.finish().await;
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("JSON")
}

fn issue_codes(issues: &Value) -> std::collections::BTreeSet<String> {
    golden::list(issues).iter().map(|issue| text_at(issue, &["code"])).collect()
}

// py: tests/test_nikke_api.py::test_get_nikke_character_exposes_incomplete_layer_metadata_status
#[tokio::test]
#[allow(clippy::too_many_lines)] // One Python test, its setup and assertions kept together.
async fn get_nikke_character_exposes_incomplete_layer_metadata_status() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("nikke");
    create_nikke_fixture(&root);
    let character_root = root.join("101 - Test NIKKE");
    let mut manifest = read_json(&character_root.join("manifest.json"));
    let mut character = read_json(&character_root.join("character.json"));
    let first = manifest["live2d_models"][0].clone();
    let with = |base: &serde_json::Value, changes: serde_json::Value| {
        let mut model = base.clone();
        model
            .as_object_mut()
            .expect("a model")
            .extend(changes.as_object().expect("changes").clone());
        model
    };
    let mut secondary_model = with(
        &first,
        json!({
            "key": "model-b",
            "stable_id": "stable-model-b",
            "live2d_key": "model-b",
            "source_z_index": 8,
            "source_layer_index": 1,
            "is_primary": false,
            "layer_match_confidence": "low",
        }),
    );
    secondary_model.as_object_mut().expect("a model").remove("layer_order");
    let skin_one = with(
        &character["skins"][0],
        json!({"skin_index": 1, "name": "Skin 2", "title": "Complete Skin"}),
    );
    let skin_model = |key: &str, order: i64, z_index: i64, layer_index: i64, primary: bool| {
        with(
            &first,
            json!({
                "skin_index": 1,
                "skin_name": "Skin 2",
                "skin_title": "Complete Skin",
                "key": key,
                "stable_id": key,
                "live2d_key": key,
                "layer_order": order,
                "source_z_index": z_index,
                "source_layer_index": layer_index,
                "is_primary": primary,
                "layer_match_confidence": "high",
            }),
        )
    };
    let live2d_models = json!([
        first,
        secondary_model,
        skin_model("skin-one-main", 2, 9, 0, true),
        skin_model("skin-one-back", 1, 8, 1, false),
    ]);
    let layer_capture = json!({
        "status": "incomplete",
        "captured_at": "2026-04-30T00:00:02+00:00",
        "attempted_at": "2026-04-30T00:00:03+00:00",
        "capture_hash": "capture-hash",
        "fingerprint": "fingerprint",
        "warnings": ["safe warning"],
        "quality_issues": [{
            "severity": "error",
            "code": "raw_quality_issue",
            "model_kind": "full",
            "models": ["stable-model-b/model-b"],
            "raw_container": {"secret": true},
        }],
    });
    manifest["live2d_models"] = live2d_models.clone();
    manifest["live2d_layer_capture"] = layer_capture.clone();
    manifest["content_summary"]["skins"]
        .as_array_mut()
        .expect("skins")
        .push(skin_one.clone());
    character["live2d_models"] = live2d_models;
    character["live2d_layer_capture"] = layer_capture;
    character["skins"].as_array_mut().expect("skins").push(skin_one);
    write_json(&character_root.join("manifest.json"), &manifest);
    write_json(&character_root.join("character.json"), &character);
    let harness = Harness::new().await;
    configure(&harness, &root).await;

    let answer = get(&harness, "/api/v2/nikke/characters/101").await;

    assert_eq!(answer.status, 200);
    let payload = answer.json();
    let skin = field(&payload, &["skins", "0"]);
    let complete_skin = field(&payload, &["skins", "1"]);
    for scope in [&payload, skin] {
        assert_eq!(field(scope, &["layer_metadata_required"]), &Value::Bool(true));
        assert_eq!(text_at(scope, &["layer_metadata_status"]), "incomplete");
        assert_eq!(field(scope, &["runtime_animation_metadata_required"]), &Value::Bool(true));
        assert_eq!(text_at(scope, &["runtime_animation_metadata_status"]), "missing");
    }
    assert_eq!(
        issue_codes(field(&payload, &["runtime_animation_metadata_issues"])),
        ["capture_schema_mismatch".to_owned()].into()
    );
    let codes = issue_codes(field(skin, &["layer_metadata_issues"]));
    for code in ["raw_quality_issue", "missing_layer_order", "low_confidence_layer_match"] {
        assert!(codes.contains(code), "{code}");
    }
    let raw_issue = golden::list(field(skin, &["layer_metadata_issues"]))
        .iter()
        .find(|issue| text_at(issue, &["code"]) == "raw_quality_issue")
        .expect("the raw issue");
    assert_eq!(field(raw_issue, &["skin_index"]), &Value::Int(0));
    for issues in [field(skin, &["layer_metadata_issues"]), field(&payload, &["layer_metadata_issues"])] {
        assert!(golden::list(issues).iter().all(|issue| issue.get("raw_container").is_none()));
    }
    assert!(codes.is_subset(&issue_codes(field(&payload, &["layer_metadata_issues"]))));
    assert_eq!(field(complete_skin, &["layer_metadata_required"]), &Value::Bool(true));
    assert_eq!(text_at(complete_skin, &["layer_metadata_status"]), "incomplete");
    assert_eq!(
        issue_codes(field(complete_skin, &["layer_metadata_issues"])),
        ["raw_quality_issue".to_owned()].into()
    );
    assert_eq!(field(complete_skin, &["layer_metadata_issues", "0", "skin_index"]), &Value::Int(1));
    assert_eq!(
        field(skin, &["layer_metadata_capture"]),
        &value(&json!({
            "status": "incomplete",
            "captured_at": "2026-04-30T00:00:02+00:00",
            "attempted_at": "2026-04-30T00:00:03+00:00",
            "capture_hash": "capture-hash",
            "fingerprint": "fingerprint",
            "reason": "",
            "error_class": "",
            "retryable": null,
            "warnings": ["safe warning"],
        }))
    );
    assert_eq!(
        field(&payload, &["layer_metadata_capture"]),
        field(skin, &["layer_metadata_capture"])
    );
    harness.finish().await;
}

// py: tests/test_nikke_api.py::test_get_nikke_character_exposes_runtime_animation_metadata_separately_from_layer_status
#[tokio::test]
#[allow(clippy::too_many_lines)] // One Python test, its setup and assertions kept together.
async fn get_nikke_character_exposes_runtime_animation_metadata_separately_from_layer_status() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("nikke");
    create_nikke_fixture(&root);
    let character_root = root.join("101 - Test NIKKE");
    let mut manifest = read_json(&character_root.join("manifest.json"));
    let mut character = read_json(&character_root.join("character.json"));
    let mut live2d_models = Vec::new();
    for (stable_id, live2d_key, order, z_index, layer_index, primary) in [
        ("stable-main", "main", 2, 9, 0, true),
        ("stable-front", "front", 3, 10, 1, false),
        ("stable-back", "back", 1, 8, 2, false),
    ] {
        let mut animations = runtime_animations(live2d_key);
        if live2d_key == "front" {
            animations["click"]["match_confidence"] = json!("low");
        }
        let mut model = manifest["live2d_models"][0].clone();
        model.as_object_mut().expect("a model").extend(
            json!({
                "key": live2d_key,
                "stable_id": stable_id,
                "live2d_key": live2d_key,
                "layer_order": order,
                "source_z_index": z_index,
                "source_layer_index": layer_index,
                "is_primary": primary,
                "layer_match_confidence": "high",
                "runtime_animations": animations,
                "runtime_animation_match_method": "gamekee-runtime-player",
                "runtime_animation_match_confidence": "high",
            })
            .as_object()
            .expect("changes")
            .clone(),
        );
        live2d_models.push(model);
    }
    let capture = json!({
        "schema": 2,
        "status": "success",
        "captured_at": "2026-04-30T00:00:02+00:00",
        "fingerprint": "fingerprint",
        "warnings": [],
    });
    manifest["live2d_models"] = json!(live2d_models);
    manifest["live2d_layer_capture"] = capture.clone();
    character["live2d_models"] = json!(live2d_models);
    character["live2d_layer_capture"] = capture;
    write_json(&character_root.join("manifest.json"), &manifest);
    write_json(&character_root.join("character.json"), &character);
    let harness = Harness::new().await;
    configure(&harness, &root).await;

    let answer = get(&harness, "/api/v2/nikke/characters/101").await;

    assert_eq!(answer.status, 200);
    let payload = answer.json();
    let skin = field(&payload, &["skins", "0"]);
    assert_eq!(field(&payload, &["layer_metadata_required"]), &Value::Bool(true));
    assert_eq!(text_at(&payload, &["layer_metadata_status"]), "complete");
    assert_eq!(field(&payload, &["runtime_animation_metadata_required"]), &Value::Bool(true));
    assert_eq!(text_at(&payload, &["runtime_animation_metadata_status"]), "incomplete");
    assert_eq!(text_at(skin, &["layer_metadata_status"]), "complete");
    assert_eq!(text_at(skin, &["runtime_animation_metadata_status"]), "incomplete");
    assert_eq!(
        field(&payload, &["runtime_animation_metadata_issues"]),
        &value(&json!([{
            "severity": "error",
            "code": "low_runtime_animation_match",
            "skin_index": 0,
            "model_kind": "full",
            "state": "click",
            "models": ["stable-front/front"],
            "runtime_animation_match_confidence": "low",
        }]))
    );
    let model = |key: &str| {
        golden::list(field(skin, &["live2d_models"]))
            .iter()
            .find(|model| text_at(model, &["live2d_key"]) == key)
            .expect("the model")
            .clone()
    };
    assert_eq!(
        field(&model("main"), &["runtime_animations"]),
        &value(&json!({
            "idle": {
                "animation": "main-idle",
                "enabled": true,
                "loop": true,
                "match_method": "gamekee-runtime-player",
                "match_confidence": "high",
                "duration_ms": null,
            },
            "click": {
                "animation": "main-click",
                "enabled": true,
                "loop": false,
                "match_method": "gamekee-runtime-player-event",
                "match_confidence": "high",
                "duration_ms": 1200,
            },
        }))
    );
    assert_eq!(
        text_at(&model("main"), &["runtime_animation_match_method"]),
        "gamekee-runtime-player"
    );
    assert_eq!(text_at(&model("main"), &["runtime_animation_match_confidence"]), "high");
    assert_eq!(text_at(&model("front"), &["runtime_animation_match_confidence"]), "high");
    harness.finish().await;
}

// py: tests/test_nikke_api.py::test_nikke_live2d_view_override_save_and_detail_overlay
#[tokio::test]
async fn nikke_live2d_view_override_save_and_detail_overlay() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("nikke");
    create_nikke_fixture(&root);
    let mut harness = Harness::new().await;
    configure(&harness, &root).await;
    let stamp = chrono::DateTime::parse_from_rfc3339("2026-04-30T00:00:00Z")
        .expect("timestamp")
        .with_timezone(&chrono::Utc);
    harness.state.live2d_overrides = std::sync::Arc::new(crate::bd2::tests::FakeStore::new(stamp, stamp));
    let target = "/api/v2/nikke/characters/101/live2d-models/model-a/view-overrides/mobile";
    let json_type = ("Content-Type", "application/json");

    let save = harness
        .request(
            "PUT",
            target,
            &[AUTH, json_type],
            Some(r#"{"position": {"x": -10, "y": 22.5}, "scale": 0.85}"#),
        )
        .await;
    let detail = get(&harness, "/api/v2/nikke/characters/101").await;
    let invalid = harness
        .request(
            "PUT",
            target,
            &[AUTH, json_type],
            Some(r#"{"position": {"x": 0, "y": 0}, "scale": 0}"#),
        )
        .await;

    assert_eq!(save.status, 200, "{}", save.text());
    let sorted = |value: &Value| fav_core::pyjson::dumps(value, &fav_core::pyjson::DumpOptions::hash()).expect("dump");
    assert_eq!(
        sorted(&save.json()),
        sorted(&value(&json!({
            "source": "nikke",
            "content_id": 101,
            "model_id": "model-a",
            "profile": "mobile",
            "position": {"x": -10.0, "y": 22.5},
            "scale": 0.85,
            "background_position": null,
            "background_scale": null,
            "created_at": "2026-04-30T00:00:00Z",
            "updated_at": "2026-04-30T00:00:00Z",
        })))
    );
    assert_eq!(detail.status, 200, "{}", detail.text());
    let payload = detail.json();
    let model = field(&payload, &["skins", "0", "live2d_models", "0"]);
    assert_eq!(text_at(model, &["model_id"]), "model-a");
    assert_eq!(
        field(model, &["view_overrides", "mobile", "position"]),
        &value(&json!({"x": -10.0, "y": 22.5}))
    );
    assert_eq!(field(model, &["view_overrides", "mobile", "scale"]), &Value::Float(0.85));
    assert_eq!(invalid.status, 422);
    assert_eq!(text_at(&invalid.json(), &["error", "code"]), "validation_error");
    harness.finish().await;
}

// py: tests/test_nikke_api.py::test_get_nikke_asset_endpoint_is_removed
#[tokio::test]
async fn get_nikke_asset_endpoint_is_removed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("nikke");
    create_nikke_fixture(&root);
    let harness = Harness::new().await;
    configure(&harness, &root).await;

    let answer = get(&harness, "/api/v2/nikke/assets/101/assets/images/icon.png").await;

    assert_eq!(answer.status, 404);
    assert_eq!(answer.error_code(), "not_found");
    harness.finish().await;
}

/// D86: the collection root is read from the settings on each request, so a new path
/// applies without a restart.
#[tokio::test]
async fn a_changed_collection_path_applies_at_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let first = tmp.path().join("first");
    create_nikke_fixture(&first);
    let harness = Harness::new().await;
    configure(&harness, &first).await;
    assert_eq!(
        field(&get(&harness, "/api/v2/nikke/characters").await.json(), &["total"]),
        &Value::Int(1)
    );

    configure(&harness, &tmp.path().join("missing")).await;
    assert_eq!(
        field(&get(&harness, "/api/v2/nikke/characters").await.json(), &["total"]),
        &Value::Int(0)
    );
    assert_eq!(get(&harness, "/api/v2/nikke/characters/101").await.status, 404);
    harness.finish().await;
}
