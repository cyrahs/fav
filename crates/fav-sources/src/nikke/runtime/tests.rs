//! The runtime layer capture: Python's tests (`tests/test_nikke_runtime.py`), the goldens
//! of the payload builders, the capture flow against a scripted page, and the real
//! Chromium capture against a local fake GameKee page (never the live site).

use std::sync::{Arc, Mutex};

use fav_browser::driver::DriverError;
use fav_core::golden;
use fav_core::pyjson::{Dict, Value, loads};
use futures_util::future::BoxFuture;

use super::payload::{
    LayerCaptureBuildInput, PayloadRequest, RuntimeContainerSnapshot, RuntimeGroupCapture, RuntimePlayerSnapshot,
    build_layer_capture_payload, capture_payload, containers_from_raw, first_skin_model_group, layer_capture_fingerprint,
    live2d_key_from_url, multi_full_model_groups, players_from_raw, select_model_group, to_float, to_int, unique_live2d_keys,
};
use super::{
    BrowserCapturer, CANVAS_COUNT_SCRIPT, CLOTHES_BUTTON_SELECTOR, CaptureBrowser, CaptureLauncher, CapturePage, ChromiumLauncher,
    GAMEKEE_RUNTIME_CAPTURE_SCRIPT, RuntimeCaptureRequest, RuntimeCapturer, capture_runtime_groups,
};
use crate::nikke_layer_metadata::{live2d_layer_fingerprint, merge_live2d_layer_captures};

const NOW: &str = "2026-07-07T00:00:00+00:00";

fn now() -> String {
    NOW.to_owned()
}

fn value(text: &str) -> Value {
    loads(text).expect("test JSON")
}

fn dict(text: &str) -> Dict {
    match value(text) {
        Value::Dict(dict) => dict,
        other => panic!("not an object: {other:?}"),
    }
}

fn model(skin_index: i64, stable_id: &str, live2d_key: &str) -> Dict {
    dict(&format!(
        r#"{{"label": "live2d(full)", "skin_index": {skin_index}, "stable_id": "{stable_id}", "live2d_key": "{live2d_key}",
            "urls": {{"atlas": "https://cdn.example.com/live2d/{live2d_key}/model.atlas",
                      "skel": "https://cdn.example.com/live2d/{live2d_key}/model.skel"}}}}"#
    ))
}

fn with(mut model: Dict, key: &str, value: Value) -> Dict {
    model.insert(key, value);
    model
}

fn runtime_animations(prefix: &str) -> Value {
    value(&format!(
        r#"{{"idle": {{"animation": "{prefix}-idle", "enabled": true, "loop": true, "match_method": "gamekee-runtime-player", "match_confidence": "high"}},
            "click": {{"animation": "{prefix}-click", "enabled": true, "loop": false, "match_method": "gamekee-runtime-player-event",
                       "match_confidence": "high", "duration_ms": 1200}}}}"#
    ))
}

fn containers(text: &str) -> Vec<RuntimeContainerSnapshot> {
    containers_from_raw(value(text).as_list().expect("a list"))
}

fn players(text: &str) -> Vec<RuntimePlayerSnapshot> {
    players_from_raw(value(text).as_list().expect("a list"))
}

fn keys(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn stable_ids(models: &[Dict]) -> Vec<String> {
    models
        .iter()
        .map(|model| golden::string(model.get("stable_id").expect("stable_id")))
        .collect()
}

fn layers(payload: &Dict) -> Vec<Dict> {
    payload
        .get("layers")
        .and_then(Value::as_list)
        .expect("layers")
        .iter()
        .map(|layer| layer.as_dict().expect("a layer").clone())
        .collect()
}

fn layer_by_key(payload: &Dict, key: &str) -> Dict {
    layers(payload)
        .into_iter()
        .find(|layer| layer.get("live2d_key") == Some(&Value::from(key)))
        .unwrap_or_else(|| panic!("no layer {key}"))
}

fn path<'a>(value: &'a Dict, keys: &[&str]) -> &'a Value {
    let (first, rest) = keys.split_first().expect("a path");
    let mut current = value.get(first).unwrap_or_else(|| panic!("no {first}"));
    for key in rest {
        current = current.get(key).unwrap_or_else(|| panic!("no {key}"));
    }
    current
}

// ---------- Python's tests ----------

// py: tests/test_nikke_runtime.py::test_live2d_key_from_url_extracts_runtime_resource_key
#[test]
fn live2d_key_from_url_extracts_runtime_resource_key() {
    let urls = [
        "https://cdn.gamekee.com/live2d/nikke/main/model.atlas?version=1",
        "https://cdn.gamekee.com/live2d/nikke/front/model.skel",
        "https://cdn.gamekee.com/live2d/nikke/main/model.png",
        "https://cdn.example.com/not-live2d/model.png",
    ];
    assert_eq!(live2d_key_from_url(urls[0]), "main");
    assert_eq!(live2d_key_from_url(urls[3]), "");
    assert_eq!(unique_live2d_keys(urls), ["main", "front"]);
}

// py: tests/test_nikke_runtime.py::test_first_skin_model_group_ignores_aim_and_cover_models
#[test]
fn first_skin_model_group_ignores_aim_and_cover_models() {
    let models = vec![
        model(0, "stable-main", "main"),
        model(0, "stable-front", "front"),
        model(0, "stable-back", "back"),
        with(model(0, "stable-aim", "aim"), "label", Value::from("live2d(aim)")),
        with(model(0, "stable-cover", "cover"), "label", Value::from("live2d(cover)")),
    ];
    assert_eq!(
        stable_ids(&first_skin_model_group(&models)),
        ["stable-main", "stable-front", "stable-back"]
    );
}

// py: tests/test_nikke_runtime.py::test_multi_full_model_groups_selects_only_layered_full_skins
#[test]
fn multi_full_model_groups_selects_only_layered_full_skins() {
    let layered = |stable: &str| with(model(3, stable, stable), "skin_title", Value::from("Layered"));
    let models = vec![
        model(0, "base-main", "base-main"),
        layered("layer-main"),
        layered("layer-back"),
        with(layered("layer-aim"), "label", Value::from("live2d(aim)")),
        with(layered("layer-cover"), "label", Value::from("live2d(cover)")),
    ];
    let groups: Vec<Vec<String>> = multi_full_model_groups(&models).iter().map(|group| stable_ids(group)).collect();
    assert_eq!(groups, [["layer-main", "layer-back"]]);
}

