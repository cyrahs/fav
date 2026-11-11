//! The Hanime1 worker's pure helpers: page parsers, URL builders and naming.
//!
//! Python origin: the static methods of `Hanime1` in `src/web/hanime1.py`. Where Python
//! read `cfg().host` or `cfg().path` the host or root is a parameter here. Every function
//! is held to `rust/goldens/hanime1/parsing.json`.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use fav_core::filename::sanitize;
use fav_core::pycasefold::casefold;
use fav_core::pyhtml::unescape;
use fav_core::pypath;
use fav_core::pytext::{is_py_space, py_isdecimal, py_strip};
use fav_core::pyurl::{quote, unquote, urlsplit};
use fav_runtime::source::JobError;

use crate::hanime1_series::value_error;
use crate::opencc::to_simplified_chinese;
use crate::pyre::{self, chars_after, chars_before, compile, compile_fancy, int_text};
use crate::runtime_config::{extract_hanime1_playlist_entries, parse_hanime1_playlist, unescape_links};

/// The worker's browser `User-Agent`.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36";

const RANKING_GENRE: &str = "裏番";
/// Titles carrying one of these are placeholders the site replaces later.
pub const IGNORED_TITLE_MARKERS: [&str; 3] = ["[新番预告]", "[中字后补]", "[中文后补]"];
const UNCATEGORIZED_GENRE: &str = "未分类";
/// How far after the artist link the video's own genre link may sit (code points).
const GENRE_SEARCH_WINDOW: usize = 1000;
const FILE_SIZE_UNIT: f64 = 1024.0;
const K_FALLBACK_MULTIPLIER: i64 = 540;

static STREAM_URL: LazyLock<regex::Regex> =
    LazyLock::new(|| compile(r#"(?:https?:)?//[^'"\s<>]+?\.(?:m3u8|mp4)(?:\?[^'"\s<>]*)?"#, true, false));
static OG_TITLE: LazyLock<[regex::Regex; 2]> = LazyLock::new(|| {
    [
        compile(
            r#"<meta[^>]+property=["']og:title["'][^>]+content=["'](?P<title>.*?)["']"#,
            true,
            true,
        ),
        compile(
            r#"<meta[^>]+content=["'](?P<title>.*?)["'][^>]+property=["']og:title["']"#,
            true,
            true,
        ),
    ]
});
static OG_IMAGE: LazyLock<[regex::Regex; 2]> = LazyLock::new(|| {
    [
        compile(
            r#"<meta[^>]+(?:property|name)=["'](?:og:image|twitter:image)["'][^>]+content=["'](?P<url>.*?)["']"#,
            true,
            true,
        ),
        compile(
            r#"<meta[^>]+content=["'](?P<url>.*?)["'][^>]+(?:property|name)=["'](?:og:image|twitter:image)["']"#,
            true,
            true,
        ),
    ]
});
static TITLE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<title[^>]*>(?P<title>.*?)</title>", true, true));
static DOWNLOAD_DATA_URL: LazyLock<regex::Regex> = LazyLock::new(|| compile(r#"data-url=["'](?P<url>https?://[^"']+)["']"#, true, false));
static WATCH_PLOT: LazyLock<regex::Regex> = LazyLock::new(|| {
    compile(
        r#"<div[^>]+class="[^"]*video-caption-text[^"]*"[^>]*>(?P<plot>.*?)</div>"#,
        true,
        true,
    )
});
static WATCH_UPLOADER: LazyLock<regex::Regex> =
    LazyLock::new(|| compile(r#"<a[^>]+id=["']video-artist-name["'][^>]*>(?P<uploader>.*?)</a>"#, true, true));
static WATCH_GENRE_LINK: LazyLock<regex::Regex> =
    LazyLock::new(|| compile(r#"href=["'][^"']*/search\?[^"']*genre=(?P<genre>[^"'&#]+)["']"#, true, false));
static PRIMARY_TITLE_TAG: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    compile_fancy(
        r#"<(?P<tag>h[1-6]|div)[^>]+id=["']shareBtn-title["'][^>]*>(?P<title>.*?)</(?P=tag)>"#,
        true,
        true,
    )
});
static PRIMARY_TITLE_META: LazyLock<[regex::Regex; 2]> = LazyLock::new(|| {
    [
        compile(r#"<meta[^>]+name=["']title["'][^>]+content=["'](?P<title>.*?)["']"#, true, true),
        compile(r#"<meta[^>]+content=["'](?P<title>.*?)["'][^>]+name=["']title["']"#, true, true),
    ]
});
static DIV_TEXT: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<div[^>]*>(?P<text>[^<]*)</div>", true, true));
static SEARCH_WATCH_HREF: LazyLock<regex::Regex> = LazyLock::new(|| {
    compile(
        r#"href=["'](?P<url>(?:https?:)?//[^"']+/watch\?v=[^"'>\s]+|/watch\?v=[^"'>\s]+|watch\?v=[^"'>\s]+)["']"#,
        true,
        false,
    )
});
static DATE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"\b\d{4}-\d{2}-\d{2}\b", false, false));
static FULL_DATE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^\d{4}-\d{2}-\d{2}$", false, false));
static WATCH_SUFFIX_SITE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"\s*-\s*Hanime1\.me\s*$", true, false));
static WATCH_SUFFIX_TAGLINE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"\s*-\s*H動漫/裏番/線上看\s*$", false, false));
// Python's `(?=\.mp4|[?&])` look-ahead; consuming it finds the same first match.
static QUALITY_P: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"(\d{3,4})p(?:\.mp4|[?&])", false, false));
static QUALITY_K: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"(\d{1,2})k(?:\.mp4|[?&])", false, false));

