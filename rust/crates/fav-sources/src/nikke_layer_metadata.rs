//! Live2D layer metadata for NIKKE characters (`src/web/nikke_layer_metadata.py`).
//!
//! A GameKee character page can draw one skin from several full-body Live2D models stacked
//! on top of each other. The runtime capture (WP17) records the stacking order GameKee's
//! own viewer uses; the functions here normalize such a capture, merge it into the
//! manifest's `live2d_models` behind quality gates, fingerprint the models so a capture
//! can be reused, and summarize it for `manifest.json`.
//!
//! Models and captures are the JSON dictionaries Python passes around ([`Dict`]); every
//! report keeps Python's key order, and comparisons follow Python's `==` (`1 == 1.0`).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use fav_core::pyjson::{self, Dict, DumpOptions, Key, Value};
use fav_core::pytext::{py_int_decimal, py_isdigit, py_split, py_strip};
use fav_runtime::pystr::{repr_str, str_value};
use sha2::{Digest, Sha256};

use crate::pydecode::PyError;

pub const GAMEKEE_BASE_URL: &str = "https://www.gamekee.com";
pub const LAYER_MATCH_CONFIDENCE_VALUES: [&str; 3] = ["high", "medium", "low"];
pub const RUNTIME_ANIMATION_STATES: [&str; 2] = ["idle", "click"];
pub const LIVE2D_LAYER_METADATA_FIELDS: [&str; 9] = [
    "layer_order",
    "source_z_index",
    "source_layer_index",
    "is_primary",
    "layer_match_method",
    "layer_match_confidence",
    "runtime_animations",
    "runtime_animation_match_method",
    "runtime_animation_match_confidence",
];
pub const LIVE2D_LAYER_CAPTURE_FIELD: &str = "live2d_layer_capture";
pub const LAYER_CAPTURE_MATCH_METHOD: &str = "gamekee-runtime-container";
pub const RUNTIME_ANIMATION_CAPTURE_SCHEMA: i128 = 2;
const MIN_LAYER_GROUP_SIZE: usize = 2;
pub const LAYER_CAPTURE_RAW_PATH: &str = "raw/live2d-layer-capture.json";
const MAX_SUMMARY_TEXT_LENGTH: usize = 240;
const MAX_SUMMARY_WARNING_COUNT: usize = 20;

/// `(skin_index, model_kind)`: the models one skin draws together.
pub type GroupKey = (Option<i128>, String);

// ---------- Python value helpers ----------

/// `_json_dumps`: compact, sorted keys, `ensure_ascii=False`.
pub fn json_dumps(value: &Value) -> String {
    // Values here come from `json.loads` or from this code, so every key is a string and
    // sorting them cannot fail.
    pyjson::dumps(value, &DumpOptions::hash()).expect("string keys always sort")
}

/// `_pretty_json`: what the manifest writers put on disk.
pub fn pretty_json(value: &Value) -> String {
    let mut text = pyjson::dumps(value, &DumpOptions::pretty()).expect("string keys always sort");
    text.push('\n');
    text
}

/// `hashlib.sha256(text.encode()).hexdigest()`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `_to_int`: an `int` (not a `bool`), or a string of digits after `strip()`.
///
/// Python's `int()` raises for digits that are not decimal (superscripts pass
/// `isdigit()`); those are `None` here (DIVERGENCES.md D83).
pub fn to_int(value: Option<&Value>) -> Option<i128> {
    match value? {
        Value::Int(int) => Some(*int),
        Value::Str(text) => {
            let text = py_strip(text);
            if py_isdigit(text) { py_int_decimal(text) } else { None }
        }
        _ => None,
    }
}

fn int_value(value: Option<i128>) -> Value {
    value.map_or(Value::Null, Value::Int)
}

fn optional_bool(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(flag)) => Some(*flag),
        _ => None,
    }
}

fn optional_non_negative_int(value: Option<&Value>) -> Option<i128> {
    to_int(value).filter(|parsed| *parsed >= 0)
}

/// `data.get(key, default)`: the default only when the key is missing.
fn get_or<'a>(data: &'a Dict, key: &str, default: Option<&'a Value>) -> Option<&'a Value> {
    if data.contains_key(key) { data.get(key) } else { default }
}

/// `data.get(first, data.get(second))`.
fn get_either<'a>(data: &'a Dict, first: &str, second: &str) -> Option<&'a Value> {
    get_or(data, first, data.get(second))
}

fn first_int(data: &Dict, keys: &[&str]) -> Option<i128> {
    keys.iter().find_map(|key| to_int(data.get(key)))
}

/// `_first_text`: the first non-`None` value whose `str()` is not blank, stripped.
pub fn first_text(data: &Dict, keys: &[&str]) -> String {
    for key in keys {
        match data.get(key) {
            None | Some(Value::Null) => {}
            Some(value) => {
                let text = str_value(value);
                let text = py_strip(&text);
                if !text.is_empty() {
                    return text.to_owned();
                }
            }
        }
    }
    String::new()
}

/// `str(data.get(key) or '')`.
pub fn str_or_empty(data: &Dict, key: &str) -> String {
    match data.get(key) {
        Some(value) if value.is_truthy() => str_value(value),
        _ => String::new(),
    }
}

/// `data.get(key) or ''` (the value itself when it is truthy).
pub fn value_or_empty(data: &Dict, key: &str) -> Value {
    match data.get(key) {
        Some(value) if value.is_truthy() => value.clone(),
        _ => Value::Str(String::new()),
    }
}

/// Python's `==` between two JSON values: numbers compare by value across `bool`, `int`
/// and `float`; containers compare element by element.
pub fn py_eq(left: &Value, right: &Value) -> bool {
    fn number(value: &Value) -> Option<Result<i128, f64>> {
        match value {
            Value::Bool(flag) => Some(Ok(i128::from(*flag))),
            Value::Int(int) => Some(Ok(*int)),
            Value::Float(float) => Some(Err(*float)),
            _ => None,
        }
    }
    if let (Some(a), Some(b)) = (number(left), number(right)) {
        return match (a, b) {
            (Ok(a), Ok(b)) => a == b,
            #[allow(clippy::float_cmp)] // reason: Python compares floats exactly
            (Err(a), Err(b)) => a == b,
            (Ok(int), Err(float)) | (Err(float), Ok(int)) => int_eq_float(int, float),
        };
    }
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::List(a), Value::List(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| py_eq(x, y)),
        (Value::Dict(a), Value::Dict(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, x)| b.iter().any(|(other, y)| key_eq(key, other) && py_eq(x, y)))
        }
        _ => false,
    }
}

fn key_eq(left: &Key, right: &Key) -> bool {
    match (left, right) {
        (Key::Str(a), Key::Str(b)) => a == b,
        (Key::Str(_), _) | (_, Key::Str(_)) => false,
        _ => left == right,
    }
}

fn int_eq_float(int: i128, float: f64) -> bool {
    if !float.is_finite() || float.fract() != 0.0 || float.abs() >= 1.7e38 {
        return false;
    }
    #[allow(clippy::cast_possible_truncation)] // reason: integral and within i128, checked above
    let whole = float as i128;
    whole == int
}

fn str_list(items: impl IntoIterator<Item = String>) -> Value {
    Value::List(items.into_iter().map(Value::Str).collect())
}

fn dict_of(entries: Vec<(&str, Value)>) -> Dict {
    let mut dict = Dict::new();
    for (key, value) in entries {
        dict.insert(key, value);
    }
    dict
}

fn truthy(dict: &Dict, key: &str) -> bool {
    dict.get(key).is_some_and(Value::is_truthy)
}

// ---------- runtime animations ----------

