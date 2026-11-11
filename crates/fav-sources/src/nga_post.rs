//! NGA forum threads forwarded to the agent's WeChat account.
//!
//! Python origin: `src/web/nga_post.py`.
//!
//! Sharing an NGA thread into WeChat (from the NGA app, 恩基爱社区) sends the same
//! `<appmsg>` link card as a 公众号 article (`<type>5</type>`), with the thread's
//! `bbs.nga.cn/read.php?tid=<id>` link and its title.
//!
//! NGA shows most boards to signed-in members only, so the thread is read with the login
//! cookies (`ngaPassportUid` and `ngaPassportCid`) of the session captured on fav's 登录态
//! page, through `read.php`'s JSON output (`__output=11`).
//!
//! Only the first floor (`lou` 0), the post itself, is kept. Its pictures are `BBCode`
//! `[img]` tags in the content, mostly relative to the attachment host
//! (`./mon_YYYYMM/DD/<name>.jpg`) and often pointing at a rendition
//! (`<name>.jpg.medium.jpg`); the upload itself is the name without that suffix. Uploads
//! the post lists as attachments but does not show inline come after the inline ones.

use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use fav_core::pyhtml::unescape;
use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::{py_isdigit, py_strip};
use fav_core::pyurl::urlunsplit;
use fav_net::{ClientOptions, HttpClient, Request};
use fav_runtime::site_sessions::{StoredCookie, cookie_value};

use crate::post_links::{Fetched, PostClient};
use crate::pydecode::{PyError, json_from_bytes, urlencode};
use crate::pyre::{compile, query_first};
use crate::pyval::{digit_timestamp, fromtimestamp_utc, join, py_str, py_str_opt, split, str_or_empty, unique};
use crate::wechat_article::{ArticleLink, ArticlePage, LINK_SHARE_TYPE, appmsg_link};

/// Every hostname NGA serves the forum under; the share card uses bbs.nga.cn.
pub const NGA_HOSTS: [&str; 6] = ["bbs.nga.cn", "nga.cn", "ngabbs.com", "nga.178.com", "bbs.ngacn.cc", "g.nga.cn"];
pub const READ_URL: &str = "https://bbs.nga.cn/read.php";
/// img.nga.178.com, the host older posts and clients name, no longer resolves; img.nga.cn
/// serves the same /attachments/ paths.
const ATTACHMENT_BASE: &str = "https://img.nga.cn/attachments/";
pub const LOGIN_COOKIES: [&str; 2] = ["ngaPassportUid", "ngaPassportCid"];

/// The key the captured login is stored under in `site_sessions`, and what the 登录态 page
/// opens for it. The login sets its cookies on .nga.cn, which bbs.nga.cn reads.
pub const SESSION_SITE: &str = "nga";
pub const LOGIN_URL: &str = "https://bbs.nga.cn/nuke.php?__lib=login&__act=account&login";
pub const SESSION_COOKIE_URLS: [&str; 1] = ["https://bbs.nga.cn/"];
pub const SESSION_COOKIE: &str = "ngaPassportCid";
pub const RELOGIN_HINT: &str = "Sign in to NGA again on fav's 登录态 page.";
/// The notification list: members get it, a guest gets 你必须先登录论坛. Cheap, and unlike
/// read.php it needs no thread.
pub const SESSION_CHECK_URL: &str = "https://bbs.nga.cn/nuke.php";
const SESSION_CHECK_PARAMS: [(&str, &str); 3] = [("__lib", "noti"), ("__act", "get_all"), ("__output", "11")];
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
const HTTP_OK: u16 = 200;
const FIRST_FLOOR: &str = "0";

static TID: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:\d{1,12})$", false, false));
static IMG_TAG: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"\[img\]\s*(.*?)\s*\[/img\]", true, true));
/// The renditions NGA derives from an upload, appended to its name.
static RENDITION: LazyLock<regex::Regex> = LazyLock::new(|| {
    compile(
        r"(\.(?:jpe?g|png|gif|webp|bmp))\.(?:medium|thumb|thumb_s|thumb_ss)\.jpe?g$",
        true,
        false,
    )
});
static RETIRED_IMAGE_HOST: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^img\d*\.nga\.178\.com$", false, false));
static TAG: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<[^>]+>", false, false));
static NUMBERED: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^\d+:", false, false));

