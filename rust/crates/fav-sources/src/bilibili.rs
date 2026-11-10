//! Bilibili favourite lists and watch-later lists, downloaded with yt-dlp.
//!
//! Python origin: `src/web/bilibili.py`. Each configured account's session is the one
//! captured on the 登录态 page (`site_sessions`, site `bilibili-<account>`): a plain browser
//! login, renewed here ([`session`]) and verified against `/x/web-interface/nav` before the
//! account is walked, so a session Bilibili has dropped is reported as a sign-in to redo
//! rather than as an API error halfway through.
//!
//! The web API calls bilibili-api-python made are made directly ([`api`]); yt-dlp downloads
//! each new video (`fav_net::subprocess`) with a per-account Netscape cookie file, and the
//! files are renamed into the list's directory. Every downloaded video is a `bilibili` row;
//! a video already in the table is skipped, and a failure notification an earlier run left
//! for it is resolved.
//!
//! Never run against the real site from tests or development (`rust/docs/PLAN.md`): every
//! test talks to a local fake, and yt-dlp is replaced by a scripted runner.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fav_core::filename::{MAX_TOTAL_BYTES, ensure_unique_path, format_video_filename};
use fav_core::logging::{self, Logger};
use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::{py_isdigit, py_strip};
use fav_core::settings::BilibiliAccount;
use fav_db::Db;
use fav_net::reqwest::{self, header};
use fav_net::subprocess::{Runner, SystemRunner};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox, Resolution, format_job_failure_dedupe_key};
use fav_runtime::pystr::{repr_str, str_value};
use fav_runtime::site_sessions::{self, SiteSession, StoredCookie, cookie_expiry};
use fav_runtime::source::{JobError, Registry, Source};
use sqlx::Row;

use crate::SourceContext;
use crate::kit::{self, HttpError, OutboxNotifier, SharedNotifier};
use crate::pydecode::{self, PyError, py_eq_int};

pub mod api;
mod cookiejar;
pub mod session;

use api::{Api, ApiError, Credential, Video};
use session::{Endpoints, RenewError, Renewal};

pub const JOB_KEY: &str = "bilibili";
/// Where the 登录态 page sends the login window.
pub const LOGIN_URL: &str = "https://passport.bilibili.com/login";
/// The pages whose cookies make up the session.
pub const SESSION_COOKIE_URLS: [&str; 1] = ["https://www.bilibili.com/"];
/// The cookie a login sets and a crawl needs first.
pub const SESSION_COOKIE: &str = "SESSDATA";
/// What a credential is built from; Bilibili sets `buvid3` on the first page view, the
/// rest when the login completes.
pub const REQUIRED_COOKIES: [&str; 4] = ["SESSDATA", "bili_jct", "buvid3", "DedeUserID"];

/// Sentinel `fav_id` for watch-later rows; the table has no separate column for it.
const TOVIEW_FAV_ID: i64 = -1;
const KIBIBYTE: f64 = 1024.0;
const NAV_PATH: &str = "/x/web-interface/nav";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const NOT_LOGGED_IN_CODE: i128 = -101;
const HTTP_OK: u16 = 200;
const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);
const GATHER_LIMIT: usize = 5;
const DOWNLOAD_ATTEMPTS: u32 = 3;
const DOWNLOAD_BASE_DELAY_SECONDS: f64 = 5.0;

fn log() -> Logger {
    logging::get("bilibili")
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

// ---------- texts ----------

/// The `site_sessions` key, and 登录态 site, of one configured account.
pub fn session_site(account_name: &str) -> String {
    format!("bilibili-{account_name}")
}

pub fn relogin_hint(account_name: &str) -> String {
    format!("Sign in to Bilibili account {account_name} again on fav's 登录态 page.")
}

fn download_failure_key(bvid: &str) -> String {
    format!("bilibili:download:{bvid}")
}

fn download_failure_notification_key(bvid: &str) -> String {
    format_job_failure_dedupe_key(JOB_KEY, &download_failure_key(bvid))
}

fn auth_dedupe_key(account_name: &str) -> String {
    format!("bilibili:auth:{account_name}")
}

/// `SessionError`: an account has no usable login.
fn session_error(message: String, account_name: Option<&str>) -> JobError {
    let error = JobError::new("SessionError", message);
    match account_name {
        Some(name) => error.with_notification_dedupe_key(auth_dedupe_key(name)),
        None => error,
    }
}

fn py_error(error: PyError) -> JobError {
    error.into_job_error()
}

/// A failed statement, as psycopg would have raised it.
fn sql_error(error: sqlx::Error) -> JobError {
    crate::db_job_error(&fav_db::DbError::from(error))
}

/// A failed `site_sessions` call.
fn session_store_error(error: fav_runtime::Error) -> JobError {
    match error {
        fav_runtime::Error::Db(error) => crate::db_job_error(&error),
        other => JobError::new("RuntimeError", other.to_string()),
    }
}

/// An `OSError` for `path`, with the file name the way Python's message carries it.
fn os_error(error: &std::io::Error, path: &Path) -> JobError {
    JobError::new(
        kit::io_class_name(error),
        format!("{}: {}", kit::py_os_error_text(error), repr_str(&path.to_string_lossy())),
    )
}

// ---------- Python value access ----------

/// `value.get(key)`: an `AttributeError` unless `value` is a dict.
fn py_get<'a>(value: &'a Value, key: &str) -> Result<Option<&'a Value>, PyError> {
    match value {
        Value::Dict(dict) => Ok(dict.get(key)),
        other => Err(PyError::new(
            "AttributeError",
            format!("'{}' object has no attribute 'get'", crate::pyerr::type_name(other)),
        )),
    }
}

/// `value.get(key, {})`.
fn py_get_or_empty(value: &Value, key: &str) -> Result<Value, PyError> {
    Ok(py_get(value, key)?.cloned().unwrap_or_else(|| Value::Dict(Dict::new())))
}

