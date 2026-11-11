//! Parsing for 公众号 (WeChat Official Account) articles forwarded to the agent's account.
//!
//! Python origin: `src/web/wechat_article.py`.
//!
//! A shared article reaches the `FavInbox` bridge as an app message (type 49) whose content
//! is the `<msg><appmsg>` XML WeChat sends; `<type>5</type>` inside it marks a link share.
//! The article page itself is public: a plain GET returns the HTML with the title, account
//! name and publish time in inline script variables, and the body images as `data-src`
//! attributes pointing at mmbiz.qpic.cn. Those URLs end in a width (`/640`); `/0` asks for
//! the original. Image posts (the 小绿书 layout) carry their pictures in
//! `picture_page_info_list` instead of the body.
//!
//! The URL has to be the one in the message, with its `chksm`: the bare
//! `__biz`/`mid`/`idx`/`sn` form is answered with a captcha page. Image posts
//! (`<itemshowtype>8</itemshowtype>`) are always put behind Tencent's slider captcha; the
//! share then names the first picture at full size (`coverpicimageurl`) and how many there
//! are (`piccount`), so that one picture is what can be saved.
//!
//! An image post's page lists its pictures in `picture_page_info_list`, a script array of
//! objects. Each object's own `cdn_url` is the picture; objects nested in it
//! (`watermark_info` and the like) carry other `cdn_url` values, which is why the list is
//! read by structure rather than by pattern.
//!
//! Every function that Python lets raise returns its exception as a [`PyError`]: a
//! malformed IPv6 host in a URL (`urlsplit`'s `ValueError`), an `isdigit()` string `int()`
//! refuses, a timestamp `datetime` cannot hold.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use chrono::{DateTime, Utc};
use fav_core::pyhtml::unescape;
use fav_core::pytext::{py_isdigit, py_strip};
use fav_core::pyurl::urlunsplit;

use crate::pydecode::{PyError, py_int};
use crate::pyre::{chars_after, compile, compile_fancy, query_first};
use crate::pyval::{digit_timestamp, fromtimestamp_utc, split};

const ARTICLE_HOST: &str = "mp.weixin.qq.com";
pub const LINK_SHARE_TYPE: &str = "5";
/// A video link card, as the Tieba app shares a video post.
pub const VIDEO_SHARE_TYPE: &str = "4";
pub const IMAGE_POST: &str = "8";
/// Paths the site redirects a request it does not trust to.
pub const CAPTCHA_PATHS: [&str; 2] = ["/mp/wappoc_appmsgcaptcha", "/mp/verifycode"];

