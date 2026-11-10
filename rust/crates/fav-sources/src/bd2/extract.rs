//! BD2 page parsing: GameKee's tree and content JSON, media assets and Live2D models, the
//! BD2-L2D-Viewer preference and supplements, archive paths and hashes, retry rules.
//!
//! Python origin: the module-level functions of `src/web/bd2.py`. Page data stays in
//! `pyjson` values so dictionary order, `x or default` and `==` behave as in Python.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use fav_core::filename::sanitize;
use fav_core::pycasefold::casefold;
use fav_core::pyjson::{Dict, Key, Value};
use fav_core::pypath;
use fav_core::pytext::{py_int_decimal, py_isdecimal, py_isdigit, py_strip};
use fav_runtime::pystr::{repr_str, str_key, str_value};
use fav_runtime::source::JobError;

use super::gamekee::{self, Asset, Assets, add_asset, add_url_list, normalize_url, sha256_hex};
use crate::bd2_l2d_viewer::{
    self, Category, VIEWER_PAGE_URL, ViewerResource, category_from_stem, resource_stem_from_url, viewer_asset_url,
};
use crate::pydecode::{PyError, json_loads};
use crate::pyre;

pub(crate) const GAMEKEE_BASE_URL: &str = gamekee::GAMEKEE_BASE_URL;
pub(crate) const GAME_ALIAS: &str = "zsca2";
pub(crate) const TREE_ROOT_PID: i64 = 194_491;
/// `CHARACTER_GROUPS`: GameKee tree nodes holding the 5-, 4- and 3-star characters.
pub(crate) const CHARACTER_GROUPS: [(i128, &str); 3] = [(122_323, "5-star"), (122_322, "4-star"), (122_318, "3-star")];
pub(crate) const DEFAULT_USER_AGENT: &str = gamekee::DEFAULT_USER_AGENT;
pub(crate) const CDN_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36";
const LIVE2D_URL_FIELDS: [&str; 3] = ["atlas", "skel", "json"];
const MEDIA_CELL_TYPES: [&str; 4] = ["audio", "image", "video", "live2d"];

const STYLE_COSTUME_NAME_ROW: usize = 0;
const STYLE_COSTUME_CATEGORY_ROW: usize = 1;
const STYLE_LIVE2D_HEADER_ROW: usize = 5;
const STANDING_LIVE2D_ROW: i64 = 6;
const INTERACTION_LIVE2D_ROW: i64 = 7;
const SKILL_LIVE2D_1_ROW: i64 = 8;

const RETRY_BASE_DELAY_SECONDS: f64 = 0.75;
const RETRY_AFTER_MAX_DELAY_SECONDS: f64 = 300.0;
const MAX_RETRIES: u32 = 3;
const LIMITED_RETRY_ATTEMPTS: u32 = 2;
const VIDEO_RETRY_COOLDOWN_DAYS: [i64; 3] = [1, 3, 7];

pub(crate) const VIEWER_SUPPLEMENT_SOURCE: &str = "bd2_l2d_viewer";
const VIEWER_PREFERRED_SOURCE_VARIANT: &str = "preferred_source";
const REASON_MATCHED_STEM_PREFERRED_SOURCE: &str = "matched_stem_preferred_source";
const REASON_EMPTY_SLOT: &str = "empty_gamekee_slot";
const REASON_CENSORED_VARIANT: &str = "censored_variant";
const REASON_MISMATCHED_SLOT: &str = "mismatched_gamekee_slot";
const VIEWER_EXTRA_REASON: &str = "viewer_extra";
const VIEWER_PREFERRED_REPLACED_LIVE2D_FIELDS: [&str; 5] = ["atlas", "skel", "json", "image", "atlas_texture"];
const ANCHOR_CATEGORY_ORDER: [Category; 4] = [Category::Character, Category::Ultimate, Category::Dating, Category::Unknown];

static CONTENT_ID_PATTERNS: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
    [
        r"/zsca2/tj/(\d+)\.html(?:[?#].*)?$",
        r"/tj/(\d+)\.html(?:[?#].*)?$",
        r"/zsca2/(\d+)\.html(?:[?#].*)?$",
        r"content/detail/(\d+)(?:[?#].*)?$",
    ]
    .iter()
    .map(|pattern| pyre::compile(pattern, false, false))
    .collect()
});

/// `ART_ROWS`: style rows that hold a costume's art, by row index.
fn art_row_field(row_index: usize) -> Option<&'static str> {
    Some(match row_index {
        2 => "costume_sprite",
        3 => "costume_portrait",
        4 => "costume_full_portrait",
        6 => "standing_live2d",
        7 => "interaction_live2d",
        8 => "skill_live2d_1",
        9 => "skill_live2d_2",
        10 => "story_live2d_1",
        11 => "story_live2d_2",
        12 => "story_live2d_3",
        13 => "story_live2d_4",
        14 => "story_live2d_5",
        _ => return None,
    })
}

// ---------- Python value semantics ----------

static NONE: Value = Value::Null;

/// `d.get(key)`: the value, or `None`.
pub(crate) fn get<'a>(dict: &'a Dict, key: &str) -> &'a Value {
    dict.get(key).unwrap_or(&NONE)
}

/// `value or default`.
fn or_value(value: &Value, default: Value) -> Value {
    if value.is_truthy() { value.clone() } else { default }
}

fn or_empty(value: &Value) -> Value {
    or_value(value, Value::Str(String::new()))
}

fn text(value: &str) -> Value {
    Value::Str(value.to_owned())
}

fn int(value: usize) -> Value {
    Value::Int(i128::try_from(value).unwrap_or(i128::MAX))
}

/// Python `a == b` for values `json.loads` produced: numbers compare by value across
/// `bool`, `int` and `float`, containers by content.
pub(crate) fn py_eq(left: &Value, right: &Value) -> bool {
    /// `int == float`: equal only when the float is exactly that integer.
    fn int_float(int: i128, float: f64) -> bool {
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)] // reason: checked integral and in range first
        let exact = float.is_finite() && float.fract() == 0.0 && float.abs() < 1.7e38 && float as i128 == int;
        exact
    }
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        #[allow(clippy::float_cmp)] // reason: Python's `==` is exact
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Int(int), Value::Bool(flag)) | (Value::Bool(flag), Value::Int(int)) => *int == i128::from(*flag),
        (Value::Int(int), Value::Float(float)) | (Value::Float(float), Value::Int(int)) => int_float(*int, *float),
        (Value::Bool(flag), Value::Float(float)) | (Value::Float(float), Value::Bool(flag)) => int_float(i128::from(*flag), *float),
        (Value::List(a), Value::List(b)) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| py_eq(a, b)),
        (Value::Dict(a), Value::Dict(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.iter().any(|(other_key, other)| key_eq(key, other_key) && py_eq(value, other)))
        }
        _ => false,
    }
}

fn key_eq(a: &Key, b: &Key) -> bool {
    match (a, b) {
        (Key::Str(a), Key::Str(b)) => a == b,
        _ => py_eq(&key_value(a), &key_value(b)),
    }
}

fn key_value(key: &Key) -> Value {
    match key {
        Key::Str(text) => Value::Str(text.clone()),
        Key::Int(int) => Value::Int(*int),
        Key::Float(float) => Value::Float(*float),
        Key::Bool(flag) => Value::Bool(*flag),
        Key::Null => Value::Null,
    }
}

/// `{**base, key: value, ...}`.
fn with(base: &Dict, entries: &[(&str, Value)]) -> Dict {
    let mut out = base.clone();
    for (key, value) in entries {
        out.insert(*key, value.clone());
    }
    out
}

fn py_error(error: PyError) -> JobError {
    error.into_job_error()
}

// ---------- ids and headers ----------

/// `_to_int(value)`.
pub(crate) fn to_int(value: &Value) -> Result<Option<i128>, JobError> {
    bd2_l2d_viewer::to_int(Some(value)).map_err(py_error)
}

/// `parse_content_id(target)`: a bare id or a GameKee page or detail URL.
pub(crate) fn parse_content_id(target: &str) -> Result<i128, JobError> {
    let raw = py_strip(target);
    let parse = |digits: &str| py_int_decimal(digits).ok_or_else(|| py_error(bd2_l2d_viewer::invalid_int_literal(digits)));
    if py_isdigit(raw) {
        return parse(raw);
    }
    for pattern in CONTENT_ID_PATTERNS.iter() {
        if let Some(captures) = pattern.captures(raw) {
            return parse(&captures[1]);
        }
    }
    Err(JobError::new(
        "ValueError",
        format!("Cannot parse a BD2 GameKee content id from {}", repr_str(target)),
    ))
}

