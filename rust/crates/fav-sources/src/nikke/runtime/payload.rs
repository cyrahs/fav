//! The capture payload: what the runtime capture read off a GameKee page, turned into the
//! layers `merge_live2d_layer_captures` takes.
//!
//! Python origin: everything in `src/web/nikke_runtime.py` that does not touch the
//! browser: `_to_int`, `_to_float`, the snapshot dataclasses and their `from_raw` /
//! `to_payload`, the model helpers, the skin groups, the runtime animation timeline and
//! `build_layer_capture_payload` / `_build_multi_group_layer_capture_payload`. Values are
//! the JSON dictionaries Python passes around ([`Dict`]) and keep Python's key order;
//! `rust/goldens/nikke_runtime/` holds Python's answers.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::{is_py_decimal, py_int_decimal, py_strip};

use crate::nikke_layer_metadata::{GAMEKEE_BASE_URL, first_text, infer_live2d_model_kind, live2d_layer_fingerprint, normalize_layer_url};

pub const LAYER_CAPTURE_SCHEMA: i128 = 2;
pub const LAYER_CAPTURE_MATCH_METHOD: &str = "gamekee-runtime-container";
pub const RUNTIME_ANIMATION_MATCH_METHOD: &str = "gamekee-runtime-player";
pub const RUNTIME_CLICK_MATCH_METHOD: &str = "gamekee-runtime-player-event";
/// The skin GameKee opens a character page on; nothing to click for it.
pub const DEFAULT_SKIN_TITLE: &str = "基础时装";
const MATCH_CONFIDENCES: [&str; 3] = ["high", "medium", "low"];

static LIVE2D_KEY_RE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"/live2d/[^/]+/([^/?#]+)/").expect("a valid pattern"));

// ---------- Python value helpers ----------

/// `_to_int`: an `int` (not a `bool`), an integral finite `float`, or the leading
/// `[+-]?\d+` of a stripped string (`\d` is any Unicode decimal digit, as `int()` reads it).
pub fn to_int(value: Option<&Value>) -> Option<i128> {
    match value? {
        Value::Int(int) => Some(*int),
        #[allow(clippy::cast_possible_truncation)] // reason: an integral float within i128 converts exactly
        Value::Float(float) if float.is_finite() && float.fract() == 0.0 && float.abs() < 1.7e38 => Some(*float as i128),
        Value::Str(text) => {
            let text = py_strip(text);
            let (negative, unsigned) = match text.chars().next() {
                Some('-') => (true, &text[1..]),
                Some('+') => (false, &text[1..]),
                _ => (false, text),
            };
            let digits: String = unsigned.chars().take_while(|ch| is_py_decimal(*ch)).collect();
            let magnitude = py_int_decimal(&digits)?;
            Some(if negative { -magnitude } else { magnitude })
        }
        _ => None,
    }
}

/// `_to_float`: a finite `int` or `float` (not a `bool`), or a stripped string `float()`
/// reads as a finite number.
pub fn to_float(value: Option<&Value>) -> Option<f64> {
    match value? {
        #[allow(clippy::cast_precision_loss)] // reason: `float(int)` rounds to the nearest double, as this does
        Value::Int(int) => Some(*int as f64),
        Value::Float(float) if float.is_finite() => Some(*float),
        Value::Str(text) => crate::pyval::float(&Value::Str(py_strip(text).to_owned()))
            .ok()
            .filter(|parsed| parsed.is_finite()),
        _ => None,
    }
}

/// `data.get(first, data.get(second))`: the second key only when the first is missing.
fn get_either<'a>(data: &'a Dict, first: &str, second: &str) -> Option<&'a Value> {
    if data.contains_key(first) {
        data.get(first)
    } else {
        data.get(second)
    }
}

/// `isinstance(value, Sequence) and not isinstance(value, str | bytes | bytearray)`.
fn as_sequence(value: Option<&Value>) -> Option<&[Value]> {
    match value {
        Some(Value::List(items)) => Some(items),
        _ => None,
    }
}

fn int_value(value: Option<i128>) -> Value {
    value.map_or(Value::Null, Value::Int)
}

fn optional_bool(value: Option<bool>) -> Value {
    value.map_or(Value::Null, Value::Bool)
}

fn dict_of(entries: Vec<(&str, Value)>) -> Dict {
    let mut dict = Dict::new();
    for (key, value) in entries {
        dict.insert(key, value);
    }
    dict
}

fn str_list(items: &[String]) -> Value {
    Value::List(items.iter().map(|item| Value::Str(item.clone())).collect())
}

/// `_unique_in_order`: the stripped, non-empty values, each once, in order.
pub fn unique_in_order<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        let value = py_strip(value);
        if !value.is_empty() && seen.insert(value.to_owned()) {
            out.push(value.to_owned());
        }
    }
    out
}

