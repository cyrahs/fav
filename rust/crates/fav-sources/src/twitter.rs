//! Liked tweets on X, downloaded through gallery-dl.
//!
//! Python origin: `src/web/twitter.py`. X sells no affordable read access to a likes
//! timeline, so the crawl drives the gallery-dl CLI against `x.com/<username>/likes` with
//! the browser session the account is signed in with. gallery-dl owns the parts that break
//! when X changes (the GraphQL endpoints, the transaction-id header, the 429 back-off); this
//! module owns scheduling, the session and the archive table.
//!
//! The session is the one captured on the 登录态 page (`site_sessions`, site `twitter`):
//! its `auth_token` and `ct0` cookies are handed to gallery-dl as a cookies.txt each run.
//!
//! Two pieces of state make a run incremental: gallery-dl's own download archive in the
//! collection directory, and `twitter_state`, which records whether the first full walk
//! of the timeline ever finished. Until it has, runs walk to the end; after that they stop
//! once they have seen `abort_after` known files in a row.
//!
//! gallery-dl writes one JSON sidecar next to every file. The sidecars are the ingest
//! queue: one is removed only once its row is in the table, so a run that dies mid-ingest
//! is picked up by the next one.
//!
//! Never run against X from tests or development (`rust/docs/PLAN.md`): gallery-dl is
//! replaced by a scripted runner and the account check talks to a local fake.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fav_core::logging::{self, Logger};
use fav_core::pyjson::{self, Dict, DumpOptions, Value};
use fav_core::pytext::py_isdigit;
use fav_core::settings::Twitter as TwitterConfig;
use fav_core::validate::PyPath;
use fav_db::Db;
use fav_net::httpx::HttpError;
use fav_net::reqwest::{self, header};
use fav_net::retry::{Sleeper, tokio_sleeper};
use fav_net::subprocess::{Runner, Stream, SystemRunner};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox};
use fav_runtime::pystr::{repr_str, str_value};
use fav_runtime::site_sessions::{self, StoredCookie, cookie_value};
use fav_runtime::source::{JobError, Registry, Source};
use sqlx::Row as _;

use crate::SourceContext;
use crate::notifier::{OutboxNotifier, SharedNotifier};
use crate::pydecode::{self, PyError};

pub const JOB_KEY: &str = "twitter";
pub const SIDECAR_SUFFIX: &str = ".json";
pub const ARCHIVE_FILENAME: &str = ".gallery-dl-archive.db";
pub const COOKIE_FILENAME: &str = "twitter-cookies.txt";
pub const BACKFILL_STATE_KEY: &str = "backfill_complete";

/// The key the captured login is stored under in `site_sessions`.
pub const SESSION_SITE: &str = "twitter";
/// What the 登录态 page opens and reads back.
pub const LOGIN_URL: &str = "https://x.com/i/flow/login";
pub const SESSION_COOKIE_URLS: [&str; 1] = ["https://x.com/"];
/// Set only once a login completes; `ct0` is the CSRF token every request echoes back.
pub const SESSION_COOKIE: &str = "auth_token";
const CSRF_COOKIE: &str = "ct0";
/// `u=<user id>`, URL-encoded, set alongside `auth_token`.
const USER_ID_COOKIE: &str = "twid";
const RELOGIN_HINT: &str = "Sign in to X again on fav's 登录态 page.";
const AUTH_DEDUPE_KEY: &str = "twitter:auth";
/// The web app's public bearer token, the same one gallery-dl sends (shipped in X's own
/// web bundle, not a secret).
const WEB_BEARER_TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";
pub const ACCOUNT_SETTINGS_URL: &str = "https://api.x.com/1.1/account/settings.json";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const HTTP_UNAUTHORIZED: u16 = 401;
const HTTP_FORBIDDEN: u16 = 403;
const HTTP_OK: u16 = 200;
const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

/// The repository's `[uploader]title [media_id].ext` shape. The tweet text is left out: it
/// is multi-line, mutable, and already in the table for the archive UI to search.
pub const FILENAME_FORMAT: &str = "[{author[name]}] {date:%Y-%m-%d} [{tweet_id}_{num}].{extension}";
pub const DIRECTORY_FORMAT: [&str; 1] = ["{author[name]}"];

