use fav_core::pyjson::{Dict, Value};
use serde_json::json;

use super::*;

fn dict(value: &serde_json::Value) -> Dict {
    match Value::from_serde(value) {
        Value::Dict(dict) => dict,
        other => panic!("not a dict: {other:?}"),
    }
}

fn model(stable_id: &str, live2d_key: &str, atlas_suffix: &str) -> Dict {
    dict(&json!({
        "label": "live2d(full)",
        "skin_index": 0,
        "stable_id": stable_id,
        "live2d_key": live2d_key,
        "urls": {
            "atlas": format!("https://cdn.example.com/{live2d_key}/model.atlas{atlas_suffix}"),
            "skel": format!("https://cdn.example.com/{live2d_key}/model.skel"),
            "json": format!("https://cdn.example.com/{live2d_key}/model.json"),
            "image": [
                format!("https://cdn.example.com/{live2d_key}/texture-b.png"),
                format!("https://cdn.example.com/{live2d_key}/texture-a.png"),
            ],
        },
    }))
}

fn with_skin(mut model: Dict, skin_index: i128) -> Dict {
    model.insert("skin_index", Value::Int(skin_index));
    model
}

fn models() -> Vec<Dict> {
    vec![
        model("stable-main", "main", ""),
        model("stable-front", "front", ""),
        model("stable-back", "back", ""),
    ]
}

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

fn layer(stable_id: &str, live2d_key: &str, order: i64, z: i64, index: i64, primary: bool) -> serde_json::Value {
    json!({
        "stable_id": stable_id,
        "live2d_key": live2d_key,
        "layer_order": order,
        "source_z_index": z,
        "source_layer_index": index,
        "is_primary": primary,
        "layer_match_confidence": "high",
        "runtime_animations": runtime_animations(live2d_key),
        "runtime_animation_match_method": "gamekee-runtime-player",
        "runtime_animation_match_confidence": "high",
    })
}

fn refs(models: &[Dict]) -> Vec<&Dict> {
    models.iter().collect()
}

fn success_capture(content_id: i128, models: &[Dict]) -> Dict {
    let mut capture = dict(&json!({
        "content_id": 711_133,
        "status": "success",
        "layer_capture_schema": 2,
        "layers": [
            layer("stable-main", "main", 2, 9, 0, true),
            layer("stable-front", "front", 3, 10, 1, false),
            layer("stable-back", "back", 1, 8, 2, false),
        ],
    }));
    capture.insert("content_id", Value::Int(content_id));
    capture.insert("fingerprint", Value::Str(live2d_layer_fingerprint(content_id, &refs(models))));
    capture
}

fn layers_mut(capture: &mut Dict) -> &mut Vec<Value> {
    match capture.get_mut("layers") {
        Some(Value::List(items)) => items,
        _ => panic!("layers is a list"),
    }
}

fn layer_mut(capture: &mut Dict, index: usize) -> &mut Dict {
    match &mut layers_mut(capture)[index] {
        Value::Dict(layer) => layer,
        _ => panic!("layer is a dict"),
    }
}

fn get<'a>(dict: &'a Dict, key: &str) -> &'a Value {
    dict.get(key).unwrap_or_else(|| panic!("no key {key}"))
}

fn sub<'a>(dict: &'a Dict, key: &str) -> &'a Dict {
    get(dict, key).as_dict().unwrap_or_else(|| panic!("{key} is not a dict"))
}

fn s(text: &str) -> Value {
    Value::Str(text.to_owned())
}