/// `live2d_key_from_url`: the model directory of a `/live2d/<game>/<key>/` URL.
pub fn live2d_key_from_url(url: &str) -> String {
    LIVE2D_KEY_RE
        .captures(url)
        .and_then(|captures| captures.get(1))
        .map_or_else(String::new, |found| found.as_str().to_owned())
}

/// `unique_live2d_keys`.
pub fn unique_live2d_keys<'a>(urls: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let keys: Vec<String> = urls.into_iter().map(live2d_key_from_url).collect();
    unique_in_order(keys.iter().map(String::as_str))
}

// ---------- snapshots ----------

/// `RuntimeContainerSnapshot`: one `.spine-player-container` of the page.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeContainerSnapshot {
    pub index: i128,
    pub z_index: Option<i128>,
    pub raw_z_index: String,
    pub class_name: String,
    pub parent_class_name: String,
    pub canvas_count: i128,
    /// `x`, `y`, `width`, `height`, in that order, those that were numbers.
    pub rect: Vec<(String, f64)>,
}

impl RuntimeContainerSnapshot {
    /// `RuntimeContainerSnapshot.from_raw(raw, fallback_index=...)`.
    pub fn from_raw(raw: &Dict, fallback_index: i128) -> Self {
        let raw_z_index = first_text(raw, &["raw_z_index", "rawZIndex"]);
        let z_index = to_int(get_either(raw, "z_index", "zIndex")).or_else(|| to_int(Some(&Value::Str(raw_z_index.clone()))));
        let mut rect = Vec::new();
        if let Some(Value::Dict(raw_rect)) = raw.get("rect") {
            for key in ["x", "y", "width", "height"] {
                if let Some(value) = to_float(raw_rect.get(key)) {
                    rect.push((key.to_owned(), value));
                }
            }
        }
        Self {
            index: to_int(raw.get("index")).unwrap_or(fallback_index),
            z_index,
            raw_z_index,
            class_name: first_text(raw, &["class_name", "className"]),
            parent_class_name: first_text(raw, &["parent_class_name", "parentClassName"]),
            canvas_count: to_int(get_either(raw, "canvas_count", "canvasCount")).unwrap_or(0),
            rect,
        }
    }

    /// `to_raw_container()`.
    pub fn to_raw_container(&self) -> Dict {
        let rect: Dict = self.rect.iter().map(|(key, value)| (key.clone(), Value::Float(*value))).collect();
        dict_of(vec![
            ("index", Value::Int(self.index)),
            ("zIndex", int_value(self.z_index)),
            ("rawZIndex", Value::Str(self.raw_z_index.clone())),
            ("className", Value::Str(self.class_name.clone())),
            ("parentClassName", Value::Str(self.parent_class_name.clone())),
            ("canvasCount", Value::Int(self.canvas_count)),
            ("rect", Value::Dict(rect)),
        ])
    }
}

/// `RuntimeAnimationEvent`: one call the init script recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAnimationEvent {
    pub sequence: i128,
    pub time_ms: Option<i128>,
    pub event_type: String,
    pub method: String,
    pub player_index: Option<i128>,
    pub track_index: Option<i128>,
    pub animation: String,
    pub r#loop: Option<bool>,
    pub delay_ms: Option<i128>,
}

impl RuntimeAnimationEvent {
    /// `RuntimeAnimationEvent.from_raw(raw)`.
    pub fn from_raw(raw: &Dict) -> Self {
        Self {
            sequence: to_int(get_either(raw, "sequence", "seq")).unwrap_or(0),
            time_ms: to_int(get_either(raw, "time_ms", "timeMs")),
            event_type: first_text(raw, &["type"]),
            method: first_text(raw, &["method"]),
            player_index: to_int(get_either(raw, "player_index", "playerIndex")),
            track_index: to_int(get_either(raw, "track_index", "trackIndex")),
            animation: first_text(raw, &["animation"]),
            r#loop: match raw.get("loop") {
                Some(Value::Bool(flag)) => Some(*flag),
                _ => None,
            },
            delay_ms: to_int(get_either(raw, "delay_ms", "delayMs")),
        }
    }

    /// `to_payload()`.
    pub fn to_payload(&self) -> Dict {
        dict_of(vec![
            ("sequence", Value::Int(self.sequence)),
            ("time_ms", int_value(self.time_ms)),
            ("type", Value::Str(self.event_type.clone())),
            ("method", Value::Str(self.method.clone())),
            ("player_index", int_value(self.player_index)),
            ("track_index", int_value(self.track_index)),
            ("animation", Value::Str(self.animation.clone())),
            ("loop", optional_bool(self.r#loop)),
            ("delay_ms", int_value(self.delay_ms)),
        ])
    }
}

/// `RuntimePlayerSnapshot`: one Spine player and the calls it received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePlayerSnapshot {
    pub player_index: i128,
    pub container_index: i128,
    pub z_index: Option<i128>,
    pub raw_z_index: String,
    pub atlas_url: String,
    pub skel_url: String,
    pub configured_animation: String,
    pub current_animation: String,
    /// In the page's order; a repeated name keeps its first place and last value.
    pub animation_durations_ms: Vec<(String, i128)>,
    pub events: Vec<RuntimeAnimationEvent>,
}