/// gallery-dl's own media types. X stores a GIF as a short video, so it belongs here.
pub const VIDEO_TYPES: [&str; 2] = ["video", "animated_gif"];

/// gallery-dl ORs an exit code per failure class: 16 is authentication, 4 an extraction
/// error such as "not found".
pub const EXIT_AUTH: i32 = 16;
pub const EXIT_EXTRACTION: i32 = 4;
/// How much of gallery-dl's stderr a failure notification carries.
const ERROR_TAIL_LINES: usize = 5;

/// X's GraphQL backend sometimes answers HTTP 200 with an empty `data` object. gallery-dl
/// retries that while paginating, but not on the username lookup that comes first, where it
/// surfaces as this line and a clean exit code 4 (mikf/gallery-dl#8317, #8627). A run that
/// dies there has fetched nothing yet, so it is run again after a pause.
pub const USER_LOOKUP_ERROR: &str = "[twitter][error] NotFoundError: Requested user could not be found";
pub const USER_LOOKUP_RETRIES: u32 = 1;
pub const USER_LOOKUP_RETRY_DELAY: Duration = Duration::from_secs(30);

fn log() -> Logger {
    logging::get("twitter")
}

/// `TwitterDownloadError(message, notification_dedupe_key=key)`.
fn download_error(message: String, dedupe_key: &str) -> JobError {
    JobError::new("TwitterDownloadError", message).with_notification_dedupe_key(dedupe_key)
}

fn sql_error(error: sqlx::Error) -> JobError {
    crate::db_job_error(&fav_db::DbError::from(error))
}

fn session_store_error(error: fav_runtime::Error) -> JobError {
    match error {
        fav_runtime::Error::Db(error) => crate::db_job_error(&error),
        other => JobError::new("RuntimeError", other.to_string()),
    }
}

/// `str(OSError)` with the file name, as Python's message carries it.
fn os_error_text(error: &std::io::Error, path: &str) -> String {
    format!("{}: {}", fav_net::download::py_os_error_text(error), repr_str(path))
}

fn os_error(error: &std::io::Error, path: &str) -> JobError {
    JobError::new(fav_net::download::io_class_name(error), os_error_text(error, path))
}

// ---------- the session ----------

/// `SessionCheck`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCheck {
    pub ok: bool,
    pub account: String,
    pub message: String,
}

impl SessionCheck {
    fn failed(account: String, message: String) -> Self {
        Self {
            ok: false,
            account,
            message,
        }
    }

    fn accepted(account: String) -> Self {
        Self {
            ok: true,
            account,
            message: String::new(),
        }
    }
}

/// `user_id_from_twid`: the numeric account id X files in the `twid` cookie, or empty.
pub fn user_id_from_twid(cookies: &[StoredCookie]) -> String {
    let unquoted = fav_core::pyurl::unquote(cookie_value(cookies, USER_ID_COOKIE));
    let raw = unquoted.trim_matches('"');
    match raw.strip_prefix("u=") {
        Some(user_id) if py_isdigit(user_id) => user_id.to_owned(),
        _ => String::new(),
    }
}

/// `verify_session`: whether a pair of X cookies is a live login, and for which account.
///
/// X sets `auth_token` only when a login completes, so its presence is most of the answer.
/// The account's own settings then name the screen name, which has to be the configured
/// `username`: the likes timeline is readable only by its owner. An answer that is neither
/// a rejection nor a screen name (a retired endpoint, a rate limit) does not hold a login
/// up; the crawl proves the session from then on.
pub async fn verify_session(cookies: &[StoredCookie], username: &str, proxy: &str) -> SessionCheck {
    verify_session_at(ACCOUNT_SETTINGS_URL, cookies, username, proxy).await
}