/// `request_headers(content_id)`.
pub(crate) fn request_headers(content_id: Option<i128>) -> Vec<(String, String)> {
    let referer = match content_id {
        Some(content_id) if content_id != 0 => format!("{GAMEKEE_BASE_URL}/{GAME_ALIAS}/tj/{content_id}.html"),
        _ => format!("{GAMEKEE_BASE_URL}/{GAME_ALIAS}/"),
    };
    [
        ("Accept", "application/json, text/plain, */*".to_owned()),
        ("Game-Alias", GAME_ALIAS.to_owned()),
        ("Lang", "zh-cn".to_owned()),
        ("Referer", referer),
        ("User-Agent", DEFAULT_USER_AGENT.to_owned()),
        ("X-Requested-With", "XMLHttpRequest".to_owned()),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect()
}

/// `cdn_request_headers()`.
pub(crate) fn cdn_request_headers() -> Vec<(String, String)> {
    [
        ("Accept", "application/json, text/plain, */*".to_owned()),
        ("Accept-Language", "en,zh;q=0.9,zh-CN;q=0.8".to_owned()),
        ("Cache-Control", "no-cache".to_owned()),
        ("Origin", GAMEKEE_BASE_URL.to_owned()),
        ("Pragma", "no-cache".to_owned()),
        ("Priority", "u=1, i".to_owned()),
        ("Referer", format!("{GAMEKEE_BASE_URL}/")),
        (
            "Sec-CH-UA",
            r#""Chromium";v="148", "Google Chrome";v="148", "Not/A)Brand";v="99""#.to_owned(),
        ),
        ("Sec-CH-UA-Mobile", "?0".to_owned()),
        ("Sec-CH-UA-Platform", r#""macOS""#.to_owned()),
        ("Sec-Fetch-Dest", "empty".to_owned()),
        ("Sec-Fetch-Mode", "cors".to_owned()),
        ("Sec-Fetch-Site", "same-site".to_owned()),
        ("User-Agent", CDN_USER_AGENT.to_owned()),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect()
}

// ---------- content JSON ----------

/// `_coerce_json_object(value)`: a dict, a JSON object in a non-blank string, else `{}`.
fn coerce_json_object(value: &Value) -> Dict {
    match value {
        Value::Dict(dict) => dict.clone(),
        Value::Str(text) if !py_strip(text).is_empty() => match json_loads(text) {
            Ok(Value::Dict(dict)) => dict,
            _ => Dict::new(),
        },
        _ => Dict::new(),
    }
}

/// `parse_content_json(detail)`.
pub(crate) fn parse_content_json(detail: &Dict) -> Dict {
    coerce_json_object(get(detail, "content_json"))
}

/// `content_json_from_cdn_payload(payload)`.
pub(crate) fn content_json_from_cdn_payload(payload: &Dict) -> Dict {
    match get(payload, "content") {
        Value::Dict(dict) => return dict.clone(),
        content @ Value::Str(_) => return coerce_json_object(content),
        _ => {}
    }
    if matches!(get(payload, "baseData"), Value::List(_)) || matches!(get(payload, "styleData"), Value::List(_)) {
        return payload.clone();
    }
    Dict::new()
}

/// `cdn_json_url(detail, field)`.
pub(crate) fn cdn_json_url(detail: &Dict, field: &str) -> String {
    match get(detail, field) {
        Value::Str(value) if !py_strip(value).is_empty() => normalize_url(value),
        _ => String::new(),
    }
}

/// `reverse_bind_from_object(entry_data_bind)`: dynamic cell key to stable id.
fn reverse_bind_from_object(entry_data_bind: &Value) -> Dict {
    let mut reverse = Dict::new();
    for (stable_id, dynamic_key) in coerce_json_object(entry_data_bind).iter() {
        reverse.insert(str_value(dynamic_key), Value::Str(str_key(stable_id)));
    }
    reverse
}

/// `reverse_bind_from_cdn_payload(payload)`.
pub(crate) fn reverse_bind_from_cdn_payload(payload: &Dict) -> Dict {
    match payload.get("entry_data_bind") {
        Some(value) => reverse_bind_from_object(value),
        None => reverse_bind_from_object(&Value::Dict(payload.clone())),
    }
}

/// `reverse_bind_map(detail)`.
pub(crate) fn reverse_bind_map(detail: &Dict) -> Dict {
    reverse_bind_from_object(get(detail, "entry_data_bind"))
}

fn bound_id(reverse_bind: &Dict, key: &str) -> String {
    reverse_bind.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// `cell_text(cell)`.
fn cell_text(cell: &Value) -> String {
    match cell.get("value") {
        Some(Value::Str(value)) => py_strip(value).to_owned(),
        _ => String::new(),
    }
}

/// `first_text(row)`.
fn first_text(row: &[Value]) -> String {
    row.iter().map(cell_text).find(|value| !value.is_empty()).unwrap_or_default()
}

/// `row_label(row)`.
fn row_label(row: &[Value]) -> String {
    let Some(first) = row.first() else {
        return String::new();
    };
    let label = cell_text(first);
    if label.is_empty() { first_text(row) } else { label }
}

/// `row_value(row, column_index)`.
fn row_value(row: &[Value], column_index: usize) -> String {
    row.get(column_index).map(cell_text).unwrap_or_default()
}

/// `CostumeMeta`.
#[derive(Debug, Clone)]
struct CostumeMeta {
    title: String,
    category: String,
}

/// `StyleRows`.
#[derive(Debug, Clone)]
struct StyleRows<'a> {
    style_index: usize,
    style_name: String,
    rows: &'a [Value],
    costume: CostumeMeta,
}

impl StyleRows<'_> {
    /// Row `index` if it is a list.
    fn list_row(&self, index: usize) -> Option<&[Value]> {
        self.rows.get(index).and_then(Value::as_list)
    }

    fn column_header(&self, column_index: usize) -> String {
        self.list_row(STYLE_LIVE2D_HEADER_ROW)
            .map(|row| row_value(row, column_index))
            .unwrap_or_default()
    }

    fn column_name(&self, column_index: usize) -> String {
        self.list_row(STYLE_COSTUME_NAME_ROW)
            .map(|row| row_value(row, column_index))
            .unwrap_or_default()
    }

    fn column_category(&self, column_index: usize) -> String {
        self.list_row(STYLE_COSTUME_CATEGORY_ROW)
            .map(|row| row_value(row, column_index))
            .unwrap_or_default()
    }

    fn column_role(&self, column_index: usize) -> String {
        let header = self.column_header(column_index);
        if header.is_empty() {
            self.column_name(column_index)
        } else {
            header
        }
    }
}

/// `style_rows(content_json)`.
fn style_rows(content_json: &Dict) -> Vec<StyleRows<'_>> {
    let Value::List(styles) = get(content_json, "styleData") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (style_index, style) in styles.iter().enumerate() {
        let Value::Dict(style) = style else { continue };
        let Value::List(rows) = get(style, "data") else { continue };
        let name = get(style, "name");
        let style_name = if name.is_truthy() {
            str_value(name)
        } else {
            format!("style-{}", style_index + 1)
        };
        let costume = CostumeMeta {
            title: rows
                .first()
                .and_then(Value::as_list)
                .map(|row| row_value(row, 1))
                .unwrap_or_default(),
            category: rows
                .get(1)
                .and_then(Value::as_list)
                .map(|row| row_value(row, 1))
                .unwrap_or_default(),
        };
        out.push(StyleRows {
            style_index,
            style_name,
            rows,
            costume,
        });
    }
    out
}

/// `cell_summary(cell, reverse_bind)`.
fn cell_summary(cell: &Dict, reverse_bind: &Dict) -> Dict {
    let key = str_value(&or_empty(get(cell, "key")));
    let stable_id = bound_id(reverse_bind, &key);
    let mut summary = Dict::new();
    summary.insert("key", Value::Str(key));
    summary.insert("is_bound", Value::Bool(!stable_id.is_empty()));
    summary.insert("stable_id", Value::Str(stable_id));
    // Python builds the dict literal in this order: key, stable_id, is_bound.
    let is_bound = summary.remove("is_bound").unwrap_or(Value::Bool(false));
    summary.insert("is_bound", is_bound);
    summary.insert("type", or_empty(get(cell, "type")));
    summary.insert("value", get(cell, "value").clone());
    if cell.contains_key("isLimit") {
        summary.insert("is_limit", get(cell, "isLimit").clone());
    }
    summary
}

/// `summarize_rows(rows, reverse_bind)`.
fn summarize_rows(rows: &[Value], reverse_bind: &Dict) -> Vec<Value> {
    let mut summary = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let Value::List(row) = row else { continue };
        let cells: Vec<Value> = row
            .iter()
            .filter_map(Value::as_dict)
            .map(|cell| Value::Dict(cell_summary(cell, reverse_bind)))
            .collect();
        let mut entry = Dict::new();
        entry.insert("row_index", int(index));
        entry.insert("label", Value::Str(row_label(row)));
        entry.insert("cells", Value::List(cells));
        summary.push(Value::Dict(entry));
    }
    summary
}