fn runtime_match_confidence(value: Option<&Value>) -> Option<String> {
    let text = match value {
        Some(value) if value.is_truthy() => str_value(value),
        _ => String::new(),
    };
    let text = py_strip(&text);
    LAYER_MATCH_CONFIDENCE_VALUES.contains(&text).then(|| text.to_owned())
}

fn normalize_runtime_animation_state(value: Option<&Value>) -> Option<Dict> {
    let Some(Value::Dict(value)) = value else {
        return None;
    };
    let animation = first_text(value, &["animation"]);
    let enabled = optional_bool(value.get("enabled"));
    let looped = optional_bool(value.get("loop"));
    let match_method = first_text(value, &["match_method", "matchMethod"]);
    let match_confidence = runtime_match_confidence(get_either(value, "match_confidence", "matchConfidence"));
    let duration_ms = optional_non_negative_int(get_either(value, "duration_ms", "durationMs"));

    let mut out = Dict::new();
    if !animation.is_empty() {
        out.insert("animation", Value::Str(animation));
    }
    if let Some(enabled) = enabled {
        out.insert("enabled", Value::Bool(enabled));
    }
    if let Some(looped) = looped {
        out.insert("loop", Value::Bool(looped));
    }
    if !match_method.is_empty() {
        out.insert("match_method", Value::Str(match_method));
    }
    if let Some(confidence) = match_confidence {
        out.insert("match_confidence", Value::Str(confidence));
    }
    if let Some(duration) = duration_ms {
        out.insert("duration_ms", Value::Int(duration));
    }
    (!out.is_empty()).then_some(out)
}

/// `normalize_runtime_animations`: the `idle` and `click` states that carry anything.
pub fn normalize_runtime_animations(value: Option<&Value>) -> Dict {
    let mut out = Dict::new();
    let Some(Value::Dict(value)) = value else {
        return out;
    };
    for state in RUNTIME_ANIMATION_STATES {
        if let Some(normalized) = normalize_runtime_animation_state(value.get(state)) {
            out.insert(state, Value::Dict(normalized));
        }
    }
    out
}

// ---------- URLs and identities ----------

/// `normalize_layer_url`: protocol-relative and site-relative URLs made absolute.
pub fn normalize_layer_url(url: &str) -> String {
    let value = py_strip(url);
    if value.is_empty() {
        return String::new();
    }
    if let Some(rest) = value.strip_prefix("//") {
        return format!("https://{rest}");
    }
    if value.starts_with('/') {
        // A path joined onto the fixed GameKee origin has no network location to reject.
        return fav_core::pyurl::urljoin(GAMEKEE_BASE_URL, value).unwrap_or_else(|_| value.to_owned());
    }
    value.to_owned()
}

/// `copy_live2d_layer_metadata`: the layer fields of a GameKee entry (snake or camel case)
/// copied onto a model.
pub fn copy_live2d_layer_metadata(model: &mut Dict, value: &Dict) {
    let layer_order = first_int(value, &["layer_order", "layerOrder"]);
    let source_z_index = first_int(value, &["source_z_index", "sourceZIndex"]);
    let source_layer_index = first_int(value, &["source_layer_index", "sourceLayerIndex"]);
    let is_primary = if value.contains_key("is_primary") {
        optional_bool(value.get("is_primary"))
    } else {
        optional_bool(value.get("isPrimary"))
    };
    let layer_match_method = first_text(value, &["layer_match_method", "layerMatchMethod"]);
    let layer_match_confidence = first_text(value, &["layer_match_confidence", "layerMatchConfidence"]);
    let runtime_animations = normalize_runtime_animations(get_either(value, "runtime_animations", "runtimeAnimations"));
    let runtime_animation_match_method = first_text(value, &["runtime_animation_match_method", "runtimeAnimationMatchMethod"]);
    let runtime_animation_match_confidence = runtime_match_confidence(get_either(
        value,
        "runtime_animation_match_confidence",
        "runtimeAnimationMatchConfidence",
    ));

    if let Some(order) = layer_order {
        model.insert("layer_order", Value::Int(order));
    }
    if let Some(index) = source_z_index {
        model.insert("source_z_index", Value::Int(index));
    }
    if let Some(index) = source_layer_index {
        model.insert("source_layer_index", Value::Int(index));
    }
    if let Some(primary) = is_primary {
        model.insert("is_primary", Value::Bool(primary));
    }
    if !layer_match_method.is_empty() {
        model.insert("layer_match_method", Value::Str(layer_match_method));
    }
    if LAYER_MATCH_CONFIDENCE_VALUES.contains(&layer_match_confidence.as_str()) {
        model.insert("layer_match_confidence", Value::Str(layer_match_confidence));
    }
    if !runtime_animations.is_empty() {
        model.insert("runtime_animations", Value::Dict(runtime_animations));
    }
    if !runtime_animation_match_method.is_empty() {
        model.insert("runtime_animation_match_method", Value::Str(runtime_animation_match_method));
    }
    if let Some(confidence) = runtime_animation_match_confidence {
        model.insert("runtime_animation_match_confidence", Value::Str(confidence));
    }
}

/// `_model_identity`: `stable_id/live2d_key`, whichever exists, or `<unknown>`.
pub fn model_identity(model: &Dict) -> String {
    let stable_id = str_or_empty(model, "stable_id");
    let live2d_key = str_or_empty(model, "live2d_key");
    if !stable_id.is_empty() && !live2d_key.is_empty() {
        return format!("{stable_id}/{live2d_key}");
    }
    if !stable_id.is_empty() {
        return stable_id;
    }
    if !live2d_key.is_empty() {
        return live2d_key;
    }
    "<unknown>".to_owned()
}

fn collect_urls(values: &[&Value], out: &mut BTreeSet<String>) {
    for value in values {
        match value {
            Value::Str(text) if !py_strip(text).is_empty() => {
                out.insert(normalize_layer_url(text));
            }
            Value::List(items) => {
                for item in items {
                    if let Value::Str(text) = item
                        && !py_strip(text).is_empty()
                    {
                        out.insert(normalize_layer_url(text));
                    }
                }
            }
            _ => {}
        }
    }
    out.remove("");
}

fn model_urls(model: &Dict) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(Value::Dict(urls)) = model.get("urls") {
        let values: Vec<&Value> = urls.iter().map(|(_, value)| value).collect();
        collect_urls(&values, &mut out);
    }
    out
}

fn capture_resource_urls(capture: &Dict) -> BTreeSet<String> {
    let raw = match capture.get("resource_urls") {
        None | Some(Value::Null) => capture.get("urls"),
        other => other,
    };
    let values: Vec<&Value> = match raw {
        Some(Value::Dict(urls)) => urls.iter().map(|(_, value)| value).collect(),
        Some(Value::List(items)) => items.iter().collect(),
        _ => Vec::new(),
    };
    let mut out = BTreeSet::new();
    collect_urls(&values, &mut out);
    out
}

fn fingerprint_resource_urls(model: &Dict) -> Dict {
    let mut out = Dict::new();
    let Some(Value::Dict(urls)) = model.get("urls") else {
        return out;
    };
    for field in ["atlas", "skel", "json", "image"] {
        match urls.get(field) {
            Some(Value::Str(text)) if !py_strip(text).is_empty() => out.insert(field, Value::Str(normalize_layer_url(text))),
            Some(Value::List(items)) => {
                let mut normalized: Vec<String> = items
                    .iter()
                    .filter_map(|item| match item {
                        Value::Str(text) if !py_strip(text).is_empty() => Some(normalize_layer_url(text)),
                        _ => None,
                    })
                    .collect();
                normalized.sort();
                if !normalized.is_empty() {
                    out.insert(field, str_list(normalized));
                }
            }
            _ => {}
        }
    }
    out
}