impl RuntimePlayerSnapshot {
    /// `RuntimePlayerSnapshot.from_raw(raw, fallback_index=...)`. A player or container
    /// index of `0` falls back too (`_to_int(...) or fallback_index`).
    pub fn from_raw(raw: &Dict, fallback_index: i128) -> Self {
        let mut durations: Vec<(String, i128)> = Vec::new();
        if let Some(Value::Dict(raw_durations)) = get_either(raw, "animation_durations_ms", "animationDurationsMs") {
            for (key, value) in raw_durations.iter() {
                if let Some(duration) = to_int(Some(value)).filter(|duration| *duration >= 0) {
                    let name = fav_runtime::pystr::str_value(&key_value(key));
                    match durations.iter_mut().find(|(existing, _)| *existing == name) {
                        Some(entry) => entry.1 = duration,
                        None => durations.push((name, duration)),
                    }
                }
            }
        }
        let events = as_sequence(raw.get("events")).map_or_else(Vec::new, |items| {
            items
                .iter()
                .filter_map(|item| item.as_dict().map(RuntimeAnimationEvent::from_raw))
                .collect()
        });
        Self {
            player_index: to_int(get_either(raw, "player_index", "playerIndex"))
                .filter(|index| *index != 0)
                .unwrap_or(fallback_index),
            container_index: to_int(get_either(raw, "container_index", "containerIndex"))
                .filter(|index| *index != 0)
                .unwrap_or(fallback_index),
            z_index: to_int(get_either(raw, "z_index", "zIndex")),
            raw_z_index: first_text(raw, &["raw_z_index", "rawZIndex"]),
            atlas_url: normalize_layer_url(&first_text(raw, &["atlas_url", "atlasUrl"])),
            skel_url: normalize_layer_url(&first_text(raw, &["skel_url", "skelUrl"])),
            configured_animation: first_text(raw, &["configured_animation", "configuredAnimation", "animation"]),
            current_animation: first_text(raw, &["current_animation", "currentAnimation", "current"]),
            animation_durations_ms: durations,
            events,
        }
    }

    /// `runtime_key()`: the model directory of the atlas, else of the skeleton.
    pub fn runtime_key(&self) -> String {
        let key = live2d_key_from_url(&self.atlas_url);
        if key.is_empty() { live2d_key_from_url(&self.skel_url) } else { key }
    }

    fn duration_of(&self, animation: &str) -> Option<i128> {
        self.animation_durations_ms
            .iter()
            .find(|(name, _)| name == animation)
            .map(|(_, duration)| *duration)
    }

    /// `to_payload()`.
    pub fn to_payload(&self) -> Dict {
        dict_of(vec![
            ("player_index", Value::Int(self.player_index)),
            ("container_index", Value::Int(self.container_index)),
            ("z_index", int_value(self.z_index)),
            ("raw_z_index", Value::Str(self.raw_z_index.clone())),
            ("atlas_url", Value::Str(self.atlas_url.clone())),
            ("skel_url", Value::Str(self.skel_url.clone())),
            ("configured_animation", Value::Str(self.configured_animation.clone())),
            ("current_animation", Value::Str(self.current_animation.clone())),
            ("runtime_key", Value::Str(self.runtime_key())),
        ])
    }
}

/// A dictionary key as the value `str(key)` would print.
fn key_value(key: &fav_core::pyjson::Key) -> Value {
    use fav_core::pyjson::Key;
    match key {
        Key::Str(text) => Value::Str(text.clone()),
        Key::Int(int) => Value::Int(*int),
        Key::Float(float) => Value::Float(*float),
        Key::Bool(flag) => Value::Bool(*flag),
        Key::Null => Value::Null,
    }
}

/// `_normalize_runtime_containers` for raw values: dictionaries only, each falling back
/// to its position in the list.
pub fn containers_from_raw(items: &[Value]) -> Vec<RuntimeContainerSnapshot> {
    (0_i128..)
        .zip(items)
        .filter_map(|(index, item)| item.as_dict().map(|raw| RuntimeContainerSnapshot::from_raw(raw, index)))
        .collect()
}

