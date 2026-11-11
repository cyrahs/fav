use std::collections::BTreeSet;
use std::sync::Arc;

use fav_core::pyjson::{self, Dict, Value};
use fav_core::settings::Settings;
use serde_json::json;

use super::*;

// ---------- fixtures ----------

pub(crate) fn dict(value: &serde_json::Value) -> Dict {
    match Value::from_serde(value) {
        Value::Dict(dict) => dict,
        other => panic!("not a dict: {other:?}"),
    }
}

fn get<'a>(dict: &'a Dict, key: &str) -> &'a Value {
    dict.get(key).unwrap_or_else(|| panic!("missing {key}"))
}

fn list<'a>(dict: &'a Dict, key: &str) -> &'a [Value] {
    match get(dict, key) {
        Value::List(items) => items,
        other => panic!("{key} is not a list: {other:?}"),
    }
}

fn entries<'a>(dict: &'a Dict, key: &str) -> Vec<&'a Dict> {
    list(dict, key)
        .iter()
        .map(|item| match item {
            Value::Dict(item) => item,
            other => panic!("not a dict: {other:?}"),
        })
        .collect()
}

/// `{item[field] for item in report[key]}` as `repr` strings.
fn field_set(dict: &Dict, key: &str, field: &str) -> BTreeSet<String> {
    entries(dict, key).into_iter().map(|item| repr_value(get(item, field))).collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| fav_runtime::pystr::repr_str(item)).collect()
}

fn int_set(items: &[i64]) -> BTreeSet<String> {
    items.iter().map(ToString::to_string).collect()
}

pub(crate) fn layered_model(stable_id: &str, live2d_key: &str, skin_index: i64) -> Dict {
    dict(&json!({
        "label": "live2d(full)",
        "section": "style",
        "skin_index": skin_index,
        "stable_id": stable_id,
        "live2d_key": live2d_key,
        "urls": {
            "atlas": format!("https://cdn.example.com/{live2d_key}/model.atlas"),
            "skel": format!("https://cdn.example.com/{live2d_key}/model.skel"),
        },
    }))
}

fn with_fields(mut model: Dict, fields: &serde_json::Value) -> Dict {
    for (key, value) in dict(fields).iter() {
        model.insert(key.clone(), value.clone());
    }
    model
}

/// Two models carrying stale layer fields from an earlier capture.
fn stale_models() -> Vec<Dict> {
    vec![
        with_fields(
            layered_model("stable-main", "main", 0),
            &json!({"layer_order": 7, "source_z_index": 99, "source_layer_index": 4, "is_primary": true,
                    "layer_match_method": "stale", "layer_match_confidence": "high"}),
        ),
        with_fields(
            layered_model("stable-back", "back", 0),
            &json!({"layer_order": 8, "source_z_index": 98, "source_layer_index": 5, "is_primary": false,
                    "layer_match_method": "stale", "layer_match_confidence": "high"}),
        ),
    ]
}

fn capture_layer(stable_id: &str, live2d_key: &str, order: i64, z: i64, index: i64, primary: bool) -> serde_json::Value {
    json!({
        "stable_id": stable_id,
        "live2d_key": live2d_key,
        "layer_order": order,
        "source_z_index": z,
        "source_layer_index": index,
        "is_primary": primary,
        "layer_match_confidence": "high",
    })
}

fn capture(layers: &[serde_json::Value]) -> Dict {
    dict(&json!({"content_id": 711_133, "layers": layers}))
}

fn has_no_layer_fields(models: &[Dict]) -> bool {
    models
        .iter()
        .all(|model| LIVE2D_LAYER_METADATA_FIELDS.iter().all(|field| !model.contains_key(field)))
}

fn int(dict: &Dict, key: &str) -> i128 {
    match get(dict, key) {
        Value::Int(value) => *value,
        other => panic!("{key} is not an int: {other:?}"),
    }
}

// ---------- registration ----------

// py: tests/test_nikke.py::test_nikke_config_defaults_to_disabled_collection_nikke_path
#[test]
fn nikke_config_defaults_to_disabled_collection_nikke_path() {
    let cfg = Settings::default().web.nikke;
    assert!(!cfg.schedule.enabled);
    assert_eq!(cfg.schedule.cron, "0 */6 * * *");
    assert_eq!(cfg.path.as_str(), "collection/nikke");
    assert!(!cfg.runtime_capture_enabled);
    assert!((cfg.runtime_capture_timeout_seconds - 60.0).abs() < f64::EPSILON);
    assert!(!cfg.runtime_capture_force_refresh);
}