/// `value[key]` with a string key.
fn py_item<'a>(value: &'a Value, key: &str) -> Result<&'a Value, PyError> {
    match value {
        Value::Dict(dict) => dict.get(key).ok_or_else(|| PyError::new("KeyError", repr_str(key))),
        Value::List(_) => Err(PyError::new("TypeError", "list indices must be integers or slices, not str")),
        Value::Str(_) => Err(PyError::new("TypeError", "string indices must be integers, not 'str'")),
        other => Err(PyError::new(
            "TypeError",
            format!("'{}' object is not subscriptable", crate::pyerr::type_name(other)),
        )),
    }
}

/// `for item in value`.
fn py_iter(value: &Value) -> Result<Vec<Value>, PyError> {
    match value {
        Value::List(items) => Ok(items.clone()),
        Value::Dict(dict) => Ok(dict.iter().map(|(key, _)| Value::Str(fav_runtime::pystr::str_key(key))).collect()),
        Value::Str(text) => Ok(text.chars().map(|ch| Value::Str(ch.to_string())).collect()),
        other => Err(PyError::new(
            "TypeError",
            format!("'{}' object is not iterable", crate::pyerr::type_name(other)),
        )),
    }
}

/// `Video(bvid=item['bvid'])` for one entry of a list answer.
fn video_of(item: &Value) -> Result<Video, JobError> {
    match py_item(item, "bvid").map_err(py_error)? {
        Value::Str(bvid) => Video::new(bvid).map_err(ApiError::into_job_error),
        other => Err(JobError::new(
            "TypeError",
            format!("expected string or bytes-like object, got '{}'", crate::pyerr::type_name(other)),
        )),
    }
}

// ---------- helpers held to goldens ----------

/// `_format_file_size`.
pub fn format_file_size(size_bytes: Option<i128>) -> Option<String> {
    let size_bytes = size_bytes.filter(|size| *size >= 0)?;
    #[allow(clippy::cast_precision_loss)] // reason: `float(size_bytes)`
    let mut size = size_bytes as f64;
    if size < KIBIBYTE {
        return Some(format!("{size_bytes} B"));
    }
    let units = ["KB", "MB", "GB", "TB"];
    for (index, unit) in units.iter().enumerate() {
        size /= KIBIBYTE;
        if size < KIBIBYTE || index == units.len() - 1 {
            return Some(format!("{size:.1} {unit}"));
        }
    }
    None
}

/// `_to_positive_int`: a positive int from an int, float or digit string; never a bool.
pub fn to_positive_int(value: Option<&Value>) -> Result<Option<i128>, PyError> {
    let number = match value {
        Some(Value::Int(_) | Value::Float(_)) => pydecode::py_int(value.unwrap_or(&Value::Null))?,
        Some(Value::Str(text)) => {
            let stripped = py_strip(text);
            if !py_isdigit(stripped) {
                return Ok(None);
            }
            pydecode::py_int(&Value::Str(stripped.to_owned()))?
        }
        _ => return Ok(None),
    };
    Ok((number > 0).then_some(number))
}

/// `_extract_resolution_label`: the video's height, else the tallest page's, as `1080p`.
pub fn extract_resolution_label(detail: &Value) -> Result<Option<String>, PyError> {
    let Some(view @ Value::Dict(_)) = py_get(detail, "View")? else {
        return Ok(None);
    };
    if let Some(dimension @ Value::Dict(_)) = py_get(view, "dimension")?
        && let Some(height) = to_positive_int(py_get(dimension, "height")?)?
    {
        return Ok(Some(format!("{height}p")));
    }
    let Some(Value::List(pages)) = py_get(view, "pages")? else {
        return Ok(None);
    };
    let mut tallest: Option<i128> = None;
    for page in pages {
        if !matches!(page, Value::Dict(_)) {
            continue;
        }
        let Some(dimension @ Value::Dict(_)) = py_get(page, "dimension")? else {
            continue;
        };
        if let Some(height) = to_positive_int(py_get(dimension, "height")?)? {
            tallest = Some(tallest.map_or(height, |tallest| tallest.max(height)));
        }
    }
    Ok(tallest.map(|height| format!("{height}p")))
}

/// `datetime.fromtimestamp(ts, tz=UTC).strftime('%Y-%m-%d')`.
fn utc_date(timestamp: i128) -> Result<String, PyError> {
    let overflow = || PyError::new("OverflowError", "timestamp out of range for platform time_t");
    if timestamp > i128::from(i64::MAX) {
        return Err(overflow());
    }
    let (year, month, day) = cookiejar::civil_from_days(timestamp.div_euclid(86_400));
    if year - 1900 > i128::from(i32::MAX) {
        return Err(overflow());
    }
    if year > 9999 {
        return Err(PyError::new("ValueError", format!("year {year} is out of range")));
    }
    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

/// `_extract_release_date`: `pubdate`, else `ctime`, as a UTC date.
pub fn extract_release_date(detail: &Value) -> Result<Option<String>, PyError> {
    let Some(view @ Value::Dict(_)) = py_get(detail, "View")? else {
        return Ok(None);
    };
    for key in ["pubdate", "ctime"] {
        if let Some(timestamp) = to_positive_int(py_get(view, key)?)? {
            return utc_date(timestamp).map(Some);
        }
    }
    Ok(None)
}

/// `_normalize_cover_url`: a protocol-relative URL gets `https:`.
pub fn normalize_cover_url(url: &str) -> String {
    if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.to_owned()
    }
}

/// `_is_placeholder_cover_url`: Bilibili's transparent stand-in images.
pub fn is_placeholder_cover_url(url: &str) -> bool {
    let path = fav_core::pyurl::urlsplit(url, "")
        .map(|parts| parts.path)
        .unwrap_or_default()
        .to_lowercase();
    path.ends_with("/transparent.png") || path.ends_with("/transparent.gif") || path.contains("/archive/transparent")
}

/// A usable cover from a candidate value, if it is one.
fn usable_cover(candidate: Option<&Value>) -> Option<String> {
    match candidate {
        Some(Value::Str(url)) if !url.is_empty() => {
            let normalized = normalize_cover_url(url);
            (!is_placeholder_cover_url(&normalized)).then_some(normalized)
        }
        _ => None,
    }
}

// ---------- session check ----------