/// What the scripted page saw, for the assertions.
#[derive(Debug, Default)]
struct Script {
    clicks: Vec<(String, u64)>,
    stage_clicks: Vec<(f64, f64)>,
    wait_args: Vec<(String, serde_json::Value, i64)>,
    init_code: String,
    recording: bool,
    gotos: Vec<(String, i64)>,
    urls: Vec<String>,
    viewport: Option<(u32, u32)>,
    headless: Option<bool>,
    closed: bool,
}

/// Python's `FakePage` / `FakeBrowser` / `FakePlaywright`: a page that answers the
/// capture's scripts and emits requests when it navigates and when a skin is picked.
#[derive(Clone, Default)]
struct ScriptedPage(Arc<Mutex<Script>>);

impl ScriptedPage {
    fn state(&self) -> std::sync::MutexGuard<'_, Script> {
        self.0.lock().expect("script state")
    }

    fn emit_request(&self, url: &str) {
        let mut state = self.state();
        if state.recording {
            state.urls.push(url.to_owned());
        }
    }
}

impl CapturePage for ScriptedPage {
    fn add_init_script<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<(), DriverError>> {
        script.clone_into(&mut self.state().init_code);
        Box::pin(async { Ok(()) })
    }

    fn record_requests(&self) -> BoxFuture<'_, Result<(), DriverError>> {
        self.state().recording = true;
        Box::pin(async { Ok(()) })
    }

    fn requested_urls(&self) -> Vec<String> {
        self.state().urls.clone()
    }

    fn goto<'a>(&'a self, url: &'a str, timeout_ms: i64) -> BoxFuture<'a, Result<(), DriverError>> {
        self.state().gotos.push((url.to_owned(), timeout_ms));
        self.emit_request("https://live2d-img.gamekee.com/wiki2.0/live2d/1253/base-main/model.skel");
        Box::pin(async { Ok(()) })
    }

    fn click<'a>(&'a self, selector: &'a str, timeout_ms: u64) -> BoxFuture<'a, Result<(), DriverError>> {
        assert_eq!(selector, r#"button.action-item[data-report-key*="clothes"]"#);
        self.state().clicks.push(("open-clothes".to_owned(), timeout_ms));
        Box::pin(async { Ok(()) })
    }

    fn click_last_exact_text<'a>(&'a self, text: &'a str, timeout_ms: u64) -> BoxFuture<'a, Result<(), DriverError>> {
        self.state().clicks.push((format!("skin:{text}"), timeout_ms));
        for key in ["target-main", "target-back"] {
            self.emit_request(&format!("https://live2d-img.gamekee.com/wiki2.0/live2d/1253/{key}/model.skel"));
        }
        Box::pin(async { Ok(()) })
    }

    fn wait_for_function<'a>(
        &'a self,
        function: &'a str,
        arg: serde_json::Value,
        timeout_ms: i64,
    ) -> BoxFuture<'a, Result<(), DriverError>> {
        self.state().wait_args.push((function.to_owned(), arg, timeout_ms));
        Box::pin(async { Ok(()) })
    }

    fn wait_for_timeout(&self, _ms: u64) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn evaluate<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<serde_json::Value, DriverError>> {
        let answer = if script.contains("eventCount") {
            serde_json::json!(10)
        } else if script.contains("snapshot") {
            serde_json::json!([{
                "playerIndex": 0, "containerIndex": 0, "zIndex": 9, "rawZIndex": "9",
                "atlasUrl": "https://cdn.example.com/live2d/target-main/model.atlas",
                "skelUrl": "https://cdn.example.com/live2d/target-main/model.skel",
                "configuredAnimation": "idle",
                "events": [
                    {"sequence": 8, "type": "animationState", "method": "setAnimationWith", "trackIndex": 0, "animation": "idle", "loop": true},
                    {"sequence": 12, "type": "animationState", "method": "setAnimationWith", "trackIndex": 0, "animation": "action", "loop": false},
                ],
            }])
        } else if script.contains("live2d-stage") {
            serde_json::json!({"x": 900, "y": 500})
        } else {
            serde_json::json!([
                {"index": 0, "zIndex": 9, "rawZIndex": "9", "canvasCount": 1},
                {"index": 1, "zIndex": 8, "rawZIndex": "8", "canvasCount": 1},
            ])
        };
        Box::pin(async move { Ok(answer) })
    }

    fn mouse_click(&self, x: f64, y: f64) -> BoxFuture<'_, Result<(), DriverError>> {
        self.state().stage_clicks.push((x, y));
        Box::pin(async { Ok(()) })
    }
}

impl CaptureBrowser for ScriptedPage {
    fn new_page(&self, viewport: (u32, u32)) -> BoxFuture<'_, Result<Box<dyn CapturePage>, DriverError>> {
        self.state().viewport = Some(viewport);
        let page = self.clone();
        Box::pin(async move { Ok(Box::new(page) as Box<dyn CapturePage>) })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        self.state().closed = true;
        Box::pin(async {})
    }
}

impl CaptureLauncher for ScriptedPage {
    fn launch(&self, headless: bool) -> BoxFuture<'_, Result<Box<dyn CaptureBrowser>, DriverError>> {
        self.state().headless = Some(headless);
        let browser = self.clone();
        Box::pin(async move { Ok(Box::new(browser) as Box<dyn CaptureBrowser>) })
    }
}

fn target_skin_models() -> Vec<Dict> {
    vec![
        with(model(0, "base-main", "base-main"), "skin_title", Value::from("基础时装")),
        with(model(3, "target-main", "target-main"), "skin_title", Value::from("Target Skin")),
        with(model(3, "target-back", "target-back"), "skin_title", Value::from("Target Skin")),
    ]
}

