//! The Nikke routes: the character list, the sidebar list and the character detail,
//! read from the crawl's manifests on disk.
//!
//! Python origin: `src/api/nikke.py` (`NikkeLibrary` and its payload builders), the
//! `/nikke/*` routes of `src/api/routes.py` (`_matches_nikke_query`, the sidebar payload,
//! sort key and `ETag`) and the matching `FavApiService` methods. The library reads the
//! directory the Nikke source writes (`web.nikke.path`): one directory per character with
//! `manifest.json` and `character.json`, and keeps the list summaries in
//! `<root>/_api/summary-cache.json` ([`crate::summary_cache`]).
//!
//! Python builds the library once, with the path the settings held when the API started;
//! here the path is read from the settings on each request, and the library (with its
//! cache) is rebuilt when it changes (DIVERGENCES.md D86).
//!
//! Not ported: `NikkeLibrary.get_asset_file`, whose route was removed (assets are served
//! by the static sidecar under `/static/nikke`). The Live2D view overrides (`model_id`,
//! `view_overrides` and the `/view-overrides/{profile}` routes) come with WP10's
//! override store; until then `model_id` is `""` and `view_overrides` `{}` (PLAN.md).

pub(crate) mod schema;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use fav_core::logging::Logger;
use fav_core::pycasefold::casefold;
use fav_core::pyjson::{self, Dict, DumpOptions, Value};
use fav_core::pytext::py_strip;
use fav_core::pyurl::{quote, unquote};
use fav_core::validate::PyPath;
use fav_runtime::pystr::str_value;
use fav_sources::nikke_layer_metadata::{infer_live2d_model_kind, read_json_object, to_int};

use crate::error::ApiError;
use crate::handlers::{internal, unhandled};
use crate::params::{self, BigInt, Errors, IntBounds, Source, StrRules};
use crate::router::{Request, json_response};
use crate::summary_cache::{self, ManifestEntry, Signature, SummaryCacheData};
use crate::{ApiState, spine};

fn log() -> Logger {
    fav_core::logging::get("fav-api.nikke")
}

/// `_PROFILE_FIELDS`: key, the base-info label, the `tj_list` fallback key.
const PROFILE_FIELDS: [(&str, &str, &str); 9] = [
    ("rarity", "稀有度", "level"),
    ("company", "企业", "qy"),
    ("burst", "阶段", "jd"),
    ("attribute", "属性", "attr"),
    ("role", "职业", "zy"),
    ("weapon", "武器", "wq"),
    ("squad", "部队名称", "bd"),
    ("cv", "CV", ""),
    ("implemented_at", "实装日期", ""),
];
const SUMMARY_TAG_KEYS: [&str; 6] = ["rarity", "company", "burst", "attribute", "role", "weapon"];
const LAYER_MATCH_CONFIDENCE_VALUES: [&str; 3] = ["high", "medium", "low"];
const LAYER_CAPTURE_FIELD: &str = "live2d_layer_capture";
const RUNTIME_ANIMATION_CAPTURE_SCHEMA: i128 = 2;
const LAYER_METADATA_ISSUE_FIELDS: [&str; 19] = [
    "severity",
    "code",
    "reason",
    "skin_index",
    "model_kind",
    "models",
    "orders",
    "primary_count",
    "candidate_count",
    "stable_id",
    "live2d_key",
    "fields",
    "error_class",
    "state",
    "states",
    "runtime_animation_match_confidence",
    "capture_schema",
    "player_index",
    "source_layer_index",
];
const NIKKE_STATIC_PREFIX: &str = "/static/nikke";
/// `_NIKKE_SIDEBAR_CACHE_CONTROL`.
const SIDEBAR_CACHE_CONTROL: &str = "public, max-age=300";

// ---------- small Python helpers ----------

/// `_clean_text`: `str(value)` with Windows newlines folded, stripped; `None` is empty.
fn clean_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(value) => py_strip(&str_value(value).replace("\r\n", "\n")).to_owned(),
    }
}

/// `type(value).__name__` of a JSON value.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "str",
        Value::List(_) => "list",
        Value::Dict(_) => "dict",
    }
}

fn int_or_null(value: Option<i128>) -> Value {
    value.map_or(Value::Null, Value::Int)
}

fn str_or_null(text: String) -> Value {
    if text.is_empty() { Value::Null } else { Value::Str(text) }
}

fn as_dict(value: Option<&Value>) -> Option<&Dict> {
    match value {
        Some(Value::Dict(dict)) => Some(dict),
        _ => None,
    }
}

fn as_list(value: Option<&Value>) -> Option<&[Value]> {
    match value {
        Some(Value::List(items)) => Some(items),
        _ => None,
    }
}

fn dicts(items: &[Value]) -> impl Iterator<Item = &Dict> {
    items.iter().filter_map(|item| match item {
        Value::Dict(dict) => Some(dict),
        _ => None,
    })
}

fn dict<const N: usize>(pairs: [(&str, Value); N]) -> Dict {
    let mut out = Dict::new();
    for (key, value) in pairs {
        out.insert(key, value);
    }
    out
}

/// `d.get(key, default)`: the value when the key is present, even when it is `null`.
fn get_or<'a>(dict: &'a Dict, key: &str, default: &'a Value) -> &'a Value {
    dict.get(key).unwrap_or(default)
}

/// `a or b` on two values.
fn or<'a>(first: Option<&'a Value>, second: Option<&'a Value>) -> Option<&'a Value> {
    if first.is_some_and(Value::is_truthy) { first } else { second }
}

/// `_layer_match_confidence`.
fn layer_match_confidence(value: Option<&Value>) -> Option<String> {
    let text = clean_text(value);
    LAYER_MATCH_CONFIDENCE_VALUES.contains(&text.as_str()).then_some(text)
}

/// `_capture_schema`: `schema`, else `layer_capture_schema`, else 0 (`or` skips a 0).
fn capture_schema(capture_summary: &Dict) -> i128 {
    [capture_summary.get("schema"), capture_summary.get("layer_capture_schema")]
        .into_iter()
        .filter_map(to_int)
        .find(|schema| *schema != 0)
        .unwrap_or(0)
}

fn iter_assets(manifest: &Dict) -> Vec<&Dict> {
    as_list(manifest.get("assets"))
        .map(|items| dicts(items).collect())
        .unwrap_or_default()
}

fn iter_contexts(asset: &Dict) -> Vec<&Dict> {
    as_list(asset.get("contexts"))
        .map(|items| dicts(items).collect())
        .unwrap_or_default()
}

