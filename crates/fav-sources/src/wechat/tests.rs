//! The Python tests of the WeChat source (`tests/test_wechat.py`), less the article, Tieba
//! and NGA parsing tests WP13 ported in `post_links_tests.rs`.
//!
//! Python fakes the database; these runs use a temporary PostgreSQL database and read the
//! rows back. The `FavInbox` service and every site are one scripted fake web: the service at
//! `http://favinbox.test`, the sites at their own addresses, `https://` ones through the
//! fake web's own TLS.

#![allow(
    clippy::format_collect,
    clippy::case_sensitive_file_extension_comparisons,
    clippy::needless_pass_by_value
)] // Fixtures are built by formatting and passed by value; names are ours.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aes::Aes128;
use aes::cipher::{Array, BlockCipherEncrypt as _, KeyInit as _};
use fav_core::settings::Settings;
use fav_core::validate::{Model as _, PyPath};
use fav_db::testdb::TestDatabase;
use serde_json::json;

use super::*;
use crate::post_links::Endpoints;
use crate::testkit::RecordingNotifier;
use crate::testutil::{FakeWeb, Reply, Seen, test_roots};
use crate::wechat_article::{image_extension, original_image_url, parse_article_link};
use crate::wechat_channels::{ENCRYPTED_BYTES, best_quality, decrypt_head, keystream};
use crate::wechat_emoji::md5_hex;

type Json = serde_json::Value;

/// 2025-10-05 03:13:20 UTC, in milliseconds as WeChat stores it.
const CREATE_TIME: i64 = 1_759_634_000_000;
const JPEG: &[u8] = b"\xff\xd8\xff\xe0original-bytes";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfull-size";
const INBOX: &str = "http://favinbox.test";
const INBOX_HOST: &str = "favinbox.test";

// ---------- fixtures ----------

fn value(json: &Json) -> Value {
    Value::from_serde(json)
}

fn object(json: &Json) -> Dict {
    match value(json) {
        Value::Dict(dict) => dict,
        other => panic!("expected an object, got {other:?}"),
    }
}

fn merge(mut base: Json, updates: Json) -> Json {
    if let (Json::Object(base), Json::Object(updates)) = (&mut base, updates) {
        for (key, value) in updates {
            base.insert(key, value);
        }
    }
    base
}

/// `_item`: an image the inbox lists.
fn inbox_item(msg_id: i64, updates: Json) -> Json {
    merge(
        json!({
            "msg_id": msg_id,
            "create_time": CREATE_TIME + msg_id,
            "type": "image",
            "talker": "wxid_sender",
            "sender": "wxid_sender",
            "md5": "wechat-md5",
            "file": format!("{msg_id}.jpg"),
            "status": "ok",
            "has_file": true,
        }),
        updates,
    )
}

/// `_message`: an image message as the bridge logs it.
fn message(msg_id: i64, updates: Json) -> Json {
    merge(
        json!({
            "msg_id": msg_id,
            "type": 3,
            "kind": "image",
            "chat": "private",
            "create_time": CREATE_TIME + msg_id,
            "talker": "wxid_sender",
            "sender": "wxid_sender",
            "is_send": false,
            "content": "<msg><img aeskey=\"secret-aes\" cdnbigimgurl=\"cdn-url\" /></msg>",
            "image": {"md5": "wechat-md5", "key": "secret-aes", "mid_img_url": "cdn-url"},
        }),
        updates,
    )
}

fn event_of(seq: i64, message: Json, kind: &str) -> Json {
    json!({"seq": seq, "kind": kind, "at": CREATE_TIME, "data": message})
}

fn event(seq: i64, message: Json) -> Json {
    event_of(seq, message, "message")
}

fn work(event: &Json) -> Option<&'static str> {
    work_for(&value(event)).expect("work_for")
}

const SHARE_URL: &str = "http://mp.weixin.qq.com/s?__biz=MzA5==&amp;mid=2247&amp;idx=1&amp;sn=abc123&amp;chksm=feed#rd";
const FETCH_URL: &str = "https://mp.weixin.qq.com/s?__biz=MzA5==&mid=2247&idx=1&sn=abc123&chksm=feed";
const CANONICAL_URL: &str = "https://mp.weixin.qq.com/s?__biz=MzA5==&mid=2247&idx=1&sn=abc123";
const IMAGE_A: &str = "https://mmbiz.qpic.cn/sz_mmbiz_jpg/AAA/640?wx_fmt=jpeg&from=appmsg";
const IMAGE_B: &str = "https://mmbiz.qpic.cn/sz_mmbiz_png/BBB/640?wx_fmt=png";

fn page() -> String {
    let escaped = IMAGE_A.replace('&', "&amp;");
    format!(
        "<html><head><meta property=\"og:title\" content=\"og title\" /></head>\n\
         <script>var msg_title = '每日Coser分享'.html(false); var nickname = htmlDecode(\"小叶菌ovo\"); var ct = \"1759600000\";</script>\n\
         <img class=\"avatar\" data-src=\"https://mmbiz.qpic.cn/mmbiz_png/AVATAR/0?wx_fmt=png\" />\n\
         <div class=\"rich_media_content\" id=\"js_content\">\n\
         <p><img class=\"rich_pages\" data-src=\"{escaped}\" /></p>\n\
         <p><img data-ratio=\"1\" data-src=\"{IMAGE_B}\" /></p>\n\
         <p><img data-src=\"{escaped}\" /></p>\n\
         </div></html>"
    )
}

/// `_share`: a link card.
fn share(url: &str, kind: &str) -> String {
    format!(
        "<msg><appmsg appid=\"\" sdkver=\"0\"><title>每日分享</title><des>今天的</des><action>view</action>\
         <type>{kind}</type><url>{url}</url><sourcedisplayname>小叶菌ovo</sourcedisplayname>\
         <refermsg><type>1</type></refermsg></appmsg><fromusername>wxid_sender</fromusername></msg>"
    )
}

fn article_message(msg_id: i64) -> Json {
    message(
        msg_id,
        json!({"type": 49, "kind": "app", "content": share(SHARE_URL, "5"), "image": null}),
    )
}

fn app_message(msg_id: i64, content: &str) -> Json {
    message(msg_id, json!({"type": 49, "kind": "app", "content": content, "image": null}))
}

fn original(url: &str) -> String {
    original_image_url(url).expect("an image URL")
}

fn image_reply(content: &[u8], content_type: &str) -> Reply {
    Reply::Respond {
        status: 200,
        headers: vec![("content-type".to_owned(), content_type.to_owned())],
        body: content.to_vec(),
    }
}

fn jpeg() -> Reply {
    image_reply(JPEG, "image/jpeg")
}

fn html(status: u16, text: &str) -> Reply {
    Reply::Respond {
        status,
        headers: vec![("content-type".to_owned(), "text/html; charset=utf-8".to_owned())],
        body: text.as_bytes().to_vec(),
    }
}

fn redirect(location: &str) -> Reply {
    Reply::Respond {
        status: 302,
        headers: vec![("location".to_owned(), location.to_owned())],
        body: Vec::new(),
    }
}

/// `_article_web`.
fn article_sites(overrides: Vec<(String, Reply)>) -> Sites {
    let mut responses = vec![
        (FETCH_URL.to_owned(), html(200, &page())),
        (original(IMAGE_A), jpeg()),
        (original(IMAGE_B), image_reply(PNG, "image/png")),
    ];
    responses.extend(overrides);
    Sites::with(responses)
}

// ---------- the fake service and sites ----------

/// `_FakeInbox`: the `FavInbox` HTTP API with the bridge behind `/wx/`.
#[derive(Default)]
struct Inbox {
    items: Vec<Json>,
    /// `None`: every listed item's file is a JPEG.
    files: Option<HashMap<i64, Vec<u8>>>,
    list_status: Option<u16>,
    events: Vec<Json>,
    last_seq: Option<i64>,
    events_status: Option<u16>,
    /// Replaces the logged-in `/wx/status` answer (D183).
    status: Option<Reply>,
    images: HashMap<i64, Vec<u8>>,
    post_page: Option<Reply>,
    link_open: Option<Reply>,
    contacts: HashMap<String, Json>,
    /// Replaces every answer to `/ack/`.
    ack: Option<Reply>,
    /// Replaces every answer to a path.
    overrides: HashMap<String, Reply>,
    acked: Vec<i64>,
}

impl Inbox {
    fn listing(items: Vec<Json>) -> Self {
        Self { items, ..Self::default() }
    }

    fn bridge(events: Vec<Json>) -> Self {
        Self { events, ..Self::default() }
    }

    fn param(request: &Seen, name: &str) -> String {
        url::form_urlencoded::parse(request.query().as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default()
    }

    fn answer(&mut self, request: &Seen) -> Reply {
        let path = request.path().to_owned();
        if let Some(reply) = self.overrides.get(&path) {
            return reply.clone();
        }
        if let Some(rest) = path.strip_prefix("/wx/") {
            return self.bridge_answer(request, rest);
        }
        if path == "/link/open" && request.method == "POST" {
            return self
                .link_open
                .clone()
                .unwrap_or_else(|| Reply::json(200, &json!({"loaded": true, "seconds": 7.2})));
        }
        if path == "/post/page" && request.method == "POST" {
            return self
                .post_page
                .clone()
                .unwrap_or_else(|| Reply::json(404, &json!({"error": "the post was not in the XWeb cache after 30s"})));
        }
        if path == "/items" {
            return Reply::json(self.list_status.unwrap_or(200), &Json::Array(self.items.clone()));
        }
        if let Some(id) = path.strip_prefix("/file/") {
            let msg_id: i64 = id.parse().expect("an id");
            let file = match &self.files {
                Some(files) => files.get(&msg_id).cloned(),
                None => self.items.iter().any(|item| item["msg_id"] == json!(msg_id)).then(|| JPEG.to_vec()),
            };
            return file.map_or_else(
                || Reply::json(404, &json!({"error": "not found"})),
                |file| image_reply(&file, "image/jpeg"),
            );
        }
        if let Some(id) = path.strip_prefix("/ack/")
            && request.method == "POST"
        {
            if let Some(reply) = &self.ack {
                return reply.clone();
            }
            let msg_id: i64 = id.parse().expect("an id");
            if !self.items.iter().any(|item| item["msg_id"] == json!(msg_id)) {
                return Reply::json(404, &json!({"error": "unknown msg_id"}));
            }
            self.acked.push(msg_id);
            return Reply::json(200, &json!({"ok": true}));
        }
        Reply::status(404)
    }

    fn bridge_answer(&mut self, request: &Seen, path: &str) -> Reply {
        match path {
            "events" => {
                if let Some(status) = self.events_status {
                    return Reply::json(status, &json!({"error": "bridge unreachable: connection refused"}));
                }
                let after: i64 = Self::param(request, "after").parse().expect("after");
                let limit: usize = Self::param(request, "limit").parse().expect("limit");
                let last_seq = self
                    .last_seq
                    .unwrap_or_else(|| self.events.iter().filter_map(|event| event["seq"].as_i64()).max().unwrap_or(0));
                let page: Vec<Json> = self
                    .events
                    .iter()
                    .filter(|event| event["seq"].as_i64().is_some_and(|seq| seq > after))
                    .take(limit)
                    .cloned()
                    .collect();
                Reply::json(200, &json!({"ok": true, "last_seq": last_seq, "events": page}))
            }
            "status" => self
                .status
                .clone()
                .unwrap_or_else(|| Reply::json(200, &json!({"ok": true, "version": "0.4.12", "login_wxid": "wxid_fav"}))),
            "image" => {
                let msg_id: i64 = Self::param(request, "msg_id").parse().expect("msg_id");
                self.images.get(&msg_id).map_or_else(
                    || Reply::json(500, &json!({"error": "download failed"})),
                    |image| image_reply(image, "application/octet-stream"),
                )
            }
            "contact" => {
                let wxid = Self::param(request, "wxid");
                match self.contacts.get(&wxid) {
                    Some(contact) => Reply::json(200, &merge(json!({"ok": true, "wxid": wxid}), contact.clone())),
                    None => Reply::json(500, &json!({"error": "no such contact"})),
                }
            }
            _ => Reply::status(404),
        }
    }
}

const TIEBA_API: &str = "http://c.tieba.baidu.com/c/f/pb/page";
const NGA_READ: &str = "https://bbs.nga.cn/read.php?tid=47695975&__output=11";

/// `_FakeWeb`, with `_TiebaWeb`'s client API and `_NgaWeb`'s `read.php` in front of it.
#[derive(Default)]
struct Sites {
    responses: HashMap<String, Reply>,
    tieba: Option<HashMap<String, Json>>,
    tieba_forms: Vec<HashMap<String, String>>,
    nga: Option<Json>,
    nga_cookies: Vec<String>,
}

impl Sites {
    fn with(responses: Vec<(String, Reply)>) -> Self {
        Self {
            responses: responses.into_iter().collect(),
            ..Self::default()
        }
    }

    fn answer(&mut self, request: &Seen) -> Reply {
        if request.method == "POST"
            && request.url == TIEBA_API
            && let Some(pages) = &self.tieba
        {
            let form: HashMap<String, String> = url::form_urlencoded::parse(&request.body).into_owned().collect();
            let page = pages[&form["pn"]].clone();
            self.tieba_forms.push(form);
            return Reply::json(200, &page);
        }
        if request.url == NGA_READ
            && let Some(thread) = &self.nga
        {
            let cookie = request.header("cookie").unwrap_or_default().to_owned();
            let signed_in = cookie.contains("ngaPassportCid=session-cid");
            self.nga_cookies.push(cookie);
            if !signed_in {
                return Reply::json(
                    403,
                    &json!({"error": ["1:未登录", "<br/><a href=/nuke.php>[登录]</a>"], "data": {"__MESSAGE": {}}}),
                );
            }
            return Reply::json(200, thread);
        }
        self.responses.get(&request.url).cloned().unwrap_or_else(|| Reply::status(404))
    }
}

/// `site_sessions` with an optional NGA login, and the checks recorded.
struct Sessions {
    login: Option<Vec<(&'static str, &'static str)>>,
    checks: Mutex<Vec<(bool, String)>>,
}

impl SessionStore for Sessions {
    fn load<'a>(&'a self, site: &'a str) -> BoxFuture<'a, Result<Option<fav_runtime::site_sessions::SiteSession>, fav_runtime::Error>> {
        assert_eq!(site, "nga");
        Box::pin(async move {
            Ok(self.login.as_ref().map(|login| fav_runtime::site_sessions::SiteSession {
                site: site.to_owned(),
                cookies: login
                    .iter()
                    .map(|(name, value)| fav_runtime::site_sessions::StoredCookie {
                        name: (*name).to_owned(),
                        value: (*value).to_owned(),
                        domain: ".nga.cn".to_owned(),
                        path: "/".to_owned(),
                        expires: None,
                    })
                    .collect(),
                account: "42".to_owned(),
                status: "valid".to_owned(),
                message: String::new(),
                expires_at: None,
                signed_in_at: None,
                checked_at: None,
                extra: Vec::new(),
            }))
        })
    }

    fn record_check<'a>(&'a self, site: &'a str, ok: bool, message: &'a str) -> BoxFuture<'a, Result<(), fav_runtime::Error>> {
        assert_eq!(site, "nga");
        self.checks.lock().expect("lock").push((ok, message.to_owned()));
        Box::pin(async { Ok(()) })
    }
}

const NGA_LOGIN: [(&str, &str); 2] = [("ngaPassportUid", "42"), ("ngaPassportCid", "session-cid")];

/// One test's world: a database, the fake web, the collection folder and the settings.
struct Env {
    database: TestDatabase,
    web: FakeWeb,
    inbox: Arc<Mutex<Inbox>>,
    sites: Arc<Mutex<Sites>>,
    notifier: Arc<RecordingNotifier>,
    dir: tempfile::TempDir,
    settings: Settings,
    sessions: Arc<Sessions>,
}

