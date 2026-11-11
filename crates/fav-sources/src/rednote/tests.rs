//! Ported from `tests/test_rednote.py` and `tests/test_rednote_missing_db.py`.
//!
//! Python drives the source with a scripted browser and a fake database that records
//! SQL; here the browser is scripted the same way ([`FakeBrowser`]) and the database is
//! a real scratch PostgreSQL, so assertions read the tables instead of the statements.
//! Downloads go to the fake web through its proxy. Nothing reaches RedNote.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use fav_browser::driver::DriverError;
use fav_core::golden;
use fav_core::logging::Level;
use fav_core::settings::Settings;
use fav_core::validate::Model as _;
use fav_db::testdb::TestDatabase;
use serde_json::json;

use super::*;
use crate::testkit::RecordingNotifier;
use crate::testutil::{FakeWeb, Reply, test_roots};

mod parsers;

// 2025-08-12 11:34:56 in the timezone the app displays.
const NOTE_TIME_MS: i64 = 1_754_969_696_000;
const NOTE_ID: &str = "64f1a2b3000000001e02c0de";
const OTHER_NOTE_ID: &str = "64f1a2b3000000001e02beef";
const FILE_NAME: &str = "[artist]2025-08-12 [64f1a2b3000000001e02c0de_1].webp";

// ---------- payloads ----------

#[allow(clippy::needless_pass_by_value)] // reason: called with `json!` literals throughout
fn value(json: serde_json::Value) -> Value {
    Value::from_serde(&json)
}

fn dict(json: serde_json::Value) -> Dict {
    match value(json) {
        Value::Dict(dict) => dict,
        other => panic!("not an object: {other:?}"),
    }
}

/// `payload.update(updates)`.
fn merged(mut base: serde_json::Value, updates: serde_json::Value) -> serde_json::Value {
    if let (Some(base), serde_json::Value::Object(updates)) = (base.as_object_mut(), updates) {
        base.extend(updates);
    }
    base
}

fn image_note_card(updates: serde_json::Value) -> serde_json::Value {
    merged(
        json!({
            "type": "normal",
            "title": "a caption",
            "time": NOTE_TIME_MS,
            "user": {"nickname": "artist", "user_id": "5ff0000000000000000fffff"},
            "image_list": [{
                "info_list": [
                    {"image_scene": "WB_PRV", "url": "https://sns-webpic.xhscdn.com/1!nd_prv_wlteh_webp_3"},
                    {"image_scene": "WB_DFT", "url": "https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3"},
                ],
            }],
        }),
        updates,
    )
}

/// The same note as `__INITIAL_STATE__` spells it: camelCase, http URLs.
fn page_note_card(updates: serde_json::Value) -> serde_json::Value {
    merged(
        json!({
            "type": "normal",
            "title": "a caption",
            "time": NOTE_TIME_MS,
            "user": {"nickname": "artist", "userId": "5ff0000000000000000fffff"},
            "imageList": [{
                "infoList": [
                    {"imageScene": "WB_PRV", "url": "http://sns-webpic.xhscdn.com/1!nd_prv_wlteh_webp_3"},
                    {"imageScene": "WB_DFT", "url": "http://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3"},
                ],
                "urlDefault": "http://sns-webpic.xhscdn.com/1!nd_dft_fallback",
                "livePhoto": false,
                "stream": {},
            }],
        }),
        updates,
    )
}

fn video_note_card(updates: serde_json::Value) -> serde_json::Value {
    merged(
        json!({
            "type": "video",
            "title": "a clip",
            "time": NOTE_TIME_MS,
            "user": {"nickname": "artist", "user_id": "5ff0000000000000000fffff"},
            // A video note also carries a cover image; it is deliberately not archived.
            "image_list": [{"info_list": [{"image_scene": "WB_DFT", "url": "https://sns-webpic.xhscdn.com/cover!nd_dft"}]}],
            // Opaque, versioned bucket names, each variant stating its own resolution.
            "video": {
                "media": {
                    "stream": {
                        "EF4": [
                            {"videoCodec": "EF4", "width": 720, "height": 1280, "videoBitrate": 3_000_000,
                             "masterUrl": "https://sns-video.xhscdn.com/stream/259_720p.mp4"},
                            {"videoCodec": "EF4", "width": 1080, "height": 1920, "videoBitrate": 5_000_000,
                             "masterUrl": "https://sns-video.xhscdn.com/stream/261_1080p.mp4"},
                        ],
                        "EF6": [],
                        "EF5": [
                            {"videoCodec": "EF5", "width": 1080, "height": 1920, "videoBitrate": 1_700_000,
                             "masterUrl": "https://sns-video.xhscdn.com/stream/301_1080p.mp4"},
                            {"videoCodec": "EF5", "width": 2160, "height": 3840, "videoBitrate": 9_000_000,
                             "masterUrl": "https://sns-video.xhscdn.com/stream/109_2160p.mp4"},
                        ],
                    },
                },
                // The upload's own resolution, a JSON string as the page renders it.
                "mediaV2": "{\"video\": {\"width\": 2160, \"height\": 3840}}",
            },
        }),
        updates,
    )
}

fn media() -> RedNoteMedia {
    RedNoteMedia {
        note_id: NOTE_ID.to_owned(),
        media_index: 1,
        media_type: "image".to_owned(),
        media_url: "https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3".to_owned(),
        title: "a caption".to_owned(),
        author: "artist".to_owned(),
        author_id: "5ff0000000000000000fffff".to_owned(),
        note_type: "normal".to_owned(),
        published_at: "2025-08-12 11:34:56".to_owned(),
        xsec_token: "TOKEN".to_owned(),
        last_error: String::new(),
    }
}

#[allow(clippy::needless_pass_by_value)] // reason: called with `json!` literals
fn envelope(data: serde_json::Value, code: i64) -> Value {
    value(json!({"code": code, "success": code == 0, "msg": "ok", "data": data}))
}

#[allow(clippy::needless_pass_by_value)] // reason: called with `json!` literals
fn like_page(notes: serde_json::Value, cursor: &str, has_more: bool, status: i64) -> CapturedPage {
    CapturedPage {
        url: format!("https://edith.xiaohongshu.com/api/sns/web/v1/note/like/page?user_id=me&num=30&cursor={cursor}"),
        status,
        body: Some(envelope(json!({"notes": notes, "cursor": cursor, "has_more": has_more}), 0)),
    }
}

fn notes(ids: &[&str]) -> serde_json::Value {
    serde_json::Value::Array(ids.iter().map(|id| json!({"note_id": id, "xsec_token": "T"})).collect())
}

fn note_refs(ids: &[&str]) -> Vec<NoteRef> {
    ids.iter()
        .map(|id| NoteRef {
            note_id: (*id).to_owned(),
            xsec_token: "T".to_owned(),
        })
        .collect()
}

fn probe(json: serde_json::Value) -> Map<String, serde_json::Value> {
    match json {
        serde_json::Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn navigation_failed() -> NoteBrowserError {
    NoteBrowserError::Driver(DriverError::Navigation("navigation failed".to_owned()))
}

// ---------- the scripted browser ----------

/// What a test reads back about the browser after the source is done with it.
#[derive(Debug, Default)]
struct BrowserLog {
    started: bool,
    closed: bool,
    opened_likes: Vec<String>,
    reloads: u32,
    note_urls: Vec<String>,
}

type NoteReader = Box<dyn FnMut(&str) -> Result<Value, NoteBrowserError> + Send>;

/// The `NoteBrowser` protocol, scripted. No Chromium, no network.
struct FakeBrowser {
    probes: VecDeque<Map<String, serde_json::Value>>,
    pages: VecDeque<CapturedPage>,
    initial_notes: Vec<Value>,
    notes: HashMap<String, Value>,
    reader: Option<NoteReader>,
    reload_fails: bool,
    open_likes_fails: bool,
    log: Arc<Mutex<BrowserLog>>,
}

impl FakeBrowser {
    fn new() -> Self {
        Self {
            probes: VecDeque::from([probe(json!({"logged_in": true, "user_id": "me"}))]),
            pages: VecDeque::new(),
            initial_notes: Vec::new(),
            notes: HashMap::new(),
            reader: None,
            reload_fails: false,
            open_likes_fails: false,
            log: Arc::default(),
        }
    }

    fn probes(mut self, probes: Vec<serde_json::Value>) -> Self {
        self.probes = probes.into_iter().map(probe).collect();
        self
    }

    fn pages(mut self, pages: Vec<CapturedPage>) -> Self {
        self.pages = pages.into();
        self
    }

    fn initial(mut self, notes: serde_json::Value) -> Self {
        let Value::List(notes) = value(notes) else { panic!("a list") };
        self.initial_notes = notes;
        self
    }

    fn note(mut self, note_id: &str, card: serde_json::Value) -> Self {
        self.notes.insert(note_id.to_owned(), value(card));
        self
    }

    fn reader(mut self, reader: impl FnMut(&str) -> Result<Value, NoteBrowserError> + Send + 'static) -> Self {
        self.reader = Some(Box::new(reader));
        self
    }

    fn log(&self) -> Arc<Mutex<BrowserLog>> {
        Arc::clone(&self.log)
    }
}

fn ready<'a, T: Send + 'a>(value: T) -> fav_runtime::BoxFuture<'a, T> {
    Box::pin(async move { value })
}

impl NoteBrowser for FakeBrowser {
    fn start(&mut self) -> fav_runtime::BoxFuture<'_, Result<(), NoteBrowserError>> {
        self.log.lock().expect("log").started = true;
        ready(Ok(()))
    }

    fn aclose(&mut self) -> fav_runtime::BoxFuture<'_, ()> {
        self.log.lock().expect("log").closed = true;
        ready(())
    }

    fn probe_login(&mut self) -> fav_runtime::BoxFuture<'_, Result<Map<String, serde_json::Value>, NoteBrowserError>> {
        let probe = if self.probes.len() == 1 {
            self.probes[0].clone()
        } else {
            self.probes.pop_front().unwrap_or_default()
        };
        ready(Ok(probe))
    }

    fn cookies(&mut self) -> fav_runtime::BoxFuture<'_, Result<Vec<serde_json::Value>, NoteBrowserError>> {
        ready(Ok(vec![json!({
            "name": "web_session", "value": "session-value", "domain": ".xiaohongshu.com", "path": "/", "expires": 1_900_000_000,
        })]))
    }

    fn cookie_dict(&mut self) -> fav_runtime::BoxFuture<'_, Result<HashMap<String, String>, NoteBrowserError>> {
        ready(Ok(HashMap::from([("web_session".to_owned(), "session-value".to_owned())])))
    }

    fn user_agent(&mut self) -> fav_runtime::BoxFuture<'_, Result<String, NoteBrowserError>> {
        ready(Ok("Mozilla/5.0 (Test) Chrome/149.0.0.0".to_owned()))
    }

    fn reload_login(&mut self) -> fav_runtime::BoxFuture<'_, Result<(), NoteBrowserError>> {
        self.log.lock().expect("log").reloads += 1;
        ready(if self.reload_fails {
            Err(NoteBrowserError::Driver(DriverError::Navigation(
                "net::ERR_ABORTED at https://www.xiaohongshu.com/".to_owned(),
            )))
        } else {
            Ok(())
        })
    }

    fn open_likes<'a>(&'a mut self, user_id: &'a str) -> fav_runtime::BoxFuture<'a, Result<bool, NoteBrowserError>> {
        if self.open_likes_fails {
            return ready(Err(navigation_failed()));
        }
        self.log.lock().expect("log").opened_likes.push(user_id.to_owned());
        ready(Ok(true))
    }

    fn initial_like_notes(&mut self) -> fav_runtime::BoxFuture<'_, Result<Vec<Value>, NoteBrowserError>> {
        ready(Ok(self.initial_notes.clone()))
    }

    fn next_like_page(&mut self, _timeout: Duration) -> fav_runtime::BoxFuture<'_, Result<Option<CapturedPage>, NoteBrowserError>> {
        ready(Ok(self.pages.pop_front()))
    }

    fn note_state<'a>(&'a mut self, note_url: &'a str, note_id: &'a str) -> fav_runtime::BoxFuture<'a, Result<Value, NoteBrowserError>> {
        self.log.lock().expect("log").note_urls.push(note_url.to_owned());
        let state = match &mut self.reader {
            Some(reader) => reader(note_id),
            None => Ok(self
                .notes
                .get(note_id)
                .cloned()
                .unwrap_or_else(|| value(image_note_card(json!({}))))),
        };
        ready(state)
    }
}

