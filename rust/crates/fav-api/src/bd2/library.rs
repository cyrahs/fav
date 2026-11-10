//! The BD2 character library the API reads: the crawler's manifests, summarised once per
//! change of the manifests and cached on disk.
//!
//! Python origin: `src/api/bd2.py` (`BD2Library`). Values come straight from the JSON the
//! crawler wrote, so every field is read the way Python reads it: `_clean_text` is
//! `str(value)` (a number becomes its digits, a list its `repr`), `_to_int` takes ints and
//! digit strings only, and a digit string `int()` cannot read (`'²'`) fails the request as
//! Python's `ValueError` does. The asset route `get_asset_file` serves is not in the API
//! (the static files are served beside it), so it is not ported.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fav_core::pycasefold::casefold;
use fav_core::pyjson::{self, Dict, Value};
use fav_core::pytext::{is_py_decimal, py_decimal_value, py_int_decimal, py_isdigit, py_strip};
use fav_core::validate::PyPath;
use fav_runtime::pystr::{repr_str, str_value};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use crate::mcp::isoformat::{Parsed, fromisoformat};
use crate::spine::spine_version_from_file;
use crate::summary_cache::{Signature, SummaryCacheData, read_summary_cache, write_summary_cache};

const BD2_STATIC_PREFIX: &str = "/static/bd2";

/// `urllib.parse.quote(text, safe='')`: letters, digits and `_.-~` stay.
const QUOTE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'-').remove(b'~');
/// `quote(text)` with its default `safe='/'`.
const QUOTE_PATH: &AsciiSet = &QUOTE.remove(b'/');

pub(crate) fn log() -> fav_core::logging::Logger {
    fav_core::logging::get("fav-api.bd2")
}

/// Why the library could not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LibraryError {
    /// `BD2CharacterNotFoundError`.
    NotFound,
    /// Any other exception, as `ClassName: message`.
    Failed(String),
}

type Result<T> = std::result::Result<T, LibraryError>;

/// `str(OSError)` with its file name: `[Errno 2] No such file or directory: '/x'`.
pub(crate) fn os_error_text(error: &std::io::Error, path: &Path) -> String {
    match error.raw_os_error() {
        Some(code) => {
            let text = error.to_string();
            let reason = text.split(" (os error").next().unwrap_or(&text).to_owned();
            format!("[Errno {code}] {reason}: {}", repr_str(&path.to_string_lossy()))
        }
        None => error.to_string(),
    }
}

/// `_read_json_object(path)`: the error is the exception's text.
fn read_json_object(path: &Path) -> std::result::Result<Dict, String> {
    let bytes = std::fs::read(path).map_err(|error| os_error_text(&error, path))?;
    let text = String::from_utf8(bytes).map_err(|error| {
        let at = error.utf8_error().valid_up_to();
        format!("'utf-8' codec can't decode byte at position {at}: invalid utf-8")
    })?;
    match pyjson::loads(&text) {
        Ok(Value::Dict(dict)) => Ok(dict),
        Ok(_) => Err(format!("JSON document is not an object: {}", path.display())),
        Err(error) => Err(error.python_message(&text)),
    }
}

// ---------- Python's conversions ----------

/// `_to_int(value)`: an int (not a bool), or a string of digits after `strip()`.
pub(crate) fn to_int(value: Option<&Value>) -> Result<Option<i128>> {
    match value {
        Some(Value::Int(int)) => Ok(Some(*int)),
        Some(Value::Str(text)) => {
            let stripped = py_strip(text);
            if !py_isdigit(stripped) {
                return Ok(None);
            }
            py_int_decimal(stripped).map(Some).ok_or_else(|| {
                LibraryError::Failed(format!(
                    "ValueError: invalid literal for int() with base 10: {}",
                    repr_str(stripped)
                ))
            })
        }
        _ => Ok(None),
    }
}

/// `_clean_text(value)`: `str(value)` with Windows line ends folded, stripped.
pub(crate) fn clean_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(value) => py_strip(&str_value(value).replace("\r\n", "\n")).to_owned(),
    }
}

/// `float(text)` for a string `isdigit()` accepted: decimal digits of any script read as
/// their values; other digits (`'²'`) are Python's `ValueError`.
fn float_of_digits(text: &str) -> Result<f64> {
    let mut ascii = String::with_capacity(text.len());
    for ch in text.chars() {
        match py_decimal_value(ch) {
            Some(digit) if is_py_decimal(ch) => ascii.push(char::from_digit(digit, 10).unwrap_or('0')),
            _ => {
                return Err(LibraryError::Failed(format!(
                    "ValueError: could not convert string to float: {}",
                    repr_str(text)
                )));
            }
        }
    }
    ascii
        .parse()
        .map_err(|_| LibraryError::Failed(format!("ValueError: could not convert string to float: {}", repr_str(text))))
}

/// `datetime.timestamp()`, a naive value read as UTC.
#[allow(clippy::cast_precision_loss)] // Python divides the microseconds the same way.
pub(crate) fn timestamp(parsed: &Parsed) -> f64 {
    let utc = match parsed {
        Parsed::Aware(value) => value.with_timezone(&chrono::Utc),
        Parsed::Naive(value) => value.and_utc(),
    };
    let micros = i128::from(utc.timestamp()) * 1_000_000 + i128::from(utc.timestamp_subsec_micros());
    micros as f64 / 1_000_000.0
}

