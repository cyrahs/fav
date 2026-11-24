//! Zhihu collections (收藏夹): the posts in the collections ticked in `web.zhihu`, the
//! account's own and the ones it follows, saved as pictures and videos.
//!
//! No Python origin (DIVERGENCES.md D189). Zhihu is read the way Zhihu links are: in the
//! Chromium profile signed in to 知乎 on the 登录态 page, through `web.rednote.proxy`, under
//! the profile's advisory lock (`fav_browser::zhihu`). Here the page is only a place to call
//! Zhihu's own API from, signed in as the page is ([`ZhihuApiSession`]):
//! `/api/v4/me` names the account, `/api/v4/people/<token>/collections` lists its own
//! collections and `/api/v4/members/<token>/following-favlists` the ones it follows (the
//! settings page ticks them from that list, [`list_collections`]), and
//! `/api/v4/collections/<id>/items` pages through a collection, newest first, each item with
//! the post's full API fields. So a post is parsed where it is listed, with
//! [`zhihu_post::parse_post`] like a shared link; nothing is opened per post. A video
//! (`zvideo`) carries its lens id and a play list.
//!
//! What is downloaded, and how a post is recognised again, is shared with the link sources:
//! pictures and videos through [`crate::post_links::PostClient`], the post's canonical URL
//! in `saved_post_link` ([`crate::saved_links`]), so a post already saved from Telegram,
//! WeChat or another collection is not saved twice. Every post seen in a collection is a
//! `zhihu_collection_item` row with what became of it: `pending` (seen, to download, with the
//! listing's entry in `content`), `saved`, `no_media` (a post of text alone, most answers),
//! `already_saved`, `unsupported` (a kind this module does not read), or `failed` until
//! [`MAX_ATTEMPTS`] tries, then `given_up` and reported.
//!
//! The browser is held only for the listing: a run walks every ticked collection first,
//! recording each new post, closes the browser, and then downloads the collections' pending
//! and failed posts, at most `max_posts_per_run` of them; the rest wait for the next run. A
//! post is recorded before anything is downloaded, so a run that stops half way loses nothing.
//! A collection is walked to its end once, at most [`MAX_PAGES_PER_COLLECTION`] pages a run
//! (a long first walk resumes at `resume_offset`); after that a run stops at the first page
//! whose posts are all known. A collection Zhihu no longer shows (deleted, made private) fails
//! the run once the other collections are done.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fav_core::filename::format_media_filename;
use fav_core::logging::{self, Logger};
use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::py_strip;
use fav_core::settings::{ZhihuCollection, ZhihuCollectionOwner};
use fav_db::{Db, SettingsStore};
use fav_net::retry::{Sleeper, tokio_sleeper};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox};
use fav_runtime::site_sessions;
use fav_runtime::source::{JobError, Registry, Source};
use sqlx::Row;

use crate::SourceContext;
use crate::notifier::{OutboxNotifier, SharedNotifier};
use crate::post_links::PostClient;
use crate::pydecode::{PyError, py_int};
use crate::pyre::strip_tags;
use crate::pyval::{as_str, py_str, str_or_empty};
use crate::saved_links::{self, NewSavedLink};
use crate::wechat_article::{ArticlePage, PostVideo};
use crate::zhihu_post::{self, ZhihuError, ZhihuKind, ZhihuLink, ZhihuVideo};

pub const JOB_KEY: &str = "zhihu";
/// The `JOB_SPECS` name the notifications are headed with.
pub const HEADER: &str = "Zhihu";
/// Where the API session's page is opened: the origin the API is called on.
pub const START_URL: &str = "https://www.zhihu.com/";
/// The dedupe key of the message that asks for a sign-in.
pub const LOGIN_DEDUPE_KEY: &str = "zhihu:login";
/// How many times a post whose media would not download is tried before it is given up.
pub const MAX_ATTEMPTS: i32 = 3;
const PAGE_SIZE: usize = 20;
/// A collection is walked this many pages at most in one run (3,000 posts).
pub const MAX_PAGES_PER_COLLECTION: usize = 150;
/// The account's collection lists are this long at most (1,000 each).
const MAX_LIST_PAGES: usize = 50;
/// Between two pages of the API: the pace of a person scrolling.
const PAGE_DELAY_SECONDS: f64 = 1.0;
const PIN_TITLE_BYTES: usize = 80;
/// How many post titles a collection's message lists.
const TITLES_IN_MESSAGE: usize = 5;

fn log() -> Logger {
    logging::get("zhihu")
}

// ---------- the API ----------

/// Zhihu's API, called from a page signed in to 知乎. Closed with [`ZhihuApiSession::close`].
pub trait ZhihuApiSession: Send + Sync {
    /// The JSON at `path` (on `www.zhihu.com`, query included), as a dict.
    fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Dict, ZhihuError>>;
    /// Close the browser and give the profile back.
    fn close(self: Box<Self>) -> BoxFuture<'static, ()>;
}

/// Opens [`ZhihuApiSession`]s.
pub trait ZhihuApiOpener: Send + Sync {
    fn open(&self) -> BoxFuture<'_, Result<Box<dyn ZhihuApiSession>, ZhihuError>>;
}

/// [`ZhihuApiOpener`] in Chromium: the profile next to RedNote's, through RedNote's proxy
/// (`web.rednote`), as [`zhihu_post::ChromiumZhihuReader`] reads a post.
pub struct ChromiumZhihuApi {
    db: Db,
    settings: Arc<SettingsStore>,
    executable: PathBuf,
}

impl std::fmt::Debug for ChromiumZhihuApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChromiumZhihuApi")
            .field("executable", &self.executable)
            .finish_non_exhaustive()
    }
}

