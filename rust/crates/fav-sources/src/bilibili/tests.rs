use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use fav_core::golden;
use fav_core::logging::Level;
use fav_core::pyjson;
use fav_core::validate::Model;
use fav_db::testdb::TestDatabase;
use fav_net::subprocess::{BoxFuture as RunFuture, Completed, LineSink};

use super::*;
use crate::testkit::{Answer, FakeServer, Received, RecordingNotifier};

const NOW: f64 = 1_800_000_000.0;
/// Valid BV ids (the Python tests' `BV1TEST1` would not decode to an aid here).
const BV_A: &str = "BV1xx411c7mD";
const BV_B: &str = "BV1GJ411x7h7";
const BV_C: &str = "BV1wM4m1S7oo";

fn clock() -> f64 {
    NOW
}

fn json(text: &str) -> Value {
    pyjson::loads(text).expect("test JSON")
}

fn no_waiting() -> Pacing {
    Pacing {
        batch_pause: Duration::ZERO,
        retry_base_seconds: 0.0,
    }
}

/// A database nothing listens on, for tests that must never reach it.
fn unused_db() -> Db {
    Db::connect_lazy("postgresql://tests@127.0.0.1:1/unused").expect("lazy pool")
}

async fn database() -> TestDatabase {
    let database = TestDatabase::create().await;
    database.db().ensure_schema().await.expect("schema applies");
    database
}

async fn insert_row(db: &Db, bvid: &str) {
    sqlx::query("INSERT INTO bilibili (bvid, fav_id, title, upper) VALUES ($1, 1, 't', 'u');")
        .bind(bvid)
        .execute(db.pool())
        .await
        .expect("seed a row");
}

async fn rows(db: &Db) -> Vec<(String, i64, String, String)> {
    sqlx::query_as("SELECT bvid, fav_id, title, upper FROM bilibili ORDER BY bvid;")
        .fetch_all(db.pool())
        .await
        .expect("rows")
}

fn stored(names: &[&str], prefix: &str) -> Vec<StoredCookie> {
    names
        .iter()
        .map(|name| StoredCookie {
            name: (*name).to_owned(),
            value: format!("{prefix}-{name}"),
            domain: ".bilibili.com".to_owned(),
            path: "/".to_owned(),
            expires: None,
        })
        .collect()
}

const FULL_SESSION: [&str; 4] = ["SESSDATA", "bili_jct", "buvid3", "DedeUserID"];

fn full_session(prefix: &str) -> Vec<StoredCookie> {
    stored(&FULL_SESSION, prefix)
}

fn account(name: &str, overrides: &str) -> BilibiliAccount {
    let mut payload = json(&format!(
        r#"{{"name": "{name}", "favorites": [{{"fav_id": 100, "path": "collection/bilibili/{name}/fav"}}]}}"#
    ));
    if let (Value::Dict(payload), Value::Dict(overrides)) = (&mut payload, json(overrides)) {
        for (key, value) in overrides.iter() {
            payload.insert(key.clone(), value.clone());
        }
    }
    BilibiliAccount::validate(&payload).expect("a valid account")
}

// ---------- the fake site ----------

