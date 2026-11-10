use std::sync::Mutex;

use fav_core::pyjson::{self, Dict, Value};
use futures_util::future::BoxFuture;
use serde_json::json;

use super::*;
use crate::nikke::tests::dict;
use crate::nikke_layer_metadata::{RUNTIME_ANIMATION_CAPTURE_SCHEMA, live2d_layer_fingerprint, py_eq};

const CONTENT_ID: i128 = 711_133;
const SECOND_CONTENT_ID: i128 = 711_134;

// ---------- fixtures ----------

/// A capturer that answers with a fixed payload (or fails) and remembers each request.
struct FakeCapturer {
    answer: Option<Dict>,
    requests: Mutex<Vec<RuntimeCaptureRequest>>,
}

impl FakeCapturer {
    fn answering(payload: Dict) -> Arc<Self> {
        Arc::new(Self {
            answer: Some(payload),
            requests: Mutex::new(Vec::new()),
        })
    }

    /// The commands that must not launch the browser.
    fn unexpected() -> Arc<Self> {
        Arc::new(Self {
            answer: None,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<RuntimeCaptureRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

impl RuntimeCapturer for FakeCapturer {
    fn capture(&self, request: RuntimeCaptureRequest) -> BoxFuture<'_, Result<Dict, PyError>> {
        Box::pin(async move {
            self.requests.lock().expect("requests").push(request);
            self.answer
                .clone()
                .ok_or_else(|| PyError::new("AssertionError", "browser capture must not run here"))
        })
    }
}

fn tool(capturer: &Arc<FakeCapturer>) -> LayerTool {
    LayerTool::new(Arc::clone(capturer) as Arc<dyn RuntimeCapturer>)
}

fn model(stable_id: &str, live2d_key: &str) -> Dict {
    dict(&json!({
        "label": "live2d(full)",
        "skin_index": 0,
        "stable_id": stable_id,
        "live2d_key": live2d_key,
        "urls": {
            "atlas": format!("https://cdn.example.com/{live2d_key}/model.atlas"),
            "skel": format!("https://cdn.example.com/{live2d_key}/model.skel"),
        },
    }))
}

fn models() -> Vec<Dict> {
    vec![model("stable-main", "main"), model("stable-back", "back")]
}

fn second_skin_models() -> Vec<Dict> {
    ["extra-main", "extra-back"]
        .iter()
        .map(|key| {
            let mut model = model(key, key);
            model.insert("skin_index", Value::Int(1));
            model
        })
        .collect()
}

fn write_character(root: &Path, content_id: i128, models: &[Dict]) {
    std::fs::create_dir_all(root).expect("root");
    let mut payload = dict(&json!({"content_id": content_id, "title": "Test"}));
    payload.insert("live2d_models", Value::List(models.iter().cloned().map(Value::Dict).collect()));
    let text = pyjson::dumps(&Value::Dict(payload), &pyjson::DumpOptions::python_default()).expect("dumps");
    std::fs::write(root.join("manifest.json"), &text).expect("manifest");
    std::fs::write(root.join("character.json"), &text).expect("character");
}

fn runtime_animations() -> serde_json::Value {
    json!({
        "idle": {"animation": "idle", "enabled": true, "loop": true,
                 "match_method": "gamekee-runtime-player", "match_confidence": "high"},
        "click": {"animation": "action", "enabled": true, "loop": false,
                  "match_method": "gamekee-runtime-player-event", "match_confidence": "high", "duration_ms": 1200},
    })
}

fn success_capture(content_id: i128, models: &[Dict]) -> Dict {
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

fn write_raw_capture(root: &Path, capture: &Dict) {
    let path = root.join("raw/live2d-layer-capture.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("raw dir");
    std::fs::write(
        &path,
        pyjson::dumps(&Value::Dict(capture.clone()), &pyjson::DumpOptions::python_default()).expect("dumps"),
    )
    .expect("raw capture");
}

fn runtime_args(nikke_root: &Path, root: &Path, write: bool) -> RuntimeCaptureArgs {
    RuntimeCaptureArgs {
        nikke_root: nikke_root.to_path_buf(),
        character_root: vec![root.to_path_buf()],
        content_id: Vec::new(),
        write,
        backup_dir: None,
        limit: 0,
        timeout_seconds: 60.0,
        headful: false,
        no_fail: false,
    }
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

fn results(report: &Dict) -> Vec<&Dict> {
    match get(report, "results") {
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::Dict(item) => item,
                other => panic!("not a dict: {other:?}"),
            })
            .collect(),
        other => panic!("not a list: {other:?}"),
    }
}

fn model_at(document: &Dict, index: usize) -> &Dict {
    match get(document, "live2d_models") {
        Value::List(models) => match &models[index] {
            Value::Dict(model) => model,
            other => panic!("not a dict: {other:?}"),
        },
        other => panic!("not a list: {other:?}"),
    }
}

fn s(text: &str) -> Value {
    Value::from(text)
}

// ---------- capture-missing and backfill-runtime-layers ----------

// py: tests/test_nikke_layer_metadata_script.py::test_capture_missing_dry_run_only_plans_missing_multi_full
#[tokio::test]
async fn capture_missing_dry_run_only_plans_missing_multi_full() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("711133 - Test");
    write_character(&root, CONTENT_ID, &models());
    let capturer = FakeCapturer::unexpected();

    let report = tool(&capturer)
        .run_runtime_capture_command(&runtime_args(tmp.path(), &root, false), false)
        .await
        .expect("report");

    assert_eq!(get(&report, "dry_run"), &Value::Bool(true));
    assert_eq!(get(&report, "failed_count"), &Value::Int(0));
    let result = results(&report)[0];
    assert_eq!(get(result, "action"), &s("would_capture"));
    assert_eq!(get(sub(result, "reuse_decision"), "reason"), &s("no_previous_capture"));
    assert!(!root.join("raw/live2d-layer-capture.json").exists());
    assert!(capturer.requests().is_empty());
}

// py: tests/test_nikke_layer_metadata_script.py::test_capture_missing_reuses_previous_capture_without_launching_browser
#[tokio::test]
async fn capture_missing_reuses_previous_capture_without_launching_browser() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("711133 - Test");
    write_character(&root, CONTENT_ID, &models());
    write_raw_capture(&root, &success_capture(CONTENT_ID, &models()));
    let capturer = FakeCapturer::unexpected();

    let report = tool(&capturer)
        .run_runtime_capture_command(&runtime_args(tmp.path(), &root, true), false)
        .await
        .expect("report");

    assert_eq!(get(&report, "failed_count"), &Value::Int(0));
    assert_eq!(get(results(&report)[0], "action"), &s("reused"));
    let manifest = read_document(&root.join("manifest.json"));
    assert_eq!(get(model_at(&manifest, 0), "layer_order"), &Value::Int(2));
    assert_eq!(get(sub(&manifest, "live2d_layer_capture"), "status"), &s("success"));
    assert!(capturer.requests().is_empty());
}

// py: tests/test_nikke_layer_metadata_script.py::test_capture_missing_does_not_reuse_previous_capture_that_leaves_groups_incomplete
#[tokio::test]
async fn capture_missing_does_not_reuse_previous_capture_that_leaves_groups_incomplete() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("711133 - Test");
    let first_group = models();
    let mut second_group = second_skin_models();
    for (model, fields) in second_group.iter_mut().zip([
        json!({"layer_order": 2, "source_z_index": 7, "source_layer_index": 0, "is_primary": true,
               "layer_match_method": "gamekee-runtime-container", "layer_match_confidence": "high"}),
        json!({"layer_order": 1, "source_z_index": 6, "source_layer_index": 1, "is_primary": false,
               "layer_match_method": "gamekee-runtime-container", "layer_match_confidence": "high"}),
    ]) {
        for (key, value) in dict(&fields).iter() {
            model.insert(key.clone(), value.clone());
        }
    }
    let all_models: Vec<Dict> = first_group.iter().chain(second_group.iter()).cloned().collect();
    write_character(&root, CONTENT_ID, &all_models);
    write_raw_capture(&root, &success_capture(CONTENT_ID, &first_group));
    let capturer = FakeCapturer::unexpected();

    let report = tool(&capturer)
        .run_runtime_capture_command(&runtime_args(tmp.path(), &root, false), false)
        .await
        .expect("report");

    let result = results(&report)[0];
    assert_eq!(get(&report, "failed_count"), &Value::Int(0));
    let decision = sub(result, "reuse_decision");
    assert_eq!(get(decision, "reusable"), &Value::Bool(false));
    assert_eq!(get(decision, "reason"), &s("capture_missing_multi_full_groups"));
    assert!(py_eq(
        get(decision, "missing_groups"),
        &Value::from_serde(&json!([{"skin_index": 1, "model_kind": "full"}]))
    ));
    assert_eq!(get(result, "action"), &s("would_capture"));
    assert_eq!(get(result, "reason"), &s("capture_missing_multi_full_groups"));
}

// py: tests/test_nikke_layer_metadata_script.py::test_backfill_runtime_layers_captures_writes_raw_and_merges
#[tokio::test]
async fn backfill_runtime_layers_captures_writes_raw_and_merges() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("711133 - Test");
    write_character(&root, CONTENT_ID, &models());
    let capture = success_capture(CONTENT_ID, &models());
    let capturer = FakeCapturer::answering(capture.clone());
    let mut args = runtime_args(tmp.path(), &root, true);
    args.timeout_seconds = 12.5;

