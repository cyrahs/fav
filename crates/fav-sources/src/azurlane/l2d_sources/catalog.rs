//! The model catalog: built from the two sources, pinned to what is archived, searched; plus
//! the l2d.su URL helpers.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::BuildHasher;

use fav_core::pyjson::{Dict, Value};
use fav_core::pypath;
use fav_core::pytext::{is_py_decimal, py_isdigit, py_split, py_strip};

use super::model::{
    ArchivedModel, AzurLaneModelCatalog, AzurLaneSourceSnapshots, CatalogSource, L2DSuCharacterSnapshot, L2DSuModelSnapshot,
    L2DSuSourceSnapshot, ModelCharacter, ModelCostume, ModelEntry, ModelKind, ModelResources, NagamiFallbackCandidate, NagamiMappingEntry,
    NagamiSourceSnapshot,
};
use super::parse::{spine_parts_manifest_url, spine_resource_manifest};
use super::{
    CATALOG_VARIANT_TOKEN_SIZE, L2D_SU_ARCHIVED_ASSET_BASE_URLS, L2D_SU_ASSET_BASE_URL, L2D_SU_AUDIO_BASE_URL, L2D_SU_PRIMARY_REGION,
    L2D_SU_STATIC_BASE_URL, NAGAMI_LIVE2D_BASE_URL, blake2b_hex, catalog_entry_id, catalog_key, nfkc, split,
};
use crate::PyError;

// ---------- URLs ----------

/// `l2d_su_ship_index_url`.
pub fn l2d_su_ship_index_url(region: &str) -> String {
    format!("https://static.l2d.su/al/data/ships-{region}.json")
}

/// `l2d_su_ship_index_url()` for the primary region.
pub fn l2d_su_primary_ship_index_url() -> String {
    l2d_su_ship_index_url(L2D_SU_PRIMARY_REGION)
}

/// `l2d_su_ship_detail_url`.
pub fn l2d_su_ship_detail_url(ship_id: i128, region: &str) -> String {
    format!("https://static.l2d.su/al/data/ships/{region}/{ship_id}.json")
}

/// `_l2d_su_static_url`: a path below the asset root; an absolute URL stays as it is.
pub fn l2d_su_static_url(path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_owned();
    }
    format!("{L2D_SU_STATIC_BASE_URL}/{}", path.trim_start_matches('/'))
}

pub fn l2d_su_painting_url(painting_key: &str) -> String {
    format!("{L2D_SU_STATIC_BASE_URL}/painting/{painting_key}.webp")
}

pub fn l2d_su_painting_index_url(painting_key: &str) -> String {
    format!("{L2D_SU_STATIC_BASE_URL}/painting/{painting_key}.json")
}

pub fn l2d_su_painting_mesh_url(layer_key: &str) -> String {
    format!("{L2D_SU_STATIC_BASE_URL}/painting/{layer_key}-mesh.obj")
}

pub fn l2d_su_painting_face_url(painting_key: &str, face_id: &str) -> String {
    format!("{L2D_SU_STATIC_BASE_URL}/paintingface/{painting_key}/{face_id}.webp")
}

/// `l2d_su_icon_url`: an icon by its index path; the hashed build already gives an absolute URL.
pub fn l2d_su_icon_url(icon_path: &str) -> String {
    if icon_path.starts_with("https://") || icon_path.starts_with("http://") {
        return icon_path.to_owned();
    }
    format!("{L2D_SU_STATIC_BASE_URL}/{icon_path}.webp")
}

pub fn l2d_su_voice_url(voice_path: &str) -> String {
    format!("{L2D_SU_AUDIO_BASE_URL}/{voice_path}.ogg")
}