/// `live2d_layer_fingerprint`: a hash of the models' identities and resource URLs, so a
/// capture taken for the same set of models can be reused.
pub fn live2d_layer_fingerprint(content_id: i128, models: &[&Dict]) -> String {
    let mut entries: Vec<(i128, String, String, String, Dict)> = models
        .iter()
        .map(|model| {
            let skin_index = to_int(model.get("skin_index"));
            let urls = fingerprint_resource_urls(model);
            let mut entry = Dict::new();
            entry.insert("skin_index", int_value(skin_index));
            entry.insert("stable_id", Value::Str(str_or_empty(model, "stable_id")));
            entry.insert("live2d_key", Value::Str(str_or_empty(model, "live2d_key")));
            let dumped = json_dumps(&Value::Dict(urls.clone()));
            entry.insert("resource_urls", Value::Dict(urls));
            (
                skin_index.filter(|index| *index != 0).unwrap_or(-1),
                str_or_empty(model, "stable_id"),
                str_or_empty(model, "live2d_key"),
                dumped,
                entry,
            )
        })
        .collect();
    entries.sort_by(|a, b| (a.0, &a.1, &a.2, &a.3).cmp(&(b.0, &b.1, &b.2, &b.3)));
    let mut payload = Dict::new();
    payload.insert("content_id", Value::Int(content_id));
    payload.insert(
        "models",
        Value::List(entries.into_iter().map(|entry| Value::Dict(entry.4)).collect()),
    );
    sha256_hex(json_dumps(&Value::Dict(payload)).as_bytes())
}

/// `_layer_group_key`.
pub fn layer_group_key(model: &Dict) -> GroupKey {
    (to_int(model.get("skin_index")), infer_live2d_model_kind(model))
}

/// Group models by key, in first-seen order.
fn group_indexes(models: &[&Dict]) -> Vec<(GroupKey, Vec<usize>)> {
    let mut groups: Vec<(GroupKey, Vec<usize>)> = Vec::new();
    for (index, model) in models.iter().enumerate() {
        let key = layer_group_key(model);
        match groups.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, members)) => members.push(index),
            None => groups.push((key, vec![index])),
        }
    }
    groups
}

/// `_multi_full_model_groups`: the indexes of each full-body group with several models.
fn multi_full_model_groups(models: &[&Dict]) -> Vec<Vec<usize>> {
    group_indexes(models)
        .into_iter()
        .filter(|((_, kind), members)| kind == "full" && members.len() > 1)
        .map(|(_, members)| members)
        .collect()
}

/// `live2d_layer_fingerprint_candidates`: the fingerprint of all models, of the stacked
/// groups together, and of each stacked group, without repeats.
pub fn live2d_layer_fingerprint_candidates(content_id: i128, models: &[&Dict]) -> Vec<String> {
    let groups = multi_full_model_groups(models);
    let mut fingerprints = vec![live2d_layer_fingerprint(content_id, models)];
    if !groups.is_empty() {
        let all: Vec<&Dict> = groups.iter().flatten().map(|index| models[*index]).collect();
        fingerprints.push(live2d_layer_fingerprint(content_id, &all));
    }
    for group in &groups {
        let members: Vec<&Dict> = group.iter().map(|index| models[*index]).collect();
        fingerprints.push(live2d_layer_fingerprint(content_id, &members));
    }
    let mut unique: Vec<String> = Vec::new();
    for fingerprint in fingerprints {
        if !unique.contains(&fingerprint) {
            unique.push(fingerprint);
        }
    }
    unique
}

fn group_payload(key: &GroupKey) -> Value {
    Value::Dict(dict_of(vec![
        ("skin_index", int_value(key.0)),
        ("model_kind", Value::Str(key.1.clone())),
    ]))
}

/// `sorted(keys, key=str)`: the order of `str((skin_index, kind))`.
fn sorted_by_str(keys: &BTreeSet<GroupKey>) -> Vec<GroupKey> {
    let mut keys: Vec<(String, GroupKey)> = keys
        .iter()
        .map(|key| {
            let skin = key.0.map_or_else(|| "None".to_owned(), |index| index.to_string());
            (format!("({skin}, {})", repr_str(&key.1)), key.clone())
        })
        .collect();
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    keys.into_iter().map(|(_, key)| key).collect()
}

fn as_refs(models: &[Dict]) -> Vec<&Dict> {
    models.iter().collect()
}

/// `layer_capture_group_coverage`: which stacked groups a capture covers.
pub fn layer_capture_group_coverage(content_id: i128, models: &[Dict], capture_payload: &Dict) -> Dict {
    let refs = as_refs(models);
    let required: BTreeSet<GroupKey> = multi_full_model_groups(&refs)
        .iter()
        .flatten()
        .map(|index| layer_group_key(&models[*index]))
        .collect();
    let mut covered: BTreeSet<GroupKey> = BTreeSet::new();
    let mut unmatched: Vec<Value> = Vec::new();

    for layer in normalized_layer_capture(capture_payload) {
        if let Some(layer_content_id) = to_int(layer.get("content_id"))
            && layer_content_id != content_id
        {
            unmatched.push(Value::Dict(dict_of(vec![
                ("reason", Value::from("content_id_mismatch")),
                ("capture_content_id", Value::Int(layer_content_id)),
                ("expected_content_id", Value::Int(content_id)),
                ("stable_id", value_or_empty(&layer, "stable_id")),
                ("live2d_key", value_or_empty(&layer, "live2d_key")),
            ])));
            continue;
        }
        let candidates: Vec<&Dict> = models.iter().filter(|model| capture_matches_model(&layer, model)).collect();
        if candidates.len() != 1 {
            unmatched.push(Value::Dict(dict_of(vec![
                (
                    "reason",
                    Value::from(if candidates.is_empty() { "no_match" } else { "ambiguous_match" }),
                ),
                ("candidate_count", Value::Int(candidates.len() as i128)),
                ("stable_id", value_or_empty(&layer, "stable_id")),
                ("live2d_key", value_or_empty(&layer, "live2d_key")),
            ])));
            continue;
        }
        let key = layer_group_key(candidates[0]);
        if required.contains(&key) {
            covered.insert(key);
        }
    }

    let missing: BTreeSet<GroupKey> = required.difference(&covered).cloned().collect();
    let payloads = |keys: &BTreeSet<GroupKey>| Value::List(sorted_by_str(keys).iter().map(group_payload).collect());
    dict_of(vec![
        ("required_groups", payloads(&required)),
        ("covered_groups", payloads(&covered)),
        ("missing_groups", payloads(&missing)),
        ("unmatched_layers", Value::List(unmatched)),
        ("complete", Value::Bool(missing.is_empty())),
    ])
}

