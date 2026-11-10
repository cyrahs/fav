//! The NIKKE job: list, page crawl, blob store and layer metadata (`Nikke` in
//! `src/web/nikke.py`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use fav_core::logging;
use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::py_strip;
use fav_core::settings;
use fav_core::validate::PyPath;
use fav_db::{Db, Param, Statement};
use fav_net::download::{Downloaded, PyOsError, TempOptions};
use fav_net::retry::{Sleeper, tokio_sleeper};
use fav_net::{ClientOptions, HttpClient, Request, Response};
use fav_runtime::notifications::{NewNotification, Outbox};
use fav_runtime::source::{JobError, Registry, Source};
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;

use super::runtime::{BrowserCapturer, RuntimeCaptureRequest, RuntimeCapturer};
use super::{
    Asset, Assets, GAMEKEE_BASE_URL, JOB_KEY, MAX_RETRIES, ReverseBind, api_payload, asset_headers, assign_asset_paths,
    base_info_from_summary, cdn_json_url, cdn_request_headers, content_hash, content_json_from_cdn_payload, context_hash, detail_name,
    extract_resources, filter_nikke_rows, is_limited_status, max_attempts_for_status, normalize_url, original_filename_from_url,
    output_dir_name, parse_content_id, parse_content_json, parse_spine_atlas_textures, relative_manifest_assets, request_headers,
    retry_delay_seconds, reverse_bind_from_cdn_payload, reverse_bind_map, row_content_id, row_name, summarize_content,
    validate_asset_response,
};
use crate::content_discovery::{
    CharacterSnapshot, collection_item_names, has_character_snapshot, load_character_snapshot, new_collection_item_names,
};
use crate::nikke_layer_metadata::{
    self as layer_metadata, LAYER_CAPTURE_RAW_PATH, LIVE2D_LAYER_CAPTURE_FIELD, LIVE2D_LAYER_METADATA_FIELDS, json_dumps, py_eq,
    random_hex, str_or_empty, to_int, value_or_empty, write_json,
};
use crate::pydecode::PyError;
use crate::{Notifier, OutboxNotifier, SourceContext, db_job_error, settings_job_error};

const API_REQUEST_INTERVAL_SECONDS: f64 = 0.5;
const CDN_REQUEST_INTERVAL_SECONDS: f64 = 0.2;
const CDN_CONCURRENCY: usize = 3;
const ASSET_PROCESS_CONCURRENCY: usize = 3;
const CIRCUIT_LIMITED_FAILURES: u32 = 5;
const CIRCUIT_PAUSE_SECONDS: f64 = 30.0;
const BODY_PREFIX_LIMIT: usize = 4096;
const DOWNLOAD_CHUNK_SIZE: usize = 1024 * 128;

fn log() -> logging::Logger {
    logging::get("nikke")
}

fn py_job_error(error: PyError) -> JobError {
    error.into_job_error()
}

fn os_job_error(error: &std::io::Error, path: &Path) -> JobError {
    let os = PyOsError::from_io(error, Some(path));
    JobError::new(os.class, os.message)
}

fn as_bigint(content_id: i128) -> Result<i64, JobError> {
    i64::try_from(content_id).map_err(|_| JobError::new("NumericValueOutOfRange", "bigint out of range"))
}

/// `datetime.now(UTC).isoformat()`.
pub(crate) fn utc_now_iso() -> String {
    let now = chrono::Utc::now().with_timezone(&chrono::FixedOffset::east_opt(0).expect("UTC offset"));
    fav_core::pyjson::py_isoformat_datetime(&now)
}

/// `secrets.randbelow(250) / 1000`.
fn random_jitter() -> f64 {
    let value = u64::from_str_radix(&random_hex(8), 16).unwrap_or(0) % 250;
    #[allow(clippy::cast_precision_loss)] // reason: below 250
    let value = value as f64;
    value / 1000.0
}

/// `_RateLimiter`: at least `interval` seconds between the starts of two requests.
struct RateLimiter {
    interval: f64,
    last_start: tokio::sync::Mutex<Option<tokio::time::Instant>>,
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
        if let Some(last) = *last_start {
            let delay = self.interval - last.elapsed().as_secs_f64();
            if delay > 0.0 {
                sleeper(delay).await;
            }
        }
        *last_start = Some(tokio::time::Instant::now());
    }
}

/// `TempBlob`: a verified download waiting to enter the blob store.
struct TempBlob {
    file: Downloaded,
    sha256: String,
    size: i64,
    content_type: String,
}

/// `BlobRef`.
#[derive(Debug, Clone)]
struct BlobRef {
    sha256: String,
    size: i64,
    content_type: String,
    path: PathBuf,
}

/// Why one asset was not stored: the error and whether GameKee answered 404.
struct AssetFailure {
    error: JobError,
    missing_upstream: bool,
}

impl From<JobError> for AssetFailure {
    fn from(error: JobError) -> Self {
        Self {
            error,
            missing_upstream: false,
        }
    }
}

/// One attempt of a download: retried (transport, status, rejected body) or not.
enum AttemptError {
    Retry(AssetFailure),
    Fatal(JobError),
}

fn file_sha256(path: &Path) -> std::io::Result<String> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut digest = sha2::Sha256::new();
    let mut buffer = vec![0_u8; DOWNLOAD_CHUNK_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().iter().fold(String::with_capacity(64), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    }))
}

/// `_verify_file`: the file has this size and hash (any error is a no).
pub(crate) fn verify_file(path: &Path, sha256: &str, size: i64) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) if i64::try_from(metadata.len()).ok() == Some(size) => file_sha256(path).is_ok_and(|digest| digest == sha256),
        _ => false,
    }
}

/// `os.link`, replaceable in tests.
pub(crate) type LinkFn = fn(&Path, &Path) -> std::io::Result<()>;

fn hard_link(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::hard_link(src, dst)
}

/// `shutil.copy2`: contents and times.
pub(crate) fn copy2(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst)?;
    let metadata = std::fs::metadata(src)?;
    let times = std::fs::FileTimes::new()
        .set_accessed(metadata.accessed()?)
        .set_modified(metadata.modified()?);
    std::fs::File::options().write(true).open(dst)?.set_times(times)
}

