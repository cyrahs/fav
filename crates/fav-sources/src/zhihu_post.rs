//! Zhihu answers, column articles and pins shared as links, read through TikHub.
//!
//! Python origin: `src/web/zhihu_post.py`.
//!
//! Zhihu answers the cluster's addresses with HTTP 403 for every page and for its
//! `/api/v4` JSON, so the post itself is looked up through TikHub (`fetch_answer_detail`,
//! `fetch_column_article_detail` and `fetch_pin_detail`, each billed), with the key in
//! `services.tikhub` that 视频号 lookups use too.
//!
//! Everything after that is public. An answer's or article's `content` is HTML: each
//! picture is an `<img>` whose `data-original` is the full-size upload on `*.zhimg.com`,
//! and each video an `<a class="video-box">` carrying the video's `data-lens-id`. A pin's
//! `content` is a list of items instead. A video id is resolved with
//! `lens.zhihu.com/api/v4/videos/<id>`, which answers without a login with signed MP4
//! URLs for each quality; the widest one is kept. Those URLs expire within the hour, so a
//! video is downloaded as soon as it is resolved.

use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use fav_core::pyhtml::unescape;
use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::{is_py_space, py_isdigit, py_strip};
use fav_core::pyurl::unquote;
use fav_net::Request;

use crate::post_links::{Fetched, PostClient, is_http_error};
use crate::pydecode::{PyError, json_from_bytes, py_int, urlencode};
use crate::pyre::{compile, strip_tags};
use crate::pyval::{as_str, first_truthy, float, fromtimestamp_utc, py_str, split, str_or_empty, unique};
use crate::wechat_article::{ArticlePage, PostVideo};

/// TikHub's answers to a missing, invalid or spent key.
const TIKHUB_ACCOUNT_STATUSES: [u16; 3] = [401, 402, 403];
/// How TikHub's message starts when it could not carry a request out at all.
const TIKHUB_REQUEST_FAILED: &str = "Request failed. Please retry";
pub const LENS_API: &str = "https://lens.zhihu.com/api/v4/videos/";
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0 Safari/537.36";
const PIN_TITLE_BYTES: usize = 80;
/// `sys.get_int_max_str_digits()`.
const INT_MAX_STR_DIGITS: usize = 4300;

static ANSWER_PATH: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:/(?:question/\d+/)?answer/(\d+))$", false, false));
static ARTICLE_PATH: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:/p/(\d+))$", false, false));
static PIN_PATH: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:/pin/(\d+))$", false, false));
static REDIRECT_TARGET: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"(?:^|&)target=([^&]+)", false, false));
static VIDEO_BOX: LazyLock<regex::Regex> =
    LazyLock::new(|| compile(r#"<a\b[^>]*\bclass="[^"]*\bvideo-box\b[^"]*"[^>]*>.*?</a>"#, true, true));
static LENS_ID: LazyLock<regex::Regex> = LazyLock::new(|| compile(r#"\bdata-lens-id="(\d+)""#, false, false));
static VIDEO_TARGET: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"zhihu\.com/video/(\d+)", false, false));
static VIDEO_POSTER: LazyLock<regex::Regex> = LazyLock::new(|| compile(r#"\bdata-poster="([^"]+)""#, false, false));
static IMG: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<img\b[^>]*>", true, false));
/// `_IMG_ATTRS`, each as `_attr`'s pattern.
static IMG_ATTRS: LazyLock<[regex::Regex; 3]> = LazyLock::new(|| {
    ["data-original", "data-actualsrc", "src"].map(|name| compile(&format!(r#"\b{}="([^"]*)""#, regex::escape(name)), false, false))
});

/// `ZhihuKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZhihuKind {
    Answer,
    Article,
    Pin,
}

impl ZhihuKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Article => "article",
            Self::Pin => "pin",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "answer" => Some(Self::Answer),
            "article" => Some(Self::Article),
            "pin" => Some(Self::Pin),
            _ => None,
        }
    }

    /// `_TIKHUB_ENDPOINTS[kind]`: the detail path and its id parameter.
    fn endpoint(self) -> (&'static str, &'static str) {
        match self {
            Self::Answer => ("/api/v1/zhihu/web/fetch_answer_detail", "answer_id"),
            Self::Article => ("/api/v1/zhihu/web/fetch_column_article_detail", "article_id"),
            Self::Pin => ("/api/v1/zhihu/web/fetch_pin_detail", "pin_id"),
        }
    }
}

/// `ZhihuLink`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZhihuLink {
    pub kind: ZhihuKind,
    pub id: String,
}

impl ZhihuLink {
    pub fn new(kind: ZhihuKind, id: impl Into<String>) -> Self {
        Self { kind, id: id.into() }
    }

    /// `ZhihuLink.url`.
    pub fn url(&self) -> String {
        match self.kind {
            ZhihuKind::Answer => format!("https://www.zhihu.com/answer/{}", self.id),
            ZhihuKind::Article => format!("https://zhuanlan.zhihu.com/p/{}", self.id),
            ZhihuKind::Pin => format!("https://www.zhihu.com/pin/{}", self.id),
        }
    }
}

/// `ZhihuVideo`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZhihuVideo {
    pub id: String,
    /// A play URL the post itself carried (pins do); used when lens has none.
    pub fallback_url: String,
    pub cover_url: String,
}

/// `ZhihuPost`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZhihuPost {
    pub link: ZhihuLink,
    pub title: String,
    pub author: String,
    pub published_at: Option<DateTime<Utc>>,
    pub images: Vec<String>,
    pub videos: Vec<ZhihuVideo>,
}

