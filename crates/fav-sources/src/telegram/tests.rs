//! `tests/test_telegram.py`, against a real database and a fake Telegram client.
//!
//! Python's tests swap the module's functions for fakes and read the calls they get. The
//! fake here is the client itself ([`FakeClient`], one channel's history, served as
//! Telegram pages `messages.getHistory`), so these run the whole path and check what it
//! leaves in the queue, the archive and the channel state.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use fav_core::settings::{Settings, TelegramAccount, TelegramChannel, TelegramMediaType};
use fav_core::validate::PyPath;
use fav_db::testdb::TestDatabase;
use fav_db::{Db, SettingsStore};
use fav_runtime::{BoxFuture, stop_signal};
use serde_json::json;
use sqlx::Row as _;
use tokio::sync::mpsc;

use super::client::{Channel, ClientConfig, Connection, Connector, ErrorKind, HistoryItem, HistoryPage, HistoryRequest, TgClient, TgError};
use super::files::Location;
use super::message::{Document, DocumentAttribute, Media, Photo, PhotoSize, TgMessage, WebPage, message_link, unsupported_reason};
use super::{
    ALBUM_SETTLE_SECONDS, Deps, LINK_MAX_ATTEMPTS, LINK_OUTAGE_KEY, LINK_OUTAGE_MAX_ATTEMPTS, LINK_WAIT_SECONDS, Sleeper, Telegram,
    account_lock_name, build_scan, first_line, job_types, owned_routes,
};
use crate::notifier::SharedNotifier;
use crate::post_links::{Endpoints, PostClient, PostLinks, SessionStore};
use crate::telegram_queue::{self as queue, JobType, QueueSource, TelegramMediaJob};
use crate::testkit::RecordingNotifier;
use crate::testutil::{FakeWeb, Reply, database, test_roots};

const CHANNEL: i64 = 123;
const ZHIHU_SHARE: &str =
    "高跟鞋配白丝真的很难看吗？ - 知乎用户的回答 - 知乎\nhttps://www.zhihu.com/question/1981023166554989468/answer/2100311203096573240";
const JPEG: &[u8] = b"\xff\xd8\xff\xe0jpeg-bytes";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\npng-bytes";
const MP4: &[u8] = b"\x00\x00\x00\x20ftypisom\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00video-bytes";
/// What the fake client writes for any download.
const MEDIA: &[u8] = b"telegram-media";

// ---------- messages ----------

fn text(id: i64, text: &str) -> TgMessage {
    TgMessage {
        text: text.to_owned(),
        ..TgMessage::new(id, CHANNEL)
    }
}

fn photo(id: i64) -> TgMessage {
    TgMessage {
        media: Some(Media::Photo(Some(Photo {
            id,
            sizes: vec![PhotoSize::Size {
                kind: "y".to_owned(),
                size: 14,
            }],
            ..Photo::default()
        }))),
        ..TgMessage::new(id, CHANNEL)
    }
}

fn video(id: i64) -> TgMessage {
    TgMessage {
        media: Some(Media::Document(Some(Document {
            size: 14,
            ..Document::of("video/mp4", vec![DocumentAttribute::Video { round_message: false }])
        }))),
        ..TgMessage::new(id, CHANNEL)
    }
}

fn document(id: i64, mime_type: &str, attributes: Vec<DocumentAttribute>) -> TgMessage {
    TgMessage {
        media: Some(Media::Document(Some(Document::of(mime_type, attributes)))),
        ..TgMessage::new(id, CHANNEL)
    }
}

fn grouped(message: TgMessage, group: i64) -> TgMessage {
    TgMessage {
        grouped_id: Some(group),
        ..message
    }
}

fn captioned(message: TgMessage, caption: &str) -> TgMessage {
    TgMessage {
        text: caption.to_owned(),
        ..message
    }
}

fn scanned(entries: &[super::Entry]) -> Vec<(i64, String, JobType)> {
    entries
        .iter()
        .map(|entry| (entry.msg.id, entry.filename.clone(), entry.media_type))
        .collect()
}

// ---------- the fake client ----------

/// One channel's history, as Telegram serves it.
#[derive(Default)]
struct FakeClient {
    messages: Mutex<Vec<TgMessage>>,
    requests: Mutex<Vec<(HistoryRequest, Instant)>>,
    lookups: Mutex<Vec<Vec<i64>>>,
    downloads: Mutex<Vec<PathBuf>>,
    channel_error: Mutex<Option<TgError>>,
    forbid_lookups: bool,
    /// Downloads write part of the file and then lose the connection.
    cut_downloads: bool,
    unauthorized: bool,
    disconnects: AtomicUsize,
}

impl FakeClient {
    fn with(messages: Vec<TgMessage>) -> Self {
        Self {
            messages: Mutex::new(messages),
            ..Self::default()
        }
    }

    fn requests(&self) -> Vec<HistoryRequest> {
        self.requests.lock().expect("lock").iter().map(|(request, _)| *request).collect()
    }
}

impl TgClient for FakeClient {
    fn is_authorized(&self) -> BoxFuture<'_, Result<bool, TgError>> {
        Box::pin(async move { Ok(!self.unauthorized) })
    }

    fn get_channel(&self, channel_id: i64) -> BoxFuture<'_, Result<Channel, TgError>> {
        Box::pin(async move {
            if let Some(error) = self.channel_error.lock().expect("lock").clone() {
                return Err(error);
            }
            Ok(Channel {
                id: channel_id,
                access_hash: 1,
                username: Some("demo_channel".to_owned()),
                title: Some("Demo".to_owned()),
            })
        })
    }

    /// Newest first: the `limit` messages from `add_offset` past the first one below
    /// `offset_id` (the newest when it is 0).
    fn get_history<'a>(&'a self, _channel: &'a Channel, request: HistoryRequest) -> BoxFuture<'a, Result<HistoryPage, TgError>> {
        Box::pin(async move {
            self.requests.lock().expect("lock").push((request, Instant::now()));
            let mut messages = self.messages.lock().expect("lock").clone();
            messages.sort_by_key(|message| std::cmp::Reverse(message.id));
            let start = if request.offset_id == 0 {
                0
            } else {
                messages
                    .iter()
                    .position(|message| message.id < request.offset_id)
                    .unwrap_or(messages.len())
            };
            let len = i64::try_from(messages.len()).expect("small");
            let first = i64::try_from(start).expect("small") + request.add_offset;
            let from = first.clamp(0, len);
            let to = (first + request.limit).clamp(0, len);
            let page = messages[usize::try_from(from).expect("bounded")..usize::try_from(to).expect("bounded")]
                .iter()
                .cloned()
                .map(HistoryItem::Message)
                .collect();
            Ok(HistoryPage {
                messages: page,
                complete: false,
            })
        })
    }

    fn get_messages<'a>(&'a self, channel_id: i64, ids: &'a [i64]) -> BoxFuture<'a, Result<Vec<Option<TgMessage>>, TgError>> {
        Box::pin(async move {
            assert!(!self.forbid_lookups, "already archived jobs must not refetch Telegram");
            self.lookups.lock().expect("lock").push(ids.to_vec());
            let messages = self.messages.lock().expect("lock").clone();
            Ok(ids
                .iter()
                .map(|id| {
                    messages
                        .iter()
                        .find(|message| message.id == *id && message.channel_id == Some(channel_id))
                        .cloned()
                })
                .collect())
        })
    }

    fn download<'a>(
        &'a self,
        _location: &'a Location,
        path: &'a Path,
        _size: Option<i64>,
        _msg_data: (i64, i64),
    ) -> BoxFuture<'a, Result<(), TgError>> {
        Box::pin(async move {
            self.downloads.lock().expect("lock").push(path.to_path_buf());
            if self.cut_downloads {
                tokio::fs::write(path, &MEDIA[..4]).await.map_err(|error| TgError::io(&error))?;
                return Err(TgError::connection("Connection to Telegram failed 5 time(s)"));
            }
            tokio::fs::write(path, MEDIA).await.map_err(|error| TgError::io(&error))
        })
    }

    fn disconnect(&self) -> BoxFuture<'_, ()> {
        self.disconnects.fetch_add(1, AtomicOrdering::SeqCst);
        Box::pin(async {})
    }
}

/// Hands out the fake client and remembers how each connection was configured.
#[derive(Default)]
struct FakeConnector {
    client: Arc<FakeClient>,
    /// `(flood_sleep_threshold, receive_updates, catch_up, session_path)`.
    configs: Mutex<Vec<(i64, bool, bool, PathBuf)>>,
    updates: Mutex<Vec<mpsc::Sender<TgMessage>>>,
}

impl Connector for FakeConnector {
    fn connect(&self, config: ClientConfig) -> BoxFuture<'_, Result<Connection, TgError>> {
        Box::pin(async move {
            self.configs.lock().expect("lock").push((
                config.flood_sleep_threshold,
                config.receive_updates,
                config.catch_up,
                config.session_path.clone(),
            ));
            let messages = config.receive_updates.then(|| {
                let (sender, receiver) = mpsc::channel(16);
                self.updates.lock().expect("lock").push(sender);
                receiver
            });
            Ok(Connection {
                client: Arc::clone(&self.client) as Arc<dyn TgClient>,
                messages,
                disconnected: Box::pin(std::future::pending()),
            })
        })
    }
}

