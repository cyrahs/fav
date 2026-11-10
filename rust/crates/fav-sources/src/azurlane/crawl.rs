//! The Azur Lane crawl (WP12): the source snapshots and catalog of [`super::l2d_sources`],
//! the per-ship detail from the l2d.su origin, every model's files archived into a
//! content-addressed blob store, and the per-character manifests the API reads.
//!
//! Python origin: `src/web/azurlane.py`. As there, the run holds the `azurlane` advisory
//! lock, only the l2d.su ship data travels through the origin client (the proxy, when one is
//! configured), and a model that failed rests out a 1/3/7-day cooldown before it is asked
//! for again. The tables come from the schema, not from the run (DIVERGENCES.md D116).
//!
//! The manifests are written one batch of 25 characters at a time, each batch reading only
//! its own characters' model and asset rows, so no query loads a whole table (D122).

mod assets;
#[cfg(test)]
mod golden;
mod manifests;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, FixedOffset, Utc};
use fav_core::logging::{self, Logger};
use fav_core::pyjson::{self, Dict, DumpOptions, Value, py_isoformat_datetime, py_str_datetime};
use fav_core::pypath;
use fav_core::pytext::py_strip;
use fav_db::{Db, Param, Statement};
use fav_net::client::BuildError;
use fav_net::download::{self, DownloadError, Downloaded, TempOptions};
use fav_net::reqwest::Method;
use fav_net::retry::{Sleeper, tokio_sleeper};
use fav_net::{ClientOptions, HttpClient, Request, TransportError};
use fav_runtime::BoxFuture;
use fav_runtime::source::{JobError, Registry, Source};
use sqlx::Row as _;
use sqlx::postgres::PgRow;
use tokio::sync::Semaphore;

use self::assets::{
    Asset, BODY_PREFIX_LIMIT, CDN_REJECTION_REASON, LIMITED_STATUSES, MAX_RETRIES, MESH_UNAVAILABLE_REASON, METHOD_UNSUPPORTED,
    assets_from_enumeration, character_root, check_archive_continuity, download_candidates, is_cdn_rejection_body, max_attempts_for_status,
    merge_character_metadata, model3_seed_asset, painting_index_seed_asset, retry_cooldown_days, source_failure_message,
    source_snapshots_complete, spine_parts_seed_asset, validate_asset_response,
};
use self::manifests::{
    AssetRow, CharacterRow, ModelRow, character_manifest_root, character_payload, character_summary_payload, model_assets, model_payload,
    model_sort_key,
};
use super::l2d_sources::{
    ArchivedModel, AzurLaneModelCatalog, AzurLaneModelResourceEnumeration, AzurLaneSourceSnapshots, DEFAULT_USER_AGENT, EnumerationInputs,
    Fetch, FetchedResponse, L2D_SU_ARCHIVED_ASSET_BASE_URLS, L2D_SU_ASSET_BASE_URL, L2D_SU_PRIMARY_REGION, L2D_SU_REFERER, L2DSuVoiceLine,
    ModelEntry, ModelKind, PaintingIndex, SOURCE_SCHEMA_ERROR, apply_l2d_su_model_paths, apply_l2d_su_ship_classes,
    build_azurlane_l2d_health_report, build_azurlane_model_catalog, check_origin_proxy, entry_painting_index_url,
    enumerate_azurlane_model_resources, fetch_source_snapshots, is_hashed_asset_url, is_value_error, l2d_su_asset_url_from_legacy,
    l2d_su_character_fingerprint, l2d_su_legacy_asset_urls, l2d_su_origin_client_options, l2d_su_ship_detail_url, model_entry_from_dict,
    model_probe_urls, parse_l2d_su_ship_models, parse_l2d_su_ship_voices, parse_painting_index, pin_archived_entries,
    protected_response_text, source_client_options, spine_parts_manifest_url, spine_resource_manifest,
};
use super::protected;
use crate::bd2::extract::{retry_after_delay_seconds, retry_delay_seconds};
use crate::bd2::gamekee::{decode_utf8_ignore, materialize_blob, os_error, sha256_hex, verify_file, write_json};
use crate::kit::{blocking, status_error_message};
use crate::{PyError, SourceContext, db_job_error, pyerr, settings_job_error};

const API_REQUEST_INTERVAL_SECONDS: f64 = 0.5;
/// The fraction of the origin interval added as uniform random jitter.
const ORIGIN_JITTER_FRACTION: f64 = 0.25;
/// A rotating proxy hands out a fresh exit per request and some are blocked, so a failed
/// origin request is worth another try on another exit.
const ORIGIN_ATTEMPTS: u32 = 3;
/// The CDN's request rate is capped by this interval alone; the concurrency below only
/// decides how close the run gets to it.
const CDN_REQUEST_INTERVAL_SECONDS: f64 = 0.2;
const CDN_CONCURRENCY: usize = 12;
const ASSET_PROCESS_CONCURRENCY: usize = 12;
/// A stalled CDN connection is the run's dominant cost; the retry after a stall almost
/// always succeeds, so failing fast is nearly free.
pub const ASSET_READ_TIMEOUT: Duration = Duration::from_secs(12);
pub const ASSET_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const SOURCE_TIMEOUT: Duration = Duration::from_secs(30);
const ORIGIN_READ_TIMEOUT: Duration = Duration::from_mins(1);
const ORIGIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CIRCUIT_LIMITED_FAILURES: u32 = 5;
const CIRCUIT_PAUSE_SECONDS: f64 = 30.0;
/// Characters whose manifests are built from one asset query: a few thousand rows.
const MANIFEST_CHARACTER_BATCH: i64 = 25;

fn log() -> Logger {
    logging::get("azurlane")
}

/// `datetime.now(UTC)`, injectable.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
/// `secrets.randbelow(250)`, injectable.
pub type Jitter = Arc<dyn Fn() -> u32 + Send + Sync>;
/// `random.uniform(0, high)`, injectable.
pub type Uniform = Arc<dyn Fn(f64) -> f64 + Send + Sync>;

fn time_seed() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos())
}

fn random_jitter() -> Jitter {
    Arc::new(|| time_seed() % 250)
}

fn random_uniform() -> Uniform {
    Arc::new(|high| f64::from(time_seed() % 1_000_000) / 1_000_000.0 * high)
}

fn utc() -> FixedOffset {
    FixedOffset::east_opt(0).unwrap_or_else(|| unreachable!())
}

/// `_asset_headers()`.
fn asset_headers() -> Vec<(String, String)> {
    vec![
        ("Accept".to_owned(), "*/*".to_owned()),
        ("Referer".to_owned(), L2D_SU_REFERER.to_owned()),
        ("User-Agent".to_owned(), DEFAULT_USER_AGENT.to_owned()),
    ]
}

/// `_http_client()`: the CDN client (`follow_redirects=True`, asset headers, a 12 s read and
/// 5 s connect timeout).
pub fn asset_client_options() -> ClientOptions {
    ClientOptions {
        headers: asset_headers(),
        timeout: ASSET_READ_TIMEOUT,
        connect_timeout: Some(ASSET_CONNECT_TIMEOUT),
        follow_redirects: true,
        ..ClientOptions::default()
    }
}

/// `_origin_http_client(...)`: the per-ship detail client, through `proxy` with keep-alive
/// off, so every request opens a new tunnel on a new exit.
pub fn origin_client_options(proxy: &str, connect_host: &str) -> ClientOptions {
    ClientOptions {
        headers: asset_headers(),
        connect_timeout: Some(ORIGIN_CONNECT_TIMEOUT),
        ..l2d_su_origin_client_options(proxy, connect_host, ORIGIN_READ_TIMEOUT, 0)
    }
}

/// The clients a run uses. `origin_source` and `origin` carry the l2d.su ship data only;
/// without them it goes through `source` and `assets`, as Python's fallbacks do.
#[derive(Debug, Clone)]
pub struct Clients {
    /// CDN assets, the model probes and, without an origin client, the ship details.
    pub assets: HttpClient,
    /// The nagami mapping and, without an origin client, the ship index.
    pub source: HttpClient,
    /// The ship index through the origin proxy.
    pub origin_source: Option<HttpClient>,
    /// The per-ship detail through the origin proxy.
    pub origin: Option<HttpClient>,
}

impl Clients {
    /// The production clients; a proxy httpx would refuse fails as it would there
    /// (DIVERGENCES.md D113).
    pub fn build(origin_proxy: &str, origin_connect_host: &str) -> Result<Self, BuildError> {
        let assets = HttpClient::new(&asset_client_options())?;
        let source = HttpClient::new(&source_client_options(SOURCE_TIMEOUT))?;
        if origin_proxy.is_empty() {
            return Ok(Self {
                assets,
                source,
                origin_source: None,
                origin: None,
            });
        }
        check_origin_proxy(origin_proxy)?;
        Ok(Self {
            assets,
            source,
            origin_source: Some(HttpClient::new(&l2d_su_origin_client_options(
                origin_proxy,
                origin_connect_host,
                SOURCE_TIMEOUT,
                0,
            ))?),
            origin: Some(HttpClient::new(&origin_client_options(origin_proxy, origin_connect_host))?),
        })
    }
}

/// The request spacing and concurrency (`AzurLane(...)`'s keyword arguments).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pacing {
    pub api_request_interval_seconds: f64,
    pub cdn_request_interval_seconds: f64,
    pub origin_request_interval_seconds: f64,
    pub origin_attempts: u32,
    pub asset_process_concurrency: usize,
}

impl Pacing {
    /// The defaults, with the origin interval from the settings.
    pub fn new(origin_request_interval_seconds: f64) -> Self {
        Self {
            api_request_interval_seconds: API_REQUEST_INTERVAL_SECONDS,
            cdn_request_interval_seconds: CDN_REQUEST_INTERVAL_SECONDS,
            origin_request_interval_seconds,
            origin_attempts: ORIGIN_ATTEMPTS,
            asset_process_concurrency: ASSET_PROCESS_CONCURRENCY,
        }
    }
}

/// `_RateLimiter`: at least `interval` (plus jitter) seconds between the starts of two
/// requests. One serves every origin request, index and detail alike: the pacing is global
/// (Python's `_SourceRateLimiter` for the index is the same limiter here, D119).
#[derive(Debug)]
struct RateLimiter {
    interval: f64,
    jitter: f64,
    last_start: tokio::sync::Mutex<Option<Instant>>,
}

impl RateLimiter {
    fn new(interval: f64, jitter: f64) -> Self {
        Self {
            interval,
            jitter,
            last_start: tokio::sync::Mutex::new(None),
        }
    }

    async fn wait(&self, sleeper: &Sleeper, uniform: &Uniform) {
        if self.interval <= 0.0 {
            return;
        }
        let mut last_start = self.last_start.lock().await;
        let target = self.interval + if self.jitter > 0.0 { uniform(self.jitter) } else { 0.0 };
        if let Some(previous) = *last_start {
            let delay = target - previous.elapsed().as_secs_f64();
            if delay > 0.0 {
                sleeper(delay).await;
            }
        }
        *last_start = Some(Instant::now());
    }

    /// `note_request()`: an origin request just happened elsewhere; space the next from now.
    fn note_request(&self) {
        if self.interval <= 0.0 {
            return;
        }
        if let Ok(mut last_start) = self.last_start.try_lock() {
            *last_start = Some(Instant::now());
        }
    }
}

/// `_CircuitBreaker`: after five throttling answers in a row the run pauses for 30 s.
#[derive(Debug, Default)]
struct CircuitBreaker {
    limited_failures: Mutex<u32>,
}