const IMAGE_HOST: &str = r#"https?://mmbiz\.qpic\.cn/[^"'\s<>]+"#;
static APPMSG: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<appmsg\b.*?</appmsg>", false, true));
static IMAGE_HOST_FULL: LazyLock<regex::Regex> = LazyLock::new(|| compile(&format!("^(?:{IMAGE_HOST})$"), false, false));
static BODY_IMAGE: LazyLock<regex::Regex> = LazyLock::new(|| compile(&format!(r#"<img\b[^>]*?\bdata-src="({IMAGE_HOST})""#), true, false));
static PICTURE_LIST: LazyLock<regex::Regex> = LazyLock::new(|| compile(r#"picture_page_info_list['"]?\s*[:=]\s*\["#, false, false));
static CDN_URL: LazyLock<fancy_regex::Regex> =
    LazyLock::new(|| compile_fancy(r#"(?<![\w$])['"]?cdn_url['"]?\s*:\s*(['"])(.*?)(?<!\\)\1"#, false, true));
static JS_ESCAPE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"\\(x[0-9a-fA-F]{2}|u[0-9a-fA-F]{4}|.)", false, true));
/// Script source split into strings, brackets and everything else, so brackets inside
/// strings do not count.
static JS_TOKEN: LazyLock<regex::Regex> = LazyLock::new(|| {
    compile(
        r#"'(?:\\.|[^'\\])*'|"(?:\\.|[^"\\])*"|`(?:\\.|[^`\\])*`|[{}\[\]]|[^'"`{}\[\]]+|."#,
        false,
        true,
    )
});
/// Python's `$` (no `MULTILINE`) also matches before a final newline.
static WIDTH_SEGMENT: LazyLock<fancy_regex::Regex> = LazyLock::new(|| compile_fancy(r"/\d+(?=(\?|\n?\z))", false, false));
static MSG_TITLE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"var msg_title = '(.*?)'", false, false));
static OG_TITLE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r#"<meta property="og:title" content="(.*?)""#, false, false));
static NICKNAME: LazyLock<regex::Regex> = LazyLock::new(|| compile(r#"var nickname = htmlDecode\("(.*?)"\)"#, false, false));
static CREATED: LazyLock<regex::Regex> = LazyLock::new(|| compile(r#"var ct = "(\d+)""#, false, false));
/// One compiled `<name>(.*?)</name>` per element name `_tag` is asked for.
static TAGS: LazyLock<Mutex<HashMap<String, regex::Regex>>> = LazyLock::new(Mutex::default);

/// Inside the array (1) and one of its elements (2).
const ELEMENT_DEPTH: usize = 2;
/// Far more than any page's picture list; bounds the scan on a malformed page.
const MAX_LIST_CHARS: usize = 4_000_000;

/// `_EXTENSIONS`.
fn extension_for(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "jpeg" | "jpg" => "jpg",
        "png" => "png",
        "gif" => "gif",
        "webp" => "webp",
        "bmp" => "bmp",
        _ => return None,
    })
}

/// A shared link, as its card names it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArticleLink {
    pub url: String,
    pub title: String,
    pub description: String,
    pub account: String,
    /// From the share's `<mmreadershare>`: 8 marks an image post.
    pub show_type: String,
    pub picture_count: i128,
    pub cover_url: String,
}

impl ArticleLink {
    /// `ArticleLink(url=..., title=..., description=...)`.
    pub fn new(url: impl Into<String>, title: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            title: title.into(),
            description: description.into(),
            ..Self::default()
        }
    }
}

/// One of a post's videos.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostVideo {
    pub url: String,
    pub cover_url: String,
}

impl PostVideo {
    pub fn new(url: impl Into<String>, cover_url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            cover_url: cover_url.into(),
        }
    }
}

/// A post read as its pictures and videos.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArticlePage {
    pub title: String,
    pub account: String,
    pub published_at: Option<DateTime<Utc>>,
    pub images: Vec<String>,
    /// How many pictures the post has, when only some of them could be read.
    pub picture_count: i128,
    pub videos: Vec<PostVideo>,
}

/// `_tag`: the text of the first `<name>` element, with CDATA and entities undone.
pub(crate) fn tag(xml: &str, name: &str) -> String {
    let mut tags = TAGS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let pattern = tags
        .entry(name.to_owned())
        .or_insert_with(|| compile(&format!("<{name}>(.*?)</{name}>"), false, true));
    let Some(found) = pattern.captures(xml) else {
        return String::new();
    };
    let mut text = py_strip(&found[1]);
    if text.starts_with("<![CDATA[") && text.ends_with("]]>") {
        text = &text[9..text.len() - 3];
    }
    py_strip(&unescape(text)).to_owned()
}

/// `appmsg_link`: the link a link-card message of one of `types` shares, whatever site it
/// points at, or `None` for anything else.
pub fn appmsg_link(content: &str, types: &[&str]) -> Result<Option<ArticleLink>, PyError> {
    let Some(appmsg) = APPMSG.find(content) else {
        return Ok(None);
    };
    let xml = appmsg.as_str();
    // The first <type> belongs to appmsg itself; nested elements come later.
    if !types.contains(&tag(xml, "type").as_str()) {
        return Ok(None);
    }
    let url = tag(xml, "url");
    if !matches!(split(&url)?.scheme.as_str(), "http" | "https") {
        return Ok(None);
    }
    let cover = tag(xml, "coverpicimageurl");
    let count = tag(xml, "piccount");
    let title = tag(xml, "title");
    let description = tag(xml, "des");
    let account = tag(xml, "sourcedisplayname");
    let show_type = tag(xml, "itemshowtype");
    let picture_count = if py_isdigit(&count) {
        py_int(&fav_core::pyjson::Value::Str(count))?
    } else {
        0
    };
    Ok(Some(ArticleLink {
        url,
        title,
        description,
        account,
        show_type,
        picture_count,
        cover_url: if IMAGE_HOST_FULL.is_match(&cover) { cover } else { String::new() },
    }))
}

