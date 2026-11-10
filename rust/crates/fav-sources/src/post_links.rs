//! Links to posts, saved the same way whichever source the link arrives through.
//!
//! Python origin: `src/web/post_links.py`.
//!
//! WeChat (a shared link card) and Telegram (a message's text or link preview) both hand
//! over links to the same kinds of post, and this module is the one place that knows
//! them: 公众号 articles ([`crate::wechat_article`]), Tieba posts ([`crate::tieba_post`]),
//! NGA threads ([`crate::nga_post`]) and Zhihu answers, column articles and pins
//! ([`crate::zhihu_post`]). It recognises a link, reads the post as an [`ArticlePage`] of
//! pictures and videos, and downloads those. Where the files go, and how the save is
//! recorded and announced, stays with each source.
//!
//! Two of the kinds need something only a person can set up: an NGA thread is read with
//! the NGA login captured on the 登录态 page, and a Zhihu post is looked up through TikHub
//! with the key in `services.tikhub`. Without either, the error says so in
//! [`PostLinkError::needs`], so a source can wait for it rather than spend its tries. A
//! 公众号 image post is put behind a captcha whatever the client; that is
//! [`PostLinkError::captcha`], which WeChat answers by opening the post in WeChat itself.
//!
//! Python reads and downloads everything with one `httpx.AsyncClient` and passes a
//! per-request timeout for the TikHub lookup, the lens call and the video download;
//! [`PostClient`] holds one client per timeout instead (DIVERGENCES.md D127).

use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use bytes::Bytes;
use fav_db::{Db, SettingsStore};
use fav_net::client::{BuildError, header_text};
use fav_net::reqwest::header::HeaderMap;
use fav_net::{ClientOptions, HttpClient, Request, StatusError, TransportError};
use fav_runtime::BoxFuture;
use fav_runtime::site_sessions::{self, SiteSession};
use tokio::io::AsyncWriteExt as _;
use url::Url;

use crate::kit::{io_class_name, py_os_error_text};
use crate::nga_post::{self, NgaError};
use crate::pydecode::PyError;
use crate::pyre::compile;
use crate::pyval::{split, unique};
use crate::tieba_post::{self, TiebaError};
use crate::wechat_article::{
    ArticleLink, ArticlePage, LINK_SHARE_TYPE, PostVideo, VIDEO_SHARE_TYPE, appmsg_link, canonical_url, image_extension, is_captcha,
    original_image_url, parse_article_link, parse_article_page,
};
use crate::zhihu_post::{self, ZhihuError};

const ARTICLE_HOST: &str = "mp.weixin.qq.com";
const REQUEST_TIMEOUT_SECONDS: u64 = 60;
/// How long a post's video may take to download from its CDN.
pub const VIDEO_READ_SECONDS: u64 = 600;
/// `httpx.Timeout(30.0, read=60.0)`, the TikHub lookup's.
const LOOKUP_CONNECT_SECONDS: u64 = 30;
const LOOKUP_READ_SECONDS: u64 = 60;
/// The lens call's `timeout=30.0`.
const LENS_TIMEOUT_SECONDS: u64 = 30;
/// A phone browser, as a share link would open in. mp.weixin.qq.com serves the full page
/// to it without a login.
pub const USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 12) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36";
static URL_IN_TEXT: LazyLock<regex::Regex> = LazyLock::new(|| compile("https?://[^\\s<>\"'\\]\u{ff09}\u{3011}]+", true, false));
const MAGIC_EXTENSIONS: [(&[u8], &str); 3] = [(b"\xff\xd8\xff", "jpg"), (b"\x89PNG", "png"), (b"GIF8", "gif")];

/// `Needs`: what a failure waits on, a login to capture or a key to set, not another try.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Needs {
    #[default]
    Nothing,
    NgaLogin,
    TikHub,
}

impl Needs {
    /// The value Python's `needs` holds: `''`, `'nga-login'` or `'tikhub'`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nothing => "",
            Self::NgaLogin => "nga-login",
            Self::TikHub => "tikhub",
        }
    }
}