// ---------- the environment ----------

struct Env {
    database: TestDatabase,
    settings: Settings,
    notifier: Arc<RecordingNotifier>,
    connector: Arc<FakeConnector>,
    sleeps: Arc<Mutex<Vec<f64>>>,
    dir: tempfile::TempDir,
    web: Option<FakeWeb>,
}

impl Env {
    async fn new(messages: Vec<TgMessage>) -> Self {
        Self::with_client(FakeClient::with(messages)).await
    }

    async fn with_client(client: FakeClient) -> Self {
        Self {
            database: database().await,
            settings: fav_db::settings_store::default_settings(),
            notifier: Arc::new(RecordingNotifier::default()),
            connector: Arc::new(FakeConnector {
                client: Arc::new(client),
                ..FakeConnector::default()
            }),
            sleeps: Arc::new(Mutex::new(Vec::new())),
            dir: tempfile::tempdir().expect("temp dir"),
            web: None,
        }
    }

    fn db(&self) -> &Db {
        self.database.db()
    }

    fn client(&self) -> &FakeClient {
        &self.connector.client
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn telegram(&self) -> Telegram {
        let store = Arc::new(SettingsStore::pinned(self.settings.clone()));
        let links = self.web.as_ref().map(|web| {
            let client = PostClient::with_roots(Some(&web.proxy), Endpoints::default(), &test_roots()).expect("client");
            let sessions: Arc<dyn SessionStore> = Arc::new(self.db().clone());
            PostLinks::new(client, Arc::clone(&store), sessions)
        });
        let sleeps = Arc::clone(&self.sleeps);
        let sleeper: Sleeper = Arc::new(move |seconds| {
            sleeps.lock().expect("lock").push(seconds);
            Box::pin(async {})
        });
        Telegram::new(Deps {
            db: self.db().clone(),
            settings: store,
            notifier: Arc::clone(&self.notifier) as SharedNotifier,
            connector: Arc::clone(&self.connector) as Arc<dyn Connector>,
            links,
            sleeper: Some(sleeper),
        })
    }

    fn channel(&self, folder: &str, media_types: &[TelegramMediaType], report_unsupported: bool) -> TelegramChannel {
        TelegramChannel {
            id: CHANNEL,
            path: PyPath::new(self.path(folder).to_str().expect("UTF-8")),
            media_types: media_types.to_vec(),
            report_unsupported,
        }
    }

    fn account(&self, name: &str, channels: Vec<TelegramChannel>) -> TelegramAccount {
        TelegramAccount {
            name: name.to_owned(),
            channels,
            api_id: 1,
            api_hash: "hash".to_owned(),
            session_path: PyPath::new(self.path(&format!("{name}-session")).to_str().expect("UTF-8")),
        }
    }

    /// One channel saving `media_types` to `collection/`.
    fn single(&self, media_types: &[TelegramMediaType]) -> TelegramAccount {
        self.account("default", vec![self.channel("collection", media_types, false)])
    }

    /// Pictures to `image/`, videos to `video/`, of the same channel.
    fn split(&self, report_unsupported: bool) -> TelegramAccount {
        self.account(
            "default",
            vec![
                self.channel("image", &[TelegramMediaType::Image], report_unsupported),
                self.channel("video", &[TelegramMediaType::Video], false),
            ],
        )
    }

    async fn scanned_up_to(&self, message_id: i64) {
        sqlx::query("INSERT INTO telegram_channel_state (account_name, channel_id, last_scanned_message_id) VALUES ('default', $1, $2)")
            .bind(CHANNEL)
            .bind(message_id)
            .execute(self.db().pool())
            .await
            .expect("state");
    }

    async fn backfilled(&self, types: &[JobType]) {
        for kind in types {
            sqlx::query("INSERT INTO telegram_channel_media_backfill (account_name, channel_id, media_type) VALUES ('default', $1, $2)")
                .bind(CHANNEL)
                .bind(kind.as_str())
                .execute(self.db().pool())
                .await
                .expect("backfill");
        }
    }

    async fn archived(&self, message_id: i64) {
        sqlx::query(
            "INSERT INTO telegram (account_name, message_id, channel_id, title, channel_name, media_type)
            VALUES ('default', $1, $2, 'Old', 'demo_channel', 'video')",
        )
        .bind(message_id)
        .bind(CHANNEL)
        .execute(self.db().pool())
        .await
        .expect("archive row");
    }

    async fn backfill_rows(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT media_type FROM telegram_channel_media_backfill WHERE channel_id = $1 ORDER BY media_type")
            .bind(CHANNEL)
            .fetch_all(self.db().pool())
            .await
            .expect("backfill rows")
    }

    async fn cursor(&self) -> Option<i64> {
        sqlx::query_scalar("SELECT last_scanned_message_id FROM telegram_channel_state WHERE account_name = 'default' AND channel_id = $1")
            .bind(CHANNEL)
            .fetch_optional(self.db().pool())
            .await
            .expect("cursor")
    }

    /// `(message_id, title, source, media_type)` of every queued row.
    async fn queued(&self) -> Vec<(i64, String, String, String)> {
        sqlx::query("SELECT message_id, title, source, media_type FROM telegram_media_queue ORDER BY message_id")
            .fetch_all(self.db().pool())
            .await
            .expect("queue")
            .iter()
            .map(|row| (row.get("message_id"), row.get("title"), row.get("source"), row.get("media_type")))
            .collect()
    }

    async fn queued_ids(&self) -> Vec<i64> {
        self.queued().await.into_iter().map(|(id, ..)| id).collect()
    }

    /// Seconds from now until the row is due.
    async fn due_in(&self, message_id: i64) -> f64 {
        sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM available_at - CURRENT_TIMESTAMP)::DOUBLE PRECISION FROM telegram_media_queue WHERE message_id = $1",
        )
        .bind(message_id)
        .fetch_one(self.db().pool())
        .await
        .expect("due")
    }

    /// `(status, last_error)` of a queued row.
    async fn status(&self, message_id: i64) -> (String, String) {
        let row = sqlx::query("SELECT status, last_error FROM telegram_media_queue WHERE message_id = $1")
            .bind(message_id)
            .fetch_one(self.db().pool())
            .await
            .expect("row");
        (row.get("status"), row.get("last_error"))
    }

    /// A job queued and claimed by `owner`, on its `attempt`th try.
    async fn claimed(&self, message_id: i64, media_type: JobType, attempt: i64) -> TelegramMediaJob {
        queue::enqueue(
            self.db(),
            "default",
            CHANNEL,
            message_id,
            None,
            media_type,
            "Title",
            QueueSource::Event,
            0.0,
        )
        .await
        .expect("enqueue");
        sqlx::query("UPDATE telegram_media_queue SET attempt_count = $1 WHERE message_id = $2")
            .bind(i32::try_from(attempt - 1).expect("small"))
            .bind(message_id)
            .execute(self.db().pool())
            .await
            .expect("attempts");
        let job = queue::claim_next(self.db(), "default", "owner")
            .await
            .expect("claim")
            .expect("a job");
        assert_eq!((job.message_id, job.attempt_count), (message_id, attempt));
        job
    }

    async fn channel_cooldown(&self) -> Option<(f64, String)> {
        let row = sqlx::query(
            "SELECT EXTRACT(EPOCH FROM cooldown_until - CURRENT_TIMESTAMP)::DOUBLE PRECISION AS remaining, last_error
            FROM telegram_channel_state WHERE channel_id = $1",
        )
        .bind(CHANNEL)
        .fetch_optional(self.db().pool())
        .await
        .expect("state")?;
        let remaining: Option<f64> = row.get("remaining");
        remaining.map(|remaining| (remaining, row.get("last_error")))
    }

    async fn archive_rows(&self) -> Vec<(i64, String, String)> {
        sqlx::query("SELECT message_id, title, media_type FROM telegram ORDER BY message_id")
            .fetch_all(self.db().pool())
            .await
            .expect("archive")
            .iter()
            .map(|row| (row.get("message_id"), row.get("title"), row.get("media_type")))
            .collect()
    }

    async fn drop_queue(&self) {
        sqlx::query("DROP TABLE telegram_media_queue")
            .execute(self.db().pool())
            .await
            .expect("drop");
    }
}

