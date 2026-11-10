use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fav_core::pyjson::{self, Dict, Value};
use fav_core::settings::{SectionValue, Settings};
use fav_core::validate::Model;
use fav_db::Db;
use fav_db::testdb::TestDatabase;
use fav_runtime::site_sessions::StoredCookie;

use super::*;
use crate::testkit::{Answer, FakeServer, RecordingNotifier};

// ---------- fixtures ----------

fn json(text: &str) -> Value {
    pyjson::loads(text).expect("test JSON")
}

fn work() -> PixivWork {
    PixivWork {
        illust_id: 96_461_036,
        title: "a title".to_owned(),
        author: "the artist".to_owned(),
        author_id: "50944445".to_owned(),
        illust_type: 0,
        page_count: 1,
        masked: false,
    }
}

fn bookmark_entry(id: &str) -> String {
    format!(
        r#"{{"id": "{id}", "title": "a title", "userId": "50944445", "userName": "the artist", "illustType": 0, "pageCount": 1, "isMasked": false}}"#
    )
}

fn no_waiting() -> Pacing {
    Pacing {
        sleep_request: Duration::ZERO,
        retry_delays: vec![Duration::ZERO; 4],
    }
}

fn config(path: &str, user_id: &str) -> PixivConfig {
    PixivConfig {
        path: path.to_owned(),
        user_id: user_id.to_owned(),
        sleep_request_seconds: 0.0,
        proxy: String::new(),
    }
}

/// A database nothing listens on, for tests that never reach it.
fn unused_db() -> Db {
    Db::connect_lazy("postgresql://tests@127.0.0.1:1/unused").expect("lazy pool")
}

fn pixiv(db: Db, ajax_base: &str, path: &str, user_id: &str) -> Pixiv {
    Pixiv::new(db, Arc::new(RecordingNotifier::default()), &config(path, user_id))
        .expect("pixiv")
        .with_ajax_base(ajax_base, no_waiting())
}

async fn database() -> TestDatabase {
    let database = TestDatabase::create().await;
    database.db().ensure_schema().await.expect("schema applies");
    database
}

async fn store_login(db: &Db, phpsessid: &str) {
    let cookie = StoredCookie {
        name: "PHPSESSID".to_owned(),
        value: phpsessid.to_owned(),
        domain: ".pixiv.net".to_owned(),
        path: "/".to_owned(),
        expires: None,
    };
    site_sessions::save_signed_in(db, "pixiv", &[cookie], "", None, &[])
        .await
        .expect("store the login");
}

async fn stored_check(db: &Db) -> (String, String) {
    let session = site_sessions::load(db, "pixiv").await.expect("load").expect("a stored login");
    (session.status, session.message)
}

fn temp_root() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8 path").to_owned();
    (dir, path)
}

fn write_frames(directory: &Path, colors: &[[u8; 3]]) -> Vec<PathBuf> {
    colors
        .iter()
        .enumerate()
        .map(|(index, color)| {
            let path = directory.join(format!("{index:06}.png"));
            image::RgbImage::from_pixel(8, 8, image::Rgb(*color))
                .save(&path)
                .expect("write a frame");
            path
        })
        .collect()
}

fn write_zip(path: &Path, members: &[(&str, Vec<u8>)]) {
    let mut archive = zip::ZipWriter::new(std::fs::File::create(path).expect("zip file"));
    for (name, bytes) in members {
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        archive.start_file(*name, options).expect("zip member");
        archive.write_all(bytes).expect("zip bytes");
    }
    archive.finish().expect("zip finish");
}

/// Each frame's end time, as libwebp's demuxer reports it.
fn frame_end_times(path: &Path) -> Vec<i32> {
    let bytes = std::fs::read(path).expect("webp");
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WEBP");
    let decoder = webp_animation::Decoder::new(&bytes).expect("decodes");
    decoder.into_iter().map(|frame| frame.timestamp()).collect()
}

fn is_animated(path: &Path) -> bool {
    let bytes = std::fs::read(path).expect("webp");
    bytes.windows(4).any(|window| window == b"ANIM")
}

// ---------- configuration ----------

fn pixiv_section(pairs: &[(&str, Value)]) -> Result<fav_core::settings::Pixiv, String> {
    let mut dict = Dict::new();
    for (key, value) in pairs {
        dict.insert(*key, value.clone());
    }
    fav_core::settings::Pixiv::validate(&Value::Dict(dict)).map_err(|error| error.to_string())
}

// py: tests/test_pixiv.py::test_nothing_in_the_settings_gates_runnability
#[test]
fn nothing_in_the_settings_gates_runnability() {
    assert!(pixiv_section(&[]).unwrap().validate_runnable().is_empty());
}

// py: tests/test_pixiv.py::test_a_cookiecloud_reference_left_in_an_older_row_is_ignored
#[test]
fn a_cookiecloud_reference_left_in_an_older_row_is_ignored() {
    let cfg = pixiv_section(&[("cookiecloud", Value::from("cc")), ("user_id", Value::from("21029043"))]).unwrap();

    assert_eq!(cfg.user_id, "21029043");
    assert!(cfg.dump().get("cookiecloud").is_none());
}