// py: tests/test_nikke_layer_metadata.py::test_live2d_layer_fingerprint_changes_when_identity_or_urls_change
#[test]
fn fingerprint_changes_when_identity_or_urls_change() {
    let content_id = 711_133;
    let models = models();
    let reordered = vec![models[2].clone(), models[0].clone(), models[1].clone()];
    let changed_url = vec![
        model("stable-main", "main", ""),
        model("stable-front", "front", "?v=2"),
        model("stable-back", "back", ""),
    ];
    let mut changed_skin = models.clone();
    changed_skin[1].insert("skin_index", Value::Int(1));
    let mut changed_stable = models.clone();
    changed_stable[1].insert("stable_id", s("stable-front-v2"));
    let mut changed_key = models.clone();
    changed_key[1].insert("live2d_key", s("front-v2"));

    let fingerprint = live2d_layer_fingerprint(content_id, &refs(&models));

    assert_eq!(fingerprint, live2d_layer_fingerprint(content_id, &refs(&reordered)));
    assert_ne!(fingerprint, live2d_layer_fingerprint(content_id, &refs(&changed_url)));
    assert_ne!(fingerprint, live2d_layer_fingerprint(content_id, &refs(&changed_skin)));
    assert_ne!(fingerprint, live2d_layer_fingerprint(content_id, &refs(&changed_stable)));
    assert_ne!(fingerprint, live2d_layer_fingerprint(content_id, &refs(&changed_key)));
    assert_ne!(fingerprint, live2d_layer_fingerprint(600_940, &refs(&models)));
}

// py: tests/test_nikke_layer_metadata.py::test_live2d_layer_fingerprint_candidates_include_multi_full_groups
#[test]
fn fingerprint_candidates_include_multi_full_groups() {
    let content_id = 711_133;
    let models = vec![
        model("skin0-main", "skin0-main", ""),
        model("skin0-back", "skin0-back", ""),
        with_skin(model("skin1-main", "skin1-main", ""), 1),
        with_skin(model("skin1-back", "skin1-back", ""), 1),
    ];

    let candidates = live2d_layer_fingerprint_candidates(content_id, &refs(&models));

    let all = live2d_layer_fingerprint(content_id, &refs(&models));
    assert!(candidates.contains(&all));
    assert!(candidates.contains(&live2d_layer_fingerprint(content_id, &refs(&models[..2]))));
    assert!(candidates.contains(&live2d_layer_fingerprint(content_id, &refs(&models[2..]))));
    assert_eq!(candidates[0], all);
}

// py: tests/test_nikke_layer_metadata.py::test_evaluate_layer_capture_reuse_accepts_matching_success_capture_without_mutating_models
#[test]
fn reuse_accepts_matching_success_capture_without_mutating_models() {
    let content_id = 711_133;
    let models = models();
    let original = models.clone();
    let capture = success_capture(content_id, &models);

    let decision = evaluate_layer_capture_reuse(content_id, &models, Some(&capture), false);

    assert_eq!(get(&decision, "reusable"), &Value::Bool(true));
    assert_eq!(get(&decision, "action"), &s("reuse"));
    assert_eq!(get(&decision, "reason"), &s("fingerprint_match"));
    assert_eq!(get(sub(&decision, "merge_report"), "skipped"), &Value::List(vec![]));
    assert_eq!(get(sub(&decision, "merge_report"), "quality_issues"), &Value::List(vec![]));
    assert_eq!(models, original);
}

