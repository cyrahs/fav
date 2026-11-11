use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use fav_core::pyjson::{self, DumpOptions, Value};
use fav_core::settings::{Defaults as _, KemonoCreator, Settings};
use fav_core::validate::PyPath;
use fav_db::testdb::TestDatabase;
use fav_db::{Db, SettingsStore};
use fav_net::retry::RecordingSleeper;
use fav_net::{ClientOptions, ErrorClass, HttpClient};
use fav_runtime::notifications::Outbox;
use serde_json::json;
use sha2::{Digest as _, Sha256};

use super::{
    Attachment, Post, Run, attachment_filename, build_file_url, build_thumbnail_url, client_options, collect_new_posts, parse_post,
    retry_after_seconds, retry_delay_seconds, sha256_from_path, should_retry_exception, should_retry_status,
};
use crate::testutil::{self, FakeWeb, Reply, Seen};

const FILE_HOST: &str = "file.pawchive.pw";
const THUMB_HOST: &str = "img.pawchive.pw";

fn value(json: &serde_json::Value) -> Value {
    pyjson::loads(&json.to_string()).expect("JSON")
}

fn post_payload(post_id: &str, path: Option<&str>, name: Option<&str>, preview_only: bool) -> serde_json::Value {
    let file = match path {
        Some(path) => json!({"name": name, "path": path, "preview_only": preview_only}),
        None => json!({}),
    };
    json!({
        "id": post_id,
        "user": "123",
        "service": "fanbox",
        "title": format!("post {post_id}"),
        "published": "2026-08-17T00:00:00",
        "file": file,
        "attachments": [],
    })
}