/// `_normalize_runtime_players` for raw values.
pub fn players_from_raw(items: &[Value]) -> Vec<RuntimePlayerSnapshot> {
    (0_i128..)
        .zip(items)
        .filter_map(|(index, item)| item.as_dict().map(|raw| RuntimePlayerSnapshot::from_raw(raw, index)))
        .collect()
}

// ---------- models ----------

fn model_live2d_key(model: &Dict) -> String {
    first_text(model, &["live2d_key", "live2dKey"])
}

fn model_stable_id(model: &Dict) -> String {
    first_text(model, &["stable_id", "stableId"])
}

fn model_skin_index(model: &Dict) -> Option<i128> {
    to_int(get_either(model, "skin_index", "skinIndex"))
}

/// `_model_skin_title`: the skin a model belongs to, as GameKee's wardrobe labels it.
pub fn model_skin_title(model: &Dict) -> String {
    first_text(model, &["skin_title", "skinTitle", "skin_name", "skinName"])
}

/// `_model_resource_urls`: the model's URLs made absolute, empty ones dropped.
fn model_resource_urls(model: &Dict) -> Dict {
    let mut raw = get_either(model, "resource_urls", "resourceUrls");
    if matches!(raw, None | Some(Value::Null)) {
        raw = model.get("urls");
    }
    let Some(Value::Dict(raw)) = raw else {
        return Dict::new();
    };
    let mut urls = Dict::new();
    for (key, value) in raw.iter() {
        let name = fav_runtime::pystr::str_value(&key_value(key));
        match value {
            Value::Str(text) => {
                let normalized = normalize_layer_url(text);
                if !normalized.is_empty() {
                    urls.insert(name, Value::Str(normalized));
                }
            }
            Value::List(items) => {
                let normalized: Vec<Value> = items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(normalize_layer_url)
                    .filter(|url| !url.is_empty())
                    .map(Value::Str)
                    .collect();
                urls.insert(name, Value::List(normalized));
            }
            _ => {}
        }
    }
    urls
}

/// `_model_runtime_animation_state`.
fn model_runtime_animation_state(value: Option<&Value>) -> Dict {
    let Some(Value::Dict(value)) = value else {
        return Dict::new();
    };
    let mut state = Dict::new();
    let animation = first_text(value, &["animation"]);
    if !animation.is_empty() {
        state.insert("animation", Value::Str(animation));
    }
    if let Some(Value::Bool(enabled)) = value.get("enabled") {
        state.insert("enabled", Value::Bool(*enabled));
    }
    if let Some(Value::Bool(looped)) = value.get("loop") {
        state.insert("loop", Value::Bool(*looped));
    }
    let match_method = first_text(value, &["match_method", "matchMethod"]);
    if !match_method.is_empty() {
        state.insert("match_method", Value::Str(match_method));
    }
    let match_confidence = first_text(value, &["match_confidence", "matchConfidence"]);
    if MATCH_CONFIDENCES.contains(&match_confidence.as_str()) {
        state.insert("match_confidence", Value::Str(match_confidence));
    }
    if let Some(duration) = to_int(get_either(value, "duration_ms", "durationMs")).filter(|duration| *duration >= 0) {
        state.insert("duration_ms", Value::Int(duration));
    }
    state
}

/// `_model_runtime_animations`.
fn model_runtime_animations(model: &Dict) -> Dict {
    let Some(Value::Dict(raw)) = get_either(model, "runtime_animations", "runtimeAnimations") else {
        return Dict::new();
    };
    let mut out = Dict::new();
    for state in ["idle", "click"] {
        let payload = model_runtime_animation_state(raw.get(state));
        if !payload.is_empty() {
            out.insert(state, Value::Dict(payload));
        }
    }
    out
}

/// `layer_capture_fingerprint`.
pub fn layer_capture_fingerprint(content_id: i128, models: &[Dict]) -> String {
    let refs: Vec<&Dict> = models.iter().collect();
    live2d_layer_fingerprint(content_id, &refs)
}

/// `_group_models_by_skin_and_kind`: in first-seen order.
fn group_models_by_skin_and_kind(models: &[Dict]) -> Vec<Vec<Dict>> {
    let mut keys: Vec<(Option<i128>, String)> = Vec::new();
    let mut groups: Vec<Vec<Dict>> = Vec::new();
    for model in models {
        let key = (model_skin_index(model), infer_live2d_model_kind(model));
        if let Some(index) = keys.iter().position(|existing| *existing == key) {
            groups[index].push(model.clone());
        } else {
            keys.push(key);
            groups.push(vec![model.clone()]);
        }
    }
    groups
}

fn full_groups_by_skin(models: &[Dict], min_len: usize) -> Vec<Vec<Dict>> {
    let mut groups: Vec<Vec<Dict>> = group_models_by_skin_and_kind(models)
        .into_iter()
        .filter(|group| group.len() >= min_len && group.first().is_some_and(|first| infer_live2d_model_kind(first) == "full"))
        .collect();
    groups.sort_by_key(|group| group.first().and_then(model_skin_index).unwrap_or(-1));
    groups
}

