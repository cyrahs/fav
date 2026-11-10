//! l2d.su and nagami as sources of Azur Lane Live2D, spine and painting models: the source
//! snapshots, the model catalog built from them, the resource files of each model, their
//! validation, the health report, and the origin probe.
//!
//! Python origin: `src/tool/azurlane_l2d_sources.py`. The submodules split it by concern:
//! [`model`] has the dataclasses, [`parse`] the payload parsers, [`catalog`] the catalog
//! and the URL helpers, [`resources`] the resource enumeration and local paths, and
//! [`fetch`] everything that makes requests.
//!
//! Failures are [`PyError`]s with Python's class and text: `SourceParseError` (a
//! `ValueError`), `SourceSchemaError` (a `TypeError`), `ProtectedResourceError`, or the
//! plain `ValueError` / `TypeError` Python would let escape (a URL `urlsplit` refuses, a
//! value it cannot iterate).

use std::fmt::Write as _;

use fav_core::pyjson::{Dict, Value};
use fav_core::pyurl::{SplitResult, urlsplit};

use crate::PyError;

pub mod catalog;
pub mod fetch;
pub mod health;
pub mod model;
pub mod parse;
pub mod resources;

pub use catalog::*;
pub use fetch::*;
pub use health::*;
pub use model::*;
pub use parse::*;
pub use resources::*;

pub const L2D_SU_HOST: &str = "l2d.su";
/// Since 2026-10-31 the site reads everything from /al/ on the CDN, and /al-b/ (its source
/// from 2026-10-28) is gone. Every file under /al/ has a hashed name, so neither ship nor
/// skin names appear in any URL any more, and every JSON file is wrapped in an AZP2
/// envelope ([`crate::azurlane::protected`]). Voices are the exception and still live at
/// /al/assets/cue/<voice path>.ogg.
pub const L2D_SU_ASSET_BASE_URL: &str = "https://static.l2d.su/al/assets";
pub const L2D_SU_STATIC_BASE_URL: &str = L2D_SU_ASSET_BASE_URL;
pub const L2D_SU_AUDIO_BASE_URL: &str = L2D_SU_ASSET_BASE_URL;
/// The roots assets were archived from before, newest first (see `l2d_su_legacy_asset_urls`).
pub const L2D_SU_PREVIOUS_ASSET_BASE_URLS: [&str; 3] = [
    "https://static.l2d.su/al-b/assets",
    "https://static.l2d.su/al/assets/webp",
    "https://static.l2d.su/al/assets/original",
];
pub const L2D_SU_LEGACY_STATIC_BASE_URL: &str = "https://static.l2d.su/azurlane";
pub const L2D_SU_ARCHIVED_ASSET_BASE_URLS: [&str; 4] = [
    L2D_SU_PREVIOUS_ASSET_BASE_URLS[0],
    L2D_SU_PREVIOUS_ASSET_BASE_URLS[1],
    L2D_SU_PREVIOUS_ASSET_BASE_URLS[2],
    L2D_SU_LEGACY_STATIC_BASE_URL,
];
pub const L2D_SU_REFERER: &str = "https://l2d.su/";
pub const L2D_SU_PRIMARY_REGION: &str = "CN";
pub const L2D_SU_ENGLISH_REGION: &str = "EN";
/// nagami moved the mapping under a per-server directory on 2026-11-03; the CN, EN and JP
/// files are identical and carry the names for every server.
pub const NAGAMI_MAPPING_URL: &str = "https://data.nagami.moe/live2d/EN/l2d_mapping.json";
pub const NAGAMI_NAME_LOCALES: [&str; 3] = ["EN", "CN", "JP"];
pub const NAGAMI_LIVE2D_BASE_URL: &str = "https://cdn.nagami.moe/live2d";
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
pub const CATALOG_ENTRY_ID_PREFIX: &str = "azurlane";
pub const CATALOG_VARIANT_TOKEN_SIZE: usize = 6;
pub const SPINE_PARTS_MANIFEST_EXTENSION: &str = ".json";
/// The painting index reserves this key for the face anchor: it carries size/pivot/position
/// for the paintingface diffs, and is not a layer of its own.
pub const PAINTING_FACE_SLOT: &str = "face";

pub const SOURCE_PARSE_ERROR: &str = "SourceParseError";
pub const SOURCE_SCHEMA_ERROR: &str = "SourceSchemaError";

pub(crate) fn parse_error(message: impl Into<String>) -> PyError {
    PyError::new(SOURCE_PARSE_ERROR, message)
}

pub(crate) fn schema_error(message: impl Into<String>) -> PyError {
    PyError::new(SOURCE_SCHEMA_ERROR, message)
}