/// `_normalized_layer_capture`: the capture's layers in one shape, with missing layer
/// orders filled in from the z-index when a skin has several layers.
pub fn normalized_layer_capture(payload: &Dict) -> Vec<Dict> {
    let Some(Value::List(raw_layers)) = get_either(payload, "layers", "captures") else {
        return Vec::new();
    };
    let content_id = to_int(payload.get("content_id"));
    let captured_at = first_text(payload, &["captured_at", "capturedAt"]);
    let mut out: Vec<Dict> = Vec::new();
    for raw_layer in raw_layers {
        let Value::Dict(raw_layer) = raw_layer else {
            continue;
        };
        let mut layer = raw_layer.clone();
        if let Some(content_id) = content_id
            && to_int(layer.get("content_id")).is_none()
        {
            layer.insert("content_id", Value::Int(content_id));
        }
        if !captured_at.is_empty() && first_text(&layer, &["captured_at", "capturedAt"]).is_empty() {
            layer.insert("captured_at", Value::Str(captured_at.clone()));
        }

        let layer_order = first_int(&layer, &["layer_order", "layerOrder"]);
        let source_z_index = first_int(&layer, &["source_z_index", "sourceZIndex"]);
        let source_layer_index = first_int(&layer, &["source_layer_index", "sourceLayerIndex"]);
        let skin_index = first_int(&layer, &["skin_index", "skinIndex"]);
        let is_primary = if layer.contains_key("is_primary") {
            optional_bool(layer.get("is_primary"))
        } else {
            optional_bool(layer.get("isPrimary"))
        };
        let confidence = first_text(&layer, &["layer_match_confidence", "layerMatchConfidence"]);
        let mut method = first_text(&layer, &["layer_match_method", "layerMatchMethod"]);
        if method.is_empty() {
            LAYER_CAPTURE_MATCH_METHOD.clone_into(&mut method);
        }
        let runtime_animations = normalize_runtime_animations(get_either(&layer, "runtime_animations", "runtimeAnimations"));
        let runtime_method = first_text(&layer, &["runtime_animation_match_method", "runtimeAnimationMatchMethod"]);
        let runtime_confidence = runtime_match_confidence(get_either(
            &layer,
            "runtime_animation_match_confidence",
            "runtimeAnimationMatchConfidence",
        ));

        let mut normalized = dict_of(vec![
            ("content_id", int_value(to_int(layer.get("content_id")))),
            ("skin_index", int_value(skin_index)),
            ("stable_id", Value::Str(first_text(&layer, &["stable_id", "stableId"]))),
            ("live2d_key", Value::Str(first_text(&layer, &["live2d_key", "live2dKey"]))),
            ("layer_order", int_value(layer_order)),
            ("source_z_index", int_value(source_z_index)),
            ("source_layer_index", int_value(source_layer_index)),
            ("is_primary", is_primary.map_or(Value::Null, Value::Bool)),
            ("layer_match_method", Value::Str(method)),
            (
                "layer_match_confidence",
                Value::from(if LAYER_MATCH_CONFIDENCE_VALUES.contains(&confidence.as_str()) {
                    confidence.as_str()
                } else {
                    "low"
                }),
            ),
            ("captured_at", Value::Str(first_text(&layer, &["captured_at", "capturedAt"]))),
            ("resource_urls", str_list(capture_resource_urls(&layer))),
        ]);
        if !runtime_animations.is_empty() {
            normalized.insert("runtime_animations", Value::Dict(runtime_animations));
        }
        if !runtime_method.is_empty() {
            normalized.insert("runtime_animation_match_method", Value::Str(runtime_method));
        }
        if let Some(confidence) = runtime_confidence {
            normalized.insert("runtime_animation_match_confidence", Value::Str(confidence));
        }
        if let Some(Value::Dict(raw_container)) = get_either(&layer, "raw_container", "rawContainer") {
            normalized.insert("raw_container", Value::Dict(raw_container.clone()));
        }
        out.push(normalized);
    }
    fill_missing_capture_layer_orders(&mut out);
    out
}

/// The `(content_id, skin_index)` group of a captured layer, and the indexes of its layers.
type CaptureOrderGroup = ((Option<i128>, Option<i128>), Vec<usize>);

fn fill_missing_capture_layer_orders(layers: &mut [Dict]) {
    let mut grouped: Vec<CaptureOrderGroup> = Vec::new();
    for (index, layer) in layers.iter().enumerate() {
        if to_int(layer.get("layer_order")).is_some() || to_int(layer.get("source_z_index")).is_none() {
            continue;
        }
        let key = (to_int(layer.get("content_id")), to_int(layer.get("skin_index")));
        match grouped.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, members)) => members.push(index),
            None => grouped.push((key, vec![index])),
        }
    }
    for (_, mut members) in grouped {
        if members.len() < MIN_LAYER_GROUP_SIZE {
            continue;
        }
        let sort_key = |index: &usize| {
            let layer = &layers[*index];
            (
                to_int(layer.get("source_z_index")).unwrap_or(0),
                to_int(layer.get("source_layer_index")).unwrap_or(0),
                str_or_empty(layer, "stable_id"),
                str_or_empty(layer, "live2d_key"),
            )
        };
        members.sort_by_key(sort_key);
        for (order, index) in members.into_iter().enumerate() {
            layers[index].insert("layer_order", Value::Int(order as i128 + 1));
        }
    }
}

fn layer_capture_updates(capture: &Dict) -> Dict {
    let mut updates = Dict::new();
    for field in LIVE2D_LAYER_METADATA_FIELDS {
        let raw = capture.get(field);
        let value = match field {
            "layer_order" | "source_z_index" | "source_layer_index" => to_int(raw).map(Value::Int),
            "is_primary" => optional_bool(raw).map(Value::Bool),
            "layer_match_confidence" => match raw {
                Some(Value::Str(text)) if LAYER_MATCH_CONFIDENCE_VALUES.contains(&text.as_str()) => Some(Value::Str(text.clone())),
                _ => None,
            },
            "runtime_animations" => Some(Value::Dict(normalize_runtime_animations(raw))),
            "runtime_animation_match_confidence" => runtime_match_confidence(raw).map(Value::Str),
            _ => match raw {
                Some(Value::Str(text)) => Some(Value::Str(py_strip(text).to_owned())),
                _ => None,
            },
        };
        let Some(value) = value else {
            continue;
        };
        let empty = match &value {
            Value::Str(text) => text.is_empty(),
            Value::Dict(dict) => dict.is_empty(),
            _ => false,
        };
        if !empty {
            updates.insert(field, value);
        }
    }
    updates
}

fn capture_matches_model(capture: &Dict, model: &Dict) -> bool {
    let stable_id = str_or_empty(capture, "stable_id");
    let live2d_key = str_or_empty(capture, "live2d_key");
    if !stable_id.is_empty() && str_or_empty(model, "stable_id") != stable_id {
        return false;
    }
    if !live2d_key.is_empty() && str_or_empty(model, "live2d_key") != live2d_key {
        return false;
    }
    if !stable_id.is_empty() || !live2d_key.is_empty() {
        return true;
    }
    let capture_urls: BTreeSet<String> = match capture.get("resource_urls") {
        Some(Value::List(items)) => items.iter().map(str_value).collect(),
        _ => BTreeSet::new(),
    };
    !capture_urls.is_empty() && !capture_urls.is_disjoint(&model_urls(model))
}

/// `has_multi_full_layer_groups`: whether any skin stacks several full-body models.
pub fn has_multi_full_layer_groups(models: &[Dict]) -> bool {
    !multi_full_model_groups(&as_refs(models)).is_empty()
}

fn issue_group_key(issue: &Dict) -> GroupKey {
    let kind = match issue.get("model_kind") {
        Some(value) if value.is_truthy() => str_value(value),
        _ => "full".to_owned(),
    };
    (to_int(issue.get("skin_index")), kind)
}

fn blocked_layer_group_keys(issues: &[Value]) -> BTreeSet<GroupKey> {
    issues
        .iter()
        .filter_map(Value::as_dict)
        .filter(|issue| matches!(issue.get("severity"), Some(Value::Str(text)) if text == "error"))
        .map(issue_group_key)
        .collect()
}

fn skipped_layer_group_keys(skipped: &[Value]) -> BTreeSet<GroupKey> {
    skipped
        .iter()
        .filter_map(Value::as_dict)
        .filter(|item| to_int(item.get("skin_index")).is_some())
        .map(issue_group_key)
        .collect()
}

fn clear_live2d_layer_metadata(model: &mut Dict) {
    for field in LIVE2D_LAYER_METADATA_FIELDS {
        model.remove(field);
    }
}