/// [`verify_session`] against another URL (tests).
pub async fn verify_session_at(url: &str, cookies: &[StoredCookie], username: &str, proxy: &str) -> SessionCheck {
    let auth_token = cookie_value(cookies, SESSION_COOKIE);
    let csrf_token = cookie_value(cookies, CSRF_COOKIE);
    if auth_token.is_empty() || csrf_token.is_empty() {
        return SessionCheck::failed(String::new(), "no auth_token/ct0 cookies yet; not signed in".to_owned());
    }
    let fallback_account = if username.is_empty() {
        user_id_from_twid(cookies)
    } else {
        username.to_owned()
    };
    let unreachable = |class: &str| SessionCheck::failed(String::new(), format!("X unreachable: {class}"));
    let client = match settings_client(proxy) {
        Ok(client) => client,
        Err(error) => return unreachable(error.class_name()),
    };
    let request = client
        .get(url)
        .header(header::AUTHORIZATION, format!("Bearer {WEB_BEARER_TOKEN}"))
        .header("x-csrf-token", csrf_token)
        .header("x-twitter-auth-type", "OAuth2Session")
        .header("x-twitter-active-user", "yes")
        .header(header::REFERER, "https://x.com/")
        .header(header::COOKIE, format!("auth_token={auth_token}; ct0={csrf_token}"));
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return unreachable(HttpError::transport(&error).class_name()),
    };
    let status = response.status().as_u16();
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => return unreachable(HttpError::transport(&error).class_name()),
    };
    if status == HTTP_UNAUTHORIZED || status == HTTP_FORBIDDEN {
        return SessionCheck::failed(String::new(), format!("X rejected the session (HTTP {status})"));
    }
    let screen_name = match pydecode::json_from_bytes(&body) {
        Ok(Value::Dict(payload)) => payload
            .get("screen_name")
            .filter(|name| name.is_truthy())
            .map(str_value)
            .unwrap_or_default(),
        _ => String::new(),
    };
    if status != HTTP_OK || screen_name.is_empty() {
        log().warning(format_args!(
            "X account settings answered HTTP {status} without a screen name; accepting the login"
        ));
        return SessionCheck::accepted(fallback_account);
    }
    if !username.is_empty() && fav_core::pycasefold::casefold(&screen_name) != fav_core::pycasefold::casefold(username) {
        let message = format!(
            "signed in as @{screen_name}, but web.twitter.username is @{username}; the likes timeline is only readable by its owner"
        );
        return SessionCheck::failed(screen_name, message);
    }
    SessionCheck::accepted(screen_name)
}

/// The account-settings client: thirty seconds for every phase, no redirects, an explicit
/// proxy replacing the environment's (httpx's `proxy=`).
fn settings_client(proxy: &str) -> Result<reqwest::Client, HttpError> {
    let mut builder = fav_net::client_builder()
        .user_agent(USER_AGENT)
        .connect_timeout(VERIFY_TIMEOUT)
        .read_timeout(VERIFY_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    if !proxy.is_empty() {
        let unknown_scheme = || HttpError::Transport {
            class: "UnsupportedProtocol",
            message: "Unknown scheme for proxy URL".to_owned(),
            host: String::new(),
        };
        // httpx's `Proxy` check; reqwest itself would take `host:port` as an HTTP proxy.
        if !fav_net::proxy::is_httpx_proxy(proxy) {
            return Err(unknown_scheme());
        }
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| unknown_scheme())?);
    }
    builder.build().map_err(|error| HttpError::transport(&error))
}

// ---------- gallery-dl ----------