impl Env {
    async fn new(inbox: Inbox, sites: Sites) -> Self {
        let inbox = Arc::new(Mutex::new(inbox));
        let sites = Arc::new(Mutex::new(sites));
        let (to_inbox, to_sites) = (Arc::clone(&inbox), Arc::clone(&sites));
        let web = FakeWeb::start(move |request| {
            if request.host() == INBOX_HOST {
                to_inbox.lock().expect("lock").answer(request)
            } else {
                to_sites.lock().expect("lock").answer(request)
            }
        })
        .await;
        let dir = tempfile::tempdir().expect("temp dir");
        let mut settings = fav_db::settings_store::default_settings();
        settings.web.wechat.path = PyPath::new(dir.path().to_str().expect("UTF-8"));
        settings.web.wechat.token = "secret".to_owned();
        settings.web.wechat.api_url = INBOX.to_owned();
        Self {
            database: crate::testutil::database().await,
            web,
            inbox,
            sites,
            notifier: Arc::new(RecordingNotifier::default()),
            dir,
            settings,
            sessions: Arc::new(Sessions {
                login: Some(NGA_LOGIN.to_vec()),
                checks: Mutex::new(Vec::new()),
            }),
        }
    }

    fn tikhub(&mut self, token: &str) {
        self.settings.services.tikhub.token = token.to_owned();
    }

    fn source(&self) -> Wechat {
        let roots = test_roots();
        let cfg = self.settings.web.wechat.clone();
        let clients = Clients::new(&cfg.token, Some(&self.web.proxy), &roots).expect("clients");
        let store = Arc::new(SettingsStore::pinned(self.settings.clone()));
        let post_client = PostClient::with_roots(Some(&self.web.proxy), Endpoints::default(), &roots).expect("client");
        let sessions: Arc<dyn SessionStore> = Arc::clone(&self.sessions) as Arc<dyn SessionStore>;
        let web = PostLinks::new(post_client, Arc::clone(&store), sessions);
        let notifier: SharedNotifier = Arc::clone(&self.notifier) as SharedNotifier;
        Wechat::new(self.database.db().clone(), notifier, store, cfg, clients, web)
    }

    async fn run(&self) -> Result<(), JobError> {
        self.source().update().await
    }

    async fn run_ok(&self) {
        if let Err(error) = self.run().await {
            panic!("the run failed: {}: {}", error.class_name(), error.message());
        }
    }

    async fn run_err(&self) -> JobError {
        self.run().await.expect_err("the run fails")
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn inbox(&self) -> std::sync::MutexGuard<'_, Inbox> {
        self.inbox.lock().expect("lock")
    }

    fn sites(&self) -> std::sync::MutexGuard<'_, Sites> {
        self.sites.lock().expect("lock")
    }

    fn inbox_requests(&self) -> Vec<Seen> {
        self.web.seen().into_iter().filter(|seen| seen.host() == INBOX_HOST).collect()
    }

    fn web_requests(&self) -> Vec<Seen> {
        self.web.seen().into_iter().filter(|seen| seen.host() != INBOX_HOST).collect()
    }

    fn web_urls(&self) -> Vec<String> {
        self.web_requests().into_iter().map(|seen| seen.url).collect()
    }

    /// `inbox.event_requests()`.
    fn event_requests(&self) -> Vec<i64> {
        self.inbox_requests()
            .iter()
            .filter(|seen| seen.path() == "/wx/events")
            .map(|seen| Inbox::param(seen, "after").parse().expect("after"))
            .collect()
    }

    async fn execute(&self, sql: String) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(self.database.db().pool())
            .await
            .expect("statement");
    }

    /// `fake_db.inserts('wechat_inbox')`: `(msg_id, create_time, local_path, size, metadata)`, with -1 and
    /// `None` for an unset size and metadata.
    async fn images(&self) -> Vec<(i64, i64, String, i64, Value)> {
        sqlx::query("SELECT msg_id, create_time, local_path, COALESCE(size, -1) AS size, COALESCE(metadata, 'null')::text AS metadata FROM wechat_inbox ORDER BY created_at, msg_id")
            .fetch_all(self.database.db().pool())
            .await
            .expect("rows")
            .into_iter()
            .map(|row| {
                (
                    row.get("msg_id"),
                    row.get("create_time"),
                    row.get("local_path"),
                    row.get("size"),
                    pyjson::loads(&row.get::<String, _>("metadata")).expect("JSON"),
                )
            })
            .collect()
    }

    async fn image_ids(&self) -> Vec<i64> {
        self.images().await.into_iter().map(|row| row.0).collect()
    }

    async fn articles(&self) -> Vec<ArticleRow> {
        sqlx::query(
            "SELECT msg_id, create_time, url, title, account, published_at, image_count, local_path, metadata::text AS metadata
             FROM wechat_article ORDER BY created_at, msg_id",
        )
        .fetch_all(self.database.db().pool())
        .await
        .expect("rows")
        .into_iter()
        .map(|row| ArticleRow {
            msg_id: row.get("msg_id"),
            create_time: row.get("create_time"),
            url: row.get("url"),
            title: row.get("title"),
            account: row.get("account"),
            published_at: row.get("published_at"),
            image_count: row.get("image_count"),
            local_path: row.get("local_path"),
            metadata: pyjson::loads(&row.get::<String, _>("metadata")).expect("JSON"),
        })
        .collect()
    }

    async fn article(&self) -> ArticleRow {
        let mut rows = self.articles().await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        rows.remove(0)
    }

    async fn videos(&self) -> Vec<VideoRow> {
        sqlx::query(
            "SELECT object_id, title, author, duration, size, local_path, metadata::text AS metadata
             FROM wechat_channel_video ORDER BY created_at, msg_id",
        )
        .fetch_all(self.database.db().pool())
        .await
        .expect("rows")
        .into_iter()
        .map(|row| VideoRow {
            object_id: row.get("object_id"),
            title: row.get("title"),
            author: row.get("author"),
            duration: row.get("duration"),
            size: row.get("size"),
            local_path: row.get("local_path"),
            metadata: pyjson::loads(&row.get::<String, _>("metadata")).expect("JSON"),
        })
        .collect()
    }

    async fn queued(&self, msg_id: i64) -> Option<Queued> {
        sqlx::query("SELECT work, status, attempts, last_error FROM wechat_events WHERE msg_id = $1 AND create_time = $2")
            .bind(msg_id)
            .bind(CREATE_TIME + msg_id)
            .fetch_optional(self.database.db().pool())
            .await
            .expect("row")
            .map(|row| Queued {
                work: row.get("work"),
                status: row.get("status"),
                attempts: row.get("attempts"),
                last_error: row.get("last_error"),
            })
    }

    async fn queue(&self, msg_id: i64) -> Queued {
        self.queued(msg_id).await.expect("a queued row")
    }

    async fn status(&self, msg_id: i64) -> String {
        self.queue(msg_id).await.status
    }

    async fn state(&self) -> HashMap<String, String> {
        sqlx::query("SELECT key, value FROM wechat_state")
            .fetch_all(self.database.db().pool())
            .await
            .expect("rows")
            .into_iter()
            .map(|row| (row.get("key"), row.get("value")))
            .collect()
    }

    async fn set_state(&self, key: &str, value: &str) {
        sqlx::query("INSERT INTO wechat_state (key, value) VALUES ($1, $2)")
            .bind(key)
            .bind(value)
            .execute(self.database.db().pool())
            .await
            .expect("state");
    }

    /// A row in the queue as an earlier run left it.
    async fn enqueue(&self, msg_id: i64, work: &str, seq: i64, message: &Json, status: &str) {
        sqlx::query("INSERT INTO wechat_events (msg_id, create_time, work, seq, message, status) VALUES ($1, $2, $3, $4, $5::jsonb, $6)")
            .bind(msg_id)
            .bind(CREATE_TIME + msg_id)
            .bind(work)
            .bind(seq)
            .bind(message.to_string())
            .bind(status)
            .execute(self.database.db().pool())
            .await
            .expect("queued");
    }

    async fn set_attempts(&self, msg_id: i64, attempts: i32) {
        sqlx::query("UPDATE wechat_events SET attempts = $1 WHERE msg_id = $2")
            .bind(attempts)
            .bind(msg_id)
            .execute(self.database.db().pool())
            .await
            .expect("update");
    }

    /// Every file under the collection, relative to it.
    fn files(&self) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    out.push(path.strip_prefix(root).expect("inside").to_string_lossy().into_owned());
                }
            }
        }
        let mut out = Vec::new();
        walk(self.path(), self.path(), &mut out);
        out.sort();
        out
    }

    /// Everything under the collection, folders included.
    fn entries(&self) -> usize {
        fn count(dir: &Path) -> usize {
            std::fs::read_dir(dir).map_or(0, |entries| {
                entries
                    .flatten()
                    .map(|entry| 1 + if entry.path().is_dir() { count(&entry.path()) } else { 0 })
                    .sum()
            })
        }
        count(self.path())
    }

    async fn finish(self) {
        self.database.drop_database().await;
    }
}

#[derive(Debug)]
struct ArticleRow {
    msg_id: i64,
    create_time: i64,
    url: String,
    title: String,
    account: String,
    published_at: String,
    image_count: i32,
    local_path: String,
    metadata: Value,
}

#[derive(Debug)]
struct VideoRow {
    object_id: String,
    title: String,
    author: String,
    duration: i32,
    size: i64,
    local_path: String,
    metadata: Value,
}

#[derive(Debug)]
struct Queued {
    work: String,
    status: String,
    attempts: i32,
    last_error: String,
}

fn names(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .expect("a folder")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn payload<'a>(notification: &'a NewNotification, key: &str) -> &'a Value {
    notification
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .unwrap_or_else(|| panic!("no {key} in the payload"))
}

fn json_file(path: &Path) -> Value {
    pyjson::loads(&std::fs::read_to_string(path).expect("a file")).expect("JSON")
}

// ---------- configuration ----------

// py: tests/test_wechat.py::test_a_source_without_a_token_is_not_runnable
#[test]
fn a_source_without_a_token_is_not_runnable() {
    assert_eq!(fav_db::settings_store::default_settings().web.wechat.validate_runnable(), ["token"]);
}

// py: tests/test_wechat.py::test_a_configured_source_is_runnable_and_its_url_is_normalised
#[test]
fn a_configured_source_is_runnable_and_its_url_is_normalised() {
    let cfg = WechatConfig::validate(&value(
        &json!({"api_url": " http://favinbox-oracle.proxy.svc/ ", "token": " secret "}),
    ))
    .expect("valid settings");

    assert!(cfg.validate_runnable().is_empty());
    assert_eq!(cfg.api_url, "http://favinbox-oracle.proxy.svc");
    assert_eq!(cfg.token, "secret");
}

// ---------- selection and naming ----------

// py: tests/test_wechat.py::test_only_saved_images_with_a_usable_key_are_downloaded_oldest_first
#[test]
fn only_saved_images_with_a_usable_key_are_downloaded_oldest_first() {
    let items = [
        inbox_item(2, json!({"create_time": CREATE_TIME + 50})),
        inbox_item(3, json!({"create_time": CREATE_TIME})),
        inbox_item(4, json!({"type": "video"})),
        inbox_item(5, json!({"has_file": false})),
        inbox_item(6, json!({"status": "error: download failed"})),
        inbox_item(7, json!({"msg_id": "7"})),
        json!("not an item"),
    ];
    let items: Vec<Value> = items.iter().map(value).collect();

    let ids: Vec<Value> = pending_images(&items)
        .iter()
        .map(|item| item.get("msg_id").cloned().expect("an id"))
        .collect();
    assert_eq!(ids, [Value::from(3_i64), Value::from(2_i64)]);
}

// py: tests/test_wechat.py::test_a_backfilled_image_counts_as_saved
#[test]
fn a_backfilled_image_counts_as_saved() {
    assert!(!pending_images(&[value(&inbox_item(1, json!({"status": "ok:backfill"})))]).is_empty());
}

// py: tests/test_wechat.py::test_the_extension_comes_from_the_plugin_and_falls_back_to_the_response
#[test]
fn the_extension_comes_from_the_plugin_and_falls_back_to_the_response() {
    let item = |file: &str| object(&inbox_item(101, json!({"file": file})));

    assert_eq!(file_extension(&item("1.png"), "image/jpeg"), "png");
    assert_eq!(file_extension(&item("1.wxgf"), ""), "wxgf");
    assert_eq!(file_extension(&item(""), "image/webp; charset=binary"), "webp");
    assert_eq!(file_extension(&item("1.exe"), "application/octet-stream"), "bin");
}

// py: tests/test_wechat.py::test_files_are_grouped_by_month_and_named_by_send_time_and_id
#[test]
fn files_are_grouped_by_month_and_named_by_send_time_and_id() {
    let item = object(&inbox_item(42, json!({"create_time": CREATE_TIME})));

    assert_eq!(relative_path(&item, "jpg").expect("a path"), "2025-10/2025-10-05 031320 [42].jpg");
}

// ---------- runs ----------

// py: tests/test_wechat.py::test_a_run_stores_records_and_then_acknowledges_each_image
#[tokio::test]
async fn a_run_stores_records_and_then_acknowledges_each_image() {
    let env = Env::new(
        Inbox::listing(vec![inbox_item(1, json!({})), inbox_item(2, json!({}))]),
        Sites::default(),
    )
    .await;

    env.run_ok().await;

    let rows = env.images().await;
    assert_eq!(rows.iter().map(|row| row.0).collect::<Vec<_>>(), [1, 2]);
    let local_path = &rows[0].2;
    assert_eq!(std::fs::read(env.path().join(local_path)).expect("saved"), JPEG);
    assert_eq!(rows[0].3, i64::try_from(JPEG.len()).expect("small"));
    assert_eq!(rows[0].4.get("md5"), Some(&Value::from("wechat-md5")));
    assert_eq!(env.inbox().acked, [1, 2]);
    let headers: Vec<String> = env
        .inbox_requests()
        .iter()
        .map(|seen| seen.header("authorization").unwrap_or_default().to_owned())
        .collect();
    assert!(headers.iter().all(|header| header == "Bearer secret"), "{headers:?}");
    assert!(!env.files().iter().any(|file| file.ends_with(".part")));
    // One photo notification per image, pointing at the stored original.
    let sent = env.notifier.sent();
    assert_eq!(
        sent.iter().map(|n| n.kind.as_str()).collect::<Vec<_>>(),
        ["download_completed", "download_completed"]
    );
    assert_eq!(sent[0].title, file_name(Path::new(local_path)));
    assert_eq!(
        payload(&sent[0], "image_path"),
        &Value::Str(path_text(&env.path().join(local_path)))
    );
    assert_eq!(payload(&sent[0], "msg_id"), &Value::from(1_i64));
    env.finish().await;
}

