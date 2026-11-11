//! Telegram: every configured account's channels, saved through a durable queue.
//!
//! Python origin: `src/web/telegram.py`. The source stays resident in the worker: per
//! account it keeps a connection that receives new channel messages (`NewMessage` and
//! `Album` events, queued at high priority) and a serial worker that downloads the queued
//! media one job at a time. A scheduled or triggered run reconciles each channel's history
//! into the queue; without the resident runtime (a trigger from the API while the worker
//! is not running it) a run connects once, reconciles and drains the queue.
//!
//! The `MTProto` client is behind [`client::Connector`]; [`live::GrammersConnector`] is the
//! real one, over grammers with Telethon's `.session` files ([`session`]). Telethon's own
//! paging (`iter_messages`), album assembly and download naming are reproduced in
//! [`history`], [`albums`] and [`files`].

pub mod albums;
pub mod client;
pub mod files;
pub mod history;
pub mod live;
pub mod message;
mod mimetypes_defaults;
mod rpc_errors;
pub mod session;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use fav_core::filename::{format_media_filename, sanitize};
use fav_core::logging::{self, Logger};
use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::py_strip;
use fav_core::settings::{Telegram as TelegramConfig, TelegramAccount, TelegramChannel, TelegramMediaType};
use fav_core::validate::LiteralStr as _;
use fav_db::{Db, SettingsStore};
use fav_runtime::notifications::{KIND_UNSUPPORTED_MESSAGE, KIND_UNSUPPORTED_RESOLVED, NewNotification, Outbox, Resolution};
use fav_runtime::source::{JobError, Registry, ResidentSource};
use fav_runtime::{BoxFuture, StopHandle, StopSignal, stop_signal};
use sqlx::Row as _;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use self::albums::{Albums, Event};
use self::client::{Channel, ClientConfig, Connection, Connector, ErrorKind, TgClient, TgError};
use self::files::{Extensions, Location};
use self::history::Scan;
use self::message::{TgMessage, message_link, message_media_type, unsupported_reason};
use crate::SourceContext;
use crate::notifier::{OutboxNotifier, SharedNotifier};
use crate::post_links::{self, Needs, PostClient, PostError, PostLinkError, PostLinks, SessionStore};
use crate::telegram_queue::{self as queue, JobType, QueueSource, TelegramMediaJob, sql_error};
use crate::wechat_article::{ArticleLink, ArticlePage, canonical_url};

/// The job key, and the `source` of its notifications.
pub const JOB_KEY: &str = "telegram";
const HEADER: &str = "Telegram";
const SESSION_SUFFIX: &str = ".session";
/// How long an album's parts wait in the queue before the worker may take them.
pub const ALBUM_SETTLE_SECONDS: f64 = 2.0;
const QUEUE_IDLE_POLL: Duration = Duration::from_secs(1);
const RECONNECT_INITIAL_SECONDS: f64 = 5.0;
const RECONNECT_MAX_SECONDS: f64 = 300.0;
/// A link whose post cannot be read is given up after this many tries and reported.
pub const LINK_MAX_ATTEMPTS: i64 = 5;
/// A link that waits on a login or a key is tried again this often, without giving up.
pub const LINK_WAIT_SECONDS: f64 = 1800.0;
/// What `post_links.fetch_image` names a picture's extension.
const LINK_IMAGE_EXTENSIONS: [&str; 5] = ["jpg", "png", "gif", "webp", "bmp"];

fn log() -> Logger {
    logging::get("telegram")
}

/// A channel's routes: media type to its channel entry, in first-seen order.
pub type Routes = Vec<(TelegramMediaType, TelegramChannel)>;

/// A message picked by a scan, with the title it is saved under (`TelegramMediaEntry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub msg: TgMessage,
    pub filename: String,
    pub media_type: JobType,
}

/// `TelegramChannelScan`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelScan {
    pub media: Vec<Entry>,
    pub max_message_id: i64,
    pub scanned_message_count: usize,
}

/// `TelegramChannelState`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelState {
    pub last_scanned_message_id: i64,
    pub has_scan_state: bool,
    pub cooldown_remaining_seconds: f64,
}

/// Python's `str.splitlines()[0]`: the text up to the first line boundary.
fn first_line(text: &str) -> &str {
    const BREAKS: [char; 10] = [
        '\n', '\r', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}', '\u{2029}',
    ];
    text.split(BREAKS).next().unwrap_or("")
}

/// `PurePath.suffix`: the last dot of the name, unless it leads or ends it.
fn py_suffix(path: &Path) -> String {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    match name.rfind('.') {
        Some(dot) if dot > 0 && dot < name.len() - 1 => name[dot..].to_owned(),
        _ => String::new(),
    }
}

/// `Path(text).expanduser().resolve()` (not strict): absolute, symlinks of the part that
/// exists resolved, `.` and `..` folded.
fn py_resolve(text: &str) -> PathBuf {
    let expanded = match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            std::env::var("HOME").map_or_else(|_| PathBuf::from(text), |home| PathBuf::from(format!("{home}{rest}")))
        }
        _ => PathBuf::from(text),
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default().join(expanded)
    };
    let mut resolved = PathBuf::from("/");
    let mut exists = true;
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(part) => {
                resolved.push(part);
                if exists {
                    match std::fs::canonicalize(&resolved) {
                        Ok(real) => resolved = real,
                        Err(_) => exists = false,
                    }
                }
            }
        }
    }
    resolved
}

/// `_effective_session_file`: the file Telethon opens for a session path.
pub fn effective_session_file(session_path: &str) -> PathBuf {
    if session_path.ends_with(SESSION_SUFFIX) {
        py_resolve(session_path)
    } else {
        py_resolve(&format!("{session_path}{SESSION_SUFFIX}"))
    }
}

/// `_account_lock_name`: one owner per session file, whatever the path's spelling.
pub fn account_lock_name(account: &TelegramAccount) -> String {
    format!(
        "telegram-session:{}",
        effective_session_file(account.session_path.as_str()).display()
    )
}

/// `TelegramSessionUnauthorizedError`.
fn unauthorized(account: &TelegramAccount) -> TgError {
    TgError::new(
        "TelegramSessionUnauthorizedError",
        format!(
            "Telegram session unauthorized: account={}, session_path={}",
            account.name,
            account.session_path.as_str()
        ),
        ErrorKind::Runtime,
    )
}

fn is_unauthorized(error: &TgError) -> bool {
    error.class == "TelegramSessionUnauthorizedError"
}

/// `_job_types`: what a channel's messages are scanned for: its media, links to posts,
/// and the rest where it is reported.
pub fn job_types(routes: &Routes) -> Vec<JobType> {
    let mut types: Vec<JobType> = routes.iter().map(|(media, _)| JobType::of_media(*media)).collect();
    types.push(JobType::Link);
    if routes.iter().any(|(_, route)| route.report_unsupported) {
        types.push(JobType::Unsupported);
    }
    types
}

/// `_history_types`: old messages with nothing to save go unreported.
pub fn history_types(types: &[JobType]) -> Vec<JobType> {
    types.iter().copied().filter(|kind| *kind != JobType::Unsupported).collect()
}

fn fallback_title(entry: &Entry) -> String {
    format!("{}_{}", entry.media_type.as_str(), entry.msg.id)
}

/// `_build_standalone_entry`: a message's own caption, the first line for a link (share
/// texts put the title first).
pub fn standalone_entry(entry: Entry) -> Entry {
    let caption = py_strip(&entry.msg.text);
    let caption = if entry.media_type == JobType::Link {
        py_strip(first_line(caption))
    } else {
        caption
    };
    let filename = if caption.is_empty() {
        fallback_title(&entry)
    } else {
        caption.to_owned()
    };
    Entry { filename, ..entry }
}

/// `_build_group_entries`: an album's parts named after its caption, numbered from 1.
fn group_entries(group: Vec<Entry>, caption: Option<&str>) -> Vec<Entry> {
    match caption {
        None | Some("") => group
            .into_iter()
            .map(|entry| Entry {
                filename: fallback_title(&entry),
                ..entry
            })
            .collect(),
        Some(caption) if group.len() == 1 => group
            .into_iter()
            .map(|entry| Entry {
                filename: caption.to_owned(),
                ..entry
            })
            .collect(),
        Some(caption) => group
            .into_iter()
            .enumerate()
            .map(|(index, entry)| Entry {
                filename: format!("{caption}-{}", index + 1),
                ..entry
            })
            .collect(),
    }
}