// py: tests/test_nikke.py::test_scheduler_registration_includes_nikke
#[tokio::test]
async fn scheduler_registration_includes_nikke() {
    let mut settings = Settings::default();
    settings.web.azurlane.schedule.enabled = true;
    settings.web.bd2.schedule.enabled = true;
    settings.web.bilibili.schedule.enabled = true;
    settings.web.hanime1.schedule.enabled = true;
    settings.web.jandan.schedule.enabled = true;
    settings.web.stellasora.schedule.enabled = true;
    settings.web.telegram.schedule.enabled = true;
    settings.web.nikke.schedule.enabled = false;

    let jobs = fav_runtime::jobs::build_jobs(&settings);
    let job = jobs.iter().find(|job| job.key == "nikke").expect("a nikke job");

    assert_eq!(job.name, "Nikke");
    assert!(!job.enabled);
    assert!(job.required_commands.is_empty());
    let db = fav_db::Db::connect_lazy("postgresql://tests@127.0.0.1:1/unused").expect("lazy pool");
    let ctx = crate::SourceContext {
        settings: Arc::new(fav_db::SettingsStore::new(db.clone())),
        db,
    };
    assert!(crate::registry(&ctx).contains(JOB_KEY));
}

// py: tests/test_nikke.py::test_nikke_module_reexports_layer_metadata_helpers_for_compatibility
#[test]
fn nikke_module_reexports_layer_metadata_helpers_for_compatibility() {
    type Merge = fn(&mut [Dict], &Dict, Option<i128>, bool) -> Dict;
    type Validate = fn(&[Dict]) -> Vec<Value>;
    type MergeFiles = fn(&std::path::Path, &Dict, bool) -> Result<Dict, PyError>;
    type Strip = fn(&std::path::Path, bool) -> Result<Dict, PyError>;
    assert!(std::ptr::fn_addr_eq(
        merge_live2d_layer_captures as Merge,
        crate::nikke_layer_metadata::merge_live2d_layer_captures as Merge
    ));
    assert!(std::ptr::fn_addr_eq(
        validate_live2d_layer_metadata as Validate,
        crate::nikke_layer_metadata::validate_live2d_layer_metadata as Validate
    ));
    assert!(std::ptr::fn_addr_eq(
        merge_layer_capture_files as MergeFiles,
        crate::nikke_layer_metadata::merge_layer_capture_files as MergeFiles
    ));
    assert!(std::ptr::fn_addr_eq(
        strip_layer_metadata_files as Strip,
        crate::nikke_layer_metadata::strip_layer_metadata_files as Strip
    ));
}

// ---------- page parsing ----------

// py: tests/test_nikke.py::test_filter_nikke_rows_keeps_only_wrapped_rows_with_content_id
#[test]
fn filter_nikke_rows_keeps_only_wrapped_rows_with_content_id() {
    let rows = Value::from_serde(&json!([
        {"nikke": {"content_id": "691899", "title": "海伦"}},
        {"stellasora": {"content_id": 1}},
        {"nikke": {"content_id": "not-an-id", "title": "bad"}},
        {"nikke": {"id": 171_728, "title": "薇尔维特"}},
    ]));

    let filtered = filter_nikke_rows(Some(&rows));

    let titles: Vec<&Value> = filtered.iter().map(|row| get(row, "title")).collect();
    assert_eq!(titles, [&Value::from("海伦"), &Value::from("薇尔维特")]);
}

