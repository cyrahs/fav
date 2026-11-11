use std::sync::Arc;

use fav_core::settings::{Defaults as _, Settings};
use fav_db::{Db, SettingsStore};
use fav_net::HttpClient;
use sqlx::Row as _;

use super::{
    Hanime1AuthorService, Hanime1AuthorVideoEntry, extract_author_display_name, extract_author_video_entries, extract_hanime1_author_id,
};
use crate::hanime1_series::{ServiceError, client_options};
use crate::testkit::{Answer, FakeServer};
use crate::testutil;

const AUTHOR_PAGE_HTML: &str = r#"
<html>
  <head><title>Maplestar - 影片 - H動漫/裏番/線上看 - Hanime1.me</title></head>
  <body>
    <h1 class="profile-display-name">Maplestar</h1>
    <a href="https://hanime1.me/watch?v=407657" class="video-link">
      <div class="thumb-container">
        <img class="main-thumb" src="https://cdn.example.com/407657l.jpg">
        <div class="duration">03:14</div>
      </div>
      <div class="title">SAKAMOTO DAYS FULL ANIMATION</div>
    </a>
    <a class="video-link" href="/watch?v=406852">
      <div class="title">Reze (Chainsaw Man) - Animation</div>
    </a>
  </body>
</html>
"#;

fn entry(video_id: &str, title: Option<&str>) -> Hanime1AuthorVideoEntry {
    Hanime1AuthorVideoEntry {
        video_id: video_id.to_owned(),
        title: title.map(str::to_owned),
    }
}

// py: tests/test_hanime1_author.py::test_extract_author_display_name_prefers_h1
#[test]
fn extract_author_display_name_prefers_h1() {
    assert_eq!(extract_author_display_name(AUTHOR_PAGE_HTML).as_deref(), Some("Maplestar"));
}

// py: tests/test_hanime1_author.py::test_extract_author_display_name_falls_back_to_page_title
#[test]
fn extract_author_display_name_falls_back_to_page_title() {
    let page = "<html><head><title>Maplestar - 影片 - H動漫/裏番/線上看 - Hanime1.me</title></head><body></body></html>";
    assert_eq!(extract_author_display_name(page).as_deref(), Some("Maplestar"));
}

// py: tests/test_hanime1_author.py::test_extract_author_display_name_returns_none_when_missing
#[test]
fn extract_author_display_name_returns_none_when_missing() {
    assert_eq!(
        extract_author_display_name("<html><head><title>搜尋 - Hanime1.me</title></head></html>"),
        None
    );
    assert_eq!(extract_author_display_name(""), None);
}

// py: tests/test_hanime1_author.py::test_extract_author_video_entries_parses_cards
#[test]
fn extract_author_video_entries_parses_cards() {
    assert_eq!(
        extract_author_video_entries(AUTHOR_PAGE_HTML).unwrap(),
        [
            entry("407657", Some("SAKAMOTO DAYS FULL ANIMATION")),
            entry("406852", Some("Reze (Chainsaw Man) - Animation")),
        ]
    );
}

// py: tests/test_hanime1_author.py::test_extract_author_video_entries_handles_attribute_order_and_missing_title
#[test]
fn extract_author_video_entries_handles_attribute_order_and_missing_title() {
    let page = r#"
    <a data-x="1" class="card video-link extra" href="https://hanime1.me/watch?v=100">
      <div class="thumb-container"></div>
    </a>
    <a href="/watch?v=100" class="video-link"><div class="title">dup</div></a>
    <a href="/watch?v=abc" class="video-link"><div class="title">bad id</div></a>
    <a href="/playlist?list=1" class="video-link"><div class="title">not a watch link</div></a>
    "#;
    assert_eq!(extract_author_video_entries(page).unwrap(), [entry("100", None)]);
}

// py: tests/test_hanime1_author.py::test_extract_author_video_entries_ignores_plain_links
#[test]
fn extract_author_video_entries_ignores_plain_links() {
    assert!(
        extract_author_video_entries(r#"<a href="/watch?v=407657">no video-link class</a>"#)
            .unwrap()
            .is_empty()
    );
}

// py: tests/test_hanime1_author.py::test_extract_hanime1_author_id
#[test]
fn extract_hanime1_author_id_cases() {
    for (raw, expected) in [
        ("202534", Some("202534")),
        (" 202534 ", Some("202534")),
        ("https://hanime1.me/user/202534/uploaded", Some("202534")),
        ("https://hanime1.me/user/202534/uploaded?page=2", Some("202534")),
        ("https://hanime1.me/user/202534", Some("202534")),
        ("/user/202534/uploaded", Some("202534")),
        ("0", None),
        ("-5", None),
        ("https://hanime1.me/watch?v=202534", None),
        ("garbage", None),
        ("", None),
    ] {
        assert_eq!(extract_hanime1_author_id(raw).unwrap().as_deref(), expected, "{raw:?}");
    }
}

/// The service against a fake site; the settings point `web.hanime1.host` at it.
fn service(db: Db, site: &FakeServer) -> Hanime1AuthorService {
    let mut settings = Settings::defaults();
    settings.web.hanime1.host.clone_from(&site.base_url);
    let http = HttpClient::new(&client_options(true)).expect("client");
    Hanime1AuthorService::with_client(db, Arc::new(SettingsStore::pinned(settings)), http)
}

fn unreachable_db() -> Db {
    Db::connect_lazy("postgresql://nobody@127.0.0.1:1/none").expect("lazy pool")
}

async fn authors(db: &Db) -> Vec<(String, String)> {
    sqlx::query("SELECT author_id, name FROM hanime1_author ORDER BY author_id")
        .fetch_all(db.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get("author_id"), row.get("name")))
        .collect()
}

