//! The BD2 routes: the character list, the sidebar and a character's detail.
//!
//! Python origin: the `/bd2/*` routes of `src/api/routes.py` (`_matches_bd2_query`, the
//! sidebar payload, sort key and `ETag`), the `*_bd2_*` methods of `FavApiService` in
//! `src/api/service.py` and the `BD2*` models of `src/api/schemas.py`. The library itself is
//! [`library`]; the view-override routes under a character are
//! [`crate::live2d_overrides`]. Python makes the library once, with the `web.bd2.path` the
//! settings held when the API started; here the path is read at each request and the
//! library (with its cache) is rebuilt when it changes, as for NIKKE (DIVERGENCES.md D95).

pub(crate) mod library;
pub(crate) mod models;
#[cfg(test)]
pub(crate) mod tests;

use std::sync::Arc;

use axum::body::Body as HttpBody;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use fav_core::pycasefold::casefold;
use fav_core::pyjson::{self, Dict, DumpOptions, Value};
use fav_runtime::pystr::str_value;

pub use library::Bd2Library;
use library::LibraryError;

use crate::error::ApiError;
use crate::handlers::{internal, unhandled};
use crate::live2d_overrides::{self, Live2DSource};
use crate::params::{self, BigInt, Errors, IntBounds, Source, StrRules};
use crate::router::{Request, json_response};
use crate::schema;
use crate::{ApiState, json};

/// `_BD2_SIDEBAR_CACHE_CONTROL`.
const SIDEBAR_CACHE_CONTROL: &str = "public, max-age=300";

/// The library, made from the settings on first use.
async fn library(state: &ApiState) -> Result<Arc<Bd2Library>, ApiError> {
    let settings = state.settings.load().await.map_err(|error| unhandled(&error))?;
    Ok(state.bd2.for_root(settings.web.bd2.path.as_path()))
}

/// The library of the configured root, rebuilt (with its cache) when the setting changes.
#[derive(Debug, Default)]
pub struct Bd2Libraries {
    current: std::sync::Mutex<Option<Arc<Bd2Library>>>,
}

impl Bd2Libraries {
    /// The library for `root`: the current one, or a new one when the root changed.
    pub fn for_root(&self, root: &std::path::Path) -> Arc<Bd2Library> {
        let mut current = self.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match current.as_ref() {
            Some(library) if library.root() == root => Arc::clone(library),
            _ => {
                let library = Arc::new(Bd2Library::new(root));
                *current = Some(Arc::clone(&library));
                library
            }
        }
    }
}

/// Run a library call off the async workers: it reads files.
async fn blocking<T: Send + 'static>(
    library: Arc<Bd2Library>,
    call: impl FnOnce(&Bd2Library) -> Result<T, LibraryError> + Send + 'static,
) -> Result<Result<T, LibraryError>, ApiError> {
    tokio::task::spawn_blocking(move || call(&library))
        .await
        .map_err(|error| unhandled(&error))
}

/// `list_bd2_characters`.
async fn list_characters(state: &ApiState) -> Result<Vec<Dict>, ApiError> {
    match blocking(library(state).await?, Bd2Library::list_characters).await? {
        Ok(characters) => Ok(characters),
        Err(LibraryError::NotFound) => Err(internal("Failed to list BD2 characters", &"BD2CharacterNotFoundError")),
        Err(LibraryError::Failed(error)) => Err(internal("Failed to list BD2 characters", &error)),
    }
}

/// A path's `content_id` (`Path(gt=0)`). Python's ints are unbounded, so a long one is
/// still looked up; nothing beyond 128 bits can name a character.
pub(crate) fn content_id_param(errors: &mut Errors, raw: Option<&str>) -> Option<i128> {
    let parsed = params::int_param(
        errors,
        Source::Path,
        "content_id",
        raw,
        None,
        IntBounds {
            gt: Some(0),
            ..IntBounds::default()
        },
    )?;
    Some(match parsed {
        BigInt::Fits(content_id) => i128::from(content_id),
        _ => raw
            .and_then(|raw| fav_core::validate::int_unbounded(&Value::Str(raw.to_owned()), &[]).ok())
            .unwrap_or(i128::MAX),
    })
}

/// The `q` query parameter (`Query(alias='q', min_length=1)`).
fn search_param(errors: &mut Errors, request: &Request) -> Option<String> {
    params::str_param(
        errors,
        Source::Query,
        "q",
        request.query_last("q"),
        false,
        StrRules {
            min_length: Some(1),
            ..StrRules::default()
        },
    )
}

