//! Jandan favourites: the app API's favourite lists, decrypted, and their images.
//!
//! Python origin: `src/web/jandan.py`. The request and response `data` fields are
//! AES-256-CBC with a fixed key and zero IV, PKCS#7 padded, base64 and then URL-quoted
//! ([`encrypt_data_field`] / [`decrypt_data_field`], golden vectors in
//! `rust/goldens/jandan/`). Each favourite type is paged newest first with a date cursor
//! until two pages in a row are already archived; new images are upserted into `jandan`,
//! then every pending image of the type is downloaded (the large variant first for
//! Sina/wangmoyu hosts, sibling Sina shards after a 403, deleted-image placeholders
//! recognised by their redirect) and its row marked downloaded, unavailable or failed.
//!
//! Differences (DIVERGENCES.md D39-D41): images are streamed to a temporary file and
//! renamed into place instead of `write_bytes(response.content)`; `_ensure_table` is the
//! consolidated schema (D6); `_state_for_image` is never called in Python and is not
//! ported.

mod crypto;
mod isodate;

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use fav_core::logging::{self, Logger};
use fav_core::pyjson::{Dict, DumpOptions, Value};
use fav_core::pytext::{py_int_decimal, py_isdigit, py_strip};
use fav_core::pyurl::{self, urlsplit, urlunsplit};
use fav_core::settings;
use fav_db::{Db, Param, Statement};
use fav_net::download::{self, TempOptions};
use fav_net::retry::{Sleeper, tokio_sleeper};
use fav_net::{ClientOptions, ErrorClass, HttpClient, Request, StatusError, TransportError};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox};
use fav_runtime::pystr::{repr_str, str_value};
use fav_runtime::source::{JobError, Registry, Source};
use regex::Regex;
use sqlx::Row as _;

pub use crypto::{decrypt_data_field, encrypt_data_field};
pub use isodate::zulu_to_offset;

use crate::{SourceContext, db_job_error, pyerr, settings_job_error};

const APP_VERSION: &str = "2.6.0";
const ACCEPT_VERSION: &str = "1.0.0";
const FAV_TYPE_DIRS: [(i64, &str); 3] = [(1, "wuliao"), (2, "snapshot"), (6, "girls")];
const IMAGE_ACCEPT: &str = "image/*,*/*;q=0.8";
const IMAGE_REFERER: &str = "https://jandan.net/";
const REFERER_HOST_SUFFIXES: [&str; 1] = ["wangmoyu.com"];
const EXT_ALIASES: [(&str, &str); 6] = [
    ("jpeg", "jpg"),
    ("jpg", "jpg"),
    ("png", "png"),
    ("gif", "gif"),
    ("webp", "webp"),
    ("bmp", "bmp"),
];
const PLACEHOLDER_IMAGE_PATH_MARKER: &str = "/images/default_";
const UNAVAILABLE_STATUS_CODES: [u16; 2] = [404, 410];
const SINAIMG_FORBIDDEN_STATUS: u16 = 403;
const LARGE_VARIANT_HOST_SUFFIXES: [&str; 2] = ["sinaimg.cn", "wangmoyu.com"];
const SINAIMG_SHARDS: [&str; 4] = ["1", "2", "3", "4"];
pub const FULL_HIT_STOP_PAGES: u32 = 2;
pub const API_RETRY_DELAYS_SECONDS: [f64; 4] = [2.0, 5.0, 15.0, 30.0];
const API_RETRYABLE_STATUS_CODES: [u16; 4] = [429, 502, 503, 504];
const LAST_ERROR_MAX_CHARS: usize = 500;

/// `_MW_VARIANT_SEGMENT_RE`: `/mw\d+/`, with Python's Unicode `\d`.
static MW_VARIANT_SEGMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/mw\d+/").expect("valid pattern"));
/// `_SINAIMG_SHARD_HOST_RE`.
static SINAIMG_SHARD_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?P<prefix>[a-z]+)(?P<shard>[1-4])\.sinaimg\.cn$").expect("valid pattern"));

fn log() -> Logger {
    logging::get("jandan")
}

/// The app's `User-Agent`.
pub fn user_agent() -> String {
    format!("jian dan/{APP_VERSION} (iPhone; iOS 26.1; Scale/3.00)")
}

/// One image of one favourite post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JandanImage {
    pub fav_type: i64,
    pub pic_id: i64,
    pub content_index: i64,
    pub content_url: String,
    pub content_md5: Option<String>,
    pub author: String,
    pub post_date: Option<String>,
    pub image_type: Option<String>,
}

impl JandanImage {
    fn key(&self) -> (i64, i64, i64) {
        (self.fav_type, self.pic_id, self.content_index)
    }
}

/// `build_fav_request(...)`: the JSON object the API expects, in Python's key order.
pub fn build_fav_request(user_id: i64, fav_type: i64, fav_num_limit: i64, action_type: &str, fav_start_date: Option<&str>) -> Value {
    let mut fav = Dict::new();
    fav.insert("favType", Value::Str(fav_type.to_string()));
    fav.insert("favNumLimit", Value::from(fav_num_limit));
    fav.insert("actionType", Value::from(action_type));
    fav.insert("userID", Value::from(user_id));
    if let Some(start) = fav_start_date.filter(|start| !start.is_empty()) {
        fav.insert("favStartDate", Value::from(start));
    }
    let mut payload = Dict::new();
    payload.insert("fav", Value::Dict(fav));
    Value::Dict(payload)
}

