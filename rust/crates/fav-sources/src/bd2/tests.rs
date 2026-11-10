//! `tests/test_bd2.py` and the BD2 cases of `tests/test_game_content_notifications.py`.
//!
//! Python's tests replace the crawler's methods with fakes; these run the real methods over
//! a scripted web (an HTTP proxy, with the crawler sending `https://` URLs as `http://`) and
//! a test database, and check the same outcomes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{TimeZone as _, Utc};
use fav_core::pyjson::{self, Dict, Value};
use fav_core::settings::Settings;
use fav_db::Db;
use fav_net::retry::RecordingSleeper;
use fav_net::{ClientOptions, HttpClient};
use sqlx::Row as _;

use super::extract::{
    add_viewer_model_assets, assign_asset_paths, extract_resources, filter_bd2_character_rows, live2d_atlas_texture_context,
    max_attempts_for_status, prefer_live2d_model_assets_from_viewer, retry_after_delay_seconds, retry_delay_seconds,
    supplement_live2d_models_from_viewer, video_retry_cooldown_days, viewer_extra_resources,
};
use super::gamekee::{Asset, Assets, add_asset, sha256_hex};
use super::{Bd2Crawler, client_options};
use crate::bd2_l2d_viewer::{Category, ViewerResource};
use crate::content_discovery::CharacterSnapshot;
use crate::testkit::RecordingNotifier;
use crate::testutil::{FakeWeb, Reply, Seen};

// ---------- helpers ----------

fn unused_db() -> Db {
    Db::connect_lazy("postgresql://tests@127.0.0.1:1/unused").expect("lazy pool")
}

fn obj(text: &str) -> Dict {
    match pyjson::loads(text).expect("JSON") {
        Value::Dict(dict) => dict,
        other => panic!("expected an object, got {other:?}"),
    }
}

fn s(text: &str) -> Value {
    Value::Str(text.to_owned())
}

fn cell(key: &str, kind: &str, value: Value) -> Value {
    let mut dict = Dict::new();
    dict.insert("key", s(key));
    dict.insert("type", s(kind));
    dict.insert("value", value);
    Value::Dict(dict)
}

fn text(key: &str, value: &str) -> Value {
    cell(key, "text", s(value))
}

fn image(key: &str, value: &str) -> Value {
    cell(key, "image", s(value))
}

fn video(key: &str, value: &str) -> Value {
    cell(key, "video", s(value))
}

fn audio(key: &str, value: &str) -> Value {
    cell(key, "audio", s(value))
}

fn row(cells: Vec<Value>) -> Value {
    Value::List(cells)
}

fn empty_row(index: usize) -> Value {
    row(vec![
        text(&format!("label_{index}"), &format!("row-{index}")),
        text(&format!("empty_{index}"), ""),
    ])
}

/// `_style_rows()`.
fn style_rows() -> Vec<Value> {
    let mut rows: Vec<Value> = (0..16).map(empty_row).collect();
    rows[0] = row(vec![text("name_label", "Costume name"), text("costume_name", "Office Worker")]);
    rows[1] = row(vec![
        text("category_label", "Costume category"),
        text("costume_category", "Support"),
    ]);
    rows[2] = row(vec![
        text("sprite_label", "Costume sprite"),
        image("bound_sprite", "/media/sprite.png"),
        image("unbound_sprite", "/media/unbound-sprite.png"),
    ]);
    rows[3] = row(vec![
        text("portrait_label", "Costume portrait"),
        image("bound_portrait", "/media/portrait.png"),
    ]);
    rows[4] = row(vec![
        text("full_portrait_label", "Full costume portrait"),
        image("bound_full_portrait", "/media/full.png"),
    ]);
    rows[5] = row(vec![
        text("live2d_header_label", ""),
        text("live2d_file_header", "Live2D file"),
        text("live2d_icon_header", "Live2D icon"),
    ]);
    rows[6] = row(vec![
        text("standing_label", "Standing Live2D"),
        cell(
            "bound_live2d",
            "live2d",
            Value::Dict(obj(
                r#"{"live2dKey": "model-a", "atlas": "/spine/model.atlas", "json": "/spine/model.json", "skel": "",
                "image": "/spine/model.png,/spine/effect.webp", "bg": "/spine/background.png"}"#,
            )),
        ),
        image("bound_live2d_icon", "/media/live2d-icon.png"),
    ]);
    rows[8] = row(vec![
        text("skill_label", "Skill Live2D"),
        video("bound_skill_video", "https://cdn.example.com/skill.mp4"),
    ]);
    rows[15] = row(vec![text("non_art_label", "Non art"), image("bound_non_art", "/media/non-art.png")]);
    rows
}

fn style_content(base_rows: Vec<Value>, rows: Vec<Value>) -> Dict {
    let mut style = Dict::new();
    style.insert("name", s("style-a"));
    style.insert("data", Value::List(rows));
    let mut content = Dict::new();
    if !base_rows.is_empty() {
        content.insert("baseData", Value::List(base_rows));
    }
    content.insert("styleData", Value::List(vec![Value::Dict(style)]));
    content
}

fn resource(entry_id: &str, category: Category, stem: &str, char_name: &str, costume_name: &str, files: &[&str]) -> ViewerResource {
    ViewerResource {
        entry_id: entry_id.to_owned(),
        category,
        stem: stem.to_owned(),
        char_name: char_name.to_owned(),
        costume_name: costume_name.to_owned(),
        files: files.iter().map(|file| (*file).to_owned()).collect(),
        missing_core_files: Vec::new(),
    }
}

fn get<'a>(dict: &'a Dict, path: &[&str]) -> &'a Value {
    let mut value = dict.get(path[0]).unwrap_or_else(|| panic!("no {}", path[0]));
    for key in &path[1..] {
        value = value.as_dict().expect("object").get(key).unwrap_or_else(|| panic!("no {key}"));
    }
    value
}

fn text_at<'a>(dict: &'a Dict, path: &[&str]) -> &'a str {
    match get(dict, path) {
        Value::Str(text) => text,
        other => panic!("expected a string at {path:?}, got {other:?}"),
    }
}

fn has(assets: &Assets, url: &str, kind: &str) -> bool {
    assets.position(url, kind).is_some()
}

fn asset<'a>(assets: &'a Assets, url: &str, kind: &str) -> &'a Asset {
    &assets.as_slice()[assets.position(url, kind).expect("asset")]
}

fn fixed_now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).single().expect("time")
}