/// `_build_scan_from_messages`: the messages worth queuing, with their titles.
pub fn build_scan(messages: &[TgMessage], types: &[JobType]) -> Result<ChannelScan, TgError> {
    let mut media: Vec<Entry> = Vec::new();
    let mut captions: HashMap<i64, String> = HashMap::new();
    for message in messages {
        if let Some(media_type) = message_media_type(message, types).map_err(|error| TgError::py(&error))? {
            media.push(Entry {
                msg: message.clone(),
                filename: String::new(),
                media_type,
            });
        }
        let caption = py_strip(&message.text);
        if let Some(group) = message.album()
            && !caption.is_empty()
        {
            captions.entry(group).or_insert_with(|| caption.to_owned());
        }
    }
    let mut groups: HashMap<i64, Vec<Entry>> = HashMap::new();
    for entry in &media {
        if let Some(group) = entry.msg.album() {
            groups.entry(group).or_default().push(entry.clone());
        }
    }
    let mut result = Vec::new();
    let mut processed: HashSet<i64> = HashSet::new();
    for entry in media {
        let Some(group) = entry.msg.album() else {
            result.push(standalone_entry(entry));
            continue;
        };
        if !processed.insert(group) {
            continue;
        }
        result.extend(group_entries(
            groups.remove(&group).unwrap_or_default(),
            captions.get(&group).map(String::as_str),
        ));
    }
    Ok(ChannelScan {
        media: result,
        max_message_id: messages.iter().map(|message| message.id).max().unwrap_or(0).max(0),
        scanned_message_count: messages.len(),
    })
}

/// `_media_batches`: an album's entries together, in first-seen order.
fn media_batches(media: &[Entry]) -> Vec<Vec<&Entry>> {
    let mut batches: Vec<Vec<&Entry>> = Vec::new();
    let mut indexes: HashMap<i64, usize> = HashMap::new();
    for entry in media {
        if let Some(group) = entry.msg.album() {
            if let Some(index) = indexes.get(&group) {
                batches[*index].push(entry);
            } else {
                indexes.insert(group, batches.len());
                batches.push(vec![entry]);
            }
        } else {
            batches.push(vec![entry]);
        }
    }
    batches
}

fn unsupported_key(account_name: &str, channel_id: i64, message_id: i64) -> String {
    format!("telegram:unsupported:{account_name}:{channel_id}:{message_id}")
}

fn dict(pairs: Vec<(&str, Value)>) -> Dict {
    let mut dict = Dict::new();
    for (key, value) in pairs {
        dict.insert(key, value);
    }
    dict
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `[account]title[-suffix] [message id].ext`, as the channel's own media are named.
fn link_filename(page: &ArticlePage, message_id: i64, suffix: &str, ext: &str) -> String {
    let title = if suffix.is_empty() {
        page.title.clone()
    } else {
        format!("{}-{suffix}", page.title)
    };
    let uploader = Some(page.account.as_str()).filter(|account| !account.is_empty());
    format_media_filename(&title, &message_id.to_string(), uploader, ext, 240)
}

/// What a job's try ends in, besides the cases that finish it on the spot.
enum JobFailure {
    Link(PostLinkError),
    Tg(TgError),
}

impl From<TgError> for JobFailure {
    fn from(error: TgError) -> Self {
        Self::Tg(error)
    }
}

/// The settings' account by name, or `account` itself when it is gone from them.
fn current_account<'a>(config: &'a TelegramConfig, account: &'a TelegramAccount) -> &'a TelegramAccount {
    config
        .resolved_accounts()
        .iter()
        .find(|item| item.name == account.name)
        .unwrap_or(account)
}

fn owned_routes(account: &TelegramAccount) -> Vec<(i64, Routes)> {
    account
        .channel_routes()
        .into_iter()
        .map(|(id, routes)| (id, routes.into_iter().map(|(media, channel)| (media, channel.clone())).collect()))
        .collect()
}

fn route_for<'a>(routes: &'a Routes, media_type: &str) -> Option<&'a TelegramChannel> {
    routes
        .iter()
        .find(|(media, _)| (*media).as_str() == media_type)
        .map(|(_, channel)| channel)
}

/// `asyncio.sleep(seconds)`, injectable so tests can see the delay between downloads.
pub type Sleeper = Arc<dyn Fn(f64) -> BoxFuture<'static, ()> + Send + Sync>;

fn real_sleeper() -> Sleeper {
    Arc::new(|seconds: f64| {
        Box::pin(async move {
            if seconds > 0.0 && seconds.is_finite() {
                tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            }
        })
    })
}

/// What the source works with.
pub struct Deps {
    pub db: Db,
    pub settings: Arc<SettingsStore>,
    pub notifier: SharedNotifier,
    pub connector: Arc<dyn Connector>,
    /// The post-link client; built on first use when `None`.
    pub links: Option<PostLinks>,
    pub sleeper: Option<Sleeper>,
}

impl std::fmt::Debug for Deps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deps").finish_non_exhaustive()
    }
}

struct Inner {
    db: Db,
    settings: Arc<SettingsStore>,
    notifier: SharedNotifier,
    connector: Arc<dyn Connector>,
    links: OnceLock<PostLinks>,
    sleeper: Sleeper,
    cache: Mutex<Option<tempfile::TempDir>>,
    clients: Mutex<HashMap<String, Arc<dyn TgClient>>>,
    one_shot: Mutex<Option<Arc<dyn TgClient>>>,
    wakes: Mutex<HashMap<String, Arc<Notify>>>,
    ready: watch::Sender<HashSet<String>>,
    reconcile_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    close: StopHandle,
    closed_signal: StopSignal,
    running: AtomicBool,
    closed: AtomicBool,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The Telegram runtime (`Telegram`). Cheap to clone; clones share the runtime.
#[derive(Clone)]
pub struct Telegram {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Telegram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telegram").finish_non_exhaustive()
    }
}

impl Telegram {
    pub fn new(deps: Deps) -> Self {
        let (close, closed_signal) = stop_signal();
        let links = OnceLock::new();
        if let Some(prebuilt) = deps.links {
            let _ = links.set(prebuilt);
        }
        Self {
            inner: Arc::new(Inner {
                db: deps.db,
                settings: deps.settings,
                notifier: deps.notifier,
                connector: deps.connector,
                links,
                sleeper: deps.sleeper.unwrap_or_else(real_sleeper),
                cache: Mutex::new(None),
                clients: Mutex::new(HashMap::new()),
                one_shot: Mutex::new(None),
                wakes: Mutex::new(HashMap::new()),
                ready: watch::Sender::new(HashSet::new()),
                reconcile_locks: Mutex::new(HashMap::new()),
                close,
                closed_signal,
                running: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            }),
        }
    }

    fn db(&self) -> &Db {
        &self.inner.db
    }

    /// `self.cfg`: read per access, so edits in the UI apply without a restart.
    async fn cfg(&self) -> Result<TelegramConfig, TgError> {
        let settings = self
            .inner
            .settings
            .load()
            .await
            .map_err(|error| TgError::runtime(error.to_string()))?;
        Ok(settings.web.telegram.clone())
    }

    /// `_channel_routes`: the account's channels as the settings have them now, looked up
    /// by name; the login itself is only read when the listener connects.
    async fn channel_routes(&self, account: &TelegramAccount) -> Result<Vec<(i64, Routes)>, TgError> {
        let config = self.cfg().await?;
        Ok(owned_routes(current_account(&config, account)))
    }

    async fn routes_of(&self, account: &TelegramAccount, channel_id: i64) -> Result<Option<Routes>, TgError> {
        Ok(self
            .channel_routes(account)
            .await?
            .into_iter()
            .find(|(id, _)| *id == channel_id)
            .map(|(_, routes)| routes))
    }

    fn links(&self) -> Result<&PostLinks, TgError> {
        if let Some(links) = self.inner.links.get() {
            return Ok(links);
        }
        let client = PostClient::new().map_err(|error| TgError::runtime(error.to_string()))?;
        let sessions: Arc<dyn SessionStore> = Arc::new(self.inner.db.clone());
        let _ = self
            .inner
            .links
            .set(PostLinks::new(client, Arc::clone(&self.inner.settings), sessions));
        self.inner
            .links
            .get()
            .ok_or_else(|| TgError::runtime("post-link client unavailable"))
    }

    /// The runtime's temporary download folder (`fav-telegram-*`).
    fn cache_dir(&self) -> Result<PathBuf, TgError> {
        let mut cache = locked(&self.inner.cache);
        if let Some(dir) = cache.as_ref() {
            return Ok(dir.path().to_path_buf());
        }
        let dir = tempfile::Builder::new()
            .prefix("fav-telegram-")
            .tempdir()
            .map_err(|error| TgError::io(&error))?;
        let path = dir.path().to_path_buf();
        *cache = Some(dir);
        Ok(path)
    }

    fn wake(&self, account_name: &str) -> Arc<Notify> {
        Arc::clone(locked(&self.inner.wakes).entry(account_name.to_owned()).or_default())
    }

    fn wake_if_waiting(&self, account_name: &str) {
        if let Some(wake) = locked(&self.inner.wakes).get(account_name) {
            wake.notify_one();
        }
    }

    // ---------- the archive and the channel state ----------

    /// `get_downloaded_ids`.
    pub async fn downloaded_ids(&self, account_name: &str, channel_id: i64, min_message_id: i64) -> Result<HashSet<i64>, TgError> {
        let rows = sqlx::query("SELECT message_id FROM telegram WHERE account_name = $1 AND channel_id = $2 AND message_id > $3")
            .bind(account_name)
            .bind(channel_id)
            .bind(min_message_id)
            .fetch_all(self.db().pool())
            .await
            .map_err(db_error)?;
        rows.iter()
            .map(|row| row.try_get::<i64, _>("message_id").map_err(db_error))
            .collect()
    }