/// A number as Python's `float(value)`.
#[allow(clippy::cast_precision_loss)] // `float(int)` rounds the same way.
fn number_float(value: &Value) -> Option<f64> {
    match value {
        Value::Int(int) => Some(*int as f64),
        Value::Float(float) => Some(*float),
        _ => None,
    }
}

/// `_sort_timestamp(value)` of `src/api/bd2.py`: numbers, digit strings and ISO dates;
/// anything else sorts as `0.0`.
pub(crate) fn sort_timestamp(value: Option<&Value>) -> Result<f64> {
    let Some(value) = value else {
        return Ok(0.0);
    };
    if let Some(number) = number_float(value) {
        return Ok(number);
    }
    let Value::Str(text) = value else {
        return Ok(0.0);
    };
    let text = py_strip(text);
    if py_isdigit(text) {
        return float_of_digits(text);
    }
    if text.is_empty() {
        return Ok(0.0);
    }
    let normalized = match text.strip_suffix('Z') {
        Some(rest) => format!("{rest}+00:00"),
        None => text.to_owned(),
    };
    Ok(fromisoformat(&normalized).map_or(0.0, |parsed| timestamp(&parsed)))
}

/// `_sort_timestamp(value)` of `src/api/routes.py`: every `Z` becomes `+00:00`.
pub(crate) fn route_sort_timestamp(value: &Value) -> Result<f64> {
    if let Some(number) = number_float(value) {
        return Ok(number);
    }
    let Value::Str(text) = value else {
        return Ok(0.0);
    };
    let text = py_strip(text);
    if text.is_empty() {
        return Ok(0.0);
    }
    if py_isdigit(text) {
        return float_of_digits(text);
    }
    Ok(fromisoformat(&text.replace('Z', "+00:00")).map_or(0.0, |parsed| timestamp(&parsed)))
}

/// Python's `char.lower()`, one character to its full lowercase.
fn lower(ch: char) -> impl Iterator<Item = char> {
    ch.to_lowercase()
}

/// `_profile_key(label)`: letters and digits lowercased, everything else one `_`.
pub(crate) fn profile_key(label: &str) -> String {
    let mut key = String::new();
    for ch in py_strip(label).chars() {
        if ch.is_alphanumeric() {
            key.extend(lower(ch));
        } else {
            key.push('_');
        }
    }
    let joined = key.split('_').filter(|part| !part.is_empty()).collect::<Vec<_>>().join("_");
    if joined.is_empty() { "field".to_owned() } else { joined }
}

fn dict_items(value: Option<&Value>) -> Vec<&Dict> {
    match value {
        Some(Value::List(items)) => items.iter().filter_map(Value::as_dict).collect(),
        _ => Vec::new(),
    }
}

/// `for item in value` over what a manifest holds where a list belongs: a string and a
/// dictionary yield no dictionaries, a number or `None` is Python's `TypeError`.
fn iterate_dicts(value: &Value) -> Result<Vec<&Dict>> {
    match value {
        Value::List(items) => Ok(items.iter().filter_map(Value::as_dict).collect()),
        Value::Str(_) | Value::Dict(_) => Ok(Vec::new()),
        other => Err(LibraryError::Failed(format!(
            "TypeError: '{}' object is not iterable",
            match other {
                Value::Null => "NoneType",
                Value::Bool(_) => "bool",
                Value::Int(_) => "int",
                _ => "float",
            }
        ))),
    }
}

fn text_eq(value: Option<&Value>, expected: &str) -> bool {
    matches!(value, Some(Value::Str(text)) if text == expected)
}

// ---------- records ----------

/// `_BD2RecordEntry`.
#[derive(Debug, Clone)]
struct RecordEntry {
    content_id: i128,
    root: PathBuf,
    manifest_path: PathBuf,
    directory_name: String,
}

/// `_BD2Record`: an entry with its manifest read.
#[derive(Debug, Clone)]
pub(crate) struct Record {
    pub(crate) content_id: i128,
    root: PathBuf,
    directory_name: String,
    manifest: Dict,
}

fn directory_name(manifest_path: &Path) -> String {
    manifest_path
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `_manifest_content_id(manifest_path, manifest)`: the manifest's id, else the number
/// before ` - ` in the directory name.
fn manifest_content_id(manifest_path: &Path, manifest: &Dict) -> Result<Option<i128>> {
    if let Some(content_id) = to_int(manifest.get("content_id"))? {
        return Ok(Some(content_id));
    }
    let name = directory_name(manifest_path);
    let prefix = py_strip(name.split(" - ").next().unwrap_or_default()).to_owned();
    to_int(Some(&Value::Str(prefix)))
}

fn freshness(manifest: &Dict, mtime_ns: i128) -> Result<(f64, f64, i128)> {
    Ok((
        sort_timestamp(manifest.get("fetched_at"))?,
        sort_timestamp(manifest.get("updated_at"))?,
        mtime_ns,
    ))
}

/// Python's `<=` on `(float, float, int)` tuples.
#[allow(clippy::float_cmp)] // Tuple comparison looks for the first unequal item exactly.
fn freshness_le(left: (f64, f64, i128), right: (f64, f64, i128)) -> bool {
    if left.0 != right.0 {
        return left.0 <= right.0;
    }
    if left.1 != right.1 {
        return left.1 <= right.1;
    }
    left.2 <= right.2
}

/// What `_assets_for_context` filters on; `None` does not filter.
#[derive(Debug, Clone, Copy, Default)]
struct Query<'a> {
    kind: Option<&'a str>,
    label: Option<&'a str>,
    field: Option<&'a str>,
    section: Option<&'a str>,
    style_index: Option<i128>,
    column_name: Option<&'a str>,
    column_category: Option<&'a str>,
    live2d_key: Option<&'a str>,
    live2d_field: Option<&'a str>,
}

