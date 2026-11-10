//! Hanime1 series seeds and the watch page's playlist parser.
//!
//! Python origin: `src/tool/runtime_config.py`. The seed text `Title {id-N}`, the playlist
//! blocks (`video-playlist-wrapper`, matched by id or by an exact class token, closed by
//! counting `<div>` tags), the playlist title, the series ids in page order and the item
//! titles (current cards, legacy related-watch cards, a watch-link fallback), and the
//! canonical id of a series (its smallest member id).
//!
//! `normalize_hanime1_keywords` and `seed_has_title` are not ported: nothing in the
//! baseline calls them since the seeds moved from settings into `hanime1_series`.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use fav_core::pyhtml::unescape;
use fav_core::pytext::{py_isdecimal, py_strip};
use fav_core::pyurl::urlsplit;
use fav_runtime::source::JobError;

use crate::opencc::to_simplified_chinese;
use crate::pyre::{self, compile, compile_fancy, int_text};

/// `Hanime1ParserIncompatibleError.notification_dedupe_key`.
pub const PARSER_INCOMPATIBLE_DEDUPE_KEY: &str = "hanime1:parser:playlist-wrapper";
/// The exception class name of `Hanime1ParserIncompatibleError`.
pub const PARSER_INCOMPATIBLE: &str = "Hanime1ParserIncompatibleError";

/// `Hanime1ParserIncompatibleError(message)`: carries its own notification dedupe key.
pub fn parser_incompatible(message: impl Into<String>) -> JobError {
    JobError::new(PARSER_INCOMPATIBLE, message).with_notification_dedupe_key(PARSER_INCOMPATIBLE_DEDUPE_KEY)
}

/// Whether an error is a `Hanime1ParserIncompatibleError`.
pub fn is_parser_incompatible(error: &JobError) -> bool {
    error.class_name() == PARSER_INCOMPATIBLE
}

static SEED: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:(?:(?P<title>.*?)\s*)?\{id-(?P<id>\d+)\}\s*)$", true, false));
static PLAYLIST_TITLE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<h4[^>]*>(?P<title>.*?)</h4>", true, true));
static PLAYLIST_LINK_TITLE: LazyLock<regex::Regex> =
    LazyLock::new(|| compile(r#"<a[^>]+href=["'][^"']*/playlist\?[^"']*["'][^>]*>(?P<title>.*?)</a>"#, true, true));
static PLAYLIST_VIDEO_TITLE: LazyLock<regex::Regex> = LazyLock::new(|| {
    compile(
        r#"<h4[^>]+class=["'][^"']*\bvideo-title\b[^"']*["'][^>]*>(?P<title>.*?)</h4>"#,
        true,
        true,
    )
});
static CARD_MOBILE_TITLE: LazyLock<regex::Regex> = LazyLock::new(|| {
    compile(
        r#"<div[^>]+class=["'][^"']*\bcard-mobile-title\b[^"']*["'][^>]*>(?P<title>.*?)</div>"#,
        true,
        true,
    )
});
static IMAGE_ALT: LazyLock<regex::Regex> = LazyLock::new(|| compile(r#"<img\b[^>]*\balt=["'](?P<title>.*?)["']"#, true, true));
static ANCHOR: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<a\b[^>]*>(?P<title>.*?)</a>", true, true));
static WATCH_HREF: LazyLock<regex::Regex> = LazyLock::new(|| {
    compile(
        r#"href=["'](?P<url>(?:https?:)?//[^"']+/watch\?v=[^"'>\s]+|/watch\?v=[^"'>\s]+|watch\?v=[^"'>\s]+)["']"#,
        true,
        false,
    )
});
static DIV_TAG: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"</?div\b[^>]*>", true, false));
static PLAYLIST_BLOCK: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    compile_fancy(
        r#"<div\b(?=[^>]*(?:\bid=["']video-playlist-wrapper["']|\bclass=["'](?:[^"']*\s)?video-playlist-wrapper(?:\s[^"']*)?["']))[^>]*>"#,
        true,
        false,
    )
});
static PLAYLIST_CARD: LazyLock<fancy_regex::Regex> = LazyLock::new(|| class_block("playlist-video-card"));
static VIDEO_ITEM_CONTAINER: LazyLock<fancy_regex::Regex> = LazyLock::new(|| class_block("video-item-container"));
static RELATED_WATCH_WRAP: LazyLock<fancy_regex::Regex> = LazyLock::new(|| class_block("related-watch-wrap"));
static WATCH_SUFFIX_SITE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"\s*-\s*Hanime1\.me\s*$", true, false));
static WATCH_SUFFIX_TAGLINE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"\s*-\s*H動漫/裏番/線上看\s*$", false, false));
static LIST_PREFIX: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:清單|清单)\s*", false, false));