fn names(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn payload_str(notification: &fav_runtime::notifications::NewNotification, key: &str) -> Option<String> {
    match notification.payload.as_ref()?.get(key)? {
        fav_core::pyjson::Value::Str(text) => Some(text.clone()),
        other => panic!("{key} is not a string: {other:?}"),
    }
}

// ---------- classification ----------

// py: tests/test_telegram.py::test_get_media_uses_configured_media_types
#[tokio::test]
async fn get_media_uses_configured_media_types() {
    let env = Env::new(vec![captioned(video(1), "Video Caption"), captioned(photo(2), "Image Caption")]).await;
    let account = env.split(false);
    let video_only = env.single(&[TelegramMediaType::Video]);
    let telegram = env.telegram();

    telegram
        .update_channel(CHANNEL, &video_only, env.client())
        .await
        .expect("reconciled");
    assert_eq!(
        env.queued().await,
        [(1, "Video Caption".to_owned(), "reconciliation".to_owned(), "video".to_owned())]
    );

    sqlx::query("DELETE FROM telegram_media_queue")
        .execute(env.db().pool())
        .await
        .expect("clear");
    sqlx::query("DELETE FROM telegram_channel_state")
        .execute(env.db().pool())
        .await
        .expect("clear");
    telegram.update_channel(CHANNEL, &account, env.client()).await.expect("reconciled");
    assert_eq!(
        env.queued().await,
        [
            (1, "Video Caption".to_owned(), "reconciliation".to_owned(), "video".to_owned()),
            (2, "Image Caption".to_owned(), "reconciliation".to_owned(), "image".to_owned()),
        ]
    );
}

// py: tests/test_telegram.py::test_get_media_supports_image_documents_and_skips_stickers_and_previews
#[test]
fn get_media_supports_image_documents_and_skips_stickers_and_previews() {
    let preview = |id: i64, page: WebPage| TgMessage {
        text: "Link preview".to_owned(),
        media: Some(Media::WebPage(page)),
        ..TgMessage::new(id, CHANNEL)
    };
    let messages = vec![
        document(1, "image/png", Vec::new()),
        document(2, "image/webp", vec![DocumentAttribute::Sticker]),
        preview(
            3,
            WebPage {
                loaded: true,
                photo: Some(Photo::default()),
                ..WebPage::default()
            },
        ),
        preview(
            4,
            WebPage {
                loaded: true,
                document: Some(Document::of("image/png", Vec::new())),
                ..WebPage::default()
            },
        ),
    ];
    let scan = build_scan(&messages, &[JobType::Image]).expect("scan");
    assert_eq!(scanned(&scan.media), [(1, "image_1".to_owned(), JobType::Image)]);
}

// py: tests/test_telegram.py::test_album_uses_caption_and_indexes_mixed_media
#[test]
fn album_uses_caption_and_indexes_mixed_media() {
    let messages = vec![
        grouped(captioned(photo(1), "Album Caption"), 99),
        grouped(video(2), 99),
        grouped(document(3, "application/octet-stream", Vec::new()), 99),
    ];
    let scan = build_scan(&messages, &[JobType::Image, JobType::Video]).expect("scan");
    assert_eq!(
        scanned(&scan.media),
        [
            (1, "Album Caption-1".to_owned(), JobType::Image),
            (2, "Album Caption-2".to_owned(), JobType::Video),
        ]
    );
    assert_eq!((scan.max_message_id, scan.scanned_message_count), (3, 3));
}

// py: tests/test_telegram.py::test_image_notification_includes_local_attachment
#[tokio::test]
async fn image_notification_includes_local_attachment() {
    let env = Env::new(Vec::new()).await;
    let telegram = env.telegram();
    let image = env.path("image.jpg");
    telegram.notify_download("default", "demo", 1, "Image", "image", &image).await;
    telegram
        .notify_download("default", "demo", 2, "Video", "video", &env.path("video.mp4"))
        .await;

    let sent = env.notifier.sent();
    assert_eq!(payload_str(&sent[0], "image_path"), Some(image.to_string_lossy().into_owned()));
    assert_eq!(payload_str(&sent[1], "image_path"), None);
    assert_eq!(sent[0].body, "Channel demo | Message ID 1");
    assert_eq!(
        (sent[0].kind.as_str(), sent[0].source.as_str(), sent[0].header.as_str()),
        ("download_completed", "telegram", "Telegram")
    );
}

// py: tests/test_telegram.py::test_initialize_tables_creates_media_backfill_state
#[tokio::test]
async fn initialize_tables_creates_media_backfill_state() {
    let env = Env::new(Vec::new()).await;
    let primary: Vec<String> = sqlx::query_scalar(
        "SELECT a.attname FROM pg_index i
        JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
        WHERE i.indrelid = 'telegram_channel_media_backfill'::regclass AND i.indisprimary
        ORDER BY array_position(i.indkey, a.attnum)",
    )
    .fetch_all(env.db().pool())
    .await
    .expect("primary key");
    assert_eq!(primary, ["account_name", "channel_id", "media_type"]);
}

// ---------- reconciliation ----------

// py: tests/test_telegram.py::test_split_routes_reconcile_channel_once_and_stop_cursor_at_limit
#[tokio::test]
async fn split_routes_reconcile_channel_once_and_stop_cursor_at_limit() {
    let mut env = Env::new(vec![
        text(100, "seen"),
        captioned(video(101), "One"),
        captioned(photo(102), "Two"),
        captioned(video(103), "Three"),
        text(104, "nothing to save"),
    ])
    .await;
    env.settings.web.telegram.scan_limit = 10;
    env.settings.web.telegram.download_limit_per_channel = 2;
    env.settings.web.telegram.history_wait_seconds = 0.5;
    env.scanned_up_to(100).await;
    env.backfilled(&[JobType::Image, JobType::Video, JobType::Link]).await;
    let account = env.split(false);

    env.telegram().reconcile_account(&account, env.client()).await.expect("reconciled");

    assert_eq!(
        env.queued().await,
        [
            (101, "One".to_owned(), "reconciliation".to_owned(), "video".to_owned()),
            (102, "Two".to_owned(), "reconciliation".to_owned(), "image".to_owned()),
        ]
    );
    assert_eq!(env.cursor().await, Some(102));
    // One forward scan of the channel from the cursor, for both routes, paced by the wait.
    let requests = env.client().requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!((requests[0].offset_id, requests[0].add_offset, requests[0].limit), (101, -10, 10));
    assert_eq!((requests[1].offset_id, requests[1].add_offset, requests[1].limit), (105, -6, 6));
    let times: Vec<Instant> = env.client().requests.lock().expect("lock").iter().map(|(_, at)| *at).collect();
    assert!(times[1].duration_since(times[0]).as_secs_f64() >= 0.45);
}

// py: tests/test_telegram.py::test_reconciliation_enqueues_whole_album_before_advancing_cursor
#[tokio::test]
async fn reconciliation_enqueues_whole_album_before_advancing_cursor() {
    let mut env = Env::new(vec![grouped(photo(11), 50), grouped(photo(12), 50)]).await;
    env.settings.web.telegram.download_limit_per_channel = 1;
    env.scanned_up_to(10).await;
    env.backfilled(&[JobType::Image, JobType::Link]).await;
    let account = env.single(&[TelegramMediaType::Image]);

    env.telegram()
        .update_channel(CHANNEL, &account, env.client())
        .await
        .expect("reconciled");

    assert_eq!(env.queued_ids().await, [11, 12]);
    assert_eq!(env.cursor().await, Some(12));
}

// py: tests/test_telegram.py::test_reconciliation_does_not_advance_cursor_when_enqueue_fails
#[tokio::test]
async fn reconciliation_does_not_advance_cursor_when_enqueue_fails() {
    let env = Env::new(vec![video(11)]).await;
    env.scanned_up_to(10).await;
    env.backfilled(&[JobType::Video, JobType::Link]).await;
    env.drop_queue().await;
    let account = env.single(&[TelegramMediaType::Video]);

    let error = env
        .telegram()
        .update_channel(CHANNEL, &account, env.client())
        .await
        .expect_err("the queue is gone");

    assert_eq!(error.class, "UndefinedTable");
    assert_eq!(env.cursor().await, Some(10));
}

// py: tests/test_telegram.py::test_new_media_type_backfills_full_recent_window
#[tokio::test]
async fn new_media_type_backfills_full_recent_window() {
    let mut messages: Vec<TgMessage> = (90..=95).map(video).collect();
    messages.extend([video(96), video(97), video(98), text(99, ZHIHU_SHARE), photo(100)]);
    let mut env = Env::new(messages).await;
    env.settings.web.telegram.scan_limit = 5;
    env.settings.web.telegram.download_limit_per_channel = 1;
    env.scanned_up_to(100).await;
    env.backfilled(&[JobType::Image]).await;
    env.archived(96).await;
    let account = env.split(false);

    env.telegram()
        .update_channel(CHANNEL, &account, env.client())
        .await
        .expect("reconciled");

    // The newest window, for the types not backfilled yet: links are a type of their own,
    // so a channel that never had them scanned backfills them too. The picture is not
    // looked back for, nor is anything past the window or already archived.
    assert_eq!(env.queued_ids().await, [97, 98, 99]);
    assert_eq!(env.queued().await[2].3, "link");
    assert_eq!(env.backfill_rows().await, ["image", "link", "video"]);
    let requests = env.client().requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    // Newest first, then forward from the cursor.
    assert_eq!((requests[0].offset_id, requests[0].add_offset, requests[0].limit), (0, 0, 5));
    assert_eq!((requests[1].offset_id, requests[1].add_offset, requests[1].limit), (101, -5, 5));
}

// py: tests/test_telegram.py::test_backfill_failure_does_not_mark_complete
#[tokio::test]
async fn backfill_failure_does_not_mark_complete() {
    let env = Env::new(vec![video(90), text(100, "")]).await;
    env.scanned_up_to(100).await;
    env.drop_queue().await;
    let account = env.single(&[TelegramMediaType::Video]);

    let error = env
        .telegram()
        .update_channel(CHANNEL, &account, env.client())
        .await
        .expect_err("the queue is gone");

    assert_eq!(error.class, "UndefinedTable");
    assert!(env.backfill_rows().await.is_empty());
}

// ---------- events ----------

// py: tests/test_telegram.py::test_single_event_and_album_are_persisted_with_event_priority
#[tokio::test]
async fn single_event_and_album_are_persisted_with_event_priority() {
    let env = Env::new(Vec::new()).await;
    let account = env.split(false);
    let telegram = env.telegram();

    telegram
        .enqueue_event_messages(&account, &[captioned(photo(1), "Single")], false)
        .await
        .expect("single");
    telegram
        .enqueue_event_messages(&account, &[grouped(captioned(photo(2), "Album"), 99), grouped(video(3), 99)], true)
        .await
        .expect("album");

    let queued: Vec<(i64, String, String)> = env
        .queued()
        .await
        .into_iter()
        .map(|(id, title, source, _)| (id, title, source))
        .collect();
    assert_eq!(
        queued,
        [
            (1, "Single".to_owned(), "event".to_owned()),
            (2, "Album-1".to_owned(), "event".to_owned()),
            (3, "Album-2".to_owned(), "event".to_owned()),
        ]
    );
    assert!(env.due_in(1).await <= 0.0);
    for id in [2, 3] {
        let due = env.due_in(id).await;
        assert!((ALBUM_SETTLE_SECONDS - 1.0..=ALBUM_SETTLE_SECONDS).contains(&due), "{due}");
    }
    let priority: Vec<i32> = sqlx::query_scalar("SELECT priority FROM telegram_media_queue ORDER BY message_id")
        .fetch_all(env.db().pool())
        .await
        .expect("priorities");
    assert_eq!(priority, [100, 100, 100]);
}

// py: tests/test_telegram.py::test_event_handlers_route_grouped_messages_only_through_album
#[tokio::test]
async fn event_handlers_route_grouped_messages_only_through_album() {
    let env = Env::new(Vec::new()).await;
    let account = env.single(&[TelegramMediaType::Image]);
    let telegram = env.telegram();
    let (sender, receiver) = mpsc::channel(8);
    let handler = {
        let (telegram, account) = (telegram.clone(), account.clone());
        tokio::spawn(async move { telegram.handle_events(&account, receiver).await })
    };

    let queued_within = |expected: Vec<i64>| {
        let env = &env;
        async move {
            let deadline = Instant::now() + std::time::Duration::from_secs(30);
            while env.queued_ids().await != expected {
                assert!(Instant::now() < deadline, "never queued {expected:?}");
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    };
    sender.send(photo(1)).await.expect("sent");
    queued_within(vec![1]).await;
    let sent_at = Instant::now();
    sender.send(grouped(photo(2), 99)).await.expect("sent");
    sender.send(grouped(photo(3), 99)).await.expect("sent");
    // The album's parts are no message events of their own: they wait for the album to settle.
    assert_eq!(env.queued_ids().await, [1]);
    queued_within(vec![1, 2, 3]).await;
    assert!(sent_at.elapsed() >= super::albums::HACK_DELAY);
    assert!(env.due_in(1).await <= 0.0);
    assert!(env.due_in(2).await > 0.0, "the album waits to settle");
    drop(sender);
    handler.await.expect("handler ends with the connection");
}

// ---------- jobs ----------

// py: tests/test_telegram.py::test_existing_archive_completes_queue_without_fetching_message
#[tokio::test]
async fn existing_archive_completes_queue_without_fetching_message() {
    let env = Env::with_client(FakeClient {
        forbid_lookups: true,
        ..FakeClient::default()
    })
    .await;
    env.archived(10).await;
    let job = env.claimed(10, JobType::Image, 1).await;

    let downloaded = env
        .telegram()
        .process_job(&env.single(&[TelegramMediaType::Image]), env.client(), &job, "owner")
        .await
        .expect("processed");

    assert!(!downloaded);
    assert_eq!(env.status(10).await.0, "completed");
}

// py: tests/test_telegram.py::test_split_route_worker_selects_media_destination
#[tokio::test]
async fn split_route_worker_selects_media_destination() {
    let env = Env::new(vec![photo(10), video(11)]).await;
    let account = env.split(false);
    let telegram = env.telegram();

    let image = env.claimed(10, JobType::Image, 1).await;
    let image_downloaded = telegram.process_job(&account, env.client(), &image, "owner").await.expect("image");
    let video_job = env.claimed(11, JobType::Video, 1).await;
    let video_downloaded = telegram
        .process_job(&account, env.client(), &video_job, "owner")
        .await
        .expect("video");

    assert!(image_downloaded);
    assert!(video_downloaded);
    assert_eq!(names(&env.path("image")), ["Title [10].jpg"]);
    assert_eq!(names(&env.path("video")), ["Title [11].mp4"]);
    assert_eq!(std::fs::read(env.path("image").join("Title [10].jpg")).expect("saved"), MEDIA);
    assert_eq!(env.status(10).await.0, "completed");
    assert_eq!(env.status(11).await.0, "completed");
}

// py: tests/test_telegram.py::test_queue_job_is_discarded_when_media_route_was_removed
#[tokio::test]
async fn queue_job_is_discarded_when_media_route_was_removed() {
    let env = Env::with_client(FakeClient {
        forbid_lookups: true,
        ..FakeClient::default()
    })
    .await;
    let job = env.claimed(10, JobType::Video, 1).await;

    let downloaded = env
        .telegram()
        .process_job(&env.single(&[TelegramMediaType::Image]), env.client(), &job, "owner")
        .await
        .expect("processed");

    assert!(!downloaded);
    assert_eq!(
        env.status(10).await,
        ("discarded".to_owned(), "Media route is no longer configured".to_owned())
    );
}

// py: tests/test_telegram.py::test_missing_message_is_permanently_discarded
#[tokio::test]
async fn missing_message_is_permanently_discarded() {
    let env = Env::new(Vec::new()).await;
    let job = env.claimed(10, JobType::Image, 1).await;

    let downloaded = env
        .telegram()
        .process_job(&env.single(&[TelegramMediaType::Image]), env.client(), &job, "owner")
        .await
        .expect("processed");

    assert!(!downloaded);
    assert_eq!(
        env.status(10).await,
        ("discarded".to_owned(), "Message was deleted or is unavailable".to_owned())
    );
    assert_eq!(*env.client().lookups.lock().expect("lock"), [vec![10]]);
}

// py: tests/test_telegram.py::test_successful_image_job_archives_notifies_and_does_not_set_cooldown
#[tokio::test]
async fn successful_image_job_archives_notifies_and_does_not_set_cooldown() {
    let env = Env::new(vec![photo(10)]).await;
    let job = env.claimed(10, JobType::Image, 1).await;

    let downloaded = env
        .telegram()
        .process_job(&env.single(&[TelegramMediaType::Image]), env.client(), &job, "owner")
        .await
        .expect("processed");

    assert!(downloaded);
    assert_eq!(env.status(10).await.0, "completed");
    assert_eq!(env.archive_rows().await, [(10, "Title".to_owned(), "image".to_owned())]);
    let sent = env.notifier.sent();
    let saved = env.path("collection").join("Title [10].jpg");
    assert_eq!(payload_str(&sent[0], "saved_path"), Some(saved.to_string_lossy().into_owned()));
    assert_eq!(payload_str(&sent[0], "image_path"), Some(saved.to_string_lossy().into_owned()));
    assert_eq!(sent[0].title, "Title");
    assert_eq!(
        env.channel_cooldown().await,
        None,
        "a successful download must not cool the channel down"
    );
}

// py: tests/test_telegram.py::test_download_creates_missing_destination_directory
#[tokio::test]
async fn download_creates_missing_destination_directory() {
    let env = Env::new(Vec::new()).await;
    let destination = env.path("missing").join("video");

    let saved = env
        .telegram()
        .download(env.client(), &video(10), &destination, "Video", "video", "default", CHANNEL)
        .await
        .expect("downloaded")
        .expect("a file");

    assert_eq!(saved.parent(), Some(destination.as_path()));
    assert_eq!(saved.file_name().and_then(|name| name.to_str()), Some("Video [10].mp4"));
    assert_eq!(std::fs::read(&saved).expect("saved"), MEDIA);
    // The download went to the runtime's own temporary folder first.
    let partial = env.client().downloads.lock().expect("lock")[0].clone();
    assert!(!partial.starts_with(&destination));
    assert!(!partial.exists());
}

/// A download cut short leaves no partial file behind (D152), cools the channel down
/// and retries the job, as an `OSError` does in Python.
#[tokio::test]
async fn a_download_cut_short_is_retried_and_leaves_nothing_behind() {
    let mut env = Env::with_client(FakeClient {
        cut_downloads: true,
        ..FakeClient::with(vec![video(10)])
    })
    .await;
    env.settings.web.telegram.channel_cooldown_seconds = 600.0;
    let job = env.claimed(10, JobType::Video, 1).await;

    let downloaded = env
        .telegram()
        .process_job(&env.single(&[TelegramMediaType::Video]), env.client(), &job, "owner")
        .await
        .expect("processed");

    assert!(!downloaded);
    let partial = env.client().downloads.lock().expect("lock")[0].clone();
    assert!(!partial.exists(), "the partial file is removed");
    assert!(names(&env.path("collection")).is_empty());
    let (status, error) = env.status(10).await;
    assert_eq!(status, "pending");
    assert_eq!(error, "ConnectionError: Connection to Telegram failed 5 time(s)");
    // The first retry's delay, under the channel cooldown setting.
    let (cooldown, cooled_by) = env.channel_cooldown().await.expect("cooling down");
    assert!((29.0..=30.0).contains(&cooldown), "{cooldown}");
    assert_eq!(cooled_by, error);
    assert!(env.archive_rows().await.is_empty());
    assert!(env.notifier.sent().is_empty());
}

// py: tests/test_telegram.py::test_flood_wait_cools_down_account_and_retries_job
#[tokio::test]
async fn flood_wait_cools_down_account_and_retries_job() {
    let mut env = Env::new(vec![photo(10)]).await;
    *env.client().channel_error.lock().expect("lock") = Some(TgError::rpc(420, "FLOOD_WAIT", Some(12), Some("GetChannelsRequest")));
    env.settings.web.telegram.channel_cooldown_seconds = 30.0;
    env.scanned_up_to(0).await;
    let job = env.claimed(10, JobType::Image, 1).await;

    let downloaded = env
        .telegram()
        .process_job(&env.single(&[TelegramMediaType::Image]), env.client(), &job, "owner")
        .await
        .expect("processed");

    assert!(!downloaded);
    let (cooldown, error) = env.channel_cooldown().await.expect("cooling down");
    assert!((29.0..=30.0).contains(&cooldown), "{cooldown}");
    assert_eq!(error, "FloodWaitError: 12s");
    let (status, error) = env.status(10).await;
    assert_eq!(status, "pending");
    assert!(error.contains("FloodWaitError"), "{error}");
    let due = env.due_in(10).await;
    assert!((29.0..=30.0).contains(&due), "{due}");
}

// py: tests/test_telegram.py::test_account_cooldown_updates_split_channel_once
#[tokio::test]
async fn account_cooldown_updates_split_channel_once() {
    let env = Env::new(Vec::new()).await;
    let account = env.split(false);
    assert_eq!(owned_routes(&account).len(), 1);

    env.telegram()
        .set_account_cooldown(&account, 30.0, "flood")
        .await
        .expect("cooled down");

    let rows: Vec<(String, i64, String)> = sqlx::query("SELECT account_name, channel_id, last_error FROM telegram_channel_state")
        .fetch_all(env.db().pool())
        .await
        .expect("state")
        .iter()
        .map(|row| (row.get("account_name"), row.get("channel_id"), row.get("last_error")))
        .collect();
    assert_eq!(rows, [("default".to_owned(), CHANNEL, "flood".to_owned())]);
    let (cooldown, _) = env.channel_cooldown().await.expect("cooling down");
    assert!((29.0..=30.0).contains(&cooldown), "{cooldown}");
}

// py: tests/test_telegram.py::test_account_worker_is_serial_and_delays_between_successes
#[tokio::test]
async fn account_worker_is_serial_and_delays_between_successes() {
    let mut env = Env::new(vec![photo(1), photo(2)]).await;
    env.settings.web.telegram.download_delay_seconds = 7.0;
    for id in [1, 2] {
        queue::enqueue(
            env.db(),
            "default",
            CHANNEL,
            id,
            None,
            JobType::Image,
            &format!("Image {id}"),
            QueueSource::Event,
            0.0,
        )
        .await
        .expect("enqueue");
    }
    let (_handle, stop) = stop_signal();

    env.telegram()
        .consume_account_queue(&env.single(&[TelegramMediaType::Image]), env.client(), "owner", &stop, true)
        .await
        .expect("drained");

    assert_eq!(names(&env.path("collection")), ["Image 1 [1].jpg", "Image 2 [2].jpg"]);
    assert_eq!(*env.sleeps.lock().expect("lock"), [7.0]);
}

// py: tests/test_telegram.py::test_queue_worker_failure_propagates_to_account_runtime
#[tokio::test]
async fn queue_worker_failure_propagates_to_account_runtime() {
    let env = Env::new(Vec::new()).await;
    let (_handle, stop) = stop_signal();
    let mut worker = Some(tokio::spawn(async { Err(TgError::runtime("worker failed")) }));

    let error = env
        .telegram()
        .wait_for_disconnect_or_stop(Box::pin(std::future::pending()), &stop, &mut worker)
        .await
        .expect_err("the worker failed");

    assert_eq!(error.formatted(), "RuntimeError: worker failed");
    // The finished worker is taken out, so the caller's clean-up never awaits it again.
    assert!(worker.is_none());
}

// ---------- connections ----------

// py: tests/test_telegram.py::test_persistent_account_client_enables_updates_and_catch_up
#[tokio::test]
async fn persistent_account_client_enables_updates_and_catch_up() {
    let mut env = Env::new(Vec::new()).await;
    let account = env.single(&[TelegramMediaType::Video]);
    env.settings.web.telegram.accounts = vec![account.clone()];
    // A job a previous owner left mid-download goes back to the queue.
    queue::enqueue(
        env.db(),
        "default",
        CHANNEL,
        5,
        None,
        JobType::Video,
        "Video",
        QueueSource::Event,
        0.0,
    )
    .await
    .expect("enqueue");
    sqlx::query(
        "UPDATE telegram_media_queue SET status = 'processing', owner_token = 'old', available_at = CURRENT_TIMESTAMP + INTERVAL '1 hour'",
    )
    .execute(env.db().pool())
    .await
    .expect("processing");
    let telegram = env.telegram();
    let (handle, stop) = stop_signal();
    let run = {
        let (telegram, account, stop) = (telegram.clone(), account.clone(), stop.clone());
        tokio::spawn(async move { telegram.run_account_once(&account, &stop).await })
    };

    tokio::time::timeout(std::time::Duration::from_secs(30), telegram.wait_until_ready())
        .await
        .expect("connected in time")
        .expect("ready");
    // The old owner's job was handed back before the workers started: it is no longer held
    // by `old` (a worker may already have claimed it again, or finished it, under its own
    // token; a worker never claims a row another owner still holds).
    let owner: Option<String> = sqlx::query_scalar("SELECT owner_token FROM telegram_media_queue WHERE message_id = 5")
        .fetch_one(env.db().pool())
        .await
        .expect("owner");
    assert_ne!(owner.as_deref(), Some("old"));
    handle.stop();
    run.await.expect("joined").expect("stopped cleanly");

    let configs = env.connector.configs.lock().expect("lock").clone();
    let threshold = env.settings.web.telegram.flood_sleep_threshold_seconds;
    assert_eq!(
        configs
            .iter()
            .map(|(flood, updates, catch_up, _)| (*flood, *updates, *catch_up))
            .collect::<Vec<_>>(),
        [(threshold, true, true)]
    );
    assert_eq!(env.client().disconnects.load(AtomicOrdering::SeqCst), 1);
}

// py: tests/test_telegram.py::test_accounts_run_in_parallel
#[tokio::test]
async fn accounts_run_in_parallel() {
    let mut env = Env::new(Vec::new()).await;
    let accounts = vec![
        env.account("one", vec![env.channel("one", &[TelegramMediaType::Video], false)]),
        env.account("two", vec![env.channel("two", &[TelegramMediaType::Video], false)]),
    ];
    env.settings.web.telegram.accounts = accounts;
    let telegram = env.telegram();
    let (handle, stop) = stop_signal();
    let run = {
        let telegram = telegram.clone();
        tokio::spawn(async move { telegram.run(stop).await })
    };

    // Both connected at once: neither waits for the other to finish.
    tokio::time::timeout(std::time::Duration::from_secs(30), telegram.wait_until_ready())
        .await
        .expect("both connected in time")
        .expect("ready");
    handle.stop();
    run.await.expect("joined").expect("stopped cleanly");

    let mut sessions: Vec<String> = env
        .connector
        .configs
        .lock()
        .expect("lock")
        .iter()
        .map(|(.., session)| session.file_name().expect("a name").to_string_lossy().into_owned())
        .collect();
    sessions.sort();
    assert_eq!(sessions, ["one-session", "two-session"]);
}

// py: tests/test_telegram.py::test_one_shot_uses_session_lock_and_rejects_unauthorized_session
#[tokio::test]
async fn one_shot_uses_session_lock_and_rejects_unauthorized_session() {
    let env = Env::with_client(FakeClient {
        unauthorized: true,
        ..FakeClient::default()
    })
    .await;
    let account = env.single(&[TelegramMediaType::Video]);
    let telegram = env.telegram();
    let lock_name = account_lock_name(&account);
    assert!(lock_name.starts_with("telegram-session:"));

    // While another owner holds the session, a one-shot run does not connect at all.
    let held = env.db().try_advisory_lock(&lock_name).await.expect("lock").expect("free");
    telegram.update_account(&account).await.expect("skipped");
    assert!(env.connector.configs.lock().expect("lock").is_empty());
    held.release().await;

    let error = telegram.update_account(&account).await.expect_err("unauthorized");

    assert_eq!(error.class, "TelegramSessionUnauthorizedError");
    assert!(error.message.contains("account=default"), "{}", error.message);
    let configs = env.connector.configs.lock().expect("lock").clone();
    assert_eq!(configs.len(), 1);
    assert!(!configs[0].1, "a one-shot connection receives no updates");
    assert_eq!(env.client().disconnects.load(AtomicOrdering::SeqCst), 1);
    // The lock went with the run.
    let again = env.db().try_advisory_lock(&lock_name).await.expect("lock");
    assert!(again.is_some());
}

// py: tests/test_telegram.py::test_account_lock_name_uses_effective_telethon_session_file
#[tokio::test]
async fn account_lock_name_uses_effective_telethon_session_file() {
    let env = Env::new(Vec::new()).await;
    let mut plain = env.single(&[TelegramMediaType::Video]);
    plain.session_path = PyPath::new(env.path("shared").to_str().expect("UTF-8"));
    let mut suffixed = env.account("other", Vec::new());
    suffixed.session_path = PyPath::new(env.path("shared.session").to_str().expect("UTF-8"));

    assert_eq!(account_lock_name(&plain), account_lock_name(&suffixed));
    let resolved = std::fs::canonicalize(env.dir.path()).expect("resolves").join("shared.session");
    assert_eq!(account_lock_name(&plain), format!("telegram-session:{}", resolved.display()));
}

// ---------- links ----------

const ZHIHU_TIKHUB_PATH: &str = "/api/v1/zhihu/web/fetch_answer_detail";
const ZHIHU_ANSWER_URL: &str = "https://www.zhihu.com/answer/2100311203096573240";

/// TikHub's answer for the share's answer, with `figures` pictures and `videos` lens ids.
fn zhihu_answer(title: &str, images: &[&str], videos: &[&str]) -> serde_json::Value {
    let mut content = String::new();
    for url in images {
        let _ = write!(content, "<figure><img data-original=\"{url}\"/></figure>");
    }
    for (index, id) in videos.iter().enumerate() {
        let poster = if index == 1 {
            r#" data-poster="https://picx.zhimg.com/v2-cover.png""#
        } else {
            ""
        };
        let _ = write!(
            content,
            r#"<a class="video-box" href="https://www.zhihu.com/video/{id}"{poster} data-lens-id="{id}"></a>"#
        );
    }
    json!({"data": {"content": content, "author": {"name": "someone"}, "question": {"title": title}}})
}

fn image_reply(content: &[u8], content_type: &str) -> Reply {
    Reply::Respond {
        status: 200,
        headers: vec![("content-type".to_owned(), content_type.to_owned())],
        body: content.to_vec(),
    }
}

/// The web a Zhihu link job reads: TikHub with `answer`, lens and the media.
async fn zhihu_web(answer: Option<serde_json::Value>) -> FakeWeb {
    FakeWeb::start(move |seen| match (seen.host(), seen.path()) {
        ("api.tikhub.io", ZHIHU_TIKHUB_PATH) => match &answer {
            Some(answer) => Reply::json(200, answer),
            None => Reply::json(404, &json!({"detail": {"code": 404, "message": "not found"}})),
        },
        ("lens.zhihu.com", path) => {
            let id = path.rsplit('/').next().unwrap_or_default();
            Reply::json(
                200,
                &json!({"playlist": {"HD": {"width": 720, "height": 1280, "play_url": format!("https://vdn3.vzuu.com/HD/{id}.mp4")}}}),
            )
        }
        (_, path) => match Path::new(path).extension().and_then(|ext| ext.to_str()) {
            Some("png") => image_reply(PNG, "image/png"),
            Some("jpg") => image_reply(JPEG, "image/jpeg"),
            Some("mp4") => image_reply(MP4, "video/mp4"),
            _ => Reply::status(404),
        },
    })
    .await
}

fn media_urls(web: &FakeWeb) -> Vec<String> {
    web.urls()
        .into_iter()
        .filter(|url| !url.contains("api.tikhub.io") && !url.contains("lens.zhihu.com"))
        .collect()
}

// py: tests/test_telegram.py::test_post_link_messages_are_scanned_as_links
#[test]
fn post_link_messages_are_scanned_as_links() {
    let preview = TgMessage {
        media: Some(Media::WebPage(WebPage {
            url: "https://tieba.baidu.com/p/9876543210?share=1".to_owned(),
            ..WebPage::default()
        })),
        ..TgMessage::new(2, CHANNEL)
    };
    let messages = vec![
        text(1, ZHIHU_SHARE),
        preview,
        captioned(photo(3), ZHIHU_SHARE),
        text(4, "https://example.com/post/1"),
        text(5, "看看 https://bbs.nga.cn/read.php?tid=12345&page=2"),
    ];

    let scan = build_scan(&messages, &[JobType::Image, JobType::Video, JobType::Link]).expect("scan");

    assert_eq!(
        scanned(&scan.media),
        [
            (1, "高跟鞋配白丝真的很难看吗？ - 知乎用户的回答 - 知乎".to_owned(), JobType::Link),
            (2, "link_2".to_owned(), JobType::Link),
            (3, ZHIHU_SHARE.to_owned(), JobType::Image),
            (5, "看看 https://bbs.nga.cn/read.php?tid=12345&page=2".to_owned(), JobType::Link),
        ]
    );
    assert!(
        build_scan(&messages[..1], &[JobType::Image, JobType::Video])
            .expect("scan")
            .media
            .is_empty()
    );
    assert_eq!(
        message_link(&messages[1]).expect("parses").expect("a link").url,
        "https://tieba.baidu.com/p/9876543210"
    );
    assert_eq!(first_line("a\u{2028}b"), "a");
}

// py: tests/test_telegram.py::test_link_job_saves_pictures_and_videos_to_their_routes
#[tokio::test]
async fn link_job_saves_pictures_and_videos_to_their_routes() {
    let mut env = Env::new(vec![text(10, ZHIHU_SHARE)]).await;
    let images = ["https://pica.zhimg.com/v2-a_r.jpg", "https://pica.zhimg.com/v2-b_r.png"];
    let web = zhihu_web(Some(zhihu_answer("高跟鞋配白丝真的很难看吗？", &images, &["2091194612627121396"]))).await;
    env.web = Some(web.clone());
    env.settings.services.tikhub.token = "tikhub-key".to_owned();
    let account = env.split(false);
    let job = env.claimed(10, JobType::Link, 1).await;
    let telegram = env.telegram();

    let downloaded = telegram
        .process_job(&account, env.client(), &job, "owner")
        .await
        .expect("processed");

    assert!(downloaded);
    assert_eq!(env.status(10).await.0, "completed");
    assert_eq!(
        media_urls(&web),
        [images[0], images[1], "https://vdn3.vzuu.com/HD/2091194612627121396.mp4"]
    );
    assert_eq!(
        names(&env.path("image")),
        [
            "[someone]高跟鞋配白丝真的很难看吗？-1 [10].jpg",
            "[someone]高跟鞋配白丝真的很难看吗？-2 [10].png"
        ]
    );
    assert_eq!(names(&env.path("video")), ["[someone]高跟鞋配白丝真的很难看吗？ [10].mp4"]);
    assert_eq!(
        std::fs::read(env.path("video").join("[someone]高跟鞋配白丝真的很难看吗？ [10].mp4")).expect("saved"),
        MP4
    );
    assert_eq!(
        env.archive_rows().await,
        [(10, "高跟鞋配白丝真的很难看吗？".to_owned(), "link".to_owned())]
    );
    let sent = env.notifier.sent();
    assert_eq!(sent[0].link_url, ZHIHU_ANSWER_URL);
    assert!(payload_str(&sent[0], "image_path").expect("a picture").ends_with("-1 [10].jpg"));
    assert_eq!(sent[0].image_url, "");
    assert_eq!(sent[0].body, "Zhihu · someone | 2/2 images, 1/1 videos");
    // Saving a message clears its unsupported report, if it ever had one.
    let resolved = env.notifier.resolved();
    assert_eq!(resolved[0].0, "telegram:unsupported:default:123:10");
    assert_eq!(env.channel_cooldown().await, None, "a link job must not cool the channel down");

    // A second try finds the files the first one saved and downloads nothing again.
    let before = web.urls().len();
    let page = crate::wechat_article::ArticlePage {
        title: "高跟鞋配白丝真的很难看吗？".to_owned(),
        account: "someone".to_owned(),
        published_at: None,
        images: images.iter().map(|url| (*url).to_owned()).collect(),
        picture_count: 2,
        videos: vec![crate::wechat_article::PostVideo::new("https://vdn3.vzuu.com/HD/v.mp4", "")],
    };
    let (saved_images, saved_videos) = telegram
        .save_link_media(&page, 10, &env.path("image"), &env.path("video"))
        .await
        .expect("saved");
    assert_eq!(web.urls().len(), before);
    assert_eq!((saved_images.len(), saved_videos.len()), (2, 1));
}

// py: tests/test_telegram.py::test_link_job_of_videos_alone_shows_the_video_cover
#[tokio::test]
async fn link_job_of_videos_alone_shows_the_video_cover() {
    let mut env = Env::new(vec![text(10, ZHIHU_SHARE)]).await;
    let web = zhihu_web(Some(zhihu_answer("黑色高跟鞋搭配什么颜色袜子好看？", &[], &["1", "2"]))).await;
    env.web = Some(web);
    env.settings.services.tikhub.token = "tikhub-key".to_owned();
    let job = env.claimed(10, JobType::Link, 1).await;

    let downloaded = env
        .telegram()
        .process_job(&env.split(false), env.client(), &job, "owner")
        .await
        .expect("processed");

    assert!(downloaded);
    let sent = env.notifier.sent();
    assert_eq!(sent[0].image_url, "https://picx.zhimg.com/v2-cover.png");
    assert_eq!(payload_str(&sent[0], "image_path"), None);
    assert_eq!(
        names(&env.path("video")),
        [
            "[someone]黑色高跟鞋搭配什么颜色袜子好看？-video-1 [10].mp4",
            "[someone]黑色高跟鞋搭配什么颜色袜子好看？-video-2 [10].mp4"
        ]
    );
}

// py: tests/test_telegram.py::test_unreadable_link_is_retried_then_given_up
#[tokio::test]
async fn unreadable_link_is_retried_then_given_up() {
    let mut env = Env::new(vec![text(10, ZHIHU_SHARE), text(11, ZHIHU_SHARE)]).await;
    env.web = Some(zhihu_web(None).await);
    env.settings.services.tikhub.token = "tikhub-key".to_owned();
    let account = env.split(false);
    let telegram = env.telegram();

    let first = env.claimed(10, JobType::Link, 1).await;
    telegram
        .process_job(&account, env.client(), &first, "owner")
        .await
        .expect("processed");
    let (status, error) = env.status(10).await;
    assert_eq!(status, "pending");
    assert!(error.starts_with("PostLinkError: "), "{error}");
    let due = env.due_in(10).await;
    assert!((29.0..=30.0).contains(&due), "{due}");
    assert!(env.notifier.sent().is_empty());

    let last = env.claimed(11, JobType::Link, LINK_MAX_ATTEMPTS).await;
    telegram
        .process_job(&account, env.client(), &last, "owner")
        .await
        .expect("processed");
    let (status, given_up) = env.status(11).await;
    assert_eq!(status, "discarded");
    assert_eq!(given_up, error);
    let sent = env.notifier.sent();
    assert_eq!(sent[0].kind, "download_failed");
    assert!(
        sent[0]
            .body
            .starts_with(&format!("Gave up on the link after {LINK_MAX_ATTEMPTS} tries: ")),
        "{}",
        sent[0].body
    );
    assert_eq!(env.channel_cooldown().await, None, "a link job must not cool the channel down");
}

/// What TikHub answers every Zhihu lookup with while its Zhihu API is down.
async fn zhihu_outage_web() -> FakeWeb {
    FakeWeb::start(|_| {
        Reply::json(
            400,
            &json!({"detail": {"code": 400, "message": "Request failed. Please retry. You won't be charged for this request."}}),
        )
    })
    .await
}

#[tokio::test]
async fn link_waits_out_an_outage_for_a_day_before_it_is_given_up() {
    let mut env = Env::new(vec![text(10, ZHIHU_SHARE), text(11, ZHIHU_SHARE), text(12, ZHIHU_SHARE)]).await;
    env.web = Some(zhihu_outage_web().await);
    env.settings.services.tikhub.token = "tikhub-key".to_owned();
    let account = env.split(false);
    let telegram = env.telegram();

    // The try an unreadable post is given up on: the link stays queued and says so.
    let waiting = env.claimed(10, JobType::Link, LINK_MAX_ATTEMPTS).await;
    telegram
        .process_job(&account, env.client(), &waiting, "owner")
        .await
        .expect("processed");
    let (status, error) = env.status(10).await;
    assert_eq!(status, "pending");
    assert!(
        error.starts_with(&format!(
            "PostLinkError: TikHub is failing for {ZHIHU_ANSWER_URL} (HTTP 400): Request failed. "
        )),
        "{error}"
    );
    let due = env.due_in(10).await;
    assert!((479.0..=480.0).contains(&due), "{due}");
    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].kind.as_str(), sent[0].dedupe_key.as_str()),
        ("download_failed", LINK_OUTAGE_KEY)
    );
    assert!(sent[0].body.starts_with("Waiting out an outage: "), "{}", sent[0].body);

    // Later tries are half an hour apart, and the outage is reported once, not per try or per link.
    let later = env.claimed(11, JobType::Link, LINK_OUTAGE_MAX_ATTEMPTS - 1).await;
    telegram
        .process_job(&account, env.client(), &later, "owner")
        .await
        .expect("processed");
    assert_eq!(env.status(11).await.0, "pending");
    let due = env.due_in(11).await;
    assert!((1799.0..=1800.0).contains(&due), "{due}");
    assert_eq!(env.notifier.sent().len(), 1);

    let last = env.claimed(12, JobType::Link, LINK_OUTAGE_MAX_ATTEMPTS).await;
    telegram
        .process_job(&account, env.client(), &last, "owner")
        .await
        .expect("processed");
    assert_eq!(env.status(12).await.0, "discarded");
    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].dedupe_key, "");
    assert!(
        sent[1]
            .body
            .starts_with(&format!("Gave up on the link after {LINK_OUTAGE_MAX_ATTEMPTS} tries: ")),
        "{}",
        sent[1].body
    );
    assert_eq!(env.channel_cooldown().await, None, "a link job must not cool the channel down");
}