impl<'a> Query<'a> {
    /// `_pick_asset`'s default kind.
    fn image() -> Self {
        Self {
            kind: Some("image"),
            ..Self::default()
        }
    }

    /// `_context_matches(context, ...)`.
    fn matches(&self, context: &Dict) -> Result<bool> {
        let text = |expected: Option<&'a str>, key: &str| expected.is_none_or(|expected| clean_text(context.get(key)) == expected);
        if !(text(self.label, "label") && text(self.field, "field") && text(self.section, "section")) {
            return Ok(false);
        }
        if let Some(style_index) = self.style_index
            && to_int(context.get("style_index"))? != Some(style_index)
        {
            return Ok(false);
        }
        Ok(text(self.column_name, "column_name")
            && text(self.column_category, "column_category")
            && text(self.live2d_key, "live2d_key")
            && text(self.live2d_field, "live2d_field"))
    }
}

/// `_normalize_asset_path(asset_path)`: a relative path under `assets/`, normalised, or
/// `None` for anything else (`BD2AssetNotFoundError`).
fn normalize_asset_path(asset_path: &str) -> Option<String> {
    let decoded = percent_decode_str(asset_path).decode_utf8_lossy().replace('\\', "/");
    let raw = py_strip(&decoded);
    if raw.is_empty() || raw.starts_with('/') {
        return None;
    }
    let path = PyPath::new(raw);
    let parts: Vec<&str> = path.as_str().split('/').collect();
    if parts.first() != Some(&"assets") || parts.iter().any(|part| matches!(*part, "" | "." | "..")) {
        return None;
    }
    Some(path.as_str().to_owned())
}

/// `_asset_static_url(directory_name, asset_path, sha256=...)`.
fn asset_static_url(directory_name: &str, asset_path: &str, sha256: &str) -> String {
    let url = format!(
        "{BD2_STATIC_PREFIX}/{}/{}",
        utf8_percent_encode(directory_name, QUOTE),
        utf8_percent_encode(asset_path, QUOTE_PATH)
    );
    if sha256.is_empty() {
        url
    } else {
        format!("{url}?v={}", utf8_percent_encode(sha256, QUOTE_PATH))
    }
}

fn str_value_of(text: String) -> Value {
    Value::Str(text)
}

fn optional_int(value: Option<i128>) -> Value {
    value.map_or(Value::Null, Value::Int)
}

impl Record {
    fn assets(&self) -> Vec<&Dict> {
        dict_items(self.manifest.get("assets"))
    }

    /// `_asset_ref(record, asset, context)`.
    fn asset_ref(&self, asset: &Dict, context: Option<&Dict>) -> Result<Value> {
        let local_path = clean_text(asset.get("local_path"));
        let empty = Dict::new();
        let context = context.unwrap_or(&empty);
        let available =
            !local_path.is_empty() && normalize_asset_path(&local_path).is_some_and(|normalized| self.root.join(normalized).is_file());
        let sha256 = clean_text(asset.get("sha256"));
        let url = if local_path.is_empty() {
            String::new()
        } else {
            asset_static_url(&self.directory_name, &local_path, &sha256)
        };
        let field = match clean_text(context.get("field")) {
            field if field.is_empty() => clean_text(context.get("live2d_field")),
            field => field,
        };
        Ok(crate::json::dict([
            ("kind", str_value_of(clean_text(asset.get("kind")))),
            ("path", Value::Str(local_path)),
            ("url", Value::Str(url)),
            ("content_type", str_value_of(clean_text(asset.get("content_type")))),
            (
                "size",
                Value::Int(to_int(asset.get("size"))?.filter(|size| *size != 0).unwrap_or(0)),
            ),
            ("sha256", Value::Str(sha256)),
            ("status", str_value_of(clean_text(asset.get("status")))),
            ("label", str_value_of(clean_text(context.get("label")))),
            ("field", Value::Str(field)),
            ("style_index", optional_int(to_int(context.get("style_index"))?)),
            ("style_name", str_value_of(clean_text(context.get("style_name")))),
            ("costume_title", str_value_of(clean_text(context.get("costume_title")))),
            ("costume_category", str_value_of(clean_text(context.get("costume_category")))),
            ("column_role", str_value_of(clean_text(context.get("column_role")))),
            ("live2d_key", str_value_of(clean_text(context.get("live2d_key")))),
            ("available", Value::Bool(available)),
            (
                "contexts",
                Value::List(dict_items(asset.get("contexts")).into_iter().cloned().map(Value::Dict).collect()),
            ),
        ]))
    }

    /// `_assets_for_context(record, ...)`: each asset path once, by style, row, label, path.
    fn assets_for_context(&self, query: Query<'_>) -> Result<Vec<Value>> {
        let mut found: Vec<((i128, i128, String, String), Value)> = Vec::new();
        let mut seen_paths: HashSet<String> = HashSet::new();
        for asset in self.assets() {
            if let Some(kind) = query.kind
                && !text_eq(asset.get("kind"), kind)
            {
                continue;
            }
            for context in dict_items(asset.get("contexts")) {
                if !query.matches(context)? {
                    continue;
                }
                let local_path = clean_text(asset.get("local_path"));
                if !seen_paths.insert(local_path.clone()) {
                    continue;
                }
                let key = (
                    to_int(context.get("style_index"))?.filter(|index| *index != 0).unwrap_or(0),
                    to_int(context.get("row_index"))?.filter(|index| *index != 0).unwrap_or(0),
                    clean_text(context.get("label")),
                    local_path,
                );
                found.push((key, self.asset_ref(asset, Some(context))?));
            }
        }
        found.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(found.into_iter().map(|(_, item)| item).collect())
    }

