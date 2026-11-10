//! The endpoints.
//!
//! Python origin: `src/api/routes.py`, `src/api/service.py` (`FavApiService`),
//! `src/api/credentials.py` and the system routes of `src/api/app.py`. Each handler
//! validates its parameters the way FastAPI does ([`crate::params`]), after the API
//! router's bearer-token dependency, and answers what the service and its
//! `response_model` would.

use std::sync::LazyLock;

use axum::http::{HeaderValue, header};
use axum::response::Response;
use fav_core::pyjson::{Dict, Value};
use fav_core::settings::{SECTIONS, SectionError, SectionValue};
use fav_db::settings_store::SettingsStoreError;
use fav_runtime::control::{self, ControlRequest};
use fav_runtime::jobs::{self, ScheduledJob};
use fav_runtime::site_sessions::{self, VALUE_IN_PROFILE};
use fav_runtime::telegram_bot::TelegramDeliveryError;
use fav_sources::kemono_creator::ResolveError;
use regex::Regex;

use crate::error::ApiError;
use crate::live2d_overrides::{self, Live2DSource};
use crate::params::{self, BigInt, Body, Errors, IntBounds, Source, StrRules};
use crate::router::{Endpoint, PathParams, Request, json_response};
use crate::{ApiState, archive, bd2, credentials, hanime1, json, masking, nikke, openapi, site_logins, web};

const AUTH_PREFIX: &str = "Bearer ";

/// `JobRequestTarget`, in its declared order.
pub(crate) const JOB_REQUEST_TARGETS: [&str; 14] = [
    "all",
    "azurlane",
    "bd2",
    "bilibili",
    "hanime1",
    "jandan",
    "kemono",
    "nikke",
    "pixiv",
    "rednote",
    "stellasora",
    "telegram",
    "twitter",
    "wechat",
];

/// `JobRequestStatus`, in its declared order.
pub(crate) const JOB_REQUEST_STATUSES: [&str; 6] = ["failed", "interrupted", "pending", "rejected", "running", "succeeded"];

pub(crate) const SETTINGS_SECTION_PATTERN: &str = r"^[a-z0-9_]+(\.[a-z0-9_]+)*$";
pub(crate) const SITE_PATTERN: &str = "^[A-Za-z0-9_-]+$";
static SETTINGS_SECTION_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(SETTINGS_SECTION_PATTERN).expect("valid pattern"));
static SITE_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(SITE_PATTERN).expect("valid pattern"));

pub(crate) fn log() -> fav_core::logging::Logger {
    fav_core::logging::get("fav-api")
}

/// `log.exception(message)` and the 500 the service raises after it.
pub(crate) fn internal(message: &str, error: &dyn std::fmt::Display) -> ApiError {
    log().error(format!("{message}: {error}"));
    ApiError::internal()
}

/// An exception no handler expected: the generic handler logs it and answers 500.
pub(crate) fn unhandled(error: &dyn std::fmt::Display) -> ApiError {
    log().error(format!("Unhandled API error: {error}"));
    ApiError::internal()
}

/// `FavApiService.authenticate`: the API router's dependency.
fn authenticate(state: &ApiState, request: &Request) -> Result<(), ApiError> {
    let authorization = request.header("authorization").unwrap_or_default();
    if authorization.is_empty() {
        return Err(ApiError::unauthorized("missing_authorization", "Authorization header is required."));
    }
    let Some(rest) = authorization.strip_prefix(AUTH_PREFIX) else {
        return Err(ApiError::unauthorized(
            "invalid_authorization_scheme",
            "Authorization scheme must be Bearer.",
        ));
    };
    let provided = fav_core::pytext::py_strip(rest);
    if provided.is_empty() {
        return Err(ApiError::unauthorized("missing_bearer_token", "Bearer token is required."));
    }
    if !constant_time_eq(provided.as_bytes(), state.token.as_bytes()) {
        return Err(ApiError::new(403, "invalid_token", "Bearer token is invalid."));
    }
    Ok(())
}

/// `secrets.compare_digest` on bytes.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = u8::from(left.len() != right.len());
    for (index, byte) in left.iter().enumerate() {
        diff |= byte ^ right.get(index).copied().unwrap_or(!byte);
    }
    diff == 0
}