fn plain_post(post_id: &str) -> serde_json::Value {
    post_payload(post_id, None, None, false)
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn sha256_path(content: &[u8], ext: &str) -> String {
    let digest = hex(content);
    format!("/{}/{}/{digest}{ext}", &digest[..2], &digest[2..4])
}

fn unreachable_db() -> Db {
    Db::connect_lazy("postgresql://nobody@127.0.0.1:1/none").expect("lazy pool")
}

struct Harness {
    run: Run,
    sleeps: RecordingSleeper,
}

fn harness(web: &FakeWeb, db: Db, root: &Path) -> Harness {
    let mut settings = Settings::defaults();
    let cfg = &mut settings.web.kemono;
    cfg.path = PyPath::new(root.to_str().expect("UTF-8 path"));
    cfg.base_url = "http://kemono.test".to_owned();
    cfg.file_base_url = format!("http://{FILE_HOST}");
    cfg.thumbnail_base_url = format!("http://{THUMB_HOST}");
    cfg.sleep_request_seconds = 0.0;
    cfg.creators = vec![KemonoCreator {
        service: "fanbox".to_owned(),
        id: "123".to_owned(),
        name: "tester".to_owned(),
    }];
    let cfg = settings.web.kemono.clone();
    let options = ClientOptions {
        proxy: Some(web.proxy.clone()),
        timeout: Duration::from_millis(500),
        ..client_options()
    };
    let sleeps = RecordingSleeper::new();
    let run = Run {
        cfg,
        http: HttpClient::new(&options).expect("client"),
        outbox: Outbox::new(db.clone(), Arc::new(SettingsStore::pinned(settings))),
        db,
        sleeper: sleeps.sleeper(),
    };
    Harness { run, sleeps }
}

fn listing(posts: &[serde_json::Value]) -> Reply {
    Reply::json(200, &serde_json::Value::Array(posts.to_vec()))
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

// ---------- the database side ----------

async fn kemono_ids(database: &TestDatabase) -> Vec<String> {
    sqlx::query_scalar("SELECT id FROM kemono ORDER BY created_at, id")
        .fetch_all(database.db().pool())
        .await
        .unwrap()
}

async fn archive_post(database: &TestDatabase, id: &str) {
    sqlx::query("INSERT INTO kemono (id, service, user_id, title) VALUES ($1, 'fanbox', '123', 'old')")
        .bind(id)
        .execute(database.db().pool())
        .await
        .unwrap();
}

/// The payloads of the queued notifications.
async fn notifications(database: &TestDatabase) -> Vec<Value> {
    let payloads: Vec<String> = sqlx::query_scalar("SELECT payload FROM notifications ORDER BY id")
        .fetch_all(database.db().pool())
        .await
        .unwrap();
    payloads.iter().map(|payload| pyjson::loads(payload).unwrap()).collect()
}

fn field(payload: &Value, name: &str) -> String {
    pyjson::dumps(payload.get(name).expect(name), &DumpOptions::compact()).unwrap()
}

type PendingRow = (String, String, String, String, String, i32, DateTime<Utc>);

async fn pending_rows(database: &TestDatabase) -> Vec<PendingRow> {
    sqlx::query_as("SELECT service, user_id, post_id, path, dst, attempts, next_check_at FROM kemono_pending_originals ORDER BY path")
        .fetch_all(database.db().pool())
        .await
        .unwrap()
}

async fn queue_pending(database: &TestDatabase, path: &str, dst: &str, attempts: i32) {
    sqlx::query(
        "INSERT INTO kemono_pending_originals (service, user_id, post_id, path, dst, attempts, next_check_at)
         VALUES ('fanbox', '123', '9', $1, $2, $3, NOW() - INTERVAL '1 minute')",
    )
    .bind(path)
    .bind(dst)
    .bind(attempts)
    .execute(database.db().pool())
    .await
    .unwrap();
}

fn gap(next_check_at: DateTime<Utc>) -> chrono::Duration {
    next_check_at - Utc::now()
}

// ---------- pure functions ----------

// py: tests/test_kemono.py::test_parse_post_merges_file_and_attachments
#[test]
fn parse_post_merges_file_and_attachments() {
    let post = parse_post(&value(&json!({
        "id": 12_441_864,
        "user": 68_870_973,
        "service": "fanbox",
        "title": "",
        "published": "2026-08-17T21:59:37",
        "file": {"name": "main.jpeg", "path": "/aa/bb/main-hash.jpeg"},
        "attachments": [
            {"name": null, "path": "/cc/dd/second-hash.png"},
            {"name": "skipped.zip", "path": ""},
        ],
    })))
    .unwrap();
    assert_eq!(post.id, "12441864");
    assert_eq!(post.user, "68870973");
    assert_eq!(post.title, Value::from("untitled"));
    let names: Vec<&str> = post
        .attachments
        .iter()
        .map(|attachment| attachment.name.as_str().unwrap())
        .collect();
    assert_eq!(names, ["main.jpeg", "second-hash.png"]);
    let paths: Vec<&str> = post
        .attachments
        .iter()
        .map(|attachment| attachment.path.as_str().unwrap())
        .collect();
    assert_eq!(paths, ["/aa/bb/main-hash.jpeg", "/cc/dd/second-hash.png"]);

    // Malformed payloads raise what Python raises.
    assert_eq!(
        parse_post(&value(&json!([]))).unwrap_err().formatted(),
        "AttributeError: 'list' object has no attribute 'get'"
    );
    assert_eq!(parse_post(&value(&json!({"user": 1}))).unwrap_err().formatted(), "KeyError: 'id'");
    assert_eq!(
        parse_post(&value(&json!({"id": 1, "attachments": null}))).unwrap_err().formatted(),
        "TypeError: 'NoneType' object is not iterable"
    );
}

// py: tests/test_kemono.py::test_sha256_from_path
#[test]
fn sha256_from_path_reads_hash_stems() {
    let digest = "a".repeat(64);
    assert_eq!(sha256_from_path(&format!("/aa/aa/{digest}.jpeg")), Some(digest.clone()));
    assert_eq!(sha256_from_path(&format!("/aa/aa/{}.jpeg", digest.to_uppercase())), Some(digest));
    assert_eq!(sha256_from_path("/aa/aa/not-a-hash.jpeg"), None);
    assert_eq!(sha256_from_path(&format!("/aa/aa/{}.jpeg", "a".repeat(63))), None);
}

// py: tests/test_kemono.py::test_attachment_filename
#[test]
fn attachment_filename_keeps_extension_and_limits_stem() {
    assert_eq!(attachment_filename("cover.jpeg", 1), "cover.jpeg");
    assert_eq!(attachment_filename("", 3), "file_3");
    let result = attachment_filename(&format!("{}.png", "x".repeat(300)), 1);
    assert_eq!(Path::new(&result).extension().and_then(|ext| ext.to_str()), Some("png"));
    assert!(result.len() <= 150 + ".png".len());
}

// py: tests/test_kemono.py::test_build_file_url
#[test]
fn build_file_url_joins_data_path() {
    let expected = "https://file.pawchive.pw/data/aa/bb/hash.jpeg";
    assert_eq!(build_file_url("https://file.pawchive.pw", "/aa/bb/hash.jpeg"), expected);
    assert_eq!(build_file_url("https://file.pawchive.pw/", "/aa/bb/hash.jpeg"), expected);
}

fn ids(posts: &[Post]) -> Vec<&str> {
    posts.iter().map(|post| post.id.as_str()).collect()
}

// py: tests/test_kemono.py::test_collect_new_posts
#[test]
fn collect_new_posts_takes_the_unarchived_prefix() {
    let posts: Vec<Post> = ["5", "4", "3"]
        .iter()
        .map(|id| parse_post(&value(&plain_post(id))).unwrap())
        .collect();
    let (fresh, hit_known) = collect_new_posts(&posts, &HashSet::new());
    assert_eq!(ids(&fresh), ["5", "4", "3"]);
    assert!(!hit_known);
    let known: HashSet<String> = ["4".to_owned()].into();
    let (fresh, hit_known) = collect_new_posts(&posts, &known);
    assert_eq!(ids(&fresh), ["5"]);
    assert!(hit_known);
    let (fresh, hit_known) = collect_new_posts(&[], &known);
    assert!(fresh.is_empty());
    assert!(!hit_known);
}

// py: tests/test_kemono.py::test_should_retry_status
#[test]
fn should_retry_status_only_throttling() {
    for status in [429, 502, 503, 504] {
        assert!(should_retry_status(status));
    }
    // 403 means blocked on Cloudflare; retrying it is deliberately off the table.
    for status in [403, 404, 500] {
        assert!(!should_retry_status(status));
    }
}

// py: tests/test_kemono.py::test_should_retry_exception
#[test]
fn should_retry_exception_only_transport_failures() {
    for class in [
        ErrorClass::ConnectError,
        ErrorClass::ReadError,
        ErrorClass::ReadTimeout,
        ErrorClass::RemoteProtocolError,
    ] {
        assert!(should_retry_exception(class), "{class:?}");
    }
    for class in [
        ErrorClass::UnsupportedProtocol,
        ErrorClass::LocalProtocolError,
        ErrorClass::DecodingError,
    ] {
        assert!(!should_retry_exception(class), "{class:?}");
    }
}

// py: tests/test_kemono.py::test_retry_after_seconds
#[test]
fn retry_after_seconds_reads_seconds_and_dates() {
    assert_eq!(retry_after_seconds(None).unwrap(), None);
    assert_eq!(retry_after_seconds(Some("17")).unwrap(), Some(17.0));
    assert_eq!(retry_after_seconds(Some("garbage")).unwrap(), None);
    // A date in the past clamps to zero rather than going negative.
    assert_eq!(retry_after_seconds(Some("Mon, 17 Aug 2020 00:00:00 GMT")).unwrap(), Some(0.0));
}

// py: tests/test_kemono.py::test_retry_delay_seconds
#[test]
fn retry_delay_seconds_follows_the_ladder() {
    assert!((retry_delay_seconds(1, None) - 2.0).abs() < f64::EPSILON);
    assert!((retry_delay_seconds(4, None) - 30.0).abs() < f64::EPSILON);
    assert!((retry_delay_seconds(99, None) - 30.0).abs() < f64::EPSILON);
    assert!((retry_delay_seconds(1, Some(17.0)) - 17.0).abs() < f64::EPSILON);
    assert!((retry_delay_seconds(1, Some(9999.0)) - 300.0).abs() < f64::EPSILON);
}

// py: tests/test_kemono.py::test_parse_post_reads_preview_only
#[test]
fn parse_post_reads_preview_only() {
    let post = parse_post(&value(&json!({
        "id": 1,
        "user": 2,
        "service": "patreon",
        "title": "t",
        "file": {"name": "a.gif", "path": "/aa/bb/a-hash.gif", "preview_only": true},
        "attachments": [{"name": "b.gif", "path": "/cc/dd/b-hash.gif"}],
    })))
    .unwrap();
    let flags: Vec<bool> = post.attachments.iter().map(|attachment| attachment.preview_only).collect();
    assert_eq!(flags, [true, false]);
}

// py: tests/test_kemono.py::test_build_thumbnail_url
#[test]
fn build_thumbnail_url_joins_thumbnail_path() {
    let expected = "https://img.pawchive.pw/thumbnail/data/aa/bb/hash.gif";
    assert_eq!(build_thumbnail_url("https://img.pawchive.pw", "/aa/bb/hash.gif"), expected);
    assert_eq!(build_thumbnail_url("https://img.pawchive.pw/", "/aa/bb/hash.gif"), expected);
}

// ---------- API pagination ----------

// py: tests/test_kemono.py::test_fetch_new_posts_pages_documented_endpoint
#[tokio::test]
async fn fetch_new_posts_pages_documented_endpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let web = FakeWeb::start(|seen| {
        assert_eq!(seen.path(), "/api/v1/fanbox/user/123");
        let ids: Vec<i32> = if seen.query() == "o=0" {
            (51..=100).rev().collect()
        } else {
            (41..=50).rev().collect()
        };
        listing(&ids.iter().map(|id| plain_post(&id.to_string())).collect::<Vec<_>>())
    })
    .await;
    let harness = harness(&web, unreachable_db(), tmp.path());
    let posts = harness
        .run
        .fetch_new_posts("fanbox", "123", &HashSet::new())
        .await
        .unwrap()
        .expect("creator exists");
    assert_eq!(
        web.urls(),
        [
            "http://kemono.test/api/v1/fanbox/user/123?o=0",
            "http://kemono.test/api/v1/fanbox/user/123?o=50"
        ]
    );
    let expected: Vec<String> = (41..=100).rev().map(|id: i32| id.to_string()).collect();
    assert_eq!(ids(&posts), expected);
    let seen = &web.seen()[0];
    assert_eq!(seen.header("user-agent"), Some(super::DEFAULT_USER_AGENT));
    assert_eq!(seen.header("accept"), Some("application/json"));
}

// py: tests/test_kemono.py::test_fetch_new_posts_stops_at_known_id
#[tokio::test]
async fn fetch_new_posts_stops_at_known_id() {
    let tmp = tempfile::tempdir().unwrap();
    let web = FakeWeb::start(|_| listing(&(51..=100).rev().map(|id: i32| plain_post(&id.to_string())).collect::<Vec<_>>())).await;
    let harness = harness(&web, unreachable_db(), tmp.path());
    let known: HashSet<String> = ["98".to_owned()].into();
    let posts = harness
        .run
        .fetch_new_posts("fanbox", "123", &known)
        .await
        .unwrap()
        .expect("creator exists");
    assert_eq!(web.urls().len(), 1);
    assert_eq!(ids(&posts), ["100", "99"]);
}

// py: tests/test_kemono.py::test_fetch_new_posts_retries_dropped_connection
#[tokio::test]
async fn fetch_new_posts_retries_dropped_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let web = FakeWeb::start(move |_| {
        // A reset mid-request: httpx's `ReadError`.
        if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Reply::Reset
        } else {
            listing(&[plain_post("2"), plain_post("1")])
        }
    })
    .await;
    let harness = harness(&web, unreachable_db(), tmp.path());
    let posts = harness
        .run
        .fetch_new_posts("fanbox", "123", &HashSet::new())
        .await
        .unwrap()
        .expect("creator exists");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(ids(&posts), ["2", "1"]);
    assert_eq!(harness.sleeps.delays(), [2.0]);
}