/// `summarize_content(content_json, reverse_bind)`.
pub(crate) fn summarize_content(content_json: &Dict, reverse_bind: &Dict) -> Dict {
    let base_rows = get(content_json, "baseData").as_list().unwrap_or_default();
    let mut costumes = Vec::new();
    for style in style_rows(content_json) {
        let mut costume = Dict::new();
        costume.insert("style_index", int(style.style_index));
        costume.insert("style_name", Value::Str(style.style_name.clone()));
        costume.insert("title", Value::Str(style.costume.title.clone()));
        costume.insert("category", Value::Str(style.costume.category.clone()));
        costume.insert("rows", Value::List(summarize_rows(style.rows, reverse_bind)));
        costumes.push(Value::Dict(costume));
    }
    let mut summary = Dict::new();
    summary.insert("base_rows", Value::List(summarize_rows(base_rows, reverse_bind)));
    summary.insert("costumes", Value::List(costumes));
    summary
}

/// `base_info_from_summary(content_summary)`: bound, non-empty values of each labelled
/// base row (the label cell itself left out).
pub(crate) fn base_info_from_summary(content_summary: &Dict) -> Dict {
    let mut base_info = Dict::new();
    // `content_summary.get('base_rows', [])`; the summaries this reads always hold a list.
    let rows = get(content_summary, "base_rows").as_list().unwrap_or_default();
    for row in rows {
        let Value::Dict(row) = row else { continue };
        let (Value::Str(label), Value::List(cells)) = (get(row, "label"), get(row, "cells")) else {
            continue;
        };
        if label.is_empty() {
            continue;
        }
        let values: Vec<Value> = cells
            .iter()
            .skip(1)
            .filter(|cell| {
                cell.as_dict().is_some_and(|cell| {
                    get(cell, "is_bound").is_truthy() && !matches!(get(cell, "value"), Value::Null) && !py_eq(get(cell, "value"), &text(""))
                })
            })
            .cloned()
            .collect();
        if values.is_empty() {
            continue;
        }
        let mut entry = Dict::new();
        entry.insert("row_index", get(row, "row_index").clone());
        entry.insert("values", Value::List(values));
        let mut list = base_info
            .get(label)
            .and_then(Value::as_list)
            .map(<[Value]>::to_vec)
            .unwrap_or_default();
        list.push(Value::Dict(entry));
        base_info.insert(label.as_str(), Value::List(list));
    }
    base_info
}

// ---------- the character tree ----------

/// `row_content_id(row)`: a positive id, else `None`.
pub(crate) fn row_content_id(row: &Dict) -> Result<Option<i128>, JobError> {
    Ok(to_int(get(row, "content_id"))?.filter(|id| *id > 0))
}

/// `row_name(row)`.
pub(crate) fn row_name(row: &Dict) -> String {
    for key in ["name", "title", "entry_name", "entryName"] {
        if let Value::Str(value) = get(row, key)
            && !py_strip(value).is_empty()
        {
            return py_strip(value).to_owned();
        }
    }
    String::new()
}

/// `detail_name(detail, content_id, row)`.
pub(crate) fn detail_name(detail: &Dict, content_id: i128, row: Option<&Dict>) -> String {
    for key in ["title", "name", "entry_name", "entryName"] {
        if let Value::Str(value) = get(detail, key)
            && !py_strip(value).is_empty()
        {
            return py_strip(value).to_owned();
        }
    }
    if let Some(row) = row.filter(|row| !row.is_empty()) {
        let name = row_name(row);
        if !name.is_empty() {
            return name;
        }
    }
    content_id.to_string()
}

fn character_group(id: i128) -> Option<&'static str> {
    CHARACTER_GROUPS.iter().find(|(group, _)| *group == id).map(|(_, name)| *name)
}

/// `_walk_tree_rows(rows, state)`: character rows under a star group, with their group and
/// sort path, each with the path it sorts by.
fn walk_tree_rows(
    rows: &Value,
    group_id: i128,
    group_name: &str,
    sort_path: &[i128],
    out: &mut Vec<(Vec<i128>, Dict)>,
) -> Result<(), JobError> {
    let Value::List(rows) = rows else { return Ok(()) };
    for (fallback_sort, item) in rows.iter().enumerate() {
        let Value::Dict(item) = item else { continue };
        let node_id = to_int(get(item, "id"))?.unwrap_or(0);
        let (mut group_id, mut group_name) = (group_id, group_name);
        if let Some(name) = character_group(node_id) {
            group_id = node_id;
            group_name = name;
        }
        let sort_value = to_int(get(item, "sort"))?;
        let mut next_path = sort_path.to_vec();
        next_path.push(sort_value.unwrap_or(i128::try_from(fallback_sort).unwrap_or(i128::MAX)));

        let content_id = row_content_id(item)?;
        if character_group(group_id).is_some() && content_id.is_some() {
            let mut row = with(
                item,
                &[
                    ("_bd2_group_id", Value::Int(group_id)),
                    ("_bd2_group_name", text(group_name)),
                    (
                        "_bd2_sort_path",
                        Value::List(next_path.iter().map(|value| Value::Int(*value)).collect()),
                    ),
                ],
            );
            row.remove("child");
            out.push((next_path.clone(), row));
        }
        walk_tree_rows(get(item, "child"), group_id, group_name, &next_path, out)?;
    }
    Ok(())
}

/// `filter_bd2_character_rows(tree_rows)`: one row per positive content id (the first
/// found), sorted by the tree's sort path.
pub(crate) fn filter_bd2_character_rows(tree_rows: &Value) -> Result<Vec<Dict>, JobError> {
    let mut rows = Vec::new();
    walk_tree_rows(tree_rows, 0, "", &[], &mut rows)?;
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for (path, row) in rows {
        let Some(content_id) = row_content_id(&row)? else { continue };
        if seen.insert(content_id) {
            deduped.push((path, row));
        }
    }
    deduped.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(deduped.into_iter().map(|(_, row)| row).collect())
}

// ---------- media contexts ----------

fn base_media_context(row: &[Value], cell: &Dict, row_index: usize, column_index: usize, reverse_bind: &Dict) -> Dict {
    let key = str_value(&or_empty(get(cell, "key")));
    let mut context = Dict::new();
    context.insert("section", text("base"));
    context.insert("row_index", int(row_index));
    context.insert("column_index", int(column_index));
    context.insert("label", Value::Str(row_label(row)));
    context.insert("field", text(""));
    context.insert("stable_id", Value::Str(bound_id(reverse_bind, &key)));
    context.insert("cell_type", or_empty(get(cell, "type")));
    // Python's literal order: key comes before stable_id.
    let mut ordered = Dict::new();
    for name in ["section", "row_index", "column_index", "label", "field"] {
        ordered.insert(name, get(&context, name).clone());
    }
    ordered.insert("key", Value::Str(key));
    ordered.insert("stable_id", get(&context, "stable_id").clone());
    ordered.insert("cell_type", get(&context, "cell_type").clone());
    ordered
}

fn style_media_context(
    style: &StyleRows<'_>,
    row: &[Value],
    cell: &Dict,
    row_index: usize,
    column_index: usize,
    reverse_bind: &Dict,
) -> Dict {
    let key = str_value(&or_empty(get(cell, "key")));
    let art_field = art_row_field(row_index);
    let mut context = Dict::new();
    context.insert("section", text("style"));
    context.insert("style_index", int(style.style_index));
    context.insert("style_name", Value::Str(style.style_name.clone()));
    context.insert("costume_title", Value::Str(style.costume.title.clone()));
    context.insert("costume_category", Value::Str(style.costume.category.clone()));
    context.insert("row_index", int(row_index));
    context.insert("column_index", int(column_index));
    context.insert("label", Value::Str(row_label(row)));
    context.insert("field", text(art_field.unwrap_or_default()));
    context.insert("is_art_row", Value::Bool(art_field.is_some()));
    context.insert("column_name", Value::Str(style.column_name(column_index)));
    context.insert("column_category", Value::Str(style.column_category(column_index)));
    context.insert("column_role", Value::Str(style.column_role(column_index)));
    context.insert("column_header", Value::Str(style.column_header(column_index)));
    context.insert("stable_id", Value::Str(bound_id(reverse_bind, &key)));
    context.insert("key", Value::Str(key));
    let stable_id = context.remove("stable_id").unwrap_or(Value::Null);
    context.insert("stable_id", stable_id);
    context.insert("cell_type", or_empty(get(cell, "type")));
    context
}