/// `materialize_blob`: put a blob at `destination` as a hard link (a copy where linking
/// fails), unless a verified copy is already there.
pub(crate) fn materialize_blob(blob_path: &Path, destination: &Path, sha256: &str, size: i64, link: LinkFn) -> Result<(), JobError> {
    if destination.exists() && verify_file(destination, sha256, size) {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| os_job_error(&error, parent))?;
    }
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = destination.with_file_name(format!(".{name}.tmp-{}-{}", std::process::id(), random_hex(4)));
    let result = (|| {
        if link(blob_path, &tmp).is_err() {
            copy2(blob_path, &tmp).map_err(|error| os_job_error(&error, blob_path))?;
        }
        if !verify_file(&tmp, sha256, size) {
            return Err(JobError::new(
                "InvalidAssetResponseError",
                format!("Blob materialization verification failed: {}", destination.display()),
            ));
        }
        std::fs::rename(&tmp, destination).map_err(|error| os_job_error(&error, &tmp))
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

fn write_json_file(path: &Path, data: &Value) -> Result<(), JobError> {
    write_json(path, data).map_err(|error| os_job_error(&error, path))
}

/// `_safe_layer_capture_error_message`: one line, paths hidden, at most 240 characters.
fn safe_error_message(message: &str) -> String {
    let text = fav_core::pytext::py_split(message).collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return String::new();
    }
    let tokens: Vec<&str> = text
        .split(' ')
        .map(|token| {
            if token.contains('/') || token.contains('\\') {
                "<path>"
            } else {
                token
            }
        })
        .collect();
    tokens.join(" ").chars().take(240).collect()
}

fn dict_of(entries: Vec<(&str, Value)>) -> Dict {
    let mut dict = Dict::new();
    for (key, value) in entries {
        dict.insert(key, value);
    }
    dict
}

/// `_layer_capture_attempt_summary`.
fn attempt_summary(
    content_id: i128,
    outcome: Vec<(&str, Value)>,
    reuse_decision: Option<&Dict>,
    error: Option<&PyError>,
    previous_capture_error: Option<&PyError>,
) -> Dict {
    let mut summary = dict_of(vec![
        ("schema", Value::Int(1)),
        ("content_id", Value::Int(content_id)),
        ("attempted_at", Value::Str(utc_now_iso())),
    ]);
    for (key, value) in outcome {
        summary.insert(key, value);
    }
    if let Some(decision) = reuse_decision.filter(|decision| !decision.is_empty()) {
        for (field, target) in [
            ("current_fingerprint", "fingerprint"),
            ("previous_fingerprint", "previous_fingerprint"),
            ("previous_status", "previous_status"),
        ] {
            if let Some(value) = decision.get(field).filter(|value| value.is_truthy()) {
                summary.insert(target, value.clone());
            }
        }
    }
    if let Some(error) = error {
        summary.insert("error_class", Value::Str(error.class.clone()));
        summary.insert("error_message", Value::Str(safe_error_message(&error.message)));
    }
    if let Some(error) = previous_capture_error {
        summary.insert("previous_capture_error_class", Value::Str(error.class.clone()));
        summary.insert("previous_capture_error_message", Value::Str(safe_error_message(&error.message)));
    }
    summary
}

fn outcome(status: &str, reason: &str, retryable: bool) -> Vec<(&'static str, Value)> {
    vec![
        ("status", Value::from(status)),
        ("reason", Value::from(reason)),
        ("retryable", Value::Bool(retryable)),
    ]
}

type PreserveKey = (Option<i128>, String, String);

fn preserve_key(model: &Dict) -> PreserveKey {
    (
        to_int(model.get("skin_index")),
        str_or_empty(model, "stable_id"),
        str_or_empty(model, "live2d_key"),
    )
}

fn layer_fields(model: &Dict) -> Vec<Value> {
    LIVE2D_LAYER_METADATA_FIELDS
        .iter()
        .map(|field| model.get(field).cloned().unwrap_or(Value::Null))
        .collect()
}

/// `_preserve_previous_manifest_layer_metadata`: carry the committed layer metadata of the
/// previous manifest over to the freshly parsed models, when it still passes the gates.
fn preserve_previous_manifest_layer_metadata(root: &Path, models: &mut [Dict]) -> Result<Dict, PyError> {
    let manifest_path = root.join("manifest.json");
    let not_preserved = |reason: &str| dict_of(vec![("preserved", Value::Int(0)), ("reason", Value::from(reason))]);
    if !manifest_path.exists() {
        return Ok(not_preserved("no_previous_manifest"));
    }
    let bytes = match std::fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            let mut report = not_preserved("previous_manifest_read_failed");
            report.insert("error_class", Value::from(PyOsError::from_io(&error, Some(&manifest_path)).class));
            return Ok(report);
        }
    };
    // `read_text` raises `UnicodeDecodeError`, which the Python code does not catch.
    let text = crate::pydecode::decode_utf8(&bytes)?;
    let previous = match crate::pydecode::json_loads(text) {
        Ok(previous) => previous,
        Err(error) => {
            let mut report = not_preserved("previous_manifest_read_failed");
            report.insert("error_class", Value::Str(error.class));
            return Ok(report);
        }
    };
    let Value::Dict(previous) = previous else {
        return Ok(not_preserved("previous_manifest_invalid"));
    };
    let summary = match previous.get(LIVE2D_LAYER_CAPTURE_FIELD) {
        Some(Value::Dict(summary)) if matches!(summary.get("status"), Some(Value::Str(status)) if status == "success") => summary.clone(),
        _ => return Ok(not_preserved("previous_layer_capture_not_success")),
    };
    let previous_models: Vec<&Dict> = match previous.get("live2d_models") {
        Some(Value::List(items)) if items.iter().all(|item| matches!(item, Value::Dict(_))) => {
            items.iter().filter_map(Value::as_dict).collect()
        }
        _ => return Ok(not_preserved("previous_manifest_models_invalid")),
    };
    let mut previous_by_key: Vec<(PreserveKey, &Dict)> = Vec::new();
    for model in previous_models {
        let key = preserve_key(model);
        match previous_by_key.iter_mut().find(|(existing, _)| *existing == key) {
            Some(slot) => slot.1 = model,
            None => previous_by_key.push((key, model)),
        }
    }
    let snapshot: Vec<Dict> = models
        .iter()
        .map(|model| {
            let mut fields = Dict::new();
            for field in LIVE2D_LAYER_METADATA_FIELDS {
                if let Some(value) = model.get(field) {
                    fields.insert(field, value.clone());
                }
            }
            fields
        })
        .collect();
    let mut preserved = 0_i128;
    for model in models.iter_mut() {
        let key = preserve_key(model);
        let Some((_, previous_model)) = previous_by_key.iter().find(|(existing, _)| *existing == key) else {
            continue;
        };
        let before = layer_fields(model);
        layer_metadata::copy_live2d_layer_metadata(model, previous_model);
        let after = layer_fields(model);
        if !before.iter().zip(&after).all(|(a, b)| py_eq(a, b)) {
            preserved += 1;
        }
    }
    if preserved == 0 {
        return Ok(not_preserved("no_previous_layer_metadata"));
    }
    let issues = layer_metadata::validate_live2d_layer_metadata(models);
    let errors: Vec<Value> = issues
        .iter()
        .filter(|issue| matches!(issue.get("severity"), Some(Value::Str(severity)) if severity == "error"))
        .cloned()
        .collect();
    if !errors.is_empty() {
        for (model, fields) in models.iter_mut().zip(snapshot) {
            for field in LIVE2D_LAYER_METADATA_FIELDS {
                model.remove(field);
            }
            for (key, value) in fields.iter() {
                model.insert(key.clone(), value.clone());
            }
        }
        let mut report = not_preserved("previous_layer_metadata_failed_validation");
        report.insert("issues", Value::List(errors));
        return Ok(report);
    }
    Ok(dict_of(vec![
        ("preserved", Value::Int(preserved)),
        ("summary", Value::Dict(summary)),
        ("issues", Value::List(issues)),
    ]))
}

// ---------- the job ----------

/// The NIKKE job (`JOB_SPECS['nikke']`).
pub struct NikkeJob {
    ctx: SourceContext,
}

impl std::fmt::Debug for NikkeJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NikkeJob").finish_non_exhaustive()
    }
}

/// Register the job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run(JOB_KEY, move || Box::new(NikkeJob { ctx: ctx.clone() }) as Box<dyn Source>);
}

/// `_http_client()`: `httpx.AsyncClient(base_url=GAMEKEE, follow_redirects=True,
/// headers=request_headers(), timeout=httpx.Timeout(60, connect=20))`.
pub fn client_options() -> ClientOptions {
    ClientOptions {
        headers: request_headers(None),
        timeout: Duration::from_mins(1),
        connect_timeout: Some(Duration::from_secs(20)),
        follow_redirects: true,
        ..ClientOptions::default()
    }
}

impl Source for NikkeJob {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self.ctx.settings.load().await.map_err(|error| settings_job_error(&error))?;
            let http = HttpClient::new(&client_options()).map_err(|error| JobError::new("ValueError", error.to_string()))?;
            let notifier = Arc::new(OutboxNotifier(Outbox::new(self.ctx.db.clone(), Arc::clone(&self.ctx.settings))));
            Nikke::new(settings.web.nikke.clone(), http, self.ctx.db.clone(), notifier)
                .update()
                .await
        })
    }
}