/// An `i64` from an integer Python holds, or the database's refusal of a larger one.
fn int_in_range(value: i128) -> Result<i64, JobError> {
    i64::try_from(value).map_err(|_| JobError::new("NumericValueOutOfRange", "integer out of range"))
}

/// `_to_int(value)`: integers, truncated floats and digit strings; `None` for anything
/// else (booleans included). `int()`'s own refusals are errors, as in Python.
pub fn to_int(value: &Value) -> Result<Option<i64>, JobError> {
    match value {
        Value::Int(int) => int_in_range(*int).map(Some),
        Value::Float(float) => {
            if float.is_nan() {
                Err(JobError::new("ValueError", "cannot convert float NaN to integer"))
            } else if float.is_infinite() {
                Err(JobError::new("OverflowError", "cannot convert float infinity to integer"))
            } else {
                // `int(float)` truncates; beyond i128 it would not fit any column either.
                #[allow(clippy::cast_possible_truncation)] // checked against the i64 range just below
                let truncated = float.trunc() as i128;
                int_in_range(truncated).map(Some)
            }
        }
        Value::Str(text) => {
            let stripped = py_strip(text);
            if !py_isdigit(stripped) {
                return Ok(None);
            }
            match py_int_decimal(stripped) {
                Some(int) => int_in_range(int).map(Some),
                None => Err(JobError::new(
                    "ValueError",
                    format!("invalid literal for int() with base 10: {}", repr_str(stripped)),
                )),
            }
        }
        _ => Ok(None),
    }
}

fn normalize_image_url(url: &str) -> String {
    let normalized = py_strip(url);
    match normalized.strip_prefix("//") {
        Some(_) => format!("https:{normalized}"),
        None => normalized.to_owned(),
    }
}

fn split(url: &str) -> Result<pyurl::SplitResult, JobError> {
    urlsplit(url, "").map_err(|error| JobError::new("ValueError", error.0))
}

fn ext_alias(name: &str) -> Option<&'static str> {
    EXT_ALIASES.iter().find(|(alias, _)| *alias == name).map(|(_, ext)| *ext)
}

/// `infer_image_extension(content_url, image_type)`: the API's image type, else the URL's
/// suffix, else `jpg`.
pub fn infer_image_extension(content_url: &str, image_type: Option<&str>) -> Result<String, JobError> {
    if let Some(image_type) = image_type.filter(|image_type| !image_type.is_empty())
        && let Some(mapped) = ext_alias(&py_strip(image_type).to_lowercase())
    {
        return Ok(mapped.to_owned());
    }
    let path = split(content_url)?.path;
    let suffix = fav_core::pypath::suffix(&path).to_lowercase();
    let suffix = suffix.strip_prefix('.').unwrap_or(&suffix);
    Ok(ext_alias(suffix).unwrap_or("jpg").to_owned())
}

/// `str(value or default).strip()`.
fn str_or(value: Option<&Value>, default: &str) -> String {
    match value.filter(|value| value.is_truthy()) {
        Some(value) => py_strip(&str_value(value)).to_owned(),
        None => py_strip(default).to_owned(),
    }
}

/// A truthy string field, stripped, when it is a non-blank `str`.
fn nonblank_str(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Str(text)) if !py_strip(text).is_empty() => Some(py_strip(text).to_owned()),
        _ => None,
    }
}

/// `extract_last_pic_date(pics)`: the last non-blank `date` string.
pub fn extract_last_pic_date(pics: &[Value]) -> Option<String> {
    pics.iter().rev().find_map(|pic| nonblank_str(pic.get("date")))
}

/// `extract_pic_images(fav_type=..., pics=...)`: the image contents of each post, numbered
/// from 1 by their place in `contents`.
pub fn extract_pic_images(fav_type: i64, pics: &[Value]) -> Result<Vec<JandanImage>, JobError> {
    let mut images = Vec::new();
    for pic in pics {
        let Some(pic_id) = to_int(pic.get("id").unwrap_or(&Value::Null))? else {
            continue;
        };
        let author = Some(str_or(pic.get("author"), "unknown"))
            .filter(|author| !author.is_empty())
            .unwrap_or_else(|| "unknown".to_owned());
        let post_date = Some(str_or(pic.get("date"), "")).filter(|date| !date.is_empty());
        let Some(Value::List(contents)) = pic.get("contents") else {
            continue;
        };
        for (index, content) in contents.iter().enumerate() {
            if !matches!(content, Value::Dict(_)) {
                continue;
            }
            if str_or(content.get("contentType"), "").to_lowercase() != "image" {
                continue;
            }
            let Some(Value::Str(content_url)) = content.get("content") else {
                continue;
            };
            if py_strip(content_url).is_empty() {
                continue;
            }
            images.push(JandanImage {
                fav_type,
                pic_id,
                content_index: i64::try_from(index).unwrap_or(i64::MAX - 1) + 1,
                content_url: normalize_image_url(content_url),
                content_md5: nonblank_str(content.get("md5")),
                author: author.clone(),
                post_date: post_date.clone(),
                image_type: nonblank_str(content.get("imageType")).map(|image_type| image_type.to_lowercase()),
            });
        }
    }
    Ok(images)
}

/// `extract_fav_items(fav_type=..., fav=...)`: the list under the type's key, else under
/// `pics`, else the first list in the object; only its objects.
pub fn extract_fav_items(fav_type: i64, fav: &Dict) -> Vec<Value> {
    let mut keys: Vec<&str> = match fav_type {
        1 => vec!["pics"],
        2 => vec!["girls"],
        6 => vec!["nzs"],
        _ => Vec::new(),
    };
    if !keys.contains(&"pics") {
        keys.push("pics");
    }
    let dicts = |items: &[Value]| items.iter().filter(|item| matches!(item, Value::Dict(_))).cloned().collect();
    for key in keys {
        if let Some(Value::List(items)) = fav.get(key) {
            return dicts(items);
        }
    }
    fav.iter()
        .find_map(|(_, value)| match value {
            Value::List(items) => Some(dicts(items)),
            _ => None,
        })
        .unwrap_or_default()
}

