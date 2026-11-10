//! NIKKE characters from GameKee (`src/web/nikke.py`).
//!
//! The job lists the characters GameKee's NIKKE wiki has (`/v1/entry/tj-list`), reads each
//! page's detail and content JSON, and archives every bound image, video and Live2D file
//! into a content-addressed blob store (`_blobs/sha256/..`) that each character directory
//! links into. `manifest.json` and `character.json` describe the page for the API reader
//! (`fav-api`'s `nikke` module); the layer metadata of stacked Live2D models lives in
//! [`crate::nikke_layer_metadata`].
//!
//! This file holds the page parsing and the pure helpers; [`crawl`] has the job itself.
//! The runtime layer capture (`nikke_runtime.py`, a headless browser) is WP17's: until then
//! [`runtime::RuntimeCapturer`]'s production implementation reports it unavailable, which the
//! scheduled job never asks for (DIVERGENCES.md D85).

use std::collections::HashMap;

use fav_core::filename::sanitize;
use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::{py_int_decimal, py_isdigit, py_strip};
use fav_core::{pycasefold, pypath, pyurl};
use fav_runtime::pystr::{repr_value, str_key, str_value};
use regex::Regex;

pub use crate::nikke_layer_metadata::{
    LAYER_CAPTURE_MATCH_METHOD, LAYER_MATCH_CONFIDENCE_VALUES, LIVE2D_LAYER_CAPTURE_FIELD, LIVE2D_LAYER_METADATA_FIELDS,
    copy_live2d_layer_metadata, layer_capture_manifest_summary, merge_layer_capture_files, merge_live2d_layer_captures,
    remove_live2d_layer_metadata, strip_layer_metadata_files, validate_live2d_layer_metadata,
};
use crate::nikke_layer_metadata::{json_dumps, normalize_layer_url, sha256_hex, str_or_empty, to_int, value_or_empty};
use crate::pydecode::PyError;

pub mod crawl;
pub mod layer_tool;
pub mod runtime;

pub use crawl::{Nikke, register};

pub const JOB_KEY: &str = "nikke";
pub const GAMEKEE_BASE_URL: &str = "https://www.gamekee.com";
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
pub const CDN_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36";
const MEDIA_CELL_TYPES: [&str; 3] = ["image", "video", "live2d"];
const IMAGE_LIST_FIELDS: [&str; 3] = ["icon", "icon1", "icon2"];
const LIVE2D_URL_FIELDS: [&str; 3] = ["atlas", "skel", "json"];
const SKIN_NAME_LABEL: &str = "时装名称";
const SKIN_SERIES_LABEL: &str = "时装系列";
const SKIN_OBTAIN_LABEL: &str = "获取方式";
const COLLECTION_TERMS: [&str; 2] = ["珍藏品", "收藏品"];
const FAVORITE_MODEL_MARKER: &str = "favorite_";

const MAX_RETRIES: u32 = 3;
const LIMITED_RETRY_ATTEMPTS: u32 = 2;
const RETRY_BASE_DELAY_SECONDS: f64 = 0.75;
const CIRCUIT_PAUSE_SECONDS: f64 = 30.0;

/// A Python `dict` keyed by `(url, kind)`: the page's assets in first-seen order.
#[derive(Debug, Clone, Default)]
pub struct Assets {
    items: Vec<Asset>,
    index: HashMap<(String, String), usize>,
}

impl Assets {
    pub fn get(&self, url: &str, kind: &str) -> Option<&Asset> {
        self.index.get(&(url.to_owned(), kind.to_owned())).map(|index| &self.items[*index])
    }

    pub fn get_mut(&mut self, url: &str, kind: &str) -> Option<&mut Asset> {
        self.index
            .get(&(url.to_owned(), kind.to_owned()))
            .map(|index| &mut self.items[*index])
    }

    pub fn iter(&self) -> impl Iterator<Item = &Asset> {
        self.items.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Asset> {
        self.items.iter_mut()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn items_mut(&mut self) -> &mut [Asset] {
        &mut self.items
    }

    fn entry(&mut self, url: String, kind: &str) -> &mut Asset {
        let key = (url, kind.to_owned());
        let index = if let Some(index) = self.index.get(&key) {
            *index
        } else {
            self.items.push(Asset::new(key.0.clone(), kind));
            self.index.insert(key, self.items.len() - 1);
            self.items.len() - 1
        };
        &mut self.items[index]
    }
}

/// One file a page refers to, with every place it appears (`contexts`).
#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub url: String,
    pub kind: String,
    pub local_path: String,
    pub content_type: String,
    pub size: i64,
    pub sha256: String,
    pub status: String,
    pub error: String,
    pub contexts: Vec<Dict>,
}

impl Asset {
    pub fn new(url: impl Into<String>, kind: &str) -> Self {
        Self {
            url: url.into(),
            kind: kind.to_owned(),
            local_path: String::new(),
            content_type: String::new(),
            size: 0,
            sha256: String::new(),
            status: "pending".to_owned(),
            error: String::new(),
            contexts: Vec::new(),
        }
    }
}

/// `SkinMeta`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkinMeta {
    pub skin_index: Option<i128>,
    pub skin_name: String,
    pub skin_title: String,
    pub skin_series: String,
    pub skin_obtain: String,
    pub is_collection_skin: bool,
}