/// `SessionCheck`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCheck {
    pub ok: bool,
    pub account: String,
    pub message: String,
    /// Bilibili itself said the session is not a login, as opposed to not answering.
    pub rejected: bool,
}

impl SessionCheck {
    fn failed(message: String, rejected: bool) -> Self {
        Self {
            ok: false,
            account: String::new(),
            message,
            rejected,
        }
    }
}

/// `{c.name.lower(): c.value for c in cookies if c.value}`: the last value of each name.
fn cookie_map(cookies: &[StoredCookie]) -> HashMap<String, String> {
    cookies
        .iter()
        .filter(|cookie| !cookie.value.is_empty())
        .map(|cookie| (cookie.name.to_lowercase(), cookie.value.clone()))
        .collect()
}

/// `missing_cookies`: the required cookies a jar lacks, in their fixed order.
pub fn missing_cookies(cookies: &[StoredCookie]) -> Vec<&'static str> {
    let present = cookie_map(cookies);
    REQUIRED_COOKIES
        .into_iter()
        .filter(|name| !present.contains_key(&name.to_lowercase()))
        .collect()
}

/// `create_credential`: the credential the API calls send.
pub fn create_credential(cookies: &[StoredCookie]) -> Result<Credential, JobError> {
    let missing = missing_cookies(cookies);
    if !missing.is_empty() {
        return Err(session_error(
            format!("Bilibili session lacks cookies: {}", missing.join(", ")),
            None,
        ));
    }
    let values = cookie_map(cookies);
    let value = |name: &str| values.get(&name.to_lowercase()).cloned().unwrap_or_default();
    Ok(Credential {
        sessdata: value("SESSDATA"),
        bili_jct: value("bili_jct"),
        buvid3: value("buvid3"),
        dedeuserid: value("DedeUserID"),
    })
}

/// `verify_session`: whether a set of bilibili.com cookies is a live login, and for whom.
///
/// `nav` answers for whoever the cookies belong to: `isLogin` with the user's name and mid,
/// or code -101 for no login at all.
pub async fn verify_session(cookies: &[StoredCookie]) -> SessionCheck {
    verify_session_at(&Endpoints::default().api, cookies).await
}

/// [`verify_session`] against another API origin (tests).
pub async fn verify_session_at(api_base: &str, cookies: &[StoredCookie]) -> SessionCheck {
    let missing = missing_cookies(cookies);
    if !missing.is_empty() {
        return SessionCheck::failed(format!("cookies not set yet: {}", missing.join(", ")), false);
    }
    let cookie_header = cookies
        .iter()
        .filter(|cookie| !cookie.value.is_empty())
        .map(|cookie| format!("{}={}", cookie.name, cookie.value))
        .collect::<Vec<_>>()
        .join("; ");
    let unreachable = |class: &str| SessionCheck::failed(format!("Bilibili unreachable: {class}"), false);
    let client = match fav_net::client_builder()
        .user_agent(USER_AGENT)
        .connect_timeout(VERIFY_TIMEOUT)
        .read_timeout(VERIFY_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(error) => return unreachable(HttpError::transport(&error).class_name()),
    };
    let request = client
        .get(format!("{}{NAV_PATH}", api_base.trim_end_matches('/')))
        .header(header::REFERER, "https://www.bilibili.com/")
        .header(header::COOKIE, cookie_header);
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return unreachable(HttpError::transport(&error).class_name()),
    };
    let status = response.status().as_u16();
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => return unreachable(HttpError::transport(&error).class_name()),
    };
    let payload = match pydecode::json_from_bytes(&body) {
        Ok(payload) => payload,
        Err(error) => return unreachable(&error.class),
    };
    let empty = Value::Dict(Dict::new());
    let (code, data) = match &payload {
        Value::Dict(dict) => (
            dict.get("code").cloned().unwrap_or(Value::Null),
            dict.get("data").filter(|data| matches!(data, Value::Dict(_))).unwrap_or(&empty),
        ),
        _ => (Value::Null, &empty),
    };
    let logged_in = data.get("isLogin").is_some_and(Value::is_truthy);
    if py_eq_int(&code, NOT_LOGGED_IN_CODE) || (py_eq_int(&code, 0) && !logged_in) {
        return SessionCheck::failed("Bilibili says these cookies are not signed in".to_owned(), true);
    }
    if status != HTTP_OK || !py_eq_int(&code, 0) {
        return SessionCheck::failed(format!("Bilibili nav answered HTTP {status}, code {}", str_value(&code)), false);
    }
    let text = |key: &str| data.get(key).filter(|value| value.is_truthy()).map(str_value).unwrap_or_default();
    let (name, mid) = (text("uname"), text("mid"));
    let account = if !name.is_empty() && !mid.is_empty() {
        format!("{name} ({mid})")
    } else if name.is_empty() {
        mid
    } else {
        name
    };
    SessionCheck {
        ok: true,
        account,
        message: String::new(),
        rejected: false,
    }
}

/// `dict(extra)[key] = value`: replace in place, or add at the end.
fn set_extra(extra: &mut Vec<(String, String)>, key: &str, value: &str) {
    match extra.iter_mut().find(|(name, _)| name == key) {
        Some(slot) => value.clone_into(&mut slot.1),
        None => extra.push((key.to_owned(), value.to_owned())),
    }
}

