//! The dataclasses of `azurlane_l2d_sources.py`, with `asdict`-shaped [`ToPy`] renderings.
//!
//! `to_dict()` output is written to the archive (`_source/*.json`, `source_metadata`) and
//! WP12 hashes contexts, so every field renders under Python's name, in declaration order,
//! with tuples as lists and `None` as null.

use std::fmt;

use fav_core::pyjson::{Dict, Value};

/// `dataclasses.asdict` for one value.
pub trait ToPy {
    fn to_py(&self) -> Value;
}

impl ToPy for String {
    fn to_py(&self) -> Value {
        Value::Str(self.clone())
    }
}

impl ToPy for &'static str {
    fn to_py(&self) -> Value {
        Value::Str((*self).to_owned())
    }
}

impl ToPy for bool {
    fn to_py(&self) -> Value {
        Value::Bool(*self)
    }
}

impl ToPy for i128 {
    fn to_py(&self) -> Value {
        Value::Int(*self)
    }
}

impl ToPy for f64 {
    fn to_py(&self) -> Value {
        Value::Float(*self)
    }
}

impl ToPy for Value {
    fn to_py(&self) -> Value {
        self.clone()
    }
}

impl ToPy for Dict {
    fn to_py(&self) -> Value {
        Value::Dict(self.clone())
    }
}

impl<T: ToPy> ToPy for Option<T> {
    fn to_py(&self) -> Value {
        self.as_ref().map_or(Value::Null, ToPy::to_py)
    }
}

impl<T: ToPy> ToPy for Vec<T> {
    fn to_py(&self) -> Value {
        Value::List(self.iter().map(ToPy::to_py).collect())
    }
}

impl<A: ToPy, B: ToPy> ToPy for (A, B) {
    fn to_py(&self) -> Value {
        Value::List(vec![self.0.to_py(), self.1.to_py()])
    }
}

impl<T: ToPy> ToPy for Box<T> {
    fn to_py(&self) -> Value {
        (**self).to_py()
    }
}

/// A frozen dataclass: the struct plus its `asdict` rendering.
macro_rules! dataclass {
    ($(#[$meta:meta])* pub struct $name:ident { $($(#[$field_meta:meta])* pub $field:ident : $ty:ty),* $(,)? }) => {
        $(#[$meta])*
        pub struct $name {
            $($(#[$field_meta])* pub $field: $ty),*
        }

        impl ToPy for $name {
            fn to_py(&self) -> Value {
                let mut dict = Dict::new();
                $(dict.insert(stringify!($field).trim_start_matches("r#"), ToPy::to_py(&self.$field));)*
                Value::Dict(dict)
            }
        }
    };
}

/// A `Literal[...]` type: its values in declaration order.
macro_rules! literal {
    ($(#[$meta:meta])* pub enum $name:ident { $($variant:ident => $text:literal),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant),*
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text),*
                }
            }

            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some($name::$variant),)*
                    _ => None,
                }
            }
        }

        impl ToPy for $name {
            fn to_py(&self) -> Value {
                Value::Str(self.as_str().to_owned())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

literal! {
    /// `SourceErrorKind`.
    pub enum SourceErrorKind { Network => "network", Parse => "parse", Schema => "schema" }
}

literal! {
    /// `L2DSuModelKind` (`_MODEL_TYPES` order).
    pub enum ModelKind { Live2d => "live2d", Spine => "spine", Painting => "painting" }
}

literal! {
    /// `CatalogSource` (`_CATALOG_SOURCES` order).
    pub enum CatalogSource { L2dSu => "l2d.su", Nagami => "nagami", Merged => "merged" }
}

literal! {
    /// `AvailabilityState` (`_AVAILABILITY_STATES` order).
    pub enum AvailabilityState { Valid => "valid", FallbackOnly => "fallback-only", Broken => "broken", Unchecked => "unchecked" }
}

literal! {
    /// `ResourceValidationStatus`.
    pub enum ValidationStatus {
        Ok => "ok", Missing => "missing", Network => "network", Parse => "parse", Schema => "schema", Unchecked => "unchecked",
    }
}

literal! {
    /// `ResourceValidationSource`.
    pub enum ValidationSource { Primary => "primary", Fallback => "fallback" }
}

literal! {
    /// `ResourceAssetKind`.
    pub enum AssetKind {
        Live2dModel3 => "live2d.model3",
        Live2dMoc3 => "live2d.moc3",
        Live2dTexture => "live2d.texture",
        Live2dPhysics => "live2d.physics",
        Live2dPose => "live2d.pose",
        Live2dDisplayInfo => "live2d.display-info",
        Live2dExpression => "live2d.expression",
        Live2dMotion => "live2d.motion",
        Live2dAudio => "live2d.audio",
        SpineParts => "spine.parts",
        SpineSkel => "spine.skel",
        SpineAtlas => "spine.atlas",
        SpineTexture => "spine.texture",
        PaintingIndex => "painting.index",
        PaintingImage => "painting.image",
        PaintingLayer => "painting.layer",
        PaintingMesh => "painting.mesh",
        PaintingFace => "painting.face",
        IconSquare => "icon.square",
        IconShipyard => "icon.shipyard",
        VoiceAudio => "voice.audio",
    }
}

impl AssetKind {
    /// `_RESOURCE_EXTENSION_BY_KIND`.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Live2dModel3 => ".model3.json",
            Self::Live2dMoc3 => ".moc3",
            Self::Live2dTexture
            | Self::SpineTexture
            | Self::PaintingImage
            | Self::PaintingLayer
            | Self::PaintingFace
            | Self::IconSquare
            | Self::IconShipyard => ".webp",
            Self::Live2dPhysics => ".physics3.json",
            Self::Live2dPose => ".pose3.json",
            Self::Live2dDisplayInfo => ".cdi3.json",
            Self::Live2dExpression => ".exp3.json",
            Self::Live2dMotion => ".motion3.json",
            Self::Live2dAudio => ".wav",
            Self::SpineParts | Self::PaintingIndex => ".json",
            Self::SpineSkel => ".skel",
            Self::SpineAtlas => ".atlas",
            Self::PaintingMesh => ".obj",
            Self::VoiceAudio => ".ogg",
        }
    }
}

