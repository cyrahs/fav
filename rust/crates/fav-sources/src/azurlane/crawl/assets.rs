//! The crawl's pure helpers: the asset record, the seed assets a model's enumeration starts
//! from, the response checks, the retry ladder and the run's sanity checks.
//!
//! Python origin: the module-level functions of `src/web/azurlane.py`.

use fav_core::filename::sanitize;
use fav_core::pyjson::{Dict, Value};
use fav_core::pypath;
use fav_runtime::source::JobError;

use crate::PyError;
use crate::azurlane::l2d_sources::{
    ArchivedModel, AzurLaneModelCatalog, AzurLaneModelResourceEnumeration, AzurLaneSourceSnapshots, ModelCharacter, ModelEntry,
    case_variant_urls, entry_painting_index_url, is_hashed_asset_url, spine_parts_manifest_url,
};
pub(crate) use crate::gamekee::is_cdn_rejection_body;
use crate::gamekee::{decode_utf8_ignore, lstrip_bytes};
use crate::pydecode::json_loads;

pub(crate) const MAX_RETRIES: u32 = 3;
const LIMITED_RETRY_ATTEMPTS: u32 = 2;
pub(crate) const BODY_PREFIX_LIMIT: usize = 4096;
/// `_ASSET_RETRY_COOLDOWN_DAYS`: the wait after a failure, for an asset and for its model.
const ASSET_RETRY_COOLDOWN_DAYS: [i64; 3] = [1, 3, 7];
pub(crate) const MESH_UNAVAILABLE_REASON: &str = "response is not a Wavefront OBJ; the CDN hosts no mesh here";
pub(crate) const CDN_REJECTION_REASON: &str = "cdn rejection response";
/// 403, 406, 429 and 503 count toward the circuit breaker.
pub(crate) const LIMITED_STATUSES: [u16; 4] = [403, 406, 429, 503];
/// `_HTTP_METHOD_UNSUPPORTED`: a HEAD answered with these is asked again as a GET.
pub(crate) const METHOD_UNSUPPORTED: [u16; 2] = [405, 501];

const JSON_ASSET_KINDS: [&str; 8] = [
    "live2d.model3",
    "live2d.physics",
    "live2d.pose",
    "live2d.display-info",
    "live2d.expression",
    "live2d.motion",
    "spine.parts",
    "painting.index",
];
const OBJ_LINE_PREFIXES: [&[u8]; 10] = [b"#", b"g ", b"o ", b"v ", b"vt ", b"vn ", b"f ", b"s ", b"mtllib ", b"usemtl "];

/// `_REQUIRED_ASSET_KINDS`: without these the model cannot be rendered.
pub(crate) const REQUIRED_ASSET_KINDS: [&str; 7] = [
    "live2d.model3",
    "live2d.moc3",
    "live2d.texture",
    "spine.skel",
    "spine.atlas",
    "spine.texture",
    "painting.image",
];

/// `_ASSET_KIND_ORDER`.
pub(crate) const ASSET_KIND_ORDER: [&str; 21] = [
    "live2d.model3",
    "live2d.moc3",
    "live2d.texture",
    "live2d.physics",
    "live2d.pose",
    "live2d.display-info",
    "live2d.expression",
    "live2d.motion",
    "live2d.audio",
    "spine.parts",
    "spine.skel",
    "spine.atlas",
    "spine.texture",
    "painting.index",
    "painting.image",
    "painting.layer",
    "painting.mesh",
    "painting.face",
    "icon.square",
    "icon.shipyard",
    "voice.audio",
];

/// `AzurLaneAsset`: one URL in one role, with every place in the model it came from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Asset {
    pub(crate) url: String,
    pub(crate) kind: String,
    pub(crate) fallback_url: String,
    pub(crate) local_path: String,
    pub(crate) original_filename: String,
    pub(crate) content_type: String,
    pub(crate) size: i64,
    pub(crate) sha256: String,
    pub(crate) status: String,
    pub(crate) error: String,
    pub(crate) downloaded_url: String,
    pub(crate) contexts: Vec<Dict>,
}

