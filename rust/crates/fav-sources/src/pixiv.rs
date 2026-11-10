//! Public pixiv bookmarks, crawled through the www.pixiv.net ajax API.
//!
//! Python origin: `src/web/pixiv.py`. The crawl authenticates with the `PHPSESSID` cookie
//! captured on the 登录态 page (`site_sessions`, site `pixiv`); the account id is the
//! cookie's `{user_id}_` prefix unless `web.pixiv.user_id` overrides it. Originals on
//! i.pximg.net need only the Referer. Illustrations and manga are saved page by page,
//! ugoira (animated works) are rebuilt from their frame zip into one animated webp.
//!
//! Every output file is a `pixiv` row whose `downloaded`/`unavailable` flags say whether it
//! still needs work. A walk stops after two consecutive bookmark pages whose works are all
//! settled; rows a dead run left pending are retried from the database at the end.
//!
//! Never run against the real site from tests or development (`rust/docs/PLAN.md`): every
//! test talks to a local fake.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fav_core::filename::{MAX_TOTAL_BYTES, format_media_filename, sanitize};
use fav_core::logging::{self, Logger};
use fav_core::pyjson::{self, Dict, Value};
use fav_core::pypath;
use fav_core::pytext::{py_isdigit, py_strip};
use fav_core::pyurl::urlsplit;
use fav_db::{Db, Param};
use fav_net::reqwest::{self, header};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox};
use fav_runtime::pystr::str_value;
use fav_runtime::site_sessions;
use fav_runtime::source::{JobError, Registry, Source};
use sqlx::Row;

use crate::SourceContext;
use crate::kit::{self, HttpError, OutboxNotifier, SharedNotifier};

pub const JOB_KEY: &str = "pixiv";
/// The key the captured login is stored under in `site_sessions`.
pub const SESSION_SITE: &str = "pixiv";
pub const LOGIN_URL: &str = "https://accounts.pixiv.net/login?return_to=https%3A%2F%2Fwww.pixiv.net%2F";
/// Where the login page leaves the session cookie; read back from the browser.
pub const SESSION_COOKIE_URLS: [&str; 1] = ["https://www.pixiv.net/"];
/// Where the ajax API lives.
pub const AJAX_BASE: &str = "https://www.pixiv.net";
const REFERER: &str = "https://www.pixiv.net/";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const PAGE_LIMIT: i64 = 48;
const FULL_HIT_STOP_PAGES: u32 = 2;
const API_RETRY_DELAYS_SECONDS: [f64; 4] = [2.0, 5.0, 15.0, 30.0];
const API_RETRYABLE_STATUS_CODES: [u16; 4] = [429, 502, 503, 504];
/// What the bookmarks endpoint answers when the session is missing or expired.
const AUTH_STATUS_CODES: [u16; 3] = [400, 401, 403];
const HTTP_ERROR_MIN: u16 = 400;
const ILLUST_TYPE_UGOIRA: i64 = 2;
const SESSION_COOKIE: &str = "PHPSESSID";
const RELOGIN_HINT: &str = "Sign in to pixiv again on fav's 登录态 page.";
const AUTH_DEDUPE_KEY: &str = "pixiv:auth";
/// 0 = illustration, 1 = manga, 2 = ugoira.
const ILLUST_TYPES: [i64; 3] = [0, 1, ILLUST_TYPE_UGOIRA];
const MASKED_REASON: &str = "masked in bookmarks";
const ERROR_REASON_MAX_CHARS: usize = 500;
const EXT_FALLBACK: &str = "jpg";
/// Lossy q90 is visually clean on drawn frames; method 4 keeps long ugoira quick.
const UGOIRA_WEBP_QUALITY: f32 = 90.0;
const UGOIRA_WEBP_METHOD: usize = 4;

fn log() -> Logger {
    logging::get("pixiv")
}

// ---------- errors ----------

/// `PixivError`: a failure of the run, optionally under its own notification key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixivError {
    pub message: String,
    pub notification_dedupe_key: String,
}

impl PixivError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            notification_dedupe_key: String::new(),
        }
    }

    fn auth(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            notification_dedupe_key: AUTH_DEDUPE_KEY.to_owned(),
        }
    }

    fn into_job_error(self) -> JobError {
        let error = JobError::new("PixivError", self.message);
        if self.notification_dedupe_key.is_empty() {
            error
        } else {
            error.with_notification_dedupe_key(self.notification_dedupe_key)
        }
    }
}

/// What `_get_body` and the work steps can raise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// `PixivApiError`: the envelope said `error: true`.
    Api {
        message: String,
        status_code: u16,
    },
    Pixiv(PixivError),
    Http(HttpError),
    /// Anything else, as `(class name, str(exc))`.
    Other {
        class: String,
        message: String,
    },
}

impl Failure {
    fn other(class: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Other {
            class: class.into(),
            message: message.into(),
        }
    }

    fn io(error: &std::io::Error) -> Self {
        Self::other(kit::io_class_name(error), kit::py_os_error_text(error))
    }

    fn db(error: &impl std::fmt::Display) -> Self {
        Self::other("DatabaseError", error.to_string())
    }

    pub fn class_name(&self) -> &str {
        match self {
            Self::Api { .. } => "PixivApiError",
            Self::Pixiv(_) => "PixivError",
            Self::Http(error) => error.class_name(),
            Self::Other { class, .. } => class,
        }
    }

    /// `str(exc)`.
    pub fn message(&self) -> &str {
        match self {
            Self::Api { message, .. } | Self::Other { message, .. } => message,
            Self::Pixiv(error) => &error.message,
            Self::Http(error) => error.message(),
        }
    }

