//! BD2 (Brown Dust 2): the GameKee character pages, their media and Live2D models, with
//! the BD2-L2D-Viewer repository filling in and correcting the Live2D files.
//!
//! Python origin: `src/web/bd2.py` (the `BD2` job). The pure page parsing is in
//! [`extract`]; the discovery helpers are [`crate::content_discovery`], shared with NIKKE. The
//! GameKee helpers Python imports from `src/web/nikke.py` are still BD2's copy in
//! [`gamekee`] (`rust/docs/PLAN.md`, cleanup notes).
//!
//! Downloads go through a content-addressed blob store under `<root>/_blobs`, each page's
//! files being hard links to it, as in Python. The run holds the `bd2` advisory lock; the
//! tables are created by the schema, not by the run (DIVERGENCES.md D96).

mod extract;
mod gamekee;
#[cfg(test)]
mod golden;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, FixedOffset, Utc};
use fav_core::logging::{self, Logger};
use fav_core::pyjson::{Dict, Value, py_isoformat_datetime};
use fav_core::pypath;
use fav_db::{Db, Param, Statement};
use fav_net::download::{self, DownloadError, Downloaded, TempOptions};
use fav_net::retry::{Sleeper, tokio_sleeper};
use fav_net::{ClientOptions, HttpClient, Request};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox};
use fav_runtime::source::{JobError, Registry, Source};
use sqlx::Row as _;
use sqlx::postgres::PgRow;
use tokio::sync::{OnceCell, Semaphore};

use self::extract::{GAME_ALIAS, GAMEKEE_BASE_URL, TREE_ROOT_PID, parse_content_id, request_headers};
use self::extract::{
    add_viewer_model_assets, asset_counts, assign_asset_paths, base_info_from_summary, cdn_json_url, cdn_request_headers, content_hash,
    content_json_from_cdn_payload, detail_name, extract_resources, filter_bd2_character_rows, get, is_nonblocking_failed_asset,
    live2d_atlas_texture_context, max_attempts_for_status, output_root, parse_content_json, prefer_live2d_model_assets_from_viewer,
    relative_manifest_assets, retry_after_delay_seconds, retry_delay_seconds, reverse_bind_from_cdn_payload, reverse_bind_map,
    row_content_id, row_name, summarize_content, supplement_live2d_models_from_viewer, to_int, video_retry_cooldown_days,
    viewer_extra_live2d_model, viewer_extra_resources, viewer_extra_root, viewer_extra_title, viewer_resource_payload,
};
use self::gamekee::{Asset, Assets, add_asset, asset_headers, normalize_url, parse_spine_atlas_textures, response_json, write_json};
use crate::bd2_l2d_viewer::{self, VIEWER_PAGE_URL, ViewerResource};
use crate::content_discovery as discovery;
use crate::kit::{OutboxNotifier, SharedNotifier, blocking};
use crate::{SourceContext, db_job_error, pyerr, settings_job_error};

const API_REQUEST_INTERVAL_SECONDS: f64 = 0.5;
const CDN_REQUEST_INTERVAL_SECONDS: f64 = 0.2;
const CDN_CONCURRENCY: usize = 3;
const ASSET_PROCESS_CONCURRENCY: usize = 3;
const MAX_RETRIES: u32 = 3;
const CIRCUIT_LIMITED_FAILURES: u32 = 5;
const CIRCUIT_PAUSE_SECONDS: f64 = 30.0;
const BODY_PREFIX_LIMIT: usize = 4096;
/// 403, 406, 429 and 503 count toward the circuit breaker.
const LIMITED_STATUSES: [u16; 4] = [403, 406, 429, 503];
const VIEWER_EXTRA_CONTENT_ID: i128 = 0;

fn log() -> Logger {
    logging::get("bd2")
}

/// `datetime.now(UTC)`, injectable.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
/// `secrets.randbelow(250)`, injectable.
pub type Jitter = Arc<dyn Fn() -> u32 + Send + Sync>;

fn system_clock() -> Clock {
    Arc::new(Utc::now)
}

fn random_jitter() -> Jitter {
    Arc::new(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        nanos % 250
    })
}

/// `_RateLimiter`: at least `interval` seconds between the starts of two requests.
#[derive(Debug)]
struct RateLimiter {
    interval: f64,
    last_start: tokio::sync::Mutex<Option<Instant>>,
}

impl RateLimiter {
    fn new(interval: f64) -> Self {
        Self {
            interval,
            last_start: tokio::sync::Mutex::new(None),
        }
    }

    async fn wait(&self, sleeper: &Sleeper) {
        let mut last_start = self.last_start.lock().await;
        if let Some(previous) = *last_start {
            let delay = self.interval - previous.elapsed().as_secs_f64();
            if delay > 0.0 {
                sleeper(delay).await;
            }
        }
        *last_start = Some(Instant::now());
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
            "BD2 CDN/API returned repeated throttling responses; pausing this run for {CIRCUIT_PAUSE_SECONDS:.0}s"
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
}

/// `TempBlob`: a downloaded body not yet in the blob store.
#[derive(Debug)]
struct TempBlob {
    file: Downloaded,
    size: i64,
    content_type: String,
}

/// How one download attempt ended when it did not produce a file.
enum AttemptError {
    /// Retry after the delay.
    Retry(JobError, f64),
    /// Raise at once.
    Raise(JobError),
}

/// A content id as a `BIGINT` parameter. psycopg sends a larger int as `numeric`, which
/// the column refuses.
fn bigint(value: i128) -> Result<i64, JobError> {
    i64::try_from(value).map_err(|_| JobError::new("NumericValueOutOfRange", "bigint out of range"))
}

fn text_param(value: impl Into<String>) -> Param {
    Param::Text(Some(value.into()))
}

/// `_json_dumps(data)`.
fn json_param(value: &Value) -> Result<Param, JobError> {
    Ok(text_param(gamekee::json_dumps(value)?))
}

fn optional_dict(value: Option<&Dict>) -> Value {
    value.map_or(Value::Null, |dict| Value::Dict(dict.clone()))
}

fn column<T>(row: &PgRow, name: &str) -> Result<T, JobError>
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get(name).map_err(|error| db_job_error(&fav_db::DbError::Sqlx(error)))
}

/// The crawler for one run: `BD2(path=..., client=..., viewer_resources=...)`.
pub struct Bd2Crawler {
    root: String,
    http: HttpClient,
    db: Db,
    notifier: SharedNotifier,
    sleeper: Sleeper,
    jitter: Jitter,
    clock: Clock,
    /// Tests only: send `https://` URLs as `http://` so the fake web, an HTTP proxy, sees
    /// them; status errors still name the `https://` URL.
    plain_http: bool,
    viewer_resources: OnceCell<Vec<ViewerResource>>,
    api_limiter: RateLimiter,
    cdn_limiter: RateLimiter,
    cdn_semaphore: Semaphore,
    circuit_breaker: CircuitBreaker,
    notify_discoveries: AtomicBool,
}

impl std::fmt::Debug for Bd2Crawler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bd2Crawler").field("root", &self.root).finish_non_exhaustive()
    }
}