/// Run an endpoint: body first (as FastAPI reads it), then the token, then the handler.
pub(crate) async fn call(state: &ApiState, endpoint: Endpoint, params: &PathParams, request: &Request) -> Result<Response, ApiError> {
    let body = match endpoint {
        Endpoint::CreateJobRequest
        | Endpoint::UpdateSettingsSection
        | Endpoint::CreateHanime1Seed
        | Endpoint::CreateHanime1Author
        | Endpoint::ResolveKemonoCreator
        | Endpoint::PutBd2Live2DViewOverride
        | Endpoint::PutNikkeLive2DViewOverride => params::read_body(&request.body, request.header("content-type").as_deref())?,
        _ => Body::Absent,
    };
    if endpoint.needs_api_token() {
        authenticate(state, request)?;
    }
    let path = |name: &str| params.iter().find(|(key, _)| *key == name).map(|(_, value)| value.as_str());
    let owned_path = |name: &str| path(name).map(str::to_owned);
    match endpoint {
        Endpoint::OpenApi => Ok(json_response(200, &openapi::document_value())),
        Endpoint::Docs => Ok(web::html(web::SWAGGER_UI_HTML)),
        Endpoint::OAuth2Redirect => Ok(web::html(web::OAUTH2_REDIRECT_HTML)),
        Endpoint::Redoc => Ok(web::html(web::REDOC_HTML)),
        Endpoint::Health => Ok(json_response(200, &health(state))),
        Endpoint::Readiness => hanime1::readiness(state).await,
        Endpoint::ListJobs => list_jobs(state).await,
        Endpoint::CreateJobRequest => create_job_request(state, &body).await,
        Endpoint::GetJobRequest => get_job_request(state, path("request_id")).await,
        Endpoint::ListJobRequests => list_job_requests(state, request).await,
        Endpoint::ListSettings => list_settings(state).await,
        Endpoint::GetSettingsSection => {
            let section = settings_section_param(path("section"))?;
            Ok(json_response(200, &get_settings_section(state, &section).await?))
        }
        Endpoint::UpdateSettingsSection => update_settings_section(state, path("section"), &body).await,
        Endpoint::TestTelegramNotification => test_telegram_notification(state).await,
        Endpoint::TestTikHub => test_tikhub(state).await,
        Endpoint::ListArchiveSources => list_archive_sources(state).await,
        Endpoint::ListArchiveItems => list_archive_items(state, request).await,
        Endpoint::ListHanime1Videos => hanime1::list_videos(state).await,
        Endpoint::CreateHanime1Seed => hanime1::create_seed(state, &body).await,
        Endpoint::ListHanime1Seeds => hanime1::list_seeds(state).await,
        Endpoint::DeleteHanime1Seed => hanime1::delete_seed(state, path("canonical_video_id")).await,
        Endpoint::CreateHanime1Author => hanime1::create_author(state, &body).await,
        Endpoint::ListHanime1Authors => hanime1::list_authors(state).await,
        Endpoint::DeleteHanime1Author => hanime1::delete_author(state, path("author_id")).await,
        Endpoint::ResolveKemonoCreator => resolve_kemono_creator(state, &body).await,
        Endpoint::ListNikkeCharacters => nikke::list_characters(state, request).await,
        Endpoint::ListNikkeSidebarCharacters => nikke::list_sidebar_characters(state, request).await,
        Endpoint::GetNikkeCharacter => nikke::get_character(state, path("content_id")).await,
        Endpoint::GetNikkeLive2DViewOverride => live2d_overrides::get_override(state, Live2DSource::Nikke, &owned_path).await,
        Endpoint::PutNikkeLive2DViewOverride => live2d_overrides::put_override(state, Live2DSource::Nikke, &owned_path, &body).await,
        Endpoint::DeleteNikkeLive2DViewOverride => live2d_overrides::delete_override(state, Live2DSource::Nikke, &owned_path).await,
        Endpoint::ListBd2Characters => bd2::list(state, request).await,
        Endpoint::ListBd2SidebarCharacters => bd2::sidebar(state, request).await,
        Endpoint::GetBd2Character => bd2::detail(state, path("content_id")).await,
        Endpoint::GetBd2Live2DViewOverride => live2d_overrides::get_override(state, Live2DSource::Bd2, &owned_path).await,
        Endpoint::PutBd2Live2DViewOverride => live2d_overrides::put_override(state, Live2DSource::Bd2, &owned_path, &body).await,
        Endpoint::DeleteBd2Live2DViewOverride => live2d_overrides::delete_override(state, Live2DSource::Bd2, &owned_path).await,
        Endpoint::ListSiteSessions => list_site_sessions(state).await,
        Endpoint::GetSiteCredential => get_site_credential(state, path("site"), request).await,
        Endpoint::Mcp => Ok(crate::mcp::handle(state, request).await),
        Endpoint::Icon(name) => match &state.web_dist {
            Some(dist) => web::file_response(&dist.join(name)).await,
            None => Err(ApiError::http(404)),
        },
    }
}

