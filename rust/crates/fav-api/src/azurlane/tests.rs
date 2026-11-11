//! The Azur Lane routes and library against Python.
//!
//! `rust/goldens/azurlane/api.json` replays requests through the real FastAPI app and
//! `AzurLaneLibrary` over a fixture tree (its files are in the golden); the replay here
//! builds the same tree. The tests after it port `tests/test_azurlane_api.py`.

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use fav_core::golden;
use fav_core::pyjson::{self, Value};
use serde_json::json;

use super::AzurLaneLibrary;
use crate::tests::{Answer, Harness, TOKEN};

const STATIC_CHARACTER_PREFIX: &str = "/static/azurlane/javelin%20-%20Javelin";

async fn harness(root: &Path) -> Harness {
    let harness = Harness::new().await;
    point_at(&harness, root).await;
    harness
}

async fn point_at(harness: &Harness, root: &Path) {
    let settings = pyjson::loads(&json!({"path": root.to_str().expect("UTF-8 path")}).to_string()).expect("JSON");
    harness.store_settings("web.azurlane", &settings).await;
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
        let answer = harness.request(&method, &path, &header_refs, None).await;
        let context = format!("{method} {path} {headers:?}");
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
    let document = golden::load("azurlane/api.json");
    let case = &golden::cases(&document)[0];
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("azurlane");
    write_files(&root, golden::get(case, "files"));
    let harness = harness(&root).await;
    replay(&harness, golden::list(golden::get(case, "exchanges"))).await;
    // A second root: a manifest whose counts fail the summary models, and no snapshot.
    let hostile = tmp.path().join("azurlane-hostile");
    write_files(&hostile, golden::get(case, "hostile_files"));
    point_at(&harness, &hostile).await;
    replay(&harness, golden::list(golden::get(case, "hostile_exchanges"))).await;
    harness.finish().await;
}

// ---------- tests/test_azurlane_api.py ----------

fn value(json: &serde_json::Value) -> Value {
    pyjson::loads(&json.to_string()).expect("JSON")
}

fn write_json(path: &Path, json: &serde_json::Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json.to_string()).unwrap();
}