    fn into_job_error(self) -> JobError {
        match self {
            Self::Pixiv(error) => error.into_job_error(),
            Self::Http(error) => error.into_job_error(),
            other => JobError::new(other.class_name().to_owned(), other.message().to_owned()),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

// ---------- parsing ----------

/// `_to_int`: an int, or a string of digits; never a bool or a float.
///
/// Python's `int()` is unbounded and also reads non-ASCII decimal digits; ids and counts
/// pixiv sends are ASCII and fit in 64 bits, so anything else counts as no int here
/// (`DIVERGENCES.md` D55).
fn to_int(value: Option<&Value>) -> Result<Option<i64>, Failure> {
    match value {
        Some(Value::Int(int)) => Ok(i64::try_from(*int).ok()),
        Some(Value::Str(text)) => {
            let stripped = py_strip(text);
            if !py_isdigit(stripped) {
                return Ok(None);
            }
            if stripped.bytes().all(|byte| byte.is_ascii_digit()) {
                return Ok(stripped.parse().ok());
            }
            Err(Failure::other(
                "ValueError",
                format!("invalid literal for int() with base 10: {}", fav_runtime::pystr::repr_str(stripped)),
            ))
        }
        _ => Ok(None),
    }
}

/// `derive_user_id`: `'12345678_AbCd...'` gives `'12345678'`; empty unless the prefix is digits.
pub fn derive_user_id(phpsessid: &str) -> &str {
    match phpsessid.split_once('_') {
        Some((prefix, _)) if py_isdigit(prefix) => prefix,
        _ => "",
    }
}

/// One bookmarked work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixivWork {
    pub illust_id: i64,
    pub title: String,
    pub author: String,
    pub author_id: String,
    pub illust_type: i64,
    pub page_count: i64,
    pub masked: bool,
}

/// `str(value or '')`.
fn str_or_empty(value: Option<&Value>) -> String {
    value.filter(|value| value.is_truthy()).map(str_value).unwrap_or_default()
}

/// `parse_bookmark_works`: one page of `body.works[]`. A masked entry (a deleted or
/// private work, sent with an int id and a placeholder title) is kept so the crawl can
/// settle it; an unknown work type is skipped.
pub fn parse_bookmark_works(body: &Value) -> Result<Vec<PixivWork>, Failure> {
    let Some(raw_works) = body.get("works").and_then(Value::as_list) else {
        return Ok(Vec::new());
    };
    let mut works = Vec::new();
    for raw in raw_works {
        if raw.as_dict().is_none() {
            continue;
        }
        let Some(illust_id) = to_int(raw.get("id"))? else { continue };
        let masked = raw.get("isMasked").is_some_and(Value::is_truthy);
        let illust_type = to_int(raw.get("illustType"))?;
        if !masked && !illust_type.is_some_and(|kind| ILLUST_TYPES.contains(&kind)) {
            continue;
        }
        works.push(PixivWork {
            illust_id,
            title: str_or_empty(raw.get("title")),
            author: str_or_empty(raw.get("userName")),
            author_id: str_or_empty(raw.get("userId")),
            illust_type: illust_type.unwrap_or(0),
            page_count: to_int(raw.get("pageCount"))?.filter(|count| *count != 0).unwrap_or(1),
            masked,
        });
    }
    Ok(works)
}

/// `parse_page_urls`: the original URL of every page, in order.
pub fn parse_page_urls(body: &Value) -> Vec<String> {
    let Some(entries) = body.as_list() else { return Vec::new() };
    entries
        .iter()
        .filter_map(|entry| entry.as_dict()?.get("urls")?.as_dict()?.get("original")?.as_str())
        .filter(|original| !original.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `parse_ugoira_meta`: the frame zip URL and `[(frame file, delay ms)]`.
pub fn parse_ugoira_meta(body: &Value) -> Result<(String, Vec<(String, i64)>), Failure> {
    let zip_url = str_or_empty(body.get("originalSrc"));
    let mut frames = Vec::new();
    if let Some(raw_frames) = body.get("frames").and_then(Value::as_list) {
        for raw in raw_frames {
            if raw.as_dict().is_none() {
                continue;
            }
            let file = str_or_empty(raw.get("file"));
            let delay = to_int(raw.get("delay"))?;
            if let Some(delay) = delay.filter(|delay| *delay > 0)
                && !file.is_empty()
            {
                frames.push((file, delay));
            }
        }
    }
    if zip_url.is_empty() || frames.is_empty() {
        return Err(Failure::other(
            "ValueError",
            "ugoira_meta payload is missing the frame zip or the frame list",
        ));
    }
    Ok((zip_url, frames))
}

/// `infer_extension`: the URL path's suffix, lowercased, or `jpg`.
pub fn infer_extension(url: &str) -> Result<String, Failure> {
    let path = urlsplit(url, "").map_err(|error| Failure::other("ValueError", error.0))?.path;
    let suffix = pypath::suffix(&path);
    let extension = suffix.strip_prefix('.').unwrap_or(suffix).to_lowercase();
    Ok(if extension.is_empty() { EXT_FALLBACK.to_owned() } else { extension })
}

// ---------- ugoira ----------

/// `synthesize_ugoira_webp`: encode the frames (JPEG or PNG files) into one looping
/// animated webp with each frame's delay, as Pillow does with `loop=0, quality=90,
/// method=4` (key frames every 3 to 5, no mixed mode, `minimize_size` off). Frames are
/// decoded one at a time. The bytes differ from Pillow's; frame count, size and timing do
/// not (`DIVERGENCES.md` D51).
pub fn synthesize_ugoira_webp(frame_paths: &[PathBuf], delays_ms: &[i64], dst_path: &Path) -> Result<(), Failure> {
    use webp_animation::{AnimParams, ColorMode, Encoder, EncoderOptions, EncodingConfig, EncodingType, LossyEncodingConfig};

    let Some(first) = frame_paths.first() else {
        return Err(Failure::other("IndexError", "list index out of range"));
    };
    let first = decode_frame(first)?;
    let options = EncoderOptions {
        anim_params: AnimParams { loop_count: 0 },
        minimize_size: false,
        kmin: 3,
        kmax: 5,
        allow_mixed: false,
        verbose: false,
        color_mode: ColorMode::Rgba,
        encoding_config: Some(EncodingConfig {
            encoding_type: EncodingType::Lossy(LossyEncodingConfig::default()),
            quality: UGOIRA_WEBP_QUALITY,
            method: UGOIRA_WEBP_METHOD,
        }),
    };
    let encoder_error = |error: webp_animation::Error| Failure::other("OSError", format!("cannot encode the ugoira as WebP: {error:?}"));
    let mut encoder = Encoder::new_with_options((first.width(), first.height()), options).map_err(encoder_error)?;
    let mut timestamp: i64 = 0;
    let to_ms = |value: i64| i32::try_from(value).map_err(|_| Failure::other("OverflowError", "ugoira is too long"));
    for (index, (path, delay)) in frame_paths.iter().zip(delays_ms).enumerate() {
        let frame = if index == 0 { first.clone() } else { decode_frame(path)? };
        if frame.dimensions() != first.dimensions() {
            return Err(Failure::other("ValueError", "attempting to add frame of different size"));
        }
        encoder.add_frame(frame.as_raw(), to_ms(timestamp)?).map_err(encoder_error)?;
        timestamp += delay;
    }
    let webp = encoder.finalize(to_ms(timestamp)?).map_err(encoder_error)?;
    std::fs::write(dst_path, &*webp).map_err(|error| Failure::io(&error))
}

/// `Image.open(path)` converted to RGBA.
fn decode_frame(path: &Path) -> Result<image::RgbaImage, Failure> {
    let reader = image::ImageReader::open(path)
        .and_then(image::ImageReader::with_guessed_format)
        .map_err(|error| Failure::io(&error))?;
    let image = reader
        .decode()
        .map_err(|_| Failure::other("UnidentifiedImageError", format!("cannot identify image file '{}'", path.display())))?;
    Ok(image.to_rgba8())
}

/// `ZipFile.extractall(directory)`: member names with `/` as separator, empty, `.` and
/// `..` parts dropped.
fn extract_zip(zip_path: &Path, directory: &Path) -> Result<(), Failure> {
    let bad_zip = |error: zip::result::ZipError| match error {
        zip::result::ZipError::Io(error) => Failure::io(&error),
        _ => Failure::other("BadZipFile", "File is not a zip file"),
    };
    let file = std::fs::File::open(zip_path).map_err(|error| Failure::io(&error))?;
    let mut archive = zip::ZipArchive::new(file).map_err(bad_zip)?;
    for index in 0..archive.len() {
        let mut member = archive.by_index(index).map_err(bad_zip)?;
        let name = member.name().replace('\\', "/");
        let parts: Vec<&str> = name.split('/').filter(|part| !matches!(*part, "" | "." | "..")).collect();
        if parts.is_empty() {
            continue;
        }
        let target = parts.iter().fold(directory.to_path_buf(), |path, part| path.join(part));
        if member.is_dir() {
            std::fs::create_dir_all(&target).map_err(|error| Failure::io(&error))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| Failure::io(&error))?;
        }
        let mut out = std::fs::File::create(&target).map_err(|error| Failure::io(&error))?;
        std::io::copy(&mut member, &mut out).map_err(|error| Failure::io(&error))?;
    }
    Ok(())
}

/// `Pixiv._synthesize_from_zip`: unpack the frame zip beside it, encode the listed frames
/// in order, then move the result into place, so a crash never leaves a partial file at
/// the destination. `work_dir` is a temporary directory holding `ugoira.zip`.
pub fn synthesize_from_zip(work_dir: &Path, zip_path: &Path, frames: &[(String, i64)], dst_path: &Path) -> Result<(), Failure> {
    extract_zip(zip_path, work_dir)?;
    let mut frame_paths = Vec::with_capacity(frames.len());
    let mut delays = Vec::with_capacity(frames.len());
    for (name, delay) in frames {
        let frame_path = work_dir.join(name);
        if !frame_path.is_file() {
            return Err(Failure::Pixiv(PixivError::new(format!("ugoira zip is missing frame {name}"))));
        }
        frame_paths.push(frame_path);
        delays.push(*delay);
    }
    let encoded = work_dir.join("ugoira.webp");
    synthesize_ugoira_webp(&frame_paths, &delays, &encoded)?;
    kit::move_file(&encoded, dst_path).map_err(|error| Failure::io(&error))
}

// ---------- session verification ----------

/// `SessionCheck`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCheck {
    pub ok: bool,
    pub account: String,
    pub message: String,
}

fn json_value(text: &str) -> Option<Value> {
    pyjson::loads(text).ok()
}

/// `verify_session`: whether a `PHPSESSID` is a live login, asked of pixiv itself. Every
/// visitor gets a cookie; only the account's own private bookmarks (`rest=hide`) prove a
/// login.
pub async fn verify_session(phpsessid: &str, user_id: &str, proxy: &str) -> SessionCheck {
    verify_session_at(AJAX_BASE, phpsessid, user_id, proxy).await
}

/// [`verify_session`] against another ajax origin (tests).
pub async fn verify_session_at(ajax_base: &str, phpsessid: &str, user_id: &str, proxy: &str) -> SessionCheck {
    if phpsessid.is_empty() {
        return SessionCheck {
            ok: false,
            account: String::new(),
            message: "no PHPSESSID cookie yet".to_owned(),
        };
    }
    let account = if user_id.is_empty() { derive_user_id(phpsessid) } else { user_id }.to_owned();
    if account.is_empty() {
        return SessionCheck {
            ok: false,
            account,
            message: "PHPSESSID carries no user id; not signed in yet".to_owned(),
        };
    }
    let unreachable = |class: &str| SessionCheck {
        ok: false,
        account: account.clone(),
        message: format!("pixiv unreachable: {class}"),
    };
    let client = match build_client(proxy, Duration::from_secs(30), true) {
        Ok(client) => client,
        Err(error) => return unreachable(error.class_name()),
    };
    let url = format!("{ajax_base}/ajax/user/{account}/illusts/bookmarks");
    let request = client
        .get(&url)
        .query(&[("tag", ""), ("offset", "0"), ("limit", "1"), ("rest", "hide")])
        .header(header::COOKIE, format!("{SESSION_COOKIE}={phpsessid}"));
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return unreachable(HttpError::transport(&error).class_name()),
    };
    let status = response.status().as_u16();
    let payload = match response.text().await {
        Ok(text) => json_value(&text),
        Err(error) => return unreachable(HttpError::transport(&error).class_name()),
    };
    let rejected = status >= HTTP_ERROR_MIN
        || payload
            .as_ref()
            .is_none_or(|payload| payload.as_dict().is_none_or(|_| payload.get("error").is_some_and(Value::is_truthy)));
    if rejected {
        let detail = payload
            .as_ref()
            .filter(|payload| payload.as_dict().is_some())
            .map(|payload| str_or_empty(payload.get("message")))
            .unwrap_or_default();
        return SessionCheck {
            ok: false,
            account,
            message: py_strip(&format!("pixiv rejected the session (HTTP {status}) {detail}")).to_owned(),
        };
    }
    SessionCheck {
        ok: true,
        account,
        message: String::new(),
    }
}

// ---------- the crawler ----------

/// `httpx.AsyncClient(headers=UA+Referer(+Accept), proxy, timeout, follow_redirects)`.
fn build_client(proxy: &str, timeout: Duration, accept_json: bool) -> Result<reqwest::Client, HttpError> {
    let mut headers = header::HeaderMap::new();
    headers.insert(header::REFERER, header::HeaderValue::from_static(REFERER));
    if accept_json {
        headers.insert(header::ACCEPT, header::HeaderValue::from_static("application/json"));
    }
    let mut builder = fav_net::client_builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .redirect(reqwest::redirect::Policy::limited(20))
        .pool_max_idle_per_host(10);
    if !proxy.is_empty() {
        // An explicit proxy replaces the environment's, as in httpx.
        let proxy = reqwest::Proxy::all(proxy).map_err(|_| HttpError::Transport {
            class: "UnsupportedProtocol",
            message: "Unknown scheme for proxy URL".to_owned(),
            host: String::new(),
        })?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(|error| HttpError::transport(&error))
}

/// How long the crawl waits: before each API request, and between API retries.
#[derive(Debug, Clone)]
pub struct Pacing {
    pub sleep_request: Duration,
    pub retry_delays: Vec<Duration>,
}

impl Pacing {
    fn from_settings(sleep_request_seconds: f64) -> Self {
        Self {
            sleep_request: Duration::try_from_secs_f64(sleep_request_seconds.max(0.0)).unwrap_or(Duration::ZERO),
            retry_delays: API_RETRY_DELAYS_SECONDS
                .iter()
                .map(|seconds| Duration::from_secs_f64(*seconds))
                .collect(),
        }
    }
}

/// `Pixiv`: one run's clients, settings, database and notification seam.
pub struct Pixiv {
    db: Db,
    notifier: SharedNotifier,
    client: reqwest::Client,
    media_client: reqwest::Client,
    ajax_base: String,
    /// `cfg.path` as `str(Path)`.
    path: String,
    user_id: String,
    pacing: Pacing,
    /// The `Cookie` header the ajax client sends once the session is resolved.
    cookie: Option<String>,
}

impl std::fmt::Debug for Pixiv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The cookie is a login; never print it.
        f.debug_struct("Pixiv")
            .field("ajax_base", &self.ajax_base)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// The parts of `web.pixiv` a run uses.
#[derive(Debug, Clone)]
pub struct PixivConfig {
    pub path: String,
    pub user_id: String,
    pub sleep_request_seconds: f64,
    pub proxy: String,
}

impl PixivConfig {
    pub fn from_settings(section: &fav_core::settings::Pixiv) -> Self {
        Self {
            path: section.path.as_str().to_owned(),
            user_id: section.user_id.clone(),
            sleep_request_seconds: section.sleep_request_seconds,
            proxy: section.proxy.clone(),
        }
    }
}

/// `_ERROR_REASON_MAX_CHARS`: `reason[:500]`.
fn truncate_reason(reason: &str) -> String {
    reason.chars().take(ERROR_REASON_MAX_CHARS).collect()
}

/// `delay` seconds as `%.1f`.
fn seconds_1(delay: Duration) -> String {
    format!("{:.1}", delay.as_secs_f64())
}

impl Pixiv {
    pub fn new(db: Db, notifier: SharedNotifier, config: &PixivConfig) -> Result<Self, JobError> {
        let client = build_client(&config.proxy, Duration::from_mins(1), true).map_err(HttpError::into_job_error)?;
        let media_client = build_client(&config.proxy, Duration::from_mins(2), false).map_err(HttpError::into_job_error)?;
        Ok(Self {
            db,
            notifier,
            client,
            media_client,
            ajax_base: AJAX_BASE.to_owned(),
            path: config.path.clone(),
            user_id: config.user_id.clone(),
            pacing: Pacing::from_settings(config.sleep_request_seconds),
            cookie: None,
        })
    }

    /// Talk to another ajax origin and wait differently (tests).
    #[must_use]
    pub fn with_ajax_base(mut self, ajax_base: &str, pacing: Pacing) -> Self {
        ajax_base.trim_end_matches('/').clone_into(&mut self.ajax_base);
        self.pacing = pacing;
        self
    }

    async fn pace(&self) {
        if !self.pacing.sleep_request.is_zero() {
            tokio::time::sleep(self.pacing.sleep_request).await;
        }
    }

    /// `_resolve_session`: the captured `PHPSESSID`; without one there is nothing to crawl with.
    async fn resolve_session(&self) -> Result<String, JobError> {
        let stored = site_sessions::load(&self.db, SESSION_SITE)
            .await
            .map_err(|error| JobError::new("DatabaseError", error.to_string()))?;
        let phpsessid = stored
            .as_ref()
            .map(|session| session.cookie(SESSION_COOKIE).to_owned())
            .unwrap_or_default();
        if phpsessid.is_empty() {
            return Err(PixivError::auth(format!("pixiv is not signed in. {RELOGIN_HINT}")).into_job_error());
        }
        Ok(phpsessid)
    }

    /// `_get_body`: one ajax GET, the `{error, message, body}` envelope unwrapped.
    ///
    /// Throttling and gateway errors, and connect, timeout and protocol failures, are
    /// retried after 2, 5, 15 and 30 s. `error: true` is a [`Failure::Api`]; any other
    /// non-2xx is an `HTTPStatusError`.
    pub async fn get_body(&self, url: &str, params: &[(&str, String)]) -> Result<Value, Failure> {
        let attempts = self.pacing.retry_delays.len() + 1;
        for attempt in 1..=attempts {
            self.pace().await;
            let mut request = self.client.get(url);
            if !params.is_empty() {
                request = request.query(params);
            }
            if let Some(cookie) = &self.cookie {
                request = request.header(header::COOKIE, cookie);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    let error = HttpError::transport(&error);
                    if !error.is_connect_timeout_or_protocol() || attempt >= attempts {
                        return Err(Failure::Http(error));
                    }
                    let delay = self.pacing.retry_delays[attempt - 1];
                    log().warning(format_args!(
                        "Pixiv API request failed url={url} attempt={attempt}/{attempts}, retry in {}s: {}: {}",
                        seconds_1(delay),
                        error.class_name(),
                        error.message()
                    ));
                    tokio::time::sleep(delay).await;
                    continue;
                }
            };
            let status = response.status().as_u16();
            if API_RETRYABLE_STATUS_CODES.contains(&status) && attempt < attempts {
                let delay = self.pacing.retry_delays[attempt - 1];
                log().warning(format_args!(
                    "Pixiv API returned HTTP {status} url={url} attempt={attempt}/{attempts}, retry in {}s",
                    seconds_1(delay)
                ));
                drop(response);
                tokio::time::sleep(delay).await;
                continue;
            }
            let status_check = kit::raise_for_status(&response);
            let text = response.text().await.map_err(|error| Failure::Http(HttpError::transport(&error)))?;
            let payload = json_value(&text);
            if let Some(payload) = payload.as_ref().filter(|payload| payload.as_dict().is_some())
                && payload.get("error").is_some_and(Value::is_truthy)
            {
                let message = py_strip(&str_or_empty(payload.get("message"))).to_owned();
                let message = if message.is_empty() { format!("http {status}") } else { message };
                return Err(Failure::Api {
                    message,
                    status_code: status,
                });
            }
            status_check.map_err(Failure::Http)?;
            return match payload {
                Some(Value::Dict(mut dict)) => Ok(dict.remove("body").unwrap_or(Value::Null)),
                _ => Err(Failure::Pixiv(PixivError::new(format!("Unexpected pixiv API payload from {url}")))),
            };
        }
        Err(Failure::other("RuntimeError", "Pixiv API request failed without response"))
    }

    fn bookmarks_url(&self, user_id: &str) -> String {
        format!("{}/ajax/user/{user_id}/illusts/bookmarks", self.ajax_base)
    }

    /// `_fetch_bookmark_page`: public bookmarks, 48 per page. A rejected session is one
    /// `pixiv:auth` failure.
    pub async fn fetch_bookmark_page(&self, user_id: &str, offset: i64) -> Result<Value, Failure> {
        let params = [
            ("tag", String::new()),
            ("offset", offset.to_string()),
            ("limit", PAGE_LIMIT.to_string()),
            ("rest", "show".to_owned()),
        ];
        let body = match self.get_body(&self.bookmarks_url(user_id), &params).await {
            Ok(body) => body,
            Err(Failure::Api { message, .. }) => {
                return Err(Failure::Pixiv(PixivError::auth(format!(
                    "pixiv rejected the bookmarks request ({message}). {RELOGIN_HINT}"
                ))));
            }
            Err(Failure::Http(HttpError::Status { status, .. })) if AUTH_STATUS_CODES.contains(&status) => {
                return Err(Failure::Pixiv(PixivError::auth(format!(
                    "pixiv answered the bookmarks request with HTTP {status}. {RELOGIN_HINT}"
                ))));
            }
            Err(other) => return Err(other),
        };
        if body.as_dict().is_none() {
            return Err(Failure::Pixiv(PixivError::new("pixiv bookmarks payload has no body")));
        }
        Ok(body)
    }

    // ---------- the pixiv table ----------

    /// `_upsert_rows`: one row per output file of a work; `entries` is `[(num, source_url)]`.
    async fn upsert_rows(&self, work: &PixivWork, entries: &[(i64, String)]) -> Result<(), Failure> {
        let rows = entries
            .iter()
            .map(|(num, source_url)| {
                vec![
                    Param::from(work.illust_id),
                    Param::from(*num),
                    Param::from(work.illust_type),
                    Param::from(work.title.as_str()),
                    Param::from(work.author.as_str()),
                    Param::from(work.author_id.as_str()),
                    Param::from(work.page_count),
                    Param::from(source_url.as_str()),
                ]
            })
            .collect();
        self.db
            .insert_batch(
                "pixiv",
                &[
                    "illust_id",
                    "num",
                    "illust_type",
                    "title",
                    "author",
                    "author_id",
                    "page_count",
                    "source_url",
                ],
                rows,
                Some(
                    "(illust_id, num) DO UPDATE SET \
                     illust_type = excluded.illust_type, \
                     title = excluded.title, \
                     author = excluded.author, \
                     author_id = excluded.author_id, \
                     page_count = excluded.page_count, \
                     source_url = excluded.source_url",
                ),
                fav_db::batch::DEFAULT_INSERT_CHUNK_SIZE,
            )
            .await
            .map_err(|error| Failure::db(&error))
    }

    async fn execute(&self, sql: &str, params: Vec<Param>) -> Result<(), Failure> {
        self.db
            .query_transaction(vec![fav_db::Statement::new(sql, params)])
            .await
            .map(|_| ())
            .map_err(|error| Failure::db(&error))
    }

    async fn insert_stub_row(&self, work: &PixivWork) -> Result<(), Failure> {
        self.execute(
            "INSERT INTO pixiv (illust_id, num, illust_type, title, author, author_id) VALUES ($1, 0, $2, $3, $4, $5) \
             ON CONFLICT DO NOTHING;",
            vec![
                Param::from(work.illust_id),
                Param::from(work.illust_type),
                Param::from(work.title.as_str()),
                Param::from(work.author.as_str()),
                Param::from(work.author_id.as_str()),
            ],
        )
        .await
    }

    /// `_mark_masked`: pending rows of a gone work become unavailable; downloaded ones stay.
    async fn mark_masked(&self, illust_id: i64) -> Result<(), Failure> {
        self.execute(
            "INSERT INTO pixiv (illust_id, num, unavailable, last_error) VALUES ($1, 0, 1, $2) ON CONFLICT DO NOTHING;",
            vec![Param::from(illust_id), Param::from(MASKED_REASON)],
        )
        .await?;
        self.execute(
            "UPDATE pixiv SET unavailable = 1, last_error = $1 WHERE illust_id = $2 AND downloaded = 0 AND unavailable = 0;",
            vec![Param::from(MASKED_REASON), Param::from(illust_id)],
        )
        .await
    }

    async fn mark_work_unavailable(&self, work: &PixivWork, reason: &str) -> Result<(), Failure> {
        self.insert_stub_row(work).await?;
        self.execute(
            "UPDATE pixiv SET unavailable = 1, failed_count = failed_count + 1, last_error = $1 WHERE illust_id = $2 AND downloaded = 0;",
            vec![Param::from(truncate_reason(reason)), Param::from(work.illust_id)],
        )
        .await
    }

    /// `_mark_work_failed`: the stub keeps a work whose detail fetch failed visible as
    /// pending, so the next run retries it.
    async fn mark_work_failed(&self, work: &PixivWork, reason: &str) -> Result<(), Failure> {
        self.insert_stub_row(work).await?;
        self.execute(
            "UPDATE pixiv SET failed_count = failed_count + 1, last_error = $1 \
             WHERE illust_id = $2 AND downloaded = 0 AND unavailable = 0;",
            vec![Param::from(truncate_reason(reason)), Param::from(work.illust_id)],
        )
        .await
    }

    async fn mark_downloaded(&self, illust_id: i64, num: i64, dst_path: &str) -> Result<(), Failure> {
        self.execute(
            "UPDATE pixiv SET downloaded = 1, local_path = $1, unavailable = 0, failed_count = 0, last_error = '' \
             WHERE illust_id = $2 AND num = $3;",
            vec![Param::from(dst_path), Param::from(illust_id), Param::from(num)],
        )
        .await
    }

    /// `_processed_work_ids`: works with at least one row and none pending.
    pub async fn processed_work_ids(&self, illust_ids: &[i64]) -> Result<HashSet<i64>, Failure> {
        if illust_ids.is_empty() {
            return Ok(HashSet::new());
        }
        let rows = sqlx::query(
            "SELECT illust_id, COUNT(*) AS total, \
                    SUM(CASE WHEN downloaded = 0 AND unavailable = 0 THEN 1 ELSE 0 END) AS pending \
             FROM pixiv WHERE illust_id = ANY($1) GROUP BY illust_id;",
        )
        .bind(illust_ids)
        .fetch_all(self.db.pool())
        .await
        .map_err(|error| Failure::db(&error))?;
        let mut processed = HashSet::new();
        for row in rows {
            let illust_id: i64 = row.try_get("illust_id").map_err(|error| Failure::db(&error))?;
            let total: i64 = row.try_get("total").map_err(|error| Failure::db(&error))?;
            let pending: Option<i64> = row.try_get("pending").map_err(|error| Failure::db(&error))?;
            if total > 0 && pending.unwrap_or(0) == 0 {
                processed.insert(illust_id);
            }
        }
        Ok(processed)
    }

    /// `_crawl_bookmarks`: newest first, collecting works that still need processing,
    /// until two consecutive pages are fully settled or the listing ends. Masked bookmarks
    /// are settled on the way. Returns the works, the pages read and pixiv's total.
    pub async fn crawl_bookmarks(&self, user_id: &str) -> Result<(Vec<PixivWork>, i64, i64), Failure> {
        let mut offset: i64 = 0;
        let mut page_index: i64 = 0;
        let mut total_reported: i64 = 0;
        let mut full_hit_pages = 0;
        let mut collected = Vec::new();
        let mut collected_ids = HashSet::new();
        loop {
            page_index += 1;
            let body = self.fetch_bookmark_page(user_id, offset).await?;
            if let Some(total) = to_int(body.get("total"))?.filter(|total| *total != 0) {
                total_reported = total;
            }
            let works = parse_bookmark_works(&body)?;
            if works.is_empty() {
                log().info(format_args!("Pixiv bookmarks reached end page={page_index} offset={offset}"));
                break;
            }
            for work in works.iter().filter(|work| work.masked) {
                self.mark_masked(work.illust_id).await?;
            }
            let visible: Vec<&PixivWork> = works.iter().filter(|work| !work.masked).collect();
            let page_ids: Vec<i64> = visible
                .iter()
                .map(|work| work.illust_id)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let processed = self.processed_work_ids(&page_ids).await?;
            if processed.len() == page_ids.len() {
                full_hit_pages += 1;
                log().info(format_args!(
                    "Pixiv bookmarks full-hit page={page_index} works={} consecutive_full_hits={full_hit_pages}/{FULL_HIT_STOP_PAGES}",
                    works.len()
                ));
                if full_hit_pages >= FULL_HIT_STOP_PAGES {
                    log().info("Pixiv bookmarks reached full-hit stop condition");
                    break;
                }
            } else {
                full_hit_pages = 0;
                for work in visible {
                    if processed.contains(&work.illust_id) || !collected_ids.insert(work.illust_id) {
                        continue;
                    }
                    collected.push(work.clone());
                }
            }
            offset += i64::try_from(works.len()).unwrap_or(i64::MAX);
            if total_reported != 0 && offset >= total_reported {
                log().info(format_args!("Pixiv bookmarks walked all {total_reported} entries"));
                break;
            }
        }
        Ok((collected, page_index, total_reported))
    }

    /// `_build_output_path`: `<path>/<author>/[author]title [id_pN].ext`, `[id_ugoira]`
    /// for an ugoira; the author id, then `unknown`, when the name sanitizes to nothing.
    pub fn build_output_path(&self, work: &PixivWork, num: i64, ext: &str) -> String {
        let media_id = if work.illust_type == ILLUST_TYPE_UGOIRA {
            format!("{}_ugoira", work.illust_id)
        } else {
            format!("{}_p{num}", work.illust_id)
        };
        let sanitized = sanitize(&work.author, 200);
        let folder = if !sanitized.is_empty() {
            sanitized.as_str()
        } else if !work.author_id.is_empty() {
            work.author_id.as_str()
        } else {
            "unknown"
        };
        let uploader = (!work.author.is_empty()).then_some(work.author.as_str());
        let filename = format_media_filename(&work.title, &media_id, uploader, ext, MAX_TOTAL_BYTES);
        pypath::join(&pypath::join(&self.path, folder), &filename)
    }

    /// `_download_image`: fetch one original into place unless it is already there;
    /// returns whether a new file was written. Streams to `<name>.part` first (D52).
    pub async fn download_image(&self, url: &str, dst_path: &str) -> Result<bool, Failure> {
        let dst = Path::new(dst_path);
        if let Some(parent) = dst.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await.map_err(|error| Failure::io(&error))?;
        }
        if dst.exists() {
            return Ok(false);
        }
        let response = self
            .media_client
            .get(url)
            .send()
            .await
            .map_err(|error| Failure::Http(HttpError::transport(&error)))?;
        kit::raise_for_status(&response).map_err(Failure::Http)?;
        let part = format!("{dst_path}.part");
        let streamed = kit::stream_body_to(response, Path::new(&part)).await;
        if let Err(error) = streamed {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(match error {
                kit::DownloadError::Http(error) => Failure::Http(error),
                kit::DownloadError::Io(error) => Failure::io(&error),
            });
        }
        tokio::fs::rename(&part, dst).await.map_err(|error| Failure::io(&error))?;
        Ok(true)
    }