/// `is_hashed_asset_url`: whether `url` names a file of the hashed /al/ build
/// (`<16 hex>/<32 hex>.<ext>`). Those names never existed under an older root.
pub fn is_hashed_asset_url(url: &str) -> bool {
    let prefix = format!("{L2D_SU_ASSET_BASE_URL}/");
    if !url.starts_with(&prefix) {
        return false;
    }
    // The prefix fixes the netloc, so urlsplit cannot refuse the URL.
    let Ok(parts) = split(url) else { return false };
    let path = parts.path.strip_prefix("/al/assets/").unwrap_or(&parts.path);
    let bytes = path.as_bytes();
    let hex = |range: &[u8]| range.iter().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    bytes.len() >= 50 && hex(&bytes[..16]) && bytes[16] == b'/' && hex(&bytes[17..49]) && bytes[49] == b'.'
}

/// `l2d_su_legacy_asset_urls`: the URLs `url` may have been archived under before the CDN
/// moved, newest root first; empty if `url` is not an l2d.su asset.
pub fn l2d_su_legacy_asset_urls(url: &str) -> Vec<String> {
    let prefix = format!("{L2D_SU_ASSET_BASE_URL}/");
    let Some(relative_path) = url.strip_prefix(&prefix) else {
        return Vec::new();
    };
    if is_hashed_asset_url(url) {
        return Vec::new();
    }
    L2D_SU_ARCHIVED_ASSET_BASE_URLS
        .iter()
        .map(|base_url| format!("{base_url}/{relative_path}"))
        .collect()
}

/// `l2d_su_asset_url_from_legacy`: move an archived model URL from an old CDN root to the
/// current one.
pub fn l2d_su_asset_url_from_legacy(url: &str) -> String {
    for base_url in L2D_SU_ARCHIVED_ASSET_BASE_URLS {
        if let Some(rest) = url.strip_prefix(&format!("{base_url}/")) {
            return format!("{L2D_SU_ASSET_BASE_URL}/{rest}");
        }
    }
    url.to_owned()
}

fn segment_case_variants(segment: &str) -> Vec<String> {
    let lowered = segment.to_lowercase();
    let mut chars = segment.chars();
    let capitalised = chars
        .next()
        .map(|first| format!("{}{}", first.to_uppercase(), chars.as_str()))
        .unwrap_or_default();
    let mut variants: Vec<String> = Vec::new();
    for variant in [lowered, capitalised] {
        if !variant.is_empty() && variant != segment && !variants.contains(&variant) {
            variants.push(variant);
        }
    }
    variants
}

