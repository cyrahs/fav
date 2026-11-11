//! Liked notes on RedNote (小红书, xiaohongshu.com), images and videos.
//!
//! Python origin: `src/web/rednote.py`. The likes list is only readable by the account
//! that owns it, so this source drives the user's own account, and account safety is the
//! constraint everything else bends around: the reading happens in a signed-in Chromium
//! profile ([`fav_browser::rednote::note_browser`]) whose traffic leaves through a
//! residential proxy, and the crawl harvests the site's *own* XHR responses as it scrolls.
//!
//! A run walks the likes list newest first, resolving each page's new notes into one row
//! per file as it goes, and then downloads every row still pending, straight from the CDN
//! with the browser closed and no cookies attached. The database joins the two halves: a
//! run that dies part-way resumes from the rows. `rednote_state` records whether the first
//! full walk ever finished; after it has, runs stop once `abort_after` pages in a row added
//! nothing.
//!
//! Never run against RedNote from tests or development (`docs/rewrite/PLAN.md`): the browser
//! is a scripted fake, or a real Chromium on loopback pages.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fav_browser::proxy::ProxyError;
use fav_browser::rednote::note_browser::{
    CapturedPage, ChromiumNoteBrowser, NoteBrowser, NoteBrowserError, RISK_CONTROL_STATUS_CODES, cursor_of, describe_shape,
};
use fav_browser::rednote::{SESSION_COOKIE, SESSION_SITE, truthy, user_id_from_probe};
use fav_core::filename::{format_media_filename, sanitize};
use fav_core::logging::{self, Logger};
use fav_core::pyjson::{self, Dict, Value};
use fav_core::pytext::{py_int_decimal, py_isdigit, py_strip};
use fav_core::settings::RedNote as RedNoteConfig;
use fav_core::validate::PyPath;
use fav_db::Db;
use fav_net::{ClientOptions, HttpClient, Request};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox};
use fav_runtime::pystr::repr_value;
use fav_runtime::site_sessions;
use fav_runtime::source::{JobError, Registry, Source};
use serde_json::Map;
use sqlx::Row as _;
use tokio::io::AsyncWriteExt as _;

use crate::SourceContext;
use crate::notifier::{OutboxNotifier, SharedNotifier};
use crate::pydecode::PyError;

pub mod proxy_probe;

pub const JOB_KEY: &str = "rednote";
const WEB_ORIGIN: &str = fav_browser::rednote::WEB_ORIGIN;
/// Where a note reached from a likes list is declared to have come from.
const XSEC_SOURCE: &str = "pc_user";
const RELOGIN_HINT: &str = "Sign in to RedNote again on fav's 登录态 page.";
pub const BACKFILL_STATE_KEY: &str = "backfill_complete";
pub const USER_ID_STATE_KEY: &str = "user_id";

pub const LOGGED_IN: &str = "logged_in";
pub const LOGGED_OUT: &str = "logged_out";
pub const LOGIN_UNKNOWN: &str = "unknown";
/// How many separate runs have to find a note gone before it stops being retried.
pub const MISSING_RUNS_BEFORE_RETIRING: i32 = 3;
/// The login state a run looks at: the first, and one more after a reload.
const LOGIN_CHECK_ATTEMPTS: u32 = 2;
/// How long the site gets to produce the next page of the likes list while scrolling.
pub const LIKE_PAGE_TIMEOUT: Duration = Duration::from_secs(25);

const SESSION_EXPIRED_CODES: [i128; 2] = [-100, -101];
/// A rotated CDN URL answers 403, a deleted note's 404 (so does a rotated one): recorded,
/// and settled on the next run with the browser open.
const MEDIA_STALE_STATUS_CODES: [u16; 3] = [403, 404, 410];
const MEDIA_ACCEPT: &str = "image/*,video/*,*/*;q=0.8";
/// Written into `last_error` so the next run can find these rows again.
pub const STALE_URL_MARKER: &str = "stale-url";
/// A file still being written; never a name the archive itself produces.
const PARTIAL_SUFFIX: &str = ".part";

const VIDEO_NOTE_TYPE: &str = "video";
const IMAGE_MEDIA_TYPE: &str = "image";
const LIVE_MEDIA_TYPE: &str = "live";
const VIDEO_MEDIA_TYPE: &str = "video";
/// Both are mp4, and both follow `video_path` when it is set.
const VIDEO_MEDIA_TYPES: [&str; 2] = [LIVE_MEDIA_TYPE, VIDEO_MEDIA_TYPE];

const PREFERRED_IMAGE_SCENE: &str = "WB_DFT";
const IMAGE_EXT_ALIASES: [(&str, &str); 7] = [
    ("jpeg", "jpg"),
    ("jpg", "jpg"),
    ("png", "png"),
    ("gif", "gif"),
    ("webp", "webp"),
    ("avif", "avif"),
    ("heic", "heic"),
];
/// The format as a token in the last path segment (`.../1040g0083!nd_dft_wlteh_webp_3`).
const IMAGE_EXT_TOKENS: [(&str, &str); 7] = [
    ("webp", "webp"),
    ("avif", "avif"),
    ("heic", "heic"),
    ("png", "png"),
    ("jpeg", "jpg"),
    ("jpg", "jpg"),
    ("gif", "gif"),
];
const VIDEO_EXT: &str = "mp4";
/// Note timestamps are epoch milliseconds, shown in the app's timezone (UTC+8).
const DISPLAY_OFFSET_SECONDS: i32 = 8 * 3600;

fn log() -> Logger {
    logging::get("rednote")
}

/// `RedNoteError(message, notification_dedupe_key=key)`: the session, or RedNote refusing
/// the whole crawl.
pub fn rednote_error(message: impl Into<String>, dedupe_key: &str) -> JobError {
    let error = JobError::new("RedNoteError", message);
    if dedupe_key.is_empty() {
        error
    } else {
        error.with_notification_dedupe_key(dedupe_key)
    }
}

fn is_rednote_error(error: &JobError) -> bool {
    error.class_name() == "RedNoteError"
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

fn browser_error(error: &NoteBrowserError) -> JobError {
    JobError::new(error.class_name(), error.to_string())
}

fn py_job_error(error: PyError) -> JobError {
    error.into_job_error()
}

fn os_error(error: &std::io::Error, path: &str) -> JobError {
    JobError::new(
        fav_net::download::io_class_name(error),
        format!(
            "{}: {}",
            fav_net::download::py_os_error_text(error),
            fav_runtime::pystr::repr_str(path)
        ),
    )
}

// ---------- parsing ----------

/// One entry of the likes list. The token is what makes the note readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteRef {
    pub note_id: String,
    pub xsec_token: String,
}

/// One file of one note, and the row that tracks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedNoteMedia {
    pub note_id: String,
    pub media_index: i64,
    pub media_type: String,
    pub media_url: String,
    pub title: String,
    pub author: String,
    pub author_id: String,
    pub note_type: String,
    pub published_at: String,
    pub xsec_token: String,
    /// Carried so a run can find the rows a previous one recorded as expired.
    pub last_error: String,
}

/// `_to_int`: an `int` (not a `bool`), a `float` truncated, or a string of digits with an
/// optional leading `-`. A float past `i128` saturates and a non-finite one is `None`
/// where Python raises (D179).
pub fn to_int(value: Option<&Value>) -> Option<i128> {
    match value? {
        Value::Int(int) => Some(*int),
        #[allow(clippy::cast_possible_truncation)] // reason: `int(float)` truncates; the cast saturates past i128 (D179)
        Value::Float(float) if float.is_finite() => Some(float.trunc() as i128),
        Value::Str(text) => {
            let text = py_strip(text);
            if !py_isdigit(text.trim_start_matches('-')) {
                return None;
            }
            // `int(text)`: one sign at most, and decimal digits only (a superscript is a
            // digit to `isdigit` but not to `int`, which raises; D179).
            let (negative, digits) = match text.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, text),
            };
            let magnitude = py_int_decimal(digits)?;
            Some(if negative { -magnitude } else { magnitude })
        }
        _ => None,
    }
}