/// `str(value or '')`.
fn or_empty(value: Option<&Value>) -> String {
    value.filter(|value| value.is_truthy()).map(str_value).unwrap_or_default()
}

/// `_matches_bd2_query(character, query)`: the id, title, tags, profile values, search
/// terms and costume names, case-folded.
fn matches_query(character: &Dict, query: &str) -> bool {
    let mut haystack = vec![or_empty(character.get("content_id")), or_empty(character.get("title"))];
    if let Some(Value::Dict(tags)) = character.get("tags") {
        haystack.extend(tags.iter().map(|(_, value)| str_value(value)));
    }
    if let Some(Value::List(profile)) = character.get("profile") {
        haystack.extend(profile.iter().filter_map(Value::as_dict).map(|item| or_empty(item.get("value"))));
    }
    if let Some(Value::List(terms)) = character.get("search_terms") {
        haystack.extend(terms.iter().map(str_value));
    }
    if let Some(Value::List(costumes)) = character.get("costumes") {
        for costume in costumes.iter().filter_map(Value::as_dict) {
            haystack.push(or_empty(costume.get("title")));
            haystack.push(or_empty(costume.get("category")));
        }
    }
    casefold(&haystack.join(" ")).contains(&casefold(query))
}

// ---------- response models ----------

/// A model that refused what the library built: pydantic's `ValidationError`, which the
/// generic handler answers with 500.
fn invalid(error: &str) -> ApiError {
    unhandled(&format!("ValidationError: {error}"))
}

// ---------- the routes ----------

/// `GET /api/v2/bd2/characters`: a page of summaries, optionally searched.
pub(crate) async fn list(state: &ApiState, request: &Request) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let query = search_param(&mut errors, request);
    let limit = params::int_param(
        &mut errors,
        Source::Query,
        "limit",
        request.query_last("limit"),
        Some(200),
        IntBounds {
            ge: Some(1),
            le: Some(500),
            ..IntBounds::default()
        },
    );
    let offset = params::int_param(
        &mut errors,
        Source::Query,
        "offset",
        request.query_last("offset"),
        Some(0),
        IntBounds {
            ge: Some(0),
            ..IntBounds::default()
        },
    );
    errors.finish()?;
    let limit = match limit {
        Some(BigInt::Fits(limit)) => limit,
        _ => 200,
    };
    // The response echoes the offset as given, however large (past 128 bits it is
    // `i128::MAX`, D99); a page that far out is empty.
    let offset = match offset {
        Some(BigInt::Fits(offset)) => i128::from(offset),
        Some(BigInt::AbovePositive) => request
            .query_last("offset")
            .and_then(|raw| fav_core::validate::int_unbounded(&Value::Str(raw.to_owned()), &[]).ok())
            .unwrap_or(i128::MAX),
        _ => 0,
    };
    let mut characters = list_characters(state).await?;
    if let Some(query) = &query {
        characters.retain(|character| matches_query(character, query));
    }
    let total = characters.len();
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
    let end = start.saturating_add(usize::try_from(limit).unwrap_or(0)).min(total);
    let items = characters[start..end]
        .iter()
        .map(|character| schema::validate(&models::BD2_CHARACTER_SUMMARY, &Value::Dict(character.clone())))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| invalid(&error))?;
    Ok(json_response(
        200,
        &json::dict([
            ("items", Value::List(items)),
            ("total", Value::Int(i128::try_from(total).unwrap_or(i128::MAX))),
            ("limit", Value::from(limit)),
            ("offset", Value::Int(offset)),
        ]),
    ))
}

/// `_bd2_sidebar_character_payload(character)`.
fn sidebar_payload(character: &Dict) -> Value {
    let icon = match character.get("icon") {
        Some(Value::Dict(icon)) => icon.clone(),
        _ => Dict::new(),
    };
    json::dict([
        ("content_id", character.get("content_id").cloned().unwrap_or(Value::Null)),
        (
            "title",
            character
                .get("title")
                .filter(|value| value.is_truthy())
                .cloned()
                .unwrap_or(Value::from("")),
        ),
        (
            "icon",
            json::dict([
                ("url", Value::Str(or_empty(icon.get("url")))),
                ("available", Value::Bool(icon.get("available").is_some_and(Value::is_truthy))),
                ("sha256", Value::Str(or_empty(icon.get("sha256")))),
                ("content_type", Value::Str(or_empty(icon.get("content_type")))),
            ]),
        ),
        ("updated_at", character.get("updated_at").cloned().unwrap_or(Value::Null)),
        ("fetched_at", character.get("fetched_at").cloned().unwrap_or(Value::Null)),
    ])
}

