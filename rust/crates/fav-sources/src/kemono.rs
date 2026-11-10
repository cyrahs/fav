//! Kemono-style archives (pawchive): every configured creator's new posts and their
//! files.
//!
//! Python origin: `src/web/kemono.py`. Posts are listed newest first, 50 a page, until
//! the first one already in `kemono`; new posts are downloaded oldest first and recorded
//! only once their files are on disk, so a failure resumes at the same post next run.
//! Files whose path stem is a sha256 are verified; a 404 is a file the archive never
//! stored. Preview-only attachments get the thumbnail and a row in
//! `kemono_pending_originals`, rechecked after 1, 3, 5 and then every 7 days until the
//! original turns up. Every request is paced by `sleep_request_seconds`, and throttling
//! and transport failures go through a 2/5/15/30-second ladder (or `Retry-After`, capped
//! at 300 seconds).
//!
//! Differences (DIVERGENCES.md D39-D43): no tqdm progress bar; `_ensure_table` is the
//! consolidated schema (D6); a non-string title, publish date or file path that reaches the
//! database is stored as its `str()` text.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use chrono::Utc;
use fav_core::filename::sanitize;
use fav_core::logging::{self, Logger};
use fav_core::pyjson::{Dict, Value};
use fav_core::pypath;
use fav_core::settings;
use fav_db::{Db, Param};
use fav_net::download::{self, TempOptions};
use fav_net::retry::{Sleeper, ladder_delay, tokio_sleeper};
use fav_net::{ClientOptions, ErrorClass, HttpClient, TransportError};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox};
use fav_runtime::pystr::{repr_str, str_value};
use fav_runtime::source::{JobError, Registry, Source};
use regex::Regex;
use sqlx::Row as _;

use crate::{SourceContext, execute, pyerr, settings_job_error};

pub const API_PREFIX: &str = "/api/v1";
pub const PAGE_SIZE: usize = 50;
/// The archive's operator asks tools to identify themselves (2026-09-01 notice); its
/// file host refuses browser user agents that are not real browsers.
pub const DEFAULT_USER_AGENT: &str = "fav/1.0 (personal archiver; read your file-server notice and respecting it, thank you)";
pub const API_RETRY_DELAYS_SECONDS: [f64; 4] = [2.0, 5.0, 15.0, 30.0];
/// 403 is deliberately absent: behind Cloudflare it means blocked.
const RETRYABLE_STATUS_CODES: [u16; 4] = [429, 502, 503, 504];
const RETRY_AFTER_MAX_DELAY_SECONDS: f64 = 300.0;
const HTTP_NOT_FOUND: u16 = 404;
/// Days between rechecks of a preview-only attachment's original; the last repeats.
pub const ORIGINAL_RECHECK_GAP_DAYS: [i64; 4] = [1, 3, 5, 7];

static SHA256_STEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{64}$").expect("valid pattern"));

fn log() -> Logger {
    logging::get("kemono")
}

/// One file of a post.
#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    /// The listing's raw value: Python keeps whatever the archive sent and fails only
    /// where it uses the value as a path.
    pub name: Value,
    pub path: Value,
    /// The archive never imported the original; only a compressed thumbnail exists.
    pub preview_only: bool,
}

/// One post of a creator's listing.
#[derive(Debug, Clone, PartialEq)]
pub struct Post {
    pub id: String,
    pub user: String,
    pub service: String,
    /// The listing's raw `title or 'untitled'`.
    pub title: Value,
    /// The listing's raw `published`, `Null` when absent.
    pub published: Value,
    pub attachments: Vec<Attachment>,
}

/// `Path(value)`: only a string is a path.
fn path_text(value: &Value) -> Result<&str, JobError> {
    match value {
        Value::Str(text) => Ok(text),
        other => Err(JobError::new(
            "TypeError",
            format!(
                "argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{}'",
                pyerr::type_name(other)
            ),
        )),
    }
}

impl Attachment {
    /// The path as an f-string or `%s` shows it.
    pub fn path_label(&self) -> String {
        str_value(&self.path)
    }

    /// `attachment_filename(attachment.name, idx)`.
    pub fn file_name(&self, idx: usize) -> Result<String, JobError> {
        Ok(attachment_filename(path_text(&self.name)?, idx))
    }

    /// `sha256_from_path(attachment.path)`.
    pub fn sha256(&self) -> Result<Option<String>, JobError> {
        Ok(sha256_from_path(path_text(&self.path)?))
    }
}