/// `_extract_text_value`: the first non-empty text cell under a base-info label.
fn extract_text_value(base_info: &Dict, label: &str) -> String {
    let Some(rows) = as_list(base_info.get(label)) else {
        return String::new();
    };
    for row in dicts(rows) {
        let Some(values) = as_list(row.get("values")) else {
            continue;
        };
        for cell in dicts(values) {
            if !matches!(cell.get("type"), Some(Value::Str(kind)) if kind == "text") {
                continue;
            }
            let text = clean_text(cell.get("value"));
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

/// `_live2d_model_identity`.
fn live2d_model_identity(model: &Dict) -> String {
    let stable_id = clean_text(model.get("stable_id"));
    let live2d_key = clean_text(model.get("live2d_key"));
    if !stable_id.is_empty() && !live2d_key.is_empty() {
        return format!("{stable_id}/{live2d_key}");
    }
    [stable_id, live2d_key]
        .into_iter()
        .find(|text| !text.is_empty())
        .unwrap_or_else(|| "<unknown>".to_owned())
}

/// `_normalize_asset_path`: a relative path under `assets/`, without `..`, as POSIX.
fn normalize_asset_path(asset_path: &str) -> Option<String> {
    let raw = unquote(asset_path).replace('\\', "/");
    let raw = py_strip(&raw);
    if raw.is_empty() || raw.starts_with('/') {
        return None;
    }
    let path = PyPath::new(raw);
    if path.as_str() == "." {
        return None;
    }
    let mut parts = path.as_str().split('/');
    if parts.next() != Some("assets") || parts.any(|part| part == "..") {
        return None;
    }
    Some(path.as_str().to_owned())
}

/// `_asset_static_url`: the static sidecar's URL of a file, with the hash as version.
fn asset_static_url(directory_name: &str, asset_path: &str, sha256: &str) -> String {
    let url = format!("{NIKKE_STATIC_PREFIX}/{}/{}", quote(directory_name, ""), quote(asset_path, "/"));
    if sha256.is_empty() {
        url
    } else {
        format!("{url}?v={}", quote(sha256, "/"))
    }
}

/// `_normalize_external_url`: protocol-relative URLs become `https:`.
fn normalize_external_url(url: &str) -> String {
    let value = py_strip(url);
    if value.starts_with("//") {
        format!("https:{value}")
    } else {
        value.to_owned()
    }
}

// ---------- CPython set order ----------

/// `hash(value)` of an `int` or `None` in `CPython` 3.12, as the unsigned `size_t` the set
/// probes with.
fn py_hash(value: Option<i128>) -> u64 {
    const MODULUS: i128 = (1 << 61) - 1;
    let Some(int) = value else {
        // `None` hashes to a constant since 3.12.
        return 0xFCA8_6420;
    };
    let reduced = int.abs() % MODULUS;
    let hash = if int < 0 { -reduced } else { reduced };
    let hash = if hash == -1 { -2 } else { hash };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // The hash fits 62 bits; `size_t` wraps a negative one.
    let unsigned = i64::try_from(hash).unwrap_or_default() as u64;
    unsigned
}

/// The iteration order of `{values}`: a `CPython` set's open-addressing table
/// (`Objects/setobject.c`: `set_add_entry`, `set_table_resize`, `set_insert_clean`; 9
/// linear probes, then the perturbed step). The skin indexes are sorted with a key that
/// ties `None`, `0` and `-1`, so their order in the set decides theirs.
fn py_set_order(values: impl IntoIterator<Item = Option<i128>>) -> Vec<Option<i128>> {
    const LINEAR_PROBES: usize = 9;
    const PERTURB_SHIFT: u32 = 5;
    fn insert(table: &mut [Option<(Option<i128>, u64)>], value: Option<i128>, hash: u64) {
        let mask = table.len() - 1;
        #[allow(clippy::cast_possible_truncation)] // `(size_t)hash & mask`.
        let mut index = (hash as usize) & mask;
        let mut perturb = hash;
        loop {
            let probes = if index + LINEAR_PROBES <= mask { LINEAR_PROBES } else { 0 };
            if let Some(slot) = table.iter_mut().skip(index).take(probes + 1).find(|slot| slot.is_none()) {
                *slot = Some((value, hash));
                return;
            }
            perturb >>= PERTURB_SHIFT;
            #[allow(clippy::cast_possible_truncation)]
            let next = (index.wrapping_mul(5).wrapping_add(1).wrapping_add(perturb as usize)) & mask;
            index = next;
        }
    }
    let mut table: Vec<Option<(Option<i128>, u64)>> = vec![None; 8];
    let mut used = 0_usize;
    for value in values {
        if table.iter().flatten().any(|(existing, _)| *existing == value) {
            continue;
        }
        insert(&mut table, value, py_hash(value));
        used += 1;
        let mask = table.len() - 1;
        if used * 5 >= mask * 3 {
            let minused = if used > 50_000 { used * 2 } else { used * 4 };
            let mut size = 8;
            while size <= minused {
                size <<= 1;
            }
            let old = std::mem::replace(&mut table, vec![None; size]);
            for (value, hash) in old.into_iter().flatten() {
                insert(&mut table, value, hash);
            }
        }
    }
    table.into_iter().flatten().map(|(value, _)| value).collect()
}

/// `sorted({_to_int(model.get('skin_index')) for model in live2d_models if ...},
/// key=lambda value: value or -1)`.
fn sorted_skin_indexes(live2d_models: &[Value]) -> Vec<Option<i128>> {
    let mut indexes = py_set_order(dicts(live2d_models).map(|model| to_int(model.get("skin_index"))));
    indexes.sort_by_key(|value| match value {
        Some(int) if *int != 0 => *int,
        _ => -1,
    });
    indexes
}

// ---------- layer and runtime-animation metadata ----------

/// `_runtime_animation_state_payload`.
fn runtime_animation_state_payload(value: Option<&Value>) -> Option<Dict> {
    let value = as_dict(value)?;
    let duration_ms = to_int(value.get("duration_ms").or_else(|| value.get("durationMs")));
    let flag = |key: &str| match value.get(key) {
        Some(Value::Bool(flag)) => *flag,
        _ => true,
    };
    Some(dict([
        ("animation", Value::Str(clean_text(value.get("animation")))),
        ("enabled", Value::Bool(flag("enabled"))),
        ("loop", Value::Bool(flag("loop"))),
        (
            "match_method",
            Value::Str(clean_text(or(value.get("match_method"), value.get("matchMethod")))),
        ),
        (
            "match_confidence",
            layer_match_confidence(or(value.get("match_confidence"), value.get("matchConfidence"))).map_or(Value::Null, Value::Str),
        ),
        ("duration_ms", int_or_null(duration_ms.filter(|duration| *duration >= 0))),
    ]))
}

/// `_runtime_animations_payload`: `None` unless one of the two states is there.
fn runtime_animations_payload(value: Option<&Value>) -> Option<Dict> {
    let value = as_dict(value)?;
    let idle = runtime_animation_state_payload(value.get("idle"));
    let click = runtime_animation_state_payload(value.get("click"));
    if idle.is_none() && click.is_none() {
        return None;
    }
    let state = |state: Option<Dict>| state.map_or(Value::Null, Value::Dict);
    Some(dict([("idle", state(idle)), ("click", state(click))]))
}

/// `_safe_layer_issue`: only the known fields, with only plain values.
fn safe_layer_issue(issue: &Dict) -> Dict {
    let mut out = Dict::new();
    for (key, value) in issue.iter() {
        let pyjson::Key::Str(name) = key else {
            continue;
        };
        if !LAYER_METADATA_ISSUE_FIELDS.contains(&name.as_str()) {
            continue;
        }
        match value {
            Value::Str(_) => out.insert(key.clone(), Value::Str(clean_text(Some(value)))),
            Value::Bool(_) | Value::Int(_) | Value::Null => out.insert(key.clone(), value.clone()),
            Value::List(items) => out.insert(
                key.clone(),
                Value::List(
                    items
                        .iter()
                        .filter_map(|item| match item {
                            Value::Str(_) => Some(Value::Str(clean_text(Some(item)))),
                            Value::Int(_) | Value::Bool(_) => Some(item.clone()),
                            _ => None,
                        })
                        .collect(),
                ),
            ),
            Value::Float(_) | Value::Dict(_) => {}
        }
    }
    out
}

/// `_capture_issues_for_skin`: the capture's own issues for one skin; issues without a
/// skin count for every skin when `include_global`.
fn capture_issues_for_skin(capture_summary: &Dict, skin_index: Option<i128>, include_global: bool) -> Vec<Value> {
    let mut issues = Vec::new();
    for field_name in ["quality_issues", "skipped", "blocked"] {
        let Some(items) = as_list(capture_summary.get(field_name)) else {
            continue;
        };
        for item in dicts(items) {
            let item_skin_index = to_int(item.get("skin_index"));
            let has_skin_index = item_skin_index.is_some();
            if item_skin_index != skin_index && (has_skin_index || !include_global) {
                continue;
            }
            let mut safe_issue = safe_layer_issue(item);
            if safe_issue.is_empty() {
                continue;
            }
            if !has_skin_index {
                safe_issue.insert("skin_index", int_or_null(skin_index));
                if !safe_issue.contains_key("model_kind") {
                    safe_issue.insert("model_kind", Value::from("full"));
                }
            }
            issues.push(Value::Dict(safe_issue));
        }
    }
    issues
}

/// `_layer_capture_summary`: `character.json`'s capture summary, else the manifest's.
fn layer_capture_summary<'a>(record: &'a Record, character: &'a Dict) -> &'a Dict {
    static EMPTY: std::sync::LazyLock<Dict> = std::sync::LazyLock::new(Dict::new);
    as_dict(character.get(LAYER_CAPTURE_FIELD))
        .or_else(|| as_dict(record.manifest.get(LAYER_CAPTURE_FIELD)))
        .unwrap_or(&EMPTY)
}

/// `_layer_capture_payload`.
fn layer_capture_payload(capture_summary: &Dict) -> Dict {
    let warnings = as_list(capture_summary.get("warnings")).map_or_else(Vec::new, |items| {
        items
            .iter()
            .filter(|item| matches!(item, Value::Str(_)))
            .map(|item| Value::Str(clean_text(Some(item))))
            .collect()
    });
    dict([
        ("status", Value::Str(clean_text(capture_summary.get("status")))),
        ("captured_at", str_or_null(clean_text(capture_summary.get("captured_at")))),
        ("attempted_at", str_or_null(clean_text(capture_summary.get("attempted_at")))),
        ("capture_hash", Value::Str(clean_text(capture_summary.get("capture_hash")))),
        ("fingerprint", Value::Str(clean_text(capture_summary.get("fingerprint")))),
        ("reason", Value::Str(clean_text(capture_summary.get("reason")))),
        ("error_class", Value::Str(clean_text(capture_summary.get("error_class")))),
        (
            "retryable",
            match capture_summary.get("retryable") {
                Some(Value::Bool(flag)) => Value::Bool(*flag),
                _ => Value::Null,
            },
        ),
        ("warnings", Value::List(warnings)),
    ])
}

/// `_multi_full_models_for_skin`: the skin's full-body models, when there are several.
fn multi_full_models_for_skin(live2d_models: &[Value], skin_index: Option<i128>) -> Vec<&Dict> {
    let models: Vec<&Dict> = dicts(live2d_models)
        .filter(|model| to_int(model.get("skin_index")) == skin_index && infer_live2d_model_kind(model) == "full")
        .collect();
    if models.len() > 1 { models } else { Vec::new() }
}

fn issue_base(code: &str, skin_index: Option<i128>) -> Dict {
    dict([
        ("severity", Value::from("error")),
        ("code", Value::from(code)),
        ("skin_index", int_or_null(skin_index)),
        ("model_kind", Value::from("full")),
    ])
}

fn str_values(items: &[String]) -> Value {
    Value::List(items.iter().cloned().map(Value::Str).collect())
}

/// `_computed_layer_metadata_issues`.
fn computed_layer_metadata_issues(models: &[&Dict], skin_index: Option<i128>) -> Vec<Value> {
    if models.is_empty() {
        return Vec::new();
    }
    let identities: Vec<String> = models.iter().map(|model| live2d_model_identity(model)).collect();
    let orders: Vec<Option<i128>> = models.iter().map(|model| to_int(model.get("layer_order"))).collect();
    let mut issues = Vec::new();
    let missing_order: Vec<String> = identities
        .iter()
        .zip(&orders)
        .filter(|(_, order)| order.is_none())
        .map(|(identity, _)| identity.clone())
        .collect();
    if missing_order.is_empty() {
        let compact_orders: Vec<i128> = orders.iter().flatten().copied().collect();
        let distinct: std::collections::HashSet<i128> = compact_orders.iter().copied().collect();
        if distinct.len() != compact_orders.len() {
            let mut issue = issue_base("duplicate_layer_order", skin_index);
            issue.insert("orders", Value::List(compact_orders.into_iter().map(Value::Int).collect()));
            issue.insert("models", str_values(&identities));
            issues.push(Value::Dict(issue));
        }
    } else {
        let mut issue = issue_base("missing_layer_order", skin_index);
        issue.insert("models", str_values(&missing_order));
        issues.push(Value::Dict(issue));
    }

    let primary_count = models
        .iter()
        .filter(|model| matches!(model.get("is_primary"), Some(Value::Bool(true))))
        .count();
    if primary_count != 1 {
        let mut issue = issue_base("invalid_primary_count", skin_index);
        issue.insert("primary_count", Value::Int(i128::try_from(primary_count).unwrap_or(i128::MAX)));
        issue.insert("models", str_values(&identities));
        issues.push(Value::Dict(issue));
    }

    let low_confidence: Vec<String> = identities
        .iter()
        .zip(models)
        .filter(|(_, model)| layer_match_confidence(model.get("layer_match_confidence")).as_deref() != Some("high"))
        .map(|(identity, _)| identity.clone())
        .collect();
    if !low_confidence.is_empty() {
        let mut issue = issue_base("low_confidence_layer_match", skin_index);
        issue.insert("models", str_values(&low_confidence));
        issues.push(Value::Dict(issue));
    }
    issues
}

/// `_runtime_animation_state_issue`.
fn runtime_animation_state_issue(model: &Dict, skin_index: Option<i128>, state_name: &str, state: Option<&Dict>) -> Option<Dict> {
    let identity = live2d_model_identity(model);
    let issue = |code: &str| {
        let mut issue = issue_base(code, skin_index);
        issue.insert("state", Value::from(state_name));
        issue.insert("models", str_values(std::slice::from_ref(&identity)));
        issue
    };
    let Some(state) = state else {
        return Some(issue("missing_runtime_animation_state"));
    };
    let confidence = state.get("match_confidence");
    if !matches!(confidence, Some(Value::Str(text)) if text == "high") {
        let mut issue = issue("low_runtime_animation_match");
        issue.insert("runtime_animation_match_confidence", confidence.cloned().unwrap_or(Value::Null));
        return Some(issue);
    }
    if matches!(state.get("enabled"), Some(Value::Bool(false))) {
        return None;
    }
    if clean_text(state.get("animation")).is_empty() {
        return Some(issue("missing_runtime_animation"));
    }
    None
}

/// `_computed_runtime_animation_metadata_issues`.
fn computed_runtime_animation_metadata_issues(models: &[&Dict], capture_summary: &Dict, skin_index: Option<i128>) -> Vec<Value> {
    if models.is_empty() {
        return Vec::new();
    }
    let schema = capture_schema(capture_summary);
    if schema < RUNTIME_ANIMATION_CAPTURE_SCHEMA {
        let mut issue = issue_base("capture_schema_mismatch", skin_index);
        issue.insert("capture_schema", Value::Int(schema));
        return vec![Value::Dict(issue)];
    }
    let mut issues = Vec::new();
    for model in models {
        let runtime_animations = runtime_animations_payload(model.get("runtime_animations"));
        for state_name in ["idle", "click"] {
            let state = runtime_animations
                .as_ref()
                .and_then(|animations| as_dict(animations.get(state_name)));
            if let Some(issue) = runtime_animation_state_issue(model, skin_index, state_name, state) {
                issues.push(Value::Dict(issue));
            }
        }
    }
    issues
}

/// `_capture_status_issue`.
fn capture_status_issue(capture_summary: &Dict, skin_index: Option<i128>) -> Option<Dict> {
    let issue = |severity: &str, code: &str| {
        let mut issue = issue_base(code, skin_index);
        issue.insert("severity", Value::from(severity));
        issue
    };
    let reason = || Value::Str(clean_text(capture_summary.get("reason")));
    match clean_text(capture_summary.get("status")).as_str() {
        "failed" => {
            let mut failed = issue("error", "capture_failed");
            failed.insert("reason", reason());
            failed.insert("error_class", Value::Str(clean_text(capture_summary.get("error_class"))));
            Some(failed)
        }
        "skipped" => {
            let mut skipped = issue("warning", "capture_skipped");
            skipped.insert("reason", reason());
            Some(skipped)
        }
        "incomplete" => Some(issue("warning", "capture_incomplete")),
        "" => Some(issue("warning", "capture_missing")),
        _ => None,
    }
}

/// `_layer_metadata_status`.
fn layer_metadata_status(required: bool, capture_summary: &Dict, issues: &[Value]) -> &'static str {
    if !required {
        return "complete";
    }
    match clean_text(capture_summary.get("status")).as_str() {
        "failed" => "error",
        "" | "skipped" => "missing",
        "incomplete" => "incomplete",
        _ if issues.is_empty() => "complete",
        _ => "incomplete",
    }
}

/// One skin's (or the whole character's) metadata status.
struct MetadataPayload {
    required: bool,
    status: &'static str,
    issues: Vec<Value>,
}

/// `_layer_metadata_payload_for_skin` (without the capture, which is the same for all).
fn layer_metadata_payload_for_skin(live2d_models: &[Value], capture_summary: &Dict, skin_index: Option<i128>) -> MetadataPayload {
    let models = multi_full_models_for_skin(live2d_models, skin_index);
    let required = !models.is_empty();
    let mut issues = if required {
        capture_issues_for_skin(capture_summary, skin_index, required)
    } else {
        Vec::new()
    };
    issues.extend(computed_layer_metadata_issues(&models, skin_index));
    if required
        && issues.is_empty()
        && let Some(issue) = capture_status_issue(capture_summary, skin_index)
    {
        issues.push(Value::Dict(issue));
    }
    MetadataPayload {
        required,
        status: layer_metadata_status(required, capture_summary, &issues),
        issues,
    }
}

/// The detail's status from its skins'; `precedence` lists the statuses that win, first
/// to last (layer metadata ranks `incomplete` above `missing`, runtime animations the
/// other way round).
fn combined_status(payloads: Vec<MetadataPayload>, precedence: [&'static str; 3]) -> MetadataPayload {
    let required = payloads.iter().any(|payload| payload.required);
    let statuses: Vec<&str> = payloads
        .iter()
        .filter(|payload| payload.required)
        .map(|payload| payload.status)
        .collect();
    let status = if required {
        precedence
            .into_iter()
            .find(|status| statuses.contains(status))
            .unwrap_or("complete")
    } else {
        "complete"
    };
    MetadataPayload {
        required,
        status,
        issues: payloads.into_iter().flat_map(|payload| payload.issues).collect(),
    }
}

/// `_layer_metadata_payload_for_detail`.
fn layer_metadata_payload_for_detail(live2d_models: &[Value], capture_summary: &Dict) -> MetadataPayload {
    let payloads = sorted_skin_indexes(live2d_models)
        .into_iter()
        .map(|skin_index| layer_metadata_payload_for_skin(live2d_models, capture_summary, skin_index))
        .collect();
    combined_status(payloads, ["error", "incomplete", "missing"])
}

/// `_runtime_animation_metadata_status`.
fn runtime_animation_metadata_status(required: bool, capture_summary: &Dict, issues: &[Value]) -> &'static str {
    if !required {
        return "complete";
    }
    if clean_text(capture_summary.get("status")) == "failed" {
        return "error";
    }
    if capture_schema(capture_summary) < RUNTIME_ANIMATION_CAPTURE_SCHEMA {
        return "missing";
    }
    if issues.is_empty() { "complete" } else { "incomplete" }
}

/// `_runtime_animation_metadata_payload_for_skin`.
fn runtime_animation_metadata_payload_for_skin(
    live2d_models: &[Value],
    capture_summary: &Dict,
    skin_index: Option<i128>,
) -> MetadataPayload {
    let models = multi_full_models_for_skin(live2d_models, skin_index);
    let required = !models.is_empty();
    let issues = computed_runtime_animation_metadata_issues(&models, capture_summary, skin_index);
    MetadataPayload {
        required,
        status: runtime_animation_metadata_status(required, capture_summary, &issues),
        issues,
    }
}

/// `_runtime_animation_metadata_payload_for_detail`.
fn runtime_animation_metadata_payload_for_detail(live2d_models: &[Value], capture_summary: &Dict) -> MetadataPayload {
    let payloads = sorted_skin_indexes(live2d_models)
        .into_iter()
        .map(|skin_index| runtime_animation_metadata_payload_for_skin(live2d_models, capture_summary, skin_index))
        .collect();
    combined_status(payloads, ["error", "missing", "incomplete"])
}

/// The metadata fields of a skin or of the detail, in the payload's order.
fn insert_metadata(payload: &mut Dict, layer: MetadataPayload, capture: Dict, runtime: MetadataPayload) {
    payload.insert("layer_metadata_required", Value::Bool(layer.required));
    payload.insert("layer_metadata_status", Value::from(layer.status));
    payload.insert("layer_metadata_issues", Value::List(layer.issues));
    payload.insert("layer_metadata_capture", Value::Dict(capture));
    payload.insert("runtime_animation_metadata_required", Value::Bool(runtime.required));
    payload.insert("runtime_animation_metadata_status", Value::from(runtime.status));
    payload.insert("runtime_animation_metadata_issues", Value::List(runtime.issues));
}

// ---------- the library ----------

/// `_NikkeRecordEntry`: where a character's files are.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordEntry {
    content_id: i128,
    root: PathBuf,
    manifest_path: PathBuf,
    directory_name: String,
}

impl RecordEntry {
    fn new(content_id: i128, manifest_path: PathBuf) -> Self {
        let root = manifest_path.parent().map(Path::to_path_buf).unwrap_or_default();
        let directory_name = root.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        Self {
            content_id,
            root,
            manifest_path,
            directory_name,
        }
    }
}

/// `_NikkeRecord`: an entry with its manifest.
struct Record {
    entry: RecordEntry,
    manifest: Dict,
}

/// Why a character cannot be shown.
#[derive(Debug)]
pub(crate) enum LibraryError {
    /// `NikkeCharacterNotFoundError`.
    NotFound,
    /// Anything else Python would raise (a 500).
    Other(String),
}

/// What the summary cache holds in memory.
#[derive(Debug, Default)]
struct CacheState {
    signature: Option<Signature>,
    record_entries: HashMap<i128, RecordEntry>,
    summaries: Vec<Dict>,
}

/// One asset filter of `_assets_for_context`; `None` matches anything.
#[derive(Debug, Clone, Copy, Default)]
struct Filter<'a> {
    kind: Option<&'a str>,
    label: Option<&'a str>,
    field: Option<&'a str>,
    section: Option<&'a str>,
    skin_index: Option<i128>,
    live2d_key: Option<&'a str>,
    live2d_field: Option<&'a str>,
}