impl CircuitBreaker {
    fn failures(&self) -> std::sync::MutexGuard<'_, u32> {
        self.limited_failures.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn record_limited_failure(&self, sleeper: &Sleeper) {
        {
            let mut failures = self.failures();
            *failures += 1;
            if *failures < CIRCUIT_LIMITED_FAILURES {
                return;
            }
        }
        log().warning(format_args!(
            "Azur Lane CDN returned repeated throttling responses; pausing this run for {CIRCUIT_PAUSE_SECONDS:.0}s"
        ));
        sleeper(CIRCUIT_PAUSE_SECONDS).await;
        *self.failures() = 0;
    }

    fn record_success(&self) {
        *self.failures() = 0;
    }
}

/// `BlobRef`: a verified file in the blob store.
#[derive(Debug, Clone)]
struct BlobRef {
    sha256: String,
    size: i64,
    content_type: String,
    path: PathBuf,
    downloaded_url: String,
}

/// `TempBlob`: a downloaded (and, for an AZP2 envelope, opened) body not yet in the store.
#[derive(Debug)]
struct TempBlob {
    file: Downloaded,
    sha256: String,
    size: i64,
    content_type: String,
    downloaded_url: String,
}

/// How one download attempt ended without a file.
enum AttemptError {
    /// `UnavailableAssetError`: a definite answer that is not the asset; try the next URL.
    Unavailable(JobError),
    /// `HTTPStatusError`, with the server's `Retry-After`.
    Status {
        error: JobError,
        status: u16,
        retry_after: Option<f64>,
    },
    /// A transport failure or an invalid body: logged, then retried.
    Retry(JobError),
    /// Anything else, raised at once.
    Raise(JobError),
}

/// `ModelRetryState`.
#[derive(Debug, Default)]
struct ModelRetryState {
    failed_model_ids: HashSet<String>,
    cooldown_until: HashMap<String, String>,
}

/// `ModelCrawlOutcome`.
#[derive(Debug, Default)]
pub struct ModelCrawlOutcome {
    pub failed_model_ids: Vec<String>,
    pub cooldown_model_ids: Vec<String>,
}

/// What one run learned about the ship details.
#[derive(Debug, Default)]
struct RunState {
    origin_detail_spent: u32,
    ship_model_paths: HashMap<i128, HashMap<i128, String>>,
    ship_detail_payloads: HashMap<i128, Option<String>>,
    ship_voices: HashMap<i128, Vec<(i128, Vec<L2DSuVoiceLine>)>>,
    character_fingerprints: HashMap<i128, String>,
}

/// A client as the [`Fetch`] the source functions take, sending `https://` as `http://`
/// in tests (so the fake web, an HTTP proxy, sees the URL) and reporting the `https://` URL.
struct Wire<'a> {
    http: &'a HttpClient,
    plain_http: bool,
}

impl Wire<'_> {
    fn url(&self, url: &str) -> String {
        wire_url(url, self.plain_http)
    }
}

fn wire_url(url: &str, plain_http: bool) -> String {
    match url.strip_prefix("https://") {
        Some(rest) if plain_http => format!("http://{rest}"),
        _ => url.to_owned(),
    }
}

impl Fetch for Wire<'_> {
    fn request<'a>(
        &'a self,
        method: &'a str,
        url: &'a str,
        timeout: Option<Duration>,
    ) -> BoxFuture<'a, Result<FetchedResponse, TransportError>> {
        Box::pin(async move {
            let wire = self.url(url);
            let mut response = Fetch::request(self.http, method, &wire, timeout).await?;
            if self.plain_http
                && let Some(rest) = response.url.strip_prefix("http://")
            {
                response.url = format!("https://{rest}");
            }
            Ok(response)
        })
    }
}

/// A count or id as a `BIGINT` parameter.
fn bigint(value: i128) -> Result<i64, JobError> {
    i64::try_from(value).map_err(|_| JobError::new("NumericValueOutOfRange", "bigint out of range"))
}

fn optional_bigint(value: Option<i128>) -> Result<Param, JobError> {
    Ok(Param::Int(value.map(bigint).transpose()?))
}

fn text_param(value: impl Into<String>) -> Param {
    Param::Text(Some(value.into()))
}

fn json_dumps(value: &Value) -> Result<String, JobError> {
    pyjson::dumps(value, &DumpOptions::hash()).map_err(|error| JobError::new("TypeError", error.to_string()))
}

fn column<T>(row: &PgRow, name: &str) -> Result<T, JobError>
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get(name).map_err(|error| db_job_error(&fav_db::DbError::Sqlx(error)))
}

/// `_row_text(row, key)`: the column stripped, `''` for NULL.
fn row_text(row: &PgRow, name: &str) -> Result<String, JobError> {
    let value: Option<String> = column(row, name)?;
    Ok(value.map(|value| py_strip(&value).to_owned()).unwrap_or_default())
}

/// `_timestamp_value(row.get(key))`: `isoformat()` of a `TIMESTAMPTZ` column.
fn row_timestamp(row: &PgRow, name: &str) -> Result<Option<String>, JobError> {
    let value: Option<DateTime<Utc>> = column(row, name)?;
    Ok(value.map(|value| py_isoformat_datetime(&value.with_timezone(&utc()))))
}

/// `_json_object(row.get(key))` of a `JSONB` column read as text: a dict, else `{}`.
fn row_json_object(row: &PgRow, name: &str) -> Result<Dict, JobError> {
    let value: Option<String> = column(row, name)?;
    Ok(match value.as_deref().map(pyjson::loads) {
        Some(Ok(Value::Dict(dict))) => dict,
        _ => Dict::new(),
    })
}

fn error_text(error: &JobError) -> String {
    if error.message().is_empty() {
        error.class_name().to_owned()
    } else {
        error.message().to_owned()
    }
}

/// A source parser's `SourceSchemaError` or `ValueError`, which the ship detail readers log
/// and move past.
fn is_parse_failure(error: &PyError) -> bool {
    is_value_error(error) || error.class == SOURCE_SCHEMA_ERROR
}

/// The crawler for one run: `AzurLane(path=..., client=..., ...)`.
pub struct AzurLaneCrawler {
    root: String,
    db: Db,
    clients: Clients,
    sleeper: Sleeper,
    jitter: Jitter,
    uniform: Uniform,
    clock: Clock,
    /// Tests only: send `https://` URLs as `http://` so the fake web sees them.
    plain_http: bool,
    pacing: Pacing,
    api_limiter: Arc<RateLimiter>,
    cdn_limiter: Arc<RateLimiter>,
    origin_limiter: Arc<RateLimiter>,
    cdn_semaphore: Semaphore,
    circuit_breaker: CircuitBreaker,
    state: Mutex<RunState>,
    /// [`MANIFEST_CHARACTER_BATCH`]; tests shrink it.
    manifest_batch: i64,
    /// Tests only: how many asset queries the manifests took.
    #[cfg(test)]
    manifest_asset_queries: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for AzurLaneCrawler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AzurLaneCrawler").field("root", &self.root).finish_non_exhaustive()
    }
}

/// The first [`BODY_PREFIX_LIMIT`] bytes of a downloaded file, for the response checks.
async fn read_prefix(path: PathBuf) -> Result<Vec<u8>, JobError> {
    blocking(move || {
        use std::io::Read as _;
        let mut prefix = Vec::with_capacity(BODY_PREFIX_LIMIT);
        std::fs::File::open(&path)
            .and_then(|handle| handle.take(BODY_PREFIX_LIMIT as u64).read_to_end(&mut prefix))
            .map_err(|error| os_error(&error, &path))?;
        Ok(prefix)
    })
    .await
}

/// `_open_protected_file`: decrypts the AZP2 envelope in place (the archive keeps the opened
/// file) and returns the content, or the decrypt error's message.
async fn open_protected_file(path: PathBuf, url: String) -> Result<Result<Vec<u8>, String>, JobError> {
    blocking(move || {
        let content = std::fs::read(&path).map_err(|error| os_error(&error, &path))?;
        match protected::decrypt_for_url(content, &url) {
            Ok(content) => {
                std::fs::write(&path, &content).map_err(|error| os_error(&error, &path))?;
                Ok(Ok(content))
            }
            Err(error) => Ok(Err(error.message)),
        }
    })
    .await
}

/// The `azurlane_models` upsert of `_upsert_catalog_state` for one entry.
fn model_upsert(entry: &ModelEntry) -> Result<Statement, JobError> {
    Ok(Statement::new(
        "INSERT INTO azurlane_models (
            model_id, model_type, source, character_key, costume_key, costume_id,
            costume_name_zh, costume_name_en, primary_url, fallback_url, display_info_url,
            availability_state, active, source_metadata, first_seen_at, last_seen_at, fetched_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, TRUE, $13::jsonb, NOW(), NOW(), NOW())
        ON CONFLICT (model_id) DO UPDATE SET
            model_type = excluded.model_type,
            source = excluded.source,
            character_key = excluded.character_key,
            costume_key = excluded.costume_key,
            costume_id = excluded.costume_id,
            costume_name_zh = excluded.costume_name_zh,
            costume_name_en = excluded.costume_name_en,
            primary_url = excluded.primary_url,
            fallback_url = excluded.fallback_url,
            display_info_url = excluded.display_info_url,
            availability_state = excluded.availability_state,
            active = TRUE,
            source_metadata = CASE
                WHEN jsonb_typeof(azurlane_models.source_metadata -> 'painting_index') = 'object'
                    THEN excluded.source_metadata
                        || jsonb_build_object('painting_index', azurlane_models.source_metadata -> 'painting_index')
                ELSE excluded.source_metadata
            END,
            failed_count = CASE
                WHEN azurlane_models.primary_url IS DISTINCT FROM excluded.primary_url THEN 0
                ELSE azurlane_models.failed_count
            END,
            next_retry_at = CASE
                WHEN azurlane_models.primary_url IS DISTINCT FROM excluded.primary_url THEN NULL
                ELSE azurlane_models.next_retry_at
            END,
            last_seen_at = NOW(),
            fetched_at = NOW();",
        vec![
            text_param(entry.id.as_str()),
            text_param(entry.r#type.as_str()),
            text_param(entry.source.as_str()),
            text_param(entry.character.key.as_str()),
            text_param(entry.costume.key.as_str()),
            optional_bigint(entry.costume.id)?,
            text_param(entry.costume.name_zh.as_str()),
            text_param(entry.costume.name_en.as_str()),
            text_param(entry.resources.primary_url.as_str()),
            text_param(entry.resources.fallback_url.as_str()),
            text_param(entry.resources.display_info_url.as_str()),
            text_param(entry.availability.state.as_str()),
            text_param(json_dumps(&entry.to_dict())?),
        ],
    ))
}