/// Why an NGA thread could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NgaError {
    /// `NgaError`: NGA refused the thread or answered something else.
    #[error("{0}")]
    Nga(String),
    /// `NgaLoginError`: NGA wants a signed-in session the cookies do not give it.
    #[error("{0}")]
    Login(String),
    /// Any other exception Python lets through.
    #[error("{}: {}", .0.class, .0.message)]
    Other(PyError),
}

impl From<PyError> for NgaError {
    fn from(error: PyError) -> Self {
        Self::Other(error)
    }
}

/// `SessionCheck`: whether a captured NGA session is a live login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCheck {
    pub ok: bool,
    pub account: String,
    pub message: String,
}

/// `login_cookies`: the two cookies NGA reads a login from, out of a captured session, in
/// [`LOGIN_COOKIES`] order.
pub fn login_cookies(cookies: &[StoredCookie]) -> Vec<(String, String)> {
    LOGIN_COOKIES
        .iter()
        .filter_map(|name| {
            let value = cookie_value(cookies, name);
            (!value.is_empty()).then(|| ((*name).to_owned(), value.to_owned()))
        })
        .collect()
}

/// The client `verify_session` builds: NGA's browser user agent and the login cookies, a
/// 30 s timeout, no redirects, no proxy from the environment.
fn session_client(cookie: &str, proxy: Option<&str>) -> Result<HttpClient, PyError> {
    HttpClient::new(&ClientOptions {
        headers: vec![
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ("Cookie".to_owned(), cookie.to_owned()),
        ],
        timeout: Duration::from_secs(30),
        proxy: proxy.map(str::to_owned),
        trust_env: false,
        ..ClientOptions::default()
    })
    .map_err(|error| PyError::new("ValueError", error.to_string()))
}

/// `verify_session`: whether a captured NGA session is a live login, and for which uid.
///
/// NGA sets `ngaPassportCid` only once a sign-in completes; the notification list then
/// answers members and turns guests away with a 登录 message. Any other answer (NGA down, a
/// changed endpoint) does not hold the login up: the next thread read is what proves the
/// session from then on.
pub async fn verify_session(cookies: &[StoredCookie]) -> SessionCheck {
    verify_session_at(SESSION_CHECK_URL, None, cookies).await
}

/// [`verify_session`] against another check URL and through a proxy, for tests.
pub async fn verify_session_at(check_url: &str, proxy: Option<&str>, cookies: &[StoredCookie]) -> SessionCheck {
    let login = login_cookies(cookies);
    if login.len() < LOGIN_COOKIES.len() {
        return SessionCheck {
            ok: false,
            account: String::new(),
            message: "no ngaPassportUid/ngaPassportCid cookies yet; not signed in".to_owned(),
        };
    }
    let account = login[0].1.clone();
    let cookie = login
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    let unreachable = |class: &str| SessionCheck {
        ok: false,
        account: String::new(),
        message: format!("NGA unreachable: {class}"),
    };
    let Ok(client) = session_client(&cookie, proxy) else {
        return unreachable("UnsupportedProtocol");
    };
    let query: Vec<(&str, String)> = SESSION_CHECK_PARAMS
        .iter()
        .map(|(key, value)| (*key, (*value).to_owned()))
        .collect();
    let fetched = match Fetched::send(&client, Request::get(format!("{check_url}?{}", urlencode(&query)))).await {
        Ok(fetched) => fetched,
        Err(error) => return unreachable(&error.class),
    };
    let payload = json_from_bytes(&fetched.body).ok();
    let error = error_text(payload.as_ref());
    if error.contains("登录") {
        return SessionCheck {
            ok: false,
            account: String::new(),
            message: format!("NGA rejected the session ({error})"),
        };
    }
    if fetched.status != HTTP_OK || !matches!(payload, Some(Value::Dict(_))) {
        fav_core::logging::get("wechat").warning(format!(
            "NGA notifications answered HTTP {} without a verdict; accepting the login",
            fetched.status
        ));
    }
    SessionCheck {
        ok: true,
        account,
        message: String::new(),
    }
}