impl<'a> Filter<'a> {
    /// `_pick_asset`'s default: images only.
    fn image() -> Self {
        Self {
            kind: Some("image"),
            ..Self::default()
        }
    }

    fn label(self, label: &'a str) -> Self {
        Self {
            label: Some(label),
            ..self
        }
    }

    fn skin(self, skin_index: Option<i128>) -> Self {
        Self { skin_index, ..self }
    }

    /// `_context_matches`.
    fn matches(&self, context: &Dict) -> bool {
        let text = |key: &str, expected: Option<&str>| expected.is_none_or(|expected| clean_text(context.get(key)) == expected);
        text("label", self.label)
            && text("field", self.field)
            && text("section", self.section)
            && self
                .skin_index
                .is_none_or(|skin_index| to_int(context.get("skin_index")) == Some(skin_index))
            && text("live2d_key", self.live2d_key)
            && text("live2d_field", self.live2d_field)
    }
}

/// `NikkeLibrary`: the characters under one collection root.
#[derive(Debug)]
pub struct NikkeLibrary {
    root: PyPath,
    cache: Mutex<CacheState>,
}

impl NikkeLibrary {
    pub fn new(root: PyPath) -> Self {
        Self {
            root,
            cache: Mutex::new(CacheState::default()),
        }
    }