/// A try can pass without failing here (a channel cooldown, a restart), so the report
/// does not hang on one try's number.
#[tokio::test]
async fn outage_is_reported_by_the_first_try_past_the_fifth_that_finds_it_unreported() {
    let mut env = Env::new(vec![text(10, ZHIHU_SHARE), text(11, ZHIHU_SHARE)]).await;
    env.web = Some(zhihu_outage_web().await);
    env.settings.services.tikhub.token = "tikhub-key".to_owned();
    let account = env.split(false);
    let telegram = env.telegram();

    // Within the tries of an unreadable post nothing is said.
    let early = env.claimed(10, JobType::Link, LINK_MAX_ATTEMPTS - 1).await;
    telegram
        .process_job(&account, env.client(), &early, "owner")
        .await
        .expect("processed");
    assert_eq!(env.status(10).await.0, "pending");
    assert!(env.notifier.sent().is_empty());

    let skipped = env.claimed(11, JobType::Link, LINK_MAX_ATTEMPTS + 2).await;
    telegram
        .process_job(&account, env.client(), &skipped, "owner")
        .await
        .expect("processed");
    assert_eq!(env.status(11).await.0, "pending");
    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].dedupe_key, LINK_OUTAGE_KEY);
}

/// Waiting is for outages alone: a post that cannot be read is given up on any try from
/// the fifth, also one that got there by waiting.
#[tokio::test]
async fn link_past_its_fifth_try_whose_post_cannot_be_read_is_given_up() {
    let mut env = Env::new(vec![text(10, ZHIHU_SHARE)]).await;
    env.web = Some(zhihu_web(None).await);
    env.settings.services.tikhub.token = "tikhub-key".to_owned();
    let job = env.claimed(10, JobType::Link, LINK_MAX_ATTEMPTS + 4).await;

    env.telegram()
        .process_job(&env.split(false), env.client(), &job, "owner")
        .await
        .expect("processed");

    assert_eq!(env.status(10).await.0, "discarded");
    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].dedupe_key, "");
}