/// An item to download: `HanimeRecord`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HanimeRecord {
    pub id: String,
    pub title: String,
    pub page_url: Option<String>,
    pub stream_url: Option<String>,
    pub keyword: Option<String>,
    pub uploader: Option<String>,
}

/// What the watch page says about a video: `WatchMetadata`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchMetadata {
    pub title: Option<String>,
    pub uploader: Option<String>,
    pub release_date: Option<String>,
    pub plot: Option<String>,
    pub cover_url: Option<String>,
    pub genre: Option<String>,
}

/// One video of a watch page's playlist: `WatchSeriesVideo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchSeriesVideo {
    pub video_id: String,
    pub title: Option<String>,
    pub position: i64,
}

/// A watch page's playlist: `WatchSeries`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchSeries {
    pub name: Option<String>,
    pub video_ids: Vec<String>,
    pub videos: Vec<WatchSeriesVideo>,
}

fn replace_all(re: &regex::Regex, text: &str, with: &str) -> String {
    re.replace_all(text, regex::NoExpand(with)).into_owned()
}

fn split(url: &str) -> Result<fav_core::pyurl::SplitResult, JobError> {
    urlsplit(url, "").map_err(|error| value_error(error.0))
}

/// `quote_plus(text)`.
pub fn quote_plus(text: &str) -> String {
    if text.contains(' ') {
        quote(text, " ").replace(' ', "+")
    } else {
        quote(text, "")
    }
}

/// `_normalize_stream_url(raw)`.
fn normalize_stream_url(raw: &str) -> String {
    let url = py_strip(raw).trim_matches(['\'', '"']);
    if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.to_owned()
    }
}

/// `extract_stream_urls(page_html)`: media URLs, unescaped and deduplicated, `.m3u8` first.
pub fn extract_stream_urls(page_html: &str) -> Result<Vec<String>, JobError> {
    let normalized_html = unescape_links(page_html);
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for raw in STREAM_URL.find_iter(&normalized_html) {
        let url = normalize_stream_url(raw.as_str());
        if url.is_empty() || !seen.insert(casefold(&url)) {
            continue;
        }
        candidates.push(url);
    }
    let mut m3u8_urls = Vec::new();
    for url in &candidates {
        if split(url)?.path.to_lowercase().ends_with(".m3u8") {
            m3u8_urls.push(url.clone());
        }
    }
    let other_urls: Vec<String> = candidates.into_iter().filter(|url| !m3u8_urls.contains(url)).collect();
    m3u8_urls.extend(other_urls);
    Ok(m3u8_urls)
}

/// `parse_title(page_html)`: `og:title`, else `<title>`.
pub fn parse_title(page_html: &str) -> Option<String> {
    for pattern in OG_TITLE.iter() {
        let Some(captures) = pattern.captures(page_html) else {
            continue;
        };
        let title = py_strip(&unescape(&captures["title"])).to_owned();
        if !title.is_empty() {
            return Some(title);
        }
    }
    let captures = TITLE.captures(page_html)?;
    let title = py_strip(&unescape(&captures["title"])).to_owned();
    (!title.is_empty()).then_some(title)
}