/// `extract_hanime1_div_blocks_by_class`'s pattern for one class token.
fn class_block(class_name: &str) -> fancy_regex::Regex {
    compile_fancy(
        &format!(
            r#"<div\b(?=[^>]*\bclass=["'](?:[^"']*\s)?{}(?:\s[^"']*)?["'])[^>]*>"#,
            fancy_regex::escape(class_name)
        ),
        true,
        false,
    )
}

/// A series target: `RuntimeSeriesSeed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSeriesSeed {
    pub video_id: String,
    pub title: String,
}

impl RuntimeSeriesSeed {
    /// `label`: `Title {id-N}`.
    pub fn label(&self) -> Result<String, JobError> {
        build_hanime1_seed(&self.video_id, Some(&self.title))
    }
}

/// One video of a playlist: `Hanime1PlaylistEntry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hanime1PlaylistEntry {
    pub video_id: String,
    pub title: Option<String>,
}

fn replace_all(re: &regex::Regex, text: &str, with: &str) -> String {
    re.replace_all(text, regex::NoExpand(with)).into_owned()
}

/// `extract_hanime1_seed_id(seed)`: the id of `Title {id-N}` / `{id-N}`, without leading
/// zeros; `None` for anything else or id 0.
pub fn extract_hanime1_seed_id(seed: &str) -> Result<Option<String>, JobError> {
    let Some(captures) = SEED.captures(py_strip(seed)) else {
        return Ok(None);
    };
    let normalized = int_text(&captures["id"])?;
    Ok((normalized != "0").then_some(normalized))
}

/// `normalize_hanime1_seed(raw, allow_id_only=...)`.
pub fn normalize_hanime1_seed(raw: &str, allow_id_only: bool) -> Result<Option<String>, JobError> {
    let text = py_strip(raw);
    if text.is_empty() {
        return Ok(None);
    }
    if py_isdecimal(text) {
        if !allow_id_only {
            return Ok(None);
        }
        let seed_id = int_text(text)?;
        return Ok((seed_id != "0").then(|| format!("{{id-{seed_id}}}")));
    }
    let Some(captures) = SEED.captures(text) else {
        return Ok(None);
    };
    let seed_id = int_text(&captures["id"])?;
    if seed_id == "0" {
        return Ok(None);
    }
    let raw_title = captures.name("title").map_or("", |title| py_strip(title.as_str()));
    match to_simplified_chinese(Some(raw_title)) {
        Some(title) => Ok(Some(format!("{title} {{id-{seed_id}}}"))),
        None if allow_id_only => Ok(Some(format!("{{id-{seed_id}}}"))),
        None => Ok(None),
    }
}

/// `normalize_hanime1_watch_title(title)`: tags removed, then entities, whitespace runs
/// collapsed, and the site's title suffixes cut.
pub fn normalize_hanime1_watch_title(title: &str) -> String {
    let normalized = unescape(&pyre::strip_tags(title)).into_owned();
    let normalized = py_strip(&pyre::collapse_whitespace(&normalized)).to_owned();
    let normalized = py_strip(&replace_all(&WATCH_SUFFIX_SITE, &normalized, "")).to_owned();
    py_strip(&replace_all(&WATCH_SUFFIX_TAGLINE, &normalized, "")).to_owned()
}

/// `_extract_hanime1_div_blocks`: every block that opens with `pattern`, up to its
/// matching `</div>`; scanning stops at the first block that never closes.
fn div_blocks(page_html: &str, pattern: &fancy_regex::Regex) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut search_pos = 0;
    while let Ok(Some(start)) = pattern.find_from_pos(page_html, search_pos) {
        let mut depth = 1_i64;
        let mut end_pos = None;
        for tag in DIV_TAG.find_iter(&page_html[start.end()..]) {
            if tag.as_str().to_lowercase().starts_with("</div") {
                depth -= 1;
            } else {
                depth += 1;
            }
            if depth == 0 {
                end_pos = Some(start.end() + tag.end());
                break;
            }
        }
        let Some(end_pos) = end_pos else {
            break;
        };
        blocks.push(page_html[start.start()..end_pos].to_owned());
        search_pos = end_pos;
    }
    blocks
}

