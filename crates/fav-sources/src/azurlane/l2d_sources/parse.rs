//! The payload parsers: the l2d.su ship index and per-ship detail, the nagami mapping, the
//! Live2D model3 file, spine part manifests and atlases, and painting indexes.

use fav_core::pyjson::{self, Dict, Key, Value};
use fav_core::pypath;
use fav_core::pytext::{is_py_space, py_strip};
use fav_runtime::pystr::repr_str;

use super::catalog::{is_hashed_asset_url, l2d_su_painting_index_url, l2d_su_static_url};
use super::model::{
    AssetKind, L2DSuCharacterSnapshot, L2DSuIndexData, L2DSuModelSnapshot, L2DSuNewSkin, L2DSuVoiceLine, Live2DResourceManifest,
    ModelEntry, ModelKind, NagamiMappingData, NagamiMappingEntry, PaintingIndex, PaintingLayer, ResourceReference, SpineModelPart,
    SpineResourceManifest,
};
use super::{
    L2D_SU_STATIC_BASE_URL, PAINTING_FACE_SLOT, SPINE_PARTS_MANIFEST_EXTENSION, optional_str, optional_str_of, parse_error, plain_int,
    required_int, required_str, resolve_resource_url, schema_error, split, string_tuple, unique_non_empty,
};
use crate::PyError;

/// A key of a parsed JSON object as text (`json.loads` only makes string keys).
pub(crate) fn key_text(key: &Key) -> &str {
    match key {
        Key::Str(text) => text,
        _ => "",
    }
}

/// `exc.msg` of a `JSONDecodeError`: the message without its position.
pub(crate) fn json_error_msg(error: &pyjson::LoadError, doc: &str) -> String {
    let full = error.python_message(doc);
    full.rsplit_once(": line ").map_or(full.clone(), |(message, _)| message.to_owned())
}

/// `_decode_json_object`.
pub(crate) fn decode_json_object(source: &str, context: &str) -> Result<Dict, PyError> {
    let payload =
        pyjson::loads(source).map_err(|error| parse_error(format!("{context} is not valid JSON: {}", json_error_msg(&error, source))))?;
    match payload {
        Value::Dict(dict) => Ok(dict),
        _ => Err(schema_error(format!("{context} root must be an object"))),
    }
}

