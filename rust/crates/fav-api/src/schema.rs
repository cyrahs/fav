//! The response models: their pydantic validation and their JSON Schema.
//!
//! Python origin: the response models of `src/api/schemas.py` (`ApiSchema`, so
//! `extra='forbid'`). Python builds a payload as plain dicts and validates it into the
//! response model (`NikkeCharacterSummary.model_validate(payload)`); the response then
//! carries every field in the model's order, defaults filled in, values coerced the lax way
//! (`"3"` for an `int`), and a payload the model refuses fails the request with a 500. One
//! field table per model drives both that validation ([`validate`]) and the components of
//! the OpenAPI document ([`components`]), so the two cannot drift apart. Each area declares
//! its table next to its routes (`nikke::schema`, `bd2::models`, `azurlane::models`, the
//! Live2D override models in [`crate::live2d_overrides`]).

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
    Float,
    /// `float` with a validator that refuses `inf` and `nan` (`Live2DVector2`); its schema
    /// is a plain `number`.
    FiniteFloat(&'static str),
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

pub(crate) const fn field(name: &'static str, ty: Ty, default: Default) -> Field {
    Field { name, ty, default }
}

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
        Ty::Float => validate::float(value, &[])
            .map(Value::Float)
            .map_err(|errors| from_errors(loc, &errors)),
        Ty::FiniteFloat(message) => match validate::float(value, &[]) {
            Ok(number) if number.is_finite() => Ok(Value::Float(number)),
            Ok(_) => Err(failure(loc, &format!("Value error, {message}"))),
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
        Ty::Float | Ty::FiniteFloat(_) => bare(Type::Number).into(),
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
        Ty::Float | Ty::FiniteFloat(_) => titled(bare(Type::Number)),
        Ty::Bool => titled(bare(Type::Boolean)),
        Ty::NonNegInt => titled(bare(Type::Integer).extensions(Some(super_extensions(&[("minimum", json!(0.0))])))),
        Ty::Literal(values) => titled(bare(Type::String).enum_values(Some(values.iter().copied()))),
        Ty::FreeDict => titled(bare(Type::Object).additional_properties(Some(AdditionalProperties::FreeForm(true)))),
        Ty::DictOf(item) => titled(bare(Type::Object).additional_properties(Some(AdditionalProperties::RefOr(item_schema(item))))),
        Ty::ListOf(item) => ArrayBuilder::new().items(item_schema(item)).title(Some(title)).into(),
        // A model field is a bare reference; a model-or-null is a union without a title.
        Ty::Model(model) => reference(model),
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

/// The components `models` add to the OpenAPI document.
pub(crate) fn components(models: &[&Model]) -> Vec<(&'static str, RefOr<Schema>)> {
    models.iter().map(|model| (model.name, model_schema(model))).collect()
}

#[cfg(test)]
mod tests {
    use fav_core::pyjson::{self, DumpOptions, Value};

    use super::{Default, Model, Ty, field, validate};

    static INNER: Model = Model {
        name: "Inner",
        base: None,
        fields: &[field("x", Ty::FiniteFloat("x must be finite"), Default::Required)],
    };

    static OUTER: Model = Model {
        name: "Outer",
        base: None,
        fields: &[
            field("a", Ty::Str, Default::Required),
            field("b", Ty::Nullable(&Ty::IntOrStr), Default::Null),
            field("c", Ty::Int, Default::Int(0)),
            field("inner", Ty::Nullable(&Ty::Model(&INNER)), Default::Null),
        ],
    };

    fn check(text: &str) -> Result<String, String> {
        let value = pyjson::loads(text).expect("JSON");
        validate(&OUTER, &value).map(|value| pyjson::dumps(&value, &DumpOptions::compact()).expect("dumpable"))
    }

    #[test]
    fn models_fill_defaults_coerce_and_forbid_extra_keys() {
        assert_eq!(
            check(r#"{"c": " 7 ", "a": "x"}"#),
            Ok(r#"{"a":"x","b":null,"c":7,"inner":null}"#.to_owned())
        );
        assert!(check(r#"{"a": "x", "d": 1}"#).is_err());
        assert!(check(r#"{"c": 1}"#).is_err());
        assert_eq!(
            check(r#"{"a": "x", "inner": {"x": 1}}"#),
            Ok(r#"{"a":"x","b":null,"c":0,"inner":{"x":1.0}}"#.to_owned())
        );
        assert_eq!(
            check(r#"{"a": "x", "inner": {"x": "inf"}}"#),
            Err("inner.x: Value error, x must be finite".to_owned())
        );
    }

    #[test]
    fn smart_unions_prefer_the_exact_type() {
        let b = |text: &str| {
            check(&format!(r#"{{"a": "", "b": {text}}}"#)).map(|out| out[12..].split(',').next().unwrap_or_default().to_owned())
        };
        assert_eq!(b(r#""12""#), Ok(r#""12""#.to_owned()));
        assert_eq!(b("999.0"), Ok("999".to_owned()));
        assert_eq!(b("true"), Ok("1".to_owned()));
        assert!(b("1.5").is_err());
        assert!(b("[]").is_err());
        assert_eq!(
            validate(&OUTER, &Value::Null).map(|_| ()),
            Err(": Input should be a valid dictionary or instance of the model".to_owned())
        );
    }
}