struct PlannedChange {
    model_index: usize,
    updates: Dict,
    change: Dict,
    has_changed_fields: bool,
}

fn missing_capture_layer_issues(projected: &[Dict], planned: &[PlannedChange]) -> Vec<Value> {
    let mut by_group: Vec<(GroupKey, BTreeSet<usize>)> = Vec::new();
    for change in planned {
        let key = layer_group_key(&projected[change.model_index]);
        match by_group.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, indexes)) => {
                indexes.insert(change.model_index);
            }
            None => by_group.push((key, BTreeSet::from([change.model_index]))),
        }
    }
    let mut issues = Vec::new();
    for (key, planned_indexes) in by_group {
        if key.1 != "full" {
            continue;
        }
        let group: BTreeSet<usize> = projected
            .iter()
            .enumerate()
            .filter(|(_, model)| layer_group_key(model) == key)
            .map(|(index, _)| index)
            .collect();
        if group.len() <= 1 {
            continue;
        }
        let missing: Vec<usize> = group.difference(&planned_indexes).copied().collect();
        if !missing.is_empty() {
            issues.push(Value::Dict(dict_of(vec![
                ("severity", Value::from("error")),
                ("code", Value::from("missing_capture_layer")),
                ("skin_index", int_value(key.0)),
                ("model_kind", Value::Str(key.1.clone())),
                ("models", str_list(missing.iter().map(|index| model_identity(&projected[*index])))),
            ])));
        }
    }
    issues
}

fn sorted_keys(dict: &Dict) -> Value {
    let mut keys: Vec<String> = dict
        .iter()
        .map(|(key, _)| match key {
            Key::Str(text) => text.clone(),
            other => fav_runtime::pystr::str_key(other),
        })
        .collect();
    keys.sort();
    str_list(keys)
}

/// `merge_live2d_layer_captures`: match each captured layer to one model and apply its
/// layer fields, unless a quality gate fails for the model's group. With `dry_run` the
/// models stay as they are and only the report is built.
#[allow(clippy::too_many_lines)] // The plan, the gates and the apply pass of the Python function, in its order.
pub fn merge_live2d_layer_captures(models: &mut [Dict], capture_payload: &Dict, content_id: Option<i128>, dry_run: bool) -> Dict {
    let layers = normalized_layer_capture(capture_payload);
    let mut projected: Vec<Dict> = models.to_vec();
    let mut matched = 0_i128;
    let mut skipped: Vec<Value> = Vec::new();
    let mut planned: Vec<PlannedChange> = Vec::new();

    for layer in &layers {
        let layer_content_id = to_int(layer.get("content_id"));
        if let (Some(expected), Some(found)) = (content_id, layer_content_id)
            && found != expected
        {
            skipped.push(Value::Dict(dict_of(vec![
                ("reason", Value::from("content_id_mismatch")),
                ("capture_content_id", Value::Int(found)),
                ("expected_content_id", Value::Int(expected)),
                ("stable_id", value_or_empty(layer, "stable_id")),
                ("live2d_key", value_or_empty(layer, "live2d_key")),
            ])));
            continue;
        }

        let candidates: Vec<usize> = projected
            .iter()
            .enumerate()
            .filter(|(_, model)| capture_matches_model(layer, model))
            .map(|(index, _)| index)
            .collect();
        if candidates.len() != 1 {
            skipped.push(Value::Dict(dict_of(vec![
                (
                    "reason",
                    Value::from(if candidates.is_empty() { "no_match" } else { "ambiguous_match" }),
                ),
                ("candidate_count", Value::Int(candidates.len() as i128)),
                ("stable_id", value_or_empty(layer, "stable_id")),
                ("live2d_key", value_or_empty(layer, "live2d_key")),
                ("source_z_index", layer.get("source_z_index").cloned().unwrap_or(Value::Null)),
                (
                    "source_layer_index",
                    layer.get("source_layer_index").cloned().unwrap_or(Value::Null),
                ),
            ])));
            continue;
        }

        let model_index = candidates[0];
        let updates = layer_capture_updates(layer);
        if !(updates.contains_key("layer_order") && updates.contains_key("is_primary")) {
            let model = &projected[model_index];
            skipped.push(Value::Dict(dict_of(vec![
                ("reason", Value::from("incomplete_layer_metadata")),
                ("stable_id", value_or_empty(layer, "stable_id")),
                ("live2d_key", value_or_empty(layer, "live2d_key")),
                ("skin_index", int_value(to_int(model.get("skin_index")))),
                ("model_kind", Value::Str(infer_live2d_model_kind(model))),
                ("model_index", Value::Int(model_index as i128)),
                ("fields", sorted_keys(&updates)),
            ])));
            continue;
        }

        let original = &models[model_index];
        let mut changed_fields: Vec<String> = updates
            .iter()
            .filter(|(key, value)| {
                let Key::Str(key) = key else { return true };
                !original.get(key).is_some_and(|current| py_eq(current, value))
            })
            .map(|(key, _)| fav_runtime::pystr::str_key(key))
            .collect();
        changed_fields.sort();
        let model = &projected[model_index];
        let change = dict_of(vec![
            ("model", Value::Str(model_identity(model))),
            ("stable_id", value_or_empty(model, "stable_id")),
            ("live2d_key", value_or_empty(model, "live2d_key")),
            ("fields", Value::Dict(updates.clone())),
            ("changed_fields", str_list(changed_fields.iter().cloned())),
        ]);
        matched += 1;
        for (key, value) in updates.iter() {
            projected[model_index].insert(key.clone(), value.clone());
        }
        planned.push(PlannedChange {
            model_index,
            updates,
            change,
            has_changed_fields: !changed_fields.is_empty(),
        });
    }

    let projected_refs = as_refs(&projected);
    let mut affected: BTreeSet<GroupKey> = planned
        .iter()
        .map(|change| layer_group_key(projected_refs[change.model_index]))
        .collect();
    affected.extend(skipped_layer_group_keys(&skipped));
    let mut quality_issues: Vec<Value> = validate_live2d_layer_metadata(&projected)
        .into_iter()
        .filter(|issue| issue.as_dict().is_some_and(|issue| affected.contains(&issue_group_key(issue))))
        .collect();
    quality_issues.extend(missing_capture_layer_issues(&projected, &planned));
    let mut blocked_keys = blocked_layer_group_keys(&quality_issues);
    blocked_keys.extend(skipped_layer_group_keys(&skipped));

    if !dry_run {
        for (index, model) in projected.iter().enumerate() {
            if blocked_keys.contains(&layer_group_key(model)) {
                clear_live2d_layer_metadata(&mut models[index]);
            }
        }
    }
    let (mut changed_count, mut unchanged_count) = (0_i128, 0_i128);
    let mut blocked: Vec<Value> = Vec::new();
    let mut changes: Vec<Value> = Vec::new();
    for plan in planned {
        let projected_model = &projected[plan.model_index];
        if blocked_keys.contains(&layer_group_key(projected_model)) {
            blocked.push(Value::Dict(dict_of(vec![
                ("reason", Value::from("quality_gates_failed")),
                ("model", plan.change.get("model").cloned().unwrap_or(Value::Null)),
                ("stable_id", plan.change.get("stable_id").cloned().unwrap_or(Value::Null)),
                ("live2d_key", plan.change.get("live2d_key").cloned().unwrap_or(Value::Null)),
                ("skin_index", int_value(to_int(projected_model.get("skin_index")))),
                ("model_kind", Value::Str(infer_live2d_model_kind(projected_model))),
                ("fields", sorted_keys(&plan.updates)),
            ])));
            continue;
        }
        changes.push(Value::Dict(plan.change));
        if plan.has_changed_fields {
            changed_count += 1;
            if !dry_run {
                for (key, value) in plan.updates.iter() {
                    models[plan.model_index].insert(key.clone(), value.clone());
                }
            }
        } else {
            unchanged_count += 1;
        }
    }

    let report_content_id = content_id.or_else(|| to_int(capture_payload.get("content_id")));
    dict_of(vec![
        ("content_id", int_value(report_content_id)),
        ("dry_run", Value::Bool(dry_run)),
        ("layer_count", Value::Int(layers.len() as i128)),
        ("matched", Value::Int(matched)),
        ("changed", Value::Int(changed_count)),
        ("unchanged", Value::Int(unchanged_count)),
        ("skipped", Value::List(skipped)),
        ("blocked", Value::List(blocked)),
        ("changes", Value::List(changes)),
        ("quality_issues", Value::List(quality_issues)),
    ])
}