/// `extract_live2d_model(assets=..., context=..., value=...)`: the model entry, and its
/// atlas, skeleton, textures and background as assets.
fn extract_live2d_model(assets: &mut Assets, context: &Dict, value: &Dict) -> Dict {
    let live2d_key = str_value(&or_empty(get(value, "live2dKey")));
    let mut model = Dict::new();
    model.insert("section", or_empty(get(context, "section")));
    model.insert("style_index", get(context, "style_index").clone());
    for name in ["style_name", "costume_title", "costume_category"] {
        model.insert(name, or_empty(get(context, name)));
    }
    model.insert("row_index", get(context, "row_index").clone());
    model.insert("column_index", get(context, "column_index").clone());
    model.insert("label", or_empty(get(context, "label")));
    model.insert("field", or_empty(get(context, "field")));
    model.insert("is_art_row", Value::Bool(get(context, "is_art_row").is_truthy()));
    for name in ["column_name", "column_category", "column_role", "column_header", "key", "stable_id"] {
        model.insert(name, or_empty(get(context, name)));
    }
    model.insert("live2d_key", Value::Str(live2d_key.clone()));
    model.insert("animation", or_empty(get(value, "animation")));
    model.insert("skin", or_empty(get(value, "skin")));
    model.insert("limit_age", Value::Bool(get(value, "limitAge").is_truthy()));
    model.insert("position", or_value(get(value, "position"), Value::Dict(Dict::new())));
    model.insert("bg_position", or_value(get(value, "bgPosition"), Value::Dict(Dict::new())));

    let mut urls = Dict::new();
    let live2d_context = |field: &str| with(context, &[("live2d_key", text(&live2d_key)), ("live2d_field", text(field))]);
    for field_name in LIVE2D_URL_FIELDS {
        if let Value::Str(raw_url) = get(value, field_name)
            && !py_strip(raw_url).is_empty()
        {
            let normalized = normalize_url(raw_url);
            urls.insert(field_name, Value::Str(normalized.clone()));
            add_asset(assets, &normalized, &format!("live2d_{field_name}"), live2d_context(field_name));
        }
    }
    if let Value::Str(images) = get(value, "image")
        && !py_strip(images).is_empty()
    {
        let image_urls: Vec<String> = images
            .split(',')
            .filter(|raw| !py_strip(raw).is_empty())
            .map(normalize_url)
            .collect();
        urls.insert("image", Value::List(image_urls.iter().cloned().map(Value::Str).collect()));
        for image_url in &image_urls {
            add_asset(assets, image_url, "live2d_texture", live2d_context("image"));
        }
    }
    if let Value::Str(bg) = get(value, "bg")
        && !py_strip(bg).is_empty()
    {
        let normalized = normalize_url(bg);
        urls.insert("bg", Value::Str(normalized.clone()));
        add_asset(assets, &normalized, "live2d_background", live2d_context("bg"));
    }
    model.insert("urls", Value::Dict(urls));
    model
}

fn is_media_cell(cell: &Dict) -> bool {
    get(cell, "type").as_str().is_some_and(|kind| MEDIA_CELL_TYPES.contains(&kind))
}

fn extract_media_cell(assets: &mut Assets, live2d_models: &mut Vec<Dict>, cell: &Dict, context: &Dict) {
    let value = get(cell, "value");
    let kind = get(cell, "type").as_str().unwrap_or_default();
    match value {
        Value::Dict(value) if kind == "live2d" => live2d_models.push(extract_live2d_model(assets, context, value)),
        Value::Str(_) => add_url_list(assets, value, kind, context),
        _ => {}
    }
}

/// `extract_resources(content_json=..., tree_row=..., reverse_bind=...)`: the page's media
/// assets and Live2D models, base rows first, then each style, then the tree row's icons.
pub(crate) fn extract_resources(content_json: &Dict, tree_row: Option<&Dict>, reverse_bind: &Dict) -> (Assets, Vec<Dict>) {
    let mut assets = Assets::new();
    let mut models = Vec::new();
    if let Value::List(rows) = get(content_json, "baseData") {
        for (row_index, row) in rows.iter().enumerate() {
            let Value::List(row) = row else { continue };
            for (column_index, cell) in row.iter().enumerate() {
                let Value::Dict(cell) = cell else { continue };
                if !is_media_cell(cell) {
                    continue;
                }
                let context = base_media_context(row, cell, row_index, column_index, reverse_bind);
                extract_media_cell(&mut assets, &mut models, cell, &context);
            }
        }
    }
    for style in style_rows(content_json) {
        for (row_index, row) in style.rows.iter().enumerate() {
            let Value::List(row) = row else { continue };
            for (column_index, cell) in row.iter().enumerate() {
                let Value::Dict(cell) = cell else { continue };
                if !is_media_cell(cell) {
                    continue;
                }
                let context = style_media_context(&style, row, cell, row_index, column_index, reverse_bind);
                extract_media_cell(&mut assets, &mut models, cell, &context);
            }
        }
    }
    if let Some(tree_row) = tree_row.filter(|row| !row.is_empty()) {
        for field_name in ["icon", "icon_small"] {
            let value = get(tree_row, field_name);
            if let Value::Str(raw) = value
                && !py_strip(raw).is_empty()
            {
                let mut context = Dict::new();
                context.insert("section", text("entry_tree"));
                context.insert("field", text(field_name));
                context.insert("label", text(field_name));
                add_url_list(&mut assets, value, "image", &context);
            }
        }
    }
    (assets, models)
}

// ---------- BD2-L2D-Viewer preference and supplements ----------

/// `_live2d_model_urls(model)`: every URL string of `model['urls']`.
fn live2d_model_urls(model: &Dict) -> Vec<String> {
    let Value::Dict(urls) = get(model, "urls") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (_, value) in urls.iter() {
        match value {
            Value::Str(url) => out.push(url.clone()),
            Value::List(items) => out.extend(items.iter().filter_map(Value::as_str).map(str::to_owned)),
            _ => {}
        }
    }
    out
}

/// `_model_stem_keys(model)`.
fn model_stem_keys(model: &Dict) -> Result<HashSet<String>, JobError> {
    let mut keys = HashSet::new();
    for url in live2d_model_urls(model) {
        let stem = resource_stem_from_url(&url).map_err(py_error)?;
        if !stem.is_empty() {
            keys.insert(casefold(&stem));
        }
    }
    Ok(keys)
}

/// `_live2d_models_by_stem(live2d_models)`, as indices into the list.
fn models_by_stem(models: &[Dict]) -> Result<HashMap<String, Vec<usize>>, JobError> {
    let mut grouped: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, model) in models.iter().enumerate() {
        for key in model_stem_keys(model)? {
            grouped.entry(key).or_default().push(index);
        }
    }
    Ok(grouped)
}

/// `_live2d_model_core_stems(model)`: the atlas, skeleton and JSON stems, first spelling
/// of each.
fn live2d_model_core_stems(model: &Dict) -> Result<Vec<String>, JobError> {
    let Value::Dict(urls) = get(model, "urls") else {
        return Ok(Vec::new());
    };
    let mut stems = Vec::new();
    let mut seen = HashSet::new();
    for field_name in LIVE2D_URL_FIELDS {
        let Value::Str(url) = get(urls, field_name) else { continue };
        let stem = resource_stem_from_url(url).map_err(py_error)?;
        let key = casefold(&stem);
        if !stem.is_empty() && !seen.contains(&key) {
            seen.insert(key);
            stems.push(stem);
        }
    }
    Ok(stems)
}

/// `_copy_live2d_urls(raw_urls)`.
fn copy_live2d_urls(raw_urls: &Value) -> Dict {
    match raw_urls {
        Value::Dict(urls) => urls.clone(),
        _ => Dict::new(),
    }
}

/// `_viewer_preferred_model_urls(original_urls, viewer_core_urls)`.
fn viewer_preferred_model_urls(original: &Dict, viewer_core_urls: &Dict) -> Dict {
    let mut urls = Dict::new();
    for (key, value) in original.iter() {
        let replaced =
            matches!(key, Key::Str(name) if LIVE2D_URL_FIELDS.contains(&name.as_str()) || name == "image" || name == "atlas_textures");
        if !replaced {
            urls.insert(key.clone(), value.clone());
        }
    }
    for (key, value) in viewer_core_urls.iter() {
        urls.insert(key.clone(), value.clone());
    }
    urls
}