impl Post {
    /// `sanitize(post.title, max_bytes=80)`: `re.sub` refuses anything but a string.
    fn sanitized_title(&self) -> Result<String, JobError> {
        match &self.title {
            Value::Str(title) => Ok(sanitize(title, 80)),
            other => Err(JobError::new(
                "TypeError",
                format!("expected string or bytes-like object, got '{}'", pyerr::type_name(other)),
            )),
        }
    }

    /// `post.published or ''` as the row stores it.
    fn published_text(&self) -> String {
        if self.published.is_truthy() {
            str_value(&self.published)
        } else {
            String::new()
        }
    }
}

/// `should_retry_status(status_code)`.
pub fn should_retry_status(status_code: u16) -> bool {
    RETRYABLE_STATUS_CODES.contains(&status_code)
}

/// `should_retry_exception(exc)`: network errors (a connection dropped mid-request
/// included), timeouts and a server closing the connection.
pub fn should_retry_exception(class: ErrorClass) -> bool {
    class.is_network() || class.is_timeout() || class == ErrorClass::RemoteProtocolError
}

/// `retry_after_seconds(response)` for the response's `Retry-After` value.
pub fn retry_after_seconds(value: Option<&str>) -> Result<Option<f64>, JobError> {
    fav_net::retry::retry_after_seconds(value, Utc::now()).map_err(|message| JobError::new("ValueError", message))
}

/// `retry_delay_seconds(attempt, retry_after)`.
pub fn retry_delay_seconds(attempt: usize, retry_after: Option<f64>) -> f64 {
    ladder_delay(&API_RETRY_DELAYS_SECONDS, attempt, retry_after, RETRY_AFTER_MAX_DELAY_SECONDS)
}

/// `sha256_from_path(path)`: archives name stored files by their sha256.
pub fn sha256_from_path(path: &str) -> Option<String> {
    let stem = pypath::stem(path).to_lowercase();
    SHA256_STEM.is_match(&stem).then_some(stem)
}

/// `attachment_filename(name, idx)`.
pub fn attachment_filename(name: &str, idx: usize) -> String {
    let ext = pypath::suffix(name);
    let stem = sanitize(pypath::stem(name), 150);
    if stem.is_empty() {
        format!("file_{idx}{ext}")
    } else {
        format!("{stem}{ext}")
    }
}

/// `build_file_url(file_base_url, path)`.
pub fn build_file_url(file_base_url: &str, path: &str) -> String {
    format!("{}/data{path}", file_base_url.trim_end_matches('/'))
}

/// `build_thumbnail_url(thumbnail_base_url, path)`.
pub fn build_thumbnail_url(thumbnail_base_url: &str, path: &str) -> String {
    format!("{}/thumbnail/data{path}", thumbnail_base_url.trim_end_matches('/'))
}

/// `for item in value`: a list's items, an object's keys, a string's characters.
fn iterate(value: &Value) -> Result<Vec<Value>, JobError> {
    match value {
        Value::List(items) => Ok(items.clone()),
        Value::Dict(dict) => Ok(dict.iter().map(|(key, _)| Value::Str(fav_runtime::pystr::str_key(key))).collect()),
        Value::Str(text) => Ok(text.chars().map(|ch| Value::Str(ch.to_string())).collect()),
        other => Err(JobError::new(
            "TypeError",
            format!("'{}' object is not iterable", pyerr::type_name(other)),
        )),
    }
}

fn as_dict<'a>(value: &'a Value, empty: &'a Dict) -> Result<&'a Dict, JobError> {
    match value {
        Value::Dict(dict) => Ok(dict),
        // `x or {}`: a falsy value is the empty object.
        other if !other.is_truthy() => Ok(empty),
        other => Err(pyerr::no_attribute(other, "get")),
    }
}

/// An attachment from a `file` or `attachments` entry, or `None` without a path.
fn parse_attachment(item: &Dict) -> Result<Option<Attachment>, JobError> {
    let Some(path) = item.get("path").filter(|path| path.is_truthy()) else {
        return Ok(None);
    };
    let name = match item.get("name").filter(|name| name.is_truthy()) {
        Some(name) => name.clone(),
        None => Value::Str(pypath::name(path_text(path)?).to_owned()),
    };
    Ok(Some(Attachment {
        name,
        path: path.clone(),
        preview_only: item.get("preview_only").is_some_and(Value::is_truthy),
    }))
}

fn required_str(data: &Dict, key: &str) -> Result<String, JobError> {
    data.get(key).map(str_value).ok_or_else(|| JobError::new("KeyError", repr_str(key)))
}