// py: tests/test_hanime1_author.py::test_author_service_adds_author_from_profile_page
#[tokio::test]
async fn author_service_adds_author_from_profile_page() {
    let database = testutil::database().await;
    let site = FakeServer::start(|_| Answer::html(AUTHOR_PAGE_HTML)).await;
    let service = service(database.db().clone(), &site);

    let created = service.add_author("https://hanime1.me/user/202534/uploaded").await.unwrap();

    assert_eq!(created.author_id, "202534");
    assert_eq!(created.name, "Maplestar");
    assert_eq!(created.author_url, site.url("/user/202534/uploaded"));
    assert_eq!(authors(database.db()).await, [("202534".to_owned(), "Maplestar".to_owned())]);
    let received = site.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].path, "/user/202534/uploaded");
    assert_eq!(received[0].header("cookie"), Some("user_lang=zhs"));
    assert_eq!(received[0].header("referer"), Some(format!("{}/", site.base_url).as_str()));
    // A second add of the same author is a duplicate.
    assert_eq!(service.add_author("202534").await.unwrap_err(), ServiceError::Duplicate);
    database.drop_database().await;
}

// py: tests/test_hanime1_author.py::test_author_service_rejects_invalid_input
#[tokio::test]
async fn author_service_rejects_invalid_input() {
    let site = FakeServer::start(|_| panic!("no request expected")).await;
    let service = service(unreachable_db(), &site);
    assert_eq!(service.add_author("not-an-author").await.unwrap_err(), ServiceError::Invalid);
    assert!(site.received().is_empty());
}

async fn assert_unresolvable(answer: Answer) {
    let database = testutil::database().await;
    let site = FakeServer::start(move |_| answer.clone()).await;
    let service = service(database.db().clone(), &site);
    assert_eq!(service.add_author("202534").await.unwrap_err(), ServiceError::ResolveFailed);
    assert!(authors(database.db()).await.is_empty());
    database.drop_database().await;
}

// py: tests/test_hanime1_author.py::test_author_service_raises_lookup_error_when_name_unresolvable
#[tokio::test]
async fn author_service_raises_lookup_error_when_name_unresolvable() {
    assert_unresolvable(Answer::html("<html><head><title>搜尋 - Hanime1.me</title></head></html>")).await;
}

// py: tests/test_hanime1_author.py::test_author_service_raises_lookup_error_on_http_error
#[tokio::test]
async fn author_service_raises_lookup_error_on_http_error() {
    assert_unresolvable(Answer::bytes(404, b"not found")).await;
}

// py: tests/test_hanime1_author.py::test_author_service_raises_lookup_error_when_blocked_by_cloudflare
#[tokio::test]
async fn author_service_raises_lookup_error_when_blocked_by_cloudflare() {
    assert_unresolvable(Answer::html(
        r#"<html><title>Attention Required! | Cloudflare</title><div id="cf-error-details"></div></html>"#,
    ))
    .await;
}

// py: tests/test_hanime1_author.py::test_author_service_converts_display_name_to_simplified
#[tokio::test]
async fn author_service_converts_display_name_to_simplified() {
    let database = testutil::database().await;
    let site = FakeServer::start(|_| Answer::html(r#"<h1 class="profile-display-name">動畫社團</h1>"#)).await;
    let service = service(database.db().clone(), &site);

    let created = service.add_author("202534").await.unwrap();

    assert_eq!(created.name, "动画社团");
    assert_eq!(authors(database.db()).await, [("202534".to_owned(), "动画社团".to_owned())]);
    database.drop_database().await;
}

// py: tests/test_hanime1_author.py::test_author_service_delete_rejects_invalid_id
#[tokio::test]
async fn author_service_delete_rejects_invalid_id() {
    let site = FakeServer::start(|_| panic!("no request expected")).await;
    let service = service(unreachable_db(), &site);
    assert_eq!(service.delete_author("abc").await.unwrap_err(), ServiceError::Invalid);
    assert_eq!(service.delete_author("0").await.unwrap_err(), ServiceError::Invalid);
}

#[tokio::test]
async fn author_service_lists_and_deletes() {
    let database = testutil::database().await;
    let site = FakeServer::start(|_| Answer::html(AUTHOR_PAGE_HTML)).await;
    let service = service(database.db().clone(), &site);
    service.add_author("202534").await.unwrap();

    let listed = service.list_authors().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].author_id, "202534");
    assert_eq!(listed[0].video_count, 0);
    assert_eq!(listed[0].last_scanned_at, None);
    assert_eq!(listed[0].author_url, site.url("/user/202534/uploaded"));

    assert!(service.delete_author(" 0202534 ").await.unwrap());
    assert!(!service.delete_author("202534").await.unwrap());
    database.drop_database().await;
}
