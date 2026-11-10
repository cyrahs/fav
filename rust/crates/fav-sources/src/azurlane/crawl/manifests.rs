//! The per-character `manifest.json` and `character.json` the API reads, built from the
//! catalog tables.
//!
//! Python origin: `_asset_payload_from_row` through `_character_summary_payload` in
//! `src/web/azurlane.py`. Rows arrive as the typed structs below; the crawl reads them one
//! batch of characters at a time.

use fav_core::pyjson::{Dict, DumpOptions, Value};
use fav_core::pypath;
use fav_runtime::source::JobError;

use super::assets::ASSET_KIND_ORDER;

/// `_MANIFEST_SCHEMA_VERSION`.
const MANIFEST_SCHEMA_VERSION: i128 = 1;
const ASSET_AVAILABLE_STATUS: &str = "downloaded";

/// An `azurlane_characters` row (text columns stripped, as `_row_text` reads them).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CharacterRow {
    /// The stored `character_key`, unstripped: the batch cursor and the model join compare
    /// it with the column itself.
    pub(crate) raw_key: String,
    pub(crate) character_key: String,
    pub(crate) source_id: Option<i64>,
    pub(crate) name_zh: String,
    pub(crate) name_en: String,
    pub(crate) manifest_path: String,
    pub(crate) active: bool,
    pub(crate) source_metadata: Dict,
    pub(crate) fetched_at: Option<String>,
    pub(crate) completed_at: Option<String>,
}

/// An `azurlane_models` row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelRow {
    pub(crate) model_id: String,
    pub(crate) model_type: String,
    pub(crate) source: String,
    pub(crate) character_key: String,
    pub(crate) costume_key: String,
    pub(crate) costume_id: Option<i64>,
    pub(crate) costume_name_zh: String,
    pub(crate) costume_name_en: String,
    pub(crate) primary_url: String,
    pub(crate) fallback_url: String,
    pub(crate) display_info_url: String,
    pub(crate) availability_state: String,
    pub(crate) source_metadata: Dict,
    pub(crate) fetched_at: Option<String>,
    pub(crate) completed_at: Option<String>,
}

/// One `azurlane_model_assets` row joined with its `azurlane_assets` row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AssetRow {
    pub(crate) model_id: String,
    pub(crate) url: String,
    pub(crate) kind: String,
    pub(crate) context_hash: String,
    pub(crate) local_path: String,
    pub(crate) original_filename: String,
    pub(crate) relation_fallback_url: String,
    pub(crate) context_json: Dict,
    pub(crate) normalized_url: String,
    pub(crate) downloaded_url: String,
    pub(crate) asset_fallback_url: String,
    pub(crate) sha256: String,
    pub(crate) size: i64,
    pub(crate) content_type: String,
    pub(crate) status: String,
    pub(crate) failed_count: i64,
    pub(crate) last_error: String,
    pub(crate) last_attempt_at: Option<String>,
    pub(crate) next_retry_at: Option<String>,
    pub(crate) last_seen_at: Option<String>,
}

fn text(value: &str) -> Value {
    Value::Str(value.to_owned())
}

fn timestamp(value: Option<&String>) -> Value {
    value.map_or(Value::Null, |value| Value::Str(value.clone()))
}

fn int(value: i64) -> Value {
    Value::Int(i128::from(value))
}

fn optional_int(value: Option<i64>) -> Value {
    value.map_or(Value::Null, int)
}

/// `_status_counts(items)`: occurrences, keys sorted.
fn status_counts<'a>(items: impl IntoIterator<Item = &'a str>) -> Dict {
    let mut counts: std::collections::BTreeMap<&str, i128> = std::collections::BTreeMap::new();
    for item in items {
        *counts.entry(item).or_default() += 1;
    }
    let mut out = Dict::new();
    for (key, count) in counts {
        out.insert(key, Value::Int(count));
    }
    out
}

/// One asset of a model as the manifest lists it, before the payload is rendered.
#[derive(Debug, Clone)]
struct ManifestAsset {
    url: String,
    normalized_url: String,
    downloaded_url: String,
    fallback_url: String,
    kind: String,
    local_path: String,
    original_filename: String,
    sha256: String,
    size: i64,
    content_type: String,
    status: String,
    failed_count: i64,
    error: String,
    last_attempt_at: Option<String>,
    next_retry_at: Option<String>,
    last_seen_at: Option<String>,
    context_hashes: Vec<String>,
    contexts: Vec<Dict>,
}