// py: tests/test_nikke_layer_metadata.py::test_layer_capture_manifest_summary_excludes_raw_container_evidence
#[test]
fn manifest_summary_excludes_raw_container_evidence() {
    let content_id = 711_133;
    let mut models = models();
    let mut capture = success_capture(content_id, &models);
    capture.insert(
        "runtime",
        Value::from_serde(&json!({
            "requested_live2d_keys": ["main", "front", "back"],
            "matched_live2d_keys": ["main", "front", "back"],
            "container_count": 3,
            "raw_browser_state": {"path": "/var/lib/chromium-profile"},
        })),
    );
    capture.insert(
        "warnings",
        Value::from_serde(&json!(["safe warning", {"raw": "object"}, "path /var/lib/chromium-profile"])),
    );
    layer_mut(&mut capture, 0).insert(
        "raw_container",
        Value::from_serde(&json!({"class_name": "spine-player-container", "style": "z-index: 9"})),
    );
    let report = merge_live2d_layer_captures(&mut models, &capture, Some(content_id), true);

    let summary = layer_capture_manifest_summary(&capture, &report);

    assert!(layer_mut(&mut capture, 0).get("raw_container").is_some_and(Value::is_truthy));
    assert!(get(&summary, "capture_hash").is_truthy());
    let layers = get(&summary, "layers").as_list().expect("layers");
    assert!(
        layers
            .iter()
            .all(|layer| !layer.as_dict().expect("dict").contains_key("raw_container"))
    );
    assert_eq!(get(&summary, "schema"), &Value::Int(2));
    let first = layers[0].as_dict().expect("dict");
    assert_eq!(get(sub(sub(first, "runtime_animations"), "click"), "animation"), &s("main-click"));
    assert_eq!(
        get(&summary, "runtime"),
        &Value::from_serde(&json!({
            "requested_live2d_keys": ["main", "front", "back"],
            "matched_live2d_keys": ["main", "front", "back"],
            "container_count": 3,
        }))
    );
    assert_eq!(
        get(&summary, "warnings"),
        &Value::from_serde(&json!(["safe warning", "path <path>"]))
    );
}

fn strip_runtime(models: &mut [Dict]) {
    for model in models {
        model.remove("runtime_animations");
        model.remove("runtime_animation_match_method");
        model.remove("runtime_animation_match_confidence");
    }
}

// py: tests/test_nikke_layer_metadata.py::test_merge_live2d_layer_captures_persists_runtime_animation_metadata
#[test]
fn merge_persists_runtime_animation_metadata() {
    let content_id = 711_133;
    let mut models = models();
    let capture = success_capture(content_id, &models);
    strip_runtime(&mut models);

    let report = merge_live2d_layer_captures(&mut models, &capture, Some(content_id), false);

    assert_eq!(get(&report, "matched"), &Value::Int(3));
    assert_eq!(get(&report, "quality_issues"), &Value::List(vec![]));
    let animations = sub(&models[0], "runtime_animations");
    assert_eq!(get(sub(animations, "idle"), "animation"), &s("main-idle"));
    assert_eq!(get(sub(animations, "click"), "animation"), &s("main-click"));
    assert_eq!(get(&models[0], "runtime_animation_match_confidence"), &s("high"));
}

// py: tests/test_nikke_layer_metadata.py::test_merge_layer_capture_files_persists_runtime_animation_metadata_to_manifest_and_character
#[test]
fn merge_files_persists_runtime_animation_metadata_to_manifest_and_character() {
    let content_id = 711_133;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("711133 - Test");
    let models = models();
    let capture = success_capture(content_id, &models);
    let mut manifest_models = models.clone();
    let mut character_models = models.clone();
    strip_runtime(&mut manifest_models);
    strip_runtime(&mut character_models);
    std::fs::create_dir(&root).expect("mkdir");
    let as_list = |models: Vec<Dict>| Value::List(models.into_iter().map(Value::Dict).collect());
    let mut manifest = Dict::new();
    manifest.insert("content_id", Value::Int(content_id));
    manifest.insert("live2d_models", as_list(manifest_models));
    let mut character = Dict::new();
    character.insert("live2d_models", as_list(character_models));
    std::fs::write(root.join("manifest.json"), json_dumps(&Value::Dict(manifest))).expect("write");
    std::fs::write(root.join("character.json"), json_dumps(&Value::Dict(character))).expect("write");

    let report = merge_layer_capture_files(&root, &capture, false).expect("merge");

    assert_eq!(get(sub(&report, "manifest"), "matched"), &Value::Int(3));
    assert_eq!(get(sub(&report, "character"), "matched"), &Value::Int(3));
    let expected = dict(&runtime_animations("main"));
    for name in ["manifest.json", "character.json"] {
        let payload = read_json_object(&root.join(name)).expect("read");
        let model = get(&payload, "live2d_models").as_list().expect("list")[0].as_dict().expect("dict");
        let animations = sub(model, "runtime_animations");
        assert!(py_eq(get(animations, "idle"), get(&expected, "idle")));
        assert!(py_eq(get(animations, "click"), get(&expected, "click")));
        assert_eq!(get(model, "runtime_animation_match_method"), &s("gamekee-runtime-player"));
        assert_eq!(get(model, "runtime_animation_match_confidence"), &s("high"));
        let summary = sub(&payload, LIVE2D_LAYER_CAPTURE_FIELD);
        assert_eq!(get(summary, "schema"), &Value::Int(2));
        let first = get(summary, "layers").as_list().expect("list")[0].as_dict().expect("dict");
        assert!(py_eq(get(sub(first, "runtime_animations"), "idle"), get(&expected, "idle")));
        assert!(py_eq(get(sub(first, "runtime_animations"), "click"), get(&expected, "click")));
        assert_eq!(get(first, "runtime_animation_match_method"), &s("gamekee-runtime-player"));
        assert_eq!(get(first, "runtime_animation_match_confidence"), &s("high"));
    }
}