/// `_normalize_download_title(title)`: the download page's title without the site suffix
/// and the `下載 ` prefix.
pub fn normalize_download_title(title: Option<&str>) -> Option<String> {
    let title = title.filter(|title| !title.is_empty())?;
    let mut normalized = py_strip(title);
    if let Some(stripped) = normalized.strip_suffix(" - Hanime1.me") {
        normalized = py_strip(stripped);
    }
    for prefix in ["下載 ", "下载 "] {
        if let Some(stripped) = normalized.strip_prefix(prefix) {
            normalized = py_strip(stripped);
        }
    }
    (!normalized.is_empty()).then(|| normalized.to_owned())
}

/// `_normalize_watch_title(title)`: entities first, then tags, whitespace runs, the site's
/// suffixes.
pub fn normalize_watch_title(title: Option<&str>) -> Option<String> {
    let title = title.filter(|title| !title.is_empty())?;
    let normalized = pyre::strip_tags(&unescape(title));
    let normalized = py_strip(&pyre::collapse_whitespace(&normalized)).to_owned();
    let normalized = py_strip(&replace_all(&WATCH_SUFFIX_SITE, &normalized, "")).to_owned();
    let normalized = py_strip(&replace_all(&WATCH_SUFFIX_TAGLINE, &normalized, "")).to_owned();
    (!normalized.is_empty()).then_some(normalized)
}

/// `int(group)` of a short `\d` run (any decimal digits).
fn small_int(digits: &str) -> i64 {
    int_text(digits).ok().and_then(|text| text.parse().ok()).unwrap_or(0)
}

/// `_stream_quality(url)`: the `720p` / `4k` label before `.mp4`, `?` or `&`, as a height.
pub fn stream_quality(url: &str) -> i64 {
    let lowered = url.to_lowercase();
    if let Some(captures) = QUALITY_P.captures(&lowered) {
        return small_int(&captures[1]);
    }
    let Some(captures) = QUALITY_K.captures(&lowered) else {
        return 0;
    };
    match small_int(&captures[1]) {
        2 => 1440,
        4 => 2160,
        8 => 4320,
        k_value => k_value * K_FALLBACK_MULTIPLIER,
    }
}

/// `_stream_resolution_label(stream_url)`: `720p`, or `None` without a label.
pub fn stream_resolution_label(stream_url: &str) -> Option<String> {
    let quality = stream_quality(stream_url);
    (quality > 0).then(|| format!("{quality}p"))
}

/// `_format_file_size(size_bytes)`: `999 B`, `1.2 KB`, ... up to TB.
pub fn format_file_size(size_bytes: Option<i128>) -> Option<String> {
    let size_bytes = size_bytes.filter(|size| *size >= 0)?;
    if size_bytes < 1024 {
        return Some(format!("{size_bytes} B"));
    }
    // `float(size_bytes)`: the nearest double, as Python rounds an int.
    #[allow(clippy::cast_precision_loss)] // Python's float() rounds the same way.
    let mut size = size_bytes as f64;
    let units = ["KB", "MB", "GB", "TB"];
    for (index, unit) in units.iter().enumerate() {
        size /= FILE_SIZE_UNIT;
        if size < FILE_SIZE_UNIT || index == units.len() - 1 {
            return Some(format!("{size:.1} {unit}"));
        }
    }
    None
}

/// `extract_download_urls(page_html)`: the download page's `data-url`s, highest quality
/// first (ties keep page order).
pub fn extract_download_urls(page_html: &str) -> Vec<String> {
    let normalized_html = unescape_links(page_html);
    let mut seen = HashSet::new();
    let mut urls = Vec::new();
    for captures in DOWNLOAD_DATA_URL.captures_iter(&normalized_html) {
        let url = normalize_stream_url(&captures["url"]);
        if url.is_empty() || !seen.insert(casefold(&url)) {
            continue;
        }
        urls.push(url);
    }
    urls.sort_by_key(|url| std::cmp::Reverse(stream_quality(url)));
    urls
}

