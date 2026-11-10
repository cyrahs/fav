//! The Azur Lane collection routes: the character list, the sidebar, the skin-update
//! feed, a character's detail and its ship detail.
//!
//! Python origin: the `/azurlane/*` reading routes of `src/api/routes.py`
//! (`_matches_azurlane_query`, the sidebar payload, sort key and `ETag`), the
//! `*_azurlane_*` methods of `FavApiService` in `src/api/service.py` and the `AzurLane*`
//! models of `src/api/schemas.py` ([`models`]). The library is [`library`]. Python makes it
//! once, with the `web.azurlane.path` the settings held when the API started; here the path
//! is read at each request and the library (with its cache) is rebuilt when it changes, as
//! for NIKKE and BD2 (DIVERGENCES.md D95).

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

pub use library::AzurLaneLibrary;
use library::LibraryError;

use crate::error::ApiError;
use crate::handlers::{internal, unhandled};
use crate::params::{self, BigInt, Errors, IntBounds, Source, StrRules};
use crate::router::{Request, json_response};
use crate::schema;
use crate::{ApiState, json};

/// `_AZURLANE_SIDEBAR_CACHE_CONTROL`, also the skin-update feed's.
const SIDEBAR_CACHE_CONTROL: &str = "public, max-age=300";

/// The library of the configured root, rebuilt (with its cache) when the setting changes.
#[derive(Debug, Default)]
pub struct AzurLaneLibraries {
    current: std::sync::Mutex<Option<Arc<AzurLaneLibrary>>>,
}

impl AzurLaneLibraries {
    /// The library for `root`: the current one, or a new one when the root changed.
    pub fn for_root(&self, root: &std::path::Path) -> Arc<AzurLaneLibrary> {
        let mut current = self.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match current.as_ref() {
            Some(library) if library.root() == root => Arc::clone(library),
            _ => {
                let library = Arc::new(AzurLaneLibrary::new(root));
                *current = Some(Arc::clone(&library));
                library
            }
        }
    }
}

/// The library, made from the settings on first use.
async fn library(state: &ApiState) -> Result<Arc<AzurLaneLibrary>, ApiError> {
    let settings = state.settings.load().await.map_err(|error| unhandled(&error))?;
    Ok(state.azurlane.for_root(settings.web.azurlane.path.as_path()))
}

/// Run a library call off the async workers: it reads files.
async fn blocking<T: Send + 'static>(
    state: &ApiState,
    call: impl FnOnce(&Arc<AzurLaneLibrary>) -> Result<T, LibraryError> + Send + 'static,
) -> Result<Result<T, LibraryError>, ApiError> {
    let library = library(state).await?;
    tokio::task::spawn_blocking(move || call(&library))
        .await
        .map_err(|error| unhandled(&error))
}

fn failure_text(error: &LibraryError) -> String {
    match error {
        LibraryError::Failed(message) => message.clone(),
        LibraryError::CharacterNotFound => "AzurLaneCharacterNotFoundError".to_owned(),
        LibraryError::AssetNotFound => "AzurLaneAssetNotFoundError".to_owned(),
    }
}

/// `list_azurlane_characters`.
async fn list_characters(state: &ApiState) -> Result<Vec<Dict>, ApiError> {
    blocking(state, AzurLaneLibrary::list_characters)
        .await?
        .map_err(|error| internal("Failed to list Azur Lane characters", &failure_text(&error)))
}

/// The `q` query parameter (`AzurLaneSearchQuery`).
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

/// The `character_key` path parameter (`AzurLaneCharacterKeyPath`).
fn character_key_param(raw: Option<&str>) -> Result<String, ApiError> {
    let mut errors = Errors::new();
    let key = params::str_param(
        &mut errors,
        Source::Path,
        "character_key",
        raw,
        true,
        StrRules {
            min_length: Some(1),
            max_length: Some(200),
            ..StrRules::default()
        },
    );
    errors.finish()?;
    key.ok_or_else(ApiError::internal)
}