/// `Nikke`: one crawler instance with its settings snapshot, client, pacing and breaker.
pub struct Nikke {
    pub(crate) cfg: settings::Nikke,
    pub(crate) path: PyPath,
    pub(crate) base_url: String,
    http: HttpClient,
    db: Db,
    notifier: Arc<dyn Notifier>,
    pub(crate) sleeper: Sleeper,
    pub(crate) pacer: Sleeper,
    pub(crate) jitter: fn() -> f64,
    pub(crate) capturer: Arc<dyn RuntimeCapturer>,
    pub(crate) link: LinkFn,
    api_limiter: RateLimiter,
    cdn_limiter: RateLimiter,
    cdn_semaphore: tokio::sync::Semaphore,
    limited_failures: AtomicU32,
    notify_discoveries: AtomicBool,
}

impl std::fmt::Debug for Nikke {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nikke").field("path", &self.path).finish_non_exhaustive()
    }
}

impl Nikke {
    /// `Nikke()`: the settings' path, GameKee's API, the real sleeps and the browser
    /// runtime capture.
    pub fn new(cfg: settings::Nikke, http: HttpClient, db: Db, notifier: Arc<dyn Notifier>) -> Self {
        Self {
            path: cfg.path.clone(),
            cfg,
            base_url: GAMEKEE_BASE_URL.to_owned(),
            http,
            db,
            notifier,
            sleeper: tokio_sleeper(),
            pacer: tokio_sleeper(),
            jitter: random_jitter,
            capturer: Arc::new(BrowserCapturer::new()),
            link: hard_link,
            api_limiter: RateLimiter::new(API_REQUEST_INTERVAL_SECONDS),
            cdn_limiter: RateLimiter::new(CDN_REQUEST_INTERVAL_SECONDS),
            cdn_semaphore: tokio::sync::Semaphore::new(CDN_CONCURRENCY),
            limited_failures: AtomicU32::new(0),
            notify_discoveries: AtomicBool::new(false),
        }
    }

    /// Use another runtime capturer (a test double).
    #[must_use]
    pub fn with_capturer(mut self, capturer: Arc<dyn RuntimeCapturer>) -> Self {
        self.capturer = capturer;
        self
    }

    /// The archive root (`cfg.path`).
    pub fn path(&self) -> &PyPath {
        &self.path
    }

    fn url_for(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            return path.to_owned();
        }
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    // ---------- the circuit breaker ----------

    async fn record_limited_failure(&self) {
        let failures = self.limited_failures.fetch_add(1, Ordering::SeqCst) + 1;
        if failures < CIRCUIT_LIMITED_FAILURES {
            return;
        }
        log().warning(format_args!(
            "Nikke CDN/API returned repeated throttling responses; pausing this run for {CIRCUIT_PAUSE_SECONDS:.0}s"
        ));
        (self.sleeper)(CIRCUIT_PAUSE_SECONDS).await;
        self.limited_failures.store(0, Ordering::SeqCst);
    }

    fn record_success(&self) {
        self.limited_failures.store(0, Ordering::SeqCst);
    }

    // ---------- HTTP ----------

    /// `_request(..., bucket='api')`: paced, retried on transport errors and on the statuses
    /// `max_attempts_for_status` allows; any other status is raised at once.
    async fn request(&self, url: &str, headers: Vec<(String, String)>) -> Result<Response, JobError> {
        let url = self.url_for(url);
        let mut last_error: Option<JobError> = None;
        let mut retry_after: Option<String> = None;
        for attempt in 1..=MAX_RETRIES {
            self.api_limiter.wait(&self.pacer).await;
            let mut request = Request::get(&url);
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            match self.http.send(request).await {
                Ok(response) => {
                    let status = response.status();
                    retry_after = response.header("retry-after");
                    if status < 400 {
                        self.record_success();
                        return Ok(response);
                    }
                    let attempts = max_attempts_for_status(status);
                    if is_limited_status(status) {
                        self.record_limited_failure().await;
                    }
                    if attempt >= attempts {
                        let error = response.error_for_status().err().map_or_else(
                            || JobError::new("HTTPStatusError", format!("HTTP {status}")),
                            |error| crate::pyerr::status_error(&error),
                        );
                        return Err(error);
                    }
                    last_error = Some(JobError::new("HTTPStatusError", format!("HTTP {status}")));
                }
                Err(error) => last_error = Some(crate::pyerr::transport_error(&error)),
            }
            if attempt < MAX_RETRIES {
                let delay = retry_delay_seconds(attempt, retry_after.as_deref(), (self.jitter)()).map_err(py_job_error)?;
                (self.sleeper)(delay).await;
            }
        }
        Err(last_error.unwrap_or_else(|| JobError::new("RuntimeError", format!("Failed to request {url}"))))
    }

    async fn body_json(response: Response) -> Result<Value, JobError> {
        let body = response.bytes().await.map_err(|error| crate::pyerr::transport_error(&error))?;
        crate::pydecode::json_from_bytes(&body).map_err(py_job_error)
    }

    /// `_fetch_json`.
    async fn fetch_json(&self, path: &str, content_id: Option<i128>) -> Result<Dict, JobError> {
        let response = self.request(path, request_headers(content_id)).await?;
        if let Err(error) = response.error_for_status() {
            return Err(crate::pyerr::status_error(&error));
        }
        api_payload(Self::body_json(response).await?).map_err(py_job_error)
    }

    /// `_fetch_cdn_json`.
    async fn fetch_cdn_json(&self, url: &str, content_id: i128, label: &str) -> Result<Dict, JobError> {
        let response = self.request(url, cdn_request_headers()).await?;
        match Self::body_json(response).await? {
            Value::Dict(data) => Ok(data),
            _ => Err(JobError::new(
                "TypeError",
                format!("GameKee {label} returned a non-object JSON response for content_id={content_id}"),
            )),
        }
    }

    /// `_load_content_json`: the detail's inline content, else the CDN copy.
    async fn load_content_json(&self, detail: &Dict, content_id: i128) -> Result<Dict, JobError> {
        let content_json = parse_content_json(detail);
        if !content_json.is_empty() {
            return Ok(content_json);
        }
        let url = cdn_json_url(detail, "content_cdn");
        if url.is_empty() {
            return Err(JobError::new(
                "RuntimeError",
                format!("GameKee detail content_json is empty for content_id={content_id}"),
            ));
        }
        let content_json = content_json_from_cdn_payload(&self.fetch_cdn_json(&url, content_id, "content_cdn").await?);
        if content_json.is_empty() {
            return Err(JobError::new(
                "RuntimeError",
                format!("GameKee content_cdn is empty for content_id={content_id}"),
            ));
        }
        Ok(content_json)
    }

    /// `_load_reverse_bind`.
    async fn load_reverse_bind(&self, detail: &Dict, content_id: i128) -> Result<ReverseBind, JobError> {
        let reverse_bind = reverse_bind_map(detail);
        if !reverse_bind.is_empty() {
            return Ok(reverse_bind);
        }
        let url = cdn_json_url(detail, "entry_data_bind_cdn");
        if url.is_empty() {
            return Ok(ReverseBind::new());
        }
        Ok(reverse_bind_from_cdn_payload(
            &self.fetch_cdn_json(&url, content_id, "entry_data_bind_cdn").await?,
        ))
    }

    /// `_fetch_tj_list_rows`.
    async fn fetch_tj_list_rows(&self) -> Result<Vec<Dict>, JobError> {
        let payload = self.fetch_json("/v1/entry/tj-list", None).await?;
        Ok(filter_nikke_rows(payload.get("data")))
    }

    /// `_fetch_detail_response`.
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

    // ---------- notifications ----------