// py: tests/test_pixiv.py::test_a_user_id_override_must_be_digits
#[test]
fn a_user_id_override_must_be_digits() {
    assert_eq!(
        pixiv_section(&[("user_id", Value::from(" 21029043 "))]).unwrap().user_id,
        "21029043"
    );
    assert_eq!(pixiv_section(&[("user_id", Value::from(""))]).unwrap().user_id, "");
    assert!(
        pixiv_section(&[("user_id", Value::from("not-a-uid"))])
            .unwrap_err()
            .contains("user_id")
    );
}

// py: tests/test_pixiv.py::test_a_negative_request_interval_is_rejected
#[test]
fn a_negative_request_interval_is_rejected() {
    assert!(
        pixiv_section(&[("sleep_request_seconds", Value::from(-1_i64))])
            .unwrap_err()
            .contains("sleep_request_seconds")
    );
}

// ---------- session parsing ----------

// py: tests/test_pixiv.py::test_the_user_id_is_derived_from_the_session_cookie
#[test]
fn the_user_id_is_derived_from_the_session_cookie() {
    assert_eq!(derive_user_id("21029043_AbCdEf0123456789AbCdEf0123456789"), "21029043");
    assert_eq!(derive_user_id("no-underscore"), "");
    assert_eq!(derive_user_id("prefix_notdigits"), "");
    assert_eq!(derive_user_id(""), "");
}

// ---------- payload parsing ----------

