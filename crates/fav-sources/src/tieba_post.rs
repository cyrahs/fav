//! Baidu Tieba posts forwarded to the agent's WeChat account.
//!
//! Python origin: `src/web/tieba_post.py`.
//!
//! Sharing a Tieba post into WeChat sends the same `<appmsg>` link card as a 公众号 article
//! (`<type>5</type>`), with the post's `tieba.baidu.com/p/<id>` link and its title. A video
//! post is shared as a video card (`<type>4</type>`) with the same link.
//!
//! The web page answers the cluster with Baidu's security check, but the Android client's
//! API does not: `c/f/pb/page` returns the post as JSON. Its form is signed with an MD5 of
//! the sorted parameters and a constant salt, as every Tieba client does. Only the first
//! floor, the post itself, is kept. Image items (`type` 3) carry `origin_src`, the
//! original upload with a fresh time-limited `tbpicau` grant. Video items (`type` 5) carry
//! the MP4 in `link`, signed with a `tbau` grant that lasts about a day, so it is
//! downloaded as soon as it is read.

use std::fmt::Write as _;
use std::sync::LazyLock;

use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::py_isdigit;
use md5::{Digest as _, Md5};

use crate::post_links::{Fetched, PostClient};
use crate::pydecode::{PyError, json_from_bytes, urlencode};
use crate::pyre::compile;
use crate::pyval::{digit_timestamp, first_truthy, fromtimestamp_utc, is_web_url, iter_dicts, py_str_opt, split, str_or_empty, unique};
use crate::wechat_article::{ArticleLink, ArticlePage, LINK_SHARE_TYPE, PostVideo, VIDEO_SHARE_TYPE, appmsg_link};

pub const PAGE_API: &str = "http://c.tieba.baidu.com/c/f/pb/page";
const CLIENT_VERSION: &str = "12.57.4.2";
const SIGN_SALT: &str = "tiebaclient!!!";
/// The first page of floors; only floor 1 is used.
const FLOORS: &str = "2";
const IMAGE_ITEM: &str = "3";
const VIDEO_ITEM: &str = "5";
static THREAD_PATH: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:/p/(\d+))$", false, false));

fn client_user_agent() -> String {
    format!("bdtb for Android {CLIENT_VERSION}")
}

/// Why a Tieba post could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TiebaError {
    /// `TiebaError`: Tieba refused the post or answered something else.
    #[error("{0}")]
    Tieba(String),
    /// Any other exception Python lets through (an HTTP status, a transport failure, a
    /// value `int()` or `datetime` refuses).
    #[error("{}: {}", .0.class, .0.message)]
    Other(PyError),
}

impl From<PyError> for TiebaError {
    fn from(error: PyError) -> Self {
        Self::Other(error)
    }
}

/// `thread_id`: the thread id of a `tieba.baidu.com/p/<id>` link, or `None` for anything else.
pub fn thread_id(url: &str) -> Result<Option<String>, PyError> {
    let parts = split(url)?;
    let host = parts.hostname().unwrap_or_default();
    if !matches!(parts.scheme.as_str(), "http" | "https") || !(host == "tieba.baidu.com" || host.ends_with(".tieba.baidu.com")) {
        return Ok(None);
    }
    Ok(THREAD_PATH
        .captures(parts.path.trim_end_matches('/'))
        .map(|found| found[1].to_owned()))
}

/// `parse_tieba_link`: the Tieba post a message shares, under its canonical URL.
pub fn parse_tieba_link(content: &str) -> Result<Option<ArticleLink>, PyError> {
    let Some(link) = appmsg_link(content, &[LINK_SHARE_TYPE, VIDEO_SHARE_TYPE])? else {
        return Ok(None);
    };
    Ok(thread_id(&link.url)?.map(|thread| ArticleLink::new(format!("https://tieba.baidu.com/p/{thread}"), link.title, link.description)))
}

/// `_signed`: the form with its `sign`, the upper-case MD5 of the sorted `key=value` pairs
/// and the salt.
pub(crate) fn signed(params: &[(String, String)]) -> Vec<(String, String)> {
    let mut sorted: Vec<&(String, String)> = params.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut raw = String::new();
    for (key, value) in sorted {
        let _ = write!(raw, "{key}={value}");
    }
    raw.push_str(SIGN_SALT);
    let digest = Md5::digest(raw.as_bytes());
    let sign = digest.iter().fold(String::new(), |mut sign, byte| {
        let _ = write!(sign, "{byte:02X}");
        sign
    });
    let mut out = params.to_vec();
    match out.iter_mut().find(|(key, _)| key == "sign") {
        Some(slot) => slot.1 = sign,
        None => out.push(("sign".to_owned(), sign)),
    }
    out
}

