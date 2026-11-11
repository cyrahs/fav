//! The BD2 response models (`BD2*` of `src/api/schemas.py`) on the shared table of
//! [`crate::schema`].

use crate::live2d_overrides::LIVE2D_VIEW_OVERRIDE_VALUE;
use crate::schema::{Default, Model, Ty, field};

pub(crate) static BD2_ASSET: Model = Model {
    name: "BD2Asset",
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
        field("style_index", Ty::Nullable(&Ty::Int), Default::Null),
        field("style_name", Ty::Str, Default::Str("")),
        field("costume_title", Ty::Str, Default::Str("")),
        field("costume_category", Ty::Str, Default::Str("")),
        field("column_role", Ty::Str, Default::Str("")),
        field("live2d_key", Ty::Str, Default::Str("")),
        field("available", Ty::Bool, Default::Bool(false)),
        field("contexts", Ty::ListOf(&Ty::FreeDict), Default::EmptyList),
    ],
};

pub(crate) static BD2_PROFILE_ITEM: Model = Model {
    name: "BD2ProfileItem",
    base: None,
    fields: &[
        field("key", Ty::Str, Default::Required),
        field("label", Ty::Str, Default::Required),
        field("value", Ty::Str, Default::Str("")),
        field("asset", Ty::Nullable(&Ty::Model(&BD2_ASSET)), Default::Null),
    ],
};

pub(crate) static BD2_CHARACTER_SUMMARY: Model = Model {
    name: "BD2CharacterSummary",
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
        field("profile", Ty::ListOf(&Ty::Model(&BD2_PROFILE_ITEM)), Default::EmptyList),
        field("icon", Ty::Nullable(&Ty::Model(&BD2_ASSET)), Default::Null),
        field("portrait", Ty::Nullable(&Ty::Model(&BD2_ASSET)), Default::Null),
        field("costume_count", Ty::Int, Default::Int(0)),
        field("live2d_model_count", Ty::Int, Default::Int(0)),
        field("search_terms", Ty::ListOf(&Ty::Str), Default::EmptyList),
    ],
};