// ---------- kemono ----------

/// `resolve_kemono_creator`: a pasted creator URL or fanbox id as `{service, id, name}`.
async fn resolve_kemono_creator(state: &ApiState, body: &Body) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let creator = params::model_body(&mut errors, body).and_then(|dict| {
        let creator = params::body_field(&mut errors, dict, "creator").and_then(|value| params::body_str(&mut errors, "creator", value, 1));
        params::forbid_extra(&mut errors, dict, &["creator"]);
        creator
    });
    errors.finish()?;
    match state.kemono_creators.resolve(&creator.unwrap_or_default()).await {
        Ok(resolved) => Ok(json_response(
            200,
            &json::dict([
                ("service", Value::Str(resolved.service)),
                ("id", Value::Str(resolved.id)),
                ("name", Value::Str(resolved.name)),
            ]),
        )),
        Err(ResolveError::Invalid) => Err(ApiError::new(422, "invalid_creator", "Creator id or URL is invalid.")),
        Err(ResolveError::NotFound) => Err(ApiError::new(404, "creator_not_found", "Creator not found upstream.")),
        Err(ResolveError::Unreachable) => Err(ApiError::new(502, "creator_resolve_failed", "Unable to reach the Kemono site.")),
        Err(ResolveError::Other(error)) => Err(internal("Failed to resolve Kemono creator", &error)),
    }
}

// ---------- system ----------

/// `get_health`: `utc_now_iso_z`.
fn health(state: &ApiState) -> Value {
    json::dict([
        ("status", Value::from("ok")),
        ("generated_at", Value::Str(json::utc_z(&(state.clock)()))),
    ])
}

// ---------- jobs ----------

/// `serialize_job` through `JobSummary`.
fn job_summary(job: &ScheduledJob) -> Value {
    json::dict([
        ("key", Value::Str(job.key.clone())),
        ("name", Value::Str(job.name.clone())),
        ("enabled", Value::Bool(job.enabled)),
        ("notify", Value::Bool(job.notify)),
        ("cron", Value::Str(job.cron.clone())),
        ("section", Value::Str(job.section.clone())),
        ("missing_fields", json::str_list(&job.missing_fields)),
    ])
}

/// `FavApiService.scheduled_jobs`.
pub(crate) async fn scheduled_jobs(state: &ApiState) -> Result<Vec<ScheduledJob>, ApiError> {
    let settings = state.settings.load().await.map_err(|error| unhandled(&error))?;
    Ok(jobs::build_jobs(&settings))
}

async fn list_jobs(state: &ApiState) -> Result<Response, ApiError> {
    let items: Vec<Value> = scheduled_jobs(state).await?.iter().map(job_summary).collect();
    Ok(json_response(200, &list_response(items)))
}

pub(crate) fn list_response(items: Vec<Value>) -> Value {
    let total = Value::from(i64::try_from(items.len()).unwrap_or(i64::MAX));
    json::dict([("items", Value::List(items)), ("total", total)])
}

