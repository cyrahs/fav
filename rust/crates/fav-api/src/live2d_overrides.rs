//! Live2D view overrides: a saved position and scale per model and display profile, laid
//! over the character a game library returns.
//!
//! Python origin: `src/api/live2d_overrides.py` (the store, the model ids, the overlay),
//! the `*_live2d_view_override` methods of `FavApiService` in `src/api/service.py`, the
//! `Live2D*` models of `src/api/schemas.py` and the view-override routes of
//! `src/api/routes.py`. The table is created by `fav-db`'s schema, not on first use (D96).
//! Both games' detail routes and view-override routes use it ([`Live2DSource`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use axum::body::Body as HttpBody;
use axum::http::StatusCode;
use axum::response::Response;
use chrono::{DateTime, Utc};
use fav_core::pyjson::{self, Dict, DumpOptions, Value};
use fav_core::pytext::py_strip;
use fav_core::validate::{self, LocItem};
use fav_db::Db;
use fav_runtime::BoxFuture;
use fav_runtime::pystr::str_value;
use regex::Regex;
use sqlx::Row as _;

use crate::error::ApiError;
use crate::handlers::{internal, unhandled};
use crate::params::{self, Body, Errors, Source, StrRules};
use crate::router::json_response;
use crate::schema::{self, Default, Model, Ty, field};
use crate::{ApiState, json};

/// `Live2DModelIdPath`'s pattern.
pub(crate) const MODEL_ID_PATTERN: &str = "^[A-Za-z0-9_.:-]+$";
/// `Live2DProfilePath`'s pattern.
pub(crate) const PROFILE_PATTERN: &str = "^[A-Za-z0-9_-]+$";
static MODEL_ID_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(MODEL_ID_PATTERN).expect("valid pattern"));
static PROFILE_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(PROFILE_PATTERN).expect("valid pattern"));
/// `_MODEL_ID_RE`.
static MODEL_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new("^[A-Za-z0-9_.:-]{1,160}$").expect("valid pattern"));
/// `_PROFILE_RE`.
static PROFILE: LazyLock<Regex> = LazyLock::new(|| Regex::new("^[A-Za-z0-9_-]{1,64}$").expect("valid pattern"));

const PROFILE_MESSAGE: &str = "Live2D override profile must contain only ASCII letters, digits, underscores, or hyphens";

// ---------- the store ----------

/// Which override a call addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideKey {
    pub source: String,
    pub content_id: i64,
    pub model_id: String,
    pub profile: String,
}

/// What a `PUT` saves: positions as `(x, y)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverrideView {
    pub position: (f64, f64),
    pub scale: f64,
    pub background_position: Option<(f64, f64)>,
    pub background_scale: Option<f64>,
}

/// One stored row. The positions are whatever JSON the column holds.
#[derive(Debug, Clone, PartialEq)]
pub struct OverrideRow {
    pub source: String,
    pub content_id: i64,
    pub model_id: String,
    pub profile: String,
    pub position: Value,
    pub scale: f64,
    pub background_position: Option<Value>,
    pub background_scale: Option<f64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `Live2DViewOverrideStore`; failures are the error's text.
pub trait Live2DViewOverrideStore: Send + Sync {
    /// A character's overrides, by model id and profile.
    fn list_for_character<'a>(&'a self, source: &'a str, content_id: i64) -> BoxFuture<'a, Result<Vec<OverrideRow>, String>>;
    fn get<'a>(&'a self, key: &'a OverrideKey) -> BoxFuture<'a, Result<Option<OverrideRow>, String>>;
    fn upsert<'a>(&'a self, key: &'a OverrideKey, view: &'a OverrideView) -> BoxFuture<'a, Result<OverrideRow, String>>;
    /// Whether a row was deleted.
    fn delete<'a>(&'a self, key: &'a OverrideKey) -> BoxFuture<'a, Result<bool, String>>;
}