/// Why a Zhihu post could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ZhihuError {
    /// `ZhihuError`: retried later.
    #[error("{0}")]
    Zhihu(String),
    /// `ZhihuAccountError`: TikHub refused the key, settings to fix rather than a failed try.
    #[error("{0}")]
    Account(String),
    /// TikHub itself is failing (it did not answer, answered with a server error, or with
    /// its own "Request failed" wrapper): an outage to wait out, which says nothing about
    /// the post (DIVERGENCES.md D185).
    #[error("{0}")]
    Outage(String),
    /// Any other exception Python lets through.
    #[error("{}: {}", .0.class, .0.message)]
    Other(PyError),
}

impl From<PyError> for ZhihuError {
    fn from(error: PyError) -> Self {
        Self::Other(error)
    }
}

fn is_zhihu_host(host: &str) -> bool {
    host == "zhihu.com" || host.ends_with(".zhihu.com")
}

/// `parse_zhihu_url`: the answer, column article or pin a Zhihu URL points at, or `None`.
pub fn parse_zhihu_url(url: &str) -> Result<Option<ZhihuLink>, PyError> {
    let parts = split(py_strip(url))?;
    let host = parts.hostname().unwrap_or_default();
    if !matches!(parts.scheme.as_str(), "http" | "https") {
        return Ok(None);
    }
    if host == "link.zhihu.com" {
        // Zhihu's own redirector wraps the real target in ?target=.
        return match REDIRECT_TARGET.captures(&parts.query) {
            Some(target) => parse_zhihu_url(&unquote(&target[1])),
            None => Ok(None),
        };
    }
    if !is_zhihu_host(&host) {
        return Ok(None);
    }
    let path = parts.path.trim_end_matches('/');
    let found = |pattern: &regex::Regex| pattern.captures(path).map(|found| found[1].to_owned());
    if host == "zhuanlan.zhihu.com" {
        return Ok(found(&ARTICLE_PATH).map(|id| ZhihuLink::new(ZhihuKind::Article, id)));
    }
    if let Some(id) = found(&ANSWER_PATH) {
        return Ok(Some(ZhihuLink::new(ZhihuKind::Answer, id)));
    }
    Ok(found(&PIN_PATH).map(|id| ZhihuLink::new(ZhihuKind::Pin, id)))
}

fn is_zhimg(url: &str) -> Result<bool, PyError> {
    let parts = split(url)?;
    let host = parts.hostname().unwrap_or_default();
    Ok(matches!(parts.scheme.as_str(), "http" | "https") && (host == "zhimg.com" || host.ends_with(".zhimg.com")))
}

/// `_attr`: an attribute's value in a tag, unescaped and stripped.
fn attr(tag: &str, pattern: &regex::Regex) -> String {
    pattern
        .captures(tag)
        .map(|found| py_strip(&unescape(&found[1])).to_owned())
        .unwrap_or_default()
}

/// `content_images`: full-size picture URLs in an answer's or article's HTML, in order,
/// without video posters.
pub fn content_images(content: &str) -> Result<Vec<String>, PyError> {
    let body = VIDEO_BOX.replace_all(content, regex::NoExpand(""));
    let mut images = Vec::new();
    for tag in IMG.find_iter(&body) {
        for pattern in IMG_ATTRS.iter() {
            let value = attr(tag.as_str(), pattern);
            if is_zhimg(&value)? {
                if !value.is_empty() {
                    images.push(value);
                }
                break;
            }
        }
    }
    Ok(unique(images))
}