// py: tests/test_nikke_runtime.py::test_capture_runtime_groups_switches_to_non_default_skin
#[tokio::test]
async fn capture_runtime_groups_switches_to_non_default_skin() {
    let page = ScriptedPage::default();
    let request = RuntimeCaptureRequest::new(711_133, target_skin_models(), "", super::DEFAULT_RUNTIME_TIMEOUT_MS);
    let captures = capture_runtime_groups(&page, &request).await.expect("captured");

    let state = page.state();
    assert!(state.closed);
    assert_eq!(state.headless, Some(true));
    assert_eq!(state.viewport, Some((1440, 1000)));
    assert_eq!(state.gotos, [("https://www.gamekee.com/nikke/tj/711133.html".to_owned(), 60_000)]);
    assert!(state.init_code.contains("SpinePlayer"));
    for method in ["setAnimation", "addAnimation", "setAnimationWith", "addAnimationWith"] {
        assert!(state.init_code.contains(&format!("\"{method}\"")), "{method}");
    }
    assert_eq!(
        state.clicks,
        [("open-clothes".to_owned(), 10_000), ("skin:Target Skin".to_owned(), 10_000)]
    );
    assert_eq!(state.stage_clicks, [(900.0, 500.0)]);
    let wait_args: Vec<&serde_json::Value> = state.wait_args.iter().map(|(_, arg, _)| arg).collect();
    assert_eq!(wait_args, [&serde_json::json!(2)]);
    assert_eq!(captures.len(), 1);
    assert_eq!(stable_ids(&captures[0].models), ["target-main", "target-back"]);
    assert_eq!(captures[0].requested_live2d_keys, ["target-main", "target-back"]);
    assert_eq!(captures[0].runtime_click_start_sequence, Some(10));
    assert_eq!(captures[0].runtime_players[0].events.last().expect("events").animation, "action");
}

// py: tests/test_nikke_runtime.py::test_capture_runtime_containers_passes_wait_for_function_arg_by_keyword
#[tokio::test]
async fn capture_runtime_containers_passes_the_expected_count_as_the_wait_functions_argument() {
    // Python checks its own source for `arg=expected_layer_count`; here the wait is seen
    // receiving the count as the function's argument, with the request's timeout.
    let page = ScriptedPage::default();
    let mut request = RuntimeCaptureRequest::new(1, vec![model(0, "a", "a"), model(0, "b", "b")], "", 1234);
    request.expected_layer_count = Some(5);
    capture_runtime_groups(&page, &request).await.expect("captured");
    let state = page.state();
    assert_eq!(state.wait_args, [(CANVAS_COUNT_SCRIPT.to_owned(), serde_json::json!(5), 1234)]);
    assert!(CANVAS_COUNT_SCRIPT.starts_with("count =>"));
    assert_eq!(state.gotos[0].1, 1234);
    // The first skin is the default one: nothing is clicked, every request counts.
    assert!(state.clicks.is_empty());
}

fn single_group() -> RuntimeGroupCapture {
    let models = vec![model(0, "stable-main", "main"), model(0, "stable-back", "back")];
    RuntimeGroupCapture {
        models,
        containers: containers(r#"[{"index": 0, "zIndex": 9, "rawZIndex": "9"}, {"index": 1, "zIndex": 8, "rawZIndex": "8"}]"#),
        requested_live2d_keys: keys(&["main", "back"]),
        runtime_click_start_sequence: Some(3),
        runtime_players: players(
            r#"[{"playerIndex": 0, "containerIndex": 0, "zIndex": 9, "rawZIndex": "9",
                 "atlasUrl": "https://cdn.example.com/live2d/nikke/main/model.atlas",
                 "skelUrl": "https://cdn.example.com/live2d/nikke/main/model.skel",
                 "configuredAnimation": "idle", "animationDurationsMs": {"action": 1200},
                 "events": [{"sequence": 1, "type": "animationState", "method": "setAnimationWith", "trackIndex": 0, "animation": "idle", "loop": true},
                            {"sequence": 4, "type": "animationState", "method": "setAnimationWith", "trackIndex": 0, "animation": "action", "loop": false}]},
                {"playerIndex": 1, "containerIndex": 1, "zIndex": 8, "rawZIndex": "8",
                 "atlasUrl": "https://cdn.example.com/live2d/nikke/back/model.atlas",
                 "skelUrl": "https://cdn.example.com/live2d/nikke/back/model.skel",
                 "configuredAnimation": "bg_idle",
                 "events": [{"sequence": 2, "type": "animationState", "method": "setAnimationWith", "trackIndex": 0, "animation": "bg_idle", "loop": true}]}]"#,
        ),
    }
}

// py: tests/test_nikke_runtime.py::test_capture_gamekee_runtime_layers_preserves_single_group_runtime_timeline
#[test]
fn capture_gamekee_runtime_layers_preserves_single_group_runtime_timeline() {
    let group = single_group();
    let request = PayloadRequest {
        content_id: 711_133,
        title: "",
        source_url: "",
        models: &group.models.clone(),
    };
    let payload = capture_payload(&request, &[group], &now);
    let main = layer_by_key(&payload, "main");
    assert_eq!(path(&main, &["runtime_animations", "idle", "animation"]), &Value::from("idle"));
    assert_eq!(
        path(&main, &["runtime_animations", "click"]),
        &value(
            r#"{"animation": "action", "enabled": true, "loop": false, "match_method": "gamekee-runtime-player-event",
                "match_confidence": "high", "duration_ms": 1200}"#
        )
    );
    assert_eq!(path(&main, &["runtime_player", "runtime_key"]), &Value::from("main"));
    let back = layer_by_key(&payload, "back");
    assert_eq!(path(&back, &["runtime_animations", "idle", "animation"]), &Value::from("bg_idle"));
    assert_eq!(path(&back, &["runtime_animations", "click", "enabled"]), &Value::Bool(false));
}