/// `format_published_at(value)`: epoch milliseconds as `YYYY-MM-DD HH:MM:SS` in UTC+8, or
/// `''` for no positive time. A time `datetime.fromtimestamp(seconds, tz)` cannot hold is its
/// error: past `time_t` an `OverflowError`, past what `gmtime` can hold an `OSError`
/// (`EOVERFLOW`), past year 9999 in UTC a `ValueError`, and past it only once moved to UTC+8
/// an `OverflowError` again.
pub fn format_published_at(value: Option<&Value>) -> Result<String, PyError> {
    let Some(milliseconds) = to_int(value).filter(|milliseconds| *milliseconds > 0) else {
        return Ok(String::new());
    };
    let overflow = || PyError::new("OverflowError", "date value out of range");
    #[allow(clippy::cast_precision_loss)] // reason: `milliseconds / 1000` is a true division to a double in Python too
    let seconds = milliseconds as f64 / 1000.0;
    // `fromtimestamp` rounds to the microsecond, half to even.
    let mut whole = seconds.floor();
    let mut micros = ((seconds - whole) * 1e6).round_ties_even();
    if micros >= 1e6 {
        micros -= 1e6;
        whole += 1.0;
    }
    // `_PyTime_ObjectToTimeval`: the seconds must fit a 64-bit `time_t`.
    #[allow(clippy::cast_precision_loss)] // reason: 2**63 is exact as a double
    if !whole.is_finite() || whole >= i64::MAX as f64 {
        return Err(PyError::new("OverflowError", "timestamp out of range for platform time_t"));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // reason: whole is integral, positive and below 2**63; micros in 0..1e6
    let (whole, micros) = (whole as i64, micros as u32);
    // glibc's `gmtime` fails with `EOVERFLOW` once `tm_year` (years since 1900) leaves an `int`.
    let utc_year = civil_year(whole.div_euclid(86_400));
    if utc_year - 1900 > i64::from(i32::MAX) {
        return Err(PyError::new("OSError", "[Errno 75] Value too large for defined data type"));
    }
    if utc_year > 9999 {
        // CPython adds 1900 back in a C `int`, which wraps for the last two thousand years
        // `gmtime` still holds.
        #[allow(clippy::cast_possible_truncation)] // reason: the `int` arithmetic of CPython's `tm_year + 1900`, wrap included
        let year = ((utc_year - 1900) as i32).wrapping_add(1900);
        return Err(PyError::new("ValueError", format!("year {year} is out of range")));
    }
    let offset = chrono::FixedOffset::east_opt(DISPLAY_OFFSET_SECONDS).ok_or_else(overflow)?;
    let local = chrono::DateTime::from_timestamp(whole, micros * 1000)
        .ok_or_else(overflow)?
        .with_timezone(&offset);
    if chrono::Datelike::year(&local) > 9999 {
        return Err(overflow());
    }
    Ok(local.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// The proleptic Gregorian year of a day counted from 1970-01-01 (Howard Hinnant's
/// `civil_from_days`), for days far past what `chrono` holds.
fn civil_year(days: i64) -> i64 {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let year = year_of_era + era * 400;
    if month_index >= 10 { year + 1 } else { year }
}

fn split(url: &str) -> Result<fav_core::pyurl::SplitResult, PyError> {
    fav_core::pyurl::urlsplit(url, "").map_err(|error| PyError::new("ValueError", error.0))
}

/// `infer_image_extension(url)`: the name's own extension, else a format token in the
/// last path segment and the query (never the host, which is `sns-webpic...`), else jpg.
pub fn infer_image_extension(url: &str) -> Result<String, PyError> {
    let parts = split(url)?;
    let name = fav_core::pypath::name(&parts.path);
    let suffix = fav_core::pypath::suffix(name).to_lowercase();
    let suffix = suffix.strip_prefix('.').unwrap_or(&suffix);
    if let Some((_, mapped)) = IMAGE_EXT_ALIASES.iter().find(|(alias, _)| *alias == suffix) {
        return Ok((*mapped).to_owned());
    }
    let marker = format!("{name}?{}", parts.query).to_lowercase();
    Ok(IMAGE_EXT_TOKENS
        .iter()
        .find(|(token, _)| marker.contains(token))
        .map_or("jpg", |(_, ext)| ext)
        .to_owned())
}

/// `_field(source, *names)`: the first of the spellings the page (camelCase) and the API
/// (`snake_case`) use that is present and not `None`.
fn field<'a>(source: Option<&'a Value>, names: &[&str]) -> Option<&'a Value> {
    let Some(Value::Dict(dict)) = source else { return None };
    names
        .iter()
        .find_map(|name| dict.get(name).filter(|value| !matches!(value, Value::Null)))
}

/// `str(value or '')`.
fn text_or_empty(value: Option<&Value>) -> String {
    crate::pyval::str_or_empty(value)
}

/// `str(value or '').strip()`.
fn stripped(value: Option<&Value>) -> String {
    py_strip(&text_or_empty(value)).to_owned()
}

/// `normalize_media_url(url)`: the page hands out `http://` CDN links; take the
/// encrypted one.
pub fn normalize_media_url(url: &str) -> Result<String, PyError> {
    let value = py_strip(url);
    if value.is_empty() {
        return Ok(String::new());
    }
    let mut parts = split(value)?;
    if parts.scheme == "http" {
        "https".clone_into(&mut parts.scheme);
        return Ok(fav_core::pyurl::urlunsplit(&parts));
    }
    Ok(value.to_owned())
}

fn dicts(value: Option<&Value>) -> Vec<&Dict> {
    match value {
        Some(Value::List(items)) => items.iter().filter_map(Value::as_dict).collect(),
        _ => Vec::new(),
    }
}

/// `pick_image_url(entry)`: the full-size variant of one image (`WB_DFT`), else any
/// variant, else the page state's `urlDefault` / `urlPre`.
pub fn pick_image_url(entry: Option<&Value>) -> Result<String, PyError> {
    let entries = dicts(field(entry, &["infoList", "info_list"]));
    let preferred = entries.iter().filter(|item| {
        let scene = stripped(field(Some(&Value::Dict((**item).clone())), &["imageScene", "image_scene"]));
        scene.to_uppercase() == PREFERRED_IMAGE_SCENE
    });
    for item in preferred.chain(entries.iter()) {
        let url = normalize_media_url(&text_or_empty(item.get("url")))?;
        if !url.is_empty() {
            return Ok(url);
        }
    }
    for name in ["urlDefault", "urlPre"] {
        let url = normalize_media_url(&text_or_empty(field(entry, &[name])))?;
        if !url.is_empty() {
            return Ok(url);
        }
    }
    Ok(String::new())
}

/// `_best_video_stream(stream)`: `(url, long_edge)` of the largest variant across every
/// bucket (largest pixel count, ties to the higher bitrate, then the first seen);
/// `('', 0)` when nothing is playable.
pub fn best_video_stream(stream: Option<&Value>) -> Result<(String, i128), PyError> {
    let mut best_url = String::new();
    let mut best_edge = 0;
    let mut best_key: (i128, i128) = (-1, -1);
    let Some(Value::Dict(buckets)) = stream else {
        return Ok((best_url, best_edge));
    };
    for (_, bucket) in buckets.iter() {
        let Value::List(variants) = bucket else { continue };
        for variant in variants {
            if !matches!(variant, Value::Dict(_)) {
                continue;
            }
            let url = normalize_media_url(&text_or_empty(field(Some(variant), &["masterUrl", "master_url"])))?;
            if url.is_empty() {
                continue;
            }
            let width = to_int(field(Some(variant), &["width"])).unwrap_or(0);
            let height = to_int(field(Some(variant), &["height"])).unwrap_or(0);
            let bitrate = to_int(field(Some(variant), &["videoBitrate", "video_bitrate"])).unwrap_or(0);
            let key = (width.saturating_mul(height), bitrate);
            if key > best_key {
                best_key = key;
                best_url = url;
                best_edge = width.max(height);
            }
        }
    }
    Ok((best_url, best_edge))
}

/// `pick_video_url(stream)`: the highest-resolution playable stream.
pub fn pick_video_url(stream: Option<&Value>) -> Result<String, PyError> {
    Ok(best_video_stream(stream)?.0)
}

/// `note_source_long_edge(video_block)`: the upload's own long edge from `mediaV2` (a
/// JSON string on the page, parsed in the API), or 0.
pub fn note_source_long_edge(video_block: Option<&Value>) -> i128 {
    let parsed;
    let mut media_v2 = field(video_block, &["mediaV2", "media_v2"]);
    if let Some(Value::Str(text)) = media_v2 {
        match pyjson::loads(text) {
            Ok(value) => {
                parsed = value;
                media_v2 = Some(&parsed);
            }
            Err(_) => return 0,
        }
    }
    let inner = field(media_v2, &["video"]);
    to_int(field(inner, &["height"]))
        .unwrap_or(0)
        .max(to_int(field(inner, &["width"])).unwrap_or(0))
}

/// `build_note_url(note_id, xsec_token)`: the page a note lives on; without the token it
/// only opens for its author.
pub fn build_note_url(note_id: &str, xsec_token: &str) -> String {
    let url = format!("{WEB_ORIGIN}/explore/{note_id}");
    if xsec_token.is_empty() {
        url
    } else {
        format!("{url}?xsec_token={xsec_token}&xsec_source={XSEC_SOURCE}")
    }
}

/// `build_media_filename(media, ext=...)`: `[nickname]2026-08-12 [<note id>_<n>].webp`.
pub fn build_media_filename(media: &RedNoteMedia, ext: &str) -> Result<String, PyError> {
    let ext = if !ext.is_empty() {
        ext.to_owned()
    } else if VIDEO_MEDIA_TYPES.contains(&media.media_type.as_str()) {
        VIDEO_EXT.to_owned()
    } else {
        infer_image_extension(&media.media_url)?
    };
    let date: String = media.published_at.chars().take(10).collect();
    Ok(format_media_filename(
        &date,
        &format!("{}_{}", media.note_id, media.media_index),
        Some(&media.author),
        &ext,
        fav_core::filename::MAX_TOTAL_BYTES,
    ))
}

/// `parse_api_envelope(payload)`: the `data` of a `{code, data}` envelope. A signed-out
/// code is the login failure (`RedNoteError`), any other code a `ValueError`, a body that
/// is not an object a `TypeError`.
pub fn parse_api_envelope(payload: Option<&Value>) -> Result<Dict, JobError> {
    let Some(Value::Dict(payload)) = payload else {
        return Err(JobError::new("TypeError", "RedNote returned a body that is not an object"));
    };
    let code = to_int(payload.get("code"));
    let message = stripped(crate::pyval::first_truthy(payload, &["msg", "message"]));
    if let Some(code) = code.filter(|code| SESSION_EXPIRED_CODES.contains(code)) {
        return Err(rednote_error(
            format!("The RedNote session is signed out ({code} {message}). {RELOGIN_HINT}"),
            "rednote:login",
        ));
    }
    if let Some(code) = code.filter(|code| *code != 0) {
        let detail = if message.is_empty() {
            String::new()
        } else {
            format!(" ({message})")
        };
        return Err(JobError::new("ValueError", format!("RedNote returned error code: {code}{detail}")));
    }
    Ok(match payload.get("data") {
        Some(Value::Dict(data)) => data.clone(),
        _ => Dict::new(),
    })
}

/// `decide_login_state(probe, cookies)`: the page's own store first, then a login or
/// verify modal or a QR, then the `web_session` cookie; [`LOGIN_UNKNOWN`] for a page that
/// has not hydrated yet.
pub fn decide_login_state<S: std::hash::BuildHasher>(
    probe: &Map<String, serde_json::Value>,
    cookies: &HashMap<String, String, S>,
) -> &'static str {
    if truthy(probe.get("logged_in")) {
        return LOGGED_IN;
    }
    if truthy(probe.get("has_login_modal")) || truthy(probe.get("has_verify_modal")) || truthy(probe.get("qr_src")) {
        return LOGGED_OUT;
    }
    if py_strip(cookies.get(SESSION_COOKIE).map_or("", String::as_str)).is_empty() {
        return LOGGED_OUT;
    }
    LOGIN_UNKNOWN
}