    /// `_pick_asset(record, ...)`: the first match, else `None`.
    fn pick_asset(&self, query: Query<'_>) -> Result<Value> {
        Ok(self.assets_for_context(query)?.into_iter().next().unwrap_or(Value::Null))
    }

    /// `_pick_costume_asset(record, costume, field=...)`: by the costume's title, then its
    /// category, then the style alone.
    fn pick_costume_asset(&self, costume: &Dict, field: &str) -> Result<Value> {
        let style_index = to_int(costume.get("style_index"))?;
        let base = Query {
            field: Some(field),
            style_index,
            ..Query::image()
        };
        let title = clean_text(costume.get("title"));
        if !title.is_empty() {
            let asset = self.pick_asset(Query {
                column_name: Some(&title),
                ..base
            })?;
            if asset != Value::Null {
                return Ok(asset);
            }
        }
        let category = clean_text(costume.get("category"));
        if !category.is_empty() {
            let asset = self.pick_asset(Query {
                column_category: Some(&category),
                ..base
            })?;
            if asset != Value::Null {
                return Ok(asset);
            }
        }
        self.pick_asset(base)
    }

    /// `_base_info(record, character)`: the character's, else the manifest's, else `{}`.
    fn base_info(&self, character: &Dict) -> Dict {
        if let Some(Value::Dict(base_info)) = character.get("base_info") {
            return base_info.clone();
        }
        match self.manifest.get("base_info") {
            Some(Value::Dict(base_info)) => base_info.clone(),
            _ => Dict::new(),
        }
    }

    /// `_profile_payload(record, character)`: each labelled row with text or an image.
    fn profile_payload(&self, character: &Dict) -> Result<Vec<Value>> {
        let base_info = self.base_info(character);
        let mut labels: Vec<String> = base_info.iter().map(|(key, _)| fav_runtime::pystr::str_key(key)).collect();
        labels.sort_by_cached_key(|label| casefold(label));
        let mut profile = Vec::new();
        for label in labels {
            let value = base_info_text(&base_info, &label);
            let asset = self.pick_asset(Query {
                label: Some(&label),
                ..Query::image()
            })?;
            if value.is_empty() && asset == Value::Null {
                continue;
            }
            profile.push(crate::json::dict([
                ("key", Value::Str(profile_key(&label))),
                ("label", Value::Str(label.clone())),
                ("value", Value::Str(value)),
                ("asset", asset),
            ]));
        }
        Ok(profile)
    }

    /// `_summary_payload(record)`: the list entry, from the manifest alone.
    fn summary_payload(&self) -> Result<Dict> {
        let profile = self.profile_payload(&Dict::new())?;
        let mut tags = Dict::new();
        for item in &profile {
            let (Some(Value::Str(key)), Some(Value::Str(value))) = (item.get("key"), item.get("value")) else {
                continue;
            };
            if !value.is_empty() && matches!(key.as_str(), "rarity" | "attribute" | "role" | "weapon" | "class") {
                tags.insert(key.clone(), Value::Str(value.clone()));
            }
        }
        let tree_row = match self.manifest.get("tree_row") {
            Some(Value::Dict(tree_row)) => tree_row.clone(),
            _ => Dict::new(),
        };
        let rarity_group = clean_text(tree_row.get("_bd2_group_name"));
        if !rarity_group.is_empty() {
            tags.insert("rarity_group", Value::Str(rarity_group));
        }
        let live2d_models = self.manifest.get("live2d_models");
        let costumes = match self.manifest.get("costumes") {
            Some(costumes @ Value::List(_)) => costumes.clone(),
            _ => match self.manifest.get("content_summary") {
                Some(Value::Dict(summary)) => summary.get("costumes").cloned().unwrap_or(Value::List(Vec::new())),
                _ => Value::List(Vec::new()),
            },
        };
        let empty = Dict::new();
        let first_costume = iterate_dicts(&costumes)?.into_iter().next().unwrap_or(&empty).clone();
        let mut icon = self.pick_asset(Query {
            field: Some("icon"),
            ..Query::image()
        })?;
        if icon == Value::Null {
            icon = self.pick_asset(Query {
                field: Some("icon_small"),
                ..Query::image()
            })?;
        }
        if icon == Value::Null {
            icon = self.pick_asset(Query::image())?;
        }
        let mut portrait = Value::Null;
        for field in ["costume_portrait", "costume_full_portrait", "costume_sprite"] {
            portrait = self.pick_costume_asset(&first_costume, field)?;
            if portrait != Value::Null {
                break;
            }
        }
        let count = |value: Option<&Value>| match value {
            Some(Value::List(items)) => Value::Int(i128::try_from(items.len()).unwrap_or(i128::MAX)),
            _ => Value::Int(0),
        };
        let search_terms = match &costumes {
            Value::List(items) => costume_search_terms(items),
            _ => Vec::new(),
        };
        let fetched_at = clean_text(self.manifest.get("fetched_at"));
        let Value::Dict(summary) = crate::json::dict([
            ("content_id", Value::Int(self.content_id)),
            ("title", Value::Str(self.safe_title())),
            ("directory_name", Value::Str(self.directory_name.clone())),
            ("source_url", str_value_of(clean_text(self.manifest.get("source_url")))),
            ("updated_at", self.manifest.get("updated_at").cloned().unwrap_or(Value::Null)),
            (
                "fetched_at",
                if fetched_at.is_empty() {
                    Value::Null
                } else {
                    Value::Str(fetched_at)
                },
            ),
            (
                "asset_counts",
                match self.manifest.get("asset_counts") {
                    Some(counts @ Value::Dict(_)) => counts.clone(),
                    _ => Value::Dict(Dict::new()),
                },
            ),
            ("tags", Value::Dict(tags)),
            ("profile", Value::List(profile)),
            ("icon", icon),
            ("portrait", portrait),
            ("costume_count", count(Some(&costumes))),
            ("live2d_model_count", count(live2d_models)),
            ("search_terms", Value::List(search_terms.into_iter().map(Value::Str).collect())),
        ]) else {
            unreachable!("json::dict builds a dictionary")
        };
        Ok(summary)
    }