/// `PostgresLive2DViewOverrideStore`.
#[derive(Debug, Clone)]
pub struct PostgresLive2DViewOverrideStore {
    db: Db,
}

impl PostgresLive2DViewOverrideStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

const COLUMNS: &str = "source, content_id::bigint AS content_id, model_id, profile, position::text AS position, scale, \
                       background_position::text AS background_position, background_scale, created_at, updated_at";

fn vector_json((x, y): (f64, f64)) -> String {
    let mut dict = Dict::new();
    dict.insert("x", Value::Float(x));
    dict.insert("y", Value::Float(y));
    pyjson::dumps(&Value::Dict(dict), &DumpOptions::python_default()).unwrap_or_default()
}

fn row_from(row: &sqlx::postgres::PgRow) -> Result<OverrideRow, String> {
    let text = |name: &str| -> Result<String, String> { row.try_get::<String, _>(name).map_err(|error| error.to_string()) };
    let json_column = |name: &str| -> Result<Option<Value>, String> {
        let value: Option<String> = row.try_get(name).map_err(|error| error.to_string())?;
        value
            .map(|text| pyjson::loads(&text).map_err(|error| error.python_message(&text)))
            .transpose()
    };
    Ok(OverrideRow {
        source: text("source")?,
        content_id: row.try_get("content_id").map_err(|error| error.to_string())?,
        model_id: text("model_id")?,
        profile: text("profile")?,
        position: json_column("position")?.unwrap_or(Value::Null),
        scale: row.try_get("scale").map_err(|error| error.to_string())?,
        background_position: json_column("background_position")?,
        background_scale: row.try_get("background_scale").map_err(|error| error.to_string())?,
        created_at: row.try_get("created_at").map_err(|error| error.to_string())?,
        updated_at: row.try_get("updated_at").map_err(|error| error.to_string())?,
    })
}

impl Live2DViewOverrideStore for PostgresLive2DViewOverrideStore {
    fn list_for_character<'a>(&'a self, source: &'a str, content_id: i64) -> BoxFuture<'a, Result<Vec<OverrideRow>, String>> {
        Box::pin(async move {
            let sql = format!(
                "SELECT {COLUMNS} FROM live2d_view_overrides WHERE source = $1 AND content_id = $2::bigint ORDER BY model_id, profile;"
            );
            let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(source)
                .bind(content_id)
                .fetch_all(self.db.pool())
                .await
                .map_err(|error| error.to_string())?;
            rows.iter().map(row_from).collect()
        })
    }

    fn get<'a>(&'a self, key: &'a OverrideKey) -> BoxFuture<'a, Result<Option<OverrideRow>, String>> {
        Box::pin(async move {
            let sql = format!(
                "SELECT {COLUMNS} FROM live2d_view_overrides \
                 WHERE source = $1 AND content_id = $2::bigint AND model_id = $3 AND profile = $4;"
            );
            let row = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&key.source)
                .bind(key.content_id)
                .bind(&key.model_id)
                .bind(&key.profile)
                .fetch_optional(self.db.pool())
                .await
                .map_err(|error| error.to_string())?;
            row.as_ref().map(row_from).transpose()
        })
    }

    fn upsert<'a>(&'a self, key: &'a OverrideKey, view: &'a OverrideView) -> BoxFuture<'a, Result<OverrideRow, String>> {
        Box::pin(async move {
            let sql = format!(
                "INSERT INTO live2d_view_overrides (
                     source, content_id, model_id, profile, position, scale, background_position, background_scale
                 )
                 VALUES ($1, $2::bigint, $3, $4, $5::jsonb, $6, $7::jsonb, $8)
                 ON CONFLICT (source, content_id, model_id, profile) DO UPDATE SET
                     position = EXCLUDED.position,
                     scale = EXCLUDED.scale,
                     background_position = EXCLUDED.background_position,
                     background_scale = EXCLUDED.background_scale,
                     updated_at = NOW()
                 RETURNING {COLUMNS};"
            );
            let row = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&key.source)
                .bind(key.content_id)
                .bind(&key.model_id)
                .bind(&key.profile)
                .bind(vector_json(view.position))
                .bind(view.scale)
                .bind(view.background_position.map(vector_json))
                .bind(view.background_scale)
                .fetch_one(self.db.pool())
                .await
                .map_err(|error| error.to_string())?;
            row_from(&row)
        })
    }

    fn delete<'a>(&'a self, key: &'a OverrideKey) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let result = sqlx::query(
                "DELETE FROM live2d_view_overrides WHERE source = $1 AND content_id = $2::bigint AND model_id = $3 AND profile = $4;",
            )
            .bind(&key.source)
            .bind(key.content_id)
            .bind(&key.model_id)
            .bind(&key.profile)
            .execute(self.db.pool())
            .await
            .map_err(|error| error.to_string())?;
            Ok(result.rows_affected() > 0)
        })
    }
}