/// `extract_like_page(data)`: the notes on one page of the likes list, its cursor and
/// whether there is more.
pub fn extract_like_page(data: &Dict) -> (Vec<NoteRef>, String, bool) {
    let notes = dicts(data.get("notes"))
        .into_iter()
        .filter_map(|item| {
            let note_id = stripped(crate::pyval::first_truthy(item, &["note_id", "id"]));
            (!note_id.is_empty()).then(|| NoteRef {
                note_id,
                xsec_token: stripped(item.get("xsec_token")),
            })
        })
        .collect();
    let has_more = data.get("has_more").is_some_and(Value::is_truthy);
    (notes, stripped(data.get("cursor")), has_more)
}

/// `extract_note_media(note_card, note_id=..., xsec_token=...)`: one row per file, indices
/// in the note's own file order (a live photo's still and clip counting separately). A
/// video note yields its row even with no readable stream: the row is what the next run
/// looks it up by.
pub fn extract_note_media(note_card: &Dict, note_id: &str, xsec_token: &str) -> Result<Vec<RedNoteMedia>, PyError> {
    let card = Value::Dict(note_card.clone());
    let user = field(Some(&card), &["user"]).filter(|user| matches!(user, Value::Dict(_)));
    let author = stripped(field(user, &["nickname"]));
    let note_type = stripped(note_card.get("type"));
    let shared = RedNoteMedia {
        note_id: note_id.to_owned(),
        media_index: 0,
        media_type: String::new(),
        media_url: String::new(),
        title: stripped(note_card.get("title")),
        author: if author.is_empty() { "unknown".to_owned() } else { author },
        author_id: stripped(field(user, &["userId", "user_id"])),
        note_type: note_type.clone(),
        published_at: format_published_at(note_card.get("time"))?,
        xsec_token: xsec_token.to_owned(),
        last_error: String::new(),
    };
    let row = |index: usize, media_type: &str, media_url: String| RedNoteMedia {
        media_index: i64::try_from(index).unwrap_or(i64::MAX),
        media_type: media_type.to_owned(),
        media_url,
        ..shared.clone()
    };

    if note_type == VIDEO_NOTE_TYPE {
        let video_block = note_card.get("video");
        let media_url = pick_video_url(field(field(video_block, &["media"]), &["stream"]))?;
        return Ok(vec![row(1, VIDEO_MEDIA_TYPE, media_url)]);
    }

    let mut media = Vec::new();
    let image_list = field(Some(&card), &["imageList", "image_list"]);
    let entries = match image_list {
        Some(Value::List(entries)) => entries.as_slice(),
        _ => &[],
    };
    for entry in entries.iter().filter(|entry| matches!(entry, Value::Dict(_))) {
        let image_url = pick_image_url(Some(entry))?;
        if !image_url.is_empty() {
            media.push(row(media.len() + 1, IMAGE_MEDIA_TYPE, image_url));
        }
        // An image with a stream attached is a live photo: the still and the clip both.
        let live_url = pick_video_url(entry.get("stream"))?;
        if !live_url.is_empty() {
            media.push(row(media.len() + 1, LIVE_MEDIA_TYPE, live_url));
        }
    }
    Ok(media)
}