/// `remove_live2d_layer_metadata`: drop every layer field (and say which).
pub fn remove_live2d_layer_metadata(models: &mut [Dict], dry_run: bool) -> Dict {
    let mut changes = Vec::new();
    for model in models.iter_mut() {
        let present: Vec<&str> = LIVE2D_LAYER_METADATA_FIELDS
            .iter()
            .copied()
            .filter(|field| model.contains_key(field))
            .collect();
        if present.is_empty() {
            continue;
        }
        changes.push(Value::Dict(dict_of(vec![
            ("model", Value::Str(model_identity(model))),
            ("removed_fields", str_list(present.iter().map(|field| (*field).to_owned()))),
        ])));
        if !dry_run {
            for field in present {
                model.remove(field);
            }
        }
    }
    dict_of(vec![
        ("dry_run", Value::Bool(dry_run)),
        ("changed", Value::Int(changes.len() as i128)),
        ("changes", Value::List(changes)),
    ])
}

/// `_infer_live2d_model_kind`: `aim` or `cover` when the model's names say so, else `full`.
pub fn infer_live2d_model_kind(model: &Dict) -> String {
    let mut parts = vec![
        str_or_empty(model, "label"),
        str_or_empty(model, "live2d_key"),
        str_or_empty(model, "animation"),
    ];
    if let Some(Value::Dict(urls)) = model.get("urls") {
        for (_, value) in urls.iter() {
            match value {
                Value::Str(text) => parts.push(text.clone()),
                Value::List(items) => parts.extend(items.iter().map(str_value)),
                _ => {}
            }
        }
    }
    let normalized = parts.join(" ").to_lowercase();
    let has_aim = normalized.contains("aim");
    let has_cover = normalized.contains("cover");
    if has_cover && !has_aim {
        "cover".to_owned()
    } else if has_aim && !has_cover {
        "aim".to_owned()
    } else {
        "full".to_owned()
    }
}

/// `validate_live2d_layer_metadata`: the quality gates for each stacked full-body group:
/// every model has a distinct layer order, exactly one is primary, all matched with high
/// confidence.
pub fn validate_live2d_layer_metadata(models: &[Dict]) -> Vec<Value> {
    let refs = as_refs(models);
    let mut groups = group_indexes(&refs);
    // `sorted(..., key=lambda item: ((skin_index or -1), kind))`, stable.
    groups.sort_by(|(a, _), (b, _)| {
        let rank = |key: &GroupKey| key.0.filter(|index| *index != 0).unwrap_or(-1);
        (rank(a), &a.1).cmp(&(rank(b), &b.1))
    });
    let mut issues = Vec::new();
    for ((skin_index, kind), members) in groups {
        if kind != "full" || members.len() <= 1 {
            continue;
        }
        let group: Vec<&Dict> = members.iter().map(|index| refs[*index]).collect();
        let identities: Vec<String> = group.iter().map(|model| model_identity(model)).collect();
        let orders: Vec<Option<i128>> = group.iter().map(|model| to_int(model.get("layer_order"))).collect();
        let header = |code: &str| {
            vec![
                ("severity", Value::from("error")),
                ("code", Value::from(code)),
                ("skin_index", int_value(skin_index)),
                ("model_kind", Value::Str(kind.clone())),
            ]
        };
        let missing: Vec<String> = identities
            .iter()
            .zip(&orders)
            .filter(|(_, order)| order.is_none())
            .map(|(identity, _)| identity.clone())
            .collect();
        if missing.is_empty() {
            let compact: Vec<i128> = orders.iter().flatten().copied().collect();
            let distinct: BTreeSet<i128> = compact.iter().copied().collect();
            if distinct.len() != compact.len() {
                let mut entries = header("duplicate_layer_order");
                entries.push(("orders", Value::List(compact.iter().map(|order| Value::Int(*order)).collect())));
                entries.push(("models", str_list(identities.iter().cloned())));
                issues.push(Value::Dict(dict_of(entries)));
            }
        } else {
            let mut entries = header("missing_layer_order");
            entries.push(("models", str_list(missing)));
            issues.push(Value::Dict(dict_of(entries)));
        }

        let primary_count = group
            .iter()
            .filter(|model| matches!(model.get("is_primary"), Some(Value::Bool(true))))
            .count();
        if primary_count != 1 {
            let mut entries = header("invalid_primary_count");
            entries.push(("primary_count", Value::Int(primary_count as i128)));
            entries.push(("models", str_list(identities.iter().cloned())));
            issues.push(Value::Dict(dict_of(entries)));
        }

        let low: Vec<String> = group
            .iter()
            .filter(|model| !matches!(model.get("layer_match_confidence"), Some(Value::Str(text)) if text == "high"))
            .map(|model| model_identity(model))
            .collect();
        if !low.is_empty() {
            let mut entries = header("low_confidence_layer_match");
            entries.push(("models", str_list(low)));
            issues.push(Value::Dict(dict_of(entries)));
        }
    }
    issues
}

// ---------- files ----------

/// `OSError` raised for `path`, with Python's text (`[Errno 2] ...: 'path'`).
pub(crate) fn os_error(error: &std::io::Error, path: &Path) -> PyError {
    PyError::new(
        fav_net::download::io_class_name(error),
        format!(
            "{}: {}",
            fav_net::download::py_os_error_text(error),
            repr_str(&path.to_string_lossy())
        ),
    )
}

/// `_read_json_object`.
pub fn read_json_object(path: &Path) -> Result<Dict, PyError> {
    let text = std::fs::read(path).map_err(|error| os_error(&error, path))?;
    let text = crate::pydecode::decode_utf8(&text)?;
    match crate::pydecode::json_loads(text)? {
        Value::Dict(dict) => Ok(dict),
        _ => Err(PyError::new(
            "TypeError",
            format!("{} does not contain a JSON object", path.display()),
        )),
    }
}

/// A capture found next to a page: the raw capture, or the manifest's summary of it.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviousLayerCapture {
    pub payload: Dict,
    pub source: &'static str,
}

/// `read_previous_layer_capture_artifact`.
pub fn read_previous_layer_capture_artifact(root: &Path) -> Result<Option<PreviousLayerCapture>, PyError> {
    let raw = root.join(LAYER_CAPTURE_RAW_PATH);
    if raw.exists() {
        return Ok(Some(PreviousLayerCapture {
            payload: read_json_object(&raw)?,
            source: "raw",
        }));
    }
    read_previous_layer_capture_manifest_summary(root)
}

/// `read_previous_layer_capture_manifest_summary`.
pub fn read_previous_layer_capture_manifest_summary(root: &Path) -> Result<Option<PreviousLayerCapture>, PyError> {
    let manifest_path = root.join("manifest.json");
    if !manifest_path.exists() {
        return Ok(None);
    }
    let manifest = read_json_object(&manifest_path)?;
    Ok(match manifest.get(LIVE2D_LAYER_CAPTURE_FIELD) {
        Some(Value::Dict(summary)) => Some(PreviousLayerCapture {
            payload: summary.clone(),
            source: "manifest_summary",
        }),
        _ => None,
    })
}