impl AzurLaneCrawler {
    pub fn new(root: &str, clients: Clients, db: Db, pacing: Pacing) -> Self {
        let origin_interval = pacing.origin_request_interval_seconds;
        Self {
            root: root.to_owned(),
            db,
            clients,
            sleeper: tokio_sleeper(),
            jitter: random_jitter(),
            uniform: random_uniform(),
            clock: Arc::new(Utc::now),
            plain_http: false,
            api_limiter: Arc::new(RateLimiter::new(pacing.api_request_interval_seconds, 0.0)),
            cdn_limiter: Arc::new(RateLimiter::new(pacing.cdn_request_interval_seconds, 0.0)),
            origin_limiter: Arc::new(RateLimiter::new(origin_interval, origin_interval * ORIGIN_JITTER_FRACTION)),
            pacing: Pacing {
                origin_attempts: pacing.origin_attempts.max(1),
                ..pacing
            },
            cdn_semaphore: Semaphore::new(CDN_CONCURRENCY),
            circuit_breaker: CircuitBreaker::default(),
            state: Mutex::new(RunState::default()),
            manifest_batch: MANIFEST_CHARACTER_BATCH,
            #[cfg(test)]
            manifest_asset_queries: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    #[must_use]
    pub fn with_sleeper(mut self, sleeper: Sleeper) -> Self {
        self.sleeper = sleeper;
        self
    }

    #[must_use]
    pub fn with_jitter(mut self, jitter: Jitter, uniform: Uniform) -> Self {
        self.jitter = jitter;
        self.uniform = uniform;
        self
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_plain_http(mut self) -> Self {
        self.plain_http = true;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_manifest_batch(mut self, batch: i64) -> Self {
        self.manifest_batch = batch;
        self
    }

    fn path(&self, relative: &str) -> PathBuf {
        PathBuf::from(pypath::join(&self.root, relative))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, RunState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `_utc_now_iso()`.
    fn now_iso(&self) -> String {
        py_isoformat_datetime(&(self.clock)().with_timezone(&utc()))
    }

    /// The detail client of the run (`self._active_origin_client or client`).
    fn origin_client(&self) -> &HttpClient {
        self.clients.origin.as_ref().unwrap_or(&self.clients.assets)
    }

    /// `_retry_delay_seconds(attempt, response)`.
    fn retry_delay(&self, attempt: u32, retry_after: Option<f64>) -> f64 {
        match retry_after {
            Some(seconds) => seconds,
            None => retry_delay_seconds(attempt, None, (self.jitter)()),
        }
    }

    async fn wait_cdn(&self) {
        self.cdn_limiter.wait(&self.sleeper, &self.uniform).await;
    }

    async fn wait_origin(&self) {
        self.origin_limiter.wait(&self.sleeper, &self.uniform).await;
    }

    // ---------- database ----------

    async fn execute(&self, sql: &str, params: Vec<Param>) -> Result<Vec<PgRow>, JobError> {
        crate::execute(&self.db, sql, params).await
    }

    async fn transaction(&self, statements: Vec<Statement>) -> Result<(), JobError> {
        self.db
            .query_transaction(statements)
            .await
            .map(|_| ())
            .map_err(|error| db_job_error(&error))
    }

    // ---------- source snapshots ----------

    /// `_fetch_source_snapshots()`: both sources; the index requests pass the origin throttle.
    async fn fetch_source_snapshots(&self) -> Result<AzurLaneSourceSnapshots, JobError> {
        self.api_limiter.wait(&self.sleeper, &self.uniform).await;
        let source = Wire {
            http: &self.clients.source,
            plain_http: self.plain_http,
        };
        let origin = self.clients.origin_source.as_ref().map(|http| Wire {
            http,
            plain_http: self.plain_http,
        });
        let (limiter, sleeper, uniform) = (Arc::clone(&self.origin_limiter), self.sleeper.clone(), self.uniform.clone());
        let throttle = move || -> BoxFuture<'static, ()> {
            let (limiter, sleeper, uniform) = (Arc::clone(&limiter), sleeper.clone(), uniform.clone());
            Box::pin(async move { limiter.wait(&sleeper, &uniform).await })
        };
        let clock = || self.now_iso();
        let snapshots = fetch_source_snapshots(&source, origin.as_ref().map(|wire| wire as &dyn Fetch), Some(&throttle), &clock).await;
        // Space the first detail request a full interval after the last index request.
        self.origin_limiter.note_request();
        snapshots.map_err(PyError::into_job_error)
    }

    /// `_write_source_artifacts(snapshots=..., catalog=...)`.
    async fn write_source_artifacts(&self, snapshots: &AzurLaneSourceSnapshots, catalog: &AzurLaneModelCatalog) -> Result<(), JobError> {
        let health_report =
            build_azurlane_l2d_health_report(snapshots, Some(catalog), None, None, self.now_iso()).map_err(PyError::into_job_error)?;
        let files = [
            (self.path("_source/l2d-su-snapshot.json"), snapshots.l2d_su.to_dict()),
            (self.path("_source/nagami-snapshot.json"), snapshots.nagami.to_dict()),
            (self.path("_source/health-report.json"), health_report.to_dict()),
        ];
        blocking(move || {
            for (path, value) in &files {
                write_json(path, value)?;
            }
            Ok(())
        })
        .await
    }

    // ---------- catalog state ----------

    /// `_archived_models()`: every model the archive has a row for, as the catalog last
    /// described it. Streamed: one row's metadata is parsed at a time. The parsed entries are
    /// kept, because pinning them needs the whole set; that is one entry per archived model
    /// (catalog-sized), not the table's assets or payloads.
    async fn archived_models(&self) -> Result<Vec<ArchivedModel>, JobError> {
        use futures_util::TryStreamExt as _;

        let mut rows = sqlx::query(
            "SELECT model_id, primary_url, source_metadata::text AS source_metadata, completed_at
            FROM azurlane_models;",
        )
        .fetch(self.db.pool());
        let mut archived = Vec::new();
        while let Some(row) = rows.try_next().await.map_err(|error| db_job_error(&fav_db::DbError::Sqlx(error)))? {
            let metadata = row_json_object(&row, "source_metadata")?;
            let model_id = row_text(&row, "model_id")?;
            let Some(mut entry) = model_entry_from_dict(&Value::Dict(metadata)) else {
                continue;
            };
            if model_id.is_empty() {
                continue;
            }
            entry.id = model_id;
            entry.resources.primary_url = row_text(&row, "primary_url")?;
            let completed: Option<DateTime<Utc>> = column(&row, "completed_at")?;
            archived.push(ArchivedModel {
                entry,
                completed: completed.is_some(),
            });
        }
        Ok(archived)
    }

    /// `_upsert_catalog_state(catalog)`: every character and model of the catalog active,
    /// everything else inactive, in one transaction.
    async fn upsert_catalog_state(&self, catalog: &AzurLaneModelCatalog) -> Result<(), JobError> {
        let mut metadata_by_key: HashMap<String, (Vec<String>, Vec<String>, Dict)> = HashMap::new();
        let mut character_rows: HashMap<String, (Option<i128>, String, String, String)> = HashMap::new();
        let mut statements = vec![
            Statement::new("UPDATE azurlane_characters SET active = FALSE;", Vec::new()),
            Statement::new("UPDATE azurlane_models SET active = FALSE;", Vec::new()),
        ];
        for entry in &catalog.entries {
            let (model_ids, sources, metadata) = metadata_by_key.entry(entry.character.key.clone()).or_default();
            model_ids.push(entry.id.clone());
            sources.push(entry.source.as_str().to_owned());
            merge_character_metadata(metadata, &entry.character);
            if character_rows.get(&entry.character.key).is_none_or(|existing| existing.0.is_none()) {
                character_rows.insert(
                    entry.character.key.clone(),
                    (
                        entry.character.id,
                        entry.character.name_zh.clone(),
                        entry.character.name_en.clone(),
                        pypath::join(&character_root(&self.root, entry), "manifest.json"),
                    ),
                );
            }
        }
        let mut keys: Vec<&String> = character_rows.keys().collect();
        keys.sort();
        for character_key in keys {
            let (source_id, name_zh, name_en, manifest_path) = &character_rows[character_key];
            let (model_ids, sources, extra) = &metadata_by_key[character_key];
            let sorted_unique = |values: &[String]| {
                let mut values: Vec<String> = values.to_vec();
                values.sort();
                values.dedup();
                Value::List(values.into_iter().map(Value::Str).collect())
            };
            let mut metadata = Dict::new();
            metadata.insert("model_ids", sorted_unique(model_ids));
            metadata.insert("sources", sorted_unique(sources));
            for (key, value) in extra.iter() {
                metadata.insert(super::l2d_sources::key_text(key), value.clone());
            }
            statements.push(Statement::new(
                "INSERT INTO azurlane_characters (
                    character_key, source_id, name_zh, name_en, manifest_path, active,
                    source_metadata, first_seen_at, last_seen_at, fetched_at
                )
                VALUES ($1, $2, $3, $4, $5, TRUE, $6::jsonb, NOW(), NOW(), NOW())
                ON CONFLICT (character_key) DO UPDATE SET
                    source_id = COALESCE(excluded.source_id, azurlane_characters.source_id),
                    name_zh = COALESCE(NULLIF(excluded.name_zh, ''), azurlane_characters.name_zh),
                    name_en = COALESCE(NULLIF(excluded.name_en, ''), azurlane_characters.name_en),
                    manifest_path = excluded.manifest_path,
                    active = TRUE,
                    source_metadata = excluded.source_metadata,
                    last_seen_at = NOW(),
                    fetched_at = NOW();",
                vec![
                    text_param(character_key.as_str()),
                    optional_bigint(*source_id)?,
                    text_param(name_zh.as_str()),
                    text_param(name_en.as_str()),
                    text_param(manifest_path.as_str()),
                    text_param(json_dumps(&Value::Dict(metadata))?),
                ],
            ));
        }
        for entry in &catalog.entries {
            statements.push(model_upsert(entry)?);
        }
        self.transaction(statements).await
    }

    /// `_mark_model_fetch_started(entry)`.
    async fn mark_model_fetch_started(&self, entry: &ModelEntry) -> Result<(), JobError> {
        self.execute(
            "UPDATE azurlane_models SET fetched_at = NOW(), completed_at = NULL, last_seen_at = NOW() WHERE model_id = $1;",
            vec![text_param(entry.id.as_str())],
        )
        .await
        .map(|_| ())
    }

    /// `_model_retry_state()`: models the source refused, and which still wait.
    async fn model_retry_state(&self) -> Result<ModelRetryState, JobError> {
        let rows = self
            .execute(
                "SELECT model_id, next_retry_at, (next_retry_at > NOW()) AS in_cooldown
                FROM azurlane_models
                WHERE failed_count > 0;",
                Vec::new(),
            )
            .await?;
        let mut state = ModelRetryState::default();
        for row in &rows {
            let model_id = row_text(row, "model_id")?;
            if model_id.is_empty() {
                continue;
            }
            let in_cooldown: Option<bool> = column(row, "in_cooldown")?;
            if in_cooldown == Some(true) {
                state
                    .cooldown_until
                    .insert(model_id.clone(), row_timestamp(row, "next_retry_at")?.unwrap_or_default());
            }
            state.failed_model_ids.insert(model_id);
        }
        Ok(state)
    }

    /// `_mark_model_retry_failed(entry, reason)`: the model waits 1, 3, then 7 days.
    async fn mark_model_retry_failed(&self, entry: &ModelEntry, reason: &str) -> Result<(), JobError> {
        self.execute(
            "UPDATE azurlane_models
            SET failed_count = failed_count + 1,
                last_error = $1,
                last_attempt_at = NOW(),
                next_retry_at = NOW() + (
                    CASE
                        WHEN failed_count + 1 <= 1 THEN $2::integer * INTERVAL '1 day'
                        WHEN failed_count + 1 = 2 THEN $3::integer * INTERVAL '1 day'
                        ELSE $4::integer * INTERVAL '1 day'
                    END
                )
            WHERE model_id = $5;",
            vec![
                text_param(reason.chars().take(500).collect::<String>()),
                Param::from(retry_cooldown_days(1)),
                Param::from(retry_cooldown_days(2)),
                Param::from(retry_cooldown_days(3)),
                text_param(entry.id.as_str()),
            ],
        )
        .await
        .map(|_| ())
    }

    /// `_clear_model_retry_state(entry)`.
    async fn clear_model_retry_state(&self, entry: &ModelEntry) -> Result<(), JobError> {
        self.execute(
            "UPDATE azurlane_models
            SET failed_count = 0, last_error = '', last_attempt_at = NOW(), next_retry_at = NULL
            WHERE model_id = $1;",
            vec![text_param(entry.id.as_str())],
        )
        .await
        .map(|_| ())
    }

    /// `_mark_character_completed(entry)`.
    async fn mark_character_completed(&self, entry: &ModelEntry) -> Result<(), JobError> {
        self.execute(
            "UPDATE azurlane_characters SET completed_at = NOW(), last_seen_at = NOW() WHERE character_key = $1;",
            vec![text_param(entry.character.key.as_str())],
        )
        .await
        .map(|_| ())
    }

    // ---------- assets and blobs ----------

    /// `_upsert_asset_seen(asset)`.
    async fn upsert_asset_seen(&self, asset: &Asset) -> Result<(), JobError> {
        self.execute(
            "INSERT INTO azurlane_assets (url, normalized_url, kind, fallback_url, status, first_seen_at, last_seen_at)
            VALUES ($1, $2, $3, $4, 'pending', NOW(), NOW())
            ON CONFLICT (url) DO UPDATE SET
                normalized_url = excluded.normalized_url,
                kind = COALESCE(NULLIF(excluded.kind, ''), azurlane_assets.kind),
                fallback_url = COALESCE(NULLIF(excluded.fallback_url, ''), azurlane_assets.fallback_url),
                last_seen_at = NOW(),
                status = CASE
                    WHEN azurlane_assets.status = 'downloaded' THEN azurlane_assets.status
                    WHEN azurlane_assets.status = 'failed' AND azurlane_assets.next_retry_at > NOW() THEN azurlane_assets.status
                    ELSE excluded.status
                END,
                next_retry_at = CASE
                    WHEN azurlane_assets.status = 'failed' AND azurlane_assets.next_retry_at > NOW() THEN azurlane_assets.next_retry_at
                    ELSE NULL
                END;",
            vec![
                text_param(asset.url.as_str()),
                text_param(asset.url.as_str()),
                text_param(asset.kind.as_str()),
                text_param(asset.fallback_url.as_str()),
            ],
        )
        .await
        .map(|_| ())
    }

    /// `_asset_retry_cooldown(asset)`: `(last_error, next_retry_at)` while a failed asset waits.
    async fn asset_retry_cooldown(&self, asset: &Asset) -> Result<Option<(String, String)>, JobError> {
        let rows = self
            .execute(
                "SELECT failed_count, last_error, next_retry_at
                FROM azurlane_assets
                WHERE url = $1 AND status = 'failed' AND next_retry_at > NOW()
                LIMIT 1;",
                vec![text_param(asset.url.as_str())],
            )
            .await?;
        let Some(row) = rows.first() else { return Ok(None) };
        let last_error: String = column(row, "last_error")?;
        let next_retry_at: Option<DateTime<Utc>> = column(row, "next_retry_at")?;
        let next_retry_at = next_retry_at.map_or_else(String::new, |when| py_str_datetime(&when.with_timezone(&utc())));
        Ok(Some((last_error, next_retry_at)))
    }

    /// `_resolve_blob_path(blob_path)`.
    fn resolve_blob_path(&self, blob_path: &str) -> PathBuf {
        if blob_path.starts_with('/') {
            PathBuf::from(blob_path)
        } else {
            self.path(blob_path)
        }
    }

    /// `_completed_blob_for_url(url)`: the archived blob for `url`, or for a URL the same
    /// file had before the CDN moved.
    async fn completed_blob_for_url(&self, url: &str) -> Result<Option<BlobRef>, JobError> {
        if let Some(blob) = self.completed_blob_for_exact_url(url).await? {
            return Ok(Some(blob));
        }
        for legacy_url in l2d_su_legacy_asset_urls(url) {
            if let Some(blob) = self.completed_blob_for_exact_url(&legacy_url).await? {
                return Ok(Some(blob));
            }
        }
        Ok(None)
    }

    async fn completed_blob_for_exact_url(&self, url: &str) -> Result<Option<BlobRef>, JobError> {
        let rows = self
            .execute(
                "SELECT
                    a.sha256,
                    a.size,
                    COALESCE(NULLIF(a.content_type, ''), b.content_type) AS content_type,
                    a.downloaded_url,
                    b.blob_path
                FROM azurlane_assets AS a
                JOIN azurlane_blobs AS b ON b.sha256 = a.sha256 AND b.size = a.size
                WHERE a.url = $1 AND a.status = 'downloaded' AND a.sha256 <> ''
                LIMIT 1;",
                vec![text_param(url)],
            )
            .await?;
        let Some(row) = rows.first() else { return Ok(None) };
        let sha256: String = column(row, "sha256")?;
        let size: i64 = column(row, "size")?;
        let content_type: Option<String> = column(row, "content_type")?;
        let downloaded_url: String = column(row, "downloaded_url")?;
        let blob_path: String = column(row, "blob_path")?;
        let path = self.resolve_blob_path(&blob_path);
        let verified = if sha256.is_empty() || size <= 0 {
            false
        } else {
            let (path, sha256) = (path.clone(), sha256.clone());
            blocking(move || Ok(verify_file(&path, &sha256, size))).await?
        };
        if !verified {
            self.execute(
                "UPDATE azurlane_assets SET status = 'missing', last_error = 'blob missing or failed verification' WHERE url = $1;",
                vec![text_param(url)],
            )
            .await?;
            return Ok(None);
        }
        Ok(Some(BlobRef {
            sha256,
            size,
            content_type: content_type.unwrap_or_default(),
            path,
            downloaded_url,
        }))
    }

    /// `_mark_asset_downloaded(asset, blob, downloaded_url=...)`.
    async fn mark_asset_downloaded(&self, asset: &Asset, blob: &BlobRef, downloaded_url: &str) -> Result<(), JobError> {
        self.execute(
            "UPDATE azurlane_assets
            SET sha256 = $1, size = $2, content_type = $3, downloaded_url = $4, status = 'downloaded',
                failed_count = 0, last_error = '', last_attempt_at = NOW(), next_retry_at = NULL, last_seen_at = NOW()
            WHERE url = $5;",
            vec![
                text_param(blob.sha256.as_str()),
                Param::from(blob.size),
                text_param(blob.content_type.as_str()),
                text_param(downloaded_url),
                text_param(asset.url.as_str()),
            ],
        )
        .await
        .map(|_| ())
    }

    /// `_mark_asset_failed(asset, reason)`: the asset waits 1, 3, then 7 days.
    async fn mark_asset_failed(&self, asset: &Asset, reason: &str) -> Result<(), JobError> {
        let first_retry_days = retry_cooldown_days(1);
        self.execute(
            "INSERT INTO azurlane_assets (
                url, normalized_url, kind, fallback_url, status, failed_count, last_error,
                last_attempt_at, next_retry_at, first_seen_at, last_seen_at
            )
            VALUES ($1, $2, $3, $4, 'failed', 1, $5, NOW(), NOW() + ($6::integer * INTERVAL '1 day'), NOW(), NOW())
            ON CONFLICT (url) DO UPDATE SET
                kind = COALESCE(NULLIF(excluded.kind, ''), azurlane_assets.kind),
                fallback_url = COALESCE(NULLIF(excluded.fallback_url, ''), azurlane_assets.fallback_url),
                status = 'failed',
                failed_count = azurlane_assets.failed_count + 1,
                last_error = excluded.last_error,
                last_attempt_at = NOW(),
                next_retry_at = NOW() + (
                    CASE
                        WHEN azurlane_assets.failed_count + 1 <= 1 THEN $7::integer * INTERVAL '1 day'
                        WHEN azurlane_assets.failed_count + 1 = 2 THEN $8::integer * INTERVAL '1 day'
                        ELSE $9::integer * INTERVAL '1 day'
                    END
                ),
                last_seen_at = NOW();",
            vec![
                text_param(asset.url.as_str()),
                text_param(asset.url.as_str()),
                text_param(asset.kind.as_str()),
                text_param(asset.fallback_url.as_str()),
                text_param(reason.chars().take(500).collect::<String>()),
                Param::from(first_retry_days),
                Param::from(first_retry_days),
                Param::from(retry_cooldown_days(2)),
                Param::from(retry_cooldown_days(3)),
            ],
        )
        .await
        .map(|_| ())
    }

    /// `_register_temp_blob(temp_blob)`: the body joins the blob store, or is dropped for an
    /// identical verified blob already there.
    async fn register_temp_blob(&self, temp_blob: TempBlob) -> Result<BlobRef, JobError> {
        let TempBlob {
            file,
            sha256,
            size,
            content_type,
            downloaded_url,
        } = temp_blob;
        let rows = self
            .execute(
                "SELECT blob_path, content_type FROM azurlane_blobs WHERE sha256 = $1 AND size = $2 LIMIT 1;",
                vec![text_param(sha256.as_str()), Param::from(size)],
            )
            .await?;
        for row in &rows {
            let blob_path: String = column(row, "blob_path")?;
            let stored_type: String = column(row, "content_type")?;
            let existing = self.resolve_blob_path(&blob_path);
            let verified = {
                let (existing, sha256) = (existing.clone(), sha256.clone());
                blocking(move || Ok(verify_file(&existing, &sha256, size))).await?
            };
            if verified {
                drop(file);
                self.execute(
                    "UPDATE azurlane_blobs SET last_verified_at = NOW() WHERE sha256 = $1 AND size = $2;",
                    vec![text_param(sha256.as_str()), Param::from(size)],
                )
                .await?;
                return Ok(BlobRef {
                    sha256,
                    size,
                    content_type: if content_type.is_empty() { stored_type } else { content_type },
                    path: existing,
                    downloaded_url,
                });
            }
        }
        let relative_path = format!("_blobs/sha256/{}/{sha256}", &sha256[..2.min(sha256.len())]);
        let blob_path = self.path(&relative_path);
        let already_there = {
            let (blob_path, sha256) = (blob_path.clone(), sha256.clone());
            blocking(move || {
                if let Some(parent) = blob_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| os_error(&error, parent))?;
                }
                Ok(blob_path.exists() && verify_file(&blob_path, &sha256, size))
            })
            .await?
        };
        if already_there {
            drop(file);
        } else {
            file.persist(&blob_path).await.map_err(|error| pyerr::download_error(&error))?;
        }
        self.execute(
            "INSERT INTO azurlane_blobs (sha256, size, content_type, blob_path, created_at, last_verified_at)
            VALUES ($1, $2, $3, $4, NOW(), NOW())
            ON CONFLICT (sha256, size) DO UPDATE SET
                content_type = COALESCE(NULLIF(excluded.content_type, ''), azurlane_blobs.content_type),
                blob_path = excluded.blob_path,
                last_verified_at = NOW();",
            vec![
                text_param(sha256.as_str()),
                Param::from(size),
                text_param(content_type.as_str()),
                text_param(relative_path.as_str()),
            ],
        )
        .await?;
        Ok(BlobRef {
            sha256,
            size,
            content_type,
            path: blob_path,
            downloaded_url,
        })
    }