// ---------- the source ----------

/// Makes the browser a run reads the account with; a proxy Chromium cannot use is
/// refused before anything starts.
pub type BrowserFactory = Box<dyn Fn() -> Result<Box<dyn NoteBrowser>, ProxyError> + Send + Sync>;

/// The page a crawl step is on: `(notes, cursor, has_more)`.
type LikePage = (Vec<NoteRef>, String, bool);

/// How one download failed.
#[derive(Debug)]
enum DownloadFailure {
    /// `MediaUrlStaleError`: the stored URL no longer serves.
    Stale(String),
    /// `httpx.HTTPStatusError` of any other status.
    Status(u16),
    /// Anything else, as `TypeName: message`.
    Other { class: String, message: String },
}

/// `RedNote`: one run of the source.
pub struct RedNote {
    db: Db,
    notifier: SharedNotifier,
    config: RedNoteConfig,
    client: HttpClient,
    browser_factory: BrowserFactory,
    user_id: String,
    user_agent: String,
    logged_card_shapes: HashSet<String>,
    warned_downscaled: bool,
}

impl std::fmt::Debug for RedNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedNote").field("user_id", &self.user_id).finish_non_exhaustive()
    }
}

/// The CDN client's options: only ever talks to the CDN, proxied only when `proxy_media`
/// asks for it (`proxy=self.cfg.proxy or None if self.cfg.proxy_media else None`).
pub fn client_options(config: &RedNoteConfig) -> ClientOptions {
    ClientOptions {
        headers: vec![("Accept-Language".to_owned(), "zh-CN,zh;q=0.9".to_owned())],
        timeout: Duration::from_mins(1),
        follow_redirects: true,
        proxy: (config.proxy_media && !config.proxy.is_empty()).then(|| config.proxy.clone()),
        max_keepalive_connections: 10,
        ..ClientOptions::default()
    }
}

/// The CDN client `RedNote.__init__` builds. httpx refuses a proxy it cannot dial when the
/// client is built, so the run fails with its `ValueError` before anything starts.
pub fn cdn_client(config: &RedNoteConfig) -> Result<HttpClient, JobError> {
    let options = client_options(config);
    if let Some(proxy) = &options.proxy {
        proxy_probe::check_httpx_proxy(proxy).map_err(|message| JobError::new("ValueError", message))?;
    }
    HttpClient::new(&options).map_err(|error| JobError::new("ValueError", error.to_string()))
}

/// `_new_browser`: the real profile.
pub fn chromium_factory(config: &RedNoteConfig) -> BrowserFactory {
    let profile = config.profile_path.clone();
    let proxy = config.proxy.clone();
    let headless = config.headless;
    Box::new(move || {
        let browser = ChromiumNoteBrowser::new(
            fav_browser::chromium::playwright_chromium_executable(),
            profile.as_path(),
            &proxy,
            headless,
        )?;
        Ok(Box::new(browser) as Box<dyn NoteBrowser>)
    })
}

impl RedNote {
    pub fn new(db: Db, notifier: SharedNotifier, config: RedNoteConfig, client: HttpClient, browser_factory: BrowserFactory) -> Self {
        Self {
            db,
            notifier,
            config,
            client,
            browser_factory,
            user_id: String::new(),
            user_agent: String::new(),
            logged_card_shapes: HashSet::new(),
            warned_downscaled: false,
        }
    }

    /// The account id the run settled on.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    #[cfg(test)]
    pub(crate) fn set_user_id(&mut self, user_id: &str) {
        user_id.clone_into(&mut self.user_id);
    }

    // ---------- state ----------

    async fn read_state(&self, key: &str) -> Result<String, JobError> {
        let row = sqlx::query("SELECT value FROM rednote_state WHERE key = $1;")
            .bind(key)
            .fetch_optional(self.db.pool())
            .await
            .map_err(sql_error)?;
        Ok(row
            .map(|row| row.try_get::<String, _>("value"))
            .transpose()
            .map_err(sql_error)?
            .unwrap_or_default())
    }

    async fn backfill_complete(&self) -> Result<bool, JobError> {
        let row = sqlx::query("SELECT value FROM rednote_state WHERE key = $1;")
            .bind(BACKFILL_STATE_KEY)
            .fetch_optional(self.db.pool())
            .await
            .map_err(sql_error)?;
        let value: Option<String> = row.map(|row| row.try_get("value")).transpose().map_err(sql_error)?;
        Ok(value.as_deref() == Some("1"))
    }

    async fn mark_backfill_complete(&self) -> Result<(), JobError> {
        sqlx::query("INSERT INTO rednote_state (key, value) VALUES ($1, $2) ON CONFLICT DO NOTHING;")
            .bind(BACKFILL_STATE_KEY)
            .bind("1")
            .execute(self.db.pool())
            .await
            .map_err(sql_error)?;
        Ok(())
    }

    async fn write_state(&self, key: &str, value: &str) -> Result<(), JobError> {
        sqlx::query("INSERT INTO rednote_state (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value;")
            .bind(key)
            .bind(value)
            .execute(self.db.pool())
            .await
            .map_err(sql_error)?;
        Ok(())
    }

    /// `_known_note_ids`: which of these notes already have rows (not "downloaded": a file
    /// that keeps failing must not hold the stop rule open).
    async fn known_note_ids(&self, note_ids: &[String]) -> Result<HashSet<String>, JobError> {
        let unique: Vec<String> = crate::pyval::unique(note_ids.to_vec());
        if unique.is_empty() {
            return Ok(HashSet::new());
        }
        let rows = sqlx::query("SELECT DISTINCT note_id FROM rednote WHERE note_id = ANY($1);")
            .bind(&unique)
            .fetch_all(self.db.pool())
            .await
            .map_err(sql_error)?;
        rows.iter()
            .map(|row| row.try_get::<String, _>("note_id").map_err(sql_error))
            .collect()
    }

    /// `_upsert_media`: one row per file; a conflict refreshes what RedNote rotates and
    /// never touches the download state.
    async fn upsert_media(&self, items: &[RedNoteMedia]) -> Result<(), JobError> {
        let mut transaction = self.db.pool().begin().await.map_err(sql_error)?;
        for media in items {
            sqlx::query(
                "INSERT INTO rednote (note_id, media_index, media_type, media_url, title, author, author_id, note_type, published_at, xsec_token)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT (note_id, media_index) DO UPDATE SET media_url = excluded.media_url, xsec_token = excluded.xsec_token, \
                 title = excluded.title, author = excluded.author, published_at = excluded.published_at;",
            )
            .bind(&media.note_id)
            .bind(media_index(media.media_index)?)
            .bind(&media.media_type)
            .bind(&media.media_url)
            .bind(&media.title)
            .bind(&media.author)
            .bind(&media.author_id)
            .bind(&media.note_type)
            .bind(&media.published_at)
            .bind(&media.xsec_token)
            .execute(&mut *transaction)
            .await
            .map_err(sql_error)?;
        }
        transaction.commit().await.map_err(sql_error)
    }