/// `parse_post(data)`: the post's `file` first, then its `attachments`, each kept only
/// with a path.
pub fn parse_post(data: &Value) -> Result<Post, JobError> {
    let Value::Dict(data) = data else {
        return Err(pyerr::no_attribute(data, "get"));
    };
    let empty = Dict::new();
    let mut attachments = Vec::new();
    let main_file = as_dict(data.get("file").unwrap_or(&Value::Null), &empty)?;
    if let Some(attachment) = parse_attachment(main_file)? {
        attachments.push(attachment);
    }
    let items = match data.get("attachments") {
        Some(value) => iterate(value)?,
        None => Vec::new(),
    };
    for item in &items {
        let Value::Dict(item) = item else {
            return Err(pyerr::no_attribute(item, "get"));
        };
        if let Some(attachment) = parse_attachment(item)? {
            attachments.push(attachment);
        }
    }
    let id = required_str(data, "id")?;
    let user = required_str(data, "user")?;
    let service = required_str(data, "service")?;
    let title = match data.get("title").filter(|title| title.is_truthy()) {
        Some(title) => title.clone(),
        None => Value::Str("untitled".to_owned()),
    };
    Ok(Post {
        id,
        user,
        service,
        title,
        published: data.get("published").cloned().unwrap_or(Value::Null),
        attachments,
    })
}

/// `collect_new_posts(page, downloaded_ids)`: the not-yet-archived prefix of a
/// newest-first page, and whether an archived post ended it.
pub fn collect_new_posts<S: std::hash::BuildHasher>(page: &[Post], downloaded_ids: &HashSet<String, S>) -> (Vec<Post>, bool) {
    let mut fresh = Vec::new();
    for post in page {
        if downloaded_ids.contains(&post.id) {
            return (fresh, true);
        }
        fresh.push(post.clone());
    }
    (fresh, false)
}

/// A creator's numbers for the summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CreatorStats {
    pub new_posts: i64,
    pub downloaded_files: i64,
    pub preview_files: i64,
    pub missing_files: i64,
    pub creator_missing: bool,
}

/// `_DownloadOutcome`.
#[derive(Debug)]
enum Outcome {
    Ok,
    Missing,
    Retry { detail: String, retry_after: Option<f64> },
}

/// What an attempt raised: a transport error the ladder may retry, or anything else.
enum AttemptError {
    Transport(TransportError),
    Other(JobError),
}

impl AttemptError {
    fn into_job_error(self) -> JobError {
        match self {
            Self::Transport(error) => pyerr::transport_error(&error),
            Self::Other(error) => error,
        }
    }
}

/// An API answer, its body read as httpx reads it inside `client.get`.
struct ApiResponse {
    status: u16,
    body: bytes::Bytes,
}

// ---------- the job ----------

/// The Kemono job (`JOB_SPECS['kemono']`).
pub struct Kemono {
    ctx: SourceContext,
}

impl std::fmt::Debug for Kemono {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kemono").finish_non_exhaustive()
    }
}

impl Kemono {
    pub fn new(ctx: SourceContext) -> Self {
        Self { ctx }
    }
}

/// Register the job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run("kemono", move || Box::new(Kemono::new(ctx.clone())));
}

/// `httpx.AsyncClient(...)` as `Kemono.__init__` builds it (the base URL is joined per
/// request).
pub fn client_options() -> ClientOptions {
    ClientOptions {
        headers: vec![
            ("User-Agent".to_owned(), DEFAULT_USER_AGENT.to_owned()),
            ("Accept".to_owned(), "application/json".to_owned()),
        ],
        timeout: Duration::from_mins(1),
        connect_timeout: Some(Duration::from_secs(15)),
        follow_redirects: true,
        max_keepalive_connections: 10,
        ..ClientOptions::default()
    }
}

impl Source for Kemono {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self.ctx.settings.load().await.map_err(|error| settings_job_error(&error))?;
            let http = HttpClient::new(&client_options()).map_err(|error| JobError::new("ValueError", error.to_string()))?;
            let run = Run {
                cfg: settings.web.kemono.clone(),
                http,
                db: self.ctx.db.clone(),
                outbox: Outbox::new(self.ctx.db.clone(), std::sync::Arc::clone(&self.ctx.settings)),
                sleeper: tokio_sleeper(),
            };
            run.update().await
        })
    }
}