/// `read_previous_layer_capture`.
pub fn read_previous_layer_capture(root: &Path) -> Result<Option<Dict>, PyError> {
    Ok(read_previous_layer_capture_artifact(root)?.map(|artifact| artifact.payload))
}

fn capture_payload_fingerprint(capture: &Dict) -> String {
    first_text(capture, &["fingerprint", "captureFingerprint"])
}

fn capture_payload_status(capture: &Dict) -> String {
    first_text(capture, &["status", "captureStatus"])
}

fn capture_payload_schema(capture: &Dict) -> i128 {
    to_int(capture.get("layer_capture_schema"))
        .filter(|schema| *schema != 0)
        .or_else(|| to_int(capture.get("schema")).filter(|schema| *schema != 0))
        .unwrap_or(1)
}

fn capture_payload_has_high_confidence(capture: &Dict) -> bool {
    let layers = normalized_layer_capture(capture);
    !layers.is_empty()
        && layers
            .iter()
            .all(|layer| matches!(layer.get("layer_match_confidence"), Some(Value::Str(text)) if text == "high"))
}

fn runtime_animation_state_is_complete(state: Option<&Value>) -> bool {
    let Some(normalized) = normalize_runtime_animation_state(state) else {
        return false;
    };
    if !matches!(normalized.get("match_confidence"), Some(Value::Str(text)) if text == "high") {
        return false;
    }
    let enabled = normalized.get("enabled").cloned().unwrap_or(Value::Bool(true));
    matches!(enabled, Value::Bool(false)) || !py_strip(&str_or_empty(&normalized, "animation")).is_empty()
}

fn capture_payload_has_complete_runtime_animations(capture: &Dict) -> bool {
    let layers = normalized_layer_capture(capture);
    if layers.is_empty() {
        return false;
    }
    layers.iter().all(|layer| match layer.get("runtime_animations") {
        Some(Value::Dict(animations)) => RUNTIME_ANIMATION_STATES
            .iter()
            .all(|state| runtime_animation_state_is_complete(animations.get(state))),
        _ => false,
    })
}

fn capture_reuse_contract_failure(previous: &Dict) -> (String, &'static str) {
    let status = capture_payload_status(previous);
    let reason = if status != "success" {
        "previous_status_not_success"
    } else if capture_payload_schema(previous) < RUNTIME_ANIMATION_CAPTURE_SCHEMA {
        "capture_schema_mismatch"
    } else if !capture_payload_has_high_confidence(previous) {
        "previous_confidence_not_high"
    } else if !capture_payload_has_complete_runtime_animations(previous) {
        "previous_runtime_animations_incomplete"
    } else {
        ""
    };
    (status, reason)
}

/// `evaluate_layer_capture_reuse`: whether a previous capture still fits the models
/// (same fingerprint, successful, high confidence, complete, passes the merge gates).
pub fn evaluate_layer_capture_reuse(content_id: i128, models: &[Dict], previous: Option<&Dict>, force_refresh: bool) -> Dict {
    let candidates = live2d_layer_fingerprint_candidates(content_id, &as_refs(models));
    let mut report = dict_of(vec![
        ("content_id", Value::Int(content_id)),
        ("current_fingerprint", Value::Str(candidates[0].clone())),
        ("current_fingerprints", str_list(candidates.iter().cloned())),
        ("previous_fingerprint", Value::from("")),
        ("reusable", Value::Bool(false)),
        ("action", Value::from("refresh")),
        ("reason", Value::from("")),
    ]);
    let reason: &str = if force_refresh {
        "force_refresh"
    } else if let Some(previous) = previous {
        let previous_fingerprint = capture_payload_fingerprint(previous);
        report.insert("previous_fingerprint", Value::Str(previous_fingerprint.clone()));
        if candidates.contains(&previous_fingerprint) {
            let (status, failure) = capture_reuse_contract_failure(previous);
            report.insert("previous_status", Value::Str(status));
            if failure.is_empty() {
                let coverage = layer_capture_group_coverage(content_id, models, previous);
                let complete = matches!(coverage.get("complete"), Some(Value::Bool(true)));
                let missing = coverage.get("missing_groups").cloned().unwrap_or(Value::Null);
                let unmatched = coverage.get("unmatched_layers").cloned().unwrap_or(Value::Null);
                report.insert("group_coverage", Value::Dict(coverage));
                if !complete {
                    report.insert("missing_groups", missing);
                    report.insert("unmatched_layers", unmatched);
                    report.insert("reason", Value::from("capture_missing_multi_full_groups"));
                    return report;
                }
                let mut scratch = models.to_vec();
                let merge_report = merge_live2d_layer_captures(&mut scratch, previous, Some(content_id), true);
                let failed = truthy(&merge_report, "skipped") || truthy(&merge_report, "quality_issues");
                report.insert("merge_report", Value::Dict(merge_report));
                if failed {
                    "quality_gates_failed"
                } else {
                    report.insert("reusable", Value::Bool(true));
                    report.insert("action", Value::from("reuse"));
                    "fingerprint_match"
                }
            } else {
                failure
            }
        } else if previous_fingerprint.is_empty() {
            "missing_fingerprint"
        } else {
            "fingerprint_changed"
        }
    } else {
        "no_previous_capture"
    };
    report.insert("reason", Value::from(reason));
    report
}

/// `_write_json`: pretty JSON through a temporary sibling and a rename.
pub fn write_json(path: &Path, data: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp-{}-{}", std::process::id(), random_hex(4)));
    let result = std::fs::write(&tmp, pretty_json(data)).and_then(|()| std::fs::rename(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    result
}

/// `secrets.token_hex(n)`.
pub(crate) fn random_hex(bytes: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(bytes * 2);
    while out.len() < bytes * 2 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_usize(out.len());
        let _ = write!(out, "{:016x}", hasher.finish());
    }
    out.truncate(bytes * 2);
    out
}

fn summary_layers(layers: Vec<Dict>) -> Value {
    Value::List(
        layers
            .into_iter()
            .map(|mut layer| {
                layer.remove("raw_container");
                Value::Dict(layer)
            })
            .collect(),
    )
}

fn safe_summary_text(value: &Value, max_length: usize) -> String {
    let text = py_split(&str_value(value)).collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return String::new();
    }
    let tokens: Vec<&str> = text
        .split(' ')
        .map(|token| {
            if token.contains('/') || token.contains('\\') {
                "<path>"
            } else {
                token
            }
        })
        .collect();
    tokens.join(" ").chars().take(max_length).collect()
}

fn is_str_or_int(value: &Value) -> bool {
    matches!(value, Value::Str(_) | Value::Int(_) | Value::Bool(_))
}

fn runtime_summary(runtime: Option<&Value>) -> Dict {
    let mut summary = Dict::new();
    let Some(Value::Dict(runtime)) = runtime else {
        return summary;
    };
    for key in ["requested_live2d_keys", "matched_live2d_keys"] {
        if let Some(Value::List(items)) = runtime.get(key) {
            let keys: Vec<String> = items
                .iter()
                .filter(|item| is_str_or_int(item))
                .map(|item| safe_summary_text(item, 120))
                .filter(|text| !text.is_empty())
                .collect();
            summary.insert(key, str_list(keys));
        }
    }
    if let Some(count) = to_int(runtime.get("container_count")) {
        summary.insert("container_count", Value::Int(count));
    }
    summary
}