impl Asset {
    pub(crate) fn new(url: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            kind: kind.into(),
            fallback_url: String::new(),
            local_path: String::new(),
            original_filename: String::new(),
            content_type: String::new(),
            size: 0,
            sha256: String::new(),
            status: "pending".to_owned(),
            error: String::new(),
            downloaded_url: String::new(),
            contexts: Vec::new(),
        }
    }

    pub(crate) fn is_required(&self) -> bool {
        REQUIRED_ASSET_KINDS.contains(&self.kind.as_str())
    }
}

/// `_retry_cooldown_days(failed_count)`: 1, 3, then 7 days.
pub(crate) fn retry_cooldown_days(failed_count: i64) -> i64 {
    match failed_count {
        ..=1 => ASSET_RETRY_COOLDOWN_DAYS[0],
        2 => ASSET_RETRY_COOLDOWN_DAYS[1],
        _ => ASSET_RETRY_COOLDOWN_DAYS[2],
    }
}

/// `_max_attempts_for_status(status_code)`.
pub(crate) fn max_attempts_for_status(status: u16) -> u32 {
    match status {
        404 => 1,
        403 | 406 => LIMITED_RETRY_ATTEMPTS,
        408 | 429 => MAX_RETRIES,
        status if status >= 500 => MAX_RETRIES,
        _ => 1,
    }
}

/// `_validate_asset_response(kind=..., content_type=..., body_prefix=..., size=...)`: why the
/// body is not the file asked for, or `""`.
pub(crate) fn validate_asset_response(kind: &str, content_type: &str, body_prefix: &[u8], size: i64) -> &'static str {
    if size <= 0 {
        return "empty response body";
    }
    let stripped = lstrip_bytes(body_prefix);
    // The mesh check comes before the rejection check: the SPA answers a mesh it does not
    // host with its own HTML shell, which would otherwise trip the circuit breaker.
    if kind == "painting.mesh" {
        let lowered = stripped.to_ascii_lowercase();
        if !OBJ_LINE_PREFIXES.iter().any(|prefix| lowered.starts_with(prefix)) {
            return MESH_UNAVAILABLE_REASON;
        }
    }
    if is_cdn_rejection_body(content_type, body_prefix) {
        return CDN_REJECTION_REASON;
    }
    if JSON_ASSET_KINDS.contains(&kind) && !stripped.is_empty() && !stripped.starts_with(b"{") {
        return "expected JSON object response";
    }
    json_error_body_reason(stripped)
}

/// `_json_error_body_reason(stripped)`.
fn json_error_body_reason(stripped: &[u8]) -> &'static str {
    if !stripped.starts_with(b"{") {
        return "";
    }
    match json_loads(&decode_utf8_ignore(stripped)) {
        Ok(Value::Dict(parsed)) if ["code", "msg", "message", "error"].iter().any(|key| parsed.contains_key(key)) => "json error response",
        _ => "",
    }
}

/// `_download_candidates(asset)`: the URL, its fallback, then case variants of the URL
/// (not for hashed names, which have no other spelling).
pub(crate) fn download_candidates(asset: &Asset) -> Result<Vec<String>, PyError> {
    let mut candidates = vec![asset.url.clone()];
    if !asset.fallback_url.is_empty() && !candidates.contains(&asset.fallback_url) {
        candidates.push(asset.fallback_url.clone());
    }
    if !is_hashed_asset_url(&asset.url) {
        for url in case_variant_urls(&asset.url)? {
            if !candidates.contains(&url) {
                candidates.push(url);
            }
        }
    }
    Ok(candidates)
}

/// `_character_root(base_dir, entry)`.
pub(crate) fn character_root(base_dir: &str, entry: &ModelEntry) -> String {
    let character = &entry.character;
    let name = [&character.name_en, &character.name_zh, &character.key]
        .into_iter()
        .find(|name| !name.is_empty())
        .map_or("", String::as_str);
    let mut safe_name = sanitize(name, 120);
    if safe_name.is_empty() {
        safe_name.clone_from(&character.key);
    }
    pypath::join(base_dir, &format!("{} - {safe_name}", character.key))
}