    pub fn root(&self) -> &PyPath {
        &self.root
    }

    /// `list_characters`: every summary, by title.
    pub(crate) fn list_characters(&self) -> Vec<Dict> {
        let mut cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.ensure_summary_cache(&mut cache);
        cache.summaries.clone()
    }

    /// `get_character`: the detail payload, before the view overrides.
    pub(crate) fn get_character(&self, content_id: i128) -> Result<Dict, LibraryError> {
        let record = self.get_record(content_id)?;
        let character = Self::read_character_json(&record);
        let skins_value = match character.get("skins") {
            Some(skins @ Value::List(_)) => skins.clone(),
            _ => match as_dict(record.manifest.get("content_summary")) {
                Some(summary) => get_or(summary, "skins", &Value::List(Vec::new())).clone(),
                None => Value::List(Vec::new()),
            },
        };
        // `for skin in skins` over what the manifest holds: a string or an object yields
        // no skin, anything else cannot be iterated (`TypeError`).
        let skins: Vec<Value> = match skins_value {
            Value::List(items) => items,
            Value::Str(_) | Value::Dict(_) => Vec::new(),
            other => {
                return Err(LibraryError::Other(format!(
                    "TypeError: '{}' object is not iterable",
                    type_name(&other)
                )));
            }
        };
        let live2d_models: Vec<Value> = match character.get("live2d_models") {
            Some(Value::List(items)) => items.clone(),
            _ => as_list(record.manifest.get("live2d_models"))
                .map(<[Value]>::to_vec)
                .unwrap_or_default(),
        };
        let layer_capture = layer_capture_summary(&record, &character);
        let layer_metadata = layer_metadata_payload_for_detail(&live2d_models, layer_capture);
        let runtime_animation_metadata = runtime_animation_metadata_payload_for_detail(&live2d_models, layer_capture);

        let mut payload = Self::summary_payload(&record);
        let tj_list = match character.get("tj_list") {
            Some(tj_list @ Value::Dict(_)) => tj_list.clone(),
            _ => record.manifest.get("tj_list").cloned().unwrap_or(Value::Null),
        };
        payload.insert("tj_list", tj_list);
        payload.insert("base_info", Value::Dict(Self::base_info(&record, &character)));
        payload.insert(
            "skins",
            Value::List(
                dicts(&skins)
                    .map(|skin| Value::Dict(Self::skin_payload(&record, skin, &live2d_models, layer_capture)))
                    .collect(),
            ),
        );
        payload.insert(
            "live2d_models",
            Value::List(
                dicts(&live2d_models)
                    .map(|model| Value::Dict(Self::live2d_model_payload(&record, model)))
                    .collect(),
            ),
        );
        insert_metadata(
            &mut payload,
            layer_metadata,
            layer_capture_payload(layer_capture),
            runtime_animation_metadata,
        );
        payload.insert(
            "assets",
            Value::List(
                iter_assets(&record.manifest)
                    .into_iter()
                    .map(|asset| Value::Dict(Self::asset_ref(&record, asset, None)))
                    .collect(),
            ),
        );
        Ok(payload)
    }

    fn root_path(&self) -> PathBuf {
        PathBuf::from(self.root.as_str())
    }