/// Python's `str.splitlines()`.
pub(crate) fn py_splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if !matches!(
            ch,
            '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        ) {
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

/// `d[key] = value` on an insertion-ordered list of pairs.
fn insert_ordered<K: PartialEq, V>(pairs: &mut Vec<(K, V)>, key: K, value: V) {
    if let Some(slot) = pairs.iter_mut().find(|(existing, _)| *existing == key) {
        slot.1 = value;
    } else {
        pairs.push((key, value));
    }
}

/// The `ship.skins` list of a per-ship detail payload.
fn detail_skins(source: &str) -> Result<Vec<Value>, PyError> {
    let payload = decode_json_object(source, "l2d.su ship detail")?;
    let Some(Value::Dict(ship)) = payload.get("ship") else {
        return Err(schema_error("l2d.su ship detail must contain a ship object"));
    };
    let Some(Value::List(skins)) = ship.get("skins") else {
        return Err(schema_error("l2d.su ship detail must contain a skins list"));
    };
    Ok(skins.clone())
}

/// `parse_l2d_su_ship_models`: `costume_id` to the authoritative model URL, from a per-ship
/// detail payload (a dict in Python: a repeated id keeps its first place, last value).
pub fn parse_l2d_su_ship_models(source: &str) -> Result<Vec<(i128, String)>, PyError> {
    let mut models = Vec::new();
    for skin in detail_skins(source)? {
        let Value::Dict(skin) = skin else { continue };
        let (Some(Value::Dict(model)), Some(costume_id)) = (skin.get("model"), plain_int(skin.get("id"))) else {
            continue;
        };
        let path = optional_str(model, "path");
        if !path.is_empty() {
            insert_ordered(&mut models, costume_id, l2d_su_static_url(&path));
        }
    }
    Ok(models)
}

/// `parse_l2d_su_ship_voices`: `costume_id` to its voice lines, from a per-ship detail payload.
pub fn parse_l2d_su_ship_voices(source: &str) -> Result<Vec<(i128, Vec<L2DSuVoiceLine>)>, PyError> {
    let mut voices = Vec::new();
    for skin in detail_skins(source)? {
        let Value::Dict(skin) = skin else { continue };
        let Some(costume_id) = plain_int(skin.get("id")) else { continue };
        let mut lines = parse_voice_lines(skin.get("words"), false);
        lines.extend(parse_voice_lines(skin.get("extraWords"), true));
        if !lines.is_empty() {
            insert_ordered(&mut voices, costume_id, lines);
        }
    }
    Ok(voices)
}

fn parse_voice_lines(value: Option<&Value>, is_extra: bool) -> Vec<L2DSuVoiceLine> {
    let Some(Value::List(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(Value::as_dict)
        .filter_map(|item| {
            let voice_path = optional_str(item, "voicePath");
            (!voice_path.is_empty()).then(|| L2DSuVoiceLine {
                key: optional_str(item, "key"),
                voice_name: optional_str(item, "voiceName"),
                resource_key: optional_str(item, "resourceKey"),
                voice_path,
                text: optional_str(item, "text"),
                face_id: optional_str(item, "faceId"),
                l2d_action: optional_str(item, "l2dAction"),
                spine_action: optional_str(item, "spineAction"),
                is_extra,
            })
        })
        .collect()
}

/// `parse_l2d_su_ship_class`: the ship class name; the index does not carry it.
pub fn parse_l2d_su_ship_class(source: &str) -> Result<String, PyError> {
    let payload = decode_json_object(source, "l2d.su ship detail")?;
    let Some(Value::Dict(ship)) = payload.get("ship") else {
        return Err(schema_error("l2d.su ship detail must contain a ship object"));
    };
    Ok(optional_str(ship, "className"))
}

// ---------- the ship index ----------

/// `_HASHED_RESOURCE_KEY_RE.match(text)`: 32 lowercase hex digits (`$` also before a final newline).
fn is_hashed_resource_key(text: &str) -> bool {
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.len() == 32 && text.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// `parse_l2d_su_ship_index`.
pub fn parse_l2d_su_ship_index(source: &str, region: &str) -> Result<L2DSuIndexData, PyError> {
    let payload = decode_json_object(source, "l2d.su ship index")?;
    let Some(Value::List(ships)) = payload.get("ships") else {
        return Err(schema_error("l2d.su ship index must contain a ships list"));
    };
    let locale = optional_str(&payload, "locale");
    let mut characters = Vec::with_capacity(ships.len());
    for (index, item) in ships.iter().enumerate() {
        characters.push(parse_ship(item, index)?);
    }
    let new_skins = parse_new_skins(payload.get("newSkins"))?;
    let filters = match payload.get("filters") {
        Some(Value::Dict(filters)) => filters.clone(),
        _ => Dict::new(),
    };
    let skin_update_history = match payload.get("skinUpdateHistory") {
        Some(Value::List(items)) => items.iter().filter_map(Value::as_dict).cloned().collect(),
        _ => Vec::new(),
    };
    Ok(L2DSuIndexData {
        region: if locale.is_empty() { region.to_owned() } else { locale },
        game_version: optional_str(&payload, "version"),
        generated_at: optional_str(&payload, "generatedAt"),
        ship_count: i128::try_from(ships.len()).unwrap_or(i128::MAX),
        characters,
        new_skins,
        filters,
        skin_update_history,
    })
}

fn parse_ship(item: &Value, index: usize) -> Result<L2DSuCharacterSnapshot, PyError> {
    let context = format!("l2d.su ships[{index}]");
    let Value::Dict(item) = item else {
        return Err(schema_error(format!("{context} must be an object")));
    };
    let models = parse_skins(item, &context)?;
    let char_id = required_int(item, "shipGroupId", &context)?;
    let char_key = character_key(item, &context)?;
    let char_name = required_str(item, "name", &context)?;
    let pick = |kind: ModelKind| models.iter().filter(|model| model.kind == kind).cloned().collect::<Vec<_>>();
    Ok(L2DSuCharacterSnapshot {
        char_id,
        char_key,
        char_name,
        char_name_en: optional_str(item, "englishName"),
        nation: optional_str(item, "nationName"),
        ship_type: optional_str(item, "typeName"),
        rarity: optional_str(item, "rarityName"),
        default_skin_id: plain_int(item.get("defaultSkinId")),
        skin_series: skin_series(item),
        live2d: pick(ModelKind::Live2d),
        spine: pick(ModelKind::Spine),
        paintings: pick(ModelKind::Painting),
    })
}

/// `_l2d_su_character_key`: the ship's resource name, or `ship-<id>` where the hashed build
/// replaced it with a digest.
fn character_key(item: &Dict, context: &str) -> Result<String, PyError> {
    let resource_key = required_str(item, "resourceKey", context)?;
    if is_hashed_resource_key(&resource_key) {
        return Ok(format!("ship-{}", required_int(item, "shipGroupId", context)?));
    }
    Ok(resource_key)
}

/// `_l2d_su_skin_series`: skin series labels, matching the index's `filters.skinSeries`.
fn skin_series(item: &Dict) -> Vec<String> {
    let Some(Value::List(skins)) = item.get("skins") else {
        return Vec::new();
    };
    let mut series: Vec<String> = Vec::new();
    for skin in skins.iter().filter_map(Value::as_dict) {
        // l2d.su labels a skin by its shop series, falling back to the skin type for shopless skins.
        let mut label = optional_str(skin, "shopTypeName");
        if label.is_empty() {
            label = optional_str(skin, "skinTypeName");
        }
        if !label.is_empty() && !series.contains(&label) {
            series.push(label);
        }
    }
    series
}

fn parse_skins(item: &Dict, context: &str) -> Result<Vec<L2DSuModelSnapshot>, PyError> {
    let skins = match item.get("skins") {
        None => return Ok(Vec::new()),
        Some(Value::List(skins)) => skins,
        Some(_) => return Err(schema_error(format!("{context}.skins must be a list"))),
    };
    let mut models = Vec::new();
    for (index, skin) in skins.iter().enumerate() {
        let skin_context = format!("{context}.skins[{index}]");
        let Value::Dict(skin) = skin else {
            return Err(schema_error(format!("{skin_context} must be an object")));
        };
        models.extend(parse_skin(skin, &skin_context)?);
    }
    Ok(models)
}

fn first_non_empty(first: String, second: impl FnOnce() -> String) -> String {
    if first.is_empty() { second() } else { first }
}

fn parse_skin(skin: &Dict, context: &str) -> Result<Vec<L2DSuModelSnapshot>, PyError> {
    if is_hashed_skin(skin) {
        return parse_hashed_skin(skin, context);
    }
    let mut snapshots = Vec::new();
    let kind = skin_kind(skin);
    let model_key = first_non_empty(optional_str(skin, "painting"), || optional_str(skin, "prefab"));
    if let Some(kind) = kind
        && !model_key.is_empty()
    {
        snapshots.push(skin_snapshot(skin, kind, &model_key, context)?);
    }
    let painting_key = optional_str(skin, "painting");
    if !painting_key.is_empty() {
        snapshots.push(skin_snapshot(skin, ModelKind::Painting, &painting_key, context)?);
    }
    Ok(snapshots)
}

/// `_l2d_su_model_url`.
fn model_url(kind: ModelKind, model_key: &str) -> String {
    let relative_path = match kind {
        ModelKind::Live2d => format!("live2d/{model_key}/{model_key}.model3.json"),
        ModelKind::Spine => format!("spinepainting/{model_key}"),
        ModelKind::Painting => format!("painting/{model_key}.webp"),
    };
    format!("{L2D_SU_STATIC_BASE_URL}/{relative_path}")
}

fn skin_snapshot(skin: &Dict, kind: ModelKind, model_key: &str, context: &str) -> Result<L2DSuModelSnapshot, PyError> {
    let empty = Dict::new();
    let icons = match skin.get("assetPaths") {
        Some(Value::Dict(paths)) => paths,
        _ => &empty,
    };
    let costume_id = required_int(skin, "id", context)?;
    let mut snapshot = L2DSuModelSnapshot::new(
        kind,
        costume_id,
        first_non_empty(optional_str(skin, "name"), || model_key.to_owned()),
        model_url(kind, model_key),
    );
    model_key.clone_into(&mut snapshot.model_key);
    snapshot.skin_type = optional_str(skin, "skinTypeName");
    snapshot.feature_tags = string_tuple(skin.get("featureTags"));
    snapshot.is_live2d_plus = matches!(skin.get("isLive2dPlus"), Some(Value::Bool(true)));
    snapshot.face_ids = string_tuple(skin.get("paintingFaceIds"));
    snapshot.square_icon = optional_str(icons, "squareIcon");
    snapshot.shipyard_icon = optional_str(icons, "shipyardIcon");
    snapshot.q_icon = optional_str(icons, "qIcon");
    snapshot.shop_type = optional_str(skin, "shopTypeName");
    Ok(snapshot)
}

fn is_hashed_skin(skin: &Dict) -> bool {
    let painting = first_non_empty(optional_str(skin, "painting"), || optional_str(skin, "prefab"));
    is_hashed_resource_key(&painting)
}

/// `_parse_hashed_l2d_su_skin`: a skin of the hashed /al/ build, keyed by its id
/// (`skin-<id>`), its assets as listed.
fn parse_hashed_skin(skin: &Dict, context: &str) -> Result<Vec<L2DSuModelSnapshot>, PyError> {
    let costume_id = required_int(skin, "id", context)?;
    let model_key = format!("skin-{costume_id}");
    let empty = Dict::new();
    let paths = match skin.get("assetPaths") {
        Some(Value::Dict(paths)) => paths,
        _ => &empty,
    };
    let model_path = optional_str_of(skin.get("model"), "path");
    let mut snapshots = Vec::new();
    if let Some(kind) = skin_kind(skin) {
        let path = if model_path.is_empty() {
            String::new()
        } else {
            l2d_su_static_url(&model_path)
        };
        snapshots.push(hashed_skin_snapshot(skin, kind, &model_key, path, paths, context)?);
    }
    let painting_path = optional_str(paths, "painting");
    if !painting_path.is_empty() {
        snapshots.push(hashed_skin_snapshot(
            skin,
            ModelKind::Painting,
            &model_key,
            l2d_su_static_url(&painting_path),
            paths,
            context,
        )?);
    }
    Ok(snapshots)
}

fn hashed_skin_snapshot(
    skin: &Dict,
    kind: ModelKind,
    model_key: &str,
    path: String,
    paths: &Dict,
    context: &str,
) -> Result<L2DSuModelSnapshot, PyError> {
    let mut snapshot = skin_snapshot(skin, kind, model_key, context)?;
    snapshot.path = path;
    snapshot.square_icon = hashed_asset_url(paths, "squareIcon");
    snapshot.shipyard_icon = hashed_asset_url(paths, "shipyardIcon");
    snapshot.q_icon = hashed_asset_url(paths, "qIcon");
    snapshot.face_urls = hashed_face_urls(paths.get("paintingFaces"));
    snapshot.painting_index_url = hashed_painting_index_url(paths.get("paintingPaths"), &optional_str(skin, "painting"))?;
    snapshot.hashed = true;
    Ok(snapshot)
}

fn hashed_asset_url(paths: &Dict, field_name: &str) -> String {
    let path = optional_str(paths, field_name);
    if path.is_empty() { String::new() } else { l2d_su_static_url(&path) }
}

fn hashed_face_urls(value: Option<&Value>) -> Vec<(String, String)> {
    let Some(Value::List(items)) = value else {
        return Vec::new();
    };
    let mut faces = Vec::new();
    for item in items.iter().filter_map(Value::as_dict) {
        let face_id = match item.get("id") {
            Some(Value::Int(id)) => id.to_string(),
            Some(Value::Str(id)) => py_strip(id).to_owned(),
            _ => continue,
        };
        let url = optional_str(item, "url");
        if !face_id.is_empty() && !url.is_empty() {
            faces.push((face_id, l2d_su_static_url(&url)));
        }
    }
    faces
}

/// `_hashed_painting_index_url`: the index of the skin's own painting; a few skins list a
/// second painting after it.
fn hashed_painting_index_url(value: Option<&Value>, painting_key: &str) -> Result<String, PyError> {
    let paths: Vec<&str> = match value {
        Some(Value::List(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .filter(|path| !py_strip(path).is_empty())
            .collect(),
        _ => Vec::new(),
    };
    for path in &paths {
        if pypath::stem(&split(path)?.path) == painting_key {
            return Ok(l2d_su_static_url(path));
        }
    }
    Ok(paths.first().map(|path| l2d_su_static_url(path)).unwrap_or_default())
}

/// `_l2d_su_skin_kind`.
fn skin_kind(skin: &Dict) -> Option<ModelKind> {
    let dynamic_type = optional_str(skin, "dynamicType");
    if matches!(skin.get("isLive2d"), Some(Value::Bool(true))) || dynamic_type == "live2d" {
        return Some(ModelKind::Live2d);
    }
    if matches!(skin.get("isSpine"), Some(Value::Bool(true))) || dynamic_type == "spine" {
        return Some(ModelKind::Spine);
    }
    None
}

/// `_parse_l2d_su_new_skins`.
fn parse_new_skins(value: Option<&Value>) -> Result<Vec<L2DSuNewSkin>, PyError> {
    let Some(Value::List(items)) = value else {
        return Ok(Vec::new());
    };
    let mut skins = Vec::new();
    for item in items.iter().filter_map(Value::as_dict) {
        let ship_group_id = match item.get("shipGroupId") {
            Some(value @ (Value::Int(_) | Value::Bool(_))) => value.clone(),
            _ => Value::Int(0),
        };
        // `item.get('skinIds', [])` iterated: a string or a dict iterates (to nothing that is
        // an int); anything else that is not a list is Python's TypeError.
        let skin_ids = match item.get("skinIds") {
            None | Some(Value::Str(_) | Value::Dict(_)) => Vec::new(),
            Some(Value::List(ids)) => ids.iter().filter_map(|id| plain_int(Some(id))).collect(),
            Some(other) => {
                return Err(PyError::new(
                    "TypeError",
                    format!("'{}' object is not iterable", crate::pyerr::type_name(other)),
                ));
            }
        };
        skins.push(L2DSuNewSkin {
            ship_group_id,
            ship_name: optional_str(item, "shipName"),
            skin_name: optional_str(item, "skinName"),
            skin_type: optional_str(item, "skinType"),
            skin_ids,
        });
    }
    Ok(skins)
}

/// `merge_l2d_su_english_names`.
pub fn merge_l2d_su_english_names(primary: &L2DSuIndexData, english: &L2DSuIndexData) -> L2DSuIndexData {
    let mut merged = primary.clone();
    merged.characters = primary
        .characters
        .iter()
        .map(|character| {
            // `{character.char_id: character for ...}`: the last one with an id wins.
            let english = english.characters.iter().rev().find(|other| other.char_id == character.char_id);
            character_with_english_names(character, english)
        })
        .collect();
    merged
}

fn character_with_english_names(character: &L2DSuCharacterSnapshot, english: Option<&L2DSuCharacterSnapshot>) -> L2DSuCharacterSnapshot {
    let Some(english) = english else {
        return character.clone();
    };
    let mut names: Vec<(i128, String)> = Vec::new();
    for model in english.models() {
        insert_ordered(&mut names, model.costume_id, model.costume_name.clone());
    }
    let rename = |models: &[L2DSuModelSnapshot]| -> Vec<L2DSuModelSnapshot> {
        models
            .iter()
            .map(|model| {
                let mut model = model.clone();
                model.costume_name_en = names
                    .iter()
                    .find(|(id, _)| *id == model.costume_id)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_default();
                model
            })
            .collect()
    };
    let mut merged = character.clone();
    merged.char_name_en = if english.char_name.is_empty() {
        character.char_name_en.clone()
    } else {
        english.char_name.clone()
    };
    merged.live2d = rename(&character.live2d);
    merged.spine = rename(&character.spine);
    merged.paintings = rename(&character.paintings);
    merged
}

// ---------- nagami ----------

/// `parse_nagami_mapping`: the nagami Live2D mapping, in either layout (grouped by ship id
/// since 2026-11-03, or model key to `"Ship - Skin"` / `{name, bg}` before), one entry per
/// model key, sorted case-insensitively.
pub fn parse_nagami_mapping(source: &str) -> Result<NagamiMappingData, PyError> {
    let payload = decode_json_object(source, "Nagami mapping")?;
    let mut entries = Vec::new();
    for (key, value) in payload.iter() {
        let key = key_text(key);
        if py_strip(key).is_empty() {
            return Err(schema_error("Nagami mapping keys must be non-empty strings"));
        }
        match value {
            Value::Dict(ship) if ship.contains_key("skins") => entries.extend(parse_nagami_ship_entries(key, ship)?),
            _ => entries.push(parse_nagami_mapping_entry(key, value)?),
        }
    }
    entries.sort_by_cached_key(|entry: &NagamiMappingEntry| fav_core::pycasefold::casefold(&entry.key));
    Ok(NagamiMappingData { entries })
}

fn parse_nagami_mapping_entry(key: &str, value: &Value) -> Result<NagamiMappingEntry, PyError> {
    let (name, background) = match value {
        Value::Str(name) => (name.clone(), String::new()),
        Value::Dict(item) => (optional_str(item, "name"), optional_str(item, "bg")),
        _ => {
            return Err(schema_error(format!(
                "Nagami mapping value for {} must be a string or an object",
                repr_str(key)
            )));
        }
    };
    if py_strip(&name).is_empty() {
        return Err(schema_error(format!(
            "Nagami mapping name for {} must be a non-empty string",
            repr_str(key)
        )));
    }
    Ok(NagamiMappingEntry {
        key: key.to_owned(),
        name,
        background,
    })
}

fn parse_nagami_ship_entries(ship_id: &str, value: &Dict) -> Result<Vec<NagamiMappingEntry>, PyError> {
    let Some(Value::List(skins)) = value.get("skins") else {
        return Err(schema_error(format!(
            "Nagami mapping skins for ship {} must be a list",
            repr_str(ship_id)
        )));
    };
    let ship_name = nagami_localized_name(value.get("names"));
    let mut entries = Vec::new();
    for (index, skin) in skins.iter().enumerate() {
        let context = format!("Nagami mapping ship {} skin {index}", repr_str(ship_id));
        let Value::Dict(skin) = skin else {
            return Err(schema_error(format!("{context} must be an object")));
        };
        let prefab = optional_str(skin, "prefab");
        if prefab.is_empty() {
            return Err(schema_error(format!("{context} must have a non-empty prefab")));
        }
        let skin_name = nagami_localized_name(skin.get("names"));
        let parts: Vec<&str> = [ship_name.as_str(), skin_name.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect();
        let name = if parts.is_empty() { prefab.clone() } else { parts.join(" - ") };
        entries.push(NagamiMappingEntry {
            key: prefab,
            name,
            background: optional_str(skin, "bg"),
        });
    }
    Ok(entries)
}

fn nagami_localized_name(names: Option<&Value>) -> String {
    let Some(Value::Dict(names)) = names else {
        return String::new();
    };
    for locale in super::NAGAMI_NAME_LOCALES {
        if let Some(Value::Str(name)) = names.get(locale)
            && !py_strip(name).is_empty()
        {
            return py_strip(name).to_owned();
        }
    }
    String::new()
}

// ---------- spine ----------

/// `_spine_stem_url`.
pub(crate) fn spine_stem_url(url: &str) -> Result<String, PyError> {
    let base_url = url.trim_end_matches('/');
    for suffix in [".skel", ".atlas", SPINE_PARTS_MANIFEST_EXTENSION] {
        if let Some(stem) = base_url.strip_suffix(suffix) {
            return Ok(stem.to_owned());
        }
    }
    let path = split(base_url)?.path;
    let path_name = pypath::name(&path);
    let file_stem = path_name.strip_suffix("-spine").unwrap_or(path_name);
    Ok(format!("{base_url}/{file_stem}"))
}

/// `spine_parts_manifest_url`.
pub fn spine_parts_manifest_url(url: &str) -> Result<String, PyError> {
    if split(url)?.path.ends_with(SPINE_PARTS_MANIFEST_EXTENSION) {
        // The hashed build points a spine skin straight at its parts manifest.
        return Ok(url.to_owned());
    }
    Ok(format!("{}{SPINE_PARTS_MANIFEST_EXTENSION}", spine_stem_url(url)?))
}

/// `spine_resource_manifest`.
pub fn spine_resource_manifest(url: &str, parts_source: Option<&str>) -> Result<SpineResourceManifest, PyError> {
    let stem_url = spine_stem_url(url)?;
    let Some(parts_source) = parts_source else {
        if is_hashed_asset_url(url) {
            return Err(schema_error(format!("Spine model {url} has no readable parts manifest")));
        }
        let name = pypath::name(&split(&stem_url)?.path).to_owned();
        return Ok(SpineResourceManifest {
            parts: vec![SpineModelPart {
                name,
                skeleton_url: format!("{stem_url}.skel"),
                atlas_urls: vec![format!("{stem_url}.atlas")],
            }],
            parts_manifest_url: String::new(),
        });
    };
    let parts_url = spine_parts_manifest_url(url)?;
    Ok(SpineResourceManifest {
        parts: parse_spine_parts_manifest(parts_source, &parts_url)?,
        parts_manifest_url: parts_url,
    })
}

/// `parse_spine_parts_manifest`.
pub fn parse_spine_parts_manifest(source: &str, parts_manifest_url: &str) -> Result<Vec<SpineModelPart>, PyError> {
    let payload = decode_json_object(source, "Spine parts manifest")?;
    let Some(Value::List(models)) = payload.get("models") else {
        return Err(schema_error("Spine parts manifest must contain a models list"));
    };
    let mut parts = Vec::new();
    for (index, model) in models.iter().enumerate() {
        if let Value::Dict(model) = model {
            parts.push(spine_model_part(model, parts_manifest_url, index)?);
        }
    }
    if parts.is_empty() {
        return Err(schema_error("Spine parts manifest does not contain a model with a skeleton"));
    }
    Ok(parts)
}

fn spine_model_part(model: &Dict, parts_manifest_url: &str, index: usize) -> Result<SpineModelPart, PyError> {
    let context = format!("Spine parts manifest models[{index}]");
    let skeleton = required_str(model, "skeleton", &context)?;
    let atlas_paths = match model.get("atlases") {
        value @ Some(Value::List(_)) => string_tuple(value),
        _ => Vec::new(),
    };
    let name = optional_str(model, "name");
    let name = if name.is_empty() { resource_basename(&skeleton)? } else { name };
    let skeleton_url = resolve_resource_url(parts_manifest_url, &skeleton)?;
    let atlas_urls = atlas_paths
        .iter()
        .map(|atlas_path| resolve_resource_url(parts_manifest_url, atlas_path))
        .collect::<Result<_, _>>()?;
    Ok(SpineModelPart {
        name,
        skeleton_url,
        atlas_urls,
    })
}

/// `_parse_spine_atlas_texture_urls`.
pub(crate) fn parse_spine_atlas_texture_urls(source: &str, atlas_url: &str) -> Result<Vec<String>, PyError> {
    parse_spine_atlas_texture_paths(source)?
        .iter()
        .map(|path| resolve_resource_url(atlas_url, path))
        .collect()
}

/// `_parse_spine_atlas_texture_paths`: the page lines of a libgdx atlas.
pub(crate) fn parse_spine_atlas_texture_paths(source: &str) -> Result<Vec<String>, PyError> {
    if py_strip(source).is_empty() {
        return Err(parse_error("Spine atlas is empty"));
    }
    let mut texture_paths = Vec::new();
    let mut next_non_empty_line_is_page = true;
    for raw_line in py_splitlines(source) {
        let line = py_strip(raw_line);
        if line.is_empty() {
            next_non_empty_line_is_page = true;
            continue;
        }
        if raw_line.chars().next().is_some_and(is_py_space) || line.contains(':') {
            continue;
        }
        if next_non_empty_line_is_page {
            texture_paths.push(line.to_owned());
            next_non_empty_line_is_page = false;
        }
    }
    let texture_paths = unique_non_empty(texture_paths);
    if texture_paths.is_empty() {
        return Err(schema_error("Spine atlas does not contain a texture page"));
    }
    Ok(texture_paths)
}

/// `_resource_basename`.
pub(crate) fn resource_basename(value: &str) -> Result<String, PyError> {
    if py_strip(value).is_empty() {
        return Ok(String::new());
    }
    let path = split(value)?.path;
    Ok(pypath::name(if path.is_empty() { value } else { &path }).to_owned())
}

// ---------- paintings ----------

/// `_painting_key_from_url`.
pub(crate) fn painting_key_from_url(url: &str) -> Result<String, PyError> {
    let path = split(url)?.path;
    let name = pypath::name(&path);
    Ok(name.strip_suffix(".webp").unwrap_or(name).to_owned())
}

/// `painting_index_url`: the `<key>.json` index beside a painting sheet on the CDN.
pub fn painting_index_url(painting_url: &str) -> Result<String, PyError> {
    Ok(l2d_su_painting_index_url(&painting_key_from_url(painting_url)?))
}

/// `entry_painting_index_url`: the painting entry's layer index; empty for a hashed painting
/// the source lists none for.
pub fn entry_painting_index_url(entry: &ModelEntry) -> Result<String, PyError> {
    if !entry.resources.painting_index_url.is_empty() {
        return Ok(entry.resources.painting_index_url.clone());
    }
    if is_hashed_asset_url(&entry.resources.primary_url) {
        return Ok(String::new());
    }
    painting_index_url(&entry.resources.primary_url)
}

/// `parse_painting_index`. `index_url` is given for the hashed build, whose index names each
/// layer's sheet and mesh (relative to the index).
pub fn parse_painting_index(source: &str, painting_url: &str, index_url: &str) -> Result<PaintingIndex, PyError> {
    let painting_key = if index_url.is_empty() {
        painting_key_from_url(painting_url)?
    } else {
        pypath::stem(&split(index_url)?.path).to_owned()
    };
    let payload = decode_json_object(source, "l2d.su painting index")?;
    let mut layers = Vec::new();
    let mut face = None;
    for (name, value) in payload.iter() {
        let name = py_strip(key_text(name));
        let Value::Dict(value) = value else { continue };
        if name.is_empty() {
            continue;
        }
        let layer = painting_layer(name, value, index_url)?;
        if layer.name == PAINTING_FACE_SLOT {
            face = Some(layer);
            continue;
        }
        layers.push(layer);
    }
    if layers.is_empty() {
        return Err(schema_error("l2d.su painting index does not contain a drawable layer"));
    }
    Ok(PaintingIndex {
        index_url: if index_url.is_empty() {
            l2d_su_painting_index_url(&painting_key)
        } else {
            index_url.to_owned()
        },
        painting_key,
        layers,
        face,
    })
}

fn painting_layer(name: &str, value: &Dict, index_url: &str) -> Result<PaintingLayer, PyError> {
    let size = painting_number_pair(value.get("size")).unwrap_or_default();
    let (image, mesh) = if index_url.is_empty() {
        (String::new(), String::new())
    } else {
        (optional_str(value, "image"), optional_str(value, "mesh"))
    };
    let image_url = if image.is_empty() {
        String::new()
    } else {
        resolve_resource_url(index_url, &image)?
    };
    let mesh_url = if mesh.is_empty() {
        String::new()
    } else {
        resolve_resource_url(index_url, &mesh)?
    };
    Ok(PaintingLayer {
        image_url,
        mesh_url,
        name: name.to_owned(),
        size,
        // rawSize is absent on layers that are stored at their declared size.
        raw_size: painting_number_pair(value.get("rawSize")).unwrap_or(size),
        position: painting_number_pair(value.get("position")).unwrap_or_default(),
        pivot: painting_number_pair(value.get("pivot")).unwrap_or_default(),
        raw: matches!(value.get("raw"), Some(Value::Bool(true))),
    })
}

#[allow(clippy::cast_precision_loss)] // `float(int)`
fn painting_number_pair(value: Option<&Value>) -> Option<(f64, f64)> {
    let Some(Value::List(items)) = value else {
        return None;
    };
    if items.len() != 2 {
        return None;
    }
    let number = |value: &Value| match value {
        Value::Int(number) => Some(*number as f64),
        Value::Float(number) => Some(*number),
        _ => None,
    };
    Some((number(&items[0])?, number(&items[1])?))
}

// ---------- Live2D ----------

fn context_of(pairs: Vec<(&str, Value)>) -> Dict {
    pairs.into_iter().map(|(key, value)| (key.to_owned(), value)).collect()
}

/// `_optional_resource_reference_path`: a stripped, non-blank string field, else empty.
fn reference_path(item: &Dict, field_name: &str) -> String {
    match item.get(field_name) {
        Some(Value::Str(text)) => py_strip(text).to_owned(),
        _ => String::new(),
    }
}

/// `_resource_reference`.
fn resource_reference(kind: AssetKind, raw_path: &str, base_url: &str, metadata: Dict) -> Result<ResourceReference, PyError> {
    Ok(ResourceReference {
        kind,
        raw_path: py_strip(raw_path).to_owned(),
        url: resolve_resource_url(base_url, raw_path)?,
        context: metadata,
    })
}

/// `_parse_live2d_model3`.
pub(crate) fn parse_live2d_model3(source: &str, model3_url: &str) -> Result<Live2DResourceManifest, PyError> {
    let payload =
        pyjson::loads(source).map_err(|error| parse_error(format!("Live2D model3 JSON is invalid: {}", json_error_msg(&error, source))))?;
    let Value::Dict(payload) = payload else {
        return Err(schema_error("Live2D model3 root must be an object"));
    };
    let Some(Value::Dict(file_references)) = payload.get("FileReferences") else {
        return Err(schema_error("Live2D model3 FileReferences must be an object"));
    };
    let context = "Live2D model3 FileReferences";

    let moc3_path = reference_path(file_references, "Moc");
    if moc3_path.is_empty() {
        return Err(schema_error(format!("{context}.Moc must be a non-empty string")));
    }
    let moc3_reference = resource_reference(
        AssetKind::Live2dMoc3,
        &moc3_path,
        model3_url,
        context_of(vec![("live2d_field", Value::from("moc3"))]),
    )?;

    let Some(Value::List(textures)) = file_references.get("Textures") else {
        return Err(schema_error(format!("{context}.Textures must be a list")));
    };
    let mut texture_references = Vec::new();
    for (index, value) in textures.iter().enumerate() {
        if let Value::Str(raw_path) = value
            && !py_strip(raw_path).is_empty()
        {
            texture_references.push(resource_reference(
                AssetKind::Live2dTexture,
                raw_path,
                model3_url,
                context_of(vec![
                    ("live2d_field", Value::from("texture")),
                    ("texture_index", Value::Int(i128::try_from(index).unwrap_or(i128::MAX))),
                ]),
            )?);
        }
    }
    if texture_references.is_empty() {
        return Err(schema_error(format!(
            "{context}.Textures must contain at least one non-empty string"
        )));
    }

    let optional = |field_name: &str, kind: AssetKind, live2d_field: &str| -> Result<Option<ResourceReference>, PyError> {
        let raw_path = reference_path(file_references, field_name);
        if raw_path.is_empty() {
            return Ok(None);
        }
        resource_reference(
            kind,
            &raw_path,
            model3_url,
            context_of(vec![("live2d_field", Value::from(live2d_field))]),
        )
        .map(Some)
    };
    let physics_reference = optional("Physics", AssetKind::Live2dPhysics, "physics")?;
    let pose_reference = optional("Pose", AssetKind::Live2dPose, "pose")?;
    let display_info_reference = optional("DisplayInfo", AssetKind::Live2dDisplayInfo, "display_info")?;
    let expression_references = parse_expression_references(file_references.get("Expressions"), model3_url)?;
    let motions = parse_motion_references(file_references.get("Motions"), model3_url)?;

    let url_of = |reference: &Option<ResourceReference>| reference.as_ref().map(|reference| reference.url.clone()).unwrap_or_default();
    let urls = |references: &[ResourceReference]| references.iter().map(|reference| reference.url.clone()).collect::<Vec<_>>();
    let mut references = vec![moc3_reference.clone()];
    references.extend(texture_references.iter().cloned());
    references.extend(
        [physics_reference.clone(), pose_reference.clone(), display_info_reference.clone()]
            .into_iter()
            .flatten(),
    );
    references.extend(expression_references.iter().cloned());
    references.extend(motions.motions.iter().cloned());
    references.extend(motions.audio.iter().cloned());
    Ok(Live2DResourceManifest {
        moc3_url: moc3_reference.url.clone(),
        texture_urls: urls(&texture_references),
        physics_url: url_of(&physics_reference),
        pose_url: url_of(&pose_reference),
        display_info_url: url_of(&display_info_reference),
        expression_urls: urls(&expression_references),
        motion_urls: urls(&motions.motions),
        audio_urls: urls(&motions.audio),
        motion_names: motions.names,
        expression_names: expression_references
            .iter()
            .map(|reference| match reference.context.get("expression_name") {
                Some(Value::Str(name)) => name.clone(),
                _ => String::new(),
            })
            .collect(),
        has_audio: motions.has_audio,
        has_text: motions.has_text,
        references,
    })
}

fn parse_expression_references(value: Option<&Value>, base_url: &str) -> Result<Vec<ResourceReference>, PyError> {
    let Some(Value::List(items)) = value else {
        return Ok(Vec::new());
    };
    let mut references = Vec::new();
    for (index, expression) in items.iter().enumerate() {
        let Value::Dict(expression) = expression else { continue };
        let raw_path = reference_path(expression, "File");
        if raw_path.is_empty() {
            continue;
        }
        let expression_name = match expression.get("Name") {
            Some(Value::Str(name)) if !py_strip(name).is_empty() => py_strip(name).to_owned(),
            _ => resource_basename(&raw_path)?,
        };
        references.push(resource_reference(
            AssetKind::Live2dExpression,
            &raw_path,
            base_url,
            context_of(vec![
                ("live2d_field", Value::from("expression")),
                ("expression_index", Value::Int(i128::try_from(index).unwrap_or(i128::MAX))),
                ("expression_name", Value::Str(expression_name)),
            ]),
        )?);
    }
    Ok(references)
}

struct MotionReferences {
    motions: Vec<ResourceReference>,
    audio: Vec<ResourceReference>,
    names: Vec<String>,
    has_audio: bool,
    has_text: bool,
}

fn parse_motion_references(value: Option<&Value>, base_url: &str) -> Result<MotionReferences, PyError> {
    let mut parsed = MotionReferences {
        motions: Vec::new(),
        audio: Vec::new(),
        names: Vec::new(),
        has_audio: false,
        has_text: false,
    };
    let Some(Value::Dict(groups)) = value else {
        return Ok(parsed);
    };
    for (group_name, motions) in groups.iter() {
        let group = py_strip(key_text(group_name)).to_owned();
        if !group.is_empty() {
            parsed.names.push(group.clone());
        }
        let Value::List(motions) = motions else { continue };
        for (motion_index, motion) in motions.iter().enumerate() {
            let Value::Dict(motion) = motion else { continue };
            let mut context = context_of(vec![
                ("live2d_field", Value::from("motion")),
                ("motion_group", Value::Str(group.clone())),
                ("motion_index", Value::Int(i128::try_from(motion_index).unwrap_or(i128::MAX))),
            ]);
            let motion_file = reference_path(motion, "File");
            // Cubism names the field Sound; l2d.su's older models carry an absolute URL under Audio.
            let mut sound_file = reference_path(motion, "Sound");
            if sound_file.is_empty() {
                sound_file = reference_path(motion, "Audio");
            }
            // Text is the subtitle line itself (spoken dialogue), not a file path.
            let text = reference_path(motion, "Text");
            if !text.is_empty() {
                context.insert("motion_text", Value::Str(text.clone()));
            }
            let motion_reference = if motion_file.is_empty() {
                None
            } else {
                Some(resource_reference(
                    AssetKind::Live2dMotion,
                    &motion_file,
                    base_url,
                    context.clone(),
                )?)
            };
            let audio_reference = if sound_file.is_empty() {
                None
            } else {
                let mut audio_context = context.clone();
                audio_context.insert("live2d_field", Value::from("audio"));
                Some(resource_reference(AssetKind::Live2dAudio, &sound_file, base_url, audio_context)?)
            };
            let name = resource_basename(&motion_file)?;
            if !name.is_empty() && group.is_empty() {
                parsed.names.push(name);
            }
            if let Some(reference) = motion_reference {
                parsed.motions.push(reference);
            }
            if let Some(reference) = audio_reference {
                parsed.has_audio = true;
                parsed.audio.push(reference);
            }
            if !text.is_empty() {
                parsed.has_text = true;
            }
        }
    }
    parsed.names = unique_non_empty(std::mem::take(&mut parsed.names));
    Ok(parsed)
}

/// `_same_name_display_info_url`.
pub(crate) fn same_name_display_info_url(model3_url: &str) -> String {
    model3_url
        .strip_suffix(".model3.json")
        .map(|stem| format!("{stem}.cdi3.json"))
        .unwrap_or_default()
}