// ---------- the job ----------

fn unused_db() -> Db {
    Db::connect_lazy("postgresql://tests@127.0.0.1:1/unused").expect("lazy pool")
}

/// `_runnable_cfg`: no pacing, a proxy, and whatever the test overrides.
fn config(overrides: serde_json::Value) -> RedNoteConfig {
    let payload = merged(
        json!({"sleep_request_seconds": 0.0, "proxy": "http://home.example:3128"}),
        overrides,
    );
    RedNoteConfig::validate(&value(payload)).expect("valid settings")
}

fn settings_of(overrides: serde_json::Value) -> RedNoteConfig {
    RedNoteConfig::validate(&value(overrides)).expect("valid settings")
}

/// A fake web that refuses nothing and records everything; `answer` decides the body.
async fn cdn(answer: impl Fn(&str) -> Reply + Send + Sync + 'static) -> FakeWeb {
    FakeWeb::start(move |seen| answer(&seen.url)).await
}

fn client_through(web: &FakeWeb, cfg: &RedNoteConfig) -> HttpClient {
    HttpClient::new(&ClientOptions {
        proxy: Some(web.proxy.clone()),
        roots: test_roots(),
        ..client_options(cfg)
    })
    .expect("client")
}

struct Harness {
    job: RedNote,
    notifier: Arc<RecordingNotifier>,
}

/// `_job`: a source over `db` whose factory hands out `browser` (a default fake after it).
fn job(db: &Db, cfg: RedNoteConfig, client: HttpClient, browser: FakeBrowser) -> Harness {
    let notifier = Arc::new(RecordingNotifier::default());
    let slot = Mutex::new(Some(browser));
    let factory: BrowserFactory = Box::new(move || {
        let browser = slot.lock().expect("slot").take().unwrap_or_else(FakeBrowser::new);
        Ok(Box::new(browser) as Box<dyn NoteBrowser>)
    });
    let mut job = RedNote::new(db.clone(), Arc::clone(&notifier) as SharedNotifier, cfg, client, factory);
    job.set_user_id("me");
    Harness { job, notifier }
}

/// A job that must not reach the CDN.
async fn offline_job(db: &Db, cfg: RedNoteConfig) -> (RedNote, FakeWeb) {
    let web = cdn(|_| Reply::status(599)).await;
    let client = client_through(&web, &cfg);
    (job(db, cfg, client, FakeBrowser::new()).job, web)
}

#[derive(Debug, PartialEq, Eq)]
struct Row {
    note_id: String,
    media_index: i32,
    media_type: String,
    media_url: String,
    downloaded: i32,
    unavailable: i32,
    failed_count: i32,
    last_error: String,
    local_path: String,
}

async fn rows(db: &Db) -> Vec<Row> {
    sqlx::query(
        "SELECT note_id, media_index, media_type, media_url, downloaded, unavailable, failed_count, last_error, local_path
         FROM rednote ORDER BY created_at, note_id, media_index;",
    )
    .fetch_all(db.pool())
    .await
    .expect("rows")
    .iter()
    .map(|row| Row {
        note_id: row.get("note_id"),
        media_index: row.get("media_index"),
        media_type: row.get("media_type"),
        media_url: row.get("media_url"),
        downloaded: row.get("downloaded"),
        unavailable: row.get("unavailable"),
        failed_count: row.get("failed_count"),
        last_error: row.get("last_error"),
        local_path: row.get("local_path"),
    })
    .collect()
}

/// `_resolved_note_ids`: the notes that gained rows, in the order they were written.
async fn resolved_note_ids(db: &Db) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for row in rows(db).await {
        if !ids.contains(&row.note_id) {
            ids.push(row.note_id);
        }
    }
    ids
}

async fn state(db: &Db, key: &str) -> Option<String> {
    sqlx::query_scalar("SELECT value FROM rednote_state WHERE key = $1;")
        .bind(key)
        .fetch_optional(db.pool())
        .await
        .expect("state")
}

async fn mark_backfill_done(db: &Db) {
    sqlx::query("INSERT INTO rednote_state (key, value) VALUES ('backfill_complete', '1');")
        .execute(db.pool())
        .await
        .expect("state");
}

/// Rows for notes already archived (`known_note_ids` of the Python fake).
async fn archive(db: &Db, note_ids: &[&str]) {
    for note_id in note_ids {
        sqlx::query("INSERT INTO rednote (note_id, media_index, media_type, downloaded) VALUES ($1, 1, 'image', 1);")
            .bind(note_id)
            .execute(db.pool())
            .await
            .expect("row");
    }
}

async fn pending_row(db: &Db, note_id: &str, media_index: i32, media_url: &str, last_error: &str) {
    sqlx::query(
        "INSERT INTO rednote (note_id, media_index, media_type, media_url, title, author, note_type, published_at, xsec_token, last_error)
         VALUES ($1, $2, 'image', $3, 'a caption', 'artist', 'normal', '2025-08-12 11:34:56', 'TOKEN', $4);",
    )
    .bind(note_id)
    .bind(media_index)
    .bind(media_url)
    .bind(last_error)
    .execute(db.pool())
    .await
    .expect("row");
}

async fn missing_runs(db: &Db, note_id: &str) -> i32 {
    sqlx::query_scalar("SELECT runs FROM rednote_missing WHERE note_id = $1;")
        .bind(note_id)
        .fetch_optional(db.pool())
        .await
        .expect("runs")
        .unwrap_or(0)
}

fn ids(set: &HashSet<String>) -> Vec<&str> {
    let mut ids: Vec<&str> = set.iter().map(String::as_str).collect();
    ids.sort_unstable();
    ids
}

/// Run a body on a current-thread runtime, so every record its tasks log is captured.
fn with_captured_log(body: impl std::future::Future<Output = ()>) -> Vec<logging::Record> {
    logging::capture(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(body);
    })
}

// ---------- configuration ----------

// py: tests/test_rednote.py::test_a_datacenter_run_has_to_be_asked_for
#[test]
fn a_datacenter_run_has_to_be_asked_for() {
    // Going out over the pod's own address cost the account its sessions once, so it is
    // a decision rather than a default.
    assert_eq!(settings_of(json!({"proxy": ""})).validate_runnable(), ["proxy"]);
    assert!(
        settings_of(json!({"proxy": "", "allow_direct_connection": true}))
            .validate_runnable()
            .is_empty()
    );
    assert!(
        settings_of(json!({"proxy": "http://home.example:3128"}))
            .validate_runnable()
            .is_empty()
    );
}

// py: tests/test_rednote.py::test_an_empty_video_path_is_not_read_as_the_working_directory
#[test]
fn an_empty_video_path_is_not_read_as_the_working_directory() {
    assert_eq!(settings_of(json!({"video_path": ""})).video_path, None);
    assert_eq!(settings_of(json!({"video_path": "   "})).video_path, None);
}

// py: tests/test_rednote.py::test_the_request_interval_cannot_be_negative
#[test]
fn the_request_interval_cannot_be_negative() {
    let error = RedNoteConfig::validate(&value(json!({"sleep_request_seconds": -1}))).expect_err("rejected");
    assert!(error.to_string().contains("sleep_request_seconds"), "{error}");
}

// py: tests/test_rednote.py::test_the_page_limits_must_leave_room_for_one_page
#[test]
fn the_page_limits_must_leave_room_for_one_page() {
    for field in ["abort_after", "max_pages_per_run"] {
        let error = RedNoteConfig::validate(&value(json!({ field: 0 }))).expect_err("rejected");
        assert!(error.to_string().contains(field), "{error}");
    }
}

// py: tests/test_rednote.py::test_the_profile_defaults_onto_the_persistent_directory
#[test]
fn the_profile_defaults_onto_the_persistent_directory() {
    // ./data is the convention for state that has to survive a restart.
    assert_eq!(settings_of(json!({})).profile_path, PyPath::new("./data/rednote-profile"));
}

// py: tests/test_rednote.py::test_xhshow_is_gone_and_should_stay_gone
#[test]
fn xhshow_is_gone_and_should_stay_gone() {
    // The signing library is what the browser rewrite removed: reaching for it again
    // means requests from outside the browser, the shape that got the account flagged.
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock")).expect("the workspace lock file");
    assert!(!lock.contains("xhshow"));
}

// ---------- login ----------