// py: tests/test_nikke.py::test_extract_resources_uses_bound_media_gate
#[test]
fn extract_resources_uses_bound_media_gate() {
    let content_json = dict(&json!({
        "baseData": [
            [
                {"type": "text", "value": "头像"},
                {"type": "image", "key": "bound_image", "value": "/media/bound.png"},
                {"type": "image", "key": "unbound_image", "value": "/media/unbound.png"},
            ],
            [
                {"type": "text", "value": "视频"},
                {"type": "video", "key": "bound_video", "value": "https://cdn.example.com/movie.mp4"},
            ],
            [
                {"type": "text", "value": "模型"},
                {
                    "type": "live2d",
                    "key": "bound_model",
                    "value": {
                        "live2dKey": "model-a",
                        "atlas": "/spine/model.atlas",
                        "skel": "/spine/model.skel",
                        "image": "/spine/model.png",
                        "layerOrder": "2",
                        "sourceZIndex": 9,
                        "sourceLayerIndex": 0,
                        "isPrimary": true,
                        "layerMatchMethod": "gamekee-player-init",
                        "layerMatchConfidence": "high",
                    },
                },
            ],
        ],
    }));
    let reverse_bind: ReverseBind = [
        ("bound_image", "stable-image"),
        ("bound_video", "stable-video"),
        ("bound_model", "stable-model"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    let row = dict(&json!({"icon": "/icons/face.png"}));

    let (assets, models) = extract_resources(&content_json, Some(&row), &reverse_bind);

    let urls: BTreeSet<&str> = assets.iter().map(|asset| asset.url.as_str()).collect();
    for expected in [
        "https://www.gamekee.com/media/bound.png",
        "https://cdn.example.com/movie.mp4",
        "https://www.gamekee.com/spine/model.atlas",
        "https://www.gamekee.com/spine/model.skel",
        "https://www.gamekee.com/spine/model.png",
        "https://www.gamekee.com/icons/face.png",
    ] {
        assert!(urls.contains(expected), "{expected} missing from {urls:?}");
    }
    assert!(!urls.contains("https://www.gamekee.com/media/unbound.png"));
    let model = &models[0];
    assert_eq!(get(model, "live2d_key"), &Value::from("model-a"));
    assert_eq!(get(model, "layer_order"), &Value::Int(2));
    assert_eq!(get(model, "source_z_index"), &Value::Int(9));
    assert_eq!(get(model, "source_layer_index"), &Value::Int(0));
    assert_eq!(get(model, "is_primary"), &Value::Bool(true));
    assert_eq!(get(model, "layer_match_method"), &Value::from("gamekee-player-init"));
    assert_eq!(get(model, "layer_match_confidence"), &Value::from("high"));
}

// py: tests/test_nikke.py::test_add_asset_preserves_same_url_different_kinds
#[test]
fn add_asset_preserves_same_url_different_kinds() {
    let mut assets = Assets::default();

    add_asset(
        &mut assets,
        "https://cdn.example.com/shared.png",
        "image",
        dict(&json!({"field": "icon"})),
    );
    add_asset(
        &mut assets,
        "https://cdn.example.com/shared.png",
        "live2d_texture",
        dict(&json!({"live2d_field": "image"})),
    );

    let keys: BTreeSet<(&str, &str)> = assets.iter().map(|asset| (asset.url.as_str(), asset.kind.as_str())).collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            ("https://cdn.example.com/shared.png", "image"),
            ("https://cdn.example.com/shared.png", "live2d_texture"),
        ])
    );
    let mut kinds: Vec<&str> = assets.iter().map(|asset| asset.kind.as_str()).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, ["image", "live2d_texture"]);
}

// py: tests/test_nikke.py::test_parse_spine_atlas_textures_dedupes_texture_lines
#[test]
fn parse_spine_atlas_textures_dedupes_texture_lines() {
    let atlas_text = "\n\u{feff}model.png\nsize: 1024,1024\nformat: RGBA8888\nregion\n  rotate: false\neffect.webp\nmodel.png\n";

    assert_eq!(parse_spine_atlas_textures(atlas_text), ["model.png", "effect.webp"]);
}

// py: tests/test_nikke.py::test_validate_asset_response_rejects_empty_and_html
#[test]
fn validate_asset_response_rejects_empty_and_html() {
    assert_eq!(validate_asset_response("image", "image/png", b"", 0), "empty response body");
    assert_eq!(
        validate_asset_response("image", "text/html; charset=utf-8", b"<!doctype html><html>Forbidden</html>", 42),
        "cdn rejection response"
    );
}