async fn first_page(client: &PostClient, thread: &str) -> Result<Dict, TiebaError> {
    let params: Vec<(String, String)> = [
        ("_client_type", "2"),
        ("_client_version", CLIENT_VERSION),
        ("kz", thread),
        ("pn", "1"),
        ("rn", FLOORS),
    ]
    .iter()
    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
    .collect();
    let form = signed(&params);
    let pairs: Vec<(&str, String)> = form.iter().map(|(key, value)| (key.as_str(), value.clone())).collect();
    let request = fav_net::Request::post(client.endpoints().tieba_page.clone(), urlencode(&pairs))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", client_user_agent());
    let fetched = Fetched::send(client.http(), request).await?;
    fetched.raise_for_status()?;
    let Ok(payload) = json_from_bytes(&fetched.body) else {
        return Err(TiebaError::Tieba(format!(
            "Tieba answered thread {thread} with something other than JSON"
        )));
    };
    match payload {
        Value::Dict(dict) if py_str_opt(dict.get("error_code")) == "0" => Ok(dict),
        other => {
            let error = match &other {
                Value::Dict(dict) => dict.get("error_msg"),
                _ => None,
            };
            let error = error
                .filter(|value| value.is_truthy())
                .map_or_else(|| "unknown error".to_owned(), crate::pyval::py_str);
            Err(TiebaError::Tieba(format!(
                "Tieba refused thread {thread}: {error}; it may have been deleted"
            )))
        }
    }
}

/// `_images`: the first floor's pictures, at full size where the item names it.
pub(crate) fn images(post: &Dict) -> Result<Vec<String>, PyError> {
    let mut images = Vec::new();
    for item in iter_dicts(post.get("content"))? {
        if py_str_opt(item.get("type")) != IMAGE_ITEM {
            continue;
        }
        if let Some(Value::Str(url)) = first_truthy(item, &["origin_src", "big_cdn_src", "cdn_src"])
            && (url.starts_with("http://") || url.starts_with("https://"))
        {
            images.push(url.clone());
        }
    }
    Ok(images)
}

/// `_videos`: the post's videos; a video post's own one is also in the thread's `video_info`.
pub(crate) fn videos(post: &Dict, thread: &Dict) -> Result<Vec<PostVideo>, PyError> {
    let mut videos: Vec<PostVideo> = Vec::new();
    for item in iter_dicts(post.get("content"))? {
        if py_str_opt(item.get("type")) == VIDEO_ITEM && is_web_url(item.get("link")) {
            let link = crate::pyval::as_str(item.get("link")).unwrap_or_default();
            let cover = if is_web_url(item.get("src")) {
                crate::pyval::as_str(item.get("src")).unwrap_or_default()
            } else {
                ""
            };
            if !videos.iter().any(|video| video.url == link) {
                videos.push(PostVideo::new(link, cover));
            }
        }
    }
    if videos.is_empty()
        && let Some(Value::Dict(info)) = thread.get("video_info")
        && is_web_url(info.get("video_url"))
    {
        let cover = if is_web_url(info.get("thumbnail_url")) {
            crate::pyval::as_str(info.get("thumbnail_url")).unwrap_or_default()
        } else {
            ""
        };
        videos.push(PostVideo::new(
            crate::pyval::as_str(info.get("video_url")).unwrap_or_default(),
            cover,
        ));
    }
    Ok(videos)
}

/// `fetch_tieba_post`: the post's first floor, as a page of its pictures and videos.
pub async fn fetch_tieba_post(client: &PostClient, link: &ArticleLink) -> Result<ArticlePage, TiebaError> {
    let Some(thread) = thread_id(&link.url)? else {
        return Err(TiebaError::Tieba(format!("{} is not a Tieba post", link.url)));
    };
    let payload = first_page(client, &thread).await?;
    let empty = Dict::new();
    let info = payload.get("thread").and_then(Value::as_dict).unwrap_or(&empty);
    let forum = payload.get("forum").and_then(Value::as_dict).unwrap_or(&empty);
    let created = str_or_empty(info.get("create_time"));
    let posts = iter_dicts(payload.get("post_list"))?;
    let Some(first) = posts.into_iter().find(|post| py_str_opt(post.get("floor")) == "1") else {
        return Err(TiebaError::Tieba(format!("Tieba returned no first floor for thread {thread}")));
    };
    let forum_name = str_or_empty(forum.get("name"));
    let title = match info.get("title") {
        Some(title) if title.is_truthy() => crate::pyval::py_str(title),
        _ => link.title.clone(),
    };
    let published_at = if py_isdigit(&created) {
        Some(fromtimestamp_utc(digit_timestamp(&created)?)?)
    } else {
        None
    };
    let images = unique(images(first)?);
    let videos = videos(first, info)?;
    Ok(ArticlePage {
        title,
        account: if forum_name.is_empty() {
            String::new()
        } else {
            format!("{forum_name}吧")
        },
        published_at,
        images,
        picture_count: 0,
        videos,
    })
}