    /// `self._root / name`, as pathlib joins (`Path('.') / 'a'` is `a`).
    fn under_root(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// `_manifest_entries`: the characters' manifests, by directory name (case-folded).
    fn manifest_entries(&self) -> Vec<ManifestEntry> {
        let root = self.root_path();
        if !root.is_dir() {
            return Vec::new();
        }
        let children = match std::fs::read_dir(&root) {
            Ok(children) => children,
            Err(error) => {
                log().error(format!("Failed to list Nikke collection root: {}: {error}", self.root.as_str()));
                return Vec::new();
            }
        };
        let mut entries = Vec::new();
        for child in children.flatten() {
            let name = child.file_name().to_string_lossy().into_owned();
            let child = self.under_root(&name);
            if name.starts_with('_') || !child.is_dir() {
                continue;
            }
            let manifest_path = child.join("manifest.json");
            if !manifest_path.is_file() {
                continue;
            }
            match std::fs::metadata(&manifest_path) {
                Ok(metadata) => entries.push(ManifestEntry::from_metadata(manifest_path, &metadata)),
                Err(_) => log().warning(format!("Failed to stat Nikke manifest: {}", manifest_path.display())),
            }
        }
        entries.sort_by_cached_key(|entry| {
            casefold(
                &entry
                    .path
                    .parent()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        });
        entries
    }

    /// `_ensure_summary_cache`: rebuild (or load from disk) when the manifests changed.
    fn ensure_summary_cache(&self, cache: &mut CacheState) {
        let entries = self.manifest_entries();
        let signature = summary_cache::manifest_signature(&entries);
        if cache.signature.as_ref() == Some(&signature) {
            return;
        }
        if self.load_summary_cache(cache, &signature) {
            return;
        }
        let (record_entries, summaries) = Self::build_summary_cache(&entries);
        *cache = CacheState {
            signature: Some(signature),
            record_entries,
            summaries,
        };
        self.write_summary_cache(cache);
    }

    /// `_build_summary_cache`.
    fn build_summary_cache(entries: &[ManifestEntry]) -> (HashMap<i128, RecordEntry>, Vec<Dict>) {
        let mut record_entries = HashMap::new();
        // Python's dict: a repeated content id keeps its first place and its last summary.
        let mut order: Vec<i128> = Vec::new();
        let mut summaries_by_content_id: HashMap<i128, Dict> = HashMap::new();
        for entry in entries {
            let manifest = match read_json_object(&entry.path) {
                Ok(manifest) => manifest,
                Err(error) => {
                    log().warning(format!(
                        "Skipping unreadable Nikke manifest {}: {}",
                        entry.path.display(),
                        error.message
                    ));
                    continue;
                }
            };
            let Some(content_id) = manifest_content_id(&entry.path, &manifest) else {
                log().warning(format!("Skipping Nikke manifest without content id: {}", entry.path.display()));
                continue;
            };
            let record = Record {
                entry: RecordEntry::new(content_id, entry.path.clone()),
                manifest,
            };
            if !summaries_by_content_id.contains_key(&content_id) {
                order.push(content_id);
            }
            summaries_by_content_id.insert(content_id, Self::summary_payload(&record));
            record_entries.insert(content_id, record.entry);
        }
        let mut summaries: Vec<Dict> = order
            .into_iter()
            .filter_map(|content_id| summaries_by_content_id.remove(&content_id))
            .collect();
        summaries.sort_by_cached_key(|summary| {
            let title = match summary.get("title") {
                Some(title) if title.is_truthy() => str_value(title),
                _ => String::new(),
            };
            let content_id = match summary.get("content_id") {
                Some(Value::Int(int)) => *int,
                _ => 0,
            };
            (casefold(&title), content_id)
        });
        (record_entries, summaries)
    }

    /// `_load_summary_cache`: take the disk cache when it matches the manifests.
    fn load_summary_cache(&self, cache: &mut CacheState, signature: &Signature) -> bool {
        let Some(cached) = summary_cache::read_summary_cache(&self.summary_cache_path(), signature, &log(), "Nikke") else {
            return false;
        };
        let Some(record_entries) = record_entries_from_cache(&cached.records, signature) else {
            return false;
        };
        *cache = CacheState {
            signature: Some(signature.clone()),
            record_entries,
            summaries: cached.summaries,
        };
        true
    }

    /// `_summary_cache_path`.
    fn summary_cache_path(&self) -> PathBuf {
        self.under_root("_api").join("summary-cache.json")
    }

    /// `_write_summary_cache`.
    fn write_summary_cache(&self, cache: &CacheState) {
        if !self.root_path().is_dir() {
            return;
        }
        let mut entries: Vec<&RecordEntry> = cache.record_entries.values().collect();
        entries.sort_by_key(|entry| entry.content_id);
        let records = entries
            .into_iter()
            .map(|entry| {
                dict([
                    ("content_id", Value::Int(entry.content_id)),
                    ("manifest_path", Value::Str(entry.manifest_path.to_string_lossy().into_owned())),
                ])
            })
            .collect();
        summary_cache::write_summary_cache(
            &self.summary_cache_path(),
            &SummaryCacheData {
                signature: cache.signature.clone().unwrap_or_default(),
                records,
                summaries: cache.summaries.clone(),
            },
            &log(),
            "Nikke",
        );
    }

    /// `_get_record`.
    fn get_record(&self, content_id: i128) -> Result<Record, LibraryError> {
        let entry = {
            let mut cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            self.ensure_summary_cache(&mut cache);
            cache.record_entries.get(&content_id).cloned()
        };
        let entry = entry.ok_or(LibraryError::NotFound)?;
        match read_json_object(&entry.manifest_path) {
            Ok(manifest) => Ok(Record { entry, manifest }),
            Err(error) => {
                log().warning(format!(
                    "Skipping unreadable Nikke manifest {}: {}",
                    entry.manifest_path.display(),
                    error.message
                ));
                Err(LibraryError::NotFound)
            }
        }
    }

    /// `_read_character_json`: `character.json`, or nothing (the manifest stands in).
    fn read_character_json(record: &Record) -> Dict {
        match read_json_object(&record.entry.root.join("character.json")) {
            Ok(character) => character,
            Err(error) => {
                log().warning(format!(
                    "Falling back to manifest data for {}: {}",
                    record.entry.directory_name, error.message
                ));
                Dict::new()
            }
        }
    }

    /// `_base_info`.
    fn base_info(record: &Record, character: &Dict) -> Dict {
        as_dict(character.get("base_info"))
            .or_else(|| as_dict(record.manifest.get("base_info")))
            .cloned()
            .unwrap_or_default()
    }

    /// `_summary_payload`.
    fn summary_payload(record: &Record) -> Dict {
        let manifest = &record.manifest;
        let profile = Self::profile_payload(record, &Dict::new());
        let mut tags = Dict::new();
        for item in &profile {
            if let (Some(Value::Str(key)), Some(value)) = (item.get("key"), item.get("value"))
                && SUMMARY_TAG_KEYS.contains(&key.as_str())
                && value.is_truthy()
            {
                tags.insert(key.as_str(), value.clone());
            }
        }
        let skin_count = as_dict(manifest.get("content_summary"))
            .and_then(|summary| match get_or(summary, "skins", &Value::Null) {
                Value::List(items) => Some(items.len()),
                _ => None,
            })
            .unwrap_or(0);
        let live2d_model_count = as_list(manifest.get("live2d_models")).map_or(0, <[Value]>::len);
        let count = |count: usize| Value::Int(i128::try_from(count).unwrap_or(i128::MAX));
        let asset = |found: Option<Dict>| found.map_or(Value::Null, Value::Dict);
        let icon = Self::pick_asset(
            record,
            Filter {
                field: Some("icon"),
                ..Filter::image()
            },
        )
        .or_else(|| Self::pick_asset(record, Filter::image().label("icon")))
        .or_else(|| Self::pick_asset(record, Filter::image().label("部队成员1")));
        let portrait = Self::pick_asset(record, Filter::image().label("时装立绘").skin(Some(0)))
            .or_else(|| Self::pick_asset(record, Filter::image().label("时装图（切换）").skin(Some(0))))
            .or_else(|| {
                Self::pick_asset(
                    record,
                    Filter {
                        field: Some("icon1"),
                        ..Filter::image()
                    },
                )
            });
        dict([
            ("content_id", Value::Int(record.entry.content_id)),
            ("title", Value::Str(safe_manifest_title(record))),
            ("directory_name", Value::Str(record.entry.directory_name.clone())),
            ("source_url", Value::Str(clean_text(manifest.get("source_url")))),
            ("updated_at", manifest.get("updated_at").cloned().unwrap_or(Value::Null)),
            ("fetched_at", str_or_null(clean_text(manifest.get("fetched_at")))),
            (
                "asset_counts",
                Value::Dict(as_dict(manifest.get("asset_counts")).cloned().unwrap_or_default()),
            ),
            ("tags", Value::Dict(tags)),
            ("profile", Value::List(profile.into_iter().map(Value::Dict).collect())),
            ("icon", asset(icon)),
            ("portrait", asset(portrait)),
            ("skin_count", count(skin_count)),
            ("live2d_model_count", count(live2d_model_count)),
        ])
    }

    /// `_profile_payload`.
    fn profile_payload(record: &Record, character: &Dict) -> Vec<Dict> {
        let base_info = Self::base_info(record, character);
        let tj_list = as_dict(character.get("tj_list"))
            .or_else(|| as_dict(record.manifest.get("tj_list")))
            .cloned()
            .unwrap_or_default();
        let mut profile = Vec::new();
        for (key, label, tj_key) in PROFILE_FIELDS {
            let mut value = extract_text_value(&base_info, label);
            if value.is_empty() && !tj_key.is_empty() {
                value = clean_text(tj_list.get(tj_key));
            }
            let asset = Self::pick_asset(record, Filter::image().label(label));
            if value.is_empty() && asset.is_none() {
                continue;
            }
            profile.push(dict([
                ("key", Value::from(key)),
                ("label", Value::from(label)),
                ("value", Value::Str(value)),
                ("asset", asset.map_or(Value::Null, Value::Dict)),
            ]));
        }
        profile
    }

    /// `_skin_payload`.
    fn skin_payload(record: &Record, skin: &Dict, live2d_models: &[Value], layer_capture: &Dict) -> Dict {
        let skin_index = to_int(skin.get("skin_index"));
        let models: Vec<Value> = dicts(live2d_models)
            .filter(|model| to_int(model.get("skin_index")) == skin_index)
            .map(|model| Value::Dict(Self::live2d_model_payload(record, model)))
            .collect();
        let layer_metadata = layer_metadata_payload_for_skin(live2d_models, layer_capture, skin_index);
        let runtime_animation_metadata = runtime_animation_metadata_payload_for_skin(live2d_models, layer_capture, skin_index);
        let rows: Vec<Value> = as_list(skin.get("rows")).map(<[Value]>::to_vec).unwrap_or_default();
        let pick = |label: &str| Self::pick_asset(record, Filter::image().label(label).skin(skin_index)).map_or(Value::Null, Value::Dict);
        let gallery = Self::assets_for_context(
            record,
            Filter {
                kind: Some("image"),
                section: Some("style"),
                skin_index,
                ..Filter::default()
            },
        );
        let mut payload = dict([
            ("skin_index", int_or_null(skin_index)),
            ("name", Value::Str(clean_text(skin.get("name")))),
            ("title", Value::Str(clean_text(skin.get("title")))),
            ("series", Value::Str(clean_text(skin.get("series")))),
            ("obtain", Value::Str(clean_text(skin.get("obtain")))),
            (
                "is_collection_skin",
                Value::Bool(skin.get("is_collection_skin").is_some_and(Value::is_truthy)),
            ),
            ("thumbnail", pick("时装图（切换）")),
            ("portrait", pick("时装立绘")),
            ("sd_model", pick("SD模型")),
            ("burst_animation", pick("爆裂动画")),
            ("gallery", Value::List(gallery.into_iter().map(Value::Dict).collect())),
            ("live2d_models", Value::List(models)),
        ]);
        insert_metadata(
            &mut payload,
            layer_metadata,
            layer_capture_payload(layer_capture),
            runtime_animation_metadata,
        );
        payload.insert("voice_lines", Value::List(voice_lines_from_rows(&rows)));
        payload.insert("rows", Value::List(rows));
        payload
    }

    /// `_live2d_model_payload`.
    fn live2d_model_payload(record: &Record, model: &Dict) -> Dict {
        let live2d_key = clean_text(model.get("live2d_key"));
        let part = |kind: &str, field: &str| {
            Self::pick_asset(
                record,
                Filter {
                    kind: Some(kind),
                    live2d_key: Some(&live2d_key),
                    live2d_field: Some(field),
                    ..Filter::default()
                },
            )
            .map_or(Value::Null, Value::Dict)
        };
        let textures = Self::assets_for_context(
            record,
            Filter {
                kind: Some("live2d_texture"),
                live2d_key: Some(&live2d_key),
                ..Filter::default()
            },
        );
        let assets = dict([
            ("atlas", part("live2d_atlas", "atlas")),
            ("skel", part("live2d_skel", "skel")),
            ("json", part("live2d_json", "json")),
            ("textures", Value::List(textures.into_iter().map(Value::Dict).collect())),
        ]);
        let text = |key: &str| Value::Str(clean_text(model.get(key)));
        let int = |key: &str| int_or_null(to_int(model.get(key)));
        let truthy = |key: &str| Value::Bool(model.get(key).is_some_and(Value::is_truthy));
        let object = |key: &str| Value::Dict(as_dict(model.get(key)).cloned().unwrap_or_default());
        let confidence = |key: &str| layer_match_confidence(model.get(key)).map_or(Value::Null, Value::Str);
        dict([
            ("label", text("label")),
            ("section", text("section")),
            ("row_index", int("row_index")),
            ("skin_index", int("skin_index")),
            ("skin_name", text("skin_name")),
            ("skin_title", text("skin_title")),
            ("skin_series", text("skin_series")),
            ("skin_obtain", text("skin_obtain")),
            ("is_collection_skin", truthy("is_collection_skin")),
            ("key", text("key")),
            ("stable_id", text("stable_id")),
            ("live2d_key", Value::Str(live2d_key.clone())),
            ("animation", text("animation")),
            ("skin", text("skin")),
            ("limit_age", truthy("limit_age")),
            ("layer_order", int("layer_order")),
            ("source_z_index", int("source_z_index")),
            ("source_layer_index", int("source_layer_index")),
            (
                "is_primary",
                match model.get("is_primary") {
                    Some(Value::Bool(flag)) => Value::Bool(*flag),
                    _ => Value::Null,
                },
            ),
            ("layer_match_method", text("layer_match_method")),
            ("layer_match_confidence", confidence("layer_match_confidence")),
            (
                "runtime_animations",
                runtime_animations_payload(model.get("runtime_animations")).map_or(Value::Null, Value::Dict),
            ),
            ("runtime_animation_match_method", text("runtime_animation_match_method")),
            (
                "runtime_animation_match_confidence",
                confidence("runtime_animation_match_confidence"),
            ),
            ("position", object("position")),
            ("bg_position", object("bg_position")),
            ("source_urls", object("urls")),
            (
                "spine_version",
                Self::spine_version(record, &assets).map_or(Value::Null, Value::Str),
            ),
            ("assets", Value::Dict(assets)),
        ])
    }

    /// `_spine_version`: the skel first, then the JSON export, as the viewer probes.
    fn spine_version(record: &Record, assets: &Dict) -> Option<String> {
        for key in ["skel", "json"] {
            let Some(reference) = as_dict(assets.get(key)) else {
                continue;
            };
            if !reference.get("available").is_some_and(Value::is_truthy) {
                continue;
            }
            let path = match reference.get("path") {
                Some(path) if path.is_truthy() => str_value(path),
                _ => String::new(),
            };
            let Some(normalized) = normalize_asset_path(&path) else {
                continue;
            };
            if let Some(version) = spine::spine_version_from_file(&record.entry.root.join(normalized)) {
                return Some(version);
            }
        }
        None
    }

    /// `_pick_asset`: the first match of `_assets_for_context`.
    fn pick_asset(record: &Record, filter: Filter<'_>) -> Option<Dict> {
        Self::assets_for_context(record, filter).into_iter().next()
    }

    /// `_assets_for_context`: the asset references whose contexts match, each path
    /// once, by row, label and path.
    fn assets_for_context(record: &Record, filter: Filter<'_>) -> Vec<Dict> {
        let mut found: Vec<((i128, String, String), Dict)> = Vec::new();
        let mut seen_paths = std::collections::HashSet::new();
        for asset in iter_assets(&record.manifest) {
            if let Some(kind) = filter.kind
                && !matches!(asset.get("kind"), Some(Value::Str(asset_kind)) if asset_kind == kind)
            {
                continue;
            }
            for context in iter_contexts(asset) {
                if !filter.matches(context) {
                    continue;
                }
                let local_path = clean_text(asset.get("local_path"));
                if !seen_paths.insert(local_path.clone()) {
                    continue;
                }
                let row_index = to_int(context.get("row_index")).unwrap_or(0);
                let sort_key = (row_index, clean_text(context.get("label")), local_path);
                found.push((sort_key, Self::asset_ref(record, asset, Some(context))));
            }
        }
        found.sort_by(|left, right| left.0.cmp(&right.0));
        found.into_iter().map(|(_, item)| item).collect()
    }

    /// `_asset_ref`: an asset as the API shows it, seen through one of its contexts.
    fn asset_ref(record: &Record, asset: &Dict, context: Option<&Dict>) -> Dict {
        let local_path = clean_text(asset.get("local_path"));
        let empty = Dict::new();
        let context = context.unwrap_or(&empty);
        let available = !local_path.is_empty()
            && normalize_asset_path(&local_path).is_some_and(|normalized| record.entry.root.join(normalized).is_file());
        let sha256 = clean_text(asset.get("sha256"));
        let url = if local_path.is_empty() {
            String::new()
        } else {
            asset_static_url(&record.entry.directory_name, &local_path, &sha256)
        };
        let field = Some(clean_text(context.get("field")))
            .filter(|field| !field.is_empty())
            .unwrap_or_else(|| clean_text(context.get("live2d_field")));
        dict([
            ("kind", Value::Str(clean_text(asset.get("kind")))),
            ("path", Value::Str(local_path)),
            ("url", Value::Str(url)),
            ("content_type", Value::Str(clean_text(asset.get("content_type")))),
            ("size", Value::Int(to_int(asset.get("size")).unwrap_or(0))),
            ("sha256", Value::Str(sha256)),
            ("status", Value::Str(clean_text(asset.get("status")))),
            ("label", Value::Str(clean_text(context.get("label")))),
            ("field", Value::Str(field)),
            ("skin_index", int_or_null(to_int(context.get("skin_index")))),
            ("live2d_key", Value::Str(clean_text(context.get("live2d_key")))),
            ("available", Value::Bool(available)),
            (
                "contexts",
                Value::List(iter_contexts(asset).into_iter().cloned().map(Value::Dict).collect()),
            ),
        ])
    }
}

/// `_manifest_content_id`: the manifest's `content_id`, else the directory name's prefix
/// (`"101 - Name"`).
fn manifest_content_id(manifest_path: &Path, manifest: &Dict) -> Option<i128> {
    if let Some(content_id) = to_int(manifest.get("content_id")) {
        return Some(content_id);
    }
    let directory = manifest_path
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = directory.split(" - ").next().unwrap_or_default();
    to_int(Some(&Value::Str(py_strip(prefix).to_owned())))
}

/// `_safe_manifest_title`: the manifest's title, else the directory name's suffix, else
/// the content id.
fn safe_manifest_title(record: &Record) -> String {
    let title = clean_text(record.manifest.get("title"));
    if !title.is_empty() {
        return title;
    }
    let directory = &record.entry.directory_name;
    // `directory_name.split(' - ', 1)[-1].strip() or str(content_id)`.
    let suffix = py_strip(directory.split_once(" - ").map_or(directory.as_str(), |(_, suffix)| suffix));
    if suffix.is_empty() {
        record.entry.content_id.to_string()
    } else {
        suffix.to_owned()
    }
}

/// `_record_entries_from_cache`: the cached records, when each still has its manifest.
fn record_entries_from_cache(records: &[Dict], signature: &Signature) -> Option<HashMap<i128, RecordEntry>> {
    let current_paths: std::collections::HashSet<&str> = signature.iter().map(|(path, _, _)| path.as_str()).collect();
    let mut entries = HashMap::new();
    for item in records {
        let content_id = to_int(item.get("content_id"))?;
        let Some(Value::Str(manifest_path)) = item.get("manifest_path") else {
            return None;
        };
        if !current_paths.contains(manifest_path.as_str()) {
            return None;
        }
        entries.insert(content_id, RecordEntry::new(content_id, PathBuf::from(manifest_path)));
    }
    Some(entries)
}

/// `_voice_lines_from_rows`: one line per audio cell; the row's label (or its first
/// text) and its second text.
fn voice_lines_from_rows(rows: &[Value]) -> Vec<Value> {
    let mut voice_lines = Vec::new();
    for row in dicts(rows) {
        let Some(cells) = as_list(row.get("cells")) else {
            continue;
        };
        let typed = |kind: &str| -> Vec<String> {
            dicts(cells)
                .filter(|cell| matches!(cell.get("type"), Some(Value::Str(cell_kind)) if cell_kind == kind))
                .map(|cell| clean_text(cell.get("value")))
                .filter(|text| !text.is_empty())
                .collect()
        };
        let audio_urls: Vec<String> = typed("audio").iter().map(|url| normalize_external_url(url)).collect();
        if audio_urls.is_empty() {
            continue;
        }
        let text_values = typed("text");
        let label = Some(clean_text(row.get("label")))
            .filter(|label| !label.is_empty())
            .or_else(|| text_values.first().cloned())
            .unwrap_or_default();
        let text = text_values.get(1).cloned().unwrap_or_default();
        for source_url in audio_urls {
            voice_lines.push(Value::Dict(dict([
                ("label", Value::Str(label.clone())),
                ("text", Value::Str(text.clone())),
                ("source_url", Value::Str(source_url)),
            ])));
        }
    }
    voice_lines
}

// ---------- the libraries the API keeps ----------

/// The library of the configured root, rebuilt when the setting changes.
#[derive(Debug, Default)]
pub struct NikkeLibraries {
    current: Mutex<Option<Arc<NikkeLibrary>>>,
}

impl NikkeLibraries {
    /// The library for `root`: the current one, or a new one when the root changed.
    pub fn for_root(&self, root: &PyPath) -> Arc<NikkeLibrary> {
        let mut current = self.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match current.as_ref() {
            Some(library) if library.root() == root => Arc::clone(library),
            _ => {
                let library = Arc::new(NikkeLibrary::new(root.clone()));
                *current = Some(Arc::clone(&library));
                library
            }
        }
    }
}

// ---------- routes ----------

/// The library for the configured `web.nikke.path`.
async fn library(state: &ApiState, failure: &str) -> Result<Arc<NikkeLibrary>, ApiError> {
    let settings = state.settings.load().await.map_err(|error| internal(failure, &error))?;
    Ok(state.nikke.for_root(&settings.web.nikke.path))
}

/// Run a library call off the async threads (FastAPI runs sync routes in a thread pool).
async fn blocking<T: Send + 'static>(failure: &str, work: impl FnOnce() -> T + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work).await.map_err(|error| internal(failure, &error))
}

/// `FavApiService.list_nikke_characters`.
async fn list_summaries(state: &ApiState) -> Result<Vec<Dict>, ApiError> {
    const FAILURE: &str = "Failed to list Nikke characters";
    let library = library(state, FAILURE).await?;
    blocking(FAILURE, move || library.list_characters()).await
}

/// `_matches_nikke_query`: the id, title, tags and profile values contain the query
/// (case-folded).
fn matches_query(character: &Dict, query: &str) -> bool {
    let text_or_empty = |value: Option<&Value>| match value {
        Some(value) if value.is_truthy() => str_value(value),
        _ => String::new(),
    };
    let mut haystack = vec![text_or_empty(character.get("content_id")), text_or_empty(character.get("title"))];
    if let Some(tags) = as_dict(character.get("tags")) {
        haystack.extend(tags.iter().map(|(_, value)| str_value(value)));
    }
    if let Some(profile) = as_list(character.get("profile")) {
        haystack.extend(dicts(profile).map(|item| text_or_empty(item.get("value"))));
    }
    casefold(&haystack.join(" ")).contains(&casefold(query))
}

/// The `q` parameter (`NikkeSearchQuery`: at least one character).
fn query_param(errors: &mut Errors, request: &Request) -> Option<String> {
    params::str_param(
        errors,
        Source::Query,
        "q",
        request.query_last("q"),
        false,
        StrRules {
            min_length: Some(1),
            ..StrRules::default()
        },
    )
}

fn validated(model: &schema::Model, payload: Dict) -> Result<Value, ApiError> {
    schema::validate(model, &Value::Dict(payload))
        .map_err(|error| unhandled(&format!("ValidationError: 1 validation error for {}: {error}", model.name)))
}

/// `GET /api/v2/nikke/characters`.
pub(crate) async fn list_characters(state: &ApiState, request: &Request) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let query = query_param(&mut errors, request);
    let limit = params::int_param(
        &mut errors,
        Source::Query,
        "limit",
        request.query_last("limit"),
        Some(200),
        IntBounds {
            ge: Some(1),
            le: Some(500),
            ..IntBounds::default()
        },
    );
    let offset = params::int_param(
        &mut errors,
        Source::Query,
        "offset",
        request.query_last("offset"),
        Some(0),
        IntBounds {
            ge: Some(0),
            ..IntBounds::default()
        },
    );
    errors.finish()?;
    let limit = match limit {
        Some(BigInt::Fits(limit)) => limit,
        _ => 200,
    };
    // The response echoes the offset as given, however large (past 128 bits it is
    // `i128::MAX`, D87); a page that far out is empty.
    let offset = match offset {
        Some(BigInt::Fits(offset)) => i128::from(offset),
        Some(BigInt::AbovePositive) => request
            .query_last("offset")
            .and_then(|raw| fav_core::validate::int_unbounded(&Value::Str(raw.to_owned()), &[]).ok())
            .unwrap_or(i128::MAX),
        _ => 0,
    };
    let mut characters = list_summaries(state).await?;
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        characters.retain(|character| matches_query(character, &query));
    }
    let total = characters.len();
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
    let end = start.saturating_add(usize::try_from(limit).unwrap_or(usize::MAX)).min(total);
    let items = characters
        .drain(start..end)
        .map(|character| validated(&schema::NIKKE_CHARACTER_SUMMARY, character))
        .collect::<Result<Vec<_>, _>>()?;
    let count = |count: usize| Value::Int(i128::try_from(count).unwrap_or(i128::MAX));
    let payload = dict([
        ("items", Value::List(items)),
        ("total", count(total)),
        ("limit", Value::Int(i128::from(limit))),
        ("offset", Value::Int(offset)),
    ]);
    Ok(json_response(200, &Value::Dict(payload)))
}