/// `build_command`: the gallery-dl invocation for one run.
///
/// `incremental` stops the run once `abort_after` consecutive files are already in the
/// archive. It is off during backfill, where stopping at the top of the timeline would
/// leave the older half unreachable.
pub fn build_command(config: &TwitterConfig, cookie_path: &str, archive_path: &str, incremental: bool) -> Vec<String> {
    let directory = Value::List(DIRECTORY_FORMAT.iter().map(|part| Value::from(*part)).collect());
    let directory = pyjson::dumps(&directory, &DumpOptions::python_default()).unwrap_or_default();
    let mut command = vec![
        "gallery-dl".to_owned(),
        "--destination".to_owned(),
        config.path.as_str().to_owned(),
        "--cookies".to_owned(),
        cookie_path.to_owned(),
        "--download-archive".to_owned(),
        archive_path.to_owned(),
        "--write-metadata".to_owned(),
        "--option".to_owned(),
        format!("extractor.twitter.directory={directory}"),
        "--option".to_owned(),
        format!("extractor.twitter.filename={FILENAME_FORMAT}"),
        "--option".to_owned(),
        format!(
            "extractor.twitter.sleep-request={}",
            pyjson::float_repr(config.sleep_request_seconds)
        ),
        // "original" files a liked retweet under the account that wrote it rather than the
        // account that boosted it.
        "--option".to_owned(),
        format!(
            "extractor.twitter.retweets={}",
            if config.include_retweets { "original" } else { "false" }
        ),
        // Stated rather than inherited: gallery-dl's own default, which an upstream change
        // would otherwise alter silently.
        "--option".to_owned(),
        format!("extractor.twitter.videos={}", if config.include_videos { "true" } else { "false" }),
    ];
    if incremental {
        command.extend([
            "--option".to_owned(),
            format!("extractor.twitter.skip=abort:{}", config.abort_after),
        ]);
    }
    if !config.proxy.is_empty() {
        command.extend(["--proxy".to_owned(), config.proxy.clone()]);
    }
    command.push(format!("https://x.com/{}/likes", config.username));
    command
}

/// `is_transient_user_lookup_failure`: whether a run died on gallery-dl's username lookup,
/// which X answers flakily.
///
/// A missing account fails the same way, so this only earns another attempt. `[info]`
/// lines are ignored (gallery-dl logs its transaction-key refresh to stderr), while any
/// warning means the run got past the lookup.
pub fn is_transient_user_lookup_failure(returncode: i32, errors: &[String]) -> bool {
    if returncode != EXIT_EXTRACTION {
        return false;
    }
    let relevant: Vec<&String> = errors.iter().filter(|line| !line.contains("[info]")).collect();
    relevant.len() == 1 && relevant[0].ends_with(USER_LOOKUP_ERROR)
}

// ---------- sidecars ----------

/// One `twitter` row, from a gallery-dl metadata sidecar.
#[derive(Debug, Clone, PartialEq)]
pub struct SidecarRow {
    pub tweet_id: i128,
    pub num: i128,
    pub author: String,
    pub author_nick: String,
    pub author_id: String,
    pub content: String,
    pub tweet_date: String,
    pub media_type: String,
    pub local_path: String,
    pub metadata: String,
}

/// `isinstance(value, int)`: a bool is an int in Python.
fn py_int(value: Option<&Value>) -> Option<i128> {
    match value? {
        Value::Int(int) => Some(*int),
        Value::Bool(flag) => Some(i128::from(*flag)),
        _ => None,
    }
}

/// `str(value or '')`.
fn text_or_empty(value: Option<&Value>) -> String {
    value.filter(|value| value.is_truthy()).map(str_value).unwrap_or_default()
}

/// `parse_sidecar`: one sidecar as a row, or `None` for one without the identifiers the
/// table is keyed on (a metadata file from some other tool).
pub fn parse_sidecar(payload: &Dict, media_path: &str, root: &str) -> Option<SidecarRow> {
    let tweet_id = py_int(payload.get("tweet_id"))?;
    let num = py_int(payload.get("num"))?;
    let empty = Dict::new();
    let author = match payload.get("author") {
        Some(Value::Dict(author)) => author,
        _ => &empty,
    };
    let media_path = PyPath::new(media_path);
    let local_path = relative_to(media_path.as_str(), root).unwrap_or_else(|| media_path.as_str().to_owned());
    // `json.dumps(payload, ensure_ascii=False, default=str)`: a parsed sidecar holds only
    // JSON values, so `default` never runs.
    let metadata = pyjson::dumps(&Value::Dict(payload.clone()), &DumpOptions::plain()).unwrap_or_default();
    Some(SidecarRow {
        tweet_id,
        num,
        author: text_or_empty(author.get("name")),
        author_nick: text_or_empty(author.get("nick")),
        author_id: text_or_empty(author.get("id")),
        content: text_or_empty(payload.get("content")),
        tweet_date: text_or_empty(payload.get("date")),
        media_type: text_or_empty(payload.get("type")),
        local_path,
        metadata,
    })
}

