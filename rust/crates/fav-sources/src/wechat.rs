//! Images, stickers, 公众号 articles, Tieba, NGA and Zhihu posts and 视频号 videos forwarded
//! to the agent's WeChat account.
//!
//! Python origin: `src/web/wechat.py`.
//!
//! WeChat runs in redroid on another host, with the `WAuxiliary` `FavInbox` plugin inside it.
//! A small HTTP service next to it answers two APIs at one address, behind one token: the
//! bridge under `/wx/` (the plugin's view of WeChat: `/wx/events`, the log of every message
//! the account sees, numbered by `seq`, and `/wx/image`, an image message's original), and
//! the inbox (`GET /items`, `GET /file/<msg_id>` and `POST /ack/<msg_id>`), the originals the
//! plugin saved on its own as each image arrived.
//!
//! A run reads the events after its cursor in `wechat_state` and queues the ones worth
//! keeping in `wechat_events`, so the cursor moves on even when saving an item fails. It then
//! works that queue: an image comes through the bridge, or from the inbox when the bridge
//! cannot; a sticker straight from WeChat's CDN ([`crate::wechat_emoji`]); a shared link is
//! first opened in the agent's WeChat (`POST /link/open`) and then read by
//! [`crate::post_links`], an image post behind the site's captcha through `POST /post/page`;
//! a 视频号 video is looked up through TikHub and its start decrypted here
//! ([`crate::wechat_channels`]). Any other message received in a private chat is reported
//! through the notification channels, and the row waits as `reported` until this source
//! learns to save it. A failed item is retried up to [`MAX_ATTEMPTS`] times. Last, the run
//! drains the inbox, which catches what the event log misses.
//!
//! Items are keyed on WeChat's local `msg_id` together with the message's `create_time`.
//!
//! Python's per-request timeouts are clients of their own here, as in [`crate::post_links`]
//! (DIVERGENCES.md D138). Never run against the real service, WeChat or TikHub from tests:
//! they talk to a scripted fake web.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Datelike as _, FixedOffset, Timelike as _, Utc};
use fav_core::filename::{ensure_unique_path, sanitize};
use fav_core::logging::{self, Logger};
use fav_core::pyjson::{self, Dict, DumpOptions, Value};
use fav_core::pytext::{py_isdigit, py_strip};
use fav_core::settings::Wechat as WechatConfig;
use fav_db::{Db, SettingsStore};
use fav_net::client::BuildError;
use fav_net::download::{io_class_name, py_os_error_text};
use fav_net::{ClientOptions, HttpClient, Request};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{KIND_UNSUPPORTED_MESSAGE, KIND_UNSUPPORTED_RESOLVED, NewNotification, Outbox, Resolution};
use fav_runtime::pystr::repr_str;
use fav_runtime::source::{JobError, Registry, Source};
use sqlx::Row as _;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

use crate::SourceContext;
use crate::notifier::{OutboxNotifier, SharedNotifier};
use crate::post_links::{self, Fetched, Needs, PostClient, PostError, PostLinks, SessionStore, is_mp4, shared_link, transport_error};
use crate::pydecode::{PyError, json_from_bytes, py_int, urlencode};
use crate::pyerr::type_name;
use crate::pyre::compile;
use crate::pyval::{py_str, str_or_empty};
use crate::wechat_article::{
    ArticleLink, ArticlePage, LINK_SHARE_TYPE, PostVideo, VIDEO_SHARE_TYPE, appmsg_link, canonical_url, cover_only_page, parse_article_page,
};
use crate::wechat_channels::{
    ChannelVideo, ENCRYPTED_BYTES, VideoSource, decrypt_in_place, parse_channel_video, parse_video_source, tikhub_lookup, video_title,
};
use crate::wechat_emoji::{Sticker, decrypt_sticker, matches, parse_sticker};

pub const JOB_KEY: &str = "wechat";
/// The source's `JOB_SPECS` name.
const HEADER: &str = "WeChat";

/// The extensions the plugin writes after sniffing the bytes. `wxgf` is WeChat's own
/// animated format and `bin` an unrecognised one; both are still the original bytes.
pub const KNOWN_EXTENSIONS: [&str; 6] = ["bin", "gif", "jpg", "png", "webp", "wxgf"];
pub const CONTENT_TYPE_EXTENSIONS: [(&str, &str); 4] = [
    ("image/jpeg", "jpg"),
    ("image/png", "png"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
];
const MAGIC_EXTENSIONS: [(&[u8], &str); 4] = [(b"\xff\xd8\xff", "jpg"), (b"\x89PNG", "png"), (b"GIF8", "gif"), (b"wxgf", "wxgf")];
pub const REQUEST_TIMEOUT_SECONDS: u64 = 60;
pub const EVENT_PAGE_SIZE: usize = 200;
pub const MAX_ATTEMPTS: i64 = 5;
/// How long the `FavInbox` service waits for WeChat to load an image post it opened.
pub const OPEN_WAIT_SECONDS: u64 = 30;
/// How long it waits for WeChat to load any other link it opened before closing it.
pub const LINK_WAIT_SECONDS: u64 = 20;
/// How long WeChat may take to reach a message before opening it: it scrolls the chat back
/// as far as the message is, however old.
pub const TAP_OPEN_SECONDS: u64 = 1200;
pub const CURSOR_KEY: &str = "events_after";
/// What [`work_for`] recognises. Bump it whenever it learns a new kind of message, so the
/// event log is read again from the start and messages skipped before are picked up.
pub const WORK_VERSION: &str = "8";
pub const WORK_VERSION_KEY: &str = "work_version";
/// From when messages with nothing to save are reported (ms, as `create_time`).
pub const UNSUPPORTED_SINCE_KEY: &str = "unsupported_since";
pub const NGA_LOGIN_KEY: &str = "wechat:nga-login";
pub const TIKHUB_KEY: &str = "wechat:tikhub";
const AUTH_KEY: &str = "wechat:auth";
const UNREACHABLE_KEY: &str = "wechat:unreachable";
const BRIDGE_KEY: &str = "wechat:bridge";
const LIST_KEY: &str = "wechat:list";
const DOWNLOAD_KEY: &str = "wechat:download";
const WORK_KEY: &str = "wechat:work";
const LOGIN_KEY: &str = "wechat:login";
pub const TIKHUB_VIDEO_DETAIL: &str = "/api/v1/wechat_channels/v2/fetch_video_detail";
/// TikHub's answers to a missing or spent key: settings to fix, not failed tries.
pub const TIKHUB_ACCOUNT_STATUSES: [u16; 3] = [401, 402, 403];

pub const WORK_IMAGE: &str = "image";
pub const WORK_STICKER: &str = "sticker";
pub const WORK_ARTICLE: &str = "article";
pub const WORK_CHANNEL_VIDEO: &str = "channel_video";
pub const WORK_UNSUPPORTED: &str = "unsupported";
/// Messages that carry nothing anyone sent on purpose: not reported as unsupported.
pub const SILENT_KINDS: [&str; 4] = ["pat", "recalled", "system", "voip"];

static APPMSG_TITLE: LazyLock<regex::Regex> =
    LazyLock::new(|| compile(r"<appmsg\b.*?<title>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</title>", false, true));

fn log() -> Logger {
    logging::get("wechat")
}

// ---------- Python's exceptions ----------

/// The httpx exception classes that are `httpx.HTTPError` (everything a request raises but
/// `InvalidURL`).
const HTTP_ERRORS: [&str; 15] = [
    "HTTPError",
    "HTTPStatusError",
    "ConnectTimeout",
    "ReadTimeout",
    "WriteTimeout",
    "PoolTimeout",
    "ConnectError",
    "ReadError",
    "WriteError",
    "RemoteProtocolError",
    "LocalProtocolError",
    "UnsupportedProtocol",
    "TooManyRedirects",
    "DecodingError",
    "ProxyError",
];
const OS_ERRORS: [&str; 6] = [
    "OSError",
    "FileNotFoundError",
    "PermissionError",
    "FileExistsError",
    "IsADirectoryError",
    "NotADirectoryError",
];
/// `ValueError` and the subclasses a JSON read raises.
const VALUE_ERRORS: [&str; 3] = ["ValueError", "JSONDecodeError", "UnicodeDecodeError"];

fn is_http_error(error: &PyError) -> bool {
    HTTP_ERRORS.contains(&error.class.as_str())
}

fn is_value_error(error: &PyError) -> bool {
    VALUE_ERRORS.contains(&error.class.as_str())
}

/// What a step of a run fails with.
#[derive(Debug)]
enum Failure {
    /// `WechatInboxError(message, notification_dedupe_key=key)`.
    Inbox { message: String, key: &'static str },
    /// Any other Python exception: httpx's, an `OSError`, a `ValueError`.
    Py(PyError),
    /// A database or settings failure, which ends the run.
    Job(JobError),
}

impl Failure {
    fn inbox(message: impl Into<String>) -> Self {
        Self::Inbox {
            message: message.into(),
            key: "",
        }
    }

    fn keyed(message: impl Into<String>, key: &'static str) -> Self {
        Self::Inbox {
            message: message.into(),
            key,
        }
    }

    /// `except (httpx.HTTPError, OSError, WechatInboxError)`.
    fn is_caught(&self) -> bool {
        match self {
            Self::Inbox { .. } => true,
            Self::Py(error) => is_http_error(error) || OS_ERRORS.contains(&error.class.as_str()),
            Self::Job(_) => false,
        }
    }

    /// `str(exc)`.
    fn text(&self) -> String {
        match self {
            Self::Inbox { message, .. } => message.clone(),
            Self::Py(error) => error.message.clone(),
            Self::Job(error) => error.message().to_owned(),
        }
    }

    /// `str(exc) or type(exc).__name__`.
    fn text_or_class(&self) -> String {
        let text = self.text();
        if !text.is_empty() {
            return text;
        }
        match self {
            Self::Inbox { .. } => "WechatInboxError".to_owned(),
            Self::Py(error) => error.class.clone(),
            Self::Job(error) => error.class_name().to_owned(),
        }
    }

    fn dedupe_key(&self) -> &'static str {
        match self {
            Self::Inbox { key, .. } => key,
            _ => "",
        }
    }

    fn into_job_error(self) -> JobError {
        match self {
            Self::Inbox { message, key } => inbox_job_error(message, key),
            Self::Py(error) => error.into_job_error(),
            Self::Job(error) => error,
        }
    }
}

impl From<PyError> for Failure {
    fn from(error: PyError) -> Self {
        Self::Py(error)
    }
}

impl From<JobError> for Failure {
    fn from(error: JobError) -> Self {
        Self::Job(error)
    }
}

fn inbox_job_error(message: String, key: &str) -> JobError {
    let error = JobError::new("WechatInboxError", message);
    if key.is_empty() {
        error
    } else {
        error.with_notification_dedupe_key(key)
    }
}

fn sql_error(error: sqlx::Error) -> Failure {
    Failure::Job(crate::db_job_error(&fav_db::DbError::from(error)))
}

fn key_error(key: &str) -> PyError {
    PyError::new("KeyError", repr_str(key))
}

/// `d[key]`.
fn item<'a>(dict: &'a Dict, key: &str) -> Result<&'a Value, PyError> {
    dict.get(key).ok_or_else(|| key_error(key))
}

/// `isinstance(value, int)`, which a bool is too.
fn is_int(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Int(_) | Value::Bool(_)))
}

/// The number an `int` (or bool) value stands for.
fn int_value(value: &Value) -> i128 {
    match value {
        Value::Int(number) => *number,
        Value::Bool(flag) => i128::from(*flag),
        _ => 0,
    }
}

/// `text.encode('ascii')`'s `UnicodeEncodeError`, naming the first run of characters it
/// cannot encode as Python does; `None` for ASCII text.
fn ascii_encode_error(text: &str) -> Option<PyError> {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.iter().position(|c| !c.is_ascii())?;
    let end = chars[start..]
        .iter()
        .position(char::is_ascii)
        .map_or(chars.len(), |run| start + run);
    let message = if end - start == 1 {
        let code = u32::from(chars[start]);
        let escaped = if code < 0x100 {
            format!("\\x{code:02x}")
        } else if code < 0x1_0000 {
            format!("\\u{code:04x}")
        } else {
            format!("\\U{code:08x}")
        };
        format!("'ascii' codec can't encode character '{escaped}' in position {start}: ordinal not in range(128)")
    } else {
        format!(
            "'ascii' codec can't encode characters in position {start}-{}: ordinal not in range(128)",
            end - 1
        )
    };
    Some(PyError::new("UnicodeEncodeError", message))
}