/// `_nikke_profile_value`: a profile entry's value, `None` when empty.
fn profile_value(character: &Dict, key: &str) -> Value {
    let Some(profile) = as_list(character.get("profile")) else {
        return Value::Null;
    };
    for item in dicts(profile) {
        if !matches!(item.get("key"), Some(Value::Str(item_key)) if item_key == key) {
            continue;
        }
        let value = match item.get("value") {
            Some(value) if value.is_truthy() => py_strip(&str_value(value)).to_owned(),
            _ => String::new(),
        };
        return str_or_null(value);
    }
    Value::Null
}

/// `_nikke_sidebar_character_payload`.
fn sidebar_character_payload(character: &Dict) -> Dict {
    let empty = Dict::new();
    let icon = as_dict(character.get("icon")).unwrap_or(&empty);
    let text = |key: &str| {
        Value::Str(match icon.get(key) {
            Some(value) if value.is_truthy() => str_value(value),
            _ => String::new(),
        })
    };
    let raw = |key: &str| character.get(key).cloned().unwrap_or(Value::Null);
    dict([
        ("content_id", raw("content_id")),
        (
            "title",
            match character.get("title") {
                Some(title) if title.is_truthy() => title.clone(),
                _ => Value::from(""),
            },
        ),
        (
            "icon",
            Value::Dict(dict([
                ("url", text("url")),
                ("available", Value::Bool(icon.get("available").is_some_and(Value::is_truthy))),
                ("sha256", text("sha256")),
                ("content_type", text("content_type")),
            ])),
        ),
        ("implemented_at", profile_value(character, "implemented_at")),
        ("updated_at", raw("updated_at")),
        ("fetched_at", raw("fetched_at")),
    ])
}