/// `case_variant_urls`: URLs differing from `url` only in the case of the segment carrying
/// the model key (the filename, or the directory above a numbered face file).
pub fn case_variant_urls(url: &str) -> Result<Vec<String>, PyError> {
    let parts = split(url)?;
    let Some((head, filename)) = parts.path.rsplit_once('/') else {
        return Ok(Vec::new());
    };
    let paths: Vec<String> = if py_isdigit(pypath::stem(filename)) {
        let Some((parent_head, parent)) = head.rsplit_once('/') else {
            return Ok(Vec::new());
        };
        segment_case_variants(parent)
            .into_iter()
            .map(|variant| format!("{parent_head}/{variant}/{filename}"))
            .collect()
    } else {
        segment_case_variants(filename)
            .into_iter()
            .map(|variant| format!("{head}/{variant}"))
            .collect()
    };
    let mut urls: Vec<String> = Vec::new();
    for path in paths {
        let mut variant = parts.clone();
        variant.path = path;
        let url = fav_core::pyurl::urlunsplit(&variant);
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    Ok(urls)
}

// ---------- per-model helpers ----------

/// `l2d_su_character_fingerprint`: a stable digest of a ship's index entry, used to tell
/// when its detail payload is stale.
pub fn l2d_su_character_fingerprint(character: &L2DSuCharacterSnapshot) -> String {
    let mut models: Vec<&L2DSuModelSnapshot> = character.models().collect();
    models.sort_by(|a, b| (a.kind.as_str(), a.costume_id, &a.model_key).cmp(&(b.kind.as_str(), b.costume_id, &b.model_key)));
    let mut parts = vec![format!("{}:{}:{}", character.char_id, character.char_key, character.char_name)];
    parts.extend(models.iter().map(|model| {
        format!(
            "{}:{}:{}:{}:{}",
            model.kind.as_str(),
            model.costume_id,
            model.model_key,
            model.costume_name,
            model.face_ids.join(",")
        )
    }));
    blake2b_hex(&parts.join("|"), 16)
}

/// `model_probe_urls`: URLs that prove a model's assets exist, cheapest first.
pub fn model_probe_urls(entry: &ModelEntry) -> Result<Vec<String>, PyError> {
    if matches!(entry.r#type, ModelKind::Live2d | ModelKind::Painting) || is_hashed_asset_url(&entry.resources.primary_url) {
        return Ok(vec![entry.resources.primary_url.clone()]);
    }
    let manifest = spine_resource_manifest(&entry.resources.primary_url, None)?;
    Ok(vec![
        manifest.parts[0].skeleton_url.clone(),
        spine_parts_manifest_url(&entry.resources.primary_url)?,
    ])
}

/// `apply_l2d_su_ship_classes`: attach ship class names, which only the per-ship detail has.
pub fn apply_l2d_su_ship_classes<S: BuildHasher>(
    catalog: &AzurLaneModelCatalog,
    classes: &HashMap<i128, String, S>,
) -> AzurLaneModelCatalog {
    let mut updated = catalog.clone();
    for entry in &mut updated.entries {
        let Some(id) = entry.character.id else { continue };
        match classes.get(&id) {
            Some(class_name) if !class_name.is_empty() && *class_name != entry.character.class_name => {
                entry.character.class_name.clone_from(class_name);
            }
            _ => {}
        }
    }
    updated
}

/// `apply_l2d_su_model_paths`: the model URLs the per-ship detail names, by costume id.
pub fn apply_l2d_su_model_paths<S: BuildHasher>(catalog: &AzurLaneModelCatalog, paths: &HashMap<i128, String, S>) -> AzurLaneModelCatalog {
    let mut updated = catalog.clone();
    for entry in &mut updated.entries {
        if entry.r#type == ModelKind::Painting || entry.source == CatalogSource::Nagami {
            continue;
        }
        let Some(id) = entry.costume.id else { continue };
        match paths.get(&id) {
            Some(resolved) if !resolved.is_empty() && *resolved != entry.resources.primary_url => {
                entry.resources.primary_url.clone_from(resolved);
            }
            _ => {}
        }
    }
    updated
}

fn path_segments(path: &str) -> Result<Vec<String>, PyError> {
    Ok(split(path)?
        .path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect())
}

/// `_l2d_su_model_key`.
pub(crate) fn l2d_su_model_key(model: &L2DSuModelSnapshot) -> Result<String, PyError> {
    if model.hashed {
        return Ok(model.model_key.clone());
    }
    let segments = path_segments(&model.path)?;
    let mut filename = segments.last().cloned().unwrap_or_default();
    if model.kind == ModelKind::Live2d {
        if segments.len() > 1 && !segments[segments.len() - 2].is_empty() {
            return Ok(catalog_key(&segments[segments.len() - 2]));
        }
        filename = filename.strip_suffix(".model3.json").unwrap_or(&filename).to_owned();
    }
    if model.kind == ModelKind::Painting {
        filename = filename.strip_suffix(".webp").unwrap_or(&filename).to_owned();
    }
    if !filename.is_empty() {
        return Ok(catalog_key(&filename));
    }
    Ok(format!("costume-{}", model.costume_id))
}

/// `_l2d_su_fallback_match_key`: the nagami key a readable Live2D model would have.
fn l2d_su_fallback_match_key(model: &L2DSuModelSnapshot) -> Result<String, PyError> {
    if model.kind != ModelKind::Live2d || model.hashed {
        return Ok(String::new());
    }
    let segments = path_segments(&model.path)?;
    let Some((filename, parents)) = segments.split_last() else {
        return Ok(String::new());
    };
    let Some(parent) = parents.last() else {
        return Ok(String::new());
    };
    let parent = catalog_key(parent);
    let Some(stem) = filename.strip_suffix(".model3.json") else {
        return Ok(String::new());
    };
    let model_stem = catalog_key(stem);
    Ok(if parent == model_stem { model_stem } else { String::new() })
}