/// `multi_full_model_groups`: the skins drawn from more than one full-body model.
pub fn multi_full_model_groups(models: &[Dict]) -> Vec<Vec<Dict>> {
    full_groups_by_skin(models, 2)
}

/// `first_skin_model_group`: the full-body models of the first skin.
pub fn first_skin_model_group(models: &[Dict]) -> Vec<Dict> {
    full_groups_by_skin(models, 1).into_iter().next().unwrap_or_default()
}

/// `select_model_group`: the one full-body group holding every runtime key, else the one
/// whose keys are exactly those, else every model.
pub fn select_model_group(models: &[Dict], runtime_keys: &[String]) -> Vec<Dict> {
    if models.is_empty() {
        return Vec::new();
    }
    let groups: Vec<Vec<Dict>> = group_models_by_skin_and_kind(models)
        .into_iter()
        .filter(|group| group.first().is_some_and(|first| infer_live2d_model_kind(first) == "full"))
        .collect();
    let candidates: Vec<&Vec<Dict>> = groups
        .iter()
        .filter(|group| {
            runtime_keys
                .iter()
                .all(|key| !key.is_empty() && group.iter().any(|model| model_live2d_key(model) == *key))
        })
        .collect();
    if let [only] = candidates.as_slice() {
        return (*only).clone();
    }
    let wanted: HashSet<&str> = runtime_keys.iter().map(String::as_str).collect();
    let exact: Vec<&Vec<Dict>> = groups
        .iter()
        .filter(|group| {
            let keys: Vec<String> = group.iter().map(model_live2d_key).filter(|key| !key.is_empty()).collect();
            let keys: HashSet<&str> = keys.iter().map(String::as_str).collect();
            !keys.is_empty() && keys == wanted
        })
        .collect();
    if let [only] = exact.as_slice() {
        return (*only).clone();
    }
    models.to_vec()
}

// ---------- runtime animations ----------

/// `_flatten_resource_urls`.
fn flatten_resource_urls(urls: &Dict) -> HashSet<String> {
    let mut out = HashSet::new();
    for (_, value) in urls.iter() {
        match value {
            Value::Str(text) => {
                let normalized = normalize_layer_url(text);
                if !normalized.is_empty() {
                    out.insert(normalized);
                }
            }
            Value::List(items) => {
                out.extend(
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .filter(|item| !py_strip(item).is_empty())
                        .map(normalize_layer_url),
                );
            }
            _ => {}
        }
    }
    out
}

/// `_runtime_player_matches_model`: by model key, else by a shared resource URL.
fn runtime_player_matches_model(player: &RuntimePlayerSnapshot, model: &Dict) -> bool {
    let player_key = player.runtime_key();
    let model_key = model_live2d_key(model);
    if !player_key.is_empty() && !model_key.is_empty() {
        return player_key == model_key;
    }
    let model_urls = flatten_resource_urls(&model_resource_urls(model));
    [&player.atlas_url, &player.skel_url]
        .into_iter()
        .any(|url| !url.is_empty() && model_urls.contains(url))
}

fn track_zero_animation_events(player: &RuntimePlayerSnapshot) -> Vec<&RuntimeAnimationEvent> {
    player
        .events
        .iter()
        .filter(|event| !event.animation.is_empty() && event.event_type == "animationState" && matches!(event.track_index, None | Some(0)))
        .collect()
}