// py: tests/test_nikke_layer_metadata.py::test_evaluate_layer_capture_reuse_accepts_matching_multi_full_group_fingerprint
#[test]
fn reuse_accepts_matching_multi_full_group_fingerprint() {
    let content_id = 711_133;
    let models = vec![
        model("skin0-single", "skin0-single", ""),
        with_skin(model("stable-main", "main", ""), 1),
        with_skin(model("stable-front", "front", ""), 1),
        with_skin(model("stable-back", "back", ""), 1),
    ];
    let capture = success_capture(content_id, &models[1..]);

    let decision = evaluate_layer_capture_reuse(content_id, &models, Some(&capture), false);

    assert_eq!(get(&decision, "reusable"), &Value::Bool(true));
    assert_eq!(get(&decision, "action"), &s("reuse"));
    assert_eq!(get(&decision, "reason"), &s("fingerprint_match"));
    assert_ne!(get(&decision, "previous_fingerprint"), get(&decision, "current_fingerprint"));
}

// py: tests/test_nikke_layer_metadata.py::test_evaluate_layer_capture_reuse_accepts_all_multi_full_groups_fingerprint_with_extra_models
#[test]
fn reuse_accepts_all_multi_full_groups_fingerprint_with_extra_models() {
    let content_id = 711_133;
    let models = vec![
        model("base-single", "base-single", ""),
        with_skin(model("skin0-main", "skin0-main", ""), 1),
        with_skin(model("skin0-back", "skin0-back", ""), 1),
        with_skin(model("skin1-main", "skin1-main", ""), 2),
        with_skin(model("skin1-back", "skin1-back", ""), 2),
    ];
    let mut capture = dict(&json!({
        "content_id": 711_133,
        "status": "success",
        "layer_capture_schema": 2,
        "layers": [
            layer("skin0-main", "skin0-main", 2, 9, 0, true),
            layer("skin0-back", "skin0-back", 1, 8, 1, false),
            layer("skin1-main", "skin1-main", 2, 9, 0, true),
            layer("skin1-back", "skin1-back", 1, 8, 1, false),
        ],
    }));
    capture.insert("fingerprint", Value::Str(live2d_layer_fingerprint(content_id, &refs(&models[1..]))));

    let decision = evaluate_layer_capture_reuse(content_id, &models, Some(&capture), false);

    assert_eq!(get(&decision, "reusable"), &Value::Bool(true));
    assert_eq!(get(&decision, "reason"), &s("fingerprint_match"));
    assert_eq!(get(sub(&decision, "group_coverage"), "complete"), &Value::Bool(true));
    assert_ne!(get(&decision, "previous_fingerprint"), get(&decision, "current_fingerprint"));
}