/// `PostLinkError`: a linked post that could not be read or saved. `captcha` is
/// `PostCaptchaError`, mp.weixin.qq.com's captcha, as it answers every 公众号 image post.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PostLinkError {
    pub message: String,
    pub needs: Needs,
    pub captcha: bool,
}

impl PostLinkError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            needs: Needs::Nothing,
            captcha: false,
        }
    }

    fn needing(message: impl Into<String>, needs: Needs) -> Self {
        Self {
            needs,
            ..Self::new(message)
        }
    }

    /// The Python class name: `PostCaptchaError` or `PostLinkError`.
    pub fn class_name(&self) -> &'static str {
        if self.captcha { "PostCaptchaError" } else { "PostLinkError" }
    }
}

/// What [`PostLinks::fetch_page`] fails with: the post's own error, or any other
/// exception Python lets through (an HTTP status, a database failure, a value `int()`
/// refuses).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PostError {
    #[error(transparent)]
    Link(PostLinkError),
    #[error("{}: {}", .0.class, .0.message)]
    Other(PyError),
}

impl From<PyError> for PostError {
    fn from(error: PyError) -> Self {
        Self::Other(error)
    }
}

impl From<PostLinkError> for PostError {
    fn from(error: PostLinkError) -> Self {
        Self::Link(error)
    }
}

/// A transport failure as Python's exception: httpx's class and the cause's text.
pub(crate) fn transport_error(error: &TransportError) -> PyError {
    PyError::new(error.class.name(), error.message.clone())
}

/// `isinstance(exc, httpx.HTTPError)` for an exception a request raised: everything but
/// `httpx.InvalidURL`, which is not one.
pub(crate) fn is_http_error(error: &PyError) -> bool {
    error.class != "InvalidURL"
}

/// The URLs the post parsers read that are not in the data they are given. Tests point
/// them at a fake web, which serves plain HTTP only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// Tieba's `c/f/pb/page`.
    pub tieba_page: String,
    /// NGA's `read.php`.
    pub nga_read: String,
    /// Zhihu's lens video API, to which the video id is appended.
    pub lens: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            tieba_page: tieba_post::PAGE_API.to_owned(),
            nga_read: nga_post::READ_URL.to_owned(),
            lens: zhihu_post::LENS_API.to_owned(),
        }
    }
}

/// `new_client()`: what posts are read and downloaded with.
///
/// The cluster reaches these sites directly, so a deployment-wide proxy is not used. No
/// Referer: mmbiz answers an image request carrying a foreign one with a 140x140
/// placeholder. Each of Python's per-request timeouts is a client of its own here.
#[derive(Debug, Clone)]
pub struct PostClient {
    page: HttpClient,
    lookup: HttpClient,
    lens: HttpClient,
    video: HttpClient,
    endpoints: Endpoints,
}

impl PostClient {
    /// `new_client()`.
    pub fn new() -> Result<Self, BuildError> {
        Self::with(None, Endpoints::default())
    }

    /// The client through `proxy` (a fake web in tests) and with other endpoints.
    pub fn with(proxy: Option<&str>, endpoints: Endpoints) -> Result<Self, BuildError> {
        let build = |read: u64, connect: u64| {
            HttpClient::new(&ClientOptions {
                headers: vec![("User-Agent".to_owned(), USER_AGENT.to_owned())],
                timeout: Duration::from_secs(read),
                connect_timeout: Some(Duration::from_secs(connect)),
                follow_redirects: true,
                proxy: proxy.map(str::to_owned),
                trust_env: false,
                ..ClientOptions::default()
            })
        };
        Ok(Self {
            page: build(REQUEST_TIMEOUT_SECONDS, REQUEST_TIMEOUT_SECONDS)?,
            lookup: build(LOOKUP_READ_SECONDS, LOOKUP_CONNECT_SECONDS)?,
            lens: build(LENS_TIMEOUT_SECONDS, LENS_TIMEOUT_SECONDS)?,
            video: build(VIDEO_READ_SECONDS, REQUEST_TIMEOUT_SECONDS)?,
            endpoints,
        })
    }

    /// The client with the 60 s timeout, for pages and pictures.
    pub fn http(&self) -> &HttpClient {
        &self.page
    }