// py: tests/test_kemono.py::test_fetch_new_posts_missing_creator
#[tokio::test]
async fn fetch_new_posts_missing_creator() {
    let tmp = tempfile::tempdir().unwrap();
    let web = FakeWeb::start(|_| Reply::bytes(404, "not found")).await;
    let harness = harness(&web, unreachable_db(), tmp.path());
    assert_eq!(harness.run.fetch_new_posts("fanbox", "123", &HashSet::new()).await.unwrap(), None);
}

// ---------- downloads ----------

fn attachment(name: &str, path: &str) -> Attachment {
    Attachment {
        name: Value::from(name),
        path: Value::from(path),
        preview_only: false,
    }
}

// py: tests/test_kemono.py::test_download_attachment_verifies_sha256_and_promotes
#[tokio::test]
async fn download_attachment_verifies_sha256_and_promotes() {
    let tmp = tempfile::tempdir().unwrap();
    let content = b"attachment-bytes";
    let path = sha256_path(content, ".bin");
    let expected_path = format!("/data{path}");
    let web = FakeWeb::start(move |seen| {
        assert_eq!(seen.host(), FILE_HOST);
        assert_eq!(seen.path(), expected_path);
        Reply::bytes(200, content.to_vec())
    })
    .await;
    let harness = harness(&web, unreachable_db(), tmp.path());
    let ok = harness
        .run
        .download_attachment(&attachment("pic.bin", &path), tmp.path(), "42", 1)
        .await
        .unwrap();
    assert!(ok);
    assert_eq!(std::fs::read(tmp.path().join("pic.bin")).unwrap(), content);
    assert_eq!(names(tmp.path()), ["pic.bin"]);
}