/// `_extract_video_code(candidate_url)`: the first `v` of the query, sanitized.
pub fn extract_video_code(candidate_url: Option<&str>) -> Result<Option<String>, JobError> {
    let Some(candidate_url) = candidate_url.filter(|url| !url.is_empty()) else {
        return Ok(None);
    };
    let query = split(candidate_url)?.query;
    let Some(value) = pyre::query_first(&query, "v") else {
        return Ok(None);
    };
    let video_id = py_strip(&value);
    if video_id.is_empty() {
        return Ok(None);
    }
    Ok(Some(sanitize(video_id, 80)))
}

fn page_url_path_contains(item: &HanimeRecord, needle: &str) -> Result<Option<String>, JobError> {
    match item.page_url.as_deref().filter(|url| !url.is_empty()) {
        Some(page_url) if split(page_url)?.path.contains(needle) => Ok(Some(page_url.to_owned())),
        _ => Ok(None),
    }
}

fn video_code_or_id(item: &HanimeRecord) -> Result<String, JobError> {
    Ok(extract_video_code(item.page_url.as_deref())?.unwrap_or_else(|| sanitize(&item.id, 80)))
}

/// `_build_download_page_url(item)`.
pub fn build_download_page_url(host: &str, item: &HanimeRecord) -> Result<String, JobError> {
    if let Some(page_url) = page_url_path_contains(item, "/download")? {
        return Ok(page_url);
    }
    Ok(format!("{}/download?v={}", host.trim_end_matches('/'), video_code_or_id(item)?))
}

/// `_build_watch_page_url(item)`.
pub fn build_watch_page_url(host: &str, item: &HanimeRecord) -> Result<String, JobError> {
    if let Some(page_url) = page_url_path_contains(item, "/watch")? {
        return Ok(page_url);
    }
    Ok(format!("{}/watch?v={}", host.trim_end_matches('/'), video_code_or_id(item)?))
}

/// `_normalize_page_url(candidate_url)`: absolute against the host.
pub fn normalize_page_url(host: &str, candidate_url: &str) -> String {
    let host = host.trim_end_matches('/');
    let unescaped = unescape(candidate_url);
    let normalized = py_strip(&unescaped);
    if normalized.starts_with("//") {
        format!("https:{normalized}")
    } else if normalized.starts_with('/') {
        format!("{host}{normalized}")
    } else if normalized.starts_with("http://") || normalized.starts_with("https://") {
        normalized.to_owned()
    } else {
        format!("{host}/{}", normalized.trim_start_matches('/'))
    }
}

/// `extract_search_video_ids(page_html)`: watch-link ids of a search page, in order.
pub fn extract_search_video_ids(host: &str, page_html: &str) -> Result<Vec<String>, JobError> {
    let normalized_html = unescape_links(page_html);
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for captures in SEARCH_WATCH_HREF.captures_iter(&normalized_html) {
        let watch_url = normalize_page_url(host, &captures["url"]);
        if !split(&watch_url)?.path.contains("/watch") {
            continue;
        }
        let Some(video_id) = extract_video_code(Some(&watch_url))? else {
            continue;
        };
        if video_id.is_empty() || !seen.insert(casefold(&video_id)) {
            continue;
        }
        ids.push(video_id);
    }
    Ok(ids)
}

/// `extract_watch_series(page_html)`: the playlist with each video's own title.
pub fn extract_watch_series(page_html: &str) -> Result<Option<WatchSeries>, JobError> {
    let Some((series_name, ids)) = parse_hanime1_playlist(page_html, None, true)? else {
        return Ok(None);
    };
    let series_name = to_simplified_chinese(Some(&series_name)).unwrap_or(series_name);
    let titles: HashMap<String, Option<String>> = extract_hanime1_playlist_entries(page_html, false)?
        .into_iter()
        .map(|entry| (entry.video_id, entry.title))
        .collect();
    let videos = ids
        .iter()
        .zip(1..)
        .map(|(video_id, position)| WatchSeriesVideo {
            video_id: video_id.clone(),
            title: titles.get(video_id).cloned().flatten(),
            position,
        })
        .collect();
    Ok(Some(WatchSeries {
        name: Some(series_name),
        video_ids: ids,
        videos,
    }))
}

/// `_playlist_item_titles_are_distinct(videos)`: every video has a title and no two are
/// the same once simplified, whitespace-collapsed and case-folded.
pub fn playlist_item_titles_are_distinct(videos: &[WatchSeriesVideo]) -> bool {
    let titles: HashSet<String> = videos
        .iter()
        .map(|video| {
            let simplified = to_simplified_chinese(video.title.as_deref()).unwrap_or_default();
            casefold(py_strip(&pyre::collapse_whitespace(&simplified)))
        })
        .collect();
    !videos.is_empty() && !titles.contains("") && titles.len() == videos.len()
}