fn ok(data: &str) -> Answer {
    Answer::json(200, &format!(r#"{{"code": 0, "message": "0", "ttl": 1, "data": {data}}}"#))
}

/// What the fake Bilibili answers, by endpoint.
struct Site {
    toview: Vec<String>,
    /// Each favourite-list page: its BV ids and `has_more`.
    fav_pages: Vec<(Vec<String>, bool)>,
    /// `View.title` and `Card.card.name`, by BV id; others get the Python dummy's.
    details: HashMap<String, (String, String)>,
    /// The `/x/web-interface/view` data, by BV id; others get a cover-less, free video.
    infos: HashMap<String, String>,
    nav: Answer,
}

impl Default for Site {
    fn default() -> Self {
        Self {
            toview: Vec::new(),
            fav_pages: vec![(Vec::new(), false)],
            details: HashMap::new(),
            infos: HashMap::new(),
            nav: ok(r#"{"isLogin": true, "uname": "u", "mid": 1}"#),
        }
    }
}

fn list(bvids: &[String]) -> String {
    let entries: Vec<String> = bvids.iter().map(|bvid| format!(r#"{{"bvid": "{bvid}"}}"#)).collect();
    format!("[{}]", entries.join(", "))
}

impl Site {
    fn answer(&self, request: &Received) -> Answer {
        let bvid = request.param("bvid").unwrap_or_default().to_owned();
        match request.path.as_str() {
            "/x/web-interface/nav" => self.nav.clone(),
            "/x/v2/history/toview" => ok(&format!(r#"{{"count": {}, "list": {}}}"#, self.toview.len(), list(&self.toview))),
            "/x/v2/history/toview/clear" => ok("null"),
            "/x/v3/fav/resource/list" => {
                let page: usize = request.param("pn").and_then(|page| page.parse().ok()).expect("a page");
                let (bvids, has_more) = &self.fav_pages[page - 1];
                ok(&format!(r#"{{"has_more": {has_more}, "medias": {}}}"#, list(bvids)))
            }
            "/x/web-interface/wbi/view/detail" => {
                let (title, upper) = self
                    .details
                    .get(&bvid)
                    .cloned()
                    .unwrap_or_else(|| ("Example Title".to_owned(), "Example Uploader".to_owned()));
                let title = repr_json(&title);
                let upper = repr_json(&upper);
                ok(&format!(
                    r#"{{"View": {{"title": {title}, "pic": "https://example.com/{bvid}.jpg", "dimension": {{"height": 1080}}, "pubdate": 1704067200}}, "Card": {{"card": {{"name": {upper}}}}}}}"#
                ))
            }
            "/x/web-interface/view" => ok(self
                .infos
                .get(&bvid)
                .map_or(r#"{"pic": "", "is_upower_exclusive": false}"#, String::as_str)),
            // A ticket renewal: refused, which only logs a warning.
            _ => Answer::json(200, r#"{"code": -400, "message": "refused in tests"}"#),
        }
    }
}

fn repr_json(text: &str) -> String {
    pyjson::dumps(&Value::from(text), &pyjson::DumpOptions::plain()).expect("a JSON string")
}

async fn site(script: Site) -> FakeServer {
    FakeServer::start(move |request| script.answer(request)).await
}

fn requests_to(server: &FakeServer, path: &str) -> Vec<Received> {
    server.received().into_iter().filter(|request| request.path == path).collect()
}

// ---------- the fake yt-dlp ----------

/// yt-dlp, scripted: each run takes the next outcome (default: success), and a
/// successful run writes `<bvid>.mp4` where `-o` points.
#[derive(Debug, Default)]
struct FakeYtDlp {
    commands: Mutex<Vec<Vec<String>>>,
    outcomes: Mutex<VecDeque<Completed>>,
    always_fail: bool,
}

impl FakeYtDlp {
    fn failing() -> Self {
        Self {
            always_fail: true,
            ..Self::default()
        }
    }

    fn with_outcomes(outcomes: Vec<Completed>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into()),
            ..Self::default()
        }
    }

    fn commands(&self) -> Vec<Vec<String>> {
        self.commands.lock().expect("commands").clone()
    }
}

impl Runner for FakeYtDlp {
    fn run(&self, argv: Vec<String>) -> RunFuture<'_, std::io::Result<Completed>> {
        self.commands.lock().expect("commands").push(argv.clone());
        let outcome = if self.always_fail {
            Completed {
                returncode: 1,
                stdout: String::new(),
                stderr: "temporary network error".to_owned(),
            }
        } else {
            self.outcomes.lock().expect("outcomes").pop_front().unwrap_or_default()
        };
        if outcome.returncode == 0 {
            let output = argv
                .iter()
                .position(|part| part == "-o")
                .map(|index| argv[index + 1].replace("%(ext)s", "mp4"));
            if let Some(output) = output
                && Path::new(&output).parent().is_some_and(Path::is_dir)
            {
                std::fs::write(&output, b"video").expect("write the download");
            }
        }
        Box::pin(async move { Ok(outcome) })
    }

    fn run_lines<'a>(&'a self, _argv: Vec<String>, _sink: LineSink<'a>) -> RunFuture<'a, std::io::Result<i32>> {
        unreachable!("yt-dlp runs to completion")
    }
}

// ---------- the crawler ----------

struct Fixture {
    crawler: Bilibili,
    notifier: Arc<RecordingNotifier>,
    runner: Arc<FakeYtDlp>,
}

fn fixture(db: Db, endpoints: Endpoints, notifier: RecordingNotifier, runner: FakeYtDlp, config: BilibiliConfig) -> Fixture {
    let notifier = Arc::new(notifier);
    let runner = Arc::new(runner);
    let shared: SharedNotifier = notifier.clone();
    let crawler = Bilibili::new(db, shared, runner.clone(), config)
        .expect("crawler")
        .with_test_seams(endpoints, no_waiting(), clock);
    Fixture { crawler, notifier, runner }
}

/// A crawler with an active account (`b.credential = object()` in Python).
fn signed_in(db: Db, server: &FakeServer, notifier: RecordingNotifier, runner: FakeYtDlp) -> Fixture {
    let mut fixture = fixture(db, Endpoints::at(&server.base_url), notifier, runner, BilibiliConfig::default());
    let credential = create_credential(&full_session("v")).expect("credential");
    fixture.crawler.api = Some(Api::new(credential, &server.base_url).expect("client"));
    fixture
}

fn bvids(videos: &[Video]) -> Vec<&str> {
    videos.iter().map(Video::bvid).collect()
}

fn owned(bvids: &[&str]) -> Vec<String> {
    bvids.iter().map(|bvid| (*bvid).to_owned()).collect()
}

fn mp4s(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.map(|entry| entry.expect("entry").path()).collect())
        .unwrap_or_default();
    files.retain(|path| path.extension().is_some_and(|extension| extension == "mp4"));
    files.sort();
    files
}

// ---------- schema ----------

// py: tests/test_bilibili.py::test_ensure_table_uses_primary_key_arbiter_without_extra_index
#[tokio::test]
async fn ensure_table_uses_primary_key_arbiter_without_extra_index() {
    let database = database().await;
    let db = database.db();
    let constraints: Vec<(String, String)> =
        sqlx::query_as("SELECT conname::text, contype::text FROM pg_constraint WHERE conrelid = 'bilibili'::regclass ORDER BY 1")
            .fetch_all(db.pool())
            .await
            .expect("constraints");
    assert_eq!(constraints, vec![("bilibili_pkey".to_owned(), "p".to_owned())]);
    // Applying the schema again adds nothing.
    db.ensure_schema().await.expect("schema applies twice");
    let again: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_constraint WHERE conrelid = 'bilibili'::regclass")
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(again, 1);
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_ensure_table_adds_missing_bvid_arbiter_with_fixed_constraint
#[tokio::test]
async fn ensure_table_adds_missing_bvid_arbiter_with_fixed_constraint() {
    let database = TestDatabase::create().await;
    let db = database.db();
    sqlx::raw_sql(
        "CREATE TABLE bilibili (bvid TEXT NOT NULL, fav_id BIGINT NOT NULL, title TEXT NOT NULL, upper TEXT NOT NULL,
                                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP);
         INSERT INTO bilibili (bvid, fav_id, title, upper) VALUES ('BV1', 1, 't', 'u');",
    )
    .execute(db.pool())
    .await
    .expect("a legacy table");
    db.ensure_schema().await.expect("schema applies over the legacy table");
    let constraints: Vec<String> = sqlx::query_scalar("SELECT conname::text FROM pg_constraint WHERE conrelid = 'bilibili'::regclass")
        .fetch_all(db.pool())
        .await
        .expect("constraints");
    assert_eq!(constraints, vec!["bilibili_bvid_unique".to_owned()]);
    // The rows stay: nothing is deleted to make room for the constraint.
    assert_eq!(rows(db).await.len(), 1);
    database.drop_database().await;
}

// ---------- update_fav ----------

// py: tests/test_bilibili.py::test_update_fav_sends_notification_for_each_video
#[tokio::test]
async fn update_fav_sends_notification_for_each_video() {
    let database = database().await;
    let server = site(Site {
        fav_pages: vec![(owned(&[BV_A, BV_B]), false)],
        details: HashMap::from([
            (BV_A.to_owned(), ("Title One".to_owned(), "Uploader One".to_owned())),
            (BV_B.to_owned(), ("Title Two".to_owned(), "Uploader Two".to_owned())),
        ]),
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(database.db().clone(), &server, RecordingNotifier::default(), FakeYtDlp::default());
    let root = tempfile::tempdir().expect("tempdir");

    fixture.crawler.update_fav(123, &root.path().join("fav")).await.expect("update");

    let sent = fixture.notifier.sent();
    assert_eq!(sent.len(), 2);
    let mut payload_bvids: Vec<String> = sent
        .iter()
        .map(|notification| str_value(notification.payload.as_ref().and_then(|payload| payload.get("bvid")).expect("bvid")))
        .collect();
    payload_bvids.sort();
    assert_eq!(payload_bvids, [BV_B, BV_A]);
    assert!(sent.iter().all(|notification| notification.header == "Bilibili (fav)"));
    let mut titles: Vec<&str> = sent.iter().map(|notification| notification.title.as_str()).collect();
    titles.sort_unstable();
    assert_eq!(titles, ["Title One", "Title Two"]);
    let stored = rows(database.db()).await;
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().all(|(_, fav_id, _, _)| *fav_id == 123));
    assert_eq!(mp4s(&root.path().join("fav")).len(), 2);
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_update_fav_continues_when_notification_fails
#[tokio::test]
async fn update_fav_continues_when_notification_fails() {
    let database = database().await;
    let server = site(Site {
        fav_pages: vec![(owned(&[BV_A]), false)],
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(database.db().clone(), &server, RecordingNotifier::failing(), FakeYtDlp::default());
    let root = tempfile::tempdir().expect("tempdir");

    fixture.crawler.update_fav(123, &root.path().join("fav")).await.expect("update");

    assert_eq!(rows(database.db()).await.len(), 1);
    assert_eq!(mp4s(&root.path().join("fav")).len(), 1);
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_update_fav_clears_toview_after_download_pass
#[tokio::test]
async fn update_fav_clears_toview_after_download_pass() {
    let database = database().await;
    let server = site(Site {
        toview: owned(&[BV_A]),
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(database.db().clone(), &server, RecordingNotifier::default(), FakeYtDlp::default());
    let root = tempfile::tempdir().expect("tempdir");

    fixture
        .crawler
        .update_fav(TOVIEW_FAV_ID, &root.path().join("toview"))
        .await
        .expect("update");

    let clears = requests_to(&server, "/x/v2/history/toview/clear");
    assert_eq!(clears.len(), 1);
    assert_eq!(clears[0].method, "POST");
    assert_eq!(rows(database.db()).await.len(), 1);
    assert_eq!(mp4s(&root.path().join("toview")).len(), 1);
    let videos = fixture.crawler.cache_dir().join("videos");
    assert!(videos.is_dir());
    assert_eq!(std::fs::read_dir(&videos).expect("videos").count(), 0);
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_update_fav_keeps_toview_when_existing_recovery_fails
#[tokio::test]
async fn update_fav_keeps_toview_when_existing_recovery_fails() {
    let database = database().await;
    insert_row(database.db(), BV_A).await;
    let server = site(Site {
        toview: owned(&[BV_A]),
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(
        database.db().clone(),
        &server,
        RecordingNotifier::failing_resolve(),
        FakeYtDlp::default(),
    );
    let root = tempfile::tempdir().expect("tempdir");

    fixture
        .crawler
        .update_fav(TOVIEW_FAV_ID, &root.path().join("toview"))
        .await
        .expect("update");

    assert!(requests_to(&server, "/x/v2/history/toview/clear").is_empty());
    assert!(fixture.runner.commands().is_empty());
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_update_fav_does_not_clear_toview_when_list_is_empty
#[tokio::test]
async fn update_fav_does_not_clear_toview_when_list_is_empty() {
    let server = site(Site::default()).await;
    // The database is never asked: an empty list has nothing to compare.
    let mut fixture = signed_in(unused_db(), &server, RecordingNotifier::default(), FakeYtDlp::default());
    let root = tempfile::tempdir().expect("tempdir");

    fixture
        .crawler
        .update_fav(TOVIEW_FAV_ID, &root.path().join("toview"))
        .await
        .expect("update");

    assert!(requests_to(&server, "/x/v2/history/toview/clear").is_empty());
}

// ---------- listing ----------

// py: tests/test_bilibili.py::test_get_toviews_filters_existing_bvids_globally
#[tokio::test]
async fn get_toviews_filters_existing_bvids_globally() {
    let database = database().await;
    insert_row(database.db(), BV_A).await;
    let server = site(Site {
        toview: owned(&[BV_A, BV_B]),
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(database.db().clone(), &server, RecordingNotifier::default(), FakeYtDlp::default());

    let (videos, has_any_toviews, recovered) = fixture.crawler.get_toviews().await.expect("toviews");

    assert!(has_any_toviews);
    assert!(recovered);
    assert_eq!(bvids(&videos), [BV_B]);
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_get_toviews_retries_recovery_for_existing_bvids
#[tokio::test]
async fn get_toviews_retries_recovery_for_existing_bvids() {
    let database = database().await;
    insert_row(database.db(), BV_A).await;
    let server = site(Site {
        toview: owned(&[BV_A]),
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(database.db().clone(), &server, RecordingNotifier::default(), FakeYtDlp::default());

    let (videos, has_any_toviews, recovered) = fixture.crawler.get_toviews().await.expect("toviews");

    assert!(videos.is_empty());
    assert!(has_any_toviews);
    assert!(recovered);
    let resolved = fixture.notifier.resolved();
    assert_eq!(resolved[0].0, format!("job_failed:bilibili:bilibili:download:{BV_A}"));
    assert_eq!(
        resolved[0].1.body,
        format!("Download succeeded: Example Title [{BV_A}]\nExample Uploader | toview | 1080p | unknown | 2024-01-01")
    );
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_get_toviews_reports_failed_existing_recovery
#[tokio::test]
async fn get_toviews_reports_failed_existing_recovery() {
    let database = database().await;
    insert_row(database.db(), BV_A).await;
    let server = site(Site {
        toview: owned(&[BV_A]),
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(
        database.db().clone(),
        &server,
        RecordingNotifier::failing_resolve(),
        FakeYtDlp::default(),
    );

    let (videos, has_any_toviews, recovered) = fixture.crawler.get_toviews().await.expect("toviews");

    assert!(videos.is_empty());
    assert!(has_any_toviews);
    assert!(!recovered);
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_get_favs_filters_existing_bvids_globally
#[tokio::test]
async fn get_favs_filters_existing_bvids_globally() {
    let database = database().await;
    insert_row(database.db(), BV_A).await;
    let server = site(Site {
        fav_pages: vec![(owned(&[BV_A, BV_B]), false)],
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(database.db().clone(), &server, RecordingNotifier::default(), FakeYtDlp::default());

    let (videos, recovered) = fixture.crawler.get_favs(123).await.expect("favs");

    assert_eq!(bvids(&videos), [BV_B]);
    assert!(recovered);
    let pages = requests_to(&server, "/x/v3/fav/resource/list");
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].param("media_id"), Some("123"));
    assert_eq!(pages[0].param("pn"), Some("1"));
    // Only read: the table still holds the one seeded row.
    assert_eq!(rows(database.db()).await.len(), 1);
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_get_favs_retries_existing_recovery_across_pages
#[tokio::test]
async fn get_favs_retries_existing_recovery_across_pages() {
    let database = database().await;
    insert_row(database.db(), BV_A).await;
    insert_row(database.db(), BV_B).await;
    let server = site(Site {
        fav_pages: vec![(owned(&[BV_A]), true), (owned(&[BV_B]), false)],
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(database.db().clone(), &server, RecordingNotifier::default(), FakeYtDlp::default());

    let (videos, recovered) = fixture.crawler.get_favs(123).await.expect("favs");

    assert!(videos.is_empty());
    assert!(recovered);
    let pages: Vec<Option<String>> = requests_to(&server, "/x/v3/fav/resource/list")
        .iter()
        .map(|request| request.param("pn").map(str::to_owned))
        .collect();
    assert_eq!(pages, [Some("1".to_owned()), Some("2".to_owned())]);
    let keys: Vec<String> = fixture.notifier.resolved().into_iter().map(|(key, _)| key).collect();
    assert_eq!(
        keys,
        [
            format!("job_failed:bilibili:bilibili:download:{BV_A}"),
            format!("job_failed:bilibili:bilibili:download:{BV_B}"),
        ]
    );
    database.drop_database().await;
}

#[tokio::test]
async fn paid_videos_are_left_out() {
    let database = database().await;
    let server = site(Site {
        fav_pages: vec![(owned(&[BV_A, BV_B]), false)],
        infos: HashMap::from([(BV_A.to_owned(), r#"{"pic": "", "is_upower_exclusive": true}"#.to_owned())]),
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(database.db().clone(), &server, RecordingNotifier::default(), FakeYtDlp::default());
    let root = tempfile::tempdir().expect("tempdir");

    fixture.crawler.update_fav(7, root.path()).await.expect("update");

    let stored: Vec<String> = rows(database.db()).await.into_iter().map(|(bvid, ..)| bvid).collect();
    assert_eq!(stored, [BV_B]);
    database.drop_database().await;
}

// ---------- notifications ----------

fn downloaded_from(input: &Value) -> Downloaded {
    let text = |key: &str| input.get(key).filter(|value| !matches!(value, Value::Null)).map(golden::string);
    Downloaded {
        bvid: golden::string(golden::get(input, "bvid")),
        title: golden::string(golden::get(input, "title")),
        upper: golden::string(golden::get(input, "upper")),
        fav_id: i64::try_from(golden::int(golden::get(input, "fav_id"))).expect("fav_id"),
        resolution: text("resolution"),
        file_size_bytes: input.get("file_size_bytes").map(golden::int),
        release_date: text("release_date"),
        cover_url: text("cover_url"),
    }
}

fn notification_as_value(notification: &NewNotification) -> Value {
    let mut dict = Dict::new();
    dict.insert("kind", Value::from(notification.kind.as_str()));
    dict.insert("source", Value::from(notification.source.as_str()));
    dict.insert("header", Value::from(notification.header.as_str()));
    dict.insert("title", Value::from(notification.title.as_str()));
    dict.insert("body", Value::from(notification.body.as_str()));
    dict.insert("link_url", Value::from(notification.link_url.as_str()));
    dict.insert("image_url", Value::from(notification.image_url.as_str()));
    dict.insert("payload", notification.payload.clone().map_or(Value::Null, Value::Dict));
    Value::Dict(dict)
}

fn resolution_as_value(key: &str, resolution: &Resolution) -> Value {
    let mut dict = Dict::new();
    dict.insert("dedupe_key", Value::from(key));
    dict.insert("kind", Value::from(resolution.kind.as_str()));
    dict.insert("source", Value::from(resolution.source.as_str()));
    dict.insert("header", Value::from(resolution.header.as_str()));
    dict.insert("title", Value::from(resolution.title.as_str()));
    dict.insert("body", Value::from(resolution.body.as_str()));
    dict.insert("link_url", Value::from(resolution.link_url.as_str()));
    dict.insert("image_url", Value::from(resolution.image_url.as_str()));
    dict.insert("payload", resolution.payload.clone().map_or(Value::Null, Value::Dict));
    Value::Dict(dict)
}

#[tokio::test]
async fn notifications_match_the_recorded_calls() {
    let document = golden::load("bilibili/notifications.json");
    for case in golden::cases(&document) {
        let input = golden::get(case, "input");
        let fixture = fixture(
            unused_db(),
            Endpoints::default(),
            RecordingNotifier::default(),
            FakeYtDlp::default(),
            BilibiliConfig::default(),
        );
        let recovered = fixture.crawler.notify_download(&downloaded_from(input)).await;
        let enqueued: Vec<Value> = fixture.notifier.sent().iter().map(notification_as_value).collect();
        let resolved: Vec<Value> = fixture
            .notifier
            .resolved()
            .iter()
            .map(|(key, resolution)| resolution_as_value(key, resolution))
            .collect();
        assert_eq!(&Value::List(enqueued), golden::get(case, "enqueued"), "{input:?}");
        assert_eq!(&Value::List(resolved), golden::get(case, "resolved"), "{input:?}");
        assert_eq!(&Value::Bool(recovered), golden::get(case, "recovered"), "{input:?}");
    }
}

// py: tests/test_bilibili.py::test_notify_download_enqueues_structured_payload
#[tokio::test]
async fn notify_download_enqueues_structured_payload() {
    let fixture = fixture(
        unused_db(),
        Endpoints::default(),
        RecordingNotifier::default(),
        FakeYtDlp::default(),
        BilibiliConfig::default(),
    );
    let video = Downloaded {
        bvid: BV_C.to_owned(),
        title: "让你说话了吗?笨蛋".to_owned(),
        upper: "孟程程_".to_owned(),
        fav_id: 123,
        resolution: Some("720p".to_owned()),
        file_size_bytes: Some(20 * 1024 * 1024),
        release_date: Some("2024-01-01".to_owned()),
        cover_url: Some("https://example.com/cover.jpg".to_owned()),
    };

    let recovered = fixture.crawler.notify_download(&video).await;

    let sent = fixture.notifier.sent();
    assert_eq!(
        notification_as_value(&sent[0]),
        json(
            r#"{"kind": "download_completed", "source": "bilibili", "header": "Bilibili (fav)", "title": "让你说话了吗?笨蛋",
                "body": "孟程程_ | 720p | 20.0 MB | 2024-01-01", "link_url": "https://www.bilibili.com/video/BV1wM4m1S7oo",
                "image_url": "https://example.com/cover.jpg",
                "payload": {"bvid": "BV1wM4m1S7oo", "fav_id": 123, "upper": "孟程程_", "resolution": "720p",
                            "file_size_bytes": 20971520, "release_date": "2024-01-01"}}"#
        )
    );
    assert_eq!(sent.len(), 1);
    let resolved = fixture.notifier.resolved();
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolution_as_value(&resolved[0].0, &resolved[0].1),
        json(
            r#"{"dedupe_key": "job_failed:bilibili:bilibili:download:BV1wM4m1S7oo", "kind": "job_recovered", "source": "worker",
                "header": "Bilibili", "title": "Job recovered",
                "body": "Download succeeded: 让你说话了吗?笨蛋 [BV1wM4m1S7oo]\n孟程程_ | fav | 720p | 20.0 MB | 2024-01-01",
                "link_url": "https://www.bilibili.com/video/BV1wM4m1S7oo", "image_url": "https://example.com/cover.jpg",
                "payload": {"job": "bilibili", "bvid": "BV1wM4m1S7oo", "fav_id": 123, "upper": "孟程程_", "resolution": "720p",
                            "file_size_bytes": 20971520, "release_date": "2024-01-01"}}"#
        )
    );
    assert!(recovered);
}

// ---------- covers ----------

// py: tests/test_bilibili.py::test_get_video_cover_url_from_detail_view_pic
#[tokio::test]
async fn get_video_cover_url_from_detail_view_pic() {
    let server = FakeServer::start(|request| panic!("unexpected request {}", request.path)).await;
    let mut fixture = signed_in(unused_db(), &server, RecordingNotifier::default(), FakeYtDlp::default());
    let video = Video::new(BV_A).expect("video");
    let detail = json(r#"{"View": {"pic": "//i0.hdslb.com/bfs/archive/test-cover.jpg"}}"#);

    let cover = fixture.crawler.get_video_cover_url(&video, Some(&detail)).await.expect("cover");

    assert_eq!(cover.as_deref(), Some("https://i0.hdslb.com/bfs/archive/test-cover.jpg"));
}

// py: tests/test_bilibili.py::test_get_video_cover_url_falls_back_to_get_info_and_cache
#[tokio::test]
async fn get_video_cover_url_falls_back_to_get_info_and_cache() {
    let server = site(Site {
        infos: HashMap::from([(BV_A.to_owned(), r#"{"pic": "https://i0.hdslb.com/test-cover.jpg"}"#.to_owned())]),
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(unused_db(), &server, RecordingNotifier::default(), FakeYtDlp::default());
    let video = Video::new(BV_A).expect("video");

    let first = fixture.crawler.get_video_cover_url(&video, None).await.expect("cover");
    let second = fixture.crawler.get_video_cover_url(&video, None).await.expect("cover");

    assert_eq!(first.as_deref(), Some("https://i0.hdslb.com/test-cover.jpg"));
    assert_eq!(second, first);
    assert_eq!(requests_to(&server, "/x/web-interface/view").len(), 1);
}

// py: tests/test_bilibili.py::test_get_video_cover_url_returns_none_when_missing
#[tokio::test]
async fn get_video_cover_url_returns_none_when_missing() {
    let server = site(Site {
        infos: HashMap::from([(BV_A.to_owned(), "{}".to_owned())]),
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(unused_db(), &server, RecordingNotifier::default(), FakeYtDlp::default());

    let cover = fixture
        .crawler
        .get_video_cover_url(&Video::new(BV_A).expect("video"), None)
        .await
        .expect("cover");

    assert_eq!(cover, None);
}

// py: tests/test_bilibili.py::test_get_video_cover_url_returns_none_for_placeholder_cover
#[tokio::test]
async fn get_video_cover_url_returns_none_for_placeholder_cover() {
    let server = site(Site {
        infos: HashMap::from([(
            BV_A.to_owned(),
            r#"{"pic": "https://i0.hdslb.com/bfs/archive/transparent.png"}"#.to_owned(),
        )]),
        ..Site::default()
    })
    .await;
    let mut fixture = signed_in(unused_db(), &server, RecordingNotifier::default(), FakeYtDlp::default());

    let cover = fixture
        .crawler
        .get_video_cover_url(&Video::new(BV_A).expect("video"), None)
        .await
        .expect("cover");

    assert_eq!(cover, None);
}

#[tokio::test]
async fn a_cover_lookup_that_fails_is_no_cover() {
    let server = FakeServer::start(|_| Answer::json(200, r#"{"code": -404, "message": "啥都木有"}"#)).await;
    let mut fixture = signed_in(unused_db(), &server, RecordingNotifier::default(), FakeYtDlp::default());

    let cover = fixture
        .crawler
        .get_video_cover_url(&Video::new(BV_A).expect("video"), None)
        .await
        .expect("cover");

    assert_eq!(cover, None);
}

// ---------- downloads ----------

fn offline(config: BilibiliConfig, runner: FakeYtDlp, cookie_path: &str) -> Fixture {
    let mut fixture = fixture(unused_db(), Endpoints::default(), RecordingNotifier::default(), runner, config);
    fixture.crawler.cookie_path = PathBuf::from(cookie_path);
    fixture
}

#[tokio::test]
async fn download_commands_match_the_recorded_ones() {
    let document = golden::load("bilibili/download_commands.json");
    for case in golden::cases(&document) {
        let proxy = golden::string(golden::get(case, "proxy"));
        let config = BilibiliConfig {
            proxy: proxy.clone(),
            accounts: Vec::new(),
        };
        let fixture = offline(config, FakeYtDlp::default(), "/tmp/fav-bilibili-x/bilibili-main.txt");
        fixture
            .crawler
            .download(
                &format!("https://www.bilibili.com/video/{BV_A}"),
                BV_A,
                Path::new("/nonexistent-fav-dir/videos"),
            )
            .await
            .expect("download");
        let want: Vec<String> = golden::list(golden::get(case, "command"))
            .iter()
            .map(golden::string)
            .map(|part| part.replace("/tmp/fav-bilibili-x/videos", "/nonexistent-fav-dir/videos"))
            .collect();
        assert_eq!(fixture.runner.commands(), [want], "{proxy:?}");
    }
}

// py: tests/test_bilibili.py::test_download_routes_yt_dlp_through_the_configured_proxy
#[tokio::test]
async fn download_routes_yt_dlp_through_the_configured_proxy() {
    for proxy in ["", "http://gluetun-hk.proxy.svc.cluster.local:80"] {
        let config = BilibiliConfig {
            proxy: proxy.to_owned(),
            accounts: Vec::new(),
        };
        let root = tempfile::tempdir().expect("tempdir");
        let fixture = offline(config, FakeYtDlp::default(), "/c.txt");
        let url = format!("https://www.bilibili.com/video/{BV_A}");

        fixture
            .crawler
            .download(&url, BV_A, &root.path().join("videos"))
            .await
            .expect("download");

        let commands = fixture.runner.commands();
        assert_eq!(commands.len(), 1);
        let command = &commands[0];
        match command.iter().position(|part| part == "--proxy") {
            Some(index) => assert_eq!(command[index + 1], proxy),
            None => assert!(proxy.is_empty()),
        }
        assert_eq!(command.last(), Some(&url));
    }
}

// py: tests/test_bilibili.py::test_download_chunks_streams_so_every_cdn_request_carries_range
#[tokio::test]
async fn download_chunks_streams_so_every_cdn_request_carries_range() {
    let fixture = offline(BilibiliConfig::default(), FakeYtDlp::default(), "/c.txt");
    let root = tempfile::tempdir().expect("tempdir");

    fixture
        .crawler
        .download(&format!("https://www.bilibili.com/video/{BV_A}"), BV_A, root.path())
        .await
        .expect("download");

    let command = &fixture.runner.commands()[0];
    let index = command.iter().position(|part| part == "--http-chunk-size").expect("chunked");
    assert_eq!(command[index + 1], "10M");
}

/// Run a download on a fresh runtime, capturing what it logs.
fn download_logged(
    runtime: &tokio::runtime::Runtime,
    fixture: &Fixture,
    url: &str,
    bvid: &str,
    dirpath: &Path,
) -> (Result<(), JobError>, Vec<logging::Record>) {
    let mut outcome = None;
    let records = logging::capture(|| outcome = Some(runtime.block_on(fixture.crawler.download(url, bvid, dirpath))));
    (outcome.expect("ran"), records)
}

// py: tests/test_bilibili.py::test_download_retry_does_not_emit_warning_per_attempt
#[test]
fn download_retry_does_not_emit_warning_per_attempt() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _entered = runtime.enter();
    let fixture = offline(BilibiliConfig::default(), FakeYtDlp::failing(), "/c.txt");
    let root = tempfile::tempdir().expect("tempdir");

    let (outcome, records) = download_logged(
        &runtime,
        &fixture,
        "https://www.bilibili.com/video/BV1TEST1",
        "BV1TEST1",
        &root.path().join("videos"),
    );

    let error = outcome.expect_err("every attempt failed");
    assert_eq!(fixture.runner.commands().len(), 3);
    assert!(records.iter().all(|record| record.level < Level::Warning), "{records:?}");
    assert_eq!(error.class_name(), "DownloadError");
    assert_eq!(error.notification_dedupe_key(), Some("bilibili:download:BV1TEST1"));
}

#[test]
fn download_failures_match_the_recorded_ones() {
    let document = golden::load("bilibili/download_failures.json");
    for case in golden::cases(&document) {
        let returncode = i32::try_from(golden::int(golden::get(case, "returncode"))).expect("code");
        let completed = Completed {
            returncode,
            stdout: golden::string(golden::get(case, "stdout")),
            stderr: golden::string(golden::get(case, "stderr")),
        };
        // The ladder Python waits on, 5 s then 10 s, on a paused clock: the log lines name
        // the waits and the test does not sit through them.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("runtime");
        let _entered = runtime.enter();
        let mut fixture = offline(
            BilibiliConfig::default(),
            FakeYtDlp::with_outcomes(vec![completed.clone(), completed.clone(), completed]),
            "/c.txt",
        );
        fixture.crawler.pacing = Pacing::default();
        let (outcome, records) = download_logged(
            &runtime,
            &fixture,
            "https://www.bilibili.com/video/BV1x",
            "BV1x",
            Path::new("/nonexistent-dir"),
        );
        let debug: Vec<String> = records
            .iter()
            .filter(|record| record.level == Level::Debug && record.message.starts_with("Retrying"))
            .map(|record| record.message.clone())
            .collect();
        let want_debug: Vec<String> = golden::list(golden::get(case, "debug")).iter().map(golden::string).collect();
        assert_eq!(debug, want_debug, "{returncode}");
        if case.get("ok").is_some() {
            outcome.expect("a success");
        } else {
            let error = outcome.expect_err("a failure");
            assert_eq!(error.class_name(), golden::string(golden::get(case, "error_class")));
            assert_eq!(error.message(), golden::string(golden::get(case, "error")));
            assert_eq!(
                error.notification_dedupe_key(),
                Some(golden::string(golden::get(case, "dedupe_key")).as_str())
            );
        }
    }
}

#[test]
fn downloads_are_renamed_into_the_list_directory() {
    let root = tempfile::tempdir().expect("tempdir");
    let source = root.path().join("videos");
    let dest = root.path().join("fav");
    std::fs::create_dir_all(&source).expect("source");
    std::fs::create_dir_all(&dest).expect("dest");
    std::fs::write(source.join("BV1x.mp4"), b"v").expect("video");
    std::fs::write(source.join("BV1x.m4a"), b"a").expect("audio");
    std::fs::write(dest.join("[u]t [BV1x].mp4"), b"old").expect("an earlier copy");

    let saved = move_downloads(&source, &dest, "t", "BV1x", "u").expect("moved");

    let names: Vec<String> = saved
        .iter()
        .map(|path| path.file_name().expect("name").to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["[u]t [BV1x].m4a", "[u]t [BV1x] (1).mp4"]);
    assert_eq!(std::fs::read_dir(&source).expect("source").count(), 0);
}

// ---------- the run ----------

fn steps(accounts: &[BilibiliAccount]) -> Vec<(i64, String)> {
    plan(accounts)
        .into_iter()
        .filter_map(|step| match step {
            Step::List { fav_id, path } => Some((fav_id, path)),
            Step::Activate(_) => None,
        })
        .collect()
}

// py: tests/test_bilibili.py::test_update_walks_every_account_and_favourite
#[test]
fn update_walks_every_account_and_favourite() {
    let accounts = [
        account(
            "main",
            r#"{"favorites": [{"fav_id": 11, "path": "collection/bilibili/main/fav-a"}, {"fav_id": 22, "path": "collection/bilibili/main/fav-b"}]}"#,
        ),
        account("alt", r#"{"favorites": [{"fav_id": 33, "path": "collection/bilibili/alt/fav"}]}"#),
    ];

    assert_eq!(
        steps(&accounts),
        [
            (11, "collection/bilibili/main/fav-a".to_owned()),
            (22, "collection/bilibili/main/fav-b".to_owned()),
            (33, "collection/bilibili/alt/fav".to_owned()),
        ]
    );
}

// py: tests/test_bilibili.py::test_update_skips_toview_when_the_account_has_it_disabled
#[test]
fn update_skips_toview_when_the_account_has_it_disabled() {
    let accounts = [account("main", r#"{"toview_enabled": false}"#)];

    assert!(steps(&accounts).iter().all(|(fav_id, _)| *fav_id != TOVIEW_FAV_ID));
}

// py: tests/test_bilibili.py::test_update_uses_the_accounts_own_toview_path
#[test]
fn update_uses_the_accounts_own_toview_path() {
    let accounts = [account(
        "main",
        r#"{"favorites": [{"fav_id": 11, "path": "collection/bilibili/main/fav"}], "toview_enabled": true, "toview_path": "collection/bilibili/main/later"}"#,
    )];

    // Favourites first, then watch-later, into directories that do not overlap.
    assert_eq!(
        steps(&accounts),
        [
            (11, "collection/bilibili/main/fav".to_owned()),
            (TOVIEW_FAV_ID, "collection/bilibili/main/later".to_owned()),
        ]
    );
}

// py: tests/test_bilibili.py::test_update_activates_each_account_before_its_favourites
#[test]
fn update_activates_each_account_before_its_favourites() {
    let accounts = [account("main", "{}"), account("alt", "{}")];

    let events: Vec<String> = plan(&accounts)
        .into_iter()
        .map(|step| match step {
            Step::Activate(name) => format!("activate:{name}"),
            Step::List { fav_id, .. } => format!("fav:{fav_id}"),
        })
        .collect();

    assert_eq!(events, ["activate:main", "fav:100", "activate:alt", "fav:100"]);
}

#[tokio::test]
async fn no_accounts_is_nothing_to_do() {
    let mut fixture = fixture(
        unused_db(),
        Endpoints::default(),
        RecordingNotifier::default(),
        FakeYtDlp::default(),
        BilibiliConfig::default(),
    );
    fixture.crawler.update().await.expect("nothing to do");
}

async fn sign_in(db: &Db, site: &str, cookies: &[StoredCookie]) {
    site_sessions::save_signed_in(db, site, cookies, "u", None, &[])
        .await
        .expect("store the login");
}

async fn stored_session(db: &Db, site: &str) -> SiteSession {
    site_sessions::load(db, site).await.expect("load").expect("a stored login")
}

// py: tests/test_bilibili.py::test_each_account_uses_its_own_login
#[tokio::test]
async fn each_account_uses_its_own_login() {
    let database = database().await;
    let db = database.db();
    sign_in(db, "bilibili-main", &full_session("a")).await;
    sign_in(db, "bilibili-alt", &full_session("b")).await;
    site_sessions::record_check(db, "bilibili-main", false, "stale")
        .await
        .expect("check");
    let server = site(Site::default()).await;
    let root = tempfile::tempdir().expect("tempdir");
    let root_path = root.path().to_str().expect("utf-8");
    let accounts = vec![
        account(
            "main",
            &format!(r#"{{"favorites": [{{"fav_id": 100, "path": "{root_path}/main"}}]}}"#),
        ),
        account(
            "alt",
            &format!(r#"{{"favorites": [{{"fav_id": 100, "path": "{root_path}/alt"}}]}}"#),
        ),
    ];
    let config = BilibiliConfig {
        proxy: String::new(),
        accounts,
    };
    let mut fixture = fixture(
        db.clone(),
        Endpoints::at(&server.base_url),
        RecordingNotifier::default(),
        FakeYtDlp::default(),
        config,
    );

    fixture.crawler.update().await.expect("update");

    // Separate credentials for the API calls, separate cookie files for yt-dlp.
    let sessdatas: Vec<String> = requests_to(&server, "/x/v3/fav/resource/list")
        .iter()
        .map(|request| {
            let cookie = request.header("cookie").expect("a cookie header");
            cookie.split("; ").next().expect("SESSDATA first").to_owned()
        })
        .collect();
    assert_eq!(sessdatas, ["SESSDATA=a-SESSDATA", "SESSDATA=b-SESSDATA"]);
    assert_eq!(fixture.crawler.cookie_path(), fixture.crawler.cache_dir().join("bilibili-alt.txt"));
    let main = std::fs::read_to_string(fixture.crawler.cache_dir().join("bilibili-main.txt")).expect("main's cookies");
    assert!(main.contains("\tSESSDATA\ta-SESSDATA"));
    let alt = std::fs::read_to_string(fixture.crawler.cache_dir().join("bilibili-alt.txt")).expect("alt's cookies");
    assert!(alt.contains("\tSESSDATA\tb-SESSDATA"));
    assert_eq!(
        fixture.crawler.credential().map(|credential| credential.sessdata.as_str()),
        Some("b-SESSDATA")
    );
    for site in ["bilibili-main", "bilibili-alt"] {
        let session = stored_session(db, site).await;
        assert_eq!((session.status.as_str(), session.message.as_str()), ("valid", ""), "{site}");
    }
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_an_account_never_signed_in_fails_with_the_login_page_hint
#[tokio::test]
async fn an_account_never_signed_in_fails_with_the_login_page_hint() {
    let database = database().await;
    let server = FakeServer::start(|request| panic!("unexpected request {}", request.path)).await;
    let mut fixture = fixture(
        database.db().clone(),
        Endpoints::at(&server.base_url),
        RecordingNotifier::default(),
        FakeYtDlp::default(),
        BilibiliConfig::default(),
    );

    let error = fixture
        .crawler
        .activate_account(&account("main", "{}"))
        .await
        .expect_err("not signed in");

    assert_eq!(error.class_name(), "SessionError");
    assert!(error.message().contains("登录态"), "{}", error.message());
    assert_eq!(error.notification_dedupe_key(), Some("bilibili:auth:main"));
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_a_session_bilibili_rejects_is_marked_invalid
#[tokio::test]
async fn a_session_bilibili_rejects_is_marked_invalid() {
    let database = database().await;
    let db = database.db();
    sign_in(db, "bilibili-main", &full_session("v")).await;
    let server = site(Site {
        nav: Answer::json(200, r#"{"code": -101, "message": "账号未登录", "data": {"isLogin": false}}"#),
        ..Site::default()
    })
    .await;
    let mut fixture = fixture(
        db.clone(),
        Endpoints::at(&server.base_url),
        RecordingNotifier::default(),
        FakeYtDlp::default(),
        BilibiliConfig::default(),
    );

    let error = fixture
        .crawler
        .activate_account(&account("main", "{}"))
        .await
        .expect_err("rejected");

    assert_eq!(error.class_name(), "SessionError");
    assert!(error.message().contains("no longer valid"), "{}", error.message());
    let session = stored_session(db, "bilibili-main").await;
    assert_eq!(session.status, "invalid");
    assert_eq!(session.message, error.message());
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_bilibili_not_answering_does_not_mark_the_session_invalid
#[tokio::test]
async fn bilibili_not_answering_does_not_mark_the_session_invalid() {
    let database = database().await;
    let db = database.db();
    sign_in(db, "bilibili-main", &full_session("v")).await;
    let before = stored_session(db, "bilibili-main").await;
    // Nothing listens there: the ticket renewal and the check both fail to connect.
    let mut fixture = fixture(
        db.clone(),
        Endpoints::at("http://127.0.0.1:1"),
        RecordingNotifier::default(),
        FakeYtDlp::default(),
        BilibiliConfig::default(),
    );

    let error = fixture
        .crawler
        .activate_account(&account("main", "{}"))
        .await
        .expect_err("unreachable");

    assert_eq!(error.class_name(), "RuntimeError");
    assert!(error.message().contains("unreachable"), "{}", error.message());
    let after = stored_session(db, "bilibili-main").await;
    assert_eq!((after.status, after.checked_at), (before.status, before.checked_at));
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_an_account_needs_favourites_or_watch_later_but_no_cookiecloud
#[test]
fn an_account_needs_favourites_or_watch_later_but_no_cookiecloud() {
    let section = |accounts: &str| fav_core::settings::Bilibili::validate(&json(&format!(r#"{{"accounts": {accounts}}}"#))).expect("valid");
    let main = r#"{"name": "main", "favorites": [{"fav_id": 100, "path": "collection/bilibili/main/fav"}]}"#;

    assert!(section(&format!("[{main}]")).validate_runnable().is_empty());
    assert_eq!(
        section(r#"[{"name": "main", "favorites": []}]"#).validate_runnable(),
        ["accounts[0].favorites"]
    );
}

// py: tests/test_bilibili.py::test_a_stored_cookiecloud_reference_is_ignored
#[test]
fn a_stored_cookiecloud_reference_is_ignored() {
    let account = BilibiliAccount::validate(&json(r#"{"name": "main", "toview_enabled": true, "cookiecloud": "main"}"#)).expect("valid");

    assert!(account.dump().get("cookiecloud").is_none());
}

// py: tests/test_bilibili.py::test_create_credential_names_the_missing_cookies
#[test]
fn create_credential_names_the_missing_cookies() {
    let error = create_credential(&stored(&["bili_jct", "buvid3", "DedeUserID"], "v")).expect_err("incomplete");

    assert_eq!(error.class_name(), "SessionError");
    assert!(error.message().contains("SESSDATA"), "{}", error.message());
}

// py: tests/test_bilibili.py::test_create_credential_builds_from_the_stored_session
#[test]
fn create_credential_builds_from_the_stored_session() {
    let credential = create_credential(&full_session("v")).expect("credential");

    assert_eq!(credential.sessdata, "v-SESSDATA");
    assert_eq!(credential.bili_jct, "v-bili_jct");
    assert_eq!(credential.buvid3, "v-buvid3");
    assert_eq!(credential.dedeuserid, "v-DedeUserID");
}

// ---------- renew_session ----------

const REFRESH_COOKIES: [&str; 2] = [
    "SESSDATA=new-SESSDATA; Path=/; Domain=bilibili.com; Expires=Wed, 01 Jan 2031 00:00:00 GMT; HttpOnly",
    "bili_jct=new-bili_jct; Path=/; Domain=bilibili.com; Expires=Wed, 01 Jan 2031 00:00:00 GMT",
];

fn renewal_site(request: &Received) -> Answer {
    let path = request.path.as_str();
    if path.ends_with("/GenWebTicket") {
        ok(r#"{"ticket": "t", "created_at": 1800000000, "ttl": 259200}"#)
    } else if path.ends_with("/cookie/info") {
        ok(r#"{"refresh": true}"#)
    } else if path.starts_with("/correspond/1/") {
        Answer::html(r#"<html><div id="1-name">rcsrf</div></html>"#)
    } else if path.ends_with("/cookie/refresh") {
        REFRESH_COOKIES.iter().fold(ok(r#"{"refresh_token": "rt-new"}"#), |answer, header| {
            answer.with_header("set-cookie", header)
        })
    } else {
        ok("null")
    }
}

async fn signed_in_with_token(db: &Db, token: Option<&str>) -> SiteSession {
    let extra: Vec<(String, String)> = token
        .map(|token| vec![(session::REFRESH_TOKEN_KEY.to_owned(), token.to_owned())])
        .unwrap_or_default();
    site_sessions::save_signed_in(db, "bilibili-main", &full_session("v"), "u", None, &extra)
        .await
        .expect("store the login");
    stored_session(db, "bilibili-main").await
}

// py: tests/test_bilibili.py::test_renew_session_stores_a_new_ticket_and_a_refreshed_sessdata
#[tokio::test]
async fn renew_session_stores_a_new_ticket_and_a_refreshed_sessdata() {
    let database = database().await;
    let db = database.db();
    let session = signed_in_with_token(db, Some("rt-old")).await;
    let server = FakeServer::start(renewal_site).await;

    let result = renew_session(db, "bilibili-main", session, &Endpoints::at(&server.base_url), NOW)
        .await
        .expect("renewed");

    assert_eq!(result.cookie("SESSDATA"), "new-SESSDATA");
    assert_eq!(result.cookie("bili_ticket"), "t");
    assert_eq!(result.extra, [(session::REFRESH_TOKEN_KEY.to_owned(), "rt-new".to_owned())]);
    let stored = stored_session(db, "bilibili-main").await;
    assert_eq!(stored.cookies, result.cookies);
    assert_eq!(stored.extra, result.extra);
    // The refresh went out with the ticket the first step stored, and the old token.
    let refresh = requests_to(&server, "/x/passport-login/web/cookie/refresh");
    assert_eq!(refresh.len(), 1);
    assert!(refresh[0].header("cookie").expect("cookies").contains("bili_ticket=t"));
    assert!(refresh[0].form().contains(&("refresh_token".to_owned(), "rt-old".to_owned())));
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_renew_session_keeps_the_session_when_bilibili_refuses
#[tokio::test]
async fn renew_session_keeps_the_session_when_bilibili_refuses() {
    let database = database().await;
    let db = database.db();
    let session = signed_in_with_token(db, Some("rt")).await;
    let server = FakeServer::start(|_| Answer::json(200, r#"{"code": -400, "message": "bad"}"#)).await;
    // The ticket is refused; the refresh cannot connect.
    let endpoints = Endpoints {
        api: server.base_url.clone(),
        passport: "http://127.0.0.1:1".to_owned(),
        www: "http://127.0.0.1:1".to_owned(),
    };

    let result = renew_session(db, "bilibili-main", session.clone(), &endpoints, NOW)
        .await
        .expect("kept");

    assert_eq!(result.cookies, session.cookies);
    assert_eq!(stored_session(db, "bilibili-main").await.cookies, session.cookies);
    database.drop_database().await;
}

// py: tests/test_bilibili.py::test_renew_session_without_a_refresh_token_only_renews_the_ticket
#[tokio::test]
async fn renew_session_without_a_refresh_token_only_renews_the_ticket() {
    let database = database().await;
    let db = database.db();
    let session = signed_in_with_token(db, None).await;
    // The ticket step answers nothing usable; no refresh is attempted without a token.
    let server = FakeServer::start(|request| {
        assert!(request.path.ends_with("/GenWebTicket"), "unexpected request {}", request.path);
        ok("{}")
    })
    .await;

    let result = renew_session(db, "bilibili-main", session.clone(), &Endpoints::at(&server.base_url), NOW)
        .await
        .expect("kept");

    assert_eq!(result.cookies, session.cookies);
    assert_eq!(stored_session(db, "bilibili-main").await.cookies, session.cookies);
    database.drop_database().await;
}

// ---------- verify_session ----------

fn golden_cookies(value: &Value) -> Vec<StoredCookie> {
    golden::list(value)
        .iter()
        .map(|cookie| StoredCookie {
            name: golden::string(golden::get(cookie, "name")),
            value: golden::string(golden::get(cookie, "value")),
            domain: golden::string(golden::get(cookie, "domain")),
            path: golden::string(golden::get(cookie, "path")),
            expires: None,
        })
        .collect()
}

/// The recorded body: latin-1 text for raw bytes, otherwise the JSON as UTF-8.
fn golden_body(text: &str) -> Vec<u8> {
    if text.chars().all(|ch| u32::from(ch) < 256) {
        text.chars().map(|ch| u8::try_from(u32::from(ch)).expect("latin-1")).collect()
    } else {
        text.as_bytes().to_vec()
    }
}

#[tokio::test]
async fn session_checks_match_the_recorded_ones() {
    let document = golden::load("bilibili/verify_session.json");
    for case in golden::cases(&document) {
        let cookies = golden_cookies(golden::get(case, "cookies"));
        let answer = golden::get(case, "answer");
        let reply = match answer {
            Value::Null => None,
            answer => Some((
                u16::try_from(golden::int(golden::get(answer, "status"))).expect("status"),
                golden_body(&golden::string(golden::get(answer, "body"))),
            )),
        };
        let server = {
            let reply = reply.clone();
            FakeServer::start(move |_| {
                let (status, body) = reply.clone().expect("no request expected");
                Answer::bytes(status, &body)
            })
            .await
        };
        let check = verify_session_at(&server.base_url, &cookies).await;
        let want = golden::get(case, "check");
        let context = format!("{answer:?}");
        assert_eq!(&Value::Bool(check.ok), golden::get(want, "ok"), "{context}");
        assert_eq!(check.account, golden::string(golden::get(want, "account")), "{context}");
        assert_eq!(check.message, golden::string(golden::get(want, "message")), "{context}");
        assert_eq!(&Value::Bool(check.rejected), golden::get(want, "rejected"), "{context}");
        let received = server.received();
        let expected = golden::list(golden::get(case, "requests"));
        assert_eq!(received.len(), expected.len(), "{context}");
        for (got, want) in received.iter().zip(expected) {
            assert_eq!(got.method, golden::string(golden::get(want, "method")), "{context}");
            assert_eq!(got.path_and_query(), "/x/web-interface/nav", "{context}");
            for header in ["user-agent", "referer", "origin", "cookie", "content-type"] {
                assert_eq!(
                    got.header(header),
                    golden::get(want, &header.replace('-', "_")).as_str(),
                    "{context} {header}"
                );
            }
        }
    }
}

fn nav(status: u16, body: &'static str) -> impl Fn(&Received) -> Answer + Send + Sync + 'static {
    move |_| Answer::json(status, body)
}

// py: tests/test_bilibili.py::test_verify_waits_for_every_cookie_a_credential_needs
#[tokio::test]
async fn verify_waits_for_every_cookie_a_credential_needs() {
    let server = FakeServer::start(nav(200, "{}")).await;

    let check = verify_session_at(&server.base_url, &stored(&["SESSDATA", "bili_jct", "DedeUserID"], "v")).await;

    assert!(!check.ok);
    assert!(!check.rejected);
    assert!(check.message.contains("buvid3"), "{}", check.message);
}

// py: tests/test_bilibili.py::test_verify_names_the_signed_in_user
#[tokio::test]
async fn verify_names_the_signed_in_user() {
    let server = FakeServer::start(nav(200, r#"{"code": 0, "data": {"isLogin": true, "uname": "某人", "mid": 42}}"#)).await;

    let check = verify_session_at(&server.base_url, &full_session("v")).await;

    assert!(check.ok);
    assert_eq!(check.account, "某人 (42)");
    let cookie = server.received()[0].header("cookie").expect("cookies").to_owned();
    assert!(cookie.contains("SESSDATA=v-SESSDATA"), "{cookie}");
}

// py: tests/test_bilibili.py::test_verify_reports_a_signed_out_session_as_rejected
#[tokio::test]
async fn verify_reports_a_signed_out_session_as_rejected() {
    let server = FakeServer::start(nav(200, r#"{"code": -101, "message": "账号未登录", "data": {"isLogin": false}}"#)).await;

    let check = verify_session_at(&server.base_url, &full_session("v")).await;

    assert!(!check.ok);
    assert!(check.rejected);
}

// py: tests/test_bilibili.py::test_verify_does_not_call_an_error_page_a_rejection
#[tokio::test]
async fn verify_does_not_call_an_error_page_a_rejection() {
    let server = FakeServer::start(nav(412, r#"{"code": -412}"#)).await;

    let check = verify_session_at(&server.base_url, &full_session("v")).await;

    assert!(!check.ok);
    assert!(!check.rejected);
}

// ---------- helpers ----------

#[test]
fn helpers_match_the_recorded_ones() {
    let document = golden::load("bilibili/helpers.json");
    let outcome = |result: Result<Option<String>, PyError>| match result {
        Ok(text) => Value::from(text),
        Err(error) => json(&format!(
            r#"{{"error_class": {}, "error": {}}}"#,
            repr_json(&error.class),
            repr_json(&error.message)
        )),
    };
    for case in golden::cases(&document) {
        let input = golden::get(case, "input");
        match golden::string(golden::get(case, "helper")).as_str() {
            "format_file_size" => {
                let size = match input {
                    Value::Null => None,
                    other => Some(golden::int(other)),
                };
                assert_eq!(Value::from(format_file_size(size)), *golden::get(case, "output"), "{input:?}");
            }
            "to_positive_int" => {
                // Floats past 1e20 are recorded as their repr.
                let input = match input {
                    Value::Str(text) if text == "1e+30" => Value::Float(1e30),
                    other => other.clone(),
                };
                let got = match to_positive_int(Some(&input)) {
                    Ok(number) => number.map_or(Value::Null, Value::Int),
                    Err(error) => outcome(Err(error)),
                };
                assert_eq!(got, *golden::get(case, "output"), "{input:?}");
            }
            "detail" => {
                assert_eq!(
                    outcome(extract_resolution_label(input)),
                    *golden::get(case, "resolution"),
                    "{input:?}"
                );
                assert_eq!(
                    outcome(extract_release_date(input)),
                    *golden::get(case, "release_date"),
                    "{input:?}"
                );
            }
            "cover" => {
                let normalized = normalize_cover_url(&golden::string(input));
                assert_eq!(normalized, golden::string(golden::get(case, "normalized")));
                assert_eq!(
                    Value::Bool(is_placeholder_cover_url(&normalized)),
                    *golden::get(case, "placeholder"),
                    "{normalized}"
                );
            }
            other => panic!("no helper {other}"),
        }
    }
}

#[test]
fn texts_match_the_recorded_ones() {
    let document = golden::load("bilibili/texts.json");
    for case in golden::cases(&document) {
        let name = golden::string(golden::get(case, "name"));
        let input = golden::get(case, "input");
        let got = match name.as_str() {
            "session_site" => Value::from(session_site(&golden::string(input))),
            "relogin_hint" => Value::from(relogin_hint(&golden::string(input))),
            "download_failure_key" => Value::from(download_failure_key(&golden::string(input))),
            "download_failure_notification_key" => Value::from(download_failure_notification_key(&golden::string(input))),
            "missing_cookies" => {
                let names: Vec<String> = golden::list(input).iter().map(golden::string).collect();
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                Value::List(missing_cookies(&stored(&names, "v")).into_iter().map(Value::from).collect())
            }
            other => panic!("no text {other}"),
        };
        assert_eq!(&got, golden::get(case, "output"), "{name}");
    }
}

#[tokio::test]
async fn the_job_is_registered() {
    let settings = Arc::new(fav_db::SettingsStore::new(unused_db()));
    let registry = crate::registry(&SourceContext { db: unused_db(), settings });
    assert!(registry.contains(JOB_KEY));
}