// py: tests/test_wechat.py::test_an_image_stored_by_an_earlier_run_is_only_acknowledged
#[tokio::test]
async fn an_image_stored_by_an_earlier_run_is_only_acknowledged() {
    let env = Env::new(Inbox::listing(vec![inbox_item(1, json!({}))]), Sites::default()).await;
    env.execute(format!(
        "INSERT INTO wechat_inbox (msg_id, create_time) VALUES (1, {})",
        CREATE_TIME + 1
    ))
    .await;

    env.run_ok().await;

    assert_eq!(env.images().await.len(), 1);
    assert_eq!(env.inbox().acked, [1]);
    assert!(env.files().is_empty());
    assert!(env.notifier.sent().is_empty());
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_failed_download_is_left_pending_and_reported_after_the_rest
#[tokio::test]
async fn a_failed_download_is_left_pending_and_reported_after_the_rest() {
    let inbox = Inbox {
        files: Some(HashMap::from([(2, JPEG.to_vec())])),
        ..Inbox::listing(vec![inbox_item(1, json!({})), inbox_item(2, json!({}))])
    };
    let env = Env::new(inbox, Sites::default()).await;

    let error = env.run_err().await;

    assert_eq!(error.notification_dedupe_key(), Some("wechat:download"));
    assert!(error.message().contains('1'), "{}", error.message());
    assert_eq!(env.image_ids().await, [2]);
    assert_eq!(env.inbox().acked, [2]);
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_rejected_token_fails_the_run_with_a_stable_dedupe_key
#[tokio::test]
async fn a_rejected_token_fails_the_run_with_a_stable_dedupe_key() {
    let inbox = Inbox {
        list_status: Some(401),
        ..Inbox::default()
    };
    let mut env = Env::new(inbox, Sites::default()).await;
    env.settings.web.wechat.token = "wrong".to_owned();

    let error = env.run_err().await;

    assert_eq!(error.notification_dedupe_key(), Some("wechat:auth"));
    assert!(!error.message().contains("wrong"));
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_failed_ack_does_not_fail_the_run
#[tokio::test]
async fn a_failed_ack_does_not_fail_the_run() {
    let inbox = Inbox {
        ack: Some(Reply::status(500)),
        ..Inbox::listing(vec![inbox_item(1, json!({}))])
    };
    let env = Env::new(inbox, Sites::default()).await;

    env.run_ok().await;

    assert_eq!(env.image_ids().await, [1]);
    env.finish().await;
}

// py: tests/test_wechat.py::test_an_unconfigured_source_does_nothing
#[tokio::test]
async fn an_unconfigured_source_does_nothing() {
    let web = FakeWeb::start(|request| panic!("unexpected request {}", request.url)).await;
    let settings = fav_db::settings_store::default_settings();
    let store = Arc::new(SettingsStore::pinned(settings.clone()));
    let db = Db::connect_lazy("postgresql://tests@127.0.0.1:1/unused").expect("lazy pool");
    let clients = Clients::new("", Some(&web.proxy), &[]).expect("clients");
    let sessions: Arc<dyn SessionStore> = Arc::new(Sessions {
        login: None,
        checks: Mutex::new(Vec::new()),
    });
    let links = PostLinks::new(
        PostClient::with(Some(&web.proxy), Endpoints::default()).expect("client"),
        Arc::clone(&store),
        sessions,
    );
    let notifier: SharedNotifier = Arc::new(RecordingNotifier::default());
    let mut source = Wechat::new(db, notifier, store, settings.web.wechat.clone(), clients, links);

    source.update().await.expect("nothing to do");

    assert!(web.seen().is_empty());
}

// ---------- bridge events ----------

// py: tests/test_wechat.py::test_only_received_private_images_and_article_shares_are_work
#[test]
fn only_received_private_images_and_article_shares_are_work() {
    assert_eq!(work(&event(1, message(1, json!({})))), Some("image"));
    assert_eq!(work(&event(2, article_message(2))), Some("article"));
    assert_eq!(work(&event(3, message(3, json!({"is_send": true})))), None);
    assert_eq!(work(&event(4, message(4, json!({"chat": "group"})))), None);
    assert_eq!(work(&event(5, message(5, json!({"chat": "official"})))), None);
    // Anything else someone sent is reported, so it is not lost without a word.
    assert_eq!(
        work(&event(6, message(6, json!({"kind": "text", "content": "hello"})))),
        Some("unsupported")
    );
    assert_eq!(work(&event(7, app_message(7, &share(SHARE_URL, "6")))), Some("unsupported"));
    assert_eq!(
        work(&event(10, message(10, json!({"kind": "system", "content": "you added a friend"})))),
        None
    );
    assert_eq!(work(&event(8, message(8, json!({"create_time": null})))), None);
    assert_eq!(work(&event_of(9, json!({"wxid": "wxid_new"}), "new_friend")), None);
}

// py: tests/test_wechat.py::test_the_queue_keeps_no_image_key_or_cdn_address
#[test]
fn the_queue_keeps_no_image_key_or_cdn_address() {
    let kept = queued_message(&object(&message(1, json!({}))), "image");

    assert_eq!(kept.get("md5"), Some(&Value::from("wechat-md5")));
    let text = pyjson::dumps(&Value::Dict(kept), &DumpOptions::python_default()).expect("JSON");
    assert!(!text.contains("secret-aes"));
    assert!(!text.contains("cdn-url"));
    assert_eq!(
        queued_message(&object(&article_message(2)), "article").get("content"),
        Some(&Value::Str(share(SHARE_URL, "5")))
    );
}

// py: tests/test_wechat.py::test_a_bridge_image_is_saved_recorded_and_dropped_from_the_inbox
#[tokio::test]
async fn a_bridge_image_is_saved_recorded_and_dropped_from_the_inbox() {
    // The plugin saved its own copy too; the inbox drain finds it already stored.
    let inbox = Inbox {
        events: vec![event(7, message(45, json!({})))],
        images: HashMap::from([(45, PNG.to_vec())]),
        ..Inbox::listing(vec![inbox_item(45, json!({}))])
    };
    let env = Env::new(inbox, Sites::default()).await;

    env.run_ok().await;

    let rows = env.images().await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].2.ends_with(".png"), "{}", rows[0].2);
    assert_eq!(std::fs::read(env.path().join(&rows[0].2)).expect("saved"), PNG);
    assert_eq!(rows[0].4.get("md5"), Some(&Value::from("wechat-md5")));
    assert_eq!(env.inbox().acked, [45, 45]);
    let requests = env.inbox_requests();
    assert!(!requests.iter().any(|seen| seen.path().starts_with("/file/")));
    let image = requests.iter().find(|seen| seen.path() == "/wx/image").expect("asked the bridge");
    assert_eq!(
        image.query(),
        format!("talker=wxid_sender&msg_id=45&create_time={}", CREATE_TIME + 45)
    );
    let state = env.state().await;
    assert_eq!(state["events_after"], "7");
    assert_eq!(state["work_version"], "8");
    assert!(state.contains_key("unsupported_since"));
    assert_eq!(state.len(), 3);
    assert_eq!(env.status(45).await, "done");
    assert_eq!(
        env.notifier.sent().iter().map(|n| payload(n, "msg_id").clone()).collect::<Vec<_>>(),
        [Value::from(45_i64)]
    );
    env.finish().await;
}

// py: tests/test_wechat.py::test_notifications_name_the_sender_as_the_account_knows_them
#[tokio::test]
async fn notifications_name_the_sender_as_the_account_knows_them() {
    let inbox = Inbox {
        images: HashMap::from([(45, PNG.to_vec()), (46, PNG.to_vec())]),
        contacts: HashMap::from([(
            "wxid_sender".to_owned(),
            json!({"remark_name": "", "name": "Max", "nick_name": "max_nick"}),
        )]),
        ..Inbox::bridge(vec![event(1, message(45, json!({}))), event(2, message(46, json!({})))])
    };
    let env = Env::new(inbox, Sites::default()).await;

    env.run_ok().await;

    let sent = env.notifier.sent();
    assert_eq!(
        sent.iter().map(|n| n.body.as_str()).collect::<Vec<_>>(),
        ["From Max | Message ID 45", "From Max | Message ID 46"]
    );
    assert_eq!(
        sent.iter().map(|n| payload(n, "sender").clone()).collect::<Vec<_>>(),
        [Value::from("wxid_sender"), Value::from("wxid_sender")]
    );
    assert_eq!(env.inbox_requests().iter().filter(|seen| seen.path() == "/wx/contact").count(), 1);
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_remark_wins_over_the_nickname_in_notifications
#[tokio::test]
async fn a_remark_wins_over_the_nickname_in_notifications() {
    let inbox = Inbox {
        contacts: HashMap::from([(
            "wxid_sender".to_owned(),
            json!({"remark_name": "我自己", "name": "Max", "nick_name": "max_nick"}),
        )]),
        ..Inbox::bridge(vec![event(1, video_message(65))])
    };
    let mut env = Env::new(inbox, video_sites(None, None)).await;
    env.tikhub("tikhub-key");

    env.run_ok().await;

    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].body, "jiang57l | 9s | 0.0 MB | From 我自己");
    env.finish().await;
}

// py: tests/test_wechat.py::test_an_image_the_bridge_cannot_fetch_comes_from_the_inbox
#[tokio::test]
async fn an_image_the_bridge_cannot_fetch_comes_from_the_inbox() {
    let inbox = Inbox {
        events: vec![event(1, message(45, json!({})))],
        ..Inbox::listing(vec![inbox_item(45, json!({}))])
    };
    let env = Env::new(inbox, Sites::default()).await;

    env.run_ok().await;

    assert_eq!(env.image_ids().await, [45]);
    let files: Vec<String> = env
        .inbox_requests()
        .iter()
        .filter(|seen| seen.path().starts_with("/file/"))
        .map(|seen| seen.path().to_owned())
        .collect();
    assert_eq!(files, ["/file/45"]);
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_failing_item_is_retried_and_given_up_after_five_runs
#[tokio::test]
async fn a_failing_item_is_retried_and_given_up_after_five_runs() {
    let env = Env::new(Inbox::bridge(vec![event(1, message(45, json!({})))]), Sites::default()).await;

    let mut last = None;
    for run in 1..=5 {
        let error = env.run_err().await;
        assert_eq!(error.notification_dedupe_key(), Some("wechat:work"));
        assert_eq!(env.queue(45).await.attempts, run);
        last = Some(error);
    }
    assert!(last.expect("a failure").message().contains("gave up"));
    assert_eq!(env.status(45).await, "failed");

    // Given up: the next run leaves it alone and succeeds.
    env.run_ok().await;
    assert_eq!(env.queue(45).await.attempts, 5);
    env.finish().await;
}

// py: tests/test_wechat.py::test_the_cursor_moves_on_and_a_restarted_numbering_is_read_again
#[tokio::test]
async fn the_cursor_moves_on_and_a_restarted_numbering_is_read_again() {
    let events = vec![
        event(3, message(1, json!({"kind": "text"}))),
        event(4, message(2, json!({"kind": "text"}))),
    ];
    let env = Env::new(Inbox::bridge(events), Sites::default()).await;

    env.run_ok().await;
    env.run_ok().await;
    assert_eq!(env.event_requests(), [0, 4]);

    // The plugin lost its log and counts from 1 again.
    {
        let mut inbox = env.inbox();
        inbox.events = vec![event(1, message(45, json!({})))];
        inbox.images = HashMap::from([(45, JPEG.to_vec())]);
    }
    env.run_ok().await;

    assert_eq!(env.event_requests()[2..], [4, 0]);
    assert_eq!(env.state().await["events_after"], "1");
    assert_eq!(env.image_ids().await, [45]);
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_bridge_that_is_down_still_lets_the_inbox_drain
#[tokio::test]
async fn a_bridge_that_is_down_still_lets_the_inbox_drain() {
    let inbox = Inbox {
        events_status: Some(502),
        ..Inbox::listing(vec![inbox_item(1, json!({}))])
    };
    let env = Env::new(inbox, Sites::default()).await;

    let error = env.run_err().await;

    assert_eq!(error.notification_dedupe_key(), Some("wechat:bridge"));
    assert!(error.message().contains("connection refused"), "{}", error.message());
    assert_eq!(env.image_ids().await, [1]);
    assert_eq!(env.inbox().acked, [1]);
    assert!(!env.state().await.contains_key("events_after"));
    env.finish().await;
}

// D183: not in the Python baseline (closed Python PR #92).
#[tokio::test]
async fn a_logged_out_wechat_fails_the_run_after_reading_events() {
    // WeChat sat on its login screen: the plugin still served its event log, but could not
    // answer /wx/status, and the runs kept succeeding silently.
    let inbox = Inbox {
        images: HashMap::from([(45, JPEG.to_vec())]),
        status: Some(Reply::json(
            502,
            &json!({"error": "bridge unreachable: Remote end closed connection without response"}),
        )),
        events: vec![event(1, message(45, json!({})))],
        ..Inbox::listing(vec![inbox_item(1, json!({}))])
    };
    let env = Env::new(inbox, Sites::default()).await;

    let error = env.run_err().await;

    assert_eq!(error.notification_dedupe_key(), Some("wechat:login"));
    assert!(error.message().contains("not logged in"), "{}", error.message());
    assert!(
        error.message().contains("HTTP 502: bridge unreachable: Remote end closed"),
        "{}",
        error.message()
    );
    // What the log already held is still saved, and the inbox still drains.
    assert_eq!(env.image_ids().await, [45, 1]);
    assert_eq!(env.inbox().acked, [1]);
    assert_eq!(env.state().await["events_after"], "1");
    env.finish().await;
}

// D183.
#[tokio::test]
async fn a_status_without_a_logged_in_account_fails_the_run() {
    // The plugin's `j.put("login_wxid", null)` drops the key, so a missing key is the
    // likeliest logged-out answer.
    for status in [
        json!({"ok": true, "login_wxid": ""}),
        json!({"ok": true}),
        json!({"ok": true, "login_wxid": null}),
    ] {
        let inbox = Inbox {
            status: Some(Reply::json(200, &status)),
            ..Inbox::default()
        };
        let env = Env::new(inbox, Sites::default()).await;

        let error = env.run_err().await;

        assert_eq!(error.notification_dedupe_key(), Some("wechat:login"), "{status}");
        assert!(error.message().contains("no logged-in account"), "{status}: {}", error.message());
        env.finish().await;
    }
}

// D183.
#[tokio::test]
async fn a_status_call_that_hangs_up_fails_the_run_as_unreachable() {
    let inbox = Inbox {
        status: Some(Reply::Close),
        ..Inbox::default()
    };
    let env = Env::new(inbox, Sites::default()).await;

    let error = env.run_err().await;

    assert_eq!(error.notification_dedupe_key(), Some("wechat:unreachable"));
    assert!(error.message().contains("is unreachable"), "{}", error.message());
    env.finish().await;
}

// D183.
#[tokio::test]
async fn a_status_that_is_not_json_fails_the_run() {
    let inbox = Inbox {
        status: Some(Reply::status(200)),
        ..Inbox::default()
    };
    let env = Env::new(inbox, Sites::default()).await;

    let error = env.run_err().await;

    assert_eq!(error.notification_dedupe_key(), Some("wechat:login"));
    assert!(error.message().contains("something other than a status"), "{}", error.message());
    env.finish().await;
}

// D183.
#[tokio::test]
async fn a_rejected_token_on_status_asks_for_the_token_again() {
    let inbox = Inbox {
        status: Some(Reply::json(401, &json!({"error": "unauthorized"}))),
        ..Inbox::default()
    };
    let env = Env::new(inbox, Sites::default()).await;

    let error = env.run_err().await;

    assert_eq!(error.notification_dedupe_key(), Some("wechat:auth"));
    env.finish().await;
}

// D183.
#[tokio::test]
async fn a_logged_in_wechat_runs_clean_and_a_down_bridge_skips_the_status_check() {
    let env = Env::new(Inbox::default(), Sites::default()).await;
    env.run_ok().await;
    assert_eq!(env.inbox_requests().iter().filter(|seen| seen.path() == "/wx/status").count(), 1);
    env.finish().await;

    let inbox = Inbox {
        events_status: Some(502),
        status: Some(Reply::json(502, &json!({"error": "should not be asked"}))),
        ..Inbox::default()
    };
    let env = Env::new(inbox, Sites::default()).await;
    let error = env.run_err().await;
    assert_eq!(error.notification_dedupe_key(), Some("wechat:bridge"));
    assert!(!error.message().contains("not logged in"), "{}", error.message());
    assert!(!env.inbox_requests().iter().any(|seen| seen.path() == "/wx/status"));
    env.finish().await;
}

// ---------- articles ----------

// py: tests/test_wechat.py::test_image_urls_ask_for_the_original_size
#[test]
fn image_urls_ask_for_the_original_size() {
    assert_eq!(
        original(IMAGE_A),
        "https://mmbiz.qpic.cn/sz_mmbiz_jpg/AAA/0?wx_fmt=jpeg&from=appmsg"
    );
    assert_eq!(
        original("https://mmbiz.qpic.cn/mmbiz_gif/C/640"),
        "https://mmbiz.qpic.cn/mmbiz_gif/C/0"
    );
    assert_eq!(image_extension(IMAGE_B, "").expect("an extension"), "png");
    assert_eq!(image_extension(IMAGE_A, "image/webp").expect("an extension"), "webp");
    assert_eq!(sniff_extension(PNG), Some("png"));
    assert_eq!(sniff_extension(b"wxgf...."), Some("wxgf"));
    assert_eq!(sniff_extension(b"????"), None);
}

fn folder_of(env: &Env, msg_id: i64, title: &str) -> PathBuf {
    env.path()
        .join(article_directory(&object(&article_message(msg_id)), title).expect("a folder"))
}

// py: tests/test_wechat.py::test_an_article_is_saved_with_every_image_at_full_size
#[tokio::test]
async fn an_article_is_saved_with_every_image_at_full_size() {
    let env = Env::new(Inbox::bridge(vec![event(5, article_message(44))]), article_sites(Vec::new())).await;

    env.run_ok().await;

    let folder = folder_of(&env, 44, "每日Coser分享");
    assert!(file_name(&folder).ends_with("每日Coser分享 [44]"));
    assert_eq!(names(&folder), ["01.jpg", "02.png", "article.html", "article.json"]);
    assert_eq!(std::fs::read(folder.join("01.jpg")).expect("saved"), JPEG);
    assert_eq!(std::fs::read(folder.join("02.png")).expect("saved"), PNG);
    let summary = json_file(&folder.join("article.json"));
    assert_eq!(summary.get("url"), Some(&Value::from(CANONICAL_URL)));
    assert_eq!(summary.get("account"), Some(&Value::from("小叶菌ovo")));
    // Straight to the original size, and never with a Referer.
    assert_eq!(env.web_urls(), [FETCH_URL.to_owned(), original(IMAGE_A), original(IMAGE_B)]);
    let requests = env.web_requests();
    assert!(requests.iter().all(|seen| seen.header("referer").is_none()));
    assert!(requests[0].header("user-agent").unwrap_or_default().contains("Mobile"));

    let row = env.article().await;
    assert_eq!((row.msg_id, row.create_time), (44, CREATE_TIME + 44));
    assert_eq!(
        (
            row.url.as_str(),
            row.title.as_str(),
            row.account.as_str(),
            row.published_at.as_str(),
            row.image_count
        ),
        (CANONICAL_URL, "每日Coser分享", "小叶菌ovo", "2025-10-04 17:46:40", 2)
    );
    assert_eq!(Path::new(&row.local_path), folder.strip_prefix(env.path()).expect("inside"));
    assert_eq!(env.status(44).await, "done");

    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].title, "每日Coser分享");
    assert_eq!(sent[0].link_url, CANONICAL_URL);
    assert_eq!(sent[0].body, "小叶菌ovo | 2 images | From wxid_sender");
    assert_eq!(payload(&sent[0], "image_path"), &Value::Str(path_text(&folder.join("01.jpg"))));
    env.finish().await;
}