    pub(crate) fn lookup(&self) -> &HttpClient {
        &self.lookup
    }

    pub(crate) fn lens(&self) -> &HttpClient {
        &self.lens
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    /// `fetch_image`: one of a post's pictures at full size, else as the page shows it,
    /// with its extension; `None` when neither loads.
    pub async fn fetch_image(&self, url: &str) -> Result<Option<(Bytes, &'static str)>, PyError> {
        let log = fav_core::logging::get("links");
        for candidate in unique(vec![original_image_url(url)?, url.to_owned()]) {
            let fetched = match Fetched::send(&self.page, Request::get(candidate.clone())).await {
                Ok(fetched) => fetched,
                Err(error) if is_http_error(&error) => {
                    log.info(format!("Post image {candidate} failed: {}", error.message));
                    continue;
                }
                Err(error) => return Err(error),
            };
            if let Err(error) = fetched.raise_for_status() {
                log.info(format!("Post image {candidate} failed: {}", error.message));
                continue;
            }
            let content_type = fetched.header("content-type").unwrap_or_default();
            if let Some(refusal) = fetched.header("error-message").filter(|refusal| !refusal.is_empty()) {
                // Tieba's answer to an expired grant: its logo, with this header.
                log.info(format!("Post image {candidate} refused: {refusal}"));
                continue;
            }
            if !fetched.body.is_empty() && content_type.to_lowercase().starts_with("image/") {
                let extension = match sniff_extension(&fetched.body) {
                    Some(extension) => extension,
                    None => image_extension(&candidate, &content_type)?,
                };
                return Ok(Some((fetched.body, extension)));
            }
        }
        Ok(None)
    }

    /// `fetch_video`: download one of a post's videos to `partial`; `false`, with nothing
    /// left behind, when it does not load as an MP4. A file-system failure is the error.
    pub async fn fetch_video(&self, video: &PostVideo, partial: &Path) -> Result<bool, PyError> {
        let log = fav_core::logging::get("links");
        let failed = |message: &str| {
            log.info(format!("Post video {} failed: {message}", video.url));
        };
        let mut response = match self.video.send(Request::get(video.url.clone())).await {
            Ok(response) => response,
            Err(error) => {
                let error = transport_error(&error);
                if !is_http_error(&error) {
                    return Err(error);
                }
                failed(&error.message);
                remove(partial).await?;
                return Ok(false);
            }
        };
        if let Err(error) = response.error_for_status() {
            failed(&error.to_string());
            remove(partial).await?;
            return Ok(false);
        }
        if let Some(folder) = partial.parent() {
            tokio::fs::create_dir_all(folder).await.map_err(|error| os_error(&error, folder))?;
        }
        let mut file = tokio::fs::File::create(partial).await.map_err(|error| os_error(&error, partial))?;
        let mut head: Vec<u8> = Vec::with_capacity(16);
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if head.len() < 16 {
                        head.extend(chunk.iter().take(16 - head.len()));
                    }
                    file.write_all(&chunk).await.map_err(|error| os_error(&error, partial))?;
                }
                Ok(None) => break,
                Err(error) => {
                    drop(file);
                    failed(&error.message);
                    remove(partial).await?;
                    return Ok(false);
                }
            }
        }
        file.flush().await.map_err(|error| os_error(&error, partial))?;
        drop(file);
        if !is_mp4(&head) {
            log.info(format!("Post video {} is not an MP4", video.url));
            remove(partial).await?;
            return Ok(false);
        }
        Ok(true)
    }
}

/// `OSError` with Python's text, naming the file.
fn os_error(error: &std::io::Error, path: &Path) -> PyError {
    PyError::new(
        io_class_name(error),
        format!(
            "{}: {}",
            py_os_error_text(error),
            fav_runtime::pystr::repr_str(&path.to_string_lossy())
        ),
    )
}

/// `path.unlink(missing_ok=True)`.
async fn remove(path: &Path) -> Result<(), PyError> {
    match tokio::fs::remove_file(path).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(os_error(&error, path)),
        _ => Ok(()),
    }
}