/// `serialize_control_request` through `JobRequest`, whose enums refuse a target or
/// status outside them (`ResponseValidationError`, a 500).
fn job_request(request: &ControlRequest) -> Result<Value, ApiError> {
    if !JOB_REQUEST_TARGETS.contains(&request.target.as_str()) || !JOB_REQUEST_STATUSES.contains(&request.status.as_str()) {
        return Err(unhandled(&format!(
            "ResponseValidationError: control request {} has target {:?} and status {:?}",
            request.request_id, request.target, request.status
        )));
    }
    Ok(serialize_control_request(request))
}

/// `serialize_control_request`.
fn serialize_control_request(request: &ControlRequest) -> Value {
    json::dict([
        ("id", Value::from(request.request_id)),
        ("target", Value::Str(request.target.clone())),
        ("kind", Value::Str(request.kind.clone())),
        ("status", Value::Str(request.status.clone())),
        ("requested_at", Value::Str(json::utc_z(&request.requested_at))),
        ("started_at", json::utc_z_opt(request.started_at.as_ref())),
        ("finished_at", json::utc_z_opt(request.finished_at.as_ref())),
        ("result", Value::Str(request.result.clone())),
        ("error", Value::Str(request.error.clone())),
    ])
}

async fn create_job_request(state: &ApiState, body: &Body) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let target = params::model_body(&mut errors, body).and_then(|dict| {
        let target = params::body_field(&mut errors, dict, "target")
            .and_then(|value| params::enum_value(&mut errors, params::body_field_loc("target"), value, &JOB_REQUEST_TARGETS));
        params::forbid_extra(&mut errors, dict, &["target"]);
        target
    });
    errors.finish()?;
    let request = queue_job_request(state, &target.unwrap_or_default()).await?;
    Ok(json_response(202, &job_request(&request)?))
}

/// `FavApiService.create_job_request`: a manual run of a job or of every job (`all`).
async fn queue_job_request(state: &ApiState, target: &str) -> Result<ControlRequest, ApiError> {
    let normalized = fav_core::pytext::py_strip(target).to_lowercase();
    let mut valid: Vec<String> = scheduled_jobs(state).await?.into_iter().map(|job| job.key).collect();
    valid.push("all".to_owned());
    if !valid.contains(&normalized) {
        valid.sort_unstable();
        valid.dedup();
        return Err(ApiError::new(400, "invalid_target", "Unsupported job target.")
            .with_details(json::dict([("allowed_values", json::str_list(&valid))])));
    }
    control::create(&state.db, control::KIND_TRIGGER_JOB, &normalized, None)
        .await
        .map_err(|error| internal(&format!("Failed to create control request target={normalized}"), &error))
}

/// `FavApiService.create_job_request` as the MCP `run_job` tool calls it: the queued
/// request, serialized without the route's response model.
pub(crate) async fn create_job_request_value(state: &ApiState, target: &str) -> Result<Value, ApiError> {
    Ok(serialize_control_request(&queue_job_request(state, target).await?))
}

async fn get_job_request(state: &ApiState, raw: Option<&str>) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let request_id = params::int_param(
        &mut errors,
        Source::Path,
        "request_id",
        raw,
        None,
        IntBounds {
            gt: Some(0),
            ..IntBounds::default()
        },
    );
    errors.finish()?;
    let not_found = || ApiError::new(404, "not_found", "Job request not found.");
    // An id beyond 64 bits names no row (DIVERGENCES.md).
    let Some(BigInt::Fits(request_id)) = request_id else {
        return Err(not_found());
    };
    let request = control::get(&state.db, request_id).await.map_err(|error| unhandled(&error))?;
    Ok(json_response(200, &job_request(&request.ok_or_else(not_found)?)?))
}

async fn list_job_requests(state: &ApiState, request: &Request) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let statuses = params::enum_list_param(&mut errors, "status", &request.query_all("status"), &JOB_REQUEST_STATUSES);
    let limit = params::int_param(
        &mut errors,
        Source::Query,
        "limit",
        request.query_last("limit"),
        Some(50),
        IntBounds {
            ge: Some(1),
            le: Some(200),
            ..IntBounds::default()
        },
    );
    errors.finish()?;
    let limit = match limit {
        Some(BigInt::Fits(limit)) => limit,
        _ => 50,
    };
    let statuses: Option<Vec<&str>> = statuses
        .as_ref()
        .filter(|items| !items.is_empty())
        .map(|items| items.iter().map(String::as_str).collect());
    let requests = control::list(&state.db, statuses.as_deref(), limit)
        .await
        .map_err(|error| match error {
            fav_runtime::Error::Invalid(message) => ApiError::new(422, "invalid_status", message),
            other => internal("Failed to list job requests", &other),
        })?;
    let items = requests.iter().map(job_request).collect::<Result<Vec<_>, _>>()?;
    Ok(json_response(200, &list_response(items)))
}