fn assert_refresh(decision: &Dict, reason: &str) {
    assert_eq!(get(decision, "reusable"), &Value::Bool(false));
    assert_eq!(get(decision, "action"), &s("refresh"));
    assert_eq!(get(decision, "reason"), &s(reason));
}

// py: tests/test_nikke_layer_metadata.py::test_evaluate_layer_capture_reuse_refreshes_when_reuse_contract_fails
#[test]
fn reuse_refreshes_when_reuse_contract_fails() {
    let cases = [
        (json!({"fingerprint": "different"}), false, "fingerprint_changed"),
        (json!({"status": "failed"}), false, "previous_status_not_success"),
        (json!({"layer_capture_schema": 1}), false, "capture_schema_mismatch"),
        (json!({"layer_capture_schema": null, "schema": 1}), false, "capture_schema_mismatch"),
        (json!({}), true, "force_refresh"),
    ];
    for (update, force_refresh, expected) in cases {
        let content_id = 711_133;
        let models = models();
        let mut capture = success_capture(content_id, &models);
        for (key, value) in dict(&update).iter() {
            capture.insert(key.clone(), value.clone());
        }

        let decision = evaluate_layer_capture_reuse(content_id, &models, Some(&capture), force_refresh);

        assert_refresh(&decision, expected);
    }
}

// py: tests/test_nikke_layer_metadata.py::test_evaluate_layer_capture_reuse_refreshes_when_runtime_animation_metadata_is_incomplete
#[test]
fn reuse_refreshes_when_runtime_animation_metadata_is_incomplete() {
    let content_id = 711_133;
    let models = models();
    let mut capture = success_capture(content_id, &models);
    let animations = layer_mut(&mut capture, 1).get_mut("runtime_animations").expect("animations");
    let Value::Dict(animations) = animations else { panic!("dict") };
    let Some(Value::Dict(click)) = animations.get_mut("click") else {
        panic!("click")
    };
    click.insert("match_confidence", s("low"));

    let decision = evaluate_layer_capture_reuse(content_id, &models, Some(&capture), false);

    assert_refresh(&decision, "previous_runtime_animations_incomplete");
}

// py: tests/test_nikke_layer_metadata.py::test_evaluate_layer_capture_reuse_refreshes_when_previous_capture_is_absent_or_unfingerprinted
#[test]
fn reuse_refreshes_when_previous_capture_is_absent_or_unfingerprinted() {
    let content_id = 711_133;
    assert_refresh(
        &evaluate_layer_capture_reuse(content_id, &models(), None, false),
        "no_previous_capture",
    );
    let previous = dict(&json!({"status": "success"}));
    assert_refresh(
        &evaluate_layer_capture_reuse(content_id, &models(), Some(&previous), false),
        "missing_fingerprint",
    );
}

// py: tests/test_nikke_layer_metadata.py::test_evaluate_layer_capture_reuse_refreshes_when_previous_confidence_is_not_high
#[test]
fn reuse_refreshes_when_previous_confidence_is_not_high() {
    let content_id = 711_133;
    let models = models();
    let mut capture = success_capture(content_id, &models);
    layer_mut(&mut capture, 1).insert("layer_match_confidence", s("low"));

    assert_refresh(
        &evaluate_layer_capture_reuse(content_id, &models, Some(&capture), false),
        "previous_confidence_not_high",
    );
}

// py: tests/test_nikke_layer_metadata.py::test_evaluate_layer_capture_reuse_refreshes_when_previous_confidence_is_missing
#[test]
fn reuse_refreshes_when_previous_confidence_is_missing() {
    let content_id = 711_133;
    let models = models();
    let mut capture = success_capture(content_id, &models);
    for index in 0..3 {
        assert!(layer_mut(&mut capture, index).remove("layer_match_confidence").is_some());
    }

    assert_refresh(
        &evaluate_layer_capture_reuse(content_id, &models, Some(&capture), false),
        "previous_confidence_not_high",
    );
}