/// `_nagami_character_key`: the key without its `_<digits>` costume suffix.
pub(crate) fn nagami_character_key(key: &str) -> String {
    // `re.sub(r'_\d+$', '', key)`: `\d` is any decimal digit, `$` also matches before a
    // final newline.
    let (body, newline) = key.strip_suffix('\n').map_or((key, ""), |body| (body, "\n"));
    let digits = body
        .chars()
        .rev()
        .take_while(|ch| is_py_decimal(*ch))
        .map(char::len_utf8)
        .sum::<usize>();
    let stripped = if digits > 0 && body[..body.len() - digits].ends_with('_') {
        format!("{}{newline}", &body[..body.len() - digits - 1])
    } else {
        key.to_owned()
    };
    if stripped.is_empty() { key.to_owned() } else { stripped }
}

fn split_nagami_name(name: &str) -> (String, String) {
    match name.split_once(" - ") {
        Some((character, costume)) => (py_strip(character).to_owned(), py_strip(costume).to_owned()),
        None => (py_strip(name).to_owned(), String::new()),
    }
}

fn nagami_model_url(key: &str) -> String {
    format!("{NAGAMI_LIVE2D_BASE_URL}/{key}/{key}.model3.json")
}

fn nagami_fallback_candidate(entry: &NagamiMappingEntry) -> NagamiFallbackCandidate {
    let key = catalog_key(&entry.key);
    let (character_name, costume_name) = split_nagami_name(&entry.name);
    NagamiFallbackCandidate {
        character_key: nagami_character_key(&key),
        costume_key: key.clone(),
        name: entry.name.clone(),
        character_name_en: character_name,
        costume_name_en: costume_name,
        url: nagami_model_url(&key),
        key,
    }
}

/// `build_nagami_fallback_candidates`.
pub fn build_nagami_fallback_candidates(snapshot: &NagamiSourceSnapshot) -> Vec<NagamiFallbackCandidate> {
    let mut entries: Vec<&NagamiMappingEntry> = snapshot.entries.iter().collect();
    entries.sort_by_cached_key(|entry| catalog_key(&entry.key));
    entries.into_iter().map(nagami_fallback_candidate).collect()
}

fn l2d_su_entry(
    character: &L2DSuCharacterSnapshot,
    model: &L2DSuModelSnapshot,
    nagami_by_key: &HashMap<String, &NagamiFallbackCandidate>,
) -> Result<(ModelEntry, String), PyError> {
    let model_key = l2d_su_model_key(model)?;
    let fallback_match_key = l2d_su_fallback_match_key(model)?;
    let fallback = if fallback_match_key.is_empty() {
        None
    } else {
        nagami_by_key.get(&fallback_match_key).copied()
    };
    let entry = ModelEntry::new(
        catalog_entry_id(model.kind, &character.char_key, &model_key),
        model.kind,
        if fallback.is_some() {
            CatalogSource::Merged
        } else {
            CatalogSource::L2dSu
        },
        ModelCharacter {
            id: Some(character.char_id),
            key: catalog_key(&character.char_key),
            name_zh: character.char_name.clone(),
            name_en: character.char_name_en.clone(),
            nation: character.nation.clone(),
            ship_type: character.ship_type.clone(),
            rarity: character.rarity.clone(),
            class_name: String::new(),
            default_skin_id: character.default_skin_id,
            skin_series: character.skin_series.clone(),
        },
        ModelCostume {
            id: Some(model.costume_id),
            key: catalog_key(&model_key),
            name_zh: model.costume_name.clone(),
            name_en: model.costume_name_en.clone(),
            skin_type: model.skin_type.clone(),
            feature_tags: model.feature_tags.clone(),
            is_live2d_plus: model.is_live2d_plus,
            face_ids: model.face_ids.clone(),
            square_icon: model.square_icon.clone(),
            shipyard_icon: model.shipyard_icon.clone(),
            q_icon: model.q_icon.clone(),
            shop_type: model.shop_type.clone(),
            face_urls: model.face_urls.clone(),
        },
        ModelResources {
            primary_url: model.path.clone(),
            fallback_url: fallback.map(|candidate| candidate.url.clone()).unwrap_or_default(),
            display_info_url: String::new(),
            painting_index_url: model.painting_index_url.clone(),
        },
    );
    Ok((entry, fallback.map(|candidate| candidate.key.clone()).unwrap_or_default()))
}