// py: tests/test_rednote.py::test_the_login_modal_outranks_a_stale_cookie
#[test]
fn the_login_modal_outranks_a_stale_cookie() {
    // web_session survives the account revoking the session elsewhere.
    let jar = |value: &str| HashMap::from([("web_session".to_owned(), value.to_owned())]);
    assert_eq!(
        decide_login_state(&probe(json!({"has_login_modal": true})), &jar("still-here")),
        LOGGED_OUT
    );
    assert_eq!(decide_login_state(&probe(json!({"logged_in": true})), &jar("v")), LOGGED_IN);
    // The page's own answer outranks even a missing cookie.
    assert_eq!(decide_login_state(&probe(json!({"logged_in": true})), &HashMap::new()), LOGGED_IN);
    assert_eq!(decide_login_state(&probe(json!({"logged_in": false})), &HashMap::new()), LOGGED_OUT);
}

// py: tests/test_rednote.py::test_a_page_that_has_not_hydrated_yet_is_not_called_signed_out
#[test]
fn a_page_that_has_not_hydrated_yet_is_not_called_signed_out() {
    let jar = HashMap::from([("web_session".to_owned(), "v".to_owned())]);
    assert_eq!(decide_login_state(&probe(json!({})), &jar), LOGIN_UNKNOWN);
}

// py: tests/test_rednote.py::test_a_signed_out_profile_fails_the_run_at_once_and_sends_no_qr
#[tokio::test]
async fn a_signed_out_profile_fails_the_run_at_once_and_sends_no_qr() {
    // Signing in is the 登录态 page's job; a run only says it is needed.
    let qr = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let mut browser = FakeBrowser::new().probes(vec![json!({"has_login_modal": true, "qr_src": qr})]);
    let log = browser.log();
    let (mut job, _web) = offline_job(&unused_db(), config(json!({}))).await;

    let error = job.check_login(&mut browser).await.expect_err("signed out");

    assert_eq!(error.class_name(), "RedNoteError");
    assert_eq!(error.notification_dedupe_key(), Some("rednote:login"));
    assert!(error.message().contains("登录态"), "{}", error.message());
    // A visible login modal is an answer; reloading it would only cost a page load.
    assert_eq!(log.lock().expect("log").reloads, 0);
}

// py: tests/test_rednote.py::test_a_page_whose_snapshot_predates_the_session_is_reloaded_once
#[tokio::test]
async fn a_page_whose_snapshot_predates_the_session_is_reloaded_once() {
    let stale = json!({"logged_in": false, "has_login_modal": false, "qr_src": ""});
    let mut browser = FakeBrowser::new().probes(vec![stale, json!({"logged_in": true, "user_id": "me"})]);
    let log = browser.log();
    let (mut job, _web) = offline_job(&unused_db(), config(json!({}))).await;
    job.set_user_id("");

    job.check_login(&mut browser).await.expect("signed in after the reload");

    assert_eq!(log.lock().expect("log").reloads, 1);
    assert_eq!(job.user_id(), "me");
}

// py: tests/test_rednote.py::test_a_page_that_never_answers_is_reported_as_signed_out
#[tokio::test]
async fn a_page_that_never_answers_is_reported_as_signed_out() {
    let stale = json!({"logged_in": false, "has_login_modal": false, "qr_src": ""});
    let mut browser = FakeBrowser::new().probes(vec![stale]);
    let log = browser.log();
    let (mut job, _web) = offline_job(&unused_db(), config(json!({}))).await;

    let error = job.check_login(&mut browser).await.expect_err("signed out");

    assert_eq!(error.class_name(), "RedNoteError");
    assert_eq!(log.lock().expect("log").reloads, 1);
}

// py: tests/test_rednote.py::test_a_reload_the_site_interrupts_does_not_end_the_run
#[tokio::test]
async fn a_reload_the_site_interrupts_does_not_end_the_run() {
    let mut browser = FakeBrowser::new().probes(vec![json!({}), json!({"logged_in": true, "user_id": "me"})]);
    browser.reload_fails = true;
    let log = browser.log();
    let (mut job, _web) = offline_job(&unused_db(), config(json!({}))).await;

    job.check_login(&mut browser).await.expect("signed in");

    assert_eq!(log.lock().expect("log").reloads, 1);
}