    let report = tool(&capturer).run_runtime_capture_command(&args, false).await.expect("report");

    let requests = capturer.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].content_id, CONTENT_ID);
    assert_eq!(requests[0].timeout_ms, 12_500);
    assert!(requests[0].headless);
    assert_eq!(get(&report, "failed_count"), &Value::Int(0));
    assert_eq!(get(results(&report)[0], "action"), &s("captured"));
    let raw_capture = read_document(&root.join("raw/live2d-layer-capture.json"));
    assert!(py_eq(&Value::Dict(raw_capture), &Value::Dict(capture)));
    let manifest = read_document(&root.join("manifest.json"));
    assert_eq!(get(model_at(&manifest, 0), "layer_order"), &Value::Int(2));
    assert_eq!(get(sub(&manifest, "live2d_layer_capture"), "status"), &s("success"));
    let character = read_document(&root.join("character.json"));
    assert_eq!(get(model_at(&character, 1), "is_primary"), &Value::Bool(false));
}

// py: tests/test_nikke_layer_metadata_script.py::test_capture_missing_limit_stops_after_requested_count
#[tokio::test]
async fn capture_missing_limit_stops_after_requested_count() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let first_root = tmp.path().join("711133 - Test");
    let second_root = tmp.path().join("711134 - Test 2");
    write_character(&first_root, CONTENT_ID, &models());
    write_character(&second_root, SECOND_CONTENT_ID, &models());
    let mut args = runtime_args(tmp.path(), &first_root, false);
    args.character_root = vec![first_root, second_root];
    args.limit = 1;

    let report = tool(&FakeCapturer::unexpected())
        .run_runtime_capture_command(&args, false)
        .await
        .expect("report");

    assert_eq!(get(&report, "checked"), &Value::Int(2));
    assert_eq!(get(&report, "processed"), &Value::Int(1));
    let ids: Vec<&Value> = results(&report).iter().map(|result| get(result, "content_id")).collect();
    assert_eq!(ids, [&Value::Int(CONTENT_ID)]);
}