/// `_format_playlist_archive_title(...)`: the item's title when titles are usable, else
/// `Series NN` numbered from the end of the playlist.
pub fn format_playlist_archive_title(series_name: Option<&str>, video: &WatchSeriesVideo, total: i64, use_item_title: bool) -> String {
    let series_title = to_simplified_chinese(series_name).or_else(|| series_name.map(str::to_owned));
    let item_title = to_simplified_chinese(video.title.as_deref()).or_else(|| video.title.clone());
    let item_title = item_title.filter(|title| !title.is_empty());
    if use_item_title && let Some(item_title) = &item_title {
        return item_title.clone();
    }
    if let Some(series_title) = series_title.filter(|title| !title.is_empty()) {
        return format!("{series_title} {:02}", total - video.position + 1);
    }
    item_title.unwrap_or_else(|| video.video_id.clone())
}

/// `_ignored_title_marker(title)`.
pub fn ignored_title_marker(title: Option<&str>) -> Option<&'static str> {
    let normalized = to_simplified_chinese(title)?;
    IGNORED_TITLE_MARKERS.into_iter().find(|marker| normalized.contains(marker))
}

/// `_extract_release_date_from_watch_html(page_html)`: a date within 300 code points after
/// the view counter.
fn release_date_from_watch_html(page_html: &str) -> Option<String> {
    let normalized = unescape(page_html).replace("&nbsp;", " ");
    for marker in ["觀看次數", "观看次数"] {
        let Some(index) = normalized.find(marker) else {
            continue;
        };
        if let Some(date) = DATE.find(chars_after(&normalized, index, 300)) {
            return Some(date.as_str().to_owned());
        }
    }
    None
}

/// The 1200 code points before the plot block.
fn plot_context(page_html: &str) -> Option<&str> {
    let plot = WATCH_PLOT.find(page_html)?;
    Some(&page_html[chars_before(page_html, plot.start(), 1200)..plot.start()])
}

/// `_extract_title_from_plot_context(page_html)`: the last plain `<div>` text before the
/// plot that is neither the view counter nor a date.
fn title_from_plot_context(page_html: &str) -> Option<String> {
    let context = unescape(plot_context(page_html)?).replace("&nbsp;", " ");
    let candidates: Vec<String> = DIV_TEXT
        .captures_iter(&context)
        .map(|captures| py_strip(&pyre::collapse_whitespace(&captures["text"])).to_owned())
        .filter(|text| !text.is_empty())
        .collect();
    for candidate in candidates.iter().rev() {
        if candidate.contains("觀看次數") || candidate.contains("观看次数") || FULL_DATE.is_match(candidate) {
            continue;
        }
        return normalize_watch_title(Some(candidate));
    }
    None
}

/// `_extract_primary_watch_title(page_html)`.
fn primary_watch_title(page_html: &str) -> Option<String> {
    if let Ok(Some(captures)) = PRIMARY_TITLE_TAG.captures(page_html)
        && let Some(title) = normalize_watch_title(captures.name("title").map(|title| title.as_str()))
    {
        return Some(title);
    }
    for pattern in PRIMARY_TITLE_META.iter() {
        if let Some(captures) = pattern.captures(page_html)
            && let Some(title) = normalize_watch_title(Some(&captures["title"]))
        {
            return Some(title);
        }
    }
    None
}

/// `_extract_release_date_from_plot_context(page_html)`.
fn release_date_from_plot_context(page_html: &str) -> Option<String> {
    let context = unescape(plot_context(page_html)?).replace("&nbsp;", " ");
    DATE.find(&context).map(|date| date.as_str().to_owned())
}

/// Entities, tags and whitespace runs out of a fragment.
fn fragment_text(fragment: &str) -> Option<String> {
    let content = pyre::strip_tags(&unescape(fragment));
    let content = py_strip(&pyre::collapse_whitespace(&content)).to_owned();
    (!content.is_empty()).then_some(content)
}