/// `_same_live2d_asset_context(context, model)`.
fn same_live2d_asset_context(context: &Dict, model: &Dict) -> bool {
    if !py_eq(get(context, "live2d_key"), get(model, "live2d_key")) {
        return false;
    }
    ["section", "style_index", "row_index", "column_index", "field"]
        .iter()
        .all(|key| py_eq(get(context, key), get(model, key)))
}

/// `_remove_live2d_asset_contexts(assets=..., model=..., live2d_fields=...)`: drop the
/// model's contexts for those fields, and assets left with none.
fn remove_live2d_asset_contexts(assets: &mut Assets, model: &Dict, live2d_fields: &[&str]) {
    for asset in assets.iter_mut() {
        asset.contexts.retain(|context| {
            let field_matches = get(context, "live2d_field")
                .as_str()
                .is_some_and(|field| live2d_fields.contains(&field));
            !(same_live2d_asset_context(context, model) && field_matches)
        });
    }
    assets.retain(|asset| !asset.contexts.is_empty());
}

/// `_viewer_core_urls(resource)`: the first atlas, skeleton and JSON file of a viewer
/// resource as raw GitHub URLs.
fn viewer_core_urls(resource: &ViewerResource) -> Dict {
    let mut urls = Dict::new();
    for path in &resource.files {
        let field_name = match casefold(pypath::suffix(path)).as_str() {
            ".atlas" => "atlas",
            ".skel" => "skel",
            ".json" => "json",
            _ => continue,
        };
        if urls.contains_key(field_name) {
            continue;
        }
        let url = viewer_asset_url(path);
        if !url.is_empty() {
            urls.insert(field_name, Value::Str(url));
        }
    }
    urls
}

/// `_has_required_live2d_core(urls)`.
fn has_required_live2d_core(urls: &Dict) -> bool {
    urls.contains_key("atlas") && (urls.contains_key("skel") || urls.contains_key("json"))
}