/// `extract_hanime1_playlist_blocks(page_html)`.
pub fn extract_hanime1_playlist_blocks(page_html: &str) -> Vec<String> {
    div_blocks(page_html, &PLAYLIST_BLOCK)
}

/// `extract_hanime1_playlist_title(playlist_html)`.
pub fn extract_hanime1_playlist_title(playlist_html: &str) -> Option<String> {
    let captures = PLAYLIST_TITLE.captures(playlist_html)?;
    let title_html = &captures["title"];
    let source = PLAYLIST_LINK_TITLE
        .captures(title_html)
        .map_or(title_html, |link| link.name("title").map_or("", |title| title.as_str()));
    let title = normalize_hanime1_watch_title(source);
    let title = py_strip(&replace_all(&LIST_PREFIX, &title, "")).to_owned();
    (!title.is_empty()).then_some(title)
}

/// The text with the escapes pages put in JSON-ish attribute values undone.
pub(crate) fn unescape_links(html: &str) -> String {
    unescape(html).replace("\\/", "/").replace("\\u0026", "&")
}

/// `extract_hanime1_series_ids(playlist_html)`: watch-link ids in page order, without
/// leading zeros, `0` and duplicates left out.
pub fn extract_hanime1_series_ids(playlist_html: &str) -> Result<Vec<String>, JobError> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let normalized_html = unescape_links(playlist_html);
    for captures in WATCH_HREF.captures_iter(&normalized_html) {
        let query = urlsplit(&captures["url"], "")
            .map_err(|error| JobError::new("ValueError", error.0))?
            .query;
        let Some(value) = pyre::query_first(&query, "v") else {
            continue;
        };
        let candidate = py_strip(&value);
        if !py_isdecimal(candidate) {
            continue;
        }
        let normalized = int_text(candidate)?;
        if normalized == "0" || !seen.insert(normalized.clone()) {
            continue;
        }
        ids.push(normalized);
    }
    Ok(ids)
}

/// `_extract_hanime1_playlist_item_title(item_html)`: a card's own title.
fn playlist_item_title(item_html: &str) -> Option<String> {
    for pattern in [&*PLAYLIST_VIDEO_TITLE, &*CARD_MOBILE_TITLE, &*IMAGE_ALT] {
        let Some(captures) = pattern.captures(item_html) else {
            continue;
        };
        let title = normalize_hanime1_watch_title(&captures["title"]);
        if !title.is_empty() {
            return to_simplified_chinese(Some(&title));
        }
    }
    None
}

/// `_extract_hanime1_watch_link_title(item_html)`: the text of the card's one-video link.
fn watch_link_title(item_html: &str) -> Result<Option<String>, JobError> {
    for captures in ANCHOR.captures_iter(item_html) {
        let video_ids = extract_hanime1_series_ids(captures.get(0).map_or("", |anchor| anchor.as_str()))?;
        if video_ids.len() != 1 {
            continue;
        }
        let title = normalize_hanime1_watch_title(&captures["title"]);
        if !title.is_empty() {
            return Ok(Some(to_simplified_chinese(Some(&title)).unwrap_or(title)));
        }
    }
    Ok(None)
}