/// `renew_session`: the stored session with a fresh `bili_ticket` and, when Bilibili asks,
/// a refreshed `SESSDATA`. Each renewal is written back as soon as it happens; one that
/// Bilibili refuses (or that does not get through) is logged and leaves the session as it
/// was: the check that follows decides whether the run can go on.
pub async fn renew_session(db: &Db, site: &str, stored: SiteSession, endpoints: &Endpoints, now: f64) -> Result<SiteSession, JobError> {
    let mut cookies = stored.cookies.clone();
    let mut extra = stored.extra.clone();
    match session::renew_ticket(&cookies, endpoints, now).await {
        Err(error) if error.is_expected() => log().warning(format_args!("Could not renew the bili_ticket of {site}: {error}")),
        Err(error) => return Err(error.into_job_error()),
        Ok(Some(renewed)) => {
            site_sessions::replace_cookies(db, site, &renewed, cookie_expiry(&renewed, SESSION_COOKIE), &extra)
                .await
                .map_err(session_store_error)?;
            cookies = renewed;
            log().info(format_args!("Renewed the bili_ticket of {site}"));
        }
        Ok(None) => {}
    }

    let token = extra
        .iter()
        .find(|(key, _)| key == session::REFRESH_TOKEN_KEY)
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    if token.is_empty() {
        log().info(format_args!(
            "No refresh token stored for {site}; signing in again on the 登录态 page lets fav refresh SESSDATA itself"
        ));
        return Ok(SiteSession { cookies, ..stored });
    }

    let saved_extra = std::sync::Mutex::new(extra.clone());
    let save = async |renewal: &Renewal| {
        let extra = {
            let mut extra = saved_extra.lock().expect("extra");
            set_extra(&mut extra, session::REFRESH_TOKEN_KEY, &renewal.refresh_token);
            extra.clone()
        };
        site_sessions::replace_cookies(db, site, &renewal.cookies, cookie_expiry(&renewal.cookies, SESSION_COOKIE), &extra)
            .await
            .map_err(session_store_error)
    };
    match session::refresh_session(&cookies, &token, endpoints, now, save).await {
        Err(error) if error.is_expected() => log().warning(format_args!("Could not refresh the SESSDATA of {site}: {error}")),
        Err(error) => return Err(error.into_job_error()),
        Ok(Some(refreshed)) => {
            cookies = refreshed.cookies;
            log().info(format_args!("Refreshed the SESSDATA of {site}"));
        }
        Ok(None) => {}
    }
    extra = saved_extra.into_inner().expect("extra");
    Ok(SiteSession { cookies, extra, ..stored })
}

impl From<RenewError> for JobError {
    fn from(error: RenewError) -> Self {
        error.into_job_error()
    }
}

// ---------- the crawler ----------

/// How long the crawl waits: between batches of validity checks, and before download
/// retries (the base of the 5 s, 10 s ladder).
#[derive(Debug, Clone)]
pub struct Pacing {
    pub batch_pause: Duration,
    pub retry_base_seconds: f64,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            batch_pause: Duration::from_secs(1),
            retry_base_seconds: DOWNLOAD_BASE_DELAY_SECONDS,
        }
    }
}

/// The parts of `web.bilibili` a run uses.
#[derive(Debug, Clone, Default)]
pub struct BilibiliConfig {
    pub proxy: String,
    pub accounts: Vec<BilibiliAccount>,
}

impl BilibiliConfig {
    pub fn from_settings(section: &fav_core::settings::Bilibili) -> Self {
        Self {
            proxy: section.proxy.clone(),
            accounts: section.accounts.clone(),
        }
    }
}

/// One step of a run: activate an account, or walk one of its lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Activate(String),
    List { fav_id: i64, path: String },
}

/// The order `update()` walks the accounts in: each account's favourite lists, then its
/// watch-later list when enabled, each after the account's own activation.
pub fn plan(accounts: &[BilibiliAccount]) -> Vec<Step> {
    let mut steps = Vec::new();
    for account in accounts {
        steps.push(Step::Activate(account.name.clone()));
        for favorite in &account.favorites {
            steps.push(Step::List {
                fav_id: favorite.fav_id,
                path: favorite.path.as_str().to_owned(),
            });
        }
        if account.toview_enabled {
            steps.push(Step::List {
                fav_id: TOVIEW_FAV_ID,
                path: account.toview_path.as_str().to_owned(),
            });
        }
    }
    steps
}

/// What a download notification says about one video.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Downloaded {
    pub bvid: String,
    pub title: String,
    pub upper: String,
    pub fav_id: i64,
    pub resolution: Option<String>,
    pub file_size_bytes: Option<i128>,
    pub release_date: Option<String>,
    pub cover_url: Option<String>,
}

/// `Bilibili`: one run's clients, settings, database and seams.
pub struct Bilibili {
    db: Db,
    notifier: SharedNotifier,
    runner: Arc<dyn Runner>,
    config: BilibiliConfig,
    endpoints: Endpoints,
    pacing: Pacing,
    clock: fn() -> f64,
    _tmp_dir: tempfile::TempDir,
    cache_dir: PathBuf,
    cookie_path: PathBuf,
    api: Option<Api>,
    /// `info_cache`, by video object.
    info_cache: HashMap<u64, Value>,
}

impl std::fmt::Debug for Bilibili {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bilibili")
            .field("cache_dir", &self.cache_dir)
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

impl Bilibili {
    pub fn new(db: Db, notifier: SharedNotifier, runner: Arc<dyn Runner>, config: BilibiliConfig) -> Result<Self, JobError> {
        let tmp_dir = tempfile::Builder::new()
            .prefix("fav-bilibili-")
            .tempdir()
            .map_err(|error| JobError::new(kit::io_class_name(&error), kit::py_os_error_text(&error)))?;
        let cache_dir = tmp_dir.path().to_path_buf();
        log().debug(format_args!("cache_dir: {}", cache_dir.display()));
        Ok(Self {
            db,
            notifier,
            runner,
            config,
            endpoints: Endpoints::default(),
            pacing: Pacing::default(),
            clock: now,
            cookie_path: cache_dir.join("bilibili.txt"),
            cache_dir,
            _tmp_dir: tmp_dir,
            api: None,
            info_cache: HashMap::new(),
        })
    }

    /// Talk to other origins, wait differently and read another clock (tests).
    #[must_use]
    pub fn with_test_seams(mut self, endpoints: Endpoints, pacing: Pacing, clock: fn() -> f64) -> Self {
        self.endpoints = endpoints;
        self.pacing = pacing;
        self.clock = clock;
        self
    }

    /// The temporary directory of this run (cookie files, downloads in progress).
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// The cookie file of the active account.
    pub fn cookie_path(&self) -> &Path {
        &self.cookie_path
    }

    /// The active account's credential, once activated.
    pub fn credential(&self) -> Option<&Credential> {
        self.api.as_ref().map(Api::credential)
    }