/// `content_videos`: the videos embedded in an answer's or article's HTML, in order.
pub fn content_videos(content: &str) -> Result<Vec<ZhihuVideo>, PyError> {
    let mut videos: Vec<ZhihuVideo> = Vec::new();
    for found in VIDEO_BOX.find_iter(content) {
        let video_box = found.as_str();
        let Some(id) = LENS_ID.captures(video_box).or_else(|| VIDEO_TARGET.captures(video_box)) else {
            continue;
        };
        let id = id[1].to_owned();
        let cover = VIDEO_POSTER
            .captures(video_box)
            .map(|poster| unescape(&poster[1]).into_owned())
            .unwrap_or_default();
        let cover = if is_zhimg(&cover)? { cover } else { String::new() };
        if !videos.iter().any(|video| video.id == id) {
            videos.push(ZhihuVideo {
                id,
                fallback_url: String::new(),
                cover_url: cover,
            });
        }
    }
    Ok(videos)
}

/// `value or 0` read by `int()`.
fn int_or_zero(value: Option<&Value>) -> Result<i128, PyError> {
    match value {
        Some(value) if value.is_truthy() => py_int(value),
        _ => Ok(0),
    }
}

/// `value or 0` read by `float()`.
fn float_or_zero(value: Option<&Value>) -> Result<f64, PyError> {
    match value {
        Some(value) if value.is_truthy() => float(value),
        _ => Ok(0.0),
    }
}

/// `_best_playlist_url`: the widest entry's URL of a lens `playlist` (a dict by quality)
/// or a pin's (a list).
pub(crate) fn best_playlist_url(playlist: Option<&Value>) -> Result<String, PyError> {
    let entries: Vec<&Value> = match playlist {
        Some(Value::Dict(dict)) => dict.iter().map(|(_, value)| value).collect(),
        Some(Value::List(items)) => items.iter().collect(),
        _ => Vec::new(),
    };
    let mut best_url = String::new();
    let mut best_key: (i128, f64) = (-1, -1.0);
    for entry in entries {
        let Some(entry) = entry.as_dict() else {
            continue;
        };
        let Some(Value::Str(url)) = first_truthy(entry, &["play_url", "url"]) else {
            continue;
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        // Python's integers are unbounded; past 128 bits the area saturates (D127).
        let area = int_or_zero(entry.get("width"))?.saturating_mul(int_or_zero(entry.get("height"))?);
        let bitrate = float_or_zero(entry.get("bitrate"))?;
        // Tuple order: the area decides unless it ties, then the bitrate (NaN never wins).
        let better = if area == best_key.0 {
            bitrate > best_key.1
        } else {
            area > best_key.0
        };
        if better {
            best_url.clone_from(url);
            best_key = (area, bitrate);
        }
    }
    Ok(best_url)
}

/// `_pin_media`: a pin's pictures, videos and text.
pub(crate) fn pin_media(items: Option<&Value>) -> Result<(Vec<String>, Vec<ZhihuVideo>, String), PyError> {
    let mut images = Vec::new();
    let mut videos = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let items: &[Value] = match items {
        Some(Value::List(items)) => items,
        _ => &[],
    };
    for item in items {
        let Value::Dict(item) = item else {
            continue;
        };
        match as_str(item.get("type")) {
            Some("image") => {
                for key in ["original_url", "url"] {
                    if let Some(value) = as_str(item.get(key))
                        && is_zhimg(value)?
                    {
                        if !value.is_empty() {
                            images.push(value.to_owned());
                        }
                        break;
                    }
                }
            }
            Some("video") if py_isdigit(&str_or_empty(item.get("video_id"))) => {
                let empty = Dict::new();
                let info = item.get("video_info").and_then(Value::as_dict).unwrap_or(&empty);
                let playlist = match info.get("playlist") {
                    Some(playlist) if playlist.is_truthy() => Some(playlist),
                    _ => item.get("playlist"),
                };
                let fallback_url = best_playlist_url(playlist)?;
                let cover = as_str(item.get("thumbnail")).unwrap_or_default();
                let cover_url = if is_zhimg(cover)? { cover.to_owned() } else { String::new() };
                videos.push(ZhihuVideo {
                    id: item.get("video_id").map(py_str).unwrap_or_default(),
                    fallback_url,
                    cover_url,
                });
            }
            Some("text") => texts.push(str_or_empty(first_truthy(item, &["own_text", "content"]))),
            _ => {}
        }
    }
    let text = py_strip(&unescape(&strip_tags(&texts.join(" ")))).to_owned();
    Ok((unique(images), videos, text))
}

/// `int(value)` for `_timestamp`: `None` where Python raises `TypeError` or `ValueError`.
///
/// A digit string past 128 bits is an integer Python would return; a positive one is past
/// `time_t` too, so it is that `OverflowError`, which `_timestamp` lets through.
fn timestamp_seconds(value: Option<&Value>) -> Result<Option<i128>, PyError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if let Value::Str(text) = value {
        return match fav_core::pyint::parse_int(text) {
            Ok(seconds) => Ok(Some(seconds)),
            Err(fav_core::pyint::IntError::Overflow { negative: false })
                if py_strip(text).chars().filter(|ch| *ch != '_').count() <= INT_MAX_STR_DIGITS =>
            {
                Err(PyError::new("OverflowError", "timestamp out of range for platform time_t"))
            }
            Err(_) => Ok(None),
        };
    }
    match py_int(value) {
        Ok(seconds) => Ok(Some(seconds)),
        Err(error) if matches!(error.class.as_str(), "TypeError" | "ValueError") => Ok(None),
        Err(error) => Err(error),
    }
}