/// `_sort_timestamp`: seconds since the epoch of a number, digit string or ISO time;
/// 0 for anything else.
fn sort_timestamp(value: &Value) -> f64 {
    #[allow(clippy::cast_precision_loss)] // `float(int)`.
    match value {
        Value::Int(int) => *int as f64,
        Value::Float(float) => *float,
        Value::Str(text) => {
            let text = py_strip(text);
            if text.is_empty() {
                return 0.0;
            }
            if fav_core::pytext::py_isdigit(text) {
                // `float(text)`; a digit `float()` refuses (a superscript) is 0 (D87).
                return fav_core::pytext::py_int_decimal(text).map_or(0.0, |int| int as f64);
            }
            match crate::mcp::fromisoformat(&text.replace('Z', "+00:00")) {
                Some(crate::mcp::Parsed::Aware(parsed)) => timestamp_seconds(parsed.timestamp(), parsed.timestamp_subsec_micros()),
                Some(crate::mcp::Parsed::Naive(parsed)) => {
                    let parsed = parsed.and_utc();
                    timestamp_seconds(parsed.timestamp(), parsed.timestamp_subsec_micros())
                }
                None => 0.0,
            }
        }
        _ => 0.0,
    }
}

/// `datetime.timestamp()`: whole microseconds over a million.
fn timestamp_seconds(seconds: i64, micros: u32) -> f64 {
    #[allow(clippy::cast_precision_loss)] // Python divides the same integer.
    let total = (i128::from(seconds) * 1_000_000 + i128::from(micros)) as f64;
    total / 1_000_000.0
}

