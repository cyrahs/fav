//! Huangguo (黄果短剧, huangguoai.com): every series in the chosen categories, saved episode
//! by episode for an Emby TV library.
//!
//! No Python origin. The site's front end reads a public JSON API, which this source reads
//! too; nothing needs an account. A run:
//!
//! 1. lists every chosen category (`/api/videos/category/<route>`, newest first, every page)
//!    and keeps the series whose tags hold none of `excluded_tags` and whose episode count
//!    moved since every available episode was saved (`huangguo_series.episode_count`);
//! 2. reads each kept series (`/api/videos/<id>`), writes `tvshow.nfo` and the poster once
//!    (the covers are AES-128-CBC encrypted with the key and IV the site's own front end
//!    decrypts them with, published in `crypto-image-protocol.js`), then downloads the
//!    missing episodes: `POST /api/videos/<id>/episodes/<ep>/playback` names an AES-128 HLS
//!    stream that `yt-dlp` saves. A run downloads at most `max_episodes_per_run` episodes;
//!    the rest wait for the next one.
//!
//! Layout: `<path>/<category name>/<series>/<series> S01E<ep>.<ext>` with `tvshow.nfo` and
//! `poster.<ext>` beside the episodes. Each series keeps the directory it was first given
//! (`huangguo_series.dir_name`), because the files move on to cloud storage and a renamed
//! series must keep landing in the same folder there.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes::Aes128;
use aes::cipher::{Array, BlockCipherDecrypt as _, KeyInit as _};
use fav_core::filename::sanitize;
use fav_core::logging::{self, Logger};
use fav_core::pyjson::{Dict, Value as PyValue};
use fav_core::pytext::py_strip;
use fav_core::settings;
use fav_db::{Db, Param};
use fav_net::download::move_file;
use fav_net::retry::{Sleeper, tokio_sleeper};
use fav_net::subprocess::{Runner, SystemRunner};
use fav_net::{ClientOptions, HttpClient, Request};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox};
use fav_runtime::source::{JobError, Registry, Source};
use serde_json::Value as Json;
use sqlx::Row as _;

use crate::notifier::{OutboxNotifier, SharedNotifier};
use crate::{SourceContext, execute, pyerr, settings_job_error};

const SOURCE: &str = "huangguo";
const HEADER: &str = "Huangguo";
/// What the site's own pages are fetched with.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// Series per listing page; the API accepts up to 100.
pub const PAGE_SIZE: i64 = 50;
/// A safety stop for a listing that never ends (about 1 900 series fill 38 pages today).
const MAX_LIST_PAGES: i64 = 200;
const REQUEST_ATTEMPTS: u32 = 3;
const DOWNLOAD_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: f64 = 5.0;
const DIR_NAME_BYTES: usize = 120;
const BLOCK_SIZE: usize = 16;
/// Where the site publishes the key its front end decrypts covers with.
pub const IMAGE_PROTOCOL_PATH: &str = "/static/web/js/plugins/crypto-image-protocol.js";
/// The site's category names, for when `/api/site/categories/video` cannot be read.
const KNOWN_CATEGORIES: [(&str, &str); 2] = [("ai-duanju", "短剧"), ("ai-manju", "漫剧")];

fn log() -> Logger {
    logging::get(SOURCE)
}

fn os_error(error: &std::io::Error, path: Option<&Path>) -> JobError {
    let os = fav_net::download::PyOsError::from_io(error, path);
    JobError::new(os.class, os.message)
}

fn huangguo_error(message: impl Into<String>) -> JobError {
    JobError::new("HuangguoError", message)
}

// ---------- the job ----------

/// The Huangguo job (`JOB_SPECS['huangguo']`).
pub struct Huangguo {
    ctx: SourceContext,
}

impl std::fmt::Debug for Huangguo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Huangguo").finish_non_exhaustive()
    }
}

impl Huangguo {
    pub fn new(ctx: SourceContext) -> Self {
        Self { ctx }
    }
}

/// Register the job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run(SOURCE, move || Box::new(Huangguo::new(ctx.clone())));
}

/// The client for the site's API (`proxy`) or its media (`proxy` when `proxy_media`).
pub fn client_options(proxy: &str) -> ClientOptions {
    ClientOptions {
        headers: vec![
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ("Accept-Language".to_owned(), "zh-CN,zh;q=0.9,en;q=0.8".to_owned()),
        ],
        timeout: Duration::from_mins(1),
        follow_redirects: true,
        proxy: (!proxy.is_empty()).then(|| proxy.to_owned()),
        max_keepalive_connections: 4,
        ..ClientOptions::default()
    }
}

/// The proxy media requests and `yt-dlp` use.
fn media_proxy(cfg: &settings::Huangguo) -> &str {
    if cfg.proxy_media { &cfg.proxy } else { "" }
}