fn write_asset(path: &Path, body: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// `_asset(path, body=..., kind=..., context=..., sha256=..., content_type=...)`.
fn asset(path: &str, body: &[u8], kind: &str, context: &serde_json::Value, sha256: &str, content_type: &str) -> serde_json::Value {
    let name = path.rsplit('/').next().unwrap_or(path);
    json!({
        "url": format!("https://static.example/live2d/azurlane/{name}"),
        "normalized_url": format!("https://static.example/live2d/azurlane/{name}"),
        "downloaded_url": format!("https://static.example/live2d/azurlane/{name}"),
        "fallback_url": format!("https://fallback.example/live2d/azurlane/{name}"),
        "source_urls": {
            "primary": format!("https://static.example/live2d/azurlane/{name}"),
            "fallback": format!("https://fallback.example/live2d/azurlane/{name}"),
            "downloaded": format!("https://static.example/live2d/azurlane/{name}"),
        },
        "kind": kind,
        "local_path": path,
        "original_filename": name,
        "sha256": sha256,
        "size": body.len(),
        "content_type": content_type,
        "status": "downloaded",
        "available": true,
        "failed_count": 0,
        "error": "",
        "last_attempt_at": null,
        "next_retry_at": null,
        "last_seen_at": "2026-05-19T00:00:00+00:00",
        "context_hashes": [format!("hash-{kind}")],
        "contexts": [context],
    })
}

/// `_model_context(model_id=..., model_type=..., costume_key=..., field=...)`.
fn model_context(model_id: &str, model_type: &str, costume_key: &str, field: &str) -> serde_json::Value {
    let mut context = json!({
        "model_id": model_id,
        "model_type": model_type,
        "character_key": "javelin",
        "costume_key": costume_key,
        "catalog_source": if model_type == "live2d" { "merged" } else { "l2d.su" },
        "source_model_url": format!("https://static.example/live2d/azurlane/{costume_key}"),
        "fallback_model_url": format!("https://fallback.example/live2d/azurlane/{costume_key}"),
    });
    let key = if model_type == "live2d" { "live2d_field" } else { "spine_field" };
    context[key] = json!(field);
    context
}

const A64: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// `_create_azurlane_fixture(root)`.
#[allow(clippy::too_many_lines)] // One literal fixture, as Python writes it.
fn create_fixture(root: &Path) {
    let character_root = root.join("javelin - Javelin");
    let files: [(&str, &[u8]); 6] = [
        ("assets/live2d/javelin/javelin.model3.json", br#"{"Version":3}"#),
        ("assets/live2d/javelin/javelin.moc3", b"moc"),
        ("assets/live2d/javelin/textures/texture_00.webp", b"texture"),
        ("assets/spine/javelin_spine/javelin_spine.skel", b"skel"),
        ("assets/spine/javelin_spine/javelin_spine.atlas", b"atlas"),
        ("assets/spine/javelin_spine/javelin_spine.webp", b"spine-texture"),
    ];
    for (relative, body) in files {
        write_asset(&character_root.join(relative), body);
    }
    let live2d_id = "azurlane:live2d:javelin:javelin";
    let spine_id = "azurlane:spine:javelin:javelin_spine";
    let b64 = "b".repeat(64);
    let c64 = "c".repeat(64);
    let live2d_assets = json!([
        asset(
            "assets/live2d/javelin/javelin.model3.json",
            files[0].1,
            "live2d.model3",
            &model_context(live2d_id, "live2d", "javelin", "model3"),
            &b64,
            "application/json",
        ),
        asset(
            "assets/live2d/javelin/javelin.moc3",
            files[1].1,
            "live2d.moc3",
            &model_context(live2d_id, "live2d", "javelin", "moc3"),
            A64,
            "application/octet-stream",
        ),
        asset(
            "assets/live2d/javelin/textures/texture_00.webp",
            files[2].1,
            "live2d.texture",
            &model_context(live2d_id, "live2d", "javelin", "texture"),
            &c64,
            "image/webp",
        ),
        asset(
            "assets/live2d/javelin/escape.txt",
            b"outside",
            "live2d.audio",
            &model_context(live2d_id, "live2d", "javelin", "audio"),
            A64,
            "text/plain",
        ),
    ]);
    let spine_assets = json!([
        asset(
            "assets/spine/javelin_spine/javelin_spine.skel",
            files[3].1,
            "spine.skel",
            &model_context(spine_id, "spine", "javelin_spine", "skel"),
            A64,
            "application/octet-stream",
        ),
        asset(
            "assets/spine/javelin_spine/javelin_spine.atlas",
            files[4].1,
            "spine.atlas",
            &model_context(spine_id, "spine", "javelin_spine", "atlas"),
            A64,
            "application/octet-stream",
        ),
        asset(
            "assets/spine/javelin_spine/javelin_spine.webp",
            files[5].1,
            "spine.texture",
            &model_context(spine_id, "spine", "javelin_spine", "texture"),
            A64,
            "image/webp",
        ),
    ]);
    let completed = "2026-05-19T00:00:00+00:00";
    let live2d_model = json!({
        "model_id": live2d_id,
        "type": "live2d",
        "source": "merged",
        "character_key": "javelin",
        "costume": {"key": "javelin", "id": 1, "name_zh": "默认", "name_en": "Default"},
        "source_urls": {
            "primary": "https://static.example/live2d/azurlane/javelin/javelin.model3.json",
            "fallback": "https://fallback.example/live2d/azurlane/javelin/javelin.model3.json",
            "display_info": "",
        },
        "availability": {
            "source_state": "available",
            "archive_state": "complete",
            "asset_status_counts": {"downloaded": 4},
            "available_asset_count": 4,
            "asset_count": 4,
            "completed_at": completed,
        },
        "source_metadata": {"id": live2d_id},
        "fetched_at": completed,
        "completed_at": completed,
        "assets": live2d_assets,
        "asset_counts": {"live2d.model3": 1, "live2d.moc3": 1, "live2d.texture": 1, "live2d.audio": 1},
    });
    let spine_model = json!({
        "model_id": spine_id,
        "type": "spine",
        "source": "l2d.su",
        "character_key": "javelin",
        "costume": {"key": "javelin_spine", "id": 2, "name_zh": "动态", "name_en": "Dynamic"},
        "source_urls": {"primary": "https://static.example/live2d/azurlane/javelin_spine", "fallback": "", "display_info": ""},
        "availability": {
            "source_state": "available",
            "archive_state": "complete",
            "asset_status_counts": {"downloaded": 3},
            "available_asset_count": 3,
            "asset_count": 3,
            "completed_at": completed,
        },
        "source_metadata": {"id": spine_id},
        "fetched_at": completed,
        "completed_at": completed,
        "assets": spine_assets,
        "asset_counts": {"spine.skel": 1, "spine.atlas": 1, "spine.texture": 1},
    });
    write_json(
        &character_root.join("manifest.json"),
        &json!({
            "schema_version": 1,
            "source": "azurlane",
            "character_key": "javelin",
            "source_id": 1,
            "name_zh": "标枪",
            "name_en": "Javelin",
            "directory_name": "javelin - Javelin",
            "manifest_path": character_root.join("manifest.json").to_str().unwrap(),
            "source_metadata": {"model_ids": [live2d_id, spine_id], "sources": ["merged", "l2d.su"]},
            "fetched_at": completed,
            "completed_at": completed,
            "active": true,
            "model_counts": {"live2d": 1, "spine": 1, "total": 2},
            "asset_counts": {
                "live2d.model3": 1,
                "live2d.moc3": 1,
                "live2d.texture": 1,
                "live2d.audio": 1,
                "spine.skel": 1,
                "spine.atlas": 1,
                "spine.texture": 1,
            },
            "models": [spine_model, live2d_model.clone()],
        }),
    );
    write_json(
        &character_root.join("character.json"),
        &json!({
            "schema_version": 1,
            "source": "azurlane",
            "character_key": "javelin",
            "name_zh": "标枪",
            "name_en": "Javelin",
            "models": [{
                "model_id": live2d_id,
                "type": "live2d",
                "source": "merged",
                "costume": live2d_model["costume"],
                "source_urls": live2d_model["source_urls"],
                "availability": live2d_model["availability"],
            }],
        }),
    );
}

/// `_add_minimal_azurlane_character(root, ...)`.
fn add_minimal_character(root: &Path, character_key: &str, name_en: &str, completed_at: &str, source_id: i64) {
    write_json(
        &root.join(format!("{character_key} - {name_en}")).join("manifest.json"),
        &json!({
            "schema_version": 1,
            "source": "azurlane",
            "character_key": character_key,
            "source_id": source_id,
            "name_zh": "",
            "name_en": name_en,
            "source_metadata": {"sources": ["l2d.su"]},
            "fetched_at": completed_at,
            "completed_at": completed_at,
            "active": true,
            "model_counts": {"live2d": 0, "spine": 0, "total": 0},
            "asset_counts": {},
            "models": [],
        }),
    );
}

fn add_z23(root: &Path) {
    add_minimal_character(root, "z23", "Z23", "2026-05-19T00:01:00+00:00", 23);
}

/// `_add_painting_azurlane_character(root, default_skin_id=...)`.
fn add_painting_character(root: &Path, default_skin_id: Option<i64>) {
    let character_root = root.join("yuzhang - Yuzhang");
    let mut models = Vec::new();
    for (costume_id, costume_key) in [(21, "yuzhang"), (22, "yuzhang_2")] {
        let icon_path = format!("assets/painting/{costume_key}/squareicon/{costume_key}.webp");
        write_asset(&character_root.join(&icon_path), b"icon");
        let model_id = format!("azurlane:painting:yuzhang:{costume_key}");
        models.push(json!({
            "model_id": model_id,
            "type": "painting",
            "source": "l2d.su",
            "character_key": "yuzhang",
            "costume": {"key": costume_key, "id": costume_id, "name_zh": "", "name_en": costume_key},
            "source_urls": {"primary": "", "fallback": "", "display_info": ""},
            "availability": {"archive_state": "complete"},
            "source_metadata": {"id": model_id},
            "assets": [asset(
                &icon_path,
                b"icon",
                "icon.square",
                &json!({"model_id": model_id, "model_type": "painting", "costume_key": costume_key}),
                &costume_id.to_string().repeat(32),
                "image/webp",
            )],
            "asset_counts": {"icon.square": 1},
        }));
    }
    let mut metadata = json!({"sources": ["l2d.su"], "skin_series": ["Default", "Swimsuits"]});
    if let Some(default_skin_id) = default_skin_id {
        metadata["default_skin_id"] = json!(default_skin_id);
    }
    write_json(
        &character_root.join("manifest.json"),
        &json!({
            "schema_version": 1,
            "source": "azurlane",
            "character_key": "yuzhang",
            "source_id": 2,
            "name_zh": "",
            "name_en": "Yuzhang",
            "source_metadata": metadata,
            "active": true,
            "model_counts": {"live2d": 0, "spine": 0, "painting": 2, "total": 2},
            "asset_counts": {"icon.square": 2},
            "models": models,
        }),
    );
}

/// `_add_layered_painting_character(root)`.
fn add_layered_painting_character(root: &Path) -> &'static str {
    let character_root = root.join("xiafei - Richelieu");
    let model_id = "azurlane:painting:xiafei:xiafei_4";
    let files: [(&str, &str, &[u8]); 4] = [
        ("assets/painting/xiafei_4/xiafei_4.json", "painting.index", b"{}"),
        ("assets/painting/xiafei_4/xiafei_4.webp", "painting.image", b"sheet"),
        ("assets/painting/xiafei_4/painting/xiafei_4_rw.webp", "painting.layer", b"layer"),
        (
            "assets/painting/xiafei_4/painting/xiafei_4_rw-mesh.obj",
            "painting.mesh",
            b"g mesh\n",
        ),
    ];
    let mut assets = Vec::new();
    let mut counts = serde_json::Map::new();
    for (path, kind, body) in files {
        write_asset(&character_root.join(path), body);
        let field = kind.split('.').nth(1).unwrap();
        assets.push(asset(
            path,
            body,
            kind,
            &json!({"model_id": model_id, "painting_field": field}),
            A64,
            "application/octet-stream",
        ));
        counts.insert(kind.to_owned(), json!(1));
    }
    let painting_index = json!({
        "painting_key": "xiafei_4",
        "index_url": "https://static.example/painting/xiafei_4.json",
        "layers": [
            {"name": "xiafei_4", "size": [4574.0, 2866.0], "rawSize": [2048.0, 1283.0], "position": [0.0, 0.0], "raw": true},
            {"name": "xiafei_4_rw", "size": [2000.0, 2048.0], "rawSize": [2000.0, 2048.0], "position": [10.0, 272.0], "raw": false},
        ],
        "face": {"name": "face", "size": [372.0, 374.0], "position": [-406.4, 596.8]},
    });
    write_json(
        &character_root.join("manifest.json"),
        &json!({
            "schema_version": 1,
            "source": "azurlane",
            "character_key": "xiafei",
            "name_en": "Richelieu",
            "source_metadata": {"sources": ["l2d.su"]},
            "active": true,
            "model_counts": {"live2d": 0, "spine": 0, "painting": 1, "total": 1},
            "asset_counts": counts,
            "models": [{
                "model_id": model_id,
                "type": "painting",
                "source": "l2d.su",
                "character_key": "xiafei",
                "costume": {"key": "xiafei_4", "id": 10, "name_zh": "", "name_en": "Richelieu"},
                "source_urls": {"primary": "", "fallback": "", "display_info": ""},
                "availability": {"archive_state": "complete"},
                "source_metadata": {"id": model_id, "painting_index": painting_index},
                "assets": assets,
                "asset_counts": counts,
            }],
        }),
    );
    model_id
}

fn library(root: &Path) -> Arc<AzurLaneLibrary> {
    Arc::new(AzurLaneLibrary::new(root))
}

fn get<'a>(value: &'a Value, path: &[&str]) -> &'a Value {
    path.iter().fold(value, |value, key| match key.parse::<usize>() {
        Ok(index) => &golden::list(value)[index],
        Err(_) => golden::get(value, key),
    })
}