    // ---------- downloads ----------

    /// `_download_candidate_to_temp(client, asset, url=...)`: one attempt at one URL.
    async fn download_candidate_to_temp(&self, asset: &Asset, url: &str) -> Result<TempBlob, AttemptError> {
        let tmp_dir = self.path("_tmp");
        {
            let tmp_dir = tmp_dir.clone();
            blocking(move || std::fs::create_dir_all(&tmp_dir).map_err(|error| os_error(&error, &tmp_dir)))
                .await
                .map_err(AttemptError::Raise)?;
        }
        let _permit = self
            .cdn_semaphore
            .acquire()
            .await
            .map_err(|error| AttemptError::Raise(JobError::new("RuntimeError", error.to_string())))?;
        self.wait_cdn().await;
        let mut response = self
            .clients
            .assets
            .send(Request::get(wire_url(url, self.plain_http)))
            .await
            .map_err(|error| AttemptError::Retry(pyerr::transport_error(&error)))?;
        let status = response.status();
        if status >= 400 {
            if LIMITED_STATUSES.contains(&status) {
                self.circuit_breaker.record_limited_failure(&self.sleeper).await;
            }
            return Err(AttemptError::Status {
                error: JobError::new("HTTPStatusError", format!("HTTP {status}")),
                status,
                retry_after: retry_after_delay_seconds(response.header("retry-after").as_deref(), (self.clock)()),
            });
        }
        let options = TempOptions {
            prefix: ".azurlane-download-",
            suffix: "",
            mode: 0o600,
        };
        // Hashed while written; Python's read-back check of the temporary file is not
        // repeated (D120).
        let file = download::to_temp_file(&mut response, &tmp_dir, &options)
            .await
            .map_err(|error| match &error {
                DownloadError::Transport(transport) => AttemptError::Retry(pyerr::transport_error(transport)),
                DownloadError::Io { .. } => AttemptError::Raise(pyerr::download_error(&error)),
            })?;
        let content_type = response.header("content-type").unwrap_or_default();
        let invalid = |reason: String| AttemptError::Retry(JobError::new("InvalidAssetResponseError", reason));
        let mut sha256 = file.sha256_hex().to_owned();
        let mut size = i64::try_from(file.len()).unwrap_or(i64::MAX);
        let mut prefix = read_prefix(file.temp_path().to_path_buf()).await.map_err(AttemptError::Raise)?;
        if protected::is_protected_url(url).map_err(|error| AttemptError::Raise(error.into_job_error()))? {
            if is_cdn_rejection_body(&content_type, &prefix) {
                self.circuit_breaker.record_limited_failure(&self.sleeper).await;
                return Err(invalid(CDN_REJECTION_REASON.to_owned()));
            }
            let opened = open_protected_file(file.temp_path().to_path_buf(), url.to_owned())
                .await
                .map_err(AttemptError::Raise)?
                .map_err(invalid)?;
            sha256 = sha256_hex(&opened);
            size = i64::try_from(opened.len()).unwrap_or(i64::MAX);
            prefix = opened[..opened.len().min(BODY_PREFIX_LIMIT)].to_vec();
        }
        let invalid_reason = validate_asset_response(&asset.kind, &content_type, &prefix, size);
        if !invalid_reason.is_empty() {
            if invalid_reason == CDN_REJECTION_REASON {
                self.circuit_breaker.record_limited_failure(&self.sleeper).await;
            }
            if invalid_reason == MESH_UNAVAILABLE_REASON {
                return Err(AttemptError::Unavailable(JobError::new("UnavailableAssetError", invalid_reason)));
            }
            return Err(invalid(invalid_reason.to_owned()));
        }
        self.circuit_breaker.record_success();
        Ok(TempBlob {
            file,
            sha256,
            size,
            content_type,
            downloaded_url: url.to_owned(),
        })
    }