impl Source for Huangguo {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self.ctx.settings.load().await.map_err(|error| settings_job_error(&error))?;
            let cfg = settings.web.huangguo.clone();
            let build =
                |proxy: &str| HttpClient::new(&client_options(proxy)).map_err(|error| JobError::new("ValueError", error.to_string()));
            let api = build(&cfg.proxy)?;
            let media = build(media_proxy(&cfg))?;
            let cache = tokio::task::spawn_blocking(|| tempfile::Builder::new().prefix("fav-huangguo-").tempdir())
                .await
                .map_err(|error| JobError::new("RuntimeError", error.to_string()))?
                .map_err(|error| os_error(&error, None))?;
            let outbox = Outbox::new(self.ctx.db.clone(), Arc::clone(&self.ctx.settings));
            let run = Run::new(
                cfg,
                api,
                media,
                self.ctx.db.clone(),
                Arc::new(OutboxNotifier(outbox)),
                Arc::new(SystemRunner),
                tokio_sleeper(),
                cache.path().to_path_buf(),
            );
            let result = run.update().await;
            // Removing the cache directory walks it; keep that off the async threads.
            let _ = tokio::task::spawn_blocking(move || drop(cache)).await;
            result
        })
    }
}

// ---------- the API's answers ----------

/// A series as a category listing shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub id: i64,
    pub title: String,
    pub tags: Vec<String>,
    pub episode_count: i64,
}

/// One episode of a series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    pub ep_num: i64,
    pub title: String,
    /// Seconds.
    pub duration: i64,
    /// `status == 1`: the site plays it.
    pub available: bool,
}

/// A series as `/api/videos/<id>` describes it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Series {
    pub id: i64,
    pub title: String,
    pub description: String,
    /// The encrypted cover's URL.
    pub cover: String,
    /// `YYYY-MM-DD HH:MM:SS`, Beijing time.
    pub created_at: String,
    pub author: String,
    pub tags: Vec<String>,
    pub score: Option<f64>,
    pub is_finished: bool,
    pub episodes: Vec<Episode>,
}

/// An integer the API may write as a number or as a numeric string (`"duration": "484"`).
fn int(value: Option<&Json>) -> Option<i64> {
    match value? {
        Json::Number(number) => number.as_i64(),
        Json::String(text) => py_strip(text).parse().ok(),
        _ => None,
    }
}

fn text(value: Option<&Json>) -> String {
    value
        .and_then(Json::as_str)
        .map(|text| py_strip(text).to_owned())
        .unwrap_or_default()
}

fn strings(value: Option<&Json>) -> Vec<String> {
    value
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Json::as_str)
                .map(|item| py_strip(item).to_owned())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// A listing page's series (those without an id are dropped) and its page count.