fn text(value: &Value, path: &[&str]) -> String {
    golden::string(get(value, path))
}

fn keys(characters: &[fav_core::pyjson::Dict], field: &str) -> Vec<String> {
    characters
        .iter()
        .map(|character| golden::string(character.get(field).expect("field")))
        .collect()
}

// py: tests/test_azurlane_api.py::test_azurlane_painting_files_expose_the_index_layers_and_meshes
#[test]
fn painting_files_expose_the_index_layers_and_meshes() {
    let tmp = tempfile::tempdir().unwrap();
    let model_id = add_layered_painting_character(tmp.path());
    let character = Value::Dict(library(tmp.path()).get_character("xiafei").expect("character"));
    let model = golden::list(get(&character, &["models"]))
        .iter()
        .find(|model| text(model, &["model_id"]) == model_id)
        .expect("model");
    let files = get(model, &["files"]);
    assert_eq!(text(files, &["index", "path"]), "assets/painting/xiafei_4/xiafei_4.json");
    assert_eq!(text(files, &["image", "path"]), "assets/painting/xiafei_4/xiafei_4.webp");
    let paths = |name: &str| -> Vec<String> { golden::list(get(files, &[name])).iter().map(|item| text(item, &["path"])).collect() };
    assert_eq!(paths("layers"), ["assets/painting/xiafei_4/painting/xiafei_4_rw.webp"]);
    assert_eq!(paths("meshes"), ["assets/painting/xiafei_4/painting/xiafei_4_rw-mesh.obj"]);
    let mut all = vec![get(files, &["index"]), get(files, &["image"])];
    all.extend(golden::list(get(files, &["layers"])));
    all.extend(golden::list(get(files, &["meshes"])));
    assert!(all.iter().all(|item| get(item, &["available"]) == &Value::Bool(true)));

    let layers = golden::list(get(model, &["source_metadata", "painting_index", "layers"]));
    let names: Vec<String> = layers.iter().map(|layer| text(layer, &["name"])).collect();
    assert_eq!(names, ["xiafei_4", "xiafei_4_rw"]);
    assert_eq!(get(&layers[0], &["raw"]), &Value::Bool(true));
    assert_eq!(
        get(model, &["source_metadata", "painting_index", "face", "size"]),
        &value(&json!([372.0, 374.0]))
    );
}