#[allow(clippy::derivable_impls)] // The enum comes from `literal!`, which cannot mark a default variant.
impl Default for AvailabilityState {
    fn default() -> Self {
        Self::Unchecked
    }
}

fn counts<T: Copy>(keys: &[T], name: impl Fn(T) -> &'static str) -> Vec<(&'static str, i128)> {
    keys.iter().map(|key| (name(*key), 0)).collect()
}

fn bump(counts: &mut [(&'static str, i128)], key: &str) {
    if let Some(slot) = counts.iter_mut().find(|(name, _)| *name == key) {
        slot.1 += 1;
    }
}

fn count_dict(counts: &[(&'static str, i128)]) -> Value {
    Value::Dict(
        counts
            .iter()
            .map(|(name, count)| ((*name).to_owned(), Value::Int(*count)))
            .collect(),
    )
}

fn len(items: usize) -> i128 {
    i128::try_from(items).unwrap_or(i128::MAX)
}

fn dict(pairs: Vec<(&str, Value)>) -> Dict {
    pairs.into_iter().map(|(key, value)| (key.to_owned(), value)).collect()
}

// ---------- source snapshots ----------

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct SourceFetchMetadata {
        pub url: String,
        pub http_status: Option<i128>,
        pub etag: String,
        pub last_modified: String,
        pub fetched_at: String,
    }
}

impl SourceFetchMetadata {
    /// `SourceFetchMetadata.for_url(url)` with only the URL known.
    pub fn for_url(url: impl Into<String>, fetched_at: String) -> Self {
        Self {
            url: url.into(),
            http_status: None,
            etag: String::new(),
            last_modified: String::new(),
            fetched_at,
        }
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct SourceSnapshotError {
        pub kind: SourceErrorKind,
        pub message: String,
        pub url: String,
        pub http_status: Option<i128>,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct L2DSuModelSnapshot {
        pub kind: ModelKind,
        pub costume_id: i128,
        pub costume_name: String,
        pub costume_name_en: String,
        pub path: String,
        pub model_key: String,
        pub skin_type: String,
        pub feature_tags: Vec<String>,
        pub is_live2d_plus: bool,
        pub face_ids: Vec<String>,
        pub square_icon: String,
        pub shipyard_icon: String,
        pub q_icon: String,
        pub shop_type: String,
        /// Set by the hashed /al/ build, whose assets cannot be derived from a name: absolute
        /// URLs of the face variations by face id, and of the painting's layer index.
        pub face_urls: Vec<(String, String)>,
        pub painting_index_url: String,
        pub hashed: bool,
    }
}

impl L2DSuModelSnapshot {
    pub fn new(kind: ModelKind, costume_id: i128, costume_name: String, path: String) -> Self {
        Self {
            kind,
            costume_id,
            costume_name,
            costume_name_en: String::new(),
            path,
            model_key: String::new(),
            skin_type: String::new(),
            feature_tags: Vec::new(),
            is_live2d_plus: false,
            face_ids: Vec::new(),
            square_icon: String::new(),
            shipyard_icon: String::new(),
            q_icon: String::new(),
            shop_type: String::new(),
            face_urls: Vec::new(),
            painting_index_url: String::new(),
            hashed: false,
        }
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct L2DSuCharacterSnapshot {
        pub char_id: i128,
        pub char_key: String,
        pub char_name: String,
        pub char_name_en: String,
        pub nation: String,
        pub ship_type: String,
        pub rarity: String,
        pub default_skin_id: Option<i128>,
        pub skin_series: Vec<String>,
        pub live2d: Vec<L2DSuModelSnapshot>,
        pub spine: Vec<L2DSuModelSnapshot>,
        pub paintings: Vec<L2DSuModelSnapshot>,
    }
}

impl L2DSuCharacterSnapshot {
    pub fn new(char_id: i128, char_key: impl Into<String>, char_name: impl Into<String>) -> Self {
        Self {
            char_id,
            char_key: char_key.into(),
            char_name: char_name.into(),
            char_name_en: String::new(),
            nation: String::new(),
            ship_type: String::new(),
            rarity: String::new(),
            default_skin_id: None,
            skin_series: Vec::new(),
            live2d: Vec::new(),
            spine: Vec::new(),
            paintings: Vec::new(),
        }
    }

    /// `(*character.live2d, *character.spine, *character.paintings)`.
    pub fn models(&self) -> impl Iterator<Item = &L2DSuModelSnapshot> {
        self.live2d.iter().chain(&self.spine).chain(&self.paintings)
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct L2DSuVoiceLine {
        pub key: String,
        pub voice_name: String,
        pub resource_key: String,
        pub voice_path: String,
        pub text: String,
        pub face_id: String,
        pub l2d_action: String,
        pub spine_action: String,
        pub is_extra: bool,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct L2DSuNewSkin {
        /// Any `int` the index gives, `bool` included (`isinstance(True, int)`), else `0`.
        pub ship_group_id: Value,
        pub ship_name: String,
        pub skin_name: String,
        pub skin_type: String,
        pub skin_ids: Vec<i128>,
    }
}

fn character_summary(characters: &[L2DSuCharacterSnapshot]) -> Vec<(&'static str, Value)> {
    vec![
        ("character_count", Value::Int(len(characters.len()))),
        (
            "live2d_count",
            Value::Int(characters.iter().map(|character| len(character.live2d.len())).sum()),
        ),
        (
            "spine_count",
            Value::Int(characters.iter().map(|character| len(character.spine.len())).sum()),
        ),
        (
            "painting_count",
            Value::Int(characters.iter().map(|character| len(character.paintings.len())).sum()),
        ),
    ]
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct L2DSuIndexData {
        pub region: String,
        pub game_version: String,
        pub generated_at: String,
        pub ship_count: i128,
        pub characters: Vec<L2DSuCharacterSnapshot>,
        pub new_skins: Vec<L2DSuNewSkin>,
        pub filters: Dict,
        pub skin_update_history: Vec<Dict>,
    }
}

impl L2DSuIndexData {
    pub fn summary(&self) -> Dict {
        dict(character_summary(&self.characters))
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct L2DSuSourceSnapshot {
        pub metadata: SourceFetchMetadata,
        pub region: String,
        pub game_version: String,
        pub generated_at: String,
        pub ship_count: i128,
        pub characters: Vec<L2DSuCharacterSnapshot>,
        pub new_skins: Vec<L2DSuNewSkin>,
        pub filters: Dict,
        pub skin_update_history: Vec<Dict>,
        pub errors: Vec<SourceSnapshotError>,
        pub source: &'static str,
    }
}

impl L2DSuSourceSnapshot {
    pub fn new(metadata: SourceFetchMetadata) -> Self {
        Self {
            metadata,
            region: String::new(),
            game_version: String::new(),
            generated_at: String::new(),
            ship_count: 0,
            characters: Vec::new(),
            new_skins: Vec::new(),
            filters: Dict::new(),
            skin_update_history: Vec::new(),
            errors: Vec::new(),
            source: "l2d.su",
        }
    }

    pub fn summary(&self) -> Dict {
        let mut pairs = character_summary(&self.characters);
        pairs.push(("new_skin_count", Value::Int(len(self.new_skins.len()))));
        pairs.push(("error_count", Value::Int(len(self.errors.len()))));
        dict(pairs)
    }

    pub fn to_dict(&self) -> Value {
        self.to_py()
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct NagamiMappingEntry {
        pub key: String,
        pub name: String,
        pub background: String,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct NagamiMappingData {
        pub entries: Vec<NagamiMappingEntry>,
    }
}

impl NagamiMappingData {
    pub fn summary(&self) -> Dict {
        dict(vec![("entry_count", Value::Int(len(self.entries.len())))])
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct NagamiSourceSnapshot {
        pub metadata: SourceFetchMetadata,
        pub entries: Vec<NagamiMappingEntry>,
        pub errors: Vec<SourceSnapshotError>,
        pub source: &'static str,
    }
}

impl NagamiSourceSnapshot {
    pub fn new(metadata: SourceFetchMetadata) -> Self {
        Self {
            metadata,
            entries: Vec::new(),
            errors: Vec::new(),
            source: "nagami",
        }
    }

    pub fn summary(&self) -> Dict {
        dict(vec![
            ("entry_count", Value::Int(len(self.entries.len()))),
            ("error_count", Value::Int(len(self.errors.len()))),
        ])
    }

    pub fn to_dict(&self) -> Value {
        self.to_py()
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct AzurLaneSourceSnapshots {
        pub l2d_su: L2DSuSourceSnapshot,
        pub nagami: NagamiSourceSnapshot,
    }
}

impl AzurLaneSourceSnapshots {
    pub fn to_dict(&self) -> Value {
        self.to_py()
    }
}

// ---------- the catalog ----------

dataclass! {
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct ModelCharacter {
        pub key: String,
        pub id: Option<i128>,
        pub name_zh: String,
        pub name_en: String,
        pub nation: String,
        pub ship_type: String,
        pub rarity: String,
        pub class_name: String,
        pub default_skin_id: Option<i128>,
        pub skin_series: Vec<String>,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct ModelCostume {
        pub key: String,
        pub id: Option<i128>,
        pub name_zh: String,
        pub name_en: String,
        pub skin_type: String,
        pub feature_tags: Vec<String>,
        pub is_live2d_plus: bool,
        pub face_ids: Vec<String>,
        pub square_icon: String,
        pub shipyard_icon: String,
        pub q_icon: String,
        pub shop_type: String,
        /// (face id, absolute URL) pairs; only the hashed build names its face files this way.
        pub face_urls: Vec<(String, String)>,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct ModelResources {
        pub primary_url: String,
        pub fallback_url: String,
        pub display_info_url: String,
        /// The hashed build's painting index; empty where it sits beside the sheet as <key>.json.
        pub painting_index_url: String,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct ModelResourceSummary {
        pub moc3: String,
        pub skeleton: String,
        pub textures: Vec<String>,
        pub physics: String,
        pub display_info: String,
        pub motions: Vec<String>,
        pub expressions: Vec<String>,
        pub has_audio: bool,
        pub has_text: bool,
        pub has_display_info: bool,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct ModelAvailability {
        pub state: AvailabilityState,
        pub validated_url: String,
        pub checked_at: String,
        pub message: String,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct ModelEntry {
        pub id: String,
        pub r#type: ModelKind,
        pub source: CatalogSource,
        pub character: ModelCharacter,
        pub costume: ModelCostume,
        pub resources: ModelResources,
        pub resource_summary: ModelResourceSummary,
        pub availability: ModelAvailability,
    }
}

impl ModelEntry {
    pub fn new(
        id: impl Into<String>,
        kind: ModelKind,
        source: CatalogSource,
        character: ModelCharacter,
        costume: ModelCostume,
        resources: ModelResources,
    ) -> Self {
        Self {
            id: id.into(),
            r#type: kind,
            source,
            character,
            costume,
            resources,
            resource_summary: ModelResourceSummary::default(),
            availability: ModelAvailability::default(),
        }
    }

    pub fn to_dict(&self) -> Value {
        self.to_py()
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct AzurLaneEnumeratedResource {
        pub model_id: String,
        pub model_type: ModelKind,
        pub character_key: String,
        pub costume_key: String,
        pub kind: AssetKind,
        pub source_url: String,
        pub fallback_url: String,
        pub local_path: String,
        pub original_filename: String,
        pub contexts: Vec<Dict>,
    }
}

impl AzurLaneEnumeratedResource {
    /// `context`: the first context, or an empty one.
    pub fn context(&self) -> Dict {
        self.contexts.first().cloned().unwrap_or_default()
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct ResourceEnumerationIssue {
        pub model_id: String,
        pub kind: AssetKind,
        pub message: String,
        pub context: Dict,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct AzurLaneModelResourceEnumeration {
        pub model_id: String,
        pub model_type: ModelKind,
        pub character_key: String,
        pub costume_key: String,
        pub source_url: String,
        pub fallback_url: String,
        pub assets: Vec<AzurLaneEnumeratedResource>,
        pub issues: Vec<ResourceEnumerationIssue>,
    }
}

impl AzurLaneModelResourceEnumeration {
    pub fn to_dict(&self) -> Value {
        self.to_py()
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct ResourceCheck {
        pub kind: AssetKind,
        pub url: String,
        pub ok: bool,
        pub status: ValidationStatus,
        pub http_status: Option<i128>,
        pub message: String,
        pub source: Option<ValidationSource>,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct ResourceValidationEntry {
        pub entry_id: String,
        pub entry_type: ModelKind,
        pub availability: ModelAvailability,
        pub resources: ModelResources,
        pub resource_summary: ModelResourceSummary,
        pub checks: Vec<ResourceCheck>,
    }
}

impl ResourceValidationEntry {
    pub fn has_available_resources(&self) -> bool {
        matches!(self.availability.state, AvailabilityState::Valid | AvailabilityState::FallbackOnly)
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct NagamiFallbackCandidate {
        pub key: String,
        pub character_key: String,
        pub costume_key: String,
        pub name: String,
        pub character_name_en: String,
        pub costume_name_en: String,
        pub url: String,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct AzurLaneModelCatalog {
        pub entries: Vec<ModelEntry>,
        pub nagami_fallback_candidates: Vec<NagamiFallbackCandidate>,
    }
}

impl AzurLaneModelCatalog {
    pub fn summary(&self) -> Dict {
        let mut by_type = counts(ModelKind::ALL, ModelKind::as_str);
        let mut by_source = counts(CatalogSource::ALL, CatalogSource::as_str);
        let mut by_type_source: Vec<(ModelKind, Vec<(&'static str, i128)>)> = ModelKind::ALL
            .iter()
            .map(|kind| (*kind, counts(CatalogSource::ALL, CatalogSource::as_str)))
            .collect();
        for entry in &self.entries {
            bump(&mut by_type, entry.r#type.as_str());
            bump(&mut by_source, entry.source.as_str());
            if let Some((_, counts)) = by_type_source.iter_mut().find(|(kind, _)| *kind == entry.r#type) {
                bump(counts, entry.source.as_str());
            }
        }
        dict(vec![
            ("entry_count", Value::Int(len(self.entries.len()))),
            ("by_type", count_dict(&by_type)),
            ("by_source", count_dict(&by_source)),
            (
                "by_type_source",
                Value::Dict(
                    by_type_source
                        .iter()
                        .map(|(kind, counts)| (kind.as_str().to_owned(), count_dict(counts)))
                        .collect(),
                ),
            ),
            (
                "nagami_fallback_candidate_count",
                Value::Int(len(self.nagami_fallback_candidates.len())),
            ),
        ])
    }

    pub fn to_dict(&self) -> Value {
        self.to_py()
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct AzurLaneResourceValidationReport {
        pub checked_at: String,
        pub catalog: AzurLaneModelCatalog,
        pub entries: Vec<ResourceValidationEntry>,
    }
}

impl AzurLaneResourceValidationReport {
    pub fn summary(&self) -> Dict {
        let mut by_state = counts(AvailabilityState::ALL, AvailabilityState::as_str);
        let mut by_type_state: Vec<(ModelKind, Vec<(&'static str, i128)>)> = ModelKind::ALL
            .iter()
            .map(|kind| (*kind, counts(AvailabilityState::ALL, AvailabilityState::as_str)))
            .collect();
        for entry in &self.entries {
            bump(&mut by_state, entry.availability.state.as_str());
            if let Some((_, counts)) = by_type_state.iter_mut().find(|(kind, _)| *kind == entry.entry_type) {
                bump(counts, entry.availability.state.as_str());
            }
        }
        dict(vec![
            ("entry_count", Value::Int(len(self.entries.len()))),
            ("by_state", count_dict(&by_state)),
            (
                "by_type_state",
                Value::Dict(
                    by_type_state
                        .iter()
                        .map(|(kind, counts)| (kind.as_str().to_owned(), count_dict(counts)))
                        .collect(),
                ),
            ),
        ])
    }

    pub fn broken_entries(&self) -> Vec<&ResourceValidationEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.availability.state == AvailabilityState::Broken)
            .collect()
    }

    pub fn to_dict(&self) -> Value {
        self.to_py()
    }
}

// ---------- the health report ----------

pub(crate) const HTTP_ERROR_MIN: i128 = 400;

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct SourceEndpointHealth {
        pub source: &'static str,
        pub url: String,
        pub http_status: Option<i128>,
        pub etag: String,
        pub last_modified: String,
        pub fetched_at: String,
        pub entry_count: i128,
        pub entry_ids: Vec<String>,
        pub errors: Vec<SourceSnapshotError>,
    }
}

impl SourceEndpointHealth {
    pub fn ok(&self) -> bool {
        self.errors.is_empty() && self.http_status.is_some_and(|status| status < HTTP_ERROR_MIN)
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct SourceDrift {
        pub source: &'static str,
        pub previous_entry_count: Option<i128>,
        pub current_entry_count: i128,
        pub entry_count_delta: Option<i128>,
        pub added_entry_ids: Vec<String>,
        pub removed_entry_ids: Vec<String>,
        pub etag_changed: bool,
        pub last_modified_changed: bool,
        pub http_status_changed: bool,
    }
}

impl SourceDrift {
    pub fn changed(&self) -> bool {
        self.entry_count_delta.is_some_and(|delta| delta != 0)
            || !self.added_entry_ids.is_empty()
            || !self.removed_entry_ids.is_empty()
            || self.etag_changed
            || self.last_modified_changed
            || self.http_status_changed
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct SourceHealthReport {
        pub l2d_su: SourceEndpointHealth,
        pub nagami: SourceEndpointHealth,
    }
}

impl SourceHealthReport {
    pub fn to_dict(&self) -> Value {
        self.to_py()
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct CatalogHealthReport {
        pub entry_count: i128,
        pub by_type: Dict,
        pub by_source: Dict,
        pub entry_ids: Vec<String>,
        pub new_entry_ids: Vec<String>,
        pub removed_entry_ids: Vec<String>,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct ResourceHealthReport {
        pub checked_at: String,
        pub entry_count: i128,
        pub broken_resource_count: i128,
        pub unchecked_resource_count: i128,
        pub fallback_only_count: i128,
        pub broken_entry_ids: Vec<String>,
        pub newly_broken_entry_ids: Vec<String>,
        pub recovered_entry_ids: Vec<String>,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct DriftSummary {
        pub l2d_su: SourceDrift,
        pub nagami: SourceDrift,
        pub previous_catalog_entry_count: Option<i128>,
        pub current_catalog_entry_count: i128,
        pub catalog_entry_count_delta: Option<i128>,
        pub newly_added_resource_count: i128,
        pub broken_resource_count_delta: Option<i128>,
    }
}

impl DriftSummary {
    pub fn changed(&self) -> bool {
        self.l2d_su.changed()
            || self.nagami.changed()
            || self.catalog_entry_count_delta.is_some_and(|delta| delta != 0)
            || self.newly_added_resource_count != 0
            || self.broken_resource_count_delta.is_some_and(|delta| delta != 0)
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct AzurLaneL2DHealthReport {
        pub checked_at: String,
        pub source_health: SourceHealthReport,
        pub catalog_health: CatalogHealthReport,
        pub resource_health: ResourceHealthReport,
        pub drift_summary: DriftSummary,
        pub snapshots: Option<AzurLaneSourceSnapshots>,
        pub catalog: Option<AzurLaneModelCatalog>,
        pub resource_validation: Option<AzurLaneResourceValidationReport>,
    }
}

impl AzurLaneL2DHealthReport {
    pub fn to_dict(&self) -> Value {
        self.to_py()
    }
}

// ---------- resource manifests ----------

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct ResourceReference {
        pub kind: AssetKind,
        pub url: String,
        pub raw_path: String,
        pub context: Dict,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq, Default)]
    pub struct Live2DResourceManifest {
        pub moc3_url: String,
        pub texture_urls: Vec<String>,
        pub physics_url: String,
        pub pose_url: String,
        pub display_info_url: String,
        pub expression_urls: Vec<String>,
        pub motion_urls: Vec<String>,
        pub audio_urls: Vec<String>,
        pub motion_names: Vec<String>,
        pub expression_names: Vec<String>,
        pub has_audio: bool,
        pub has_text: bool,
        pub references: Vec<ResourceReference>,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct SpineModelPart {
        pub name: String,
        pub skeleton_url: String,
        pub atlas_urls: Vec<String>,
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct SpineResourceManifest {
        pub parts: Vec<SpineModelPart>,
        pub parts_manifest_url: String,
    }
}

/// One entry of `painting/<key>.json`.
///
/// A painting webp is a tight-packed sprite sheet: the mesh fragments with their transparent
/// space squeezed out. Reassembling the artwork needs `size` (the canvas the layer draws
/// into), `raw_size` (the sheet as stored, so `size / raw_size` is the downsample factor),
/// `position` (the layer's offset within the painting) and `pivot`, plus the mesh that maps
/// each packed triangle back to its place. `raw` layers are stored unpacked and have no mesh.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PaintingLayer {
    pub name: String,
    pub size: (f64, f64),
    pub raw_size: (f64, f64),
    pub position: (f64, f64),
    pub pivot: (f64, f64),
    pub raw: bool,
    /// The hashed build names each layer's sheet and mesh in the index instead of by layer name.
    pub image_url: String,
    pub mesh_url: String,
}

impl PaintingLayer {
    /// The origin's own field names, so a consumer parses this exactly as it parses the CDN copy.
    pub fn to_dict(&self) -> Value {
        Value::Dict(dict(vec![
            ("name", Value::Str(self.name.clone())),
            ("size", self.size.to_py()),
            ("rawSize", self.raw_size.to_py()),
            ("position", self.position.to_py()),
            ("pivot", self.pivot.to_py()),
            ("raw", Value::Bool(self.raw)),
        ]))
    }
}

/// `painting/<key>.json`: the drawing layers plus the `face` slot.
///
/// Layers keep source order, because a jsonb column does not preserve object key order and
/// the order is what decides which layer is drawn over which.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PaintingIndex {
    pub painting_key: String,
    pub index_url: String,
    pub layers: Vec<PaintingLayer>,
    pub face: Option<PaintingLayer>,
}

impl PaintingIndex {
    pub fn to_dict(&self) -> Value {
        let mut payload = dict(vec![
            ("painting_key", Value::Str(self.painting_key.clone())),
            ("index_url", Value::Str(self.index_url.clone())),
            ("layers", Value::List(self.layers.iter().map(PaintingLayer::to_dict).collect())),
        ]);
        if let Some(face) = &self.face {
            payload.insert("face", face.to_dict());
        }
        Value::Dict(payload)
    }
}

dataclass! {
    #[derive(Debug, Clone, PartialEq)]
    pub struct ResourceCandidateValidation {
        pub ok: bool,
        pub url: String,
        pub resource_summary: ModelResourceSummary,
        pub display_info_url: String,
        pub checks: Vec<ResourceCheck>,
        pub message: String,
    }
}

dataclass! {
    /// Outcome of a connectivity check against the l2d.su origin.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct L2DSuOriginProbe {
        pub ok: bool,
        pub code: String,
        pub message: String,
        pub exit_ip: String,
    }
}

/// `ArchivedModel`: a model already in the archive, as the catalog last described it.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchivedModel {
    pub entry: ModelEntry,
    pub completed: bool,
}