// py: tests/test_wechat.py::test_an_article_is_opened_in_wechat_before_it_is_fetched
#[tokio::test]
async fn an_article_is_opened_in_wechat_before_it_is_fetched() {
    let article = article_message(44);
    let env = Env::new(Inbox::bridge(vec![event(5, article.clone())]), article_sites(Vec::new())).await;

    env.run_ok().await;

    let seen = env.web.seen();
    let opened: Vec<&Seen> = seen.iter().filter(|seen| seen.path() == "/link/open").collect();
    assert_eq!(opened.len(), 1);
    let link = shared_link(article["content"].as_str().expect("content"))
        .expect("parses")
        .expect("a link");
    // The service taps the message open, so it is named, with the card title its bubble shows.
    assert_eq!(
        serde_json::from_slice::<Json>(&opened[0].body).expect("JSON"),
        json!({
            "url": link.url,
            "message": {"talker": "wxid_sender", "msg_id": 44, "create_time": CREATE_TIME + 44, "texts": [link.title]},
            "wait": 20,
        })
    );
    assert_eq!(opened[0].header("content-type"), Some("application/json"));
    let open_at = seen.iter().position(|seen| seen.path() == "/link/open").expect("opened");
    let fetch_at = seen
        .iter()
        .position(|seen| seen.host() == "mp.weixin.qq.com" && seen.path() == "/s")
        .expect("fetched");
    assert!(open_at < fetch_at);
    assert_eq!(env.status(44).await, "done");
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_link_that_never_reports_loaded_is_still_saved
#[tokio::test]
async fn a_link_that_never_reports_loaded_is_still_saved() {
    let inbox = Inbox {
        link_open: Some(Reply::json(200, &json!({"loaded": false, "seconds": 20}))),
        ..Inbox::bridge(vec![event(5, article_message(44))])
    };
    let env = Env::new(inbox, article_sites(Vec::new())).await;

    env.run_ok().await;

    assert_eq!(env.status(44).await, "done");
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_link_wechat_cannot_open_is_retried_without_being_fetched
#[tokio::test]
async fn a_link_wechat_cannot_open_is_retried_without_being_fetched() {
    let inbox = Inbox {
        link_open: Some(Reply::json(
            502,
            &json!({"error": "adb could not open the link in WeChat: device offline"}),
        )),
        ..Inbox::bridge(vec![event(5, article_message(44))])
    };
    let env = Env::new(inbox, article_sites(Vec::new())).await;

    let error = env.run_err().await;

    assert!(error.message().contains("device offline"), "{}", error.message());
    assert!(env.web_requests().is_empty());
    let queued = env.queue(44).await;
    assert_eq!((queued.status.as_str(), queued.attempts), ("pending", 1));
    assert_eq!(env.entries(), 0);
    env.finish().await;
}

// py: tests/test_wechat.py::test_an_image_that_will_not_load_at_any_size_is_skipped
#[tokio::test]
async fn an_image_that_will_not_load_at_any_size_is_skipped() {
    let env = Env::new(
        Inbox::bridge(vec![event(5, article_message(44))]),
        article_sites(vec![(original(IMAGE_B), Reply::status(404))]),
    )
    .await;

    env.run_ok().await;

    assert_eq!(env.web_urls().last().map(String::as_str), Some(IMAGE_B));
    assert_eq!(env.article().await.image_count, 1);
    assert_eq!(env.notifier.sent()[0].body, "小叶菌ovo | 1 of 2 images | From wxid_sender");
    env.finish().await;
}

const CAPTCHA: &str = "https://mp.weixin.qq.com/mp/wappoc_appmsgcaptcha?poc_token=x";

// py: tests/test_wechat.py::test_a_captcha_page_fails_the_article_without_writing_anything
#[tokio::test]
async fn a_captcha_page_fails_the_article_without_writing_anything() {
    let sites = Sites::with(vec![
        (FETCH_URL.to_owned(), redirect(CAPTCHA)),
        (CAPTCHA.to_owned(), html(200, "verify")),
    ]);
    let env = Env::new(Inbox::bridge(vec![event(5, article_message(44))]), sites).await;

    let error = env.run_err().await;

    assert!(error.message().contains("captcha"), "{}", error.message());
    assert!(env.articles().await.is_empty());
    assert_eq!(env.status(44).await, "pending");
    assert_eq!(env.entries(), 0);
    env.finish().await;
}

const COVER: &str = "https://mmbiz.qpic.cn/mmbiz_jpg/COVER/0?wx_fmt=jpeg";
const IMAGE_POST_URL: &str =
    "https://mp.weixin.qq.com/s?__biz=MzA5==&amp;mid=2247&amp;idx=1&amp;sn=abc123&amp;scene=1&amp;from_masonry=1&amp;t=1#rd";
const IMAGE_POST_FETCH: &str = "https://mp.weixin.qq.com/s?__biz=MzA5==&mid=2247&idx=1&sn=abc123&scene=1&from_masonry=1&t=1";

/// `_image_post`.
fn image_post(msg_id: i64, pictures: u32) -> Json {
    let content = share(IMAGE_POST_URL, "5").replace(
        "</appmsg>",
        &format!(
            "<mmreadershare><itemshowtype>8</itemshowtype><coverpicimageurl>{COVER}</coverpicimageurl>\
             <piccount>{pictures}</piccount></mmreadershare></appmsg>"
        ),
    );
    merge(article_message(msg_id), json!({"content": content}))
}

/// `_picture_item`: a picture as an image post's page writes it.
fn picture_item(name: &str) -> String {
    format!(
        "{{ width: '2560' * 1, height: '3413' * 1, \
         cdn_url: 'https:\\x2f\\x2fmmbiz.qpic.cn\\x2fsz_mmbiz_jpg\\x2f{name}\\x2f0?wx_fmt=jpeg\\x26amp;from=appmsg', \
         watermark_info: {{ cdn_url: 'https://mmbiz.qpic.cn/sz_mmbiz_jpg/W{name}/0?wx_fmt=jpeg', width: '0' * 1, \
         title: 'a }} [ \\' tricky' }}, live_photo: [], }}"
    )
}

fn pictures() -> Vec<String> {
    (0..3)
        .map(|n| format!("https://mmbiz.qpic.cn/sz_mmbiz_jpg/P{n}/0?wx_fmt=jpeg&from=appmsg"))
        .collect()
}

fn picture_page() -> String {
    let items: Vec<String> = (0..3).map(|n| picture_item(&format!("P{n}"))).collect();
    format!(
        "<html><script>var msg_title = '九张图';\nvar nickname = htmlDecode(\"某号\");\nvar ct = \"1760000000\";\n\
         window.picture_page_info_list = [{}];\n\
         if (window.picture_page_info_list.length) {{}}</script><div id=\"js_content\">caption</div></html>",
        items.join(",")
    )
}

/// `_captcha_web`.
fn captcha_sites(images: Vec<(String, Reply)>) -> Sites {
    let mut responses = vec![
        (IMAGE_POST_FETCH.to_owned(), redirect(CAPTCHA)),
        (
            CAPTCHA.to_owned(),
            html(200, "<script src=\"https://captcha.gtimg.com/TCaptcha.js\"></script>"),
        ),
    ];
    responses.extend(images);
    Sites::with(responses)
}

// py: tests/test_wechat.py::test_an_image_post_behind_the_captcha_is_read_from_wechat
#[tokio::test]
async fn an_image_post_behind_the_captcha_is_read_from_wechat() {
    let inbox = Inbox {
        post_page: Some(html(200, &picture_page())),
        ..Inbox::bridge(vec![event(5, image_post(47, 3))])
    };
    let env = Env::new(inbox, captcha_sites(pictures().into_iter().map(|url| (url, jpeg())).collect())).await;

    env.run_ok().await;

    let opened: Vec<Seen> = env
        .inbox_requests()
        .into_iter()
        .filter(|seen| seen.path() == "/post/page")
        .collect();
    assert_eq!(opened.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Json>(&opened[0].body).expect("JSON"),
        json!({
            "url": format!("{IMAGE_POST_FETCH}#rd"),
            "message": {"talker": "wxid_sender", "msg_id": 47, "create_time": CREATE_TIME + 47, "texts": ["每日分享"]},
            "wait": 30,
        })
    );
    let row = env.article().await;
    assert_eq!((row.published_at.as_str(), row.image_count), ("2025-10-09 08:53:20", 3));
    let folder = env.path().join(&row.local_path);
    assert_eq!(names(&folder), ["01.jpg", "02.jpg", "03.jpg", "article.html", "article.json"]);
    assert_eq!(std::fs::read_to_string(folder.join("article.html")).expect("saved"), picture_page());
    assert_eq!(env.status(47).await, "done");
    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].title, "九张图");
    assert_eq!(sent[0].body, "某号 | 3 images | From wxid_sender");
    env.finish().await;
}

// py: tests/test_wechat.py::test_an_image_post_wechat_cannot_open_is_retried_before_keeping_its_first_picture
#[tokio::test]
async fn an_image_post_wechat_cannot_open_is_retried_before_keeping_its_first_picture() {
    let env = Env::new(
        Inbox::bridge(vec![event(5, image_post(47, 9))]),
        captcha_sites(vec![(COVER.to_owned(), jpeg())]),
    )
    .await;

    let error = env.run_err().await;

    assert!(error.message().contains("not in the XWeb cache"), "{}", error.message());
    assert!(env.articles().await.is_empty());
    assert_eq!(env.status(47).await, "pending");

    env.set_attempts(47, 4).await;
    env.run_ok().await;

    let row = env.article().await;
    assert_eq!(row.image_count, 1);
    let folder = env.path().join(&row.local_path);
    assert_eq!(names(&folder), ["01.jpg", "article.json"]);
    assert_eq!(std::fs::read(folder.join("01.jpg")).expect("saved"), JPEG);
    assert_eq!(
        json_file(&folder.join("article.json")).get("picture_count"),
        Some(&Value::from(9_i64))
    );
    assert_eq!(env.status(47).await, "done");
    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].title, "每日分享");
    assert_eq!(
        sent[0].body,
        "小叶菌ovo | 1 of 9 images (WeChat could not open the rest) | From wxid_sender"
    );
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_rejected_token_is_never_settled_with_the_first_picture
#[tokio::test]
async fn a_rejected_token_is_never_settled_with_the_first_picture() {
    let inbox = Inbox {
        post_page: Some(Reply::json(401, &json!({"error": "unauthorized"}))),
        ..Inbox::bridge(vec![event(5, image_post(47, 9))])
    };
    let env = Env::new(inbox, captcha_sites(vec![(COVER.to_owned(), jpeg())])).await;
    env.run_err().await;
    env.set_attempts(47, 4).await;

    env.run_err().await;

    assert!(env.articles().await.is_empty());
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_single_picture_post_is_saved_whole
#[tokio::test]
async fn a_single_picture_post_is_saved_whole() {
    let page = picture_page().replacen(
        "picture_page_info_list = [",
        &format!("picture_page_info_list = [{}];//", picture_item("ONLY")),
        1,
    );
    let inbox = Inbox {
        post_page: Some(html(200, &page)),
        ..Inbox::bridge(vec![event(5, image_post(46, 1))])
    };
    let only = "https://mmbiz.qpic.cn/sz_mmbiz_jpg/ONLY/0?wx_fmt=jpeg&from=appmsg";
    let env = Env::new(inbox, captcha_sites(vec![(only.to_owned(), jpeg())])).await;

    env.run_ok().await;

    assert_eq!(env.notifier.sent()[0].body, "某号 | 1 image | From wxid_sender");
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_second_forward_of_a_saved_article_is_not_saved_again
#[tokio::test]
async fn a_second_forward_of_a_saved_article_is_not_saved_again() {
    let env = Env::new(
        Inbox::bridge(vec![event(5, article_message(44)), event(6, article_message(52))]),
        article_sites(Vec::new()),
    )
    .await;

    env.run_ok().await;

    assert_eq!(env.articles().await.iter().map(|row| row.msg_id).collect::<Vec<_>>(), [44]);
    assert_eq!(env.status(52).await, "done");
    assert_eq!(env.notifier.sent().len(), 1);
    env.finish().await;
}

// py: tests/test_wechat.py::test_an_article_saved_earlier_is_not_fetched_again
#[tokio::test]
async fn an_article_saved_earlier_is_not_fetched_again() {
    let env = Env::new(Inbox::bridge(vec![event(5, article_message(44))]), article_sites(Vec::new())).await;
    env.execute(format!(
        "INSERT INTO wechat_article (msg_id, create_time) VALUES (44, {})",
        CREATE_TIME + 44
    ))
    .await;

    env.run_ok().await;

    assert!(env.web_requests().is_empty());
    assert_eq!(env.status(44).await, "done");
    env.finish().await;
}

// ---------- tieba ----------

const TIEBA_SHARE_URL: &str = "https://tieba.baidu.com/p/11093034639?&amp;share=9105&amp;fr=sharewise&amp;is_video=false\
     &amp;unique=F9676370&amp;st=1791654528&amp;client_type=1&amp;sfc=weixin&amp;share_from=post";
const TIEBA_URL: &str = "https://tieba.baidu.com/p/11093034639";

fn tieba_pic(name: &str) -> String {
    format!("http://tiebapic.baidu.com/forum/pic/item/{name}.jpg?tbpicau=2026-10-12-05_grant")
}

fn tieba_message() -> Json {
    let content = share(TIEBA_SHARE_URL, "5").replace("<title>每日分享</title>", "<title>其实HDR比dlss5重要</title>");
    app_message(63, &content)
}

fn floor(floor: u32, author: &str, pictures: &[&str]) -> Json {
    let mut content = vec![json!({"type": "0", "text": format!("floor {floor}")})];
    content.extend(pictures.iter().map(
        |name| json!({"type": "3", "origin_src": tieba_pic(name), "big_cdn_src": format!("http://tiebapic.baidu.com/big/{name}.jpg")}),
    ));
    json!({"floor": floor.to_string(), "author": {"id": author}, "content": content})
}

fn tieba_pages() -> HashMap<String, Json> {
    HashMap::from([(
        "1".to_owned(),
        json!({
            "error_code": "0",
            "thread": {"title": "其实HDR比dlss5重要", "create_time": "1791654000", "author": {"id": "7"}},
            "forum": {"name": "显卡"},
            "post_list": [floor(1, "7", &["A", "B", "C", "B"]), floor(2, "7", &["LATER"])],
        }),
    )])
}

/// `_tieba_web`.
fn tieba_sites(overrides: Vec<(String, Reply)>) -> Sites {
    let mut responses: Vec<(String, Reply)> = ["A", "B", "C"].iter().map(|name| (tieba_pic(name), jpeg())).collect();
    responses.extend(overrides);
    Sites {
        tieba: Some(tieba_pages()),
        ..Sites::with(responses)
    }
}

// py: tests/test_wechat.py::test_a_tieba_post_keeps_the_pictures_of_its_first_floor
#[tokio::test]
async fn a_tieba_post_keeps_the_pictures_of_its_first_floor() {
    let env = Env::new(Inbox::bridge(vec![event(5, tieba_message())]), tieba_sites(Vec::new())).await;

    env.run_ok().await;

    let forms = env.sites().tieba_forms.clone();
    assert_eq!(forms.len(), 1);
    let form = &forms[0];
    assert_eq!((form["kz"].as_str(), form["pn"].as_str()), ("11093034639", "1"));
    let mut unsigned: Vec<(&String, &String)> = form.iter().filter(|(key, _)| *key != "sign").collect();
    unsigned.sort();
    let raw: String = unsigned.iter().map(|(key, value)| format!("{key}={value}")).collect::<String>() + "tiebaclient!!!";
    assert_eq!(form["sign"], md5_hex(raw.as_bytes()).to_uppercase());

    let row = env.article().await;
    assert_eq!(
        (
            row.url.as_str(),
            row.title.as_str(),
            row.account.as_str(),
            row.published_at.as_str(),
            row.image_count
        ),
        (TIEBA_URL, "其实HDR比dlss5重要", "显卡吧", "2026-10-10 17:40:00", 3)
    );
    let folder = env.path().join(&row.local_path);
    assert_eq!(names(&folder), ["01.jpg", "02.jpg", "03.jpg", "article.json"]);
    let summary = json_file(&folder.join("article.json"));
    let urls: Vec<Value> = summary
        .get("images")
        .and_then(Value::as_list)
        .expect("images")
        .iter()
        .map(|entry| entry.get("url").cloned().expect("a url"))
        .collect();
    assert_eq!(urls, ["A", "B", "C"].map(|name| Value::Str(tieba_pic(name))));
    assert_eq!(env.status(63).await, "done");
    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].link_url, TIEBA_URL);
    assert_eq!(sent[0].body, "显卡吧 | 3 images | From wxid_sender");
    env.finish().await;
}