/// `httpx.AsyncClient(base_url=GAMEKEE_BASE_URL, follow_redirects=True, headers=request_headers(), timeout=...)`.
pub fn client_options() -> ClientOptions {
    ClientOptions {
        headers: request_headers(None),
        timeout: Duration::from_mins(1),
        connect_timeout: Some(Duration::from_secs(20)),
        follow_redirects: true,
        ..ClientOptions::default()
    }
}

impl Bd2Crawler {
    pub fn new(root: &str, http: HttpClient, db: Db, notifier: SharedNotifier) -> Self {
        Self {
            root: root.to_owned(),
            http,
            db,
            notifier,
            sleeper: tokio_sleeper(),
            jitter: random_jitter(),
            clock: system_clock(),
            plain_http: false,
            viewer_resources: OnceCell::new(),
            api_limiter: RateLimiter::new(API_REQUEST_INTERVAL_SECONDS),
            cdn_limiter: RateLimiter::new(CDN_REQUEST_INTERVAL_SECONDS),
            cdn_semaphore: Semaphore::new(CDN_CONCURRENCY),
            circuit_breaker: CircuitBreaker::default(),
            notify_discoveries: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn with_sleeper(mut self, sleeper: Sleeper) -> Self {
        self.sleeper = sleeper;
        self
    }

    #[must_use]
    pub fn with_jitter(mut self, jitter: Jitter) -> Self {
        self.jitter = jitter;
        self
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// `viewer_resources=`: the viewer's resources, so the run does not fetch them.
    #[must_use]
    pub fn with_viewer_resources(self, resources: Vec<ViewerResource>) -> Self {
        let _ = self.viewer_resources.set(resources);
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_plain_http(mut self) -> Self {
        self.plain_http = true;
        self
    }

    fn path(&self, relative: &str) -> PathBuf {
        PathBuf::from(pypath::join(&self.root, relative))
    }

    /// `_utc_now_iso()`.
    fn now_iso(&self) -> String {
        py_isoformat_datetime(&(self.clock)().with_timezone(&FixedOffset::east_opt(0).unwrap_or_else(|| unreachable!())))
    }

    fn wire_url(&self, url: &str) -> String {
        match url.strip_prefix("https://") {
            Some(rest) if self.plain_http => format!("http://{rest}"),
            _ => url.to_owned(),
        }
    }

    fn status_error(&self, mut error: fav_net::StatusError) -> JobError {
        if self.plain_http
            && let Some(rest) = error.url.strip_prefix("http://")
        {
            error.url = format!("https://{rest}");
        }
        pyerr::status_error(&error)
    }

    /// `retry_delay_seconds(attempt, response)`: the jitter is drawn only when the response
    /// has no usable `Retry-After`.
    fn retry_delay(&self, attempt: u32, retry_after: Option<f64>) -> f64 {
        match retry_after {
            Some(seconds) => seconds,
            None => retry_delay_seconds(attempt, None, (self.jitter)()),
        }
    }

    fn response_retry_after(&self, response: &fav_net::Response) -> Option<f64> {
        retry_after_delay_seconds(response.header("retry-after").as_deref(), (self.clock)())
    }

    // ---------- HTTP ----------

    /// `_request(client, 'GET', url, headers=..., bucket=...)`: the body of the first
    /// answer below 400, with retries for transport errors and retryable statuses.
    async fn request(&self, url: &str, headers: &[(String, String)], api_bucket: bool) -> Result<bytes::Bytes, JobError> {
        let limiter = if api_bucket { &self.api_limiter } else { &self.cdn_limiter };
        let mut last_error: Option<JobError> = None;
        for attempt in 1..=MAX_RETRIES {
            let mut retry_delay = self.retry_delay(attempt, None);
            limiter.wait(&self.sleeper).await;
            let mut request = Request::get(self.wire_url(url));
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
            match self.http.send(request).await {
                Ok(response) if response.status() < 400 => match response.bytes().await {
                    Ok(body) => {
                        self.circuit_breaker.record_success();
                        return Ok(body);
                    }
                    Err(error) => last_error = Some(pyerr::transport_error(&error)),
                },
                Ok(response) => {
                    let status = response.status();
                    let attempts = max_attempts_for_status(status);
                    if LIMITED_STATUSES.contains(&status) {
                        self.circuit_breaker.record_limited_failure(&self.sleeper).await;
                    }
                    if attempt >= attempts {
                        return Err(match response.error_for_status() {
                            Err(error) => self.status_error(error),
                            Ok(()) => JobError::new("HTTPStatusError", format!("HTTP {status}")),
                        });
                    }
                    retry_delay = self.retry_delay(attempt, self.response_retry_after(&response));
                    last_error = Some(JobError::new("HTTPStatusError", format!("HTTP {status}")));
                }
                Err(error) => last_error = Some(pyerr::transport_error(&error)),
            }
            if attempt < MAX_RETRIES {
                (self.sleeper)(retry_delay).await;
            }
        }
        Err(last_error.unwrap_or_else(|| JobError::new("RuntimeError", format!("Failed to request {url}"))))
    }

    /// `_fetch_json(client, path, content_id=...)`.
    async fn fetch_json(&self, path: &str, content_id: Option<i128>) -> Result<Dict, JobError> {
        let body = self
            .request(&format!("{GAMEKEE_BASE_URL}{path}"), &request_headers(content_id), true)
            .await?;
        response_json(&body).map_err(crate::PyError::into_job_error)
    }

    /// `_fetch_cdn_json(client, url, content_id=..., label=...)`.
    async fn fetch_cdn_json(&self, url: &str, content_id: i128, label: &str) -> Result<Dict, JobError> {
        let body = self.request(url, &cdn_request_headers(), true).await?;
        match pyerr::json_loads_bytes(&body)? {
            Value::Dict(data) => Ok(data),
            _ => Err(JobError::new(
                "TypeError",
                format!("GameKee {label} returned a non-object JSON response for content_id={content_id}"),
            )),
        }
    }

    /// `_load_content_json(client, detail, content_id=...)`.
    async fn load_content_json(&self, detail: &Dict, content_id: i128) -> Result<Dict, JobError> {
        let content_json = parse_content_json(detail);
        if !content_json.is_empty() {
            return Ok(content_json);
        }
        let url = cdn_json_url(detail, "content_cdn");
        if !url.is_empty() {
            let content_json = content_json_from_cdn_payload(&self.fetch_cdn_json(&url, content_id, "content_cdn").await?);
            if !content_json.is_empty() {
                return Ok(content_json);
            }
            return Err(JobError::new(
                "RuntimeError",
                format!("GameKee content_cdn is empty for content_id={content_id}"),
            ));
        }
        Err(JobError::new(
            "RuntimeError",
            format!("GameKee detail content_json is empty for content_id={content_id}"),
        ))
    }

    /// `_load_reverse_bind(client, detail, content_id=...)`.
    async fn load_reverse_bind(&self, detail: &Dict, content_id: i128) -> Result<Dict, JobError> {
        let reverse_bind = reverse_bind_map(detail);
        if !reverse_bind.is_empty() {
            return Ok(reverse_bind);
        }
        let url = cdn_json_url(detail, "entry_data_bind_cdn");
        if url.is_empty() {
            return Ok(Dict::new());
        }
        Ok(reverse_bind_from_cdn_payload(
            &self.fetch_cdn_json(&url, content_id, "entry_data_bind_cdn").await?,
        ))
    }

    /// `_fetch_tree_rows(client)`.
    async fn fetch_tree_rows(&self) -> Result<Vec<Dict>, JobError> {
        let payload = self.fetch_json(&format!("/v1/entry/treesByPid?pid={TREE_ROOT_PID}"), None).await?;
        filter_bd2_character_rows(get(&payload, "data"))
    }

    /// `_fetch_detail_response(client, content_id)`.
    async fn fetch_detail_response(&self, content_id: i128) -> Result<Dict, JobError> {
        let payload = self
            .fetch_json(&format!("/v1/content/detail/{content_id}"), Some(content_id))
            .await?;
        if !matches!(payload.get("data"), Some(Value::Dict(_))) {
            return Err(JobError::new(
                "TypeError",
                format!("GameKee detail payload is missing data for content_id={content_id}"),
            ));
        }
        Ok(payload)
    }

    /// `_viewer_resources_for_supplement()`: fetched once per run; a failure leaves the
    /// run without the viewer.
    async fn viewer_resources(&self) -> &[ViewerResource] {
        self.viewer_resources
            .get_or_init(|| async {
                let options = ClientOptions {
                    timeout: Duration::from_mins(1),
                    follow_redirects: true,
                    ..ClientOptions::default()
                };
                let fetched = match HttpClient::new(&options) {
                    Ok(client) => bd2_l2d_viewer::fetch_viewer_resources(
                        &client,
                        bd2_l2d_viewer::VIEWER_CHARACTER_LIST_URL,
                        bd2_l2d_viewer::VIEWER_REPO_TREE_URL,
                    )
                    .await
                    .map_err(|error| error.message),
                    Err(error) => Err(error.to_string()),
                };
                fetched.unwrap_or_else(|error| {
                    log().warning(format_args!("BD2 L2D Viewer supplement is unavailable for this run: {error}"));
                    Vec::new()
                })
            })
            .await
    }

    // ---------- notifications ----------

    /// `_notify_content_discovery(snapshot=..., character=...)`.
    async fn notify_content_discovery(&self, snapshot: &discovery::CharacterSnapshot, character: &Dict) {
        if !self.notify_discoveries.load(Ordering::Relaxed) || (snapshot.exists && snapshot.character.is_none()) {
            return;
        }
        let content_id = match get(character, "content_id") {
            Value::Int(id) => *id,
            _ => 0,
        };
        let character_name = match get(character, "title") {
            Value::Str(title) if !title.is_empty() => title.clone(),
            _ => content_id.to_string(),
        };
        let skin_names = |costumes: Option<&Value>| {
            discovery::collection_item_names(costumes, &["style_name", "title"], &["title", "style_name"], "Skin")
        };
        let current_skins = skin_names(character.get("costumes"));
        let (names, discovery_kind) = if snapshot.exists {
            let previous = skin_names(snapshot.character.as_ref().and_then(|previous| previous.get("costumes")));
            (discovery::new_collection_item_names(&previous, &current_skins), "skin")
        } else {
            (current_skins.into_iter().map(|(_, name)| name).collect(), "character")
        };
        if snapshot.exists && names.is_empty() {
            return;
        }
        let joined = names.join(", ");
        let body = format!(
            "Character: {character_name}\nSkin: {}",
            if joined.is_empty() { "None" } else { &joined }
        );
        let mut payload = Dict::new();
        payload.insert("content_id", Value::Int(content_id));
        payload.insert("character_name", Value::Str(character_name.clone()));
        payload.insert("skin_names", Value::List(names.iter().cloned().map(Value::Str).collect()));
        payload.insert("discovery", Value::Str(discovery_kind.to_owned()));
        let notification = NewNotification {
            kind: "content_discovered".to_owned(),
            source: "bd2".to_owned(),
            header: "BD2".to_owned(),
            title: format!("New {discovery_kind}: {character_name}"),
            body,
            link_url: format!("{GAMEKEE_BASE_URL}/{GAME_ALIAS}/tj/{content_id}.html"),
            payload: Some(payload),
            ..NewNotification::default()
        };
        if let Err(error) = self.notifier.enqueue(notification).await {
            log().warning(format_args!(
                "Failed to enqueue BD2 {discovery_kind} notification for content_id={content_id}: {error}"
            ));
        }
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

    /// `_upsert_tree_pages(rows, deactivate_missing=...)`.
    async fn upsert_tree_pages(&self, rows: &[Dict], deactivate_missing: bool) -> Result<(), JobError> {
        let mut statements = Vec::new();
        for row in rows {
            let Some(content_id) = row_content_id(row)? else { continue };
            statements.push(Statement::new(
                "INSERT INTO bd2_pages (
                    content_id, name, source_url, rarity_group_id, rarity_group_name,
                    active, raw_tree_json, first_seen_at, last_seen_at
                )
                VALUES ($1, $2, $3, $4, $5, TRUE, $6::jsonb, NOW(), NOW())
                ON CONFLICT (content_id) DO UPDATE SET
                    name = excluded.name,
                    source_url = excluded.source_url,
                    rarity_group_id = excluded.rarity_group_id,
                    rarity_group_name = excluded.rarity_group_name,
                    active = TRUE,
                    raw_tree_json = excluded.raw_tree_json,
                    last_seen_at = NOW();",
                vec![
                    Param::from(bigint(content_id)?),
                    text_param(row_name(row)),
                    text_param(format!("{GAMEKEE_BASE_URL}/{GAME_ALIAS}/tj/{content_id}.html")),
                    Param::from(bigint(group_id(row)?)?),
                    text_param(group_name(row)),
                    json_param(&Value::Dict(row.clone()))?,
                ],
            ));
        }
        if statements.is_empty() {
            return Ok(());
        }
        if deactivate_missing {
            statements.insert(0, Statement::new("UPDATE bd2_pages SET active = FALSE;", Vec::new()));
        }
        self.transaction(statements).await
    }

    /// `_upsert_page_fetch_start(content_id=..., name=..., row=..., detail_response=..., manifest_path=..., hash_value=...)`.
    async fn upsert_page_fetch_start(
        &self,
        content_id: i128,
        name: &str,
        row: Option<&Dict>,
        detail_response: &Dict,
        manifest_path: &str,
        hash_value: &str,
    ) -> Result<(), JobError> {
        let empty = Dict::new();
        let row_dict = row.unwrap_or(&empty);
        self.execute(
            "INSERT INTO bd2_pages (
                content_id, name, source_url, manifest_path, content_hash, rarity_group_id, rarity_group_name,
                active, first_seen_at, last_seen_at, fetched_at, raw_tree_json, raw_detail_json
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, TRUE, NOW(), NOW(), NOW(), $8::jsonb, $9::jsonb)
            ON CONFLICT (content_id) DO UPDATE SET
                name = excluded.name,
                source_url = excluded.source_url,
                manifest_path = excluded.manifest_path,
                content_hash = excluded.content_hash,
                rarity_group_id = excluded.rarity_group_id,
                rarity_group_name = excluded.rarity_group_name,
                active = TRUE,
                last_seen_at = NOW(),
                fetched_at = NOW(),
                raw_tree_json = excluded.raw_tree_json,
                raw_detail_json = excluded.raw_detail_json;",
            vec![
                Param::from(bigint(content_id)?),
                text_param(name),
                text_param(format!("{GAMEKEE_BASE_URL}/{GAME_ALIAS}/tj/{content_id}.html")),
                text_param(manifest_path),
                text_param(hash_value),
                Param::from(bigint(group_id(row_dict)?)?),
                text_param(group_name(row_dict)),
                json_param(&Value::Dict(row_dict.clone()))?,
                json_param(&Value::Dict(detail_response.clone()))?,
            ],
        )
        .await
        .map(|_| ())
    }

    /// `_upsert_asset_seen(url)`.
    async fn upsert_asset_seen(&self, url: &str) -> Result<(), JobError> {
        self.execute(
            "INSERT INTO bd2_assets (url, normalized_url, status, first_seen_at, last_seen_at)
            VALUES ($1, $2, 'pending', NOW(), NOW())
            ON CONFLICT (url) DO UPDATE SET
                normalized_url = excluded.normalized_url,
                last_seen_at = NOW(),
                status = CASE
                    WHEN bd2_assets.status = 'downloaded' THEN bd2_assets.status
                    WHEN bd2_assets.status = 'failed' AND bd2_assets.next_retry_at > NOW() THEN bd2_assets.status
                    ELSE excluded.status
                END,
                next_retry_at = CASE
                    WHEN bd2_assets.status = 'failed' AND bd2_assets.next_retry_at > NOW() THEN bd2_assets.next_retry_at
                    ELSE NULL
                END;",
            vec![text_param(url), text_param(url)],
        )
        .await
        .map(|_| ())
    }

    /// `_video_retry_cooldown(asset)`: `(last_error, next_retry_at)` while a failed video
    /// waits for its retry.
    async fn video_retry_cooldown(&self, asset: &Asset) -> Result<Option<(String, String)>, JobError> {
        if asset.kind != "video" {
            return Ok(None);
        }
        let rows = self
            .execute(
                "SELECT failed_count, last_error, next_retry_at
                FROM bd2_assets
                WHERE url = $1 AND status = 'failed' AND next_retry_at > NOW()
                LIMIT 1;",
                vec![text_param(asset.url.as_str())],
            )
            .await?;
        let Some(row) = rows.first() else { return Ok(None) };
        let last_error: String = column(row, "last_error")?;
        let next_retry_at: Option<DateTime<Utc>> = column(row, "next_retry_at")?;
        let next_retry_at = next_retry_at.map_or_else(String::new, |when| {
            fav_core::pyjson::py_str_datetime(&when.with_timezone(&FixedOffset::east_opt(0).unwrap_or_else(|| unreachable!())))
        });
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

    /// `_completed_blob_for_url(url)`: the verified blob of a downloaded URL.
    async fn completed_blob_for_url(&self, url: &str) -> Result<Option<BlobRef>, JobError> {
        let rows = self
            .execute(
                "SELECT a.sha256, a.size, COALESCE(NULLIF(a.content_type, ''), b.content_type) AS content_type, b.blob_path
                FROM bd2_assets AS a
                JOIN bd2_blobs AS b ON b.sha256 = a.sha256 AND b.size = a.size
                WHERE a.url = $1 AND a.status = 'downloaded' AND a.sha256 <> ''
                LIMIT 1;",
                vec![text_param(url)],
            )
            .await?;
        let Some(row) = rows.first() else { return Ok(None) };
        let sha256: String = column(row, "sha256")?;
        let size: i64 = column(row, "size")?;
        let content_type: Option<String> = column(row, "content_type")?;
        let blob_path: String = column(row, "blob_path")?;
        let path = self.resolve_blob_path(&blob_path);
        let verified = if sha256.is_empty() || size <= 0 {
            false
        } else {
            let (path, sha256) = (path.clone(), sha256.clone());
            blocking(move || Ok(gamekee::verify_file(&path, &sha256, size))).await?
        };
        if !verified {
            self.execute(
                "UPDATE bd2_assets SET status = 'missing', last_error = 'blob missing or failed verification' WHERE url = $1;",
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
        }))
    }

    /// `_mark_asset_downloaded(asset, blob)`.
    async fn mark_asset_downloaded(&self, asset: &Asset, blob: &BlobRef) -> Result<(), JobError> {
        self.execute(
            "UPDATE bd2_assets
            SET sha256 = $1, size = $2, content_type = $3, status = 'downloaded',
                failed_count = 0, last_error = '', next_retry_at = NULL, last_seen_at = NOW()
            WHERE url = $4;",
            vec![
                text_param(blob.sha256.as_str()),
                Param::from(blob.size),
                text_param(blob.content_type.as_str()),
                text_param(asset.url.as_str()),
            ],
        )
        .await
        .map(|_| ())
    }

    /// `_mark_asset_failed(asset, reason)`: videos wait 1, 3, then 7 days before a retry.
    async fn mark_asset_failed(&self, asset: &Asset, reason: &str) -> Result<(), JobError> {
        let is_video = asset.kind == "video";
        let first_retry_days = video_retry_cooldown_days(1);
        self.execute(
            "INSERT INTO bd2_assets (
                url, normalized_url, status, failed_count, last_error, next_retry_at, first_seen_at, last_seen_at
            )
            VALUES ($1, $2, 'failed', 1, $3, NOW() + ($4::integer * INTERVAL '1 day'), NOW(), NOW())
            ON CONFLICT (url) DO UPDATE SET
                status = 'failed',
                failed_count = bd2_assets.failed_count + 1,
                last_error = excluded.last_error,
                next_retry_at = CASE
                    WHEN $5::boolean THEN NOW() + (
                        CASE
                            WHEN bd2_assets.failed_count + 1 <= 1 THEN $6::integer * INTERVAL '1 day'
                            WHEN bd2_assets.failed_count + 1 = 2 THEN $7::integer * INTERVAL '1 day'
                            ELSE $8::integer * INTERVAL '1 day'
                        END
                    )
                    ELSE NULL
                END,
                last_seen_at = NOW();",
            vec![
                text_param(asset.url.as_str()),
                text_param(asset.url.as_str()),
                text_param(reason.chars().take(500).collect::<String>()),
                Param::Int(is_video.then_some(first_retry_days)),
                Param::Bool(Some(is_video)),
                Param::from(first_retry_days),
                Param::from(video_retry_cooldown_days(2)),
                Param::from(video_retry_cooldown_days(3)),
            ],
        )
        .await
        .map(|_| ())
    }

    /// `_register_temp_blob(temp_blob)`: the body joins the blob store, or is dropped for an
    /// identical verified blob already there.
    async fn register_temp_blob(&self, temp_blob: TempBlob) -> Result<BlobRef, JobError> {
        let TempBlob { file, size, content_type } = temp_blob;
        let sha256 = file.sha256_hex().to_owned();
        let rows = self
            .execute(
                "SELECT blob_path
                FROM bd2_blobs
                WHERE sha256 = $1 AND size = $2
                LIMIT 1;",
                vec![text_param(sha256.as_str()), Param::from(size)],
            )
            .await?;
        for row in &rows {
            let blob_path: String = column(row, "blob_path")?;
            let existing = self.resolve_blob_path(&blob_path);
            let verified = {
                let (existing, sha256) = (existing.clone(), sha256.clone());
                blocking(move || Ok(gamekee::verify_file(&existing, &sha256, size))).await?
            };
            if verified {
                drop(file);
                self.execute(
                    "UPDATE bd2_blobs SET last_verified_at = NOW() WHERE sha256 = $1 AND size = $2;",
                    vec![text_param(sha256.as_str()), Param::from(size)],
                )
                .await?;
                return Ok(BlobRef {
                    sha256,
                    size,
                    content_type,
                    path: existing,
                });
            }
        }
        let relative_path = format!("_blobs/sha256/{}/{sha256}", &sha256[..2.min(sha256.len())]);
        let blob_path = self.path(&relative_path);
        let already_there = {
            let (blob_path, sha256) = (blob_path.clone(), sha256.clone());
            blocking(move || {
                if let Some(parent) = blob_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| gamekee::os_error(&error, parent))?;
                }
                Ok(blob_path.exists() && gamekee::verify_file(&blob_path, &sha256, size))
            })
            .await?
        };
        if already_there {
            drop(file);
        } else {
            file.persist(&blob_path).await.map_err(|error| pyerr::download_error(&error))?;
        }
        self.execute(
            "INSERT INTO bd2_blobs (sha256, size, content_type, blob_path, created_at, last_verified_at)
            VALUES ($1, $2, $3, $4, NOW(), NOW())
            ON CONFLICT (sha256, size) DO UPDATE SET
                content_type = COALESCE(NULLIF(excluded.content_type, ''), bd2_blobs.content_type),
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
        })
    }

    // ---------- downloads ----------

    /// One attempt of `_download_asset_to_temp`, inside the CDN semaphore.
    async fn download_attempt(&self, asset: &Asset, tmp_dir: &Path, attempt: u32, retry_delay: f64) -> Result<TempBlob, AttemptError> {
        let _permit = self
            .cdn_semaphore
            .acquire()
            .await
            .map_err(|error| AttemptError::Raise(JobError::new("RuntimeError", error.to_string())))?;
        self.cdn_limiter.wait(&self.sleeper).await;
        let mut request = Request::get(self.wire_url(&asset.url));
        for (name, value) in asset_headers() {
            request = request.header(name, value);
        }
        let mut response = self
            .http
            .send(request)
            .await
            .map_err(|error| AttemptError::Retry(pyerr::transport_error(&error), retry_delay))?;
        let status = response.status();
        if status >= 400 {
            let attempts = max_attempts_for_status(status);
            if LIMITED_STATUSES.contains(&status) {
                self.circuit_breaker.record_limited_failure(&self.sleeper).await;
            }
            let error = JobError::new("HTTPStatusError", format!("HTTP {status}"));
            if attempt >= attempts {
                return Err(AttemptError::Raise(error));
            }
            return Err(AttemptError::Retry(
                error,
                self.retry_delay(attempt, self.response_retry_after(&response)),
            ));
        }
        let options = TempOptions {
            prefix: ".bd2-download-",
            suffix: "",
            mode: 0o600,
        };
        let file = download::to_temp_file(&mut response, tmp_dir, &options)
            .await
            .map_err(|error| match &error {
                DownloadError::Transport(transport) => AttemptError::Retry(pyerr::transport_error(transport), retry_delay),
                DownloadError::Io { .. } => AttemptError::Raise(pyerr::download_error(&error)),
            })?;
        let prefix = {
            let path = file.temp_path().to_path_buf();
            blocking(move || {
                use std::io::Read as _;
                let mut prefix = Vec::with_capacity(BODY_PREFIX_LIMIT);
                std::fs::File::open(&path)
                    .and_then(|handle| handle.take(BODY_PREFIX_LIMIT as u64).read_to_end(&mut prefix))
                    .map_err(|error| gamekee::os_error(&error, &path))?;
                Ok(prefix)
            })
            .await
            .map_err(AttemptError::Raise)?
        };
        let content_type = response.header("content-type").unwrap_or_default();
        let size = i64::try_from(file.len()).unwrap_or(i64::MAX);
        let invalid_reason = gamekee::validate_asset_response(&asset.kind, &content_type, &prefix, size);
        if !invalid_reason.is_empty() {
            if invalid_reason == "cdn rejection response" {
                self.circuit_breaker.record_limited_failure(&self.sleeper).await;
            }
            return Err(AttemptError::Retry(
                JobError::new("InvalidAssetResponseError", invalid_reason),
                retry_delay,
            ));
        }
        self.circuit_breaker.record_success();
        Ok(TempBlob { file, size, content_type })
    }

    /// `_download_asset_to_temp(client, asset)`.
    async fn download_asset_to_temp(&self, asset: &Asset) -> Result<TempBlob, JobError> {
        let tmp_dir = self.path("_tmp");
        {
            let tmp_dir = tmp_dir.clone();
            blocking(move || std::fs::create_dir_all(&tmp_dir).map_err(|error| gamekee::os_error(&error, &tmp_dir))).await?;
        }
        let mut last_error: Option<JobError> = None;
        for attempt in 1..=MAX_RETRIES {
            let retry_delay = self.retry_delay(attempt, None);
            match self.download_attempt(asset, &tmp_dir, attempt, retry_delay).await {
                Ok(blob) => return Ok(blob),
                Err(AttemptError::Raise(error)) => return Err(error),
                Err(AttemptError::Retry(error, delay)) => {
                    last_error = Some(error);
                    if attempt < MAX_RETRIES {
                        (self.sleeper)(delay).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| JobError::new("RuntimeError", format!("Failed to download {}", asset.url))))
    }

    /// The part of `_process_asset` inside its `try`.
    async fn fetch_and_materialize(&self, root: &str, asset: &mut Asset) -> Result<(), JobError> {
        let blob = if let Some(blob) = self.completed_blob_for_url(&asset.url).await? {
            "reused".clone_into(&mut asset.status);
            blob
        } else {
            let temp_blob = self.download_asset_to_temp(asset).await?;
            let blob = self.register_temp_blob(temp_blob).await?;
            "downloaded".clone_into(&mut asset.status);
            blob
        };
        {
            let destination = PathBuf::from(pypath::join(root, &asset.local_path));
            let (blob_path, sha256, size) = (blob.path.clone(), blob.sha256.clone(), blob.size);
            blocking(move || gamekee::materialize_blob(&blob_path, &destination, &sha256, size)).await?;
        }
        asset.sha256.clone_from(&blob.sha256);
        asset.size = blob.size;
        asset.content_type.clone_from(&blob.content_type);
        asset.error = String::new();
        self.mark_asset_downloaded(asset, &blob).await
    }

    /// `_process_asset(client=..., root=..., asset=...)`: whether the file is in place.
    async fn process_asset(&self, root: &str, asset: &mut Asset) -> Result<bool, JobError> {
        self.upsert_asset_seen(&asset.url).await?;
        if let Some((last_error, next_retry_at)) = self.video_retry_cooldown(asset).await? {
            "failed".clone_into(&mut asset.status);
            asset.error = if last_error.is_empty() {
                "retry cooldown active".to_owned()
            } else {
                last_error
            };
            log().info(format_args!(
                "Skipping BD2 video asset during retry cooldown until {next_retry_at}: {}",
                asset.url
            ));
            return Ok(false);
        }
        match self.fetch_and_materialize(root, asset).await {
            Ok(()) => Ok(true),
            Err(error) => {
                let message = if error.message().is_empty() {
                    error.class_name()
                } else {
                    error.message()
                }
                .to_owned();
                "failed".clone_into(&mut asset.status);
                asset.error.clone_from(&message);
                log().warning(format_args!("Failed to download BD2 asset {}: {message}", asset.url));
                self.mark_asset_failed(asset, &message).await?;
                Ok(false)
            }
        }
    }

    /// `_process_assets(client=..., root=..., assets=...)`: each URL's assets in turn, three
    /// URLs at a time; a failure other than an optional one fails the page.
    async fn process_assets(&self, root: &str, assets: &mut Assets) -> Result<(), JobError> {
        use futures_util::stream::{self, StreamExt as _, TryStreamExt as _};

        if assets.is_empty() {
            return Ok(());
        }
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (index, asset) in assets.iter().enumerate() {
            match groups.iter_mut().find(|(url, _)| *url == asset.url) {
                Some((_, indices)) => indices.push(index),
                None => groups.push((asset.url.clone(), vec![index])),
            }
        }
        let jobs: Vec<Vec<(usize, Asset)>> = groups
            .into_iter()
            .map(|(_, indices)| indices.into_iter().map(|index| (index, assets.as_slice()[index].clone())).collect())
            .collect();
        let processed: Vec<Vec<(usize, Asset)>> = stream::iter(jobs)
            .map(|mut group| async move {
                for (_, asset) in &mut group {
                    self.process_asset(root, asset).await?;
                }
                Ok::<_, JobError>(group)
            })
            .buffer_unordered(ASSET_PROCESS_CONCURRENCY)
            .try_collect()
            .await?;
        for (index, asset) in processed.into_iter().flatten() {
            assets.as_mut_slice()[index] = asset;
        }
        let failed: Vec<&Asset> = assets.iter().filter(|asset| asset.status == "failed").collect();
        let blocking_failed: Vec<&&Asset> = failed.iter().filter(|asset| !is_nonblocking_failed_asset(asset)).collect();
        if !blocking_failed.is_empty() {
            let examples: Vec<&str> = blocking_failed.iter().take(3).map(|asset| asset.url.as_str()).collect();
            return Err(JobError::new(
                "AssetProcessingError",
                format!("{} BD2 assets failed: {}", blocking_failed.len(), examples.join(", ")),
            ));
        }
        if !failed.is_empty() {
            log().info(format_args!(
                "BD2 page kept {} unavailable optional assets in manifest without failing the page",
                failed.len()
            ));
        }
        Ok(())
    }

    /// `_read_known_atlas_text(atlas_url)`.
    async fn read_known_atlas_text(&self, atlas_url: &str) -> Result<Option<String>, JobError> {
        let Some(blob) = self.completed_blob_for_url(atlas_url).await? else {
            return Ok(None);
        };
        blocking(move || Ok(std::fs::read(&blob.path).ok().map(|bytes| gamekee::decode_utf8_ignore(&bytes)))).await
    }

    /// `_expand_model_atlas_textures(...)`: the atlas's URL when it could not be read.
    async fn expand_model_atlas_textures(&self, root: &str, assets: &mut Assets, model: &mut Dict) -> Result<Option<String>, JobError> {
        let Some(Value::Dict(urls)) = model.get("urls") else {
            return Ok(None);
        };
        let atlas_url = match urls.get("atlas") {
            Some(Value::Str(url)) if !url.is_empty() => url.clone(),
            _ => return Ok(None),
        };
        let processed = match assets.position(&atlas_url, "live2d_atlas") {
            Some(index) => {
                let mut atlas = assets.as_slice()[index].clone();
                let processed = self.process_asset(root, &mut atlas).await?;
                assets.as_mut_slice()[index] = atlas;
                processed
            }
            None => false,
        };
        if !processed {
            return Ok(Some(atlas_url));
        }
        let Some(atlas_text) = self.read_known_atlas_text(&atlas_url).await? else {
            log().warning(format_args!(
                "Failed to inspect atlas textures for {atlas_url}: atlas blob is unavailable"
            ));
            return Ok(Some(atlas_url));
        };
        let textures = parse_spine_atlas_textures(&atlas_text);
        if !textures.is_empty() {
            let mut texture_urls = Vec::with_capacity(textures.len());
            for texture in &textures {
                let joined =
                    fav_core::pyurl::urljoin(&atlas_url, texture).map_err(|error| bd2_l2d_viewer::url_error(&error).into_job_error())?;
                texture_urls.push(normalize_url(&joined));
            }
            if let Some(Value::Dict(urls)) = model.get("urls") {
                // `urls['atlas_textures'] = ...` on the model's own `urls` object.
                let mut urls = urls.clone();
                urls.insert(
                    "atlas_textures",
                    Value::List(texture_urls.iter().cloned().map(Value::Str).collect()),
                );
                model.insert("urls", Value::Dict(urls));
            }
            let context = live2d_atlas_texture_context(model);
            for texture_url in &texture_urls {
                add_asset(assets, texture_url, "live2d_texture", context.clone());
            }
        }
        Ok(None)
    }

    /// `_expand_atlas_textures(...)`: every model's atlas downloaded and its textures added.
    async fn expand_atlas_textures(&self, root: &str, assets: &mut Assets, live2d_models: &mut [Dict]) -> Result<(), JobError> {
        let mut failed_atlas_urls = Vec::new();
        for model in live2d_models.iter_mut() {
            if let Some(url) = self.expand_model_atlas_textures(root, assets, model).await? {
                failed_atlas_urls.push(url);
            }
        }
        if !failed_atlas_urls.is_empty() {
            let examples: Vec<&str> = failed_atlas_urls.iter().take(3).map(String::as_str).collect();
            return Err(JobError::new(
                "AssetProcessingError",
                format!(
                    "Failed to inspect {} BD2 Live2D atlas files: {}",
                    failed_atlas_urls.len(),
                    examples.join(", ")
                ),
            ));
        }
        Ok(())
    }

    /// `_replace_page_assets_and_mark_completed(content_id=..., assets=...)`.
    async fn replace_page_assets_and_mark_completed(&self, content_id: i128, assets: &Assets) -> Result<(), JobError> {
        let id = bigint(content_id)?;
        let mut statements = vec![Statement::new(
            "DELETE FROM bd2_page_assets WHERE content_id = $1;",
            vec![Param::from(id)],
        )];
        let empty_context = [Dict::new()];
        for asset in assets.iter() {
            let original_filename = gamekee::original_filename_from_url(&asset.url, &asset.kind).map_err(crate::PyError::into_job_error)?;
            let contexts: &[Dict] = if asset.contexts.is_empty() {
                &empty_context
            } else {
                &asset.contexts
            };
            for context in contexts {
                statements.push(Statement::new(
                    "INSERT INTO bd2_page_assets (
                        content_id, url, kind, context_hash, local_path, original_filename, context_json, created_at, updated_at
                    )
                    VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, NOW(), NOW())
                    ON CONFLICT (content_id, url, kind, context_hash) DO UPDATE SET
                        local_path = excluded.local_path,
                        original_filename = excluded.original_filename,
                        context_json = excluded.context_json,
                        updated_at = NOW();",
                    vec![
                        Param::from(id),
                        text_param(asset.url.as_str()),
                        text_param(asset.kind.as_str()),
                        text_param(gamekee::context_hash(context)?),
                        text_param(asset.local_path.as_str()),
                        text_param(original_filename.as_str()),
                        json_param(&Value::Dict(context.clone()))?,
                    ],
                ));
            }
        }
        statements.push(Statement::new(
            "UPDATE bd2_pages SET completed_at = NOW() WHERE content_id = $1;",
            vec![Param::from(id)],
        ));
        self.transaction(statements).await
    }

    // ---------- pages ----------

    /// `_archive_viewer_extra_resource(client=..., resource=..., skip_assets=...)`.
    async fn archive_viewer_extra_resource(&self, resource: &ViewerResource, skip_assets: bool) -> Result<Option<String>, JobError> {
        let Some(model) = viewer_extra_live2d_model(resource) else {
            return Ok(None);
        };
        let mut assets = Assets::new();
        add_viewer_model_assets(&mut assets, &model);
        let root = viewer_extra_root(&self.root, resource);
        let payload = viewer_resource_payload(resource);
        write_json_blocking(pypath::join(&root, "raw/viewer-resource.json"), payload.clone()).await?;

        let mut models = vec![model];
        assign_asset_paths(&mut assets, VIEWER_EXTRA_CONTENT_ID)?;
        if !skip_assets {
            self.expand_atlas_textures(&root, &mut assets, &mut models).await?;
            assign_asset_paths(&mut assets, VIEWER_EXTRA_CONTENT_ID)?;
            self.process_assets(&root, &mut assets).await?;
        }
        let mut manifest = Dict::new();
        manifest.insert("source_url", Value::Str(VIEWER_PAGE_URL.to_owned()));
        manifest.insert("fetched_at", Value::Str(self.now_iso()));
        manifest.insert("collection_type", Value::Str("extra".to_owned()));
        manifest.insert("source", Value::Str(extract::VIEWER_SUPPLEMENT_SOURCE.to_owned()));
        manifest.insert("title", Value::Str(viewer_extra_title(resource)));
        manifest.insert("entry_id", Value::Str(resource.entry_id.clone()));
        manifest.insert("viewer_resource", payload);
        manifest.insert("live2d_models", Value::List(models.into_iter().map(Value::Dict).collect()));
        manifest.insert("assets", relative_manifest_assets(&assets));
        manifest.insert("asset_counts", Value::Dict(asset_counts(&assets)));
        write_json_blocking(pypath::join(&root, "manifest.json"), Value::Dict(manifest)).await?;
        Ok(Some(root))
    }

    /// `_archive_viewer_extras(client=..., skip_assets=...)`: the viewer's minigame models.
    async fn archive_viewer_extras(&self, skip_assets: bool) -> Result<Vec<String>, JobError> {
        let extras = viewer_extra_resources(self.viewer_resources().await);
        let mut archived = Vec::new();
        for resource in &extras {
            if let Some(root) = self.archive_viewer_extra_resource(resource, skip_assets).await? {
                archived.push(root);
            }
        }
        if !archived.is_empty() {
            log().info(format_args!("Archived {} BD2 L2D Viewer extra resources", archived.len()));
        }
        Ok(archived)
    }

    /// `_crawl_page(client=..., tree_row=..., content_id=..., skip_assets=...)`: the page's
    /// directory.
    #[allow(clippy::too_many_lines)] // One step per line of Python's `_crawl_page`.
    async fn crawl_page(&self, tree_row: Option<&Dict>, content_id: i128, skip_assets: bool) -> Result<String, JobError> {
        let detail_response = self.fetch_detail_response(content_id).await?;
        let Some(Value::Dict(detail)) = detail_response.get("data") else {
            unreachable!("fetch_detail_response checked the data object")
        };
        let detail = detail.clone();
        let content_json = self.load_content_json(&detail, content_id).await?;
        let reverse_bind = self.load_reverse_bind(&detail, content_id).await?;
        let content_summary = summarize_content(&content_json, &reverse_bind);
        let base_info = base_info_from_summary(&content_summary);
        let (mut assets, mut live2d_models) = extract_resources(&content_json, tree_row, &reverse_bind);
        let viewer_resources = self.viewer_resources().await;
        let viewer_preferred = prefer_live2d_model_assets_from_viewer(&mut assets, &mut live2d_models, viewer_resources)?;
        if viewer_preferred > 0 {
            log().info(format_args!(
                "Preferred BD2-L2D-Viewer assets for {viewer_preferred} BD2 Live2D models for content_id={content_id}"
            ));
        }
        let viewer_added = supplement_live2d_models_from_viewer(
            &mut assets,
            &mut live2d_models,
            viewer_resources,
            Some(&content_json),
            Some(&reverse_bind),
        )?;
        if viewer_added > 0 {
            log().info(format_args!(
                "Supplemented {viewer_added} BD2 Live2D models from BD2-L2D-Viewer for content_id={content_id}"
            ));
        }

        let name = detail_name(&detail, content_id, tree_row);
        let root = output_root(&self.root, &detail, content_id, tree_row);
        let previous_character = {
            let (base, current) = (PathBuf::from(&self.root), PathBuf::from(&root));
            tokio::task::spawn_blocking(move || discovery::load_character_snapshot(&base, &current, content_id))
                .await
                .map_err(|error| JobError::new("RuntimeError", error.to_string()))?
        };
        let manifest_path = pypath::join(&root, "manifest.json");
        let hash_value = content_hash(&detail_response, &content_json, tree_row)?;
        self.upsert_page_fetch_start(content_id, &name, tree_row, &detail_response, &manifest_path, &hash_value)
            .await?;

        write_json_blocking(pypath::join(&root, "raw/detail.json"), Value::Dict(detail_response.clone())).await?;
        write_json_blocking(pypath::join(&root, "raw/content.json"), Value::Dict(content_json.clone())).await?;
        write_json_blocking(pypath::join(&root, "raw/tree-row.json"), optional_dict(tree_row)).await?;

        assign_asset_paths(&mut assets, content_id)?;
        if !skip_assets {
            self.expand_atlas_textures(&root, &mut assets, &mut live2d_models).await?;
            assign_asset_paths(&mut assets, content_id)?;
            self.process_assets(&root, &mut assets).await?;
        }

        let costumes = content_summary.get("costumes").cloned().unwrap_or_else(|| Value::List(Vec::new()));
        let models_value = Value::List(live2d_models.into_iter().map(Value::Dict).collect());
        let mut manifest = Dict::new();
        manifest.insert(
            "source_url",
            Value::Str(format!("{GAMEKEE_BASE_URL}/{GAME_ALIAS}/tj/{content_id}.html")),
        );
        manifest.insert(
            "detail_api_url",
            Value::Str(format!("{GAMEKEE_BASE_URL}/v1/content/detail/{content_id}")),
        );
        manifest.insert("fetched_at", Value::Str(self.now_iso()));
        manifest.insert("content_id", Value::Int(content_id));
        manifest.insert("title", Value::Str(name.clone()));
        manifest.insert("entry_id", get(&detail, "entry_id").clone());
        manifest.insert("updated_at", get(&detail, "updated_at").clone());
        manifest.insert("tree_row", optional_dict(tree_row));
        manifest.insert("base_info", Value::Dict(base_info.clone()));
        manifest.insert("content_summary", Value::Dict(content_summary));
        manifest.insert("costumes", costumes.clone());
        manifest.insert("live2d_models", models_value.clone());
        manifest.insert("assets", relative_manifest_assets(&assets));
        manifest.insert("asset_counts", Value::Dict(asset_counts(&assets)));
        write_json_blocking(manifest_path, Value::Dict(manifest)).await?;

        let mut character = Dict::new();
        character.insert("content_id", Value::Int(content_id));
        character.insert("title", Value::Str(name));
        character.insert("entry_id", get(&detail, "entry_id").clone());
        character.insert("tree_row", optional_dict(tree_row));
        character.insert("base_info", Value::Dict(base_info));
        character.insert("costumes", costumes);
        character.insert("live2d_models", models_value);
        write_json_blocking(pypath::join(&root, "character.json"), Value::Dict(character.clone())).await?;
        if !skip_assets {
            self.replace_page_assets_and_mark_completed(content_id, &assets).await?;
        }
        self.notify_content_discovery(&previous_character, &character).await;
        Ok(root)
    }

    async fn ensure_root(&self) -> Result<(), JobError> {
        let root = PathBuf::from(&self.root);
        blocking(move || std::fs::create_dir_all(&root).map_err(|error| gamekee::os_error(&error, &root))).await
    }

    /// `download_character(target, skip_assets=...)`: one page, by id or GameKee URL.
    pub async fn download_character(&self, target: &str, skip_assets: bool) -> Result<String, JobError> {
        let content_id = parse_content_id(target)?;
        let Some(lock) = self.db.try_advisory_lock("bd2").await.map_err(|error| db_job_error(&error))? else {
            return Err(JobError::new(
                "RuntimeError",
                "BD2 character download skipped because another run holds the advisory lock",
            ));
        };
        let result = async {
            self.ensure_root().await?;
            let rows = self.fetch_tree_rows().await?;
            let mut row = None;
            for item in &rows {
                if row_content_id(item)? == Some(content_id) {
                    row = Some(item.clone());
                    break;
                }
            }
            let row = row.unwrap_or_else(|| {
                let mut row = Dict::new();
                row.insert("content_id", Value::Int(content_id));
                row
            });
            let upsert = if rows.is_empty() { vec![row.clone()] } else { rows };
            self.upsert_tree_pages(&upsert, false).await?;
            self.crawl_page(Some(&row), content_id, skip_assets).await
        }
        .await;
        lock.release().await;
        result
    }

    /// `update()`: every character page, then the viewer's extras when all pages passed.
    pub async fn update(&self) -> Result<(), JobError> {
        let Some(lock) = self.db.try_advisory_lock("bd2").await.map_err(|error| db_job_error(&error))? else {
            log().info("BD2 update skipped because another run holds the advisory lock");
            return Ok(());
        };
        let result = self.update_locked().await;
        lock.release().await;
        result
    }

    async fn update_locked(&self) -> Result<(), JobError> {
        let has_snapshot = {
            let root = PathBuf::from(&self.root);
            tokio::task::spawn_blocking(move || discovery::has_character_snapshot(&root))
                .await
                .map_err(|error| JobError::new("RuntimeError", error.to_string()))?
        };
        self.notify_discoveries.store(has_snapshot, Ordering::Relaxed);
        self.ensure_root().await?;
        let rows = self.fetch_tree_rows().await?;
        if rows.is_empty() {
            log().warning("GameKee BD2 tree returned no character rows");
            return Ok(());
        }
        self.upsert_tree_pages(&rows, true).await?;
        let total = rows.len();
        log().info(format_args!("Found {total} GameKee BD2 character pages"));
        let mut failed_content_ids: Vec<i128> = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let Some(content_id) = row_content_id(row)? else { continue };
            log().info(format_args!("Crawling BD2 {content_id} ({}/{total})", index + 1));
            if let Err(error) = self.crawl_page(Some(row), content_id, false).await {
                log().error(format_args!("Failed to crawl BD2 content_id={content_id}\n{}", error.formatted()));
                failed_content_ids.push(content_id);
            }
        }
        if !failed_content_ids.is_empty() {
            let examples: Vec<String> = failed_content_ids.iter().take(5).map(ToString::to_string).collect();
            return Err(JobError::new(
                "CrawlRunError",
                format!("{} BD2 pages failed: {}", failed_content_ids.len(), examples.join(", ")),
            ));
        }
        self.archive_viewer_extras(false).await?;
        Ok(())
    }
}