// py: tests/test_nikke.py::test_assign_asset_paths_splits_live2d_dir_on_same_basename_collision
#[test]
fn assign_asset_paths_splits_live2d_dir_on_same_basename_collision() {
    let mut assets = Assets::default();
    for url in ["https://cdn.example.com/a/model.atlas", "https://cdn.example.com/b/model.atlas"] {
        assets.entry(url.to_owned(), "live2d_atlas").contexts = vec![dict(&json!({"live2d_key": "same-model"}))];
    }

    assign_asset_paths(&mut assets, 1).expect("paths");
    let paths: Vec<&str> = assets.iter().map(|asset| asset.local_path.as_str()).collect();

    assert_eq!(paths[0], "assets/live2d/same-model/model.atlas");
    assert!(paths[1].starts_with("assets/live2d/same-model-"), "{}", paths[1]);
    assert!(paths[1].ends_with("/model.atlas"), "{}", paths[1]);
}

// ---------- layer captures ----------

// py: tests/test_nikke.py::test_merge_live2d_layer_captures_projects_dry_run_quality
#[test]
fn merge_live2d_layer_captures_projects_dry_run_quality() {
    let mut models = vec![
        layered_model("stable-main", "main", 0),
        layered_model("stable-front", "front", 0),
        layered_model("stable-back", "back", 0),
    ];
    let capture = dict(&json!({
        "content_id": 711_133,
        "layers": [
            {"stable_id": "stable-main", "live2dKey": "main", "sourceZIndex": 9, "sourceLayerIndex": 0,
             "isPrimary": true, "layerMatchConfidence": "high"},
            {"stable_id": "stable-front", "live2dKey": "front", "sourceZIndex": 10, "sourceLayerIndex": 1,
             "isPrimary": false, "layerMatchConfidence": "high"},
            {"stable_id": "stable-back", "live2dKey": "back", "sourceZIndex": 8, "sourceLayerIndex": 2,
             "isPrimary": false, "layerMatchConfidence": "high"},
        ],
    }));

    let dry_run_report = merge_live2d_layer_captures(&mut models, &capture, Some(711_133), true);

    assert_eq!(int(&dry_run_report, "changed"), 3);
    assert!(list(&dry_run_report, "quality_issues").is_empty());
    assert!(models.iter().all(|model| !model.contains_key("layer_order")));

    let write_report = merge_live2d_layer_captures(&mut models, &capture, Some(711_133), false);

    assert!(list(&write_report, "quality_issues").is_empty());
    let projected: Vec<(Value, Value, Value, Value, Value)> = models
        .iter()
        .map(|model| {
            (
                get(model, "stable_id").clone(),
                get(model, "layer_order").clone(),
                get(model, "source_z_index").clone(),
                get(model, "source_layer_index").clone(),
                get(model, "is_primary").clone(),
            )
        })
        .collect();
    assert_eq!(
        projected,
        [
            (
                Value::from("stable-main"),
                Value::Int(2),
                Value::Int(9),
                Value::Int(0),
                Value::Bool(true)
            ),
            (
                Value::from("stable-front"),
                Value::Int(3),
                Value::Int(10),
                Value::Int(1),
                Value::Bool(false)
            ),
            (
                Value::from("stable-back"),
                Value::Int(1),
                Value::Int(8),
                Value::Int(2),
                Value::Bool(false)
            ),
        ]
    );
    let methods: BTreeSet<String> = models.iter().map(|model| repr_value(get(model, "layer_match_method"))).collect();
    assert_eq!(methods, set(&["gamekee-runtime-container"]));
    let confidences: BTreeSet<String> = models
        .iter()
        .map(|model| repr_value(get(model, "layer_match_confidence")))
        .collect();
    assert_eq!(confidences, set(&["high"]));
}

// py: tests/test_nikke.py::test_merge_live2d_layer_captures_treats_missing_confidence_as_low
#[test]
fn merge_live2d_layer_captures_treats_missing_confidence_as_low() {
    let mut models = vec![layered_model("stable-main", "main", 0), layered_model("stable-back", "back", 0)];
    let capture = dict(&json!({
        "content_id": 711_133,
        "layers": [
            {"stable_id": "stable-main", "live2d_key": "main", "layer_order": 2, "source_z_index": 9,
             "source_layer_index": 0, "is_primary": true},
            {"stable_id": "stable-back", "live2d_key": "back", "layer_order": 1, "source_z_index": 8,
             "source_layer_index": 1, "is_primary": false},
        ],
    }));

    let report = merge_live2d_layer_captures(&mut models, &capture, Some(711_133), false);

    assert_eq!(field_set(&report, "quality_issues", "code"), set(&["low_confidence_layer_match"]));
    assert_eq!(field_set(&report, "quality_issues", "skin_index"), int_set(&[0]));
    assert_eq!(field_set(&report, "quality_issues", "model_kind"), set(&["full"]));
    assert_eq!(int(&report, "changed"), 0);
    assert_eq!(field_set(&report, "blocked", "reason"), set(&["quality_gates_failed"]));
    assert_eq!(field_set(&report, "blocked", "skin_index"), int_set(&[0]));
    assert_eq!(field_set(&report, "blocked", "model_kind"), set(&["full"]));
    assert!(
        entries(&report, "blocked")
            .iter()
            .all(|item| list(item, "fields").contains(&Value::from("layer_order")))
    );
    assert!(has_no_layer_fields(&models));
}