fn nagami_entry(candidate: &NagamiFallbackCandidate) -> ModelEntry {
    ModelEntry::new(
        catalog_entry_id(ModelKind::Live2d, &candidate.character_key, &candidate.costume_key),
        ModelKind::Live2d,
        CatalogSource::Nagami,
        ModelCharacter {
            key: candidate.character_key.clone(),
            name_en: candidate.character_name_en.clone(),
            ..ModelCharacter::default()
        },
        ModelCostume {
            key: candidate.costume_key.clone(),
            name_en: candidate.costume_name_en.clone(),
            ..ModelCostume::default()
        },
        ModelResources {
            primary_url: candidate.url.clone(),
            ..ModelResources::default()
        },
    )
}

/// The catalog's `entries_by_id`; Python sorts its keys at the end, a `BTreeMap` keeps
/// them in that order (UTF-8 byte order is code point order).
type EntriesById = BTreeMap<String, ModelEntry>;

fn merge_same_asset_entries(existing: &ModelEntry, incoming: &ModelEntry) -> ModelEntry {
    let fallback_url = if existing.resources.fallback_url.is_empty() {
        incoming.resources.fallback_url.clone()
    } else {
        existing.resources.fallback_url.clone()
    };
    let mut merged = existing.clone();
    merged.source = if fallback_url.is_empty() {
        existing.source
    } else {
        CatalogSource::Merged
    };
    merged.resources.fallback_url = fallback_url;
    merged
}

fn variant_entry_id(entry: &ModelEntry, entries_by_id: &EntriesById) -> String {
    let token = blake2b_hex(&entry.resources.primary_url, CATALOG_VARIANT_TOKEN_SIZE);
    let mut variant_id = format!("{}:asset-{token}", entry.id);
    let mut index = 2;
    while entries_by_id
        .get(&variant_id)
        .is_some_and(|existing| existing.resources.primary_url != entry.resources.primary_url)
    {
        variant_id = format!("{}:asset-{token}-{index}", entry.id);
        index += 1;
    }
    variant_id
}

fn add_catalog_entry(entries_by_id: &mut EntriesById, entry: ModelEntry) {
    let Some(existing) = entries_by_id.get(&entry.id) else {
        entries_by_id.insert(entry.id.clone(), entry);
        return;
    };
    if existing.resources.primary_url == entry.resources.primary_url {
        let merged = merge_same_asset_entries(existing, &entry);
        entries_by_id.insert(entry.id.clone(), merged);
        return;
    }
    let mut variant = entry;
    variant.id = variant_entry_id(&variant, entries_by_id);
    if let Some(existing) = entries_by_id.get(&variant.id)
        && existing.resources.primary_url == variant.resources.primary_url
    {
        let merged = merge_same_asset_entries(existing, &variant);
        entries_by_id.insert(variant.id.clone(), merged);
        return;
    }
    entries_by_id.insert(variant.id.clone(), variant);
}