/// One run: the settings snapshot and everything the run talks to.
pub(crate) struct Run {
    pub(crate) cfg: settings::Kemono,
    pub(crate) http: HttpClient,
    pub(crate) db: Db,
    pub(crate) outbox: Outbox,
    pub(crate) sleeper: Sleeper,
}

fn io_error(error: &std::io::Error, path: &Path) -> JobError {
    let os = fav_net::download::PyOsError::from_io(error, Some(path));
    JobError::new(os.class, os.message)
}

async fn create_dir_all(dir: &Path) -> Result<(), JobError> {
    tokio::fs::create_dir_all(dir).await.map_err(|error| io_error(&error, dir))
}

impl Run {
    fn root(&self) -> PathBuf {
        self.cfg.path.as_path().to_path_buf()
    }

    /// `_pace`.
    async fn pace(&self) {
        if self.cfg.sleep_request_seconds > 0.0 {
            (self.sleeper)(self.cfg.sleep_request_seconds).await;
        }
    }

    /// The client's `base_url` joined with an API path, as httpx merges them.
    fn api_url(&self, path: &str, offset: usize) -> String {
        let base = &self.cfg.base_url;
        let separator = if base.ends_with('/') { "" } else { "/" };
        format!("{base}{separator}{}?o={offset}", path.trim_start_matches('/'))
    }

    async fn downloaded_ids(&self, service: &str, user_id: &str) -> Result<HashSet<String>, JobError> {
        let rows = execute(
            &self.db,
            "SELECT id FROM kemono WHERE service = $1 AND user_id = $2;",
            vec![Param::from(service), Param::from(user_id)],
        )
        .await?;
        Ok(rows.iter().map(|row| row.get::<String, _>("id")).collect())
    }

    /// `_get_api`: GET with the retry ladder; a 200 or 404 is returned, anything else
    /// raised.
    async fn get_api(&self, path: &str, offset: usize) -> Result<ApiResponse, JobError> {
        let attempts = API_RETRY_DELAYS_SECONDS.len() + 1;
        let url = self.api_url(path, offset);
        for attempt in 1..=attempts {
            self.pace().await;
            let fetched = match self.http.get(&url).await {
                Ok(response) => {
                    let status = response.status();
                    let retry_after = response.header("retry-after");
                    let check = response.error_for_status();
                    response.bytes().await.map(|body| (status, retry_after, check, body))
                }
                Err(error) => Err(error),
            };
            let (status, retry_after, check, body) = match fetched {
                Ok(fetched) => fetched,
                Err(error) => {
                    if !should_retry_exception(error.class) || attempt >= attempts {
                        return Err(pyerr::transport_error(&error));
                    }
                    let delay = retry_delay_seconds(attempt, None);
                    log().warning(format!(
                        "Kemono API request failed {path} attempt={attempt}/{attempts}, retry in {delay:.1}s: {}: {}",
                        error.class.name(),
                        error.message
                    ));
                    (self.sleeper)(delay).await;
                    continue;
                }
            };
            if should_retry_status(status) && attempt < attempts {
                let delay = retry_delay_seconds(attempt, retry_after_seconds(retry_after.as_deref())?);
                log().warning(format!(
                    "Kemono API returned HTTP {status} for {path} attempt={attempt}/{attempts}, retry in {delay:.1}s"
                ));
                (self.sleeper)(delay).await;
                continue;
            }
            if status != HTTP_NOT_FOUND {
                check.map_err(|error| pyerr::status_error(&error))?;
            }
            return Ok(ApiResponse { status, body });
        }
        Err(JobError::new("RuntimeError", "Kemono API request failed without response"))
    }

    /// `_fetch_new_posts`: newest first until the first archived post; `None` when the
    /// creator does not exist upstream.
    pub(crate) async fn fetch_new_posts(
        &self,
        service: &str,
        user_id: &str,
        downloaded_ids: &HashSet<String>,
    ) -> Result<Option<Vec<Post>>, JobError> {
        let mut new_posts = Vec::new();
        let mut offset = 0;
        let path = format!("{API_PREFIX}/{service}/user/{user_id}");
        loop {
            let response = self.get_api(&path, offset).await?;
            if response.status == HTTP_NOT_FOUND {
                // At offset 0 the whole creator is missing; past it, the listing ended.
                return Ok(if offset == 0 { None } else { Some(new_posts) });
            }
            let page = iterate(&pyerr::json_loads_bytes(&response.body)?)?
                .iter()
                .map(parse_post)
                .collect::<Result<Vec<_>, _>>()?;
            let (fresh, hit_known) = collect_new_posts(&page, downloaded_ids);
            new_posts.extend(fresh);
            if hit_known || page.len() < PAGE_SIZE {
                return Ok(Some(new_posts));
            }
            offset += PAGE_SIZE;
        }
    }