const ZHIHU_SHARE_URL: &str = "https://www.zhihu.com/question/1981023166554989468/answer/2100311203096573240?share_code=x";
const ZHIHU_TIKHUB: &str = "https://api.tikhub.io/api/v1/zhihu/web/fetch_answer_detail?answer_id=2100311203096573240";

fn zhihu_pic(name: &str) -> String {
    format!("https://pica.zhimg.com/v2-{name}_r.jpg")
}

// py: tests/test_wechat.py::test_a_zhihu_answer_is_saved_like_any_shared_post
#[tokio::test]
async fn a_zhihu_answer_is_saved_like_any_shared_post() {
    let content = share(ZHIHU_SHARE_URL, "5").replace("<title>每日分享</title>", "<title>高跟鞋配白丝真的很难看吗？</title>");
    let figures: String = ["A", "B"]
        .iter()
        .map(|name| format!("<figure><img data-original=\"{}\"/></figure>", zhihu_pic(name)))
        .collect();
    let answer = json!({
        "data": {
            "content": figures,
            "author": {"name": "知乎用户"},
            "question": {"title": "高跟鞋配白丝真的很难看吗？"},
            "created_time": 1_793_533_212,
        },
    });
    let sites = Sites::with(vec![
        (ZHIHU_TIKHUB.to_owned(), Reply::json(200, &answer)),
        (zhihu_pic("A"), jpeg()),
        (zhihu_pic("B"), jpeg()),
    ]);
    let mut env = Env::new(Inbox::bridge(vec![event(5, app_message(64, &content))]), sites).await;
    env.tikhub("tikhub-key");

    env.run_ok().await;

    let requests = env.web_requests();
    let tikhub = requests.iter().find(|seen| seen.host() == "api.tikhub.io").expect("asked TikHub");
    assert_eq!(tikhub.header("authorization"), Some("Bearer tikhub-key"));
    let row = env.article().await;
    assert_eq!(
        (row.url.as_str(), row.title.as_str(), row.account.as_str()),
        (
            "https://www.zhihu.com/answer/2100311203096573240",
            "高跟鞋配白丝真的很难看吗？",
            "知乎用户"
        )
    );
    assert_eq!(names(&env.path().join(&row.local_path)), ["01.jpg", "02.jpg", "article.json"]);
    assert_eq!(env.status(64).await, "done");
    env.finish().await;
}

// py: tests/test_wechat.py::test_an_expired_tieba_grant_is_not_saved_as_a_picture
#[tokio::test]
async fn an_expired_tieba_grant_is_not_saved_as_a_picture() {
    let logo = Reply::Respond {
        status: 200,
        headers: vec![
            ("content-type".to_owned(), "image/jpeg".to_owned()),
            ("error-message".to_owned(), "img not auth".to_owned()),
        ],
        body: JPEG.to_vec(),
    };
    let env = Env::new(
        Inbox::bridge(vec![event(5, tieba_message())]),
        tieba_sites(vec![(tieba_pic("C"), logo)]),
    )
    .await;

    env.run_ok().await;

    assert_eq!(env.article().await.image_count, 2);
    assert_eq!(env.notifier.sent()[0].body, "显卡吧 | 2 of 3 images | From wxid_sender");
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_deleted_tieba_post_is_retried_without_writing_anything
#[tokio::test]
async fn a_deleted_tieba_post_is_retried_without_writing_anything() {
    let sites = Sites {
        tieba: Some(HashMap::from([(
            "1".to_owned(),
            json!({"error_code": "4", "error_msg": "贴子可能已被删除"}),
        )])),
        ..Sites::default()
    };
    let env = Env::new(Inbox::bridge(vec![event(5, tieba_message())]), sites).await;

    let error = env.run_err().await;

    assert!(error.message().contains("贴子可能已被删除"), "{}", error.message());
    assert!(env.articles().await.is_empty());
    assert_eq!(env.status(63).await, "pending");
    assert_eq!(env.entries(), 0);
    env.finish().await;
}

const TIEBA_VIDEO: &str =
    "http://tb-video.bdstatic.com/tieba-smallvideo-transcode-cae/12_abc_0_cae3.mp4?tbau=2026-10-21_grant&ptid=11104915948";
const TIEBA_VIDEO_COVER: &str = "http://tiebapic.baidu.com/forum/w%3D960/sign=x/cover.jpg?tbpicau=2026-10-31-05_grant";
/// `_MP4` as the module defines it last, which every test reads.
const MP4: &[u8] = b"\x00\x00\x00\x20ftypisom\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00video-bytes";

fn tieba_video_message() -> Json {
    let url = "https://tieba.baidu.com/p/11104915948?share_from=post&amp;is_video=true&amp;sfc=weixin";
    let content = share(url, "4").replace("<title>每日分享</title>", "<title>这个数值够不够还原</title>");
    app_message(64, &content)
}

fn tieba_video_pages(in_floor: bool) -> HashMap<String, Json> {
    let mut first = floor(1, "7", &[]);
    if in_floor {
        first["content"]
            .as_array_mut()
            .expect("content")
            .push(json!({"type": 5, "link": TIEBA_VIDEO, "src": TIEBA_VIDEO_COVER, "during_time": 13}));
    }
    let thread = json!({
        "title": "这个数值够不够还原",
        "create_time": "1791984959",
        "video_info": {"video_url": TIEBA_VIDEO, "thumbnail_url": TIEBA_VIDEO_COVER, "video_duration": 13},
    });
    HashMap::from([(
        "1".to_owned(),
        json!({"error_code": "0", "thread": thread, "forum": {"name": "绝区零内鬼"}, "post_list": [first]}),
    )])
}

/// `_tieba_video_web`.
fn tieba_video_sites(in_floor: bool, video: Option<Reply>) -> Sites {
    Sites {
        tieba: Some(tieba_video_pages(in_floor)),
        ..Sites::with(vec![
            (TIEBA_VIDEO.to_owned(), video.unwrap_or_else(|| image_reply(MP4, "video/mp4"))),
            (TIEBA_VIDEO_COVER.to_owned(), jpeg()),
        ])
    }
}

// py: tests/test_wechat.py::test_a_tieba_video_share_names_its_post
#[test]
fn a_tieba_video_share_names_its_post() {
    let content = tieba_video_message()["content"].as_str().expect("content").to_owned();
    let link = crate::tieba_post::parse_tieba_link(&content)
        .expect("parses")
        .expect("a Tieba link");

    assert_eq!(
        (link.url.as_str(), link.title.as_str()),
        ("https://tieba.baidu.com/p/11104915948", "这个数值够不够还原")
    );
    assert_eq!(work(&event(9, tieba_video_message())), Some("article"));
    // Only Tieba shares video cards this source reads.
    assert!(parse_article_link(&share(SHARE_URL, "4")).expect("parses").is_none());
    assert_eq!(
        work(&event(10, message(10, json!({"kind": "app", "content": share(SHARE_URL, "4")})))),
        Some("unsupported")
    );
}

// py: tests/test_wechat.py::test_a_tieba_video_post_keeps_its_video_and_cover
#[tokio::test]
async fn a_tieba_video_post_keeps_its_video_and_cover() {
    for in_floor in [true, false] {
        let env = Env::new(
            Inbox::bridge(vec![event(5, tieba_video_message())]),
            tieba_video_sites(in_floor, None),
        )
        .await;

        env.run_ok().await;

        let row = env.article().await;
        assert_eq!(
            (
                row.url.as_str(),
                row.title.as_str(),
                row.account.as_str(),
                row.published_at.as_str(),
                row.image_count
            ),
            (
                "https://tieba.baidu.com/p/11104915948",
                "这个数值够不够还原",
                "绝区零内鬼吧",
                "2026-10-14 13:35:59",
                0
            ),
            "{in_floor}"
        );
        let videos = row.metadata.get("videos").cloned().expect("videos");
        assert_eq!(
            videos,
            value(&json!([{"url": TIEBA_VIDEO, "file": "video.mp4", "cover": "video.jpg"}]))
        );
        let folder = env.path().join(&row.local_path);
        assert_eq!(names(&folder), ["article.json", "video.jpg", "video.mp4"]);
        assert_eq!(std::fs::read(folder.join("video.mp4")).expect("saved"), MP4);
        assert_eq!(names(folder.parent().expect("a month")), [file_name(&folder)]);
        assert_eq!(env.status(64).await, "done");
        let sent = env.notifier.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].body, "绝区零内鬼吧 | 1 video | From wxid_sender");
        assert_eq!(payload(&sent[0], "image_path"), &Value::Str(path_text(&folder.join("video.jpg"))));
        env.finish().await;
    }
}

// py: tests/test_wechat.py::test_a_tieba_video_that_will_not_download_is_retried_without_writing_anything
#[tokio::test]
async fn a_tieba_video_that_will_not_download_is_retried_without_writing_anything() {
    let refused = html(401, "<html>401 Authorization Required</html>");
    let env = Env::new(
        Inbox::bridge(vec![event(5, tieba_video_message())]),
        tieba_video_sites(true, Some(refused)),
    )
    .await;

    env.run_err().await;

    assert!(env.articles().await.is_empty());
    let queued = env.queue(64).await;
    assert!(queued.last_error.contains("none of the 1 videos"), "{}", queued.last_error);
    assert_eq!(queued.status, "pending");
    assert!(env.files().is_empty());
    assert!(env.notifier.sent().is_empty());
    env.finish().await;
}

// ---------- nga ----------

const NGA_SHARE_URL: &str = "https://bbs.nga.cn/read.php?tid=47695975";

fn nga_attach(name: &str) -> String {
    format!("https://img.nga.cn/attachments/mon_202510/11/{name}.jpg")
}

fn nga_message() -> Json {
    let content = share(NGA_SHARE_URL, "5").replace("<title>每日分享</title>", "<title>[同人搬运]贪吃蛇</title>");
    app_message(81, &content)
}

fn nga_thread() -> Json {
    let first = json!({
        "lou": 0,
        "subject": "",
        "postdatetimestamp": 1_791_654_000,
        "content": "text<br/>[img]./mon_202510/11/A.jpg.medium.jpg[/img]\
                    [img]https://img.nga.178.com/attachments/mon_202510/11/B.jpg[/img][img]./mon_202510/11/A.jpg[/img]",
        "attachs": {"0": {"attachurl": "mon_202510/11/C.jpg", "type": "img"}, "1": {"attachurl": "mon_202510/11/x.zip", "type": "zip"}},
    });
    let reply = json!({"lou": 1, "content": "[img]./mon_202510/11/LATER.jpg[/img]"});
    json!({
        "data": {
            "__R": {"0": first, "1": reply},
            "__T": {"subject": "[同人搬运]贪吃蛇 &amp; 更多", "postdate": 1_791_654_000},
            "__F": {"name": "二次元国家地理"},
        },
        "time": 1_791_654_100,
    })
}

/// `_nga_web`.
fn nga_sites() -> Sites {
    Sites {
        nga: Some(nga_thread()),
        ..Sites::with(["A", "B", "C"].iter().map(|name| (nga_attach(name), jpeg())).collect())
    }
}

// py: tests/test_wechat.py::test_an_nga_share_names_its_thread
#[test]
fn an_nga_share_names_its_thread() {
    let content = nga_message()["content"].as_str().expect("content").to_owned();
    let link = crate::nga_post::parse_nga_link(&content).expect("parses").expect("an NGA link");

    assert_eq!((link.url.as_str(), link.title.as_str()), (NGA_SHARE_URL, "[同人搬运]贪吃蛇"));
    assert_eq!(canonical_url(&link.url).expect("canonical"), NGA_SHARE_URL);
    assert_eq!(work(&event(1, nga_message())), Some("article"));
    let thread_id = |url: &str| crate::nga_post::nga_thread_id(url).expect("parses");
    assert_eq!(thread_id("https://ngabbs.com/read.php?tid=123&rand=5").as_deref(), Some("123"));
    assert_eq!(thread_id("https://bbs.nga.cn/thread.php?fid=-7"), None);
    assert_eq!(thread_id("https://evil.example/read.php?tid=1"), None);
    assert!(crate::nga_post::parse_nga_link(&share(SHARE_URL, "5")).expect("parses").is_none());
}

// py: tests/test_wechat.py::test_an_nga_thread_keeps_the_pictures_of_its_first_floor
#[tokio::test]
async fn an_nga_thread_keeps_the_pictures_of_its_first_floor() {
    let env = Env::new(Inbox::bridge(vec![event(5, nga_message())]), nga_sites()).await;

    env.run_ok().await;

    assert_eq!(env.sites().nga_cookies, ["ngaPassportUid=42; ngaPassportCid=session-cid"]);
    let row = env.article().await;
    assert_eq!(
        (
            row.url.as_str(),
            row.title.as_str(),
            row.account.as_str(),
            row.published_at.as_str(),
            row.image_count
        ),
        (NGA_SHARE_URL, "[同人搬运]贪吃蛇 & 更多", "二次元国家地理", "2026-10-10 17:40:00", 3)
    );
    let folder = env.path().join(&row.local_path);
    assert_eq!(names(&folder), ["01.jpg", "02.jpg", "03.jpg", "article.json"]);
    assert_eq!(env.status(81).await, "done");
    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].link_url, NGA_SHARE_URL);
    assert_eq!(sent[0].body, "二次元国家地理 | 3 images | From wxid_sender");
    assert_eq!(*env.sessions.checks.lock().expect("lock"), [(true, String::new())]);
    env.finish().await;
}

// py: tests/test_wechat.py::test_an_nga_thread_waits_for_a_login_without_using_up_its_tries
#[tokio::test]
async fn an_nga_thread_waits_for_a_login_without_using_up_its_tries() {
    let mut env = Env::new(Inbox::bridge(vec![event(5, nga_message())]), nga_sites()).await;
    env.sessions = Arc::new(Sessions {
        login: None,
        checks: Mutex::new(Vec::new()),
    });

    let error = env.run_err().await;

    assert!(error.message().contains("NGA login"), "{}", error.message());
    let queued = env.queue(81).await;
    assert_eq!((queued.status.as_str(), queued.attempts), ("pending", 0));
    assert_eq!(env.entries(), 0);
    env.finish().await;
}