// ---------- settings ----------

fn settings_section_param(raw: Option<&str>) -> Result<String, ApiError> {
    let mut errors = Errors::new();
    let section = params::str_param(
        &mut errors,
        Source::Path,
        "section",
        raw,
        true,
        StrRules {
            min_length: Some(1),
            max_length: Some(64),
            pattern: Some((SETTINGS_SECTION_PATTERN, &SETTINGS_SECTION_REGEX)),
        },
    );
    errors.finish()?;
    Ok(section.unwrap_or_default())
}

fn unknown_section(section: &str) -> ApiError {
    ApiError::new(404, "unknown_section", format!("Unknown settings section: {section}"))
}

fn as_dict(value: Value) -> Dict {
    match value {
        Value::Dict(dict) => dict,
        _ => Dict::new(),
    }
}

/// `{section, value (masked), missing_fields}`.
fn settings_section_value(section: &str, model: &SectionValue) -> Value {
    json::dict([
        ("section", Value::from(section)),
        ("value", Value::Dict(masking::mask_section(section, &as_dict(model.dump())))),
        ("missing_fields", json::str_list(&model.validate_runnable())),
    ])
}

/// `FavApiService.get_settings_section`.
pub(crate) async fn get_settings_section(state: &ApiState, section: &str) -> Result<Value, ApiError> {
    match state.settings.load_section(section).await {
        Ok(model) => Ok(settings_section_value(section, &model)),
        Err(SettingsStoreError::Section(SectionError::Unknown(_))) => Err(unknown_section(section)),
        Err(error) => Err(internal(&format!("Failed to load settings section={section}"), &error)),
    }
}

async fn list_settings(state: &ApiState) -> Result<Response, ApiError> {
    Ok(json_response(200, &list_response(list_settings_sections(state).await?)))
}

/// `FavApiService.list_settings`: every section.
pub(crate) async fn list_settings_sections(state: &ApiState) -> Result<Vec<Value>, ApiError> {
    let mut items = Vec::new();
    for section in SECTIONS {
        items.push(get_settings_section(state, section).await?);
    }
    Ok(items)
}

/// `FavApiService.update_settings_section`: secrets sent back masked keep their stored
/// value; the result is validated before it is saved, and a source cannot be switched on
/// while its configuration is incomplete.
async fn update_settings_section(state: &ApiState, raw: Option<&str>, body: &Body) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let section = params::str_param(
        &mut errors,
        Source::Path,
        "section",
        raw,
        true,
        StrRules {
            min_length: Some(1),
            max_length: Some(64),
            pattern: Some((SETTINGS_SECTION_PATTERN, &SETTINGS_SECTION_REGEX)),
        },
    );
    let payload = params::dict_body(&mut errors, body);
    errors.finish()?;
    let (section, payload) = (section.unwrap_or_default(), payload.unwrap_or_default());
    Ok(json_response(200, &save_settings_section(state, &section, &payload).await?))
}

/// `FavApiService.update_settings_section`.
pub(crate) async fn save_settings_section(state: &ApiState, section: &str, payload: &Dict) -> Result<Value, ApiError> {
    let section = section.to_owned();
    if !SECTIONS.contains(&section.as_str()) {
        return Err(unknown_section(&section));
    }
    let stored = state
        .settings
        .load_section(&section)
        .await
        .map_err(|error| internal(&format!("Failed to load settings section={section}"), &error))?;
    let merged = Value::Dict(masking::unmask_section(&section, payload, &as_dict(stored.dump())));

    let candidate = match SectionValue::validate(&section, &merged) {
        Ok(candidate) => candidate,
        Err(SectionError::Invalid(error)) => {
            return Err(ApiError::new(422, "invalid_settings", "Settings validation failed.").with_details(error.to_value()));
        }
        Err(SectionError::Unknown(_)) => return Err(unknown_section(&section)),
    };
    let missing = candidate.validate_runnable();
    if candidate.enabled() && !missing.is_empty() {
        return Err(ApiError::new(
            422,
            "incomplete_settings",
            "Cannot enable a source while its configuration is incomplete.",
        )
        .with_details(json::dict([("missing_fields", json::str_list(&missing))])));
    }
    let saved = state
        .settings
        .save_section(&section, &merged)
        .await
        .map_err(|error| internal(&format!("Failed to save settings section={section}"), &error))?;
    Ok(settings_section_value(&section, &saved))
}

