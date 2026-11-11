//! The Hanime1 routes and the readiness check.
//!
//! Python origin: the `/hanime1/*` routes of `src/api/routes.py`, the matching
//! `FavApiService` methods and its readiness check (`get_readiness`,
//! `_build_readiness`, `_probe_readiness_seeds`) in `src/api/service.py`,
//! `fetch_hanime1_videos_from_db` in `src/api/config.py` and `GET /readyz` in
//! `src/api/app.py`. The seed and author services are `fav_sources::hanime1_series` and
//! `fav_sources::hanime1_author`, behind traits so the tests can put fakes there.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::body::Body as HttpBody;
use axum::http::StatusCode;
use axum::response::Response;
use fav_core::pycasefold::casefold;
use fav_core::pyjson::Value;
use fav_core::pytext::py_strip;
use fav_sources::hanime1_series::{ProbeError, ServiceError};
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use regex::Regex;
use sqlx::Row as _;

use crate::error::ApiError;
use crate::handlers::{internal, list_response, log, unhandled};
use crate::params::{self, Body, Errors, Source, StrRules};
use crate::router::json_response;
use crate::{ApiState, json};

/// The path pattern of a seed or author id (`Path(pattern=r'^[0-9]+$')`).
pub(crate) const NUMERIC_ID_PATTERN: &str = "^[0-9]+$";
static NUMERIC_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(NUMERIC_ID_PATTERN).expect("valid pattern"));

/// `_READINESS_SAMPLE_SIZE`.
const READINESS_SAMPLE_SIZE: i64 = 3;

/// `_READINESS_MESSAGES`.
fn readiness_message(code: &str) -> &'static str {
    match code {
        "database_unavailable" => "Unable to load Hanime1 readiness targets.",
        "disabled" => "Hanime1 is disabled.",
        "no_targets" => "No Hanime1 scan targets are configured.",
        "ok" => "Hanime1 playlist parsing is healthy.",
        "parser_incompatible" => "Hanime1 playlist structure is incompatible with the parser.",
        "probe_failed" => "Hanime1 readiness probes failed.",
        "upstream_timeout" => "Hanime1 readiness check timed out.",
        _ => "Hanime1 watch pages are unavailable.",
    }
}

/// A monotonic clock, as time since some fixed start (`time.monotonic`).
pub type Monotonic = Arc<dyn Fn() -> Duration + Send + Sync>;

/// The readiness check's timings and its cached answer.
pub struct Readiness {
    /// `_READINESS_CACHE_TTL_SECONDS`: how long an answer is reused.
    pub cache_ttl: Duration,
    /// `_READINESS_PROBE_TIMEOUT_SECONDS`: each probe's own limit.
    pub probe_timeout: Duration,
    /// `_READINESS_TOTAL_TIMEOUT_SECONDS`: loading the targets and probing them.
    pub total_timeout: Duration,
    pub monotonic: Monotonic,
    /// When the last answer was built, and the answer. The lock also makes concurrent
    /// requests wait for one build (`_readiness_lock`).
    cache: tokio::sync::Mutex<Option<(Duration, Value)>>,
}

impl std::fmt::Debug for Readiness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Readiness")
            .field("cache_ttl", &self.cache_ttl)
            .field("probe_timeout", &self.probe_timeout)
            .field("total_timeout", &self.total_timeout)
            .finish_non_exhaustive()
    }
}

impl Default for Readiness {
    /// Python's timings on the process's monotonic clock.
    fn default() -> Self {
        let start = std::time::Instant::now();
        Self::new(Arc::new(move || start.elapsed()))
    }
}

impl Readiness {
    pub fn new(monotonic: Monotonic) -> Self {
        Self {
            cache_ttl: Duration::from_mins(5),
            probe_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(12),
            monotonic,
            cache: tokio::sync::Mutex::new(None),
        }
    }
}

fn service_error_text(error: &ServiceError) -> String {
    match error {
        ServiceError::Other(message) => message.clone(),
        other => format!("{other:?}"),
    }
}