    /// `_attempt_download`: one GET of a file into a temporary file next to `dst`,
    /// verified when the expected sha256 is known, then moved into place.
    async fn attempt_download(
        &self,
        src_url: &str,
        dst: &Path,
        expected_sha256: Option<&str>,
        desc: &str,
    ) -> Result<Outcome, AttemptError> {
        let mut response = self.http.get(src_url).await.map_err(AttemptError::Transport)?;
        let status = response.status();
        if status == HTTP_NOT_FOUND {
            return Ok(Outcome::Missing);
        }
        if should_retry_status(status) {
            let retry_after = retry_after_seconds(response.header("retry-after").as_deref()).map_err(AttemptError::Other)?;
            return Ok(Outcome::Retry {
                detail: format!("HTTP {status}"),
                retry_after,
            });
        }
        response
            .error_for_status()
            .map_err(|error| AttemptError::Other(pyerr::status_error(&error)))?;
        let dir = dst.parent().map(Path::to_path_buf).unwrap_or_default();
        let prefix = format!(".{desc}-");
        let options = TempOptions {
            prefix: &prefix,
            suffix: "",
            mode: 0o600,
        };
        let file = download::to_temp_file(&mut response, &dir, &options)
            .await
            .map_err(|error| match error {
                download::DownloadError::Transport(error) => AttemptError::Transport(error),
                other @ download::DownloadError::Io { .. } => AttemptError::Other(pyerr::download_error(&other)),
            })?;
        // A truncated body shows up as a digest mismatch, so it is retryable.
        if let Some(expected) = expected_sha256.filter(|expected| !expected.is_empty())
            && file.sha256_hex() != expected
        {
            return Ok(Outcome::Retry {
                detail: format!("sha256 mismatch (got {})", file.sha256_hex()),
                retry_after: None,
            });
        }
        file.persist(dst)
            .await
            .map_err(|error| AttemptError::Other(pyerr::download_error(&error)))?;
        Ok(Outcome::Ok)
    }

    /// `_download_with_retries`: `true` when the file landed, `false` on a 404.
    async fn download_with_retries(
        &self,
        src_url: &str,
        dst: &Path,
        expected_sha256: Option<&str>,
        desc: &str,
        label: &str,
    ) -> Result<bool, JobError> {
        let attempts = API_RETRY_DELAYS_SECONDS.len() + 1;
        for attempt in 1..=attempts {
            self.pace().await;
            let outcome = match self.attempt_download(src_url, dst, expected_sha256, desc).await {
                Ok(outcome) => outcome,
                Err(AttemptError::Transport(error)) if should_retry_exception(error.class) && attempt < attempts => {
                    let delay = retry_delay_seconds(attempt, None);
                    log().warning(format!(
                        "Download failed {label} attempt={attempt}/{attempts}, retry in {delay:.1}s: {}: {}",
                        error.class.name(),
                        error.message
                    ));
                    (self.sleeper)(delay).await;
                    continue;
                }
                Err(error) => return Err(error.into_job_error()),
            };
            match outcome {
                Outcome::Ok => return Ok(true),
                Outcome::Missing => {
                    log().warning(format!("Attachment missing upstream (404): {label}"));
                    return Ok(false);
                }
                Outcome::Retry { detail, retry_after } => {
                    if attempt >= attempts {
                        return Err(JobError::new(
                            "RuntimeError",
                            format!("Download of {label} failed after {attempts} attempts: {detail}"),
                        ));
                    }
                    let delay = retry_delay_seconds(attempt, retry_after);
                    log().warning(format!(
                        "Download retryable failure {label} attempt={attempt}/{attempts}, retry in {delay:.1}s: {detail}"
                    ));
                    (self.sleeper)(delay).await;
                }
            }
        }
        Err(JobError::new("RuntimeError", format!("Download of {label} failed")))
    }

    /// `_download_attachment`: `true` when the file is on disk, `false` when the archive
    /// never stored it.
    pub(crate) async fn download_attachment(
        &self,
        attachment: &Attachment,
        dst_dir: &Path,
        post_id: &str,
        idx: usize,
    ) -> Result<bool, JobError> {
        let label = attachment.path_label();
        let src_url = build_file_url(&self.cfg.file_base_url, &label);
        let dst = dst_dir.join(attachment.file_name(idx)?);
        // Files are promoted atomically, so an existing one is complete.
        if tokio::fs::try_exists(&dst).await.unwrap_or(false) {
            log().debug(format!("File already exists, skip {}", file_name(&dst)));
            return Ok(true);
        }
        create_dir_all(dst_dir).await?;
        let sha256 = attachment.sha256()?;
        self.download_with_retries(&src_url, &dst, sha256.as_deref(), &format!("{post_id}-{idx}"), &label)
            .await
    }