impl ChromiumZhihuApi {
    pub fn new(db: Db, settings: Arc<SettingsStore>) -> Self {
        Self {
            db,
            settings,
            executable: fav_browser::chromium::playwright_chromium_executable(),
        }
    }

    async fn open_session(&self) -> Result<Box<dyn ZhihuApiSession>, ZhihuError> {
        let settings = self
            .settings
            .load()
            .await
            .map_err(|error| PyError::new("RuntimeError", error.to_string()))?;
        let rednote = &settings.web.rednote;
        let profile = fav_browser::zhihu::profile_dir(rednote.profile_path.as_path());
        // Always headless: the worker has no display (the login window brings its own).
        let browser = fav_browser::zhihu::ZhihuBrowser::new(self.executable.clone(), &profile, &rednote.proxy, true).map_err(|error| {
            ZhihuError::Outage(format!(
                "The Zhihu browser cannot use web.rednote.proxy ({}: {})",
                error.class, error.message
            ))
        })?;
        let lock = zhihu_post::lock_profile(
            &self.db,
            &fav_browser::zhihu::profile_lock_name(&profile),
            "reading Zhihu collections",
        )
        .await?;
        match browser.open_api(START_URL).await {
            Ok(session) => Ok(Box::new(ChromiumSession { session, lock })),
            Err(error) => {
                lock.release().await;
                Err(ZhihuError::Outage(format!("The Zhihu browser failed opening {START_URL}: {error}")))
            }
        }
    }
}

impl ZhihuApiOpener for ChromiumZhihuApi {
    fn open(&self) -> BoxFuture<'_, Result<Box<dyn ZhihuApiSession>, ZhihuError>> {
        Box::pin(self.open_session())
    }
}

struct ChromiumSession {
    session: fav_browser::zhihu::ZhihuApiSession,
    lock: fav_db::lock::AdvisoryLock,
}

impl ZhihuApiSession for ChromiumSession {
    fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<Dict, ZhihuError>> {
        Box::pin(async move { api_answer(path, self.session.get(path).await) })
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        let Self { session, lock } = *self;
        Box::pin(async move {
            // Only once the browser is down: the lock is what keeps a login out of the profile.
            session.close().await;
            lock.release().await;
        })
    }
}

/// What the page's call answered, as the dict or the error it is.
pub fn api_answer(path: &str, answer: fav_browser::zhihu::ApiAnswer) -> Result<Dict, ZhihuError> {
    use fav_browser::zhihu::ApiAnswer;
    match answer {
        ApiAnswer::Json { status: 200, body } => match Value::from_serde(&body) {
            Value::Dict(body) => Ok(body),
            _ => Err(ZhihuError::Outage(format!("Zhihu answered {path} with no JSON object"))),
        },
        ApiAnswer::Json { status, body } => {
            let message = body
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>();
            let text = format!(
                "Zhihu answered {path} with HTTP {status}{}",
                if message.is_empty() {
                    String::new()
                } else {
                    format!(": {message}")
                }
            );
            // 403 here is not about the login (`classify_api` took those): a collection
            // made private, say. The rest is Zhihu failing.
            if status == 403 || status == 404 {
                Err(ZhihuError::Zhihu(text))
            } else {
                Err(ZhihuError::Outage(text))
            }
        }
        ApiAnswer::SignedOut => Err(ZhihuError::Login(format!(
            "Zhihu is not signed in any more (calling {path}); sign in to 知乎 on the 登录态 page."
        ))),
        ApiAnswer::Challenge(href) => Err(ZhihuError::Outage(format!(
            "Zhihu put a security check in front of {path} ({href}); signing in to 知乎 again on the 登录态 page may clear it"
        ))),
        ApiAnswer::Failed(reason) => Err(ZhihuError::Outage(format!("Zhihu could not be asked for {path}: {reason}"))),
    }
}

fn dict_at<'a>(dict: &'a Dict, key: &str) -> Option<&'a Dict> {
    dict.get(key).and_then(Value::as_dict)
}

fn list_at<'a>(dict: &'a Dict, key: &str) -> &'a [Value] {
    match dict.get(key) {
        Some(Value::List(items)) => items,
        _ => &[],
    }
}

fn text_at(dict: &Dict, key: &str) -> String {
    py_strip(&str_or_empty(dict.get(key))).to_owned()
}

/// A page's `paging.is_end`; a page that does not say is the last.
fn is_end(page: &Dict) -> bool {
    dict_at(page, "paging")
        .and_then(|paging| paging.get("is_end"))
        .is_none_or(Value::is_truthy)
}

fn positive_id(value: Option<&Value>) -> Option<i64> {
    let id = py_int(value?).ok()?;
    i64::try_from(id).ok().filter(|id| *id > 0)
}

// ---------- the account's collections ----------

/// One collection, as the settings page offers it to tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionInfo {
    pub id: i64,
    pub title: String,
    pub owner: ZhihuCollectionOwner,
    /// How many posts it has; only the account's own collections say.
    pub item_count: Option<i64>,
    pub is_public: bool,
    /// Whose it is, for one the account follows.
    pub creator: String,
}

/// The signed-in account and its collections: its own first, then the ones it follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionLists {
    pub account: String,
    pub collections: Vec<CollectionInfo>,
}

/// Every entry of a paged list at `base` (which has no query yet).
async fn paged(api: &dyn ZhihuApiSession, base: &str) -> Result<Vec<Value>, ZhihuError> {
    let mut entries = Vec::new();
    for page_index in 0..MAX_LIST_PAGES {
        let page = api
            .get(&format!("{base}?offset={}&limit={PAGE_SIZE}", page_index * PAGE_SIZE))
            .await?;
        entries.extend(list_at(&page, "data").iter().cloned());
        if is_end(&page) {
            break;
        }
    }
    Ok(entries)
}