#[tokio::test]
async fn zhihu_link_saved_while_an_outage_is_reported_closes_the_report() {
    let mut env = Env::new(vec![text(10, ZHIHU_SHARE), text(11, ZHIHU_SHARE), text(12, ZHIHU_SHARE)]).await;
    env.settings.services.tikhub.token = "tikhub-key".to_owned();
    let account = env.split(false);
    let outage_keys = |env: &Env| -> usize { env.notifier.resolved().iter().filter(|(key, _)| key == LINK_OUTAGE_KEY).count() };

    env.web = Some(zhihu_outage_web().await);
    let waiting = env.claimed(10, JobType::Link, LINK_MAX_ATTEMPTS).await;
    env.telegram()
        .process_job(&account, env.client(), &waiting, "owner")
        .await
        .expect("processed");
    assert_eq!(env.notifier.sent().len(), 1);

    // TikHub answers again: whichever link is saved first closes the report, however few tries it had.
    let images = ["https://pica.zhimg.com/v2-a_r.jpg"];
    env.web = Some(zhihu_web(Some(zhihu_answer("Q", &images, &[]))).await);
    let telegram = env.telegram();
    let first = env.claimed(11, JobType::Link, 1).await;
    assert!(
        telegram
            .process_job(&account, env.client(), &first, "owner")
            .await
            .expect("processed")
    );
    assert_eq!(env.status(11).await.0, "completed");
    assert_eq!(outage_keys(&env), 1);
    let resolved = env.notifier.resolved();
    let (_, resolution) = resolved.iter().find(|(key, _)| key == LINK_OUTAGE_KEY).expect("closed");
    assert_eq!((resolution.kind.as_str(), resolution.title.as_str()), ("job_recovered", "Q"));

    // With no report open there is nothing to close.
    let second = env.claimed(12, JobType::Link, LINK_MAX_ATTEMPTS + 1).await;
    assert!(
        telegram
            .process_job(&account, env.client(), &second, "owner")
            .await
            .expect("processed")
    );
    assert_eq!(outage_keys(&env), 1);
}

