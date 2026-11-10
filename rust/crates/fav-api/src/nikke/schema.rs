//! The Nikke response models: their pydantic validation and their JSON Schema.
//!
//! Python origin: the `Nikke*` models of `src/api/schemas.py` (`NikkeLive2DModel.view_overrides` refers to
//! `Live2DViewOverrideValue`, which [`crate::live2d_overrides`] declares). Python
//! builds a payload as plain dicts and validates it into the response model
//! (`NikkeCharacterSummary.model_validate(payload)`); the response then carries every field
//! in the model's order, defaults filled in, values coerced the lax way (`"3"` for an
//! `int`), and a payload the model refuses fails the request with a 500. One field table
//! per model drives both that validation ([`validate`]) and the components of the OpenAPI
//! document ([`components`]), so the two cannot drift apart.

use fav_core::pyjson::{Dict, Value};
use fav_core::validate;
use serde_json::json;
use utoipa::openapi::schema::{AdditionalProperties, AnyOfBuilder, ArrayBuilder, ObjectBuilder, Schema, Type};
use utoipa::openapi::{Ref, RefOr};

/// A field's type.
#[derive(Debug)]
pub(crate) enum Ty {
    Str,
    Int,
    Bool,
    /// `int` with `ge=0`.
    NonNegInt,
    /// `Literal[...]` of strings.
    Literal(&'static [&'static str]),
    /// `int | str` (a smart union).
    IntOrStr,
    /// `dict[str, Any]`.
    FreeDict,
    /// `dict[str, T]`.
    DictOf(&'static Ty),
    /// `list[T]`.
    ListOf(&'static Ty),
    Model(&'static Model),
    /// A model declared elsewhere: its component name and its validation
    /// (`Live2DViewOverrideValue`, which [`crate::live2d_overrides`] owns).
    External(&'static str, fn(&Value) -> Result<Value, String>),
    /// `T | None`.
    Nullable(&'static Ty),
}

/// A field's default; `Required` fields have none.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Default {
    Required,
    Null,
    Str(&'static str),
    Int(i64),
    Bool(bool),
    /// `Field(default_factory=list)` / `dict`.
    EmptyList,
    EmptyDict,
    /// `Field(default_factory=Model)`.
    Factory(&'static Model),
}

#[derive(Debug)]
pub(crate) struct Field {
    pub name: &'static str,
    pub ty: Ty,
    pub default: Default,
}

/// A model (`ApiSchema`, so `extra='forbid'`); `base` is the parent class whose fields
/// come first.
#[derive(Debug)]
pub(crate) struct Model {
    pub name: &'static str,
    pub base: Option<&'static Model>,
    pub fields: &'static [Field],
}

impl Model {
    fn all_fields(&self) -> Vec<&'static Field> {
        let mut fields = self.base.map(Model::all_fields).unwrap_or_default();
        fields.extend(self.fields.iter());
        fields
    }
}

const fn field(name: &'static str, ty: Ty, default: Default) -> Field {
    Field { name, ty, default }
}

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
            Ty::DictOf(&Ty::External(
                "Live2DViewOverrideValue",
                crate::live2d_overrides::override_value_model,
            )),
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

/// Every model of the document, for [`components`].
const MODELS: [&Model; 14] = [
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

// ---------- validation ----------

/// Why a payload does not fit its model: the first error pydantic would list, as
/// `loc: message`.
fn failure(loc: &str, message: &str) -> String {
    format!("{loc}: {message}")
}

fn child(loc: &str, name: &str) -> String {
    if loc.is_empty() { name.to_owned() } else { format!("{loc}.{name}") }
}

fn from_errors(loc: &str, errors: &[validate::ErrorItem]) -> String {
    failure(loc, errors.first().map_or("invalid", |error| error.msg.as_str()))
}

/// `Model.model_validate(value)` dumped as `model_dump(mode='json')`: the fields in the
/// model's order, defaults filled in, values coerced; `Err` names what pydantic refuses.
pub(crate) fn validate(model: &Model, value: &Value) -> Result<Value, String> {
    validate_model(model, value, "").map(Value::Dict)
}

fn validate_model(model: &Model, value: &Value, loc: &str) -> Result<Dict, String> {
    let Value::Dict(input) = value else {
        return Err(failure(loc, "Input should be a valid dictionary or instance of the model"));
    };
    let fields = model.all_fields();
    if let Some((key, _)) = input
        .iter()
        .find(|(key, _)| !fields.iter().any(|field| fav_runtime::pystr::str_key(key) == field.name))
    {
        return Err(failure(
            &child(loc, &fav_runtime::pystr::str_key(key)),
            "Extra inputs are not permitted",
        ));
    }
    let mut out = Dict::new();
    for field in fields {
        let loc = child(loc, field.name);
        let value = match input.get(field.name) {
            Some(value) => validate_ty(&field.ty, value, &loc)?,
            None => default_value(field.default).ok_or_else(|| failure(&loc, "Field required"))?,
        };
        out.insert(field.name, value);
    }
    Ok(out)
}

fn default_value(default: Default) -> Option<Value> {
    Some(match default {
        Default::Required => return None,
        Default::Null => Value::Null,
        Default::Str(text) => Value::from(text),
        Default::Int(int) => Value::Int(i128::from(int)),
        Default::Bool(flag) => Value::Bool(flag),
        Default::EmptyList => Value::List(Vec::new()),
        Default::EmptyDict => Value::Dict(Dict::new()),
        Default::Factory(model) => Value::Dict(validate_model(model, &Value::Dict(Dict::new()), "").ok()?),
    })
}

fn validate_ty(ty: &Ty, value: &Value, loc: &str) -> Result<Value, String> {
    match ty {
        Ty::Str => match value {
            Value::Str(_) => Ok(value.clone()),
            _ => Err(failure(loc, "Input should be a valid string")),
        },
        Ty::Int => validate::int_unbounded(value, &[])
            .map(Value::Int)
            .map_err(|errors| from_errors(loc, &errors)),
        Ty::NonNegInt => match validate::int_unbounded(value, &[]) {
            Ok(int) if int >= 0 => Ok(Value::Int(int)),
            Ok(_) => Err(failure(loc, "Input should be greater than or equal to 0")),
            Err(errors) => Err(from_errors(loc, &errors)),
        },
        Ty::Bool => validate::boolean(value, &[])
            .map(Value::Bool)
            .map_err(|errors| from_errors(loc, &errors)),
        Ty::Literal(values) => match value {
            Value::Str(text) if values.contains(&text.as_str()) => Ok(value.clone()),
            _ => Err(failure(loc, "Input should be one of the literal values")),
        },
        // Smart mode: an exact `int` or `str` first, then the lax `int` (`True`, `1.0`).
        Ty::IntOrStr => match value {
            Value::Int(_) | Value::Str(_) => Ok(value.clone()),
            other => validate::int_unbounded(other, &[])
                .map(Value::Int)
                .map_err(|_| failure(loc, "Input should be a valid integer or string")),
        },
        Ty::FreeDict => match value {
            Value::Dict(_) => Ok(value.clone()),
            _ => Err(failure(loc, "Input should be a valid dictionary")),
        },
        Ty::DictOf(item) => match value {
            Value::Dict(input) => {
                let mut out = Dict::new();
                for (key, value) in input.iter() {
                    out.insert(
                        key.clone(),
                        validate_ty(item, value, &child(loc, &fav_runtime::pystr::str_key(key)))?,
                    );
                }
                Ok(Value::Dict(out))
            }
            _ => Err(failure(loc, "Input should be a valid dictionary")),
        },
        Ty::ListOf(item) => match value {
            Value::List(items) => items
                .iter()
                .enumerate()
                .map(|(index, value)| validate_ty(item, value, &child(loc, &index.to_string())))
                .collect::<Result<_, _>>()
                .map(Value::List),
            _ => Err(failure(loc, "Input should be a valid list")),
        },
        Ty::Model(model) => validate_model(model, value, loc).map(Value::Dict),
        Ty::External(_, convert) => convert(value).map_err(|error| failure(loc, &error)),
        Ty::Nullable(inner) => match value {
            Value::Null => Ok(Value::Null),
            other => validate_ty(inner, other, loc),
        },
    }
}

// ---------- JSON Schema ----------

/// pydantic's field title: `name.replace('_', ' ').title()` (`live2d_key` is `Live2D Key`).
pub(crate) fn title(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut previous_cased = false;
    for ch in name.replace('_', " ").chars() {
        if previous_cased {
            out.extend(ch.to_lowercase());
        } else {
            out.extend(ch.to_uppercase());
        }
        previous_cased = ch.is_alphabetic();
    }
    out
}

fn reference(model: &Model) -> RefOr<Schema> {
    Ref::from_schema_name(model.name).into()
}

fn bare(kind: Type) -> ObjectBuilder {
    ObjectBuilder::new().schema_type(kind)
}

/// The schema of a value inside a container or a union: no title, no default.
fn item_schema(ty: &Ty) -> RefOr<Schema> {
    match ty {
        Ty::Str => bare(Type::String).into(),
        Ty::Int | Ty::IntOrStr => bare(Type::Integer).into(),
        Ty::Bool => bare(Type::Boolean).into(),
        // pydantic writes `ge=0` as the float `0.0`, which utoipa's `minimum` would turn
        // into an integer.
        Ty::NonNegInt => bare(Type::Integer)
            .extensions(Some(super_extensions(&[("minimum", json!(0.0))])))
            .into(),
        Ty::Literal(values) => bare(Type::String).enum_values(Some(values.iter().copied())).into(),
        Ty::FreeDict | Ty::Nullable(_) => bare(Type::Object)
            .additional_properties(Some(AdditionalProperties::FreeForm(true)))
            .into(),
        Ty::DictOf(item) => bare(Type::Object)
            .additional_properties(Some(AdditionalProperties::RefOr(item_schema(item))))
            .into(),
        Ty::ListOf(item) => ArrayBuilder::new().items(item_schema(item)).into(),
        Ty::Model(model) => reference(model),
        Ty::External(name, _) => Ref::from_schema_name(*name).into(),
    }
}

fn super_extensions(pairs: &[(&str, serde_json::Value)]) -> utoipa::openapi::extensions::Extensions {
    pairs.iter().map(|(key, value)| (*key, value.clone())).collect()
}

fn default_json(default: Default) -> Option<serde_json::Value> {
    match default {
        Default::Str(text) => Some(json!(text)),
        Default::Int(int) => Some(json!(int)),
        Default::Bool(flag) => Some(json!(flag)),
        _ => None,
    }
}

/// A field's property schema, as pydantic writes it.
fn field_schema(field: &Field) -> RefOr<Schema> {
    let title = title(field.name);
    let titled = |builder: ObjectBuilder| {
        let builder = builder.title(Some(title.clone()));
        match default_json(field.default) {
            Some(default) => builder.default(Some(default)).into(),
            None => builder.into(),
        }
    };
    match &field.ty {
        Ty::Str => titled(bare(Type::String)),
        Ty::Int => titled(bare(Type::Integer)),
        Ty::Bool => titled(bare(Type::Boolean)),
        Ty::NonNegInt => titled(bare(Type::Integer).extensions(Some(super_extensions(&[("minimum", json!(0.0))])))),
        Ty::Literal(values) => titled(bare(Type::String).enum_values(Some(values.iter().copied()))),
        Ty::FreeDict => titled(bare(Type::Object).additional_properties(Some(AdditionalProperties::FreeForm(true)))),
        Ty::DictOf(item) => titled(bare(Type::Object).additional_properties(Some(AdditionalProperties::RefOr(item_schema(item))))),
        Ty::ListOf(item) => ArrayBuilder::new().items(item_schema(item)).title(Some(title)).into(),
        // A model field is a bare reference; a model-or-null is a union without a title.
        Ty::Model(model) => reference(model),
        Ty::External(name, _) => Ref::from_schema_name(*name).into(),
        Ty::Nullable(Ty::Model(model)) => AnyOfBuilder::new().item(reference(model)).item(bare(Type::Null)).into(),
        Ty::Nullable(inner) => {
            let mut union = AnyOfBuilder::new();
            if matches!(inner, Ty::IntOrStr) {
                union = union.item(bare(Type::Integer)).item(bare(Type::String));
            } else {
                union = union.item(item_schema(inner));
            }
            union
                .item(bare(Type::Null))
                .extensions(Some(super_extensions(&[("title", json!(title))])))
                .into()
        }
        Ty::IntOrStr => AnyOfBuilder::new()
            .item(bare(Type::Integer))
            .item(bare(Type::String))
            .extensions(Some(super_extensions(&[("title", json!(title))])))
            .into(),
    }
}

/// A model's component schema.
fn model_schema(model: &Model) -> RefOr<Schema> {
    let mut builder = bare(Type::Object).title(Some(model.name));
    for field in model.all_fields() {
        builder = builder.property(field.name, field_schema(field));
        if matches!(field.default, Default::Required) {
            builder = builder.required(field.name);
        }
    }
    builder.additional_properties(Some(AdditionalProperties::FreeForm(false))).into()
}

/// The components the Nikke routes add to the OpenAPI document.
pub(crate) fn components() -> Vec<(&'static str, RefOr<Schema>)> {
    MODELS.iter().map(|model| (model.name, model_schema(model))).collect()
}