/// `_extract_genre_from_watch_html(page_html)`: the genre link right after the artist
/// link (the nav bar before it and related cards far below carry genre links too).
pub fn extract_genre_from_watch_html(page_html: &str) -> Option<String> {
    let artist = WATCH_UPLOADER.find(page_html)?;
    let window = chars_after(page_html, artist.end(), GENRE_SEARCH_WINDOW);
    let captures = WATCH_GENRE_LINK.captures(window)?;
    let genre = py_strip(&unescape(&unquote(&captures["genre"]))).to_owned();
    (!genre.is_empty()).then_some(genre)
}

/// `_strip_uploader_prefix(title, uploader)`: `[Uploader] Title` as `Title`.
pub fn strip_uploader_prefix(title: &str, uploader: Option<&str>) -> String {
    let Some(uploader) = uploader.filter(|uploader| !uploader.is_empty()) else {
        return title.to_owned();
    };
    // `^\[\s*<uploader>\s*\]\s*`, case-insensitive, matched by hand: the uploader comes
    // from the page, and a pattern built from a huge one would exceed the regex size limit.
    let rest = match_uploader_prefix(title, py_strip(uploader)).unwrap_or(title);
    let stripped = py_strip(rest).to_owned();
    if stripped.is_empty() { title.to_owned() } else { stripped }
}

/// What follows `[ uploader ]` and its trailing whitespace at the start of `title`.
fn match_uploader_prefix<'a>(title: &'a str, uploader: &str) -> Option<&'a str> {
    let rest = title.strip_prefix('[')?.trim_start_matches(is_py_space);
    let mut chars = rest.char_indices();
    for expected in uploader.chars() {
        let (_, actual) = chars.next()?;
        if !same_ignoring_case(actual, expected) {
            return None;
        }
    }
    let rest = chars.next().map_or("", |(index, _)| &rest[index..]);
    let rest = rest.trim_start_matches(is_py_space).strip_prefix(']')?;
    Some(rest.trim_start_matches(is_py_space))
}

/// One character against another under `re.IGNORECASE` (simple, one-to-one case mappings).
fn same_ignoring_case(left: char, right: char) -> bool {
    fn single(mut chars: impl Iterator<Item = char>) -> Option<char> {
        let first = chars.next()?;
        chars.next().is_none().then_some(first)
    }
    left == right
        || matches!((single(left.to_lowercase()), single(right.to_lowercase())), (Some(a), Some(b)) if a == b)
        || matches!((single(left.to_uppercase()), single(right.to_uppercase())), (Some(a), Some(b)) if a == b)
}

/// `_extract_cover_url_from_watch_html(page_html)`.
fn cover_url_from_watch_html(host: &str, page_html: &str) -> Option<String> {
    for pattern in OG_IMAGE.iter() {
        let Some(captures) = pattern.captures(page_html) else {
            continue;
        };
        let raw = py_strip(&unescape(&captures["url"])).to_owned();
        if raw.is_empty() || raw.starts_with("data:") {
            continue;
        }
        return Some(normalize_page_url(host, &raw));
    }
    None
}

/// `extract_watch_metadata(page_html)`.
pub fn extract_watch_metadata(host: &str, page_html: &str) -> WatchMetadata {
    let title = primary_watch_title(page_html)
        .or_else(|| title_from_plot_context(page_html))
        .or_else(|| normalize_watch_title(parse_title(page_html).as_deref()));
    let uploader = WATCH_UPLOADER
        .captures(page_html)
        .and_then(|captures| fragment_text(&captures["uploader"]));
    let release_date = release_date_from_plot_context(page_html).or_else(|| release_date_from_watch_html(page_html));
    let plot = WATCH_PLOT.captures(page_html).and_then(|captures| fragment_text(&captures["plot"]));
    WatchMetadata {
        title: to_simplified_chinese(title.as_deref()),
        uploader: to_simplified_chinese(uploader.as_deref()),
        release_date,
        plot: to_simplified_chinese(plot.as_deref()),
        cover_url: cover_url_from_watch_html(host, page_html),
        genre: to_simplified_chinese(extract_genre_from_watch_html(page_html).as_deref()),
    }
}