// py: tests/test_telegram.py::test_link_waiting_on_a_setting_is_never_given_up
#[tokio::test]
async fn link_waiting_on_a_setting_is_never_given_up() {
    let mut env = Env::new(vec![text(10, ZHIHU_SHARE)]).await;
    let web = zhihu_web(None).await;
    env.web = Some(web.clone());
    // No TikHub key.
    let job = env.claimed(10, JobType::Link, LINK_MAX_ATTEMPTS + 3).await;

    env.telegram()
        .process_job(&env.split(false), env.client(), &job, "owner")
        .await
        .expect("processed");

    let (status, error) = env.status(10).await;
    assert_eq!(status, "pending");
    assert!(error.starts_with("PostLinkError: "), "{error}");
    let due = env.due_in(10).await;
    assert!((LINK_WAIT_SECONDS - 1.0..=LINK_WAIT_SECONDS).contains(&due), "{due}");
    let sent = env.notifier.sent();
    assert_eq!(sent[0].dedupe_key, "telegram:links:tikhub");
    assert!(web.urls().is_empty());
}

// ---------- unsupported messages ----------

// py: tests/test_telegram.py::test_messages_with_nothing_to_save_are_queued_to_be_reported
#[test]
fn messages_with_nothing_to_save_are_queued_to_be_reported() {
    let messages = vec![
        text(1, "https://example.com/post/1"),
        text(2, "just a note"),
        document(
            3,
            "application/octet-stream",
            vec![DocumentAttribute::Filename("clip.mkv".to_owned())],
        ),
        TgMessage {
            action: true,
            ..TgMessage::new(4, CHANNEL)
        },
        text(5, ZHIHU_SHARE),
        photo(6),
    ];

    let scan = build_scan(&messages, &[JobType::Video, JobType::Link, JobType::Unsupported]).expect("scan");

    // A picture in a channel with no image route stays unsaved on purpose, and a service message has nothing.
    let kinds: Vec<(i64, JobType)> = scan.media.iter().map(|entry| (entry.msg.id, entry.media_type)).collect();
    assert_eq!(
        kinds,
        [
            (1, JobType::Unsupported),
            (2, JobType::Unsupported),
            (3, JobType::Unsupported),
            (5, JobType::Link),
        ]
    );
    assert_eq!(unsupported_reason(&messages[0]), "Link not supported: https://example.com/post/1");
    assert_eq!(unsupported_reason(&messages[1]), "Nothing to download: a text message: just a note");
    assert_eq!(unsupported_reason(&messages[2]), "Nothing to download: a file (clip.mkv)");
}