pub fn parse_listing(data: &Json) -> (Vec<Listed>, i64) {
    let items = data
        .get("items")
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(Listed {
                        id: int(item.get("id")).filter(|id| *id > 0)?,
                        title: text(item.get("title")),
                        tags: strings(item.get("tags")),
                        episode_count: int(item.get("episode_count")).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let pages = int(data.get("pagination").and_then(|pagination| pagination.get("pages"))).unwrap_or(1);
    (items, pages)
}

/// `/api/videos/<id>`'s `data`.
pub fn parse_series(data: &Json) -> Result<Series, JobError> {
    let id = int(data.get("id"))
        .filter(|id| *id > 0)
        .ok_or_else(|| huangguo_error("series without an id"))?;
    let title = text(data.get("title"));
    if title.is_empty() {
        return Err(huangguo_error(format!("series {id} has no title")));
    }
    let mut episodes: Vec<Episode> = data
        .get("episodes")
        .and_then(Json::as_array)
        .map(|episodes| {
            episodes
                .iter()
                .filter_map(|episode| {
                    Some(Episode {
                        ep_num: int(episode.get("ep_num")).filter(|ep| *ep > 0)?,
                        title: text(episode.get("title")),
                        duration: int(episode.get("duration")).unwrap_or(0),
                        available: int(episode.get("status")) == Some(1),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    episodes.sort_by_key(|episode| episode.ep_num);
    episodes.dedup_by_key(|episode| episode.ep_num);
    Ok(Series {
        id,
        title,
        description: data.get("description").and_then(Json::as_str).unwrap_or_default().trim().to_owned(),
        cover: text(data.get("cover")),
        created_at: text(data.get("created_at")),
        author: text(data.get("author").and_then(|author| author.get("name"))),
        tags: strings(data.get("tags")),
        score: data.get("score").and_then(Json::as_f64).filter(|score| *score > 0.0),
        is_finished: data.get("is_finished").and_then(Json::as_bool).unwrap_or(false),
        episodes,
    })
}

/// `/api/site/categories/video`: route to display name.
pub fn parse_categories(data: &Json) -> HashMap<String, String> {
    data.get("items")
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let route = text(item.get("route"));
                    let name = text(item.get("name"));
                    (!route.is_empty() && !name.is_empty()).then_some((route, name))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The playback answer's stream: `streams.main`, else `streams.hevc`.
pub fn parse_stream_url(data: &Json) -> Option<String> {
    let streams = data.get("streams")?;
    ["main", "hevc"]
        .into_iter()
        .map(|key| text(streams.get(key)))
        .find(|url| url.starts_with("http://") || url.starts_with("https://"))
}

/// How covers are stored: in the clear, or AES-128-CBC with this key and IV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageKey {
    Plain,
    Aes { key: [u8; BLOCK_SIZE], iv: [u8; BLOCK_SIZE] },
}

fn js_string(script: &str, name: &str) -> Option<String> {
    let pattern = regex::Regex::new(&format!(r#""{name}"\s*:\s*"([^"]*)""#)).ok()?;
    pattern.captures(script).map(|captures| captures[1].to_owned())
}

/// `window.CryptoImageProtocol = {"decryptEnabled": true, "decryptKey": "...", "decryptIV": "..."}`.
pub fn parse_image_protocol(script: &str) -> Result<ImageKey, JobError> {
    let enabled = regex::Regex::new(r#""decryptEnabled"\s*:\s*(true|false)"#)
        .ok()
        .and_then(|pattern| pattern.captures(script).map(|captures| &captures[1] == "true"))
        .ok_or_else(|| huangguo_error("crypto-image-protocol.js does not say whether covers are encrypted"))?;
    if !enabled {
        return Ok(ImageKey::Plain);
    }
    let bytes = |name: &str| -> Result<[u8; BLOCK_SIZE], JobError> {
        let value = js_string(script, name).ok_or_else(|| huangguo_error(format!("crypto-image-protocol.js has no {name}")))?;
        value
            .as_bytes()
            .try_into()
            .map_err(|_| huangguo_error(format!("crypto-image-protocol.js {name} is not 16 bytes")))
    };
    Ok(ImageKey::Aes {
        key: bytes("decryptKey")?,
        iv: bytes("decryptIV")?,
    })
}

/// The file extension of an image by its first bytes.
pub fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"GIF8") {
        Some("gif")
    } else {
        None
    }
}

/// A cover as the CDN serves it, decrypted: AES-128-CBC with PKCS#7 padding. Bytes that
/// already are an image are kept as they are.
pub fn decode_cover(raw: &[u8], key: &ImageKey) -> Result<(Vec<u8>, &'static str), String> {
    if let Some(extension) = image_extension(raw) {
        return Ok((raw.to_vec(), extension));
    }
    let ImageKey::Aes { key, iv } = key else {
        return Err("cover is not an image and covers are not encrypted".to_owned());
    };
    if raw.is_empty() || !raw.len().is_multiple_of(BLOCK_SIZE) {
        return Err(format!("encrypted cover of {} bytes is not whole AES blocks", raw.len()));
    }
    let cipher = Aes128::new(&Array::from(*key));
    let mut data = raw.to_vec();
    let mut previous = *iv;
    for block in data.chunks_exact_mut(BLOCK_SIZE) {
        let mut current = [0_u8; BLOCK_SIZE];
        current.copy_from_slice(block);
        let mut array = Array::from(current);
        cipher.decrypt_block(&mut array);
        for (out, (plain, chain)) in block.iter_mut().zip(array.iter().zip(previous)) {
            *out = plain ^ chain;
        }
        previous = current;
    }
    let pad = usize::from(data[data.len() - 1]);
    if !(1..=BLOCK_SIZE).contains(&pad) || data[data.len() - pad..].iter().any(|&byte| usize::from(byte) != pad) {
        return Err("decrypted cover has no valid PKCS#7 padding".to_owned());
    }
    data.truncate(data.len() - pad);
    let extension = image_extension(&data).ok_or_else(|| "decrypted cover is not an image".to_owned())?;
    Ok((data, extension))
}

// ---------- names and files ----------

/// The first tag of `tags` that `excluded` names.
pub fn excluded_tag<'a>(tags: &'a [String], excluded: &[String]) -> Option<&'a str> {
    tags.iter().find(|tag| excluded.contains(tag)).map(String::as_str)
}

/// A directory name from a title, `id-<id>` when nothing is left of it.
pub fn dir_name_for(title: &str, id: i64) -> String {
    let name = sanitize(title, DIR_NAME_BYTES);
    // A leading dot would hide the folder; trailing dots are dropped by some file systems.
    let name = name.trim_matches('.').trim().to_owned();
    if name.is_empty() { format!("id-{id}") } else { name }
}

/// The directory of a category, `未分类` when its name leaves nothing.
pub fn category_dir_name(category: &str) -> String {
    let name = sanitize(category, DIR_NAME_BYTES);
    let name = name.trim_matches('.').trim();
    if name.is_empty() { "未分类".to_owned() } else { name.to_owned() }
}

/// `<series> S01E<ep>.<ext>`, the episode naming Emby reads.
pub fn episode_file_name(dir_name: &str, ep_num: i64, extension: &str) -> String {
    format!("{dir_name} S01E{ep_num:02}.{extension}")
}

fn xml_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            // Characters XML 1.0 cannot carry at all.
            ch if (ch as u32) < 0x20 && !matches!(ch, '\t' | '\n' | '\r') => {}
            ch => escaped.push(ch),
        }
    }
    escaped
}

fn element(xml: &mut String, name: &str, value: &str) {
    if !value.is_empty() {
        let _ = writeln!(xml, "  <{name}>{}</{name}>", xml_escape(value));
    }
}

/// The date part of `YYYY-MM-DD HH:MM:SS`.
fn date_of(created_at: &str) -> Option<&str> {
    let date = created_at.get(..10)?;
    let bytes = date.as_bytes();
    let shaped = bytes.iter().enumerate().all(|(index, byte)| {
        if index == 4 || index == 7 {
            *byte == b'-'
        } else {
            byte.is_ascii_digit()
        }
    });
    shaped.then_some(date)
}

/// `tvshow.nfo` for Emby: the series' tags are its genres, the category a tag.
pub fn tvshow_nfo(series: &Series, category: &str) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<tvshow>\n");
    element(&mut xml, "title", &series.title);
    element(&mut xml, "plot", &series.description);
    if let Some(date) = date_of(&series.created_at) {
        element(&mut xml, "premiered", date);
        element(&mut xml, "year", &date[..4]);
    }
    element(&mut xml, "studio", &series.author);
    if let Some(score) = series.score {
        element(&mut xml, "rating", &score.to_string());
    }
    element(&mut xml, "status", if series.is_finished { "Ended" } else { "Continuing" });
    for tag in &series.tags {
        element(&mut xml, "genre", tag);
    }
    element(&mut xml, "tag", category);
    let _ = writeln!(xml, "  <uniqueid type=\"huangguo\" default=\"true\">{}</uniqueid>", series.id);
    xml.push_str("</tvshow>\n");
    xml
}

/// `yt-dlp` for one episode's HLS stream.
pub fn build_download_command(stream_url: &str, output_template: &str, referer: &str, proxy: &str) -> Vec<String> {
    let mut command: Vec<String> = [
        "yt-dlp",
        "-o",
        output_template,
        "--no-mtime",
        "--no-part",
        "--no-playlist",
        "-N",
        "8",
        "--retries",
        "15",
        "--fragment-retries",
        "15",
        "--socket-timeout",
        "30",
        "--referer",
        referer,
        "--add-header",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    command.push(format!("User-Agent:{USER_AGENT}"));
    if !proxy.is_empty() {
        command.extend(["--proxy".to_owned(), proxy.to_owned()]);
    }
    command.push(stream_url.to_owned());
    command
}

/// `E01`, or `E01–E05 (3)` for several (not necessarily consecutive) episodes.
fn episode_span(episodes: &[i64]) -> String {
    match (episodes.iter().min(), episodes.iter().max()) {
        (Some(first), Some(last)) if first == last => format!("E{first:02}"),
        (Some(first), Some(last)) => format!("E{first:02}–E{last:02} ({})", episodes.len()),
        _ => String::new(),
    }
}

/// Write through a `.part` file and rename it, so nothing that watches the directory (the
/// cloud upload) ever picks up half a file.
async fn write_atomic(path: &Path, bytes: Vec<u8>) -> Result<(), JobError> {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    tokio::fs::write(&part, bytes)
        .await
        .map_err(|error| os_error(&error, Some(&part)))?;
    tokio::fs::rename(&part, path).await.map_err(|error| os_error(&error, Some(path)))
}

async fn create_dir_all(dir: &Path) -> Result<(), JobError> {
    tokio::fs::create_dir_all(dir).await.map_err(|error| os_error(&error, Some(dir)))
}

/// Empty the directory.
async fn cleanup_dir(dirpath: &Path) -> Result<(), JobError> {
    let mut entries = match tokio::fs::read_dir(dirpath).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(os_error(&error, Some(dirpath))),
    };
    while let Some(entry) = entries.next_entry().await.map_err(|error| os_error(&error, Some(dirpath)))? {
        let path = entry.path();
        let is_dir = entry.file_type().await.map_err(|error| os_error(&error, Some(&path)))?.is_dir();
        let removed = if is_dir {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_file(&path).await
        };
        removed.map_err(|error| os_error(&error, Some(&path)))?;
    }
    Ok(())
}

/// The largest file `yt-dlp` left in `dirpath`.
async fn largest_file(dirpath: &Path) -> Result<Option<PathBuf>, JobError> {
    let mut largest: Option<(u64, PathBuf)> = None;
    let mut entries = tokio::fs::read_dir(dirpath)
        .await
        .map_err(|error| os_error(&error, Some(dirpath)))?;
    while let Some(entry) = entries.next_entry().await.map_err(|error| os_error(&error, Some(dirpath)))? {
        let metadata = entry.metadata().await.map_err(|error| os_error(&error, Some(&entry.path())))?;
        if metadata.is_file() && largest.as_ref().is_none_or(|(size, _)| metadata.len() > *size) {
            largest = Some((metadata.len(), entry.path()));
        }
    }
    Ok(largest.map(|(_, path)| path))
}

fn db_error(error: sqlx::Error) -> JobError {
    pyerr::db_error(&fav_db::DbError::Sqlx(error))
}

// ---------- the API ----------

/// A request failed in a way worth trying again (transport, 429, 5xx) or not.
enum Failure {
    Retryable(JobError),
    Fatal(JobError),
}

/// The site's JSON API, paced by `sleep_request_seconds`.
pub(crate) struct Api {
    http: HttpClient,
    host: String,
    sleeper: Sleeper,
    interval: f64,
    started: AtomicBool,
}

impl Api {
    pub(crate) fn new(http: HttpClient, host: &str, sleeper: Sleeper, interval: f64) -> Self {
        Self {
            http,
            host: host.trim_end_matches('/').to_owned(),
            sleeper,
            interval,
            started: AtomicBool::new(false),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.host)
    }

    fn get(&self, path: &str) -> Request {
        Request::get(self.url(path))
            .header("Accept", "application/json, text/plain, */*")
            .header("Referer", format!("{}/", self.host))
    }

    fn post(&self, path: &str) -> Request {
        Request::post(self.url(path), b"{}".to_vec())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/plain, */*")
            .header("Origin", self.host.clone())
            .header("Referer", format!("{}/", self.host))
    }

    /// Wait `interval` before every request but the run's first.
    async fn pace(&self) {
        if self.started.swap(true, Ordering::Relaxed) && self.interval > 0.0 {
            (self.sleeper)(self.interval).await;
        }
    }

    /// The body of a request, tried up to three times (waits 5 s, 10 s).
    async fn body(&self, path: &str, request: impl Fn() -> Request) -> Result<bytes::Bytes, JobError> {
        let mut attempt = 1;
        loop {
            self.pace().await;
            match self.body_once(request()).await {
                Ok(body) => return Ok(body),
                Err(Failure::Retryable(error)) if attempt < REQUEST_ATTEMPTS => {
                    let wait = RETRY_BASE_DELAY * f64::from(attempt);
                    log().warning(format!(
                        "Retrying Huangguo {path} ({attempt}/{REQUEST_ATTEMPTS}) in {wait:.0}s: {}",
                        error.formatted()
                    ));
                    (self.sleeper)(wait).await;
                    attempt += 1;
                }
                Err(Failure::Retryable(error) | Failure::Fatal(error)) => return Err(error),
            }
        }
    }

    async fn body_once(&self, request: Request) -> Result<bytes::Bytes, Failure> {
        let response = self
            .http
            .send(request)
            .await
            .map_err(|error| Failure::Retryable(pyerr::transport_error(&error)))?;
        let status = response.status();
        if let Err(error) = response.error_for_status() {
            let error = pyerr::status_error(&error);
            return Err(if status == 429 || status >= 500 {
                Failure::Retryable(error)
            } else {
                Failure::Fatal(error)
            });
        }
        response
            .bytes()
            .await
            .map_err(|error| Failure::Retryable(pyerr::transport_error(&error)))
    }

    /// An API answer's `data`, after its `status` said 1.
    async fn json(&self, path: &str, request: impl Fn() -> Request) -> Result<Json, JobError> {
        let body = self.body(path, request).await?;
        let answer: Json = serde_json::from_slice(&body)
            .map_err(|error| JobError::new("ValueError", format!("Huangguo {path} did not answer JSON: {error}")))?;
        if int(answer.get("status")) != Some(1) {
            let message = text(answer.get("msg"));
            return Err(huangguo_error(format!(
                "Huangguo {path} refused: {}",
                if message.is_empty() { "status is not 1" } else { &message }
            )));
        }
        Ok(answer.get("data").cloned().unwrap_or(Json::Null))
    }

    pub(crate) async fn categories(&self) -> Result<HashMap<String, String>, JobError> {
        let path = "/api/site/categories/video";
        Ok(parse_categories(&self.json(path, || self.get(path)).await?))
    }

    pub(crate) async fn listing_page(&self, route: &str, page: i64) -> Result<(Vec<Listed>, i64), JobError> {
        let path = format!("/api/videos/category/{route}?page={page}&size={PAGE_SIZE}&sort=new");
        Ok(parse_listing(&self.json(&path, || self.get(&path)).await?))
    }

    pub(crate) async fn series(&self, id: i64) -> Result<Series, JobError> {
        let path = format!("/api/videos/{id}");
        parse_series(&self.json(&path, || self.get(&path)).await?)
    }

    pub(crate) async fn stream_url(&self, id: i64, ep_num: i64) -> Result<String, JobError> {
        let path = format!("/api/videos/{id}/episodes/{ep_num}/playback");
        let data = self.json(&path, || self.post(&path)).await?;
        parse_stream_url(&data).ok_or_else(|| huangguo_error(format!("Huangguo {path} named no stream")))
    }

    pub(crate) async fn image_key(&self) -> Result<ImageKey, JobError> {
        let body = self
            .body(IMAGE_PROTOCOL_PATH, || {
                Request::get(self.url(IMAGE_PROTOCOL_PATH)).header("Referer", format!("{}/", self.host))
            })
            .await?;
        parse_image_protocol(&String::from_utf8_lossy(&body))
    }
}

// ---------- one run ----------

/// What a run's downloads came to.
#[derive(Debug, Default)]
struct Tally {
    attempted: usize,
    saved: usize,
    failed_series: usize,
    last_error: Option<String>,
}

/// The stored side of a series.
struct Stored {
    dir_name: String,
    category: String,
    metadata_written: bool,
}

/// One run: the settings snapshot and everything the run talks to.
pub(crate) struct Run {
    cfg: settings::Huangguo,
    api: Api,
    media: HttpClient,
    db: Db,
    notifier: SharedNotifier,
    runner: Arc<dyn Runner>,
    sleeper: Sleeper,
    cache_dir: PathBuf,
    image_key: Mutex<Option<ImageKey>>,
}

impl Run {
    #[allow(clippy::too_many_arguments)] // Everything a run talks to, injected.
    pub(crate) fn new(
        cfg: settings::Huangguo,
        api: HttpClient,
        media: HttpClient,
        db: Db,
        notifier: SharedNotifier,
        runner: Arc<dyn Runner>,
        sleeper: Sleeper,
        cache_dir: PathBuf,
    ) -> Self {
        let api = Api::new(api, &cfg.host, Arc::clone(&sleeper), cfg.sleep_request_seconds);
        Self {
            cfg,
            api,
            media,
            db,
            notifier,
            runner,
            sleeper,
            cache_dir,
            image_key: Mutex::new(None),
        }
    }

    fn series_url(&self, id: i64) -> String {
        format!("{}/video/{id}/", self.api.host)
    }

    /// Category routes to names; the known ones when the site's list cannot be read.
    async fn category_names(&self) -> HashMap<String, String> {
        let mut names: HashMap<String, String> = KNOWN_CATEGORIES
            .iter()
            .map(|(route, name)| ((*route).to_owned(), (*name).to_owned()))
            .collect();
        match self.api.categories().await {
            Ok(site) => names.extend(site),
            Err(error) => log().warning(format!("Could not read the Huangguo category names: {}", error.formatted())),
        }
        names
    }

    /// Every series of a category, newest first.
    async fn list_category(&self, route: &str) -> Result<Vec<Listed>, JobError> {
        let mut listed = Vec::new();
        let mut page = 1;
        loop {
            let (items, pages) = self.api.listing_page(route, page).await?;
            let empty = items.is_empty();
            listed.extend(items);
            if empty || page >= pages {
                break;
            }
            if page >= MAX_LIST_PAGES {
                log().warning(format!(
                    "Huangguo category {route} lists more than {MAX_LIST_PAGES} pages; stopping there"
                ));
                break;
            }
            page += 1;
        }
        Ok(listed)
    }

    /// Series id to the episode count it listed when every available episode was saved.
    async fn checked_counts(&self) -> Result<HashMap<i64, i64>, JobError> {
        let rows = execute(&self.db, "SELECT video_id, episode_count FROM huangguo_series", Vec::new()).await?;
        let mut counts = HashMap::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row.try_get("video_id").map_err(db_error)?;
            let count: i32 = row.try_get("episode_count").map_err(db_error)?;
            counts.insert(id, i64::from(count));
        }
        Ok(counts)
    }

    async fn downloaded_episodes(&self, id: i64) -> Result<HashSet<i64>, JobError> {
        let rows = execute(&self.db, "SELECT ep_num FROM huangguo WHERE video_id = $1", vec![Param::from(id)]).await?;
        rows.iter()
            .map(|row| row.try_get::<i32, _>("ep_num").map(i64::from).map_err(db_error))
            .collect()
    }

    /// Insert or refresh the series' row; a new one gets its directory here, with the id
    /// appended when another series of the category already has the name.
    async fn store_series(&self, series: &Series, category: &str) -> Result<Stored, JobError> {
        let mut dir_name = dir_name_for(&series.title, series.id);
        let taken = execute(
            &self.db,
            "SELECT 1 FROM huangguo_series WHERE category = $1 AND dir_name = $2 AND video_id <> $3",
            vec![Param::from(category), Param::from(dir_name.as_str()), Param::from(series.id)],
        )
        .await?;
        if !taken.is_empty() {
            dir_name = format!("{dir_name} [{}]", series.id);
        }
        let rows = execute(
            &self.db,
            "INSERT INTO huangguo_series (video_id, category, title, author, tags, dir_name) VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (video_id) DO UPDATE SET title = EXCLUDED.title, author = EXCLUDED.author, tags = EXCLUDED.tags, \
             updated_at = CURRENT_TIMESTAMP \
             RETURNING dir_name, category, metadata_written",
            vec![
                Param::from(series.id),
                Param::from(category),
                Param::from(series.title.as_str()),
                Param::from(series.author.as_str()),
                Param::from(series.tags.join(",")),
                Param::from(dir_name.as_str()),
            ],
        )
        .await?;
        let row = rows.first().ok_or_else(|| huangguo_error("storing a series returned no row"))?;
        Ok(Stored {
            dir_name: row.try_get("dir_name").map_err(db_error)?,
            category: row.try_get("category").map_err(db_error)?,
            metadata_written: row.try_get("metadata_written").map_err(db_error)?,
        })
    }

    async fn mark_checked(&self, id: i64, episode_count: i64) -> Result<(), JobError> {
        execute(
            &self.db,
            "UPDATE huangguo_series SET episode_count = $1, last_error = '', updated_at = CURRENT_TIMESTAMP WHERE video_id = $2",
            vec![Param::from(episode_count), Param::from(id)],
        )
        .await
        .map(drop)
    }

    async fn mark_metadata_written(&self, id: i64) -> Result<(), JobError> {
        execute(
            &self.db,
            "UPDATE huangguo_series SET metadata_written = TRUE, updated_at = CURRENT_TIMESTAMP WHERE video_id = $1",
            vec![Param::from(id)],
        )
        .await
        .map(drop)
    }

    /// Remember a failure on the series' row (a series never stored yet has none).
    async fn record_error(&self, id: i64, error: &JobError) -> Result<(), JobError> {
        execute(
            &self.db,
            "UPDATE huangguo_series SET last_error = $1, updated_at = CURRENT_TIMESTAMP WHERE video_id = $2",
            vec![Param::from(error.formatted()), Param::from(id)],
        )
        .await
        .map(drop)
    }

    async fn image_key(&self) -> Result<ImageKey, JobError> {
        if let Some(key) = self.image_key.lock().expect("image key").clone() {
            return Ok(key);
        }
        let key = self.api.image_key().await?;
        *self.image_key.lock().expect("image key") = Some(key.clone());
        Ok(key)
    }

    async fn poster(&self, series: &Series) -> Result<(Vec<u8>, &'static str), JobError> {
        let key = self.image_key().await?;
        let request = Request::get(series.cover.clone()).header("Referer", format!("{}/", self.api.host));
        let response = self.media.send(request).await.map_err(|error| pyerr::transport_error(&error))?;
        response.error_for_status().map_err(|error| pyerr::status_error(&error))?;
        let raw = response.bytes().await.map_err(|error| pyerr::transport_error(&error))?;
        decode_cover(&raw, &key).map_err(huangguo_error)
    }

    /// `tvshow.nfo` and the poster; written once, a failure is logged and tried again on
    /// the series' next download.
    async fn write_metadata(&self, series: &Series, category: &str, series_dir: &Path) -> Result<(), JobError> {
        write_atomic(&series_dir.join("tvshow.nfo"), tvshow_nfo(series, category).into_bytes()).await?;
        if !series.cover.is_empty() {
            let (bytes, extension) = self.poster(series).await?;
            write_atomic(&series_dir.join(format!("poster.{extension}")), bytes).await?;
        }
        self.mark_metadata_written(series.id).await
    }

    /// One episode: its stream, `yt-dlp` up to three times (waits 5 s, 10 s), the file moved
    /// into the series' directory.
    async fn download_episode(&self, series: &Series, ep_num: i64, series_dir: &Path, dir_name: &str) -> Result<PathBuf, JobError> {
        let work_dir = self.cache_dir.join("videos");
        create_dir_all(&work_dir).await?;
        let template = work_dir
            .join(format!("{}-{ep_num}.%(ext)s", series.id))
            .to_string_lossy()
            .into_owned();
        let referer = format!("{}/", self.api.host);
        let mut attempt = 1;
        loop {
            cleanup_dir(&work_dir).await?;
            // The stream's signature expires, so every attempt asks for a fresh one.
            let stream_url = self.api.stream_url(series.id, ep_num).await?;
            let command = build_download_command(&stream_url, &template, &referer, media_proxy(&self.cfg));
            let output = self
                .runner
                .run(command)
                .await
                .map_err(|error| os_error(&error, Some(Path::new("yt-dlp"))))?;
            if output.returncode == 0 {
                break;
            }
            let message = [py_strip(&output.stderr), py_strip(&output.stdout)]
                .into_iter()
                .find(|text| !text.is_empty())
                .map_or_else(|| format!("yt-dlp exited with code {}", output.returncode), str::to_owned);
            if attempt >= DOWNLOAD_ATTEMPTS {
                return Err(JobError::new("DownloadError", message));
            }
            let wait = RETRY_BASE_DELAY * f64::from(attempt);
            log().debug(format!(
                "Retrying Huangguo {} E{ep_num:02} ({attempt}/{DOWNLOAD_ATTEMPTS}) in {wait:.0}s: {message}",
                series.id
            ));
            (self.sleeper)(wait).await;
            attempt += 1;
        }
        let downloaded = largest_file(&work_dir)
            .await?
            .ok_or_else(|| JobError::new("DownloadError", format!("No output file generated for {} E{ep_num:02}", series.id)))?;
        let extension = downloaded
            .extension()
            .map_or_else(|| "mp4".to_owned(), |ext| ext.to_string_lossy().into_owned());
        let target = series_dir.join(episode_file_name(dir_name, ep_num, &extension));
        let (source, destination) = (downloaded.clone(), target.clone());
        tokio::task::spawn_blocking(move || move_file(&source, &destination))
            .await
            .map_err(|error| JobError::new("RuntimeError", error.to_string()))?
            .map_err(|error| os_error(&error, Some(&downloaded)))?;
        Ok(target)
    }

    async fn record_episode(&self, series: &Series, episode: &Episode, category: &str, path: &Path) -> Result<(), JobError> {
        execute(
            &self.db,
            "INSERT INTO huangguo (video_id, ep_num, title, series_title, category, duration, local_path) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (video_id, ep_num) DO UPDATE SET local_path = EXCLUDED.local_path",
            vec![
                Param::from(series.id),
                Param::from(episode.ep_num),
                Param::from(episode.title.as_str()),
                Param::from(series.title.as_str()),
                Param::from(category),
                Param::from(episode.duration),
                Param::from(path.to_string_lossy().into_owned()),
            ],
        )
        .await
        .map(drop)
    }

    /// One notification per series and run; a failure to queue is logged, never raised.
    async fn notify(&self, series: &Series, category: &str, episodes: &[i64]) {
        let mut payload = Dict::new();
        payload.insert("video_id", PyValue::Int(i128::from(series.id)));
        payload.insert(
            "episodes",
            PyValue::List(episodes.iter().map(|ep| PyValue::Int(i128::from(*ep))).collect()),
        );
        let notification = NewNotification {
            kind: "download_completed".to_owned(),
            source: SOURCE.to_owned(),
            header: HEADER.to_owned(),
            title: series.title.clone(),
            body: format!("{category} | {}", episode_span(episodes)),
            link_url: self.series_url(series.id),
            payload: Some(payload),
            ..NewNotification::default()
        };
        if let Err(error) = self.notifier.enqueue(notification).await {
            log().warning(format!("Failed to enqueue Huangguo notification for {}: {error}", series.id));
        }
    }

    /// Read one series and download what it is missing, within `budget`.
    async fn sync_series(&self, listed: &Listed, category: &str, budget: &mut usize, tally: &mut Tally) -> Result<(), JobError> {
        let series = self.api.series(listed.id).await?;
        if let Some(tag) = excluded_tag(&series.tags, &self.cfg.excluded_tags) {
            log().info(format!("Skipping Huangguo series {} ({}): tagged {tag}", series.id, series.title));
            return Ok(());
        }
        let stored = self.store_series(&series, category).await?;
        let done = self.downloaded_episodes(series.id).await?;
        let pending: Vec<&Episode> = series
            .episodes
            .iter()
            .filter(|episode| episode.available && !done.contains(&episode.ep_num))
            .collect();
        if pending.is_empty() {
            return self.mark_checked(series.id, listed.episode_count).await;
        }
        let series_dir = Path::new(self.cfg.path.as_str())
            .join(category_dir_name(&stored.category))
            .join(&stored.dir_name);
        create_dir_all(&series_dir).await?;
        if !stored.metadata_written
            && let Err(error) = self.write_metadata(&series, &stored.category, &series_dir).await
        {
            log().warning(format!(
                "Could not write the Huangguo metadata of {} ({}): {}",
                series.id,
                series.title,
                error.formatted()
            ));
        }
        let mut saved = Vec::new();
        let mut failed = false;
        for episode in &pending {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            tally.attempted += 1;
            log().info(format!(
                "Downloading Huangguo {} E{:02} ({})",
                series.id, episode.ep_num, series.title
            ));
            match self.download_episode(&series, episode.ep_num, &series_dir, &stored.dir_name).await {
                Ok(path) => {
                    self.record_episode(&series, episode, &stored.category, &path).await?;
                    tally.saved += 1;
                    saved.push(episode.ep_num);
                    log().notice(format!(
                        "Saved {}",
                        path.file_name().map(|name| name.to_string_lossy()).unwrap_or_default()
                    ));
                }
                Err(error) => {
                    failed = true;
                    log().error(format!(
                        "Failed to download Huangguo {} E{:02}: {}",
                        series.id,
                        episode.ep_num,
                        error.formatted()
                    ));
                    tally.last_error = Some(error.formatted());
                    self.record_error(series.id, &error).await?;
                }
            }
        }
        if !failed && saved.len() == pending.len() {
            self.mark_checked(series.id, listed.episode_count).await?;
        }
        if !saved.is_empty() {
            self.notify(&series, &stored.category, &saved).await;
        }
        Ok(())
    }

    /// `update()`.
    pub(crate) async fn update(&self) -> Result<(), JobError> {
        if self.cfg.categories.is_empty() {
            log().info("No Huangguo categories configured");
            return Ok(());
        }
        let names = self.category_names().await;
        let checked = self.checked_counts().await?;
        let mut seen = HashSet::new();
        let mut queue: Vec<(Listed, String)> = Vec::new();
        let (mut listed_total, mut excluded) = (0_usize, 0_usize);
        for route in &self.cfg.categories {
            let category = names.get(route).cloned().unwrap_or_else(|| route.clone());
            for item in self.list_category(route).await? {
                if !seen.insert(item.id) {
                    continue;
                }
                listed_total += 1;
                if excluded_tag(&item.tags, &self.cfg.excluded_tags).is_some() {
                    excluded += 1;
                    continue;
                }
                if item.episode_count > 0 && checked.get(&item.id) == Some(&item.episode_count) {
                    continue;
                }
                queue.push((item, category.clone()));
            }
        }
        log().info(format!(
            "Huangguo listed {listed_total} series ({excluded} excluded by tag); {} may have new episodes",
            queue.len()
        ));
        if queue.is_empty() {
            return Ok(());
        }
        create_dir_all(Path::new(self.cfg.path.as_str())).await?;
        let mut budget = usize::try_from(self.cfg.max_episodes_per_run).unwrap_or(usize::MAX);
        let mut tally = Tally::default();
        for (index, (item, category)) in queue.iter().enumerate() {
            if budget == 0 {
                log().info(format!(
                    "Huangguo reached max_episodes_per_run ({}); {} series wait for the next run",
                    self.cfg.max_episodes_per_run,
                    queue.len() - index
                ));
                break;
            }
            if let Err(error) = self.sync_series(item, category, &mut budget, &mut tally).await {
                tally.failed_series += 1;
                log().error(format!(
                    "Failed to sync Huangguo series {} ({}): {}",
                    item.id,
                    item.title,
                    error.formatted()
                ));
                tally.last_error = Some(error.formatted());
                self.record_error(item.id, &error).await?;
            }
        }
        log().info(format!(
            "Huangguo saved {} of {} episodes ({} series failed)",
            tally.saved, tally.attempted, tally.failed_series
        ));
        if tally.saved == 0 && tally.attempted + tally.failed_series > 0 {
            return Err(JobError::new(
                "DownloadError",
                format!(
                    "Huangguo saved nothing: {} episodes and {} series failed; last error: {}",
                    tally.attempted,
                    tally.failed_series,
                    tally.last_error.unwrap_or_default()
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