    /// `_pending_media`: every row not downloaded and not retired, oldest first (ties in
    /// file order, D180).
    pub async fn pending_media(&self) -> Result<Vec<RedNoteMedia>, JobError> {
        let rows = sqlx::query(
            "SELECT note_id, media_index, media_type, media_url, title, author, author_id, note_type, published_at, xsec_token, last_error
             FROM rednote
             WHERE downloaded = 0 AND unavailable = 0
             ORDER BY created_at ASC, note_id ASC, media_index ASC;",
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(sql_error)?;
        let mut pending = Vec::new();
        for row in &rows {
            let text = |column: &str| row.try_get::<String, _>(column).map_err(sql_error);
            let note_id = py_strip(&text("note_id")?).to_owned();
            if note_id.is_empty() {
                continue;
            }
            let author = text("author")?;
            let author = py_strip(if author.is_empty() { "unknown" } else { &author }).to_owned();
            pending.push(RedNoteMedia {
                note_id,
                media_index: i64::from(row.try_get::<i32, _>("media_index").map_err(sql_error)?),
                media_type: text("media_type")?,
                media_url: text("media_url")?,
                title: text("title")?,
                author: if author.is_empty() { "unknown".to_owned() } else { author },
                author_id: text("author_id")?,
                note_type: text("note_type")?,
                published_at: text("published_at")?,
                xsec_token: text("xsec_token")?,
                last_error: text("last_error")?,
            });
        }
        Ok(pending)
    }

    async fn mark_downloaded(&self, media: &RedNoteMedia, dst_path: &str) -> Result<(), JobError> {
        sqlx::query(
            "UPDATE rednote
             SET downloaded = 1, local_path = $1, unavailable = 0, failed_count = 0, last_error = ''
             WHERE note_id = $2 AND media_index = $3;",
        )
        .bind(dst_path)
        .bind(&media.note_id)
        .bind(media_index(media.media_index)?)
        .execute(self.db.pool())
        .await
        .map_err(sql_error)?;
        Ok(())
    }

    async fn mark_unavailable(&self, media: &RedNoteMedia, reason: &str) -> Result<(), JobError> {
        sqlx::query(
            "UPDATE rednote
             SET unavailable = 1, failed_count = failed_count + 1, last_error = $1
             WHERE note_id = $2 AND media_index = $3;",
        )
        .bind(reason.chars().take(500).collect::<String>())
        .bind(&media.note_id)
        .bind(media_index(media.media_index)?)
        .execute(self.db.pool())
        .await
        .map_err(sql_error)?;
        Ok(())
    }

    async fn mark_failed(&self, media: &RedNoteMedia, reason: &str) -> Result<(), JobError> {
        sqlx::query(
            "UPDATE rednote
             SET failed_count = failed_count + 1, last_error = $1
             WHERE note_id = $2 AND media_index = $3;",
        )
        .bind(reason.chars().take(500).collect::<String>())
        .bind(&media.note_id)
        .bind(media_index(media.media_index)?)
        .execute(self.db.pool())
        .await
        .map_err(sql_error)?;
        Ok(())
    }

    // ---------- the signed-in browser ----------

    /// `_check_login`: go on only with a signed-in profile (signing in is the 登录态 page's
    /// job). One reload is allowed for a page whose snapshot predates the session.
    pub async fn check_login(&mut self, browser: &mut dyn NoteBrowser) -> Result<(), JobError> {
        for attempt in 1..=LOGIN_CHECK_ATTEMPTS {
            let probe = browser.probe_login().await.map_err(|error| browser_error(&error))?;
            let cookies = browser.cookie_dict().await.map_err(|error| browser_error(&error))?;
            match decide_login_state(&probe, &cookies) {
                LOGGED_IN => {
                    self.user_id = if self.config.user_id.is_empty() {
                        user_id_from_probe(&probe)
                    } else {
                        self.config.user_id.clone()
                    };
                    return Ok(());
                }
                LOGGED_OUT => break,
                _ => {}
            }
            if attempt < LOGIN_CHECK_ATTEMPTS {
                // The site routes itself while it settles; an interrupted reload is not the
                // run's problem, the next probe answers either way.
                if let Err(error) = browser.reload_login().await {
                    log().info(format!("RedNote login page reload was interrupted: {error}"));
                }
            }
        }
        Err(rednote_error(format!("RedNote is signed out. {RELOGIN_HINT}"), "rednote:login"))
    }

    /// `_record_session`: keep the 登录态 page's row in step. The first signed-in run writes
    /// one (names and expiry only: the profile is the session); later runs record the check.
    async fn record_session(&self, browser: &mut dyn NoteBrowser) -> Result<(), JobError> {
        if site_sessions::load(&self.db, SESSION_SITE)
            .await
            .map_err(session_store_error)?
            .is_some()
        {
            return site_sessions::record_check(&self.db, SESSION_SITE, true, "")
                .await
                .map_err(session_store_error);
        }
        let raw: Vec<Value> = browser
            .cookies()
            .await
            .map_err(|error| browser_error(&error))?
            .iter()
            .map(Value::from_serde)
            .collect();
        let cookies = site_sessions::cookies_from_browser(&raw);
        site_sessions::save_signed_in(
            &self.db,
            SESSION_SITE,
            &site_sessions::without_values(&cookies),
            &self.user_id,
            site_sessions::cookie_expiry(&cookies, SESSION_COOKIE),
            &[],
        )
        .await
        .map_err(session_store_error)
    }

    /// `_record_signed_out`: mark the login invalid, without hiding the run's own error.
    async fn record_signed_out(&self, message: &str) {
        if let Err(error) = site_sessions::record_check(&self.db, SESSION_SITE, false, message).await {
            log().warning(format!("Could not record the RedNote sign-out on the 登录态 page: {error}"));
        }
    }

    /// `_prepare_session`: check the login, then settle the account id and the UA.
    pub async fn prepare_session(&mut self, browser: &mut dyn NoteBrowser) -> Result<(), JobError> {
        self.check_login(browser).await?;
        if self.user_id.is_empty() {
            self.user_id = if self.config.user_id.is_empty() {
                self.read_state(USER_ID_STATE_KEY).await?
            } else {
                self.config.user_id.clone()
            };
        }
        if self.user_id.is_empty() {
            // Its own key: the login is fine, and must not be marked invalid.
            return Err(rednote_error(
                "Signed in, but the profile id could not be read off the page; set user_id in the settings.",
                "rednote:user_id",
            ));
        }
        if self.config.user_id.is_empty() {
            self.write_state(USER_ID_STATE_KEY, &self.user_id).await?;
        }
        self.record_session(browser).await?;
        self.user_agent = browser.user_agent().await.map_err(|error| browser_error(&error))?;
        Ok(())
    }

    async fn sleep_between_requests(&self) {
        if self.config.sleep_request_seconds > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(self.config.sleep_request_seconds)).await;
        }
    }

    // ---------- reading the account ----------

    /// `_initial_like_page`: the likes the page was served with, as a page without a
    /// cursor (it can never be sent twice).
    async fn initial_like_page(&self, browser: &mut dyn NoteBrowser) -> Result<Option<LikePage>, JobError> {
        let raw = browser.initial_like_notes().await.map_err(|error| browser_error(&error))?;
        let mut data = Dict::new();
        data.insert("notes", Value::List(raw));
        let (notes, _, _) = extract_like_page(&data);
        if notes.is_empty() {
            log().debug("RedNote found no pre-rendered likes on the profile page");
            return Ok(None);
        }
        log().info(format!("RedNote read {} likes rendered into the page itself", notes.len()));
        Ok(Some((notes, String::new(), true)))
    }

    /// `_next_like_page`: one page of the likes list, from the browser's own XHR.
    async fn next_like_page(&self, browser: &mut dyn NoteBrowser) -> Result<Option<LikePage>, JobError> {
        let captured: Option<CapturedPage> = browser
            .next_like_page(LIKE_PAGE_TIMEOUT)
            .await
            .map_err(|error| browser_error(&error))?;
        let Some(captured) = captured else { return Ok(None) };
        if RISK_CONTROL_STATUS_CODES.contains(&captured.status) {
            return Err(rednote_error(
                format!(
                    "RedNote answered the likes list with HTTP {}: the account is behind a captcha. Clear it in the browser.",
                    captured.status
                ),
                "rednote:risk",
            ));
        }
        let (notes, _, has_more) = extract_like_page(&parse_api_envelope(captured.body.as_ref())?);
        // The cursor the page was *fetched* with identifies it.
        Ok(Some((notes, cursor_of(&captured.url), has_more)))
    }

    /// `_fetch_note_card`: one note's state, its shape logged once per note type.
    async fn fetch_note_card(&mut self, note_id: &str, xsec_token: &str, browser: &mut dyn NoteBrowser) -> Result<Dict, NoteBrowserError> {
        let card = browser.note_state(&build_note_url(note_id, xsec_token), note_id).await?;
        let note_type = text_or_empty(field(Some(&card), &["type"]));
        let note_type = if note_type.is_empty() { "unknown".to_owned() } else { note_type };
        if self.logged_card_shapes.insert(note_type.clone()) {
            log().debug(format!(
                "RedNote note card shape (type={note_type}): {}",
                repr_value(&describe_shape(&card, 5))
            ));
        }
        Ok(match card {
            Value::Dict(card) => card,
            _ => Dict::new(),
        })
    }

    // ---------- downloads ----------

    /// `_destination_root`: videos and live clips follow `video_path` when it is set.
    pub fn destination_root(&self, media_type: &str) -> &PyPath {
        match &self.config.video_path {
            Some(video_path) if VIDEO_MEDIA_TYPES.contains(&media_type) => video_path,
            _ => &self.config.path,
        }
    }

    /// `_build_output_path`: `<root>/<author>/<filename>`.
    pub fn build_output_path(&self, media: &RedNoteMedia, ext: &str) -> Result<String, PyError> {
        let author_dir = sanitize(&media.author, 200);
        let author_dir = if author_dir.is_empty() { "unknown".to_owned() } else { author_dir };
        let root = self.destination_root(&media.media_type).as_str();
        let directory = fav_core::pypath::join(root, &author_dir);
        Ok(fav_core::pypath::join(&directory, &build_media_filename(media, ext)?))
    }

    /// `_refresh_stale_media`: re-resolve the notes behind expired URLs while the browser
    /// is open. A row is only retired here, after a fresh look at the note itself.
    pub async fn refresh_stale_media(&mut self, browser: &mut dyn NoteBrowser) -> Result<u64, JobError> {
        let stale: Vec<RedNoteMedia> = self
            .pending_media()
            .await?
            .into_iter()
            .filter(|media| media.last_error.starts_with(STALE_URL_MARKER))
            .collect();
        if stale.is_empty() {
            return Ok(0);
        }
        log().info(format!("RedNote re-resolving {} file(s) whose URL expired", stale.len()));
        let mut refreshed = 0;
        for media in &stale {
            self.sleep_between_requests().await;
            let note_card = match self.fetch_note_card(&media.note_id, &media.xsec_token, browser).await {
                Ok(card) => card,
                Err(error) => {
                    log().warning(format!("Could not re-resolve note {}: {error}", media.note_id));
                    continue;
                }
            };
            let fresh = if note_card.is_empty() {
                Vec::new()
            } else {
                extract_note_media(&note_card, &media.note_id, &media.xsec_token).map_err(py_job_error)?
            };
            let found = fresh
                .into_iter()
                .find(|item| item.media_index == media.media_index && item.media_type == media.media_type);
            let Some(found) = found.filter(|item| !item.media_url.is_empty()) else {
                self.mark_unavailable(media, "the note no longer offers this file").await?;
                continue;
            };
            if found.media_url == media.media_url {
                continue;
            }
            sqlx::query("UPDATE rednote SET media_url = $1, last_error = '' WHERE note_id = $2 AND media_index = $3;")
                .bind(&found.media_url)
                .bind(&media.note_id)
                .bind(media_index(media.media_index)?)
                .execute(self.db.pool())
                .await
                .map_err(sql_error)?;
            refreshed += 1;
        }
        Ok(refreshed)
    }

    /// `_stream_to_file`: the body written out as it arrives, never held whole.
    async fn stream_to_file(&self, url: &str, part: &Path) -> Result<(), DownloadFailure> {
        let other = |class: &str, message: String| DownloadFailure::Other {
            class: class.to_owned(),
            message,
        };
        let io = |error: &std::io::Error| other(fav_net::download::io_class_name(error), fav_net::download::py_os_error_text(error));
        let mut file = tokio::fs::File::create(part).await.map_err(|error| io(&error))?;
        let mut response = self
            .client
            .send(Request::get(url).header("Accept", MEDIA_ACCEPT))
            .await
            .map_err(|error| other(error.class.name(), error.message.clone()))?;
        if let Err(error) = response.error_for_status() {
            return Err(if MEDIA_STALE_STATUS_CODES.contains(&error.status) {
                // Not resolved here: the browser is closed, and a 404 on a CDN URL says as
                // much about rotation as about deletion.
                DownloadFailure::Stale(format!("{STALE_URL_MARKER} http {}", error.status))
            } else {
                DownloadFailure::Status(error.status)
            });
        }
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| other(error.class.name(), error.message.clone()))?
        {
            file.write_all(&chunk).await.map_err(|error| io(&error))?;
        }
        file.flush().await.map_err(|error| io(&error))
    }

    /// `_download_file`: one file straight from the CDN, written beside its destination
    /// and moved into place at the end, so a partial file never looks finished.
    async fn download_file(&self, media: &RedNoteMedia) -> Result<String, DownloadFailure> {
        let other = |class: &str, message: String| DownloadFailure::Other {
            class: class.to_owned(),
            message,
        };
        let dst_path = self
            .build_output_path(media, "")
            .map_err(|error| other(&error.class, error.message))?;
        let destination = Path::new(&dst_path);
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                let error = os_error(&error, &parent.to_string_lossy());
                other(error.class_name(), error.message().to_owned())
            })?;
        }
        if tokio::fs::try_exists(destination).await.unwrap_or(false) {
            return Ok(dst_path);
        }
        if media.media_url.is_empty() {
            return Err(DownloadFailure::Stale(format!("{STALE_URL_MARKER} the row carries no URL")));
        }
        let part = format!("{dst_path}{PARTIAL_SUFFIX}");
        let part_path = Path::new(&part);
        // Cancellation included: dropping this future (a killed run) leaves no half file.
        let guard = PartGuard(Some(part_path.to_path_buf()));
        self.stream_to_file(&media.media_url, part_path).await?;
        tokio::fs::rename(part_path, destination).await.map_err(|error| {
            let error = os_error(&error, &part);
            other(error.class_name(), error.message().to_owned())
        })?;
        guard.disarm();
        Ok(dst_path)
    }

    /// `_download_pending`: every pending row, each marked as it lands; one dead URL never
    /// ends the run. The CDN is not paced like the site.
    pub async fn download_pending(&self, items: &[RedNoteMedia]) -> Result<u64, JobError> {
        let mut downloaded = 0;
        let total = items.len();
        for (position, media) in items.iter().enumerate() {
            let position = position + 1;
            log().info(format!(
                "RedNote downloading progress={position}/{total} note={} index={} type={}",
                media.note_id, media.media_index, media.media_type
            ));
            match self.download_file(media).await {
                Ok(dst_path) => {
                    self.mark_downloaded(media, &dst_path).await?;
                    downloaded += 1;
                    log().notice(format!(
                        "RedNote downloaded progress={position}/{total} file={}",
                        fav_core::pypath::name(&dst_path)
                    ));
                }
                Err(DownloadFailure::Stale(reason)) => {
                    // Recorded rather than resolved: the next run re-reads the note.
                    self.mark_failed(media, &reason).await?;
                    log().info(format!(
                        "RedNote URL expired note={} index={}: {reason}",
                        media.note_id, media.media_index
                    ));
                }
                Err(DownloadFailure::Status(status)) => {
                    self.mark_failed(media, &format!("http {status}")).await?;
                    log().warning(format!("Failed note={} index={}: http {status}", media.note_id, media.media_index));
                }
                Err(DownloadFailure::Other { class, message }) => {
                    self.mark_failed(media, &format!("{class}: {message}")).await?;
                    log().warning(format!("Failed note={} index={}: {message}", media.note_id, media.media_index));
                }
            }
        }
        Ok(downloaded)
    }

    // ---------- phases ----------

    /// `_absorb_page`: resolve whatever on this page is not archived, retired or already
    /// seen in this walk, and say what that added.
    pub async fn absorb_page(
        &mut self,
        notes: &[NoteRef],
        browser: &mut dyn NoteBrowser,
        seen: &mut HashSet<String>,
        page_index: i64,
        run_id: &str,
    ) -> Result<u64, JobError> {
        let note_ids: Vec<String> = notes.iter().map(|note| note.note_id.clone()).collect();
        let known = self.known_note_ids(&note_ids).await?;
        let retired = self.retired_note_ids(&note_ids).await?;
        let unknown: Vec<NoteRef> = notes
            .iter()
            .filter(|note| !known.contains(&note.note_id) && !retired.contains(&note.note_id) && !seen.contains(&note.note_id))
            .cloned()
            .collect();
        seen.extend(note_ids);

        let mut added = 0;
        if !unknown.is_empty() {
            added = self.resolve_notes(&unknown, browser, run_id).await?;
            // Reading a note navigates away from the list, so it is reopened.
            let user_id = self.user_id.clone();
            browser.open_likes(&user_id).await.map_err(|error| browser_error(&error))?;
        }
        log().info(format!(
            "RedNote likes page={page_index} notes={} new={} added={added}",
            notes.len(),
            unknown.len()
        ));
        Ok(added)
    }

    /// `_crawl`: walk the likes list newest first, resolving each page's new notes as it
    /// goes. The early stop applies only once a full walk has finished.
    pub async fn crawl(&mut self, browser: &mut dyn NoteBrowser) -> Result<u64, JobError> {
        let backfill_complete = self.backfill_complete().await?;
        // Identifies this walk, so a note seen twice in it counts once toward retiring.
        let run_id = run_id();
        let mut seen_cursors: HashSet<String> = HashSet::new();
        let mut seen_note_ids: HashSet<String> = HashSet::new();
        let mut page_index: i64 = 0;
        let mut full_hit_pages: i64 = 0;
        let mut resolved = 0;
        let mut reached_end = false;
        let mut stopped = false;

        log().info(format!(
            "RedNote walking the likes list (backfill_complete={})",
            if backfill_complete { "True" } else { "False" }
        ));
        let user_id = self.user_id.clone();
        browser.open_likes(&user_id).await.map_err(|error| browser_error(&error))?;
        // The first screenful is rendered into the page, so the first request is for what
        // comes after it: reading only requests would skip the newest likes.
        let mut pending_first = self.initial_like_page(browser).await?;

        while page_index < self.config.max_pages_per_run {
            page_index += 1;
            let page = match pending_first.take() {
                Some(page) => Some(page),
                None => self.next_like_page(browser).await?,
            };
            let Some((notes, cursor, has_more)) = page else {
                // Not the same as reaching the end: the backfill stays unfinished.
                log().info(format!("RedNote likes list stopped producing at page={page_index}"));
                stopped = true;
                break;
            };
            if notes.is_empty() {
                log().info(format!("RedNote likes list ended at page={page_index}"));
                reached_end = true;
                stopped = true;
                break;
            }
            if !cursor.is_empty() && seen_cursors.contains(&cursor) {
                log().debug(format!(
                    "RedNote repeated the page at cursor {}, skipping it",
                    fav_runtime::pystr::repr_str(&cursor)
                ));
                page_index -= 1;
                continue;
            }
            seen_cursors.insert(cursor);

            let added = self.absorb_page(&notes, browser, &mut seen_note_ids, page_index, &run_id).await?;
            resolved += added;

            // What the page contributed, not whether every id on it was known: a note
            // that can never be resolved must not hold the stop rule open.
            if added > 0 {
                full_hit_pages = 0;
            } else {
                full_hit_pages += 1;
                log().info(format!(
                    "RedNote page={page_index} added nothing, consecutive={full_hit_pages}/{}",
                    self.config.abort_after
                ));
                if backfill_complete && full_hit_pages >= self.config.abort_after {
                    log().info("RedNote reached the incremental stop condition");
                    stopped = true;
                    break;
                }
            }

            if !has_more {
                log().info("RedNote likes list reported no further pages");
                reached_end = true;
                stopped = true;
                break;
            }
        }
        if !stopped {
            log().info(format!(
                "RedNote stopped at the {} page cap; the next run continues from here",
                self.config.max_pages_per_run
            ));
        }

        if reached_end && !backfill_complete {
            self.mark_backfill_complete().await?;
            log().info(format!(
                "RedNote backfill finished; later runs stop after {} archived pages",
                self.config.abort_after
            ));
        }
        Ok(resolved)
    }

    /// `_resolve_notes`: each new note into its rows. One unreadable note never ends the
    /// run, and only the site's own 404 counts toward retiring it.
    pub async fn resolve_notes(&mut self, notes: &[NoteRef], browser: &mut dyn NoteBrowser, run_id: &str) -> Result<u64, JobError> {
        let mut resolved = 0;
        let total = notes.len();
        for (position, note) in notes.iter().enumerate() {
            self.sleep_between_requests().await;
            let note_card = match self.fetch_note_card(&note.note_id, &note.xsec_token, browser).await {
                Ok(card) => card,
                Err(NoteBrowserError::Gone(_)) => {
                    self.record_missing(&note.note_id, run_id).await?;
                    continue;
                }
                Err(error) => {
                    // This run's problem, not the note's.
                    log().warning(format!("Failed to read note {} ({}/{total}): {error}", note.note_id, position + 1));
                    continue;
                }
            };
            let media = extract_note_media(&note_card, &note.note_id, &note.xsec_token).map_err(py_job_error)?;
            if media.is_empty() {
                // Read fine, carries nothing this source handles. Not a deletion.
                log().warning(format!("Note {} carries no files, skipping", note.note_id));
                continue;
            }
            self.warn_if_downscaled(&note_card, &media)?;
            self.upsert_media(&media).await?;
            self.clear_missing(&note.note_id).await?;
            resolved += u64::try_from(media.len()).unwrap_or(u64::MAX);
        }
        Ok(resolved)
    }

    /// `_warn_if_downscaled`: once per run, when a video's best stream falls short of the
    /// upload's own size (the likeliest cause: a session quietly served as signed out).
    pub fn warn_if_downscaled(&mut self, note_card: &Dict, media: &[RedNoteMedia]) -> Result<(), JobError> {
        if self.warned_downscaled || !media.iter().any(|item| item.media_type == VIDEO_MEDIA_TYPE) {
            return Ok(());
        }
        let video_block = note_card.get("video");
        let (_, best_edge) = best_video_stream(field(field(video_block, &["media"]), &["stream"])).map_err(py_job_error)?;
        let source_edge = note_source_long_edge(video_block);
        if best_edge == 0 || source_edge == 0 || best_edge >= source_edge {
            return Ok(());
        }
        self.warned_downscaled = true;
        let note_id = text_or_empty(crate::pyval::first_truthy(note_card, &["noteId", "note_id"]));
        log().warning(format!(
            "RedNote note {note_id}: best stream is {best_edge}p of a {source_edge}p upload; the signed-in high-res streams may be gone"
        ));
        Ok(())
    }

    /// `_record_missing`: count one run that found this note gone (per run, not per
    /// sighting: `last_run` makes the second sighting in a run a no-op).
    pub async fn record_missing(&self, note_id: &str, run_id: &str) -> Result<(), JobError> {
        sqlx::query(
            "INSERT INTO rednote_missing (note_id, runs, last_run) VALUES ($1, 1, $2)
             ON CONFLICT (note_id) DO UPDATE
             SET runs = rednote_missing.runs + 1, last_run = excluded.last_run, last_seen_at = CURRENT_TIMESTAMP
             WHERE rednote_missing.last_run <> excluded.last_run;",
        )
        .bind(note_id)
        .bind(run_id)
        .execute(self.db.pool())
        .await
        .map_err(sql_error)?;
        log().info(format!(
            "RedNote note {note_id} is gone; it is retried until {MISSING_RUNS_BEFORE_RETIRING} runs agree"
        ));
        Ok(())
    }

    /// `_clear_missing`: forget a note that read fine.
    pub async fn clear_missing(&self, note_id: &str) -> Result<(), JobError> {
        sqlx::query("DELETE FROM rednote_missing WHERE note_id = $1;")
            .bind(note_id)
            .execute(self.db.pool())
            .await
            .map_err(sql_error)?;
        Ok(())
    }

    /// `_retired_note_ids`: notes enough separate runs have found gone.
    pub async fn retired_note_ids(&self, note_ids: &[String]) -> Result<HashSet<String>, JobError> {
        if note_ids.is_empty() {
            return Ok(HashSet::new());
        }
        let rows = sqlx::query("SELECT note_id FROM rednote_missing WHERE runs >= $1 AND note_id = ANY($2);")
            .bind(MISSING_RUNS_BEFORE_RETIRING)
            .bind(note_ids)
            .fetch_all(self.db.pool())
            .await
            .map_err(sql_error)?;
        rows.iter()
            .map(|row| row.try_get::<String, _>("note_id").map_err(sql_error))
            .collect()
    }

    async fn notify_summary(&self, downloaded: u64, resolved: u64, pending: usize) {
        let mut payload = Dict::new();
        payload.insert("downloaded", Value::Int(i128::from(downloaded)));
        payload.insert("resolved", Value::Int(i128::from(resolved)));
        payload.insert("pending", Value::Int(i128::try_from(pending).unwrap_or(i128::MAX)));
        let notification = NewNotification {
            kind: "summary".to_owned(),
            source: JOB_KEY.to_owned(),
            header: "RedNote".to_owned(),
            title: "Update completed".to_owned(),
            body: format!("Downloaded {downloaded} files, {resolved} of them newly liked."),
            payload: Some(payload),
            ..NewNotification::default()
        };
        if let Err(error) = self.notifier.enqueue(notification).await {
            log().warning(format!("Failed to enqueue rednote summary notification: {error}"));
        }
    }

    /// `_read_account`: everything that needs the browser, in one session held open as
    /// briefly as possible; the browser is closed whatever happens.
    async fn read_account(&mut self) -> Result<u64, JobError> {
        let mut browser = (self.browser_factory)().map_err(|error| {
            if error.class == "ProxyConfigurationError" {
                rednote_error(error.message, "rednote:proxy")
            } else {
                JobError::new(error.class, error.message)
            }
        })?;
        let result = async {
            browser.start().await.map_err(|error| browser_error(&error))?;
            self.prepare_session(browser.as_mut()).await?;
            let resolved = self.crawl(browser.as_mut()).await?;
            self.refresh_stale_media(browser.as_mut()).await?;
            Ok(resolved)
        }
        .await;
        browser.aclose().await;
        result
    }

    /// `update()`: one run.
    pub async fn update(&mut self) -> Result<(), JobError> {
        let missing = self.config.validate_runnable();
        if !missing.is_empty() {
            log().warning(format!("RedNote is not configured (missing {}), skip update", missing.join(", ")));
            return Ok(());
        }
        for directory in [
            Some(&self.config.path),
            Some(&self.config.profile_path),
            self.config.video_path.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            let directory = directory.as_str().to_owned();
            tokio::fs::create_dir_all(&directory)
                .await
                .map_err(|error| os_error(&error, &directory))?;
        }

        // One writer per profile: a Chromium user-data-dir is single-owner, and the
        // worker, the API's 登录态 login and a manual run are all plausible claimants.
        let lock_name = fav_browser::profile::rednote_profile_lock_name(self.config.profile_path.as_path());
        let Some(lock) = self
            .db
            .try_advisory_lock(&lock_name)
            .await
            .map_err(|error| crate::db_job_error(&error))?
        else {
            log().warning("The RedNote browser profile is in use by another run or a 登录态 login; skip this one");
            return Ok(());
        };
        let read = self.read_account().await;
        if let Err(error) = &read
            && is_rednote_error(error)
            && error.notification_dedupe_key() == Some("rednote:login")
        {
            self.record_signed_out(error.message()).await;
        }
        lock.release().await;
        let resolved = read?;

        // Outside the lock, with the browser gone: plain CDN traffic.
        let pending = self.pending_media().await?;
        let downloaded = self.download_pending(&pending).await?;
        log().info(format!(
            "RedNote resolved {resolved} new files and downloaded {downloaded} of {} pending",
            pending.len()
        ));
        if downloaded > 0 {
            self.notify_summary(downloaded, resolved, pending.len()).await;
        }
        Ok(())
    }
}