// py: tests/test_nikke_layer_metadata_script.py::test_backfill_runtime_layers_force_refresh_captures_even_when_previous_capture_is_reusable
#[tokio::test]
async fn backfill_runtime_layers_force_refresh_captures_even_when_previous_capture_is_reusable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("711133 - Test");
    write_character(&root, CONTENT_ID, &models());
    let reusable_capture = success_capture(CONTENT_ID, &models());
    write_raw_capture(&root, &reusable_capture);
    let capturer = FakeCapturer::answering(reusable_capture);

    let report = tool(&capturer)
        .run_runtime_capture_command(&runtime_args(tmp.path(), &root, true), true)
        .await
        .expect("report");

    let captured: Vec<i128> = capturer.requests().iter().map(|request| request.content_id).collect();
    assert_eq!(captured, [CONTENT_ID]);
    assert_eq!(get(&report, "failed_count"), &Value::Int(0));
    let result = results(&report)[0];
    assert_eq!(get(result, "action"), &s("captured"));
    assert_eq!(get(sub(result, "reuse_decision"), "reason"), &s("force_refresh"));
}

fn failing_merge_layer_capture_files(root: &Path, _capture: &Dict, _dry_run: bool) -> Result<Dict, PyError> {
    let broken = json!({"content_id": CONTENT_ID, "title": "BROKEN", "live2d_models": []});
    std::fs::write(root.join("manifest.json"), serde_json::to_string(&broken).expect("JSON")).expect("write");
    Err(PyError::new("RuntimeError", "merge failed after manifest write"))
}