fn collection_info(entry: &Value, owner: ZhihuCollectionOwner) -> Option<CollectionInfo> {
    let entry = entry.as_dict()?;
    let id = positive_id(entry.get("id"))?;
    let item_count = entry
        .get("item_count")
        .and_then(|count| py_int(count).ok())
        .and_then(|count| i64::try_from(count).ok());
    let creator = dict_at(entry, "creator")
        .map(|creator| text_at(creator, "name"))
        .unwrap_or_default();
    Some(CollectionInfo {
        id,
        title: text_at(entry, "title"),
        owner,
        item_count,
        is_public: entry.get("is_public").is_some_and(Value::is_truthy),
        creator,
    })
}

/// The account's own collections and the ones it follows.
pub async fn list_collections(api: &dyn ZhihuApiSession) -> Result<CollectionLists, ZhihuError> {
    let me = api.get("/api/v4/me").await?;
    let token = text_at(&me, "url_token");
    if token.is_empty() {
        return Err(ZhihuError::Login(
            "Zhihu does not name the signed-in account; sign in to 知乎 on the 登录态 page.".to_owned(),
        ));
    }
    let name = text_at(&me, "name");
    let mut collections = Vec::new();
    let mut seen = HashSet::new();
    let lists = [
        (format!("/api/v4/people/{token}/collections"), ZhihuCollectionOwner::Mine),
        (
            format!("/api/v4/members/{token}/following-favlists"),
            ZhihuCollectionOwner::Followed,
        ),
    ];
    for (base, owner) in lists {
        for entry in paged(api, &base).await? {
            if let Some(info) = collection_info(&entry, owner)
                && seen.insert(info.id)
            {
                collections.push(info);
            }
        }
    }
    let account = if name.is_empty() { token } else { format!("{name} ({token})") };
    Ok(CollectionLists { account, collections })
}

/// [`list_collections`] in its own session, for the settings page.
pub async fn read_collections(opener: &dyn ZhihuApiOpener) -> Result<CollectionLists, ZhihuError> {
    let api = opener.open().await?;
    let lists = list_collections(api.as_ref()).await;
    api.close().await;
    lists
}

// ---------- one listed post ----------

/// What a listed post is read as.
#[derive(Debug, Clone, PartialEq)]
enum Media {
    /// An answer, column article or pin, parsed as a shared link's post is.
    Post(zhihu_post::ZhihuPost),
    /// A video post: its one video.
    Video(ZhihuVideo),
}

/// One post as a collection lists it.
#[derive(Debug, Clone, PartialEq)]
struct Listed {
    url: String,
    kind: String,
    /// The post's own id, in its file names.
    id: String,
    title: String,
    author: String,
    /// `None` for a kind this module does not read.
    media: Option<Media>,
}

impl Listed {
    fn has_media(&self) -> bool {
        match &self.media {
            Some(Media::Post(post)) => !post.images.is_empty() || !post.videos.is_empty(),
            Some(Media::Video(_)) => true,
            None => false,
        }
    }
}

fn author_of(content: &Dict) -> String {
    dict_at(content, "author").map(|author| text_at(author, "name")).unwrap_or_default()
}

/// A zvideo's title, video and cover.
fn zvideo(content: &Dict) -> Result<Option<ZhihuVideo>, PyError> {
    let empty = Dict::new();
    let video = dict_at(content, "video").unwrap_or(&empty);
    let lens_id = [video.get("video_id"), content.get("video_id")]
        .into_iter()
        .flatten()
        .map(py_str)
        .find(|id| !id.is_empty() && id != "None")
        .unwrap_or_default();
    let fallback_url = zhihu_post::best_playlist_url(video.get("playlist"))?;
    if lens_id.is_empty() && fallback_url.is_empty() {
        return Ok(None);
    }
    let cover_url = [
        as_str(video.get("thumbnail")),
        as_str(content.get("thumbnail")),
        as_str(content.get("image_url")),
    ]
    .into_iter()
    .flatten()
    .find(|cover| cover.starts_with("https://") || cover.starts_with("http://"))
    .unwrap_or_default()
    .to_owned();
    Ok(Some(ZhihuVideo {
        id: lens_id,
        fallback_url,
        cover_url,
    }))
}

/// One entry of a collection's items, or `None` when it names no post at all.
fn listed(entry: &Value) -> Result<Option<Listed>, PyError> {
    let Some(content) = entry.as_dict().and_then(|entry| dict_at(entry, "content")) else {
        return Ok(None);
    };
    let kind = text_at(content, "type");
    let id = content.get("id").map(py_str).unwrap_or_default();
    if id.is_empty() {
        return Ok(None);
    }
    let link_kind = ZhihuKind::from_name(&kind);
    if let Some(link_kind) = link_kind {
        let link = ZhihuLink::new(link_kind, id.clone());
        let post = zhihu_post::parse_post(&link, content)?;
        return Ok(Some(Listed {
            url: link.url(),
            kind,
            id,
            title: post.title.clone(),
            author: post.author.clone(),
            media: Some(Media::Post(post)),
        }));
    }
    let author = author_of(content);
    if kind == "zvideo" {
        let title = text_at(content, "title");
        return Ok(Some(Listed {
            url: format!("https://www.zhihu.com/zvideo/{id}"),
            kind,
            title: if title.is_empty() { format!("Video {id}") } else { title },
            id,
            author,
            media: zvideo(content)?.map(Media::Video),
        }));
    }
    let url = text_at(content, "url");
    if url.is_empty() {
        return Ok(None);
    }
    let title = [
        text_at(content, "title"),
        dict_at(content, "question")
            .map(|question| text_at(question, "title"))
            .unwrap_or_default(),
    ]
    .into_iter()
    .find(|title| !title.is_empty())
    .unwrap_or_else(|| {
        let excerpt = py_strip(&strip_tags(&str_or_empty(content.get("excerpt_title")))).to_owned();
        zhihu_post::short(&excerpt, PIN_TITLE_BYTES)
    });
    Ok(Some(Listed {
        url,
        kind,
        id,
        title,
        author,
        media: None,
    }))
}