/// A value bound to a `BIGINT` column, as psycopg sends it: an int in range, else
/// PostgreSQL's refusal (DIVERGENCES.md D139).
fn bigint(value: &Value, column: &str) -> Result<i64, Failure> {
    match value {
        Value::Int(number) => {
            i64::try_from(*number).map_err(|_| Failure::Job(JobError::new("NumericValueOutOfRange", "bigint out of range")))
        }
        other => Err(Failure::Job(JobError::new(
            "DatatypeMismatch",
            format!("column \"{column}\" is of type bigint but expression is of type {}", pg_type(other)),
        ))),
    }
}

fn pg_type(value: &Value) -> &'static str {
    match value {
        Value::Bool(_) => "boolean",
        Value::Float(_) => "double precision",
        Value::Str(_) => "text",
        Value::Null => "unknown",
        Value::List(_) | Value::Dict(_) => "jsonb",
        Value::Int(_) => "bigint",
    }
}

/// A query parameter as httpx renders it (`primitive_value_to_str`).
fn param_text(value: &Value) -> String {
    match value {
        Value::Bool(true) => "true".to_owned(),
        Value::Bool(false) => "false".to_owned(),
        Value::Null => String::new(),
        other => py_str(other),
    }
}

/// `text[:count]`, by characters.
fn head(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

/// `d.get(a) or d.get(b) or ''` rendered with `str()`.
fn sender_of(dict: &Dict) -> String {
    sender_value(dict).map_or_else(String::new, py_str)
}

/// `d.get('sender') or d.get('talker') or ''`, as the value.
fn sender_value(dict: &Dict) -> Option<&Value> {
    ["sender", "talker"]
        .into_iter()
        .find_map(|key| dict.get(key).filter(|value| value.is_truthy()))
}

fn dumps(value: &Value, options: &DumpOptions) -> Result<String, Failure> {
    pyjson::dumps(value, options).map_err(|error| Failure::Job(JobError::new("TypeError", error.to_string())))
}

/// An `OSError` naming the file, as `open`, `mkdir` and `unlink` raise it.
fn os_error(error: &std::io::Error, path: &Path) -> PyError {
    PyError::new(
        io_class_name(error),
        format!("{}: {}", py_os_error_text(error), repr_str(&path.to_string_lossy())),
    )
}

/// An `OSError` from `rename`, which names both files.
fn rename_error(error: &std::io::Error, source: &Path, destination: &Path) -> PyError {
    PyError::new(
        io_class_name(error),
        format!(
            "{}: {} -> {}",
            py_os_error_text(error),
            repr_str(&source.to_string_lossy()),
            repr_str(&destination.to_string_lossy())
        ),
    )
}

/// An `OSError` from reading or writing an open file, which names no file.
fn io_error(error: &std::io::Error) -> PyError {
    PyError::new(io_class_name(error), py_os_error_text(error))
}

async fn remove_file(path: &Path) -> Result<(), PyError> {
    match tokio::fs::remove_file(path).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(os_error(&error, path)),
        _ => Ok(()),
    }
}

fn with_name(path: &Path, name: &str) -> PathBuf {
    path.with_file_name(name)
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---------- selection and naming ----------

/// `pending_images`: the listed items to download, oldest first. Only images the plugin
/// saved successfully, whose file is still there and that have an integer `msg_id` and
/// `create_time`.
pub fn pending_images(items: &[Value]) -> Vec<Dict> {
    let mut selected: Vec<Dict> = items
        .iter()
        .filter_map(Value::as_dict)
        .filter(|item| item.get("type") == Some(&Value::from("image")) && item.get("has_file").is_some_and(Value::is_truthy))
        .filter(|item| str_or_empty(item.get("status")).starts_with("ok"))
        .filter(|item| is_int(item.get("msg_id")) && is_int(item.get("create_time")))
        .cloned()
        .collect();
    selected.sort_by_key(|item| {
        (
            item.get("create_time").map_or(0, int_value),
            item.get("msg_id").map_or(0, int_value),
        )
    });
    selected
}

/// `message.get('kind') in _SILENT_KINDS`, which raises for a value that cannot be hashed.
fn is_silent(kind: Option<&Value>) -> Result<bool, PyError> {
    match kind {
        Some(Value::Str(kind)) => Ok(SILENT_KINDS.contains(&kind.as_str())),
        Some(value @ (Value::List(_) | Value::Dict(_))) => {
            Err(PyError::new("TypeError", format!("unhashable type: '{}'", type_name(value))))
        }
        _ => Ok(false),
    }
}

/// `work_for`: what a bridge event asks of this source, or `None`.
///
/// Only messages the account received in a private chat count. An image is an image; any
/// other message is a sticker, a shared post or a 视频号 video by its content, else a report
/// unless it is a kind nobody sends on purpose.
pub fn work_for(event: &Value) -> Result<Option<&'static str>, PyError> {
    let Some(event) = event.as_dict() else { return Ok(None) };
    if event.get("kind") != Some(&Value::from("message")) {
        return Ok(None);
    }
    let Some(message) = event.get("data").and_then(Value::as_dict) else {
        return Ok(None);
    };
    if message.get("is_send").is_some_and(Value::is_truthy) || message.get("chat") != Some(&Value::from("private")) {
        return Ok(None);
    }
    if !is_int(message.get("msg_id")) || !is_int(message.get("create_time")) {
        return Ok(None);
    }
    if message.get("kind") == Some(&Value::from("image")) {
        return Ok(Some(WORK_IMAGE));
    }
    let content = str_or_empty(message.get("content"));
    // A large sticker is an app message rather than an emoji one, so the content decides.
    if parse_sticker(&content).is_some() {
        return Ok(Some(WORK_STICKER));
    }
    if shared_link(&content)?.is_some() {
        return Ok(Some(WORK_ARTICLE));
    }
    if parse_channel_video(&content)?.is_some() {
        return Ok(Some(WORK_CHANNEL_VIDEO));
    }
    Ok(if is_silent(message.get("kind"))? {
        None
    } else {
        Some(WORK_UNSUPPORTED)
    })
}

/// `unsupported_reason`: what a message with nothing to save holds, for its report.
pub fn unsupported_reason(message: &Dict) -> Result<String, PyError> {
    let kind = match message.get("kind") {
        Some(kind) if kind.is_truthy() => py_str(kind),
        _ => "unknown".to_owned(),
    };
    let content = str_or_empty(message.get("content"));
    if kind == "text" {
        let what = format!("a text message: {}", head(py_strip(&content), 100));
        return Ok(post_links::unsupported_reason(&post_links::first_url(&[&content]), &what));
    }
    if let Some(card) = appmsg_link(&content, &[LINK_SHARE_TYPE, VIDEO_SHARE_TYPE])? {
        return Ok(post_links::unsupported_reason(&card.url, ""));
    }
    let mut what = format!("a WeChat {kind} message");
    if let Some(title) = APPMSG_TITLE.captures(&content).map(|found| py_strip(&found[1]).to_owned())
        && !title.is_empty()
    {
        what = format!("{what}: {}", head(&title, 100));
    }
    Ok(post_links::unsupported_reason("", &what))
}

/// `queued_message`: the part of a message event worth keeping in the queue. An image
/// message's content and `image` bean carry its CDN address and AES key; neither is kept.
pub fn queued_message(message: &Dict, work: &str) -> Dict {
    let mut kept = Dict::new();
    for key in ["msg_id", "create_time", "type", "kind", "talker", "sender"] {
        kept.insert(key, message.get(key).cloned().unwrap_or(Value::Null));
    }
    if [WORK_ARTICLE, WORK_CHANNEL_VIDEO, WORK_STICKER, WORK_UNSUPPORTED].contains(&work) {
        kept.insert("content", message.get("content").cloned().unwrap_or(Value::Null));
    }
    if work == WORK_IMAGE
        && let Some(Value::Dict(image)) = message.get("image")
    {
        let md5 = image
            .get("md5")
            .filter(|md5| md5.is_truthy())
            .cloned()
            .unwrap_or_else(|| Value::from(""));
        kept.insert("md5", md5);
    }
    kept
}

/// `image_item`: an image message from the queue, in the shape the inbox lists items.
pub fn image_item(message: &Dict) -> Result<Dict, PyError> {
    let mut item = Dict::new();
    item.insert("msg_id", item_of(message, "msg_id")?);
    item.insert("create_time", item_of(message, "create_time")?);
    item.insert("type", Value::from("image"));
    item.insert("talker", Value::Str(str_or_empty(message.get("talker"))));
    item.insert("sender", Value::Str(sender_of(message)));
    item.insert("md5", Value::Str(str_or_empty(message.get("md5"))));
    Ok(item)
}

fn item_of(dict: &Dict, key: &str) -> Result<Value, PyError> {
    item(dict, key).cloned()
}

/// `sniff_extension`: an image's type from its first bytes.
pub fn sniff_extension(content: &[u8]) -> Option<&'static str> {
    for (magic, extension) in MAGIC_EXTENSIONS {
        if content.starts_with(magic) {
            return Some(extension);
        }
    }
    (content.get(..4) == Some(b"RIFF".as_slice()) && content.get(8..12) == Some(b"WEBP".as_slice())).then_some("webp")
}

/// `file_extension`: the extension to store an item under, the plugin's else the response's.
pub fn file_extension(item: &Dict, content_type: &str) -> String {
    let file = str_or_empty(item.get("file"));
    let suffix = fav_core::pypath::suffix(&file).trim_start_matches('.').to_lowercase();
    if KNOWN_EXTENSIONS.contains(&suffix.as_str()) {
        return suffix;
    }
    let media_type = py_strip(content_type.split(';').next().unwrap_or_default()).to_lowercase();
    CONTENT_TYPE_EXTENSIONS
        .iter()
        .find(|(name, _)| *name == media_type)
        .map_or("bin", |(_, extension)| extension)
        .to_owned()
}