// py: tests/test_nikke_layer_metadata_script.py::test_backfill_runtime_layers_restores_backup_when_write_merge_fails
#[tokio::test]
async fn backfill_runtime_layers_restores_backup_when_write_merge_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("711133 - Test");
    write_character(&root, CONTENT_ID, &models());
    let before_manifest = read_document(&root.join("manifest.json"));
    let before_character = read_document(&root.join("character.json"));
    let capturer = FakeCapturer::answering(success_capture(CONTENT_ID, &models()));
    let mut tool = tool(&capturer);
    tool.merge_files = failing_merge_layer_capture_files;

    let report = tool
        .run_runtime_capture_command(&runtime_args(tmp.path(), &root, true), false)
        .await
        .expect("report");

    let result = results(&report)[0];
    assert_eq!(get(&report, "failed_count"), &Value::Int(1));
    assert_eq!(get(result, "action"), &s("failed"));
    assert_eq!(get(result, "error_class"), &s("RuntimeError"));
    let backup = sub(result, "backup");
    assert!(get(backup, "manifest.json").is_truthy());
    assert!(get(backup, "character.json").is_truthy());
    assert_eq!(
        sub(result, "restored"),
        &dict(&json!({
            "manifest.json": "restored",
            "character.json": "restored",
            "raw/live2d-layer-capture.json": "removed",
        }))
    );
    assert_eq!(read_document(&root.join("manifest.json")), before_manifest);
    assert_eq!(read_document(&root.join("character.json")), before_character);
    assert!(!root.join("raw/live2d-layer-capture.json").exists());
    // The backup sits under the archive root, out of the way of `*/manifest.json`.
    assert!(PathBuf::from(fav_runtime::pystr::str_value(get(backup, "manifest.json"))).starts_with(tmp.path().join("_layer-backups")));
}

// ---------- the other commands ----------