// ---------- the rows ----------

/// What became of a post, as `zhihu_collection_item.status` records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    Saved,
    NoMedia,
    AlreadySaved,
    Unsupported,
    Failed,
    GivenUp,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Saved => "saved",
            Self::NoMedia => "no_media",
            Self::AlreadySaved => "already_saved",
            Self::Unsupported => "unsupported",
            Self::Failed => "failed",
            Self::GivenUp => "given_up",
        }
    }
}

fn sql_error(error: sqlx::Error) -> JobError {
    crate::db_job_error(&fav_db::DbError::from(error))
}

/// What the table knows of one collection: every post seen, and how far its walk got.
#[derive(Debug, Default)]
struct Known {
    urls: HashSet<String>,
    complete: bool,
    resume_offset: usize,
}

async fn known(db: &Db, collection_id: i64) -> Result<Known, JobError> {
    let urls = sqlx::query_scalar::<_, String>("SELECT url FROM zhihu_collection_item WHERE collection_id = $1")
        .bind(collection_id)
        .fetch_all(db.pool())
        .await
        .map_err(sql_error)?;
    let walk = sqlx::query("SELECT complete, resume_offset FROM zhihu_collection WHERE id = $1")
        .bind(collection_id)
        .fetch_optional(db.pool())
        .await
        .map_err(sql_error)?;
    let mut known = Known {
        urls: urls.into_iter().collect(),
        ..Known::default()
    };
    if let Some(row) = walk {
        known.complete = row.try_get("complete").map_err(sql_error)?;
        let offset: i32 = row.try_get("resume_offset").map_err(sql_error)?;
        known.resume_offset = usize::try_from(offset).unwrap_or(0);
    }
    Ok(known)
}

async fn record_walk(db: &Db, collection: &ZhihuCollection, complete: bool, resume_offset: usize) -> Result<(), JobError> {
    sqlx::query(
        "INSERT INTO zhihu_collection (id, title, complete, resume_offset, walked_at) VALUES ($1, $2, $3, $4, CURRENT_TIMESTAMP)
        ON CONFLICT (id) DO UPDATE SET title = EXCLUDED.title, complete = EXCLUDED.complete,
            resume_offset = EXCLUDED.resume_offset, walked_at = EXCLUDED.walked_at",
    )
    .bind(collection.id)
    .bind(&collection.title)
    .bind(complete)
    .bind(i32::try_from(resume_offset).unwrap_or(i32::MAX))
    .execute(db.pool())
    .await
    .map_err(sql_error)?;
    Ok(())
}

/// The row's outcome. A failure adds a try; any other outcome keeps the count.
struct Outcome<'a> {
    status: Status,
    images: usize,
    videos: usize,
    local_path: &'a str,
    error: &'a str,
    /// The listing's entry, kept only while the post waits to be downloaded.
    content: &'a str,
}

impl Outcome<'_> {
    fn of(status: Status) -> Self {
        Outcome {
            status,
            images: 0,
            videos: 0,
            local_path: "",
            error: "",
            content: "",
        }
    }
}

/// Record a post's outcome; the number of tries it has had now.
async fn record_item(db: &Db, collection: &ZhihuCollection, item: &Listed, outcome: &Outcome<'_>) -> Result<i32, JobError> {
    let failed = matches!(outcome.status, Status::Failed | Status::GivenUp);
    let count = |count: usize| i32::try_from(count).unwrap_or(i32::MAX);
    let attempts: i32 = sqlx::query_scalar(
        "INSERT INTO zhihu_collection_item
            (collection_id, url, kind, title, author, collection_title, status, image_count, video_count, local_path, attempts, last_error, content)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, CASE WHEN $11 THEN 1 ELSE 0 END, $12, $13)
        ON CONFLICT (collection_id, url) DO UPDATE SET
            kind = EXCLUDED.kind, title = EXCLUDED.title, author = EXCLUDED.author,
            collection_title = EXCLUDED.collection_title, status = EXCLUDED.status,
            image_count = EXCLUDED.image_count, video_count = EXCLUDED.video_count, local_path = EXCLUDED.local_path,
            attempts = zhihu_collection_item.attempts + CASE WHEN $11 THEN 1 ELSE 0 END,
            last_error = EXCLUDED.last_error, updated_at = CURRENT_TIMESTAMP,
            content = CASE WHEN EXCLUDED.status IN ('pending', 'failed') THEN zhihu_collection_item.content ELSE '' END
        RETURNING attempts",
    )
    .bind(collection.id)
    .bind(&item.url)
    .bind(&item.kind)
    .bind(&item.title)
    .bind(&item.author)
    .bind(&collection.title)
    .bind(outcome.status.as_str())
    .bind(count(outcome.images))
    .bind(count(outcome.videos))
    .bind(outcome.local_path)
    .bind(failed)
    .bind(outcome.error)
    .bind(outcome.content)
    .fetch_one(db.pool())
    .await
    .map_err(sql_error)?;
    Ok(attempts)
}