/// `_build_download_command(...)`: the `yt-dlp` argv.
pub fn build_download_command(stream_url: &str, output_template: &str, referer: Option<&str>, cookie_header: Option<&str>) -> Vec<String> {
    let mut command: Vec<String> = [
        "yt-dlp",
        "-o",
        output_template,
        "--no-mtime",
        "--no-part",
        "-N",
        "8",
        "--retries",
        "15",
        "--fragment-retries",
        "15",
        "--socket-timeout",
        "30",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(referer) = referer.filter(|referer| !referer.is_empty()) {
        command.extend(["--referer".to_owned(), referer.to_owned()]);
    }
    command.extend(["--add-header".to_owned(), format!("User-Agent:{USER_AGENT}")]);
    if let Some(cookie_header) = cookie_header.filter(|cookie| !cookie.is_empty()) {
        command.extend(["--add-header".to_owned(), format!("Cookie:{cookie_header}")]);
    }
    command.push(stream_url.to_owned());
    command
}

/// `_derive_item_id(item)`: the sanitized id, else the last path segment of the page or
/// stream URL, else `unknown`.
pub fn derive_item_id(item: &HanimeRecord) -> Result<String, JobError> {
    let base = sanitize(py_strip(&item.id), 80);
    if !base.is_empty() {
        return Ok(base);
    }
    for candidate in [item.page_url.as_deref(), item.stream_url.as_deref()].into_iter().flatten() {
        if candidate.is_empty() {
            continue;
        }
        let path = split(candidate)?.path;
        let path = path.trim_matches('/');
        if path.is_empty() {
            continue;
        }
        let slug = sanitize(path.rsplit('/').next().unwrap_or_default(), 80);
        if !slug.is_empty() {
            return Ok(slug);
        }
    }
    Ok("unknown".to_owned())
}

/// `_resolve_output_dir(...)`: `<root>/<genre>/<group>`, `未分类` without a genre. The
/// second value says whether the genre was missing (Python logs a warning).
pub fn resolve_output_dir(root: &str, genre: Option<&str>, group: Option<&str>) -> (String, bool) {
    let mut genre_dir = sanitize(py_strip(genre.unwrap_or_default()), 120);
    let missing = genre_dir.is_empty();
    if missing {
        UNCATEGORIZED_GENRE.clone_into(&mut genre_dir);
    }
    let base = pypath::join(root, &genre_dir);
    let group_dir = sanitize(py_strip(group.unwrap_or_default()), 120);
    if group_dir.is_empty() {
        return (base, missing);
    }
    (pypath::join(&base, &group_dir), missing)
}

/// `_build_ranking_page_url(period=..., page=...)`.
pub fn build_ranking_page_url(host: &str, period: &str, page: i64) -> Result<String, JobError> {
    let sort_value = match period {
        "weekly" => "本週排行",
        "monthly" => "本月排行",
        _ => return Err(value_error(format!("Unsupported Hanime1 ranking period: {period}"))),
    };
    let mut params = format!("genre={}&sort={}", quote_plus(RANKING_GENRE), quote_plus(sort_value));
    if page > 1 {
        params = format!("{params}&page={page}");
    }
    Ok(format!("{}/search?{params}", host.trim_end_matches('/')))
}

/// `_normalize_runtime_video_id(raw)` for a string: a positive decimal id (as written),
/// read from a watch URL's `v` when it is one.
pub fn normalize_runtime_video_id(raw: &str) -> Result<Option<String>, JobError> {
    let mut text = py_strip(raw).to_owned();
    if !text.is_empty()
        && let Some(code) = extract_video_code(Some(&text))?
    {
        text = code;
    }
    if text.is_empty() {
        return Ok(None);
    }
    let normalized = sanitize(&text, 80);
    if normalized.is_empty() || !py_isdecimal(&normalized) || !pyre::is_positive_decimal(&normalized)? {
        return Ok(None);
    }
    Ok(Some(normalized))
}

/// `_series_member_ids_from_watch_series(...)`: the seed and the playlist's ids, each once.
pub fn series_member_ids_from_watch_series(seed_video_id: &str, series: &WatchSeries) -> Result<Vec<String>, JobError> {
    let mut member_ids = Vec::new();
    let mut seen = HashSet::new();
    for raw_id in std::iter::once(seed_video_id).chain(series.video_ids.iter().map(String::as_str)) {
        let Some(video_id) = normalize_runtime_video_id(raw_id)? else {
            continue;
        };
        if seen.insert(casefold(&video_id)) {
            member_ids.push(video_id);
        }
    }
    Ok(member_ids)
}

/// `_format_scan_error(exc)`: `Class: message`, cut to 500 code points.
pub fn format_scan_error(error: &JobError) -> String {
    pyre::truncate_chars(&format!("{}: {}", error.class_name(), error.message()), 500).to_owned()
}