/// A scripted web: answers by `https://` URL, recording every request.
struct Web {
    fake: FakeWeb,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Web {
    async fn start(answer: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let fake = FakeWeb::start(move |request: &Seen| {
            let url = request
                .url
                .strip_prefix("http://")
                .map_or_else(|| request.url.clone(), |rest| format!("https://{rest}"));
            recorder.lock().expect("lock").push(url.clone());
            answer(&url)
        })
        .await;
        Self { fake, seen }
    }

    fn urls(&self) -> Vec<String> {
        self.seen.lock().expect("lock").clone()
    }

    fn client(&self) -> HttpClient {
        HttpClient::new(&ClientOptions {
            proxy: Some(self.fake.proxy.clone()),
            timeout: Duration::from_secs(10),
            ..client_options()
        })
        .expect("client")
    }
}

fn json_reply(text: &str) -> Reply {
    Reply::Respond {
        status: 200,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: text.as_bytes().to_vec(),
    }
}

fn crawler(root: &std::path::Path, http: HttpClient, db: Db, notifier: Arc<RecordingNotifier>) -> Bd2Crawler {
    Bd2Crawler::new(&root.to_string_lossy(), http, db, notifier)
        .with_sleeper(RecordingSleeper::new().sleeper())
        .with_jitter(Arc::new(|| 0))
        .with_clock(Arc::new(fixed_now))
        .with_viewer_resources(Vec::new())
        .with_plain_http()
}

fn offline_crawler(root: &std::path::Path) -> Bd2Crawler {
    let http = HttpClient::new(&client_options()).expect("client");
    crawler(root, http, unused_db(), Arc::new(RecordingNotifier::default()))
}

// ---------- configuration ----------

// py: tests/test_bd2.py::test_bd2_config_defaults_to_disabled_collection_bd2_path
#[test]
fn bd2_config_defaults_to_disabled_collection_bd2_path() {
    let cfg = Settings::default().web.bd2;
    assert!(!cfg.schedule.enabled);
    assert_eq!(cfg.schedule.cron, "0 */6 * * *");
    assert_eq!(cfg.path.as_str(), "collection/bd2");
}

// py: tests/test_bd2.py::test_scheduler_registration_includes_bd2
#[tokio::test]
async fn scheduler_registration_includes_bd2() {
    let jobs = fav_runtime::jobs::build_jobs(&Settings::default());
    let job = jobs.iter().find(|job| job.key == "bd2").expect("a bd2 job");
    assert_eq!(job.name, "BD2");
    assert!(!job.enabled);
    assert!(job.required_commands.is_empty());
    let db = unused_db();
    let ctx = crate::SourceContext {
        settings: Arc::new(fav_db::SettingsStore::new(db.clone())),
        db,
    };
    assert!(crate::registry(&ctx).contains("bd2"));
}

// ---------- retries ----------

// py: tests/test_bd2.py::test_retry_delay_prefers_retry_after_header
#[test]
fn retry_delay_prefers_retry_after_header() {
    let retry_after = retry_after_delay_seconds(Some("2"), fixed_now());
    assert_eq!(retry_after, Some(2.0));
    assert!((retry_delay_seconds(1, retry_after, 249) - 2.0).abs() < f64::EPSILON);
}

// py: tests/test_bd2.py::test_max_attempts_treats_tencent_edge_restriction_as_non_retryable
#[test]
fn max_attempts_treats_tencent_edge_restriction_as_non_retryable() {
    assert_eq!(max_attempts_for_status(567), 1);
}

// py: tests/test_bd2.py::test_video_retry_cooldown_days_caps_at_one_week
#[test]
fn video_retry_cooldown_days_caps_at_one_week() {
    assert_eq!((1..6).map(video_retry_cooldown_days).collect::<Vec<_>>(), [1, 3, 7, 7, 7]);
    assert_eq!(video_retry_cooldown_days(0), 1);
}

// ---------- content JSON ----------

// py: tests/test_bd2.py::test_load_content_json_uses_content_cdn_when_inline_content_is_empty
#[tokio::test]
async fn load_content_json_uses_content_cdn_when_inline_content_is_empty() {
    let web = Web::start(|url| match url {
        "https://api-cdn.example.test/content/1.json" => {
            json_reply(r#"{"content": "{\"styleData\": [{\"name\": \"style-a\", \"data\": []}]}"}"#)
        }
        _ => Reply::status(404),
    })
    .await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let crawler = crawler(tmp.path(), web.client(), unused_db(), Arc::new(RecordingNotifier::default()));

    let content_json = crawler
        .load_content_json(
            &obj(r#"{"content_json": "", "content_cdn": "//api-cdn.example.test/content/1.json"}"#),
            1,
        )
        .await
        .expect("content");

    assert_eq!(content_json, obj(r#"{"styleData": [{"name": "style-a", "data": []}]}"#));
    assert_eq!(web.urls(), ["https://api-cdn.example.test/content/1.json"]);
}

// py: tests/test_bd2.py::test_load_content_json_rejects_empty_inline_content_without_cdn
#[tokio::test]
async fn load_content_json_rejects_empty_inline_content_without_cdn() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let error = offline_crawler(tmp.path())
        .load_content_json(&obj(r#"{"content_json": ""}"#), 1)
        .await
        .unwrap_err();
    assert_eq!(error.class_name(), "RuntimeError");
    assert!(error.message().contains("content_json is empty"));
}

// py: tests/test_bd2.py::test_load_reverse_bind_uses_entry_data_bind_cdn
#[tokio::test]
async fn load_reverse_bind_uses_entry_data_bind_cdn() {
    let web = Web::start(|url| match url {
        "https://api-cdn.example.test/entry_binding/entry_bind_data.json" => {
            json_reply(r#"{"entry_data_bind": {"stable-image": "dynamic-image"}}"#)
        }
        _ => Reply::status(404),
    })
    .await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let crawler = crawler(tmp.path(), web.client(), unused_db(), Arc::new(RecordingNotifier::default()));

    let reverse_bind = crawler
        .load_reverse_bind(
            &obj(r#"{"entry_data_bind_cdn": "//api-cdn.example.test/entry_binding/entry_bind_data.json"}"#),
            1,
        )
        .await
        .expect("reverse bind");

    assert_eq!(reverse_bind, obj(r#"{"dynamic-image": "stable-image"}"#));
}

// ---------- the tree and the page ----------

// py: tests/test_bd2.py::test_filter_bd2_character_rows_keeps_positive_character_descendants_only
#[test]
fn filter_bd2_character_rows_keeps_positive_character_descendants_only() {
    let tree = pyjson::loads(
        r#"[
        {"id": 122323, "name": "5-star characters", "content_id": 0, "sort": 2, "child": [
            {"id": 1, "name": "Alice", "content_id": "1001", "sort": 2, "icon": "/alice.png"},
            {"id": 2, "name": "Duplicate Alice", "content_id": 1001, "sort": 3},
            {"id": 3, "name": "Group placeholder", "content_id": 0, "sort": 4},
            {"id": 4, "name": "Nested", "content_id": 0, "sort": 5, "child": [{"id": 5, "name": "Bea", "content_id": 1002}]}
        ]},
        {"id": 189899, "name": "Equipment", "content_id": 0, "child": [{"id": 6, "name": "Sword", "content_id": 2001}]}
    ]"#,
    )
    .expect("JSON");

    let rows = filter_bd2_character_rows(&tree).expect("rows");

    assert_eq!(rows.iter().map(|row| text_at(row, &["name"])).collect::<Vec<_>>(), ["Alice", "Bea"]);
    assert_eq!(rows[0].get("content_id"), Some(&s("1001")));
    assert_eq!(rows[1].get("content_id"), Some(&Value::Int(1002)));
    assert!(rows.iter().all(|row| row.get("_bd2_group_id") == Some(&Value::Int(122_323))));
    assert!(rows.iter().all(|row| row.get("_bd2_group_name") == Some(&s("5-star"))));
    assert!(rows.iter().all(|row| row.get("child").is_none()));
}

// py: tests/test_bd2.py::test_extract_resources_collects_all_page_media_and_column_contexts
#[test]
fn extract_resources_collects_all_page_media_and_column_contexts() {
    let content_json = style_content(
        vec![
            row(vec![
                text("base_icon_label", "Character icon"),
                image("bound_base_icon", "/base/icon.png"),
            ]),
            row(vec![
                text("base_audio_label", "Voice"),
                audio("bound_base_audio", "/audio/voice.mp3"),
            ]),
        ],
        style_rows(),
    );
    let reverse_bind = obj(
        r#"{"bound_base_icon": "stable-base-icon", "bound_base_audio": "stable-base-audio", "bound_sprite": "stable-sprite",
        "bound_portrait": "stable-portrait", "bound_full_portrait": "stable-full", "bound_live2d": "stable-live2d",
        "bound_live2d_icon": "stable-live2d-icon", "bound_skill_video": "stable-skill-video", "bound_non_art": "stable-non-art"}"#,
    );

    let (assets, models) = extract_resources(
        &content_json,
        Some(&obj(r#"{"icon": "/tree/icon.png", "icon_small": ""}"#)),
        &reverse_bind,
    );

    let urls: Vec<&str> = assets.iter().map(|asset| asset.url.as_str()).collect();
    for expected in [
        "https://www.gamekee.com/media/sprite.png",
        "https://www.gamekee.com/media/portrait.png",
        "https://www.gamekee.com/media/full.png",
        "https://www.gamekee.com/base/icon.png",
        "https://www.gamekee.com/audio/voice.mp3",
        "https://www.gamekee.com/spine/model.atlas",
        "https://www.gamekee.com/spine/model.json",
        "https://www.gamekee.com/spine/model.png",
        "https://www.gamekee.com/spine/effect.webp",
        "https://www.gamekee.com/spine/background.png",
        "https://cdn.example.com/skill.mp4",
        "https://www.gamekee.com/tree/icon.png",
        "https://www.gamekee.com/media/unbound-sprite.png",
        "https://www.gamekee.com/media/non-art.png",
    ] {
        assert!(urls.contains(&expected), "{expected}");
    }

    assert_eq!(text_at(&models[0], &["live2d_key"]), "model-a");
    assert_eq!(text_at(&models[0], &["costume_title"]), "Office Worker");
    assert_eq!(text_at(&models[0], &["field"]), "standing_live2d");
    assert_eq!(get(&models[0], &["column_index"]), &Value::Int(1));
    assert_eq!(text_at(&models[0], &["column_header"]), "Live2D file");
    assert_eq!(text_at(&models[0], &["urls", "bg"]), "https://www.gamekee.com/spine/background.png");

    let by_url = |url: &str| assets.iter().find(|asset| asset.url == url).expect("asset");
    let live2d_icon = by_url("https://www.gamekee.com/media/live2d-icon.png");
    assert_eq!(text_at(&live2d_icon.contexts[0], &["column_role"]), "Live2D icon");
    assert_eq!(get(&live2d_icon.contexts[0], &["is_art_row"]), &Value::Bool(true));
    let non_art = by_url("https://www.gamekee.com/media/non-art.png");
    assert_eq!(text_at(&non_art.contexts[0], &["field"]), "");
    assert_eq!(get(&non_art.contexts[0], &["is_art_row"]), &Value::Bool(false));
    let unbound = by_url("https://www.gamekee.com/media/unbound-sprite.png");
    assert_eq!(text_at(&unbound.contexts[0], &["stable_id"]), "");
}

// ---------- the viewer ----------

/// The model's assets as Python's tests add them: one per Live2D field.
fn model_assets(model: &Dict, fields: &[(&str, &str, &str)]) -> Assets {
    let mut assets = Assets::new();
    for (url, kind, live2d_field) in fields {
        let mut context = live2d_atlas_texture_context(model);
        context.insert("live2d_field", s(live2d_field));
        add_asset(&mut assets, url, kind, context);
    }
    assets
}

const MODEL_FIELDS: &str = r#""section": "style", "row_index": 6, "column_index": 1, "is_art_row": true,
    "column_role": "Live2D file", "column_header": "Live2D file""#;

// py: tests/test_bd2.py::test_viewer_preferred_source_replaces_matching_gamekee_live2d_assets
#[test]
fn viewer_preferred_source_replaces_matching_gamekee_live2d_assets() {
    let original_atlas = "https://www.gamekee.com/spines/cutscene_char060502.atlas";
    let original_skel = "https://www.gamekee.com/spines/cutscene_char060502.skel";
    let original_json = "https://www.gamekee.com/spines/cutscene_char060502.json";
    let original_image = "https://www.gamekee.com/spines/cutscene_char060502.png";
    let original_bg = "https://www.gamekee.com/spines/background.png";
    let model = obj(&format!(
        r#"{{"section": "style", "style_index": 1, "style_name": "style-b", "costume_title": "Fire Graffiti",
        "costume_category": "Attacker", "row_index": 8, "column_index": 1, "label": "Skill Live2D", "field": "skill_live2d_1",
        "is_art_row": true, "column_name": "Fire Graffiti", "column_category": "Attacker", "column_role": "Live2D file",
        "column_header": "Live2D file", "key": "skill_live2d", "stable_id": "stable-skill-live2d", "live2d_key": "9z008il5",
        "urls": {{"atlas": "{original_atlas}", "skel": "{original_skel}", "json": "{original_json}", "image": ["{original_image}"],
        "bg": "{original_bg}"}}}}"#
    ));
    let mut assets = model_assets(
        &model,
        &[
            (original_atlas, "live2d_atlas", "atlas"),
            (original_skel, "live2d_skel", "skel"),
            (original_json, "live2d_json", "json"),
            (original_image, "live2d_texture", "image"),
            (original_bg, "live2d_background", "bg"),
        ],
    );
    let mut models = vec![model];
    let viewer = [resource(
        "060502",
        Category::Ultimate,
        "cutscene_char060502",
        "Anastasia",
        "Fire Graffiti",
        &[
            "060502/cutscene/cutscene_char060502.atlas",
            "060502/cutscene/cutscene_char060502.skel",
            "060502/cutscene/cutscene_char060502.png",
        ],
    )];

    let preferred = prefer_live2d_model_assets_from_viewer(&mut assets, &mut models, &viewer).expect("prefer");

    let model = &models[0];
    assert_eq!(preferred, 1);
    assert_eq!(text_at(model, &["source"]), "bd2_l2d_viewer");
    assert_eq!(text_at(model, &["variant"]), "preferred_source");
    assert_eq!(text_at(model, &["supplement_reason"]), "matched_stem_preferred_source");
    assert_eq!(text_at(model, &["viewer_entry_id"]), "060502");
    assert_eq!(text_at(model, &["viewer_stem"]), "cutscene_char060502");
    assert_eq!(text_at(model, &["gamekee_urls", "atlas"]), original_atlas);
    assert_eq!(text_at(model, &["gamekee_urls", "json"]), original_json);
    assert_eq!(
        get(model, &["source_candidates", "gamekee", "image"]),
        &Value::List(vec![s(original_image)])
    );
    assert!(text_at(model, &["source_candidates", "bd2_l2d_viewer", "atlas"]).ends_with("/060502/cutscene/cutscene_char060502.atlas"));
    assert!(text_at(model, &["urls", "atlas"]).ends_with("/060502/cutscene/cutscene_char060502.atlas"));
    assert!(text_at(model, &["urls", "skel"]).ends_with("/060502/cutscene/cutscene_char060502.skel"));
    assert_eq!(text_at(model, &["urls", "bg"]), original_bg);
    let urls = get(model, &["urls"]).as_dict().expect("urls");
    assert!(urls.get("json").is_none());
    assert!(urls.get("image").is_none());
    assert!(!has(&assets, original_atlas, "live2d_atlas"));
    assert!(!has(&assets, original_skel, "live2d_skel"));
    assert!(!has(&assets, original_json, "live2d_json"));
    assert!(!has(&assets, original_image, "live2d_texture"));
    assert!(has(&assets, original_bg, "live2d_background"));

    let viewer_atlas = asset(
        &assets,
        "https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/assets/spines/060502/cutscene/cutscene_char060502.atlas",
        "live2d_atlas",
    );
    assert_eq!(text_at(&viewer_atlas.contexts[0], &["live2d_key"]), "9z008il5");
    assert_eq!(text_at(&viewer_atlas.contexts[0], &["source"]), "bd2_l2d_viewer");
    assert_eq!(text_at(&viewer_atlas.contexts[0], &["variant"]), "preferred_source");
}

/// The two-file GameKee model of the "keeps" and "category" cases.
fn two_file_case(model_json: &str, atlas: &str, skel: &str, viewer: &ViewerResource) {
    let model = obj(model_json);
    let mut assets = model_assets(&model, &[(atlas, "live2d_atlas", "atlas"), (skel, "live2d_skel", "skel")]);
    let mut models = vec![model];

    let preferred = prefer_live2d_model_assets_from_viewer(&mut assets, &mut models, std::slice::from_ref(viewer)).expect("prefer");

    assert_eq!(preferred, 0);
    let mut expected_urls = Dict::new();
    expected_urls.insert("atlas", s(atlas));
    expected_urls.insert("skel", s(skel));
    assert_eq!(get(&models[0], &["urls"]), &Value::Dict(expected_urls));
    assert!(models[0].get("source").is_none());
    assert!(models[0].get("gamekee_urls").is_none());
    assert!(has(&assets, atlas, "live2d_atlas"));
    assert!(has(&assets, skel, "live2d_skel"));
}

// py: tests/test_bd2.py::test_viewer_preferred_source_keeps_gamekee_only_live2d_assets
#[test]
fn viewer_preferred_source_keeps_gamekee_only_live2d_assets() {
    let atlas = "https://www.gamekee.com/spines/illust_special1.atlas";
    let skel = "https://www.gamekee.com/spines/illust_special1.skel";
    two_file_case(
        &format!(
            r#"{{"style_index": 0, "style_name": "style-a", "costume_title": "Special Story", "costume_category": "",
            "section": "style", "row_index": 10, "column_index": 1, "label": "Story Live2D", "field": "story_live2d_1",
            "is_art_row": true, "column_name": "Special Story", "column_category": "", "column_role": "Live2D file",
            "column_header": "Live2D file", "key": "story_live2d", "stable_id": "stable-story-live2d",
            "live2d_key": "illust-special", "urls": {{"atlas": "{atlas}", "skel": "{skel}"}}}}"#
        ),
        atlas,
        skel,
        &resource(
            "060502",
            Category::Ultimate,
            "cutscene_char060502",
            "Anastasia",
            "Fire Graffiti",
            &[
                "060502/cutscene/cutscene_char060502.atlas",
                "060502/cutscene/cutscene_char060502.skel",
            ],
        ),
    );
}

// py: tests/test_bd2.py::test_viewer_preferred_source_requires_matching_category
#[test]
fn viewer_preferred_source_requires_matching_category() {
    let atlas = "https://www.gamekee.com/spines/char123.atlas";
    let skel = "https://www.gamekee.com/spines/char123.skel";
    two_file_case(
        &format!(
            r#"{{{MODEL_FIELDS}, "style_index": 0, "style_name": "style-a", "costume_title": "Default", "costume_category": "",
            "label": "Standing Live2D", "field": "standing_live2d", "column_name": "Default", "column_category": "",
            "key": "standing_live2d", "stable_id": "stable-standing-live2d", "live2d_key": "char123",
            "urls": {{"atlas": "{atlas}", "skel": "{skel}"}}}}"#
        ),
        atlas,
        skel,
        &resource(
            "dating123",
            Category::Dating,
            "char123",
            "Test",
            "Dating",
            &["dating123/dating/char123.atlas", "dating123/dating/char123.skel"],
        ),
    );
}

fn standing_model(costume: &str, category: &str, live2d_key: &str, stem: &str) -> Dict {
    obj(&format!(
        r#"{{{MODEL_FIELDS}, "style_index": 0, "style_name": "style-a", "costume_title": "{costume}",
        "costume_category": "{category}", "label": "Standing Live2D", "field": "standing_live2d", "column_name": "{costume}",
        "column_category": "{category}", "live2d_key": "{live2d_key}",
        "urls": {{"atlas": "https://www.gamekee.com/spines/{stem}.atlas", "skel": "https://www.gamekee.com/spines/{stem}.skel"}}}}"#
    ))
}

// py: tests/test_bd2.py::test_viewer_supplement_fills_missing_interaction_live2d
#[test]
fn viewer_supplement_fills_missing_interaction_live2d() {
    let mut assets = Assets::new();
    let mut models = vec![standing_model("Stranger Bunny", "Warrior", "char061492", "char061492")];
    let viewer = [
        resource(
            "061492",
            Category::Character,
            "char061492",
            "Zenith",
            "Stranger Bunny",
            &["061492/char061492.atlas", "061492/char061492.skel"],
        ),
        resource(
            "061492",
            Category::Dating,
            "illust_dating12",
            "Zenith",
            "Stranger Bunny",
            &[
                "061492/dating/illust_dating12.atlas",
                "061492/dating/illust_dating12.skel",
                "061492/dating/illust_dating12.png",
            ],
        ),
    ];

    let added = supplement_live2d_models_from_viewer(&mut assets, &mut models, &viewer, None, None).expect("supplement");

    assert_eq!(added, 1);
    let model = models.last().expect("model");
    assert_eq!(text_at(model, &["field"]), "interaction_live2d");
    assert_eq!(get(model, &["row_index"]), &Value::Int(7));
    assert_eq!(text_at(model, &["source"]), "bd2_l2d_viewer");
    assert_eq!(text_at(model, &["supplement_reason"]), "empty_gamekee_slot");
    assert_eq!(text_at(model, &["viewer_entry_id"]), "061492");
    assert_eq!(text_at(model, &["viewer_stem"]), "illust_dating12");
    assert!(text_at(model, &["urls", "atlas"]).ends_with("/061492/dating/illust_dating12.atlas"));
    let atlas = asset(
        &assets,
        "https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/assets/spines/061492/dating/illust_dating12.atlas",
        "live2d_atlas",
    );
    assert_eq!(
        text_at(&atlas.contexts[0], &["live2d_key"]),
        "bd2-l2d-viewer-061492-illust_dating12"
    );
    assert_eq!(text_at(&atlas.contexts[0], &["source"]), "bd2_l2d_viewer");
}

// py: tests/test_bd2.py::test_viewer_supplement_keeps_censored_as_model_variant
#[test]
fn viewer_supplement_keeps_censored_as_model_variant() {
    let mut assets = Assets::new();
    let mut models = vec![standing_model("Demon's Daughter", "Defender", "char101101", "char101101")];
    let viewer = [
        resource(
            "101101",
            Category::Character,
            "char101101",
            "Seir",
            "Demon's Daughter",
            &["101101/char101101.atlas", "101101/char101101.skel"],
        ),
        resource(
            "101101_c",
            Category::Character,
            "char101101_c",
            "Seir",
            "Demon's Daughter (Censored)",
            &["101101_c/char101101_c.atlas", "101101_c/char101101_c.skel"],
        ),
    ];

    let added = supplement_live2d_models_from_viewer(&mut assets, &mut models, &viewer, None, None).expect("supplement");

    assert_eq!(added, 1);
    let censored = models.last().expect("model");
    assert_eq!(text_at(censored, &["field"]), "standing_live2d");
    assert_eq!(text_at(censored, &["variant"]), "censored");
    assert_eq!(text_at(censored, &["supplement_reason"]), "censored_variant");
    assert_eq!(get(censored, &["style_index"]), &Value::Int(0));
    assert_eq!(get(censored, &["column_index"]), &Value::Int(1));
    assert_eq!(text_at(censored, &["viewer_entry_id"]), "101101_c");
    assert_eq!(text_at(censored, &["viewer_stem"]), "char101101_c");
}

fn named_page(chinese: &str, english: &str) -> Dict {
    style_content(
        vec![row(vec![
            text("name_label", "角色名称"),
            text("name_cn", chinese),
            text("name_en", english),
        ])],
        style_rows(),
    )
}

// py: tests/test_bd2.py::test_viewer_supplement_adds_correction_for_mismatched_gamekee_slot
#[test]
fn viewer_supplement_adds_correction_for_mismatched_gamekee_slot() {
    let mut assets = Assets::new();
    let content_json = named_page("英格利得", "Ingrid");
    let mut models = vec![standing_model("卡迪斯的子弹", "", "wrong-model", "char004202")];
    let viewer = [
        resource(
            "004202",
            Category::Character,
            "char004202",
            "Palette",
            "Miracle Violet",
            &["004202/char004202.atlas", "004202/char004202.skel"],
        ),
        resource(
            "063001",
            Category::Character,
            "char063001",
            "Ingrid",
            "Kardis' Bullet",
            &["063001/char063001.atlas", "063001/char063001.skel"],
        ),
    ];

    let added = supplement_live2d_models_from_viewer(&mut assets, &mut models, &viewer, Some(&content_json), Some(&Dict::new()))
        .expect("supplement");

    assert_eq!(added, 1);
    let correction = models.last().expect("model");
    assert_eq!(text_at(correction, &["viewer_entry_id"]), "063001");
    assert_eq!(text_at(correction, &["viewer_stem"]), "char063001");
    assert_eq!(text_at(correction, &["variant"]), "viewer_correction");
    assert_eq!(text_at(correction, &["supplement_reason"]), "mismatched_gamekee_slot");
    assert_eq!(text_at(correction, &["field"]), "standing_live2d");
    assert_eq!(get(correction, &["column_index"]), &Value::Int(1));
}

// py: tests/test_bd2.py::test_viewer_supplement_does_not_add_unpublished_same_character_costume
#[test]
fn viewer_supplement_does_not_add_unpublished_same_character_costume() {
    let mut assets = Assets::new();
    let content_json = named_page("马莫尼勒", "Mamonir");
    let mut models = vec![standing_model("死之夜", "", "mamonir-current", "char067801")];
    let viewer = [
        resource(
            "067801",
            Category::Character,
            "char067801",
            "Mamonir",
            "Night of Death",
            &["067801/char067801.atlas", "067801/char067801.skel"],
        ),
        resource(
            "067803",
            Category::Character,
            "char067803",
            "Mamonir",
            "Miracle Marine",
            &["067803/char067803.atlas", "067803/char067803.skel"],
        ),
    ];

    let added = supplement_live2d_models_from_viewer(&mut assets, &mut models, &viewer, Some(&content_json), Some(&Dict::new()))
        .expect("supplement");

    assert_eq!(added, 0);
    assert_eq!(models.len(), 1);
}

fn minigame() -> ViewerResource {
    resource(
        "minigame1",
        Category::Character,
        "RhythmHitAnim",
        "Minigame",
        "Slap Slap Pop",
        &["minigame1/RhythmHitAnim.atlas", "minigame1/RhythmHitAnim.skel"],
    )
}

// py: tests/test_bd2.py::test_viewer_extra_resources_only_archives_minigame_entries
#[test]
fn viewer_extra_resources_only_archives_minigame_entries() {
    let resources = [
        minigame(),
        resource(
            "067803",
            Category::Character,
            "char067803",
            "Mamonir",
            "Miracle Marine",
            &["067803/char067803.atlas", "067803/char067803.skel"],
        ),
    ];
    let extras = viewer_extra_resources(&resources);
    assert_eq!(
        extras.iter().map(|resource| resource.entry_id.as_str()).collect::<Vec<_>>(),
        ["minigame1"]
    );
}

// py: tests/test_bd2.py::test_archive_viewer_extra_resource_writes_extra_manifest
#[tokio::test]
async fn archive_viewer_extra_resource_writes_extra_manifest() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let crawler = offline_crawler(tmp.path());

    let root = crawler
        .archive_viewer_extra_resource(&minigame(), true)
        .await
        .expect("archive")
        .expect("a root");

    let expected_root = tmp.path().join("_extra").join("minigame1 - Minigame - Slap Slap Pop");
    assert_eq!(std::path::Path::new(&root), expected_root);
    let manifest = obj(&std::fs::read_to_string(expected_root.join("manifest.json")).expect("manifest"));
    assert_eq!(text_at(&manifest, &["collection_type"]), "extra");
    assert_eq!(text_at(&manifest, &["entry_id"]), "minigame1");
    assert_eq!(text_at(&manifest, &["viewer_resource", "stem"]), "RhythmHitAnim");
    let Value::List(models) = get(&manifest, &["live2d_models"]) else {
        panic!("models")
    };
    let model = models[0].as_dict().expect("model");
    assert_eq!(text_at(model, &["section"]), "extra");
    assert_eq!(text_at(model, &["field"]), "extra_live2d");
    assert_eq!(text_at(model, &["variant"]), "extra");
    assert_eq!(text_at(model, &["supplement_reason"]), "viewer_extra");
    assert_eq!(text_at(model, &["viewer_entry_id"]), "minigame1");
    assert_eq!(
        get(&manifest, &["asset_counts"]),
        &Value::Dict(obj(r#"{"live2d_atlas": 1, "live2d_skel": 1}"#))
    );
    let Value::List(assets) = get(&manifest, &["assets"]) else {
        panic!("assets")
    };
    let mut kinds: Vec<&str> = assets
        .iter()
        .map(|asset| text_at(asset.as_dict().expect("asset"), &["kind"]))
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(kinds, ["live2d_atlas", "live2d_skel"]);
    // `add_viewer_model_assets` is what put them there.
    let mut direct = Assets::new();
    add_viewer_model_assets(&mut direct, model);
    assert_eq!(direct.len(), 2);
}

// py: tests/test_bd2.py::test_assign_asset_paths_routes_audio_to_audio_directory
#[test]
fn assign_asset_paths_routes_audio_to_audio_directory() {
    let mut voice = Asset::new("https://cdn.example.com/voice.mp3", "audio");
    voice.contexts = vec![obj(r#"{"section": "base", "field": "voice"}"#)];
    let mut assets = Assets::new();
    assets.push(voice);

    assign_asset_paths(&mut assets, 1).expect("paths");

    assert_eq!(assets.as_slice()[0].local_path, "assets/audio/1_audio_voice_voice.mp3");
}

// ---------- assets ----------

/// A crawler over a scripted web and a fresh test database.
struct Harness {
    _database: fav_db::testdb::TestDatabase,
    db: Db,
    web: Web,
    tmp: tempfile::TempDir,
    crawler: Bd2Crawler,
}

impl Harness {
    async fn start(answer: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
        let database = crate::testutil::database().await;
        let db = database.db().clone();
        let web = Web::start(answer).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let crawler = crawler(tmp.path(), web.client(), db.clone(), Arc::new(RecordingNotifier::default()));
        Self {
            _database: database,
            db,
            web,
            tmp,
            crawler,
        }
    }

    fn root(&self) -> String {
        self.tmp.path().to_string_lossy().into_owned()
    }

    async fn sql(&self, sql: &str) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(self.db.pool())
            .await
            .expect(sql);
    }
}

fn one_asset(url: &str, kind: &str, local_path: &str) -> Assets {
    let mut asset = Asset::new(url, kind);
    asset.local_path = local_path.to_owned();
    let mut assets = Assets::new();
    assets.push(asset);
    assets
}

// py: tests/test_bd2.py::test_expand_atlas_textures_uses_bd2_blob_pipeline
#[tokio::test]
async fn expand_atlas_textures_uses_bd2_blob_pipeline() {
    let harness = Harness::start(|url| panic!("unexpected request {url}")).await;
    let atlas_text = "texture.png\nsize: 1,1\n";
    let blob = harness.tmp.path().join("_blobs/sha256/aa/atlas");
    std::fs::create_dir_all(blob.parent().expect("parent")).expect("mkdir");
    std::fs::write(&blob, atlas_text).expect("write");
    let sha = sha256_hex(atlas_text.as_bytes());
    harness
        .sql(&format!(
            "INSERT INTO bd2_assets (url, sha256, size, status) VALUES ('https://cdn.example.com/model.atlas', '{sha}', {}, 'downloaded');
             INSERT INTO bd2_blobs (sha256, size, content_type, blob_path) VALUES ('{sha}', {}, 'text/plain', '_blobs/sha256/aa/atlas');",
            atlas_text.len(),
            atlas_text.len()
        ))
        .await;
    let mut assets = one_asset(
        "https://cdn.example.com/model.atlas",
        "live2d_atlas",
        "assets/live2d/model/model.atlas",
    );
    assets.as_mut_slice()[0].contexts = vec![obj(r#"{"live2d_key": "model"}"#)];
    let mut models = vec![obj(
        r#"{"urls": {"atlas": "https://cdn.example.com/model.atlas"}, "live2d_key": "model", "field": "standing_live2d"}"#,
    )];

    harness
        .crawler
        .expand_atlas_textures(&harness.root(), &mut assets, &mut models)
        .await
        .expect("expand");

    // The atlas went through the blob store (reused, linked into place); nothing was fetched.
    assert_eq!(assets.as_slice()[0].status, "reused");
    assert!(harness.tmp.path().join("assets/live2d/model/model.atlas").is_file());
    assert!(harness.web.urls().is_empty());
    assert!(has(&assets, "https://cdn.example.com/texture.png", "live2d_texture"));
    assert_eq!(
        get(&models[0], &["urls", "atlas_textures"]),
        &Value::List(vec![s("https://cdn.example.com/texture.png")])
    );
}

// py: tests/test_bd2.py::test_expand_atlas_textures_fails_when_atlas_blob_is_unavailable
#[tokio::test]
async fn expand_atlas_textures_fails_when_atlas_blob_is_unavailable() {
    let harness = Harness::start(|_| Reply::status(404)).await;
    let mut assets = one_asset(
        "https://cdn.example.com/model.atlas",
        "live2d_atlas",
        "assets/live2d/model/model.atlas",
    );
    let mut models = vec![obj(
        r#"{"urls": {"atlas": "https://cdn.example.com/model.atlas"}, "live2d_key": "model"}"#,
    )];

    let error = harness
        .crawler
        .expand_atlas_textures(&harness.root(), &mut assets, &mut models)
        .await
        .unwrap_err();

    assert_eq!(error.class_name(), "AssetProcessingError");
    assert!(error.message().contains("Live2D atlas"), "{}", error.message());
    assert_eq!(assets.as_slice()[0].status, "failed");
}

// py: tests/test_bd2.py::test_process_asset_skips_video_during_retry_cooldown
#[tokio::test]
async fn process_asset_skips_video_during_retry_cooldown() {
    let harness = Harness::start(|url| panic!("download should not run during cooldown: {url}")).await;
    harness
        .sql(
            "INSERT INTO bd2_assets (url, status, failed_count, last_error, next_retry_at)
             VALUES ('https://cdn.example.com/broken.mp4', 'failed', 2, 'HTTP 403', NOW() + INTERVAL '2 days')",
        )
        .await;
    let mut asset = Asset::new("https://cdn.example.com/broken.mp4", "video");
    asset.local_path = "assets/videos/broken.mp4".to_owned();

    let result = harness.crawler.process_asset(&harness.root(), &mut asset).await.expect("process");

    assert!(!result);
    assert_eq!(asset.status, "failed");
    assert_eq!(asset.error, "HTTP 403");
    // `_upsert_asset_seen` touched the row and kept it failed during the cooldown.
    let row = sqlx::query("SELECT status, next_retry_at > NOW() AS waiting FROM bd2_assets")
        .fetch_one(harness.db.pool())
        .await
        .expect("row");
    assert_eq!(row.get::<String, _>("status"), "failed");
    assert!(row.get::<bool, _>("waiting"));
}

/// `_process_assets` over one asset the web answers with `status`.
async fn process_one(kind: &str, url: &'static str, status: u16) -> (Assets, Result<(), fav_runtime::source::JobError>) {
    let harness = Harness::start(move |_| Reply::status(status)).await;
    let mut assets = one_asset(url, kind, "assets/x");
    let result = harness.crawler.process_assets(&harness.root(), &mut assets).await;
    (assets, result)
}

// py: tests/test_bd2.py::test_process_assets_does_not_raise_for_failed_videos
#[tokio::test]
async fn process_assets_does_not_raise_for_failed_videos() {
    let (assets, result) = process_one("video", "https://cdn.example.com/broken.mp4", 403).await;
    result.expect("a failed video does not fail the page");
    assert_eq!(assets.as_slice()[0].status, "failed");
    assert_eq!(assets.as_slice()[0].error, "HTTP 403");
}

// py: tests/test_bd2.py::test_process_assets_does_not_raise_for_upstream_bad_gamekee_live2d_texture
#[tokio::test]
async fn process_assets_does_not_raise_for_upstream_bad_gamekee_live2d_texture() {
    let (assets, result) = process_one(
        "live2d_texture",
        "https://cdnimg-v2.gamekee.com/wiki2.0/pro/50118/live2d/50118/ujy5itu9/char000396 back2.png",
        400,
    )
    .await;
    result.expect("an upstream-missing GameKee texture does not fail the page");
    assert_eq!(assets.as_slice()[0].status, "failed");
    assert_eq!(assets.as_slice()[0].error, "HTTP 400");
}

// py: tests/test_bd2.py::test_process_assets_still_raises_for_live2d_texture_server_error
#[tokio::test]
async fn process_assets_still_raises_for_live2d_texture_server_error() {
    let (_, result) = process_one(
        "live2d_texture",
        "https://cdnimg-v2.gamekee.com/wiki2.0/pro/50118/live2d/model.png",
        500,
    )
    .await;
    let error = result.unwrap_err();
    assert_eq!(error.class_name(), "AssetProcessingError");
    assert!(error.message().starts_with("1 BD2 assets failed"), "{}", error.message());
}

// py: tests/test_bd2.py::test_process_assets_still_raises_for_failed_non_video
#[tokio::test]
async fn process_assets_still_raises_for_failed_non_video() {
    let (_, result) = process_one("image", "https://cdn.example.com/broken.png", 403).await;
    let error = result.unwrap_err();
    assert_eq!(error.class_name(), "AssetProcessingError");
    assert!(error.message().starts_with("1 BD2 assets failed"), "{}", error.message());
}

// py: tests/test_bd2.py::test_crawl_page_writes_manifest_when_video_asset_fails
#[tokio::test]
async fn crawl_page_writes_manifest_when_video_asset_fails() {
    let mut rows: Vec<Value> = (0..33).map(empty_row).collect();
    rows[0] = row(vec![
        text("costume_name_label", "Costume name"),
        text("costume_name", "Broken Video Costume"),
    ]);
    rows[1] = row(vec![
        text("costume_category_label", "Costume category"),
        text("costume_category", "Limited"),
    ]);
    rows[32] = row(vec![
        text("skill_label", "Skill effect"),
        video("skill_video", "https://cdn.example.com/broken.mp4"),
    ]);
    let content_json = style_content(Vec::new(), rows);
    let mut data = Dict::new();
    data.insert("title", s("Video Soft Failure"));
    data.insert(
        "content_json",
        Value::Str(pyjson::dumps(&Value::Dict(content_json), &pyjson::DumpOptions::python_default()).expect("JSON")),
    );
    data.insert("entry_data_bind", s(r#"{"stable-video": "skill_video"}"#));
    let mut detail = Dict::new();
    detail.insert("code", Value::Int(0));
    detail.insert("data", Value::Dict(data));
    let detail = pyjson::dumps(&Value::Dict(detail), &pyjson::DumpOptions::python_default()).expect("JSON");
    let harness = Harness::start(move |url| match url {
        "https://www.gamekee.com/v1/content/detail/1" => json_reply(&detail),
        _ => Reply::status(403),
    })
    .await;

    let root = harness
        .crawler
        .crawl_page(Some(&obj(r#"{"content_id": 1, "name": "Video Soft Failure"}"#)), 1, false)
        .await
        .expect("the page passes");

    let manifest = obj(&std::fs::read_to_string(std::path::Path::new(&root).join("manifest.json")).expect("manifest"));
    assert_eq!(get(&manifest, &["asset_counts", "video"]), &Value::Int(1));
    let Value::List(assets) = get(&manifest, &["assets"]) else {
        panic!("assets")
    };
    let first = assets[0].as_dict().expect("asset");
    assert_eq!(text_at(first, &["status"]), "failed");
    assert_eq!(text_at(first, &["error"]), "HTTP 403");
    assert!(std::path::Path::new(&root).join("character.json").is_file());
    // The page was completed with its assets.
    let completed = sqlx::query("SELECT completed_at IS NOT NULL AS done FROM bd2_pages WHERE content_id = 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("page");
    assert!(completed.get::<bool, _>("done"));
    let page_assets = sqlx::query("SELECT count(*) AS n FROM bd2_page_assets WHERE content_id = 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("count");
    assert_eq!(page_assets.get::<i64, _>("n"), 1);
}

// ---------- the database ----------

// py: tests/test_bd2.py::test_replace_page_assets_and_complete_uses_bd2_tables
#[tokio::test]
async fn replace_page_assets_and_complete_uses_bd2_tables() {
    let harness = Harness::start(|url| panic!("unexpected request {url}")).await;
    harness
        .sql(
            "INSERT INTO bd2_pages (content_id, name) VALUES (1, 'Name');
             INSERT INTO bd2_assets (url) VALUES ('https://cdn.example.com/a.png'), ('https://cdn.example.com/old.png');
             INSERT INTO bd2_page_assets (content_id, url, kind, context_hash) VALUES (1, 'https://cdn.example.com/old.png', 'image', 'x');",
        )
        .await;
    let mut assets = one_asset("https://cdn.example.com/a.png", "image", "assets/images/a.png");
    assets.as_mut_slice()[0].contexts = vec![obj(r#"{"field": "costume_sprite"}"#)];

    harness
        .crawler
        .replace_page_assets_and_mark_completed(1, &assets)
        .await
        .expect("replace");

    let rows = sqlx::query("SELECT url, kind, local_path, original_filename, context_json::text AS context FROM bd2_page_assets")
        .fetch_all(harness.db.pool())
        .await
        .expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<String, _>("url"), "https://cdn.example.com/a.png");
    assert_eq!(rows[0].get::<String, _>("local_path"), "assets/images/a.png");
    assert_eq!(rows[0].get::<String, _>("original_filename"), "a.png");
    assert_eq!(rows[0].get::<String, _>("context"), r#"{"field": "costume_sprite"}"#);
    let page = sqlx::query("SELECT completed_at IS NOT NULL AS done FROM bd2_pages WHERE content_id = 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("page");
    assert!(page.get::<bool, _>("done"));
}

// py: tests/test_bd2.py::test_upsert_page_fetch_start_preserves_previous_completed_at
#[tokio::test]
async fn upsert_page_fetch_start_preserves_previous_completed_at() {
    let harness = Harness::start(|url| panic!("unexpected request {url}")).await;
    harness
        .sql("INSERT INTO bd2_pages (content_id, name, completed_at) VALUES (1, 'Old', '2026-01-02T03:04:05Z')")
        .await;

    harness
        .crawler
        .upsert_page_fetch_start(
            1,
            "Name",
            Some(&obj(r#"{"content_id": 1}"#)),
            &obj(r#"{"data": {"content_json": "{}"}}"#),
            &format!("{}/manifest.json", harness.root()),
            "hash",
        )
        .await
        .expect("upsert");

    let row = sqlx::query("SELECT name, content_hash, completed_at = '2026-01-02T03:04:05Z' AS kept FROM bd2_pages WHERE content_id = 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("row");
    assert_eq!(row.get::<String, _>("name"), "Name");
    assert_eq!(row.get::<String, _>("content_hash"), "hash");
    assert!(row.get::<bool, _>("kept"));
}

// py: tests/test_bd2.py::test_upsert_tree_pages_can_skip_deactivating_missing_pages
#[tokio::test]
async fn upsert_tree_pages_can_skip_deactivating_missing_pages() {
    let harness = Harness::start(|url| panic!("unexpected request {url}")).await;
    harness
        .sql("INSERT INTO bd2_pages (content_id, name, active) VALUES (2, 'Other', TRUE)")
        .await;

    harness
        .crawler
        .upsert_tree_pages(&[obj(r#"{"content_id": 1, "name": "Name"}"#)], false)
        .await
        .expect("upsert");

    let rows = sqlx::query("SELECT content_id, active FROM bd2_pages ORDER BY content_id")
        .fetch_all(harness.db.pool())
        .await
        .expect("rows");
    let pages: Vec<(i64, bool)> = rows.iter().map(|row| (row.get("content_id"), row.get("active"))).collect();
    assert_eq!(pages, [(1, true), (2, true)]);

    // With deactivation, a page missing from the tree is marked inactive.
    harness
        .crawler
        .upsert_tree_pages(&[obj(r#"{"content_id": 1, "name": "Name"}"#)], true)
        .await
        .expect("upsert");
    let inactive = sqlx::query("SELECT active FROM bd2_pages WHERE content_id = 2")
        .fetch_one(harness.db.pool())
        .await
        .expect("row");
    assert!(!inactive.get::<bool, _>("active"));
}

// ---------- runs ----------

// py: tests/test_bd2.py::test_download_character_raises_when_bd2_lock_is_held
#[tokio::test]
async fn download_character_raises_when_bd2_lock_is_held() {
    let harness = Harness::start(|url| panic!("unexpected request {url}")).await;
    let held = harness.db.try_advisory_lock("bd2").await.expect("lock").expect("acquired");

    let error = harness.crawler.download_character("1", true).await.unwrap_err();

    assert_eq!(error.class_name(), "RuntimeError");
    assert!(error.message().contains("advisory lock"));
    held.release().await;
}

const TREE: &str = r#"{"code": 0, "data": [{"id": 122323, "name": "5-star", "child": [
    {"id": 1, "name": "First", "content_id": 1, "sort": 1},
    {"id": 2, "name": "Second", "content_id": 2, "sort": 2}
]}]}"#;

// py: tests/test_bd2.py::test_update_reports_failures_after_attempting_remaining_pages
#[tokio::test]
async fn update_reports_failures_after_attempting_remaining_pages() {
    let harness = Harness::start(|url| match url {
        "https://www.gamekee.com/v1/entry/treesByPid?pid=194491" => json_reply(TREE),
        "https://www.gamekee.com/v1/content/detail/2" => {
            json_reply(r#"{"code": 0, "data": {"title": "Second", "content_json": "{\"styleData\": []}"}}"#)
        }
        _ => Reply::status(404),
    })
    .await;

    let error = harness.crawler.update().await.unwrap_err();

    assert_eq!(error.class_name(), "CrawlRunError");
    assert_eq!(error.message(), "1 BD2 pages failed: 1");
    let details: Vec<String> = harness
        .web
        .urls()
        .into_iter()
        .filter(|url| url.contains("/v1/content/detail/"))
        .collect();
    assert_eq!(
        details,
        [
            "https://www.gamekee.com/v1/content/detail/1",
            "https://www.gamekee.com/v1/content/detail/2"
        ]
    );
    // The lock was released: another run can take it.
    let lock = harness.db.try_advisory_lock("bd2").await.expect("lock").expect("free again");
    lock.release().await;
}

// ---------- notifications ----------

fn justia(costumes: &str) -> Dict {
    obj(&format!(r#"{{"content_id": 202, "title": "Justia", "costumes": {costumes}}}"#))
}

// py: tests/test_game_content_notifications.py::test_bd2_existing_character_notification_includes_new_skin_name
#[tokio::test]
async fn bd2_existing_character_notification_includes_new_skin_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let notifier = Arc::new(RecordingNotifier::default());
    let crawler = crawler(
        tmp.path(),
        HttpClient::new(&client_options()).expect("client"),
        unused_db(),
        Arc::clone(&notifier),
    );
    crawler.notify_discoveries.store(true, std::sync::atomic::Ordering::Relaxed);
    let previous = CharacterSnapshot {
        exists: true,
        character: Some(obj(r#"{"costumes": [{"style_name": "office", "title": "Office Worker"}]}"#)),
    };

    crawler
        .notify_content_discovery(
            &previous,
            &justia(r#"[{"style_name": "office", "title": "Office Worker"}, {"style_name": "summer", "title": "Summer Knight"}]"#),
        )
        .await;

    let sent = notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].header, "BD2");
    assert_eq!(sent[0].title, "New skin: Justia");
    assert_eq!(sent[0].body, "Character: Justia\nSkin: Summer Knight");
    assert_eq!(sent[0].link_url, "https://www.gamekee.com/zsca2/tj/202.html");
}

// py: tests/test_game_content_notifications.py::test_discovery_notifications_are_suppressed_without_an_existing_archive_baseline
#[tokio::test]
async fn discovery_notifications_are_suppressed_without_an_existing_archive_baseline() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let notifier = Arc::new(RecordingNotifier::default());
    let crawler = crawler(
        tmp.path(),
        HttpClient::new(&client_options()).expect("client"),
        unused_db(),
        Arc::clone(&notifier),
    );

    crawler
        .notify_content_discovery(
            &CharacterSnapshot {
                exists: false,
                character: None,
            },
            &justia(r#"[{"style_name": "default", "title": "Default"}]"#),
        )
        .await;

    assert!(notifier.sent().is_empty());
}