// py: tests/test_rednote.py::test_the_first_signed_in_run_writes_the_login_page_row
#[tokio::test]
async fn the_first_signed_in_run_writes_the_login_page_row() {
    // A profile signed in before the 登录态 page existed has no row; the page would call
    // it 未登录 although every run works.
    let database = crate::testutil::database().await;
    let db = database.db();
    let mut browser = FakeBrowser::new();
    let (mut job, _web) = offline_job(db, config(json!({}))).await;

    job.prepare_session(&mut browser).await.expect("prepared");

    let session = site_sessions::load(db, "rednote").await.expect("load").expect("a row");
    assert_eq!(session.account, "me");
    assert_eq!(
        session.cookies.iter().map(|cookie| cookie.name.as_str()).collect::<Vec<_>>(),
        ["web_session"]
    );
    // Names and expiry for the page; the credential itself stays in the profile.
    assert_eq!(session.cookies[0].value, "in-profile");
    assert!(session.expires_at.is_some());
    assert_eq!((session.status.as_str(), session.message.as_str()), ("valid", ""));
    assert_eq!(state(db, USER_ID_STATE_KEY).await.as_deref(), Some("me"));
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_later_runs_only_record_the_check
#[tokio::test]
async fn later_runs_only_record_the_check() {
    let database = crate::testutil::database().await;
    let db = database.db();
    site_sessions::save_signed_in(db, "rednote", &[], "earlier", None, &[])
        .await
        .expect("row");
    let mut browser = FakeBrowser::new();
    let (mut job, _web) = offline_job(db, config(json!({}))).await;

    job.prepare_session(&mut browser).await.expect("prepared");

    let session = site_sessions::load(db, "rednote").await.expect("load").expect("a row");
    assert_eq!(session.account, "earlier");
    assert!(session.cookies.is_empty());
    assert_eq!((session.status.as_str(), session.message.as_str()), ("valid", ""));
    assert!(session.checked_at.is_some());
    database.drop_database().await;
}

/// A run over `browser`, with a stored 登录态 row to see what it records.
async fn run_with_session(browser: FakeBrowser) -> (JobError, fav_runtime::site_sessions::SiteSession) {
    let database = crate::testutil::database().await;
    let db = database.db().clone();
    let root = tempfile::tempdir().expect("tmp");
    site_sessions::save_signed_in(&db, "rednote", &[], "me", None, &[])
        .await
        .expect("row");
    // Unchecked, so a check this run records shows.
    sqlx::query("UPDATE site_sessions SET checked_at = NULL;")
        .execute(db.pool())
        .await
        .expect("reset");
    let cfg = config(json!({"path": root.path().join("files"), "profile_path": root.path().join("profile")}));
    let web = cdn(|_| Reply::status(599)).await;
    let client = client_through(&web, &cfg);
    let mut harness = job(&db, cfg, client, browser);

    let error = harness.job.update().await.expect_err("the run fails");

    let session = site_sessions::load(&db, "rednote").await.expect("load").expect("a row");
    database.drop_database().await;
    (error, session)
}

// py: tests/test_rednote.py::test_an_unreadable_profile_id_does_not_mark_the_login_invalid
#[tokio::test]
async fn an_unreadable_profile_id_does_not_mark_the_login_invalid() {
    let (error, session) = run_with_session(FakeBrowser::new().probes(vec![json!({"logged_in": true, "user_id": ""})])).await;

    assert_eq!(error.notification_dedupe_key(), Some("rednote:user_id"));
    assert_eq!(session.checked_at, None);
    assert_eq!(session.status, "valid");
}

// py: tests/test_rednote.py::test_a_signed_out_run_marks_the_login_invalid
#[tokio::test]
async fn a_signed_out_run_marks_the_login_invalid() {
    let (error, session) = run_with_session(FakeBrowser::new().probes(vec![json!({"has_login_modal": true})])).await;

    assert_eq!(error.class_name(), "RedNoteError");
    assert_eq!(session.status, "invalid");
    assert!(session.message.contains("登录态"), "{}", session.message);
}

// ---------- reading the likes list ----------

// py: tests/test_rednote.py::test_a_likes_page_yields_its_notes_with_the_cursor_for_the_next_one
#[test]
fn a_likes_page_yields_its_notes_with_the_cursor_for_the_next_one() {
    let (notes, cursor, has_more) = extract_like_page(&dict(json!({
        "notes": [
            {"note_id": NOTE_ID, "xsec_token": "TOKEN", "type": "normal"},
            {"id": OTHER_NOTE_ID, "xsec_token": "OTHER"},
        ],
        "cursor": "CURSOR-2",
        "has_more": true,
    })));

    assert_eq!(
        notes.iter().map(|note| note.note_id.as_str()).collect::<Vec<_>>(),
        [NOTE_ID, OTHER_NOTE_ID]
    );
    assert_eq!(notes[0].xsec_token, "TOKEN");
    assert_eq!(cursor, "CURSOR-2");
    assert!(has_more);
}

// py: tests/test_rednote.py::test_an_entry_without_an_id_is_skipped_rather_than_crashing_the_page
#[test]
fn an_entry_without_an_id_is_skipped_rather_than_crashing_the_page() {
    let (notes, cursor, has_more) = extract_like_page(&dict(json!({"notes": [{"xsec_token": "T"}, "not-a-dict", {"note_id": NOTE_ID}]})));

    assert_eq!(notes.iter().map(|note| note.note_id.as_str()).collect::<Vec<_>>(), [NOTE_ID]);
    assert_eq!(cursor, "");
    assert!(!has_more);
}

// py: tests/test_rednote.py::test_a_signed_out_response_is_reported_as_a_login_failure
#[test]
fn a_signed_out_response_is_reported_as_a_login_failure() {
    let error = parse_api_envelope(Some(&envelope(json!({}), -101))).expect_err("signed out");
    assert_eq!(error.class_name(), "RedNoteError");
    assert_eq!(error.notification_dedupe_key(), Some("rednote:login"));
}

// py: tests/test_rednote.py::test_any_other_error_code_is_an_ordinary_failure
#[test]
fn any_other_error_code_is_an_ordinary_failure() {
    assert_eq!(
        parse_api_envelope(Some(&envelope(json!({"notes": []}), 0))).expect("data"),
        dict(json!({"notes": []}))
    );
    let error = parse_api_envelope(Some(&envelope(json!({}), -510_001))).expect_err("an error code");
    assert_eq!(error.class_name(), "ValueError");
    assert!(error.message().contains("-510001"), "{}", error.message());
    let error = parse_api_envelope(Some(&Value::from("not an object"))).expect_err("not an object");
    assert_eq!(error.class_name(), "TypeError");
}

// ---------- reading one note ----------

fn extract(card: serde_json::Value, token: &str) -> Vec<RedNoteMedia> {
    extract_note_media(&dict(card), NOTE_ID, token).expect("parsed")
}

// py: tests/test_rednote.py::test_an_image_note_prefers_the_full_size_variant
#[test]
fn an_image_note_prefers_the_full_size_variant() {
    let media = extract(image_note_card(json!({})), "TOKEN");

    assert_eq!(media.len(), 1);
    assert_eq!(media[0].media_type, "image");
    assert!(media[0].media_url.ends_with("nd_dft_wlteh_webp_3"));
    assert_eq!(media[0].media_index, 1);
    assert_eq!(media[0].author, "artist");
    assert_eq!(media[0].published_at, "2025-08-12 11:34:56");
    assert_eq!(media[0].xsec_token, "TOKEN");
}

// py: tests/test_rednote.py::test_the_page_state_parses_to_exactly_what_the_api_did
#[test]
fn the_page_state_parses_to_exactly_what_the_api_did() {
    // Same note, same rows: moving to the browser did not shift `media_index`, which is
    // what `refresh_stale_media` looks rows up by.
    assert_eq!(extract(page_note_card(json!({})), "T"), extract(image_note_card(json!({})), "T"));
}

// py: tests/test_rednote.py::test_the_page_state_url_is_upgraded_to_https
#[test]
fn the_page_state_url_is_upgraded_to_https() {
    assert_eq!(
        normalize_media_url("http://sns-webpic.xhscdn.com/x").expect("url"),
        "https://sns-webpic.xhscdn.com/x"
    );
    assert!(extract(page_note_card(json!({})), "T")[0].media_url.starts_with("https://"));
}

// py: tests/test_rednote.py::test_an_image_with_only_a_default_url_still_yields_a_file
#[test]
fn an_image_with_only_a_default_url_still_yields_a_file() {
    let card = page_note_card(json!({"imageList": [{"urlDefault": "http://cdn/only-default"}]}));
    let urls: Vec<String> = extract(card, "T").into_iter().map(|item| item.media_url).collect();
    assert_eq!(urls, ["https://cdn/only-default"]);
}

// py: tests/test_rednote.py::test_a_live_photo_is_kept_as_both_the_still_and_the_clip
#[test]
fn a_live_photo_is_kept_as_both_the_still_and_the_clip() {
    let card = image_note_card(json!({"image_list": [
        {"info_list": [{"image_scene": "WB_DFT", "url": "https://cdn/plain-1"}]},
        {
            "info_list": [{"image_scene": "WB_DFT", "url": "https://cdn/live-still"}],
            // Opaque buckets, most of them empty, and a variant naming no codec at all.
            "stream": {
                "EF6": [], "EF5": [], "EF7": [],
                "EF4": [{"masterUrl": "https://cdn/live-clip.mp4", "backupUrls": ["https://cdn/backup.mp4"], "qualityType": "HD"}],
            },
        },
        {"info_list": [{"image_scene": "WB_DFT", "url": "https://cdn/plain-2"}]},
    ]}));

    let media = extract(card, "T");

    // Indices are handed out in file order, the clip counting as its own file.
    let order: Vec<(i64, &str)> = media.iter().map(|item| (item.media_index, item.media_type.as_str())).collect();
    assert_eq!(order, [(1, "image"), (2, "image"), (3, "live"), (4, "image")]);
    assert_eq!(media[2].media_url, "https://cdn/live-clip.mp4");
}

// py: tests/test_rednote.py::test_a_video_note_is_one_file_and_its_cover_is_not_archived
// py: tests/test_rednote.py::test_the_4k_stream_outranks_the_smaller_ones_beside_it
#[test]
fn a_video_note_is_one_file_and_its_cover_is_not_archived() {
    let media = extract(video_note_card(json!({})), "T");

    let order: Vec<(i64, &str)> = media.iter().map(|item| (item.media_index, item.media_type.as_str())).collect();
    assert_eq!(order, [(1, "video")]);
    // The 4K variant, not one of the smaller ones the web player streams.
    assert_eq!(media[0].media_url, "https://sns-video.xhscdn.com/stream/109_2160p.mp4");
}

// py: tests/test_rednote.py::test_the_highest_resolution_stream_is_picked_across_all_buckets
#[test]
fn the_highest_resolution_stream_is_picked_across_all_buckets() {
    // Codec names are no basis to rank on (they were renamed once); resolution is.
    let card = value(video_note_card(json!({})));
    let stream = card
        .as_dict()
        .and_then(|card| card.get("video"))
        .and_then(Value::as_dict)
        .and_then(|video| video.get("media"));
    let stream = stream.and_then(Value::as_dict).and_then(|media| media.get("stream"));
    assert!(pick_video_url(stream).expect("url").ends_with("109_2160p.mp4"));
}

// py: tests/test_rednote.py::test_a_stream_tie_on_resolution_breaks_to_the_higher_bitrate
#[test]
fn a_stream_tie_on_resolution_breaks_to_the_higher_bitrate() {
    let stream = value(json!({"EF5": [
        {"width": 1080, "height": 1920, "videoBitrate": 2_000_000, "masterUrl": "https://cdn/low.mp4"},
        {"width": 1080, "height": 1920, "videoBitrate": 6_000_000, "masterUrl": "https://cdn/high.mp4"},
    ]}));
    assert_eq!(pick_video_url(Some(&stream)).expect("url"), "https://cdn/high.mp4");
}

// py: tests/test_rednote.py::test_a_stream_that_states_no_resolution_still_yields_a_url
#[test]
fn a_stream_that_states_no_resolution_still_yields_a_url() {
    let unsized_card = video_note_card(json!({"video": {"media": {"stream": {"EF7": [{"masterUrl": "https://cdn/only.mp4"}]}}}}));
    assert_eq!(extract(unsized_card, "T")[0].media_url, "https://cdn/only.mp4");

    // The pre-rename shape still resolves, so a rollback of the site does not break it.
    let legacy = video_note_card(
        json!({"video": {"media": {"stream": {"h264": [{"width": 720, "height": 1280, "master_url": "https://cdn/legacy.mp4"}]}}}}),
    );
    assert_eq!(extract(legacy, "T")[0].media_url, "https://cdn/legacy.mp4");
}

// py: tests/test_rednote.py::test_a_video_note_yields_a_row_even_with_no_readable_stream
#[test]
fn a_video_note_yields_a_row_even_with_no_readable_stream() {
    let media = extract(video_note_card(json!({"video": {}})), "T");
    let rows: Vec<(i64, &str, &str)> = media
        .iter()
        .map(|item| (item.media_index, item.media_type.as_str(), item.media_url.as_str()))
        .collect();
    assert_eq!(rows, [(1, "video", "")]);
}

// py: tests/test_rednote.py::test_a_note_without_a_time_still_produces_rows
#[test]
fn a_note_without_a_time_still_produces_rows() {
    assert_eq!(extract(image_note_card(json!({"time": null})), "T")[0].published_at, "");
}

// py: tests/test_rednote.py::test_a_note_offered_only_smaller_streams_still_yields_its_best
#[test]
fn a_note_offered_only_smaller_streams_still_yields_its_best() {
    let capped = video_note_card(json!({"video": {"media": {"stream": {
        "EF4": [{"width": 720, "height": 1280, "masterUrl": "https://cdn/720.mp4"}],
        "EF5": [{"width": 1080, "height": 1920, "masterUrl": "https://cdn/1080.mp4"}],
    }}}}));
    assert_eq!(extract(capped, "T")[0].media_url, "https://cdn/1080.mp4");
}

// py: tests/test_rednote.py::test_the_upload_resolution_is_read_off_mediav2_in_either_spelling
#[test]
fn the_upload_resolution_is_read_off_mediav2_in_either_spelling() {
    let edge = |block: serde_json::Value| note_source_long_edge(Some(&value(block)));
    assert_eq!(edge(json!({"mediaV2": "{\"video\": {\"width\": 2160, \"height\": 3840}}"})), 3840);
    assert_eq!(edge(json!({"media_v2": {"video": {"width": 1080, "height": 1920}}})), 1920);
    // Nothing to measure against is not an error; it just disables the check.
    assert_eq!(edge(json!({})), 0);
    assert_eq!(edge(json!({"mediaV2": "not json"})), 0);
}

// py: tests/test_rednote.py::test_a_stream_short_of_the_upload_is_said_out_loud_once
#[tokio::test]
async fn a_stream_short_of_the_upload_is_said_out_loud_once() {
    let downscaled = dict(video_note_card(json!({"video": {
        // Only up to 1080p on offer, but the upload itself is 4K.
        "media": {"stream": {"EF5": [{"width": 1080, "height": 1920, "masterUrl": "https://cdn/1080.mp4"}]}},
        "mediaV2": "{\"video\": {\"width\": 2160, \"height\": 3840}}",
    }})));
    let full = dict(video_note_card(json!({})));
    let client = HttpClient::new(&ClientOptions::default()).expect("client");
    let mut harness = job(&unused_db(), config(json!({})), client, FakeBrowser::new());
    let records = logging::capture(|| {
        let media = extract_note_media(&downscaled, NOTE_ID, "T").expect("parsed");
        harness.job.warn_if_downscaled(&downscaled, &media).expect("checked");
        harness.job.warn_if_downscaled(&downscaled, &media).expect("checked");
    });
    let warnings: Vec<&logging::Record> = records.iter().filter(|record| record.level == Level::Warning).collect();
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].message.contains("high-res streams may be gone"),
        "{}",
        warnings[0].message
    );

    // A note whose best stream matches its upload says nothing at all.
    let client = HttpClient::new(&ClientOptions::default()).expect("client");
    let mut harness = job(&unused_db(), config(json!({})), client, FakeBrowser::new());
    let records = logging::capture(|| {
        let media = extract_note_media(&full, NOTE_ID, "T").expect("parsed");
        harness.job.warn_if_downscaled(&full, &media).expect("checked");
    });
    assert!(records.iter().all(|record| record.level < Level::Warning), "{records:?}");
}

// ---------- naming and links ----------