    /// `is_downloaded`.
    pub async fn is_downloaded(&self, account_name: &str, channel_id: i64, message_id: i64) -> Result<bool, TgError> {
        let row = sqlx::query("SELECT 1 FROM telegram WHERE account_name = $1 AND channel_id = $2 AND message_id = $3 LIMIT 1")
            .bind(account_name)
            .bind(channel_id)
            .bind(message_id)
            .fetch_optional(self.db().pool())
            .await
            .map_err(db_error)?;
        Ok(row.is_some())
    }

    /// `get_latest_downloaded_id`.
    async fn latest_downloaded_id(&self, account_name: &str, channel_id: i64) -> Result<i64, TgError> {
        let row =
            sqlx::query("SELECT COALESCE(MAX(message_id), 0) AS message_id FROM telegram WHERE account_name = $1 AND channel_id = $2")
                .bind(account_name)
                .bind(channel_id)
                .fetch_one(self.db().pool())
                .await
                .map_err(db_error)?;
        row.try_get::<i64, _>("message_id").map_err(db_error)
    }

    /// `get_channel_state`.
    pub async fn channel_state(&self, account_name: &str, channel_id: i64) -> Result<ChannelState, TgError> {
        let row = sqlx::query(
            "SELECT
                last_scanned_message_id,
                CASE
                    WHEN cooldown_until IS NULL OR cooldown_until <= CURRENT_TIMESTAMP THEN 0
                    ELSE EXTRACT(EPOCH FROM (cooldown_until - CURRENT_TIMESTAMP))
                END::DOUBLE PRECISION AS cooldown_remaining_seconds
            FROM telegram_channel_state
            WHERE account_name = $1 AND channel_id = $2",
        )
        .bind(account_name)
        .bind(channel_id)
        .fetch_optional(self.db().pool())
        .await
        .map_err(db_error)?;
        let Some(row) = row else {
            return Ok(ChannelState {
                last_scanned_message_id: self.latest_downloaded_id(account_name, channel_id).await?,
                has_scan_state: false,
                cooldown_remaining_seconds: 0.0,
            });
        };
        let mut last_scanned: i64 = row.try_get("last_scanned_message_id").map_err(db_error)?;
        if last_scanned <= 0 {
            last_scanned = self.latest_downloaded_id(account_name, channel_id).await?;
        }
        let cooldown: Option<f64> = row.try_get("cooldown_remaining_seconds").map_err(db_error)?;
        Ok(ChannelState {
            last_scanned_message_id: last_scanned,
            has_scan_state: true,
            cooldown_remaining_seconds: cooldown.unwrap_or(0.0),
        })
    }