pub(crate) static BD2_LIVE2D_MODEL: Model = Model {
    name: "BD2Live2DModel",
    base: None,
    fields: &[
        field("model_id", Ty::Str, Default::Str("")),
        field("label", Ty::Str, Default::Str("")),
        field("section", Ty::Str, Default::Str("")),
        field("style_index", Ty::Nullable(&Ty::Int), Default::Null),
        field("style_name", Ty::Str, Default::Str("")),
        field("costume_title", Ty::Str, Default::Str("")),
        field("costume_category", Ty::Str, Default::Str("")),
        field("row_index", Ty::Nullable(&Ty::Int), Default::Null),
        field("column_index", Ty::Nullable(&Ty::Int), Default::Null),
        field("field", Ty::Str, Default::Str("")),
        field("is_art_row", Ty::Bool, Default::Bool(false)),
        field("column_name", Ty::Str, Default::Str("")),
        field("column_category", Ty::Str, Default::Str("")),
        field("column_role", Ty::Str, Default::Str("")),
        field("column_header", Ty::Str, Default::Str("")),
        field("key", Ty::Str, Default::Str("")),
        field("stable_id", Ty::Str, Default::Str("")),
        field("live2d_key", Ty::Str, Default::Str("")),
        field("animation", Ty::Str, Default::Str("")),
        field("skin", Ty::Str, Default::Str("")),
        field("limit_age", Ty::Bool, Default::Bool(false)),
        field("source", Ty::Str, Default::Str("")),
        field("variant", Ty::Str, Default::Str("")),
        field("supplement_reason", Ty::Str, Default::Str("")),
        field("viewer_entry_id", Ty::Str, Default::Str("")),
        field("viewer_stem", Ty::Str, Default::Str("")),
        field("source_page_url", Ty::Str, Default::Str("")),
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

pub(crate) static BD2_COSTUME: Model = Model {
    name: "BD2Costume",
    base: None,
    fields: &[
        field("style_index", Ty::Nullable(&Ty::Int), Default::Null),
        field("style_name", Ty::Str, Default::Str("")),
        field("title", Ty::Str, Default::Str("")),
        field("category", Ty::Str, Default::Str("")),
        field("sprite", Ty::Nullable(&Ty::Model(&BD2_ASSET)), Default::Null),
        field("portrait", Ty::Nullable(&Ty::Model(&BD2_ASSET)), Default::Null),
        field("full_portrait", Ty::Nullable(&Ty::Model(&BD2_ASSET)), Default::Null),
        field("gallery", Ty::ListOf(&Ty::Model(&BD2_ASSET)), Default::EmptyList),
        field("videos", Ty::ListOf(&Ty::Model(&BD2_ASSET)), Default::EmptyList),
        field("audio", Ty::ListOf(&Ty::Model(&BD2_ASSET)), Default::EmptyList),
        field("live2d_models", Ty::ListOf(&Ty::Model(&BD2_LIVE2D_MODEL)), Default::EmptyList),
        field("rows", Ty::ListOf(&Ty::FreeDict), Default::EmptyList),
    ],
};

pub(crate) static BD2_CHARACTER_DETAIL: Model = Model {
    name: "BD2CharacterDetail",
    base: Some(&BD2_CHARACTER_SUMMARY),
    fields: &[
        field("tree_row", Ty::Nullable(&Ty::FreeDict), Default::Null),
        field("base_info", Ty::FreeDict, Default::EmptyDict),
        field("costumes", Ty::ListOf(&Ty::Model(&BD2_COSTUME)), Default::EmptyList),
        field("live2d_models", Ty::ListOf(&Ty::Model(&BD2_LIVE2D_MODEL)), Default::EmptyList),
        field("assets", Ty::ListOf(&Ty::Model(&BD2_ASSET)), Default::EmptyList),
    ],
};

pub(crate) static BD2_CHARACTER_LIST_RESPONSE: Model = Model {
    name: "BD2CharacterListResponse",
    base: None,
    fields: &[
        field("items", Ty::ListOf(&Ty::Model(&BD2_CHARACTER_SUMMARY)), Default::Required),
        field("total", Ty::Int, Default::Required),
        field("limit", Ty::Nullable(&Ty::Int), Default::Null),
        field("offset", Ty::Int, Default::Int(0)),
    ],
};

pub(crate) static BD2_SIDEBAR_ICON: Model = Model {
    name: "BD2SidebarIcon",
    base: None,
    fields: &[
        field("url", Ty::Str, Default::Required),
        field("available", Ty::Bool, Default::Required),
        field("sha256", Ty::Str, Default::Required),
        field("content_type", Ty::Str, Default::Required),
    ],
};

pub(crate) static BD2_SIDEBAR_CHARACTER: Model = Model {
    name: "BD2SidebarCharacter",
    base: None,
    fields: &[
        field("content_id", Ty::Int, Default::Required),
        field("title", Ty::Str, Default::Required),
        field("icon", Ty::Model(&BD2_SIDEBAR_ICON), Default::Required),
        field("updated_at", Ty::Nullable(&Ty::IntOrStr), Default::Null),
        field("fetched_at", Ty::Nullable(&Ty::Str), Default::Null),
    ],
};

pub(crate) static BD2_SIDEBAR_CHARACTER_LIST_RESPONSE: Model = Model {
    name: "BD2SidebarCharacterListResponse",
    base: None,
    fields: &[
        field("items", Ty::ListOf(&Ty::Model(&BD2_SIDEBAR_CHARACTER)), Default::Required),
        field("total", Ty::Int, Default::Required),
    ],
};

/// Every BD2 model of the document.
pub(crate) const MODELS: [&Model; 10] = [
    &BD2_ASSET,
    &BD2_CHARACTER_DETAIL,
    &BD2_CHARACTER_LIST_RESPONSE,
    &BD2_CHARACTER_SUMMARY,
    &BD2_COSTUME,
    &BD2_LIVE2D_MODEL,
    &BD2_PROFILE_ITEM,
    &BD2_SIDEBAR_CHARACTER,
    &BD2_SIDEBAR_CHARACTER_LIST_RESPONSE,
    &BD2_SIDEBAR_ICON,
];
