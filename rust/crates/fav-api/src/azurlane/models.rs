//! The Azur Lane response models (`AzurLane*` of `src/api/schemas.py`) on the shared table
//! of [`crate::schema`]. The proxy test's request and result are `openapi`'s, with the
//! other settings bodies.

use crate::schema::{Default, Model, Ty, field};

pub(crate) static AZURLANE_ASSET: Model = Model {
    name: "AzurLaneAsset",
    base: None,
    fields: &[
        field("kind", Ty::Str, Default::Required),
        field("path", Ty::Str, Default::Required),
        field("url", Ty::Str, Default::Required),
        field("source_url", Ty::Str, Default::Str("")),
        field("normalized_url", Ty::Str, Default::Str("")),
        field("downloaded_url", Ty::Str, Default::Str("")),
        field("fallback_url", Ty::Str, Default::Str("")),
        field("source_urls", Ty::FreeDict, Default::EmptyDict),
        field("original_filename", Ty::Str, Default::Str("")),
        field("content_type", Ty::Str, Default::Str("")),
        field("size", Ty::Int, Default::Int(0)),
        field("sha256", Ty::Str, Default::Str("")),
        field("status", Ty::Str, Default::Str("")),
        field("available", Ty::Bool, Default::Bool(false)),
        field("failed_count", Ty::Int, Default::Int(0)),
        field("error", Ty::Str, Default::Str("")),
        field("last_attempt_at", Ty::Nullable(&Ty::Str), Default::Null),
        field("next_retry_at", Ty::Nullable(&Ty::Str), Default::Null),
        field("last_seen_at", Ty::Nullable(&Ty::Str), Default::Null),
        field("model_id", Ty::Str, Default::Str("")),
        field("model_type", Ty::Str, Default::Str("")),
        field("character_key", Ty::Str, Default::Str("")),
        field("costume_key", Ty::Str, Default::Str("")),
        field("field", Ty::Str, Default::Str("")),
        field("catalog_source", Ty::Str, Default::Str("")),
        field("context_hashes", Ty::ListOf(&Ty::Str), Default::EmptyList),
        field("contexts", Ty::ListOf(&Ty::FreeDict), Default::EmptyList),
    ],
};

pub(crate) static AZURLANE_CHARACTER_SUMMARY: Model = Model {
    name: "AzurLaneCharacterSummary",
    base: None,
    fields: &[
        field("schema_version", Ty::Int, Default::Int(0)),
        field("source", Ty::Str, Default::Str("azurlane")),
        field("character_key", Ty::Str, Default::Required),
        field("source_id", Ty::Nullable(&Ty::Int), Default::Null),
        field("title", Ty::Str, Default::Required),
        field("display_name", Ty::Str, Default::Required),
        field("name_zh", Ty::Str, Default::Str("")),
        field("name_en", Ty::Str, Default::Str("")),
        field("directory_name", Ty::Str, Default::Str("")),
        field("source_metadata", Ty::FreeDict, Default::EmptyDict),
        field("fetched_at", Ty::Nullable(&Ty::Str), Default::Null),
        field("completed_at", Ty::Nullable(&Ty::Str), Default::Null),
        field("model_counts", Ty::DictOf(&Ty::Int), Default::EmptyDict),
        field("asset_counts", Ty::DictOf(&Ty::Int), Default::EmptyDict),
        field("source_counts", Ty::DictOf(&Ty::Int), Default::EmptyDict),
        field("tags", Ty::DictOf(&Ty::Str), Default::EmptyDict),
        field("representative_asset", Ty::Nullable(&Ty::Model(&AZURLANE_ASSET)), Default::Null),
        field("icon", Ty::Nullable(&Ty::Model(&AZURLANE_ASSET)), Default::Null),
        field("model_count", Ty::Int, Default::Int(0)),
    ],
};

pub(crate) static AZURLANE_CHARACTER_LIST_RESPONSE: Model = Model {
    name: "AzurLaneCharacterListResponse",
    base: None,
    fields: &[
        field("items", Ty::ListOf(&Ty::Model(&AZURLANE_CHARACTER_SUMMARY)), Default::Required),
        field("total", Ty::Int, Default::Required),
        field("limit", Ty::Nullable(&Ty::Int), Default::Null),
        field("offset", Ty::Int, Default::Int(0)),
    ],
};

pub(crate) static AZURLANE_SIDEBAR_CHARACTER: Model = Model {
    name: "AzurLaneSidebarCharacter",
    base: None,
    fields: &[
        field("character_key", Ty::Str, Default::Required),
        field("title", Ty::Str, Default::Required),
        field("display_name", Ty::Str, Default::Required),
        field("name_zh", Ty::Str, Default::Str("")),
        field("name_en", Ty::Str, Default::Str("")),
        field("representative_asset", Ty::Nullable(&Ty::Model(&AZURLANE_ASSET)), Default::Null),
        field("icon", Ty::Nullable(&Ty::Model(&AZURLANE_ASSET)), Default::Null),
        field("source_metadata", Ty::FreeDict, Default::EmptyDict),
        field("model_count", Ty::Int, Default::Int(0)),
        field("model_counts", Ty::DictOf(&Ty::Int), Default::EmptyDict),
        field("source_counts", Ty::DictOf(&Ty::Int), Default::EmptyDict),
        field("fetched_at", Ty::Nullable(&Ty::Str), Default::Null),
        field("completed_at", Ty::Nullable(&Ty::Str), Default::Null),
    ],
};