/// `MediaRow`.
#[derive(Debug, Clone)]
pub struct MediaRow<'a> {
    pub section: &'static str,
    pub row_index: usize,
    pub label: String,
    pub cells: &'a [Value],
    pub skin_meta: SkinMeta,
}

fn dict_of(entries: Vec<(&str, Value)>) -> Dict {
    let mut dict = Dict::new();
    for (key, value) in entries {
        dict.insert(key, value);
    }
    dict
}

fn int(value: usize) -> Value {
    Value::Int(value as i128)
}

// ---------- ids, names, headers ----------

/// `parse_content_id`: a bare id, or a GameKee page or API URL.
pub fn parse_content_id(target: &str) -> Result<i128, PyError> {
    static PATTERNS: std::sync::LazyLock<[Regex; 3]> = std::sync::LazyLock::new(|| {
        [
            Regex::new(r"/tj/(\d+)\.html(?:[?#].*)?$").expect("valid pattern"),
            Regex::new(r"/(\d+)\.html(?:[?#].*)?$").expect("valid pattern"),
            Regex::new(r"content/detail/(\d+)(?:[?#].*)?$").expect("valid pattern"),
        ]
    });
    let raw = py_strip(target);
    if py_isdigit(raw) {
        return crate::pydecode::py_int(&Value::Str(raw.to_owned()));
    }
    for pattern in PATTERNS.iter() {
        if let Some(found) = pattern.captures(raw)
            && let Some(id) = py_int_decimal(&found[1])
        {
            return Ok(id);
        }
    }
    Err(PyError::new(
        "ValueError",
        format!("Cannot parse a GameKee content id from {}", fav_runtime::pystr::repr_str(target)),
    ))
}

/// `normalize_url`: protocol- and site-relative URLs made absolute on GameKee.
pub fn normalize_url(url: &str) -> String {
    normalize_layer_url(url)
}

/// `request_headers(content_id)`: the API headers, with the character page as referer.
pub fn request_headers(content_id: Option<i128>) -> Vec<(String, String)> {
    let referer = match content_id {
        Some(id) if id != 0 => format!("{GAMEKEE_BASE_URL}/nikke/tj/{id}.html"),
        _ => format!("{GAMEKEE_BASE_URL}/nikke/"),
    };
    pairs(&[
        ("Accept", "application/json, text/plain, */*"),
        ("Game-Alias", "nikke"),
        ("Lang", "zh-cn"),
        ("Referer", &referer),
        ("User-Agent", DEFAULT_USER_AGENT),
        ("X-Requested-With", "XMLHttpRequest"),
    ])
}

/// `cdn_request_headers()`: what a browser sends for the CDN's JSON.
pub fn cdn_request_headers() -> Vec<(String, String)> {
    pairs(&[
        ("Accept", "application/json, text/plain, */*"),
        ("Accept-Language", "en,zh;q=0.9,zh-CN;q=0.8"),
        ("Cache-Control", "no-cache"),
        ("Origin", GAMEKEE_BASE_URL),
        ("Pragma", "no-cache"),
        ("Priority", "u=1, i"),
        ("Referer", &format!("{GAMEKEE_BASE_URL}/")),
        (
            "Sec-CH-UA",
            "\"Chromium\";v=\"148\", \"Google Chrome\";v=\"148\", \"Not/A)Brand\";v=\"99\"",
        ),
        ("Sec-CH-UA-Mobile", "?0"),
        ("Sec-CH-UA-Platform", "\"macOS\""),
        ("Sec-Fetch-Dest", "empty"),
        ("Sec-Fetch-Mode", "cors"),
        ("Sec-Fetch-Site", "same-site"),
        ("User-Agent", CDN_USER_AGENT),
    ])
}

/// `asset_headers()`.
pub fn asset_headers() -> Vec<(String, String)> {
    pairs(&[
        ("Accept", "*/*"),
        ("Origin", GAMEKEE_BASE_URL),
        ("Referer", &format!("{GAMEKEE_BASE_URL}/")),
        ("User-Agent", DEFAULT_USER_AGENT),
    ])
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// `response_json` after the status check: an object whose `code` is 0.
pub fn api_payload(data: Value) -> Result<Dict, PyError> {
    let Value::Dict(data) = data else {
        return Err(PyError::new("TypeError", "GameKee returned a non-object JSON response"));
    };
    let code = data.get("code").cloned().unwrap_or(Value::Null);
    if !crate::nikke_layer_metadata::py_eq(&code, &Value::Int(0)) {
        let msg = data.get("msg").cloned().unwrap_or(Value::Null);
        return Err(PyError::new(
            "RuntimeError",
            format!("GameKee API returned code={}, msg={}", repr_value(&code), repr_value(&msg)),
        ));
    }
    Ok(data)
}

/// `filter_nikke_rows`: the `nikke` objects of the list rows that carry a content id.
pub fn filter_nikke_rows(rows: Option<&Value>) -> Vec<Dict> {
    let Some(Value::List(rows)) = rows else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|item| match item.get("nikke") {
            Some(Value::Dict(nikke)) if row_content_id(nikke).is_some() => Some(nikke.clone()),
            _ => None,
        })
        .collect()
}