/// `test_telegram_notification`: a test message with the stored bot credentials.
async fn test_telegram_notification(state: &ApiState) -> Result<Response, ApiError> {
    Ok(json_response(200, &send_test_notification(state).await?))
}

/// `FavApiService.test_telegram_notification`.
pub(crate) async fn send_test_notification(state: &ApiState) -> Result<Value, ApiError> {
    let settings = state.settings.load().await.map_err(|error| unhandled(&error))?;
    match state.bot.send_test_notification(&settings).await {
        Ok(result) => Ok(json::dict([
            ("status", Value::from("delivered")),
            ("message_id", Value::from(result.message_id)),
            ("warnings", json::str_list(&result.warnings)),
        ])),
        Err(error) if error == TelegramDeliveryError::not_configured() => Err(ApiError::new(
            422,
            "telegram_not_configured",
            "Save bot_token and chat_id before sending a test notification.",
        )),
        Err(error) => Err(ApiError::new(502, "telegram_delivery_failed", error.message)),
    }
}

/// `test_tikhub`: check the stored TikHub key and read the balance left on its account
/// (not billed).
async fn test_tikhub(state: &ApiState) -> Result<Response, ApiError> {
    Ok(json_response(200, &check_tikhub(state).await?))
}

/// `FavApiService.test_tikhub`.
pub(crate) async fn check_tikhub(state: &ApiState) -> Result<Value, ApiError> {
    let stored = state
        .settings
        .load_section("services.tikhub")
        .await
        .map_err(|error| unhandled(&error))?;
    let SectionValue::TikHub(stored) = stored else {
        return Err(unhandled(&"services.tikhub is not a TikHub section"));
    };
    match fav_sources::tikhub::check_account(&stored).await {
        Ok(account) => Ok(json::dict([
            ("key_name", Value::from(account.key_name.as_str())),
            ("balance", Value::Float(account.balance)),
            ("free_credit", Value::Float(account.free_credit)),
        ])),
        Err(error) => {
            let status = if error.code == "tikhub_not_configured" { 422 } else { 502 };
            Err(ApiError::new(status, error.code, error.message))
        }
    }
}

// ---------- archive ----------

async fn list_archive_sources(state: &ApiState) -> Result<Response, ApiError> {
    Ok(json_response(200, &list_response(archive_sources(state).await?)))
}

/// `FavApiService.list_archive_sources`.
pub(crate) async fn archive_sources(state: &ApiState) -> Result<Vec<Value>, ApiError> {
    archive::list_sources(&state.db)
        .await
        .map_err(|error| internal("Failed to list archive sources", &error))
}

async fn list_archive_items(state: &ApiState, request: &Request) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let source = params::str_param(
        &mut errors,
        Source::Query,
        "source",
        request.query_last("source"),
        true,
        StrRules {
            min_length: Some(1),
            max_length: Some(40),
            pattern: None,
        },
    );
    let query = params::str_param(
        &mut errors,
        Source::Query,
        "q",
        request.query_last("q"),
        false,
        StrRules {
            min_length: Some(1),
            max_length: Some(200),
            pattern: None,
        },
    );
    let limit = params::int_param(
        &mut errors,
        Source::Query,
        "limit",
        request.query_last("limit"),
        Some(archive::DEFAULT_LIMIT),
        IntBounds {
            ge: Some(1),
            le: Some(200),
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
        _ => archive::DEFAULT_LIMIT,
    };
    let offset = match offset {
        Some(BigInt::Fits(offset)) => offset,
        Some(BigInt::AbovePositive) => i64::MAX,
        _ => 0,
    };
    let page = archive_items(
        state,
        &source.unwrap_or_default(),
        query.as_deref().unwrap_or_default(),
        limit,
        offset,
    )
    .await?;
    Ok(json_response(200, &page))
}