/// `nga_thread_id`: the thread id of an NGA `read.php?tid=<id>` link, or `None`.
pub fn nga_thread_id(url: &str) -> Result<Option<String>, PyError> {
    let parts = split(url)?;
    let host = parts.hostname().unwrap_or_default();
    if !matches!(parts.scheme.as_str(), "http" | "https") || !NGA_HOSTS.contains(&host.as_str()) {
        return Ok(None);
    }
    if !matches!(parts.path.trim_end_matches('/'), "/read.php" | "/read") {
        return Ok(None);
    }
    let tid = query_first(&parts.query, "tid").unwrap_or_default();
    Ok(TID.is_match(&tid).then_some(tid))
}

/// `parse_nga_link`: the NGA thread a message shares, under its canonical URL.
pub fn parse_nga_link(content: &str) -> Result<Option<ArticleLink>, PyError> {
    let Some(link) = appmsg_link(content, &[LINK_SHARE_TYPE])? else {
        return Ok(None);
    };
    Ok(nga_thread_id(&link.url)?.map(|tid| ArticleLink::new(format!("{READ_URL}?tid={tid}"), link.title, link.description)))
}

/// `original_url`: the upload an `[img]` tag shows, at full size; `None` for something
/// that is not a picture URL.
pub fn original_url(src: &str) -> Result<Option<String>, PyError> {
    let unescaped = unescape(src);
    let mut src = py_strip(&unescaped).to_owned();
    if let Some(rest) = src.strip_prefix("./") {
        src = join(ATTACHMENT_BASE, rest)?;
    } else if let Some(rest) = src.strip_prefix("/attachments/") {
        src = join(ATTACHMENT_BASE, rest)?;
    } else if src.starts_with("mon_") {
        src = join(ATTACHMENT_BASE, &src)?;
    }
    let mut parts = split(&src)?;
    if !matches!(parts.scheme.as_str(), "http" | "https") {
        return Ok(None);
    }
    if RETIRED_IMAGE_HOST.is_match(&parts.hostname().unwrap_or_default()) {
        "https".clone_into(&mut parts.scheme);
        "img.nga.cn".clone_into(&mut parts.netloc);
        src = urlunsplit(&parts);
    }
    Ok(Some(RENDITION.replace_all(&src, "$1").into_owned()))
}

/// The values of a dictionary or the items of a list (`d.values()` / the list itself).
fn values(value: Option<&Value>) -> Vec<&Value> {
    match value {
        Some(Value::Dict(dict)) => dict.iter().map(|(_, item)| item).collect(),
        Some(Value::List(items)) => items.iter().collect(),
        _ => Vec::new(),
    }
}

fn attachments(row: &Dict) -> Result<Vec<String>, PyError> {
    let mut urls = Vec::new();
    for item in values(row.get("attachs")) {
        let Value::Dict(item) = item else { continue };
        let kind = match item.get("type") {
            Some(kind) if kind.is_truthy() => py_str(kind),
            _ => "img".to_owned(),
        };
        if kind != "img" {
            continue;
        }
        if let Some(Value::Str(path)) = item.get("attachurl")
            && !path.is_empty()
        {
            let source = if path.starts_with("http://") || path.starts_with("https://") || path.starts_with("./") {
                path.clone()
            } else {
                format!("./{path}")
            };
            if let Some(url) = original_url(&source)?.filter(|url| !url.is_empty()) {
                urls.push(url);
            }
        }
    }
    Ok(urls)
}

/// `floor_images`: the pictures of one floor, inline ones first, at full size and without
/// repeats.
pub fn floor_images(row: &Dict) -> Result<Vec<String>, PyError> {
    let content = str_or_empty(row.get("content"));
    let mut images = Vec::new();
    for found in IMG_TAG.captures_iter(&content) {
        if let Some(url) = original_url(&found[1])?.filter(|url| !url.is_empty()) {
            images.push(url);
        }
    }
    images.extend(attachments(row)?);
    Ok(unique(images))
}

fn rows(data: &Dict) -> Vec<&Dict> {
    values(data.get("__R")).into_iter().filter_map(Value::as_dict).collect()
}