/// `build_azurlane_model_catalog`.
pub fn build_azurlane_model_catalog(snapshots: &AzurLaneSourceSnapshots) -> Result<AzurLaneModelCatalog, PyError> {
    let nagami_candidates = build_nagami_fallback_candidates(&snapshots.nagami);
    let mut nagami_by_key: HashMap<String, &NagamiFallbackCandidate> = HashMap::new();
    for candidate in &nagami_candidates {
        nagami_by_key.insert(candidate.key.clone(), candidate);
    }
    let mut matched_nagami_keys: HashSet<String> = HashSet::new();
    let mut entries_by_id = EntriesById::new();

    let mut characters: Vec<&L2DSuCharacterSnapshot> = snapshots.l2d_su.characters.iter().collect();
    characters.sort_by_cached_key(|character| (catalog_key(&character.char_key), character.char_id));
    for character in characters {
        let mut models = Vec::new();
        for model in character.models() {
            models.push((
                (model.kind.as_str(), l2d_su_model_key(model)?, model.costume_id, model.path.clone()),
                model,
            ));
        }
        models.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, model) in models {
            let (entry, matched_key) = l2d_su_entry(character, model, &nagami_by_key)?;
            add_catalog_entry(&mut entries_by_id, entry);
            if !matched_key.is_empty() {
                matched_nagami_keys.insert(matched_key);
            }
        }
    }
    for candidate in &nagami_candidates {
        if !matched_nagami_keys.contains(&candidate.key) {
            add_catalog_entry(&mut entries_by_id, nagami_entry(candidate));
        }
    }
    Ok(AzurLaneModelCatalog {
        entries: entries_by_id.into_values().collect(),
        nagami_fallback_candidates: nagami_candidates,
    })
}

// ---------- rebuilding archived entries ----------

/// A field `_dataclass_from_dict` would pass through: `None` when the payload's value is
/// of a type the Rust field cannot hold (DIVERGENCES.md).
trait FromPy: Sized {
    fn from_py(value: &Value) -> Option<Self>;
}

impl FromPy for String {
    fn from_py(value: &Value) -> Option<Self> {
        value.as_str().map(str::to_owned)
    }
}

impl FromPy for bool {
    fn from_py(value: &Value) -> Option<Self> {
        match value {
            Value::Bool(flag) => Some(*flag),
            _ => None,
        }
    }
}

impl FromPy for Option<i128> {
    fn from_py(value: &Value) -> Option<Self> {
        match value {
            Value::Null => Some(None),
            Value::Int(number) => Some(Some(*number)),
            _ => None,
        }
    }
}

impl FromPy for Vec<String> {
    fn from_py(value: &Value) -> Option<Self> {
        value.as_list()?.iter().map(String::from_py).collect()
    }
}

impl FromPy for Vec<(String, String)> {
    fn from_py(value: &Value) -> Option<Self> {
        value
            .as_list()?
            .iter()
            .map(|pair| match pair.as_list()? {
                [first, second] => Some((String::from_py(first)?, String::from_py(second)?)),
                _ => None,
            })
            .collect()
    }
}

/// `_dataclass_from_dict` for one field: `Some(None)` when the key is absent (the default
/// stays), `None` when the value does not fit.
fn field<T: FromPy>(dict: &Dict, name: &str, slot: &mut T) -> Option<()> {
    if let Some(value) = dict.get(name) {
        *slot = T::from_py(value)?;
    }
    Some(())
}

fn character_from_dict(value: &Value) -> Option<ModelCharacter> {
    let dict = value.as_dict()?;
    // `key` has no default: without it the constructor raises TypeError.
    let mut character = ModelCharacter {
        key: String::from_py(dict.get("key")?)?,
        ..ModelCharacter::default()
    };
    field(dict, "id", &mut character.id)?;
    field(dict, "name_zh", &mut character.name_zh)?;
    field(dict, "name_en", &mut character.name_en)?;
    field(dict, "nation", &mut character.nation)?;
    field(dict, "ship_type", &mut character.ship_type)?;
    field(dict, "rarity", &mut character.rarity)?;
    field(dict, "class_name", &mut character.class_name)?;
    field(dict, "default_skin_id", &mut character.default_skin_id)?;
    field(dict, "skin_series", &mut character.skin_series)?;
    Some(character)
}