/// `FavApiService.list_archive_items`.
pub(crate) async fn archive_items(state: &ApiState, source_key: &str, query: &str, limit: i64, offset: i64) -> Result<Value, ApiError> {
    let Some(source) = archive::source(source_key) else {
        return Err(
            ApiError::new(404, "unknown_archive_source", format!("Unknown archive source: {source_key}"))
                .with_details(json::dict([("allowed_values", json::str_list(&archive::sorted_keys()))])),
        );
    };
    let failed = |error: &dyn std::fmt::Display| internal(&format!("Failed to list archive items source={source_key}"), error);
    let settings = state.settings.load().await.map_err(|error| failed(&error))?;
    archive::list_items(&state.db, &settings, source, query, limit, offset)
        .await
        .map_err(|error| failed(&error))
}

// ---------- site sessions and credentials ----------

/// `list_site_sessions`: every login-backed site with its stored session. No login can be
/// in progress before the login window exists (WP16), so `login` is always null.
async fn list_site_sessions(state: &ApiState) -> Result<Response, ApiError> {
    let settings = state.settings.load().await.map_err(|error| unhandled(&error))?;
    let stored = site_sessions::load_all(&state.db).await.map_err(|error| unhandled(&error))?;
    let items = site_logins::login_sites(&settings)
        .iter()
        .map(|site| site_logins::session_view(site, stored.get(&site.key)))
        .collect();
    Ok(json_response(
        200,
        &json::dict([("items", Value::List(items)), ("login", Value::Null)]),
    ))
}

/// `get_site_credential`: a client's read of one site's cookies.
async fn get_site_credential(state: &ApiState, raw: Option<&str>, request: &Request) -> Result<Response, ApiError> {
    let mut errors = Errors::new();
    let site = params::str_param(
        &mut errors,
        Source::Path,
        "site",
        raw,
        true,
        StrRules {
            min_length: Some(1),
            max_length: Some(96),
            pattern: Some((SITE_PATTERN, &SITE_REGEX)),
        },
    );
    errors.finish()?;
    let site = site.unwrap_or_default();
    let client = credentials::authenticate_client(&state.credential_clients, request.header("authorization").as_deref())?;
    if !client.sites.contains(&site) {
        return Err(ApiError::new(403, "site_not_allowed", format!("This client may not read {site}.")));
    }
    let stored = site_sessions::load(&state.db, &site).await.map_err(|error| unhandled(&error))?;
    let cookies: Vec<_> = stored
        .iter()
        .flat_map(|stored| stored.cookies.iter())
        .filter(|cookie| !cookie.value.is_empty() && cookie.value != VALUE_IN_PROFILE)
        .collect();
    let Some(stored) = stored.as_ref().filter(|_| !cookies.is_empty()) else {
        return Err(ApiError::new(404, "not_signed_in", format!("{site} has no stored session.")));
    };
    log().info(format!("Served the {site} session to {}", client.name));
    let cookies = cookies
        .iter()
        .map(|cookie| {
            json::dict([
                ("name", Value::Str(cookie.name.clone())),
                ("value", Value::Str(cookie.value.clone())),
                ("domain", Value::Str(cookie.domain.clone())),
                ("path", Value::Str(cookie.path.clone())),
                ("expires", Value::from(cookie.expires)),
            ])
        })
        .collect();
    let body = json::dict([
        ("site", Value::Str(site.clone())),
        ("account", Value::Str(stored.account.clone())),
        ("status", Value::Str(stored.status.clone())),
        ("signed_in_at", Value::from(stored.signed_in_at.as_ref().map(json::isoformat_utc))),
        ("checked_at", Value::from(stored.checked_at.as_ref().map(json::isoformat_utc))),
        ("cookies", Value::List(cookies)),
    ]);
    let mut response = json_response(200, &body);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