/// A response read whole, as `await client.get(...)` gives it.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub status: u16,
    /// `response.url`: the last URL after redirects.
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Bytes,
    status_error: Option<StatusError>,
}

impl Fetched {
    /// Send `request` and read the whole body; a transport failure while sending or
    /// reading is httpx's exception for it.
    pub async fn send(client: &HttpClient, request: Request) -> Result<Self, PyError> {
        let response = client.send(request).await.map_err(|error| transport_error(&error))?;
        let status = response.status();
        let url = response.url().clone();
        let headers = response.headers().clone();
        let status_error = response.error_for_status().err();
        let body = response.bytes().await.map_err(|error| transport_error(&error))?;
        Ok(Self {
            status,
            url,
            headers,
            body,
            status_error,
        })
    }

    /// `response.raise_for_status()`: `HTTPStatusError` for anything but a 2xx.
    pub fn raise_for_status(&self) -> Result<(), PyError> {
        match &self.status_error {
            Some(error) => Err(PyError::new("HTTPStatusError", error.to_string())),
            None => Ok(()),
        }
    }

    /// A header as text, as httpx reads it.
    pub fn header(&self, name: &str) -> Option<String> {
        header_text(&self.headers, name)
    }
}

/// `link_for_url`: the post a URL points at, under its canonical URL, or `None` for a site
/// no source saves posts of. A 公众号 article keeps the URL as given: the site checks the
/// `chksm` it carries.
pub fn link_for_url(url: &str, title: &str, description: &str) -> Result<Option<ArticleLink>, PyError> {
    let url = fav_core::pytext::py_strip(url);
    let parts = split(url)?;
    if !matches!(parts.scheme.as_str(), "http" | "https") {
        return Ok(None);
    }
    if parts.hostname().as_deref() == Some(ARTICLE_HOST) {
        let url = match url.strip_prefix("http:") {
            Some(rest) => format!("https:{rest}"),
            None => url.to_owned(),
        };
        return Ok(Some(ArticleLink::new(url, title, description)));
    }
    if let Some(thread) = tieba_post::thread_id(url)? {
        return Ok(Some(ArticleLink::new(
            format!("https://tieba.baidu.com/p/{thread}"),
            title,
            description,
        )));
    }
    if let Some(tid) = nga_post::nga_thread_id(url)? {
        return Ok(Some(ArticleLink::new(
            format!("{}?tid={tid}", nga_post::READ_URL),
            title,
            description,
        )));
    }
    Ok(zhihu_post::parse_zhihu_url(url)?.map(|zhihu| ArticleLink::new(zhihu.url(), title, description)))
}

/// `find_link`: the first saveable post linked from any of `texts`, such as a message and
/// its link preview.
pub fn find_link(texts: &[&str]) -> Result<Option<ArticleLink>, PyError> {
    for text in texts {
        for found in URL_IN_TEXT.find_iter(text) {
            if let Some(link) = link_for_url(found.as_str(), "", "")? {
                return Ok(Some(link));
            }
        }
    }
    Ok(None)
}

/// `first_url`: the first web address in any of `texts`, saveable or not; empty when there
/// is none.
pub fn first_url(texts: &[&str]) -> String {
    texts
        .iter()
        .find_map(|text| URL_IN_TEXT.find(text))
        .map(|found| found.as_str().to_owned())
        .unwrap_or_default()
}

/// `unsupported_reason`: why a message was not saved, for the notification that says so.
/// `what` names the message when it links nothing: a text, a file, a sticker.
pub fn unsupported_reason(url: &str, what: &str) -> String {
    if url.is_empty() {
        format!("Nothing to download: {what}")
    } else {
        format!("Link not supported: {url}")
    }
}

/// `shared_link`: the post a WeChat link card shares, if it is one this module reads.
///
/// A 公众号 article keeps everything its card says (an image post's picture count and cover
/// among it); a post on another site is read from its link alone. The Tieba app shares a
/// video post as a video card rather than a link card.
pub fn shared_link(content: &str) -> Result<Option<ArticleLink>, PyError> {
    if let Some(article) = parse_article_link(content)? {
        return Ok(Some(article));
    }
    let Some(card) = appmsg_link(content, &[LINK_SHARE_TYPE, VIDEO_SHARE_TYPE])? else {
        return Ok(None);
    };
    if split(&card.url)?.hostname().as_deref() == Some(ARTICLE_HOST) {
        return Ok(None);
    }
    link_for_url(&card.url, &card.title, &card.description)
}