/// `str(value or '')`.
fn or_empty(value: Option<&Value>) -> String {
    value.filter(|value| value.is_truthy()).map(str_value).unwrap_or_default()
}

/// `_matches_azurlane_query(character, query)`: the key, id, names, tags and source
/// metadata (each value as sorted JSON), case-folded.
fn matches_query(character: &Dict, query: &str) -> bool {
    let mut haystack: Vec<String> = [
        "character_key",
        "source_id",
        "title",
        "display_name",
        "name_zh",
        "name_en",
        "directory_name",
    ]
    .iter()
    .map(|key| or_empty(character.get(key)))
    .collect();
    if let Some(Value::Dict(tags)) = character.get("tags") {
        haystack.extend(tags.iter().map(|(_, value)| str_value(value)));
    }
    if let Some(Value::Dict(metadata)) = character.get("source_metadata") {
        let options = DumpOptions {
            sort_keys: true,
            ..DumpOptions::python_default()
        };
        let options = DumpOptions {
            ensure_ascii: false,
            ..options
        };
        haystack.extend(metadata.iter().map(|(_, value)| pyjson::dumps(value, &options).unwrap_or_default()));
    }
    casefold(&haystack.join(" ")).contains(&casefold(query))
}

/// A model that refused what the library built: pydantic's `ValidationError`, which the
/// generic handler answers with 500.
fn invalid(error: &str) -> ApiError {
    unhandled(&format!("ValidationError: {error}"))
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

/// The payload with its `ETag` (`_json_etag`) and `Cache-Control`, or a bare 304 when the
/// client holds it already.
fn etagged(request: &Request, payload: &Value) -> Result<Response, ApiError> {
    let text = pyjson::dumps(payload, &DumpOptions::hash()).map_err(|error| unhandled(&error))?;
    let etag = format!("\"{}\"", crate::credentials::sha256_hex(&text));
    let mut response = if etag_matches(request.header("if-none-match").as_deref(), &etag) {
        let mut response = Response::new(HttpBody::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        response
    } else {
        json_response(200, payload)
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(SIDEBAR_CACHE_CONTROL));
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    Ok(response)
}

// ---------- the routes ----------

/// `GET /api/v2/azurlane/characters`: a page of summaries, optionally searched.
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
        .map(|character| schema::validate(&models::AZURLANE_CHARACTER_SUMMARY, &Value::Dict(character.clone())))
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

/// `_azurlane_sidebar_character_payload(character)`.
fn sidebar_payload(character: &Dict) -> Value {
    let truthy = |key: &str| character.get(key).filter(|value| value.is_truthy()).cloned();
    let dict_or = |key: &str, default: Value| match character.get(key) {
        Some(value @ Value::Dict(_)) => value.clone(),
        _ => default,
    };
    json::dict([
        ("character_key", truthy("character_key").unwrap_or(Value::from(""))),
        ("title", truthy("title").unwrap_or(Value::from(""))),
        (
            "display_name",
            truthy("display_name").or_else(|| truthy("title")).unwrap_or(Value::from("")),
        ),
        ("name_zh", truthy("name_zh").unwrap_or(Value::from(""))),
        ("name_en", truthy("name_en").unwrap_or(Value::from(""))),
        ("representative_asset", dict_or("representative_asset", Value::Null)),
        ("icon", dict_or("icon", Value::Null)),
        ("source_metadata", dict_or("source_metadata", Value::Dict(Dict::new()))),
        ("model_count", truthy("model_count").unwrap_or(Value::Int(0))),
        ("model_counts", dict_or("model_counts", Value::Dict(Dict::new()))),
        ("source_counts", dict_or("source_counts", Value::Dict(Dict::new()))),
        ("fetched_at", character.get("fetched_at").cloned().unwrap_or(Value::Null)),
        ("completed_at", character.get("completed_at").cloned().unwrap_or(Value::Null)),
    ])
}

/// `_azurlane_sidebar_sort_key(item)`.
fn sidebar_sort_key(item: &Value) -> Result<(f64, f64, String), ApiError> {
    let timestamp = |name: &str| {
        crate::bd2::library::route_sort_timestamp(&item.get(name).cloned().unwrap_or(Value::Null)).map_err(|error| match error {
            crate::bd2::library::LibraryError::Failed(message) => unhandled(&message),
            crate::bd2::library::LibraryError::NotFound => ApiError::internal(),
        })
    };
    let key = match item.get("character_key") {
        Some(Value::Str(key)) => key.clone(),
        _ => String::new(),
    };
    Ok((timestamp("completed_at")?, timestamp("fetched_at")?, key))
}

/// `GET /api/v2/azurlane/sidebar/characters`: every character, newest first, with an
/// `ETag`.
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
        let item = schema::validate(&models::AZURLANE_SIDEBAR_CHARACTER, &sidebar_payload(character)).map_err(|error| invalid(&error))?;
        keyed.push((sidebar_sort_key(&item)?, item));
    }
    // `sorted(reverse=True)` keeps equal keys in their order.
    keyed.sort_by(|left, right| right.0.partial_cmp(&left.0).unwrap_or(std::cmp::Ordering::Equal));
    let items: Vec<Value> = keyed.into_iter().map(|(_, item)| item).collect();
    let total = Value::Int(i128::try_from(items.len()).unwrap_or(i128::MAX));
    etagged(request, &json::dict([("items", Value::List(items)), ("total", total)]))
}