/// `Path(urlsplit(url).path).name`.
fn url_filename(url: &str) -> Result<String, PyError> {
    let parts = fav_core::pyurl::urlsplit(url, "").map_err(|error| PyError::new("ValueError", error.0))?;
    Ok(pypath::name(&parts.path).to_owned())
}

fn model_key(entry: &ModelEntry) -> String {
    let key = sanitize(&entry.costume.key, 120);
    if key.is_empty() { "unknown".to_owned() } else { key }
}

fn seed_context(entry: &ModelEntry, extra: &[(&str, &str)]) -> Dict {
    let mut context = Dict::new();
    context.insert("model_id", Value::Str(entry.id.clone()));
    context.insert("model_type", Value::Str(entry.r#type.as_str().to_owned()));
    context.insert("character_key", Value::Str(entry.character.key.clone()));
    context.insert("costume_key", Value::Str(entry.costume.key.clone()));
    context.insert("catalog_source", Value::Str(entry.source.as_str().to_owned()));
    context.insert("source_model_url", Value::Str(entry.resources.primary_url.clone()));
    for (key, value) in extra {
        context.insert(*key, Value::Str((*value).to_owned()));
    }
    context
}

/// `_model3_seed_asset(entry)`.
pub(crate) fn model3_seed_asset(entry: &ModelEntry) -> Result<Asset, PyError> {
    let mut filename = url_filename(&entry.resources.primary_url)?;
    if filename.is_empty() {
        filename = format!("{}.model3.json", entry.costume.key);
    }
    let mut asset = Asset::new(entry.resources.primary_url.clone(), "live2d.model3");
    asset.fallback_url.clone_from(&entry.resources.fallback_url);
    asset.local_path = pypath::join(&pypath::join("assets/live2d", &model_key(entry)), &filename);
    asset.contexts = vec![seed_context(
        entry,
        &[("fallback_model_url", &entry.resources.fallback_url), ("live2d_field", "model3")],
    )];
    asset.original_filename = filename;
    Ok(asset)
}

/// `_spine_parts_seed_asset(entry)`.
pub(crate) fn spine_parts_seed_asset(entry: &ModelEntry) -> Result<Asset, PyError> {
    let url = spine_parts_manifest_url(&entry.resources.primary_url)?;
    let filename = url_filename(&url)?;
    let mut asset = Asset::new(url, "spine.parts");
    asset.local_path = pypath::join(&pypath::join("assets/spine", &model_key(entry)), &filename);
    asset.contexts = vec![seed_context(entry, &[("spine_field", "parts")])];
    asset.original_filename = filename;
    Ok(asset)
}

/// `_painting_index_seed_asset(entry)`.
pub(crate) fn painting_index_seed_asset(entry: &ModelEntry) -> Result<Asset, PyError> {
    let url = entry_painting_index_url(entry)?;
    let filename = url_filename(&url)?;
    let mut asset = Asset::new(url, "painting.index");
    asset.local_path = pypath::join(&pypath::join("assets/painting", &model_key(entry)), &filename);
    asset.contexts = vec![seed_context(entry, &[("painting_field", "index")])];
    asset.original_filename = filename;
    Ok(asset)
}

/// `_assets_from_enumeration(enumeration)`: one asset per (URL, kind), its contexts merged.
pub(crate) fn assets_from_enumeration(enumeration: &AzurLaneModelResourceEnumeration) -> Vec<Asset> {
    let mut assets: Vec<Asset> = Vec::new();
    for resource in &enumeration.assets {
        let kind = resource.kind.as_str();
        if let Some(existing) = assets
            .iter_mut()
            .find(|asset| asset.url == resource.source_url && asset.kind == kind)
        {
            existing.contexts.extend(resource.contexts.iter().cloned());
            if existing.fallback_url.is_empty() {
                existing.fallback_url.clone_from(&resource.fallback_url);
            }
            if existing.original_filename.is_empty() {
                existing.original_filename.clone_from(&resource.original_filename);
            }
            if existing.local_path.is_empty() {
                existing.local_path.clone_from(&resource.local_path);
            }
            continue;
        }
        let mut asset = Asset::new(resource.source_url.clone(), kind);
        asset.fallback_url.clone_from(&resource.fallback_url);
        asset.local_path.clone_from(&resource.local_path);
        asset.original_filename.clone_from(&resource.original_filename);
        asset.contexts.clone_from(&resource.contexts);
        assets.push(asset);
    }
    assets
}

/// `_merge_character_metadata(metadata, character)`: fill what the metadata lacks.
pub(crate) fn merge_character_metadata(metadata: &mut Dict, character: &ModelCharacter) {
    let lacks = |metadata: &Dict, key: &str| !metadata.get(key).is_some_and(Value::is_truthy);
    for (key, value) in [
        ("nation", &character.nation),
        ("ship_type", &character.ship_type),
        ("rarity", &character.rarity),
        ("class_name", &character.class_name),
    ] {
        if !value.is_empty() && lacks(metadata, key) {
            metadata.insert(key, Value::Str(value.clone()));
        }
    }
    if let Some(default_skin_id) = character.default_skin_id
        && lacks(metadata, "default_skin_id")
    {
        metadata.insert("default_skin_id", Value::Int(default_skin_id));
    }
    if !character.skin_series.is_empty() && lacks(metadata, "skin_series") {
        metadata.insert(
            "skin_series",
            Value::List(character.skin_series.iter().cloned().map(Value::Str).collect()),
        );
    }
}

/// `_source_snapshots_complete(snapshots)`: no source error, and l2d.su listed a model.
pub(crate) fn source_snapshots_complete(snapshots: &AzurLaneSourceSnapshots) -> bool {
    let models: usize = snapshots
        .l2d_su
        .characters
        .iter()
        .map(|character| character.live2d.len() + character.spine.len())
        .sum();
    snapshots.l2d_su.errors.is_empty() && snapshots.nagami.errors.is_empty() && models > 0
}

/// `_source_failure_message(snapshots, catalog)`: why the run left the catalog alone.
pub(crate) fn source_failure_message(snapshots: &AzurLaneSourceSnapshots, catalog: &AzurLaneModelCatalog) -> String {
    let reason = if catalog.entries.is_empty() {
        "source catalog returned no model entries"
    } else {
        "source snapshots incomplete"
    };
    let detail: Vec<String> = snapshots
        .l2d_su
        .errors
        .iter()
        .chain(&snapshots.nagami.errors)
        .take(3)
        .map(|error| format!("{} {}: {}", error.url, error.kind.as_str(), error.message))
        .collect();
    let message = format!("Azur Lane {reason}; preserving existing catalog state");
    if detail.is_empty() {
        message
    } else {
        format!("{message}: {}", detail.join("; "))
    }
}

/// `_check_archive_continuity(catalog, archived)`: refuse a catalog that keeps none of the
/// archived models, which would download the whole archive again under new names.
pub(crate) fn check_archive_continuity(catalog: &AzurLaneModelCatalog, archived: &[ArchivedModel]) -> Result<(), JobError> {
    let archived_ids: std::collections::HashSet<&str> = archived
        .iter()
        .filter(|model| model.completed)
        .map(|model| model.entry.id.as_str())
        .collect();
    let kept = catalog
        .entries
        .iter()
        .filter(|entry| archived_ids.contains(entry.id.as_str()))
        .count();
    super::log().info(format_args!(
        "Azur Lane catalog: {} entries, {kept} of {} completed archive models kept",
        catalog.entries.len(),
        archived_ids.len()
    ));
    if !archived_ids.is_empty() && kept == 0 {
        return Err(JobError::new(
            "CrawlRunError",
            format!(
                "Azur Lane catalog matches none of the {} archived models; refusing to download the archive again",
                archived_ids.len()
            ),
        ));
    }
    Ok(())
}