/// `_to_int(row.get('_bd2_group_id')) or 0`.
fn group_id(row: &Dict) -> Result<i128, JobError> {
    Ok(to_int(get(row, "_bd2_group_id"))?.unwrap_or(0))
}

/// `str(row.get('_bd2_group_name') or '')`.
fn group_name(row: &Dict) -> String {
    match get(row, "_bd2_group_name") {
        Value::Str(name) => name.clone(),
        value if value.is_truthy() => fav_runtime::pystr::str_value(value),
        _ => String::new(),
    }
}

async fn write_json_blocking(path: String, value: Value) -> Result<(), JobError> {
    blocking(move || write_json(Path::new(&path), &value)).await
}

/// The scheduled job: a fresh crawler per run, reading `web.bd2.path` when it starts.
#[derive(Debug)]
pub struct Bd2Job {
    ctx: SourceContext,
}

impl Bd2Job {
    pub fn new(ctx: SourceContext) -> Self {
        Self { ctx }
    }
}

impl Source for Bd2Job {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self.ctx.settings.load().await.map_err(|error| settings_job_error(&error))?;
            let http = HttpClient::new(&client_options()).map_err(|error| JobError::new("ValueError", error.to_string()))?;
            let outbox = Outbox::new(self.ctx.db.clone(), Arc::clone(&self.ctx.settings));
            let crawler = Bd2Crawler::new(
                settings.web.bd2.path.as_str(),
                http,
                self.ctx.db.clone(),
                Arc::new(OutboxNotifier(outbox)),
            );
            crawler.update().await
        })
    }
}

/// Register the job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run("bd2", move || Box::new(Bd2Job::new(ctx.clone())));
}