/// `sent_at`: `datetime.fromtimestamp(create_time_ms / 1000, tz=UTC)`, through the float
/// Python divides into, with `CPython`'s half-even microseconds and its failures.
pub fn sent_at(create_time_ms: i128) -> Result<DateTime<Utc>, PyError> {
    #[allow(clippy::cast_precision_loss)] // reason: Python's true division of two ints rounds the same way below 2**53
    let timestamp = create_time_ms as f64 / 1000.0;
    let mut whole = timestamp.trunc();
    let mut micro = ((timestamp - whole) * 1e6).round_ties_even();
    if micro >= 1e6 {
        micro -= 1e6;
        whole += 1.0;
    } else if micro < 0.0 {
        micro += 1e6;
        whole -= 1.0;
    }
    #[allow(clippy::cast_precision_loss)] // reason: the bounds CPython checks against, as doubles
    if !((i64::MIN as f64) <= whole && whole < (i64::MAX as f64)) {
        return Err(PyError::new("OverflowError", "timestamp out of range for platform time_t"));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // reason: whole numbers checked to be in range above
    let (seconds, micro) = (whole as i64, micro as u32);
    let at = crate::pyval::fromtimestamp_utc(i128::from(seconds))?;
    Ok(at + chrono::Duration::microseconds(i64::from(micro)))
}

/// `sent_at(value)` for a JSON value: an int, or a bool as Python divides it.
fn sent_at_value(value: &Value) -> Result<DateTime<Utc>, PyError> {
    match value {
        Value::Int(_) | Value::Bool(_) => sent_at(int_value(value)),
        other => Err(PyError::new(
            "TypeError",
            format!("unsupported operand type(s) for /: '{}' and 'int'", type_name(other)),
        )),
    }
}

/// `strftime('%Y-%m')`; glibc does not pad the year.
fn month(at: &DateTime<Utc>) -> String {
    format!("{}-{:02}", at.year(), at.month())
}

/// `strftime('%Y-%m-%d %H%M%S')`.
fn stamp(at: &DateTime<Utc>) -> String {
    format!(
        "{}-{:02}-{:02} {:02}{:02}{:02}",
        at.year(),
        at.month(),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// `strftime('%Y-%m-%d %H:%M:%S')`, as the tables store times.
fn db_time(at: &DateTime<Utc>) -> String {
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}",
        at.year(),
        at.month(),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// `relative_path`: where an item is stored in the collection, grouped by month and named
/// by send time, with the id in brackets.
pub fn relative_path(item: &Dict, ext: &str) -> Result<String, PyError> {
    let when = sent_at_value(self::item(item, "create_time")?)?;
    let msg_id = py_str(self::item(item, "msg_id")?);
    Ok(format!("{}/{} [{msg_id}].{ext}", month(&when), stamp(&when)))
}

/// The name of a message's folder or file: its send time, `title` and id.
fn titled_name(message: &Dict, title: &str) -> Result<(DateTime<Utc>, String), PyError> {
    let when = sent_at_value(item(message, "create_time")?)?;
    let stamp = stamp(&when);
    let title = sanitize(title, 120);
    let msg_id = format!("[{}]", py_str(item(message, "msg_id")?));
    let name = [stamp.as_str(), title.as_str(), msg_id.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Ok((when, name))
}

/// `article_directory`: the folder an article's files go in.
pub fn article_directory(message: &Dict, title: &str) -> Result<String, PyError> {
    let (when, name) = titled_name(message, title)?;
    Ok(format!("{}/{name}", month(&when)))
}

/// `video_path`: where a 视频号 video is stored, named like an article's folder.
pub fn video_path(message: &Dict, video: &ChannelVideo) -> Result<String, PyError> {
    let (when, name) = titled_name(message, &video_title(video))?;
    Ok(format!("{}/{name}.mp4", month(&when)))
}

/// `open_target`: the message the `FavInbox` service has WeChat tap open, with what its
/// bubble may show.
pub fn open_target(message: &Dict, texts: &[&str]) -> Result<Dict, PyError> {
    let mut target = Dict::new();
    target.insert("talker", Value::Str(str_or_empty(message.get("talker"))));
    target.insert("msg_id", item_of(message, "msg_id")?);
    target.insert("create_time", item_of(message, "create_time")?);
    let mut kept: Vec<Value> = Vec::new();
    for text in texts {
        let text = Value::from(*text);
        if !text.as_str().unwrap_or_default().is_empty() && !kept.contains(&text) {
            kept.push(text);
        }
    }
    target.insert("texts", Value::List(kept));
    Ok(target)
}

/// `post_video_name`: `video`, or `video-01` and on when a post has several.
pub fn post_video_name(index: usize, count: usize) -> String {
    if count == 1 {
        "video".to_owned()
    } else {
        format!("video-{index:02}")
    }
}

/// `await asyncio.to_thread(function, path)`: the `stat` loops of picking an unused name
/// run off the async threads.
async fn off_thread(function: fn(&Path) -> PathBuf, path: PathBuf) -> PathBuf {
    tokio::task::spawn_blocking(move || function(&path))
        .await
        .unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()))
}

/// `_unused_directory`.
fn unused_directory(path: &Path) -> PathBuf {
    let name = file_name(path);
    let mut candidate = path.to_path_buf();
    let mut counter = 1;
    while candidate.exists() {
        candidate = with_name(path, &format!("{name} ({counter})"));
        counter += 1;
    }
    candidate
}

/// `_error_detail`: the service's own `{"error": ...}` text, which says which side is down.
fn error_detail(body: &[u8]) -> String {
    match json_from_bytes(body) {
        Ok(Value::Dict(payload)) => match payload.get("error") {
            Some(error) if error.is_truthy() => format!(": {}", py_str(error)),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn dict(pairs: Vec<(&str, Value)>) -> Dict {
    let mut dict = Dict::new();
    for (key, value) in pairs {
        dict.insert(key, value);
    }
    dict
}

fn py_isoformat(at: &DateTime<Utc>) -> String {
    pyjson::py_isoformat_datetime(&at.with_timezone(&FixedOffset::east_opt(0).expect("UTC")))
}

// ---------- the clients ----------

/// The `FavInbox` clients: Python's one client with a per-request timeout for opening a
/// link and an image post is three here (D138). `trust_env` is off: the service is an
/// in-cluster address a deployment-wide proxy would otherwise swallow.
#[derive(Debug, Clone)] // `HttpClient`'s `Debug` names its headers, never their values.
pub struct Clients {
    inbox: HttpClient,
    link_open: HttpClient,
    post_page: HttpClient,
}

impl Clients {
    /// The clients for `token`, through `proxy` (a fake web in tests) and trusting only
    /// `roots` when any are given.
    /// A token that is not ASCII is refused as httpx refuses it (D140); reqwest would send
    /// its UTF-8 bytes.
    pub fn new(token: &str, proxy: Option<&str>, roots: &[Vec<u8>]) -> Result<Self, BuildError> {
        if !token.is_ascii() {
            return Err(BuildError::Header("authorization".to_owned()));
        }
        let build = |read: u64| {
            HttpClient::new(&ClientOptions {
                headers: vec![("Authorization".to_owned(), format!("Bearer {token}"))],
                timeout: Duration::from_secs(read),
                connect_timeout: Some(Duration::from_secs(REQUEST_TIMEOUT_SECONDS)),
                proxy: proxy.map(str::to_owned),
                trust_env: false,
                roots: roots.to_vec(),
                ..ClientOptions::default()
            })
        };
        Ok(Self {
            inbox: build(REQUEST_TIMEOUT_SECONDS)?,
            link_open: build(TAP_OPEN_SECONDS + LINK_WAIT_SECONDS + REQUEST_TIMEOUT_SECONDS)?,
            post_page: build(TAP_OPEN_SECONDS + OPEN_WAIT_SECONDS + REQUEST_TIMEOUT_SECONDS)?,
        })
    }
}

/// `_FetchedVideo`: a post's video, downloaded beside the folder it goes in, and its cover.
#[derive(Debug)]
struct FetchedVideo {
    file: PathBuf,
    cover: Option<(Bytes, &'static str)>,
}

/// `_SavedArticle`: where an article was written, with an entry per image and per video.
#[derive(Debug)]
struct SavedArticle {
    local_path: String,
    images: Vec<Dict>,
    videos: Vec<Dict>,
}

/// A queued item, as `_pending_work` reads it.
#[derive(Debug)]
struct QueueRow {
    msg_id: i64,
    create_time: i64,
    work: String,
    message: Dict,
    attempts: i64,
}

/// `row['message'] if isinstance(row['message'], dict) else json.loads(row['message'])`.
fn stored_message(text: &str) -> Result<Dict, Failure> {
    match pyjson::loads(text) {
        Ok(Value::Dict(message)) => Ok(message),
        Ok(Value::Str(inner)) => match pyjson::loads(&inner) {
            Ok(Value::Dict(message)) => Ok(message),
            Ok(other) => Err(PyError::new("TypeError", format!("'{}' object is not subscriptable", type_name(&other))).into()),
            Err(error) => Err(PyError::new("JSONDecodeError", error.python_message(&inner)).into()),
        },
        Ok(other) => Err(PyError::new(
            "TypeError",
            format!("the JSON object must be str, bytes or bytearray, not {}", type_name(&other)),
        )
        .into()),
        Err(error) => Err(Failure::Job(JobError::new("RuntimeError", error.python_message(text)))),
    }
}

// ---------- the source ----------

/// `Wechat`: one run's state.
pub struct Wechat {
    db: Db,
    notifier: SharedNotifier,
    settings: Arc<SettingsStore>,
    cfg: WechatConfig,
    clients: Clients,
    web: PostLinks,
    sender_names: HashMap<String, String>,
}

impl std::fmt::Debug for Wechat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wechat").field("api_url", &self.cfg.api_url).finish_non_exhaustive()
    }
}

impl Wechat {
    pub fn new(
        db: Db,
        notifier: SharedNotifier,
        settings: Arc<SettingsStore>,
        cfg: WechatConfig,
        clients: Clients,
        web: PostLinks,
    ) -> Self {
        Self {
            db,
            notifier,
            settings,
            cfg,
            clients,
            web,
            sender_names: HashMap::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.cfg.api_url)
    }

    fn unauthorized() -> Failure {
        Failure::keyed(
            "FavInbox rejected the token. Copy FAVINBOX_TOKEN from the host into the WeChat settings again.",
            AUTH_KEY,
        )
    }

    fn post_client(&self) -> &PostClient {
        self.web.client()
    }

    // ---------- bridge events ----------

    async fn state(&self, key: &str) -> Result<Option<String>, Failure> {
        let row = sqlx::query("SELECT value FROM wechat_state WHERE key = $1;")
            .bind(key)
            .fetch_optional(self.db.pool())
            .await
            .map_err(sql_error)?;
        Ok(row.map(|row| row.get::<String, _>("value")))
    }

    async fn save_state(&self, key: &str, value: &str) -> Result<(), Failure> {
        sqlx::query("INSERT INTO wechat_state (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value;")
            .bind(key)
            .bind(value)
            .execute(self.db.pool())
            .await
            .map_err(sql_error)?;
        Ok(())
    }

    async fn cursor(&self) -> Result<i128, Failure> {
        match self.state(CURSOR_KEY).await? {
            Some(value) => Ok(py_int(&Value::Str(value))?),
            None => Ok(0),
        }
    }

    async fn save_cursor(&self, seq: i128) -> Result<(), Failure> {
        self.save_state(CURSOR_KEY, &seq.to_string()).await
    }

    /// `_rescan_for_new_work`: read the event log from the start once [`work_for`]
    /// recognises more than it did.
    async fn rescan_for_new_work(&self) -> Result<(), Failure> {
        if self.state(WORK_VERSION_KEY).await?.as_deref() == Some(WORK_VERSION) {
            return Ok(());
        }
        if self.cursor().await? != 0 {
            log().info("WeChat source learned new kinds of messages; reading the bridge event log again from the start");
            self.save_cursor(0).await?;
        }
        self.save_state(WORK_VERSION_KEY, WORK_VERSION).await
    }

    /// `_unsupported_since`: when reporting messages with nothing to save began; now, the
    /// first time.
    async fn unsupported_since(&self) -> Result<i128, Failure> {
        if let Some(value) = self.state(UNSUPPORTED_SINCE_KEY).await?
            && py_isdigit(&value)
        {
            return Ok(py_int(&Value::Str(value))?);
        }
        let since = i128::from(Utc::now().timestamp_millis());
        self.save_state(UNSUPPORTED_SINCE_KEY, &since.to_string()).await?;
        Ok(since)
    }

    async fn fetch_events(&self, after: i128) -> Result<(i128, Vec<Dict>), Failure> {
        let query = urlencode(&[("after", after.to_string()), ("limit", EVENT_PAGE_SIZE.to_string())]);
        let fetched = match Fetched::send(&self.clients.inbox, Request::get(format!("{}?{query}", self.url("/wx/events")))).await {
            Ok(fetched) => fetched,
            Err(error) if is_http_error(&error) => {
                return Err(Failure::keyed(
                    format!("FavInbox at {} is unreachable: {}", self.cfg.api_url, error.message),
                    UNREACHABLE_KEY,
                ));
            }
            Err(error) => return Err(error.into()),
        };
        if fetched.status == 401 {
            return Err(Self::unauthorized());
        }
        if fetched.status != 200 {
            return Err(Failure::keyed(
                format!(
                    "The WeChat bridge answered /wx/events with HTTP {}{}",
                    fetched.status,
                    error_detail(&fetched.body)
                ),
                BRIDGE_KEY,
            ));
        }
        let payload = json_from_bytes(&fetched.body)?;
        let page = payload
            .as_dict()
            .and_then(|payload| match (payload.get("last_seq"), payload.get("events")) {
                (last_seq @ Some(Value::Int(_) | Value::Bool(_)), Some(Value::List(events))) => {
                    Some((last_seq.map_or(0, int_value), events))
                }
                _ => None,
            });
        let Some((last_seq, events)) = page else {
            return Err(Failure::keyed(
                "The WeChat bridge answered /wx/events with something other than an event page",
                BRIDGE_KEY,
            ));
        };
        let events = events
            .iter()
            .filter_map(Value::as_dict)
            .filter(|event| is_int(event.get("seq")))
            .cloned()
            .collect();
        Ok((last_seq, events))
    }

    /// Fail when WeChat itself is gone from behind the bridge. Not in the Python baseline
    /// (closed Python PR #92, D183): the event log is a file the plugin keeps serving after
    /// WeChat logs out, so an empty page alone looks like nobody forwarded anything.
    /// `/wx/status` asks WeChat for the logged-in account, which the plugin cannot answer
    /// without a session.
    async fn check_login(&self) -> Result<(), Failure> {
        let fetched = match Fetched::send(&self.clients.inbox, Request::get(self.url("/wx/status"))).await {
            Ok(fetched) => fetched,
            Err(error) if is_http_error(&error) => {
                return Err(Failure::keyed(
                    format!("FavInbox at {} is unreachable: {}", self.cfg.api_url, error.message),
                    UNREACHABLE_KEY,
                ));
            }
            Err(error) => return Err(error.into()),
        };
        if fetched.status == 401 {
            return Err(Self::unauthorized());
        }
        let detail = if fetched.status == 200 {
            match json_from_bytes(&fetched.body) {
                Ok(Value::Dict(payload)) if payload.get("login_wxid").is_some_and(Value::is_truthy) => return Ok(()),
                Ok(Value::Dict(_)) => "no logged-in account".to_owned(),
                _ => "HTTP 200 with something other than a status".to_owned(),
            }
        } else {
            format!("HTTP {}{}", fetched.status, error_detail(&fetched.body))
        };
        Err(Failure::keyed(
            format!(
                "WeChat behind FavInbox is not logged in or not answering (/wx/status: {detail}); \
                 new messages are not reaching fav until WeChat is logged in again"
            ),
            LOGIN_KEY,
        ))
    }

    /// `_queue`: queue one message; `false` when it was queued before.
    async fn queue(&self, event: &Dict, work: &str) -> Result<bool, Failure> {
        let message = item(event, "data")?.as_dict().cloned().unwrap_or_default();
        let msg_id = bigint(item(&message, "msg_id")?, "msg_id")?;
        let create_time = bigint(item(&message, "create_time")?, "create_time")?;
        let seq = bigint(item(event, "seq")?, "seq")?;
        let kept = dumps(&Value::Dict(queued_message(&message, work)), &DumpOptions::plain())?;
        let row = sqlx::query(
            "INSERT INTO wechat_events (msg_id, create_time, work, seq, message)
             VALUES ($1, $2, $3, $4, $5::jsonb)
             ON CONFLICT (msg_id, create_time) DO NOTHING
             RETURNING msg_id;",
        )
        .bind(msg_id)
        .bind(create_time)
        .bind(work)
        .bind(seq)
        .bind(kept)
        .fetch_optional(self.db.pool())
        .await
        .map_err(sql_error)?;
        Ok(row.is_some())
    }

    /// `_read_events`: queue the new events worth keeping and move the cursor past them.
    async fn read_events(&self) -> Result<usize, Failure> {
        self.rescan_for_new_work().await?;
        let mut cursor = self.cursor().await?;
        let unsupported_since = self.unsupported_since().await?;
        let mut queued = 0;
        loop {
            let (last_seq, events) = self.fetch_events(cursor).await?;
            if last_seq < cursor {
                // The plugin numbers events from its log file, so a lost log starts over
                // at 1. The queue's key keeps a re-read harmless.
                log().warning(format_args!(
                    "The WeChat bridge restarted its event numbering (last seq {last_seq}, cursor {cursor}); reading from the start"
                ));
                cursor = 0;
                self.save_cursor(cursor).await?;
                continue;
            }
            let seq = |event: &Dict| event.get("seq").map_or(0, int_value);
            let Some(first) = events.first() else { return Ok(queued) };
            if seq(first) > cursor + 1 {
                log().warning(format_args!(
                    "WeChat bridge events {}-{} were rotated out before this source read them",
                    cursor + 1,
                    seq(first) - 1
                ));
            }
            for event in &events {
                let work = work_for(&Value::Dict(event.clone()))?;
                if work == Some(WORK_UNSUPPORTED) {
                    let create_time = event
                        .get("data")
                        .and_then(Value::as_dict)
                        .and_then(|message| message.get("create_time"))
                        .map_or(0, int_value);
                    if create_time < unsupported_since {
                        continue;
                    }
                }
                if let Some(work) = work
                    && self.queue(event, work).await?
                {
                    queued += 1;
                }
            }
            cursor = events.iter().map(seq).max().unwrap_or(cursor);
            self.save_cursor(cursor).await?;
            if events.len() < EVENT_PAGE_SIZE || cursor >= last_seq {
                return Ok(queued);
            }
        }
    }

    async fn queue_rows(&self, status: &str) -> Result<Vec<QueueRow>, Failure> {
        let rows = sqlx::query(
            "SELECT msg_id, create_time, work, message::text AS message, attempts FROM wechat_events
             WHERE status = $1 ORDER BY create_time, msg_id;",
        )
        .bind(status)
        .fetch_all(self.db.pool())
        .await
        .map_err(sql_error)?;
        rows.into_iter()
            .map(|row| {
                Ok(QueueRow {
                    msg_id: row.get("msg_id"),
                    create_time: row.get("create_time"),
                    work: row.get("work"),
                    message: stored_message(&row.get::<String, _>("message"))?,
                    attempts: i64::from(row.get::<i32, _>("attempts")),
                })
            })
            .collect()
    }

    /// `_finish_work`: record the outcome of one queued item; `true` when it will not be
    /// tried again. An error that is not `counted` leaves the attempts alone.
    async fn finish_work(&self, row: &QueueRow, error: &str, counted: bool) -> Result<bool, Failure> {
        let attempts = row.attempts + i64::from(!error.is_empty() && counted);
        let status = if error.is_empty() {
            "done"
        } else if attempts >= MAX_ATTEMPTS {
            "failed"
        } else {
            "pending"
        };
        let attempts =
            i32::try_from(attempts).map_err(|_| Failure::Job(JobError::new("NumericValueOutOfRange", "integer out of range")))?;
        sqlx::query(
            "UPDATE wechat_events SET status = $1, attempts = $2, last_error = $3, updated_at = CURRENT_TIMESTAMP
             WHERE msg_id = $4 AND create_time = $5;",
        )
        .bind(status)
        .bind(attempts)
        .bind(head(error, 1000))
        .bind(row.msg_id)
        .bind(row.create_time)
        .execute(self.db.pool())
        .await
        .map_err(sql_error)?;
        Ok(status == "failed")
    }

    /// `_recheck_reported`: queue again each reported message this source has since
    /// learned to save.
    async fn recheck_reported(&self) -> Result<(), Failure> {
        let mut requeued = 0;
        for row in self.queue_rows("reported").await? {
            // The queue keeps only received private messages, so the event is rebuilt as one.
            let mut data = row.message.clone();
            data.insert("chat", Value::from("private"));
            data.insert("is_send", Value::Bool(false));
            let event = dict(vec![("kind", Value::from("message")), ("data", Value::Dict(data))]);
            let Some(work) = work_for(&Value::Dict(event))?.filter(|work| *work != WORK_UNSUPPORTED) else {
                continue;
            };
            sqlx::query(
                "UPDATE wechat_events SET work = $1, status = 'pending', attempts = 0, last_error = '', updated_at = CURRENT_TIMESTAMP
                 WHERE msg_id = $2 AND create_time = $3 AND status = 'reported';",
            )
            .bind(work)
            .bind(row.msg_id)
            .bind(row.create_time)
            .execute(self.db.pool())
            .await
            .map_err(sql_error)?;
            requeued += 1;
        }
        if requeued != 0 {
            log().info(format_args!(
                "WeChat queued {requeued} reported messages again: they can be saved now"
            ));
        }
        Ok(())
    }

    async fn mark_reported(&self, row: &QueueRow) -> Result<(), Failure> {
        sqlx::query("UPDATE wechat_events SET status = 'reported', updated_at = CURRENT_TIMESTAMP WHERE msg_id = $1 AND create_time = $2;")
            .bind(row.msg_id)
            .bind(row.create_time)
            .execute(self.db.pool())
            .await
            .map_err(sql_error)?;
        Ok(())
    }

    fn unsupported_key(message: &Dict) -> Result<String, PyError> {
        Ok(format!(
            "wechat:unsupported:{}:{}",
            py_str(item(message, "msg_id")?),
            py_str(item(message, "create_time")?)
        ))
    }

    /// `_resolve_unsupported`: unpin the report of a message that was saved after all.
    async fn resolve_unsupported(&mut self, message: &Dict, work: &str) -> Result<(), Failure> {
        let sender = self.sender_name(&sender_of(message)).await?;
        let resolution = Self::unsupported_key(message).and_then(|key| {
            Ok((
                key,
                Resolution {
                    kind: KIND_UNSUPPORTED_RESOLVED.to_owned(),
                    source: JOB_KEY.to_owned(),
                    header: HEADER.to_owned(),
                    title: format!("From {sender}"),
                    body: format!("Saved after all as {work} | Message ID {}", py_str(item(message, "msg_id")?)),
                    ..Resolution::default()
                },
            ))
        });
        let outcome = match resolution {
            Ok((key, resolution)) => self.notifier.resolve(key, resolution).await,
            Err(error) => Err(error.message),
        };
        if let Err(error) = outcome {
            log().warning(format_args!(
                "Failed to resolve the unsupported-message report for WeChat message {}: {error}",
                message.get("msg_id").map_or_else(String::new, py_str)
            ));
        }
        Ok(())
    }

    /// One queued item's work; `true` when it was reported rather than saved.
    async fn work_one(&mut self, row: &QueueRow) -> Result<bool, Failure> {
        let message = &row.message;
        match row.work.as_str() {
            WORK_ARTICLE => self.save_article(message, row.attempts + 1 >= MAX_ATTEMPTS).await?,
            WORK_CHANNEL_VIDEO => self.save_channel_video(message).await?,
            WORK_STICKER => self.save_sticker(message).await?,
            WORK_UNSUPPORTED => {
                self.report_unsupported(message).await?;
                self.mark_reported(row).await?;
                return Ok(true);
            }
            _ => self.save_bridge_image(message).await?,
        }
        Ok(false)
    }

    /// `_work_queue`: the items saved, and a line per failure.
    async fn work_queue(&mut self) -> Result<(usize, Vec<String>), Failure> {
        let mut saved = 0;
        let mut failures: Vec<String> = Vec::new();
        self.recheck_reported().await?;
        for row in self.queue_rows("pending").await? {
            match self.work_one(&row).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(failure) if failure.is_caught() => {
                    let error = failure.text_or_class();
                    log().warning(format_args!("Failed to save WeChat {} {}: {error}", row.work, row.msg_id));
                    // A missing NGA login or TikHub key is a setting to fix, not a failed try.
                    let waits = [NGA_LOGIN_KEY, TIKHUB_KEY].contains(&failure.dedupe_key());
                    let gave_up = self.finish_work(&row, &error, !waits).await?;
                    failures.push(format!(
                        "{} {}{}: {}",
                        row.work,
                        row.msg_id,
                        if gave_up { " (gave up)" } else { "" },
                        head(&error, 200)
                    ));
                    continue;
                }
                Err(failure) => return Err(failure),
            }
            self.finish_work(&row, "", true).await?;
            // A row requeued from a report unpins it; for any other row this finds nothing.
            self.resolve_unsupported(&row.message, &row.work).await?;
            saved += 1;
        }
        Ok((saved, failures))
    }

    // ---------- images ----------

    async fn exists(&self, sql: &'static str, msg_id: &Value, create_time: &Value, third: Option<String>) -> Result<bool, Failure> {
        let mut query = sqlx::query(sql)
            .bind(bigint(msg_id, "msg_id")?)
            .bind(bigint(create_time, "create_time")?);
        if let Some(third) = third {
            query = query.bind(third);
        }
        let row = query.fetch_optional(self.db.pool()).await.map_err(sql_error)?;
        Ok(row.is_some())
    }

    async fn already_stored(&self, item: &Dict) -> Result<bool, Failure> {
        self.exists(
            "SELECT 1 FROM wechat_inbox WHERE msg_id = $1 AND create_time = $2;",
            self::item(item, "msg_id")?,
            self::item(item, "create_time")?,
            None,
        )
        .await
    }

    async fn inbox_file(&self, item: &Dict) -> Result<(Bytes, String), Failure> {
        let msg_id = py_str(self::item(item, "msg_id")?);
        let fetched = Fetched::send(&self.clients.inbox, Request::get(self.url(&format!("/file/{msg_id}")))).await?;
        fetched.raise_for_status()?;
        if fetched.body.is_empty() {
            return Err(Failure::inbox(format!("FavInbox returned an empty file for message {msg_id}")));
        }
        let content_type = fetched.header("content-type").unwrap_or_default();
        Ok((fetched.body, content_type))
    }

    /// `_bridge_image`: the original through the bridge, or the inbox's copy when the bridge
    /// cannot get it.
    async fn bridge_image(&self, item: &Dict) -> Result<(Bytes, String), Failure> {
        let query = urlencode(&[
            ("talker", param_text(self::item(item, "talker")?)),
            ("msg_id", param_text(self::item(item, "msg_id")?)),
            ("create_time", param_text(self::item(item, "create_time")?)),
        ]);
        let reason = match Fetched::send(&self.clients.inbox, Request::get(format!("{}?{query}", self.url("/wx/image")))).await {
            Err(error) if is_http_error(&error) => error.message,
            Err(error) => return Err(error.into()),
            Ok(fetched) => match fetched.raise_for_status() {
                Err(error) => error.message,
                Ok(()) if !fetched.body.is_empty() => {
                    let content_type = fetched.header("content-type").unwrap_or_default();
                    return Ok((fetched.body, content_type));
                }
                Ok(()) => "an empty body".to_owned(),
            },
        };
        log().info(format_args!(
            "The WeChat bridge could not return image {} ({reason}); trying the inbox",
            py_str(self::item(item, "msg_id")?)
        ));
        self.inbox_file(item).await
    }

    /// `_store_image`: write one image into the collection; its path relative to it.
    async fn store_image(&self, item: &Dict, content: &[u8], content_type: &str) -> Result<String, Failure> {
        let ext = if item.get("file").is_some_and(Value::is_truthy) {
            file_extension(item, content_type)
        } else {
            sniff_extension(content).map_or_else(|| file_extension(item, content_type), str::to_owned)
        };
        let relative = relative_path(item, &ext)?;
        let destination = off_thread(ensure_unique_path, self.cfg.path.join(&relative)).await;
        if let Some(folder) = destination.parent() {
            tokio::fs::create_dir_all(folder).await.map_err(|error| os_error(&error, folder))?;
        }
        let partial = with_name(&destination, &format!("{}.part", file_name(&destination)));
        tokio::fs::write(&partial, content)
            .await
            .map_err(|error| os_error(&error, &partial))?;
        tokio::fs::rename(&partial, &destination)
            .await
            .map_err(|error| rename_error(&error, &partial, &destination))?;
        Ok(self.relative(&destination))
    }

    /// `destination.relative_to(self.cfg.path)`.
    fn relative(&self, destination: &Path) -> String {
        destination
            .strip_prefix(self.cfg.path.as_path())
            .map_or_else(|_| path_text(destination), path_text)
    }

    async fn record(&self, item: &Dict, local_path: &str, size: usize) -> Result<(), Failure> {
        let msg_id = bigint(self::item(item, "msg_id")?, "msg_id")?;
        let create_time = bigint(self::item(item, "create_time")?, "create_time")?;
        let sent = db_time(&sent_at_value(self::item(item, "create_time")?)?);
        let metadata = dumps(&Value::Dict(item.clone()), &DumpOptions::plain())?;
        sqlx::query(
            "INSERT INTO wechat_inbox (
                 msg_id, create_time, talker, sender, sent_at, md5, size, local_path, metadata
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::jsonb)
             ON CONFLICT (msg_id, create_time) DO NOTHING;",
        )
        .bind(msg_id)
        .bind(create_time)
        .bind(str_or_empty(item.get("talker")))
        .bind(str_or_empty(item.get("sender")))
        .bind(sent)
        .bind(str_or_empty(item.get("md5")))
        .bind(i64::try_from(size).unwrap_or(i64::MAX))
        .bind(local_path)
        .bind(metadata)
        .execute(self.db.pool())
        .await
        .map_err(sql_error)?;
        Ok(())
    }

    /// `_ack`: tell the host it may drop its copy. A failure only delays that to the next
    /// run.
    async fn ack(&self, item: &Dict, missing_ok: bool) -> Result<(), Failure> {
        let msg_id = py_str(self::item(item, "msg_id")?);
        let request = Request::post(self.url(&format!("/ack/{msg_id}")), Vec::new());
        let failed = |message: &str| {
            log().warning(format_args!(
                "Could not acknowledge WeChat message {msg_id}; the next run retries: {message}"
            ));
        };
        match Fetched::send(&self.clients.inbox, request).await {
            Err(error) if is_http_error(&error) => failed(&error.message),
            Err(error) => return Err(error.into()),
            Ok(fetched) => {
                if missing_ok && fetched.status == 404 {
                    return Ok(());
                }
                if let Err(error) = fetched.raise_for_status() {
                    failed(&error.message);
                }
            }
        }
        Ok(())
    }

    /// `_sender_name`: how the account knows a sender, the remark it gave them, else their
    /// nickname, else the wxid.
    async fn sender_name(&mut self, wxid: &str) -> Result<String, Failure> {
        if wxid.is_empty() {
            return Ok("unknown".to_owned());
        }
        if let Some(name) = self.sender_names.get(wxid) {
            return Ok(name.clone());
        }
        let mut name = String::new();
        let query = urlencode(&[("wxid", wxid.to_owned())]);
        match Fetched::send(&self.clients.inbox, Request::get(format!("{}?{query}", self.url("/wx/contact")))).await {
            Err(error) if is_http_error(&error) => log().debug(format_args!("Could not look up WeChat contact {wxid}: {}", error.message)),
            Err(error) => return Err(error.into()),
            Ok(fetched) if fetched.status == 200 => match json_from_bytes(&fetched.body) {
                Ok(Value::Dict(contact)) => {
                    name = ["remark_name", "name", "nick_name"]
                        .into_iter()
                        .find_map(|key| contact.get(key).filter(|value| value.is_truthy()))
                        .map(|value| py_strip(&py_str(value)).to_owned())
                        .unwrap_or_default();
                }
                Ok(_) => {}
                Err(error) => log().debug(format_args!("Could not look up WeChat contact {wxid}: {}", error.message)),
            },
            Ok(_) => {}
        }
        let name = if name.is_empty() { wxid.to_owned() } else { name };
        self.sender_names.insert(wxid.to_owned(), name.clone());
        Ok(name)
    }

    async fn enqueue(&self, notification: NewNotification, what: &str, msg_id: &Value) {
        if let Err(error) = self.notifier.enqueue(notification).await {
            log().warning(format_args!("Failed to enqueue {what} for message {}: {error}", py_str(msg_id)));
        }
    }

    /// `_notify_download`: one message per image, sent as the photo itself.
    async fn notify_download(&mut self, item: &Dict, local_path: &str) -> Result<(), Failure> {
        let saved_path = path_text(&self.cfg.path.join(local_path));
        let sender = self.sender_name(&sender_of(item)).await?;
        let msg_id = self::item(item, "msg_id")?;
        let payload = dict(vec![
            ("msg_id", msg_id.clone()),
            ("sender", Value::Str(str_or_empty(item.get("sender")))),
            ("saved_path", Value::Str(saved_path.clone())),
            ("image_path", Value::Str(saved_path)),
        ]);
        let notification = NewNotification {
            kind: "download_completed".to_owned(),
            source: JOB_KEY.to_owned(),
            header: HEADER.to_owned(),
            title: file_name(Path::new(local_path)),
            body: format!("From {sender} | Message ID {}", py_str(msg_id)),
            payload: Some(payload),
            ..NewNotification::default()
        };
        self.enqueue(notification, "wechat download notification", msg_id).await;
        Ok(())
    }

    /// `_report_unsupported`.
    async fn report_unsupported(&mut self, message: &Dict) -> Result<(), Failure> {
        let reason = unsupported_reason(message)?;
        let sender = self.sender_name(&sender_of(message)).await?;
        let msg_id = item(message, "msg_id")?;
        log().warning(format_args!(
            "WeChat message {} from {sender} has nothing to save: {reason}",
            py_str(msg_id)
        ));
        let notification = Self::unsupported_key(message).map(|dedupe_key| NewNotification {
            kind: KIND_UNSUPPORTED_MESSAGE.to_owned(),
            source: JOB_KEY.to_owned(),
            header: HEADER.to_owned(),
            title: format!("From {sender}"),
            body: format!("{reason} | Message ID {}", py_str(msg_id)),
            payload: Some(dict(vec![
                ("msg_id", msg_id.clone()),
                ("sender", Value::Str(str_or_empty(message.get("sender")))),
                ("reason", Value::Str(reason.clone())),
            ])),
            dedupe_key,
            ..NewNotification::default()
        });
        match notification {
            Ok(notification) => {
                self.enqueue(notification, "the unsupported-message notification for WeChat", msg_id)
                    .await;
            }
            Err(error) => log().warning(format_args!(
                "Failed to enqueue the unsupported-message notification for WeChat message {}: {}",
                py_str(msg_id),
                error.message
            )),
        }
        Ok(())
    }

    async fn keep_image(&mut self, item: &Dict, content: &[u8], content_type: &str) -> Result<(), Failure> {
        let local_path = self.store_image(item, content, content_type).await?;
        self.record(item, &local_path, content.len()).await?;
        log().info(format_args!(
            "Saved WeChat message {} as {local_path}",
            py_str(self::item(item, "msg_id")?)
        ));
        self.notify_download(item, &local_path).await
    }

    async fn save_bridge_image(&mut self, message: &Dict) -> Result<(), Failure> {
        let item = image_item(message)?;
        if !self.already_stored(&item).await? {
            let (content, content_type) = self.bridge_image(&item).await?;
            self.keep_image(&item, &content, &content_type).await?;
        }
        // The plugin saved its own copy when the image arrived, unless it was off.
        self.ack(&item, true).await
    }

    /// `_drain_inbox`.
    async fn drain_inbox(&mut self) -> Result<(), Failure> {
        let fetched = match Fetched::send(&self.clients.inbox, Request::get(self.url("/items"))).await {
            Ok(fetched) => fetched,
            Err(error) if is_http_error(&error) => {
                return Err(Failure::keyed(
                    format!("FavInbox at {} is unreachable: {}", self.cfg.api_url, error.message),
                    UNREACHABLE_KEY,
                ));
            }
            Err(error) => return Err(error.into()),
        };
        if fetched.status == 401 {
            return Err(Self::unauthorized());
        }
        if fetched.status != 200 {
            return Err(Failure::keyed(
                format!("FavInbox answered /items with HTTP {}", fetched.status),
                LIST_KEY,
            ));
        }
        let Value::List(payload) = json_from_bytes(&fetched.body)? else {
            return Err(Failure::keyed("FavInbox /items did not return a list", LIST_KEY));
        };
        let items = pending_images(&payload);
        let mut downloaded = 0;
        let mut failed: Vec<String> = Vec::new();
        for item in &items {
            if self.already_stored(item).await? {
                // Saved through the bridge, or by a run that died before its ack.
                self.ack(item, false).await?;
                continue;
            }
            let kept = match self.inbox_file(item).await {
                Ok((content, content_type)) => self.keep_image(item, &content, &content_type).await,
                Err(failure) => Err(failure),
            };
            match kept {
                Ok(()) => {}
                Err(failure) if failure.is_caught() => {
                    let msg_id = py_str(self::item(item, "msg_id")?);
                    log().warning(format_args!("Failed to download WeChat message {msg_id}: {}", failure.text()));
                    failed.push(msg_id);
                    continue;
                }
                Err(failure) => return Err(failure),
            }
            self.ack(item, false).await?;
            downloaded += 1;
        }
        log().info(format_args!(
            "WeChat inbox: saved {downloaded} new images ({} pending, {} failed)",
            items.len(),
            failed.len()
        ));
        if !failed.is_empty() {
            return Err(Failure::keyed(
                format!("Could not download {} WeChat image(s): {}", failed.len(), failed.join(", ")),
                DOWNLOAD_KEY,
            ));
        }
        Ok(())
    }

    // ---------- stickers ----------

    /// One GET with the posts' client: the body, or `None` with httpx's text when the
    /// request or its status fails.
    async fn web_get(&self, url: &str) -> Result<Result<Fetched, String>, Failure> {
        match Fetched::send(self.post_client().http(), Request::get(url)).await {
            Err(error) if is_http_error(&error) => Ok(Err(error.message)),
            Err(error) => Err(error.into()),
            Ok(fetched) => Ok(match fetched.raise_for_status() {
                Err(error) => Err(error.message),
                Ok(()) => Ok(fetched),
            }),
        }
    }

    /// `_sticker_file`: the sticker as WeChat's CDN serves it, else its encrypted copy
    /// decrypted.
    async fn sticker_file(&self, sticker: &Sticker) -> Result<(Vec<u8>, String), Failure> {
        let mut reasons: Vec<String> = Vec::new();
        if !sticker.cdn_url.is_empty() {
            match self.web_get(&sticker.cdn_url).await? {
                Err(error) => reasons.push(format!("cdnurl: {error}")),
                Ok(fetched) if matches(sticker, &fetched.body) => {
                    let content_type = fetched.header("content-type").unwrap_or_default();
                    return Ok((fetched.body.to_vec(), content_type));
                }
                Ok(_) => reasons.push("cdnurl: the file does not match its md5".to_owned()),
            }
        }
        if !sticker.encrypted_url.is_empty() && !sticker.aes_key.is_empty() {
            match self.web_get(&sticker.encrypted_url).await? {
                Err(error) => reasons.push(format!("encrypturl: {error}")),
                Ok(fetched) => match decrypt_sticker(&fetched.body, &sticker.aes_key) {
                    Err(_) => reasons.push("encrypturl: the key does not decrypt the file".to_owned()),
                    Ok(content) if matches(sticker, &content) => return Ok((content, String::new())),
                    Ok(_) => reasons.push("encrypturl: the file does not match its md5".to_owned()),
                },
            }
        }
        Err(Failure::inbox(format!("Could not download the sticker: {}", reasons.join("; "))))
    }

    async fn save_sticker(&mut self, message: &Dict) -> Result<(), Failure> {
        let Some(sticker) = parse_sticker(&str_or_empty(message.get("content"))) else {
            return Err(Failure::inbox(format!(
                "Message {} names no sticker file",
                py_str(item(message, "msg_id")?)
            )));
        };
        let mut item = image_item(message)?;
        item.insert("type", Value::from("sticker"));
        item.insert("md5", Value::Str(sticker.md5.clone()));
        if self.already_stored(&item).await? {
            return Ok(());
        }
        let (content, content_type) = self.sticker_file(&sticker).await?;
        self.keep_image(&item, &content, &content_type).await
    }

    // ---------- articles ----------

    /// `_article_stored`: whether this message, or an earlier forward of the same article,
    /// was saved already.
    async fn article_stored(&self, message: &Dict, link: &ArticleLink) -> Result<bool, Failure> {
        let msg_id = item(message, "msg_id")?;
        let create_time = item(message, "create_time")?;
        let url = canonical_url(&link.url)?;
        self.exists(
            "SELECT 1 FROM wechat_article WHERE (msg_id = $1 AND create_time = $2) OR url = $3;",
            msg_id,
            create_time,
            Some(url),
        )
        .await
    }

    /// `_write_article`: the images, the videos, the page and a summary into the article's
    /// folder, all or nothing.
    async fn write_article(
        &self,
        message: &Dict,
        link: &ArticleLink,
        page: &ArticlePage,
        page_html: &str,
        images: &[Option<(Bytes, &'static str)>],
        videos: &[Option<FetchedVideo>],
    ) -> Result<SavedArticle, Failure> {
        let title = if page.title.is_empty() { &link.title } else { &page.title };
        let destination = off_thread(unused_directory, self.cfg.path.join(article_directory(message, title)?)).await;
        let partial = with_name(&destination, &format!("{}.part", file_name(&destination)));
        if tokio::fs::try_exists(&partial).await.unwrap_or(false) {
            tokio::fs::remove_dir_all(&partial)
                .await
                .map_err(|error| os_error(&error, &partial))?;
        }
        tokio::fs::create_dir_all(&partial)
            .await
            .map_err(|error| os_error(&error, &partial))?;
        let width = 2.max(images.len().to_string().len());
        let mut entries: Vec<Dict> = Vec::new();
        for (index, (source, fetched)) in page.images.iter().zip(images).enumerate() {
            let mut entry = dict(vec![("url", Value::from(source.as_str()))]);
            if let Some((content, ext)) = fetched {
                let file = format!("{:0width$}.{ext}", index + 1);
                let path = partial.join(&file);
                tokio::fs::write(&path, content).await.map_err(|error| os_error(&error, &path))?;
                entry.insert("file", Value::Str(file));
            }
            entries.push(entry);
        }
        let mut video_entries: Vec<Dict> = Vec::new();
        for (index, (video, fetched)) in page.videos.iter().zip(videos).enumerate() {
            let mut entry = dict(vec![("url", Value::from(video.url.as_str()))]);
            if let Some(fetched) = fetched {
                let name = post_video_name(index + 1, videos.len());
                let file = format!("{name}.mp4");
                let path = partial.join(&file);
                tokio::fs::rename(&fetched.file, &path)
                    .await
                    .map_err(|error| rename_error(&error, &fetched.file, &path))?;
                entry.insert("file", Value::Str(file));
                if let Some((content, ext)) = &fetched.cover {
                    let cover = format!("{name}.{ext}");
                    let path = partial.join(&cover);
                    tokio::fs::write(&path, content).await.map_err(|error| os_error(&error, &path))?;
                    entry.insert("cover", Value::Str(cover));
                }
            }
            video_entries.push(entry);
        }
        if !page_html.is_empty() {
            let path = partial.join("article.html");
            tokio::fs::write(&path, page_html).await.map_err(|error| os_error(&error, &path))?;
        }
        let mut summary = dict(vec![
            ("title", Value::from(title.as_str())),
            (
                "account",
                Value::from(if page.account.is_empty() {
                    link.account.as_str()
                } else {
                    page.account.as_str()
                }),
            ),
            (
                "published_at",
                page.published_at.map_or(Value::Null, |at| Value::Str(py_isoformat(&at))),
            ),
            ("url", Value::Str(canonical_url(&link.url)?)),
            ("share_url", Value::from(link.url.as_str())),
            ("description", Value::from(link.description.as_str())),
            ("msg_id", item_of(message, "msg_id")?),
            ("create_time", item_of(message, "create_time")?),
            ("sender", sender_value(message).cloned().unwrap_or_else(|| Value::from(""))),
            (
                "picture_count",
                Value::Int(page.picture_count.max(i128::try_from(entries.len()).unwrap_or(i128::MAX))),
            ),
            ("images", Value::List(entries.iter().cloned().map(Value::Dict).collect())),
        ]);
        if !video_entries.is_empty() {
            summary.insert("videos", Value::List(video_entries.iter().cloned().map(Value::Dict).collect()));
        }
        let path = partial.join("article.json");
        let text = dumps(&Value::Dict(summary), &DumpOptions::pretty_unsorted())?;
        tokio::fs::write(&path, text).await.map_err(|error| os_error(&error, &path))?;
        tokio::fs::rename(&partial, &destination)
            .await
            .map_err(|error| rename_error(&error, &partial, &destination))?;
        Ok(SavedArticle {
            local_path: self.relative(&destination),
            images: entries,
            videos: video_entries,
        })
    }

    /// The JSON body httpx sends for `json=`.
    fn json_request(client_url: String, body: &Dict) -> Result<Request, Failure> {
        let body = dumps(&Value::Dict(body.clone()), &DumpOptions::compact())?;
        Ok(Request::post(client_url, body.into_bytes()).header("Content-Type", "application/json"))
    }

    /// `_open_link`: open the link in the agent's WeChat, let it load and close it again,
    /// as its reader would.
    async fn open_link(&self, message: &Dict, link: &ArticleLink) -> Result<(), Failure> {
        let body = dict(vec![
            ("url", Value::from(link.url.as_str())),
            ("message", Value::Dict(open_target(message, &[link.title.as_str()])?)),
            ("wait", Value::Int(i128::from(LINK_WAIT_SECONDS))),
        ]);
        let request = Self::json_request(self.url("/link/open"), &body)?;
        let failure = "FavInbox could not open the link in WeChat";
        let fetched = match Fetched::send(&self.clients.link_open, request).await {
            Ok(fetched) => fetched,
            Err(error) if is_http_error(&error) => return Err(Failure::inbox(format!("{failure}: {}", error.message))),
            Err(error) => return Err(error.into()),
        };
        if fetched.status == 401 {
            return Err(Self::unauthorized());
        }
        if fetched.status != 200 {
            return Err(Failure::inbox(format!(
                "{failure}: HTTP {}{}",
                fetched.status,
                error_detail(&fetched.body)
            )));
        }
        let loaded =
            matches!(json_from_bytes(&fetched.body), Ok(Value::Dict(answer)) if answer.get("loaded").is_some_and(Value::is_truthy));
        if !loaded {
            // Only a missing cache entry, not a failure: some pages are never cached.
            log().info(format_args!(
                "WeChat opened {} but did not report it loaded within {LINK_WAIT_SECONDS}s",
                canonical_url(&link.url)?
            ));
        }
        Ok(())
    }

    /// `_page_in_wechat`: an image post as the agent's WeChat loads it, through the service.
    async fn page_in_wechat(&self, message: &Dict, link: &ArticleLink) -> Result<(ArticlePage, String), Failure> {
        let body = dict(vec![
            ("url", Value::from(link.url.as_str())),
            ("message", Value::Dict(open_target(message, &[link.title.as_str()])?)),
            ("wait", Value::Int(i128::from(OPEN_WAIT_SECONDS))),
        ]);
        let request = Self::json_request(self.url("/post/page"), &body)?;
        let failure = "FavInbox could not open the image post in WeChat";
        let fetched = match Fetched::send(&self.clients.post_page, request).await {
            Ok(fetched) => fetched,
            Err(error) if is_http_error(&error) => return Err(Failure::inbox(format!("{failure}: {}", error.message))),
            Err(error) => return Err(error.into()),
        };
        if fetched.status == 401 {
            return Err(Self::unauthorized());
        }
        if fetched.status != 200 {
            return Err(Failure::inbox(format!(
                "{failure}: HTTP {}{}",
                fetched.status,
                error_detail(&fetched.body)
            )));
        }
        // `response.text`, read as UTF-8 whatever the page declares (D141, as D128).
        let text = String::from_utf8_lossy(&fetched.body).into_owned();
        let mut page = parse_article_page(&text)?;
        if page.images.is_empty() {
            return Err(Failure::inbox("the image post WeChat opened lists no pictures"));
        }
        page.picture_count = link.picture_count.max(i128::try_from(page.images.len()).unwrap_or(i128::MAX));
        Ok((page, text))
    }

    /// `_image_post`: an image post the site put behind its captcha, all of it from WeChat,
    /// else its first picture.
    async fn image_post(&self, message: &Dict, link: &ArticleLink, final_attempt: bool) -> Result<(ArticlePage, String), Failure> {
        let Some(cover) = cover_only_page(link) else {
            return Err(Failure::inbox(format!(
                "mp.weixin.qq.com answered {} with a captcha page",
                canonical_url(&link.url)?
            )));
        };
        match self.page_in_wechat(message, link).await {
            Ok(page) => Ok(page),
            // Retried while attempts remain; the first picture beats nothing at all.
            Err(failure @ Failure::Inbox { key, .. }) if final_attempt && key != AUTH_KEY => {
                log().warning(format_args!(
                    "WeChat image post {}: {}; saving its first picture only",
                    canonical_url(&link.url)?,
                    failure.text()
                ));
                Ok((cover, String::new()))
            }
            Err(failure) => Err(failure),
        }
    }

    /// `_article_page`: the shared page and its HTML, if any was kept.
    async fn article_page(&self, message: &Dict, link: &ArticleLink, final_attempt: bool) -> Result<(ArticlePage, String), Failure> {
        match self.web.fetch_page(link).await {
            Ok(page) => Ok(page),
            Err(PostError::Link(error)) if error.captcha => self.image_post(message, link, final_attempt).await,
            Err(PostError::Link(error)) => {
                let key = match error.needs {
                    Needs::NgaLogin => NGA_LOGIN_KEY,
                    Needs::TikHub => TIKHUB_KEY,
                    Needs::Nothing => "",
                };
                Err(Failure::keyed(error.message, key))
            }
            Err(PostError::Other(error)) => Err(error.into()),
        }
    }

    async fn save_article(&mut self, message: &Dict, final_attempt: bool) -> Result<(), Failure> {
        let content = str_or_empty(message.get("content"));
        let Some(link) = shared_link(&content)? else {
            return Err(Failure::inbox(format!(
                "message {} is not an article share",
                py_str(item(message, "msg_id")?)
            )));
        };
        if self.article_stored(message, &link).await? {
            return Ok(());
        }
        self.open_link(message, &link).await?;
        let (page, page_html) = self.article_page(message, &link, final_attempt).await?;
        let canonical = canonical_url(&link.url)?;
        if page.title.is_empty() && page.images.is_empty() && page.videos.is_empty() {
            return Err(Failure::inbox(format!(
                "{canonical} has no title, images or videos; the article may have been deleted"
            )));
        }
        let mut images = Vec::with_capacity(page.images.len());
        for url in &page.images {
            images.push(self.post_client().fetch_image(url).await?);
        }
        let saved = images.iter().filter(|image| image.is_some()).count();
        if !page.images.is_empty() && saved == 0 {
            return Err(Failure::inbox(format!(
                "none of the {} images of {canonical} could be downloaded",
                page.images.len()
            )));
        }
        let title = if page.title.is_empty() { &link.title } else { &page.title };
        let staging = self.cfg.path.join(article_directory(message, title)?);
        let mut videos: Vec<Option<FetchedVideo>> = Vec::new();
        let written = self
            .download_and_write(message, &link, &page, &page_html, &images, &staging, &mut videos)
            .await;
        // Whatever the folder did not take in.
        for fetched in videos.iter().flatten() {
            remove_file(&fetched.file).await?;
        }
        let article = written?;
        self.record_article(message, &link, &page, &article).await?;
        let mut counts = format!("{saved} of {} images", images.len());
        if !videos.is_empty() {
            counts = format!(
                "{counts}, {} of {} videos",
                videos.iter().filter(|video| video.is_some()).count(),
                videos.len()
            );
        }
        log().info(format_args!(
            "Saved WeChat article {} ({counts}) as {}",
            py_str(item(message, "msg_id")?),
            article.local_path
        ));
        self.notify_article(message, &link, &page, &article).await
    }

    /// The `try` of `_save_article`: the videos, then the folder.
    #[allow(clippy::too_many_arguments)] // reason: the locals of Python's `_save_article`
    async fn download_and_write(
        &self,
        message: &Dict,
        link: &ArticleLink,
        page: &ArticlePage,
        page_html: &str,
        images: &[Option<(Bytes, &'static str)>],
        staging: &Path,
        videos: &mut Vec<Option<FetchedVideo>>,
    ) -> Result<SavedArticle, Failure> {
        let staging_name = file_name(staging);
        for (index, video) in page.videos.iter().enumerate() {
            let partial = with_name(staging, &format!("{staging_name}.video-{}.part", index + 1));
            videos.push(self.post_video(video, &partial).await?);
        }
        if !page.videos.is_empty() && videos.iter().all(Option::is_none) {
            return Err(Failure::inbox(format!(
                "none of the {} videos of {} could be downloaded",
                page.videos.len(),
                canonical_url(&link.url)?
            )));
        }
        self.write_article(message, link, page, page_html, images, videos).await
    }

    /// `_post_video`: one of a post's videos in `partial`, with its cover; `None` when it
    /// does not load as an MP4.
    async fn post_video(&self, video: &PostVideo, partial: &Path) -> Result<Option<FetchedVideo>, Failure> {
        if !self.post_client().fetch_video(video, partial).await? {
            return Ok(None);
        }
        let cover = if video.cover_url.is_empty() {
            None
        } else {
            self.post_client().fetch_image(&video.cover_url).await?
        };
        Ok(Some(FetchedVideo {
            file: partial.to_path_buf(),
            cover,
        }))
    }

    async fn record_article(&self, message: &Dict, link: &ArticleLink, page: &ArticlePage, article: &SavedArticle) -> Result<(), Failure> {
        let mut metadata = dict(vec![
            ("share_url", Value::from(link.url.as_str())),
            ("description", Value::from(link.description.as_str())),
            (
                "picture_count",
                Value::Int(page.picture_count.max(i128::try_from(article.images.len()).unwrap_or(i128::MAX))),
            ),
            ("images", Value::List(article.images.iter().cloned().map(Value::Dict).collect())),
        ]);
        if !article.videos.is_empty() {
            metadata.insert("videos", Value::List(article.videos.iter().cloned().map(Value::Dict).collect()));
        }
        let msg_id = bigint(item(message, "msg_id")?, "msg_id")?;
        let create_time = bigint(item(message, "create_time")?, "create_time")?;
        let sent = db_time(&sent_at_value(item(message, "create_time")?)?);
        let image_count = article.images.iter().filter(|entry| entry.contains_key("file")).count();
        sqlx::query(
            "INSERT INTO wechat_article (
                 msg_id, create_time, talker, sender, sent_at, url, title, account, published_at, image_count, local_path, metadata
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12::jsonb)
             ON CONFLICT (msg_id, create_time) DO NOTHING;",
        )
        .bind(msg_id)
        .bind(create_time)
        .bind(str_or_empty(message.get("talker")))
        .bind(sender_of(message))
        .bind(sent)
        .bind(canonical_url(&link.url)?)
        .bind(if page.title.is_empty() { &link.title } else { &page.title })
        .bind(if page.account.is_empty() { &link.account } else { &page.account })
        .bind(page.published_at.map_or_else(String::new, |at| db_time(&at)))
        .bind(i32::try_from(image_count).unwrap_or(i32::MAX))
        .bind(&article.local_path)
        .bind(dumps(&Value::Dict(metadata), &DumpOptions::plain())?)
        .execute(self.db.pool())
        .await
        .map_err(sql_error)?;
        Ok(())
    }

    /// `_notify_article`: one message per article, linking to it and showing its first
    /// image, else its first video's cover.
    async fn notify_article(
        &mut self,
        message: &Dict,
        link: &ArticleLink,
        page: &ArticlePage,
        article: &SavedArticle,
    ) -> Result<(), Failure> {
        let files: Vec<String> = article.images.iter().filter_map(|entry| entry.get("file").map(py_str)).collect();
        let folder = self.cfg.path.join(&article.local_path);
        let mut counts: Vec<String> = Vec::new();
        let entries = i128::try_from(article.images.len()).unwrap_or(i128::MAX);
        let total = page.picture_count.max(entries);
        if total != 0 || article.videos.is_empty() {
            let noun = if total == 1 { "image" } else { "images" };
            let mut count = if i128::try_from(files.len()).unwrap_or(i128::MAX) == total {
                format!("{} {noun}", files.len())
            } else {
                format!("{} of {total} {noun}", files.len())
            };
            if page.picture_count > entries {
                count.push_str(" (WeChat could not open the rest)");
            }
            counts.push(count);
        }
        if !article.videos.is_empty() {
            let videos = article.videos.iter().filter(|entry| entry.contains_key("file")).count();
            let noun = if article.videos.len() == 1 { "video" } else { "videos" };
            counts.push(if videos == article.videos.len() {
                format!("{videos} {noun}")
            } else {
                format!("{videos} of {} {noun}", article.videos.len())
            });
        }
        let sender = sender_of(message);
        let msg_id = item(message, "msg_id")?;
        let mut payload = dict(vec![
            ("msg_id", msg_id.clone()),
            ("sender", Value::Str(sender.clone())),
            ("saved_path", Value::Str(path_text(&folder))),
        ]);
        let name = self.sender_name(&sender).await?;
        let covers: Vec<String> = article.videos.iter().filter_map(|entry| entry.get("cover").map(py_str)).collect();
        if let Some(first) = files.first().or_else(|| covers.first()) {
            payload.insert("image_path", Value::Str(path_text(&folder.join(first))));
        }
        let account = if page.account.is_empty() { &link.account } else { &page.account };
        let from = format!("From {name}");
        let body = std::iter::once(account.as_str())
            .chain(counts.iter().map(String::as_str))
            .chain(std::iter::once(from.as_str()))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" | ");
        let title = [page.title.as_str(), link.title.as_str()]
            .into_iter()
            .find(|title| !title.is_empty())
            .map_or_else(|| file_name(Path::new(&article.local_path)), str::to_owned);
        let notification = NewNotification {
            kind: "download_completed".to_owned(),
            source: JOB_KEY.to_owned(),
            header: HEADER.to_owned(),
            title,
            link_url: canonical_url(&link.url)?,
            body,
            payload: Some(payload),
            ..NewNotification::default()
        };
        self.enqueue(notification, "wechat article notification", msg_id).await;
        Ok(())
    }

    // ---------- 视频号 videos ----------

    async fn video_stored(&self, message: &Dict, video: &ChannelVideo) -> Result<bool, Failure> {
        self.exists(
            "SELECT 1 FROM wechat_channel_video WHERE (msg_id = $1 AND create_time = $2) OR object_id = $3;",
            item(message, "msg_id")?,
            item(message, "create_time")?,
            Some(video.object_id.clone()),
        )
        .await
    }

    /// `_video_source`: where the video file is and how to decrypt it, as TikHub looks it up.
    async fn video_source(&self, video: &ChannelVideo) -> Result<VideoSource, Failure> {
        let settings = self
            .settings
            .load()
            .await
            .map_err(|error| Failure::Job(crate::settings_job_error(&error)))?;
        let tikhub = &settings.services.tikhub;
        if tikhub.token.is_empty() {
            return Err(Failure::keyed("视频号 videos need a TikHub API key (services.tikhub)", TIKHUB_KEY));
        }
        let failure = format!("TikHub could not look up 视频号 video {}", video.object_id);
        let authorization = format!("Bearer {}", tikhub.token);
        if let Some(error) = ascii_encode_error(&authorization) {
            // httpx encodes header values as ASCII before sending: not an `HTTPError`.
            return Err(Failure::Py(error));
        }
        let request = Self::json_request(format!("{}{TIKHUB_VIDEO_DETAIL}", tikhub.api_url), &tikhub_lookup(video))?
            .header("Authorization", authorization);
        let fetched = match Fetched::send(self.post_client().http(), request).await {
            Ok(fetched) => fetched,
            Err(error) if is_http_error(&error) => return Err(Failure::inbox(format!("{failure}: {}", error.message))),
            Err(error) => return Err(error.into()),
        };
        if TIKHUB_ACCOUNT_STATUSES.contains(&fetched.status) {
            return Err(Failure::keyed(
                format!(
                    "{failure}: HTTP {}{}; check the TikHub key and balance",
                    fetched.status,
                    error_detail(&fetched.body)
                ),
                TIKHUB_KEY,
            ));
        }
        if fetched.status != 200 {
            return Err(Failure::inbox(format!(
                "{failure}: HTTP {}{}",
                fetched.status,
                error_detail(&fetched.body)
            )));
        }
        let source = match json_from_bytes(&fetched.body).and_then(|payload| parse_video_source(&payload)) {
            Ok(source) => source,
            Err(error) if is_value_error(&error) => None,
            Err(error) => return Err(error.into()),
        };
        source.ok_or_else(|| Failure::inbox(format!("{failure}: the answer names no video file and decode key")))
    }

    /// `_download_video`: download the video into `partial`, decrypt its start, and return
    /// its size.
    async fn download_video(&self, video: &ChannelVideo, partial: &Path) -> Result<u64, Failure> {
        let source = self.video_source(video).await?;
        let failure = format!("Could not download 视频号 video {}", video.object_id);
        let mut response = match self.post_client().video().send(Request::get(source.url.clone())).await {
            Ok(response) => response,
            Err(error) => {
                let error = transport_error(&error);
                if !is_http_error(&error) {
                    return Err(error.into());
                }
                return Err(cut_short(partial, &failure, &error.message).await);
            }
        };
        if let Err(error) = response.error_for_status() {
            return Err(cut_short(partial, &failure, &error.to_string()).await);
        }
        if let Some(folder) = partial.parent() {
            tokio::fs::create_dir_all(folder).await.map_err(|error| os_error(&error, folder))?;
        }
        let mut file = tokio::fs::File::create(partial).await.map_err(|error| os_error(&error, partial))?;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => file.write_all(&chunk).await.map_err(|error| io_error(&error))?,
                Ok(None) => break,
                Err(error) => {
                    drop(file);
                    return Err(cut_short(partial, &failure, &error.message).await);
                }
            }
        }
        file.flush().await.map_err(|error| io_error(&error))?;
        drop(file);
        decrypt_file_head(partial, source.decode_key).await?;
        if !is_mp4(&file_head(partial).await?) {
            remove_file(partial).await?;
            return Err(Failure::inbox(format!("{failure}: it is not an MP4 once decrypted")));
        }
        let metadata = tokio::fs::metadata(partial).await.map_err(|error| os_error(&error, partial))?;
        Ok(metadata.len())
    }

    /// `_video_cover`: the cover as an image file, if it loads.
    async fn video_cover(&self, video: &ChannelVideo) -> Result<Option<(Bytes, &'static str)>, Failure> {
        if video.cover_url.is_empty() {
            return Ok(None);
        }
        let fetched = match self.web_get(&video.cover_url).await? {
            Ok(fetched) => fetched,
            Err(error) => {
                log().info(format_args!("视频号 cover of {} failed: {error}", video.object_id));
                return Ok(None);
            }
        };
        Ok(match sniff_extension(&fetched.body) {
            Some(ext) if !fetched.body.is_empty() => Some((fetched.body, ext)),
            _ => None,
        })
    }

    async fn save_channel_video(&mut self, message: &Dict) -> Result<(), Failure> {
        let Some(video) = parse_channel_video(&str_or_empty(message.get("content")))? else {
            return Err(Failure::inbox(format!(
                "message {} is not a 视频号 video share",
                py_str(item(message, "msg_id")?)
            )));
        };
        if self.video_stored(message, &video).await? {
            return Ok(());
        }
        let destination = off_thread(ensure_unique_path, self.cfg.path.join(video_path(message, &video)?)).await;
        let partial = with_name(&destination, &format!("{}.part", file_name(&destination)));
        let size = self.download_video(&video, &partial).await?;
        tokio::fs::rename(&partial, &destination)
            .await
            .map_err(|error| rename_error(&error, &partial, &destination))?;
        let local_path = self.relative(&destination);
        let mut cover_path = None;
        if let Some((content, ext)) = self.video_cover(&video).await? {
            let path = destination.with_extension(ext);
            tokio::fs::write(&path, &content).await.map_err(|error| os_error(&error, &path))?;
            cover_path = Some(path);
        }
        self.record_video(message, &video, &local_path, size).await?;
        log().info(format_args!(
            "Saved WeChat 视频号 video {} ({}, {size} bytes) as {local_path}",
            py_str(item(message, "msg_id")?),
            video.object_id
        ));
        self.notify_video(message, &video, &destination, size, cover_path.as_deref()).await
    }

    async fn record_video(&self, message: &Dict, video: &ChannelVideo, local_path: &str, size: u64) -> Result<(), Failure> {
        let msg_id = bigint(item(message, "msg_id")?, "msg_id")?;
        let create_time = bigint(item(message, "create_time")?, "create_time")?;
        let sent = db_time(&sent_at_value(item(message, "create_time")?)?);
        let duration =
            i32::try_from(video.duration).map_err(|_| Failure::Job(JobError::new("NumericValueOutOfRange", "integer out of range")))?;
        let metadata = dict(vec![
            ("nonce_id", Value::from(video.nonce_id.as_str())),
            ("description", Value::from(video.description.as_str())),
            ("width", Value::Int(video.width)),
            ("height", Value::Int(video.height)),
        ]);
        sqlx::query(
            "INSERT INTO wechat_channel_video (
                 msg_id, create_time, talker, sender, sent_at, object_id, title, author, duration, size, local_path, metadata
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12::jsonb)
             ON CONFLICT (msg_id, create_time) DO NOTHING;",
        )
        .bind(msg_id)
        .bind(create_time)
        .bind(str_or_empty(message.get("talker")))
        .bind(sender_of(message))
        .bind(sent)
        .bind(&video.object_id)
        .bind(video_title(video))
        .bind(&video.author)
        .bind(duration)
        .bind(i64::try_from(size).unwrap_or(i64::MAX))
        .bind(local_path)
        .bind(dumps(&Value::Dict(metadata), &DumpOptions::plain())?)
        .execute(self.db.pool())
        .await
        .map_err(sql_error)?;
        Ok(())
    }

    /// `_notify_video`: one message per video, showing its cover.
    async fn notify_video(
        &mut self,
        message: &Dict,
        video: &ChannelVideo,
        saved: &Path,
        size: u64,
        cover: Option<&Path>,
    ) -> Result<(), Failure> {
        let sender = sender_of(message);
        let msg_id = item(message, "msg_id")?;
        let mut payload = dict(vec![
            ("msg_id", msg_id.clone()),
            ("sender", Value::Str(sender.clone())),
            ("saved_path", Value::Str(path_text(saved))),
        ]);
        if let Some(cover) = cover {
            payload.insert("image_path", Value::Str(path_text(cover)));
        }
        #[allow(clippy::cast_precision_loss)] // reason: `size / 1_000_000` is a float division in Python
        let megabytes = format!("{:.1} MB", size as f64 / 1_000_000.0);
        let duration = if video.duration == 0 {
            String::new()
        } else {
            format!("{}s", video.duration)
        };
        let from = format!("From {}", self.sender_name(&sender).await?);
        let body = [video.author.as_str(), duration.as_str(), megabytes.as_str(), from.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" | ");
        let notification = NewNotification {
            kind: "download_completed".to_owned(),
            source: JOB_KEY.to_owned(),
            header: HEADER.to_owned(),
            title: video_title(video),
            body,
            payload: Some(payload),
            ..NewNotification::default()
        };
        self.enqueue(notification, "wechat video notification", msg_id).await;
        Ok(())
    }

    // ---------- run ----------

    /// `update()`: read the bridge's events and check WeChat is logged in (D183), work the
    /// queue, then drain the inbox. Each step runs even when the one before it failed; the
    /// failures are reported together.
    pub async fn update(&mut self) -> Result<(), JobError> {
        let missing = self.cfg.validate_runnable();
        if !missing.is_empty() {
            log().warning(format_args!(
                "WeChat is not configured (missing {}), skip update",
                missing.join(", ")
            ));
            return Ok(());
        }
        let root = self.cfg.path.as_path().to_path_buf();
        tokio::fs::create_dir_all(&root)
            .await
            .map_err(|error| os_error(&error, &root).into_job_error())?;

        let mut problems: Vec<(String, &'static str)> = Vec::new();
        match self.read_events().await {
            Ok(queued) => {
                log().info(format_args!("WeChat bridge: queued {queued} new item(s)"));
                // Only once the bridge answered: a bridge that is down is reported already.
                match self.check_login().await {
                    Ok(()) => {}
                    Err(failure @ Failure::Inbox { .. }) => {
                        log().warning(format_args!("WeChat is not usable behind the bridge: {}", failure.text()));
                        problems.push((failure.text(), failure.dedupe_key()));
                    }
                    Err(failure) => return Err(failure.into_job_error()),
                }
            }
            Err(failure @ Failure::Inbox { .. }) => {
                log().warning(format_args!("Could not read WeChat bridge events: {}", failure.text()));
                problems.push((failure.text(), failure.dedupe_key()));
            }
            Err(failure) => return Err(failure.into_job_error()),
        }

        let (saved, failures) = self.work_queue().await.map_err(Failure::into_job_error)?;
        if saved != 0 || !failures.is_empty() {
            log().info(format_args!("WeChat bridge: saved {saved} item(s), {} failed", failures.len()));
        }
        if !failures.is_empty() {
            problems.push((
                format!("Could not save {} WeChat item(s): {}", failures.len(), failures.join(", ")),
                WORK_KEY,
            ));
        }

        match self.drain_inbox().await {
            Ok(()) => {}
            Err(failure @ Failure::Inbox { .. }) => problems.push((failure.text(), failure.dedupe_key())),
            Err(failure) => return Err(failure.into_job_error()),
        }

        let Some((_, key)) = problems.first() else { return Ok(()) };
        let messages = crate::pyval::unique(problems.iter().map(|(message, _)| message.clone()).collect());
        Err(inbox_job_error(messages.join("; "), key))
    }
}

/// A transfer cut short leaves a partial file nothing else would clean up.
async fn cut_short(partial: &Path, failure: &str, message: &str) -> Failure {
    match remove_file(partial).await {
        Err(error) => error.into(),
        Ok(()) => Failure::inbox(format!("{failure}: {message}")),
    }
}

/// `_decrypt_file_head`: undo WeChat's encryption of the start of a downloaded video, in
/// place.
async fn decrypt_file_head(path: &Path, decode_key: u64) -> Result<(), PyError> {
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .await
        .map_err(|error| os_error(&error, path))?;
    let mut head = Vec::with_capacity(ENCRYPTED_BYTES);
    (&mut file)
        .take(ENCRYPTED_BYTES as u64)
        .read_to_end(&mut head)
        .await
        .map_err(|error| io_error(&error))?;
    decrypt_in_place(&mut head, decode_key);
    file.seek(std::io::SeekFrom::Start(0)).await.map_err(|error| io_error(&error))?;
    file.write_all(&head).await.map_err(|error| io_error(&error))?;
    file.flush().await.map_err(|error| io_error(&error))?;
    Ok(())
}

/// `file_head`: a file's first 16 bytes.
async fn file_head(path: &Path) -> Result<Vec<u8>, PyError> {
    let file = tokio::fs::File::open(path).await.map_err(|error| os_error(&error, path))?;
    let mut head = Vec::with_capacity(16);
    file.take(16).read_to_end(&mut head).await.map_err(|error| io_error(&error))?;
    Ok(head)
}

/// The `wechat` job: each run reads its settings and builds its clients.
struct WechatJob {
    ctx: SourceContext,
}

impl Source for WechatJob {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self.ctx.settings.load().await.map_err(|error| crate::settings_job_error(&error))?;
            let cfg = settings.web.wechat.clone();
            let build_error = |error: BuildError| JobError::new("RuntimeError", error.to_string());
            // An unconfigured source only logs and returns, whatever its token (D140).
            let token = if cfg.validate_runnable().is_empty() {
                cfg.token.as_str()
            } else {
                ""
            };
            let clients = Clients::new(token, None, &[]).map_err(build_error)?;
            let sessions: Arc<dyn SessionStore> = Arc::new(self.ctx.db.clone());
            let web = PostLinks::new(PostClient::new().map_err(build_error)?, Arc::clone(&self.ctx.settings), sessions);
            let notifier: SharedNotifier = Arc::new(OutboxNotifier(Outbox::new(self.ctx.db.clone(), Arc::clone(&self.ctx.settings))));
            let mut source = Wechat::new(self.ctx.db.clone(), notifier, Arc::clone(&self.ctx.settings), cfg, clients, web);
            source.update().await
        })
    }
}

/// Register the `wechat` job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run(JOB_KEY, move || Box::new(WechatJob { ctx: ctx.clone() }) as Box<dyn Source>);
}

#[cfg(test)]
mod tests;