/// `GET /api/v2/azurlane/skin-updates`: the source's update feed, with an `ETag`.
pub(crate) async fn skin_updates(state: &ApiState, request: &Request) -> Result<Response, ApiError> {
    let payload = match blocking(state, |library| library.get_skin_updates()).await? {
        Ok(payload) => payload,
        Err(LibraryError::AssetNotFound) => {
            return Err(ApiError::new(
                404,
                "azurlane_skin_updates_not_found",
                "Azur Lane skin updates not found.",
            ));
        }
        Err(error) => return Err(internal("Failed to get Azur Lane skin updates", &failure_text(&error))),
    };
    let payload = schema::validate(&models::AZURLANE_SKIN_UPDATES_RESPONSE, &Value::Dict(payload)).map_err(|error| invalid(&error))?;
    etagged(request, &payload)
}

/// `GET /api/v2/azurlane/characters/{character_key}`.
pub(crate) async fn detail(state: &ApiState, raw: Option<&str>) -> Result<Response, ApiError> {
    let character_key = character_key_param(raw)?;
    let key = character_key.clone();
    let character = match blocking(state, move |library| library.get_character(&key)).await? {
        Ok(character) => character,
        Err(LibraryError::CharacterNotFound) => {
            return Err(ApiError::new(404, "azurlane_character_not_found", "Azur Lane character not found."));
        }
        Err(error) => {
            return Err(internal(
                &format!("Failed to get Azur Lane character character_key={character_key}"),
                &failure_text(&error),
            ));
        }
    };
    let dumped = schema::validate(&models::AZURLANE_CHARACTER_DETAIL, &Value::Dict(character)).map_err(|error| invalid(&error))?;
    Ok(json_response(200, &dumped))
}

/// `GET /api/v2/azurlane/characters/{character_key}/ship-detail`: the crawler's
/// `detail.json`, as it is.
pub(crate) async fn ship_detail(state: &ApiState, raw: Option<&str>) -> Result<Response, ApiError> {
    let character_key = character_key_param(raw)?;
    let key = character_key.clone();
    match blocking(state, move |library| library.get_ship_detail(&key)).await? {
        Ok(detail) => Ok(json_response(200, &Value::Dict(detail))),
        Err(LibraryError::CharacterNotFound | LibraryError::AssetNotFound) => Err(ApiError::new(
            404,
            "azurlane_ship_detail_not_found",
            "Azur Lane ship detail not found.",
        )),
        Err(error) => Err(internal(
            &format!("Failed to get Azur Lane ship detail character_key={character_key}"),
            &failure_text(&error),
        )),
    }
}