fn yuzhang(root: &Path) -> Value {
    let characters = library(root).list_characters().expect("characters");
    Value::Dict(
        characters
            .into_iter()
            .find(|character| character.get("character_key") == Some(&Value::from("yuzhang")))
            .expect("yuzhang"),
    )
}

// py: tests/test_azurlane_api.py::test_azurlane_summary_icon_prefers_the_default_skin_square_icon
#[test]
fn summary_icon_prefers_the_default_skin_square_icon() {
    let tmp = tempfile::tempdir().unwrap();
    add_painting_character(tmp.path(), Some(22));
    let character = yuzhang(tmp.path());
    assert_eq!(text(&character, &["icon", "kind"]), "icon.square");
    assert_eq!(get(&character, &["icon", "available"]), &Value::Bool(true));
    assert!(text(&character, &["icon", "url"]).contains("yuzhang_2/squareicon/yuzhang_2.webp"));
    assert_eq!(
        get(&character, &["source_metadata", "skin_series"]),
        &value(&json!(["Default", "Swimsuits"]))
    );
}

// py: tests/test_azurlane_api.py::test_azurlane_summary_icon_falls_back_when_no_default_skin_is_recorded
#[test]
fn summary_icon_falls_back_when_no_default_skin_is_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    add_painting_character(tmp.path(), None);
    let character = yuzhang(tmp.path());
    assert_eq!(text(&character, &["icon", "kind"]), "icon.square");
    assert_eq!(get(&character, &["icon", "available"]), &Value::Bool(true));
}