/// Whether `error` is one a caller catching `ValueError` sees.
pub fn is_value_error(error: &PyError) -> bool {
    matches!(
        error.class.as_str(),
        "ValueError" | SOURCE_PARSE_ERROR | crate::azurlane::protected::CLASS | "UnicodeDecodeError"
    )
}

/// `urlsplit(url)`, its `ValueError` as a [`PyError`].
pub(crate) fn split(url: &str) -> Result<SplitResult, PyError> {
    urlsplit(url, "").map_err(|error| PyError::new("ValueError", error.0))
}

/// `urljoin(base, value.strip())`.
pub(crate) fn resolve_resource_url(base_url: &str, value: &str) -> Result<String, PyError> {
    fav_core::pyurl::urljoin(base_url, fav_core::pytext::py_strip(value)).map_err(|error| PyError::new("ValueError", error.0))
}

/// `blake2b(text.encode(), digest_size=size).hexdigest()`.
pub(crate) fn blake2b_hex(text: &str, size: usize) -> String {
    use blake2::digest::{Update, VariableOutput};
    let mut hasher = blake2::Blake2bVar::new(size).expect("a digest size from 1 to 64");
    hasher.update(text.as_bytes());
    let mut digest = vec![0_u8; size];
    hasher.finalize_variable(&mut digest).expect("the buffer has the digest's size");
    let mut out = String::with_capacity(size * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `unicodedata.normalize('NFKC', text)`.
pub(crate) fn nfkc(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    text.nfkc().collect()
}

/// `_catalog_key`: the stripped NFKC text, or `unknown`.
pub fn catalog_key(value: &str) -> String {
    let normalized = nfkc(fav_core::pytext::py_strip(value));
    if normalized.is_empty() { "unknown".to_owned() } else { normalized }
}

/// `_catalog_entry_id`.
pub fn catalog_entry_id(model_type: ModelKind, character_key: &str, costume_key: &str) -> String {
    format!(
        "{CATALOG_ENTRY_ID_PREFIX}:{}:{}:{}",
        model_type.as_str(),
        catalog_key(character_key),
        catalog_key(costume_key)
    )
}

/// `_optional_str`: a string field, stripped; anything else is empty.
pub(crate) fn optional_str(item: &Dict, field_name: &str) -> String {
    match item.get(field_name) {
        Some(Value::Str(text)) => fav_core::pytext::py_strip(text).to_owned(),
        _ => String::new(),
    }
}

/// `_optional_str` on a value that may not be a dict (`isinstance(model, dict)` guards).
pub(crate) fn optional_str_of(item: Option<&Value>, field_name: &str) -> String {
    item.and_then(Value::as_dict)
        .map(|dict| optional_str(dict, field_name))
        .unwrap_or_default()
}

/// `_required_int`.
pub(crate) fn required_int(item: &Dict, field_name: &str, context: &str) -> Result<i128, PyError> {
    match item.get(field_name) {
        Some(Value::Int(value)) => Ok(*value),
        _ => Err(schema_error(format!("{context}.{field_name} must be an integer"))),
    }
}

/// `_required_str`: the value itself (not stripped) when it has text.
pub(crate) fn required_str(item: &Dict, field_name: &str, context: &str) -> Result<String, PyError> {
    match item.get(field_name) {
        Some(Value::Str(text)) if !fav_core::pytext::py_strip(text).is_empty() => Ok(text.clone()),
        _ => Err(schema_error(format!("{context}.{field_name} must be a non-empty string"))),
    }
}

/// `_string_tuple`: the list's non-blank strings, stripped.
pub(crate) fn string_tuple(value: Option<&Value>) -> Vec<String> {
    let Some(Value::List(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .map(fav_core::pytext::py_strip)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `isinstance(value, int) and not isinstance(value, bool)`.
pub(crate) fn plain_int(value: Option<&Value>) -> Option<i128> {
    match value {
        Some(Value::Int(value)) => Some(*value),
        _ => None,
    }
}

/// `tuple(dict.fromkeys(value for value in values if value))`.
pub(crate) fn unique_non_empty<I: IntoIterator<Item = String>>(values: I) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

/// `datetime.now(UTC).isoformat()`.
pub fn utc_now_iso() -> String {
    let now = chrono::Utc::now().fixed_offset();
    fav_core::pyjson::py_isoformat_datetime(&now)
}

/// `_request_headers()`.
pub fn request_headers() -> Vec<(String, String)> {
    vec![
        ("Accept".to_owned(), "application/json, text/javascript, */*".to_owned()),
        ("Referer".to_owned(), L2D_SU_REFERER.to_owned()),
        ("User-Agent".to_owned(), DEFAULT_USER_AGENT.to_owned()),
    ]
}

#[cfg(test)]
mod tests;