/// `extract_hanime1_playlist_entries(page_html, require_current_titles=...)`.
pub fn extract_hanime1_playlist_entries(page_html: &str, require_current_titles: bool) -> Result<Vec<Hanime1PlaylistEntry>, JobError> {
    let mut ordered_ids: Vec<String> = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut explicit_titles: HashMap<String, String> = HashMap::new();
    let mut fallback_titles: HashMap<String, String> = HashMap::new();
    let mut has_current_cards = false;

    for playlist_html in extract_hanime1_playlist_blocks(page_html) {
        for video_id in extract_hanime1_series_ids(&playlist_html)? {
            if seen_ids.insert(video_id.clone()) {
                ordered_ids.push(video_id);
            }
        }
        let mut current_blocks = div_blocks(&playlist_html, &PLAYLIST_CARD);
        let allow_link_fallback = current_blocks.is_empty();
        if current_blocks.is_empty() {
            current_blocks = div_blocks(&playlist_html, &VIDEO_ITEM_CONTAINER);
        }
        has_current_cards |= !current_blocks.is_empty();
        let legacy_blocks = div_blocks(&playlist_html, &RELATED_WATCH_WRAP);

        for item_html in current_blocks.iter().chain(&legacy_blocks) {
            let video_ids = extract_hanime1_series_ids(item_html)?;
            let mut title = playlist_item_title(item_html);
            let mut fallback = false;
            if title.is_none() && allow_link_fallback && current_blocks.contains(item_html) {
                title = watch_link_title(item_html)?;
                fallback = true;
            }
            if let Some(title) = title {
                let title_map = if fallback { &mut fallback_titles } else { &mut explicit_titles };
                for video_id in video_ids {
                    title_map.entry(video_id).or_insert_with(|| title.clone());
                }
            }
        }
    }

    // `fallback_titles | explicit_titles`: an explicit title wins.
    let mut titles = fallback_titles;
    titles.extend(explicit_titles);
    if require_current_titles && has_current_cards && titles.is_empty() {
        return Err(parser_incompatible("Hanime1 playlist cards did not expose compatible item titles"));
    }
    Ok(ordered_ids
        .into_iter()
        .map(|video_id| Hanime1PlaylistEntry {
            title: titles.get(&video_id).cloned(),
            video_id,
        })
        .collect())
}

/// `parse_hanime1_playlist(page_html, seed_id=..., require_current_titles=...)`: the
/// series title and its ids (the seed appended when the page did not list it).
pub fn parse_hanime1_playlist(
    page_html: &str,
    seed_id: Option<&str>,
    require_current_titles: bool,
) -> Result<Option<(String, Vec<String>)>, JobError> {
    let Some(title) = extract_hanime1_playlist_blocks(page_html)
        .iter()
        .find_map(|block| extract_hanime1_playlist_title(block))
    else {
        return Ok(None);
    };
    let entries = extract_hanime1_playlist_entries(page_html, require_current_titles)?;
    let mut series_ids: Vec<String> = entries.into_iter().map(|entry| entry.video_id).collect();
    if series_ids.is_empty() {
        return Ok(None);
    }
    if let Some(seed_id) = seed_id
        && !series_ids.iter().any(|id| id == seed_id)
    {
        series_ids.push(seed_id.to_owned());
    }
    Ok(Some((title, series_ids)))
}

/// `select_hanime1_canonical_id(series_ids)`: the smallest id as `str(int(...))`; `None`
/// for an empty list or one with a non-number.
pub fn select_hanime1_canonical_id(series_ids: &[String]) -> Option<String> {
    let mut best: Option<String> = None;
    for item in series_ids {
        let text = py_strip(item);
        if !py_isdecimal(text) {
            return None;
        }
        let value = int_text(text).ok()?;
        let smaller = best
            .as_ref()
            .is_none_or(|current| (value.len(), value.as_str()) < (current.len(), current.as_str()));
        if smaller {
            best = Some(value);
        }
    }
    best
}

/// `build_hanime1_seed(seed_id=..., title=...)`: `Title {id-N}` with the title in
/// Simplified Chinese.
pub fn build_hanime1_seed(seed_id: &str, title: Option<&str>) -> Result<String, JobError> {
    let Some(title) = to_simplified_chinese(title) else {
        return Err(JobError::new("ValueError", "title is required for hanime1 seed"));
    };
    Ok(format!("{title} {{id-{seed_id}}}"))
}

/// `parse_runtime_series_seed(raw)` for a string: only `Title {id-N}` with a title is a
/// seed.
pub fn parse_runtime_series_seed(raw: &str) -> Result<Option<RuntimeSeriesSeed>, JobError> {
    let text = py_strip(raw);
    if text.is_empty() {
        return Ok(None);
    }
    let Some(captures) = SEED.captures(text) else {
        return Ok(None);
    };
    let Some(video_id) = extract_hanime1_seed_id(text)? else {
        return Ok(None);
    };
    let raw_title = captures.name("title").map_or("", |title| py_strip(title.as_str()));
    Ok(to_simplified_chinese(Some(raw_title)).map(|title| RuntimeSeriesSeed { video_id, title }))
}

#[cfg(test)]
mod tests;