fn no_content() -> Response {
    let mut response = Response::new(HttpBody::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    response
}

/// A path id: one to 32 ASCII digits.
fn numeric_path_id(name: &str, raw: Option<&str>) -> Result<String, ApiError> {
    let mut errors = Errors::new();
    let value = params::str_param(
        &mut errors,
        Source::Path,
        name,
        raw,
        true,
        StrRules {
            min_length: Some(1),
            max_length: Some(32),
            pattern: Some((NUMERIC_ID_PATTERN, &NUMERIC_ID_REGEX)),
        },
    );
    errors.finish()?;
    Ok(value.unwrap_or_default())
}

/// The one string field of a create body (`Hanime1SeedCreate`, `Hanime1AuthorCreate`).
fn create_field(body: &Body, name: &str) -> Result<String, ApiError> {
    let mut errors = Errors::new();
    let value = params::model_body(&mut errors, body).and_then(|dict| {
        let value = params::body_field(&mut errors, dict, name).and_then(|value| params::body_str(&mut errors, name, value, 1));
        params::forbid_extra(&mut errors, dict, &[name]);
        value
    });
    errors.finish()?;
    Ok(value.unwrap_or_default())
}

// ---------- videos ----------

/// `list_hanime1_videos` over `fetch_hanime1_videos_from_db`: the downloaded videos, the
/// first spelling of each id (case-folded) kept.
pub(crate) async fn list_videos(state: &ApiState) -> Result<Response, ApiError> {
    let items = fetch_videos(state)
        .await
        .map_err(|error| internal("Failed to list Hanime1 videos", &error))?;
    Ok(json_response(200, &list_response(items)))
}

async fn fetch_videos(state: &ApiState) -> Result<Vec<Value>, String> {
    let settings = state.settings.load().await.map_err(|error| error.to_string())?;
    let host = settings.web.hanime1.host.trim_end_matches('/').to_owned();
    let rows = sqlx::query("SELECT id, title, uploader, release_date, plot FROM hanime1 ORDER BY id;")
        .fetch_all(state.db.pool())
        .await
        .map_err(|error| error.to_string())?;
    let mut seen = HashSet::new();
    let mut videos = Vec::new();
    for row in rows {
        let text = |name: &str| -> Result<String, String> {
            let value: Option<String> = row.try_get(name).map_err(|error| error.to_string())?;
            Ok(value.unwrap_or_default())
        };
        let optional = |name: &str| -> Result<Value, String> {
            let value = text(name)?;
            let stripped = py_strip(&value);
            Ok(if stripped.is_empty() {
                Value::Null
            } else {
                Value::Str(stripped.to_owned())
            })
        };
        let id = text("id")?;
        let video_id = py_strip(&id);
        if video_id.is_empty() || !seen.insert(casefold(video_id)) {
            continue;
        }
        videos.push(json::dict([
            ("video_id", Value::Str(video_id.to_owned())),
            ("title", Value::Str(text("title")?)),
            ("downloaded", Value::Bool(true)),
            ("uploader", optional("uploader")?),
            ("release_date", optional("release_date")?),
            ("plot", optional("plot")?),
            ("watch_url", Value::Str(format!("{host}/watch?v={video_id}"))),
        ]));
    }
    Ok(videos)
}

// ---------- seeds ----------

/// `add_hanime1_seed`: 201 with the resolved series.
pub(crate) async fn create_seed(state: &ApiState, body: &Body) -> Result<Response, ApiError> {
    let raw = create_field(body, "seed")?;
    match state.hanime1_series.add(&raw).await {
        Ok(seed) => {
            let label = seed.label().map_err(|error| unhandled(&error))?;
            Ok(json_response(
                201,
                &json::dict([
                    ("video_id", Value::Str(seed.video_id)),
                    ("title", Value::Str(seed.title)),
                    ("label", Value::Str(label)),
                ]),
            ))
        }
        Err(ServiceError::Invalid) => Err(ApiError::new(422, "invalid_seed", "Seed format is invalid.")),
        Err(ServiceError::Duplicate) => Err(ApiError::new(409, "duplicate_seed", "Hanime1 seed already exists.")),
        Err(ServiceError::ResolveFailed) => Err(ApiError::new(422, "seed_resolve_failed", "Unable to resolve Hanime1 seed.")),
        Err(ServiceError::Other(error)) => Err(internal("Failed to add Hanime1 series seed", &error)),
    }
}

/// `list_hanime1_seeds` through `Hanime1SeedListResponse`.
pub(crate) async fn list_seeds(state: &ApiState) -> Result<Response, ApiError> {
    let seeds = state
        .hanime1_series
        .list()
        .await
        .map_err(|error| internal("Failed to list Hanime1 series seeds", &service_error_text(&error)))?;
    let items = seeds
        .into_iter()
        .map(|seed| {
            json::dict([
                ("video_id", Value::Str(seed.video_id)),
                ("title", Value::Str(seed.title)),
                ("label", Value::Str(seed.label)),
                ("added_from_video_id", Value::Str(seed.added_from_video_id)),
                ("video_count", Value::from(seed.video_count)),
                ("created_at", Value::Str(json::utc_z(&seed.created_at))),
                ("updated_at", Value::Str(json::utc_z(&seed.updated_at))),
                ("last_scanned_at", json::utc_z_opt(seed.last_scanned_at.as_ref())),
                ("last_scan_error", Value::Str(seed.last_scan_error)),
                ("watch_url", Value::Str(seed.watch_url)),
            ])
        })
        .collect();
    Ok(json_response(200, &list_response(items)))
}

/// `delete_hanime1_seed`: 204, or 404 when no such series is subscribed.
pub(crate) async fn delete_seed(state: &ApiState, raw: Option<&str>) -> Result<Response, ApiError> {
    let canonical_video_id = numeric_path_id("canonical_video_id", raw)?;
    match state.hanime1_series.delete(&canonical_video_id).await {
        Ok(true) => Ok(no_content()),
        Ok(false) => Err(ApiError::new(404, "not_found", "Hanime1 seed not found.")),
        Err(ServiceError::Invalid) => Err(ApiError::new(422, "invalid_seed", "Seed id is invalid.")),
        Err(error) => Err(internal(
            &format!("Failed to delete Hanime1 series seed id={canonical_video_id}"),
            &service_error_text(&error),
        )),
    }
}

// ---------- authors ----------

/// `add_hanime1_author`: 201 with the resolved author.
pub(crate) async fn create_author(state: &ApiState, body: &Body) -> Result<Response, ApiError> {
    let raw = create_field(body, "author")?;
    match state.hanime1_authors.add(&raw).await {
        Ok(author) => Ok(json_response(
            201,
            &json::dict([
                ("author_id", Value::Str(author.author_id)),
                ("name", Value::Str(author.name)),
                ("author_url", Value::Str(author.author_url)),
            ]),
        )),
        Err(ServiceError::Invalid) => Err(ApiError::new(422, "invalid_author", "Author id or URL is invalid.")),
        Err(ServiceError::Duplicate) => Err(ApiError::new(409, "duplicate_author", "Hanime1 author already exists.")),
        Err(ServiceError::ResolveFailed) => Err(ApiError::new(422, "author_resolve_failed", "Unable to resolve Hanime1 author.")),
        Err(ServiceError::Other(error)) => Err(internal("Failed to add Hanime1 author subscription", &error)),
    }
}

/// `list_hanime1_authors` through `Hanime1AuthorListResponse`.
pub(crate) async fn list_authors(state: &ApiState) -> Result<Response, ApiError> {
    let authors = state
        .hanime1_authors
        .list()
        .await
        .map_err(|error| internal("Failed to list Hanime1 author subscriptions", &service_error_text(&error)))?;
    let items = authors
        .into_iter()
        .map(|author| {
            json::dict([
                ("author_id", Value::Str(author.author_id)),
                ("name", Value::Str(author.name)),
                ("author_url", Value::Str(author.author_url)),
                ("video_count", Value::from(author.video_count)),
                ("created_at", Value::Str(json::utc_z(&author.created_at))),
                ("updated_at", Value::Str(json::utc_z(&author.updated_at))),
                ("last_scanned_at", json::utc_z_opt(author.last_scanned_at.as_ref())),
                ("last_scan_error", Value::Str(author.last_scan_error)),
            ])
        })
        .collect();
    Ok(json_response(200, &list_response(items)))
}

/// `delete_hanime1_author`: 204, or 404 when the author is not subscribed.
pub(crate) async fn delete_author(state: &ApiState, raw: Option<&str>) -> Result<Response, ApiError> {
    let author_id = numeric_path_id("author_id", raw)?;
    match state.hanime1_authors.delete(&author_id).await {
        Ok(true) => Ok(no_content()),
        Ok(false) => Err(ApiError::new(404, "not_found", "Hanime1 author not found.")),
        Err(ServiceError::Invalid) => Err(ApiError::new(422, "invalid_author", "Author id is invalid.")),
        Err(error) => Err(internal(
            &format!("Failed to delete Hanime1 author subscription id={author_id}"),
            &service_error_text(&error),
        )),
    }
}

// ---------- readiness ----------

/// `GET /readyz`: 503 when degraded, the answer cached for five minutes.
pub(crate) async fn readiness(state: &ApiState) -> Result<Response, ApiError> {
    let payload = cached_readiness(state).await?;
    let degraded = payload.get("status").and_then(Value::as_str) == Some("degraded");
    Ok(json_response(if degraded { 503 } else { 200 }, &payload))
}

/// `get_readiness`.
async fn cached_readiness(state: &ApiState) -> Result<Value, ApiError> {
    let readiness = &state.readiness;
    let mut cache = readiness.cache.lock().await;
    if let Some((checked_at, payload)) = cache.as_ref()
        && (readiness.monotonic)().saturating_sub(*checked_at) < readiness.cache_ttl
    {
        return Ok(payload.clone());
    }
    let payload = build_readiness(state).await?;
    *cache = Some(((readiness.monotonic)(), payload.clone()));
    Ok(payload)
}

/// `_build_readiness`: the targets and the probes share one deadline.
async fn build_readiness(state: &ApiState) -> Result<Value, ApiError> {
    let settings = state.settings.load().await.map_err(|error| unhandled(&error))?;
    if !settings.web.hanime1.schedule.enabled {
        return Ok(readiness_payload(state, "skipped", "disabled", 0));
    }
    let readiness = &state.readiness;
    let deadline = tokio::time::Instant::now() + readiness.total_timeout;
    let seed_ids = match tokio::time::timeout_at(deadline, state.hanime1_series.readiness_seed_ids(READINESS_SAMPLE_SIZE)).await {
        Err(_) => return Ok(readiness_payload(state, "degraded", "upstream_timeout", 0)),
        Ok(Err(error)) => {
            log().error(format!("Failed to load Hanime1 readiness targets: {error}"));
            return Ok(readiness_payload(state, "degraded", "database_unavailable", 0));
        }
        Ok(Ok(seed_ids)) => seed_ids,
    };
    if seed_ids.is_empty() {
        return Ok(readiness_payload(state, "skipped", "no_targets", 0));
    }
    let sampled = seed_ids.len();
    let Ok(failures) = tokio::time::timeout_at(deadline, probe_seeds(state, &seed_ids)).await else {
        return Ok(readiness_payload(state, "degraded", "upstream_timeout", sampled));
    };
    let Some(failures) = failures else {
        return Ok(readiness_payload(state, "ok", "ok", sampled));
    };
    let code = if failures.iter().all(|failure| *failure == ProbeError::Timeout) {
        "upstream_timeout"
    } else if failures.iter().all(|failure| *failure == ProbeError::ParserIncompatible) {
        "parser_incompatible"
    } else if failures.contains(&ProbeError::ParserIncompatible) {
        "probe_failed"
    } else {
        "upstream_unavailable"
    };
    Ok(readiness_payload(state, "degraded", code, sampled))
}

/// `_probe_readiness_seeds`: all probes at once; the first success ends the check
/// (`None`, the rest dropped), otherwise every failure.
async fn probe_seeds(state: &ApiState, seed_ids: &[String]) -> Option<Vec<ProbeError>> {
    let limit = state.readiness.probe_timeout;
    let mut probes: FuturesUnordered<_> = seed_ids
        .iter()
        .map(|seed_id| async move {
            tokio::time::timeout(limit, state.hanime1_series.probe_seed(seed_id, limit))
                .await
                .unwrap_or(Err(ProbeError::Timeout))
        })
        .collect();
    let mut failures = Vec::new();
    while let Some(result) = probes.next().await {
        match result {
            Ok(()) => return None,
            Err(error) => failures.push(error),
        }
    }
    Some(failures)
}

/// `_readiness_payload` through `ReadinessResponse`.
fn readiness_payload(state: &ApiState, component_status: &str, code: &str, sampled_targets: usize) -> Value {
    let status = if component_status == "degraded" { "degraded" } else { "ok" };
    json::dict([
        ("status", Value::from(status)),
        ("generated_at", Value::Str(json::utc_z(&(state.clock)()))),
        (
            "checks",
            json::dict([(
                "hanime1",
                json::dict([
                    ("status", Value::from(component_status)),
                    ("code", Value::from(code)),
                    ("message", Value::from(readiness_message(code))),
                    ("sampled_targets", Value::from(i64::try_from(sampled_targets).unwrap_or(i64::MAX))),
                ]),
            )]),
        ),
    ])
}