async fn set_status(db: &Db, collection_id: i64, url: &str, status: Status) -> Result<(), JobError> {
    sqlx::query(
        "UPDATE zhihu_collection_item SET status = $3, content = '', updated_at = CURRENT_TIMESTAMP WHERE collection_id = $1 AND url = $2",
    )
    .bind(collection_id)
    .bind(url)
    .bind(status.as_str())
    .execute(db.pool())
    .await
    .map_err(sql_error)?;
    Ok(())
}

// ---------- the crawl ----------

/// A post waiting to be downloaded: its collection's place in the settings, and the
/// listing's entry as the walk stored it.
#[derive(Debug, Clone)]
struct Queued {
    collection: usize,
    url: String,
    content: String,
}

/// Why a collection's walk ended early.
#[derive(Debug)]
enum Walk {
    /// Zhihu does not show this collection (deleted, private); the others go on.
    Gone(JobError),
    /// Nothing more can be read this run: the login, an outage, the database.
    Stop(JobError),
}

impl Walk {
    fn of(error: ZhihuError) -> Self {
        match error {
            ZhihuError::Zhihu(_) => Self::Gone(zhihu_job_error(error)),
            other => Self::Stop(zhihu_job_error(other)),
        }
    }
}

/// What one collection got in this run.
#[derive(Debug, Default)]
struct Tally {
    saved: Vec<String>,
    images: usize,
    videos: usize,
    first_image: Option<PathBuf>,
}

/// The source.
pub struct ZhihuCollections {
    db: Db,
    notifier: SharedNotifier,
    opener: Arc<dyn ZhihuApiOpener>,
    client: PostClient,
    config: fav_core::settings::Zhihu,
    sleeper: Sleeper,
}

impl std::fmt::Debug for ZhihuCollections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZhihuCollections")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn login_error(message: impl Into<String>) -> JobError {
    JobError::new("ZhihuLoginError", message).with_notification_dedupe_key(LOGIN_DEDUPE_KEY)
}

fn zhihu_job_error(error: ZhihuError) -> JobError {
    match error {
        ZhihuError::Login(message) => login_error(message),
        ZhihuError::Zhihu(message) => JobError::new("ZhihuError", message),
        ZhihuError::Outage(message) => JobError::new("ZhihuOutage", message),
        ZhihuError::Other(error) => error.into_job_error(),
    }
}