// py: tests/test_azurlane_api.py::test_azurlane_library_discovers_manifests_and_uses_summary_cache
#[test]
fn library_discovers_manifests_and_uses_summary_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("azurlane");
    create_fixture(&root);
    write_json(
        &root.join("_source").join("manifest.json"),
        &json!({"character_key": "ignored-source"}),
    );
    let external = tmp.path().join("external");
    write_json(
        &external.join("manifest.json"),
        &json!({"character_key": "outside", "name_en": "Outside"}),
    );
    std::os::unix::fs::symlink(&external, root.join("outside - Outside")).unwrap();

    let expected = library(&root).list_characters().expect("characters");
    assert!(root.join("_api").join("summary-cache.json").is_file());
    assert_eq!(keys(&expected, "character_key"), ["javelin"]);
    let summary = Value::Dict(expected[0].clone());
    assert_eq!(text(&summary, &["display_name"]), "Javelin");
    assert_eq!(
        get(&summary, &["model_counts"]),
        &value(&json!({"live2d": 1, "spine": 1, "total": 2}))
    );
    assert_eq!(get(&summary, &["source_counts"]), &value(&json!({"l2d.su": 1, "merged": 1})));
    assert_eq!(
        get(&summary, &["tags"]),
        &value(&json!({"model_types": "live2d, spine", "sources": "l2d.su, merged"}))
    );
    assert_eq!(
        text(&summary, &["representative_asset", "url"]),
        format!(
            "{STATIC_CHARACTER_PREFIX}/assets/live2d/javelin/textures/texture_00.webp?v={}",
            "c".repeat(64)
        )
    );

    // A second library answers from the disk cache without reading a manifest.
    let cached = library(&root);
    let summaries = cached.list_characters().expect("characters");
    assert_eq!(cached.hooks.manifest_reads.lock().unwrap().as_slice(), &[] as &[std::path::PathBuf]);
    let dumps = |items: &[fav_core::pyjson::Dict]| -> Vec<String> {
        items
            .iter()
            .map(|item| pyjson::dumps(&Value::Dict(item.clone()), &pyjson::DumpOptions::hash()).unwrap())
            .collect()
    };
    assert_eq!(dumps(&summaries), dumps(&expected));
}

/// `_gate_summary_rebuild`: block summary rebuilds until the returned gate opens.
fn gate_summary_rebuild(library: &AzurLaneLibrary) -> Arc<(Mutex<bool>, Condvar)> {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let waiting = Arc::clone(&gate);
    *library.hooks.before_build.lock().unwrap() = Some(Arc::new(move || {
        let (open, condvar) = &*waiting;
        let (open, timeout) = condvar
            .wait_timeout_while(open.lock().unwrap(), Duration::from_secs(10), |open| !*open)
            .unwrap();
        assert!(*open && !timeout.timed_out(), "the gate opens");
    }));
    gate
}

fn open(gate: &(Mutex<bool>, Condvar)) {
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
}

// py: tests/test_azurlane_api.py::test_azurlane_summary_cache_serves_stale_data_while_refreshing
#[test]
fn summary_cache_serves_stale_data_while_refreshing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("azurlane");
    create_fixture(&root);
    let library = library(&root);
    assert_eq!(keys(&library.list_characters().unwrap(), "character_key"), ["javelin"]);

    add_z23(&root);
    let gate = gate_summary_rebuild(&library);
    let stale = library.list_characters().unwrap();
    assert_eq!(keys(&stale, "character_key"), ["javelin"]);

    open(&gate);
    assert!(library.join_refresh(), "the refresh ran");
    assert_eq!(keys(&library.list_characters().unwrap(), "character_key"), ["javelin", "z23"]);
}

// py: tests/test_azurlane_api.py::test_azurlane_summary_cache_incrementally_rebuilds_only_changed_manifests
#[test]
fn summary_cache_incrementally_rebuilds_only_changed_manifests() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("azurlane");
    create_fixture(&root);
    add_z23(&root);
    library(&root).list_characters().unwrap();

    let manifest_path = root.join("z23 - Z23").join("manifest.json");
    let mut manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["name_en"] = json!("Z23 Kai");
    write_json(&manifest_path, &manifest);

    let library = library(&root);
    let gate = gate_summary_rebuild(&library);
    let stale = library.list_characters().unwrap();
    assert_eq!(keys(&stale, "display_name"), ["Javelin", "Z23"]);

    open(&gate);
    assert!(library.join_refresh(), "the refresh ran");
    assert_eq!(keys(&library.list_characters().unwrap(), "display_name"), ["Javelin", "Z23 Kai"]);
    assert_eq!(library.hooks.manifest_reads.lock().unwrap().as_slice(), [manifest_path]);
}

// py: tests/test_azurlane_api.py::test_azurlane_summary_cache_rebuilds_from_legacy_cache_format
#[test]
fn summary_cache_rebuilds_from_legacy_cache_format() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("azurlane");
    create_fixture(&root);
    write_json(
        &root.join("_api").join("summary-cache.json"),
        &json!({"version": 1, "signature": [], "records": [], "summaries": []}),
    );
    assert_eq!(keys(&library(&root).list_characters().unwrap(), "character_key"), ["javelin"]);
}