/// `parse_article_link`: the 公众号 article a message shares, or `None` for anything else.
pub fn parse_article_link(content: &str) -> Result<Option<ArticleLink>, PyError> {
    let Some(mut link) = appmsg_link(content, &[LINK_SHARE_TYPE])? else {
        return Ok(None);
    };
    if split(&link.url)?.hostname().as_deref() != Some(ARTICLE_HOST) {
        return Ok(None);
    }
    if let Some(rest) = link.url.strip_prefix("http:") {
        link.url = format!("https:{rest}");
    }
    Ok(Some(link))
}

/// `cover_only_page`: what can be saved of an image post whose page is behind the captcha:
/// its first picture.
pub fn cover_only_page(link: &ArticleLink) -> Option<ArticlePage> {
    if link.show_type != IMAGE_POST || link.cover_url.is_empty() {
        return None;
    }
    Some(ArticlePage {
        title: link.title.clone(),
        account: link.account.clone(),
        published_at: None,
        images: vec![link.cover_url.clone()],
        picture_count: link.picture_count.max(1),
        videos: Vec::new(),
    })
}

/// `canonical_url`: the article's URL without the sharer's tracking parameters, for links
/// and display. Short links (`/s/<id>`) are already canonical; fetching should still use
/// the URL from the message, which carries the `chksm` the site checks.
pub fn canonical_url(url: &str) -> Result<String, PyError> {
    let mut parts = split(url)?;
    if parts.hostname().as_deref() != Some(ARTICLE_HOST) {
        // Links to other sites are built canonical by their parsers; an NGA thread is
        // named by its query.
        parts.fragment.clear();
        return Ok(urlunsplit(&parts));
    }
    let keys = ["__biz", "mid", "idx", "sn"];
    let values: Vec<Option<String>> = keys.iter().map(|key| query_first(&parts.query, key)).collect();
    if parts.path != "/s" || values.iter().any(Option::is_none) {
        return Ok(format!("https://{ARTICLE_HOST}{}", parts.path));
    }
    let params: Vec<String> = keys
        .iter()
        .zip(values)
        .map(|(key, value)| format!("{key}={}", value.unwrap_or_default()))
        .collect();
    Ok(format!("https://{ARTICLE_HOST}/s?{}", params.join("&")))
}

fn script_string(page: &str, pattern: &regex::Regex) -> String {
    pattern
        .captures(page)
        .map(|found| py_strip(&unescape(&found[1])).to_owned())
        .unwrap_or_default()
}

/// `_js_unescape`: a script string's escapes undone, then its HTML entities.
///
/// `\uD800`-style escapes of a lone surrogate become U+FFFD: a Rust string cannot hold one
/// (Python's can; DIVERGENCES.md D8).
pub(crate) fn js_unescape(text: &str) -> String {
    let replaced = JS_ESCAPE.replace_all(text, |found: &regex::Captures<'_>| {
        let escape = &found[1];
        if escape.chars().count() > 1 {
            let code = u32::from_str_radix(&escape[1..], 16).unwrap_or(0xFFFD);
            return char::from_u32(code).unwrap_or('\u{FFFD}').to_string();
        }
        match escape {
            "n" => "\n".to_owned(),
            "t" => "\t".to_owned(),
            "r" => "\r".to_owned(),
            other => other.to_owned(),
        }
    });
    unescape(&replaced).into_owned()
}