// py: tests/test_nikke_layer_metadata.py::test_evaluate_layer_capture_reuse_refreshes_when_merge_skips_layer
#[test]
fn reuse_refreshes_when_merge_skips_layer() {
    let content_id = 711_133;
    let models = models();
    let mut capture = success_capture(content_id, &models);
    layer_mut(&mut capture, 1).insert("stable_id", s("missing-model"));

    let decision = evaluate_layer_capture_reuse(content_id, &models, Some(&capture), false);

    assert_refresh(&decision, "quality_gates_failed");
    let skipped = get(sub(&decision, "merge_report"), "skipped").as_list().expect("list");
    assert_eq!(get(skipped[0].as_dict().expect("dict"), "reason"), &s("no_match"));
}

// py: tests/test_nikke_layer_metadata.py::test_evaluate_layer_capture_reuse_refreshes_when_quality_issues_remain_after_merge
#[test]
fn reuse_refreshes_when_quality_issues_remain_after_merge() {
    let content_id = 711_133;
    let models = models();
    let mut capture = success_capture(content_id, &models);
    layer_mut(&mut capture, 0).insert("layer_order", Value::Int(1));
    layer_mut(&mut capture, 1).insert("layer_order", Value::Int(1));

    let decision = evaluate_layer_capture_reuse(content_id, &models, Some(&capture), false);

    assert_refresh(&decision, "quality_gates_failed");
    let merge = sub(&decision, "merge_report");
    assert_eq!(get(merge, "skipped"), &Value::List(vec![]));
    let codes: BTreeSet<String> = get(merge, "quality_issues")
        .as_list()
        .expect("list")
        .iter()
        .map(|issue| str_value(get(issue.as_dict().expect("dict"), "code")))
        .collect();
    assert_eq!(codes, BTreeSet::from(["duplicate_layer_order".to_owned()]));
}

// py: tests/test_nikke_layer_metadata.py::test_read_previous_layer_capture_prefers_raw_capture_over_manifest_summary
#[test]
fn read_previous_capture_prefers_raw_capture_over_manifest_summary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw_capture = dict(&json!({"status": "success", "fingerprint": "raw"}));
    let summary_capture = dict(&json!({"status": "success", "fingerprint": "summary"}));
    let raw_path = tmp.path().join("raw/live2d-layer-capture.json");
    std::fs::create_dir(raw_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&raw_path, json_dumps(&Value::Dict(raw_capture.clone()))).expect("write");
    let mut manifest = Dict::new();
    manifest.insert(LIVE2D_LAYER_CAPTURE_FIELD, Value::Dict(summary_capture.clone()));
    std::fs::write(tmp.path().join("manifest.json"), json_dumps(&Value::Dict(manifest))).expect("write");

    let read = read_previous_layer_capture(tmp.path()).expect("read").expect("a capture");
    assert!(py_eq(&Value::Dict(read), &Value::Dict(raw_capture)));

    std::fs::remove_file(&raw_path).expect("unlink");

    let read = read_previous_layer_capture(tmp.path()).expect("read").expect("a capture");
    assert!(py_eq(&Value::Dict(read), &Value::Dict(summary_capture)));
}

#[test]
fn python_equality_crosses_number_types() {
    assert!(py_eq(&Value::Int(1), &Value::Float(1.0)));
    assert!(py_eq(&Value::Bool(true), &Value::Int(1)));
    assert!(!py_eq(&Value::Int(1), &s("1")));
    assert!(py_eq(
        &Value::from_serde(&json!({"a": [1, 2]})),
        &Value::from_serde(&json!({"a": [1.0, 2]}))
    ));
    assert_eq!(to_int(Some(&s(" 12 "))), Some(12));
    assert_eq!(to_int(Some(&Value::Bool(true))), None);
    assert_eq!(to_int(Some(&Value::Float(1.0))), None);
    // `int('²')` raises in Python; `None` here (D83).
    assert_eq!(to_int(Some(&s("²"))), None);
}