    /// `_download_asset_to_temp(client, asset)`: each candidate URL in turn, with retries.
    async fn download_asset_to_temp(&self, asset: &Asset) -> Result<TempBlob, JobError> {
        let mut last_error: Option<JobError> = None;
        for url in download_candidates(asset).map_err(PyError::into_job_error)? {
            for attempt in 1..=MAX_RETRIES {
                let mut retry_after = None;
                match self.download_candidate_to_temp(asset, &url).await {
                    Ok(blob) => return Ok(blob),
                    Err(AttemptError::Raise(error)) => return Err(error),
                    // A definite answer, just not the asset: try the next candidate URL.
                    Err(AttemptError::Unavailable(error)) => {
                        last_error = Some(error);
                        break;
                    }
                    Err(AttemptError::Status {
                        error,
                        status,
                        retry_after: after,
                    }) => {
                        last_error = Some(error);
                        retry_after = after;
                        if attempt >= max_attempts_for_status(status) {
                            break;
                        }
                    }
                    Err(AttemptError::Retry(error)) => {
                        // A transport failure a later attempt recovers from leaves no other trace.
                        log().info(format_args!(
                            "Retrying Azur Lane asset after {} (attempt {attempt}): {url}",
                            error.class_name()
                        ));
                        last_error = Some(error);
                    }
                }
                if attempt < MAX_RETRIES {
                    (self.sleeper)(self.retry_delay(attempt, retry_after)).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| JobError::new("RuntimeError", format!("Failed to download {}", asset.url))))
    }

    /// The blob for `asset`: the archived one, else a fresh download (`status` says which).
    async fn obtain_blob(&self, asset: &mut Asset) -> Result<BlobRef, JobError> {
        if let Some(blob) = self.completed_blob_for_url(&asset.url).await? {
            "reused".clone_into(&mut asset.status);
            asset.downloaded_url = if blob.downloaded_url.is_empty() {
                asset.url.clone()
            } else {
                blob.downloaded_url.clone()
            };
            return Ok(blob);
        }
        let temp_blob = self.download_asset_to_temp(asset).await?;
        let downloaded_url = temp_blob.downloaded_url.clone();
        let blob = self.register_temp_blob(temp_blob).await?;
        "downloaded".clone_into(&mut asset.status);
        asset.downloaded_url = downloaded_url;
        Ok(blob)
    }

    /// The bookkeeping after an asset's blob is in place.
    async fn record_blob(&self, asset: &mut Asset, blob: &BlobRef) -> Result<(), JobError> {
        asset.sha256.clone_from(&blob.sha256);
        asset.size = blob.size;
        asset.content_type.clone_from(&blob.content_type);
        asset.error = String::new();
        let downloaded_url = [&asset.downloaded_url, &blob.downloaded_url, &asset.url]
            .into_iter()
            .find(|url| !url.is_empty())
            .cloned()
            .unwrap_or_default();
        self.mark_asset_downloaded(asset, blob, &downloaded_url).await
    }

    /// The part of `_process_asset` inside its `try`.
    async fn fetch_and_materialize(&self, root: &str, asset: &mut Asset) -> Result<(), JobError> {
        let blob = self.obtain_blob(asset).await?;
        {
            let destination = PathBuf::from(pypath::join(root, &asset.local_path));
            let (blob_path, sha256, size) = (blob.path.clone(), blob.sha256.clone(), blob.size);
            blocking(move || materialize_blob(&blob_path, &destination, &sha256, size)).await?;
        }
        self.record_blob(asset, &blob).await
    }

    /// `_process_asset(client=..., root=..., asset=...)`: whether the file is in place.
    async fn process_asset(&self, root: &str, asset: &mut Asset) -> Result<bool, JobError> {
        self.upsert_asset_seen(asset).await?;
        if let Some((last_error, next_retry_at)) = self.asset_retry_cooldown(asset).await? {
            "failed".clone_into(&mut asset.status);
            asset.error = if last_error.is_empty() {
                "retry cooldown active".to_owned()
            } else {
                last_error
            };
            log().info(format_args!(
                "Skipping Azur Lane asset during retry cooldown until {next_retry_at}: {}",
                asset.url
            ));
            return Ok(false);
        }
        match self.fetch_and_materialize(root, asset).await {
            Ok(()) => Ok(true),
            Err(error) => {
                let message = error_text(&error);
                "failed".clone_into(&mut asset.status);
                asset.error.clone_from(&message);
                log().warning(format_args!("Failed to download Azur Lane asset {}: {message}", asset.url));
                self.mark_asset_failed(asset, &message).await?;
                Ok(false)
            }
        }
    }

    /// `_process_assets(client=..., root=..., assets=...)`: each URL's assets in turn, up to
    /// the configured number of URLs at a time; the failed assets.
    async fn process_assets(&self, root: &str, assets: &mut [Asset]) -> Result<(), JobError> {
        use futures_util::stream::{self, StreamExt as _, TryStreamExt as _};

        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (index, asset) in assets.iter().enumerate() {
            match groups.iter_mut().find(|(url, _)| *url == asset.url) {
                Some((_, indices)) => indices.push(index),
                None => groups.push((asset.url.clone(), vec![index])),
            }
        }
        let jobs: Vec<Vec<(usize, Asset)>> = groups
            .into_iter()
            .map(|(_, indices)| indices.into_iter().map(|index| (index, assets[index].clone())).collect())
            .collect();
        let processed: Vec<Vec<(usize, Asset)>> = stream::iter(jobs)
            .map(|mut group| async move {
                for (_, asset) in &mut group {
                    self.process_asset(root, asset).await?;
                }
                Ok::<_, JobError>(group)
            })
            .buffer_unordered(self.pacing.asset_process_concurrency.max(1))
            .try_collect()
            .await?;
        for (index, asset) in processed.into_iter().flatten() {
            assets[index] = asset;
        }
        Ok(())
    }

    /// `_seed_asset_text(client=..., asset=...)`: the file a model's enumeration starts from.
    async fn seed_asset_text(&self, asset: &mut Asset) -> Result<String, JobError> {
        self.upsert_asset_seen(asset).await?;
        let blob = self.obtain_blob(asset).await?;
        self.record_blob(asset, &blob).await?;
        let path = blob.path.clone();
        blocking(move || {
            std::fs::read(&path)
                .map(|bytes| decode_utf8_ignore(&bytes))
                .map_err(|error| os_error(&error, &path))
        })
        .await
    }

    /// A failed seed asset's bookkeeping.
    async fn mark_seed_failed(&self, asset: &mut Asset, error: &JobError) -> Result<(), JobError> {
        let message = error_text(error);
        "failed".clone_into(&mut asset.status);
        asset.error.clone_from(&message);
        self.mark_asset_failed(asset, &message).await
    }

    /// `_live2d_enumeration(client=..., entry=...)`.
    async fn live2d_enumeration(&self, entry: &ModelEntry) -> Result<AzurLaneModelResourceEnumeration, JobError> {
        let mut seed = model3_seed_asset(entry).map_err(PyError::into_job_error)?;
        let result = async {
            let model3_source = self.seed_asset_text(&mut seed).await?;
            enumerate_azurlane_model_resources(
                entry,
                EnumerationInputs {
                    model3_source: Some(&model3_source),
                    ..EnumerationInputs::default()
                },
            )
            .map_err(PyError::into_job_error)
        }
        .await;
        if let Err(error) = &result {
            self.mark_seed_failed(&mut seed, error).await?;
        }
        result
    }

    /// `_asset_url_exists(client=..., url=...)`: a HEAD, and a GET where HEAD is refused
    /// (whose body is not read, D121).
    async fn asset_url_exists(&self, url: &str) -> bool {
        for method in [Method::HEAD, Method::GET] {
            self.wait_cdn().await;
            let Ok(response) = self.clients.assets.send(Request::new(method, wire_url(url, self.plain_http))).await else {
                return false;
            };
            let status = response.status();
            if status < 400 {
                return true;
            }
            if !METHOD_UNSUPPORTED.contains(&status) {
                return false;
            }
        }
        false
    }

    /// `_known_model_paths()`: archived model paths by (costume id, model type), moved to the
    /// CDN's current root.
    async fn known_model_paths(&self) -> Result<HashMap<(i128, String), String>, JobError> {
        let base_urls: Vec<&str> = std::iter::once(L2D_SU_ASSET_BASE_URL)
            .chain(L2D_SU_ARCHIVED_ASSET_BASE_URLS)
            .collect();
        let url_filter: Vec<String> = (0..base_urls.len())
            .map(|index| format!("primary_url LIKE ${}", index + 1))
            .collect();
        let rows = self
            .execute(
                &format!(
                    "SELECT costume_id, model_type, primary_url
                    FROM azurlane_models
                    WHERE costume_id IS NOT NULL AND completed_at IS NOT NULL AND model_type <> 'painting'
                        AND ({});",
                    url_filter.join(" OR ")
                ),
                base_urls.iter().map(|base_url| text_param(format!("{base_url}/%"))).collect(),
            )
            .await?;
        let mut paths = HashMap::new();
        for row in &rows {
            let costume_id: Option<i64> = column(row, "costume_id")?;
            let model_type = row_text(row, "model_type")?;
            let primary_url = row_text(row, "primary_url")?;
            if let Some(costume_id) = costume_id
                && !model_type.is_empty()
                && !primary_url.is_empty()
            {
                paths.insert((i128::from(costume_id), model_type), l2d_su_asset_url_from_legacy(&primary_url));
            }
        }
        Ok(paths)
    }

    // ---------- ship details ----------

    /// `_read_ship_detail(client=..., url=..., attempt=...)`: one origin attempt; a failure is
    /// usually a blocked proxy exit, so the caller retries.
    async fn read_ship_detail(&self, url: &str, attempt: u32) -> Option<String> {
        let wire = Wire {
            http: self.origin_client(),
            plain_http: self.plain_http,
        };
        let result = async {
            let response = wire.request("GET", url, None).await.map_err(|error| error.message)?;
            if let Some(message) = status_error_message(response.status, &response.url, None) {
                return Err(message);
            }
            let payload = protected_response_text(&response, url).map_err(|error| error.message)?;
            // Reject anti-bot pages before they poison the jsonb column.
            match pyjson::loads(&payload) {
                Ok(Value::Dict(_)) => Ok(payload),
                Ok(_) => Err("ship detail payload is not a JSON object".to_owned()),
                Err(error) => Err(error.python_message(&payload)),
            }
        }
        .await;
        match result {
            Ok(payload) => Some(payload),
            Err(message) => {
                log().warning(format_args!(
                    "Failed to fetch Azur Lane ship detail from {url} (attempt {attempt}): {message}"
                ));
                None
            }
        }
    }

    /// `_fetch_ship_detail_from_origin(client=..., ship_id=...)`.
    async fn fetch_ship_detail_from_origin(&self, ship_id: i128) -> Result<Option<String>, JobError> {
        if let Some(payload) = self.state().ship_detail_payloads.get(&ship_id) {
            return Ok(payload.clone());
        }
        let url = l2d_su_ship_detail_url(ship_id, L2D_SU_PRIMARY_REGION);
        let mut payload = None;
        for attempt in 1..=self.pacing.origin_attempts {
            self.state().origin_detail_spent += 1;
            self.wait_origin().await;
            payload = self.read_ship_detail(&url, attempt).await;
            if payload.is_some() {
                break;
            }
        }
        self.state().ship_detail_payloads.insert(ship_id, payload.clone());
        if let Some(payload) = &payload {
            self.store_ship_detail(ship_id, payload).await?;
        }
        Ok(payload)
    }

    /// `_store_ship_detail(ship_id=..., payload=...)`.
    async fn store_ship_detail(&self, ship_id: i128, payload: &str) -> Result<(), JobError> {
        let fingerprint = self.state().character_fingerprints.get(&ship_id).cloned().unwrap_or_default();
        self.execute(
            "INSERT INTO azurlane_ship_details (ship_group_id, region, fingerprint, payload, fetched_at)
            VALUES ($1, $2, $3, $4::jsonb, NOW())
            ON CONFLICT (ship_group_id) DO UPDATE SET
                region = excluded.region,
                fingerprint = excluded.fingerprint,
                payload = excluded.payload,
                fetched_at = NOW();",
            vec![
                Param::from(bigint(ship_id)?),
                text_param(L2D_SU_PRIMARY_REGION),
                text_param(fingerprint),
                text_param(payload),
            ],
        )
        .await
        .map(|_| ())
    }

    /// `_ship_detail_source(client=..., ship_id=...)`: the stored detail, else the origin's.
    async fn ship_detail_source(&self, ship_id: i128) -> Result<Option<String>, JobError> {
        if let Some(payload) = self.state().ship_detail_payloads.get(&ship_id) {
            return Ok(payload.clone());
        }
        let rows = self
            .execute(
                "SELECT payload::text AS payload FROM azurlane_ship_details WHERE ship_group_id = $1;",
                vec![Param::from(bigint(ship_id)?)],
            )
            .await?;
        let stored = match rows.first() {
            Some(row) => row_json_object(row, "payload")?,
            None => Dict::new(),
        };
        if !stored.is_empty() {
            let payload = json_dumps(&Value::Dict(stored))?;
            self.state().ship_detail_payloads.insert(ship_id, Some(payload.clone()));
            return Ok(Some(payload));
        }
        self.fetch_ship_detail_from_origin(ship_id).await
    }

    /// `_sync_ship_details(snapshots, client=...)`: fetch the detail of every ship whose
    /// index entry changed since its detail was stored.
    async fn sync_ship_details(&self, snapshots: &AzurLaneSourceSnapshots) -> Result<(), JobError> {
        use futures_util::TryStreamExt as _;

        let mut rows = sqlx::query("SELECT ship_group_id, fingerprint FROM azurlane_ship_details;").fetch(self.db.pool());
        let mut stored: HashMap<i128, String> = HashMap::new();
        while let Some(row) = rows.try_next().await.map_err(|error| db_job_error(&fav_db::DbError::Sqlx(error)))? {
            let ship_id: i64 = column(&row, "ship_group_id")?;
            stored.insert(i128::from(ship_id), row_text(&row, "fingerprint")?);
        }
        drop(rows);
        let mut stale = Vec::new();
        {
            let mut run = self.state();
            run.character_fingerprints.clear();
            for character in &snapshots.l2d_su.characters {
                let fingerprint = l2d_su_character_fingerprint(character);
                if stored.get(&character.char_id) != Some(&fingerprint) && !stale.contains(&character.char_id) {
                    stale.push(character.char_id);
                }
                run.character_fingerprints.insert(character.char_id, fingerprint);
            }
            // A ship listed twice is stale or not by its last listing, as Python's dict has it.
            stale.retain(|ship_id| stored.get(ship_id) != run.character_fingerprints.get(ship_id));
        }
        let mut fetched = 0;
        for ship_id in &stale {
            if self.fetch_ship_detail_from_origin(*ship_id).await?.is_some() {
                fetched += 1;
            }
        }
        log().info(format_args!(
            "Azur Lane ship details: {} stored, {} stale, {fetched} fetched, {} origin requests spent",
            stored.len(),
            stale.len(),
            self.state().origin_detail_spent
        ));
        Ok(())
    }

    /// `_voice_lines_for(client=..., entry=...)`.
    async fn voice_lines_for(&self, entry: &ModelEntry) -> Result<Vec<L2DSuVoiceLine>, JobError> {
        let (Some(ship_id), Some(costume_id)) = (entry.character.id, entry.costume.id) else {
            return Ok(Vec::new());
        };
        let cached = self.state().ship_voices.get(&ship_id).cloned();
        let voices = if let Some(voices) = cached {
            voices
        } else {
            let mut voices = Vec::new();
            if let Some(payload) = self.ship_detail_source(ship_id).await? {
                match parse_l2d_su_ship_voices(&payload) {
                    Ok(parsed) => voices = parsed,
                    Err(error) if is_parse_failure(&error) => {
                        log().warning(format_args!(
                            "Failed to parse Azur Lane voices for ship {ship_id}: {}",
                            error.message
                        ));
                    }
                    Err(error) => return Err(error.into_job_error()),
                }
            }
            self.state().ship_voices.insert(ship_id, voices.clone());
            voices
        };
        Ok(voices
            .into_iter()
            .find(|(id, _)| *id == costume_id)
            .map(|(_, lines)| lines)
            .unwrap_or_default())
    }

    /// `_ship_detail_model_paths(client=..., ship_id=...)`.
    async fn ship_detail_model_paths(&self, ship_id: i128) -> Result<HashMap<i128, String>, JobError> {
        if let Some(cached) = self.state().ship_model_paths.get(&ship_id) {
            return Ok(cached.clone());
        }
        let mut models = HashMap::new();
        if let Some(payload) = self.ship_detail_source(ship_id).await? {
            match parse_l2d_su_ship_models(&payload) {
                Ok(parsed) => models = parsed.into_iter().collect(),
                Err(error) if is_parse_failure(&error) => {
                    log().warning(format_args!(
                        "Failed to parse Azur Lane model paths for ship {ship_id}: {}",
                        error.message
                    ));
                }
                Err(error) => return Err(error.into_job_error()),
            }
        }
        self.state().ship_model_paths.insert(ship_id, models.clone());
        Ok(models)
    }

    /// `_stored_ship_classes()`: ship class names extracted by the server from stored details.
    async fn stored_ship_classes(&self) -> Result<HashMap<i128, String>, JobError> {
        use futures_util::TryStreamExt as _;

        let mut rows = sqlx::query("SELECT ship_group_id, payload->'ship'->>'className' AS class_name FROM azurlane_ship_details;")
            .fetch(self.db.pool());
        let mut classes = HashMap::new();
        while let Some(row) = rows.try_next().await.map_err(|error| db_job_error(&fav_db::DbError::Sqlx(error)))? {
            let ship_id: i64 = column(&row, "ship_group_id")?;
            let class_name = row_text(&row, "class_name")?;
            if !class_name.is_empty() {
                classes.insert(i128::from(ship_id), class_name);
            }
        }
        Ok(classes)
    }

    /// `_model_assets_exist(client=..., entry=...)`.
    async fn model_assets_exist(&self, entry: &ModelEntry) -> Result<bool, JobError> {
        for url in model_probe_urls(entry).map_err(PyError::into_job_error)? {
            if self.asset_url_exists(&url).await {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `_resolve_model_paths(catalog, client=...)`: an archived path where the archive has
    /// one, else the derived path if the CDN has it, else the path the ship detail names.
    async fn resolve_model_paths(&self, catalog: &AzurLaneModelCatalog) -> Result<AzurLaneModelCatalog, JobError> {
        let known = self.known_model_paths().await?;
        let mut resolved: HashMap<i128, String> = HashMap::new();
        let (mut reused, mut probed, mut corrections) = (0, 0, 0);
        for entry in &catalog.entries {
            let (Some(costume_id), Some(ship_id)) = (entry.costume.id, entry.character.id) else {
                continue;
            };
            if entry.r#type == ModelKind::Painting
                || entry.source == super::l2d_sources::CatalogSource::Nagami
                || is_hashed_asset_url(&entry.resources.primary_url)
            {
                continue;
            }
            if let Some(stored) = known.get(&(costume_id, entry.r#type.as_str().to_owned())) {
                resolved.insert(costume_id, stored.clone());
                reused += 1;
                continue;
            }
            // A skin without a path has nothing to probe; only the ship detail can name it.
            if !entry.resources.primary_url.is_empty() {
                probed += 1;
                if self.model_assets_exist(entry).await? {
                    continue;
                }
            }
            let models = self.ship_detail_model_paths(ship_id).await?;
            if let Some(authoritative) = models.get(&costume_id).filter(|path| !path.is_empty())
                && *authoritative != entry.resources.primary_url
            {
                log().info(format_args!("Resolved Azur Lane model {} to {authoritative}", entry.id));
                resolved.insert(costume_id, authoritative.clone());
                corrections += 1;
            }
        }
        log().info(format_args!(
            "Azur Lane model paths: {reused} reused, {probed} probed, {corrections} corrected, {} origin detail requests",
            self.state().origin_detail_spent
        ));
        Ok(apply_l2d_su_model_paths(catalog, &resolved))
    }

    // ---------- model enumeration ----------

    /// `_spine_is_single_part(client=..., entry=...)`: whether a spine model has only
    /// `<key>.skel` and `<key>.atlas`, answered from the archive where it can be.
    async fn spine_is_single_part(&self, entry: &ModelEntry) -> Result<bool, JobError> {
        let url = &entry.resources.primary_url;
        if is_hashed_asset_url(url) {
            return Ok(false);
        }
        let parts_url = spine_parts_manifest_url(url).map_err(PyError::into_job_error)?;
        if self.completed_blob_for_url(&parts_url).await?.is_some() {
            return Ok(false);
        }
        let manifest = spine_resource_manifest(url, None).map_err(PyError::into_job_error)?;
        let Some(part) = manifest.parts.first() else {
            return Err(JobError::new("IndexError", "tuple index out of range"));
        };
        let skeleton_url = part.skeleton_url.clone();
        if self.completed_blob_for_url(&skeleton_url).await?.is_some() {
            return Ok(true);
        }
        Ok(self.asset_url_exists(&skeleton_url).await)
    }

    /// `_spine_parts_source(client=..., entry=...)`: the parts manifest, `None` for a
    /// single-part model or one whose manifest cannot be had.
    async fn spine_parts_source(&self, entry: &ModelEntry) -> Result<Option<String>, JobError> {
        if self.spine_is_single_part(entry).await? {
            return Ok(None);
        }
        let mut seed = spine_parts_seed_asset(entry).map_err(PyError::into_job_error)?;
        match self.seed_asset_text(&mut seed).await {
            Ok(text) => Ok(Some(text)),
            Err(error) => {
                self.mark_seed_failed(&mut seed, &error).await?;
                Ok(None)
            }
        }
    }

    /// `_spine_enumeration(client=..., root=..., entry=...)`: the textures are named in the
    /// atlases, so those are archived first.
    async fn spine_enumeration(&self, root: &str, entry: &ModelEntry) -> Result<AzurLaneModelResourceEnumeration, JobError> {
        let parts_source = self.spine_parts_source(entry).await?;
        let seed_enumeration = enumerate_azurlane_model_resources(
            entry,
            EnumerationInputs {
                spine_parts_source: parts_source.as_deref(),
                ..EnumerationInputs::default()
            },
        )
        .map_err(PyError::into_job_error)?;
        let mut atlas_assets: Vec<Asset> = assets_from_enumeration(&seed_enumeration)
            .into_iter()
            .filter(|asset| asset.kind == "spine.atlas")
            .collect();
        if atlas_assets.is_empty() {
            return Err(JobError::new(
                SOURCE_SCHEMA_ERROR,
                format!("Spine entry {} did not enumerate an atlas asset", entry.id),
            ));
        }
        let mut atlas_sources: HashMap<String, String> = HashMap::new();
        for asset in &mut atlas_assets {
            if self.process_asset(root, asset).await? {
                let path = PathBuf::from(pypath::join(root, &asset.local_path));
                let text = blocking(move || {
                    std::fs::read(&path)
                        .map(|bytes| decode_utf8_ignore(&bytes))
                        .map_err(|error| os_error(&error, &path))
                })
                .await?;
                atlas_sources.insert(asset.url.clone(), text);
            }
        }
        if atlas_sources.is_empty() {
            return Ok(seed_enumeration);
        }
        enumerate_azurlane_model_resources(
            entry,
            EnumerationInputs {
                spine_parts_source: parts_source.as_deref(),
                atlas_sources: Some(&atlas_sources),
                ..EnumerationInputs::default()
            },
        )
        .map_err(PyError::into_job_error)
    }

    /// `_painting_index(client=..., entry=...)`: the layer index, `None` when the CDN has
    /// none. Losing it costs the reassembled artwork, not the model.
    async fn painting_index(&self, entry: &ModelEntry) -> Result<Option<PaintingIndex>, JobError> {
        if entry_painting_index_url(entry).map_err(PyError::into_job_error)?.is_empty() {
            return Ok(None);
        }
        let mut seed = painting_index_seed_asset(entry).map_err(PyError::into_job_error)?;
        let result = async {
            let source = self.seed_asset_text(&mut seed).await?;
            parse_painting_index(&source, &entry.resources.primary_url, &entry.resources.painting_index_url)
                .map_err(PyError::into_job_error)
        }
        .await;
        match result {
            Ok(index) => Ok(Some(index)),
            Err(error) => {
                log().info(format_args!("No Azur Lane painting index for {}: {}", entry.id, error_text(&error)));
                self.mark_seed_failed(&mut seed, &error).await?;
                Ok(None)
            }
        }
    }

    /// `_model_enumeration(client=..., root=..., entry=...)`.
    async fn model_enumeration(
        &self,
        root: &str,
        entry: &ModelEntry,
    ) -> Result<(AzurLaneModelResourceEnumeration, Option<PaintingIndex>), JobError> {
        match entry.r#type {
            ModelKind::Live2d => Ok((self.live2d_enumeration(entry).await?, None)),
            ModelKind::Painting => {
                let voice_lines = self.voice_lines_for(entry).await?;
                let painting_index = self.painting_index(entry).await?;
                let enumeration = enumerate_azurlane_model_resources(
                    entry,
                    EnumerationInputs {
                        voice_lines: &voice_lines,
                        painting_index: painting_index.as_ref(),
                        ..EnumerationInputs::default()
                    },
                )
                .map_err(PyError::into_job_error)?;
                Ok((enumeration, painting_index))
            }
            ModelKind::Spine => Ok((self.spine_enumeration(root, entry).await?, None)),
        }
    }

    /// `_store_painting_index(model_id=..., painting_index=...)`: the layer geometry on the
    /// model, where the front end reads it.
    async fn store_painting_index(&self, model_id: &str, painting_index: &PaintingIndex) -> Result<(), JobError> {
        let mut patch = Dict::new();
        patch.insert("painting_index", painting_index.to_dict());
        self.execute(
            "UPDATE azurlane_models SET source_metadata = source_metadata || $1::jsonb, last_seen_at = NOW() WHERE model_id = $2;",
            vec![text_param(json_dumps(&Value::Dict(patch))?), text_param(model_id)],
        )
        .await
        .map(|_| ())
    }

    /// `_replace_model_assets(model_id=..., assets=..., completed=...)`.
    async fn replace_model_assets(&self, model_id: &str, assets: &[Asset], completed: bool) -> Result<(), JobError> {
        let mut statements = vec![Statement::new(
            "DELETE FROM azurlane_model_assets WHERE model_id = $1;",
            vec![text_param(model_id)],
        )];
        let empty = [Dict::new()];
        for asset in assets {
            let contexts = if asset.contexts.is_empty() {
                &empty[..]
            } else {
                &asset.contexts[..]
            };
            for context in contexts {
                let context_json = json_dumps(&Value::Dict(context.clone()))?;
                statements.push(Statement::new(
                    "INSERT INTO azurlane_model_assets (
                        model_id, url, kind, context_hash, local_path, original_filename,
                        fallback_url, context_json, created_at, updated_at
                    )
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, NOW(), NOW())
                    ON CONFLICT (model_id, url, kind, context_hash) DO UPDATE SET
                        local_path = excluded.local_path,
                        original_filename = excluded.original_filename,
                        fallback_url = excluded.fallback_url,
                        context_json = excluded.context_json,
                        updated_at = NOW();",
                    vec![
                        text_param(model_id),
                        text_param(asset.url.as_str()),
                        text_param(asset.kind.as_str()),
                        text_param(sha256_hex(context_json.as_bytes())),
                        text_param(asset.local_path.as_str()),
                        text_param(asset.original_filename.as_str()),
                        text_param(asset.fallback_url.as_str()),
                        text_param(context_json),
                    ],
                ));
            }
        }
        if completed {
            statements.push(Statement::new(
                "UPDATE azurlane_models SET completed_at = NOW(), last_seen_at = NOW() WHERE model_id = $1;",
                vec![text_param(model_id)],
            ));
        }
        self.transaction(statements).await
    }

    /// `_archive_model(client=..., entry=...)`.
    async fn archive_model(&self, entry: &ModelEntry) -> Result<(), JobError> {
        if entry.resources.primary_url.is_empty() {
            return Err(JobError::new(
                "AssetProcessingError",
                format!("No model path for Azur Lane model {}: its ship detail did not list one", entry.id),
            ));
        }
        let root = character_root(&self.root, entry);
        self.mark_model_fetch_started(entry).await?;
        let (enumeration, painting_index) = self.model_enumeration(&root, entry).await.map_err(|error| {
            JobError::new(
                "AssetProcessingError",
                format!("Failed to enumerate Azur Lane model {}: {}", entry.id, error.message()),
            )
        })?;
        if let Some(painting_index) = &painting_index {
            self.store_painting_index(&entry.id, painting_index).await?;
        }
        let mut assets = assets_from_enumeration(&enumeration);
        self.process_assets(&root, &mut assets).await?;
        let failed: Vec<&Asset> = assets.iter().filter(|asset| asset.status == "failed").collect();
        let blocking_failed: Vec<&&Asset> = failed.iter().filter(|asset| asset.is_required()).collect();
        self.replace_model_assets(&entry.id, &assets, blocking_failed.is_empty()).await?;
        let optional: Vec<&&Asset> = failed.iter().filter(|asset| !asset.is_required()).collect();
        if !optional.is_empty() {
            // The source advertises companion files it does not host; losing one costs a
            // subtitle or an icon, and the model still renders.
            let examples: Vec<String> = optional
                .iter()
                .take(3)
                .map(|asset| format!("{} {}", asset.kind, asset.url))
                .collect();
            log().info(format_args!(
                "Azur Lane model {} is missing {} optional asset(s): {}",
                entry.id,
                optional.len(),
                examples.join(", ")
            ));
        }
        if !blocking_failed.is_empty() {
            let examples: Vec<&str> = blocking_failed.iter().take(3).map(|asset| asset.url.as_str()).collect();
            let mut message = format!("{} Azur Lane assets failed", blocking_failed.len());
            if !examples.is_empty() {
                message = format!("{message}: {}", examples.join(", "));
            }
            return Err(JobError::new("AssetProcessingError", message));
        }
        self.mark_character_completed(entry).await
    }

    /// `download_models(entries, client=...)`: every entry not resting out a cooldown.
    pub async fn download_models(&self, entries: &[ModelEntry]) -> Result<ModelCrawlOutcome, JobError> {
        let retry_state = self.model_retry_state().await?;
        let mut outcome = ModelCrawlOutcome::default();
        for entry in entries {
            if retry_state.cooldown_until.contains_key(&entry.id) {
                outcome.cooldown_model_ids.push(entry.id.clone());
                continue;
            }
            match self.archive_model(entry).await {
                Ok(()) => {
                    // Only a model carrying failures has anything to clear.
                    if retry_state.failed_model_ids.contains(&entry.id) {
                        self.clear_model_retry_state(entry).await?;
                    }
                }
                Err(error) => {
                    log().error(format_args!(
                        "Failed to crawl Azur Lane model_id={}\n{}",
                        entry.id,
                        error.formatted()
                    ));
                    self.mark_model_retry_failed(entry, &error_text(&error)).await?;
                    outcome.failed_model_ids.push(entry.id.clone());
                }
            }
        }
        if !outcome.cooldown_model_ids.is_empty() {
            let next_due = outcome
                .cooldown_model_ids
                .iter()
                .filter_map(|model_id| retry_state.cooldown_until.get(model_id))
                .min()
                .cloned()
                .unwrap_or_default();
            let examples: Vec<&str> = outcome.cooldown_model_ids.iter().take(5).map(String::as_str).collect();
            log().info(format_args!(
                "Skipped {} Azur Lane model(s) resting out a retry cooldown, next due {next_due}: {}",
                outcome.cooldown_model_ids.len(),
                examples.join(", ")
            ));
        }
        Ok(outcome)
    }

    // ---------- manifests ----------

    /// The next batch of active characters after `after`, in code point order.
    async fn manifest_character_batch(&self, after: Option<&str>) -> Result<Vec<CharacterRow>, JobError> {
        let rows = self
            .execute(
                "SELECT
                    character_key, source_id, name_zh, name_en, manifest_path, active,
                    source_metadata::text AS source_metadata, fetched_at, completed_at
                FROM azurlane_characters
                WHERE active = TRUE AND ($1::text IS NULL OR character_key COLLATE \"C\" > $1::text COLLATE \"C\")
                ORDER BY character_key COLLATE \"C\"
                LIMIT $2;",
                vec![Param::Text(after.map(str::to_owned)), Param::from(self.manifest_batch)],
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(CharacterRow {
                    raw_key: column::<Option<String>>(row, "character_key")?.unwrap_or_default(),
                    character_key: row_text(row, "character_key")?,
                    source_id: column(row, "source_id")?,
                    name_zh: row_text(row, "name_zh")?,
                    name_en: row_text(row, "name_en")?,
                    manifest_path: row_text(row, "manifest_path")?,
                    active: column::<Option<bool>>(row, "active")?.unwrap_or(true),
                    source_metadata: row_json_object(row, "source_metadata")?,
                    fetched_at: row_timestamp(row, "fetched_at")?,
                    completed_at: row_timestamp(row, "completed_at")?,
                })
            })
            .collect()
    }

    /// The active models of `character_keys` (stored keys, unstripped; the caller matches
    /// the rows to characters by their stripped keys, as Python does).
    async fn manifest_model_rows(&self, character_keys: &[String]) -> Result<Vec<ModelRow>, JobError> {
        let keys = Value::List(character_keys.iter().cloned().map(Value::Str).collect());
        let rows = self
            .execute(
                "SELECT
                    model_id, model_type, source, character_key, costume_key, costume_id,
                    costume_name_zh, costume_name_en, primary_url, fallback_url, display_info_url,
                    availability_state, source_metadata::text AS source_metadata, fetched_at, completed_at
                FROM azurlane_models
                WHERE active = TRUE AND character_key IN (SELECT jsonb_array_elements_text($1::jsonb))
                ORDER BY character_key, model_type, costume_key, model_id;",
                vec![text_param(json_dumps(&keys)?)],
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(ModelRow {
                    model_id: row_text(row, "model_id")?,
                    model_type: row_text(row, "model_type")?,
                    source: row_text(row, "source")?,
                    character_key: row_text(row, "character_key")?,
                    costume_key: row_text(row, "costume_key")?,
                    costume_id: column(row, "costume_id")?,
                    costume_name_zh: row_text(row, "costume_name_zh")?,
                    costume_name_en: row_text(row, "costume_name_en")?,
                    primary_url: row_text(row, "primary_url")?,
                    fallback_url: row_text(row, "fallback_url")?,
                    display_info_url: row_text(row, "display_info_url")?,
                    availability_state: row_text(row, "availability_state")?,
                    source_metadata: row_json_object(row, "source_metadata")?,
                    fetched_at: row_timestamp(row, "fetched_at")?,
                    completed_at: row_timestamp(row, "completed_at")?,
                })
            })
            .collect()
    }

    /// `_manifest_asset_rows(model_ids)`.
    async fn manifest_asset_rows(&self, model_ids: &[String]) -> Result<Vec<AssetRow>, JobError> {
        #[cfg(test)]
        self.manifest_asset_queries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if model_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = Value::List(model_ids.iter().cloned().map(Value::Str).collect());
        let rows = self
            .execute(
                "SELECT
                    ma.model_id, ma.url, ma.kind, ma.context_hash, ma.local_path, ma.original_filename,
                    ma.fallback_url AS relation_fallback_url, ma.context_json::text AS context_json,
                    a.normalized_url, a.downloaded_url, a.fallback_url AS asset_fallback_url, a.sha256, a.size,
                    a.content_type, a.status, a.failed_count, a.last_error, a.last_attempt_at, a.next_retry_at, a.last_seen_at
                FROM azurlane_model_assets AS ma
                JOIN azurlane_assets AS a ON a.url = ma.url
                WHERE ma.model_id IN (SELECT jsonb_array_elements_text($1::jsonb))
                ORDER BY ma.model_id, ma.kind, ma.local_path, ma.url, ma.context_hash;",
                vec![text_param(json_dumps(&ids)?)],
            )
            .await?;
        rows.iter()
            .map(|row| {
                let failed_count: i32 = column(row, "failed_count")?;
                Ok(AssetRow {
                    model_id: row_text(row, "model_id")?,
                    url: row_text(row, "url")?,
                    kind: row_text(row, "kind")?,
                    context_hash: row_text(row, "context_hash")?,
                    local_path: row_text(row, "local_path")?,
                    original_filename: row_text(row, "original_filename")?,
                    relation_fallback_url: row_text(row, "relation_fallback_url")?,
                    context_json: row_json_object(row, "context_json")?,
                    normalized_url: row_text(row, "normalized_url")?,
                    downloaded_url: row_text(row, "downloaded_url")?,
                    asset_fallback_url: row_text(row, "asset_fallback_url")?,
                    sha256: row_text(row, "sha256")?,
                    size: column(row, "size")?,
                    content_type: row_text(row, "content_type")?,
                    status: row_text(row, "status")?,
                    failed_count: i64::from(failed_count),
                    last_error: row_text(row, "last_error")?,
                    last_attempt_at: row_timestamp(row, "last_attempt_at")?,
                    next_retry_at: row_timestamp(row, "next_retry_at")?,
                    last_seen_at: row_timestamp(row, "last_seen_at")?,
                })
            })
            .collect()
    }

    /// `_write_character_detail(root, character_row)`: the stored ship detail as `detail.json`.
    async fn character_detail(&self, row: &CharacterRow) -> Result<Option<Dict>, JobError> {
        let Some(source_id) = row.source_id else { return Ok(None) };
        let rows = self
            .execute(
                "SELECT payload::text AS payload, fetched_at FROM azurlane_ship_details WHERE ship_group_id = $1;",
                vec![Param::from(source_id)],
            )
            .await?;
        let Some(row) = rows.first() else { return Ok(None) };
        let payload = row_json_object(row, "payload")?;
        Ok((!payload.is_empty()).then_some(payload))
    }

    /// `_write_backend_manifests()`: `manifest.json`, `character.json` and `detail.json` for
    /// every active character, one batch of characters at a time.
    async fn write_backend_manifests(&self) -> Result<(), JobError> {
        let mut after: Option<String> = None;
        loop {
            let batch = self.manifest_character_batch(after.as_deref()).await?;
            let Some(last) = batch.last() else { break };
            after = Some(last.raw_key.clone());
            let character_keys: Vec<String> = batch.iter().map(|row| row.raw_key.clone()).collect();
            let model_rows = self.manifest_model_rows(&character_keys).await?;
            let model_ids: Vec<String> = model_rows.iter().map(|row| row.model_id.clone()).collect();
            let asset_rows = self.manifest_asset_rows(&model_ids).await?;
            let mut assets_by_model: HashMap<&str, Vec<&AssetRow>> = HashMap::new();
            for row in &asset_rows {
                assets_by_model.entry(row.model_id.as_str()).or_default().push(row);
            }
            for row in &batch {
                let root = character_manifest_root(&self.root, row);
                let mut models = Vec::new();
                for model_row in model_rows.iter().filter(|model| model.character_key == row.character_key) {
                    let assets = model_assets(assets_by_model.get(model_row.model_id.as_str()).cloned().unwrap_or_default())?;
                    models.push(model_payload(model_row, &assets));
                }
                models.sort_by_key(model_sort_key);
                let manifest = character_payload(row, models, &root);
                let summary = character_summary_payload(&manifest);
                let detail = self.character_detail(row).await?;
                blocking(move || {
                    write_json(&PathBuf::from(pypath::join(&root, "manifest.json")), &manifest)?;
                    write_json(&PathBuf::from(pypath::join(&root, "character.json")), &summary)?;
                    if let Some(detail) = detail {
                        write_json(&PathBuf::from(pypath::join(&root, "detail.json")), &Value::Dict(detail))?;
                    }
                    Ok(())
                })
                .await?;
            }
            if batch.len() < usize::try_from(self.manifest_batch).unwrap_or(usize::MAX) {
                break;
            }
        }
        Ok(())
    }

    // ---------- the run ----------

    async fn ensure_root(&self) -> Result<(), JobError> {
        let root = PathBuf::from(&self.root);
        blocking(move || std::fs::create_dir_all(&root).map_err(|error| os_error(&error, &root))).await
    }

    /// `update()`.
    pub async fn update(&self) -> Result<(), JobError> {
        let Some(lock) = self.db.try_advisory_lock("azurlane").await.map_err(|error| db_job_error(&error))? else {
            log().info("Azur Lane update skipped because another run holds the advisory lock");
            return Ok(());
        };
        let result = self.update_locked().await;
        lock.release().await;
        result
    }

    async fn update_locked(&self) -> Result<(), JobError> {
        self.ensure_root().await?;
        let snapshots = self.fetch_source_snapshots().await?;
        let catalog = build_azurlane_model_catalog(&snapshots).map_err(PyError::into_job_error)?;
        self.write_source_artifacts(&snapshots, &catalog).await?;
        if catalog.entries.is_empty() || !source_snapshots_complete(&snapshots) {
            // The existing catalog is left untouched, but the run must not report success.
            return Err(JobError::new("CrawlRunError", source_failure_message(&snapshots, &catalog)));
        }
        let archived = self.archived_models().await?;
        let catalog = pin_archived_entries(&catalog, &archived);
        check_archive_continuity(&catalog, &archived)?;
        drop(archived);
        *self.state() = RunState::default();
        self.sync_ship_details(&snapshots).await?;
        let catalog = apply_l2d_su_ship_classes(&catalog, &self.stored_ship_classes().await?);
        let catalog = self.resolve_model_paths(&catalog).await?;
        self.write_source_artifacts(&snapshots, &catalog).await?;
        self.upsert_catalog_state(&catalog).await?;
        log().info(format_args!("Found {} Azur Lane model entries", catalog.entries.len()));
        let outcome = self.download_models(&catalog.entries).await?;
        self.write_backend_manifests().await?;
        // Only what this run attempted can fail it; a model resting out its cooldown is
        // reported again once the wait lapses.
        if !outcome.failed_model_ids.is_empty() {
            let examples: Vec<&str> = outcome.failed_model_ids.iter().take(5).map(String::as_str).collect();
            let mut message = format!("{} Azur Lane models failed", outcome.failed_model_ids.len());
            if !examples.is_empty() {
                message = format!("{message}: {}", examples.join(", "));
            }
            if !outcome.cooldown_model_ids.is_empty() {
                message = format!("{message} ({} more still in retry cooldown)", outcome.cooldown_model_ids.len());
            }
            return Err(JobError::new("CrawlRunError", message));
        }
        Ok(())
    }
}

/// The scheduled job: a fresh crawler per run, reading `web.azurlane` when it starts.
#[derive(Debug)]
pub struct AzurLaneJob {
    ctx: SourceContext,
}

impl AzurLaneJob {
    pub fn new(ctx: SourceContext) -> Self {
        Self { ctx }
    }
}

impl Source for AzurLaneJob {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self.ctx.settings.load().await.map_err(|error| settings_job_error(&error))?;
            crawler_from_settings(&settings.web.azurlane, self.ctx.db.clone())?.update().await
        })
    }
}

/// `AzurLane()` as the job builds it: the path, origin proxy and origin interval from the
/// settings.
pub fn crawler_from_settings(config: &fav_core::settings::AzurLane, db: Db) -> Result<AzurLaneCrawler, JobError> {
    let clients = Clients::build(&config.origin_proxy, &config.origin_connect_host)
        .map_err(|error| JobError::new("ValueError", error.to_string()))?;
    Ok(AzurLaneCrawler::new(
        config.path.as_str(),
        clients,
        db,
        Pacing::new(config.origin_request_interval_seconds),
    ))
}

/// Register the job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run("azurlane", move || Box::new(AzurLaneJob::new(ctx.clone())));
}