// py: tests/test_nikke_runtime.py::test_capture_gamekee_runtime_layers_combines_multiple_group_payloads
#[test]
fn capture_gamekee_runtime_layers_combines_multiple_group_payloads() {
    let first = vec![model(0, "first-main", "first-main"), model(0, "first-back", "first-back")];
    let second = vec![model(1, "second-main", "second-main"), model(1, "second-back", "second-back")];
    let groups = [
        RuntimeGroupCapture {
            models: first.clone(),
            containers: containers(r#"[{"index": 0, "zIndex": 9, "rawZIndex": "9"}, {"index": 1, "zIndex": 8, "rawZIndex": "8"}]"#),
            requested_live2d_keys: keys(&["first-main", "first-back"]),
            ..RuntimeGroupCapture::default()
        },
        RuntimeGroupCapture {
            models: second.clone(),
            containers: containers(r#"[{"index": 0, "zIndex": 7, "rawZIndex": "7"}, {"index": 1, "zIndex": 6, "rawZIndex": "6"}]"#),
            requested_live2d_keys: keys(&["second-main", "second-back"]),
            ..RuntimeGroupCapture::default()
        },
    ];
    let all = [first, second].concat();
    let request = PayloadRequest {
        content_id: 711_133,
        title: "",
        source_url: "",
        models: &all,
    };
    let payload = capture_payload(&request, &groups, &now);
    assert_eq!(
        payload.get("runtime"),
        Some(&value(
            r#"{"requested_live2d_keys": ["first-main", "first-back", "second-main", "second-back"],
                "matched_live2d_keys": ["first-main", "first-back", "second-main", "second-back"], "container_count": 4}"#
        ))
    );
    let refs: Vec<&Dict> = all.iter().collect();
    assert_eq!(
        payload.get("fingerprint"),
        Some(&Value::Str(live2d_layer_fingerprint(711_133, &refs)))
    );
    let summary: Vec<(Value, Value, Value)> = layers(&payload)
        .iter()
        .map(|layer| {
            (
                layer.get("skin_index").cloned().expect("skin"),
                layer.get("stable_id").cloned().expect("id"),
                layer.get("layer_order").cloned().expect("order"),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (Value::Int(0), Value::from("first-main"), Value::Int(2)),
            (Value::Int(0), Value::from("first-back"), Value::Int(1)),
            (Value::Int(1), Value::from("second-main"), Value::Int(2)),
            (Value::Int(1), Value::from("second-back"), Value::Int(1)),
        ]
    );
}

fn build(models: Vec<Dict>, containers_json: &str, requested: &[&str]) -> LayerCaptureBuildInput {
    LayerCaptureBuildInput {
        content_id: 711_133,
        models,
        containers: containers(containers_json),
        requested_live2d_keys: keys(requested),
        captured_at: NOW.to_owned(),
        ..LayerCaptureBuildInput::default()
    }
}

fn layer_summary(payload: &Dict, fields: &[&str]) -> Vec<Vec<Value>> {
    layers(payload)
        .iter()
        .map(|layer| {
            fields
                .iter()
                .map(|field| layer.get(field).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect()
}

// py: tests/test_nikke_runtime.py::test_build_layer_capture_payload_orders_runtime_layers_by_z_index
#[test]
fn build_layer_capture_payload_orders_runtime_layers_by_z_index() {
    let models = vec![
        model(0, "other-main", "other-main"),
        model(0, "other-front", "other-front"),
        model(1, "stable-main", "main"),
        model(1, "stable-front", "front"),
        model(1, "stable-back", "back"),
    ];
    let mut input = build(
        models.clone(),
        r#"[{"index": 0, "zIndex": 9, "rawZIndex": "9", "canvasCount": 1}, {"index": 1, "zIndex": "10", "rawZIndex": "10", "canvasCount": 1},
            {"index": 2, "zIndex": 8, "rawZIndex": "8", "canvasCount": 1}]"#,
        &["main", "front", "back"],
    );
    input.title = "Test".to_owned();
    let payload = build_layer_capture_payload(&input, &now);
    assert_eq!(
        payload.get("runtime"),
        Some(&value(
            r#"{"requested_live2d_keys": ["main", "front", "back"], "matched_live2d_keys": ["main", "front", "back"], "container_count": 3}"#
        ))
    );
    assert_eq!(payload.get("status"), Some(&Value::from("success")));
    assert_eq!(payload.get("warnings"), Some(&Value::List(Vec::new())));
    let refs: Vec<&Dict> = models[2..].iter().collect();
    assert_eq!(
        payload.get("fingerprint"),
        Some(&Value::Str(live2d_layer_fingerprint(711_133, &refs)))
    );
    assert_eq!(
        layer_capture_fingerprint(711_133, &models[2..]),
        live2d_layer_fingerprint(711_133, &refs)
    );
    assert_eq!(
        layer_summary(
            &payload,
            &[
                "skin_index",
                "stable_id",
                "live2d_key",
                "source_layer_index",
                "source_z_index",
                "layer_order",
                "is_primary",
                "layer_match_confidence"
            ]
        ),
        [
            value(r#"[1, "stable-main", "main", 0, 9, 2, true, "high"]"#)
                .as_list()
                .expect("row")
                .to_vec(),
            value(r#"[1, "stable-front", "front", 1, 10, 3, false, "high"]"#)
                .as_list()
                .expect("row")
                .to_vec(),
            value(r#"[1, "stable-back", "back", 2, 8, 1, false, "high"]"#)
                .as_list()
                .expect("row")
                .to_vec(),
        ]
    );
    let first = &layers(&payload)[0];
    assert_eq!(
        path(first, &["resource_urls", "atlas"]),
        &Value::from("https://cdn.example.com/live2d/main/model.atlas")
    );
    assert_eq!(path(first, &["raw_container", "zIndex"]), &Value::Int(9));
}

// py: tests/test_nikke_runtime.py::test_build_layer_capture_payload_includes_runtime_animation_metadata
#[test]
fn build_layer_capture_payload_includes_runtime_animation_metadata() {
    let captured = |stable: &str, key: &str| {
        let mut model = model(0, stable, key);
        model.insert("runtime_animations", runtime_animations(key));
        model.insert("runtime_animation_match_method", Value::from("gamekee-runtime-player"));
        model.insert("runtime_animation_match_confidence", Value::from("high"));
        model
    };
    let capture_models = vec![captured("stable-main", "main"), captured("stable-front", "front")];
    let mut target_models = vec![model(0, "stable-main", "main"), model(0, "stable-front", "front")];
    let payload = build_layer_capture_payload(
        &build(
            capture_models,
            r#"[{"index": 0, "zIndex": 9, "rawZIndex": "9"}, {"index": 1, "zIndex": 8, "rawZIndex": "8"}]"#,
            &["main", "front"],
        ),
        &now,
    );
    let report = merge_live2d_layer_captures(&mut target_models, &payload, Some(711_133), false);

    assert_eq!(payload.get("layer_capture_schema"), Some(&Value::Int(2)));
    let first = &layers(&payload)[0];
    assert_eq!(first.get("runtime_animations"), Some(&runtime_animations("main")));
    assert_eq!(
        first.get("runtime_animation_match_method"),
        Some(&Value::from("gamekee-runtime-player"))
    );
    assert_eq!(first.get("runtime_animation_match_confidence"), Some(&Value::from("high")));
    assert_eq!(report.get("matched"), Some(&Value::Int(2)));
    assert_eq!(target_models[0].get("runtime_animations"), Some(&runtime_animations("main")));
    assert_eq!(
        target_models[0].get("runtime_animation_match_method"),
        Some(&Value::from("gamekee-runtime-player"))
    );
    assert_eq!(
        target_models[0].get("runtime_animation_match_confidence"),
        Some(&Value::from("high"))
    );
    assert_eq!(
        target_models[1].get("runtime_animation_match_confidence"),
        Some(&Value::from("high"))
    );
}

// py: tests/test_nikke_runtime.py::test_build_layer_capture_payload_uses_gamekee_runtime_timeline_without_guessing_background_click
#[test]
fn build_layer_capture_payload_uses_gamekee_runtime_timeline_without_guessing_background_click() {
    let models = vec![
        model(0, "stable-main", "main"),
        model(0, "stable-front", "front"),
        model(0, "stable-back", "back"),
    ];
    let mut input = build(
        models,
        r#"[{"index": 0, "zIndex": 9, "rawZIndex": "9"}, {"index": 1, "zIndex": 10, "rawZIndex": "10"}, {"index": 2, "zIndex": 8, "rawZIndex": "8"}]"#,
        &["main", "front", "back"],
    );
    input.runtime_players = players(
        r#"[{"playerIndex": 0, "containerIndex": 0, "zIndex": 9, "rawZIndex": "9",
             "atlasUrl": "https://cdn.example.com/live2d/nikke/main/model.atlas", "skelUrl": "https://cdn.example.com/live2d/nikke/main/model.skel",
             "configuredAnimation": "idle", "animationDurationsMs": {"action": 1333},
             "events": [{"sequence": 1, "type": "animationState", "method": "setAnimationWith", "trackIndex": 0, "animation": "idle", "loop": true},
                        {"sequence": 5, "type": "animationState", "method": "setAnimationWith", "trackIndex": 0, "animation": "action", "loop": false}]},
            {"playerIndex": 1, "containerIndex": 1, "zIndex": 10, "rawZIndex": "10",
             "atlasUrl": "https://cdn.example.com/live2d/nikke/front/model.atlas", "skelUrl": "https://cdn.example.com/live2d/nikke/front/model.skel",
             "configuredAnimation": "bg_idle", "animationDurationsMs": {"action": 900},
             "events": [{"sequence": 2, "type": "animationState", "method": "setAnimationWith", "trackIndex": 0, "animation": "bg_idle", "loop": true},
                        {"sequence": 6, "type": "animationState", "method": "addAnimationWith", "trackIndex": 0, "animation": "action", "loop": false, "delayMs": 250}]},
            {"playerIndex": 2, "containerIndex": 2, "zIndex": 8, "rawZIndex": "8",
             "atlasUrl": "https://cdn.example.com/live2d/nikke/back/model.atlas", "skelUrl": "https://cdn.example.com/live2d/nikke/back/model.skel",
             "configuredAnimation": "bg_idle",
             "events": [{"sequence": 3, "type": "animationState", "method": "setAnimationWith", "trackIndex": 0, "animation": "bg_idle", "loop": true}]}]"#,
    );
    input.runtime_click_start_sequence = Some(4);
    let payload = build_layer_capture_payload(&input, &now);

    assert_eq!(
        layer_by_key(&payload, "main").get("runtime_animations"),
        Some(&value(
            r#"{"idle": {"animation": "idle", "enabled": true, "loop": true, "match_method": "gamekee-runtime-player", "match_confidence": "high"},
                "click": {"animation": "action", "enabled": true, "loop": false, "match_method": "gamekee-runtime-player-event",
                          "match_confidence": "high", "duration_ms": 1333}}"#
        ))
    );
    let front = layer_by_key(&payload, "front");
    assert_eq!(path(&front, &["runtime_animations", "idle", "animation"]), &Value::from("bg_idle"));
    assert_eq!(path(&front, &["runtime_animations", "click", "animation"]), &Value::from("action"));
    assert_eq!(path(&front, &["runtime_animations", "click", "duration_ms"]), &Value::Int(900));
    let back = layer_by_key(&payload, "back");
    assert_eq!(path(&back, &["runtime_animations", "idle", "animation"]), &Value::from("bg_idle"));
    assert_eq!(
        path(&back, &["runtime_animations", "click"]),
        &value(r#"{"enabled": false, "match_method": "gamekee-runtime-player-event", "match_confidence": "high"}"#)
    );
    assert_eq!(path(&front, &["runtime_player", "runtime_key"]), &Value::from("front"));
    assert!(
        layers(&payload)
            .iter()
            .all(|layer| layer.get("runtime_animation_match_confidence") == Some(&Value::from("high")))
    );
}

// py: tests/test_nikke_runtime.py::test_build_layer_capture_payload_marks_runtime_match_low_when_player_resource_does_not_match_model
#[test]
fn build_layer_capture_payload_marks_runtime_match_low_when_player_resource_does_not_match_model() {
    let mut input = build(
        vec![model(0, "stable-main", "main")],
        r#"[{"index": 0, "zIndex": 9, "rawZIndex": "9"}]"#,
        &["main"],
    );
    input.runtime_players = players(
        r#"[{"playerIndex": 0, "containerIndex": 0, "zIndex": 9, "rawZIndex": "9",
             "atlasUrl": "https://cdn.example.com/live2d/nikke/other/model.atlas", "configuredAnimation": "idle",
             "events": [{"sequence": 1, "type": "animationState", "method": "setAnimationWith", "trackIndex": 0, "animation": "idle", "loop": true}]}]"#,
    );
    input.runtime_click_start_sequence = Some(2);
    let payload = build_layer_capture_payload(&input, &now);
    let first = &layers(&payload)[0];
    assert_eq!(path(first, &["runtime_animations", "idle", "animation"]), &Value::from("idle"));
    assert_eq!(first.get("runtime_animation_match_confidence"), Some(&Value::from("low")));
}

fn two_models() -> Vec<Dict> {
    vec![model(0, "stable-main", "main"), model(0, "stable-front", "front")]
}

const AUTO_CONTAINERS: &str = r#"[{"index": 0, "zIndex": 5, "rawZIndex": "5"}, {"index": 1, "zIndex": "auto", "rawZIndex": "auto"}]"#;

// py: tests/test_nikke_runtime.py::test_build_layer_capture_payload_marks_low_confidence_when_runtime_key_is_missing
#[test]
fn build_layer_capture_payload_marks_low_confidence_when_runtime_key_is_missing() {
    let payload = build_layer_capture_payload(&build(two_models(), AUTO_CONTAINERS, &["main"]), &now);
    assert_eq!(path(&payload, &["runtime", "matched_live2d_keys"]), &value(r#"["main"]"#));
    assert_eq!(
        payload.get("warnings"),
        Some(&value(r#"["fewer runtime live2d keys than visible containers"]"#))
    );
    let layers = layers(&payload);
    assert_eq!(layers[0].get("layer_match_confidence"), Some(&Value::from("high")));
    assert_eq!(layers[0].get("layer_order"), Some(&Value::Int(1)));
    assert_eq!(layers[1].get("stable_id"), Some(&Value::from("stable-front")));
    assert_eq!(layers[1].get("source_z_index"), Some(&Value::Null));
    assert_eq!(layers[1].get("layer_match_confidence"), Some(&Value::from("low")));
    assert!(!layers[1].contains_key("layer_order"));
}

// py: tests/test_nikke_runtime.py::test_build_layer_capture_payload_marks_low_confidence_when_z_index_is_non_finite
#[test]
fn build_layer_capture_payload_marks_low_confidence_when_z_index_is_non_finite() {
    let payload = build_layer_capture_payload(&build(two_models(), AUTO_CONTAINERS, &["main", "front"]), &now);
    assert_eq!(path(&payload, &["runtime", "matched_live2d_keys"]), &value(r#"["main", "front"]"#));
    assert_eq!(payload.get("warnings"), Some(&Value::List(Vec::new())));
    let layers = layers(&payload);
    assert_eq!(layers[0].get("layer_match_confidence"), Some(&Value::from("high")));
    assert_eq!(layers[0].get("layer_order"), Some(&Value::Int(1)));
    assert_eq!(layers[1].get("live2d_key"), Some(&Value::from("front")));
    assert_eq!(layers[1].get("source_z_index"), Some(&Value::Null));
    assert_eq!(layers[1].get("layer_match_confidence"), Some(&Value::from("low")));
    assert!(!layers[1].contains_key("layer_order"));
}

// py: tests/test_nikke_runtime.py::test_build_layer_capture_payload_is_accepted_by_existing_merge_boundary
#[test]
fn build_layer_capture_payload_is_accepted_by_existing_merge_boundary() {
    let mut models = vec![
        model(0, "stable-main", "main"),
        model(0, "stable-front", "front"),
        model(0, "stable-back", "back"),
    ];
    let payload = build_layer_capture_payload(
        &build(
            models.clone(),
            r#"[{"index": 0, "zIndex": 9, "rawZIndex": "9"}, {"index": 1, "zIndex": 10, "rawZIndex": "10"}, {"index": 2, "zIndex": 8, "rawZIndex": "8"}]"#,
            &["main", "front", "back"],
        ),
        &now,
    );
    let report = merge_live2d_layer_captures(&mut models, &payload, Some(711_133), false);
    assert_eq!(report.get("matched"), Some(&Value::Int(3)));
    assert_eq!(report.get("skipped"), Some(&Value::List(Vec::new())));
    assert_eq!(report.get("quality_issues"), Some(&Value::List(Vec::new())));
    let rows: Vec<Vec<Value>> = models
        .iter()
        .map(|model| {
            [
                "stable_id",
                "layer_order",
                "source_z_index",
                "source_layer_index",
                "is_primary",
                "layer_match_confidence",
            ]
            .iter()
            .map(|field| model.get(field).cloned().unwrap_or(Value::Null))
            .collect()
        })
        .collect();
    assert_eq!(
        rows,
        [
            value(r#"["stable-main", 2, 9, 0, true, "high"]"#).as_list().expect("row").to_vec(),
            value(r#"["stable-front", 3, 10, 1, false, "high"]"#)
                .as_list()
                .expect("row")
                .to_vec(),
            value(r#"["stable-back", 1, 8, 2, false, "high"]"#).as_list().expect("row").to_vec(),
        ]
    );
}

// ---------- goldens ----------

fn dicts(value: &Value) -> Vec<Dict> {
    golden::list(value)
        .iter()
        .map(|item| item.as_dict().expect("a dict").clone())
        .collect()
}

fn strings(value: &Value) -> Vec<String> {
    golden::list(value).iter().map(golden::string).collect()
}

#[test]
fn helpers_match_python() {
    let document = golden::load("nikke_runtime/helpers.json");
    for case in golden::cases(&document) {
        let result = case.get("result").unwrap_or(&Value::Null);
        match golden::string(golden::get(case, "fn")).as_str() {
            "to_int" => {
                assert_eq!(
                    to_int(Some(golden::get(case, "value"))).map_or(Value::Null, Value::Int),
                    *result,
                    "{case:?}"
                );
            }
            "to_float" => {
                assert_eq!(
                    to_float(Some(golden::get(case, "value"))).map_or(Value::Null, Value::Float),
                    *result,
                    "{case:?}"
                );
            }
            "live2d_key_from_url" => {
                assert_eq!(Value::Str(live2d_key_from_url(&golden::string(golden::get(case, "url")))), *result);
            }
            "unique_live2d_keys" => {
                let urls = strings(golden::get(case, "urls"));
                assert_eq!(unique_live2d_keys(urls.iter().map(String::as_str)), strings(result));
            }
            "container_from_raw" => {
                let raw = golden::get(case, "raw").as_dict().expect("raw");
                let snapshot = RuntimeContainerSnapshot::from_raw(raw, golden::int(golden::get(case, "fallback")));
                assert_eq!(Value::Dict(snapshot.to_raw_container()), *result, "{case:?}");
            }
            "player_from_raw" => {
                let raw = golden::get(case, "raw").as_dict().expect("raw");
                let player = RuntimePlayerSnapshot::from_raw(raw, golden::int(golden::get(case, "fallback")));
                assert_eq!(Value::Dict(player.to_payload()), *result, "{case:?}");
                let durations: Dict = player
                    .animation_durations_ms
                    .iter()
                    .map(|(name, duration)| (name.clone(), Value::Int(*duration)))
                    .collect();
                assert_eq!(Value::Dict(durations), *golden::get(case, "durations"), "{case:?}");
                let events: Vec<Value> = player.events.iter().map(|event| Value::Dict(event.to_payload())).collect();
                assert_eq!(Value::List(events), *golden::get(case, "events"), "{case:?}");
            }
            "groups" => {
                let models = dicts(golden::get(case, "models"));
                let multi: Vec<Value> = multi_full_model_groups(&models)
                    .iter()
                    .map(|group| Value::List(stable_ids(group).into_iter().map(Value::Str).collect()))
                    .collect();
                assert_eq!(Value::List(multi), *golden::get(case, "multi"), "{case:?}");
                assert_eq!(stable_ids(&first_skin_model_group(&models)), strings(golden::get(case, "first")));
                for selection in golden::list(golden::get(case, "select")) {
                    let runtime_keys = strings(golden::get(selection, "keys"));
                    assert_eq!(
                        stable_ids(&select_model_group(&models, &runtime_keys)),
                        strings(golden::get(selection, "result")),
                        "{selection:?}"
                    );
                }
            }
            other => panic!("unknown case {other}"),
        }
    }
}

fn optional_int(value: Option<&Value>) -> Option<i128> {
    match value {
        Some(Value::Int(int)) => Some(*int),
        _ => None,
    }
}

fn group_from_golden(group: &Value) -> RuntimeGroupCapture {
    RuntimeGroupCapture {
        models: dicts(golden::get(group, "models")),
        containers: containers_from_raw(golden::list(golden::get(group, "containers"))),
        requested_live2d_keys: strings(golden::get(group, "requested_live2d_keys")),
        runtime_players: group
            .get("runtime_players")
            .map_or_else(Vec::new, |items| players_from_raw(golden::list(items))),
        runtime_click_start_sequence: optional_int(group.get("runtime_click_start_sequence")),
    }
}

fn text_or_empty(value: Option<&Value>) -> String {
    value.map(golden::string).unwrap_or_default()
}

#[test]
fn payloads_match_python() {
    let document = golden::load("nikke_runtime/payloads.json");
    for case in golden::cases(&document) {
        let fixed = golden::string(golden::get(case, "now"));
        let clock = move || fixed.clone();
        let expected = golden::get(case, "result");
        let produced = match golden::string(golden::get(case, "fn")).as_str() {
            "build" => {
                let input = golden::get(case, "input");
                let build = LayerCaptureBuildInput {
                    content_id: golden::int(golden::get(input, "content_id")),
                    title: text_or_empty(input.get("title")),
                    models: dicts(golden::get(input, "models")),
                    containers: containers_from_raw(golden::list(golden::get(input, "containers"))),
                    runtime_players: input
                        .get("runtime_players")
                        .map_or_else(Vec::new, |items| players_from_raw(golden::list(items))),
                    runtime_click_start_sequence: optional_int(input.get("runtime_click_start_sequence")),
                    requested_live2d_keys: strings(golden::get(input, "requested_live2d_keys")),
                    captured_at: text_or_empty(input.get("captured_at")),
                    source_url: text_or_empty(input.get("source_url")),
                };
                build_layer_capture_payload(&build, &clock)
            }
            "capture" => {
                let scenario = golden::get(case, "scenario");
                let request = golden::get(scenario, "request");
                let models = dicts(golden::get(request, "models"));
                let title = text_or_empty(request.get("title"));
                let source_url = text_or_empty(request.get("source_url"));
                let groups: Vec<RuntimeGroupCapture> = golden::list(golden::get(scenario, "groups"))
                    .iter()
                    .map(group_from_golden)
                    .collect();
                let payload_request = PayloadRequest {
                    content_id: golden::int(golden::get(request, "content_id")),
                    title: &title,
                    source_url: &source_url,
                    models: &models,
                };
                capture_payload(&payload_request, &groups, &clock)
            }
            other => panic!("unknown case {other}"),
        };
        assert_eq!(Value::Dict(produced), *expected, "{case:?}");
    }
}

// ---------- real Chromium, local fake page ----------

/// A GameKee character page as far as the capture reads it: a Spine runtime assigned to
/// `window.spine_4_1_54` after the init script ran, two skins, a wardrobe button, players
/// that fetch their skeletons and switch animation when the stage is clicked.
const FAKE_GAMEKEE_PAGE: &str = r#"<!doctype html><html><head><style>
  body { margin: 0; }
  .live2d-stage { position: relative; width: 600px; height: 600px; }
  .spine-player-container { position: absolute; inset: 0; }
  .wardrobe { display: none; position: absolute; top: 620px; }
  .wardrobe.open { display: block; }
</style></head><body>
<div class="live2d-stage"></div>
<button class="action-item" data-report-key="nikke-clothes-open" style="position:absolute; top:610px; left:700px">时装</button>
<div class="wardrobe"><span class="skin">基础时装</span> <span class="skin">Target Skin</span></div>
<script>
  class AnimationState {
    constructor() { this.tracks = []; }
    setAnimationWith(track, name, loop) { this.tracks[track] = { animation: { name } }; }
  }
  class SpinePlayer {
    constructor(parent, config) {
      this.parent = parent;
      this.config = config;
      this.animationState = new AnimationState();
      this.skeleton = { data: { animations: [{ name: config.animation, duration: 2 }, { name: "action", duration: 1.2 }] } };
      this.initialize();
    }
    initialize() {
      this.parent.appendChild(document.createElement("canvas"));
      fetch(this.config.skelUrl);
      this.animationState.setAnimationWith(0, this.config.animation, true);
    }
  }
  window.spine_4_1_54 = { SpinePlayer, AnimationState };
  const stage = document.querySelector(".live2d-stage");
  let players = [];
  function show(keys) {
    stage.innerHTML = "";
    players = keys.map((key, index) => {
      const container = document.createElement("div");
      container.className = "spine-player-container layer-" + index;
      container.style.zIndex = String(9 - index);
      stage.appendChild(container);
      return new window.spine_4_1_54.SpinePlayer(container, {
        atlasUrl: "/live2d/nikke/" + key + "/model.atlas",
        skelUrl: "/live2d/nikke/" + key + "/model.skel",
        animation: index === 0 ? "idle" : "bg_idle",
      });
    });
  }
  show(["base-main"]);
  document.querySelector("button.action-item").addEventListener("click", () => document.querySelector(".wardrobe").classList.add("open"));
  document.querySelectorAll(".skin").forEach((skin) => skin.addEventListener("click", () => {
    if (skin.textContent === "Target Skin") { setTimeout(() => show(["target-main", "target-back"]), 50); }
  }));
  stage.addEventListener("click", () => players[0].animationState.setAnimationWith(0, "action", false));
</script></body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn captures_a_local_gamekee_page_in_chromium() {
    use crate::testkit::{Answer, FakeServer, chromium};

    let Some(executable) = chromium() else { return };
    let server = FakeServer::start(|request| {
        if request.path.starts_with("/live2d/") {
            Answer::bytes(200, b"skel")
        } else {
            Answer::html(FAKE_GAMEKEE_PAGE)
        }
    })
    .await;
    let mut request = RuntimeCaptureRequest::new(1253, target_skin_models(), "Target", 20_000);
    request.source_url = server.url("/nikke/tj/1253.html");
    let capturer = BrowserCapturer::with_launcher(Box::new(ChromiumLauncher { executable }));
    let payload = capturer.capture(request).await.expect("captured");

    assert_eq!(
        payload.get("runtime"),
        Some(&value(
            r#"{"requested_live2d_keys": ["target-main", "target-back"], "matched_live2d_keys": ["target-main", "target-back"], "container_count": 2}"#
        ))
    );
    assert_eq!(payload.get("warnings"), Some(&Value::List(Vec::new())));
    let main = layer_by_key(&payload, "target-main");
    assert_eq!(main.get("layer_order"), Some(&Value::Int(2)));
    assert_eq!(main.get("layer_match_confidence"), Some(&Value::from("high")));
    assert_eq!(
        path(&main, &["raw_container", "className"]),
        &Value::from("spine-player-container layer-0")
    );
    assert_eq!(path(&main, &["raw_container", "canvasCount"]), &Value::Int(1));
    assert_eq!(
        path(&main, &["runtime_animations", "click"]),
        &value(
            r#"{"animation": "action", "enabled": true, "loop": false, "match_method": "gamekee-runtime-player-event",
                "match_confidence": "high", "duration_ms": 1200}"#
        )
    );
    assert_eq!(path(&main, &["runtime_animations", "idle", "animation"]), &Value::from("idle"));
    assert_eq!(
        path(&main, &["runtime_player", "atlas_url"]),
        // The page's site-relative URL, made absolute against GameKee as Python does.
        &Value::from("https://www.gamekee.com/live2d/nikke/target-main/model.atlas")
    );
    let back = layer_by_key(&payload, "target-back");
    assert_eq!(back.get("layer_order"), Some(&Value::Int(1)));
    assert_eq!(path(&back, &["runtime_animations", "idle", "animation"]), &Value::from("bg_idle"));
    assert_eq!(path(&back, &["runtime_animations", "click", "enabled"]), &Value::Bool(false));
    assert_eq!(path(&back, &["runtime_animation_match_confidence"]), &Value::from("high"));
    // The base skin's player was replaced, so only the target skin's two are reported.
    assert_eq!(layers(&payload).len(), 2);
    assert!(GAMEKEE_RUNTIME_CAPTURE_SCRIPT.contains("__gvRuntimeCapture"));
    assert!(CLOTHES_BUTTON_SELECTOR.contains("clothes"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_that_never_draws_times_out_and_closes_the_browser() {
    use crate::testkit::{Answer, FakeServer, chromium};

    let Some(executable) = chromium() else { return };
    let server = FakeServer::start(|_| Answer::html("<html><body>nothing here</body></html>")).await;
    let mut request = RuntimeCaptureRequest::new(1, vec![model(0, "a", "a")], "", 500);
    request.source_url = server.url("/");
    let capturer = BrowserCapturer::with_launcher(Box::new(ChromiumLauncher { executable }));
    let error = capturer.capture(request).await.expect_err("no canvas ever");
    assert_eq!(error.class, "TimeoutError");
    assert_eq!(error.message, "Timeout 500ms exceeded.");
}

#[tokio::test]
async fn a_missing_browser_is_reported_as_playwright_does() {
    let capturer = BrowserCapturer::with_launcher(Box::new(ChromiumLauncher {
        executable: "/nonexistent/chrome".into(),
    }));
    let error = capturer
        .capture(RuntimeCaptureRequest::new(1, vec![model(0, "a", "a")], "", 500))
        .await
        .expect_err("nothing to launch");
    assert_eq!(error.class, "Error");
    assert_eq!(error.message, "Executable doesn't exist at /nonexistent/chrome");
}