pub(crate) static AZURLANE_SIDEBAR_CHARACTER_LIST_RESPONSE: Model = Model {
    name: "AzurLaneSidebarCharacterListResponse",
    base: None,
    fields: &[
        field("items", Ty::ListOf(&Ty::Model(&AZURLANE_SIDEBAR_CHARACTER)), Default::Required),
        field("total", Ty::Int, Default::Required),
    ],
};

pub(crate) static AZURLANE_SKIN_UPDATE: Model = Model {
    name: "AzurLaneSkinUpdate",
    base: None,
    fields: &[
        field("ship_group_id", Ty::Int, Default::Int(0)),
        field("ship_name", Ty::Str, Default::Str("")),
        field("skin_name", Ty::Str, Default::Str("")),
        field("skin_type", Ty::Str, Default::Str("")),
        field("skin_ids", Ty::ListOf(&Ty::Int), Default::EmptyList),
    ],
};

pub(crate) static AZURLANE_SKIN_UPDATE_HISTORY_ENTRY: Model = Model {
    name: "AzurLaneSkinUpdateHistoryEntry",
    base: None,
    fields: &[
        field("date", Ty::Str, Default::Str("")),
        field("version", Ty::Str, Default::Str("")),
        field("skins", Ty::ListOf(&Ty::Model(&AZURLANE_SKIN_UPDATE)), Default::EmptyList),
    ],
};

pub(crate) static AZURLANE_SKIN_UPDATES_RESPONSE: Model = Model {
    name: "AzurLaneSkinUpdatesResponse",
    base: None,
    fields: &[
        field("game_version", Ty::Str, Default::Str("")),
        field("region", Ty::Str, Default::Str("")),
        field("generated_at", Ty::Str, Default::Str("")),
        field("new_skins", Ty::ListOf(&Ty::Model(&AZURLANE_SKIN_UPDATE)), Default::EmptyList),
        field(
            "skin_update_history",
            Ty::ListOf(&Ty::Model(&AZURLANE_SKIN_UPDATE_HISTORY_ENTRY)),
            Default::EmptyList,
        ),
    ],
};

pub(crate) static AZURLANE_COSTUME: Model = Model {
    name: "AzurLaneCostume",
    base: None,
    fields: &[
        field("key", Ty::Str, Default::Str("")),
        field("id", Ty::Nullable(&Ty::Int), Default::Null),
        field("name_zh", Ty::Str, Default::Str("")),
        field("name_en", Ty::Str, Default::Str("")),
    ],
};

pub(crate) static AZURLANE_MODEL: Model = Model {
    name: "AzurLaneModel",
    base: None,
    fields: &[
        field("model_id", Ty::Str, Default::Str("")),
        field("type", Ty::Str, Default::Str("")),
        field("source", Ty::Str, Default::Str("")),
        field("character_key", Ty::Str, Default::Str("")),
        field("costume", Ty::Model(&AZURLANE_COSTUME), Default::Factory(&AZURLANE_COSTUME)),
        field("source_urls", Ty::FreeDict, Default::EmptyDict),
        field("availability", Ty::FreeDict, Default::EmptyDict),
        field("source_metadata", Ty::FreeDict, Default::EmptyDict),
        field("fetched_at", Ty::Nullable(&Ty::Str), Default::Null),
        field("completed_at", Ty::Nullable(&Ty::Str), Default::Null),
        field("asset_counts", Ty::DictOf(&Ty::Int), Default::EmptyDict),
        field("files", Ty::FreeDict, Default::EmptyDict),
        field("assets", Ty::ListOf(&Ty::Model(&AZURLANE_ASSET)), Default::EmptyList),
    ],
};

pub(crate) static AZURLANE_CHARACTER_DETAIL: Model = Model {
    name: "AzurLaneCharacterDetail",
    base: Some(&AZURLANE_CHARACTER_SUMMARY),
    fields: &[
        field("active", Ty::Bool, Default::Bool(true)),
        field("models", Ty::ListOf(&Ty::Model(&AZURLANE_MODEL)), Default::EmptyList),
        field("live2d_models", Ty::ListOf(&Ty::Model(&AZURLANE_MODEL)), Default::EmptyList),
        field("spine_models", Ty::ListOf(&Ty::Model(&AZURLANE_MODEL)), Default::EmptyList),
        field("painting_models", Ty::ListOf(&Ty::Model(&AZURLANE_MODEL)), Default::EmptyList),
        field("assets", Ty::ListOf(&Ty::Model(&AZURLANE_ASSET)), Default::EmptyList),
    ],
};

/// Every Azur Lane collection model of the document.
pub(crate) const MODELS: [&Model; 11] = [
    &AZURLANE_ASSET,
    &AZURLANE_CHARACTER_DETAIL,
    &AZURLANE_CHARACTER_LIST_RESPONSE,
    &AZURLANE_CHARACTER_SUMMARY,
    &AZURLANE_COSTUME,
    &AZURLANE_MODEL,
    &AZURLANE_SIDEBAR_CHARACTER,
    &AZURLANE_SIDEBAR_CHARACTER_LIST_RESPONSE,
    &AZURLANE_SKIN_UPDATE,
    &AZURLANE_SKIN_UPDATE_HISTORY_ENTRY,
    &AZURLANE_SKIN_UPDATES_RESPONSE,
];
