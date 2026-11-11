//! The Nikke response models (`Nikke*` of `src/api/schemas.py`) on the shared table of
//! [`crate::schema`]; `NikkeLive2DModel.view_overrides` refers to
//! `Live2DViewOverrideValue`, which [`crate::live2d_overrides`] declares.

use crate::live2d_overrides::LIVE2D_VIEW_OVERRIDE_VALUE;
use crate::schema::{Default, Model, Ty, field};

const CONFIDENCE: &[&str] = &["high", "medium", "low"];
const METADATA_STATUS: &[&str] = &["complete", "incomplete", "error", "missing"];

pub(crate) static NIKKE_ASSET: Model = Model {
    name: "NikkeAsset",
    base: None,
    fields: &[
        field("kind", Ty::Str, Default::Required),
        field("path", Ty::Str, Default::Required),
        field("url", Ty::Str, Default::Required),
        field("content_type", Ty::Str, Default::Str("")),
        field("size", Ty::Int, Default::Int(0)),
        field("sha256", Ty::Str, Default::Str("")),
        field("status", Ty::Str, Default::Str("")),
        field("label", Ty::Str, Default::Str("")),
        field("field", Ty::Str, Default::Str("")),
        field("skin_index", Ty::Nullable(&Ty::Int), Default::Null),
        field("live2d_key", Ty::Str, Default::Str("")),
        field("available", Ty::Bool, Default::Bool(false)),
        field("contexts", Ty::ListOf(&Ty::FreeDict), Default::EmptyList),
    ],
};

pub(crate) static NIKKE_PROFILE_ITEM: Model = Model {
    name: "NikkeProfileItem",
    base: None,
    fields: &[
        field("key", Ty::Str, Default::Required),
        field("label", Ty::Str, Default::Required),
        field("value", Ty::Str, Default::Str("")),
        field("asset", Ty::Nullable(&Ty::Model(&NIKKE_ASSET)), Default::Null),
    ],
};

pub(crate) static NIKKE_CHARACTER_SUMMARY: Model = Model {
    name: "NikkeCharacterSummary",
    base: None,
    fields: &[
        field("content_id", Ty::Int, Default::Required),
        field("title", Ty::Str, Default::Required),
        field("directory_name", Ty::Str, Default::Str("")),
        field("source_url", Ty::Str, Default::Str("")),
        field("updated_at", Ty::Nullable(&Ty::IntOrStr), Default::Null),
        field("fetched_at", Ty::Nullable(&Ty::Str), Default::Null),
        field("asset_counts", Ty::DictOf(&Ty::Int), Default::EmptyDict),
        field("tags", Ty::DictOf(&Ty::Str), Default::EmptyDict),
        field("profile", Ty::ListOf(&Ty::Model(&NIKKE_PROFILE_ITEM)), Default::EmptyList),
        field("icon", Ty::Nullable(&Ty::Model(&NIKKE_ASSET)), Default::Null),
        field("portrait", Ty::Nullable(&Ty::Model(&NIKKE_ASSET)), Default::Null),
        field("skin_count", Ty::Int, Default::Int(0)),
        field("live2d_model_count", Ty::Int, Default::Int(0)),
    ],
};

pub(crate) static NIKKE_LAYER_METADATA_CAPTURE: Model = Model {
    name: "NikkeLayerMetadataCapture",
    base: None,
    fields: &[
        field("status", Ty::Str, Default::Str("")),
        field("captured_at", Ty::Nullable(&Ty::Str), Default::Null),
        field("attempted_at", Ty::Nullable(&Ty::Str), Default::Null),
        field("capture_hash", Ty::Str, Default::Str("")),
        field("fingerprint", Ty::Str, Default::Str("")),
        field("reason", Ty::Str, Default::Str("")),
        field("error_class", Ty::Str, Default::Str("")),
        field("retryable", Ty::Nullable(&Ty::Bool), Default::Null),
        field("warnings", Ty::ListOf(&Ty::Str), Default::EmptyList),
    ],
};

pub(crate) static NIKKE_RUNTIME_ANIMATION: Model = Model {
    name: "NikkeRuntimeAnimation",
    base: None,
    fields: &[
        field("animation", Ty::Str, Default::Str("")),
        field("enabled", Ty::Bool, Default::Bool(true)),
        field("loop", Ty::Bool, Default::Bool(true)),
        field("match_method", Ty::Str, Default::Str("")),
        field("match_confidence", Ty::Nullable(&Ty::Literal(CONFIDENCE)), Default::Null),
        field("duration_ms", Ty::Nullable(&Ty::NonNegInt), Default::Null),
    ],
};

pub(crate) static NIKKE_RUNTIME_ANIMATIONS: Model = Model {
    name: "NikkeRuntimeAnimations",
    base: None,
    fields: &[
        field("idle", Ty::Nullable(&Ty::Model(&NIKKE_RUNTIME_ANIMATION)), Default::Null),
        field("click", Ty::Nullable(&Ty::Model(&NIKKE_RUNTIME_ANIMATION)), Default::Null),
    ],
};