/// `row_content_id`.
pub fn row_content_id(row: &Dict) -> Option<i128> {
    ["content_id", "contentId", "id"].iter().find_map(|key| to_int(row.get(key)))
}

fn first_stripped_str(data: &Dict, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match data.get(key) {
        Some(Value::Str(text)) if !py_strip(text).is_empty() => Some(py_strip(text).to_owned()),
        _ => None,
    })
}

/// `row_name`.
pub fn row_name(row: &Dict) -> String {
    first_stripped_str(row, &["title", "name", "entry_name", "entryName"]).unwrap_or_default()
}

/// `detail_name`: the detail's title, else the list row's, else the id.
pub fn detail_name(detail: &Dict, content_id: i128, row: Option<&Dict>) -> String {
    if let Some(name) = first_stripped_str(detail, &["title", "name", "entry_name", "entryName"]) {
        return name;
    }
    if let Some(row) = row.filter(|row| !row.is_empty()) {
        let name = row_name(row);
        if !name.is_empty() {
            return name;
        }
    }
    content_id.to_string()
}

/// `_coerce_json_object`: an object, or a string holding one; `{}` otherwise.
pub fn coerce_json_object(value: Option<&Value>) -> Dict {
    match value {
        Some(Value::Dict(dict)) => dict.clone(),
        Some(Value::Str(text)) if !py_strip(text).is_empty() => match fav_core::pyjson::loads(text) {
            Ok(Value::Dict(dict)) => dict,
            _ => Dict::new(),
        },
        _ => Dict::new(),
    }
}

/// `parse_content_json`.
pub fn parse_content_json(detail: &Dict) -> Dict {
    coerce_json_object(detail.get("content_json"))
}

/// `content_json_from_cdn_payload`.
pub fn content_json_from_cdn_payload(payload: &Dict) -> Dict {
    match payload.get("content") {
        Some(Value::Dict(content)) => return content.clone(),
        Some(content @ Value::Str(_)) => return coerce_json_object(Some(content)),
        _ => {}
    }
    if matches!(payload.get("baseData"), Some(Value::List(_))) || matches!(payload.get("styleData"), Some(Value::List(_))) {
        return payload.clone();
    }
    Dict::new()
}

/// `cdn_json_url`.
pub fn cdn_json_url(detail: &Dict, field: &str) -> String {
    match detail.get(field) {
        Some(Value::Str(text)) if !py_strip(text).is_empty() => normalize_url(text),
        _ => String::new(),
    }
}

/// The reverse of `entry_data_bind` (dynamic cell key to stable id).
pub type ReverseBind = HashMap<String, String>;

/// `reverse_bind_from_object`.
pub fn reverse_bind_from_object(entry_data_bind: Option<&Value>) -> ReverseBind {
    coerce_json_object(entry_data_bind)
        .iter()
        .map(|(stable_id, dynamic_key)| (str_value(dynamic_key), str_key(stable_id)))
        .collect()
}

/// `reverse_bind_from_cdn_payload`.
pub fn reverse_bind_from_cdn_payload(payload: &Dict) -> ReverseBind {
    let owned = Value::Dict(payload.clone());
    let value = if payload.contains_key("entry_data_bind") {
        payload.get("entry_data_bind")
    } else {
        Some(&owned)
    };
    reverse_bind_from_object(value)
}

/// `reverse_bind_map`.
pub fn reverse_bind_map(detail: &Dict) -> ReverseBind {
    reverse_bind_from_object(detail.get("entry_data_bind"))
}

fn bound<'a>(reverse_bind: &'a ReverseBind, key: &str) -> &'a str {
    reverse_bind.get(key).map_or("", String::as_str)
}

// ---------- page content ----------

/// `first_text`: the first non-blank text cell of a row.
pub fn row_first_text(row: &[Value]) -> String {
    for cell in row {
        let Value::Dict(cell) = cell else { continue };
        if !matches!(cell.get("type"), Some(Value::Str(kind)) if kind == "text") {
            continue;
        }
        if let Some(Value::Str(text)) = cell.get("value")
            && !py_strip(text).is_empty()
        {
            return py_strip(text).to_owned();
        }
    }
    String::new()
}

fn first_value_text(row: &[Value]) -> String {
    for cell in row.iter().skip(1) {
        if let Some(Value::Str(text)) = cell.as_dict().and_then(|cell| cell.get("value"))
            && !py_strip(text).is_empty()
        {
            return py_strip(text).to_owned();
        }
    }
    String::new()
}

fn row_value_by_label(rows: &[Value], label: &str) -> String {
    for row in rows {
        if let Value::List(row) = row
            && row_first_text(row) == label
        {
            return first_value_text(row);
        }
    }
    String::new()
}