    /// `_notify_content_discovery`: a new character, or new skins of a known one.
    async fn notify_content_discovery(&self, snapshot: &CharacterSnapshot, character: &Dict) {
        if !self.notify_discoveries.load(Ordering::SeqCst) || (snapshot.exists && snapshot.character.is_none()) {
            return;
        }
        let content_id = to_int(character.get("content_id")).unwrap_or_default();
        let character_name = match character.get("title") {
            Some(title) if title.is_truthy() => fav_runtime::pystr::str_value(title),
            _ => content_id.to_string(),
        };
        let fields = (["name", "title"], ["title", "name"]);
        let current = collection_item_names(character.get("skins"), &fields.0, &fields.1, "Skin");
        let (skin_names, discovery) = if snapshot.exists {
            let previous_skins = snapshot.character.as_ref().and_then(|previous| previous.get("skins"));
            let previous = collection_item_names(previous_skins, &fields.0, &fields.1, "Skin");
            (new_collection_item_names(&previous, &current), "skin")
        } else {
            (current.into_iter().map(|(_, name)| name).collect::<Vec<_>>(), "character")
        };
        if snapshot.exists && skin_names.is_empty() {
            return;
        }
        let joined = skin_names.join(", ");
        let body = format!(
            "Character: {character_name}\nSkin: {}",
            if joined.is_empty() { "None" } else { &joined }
        );
        let payload = dict_of(vec![
            ("content_id", Value::Int(content_id)),
            ("character_name", Value::Str(character_name.clone())),
            ("skin_names", Value::List(skin_names.iter().cloned().map(Value::Str).collect())),
            ("discovery", Value::from(discovery)),
        ]);
        let notification = NewNotification {
            kind: "content_discovered".to_owned(),
            source: JOB_KEY.to_owned(),
            header: "Nikke".to_owned(),
            title: format!("New {discovery}: {character_name}"),
            body,
            link_url: format!("{GAMEKEE_BASE_URL}/nikke/tj/{content_id}.html"),
            payload: Some(payload),
            ..NewNotification::default()
        };
        if let Err(error) = self.notifier.enqueue(notification).await {
            log().warning(format_args!(
                "Failed to enqueue NIKKE {discovery} notification for content_id={content_id}: {error}"
            ));
        }
    }

    // ---------- the database ----------

    async fn execute(&self, sql: &str, params: Vec<Param>) -> Result<Vec<sqlx::postgres::PgRow>, JobError> {
        crate::execute(&self.db, sql, params).await
    }

    async fn transaction(&self, statements: Vec<Statement>) -> Result<(), JobError> {
        self.db.query_transaction(statements).await.map_err(|error| db_job_error(&error))?;
        Ok(())
    }

    /// `_upsert_list_pages`: every listed page active, the others inactive.
    async fn upsert_list_pages(&self, rows: &[Dict]) -> Result<(), JobError> {
        let mut statements = Vec::new();
        for row in rows {
            let Some(content_id) = row_content_id(row) else { continue };
            statements.push(Statement::new(
                "INSERT INTO nikke_pages (content_id, name, source_url, active, raw_list_json, first_seen_at, last_seen_at) \
                 VALUES ($1, $2, $3, TRUE, $4::jsonb, NOW(), NOW()) \
                 ON CONFLICT (content_id) DO UPDATE SET name = excluded.name, source_url = excluded.source_url, active = TRUE, \
                 raw_list_json = excluded.raw_list_json, last_seen_at = NOW();",
                vec![
                    Param::from(as_bigint(content_id)?),
                    Param::from(row_name(row)),
                    Param::from(format!("{GAMEKEE_BASE_URL}/nikke/tj/{content_id}.html")),
                    Param::from(json_dumps(&Value::Dict(row.clone()))),
                ],
            ));
        }
        if statements.is_empty() {
            return Ok(());
        }
        statements.insert(0, Statement::new("UPDATE nikke_pages SET active = FALSE;", Vec::new()));
        self.transaction(statements).await
    }

    /// `_upsert_page_fetch_start`: the page is being fetched (`completed_at` cleared).
    async fn upsert_page_fetch_start(
        &self,
        content_id: i128,
        name: &str,
        row: Option<&Dict>,
        detail_response: &Dict,
        manifest_path: &Path,
        hash_value: &str,
    ) -> Result<(), JobError> {
        self.execute(
            "INSERT INTO nikke_pages (content_id, name, source_url, manifest_path, content_hash, active, \
             first_seen_at, last_seen_at, fetched_at, raw_list_json, raw_detail_json) \
             VALUES ($1, $2, $3, $4, $5, TRUE, NOW(), NOW(), NOW(), $6::jsonb, $7::jsonb) \
             ON CONFLICT (content_id) DO UPDATE SET name = excluded.name, source_url = excluded.source_url, \
             manifest_path = excluded.manifest_path, content_hash = excluded.content_hash, active = TRUE, \
             last_seen_at = NOW(), fetched_at = NOW(), completed_at = NULL, raw_list_json = excluded.raw_list_json, \
             raw_detail_json = excluded.raw_detail_json;",
            vec![
                Param::from(as_bigint(content_id)?),
                Param::from(name),
                Param::from(format!("{GAMEKEE_BASE_URL}/nikke/tj/{content_id}.html")),
                Param::from(manifest_path.to_string_lossy().into_owned()),
                Param::from(hash_value),
                Param::from(json_dumps(&Value::Dict(row.cloned().unwrap_or_default()))),
                Param::from(json_dumps(&Value::Dict(detail_response.clone()))),
            ],
        )
        .await?;
        Ok(())
    }

    /// `_upsert_asset_seen`.
    async fn upsert_asset_seen(&self, url: &str) -> Result<(), JobError> {
        self.execute(
            "INSERT INTO nikke_assets (url, normalized_url, status, first_seen_at, last_seen_at) \
             VALUES ($1, $2, 'pending', NOW(), NOW()) \
             ON CONFLICT (url) DO UPDATE SET normalized_url = excluded.normalized_url, last_seen_at = NOW(), \
             status = CASE WHEN nikke_assets.status = 'downloaded' THEN nikke_assets.status ELSE excluded.status END;",
            vec![Param::from(url), Param::from(url)],
        )
        .await?;
        Ok(())
    }

    /// `_resolve_blob_path`: relative blob paths live under the archive root.
    fn resolve_blob_path(&self, blob_path: &str) -> PathBuf {
        let path = PyPath::new(blob_path);
        if path.as_str().starts_with('/') {
            return path.as_path().to_path_buf();
        }
        if path.as_str() == "." {
            return self.path.as_path().to_path_buf();
        }
        self.path.join(path.as_str())
    }