// py: tests/test_azurlane_api.py::test_get_azurlane_character_shapes_models_and_asset_urls
#[test]
fn get_character_shapes_models_and_asset_urls() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("azurlane");
    create_fixture(&root);
    let library = library(&root);
    let payload = Value::Dict(library.get_character("javelin").unwrap());

    assert_eq!(text(&payload, &["character_key"]), "javelin");
    assert_eq!(text(&payload, &["title"]), "Javelin");
    assert_eq!(text(&payload, &["name_zh"]), "标枪");
    assert_eq!(golden::int(get(&payload, &["model_count"])), 2);
    let ids: Vec<String> = golden::list(get(&payload, &["models"]))
        .iter()
        .map(|model| text(model, &["model_id"]))
        .collect();
    assert_eq!(ids, ["azurlane:live2d:javelin:javelin", "azurlane:spine:javelin:javelin_spine"]);
    let live2d = get(&payload, &["live2d_models", "0"]);
    assert_eq!(
        get(live2d, &["costume"]),
        &value(&json!({"key": "javelin", "id": 1, "name_zh": "默认", "name_en": "Default"}))
    );
    assert_eq!(
        text(live2d, &["source_urls", "fallback"]),
        "https://fallback.example/live2d/azurlane/javelin/javelin.model3.json"
    );
    assert_eq!(text(live2d, &["availability", "archive_state"]), "complete");
    assert_eq!(
        text(live2d, &["files", "model3", "path"]),
        "assets/live2d/javelin/javelin.model3.json"
    );
    assert_eq!(
        text(live2d, &["files", "model3", "url"]),
        format!(
            "{STATIC_CHARACTER_PREFIX}/assets/live2d/javelin/javelin.model3.json?v={}",
            "b".repeat(64)
        )
    );
    assert_eq!(text(live2d, &["files", "textures", "0", "content_type"]), "image/webp");
    assert_eq!(get(live2d, &["files", "textures", "0", "available"]), &Value::Bool(true));
    assert_eq!(text(live2d, &["files", "textures", "0", "field"]), "texture");
    assert_eq!(text(live2d, &["files", "audio", "0", "path"]), "assets/live2d/javelin/escape.txt");
    assert_eq!(
        text(&payload, &["spine_models", "0", "files", "atlas", "path"]),
        "assets/spine/javelin_spine/javelin_spine.atlas"
    );
    assert_eq!(golden::list(get(&payload, &["assets"])).len(), 7);

    assert_eq!(library.get_character("missing"), Err(super::LibraryError::CharacterNotFound));
}

fn auth() -> String {
    format!("Bearer {TOKEN}")
}

async fn request(harness: &Harness, target: &str, extra: &[(&str, &str)]) -> Answer {
    let auth = auth();
    let mut headers = vec![("Authorization", auth.as_str())];
    headers.extend_from_slice(extra);
    harness.request("GET", target, &headers, None).await
}

async fn fixture_harness(tmp: &Path) -> Harness {
    let root = tmp.join("azurlane");
    create_fixture(&root);
    harness(&root).await
}

// py: tests/test_azurlane_api.py::test_azurlane_character_routes_require_authorization
#[tokio::test]
async fn character_routes_require_authorization() {
    let tmp = tempfile::tempdir().unwrap();
    let harness = fixture_harness(tmp.path()).await;
    let missing = harness.request("GET", "/api/v2/azurlane/characters", &[], None).await;
    let invalid = harness
        .request(
            "GET",
            "/api/v2/azurlane/characters",
            &[("Authorization", "Bearer wrong-token")],
            None,
        )
        .await;
    assert_eq!(missing.status, 401);
    assert_eq!(missing.header("www-authenticate"), Some("Bearer realm=\"fav-api\""));
    assert_eq!(text(&missing.json(), &["error", "code"]), "missing_authorization");
    assert_eq!(invalid.status, 403);
    assert_eq!(text(&invalid.json(), &["error", "code"]), "invalid_token");
    harness.finish().await;
}

fn item_keys(payload: &Value) -> Vec<String> {
    golden::list(get(payload, &["items"]))
        .iter()
        .map(|item| text(item, &["character_key"]))
        .collect()
}

// py: tests/test_azurlane_api.py::test_list_azurlane_characters_supports_pagination_and_search
#[tokio::test]
async fn list_characters_supports_pagination_and_search() {
    let tmp = tempfile::tempdir().unwrap();
    add_z23(&tmp.path().join("azurlane"));
    let harness = fixture_harness(tmp.path()).await;
    let page = request(&harness, "/api/v2/azurlane/characters?limit=1&offset=1", &[]).await;
    let search = request(&harness, "/api/v2/azurlane/characters?q=javelin", &[]).await;
    let source = request(&harness, "/api/v2/azurlane/characters?q=merged", &[]).await;

    assert_eq!(page.status, 200);
    let payload = page.json();
    assert_eq!(golden::int(get(&payload, &["total"])), 2);
    assert_eq!(golden::int(get(&payload, &["limit"])), 1);
    assert_eq!(golden::int(get(&payload, &["offset"])), 1);
    assert_eq!(item_keys(&payload), ["z23"]);
    for answer in [search, source] {
        assert_eq!(answer.status, 200);
        assert_eq!(golden::int(get(&answer.json(), &["total"])), 1);
        assert_eq!(item_keys(&answer.json()), ["javelin"]);
    }
    harness.finish().await;
}

// py: tests/test_azurlane.py::test_api_job_enum_includes_azurlane
#[test]
fn api_job_enum_includes_azurlane() {
    assert!(crate::handlers::JOB_REQUEST_TARGETS.contains(&"azurlane"));
}