// ---------- model ids and the overlay ----------

/// `validate_live2d_source(source)`.
pub(crate) fn validate_live2d_source(source: &str) -> Result<String, String> {
    let normalized = py_strip(source).to_lowercase();
    if matches!(normalized.as_str(), "bd2" | "nikke") {
        Ok(normalized)
    } else {
        Err(format!("Unsupported Live2D source: {source}"))
    }
}

/// `validate_live2d_profile(profile)`.
pub(crate) fn validate_live2d_profile(profile: &str) -> Result<String, String> {
    let normalized = py_strip(profile);
    if PROFILE.is_match(normalized) {
        Ok(normalized.to_owned())
    } else {
        Err(PROFILE_MESSAGE.to_owned())
    }
}

/// `_clean_text(value)`: `str(value).strip()`, `''` for `None`.
fn clean_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(value) => py_strip(&str_value(value)).to_owned(),
    }
}

/// `_short_hash(value)`: 16 hex digits of SHA-256.
fn short_hash(value: &str) -> String {
    let mut digest = crate::credentials::sha256_hex(value);
    digest.truncate(16);
    digest
}

/// `_live2d_model_fingerprint(model)`: what identifies a model when it has no id.
fn fingerprint(model: &Dict) -> String {
    let mut basis = Dict::new();
    for key in ["stable_id", "live2d_key", "key", "field", "label", "source", "variant"] {
        basis.insert(key, Value::Str(clean_text(model.get(key))));
    }
    for key in ["row_index", "column_index", "style_index", "skin_index"] {
        basis.insert(key, model.get(key).cloned().unwrap_or(Value::Null));
    }
    let urls = match model.get("source_urls") {
        Some(urls @ Value::Dict(_)) => urls.clone(),
        _ => Value::Dict(Dict::new()),
    };
    basis.insert("source_urls", urls);
    pyjson::dumps(&Value::Dict(basis), &DumpOptions::hash()).unwrap_or_default()
}

/// `live2d_model_id(model)`: the stable id or Live2D key when it is a valid id, a hash of
/// it when not, else a hash of the fingerprint.
pub(crate) fn live2d_model_id(model: &Dict) -> String {
    for key in ["stable_id", "live2d_key"] {
        let value = clean_text(model.get(key));
        if value.is_empty() {
            continue;
        }
        if MODEL_ID.is_match(&value) {
            return value;
        }
        return format!("{key}_{}", short_hash(&value));
    }
    format!("generated_{}", short_hash(&fingerprint(model)))
}

/// `_deduped_model_id(base_model_id, fingerprint)`.
fn deduped_model_id(base: &str, fingerprint: &str) -> String {
    let suffix: String = short_hash(fingerprint).chars().take(8).collect();
    let prefix: String = base.chars().take(151).collect();
    format!("{prefix}_{suffix}")
}