    /// `_queue_original_recheck`: the first recheck a day out; a row already queued keeps
    /// its schedule.
    async fn queue_original_recheck(&self, post: &Post, attachment: &Attachment, dst: &Path) -> Result<(), JobError> {
        let next_check_at = Utc::now() + chrono::Duration::days(ORIGINAL_RECHECK_GAP_DAYS[0]);
        let relative = dst.strip_prefix(self.root()).unwrap_or(dst).to_string_lossy().into_owned();
        execute(
            &self.db,
            "INSERT INTO kemono_pending_originals (service, user_id, post_id, path, dst, attempts, next_check_at)
             VALUES ($1, $2, $3, $4, $5, 0, $6) ON CONFLICT DO NOTHING;",
            vec![
                Param::from(post.service.as_str()),
                Param::from(post.user.as_str()),
                Param::from(post.id.as_str()),
                Param::from(attachment.path_label()),
                Param::from(relative),
                Param::Timestamp(Some(next_check_at)),
            ],
        )
        .await
        .map(drop)
    }

    /// `_download_preview`: the thumbnail of a preview-only attachment, and a queued
    /// recheck for its original. A failed thumbnail is a missing file, never an error.
    async fn download_preview(&self, attachment: &Attachment, dst_dir: &Path, post: &Post, idx: usize) -> Result<bool, JobError> {
        let dst = dst_dir.join(attachment.file_name(idx)?);
        self.queue_original_recheck(post, attachment, &dst).await?;
        if tokio::fs::try_exists(&dst).await.unwrap_or(false) {
            log().debug(format!("File already exists, skip {}", file_name(&dst)));
            return Ok(true);
        }
        create_dir_all(dst_dir).await?;
        let label = attachment.path_label();
        let src_url = build_thumbnail_url(&self.cfg.thumbnail_base_url, &label);
        // The path stem is the original's sha256, so the thumbnail cannot be verified.
        match self
            .download_with_retries(&src_url, &dst, None, &format!("{}-{idx}-preview", post.id), &label)
            .await
        {
            Ok(landed) => Ok(landed),
            Err(error) => {
                log().warning(format!(
                    "Preview download failed {label}: {}: {}",
                    error.class_name(),
                    error.message()
                ));
                Ok(false)
            }
        }
    }

    /// `_download_post`: (downloaded, previews, missing).
    async fn download_post(&self, post: &Post, creator_dir: &Path) -> Result<(i64, i64, i64), JobError> {
        if post.attachments.is_empty() {
            log().warning(format!("Post {} has no downloadable attachments", post.id));
            return Ok((0, 0, 0));
        }
        let title = post.sanitized_title()?;
        let title = if title.is_empty() { "post".to_owned() } else { title };
        let post_dir = creator_dir.join(format!("{title} [{}]", post.id));
        let (mut downloaded, mut previews, mut missing) = (0, 0, 0);
        for (index, attachment) in post.attachments.iter().enumerate() {
            let idx = index + 1;
            if attachment.preview_only {
                if self.download_preview(attachment, &post_dir, post, idx).await? {
                    previews += 1;
                } else {
                    missing += 1;
                }
            } else if self.download_attachment(attachment, &post_dir, &post.id, idx).await? {
                downloaded += 1;
            } else {
                missing += 1;
            }
        }
        Ok((downloaded, previews, missing))
    }