// py: tests/test_azurlane_api.py::test_list_azurlane_sidebar_characters_returns_etagged_light_payload
#[tokio::test]
async fn list_sidebar_characters_returns_etagged_light_payload() {
    let tmp = tempfile::tempdir().unwrap();
    add_z23(&tmp.path().join("azurlane"));
    let harness = fixture_harness(tmp.path()).await;
    let response = request(&harness, "/api/v2/azurlane/sidebar/characters", &[]).await;
    let query = request(&harness, "/api/v2/azurlane/sidebar/characters?q=%E6%A0%87%E6%9E%AA", &[]).await;
    let etag = response.header("etag").expect("etag").to_owned();
    let cached = request(&harness, "/api/v2/azurlane/sidebar/characters", &[("If-None-Match", &etag)]).await;

    assert_eq!(response.status, 200);
    assert_eq!(response.header("cache-control"), Some("public, max-age=300"));
    assert!(etag.starts_with('"'));
    let payload = response.json();
    assert_eq!(golden::int(get(&payload, &["total"])), 2);
    assert_eq!(item_keys(&payload), ["z23", "javelin"]);
    let javelin = get(&payload, &["items", "1"]);
    let fields: Vec<String> = javelin
        .as_dict()
        .unwrap()
        .iter()
        .map(|(key, _)| fav_runtime::pystr::str_key(key))
        .collect();
    let mut sorted = fields.clone();
    sorted.sort();
    let mut expected = vec![
        "character_key",
        "title",
        "display_name",
        "name_zh",
        "name_en",
        "representative_asset",
        "icon",
        "source_metadata",
        "model_count",
        "model_counts",
        "source_counts",
        "fetched_at",
        "completed_at",
    ];
    expected.sort_unstable();
    assert_eq!(sorted, expected);
    assert_eq!(
        text(javelin, &["representative_asset", "url"]),
        format!(
            "{STATIC_CHARACTER_PREFIX}/assets/live2d/javelin/textures/texture_00.webp?v={}",
            "c".repeat(64)
        )
    );
    assert_eq!(query.status, 200);
    assert_eq!(golden::int(get(&query.json(), &["total"])), 1);
    assert_eq!(item_keys(&query.json()), ["javelin"]);
    assert_eq!(cached.status, 304);
    assert!(cached.body.is_empty());
    assert_eq!(cached.header("cache-control"), Some("public, max-age=300"));
    assert_eq!(cached.header("etag"), Some(etag.as_str()));
    harness.finish().await;
}

// py: tests/test_azurlane_api.py::test_get_azurlane_skin_updates_normalises_both_arrays
#[tokio::test]
async fn skin_updates_normalise_both_arrays() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("azurlane");
    write_json(
        &root.join("_source").join("l2d-su-snapshot.json"),
        &json!({
            "game_version": "9.7.323",
            "region": "CN",
            "generated_at": "2026-08-13T05:33:57.726Z",
            "characters": [{"char_id": 1}],
            "filters": {"skinSeries": vec!["a"; 32]},
            "new_skins": [
                {"ship_group_id": 10720, "ship_name": "本宁顿", "skin_name": "盛夏", "skin_type": "Live2D+", "skin_ids": [107_201]},
            ],
            "skin_update_history": [{
                "date": "2026-08-13T03:57:01.516Z",
                "version": "9.7.323",
                "skins": [{"shipGroupId": 20712, "shipName": "不挠", "skinName": "领航员", "skinType": "Live2D+", "skinIds": [207_122]}],
            }],
        }),
    );
    let harness = fixture_harness(tmp.path()).await;
    let response = request(&harness, "/api/v2/azurlane/skin-updates", &[]).await;
    let etag = response.header("etag").expect("etag").to_owned();
    let cached = request(&harness, "/api/v2/azurlane/skin-updates", &[("If-None-Match", &etag)]).await;

    assert_eq!(response.status, 200);
    assert_eq!(response.header("cache-control"), Some("public, max-age=300"));
    let payload = response.json();
    assert_eq!(text(&payload, &["game_version"]), "9.7.323");
    assert_eq!(text(&payload, &["region"]), "CN");
    assert_eq!(
        get(&payload, &["new_skins"]),
        &value(
            &json!([{"ship_group_id": 10720, "ship_name": "本宁顿", "skin_name": "盛夏", "skin_type": "Live2D+", "skin_ids": [107_201]}])
        )
    );
    // camelCase in, snake_case out, so both arrays carry the same shape.
    assert_eq!(
        get(&payload, &["skin_update_history", "0", "skins"]),
        &value(
            &json!([{"ship_group_id": 20712, "ship_name": "不挠", "skin_name": "领航员", "skin_type": "Live2D+", "skin_ids": [207_122]}])
        )
    );
    // The catalog and filter enumerations are not echoed back.
    let fields: Vec<String> = payload
        .as_dict()
        .unwrap()
        .iter()
        .map(|(key, _)| fav_runtime::pystr::str_key(key))
        .collect();
    assert_eq!(
        fields,
        ["game_version", "region", "generated_at", "new_skins", "skin_update_history"]
    );
    assert_eq!(cached.status, 304);
    harness.finish().await;
}