/// Visit every model `iter_live2d_models` yields, in its order: the character's own,
/// then each costume's, then each skin's.
fn for_each_model(character: &mut Dict, mut visit: impl FnMut(&mut Dict)) {
    let mut visit_list = |holder: &mut Dict| {
        if let Some(Value::List(mut models)) = holder.get("live2d_models").cloned() {
            for model in &mut models {
                if let Value::Dict(model) = model {
                    visit(model);
                }
            }
            holder.insert("live2d_models", Value::List(models));
        }
    };
    visit_list(character);
    for container in ["costumes", "skins"] {
        if let Some(Value::List(mut items)) = character.get(container).cloned() {
            for item in &mut items {
                if let Value::Dict(item) = item {
                    visit_list(item);
                }
            }
            character.insert(container, Value::List(items));
        }
    }
}

/// `assign_live2d_model_ids(character)`: one id per fingerprint, unique across the
/// character.
pub(crate) fn assign_live2d_model_ids(character: &mut Dict) {
    let mut by_fingerprint: HashMap<String, String> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();
    for_each_model(character, |model| {
        let print = fingerprint(model);
        let model_id = by_fingerprint.get(&print).cloned().unwrap_or_else(|| {
            let base = live2d_model_id(model);
            let model_id = if used.contains(&base) {
                deduped_model_id(&base, &print)
            } else {
                base
            };
            by_fingerprint.insert(print, model_id.clone());
            used.insert(model_id.clone());
            model_id
        });
        model.insert("model_id", Value::Str(model_id));
    });
}

/// The model ids `iter_live2d_models` would show after `assign_live2d_model_ids`.
fn model_ids(character: &mut Dict) -> Vec<Value> {
    let mut ids = Vec::new();
    for_each_model(character, |model| ids.push(model.get("model_id").cloned().unwrap_or(Value::Null)));
    ids
}

/// `live2d_view_override_value(override)`.
fn view_override_value(item: &Dict) -> Value {
    let get = |key: &str| item.get(key).cloned().unwrap_or(Value::Null);
    json::dict([
        (
            "position",
            match item.get("position") {
                Some(position @ Value::Dict(_)) => position.clone(),
                _ => Value::Dict(Dict::new()),
            },
        ),
        ("scale", get("scale")),
        (
            "background_position",
            match item.get("background_position") {
                Some(position @ Value::Dict(_)) => position.clone(),
                _ => Value::Null,
            },
        ),
        ("background_scale", get("background_scale")),
        ("created_at", get("created_at")),
        ("updated_at", get("updated_at")),
    ])
}

/// `apply_live2d_view_overrides(character, overrides)`: each model gets its profiles.
pub(crate) fn apply_live2d_view_overrides(character: &mut Dict, overrides: &[Value]) {
    assign_live2d_model_ids(character);
    let mut grouped: HashMap<String, Dict> = HashMap::new();
    for item in overrides.iter().filter_map(Value::as_dict) {
        let model_id = clean_text(item.get("model_id"));
        let profile = clean_text(item.get("profile"));
        if model_id.is_empty() || profile.is_empty() {
            continue;
        }
        grouped.entry(model_id).or_default().insert(profile, view_override_value(item));
    }
    for_each_model(character, |model| {
        let profiles = grouped.get(&clean_text(model.get("model_id"))).cloned().unwrap_or_default();
        model.insert("view_overrides", Value::Dict(profiles));
    });
}