    fn api(&self) -> Result<&Api, JobError> {
        self.api
            .as_ref()
            .ok_or_else(|| JobError::new("RuntimeError", "no Bilibili account is active"))
    }

    // ----- notifications -----

    /// `_notify_download`: the download notification, then the resolve of an earlier
    /// failure; whether the resolve went through.
    pub async fn notify_download(&self, video: &Downloaded) -> bool {
        let source = if video.fav_id == TOVIEW_FAV_ID { "toview" } else { "fav" };
        let labels = Labels::of(video);
        let notification = NewNotification {
            kind: "download_completed".to_owned(),
            source: JOB_KEY.to_owned(),
            header: format!("Bilibili ({source})"),
            title: video.title.clone(),
            body: format!(
                "{} | {} | {} | {}",
                video.upper, labels.resolution, labels.file_size, labels.release_date
            ),
            link_url: video_url(&video.bvid),
            image_url: video.cover_url.clone().unwrap_or_default(),
            payload: Some(payload(video, false)),
            ..NewNotification::default()
        };
        if let Err(error) = self.notifier.enqueue(notification).await {
            log().warning(format_args!(
                "Failed to enqueue bilibili download notification for {}: {error}",
                video.bvid
            ));
        }
        self.resolve_download_failure_notification(video).await
    }

    /// `_resolve_download_failure_notification`.
    pub async fn resolve_download_failure_notification(&self, video: &Downloaded) -> bool {
        let source = if video.fav_id == TOVIEW_FAV_ID { "toview" } else { "fav" };
        let labels = Labels::of(video);
        let metadata = format!(
            "{} | {source} | {} | {} | {}",
            video.upper, labels.resolution, labels.file_size, labels.release_date
        );
        let resolution = Resolution {
            link_url: video_url(&video.bvid),
            image_url: video.cover_url.clone().unwrap_or_default(),
            ..Resolution::job_recovered(
                "Bilibili",
                &format!("Download succeeded: {} [{}]\n{metadata}", video.title, video.bvid),
                Some(payload(video, true)),
            )
        };
        match self
            .notifier
            .resolve(download_failure_notification_key(&video.bvid), resolution)
            .await
        {
            Ok(()) => true,
            Err(error) => {
                log().warning(format_args!(
                    "Failed to resolve bilibili download failure notification for {}: {error}",
                    video.bvid
                ));
                false
            }
        }
    }

    /// `_resolve_existing_download_failure_notification`: a video already in the table
    /// may still have a failure notification from an earlier run.
    async fn resolve_existing_download_failure_notification(&mut self, video: &Video, fav_id: i64) -> Result<bool, JobError> {
        let bvid = video.bvid().to_owned();
        let mut downloaded = Downloaded {
            bvid: bvid.clone(),
            title: bvid.clone(),
            upper: "unknown".to_owned(),
            fav_id,
            ..Downloaded::default()
        };
        match self.api()?.video_detail(video).await {
            Err(error) => log().debug(format_args!(
                "Failed to read detail for existing bilibili video {bvid} before recovery notification: {error}"
            )),
            Ok(detail) => {
                let view = py_get_or_empty(&detail, "View").map_err(py_error)?;
                downloaded.title = py_get(&view, "title")
                    .map_err(py_error)?
                    .filter(|title| title.is_truthy())
                    .map_or_else(|| bvid.clone(), str_value);
                let card = py_get_or_empty(&detail, "Card").map_err(py_error)?;
                let inner = py_get_or_empty(&card, "card").map_err(py_error)?;
                downloaded.upper = py_get(&inner, "name")
                    .map_err(py_error)?
                    .filter(|name| name.is_truthy())
                    .map_or_else(|| "unknown".to_owned(), str_value);
                downloaded.resolution = extract_resolution_label(&detail).map_err(py_error)?;
                downloaded.release_date = extract_release_date(&detail).map_err(py_error)?;
                downloaded.cover_url = self.get_video_cover_url(video, Some(&detail)).await?;
            }
        }
        Ok(self.resolve_download_failure_notification(&downloaded).await)
    }

    // ----- videos -----

    /// `get_video_cover_url`: the detail's cover, else the info's, unless a placeholder.
    pub async fn get_video_cover_url(&mut self, video: &Video, detail: Option<&Value>) -> Result<Option<String>, JobError> {
        if let Some(detail) = detail.filter(|detail| !matches!(detail, Value::Null)) {
            let mut candidate = match py_get(detail, "View").map_err(py_error)? {
                Some(view @ Value::Dict(_)) => py_get(view, "pic").map_err(py_error)?.cloned(),
                _ => None,
            };
            if !candidate.as_ref().is_some_and(Value::is_truthy) {
                candidate = py_get(detail, "pic").map_err(py_error)?.cloned();
            }
            if let Some(cover) = usable_cover(candidate.as_ref()) {
                return Ok(Some(cover));
            }
        }
        let info = match self.info_cache.get(&video.key()).filter(|info| !matches!(info, Value::Null)) {
            Some(info) => info.clone(),
            None => match self.api()?.video_info(video).await {
                Ok(info) => {
                    self.info_cache.insert(video.key(), info.clone());
                    info
                }
                Err(error) => {
                    log().debug(format_args!("Failed to fetch cover for {}: {error}", video.bvid()));
                    return Ok(None);
                }
            },
        };
        Ok(usable_cover(py_get(&info, "pic").map_err(py_error)?))
    }

    /// `check_valid` for a batch: the infos are fetched together, then read in order.
    async fn check_valid_batch(&mut self, videos: &[Video]) -> Result<Vec<bool>, JobError> {
        let api = self.api()?.clone();
        let fetches = videos.iter().map(|video| {
            let api = api.clone();
            let cached = self.info_cache.contains_key(&video.key());
            async move {
                if cached {
                    return None;
                }
                Some(api.video_info(video).await)
            }
        });
        let outcomes = futures_util::future::join_all(fetches).await;
        let mut valid = Vec::with_capacity(videos.len());
        for (video, outcome) in videos.iter().zip(outcomes) {
            let Some(outcome) = outcome else {
                valid.push(true);
                continue;
            };
            let info = match outcome {
                Ok(info) => info,
                Err(error) => {
                    log().warning(format_args!("Video {} is invalid: {error}", video.bvid()));
                    valid.push(false);
                    continue;
                }
            };
            self.info_cache.insert(video.key(), info.clone());
            if py_item(&info, "is_upower_exclusive").map_err(py_error)?.is_truthy() {
                log().warning(format_args!("Video {} is a paid video", video.bvid()));
                valid.push(false);
            } else {
                valid.push(true);
            }
        }
        Ok(valid)
    }