// py: tests/test_pixiv.py::test_a_bookmark_page_becomes_typed_works
#[test]
fn a_bookmark_page_becomes_typed_works() {
    let anim = r#"{"id": "2", "title": "anim", "userId": "50944445", "userName": "the artist", "illustType": 2, "pageCount": 1, "isMasked": false}"#;
    let body = json(&format!(r#"{{"works": [{}, {anim}]}}"#, bookmark_entry("96461036")));

    let works = parse_bookmark_works(&body).unwrap();

    assert_eq!(works.iter().map(|work| work.illust_id).collect::<Vec<_>>(), [96_461_036, 2]);
    assert_eq!(works[0].author, "the artist");
    assert_eq!(works[0].author_id, "50944445");
    assert_eq!(works[1].illust_type, 2);
}

// py: tests/test_pixiv.py::test_a_masked_bookmark_is_kept_even_though_its_id_is_an_int
#[test]
fn a_masked_bookmark_is_kept_even_though_its_id_is_an_int() {
    let body = json(r#"{"works": [{"id": 130954211, "title": "-----", "isMasked": true}]}"#);

    let works = parse_bookmark_works(&body).unwrap();

    assert_eq!(works.len(), 1);
    assert_eq!(works[0].illust_id, 130_954_211);
    assert!(works[0].masked);
}

// py: tests/test_pixiv.py::test_an_unknown_work_type_is_skipped_rather_than_downloaded_wrong
#[test]
fn an_unknown_work_type_is_skipped_rather_than_downloaded_wrong() {
    let unknown = bookmark_entry("96461036").replace(r#""illustType": 0"#, r#""illustType": 7"#);
    let body = json(&format!(r#"{{"works": [{unknown}, {}]}}"#, bookmark_entry("5")));

    assert_eq!(
        parse_bookmark_works(&body)
            .unwrap()
            .iter()
            .map(|work| work.illust_id)
            .collect::<Vec<_>>(),
        [5]
    );
}

// py: tests/test_pixiv.py::test_page_urls_keep_their_order
#[test]
fn page_urls_keep_their_order() {
    let body = json(
        r#"[{"urls": {"original": "https://i.pximg.net/img-original/img/x/1_p0.jpg"}},
            {"urls": {"original": "https://i.pximg.net/img-original/img/x/1_p1.png"}}]"#,
    );

    assert_eq!(
        parse_page_urls(&body),
        [
            "https://i.pximg.net/img-original/img/x/1_p0.jpg",
            "https://i.pximg.net/img-original/img/x/1_p1.png"
        ]
    );
    assert!(parse_page_urls(&Value::Null).is_empty());
}

// py: tests/test_pixiv.py::test_ugoira_meta_yields_the_zip_and_the_frame_delays
#[test]
fn ugoira_meta_yields_the_zip_and_the_frame_delays() {
    let body = json(
        r#"{"originalSrc": "https://i.pximg.net/img-zip-ugoira/img/x/2_ugoira1920x1080.zip",
            "frames": [{"file": "000000.jpg", "delay": 45}, {"file": "000001.jpg", "delay": 90}]}"#,
    );

    let (zip_url, frames) = parse_ugoira_meta(&body).unwrap();

    assert!(zip_url.ends_with("_ugoira1920x1080.zip"));
    assert_eq!(frames, [("000000.jpg".to_owned(), 45), ("000001.jpg".to_owned(), 90)]);
}

// py: tests/test_pixiv.py::test_an_empty_ugoira_meta_raises_instead_of_encoding_nothing
#[test]
fn an_empty_ugoira_meta_raises_instead_of_encoding_nothing() {
    let error = parse_ugoira_meta(&json(r#"{"originalSrc": "", "frames": []}"#)).unwrap_err();

    assert_eq!(error.class_name(), "ValueError");
    assert!(error.message().contains("ugoira_meta"));
}

// py: tests/test_pixiv.py::test_the_extension_comes_from_the_original_url
#[test]
fn the_extension_comes_from_the_original_url() {
    assert_eq!(infer_extension("https://i.pximg.net/img-original/img/x/1_p0.png").unwrap(), "png");
    assert_eq!(infer_extension("https://i.pximg.net/no-extension").unwrap(), "jpg");
}

// ---------- ugoira synthesis ----------

// py: tests/test_pixiv.py::test_frames_become_an_animated_webp_honoring_each_delay
#[test]
fn frames_become_an_animated_webp_honoring_each_delay() {
    let dir = tempfile::tempdir().unwrap();
    let frame_paths = write_frames(dir.path(), &[[255, 0, 0], [0, 255, 0], [0, 0, 255]]);
    let dst = dir.path().join("out.webp");

    synthesize_ugoira_webp(&frame_paths, &[100, 200, 300], &dst).unwrap();

    assert!(is_animated(&dst));
    // Durations 100, 200 and 300 ms: the frames end at 100, 300 and 600.
    assert_eq!(frame_end_times(&dst), [100, 300, 600]);
}

#[test]
fn a_single_frame_ugoira_becomes_a_still_webp_as_with_pillow() {
    let dir = tempfile::tempdir().unwrap();
    let frame_paths = write_frames(dir.path(), &[[255, 0, 0]]);
    let dst = dir.path().join("out.webp");

    synthesize_ugoira_webp(&frame_paths, &[100], &dst).unwrap();

    assert!(!is_animated(&dst));
    assert_eq!(frame_end_times(&dst).len(), 1);
}

// py: tests/test_pixiv.py::test_the_zip_pipeline_extracts_encodes_and_moves_into_place
#[test]
fn the_zip_pipeline_extracts_encodes_and_moves_into_place() {
    let dir = tempfile::tempdir().unwrap();
    let frame_paths = write_frames(dir.path(), &[[255, 0, 0], [0, 0, 255]]);
    let work_dir = tempfile::tempdir().unwrap();
    let zip_path = work_dir.path().join("ugoira.zip");
    let members: Vec<(&str, Vec<u8>)> = vec![
        ("000000.png", std::fs::read(&frame_paths[0]).unwrap()),
        ("000001.png", std::fs::read(&frame_paths[1]).unwrap()),
    ];
    write_zip(&zip_path, &members);
    let dst = dir.path().join("nested").join("out.webp");
    std::fs::create_dir(dir.path().join("nested")).unwrap();

    let frames = [("000000.png".to_owned(), 50), ("000001.png".to_owned(), 50)];
    synthesize_from_zip(work_dir.path(), &zip_path, &frames, &dst).unwrap();

    assert!(is_animated(&dst));
    assert_eq!(frame_end_times(&dst).len(), 2);
}

// py: tests/test_pixiv.py::test_a_zip_missing_a_listed_frame_fails_loudly
#[test]
fn a_zip_missing_a_listed_frame_fails_loudly() {
    let work_dir = tempfile::tempdir().unwrap();
    let zip_path = work_dir.path().join("ugoira.zip");
    write_zip(&zip_path, &[("000000.png", b"x".to_vec())]);

    let error = synthesize_from_zip(
        work_dir.path(),
        &zip_path,
        &[("000001.png".to_owned(), 50)],
        &work_dir.path().join("out.webp"),
    )
    .unwrap_err();

    assert_eq!(error.class_name(), "PixivError");
    assert!(error.message().contains("missing frame"));
}

#[test]
fn zip_members_cannot_escape_the_work_directory() {
    let outer = tempfile::tempdir().unwrap();
    let work_dir = outer.path().join("work");
    std::fs::create_dir(&work_dir).unwrap();
    let zip_path = work_dir.join("ugoira.zip");
    write_zip(&zip_path, &[("../escaped.png", b"x".to_vec()), ("/abs/000000.png", b"y".to_vec())]);

    extract_zip(&zip_path, &work_dir).unwrap();

    assert!(!outer.path().join("escaped.png").exists());
    assert_eq!(std::fs::read(work_dir.join("escaped.png")).unwrap(), b"x");
    assert_eq!(std::fs::read(work_dir.join("abs").join("000000.png")).unwrap(), b"y");
}

// ---------- filenames ----------

// py: tests/test_pixiv.py::test_files_land_under_the_author_with_the_repo_filename_shape
#[tokio::test]
async fn files_land_under_the_author_with_the_repo_filename_shape() {
    let source = pixiv(unused_db(), AJAX_BASE, "/data/pixiv", "");

    let illust = source.build_output_path(&PixivWork { page_count: 2, ..work() }, 1, "png");
    assert_eq!(illust, "/data/pixiv/the artist/[the artist]a title [96461036_p1].png");

    let ugoira = source.build_output_path(&PixivWork { illust_type: 2, ..work() }, 0, "webp");
    assert_eq!(ugoira, "/data/pixiv/the artist/[the artist]a title [96461036_ugoira].webp");
}

// py: tests/test_pixiv.py::test_an_author_without_a_name_falls_back_to_the_id_directory
#[tokio::test]
async fn an_author_without_a_name_falls_back_to_the_id_directory() {
    let source = pixiv(unused_db(), AJAX_BASE, "/data/pixiv", "");

    let path = source.build_output_path(
        &PixivWork {
            author: String::new(),
            author_id: "42".to_owned(),
            ..work()
        },
        0,
        "jpg",
    );

    // No empty [] prefix either: an unnamed author drops the uploader segment.
    assert_eq!(path, "/data/pixiv/42/a title [96461036_p0].jpg");
}

// ---------- clients ----------

// py: tests/test_pixiv.py::test_the_media_client_carries_the_referer_but_never_the_session
#[tokio::test]
async fn the_media_client_carries_the_referer_but_never_the_session() {
    let (_dir, root) = temp_root();
    let server = FakeServer::start(|request| {
        if request.path.starts_with("/ajax/") {
            Answer::json(200, r#"{"error": false, "body": {"total": 0, "works": []}}"#)
        } else {
            Answer::bytes(200, b"image-bytes")
        }
    })
    .await;
    let mut source = pixiv(unused_db(), &server.base_url, &root, "");
    source.cookie = Some("PHPSESSID=21029043_secret".to_owned());

    source.fetch_bookmark_page("21029043", 0).await.unwrap();
    source
        .download_image(&server.url("/img/1_p0.jpg"), &pypath::join(&root, "a.jpg"))
        .await
        .unwrap();

    let received = server.received();
    assert_eq!(received[0].header("referer"), Some("https://www.pixiv.net/"));
    assert_eq!(received[0].header("cookie"), Some("PHPSESSID=21029043_secret"));
    assert_eq!(received[1].header("referer"), Some("https://www.pixiv.net/"));
    assert_eq!(received[1].header("cookie"), None);
}

// py: tests/test_pixiv.py::test_the_bookmarks_request_asks_for_public_bookmarks_page_by_page
#[tokio::test]
async fn the_bookmarks_request_asks_for_public_bookmarks_page_by_page() {
    let server = FakeServer::start(|_| Answer::json(200, r#"{"error": false, "body": {"total": 1, "works": []}}"#)).await;
    let source = pixiv(unused_db(), &server.base_url, "/data/pixiv", "");

    let body = source.fetch_bookmark_page("21029043", 96).await.unwrap();

    assert_eq!(body, json(r#"{"total": 1, "works": []}"#));
    let request = &server.received()[0];
    assert_eq!(request.path, "/ajax/user/21029043/illusts/bookmarks");
    assert_eq!(request.param("rest"), Some("show"));
    assert_eq!(request.param("limit"), Some("48"));
    assert_eq!(request.param("offset"), Some("96"));
    assert_eq!(request.param("tag"), Some(""));
}

// py: tests/test_pixiv.py::test_a_rejected_session_becomes_one_deduplicated_auth_alert
#[tokio::test]
async fn a_rejected_session_becomes_one_deduplicated_auth_alert() {
    let server = FakeServer::start(|_| Answer::json(400, r#"{"error": true, "message": "invalid request", "body": []}"#)).await;
    let source = pixiv(unused_db(), &server.base_url, "/data/pixiv", "");

    let error = source.fetch_bookmark_page("21029043", 0).await.unwrap_err();

    let Failure::Pixiv(error) = error else {
        panic!("a PixivError, got {error:?}")
    };
    assert_eq!(error.notification_dedupe_key, "pixiv:auth");
}

// py: tests/test_pixiv.py::test_a_work_endpoint_error_keeps_its_message_for_the_unavailable_mark
#[tokio::test]
async fn a_work_endpoint_error_keeps_its_message_for_the_unavailable_mark() {
    let server = FakeServer::start(|_| Answer::json(404, r#"{"error": true, "message": "deleted", "body": []}"#)).await;
    let source = pixiv(unused_db(), &server.base_url, "/data/pixiv", "");

    let error = source.get_body(&server.url("/ajax/illust/1/pages"), &[]).await.unwrap_err();

    assert_eq!(
        error,
        Failure::Api {
            message: "deleted".to_owned(),
            status_code: 404
        }
    );
}

#[tokio::test]
async fn throttling_is_retried_and_a_lasting_server_error_is_an_http_status_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let server = FakeServer::start(move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            Answer::json(429, "{}")
        } else {
            Answer::json(500, "not json")
        }
    })
    .await;
    let source = pixiv(unused_db(), &server.base_url, "/data/pixiv", "");

    let error = source.get_body(&server.url("/ajax/illust/1/pages"), &[]).await.unwrap_err();

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(error.class_name(), "HTTPStatusError");
    assert!(
        error.message().starts_with("Server error '500 Internal Server Error' for url '"),
        "{}",
        error.message()
    );
}

// py: tests/test_pixiv.py::test_a_download_writes_the_original_and_skips_files_already_on_disk
#[tokio::test]
async fn a_download_writes_the_original_and_skips_files_already_on_disk() {
    let (_dir, root) = temp_root();
    let server = FakeServer::start(|_| Answer::bytes(200, b"image-bytes")).await;
    let source = pixiv(unused_db(), &server.base_url, &root, "");
    let dst = pypath::join(&pypath::join(&root, "artist"), "file.jpg");
    let url = server.url("/img-original/img/x/1_p0.jpg");

    assert!(source.download_image(&url, &dst).await.unwrap());
    assert_eq!(std::fs::read(&dst).unwrap(), b"image-bytes");
    let first = &server.received()[0];
    assert_eq!(first.header("referer"), Some("https://www.pixiv.net/"));
    assert_eq!(first.header("cookie"), None);

    // Already on disk: no second request, no rewrite.
    assert!(!source.download_image(&url, &dst).await.unwrap());
    assert_eq!(server.received().len(), 1);
}

#[tokio::test]
async fn a_failed_download_leaves_neither_the_file_nor_its_part() {
    let (_dir, root) = temp_root();
    let server = FakeServer::start(|_| Answer::bytes(403, b"no")).await;
    let source = pixiv(unused_db(), &server.base_url, &root, "");
    let dst = pypath::join(&root, "file.jpg");

    let error = source.download_image(&server.url("/x.jpg"), &dst).await.unwrap_err();

    assert_eq!(error.class_name(), "HTTPStatusError");
    assert!(!Path::new(&dst).exists());
    assert!(!Path::new(&format!("{dst}.part")).exists());
}

// ---------- crawl ----------

// py: tests/test_pixiv.py::test_the_crawl_stops_after_consecutive_fully_archived_pages
#[tokio::test]
async fn the_crawl_stops_after_consecutive_fully_archived_pages() {
    let database = database().await;
    // Every work the pages list is already archived.
    for illust_id in 1..=4_i64 {
        sqlx::query("INSERT INTO pixiv (illust_id, num, downloaded) VALUES ($1, 0, 1);")
            .bind(illust_id)
            .execute(database.db().pool())
            .await
            .unwrap();
    }
    let server = FakeServer::start(|request| {
        let offset: i64 = request.param("offset").unwrap().parse().unwrap();
        let works: Vec<String> = (1..=2).map(|index| bookmark_entry(&(offset + index).to_string())).collect();
        Answer::json(
            200,
            &format!(r#"{{"error": false, "body": {{"total": 1000, "works": [{}]}}}}"#, works.join(",")),
        )
    })
    .await;
    let source = pixiv(database.db().clone(), &server.base_url, "/data/pixiv", "");

    let (works, pages, total) = source.crawl_bookmarks("21029043").await.unwrap();

    assert!(works.is_empty());
    assert_eq!(pages, 2);
    assert_eq!(total, 1000);
    let offsets: Vec<String> = server
        .received()
        .iter()
        .map(|request| request.param("offset").unwrap().to_owned())
        .collect();
    assert_eq!(offsets, ["0", "2"]);
    database.drop_database().await;
}

// py: tests/test_pixiv.py::test_a_masked_bookmark_is_settled_as_unavailable_during_the_walk
#[tokio::test]
async fn a_masked_bookmark_is_settled_as_unavailable_during_the_walk() {
    let database = database().await;
    let server = FakeServer::start(|request| {
        if request.param("offset") == Some("0") {
            Answer::json(
                200,
                r#"{"error": false, "body": {"total": 1, "works": [{"id": 130954211, "title": "-----", "isMasked": true}]}}"#,
            )
        } else {
            Answer::json(200, r#"{"error": false, "body": {"total": 1, "works": []}}"#)
        }
    })
    .await;
    let source = pixiv(database.db().clone(), &server.base_url, "/data/pixiv", "");

    let (works, _pages, _total) = source.crawl_bookmarks("21029043").await.unwrap();

    assert!(works.is_empty());
    let row = sqlx::query("SELECT num, unavailable, downloaded, last_error FROM pixiv WHERE illust_id = 130954211;")
        .fetch_one(database.db().pool())
        .await
        .unwrap();
    assert_eq!(row.get::<i32, _>("num"), 0);
    assert_eq!(row.get::<i32, _>("unavailable"), 1);
    assert_eq!(row.get::<i32, _>("downloaded"), 0);
    assert_eq!(row.get::<String, _>("last_error"), "masked in bookmarks");
    database.drop_database().await;
}

// py: tests/test_pixiv.py::test_with_no_captured_login_the_run_asks_for_a_login
#[tokio::test]
async fn with_no_captured_login_the_run_asks_for_a_login() {
    let database = database().await;
    let (_dir, root) = temp_root();
    let server = FakeServer::start(|_| Answer::json(500, "{}")).await;
    let mut source = pixiv(database.db().clone(), &server.base_url, &root, "");

    let error = source.update().await.unwrap_err();

    assert_eq!(error.class_name(), "PixivError");
    assert!(error.message().contains("登录态"));
    assert_eq!(error.notification_dedupe_key(), Some("pixiv:auth"));
    assert!(server.received().is_empty());
    database.drop_database().await;
}

// py: tests/test_pixiv.py::test_a_captured_login_is_crawled_with_and_marked_valid
#[tokio::test]
async fn a_captured_login_is_crawled_with_and_marked_valid() {
    let database = database().await;
    store_login(database.db(), "21029043_captured").await;
    site_sessions::record_check(database.db(), "pixiv", false, "stale").await.unwrap();
    let (_dir, root) = temp_root();
    let server = FakeServer::start(|_| Answer::json(200, r#"{"error": false, "body": {"total": 0, "works": []}}"#)).await;
    let mut source = pixiv(database.db().clone(), &server.base_url, &root, "");

    source.update().await.unwrap();

    let request = &server.received()[0];
    assert_eq!(request.path, "/ajax/user/21029043/illusts/bookmarks");
    assert_eq!(request.header("cookie"), Some("PHPSESSID=21029043_captured"));
    assert_eq!(stored_check(database.db()).await, ("valid".to_owned(), String::new()));
    database.drop_database().await;
}

// py: tests/test_pixiv.py::test_a_rejected_captured_login_is_marked_invalid
#[tokio::test]
async fn a_rejected_captured_login_is_marked_invalid() {
    let database = database().await;
    store_login(database.db(), "21029043_stale").await;
    let (_dir, root) = temp_root();
    let server = FakeServer::start(|_| Answer::json(403, r#"{"error": true, "message": "forbidden", "body": []}"#)).await;
    let mut source = pixiv(database.db().clone(), &server.base_url, &root, "");

    let error = source.update().await.unwrap_err();

    assert_eq!(error.class_name(), "PixivError");
    assert_eq!(
        stored_check(database.db()).await,
        ("invalid".to_owned(), error.message().to_owned())
    );
    database.drop_database().await;
}

// py: tests/test_pixiv.py::test_a_session_without_an_embedded_uid_asks_for_an_explicit_override
#[tokio::test]
async fn a_session_without_an_embedded_uid_asks_for_an_explicit_override() {
    let database = database().await;
    store_login(database.db(), "strange-cookie-shape").await;
    let (_dir, root) = temp_root();
    let mut source = pixiv(database.db().clone(), "http://127.0.0.1:9", &root, "");

    let error = source.update().await.unwrap_err();

    assert!(error.message().contains("user_id"));
    assert_eq!(error.notification_dedupe_key(), Some("pixiv:auth"));
    database.drop_database().await;
}

#[tokio::test]
async fn a_run_downloads_new_works_settles_gone_ones_and_reports_once() {
    let database = database().await;
    store_login(database.db(), "21029043_captured").await;
    let (_dir, root) = temp_root();
    let dir = tempfile::tempdir().unwrap();
    let frames = write_frames(dir.path(), &[[255, 0, 0], [0, 255, 0]]);
    let zip_path = dir.path().join("frames.zip");
    write_zip(
        &zip_path,
        &[
            ("000000.png", std::fs::read(&frames[0]).unwrap()),
            ("000001.png", std::fs::read(&frames[1]).unwrap()),
        ],
    );
    let zip_bytes = std::fs::read(&zip_path).unwrap();
    let server = FakeServer::start(move |request| {
        // Media URLs in the payloads point back at this server.
        let origin = format!("http://{}", request.header("host").unwrap_or_default());
        match request.path.as_str() {
            "/ajax/user/21029043/illusts/bookmarks" if request.param("offset") == Some("0") => Answer::json(
                200,
                &format!(
                    r#"{{"error": false, "body": {{"total": 3, "works": [{}, {}, {}]}}}}"#,
                    bookmark_entry("10"),
                    bookmark_entry("11").replace(r#""illustType": 0"#, r#""illustType": 2"#),
                    bookmark_entry("12"),
                ),
            ),
            "/ajax/user/21029043/illusts/bookmarks" => Answer::json(200, r#"{"error": false, "body": {"total": 3, "works": []}}"#),
            "/ajax/illust/10/pages" => Answer::json(
                200,
                &format!(
                    r#"{{"error": false, "body": [{{"urls": {{"original": "{origin}/img/10_p0.png"}}}}, {{"urls": {{"original": "{origin}/img/10_p1.jpg"}}}}]}}"#
                ),
            ),
            "/ajax/illust/11/ugoira_meta" => Answer::json(
                200,
                &format!(
                    r#"{{"error": false, "body": {{"originalSrc": "{origin}/zip/11.zip", "frames": [{{"file": "000000.png", "delay": 40}}, {{"file": "000001.png", "delay": 60}}]}}}}"#
                ),
            ),
            "/ajax/illust/12/pages" => Answer::json(404, r#"{"error": true, "message": "gone", "body": []}"#),
            "/zip/11.zip" => Answer::bytes(200, &zip_bytes),
            path => Answer::bytes(200, path.as_bytes()),
        }
    })
    .await;
    let notifier = Arc::new(RecordingNotifier::default());
    let mut source = Pixiv::new(database.db().clone(), Arc::clone(&notifier) as SharedNotifier, &config(&root, ""))
        .unwrap()
        .with_ajax_base(&server.base_url, no_waiting());

    source.update().await.unwrap();

    let author = pypath::join(&root, "the artist");
    assert_eq!(
        std::fs::read(pypath::join(&author, "[the artist]a title [10_p0].png")).unwrap(),
        b"/img/10_p0.png"
    );
    assert!(Path::new(&pypath::join(&author, "[the artist]a title [10_p1].jpg")).exists());
    let webp = PathBuf::from(pypath::join(&author, "[the artist]a title [11_ugoira].webp"));
    assert_eq!(frame_end_times(&webp), [40, 100]);
    let rows = sqlx::query("SELECT illust_id, num, downloaded, unavailable, last_error FROM pixiv ORDER BY illust_id, num;")
        .fetch_all(database.db().pool())
        .await
        .unwrap();
    let rows: Vec<(i64, i32, i32, i32, String)> = rows
        .iter()
        .map(|row| {
            (
                row.get("illust_id"),
                row.get("num"),
                row.get("downloaded"),
                row.get("unavailable"),
                row.get("last_error"),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (10, 0, 1, 0, String::new()),
            (10, 1, 1, 0, String::new()),
            (11, 0, 1, 0, String::new()),
            (12, 0, 0, 1, "work endpoint error: gone".to_owned()),
        ]
    );
    let sent = notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].body, "Downloaded 3 files from 3 bookmarked works.");
    database.drop_database().await;
}

// ---------- registration ----------

// py: tests/test_pixiv.py::test_the_job_is_registered_and_schedulable_without_any_credentials
#[tokio::test]
async fn the_job_is_registered_and_schedulable_without_any_credentials() {
    let mut settings = Settings::default();
    settings.web.pixiv.schedule.enabled = true;

    let jobs = fav_runtime::jobs::build_jobs(&settings);
    let job = jobs.iter().find(|job| job.key == "pixiv").expect("a pixiv job");

    assert_eq!(job.name, "Pixiv");
    assert_eq!(job.section, "web.pixiv");
    assert!(job.required_commands.is_empty());
    let db = unused_db();
    let ctx = crate::SourceContext {
        settings: Arc::new(fav_db::SettingsStore::new(db.clone())),
        db,
    };
    assert!(crate::registry(&ctx).contains("pixiv"));
    // The login lives in the database, so nothing in the settings holds it back.
    assert!(job.enabled);
    assert!(job.missing_fields.is_empty());
}

// py: tests/test_pixiv.py::test_the_settings_section_is_registered
#[test]
fn the_settings_section_is_registered() {
    let section = SectionValue::validate("web.pixiv", &Value::Dict(Dict::new())).unwrap();
    assert_eq!(section.class_name(), "Pixiv");
}

// ---------- session verification ----------

// py: tests/test_pixiv.py::test_a_login_is_proved_by_reading_the_private_bookmarks
#[tokio::test]
async fn a_login_is_proved_by_reading_the_private_bookmarks() {
    let server = FakeServer::start(|_| Answer::json(200, r#"{"error": false, "message": "", "body": {"works": [], "total": 0}}"#)).await;

    let check = verify_session_at(&server.base_url, "21029043_abc", "", "").await;

    assert!(check.ok);
    assert_eq!(check.account, "21029043");
    let request = &server.received()[0];
    assert_eq!(request.path, "/ajax/user/21029043/illusts/bookmarks");
    assert_eq!(request.param("rest"), Some("hide"));
    assert_eq!(request.header("cookie"), Some("PHPSESSID=21029043_abc"));
}

// py: tests/test_pixiv.py::test_a_rejected_session_does_not_verify
#[tokio::test]
async fn a_rejected_session_does_not_verify() {
    let server = FakeServer::start(|_| Answer::json(403, r#"{"error": true, "message": "forbidden", "body": []}"#)).await;

    let check = verify_session_at(&server.base_url, "21029043_abc", "", "").await;

    assert!(!check.ok);
    assert!(check.message.contains("HTTP 403"));
}

// py: tests/test_pixiv.py::test_an_error_envelope_with_status_200_does_not_verify
#[tokio::test]
async fn an_error_envelope_with_status_200_does_not_verify() {
    let server = FakeServer::start(|_| Answer::json(200, r#"{"error": true, "message": "nope", "body": []}"#)).await;

    let check = verify_session_at(&server.base_url, "21029043_abc", "", "").await;

    assert!(!check.ok);
}

// py: tests/test_pixiv.py::test_a_pre_login_cookie_is_rejected_without_asking_pixiv
#[tokio::test]
async fn a_pre_login_cookie_is_rejected_without_asking_pixiv() {
    let server = FakeServer::start(|_| Answer::json(200, "{}")).await;

    let check = verify_session_at(&server.base_url, "anonymous-visitor-cookie", "", "").await;

    assert!(!check.ok);
    assert_eq!(check.account, "");
    assert!(server.received().is_empty());
}

// ---------- goldens from the Python module ----------

use fav_core::golden;

fn outcome<T>(result: Result<T, Failure>, to_value: impl Fn(T) -> Value) -> Value {
    let mut dict = Dict::new();
    match result {
        Ok(value) => dict.insert("result", to_value(value)),
        Err(error) => dict.insert(
            "error",
            Value::List(vec![Value::from(error.class_name()), Value::from(error.message())]),
        ),
    }
    Value::Dict(dict)
}

/// The case without its input, which is what the outcome is compared with.
fn expected_outcome(case: &Value, input: &str) -> (Value, Value) {
    let mut dict = case.as_dict().expect("a case").clone();
    let input = dict.remove(input).expect("the input");
    (input, Value::Dict(dict))
}

fn work_fields(work: &PixivWork) -> Value {
    Value::List(vec![
        Value::from(work.illust_id),
        Value::from(work.title.as_str()),
        Value::from(work.author.as_str()),
        Value::from(work.author_id.as_str()),
        Value::from(work.illust_type),
        Value::from(work.page_count),
        Value::Bool(work.masked),
    ])
}

fn str_list(items: &[String]) -> Value {
    Value::List(items.iter().map(|item| Value::from(item.as_str())).collect())
}

#[test]
fn bookmark_pages_read_like_python() {
    let document = golden::load("pixiv/bookmarks.json");
    for case in golden::cases(&document) {
        let (body, expected) = expected_outcome(case, "body");
        let actual = outcome(parse_bookmark_works(&body), |works| {
            Value::List(works.iter().map(work_fields).collect())
        });
        assert_eq!(actual, expected, "{body:?}");
    }
}

#[test]
fn page_urls_read_like_python() {
    let document = golden::load("pixiv/page_urls.json");
    for case in golden::cases(&document) {
        let (body, expected) = expected_outcome(case, "body");
        assert_eq!(outcome(Ok(parse_page_urls(&body)), |urls| str_list(&urls)), expected, "{body:?}");
    }
}

#[test]
fn ugoira_meta_reads_like_python() {
    let document = golden::load("pixiv/ugoira_meta.json");
    for case in golden::cases(&document) {
        let (body, expected) = expected_outcome(case, "body");
        let actual = outcome(parse_ugoira_meta(&body), |(zip_url, frames)| {
            Value::List(vec![
                Value::from(zip_url),
                Value::List(
                    frames
                        .into_iter()
                        .map(|(file, delay)| Value::List(vec![Value::from(file), Value::from(delay)]))
                        .collect(),
                ),
            ])
        });
        assert_eq!(actual, expected, "{body:?}");
    }
}

#[test]
fn extensions_read_like_python() {
    let document = golden::load("pixiv/extensions.json");
    for case in golden::cases(&document) {
        let (url, expected) = expected_outcome(case, "url");
        let url = golden::string(&url);
        assert_eq!(outcome(infer_extension(&url), Value::from), expected, "{url}");
    }
}

#[test]
fn user_ids_read_like_python() {
    let document = golden::load("pixiv/user_ids.json");
    for case in golden::cases(&document) {
        let pair = golden::list(case);
        let cookie = golden::string(&pair[0]);
        assert_eq!(derive_user_id(&cookie), golden::string(&pair[1]), "{cookie:?}");
    }
}

#[tokio::test]
async fn output_paths_match_python() {
    let source = pixiv(unused_db(), AJAX_BASE, "/data/pixiv", "");
    let document = golden::load("pixiv/output_paths.json");
    for case in golden::cases(&document) {
        let fields = golden::list(golden::get(case, "work"));
        let work = PixivWork {
            illust_id: i64::try_from(golden::int(&fields[0])).unwrap(),
            title: golden::string(&fields[1]),
            author: golden::string(&fields[2]),
            author_id: golden::string(&fields[3]),
            illust_type: i64::try_from(golden::int(&fields[4])).unwrap(),
            page_count: i64::try_from(golden::int(&fields[5])).unwrap(),
            masked: false,
        };
        let num = i64::try_from(golden::int(golden::get(case, "num"))).unwrap();
        let ext = golden::string(golden::get(case, "ext"));
        assert_eq!(
            source.build_output_path(&work, num, &ext),
            golden::string(golden::get(case, "path")),
            "{work:?}"
        );
    }
}