fn select_viewer_preferred_resource<'a>(
    model: &Dict,
    viewer_by_stem: &HashMap<String, Vec<&'a ViewerResource>>,
) -> Result<Option<(&'a ViewerResource, Dict)>, JobError> {
    for stem in live2d_model_core_stems(model)? {
        let resources: Vec<&ViewerResource> = viewer_by_stem
            .get(&casefold(&stem))
            .map(|list| {
                list.iter()
                    .copied()
                    .filter(|resource| resource.missing_core_files.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if resources.is_empty() {
            continue;
        }
        let category = category_from_stem(&stem);
        for resource in resources.into_iter().filter(|resource| resource.category == category) {
            let urls = viewer_core_urls(resource);
            if has_required_live2d_core(&urls) {
                return Ok(Some((resource, urls)));
            }
        }
    }
    Ok(None)
}

fn apply_viewer_preferred_source(assets: &mut Assets, model: &mut Dict, resource: &ViewerResource, viewer_core_urls: &Dict) -> bool {
    let original_urls = copy_live2d_urls(get(model, "urls"));
    if original_urls.is_empty() {
        return false;
    }
    model.insert("gamekee_urls", Value::Dict(original_urls.clone()));
    let mut candidates = match get(model, "source_candidates") {
        Value::Dict(existing) => existing.clone(),
        _ => Dict::new(),
    };
    candidates.insert("gamekee", Value::Dict(original_urls.clone()));
    candidates.insert(VIEWER_SUPPLEMENT_SOURCE, Value::Dict(viewer_core_urls.clone()));
    model.insert("source_candidates", Value::Dict(candidates));
    model.insert("urls", Value::Dict(viewer_preferred_model_urls(&original_urls, viewer_core_urls)));
    model.insert("source", text(VIEWER_SUPPLEMENT_SOURCE));
    model.insert("variant", text(VIEWER_PREFERRED_SOURCE_VARIANT));
    model.insert("supplement_reason", text(REASON_MATCHED_STEM_PREFERRED_SOURCE));
    model.insert("viewer_entry_id", Value::Str(resource.entry_id.clone()));
    model.insert("viewer_stem", Value::Str(resource.stem.clone()));
    model.insert("source_page_url", text(VIEWER_PAGE_URL));
    remove_live2d_asset_contexts(assets, model, &VIEWER_PREFERRED_REPLACED_LIVE2D_FIELDS);
    add_viewer_model_assets(assets, model);
    true
}

fn viewer_lists_by_stem(viewer_resources: &[ViewerResource]) -> HashMap<String, Vec<&ViewerResource>> {
    let mut grouped: HashMap<String, Vec<&ViewerResource>> = HashMap::new();
    for resource in viewer_resources {
        grouped.entry(casefold(&resource.stem)).or_default().push(resource);
    }
    grouped
}

/// `prefer_live2d_model_assets_from_viewer(assets=..., live2d_models=..., viewer_resources=...)`:
/// models whose atlas or skeleton stem the viewer also has (same category, all core files
/// present) switch to the viewer's files; the count of switched models.
pub(crate) fn prefer_live2d_model_assets_from_viewer(
    assets: &mut Assets,
    live2d_models: &mut [Dict],
    viewer_resources: &[ViewerResource],
) -> Result<usize, JobError> {
    let viewer_by_stem = viewer_lists_by_stem(viewer_resources);
    let mut preferred = 0;
    for model in live2d_models.iter_mut() {
        if py_eq(get(model, "source"), &text(VIEWER_SUPPLEMENT_SOURCE)) {
            continue;
        }
        let Some((resource, urls)) = select_viewer_preferred_resource(model, &viewer_by_stem)? else {
            continue;
        };
        if apply_viewer_preferred_source(assets, model, resource, &urls) {
            preferred += 1;
        }
    }
    Ok(preferred)
}

fn viewer_anchor_entry_ids(resource: &ViewerResource) -> Vec<String> {
    let mut ids = vec![resource.entry_id.clone()];
    if let Some(base) = resource.entry_id.strip_suffix("_c")
        && base != resource.entry_id
    {
        ids.push(base.to_owned());
    }
    ids
}

/// `_viewer_anchor_model(...)`: the first model sharing a stem with a sibling resource of
/// the same viewer entry (or its uncensored entry), siblings by category order.
fn viewer_anchor_model(
    resource: &ViewerResource,
    viewer_by_entry: &HashMap<&str, Vec<&ViewerResource>>,
    models_by_stem: &HashMap<String, Vec<usize>>,
) -> Option<usize> {
    for entry_id in viewer_anchor_entry_ids(resource) {
        let siblings = viewer_by_entry.get(entry_id.as_str()).map(Vec::as_slice).unwrap_or_default();
        for category in ANCHOR_CATEGORY_ORDER {
            for sibling in siblings.iter().filter(|sibling| sibling.category == category) {
                if let Some(first) = models_by_stem.get(&casefold(&sibling.stem)).and_then(|models| models.first()) {
                    return Some(*first);
                }
            }
        }
    }
    None
}

fn is_viewer_censored_resource(resource: &ViewerResource) -> bool {
    resource.entry_id.ends_with("_c") || resource.stem.ends_with("_c")
}

/// `ViewerSupplementTarget`.
#[derive(Debug, Clone, Copy)]
struct SupplementTarget {
    row_index: i64,
    field: &'static str,
    label: &'static str,
}

fn viewer_supplement_target(resource: &ViewerResource) -> Option<SupplementTarget> {
    match resource.category {
        Category::Character => Some(SupplementTarget {
            row_index: STANDING_LIVE2D_ROW,
            field: "standing_live2d",
            label: "Standing Live2D",
        }),
        Category::Dating => Some(SupplementTarget {
            row_index: INTERACTION_LIVE2D_ROW,
            field: "interaction_live2d",
            label: "Interaction Live2D",
        }),
        Category::Ultimate => Some(SupplementTarget {
            row_index: SKILL_LIVE2D_1_ROW,
            field: "skill_live2d_1",
            label: "Skill Live2D",
        }),
        Category::Unknown => None,
    }
}

fn text_values_from_row(row: &Value) -> Vec<String> {
    let Value::List(row) = row else {
        return Vec::new();
    };
    row.iter()
        .filter_map(Value::as_dict)
        .filter(|cell| get(cell, "type").as_str() == Some("text"))
        .filter_map(|cell| get(cell, "value").as_str())
        .map(py_strip)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `_page_character_names(content_json)`: the values of the base rows labelled 角色名称 /
/// Character Name.
fn page_character_names(content_json: &Dict) -> HashSet<String> {
    let Value::List(base_rows) = get(content_json, "baseData") else {
        return HashSet::new();
    };
    let mut names = HashSet::new();
    for row in base_rows {
        let values = text_values_from_row(row);
        let Some(label) = values.first() else { continue };
        let label = casefold(label);
        if label != "角色名称" && label != "character name" {
            continue;
        }
        names.extend(values.into_iter().skip(1).filter(|value| !value.is_empty()));
    }
    names
}

fn text_key(value: &str) -> String {
    casefold(py_strip(value))
}

fn has_matching_page_character(content_json: &Dict, resource: &ViewerResource) -> bool {
    let page_names: HashSet<String> = page_character_names(content_json).iter().map(|name| text_key(name)).collect();
    !resource.char_name.is_empty() && page_names.contains(&text_key(&resource.char_name))
}

/// `_style_live2d_model_columns(style)`: the columns whose header names a Live2D file
/// (not an icon), else column 1.
fn style_live2d_model_columns(style: &StyleRows<'_>) -> Vec<usize> {
    let Some(header_row) = style.list_row(STYLE_LIVE2D_HEADER_ROW) else {
        return vec![1];
    };
    let mut columns = Vec::new();
    for column_index in 1..header_row.len() {
        let header = casefold(&style.column_header(column_index));
        if !header.contains("live2d") && !header.contains("live 2d") {
            continue;
        }
        if ["头像", "icon", "avatar", "小人"].iter().any(|token| header.contains(token)) {
            continue;
        }
        if ["文件", "file"].iter().any(|token| header.contains(token)) {
            columns.push(column_index);
        }
    }
    if columns.is_empty() { vec![1] } else { columns }
}

fn page_live2d_slot_contexts(content_json: &Dict, reverse_bind: &Dict, target: SupplementTarget) -> Vec<Dict> {
    let target_row = usize::try_from(target.row_index).unwrap_or(usize::MAX);
    let mut contexts = Vec::new();
    for style in style_rows(content_json) {
        let Some(row) = style.list_row(target_row) else { continue };
        for column_index in style_live2d_model_columns(&style) {
            let cell = if let Some(Value::Dict(cell)) = row.get(column_index) {
                cell.clone()
            } else {
                let mut empty = Dict::new();
                empty.insert("key", text(""));
                empty.insert("value", text(""));
                empty
            };
            let cell = with(&cell, &[("type", text("live2d"))]);
            contexts.push(style_media_context(&style, row, &cell, target_row, column_index, reverse_bind));
        }
    }
    contexts
}

/// `_same_live2d_slot(model, anchor, field_name=...)`.
fn same_live2d_slot(model: &Dict, anchor: &Dict, field_name: &str) -> bool {
    py_eq(get(model, "style_index"), get(anchor, "style_index"))
        && py_eq(get(model, "column_index"), get(anchor, "column_index"))
        && py_eq(get(model, "field"), &text(field_name))
}

fn slot_has_mismatched_viewer_owner(
    models: &[&Dict],
    page_names: &HashSet<String>,
    viewer_by_stem: &HashMap<String, &ViewerResource>,
) -> Result<bool, JobError> {
    let page_name_keys: HashSet<String> = page_names.iter().map(|name| text_key(name)).collect();
    for model in models {
        for stem_key in model_stem_keys(model)? {
            let Some(owner) = viewer_by_stem.get(&stem_key) else { continue };
            if owner.char_name.is_empty() {
                continue;
            }
            if !page_name_keys.contains(&text_key(&owner.char_name)) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn viewer_slot_is_filled(models: &[Dict], anchor: &Dict, target: SupplementTarget, resource: &ViewerResource) -> bool {
    if is_viewer_censored_resource(resource) && target.field == "standing_live2d" {
        return false;
    }
    models.iter().any(|model| same_live2d_slot(model, anchor, target.field))
}

/// `_viewer_supplement_context(model, live2d_field=...)`.
fn viewer_supplement_context(model: &Dict, live2d_field: &str) -> Dict {
    let mut context = live2d_atlas_texture_context(model);
    context.insert("live2d_field", text(live2d_field));
    context
}

fn viewer_supplement_model(
    anchor: &Dict,
    resource: &ViewerResource,
    target: SupplementTarget,
    reason: &str,
    variant: &str,
) -> Option<Dict> {
    let urls = viewer_core_urls(resource);
    if !has_required_live2d_core(&urls) {
        return None;
    }
    let variant = if variant.is_empty() && is_viewer_censored_resource(resource) {
        "censored"
    } else {
        variant
    };
    let mut model = Dict::new();
    model.insert("section", or_value(get(anchor, "section"), text("style")));
    model.insert("style_index", get(anchor, "style_index").clone());
    for name in ["style_name", "costume_title", "costume_category"] {
        model.insert(name, or_empty(get(anchor, name)));
    }
    model.insert("row_index", Value::Int(i128::from(target.row_index)));
    model.insert("column_index", get(anchor, "column_index").clone());
    model.insert("label", text(target.label));
    model.insert("field", text(target.field));
    model.insert("is_art_row", Value::Bool(true));
    for name in ["column_name", "column_category", "column_role", "column_header"] {
        model.insert(name, or_empty(get(anchor, name)));
    }
    model.insert("key", text(""));
    model.insert("stable_id", text(""));
    model.insert(
        "live2d_key",
        Value::Str(format!("bd2-l2d-viewer-{}-{}", resource.entry_id, resource.stem)),
    );
    model.insert("animation", text(""));
    model.insert("skin", text(""));
    model.insert("limit_age", Value::Bool(false));
    model.insert("position", Value::Dict(Dict::new()));
    model.insert("bg_position", Value::Dict(Dict::new()));
    model.insert("source", text(VIEWER_SUPPLEMENT_SOURCE));
    model.insert("variant", text(variant));
    model.insert("supplement_reason", text(reason));
    model.insert("viewer_entry_id", Value::Str(resource.entry_id.clone()));
    model.insert("viewer_stem", Value::Str(resource.stem.clone()));
    model.insert("source_page_url", text(VIEWER_PAGE_URL));
    model.insert("urls", Value::Dict(urls));
    Some(model)
}

/// `_add_viewer_model_assets(assets, model)`.
pub(crate) fn add_viewer_model_assets(assets: &mut Assets, model: &Dict) {
    let Value::Dict(urls) = get(model, "urls") else { return };
    for field_name in LIVE2D_URL_FIELDS {
        if let Value::Str(url) = get(urls, field_name)
            && !url.is_empty()
        {
            add_asset(
                assets,
                url,
                &format!("live2d_{field_name}"),
                viewer_supplement_context(model, field_name),
            );
        }
    }
}

/// `ViewerSupplementAnchor`.
struct Anchor {
    context: Dict,
    reason: &'static str,
    variant: &'static str,
}

fn viewer_page_anchor(
    resource: &ViewerResource,
    models: &[Dict],
    viewer_by_stem: &HashMap<String, &ViewerResource>,
    content_json: Option<&Dict>,
    reverse_bind: Option<&Dict>,
) -> Result<Option<Anchor>, JobError> {
    let Some(content_json) = content_json else {
        return Ok(None);
    };
    if resource.category != Category::Character || !has_matching_page_character(content_json, resource) {
        return Ok(None);
    }
    let Some(target) = viewer_supplement_target(resource) else {
        return Ok(None);
    };
    let page_names = page_character_names(content_json);
    let empty = Dict::new();
    let slots = page_live2d_slot_contexts(content_json, reverse_bind.unwrap_or(&empty), target);
    let mut candidates = Vec::new();
    for slot in slots {
        let slot_models: Vec<&Dict> = models.iter().filter(|model| same_live2d_slot(model, &slot, target.field)).collect();
        if slot_models.is_empty() {
            candidates.push(Anchor {
                context: slot,
                reason: REASON_EMPTY_SLOT,
                variant: "",
            });
        } else if slot_has_mismatched_viewer_owner(&slot_models, &page_names, viewer_by_stem)? {
            candidates.push(Anchor {
                context: slot,
                reason: REASON_MISMATCHED_SLOT,
                variant: "viewer_correction",
            });
        }
    }
    let mismatched = candidates
        .iter()
        .filter(|candidate| candidate.reason == REASON_MISMATCHED_SLOT)
        .count();
    if mismatched == 1 {
        return Ok(candidates.into_iter().find(|candidate| candidate.reason == REASON_MISMATCHED_SLOT));
    }
    if candidates.len() == 1 {
        return Ok(candidates.pop());
    }
    Ok(None)
}

/// `supplement_live2d_models_from_viewer(assets=..., live2d_models=..., viewer_resources=...,
/// content_json=..., reverse_bind=...)`: viewer resources the page lacks become models in an
/// empty or mismatched slot next to a sibling's model, or in the page's own slot for the
/// character; the count added.
pub(crate) fn supplement_live2d_models_from_viewer(
    assets: &mut Assets,
    live2d_models: &mut Vec<Dict>,
    viewer_resources: &[ViewerResource],
    content_json: Option<&Dict>,
    reverse_bind: Option<&Dict>,
) -> Result<usize, JobError> {
    let mut by_stem = models_by_stem(live2d_models)?;
    let mut existing: HashSet<String> = by_stem.keys().cloned().collect();
    let mut viewer_by_entry: HashMap<&str, Vec<&ViewerResource>> = HashMap::new();
    let mut viewer_by_stem: HashMap<String, &ViewerResource> = HashMap::new();
    for resource in viewer_resources {
        viewer_by_entry.entry(resource.entry_id.as_str()).or_default().push(resource);
        viewer_by_stem.insert(casefold(&resource.stem), resource);
    }
    let mut added = 0;
    for resource in viewer_resources {
        let stem_key = casefold(&resource.stem);
        if existing.contains(&stem_key) || !resource.missing_core_files.is_empty() {
            continue;
        }
        let Some(target) = viewer_supplement_target(resource) else {
            continue;
        };
        let anchor_index = viewer_anchor_model(resource, &viewer_by_entry, &by_stem);
        let anchor = match anchor_index {
            Some(index) if !viewer_slot_is_filled(live2d_models, &live2d_models[index], target, resource) => Some(Anchor {
                context: live2d_models[index].clone(),
                reason: if is_viewer_censored_resource(resource) {
                    REASON_CENSORED_VARIANT
                } else {
                    REASON_EMPTY_SLOT
                },
                variant: "",
            }),
            _ => viewer_page_anchor(resource, live2d_models, &viewer_by_stem, content_json, reverse_bind)?,
        };
        let Some(anchor) = anchor else { continue };
        let Some(model) = viewer_supplement_model(&anchor.context, resource, target, anchor.reason, anchor.variant) else {
            continue;
        };
        add_viewer_model_assets(assets, &model);
        live2d_models.push(model);
        existing.insert(stem_key.clone());
        by_stem.entry(stem_key).or_default().push(live2d_models.len() - 1);
        added += 1;
    }
    Ok(added)
}

// ---------- viewer extras ----------

/// `is_viewer_extra_resource(resource)`: a complete `minigame*` entry.
fn is_viewer_extra_resource(resource: &ViewerResource) -> bool {
    casefold(&resource.entry_id).starts_with("minigame") && resource.missing_core_files.is_empty()
}

/// `viewer_extra_resources(viewer_resources)`.
pub(crate) fn viewer_extra_resources(viewer_resources: &[ViewerResource]) -> Vec<ViewerResource> {
    viewer_resources
        .iter()
        .filter(|resource| is_viewer_extra_resource(resource))
        .cloned()
        .collect()
}

/// `viewer_extra_title(resource)`.
pub(crate) fn viewer_extra_title(resource: &ViewerResource) -> String {
    let parts: Vec<&str> = [resource.char_name.as_str(), resource.costume_name.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect();
    let joined = parts.join(" - ");
    if !joined.is_empty() {
        joined
    } else if !resource.stem.is_empty() {
        resource.stem.clone()
    } else {
        resource.entry_id.clone()
    }
}

/// `viewer_extra_root(base_dir, resource)`.
pub(crate) fn viewer_extra_root(base_dir: &str, resource: &ViewerResource) -> String {
    let entry_id = sanitize(&resource.entry_id, 80);
    let safe_entry_id = if entry_id.is_empty() { "viewer-extra".to_owned() } else { entry_id };
    let title = sanitize(&viewer_extra_title(resource), 120);
    let safe_title = if title.is_empty() { safe_entry_id.clone() } else { title };
    pypath::join(&pypath::join(base_dir, "_extra"), &format!("{safe_entry_id} - {safe_title}"))
}

/// `viewer_extra_live2d_model(resource)`.
pub(crate) fn viewer_extra_live2d_model(resource: &ViewerResource) -> Option<Dict> {
    let urls = viewer_core_urls(resource);
    if !has_required_live2d_core(&urls) {
        return None;
    }
    let title = viewer_extra_title(resource);
    let mut model = Dict::new();
    model.insert("section", text("extra"));
    model.insert("style_index", Value::Null);
    model.insert("style_name", text("extra"));
    model.insert("costume_title", Value::Str(title.clone()));
    model.insert("costume_category", text("extra"));
    model.insert("row_index", Value::Null);
    model.insert("column_index", Value::Null);
    model.insert("label", text("Extra Live2D"));
    model.insert("field", text("extra_live2d"));
    model.insert("is_art_row", Value::Bool(false));
    model.insert("column_name", Value::Str(title));
    model.insert("column_category", text("extra"));
    model.insert("column_role", text("Extra Live2D"));
    model.insert("column_header", text(""));
    model.insert("key", text(""));
    model.insert("stable_id", text(""));
    model.insert(
        "live2d_key",
        Value::Str(format!("bd2-l2d-viewer-extra-{}-{}", resource.entry_id, resource.stem)),
    );
    model.insert("animation", text(""));
    model.insert("skin", text(""));
    model.insert("limit_age", Value::Bool(false));
    model.insert("position", Value::Dict(Dict::new()));
    model.insert("bg_position", Value::Dict(Dict::new()));
    model.insert("source", text(VIEWER_SUPPLEMENT_SOURCE));
    model.insert("variant", text("extra"));
    model.insert("supplement_reason", text(VIEWER_EXTRA_REASON));
    model.insert("viewer_entry_id", Value::Str(resource.entry_id.clone()));
    model.insert("viewer_stem", Value::Str(resource.stem.clone()));
    model.insert("source_page_url", text(VIEWER_PAGE_URL));
    model.insert("urls", Value::Dict(urls));
    Some(model)
}

// ---------- archive paths ----------

/// `extension_for_kind(kind)`.
pub(crate) fn extension_for_kind(kind: &str) -> &'static str {
    match kind {
        "audio" => ".mp3",
        "video" => ".mp4",
        "live2d_atlas" => ".atlas",
        "live2d_skel" => ".skel",
        "live2d_json" => ".json",
        _ => ".bin",
    }
}

/// `_safe_suffix(filename, kind)`.
fn safe_suffix(filename: &str, kind: &str) -> String {
    let suffix = pypath::suffix(filename);
    let suffix = if suffix.is_empty() { extension_for_kind(kind) } else { suffix };
    let suffix = if suffix.starts_with('.') {
        suffix.to_owned()
    } else {
        format!(".{suffix}")
    };
    let sanitized = sanitize(&suffix, 24);
    if sanitized.starts_with('.') {
        sanitized
    } else if sanitized.is_empty() {
        ".bin".to_owned()
    } else {
        format!(".{sanitized}")
    }
}

/// `_context_source_id(context)`.
fn context_source_id(context: &Dict) -> String {
    for key in ["field", "stable_id", "live2d_field", "column_role", "key", "label"] {
        if let Value::Str(value) = get(context, key)
            && !py_strip(value).is_empty()
        {
            return py_strip(value).to_owned();
        }
    }
    match get(context, "row_index") {
        Value::Null => "asset".to_owned(),
        row_index => format!("row-{}", str_value(row_index)),
    }
}

/// `filename_from_url(url, kind, contexts, content_id)`.
pub(crate) fn filename_from_url(url: &str, kind: &str, contexts: &[Dict], content_id: i128) -> Result<String, JobError> {
    let original = gamekee::original_filename_from_url(url, kind).map_err(py_error)?;
    if kind.starts_with("live2d_") {
        return Ok(original);
    }
    let empty = Dict::new();
    let context = contexts.first().unwrap_or(&empty);
    let suffix = safe_suffix(&original, kind);
    let original_stem = pypath::stem(&original);
    let original_stem = if original_stem.is_empty() { kind } else { original_stem };
    let source_id = context_source_id(context);
    let stem = sanitize(&format!("{content_id}_{kind}_{source_id}_{original_stem}"), 180);
    let stem = if stem.is_empty() { format!("{content_id}_{kind}") } else { stem };
    Ok(format!("{stem}{suffix}"))
}

/// `asset_subdir(asset)`.
fn asset_subdir(asset: &Asset) -> String {
    if asset.kind.starts_with("live2d_") {
        let model_key = asset
            .contexts
            .iter()
            .map(|context| {
                [get(context, "live2d_key"), get(context, "stable_id"), get(context, "key")]
                    .into_iter()
                    .find(|value| value.is_truthy())
            })
            .find_map(Option::<&Value>::cloned)
            .unwrap_or_else(|| text("unknown"));
        let name = sanitize(&str_value(&model_key), 80);
        let name = if name.is_empty() { "unknown".to_owned() } else { name };
        return pypath::join("assets/live2d", &name);
    }
    match asset.kind.as_str() {
        "audio" => "assets/audio",
        "video" => "assets/videos",
        _ => "assets/images",
    }
    .to_owned()
}

/// `PurePosixPath(path).with_name(name)`.
fn with_name(path: &str, name: &str) -> String {
    match path.rsplit_once('/') {
        Some((parent, _)) => pypath::join(if parent.is_empty() { "/" } else { parent }, name),
        None => name.to_owned(),
    }
}

/// `PurePosixPath(path).with_stem(stem)`.
fn with_stem(path: &str, stem: &str) -> String {
    with_name(path, &format!("{stem}{}", pypath::suffix(path)))
}

/// `assign_asset_paths(assets, content_id=...)`: each asset's path under the character
/// directory, made unique with a hash of the URL.
pub(crate) fn assign_asset_paths(assets: &mut Assets, content_id: i128) -> Result<(), JobError> {
    let mut used: HashMap<String, String> = HashMap::new();
    for asset in assets.iter_mut() {
        let subdir = asset_subdir(asset);
        let filename = filename_from_url(&asset.url, &asset.kind, &asset.contexts, content_id)?;
        let mut relative = pypath::join(&subdir, &filename);
        let taken = |used: &HashMap<String, String>, key: &str| used.get(key).is_some_and(|url| *url != asset.url);
        if taken(&used, &relative) {
            let digest = &sha256_hex(asset.url.as_bytes())[..8];
            relative = if asset.kind.starts_with("live2d_") {
                let renamed = with_name(&subdir, &format!("{}-{digest}", pypath::name(&subdir)));
                pypath::join(&renamed, &filename)
            } else {
                with_stem(&relative, &format!("{}-{digest}", pypath::stem(&relative)))
            };
        }
        while taken(&used, &relative) {
            let digest = sha256_hex(format!("{}:{}", asset.url, used.len()).as_bytes());
            relative = with_stem(&relative, &format!("{}-{}", pypath::stem(&relative), &digest[..8]));
        }
        used.insert(relative.clone(), asset.url.clone());
        asset.local_path = relative;
    }
    Ok(())
}

/// `output_root(base_dir, detail, content_id, row)`.
pub(crate) fn output_root(base_dir: &str, detail: &Dict, content_id: i128, row: Option<&Dict>) -> String {
    let title = sanitize(&detail_name(detail, content_id, row), 120);
    let safe_title = if title.is_empty() { content_id.to_string() } else { title };
    pypath::join(base_dir, &format!("{content_id} - {safe_title}"))
}

/// `asset_counts(assets)`.
pub(crate) fn asset_counts(assets: &Assets) -> Dict {
    let mut counts = Dict::new();
    for asset in assets.iter() {
        let count = match counts.get(&asset.kind) {
            Some(Value::Int(count)) => count + 1,
            _ => 1,
        };
        counts.insert(asset.kind.as_str(), Value::Int(count));
    }
    counts
}

/// `relative_manifest_assets(assets)`.
pub(crate) fn relative_manifest_assets(assets: &Assets) -> Value {
    Value::List(
        assets
            .iter()
            .map(|asset| {
                let mut entry = Dict::new();
                entry.insert("url", Value::Str(asset.url.clone()));
                entry.insert("kind", Value::Str(asset.kind.clone()));
                entry.insert("local_path", Value::Str(asset.local_path.clone()));
                entry.insert("sha256", Value::Str(asset.sha256.clone()));
                entry.insert("content_type", Value::Str(asset.content_type.clone()));
                entry.insert("size", Value::Int(i128::from(asset.size)));
                entry.insert("status", Value::Str(asset.status.clone()));
                entry.insert("error", Value::Str(asset.error.clone()));
                entry.insert("contexts", Value::List(asset.contexts.iter().cloned().map(Value::Dict).collect()));
                Value::Dict(entry)
            })
            .collect(),
    )
}

/// `live2d_atlas_texture_context(model)`: the context of textures listed by a model's atlas.
pub(crate) fn live2d_atlas_texture_context(model: &Dict) -> Dict {
    let mut context = Dict::new();
    context.insert("section", or_empty(get(model, "section")));
    context.insert("style_index", get(model, "style_index").clone());
    for name in ["style_name", "costume_title", "costume_category"] {
        context.insert(name, or_empty(get(model, name)));
    }
    context.insert("row_index", get(model, "row_index").clone());
    context.insert("column_index", get(model, "column_index").clone());
    context.insert("label", or_empty(get(model, "label")));
    context.insert("field", or_empty(get(model, "field")));
    context.insert("is_art_row", Value::Bool(get(model, "is_art_row").is_truthy()));
    for name in [
        "column_name",
        "column_category",
        "column_role",
        "column_header",
        "key",
        "stable_id",
        "live2d_key",
    ] {
        context.insert(name, or_empty(get(model, name)));
    }
    context.insert("live2d_field", text("atlas_texture"));
    for name in [
        "source",
        "variant",
        "supplement_reason",
        "viewer_entry_id",
        "viewer_stem",
        "source_page_url",
    ] {
        context.insert(name, or_empty(get(model, name)));
    }
    context
}

/// `content_hash(detail_response=..., content_json=..., tree_row=...)`.
pub(crate) fn content_hash(detail_response: &Dict, content_json: &Dict, tree_row: Option<&Dict>) -> Result<String, JobError> {
    let mut payload = Dict::new();
    payload.insert("detail_response", Value::Dict(detail_response.clone()));
    payload.insert("content_json", Value::Dict(content_json.clone()));
    payload.insert("tree_row", tree_row.map_or(Value::Null, |row| Value::Dict(row.clone())));
    Ok(sha256_hex(gamekee::json_dumps(&Value::Dict(payload))?.as_bytes()))
}

// ---------- retries ----------

/// `retry_after_delay_seconds(response, now=...)`: `Retry-After` as seconds or an HTTP date,
/// capped at five minutes.
pub(crate) fn retry_after_delay_seconds(value: Option<&str>, now: DateTime<Utc>) -> Option<f64> {
    let value = value.filter(|value| !value.is_empty())?;
    let stripped = py_strip(value);
    if stripped.is_empty() {
        return None;
    }
    if py_isdecimal(stripped) {
        #[allow(clippy::cast_precision_loss)] // reason: `float(text)` of a digit string, capped at 300 below
        let seconds = py_int_decimal(stripped).map_or(f64::INFINITY, |seconds| seconds as f64);
        return Some(seconds.min(RETRY_AFTER_MAX_DELAY_SECONDS));
    }
    let retry_at = fav_net::retry::parsedate_to_datetime(stripped)?;
    #[allow(clippy::cast_precision_loss)] // reason: `timedelta.total_seconds()` is a float
    let delay = (retry_at - now)
        .num_microseconds()
        .map_or(f64::INFINITY, |micros| micros as f64 / 1e6);
    Some(delay.clamp(0.0, RETRY_AFTER_MAX_DELAY_SECONDS))
}

/// `retry_delay_seconds(attempt, response)`: the server's `Retry-After`, else the doubling
/// base delay plus `jitter_ms` (`secrets.randbelow(250)`) milliseconds.
pub(crate) fn retry_delay_seconds(attempt: u32, retry_after: Option<f64>, jitter_ms: u32) -> f64 {
    if let Some(retry_after) = retry_after {
        return retry_after;
    }
    RETRY_BASE_DELAY_SECONDS * f64::from(2_u32.pow(attempt.saturating_sub(1).min(30))) + f64::from(jitter_ms) / 1000.0
}

/// `max_attempts_for_status(status_code)`.
pub(crate) fn max_attempts_for_status(status: u16) -> u32 {
    match status {
        404 | 567 => 1,
        403 | 406 => LIMITED_RETRY_ATTEMPTS,
        408 | 429 => MAX_RETRIES,
        status if status >= 500 => MAX_RETRIES,
        _ => 1,
    }
}

/// `video_retry_cooldown_days(failed_count)`.
pub(crate) fn video_retry_cooldown_days(failed_count: i64) -> i64 {
    let index = usize::try_from(failed_count.clamp(1, 3) - 1).unwrap_or(0);
    VIDEO_RETRY_COOLDOWN_DAYS[index]
}

/// `is_nonblocking_failed_asset(asset)`: a failed video, or a GameKee CDN texture the CDN
/// does not have, does not fail the page.
pub(crate) fn is_nonblocking_failed_asset(asset: &Asset) -> bool {
    match asset.kind.as_str() {
        "video" => true,
        "live2d_texture" => {
            asset.url.starts_with("https://cdnimg-v2.gamekee.com/")
                && [404, 567, 400].iter().any(|status| asset.error.contains(&format!("HTTP {status}")))
        }
        _ => false,
    }
}

/// `_viewer_resource_payload(resource)`.
pub(crate) fn viewer_resource_payload(resource: &ViewerResource) -> Value {
    resource.to_value()
}