fn runtime_event_state(event: &RuntimeAnimationEvent, match_method: &str, default_loop: bool, duration_ms: Option<i128>) -> Dict {
    let mut state = dict_of(vec![
        ("animation", Value::Str(event.animation.clone())),
        ("enabled", Value::Bool(true)),
        ("loop", Value::Bool(event.r#loop.unwrap_or(default_loop))),
        ("match_method", Value::from(match_method)),
        ("match_confidence", Value::from("high")),
    ]);
    if let Some(duration) = duration_ms {
        state.insert("duration_ms", Value::Int(duration));
    }
    state
}

/// `_runtime_animations_from_player`: the last track-0 animation before the stage click is
/// the idle one, the first after it the click one.
fn runtime_animations_from_player(player: &RuntimePlayerSnapshot, click_start_sequence: Option<i128>) -> Dict {
    let events = track_zero_animation_events(player);
    // `max(..., key=...)` keeps the first of equal maxima.
    let idle_event = events
        .iter()
        .filter(|event| click_start_sequence.is_none_or(|start| event.sequence <= start))
        .fold(None::<&&RuntimeAnimationEvent>, |best, event| match best {
            Some(best) if best.sequence >= event.sequence => Some(best),
            _ => Some(event),
        });
    let click_event = click_start_sequence.and_then(|start| {
        events
            .iter()
            .filter(|event| event.sequence > start)
            .min_by_key(|event| event.sequence)
    });

    let mut animations = Dict::new();
    if let Some(event) = idle_event {
        animations.insert(
            "idle",
            Value::Dict(runtime_event_state(event, RUNTIME_ANIMATION_MATCH_METHOD, true, None)),
        );
    } else if !player.configured_animation.is_empty() {
        animations.insert(
            "idle",
            Value::Dict(dict_of(vec![
                ("animation", Value::Str(player.configured_animation.clone())),
                ("enabled", Value::Bool(true)),
                ("loop", Value::Bool(true)),
                ("match_method", Value::from(RUNTIME_ANIMATION_MATCH_METHOD)),
                ("match_confidence", Value::from("high")),
            ])),
        );
    }
    if let Some(event) = click_event {
        animations.insert(
            "click",
            Value::Dict(runtime_event_state(
                event,
                RUNTIME_CLICK_MATCH_METHOD,
                false,
                player.duration_of(&event.animation),
            )),
        );
    } else if click_start_sequence.is_some() {
        animations.insert(
            "click",
            Value::Dict(dict_of(vec![
                ("enabled", Value::Bool(false)),
                ("match_method", Value::from(RUNTIME_CLICK_MATCH_METHOD)),
                ("match_confidence", Value::from("high")),
            ])),
        );
    }
    animations
}

/// `_runtime_layer_metadata`: from the page's player when there is one, else what the
/// model already carries.
fn runtime_layer_metadata(model: Option<&Dict>, player: Option<&RuntimePlayerSnapshot>, click_start_sequence: Option<i128>) -> Dict {
    let Some(model) = model else {
        return Dict::new();
    };
    let (animations, match_method, match_confidence, mut metadata) = if let Some(player) = player {
        let animations = runtime_animations_from_player(player, click_start_sequence);
        let confidence = if !animations.is_empty() && runtime_player_matches_model(player, model) {
            "high"
        } else {
            "low"
        };
        (
            animations,
            RUNTIME_ANIMATION_MATCH_METHOD.to_owned(),
            confidence.to_owned(),
            dict_of(vec![("runtime_player", Value::Dict(player.to_payload()))]),
        )
    } else {
        (
            model_runtime_animations(model),
            first_text(model, &["runtime_animation_match_method", "runtimeAnimationMatchMethod"]),
            first_text(model, &["runtime_animation_match_confidence", "runtimeAnimationMatchConfidence"]),
            Dict::new(),
        )
    };
    if !animations.is_empty() {
        metadata.insert("runtime_animations", Value::Dict(animations));
    }
    if !match_method.is_empty() {
        metadata.insert("runtime_animation_match_method", Value::Str(match_method));
    }
    if MATCH_CONFIDENCES.contains(&match_confidence.as_str()) {
        metadata.insert("runtime_animation_match_confidence", Value::Str(match_confidence));
    }
    metadata
}

// ---------- payloads ----------

/// `LayerCaptureBuildInput`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayerCaptureBuildInput {
    pub content_id: i128,
    pub title: String,
    pub models: Vec<Dict>,
    pub containers: Vec<RuntimeContainerSnapshot>,
    pub runtime_players: Vec<RuntimePlayerSnapshot>,
    pub runtime_click_start_sequence: Option<i128>,
    pub requested_live2d_keys: Vec<String>,
    /// `''` is the moment of the build ([`utc_now`] is asked).
    pub captured_at: String,
    pub source_url: String,
}

/// `RuntimeGroupCapture`: what one skin's pass over the page read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuntimeGroupCapture {
    pub models: Vec<Dict>,
    pub containers: Vec<RuntimeContainerSnapshot>,
    pub requested_live2d_keys: Vec<String>,
    pub runtime_players: Vec<RuntimePlayerSnapshot>,
    pub runtime_click_start_sequence: Option<i128>,
}

/// The character page a capture reads when the caller names none.
pub fn default_source_url(content_id: i128) -> String {
    format!("{GAMEKEE_BASE_URL}/nikke/tj/{content_id}.html")
}