/// `_error_text`: NGA's message in an answer (`error`, else `data.__MESSAGE`), without tags
/// or its number.
pub(crate) fn error_text(payload: Option<&Value>) -> String {
    let Some(Value::Dict(payload)) = payload else {
        return String::new();
    };
    let mut error: Option<Value> = payload.get("error").cloned();
    if let Some(Value::List(items)) = &error {
        error = Some(items.first().cloned().unwrap_or_else(|| Value::Str(String::new())));
    }
    if !error.as_ref().is_some_and(Value::is_truthy)
        && let Some(Value::Dict(data)) = payload.get("data")
    {
        error = data.get("__MESSAGE").cloned();
        if let Some(Value::Dict(message)) = &error {
            error = match message.get("1") {
                Some(first) if first.is_truthy() => Some(first.clone()),
                _ => Some(
                    message
                        .iter()
                        .next()
                        .map_or_else(|| Value::Str(String::new()), |(_, value)| value.clone()),
                ),
            };
        }
    }
    let text = str_or_empty(error.as_ref());
    let stripped = TAG.replace_all(&unescape(&text), "").into_owned();
    // NGA numbers its messages: '1:未登录'.
    NUMBERED.replace(py_strip(&stripped), "").into_owned()
}

/// `_timestamp`: a digit string's `datetime.fromtimestamp(..., tz=UTC)`, else `None`.
pub(crate) fn timestamp(value: Option<&Value>) -> Result<Option<DateTime<Utc>>, PyError> {
    let raw = str_or_empty(value);
    let text = py_strip(&raw);
    if !py_isdigit(text) {
        return Ok(None);
    }
    Ok(Some(fromtimestamp_utc(digit_timestamp(text)?)?))
}

/// `fetch_nga_post`: the thread's first floor, as a page of its pictures, read with a
/// signed-in session. `cookies` are the login cookies by name.
pub async fn fetch_nga_post(client: &PostClient, link: &ArticleLink, cookies: &[(String, String)]) -> Result<ArticlePage, NgaError> {
    let Some(tid) = nga_thread_id(&link.url)? else {
        return Err(NgaError::Nga(format!("{} is not an NGA thread", link.url)));
    };
    let cookie = LOGIN_COOKIES
        .iter()
        .filter_map(|name| {
            cookies
                .iter()
                .find(|(key, _)| key == name)
                .filter(|(_, value)| !value.is_empty())
                .map(|(_, value)| format!("{name}={value}"))
        })
        .collect::<Vec<_>>()
        .join("; ");
    let query = urlencode(&[("tid", tid.clone()), ("__output", "11".to_owned())]);
    let request = Request::get(format!("{}?{query}", client.endpoints().nga_read)).header("Cookie", cookie);
    let fetched = Fetched::send(client.http(), request).await?;
    let payload = json_from_bytes(&fetched.body).ok();
    let error = error_text(payload.as_ref());
    let is_dict = matches!(payload, Some(Value::Dict(_)));
    if error.contains("登录") || (!is_dict && matches!(fetched.status, 401 | 403)) {
        let reason = if error.is_empty() {
            format!("HTTP {}", fetched.status)
        } else {
            error
        };
        return Err(NgaError::Login(format!(
            "NGA wants a signed-in session for thread {tid} ({reason}). {RELOGIN_HINT}"
        )));
    }
    let data = match &payload {
        Some(Value::Dict(payload)) => payload.get("data").and_then(Value::as_dict),
        _ => None,
    };
    let Some(data) = data.filter(|_| error.is_empty()) else {
        let reason = if error.is_empty() {
            format!("HTTP {}", fetched.status)
        } else {
            error
        };
        return Err(NgaError::Nga(format!(
            "NGA refused thread {tid}: {reason}; it may have been deleted"
        )));
    };
    let Some(first) = rows(data).into_iter().find(|row| py_str_opt(row.get("lou")) == FIRST_FLOOR) else {
        return Err(NgaError::Nga(format!("NGA returned no first floor for thread {tid}")));
    };
    let empty = Dict::new();
    let thread = data.get("__T").and_then(Value::as_dict).unwrap_or(&empty);
    let forum = data.get("__F").and_then(Value::as_dict).unwrap_or(&empty);
    let subject = match thread.get("subject") {
        Some(subject) if subject.is_truthy() => py_str(subject),
        _ => str_or_empty(first.get("subject")),
    };
    let title = py_strip(&unescape(&subject)).to_owned();
    let account = py_strip(&unescape(&str_or_empty(forum.get("name")))).to_owned();
    let published_at = match timestamp(thread.get("postdate"))? {
        Some(time) => Some(time),
        None => timestamp(first.get("postdatetimestamp"))?,
    };
    Ok(ArticlePage {
        title: if title.is_empty() { link.title.clone() } else { title },
        account,
        published_at,
        images: floor_images(first)?,
        ..ArticlePage::default()
    })
}