// py: tests/test_telegram.py::test_unsupported_job_is_reported_and_completed
#[tokio::test]
async fn unsupported_job_is_reported_and_completed() {
    let env = Env::new(vec![text(10, "see https://example.com/x")]).await;
    let job = env.claimed(10, JobType::Unsupported, 1).await;

    let downloaded = env
        .telegram()
        .process_job(&env.split(true), env.client(), &job, "owner")
        .await
        .expect("processed");

    assert!(!downloaded);
    assert_eq!(env.status(10).await.0, "completed");
    let sent = env.notifier.sent();
    assert_eq!(sent[0].kind, "unsupported_message");
    assert_eq!(sent[0].dedupe_key, "telegram:unsupported:default:123:10");
    assert_eq!(sent[0].body, "Link not supported: https://example.com/x");
    assert_eq!(sent[0].link_url, "https://t.me/c/123/10");
    assert_eq!(sent[0].title, "demo_channel #10");
}

// py: tests/test_telegram.py::test_only_channels_that_report_unsupported_scan_for_it
#[tokio::test]
async fn only_channels_that_report_unsupported_scan_for_it() {
    let env = Env::new(vec![text(10, "just a note")]).await;
    let listened = &owned_routes(&env.split(false))[0].1;
    let inbox = &owned_routes(&env.split(true))[0].1;

    assert_eq!(job_types(listened), [JobType::Image, JobType::Video, JobType::Link]);
    assert_eq!(
        job_types(inbox),
        [JobType::Image, JobType::Video, JobType::Link, JobType::Unsupported]
    );

    // A job queued while the channel reported them is dropped once it stops.
    let job = env.claimed(10, JobType::Unsupported, 1).await;
    env.telegram()
        .process_job(&env.split(false), env.client(), &job, "owner")
        .await
        .expect("processed");
    assert!(env.notifier.sent().is_empty());
    assert_eq!(
        env.status(10).await,
        ("discarded".to_owned(), "Configured media is no longer available".to_owned())
    );
}