fn costume_from_dict(value: &Value) -> Option<ModelCostume> {
    let dict = value.as_dict()?;
    let mut costume = ModelCostume {
        key: String::from_py(dict.get("key")?)?,
        ..ModelCostume::default()
    };
    field(dict, "id", &mut costume.id)?;
    field(dict, "name_zh", &mut costume.name_zh)?;
    field(dict, "name_en", &mut costume.name_en)?;
    field(dict, "skin_type", &mut costume.skin_type)?;
    field(dict, "feature_tags", &mut costume.feature_tags)?;
    field(dict, "is_live2d_plus", &mut costume.is_live2d_plus)?;
    field(dict, "face_ids", &mut costume.face_ids)?;
    field(dict, "square_icon", &mut costume.square_icon)?;
    field(dict, "shipyard_icon", &mut costume.shipyard_icon)?;
    field(dict, "q_icon", &mut costume.q_icon)?;
    field(dict, "shop_type", &mut costume.shop_type)?;
    field(dict, "face_urls", &mut costume.face_urls)?;
    Some(costume)
}

fn resources_from_dict(value: &Value) -> Option<ModelResources> {
    let dict = value.as_dict()?;
    let mut resources = ModelResources {
        primary_url: String::from_py(dict.get("primary_url")?)?,
        ..ModelResources::default()
    };
    field(dict, "fallback_url", &mut resources.fallback_url)?;
    field(dict, "display_info_url", &mut resources.display_info_url)?;
    field(dict, "painting_index_url", &mut resources.painting_index_url)?;
    Some(resources)
}

/// `model_entry_from_dict`: rebuild a catalog entry from `ModelEntry.to_dict()` output;
/// `None` if it does not parse. The summary and availability start fresh, as in Python.
pub fn model_entry_from_dict(payload: &Value) -> Option<ModelEntry> {
    let payload = payload.as_dict()?;
    let character = character_from_dict(payload.get("character")?)?;
    let costume = costume_from_dict(payload.get("costume")?)?;
    let resources = resources_from_dict(payload.get("resources")?)?;
    let id = payload.get("id")?.as_str()?.to_owned();
    let kind = ModelKind::parse(payload.get("type")?.as_str()?)?;
    let source = CatalogSource::parse(payload.get("source")?.as_str()?)?;
    Some(ModelEntry::new(id, kind, source, character, costume, resources))
}