/// A normalised path's anchor (`/`, `//` or nothing) and its parts.
fn path_parts(path: &str) -> (&str, Vec<&str>) {
    let anchor = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts = path[anchor.len()..]
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    (anchor, parts)
}

/// `str(PurePosixPath(path).relative_to(root))`, or `None` where it raises `ValueError`.
pub fn relative_to(path: &str, root: &str) -> Option<String> {
    let path = PyPath::new(path);
    let root = PyPath::new(root);
    let (path_anchor, parts) = path_parts(path.as_str());
    let (root_anchor, root_parts) = path_parts(root.as_str());
    if path_anchor != root_anchor || !parts.starts_with(&root_parts) {
        return None;
    }
    let rest = &parts[root_parts.len()..];
    Some(if rest.is_empty() { ".".to_owned() } else { rest.join("/") })
}

/// `PurePosixPath.with_suffix('')`.
fn without_suffix(path: &str) -> String {
    let suffix = fav_core::pypath::suffix(path);
    path[..path.len() - suffix.len()].to_owned()
}

/// `sorted(root.rglob('*.json'))`: every entry under `root` whose name ends in `.json`,
/// in `PurePath` order (part by part).
fn sidecars_under(root: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = fav_core::pypath::join(&directory, &name);
            if name.ends_with(SIDECAR_SUFFIX) {
                found.push(path.clone());
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(path);
            }
        }
    }
    found.sort_by(|left, right| left.split('/').cmp(right.split('/')));
    found
}

// ---------- the crawler ----------

/// `Twitter`: one run's settings, database, notifier and gallery-dl.
pub struct Twitter {
    db: Db,
    notifier: SharedNotifier,
    runner: Arc<dyn Runner>,
    config: TwitterConfig,
    /// `asyncio.sleep`: the user-lookup retry wait goes through it.
    sleeper: Sleeper,
}

impl std::fmt::Debug for Twitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Twitter")
            .field("username", &self.config.username)
            .finish_non_exhaustive()
    }
}

impl Twitter {
    pub fn new(db: Db, notifier: SharedNotifier, runner: Arc<dyn Runner>, config: TwitterConfig) -> Self {
        Self {
            db,
            notifier,
            runner,
            config,
            sleeper: tokio_sleeper(),
        }
    }

    /// Sleep through `sleeper` (tests record the waits instead of taking them).
    #[must_use]
    pub fn with_sleeper(mut self, sleeper: Sleeper) -> Self {
        self.sleeper = sleeper;
        self
    }

    async fn backfill_complete(&self) -> Result<bool, JobError> {
        let row = sqlx::query("SELECT value FROM twitter_state WHERE key = $1;")
            .bind(BACKFILL_STATE_KEY)
            .fetch_optional(self.db.pool())
            .await
            .map_err(sql_error)?;
        let value: Option<String> = row.map(|row| row.try_get("value")).transpose().map_err(sql_error)?;
        Ok(value.as_deref() == Some("1"))
    }

    async fn mark_backfill_complete(&self) -> Result<(), JobError> {
        sqlx::query("INSERT INTO twitter_state (key, value) VALUES ($1, $2) ON CONFLICT DO NOTHING;")
            .bind(BACKFILL_STATE_KEY)
            .bind("1")
            .execute(self.db.pool())
            .await
            .map_err(sql_error)?;
        Ok(())
    }

    /// `_resolve_session`: the cookies captured on the 登录态 page.
    async fn resolve_session(&self) -> Result<Vec<StoredCookie>, JobError> {
        let stored = site_sessions::load(&self.db, SESSION_SITE).await.map_err(session_store_error)?;
        match stored {
            Some(stored) if !stored.cookie(SESSION_COOKIE).is_empty() && !stored.cookie(CSRF_COOKIE).is_empty() => Ok(stored.cookies),
            _ => Err(download_error(format!("X is not signed in. {RELOGIN_HINT}"), AUTH_DEDUPE_KEY)),
        }
    }