/// `detect_deleted_placeholder(response)`: Sina answers a deleted image with a redirect to
/// a `/images/default_...` placeholder.
pub fn detect_deleted_placeholder(final_url: &url::Url, history: &[fav_net::client::Redirect]) -> (bool, Option<String>) {
    if pyurl::unquote(final_url.path())
        .to_lowercase()
        .contains(PLACEHOLDER_IMAGE_PATH_MARKER)
    {
        return (true, Some(format!("placeholder path: {final_url}")));
    }
    for redirect in history {
        let Some(location) = fav_net::client::header_text(&redirect.headers, "location").filter(|location| !location.is_empty()) else {
            continue;
        };
        let location = location.to_lowercase();
        if location.contains(PLACEHOLDER_IMAGE_PATH_MARKER) {
            let errno = fav_net::client::header_text(&redirect.headers, "x-image-errno").unwrap_or_default();
            let errno = py_strip(&errno);
            let mut reason = format!("redirected to placeholder: {location}");
            if !errno.is_empty() {
                reason = format!("{reason} (errno={errno})");
            }
            return (true, Some(reason));
        }
    }
    (false, None)
}

/// `should_mark_unavailable_http(status_code=..., url_host=...)`: gone (404/410), or a
/// Sina 403 that survived the shard retry.
pub fn should_mark_unavailable_http(status_code: u16, url_host: Option<&str>) -> bool {
    if UNAVAILABLE_STATUS_CODES.contains(&status_code) {
        return true;
    }
    let host = py_strip(url_host.unwrap_or_default()).to_lowercase();
    status_code == SINAIMG_FORBIDDEN_STATUS && host.ends_with("sinaimg.cn")
}

fn hostname(parts: &pyurl::SplitResult) -> String {
    parts.hostname().unwrap_or_default().to_lowercase()
}

/// `build_image_download_candidates(content_url)`: the `/large/` variant first where the
/// host serves one, then the URL itself.
pub fn build_image_download_candidates(content_url: &str) -> Result<Vec<String>, JobError> {
    let base_url = normalize_image_url(content_url);
    let parts = split(&base_url)?;
    let host = hostname(&parts);
    let supports_large = LARGE_VARIANT_HOST_SUFFIXES.iter().any(|suffix| host.ends_with(suffix));
    if !supports_large || !MW_VARIANT_SEGMENT.is_match(&parts.path) {
        return Ok(vec![base_url]);
    }
    let large_path = MW_VARIANT_SEGMENT.replacen(&parts.path, 1, "/large/");
    let large_url = urlunsplit(&pyurl::SplitResult {
        path: large_path.into_owned(),
        ..parts.clone()
    });
    if large_url == base_url {
        return Ok(vec![base_url]);
    }
    Ok(vec![large_url, base_url])
}

/// `build_image_request_headers(url)`.
pub fn build_image_request_headers(url: &str) -> Result<Vec<(String, String)>, JobError> {
    let mut headers = vec![
        ("Accept".to_owned(), IMAGE_ACCEPT.to_owned()),
        ("Connection".to_owned(), "keep-alive".to_owned()),
    ];
    let host = hostname(&split(url)?);
    if REFERER_HOST_SUFFIXES.iter().any(|suffix| host.ends_with(suffix)) {
        headers.push(("Referer".to_owned(), IMAGE_REFERER.to_owned()));
    }
    Ok(headers)
}

/// `build_sinaimg_shard_candidates(url)`: the same path on the other numbered Sina shards.
pub fn build_sinaimg_shard_candidates(url: &str) -> Result<Vec<String>, JobError> {
    let parts = split(url)?;
    let host = hostname(&parts);
    let Some(captures) = SINAIMG_SHARD_HOST.captures(&host) else {
        return Ok(Vec::new());
    };
    let prefix = &captures["prefix"];
    let current = &captures["shard"];
    Ok(SINAIMG_SHARDS
        .iter()
        .filter(|shard| **shard != current)
        .map(|shard| {
            urlunsplit(&pyurl::SplitResult {
                netloc: format!("{prefix}{shard}.sinaimg.cn"),
                ..parts.clone()
            })
        })
        .collect())
}

/// `should_retry_jandan_api_status(status_code)`.
pub fn should_retry_jandan_api_status(status_code: u16) -> bool {
    API_RETRYABLE_STATUS_CODES.contains(&status_code)
}

/// `should_retry_jandan_api_exception(exc)`: connect errors, timeouts and connections
/// closed early (`RemoteProtocolError`, a body cut short included); a reset or other
/// socket error while reading (`ReadError`) is not retried.
pub fn should_retry_jandan_api_exception(class: ErrorClass) -> bool {
    matches!(class, ErrorClass::ConnectError | ErrorClass::RemoteProtocolError) || class.is_timeout()
}

/// Python's `reason[:500]`.
fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

// ---------- the job ----------

/// The Jandan job (`JOB_SPECS['jandan']`).
pub struct Jandan {
    ctx: SourceContext,
}

impl std::fmt::Debug for Jandan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jandan").finish_non_exhaustive()
    }
}

impl Jandan {
    pub fn new(ctx: SourceContext) -> Self {
        Self { ctx }
    }
}