    /// `_completed_blob_for_url`: the verified blob of a downloaded URL; a blob that is gone
    /// or damaged marks the URL missing.
    async fn completed_blob_for_url(&self, url: &str) -> Result<Option<BlobRef>, JobError> {
        use sqlx::Row as _;
        let rows = self
            .execute(
                "SELECT a.sha256, a.size, COALESCE(NULLIF(a.content_type, ''), b.content_type) AS content_type, b.blob_path \
                 FROM nikke_assets AS a JOIN nikke_blobs AS b ON b.sha256 = a.sha256 AND b.size = a.size \
                 WHERE a.url = $1 AND a.status = 'downloaded' AND a.sha256 <> '' LIMIT 1;",
                vec![Param::from(url)],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let sha256: String = row.try_get::<Option<String>, _>("sha256").ok().flatten().unwrap_or_default();
        let size: i64 = row.try_get::<Option<i64>, _>("size").ok().flatten().unwrap_or_default();
        let content_type: String = row.try_get::<Option<String>, _>("content_type").ok().flatten().unwrap_or_default();
        let blob_path = self.resolve_blob_path(&row.try_get::<Option<String>, _>("blob_path").ok().flatten().unwrap_or_default());
        let verified = if sha256.is_empty() || size <= 0 {
            false
        } else {
            let (path, digest) = (blob_path.clone(), sha256.clone());
            tokio::task::spawn_blocking(move || verify_file(&path, &digest, size))
                .await
                .unwrap_or(false)
        };
        if !verified {
            self.execute(
                "UPDATE nikke_assets SET status = 'missing', last_error = 'blob missing or failed verification' WHERE url = $1;",
                vec![Param::from(url)],
            )
            .await?;
            return Ok(None);
        }
        Ok(Some(BlobRef {
            sha256,
            size,
            content_type,
            path: blob_path,
        }))
    }

    /// `_mark_asset_downloaded`.
    async fn mark_asset_downloaded(&self, asset: &Asset, blob: &BlobRef) -> Result<(), JobError> {
        self.execute(
            "UPDATE nikke_assets SET sha256 = $1, size = $2, content_type = $3, status = 'downloaded', \
             failed_count = 0, last_error = '', last_seen_at = NOW() WHERE url = $4;",
            vec![
                Param::from(blob.sha256.clone()),
                Param::from(blob.size),
                Param::from(blob.content_type.clone()),
                Param::from(asset.url.clone()),
            ],
        )
        .await?;
        Ok(())
    }

    /// `_mark_asset_failed`: the reason is cut to 500 characters.
    async fn mark_asset_failed(&self, asset: &Asset, reason: &str) -> Result<(), JobError> {
        self.execute(
            "INSERT INTO nikke_assets (url, normalized_url, status, failed_count, last_error, first_seen_at, last_seen_at) \
             VALUES ($1, $2, 'failed', 1, $3, NOW(), NOW()) \
             ON CONFLICT (url) DO UPDATE SET status = 'failed', failed_count = nikke_assets.failed_count + 1, \
             last_error = excluded.last_error, last_seen_at = NOW();",
            vec![
                Param::from(asset.url.clone()),
                Param::from(asset.url.clone()),
                Param::from(reason.chars().take(500).collect::<String>()),
            ],
        )
        .await?;
        Ok(())
    }

    /// `_register_temp_blob`: keep an existing verified blob with the same hash and size,
    /// else move the download into `_blobs/sha256/<2>/<sha256>`.
    async fn register_temp_blob(&self, temp: TempBlob) -> Result<BlobRef, JobError> {
        use sqlx::Row as _;
        let rows = self
            .execute(
                "SELECT blob_path FROM nikke_blobs WHERE sha256 = $1 AND size = $2 LIMIT 1;",
                vec![Param::from(temp.sha256.clone()), Param::from(temp.size)],
            )
            .await?;
        for row in &rows {
            let existing = self.resolve_blob_path(&row.try_get::<Option<String>, _>("blob_path").ok().flatten().unwrap_or_default());
            let (path, digest, size) = (existing.clone(), temp.sha256.clone(), temp.size);
            if tokio::task::spawn_blocking(move || verify_file(&path, &digest, size))
                .await
                .unwrap_or(false)
            {
                drop(temp.file);
                self.execute(
                    "UPDATE nikke_blobs SET last_verified_at = NOW() WHERE sha256 = $1 AND size = $2;",
                    vec![Param::from(temp.sha256.clone()), Param::from(temp.size)],
                )
                .await?;
                return Ok(BlobRef {
                    sha256: temp.sha256,
                    size: temp.size,
                    content_type: temp.content_type,
                    path: existing,
                });
            }
        }
        let relative = format!("_blobs/sha256/{}/{}", &temp.sha256[..2], temp.sha256);
        let blob_path = self.path.join(&relative);
        if let Some(parent) = blob_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| os_job_error(&error, parent))?;
        }
        let (path, digest, size) = (blob_path.clone(), temp.sha256.clone(), temp.size);
        let present = tokio::task::spawn_blocking(move || path.exists() && verify_file(&path, &digest, size))
            .await
            .unwrap_or(false);
        if present {
            drop(temp.file);
        } else {
            temp.file
                .persist(&blob_path)
                .await
                .map_err(|error| crate::pyerr::download_error(&error))?;
        }
        self.execute(
            "INSERT INTO nikke_blobs (sha256, size, content_type, blob_path, created_at, last_verified_at) \
             VALUES ($1, $2, $3, $4, NOW(), NOW()) \
             ON CONFLICT (sha256, size) DO UPDATE SET content_type = COALESCE(NULLIF(excluded.content_type, ''), nikke_blobs.content_type), \
             blob_path = excluded.blob_path, last_verified_at = NOW();",
            vec![
                Param::from(temp.sha256.clone()),
                Param::from(temp.size),
                Param::from(temp.content_type.clone()),
                Param::from(relative),
            ],
        )
        .await?;
        Ok(BlobRef {
            sha256: temp.sha256,
            size: temp.size,
            content_type: temp.content_type,
            path: blob_path,
        })
    }

    /// One attempt of `_download_asset_to_temp`, inside the CDN semaphore.
    async fn download_attempt(
        &self,
        asset: &Asset,
        tmp_dir: &Path,
        attempt: u32,
        retry_after: &mut Option<String>,
    ) -> Result<TempBlob, AttemptError> {
        let _permit = self
            .cdn_semaphore
            .acquire()
            .await
            .map_err(|error| AttemptError::Fatal(JobError::new("RuntimeError", error.to_string())))?;
        self.cdn_limiter.wait(&self.pacer).await;
        let mut request = Request::get(&asset.url);
        for (name, value) in asset_headers() {
            request = request.header(name, value);
        }
        let mut response = self
            .http
            .send(request)
            .await
            .map_err(|error| AttemptError::Retry(crate::pyerr::transport_error(&error).into()))?;
        let status = response.status();
        *retry_after = response.header("retry-after");
        if status >= 400 {
            let attempts = max_attempts_for_status(status);
            if is_limited_status(status) {
                self.record_limited_failure().await;
            }
            let error = if attempt >= attempts {
                response.error_for_status().err().map_or_else(
                    || JobError::new("HTTPStatusError", format!("HTTP {status}")),
                    |error| crate::pyerr::status_error(&error),
                )
            } else {
                JobError::new("HTTPStatusError", format!("HTTP {status}"))
            };
            return Err(AttemptError::Retry(AssetFailure {
                error,
                missing_upstream: status == 404,
            }));
        }
        let options = TempOptions {
            prefix: ".nikke-download-",
            suffix: "",
            mode: 0o600,
        };
        let file = match fav_net::download::to_temp_file(&mut response, tmp_dir, &options).await {
            Ok(file) => file,
            Err(fav_net::download::DownloadError::Transport(error)) => {
                return Err(AttemptError::Retry(crate::pyerr::transport_error(&error).into()));
            }
            Err(error) => return Err(AttemptError::Fatal(crate::pyerr::download_error(&error))),
        };
        let content_type = response.header("content-type").unwrap_or_default();
        let prefix = {
            use std::io::Read as _;
            let mut prefix = Vec::with_capacity(BODY_PREFIX_LIMIT);
            std::fs::File::open(file.temp_path())
                .and_then(|handle| handle.take(BODY_PREFIX_LIMIT as u64).read_to_end(&mut prefix))
                .map_err(|error| AttemptError::Fatal(os_job_error(&error, file.temp_path())))?;
            prefix
        };
        let invalid = validate_asset_response(&asset.kind, &content_type, &prefix, file.len());
        if !invalid.is_empty() {
            if invalid == "cdn rejection response" {
                self.record_limited_failure().await;
            }
            return Err(AttemptError::Retry(JobError::new("InvalidAssetResponseError", invalid).into()));
        }
        self.record_success();
        Ok(TempBlob {
            sha256: file.sha256_hex().to_owned(),
            size: i64::try_from(file.len()).unwrap_or(i64::MAX),
            content_type,
            file,
        })
    }

    /// `_download_asset_to_temp`: up to three attempts; every status, transport error or
    /// rejected body is retried.
    async fn download_asset_to_temp(&self, asset: &Asset) -> Result<TempBlob, AssetFailure> {
        let tmp_dir = self.path.join("_tmp");
        tokio::fs::create_dir_all(&tmp_dir)
            .await
            .map_err(|error| AssetFailure::from(os_job_error(&error, &tmp_dir)))?;
        let mut last: Option<AssetFailure> = None;
        let mut retry_after: Option<String> = None;
        for attempt in 1..=MAX_RETRIES {
            match self.download_attempt(asset, &tmp_dir, attempt, &mut retry_after).await {
                Ok(blob) => return Ok(blob),
                Err(AttemptError::Fatal(error)) => return Err(error.into()),
                Err(AttemptError::Retry(failure)) => last = Some(failure),
            }
            if attempt < MAX_RETRIES {
                let delay = retry_delay_seconds(attempt, retry_after.as_deref(), (self.jitter)())
                    .map_err(|error| AssetFailure::from(py_job_error(error)))?;
                (self.sleeper)(delay).await;
            }
        }
        Err(last.unwrap_or_else(|| JobError::new("RuntimeError", format!("Failed to download {}", asset.url)).into()))
    }

    async fn store_asset(&self, root: &Path, asset: &mut Asset) -> Result<(), AssetFailure> {
        let blob = if let Some(blob) = self.completed_blob_for_url(&asset.url).await? {
            "reused".clone_into(&mut asset.status);
            blob
        } else {
            let temp = self.download_asset_to_temp(asset).await?;
            let blob = self.register_temp_blob(temp).await?;
            "downloaded".clone_into(&mut asset.status);
            blob
        };
        let destination = root.join(&asset.local_path);
        let (source, target, digest, size, link) = (blob.path.clone(), destination, blob.sha256.clone(), blob.size, self.link);
        tokio::task::spawn_blocking(move || materialize_blob(&source, &target, &digest, size, link))
            .await
            .map_err(|error| AssetFailure::from(JobError::new("RuntimeError", error.to_string())))??;
        asset.sha256.clone_from(&blob.sha256);
        asset.size = blob.size;
        asset.content_type.clone_from(&blob.content_type);
        asset.error = String::new();
        self.mark_asset_downloaded(asset, &blob).await?;
        Ok(())
    }

    /// `_process_asset`: store one asset; a failure is recorded (`missing` for a 404,
    /// else `failed`) and reported as `false`.
    pub(crate) async fn process_asset(&self, root: &Path, asset: &mut Asset) -> Result<bool, JobError> {
        self.upsert_asset_seen(&asset.url).await?;
        match self.store_asset(root, asset).await {
            Ok(()) => Ok(true),
            Err(failure) => {
                let error = if failure.error.message().is_empty() {
                    failure.error.class_name().to_owned()
                } else {
                    failure.error.message().to_owned()
                };
                let status = if failure.missing_upstream { "missing" } else { "failed" };
                status.clone_into(&mut asset.status);
                asset.error.clone_from(&error);
                log().warning(format_args!("Failed to download Nikke asset {}: {error}", asset.url));
                self.mark_asset_failed(asset, &error).await?;
                Ok(false)
            }
        }
    }

    /// `_process_assets`: every asset, three URLs at a time (one URL's kinds in order).
    /// Assets GameKee does not host are listed as missing; any other failure fails the page.
    pub(crate) async fn process_assets(&self, root: &Path, assets: &mut Assets) -> Result<(), JobError> {
        if assets.is_empty() {
            return Ok(());
        }
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (index, asset) in assets.iter().enumerate() {
            match groups.iter_mut().find(|(url, _)| *url == asset.url) {
                Some((_, members)) => members.push(index),
                None => groups.push((asset.url.clone(), vec![index])),
            }
        }
        let items = assets.items_mut();
        let jobs: Vec<Vec<(usize, Asset)>> = groups
            .into_iter()
            .map(|(_, members)| members.into_iter().map(|index| (index, items[index].clone())).collect())
            .collect();
        let results: Vec<Result<Vec<(usize, Asset)>, JobError>> = futures_util::stream::iter(jobs)
            .map(|mut group| async move {
                for (_, asset) in &mut group {
                    self.process_asset(root, asset).await?;
                }
                Ok(group)
            })
            .buffer_unordered(ASSET_PROCESS_CONCURRENCY)
            .collect()
            .await;
        let mut first_error = None;
        for result in results {
            match result {
                Ok(group) => {
                    for (index, asset) in group {
                        items[index] = asset;
                    }
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        let missing: Vec<&Asset> = assets.iter().filter(|asset| asset.status == "missing").collect();
        if !missing.is_empty() {
            log().warning(format_args!(
                "GameKee does not host {} Nikke asset(s), saving the page without them: {}",
                missing.len(),
                missing
                    .iter()
                    .take(3)
                    .map(|asset| asset.url.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let failed: Vec<&Asset> = assets.iter().filter(|asset| asset.status == "failed").collect();
        if !failed.is_empty() {
            let examples = failed.iter().take(3).map(|asset| asset.url.as_str()).collect::<Vec<_>>().join(", ");
            let mut message = format!("{} Nikke assets failed", failed.len());
            if !examples.is_empty() {
                message = format!("{message}: {examples}");
            }
            return Err(JobError::new("AssetProcessingError", message));
        }
        Ok(())
    }

    // ---------- layer metadata ----------

    #[allow(clippy::too_many_lines)] // One branch per outcome of `_merge_live2d_layer_capture`, kept in its order.
    /// `_merge_live2d_layer_capture`: reuse, capture or preserve the layer metadata of a page
    /// with stacked Live2D models; the result's `summary` goes into the manifest.
    pub(crate) async fn merge_live2d_layer_capture(
        &self,
        root: &Path,
        content_id: i128,
        title: &str,
        models: &mut [Dict],
        allow_runtime_capture: bool,
    ) -> Result<Dict, PyError> {
        if !layer_metadata::has_multi_full_layer_groups(models) {
            return Ok(dict_of(vec![
                ("action", Value::from("skipped")),
                ("reason", Value::from("no_multi_full_groups")),
                (
                    "summary",
                    Value::Dict(attempt_summary(
                        content_id,
                        outcome("skipped", "no_multi_full_groups", false),
                        None,
                        None,
                        None,
                    )),
                ),
            ]));
        }

        if !allow_runtime_capture {
            let preserve_report = preserve_previous_manifest_layer_metadata(root, models)?;
            if preserve_report.get("preserved").is_some_and(Value::is_truthy) {
                let mut summary = match preserve_report.get("summary") {
                    Some(Value::Dict(summary)) => summary.clone(),
                    _ => Dict::new(),
                };
                summary.insert("attempted_at", Value::Str(utc_now_iso()));
                summary.insert("action", Value::from("preserved"));
                summary.insert("reason", Value::from("runtime_capture_not_allowed"));
                return Ok(dict_of(vec![
                    ("action", Value::from("preserved")),
                    ("reason", Value::from("runtime_capture_not_allowed")),
                    ("preserve_report", Value::Dict(preserve_report)),
                    ("summary", Value::Dict(summary)),
                ]));
            }
            return Ok(dict_of(vec![
                ("action", Value::from("skipped")),
                ("reason", Value::from("runtime_capture_not_allowed")),
                ("preserve_report", Value::Dict(preserve_report)),
                (
                    "summary",
                    Value::Dict(attempt_summary(
                        content_id,
                        outcome("skipped", "runtime_capture_not_allowed", false),
                        None,
                        None,
                        None,
                    )),
                ),
            ]));
        }

        let mut previous_error: Option<PyError> = None;
        let artifact = match layer_metadata::read_previous_layer_capture_artifact(root) {
            Ok(artifact) => artifact,
            Err(error) => {
                log().warning(format_args!(
                    "Failed to read previous Nikke runtime layer capture for {content_id}: {}",
                    error.message
                ));
                previous_error = Some(error);
                if root.join(LAYER_CAPTURE_RAW_PATH).exists() {
                    layer_metadata::read_previous_layer_capture_manifest_summary(root).ok().flatten()
                } else {
                    None
                }
            }
        };
        let previous_source = artifact.as_ref().map_or("", |artifact| artifact.source);
        let previous = artifact.map(|artifact| artifact.payload);
        let reuse_decision =
            layer_metadata::evaluate_layer_capture_reuse(content_id, models, previous.as_ref(), self.cfg.runtime_capture_force_refresh);
        let reusable = reuse_decision.get("reusable").is_some_and(Value::is_truthy);
        let (capture, action) = if reusable && let Some(previous) = previous {
            (previous, "reused")
        } else if self.cfg.runtime_capture_enabled {
            #[allow(clippy::cast_possible_truncation)] // reason: `int(seconds * 1000)` truncates, as here
            let timeout_ms = (self.cfg.runtime_capture_timeout_seconds * 1000.0) as i64;
            let request = RuntimeCaptureRequest::new(content_id, models.to_vec(), title, timeout_ms);
            match self.capturer.capture(request).await {
                Ok(capture) => (capture, "captured"),
                Err(error) => {
                    log().warning(format_args!(
                        "Failed to capture Nikke runtime layers for {content_id}: {}",
                        error.message
                    ));
                    let summary = attempt_summary(
                        content_id,
                        outcome("failed", "runtime_capture_failed", true),
                        Some(&reuse_decision),
                        Some(&error),
                        previous_error.as_ref(),
                    );
                    return Ok(dict_of(vec![
                        ("action", Value::from("failed")),
                        ("reason", Value::from("runtime_capture_failed")),
                        ("reuse_decision", Value::Dict(reuse_decision)),
                        ("error_class", Value::Str(error.class.clone())),
                        ("error_message", Value::Str(safe_error_message(&error.message))),
                        ("summary", Value::Dict(summary)),
                    ]));
                }
            }
        } else {
            let summary = attempt_summary(
                content_id,
                outcome("skipped", "runtime_capture_disabled", false),
                Some(&reuse_decision),
                None,
                previous_error.as_ref(),
            );
            return Ok(dict_of(vec![
                ("action", Value::from("skipped")),
                ("reason", Value::from("runtime_capture_disabled")),
                ("reuse_decision", Value::Dict(reuse_decision)),
                ("summary", Value::Dict(summary)),
            ]));
        };

        let merge_report = layer_metadata::merge_live2d_layer_captures(models, &capture, Some(content_id), false);
        let mut summary = layer_metadata::layer_capture_manifest_summary(&capture, &merge_report);
        if action == "reused"
            && previous_source == "manifest_summary"
            && let Some(Value::Str(hash)) = capture.get("capture_hash")
            && !py_strip(hash).is_empty()
        {
            summary.insert("capture_hash", Value::Str(py_strip(hash).to_owned()));
        }
        summary.insert("attempted_at", Value::Str(utc_now_iso()));
        summary.insert("action", Value::from(action));
        if let Some(error) = &previous_error {
            summary.insert("previous_capture_error_class", Value::Str(error.class.clone()));
            summary.insert("previous_capture_error_message", Value::Str(safe_error_message(&error.message)));
        }
        let reason = value_or_empty(&reuse_decision, "reason");
        Ok(dict_of(vec![
            ("action", Value::from(action)),
            ("reason", reason),
            ("reuse_decision", Value::Dict(reuse_decision)),
            ("merge_report", Value::Dict(merge_report)),
            ("capture_payload", Value::Dict(capture)),
            (
                "capture_payload_source",
                Value::from(if action == "reused" { previous_source } else { "runtime" }),
            ),
            ("persist_raw_capture", Value::Bool(action == "captured")),
            ("summary", Value::Dict(summary)),
        ]))
    }

    /// `_read_known_atlas_text`.
    async fn read_known_atlas_text(&self, atlas_url: &str) -> Result<Option<String>, JobError> {
        let Some(blob) = self.completed_blob_for_url(atlas_url).await? else {
            return Ok(None);
        };
        Ok(tokio::fs::read(&blob.path)
            .await
            .ok()
            .map(|bytes| bytes.utf8_chunks().map(|chunk| chunk.valid()).collect()))
    }

    /// `_expand_atlas_textures`: store each atlas first, then add the textures it names.
    pub(crate) async fn expand_atlas_textures(&self, root: &Path, assets: &mut Assets, models: &mut [Dict]) -> Result<(), JobError> {
        for model in models.iter_mut() {
            let atlas_url = match model.get("urls") {
                Some(Value::Dict(urls)) => match urls.get("atlas") {
                    Some(Value::Str(atlas)) if !atlas.is_empty() => atlas.clone(),
                    _ => continue,
                },
                _ => continue,
            };
            if let Some(atlas_asset) = assets.get_mut(&atlas_url, "live2d_atlas") {
                let mut asset = atlas_asset.clone();
                self.process_asset(root, &mut asset).await?;
                if let Some(slot) = assets.get_mut(&atlas_url, "live2d_atlas") {
                    *slot = asset;
                }
            }
            let Some(atlas_text) = self.read_known_atlas_text(&atlas_url).await? else {
                log().warning(format_args!(
                    "Failed to inspect atlas textures for {atlas_url}: atlas blob is unavailable"
                ));
                continue;
            };
            let textures = parse_spine_atlas_textures(&atlas_text);
            if textures.is_empty() {
                continue;
            }
            let mut texture_urls = Vec::with_capacity(textures.len());
            for texture in &textures {
                let joined =
                    fav_core::pyurl::urljoin(&atlas_url, texture).map_err(|error| JobError::new("ValueError", error.to_string()))?;
                texture_urls.push(normalize_url(&joined));
            }
            if let Some(Value::Dict(urls)) = model.get_mut("urls") {
                urls.insert(
                    "atlas_textures",
                    Value::List(texture_urls.iter().cloned().map(Value::Str).collect()),
                );
            }
            let context = dict_of(vec![
                ("section", value_or_empty(model, "section")),
                ("row_index", model.get("row_index").cloned().unwrap_or(Value::Null)),
                ("label", value_or_empty(model, "label")),
                ("key", value_or_empty(model, "key")),
                ("stable_id", value_or_empty(model, "stable_id")),
                ("skin_index", model.get("skin_index").cloned().unwrap_or(Value::Null)),
                ("skin_name", value_or_empty(model, "skin_name")),
                ("skin_title", value_or_empty(model, "skin_title")),
                ("skin_series", value_or_empty(model, "skin_series")),
                ("skin_obtain", value_or_empty(model, "skin_obtain")),
                (
                    "is_collection_skin",
                    Value::Bool(model.get("is_collection_skin").is_some_and(Value::is_truthy)),
                ),
                ("live2d_key", value_or_empty(model, "live2d_key")),
                ("live2d_field", Value::from("atlas_texture")),
            ]);
            for texture_url in &texture_urls {
                super::add_asset(assets, texture_url, "live2d_texture", context.clone());
            }
        }
        Ok(())
    }

    /// `_replace_page_assets_and_mark_completed`: one transaction.
    async fn replace_page_assets_and_mark_completed(&self, content_id: i128, assets: &Assets) -> Result<(), JobError> {
        let id = as_bigint(content_id)?;
        let mut statements = vec![Statement::new(
            "DELETE FROM nikke_page_assets WHERE content_id = $1;",
            vec![Param::from(id)],
        )];
        for asset in assets.iter() {
            let original_filename = original_filename_from_url(&asset.url, &asset.kind).map_err(py_job_error)?;
            let empty = [Dict::new()];
            let contexts: &[Dict] = if asset.contexts.is_empty() { &empty } else { &asset.contexts };
            for context in contexts {
                statements.push(Statement::new(
                    "INSERT INTO nikke_page_assets (content_id, url, kind, context_hash, local_path, original_filename, context_json, \
                     created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, NOW(), NOW()) \
                     ON CONFLICT (content_id, url, kind, context_hash) DO UPDATE SET local_path = excluded.local_path, \
                     original_filename = excluded.original_filename, context_json = excluded.context_json, updated_at = NOW();",
                    vec![
                        Param::from(id),
                        Param::from(asset.url.clone()),
                        Param::from(asset.kind.clone()),
                        Param::from(context_hash(context)),
                        Param::from(asset.local_path.clone()),
                        Param::from(original_filename.clone()),
                        Param::from(json_dumps(&Value::Dict(context.clone()))),
                    ],
                ));
            }
        }
        statements.push(Statement::new(
            "UPDATE nikke_pages SET completed_at = NOW() WHERE content_id = $1;",
            vec![Param::from(id)],
        ));
        self.transaction(statements).await
    }

    // ---------- pages ----------

    /// `_crawl_page`: fetch, parse, store and describe one character page.
    pub(crate) async fn crawl_page(
        &self,
        tj_list_row: Option<&Dict>,
        content_id: i128,
        skip_assets: bool,
        allow_runtime_capture: Option<bool>,
    ) -> Result<PathBuf, JobError> {
        let detail_response = self.fetch_detail_response(content_id).await?;
        let detail = match detail_response.get("data") {
            Some(Value::Dict(detail)) => detail.clone(),
            _ => Dict::new(),
        };
        let content_json = self.load_content_json(&detail, content_id).await?;
        let reverse_bind = self.load_reverse_bind(&detail, content_id).await?;
        let content_summary = summarize_content(&content_json, &reverse_bind);
        let base_info = base_info_from_summary(&content_summary);
        let (mut assets, mut models) = extract_resources(&content_json, tj_list_row, &reverse_bind);

        let name = detail_name(&detail, content_id, tj_list_row);
        let root = self.path.join(output_dir_name(&detail, content_id, tj_list_row));
        let previous_character = load_character_snapshot(self.path.as_path(), &root, content_id);
        let manifest_path = root.join("manifest.json");
        let hash_value = content_hash(&detail_response, &content_json, tj_list_row);
        self.upsert_page_fetch_start(content_id, &name, tj_list_row, &detail_response, &manifest_path, &hash_value)
            .await?;

        write_json_file(&root.join("raw/detail.json"), &Value::Dict(detail_response.clone()))?;
        write_json_file(&root.join("raw/content.json"), &Value::Dict(content_json.clone()))?;
        let row_value = tj_list_row.map_or(Value::Null, |row| Value::Dict(row.clone()));
        write_json_file(&root.join("raw/tj-list-row.json"), &row_value)?;

        assign_asset_paths(&mut assets, content_id).map_err(py_job_error)?;
        if !skip_assets {
            self.expand_atlas_textures(&root, &mut assets, &mut models).await?;
            assign_asset_paths(&mut assets, content_id).map_err(py_job_error)?;
        }

        let allow = allow_runtime_capture.map_or(!skip_assets, |allow| allow && !skip_assets);
        let layer_result = self
            .merge_live2d_layer_capture(&root, content_id, &name, &mut models, allow)
            .await
            .map_err(py_job_error)?;
        if let Some(Value::Dict(payload)) = layer_result.get("capture_payload")
            && matches!(layer_result.get("persist_raw_capture"), Some(Value::Bool(true)))
        {
            write_json_file(&root.join(LAYER_CAPTURE_RAW_PATH), &Value::Dict(payload.clone()))?;
        }
        let layer_summary = match layer_result.get("summary") {
            Some(Value::Dict(summary)) => Some(summary.clone()),
            _ => None,
        };

        if !skip_assets {
            self.process_assets(&root, &mut assets).await?;
        }

        let models_value = Value::List(models.iter().cloned().map(Value::Dict).collect());
        let skins = content_summary.get("skins").cloned().unwrap_or(Value::List(Vec::new()));
        let mut manifest = dict_of(vec![
            ("source_url", Value::Str(format!("{GAMEKEE_BASE_URL}/nikke/tj/{content_id}.html"))),
            (
                "detail_api_url",
                Value::Str(format!("{GAMEKEE_BASE_URL}/v1/content/detail/{content_id}")),
            ),
            ("fetched_at", Value::Str(utc_now_iso())),
            ("content_id", Value::Int(content_id)),
            ("title", Value::Str(name.clone())),
            ("entry_id", detail.get("entry_id").cloned().unwrap_or(Value::Null)),
            ("updated_at", detail.get("updated_at").cloned().unwrap_or(Value::Null)),
            ("tj_list", row_value.clone()),
            ("base_info", Value::Dict(base_info.clone())),
            ("content_summary", Value::Dict(content_summary)),
            ("live2d_models", models_value.clone()),
            ("assets", relative_manifest_assets(&assets)),
            ("asset_counts", Value::Dict(super::asset_counts(&assets))),
        ]);
        if let Some(summary) = &layer_summary {
            manifest.insert(LIVE2D_LAYER_CAPTURE_FIELD, Value::Dict(summary.clone()));
        }
        write_json_file(&manifest_path, &Value::Dict(manifest))?;
        let mut character = dict_of(vec![
            ("content_id", Value::Int(content_id)),
            ("title", Value::Str(name)),
            ("entry_id", detail.get("entry_id").cloned().unwrap_or(Value::Null)),
            ("tj_list", row_value),
            ("base_info", Value::Dict(base_info)),
            ("skins", skins),
            ("live2d_models", models_value),
        ]);
        if let Some(summary) = layer_summary {
            character.insert(LIVE2D_LAYER_CAPTURE_FIELD, Value::Dict(summary));
        }
        write_json_file(&root.join("character.json"), &Value::Dict(character.clone()))?;
        if !skip_assets {
            self.replace_page_assets_and_mark_completed(content_id, &assets).await?;
        }
        self.notify_content_discovery(&previous_character, &character).await;
        Ok(root)
    }

    /// `download_character`: one page by id or URL, outside the schedule. Runtime capture
    /// is allowed when assets are downloaded.
    pub async fn download_character(&self, target: &str, skip_assets: bool) -> Result<PathBuf, JobError> {
        let content_id = parse_content_id(target).map_err(py_job_error)?;
        let root = self.path.as_path().to_path_buf();
        tokio::fs::create_dir_all(&root)
            .await
            .map_err(|error| os_job_error(&error, &root))?;
        let rows = self.fetch_tj_list_rows().await?;
        let row = rows
            .iter()
            .find(|row| row_content_id(row) == Some(content_id))
            .cloned()
            .unwrap_or_else(|| dict_of(vec![("content_id", Value::Int(content_id))]));
        if rows.is_empty() {
            self.upsert_list_pages(std::slice::from_ref(&row)).await?;
        } else {
            self.upsert_list_pages(&rows).await?;
        }
        self.crawl_page(Some(&row), content_id, skip_assets, Some(!skip_assets)).await
    }

    /// `update()`: every listed page, without runtime capture; a failed page is reported
    /// at the end and does not stop the others.
    pub async fn update(&self) -> Result<(), JobError> {
        let Some(lock) = self.db.try_advisory_lock(JOB_KEY).await.map_err(|error| db_job_error(&error))? else {
            log().info("Nikke update skipped because another run holds the advisory lock");
            return Ok(());
        };
        let result = self.update_locked().await;
        lock.release().await;
        result
    }

    async fn update_locked(&self) -> Result<(), JobError> {
        let root = self.path.as_path().to_path_buf();
        self.notify_discoveries.store(has_character_snapshot(&root), Ordering::SeqCst);
        tokio::fs::create_dir_all(&root)
            .await
            .map_err(|error| os_job_error(&error, &root))?;
        let rows = self.fetch_tj_list_rows().await?;
        if rows.is_empty() {
            log().warning("GameKee NIKKE list returned no character rows");
            return Ok(());
        }
        self.upsert_list_pages(&rows).await?;
        let total = rows.len();
        log().info(format_args!("Found {total} GameKee NIKKE character pages"));
        let mut failed: Vec<String> = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let Some(content_id) = row_content_id(row) else { continue };
            log().info(format_args!("Crawling Nikke {content_id} ({}/{total})", index + 1));
            if let Err(error) = self.crawl_page(Some(row), content_id, false, Some(false)).await {
                log().error(format_args!("Failed to crawl Nikke content_id={content_id}\n{}", error.formatted()));
                failed.push(content_id.to_string());
            }
        }
        if !failed.is_empty() {
            let shown: Vec<&str> = failed.iter().take(10).map(String::as_str).collect();
            return Err(JobError::new(
                "CrawlRunError",
                format!("{} Nikke page(s) failed: {}", failed.len(), shown.join(", ")),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests;