fn warning_summary(warnings: Option<&Value>) -> Value {
    let Some(Value::List(warnings)) = warnings else {
        return Value::List(Vec::new());
    };
    let mut out = Vec::new();
    for warning in warnings {
        if !is_str_or_int(warning) {
            continue;
        }
        let text = safe_summary_text(warning, MAX_SUMMARY_TEXT_LENGTH);
        if !text.is_empty() {
            out.push(text);
        }
        if out.len() >= MAX_SUMMARY_WARNING_COUNT {
            break;
        }
    }
    str_list(out)
}

/// `layer_capture_manifest_summary`: what `manifest.json` keeps of a capture and its merge.
pub fn layer_capture_manifest_summary(capture: &Dict, merge_report: &Dict) -> Dict {
    let layers = normalized_layer_capture(capture);
    let mut status = first_text(capture, &["status", "captureStatus"]);
    if status.is_empty() {
        "success".clone_into(&mut status);
    }
    if truthy(merge_report, "skipped") || truthy(merge_report, "blocked") || truthy(merge_report, "quality_issues") {
        "incomplete".clone_into(&mut status);
    }
    let report_field = |key: &str, default: Value| merge_report.get(key).cloned().unwrap_or(default);
    dict_of(vec![
        (
            "schema",
            Value::Int(
                to_int(capture.get("layer_capture_schema"))
                    .filter(|schema| *schema != 0)
                    .unwrap_or(1),
            ),
        ),
        ("content_id", int_value(to_int(capture.get("content_id")))),
        ("source_url", Value::Str(first_text(capture, &["source_url", "sourceUrl"]))),
        ("captured_at", Value::Str(first_text(capture, &["captured_at", "capturedAt"]))),
        ("status", Value::Str(status)),
        ("fingerprint", Value::Str(capture_payload_fingerprint(capture))),
        (
            "capture_hash",
            Value::Str(sha256_hex(json_dumps(&Value::Dict(capture.clone())).as_bytes())),
        ),
        ("runtime", Value::Dict(runtime_summary(capture.get("runtime")))),
        ("warnings", warning_summary(capture.get("warnings"))),
        ("layer_count", Value::Int(layers.len() as i128)),
        ("matched", report_field("matched", Value::Int(0))),
        ("changed", report_field("changed", Value::Int(0))),
        ("skipped", report_field("skipped", Value::List(Vec::new()))),
        ("blocked", report_field("blocked", Value::List(Vec::new()))),
        ("quality_issues", report_field("quality_issues", Value::List(Vec::new()))),
        ("layers", summary_layers(layers)),
    ])
}

/// The `live2d_models` of a manifest or character file as dictionaries: Python calls
/// `.get` on each and fails on anything else.
pub(crate) fn take_models(document: &Dict) -> Result<Option<Vec<Dict>>, PyError> {
    let Some(Value::List(items)) = document.get("live2d_models") else {
        return Ok(None);
    };
    let mut models = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Value::Dict(model) => models.push(model.clone()),
            other => {
                return Err(PyError::new(
                    "AttributeError",
                    format!("'{}' object has no attribute 'get'", crate::pyerr::type_name(other)),
                ));
            }
        }
    }
    Ok(Some(models))
}

fn put_models(document: &mut Dict, models: Vec<Dict>) {
    document.insert("live2d_models", Value::List(models.into_iter().map(Value::Dict).collect()));
}

struct PageFiles {
    manifest_path: PathBuf,
    character_path: PathBuf,
    manifest: Dict,
    character: Dict,
    manifest_models: Vec<Dict>,
    character_models: Vec<Dict>,
}

fn read_page_files(root: &Path) -> Result<PageFiles, PyError> {
    let manifest_path = root.join("manifest.json");
    let character_path = root.join("character.json");
    let manifest = read_json_object(&manifest_path)?;
    let character = read_json_object(&character_path)?;
    let manifest_models = take_models(&manifest)?;
    let character_models = take_models(&character)?;
    let (Some(manifest_models), Some(character_models)) = (manifest_models, character_models) else {
        return Err(PyError::new(
            "TypeError",
            format!("{} is missing live2d_models in manifest.json or character.json", root.display()),
        ));
    };
    Ok(PageFiles {
        manifest_path,
        character_path,
        manifest,
        character,
        manifest_models,
        character_models,
    })
}

fn write_failure(error: &std::io::Error, path: &Path) -> PyError {
    os_error(error, path)
}

/// `merge_layer_capture_files`: merge a capture into a page's `manifest.json` and
/// `character.json`; unless `dry_run`, write both with the capture summary.
pub fn merge_layer_capture_files(root: &Path, capture: &Dict, dry_run: bool) -> Result<Dict, PyError> {
    let mut files = read_page_files(root)?;
    let content_id = to_int(files.manifest.get("content_id"));
    let manifest_report = merge_live2d_layer_captures(&mut files.manifest_models, capture, content_id, dry_run);
    let character_report = merge_live2d_layer_captures(&mut files.character_models, capture, content_id, dry_run);
    let summary = (!dry_run).then(|| layer_capture_manifest_summary(capture, &manifest_report));
    let report = dict_of(vec![
        ("root", Value::Str(root.to_string_lossy().into_owned())),
        ("dry_run", Value::Bool(dry_run)),
        ("manifest", Value::Dict(manifest_report)),
        ("character", Value::Dict(character_report)),
    ]);
    let Some(summary) = summary else {
        return Ok(report);
    };
    put_models(&mut files.manifest, files.manifest_models);
    put_models(&mut files.character, files.character_models);
    files.manifest.insert(LIVE2D_LAYER_CAPTURE_FIELD, Value::Dict(summary.clone()));
    files.character.insert(LIVE2D_LAYER_CAPTURE_FIELD, Value::Dict(summary));
    write_json(&files.manifest_path, &Value::Dict(files.manifest)).map_err(|error| write_failure(&error, &files.manifest_path))?;
    write_json(&files.character_path, &Value::Dict(files.character)).map_err(|error| write_failure(&error, &files.character_path))?;
    Ok(report)
}

/// `strip_layer_metadata_files`: remove all layer metadata and the capture summary.
pub fn strip_layer_metadata_files(root: &Path, dry_run: bool) -> Result<Dict, PyError> {
    let mut files = read_page_files(root)?;
    let manifest_report = remove_live2d_layer_metadata(&mut files.manifest_models, dry_run);
    let character_report = remove_live2d_layer_metadata(&mut files.character_models, dry_run);
    let manifest_has_capture = files.manifest.contains_key(LIVE2D_LAYER_CAPTURE_FIELD);
    let character_has_capture = files.character.contains_key(LIVE2D_LAYER_CAPTURE_FIELD);
    let report = dict_of(vec![
        ("root", Value::Str(root.to_string_lossy().into_owned())),
        ("dry_run", Value::Bool(dry_run)),
        ("manifest", Value::Dict(manifest_report)),
        ("character", Value::Dict(character_report)),
        (
            "capture_summary",
            Value::Dict(dict_of(vec![
                ("manifest_removed", Value::Bool(manifest_has_capture)),
                ("character_removed", Value::Bool(character_has_capture)),
            ])),
        ),
    ]);
    if dry_run {
        return Ok(report);
    }
    put_models(&mut files.manifest, files.manifest_models);
    put_models(&mut files.character, files.character_models);
    files.manifest.remove(LIVE2D_LAYER_CAPTURE_FIELD);
    files.character.remove(LIVE2D_LAYER_CAPTURE_FIELD);
    write_json(&files.manifest_path, &Value::Dict(files.manifest)).map_err(|error| write_failure(&error, &files.manifest_path))?;
    write_json(&files.character_path, &Value::Dict(files.character)).map_err(|error| write_failure(&error, &files.character_path))?;
    Ok(report)
}

#[cfg(test)]
mod tests;