// py: tests/test_nikke.py::test_merge_live2d_layer_captures_commits_only_valid_skin_groups
#[test]
fn merge_live2d_layer_captures_commits_only_valid_skin_groups() {
    let mut models = vec![
        layered_model("skin0-main", "skin0-main", 0),
        layered_model("skin0-back", "skin0-back", 0),
        layered_model("skin1-main", "skin1-main", 1),
        layered_model("skin1-back", "skin1-back", 1),
    ];
    let capture = capture(&[
        capture_layer("skin0-main", "skin0-main", 2, 9, 0, true),
        capture_layer("skin0-back", "skin0-back", 1, 8, 1, false),
        capture_layer("skin1-main", "skin1-main", 1, 9, 0, true),
        capture_layer("skin1-back", "skin1-back", 1, 8, 1, false),
    ]);

    let report = merge_live2d_layer_captures(&mut models, &capture, Some(711_133), false);

    assert_eq!(int(&report, "changed"), 2);
    assert_eq!(field_set(&report, "quality_issues", "code"), set(&["duplicate_layer_order"]));
    assert_eq!(field_set(&report, "quality_issues", "model_kind"), set(&["full"]));
    assert_eq!(field_set(&report, "blocked", "skin_index"), int_set(&[1]));
    assert_eq!(field_set(&report, "blocked", "model_kind"), set(&["full"]));
    assert!(
        entries(&report, "blocked")
            .iter()
            .all(|item| list(item, "fields").contains(&Value::from("layer_order")))
    );
    assert_eq!(get(&models[0], "layer_order"), &Value::Int(2));
    assert_eq!(get(&models[1], "layer_order"), &Value::Int(1));
    assert!(has_no_layer_fields(&models[2..]));
}

// py: tests/test_nikke.py::test_merge_live2d_layer_captures_clears_existing_fields_for_blocked_skin_group
#[test]
fn merge_live2d_layer_captures_clears_existing_fields_for_blocked_skin_group() {
    let mut models = stale_models();
    let capture = capture(&[
        capture_layer("stable-main", "main", 1, 9, 0, true),
        capture_layer("stable-back", "back", 1, 8, 1, false),
    ]);

    let report = merge_live2d_layer_captures(&mut models, &capture, Some(711_133), false);

    assert_eq!(field_set(&report, "quality_issues", "code"), set(&["duplicate_layer_order"]));
    assert!(!list(&report, "blocked").is_empty());
    assert!(has_no_layer_fields(&models));
}

// py: tests/test_nikke.py::test_merge_live2d_layer_captures_clears_uncaptured_siblings_in_blocked_skin_group
#[test]
fn merge_live2d_layer_captures_clears_uncaptured_siblings_in_blocked_skin_group() {
    let mut models = stale_models();
    let mut low = capture_layer("stable-main", "main", 1, 9, 0, true);
    low["layer_match_confidence"] = json!("low");
    let capture = capture(&[low]);

    let report = merge_live2d_layer_captures(&mut models, &capture, Some(711_133), false);

    assert_eq!(
        field_set(&report, "quality_issues", "code"),
        set(&["low_confidence_layer_match", "missing_capture_layer"])
    );
    assert_eq!(field_set(&report, "blocked", "stable_id"), set(&["stable-main"]));
    assert!(has_no_layer_fields(&models));
}