pub(crate) static NIKKE_CHARACTER_LIST_RESPONSE: Model = Model {
    name: "NikkeCharacterListResponse",
    base: None,
    fields: &[
        field("items", Ty::ListOf(&Ty::Model(&NIKKE_CHARACTER_SUMMARY)), Default::Required),
        field("total", Ty::Int, Default::Required),
        field("limit", Ty::Nullable(&Ty::Int), Default::Null),
        field("offset", Ty::Int, Default::Int(0)),
    ],
};

pub(crate) static NIKKE_SIDEBAR_ICON: Model = Model {
    name: "NikkeSidebarIcon",
    base: None,
    fields: &[
        field("url", Ty::Str, Default::Required),
        field("available", Ty::Bool, Default::Required),
        field("sha256", Ty::Str, Default::Required),
        field("content_type", Ty::Str, Default::Required),
    ],
};

pub(crate) static NIKKE_SIDEBAR_CHARACTER: Model = Model {
    name: "NikkeSidebarCharacter",
    base: None,
    fields: &[
        field("content_id", Ty::Int, Default::Required),
        field("title", Ty::Str, Default::Required),
        field("icon", Ty::Model(&NIKKE_SIDEBAR_ICON), Default::Required),
        field("implemented_at", Ty::Nullable(&Ty::Str), Default::Null),
        field("updated_at", Ty::Nullable(&Ty::IntOrStr), Default::Null),
        field("fetched_at", Ty::Nullable(&Ty::Str), Default::Null),
    ],
};

pub(crate) static NIKKE_SIDEBAR_CHARACTER_LIST_RESPONSE: Model = Model {
    name: "NikkeSidebarCharacterListResponse",
    base: None,
    fields: &[
        field("items", Ty::ListOf(&Ty::Model(&NIKKE_SIDEBAR_CHARACTER)), Default::Required),
        field("total", Ty::Int, Default::Required),
    ],
};

pub(crate) static NIKKE_LIVE2D_MODEL: Model = Model {
    name: "NikkeLive2DModel",
    base: None,
    fields: &[
        field("model_id", Ty::Str, Default::Str("")),
        field("label", Ty::Str, Default::Str("")),
        field("section", Ty::Str, Default::Str("")),
        field("row_index", Ty::Nullable(&Ty::Int), Default::Null),
        field("skin_index", Ty::Nullable(&Ty::Int), Default::Null),
        field("skin_name", Ty::Str, Default::Str("")),
        field("skin_title", Ty::Str, Default::Str("")),
        field("skin_series", Ty::Str, Default::Str("")),
        field("skin_obtain", Ty::Str, Default::Str("")),
        field("is_collection_skin", Ty::Bool, Default::Bool(false)),
        field("key", Ty::Str, Default::Str("")),
        field("stable_id", Ty::Str, Default::Str("")),
        field("live2d_key", Ty::Str, Default::Str("")),
        field("animation", Ty::Str, Default::Str("")),
        field("skin", Ty::Str, Default::Str("")),
        field("limit_age", Ty::Bool, Default::Bool(false)),
        field("layer_order", Ty::Nullable(&Ty::Int), Default::Null),
        field("source_z_index", Ty::Nullable(&Ty::Int), Default::Null),
        field("source_layer_index", Ty::Nullable(&Ty::Int), Default::Null),
        field("is_primary", Ty::Nullable(&Ty::Bool), Default::Null),
        field("layer_match_method", Ty::Str, Default::Str("")),
        field("layer_match_confidence", Ty::Nullable(&Ty::Literal(CONFIDENCE)), Default::Null),
        field(
            "runtime_animations",
            Ty::Nullable(&Ty::Model(&NIKKE_RUNTIME_ANIMATIONS)),
            Default::Null,
        ),
        field("runtime_animation_match_method", Ty::Str, Default::Str("")),
        field(
            "runtime_animation_match_confidence",
            Ty::Nullable(&Ty::Literal(CONFIDENCE)),
            Default::Null,
        ),
        field("position", Ty::FreeDict, Default::EmptyDict),
        field("bg_position", Ty::FreeDict, Default::EmptyDict),
        field(
            "view_overrides",
            Ty::DictOf(&Ty::Model(&LIVE2D_VIEW_OVERRIDE_VALUE)),
            Default::EmptyDict,
        ),
        field("source_urls", Ty::FreeDict, Default::EmptyDict),
        field("spine_version", Ty::Nullable(&Ty::Str), Default::Null),
        field("assets", Ty::FreeDict, Default::EmptyDict),
    ],
};

pub(crate) static NIKKE_VOICE_LINE: Model = Model {
    name: "NikkeVoiceLine",
    base: None,
    fields: &[
        field("label", Ty::Str, Default::Required),
        field("text", Ty::Str, Default::Str("")),
        field("source_url", Ty::Str, Default::Str("")),
    ],
};