// py: tests/test_rednote.py::test_the_format_is_read_out_of_the_url_when_there_is_no_extension
#[test]
fn the_format_is_read_out_of_the_url_when_there_is_no_extension() {
    let ext = |url: &str| infer_image_extension(url).expect("ext");
    assert_eq!(ext("https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3"), "webp");
    // The host is `sns-webpic`, so reading the whole URL would call every image a webp.
    assert_eq!(ext("https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_avif_3"), "avif");
    assert_eq!(ext("https://sns-webpic.xhscdn.com/photo.jpeg"), "jpg");
    assert_eq!(ext("https://sns-webpic.xhscdn.com/opaque"), "jpg");
}

// py: tests/test_rednote.py::test_a_filename_carries_the_date_the_note_was_posted
#[test]
fn a_filename_carries_the_date_the_note_was_posted() {
    assert_eq!(build_media_filename(&media(), "").expect("name"), FILE_NAME);
}

// py: tests/test_rednote.py::test_videos_and_live_clips_are_named_as_mp4
#[test]
fn videos_and_live_clips_are_named_as_mp4() {
    let video = RedNoteMedia {
        media_type: "video".to_owned(),
        media_url: String::new(),
        ..media()
    };
    let live = RedNoteMedia {
        media_type: "live".to_owned(),
        media_index: 3,
        media_url: "https://cdn/x".to_owned(),
        ..media()
    };
    for media in [video, live] {
        let name = build_media_filename(&media, "").expect("name");
        assert_eq!(std::path::Path::new(&name).extension(), Some("mp4".as_ref()), "{name}");
    }
}

// py: tests/test_rednote.py::test_a_note_link_carries_the_token_that_makes_it_readable
#[test]
fn a_note_link_carries_the_token_that_makes_it_readable() {
    assert_eq!(
        build_note_url(NOTE_ID, "TOKEN"),
        format!("https://www.xiaohongshu.com/explore/{NOTE_ID}?xsec_token=TOKEN&xsec_source=pc_user")
    );
    assert_eq!(
        build_note_url(NOTE_ID, ""),
        format!("https://www.xiaohongshu.com/explore/{NOTE_ID}")
    );
}

// ---------- walking the likes list ----------

/// Walk the likes list of `browser` over a fresh database prepared by `setup`.
async fn crawl_with(
    browser: FakeBrowser,
    overrides: serde_json::Value,
    backfill_complete: bool,
    known: &[&str],
) -> (TestDatabase, Result<u64, JobError>) {
    let database = crate::testutil::database().await;
    let db = database.db().clone();
    if backfill_complete {
        mark_backfill_done(&db).await;
    }
    archive(&db, known).await;
    let (mut job, _web) = offline_job(&db, config(overrides)).await;
    let mut browser = browser;
    let result = job.crawl(&mut browser).await;
    (database, result)
}