    /// `limit_gather(*[check_valid(v) for v in videos])`: batches of five, a pause after each.
    async fn check_valid_all(&mut self, videos: &[Video]) -> Result<Vec<bool>, JobError> {
        let mut valid = Vec::with_capacity(videos.len());
        for batch in videos.chunks(GATHER_LIMIT) {
            valid.extend(self.check_valid_batch(batch).await?);
            tokio::time::sleep(self.pacing.batch_pause).await;
        }
        Ok(valid)
    }

    async fn existing_bvids(&self) -> Result<HashSet<String>, JobError> {
        let rows = sqlx::query("SELECT bvid FROM bilibili;")
            .fetch_all(self.db.pool())
            .await
            .map_err(sql_error)?;
        rows.iter().map(|row| row.try_get::<String, _>("bvid").map_err(sql_error)).collect()
    }

    /// Drop the videos already in the table, resolving their old failure notifications.
    async fn skip_existing(&mut self, videos: Vec<Video>, fav_id: i64, existing: &HashSet<String>) -> Result<(Vec<Video>, bool), JobError> {
        let mut recovered_all = true;
        let mut remaining = Vec::with_capacity(videos.len());
        for video in videos {
            if existing.contains(video.bvid()) {
                let recovered = self.resolve_existing_download_failure_notification(&video, fav_id).await?;
                recovered_all = recovered && recovered_all;
            } else {
                remaining.push(video);
            }
        }
        Ok((remaining, recovered_all))
    }

    /// `get_toviews`: the watch-later videos to download, whether the list had any, and
    /// whether every recovery notification went through.
    pub async fn get_toviews(&mut self) -> Result<(Vec<Video>, bool, bool), JobError> {
        let toview = self.api()?.toview_list().await.map_err(ApiError::into_job_error)?;
        let list = py_item(&toview, "list").map_err(py_error)?;
        if !list.is_truthy() {
            return Ok((Vec::new(), false, true));
        }
        let existing = self.existing_bvids().await?;
        let videos = py_iter(list)
            .map_err(py_error)?
            .iter()
            .map(video_of)
            .collect::<Result<Vec<_>, _>>()?;
        log().info(format_args!("Find {} toviews in total", videos.len()));
        let (videos, recovered) = self.skip_existing(videos, TOVIEW_FAV_ID, &existing).await?;
        log().info(format_args!("Find {} toviews to download", videos.len()));
        Ok((videos, true, recovered))
    }

    /// `get_favs`: the favourite list's videos to download, page by page.
    pub async fn get_favs(&mut self, fav_id: i64) -> Result<(Vec<Video>, bool), JobError> {
        let existing = self.existing_bvids().await?;
        let mut page = 1;
        let mut has_more = true;
        let mut videos = Vec::new();
        while has_more {
            let answer = self.api()?.favorite_page(fav_id, page).await.map_err(ApiError::into_job_error)?;
            has_more = py_item(&answer, "has_more").map_err(py_error)?.is_truthy();
            page += 1;
            let medias = py_item(&answer, "medias").map_err(py_error)?;
            for media in py_iter(medias).map_err(py_error)? {
                videos.push(video_of(&media)?);
            }
        }
        log().info(format_args!("Find {} favs in total", videos.len()));
        let (videos, recovered) = self.skip_existing(videos, fav_id, &existing).await?;
        log().info(format_args!("Find {} favs to download", videos.len()));
        Ok((videos, recovered))
    }

    // ----- downloads -----

    /// The yt-dlp command for one video.
    pub fn download_command(&self, url: &str, bvid: &str, dirpath: &Path) -> Vec<String> {
        let mut command: Vec<String> = [
            "yt-dlp",
            "-o",
            &dirpath.join(format!("{bvid}.%(ext)s")).to_string_lossy(),
            "--no-mtime",
            "--cookies",
            &self.cookie_path.to_string_lossy(),
            "-N",
            "8",
            "--retries",
            "15",
            "--fragment-retries",
            "15",
            "--socket-timeout",
            "30",
            // Since 2026-10-28 the upos CDN answers a GET without a Range header with HTTP 567
            // (an HTML block page) while ranged requests still get 206. yt-dlp only sends Range
            // when the download is chunked, so chunk every stream.
            "--http-chunk-size",
            "10M",
        ]
        .iter()
        .map(|part| (*part).to_owned())
        .collect();
        if !self.config.proxy.is_empty() {
            // yt-dlp resolves the playurl itself, so the proxy steers both the CDN mirror
            // assignment and the media transfer.
            command.extend(["--proxy".to_owned(), self.config.proxy.clone()]);
        }
        command.push(url.to_owned());
        command
    }