// py: tests/test_nikke.py::test_merge_live2d_layer_captures_clears_stale_group_when_capture_omits_sibling
#[test]
fn merge_live2d_layer_captures_clears_stale_group_when_capture_omits_sibling() {
    let mut models = stale_models();
    let capture = capture(&[capture_layer("stable-main", "main", 1, 9, 0, true)]);

    let report = merge_live2d_layer_captures(&mut models, &capture, Some(711_133), false);

    assert_eq!(field_set(&report, "quality_issues", "code"), set(&["missing_capture_layer"]));
    assert_eq!(
        list(entries(&report, "quality_issues")[0], "models"),
        [Value::from("stable-back/back")]
    );
    assert_eq!(field_set(&report, "blocked", "stable_id"), set(&["stable-main"]));
    assert!(has_no_layer_fields(&models));
}

// py: tests/test_nikke.py::test_merge_live2d_layer_captures_clears_stale_group_when_matched_capture_is_incomplete
#[test]
fn merge_live2d_layer_captures_clears_stale_group_when_matched_capture_is_incomplete() {
    let mut models = stale_models();
    let capture = dict(&json!({
        "content_id": 711_133,
        "layers": [
            {"stable_id": "stable-main", "live2d_key": "main", "source_z_index": 9, "source_layer_index": 0,
             "layer_match_confidence": "high"},
        ],
    }));

    let report = merge_live2d_layer_captures(&mut models, &capture, Some(711_133), false);

    assert!(list(&report, "quality_issues").is_empty());
    assert!(list(&report, "blocked").is_empty());
    let skipped = entries(&report, "skipped")[0];
    assert_eq!(get(skipped, "reason"), &Value::from("incomplete_layer_metadata"));
    assert_eq!(get(skipped, "skin_index"), &Value::Int(0));
    assert_eq!(get(skipped, "model_kind"), &Value::from("full"));
    assert!(has_no_layer_fields(&models));
}

// py: tests/test_nikke.py::test_merge_live2d_layer_captures_skips_ambiguous_url_only_capture
#[test]
fn merge_live2d_layer_captures_skips_ambiguous_url_only_capture() {
    let shared = json!({"atlas": "https://cdn.example.com/shared/model.atlas"});
    let mut models = vec![
        with_fields(layered_model("stable-a", "a", 0), &json!({"urls": shared})),
        with_fields(layered_model("stable-b", "b", 0), &json!({"urls": shared})),
    ];
    let capture = dict(&json!({
        "content_id": 711_133,
        "layers": [{"resource_urls": shared, "layer_order": 1, "is_primary": true}],
    }));

    let report = merge_live2d_layer_captures(&mut models, &capture, Some(711_133), false);

    assert_eq!(int(&report, "changed"), 0);
    assert_eq!(get(entries(&report, "skipped")[0], "reason"), &Value::from("ambiguous_match"));
    assert!(models.iter().all(|model| !model.contains_key("layer_order")));
}

// py: tests/test_nikke.py::test_validate_live2d_layer_metadata_flags_incomplete_multi_full
#[test]
fn validate_live2d_layer_metadata_flags_incomplete_multi_full() {
    let models = vec![
        with_fields(
            layered_model("stable-a", "a", 0),
            &json!({"layer_order": 1, "is_primary": true, "layer_match_confidence": "high"}),
        ),
        layered_model("stable-b", "b", 0),
    ];

    let issues = validate_live2d_layer_metadata(&models);

    let codes: BTreeSet<String> = issues
        .iter()
        .map(|issue| match issue {
            Value::Dict(issue) => repr_value(get(issue, "code")),
            other => panic!("not a dict: {other:?}"),
        })
        .collect();
    assert_eq!(codes, set(&["missing_layer_order", "low_confidence_layer_match"]));
}

fn write_page(root: &std::path::Path, models: &[Dict]) {
    std::fs::create_dir_all(root).expect("root");
    let models = Value::List(models.iter().cloned().map(Value::Dict).collect());
    for name in ["manifest.json", "character.json"] {
        let mut document = dict(&json!({"content_id": 711_133, "title": "Test"}));
        document.insert("live2d_models", models.clone());
        std::fs::write(
            root.join(name),
            pyjson::dumps(&Value::Dict(document), &pyjson::DumpOptions::python_default()).expect("dumps"),
        )
        .expect("write");
    }
}

fn read_document(path: &std::path::Path) -> Dict {
    match pyjson::loads(&std::fs::read_to_string(path).expect("read")).expect("JSON") {
        Value::Dict(document) => document,
        other => panic!("not an object: {other:?}"),
    }
}