// py: tests/test_wechat.py::test_an_expired_nga_login_is_reported_as_a_login_problem
#[tokio::test]
async fn an_expired_nga_login_is_reported_as_a_login_problem() {
    let mut env = Env::new(Inbox::bridge(vec![event(5, nga_message())]), nga_sites()).await;
    env.sessions = Arc::new(Sessions {
        login: Some(vec![("ngaPassportUid", "42"), ("ngaPassportCid", "stale")]),
        checks: Mutex::new(Vec::new()),
    });

    let error = env.run_err().await;

    assert!(error.message().contains("signed-in session"), "{}", error.message());
    let queued = env.queue(81).await;
    assert_eq!((queued.status.as_str(), queued.attempts), ("pending", 0));
    assert!(queued.last_error.contains("未登录"));
    assert!(queued.last_error.contains("登录态"));
    let checks = env.sessions.checks.lock().expect("lock").clone();
    assert_eq!(checks.len(), 1);
    assert!(!checks[0].0);
    assert!(checks[0].1.contains("未登录"));
    env.finish().await;
}

// py: tests/test_wechat.py::test_new_kinds_of_work_read_the_event_log_again_once
#[tokio::test]
async fn new_kinds_of_work_read_the_event_log_again_once() {
    let events = vec![
        event(4, message(45, json!({}))),
        event(5, tieba_message()),
        event(6, message(46, json!({"kind": "text"}))),
    ];
    let env = Env::new(Inbox::bridge(events), tieba_sites(Vec::new())).await;
    // An earlier version read up to seq 6 and kept only the image.
    env.set_state("events_after", "6").await;
    env.enqueue(45, "image", 4, &json!({}), "done").await;

    env.run_ok().await;
    env.run_ok().await;

    assert_eq!(env.event_requests(), [0, 6]);
    assert_eq!(env.status(45).await, "done");
    assert_eq!(env.status(63).await, "done");
    let state = env.state().await;
    assert_eq!((state["events_after"].as_str(), state["work_version"].as_str()), ("6", "8"));
    assert!(state.contains_key("unsupported_since"));
    assert_eq!(state.len(), 3);
    assert_eq!(env.notifier.sent().len(), 1);
    env.finish().await;
}

// ---------- stickers ----------

const STICKER: &[u8] = b"GIF89a-sticker-frames";
const STICKER_KEY: &str = "00112233445566778899aabbccddeeff";
const STICKER_CDN: &str = "http://wxapp.tc.qq.com/262/20304/stodownload?m=plain&filekey=aaa&bizid=1023";
const STICKER_ENCRYPTED: &str = "http://wxapp.tc.qq.com/262/20304/stodownload?m=enc&filekey=bbb&bizid=1023";

fn sticker_md5() -> String {
    md5_hex(STICKER)
}

/// `_sticker_share`.
fn sticker_share(cdn: &str, md5: &str) -> String {
    let attribute = |url: &str| url.replace("://", "*#*//").replace('&', "&amp;");
    format!(
        "<msg><emoji fromusername=\"wxid_sender\" tousername=\"wxid_me\" type=\"2\" idbuffer=\"media*#*0_0\" \
         md5=\"{md5}\" len=\"{}\" productid=\"\" cdnurl=\"{}\" thumburl=\"\" \
         encrypturl=\"{}\" aeskey=\"{STICKER_KEY}\" width=\"240\" height=\"240\"></emoji>\
         <gameext type=\"0\" content=\"0\"></gameext></msg>",
        STICKER.len(),
        if cdn.is_empty() { String::new() } else { attribute(cdn) },
        attribute(STICKER_ENCRYPTED)
    )
}

fn sticker_message(content: &str) -> Json {
    message(260, json!({"type": 47, "kind": "emoji", "content": content, "image": null}))
}

/// `_encrypted`: AES-128-CBC with the key as IV, PKCS#7-padded.
fn encrypted(content: &[u8]) -> Vec<u8> {
    let mut key = [0_u8; 16];
    for (slot, at) in key.iter_mut().zip((0..32).step_by(2)) {
        *slot = u8::from_str_radix(&STICKER_KEY[at..at + 2], 16).expect("hex");
    }
    let padding = 16 - content.len() % 16;
    let mut data = content.to_vec();
    data.extend(std::iter::repeat_n(u8::try_from(padding).expect("small"), padding));
    let cipher = Aes128::new(&Array::from(key));
    let mut previous = key;
    for block in data.chunks_exact_mut(16) {
        let mut current = [0_u8; 16];
        for (out, (plain, chain)) in current.iter_mut().zip(block.iter().zip(previous)) {
            *out = plain ^ chain;
        }
        let mut array = Array::from(current);
        cipher.encrypt_block(&mut array);
        block.copy_from_slice(&array);
        previous.copy_from_slice(&array);
    }
    data
}

/// `_sticker_web`.
fn sticker_sites(plain: Option<Reply>) -> Sites {
    Sites::with(vec![
        (STICKER_CDN.to_owned(), plain.unwrap_or_else(|| image_reply(STICKER, "image/jpg"))),
        (STICKER_ENCRYPTED.to_owned(), Reply::bytes(200, encrypted(STICKER))),
    ])
}

fn b64encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let bytes = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let bits = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(
                    ALPHABET[usize::try_from((bits >> (18 - 6 * index)) & 63).expect("small")],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn protobuf_field(number: u8, value: &str) -> Vec<u8> {
    let mut field = vec![(number << 3) | 2, u8::try_from(value.len()).expect("short")];
    field.extend_from_slice(value.as_bytes());
    field
}

/// `_large_sticker_share`: an app message of type 8 whose `emojiinfo` names the files.
fn large_sticker_share(cdn: &str) -> String {
    let md5 = sticker_md5();
    let mut info = Vec::new();
    info.extend(protobuf_field(1, &md5));
    info.extend(protobuf_field(2, cdn));
    info.extend([3 << 3, 1]); // a number in between
    info.extend(protobuf_field(5, STICKER_ENCRYPTED));
    info.extend(protobuf_field(6, STICKER_KEY));
    info.extend(protobuf_field(7, ""));
    info.extend(protobuf_field(9, &"f".repeat(32)));
    format!(
        "<?xml version=\"1.0\"?>\n<msg>\n\t<appmsg appid=\"\" sdkver=\"0\">\n\t\t<title />\n\t\t<type>8</type>\n\t\t<appattach>\
         <totallen>{}</totallen><emoticonmd5>{md5}</emoticonmd5><fileext>pic</fileext>\
         <emojiinfo>{}</emojiinfo></appattach>\n\t</appmsg>\n\t<fromusername>wxid_sender</fromusername>\n</msg>\n",
        STICKER.len(),
        b64encode(&info)
    )
}

// py: tests/test_wechat.py::test_a_sticker_names_its_plain_and_encrypted_files
#[test]
fn a_sticker_names_its_plain_and_encrypted_files() {
    let share = sticker_share(STICKER_CDN, &sticker_md5());
    let sticker = parse_sticker(&share).expect("a sticker");

    assert_eq!(
        (
            sticker.md5.as_str(),
            sticker.cdn_url.as_str(),
            sticker.encrypted_url.as_str(),
            sticker.aes_key.as_str()
        ),
        (sticker_md5().as_str(), STICKER_CDN, STICKER_ENCRYPTED, STICKER_KEY)
    );
    assert_eq!(decrypt_sticker(&encrypted(STICKER), STICKER_KEY).expect("decrypts"), STICKER);
    assert_eq!(work(&event(1, sticker_message(&share))), Some("sticker"));
    assert_eq!(
        queued_message(&object(&sticker_message(&share)), "sticker").get("content"),
        Some(&Value::Str(share.clone()))
    );
    assert_eq!(
        work(&event(2, sticker_message("<msg><emoji md5=\"x\" cdnurl=\"\" /></msg>"))),
        Some("unsupported")
    );
    assert!(parse_sticker(&self::share(SHARE_URL, "5")).is_none());
}

// py: tests/test_wechat.py::test_a_sticker_is_saved_from_the_cdn_beside_the_images
#[tokio::test]
async fn a_sticker_is_saved_from_the_cdn_beside_the_images() {
    let share = sticker_share(STICKER_CDN, &sticker_md5());
    let env = Env::new(Inbox::bridge(vec![event(1, sticker_message(&share))]), sticker_sites(None)).await;

    env.run_ok().await;

    let rows = env.images().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, "2025-10/2025-10-05 031320 [260].gif");
    assert_eq!(std::fs::read(env.path().join(&rows[0].2)).expect("saved"), STICKER);
    assert_eq!(rows[0].4.get("type"), Some(&Value::from("sticker")));
    assert_eq!(env.web_urls(), [STICKER_CDN]);
    assert!(env.inbox().acked.is_empty());
    assert_eq!(env.status(260).await, "done");
    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        payload(&sent[0], "image_path"),
        &Value::Str(path_text(&env.path().join(&rows[0].2)))
    );
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_large_sticker_names_its_files_in_its_emoji_info
#[test]
fn a_large_sticker_names_its_files_in_its_emoji_info() {
    let sticker = parse_sticker(&large_sticker_share(STICKER_CDN)).expect("a sticker");

    assert_eq!(
        (
            sticker.md5.as_str(),
            sticker.cdn_url.as_str(),
            sticker.encrypted_url.as_str(),
            sticker.aes_key.as_str()
        ),
        (sticker_md5().as_str(), STICKER_CDN, STICKER_ENCRYPTED, STICKER_KEY)
    );
    let large = message(
        264,
        json!({"type": 1_048_625, "kind": "other", "content": large_sticker_share(STICKER_CDN), "image": null}),
    );
    assert_eq!(work(&event(1, large)), Some("sticker"));
    assert!(parse_sticker(&large_sticker_share(STICKER_CDN).replace("<type>8</type>", "<type>6</type>")).is_none());
    assert!(parse_sticker(&large_sticker_share(STICKER_CDN).replace("<emojiinfo>", "<emojiinfo>!!")).is_none());
}

// py: tests/test_wechat.py::test_a_large_sticker_is_saved_like_any_other
#[tokio::test]
async fn a_large_sticker_is_saved_like_any_other() {
    let large = message(
        264,
        json!({"type": 1_048_625, "kind": "other", "content": large_sticker_share(""), "image": null}),
    );
    let env = Env::new(Inbox::bridge(vec![event(1, large)]), sticker_sites(None)).await;

    env.run_ok().await;

    let rows = env.images().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(std::fs::read(env.path().join(&rows[0].2)).expect("saved"), STICKER);
    assert_eq!(env.status(264).await, "done");
    assert_eq!(env.notifier.sent().len(), 1);
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_sticker_the_cdn_will_not_give_is_decrypted_from_its_encrypted_copy
#[tokio::test]
async fn a_sticker_the_cdn_will_not_give_is_decrypted_from_its_encrypted_copy() {
    let md5 = sticker_md5();
    let cases = [
        ("no-cdnurl", sticker_share("", &md5), None),
        ("cdnurl-fails", sticker_share(STICKER_CDN, &md5), Some(Reply::status(404))),
        (
            "cdnurl-wrong-file",
            sticker_share(STICKER_CDN, &md5),
            Some(Reply::bytes(200, b"GIF89a-something-else".to_vec())),
        ),
        // A connection the CDN drops is the same as an error.
        ("cdnurl-reset", sticker_share(STICKER_CDN, &md5), Some(Reply::Reset)),
    ];
    for (case, content, plain) in cases {
        let env = Env::new(Inbox::bridge(vec![event(1, sticker_message(&content))]), sticker_sites(plain)).await;

        env.run_ok().await;

        let rows = env.images().await;
        assert_eq!(rows.len(), 1, "{case}");
        assert_eq!(std::fs::read(env.path().join(&rows[0].2)).expect("saved"), STICKER, "{case}");
        env.finish().await;
    }
}

// py: tests/test_wechat.py::test_a_sticker_that_matches_neither_copy_is_retried_without_writing_anything
#[tokio::test]
async fn a_sticker_that_matches_neither_copy_is_retried_without_writing_anything() {
    let content = sticker_share(STICKER_CDN, &"0".repeat(32));
    let env = Env::new(Inbox::bridge(vec![event(1, sticker_message(&content))]), sticker_sites(None)).await;

    let error = env.run_err().await;

    assert!(error.message().contains("does not match its md5"), "{}", error.message());
    assert!(env.images().await.is_empty());
    assert!(!env.files().iter().any(|file| file.ends_with(".gif")));
    assert_eq!(env.status(260).await, "pending");
    env.finish().await;
}

// ---------- 视频号 videos ----------

const OBJECT_ID: &str = "14983747958594083406";
const NONCE_ID: &str = "12832287456652343062_0_25_90_26_1791657144215330_efda1c2e-c4d8-11f1-83d0-83b106a41c8a";
const COVER_URL: &str = "https://wxapp.tc.qq.com/251/20304/stodownload?encfilekey=abc&amp;token=cover-token";

/// `_finder_share`.
fn finder_share(media_type: &str, live_id: &str, object_id: &str) -> String {
    format!(
        "<msg><appmsg appid=\"\" sdkver=\"0\"><title>当前版本不支持展示该内容</title><type>51</type>\
         <url>https://support.weixin.qq.com/update/</url>\
         <finderFeed><objectId>{object_id}</objectId><feedType>4</feedType><nickname>jiang57l</nickname>\
         <avatar><![CDATA[https://wx.qlogo.cn/finderhead/ver_1/x/0]]></avatar>\
         <desc>镜头之外才是故事真正的开始 偶遇着他们的幸福#徐绿杨</desc><mediaCount>1</mediaCount>\
         <objectNonceId>{NONCE_ID}</objectNonceId><liveId>{live_id}</liveId>\
         <username>v2_0600@finder</username><mediaList><media>\
         <thumbUrl>{COVER_URL}</thumbUrl><videoPlayDuration>9</videoPlayDuration>\
         <url>http://wxapp.tc.qq.com/251/20302/stodownload?encfilekey=def&amp;m=1</url>\
         <coverUrl>{COVER_URL}</coverUrl><height>1920</height><mediaType>{media_type}</mediaType><width>1080</width>\
         </media></mediaList><megaVideo><objectId></objectId><objectNonceId></objectNonceId></megaVideo>\
         </finderFeed></appmsg><fromusername>wxid_sender</fromusername></msg>"
    )
}

fn default_finder() -> String {
    finder_share("4", "0", OBJECT_ID)
}

fn video_message(msg_id: i64) -> Json {
    message(
        msg_id,
        json!({"type": 754_974_769, "kind": "channel_video", "content": default_finder(), "image": null}),
    )
}

const TIKHUB_DETAIL: &str = "https://api.tikhub.io/api/v1/wechat_channels/v2/fetch_video_detail";
const VIDEO_URL: &str = "https://finder.video.qq.com/251/20302/stodownload?encfilekey=enc&token=video-token";
const DECODE_KEY: u64 = 2_136_343_393;

fn channels_encrypted(plain: &[u8], decode_key: u64) -> Vec<u8> {
    decrypt_head(plain, decode_key)
}

fn tikhub_answer() -> Reply {
    Reply::json(
        200,
        &json!({
            "code": 200,
            "router": "/api/v1/wechat_channels/v2/fetch_video_detail",
            "data": {"id": OBJECT_ID, "media": {
                "url": VIDEO_URL.split("&token=").next().expect("a URL"),
                "url_token": "&token=video-token",
                "full_url": VIDEO_URL,
                "decode_key": DECODE_KEY.to_string(),
            }},
        }),
    )
}

/// `_video_web`.
fn video_sites(video: Option<Reply>, tikhub: Option<Reply>) -> Sites {
    Sites::with(vec![
        (COVER_URL.replace("&amp;", "&"), jpeg()),
        (TIKHUB_DETAIL.to_owned(), tikhub.unwrap_or_else(tikhub_answer)),
        (
            VIDEO_URL.to_owned(),
            video.unwrap_or_else(|| image_reply(&channels_encrypted(MP4, DECODE_KEY), "video/mp4")),
        ),
    ])
}

fn parsed_video(content: &str) -> ChannelVideo {
    parse_channel_video(content).expect("parses").expect("a video")
}

fn tikhub_requests(env: &Env) -> Vec<Seen> {
    env.web_requests().into_iter().filter(|seen| seen.url == TIKHUB_DETAIL).collect()
}

// py: tests/test_wechat.py::test_a_channels_video_share_names_its_feed
#[test]
fn a_channels_video_share_names_its_feed() {
    let video = parsed_video(&default_finder());

    assert_eq!((video.object_id.as_str(), video.nonce_id.as_str()), (OBJECT_ID, NONCE_ID));
    assert_eq!(
        (video.author.as_str(), video.duration, video.width, video.height),
        ("jiang57l", 9, 1080, 1920)
    );
    assert_eq!(video.cover_url, COVER_URL.replace("&amp;", "&"));
    assert_eq!(video_title(&video), "镜头之外才是故事真正的开始 偶遇着他们的幸福#徐绿杨");
    assert_eq!(work(&event(1, video_message(65))), Some("channel_video"));
    assert_eq!(
        queued_message(&object(&video_message(65)), "channel_video").get("content"),
        Some(&Value::Str(default_finder()))
    );
}

// py: tests/test_wechat.py::test_channels_image_feeds_lives_and_broken_ids_are_not_videos
#[test]
fn channels_image_feeds_lives_and_broken_ids_are_not_videos() {
    let none = |content: &str| parse_channel_video(content).expect("parses").is_none();
    assert!(none(&finder_share("2", "0", OBJECT_ID)));
    assert!(none(&finder_share("4", "2054", OBJECT_ID)));
    assert!(none(&finder_share("4", "0", "99999999999999999999")));
    assert!(none(&share(SHARE_URL, "5")));
}

// py: tests/test_wechat.py::test_the_keystream_decrypts_a_known_video_header
#[test]
fn the_keystream_decrypts_a_known_video_header() {
    // The first bytes of an encrypted file and its decode_key, as published with
    // github.com/Evil0ctal/WeChat-Channels-Video-File-Decryption.
    let head = b"\x23\x76\x6a\x16\xff\x8f\xfe\x1a\x1c\xa6\xcd\x5f\x99\x48\x46\xab";

    assert_eq!(decrypt_head(head, DECODE_KEY), b"\x00\x00\x00\x20ftypisom\x00\x00\x02\x00");
    assert_eq!(keystream(DECODE_KEY, ENCRYPTED_BYTES).len(), ENCRYPTED_BYTES);
}

// py: tests/test_wechat.py::test_only_the_start_of_a_video_is_encrypted
#[test]
fn only_the_start_of_a_video_is_encrypted() {
    let plain: Vec<u8> = (0..600).flat_map(|_| 0..=255_u8).collect();

    let encrypted = channels_encrypted(&plain, DECODE_KEY);

    assert_eq!(encrypted[ENCRYPTED_BYTES..], plain[ENCRYPTED_BYTES..]);
    assert_ne!(encrypted[..ENCRYPTED_BYTES], plain[..ENCRYPTED_BYTES]);
    assert_eq!(decrypt_head(&encrypted, DECODE_KEY), plain);
    assert_ne!(decrypt_head(&encrypted, DECODE_KEY + 1), plain);
}

// py: tests/test_wechat.py::test_tikhub_is_asked_by_object_id_with_a_numeric_nonce_only
#[test]
fn tikhub_is_asked_by_object_id_with_a_numeric_nonce_only() {
    let video = parsed_video(&default_finder());
    let numeric = parsed_video(&default_finder().replace(NONCE_ID, "11875607166103288523"));

    assert_eq!(
        Value::Dict(tikhub_lookup(&video)),
        value(&json!({"object_id": OBJECT_ID, "raw": false}))
    );
    assert_eq!(
        Value::Dict(tikhub_lookup(&numeric)),
        value(&json!({"object_id": OBJECT_ID, "raw": false, "object_nonce_id": "11875607166103288523"}))
    );
}

// py: tests/test_wechat.py::test_a_tikhub_answer_names_the_file_and_its_key
#[test]
fn a_tikhub_answer_names_the_file_and_its_key() {
    let source = |payload: Json| parse_video_source(&value(&payload)).expect("parses");
    let Reply::Respond { body, .. } = tikhub_answer() else {
        panic!("an answer")
    };
    let answer: Json = serde_json::from_slice(&body).expect("JSON");
    let joined = json!({"data": {"media": {"url": "https://finder.video.qq.com/x?a=1", "url_token": "&token=t", "decode_key": "42"}}});
    let nested = json!({"data": {"data": {"media": [{"full_url": VIDEO_URL, "decode_key": "7"}]}}});

    let found = source(answer).expect("a source");
    assert_eq!((found.url.as_str(), found.decode_key), (VIDEO_URL, DECODE_KEY));
    assert_eq!(source(joined).expect("a source").url, "https://finder.video.qq.com/x?a=1&token=t");
    assert_eq!(source(nested).expect("a source").decode_key, 7);
    assert!(source(json!({"data": {"media": {"full_url": VIDEO_URL, "decode_key": ""}}})).is_none());
    assert!(source(json!({"data": null})).is_none());
    assert!(source(json!([])).is_none());
}

fn urlsafe_b64(data: &[u8]) -> String {
    b64encode(data).replace('+', "-").replace('/', "_").trim_end_matches('=').to_owned()
}

// py: tests/test_wechat.py::test_the_best_offered_encoding_is_asked_for
#[test]
fn the_best_offered_encoding_is_asked_for() {
    // basedata as the CDN signs it: the default spec, then each spec on offer.
    let offered =
        b"\x08\x03\x12\x06xWT113\"6\n\n\n\x06xWT113\x10\x00\n\n\n\x06xWT112\x10\x01\n\n\n\x06xWT111\x10\x00\n\x07\n\x03xA0\x10\x00";
    let url = format!(
        "https://finder.video.qq.com/251/20302/stodownload?encfilekey=enc&basedata={}&sign=s",
        urlsafe_b64(offered)
    );
    let answer = json!({"data": {"media": {"full_url": url, "decode_key": "7"}}});

    assert_eq!(best_quality(&url).expect("a URL"), format!("{url}&X-snsvideoflag=xWT111"));
    assert_eq!(
        parse_video_source(&value(&answer)).expect("parses").expect("a source").url,
        format!("{url}&X-snsvideoflag=xWT111")
    );
    let chosen = format!("{url}&X-snsvideoflag=xWT113");
    assert_eq!(best_quality(&chosen).expect("a URL"), chosen);
    assert_eq!(best_quality(VIDEO_URL).expect("a URL"), VIDEO_URL);
    let broken = format!("{VIDEO_URL}&basedata=%%%");
    assert_eq!(best_quality(&broken).expect("a URL"), broken);
}

fn video_stem() -> &'static str {
    "2025-10-05 031320 镜头之外才是故事真正的开始 偶遇着他们的幸福#徐绿杨 [65]"
}