/// `_sort_date`: the ordinal of the first three numbers as a date; 0 when they are not one.
fn sort_date(value: &Value) -> i64 {
    use chrono::Datelike as _;
    static NUMBERS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"\d+").expect("valid pattern"));
    let Value::Str(text) = value else {
        return 0;
    };
    let parts: Vec<Option<i128>> = NUMBERS
        .find_iter(text)
        .take(3)
        .map(|found| fav_core::pytext::py_int_decimal(found.as_str()))
        .collect();
    let [Some(year), Some(month), Some(day)] = parts[..] else {
        return 0;
    };
    let (Ok(year), Ok(month), Ok(day)) = (i32::try_from(year), u32::try_from(month), u32::try_from(day)) else {
        return 0;
    };
    if !(1..=9999).contains(&year) {
        return 0;
    }
    chrono::NaiveDate::from_ymd_opt(year, month, day).map_or(0, |date| i64::from(date.num_days_from_ce()))
}

/// `_nikke_sidebar_sort_key`.
fn sidebar_sort_key(item: &Dict) -> (i64, f64, f64, i128) {
    let field = |key: &str| item.get(key).unwrap_or(&Value::Null);
    (
        sort_date(field("implemented_at")),
        sort_timestamp(field("updated_at")),
        sort_timestamp(field("fetched_at")),
        match field("content_id") {
            Value::Int(int) => *int,
            _ => 0,
        },
    )
}

/// `_etag_matches`: `*` or one of the comma-separated tags.
fn etag_matches(header_value: Option<&str>, etag: &str) -> bool {
    let Some(header_value) = header_value.filter(|value| !value.is_empty()) else {
        return false;
    };
    header_value
        .split(',')
        .map(py_strip)
        .any(|candidate| candidate == "*" || candidate == etag)
}

/// `_json_etag`: the hash of the payload's sorted compact JSON.
fn json_etag(payload: &Value) -> Result<String, ApiError> {
    let body = pyjson::dumps(payload, &DumpOptions::hash()).map_err(|error| unhandled(&format!("TypeError: {error}")))?;
    Ok(format!("\"{}\"", fav_sources::nikke_layer_metadata::sha256_hex(body.as_bytes())))
}

/// `GET /api/v2/nikke/sidebar/characters`: every character, newest first, with an
/// `ETag` so an unchanged list is a 304.
pub(crate) async fn list_sidebar_characters(state: &ApiState, request: &Request) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let query = query_param(&mut errors, request);
    errors.finish()?;
    let mut characters = list_summaries(state).await?;
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        characters.retain(|character| matches_query(character, &query));
    }
    let mut items = Vec::with_capacity(characters.len());
    for character in &characters {
        match validated(&schema::NIKKE_SIDEBAR_CHARACTER, sidebar_character_payload(character))? {
            Value::Dict(item) => items.push(item),
            _ => return Err(ApiError::internal()),
        }
    }
    let keys: Vec<(i64, f64, f64, i128)> = items.iter().map(sidebar_sort_key).collect();
    let mut order: Vec<usize> = (0..items.len()).collect();
    // `sorted(..., reverse=True)` keeps equal items in their order.
    order.sort_by(|&left, &right| keys[right].partial_cmp(&keys[left]).unwrap_or(std::cmp::Ordering::Equal));
    let total = Value::Int(i128::try_from(items.len()).unwrap_or(i128::MAX));
    let sorted: Vec<Value> = order.into_iter().map(|index| Value::Dict(items[index].clone())).collect();
    let payload = Value::Dict(dict([("items", Value::List(sorted)), ("total", total)]));
    let etag = json_etag(&payload)?;
    let headers = |response: &mut Response| {
        response
            .headers_mut()
            .insert("cache-control", HeaderValue::from_static(SIDEBAR_CACHE_CONTROL));
        if let Ok(value) = HeaderValue::from_str(&etag) {
            response.headers_mut().insert("etag", value);
        }
    };
    if etag_matches(request.header("if-none-match").as_deref(), &etag) {
        let mut response = Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        headers(&mut response);
        return Ok(response);
    }
    let mut response = json_response(200, &payload);
    headers(&mut response);
    Ok(response)
}

/// `GET /api/v2/nikke/characters/{content_id}`.
pub(crate) async fn get_character(state: &ApiState, raw_content_id: Option<&str>) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let content_id = params::int_param(
        &mut errors,
        Source::Path,
        "content_id",
        raw_content_id,
        None,
        IntBounds {
            gt: Some(0),
            ..IntBounds::default()
        },
    );
    errors.finish()?;
    let not_found = || ApiError::new(404, "nikke_character_not_found", "Nikke character not found.");
    // An id past `i64::MAX` is answered 404 without reading the collection (D87).
    let Some(BigInt::Fits(content_id)) = content_id else {
        return Err(not_found());
    };
    let failure = format!("Failed to get Nikke character content_id={content_id}");
    let library = library(state, &failure).await?;
    let character = blocking(&failure, move || library.get_character(i128::from(content_id))).await?;
    match character {
        Ok(character) => Ok(json_response(200, &validated(&schema::NIKKE_CHARACTER_DETAIL, character)?)),
        Err(LibraryError::NotFound) => Err(not_found()),
        Err(LibraryError::Other(error)) => Err(internal(&failure, &error)),
    }
}

#[cfg(test)]
mod tests;