/// Register the job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run("jandan", move || Box::new(Jandan::new(ctx.clone())));
}

/// `httpx.AsyncClient(...)` as `Jandan.__init__` builds it.
pub fn client_options() -> ClientOptions {
    ClientOptions {
        headers: vec![
            ("User-Agent".to_owned(), user_agent()),
            ("Accept-Language".to_owned(), "en-US;q=1, zh-Hans-US;q=0.9".to_owned()),
        ],
        timeout: std::time::Duration::from_mins(1),
        follow_redirects: true,
        max_keepalive_connections: 10,
        ..ClientOptions::default()
    }
}

impl Source for Jandan {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self.ctx.settings.load().await.map_err(|error| settings_job_error(&error))?;
            let http = HttpClient::new(&client_options()).map_err(|error| JobError::new("ValueError", error.to_string()))?;
            let run = Run {
                cfg: settings.web.jandan.clone(),
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
    pub(crate) cfg: settings::Jandan,
    pub(crate) http: HttpClient,
    pub(crate) db: Db,
    pub(crate) outbox: Outbox,
    pub(crate) sleeper: Sleeper,
}

/// Why one image did not land.
#[derive(Debug)]
pub(crate) enum ImageError {
    /// `UnavailableImageError`: a deleted-image placeholder.
    Unavailable(String),
    /// `httpx.HTTPStatusError`, with the host of the request that got it.
    Status { error: StatusError, host: Option<String> },
    /// Anything else, as the Python exception it would be.
    Other(JobError),
}

impl From<JobError> for ImageError {
    fn from(error: JobError) -> Self {
        Self::Other(error)
    }
}

/// A fetched image: where it came from and its body in a temporary file.
pub(crate) struct Fetched {
    url: url::Url,
    history: Vec<fav_net::client::Redirect>,
    file: download::Downloaded,
}

enum FetchError {
    Status { error: StatusError, host: Option<String> },
    Transport(TransportError),
    Other(JobError),
}

impl From<FetchError> for ImageError {
    fn from(error: FetchError) -> Self {
        match error {
            FetchError::Status { error, host } => Self::Status { error, host },
            FetchError::Transport(error) => Self::Other(pyerr::transport_error(&error)),
            FetchError::Other(error) => Self::Other(error),
        }
    }
}

type Key = (i64, i64, i64);

/// One favourite type's numbers: pages, API images, newly collected, pending, downloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TypeStats {
    pub downloaded: i64,
    pub pages: i64,
    pub api_total: i64,
    pub collected: i64,
    pub pending: i64,
}

impl Run {
    pub(crate) fn output_path(&self, image: &JandanImage) -> Result<PathBuf, JobError> {
        let ext = infer_image_extension(&image.content_url, image.image_type.as_deref())?;
        let mut file_id = format!("{}-{}", image.pic_id, image.content_index);
        if let Some(md5) = image.content_md5.as_deref().filter(|md5| !md5.is_empty()) {
            file_id = format!("{file_id}-{}", md5.chars().take(8).collect::<String>());
        }
        let filename = fav_core::filename::format_video_filename(
            &image.author,
            &file_id,
            Some(&format!("type{}", image.fav_type)),
            &ext,
            fav_core::filename::MAX_TOTAL_BYTES,
        );
        let folder = FAV_TYPE_DIRS
            .iter()
            .find(|(fav_type, _)| *fav_type == image.fav_type)
            .map_or_else(|| format!("type-{}", image.fav_type), |(_, folder)| (*folder).to_owned());
        Ok(self.cfg.path.join(folder).join(filename))
    }

    async fn execute(&self, sql: &str, params: Vec<Param>) -> Result<Vec<sqlx::postgres::PgRow>, JobError> {
        crate::execute(&self.db, sql, params).await
    }

    fn key_params(image: &JandanImage) -> [Param; 3] {
        [
            Param::from(image.fav_type),
            Param::from(image.pic_id),
            Param::from(image.content_index),
        ]
    }

    async fn mark_downloaded(&self, image: &JandanImage, dst: &Path) -> Result<(), JobError> {
        let mut params = vec![Param::from(dst.to_string_lossy().into_owned())];
        params.extend(Self::key_params(image));
        self.execute(
            "UPDATE jandan
             SET downloaded = 1, local_path = $1, unavailable = 0, failed_count = 0, last_error = ''
             WHERE fav_type = $2 AND pic_id = $3 AND content_index = $4;",
            params,
        )
        .await
        .map(drop)
    }

    async fn mark_unavailable(&self, image: &JandanImage, reason: &str) -> Result<(), JobError> {
        let mut params = vec![Param::from(truncate_chars(reason, LAST_ERROR_MAX_CHARS))];
        params.extend(Self::key_params(image));
        self.execute(
            "UPDATE jandan
             SET unavailable = 1, failed_count = failed_count + 1, last_error = $1
             WHERE fav_type = $2 AND pic_id = $3 AND content_index = $4;",
            params,
        )
        .await
        .map(drop)
    }

    async fn mark_failed(&self, image: &JandanImage, reason: &str) -> Result<(), JobError> {
        let mut params = vec![Param::from(truncate_chars(reason, LAST_ERROR_MAX_CHARS))];
        params.extend(Self::key_params(image));
        self.execute(
            "UPDATE jandan
             SET failed_count = failed_count + 1, last_error = $1
             WHERE fav_type = $2 AND pic_id = $3 AND content_index = $4;",
            params,
        )
        .await
        .map(drop)
    }