    /// `download`: yt-dlp, three attempts with 5 s and 10 s between them. A failure is a
    /// `DownloadError` under the video's own notification key.
    pub async fn download(&self, url: &str, bvid: &str, dirpath: &Path) -> Result<(), JobError> {
        let command = self.download_command(url, bvid, dirpath);
        let mut attempt = 1;
        loop {
            cleanup_dir(dirpath)?;
            let result = self.runner.run(command.clone()).await.map_err(|error| {
                JobError::new(
                    kit::io_class_name(&error),
                    format!("{}: {}", kit::py_os_error_text(&error), repr_str(&command[0])),
                )
            })?;
            if result.returncode == 0 {
                if !result.stderr.is_empty() {
                    log().debug(format_args!("yt-dlp stderr: {}", py_strip(&result.stderr)));
                }
                return Ok(());
            }
            let stderr = py_strip(&result.stderr);
            let stdout = py_strip(&result.stdout);
            let message = if !stderr.is_empty() {
                stderr.to_owned()
            } else if !stdout.is_empty() {
                stdout.to_owned()
            } else {
                format!("yt-dlp exited with code {}", result.returncode)
            };
            let message = format!("{url}: {message}");
            if attempt >= DOWNLOAD_ATTEMPTS {
                return Err(JobError::new("DownloadError", message).with_notification_dedupe_key(download_failure_key(bvid)));
            }
            // tenacity's `wait_exponential(multiplier=b, min=b, max=6b)`.
            let base = self.pacing.retry_base_seconds;
            let wait = (base * 2_f64.powi(i32::try_from(attempt - 1).unwrap_or(0))).clamp(base, base * 6.0);
            // Debug only: a transient failure is not worth an alert.
            log().debug(format_args!(
                "Retrying download for {bvid} ({attempt}/{DOWNLOAD_ATTEMPTS}), next wait {wait:.1}s: {message}"
            ));
            tokio::time::sleep(Duration::try_from_secs_f64(wait).unwrap_or(Duration::ZERO)).await;
            attempt += 1;
        }
    }

    // ----- one list -----

    /// `update_fav`: download the new videos of one list into `path`; for the watch-later
    /// list, clear it once the pass went through and every recovery was recorded.
    pub async fn update_fav(&mut self, fav_id: i64, path: &Path) -> Result<(), JobError> {
        std::fs::create_dir_all(path).map_err(|error| os_error(&error, path))?;
        let is_toview = fav_id == TOVIEW_FAV_ID;
        let mut has_any_toviews = false;
        let (videos, mut recovered_all) = if is_toview {
            let (videos, has_any, recovered) = self.get_toviews().await?;
            has_any_toviews = has_any;
            (videos, recovered)
        } else {
            self.get_favs(fav_id).await?
        };
        if videos.is_empty() {
            log().info("No new videos");
        } else {
            let valid = self.check_valid_all(&videos).await?;
            let videos: Vec<Video> = videos
                .into_iter()
                .zip(valid)
                .filter_map(|(video, valid)| valid.then_some(video))
                .collect();
            for video in videos.iter().rev() {
                let recovered = self.download_video(video, fav_id, path).await?;
                recovered_all = recovered && recovered_all;
            }
        }

        // Clear the watch-later list only after the download pass completes successfully.
        if is_toview && has_any_toviews && recovered_all {
            log().info("Clearing toview list ...");
            self.api()?.clear_toview_list().await.map_err(ApiError::into_job_error)?;
        } else if is_toview && has_any_toviews {
            log().warning("Skip clearing toview list because a download recovery notification did not enqueue successfully");
        }
        Ok(())
    }

    /// One video of `update_fav`'s loop: detail, download, move, row, notification.
    async fn download_video(&mut self, video: &Video, fav_id: i64, path: &Path) -> Result<bool, JobError> {
        let bvid = video.bvid().to_owned();
        let detail = self.api()?.video_detail(video).await.map_err(ApiError::into_job_error)?;
        let title = str_value(py_item(py_item(&detail, "View").map_err(py_error)?, "title").map_err(py_error)?);
        let card = py_item(py_item(&detail, "Card").map_err(py_error)?, "card").map_err(py_error)?;
        let upper = str_value(py_item(card, "name").map_err(py_error)?);
        let cover_url = self.get_video_cover_url(video, Some(&detail)).await?;
        let url = video_url(&bvid);
        let video_cache_dir = self.cache_dir.join("videos");
        match std::fs::create_dir(&video_cache_dir) {
            Err(error) if !(error.kind() == std::io::ErrorKind::AlreadyExists && video_cache_dir.is_dir()) => {
                return Err(os_error(&error, &video_cache_dir));
            }
            _ => {}
        }
        log().info(format_args!("Downloading [{upper}]{title} [{bvid}]"));
        self.download(&url, &bvid, &video_cache_dir).await?;
        let saved_paths = move_downloads(&video_cache_dir, path, &title, &bvid, &upper)?;
        let resolution = extract_resolution_label(&detail).map_err(py_error)?;
        let mut file_size_bytes: Option<i128> = None;
        for saved_path in &saved_paths {
            match std::fs::metadata(saved_path) {
                Ok(metadata) => {
                    let size = i128::from(metadata.len());
                    if file_size_bytes.is_none_or(|largest| size > largest) {
                        file_size_bytes = Some(size);
                    }
                }
                Err(error) => log().debug(format_args!(
                    "Failed to read file size for {}: {}",
                    saved_path.display(),
                    kit::py_os_error_text(&error)
                )),
            }
        }
        let release_date = extract_release_date(&detail).map_err(py_error)?;
        sqlx::query(
            "INSERT INTO bilibili (bvid, fav_id, title, upper)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (bvid) DO UPDATE SET
                 fav_id = EXCLUDED.fav_id,
                 title = EXCLUDED.title,
                 upper = EXCLUDED.upper;",
        )
        .bind(&bvid)
        .bind(fav_id)
        .bind(&title)
        .bind(&upper)
        .execute(self.db.pool())
        .await
        .map_err(sql_error)?;
        let downloaded = Downloaded {
            bvid,
            title,
            upper,
            fav_id,
            resolution,
            file_size_bytes,
            release_date,
            cover_url,
        };
        Ok(self.notify_download(&downloaded).await)
    }

    // ----- accounts -----