/// `_bd2_sidebar_sort_key(item)`.
fn sidebar_sort_key(item: &Value) -> (f64, f64, i128) {
    let field = |name: &str| item.get(name).cloned().unwrap_or(Value::Null);
    let content_id = match item.get("content_id") {
        Some(Value::Int(content_id)) => *content_id,
        _ => 0,
    };
    (
        library::route_sort_timestamp(&field("updated_at")),
        library::route_sort_timestamp(&field("fetched_at")),
        content_id,
    )
}

/// `_etag_matches(header_value, etag)`.
fn etag_matches(header_value: Option<&str>, etag: &str) -> bool {
    let Some(header_value) = header_value.filter(|value| !value.is_empty()) else {
        return false;
    };
    header_value
        .split(',')
        .map(fav_core::pytext::py_strip)
        .any(|candidate| candidate == "*" || candidate == etag)
}

/// `GET /api/v2/bd2/sidebar/characters`: every character, newest first, with an `ETag`.
pub(crate) async fn sidebar(state: &ApiState, request: &Request) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let query = search_param(&mut errors, request);
    errors.finish()?;
    let mut characters = list_characters(state).await?;
    if let Some(query) = &query {
        characters.retain(|character| matches_query(character, query));
    }
    let mut keyed = Vec::with_capacity(characters.len());
    for character in &characters {
        let item = schema::validate(&models::BD2_SIDEBAR_CHARACTER, &sidebar_payload(character)).map_err(|error| invalid(&error))?;
        keyed.push((sidebar_sort_key(&item), item));
    }
    // `sorted(reverse=True)` keeps equal keys in their order.
    keyed.sort_by(|left, right| right.0.partial_cmp(&left.0).unwrap_or(std::cmp::Ordering::Equal));
    let items: Vec<Value> = keyed.into_iter().map(|(_, item)| item).collect();
    let total = Value::Int(i128::try_from(items.len()).unwrap_or(i128::MAX));
    let payload = json::dict([("items", Value::List(items)), ("total", total)]);
    let text = pyjson::dumps(&payload, &DumpOptions::hash()).map_err(|error| unhandled(&error))?;
    let etag = format!("\"{}\"", crate::credentials::sha256_hex(&text));
    let mut response = if etag_matches(request.header("if-none-match").as_deref(), &etag) {
        let mut response = Response::new(HttpBody::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        response
    } else {
        json_response(200, &payload)
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(SIDEBAR_CACHE_CONTROL));
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    Ok(response)
}

/// `get_bd2_character(content_id)` without the overlay.
async fn get_character(state: &ApiState, content_id: i128, failure: &str) -> Result<Dict, ApiError> {
    match blocking(library(state).await?, move |library| library.get_character(content_id)).await? {
        Ok(character) => Ok(character),
        Err(LibraryError::NotFound) => Err(ApiError::new(404, "bd2_character_not_found", "BD2 character not found.")),
        Err(LibraryError::Failed(error)) => Err(internal(&format!("{failure} content_id={content_id}"), &error)),
    }
}

/// `_load_character_for_live2d_source('bd2', content_id)`.
pub(crate) async fn character_for_live2d(state: &ApiState, content_id: i128) -> Result<Dict, ApiError> {
    get_character(state, content_id, "Failed to load Live2D source character source=bd2").await
}

/// `GET /api/v2/bd2/characters/{content_id}`: the detail, with the view overrides laid
/// over its models.
pub(crate) async fn detail(state: &ApiState, raw: Option<&str>) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let content_id = content_id_param(&mut errors, raw);
    errors.finish()?;
    let content_id = content_id.ok_or_else(ApiError::internal)?;
    let mut character = get_character(state, content_id, "Failed to get BD2 character").await?;
    let overrides = live2d_overrides::overrides_for_character(state, Live2DSource::Bd2, content_id).await?;
    live2d_overrides::apply_live2d_view_overrides(&mut character, &overrides);
    let dumped = schema::validate(&models::BD2_CHARACTER_DETAIL, &Value::Dict(character)).map_err(|error| invalid(&error))?;
    Ok(json_response(200, &dumped))
}