#[tokio::test]
async fn transport_failures_are_classed_like_httpx_and_retried() {
    // A listing cut short and a file whose connection closes mid-body are both h11's
    // `RemoteProtocolError`; a server that closes before answering is too, and a reset
    // is `ReadError`. Every one of them goes through the retry ladder.
    let tmp = tempfile::tempdir().unwrap();
    let content = b"attachment-bytes";
    let path = sha256_path(content, ".bin");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let web = FakeWeb::start(move |_| match counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
        0 => Reply::Truncated {
            status: 200,
            sent: content[..4].to_vec(),
            length: content.len(),
        },
        1 => Reply::Reset,
        2 => Reply::Close,
        _ => Reply::bytes(200, content.to_vec()),
    })
    .await;
    let harness = harness(&web, unreachable_db(), tmp.path());
    let ok = harness
        .run
        .download_attachment(&attachment("pic.bin", &path), tmp.path(), "42", 1)
        .await
        .unwrap();
    assert!(ok);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 4);
    assert_eq!(harness.sleeps.delays(), [2.0, 5.0, 15.0]);
    assert_eq!(std::fs::read(tmp.path().join("pic.bin")).unwrap(), content);
    assert_eq!(names(tmp.path()), ["pic.bin"]);

    for (reply, class) in [
        (
            Reply::Truncated {
                status: 200,
                sent: b"[{".to_vec(),
                length: 100,
            },
            ErrorClass::RemoteProtocolError,
        ),
        (Reply::Reset, ErrorClass::ReadError),
        (Reply::Close, ErrorClass::RemoteProtocolError),
    ] {
        let web = FakeWeb::start(move |_| reply.clone()).await;
        let options = ClientOptions {
            proxy: Some(web.proxy.clone()),
            ..client_options()
        };
        let client = HttpClient::new(&options).unwrap();
        let error = match client.get("http://kemono.test/api/v1/fanbox/user/1/posts").await {
            Ok(response) => response.bytes().await.unwrap_err(),
            Err(error) => error,
        };
        assert_eq!(error.class, class, "{error:?}");
        assert!(super::should_retry_exception(error.class));
    }
}