impl ManifestAsset {
    /// `_asset_payload_from_row(row)`.
    fn from_row(row: &AssetRow) -> Self {
        let fallback_url = if row.relation_fallback_url.is_empty() {
            row.asset_fallback_url.clone()
        } else {
            row.relation_fallback_url.clone()
        };
        Self {
            url: row.url.clone(),
            normalized_url: if row.normalized_url.is_empty() {
                row.url.clone()
            } else {
                row.normalized_url.clone()
            },
            downloaded_url: row.downloaded_url.clone(),
            fallback_url,
            kind: row.kind.clone(),
            local_path: row.local_path.clone(),
            original_filename: row.original_filename.clone(),
            sha256: row.sha256.clone(),
            size: row.size,
            content_type: row.content_type.clone(),
            status: if row.status.is_empty() {
                "pending".to_owned()
            } else {
                row.status.clone()
            },
            failed_count: row.failed_count,
            error: row.last_error.clone(),
            last_attempt_at: row.last_attempt_at.clone(),
            next_retry_at: row.next_retry_at.clone(),
            last_seen_at: row.last_seen_at.clone(),
            context_hashes: Vec::new(),
            contexts: Vec::new(),
        }
    }

    /// `_asset_available(asset)`.
    fn available(&self) -> bool {
        self.status == ASSET_AVAILABLE_STATUS && !self.local_path.is_empty() && !self.sha256.is_empty() && self.size > 0
    }

    fn to_value(&self) -> Value {
        let mut source_urls = Dict::new();
        source_urls.insert("primary", text(&self.url));
        source_urls.insert("fallback", text(&self.fallback_url));
        source_urls.insert("downloaded", text(&self.downloaded_url));
        let mut payload = Dict::new();
        payload.insert("url", text(&self.url));
        payload.insert("normalized_url", text(&self.normalized_url));
        payload.insert("downloaded_url", text(&self.downloaded_url));
        payload.insert("fallback_url", text(&self.fallback_url));
        payload.insert("source_urls", Value::Dict(source_urls));
        payload.insert("kind", text(&self.kind));
        payload.insert("local_path", text(&self.local_path));
        payload.insert("original_filename", text(&self.original_filename));
        payload.insert("sha256", text(&self.sha256));
        payload.insert("size", int(self.size));
        payload.insert("content_type", text(&self.content_type));
        payload.insert("status", text(&self.status));
        payload.insert("available", Value::Bool(self.available()));
        payload.insert("failed_count", int(self.failed_count));
        payload.insert("error", text(&self.error));
        payload.insert("last_attempt_at", timestamp(self.last_attempt_at.as_ref()));
        payload.insert("next_retry_at", timestamp(self.next_retry_at.as_ref()));
        payload.insert("last_seen_at", timestamp(self.last_seen_at.as_ref()));
        payload.insert(
            "context_hashes",
            Value::List(self.context_hashes.iter().map(|hash| text(hash)).collect()),
        );
        payload.insert("contexts", Value::List(self.contexts.iter().cloned().map(Value::Dict).collect()));
        Value::Dict(payload)
    }
}

fn kind_order(kind: &str) -> usize {
    ASSET_KIND_ORDER
        .iter()
        .position(|known| *known == kind)
        .unwrap_or(ASSET_KIND_ORDER.len())
}

fn json_dumps(value: &Value) -> Result<String, JobError> {
    fav_core::pyjson::dumps(value, &DumpOptions::hash()).map_err(|error| JobError::new("TypeError", error.to_string()))
}

/// The rendered assets of a model, with their manifest view of availability.
pub(crate) struct ModelAssets {
    assets: Vec<ManifestAsset>,
}