/// `_timestamp`: a positive Unix time, else `None`.
pub(crate) fn timestamp(value: Option<&Value>) -> Result<Option<DateTime<Utc>>, PyError> {
    match timestamp_seconds(value)? {
        Some(seconds) if seconds > 0 => fromtimestamp_utc(seconds).map(Some),
        _ => Ok(None),
    }
}

fn author(data: &Dict) -> String {
    match data.get("author") {
        Some(Value::Dict(author)) => py_strip(&str_or_empty(author.get("name"))).to_owned(),
        _ => String::new(),
    }
}

/// `_short`: the text cut to `max_bytes` of UTF-8, a partial character dropped, with `…`.
pub(crate) fn short(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", text[..end].trim_end_matches(is_py_space))
}

/// `parse_post`: the pictures and videos of a TikHub detail answer's `data`.
pub fn parse_post(link: &ZhihuLink, data: &Dict) -> Result<ZhihuPost, PyError> {
    let author = author(data);
    if link.kind == ZhihuKind::Pin {
        let (images, videos, text) = pin_media(data.get("content"))?;
        // excerpt_title carries the pin's opening text as HTML, topic links and all.
        let excerpt = py_strip(&unescape(&strip_tags(&str_or_empty(data.get("excerpt_title"))))).to_owned();
        let title = short(if excerpt.is_empty() { &text } else { &excerpt }, PIN_TITLE_BYTES);
        let title = if title.is_empty() { format!("Pin {}", link.id) } else { title };
        return Ok(ZhihuPost {
            link: link.clone(),
            title,
            author,
            published_at: timestamp(data.get("created"))?,
            images,
            videos,
        });
    }
    let content = str_or_empty(data.get("content"));
    let (title, published_at) = if link.kind == ZhihuKind::Answer {
        let title = match data.get("question") {
            Some(Value::Dict(question)) => py_strip(&str_or_empty(question.get("title"))).to_owned(),
            _ => String::new(),
        };
        let title = if title.is_empty() { format!("Answer {}", link.id) } else { title };
        (title, timestamp(data.get("created_time"))?)
    } else {
        let title = py_strip(&str_or_empty(data.get("title"))).to_owned();
        let title = if title.is_empty() { format!("Article {}", link.id) } else { title };
        (title, timestamp(data.get("created"))?)
    };
    Ok(ZhihuPost {
        link: link.clone(),
        title,
        author,
        published_at,
        images: content_images(&content)?,
        videos: content_videos(&content)?,
    })
}

/// Why an answer without the post's data is TikHub's failure and not the post's: a rate
/// limit, a server error, or the wrapper TikHub puts around a request it could not carry
/// out (`{"detail": {"code": 400, "message": "Request failed. Please retry. ..."}}`, which
/// it sends for every Zhihu lookup while its Zhihu API is down). `None` for any other
/// answer, such as a post that is gone.
fn outage_reason(status: u16, detail: Option<&Value>) -> Option<String> {
    let message = match detail {
        Some(Value::Dict(detail)) => detail.get("message").filter(|message| message.is_truthy()).map(py_str),
        Some(detail) => Some(py_str(detail)),
        None => None,
    };
    if status == 429 || status >= 500 {
        return Some(message.unwrap_or_else(|| "no data".to_owned()));
    }
    message.filter(|message| message.starts_with(TIKHUB_REQUEST_FAILED))
}