    /// `_safe_manifest_title(record)`.
    fn safe_title(&self) -> String {
        let title = clean_text(self.manifest.get("title"));
        if !title.is_empty() {
            return title;
        }
        let tail = self
            .directory_name
            .split_once(" - ")
            .map_or(self.directory_name.as_str(), |(_, rest)| rest);
        let tail = py_strip(tail);
        if tail.is_empty() {
            self.content_id.to_string()
        } else {
            tail.to_owned()
        }
    }

    /// `_costume_payload(record, costume, live2d_models)`.
    fn costume_payload(&self, costume: &Dict, live2d_models: &[Value]) -> Result<Value> {
        let style_index = to_int(costume.get("style_index"))?;
        let mut models = Vec::new();
        for model in live2d_models.iter().filter_map(Value::as_dict) {
            if to_int(model.get("style_index"))? == style_index {
                models.push(self.live2d_model_payload(model)?);
            }
        }
        let rows = match costume.get("rows") {
            Some(rows @ Value::List(_)) => rows.clone(),
            _ => Value::List(Vec::new()),
        };
        let style = |kind: &'static str| Query {
            section: Some("style"),
            style_index,
            kind: Some(kind),
            ..Query::default()
        };
        Ok(crate::json::dict([
            ("style_index", optional_int(style_index)),
            ("style_name", str_value_of(clean_text(costume.get("style_name")))),
            ("title", str_value_of(clean_text(costume.get("title")))),
            ("category", str_value_of(clean_text(costume.get("category")))),
            ("sprite", self.pick_costume_asset(costume, "costume_sprite")?),
            ("portrait", self.pick_costume_asset(costume, "costume_portrait")?),
            ("full_portrait", self.pick_costume_asset(costume, "costume_full_portrait")?),
            ("gallery", Value::List(self.assets_for_context(style("image"))?)),
            ("videos", Value::List(self.assets_for_context(style("video"))?)),
            ("audio", Value::List(self.assets_for_context(style("audio"))?)),
            ("live2d_models", Value::List(models)),
            ("rows", rows),
        ]))
    }

    /// `_live2d_model_payload(record, model)`.
    fn live2d_model_payload(&self, model: &Dict) -> Result<Value> {
        let live2d_key = clean_text(model.get("live2d_key"));
        let pick = |kind: &'static str, field: &'static str| {
            self.pick_asset(Query {
                kind: Some(kind),
                live2d_key: Some(&live2d_key),
                live2d_field: Some(field),
                ..Query::default()
            })
        };
        let textures = self.assets_for_context(Query {
            live2d_key: Some(&live2d_key),
            kind: Some("live2d_texture"),
            ..Query::default()
        })?;
        let assets = crate::json::dict([
            ("atlas", pick("live2d_atlas", "atlas")?),
            ("skel", pick("live2d_skel", "skel")?),
            ("json", pick("live2d_json", "json")?),
            ("textures", Value::List(textures)),
            ("background", pick("live2d_background", "bg")?),
        ]);
        let text = |key: &str| str_value_of(clean_text(model.get(key)));
        let dict_or_empty = |key: &str| match model.get(key) {
            Some(value @ Value::Dict(_)) => value.clone(),
            _ => Value::Dict(Dict::new()),
        };
        let truthy = |key: &str| Value::Bool(model.get(key).is_some_and(Value::is_truthy));
        let spine_version = self.spine_version(&assets);
        Ok(crate::json::dict([
            ("label", text("label")),
            ("section", text("section")),
            ("style_index", optional_int(to_int(model.get("style_index"))?)),
            ("style_name", text("style_name")),
            ("costume_title", text("costume_title")),
            ("costume_category", text("costume_category")),
            ("row_index", optional_int(to_int(model.get("row_index"))?)),
            ("column_index", optional_int(to_int(model.get("column_index"))?)),
            ("field", text("field")),
            ("is_art_row", truthy("is_art_row")),
            ("column_name", text("column_name")),
            ("column_category", text("column_category")),
            ("column_role", text("column_role")),
            ("column_header", text("column_header")),
            ("key", text("key")),
            ("stable_id", text("stable_id")),
            ("live2d_key", Value::Str(live2d_key.clone())),
            ("animation", text("animation")),
            ("skin", text("skin")),
            ("limit_age", truthy("limit_age")),
            ("source", text("source")),
            ("variant", text("variant")),
            ("supplement_reason", text("supplement_reason")),
            ("viewer_entry_id", text("viewer_entry_id")),
            ("viewer_stem", text("viewer_stem")),
            ("source_page_url", text("source_page_url")),
            ("position", dict_or_empty("position")),
            ("bg_position", dict_or_empty("bg_position")),
            ("source_urls", dict_or_empty("urls")),
            ("spine_version", spine_version.map_or(Value::Null, Value::Str)),
            ("assets", assets),
        ]))
    }

    /// `_spine_version(record, assets)`: the binary skeleton first, the JSON export second.
    fn spine_version(&self, assets: &Value) -> Option<String> {
        for key in ["skel", "json"] {
            let Some(Value::Dict(asset)) = assets.get(key) else {
                continue;
            };
            if !asset.get("available").is_some_and(Value::is_truthy) {
                continue;
            }
            let path = match asset.get("path") {
                Some(value) if value.is_truthy() => str_value(value),
                _ => String::new(),
            };
            let Some(normalized) = normalize_asset_path(&path) else {
                continue;
            };
            if let Some(version) = spine_version_from_file(&self.root.join(normalized)) {
                return Some(version);
            }
        }
        None
    }

    /// `_read_character_json(record)`: `character.json`, else `{}` with a warning.
    fn read_character_json(&self) -> Dict {
        let path = self.root.join("character.json");
        read_json_object(&path).unwrap_or_else(|error| {
            log().warning(format!("Falling back to manifest data for {}: {error}", self.directory_name));
            Dict::new()
        })
    }
}