fn has_favorite_model_marker(rows: &[Value]) -> bool {
    rows.iter().filter_map(Value::as_list).flatten().any(|cell| {
        let Some(cell) = cell.as_dict() else { return false };
        if !matches!(cell.get("type"), Some(Value::Str(kind)) if kind == "live2d") {
            return false;
        }
        let Some(Value::Dict(value)) = cell.get("value") else {
            return false;
        };
        LIVE2D_URL_FIELDS
            .iter()
            .chain(["image"].iter())
            .any(|field| matches!(value.get(field), Some(Value::Str(url)) if url.contains(FAVORITE_MODEL_MARKER)))
    })
}

/// `skin_metadata`.
pub fn skin_metadata(skin: &Dict, skin_index: usize) -> SkinMeta {
    let rows: &[Value] = skin.get("data").and_then(Value::as_list).unwrap_or(&[]);
    let skin_name = match skin.get("name") {
        Some(name) if name.is_truthy() => str_value(name),
        _ => format!("skin-{}", skin_index + 1),
    };
    let skin_title = row_value_by_label(rows, SKIN_NAME_LABEL);
    let skin_series = row_value_by_label(rows, SKIN_SERIES_LABEL);
    let skin_obtain = row_value_by_label(rows, SKIN_OBTAIN_LABEL);
    let searchable = format!("{skin_title}{skin_series}{skin_obtain}");
    let is_collection_skin = COLLECTION_TERMS.iter().any(|term| searchable.contains(term)) || has_favorite_model_marker(rows);
    SkinMeta {
        skin_index: Some(skin_index as i128),
        skin_name,
        skin_title,
        skin_series,
        skin_obtain,
        is_collection_skin,
    }
}

fn cell_summary(cell: &Dict, reverse_bind: &ReverseBind) -> Dict {
    let key = str_or_empty(cell, "key");
    let stable_id = bound(reverse_bind, &key).to_owned();
    let mut summary = dict_of(vec![
        ("key", Value::Str(key)),
        ("stable_id", Value::Str(stable_id.clone())),
        ("is_bound", Value::Bool(!stable_id.is_empty())),
        ("type", value_or_empty(cell, "type")),
        ("value", cell.get("value").cloned().unwrap_or(Value::Null)),
    ]);
    if cell.contains_key("isLimit") {
        summary.insert("is_limit", cell.get("isLimit").cloned().unwrap_or(Value::Null));
    }
    summary
}

fn summarize_rows(rows: &[Value], reverse_bind: &ReverseBind) -> Value {
    Value::List(
        rows.iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let Value::List(row) = row else { return None };
                let cells = row
                    .iter()
                    .filter_map(Value::as_dict)
                    .map(|cell| Value::Dict(cell_summary(cell, reverse_bind)))
                    .collect();
                Some(Value::Dict(dict_of(vec![
                    ("row_index", int(index)),
                    ("label", Value::Str(row_first_text(row))),
                    ("cells", Value::List(cells)),
                ])))
            })
            .collect(),
    )
}

/// `summarize_content`: the base rows and each skin's rows with bound stable ids.
pub fn summarize_content(content_json: &Dict, reverse_bind: &ReverseBind) -> Dict {
    let base_rows: &[Value] = content_json.get("baseData").and_then(Value::as_list).unwrap_or(&[]);
    let mut summary = dict_of(vec![("base_rows", summarize_rows(base_rows, reverse_bind))]);
    let Some(Value::List(style_rows)) = content_json.get("styleData") else {
        summary.insert("skins", Value::List(Vec::new()));
        return summary;
    };
    let mut skins = Vec::new();
    for (skin_index, skin) in style_rows.iter().enumerate() {
        let Value::Dict(skin) = skin else { continue };
        let rows: &[Value] = skin.get("data").and_then(Value::as_list).unwrap_or(&[]);
        let meta = skin_metadata(skin, skin_index);
        skins.push(Value::Dict(dict_of(vec![
            ("skin_index", int(skin_index)),
            ("name", Value::Str(meta.skin_name)),
            ("title", Value::Str(meta.skin_title)),
            ("series", Value::Str(meta.skin_series)),
            ("obtain", Value::Str(meta.skin_obtain)),
            ("is_collection_skin", Value::Bool(meta.is_collection_skin)),
            ("rows", summarize_rows(rows, reverse_bind)),
        ])));
    }
    summary.insert("skins", Value::List(skins));
    summary
}

/// `base_info_from_summary`: label to the bound, non-empty values of its rows.
pub fn base_info_from_summary(content_summary: &Dict) -> Dict {
    let mut base_info = Dict::new();
    let rows: &[Value] = content_summary.get("base_rows").and_then(Value::as_list).unwrap_or(&[]);
    for row in rows {
        let Value::Dict(row) = row else { continue };
        let (Some(Value::Str(label)), Some(Value::List(cells))) = (row.get("label"), row.get("cells")) else {
            continue;
        };
        if label.is_empty() {
            continue;
        }
        let values: Vec<Value> = cells
            .iter()
            .skip(1)
            .filter(|cell| {
                let Some(cell) = cell.as_dict() else { return false };
                cell.get("is_bound").is_some_and(Value::is_truthy)
                    && !matches!(cell.get("value"), None | Some(Value::Null))
                    && !matches!(cell.get("value"), Some(Value::Str(text)) if text.is_empty())
            })
            .cloned()
            .collect();
        if values.is_empty() {
            continue;
        }
        let entry = Value::Dict(dict_of(vec![
            ("row_index", row.get("row_index").cloned().unwrap_or(Value::Null)),
            ("values", Value::List(values)),
        ]));
        match base_info.get_mut(label) {
            Some(Value::List(entries)) => entries.push(entry),
            _ => base_info.insert(label.clone(), Value::List(vec![entry])),
        }
    }
    base_info
}