#[tokio::test]
async fn requests_are_paced_by_sleep_request_seconds() {
    let tmp = tempfile::tempdir().unwrap();
    let web = FakeWeb::start(|seen| {
        if seen.query() == "o=0" {
            listing(&(0..50).map(|n| plain_post(&(100 - n).to_string())).collect::<Vec<_>>())
        } else {
            listing(&[])
        }
    })
    .await;
    let mut harness = harness(&web, unreachable_db(), tmp.path());
    harness.run.cfg.sleep_request_seconds = 1.5;
    let posts = harness
        .run
        .fetch_new_posts("fanbox", "123", &HashSet::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(posts.len(), 50);
    assert_eq!(web.urls().len(), 2);
    assert_eq!(harness.sleeps.delays(), [1.5, 1.5]);
}

// py: tests/test_kemono.py::test_download_attachment_rejects_corrupt_body
#[tokio::test]
async fn download_attachment_rejects_corrupt_body() {
    let tmp = tempfile::tempdir().unwrap();
    let path = sha256_path(b"expected-bytes", ".bin");
    let web = FakeWeb::start(|_| Reply::bytes(200, "corrupted-bytes")).await;
    let harness = harness(&web, unreachable_db(), tmp.path());
    let error = harness
        .run
        .download_attachment(&attachment("pic.bin", &path), tmp.path(), "42", 1)
        .await
        .unwrap_err();
    assert_eq!(error.class_name(), "RuntimeError");
    assert_eq!(
        error.message(),
        format!(
            "Download of {path} failed after 5 attempts: sha256 mismatch (got {})",
            hex(b"corrupted-bytes")
        )
    );
    assert_eq!(web.seen().len(), 5);
    assert_eq!(harness.sleeps.delays(), [2.0, 5.0, 15.0, 30.0]);
    assert!(names(tmp.path()).is_empty());
}

// py: tests/test_kemono.py::test_download_attachment_missing_upstream
#[tokio::test]
async fn download_attachment_missing_upstream() {
    let tmp = tempfile::tempdir().unwrap();
    let web = FakeWeb::start(|_| Reply::status(404)).await;
    let harness = harness(&web, unreachable_db(), tmp.path());
    let ok = harness
        .run
        .download_attachment(&attachment("pic.bin", &sha256_path(b"x", ".bin")), tmp.path(), "42", 1)
        .await
        .unwrap();
    assert!(!ok);
    assert!(names(tmp.path()).is_empty());
}

// ---------- update flow ----------

// py: tests/test_kemono.py::test_update_aborts_creator_on_transient_failure
#[tokio::test]
async fn update_aborts_creator_on_transient_failure() {
    let database = testutil::database().await;
    archive_post(&database, "1").await;
    let tmp = tempfile::tempdir().unwrap();
    let ok_content = b"older-post-bytes";
    let ok_path = sha256_path(ok_content, ".bin");
    let bad_path = format!("/ee/ff/{}.bin", "e".repeat(64));
    let ok_data = format!("/data{ok_path}");
    let posts = [
        post_payload("3", Some(&bad_path), Some("newer.bin"), false),
        post_payload("2", Some(&ok_path), Some("older.bin"), false),
        post_payload("1", Some(&ok_path), Some("archived.bin"), false),
    ];
    let web = FakeWeb::start(move |seen: &Seen| {
        if seen.host() == FILE_HOST {
            // The newer post's file 500s: not retryable, it fails the creator at once.
            return if seen.path() == ok_data {
                Reply::bytes(200, ok_content.to_vec())
            } else {
                Reply::status(500)
            };
        }
        listing(&posts)
    })
    .await;
    let harness = harness(&web, database.db().clone(), tmp.path());
    let error = harness.run.update().await.unwrap_err();
    assert_eq!(error.formatted(), "CrawlRunError: 1 creator(s) failed: fanbox/123");
    // Only the older post, downloaded before the failure, is recorded.
    assert_eq!(kemono_ids(&database).await, ["1", "2"]);
    let sent = notifications(&database).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(field(&sent[0], "failed_creators"), r#"["fanbox/123"]"#);
    database.drop_database().await;
}

// py: tests/test_kemono.py::test_update_skips_summary_when_there_are_no_new_posts
#[tokio::test]
async fn update_skips_summary_when_there_are_no_new_posts() {
    let database = testutil::database().await;
    archive_post(&database, "1").await;
    let tmp = tempfile::tempdir().unwrap();
    let web = FakeWeb::start(|_| listing(&[plain_post("1")])).await;
    let harness = harness(&web, database.db().clone(), tmp.path());
    harness.run.update().await.unwrap();
    assert_eq!(kemono_ids(&database).await, ["1"]);
    assert!(notifications(&database).await.is_empty());
    database.drop_database().await;
}

// py: tests/test_kemono.py::test_update_reports_missing_creator
#[tokio::test]
async fn update_reports_missing_creator() {
    let database = testutil::database().await;
    let tmp = tempfile::tempdir().unwrap();
    let web = FakeWeb::start(|_| Reply::bytes(404, "not found")).await;
    let harness = harness(&web, database.db().clone(), tmp.path());
    harness.run.update().await.unwrap();
    assert!(kemono_ids(&database).await.is_empty());
    let sent = notifications(&database).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(field(&sent[0], "missing_creators"), r#"["fanbox/123"]"#);
    assert_eq!(field(&sent[0], "failed_creators"), "[]");
    database.drop_database().await;
}

// py: tests/test_kemono.py::test_downloaded_post_survives_missing_attachment
#[tokio::test]
async fn downloaded_post_survives_missing_attachment() {
    let database = testutil::database().await;
    let tmp = tempfile::tempdir().unwrap();
    let path = format!("/aa/bb/{}.bin", "a".repeat(64));
    let posts = [post_payload("7", Some(&path), Some("gone.bin"), false)];
    let web = FakeWeb::start(move |seen| {
        if seen.host() == FILE_HOST {
            Reply::status(404)
        } else {
            listing(&posts)
        }
    })
    .await;
    let harness = harness(&web, database.db().clone(), tmp.path());
    harness.run.update().await.unwrap();
    assert_eq!(kemono_ids(&database).await, ["7"]);
    let sent = notifications(&database).await;
    assert_eq!(field(&sent[0], "missing_files"), "1");
    database.drop_database().await;
}

// ---------- preview-only attachments ----------

// py: tests/test_kemono.py::test_update_archives_preview_only_thumbnail_and_queues_recheck
#[tokio::test]
async fn update_archives_preview_only_thumbnail_and_queues_recheck() {
    let database = testutil::database().await;
    let tmp = tempfile::tempdir().unwrap();
    let content = b"thumbnail-bytes";
    let path = format!("/aa/bb/{}.gif", "a".repeat(64));
    let thumbnail = format!("/thumbnail/data{path}");
    let posts = [post_payload("9", Some(&path), Some("anim.gif"), true)];
    let web = FakeWeb::start(move |seen| {
        if seen.host() == THUMB_HOST {
            assert_eq!(seen.path(), thumbnail);
            return Reply::bytes(200, content.to_vec());
        }
        assert_ne!(
            seen.host(),
            FILE_HOST,
            "preview-only attachments must not hit the file host during the crawl"
        );
        listing(&posts)
    })
    .await;
    let harness = harness(&web, database.db().clone(), tmp.path());
    harness.run.update().await.unwrap();
    assert_eq!(kemono_ids(&database).await, ["9"]);
    assert_eq!(
        std::fs::read(tmp.path().join("fanbox/tester_123/post 9 [9]/anim.gif")).unwrap(),
        content
    );
    let pending = pending_rows(&database).await;
    assert_eq!(pending.len(), 1);
    let (service, user_id, post_id, pending_path, dst, attempts, next_check_at) = pending[0].clone();
    assert_eq!(
        (service.as_str(), user_id.as_str(), post_id.as_str(), pending_path.as_str()),
        ("fanbox", "123", "9", path.as_str())
    );
    assert_eq!(dst, "fanbox/tester_123/post 9 [9]/anim.gif");
    assert_eq!(attempts, 0);
    let gap = gap(next_check_at);
    assert!(chrono::Duration::hours(23) < gap && gap < chrono::Duration::hours(25), "{gap}");
    let sent = notifications(&database).await;
    assert_eq!(field(&sent[0], "preview_files"), "1");
    assert_eq!(field(&sent[0], "missing_files"), "0");
    assert_eq!(field(&sent[0], "failed_creators"), "[]");
    database.drop_database().await;
}

// py: tests/test_kemono.py::test_preview_download_failure_does_not_fail_the_creator
#[tokio::test]
async fn preview_download_failure_does_not_fail_the_creator() {
    let database = testutil::database().await;
    let tmp = tempfile::tempdir().unwrap();
    let path = format!("/aa/bb/{}.gif", "b".repeat(64));
    let posts = [post_payload("9", Some(&path), Some("anim.gif"), true)];
    let web = FakeWeb::start(move |seen| {
        if seen.host() == THUMB_HOST {
            Reply::status(403)
        } else {
            listing(&posts)
        }
    })
    .await;
    let harness = harness(&web, database.db().clone(), tmp.path());
    harness.run.update().await.unwrap();
    assert_eq!(kemono_ids(&database).await, ["9"]);
    assert_eq!(pending_rows(&database).await.len(), 1);
    let sent = notifications(&database).await;
    assert_eq!(field(&sent[0], "missing_files"), "1");
    assert_eq!(field(&sent[0], "failed_creators"), "[]");
    database.drop_database().await;
}

// py: tests/test_kemono.py::test_promotion_replaces_preview_with_original
#[tokio::test]
async fn promotion_replaces_preview_with_original() {
    let database = testutil::database().await;
    archive_post(&database, "1").await;
    let tmp = tempfile::tempdir().unwrap();
    let original = b"original-bytes";
    let path = sha256_path(original, ".gif");
    let dst = "fanbox/tester_123/post 9 [9]/anim.gif";
    let preview = tmp.path().join(dst);
    std::fs::create_dir_all(preview.parent().unwrap()).unwrap();
    std::fs::write(&preview, b"preview-bytes").unwrap();
    queue_pending(&database, &path, dst, 0).await;
    let web = FakeWeb::start(move |seen| {
        if seen.host() == FILE_HOST {
            Reply::bytes(200, original.to_vec())
        } else {
            listing(&[plain_post("1")])
        }
    })
    .await;
    let harness = harness(&web, database.db().clone(), tmp.path());
    harness.run.update().await.unwrap();
    assert_eq!(std::fs::read(&preview).unwrap(), original);
    assert!(pending_rows(&database).await.is_empty());
    let sent = notifications(&database).await;
    assert_eq!(field(&sent[0], "promoted_originals"), "1");
    database.drop_database().await;
}

// py: tests/test_kemono.py::test_promotion_failure_advances_schedule_without_raising
#[tokio::test]
async fn promotion_failure_advances_schedule_without_raising() {
    let database = testutil::database().await;
    let tmp = tempfile::tempdir().unwrap();
    let path = format!("/aa/bb/{}.gif", "c".repeat(64));
    queue_pending(&database, &path, "fanbox/tester_123/post 9 [9]/anim.gif", 0).await;
    let web = FakeWeb::start(|_| Reply::status(403)).await;
    let harness = harness(&web, database.db().clone(), tmp.path());
    assert_eq!(harness.run.promote_pending_originals().await.unwrap(), 0);
    let pending = pending_rows(&database).await;
    assert_eq!(pending.len(), 1);
    let (service, user_id, post_id, pending_path, _, attempts, next_check_at) = pending[0].clone();
    assert_eq!(attempts, 1);
    assert_eq!(
        (service.as_str(), user_id.as_str(), post_id.as_str(), pending_path.as_str()),
        ("fanbox", "123", "9", path.as_str())
    );
    // The second check follows the 3-day gap of the 1/3/5/7/7/7/... ladder.
    let gap = gap(next_check_at);
    assert!(chrono::Duration::hours(71) < gap && gap < chrono::Duration::hours(73), "{gap}");
    database.drop_database().await;
}

// py: tests/test_kemono.py::test_promotion_keeps_checking_every_seven_days_forever
#[tokio::test]
async fn promotion_keeps_checking_every_seven_days_forever() {
    let database = testutil::database().await;
    let tmp = tempfile::tempdir().unwrap();
    let path = format!("/aa/bb/{}.gif", "d".repeat(64));
    queue_pending(&database, &path, "fanbox/tester_123/post 9 [9]/anim.gif", 41).await;
    let web = FakeWeb::start(|_| Reply::status(404)).await;
    let harness = harness(&web, database.db().clone(), tmp.path());
    assert_eq!(harness.run.promote_pending_originals().await.unwrap(), 0);
    let pending = pending_rows(&database).await;
    assert_eq!(pending.len(), 1);
    let (_, _, _, _, _, attempts, next_check_at) = pending[0].clone();
    assert_eq!(attempts, 42);
    let gap = gap(next_check_at);
    assert!(
        chrono::Duration::hours(7 * 24 - 1) < gap && gap < chrono::Duration::hours(7 * 24 + 1),
        "{gap}"
    );
    database.drop_database().await;
}

// ---------- Python's answers for the pure helpers (`goldens/kemono/`) ----------

fn post_value(post: &Post) -> Value {
    let mut dict = pyjson::Dict::new();
    dict.insert("id", Value::from(post.id.as_str()));
    dict.insert("user", Value::from(post.user.as_str()));
    dict.insert("service", Value::from(post.service.as_str()));
    dict.insert("title", post.title.clone());
    dict.insert("published", post.published.clone());
    let attachments = post
        .attachments
        .iter()
        .map(|attachment| {
            let mut item = pyjson::Dict::new();
            item.insert("name", attachment.name.clone());
            item.insert("path", attachment.path.clone());
            item.insert("preview_only", Value::Bool(attachment.preview_only));
            Value::Dict(item)
        })
        .collect();
    dict.insert("attachments", Value::List(attachments));
    Value::Dict(dict)
}

#[test]
fn kemono_parsing_matches_python() {
    use fav_core::golden::{cases, get, load, string};
    let document = load("kemono/parsing.json");
    let null = Value::Null;
    for case in cases(&document) {
        let expected = case.get("result").unwrap_or(&null);
        if let Some(data) = case.get("parse_post") {
            let actual = parse_post(data).map_or_else(|error| Value::Str(error.formatted()), |post| post_value(&post));
            assert_eq!(&actual, expected, "{data:?}");
        } else if let Some(path) = case.get("sha256_from_path") {
            let path = string(path);
            assert_eq!(sha256_from_path(&path).map_or(Value::Null, Value::Str), *expected, "{path}");
        } else if let Some(name) = case.get("attachment_filename") {
            let name = string(name);
            assert_eq!(Value::Str(attachment_filename(&name, 3)), *expected, "{name}");
        } else if let Some(base) = case.get("base") {
            let base = string(base);
            assert_eq!(
                Value::Str(build_file_url(&base, "/aa/bb/hash.jpeg")),
                *get(case, "file_url"),
                "{base}"
            );
            assert_eq!(
                Value::Str(build_thumbnail_url(&base, "/aa/bb/hash.jpeg")),
                *get(case, "thumbnail_url"),
                "{base}"
            );
        } else if let Some(header) = case.get("retry_after") {
            let header = header.as_str();
            let actual = retry_after_seconds(header).map_or_else(
                |error| Value::Str(error.formatted()),
                |seconds| seconds.map_or(Value::Null, Value::Float),
            );
            assert_eq!(&actual, expected, "{header:?}");
        } else if let Some(date) = case.get("parsedate") {
            let date = string(date);
            let actual = fav_net::retry::parsedate_to_datetime(&date).map_or(Value::Null, |parsed| {
                Value::Str(parsed.format("%Y-%m-%dT%H:%M:%S+00:00").to_string())
            });
            assert_eq!(&actual, expected, "{date:?}");
        } else {
            let raw = string(get(case, "creator"));
            let actual = match crate::kemono_creator::extract_kemono_creator(&raw) {
                Ok(Some((service, id))) => Value::List(vec![Value::Str(service), Value::Str(id)]),
                Ok(None) => Value::Null,
                Err(message) => Value::Str(format!("ValueError: {message}")),
            };
            assert_eq!(&actual, expected, "{raw:?}");
        }
    }
}