/// `_first_text_value(values)`.
fn first_text_value(values: Option<&Value>) -> String {
    for cell in dict_items(values) {
        if !text_eq(cell.get("type"), "text") {
            continue;
        }
        let text = clean_text(cell.get("value"));
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

/// `_base_info_text(base_info, label)`.
fn base_info_text(base_info: &Dict, label: &str) -> String {
    for row in dict_items(base_info.get(label)) {
        let text = first_text_value(row.get("values"));
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

/// `_costume_search_terms(costumes)`: names, titles, categories and row labels, once each,
/// by their case-folded text.
fn costume_search_terms(costumes: &[Value]) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for costume in costumes.iter().filter_map(Value::as_dict) {
        for key in ["style_name", "title", "category"] {
            let value = clean_text(costume.get(key));
            if !value.is_empty() {
                terms.push(value);
            }
        }
        for row in dict_items(costume.get("rows")) {
            let label = clean_text(row.get("label"));
            if !label.is_empty() {
                terms.push(label);
            }
        }
    }
    let mut seen = HashSet::new();
    terms.retain(|term| seen.insert(term.clone()));
    terms.sort_by_cached_key(|term| casefold(term));
    terms
}

// ---------- the library ----------

#[derive(Debug, Default)]
struct CacheState {
    signature: Option<Signature>,
    record_entries: HashMap<i128, RecordEntry>,
    summaries: Vec<Dict>,
}

/// `BD2Library`.
#[derive(Debug)]
pub struct Bd2Library {
    root: PathBuf,
    cache: Mutex<CacheState>,
}

/// One manifest found under the root, with its `st_mtime_ns` and `st_size`.
type ManifestEntry = (PathBuf, i128, i128);

fn signature_of(entries: &[ManifestEntry]) -> Signature {
    entries
        .iter()
        .map(|(path, mtime_ns, size)| (path.to_string_lossy().into_owned(), *mtime_ns, *size))
        .collect()
}

impl Bd2Library {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cache: Mutex::new(CacheState::default()),
        }
    }

    /// The collection root it reads.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `list_characters()`: every summary, by title.
    pub(crate) fn list_characters(&self) -> Result<Vec<Dict>> {
        let mut cache = self.lock();
        self.ensure_summary_cache(&mut cache)?;
        Ok(cache.summaries.clone())
    }

    /// `get_character(content_id)`: the summary with the costumes, models and assets.
    pub(crate) fn get_character(&self, content_id: i128) -> Result<Dict> {
        let record = self.get_record(content_id)?;
        let character = record.read_character_json();
        let costumes = if let Some(Value::List(items)) = character.get("costumes") {
            items.clone()
        } else {
            let fallback = match record.manifest.get("content_summary") {
                Some(Value::Dict(summary)) => summary.get("costumes"),
                _ => record.manifest.get("costumes"),
            };
            match fallback {
                Some(Value::List(items)) => items.clone(),
                _ => Vec::new(),
            }
        };
        let live2d_models = match character.get("live2d_models") {
            Some(Value::List(items)) => items.clone(),
            _ => match record.manifest.get("live2d_models") {
                Some(Value::List(items)) => items.clone(),
                _ => Vec::new(),
            },
        };
        let mut payload = record.summary_payload()?;
        let tree_row = match character.get("tree_row") {
            Some(tree_row @ Value::Dict(_)) => tree_row.clone(),
            _ => record.manifest.get("tree_row").cloned().unwrap_or(Value::Null),
        };
        payload.insert("tree_row", tree_row);
        payload.insert("base_info", Value::Dict(record.base_info(&character)));
        let mut costume_payloads = Vec::new();
        for costume in costumes.iter().filter_map(Value::as_dict) {
            costume_payloads.push(record.costume_payload(costume, &live2d_models)?);
        }
        payload.insert("costumes", Value::List(costume_payloads));
        let mut model_payloads = Vec::new();
        for model in live2d_models.iter().filter_map(Value::as_dict) {
            model_payloads.push(record.live2d_model_payload(model)?);
        }
        payload.insert("live2d_models", Value::List(model_payloads));
        let mut assets = Vec::new();
        for asset in record.assets() {
            assets.push(record.asset_ref(asset, None)?);
        }
        payload.insert("assets", Value::List(assets));
        Ok(payload)
    }

    /// `_manifest_entries()`: `*/manifest.json` under the root, skipping `_*` directories
    /// and those resolving outside it, by directory name.
    fn manifest_entries(&self) -> Vec<ManifestEntry> {
        if !self.root.is_dir() {
            return Vec::new();
        }
        let root_resolved = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let children = match std::fs::read_dir(&self.root).and_then(Iterator::collect::<std::io::Result<Vec<_>>>) {
            Ok(children) => children,
            Err(error) => {
                log().error(format!(
                    "Failed to list BD2 collection root: {}\n{}",
                    self.root.display(),
                    os_error_text(&error, &self.root)
                ));
                return Vec::new();
            }
        };
        let mut entries = Vec::new();
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            let child = self.root.join(&name);
            if name.starts_with('_') || !child.is_dir() {
                continue;
            }
            let resolved = std::fs::canonicalize(&child).unwrap_or_else(|_| child.clone());
            if !resolved.starts_with(&root_resolved) {
                log().warning(format!(
                    "Skipping BD2 character directory outside collection root: {}",
                    child.display()
                ));
                continue;
            }
            let manifest_path = child.join("manifest.json");
            if !manifest_path.is_file() {
                continue;
            }
            match std::fs::metadata(&manifest_path) {
                Ok(stat) => entries.push((
                    manifest_path,
                    i128::from(stat.mtime()) * 1_000_000_000 + i128::from(stat.mtime_nsec()),
                    i128::from(stat.size()),
                )),
                Err(_) => log().warning(format!("Failed to stat BD2 manifest: {}", manifest_path.display())),
            }
        }
        entries.sort_by_cached_key(|(path, _, _)| casefold(&directory_name(path)));
        entries
    }

    /// `_ensure_summary_cache()`: rebuild only when the manifests changed, preferring the
    /// cache file another process (or an earlier run) wrote for the same manifests.
    fn ensure_summary_cache(&self, cache: &mut CacheState) -> Result<()> {
        let entries = self.manifest_entries();
        let signature = signature_of(&entries);
        if cache.signature.as_ref() == Some(&signature) {
            return Ok(());
        }
        if self.load_summary_cache(cache, &signature)? {
            return Ok(());
        }
        let (record_entries, summaries) = Self::build_summary_cache(&entries)?;
        cache.signature = Some(signature);
        cache.record_entries = record_entries;
        cache.summaries = summaries;
        self.write_summary_cache(cache);
        Ok(())
    }

    fn build_summary_cache(entries: &[ManifestEntry]) -> Result<(HashMap<i128, RecordEntry>, Vec<Dict>)> {
        let mut record_entries: HashMap<i128, RecordEntry> = HashMap::new();
        let mut summaries: HashMap<i128, Dict> = HashMap::new();
        let mut order: Vec<i128> = Vec::new();
        let mut freshness_by_id: HashMap<i128, (f64, f64, i128)> = HashMap::new();
        for (manifest_path, mtime_ns, _size) in entries {
            let manifest = match read_json_object(manifest_path) {
                Ok(manifest) => manifest,
                Err(error) => {
                    log().warning(format!("Skipping unreadable BD2 manifest {}: {error}", manifest_path.display()));
                    continue;
                }
            };
            let Some(content_id) = manifest_content_id(manifest_path, &manifest)? else {
                log().warning(format!("Skipping BD2 manifest without content id: {}", manifest_path.display()));
                continue;
            };
            let fresh = freshness(&manifest, *mtime_ns)?;
            if let Some(existing) = record_entries.get(&content_id) {
                if freshness_le(fresh, freshness_by_id[&content_id]) {
                    log().warning(format!(
                        "Skipping stale duplicate BD2 manifest for content_id={content_id}: {}",
                        manifest_path.display()
                    ));
                    continue;
                }
                log().warning(format!(
                    "Replacing stale duplicate BD2 manifest for content_id={content_id}: {} -> {}",
                    existing.manifest_path.display(),
                    manifest_path.display()
                ));
            }
            let root = manifest_path.parent().map(Path::to_path_buf).unwrap_or_default();
            let record = Record {
                content_id,
                root: root.clone(),
                directory_name: directory_name(manifest_path),
                manifest,
            };
            let summary = record.summary_payload()?;
            record_entries.insert(
                content_id,
                RecordEntry {
                    content_id,
                    root,
                    manifest_path: manifest_path.clone(),
                    directory_name: record.directory_name,
                },
            );
            if summaries.insert(content_id, summary).is_none() {
                order.push(content_id);
            }
            freshness_by_id.insert(content_id, fresh);
        }
        let mut sorted: Vec<Dict> = order.into_iter().filter_map(|content_id| summaries.remove(&content_id)).collect();
        sorted.sort_by_cached_key(|summary| {
            let title = summary
                .get("title")
                .filter(|value| value.is_truthy())
                .map(str_value)
                .unwrap_or_default();
            let content_id = match summary.get("content_id") {
                Some(Value::Int(content_id)) => *content_id,
                _ => 0,
            };
            (casefold(&title), content_id)
        });
        Ok((record_entries, sorted))
    }

    fn summary_cache_path(&self) -> PathBuf {
        self.root.join("_api").join("summary-cache.json")
    }

    /// `_load_summary_cache(signature)`.
    fn load_summary_cache(&self, cache: &mut CacheState, signature: &Signature) -> Result<bool> {
        let Some(cached) = read_summary_cache(&self.summary_cache_path(), signature, &log(), "BD2") else {
            return Ok(false);
        };
        let current_paths: HashSet<&str> = signature.iter().map(|(path, _, _)| path.as_str()).collect();
        let mut record_entries = HashMap::new();
        for item in &cached.records {
            let content_id = to_int(item.get("content_id"))?;
            let (Some(content_id), Some(Value::Str(manifest_path))) = (content_id, item.get("manifest_path")) else {
                return Ok(false);
            };
            if !current_paths.contains(manifest_path.as_str()) {
                return Ok(false);
            }
            let manifest_path = PathBuf::from(manifest_path);
            record_entries.insert(
                content_id,
                RecordEntry {
                    content_id,
                    root: manifest_path.parent().map(Path::to_path_buf).unwrap_or_default(),
                    directory_name: directory_name(&manifest_path),
                    manifest_path,
                },
            );
        }
        cache.signature = Some(signature.clone());
        cache.record_entries = record_entries;
        cache.summaries = cached.summaries;
        Ok(true)
    }

    /// `_write_summary_cache()`: records by content id, with the summaries.
    fn write_summary_cache(&self, cache: &CacheState) {
        if !self.root.is_dir() {
            return;
        }
        let mut entries: Vec<&RecordEntry> = cache.record_entries.values().collect();
        entries.sort_by_key(|entry| entry.content_id);
        let records = entries
            .into_iter()
            .map(|entry| {
                let mut record = Dict::new();
                record.insert("content_id", Value::Int(entry.content_id));
                record.insert("manifest_path", Value::Str(entry.manifest_path.to_string_lossy().into_owned()));
                record
            })
            .collect();
        write_summary_cache(
            &self.summary_cache_path(),
            &SummaryCacheData {
                signature: cache.signature.clone().unwrap_or_default(),
                records,
                summaries: cache.summaries.clone(),
            },
            &log(),
            "BD2",
        );
    }

    /// `_get_record(content_id)`: the entry with its manifest read again.
    fn get_record(&self, content_id: i128) -> Result<Record> {
        let entry = {
            let mut cache = self.lock();
            self.ensure_summary_cache(&mut cache)?;
            cache.record_entries.get(&content_id).cloned().ok_or(LibraryError::NotFound)?
        };
        let manifest = read_json_object(&entry.manifest_path).map_err(|error| {
            log().warning(format!(
                "Skipping unreadable BD2 manifest {}: {error}",
                entry.manifest_path.display()
            ));
            LibraryError::NotFound
        })?;
        Ok(Record {
            content_id: entry.content_id,
            root: entry.root,
            directory_name: entry.directory_name,
            manifest,
        })
    }
}