/// `_model_assets(asset_rows)`: one entry per (URL, kind, local path), its context hashes
/// and contexts gathered and sorted, in manifest order.
pub(crate) fn model_assets(mut rows: Vec<&AssetRow>) -> Result<ModelAssets, JobError> {
    rows.sort_by(|a, b| {
        (kind_order(&a.kind), &a.local_path, &a.url, &a.context_hash).cmp(&(kind_order(&b.kind), &b.local_path, &b.url, &b.context_hash))
    });
    let mut assets: Vec<ManifestAsset> = Vec::new();
    let mut positions: std::collections::HashMap<(&str, &str, &str), usize> = std::collections::HashMap::new();
    for row in rows {
        let index = *positions
            .entry((row.url.as_str(), row.kind.as_str(), row.local_path.as_str()))
            .or_insert_with(|| {
                assets.push(ManifestAsset::from_row(row));
                assets.len() - 1
            });
        let asset = &mut assets[index];
        if !row.context_hash.is_empty() && !asset.context_hashes.contains(&row.context_hash) {
            asset.context_hashes.push(row.context_hash.clone());
        }
        if !row.context_json.is_empty() && !asset.contexts.contains(&row.context_json) {
            asset.contexts.push(row.context_json.clone());
        }
    }
    for asset in &mut assets {
        asset.context_hashes.sort();
        let mut keyed: Vec<(String, Dict)> = Vec::with_capacity(asset.contexts.len());
        for context in asset.contexts.drain(..) {
            keyed.push((json_dumps(&Value::Dict(context.clone()))?, context));
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        asset.contexts = keyed.into_iter().map(|(_, context)| context).collect();
    }
    assets.sort_by(|a, b| (kind_order(&a.kind), &a.local_path, &a.url).cmp(&(kind_order(&b.kind), &b.local_path, &b.url)));
    Ok(ModelAssets { assets })
}

/// `_archive_state(assets)`.
fn archive_state(assets: &[ManifestAsset]) -> &'static str {
    if assets.is_empty() {
        "no-assets"
    } else if assets.iter().all(ManifestAsset::available) {
        "complete"
    } else if assets.iter().any(ManifestAsset::available) {
        "partial"
    } else {
        "unavailable"
    }
}

/// `_model_payload(row, assets)`.
pub(crate) fn model_payload(row: &ModelRow, assets: &ModelAssets) -> Value {
    let assets = &assets.assets;
    let mut costume = Dict::new();
    costume.insert("key", text(&row.costume_key));
    costume.insert("id", optional_int(row.costume_id));
    costume.insert("name_zh", text(&row.costume_name_zh));
    costume.insert("name_en", text(&row.costume_name_en));
    let mut source_urls = Dict::new();
    source_urls.insert("primary", text(&row.primary_url));
    source_urls.insert("fallback", text(&row.fallback_url));
    source_urls.insert("display_info", text(&row.display_info_url));
    let mut availability = Dict::new();
    availability.insert(
        "source_state",
        text(if row.availability_state.is_empty() {
            "unchecked"
        } else {
            &row.availability_state
        }),
    );
    availability.insert("archive_state", text(archive_state(assets)));
    availability.insert(
        "asset_status_counts",
        Value::Dict(status_counts(assets.iter().map(|asset| asset.status.as_str()))),
    );
    #[allow(clippy::cast_possible_wrap)] // reason: an asset count is far below 2^127
    let available = assets.iter().filter(|asset| asset.available()).count() as i128;
    #[allow(clippy::cast_possible_wrap)] // reason: as above
    let total = assets.len() as i128;
    availability.insert("available_asset_count", Value::Int(available));
    availability.insert("asset_count", Value::Int(total));
    availability.insert("completed_at", timestamp(row.completed_at.as_ref()));
    let mut payload = Dict::new();
    payload.insert("model_id", text(&row.model_id));
    payload.insert("type", text(&row.model_type));
    payload.insert("source", text(&row.source));
    payload.insert("character_key", text(&row.character_key));
    payload.insert("costume", Value::Dict(costume));
    payload.insert("source_urls", Value::Dict(source_urls));
    payload.insert("availability", Value::Dict(availability));
    payload.insert("source_metadata", Value::Dict(row.source_metadata.clone()));
    payload.insert("fetched_at", timestamp(row.fetched_at.as_ref()));
    payload.insert("completed_at", timestamp(row.completed_at.as_ref()));
    payload.insert("assets", Value::List(assets.iter().map(ManifestAsset::to_value).collect()));
    payload.insert(
        "asset_counts",
        Value::Dict(status_counts(assets.iter().map(|asset| asset.kind.as_str()))),
    );
    Value::Dict(payload)
}

fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.as_dict().and_then(|dict| dict.get(key)).unwrap_or(&Value::Null)
}

fn field_text<'a>(value: &'a Value, key: &str) -> &'a str {
    field(value, key).as_str().unwrap_or_default()
}

/// The key a character's models are sorted by: (type, costume key, model id).
pub(crate) fn model_sort_key(model: &Value) -> (String, String, String) {
    (
        field_text(model, "type").to_owned(),
        field_text(field(model, "costume"), "key").to_owned(),
        field_text(model, "model_id").to_owned(),
    )
}