// ---------- resources ----------

/// `add_asset`: record one more place `url` appears as `kind`.
pub fn add_asset(assets: &mut Assets, url: &str, kind: &str, context: Dict) {
    let normalized = normalize_url(url);
    if normalized.is_empty() {
        return;
    }
    assets.entry(normalized, kind).contexts.push(context);
}

fn add_url_list(assets: &mut Assets, raw_urls: &str, kind: &str, context: &Dict) {
    for raw_url in raw_urls.split(',') {
        add_asset(assets, raw_url, kind, context.clone());
    }
}

fn media_context(row: &MediaRow<'_>, cell: &Dict, reverse_bind: &ReverseBind) -> Dict {
    let key = str_or_empty(cell, "key");
    let stable_id = bound(reverse_bind, &key).to_owned();
    let mut context = dict_of(vec![
        ("section", Value::from(row.section)),
        ("row_index", int(row.row_index)),
        ("label", Value::Str(row.label.clone())),
        ("key", Value::Str(key)),
        ("stable_id", Value::Str(stable_id)),
        ("cell_type", value_or_empty(cell, "type")),
    ]);
    if let Some(skin_index) = row.skin_meta.skin_index {
        let meta = &row.skin_meta;
        context.insert("skin_index", Value::Int(skin_index));
        context.insert("skin_name", Value::Str(meta.skin_name.clone()));
        context.insert("skin_title", Value::Str(meta.skin_title.clone()));
        context.insert("skin_series", Value::Str(meta.skin_series.clone()));
        context.insert("skin_obtain", Value::Str(meta.skin_obtain.clone()));
        context.insert("is_collection_skin", Value::Bool(meta.is_collection_skin));
    }
    context
}

/// `iter_media_rows`: the base rows, then every skin's rows.
pub fn iter_media_rows(content_json: &Dict) -> Vec<MediaRow<'_>> {
    let mut rows = Vec::new();
    if let Some(Value::List(base)) = content_json.get("baseData") {
        for (row_index, row) in base.iter().enumerate() {
            if let Value::List(cells) = row {
                rows.push(MediaRow {
                    section: "base",
                    row_index,
                    label: row_first_text(cells),
                    cells,
                    skin_meta: SkinMeta::default(),
                });
            }
        }
    }
    if let Some(Value::List(skins)) = content_json.get("styleData") {
        for (skin_index, skin) in skins.iter().enumerate() {
            let Value::Dict(skin) = skin else { continue };
            let meta = skin_metadata(skin, skin_index);
            let Some(Value::List(skin_rows)) = skin.get("data") else { continue };
            for (row_index, row) in skin_rows.iter().enumerate() {
                if let Value::List(cells) = row {
                    rows.push(MediaRow {
                        section: "style",
                        row_index,
                        label: row_first_text(cells),
                        cells,
                        skin_meta: meta.clone(),
                    });
                }
            }
        }
    }
    rows
}

fn context_or_empty(context: &Dict, key: &str) -> Value {
    value_or_empty(context, key)
}

fn with_entries(context: &Dict, entries: Vec<(&str, Value)>) -> Dict {
    let mut out = context.clone();
    for (key, value) in entries {
        out.insert(key, value);
    }
    out
}