fn io_error(error: &std::io::Error, path: &Path) -> JobError {
    JobError::new("OSError", format!("{error}: {}", path.display()))
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl ZhihuCollections {
    pub fn new(
        db: Db,
        notifier: SharedNotifier,
        opener: Arc<dyn ZhihuApiOpener>,
        client: PostClient,
        config: fav_core::settings::Zhihu,
    ) -> Self {
        Self {
            db,
            notifier,
            opener,
            client,
            config,
            sleeper: tokio_sleeper(),
        }
    }

    #[must_use]
    pub fn with_sleeper(mut self, sleeper: Sleeper) -> Self {
        self.sleeper = sleeper;
        self
    }

    fn budget(&self) -> usize {
        usize::try_from(self.config.max_posts_per_run).unwrap_or(usize::MAX)
    }

    /// Walk the ticked collections, recording every new post. A collection Zhihu no longer
    /// shows does not stop the others; its error comes back with the first error that
    /// stopped the walk (the login, an outage), if one did.
    async fn walk_all(&self, api: &dyn ZhihuApiSession) -> (Option<JobError>, Option<JobError>) {
        let mut gone = None;
        for collection in &self.config.collections {
            match self.walk(api, collection).await {
                Ok(()) => {}
                Err(Walk::Gone(error)) => {
                    log().warning(format!(
                        "Zhihu collection {} ({}) could not be read: {}",
                        collection.id,
                        collection.title,
                        error.message()
                    ));
                    gone.get_or_insert(error);
                }
                Err(Walk::Stop(error)) => return (gone, Some(error)),
            }
        }
        (gone, None)
    }

    /// Read one collection from the top, recording the posts the table does not know yet.
    /// A collection walked to its end before stops at the first page with nothing new; one
    /// that was not jumps from there to where its last walk stopped, and reads on to its
    /// end or [`MAX_PAGES_PER_COLLECTION`] pages.
    async fn walk(&self, api: &dyn ZhihuApiSession, collection: &ZhihuCollection) -> Result<(), Walk> {
        let known = known(&self.db, collection.id).await.map_err(Walk::Stop)?;
        let mut seen = HashSet::new();
        let mut complete = known.complete;
        let mut resume_offset = known.resume_offset;
        let mut offset = 0;
        let mut jumped = false;
        let mut pages = 0;
        let mut new_posts = 0;
        loop {
            if pages == MAX_PAGES_PER_COLLECTION {
                // Everything above `offset` has been read; a later walk goes on from there.
                complete = false;
                resume_offset = offset;
                break;
            }
            if pages > 0 {
                (self.sleeper)(PAGE_DELAY_SECONDS).await;
            }
            let path = format!("/api/v4/collections/{}/items?offset={offset}&limit={PAGE_SIZE}", collection.id);
            let page = api.get(&path).await.map_err(Walk::of)?;
            pages += 1;
            let entries = list_at(&page, "data");
            let mut fresh = 0;
            for entry in entries {
                if self.take(collection, entry, &known.urls, &mut seen).await.map_err(Walk::Stop)? {
                    fresh += 1;
                }
            }
            new_posts += fresh;
            offset += PAGE_SIZE;
            if is_end(&page) || entries.is_empty() {
                complete = true;
                resume_offset = 0;
                break;
            }
            if fresh == 0 && !jumped {
                if known.complete {
                    break;
                }
                jumped = true;
                offset = offset.max(resume_offset);
            }
        }
        record_walk(&self.db, collection, complete, resume_offset)
            .await
            .map_err(Walk::Stop)?;
        log().info(format!(
            "Walked {pages} pages of Zhihu collection {} ({}): {new_posts} new posts{}",
            collection.id,
            collection.title,
            if complete {
                String::new()
            } else {
                format!(", read down to {resume_offset} so far")
            }
        ));
        Ok(())
    }

    /// Record a listed post the table does not know yet: `pending` with its entry when it
    /// has media to download, settled at once otherwise. Whether it was new.
    async fn take(
        &self,
        collection: &ZhihuCollection,
        entry: &Value,
        known: &HashSet<String>,
        seen: &mut HashSet<String>,
    ) -> Result<bool, JobError> {
        let item = match listed(entry) {
            Ok(Some(item)) => item,
            Ok(None) => return Ok(false),
            Err(error) => {
                // Recorded, so the walk does not take its page for a new one every run.
                let content = entry.as_dict().and_then(|entry| dict_at(entry, "content"));
                let url = content.map(|content| text_at(content, "url")).unwrap_or_default();
                log().warning(format!(
                    "A post in Zhihu collection {} could not be read: {}: {} ({url})",
                    collection.id, error.class, error.message
                ));
                if url.is_empty() || known.contains(&url) || !seen.insert(url.clone()) {
                    return Ok(false);
                }
                let item = Listed {
                    url,
                    kind: content.map(|content| text_at(content, "type")).unwrap_or_default(),
                    id: String::new(),
                    title: content.map(|content| text_at(content, "title")).unwrap_or_default(),
                    author: content.map(author_of).unwrap_or_default(),
                    media: None,
                };
                let message = format!("{}: {}", error.class, error.message);
                let outcome = Outcome {
                    error: &message,
                    ..Outcome::of(Status::Unsupported)
                };
                record_item(&self.db, collection, &item, &outcome).await?;
                return Ok(true);
            }
        };
        if known.contains(&item.url) || !seen.insert(item.url.clone()) {
            return Ok(false);
        }
        if item.media.is_none() {
            record_item(&self.db, collection, &item, &Outcome::of(Status::Unsupported)).await?;
            log().info(format!(
                "Zhihu collection {}: {} is a {} post, which is not read",
                collection.id, item.url, item.kind
            ));
        } else if !item.has_media() {
            record_item(&self.db, collection, &item, &Outcome::of(Status::NoMedia)).await?;
        } else {
            let content = fav_core::pyjson::dumps(entry, &fav_core::pyjson::DumpOptions::python_default())
                .map_err(|error| JobError::new("TypeError", error.to_string()))?;
            let outcome = Outcome {
                content: &content,
                ..Outcome::of(Status::Pending)
            };
            record_item(&self.db, collection, &item, &outcome).await?;
        }
        Ok(true)
    }

    /// The posts to download this run: pending, or failed with tries left, in the ticked
    /// collections, oldest first, at most `max_posts_per_run`.
    async fn queued(&self) -> Result<Vec<Queued>, JobError> {
        let ids: Vec<i64> = self.config.collections.iter().map(|collection| collection.id).collect();
        let rows = sqlx::query(
            "SELECT collection_id, url, content FROM zhihu_collection_item
            WHERE collection_id = ANY($1) AND (status = 'pending' OR (status = 'failed' AND attempts < $2))
            ORDER BY created_at, collection_id, url LIMIT $3",
        )
        .bind(&ids)
        .bind(MAX_ATTEMPTS)
        .bind(i64::try_from(self.budget()).unwrap_or(i64::MAX))
        .fetch_all(self.db.pool())
        .await
        .map_err(sql_error)?;
        let mut queued = Vec::with_capacity(rows.len());
        for row in rows {
            let collection_id: i64 = row.try_get("collection_id").map_err(sql_error)?;
            let Some(collection) = self.config.collections.iter().position(|collection| collection.id == collection_id) else {
                continue;
            };
            queued.push(Queued {
                collection,
                url: row.try_get("url").map_err(sql_error)?,
                content: row.try_get("content").map_err(sql_error)?,
            });
        }
        Ok(queued)
    }

    /// Download one queued post, or settle it when there is nothing left to download.
    async fn download(&self, queued: &Queued, tally: &mut Tally) -> Result<(), JobError> {
        let collection = &self.config.collections[queued.collection];
        let item = fav_core::pyjson::loads(&queued.content)
            .map_err(|error| PyError::new("JSONDecodeError", error.to_string()))
            .and_then(|entry| listed(&entry));
        let item = match item {
            Ok(Some(item)) if item.url == queued.url && item.has_media() => item,
            other => {
                let message = match other {
                    Err(error) => format!("{}: {}", error.class, error.message),
                    _ => "the stored listing no longer names a post with media".to_owned(),
                };
                let item = Listed {
                    url: queued.url.clone(),
                    kind: String::new(),
                    id: String::new(),
                    title: String::new(),
                    author: String::new(),
                    media: None,
                };
                return self.failed(collection, &item, &message).await;
            }
        };
        // Saved since the walk: by another collection in this run, or through a shared link.
        if let Some(saved) = saved_links::find(&self.db, &item.url).await.map_err(sql_error)? {
            let outcome = Outcome {
                local_path: &saved.saved_path,
                ..Outcome::of(Status::AlreadySaved)
            };
            record_item(&self.db, collection, &item, &outcome).await?;
            log().info(format!(
                "Zhihu collection {}: {} {}",
                collection.id,
                item.url,
                saved_links::reason(&saved)
            ));
            return Ok(());
        }
        self.save(collection, &item, tally).await
    }

    /// The post's pictures and videos, its videos resolved to URLs to download now.
    async fn page(&self, item: &Listed) -> Result<ArticlePage, PyError> {
        let (post_videos, page) = match &item.media {
            Some(Media::Post(post)) => (
                post.videos.clone(),
                ArticlePage {
                    title: post.title.clone(),
                    account: post.author.clone(),
                    published_at: post.published_at,
                    images: post.images.clone(),
                    picture_count: 0,
                    videos: Vec::new(),
                },
            ),
            Some(Media::Video(video)) => (
                vec![video.clone()],
                ArticlePage {
                    title: item.title.clone(),
                    account: item.author.clone(),
                    published_at: None,
                    images: Vec::new(),
                    picture_count: 0,
                    videos: Vec::new(),
                },
            ),
            None => return Ok(ArticlePage::default()),
        };
        let mut videos = Vec::new();
        for video in &post_videos {
            let url = if video.id.is_empty() {
                video.fallback_url.clone()
            } else {
                zhihu_post::resolve_video(&self.client, video).await?
            };
            if url.is_empty() {
                log().info(format!("Zhihu video {} of {} has no play URL", video.id, item.url));
            } else {
                videos.push(PostVideo::new(url, video.cover_url.clone()));
            }
        }
        Ok(ArticlePage { videos, ..page })
    }

    /// Download a post's pictures and videos into `folder`; a file an earlier try saved
    /// is kept. The files there now.
    async fn save_media(&self, item: &Listed, page: &ArticlePage, folder: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>), JobError> {
        // The number goes with the id, which a long title cannot push out of the name.
        let name = |suffix: &str, ext: &str| {
            let id = if suffix.is_empty() {
                item.id.clone()
            } else {
                format!("{}_{suffix}", item.id)
            };
            let author = Some(page.account.as_str()).filter(|author| !author.is_empty());
            format_media_filename(&page.title, &id, author, ext, 240)
        };
        let mut images = Vec::new();
        for (index, url) in page.images.iter().enumerate() {
            let suffix = if page.images.len() > 1 {
                (index + 1).to_string()
            } else {
                String::new()
            };
            if let Some(existing) = ["jpg", "png", "gif", "webp"]
                .iter()
                .map(|ext| folder.join(name(&suffix, ext)))
                .find(|path| path.exists())
            {
                images.push(existing);
                continue;
            }
            let Some((content, ext)) = self.client.fetch_image(url).await.map_err(PyError::into_job_error)? else {
                continue;
            };
            let path = folder.join(name(&suffix, ext));
            tokio::fs::create_dir_all(folder).await.map_err(|error| io_error(&error, folder))?;
            tokio::fs::write(&path, &content).await.map_err(|error| io_error(&error, &path))?;
            images.push(path);
        }
        let mut videos = Vec::new();
        for (index, video) in page.videos.iter().enumerate() {
            let suffix = if page.videos.len() > 1 {
                format!("video-{}", index + 1)
            } else {
                String::new()
            };
            let path = folder.join(name(&suffix, "mp4"));
            if path.exists() {
                videos.push(path);
                continue;
            }
            let partial = path.with_extension("mp4.part");
            if self.client.fetch_video(video, &partial).await.map_err(PyError::into_job_error)? {
                tokio::fs::rename(&partial, &path).await.map_err(|error| io_error(&error, &path))?;
                videos.push(path);
            }
        }
        Ok((images, videos))
    }

    /// Download one post and record what became of it.
    async fn save(&self, collection: &ZhihuCollection, item: &Listed, tally: &mut Tally) -> Result<(), JobError> {
        let page = match self.page(item).await {
            Ok(page) => page,
            Err(error) => return self.failed(collection, item, &format!("{}: {}", error.class, error.message)).await,
        };
        let (images, videos) = match self.save_media(item, &page, collection.path.as_path()).await {
            Ok(files) => files,
            Err(error) => {
                return self
                    .failed(collection, item, &format!("{}: {}", error.class_name(), error.message()))
                    .await;
            }
        };
        if images.is_empty() && videos.is_empty() {
            let message = format!(
                "none of the {} images and {} videos of {} could be downloaded",
                page.images.len(),
                page.videos.len(),
                item.url
            );
            return self.failed(collection, item, &message).await;
        }
        let first = images.iter().chain(&videos).next().map(|path| path_text(path)).unwrap_or_default();
        let outcome = Outcome {
            images: images.len(),
            videos: videos.len(),
            local_path: &first,
            ..Outcome::of(Status::Saved)
        };
        record_item(&self.db, collection, item, &outcome).await?;
        let saved = NewSavedLink {
            url: &item.url,
            source: JOB_KEY,
            item: &format!("收藏夹 {}", collection.title),
            title: &item.title,
            saved_path: &first,
        };
        saved_links::record(&self.db, saved).await.map_err(sql_error)?;
        log().info(format!(
            "Saved Zhihu post {} from collection {} ({}/{} images, {}/{} videos)",
            item.url,
            collection.id,
            images.len(),
            page.images.len(),
            videos.len(),
            page.videos.len()
        ));
        tally.saved.push(item.title.clone());
        tally.images += images.len();
        tally.videos += videos.len();
        if tally.first_image.is_none() {
            tally.first_image = images.into_iter().next();
        }
        Ok(())
    }

    /// A post that could not be saved: tried again next run, given up and reported after
    /// [`MAX_ATTEMPTS`] tries.
    async fn failed(&self, collection: &ZhihuCollection, item: &Listed, message: &str) -> Result<(), JobError> {
        let outcome = Outcome {
            error: message,
            ..Outcome::of(Status::Failed)
        };
        let attempts = record_item(&self.db, collection, item, &outcome).await?;
        log().warning(format!(
            "Zhihu post {} from collection {} failed (try {attempts} of {MAX_ATTEMPTS}): {message}",
            item.url, collection.id
        ));
        if attempts >= MAX_ATTEMPTS {
            set_status(&self.db, collection.id, &item.url, Status::GivenUp).await?;
            let notification = NewNotification {
                kind: "download_failed".to_owned(),
                source: JOB_KEY.to_owned(),
                header: HEADER.to_owned(),
                title: item.title.clone(),
                link_url: item.url.clone(),
                body: format!("Given up after {attempts} tries (收藏夹 {}): {message}", collection.title),
                ..NewNotification::default()
            };
            if let Err(error) = self.notifier.enqueue(notification).await {
                log().warning(format!("Failed to enqueue the Zhihu download failure notification: {error}"));
            }
        }
        Ok(())
    }

    async fn notify(&self, collection: &ZhihuCollection, tally: &Tally) {
        if tally.saved.is_empty() {
            return;
        }
        let mut titles: Vec<String> = tally
            .saved
            .iter()
            .take(TITLES_IN_MESSAGE)
            .map(|title| format!("· {title}"))
            .collect();
        if tally.saved.len() > TITLES_IN_MESSAGE {
            titles.push(format!("… and {} more", tally.saved.len() - TITLES_IN_MESSAGE));
        }
        let mut payload = Dict::new();
        payload.insert("collection_id", Value::from(collection.id));
        payload.insert("posts", Value::from(i64::try_from(tally.saved.len()).unwrap_or(i64::MAX)));
        if let Some(first) = &tally.first_image {
            payload.insert("image_path", Value::Str(path_text(first)));
        }
        let notification = NewNotification {
            kind: "download_completed".to_owned(),
            source: JOB_KEY.to_owned(),
            header: HEADER.to_owned(),
            title: format!("收藏夹「{}」", collection.title),
            link_url: format!("https://www.zhihu.com/collection/{}", collection.id),
            body: format!(
                "Saved {} posts | {} images, {} videos\n{}",
                tally.saved.len(),
                tally.images,
                tally.videos,
                titles.join("\n")
            ),
            payload: Some(payload),
            ..NewNotification::default()
        };
        if let Err(error) = self.notifier.enqueue(notification).await {
            log().warning(format!("Failed to enqueue the Zhihu collection notification: {error}"));
        }
    }

    /// One run.
    pub async fn update(&mut self) -> Result<(), JobError> {
        if self.config.collections.is_empty() {
            return Ok(());
        }
        let stored = site_sessions::load(&self.db, fav_browser::zhihu::SESSION_SITE)
            .await
            .map_err(|error| match error {
                fav_runtime::Error::Db(error) => crate::db_job_error(&error),
                other => JobError::new("RuntimeError", other.to_string()),
            })?;
        if stored.is_none_or(|stored| stored.cookie(fav_browser::zhihu::SESSION_COOKIE).is_empty()) {
            return Err(login_error(
                "Zhihu collections need the Zhihu login, and 知乎 is not signed in. Sign in to 知乎 on the 登录态 page.",
            ));
        }
        let api = self.opener.open().await.map_err(zhihu_job_error)?;
        let (gone, walk_error) = self.walk_all(api.as_ref()).await;
        api.close().await;
        if let Some(error) = &walk_error
            && error.notification_dedupe_key() == Some(LOGIN_DEDUPE_KEY)
            && let Err(store) = site_sessions::record_check(&self.db, fav_browser::zhihu::SESSION_SITE, false, error.message()).await
        {
            log().warning(format!("Could not record the Zhihu login check: {store}"));
        }

        // Outside the lock, with the browser gone: plain CDN traffic. What the walk found
        // is in the table already, so a walk cut short still leaves its posts to download.
        let queued = self.queued().await?;
        let mut tallies: Vec<Tally> = self.config.collections.iter().map(|_| Tally::default()).collect();
        let mut stopped = None;
        for entry in &queued {
            if let Err(error) = self.download(entry, &mut tallies[entry.collection]).await {
                stopped = Some(error);
                break;
            }
        }
        for (collection, tally) in self.config.collections.iter().zip(&tallies) {
            self.notify(collection, tally).await;
        }
        let saved: usize = tallies.iter().map(|tally| tally.saved.len()).sum();
        log().info(format!("Zhihu collections: saved {saved} of {} queued posts", queued.len()));
        match stopped.or(walk_error).or(gone) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

// ---------- the job ----------

struct ZhihuJob {
    ctx: SourceContext,
}

impl Source for ZhihuJob {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self.ctx.settings.load().await.map_err(|error| crate::settings_job_error(&error))?;
            let config = settings.web.zhihu.clone();
            let notifier: SharedNotifier = Arc::new(OutboxNotifier(Outbox::new(self.ctx.db.clone(), Arc::clone(&self.ctx.settings))));
            let opener: Arc<dyn ZhihuApiOpener> = Arc::new(ChromiumZhihuApi::new(self.ctx.db.clone(), Arc::clone(&self.ctx.settings)));
            let client = PostClient::new().map_err(|error| JobError::new("RuntimeError", error.to_string()))?;
            let mut source = ZhihuCollections::new(self.ctx.db.clone(), notifier, opener, client, config);
            source.update().await
        })
    }
}

/// Register the `zhihu` job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run(JOB_KEY, move || Box::new(ZhihuJob { ctx: ctx.clone() }) as Box<dyn Source>);
}

#[cfg(test)]
mod tests;