/// `build_layer_capture_payload`: one skin's layers, matched to its models by runtime key
/// and ordered by the containers' `z-index`. `utc_now` stands in for `_utc_now_iso`.
#[allow(clippy::too_many_lines)] // reason: one Python function, its dict built key by key in Python's order
pub fn build_layer_capture_payload(capture: &LayerCaptureBuildInput, utc_now: &(dyn Fn() -> String + Sync)) -> Dict {
    let captured_at = if capture.captured_at.is_empty() {
        utc_now()
    } else {
        capture.captured_at.clone()
    };
    let source_url = if capture.source_url.is_empty() {
        default_source_url(capture.content_id)
    } else {
        capture.source_url.clone()
    };
    let containers = &capture.containers;
    let player_by_container: BTreeMap<i128, &RuntimePlayerSnapshot> = capture
        .runtime_players
        .iter()
        .filter(|player| player.container_index >= 0)
        .map(|player| (player.container_index, player))
        .collect();
    let requested = unique_in_order(capture.requested_live2d_keys.iter().map(String::as_str));
    let model_keys: HashSet<String> = capture.models.iter().map(model_live2d_key).filter(|key| !key.is_empty()).collect();
    let runtime_model_keys: Vec<String> = requested.iter().filter(|key| model_keys.contains(*key)).cloned().collect();
    let model_group = select_model_group(&capture.models, &runtime_model_keys);
    let mut model_by_runtime_key: Vec<(String, &Dict)> = Vec::new();
    for model in &model_group {
        let key = model_live2d_key(model);
        if key.is_empty() {
            continue;
        }
        match model_by_runtime_key.iter_mut().find(|(existing, _)| *existing == key) {
            Some(entry) => entry.1 = model,
            None => model_by_runtime_key.push((key, model)),
        }
    }
    let model_for = |key: &str| {
        model_by_runtime_key
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, model)| *model)
    };
    let matched: Vec<String> = requested.iter().filter(|key| model_for(key).is_some()).cloned().collect();
    let mut warnings = Vec::new();
    if matched.len() < containers.len() {
        warnings.push("fewer runtime live2d keys than visible containers".to_owned());
    }
    if containers.len() != model_group.len() {
        warnings.push(format!(
            "visible container count {} differs from full model count {}",
            containers.len(),
            model_group.len()
        ));
    }

    let mut layers: Vec<Dict> = Vec::with_capacity(containers.len());
    for (source_layer_index, container) in containers.iter().enumerate() {
        let runtime_key = matched.get(source_layer_index).cloned().unwrap_or_default();
        let model = model_for(&runtime_key).or_else(|| model_group.get(source_layer_index));
        let live2d_key = model.map_or_else(|| runtime_key.clone(), model_live2d_key);
        let z_index = container.z_index;
        let high = model.is_some() && !runtime_key.is_empty() && live2d_key == runtime_key && z_index.is_some();
        #[allow(clippy::cast_possible_wrap)] // reason: a page has far fewer containers than i128::MAX
        let index = source_layer_index as i128;
        let mut layer = dict_of(vec![
            ("content_id", Value::Int(capture.content_id)),
            ("skin_index", int_value(model.and_then(model_skin_index))),
            ("stable_id", Value::Str(model.map(model_stable_id).unwrap_or_default())),
            ("live2d_key", Value::Str(live2d_key)),
            ("resource_urls", Value::Dict(model.map(model_resource_urls).unwrap_or_default())),
            ("source_layer_index", Value::Int(index)),
            ("source_z_index", int_value(z_index)),
            ("is_primary", Value::Bool(source_layer_index == 0)),
            ("layer_match_method", Value::from(LAYER_CAPTURE_MATCH_METHOD)),
            ("layer_match_confidence", Value::from(if high { "high" } else { "low" })),
            ("captured_at", Value::Str(captured_at.clone())),
            ("raw_container", Value::Dict(container.to_raw_container())),
        ]);
        let metadata = runtime_layer_metadata(
            model,
            player_by_container.get(&container.index).copied(),
            capture.runtime_click_start_sequence,
        );
        for (key, value) in metadata.iter() {
            layer.insert(key.clone(), value.clone());
        }
        layers.push(layer);
    }

    let mut ordered: Vec<(i128, i128, usize)> = layers
        .iter()
        .enumerate()
        .filter_map(|(position, layer)| {
            let z_index = to_int(layer.get("source_z_index"))?;
            Some((z_index, to_int(layer.get("source_layer_index")).unwrap_or(0), position))
        })
        .collect();
    ordered.sort_by_key(|(z_index, layer_index, _)| (*z_index, *layer_index));
    for (order, (_, _, position)) in (1_i128..).zip(ordered) {
        layers[position].insert("layer_order", Value::Int(order));
    }

    dict_of(vec![
        ("content_id", Value::Int(capture.content_id)),
        ("source_url", Value::Str(source_url)),
        ("title", Value::Str(capture.title.clone())),
        ("captured_at", Value::Str(captured_at)),
        ("status", Value::from("success")),
        ("layer_capture_schema", Value::Int(LAYER_CAPTURE_SCHEMA)),
        (
            "runtime",
            Value::Dict(dict_of(vec![
                ("requested_live2d_keys", str_list(&requested)),
                ("matched_live2d_keys", str_list(&matched)),
                ("container_count", Value::Int(i128::try_from(containers.len()).unwrap_or(i128::MAX))),
            ])),
        ),
        (
            "fingerprint",
            Value::Str(layer_capture_fingerprint(capture.content_id, &model_group)),
        ),
        ("warnings", str_list(&warnings)),
        ("layers", Value::List(layers.into_iter().map(Value::Dict).collect())),
    ])
}