/// `extract_live2d_model`: the model entry of a Live2D cell, and its files as assets.
pub fn extract_live2d_model(assets: &mut Assets, context: &Dict, value: &Dict) -> Dict {
    let live2d_key = str_or_empty(value, "live2dKey");
    let or_empty_dict = |key: &str| match value.get(key) {
        Some(found) if found.is_truthy() => found.clone(),
        _ => Value::Dict(Dict::new()),
    };
    let mut model = dict_of(vec![
        ("label", context_or_empty(context, "label")),
        ("section", context_or_empty(context, "section")),
        ("row_index", context.get("row_index").cloned().unwrap_or(Value::Null)),
        ("skin_index", context.get("skin_index").cloned().unwrap_or(Value::Null)),
        ("skin_name", context_or_empty(context, "skin_name")),
        ("skin_title", context_or_empty(context, "skin_title")),
        ("skin_series", context_or_empty(context, "skin_series")),
        ("skin_obtain", context_or_empty(context, "skin_obtain")),
        (
            "is_collection_skin",
            Value::Bool(context.get("is_collection_skin").is_some_and(Value::is_truthy)),
        ),
        ("key", context_or_empty(context, "key")),
        ("stable_id", context_or_empty(context, "stable_id")),
        ("live2d_key", Value::Str(live2d_key.clone())),
        ("animation", value_or_empty(value, "animation")),
        ("skin", value_or_empty(value, "skin")),
        ("limit_age", Value::Bool(value.get("limitAge").is_some_and(Value::is_truthy))),
        ("position", or_empty_dict("position")),
        ("bg_position", or_empty_dict("bgPosition")),
        ("urls", Value::Dict(Dict::new())),
    ]);
    copy_live2d_layer_metadata(&mut model, value);

    let mut urls = Dict::new();
    for field in LIVE2D_URL_FIELDS {
        if let Some(Value::Str(raw_url)) = value.get(field)
            && !py_strip(raw_url).is_empty()
        {
            let normalized = normalize_url(raw_url);
            urls.insert(field, Value::Str(normalized.clone()));
            let asset_context = with_entries(
                context,
                vec![("live2d_key", Value::Str(live2d_key.clone())), ("live2d_field", Value::from(field))],
            );
            add_asset(assets, &normalized, &format!("live2d_{field}"), asset_context);
        }
    }
    if let Some(Value::Str(images)) = value.get("image")
        && !py_strip(images).is_empty()
    {
        let image_urls: Vec<String> = images
            .split(',')
            .filter(|raw| !py_strip(raw).is_empty())
            .map(normalize_url)
            .collect();
        for image_url in &image_urls {
            let asset_context = with_entries(
                context,
                vec![
                    ("live2d_key", Value::Str(live2d_key.clone())),
                    ("live2d_field", Value::from("image")),
                ],
            );
            add_asset(assets, image_url, "live2d_texture", asset_context);
        }
        urls.insert("image", Value::List(image_urls.into_iter().map(Value::Str).collect()));
    }
    model.insert("urls", Value::Dict(urls));
    model
}

fn extract_media_row(row: &MediaRow<'_>, assets: &mut Assets, live2d_models: &mut Vec<Dict>, reverse_bind: &ReverseBind) {
    for cell in row.cells {
        let Value::Dict(cell) = cell else { continue };
        let Some(Value::Str(kind)) = cell.get("type") else { continue };
        if !MEDIA_CELL_TYPES.contains(&kind.as_str()) {
            continue;
        }
        if bound(reverse_bind, &str_or_empty(cell, "key")).is_empty() {
            continue;
        }
        let context = media_context(row, cell, reverse_bind);
        match cell.get("value") {
            Some(Value::Dict(value)) if kind == "live2d" => live2d_models.push(extract_live2d_model(assets, &context, value)),
            Some(Value::Str(value)) => add_url_list(assets, value, kind, &context),
            _ => {}
        }
    }
}

fn extract_tj_list_assets(assets: &mut Assets, tj_list_row: Option<&Dict>) {
    let Some(row) = tj_list_row.filter(|row| !row.is_empty()) else {
        return;
    };
    for field in IMAGE_LIST_FIELDS {
        if let Some(Value::Str(value)) = row.get(field)
            && !py_strip(value).is_empty()
        {
            let context = dict_of(vec![
                ("section", Value::from("tj_list")),
                ("field", Value::from(field)),
                ("label", Value::from(field)),
            ]);
            add_url_list(assets, value, "image", &context);
        }
    }
}

/// `extract_resources`: every bound media cell's files, and the Live2D models.
pub fn extract_resources(content_json: &Dict, tj_list_row: Option<&Dict>, reverse_bind: &ReverseBind) -> (Assets, Vec<Dict>) {
    let mut assets = Assets::default();
    let mut live2d_models = Vec::new();
    for row in iter_media_rows(content_json) {
        extract_media_row(&row, &mut assets, &mut live2d_models, reverse_bind);
    }
    extract_tj_list_assets(&mut assets, tj_list_row);
    (assets, live2d_models)
}