    async fn process_ugoira(&self, work: &PixivWork) -> Result<u64, Failure> {
        let body = self
            .get_body(&format!("{}/ajax/illust/{}/ugoira_meta", self.ajax_base, work.illust_id), &[])
            .await?;
        if body.as_dict().is_none() {
            return Err(Failure::Pixiv(PixivError::new(format!(
                "pixiv ugoira_meta payload has no body for {}",
                work.illust_id
            ))));
        }
        let (zip_url, frames) = parse_ugoira_meta(&body)?;
        self.upsert_rows(work, &[(0, zip_url.clone())]).await?;

        let dst_path = self.build_output_path(work, 0, "webp");
        let dst = PathBuf::from(&dst_path);
        if let Some(parent) = dst.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await.map_err(|error| Failure::io(&error))?;
        }
        let mut downloaded = 0;
        if !dst.exists() {
            let response = self
                .media_client
                .get(&zip_url)
                .send()
                .await
                .map_err(|error| Failure::Http(HttpError::transport(&error)))?;
            kit::raise_for_status(&response).map_err(Failure::Http)?;
            let work_dir = tempfile::tempdir().map_err(|error| Failure::io(&error))?;
            let zip_path = work_dir.path().join("ugoira.zip");
            kit::stream_body_to(response, &zip_path).await.map_err(|error| match error {
                kit::DownloadError::Http(error) => Failure::Http(error),
                kit::DownloadError::Io(error) => Failure::io(&error),
            })?;
            let encoded = tokio::task::spawn_blocking(move || {
                let outcome = synthesize_from_zip(work_dir.path(), &zip_path, &frames, &dst);
                drop(work_dir);
                outcome
            })
            .await
            .map_err(|error| Failure::other("RuntimeError", error.to_string()))?;
            encoded?;
            downloaded = 1;
        }
        self.mark_downloaded(work.illust_id, 0, &dst_path).await?;
        Ok(downloaded)
    }

    async fn process_illust(&self, work: &PixivWork) -> Result<u64, Failure> {
        let body = self
            .get_body(&format!("{}/ajax/illust/{}/pages", self.ajax_base, work.illust_id), &[])
            .await?;
        let urls = parse_page_urls(&body);
        if urls.is_empty() {
            return Err(Failure::Pixiv(PixivError::new(format!(
                "pixiv pages payload has no originals for {}",
                work.illust_id
            ))));
        }
        let entries: Vec<(i64, String)> = (0_i64..).zip(urls.iter().cloned()).collect();
        self.upsert_rows(work, &entries).await?;
        let mut downloaded = 0;
        for (num, url) in &entries {
            let dst_path = self.build_output_path(work, *num, &infer_extension(url)?);
            if self.download_image(url, &dst_path).await? {
                downloaded += 1;
            }
            self.mark_downloaded(work.illust_id, *num, &dst_path).await?;
        }
        Ok(downloaded)
    }

    /// `_process_work`: a failure settles or defers the work instead of ending the run.
    /// `error: true` from a work endpoint means it is gone; anything else is retried next run.
    pub async fn process_work(&self, work: &PixivWork, progress: &str) -> Result<u64, JobError> {
        let outcome = if work.illust_type == ILLUST_TYPE_UGOIRA {
            self.process_ugoira(work).await
        } else {
            self.process_illust(work).await
        };
        let settle = |failure: Failure| failure.into_job_error();
        match outcome {
            Ok(downloaded) => {
                if downloaded > 0 {
                    log().notice(format_args!("Pixiv downloaded {progress} id={} files={downloaded}", work.illust_id));
                }
                Ok(downloaded)
            }
            Err(Failure::Api { message, .. }) => {
                let reason = format!("work endpoint error: {message}");
                self.mark_work_unavailable(work, &reason).await.map_err(settle)?;
                log().info(format_args!(
                    "Marked unavailable pixiv work {progress} id={}: {reason}",
                    work.illust_id
                ));
                Ok(0)
            }
            Err(failure) => {
                self.mark_work_failed(work, &format!("{}: {}", failure.class_name(), failure.message()))
                    .await
                    .map_err(settle)?;
                log().warning(format_args!(
                    "Failed to process pixiv work {progress} id={}: {failure}",
                    work.illust_id
                ));
                Ok(0)
            }
        }
    }

    /// `_retry_pending_from_db`: works earlier runs left pending, back through the work
    /// endpoints (a stub row has no URL, and an ugoira's frame table is never stored).
    pub async fn retry_pending_from_db(&self, handled_ids: &HashSet<i64>) -> Result<u64, JobError> {
        let rows = sqlx::query(
            "SELECT illust_id, num, illust_type, title, author, author_id, page_count \
             FROM pixiv WHERE downloaded = 0 AND unavailable = 0 ORDER BY created_at ASC;",
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(|error| JobError::new("DatabaseError", error.to_string()))?;
        let mut order = Vec::new();
        let mut pending: HashMap<i64, PixivWork> = HashMap::new();
        for row in rows {
            let get = |column: &str| row.try_get::<Option<i64>, _>(column).ok().flatten();
            let text = |column: &str| row.try_get::<Option<String>, _>(column).ok().flatten().unwrap_or_default();
            let Some(illust_id) = get("illust_id") else { continue };
            if handled_ids.contains(&illust_id) || pending.contains_key(&illust_id) {
                continue;
            }
            let illust_type = row.try_get::<Option<i32>, _>("illust_type").ok().flatten().map_or(0, i64::from);
            let page_count = row.try_get::<Option<i32>, _>("page_count").ok().flatten().map_or(0, i64::from);
            order.push(illust_id);
            pending.insert(
                illust_id,
                PixivWork {
                    illust_id,
                    title: text("title"),
                    author: text("author"),
                    author_id: text("author_id"),
                    illust_type,
                    page_count: if page_count == 0 { 1 } else { page_count },
                    masked: false,
                },
            );
        }
        let total = order.len();
        let mut downloaded = 0;
        for (index, illust_id) in order.iter().enumerate() {
            let work = &pending[illust_id];
            downloaded += self.process_work(work, &format!("retry {}/{total}", index + 1)).await?;
        }
        Ok(downloaded)
    }

    async fn notify_summary(&self, downloaded: u64, works: usize, total_reported: i64) {
        let mut payload = Dict::new();
        payload.insert("downloaded", Value::from(i64::try_from(downloaded).unwrap_or(i64::MAX)));
        payload.insert("works", Value::from(i64::try_from(works).unwrap_or(i64::MAX)));
        payload.insert("total_reported", Value::from(total_reported));
        let notification = NewNotification {
            kind: "summary".to_owned(),
            source: JOB_KEY.to_owned(),
            header: "Pixiv".to_owned(),
            title: "Bookmarks update completed".to_owned(),
            body: format!("Downloaded {downloaded} files from {works} bookmarked works."),
            payload: Some(payload),
            ..NewNotification::default()
        };
        if let Err(error) = self.notifier.enqueue(notification).await {
            log().warning(format_args!("Failed to enqueue pixiv summary notification: {error}"));
        }
    }

    /// `update()`: one run.
    pub async fn update(&mut self) -> Result<(), JobError> {
        let phpsessid = self.resolve_session().await?;
        let user_id = if self.user_id.is_empty() {
            derive_user_id(&phpsessid).to_owned()
        } else {
            self.user_id.clone()
        };
        if user_id.is_empty() {
            return Err(
                PixivError::auth("Could not derive the pixiv user id from PHPSESSID; set web.pixiv.user_id explicitly.").into_job_error(),
            );
        }
        self.cookie = Some(format!("{SESSION_COOKIE}={phpsessid}"));

        tokio::fs::create_dir_all(&self.path)
            .await
            .map_err(|error| JobError::new(kit::io_class_name(&error), kit::py_os_error_text(&error)))?;

        let (works, page_count, total_reported) = match self.crawl_bookmarks(&user_id).await {
            Ok(result) => result,
            Err(Failure::Pixiv(error)) => {
                if error.notification_dedupe_key == AUTH_DEDUPE_KEY {
                    site_sessions::record_check(&self.db, SESSION_SITE, false, &error.message)
                        .await
                        .map_err(|error| JobError::new("DatabaseError", error.to_string()))?;
                }
                return Err(error.into_job_error());
            }
            Err(other) => return Err(other.into_job_error()),
        };
        site_sessions::record_check(&self.db, SESSION_SITE, true, "")
            .await
            .map_err(|error| JobError::new("DatabaseError", error.to_string()))?;
        log().info(format_args!(
            "Pixiv bookmarks scanned pages={page_count} total_reported={total_reported} works_to_process={}",
            works.len()
        ));

        let mut downloaded = 0;
        for (index, work) in works.iter().enumerate() {
            downloaded += self.process_work(work, &format!("{}/{}", index + 1, works.len())).await?;
        }
        let handled: HashSet<i64> = works.iter().map(|work| work.illust_id).collect();
        downloaded += self.retry_pending_from_db(&handled).await?;

        log().info(format_args!("Pixiv downloaded {downloaded} new files"));
        if downloaded > 0 {
            self.notify_summary(downloaded, works.len(), total_reported).await;
        }
        Ok(())
    }
}

// ---------- the job ----------

/// The `pixiv` job: settings and clients are read when the run starts.
struct PixivJob {
    ctx: SourceContext,
}

impl Source for PixivJob {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self
                .ctx
                .settings
                .load()
                .await
                .map_err(|error| JobError::new("RuntimeError", error.to_string()))?;
            let notifier: SharedNotifier = Arc::new(OutboxNotifier(Outbox::new(self.ctx.db.clone(), Arc::clone(&self.ctx.settings))));
            let config = PixivConfig::from_settings(&settings.web.pixiv);
            let mut source = Pixiv::new(self.ctx.db.clone(), notifier, &config)?;
            source.update().await
        })
    }
}

/// Register the `pixiv` job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run(JOB_KEY, move || Box::new(PixivJob { ctx: ctx.clone() }) as Box<dyn Source>);
}

#[cfg(test)]
mod tests;