// py: tests/test_wechat.py::test_a_channels_video_is_saved_with_its_cover
#[tokio::test]
async fn a_channels_video_is_saved_with_its_cover() {
    let mut env = Env::new(Inbox::bridge(vec![event(1, video_message(65))]), video_sites(None, None)).await;
    env.tikhub("tikhub-key");

    env.run_ok().await;

    let requests = tikhub_requests(&env);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].header("authorization"), Some("Bearer tikhub-key"));
    assert_eq!(
        serde_json::from_slice::<Json>(&requests[0].body).expect("JSON"),
        json!({"object_id": OBJECT_ID, "raw": false})
    );
    // WeChat itself never opens the video.
    assert!(
        !env.inbox_requests()
            .iter()
            .any(|seen| seen.method == "POST" && !seen.path().starts_with("/ack/"))
    );
    let month = env.path().join("2025-10");
    assert_eq!(std::fs::read(month.join(format!("{}.mp4", video_stem()))).expect("saved"), MP4);
    assert_eq!(std::fs::read(month.join(format!("{}.jpg", video_stem()))).expect("saved"), JPEG);
    assert!(!env.files().iter().any(|file| file.ends_with(".part")));
    let rows = env.videos().await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    let title = video_title(&parsed_video(&default_finder()));
    assert_eq!(
        (
            row.object_id.as_str(),
            row.title.as_str(),
            row.author.as_str(),
            row.duration,
            row.size,
            row.local_path.clone()
        ),
        (
            OBJECT_ID,
            title.as_str(),
            "jiang57l",
            9,
            i64::try_from(MP4.len()).expect("small"),
            format!("2025-10/{}.mp4", video_stem())
        )
    );
    assert_eq!(row.metadata.get("nonce_id"), Some(&Value::from(NONCE_ID)));
    assert_eq!(env.status(65).await, "done");
    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].title, title);
    assert_eq!(sent[0].body, "jiang57l | 9s | 0.0 MB | From wxid_sender");
    assert_eq!(
        payload(&sent[0], "image_path"),
        &Value::Str(path_text(&month.join(format!("{}.jpg", video_stem()))))
    );
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_second_forward_of_a_saved_video_is_not_fetched_again
#[tokio::test]
async fn a_second_forward_of_a_saved_video_is_not_fetched_again() {
    let mut env = Env::new(
        Inbox::bridge(vec![event(1, video_message(65)), event(2, video_message(70))]),
        video_sites(None, None),
    )
    .await;
    env.tikhub("tikhub-key");

    env.run_ok().await;

    assert_eq!(tikhub_requests(&env).len(), 1);
    assert_eq!((env.status(65).await, env.status(70).await), ("done".to_owned(), "done".to_owned()));
    assert_eq!(env.notifier.sent().len(), 1);
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_video_waits_without_using_its_tries_until_a_tikhub_key_is_set
#[tokio::test]
async fn a_video_waits_without_using_its_tries_until_a_tikhub_key_is_set() {
    let env = Env::new(Inbox::bridge(vec![event(1, video_message(65))]), video_sites(None, None)).await;

    let error = env.run_err().await;

    assert!(error.message().contains("TikHub API key"), "{}", error.message());
    let queued = env.queue(65).await;
    assert_eq!((queued.status.as_str(), queued.attempts), ("pending", 0));
    assert!(tikhub_requests(&env).is_empty());
    assert!(env.files().is_empty());
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_refused_tikhub_key_does_not_use_up_tries_but_a_tikhub_error_does
#[tokio::test]
async fn a_refused_tikhub_key_does_not_use_up_tries_but_a_tikhub_error_does() {
    let refused = Reply::json(402, &json!({"detail": "insufficient balance"}));
    let mut env = Env::new(Inbox::bridge(vec![event(1, video_message(65))]), video_sites(None, Some(refused))).await;
    env.tikhub("tikhub-key");

    let error = env.run_err().await;
    assert!(error.message().contains("check the TikHub key and balance"), "{}", error.message());
    assert_eq!(env.queue(65).await.attempts, 0);

    env.sites()
        .responses
        .insert(TIKHUB_DETAIL.to_owned(), Reply::json(500, &json!({"detail": "upstream timeout"})));
    let error = env.run_err().await;
    assert!(error.message().contains("HTTP 500"), "{}", error.message());
    let queued = env.queue(65).await;
    assert_eq!((queued.status.as_str(), queued.attempts), ("pending", 1));
    env.finish().await;
}

// py: tests/test_wechat.py::test_an_answer_without_a_file_is_retried
#[tokio::test]
async fn an_answer_without_a_file_is_retried() {
    let empty = Reply::json(200, &json!({"code": 200, "data": {}}));
    let mut env = Env::new(Inbox::bridge(vec![event(1, video_message(65))]), video_sites(None, Some(empty))).await;
    env.tikhub("tikhub-key");

    let error = env.run_err().await;

    assert!(error.message().contains("names no video file"), "{}", error.message());
    assert_eq!(env.status(65).await, "pending");
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_video_that_does_not_decrypt_to_an_mp4_is_not_kept
#[tokio::test]
async fn a_video_that_does_not_decrypt_to_an_mp4_is_not_kept() {
    let wrong = image_reply(&channels_encrypted(MP4, DECODE_KEY + 1), "video/mp4");
    let mut env = Env::new(Inbox::bridge(vec![event(1, video_message(65))]), video_sites(Some(wrong), None)).await;
    env.tikhub("tikhub-key");

    let error = env.run_err().await;

    assert!(error.message().contains("not an MP4 once decrypted"), "{}", error.message());
    assert_eq!(env.status(65).await, "pending");
    assert!(env.files().is_empty());
    assert!(env.videos().await.is_empty());
    env.finish().await;
}

// ---------- waiting out a TikHub outage (D185: not in the Python baseline) ----------

/// What TikHub answers every lookup with while one of its APIs is down.
fn tikhub_request_failed() -> Reply {
    Reply::json(
        400,
        &json!({"detail": {"code": 400, "message": "Request failed. Please retry. Check the docs below to verify your parameters."}}),
    )
}

const ZHIHU_TITLE: &str = "高跟鞋配白丝真的很难看吗？";

fn zhihu_share_message(msg_id: i64) -> Json {
    let content = share(ZHIHU_SHARE_URL, "5").replace("<title>每日分享</title>", &format!("<title>{ZHIHU_TITLE}</title>"));
    app_message(msg_id, &content)
}

fn zhihu_answer() -> Reply {
    Reply::json(
        200,
        &json!({
            "data": {
                "content": format!("<figure><img data-original=\"{}\"/></figure>", zhihu_pic("A")),
                "author": {"name": "知乎用户"},
                "question": {"title": ZHIHU_TITLE},
                "created_time": 1_793_533_212,
            },
        }),
    )
}

fn link_opens(env: &Env) -> usize {
    env.inbox_requests().iter().filter(|seen| seen.path() == "/link/open").count()
}

#[tokio::test]
async fn a_zhihu_link_waits_out_a_tikhub_outage_for_a_day_before_it_is_given_up() {
    let sites = Sites::with(vec![(ZHIHU_TIKHUB.to_owned(), tikhub_request_failed())]);
    let mut env = Env::new(Inbox::bridge(vec![event(5, zhihu_share_message(64))]), sites).await;
    env.tikhub("tikhub-key");

    // The tries any item gets fail the run, as before.
    for run in 1..MAX_ATTEMPTS {
        let error = env.run_err().await;
        assert_eq!(error.notification_dedupe_key(), Some("wechat:work"));
        assert!(error.message().contains("TikHub is failing for"), "{}", error.message());
        assert_eq!(i64::from(env.queue(64).await.attempts), run);
    }
    assert!(env.notifier.sent().is_empty());

    // The try an unreadable post is given up on: the link stays queued, the run does not
    // fail over it, and the outage is reported.
    env.run_ok().await;
    let queued = env.queue(64).await;
    assert_eq!((queued.status.as_str(), i64::from(queued.attempts)), ("pending", MAX_ATTEMPTS));
    assert!(queued.last_error.contains("Request failed. Please retry"), "{}", queued.last_error);
    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (
            sent[0].kind.as_str(),
            sent[0].dedupe_key.as_str(),
            sent[0].header.as_str(),
            sent[0].title.as_str()
        ),
        ("download_failed", "tikhub:outage:zhihu", "WeChat", ZHIHU_TITLE)
    );
    assert!(
        sent[0].body.starts_with("Waiting out an outage: links it stops"),
        "{}",
        sent[0].body
    );

    // Later runs try it again without reporting again, and without opening it in WeChat
    // once more for every try.
    env.run_ok().await;
    assert_eq!(i64::from(env.queue(64).await.attempts), MAX_ATTEMPTS + 1);
    assert_eq!(env.notifier.sent().len(), 1);
    assert_eq!(link_opens(&env), usize::try_from(MAX_ATTEMPTS).expect("small"));

    // A day of tries later it is given up like any other.
    env.set_attempts(64, i32::try_from(OUTAGE_MAX_ATTEMPTS - 1).expect("small")).await;
    let error = env.run_err().await;
    assert!(error.message().contains("article 64 (gave up)"), "{}", error.message());
    let queued = env.queue(64).await;
    assert_eq!(
        (queued.status.as_str(), i64::from(queued.attempts)),
        ("failed", OUTAGE_MAX_ATTEMPTS)
    );
    env.finish().await;
}

#[tokio::test]
async fn a_zhihu_link_saved_after_an_outage_closes_its_notification() {
    let sites = Sites::with(vec![(ZHIHU_TIKHUB.to_owned(), tikhub_request_failed()), (zhihu_pic("A"), jpeg())]);
    let mut env = Env::new(Inbox::bridge(vec![event(5, zhihu_share_message(64))]), sites).await;
    env.tikhub("tikhub-key");
    env.run_err().await;
    env.set_attempts(64, i32::try_from(MAX_ATTEMPTS + 2).expect("small")).await;
    env.run_ok().await;
    assert_eq!(env.notifier.sent().len(), 1);
    assert!(env.notifier.resolved().is_empty());

    // TikHub answers again.
    env.sites().responses.insert(ZHIHU_TIKHUB.to_owned(), zhihu_answer());
    env.run_ok().await;

    assert_eq!(env.status(64).await, "done");
    let resolved = env.notifier.resolved();
    let (_, resolution) = resolved.iter().find(|(key, _)| key == "tikhub:outage:zhihu").expect("closed");
    assert_eq!(
        (resolution.kind.as_str(), resolution.header.as_str(), resolution.title.as_str()),
        ("job_recovered", "WeChat", ZHIHU_TITLE)
    );
    assert_eq!(resolution.body, "The outage is over: this link was saved.");
    env.finish().await;
}

#[tokio::test]
async fn a_zhihu_link_saved_with_no_outage_open_resolves_nothing() {
    let sites = Sites::with(vec![(ZHIHU_TIKHUB.to_owned(), zhihu_answer()), (zhihu_pic("A"), jpeg())]);
    let mut env = Env::new(Inbox::bridge(vec![event(5, zhihu_share_message(64))]), sites).await;
    env.tikhub("tikhub-key");

    env.run_ok().await;

    assert_eq!(env.status(64).await, "done");
    assert!(env.notifier.resolved().iter().all(|(key, _)| !key.starts_with("tikhub:outage")));
    env.finish().await;
}

#[tokio::test]
async fn a_zhihu_post_tikhub_cannot_read_is_given_up_at_its_fifth_try_whatever_came_before() {
    let gone = Reply::json(200, &json!({"data": {}}));
    let sites = Sites::with(vec![(ZHIHU_TIKHUB.to_owned(), gone)]);
    let mut env = Env::new(Inbox::bridge(vec![event(5, zhihu_share_message(64))]), sites).await;
    env.tikhub("tikhub-key");
    env.run_err().await;
    // As a link is left that waited out an outage for a while.
    env.set_attempts(64, i32::try_from(MAX_ATTEMPTS + 6).expect("small")).await;

    let error = env.run_err().await;

    assert!(error.message().contains("article 64 (gave up)"), "{}", error.message());
    assert_eq!(env.status(64).await, "failed");
    assert!(env.notifier.sent().is_empty());
    env.finish().await;
}

#[tokio::test]
async fn a_video_waits_out_a_tikhub_outage_and_closes_it_once_saved() {
    let mut env = Env::new(
        Inbox::bridge(vec![event(1, video_message(65))]),
        video_sites(None, Some(tikhub_request_failed())),
    )
    .await;
    env.tikhub("tikhub-key");
    let error = env.run_err().await;
    assert!(
        error.message().contains("HTTP 400: Request failed. Please retry"),
        "{}",
        error.message()
    );
    env.set_attempts(65, i32::try_from(MAX_ATTEMPTS - 1).expect("small")).await;

    // TikHub's wrapper, then a server error, then no answer at all: each is TikHub's failing.
    env.run_ok().await;
    env.sites()
        .responses
        .insert(TIKHUB_DETAIL.to_owned(), Reply::json(502, &json!({"detail": "bad gateway"})));
    env.run_ok().await;
    env.sites().responses.insert(TIKHUB_DETAIL.to_owned(), Reply::Reset);
    env.run_ok().await;

    let queued = env.queue(65).await;
    assert_eq!((queued.status.as_str(), i64::from(queued.attempts)), ("pending", MAX_ATTEMPTS + 2));
    let sent = env.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].kind.as_str(), sent[0].dedupe_key.as_str(), sent[0].header.as_str()),
        ("download_failed", "tikhub:outage:channels", "WeChat")
    );
    assert_eq!(sent[0].title, video_title(&parsed_video(&default_finder())));
    assert!(
        sent[0].body.starts_with("Waiting out an outage: videos it stops"),
        "{}",
        sent[0].body
    );
    assert!(env.files().is_empty());

    env.sites().responses.insert(TIKHUB_DETAIL.to_owned(), tikhub_answer());
    env.run_ok().await;

    assert_eq!(env.status(65).await, "done");
    let resolved = env.notifier.resolved();
    let (_, resolution) = resolved.iter().find(|(key, _)| key == "tikhub:outage:channels").expect("closed");
    assert_eq!(resolution.body, "The outage is over: this video was saved.");
    env.finish().await;
}