/// `_serialize_live2d_view_override(override)`.
pub(crate) fn serialize_override(row: &OverrideRow) -> Value {
    json::dict([
        ("source", Value::Str(row.source.clone())),
        ("content_id", Value::Int(i128::from(row.content_id))),
        ("model_id", Value::Str(row.model_id.clone())),
        ("profile", Value::Str(row.profile.clone())),
        (
            "position",
            match &row.position {
                position @ Value::Dict(_) => position.clone(),
                _ => Value::Dict(Dict::new()),
            },
        ),
        ("scale", Value::Float(row.scale)),
        (
            "background_position",
            match &row.background_position {
                Some(position @ Value::Dict(_)) => position.clone(),
                _ => Value::Null,
            },
        ),
        ("background_scale", row.background_scale.map_or(Value::Null, Value::Float)),
        ("created_at", Value::Str(json::utc_z(&row.created_at))),
        ("updated_at", Value::Str(json::utc_z(&row.updated_at))),
    ])
}

/// The overrides of one character, serialised, for the detail overlay
/// (`_apply_live2d_view_overrides`).
pub(crate) async fn overrides_for_character(state: &ApiState, source: Live2DSource, content_id: i128) -> Result<Vec<Value>, ApiError> {
    // An id beyond 64 bits matches no row (PostgreSQL compares it as a numeric).
    let Ok(id) = i64::try_from(content_id) else {
        return Ok(Vec::new());
    };
    let rows = state
        .live2d_overrides
        .list_for_character(source.name(), id)
        .await
        .map_err(|error| {
            internal(
                &format!(
                    "Failed to list Live2D view overrides source={} content_id={content_id}",
                    source.name()
                ),
                &error,
            )
        })?;
    Ok(rows.iter().map(serialize_override).collect())
}

// ---------- response models ----------

pub(crate) static LIVE2D_VECTOR2: Model = Model {
    name: "Live2DVector2",
    base: None,
    fields: &[
        field("x", Ty::FiniteFloat("coordinate must be finite"), Default::Required),
        field("y", Ty::FiniteFloat("coordinate must be finite"), Default::Required),
    ],
};

pub(crate) static LIVE2D_VIEW_OVERRIDE_VALUE: Model = Model {
    name: "Live2DViewOverrideValue",
    base: None,
    fields: &[
        field("position", Ty::Model(&LIVE2D_VECTOR2), Default::Required),
        field("scale", Ty::Float, Default::Required),
        field("background_position", Ty::Nullable(&Ty::Model(&LIVE2D_VECTOR2)), Default::Null),
        field("background_scale", Ty::Nullable(&Ty::Float), Default::Null),
        field("created_at", Ty::Str, Default::Required),
        field("updated_at", Ty::Str, Default::Required),
    ],
};

/// `Live2DViewOverride`: the value's fields first, as the subclass lists them.
pub(crate) static LIVE2D_VIEW_OVERRIDE: Model = Model {
    name: "Live2DViewOverride",
    base: Some(&LIVE2D_VIEW_OVERRIDE_VALUE),
    fields: &[
        field("source", Ty::Literal(&["bd2", "nikke"]), Default::Required),
        field("content_id", Ty::Int, Default::Required),
        field("model_id", Ty::Str, Default::Required),
        field("profile", Ty::Str, Default::Required),
    ],
};

/// The Live2D response models of the document (the upsert body is a request model,
/// which `openapi` declares with the other request bodies).
pub(crate) const MODELS: [&Model; 3] = [&LIVE2D_VECTOR2, &LIVE2D_VIEW_OVERRIDE, &LIVE2D_VIEW_OVERRIDE_VALUE];

// ---------- the routes ----------

/// The sources with Live2D models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Live2DSource {
    Bd2,
    Nikke,
}

impl Live2DSource {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Bd2 => "bd2",
            Self::Nikke => "nikke",
        }
    }
}

/// The path of an override route, validated.
struct OverridePath {
    content_id: i128,
    model_id: String,
    profile: String,
}