// py: tests/test_rednote.py::test_the_newest_likes_arrive_with_the_page_and_are_not_skipped
#[tokio::test]
async fn the_newest_likes_arrive_with_the_page_and_are_not_skipped() {
    // The liked tab is served with its first screenful rendered into it, so a crawl
    // reading only requests would start below the newest likes, forever.
    let browser = FakeBrowser::new()
        .initial(json!([{"note_id": "newest-1", "xsec_token": "T1"}, {"note_id": "newest-2", "xsec_token": "T2"}]))
        .pages(vec![like_page(
            json!([{"note_id": "older-1", "xsec_token": "T3"}]),
            "c1",
            true,
            200,
        )]);
    let (database, result) = crawl_with(browser, json!({}), true, &[]).await;
    result.expect("walked");

    let archived = resolved_note_ids(database.db()).await;
    assert!(archived.contains(&"newest-1".to_owned()));
    assert!(archived.contains(&"newest-2".to_owned()));
    // And the requested pages are still walked after them.
    assert!(archived.contains(&"older-1".to_owned()));
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_only_the_site_saying_404_counts_toward_retiring_a_note
#[tokio::test]
async fn only_the_site_saying_404_counts_toward_retiring_a_note() {
    // A failure to load is not evidence: counting it would retire notes on a bad night.
    let database = crate::testutil::database().await;
    let db = database.db();
    let mut browser = FakeBrowser::new().reader(|note_id| match note_id {
        "deleted" => Err(NoteBrowserError::Gone("redirected to 404".to_owned())),
        "timed-out" => Err(NoteBrowserError::Driver(DriverError::Timeout(30_000))),
        _ => Ok(value(json!({}))), // read fine, carries nothing this source handles
    });
    let (mut job, _web) = offline_job(db, config(json!({}))).await;

    job.resolve_notes(&note_refs(&["deleted", "timed-out", "empty"]), &mut browser, "run-1")
        .await
        .expect("resolved");

    let missing: Vec<String> = sqlx::query_scalar("SELECT note_id FROM rednote_missing;")
        .fetch_all(db.pool())
        .await
        .expect("missing");
    assert_eq!(missing, ["deleted"]);
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_three_separate_runs_have_to_agree_before_a_note_is_retired
#[tokio::test]
async fn three_separate_runs_have_to_agree_before_a_note_is_retired() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let mut browser = FakeBrowser::new().reader(|_| Err(NoteBrowserError::Gone("redirected to 404".to_owned())));
    let (mut job, _web) = offline_job(db, config(json!({}))).await;
    let note = note_refs(&["deleted"]);
    let deleted = ["deleted".to_owned()];

    // Twice inside one run counts once: a note can come round twice in a single walk.
    job.resolve_notes(&note_refs(&["deleted", "deleted"]), &mut browser, "run-1")
        .await
        .expect("resolved");
    assert_eq!(missing_runs(db, "deleted").await, 1);
    assert!(job.retired_note_ids(&deleted).await.expect("retired").is_empty());

    job.resolve_notes(&note, &mut browser, "run-2").await.expect("resolved");
    assert!(job.retired_note_ids(&deleted).await.expect("retired").is_empty());

    job.resolve_notes(&note, &mut browser, "run-3").await.expect("resolved");
    assert_eq!(ids(&job.retired_note_ids(&deleted).await.expect("retired")), ["deleted"]);
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_a_note_that_reads_again_stops_being_counted_as_gone
#[tokio::test]
async fn a_note_that_reads_again_stops_being_counted_as_gone() {
    // Otherwise two unlucky runs plus one later hiccup retires a live note.
    let database = crate::testutil::database().await;
    let db = database.db();
    let gone = Arc::new(Mutex::new(true));
    let flag = Arc::clone(&gone);
    let mut browser = FakeBrowser::new().reader(move |_| {
        if *flag.lock().expect("flag") {
            Err(NoteBrowserError::Gone("redirected to 404".to_owned()))
        } else {
            Ok(value(image_note_card(json!({}))))
        }
    });
    let (mut job, _web) = offline_job(db, config(json!({}))).await;
    let note = note_refs(&["flaky"]);

    job.resolve_notes(&note, &mut browser, "run-1").await.expect("resolved");
    job.resolve_notes(&note, &mut browser, "run-2").await.expect("resolved");
    assert_eq!(missing_runs(db, "flaky").await, 2);

    *gone.lock().expect("flag") = false;
    job.resolve_notes(&note, &mut browser, "run-3").await.expect("resolved");

    assert_eq!(missing_runs(db, "flaky").await, 0);
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_a_retired_note_is_not_opened_again
#[tokio::test]
async fn a_retired_note_is_not_opened_again() {
    let database = crate::testutil::database().await;
    let db = database.db();
    sqlx::query("INSERT INTO rednote_missing (note_id, runs, last_run) VALUES ('retired', 3, 'run-3');")
        .execute(db.pool())
        .await
        .expect("missing");
    let mut browser = FakeBrowser::new().note("fresh", image_note_card(json!({})));
    let log = browser.log();
    let (mut job, _web) = offline_job(db, config(json!({}))).await;

    job.absorb_page(&note_refs(&["retired", "fresh"]), &mut browser, &mut HashSet::new(), 1, "run-4")
        .await
        .expect("absorbed");

    // The whole point of counting: it stops costing a page load on every run.
    let opened = log.lock().expect("log").note_urls.clone();
    assert!(opened.iter().all(|url| !url.contains("retired")), "{opened:?}");
    assert_eq!(opened.len(), 1);
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_a_note_that_can_never_be_read_does_not_hold_the_stop_rule_open
#[tokio::test]
async fn a_note_that_can_never_be_read_does_not_hold_the_stop_rule_open() {
    // What a page *added* decides the stop rule, so a note that never gains rows (deleted,
    // unliked while being looked at) cannot keep every incremental run walking the list.
    let browser = FakeBrowser::new()
        .initial(notes(&["gone", "archived-1"]))
        .pages(vec![
            like_page(notes(&["gone", "archived-1"]), "c1", true, 200),
            like_page(notes(&["archived-1"]), "c2", true, 200),
            like_page(notes(&["should-not-be-reached"]), "c3", true, 200),
        ])
        .note("gone", json!({}));
    let (database, result) = crawl_with(browser, json!({"abort_after": 2}), true, &["archived-1"]).await;
    result.expect("walked");

    assert!(!resolved_note_ids(database.db()).await.contains(&"should-not-be-reached".to_owned()));
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_the_crawl_stops_once_it_has_caught_up
#[tokio::test]
async fn the_crawl_stops_once_it_has_caught_up() {
    let browser = FakeBrowser::new().pages(vec![
        like_page(notes(&["new-1"]), "", true, 200),
        like_page(notes(&["old-1"]), "C1", true, 200),
        like_page(notes(&["old-2"]), "C2", true, 200),
        // Reaching this page would mean the stop rule never fired.
        like_page(notes(&["new-2"]), "C3", true, 200),
    ]);
    let (database, result) = crawl_with(browser, json!({"abort_after": 2}), true, &["old-1", "old-2"]).await;

    assert_eq!(result.expect("walked"), 1);
    assert_eq!(resolved_note_ids(database.db()).await, ["old-1", "old-2", "new-1"]);
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_a_page_with_anything_new_resets_the_stop_counter
#[tokio::test]
async fn a_page_with_anything_new_resets_the_stop_counter() {
    let browser = FakeBrowser::new().pages(vec![
        like_page(notes(&["old-1"]), "", true, 200),
        like_page(notes(&["new-1"]), "C1", true, 200),
        like_page(notes(&["old-2"]), "C2", true, 200),
        like_page(notes(&["old-3"]), "C3", false, 200),
    ]);
    let (database, result) = crawl_with(browser, json!({"abort_after": 2}), true, &["old-1", "old-2", "old-3"]).await;

    assert_eq!(result.expect("walked"), 1);
    assert_eq!(resolved_note_ids(database.db()).await, ["old-1", "old-2", "old-3", "new-1"]);
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_the_page_the_site_fetched_twice_is_only_counted_once
#[tokio::test]
async fn the_page_the_site_fetched_twice_is_only_counted_once() {
    // The site's own client sometimes fires the same request twice per scroll; counted,
    // the repeat would satisfy the stop rule a page early.
    let browser = FakeBrowser::new().pages(vec![
        like_page(notes(&["old-1"]), "C1", true, 200),
        like_page(notes(&["old-1"]), "C1", true, 200),
        like_page(notes(&["old-2"]), "C2", true, 200),
        like_page(notes(&["new-1"]), "C3", false, 200),
    ]);
    let (database, result) = crawl_with(browser, json!({"abort_after": 3}), true, &["old-1", "old-2"]).await;

    assert_eq!(result.expect("walked"), 1);
    assert_eq!(resolved_note_ids(database.db()).await, ["old-1", "old-2", "new-1"]);
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_the_first_run_walks_past_notes_it_has_already_stored
#[tokio::test]
async fn the_first_run_walks_past_notes_it_has_already_stored() {
    // A backfill that died part-way leaves an archived prefix; stopping on it would put
    // everything below out of reach for good.
    let browser = FakeBrowser::new().pages(vec![
        like_page(notes(&["old-1"]), "", true, 200),
        like_page(notes(&["old-2"]), "C1", true, 200),
        like_page(notes(&["old-3"]), "C2", true, 200),
        like_page(notes(&["new-1"]), "C3", false, 200),
    ]);
    let (database, result) = crawl_with(browser, json!({"abort_after": 2}), false, &["old-1", "old-2", "old-3"]).await;

    assert_eq!(result.expect("walked"), 1);
    assert!(resolved_note_ids(database.db()).await.contains(&"new-1".to_owned()));
    assert_eq!(state(database.db(), BACKFILL_STATE_KEY).await.as_deref(), Some("1"));
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_a_walk_that_stopped_early_does_not_claim_the_backfill_finished
#[tokio::test]
async fn a_walk_that_stopped_early_does_not_claim_the_backfill_finished() {
    // Scrolling stopped producing, which is not the same as reaching the end.
    let browser = FakeBrowser::new().pages(vec![like_page(notes(&["old-1"]), "", true, 200)]);
    let (database, result) = crawl_with(browser, json!({"abort_after": 2}), false, &["old-1"]).await;

    result.expect("walked");
    assert_eq!(state(database.db(), BACKFILL_STATE_KEY).await, None);
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_the_page_cap_leaves_the_backfill_open_for_the_next_run
#[tokio::test]
async fn the_page_cap_leaves_the_backfill_open_for_the_next_run() {
    let pages = (0..5)
        .map(|index| like_page(notes(&[&format!("n-{index}")]), &format!("C{index}"), true, 200))
        .collect();
    let browser = FakeBrowser::new().pages(pages);
    let (database, result) = crawl_with(browser, json!({"abort_after": 2, "max_pages_per_run": 2}), false, &[]).await;

    result.expect("walked");
    assert_eq!(resolved_note_ids(database.db()).await, ["n-0", "n-1"]);
    assert_eq!(state(database.db(), BACKFILL_STATE_KEY).await, None);
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_the_captcha_wall_ends_the_run_instead_of_being_scrolled_past
#[tokio::test]
async fn the_captcha_wall_ends_the_run_instead_of_being_scrolled_past() {
    let browser = FakeBrowser::new().pages(vec![like_page(json!([]), "", true, 461)]);
    let (database, result) = crawl_with(browser, json!({}), true, &[]).await;

    let error = result.expect_err("the wall");
    assert_eq!(error.class_name(), "RedNoteError");
    assert_eq!(error.notification_dedupe_key(), Some("rednote:risk"));
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_resolving_a_note_writes_one_row_per_file
#[tokio::test]
async fn resolving_a_note_writes_one_row_per_file() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let mut browser = FakeBrowser::new().note(NOTE_ID, page_note_card(json!({})));
    let log = browser.log();
    let (mut job, _web) = offline_job(db, config(json!({}))).await;
    let note = [NoteRef {
        note_id: NOTE_ID.to_owned(),
        xsec_token: "TOKEN".to_owned(),
    }];

    assert_eq!(job.resolve_notes(&note, &mut browser, "run-1").await.expect("resolved"), 1);

    let written = rows(db).await;
    assert_eq!(
        (written[0].note_id.as_str(), written[0].media_index, written[0].media_type.as_str()),
        (NOTE_ID, 1, "image")
    );
    // The note is opened with its token, or the page only loads for its author.
    assert_eq!(
        log.lock().expect("log").note_urls,
        [format!(
            "https://www.xiaohongshu.com/explore/{NOTE_ID}?xsec_token=TOKEN&xsec_source=pc_user"
        )]
    );

    // A second sighting refreshes what RedNote rotates and leaves the download state be.
    sqlx::query("UPDATE rednote SET downloaded = 1, local_path = '/kept';")
        .execute(db.pool())
        .await
        .expect("update");
    let rotated = page_note_card(json!({"imageList": [{"infoList": [{"imageScene": "WB_DFT", "url": "https://cdn/rotated"}]}]}));
    let mut browser = FakeBrowser::new().note(NOTE_ID, rotated);
    job.resolve_notes(&note, &mut browser, "run-2").await.expect("resolved");
    let written = rows(db).await;
    assert_eq!(
        (written[0].media_url.as_str(), written[0].downloaded, written[0].local_path.as_str()),
        ("https://cdn/rotated", 1, "/kept")
    );
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_a_note_that_cannot_be_read_does_not_end_the_run
#[tokio::test]
async fn a_note_that_cannot_be_read_does_not_end_the_run() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let mut browser = FakeBrowser::new().reader(|note_id| {
        if note_id == NOTE_ID {
            Err(navigation_failed())
        } else {
            Ok(value(page_note_card(json!({}))))
        }
    });
    let (mut job, _web) = offline_job(db, config(json!({}))).await;

    assert_eq!(
        job.resolve_notes(&note_refs(&[NOTE_ID, OTHER_NOTE_ID]), &mut browser, "run-1")
            .await
            .expect("resolved"),
        1
    );
    database.drop_database().await;
}

// ---------- where files land ----------

// py: tests/test_rednote.py::test_an_unset_video_path_keeps_everything_in_one_place
#[tokio::test]
async fn an_unset_video_path_keeps_everything_in_one_place() {
    let (job, _web) = offline_job(&unused_db(), config(json!({"path": "/data/rednote", "video_path": null}))).await;

    assert_eq!(job.destination_root("video").as_str(), "/data/rednote");
    assert_eq!(job.destination_root("image").as_str(), "/data/rednote");
}

// py: tests/test_rednote.py::test_videos_and_live_clips_go_to_the_video_path_while_images_stay
#[tokio::test]
async fn videos_and_live_clips_go_to_the_video_path_while_images_stay() {
    let cfg = config(json!({"path": "/data/rednote", "video_path": "/media/rednote-videos"}));
    let (job, _web) = offline_job(&unused_db(), cfg).await;

    assert_eq!(job.destination_root("video").as_str(), "/media/rednote-videos");
    // A live photo's clip is an mp4 like any other.
    assert_eq!(job.destination_root("live").as_str(), "/media/rednote-videos");
    assert_eq!(job.destination_root("image").as_str(), "/data/rednote");
    assert_eq!(
        job.build_output_path(&media(), "").expect("path"),
        format!("/data/rednote/artist/{FILE_NAME}")
    );
}

// ---------- downloading ----------

/// A job whose CDN answers with `answer`, writing under a fresh directory.
async fn downloading_job(
    db: &Db,
    answer: impl Fn(&str) -> Reply + Send + Sync + 'static,
    overrides: serde_json::Value,
) -> (Harness, FakeWeb, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("tmp");
    let cfg = config(merged(
        json!({"path": root.path().join("images"), "profile_path": root.path().join("profile")}),
        overrides,
    ));
    let web = cdn(answer).await;
    let client = client_through(&web, &cfg);
    (job(db, cfg, client, FakeBrowser::new()), web, root)
}

fn entries(directory: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

// py: tests/test_rednote.py::test_an_image_is_written_where_the_row_says_it_belongs
#[tokio::test]
async fn an_image_is_written_where_the_row_says_it_belongs() {
    let (harness, web, root) = downloading_job(&unused_db(), |_| Reply::bytes(200, b"image-bytes".to_vec()), json!({})).await;

    let dst_path = harness.job.download_file(&media()).await.expect("downloaded");

    let expected = root.path().join("images").join("artist").join(FILE_NAME);
    assert_eq!(dst_path, expected.to_string_lossy());
    assert_eq!(std::fs::read(&expected).expect("file"), b"image-bytes");
    let seen = web.seen();
    assert_eq!(seen[0].url, "https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3");
    assert_eq!(seen[0].header("Accept"), Some("image/*,video/*,*/*;q=0.8"));
}

// py: tests/test_rednote.py::test_a_file_already_on_disk_is_not_fetched_again
#[tokio::test]
async fn a_file_already_on_disk_is_not_fetched_again() {
    let (harness, web, root) = downloading_job(&unused_db(), |_| Reply::status(599), json!({})).await;
    let dst_path = root.path().join("images").join("artist").join(FILE_NAME);
    std::fs::create_dir_all(dst_path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&dst_path, b"already here").expect("write");

    assert_eq!(
        harness.job.download_file(&media()).await.expect("on disk"),
        dst_path.to_string_lossy()
    );
    assert!(web.seen().is_empty());
}

// py: tests/test_rednote.py::test_an_expired_url_is_recorded_rather_than_guessed_at
#[tokio::test]
async fn an_expired_url_is_recorded_rather_than_guessed_at() {
    // A 404 on a CDN URL says as much about rotation as about deletion, so the row waits
    // for a fresh look at the note.
    let (harness, _web, _root) = downloading_job(&unused_db(), |_| Reply::status(404), json!({})).await;

    match harness.job.download_file(&media()).await {
        Err(DownloadFailure::Stale(reason)) => assert!(reason.starts_with("stale-url"), "{reason}"),
        other => panic!("not a stale URL: {other:?}"),
    }
}

#[tokio::test]
async fn every_status_a_rotated_url_answers_is_recorded_as_stale() {
    for status in [403, 404, 410] {
        let (harness, _web, _root) = downloading_job(&unused_db(), move |_| Reply::status(status), json!({})).await;
        match harness.job.download_file(&media()).await {
            Err(DownloadFailure::Stale(reason)) => assert_eq!(reason, format!("stale-url http {status}")),
            other => panic!("{status} is not a stale URL: {other:?}"),
        }
    }
    let (harness, _web, _root) = downloading_job(&unused_db(), |_| Reply::status(500), json!({})).await;
    assert!(
        !matches!(harness.job.download_file(&media()).await, Err(DownloadFailure::Stale(_))),
        "a server error is not a rotated URL"
    );
}

// py: tests/test_rednote.py::test_downloading_marks_each_row_and_keeps_going_past_a_dead_url
#[tokio::test]
async fn downloading_marks_each_row_and_keeps_going_past_a_dead_url() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let dead_url = "https://sns-webpic.xhscdn.com/dead!nd_dft_wlteh_webp_3";
    let (harness, _web, _root) = downloading_job(
        db,
        move |url| match url {
            _ if url == dead_url => Reply::status(403),
            "https://cdn/x" => Reply::status(599),
            _ => Reply::bytes(200, b"bytes".to_vec()),
        },
        json!({}),
    )
    .await;
    pending_row(db, NOTE_ID, 1, dead_url, "").await;
    pending_row(db, NOTE_ID, 2, "https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3", "").await;
    sqlx::query("INSERT INTO rednote (note_id, media_index, media_url) VALUES ('broken', 1, 'https://cdn/x');")
        .execute(db.pool())
        .await
        .expect("row");
    let pending = harness.job.pending_media().await.expect("pending");

    assert_eq!(harness.job.download_pending(&pending).await.expect("downloaded"), 1);

    let written = rows(db).await;
    let state: Vec<(&str, i32, i32, i32, &str)> = written
        .iter()
        .map(|row| {
            (
                row.note_id.as_str(),
                row.media_index,
                row.downloaded,
                row.failed_count,
                row.last_error.as_str(),
            )
        })
        .collect();
    assert_eq!(state[0], (NOTE_ID, 1, 0, 1, "stale-url http 403"));
    assert_eq!(state[1], (NOTE_ID, 2, 1, 0, ""));
    assert!(
        written[1].local_path.ends_with("[64f1a2b3000000001e02c0de_2].webp"),
        "{}",
        written[1].local_path
    );
    // A status outside the stale ones is recorded as such.
    assert_eq!((state[2].0, state[2].3, state[2].4), ("broken", 1, "http 599"));
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_the_cdn_is_not_paced_like_the_site
#[tokio::test]
async fn the_cdn_is_not_paced_like_the_site() {
    // The request interval exists for RedNote's risk control; charging it per image
    // would add hours of pure waiting to a first backfill.
    let database = crate::testutil::database().await;
    let db = database.db();
    let (harness, _web, _root) = downloading_job(db, |_| Reply::bytes(200, b"bytes".to_vec()), json!({"sleep_request_seconds": 3.0})).await;
    let second = RedNoteMedia { media_index: 2, ..media() };

    let started = Instant::now();
    assert_eq!(harness.job.download_pending(&[media(), second]).await.expect("downloaded"), 2);
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_an_expired_url_is_refreshed_on_the_next_run_with_the_browser_open
#[tokio::test]
async fn an_expired_url_is_refreshed_on_the_next_run_with_the_browser_open() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let fresh_url = "https://sns-webpic.xhscdn.com/fresh!nd_dft_wlteh_webp_3";
    pending_row(
        db,
        NOTE_ID,
        1,
        "https://sns-webpic.xhscdn.com/expired!nd_dft_wlteh_webp_3",
        "stale-url http 403",
    )
    .await;
    let card = page_note_card(json!({"imageList": [{"infoList": [{"imageScene": "WB_DFT", "url": fresh_url}]}]}));
    let mut browser = FakeBrowser::new().note(NOTE_ID, card);
    let log = browser.log();
    let (mut job, _web) = offline_job(db, config(json!({}))).await;

    assert_eq!(job.refresh_stale_media(&mut browser).await.expect("refreshed"), 1);

    let written = rows(db).await;
    assert_eq!((written[0].media_url.as_str(), written[0].last_error.as_str()), (fresh_url, ""));
    assert_eq!(
        log.lock().expect("log").note_urls,
        [format!(
            "https://www.xiaohongshu.com/explore/{NOTE_ID}?xsec_token=TOKEN&xsec_source=pc_user"
        )]
    );
    database.drop_database().await;
}

#[tokio::test]
async fn a_url_the_note_still_offers_unchanged_is_left_to_be_looked_at_again() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let same_url = "https://sns-webpic.xhscdn.com/same!nd_dft_wlteh_webp_3";
    pending_row(db, NOTE_ID, 1, same_url, "stale-url http 403").await;
    let card = page_note_card(json!({"imageList": [{"infoList": [{"imageScene": "WB_DFT", "url": same_url}]}]}));
    let mut browser = FakeBrowser::new().note(NOTE_ID, card);
    let (mut job, _web) = offline_job(db, config(json!({}))).await;

    assert_eq!(job.refresh_stale_media(&mut browser).await.expect("refreshed"), 0);

    let written = rows(db).await;
    assert_eq!(
        (written[0].media_url.as_str(), written[0].last_error.as_str()),
        (same_url, "stale-url http 403")
    );
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_a_file_the_note_no_longer_offers_is_retired
#[tokio::test]
async fn a_file_the_note_no_longer_offers_is_retired() {
    let database = crate::testutil::database().await;
    let db = database.db();
    pending_row(db, NOTE_ID, 4, "https://cdn/gone", "stale-url http 404").await;
    // The note now has one image, so index 4 is gone for good.
    let mut browser = FakeBrowser::new().note(NOTE_ID, page_note_card(json!({})));
    let (mut job, _web) = offline_job(db, config(json!({}))).await;

    assert_eq!(job.refresh_stale_media(&mut browser).await.expect("refreshed"), 0);

    let written = rows(db).await;
    assert_eq!(
        (written[0].unavailable, written[0].failed_count, written[0].last_error.as_str()),
        (1, 1, "the note no longer offers this file")
    );
    assert!(job.pending_media().await.expect("pending").is_empty());
    database.drop_database().await;
}

// ---------- video notes ----------

// py: tests/test_rednote.py::test_a_video_note_is_fetched_from_the_cdn_like_every_other_file
#[tokio::test]
async fn a_video_note_is_fetched_from_the_cdn_like_every_other_file() {
    // No second client, no exported session: a video is a URL and a write.
    let root = tempfile::tempdir().expect("tmp");
    let video_root = root.path().join("videos");
    let (harness, web, root) = downloading_job(
        &unused_db(),
        |_| Reply::bytes(200, b"video-bytes".to_vec()),
        json!({"video_path": video_root}),
    )
    .await;
    let video = RedNoteMedia {
        media_type: "video".to_owned(),
        media_url: "https://sns-video-v28.xhscdn.com/stream/109_2160p.mp4?sign=abc".to_owned(),
        ..media()
    };

    let dst_path = harness.job.download_file(&video).await.expect("downloaded");

    let expected = video_root.join("artist").join(format!("[artist]2025-08-12 [{NOTE_ID}_1].mp4"));
    assert_eq!(dst_path, expected.to_string_lossy());
    assert_eq!(std::fs::read(&expected).expect("file"), b"video-bytes");
    for seen in web.seen() {
        assert!(!seen.url.contains("xiaohongshu.com"), "{}", seen.url);
        assert_eq!(seen.header("cookie"), None);
    }
    drop(root);
}

// py: tests/test_rednote.py::test_a_download_that_dies_half_way_leaves_nothing_that_looks_finished
#[tokio::test]
async fn a_download_that_dies_half_way_leaves_nothing_that_looks_finished() {
    // The next run skips whatever is already on disk, so a partial file is forever.
    for reply in [
        Reply::Reset,
        Reply::Truncated {
            status: 200,
            sent: b"half".to_vec(),
            length: 1000,
        },
    ] {
        let (harness, _web, root) = downloading_job(&unused_db(), move |_| reply.clone(), json!({})).await;
        let video = RedNoteMedia {
            media_type: "video".to_owned(),
            media_url: "https://sns-video-bd.xhscdn.com/k".to_owned(),
            ..media()
        };

        match harness.job.download_file(&video).await {
            Err(DownloadFailure::Other { class, .. }) => assert!(["ReadError", "RemoteProtocolError"].contains(&class.as_str()), "{class}"),
            other => panic!("not a transport failure: {other:?}"),
        }
        assert_eq!(entries(&root.path().join("images").join("artist")), Vec::<String>::new());
    }
}

// py: tests/test_rednote.py::test_an_expired_video_url_is_recorded_without_leaving_a_stub
#[tokio::test]
async fn an_expired_video_url_is_recorded_without_leaving_a_stub() {
    let (harness, _web, root) = downloading_job(&unused_db(), |_| Reply::status(403), json!({})).await;
    let video = RedNoteMedia {
        media_type: "video".to_owned(),
        media_url: "https://sns-video-bd.xhscdn.com/k".to_owned(),
        ..media()
    };

    assert!(matches!(harness.job.download_file(&video).await, Err(DownloadFailure::Stale(reason)) if reason.starts_with("stale-url")));
    assert_eq!(entries(&root.path().join("images").join("artist")), Vec::<String>::new());
}

// ---------- run ----------

// py: tests/test_rednote.py::test_a_run_signs_in_walks_the_list_and_saves_the_files
#[tokio::test]
async fn a_run_signs_in_walks_the_list_and_saves_the_files() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let root = tempfile::tempdir().expect("tmp");
    let cfg = config(json!({"path": root.path(), "profile_path": root.path().join("profile")}));
    let web = cdn(|_| Reply::bytes(200, b"image-bytes".to_vec())).await;
    let client = client_through(&web, &cfg);
    let browser = FakeBrowser::new()
        .pages(vec![like_page(
            json!([{"note_id": NOTE_ID, "xsec_token": "TOKEN"}]),
            "",
            false,
            200,
        )])
        .note(NOTE_ID, page_note_card(json!({})));
    let log = browser.log();
    let mut harness = job(db, cfg, client, browser);

    harness.job.update().await.expect("the run");

    {
        let log = log.lock().expect("log");
        assert!(log.started && log.closed);
        assert_eq!(log.opened_likes[0], "me");
    }
    assert_eq!(
        std::fs::read(root.path().join("artist").join(FILE_NAME)).expect("file"),
        b"image-bytes"
    );
    assert_eq!(rows(db).await[0].downloaded, 1);
    let sent = harness.notifier.sent();
    assert_eq!((sent[0].source.as_str(), sent[0].header.as_str()), ("rednote", "RedNote"));
    // Resolved once and remembered, so later runs skip the lookup.
    assert_eq!(state(db, USER_ID_STATE_KEY).await.as_deref(), Some("me"));
    database.drop_database().await;
}

// py: tests/test_rednote.py::test_the_browser_is_closed_even_when_the_walk_fails
#[tokio::test]
async fn the_browser_is_closed_even_when_the_walk_fails() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let root = tempfile::tempdir().expect("tmp");
    let cfg = config(json!({"path": root.path(), "profile_path": root.path().join("profile")}));
    let web = cdn(|_| Reply::status(599)).await;
    let client = client_through(&web, &cfg);
    let mut browser = FakeBrowser::new();
    browser.open_likes_fails = true;
    let log = browser.log();
    let mut harness = job(db, cfg, client, browser);

    let error = harness.job.update().await.expect_err("the walk fails");

    assert!(error.message().contains("navigation failed"), "{}", error.message());
    // Chromium costs hundreds of megabytes; leaking one per failed run would take the
    // whole worker down with it.
    assert!(log.lock().expect("log").closed);
    // The profile lock went with the run.
    let lock = fav_browser::profile::rednote_profile_lock_name(&root.path().join("profile"));
    assert!(db.try_advisory_lock(&lock).await.expect("lock").is_some());
    database.drop_database().await;
}

#[tokio::test]
async fn the_cdn_is_proxied_only_when_proxy_media_asks_for_it() {
    let proxy = "http://home.example:3128";
    let options = |overrides: serde_json::Value| client_options(&settings_of(overrides)).proxy;
    assert_eq!(options(json!({"proxy": proxy, "proxy_media": false})), None);
    assert_eq!(options(json!({"proxy": proxy, "proxy_media": true})).as_deref(), Some(proxy));
    assert_eq!(options(json!({"proxy": "", "proxy_media": true})), None);

    // httpx refuses a scheme it cannot dial when the client is built, so the run never starts.
    let error = cdn_client(&settings_of(json!({"proxy": "ftp://home.example:21", "proxy_media": true}))).expect_err("refused");
    assert_eq!(
        (error.class_name(), error.message()),
        ("ValueError", "Unknown scheme for proxy URL URL('ftp://home.example')")
    );
    // Unproxied, the same string is the browser's business, not the client's.
    cdn_client(&settings_of(json!({"proxy": "ftp://home.example:21", "proxy_media": false}))).expect("a client");
}

// py: tests/test_rednote.py::test_an_unconfigured_source_does_nothing_rather_than_opening_a_browser
#[tokio::test]
async fn an_unconfigured_source_does_nothing_rather_than_opening_a_browser() {
    let cfg = settings_of(json!({"proxy": ""}));
    let client = HttpClient::new(&ClientOptions::default()).expect("client");
    let factory: BrowserFactory = Box::new(|| panic!("should not open the browser"));
    let mut job = RedNote::new(unused_db(), Arc::new(RecordingNotifier::default()), cfg, client, factory);

    job.update().await.expect("parked");
}

#[tokio::test]
async fn a_proxy_chromium_cannot_use_ends_the_run_under_its_own_key() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let root = tempfile::tempdir().expect("tmp");
    let cfg = config(json!({"path": root.path(), "profile_path": root.path().join("profile")}));
    let client = HttpClient::new(&ClientOptions::default()).expect("client");
    let factory: BrowserFactory =
        Box::new(|| Err(fav_browser::proxy::build_proxy_settings("socks5://user:pw@home.example:1080").expect_err("refused")));
    let mut job = RedNote::new(db.clone(), Arc::new(RecordingNotifier::default()), cfg, client, factory);

    let error = job.update().await.expect_err("refused");

    assert_eq!(error.class_name(), "RedNoteError");
    assert_eq!(error.notification_dedupe_key(), Some("rednote:proxy"));
    database.drop_database().await;
}

#[test]
fn a_run_logs_the_walk_it_took() {
    let records = with_captured_log(async {
        let database = crate::testutil::database().await;
        let db = database.db();
        mark_backfill_done(db).await;
        let mut browser = FakeBrowser::new().pages(vec![like_page(notes(&["new-1"]), "C1", false, 200)]);
        let (mut job, _web) = offline_job(db, config(json!({}))).await;
        job.crawl(&mut browser).await.expect("walked");
        database.drop_database().await;
    });
    let messages: Vec<&str> = records.iter().map(|record| record.message.as_str()).collect();
    assert!(
        messages.contains(&"RedNote walking the likes list (backfill_complete=True)"),
        "{messages:?}"
    );
    assert!(messages.contains(&"RedNote likes page=1 notes=1 new=1 added=1"), "{messages:?}");
    assert!(messages.contains(&"RedNote likes list reported no further pages"), "{messages:?}");
}

// ---------- registration ----------

// py: tests/test_rednote.py::test_the_job_is_registered_and_parked_until_configured
#[tokio::test]
async fn the_job_is_registered_and_parked_until_configured() {
    let mut settings = Settings::default();
    settings.web.rednote.schedule.enabled = true;

    let jobs = fav_runtime::jobs::build_jobs(&settings);
    let job = jobs.iter().find(|job| job.key == "rednote").expect("a rednote job");

    assert_eq!(job.section, "web.rednote");
    // Nothing to shell out to: the browser reads, the HTTP client downloads.
    assert!(job.required_commands.is_empty());
    let db = unused_db();
    let ctx = crate::SourceContext {
        settings: Arc::new(fav_db::SettingsStore::new(db.clone())),
        db,
    };
    assert!(crate::registry(&ctx).contains(JOB_KEY));
    assert!(!job.enabled);
    assert!(job.missing_fields.iter().any(|field| field == "proxy"));
}

// ---------- the retire-a-deleted-note bookkeeping (test_rednote_missing_db.py) ----------

const RETIRE_AT: i32 = MISSING_RUNS_BEFORE_RETIRING;

// py: tests/test_rednote_missing_db.py::test_a_second_sighting_in_the_same_run_does_not_count_twice
#[tokio::test]
async fn a_second_sighting_in_the_same_run_does_not_count_twice() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let (job, _web) = offline_job(db, config(json!({}))).await;
    for _ in 0..3 {
        job.record_missing("same-run", "run-1").await.expect("recorded");
    }

    assert_eq!(missing_runs(db, "same-run").await, 1);
    assert!(job.retired_note_ids(&["same-run".to_owned()]).await.expect("retired").is_empty());
    database.drop_database().await;
}

// py: tests/test_rednote_missing_db.py::test_a_note_is_retired_only_once_three_separate_runs_agree
#[tokio::test]
async fn a_note_is_retired_only_once_three_separate_runs_agree() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let (job, _web) = offline_job(db, config(json!({}))).await;
    let note = ["three-runs".to_owned()];
    for index in 1..RETIRE_AT {
        job.record_missing("three-runs", &format!("run-{index}")).await.expect("recorded");
        assert!(
            job.retired_note_ids(&note).await.expect("retired").is_empty(),
            "retired before three runs agreed"
        );
    }
    job.record_missing("three-runs", &format!("run-{RETIRE_AT}"))
        .await
        .expect("recorded");

    assert_eq!(missing_runs(db, "three-runs").await, RETIRE_AT);
    assert_eq!(ids(&job.retired_note_ids(&note).await.expect("retired")), ["three-runs"]);
    database.drop_database().await;
}

// py: tests/test_rednote_missing_db.py::test_reading_a_note_again_clears_what_was_counted_against_it
#[tokio::test]
async fn reading_a_note_again_clears_what_was_counted_against_it() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let (job, _web) = offline_job(db, config(json!({}))).await;
    job.record_missing("recovered", "run-1").await.expect("recorded");
    job.record_missing("recovered", "run-2").await.expect("recorded");
    job.clear_missing("recovered").await.expect("cleared");
    assert_eq!(missing_runs(db, "recovered").await, 0);

    job.record_missing("recovered", "run-3").await.expect("recorded");

    assert_eq!(missing_runs(db, "recovered").await, 1);
    assert!(job.retired_note_ids(&["recovered".to_owned()]).await.expect("retired").is_empty());
    database.drop_database().await;
}

// py: tests/test_rednote_missing_db.py::test_the_retired_lookup_answers_for_a_batch_and_ignores_the_rest
#[tokio::test]
async fn the_retired_lookup_answers_for_a_batch_and_ignores_the_rest() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let (job, _web) = offline_job(db, config(json!({}))).await;
    for index in 1..=RETIRE_AT {
        job.record_missing("retired", &format!("run-{index}")).await.expect("recorded");
    }
    job.record_missing("counting", "run-1").await.expect("recorded");

    let batch = ["retired".to_owned(), "counting".to_owned(), "unseen".to_owned()];
    assert_eq!(ids(&job.retired_note_ids(&batch).await.expect("retired")), ["retired"]);
    assert!(job.retired_note_ids(&[]).await.expect("retired").is_empty());
    database.drop_database().await;
}

// py: tests/test_rednote_missing_db.py::test_the_first_sighting_is_stamped_and_later_ones_move_only_the_last
#[tokio::test]
async fn the_first_sighting_is_stamped_and_later_ones_move_only_the_last() {
    let database = crate::testutil::database().await;
    let db = database.db();
    let (job, _web) = offline_job(db, config(json!({}))).await;
    let stamps = || async {
        let row = sqlx::query("SELECT first_seen_at, last_seen_at FROM rednote_missing WHERE note_id = 'timestamps';")
            .fetch_one(db.pool())
            .await
            .expect("stamps");
        let first: chrono::DateTime<chrono::Utc> = row.get("first_seen_at");
        let last: chrono::DateTime<chrono::Utc> = row.get("last_seen_at");
        (first, last)
    };
    job.record_missing("timestamps", "run-1").await.expect("recorded");
    let first = stamps().await;
    job.record_missing("timestamps", "run-2").await.expect("recorded");
    let second = stamps().await;

    assert_eq!(second.0, first.0);
    assert!(second.1 >= first.1);
    database.drop_database().await;
}