pub(crate) static NIKKE_SKIN: Model = Model {
    name: "NikkeSkin",
    base: None,
    fields: &[
        field("skin_index", Ty::Nullable(&Ty::Int), Default::Null),
        field("name", Ty::Str, Default::Str("")),
        field("title", Ty::Str, Default::Str("")),
        field("series", Ty::Str, Default::Str("")),
        field("obtain", Ty::Str, Default::Str("")),
        field("is_collection_skin", Ty::Bool, Default::Bool(false)),
        field("thumbnail", Ty::Nullable(&Ty::Model(&NIKKE_ASSET)), Default::Null),
        field("portrait", Ty::Nullable(&Ty::Model(&NIKKE_ASSET)), Default::Null),
        field("sd_model", Ty::Nullable(&Ty::Model(&NIKKE_ASSET)), Default::Null),
        field("burst_animation", Ty::Nullable(&Ty::Model(&NIKKE_ASSET)), Default::Null),
        field("gallery", Ty::ListOf(&Ty::Model(&NIKKE_ASSET)), Default::EmptyList),
        field("live2d_models", Ty::ListOf(&Ty::Model(&NIKKE_LIVE2D_MODEL)), Default::EmptyList),
        field("layer_metadata_required", Ty::Bool, Default::Bool(false)),
        field("layer_metadata_status", Ty::Literal(METADATA_STATUS), Default::Str("missing")),
        field("layer_metadata_issues", Ty::ListOf(&Ty::FreeDict), Default::EmptyList),
        field(
            "layer_metadata_capture",
            Ty::Model(&NIKKE_LAYER_METADATA_CAPTURE),
            Default::Factory(&NIKKE_LAYER_METADATA_CAPTURE),
        ),
        field("runtime_animation_metadata_required", Ty::Bool, Default::Bool(false)),
        field(
            "runtime_animation_metadata_status",
            Ty::Literal(METADATA_STATUS),
            Default::Str("missing"),
        ),
        field("runtime_animation_metadata_issues", Ty::ListOf(&Ty::FreeDict), Default::EmptyList),
        field("voice_lines", Ty::ListOf(&Ty::Model(&NIKKE_VOICE_LINE)), Default::EmptyList),
        field("rows", Ty::ListOf(&Ty::FreeDict), Default::EmptyList),
    ],
};

pub(crate) static NIKKE_CHARACTER_DETAIL: Model = Model {
    name: "NikkeCharacterDetail",
    base: Some(&NIKKE_CHARACTER_SUMMARY),
    fields: &[
        field("tj_list", Ty::Nullable(&Ty::FreeDict), Default::Null),
        field("base_info", Ty::FreeDict, Default::EmptyDict),
        field("skins", Ty::ListOf(&Ty::Model(&NIKKE_SKIN)), Default::EmptyList),
        field("live2d_models", Ty::ListOf(&Ty::Model(&NIKKE_LIVE2D_MODEL)), Default::EmptyList),
        field("layer_metadata_required", Ty::Bool, Default::Bool(false)),
        field("layer_metadata_status", Ty::Literal(METADATA_STATUS), Default::Str("missing")),
        field("layer_metadata_issues", Ty::ListOf(&Ty::FreeDict), Default::EmptyList),
        field(
            "layer_metadata_capture",
            Ty::Model(&NIKKE_LAYER_METADATA_CAPTURE),
            Default::Factory(&NIKKE_LAYER_METADATA_CAPTURE),
        ),
        field("runtime_animation_metadata_required", Ty::Bool, Default::Bool(false)),
        field(
            "runtime_animation_metadata_status",
            Ty::Literal(METADATA_STATUS),
            Default::Str("missing"),
        ),
        field("runtime_animation_metadata_issues", Ty::ListOf(&Ty::FreeDict), Default::EmptyList),
        field("assets", Ty::ListOf(&Ty::Model(&NIKKE_ASSET)), Default::EmptyList),
    ],
};

/// Every Nikke model of the document.
pub(crate) const MODELS: [&Model; 14] = [
    &NIKKE_ASSET,
    &NIKKE_CHARACTER_DETAIL,
    &NIKKE_CHARACTER_LIST_RESPONSE,
    &NIKKE_CHARACTER_SUMMARY,
    &NIKKE_LAYER_METADATA_CAPTURE,
    &NIKKE_LIVE2D_MODEL,
    &NIKKE_PROFILE_ITEM,
    &NIKKE_RUNTIME_ANIMATION,
    &NIKKE_RUNTIME_ANIMATIONS,
    &NIKKE_SIDEBAR_CHARACTER,
    &NIKKE_SIDEBAR_CHARACTER_LIST_RESPONSE,
    &NIKKE_SIDEBAR_ICON,
    &NIKKE_SKIN,
    &NIKKE_VOICE_LINE,
];