fn override_path(errors: &mut Errors, path: &(dyn Fn(&str) -> Option<String> + Sync)) -> Option<OverridePath> {
    let content_id = crate::bd2::content_id_param(errors, path("content_id").as_deref());
    let model_id = params::str_param(
        errors,
        Source::Path,
        "model_id",
        path("model_id").as_deref(),
        true,
        StrRules {
            min_length: Some(1),
            max_length: Some(160),
            pattern: Some((MODEL_ID_PATTERN, &MODEL_ID_PATH)),
        },
    );
    let profile = params::str_param(
        errors,
        Source::Path,
        "profile",
        path("profile").as_deref(),
        true,
        StrRules {
            min_length: Some(1),
            max_length: Some(64),
            pattern: Some((PROFILE_PATTERN, &PROFILE_PATH)),
        },
    );
    Some(OverridePath {
        content_id: content_id?,
        model_id: model_id?,
        profile: profile?,
    })
}

/// One body field after validation.
enum Field<T> {
    Valid(T),
    /// Its errors were pushed.
    Invalid,
    /// A failed finite check: a `ValueError` FastAPI's error handler cannot render.
    Unrenderable,
}

impl<T> Field<T> {
    /// The value, noting an unrenderable failure in `unrenderable`.
    fn into_option(self, unrenderable: &mut bool) -> Option<T> {
        match self {
            Self::Valid(value) => Some(value),
            Self::Invalid => None,
            Self::Unrenderable => {
                *unrenderable = true;
                None
            }
        }
    }
}

/// pydantic's `float` with `gt=0` and the finite check.
fn positive_float(errors: &mut Vec<validate::ErrorItem>, value: &Value, loc: &[LocItem]) -> Field<f64> {
    match validate::float(value, loc) {
        Err(failures) => {
            errors.extend(failures);
            Field::Invalid
        }
        Ok(number) if number.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) => {
            let mut error = validate::ErrorItem::new("greater_than", "Input should be greater than 0", loc, value);
            let mut ctx = Dict::new();
            ctx.insert("gt", Value::Float(0.0));
            error.ctx = Some(ctx);
            errors.push(error);
            Field::Invalid
        }
        Ok(number) if !number.is_finite() => Field::Unrenderable,
        Ok(number) => Field::Valid(number),
    }
}

/// `Live2DVector2` in a request body.
fn vector_body(errors: &mut Vec<validate::ErrorItem>, value: &Value, loc: &[LocItem]) -> Field<(f64, f64)> {
    let Value::Dict(dict) = value else {
        errors.push(validate::ErrorItem::new(
            "model_attributes_type",
            "Input should be a valid dictionary or object to extract fields from",
            loc,
            value,
        ));
        return Field::Invalid;
    };
    let mut coordinates = [None, None];
    let mut unrenderable = false;
    for (slot, name) in coordinates.iter_mut().zip(["x", "y"]) {
        let field_loc = validate::key(loc, name);
        match dict.get(name) {
            None => errors.push(validate::ErrorItem::new("missing", "Field required", &field_loc, value)),
            Some(item) => match validate::float(item, &field_loc) {
                Err(failures) => errors.extend(failures),
                Ok(number) if !number.is_finite() => unrenderable = true,
                Ok(number) => *slot = Some(number),
            },
        }
    }
    for (key, item) in dict.iter() {
        let name = fav_runtime::pystr::str_key(key);
        if name != "x" && name != "y" {
            errors.push(validate::ErrorItem::new(
                "extra_forbidden",
                "Extra inputs are not permitted",
                &validate::key(loc, &name),
                item,
            ));
        }
    }
    if unrenderable {
        return Field::Unrenderable;
    }
    coordinates[0].zip(coordinates[1]).map_or(Field::Invalid, Field::Valid)
}

fn has_non_finite(value: &Value) -> bool {
    match value {
        Value::Float(number) => !number.is_finite(),
        Value::List(items) => items.iter().any(has_non_finite),
        Value::Dict(dict) => dict.iter().any(|(_, item)| has_non_finite(item)),
        _ => false,
    }
}