/// Python's `str.splitlines()` boundaries.
fn py_splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        let is_break = matches!(
            ch,
            '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        );
        if !is_break {
            continue;
        }
        lines.push(&text[start..index]);
        let mut end = index + ch.len_utf8();
        if ch == '\r'
            && let Some((next, '\n')) = chars.peek().copied()
        {
            chars.next();
            end = next + 1;
        }
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// `parse_spine_atlas_textures`: the page (texture) lines of a Spine atlas, without repeats.
pub fn parse_spine_atlas_textures(text: &str) -> Vec<String> {
    let mut textures = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in py_splitlines(text) {
        let value = py_strip(line).trim_start_matches('\u{feff}');
        if value.is_empty() || value.contains(':') || !is_texture_name(value) {
            continue;
        }
        if seen.insert(pycasefold::casefold(value)) {
            textures.push(value.to_owned());
        }
    }
    textures
}

/// `re.match(r'^[^\s:]+\.(?:png|webp|jpe?g)$', value, re.IGNORECASE)`.
fn is_texture_name(value: &str) -> bool {
    if value.chars().any(|ch| ch == ':' || fav_core::pytext::is_py_space(ch)) {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    [".png", ".webp", ".jpg", ".jpeg"].iter().any(|extension| {
        lower.len() > extension.len() && lower.ends_with(extension) && value.is_char_boundary(value.len() - extension.len())
    })
}

/// `extension_for_kind`.
pub fn extension_for_kind(kind: &str) -> &'static str {
    match kind {
        "video" => ".mp4",
        "live2d_atlas" => ".atlas",
        "live2d_skel" => ".skel",
        "live2d_json" => ".json",
        _ => ".bin",
    }
}

fn url_error(error: &pyurl::UrlError) -> PyError {
    PyError::new("ValueError", error.to_string())
}

/// `original_filename_from_url`: the URL's file name, unquoted and sanitized.
pub fn original_filename_from_url(url: &str, kind: &str) -> Result<String, PyError> {
    let split = pyurl::urlsplit(url, "").map_err(|error| url_error(&error))?;
    let raw_name = pyurl::unquote(pypath::name(&split.path));
    let fallback = format!("{kind}{}", extension_for_kind(kind));
    if raw_name.is_empty() {
        return Ok(fallback);
    }
    let sanitized = sanitize(&raw_name, 180);
    Ok(if sanitized.is_empty() { fallback } else { sanitized })
}

fn safe_suffix(filename: &str, kind: &str) -> String {
    let suffix = pypath::suffix(filename);
    let mut suffix = if suffix.is_empty() {
        extension_for_kind(kind).to_owned()
    } else {
        suffix.to_owned()
    };
    if !suffix.starts_with('.') {
        suffix = format!(".{suffix}");
    }
    let sanitized = sanitize(&suffix, 24);
    if sanitized.starts_with('.') {
        return sanitized;
    }
    format!(".{}", if sanitized.is_empty() { "bin" } else { &sanitized })
}

fn context_source_id(context: &Dict) -> String {
    for key in ["stable_id", "field", "live2d_field", "key", "label"] {
        if let Some(Value::Str(text)) = context.get(key)
            && !py_strip(text).is_empty()
        {
            return py_strip(text).to_owned();
        }
    }
    match context.get("row_index") {
        None | Some(Value::Null) => "asset".to_owned(),
        Some(row_index) => format!("row-{}", str_value(row_index)),
    }
}

/// `filename_from_url`: Live2D files keep their name (the atlas names its textures);
/// others get `<content_id>_<kind>_<source>_<stem><suffix>`.
pub fn filename_from_url(url: &str, kind: &str, contexts: &[Dict], content_id: i128) -> Result<String, PyError> {
    let original = original_filename_from_url(url, kind)?;
    if kind.starts_with("live2d_") {
        return Ok(original);
    }
    let empty = Dict::new();
    let context = contexts.first().unwrap_or(&empty);
    let suffix = safe_suffix(&original, kind);
    let stem = pypath::stem(&original);
    let original_stem = if stem.is_empty() { kind } else { stem };
    let source_id = context_source_id(context);
    let mut name = sanitize(&format!("{content_id}_{kind}_{source_id}_{original_stem}"), 180);
    if name.is_empty() {
        name = format!("{content_id}_{kind}");
    }
    Ok(format!("{name}{suffix}"))
}

/// `asset_subdir`.
pub fn asset_subdir(asset: &Asset) -> String {
    if asset.kind.starts_with("live2d_") {
        let model_key = asset
            .contexts
            .iter()
            .find_map(|context| {
                ["live2d_key", "stable_id", "key"]
                    .iter()
                    .find_map(|key| context.get(key).filter(|value| value.is_truthy()))
            })
            .map_or_else(|| "unknown".to_owned(), str_value);
        let name = sanitize(&model_key, 80);
        return format!("assets/live2d/{}", if name.is_empty() { "unknown" } else { &name });
    }
    if asset.kind == "video" {
        return "assets/videos".to_owned();
    }
    "assets/images".to_owned()
}

/// `PurePosixPath.with_stem`.
fn with_stem(path: &str, stem: &str) -> String {
    let name = pypath::name(path);
    let parent = &path[..path.len() - name.len()];
    format!("{parent}{stem}{}", pypath::suffix(name))
}

/// `assign_asset_paths`: each asset's path under the character directory, made unique by
/// a short hash of the URL where two assets would share one.
pub fn assign_asset_paths(assets: &mut Assets, content_id: i128) -> Result<(), PyError> {
    let mut used: HashMap<String, String> = HashMap::new();
    for asset in assets.items_mut() {
        let subdir = asset_subdir(asset);
        let filename = filename_from_url(&asset.url, &asset.kind, &asset.contexts, content_id)?;
        let mut relative = pypath::join(&subdir, &filename);
        if used.get(&relative).is_some_and(|url| *url != asset.url) {
            let digest = &sha256_hex(asset.url.as_bytes())[..8];
            if asset.kind.starts_with("live2d_") {
                relative = pypath::join(&format!("{subdir}-{digest}"), &filename);
            } else {
                relative = with_stem(&relative, &format!("{}-{digest}", pypath::stem(&relative)));
            }
        }
        while used.get(&relative).is_some_and(|url| *url != asset.url) {
            let digest = sha256_hex(format!("{}:{}", asset.url, used.len()).as_bytes());
            relative = with_stem(&relative, &format!("{}-{}", pypath::stem(&relative), &digest[..8]));
        }
        used.insert(relative.clone(), asset.url.clone());
        asset.local_path = relative;
    }
    Ok(())
}

/// `output_root`'s directory name: `<content_id> - <title>`.
pub fn output_dir_name(detail: &Dict, content_id: i128, row: Option<&Dict>) -> String {
    let title = detail_name(detail, content_id, row);
    let safe = sanitize(&title, 120);
    format!("{content_id} - {}", if safe.is_empty() { content_id.to_string() } else { safe })
}

/// `asset_counts`.
pub fn asset_counts(assets: &Assets) -> Dict {
    let mut counts = Dict::new();
    for asset in assets.iter() {
        let current = match counts.get(&asset.kind) {
            Some(Value::Int(count)) => *count,
            _ => 0,
        };
        counts.insert(asset.kind.clone(), Value::Int(current + 1));
    }
    counts
}

/// `relative_manifest_assets`.
pub fn relative_manifest_assets(assets: &Assets) -> Value {
    Value::List(
        assets
            .iter()
            .map(|asset| {
                Value::Dict(dict_of(vec![
                    ("url", Value::Str(asset.url.clone())),
                    ("kind", Value::Str(asset.kind.clone())),
                    ("local_path", Value::Str(asset.local_path.clone())),
                    ("sha256", Value::Str(asset.sha256.clone())),
                    ("content_type", Value::Str(asset.content_type.clone())),
                    ("size", Value::Int(i128::from(asset.size))),
                    ("status", Value::Str(asset.status.clone())),
                    ("error", Value::Str(asset.error.clone())),
                    ("contexts", Value::List(asset.contexts.iter().cloned().map(Value::Dict).collect())),
                ]))
            })
            .collect(),
    )
}

/// `context_hash`.
pub fn context_hash(context: &Dict) -> String {
    sha256_hex(json_dumps(&Value::Dict(context.clone())).as_bytes())
}

/// `content_hash`: what the page's detail, content and list row hash to.
pub fn content_hash(detail_response: &Dict, content_json: &Dict, tj_list_row: Option<&Dict>) -> String {
    let payload = dict_of(vec![
        ("detail_response", Value::Dict(detail_response.clone())),
        ("content_json", Value::Dict(content_json.clone())),
        ("tj_list_row", tj_list_row.map_or(Value::Null, |row| Value::Dict(row.clone()))),
    ]);
    sha256_hex(json_dumps(&Value::Dict(payload)).as_bytes())
}

/// `retry_delay_seconds`: `Retry-After` (at most 30 s), else 0.75 s doubling plus jitter.
pub fn retry_delay_seconds(attempt: u32, retry_after: Option<&str>, jitter: f64) -> Result<f64, PyError> {
    let retry_after =
        fav_net::retry::retry_after_seconds(retry_after, chrono::Utc::now()).map_err(|message| PyError::new("ValueError", message))?;
    if let Some(seconds) = retry_after {
        return Ok(seconds.min(CIRCUIT_PAUSE_SECONDS));
    }
    Ok(RETRY_BASE_DELAY_SECONDS * f64::from(2_u32.pow(attempt.saturating_sub(1))) + jitter)
}

/// `max_attempts_for_status`.
pub fn max_attempts_for_status(status: u16) -> u32 {
    match status {
        404 | 567 => 1,
        403 | 406 => LIMITED_RETRY_ATTEMPTS,
        408 | 429 => MAX_RETRIES,
        status if status >= 500 => MAX_RETRIES,
        _ => 1,
    }
}

/// The statuses that count towards the throttling pause.
pub(crate) fn is_limited_status(status: u16) -> bool {
    matches!(status, 403 | 406 | 429 | 503)
}

fn bytes_lstrip(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// `is_cdn_rejection_body`: an HTML page or an access-denied text instead of the file.
pub fn is_cdn_rejection_body(content_type: &str, body_prefix: &[u8]) -> bool {
    if content_type.to_lowercase().contains("text/html") {
        return true;
    }
    let stripped = bytes_lstrip(body_prefix).to_ascii_lowercase();
    if stripped.starts_with(b"<!doctype html") || stripped.starts_with(b"<html") {
        return true;
    }
    [&b"accessdenied"[..], b"access denied", b"forbidden", b"<error>", b"error code"]
        .iter()
        .any(|signature| contains_bytes(&stripped, signature))
}

/// `bytes.decode('utf-8', errors='ignore')`.
fn decode_ignore(bytes: &[u8]) -> String {
    bytes.utf8_chunks().map(|chunk| chunk.valid()).collect()
}

/// `validate_asset_response`: why a 2xx answer is not the file, or `""`.
pub fn validate_asset_response(kind: &str, content_type: &str, body_prefix: &[u8], size: u64) -> &'static str {
    if size == 0 {
        return "empty response body";
    }
    if is_cdn_rejection_body(content_type, body_prefix) {
        return "cdn rejection response";
    }
    let stripped = bytes_lstrip(body_prefix);
    if stripped.starts_with(b"{")
        && kind != "live2d_json"
        && let Ok(Value::Dict(parsed)) = fav_core::pyjson::loads(&decode_ignore(stripped))
        && ["code", "msg", "message", "error"].iter().any(|key| parsed.contains_key(key))
    {
        return "json error response";
    }
    ""
}

#[cfg(test)]
mod tests;