/// What a capture was asked for, as far as the payload goes.
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadRequest<'a> {
    pub content_id: i128,
    pub title: &'a str,
    pub source_url: &'a str,
    pub models: &'a [Dict],
}

/// `_build_multi_group_layer_capture_payload`: every skin's layers in one payload.
pub fn build_multi_group_layer_capture_payload(
    request: &PayloadRequest<'_>,
    groups: &[RuntimeGroupCapture],
    utc_now: &(dyn Fn() -> String + Sync),
) -> Dict {
    let captured_at = utc_now();
    let source_url = if request.source_url.is_empty() {
        default_source_url(request.content_id)
    } else {
        request.source_url.to_owned()
    };
    let mut layers = Vec::new();
    let mut warnings = Vec::new();
    let mut requested = Vec::new();
    let mut matched = Vec::new();
    let mut captured_models = Vec::new();
    let mut container_count = 0_i128;
    for group in groups {
        let payload = build_layer_capture_payload(
            &LayerCaptureBuildInput {
                content_id: request.content_id,
                title: request.title.to_owned(),
                models: group.models.clone(),
                containers: group.containers.clone(),
                runtime_players: group.runtime_players.clone(),
                runtime_click_start_sequence: group.runtime_click_start_sequence,
                requested_live2d_keys: group.requested_live2d_keys.clone(),
                captured_at: captured_at.clone(),
                source_url: source_url.clone(),
            },
            utc_now,
        );
        if let Some(Value::List(items)) = payload.get("layers") {
            layers.extend(items.iter().cloned());
        }
        if let Some(Value::List(items)) = payload.get("warnings") {
            warnings.extend(items.iter().map(fav_runtime::pystr::str_value));
        }
        if let Some(Value::Dict(runtime)) = payload.get("runtime") {
            for (key, out) in [("requested_live2d_keys", &mut requested), ("matched_live2d_keys", &mut matched)] {
                if let Some(Value::List(items)) = runtime.get(key) {
                    out.extend(items.iter().filter(|item| item.is_truthy()).map(fav_runtime::pystr::str_value));
                }
            }
            container_count += to_int(runtime.get("container_count")).unwrap_or(0);
        }
        captured_models.extend(group.models.iter().cloned());
    }
    dict_of(vec![
        ("content_id", Value::Int(request.content_id)),
        ("source_url", Value::Str(source_url)),
        ("title", Value::from(request.title)),
        ("captured_at", Value::Str(captured_at)),
        ("status", Value::from("success")),
        ("layer_capture_schema", Value::Int(LAYER_CAPTURE_SCHEMA)),
        (
            "runtime",
            Value::Dict(dict_of(vec![
                (
                    "requested_live2d_keys",
                    str_list(&unique_in_order(requested.iter().map(String::as_str))),
                ),
                (
                    "matched_live2d_keys",
                    str_list(&unique_in_order(matched.iter().map(String::as_str))),
                ),
                ("container_count", Value::Int(container_count)),
            ])),
        ),
        (
            "fingerprint",
            Value::Str(layer_capture_fingerprint(request.content_id, &captured_models)),
        ),
        ("warnings", str_list(&unique_in_order(warnings.iter().map(String::as_str)))),
        ("layers", Value::List(layers)),
    ])
}

/// `capture_gamekee_runtime_layers`' assembly: several skins make a multi-group payload;
/// one (or none) a single one, over the request's models when nothing was captured.
pub fn capture_payload(request: &PayloadRequest<'_>, groups: &[RuntimeGroupCapture], utc_now: &(dyn Fn() -> String + Sync)) -> Dict {
    if groups.len() > 1 {
        return build_multi_group_layer_capture_payload(request, groups, utc_now);
    }
    let first = groups.first();
    build_layer_capture_payload(
        &LayerCaptureBuildInput {
            content_id: request.content_id,
            title: request.title.to_owned(),
            models: first.map_or_else(|| request.models.to_vec(), |group| group.models.clone()),
            containers: first.map(|group| group.containers.clone()).unwrap_or_default(),
            runtime_players: first.map(|group| group.runtime_players.clone()).unwrap_or_default(),
            runtime_click_start_sequence: first.and_then(|group| group.runtime_click_start_sequence),
            requested_live2d_keys: first.map(|group| group.requested_live2d_keys.clone()).unwrap_or_default(),
            captured_at: String::new(),
            source_url: request.source_url.to_owned(),
        },
        utc_now,
    )
}