    /// `set_channel_last_scanned_message_id`: the cursor only moves forward.
    pub async fn set_last_scanned(&self, account_name: &str, channel_id: i64, message_id: i64) -> Result<(), TgError> {
        if message_id <= 0 {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO telegram_channel_state (account_name, channel_id, last_scanned_message_id)
            VALUES ($1, $2, $3)
            ON CONFLICT (account_name, channel_id)
            DO UPDATE SET
                last_scanned_message_id = GREATEST(
                    telegram_channel_state.last_scanned_message_id,
                    EXCLUDED.last_scanned_message_id
                ),
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(account_name)
        .bind(channel_id)
        .bind(message_id)
        .execute(self.db().pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    /// `set_channel_cooldown`: a cooldown only ever extends.
    pub async fn set_channel_cooldown(&self, account_name: &str, channel_id: i64, seconds: f64, error: &str) -> Result<(), TgError> {
        if seconds <= 0.0 {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO telegram_channel_state (account_name, channel_id, cooldown_until, last_error)
            VALUES ($1, $2, CURRENT_TIMESTAMP + ($3 * INTERVAL '1 second'), $4)
            ON CONFLICT (account_name, channel_id)
            DO UPDATE SET
                cooldown_until = GREATEST(
                    COALESCE(telegram_channel_state.cooldown_until, CURRENT_TIMESTAMP),
                    EXCLUDED.cooldown_until
                ),
                last_error = EXCLUDED.last_error,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(account_name)
        .bind(channel_id)
        .bind(seconds)
        .bind(queue::head(error, 500))
        .execute(self.db().pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    /// `mark_channel_downloaded`.
    async fn mark_channel_downloaded(&self, account_name: &str, channel_id: i64) -> Result<(), TgError> {
        sqlx::query(
            "INSERT INTO telegram_channel_state (account_name, channel_id, last_download_at)
            VALUES ($1, $2, CURRENT_TIMESTAMP)
            ON CONFLICT (account_name, channel_id)
            DO UPDATE SET
                last_download_at = CURRENT_TIMESTAMP,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(account_name)
        .bind(channel_id)
        .execute(self.db().pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    /// `get_backfilled_media_types`.
    pub async fn backfilled_types(&self, account_name: &str, channel_id: i64) -> Result<HashSet<JobType>, TgError> {
        let rows = sqlx::query("SELECT media_type FROM telegram_channel_media_backfill WHERE account_name = $1 AND channel_id = $2")
            .bind(account_name)
            .bind(channel_id)
            .fetch_all(self.db().pool())
            .await
            .map_err(db_error)?;
        let mut types = HashSet::new();
        for row in &rows {
            let text: String = row.try_get("media_type").map_err(db_error)?;
            if let Some(kind) = JobType::parse(&text) {
                types.insert(kind);
            }
        }
        Ok(types)
    }

    /// `mark_media_types_backfilled`.
    pub async fn mark_backfilled(&self, account_name: &str, channel_id: i64, types: &[JobType]) -> Result<(), TgError> {
        for kind in types {
            sqlx::query(
                "INSERT INTO telegram_channel_media_backfill (account_name, channel_id, media_type)
                VALUES ($1, $2, $3)
                ON CONFLICT (account_name, channel_id, media_type) DO NOTHING",
            )
            .bind(account_name)
            .bind(channel_id)
            .bind(kind.as_str())
            .execute(self.db().pool())
            .await
            .map_err(db_error)?;
        }
        Ok(())
    }

    /// The archive row of a saved message.
    async fn record(
        &self,
        account_name: &str,
        job: &TelegramMediaJob,
        title: &str,
        channel_name: &str,
        media_type: &str,
    ) -> Result<(), TgError> {
        sqlx::query(
            "INSERT INTO telegram (account_name, message_id, channel_id, title, channel_name, media_type)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (account_name, channel_id, message_id) DO NOTHING",
        )
        .bind(account_name)
        .bind(job.message_id)
        .bind(job.channel_id)
        .bind(title)
        .bind(channel_name)
        .bind(media_type)
        .execute(self.db().pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    /// `set_account_cooldown`: every channel of the account, a split channel once.
    pub async fn set_account_cooldown(&self, account: &TelegramAccount, seconds: f64, error: &str) -> Result<(), TgError> {
        for (channel_id, _) in self.channel_routes(account).await? {
            self.set_channel_cooldown(&account.name, channel_id, seconds, error).await?;
        }
        Ok(())
    }

    // ---------- notifications ----------

    async fn enqueue_notification(&self, notification: NewNotification, what: &str, message_id: i64) {
        if let Err(error) = self.inner.notifier.enqueue(notification).await {
            log().warning(format_args!("Failed to enqueue {what} for message {message_id}: {error}"));
        }
    }

    /// `_notify_download`.
    pub async fn notify_download(
        &self,
        account_name: &str,
        channel_name: &str,
        message_id: i64,
        title: &str,
        media_type: &str,
        saved_path: &Path,
    ) {
        let mut payload = dict(vec![
            ("account_name", Value::Str(account_name.to_owned())),
            ("channel_name", Value::Str(channel_name.to_owned())),
            ("message_id", Value::Int(i128::from(message_id))),
            ("saved_path", Value::Str(path_text(saved_path))),
        ]);
        if media_type == "image" {
            payload.insert("image_path", Value::Str(path_text(saved_path)));
        }
        let notification = NewNotification {
            kind: "download_completed".to_owned(),
            source: JOB_KEY.to_owned(),
            header: HEADER.to_owned(),
            title: title.to_owned(),
            body: format!("Channel {channel_name} | Message ID {message_id}"),
            payload: Some(payload),
            ..NewNotification::default()
        };
        self.enqueue_notification(notification, "telegram download notification", message_id)
            .await;
    }

    async fn notify_link_failure(&self, job: &TelegramMediaJob, body: &str, dedupe_key: &str) {
        let notification = NewNotification {
            kind: "download_failed".to_owned(),
            source: JOB_KEY.to_owned(),
            header: HEADER.to_owned(),
            title: job.title.clone(),
            body: body.to_owned(),
            dedupe_key: dedupe_key.to_owned(),
            ..NewNotification::default()
        };
        self.enqueue_notification(notification, "the link failure notification", job.message_id)
            .await;
    }

    async fn report_unsupported(&self, account: &TelegramAccount, channel_name: &str, job: &TelegramMediaJob, message: &TgMessage) {
        let reason = unsupported_reason(message);
        log().warning(format_args!(
            "Telegram message {}/{} in {channel_name} has nothing to save: {reason}",
            job.channel_id, job.message_id
        ));
        let notification = NewNotification {
            kind: KIND_UNSUPPORTED_MESSAGE.to_owned(),
            source: JOB_KEY.to_owned(),
            header: HEADER.to_owned(),
            title: format!("{channel_name} #{}", job.message_id),
            link_url: format!("https://t.me/c/{}/{}", job.channel_id, job.message_id),
            body: reason.clone(),
            payload: Some(dict(vec![
                ("account_name", Value::Str(account.name.clone())),
                ("channel_name", Value::Str(channel_name.to_owned())),
                ("message_id", Value::Int(i128::from(job.message_id))),
                ("reason", Value::Str(reason)),
            ])),
            dedupe_key: unsupported_key(&account.name, job.channel_id, job.message_id),
            ..NewNotification::default()
        };
        self.enqueue_notification(notification, "the unsupported-message notification", job.message_id)
            .await;
    }

    /// `_resolve_unsupported`: unpin the report of a message saved after all.
    async fn resolve_unsupported(&self, account: &TelegramAccount, channel_name: &str, job: &TelegramMediaJob, saved: &Path) {
        let resolution = Resolution {
            kind: KIND_UNSUPPORTED_RESOLVED.to_owned(),
            source: JOB_KEY.to_owned(),
            header: HEADER.to_owned(),
            title: format!("{channel_name} #{}", job.message_id),
            link_url: format!("https://t.me/c/{}/{}", job.channel_id, job.message_id),
            body: format!(
                "Saved after all: {}",
                saved
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            ..Resolution::default()
        };
        let key = unsupported_key(&account.name, job.channel_id, job.message_id);
        if let Err(error) = self.inner.notifier.resolve(key, resolution).await {
            log().warning(format_args!(
                "Failed to resolve the unsupported-message report for message {}: {error}",
                job.message_id
            ));
        }
    }

    // ---------- scanning ----------

    /// `scan_media`: a channel's messages as Telethon pages them, picked and titled.
    #[allow(clippy::too_many_arguments)] // reason: Python's keyword arguments
    async fn scan_media(
        &self,
        client: &dyn TgClient,
        channel: &Channel,
        types: &[JobType],
        min_id: i64,
        limit: Option<i64>,
        wait_time: Option<f64>,
        newest_window: bool,
    ) -> Result<ChannelScan, TgError> {
        let scan = Scan {
            min_id,
            limit,
            reverse: !newest_window,
            wait_time,
        };
        let mut messages = history::iter_messages(client, channel, scan).await?;
        if newest_window {
            messages.sort_by_key(|message| message.id);
        }
        build_scan(&messages, types)
    }

    /// `_enqueue_entries`: queue the entries; how many were new.
    async fn enqueue_entries(
        &self,
        account: &TelegramAccount,
        channel_id: i64,
        entries: &[&Entry],
        source: QueueSource,
        delay_seconds: f64,
    ) -> Result<usize, TgError> {
        let mut inserted = 0;
        for entry in entries {
            if entry.msg.id <= 0 {
                continue;
            }
            let new = queue::enqueue(
                self.db(),
                &account.name,
                channel_id,
                entry.msg.id,
                entry.msg.grouped_id,
                entry.media_type,
                &entry.filename,
                source,
                delay_seconds,
            )
            .await
            .map_err(|error| TgError::py(&error))?;
            inserted += usize::from(new);
        }
        if !entries.is_empty() {
            self.wake_if_waiting(&account.name);
        }
        Ok(inserted)
    }

    /// `_backfill_channel_media`: a type the channel did not have before looks back over
    /// its newest window.
    #[allow(clippy::too_many_arguments)] // reason: Python's keyword arguments
    async fn backfill(
        &self,
        account: &TelegramAccount,
        channel_id: i64,
        channel: &Channel,
        types: &[JobType],
        client: &dyn TgClient,
        channel_name: &str,
        config: &TelegramConfig,
    ) -> Result<(), TgError> {
        let scan_types = history_types(types);
        if scan_types.is_empty() {
            return self.mark_backfilled(&account.name, channel_id, types).await;
        }
        let scan = self
            .scan_media(
                client,
                channel,
                &scan_types,
                0,
                Some(config.scan_limit),
                Some(config.history_wait_seconds),
                true,
            )
            .await?;
        let downloaded = self.downloaded_ids(&account.name, channel_id, 0).await?;
        let pending: Vec<&Entry> = scan.media.iter().filter(|entry| !downloaded.contains(&entry.msg.id)).collect();
        let inserted = self
            .enqueue_entries(account, channel_id, &pending, QueueSource::Reconciliation, 0.0)
            .await?;
        self.mark_backfilled(&account.name, channel_id, types).await?;
        log().info(format_args!(
            "Telegram backfill for {channel_name} scanned {} messages and queued {inserted} new media for types={}",
            scan.scanned_message_count,
            types.iter().map(|kind| kind.as_str()).collect::<Vec<_>>().join(",")
        ));
        Ok(())
    }

    /// `update_channel`: reconcile a channel's history into the queue, without downloading.
    pub async fn update_channel(&self, channel_id: i64, account: &TelegramAccount, client: &dyn TgClient) -> Result<(), TgError> {
        let Some(routes) = self.routes_of(account, channel_id).await? else {
            return Err(TgError::new(
                "ValueError",
                format!("telegram channel is not configured: {channel_id}"),
                ErrorKind::Other,
            ));
        };
        let config = self.cfg().await?;
        let types = job_types(&routes);
        let channel = client.get_channel(channel_id).await?;
        let channel_name = channel.name();
        let state = self.channel_state(&account.name, channel_id).await?;
        if state.cooldown_remaining_seconds > 0.0 {
            log().info(format_args!(
                "Telegram channel {channel_name} is cooling down for {:.0}s; skip reconciliation",
                state.cooldown_remaining_seconds
            ));
            return Ok(());
        }
        if types.contains(&JobType::Unsupported) {
            self.recheck_unsupported(account, channel_id, client, &channel_name, &types).await?;
        }
        let backfilled = self.backfilled_types(&account.name, channel_id).await?;
        let pending_backfill: Vec<JobType> = types.iter().copied().filter(|kind| !backfilled.contains(kind)).collect();
        if state.last_scanned_message_id > 0 && !pending_backfill.is_empty() {
            self.backfill(account, channel_id, &channel, &pending_backfill, client, &channel_name, &config)
                .await?;
        }
        // A channel seen for the first time looks back over its history.
        let first_time = state.last_scanned_message_id == 0;
        let scan_types = if first_time { history_types(&types) } else { types.clone() };
        let scan = self
            .scan_media(
                client,
                &channel,
                &scan_types,
                state.last_scanned_message_id,
                Some(config.scan_limit),
                Some(config.history_wait_seconds),
                first_time,
            )
            .await?;
        if scan.max_message_id <= state.last_scanned_message_id {
            if first_time && !pending_backfill.is_empty() {
                self.mark_backfilled(&account.name, channel_id, &pending_backfill).await?;
            }
            log().info(format_args!("No new Telegram messages to reconcile in {channel_name}"));
            return Ok(());
        }
        let downloaded = self
            .downloaded_ids(&account.name, channel_id, state.last_scanned_message_id)
            .await?;
        let mut inserted = 0_usize;
        let mut cursor = state.last_scanned_message_id;
        let mut limit_reached = false;
        let limit = usize::try_from(config.download_limit_per_channel).unwrap_or(0);
        for batch in media_batches(&scan.media) {
            let pending: Vec<&Entry> = batch.iter().copied().filter(|entry| !downloaded.contains(&entry.msg.id)).collect();
            if !pending.is_empty() {
                if inserted >= limit {
                    limit_reached = true;
                    break;
                }
                inserted += self
                    .enqueue_entries(account, channel_id, &pending, QueueSource::Reconciliation, 0.0)
                    .await?;
            }
            cursor = batch.iter().map(|entry| entry.msg.id).fold(cursor, i64::max);
        }
        if !limit_reached {
            cursor = cursor.max(scan.max_message_id);
        }
        if cursor > state.last_scanned_message_id {
            self.set_last_scanned(&account.name, channel_id, cursor).await?;
        }
        if first_time && !limit_reached && !pending_backfill.is_empty() {
            self.mark_backfilled(&account.name, channel_id, &pending_backfill).await?;
        }
        log().info(format_args!(
            "Telegram reconciliation for {channel_name} scanned {} messages and queued {inserted} new media{}",
            scan.scanned_message_count,
            if limit_reached { " (per-run limit reached)" } else { "" }
        ));
        Ok(())
    }

    /// `_recheck_unsupported`: queue again each reported message this source has since
    /// learned to save; close the report of one that was deleted.
    pub async fn recheck_unsupported(
        &self,
        account: &TelegramAccount,
        channel_id: i64,
        client: &dyn TgClient,
        channel_name: &str,
        types: &[JobType],
    ) -> Result<(), TgError> {
        let ids = queue::open_unsupported_reports(self.db(), &account.name, channel_id, 100)
            .await
            .map_err(|error| TgError::py(&error))?;
        if ids.is_empty() {
            return Ok(());
        }
        let messages = history::get_messages(client, channel_id, &ids).await?;
        if messages.len() != ids.len() {
            let which = if messages.len() < ids.len() {
                "2 is shorter than argument 1"
            } else {
                "2 is longer than argument 1"
            };
            return Err(TgError::new("ValueError", format!("zip() argument {which}"), ErrorKind::Other));
        }
        let mut requeued = 0;
        for (message_id, message) in ids.iter().zip(messages) {
            let Some(message) = message else {
                // Deleted from the channel: nothing left to save. The report stays pinned until it is unpinned by hand.
                queue::close_unsupported_report(self.db(), &account.name, channel_id, *message_id, "Message was deleted")
                    .await
                    .map_err(|error| TgError::py(&error))?;
                continue;
            };
            let Some(media_type) = message_media_type(&message, types).map_err(|error| TgError::py(&error))? else {
                continue;
            };
            if media_type == JobType::Unsupported {
                continue;
            }
            let entry = standalone_entry(Entry {
                msg: message,
                filename: String::new(),
                media_type,
            });
            if queue::requeue_unsupported_report(self.db(), &account.name, channel_id, *message_id, media_type, &entry.filename)
                .await
                .map_err(|error| TgError::py(&error))?
            {
                requeued += 1;
            }
        }
        if requeued > 0 {
            log().info(format_args!(
                "Telegram queued {requeued} reported messages in {channel_name} again: they can be saved now"
            ));
            self.wake_if_waiting(&account.name);
        }
        Ok(())
    }

    fn reconcile_lock(&self, account_name: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(locked(&self.inner.reconcile_locks).entry(account_name.to_owned()).or_default())
    }

    /// `_reconcile_account`: every channel in turn; a flood wait cools the whole account
    /// down and ends the pass, any other failure cools the channel down and is raised.
    pub async fn reconcile_account(&self, account: &TelegramAccount, client: &dyn TgClient) -> Result<(), TgError> {
        let lock = self.reconcile_lock(&account.name);
        let _guard = lock.lock().await;
        for (channel_id, _) in self.channel_routes(account).await? {
            let Err(error) = self.update_channel(channel_id, account, client).await else {
                continue;
            };
            let config = self.cfg().await?;
            if let ErrorKind::FloodWait(seconds) = error.kind {
                let wait = f64::from(seconds);
                let cooldown = wait.max(config.channel_cooldown_seconds);
                self.set_account_cooldown(account, cooldown, &format!("FloodWaitError: {wait:.0}s"))
                    .await?;
                log().warning(format_args!(
                    "Telegram account {} hit FloodWait during reconciliation; cooling down {cooldown:.0}s",
                    account.name
                ));
                break;
            }
            self.set_channel_cooldown(&account.name, channel_id, config.channel_cooldown_seconds, &error.formatted())
                .await?;
            return Err(error);
        }
        Ok(())
    }

    // ---------- events ----------

    /// `_enqueue_event_messages`: a new message or album, queued at event priority.
    pub async fn enqueue_event_messages(&self, account: &TelegramAccount, messages: &[TgMessage], album: bool) -> Result<(), TgError> {
        let Some(first) = messages.first() else { return Ok(()) };
        let Some(channel_id) = first.channel_id else { return Ok(()) };
        let Some(routes) = self.routes_of(account, channel_id).await? else {
            return Ok(());
        };
        let entries = build_scan(messages, &job_types(&routes))?.media;
        if entries.is_empty() {
            return Ok(());
        }
        let refs: Vec<&Entry> = entries.iter().collect();
        let delay = if album { ALBUM_SETTLE_SECONDS } else { 0.0 };
        let inserted = self.enqueue_entries(account, channel_id, &refs, QueueSource::Event, delay).await?;
        log().info(format_args!(
            "Telegram event queued {inserted}/{} media for account={} channel={channel_id}{}",
            entries.len(),
            account.name,
            if album { " album" } else { "" }
        ));
        Ok(())
    }

    /// The two event handlers over a connection's new messages, until it closes.
    pub async fn handle_events(&self, account: &TelegramAccount, mut messages: mpsc::Receiver<TgMessage>) {
        let mut albums = Albums::default();
        loop {
            let due = albums.next_due();
            let event = tokio::select! {
                received = messages.recv() => match received {
                    Some(message) => albums.push(message, tokio::time::Instant::now()).map(|event| vec![event]).unwrap_or_default(),
                    None => return,
                },
                () = async {
                    match due {
                        Some(due) => tokio::time::sleep_until(due).await,
                        None => std::future::pending().await,
                    }
                } => albums.take_due(tokio::time::Instant::now()),
            };
            for event in event {
                let (batch, album, what) = match event {
                    Event::NewMessage(message) => (vec![message], false, "NewMessage"),
                    Event::Album(messages) => (messages, true, "Album"),
                };
                if let Err(error) = self.enqueue_event_messages(account, &batch, album).await {
                    log().error(format_args!(
                        "Failed to persist Telegram {what} event for account {}: {}",
                        account.name,
                        error.formatted()
                    ));
                }
            }
        }
    }

    // ---------- downloads ----------

    /// `download`: the message's media into `dst_dir`, named `title [id].ext` with the
    /// extension Telethon chose; `None` when there was nothing to download.
    #[allow(clippy::too_many_arguments)] // reason: Python's `_download` signature, argument for argument
    pub async fn download(
        &self,
        client: &dyn TgClient,
        message: &TgMessage,
        dst_dir: &Path,
        title: &str,
        media_type: &str,
        account_name: &str,
        channel_id: i64,
    ) -> Result<Option<PathBuf>, TgError> {
        if dst_dir.is_file() {
            return Err(TgError::new(
                "ValueError",
                format!("{} is a file", dst_dir.display()),
                ErrorKind::Other,
            ));
        }
        let account = if account_name.is_empty() { "account" } else { account_name };
        let tmp_path = self
            .cache_dir()?
            .join(format!("{}_{channel_id}_{media_type}_{}", sanitize(account, 200), message.id));
        let Some(plan) = files::plan(message, &tmp_path, Extensions::system(), &|path: &Path| path.is_file()) else {
            return Ok(None);
        };
        let written = match &plan.location {
            Location::Inline(bytes) => tokio::fs::write(&plan.path, bytes).await.map_err(|error| TgError::io(&error)),
            location => {
                let msg_data = (message.channel_id.unwrap_or(channel_id), message.id);
                client.download(location, &plan.path, plan.size, msg_data).await
            }
        };
        if let Err(error) = written {
            // Telethon leaves the partial file in its temporary folder (D152).
            let _ = tokio::fs::remove_file(&plan.path).await;
            return Err(error);
        }
        let filename = format_media_filename(title, &message.id.to_string(), None, &py_suffix(&plan.path), 240);
        let dst_path = dst_dir.join(filename);
        let (src, dst, dir) = (plan.path.clone(), dst_path.clone(), dst_dir.to_path_buf());
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            fav_net::download::move_file(&src, &dst)
        })
        .await
        .map_err(|error| TgError::runtime(error.to_string()))?
        .map_err(|error| TgError::io(&error))?;
        Ok(Some(dst_path))
    }

    /// `_archive_job`: download a route's media, record it and say so.
    async fn archive_job(
        &self,
        account: &TelegramAccount,
        client: &dyn TgClient,
        channel_cfg: &TelegramChannel,
        channel_name: &str,
        job: &TelegramMediaJob,
        message: &TgMessage,
    ) -> Result<Option<PathBuf>, TgError> {
        let Some(result) = self
            .download(
                client,
                message,
                channel_cfg.path.as_path(),
                &job.title,
                &job.media_type,
                &account.name,
                job.channel_id,
            )
            .await?
        else {
            return Ok(None);
        };
        self.record(&account.name, job, &job.title, channel_name, &job.media_type).await?;
        self.mark_channel_downloaded(&account.name, job.channel_id).await?;
        self.notify_download(&account.name, channel_name, job.message_id, &job.title, &job.media_type, &result)
            .await;
        Ok(Some(result))
    }

    /// `_existing_image`: a picture a previous try saved, under any extension.
    fn existing_image(image_dir: &Path, page: &ArticlePage, message_id: i64, suffix: &str) -> Option<PathBuf> {
        LINK_IMAGE_EXTENSIONS
            .iter()
            .map(|ext| image_dir.join(link_filename(page, message_id, suffix, ext)))
            .find(|path| path.exists())
    }

    /// `_save_link_media`: the post's pictures and videos; a file a previous try saved is
    /// kept as it is.
    pub async fn save_link_media(
        &self,
        page: &ArticlePage,
        message_id: i64,
        image_dir: &Path,
        video_dir: &Path,
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>), TgError> {
        let links = self.links()?;
        let mut images = Vec::new();
        for (index, url) in page.images.iter().enumerate() {
            let suffix = if page.images.len() > 1 {
                (index + 1).to_string()
            } else {
                String::new()
            };
            if let Some(existing) = Self::existing_image(image_dir, page, message_id, &suffix) {
                images.push(existing);
                continue;
            }
            let Some((content, ext)) = links.client().fetch_image(url).await.map_err(|error| TgError::py(&error))? else {
                continue;
            };
            let dst_path = image_dir.join(link_filename(page, message_id, &suffix, ext));
            tokio::fs::create_dir_all(image_dir).await.map_err(|error| TgError::io(&error))?;
            tokio::fs::write(&dst_path, &content).await.map_err(|error| TgError::io(&error))?;
            images.push(dst_path);
        }
        let mut videos = Vec::new();
        for (index, video) in page.videos.iter().enumerate() {
            let suffix = if page.videos.len() > 1 {
                format!("video-{}", index + 1)
            } else {
                String::new()
            };
            let dst_path = video_dir.join(link_filename(page, message_id, &suffix, "mp4"));
            if dst_path.exists() {
                videos.push(dst_path);
                continue;
            }
            let partial = self.cache_dir()?.join(format!("{}.part", uuid::Uuid::new_v4().simple()));
            if links
                .client()
                .fetch_video(video, &partial)
                .await
                .map_err(|error| TgError::py(&error))?
            {
                let (src, dst, dir) = (partial.clone(), dst_path.clone(), video_dir.to_path_buf());
                tokio::task::spawn_blocking(move || {
                    std::fs::create_dir_all(&dir)?;
                    fav_net::download::move_file(&src, &dst)
                })
                .await
                .map_err(|error| TgError::runtime(error.to_string()))?
                .map_err(|error| TgError::io(&error))?;
                videos.push(dst_path);
            }
        }
        Ok((images, videos))
    }

    /// `_archive_link_job`: save a linked post's pictures and videos to the channel's
    /// routes, each falling back to the other.
    async fn archive_link_job(
        &self,
        account: &TelegramAccount,
        routes: &Routes,
        channel_name: &str,
        job: &TelegramMediaJob,
        link: &ArticleLink,
    ) -> Result<PathBuf, JobFailure> {
        let py = |error: crate::pydecode::PyError| JobFailure::Tg(TgError::py(&error));
        let site = post_links::site_name(link).map_err(py)?;
        let (mut page, _html) = match self.links()?.fetch_page(link).await {
            Ok(fetched) => fetched,
            // WeChat opens such a post in WeChat itself; from here there is no way past it.
            Err(PostError::Link(error)) if error.captcha => {
                return Err(JobFailure::Link(PostLinkError::new(format!("{error}; share it to WeChat instead"))));
            }
            Err(PostError::Link(error)) => return Err(JobFailure::Link(error)),
            Err(PostError::Other(error)) => return Err(py(error)),
        };
        let title = [&page.title, &link.title, &job.title]
            .into_iter()
            .find(|title| !title.is_empty())
            .cloned()
            .unwrap_or_default();
        page.title.clone_from(&title);
        let image_route = route_for(routes, "image").or_else(|| route_for(routes, "video"));
        let video_route = route_for(routes, "video").or_else(|| route_for(routes, "image"));
        let (Some(image_route), Some(video_route)) = (image_route, video_route) else {
            return Err(JobFailure::Tg(TgError::new("KeyError", "'video'", ErrorKind::Other)));
        };
        let (images, videos) = self
            .save_link_media(&page, job.message_id, image_route.path.as_path(), video_route.path.as_path())
            .await?;
        let url = canonical_url(&link.url).map_err(py)?;
        if images.is_empty() && videos.is_empty() {
            let message = if page.images.is_empty() && page.videos.is_empty() {
                format!("{url} has no pictures or videos that could be downloaded")
            } else {
                format!(
                    "none of the {} images and {} videos of {url} could be downloaded",
                    page.images.len(),
                    page.videos.len()
                )
            };
            return Err(JobFailure::Link(PostLinkError::new(message)));
        }
        self.record(&account.name, job, &title, channel_name, JobType::Link.as_str())
            .await?;
        self.mark_channel_downloaded(&account.name, job.channel_id).await?;
        let counts = format!(
            "{}/{} images, {}/{} videos",
            images.len(),
            page.images.len(),
            videos.len(),
            page.videos.len()
        );
        log().info(format_args!("Saved {site} post {url} from {channel_name} ({counts})"));
        let paths = |paths: &[PathBuf]| Value::List(paths.iter().map(|path| Value::Str(path_text(path))).collect());
        let mut payload = dict(vec![
            ("account_name", Value::Str(account.name.clone())),
            ("channel_name", Value::Str(channel_name.to_owned())),
            ("message_id", Value::Int(i128::from(job.message_id))),
            ("url", Value::Str(url.clone())),
            ("images", paths(&images)),
            ("videos", paths(&videos)),
        ]);
        if let Some(first) = images.first() {
            payload.insert("image_path", Value::Str(path_text(first)));
        }
        // A post of videos alone shows its first video's cover, which Telegram fetches itself.
        let cover_url = if images.is_empty() {
            page.videos
                .iter()
                .map(|video| video.cover_url.clone())
                .find(|cover| !cover.is_empty())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let account_part = if page.account.is_empty() {
            String::new()
        } else {
            format!(" · {}", page.account)
        };
        let notification = NewNotification {
            kind: "download_completed".to_owned(),
            source: JOB_KEY.to_owned(),
            header: HEADER.to_owned(),
            title: title.clone(),
            link_url: url,
            image_url: cover_url,
            body: format!("{site}{account_part} | {counts}"),
            payload: Some(payload),
            ..NewNotification::default()
        };
        self.enqueue_notification(notification, "the link download notification", job.message_id)
            .await;
        Ok(images.into_iter().chain(videos).next().unwrap_or_default())
    }

    /// `_link_failed`: a link that waits on a setting waits for it; any other is retried
    /// and finally given up and reported.
    async fn link_failed(&self, job: &TelegramMediaJob, owner_token: &str, error: &PostLinkError) -> Result<(), TgError> {
        let text = format!("{}: {error}", error.class_name());
        let py = |error: crate::pydecode::PyError| TgError::py(&error);
        if error.needs != Needs::Nothing {
            let needs = error.needs.as_str();
            queue::mark_retry(self.db(), job, owner_token, &text, LINK_WAIT_SECONDS)
                .await
                .map_err(py)?;
            log().warning(format_args!(
                "Telegram link message {}/{} waits for {needs}: {error}",
                job.channel_id, job.message_id
            ));
            self.notify_link_failure(job, &format!("Waiting for a setting: {error}"), &format!("telegram:links:{needs}"))
                .await;
            return Ok(());
        }
        if job.attempt_count < LINK_MAX_ATTEMPTS {
            let delay = queue::retry_delay(job.attempt_count);
            queue::mark_retry(self.db(), job, owner_token, &text, delay).await.map_err(py)?;
            log().warning(format_args!(
                "Telegram link message {}/{} will be retried in {delay:.0}s: {error}",
                job.channel_id, job.message_id
            ));
            return Ok(());
        }
        queue::mark_discarded(self.db(), job, owner_token, &text).await.map_err(py)?;
        log().warning(format_args!(
            "Telegram link message {}/{} given up after {} tries: {error}",
            job.channel_id, job.message_id, job.attempt_count
        ));
        self.notify_link_failure(job, &format!("Gave up on the link after {} tries: {error}", job.attempt_count), "")
            .await;
        Ok(())
    }

    /// The part of `_process_job` inside its `try`: `Ok(Err(downloaded))` when the job
    /// was finished on the way, `Ok(Ok(..))` with what was saved.
    #[allow(clippy::too_many_arguments)] // reason: the locals of `_process_job` its `try` block reads
    async fn try_job(
        &self,
        account: &TelegramAccount,
        client: &dyn TgClient,
        job: &TelegramMediaJob,
        owner_token: &str,
        routes: &Routes,
        channel_cfg: &TelegramChannel,
        message_job: bool,
    ) -> Result<Result<(PathBuf, String), bool>, JobFailure> {
        let py = |error: crate::pydecode::PyError| JobFailure::Tg(TgError::py(&error));
        let channel = client.get_channel(job.channel_id).await?;
        let channel_name = channel.name();
        let messages = history::get_messages(client, job.channel_id, &[job.message_id]).await?;
        let Some(Some(message)) = messages.into_iter().next() else {
            queue::mark_discarded(self.db(), job, owner_token, "Message was deleted or is unavailable")
                .await
                .map_err(py)?;
            return Ok(Err(false));
        };
        let types: Vec<JobType> = if message_job {
            job_types(routes)
        } else {
            channel_cfg.media_types.iter().copied().map(JobType::of_media).collect()
        };
        let current = message_media_type(&message, &types).map_err(py)?;
        if current.is_none_or(|current| current.as_str() != job.media_type) {
            queue::mark_discarded(self.db(), job, owner_token, "Configured media is no longer available")
                .await
                .map_err(py)?;
            return Ok(Err(false));
        }
        if job.media_type == JobType::Unsupported.as_str() {
            self.report_unsupported(account, &channel_name, job, &message).await;
            queue::mark_completed(self.db(), job, owner_token).await.map_err(py)?;
            return Ok(Err(false));
        }
        let link = if job.media_type == JobType::Link.as_str() {
            message_link(&message).map_err(py)?
        } else {
            None
        };
        let result = match link {
            Some(link) => Some(self.archive_link_job(account, routes, &channel_name, job, &link).await?),
            None => self.archive_job(account, client, channel_cfg, &channel_name, job, &message).await?,
        };
        let Some(result) = result else {
            let delay = queue::retry_delay(job.attempt_count);
            queue::mark_retry(self.db(), job, owner_token, "Telegram returned no downloaded media path", delay)
                .await
                .map_err(py)?;
            return Ok(Err(false));
        };
        Ok(Ok((result, channel_name)))
    }

    /// `_process_job`: one claimed job to its end; whether something was downloaded.
    pub async fn process_job(
        &self,
        account: &TelegramAccount,
        client: &dyn TgClient,
        job: &TelegramMediaJob,
        owner_token: &str,
    ) -> Result<bool, TgError> {
        let py = |error: crate::pydecode::PyError| TgError::py(&error);
        let routes = self.routes_of(account, job.channel_id).await?.unwrap_or_default();
        let message_job = job.media_type == JobType::Link.as_str() || job.media_type == JobType::Unsupported.as_str();
        let channel_cfg = if message_job {
            routes.first().map(|(_, channel)| channel.clone())
        } else {
            route_for(&routes, &job.media_type).cloned()
        };
        let Some(channel_cfg) = channel_cfg else {
            queue::mark_discarded(self.db(), job, owner_token, "Media route is no longer configured")
                .await
                .map_err(py)?;
            return Ok(false);
        };
        if self.is_downloaded(&account.name, job.channel_id, job.message_id).await? {
            queue::mark_completed(self.db(), job, owner_token).await.map_err(py)?;
            log().info(format_args!(
                "Telegram queue completed already archived message {}/{}",
                job.channel_id, job.message_id
            ));
            return Ok(false);
        }
        let state = self.channel_state(&account.name, job.channel_id).await?;
        if state.cooldown_remaining_seconds > 0.0 {
            queue::mark_retry(
                self.db(),
                job,
                owner_token,
                "Channel cooldown is active",
                state.cooldown_remaining_seconds,
            )
            .await
            .map_err(py)?;
            return Ok(false);
        }
        let outcome = self
            .try_job(account, client, job, owner_token, &routes, &channel_cfg, message_job)
            .await;
        let (result, channel_name) = match outcome {
            Ok(Ok(saved)) => saved,
            Ok(Err(downloaded)) => return Ok(downloaded),
            // Before the generic handlers: a post that will not load is no reason to cool the channel down.
            Err(JobFailure::Link(error)) => {
                self.link_failed(job, owner_token, &error).await?;
                return Ok(false);
            }
            Err(JobFailure::Tg(error)) => {
                self.job_failed(account, job, owner_token, &error).await?;
                return Ok(false);
            }
        };
        queue::mark_completed(self.db(), job, owner_token).await.map_err(py)?;
        self.resolve_unsupported(account, &channel_name, job, &result).await;
        log().notice(format_args!(
            "Telegram queue completed account={} channel={} message={}: {}",
            account.name,
            job.channel_id,
            job.message_id,
            result.display()
        ));
        Ok(true)
    }

    /// The `except` clauses of `_process_job` after the link one.
    async fn job_failed(
        &self,
        account: &TelegramAccount,
        job: &TelegramMediaJob,
        owner_token: &str,
        error: &TgError,
    ) -> Result<(), TgError> {
        let py = |error: crate::pydecode::PyError| TgError::py(&error);
        let config = self.cfg().await?;
        match error.kind {
            ErrorKind::MessageIdInvalid => {
                queue::mark_discarded(self.db(), job, owner_token, &error.formatted())
                    .await
                    .map_err(py)?;
            }
            ErrorKind::FloodWait(seconds) => {
                let wait = f64::from(seconds);
                let cooldown = wait.max(config.channel_cooldown_seconds);
                let text = format!("FloodWaitError: {wait:.0}s");
                self.set_account_cooldown(account, cooldown, &text).await?;
                queue::mark_retry(self.db(), job, owner_token, &text, cooldown).await.map_err(py)?;
                log().warning(format_args!(
                    "Telegram queue hit FloodWait for account {}; retry in {cooldown:.0}s",
                    account.name
                ));
            }
            ErrorKind::Rpc | ErrorKind::Os | ErrorKind::Runtime => {
                let delay = queue::retry_delay(job.attempt_count);
                let cooldown = if config.channel_cooldown_seconds > 0.0 {
                    delay.min(config.channel_cooldown_seconds)
                } else {
                    delay
                };
                self.set_channel_cooldown(&account.name, job.channel_id, cooldown, &error.formatted())
                    .await?;
                queue::mark_retry(self.db(), job, owner_token, &error.formatted(), delay)
                    .await
                    .map_err(py)?;
                log().warning(format_args!(
                    "Telegram queue retry account={} message={} in {delay:.0}s: {error}",
                    account.name, job.message_id
                ));
            }
            ErrorKind::Other => {
                let delay = queue::retry_delay(job.attempt_count);
                queue::mark_retry(self.db(), job, owner_token, &error.formatted(), delay)
                    .await
                    .map_err(py)?;
                log().error(format_args!(
                    "Unexpected Telegram queue error account={} message={}: {}",
                    account.name,
                    job.message_id,
                    error.formatted()
                ));
            }
        }
        Ok(())
    }

    /// `_consume_account_queue`: the account's serial worker. Between two downloads it
    /// waits `download_delay_seconds`; with `drain_only` it returns once the queue is empty.
    pub async fn consume_account_queue(
        &self,
        account: &TelegramAccount,
        client: &dyn TgClient,
        owner_token: &str,
        stop: &StopSignal,
        drain_only: bool,
    ) -> Result<(), TgError> {
        let wake = self.wake(&account.name);
        let mut delay_before_next = false;
        while !stop.is_set() {
            let job = queue::claim_next(self.db(), &account.name, owner_token)
                .await
                .map_err(|error| TgError::py(&error))?;
            let Some(job) = job else {
                if drain_only {
                    return Ok(());
                }
                tokio::select! {
                    () = wake.notified() => {}
                    () = tokio::time::sleep(QUEUE_IDLE_POLL) => {}
                    () = stop.wait() => {}
                }
                continue;
            };
            if delay_before_next {
                (self.inner.sleeper)(self.cfg().await?.download_delay_seconds).await;
            }
            delay_before_next = self.process_job(account, client, &job, owner_token).await?;
        }
        Ok(())
    }

    // ---------- connections ----------

    async fn client_config(&self, account: &TelegramAccount, receive_updates: bool) -> Result<ClientConfig, TgError> {
        let config = self.cfg().await?;
        Ok(ClientConfig {
            session_path: account.session_path.as_path().to_path_buf(),
            api_id: account.api_id,
            api_hash: account.api_hash.clone(),
            flood_sleep_threshold: config.flood_sleep_threshold_seconds,
            receive_updates,
            catch_up: receive_updates,
        })
    }

    fn set_ready(&self, account_name: &str, ready: bool) {
        self.inner.ready.send_modify(|accounts| {
            if ready {
                accounts.insert(account_name.to_owned());
            } else {
                accounts.remove(account_name);
            }
        });
    }

    /// `_wait_for_disconnect_or_stop`: until the connection drops, the worker ends, or a
    /// stop; a failure of the connection or of the worker is raised.
    pub async fn wait_for_disconnect_or_stop(
        &self,
        disconnected: BoxFuture<'static, Result<(), TgError>>,
        stop: &StopSignal,
        worker: &mut Option<JoinHandle<Result<(), TgError>>>,
    ) -> Result<(), TgError> {
        let closed = self.inner.closed_signal.clone();
        let Some(handle) = worker.as_mut() else {
            return tokio::select! {
                outcome = disconnected => outcome,
                () = stop.wait() => Ok(()),
                () = closed.wait() => Ok(()),
            };
        };
        let (outcome, finished) = tokio::select! {
            outcome = disconnected => (outcome, false),
            () = stop.wait() => (Ok(()), false),
            () = closed.wait() => (Ok(()), false),
            outcome = handle => (outcome.unwrap_or_else(|error| Err(TgError::runtime(error.to_string()))), true),
        };
        // A finished worker is taken out: its handle must not be awaited again.
        if finished {
            *worker = None;
        }
        outcome
    }

    /// `_run_account_once`: one connection with its listener and worker, until it ends.
    async fn run_account_once(&self, account: &TelegramAccount, stop: &StopSignal) -> Result<(), TgError> {
        let owner_token = uuid::Uuid::new_v4().simple().to_string();
        let config = self.client_config(account, true).await?;
        let connection = self.inner.connector.connect(config).await;
        let Connection {
            client,
            messages,
            disconnected,
        } = match connection {
            Ok(connection) => connection,
            Err(error) => {
                log().info(format_args!("Telegram listener disconnected for account {}", account.name));
                return Err(error);
            }
        };
        let mut worker: Option<JoinHandle<Result<(), TgError>>> = None;
        let mut listener: Option<JoinHandle<()>> = None;
        let result = async {
            if !client.is_authorized().await? {
                return Err(unauthorized(account));
            }
            locked(&self.inner.clients).insert(account.name.clone(), Arc::clone(&client));
            let recovered = queue::reset_processing(self.db(), &account.name)
                .await
                .map_err(|error| TgError::py(&error))?;
            if recovered > 0 {
                log().warning(format_args!(
                    "Recovered {recovered} Telegram queue jobs for account {}",
                    account.name
                ));
            }
            if let Some(messages) = messages {
                let (runtime, account) = (self.clone(), account.clone());
                listener = Some(tokio::spawn(async move { runtime.handle_events(&account, messages).await }));
            }
            self.set_ready(&account.name, true);
            let (runtime, worker_account, worker_client, worker_stop) = (self.clone(), account.clone(), Arc::clone(&client), stop.clone());
            worker = Some(tokio::spawn(async move {
                runtime
                    .consume_account_queue(&worker_account, worker_client.as_ref(), &owner_token, &worker_stop, false)
                    .await
            }));
            log().notice(format_args!("Telegram listener connected for account {}", account.name));
            self.wait_for_disconnect_or_stop(disconnected, stop, &mut worker).await
        }
        .await;
        self.set_ready(&account.name, false);
        if let Some(worker) = worker {
            worker.abort();
            let _ = worker.await;
        }
        if let Some(listener) = listener {
            listener.abort();
            let _ = listener.await;
        }
        locked(&self.inner.clients).remove(&account.name);
        client.disconnect().await;
        log().info(format_args!("Telegram listener disconnected for account {}", account.name));
        result
    }

    /// `_run_account_forever`: reconnect with a growing delay; an unauthorized session
    /// ends the runtime.
    async fn run_account_forever(&self, account: &TelegramAccount, stop: &StopSignal) -> Result<(), TgError> {
        let mut delay = RECONNECT_INITIAL_SECONDS;
        let closed = self.inner.closed_signal.clone();
        while !stop.is_set() && !closed.is_set() {
            let lock_name = account_lock_name(account);
            match self.db().try_advisory_lock(&lock_name).await {
                Ok(None) => log().warning(format_args!(
                    "Telegram session lock is held for account {}; retry in {delay:.0}s",
                    account.name
                )),
                Ok(Some(lock)) => {
                    let outcome = self.run_account_once(account, stop).await;
                    lock.release().await;
                    match outcome {
                        Ok(()) => delay = RECONNECT_INITIAL_SECONDS,
                        Err(error) if is_unauthorized(&error) => return Err(error),
                        Err(error) => log().warning(format_args!(
                            "Telegram listener for account {} failed; reconnect in {delay:.0}s: {error}",
                            account.name
                        )),
                    }
                }
                // Outside the listener's `try` in Python: the runtime fails.
                Err(error) => return Err(db_job(&error)),
            }
            if stop.is_set() || closed.is_set() {
                return Ok(());
            }
            tokio::select! {
                () = stop.wait() => return Ok(()),
                () = closed.wait() => return Ok(()),
                () = tokio::time::sleep(Duration::from_secs_f64(delay)) => delay = (delay * 2.0).min(RECONNECT_MAX_SECONDS),
            }
        }
        Ok(())
    }

    /// `run`: the listeners and serial workers of every configured account, in parallel.
    pub async fn run(&self, stop: StopSignal) -> Result<(), TgError> {
        if self.inner.running.swap(true, Ordering::SeqCst) {
            return Err(TgError::runtime("Telegram runtime is already running"));
        }
        let outcome = async {
            let accounts = self.cfg().await?.accounts;
            let runs = accounts.iter().map(|account| self.run_account_forever(account, &stop));
            futures_util::future::try_join_all(runs).await.map(|_| ())
        }
        .await;
        self.inner.running.store(false, Ordering::SeqCst);
        outcome
    }

    /// `wait_until_ready`: every configured account connected.
    pub async fn wait_until_ready(&self) -> Result<(), TgError> {
        let names: Vec<String> = self.cfg().await?.accounts.into_iter().map(|account| account.name).collect();
        let mut ready = self.inner.ready.subscribe();
        ready
            .wait_for(|accounts| names.iter().all(|name| accounts.contains(name)))
            .await
            .map_err(|error| TgError::runtime(error.to_string()))?;
        Ok(())
    }

    /// `reconcile` / `update`: with the runtime running, a pass over every connected
    /// account; without it, a one-shot connection per account.
    pub async fn update(&self) -> Result<(), TgError> {
        if !self.inner.running.load(Ordering::SeqCst) {
            return self.run_one_shot().await;
        }
        let mut passes = Vec::new();
        for account in self.cfg().await?.accounts {
            let client = locked(&self.inner.clients).get(&account.name).cloned();
            let Some(client) = client else {
                log().warning(format_args!(
                    "Telegram account {} is not connected; reconciliation deferred",
                    account.name
                ));
                continue;
            };
            passes.push(async move { self.reconcile_account(&account, client.as_ref()).await });
        }
        futures_util::future::join_all(passes).await.into_iter().collect()
    }

    /// `update_account`: one account, once, unless its session is in use.
    pub async fn update_account(&self, account: &TelegramAccount) -> Result<(), TgError> {
        let Some(lock) = self
            .db()
            .try_advisory_lock(&account_lock_name(account))
            .await
            .map_err(|error| db_job(&error))?
        else {
            log().warning(format_args!(
                "Telegram account {} is already running; skip one-shot reconciliation",
                account.name
            ));
            return Ok(());
        };
        let outcome = self.run_one_shot_account(account).await;
        lock.release().await;
        outcome
    }

    async fn run_one_shot(&self) -> Result<(), TgError> {
        for account in self.cfg().await?.accounts {
            self.update_account(&account).await?;
        }
        Ok(())
    }

    /// `_run_one_shot_account`: connect without updates, reconcile, drain the queue.
    async fn run_one_shot_account(&self, account: &TelegramAccount) -> Result<(), TgError> {
        let owner_token = uuid::Uuid::new_v4().simple().to_string();
        let config = self.client_config(account, false).await?;
        let Connection { client, .. } = self.inner.connector.connect(config).await?;
        *locked(&self.inner.one_shot) = Some(Arc::clone(&client));
        let (_handle, stop) = stop_signal();
        let outcome = async {
            if !client.is_authorized().await? {
                return Err(unauthorized(account));
            }
            queue::reset_processing(self.db(), &account.name)
                .await
                .map_err(|error| TgError::py(&error))?;
            self.reconcile_account(account, client.as_ref()).await?;
            self.consume_account_queue(account, client.as_ref(), &owner_token, &stop, true)
                .await
        }
        .await;
        client.disconnect().await;
        *locked(&self.inner.one_shot) = None;
        outcome
    }

    /// `aclose`: disconnect every client and drop the temporary folder.
    pub async fn close(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.close.stop();
        let mut clients: Vec<Arc<dyn TgClient>> = locked(&self.inner.clients).drain().map(|(_, client)| client).collect();
        if let Some(client) = locked(&self.inner.one_shot).take() {
            clients.push(client);
        }
        for client in clients {
            client.disconnect().await;
        }
        let cache = locked(&self.inner.cache).take();
        if let Some(cache) = cache {
            let _ = cache.close();
        }
    }
}

fn db_error(error: sqlx::Error) -> TgError {
    TgError::py(&sql_error(error))
}

fn db_job(error: &fav_db::DbError) -> TgError {
    let job = crate::db_job_error(error);
    TgError::new(job.class_name(), job.message(), ErrorKind::Other)
}

fn job_error(error: &TgError) -> JobError {
    JobError::new(error.class.clone(), error.message.clone())
}

impl ResidentSource for Telegram {
    fn run(&self, stop: StopSignal) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move { Telegram::run(self, stop).await.map_err(|error| job_error(&error)) })
    }

    fn wait_until_ready(&self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move { Telegram::wait_until_ready(self).await.map_err(|error| job_error(&error)) })
    }

    fn update(&self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move { Telegram::update(self).await.map_err(|error| job_error(&error)) })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            Telegram::close(self).await;
            Ok(())
        })
    }
}

/// Register the `telegram` job: resident in the worker, over grammers.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.resident(JOB_KEY, move || {
        let notifier: SharedNotifier = Arc::new(OutboxNotifier(Outbox::new(ctx.db.clone(), Arc::clone(&ctx.settings))));
        Arc::new(Telegram::new(Deps {
            db: ctx.db.clone(),
            settings: Arc::clone(&ctx.settings),
            notifier,
            connector: Arc::new(live::GrammersConnector),
            links: None,
            sleeper: None,
        })) as Arc<dyn ResidentSource>
    });
}

#[cfg(test)]
mod golden;
#[cfg(test)]
mod tests;