    /// `_recheck_pending_original`: one try at replacing a preview with its original;
    /// otherwise the next gap of the schedule.
    async fn recheck_pending_original(&self, row: &PendingOriginal) -> Result<bool, JobError> {
        let dst = self.cfg.path.join(&row.dst);
        let src_url = build_file_url(&self.cfg.file_base_url, &row.path);
        if let Some(parent) = dst.parent() {
            create_dir_all(parent).await?;
        }
        self.pace().await;
        let outcome = match self
            .attempt_download(
                &src_url,
                &dst,
                sha256_from_path(&row.path).as_deref(),
                &format!("original-{}", row.post_id),
            )
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                let error = error.into_job_error();
                Outcome::Retry {
                    detail: format!("{}: {}", error.class_name(), error.message()),
                    retry_after: None,
                }
            }
        };
        let key = || {
            vec![
                Param::from(row.service.as_str()),
                Param::from(row.user_id.as_str()),
                Param::from(row.post_id.as_str()),
                Param::from(row.path.as_str()),
            ]
        };
        if matches!(outcome, Outcome::Ok) {
            execute(
                &self.db,
                "DELETE FROM kemono_pending_originals WHERE service = $1 AND user_id = $2 AND post_id = $3 AND path = $4;",
                key(),
            )
            .await?;
            log().info(format!("Original uploaded, replaced preview: {}", dst.display()));
            return Ok(true);
        }
        let attempts = row.attempts + 1;
        let gap_index = usize::try_from(attempts)
            .unwrap_or(usize::MAX)
            .min(ORIGINAL_RECHECK_GAP_DAYS.len() - 1);
        let gap_days = ORIGINAL_RECHECK_GAP_DAYS[gap_index];
        let next_check_at = Utc::now() + chrono::Duration::days(gap_days);
        let mut params = vec![Param::from(attempts), Param::Timestamp(Some(next_check_at))];
        params.extend(key());
        execute(
            &self.db,
            "UPDATE kemono_pending_originals SET attempts = $1, next_check_at = $2
             WHERE service = $3 AND user_id = $4 AND post_id = $5 AND path = $6;",
            params,
        )
        .await?;
        let detail = match &outcome {
            Outcome::Retry { detail, .. } if !detail.is_empty() => detail.clone(),
            _ => "missing".to_owned(),
        };
        log().info(format!(
            "Original still unavailable ({detail}), next check in {gap_days}d: {}",
            row.path
        ));
        Ok(false)
    }

    /// `_promote_pending_originals`: the due rechecks; how many previews were replaced.
    pub(crate) async fn promote_pending_originals(&self) -> Result<i64, JobError> {
        let rows = execute(
            &self.db,
            "SELECT service, user_id, post_id, path, dst, attempts FROM kemono_pending_originals WHERE next_check_at <= $1;",
            vec![Param::Timestamp(Some(Utc::now()))],
        )
        .await?;
        let mut promoted = 0;
        for row in rows {
            let row = PendingOriginal {
                service: row.get("service"),
                user_id: row.get("user_id"),
                post_id: row.get("post_id"),
                path: row.get("path"),
                dst: row.get("dst"),
                attempts: i64::from(row.get::<i32, _>("attempts")),
            };
            match self.recheck_pending_original(&row).await {
                Ok(true) => promoted += 1,
                Ok(false) => {}
                Err(error) => log().error(format!(
                    "Original recheck failed for {}/{} post {}\n{}",
                    row.service,
                    row.user_id,
                    row.post_id,
                    error.formatted()
                )),
            }
        }
        Ok(promoted)
    }

    /// `_update_creator`.
    pub(crate) async fn update_creator(&self, creator: &settings::KemonoCreator) -> Result<CreatorStats, JobError> {
        let creator_name = if creator.name.is_empty() {
            creator.id.clone()
        } else {
            creator.name.clone()
        };
        let creator_dir = self
            .cfg
            .path
            .join(&creator.service)
            .join(format!("{}_{}", sanitize(&creator_name, 80), creator.id));
        let downloaded_ids = self.downloaded_ids(&creator.service, &creator.id).await?;
        let mut stats = CreatorStats::default();
        let Some(new_posts) = self.fetch_new_posts(&creator.service, &creator.id, &downloaded_ids).await? else {
            log().warning(format!(
                "Creator {creator_name} ({}/{}) not found upstream",
                creator.service, creator.id
            ));
            stats.creator_missing = true;
            return Ok(stats);
        };
        if new_posts.is_empty() {
            log().info(format!("No new posts for {creator_name} ({})", creator.service));
            return Ok(stats);
        }
        let total = new_posts.len();
        log().info(format!("Found {total} new posts for {creator_name} ({})", creator.service));
        // Oldest first, each recorded only once its files are on disk.
        for (index, post) in new_posts.iter().rev().enumerate() {
            log().info(format!("Downloading post {} ({}/{total})", post.id, index + 1));
            let (downloaded, previews, missing) = self.download_post(post, &creator_dir).await?;
            execute(
                &self.db,
                "INSERT INTO kemono (id, service, user_id, title, creator, published_at)
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING;",
                vec![
                    Param::from(post.id.as_str()),
                    Param::from(post.service.as_str()),
                    Param::from(post.user.as_str()),
                    Param::from(str_value(&post.title)),
                    Param::from(creator_name.as_str()),
                    Param::from(post.published_text()),
                ],
            )
            .await?;
            stats.new_posts += 1;
            stats.downloaded_files += downloaded;
            stats.preview_files += previews;
            stats.missing_files += missing;
        }
        Ok(stats)
    }

    /// `_notify_summary`.
    pub(crate) async fn notify_summary(&self, totals: &CreatorStats, missing_creators: &[String], failed: &[String], promoted: i64) {
        let Some(notification) = summary_notification(totals, missing_creators, failed, promoted) else {
            log().info("No new Kemono posts or warnings; summary notification skipped");
            return;
        };
        if let Err(error) = self.outbox.enqueue(&notification).await {
            log().warning(format!("Failed to enqueue kemono summary notification: {error}"));
        }
    }

    /// `update()`.
    pub(crate) async fn update(&self) -> Result<(), JobError> {
        if self.cfg.creators.is_empty() {
            log().info("No kemono creators configured");
            return Ok(());
        }
        create_dir_all(&self.root()).await?;
        let promoted = self.promote_pending_originals().await?;
        let mut totals = CreatorStats::default();
        let mut missing_creators = Vec::new();
        let mut failed = Vec::new();
        for creator in &self.cfg.creators {
            let label = format!("{}/{}", creator.service, creator.id);
            let stats = match self.update_creator(creator).await {
                Ok(stats) => stats,
                Err(error) => {
                    let name = if creator.name.is_empty() { &creator.id } else { &creator.name };
                    log().error(format!("Kemono creator {name} ({}) failed\n{}", creator.service, error.formatted()));
                    failed.push(label);
                    continue;
                }
            };
            totals.new_posts += stats.new_posts;
            totals.downloaded_files += stats.downloaded_files;
            totals.preview_files += stats.preview_files;
            totals.missing_files += stats.missing_files;
            if stats.creator_missing {
                missing_creators.push(label);
            }
        }
        self.notify_summary(&totals, &missing_creators, &failed, promoted).await;
        if !failed.is_empty() {
            return Err(JobError::new(
                "CrawlRunError",
                format!("{} creator(s) failed: {}", failed.len(), failed.join(", ")),
            ));
        }
        Ok(())
    }
}

