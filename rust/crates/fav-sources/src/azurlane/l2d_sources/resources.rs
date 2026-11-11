//! The resource files of one catalog entry (`enumerate_azurlane_model_resources`) and the
//! local paths they are archived under.

use std::collections::HashMap;

use fav_core::filename::sanitize;
use fav_core::pyjson::{Dict, Value};
use fav_core::pypath;

use super::catalog::{
    is_hashed_asset_url, l2d_su_icon_url, l2d_su_painting_face_url, l2d_su_painting_mesh_url, l2d_su_painting_url, l2d_su_voice_url,
};
use super::model::{
    AssetKind, AzurLaneEnumeratedResource, AzurLaneModelResourceEnumeration, L2DSuVoiceLine, ModelEntry, ModelKind, PaintingIndex,
    PaintingLayer, ToPy,
};
use super::parse::{
    painting_key_from_url, parse_live2d_model3, parse_spine_atlas_texture_paths, same_name_display_info_url, spine_resource_manifest,
};
use super::{L2D_SU_AUDIO_BASE_URL, L2D_SU_STATIC_BASE_URL, blake2b_hex, catalog_entry_id, resolve_resource_url, schema_error, split};
use crate::PyError;

/// `_ResourceSpec`.
struct ResourceSpec {
    kind: AssetKind,
    source_url: String,
    fallback_url: String,
    base_url: String,
    context: Dict,
}

impl ResourceSpec {
    fn new(kind: AssetKind, source_url: String, base_url: String, context: Dict) -> Self {
        Self {
            kind,
            source_url,
            fallback_url: String::new(),
            base_url,
            context,
        }
    }
}

/// What `enumerate_azurlane_model_resources` is given beside the entry.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnumerationInputs<'a> {
    pub model3_source: Option<&'a str>,
    pub spine_parts_source: Option<&'a str>,
    pub atlas_sources: Option<&'a HashMap<String, String>>,
    pub voice_lines: &'a [L2DSuVoiceLine],
    pub painting_index: Option<&'a PaintingIndex>,
}

/// `enumerate_azurlane_model_resources`.
pub fn enumerate_azurlane_model_resources(
    entry: &ModelEntry,
    inputs: EnumerationInputs<'_>,
) -> Result<AzurLaneModelResourceEnumeration, PyError> {
    let specs = match entry.r#type {
        ModelKind::Live2d => {
            let Some(model3_source) = inputs.model3_source else {
                return Err(schema_error(format!(
                    "Live2D entry {} requires model3_source for resource enumeration",
                    entry.id
                )));
            };
            live2d_resource_specs(entry, model3_source)?
        }
        ModelKind::Painting => painting_resource_specs(entry, inputs.voice_lines, inputs.painting_index)?,
        ModelKind::Spine => spine_resource_specs(entry, inputs.spine_parts_source, inputs.atlas_sources)?,
    };
    Ok(AzurLaneModelResourceEnumeration {
        model_id: entry.id.clone(),
        model_type: entry.r#type,
        character_key: entry.character.key.clone(),
        costume_key: entry.costume.key.clone(),
        source_url: entry.resources.primary_url.clone(),
        fallback_url: entry.resources.fallback_url.clone(),
        assets: enumerated_resources_from_specs(entry, &specs)?,
        issues: Vec::new(),
    })
}

fn metadata(pairs: Vec<(&str, Value)>) -> Dict {
    pairs.into_iter().map(|(key, value)| (key.to_owned(), value)).collect()
}