    pub(crate) async fn upsert_images(&self, images: &[JandanImage]) -> Result<(), JobError> {
        let rows = images
            .iter()
            .map(|image| {
                vec![
                    Param::from(image.fav_type),
                    Param::from(image.pic_id),
                    Param::from(image.content_index),
                    Param::from(image.content_md5.clone().unwrap_or_default()),
                    Param::from(image.content_url.clone()),
                    Param::from(image.author.clone()),
                    Param::from(image.post_date.clone().unwrap_or_default()),
                ]
            })
            .collect();
        self.db
            .insert_batch(
                "jandan",
                &[
                    "fav_type",
                    "pic_id",
                    "content_index",
                    "content_md5",
                    "content_url",
                    "author",
                    "post_date",
                ],
                rows,
                Some(
                    "(fav_type, pic_id, content_index) DO UPDATE SET \
                     content_md5 = excluded.content_md5, \
                     content_url = excluded.content_url, \
                     author = excluded.author, \
                     post_date = excluded.post_date",
                ),
                fav_db::batch::DEFAULT_INSERT_CHUNK_SIZE,
            )
            .await
            .map_err(|error| db_job_error(&error))
    }

    /// `_processed_keys_for_page`: the keys already downloaded or known unavailable.
    async fn processed_keys(&self, keys: &[Key]) -> Result<Vec<Key>, JobError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let statements = keys
            .iter()
            .map(|(fav_type, pic_id, content_index)| {
                Statement::new(
                    "SELECT fav_type, pic_id, content_index
                     FROM jandan
                     WHERE fav_type = $1 AND pic_id = $2 AND content_index = $3
                       AND (downloaded = 1 OR unavailable = 1)
                     LIMIT 1;",
                    vec![Param::from(*fav_type), Param::from(*pic_id), Param::from(*content_index)],
                )
            })
            .collect();
        let results = self
            .db
            .query_batch(
                statements,
                fav_db::batch::DEFAULT_QUERY_CHUNK_SIZE,
                fav_db::batch::DEFAULT_MAX_BIND_PARAMS,
            )
            .await
            .map_err(|error| db_job_error(&error))?;
        Ok(keys
            .iter()
            .zip(results)
            .filter(|(_, rows)| !rows.is_empty())
            .map(|(key, _)| *key)
            .collect())
    }

    /// `_is_page_fully_processed`: (all processed, processed count, distinct keys).
    pub(crate) async fn is_page_fully_processed(&self, images: &[JandanImage]) -> Result<(bool, usize, usize), JobError> {
        let mut keys: Vec<Key> = images.iter().map(JandanImage::key).collect();
        keys.sort_unstable();
        keys.dedup();
        if keys.is_empty() {
            return Ok((false, 0, 0));
        }
        let processed = self.processed_keys(&keys).await?.len();
        Ok((processed == keys.len(), processed, keys.len()))
    }

    /// `_pending_images_from_db`: not downloaded and not unavailable, oldest first.
    pub(crate) async fn pending_images(&self, fav_type: i64) -> Result<Vec<JandanImage>, JobError> {
        let rows = self
            .execute(
                "SELECT fav_type, pic_id, content_index, content_url, content_md5, author, post_date
                 FROM jandan
                 WHERE fav_type = $1 AND downloaded = 0 AND unavailable = 0
                 ORDER BY created_at ASC;",
                vec![Param::from(fav_type)],
            )
            .await?;
        let mut images = Vec::with_capacity(rows.len());
        for row in rows {
            let int = |name: &str| row.try_get::<Option<i32>, _>(name).ok().flatten().map(i64::from);
            let text = |name: &str| row.try_get::<Option<String>, _>(name).ok().flatten();
            let (Some(row_fav_type), Some(pic_id), Some(content_index)) = (int("fav_type"), int("pic_id"), int("content_index")) else {
                continue;
            };
            let Some(content_url) = text("content_url").filter(|url| !py_strip(url).is_empty()) else {
                continue;
            };
            let author = text("author")
                .filter(|author| !author.is_empty())
                .unwrap_or_else(|| "unknown".to_owned());
            let author = Some(py_strip(&author).to_owned())
                .filter(|author| !author.is_empty())
                .unwrap_or_else(|| "unknown".to_owned());
            images.push(JandanImage {
                fav_type: row_fav_type,
                pic_id,
                content_index,
                content_url: normalize_image_url(&content_url),
                content_md5: text("content_md5")
                    .map(|md5| py_strip(&md5).to_owned())
                    .filter(|md5| !md5.is_empty()),
                author,
                post_date: text("post_date")
                    .map(|date| py_strip(&date).to_owned())
                    .filter(|date| !date.is_empty()),
                image_type: None,
            });
        }
        Ok(images)
    }

    /// `_fetch_fav_page`: one page of a favourite type, decrypted.
    pub(crate) async fn fetch_fav_page(&self, fav_type: i64, fav_start_date: Option<&str>) -> Result<Vec<Value>, JobError> {
        let request = build_fav_request(self.cfg.user_id, fav_type, self.cfg.fav_num_limit, "get", fav_start_date);
        let plain =
            fav_core::pyjson::dumps(&request, &DumpOptions::compact()).map_err(|error| JobError::new("TypeError", error.to_string()))?;
        let data_value = encrypt_data_field(&plain);
        let (status, body) = self.post_fav_api(&data_value, fav_type).await?;
        status.map_err(|error| pyerr::status_error(&error))?;
        let payload = pyerr::json_loads_bytes(&body)?;
        let Value::Dict(payload) = payload else {
            return Err(pyerr::no_attribute(&payload, "get"));
        };
        let code = to_int(payload.get("code").unwrap_or(&Value::Null))?;
        if let Some(code) = code.filter(|code| *code != 0) {
            let message = [payload.get("msg"), payload.get("message")]
                .into_iter()
                .flatten()
                .find(|value| value.is_truthy())
                .map(|value| py_strip(&str_value(value)).to_owned())
                .unwrap_or_default();
            let detail = if message.is_empty() {
                String::new()
            } else {
                format!(" ({message})")
            };
            return Err(JobError::new(
                "ValueError",
                format!("Jandan API returned error code: {code}{detail}"),
            ));
        }
        let Some(Value::Str(encrypted)) = payload.get("data") else {
            return Ok(Vec::new());
        };
        if py_strip(encrypted).is_empty() {
            return Ok(Vec::new());
        }
        let decoded = decrypt_data_field(encrypted)?;
        let parsed = pyerr::json_loads(&decoded)?;
        let Value::Dict(parsed) = parsed else {
            return Err(pyerr::no_attribute(&parsed, "get"));
        };
        match parsed.get("fav") {
            Some(Value::Dict(fav)) => Ok(extract_fav_items(fav_type, fav)),
            _ => Ok(Vec::new()),
        }
    }

    /// `_post_fav_api`: the API call with its retry ladder for transient failures. The
    /// body is read inside the ladder, as httpx reads it within `client.post`.
    async fn post_fav_api(&self, data_value: &str, fav_type: i64) -> Result<(Result<(), StatusError>, bytes::Bytes), JobError> {
        let attempts = API_RETRY_DELAYS_SECONDS.len() + 1;
        for attempt in 1..=attempts {
            let request = Request::post(self.cfg.api_url.clone(), format!("data={data_value}").into_bytes())
                .header("Accept-version", ACCEPT_VERSION)
                .header("Content-Type", "application/x-www-form-urlencoded");
            let fetched = match self.http.send(request).await {
                Ok(response) => {
                    let status = response.status();
                    let check = response.error_for_status();
                    response.bytes().await.map(|body| (status, check, body))
                }
                Err(error) => Err(error),
            };
            let (status, check, body) = match fetched {
                Ok(fetched) => fetched,
                Err(error) => {
                    if !should_retry_jandan_api_exception(error.class) || attempt >= attempts {
                        return Err(pyerr::transport_error(&error));
                    }
                    let delay = API_RETRY_DELAYS_SECONDS[attempt - 1];
                    log().warning(format!(
                        "Jandan API request failed favType={fav_type} attempt={attempt}/{attempts}, retry in {delay:.1}s: {}: {}",
                        error.class.name(),
                        error.message
                    ));
                    (self.sleeper)(delay).await;
                    continue;
                }
            };
            if !should_retry_jandan_api_status(status) || attempt >= attempts {
                return Ok((check, body));
            }
            let delay = API_RETRY_DELAYS_SECONDS[attempt - 1];
            log().warning(format!(
                "Jandan API returned HTTP {status} favType={fav_type} attempt={attempt}/{attempts}, retry in {delay:.1}s"
            ));
            (self.sleeper)(delay).await;
        }
        Err(JobError::new("RuntimeError", "Jandan API request failed without response"))
    }

    /// `_fetch_image`: GET with the image headers; the body goes to a temporary file in
    /// `dir`, as httpx reads it before `raise_for_status()`.
    async fn fetch_image(&self, url: &str, dir: &Path) -> Result<Fetched, FetchError> {
        let mut request = Request::get(url);
        for (name, value) in build_image_request_headers(url).map_err(FetchError::Other)? {
            request = request.header(name, value);
        }
        let mut response = self.http.send(request).await.map_err(FetchError::Transport)?;
        if let Err(error) = response.error_for_status() {
            let host = response.url().host_str().map(str::to_owned);
            // httpx has read the error body by now; a failure reading it wins.
            response.bytes().await.map_err(FetchError::Transport)?;
            return Err(FetchError::Status { error, host });
        }
        let options = TempOptions {
            prefix: ".jandan-",
            suffix: ".part",
            mode: 0o666,
        };
        let file = download::to_temp_file(&mut response, dir, &options)
            .await
            .map_err(|error| match error {
                download::DownloadError::Transport(error) => FetchError::Transport(error),
                other @ download::DownloadError::Io { .. } => FetchError::Other(pyerr::download_error(&other)),
            })?;
        Ok(Fetched {
            url: response.url().clone(),
            history: response.history().to_vec(),
            file,
        })
    }

    /// `_fetch_from_sinaimg_shards`: the first sibling shard that answers.
    async fn fetch_from_sinaimg_shards(&self, url: &str, dir: &Path) -> Result<Option<Fetched>, FetchError> {
        for shard_url in build_sinaimg_shard_candidates(url).map_err(FetchError::Other)? {
            match self.fetch_image(&shard_url, dir).await {
                Ok(fetched) => {
                    log().info(format!("Recovered forbidden Jandan image from shard: {shard_url}"));
                    return Ok(Some(fetched));
                }
                Err(FetchError::Status { .. }) => {}
                Err(FetchError::Transport(error)) if error.class.is_transport() => {}
                Err(other) => return Err(other),
            }
        }
        Ok(None)
    }

    /// `_fetch_candidate`: a 403 is retried on the sibling shards.
    async fn fetch_candidate(&self, url: &str, dir: &Path) -> Result<Fetched, FetchError> {
        match self.fetch_image(url, dir).await {
            Err(FetchError::Status { error, host }) if error.status == SINAIMG_FORBIDDEN_STATUS => {
                match self.fetch_from_sinaimg_shards(url, dir).await? {
                    Some(fetched) => Ok(fetched),
                    None => Err(FetchError::Status { error, host }),
                }
            }
            other => other,
        }
    }

    /// `_download_image`: the first candidate that is a real image, moved into place.
    pub(crate) async fn download_image(&self, image: &JandanImage) -> Result<PathBuf, ImageError> {
        let dst = self.output_path(image)?;
        let dir = dst.parent().map(Path::to_path_buf).unwrap_or_default();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| ImageError::Other(io_error(&error, &dir)))?;
        if tokio::fs::try_exists(&dst).await.unwrap_or(false) {
            return Ok(dst);
        }
        let candidates = build_image_download_candidates(&image.content_url)?;
        let mut placeholder_reason: Option<String> = None;
        let mut last_status: Option<(StatusError, Option<String>)> = None;
        for (index, candidate) in candidates.iter().enumerate() {
            let is_last = index + 1 == candidates.len();
            let fetched = match self.fetch_candidate(candidate, &dir).await {
                Ok(fetched) => fetched,
                Err(FetchError::Status { error, host }) => {
                    if !is_last {
                        last_status = Some((error, host));
                        continue;
                    }
                    return Err(ImageError::Status { error, host });
                }
                Err(other) => return Err(other.into()),
            };
            let (is_placeholder, reason) = detect_deleted_placeholder(&fetched.url, &fetched.history);
            if is_placeholder {
                let reason = reason.unwrap_or_else(|| format!("placeholder response: {}", fetched.url));
                if !is_last {
                    placeholder_reason = Some(reason);
                    continue;
                }
                return Err(ImageError::Unavailable(reason));
            }
            fetched
                .file
                .persist(&dst)
                .await
                .map_err(|error| ImageError::Other(pyerr::download_error(&error)))?;
            return Ok(dst);
        }
        if let Some(reason) = placeholder_reason {
            return Err(ImageError::Unavailable(reason));
        }
        if let Some((error, host)) = last_status {
            return Err(ImageError::Status { error, host });
        }
        Err(ImageError::Other(JobError::new(
            "RuntimeError",
            format!("Failed to download image from all candidates: {}", image.content_url),
        )))
    }

    async fn process_image(&self, image: &JandanImage) -> Result<PathBuf, ImageError> {
        let dst = self.download_image(image).await?;
        self.mark_downloaded(image, &dst).await?;
        Ok(dst)
    }

    /// `_download_pending_images`: each image's outcome recorded on its row.
    pub(crate) async fn download_pending_images(&self, images: &[JandanImage]) -> Result<i64, JobError> {
        let mut downloaded = 0;
        let total = images.len();
        for (position, image) in images.iter().enumerate() {
            let current = position + 1;
            let (fav_type, pic, index) = (image.fav_type, image.pic_id, image.content_index);
            log().info(format!(
                "Jandan downloading favType={fav_type} progress={current}/{total} pic={pic} index={index}"
            ));
            match self.process_image(image).await {
                Ok(dst) => {
                    downloaded += 1;
                    let name = dst.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                    log().notice(format!(
                        "Jandan downloaded favType={fav_type} progress={current}/{total} file={name}"
                    ));
                }
                Err(ImageError::Unavailable(reason)) => {
                    let reason = Some(py_strip(&reason).to_owned())
                        .filter(|reason| !reason.is_empty())
                        .unwrap_or_else(|| "confirmed unavailable".to_owned());
                    self.mark_unavailable(image, &reason).await?;
                    log().info(format!(
                        "Marked unavailable Jandan item favType={fav_type} progress={current}/{total} pic={pic} index={index}: {reason}"
                    ));
                }
                Err(ImageError::Status { error, host }) => {
                    let status = error.status;
                    if should_mark_unavailable_http(status, host.as_deref()) {
                        let mut reason = format!("http {status}");
                        if status == SINAIMG_FORBIDDEN_STATUS
                            && let Some(host) = host.as_deref().filter(|host| !host.is_empty())
                        {
                            reason = format!("{reason} on {host}");
                        }
                        self.mark_unavailable(image, &reason).await?;
                        log().info(format!(
                            "Marked unavailable Jandan item favType={fav_type} progress={current}/{total} pic={pic} index={index}: {reason}"
                        ));
                        continue;
                    }
                    self.mark_failed(image, &format!("http {status}")).await?;
                    log().warning(format!(
                        "Failed to process Jandan item favType={fav_type} progress={current}/{total} pic={pic} index={index}: http {status}"
                    ));
                }
                Err(ImageError::Other(error)) => {
                    self.mark_failed(image, &format!("{}: {}", error.class_name(), error.message()))
                        .await?;
                    log().warning(format!(
                        "Failed to process Jandan item favType={fav_type} progress={current}/{total} pic={pic} index={index}: {}",
                        error.message()
                    ));
                }
            }
        }
        Ok(downloaded)
    }

    /// `_crawl_fav_type`: page until two fully archived pages in a row, store what is new,
    /// then download everything pending for the type.
    pub(crate) async fn crawl_fav_type(&self, fav_type: i64) -> Result<TypeStats, JobError> {
        let mut fav_start_date: Option<String> = None;
        let mut page_count = 0;
        let mut seen_cursors: Vec<String> = Vec::new();
        let mut page_index = 0;
        let mut api_total_images = 0_i64;
        let mut full_hit_pages = 0;
        let mut collected: Vec<JandanImage> = Vec::new();
        let mut collected_keys: std::collections::HashSet<Key> = std::collections::HashSet::new();
        log().info(format!(
            "Jandan stop rule favType={fav_type} stop_after_full_hit_pages={FULL_HIT_STOP_PAGES}"
        ));
        loop {
            page_index += 1;
            let requested_cursor = fav_start_date.clone().unwrap_or_else(|| "(first-page)".to_owned());
            let pics = self.fetch_fav_page(fav_type, fav_start_date.as_deref()).await?;
            if pics.is_empty() {
                log().info(format!("Jandan API reached end favType={fav_type} page={page_index}"));
                break;
            }
            page_count = page_index;
            let images = extract_pic_images(fav_type, &pics)?;
            if images.is_empty() {
                break;
            }
            api_total_images += i64::try_from(images.len()).unwrap_or(i64::MAX);
            log().info(format!(
                "Jandan API fetched favType={fav_type} page={page_index} cursor={requested_cursor} page_images={} total_images={api_total_images}",
                images.len()
            ));
            let (fully_processed, processed, total) = self.is_page_fully_processed(&images).await?;
            if fully_processed {
                full_hit_pages += 1;
                log().info(format!(
                    "Jandan API full-hit favType={fav_type} page={page_index} matched={processed}/{total} consecutive_full_hits={full_hit_pages}/{FULL_HIT_STOP_PAGES}"
                ));
                if full_hit_pages >= FULL_HIT_STOP_PAGES {
                    log().info(format!("Jandan favType={fav_type} reached full-hit stop condition"));
                    break;
                }
            } else {
                full_hit_pages = 0;
                for image in images {
                    if collected_keys.insert(image.key()) {
                        collected.push(image);
                    }
                }
            }
            let Some(last_date) = extract_last_pic_date(&pics) else {
                break;
            };
            let next_cursor = zulu_to_offset(&last_date)?;
            if seen_cursors.contains(&next_cursor) {
                log().warning(format!("Jandan favType={fav_type} repeated cursor {next_cursor}, stop pagination"));
                break;
            }
            seen_cursors.push(next_cursor.clone());
            fav_start_date = Some(next_cursor);
        }
        if !collected.is_empty() {
            self.upsert_images(&collected).await?;
        }
        let pending = self.pending_images(fav_type).await?;
        let downloaded = self.download_pending_images(&pending).await?;
        Ok(TypeStats {
            downloaded,
            pages: page_count,
            api_total: api_total_images,
            collected: i64::try_from(collected.len()).unwrap_or(i64::MAX),
            pending: i64::try_from(pending.len()).unwrap_or(i64::MAX),
        })
    }

    /// `_notify_summary`.
    pub(crate) async fn notify_summary(&self, total_downloaded: i64, total_api_images: i64, stats: &[(i64, TypeStats)]) {
        let notification = summary_notification(total_downloaded, total_api_images, stats);
        if let Err(error) = self.outbox.enqueue(&notification).await {
            log().warning(format!("Failed to enqueue jandan summary notification: {error}"));
        }
    }

    /// `update()`.
    pub(crate) async fn update(&self) -> Result<(), JobError> {
        if self.cfg.user_id <= 0 {
            log().warning("Jandan user_id is not configured, skip update");
            return Ok(());
        }
        let root = self.cfg.path.as_path().to_path_buf();
        tokio::fs::create_dir_all(&root).await.map_err(|error| io_error(&error, &root))?;
        let mut total_downloaded = 0;
        let mut total_api_images = 0;
        let mut stats = Vec::new();
        for &fav_type in &self.cfg.fav_types {
            let type_stats = self.crawl_fav_type(fav_type).await?;
            total_downloaded += type_stats.downloaded;
            total_api_images += type_stats.api_total;
            stats.push((fav_type, type_stats));
            log().info(format!(
                "Jandan favType={fav_type} scanned={} pages api_total={} collected={} pending={} downloaded={} files",
                type_stats.pages, type_stats.api_total, type_stats.collected, type_stats.pending, type_stats.downloaded
            ));
        }
        log().info(format!("Jandan API total images across fav_types={total_api_images}"));
        if total_downloaded == 0 {
            log().info("No new Jandan images");
            return Ok(());
        }
        self.notify_summary(total_downloaded, total_api_images, &stats).await;
        Ok(())
    }
}