// py: tests/test_azurlane_api.py::test_get_azurlane_skin_updates_404s_without_a_snapshot
#[tokio::test]
async fn skin_updates_404_without_a_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let harness = fixture_harness(tmp.path()).await;
    let response = request(&harness, "/api/v2/azurlane/skin-updates", &[]).await;
    assert_eq!(response.status, 404);
    assert_eq!(text(&response.json(), &["error", "code"]), "azurlane_skin_updates_not_found");
    harness.finish().await;
}

// py: tests/test_azurlane_api.py::test_get_azurlane_character_route_returns_models_and_assets
#[tokio::test]
async fn character_route_returns_models_and_assets() {
    let tmp = tempfile::tempdir().unwrap();
    let harness = fixture_harness(tmp.path()).await;
    let response = request(&harness, "/api/v2/azurlane/characters/javelin", &[]).await;
    assert_eq!(response.status, 200);
    let payload = response.json();
    assert_eq!(text(&payload, &["character_key"]), "javelin");
    assert_eq!(get(&payload, &["active"]), &Value::Bool(true));
    assert_eq!(
        get(&payload, &["model_counts"]),
        &value(&json!({"live2d": 1, "spine": 1, "total": 2}))
    );
    let ids: Vec<String> = golden::list(get(&payload, &["models"]))
        .iter()
        .map(|model| text(model, &["model_id"]))
        .collect();
    assert_eq!(ids, ["azurlane:live2d:javelin:javelin", "azurlane:spine:javelin:javelin_spine"]);
    assert_eq!(
        text(&payload, &["live2d_models", "0", "files", "model3", "path"]),
        "assets/live2d/javelin/javelin.model3.json"
    );
    assert_eq!(
        text(&payload, &["spine_models", "0", "files", "atlas", "path"]),
        "assets/spine/javelin_spine/javelin_spine.atlas"
    );
    assert_eq!(golden::list(get(&payload, &["assets"])).len(), 7);
    harness.finish().await;
}

// py: tests/test_azurlane_api.py::test_get_azurlane_character_route_returns_not_found
#[tokio::test]
async fn character_route_returns_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let harness = fixture_harness(tmp.path()).await;
    let response = request(&harness, "/api/v2/azurlane/characters/missing", &[]).await;
    assert_eq!(response.status, 404);
    assert_eq!(
        response.json(),
        value(&json!({"error": {"code": "azurlane_character_not_found", "message": "Azur Lane character not found.", "details": null}}))
    );
    harness.finish().await;
}

// py: tests/test_azurlane_api.py::test_azurlane_asset_resolution_is_static_and_root_safe
// `get_asset_file` is not ported (D117); what remains is the path check it shared with
// the asset references (`_normalize_asset_path`, its unsafe paths plus the empty and `.`
// segments Azur Lane rejects and BD2 collapses) and the root safety of those references:
// a symlink out of the collection is not available.
#[test]
fn asset_resolution_is_static_and_root_safe() {
    use super::library::normalize_asset_path;
    for unsafe_path in [
        "../outside.txt",
        "/assets/live2d/javelin/javelin.moc3",
        "assets/../outside.txt",
        "assets/%2e%2e/outside.txt",
        "assets/./a.png",
        "assets//a.png",
        "assets/a/",
        "assetsx/a",
        "",
    ] {
        assert_eq!(normalize_asset_path(unsafe_path), None, "{unsafe_path:?}");
    }
    assert_eq!(normalize_asset_path("assets\\a\\b.png").as_deref(), Some("assets/a/b.png"));
    assert_eq!(normalize_asset_path(" assets/a%20b.png ").as_deref(), Some("assets/a b.png"));

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("azurlane");
    create_fixture(&root);
    let outside = tmp.path().join("outside.txt");
    std::fs::write(&outside, "outside").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("javelin - Javelin/assets/live2d/javelin/escape.txt")).unwrap();
    // A `.` segment is not collapsed: the reference keeps its raw path and has no URL.
    let manifest = root.join("javelin - Javelin/manifest.json");
    let text_before = std::fs::read_to_string(&manifest).unwrap();
    let dotted = "assets/spine/javelin_spine/./javelin_spine.webp";
    std::fs::write(
        &manifest,
        text_before.replace("assets/spine/javelin_spine/javelin_spine.webp", dotted),
    )
    .unwrap();

    let payload = Value::Dict(library(&root).get_character("javelin").unwrap());
    let assets = golden::list(get(&payload, &["assets"]));
    let by_path = |path: &str| assets.iter().find(|asset| text(asset, &["path"]) == path).expect("asset");
    let texture = by_path("assets/live2d/javelin/textures/texture_00.webp");
    assert_eq!(get(texture, &["available"]), &Value::Bool(true));
    assert_eq!(text(texture, &["content_type"]), "image/webp");
    let escape = by_path("assets/live2d/javelin/escape.txt");
    assert_eq!(get(escape, &["available"]), &Value::Bool(false));
    assert_eq!(
        text(escape, &["url"]),
        format!("{STATIC_CHARACTER_PREFIX}/assets/live2d/javelin/escape.txt?v={A64}")
    );
    let dotted_asset = by_path(dotted);
    assert_eq!(get(dotted_asset, &["available"]), &Value::Bool(false));
    assert_eq!(text(dotted_asset, &["url"]), "");
}