/// `site_name`: which site a link's post is on, for logs and messages.
pub fn site_name(link: &ArticleLink) -> Result<&'static str, PyError> {
    if tieba_post::thread_id(&link.url)?.is_some() {
        return Ok("Tieba");
    }
    if nga_post::nga_thread_id(&link.url)?.is_some() {
        return Ok("NGA");
    }
    if zhihu_post::parse_zhihu_url(&link.url)?.is_some() {
        return Ok("Zhihu");
    }
    Ok("WeChat article")
}

/// `sniff_extension`: a picture's type from its first bytes.
pub fn sniff_extension(content: &[u8]) -> Option<&'static str> {
    for (magic, extension) in MAGIC_EXTENSIONS {
        if content.starts_with(magic) {
            return Some(extension);
        }
    }
    (content.get(..4) == Some(b"RIFF".as_slice()) && content.get(8..12) == Some(b"WEBP".as_slice())).then_some("webp")
}

/// `is_mp4`: whether a file's first bytes are an MP4's `ftyp` box.
pub fn is_mp4(head: &[u8]) -> bool {
    head.get(4..8) == Some(b"ftyp".as_slice())
}

/// The stored site sessions `fetch_page` reads the NGA login from (`src.tool.site_sessions`).
pub trait SessionStore: Send + Sync {
    /// `site_sessions.load(site)`.
    fn load<'a>(&'a self, site: &'a str) -> BoxFuture<'a, Result<Option<SiteSession>, fav_runtime::Error>>;
    /// `site_sessions.record_check(site, ok=..., message=...)`.
    fn record_check<'a>(&'a self, site: &'a str, ok: bool, message: &'a str) -> BoxFuture<'a, Result<(), fav_runtime::Error>>;
}

impl SessionStore for Db {
    fn load<'a>(&'a self, site: &'a str) -> BoxFuture<'a, Result<Option<SiteSession>, fav_runtime::Error>> {
        Box::pin(site_sessions::load(self, site))
    }

    fn record_check<'a>(&'a self, site: &'a str, ok: bool, message: &'a str) -> BoxFuture<'a, Result<(), fav_runtime::Error>> {
        Box::pin(site_sessions::record_check(self, site, ok, message))
    }
}

/// A store failure as Python's exception: psycopg's class for a database error.
fn store_error(error: &fav_runtime::Error) -> PyError {
    let job = match error {
        fav_runtime::Error::Db(error) => crate::pyerr::db_error(error),
        fav_runtime::Error::Settings(error) => crate::settings_job_error(error),
        fav_runtime::Error::Invalid(message) => return PyError::new("ValueError", message.clone()),
        fav_runtime::Error::Failed(message) => return PyError::new("RuntimeError", message.clone()),
    };
    PyError::new(job.class_name(), job.message())
}

/// What reading a linked post needs besides the client: the settings (the TikHub key) and
/// the stored sessions (the NGA login).
#[derive(Clone)]
pub struct PostLinks {
    client: PostClient,
    settings: Arc<SettingsStore>,
    sessions: Arc<dyn SessionStore>,
}

impl std::fmt::Debug for PostLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostLinks").field("client", &self.client).finish_non_exhaustive()
    }
}

impl PostLinks {
    pub fn new(client: PostClient, settings: Arc<SettingsStore>, sessions: Arc<dyn SessionStore>) -> Self {
        Self {
            client,
            settings,
            sessions,
        }
    }

    pub fn client(&self) -> &PostClient {
        &self.client
    }