/// `fetch_post`: look the post up through TikHub.
pub async fn fetch_post(client: &PostClient, link: &ZhihuLink, api_url: &str, token: &str) -> Result<ZhihuPost, ZhihuError> {
    if token.is_empty() {
        return Err(ZhihuError::Account(
            "Zhihu links need a TikHub API key (services.tikhub)".to_owned(),
        ));
    }
    let (path, param) = link.kind.endpoint();
    let url = format!("{}{path}?{}", api_url.trim_end_matches('/'), urlencode(&[(param, link.id.clone())]));
    let request = Request::get(url).header("Authorization", format!("Bearer {token}"));
    let fetched = match Fetched::send(client.lookup(), request).await {
        Ok(fetched) => fetched,
        Err(error) if is_http_error(&error) => {
            return Err(ZhihuError::Outage(format!(
                "TikHub did not answer for {}: {}: {}",
                link.url(),
                error.class,
                error.message
            )));
        }
        Err(error) => return Err(error.into()),
    };
    if TIKHUB_ACCOUNT_STATUSES.contains(&fetched.status) {
        return Err(ZhihuError::Account(format!(
            "TikHub refused the API key (HTTP {}); check the key in services.tikhub and the account balance",
            fetched.status
        )));
    }
    let server_error = fetched.status == 429 || fetched.status >= 500;
    let Ok(payload) = json_from_bytes(&fetched.body) else {
        let message = format!("TikHub answered {} with HTTP {} and no JSON", link.url(), fetched.status);
        return Err(if server_error {
            ZhihuError::Outage(message)
        } else {
            ZhihuError::Zhihu(message)
        });
    };
    let data = match &payload {
        Value::Dict(payload) => payload.get("data").and_then(Value::as_dict),
        _ => None,
    };
    let Some(data) = data.filter(|data| fetched.status == 200 && !data.is_empty()) else {
        let detail = match &payload {
            Value::Dict(payload) => payload.get("detail").filter(|detail| detail.is_truthy()),
            _ => None,
        };
        if let Some(reason) = outage_reason(fetched.status, detail) {
            return Err(ZhihuError::Outage(format!(
                "TikHub is failing for {} (HTTP {}): {reason}",
                link.url(),
                fetched.status
            )));
        }
        return Err(ZhihuError::Zhihu(format!(
            "TikHub could not read {} (HTTP {}): {}; it may have been deleted",
            link.url(),
            fetched.status,
            detail.map(py_str).as_deref().unwrap_or("no data")
        )));
    };
    if let Some(error) = data.get("error")
        && let Value::Dict(fields) = error
    {
        let message = match fields.get("message") {
            Some(message) if message.is_truthy() => py_str(message),
            _ => py_str(error),
        };
        return Err(ZhihuError::Zhihu(format!("Zhihu refused {}: {message}", link.url())));
    }
    Ok(parse_post(link, data)?)
}

/// `resolve_video`: a fresh MP4 URL for the video at its best quality, or `""` when Zhihu
/// has none.
pub async fn resolve_video(client: &PostClient, video: &ZhihuVideo) -> Result<String, PyError> {
    let request = Request::get(format!("{}{}", client.endpoints().lens, video.id))
        .header("User-Agent", USER_AGENT)
        .header("Referer", "https://www.zhihu.com/");
    let payload = match Fetched::send(client.lens(), request).await {
        Ok(fetched) if fetched.status == 200 => json_from_bytes(&fetched.body).ok(),
        Ok(_) => None,
        Err(error) if is_http_error(&error) => None,
        Err(error) => return Err(error),
    };
    let playlist = match &payload {
        Some(Value::Dict(payload)) => payload.get("playlist"),
        _ => None,
    };
    let url = best_playlist_url(playlist)?;
    Ok(if url.is_empty() { video.fallback_url.clone() } else { url })
}

/// `fetch_page`: the post as a page of pictures and videos, each video resolved to a URL
/// to download now.
pub async fn fetch_page(client: &PostClient, link: &ZhihuLink, api_url: &str, token: &str) -> Result<ArticlePage, ZhihuError> {
    let post = fetch_post(client, link, api_url, token).await?;
    let mut videos = Vec::new();
    for video in &post.videos {
        let url = resolve_video(client, video).await?;
        if url.is_empty() {
            fav_core::logging::get("links").info(format!("Zhihu video {} of {} has no play URL", video.id, link.url()));
        } else {
            videos.push(PostVideo::new(url, video.cover_url.clone()));
        }
    }
    Ok(ArticlePage {
        title: post.title,
        account: post.author,
        published_at: post.published_at,
        images: post.images,
        picture_count: 0,
        videos,
    })
}