/// `_asset_context`: the entry's identity, then the spec's own fields.
fn asset_context(entry: &ModelEntry, extra: &Dict) -> Dict {
    let optional_int = |value: Option<i128>| value.map_or(Value::Null, Value::Int);
    let mut context = metadata(vec![
        ("model_id", Value::Str(entry.id.clone())),
        ("model_type", Value::from(entry.r#type.as_str())),
        ("character_key", Value::Str(entry.character.key.clone())),
        ("character_id", optional_int(entry.character.id)),
        ("character_name_zh", Value::Str(entry.character.name_zh.clone())),
        ("character_name_en", Value::Str(entry.character.name_en.clone())),
        ("costume_key", Value::Str(entry.costume.key.clone())),
        ("costume_id", optional_int(entry.costume.id)),
        ("costume_name_zh", Value::Str(entry.costume.name_zh.clone())),
        ("costume_name_en", Value::Str(entry.costume.name_en.clone())),
        ("catalog_source", Value::from(entry.source.as_str())),
        ("source_model_url", Value::Str(entry.resources.primary_url.clone())),
        ("fallback_model_url", Value::Str(entry.resources.fallback_url.clone())),
    ]);
    for (key, value) in extra.iter() {
        context.insert(key.clone(), value.clone());
    }
    context
}

/// `_entry_local_base_url`: a readable model keeps its files beside the file that names
/// them; a hashed one lays them out as the CDN has them below the asset root.
fn entry_local_base_url(entry: &ModelEntry, natural_base_url: &str) -> String {
    if is_hashed_asset_url(&entry.resources.primary_url) {
        return relative_base_url();
    }
    natural_base_url.to_owned()
}

/// `_l2d_su_relative_base_url`: the base marker that makes local paths mirror the CDN layout.
fn relative_base_url() -> String {
    format!("{L2D_SU_STATIC_BASE_URL}/marker")
}

fn audio_relative_base_url() -> String {
    format!("{L2D_SU_AUDIO_BASE_URL}/marker")
}

fn live2d_resource_specs(entry: &ModelEntry, model3_source: &str) -> Result<Vec<ResourceSpec>, PyError> {
    let manifest = parse_live2d_model3(model3_source, &entry.resources.primary_url)?;
    let base_url = entry_local_base_url(entry, &entry.resources.primary_url);
    let mut specs = vec![ResourceSpec {
        kind: AssetKind::Live2dModel3,
        source_url: entry.resources.primary_url.clone(),
        fallback_url: entry.resources.fallback_url.clone(),
        base_url: base_url.clone(),
        context: asset_context(entry, &metadata(vec![("live2d_field", Value::from("model3"))])),
    }];
    for reference in &manifest.references {
        let fallback_url = if entry.resources.fallback_url.is_empty() {
            String::new()
        } else {
            resolve_resource_url(&entry.resources.fallback_url, &reference.raw_path)?
        };
        specs.push(ResourceSpec {
            kind: reference.kind,
            source_url: reference.url.clone(),
            fallback_url,
            base_url: base_url.clone(),
            context: asset_context(entry, &reference.context),
        });
    }
    if !entry.resources.display_info_url.is_empty() && !specs.iter().any(|spec| spec.kind == AssetKind::Live2dDisplayInfo) {
        specs.push(ResourceSpec {
            kind: AssetKind::Live2dDisplayInfo,
            source_url: entry.resources.display_info_url.clone(),
            fallback_url: same_name_display_info_url(&entry.resources.fallback_url),
            base_url,
            context: asset_context(
                entry,
                &metadata(vec![("live2d_field", Value::from("display_info")), ("inferred", Value::Bool(true))]),
            ),
        });
    }
    Ok(specs)
}

fn spine_resource_specs(
    entry: &ModelEntry,
    parts_source: Option<&str>,
    atlas_sources: Option<&HashMap<String, String>>,
) -> Result<Vec<ResourceSpec>, PyError> {
    let manifest = spine_resource_manifest(&entry.resources.primary_url, parts_source)?;
    let mut specs = Vec::new();
    if !manifest.parts_manifest_url.is_empty() {
        specs.push(ResourceSpec::new(
            AssetKind::SpineParts,
            manifest.parts_manifest_url.clone(),
            entry_local_base_url(entry, &manifest.parts_manifest_url),
            asset_context(entry, &metadata(vec![("spine_field", Value::from("parts"))])),
        ));
    }
    for part in &manifest.parts {
        specs.push(ResourceSpec::new(
            AssetKind::SpineSkel,
            part.skeleton_url.clone(),
            entry_local_base_url(entry, &part.skeleton_url),
            asset_context(
                entry,
                &metadata(vec![
                    ("spine_field", Value::from("skel")),
                    ("spine_part", Value::Str(part.name.clone())),
                ]),
            ),
        ));
        for atlas_url in &part.atlas_urls {
            specs.push(ResourceSpec::new(
                AssetKind::SpineAtlas,
                atlas_url.clone(),
                entry_local_base_url(entry, atlas_url),
                asset_context(
                    entry,
                    &metadata(vec![
                        ("spine_field", Value::from("atlas")),
                        ("spine_part", Value::Str(part.name.clone())),
                    ]),
                ),
            ));
        }
        let Some(atlas_sources) = atlas_sources else { continue };
        for atlas_url in &part.atlas_urls {
            let Some(atlas_source) = atlas_sources.get(atlas_url) else {
                continue;
            };
            for (page_index, texture_path) in parse_spine_atlas_texture_paths(atlas_source)?.into_iter().enumerate() {
                specs.push(ResourceSpec::new(
                    AssetKind::SpineTexture,
                    resolve_resource_url(atlas_url, &texture_path)?,
                    entry_local_base_url(entry, atlas_url),
                    asset_context(
                        entry,
                        &metadata(vec![
                            ("spine_field", Value::from("texture")),
                            ("spine_part", Value::Str(part.name.clone())),
                            ("atlas_page", Value::Str(texture_path)),
                            ("page_index", Value::Int(i128::try_from(page_index).unwrap_or(i128::MAX))),
                        ]),
                    ),
                ));
            }
        }
    }
    Ok(specs)
}

fn painting_resource_specs(
    entry: &ModelEntry,
    voice_lines: &[L2DSuVoiceLine],
    painting_index: Option<&PaintingIndex>,
) -> Result<Vec<ResourceSpec>, PyError> {
    let painting_key = painting_key_from_url(&entry.resources.primary_url)?;
    let mut specs = vec![ResourceSpec::new(
        AssetKind::PaintingImage,
        entry.resources.primary_url.clone(),
        entry_local_base_url(entry, &entry.resources.primary_url),
        asset_context(entry, &metadata(vec![("painting_field", Value::from("image"))])),
    )];
    if let Some(painting_index) = painting_index {
        specs.extend(painting_layer_specs(entry, &painting_key, painting_index)?);
    }
    // No case fallback is spelled out here: the downloader derives case variants for any
    // candidate that fails, which covers the filename and the key-bearing directory alike.
    let face_urls: Vec<(String, String)> = if entry.costume.face_urls.is_empty() {
        entry
            .costume
            .face_ids
            .iter()
            .map(|face_id| (face_id.clone(), l2d_su_painting_face_url(&painting_key, face_id)))
            .collect()
    } else {
        entry.costume.face_urls.clone()
    };
    for (face_id, face_url) in face_urls {
        specs.push(ResourceSpec::new(
            AssetKind::PaintingFace,
            face_url,
            relative_base_url(),
            asset_context(
                entry,
                &metadata(vec![("painting_field", Value::from("face")), ("face_id", Value::Str(face_id))]),
            ),
        ));
    }
    // q_icon is deliberately absent: the index advertises qicon/<key> for every skin, but the
    // CDN hosts none of them -- l2d.su's own page renders that slot broken too.
    for (kind, icon_path) in [
        (AssetKind::IconSquare, &entry.costume.square_icon),
        (AssetKind::IconShipyard, &entry.costume.shipyard_icon),
    ] {
        if icon_path.is_empty() {
            continue;
        }
        specs.push(ResourceSpec::new(
            kind,
            l2d_su_icon_url(icon_path),
            relative_base_url(),
            asset_context(
                entry,
                &metadata(vec![
                    ("painting_field", Value::from("icon")),
                    ("icon_path", Value::Str(icon_path.clone())),
                ]),
            ),
        ));
    }
    for line in voice_lines {
        let mut extra = metadata(vec![("painting_field", Value::from("voice"))]);
        if let Value::Dict(fields) = line.to_py() {
            for (key, value) in fields.iter() {
                extra.insert(key.clone(), value.clone());
            }
        }
        specs.push(ResourceSpec::new(
            AssetKind::VoiceAudio,
            l2d_su_voice_url(&line.voice_path),
            audio_relative_base_url(),
            asset_context(entry, &extra),
        ));
    }
    Ok(specs)
}

/// `_painting_layer_specs`: the index, the sibling layer sheets, and the mesh that unpacks
/// each one (none for a `raw` layer, whose mesh URL answers with the site's SPA shell).
fn painting_layer_specs(entry: &ModelEntry, painting_key: &str, painting_index: &PaintingIndex) -> Result<Vec<ResourceSpec>, PyError> {
    let mut specs = vec![ResourceSpec::new(
        AssetKind::PaintingIndex,
        painting_index.index_url.clone(),
        entry_local_base_url(entry, &painting_index.index_url),
        asset_context(entry, &metadata(vec![("painting_field", Value::from("index"))])),
    )];
    let layer_context = |field: &str, layer: &PaintingLayer| {
        asset_context(
            entry,
            &metadata(vec![
                ("painting_field", Value::from(field)),
                ("layer", Value::Str(layer.name.clone())),
            ]),
        )
    };
    for layer in &painting_index.layers {
        if !layer.image_url.is_empty() {
            if split(&layer.image_url)?.path != split(&entry.resources.primary_url)?.path {
                specs.push(ResourceSpec::new(
                    AssetKind::PaintingLayer,
                    layer.image_url.clone(),
                    relative_base_url(),
                    layer_context("layer", layer),
                ));
            }
            if !layer.mesh_url.is_empty() && !layer.raw {
                specs.push(ResourceSpec::new(
                    AssetKind::PaintingMesh,
                    layer.mesh_url.clone(),
                    relative_base_url(),
                    layer_context("mesh", layer),
                ));
            }
            continue;
        }
        // Case-insensitively: the index and storage disagree on case in both directions.
        if fav_core::pycasefold::casefold(&layer.name) != fav_core::pycasefold::casefold(painting_key) {
            specs.push(ResourceSpec::new(
                AssetKind::PaintingLayer,
                l2d_su_painting_url(&layer.name),
                relative_base_url(),
                layer_context("layer", layer),
            ));
        }
        if layer.raw {
            continue;
        }
        specs.push(ResourceSpec::new(
            AssetKind::PaintingMesh,
            l2d_su_painting_mesh_url(&layer.name),
            relative_base_url(),
            layer_context("mesh", layer),
        ));
    }
    Ok(specs)
}

// ---------- local paths ----------

/// A `PurePosixPath`: its anchor (`""`, `/` or `//`) and its parts (no empty or `.` ones).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PosixPath {
    root: &'static str,
    parts: Vec<String>,
}

impl PosixPath {
    fn parse(path: &str) -> Self {
        let root = if path.starts_with("//") && !path.starts_with("///") {
            "//"
        } else if path.starts_with('/') {
            "/"
        } else {
            ""
        };
        let parts = path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .map(str::to_owned)
            .collect();
        Self { root, parts }
    }

    fn from_parts<I: IntoIterator<Item = String>>(parts: I) -> Self {
        let mut path = Self {
            root: "",
            parts: Vec::new(),
        };
        for part in parts {
            path.parts.extend(Self::parse(&part).parts);
        }
        path
    }

    fn name(&self) -> &str {
        self.parts.last().map_or("", String::as_str)
    }

    fn parent(&self) -> Self {
        let mut parent = self.clone();
        parent.parts.pop();
        parent
    }

    fn relative_to(&self, base: &Self) -> Option<Self> {
        (self.root == base.root && self.parts.starts_with(&base.parts)).then(|| Self {
            root: "",
            parts: self.parts[base.parts.len()..].to_vec(),
        })
    }

    fn as_posix(&self) -> String {
        let joined = self.parts.join("/");
        if self.root.is_empty() && joined.is_empty() {
            return ".".to_owned();
        }
        format!("{}{joined}", self.root)
    }
}

fn safe_path_segment(value: &str, fallback: &str) -> String {
    let sanitized = sanitize(value, 120);
    if sanitized.is_empty() { fallback.to_owned() } else { sanitized }
}

/// `_model_resource_directory`: `assets/<type>/<costume key>`, with a token of the entry id
/// for a variant entry.
fn model_resource_directory(entry: &ModelEntry) -> PosixPath {
    let mut model_key = safe_path_segment(&entry.costume.key, "unknown");
    let canonical_id = catalog_entry_id(entry.r#type, &entry.character.key, &entry.costume.key);
    if entry.id != canonical_id {
        let token = blake2b_hex(&entry.id, 4);
        model_key = safe_path_segment(&format!("{model_key}-{token}"), &model_key);
    }
    PosixPath::from_parts(["assets".to_owned(), entry.r#type.as_str().to_owned(), model_key])
}

fn relative_resource_path(source_url: &str, base_url: &str, fallback_filename: &str) -> Result<PosixPath, PyError> {
    let source_path = PosixPath::parse(&split(source_url)?.path);
    let base_path = PosixPath::parse(&split(base_url)?.path);
    let base_dir = if base_path.name().is_empty() {
        base_path
    } else {
        base_path.parent()
    };
    let relative = source_path
        .relative_to(&base_dir)
        .unwrap_or_else(|| PosixPath::parse(fallback_filename));
    let mut segments: Vec<String> = relative
        .parts
        .iter()
        .filter(|segment| !matches!(segment.as_str(), "" | "/" | "." | ".."))
        .map(|segment| safe_path_segment(segment, "asset"))
        .collect();
    if segments.is_empty() {
        segments = vec![safe_path_segment(fallback_filename, "asset")];
    }
    Ok(PosixPath::from_parts(segments))
}

/// `_resource_filename`.
fn resource_filename(url: &str, kind: AssetKind) -> Result<String, PyError> {
    let path = split(url)?.path;
    let filename = pypath::name(&path);
    let fallback = format!("asset{}", kind.extension());
    if filename.is_empty() {
        return Ok(fallback);
    }
    Ok(safe_path_segment(filename, &fallback))
}

/// `_disambiguated_local_path`: `<stem>-<digest><suffix>`.
fn disambiguated_local_path(local_path: &str, value: &str) -> String {
    let path = PosixPath::parse(local_path);
    let digest = blake2b_hex(value, 4);
    let name = path.name();
    let suffix = pypath::suffix(name);
    let stem = &name[..name.len() - suffix.len()];
    let mut renamed = path.parent();
    renamed.parts.push(format!("{stem}-{digest}{suffix}"));
    renamed.as_posix()
}

fn resource_local_path(
    entry: &ModelEntry,
    spec: &ResourceSpec,
    original_filename: &str,
    used_paths: &mut HashMap<String, String>,
) -> Result<String, PyError> {
    let mut path = model_resource_directory(entry);
    path.parts
        .extend(relative_resource_path(&spec.source_url, &spec.base_url, original_filename)?.parts);
    let mut local_path = path.as_posix();
    let mut collision_key = format!("{}:{}", spec.kind.as_str(), spec.source_url);
    if used_paths.get(&local_path).is_some_and(|key| *key != collision_key) {
        local_path = disambiguated_local_path(&local_path, &collision_key);
    }
    while used_paths.get(&local_path).is_some_and(|key| *key != collision_key) {
        collision_key = format!("{collision_key}:{}", used_paths.len());
        local_path = disambiguated_local_path(&local_path, &collision_key);
    }
    used_paths.insert(local_path.clone(), collision_key);
    Ok(local_path)
}

fn enumerated_resources_from_specs(entry: &ModelEntry, specs: &[ResourceSpec]) -> Result<Vec<AzurLaneEnumeratedResource>, PyError> {
    let mut used_paths = HashMap::new();
    let mut resources = Vec::with_capacity(specs.len());
    for spec in specs {
        let original_filename = resource_filename(&spec.source_url, spec.kind)?;
        let local_path = resource_local_path(entry, spec, &original_filename, &mut used_paths)?;
        resources.push(AzurLaneEnumeratedResource {
            model_id: entry.id.clone(),
            model_type: entry.r#type,
            character_key: entry.character.key.clone(),
            costume_key: entry.costume.key.clone(),
            kind: spec.kind,
            source_url: spec.source_url.clone(),
            fallback_url: spec.fallback_url.clone(),
            local_path,
            original_filename,
            contexts: vec![spec.context.clone()],
        });
    }
    Ok(resources)
}