    /// `_run_gallery_dl`: gallery-dl to completion, its exit code and the tail of its
    /// errors. stdout (a line per downloaded file) is logged at debug; stderr carries the
    /// throttling and authentication messages that explain a bad exit, so it is logged as
    /// warnings and kept for the failure notification.
    pub async fn run_gallery_dl(&self, command: &[String]) -> Result<(i32, Vec<String>), JobError> {
        log().info(format_args!("Running gallery-dl for {} likes", self.config.username));
        let errors = Mutex::new(Vec::new());
        let mut sink = |stream: Stream, line: &str| {
            if line.is_empty() {
                return;
            }
            match stream {
                Stream::Stdout => log().debug(format_args!("gallery-dl: {line}")),
                Stream::Stderr => {
                    log().warning(format_args!("gallery-dl: {line}"));
                    let mut errors = errors.lock().expect("errors");
                    errors.push(line.to_owned());
                    let excess = errors.len().saturating_sub(ERROR_TAIL_LINES);
                    errors.drain(..excess);
                }
            }
        };
        let returncode = self
            .runner
            .run_lines(command.to_vec(), &mut sink)
            .await
            .map_err(|error| JobError::new(fav_net::download::io_class_name(&error), os_error_text(&error, &command[0])))?;
        Ok((returncode, errors.into_inner().expect("errors")))
    }

    /// `_run_gallery_dl_with_retry`: run gallery-dl, again when X flaked on the lookup.
    pub async fn run_gallery_dl_with_retry(&self, command: &[String]) -> Result<(i32, Vec<String>), JobError> {
        let (mut returncode, mut errors) = self.run_gallery_dl(command).await?;
        for attempt in 1..=USER_LOOKUP_RETRIES {
            if !is_transient_user_lookup_failure(returncode, &errors) {
                break;
            }
            log().warning(format_args!(
                "X did not resolve {}; retrying in {:.0}s ({attempt}/{USER_LOOKUP_RETRIES})",
                self.config.username,
                USER_LOOKUP_RETRY_DELAY.as_secs_f64()
            ));
            (self.sleeper)(USER_LOOKUP_RETRY_DELAY.as_secs_f64()).await;
            (returncode, errors) = self.run_gallery_dl(command).await?;
        }
        Ok((returncode, errors))
    }

    /// `_destination_root`: the configured directory a file of this type belongs under.
    pub fn destination_root(&self, media_type: &str) -> &PyPath {
        match &self.config.video_path {
            Some(video_path) if VIDEO_TYPES.contains(&media_type) => video_path,
            _ => &self.config.path,
        }
    }

    /// `_destination_path`: where a downloaded file belongs once its type is known.
    /// gallery-dl picks a directory once per tweet, before it knows whether the files are
    /// photos or videos, so the split happens here.
    pub fn destination_path(&self, media_path: &str, root: &PyPath) -> String {
        let media_path = PyPath::new(media_path);
        if *root == self.config.path {
            return media_path.as_str().to_owned();
        }
        if let Some(relative) = relative_to(media_path.as_str(), self.config.path.as_str()) {
            return fav_core::pypath::join(root.as_str(), &relative);
        }
        log().warning(format_args!(
            "Leaving {} where it is: not under the collection directory",
            media_path.as_str()
        ));
        media_path.as_str().to_owned()
    }