// py: tests/test_telegram.py::test_a_running_listener_follows_channel_edits
#[tokio::test]
async fn a_running_listener_follows_channel_edits() {
    let mut env = Env::new(vec![text(10, "just a note")]).await;
    let started_with = env.split(false);
    env.settings.web.telegram.accounts = vec![env.split(true)];
    let job = env.claimed(10, JobType::Unsupported, 1).await;

    // The listener still holds the account it connected with; the routes come from the settings.
    env.telegram()
        .process_job(&started_with, env.client(), &job, "owner")
        .await
        .expect("processed");

    assert_eq!(env.status(10).await.0, "completed");
    assert_eq!(env.notifier.sent()[0].body, "Nothing to download: a text message: just a note");
}

// py: tests/test_telegram.py::test_reported_messages_are_queued_again_once_they_can_be_saved
#[tokio::test]
async fn reported_messages_are_queued_again_once_they_can_be_saved() {
    let env = Env::new(vec![text(10, ZHIHU_SHARE), text(12, "still just a note")]).await;
    for id in [10, 11, 12] {
        queue::enqueue(
            env.db(),
            "default",
            CHANNEL,
            id,
            None,
            JobType::Unsupported,
            "Note",
            QueueSource::Reconciliation,
            0.0,
        )
        .await
        .expect("enqueue");
    }
    sqlx::query("UPDATE telegram_media_queue SET status = 'completed'")
        .execute(env.db().pool())
        .await
        .expect("reported");
    let types = [JobType::Image, JobType::Video, JobType::Link, JobType::Unsupported];

    env.telegram()
        .recheck_unsupported(&env.split(true), CHANNEL, env.client(), "fav", &types)
        .await
        .expect("rechecked");

    assert_eq!(*env.client().lookups.lock().expect("lock"), [vec![10, 11, 12]]);
    assert_eq!(
        env.queued().await[0],
        (
            10,
            "高跟鞋配白丝真的很难看吗？ - 知乎用户的回答 - 知乎".to_owned(),
            "reconciliation".to_owned(),
            "link".to_owned()
        )
    );
    assert_eq!(env.status(10).await.0, "pending");
    assert_eq!(env.status(11).await, ("discarded".to_owned(), "Message was deleted".to_owned()));
    assert_eq!(env.status(12).await.0, "completed");
}

// The rest of the module: what Telethon itself does for the source.

/// An `RpcError` without a wait reads as Telethon raises it.
#[test]
fn errors_name_their_kind() {
    let invalid = TgError::rpc(400, "MESSAGE_ID_INVALID", None, Some("GetMessagesRequest"));
    assert_eq!(invalid.kind, ErrorKind::MessageIdInvalid);
    assert_eq!(invalid.class, "MessageIdInvalidError");
}