/// `_manifest_asset_counts(models)`.
fn manifest_asset_counts(models: &[Value]) -> Dict {
    let kinds = models
        .iter()
        .filter_map(|model| field(model, "assets").as_list())
        .flatten()
        .map(|asset| field_text(asset, "kind"))
        .filter(|kind| !kind.is_empty());
    status_counts(kinds)
}

/// `_model_counts(models)`.
fn model_counts(models: &[Value]) -> Dict {
    let mut counts = [("live2d", 0_i128), ("spine", 0), ("painting", 0)];
    for model in models {
        let kind = field_text(model, "type");
        if let Some((_, count)) = counts.iter_mut().find(|(name, _)| *name == kind) {
            *count += 1;
        }
    }
    let mut out = Dict::new();
    for (name, count) in counts {
        out.insert(name, Value::Int(count));
    }
    #[allow(clippy::cast_possible_wrap)] // reason: a model count is far below 2^127
    out.insert("total", Value::Int(models.len() as i128));
    out
}

/// `_character_manifest_root(base_dir, row)`: the directory of the stored manifest path,
/// else the directory a new character gets.
pub(crate) fn character_manifest_root(base_dir: &str, row: &CharacterRow) -> String {
    if !row.manifest_path.is_empty() {
        return parent(&row.manifest_path);
    }
    let name = [&row.name_en, &row.name_zh, &row.character_key]
        .into_iter()
        .find(|name| !name.is_empty())
        .map_or("", String::as_str);
    let mut safe_name = fav_core::filename::sanitize(name, 120);
    if safe_name.is_empty() {
        safe_name.clone_from(&row.character_key);
    }
    pypath::join(base_dir, &format!("{} - {safe_name}", row.character_key))
}

/// `str(PurePosixPath(path).parent)`.
pub(crate) fn parent(path: &str) -> String {
    let normalized = pypath::join(".", path);
    match normalized.rsplit_once('/') {
        Some(("", _)) => "/".to_owned(),
        Some((head, _)) => head.to_owned(),
        None => ".".to_owned(),
    }
}

/// `_character_payload(row, models, root)`.
pub(crate) fn character_payload(row: &CharacterRow, models: Vec<Value>, root: &str) -> Value {
    let mut payload = Dict::new();
    payload.insert("schema_version", Value::Int(MANIFEST_SCHEMA_VERSION));
    payload.insert("source", text("azurlane"));
    payload.insert("character_key", text(&row.character_key));
    payload.insert("source_id", optional_int(row.source_id));
    payload.insert("name_zh", text(&row.name_zh));
    payload.insert("name_en", text(&row.name_en));
    payload.insert("directory_name", text(pypath::name(root)));
    payload.insert("manifest_path", text(&pypath::join(root, "manifest.json")));
    payload.insert("source_metadata", Value::Dict(row.source_metadata.clone()));
    payload.insert("fetched_at", timestamp(row.fetched_at.as_ref()));
    payload.insert("completed_at", timestamp(row.completed_at.as_ref()));
    payload.insert("active", Value::Bool(row.active));
    payload.insert("model_counts", Value::Dict(model_counts(&models)));
    payload.insert("asset_counts", Value::Dict(manifest_asset_counts(&models)));
    payload.insert("models", Value::List(models));
    Value::Dict(payload)
}

/// `_character_summary_payload(manifest)`: `character.json`, the manifest without assets.
pub(crate) fn character_summary_payload(manifest: &Value) -> Value {
    let mut payload = Dict::new();
    for key in [
        "schema_version",
        "source",
        "character_key",
        "source_id",
        "name_zh",
        "name_en",
        "directory_name",
        "source_metadata",
        "fetched_at",
        "completed_at",
        "model_counts",
        "asset_counts",
    ] {
        payload.insert(key, field(manifest, key).clone());
    }
    let models = field(manifest, "models")
        .as_list()
        .map(|models| {
            models
                .iter()
                .map(|model| {
                    let mut summary = Dict::new();
                    for key in ["model_id", "type", "source", "costume", "source_urls", "availability"] {
                        summary.insert(key, field(model, key).clone());
                    }
                    Value::Dict(summary)
                })
                .collect()
        })
        .unwrap_or_default();
    payload.insert("models", Value::List(models));
    Value::Dict(payload)
}