/// `Live2DViewOverrideUpsert` from the body, its errors joining the path's. A finite check
/// that failed, or an error whose input is not finite, cannot be rendered by FastAPI's
/// handler: Python answers 500 then, and so does this.
fn upsert_body(errors: &mut Errors, body: &Body) -> Result<Option<OverrideView>, ApiError> {
    let Some(dict) = params::model_body(errors, body) else {
        return Ok(None);
    };
    let mut failures: Vec<validate::ErrorItem> = Vec::new();
    let body_loc = [LocItem::Key("body".to_owned())];
    let mut unrenderable = false;
    let position = match dict.get("position") {
        None => {
            failures.push(validate::ErrorItem::new(
                "missing",
                "Field required",
                &validate::key(&body_loc, "position"),
                &Value::Dict(dict.clone()),
            ));
            None
        }
        Some(value) => vector_body(&mut failures, value, &validate::key(&body_loc, "position")).into_option(&mut unrenderable),
    };
    let scale = match dict.get("scale") {
        None => {
            failures.push(validate::ErrorItem::new(
                "missing",
                "Field required",
                &validate::key(&body_loc, "scale"),
                &Value::Dict(dict.clone()),
            ));
            None
        }
        Some(value) => positive_float(&mut failures, value, &validate::key(&body_loc, "scale")).into_option(&mut unrenderable),
    };
    let background_position = match dict.get("background_position") {
        None | Some(Value::Null) => Some(None),
        Some(value) => vector_body(&mut failures, value, &validate::key(&body_loc, "background_position"))
            .into_option(&mut unrenderable)
            .map(Some),
    };
    let background_scale = match dict.get("background_scale") {
        None | Some(Value::Null) => Some(None),
        Some(value) => positive_float(&mut failures, value, &validate::key(&body_loc, "background_scale"))
            .into_option(&mut unrenderable)
            .map(Some),
    };
    for (key, item) in dict.iter() {
        let name = fav_runtime::pystr::str_key(key);
        if !["position", "scale", "background_position", "background_scale"].contains(&name.as_str()) {
            failures.push(validate::ErrorItem::new(
                "extra_forbidden",
                "Extra inputs are not permitted",
                &validate::key(&body_loc, &name),
                item,
            ));
        }
    }
    if unrenderable || failures.iter().any(|failure| has_non_finite(&failure.input)) {
        return Err(unhandled(&"TypeError: Object of type ValueError is not JSON serializable"));
    }
    for failure in failures {
        let loc = failure
            .loc
            .iter()
            .map(|item| match item {
                LocItem::Key(name) => Value::Str(name.clone()),
                LocItem::Index(index) => Value::Int(i128::try_from(*index).unwrap_or(i128::MAX)),
            })
            .collect();
        errors.push(failure.kind, loc, &failure.msg, failure.input, failure.ctx);
    }
    Ok(match (position, scale, background_position, background_scale) {
        (Some(position), Some(scale), Some(background_position), Some(background_scale)) => Some(OverrideView {
            position,
            scale,
            background_position,
            background_scale,
        }),
        _ => None,
    })
}

/// `_normalize_live2d_profile` and `_require_live2d_model`: the key of an existing model.
async fn require_model(state: &ApiState, source: Live2DSource, path: &OverridePath) -> Result<OverrideKey, ApiError> {
    let source_name = validate_live2d_source(source.name()).map_err(|message| ApiError::new(400, "invalid_live2d_source", message))?;
    let profile = validate_live2d_profile(&path.profile).map_err(|message| ApiError::new(422, "invalid_live2d_profile", message))?;
    let mut character = match source {
        Live2DSource::Bd2 => crate::bd2::character_for_live2d(state, path.content_id).await?,
        Live2DSource::Nikke => crate::nikke::character_for_live2d(state, path.content_id).await?,
    };
    assign_live2d_model_ids(&mut character);
    if !model_ids(&mut character).contains(&Value::Str(path.model_id.clone())) {
        let mut details = Dict::new();
        details.insert("source", Value::Str(source_name.clone()));
        details.insert("content_id", Value::Int(path.content_id));
        details.insert("model_id", Value::Str(path.model_id.clone()));
        return Err(
            ApiError::new(404, format!("{source_name}_live2d_model_not_found"), "Live2D model not found.")
                .with_details(Value::Dict(details)),
        );
    }
    Ok(OverrideKey {
        source: source_name,
        // Checked by the caller's store calls: an id beyond 64 bits names no row.
        content_id: i64::try_from(path.content_id).unwrap_or(i64::MAX),
        model_id: path.model_id.clone(),
        profile,
    })
}