/// The summary `enqueue_notification` call.
pub(crate) fn summary_notification(total_downloaded: i64, total_api_images: i64, stats: &[(i64, TypeStats)]) -> NewNotification {
    let fav_type_stats = stats
        .iter()
        .map(|(fav_type, stats)| {
            let mut entry = Dict::new();
            entry.insert("fav_type", Value::from(*fav_type));
            entry.insert("page_count", Value::from(stats.pages));
            entry.insert("api_total_count", Value::from(stats.api_total));
            entry.insert("collected_count", Value::from(stats.collected));
            entry.insert("downloaded_count", Value::from(stats.downloaded));
            Value::Dict(entry)
        })
        .collect();
    let mut payload = Dict::new();
    payload.insert("total_downloaded", Value::from(total_downloaded));
    payload.insert("total_api_images", Value::from(total_api_images));
    payload.insert("fav_type_stats", Value::List(fav_type_stats));
    NewNotification {
        kind: "summary".to_owned(),
        source: "jandan".to_owned(),
        header: "Jandan".to_owned(),
        title: "Update completed".to_owned(),
        body: format!("Downloaded {total_downloaded} images from {total_api_images} API images."),
        payload: Some(payload),
        ..NewNotification::default()
    }
}

fn io_error(error: &std::io::Error, path: &Path) -> JobError {
    let os = fav_net::download::PyOsError::from_io(error, Some(path));
    JobError::new(os.class, os.message)
}

#[cfg(test)]
mod tests;