/// `pin_archived_entries`: keep archived skins under the names and files they were archived
/// with (matched on costume id and model type); a new skin of an archived ship joins that
/// ship's directory; nagami entries stay only where they are already archived on their own.
pub fn pin_archived_entries(catalog: &AzurLaneModelCatalog, archived: &[ArchivedModel]) -> AzurLaneModelCatalog {
    let mut ordered: Vec<&ArchivedModel> = archived.iter().collect();
    ordered.sort_by(|a, b| {
        (!a.completed, a.entry.id.contains(":asset-"), &a.entry.id).cmp(&(!b.completed, b.entry.id.contains(":asset-"), &b.entry.id))
    });
    let mut by_costume: HashMap<(i128, ModelKind), &ArchivedModel> = HashMap::new();
    let mut character_keys: HashMap<i128, String> = HashMap::new();
    let mut nagami_ids: HashSet<&str> = HashSet::new();
    for model in ordered {
        let entry = &model.entry;
        if entry.source == CatalogSource::Nagami {
            nagami_ids.insert(&entry.id);
            continue;
        }
        if let Some(id) = entry.costume.id {
            by_costume.entry((id, entry.r#type)).or_insert(model);
        }
        if let Some(id) = entry.character.id {
            character_keys.entry(id).or_insert_with(|| entry.character.key.clone());
        }
    }

    let mut entries_by_id = EntriesById::new();
    for entry in &catalog.entries {
        if entry.source == CatalogSource::Nagami {
            if nagami_ids.contains(entry.id.as_str()) {
                add_catalog_entry(&mut entries_by_id, entry.clone());
            }
            continue;
        }
        add_catalog_entry(&mut entries_by_id, pinned_entry(entry, &by_costume, &character_keys));
    }
    AzurLaneModelCatalog {
        entries: entries_by_id.into_values().collect(),
        nagami_fallback_candidates: catalog.nagami_fallback_candidates.clone(),
    }
}

fn pinned_entry(
    entry: &ModelEntry,
    by_costume: &HashMap<(i128, ModelKind), &ArchivedModel>,
    character_keys: &HashMap<i128, String>,
) -> ModelEntry {
    let archived = entry.costume.id.and_then(|id| by_costume.get(&(id, entry.r#type)).copied());
    let Some(archived) = archived else {
        let character_key = entry.character.id.and_then(|id| character_keys.get(&id));
        let Some(character_key) = character_key.filter(|key| !key.is_empty() && **key != entry.character.key) else {
            return entry.clone();
        };
        let mut moved = entry.clone();
        moved.id = catalog_entry_id(entry.r#type, character_key, &entry.costume.key);
        moved.character.key.clone_from(character_key);
        return moved;
    };

    let previous = &archived.entry;
    let mut pinned = entry.clone();
    pinned.id.clone_from(&previous.id);
    pinned.character.key.clone_from(&previous.character.key);
    pinned.costume.key.clone_from(&previous.costume.key);
    if !archived.completed || is_hashed_asset_url(&previous.resources.primary_url) {
        return pinned;
    }
    pinned.source = previous.source;
    pinned.costume.face_ids.clone_from(&previous.costume.face_ids);
    pinned.costume.face_urls.clone_from(&previous.costume.face_urls);
    pinned.costume.square_icon.clone_from(&previous.costume.square_icon);
    pinned.costume.shipyard_icon.clone_from(&previous.costume.shipyard_icon);
    pinned.costume.q_icon.clone_from(&previous.costume.q_icon);
    pinned.resources = previous.resources.clone();
    pinned
}

// ---------- search ----------

fn normalize_search_text(value: &str) -> String {
    fav_core::pycasefold::casefold(&nfkc(value))
}

fn entry_search_text(entry: &ModelEntry) -> String {
    let values = [
        entry.id.as_str(),
        entry.r#type.as_str(),
        entry.source.as_str(),
        &entry.character.key,
        &entry.character.name_zh,
        &entry.character.name_en,
        &entry.costume.key,
        &entry.costume.name_zh,
        &entry.costume.name_en,
    ];
    let joined: Vec<&str> = values.into_iter().filter(|value| !value.is_empty()).collect();
    normalize_search_text(&joined.join(" "))
}

impl AzurLaneModelCatalog {
    /// `search`: entries whose names and keys contain every term of the query.
    pub fn search(&self, query: &str, model_type: Option<ModelKind>) -> Vec<&ModelEntry> {
        let normalized = normalize_search_text(query);
        let terms: Vec<&str> = py_split(&normalized).filter(|term| !term.is_empty()).collect();
        self.entries
            .iter()
            .filter(|entry| model_type.is_none_or(|kind| entry.r#type == kind))
            .filter(|entry| {
                if terms.is_empty() {
                    return true;
                }
                let haystack = entry_search_text(entry);
                terms.iter().all(|term| haystack.contains(term))
            })
            .collect()
    }
}

/// `_l2d_su_source_entry_ids`.
pub(crate) fn l2d_su_source_entry_ids(snapshot: &L2DSuSourceSnapshot) -> Result<Vec<String>, PyError> {
    let mut ids = std::collections::BTreeSet::new();
    for character in &snapshot.characters {
        for model in character.models() {
            ids.insert(catalog_entry_id(model.kind, &character.char_key, &l2d_su_model_key(model)?));
        }
    }
    Ok(ids.into_iter().collect())
}

/// `_nagami_source_entry_ids`.
pub(crate) fn nagami_source_entry_ids(snapshot: &NagamiSourceSnapshot) -> Vec<String> {
    let ids: std::collections::BTreeSet<String> = snapshot
        .entries
        .iter()
        .map(|entry| {
            let key = catalog_key(&entry.key);
            catalog_entry_id(ModelKind::Live2d, &nagami_character_key(&key), &key)
        })
        .collect();
    ids.into_iter().collect()
}
