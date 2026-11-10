use std::path::Path;
use std::sync::Mutex;

use clap::Parser as _;
use fav_core::pyjson::Dict;
use fav_sources::nikke::runtime::{RuntimeCaptureRequest, RuntimeCapturer};
use futures_util::future::BoxFuture;

use super::*;
use crate::{Cli, Command};

/// A capturer that fails every capture and remembers the requests.
#[derive(Default)]
struct RecordingCapturer {
    requests: Mutex<Vec<RuntimeCaptureRequest>>,
}

impl RuntimeCapturer for RecordingCapturer {
    fn capture(&self, request: RuntimeCaptureRequest) -> BoxFuture<'_, Result<Dict, PyError>> {
        Box::pin(async move {
            self.requests.lock().expect("requests").push(request);
            Err(PyError {
                class: "AssertionError".to_owned(),
                message: "no browser in tests".to_owned(),
            })
        })
    }
}

fn model(stable_id: &str) -> serde_json::Value {
    serde_json::json!({
        "label": "live2d(full)",
        "skin_index": 0,
        "stable_id": stable_id,
        "live2d_key": stable_id,
        "urls": {"atlas": format!("https://cdn.example.com/{stable_id}/model.atlas")},
    })
}

fn write_character(root: &Path, models: &[serde_json::Value]) {
    std::fs::create_dir_all(root).expect("root");
    let payload = serde_json::json!({"content_id": 711_133, "title": "Test", "live2d_models": models});
    let text = serde_json::to_string(&payload).expect("JSON");
    std::fs::write(root.join("manifest.json"), &text).expect("manifest");
    std::fs::write(root.join("character.json"), &text).expect("character");
}

fn parse(args: &[&str]) -> NikkeLayers {
    let mut command_line = vec!["fav", "nikke-layers"];
    command_line.extend_from_slice(args);
    match Cli::parse_from(command_line).command {
        Some(Command::NikkeLayers { command }) => command,
        other => panic!("not nikke-layers: {other:?}"),
    }
}

async fn run_with(command: NikkeLayers, capturer: &Arc<RecordingCapturer>) -> (u8, serde_json::Value, String) {
    let tool = LayerTool::new(Arc::clone(capturer) as Arc<dyn RuntimeCapturer>);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = run(command, &tool, &mut out, &mut err).await;
    let report = serde_json::from_slice(&out).unwrap_or(serde_json::Value::Null);
    (status, report, String::from_utf8(err).expect("UTF-8"))
}

// py: tests/test_nikke_layer_metadata_script.py::test_main_dispatches_backfill_force_refresh_and_limit
#[tokio::test]
async fn main_dispatches_backfill_force_refresh_and_limit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("711133 - Test");
    write_character(&root, &[model("main"), model("back")]);
    let root_arg = root.to_str().expect("UTF-8 path");

    let command = parse(&[
        "backfill-runtime-layers",
        "--character-root",
        root_arg,
        "--limit",
        "1",
        "--force-refresh",
    ]);

    let NikkeLayers::BackfillRuntimeLayers { options, force_refresh } = &command else {
        panic!("not backfill-runtime-layers: {command:?}");
    };
    assert_eq!(options.limit, 1);
    assert!(*force_refresh);
    assert_eq!(options.character_root, std::slice::from_ref(&root));
    assert!(!options.write);
    let capturer = Arc::new(RecordingCapturer::default());
    let (status, report, err) = run_with(command, &capturer).await;
    assert_eq!(status, 0, "{err}");
    assert_eq!(report["failed_count"], 0);
    // The dry run plans a capture because of `--force-refresh`, and launches nothing.
    assert_eq!(report["results"][0]["reuse_decision"]["reason"], "force_refresh");
    assert!(capturer.requests.lock().expect("requests").is_empty());
}

// py: tests/test_nikke_layer_metadata_script.py::test_main_dispatches_capture_missing_without_force_refresh
#[tokio::test]
async fn main_dispatches_capture_missing_without_force_refresh() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("711133 - Test");
    write_character(&root, &[model("main"), model("back")]);
    let root_arg = root.to_str().expect("UTF-8 path");

    let command = parse(&["capture-missing", "--character-root", root_arg, "--write"]);

    let NikkeLayers::CaptureMissing(options) = &command else {
        panic!("not capture-missing: {command:?}");
    };
    assert!(options.write);
    assert_eq!(options.character_root, std::slice::from_ref(&root));
    let capturer = Arc::new(RecordingCapturer::default());
    let (status, report, _) = run_with(command, &capturer).await;
    // `--write` launches the capture, without `force_refresh` (no previous capture).
    let requests = capturer.requests.lock().expect("requests").clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].timeout_ms, 60_000);
    assert_eq!(report["results"][0]["reuse_decision"]["reason"], "no_previous_capture");
    assert_eq!(report["failed_count"], 1);
    assert_eq!(status, 1);

    // `--no-fail` keeps the exit status at 0.
    let command = parse(&["capture-missing", "--character-root", root_arg, "--write", "--no-fail"]);
    assert_eq!(run_with(command, &capturer).await.0, 0);
}

/// The script's `SystemExit` message goes to stderr with status 1, and nothing is printed.
#[tokio::test]
async fn a_missing_page_is_a_system_exit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root_arg = tmp.path().to_str().expect("UTF-8 path");

    let command = parse(&["strip", "--nikke-root", root_arg, "--content-id", "5"]);
    let (status, report, err) = run_with(command, &Arc::new(RecordingCapturer::default())).await;

    assert_eq!(status, 1);
    assert_eq!(report, serde_json::Value::Null);
    assert_eq!(err, format!("No NIKKE manifest found for content_id=5 under {root_arg}\n"));
}

#[test]
fn defaults_match_the_script() {
    let NikkeLayers::Validate { nikke_root, .. } = parse(&["validate"]) else {
        panic!("not validate");
    };
    assert_eq!(nikke_root, PathBuf::from("./collection/nikke"));
    let NikkeLayers::MergeDir { pattern, .. } = parse(&["merge-dir", "--capture-dir", "captures"]) else {
        panic!("not merge-dir");
    };
    assert_eq!(pattern, "*.json");
    let NikkeLayers::CaptureMissing(options) = parse(&["capture-missing"]) else {
        panic!("not capture-missing");
    };
    assert_eq!((options.limit, options.headful, options.no_fail), (0, false, false));
    assert!((options.timeout_seconds - 60.0).abs() < f64::EPSILON);
    assert!(
        Cli::try_parse_from(["fav", "nikke-layers", "merge"]).is_err(),
        "--capture is required"
    );
}