#[cfg(test)]
mod tests {
    use fav_core::pyjson::Value;

    use super::{LibraryError, asset_static_url, normalize_asset_path, profile_key, route_sort_timestamp, sort_timestamp, to_int};

    #[test]
    fn asset_paths_stay_under_assets() {
        assert_eq!(normalize_asset_path("assets/a/./b.png").as_deref(), Some("assets/a/b.png"));
        assert_eq!(normalize_asset_path("assets\\a\\b.png").as_deref(), Some("assets/a/b.png"));
        assert_eq!(normalize_asset_path(" assets//a%20b.png ").as_deref(), Some("assets/a b.png"));
        for bad in ["", "/assets/a", "../a", "assets/../a", "other/a", ".", "assets/%2e%2e/a"] {
            assert_eq!(normalize_asset_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn static_urls_quote_like_urllib() {
        assert_eq!(
            asset_static_url("101 - Test é", "assets/a b/~x.png", "s h"),
            "/static/bd2/101%20-%20Test%20%C3%A9/assets/a%20b/~x.png?v=s%20h"
        );
        assert_eq!(asset_static_url("a/b", "assets/x", ""), "/static/bd2/a%2Fb/assets/x");
    }

    #[test]
    fn ints_and_timestamps_read_like_python() {
        assert_eq!(to_int(Some(&Value::Str(" 12 ".to_owned()))), Ok(Some(12)));
        assert_eq!(to_int(Some(&Value::Bool(true))), Ok(None));
        assert_eq!(to_int(Some(&Value::Float(3.0))), Ok(None));
        assert!(matches!(to_int(Some(&Value::Str("²".to_owned()))), Err(LibraryError::Failed(_))));
        assert_eq!(sort_timestamp(Some(&Value::Str("1970-01-01T00:00:01Z".to_owned()))), Ok(1.0));
        assert_eq!(sort_timestamp(Some(&Value::Str("1970-01-01T00:00:01.5".to_owned()))), Ok(1.5));
        assert_eq!(sort_timestamp(Some(&Value::Str("later".to_owned()))), Ok(0.0));
        assert_eq!(sort_timestamp(Some(&Value::Bool(true))), Ok(0.0));
        assert_eq!(sort_timestamp(Some(&Value::Int(7))), Ok(7.0));
        assert_eq!(route_sort_timestamp(&Value::Str("٣".to_owned())), Ok(3.0));
        assert!(route_sort_timestamp(&Value::Str("²".to_owned())).is_err());
    }

    #[test]
    fn profile_keys_keep_letters_and_digits() {
        assert_eq!(profile_key(" Élan Vital! "), "élan_vital");
        assert_eq!(profile_key("稀有度"), "稀有度");
        assert_eq!(profile_key("--"), "field");
    }
}