/// `secrets.token_hex(8)`.
fn run_id() -> String {
    crate::nikke_layer_metadata::random_hex(8)
}

fn media_index(index: i64) -> Result<i32, JobError> {
    i32::try_from(index).map_err(|_| JobError::new("NumericValueOutOfRange", "integer out of range"))
}

/// Removes a `.part` file unless the download finished.
struct PartGuard(Option<std::path::PathBuf>);

impl PartGuard {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for PartGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

// ---------- the job ----------

struct RedNoteJob {
    ctx: SourceContext,
}

impl Source for RedNoteJob {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self.ctx.settings.load().await.map_err(|error| crate::settings_job_error(&error))?;
            let config = settings.web.rednote.clone();
            let notifier: SharedNotifier = Arc::new(OutboxNotifier(Outbox::new(self.ctx.db.clone(), Arc::clone(&self.ctx.settings))));
            let client = cdn_client(&config)?;
            let factory = chromium_factory(&config);
            let mut source = RedNote::new(self.ctx.db.clone(), notifier, config, client, factory);
            source.update().await
        })
    }
}

/// Register the `rednote` job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run(JOB_KEY, move || Box::new(RedNoteJob { ctx: ctx.clone() }) as Box<dyn Source>);
}

#[cfg(test)]
mod tests;