    /// `_activate_account`: renew and check the account's session, then point the cookie
    /// file and the credential at it. The info cache is dropped because what a video shows
    /// depends on who is logged in.
    pub async fn activate_account(&mut self, account: &BilibiliAccount) -> Result<(), JobError> {
        let name = &account.name;
        let site = session_site(name);
        let stored = site_sessions::load(&self.db, &site).await.map_err(session_store_error)?;
        let Some(stored) = stored.filter(|stored| !stored.cookie(SESSION_COOKIE).is_empty()) else {
            return Err(session_error(
                format!("Bilibili account {name} is not signed in. {}", relogin_hint(name)),
                Some(name),
            ));
        };
        let stored = renew_session(&self.db, &site, stored, &self.endpoints, (self.clock)()).await?;
        let check = verify_session_at(&self.endpoints.api, &stored.cookies).await;
        if !check.ok {
            if !check.rejected && missing_cookies(&stored.cookies).is_empty() {
                // Bilibili not answering is a network failure, not a signed-out session.
                return Err(JobError::new(
                    "RuntimeError",
                    format!("Could not check the Bilibili session of {name}: {}", check.message),
                ));
            }
            let message = format!(
                "Bilibili session of {name} is no longer valid ({}). {}",
                check.message,
                relogin_hint(name)
            );
            site_sessions::record_check(&self.db, &site, false, &message)
                .await
                .map_err(session_store_error)?;
            return Err(session_error(message, Some(name)));
        }
        site_sessions::record_check(&self.db, &site, true, "")
            .await
            .map_err(session_store_error)?;
        self.cookie_path = self.cache_dir.join(format!("bilibili-{name}.txt"));
        std::fs::write(&self.cookie_path, site_sessions::to_netscape(&stored.cookies))
            .map_err(|error| os_error(&error, &self.cookie_path))?;
        let credential = create_credential(&stored.cookies)?;
        self.api = Some(Api::new(credential, &self.endpoints.api).map_err(HttpError::into_job_error)?);
        self.info_cache.clear();
        Ok(())
    }

    /// `update()`: every configured account, its favourite lists, then its watch-later list.
    pub async fn update(&mut self) -> Result<(), JobError> {
        if self.config.accounts.is_empty() {
            log().warning("No bilibili accounts configured; nothing to do");
            return Ok(());
        }
        let accounts = self.config.accounts.clone();
        for account in &accounts {
            log().info(format_args!("Processing bilibili account {}", account.name));
            for step in plan(std::slice::from_ref(account)) {
                match step {
                    Step::Activate(_) => self.activate_account(account).await?,
                    Step::List { fav_id, path } => {
                        if fav_id == TOVIEW_FAV_ID {
                            log().info(format_args!("Account {}: watch-later -> {path}", account.name));
                        } else {
                            log().info(format_args!("Account {}: favourite list {fav_id} -> {path}", account.name));
                        }
                        self.update_fav(fav_id, Path::new(&path)).await?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// The labels a notification shows for missing values.
struct Labels {
    resolution: String,
    file_size: String,
    release_date: String,
}

impl Labels {
    fn of(video: &Downloaded) -> Self {
        let resolution = video
            .resolution
            .clone()
            .filter(|resolution| !resolution.is_empty())
            .unwrap_or_else(|| "unknown".to_owned());
        let file_size = format_file_size(video.file_size_bytes).unwrap_or_else(|| "unknown".to_owned());
        let release_date = py_strip(video.release_date.as_deref().filter(|date| !date.is_empty()).unwrap_or("unknown")).to_owned();
        Self {
            resolution,
            file_size,
            release_date: if release_date.is_empty() {
                "unknown".to_owned()
            } else {
                release_date
            },
        }
    }
}

fn video_url(bvid: &str) -> String {
    format!("https://www.bilibili.com/video/{bvid}")
}

/// The notification payload; the resolve's starts with the job key.
fn payload(video: &Downloaded, with_job: bool) -> Dict {
    let mut payload = Dict::new();
    if with_job {
        payload.insert("job", Value::from(JOB_KEY));
    }
    payload.insert("bvid", Value::from(video.bvid.as_str()));
    payload.insert("fav_id", Value::from(video.fav_id));
    payload.insert("upper", Value::from(video.upper.as_str()));
    payload.insert("resolution", Value::from(video.resolution.clone()));
    payload.insert("file_size_bytes", video.file_size_bytes.map_or(Value::Null, Value::Int));
    payload.insert("release_date", Value::from(video.release_date.clone()));
    payload
}

/// `_cleanup_dir`: empty the download directory, if it exists.
fn cleanup_dir(dirpath: &Path) -> Result<(), JobError> {
    if !dirpath.exists() {
        return Ok(());
    }
    let entries = std::fs::read_dir(dirpath).map_err(|error| os_error(&error, dirpath))?;
    for entry in entries {
        let entry = entry.map_err(|error| os_error(&error, dirpath))?;
        let path = entry.path();
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.map_err(|error| os_error(&error, &path))?;
    }
    Ok(())
}

/// `_move_downloads`: each downloaded file, in name order, renamed to
/// `[upper]title [bvid].ext` (made unique) in the list's directory.
pub fn move_downloads(source_dir: &Path, dest_dir: &Path, title: &str, bvid: &str, upper: &str) -> Result<Vec<PathBuf>, JobError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(source_dir)
        .map_err(|error| os_error(&error, source_dir))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()
        .map_err(|error| os_error(&error, source_dir))?;
    entries.sort();
    let mut saved = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let filename = format_video_filename(title, bvid, Some(upper), fav_core::pypath::suffix(&name), MAX_TOTAL_BYTES);
        let destination = ensure_unique_path(&dest_dir.join(filename));
        kit::move_file(&entry, &destination).map_err(|error| os_error(&error, &entry))?;
        saved.push(destination);
    }
    Ok(saved)
}

// ---------- the job ----------

/// The `bilibili` job: settings and clients are read when the run starts.
struct BilibiliJob {
    ctx: SourceContext,
}

impl Source for BilibiliJob {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self
                .ctx
                .settings
                .load()
                .await
                .map_err(|error| JobError::new("RuntimeError", error.to_string()))?;
            let notifier: SharedNotifier = Arc::new(OutboxNotifier(Outbox::new(self.ctx.db.clone(), Arc::clone(&self.ctx.settings))));
            let config = BilibiliConfig::from_settings(&settings.web.bilibili);
            let mut source = Bilibili::new(self.ctx.db.clone(), notifier, Arc::new(SystemRunner), config)?;
            source.update().await
        })
    }
}

/// Register the `bilibili` job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run(JOB_KEY, move || Box::new(BilibiliJob { ctx: ctx.clone() }) as Box<dyn Source>);
}

#[cfg(test)]
mod tests;