    /// `_ingest_sidecars`: a row per downloaded file, then the sidecar that produced it is
    /// dropped.
    pub async fn ingest_sidecars(&self) -> Result<u64, JobError> {
        let mut inserted = 0;
        for sidecar in sidecars_under(self.config.path.as_str()) {
            let media_path = without_suffix(&sidecar);
            let payload = match read_sidecar(&sidecar) {
                Ok(payload) => payload,
                Err(error) => {
                    log().warning(format_args!("Skipping unreadable sidecar {sidecar}: {error}"));
                    continue;
                }
            };
            let Value::Dict(payload) = payload else {
                log().warning(format_args!("Skipping sidecar without tweet identifiers: {sidecar}"));
                continue;
            };
            let media_type = text_or_empty(payload.get("type"));
            let root = self.destination_root(&media_type).clone();
            let destination = self.destination_path(&media_path, &root);
            // Recorded relative to the directory the file actually lives in, and parsed
            // before the move so a sidecar that is not a tweet leaves the file alone.
            let Some(row) = parse_sidecar(&payload, &destination, root.as_str()) else {
                log().warning(format_args!("Skipping sidecar without tweet identifiers: {sidecar}"));
                continue;
            };
            {
                let (media_path, destination) = (media_path.clone(), destination.clone());
                crate::notifier::blocking(move || relocate(&media_path, &destination)).await?;
            }
            insert_row(&self.db, &row).await?;
            if let Err(error) = std::fs::remove_file(&sidecar)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(os_error(&error, &sidecar));
            }
            inserted += 1;
        }
        Ok(inserted)
    }

    async fn notify_summary(&self, downloaded: u64, backfilling: bool) {
        let mut payload = Dict::new();
        payload.insert("downloaded", Value::Int(i128::from(downloaded)));
        payload.insert("backfilling", Value::Bool(backfilling));
        payload.insert("username", Value::from(self.config.username.as_str()));
        let notification = NewNotification {
            kind: "summary".to_owned(),
            source: JOB_KEY.to_owned(),
            header: "X".to_owned(),
            title: "Likes update completed".to_owned(),
            body: format!("Downloaded {downloaded} files from liked tweets."),
            payload: Some(payload),
            ..NewNotification::default()
        };
        if let Err(error) = self.notifier.enqueue(notification).await {
            log().warning(format_args!("Failed to enqueue twitter summary notification: {error}"));
        }
    }

    /// `update()`: one crawl of the likes timeline.
    pub async fn update(&mut self) -> Result<(), JobError> {
        let missing = self.config.validate_runnable();
        if !missing.is_empty() {
            log().warning(format_args!(
                "Twitter is not configured (missing {}), skip update",
                missing.join(", ")
            ));
            return Ok(());
        }
        let path = self.config.path.as_str().to_owned();
        std::fs::create_dir_all(&path).map_err(|error| os_error(&error, &path))?;
        if let Some(video_path) = &self.config.video_path {
            std::fs::create_dir_all(video_path.as_str()).map_err(|error| os_error(&error, video_path.as_str()))?;
        }

        let cookies = self.resolve_session().await?;
        let backfill_complete = self.backfill_complete().await?;
        let (returncode, errors) = {
            let tmp_dir = tempfile::tempdir().map_err(|error| {
                JobError::new(
                    fav_net::download::io_class_name(&error),
                    fav_net::download::py_os_error_text(&error),
                )
            })?;
            let cookie_path = tmp_dir.path().join(COOKIE_FILENAME);
            let cookie_text = cookie_path.to_string_lossy().into_owned();
            std::fs::write(&cookie_path, site_sessions::to_netscape(&cookies)).map_err(|error| os_error(&error, &cookie_text))?;
            let archive = fav_core::pypath::join(&path, ARCHIVE_FILENAME);
            let command = build_command(&self.config, &cookie_text, &archive, backfill_complete);
            self.run_gallery_dl_with_retry(&command).await?
        };

        // Ingest whatever was downloaded before judging the exit code: a run that died
        // halfway still fetched files, and their sidecars are on disk.
        let downloaded = self.ingest_sidecars().await?;

        if returncode & EXIT_AUTH != 0 {
            let message = format!("gallery-dl could not authenticate with X. {RELOGIN_HINT}");
            site_sessions::record_check(&self.db, SESSION_SITE, false, &message)
                .await
                .map_err(session_store_error)?;
            return Err(download_error(message, AUTH_DEDUPE_KEY));
        }
        if returncode != 0 {
            let detail = errors.join(" | ");
            let message = if detail.is_empty() {
                format!("gallery-dl exited with code {returncode}")
            } else {
                format!("gallery-dl exited with code {returncode}: {detail}")
            };
            return Err(download_error(message, &format!("twitter:exit:{returncode}")));
        }

        site_sessions::record_check(&self.db, SESSION_SITE, true, "")
            .await
            .map_err(session_store_error)?;
        if !backfill_complete {
            // A clean exit without an early abort means the whole timeline was walked, so
            // later runs can stop as soon as they recognise the top of it.
            self.mark_backfill_complete().await?;
            log().info(format_args!(
                "Twitter likes backfill finished; later runs stop after {} known files",
                self.config.abort_after
            ));
        }
        log().info(format_args!("Twitter downloaded {downloaded} new files"));
        if downloaded > 0 {
            self.notify_summary(downloaded, !backfill_complete).await;
        }
        Ok(())
    }
}