/// `_top_level_objects`: the text of each object in the script array opening at byte
/// `start`, nested values left out.
fn top_level_objects(page: &str, start: usize) -> Vec<String> {
    let scanned = chars_after(page, start, MAX_LIST_CHARS);
    let mut objects = Vec::new();
    let mut current = String::new();
    let mut depth = 1_usize; // inside the array
    for token in JS_TOKEN.find_iter(scanned) {
        let text = token.as_str();
        match text {
            "{" | "[" => {
                depth += 1;
                if depth == ELEMENT_DEPTH {
                    current.clear();
                }
            }
            "}" | "]" => {
                depth -= 1;
                if depth == ELEMENT_DEPTH - 1 {
                    objects.push(std::mem::take(&mut current));
                } else if depth == 0 {
                    break;
                }
            }
            _ if depth == ELEMENT_DEPTH => current.push_str(text),
            _ => {}
        }
    }
    objects
}

/// `picture_list`: the pictures of an image post, in order, from its
/// `picture_page_info_list`.
pub fn picture_list(page: &str) -> Vec<String> {
    for found in PICTURE_LIST.find_iter(page) {
        let mut urls: Vec<String> = Vec::new();
        for item in top_level_objects(page, found.end()) {
            // A search that exceeds the backtracking limit counts as no match (`pyre`).
            let url = match CDN_URL.captures(&item) {
                Ok(Some(captures)) => py_strip(&js_unescape(captures.get(2).map_or("", |group| group.as_str()))).to_owned(),
                _ => String::new(),
            };
            if IMAGE_HOST_FULL.is_match(&url) && !urls.contains(&url) {
                urls.push(url);
            }
        }
        if !urls.is_empty() {
            return urls;
        }
    }
    Vec::new()
}

/// `parse_article_page`: an article's title, account, publish time and pictures.
pub fn parse_article_page(page: &str) -> Result<ArticlePage, PyError> {
    let mut title = script_string(page, &MSG_TITLE);
    if title.is_empty() {
        title = script_string(page, &OG_TITLE);
    }
    let account = script_string(page, &NICKNAME);
    let published_at = match CREATED.captures(page) {
        Some(found) => Some(fromtimestamp_utc(digit_timestamp(&found[1])?)?),
        None => None,
    };
    // An image post's body is just its caption; the pictures are in the list.
    let mut images = picture_list(page);
    if images.is_empty() {
        let body = page.find(r#"id="js_content""#).map_or("", |start| &page[start..]);
        for found in BODY_IMAGE.captures_iter(body) {
            let url = unescape(&found[1]).into_owned();
            if !images.contains(&url) {
                images.push(url);
            }
        }
    }
    Ok(ArticlePage {
        title,
        account,
        published_at,
        images,
        ..ArticlePage::default()
    })
}

/// `original_image_url`: ask mmbiz for the full-size image rather than the 640px rendition.
pub fn original_image_url(url: &str) -> Result<String, PyError> {
    let mut parts = split(url)?;
    // A search past the backtracking limit leaves the path as it is.
    if let Ok(Some(found)) = WIDTH_SEGMENT.find(&parts.path) {
        parts.path = format!("{}/0{}", &parts.path[..found.start()], &parts.path[found.end()..]);
    }
    Ok(urlunsplit(&parts))
}

/// `image_extension`: from the content type, else the `wx_fmt` parameter, else `jpg`.
pub fn image_extension(url: &str, content_type: &str) -> Result<&'static str, PyError> {
    let kind = py_strip(content_type.split(';').next().unwrap_or_default()).to_lowercase();
    if let Some(subtype) = kind.strip_prefix("image/")
        && let Some(extension) = extension_for(subtype)
    {
        return Ok(extension);
    }
    let format = query_first(&split(url)?.query, "wx_fmt").unwrap_or_default().to_lowercase();
    Ok(extension_for(&format).unwrap_or("jpg"))
}

/// `is_captcha`: a URL on one of the captcha paths.
pub fn is_captcha(url: &str) -> Result<bool, PyError> {
    Ok(CAPTCHA_PATHS.contains(&split(url)?.path.as_str()))
}