fn not_found() -> ApiError {
    ApiError::new(404, "live2d_view_override_not_found", "Live2D view override not found.")
}

fn store_failure(action: &str, key: &OverrideKey, content_id: i128, error: &dyn std::fmt::Display) -> ApiError {
    internal(
        &format!(
            "Failed to {action} Live2D view override source={} content_id={content_id} model_id={} profile={}",
            key.source, key.model_id, key.profile
        ),
        error,
    )
}

fn override_response(row: &OverrideRow) -> Result<Response, ApiError> {
    let dumped = schema::validate(&LIVE2D_VIEW_OVERRIDE, &serialize_override(row))
        .map_err(|error| unhandled(&format!("ValidationError: {error}")))?;
    Ok(json_response(200, &dumped))
}

/// `GET .../live2d-models/{model_id}/view-overrides/{profile}`.
pub(crate) async fn get_override(
    state: &ApiState,
    source: Live2DSource,
    path: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let parsed = override_path(&mut errors, path);
    errors.finish()?;
    let path = parsed.ok_or_else(ApiError::internal)?;
    let key = require_model(state, source, &path).await?;
    if i64::try_from(path.content_id).is_err() {
        return Err(not_found());
    }
    let row = state
        .live2d_overrides
        .get(&key)
        .await
        .map_err(|error| store_failure("get", &key, path.content_id, &error))?;
    override_response(&row.ok_or_else(not_found)?)
}

/// `PUT .../live2d-models/{model_id}/view-overrides/{profile}`.
pub(crate) async fn put_override(
    state: &ApiState,
    source: Live2DSource,
    path: &(dyn Fn(&str) -> Option<String> + Sync),
    body: &Body,
) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let parsed = override_path(&mut errors, path);
    let view = upsert_body(&mut errors, body)?;
    errors.finish()?;
    let (Some(path), Some(view)) = (parsed, view) else {
        return Err(ApiError::internal());
    };
    let key = require_model(state, source, &path).await?;
    if i64::try_from(path.content_id).is_err() {
        return Err(store_failure("save", &key, path.content_id, &"integer out of range"));
    }
    let row = state
        .live2d_overrides
        .upsert(&key, &view)
        .await
        .map_err(|error| store_failure("save", &key, path.content_id, &error))?;
    override_response(&row)
}

/// `DELETE .../live2d-models/{model_id}/view-overrides/{profile}`: 204, or 404.
pub(crate) async fn delete_override(
    state: &ApiState,
    source: Live2DSource,
    path: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let parsed = override_path(&mut errors, path);
    errors.finish()?;
    let path = parsed.ok_or_else(ApiError::internal)?;
    let key = require_model(state, source, &path).await?;
    if i64::try_from(path.content_id).is_err() {
        return Err(not_found());
    }
    let deleted = state
        .live2d_overrides
        .delete(&key)
        .await
        .map_err(|error| store_failure("delete", &key, path.content_id, &error))?;
    if !deleted {
        return Err(not_found());
    }
    let mut response = Response::new(HttpBody::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    Ok(response)
}

/// The store `ApiState::new` builds.
pub fn postgres_store(db: Db) -> Arc<dyn Live2DViewOverrideStore> {
    Arc::new(PostgresLive2DViewOverrideStore::new(db))
}