/// `merge`, then `validate`, then `strip`, by content id, as an operator runs them.
#[test]
fn merge_validate_and_strip_find_the_page_by_content_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("711133 - Test");
    write_character(&root, CONTENT_ID, &models());
    let capture_path = tmp.path().join("capture.json");
    std::fs::write(
        &capture_path,
        pyjson::dumps(
            &Value::Dict(success_capture(CONTENT_ID, &models())),
            &pyjson::DumpOptions::python_default(),
        )
        .expect("dumps"),
    )
    .expect("capture");
    let backups = tmp.path().join("backups");

    let invalid = command_validate(&ValidateArgs {
        nikke_root: tmp.path().to_path_buf(),
        character_root: Vec::new(),
        content_id: Vec::new(),
        no_fail: false,
    })
    .expect("validate");
    assert_eq!(invalid.status, 1);
    assert_eq!(get(&invalid.report, "checked"), &Value::Int(1));

    let dry_run = command_merge(&MergeArgs {
        nikke_root: tmp.path().to_path_buf(),
        character_root: None,
        content_id: None,
        capture: capture_path.clone(),
        write: false,
        backup_dir: Some(backups.clone()),
    })
    .expect("merge");
    assert_eq!(dry_run.status, 0);
    assert!(!model_at(&read_document(&root.join("manifest.json")), 0).contains_key("layer_order"));

    let written = command_merge(&MergeArgs {
        nikke_root: tmp.path().to_path_buf(),
        character_root: None,
        content_id: Some(CONTENT_ID),
        capture: capture_path,
        write: true,
        backup_dir: Some(backups.clone()),
    })
    .expect("merge");
    assert_eq!(written.status, 0);
    assert!(backups.join("711133 - Test/manifest.json").exists());
    assert_eq!(
        get(model_at(&read_document(&root.join("manifest.json")), 0), "layer_order"),
        &Value::Int(2)
    );

    let valid = command_validate(&ValidateArgs {
        nikke_root: tmp.path().to_path_buf(),
        character_root: Vec::new(),
        content_id: vec![CONTENT_ID],
        no_fail: false,
    })
    .expect("validate");
    assert_eq!((valid.status, get(&valid.report, "error_count")), (0, &Value::Int(0)));

    let stripped = command_strip(&StripArgs {
        nikke_root: tmp.path().to_path_buf(),
        character_root: Some(root.clone()),
        content_id: None,
        write: true,
        backup_dir: Some(backups),
    })
    .expect("strip");
    assert_eq!(stripped.status, 0);
    assert!(!read_document(&root.join("manifest.json")).contains_key("live2d_layer_capture"));
}

/// An id no page has is the script's `SystemExit`; two pages with it are too.
#[test]
fn find_character_root_exits_on_no_or_several_matches() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let error = find_character_root(tmp.path(), CONTENT_ID).expect_err("no page");
    assert_eq!(error.class, "SystemExit");
    assert!(
        error.message.starts_with("No NIKKE manifest found for content_id=711133 under "),
        "{}",
        error.message
    );

    write_character(&tmp.path().join("a"), CONTENT_ID, &models());
    write_character(&tmp.path().join("b"), CONTENT_ID, &models());
    std::fs::create_dir_all(tmp.path().join("c")).expect("c");
    std::fs::write(tmp.path().join("c/manifest.json"), "{not-json").expect("c");
    let error = find_character_root(tmp.path(), CONTENT_ID).expect_err("two pages");
    assert_eq!(error.class, "SystemExit");
    assert!(
        error.message.starts_with("Multiple NIKKE manifests found for content_id=711133: "),
        "{}",
        error.message
    );
}

/// `merge-dir`'s `--pattern` is a pathlib glob.
#[test]
fn glob_matches_like_pathlib() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for name in ["a.json", "b.json", "c.txt", "sub/d.json", "sub/deeper/e.json"] {
        let path = tmp.path().join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, "{}").expect("write");
    }
    let names = |pattern: &str| -> Vec<String> {
        glob(tmp.path(), pattern)
            .expect("glob")
            .iter()
            .map(|path| path.strip_prefix(tmp.path()).expect("under").to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(names("*.json"), ["a.json", "b.json"]);
    assert_eq!(names("[ab].json"), ["a.json", "b.json"]);
    assert_eq!(names("[!a].json"), ["b.json"]);
    assert_eq!(names("?.txt"), ["c.txt"]);
    assert_eq!(names("*/*.json"), ["sub/d.json"]);
    assert_eq!(names("**/*.json"), ["a.json", "b.json", "sub/d.json", "sub/deeper/e.json"]);
    assert!(glob(tmp.path(), "").is_err());
}