    /// `_nga_cookies`: the stored NGA login, or the error that waits for one.
    async fn nga_cookies(&self) -> Result<Vec<(String, String)>, PostError> {
        let stored = self
            .sessions
            .load(nga_post::SESSION_SITE)
            .await
            .map_err(|error| store_error(&error))?;
        let login = stored.map(|stored| nga_post::login_cookies(&stored.cookies)).unwrap_or_default();
        if login.len() < nga_post::LOGIN_COOKIES.len() {
            return Err(PostLinkError::needing(
                format!(
                    "An NGA thread needs an NGA login, and NGA is not signed in. {}",
                    nga_post::RELOGIN_HINT
                ),
                Needs::NgaLogin,
            )
            .into());
        }
        Ok(login)
    }

    async fn record_check(&self, ok: bool, message: &str) -> Result<(), PostError> {
        self.sessions
            .record_check(nga_post::SESSION_SITE, ok, message)
            .await
            .map_err(|error| store_error(&error).into())
    }

    async fn nga_page(&self, link: &ArticleLink) -> Result<ArticlePage, PostError> {
        let cookies = self.nga_cookies().await?;
        match nga_post::fetch_nga_post(&self.client, link, &cookies).await {
            Ok(page) => {
                self.record_check(true, "").await?;
                Ok(page)
            }
            Err(NgaError::Login(message)) => {
                self.record_check(false, &message).await?;
                Err(PostLinkError::needing(message, Needs::NgaLogin).into())
            }
            Err(NgaError::Nga(message)) => Err(PostLinkError::new(message).into()),
            Err(NgaError::Other(error)) => Err(error.into()),
        }
    }

    async fn zhihu_page(&self, link: &ArticleLink) -> Result<ArticlePage, PostError> {
        let Some(zhihu) = zhihu_post::parse_zhihu_url(&link.url)? else {
            return Err(PostLinkError::new(format!("{} is not a Zhihu post", link.url)).into());
        };
        let settings = self
            .settings
            .load()
            .await
            .map_err(|error| PyError::new("RuntimeError", error.to_string()))?;
        let tikhub = &settings.services.tikhub;
        match zhihu_post::fetch_page(&self.client, &zhihu, &tikhub.api_url, &tikhub.token).await {
            Ok(page) => Ok(page),
            Err(ZhihuError::Account(message)) => Err(PostLinkError::needing(message, Needs::TikHub).into()),
            Err(ZhihuError::Zhihu(message)) => Err(PostLinkError::new(message).into()),
            Err(ZhihuError::Other(error)) => Err(error.into()),
        }
    }

    /// `fetch_page`: the linked post as pictures and videos, with the page's HTML where
    /// one was read.
    pub async fn fetch_page(&self, link: &ArticleLink) -> Result<(ArticlePage, String), PostError> {
        if tieba_post::thread_id(&link.url)?.is_some() {
            return match tieba_post::fetch_tieba_post(&self.client, link).await {
                Ok(page) => Ok((page, String::new())),
                Err(TiebaError::Tieba(message)) => Err(PostLinkError::new(message).into()),
                Err(TiebaError::Other(error)) => Err(error.into()),
            };
        }
        if nga_post::nga_thread_id(&link.url)?.is_some() {
            return Ok((self.nga_page(link).await?, String::new()));
        }
        if zhihu_post::parse_zhihu_url(&link.url)?.is_some() {
            return Ok((self.zhihu_page(link).await?, String::new()));
        }
        let fetched = match Fetched::send(self.client.http(), Request::get(link.url.clone())).await {
            Ok(fetched) => fetched,
            Err(error) if is_http_error(&error) => {
                return Err(PostLinkError::new(format!("Could not read {}: {}", canonical_url(&link.url)?, error.message)).into());
            }
            Err(error) => return Err(error.into()),
        };
        if is_captcha(fetched.url.as_str())? {
            return Err(PostLinkError {
                captcha: true,
                ..PostLinkError::new(format!(
                    "mp.weixin.qq.com answered {} with a captcha page",
                    canonical_url(&link.url)?
                ))
            }
            .into());
        }
        if fetched.status != 200 {
            return Err(PostLinkError::new(format!("Could not read {}: HTTP {}", canonical_url(&link.url)?, fetched.status)).into());
        }
        // `response.text`, read as UTF-8 whatever the page declares (D127).
        let text = String::from_utf8_lossy(&fetched.body).into_owned();
        Ok((parse_article_page(&text)?, text))
    }
}