/// A due row of `kemono_pending_originals`.
struct PendingOriginal {
    service: String,
    user_id: String,
    post_id: String,
    path: String,
    dst: String,
    attempts: i64,
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The summary `enqueue_notification` call; `None` when there is nothing to report.
pub(crate) fn summary_notification(
    totals: &CreatorStats,
    missing_creators: &[String],
    failed: &[String],
    promoted: i64,
) -> Option<NewNotification> {
    if totals.new_posts == 0 && totals.missing_files == 0 && promoted == 0 && missing_creators.is_empty() && failed.is_empty() {
        return None;
    }
    let mut body = format!("Archived {} new posts with {} files.", totals.new_posts, totals.downloaded_files);
    if totals.preview_files != 0 {
        let _ = write!(body, " {} preview-only files saved as thumbnails.", totals.preview_files);
    }
    if promoted != 0 {
        let _ = write!(body, " {promoted} originals replaced previews.");
    }
    if totals.missing_files != 0 {
        let _ = write!(body, " {} files missing upstream.", totals.missing_files);
    }
    if !missing_creators.is_empty() {
        let _ = write!(body, " Creators not found: {}.", missing_creators.join(", "));
    }
    if !failed.is_empty() {
        let _ = write!(body, " Creators failed: {}.", failed.join(", "));
    }
    let strings = |items: &[String]| Value::List(items.iter().cloned().map(Value::Str).collect());
    let mut payload = Dict::new();
    payload.insert("new_posts", Value::from(totals.new_posts));
    payload.insert("downloaded_files", Value::from(totals.downloaded_files));
    payload.insert("preview_files", Value::from(totals.preview_files));
    payload.insert("promoted_originals", Value::from(promoted));
    payload.insert("missing_files", Value::from(totals.missing_files));
    payload.insert("missing_creators", strings(missing_creators));
    payload.insert("failed_creators", strings(failed));
    Some(NewNotification {
        kind: "summary".to_owned(),
        source: "kemono".to_owned(),
        header: "Kemono".to_owned(),
        title: "Update completed".to_owned(),
        body,
        payload: Some(payload),
        ..NewNotification::default()
    })
}

#[cfg(test)]
mod tests;