fn model_at(document: &Dict, index: usize) -> &Dict {
    entries(document, "live2d_models")[index]
}

fn sub<'a>(dict: &'a Dict, key: &str) -> &'a Dict {
    match get(dict, key) {
        Value::Dict(inner) => inner,
        other => panic!("{key} is not a dict: {other:?}"),
    }
}

// py: tests/test_nikke.py::test_merge_layer_capture_files_updates_manifest_and_character
#[test]
fn merge_layer_capture_files_updates_manifest_and_character() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("711133 - Test");
    write_page(
        &root,
        &[layered_model("stable-main", "main", 0), layered_model("stable-back", "back", 0)],
    );
    let capture = dict(&json!({
        "content_id": 711_133,
        "status": "reused-sentinel",
        "fingerprint": "payload-fingerprint-sentinel",
        "layers": [
            {"stable_id": "stable-main", "live2d_key": "main", "layer_order": 2, "source_z_index": 9,
             "is_primary": true, "layer_match_confidence": "high"},
            {"stable_id": "stable-back", "live2d_key": "back", "layer_order": 1, "source_z_index": 8,
             "is_primary": false, "layer_match_confidence": "high"},
        ],
    }));

    let report = merge_layer_capture_files(&root, &capture, false).expect("merge");

    assert!(list(sub(&report, "manifest"), "quality_issues").is_empty());
    let saved_manifest = read_document(&root.join("manifest.json"));
    let saved_character = read_document(&root.join("character.json"));
    assert_eq!(get(model_at(&saved_manifest, 0), "layer_order"), &Value::Int(2));
    assert_eq!(get(model_at(&saved_character, 1), "is_primary"), &Value::Bool(false));
    let summary = sub(&saved_manifest, "live2d_layer_capture");
    assert_eq!(get(summary, "layer_count"), &Value::Int(2));
    assert_eq!(get(summary, "status"), &Value::from("reused-sentinel"));
    assert_eq!(get(summary, "fingerprint"), &Value::from("payload-fingerprint-sentinel"));
    assert!(get(summary, "capture_hash").is_truthy());

    let strip_report = strip_layer_metadata_files(&root, false).expect("strip");

    let stripped_manifest = read_document(&root.join("manifest.json"));
    assert_eq!(
        sub(&strip_report, "capture_summary"),
        &dict(&json!({"manifest_removed": true, "character_removed": true}))
    );
    assert!(!stripped_manifest.contains_key("live2d_layer_capture"));
    assert!(!model_at(&stripped_manifest, 0).contains_key("layer_order"));
}

// py: tests/test_nikke.py::test_merge_layer_capture_files_marks_summary_incomplete_when_quality_gates_fail
#[test]
fn merge_layer_capture_files_marks_summary_incomplete_when_quality_gates_fail() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("711133 - Test");
    write_page(
        &root,
        &[layered_model("stable-main", "main", 0), layered_model("stable-back", "back", 0)],
    );
    let capture = dict(&json!({
        "content_id": 711_133,
        "status": "success",
        "fingerprint": "payload-fingerprint-sentinel",
        "layers": [
            {"stable_id": "stable-main", "live2d_key": "main", "layer_order": 1, "source_z_index": 9, "is_primary": true},
            {"stable_id": "stable-back", "live2d_key": "back", "layer_order": 1, "source_z_index": 8, "is_primary": false},
        ],
    }));

    let report = merge_layer_capture_files(&root, &capture, false).expect("merge");

    let saved_manifest = read_document(&root.join("manifest.json"));
    let saved_character = read_document(&root.join("character.json"));
    assert!(get(sub(&report, "manifest"), "blocked").is_truthy());
    let summary = sub(&saved_manifest, "live2d_layer_capture");
    assert_eq!(get(summary, "status"), &Value::from("incomplete"));
    assert!(get(summary, "blocked").is_truthy());
    assert!(get(summary, "quality_issues").is_truthy());
    assert!(has_no_layer_fields(std::slice::from_ref(model_at(&saved_manifest, 0))));
    assert_eq!(
        get(&saved_character, "live2d_layer_capture"),
        get(&saved_manifest, "live2d_layer_capture")
    );
    assert_eq!(get(&saved_character, "live2d_models"), get(&saved_manifest, "live2d_models"));
}