/// `json.loads(sidecar.read_text(encoding='utf-8'))`; the error is `str(exc)`.
fn read_sidecar(sidecar: &str) -> Result<Value, String> {
    let bytes = std::fs::read(sidecar).map_err(|error| os_error_text(&error, sidecar))?;
    let text = pydecode::decode_utf8(&bytes).map_err(|error: PyError| error.message)?;
    // Text mode: universal newlines.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    pydecode::json_loads(&text).map_err(|error| error.message)
}

/// `_relocate`: move a file to its media type's directory, if it is not there already. A
/// missing source means an earlier run moved it and died before clearing the sidecar.
fn relocate(media_path: &str, destination: &str) -> Result<(), JobError> {
    if destination == media_path || !Path::new(media_path).exists() {
        return Ok(());
    }
    if let Some(parent) = Path::new(destination).parent() {
        std::fs::create_dir_all(parent).map_err(|error| os_error(&error, &parent.to_string_lossy()))?;
    }
    // A move, not a rename: the video directory is usually another mount.
    fav_net::download::move_file(Path::new(media_path), &PathBuf::from(destination)).map_err(|error| os_error(&error, media_path))
}

async fn insert_row(db: &Db, row: &SidecarRow) -> Result<(), JobError> {
    let out_of_range = |what: &str| JobError::new("NumericValueOutOfRange", format!("{what} out of range"));
    let tweet_id = i64::try_from(row.tweet_id).map_err(|_| out_of_range("bigint"))?;
    let num = i32::try_from(row.num).map_err(|_| out_of_range("integer"))?;
    sqlx::query(
        "INSERT INTO twitter (
             tweet_id, num, author, author_nick, author_id,
             content, tweet_date, media_type, local_path, metadata
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::jsonb)
         ON CONFLICT (tweet_id, num) DO UPDATE SET
             local_path = EXCLUDED.local_path,
             metadata = EXCLUDED.metadata;",
    )
    .bind(tweet_id)
    .bind(num)
    .bind(&row.author)
    .bind(&row.author_nick)
    .bind(&row.author_id)
    .bind(&row.content)
    .bind(&row.tweet_date)
    .bind(&row.media_type)
    .bind(&row.local_path)
    .bind(&row.metadata)
    .execute(db.pool())
    .await
    .map_err(sql_error)?;
    Ok(())
}

// ---------- the job ----------

/// The `twitter` job: settings and clients are read when the run starts.
struct TwitterJob {
    ctx: SourceContext,
}

impl Source for TwitterJob {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self.ctx.settings.load().await.map_err(|error| crate::settings_job_error(&error))?;
            let notifier: SharedNotifier = Arc::new(OutboxNotifier(Outbox::new(self.ctx.db.clone(), Arc::clone(&self.ctx.settings))));
            let mut source = Twitter::new(self.ctx.db.clone(), notifier, Arc::new(SystemRunner), settings.web.twitter.clone());
            source.update().await
        })
    }
}

/// Register the `twitter` job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run(JOB_KEY, move || Box::new(TwitterJob { ctx: ctx.clone() }) as Box<dyn Source>);
}

#[cfg(test)]
mod tests;