#[tokio::test]
async fn a_video_tikhub_answers_about_is_given_up_at_its_fifth_try() {
    let not_found = Reply::json(404, &json!({"detail": {"code": 404, "message": "video not found"}}));
    let mut env = Env::new(Inbox::bridge(vec![event(1, video_message(65))]), video_sites(None, Some(not_found))).await;
    env.tikhub("tikhub-key");
    env.run_err().await;
    env.set_attempts(65, i32::try_from(MAX_ATTEMPTS - 1).expect("small")).await;

    let error = env.run_err().await;

    assert!(error.message().contains("channel_video 65 (gave up)"), "{}", error.message());
    assert_eq!(env.status(65).await, "failed");
    assert!(env.notifier.sent().is_empty());
    env.finish().await;
}

// ---------- registration ----------

// py: tests/test_wechat.py::test_the_job_is_registered_and_parked_until_configured
#[tokio::test]
async fn the_job_is_registered_and_parked_until_configured() {
    let mut settings = Settings::default();
    settings.web.wechat.schedule.enabled = true;

    let jobs = fav_runtime::jobs::build_jobs(&settings);
    let job = jobs.iter().find(|job| job.key == "wechat").expect("a wechat job");

    assert_eq!(job.name, "WeChat");
    assert_eq!(job.section, "web.wechat");
    assert!(job.required_commands.is_empty());
    let db = Db::connect_lazy("postgresql://tests@127.0.0.1:1/unused").expect("lazy pool");
    let ctx = crate::SourceContext {
        settings: Arc::new(SettingsStore::new(db.clone())),
        db,
    };
    assert!(crate::registry(&ctx).contains(JOB_KEY));
    assert!(!job.enabled);
    assert!(job.missing_fields.iter().any(|field| field == "token"));
}

// ---------- unsupported messages ----------

// py: tests/test_wechat.py::test_unsupported_reason_names_the_link_or_the_message
#[test]
fn unsupported_reason_names_the_link_or_the_message() {
    let reason = |message: Json| unsupported_reason(&object(&message)).expect("a reason");

    assert_eq!(
        reason(message(1, json!({"kind": "text", "content": "see https://example.com/a"}))),
        "Link not supported: https://example.com/a"
    );
    assert_eq!(
        reason(message(2, json!({"kind": "text", "content": "hello"}))),
        "Nothing to download: a text message: hello"
    );
    assert_eq!(
        reason(message(3, json!({"kind": "app", "content": share("https://example.com/p/1", "5")}))),
        "Link not supported: https://example.com/p/1"
    );
    assert_eq!(
        reason(message(4, json!({"kind": "app", "content": share(SHARE_URL, "6")}))),
        "Nothing to download: a WeChat app message: 每日分享"
    );
    assert_eq!(
        reason(message(5, json!({"kind": "video", "content": ""}))),
        "Nothing to download: a WeChat video message"
    );
}

// py: tests/test_wechat.py::test_messages_with_nothing_to_save_are_reported_from_now_on
#[tokio::test]
async fn messages_with_nothing_to_save_are_reported_from_now_on() {
    let now = Utc::now().timestamp_millis();
    let events = vec![
        // From before reporting began: the event log's history stays quiet.
        event(1, message(50, json!({"kind": "text", "content": "old note"}))),
        event(
            2,
            message(
                51,
                json!({"kind": "text", "content": "see https://example.com/a", "create_time": now + 60_000}),
            ),
        ),
    ];
    let env = Env::new(Inbox::bridge(events), Sites::default()).await;

    env.run_ok().await;

    assert!(env.queued(50).await.is_none());
    let status: String = sqlx::query("SELECT status FROM wechat_events WHERE msg_id = 51 AND create_time = $1")
        .bind(now + 60_000)
        .fetch_one(env.database.db().pool())
        .await
        .expect("queued")
        .get("status");
    // The report stays open: the row waits to be checked again on later runs.
    assert_eq!(status, "reported");
    let sent: Vec<(String, String, String)> = env.notifier.sent().into_iter().map(|n| (n.kind, n.body, n.dedupe_key)).collect();
    assert_eq!(
        sent,
        [(
            "unsupported_message".to_owned(),
            "Link not supported: https://example.com/a | Message ID 51".to_owned(),
            format!("wechat:unsupported:51:{}", now + 60_000)
        )]
    );
    env.finish().await;
}

// py: tests/test_wechat.py::test_a_reported_message_is_saved_once_supported_and_its_report_resolved
#[tokio::test]
async fn a_reported_message_is_saved_once_supported_and_its_report_resolved() {
    let env = Env::new(Inbox::bridge(Vec::new()), tieba_sites(Vec::new())).await;
    // Reported by an earlier version that did not read Tieba links yet.
    let tieba = tieba_message();
    env.set_state("events_after", "9").await;
    env.set_state("work_version", "8").await;
    env.set_state("unsupported_since", "0").await;
    let kept = queued_message(&object(&tieba), "unsupported");
    let kept: Json = serde_json::from_str(&pyjson::dumps(&Value::Dict(kept), &DumpOptions::plain()).expect("JSON")).expect("JSON");
    env.enqueue(63, "unsupported", 5, &kept, "reported").await;

    env.run_ok().await;

    let queued = env.queue(63).await;
    assert_eq!((queued.work.as_str(), queued.status.as_str()), ("article", "done"));
    let resolved: Vec<String> = env.notifier.resolved().into_iter().map(|(key, _)| key).collect();
    assert_eq!(resolved, [format!("wechat:unsupported:63:{}", CREATE_TIME + 63)]);
    env.finish().await;
}

// ---------- transport failures ----------

/// The first half of `content` under a `content-length` naming all of it.
fn cut_short(content: &[u8]) -> Reply {
    Reply::Truncated {
        status: 200,
        sent: content[..content.len() / 2].to_vec(),
        length: content.len(),
    }
}

#[tokio::test]
async fn an_inbox_that_hangs_up_fails_the_run_as_unreachable() {
    let mut inbox = Inbox::listing(vec![inbox_item(1, json!({}))]);
    inbox.overrides.insert("/items".to_owned(), Reply::Reset);
    let env = Env::new(inbox, Sites::default()).await;

    let error = env.run_err().await;

    assert_eq!(error.notification_dedupe_key(), Some("wechat:unreachable"));
    assert!(error.message().contains("is unreachable"), "{}", error.message());
    assert!(env.image_ids().await.is_empty());
    assert!(env.inbox().acked.is_empty());
    env.finish().await;
}

#[tokio::test]
async fn an_image_cut_short_is_left_pending_and_reported() {
    let mut inbox = Inbox::listing(vec![inbox_item(1, json!({})), inbox_item(2, json!({}))]);
    inbox.overrides.insert("/file/1".to_owned(), cut_short(JPEG));
    let env = Env::new(inbox, Sites::default()).await;

    let error = env.run_err().await;

    assert_eq!(error.notification_dedupe_key(), Some("wechat:download"));
    assert_eq!(env.image_ids().await, [2]);
    assert_eq!(env.inbox().acked, [2]);
    assert_eq!(env.files().len(), 1, "{:?}", env.files());
    env.finish().await;
}

#[tokio::test]
async fn a_link_open_that_hangs_up_is_retried_without_being_fetched() {
    let inbox = Inbox {
        link_open: Some(Reply::Close),
        ..Inbox::bridge(vec![event(5, article_message(44))])
    };
    let env = Env::new(inbox, article_sites(Vec::new())).await;

    let error = env.run_err().await;

    assert!(error.message().contains("could not open the link in WeChat"), "{}", error.message());
    assert!(env.web_requests().is_empty());
    let queued = env.queue(44).await;
    assert_eq!((queued.status.as_str(), queued.attempts), ("pending", 1));
    assert_eq!(env.entries(), 0);
    env.finish().await;
}

#[tokio::test]
async fn a_tikhub_that_hangs_up_uses_up_a_try() {
    let mut env = Env::new(
        Inbox::bridge(vec![event(1, video_message(65))]),
        video_sites(None, Some(Reply::Reset)),
    )
    .await;
    env.tikhub("tikhub-key");

    let error = env.run_err().await;

    assert!(error.message().contains(OBJECT_ID), "{}", error.message());
    let queued = env.queue(65).await;
    assert_eq!((queued.status.as_str(), queued.attempts), ("pending", 1));
    assert!(env.files().is_empty());
    env.finish().await;
}

#[tokio::test]
async fn a_channels_video_cut_short_leaves_no_partial_file() {
    let video = cut_short(&channels_encrypted(MP4, DECODE_KEY));
    let mut env = Env::new(Inbox::bridge(vec![event(1, video_message(65))]), video_sites(Some(video), None)).await;
    env.tikhub("tikhub-key");

    let error = env.run_err().await;

    assert!(error.message().contains("Could not download 视频号 video"), "{}", error.message());
    let queued = env.queue(65).await;
    assert_eq!((queued.status.as_str(), queued.attempts), ("pending", 1));
    assert!(env.files().is_empty(), "{:?}", env.files());
    assert!(env.videos().await.is_empty());
    assert!(env.notifier.sent().is_empty());
    env.finish().await;
}

#[tokio::test]
async fn a_tieba_video_cut_short_is_retried_without_leaving_a_file() {
    let env = Env::new(
        Inbox::bridge(vec![event(5, tieba_video_message())]),
        tieba_video_sites(true, Some(cut_short(MP4))),
    )
    .await;

    env.run_err().await;

    assert!(env.articles().await.is_empty());
    let queued = env.queue(64).await;
    assert_eq!(queued.status, "pending");
    assert!(env.files().is_empty(), "{:?}", env.files());
    assert!(env.notifier.sent().is_empty());
    env.finish().await;
}

#[tokio::test]
async fn an_article_picture_cut_short_is_skipped_like_one_that_will_not_load() {
    let env = Env::new(
        Inbox::bridge(vec![event(5, article_message(44))]),
        article_sites(vec![(original(IMAGE_B), cut_short(JPEG))]),
    )
    .await;

    env.run_ok().await;

    assert_eq!(env.status(44).await, "done");
    assert_eq!(env.web_urls().last().map(String::as_str), Some(IMAGE_B));
    assert_eq!(env.article().await.image_count, 1);
    assert!(!env.files().iter().any(|file| file.ends_with(".part")));
    env.finish().await;
}

// D140: a token that cannot be a header value fails when the clients are built, unnamed.
#[test]
fn a_token_that_cannot_be_sent_fails_without_naming_it() {
    for token in ["tökén-secret", "line\nbreak-secret"] {
        let error = Clients::new(token, None, &[]).expect_err("refused").to_string();
        assert_eq!(error, "Invalid header authorization");
        assert!(!error.contains("secret"));
    }
}

#[test]
fn the_clients_never_show_the_token() {
    let clients = Clients::new("plain-secret", None, &[]).expect("clients");

    let shown = format!("{clients:?}");

    assert!(shown.contains("authorization"), "{shown}");
    assert!(!shown.contains("plain-secret"), "{shown}");
}

// D140: httpx refuses a TikHub key it cannot encode before sending, outside `HTTPError`.
#[tokio::test]
async fn a_tikhub_key_that_is_not_ascii_fails_the_run_uncounted() {
    let mut env = Env::new(Inbox::bridge(vec![event(1, video_message(65))]), video_sites(None, None)).await;
    env.tikhub("kéy");

    let error = env.run_err().await;

    assert_eq!(error.class_name(), "UnicodeEncodeError");
    assert_eq!(
        error.message(),
        "'ascii' codec can't encode character '\\xe9' in position 8: ordinal not in range(128)"
    );
    assert!(tikhub_requests(&env).is_empty());
    assert_eq!(env.queue(65).await.attempts, 0);
    // The run stops there: the inbox is not drained.
    assert!(!env.inbox_requests().iter().any(|seen| seen.path() == "/items"));
    env.finish().await;
}

#[test]
fn an_unencodable_run_is_named_by_its_positions() {
    let message = |text: &str| super::ascii_encode_error(text).map(|error| error.message);
    assert_eq!(message("Bearer key"), None);
    assert_eq!(
        message("Bearer 中文x😀").as_deref(),
        Some("'ascii' codec can't encode characters in position 7-8: ordinal not in range(128)")
    );
    assert_eq!(
        message("Bearer x😀").as_deref(),
        Some("'ascii' codec can't encode character '\\U0001f600' in position 8: ordinal not in range(128)")
    );
    assert_eq!(
        message("Bearer x中").as_deref(),
        Some("'ascii' codec can't encode character '\\u4e2d' in position 8: ordinal not in range(128)")
    );
}
