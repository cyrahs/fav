use std::sync::Arc;
use std::time::Duration;

use fav_core::settings::{Defaults as _, Settings};
use fav_db::{Db, SettingsStore};
use fav_net::HttpClient;
use sqlx::Row as _;

use super::{Hanime1SeriesService, ProbeError, ServiceError, client_options};
use crate::runtime_config::RuntimeSeriesSeed;
use crate::testkit::{Answer, FakeServer};
use crate::testutil;

fn service(db: Db, site: &FakeServer) -> Hanime1SeriesService {
    let mut settings = Settings::defaults();
    settings.web.hanime1.host.clone_from(&site.base_url);
    let http = HttpClient::new(&client_options(false)).expect("client");
    Hanime1SeriesService::with_client(db, Arc::new(SettingsStore::pinned(settings)), http)
}

async fn series_rows(db: &Db) -> Vec<(String, String, String)> {
    sqlx::query("SELECT canonical_video_id, title, added_from_video_id FROM hanime1_series ORDER BY canonical_video_id")
        .fetch_all(db.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect()
}

async fn member_rows(db: &Db) -> Vec<(String, String)> {
    sqlx::query("SELECT video_id, canonical_video_id FROM hanime1_series_video ORDER BY video_id")
        .fetch_all(db.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

const CLASS_WRAPPER_HTML: &str = r#"
<div class="video-playlist-wrapper">
  <h4><span>清單</span><a href="/playlist?list=1">屈辱</a></h4>
  <a href="/watch?v=13253"></a>
  <a href="/watch?v=12488"></a>
</div>
"#;

// py: tests/test_hanime1.py::test_series_service_adds_seed_from_current_class_wrapper
#[tokio::test]
async fn series_service_adds_seed_from_current_class_wrapper() {
    let database = testutil::database().await;
    let site = FakeServer::start(|_| Answer::html(CLASS_WRAPPER_HTML)).await;
    let service = service(database.db().clone(), &site);

    let seed = service.add_seed("12488").await.unwrap();

    assert_eq!(
        seed,
        RuntimeSeriesSeed {
            video_id: "12488".to_owned(),
            title: "屈辱".to_owned()
        }
    );
    assert_eq!(series_rows(database.db()).await, [("12488".into(), "屈辱".into(), "12488".into())]);
    assert_eq!(
        member_rows(database.db()).await,
        [("12488".into(), "12488".into()), ("13253".into(), "12488".into())]
    );
    let received = site.received();
    assert_eq!(received[0].path, "/watch");
    assert_eq!(received[0].param("v"), Some("12488"));
    assert_eq!(received[0].header("cookie"), Some("user_lang=zhs"));

    // Any member already claimed makes the seed a duplicate, whichever id was pasted.
    assert_eq!(service.add_seed("屈辱 {id-13253}").await.unwrap_err(), ServiceError::Duplicate);
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_series_service_rejects_current_cards_without_titles
#[tokio::test]
async fn series_service_rejects_current_cards_without_titles() {
    for class_name in ["playlist-video-card", "video-item-container"] {
        let page = format!(
            r#"
            <div class="video-playlist-wrapper">
              <h4><a href="/playlist?list=1">Series</a></h4>
              <div class="{class_name}">
                <a href="/watch?v=12488"></a>
                <div class="video-info-container"><h4 class="renamed-title">Episode 1</h4></div>
              </div>
            </div>
            "#
        );
        let database = testutil::database().await;
        let site = FakeServer::start(move |_| Answer::html(&page)).await;
        let service = service(database.db().clone(), &site);

        assert_eq!(
            service.probe("12488", Duration::from_secs(10)).await,
            Err(ProbeError::ParserIncompatible)
        );
        assert_eq!(
            service.add_seed("12488").await.unwrap_err(),
            ServiceError::ResolveFailed,
            "{class_name}"
        );
        assert!(series_rows(database.db()).await.is_empty());
        database.drop_database().await;
    }
}

// py: tests/test_hanime1.py::test_series_service_rejects_playlist_without_video_id
#[tokio::test]
async fn series_service_rejects_playlist_without_video_id() {
    let database = testutil::database().await;
    let site = FakeServer::start(|_| Answer::html(r#"<div class="video-playlist-wrapper"><h4>Series &amp; More</h4></div>"#)).await;
    let service = service(database.db().clone(), &site);

    assert_eq!(service.add_seed("12488").await.unwrap_err(), ServiceError::ResolveFailed);
    assert!(series_rows(database.db()).await.is_empty());
    database.drop_database().await;
}

#[tokio::test]
async fn series_service_validates_seeds_before_fetching() {
    let site = FakeServer::start(|_| panic!("no request expected")).await;
    let service = service(Db::connect_lazy("postgresql://nobody@127.0.0.1:1/none").unwrap(), &site);
    for raw in ["", "  ", "0", "屈辱", "{id-0}"] {
        assert_eq!(service.add_seed(raw).await.unwrap_err(), ServiceError::Invalid, "{raw:?}");
    }
    for raw in ["abc", "0", "-1", ""] {
        assert_eq!(service.delete_seed(raw).await.unwrap_err(), ServiceError::Invalid, "{raw:?}");
    }
    assert!(site.received().is_empty());
}

#[tokio::test]
async fn series_service_lists_deletes_and_samples_readiness_targets() {
    let database = testutil::database().await;
    let site = FakeServer::start(|_| Answer::html(CLASS_WRAPPER_HTML)).await;
    let service = service(database.db().clone(), &site);
    service.add_seed("12488").await.unwrap();
    sqlx::query(
        "INSERT INTO hanime1_series (canonical_video_id, title, added_from_video_id, last_scanned_at) VALUES ('5', '後宮', '5', now())",
    )
    .execute(database.db().pool())
    .await
    .unwrap();

    let listed = service.list_seeds().await.unwrap();
    assert_eq!(listed.len(), 2);
    let first = &listed[0];
    assert_eq!(first.video_id, "12488");
    assert_eq!(first.label, "屈辱 {id-12488}");
    assert_eq!(first.video_count, 2);
    assert_eq!(first.added_from_video_id, "12488");
    assert_eq!(first.last_scan_error, "");
    assert_eq!(first.watch_url, site.url("/watch?v=12488"));
    assert_eq!(listed[1].label, "后宫 {id-5}");

    // Scanned series first.
    assert_eq!(service.load_readiness_seed_ids(3).await.unwrap(), ["5", "12488"]);
    assert_eq!(service.load_readiness_seed_ids(1).await.unwrap(), ["5"]);

    assert!(service.delete_seed("012488").await.unwrap());
    assert!(member_rows(database.db()).await.is_empty(), "members cascade");
    assert!(!service.delete_seed("12488").await.unwrap());
    database.drop_database().await;
}

#[tokio::test]
async fn probe_reports_blocked_pages_and_status_errors() {
    let blocked = FakeServer::start(|_| {
        Answer::html(r#"<html><title>Attention Required! | Cloudflare</title><div id="cf-error-details"></div></html>"#)
    })
    .await;
    let db = Db::connect_lazy("postgresql://nobody@127.0.0.1:1/none").unwrap();
    assert_eq!(
        service(db.clone(), &blocked).probe("1", Duration::from_secs(10)).await,
        Err(ProbeError::Other(
            "RuntimeError: Hanime1 watch page was blocked by Cloudflare".to_owned()
        ))
    );
    let missing = FakeServer::start(|_| Answer::bytes(503, b"down")).await;
    let Err(ProbeError::Other(message)) = service(db.clone(), &missing).probe("1", Duration::from_secs(10)).await else {
        panic!("expected a status error");
    };
    assert!(
        message.starts_with("HTTPStatusError: Server error '503 Service Unavailable'"),
        "{message}"
    );
    let healthy = FakeServer::start(|_| Answer::html(CLASS_WRAPPER_HTML)).await;
    assert_eq!(service(db, &healthy).probe("12488", Duration::from_secs(10)).await, Ok(()));
}

#[tokio::test]
async fn readiness_targets_fail_at_once_when_the_database_refuses_connections() {
    let site = FakeServer::start(|_| panic!("no request expected")).await;
    let service = service(Db::connect_lazy("postgresql://nobody@127.0.0.1:1/none").unwrap(), &site);
    let loaded = tokio::time::timeout(Duration::from_secs(5), service.load_readiness_seed_ids(3)).await;
    assert!(matches!(loaded, Ok(Err(_))), "{loaded:?}");
}

#[tokio::test]
async fn series_service_rejects_a_page_with_a_malformed_link_as_invalid() {
    // `urlsplit` raises `ValueError` on the link, which the API answers with 422 `invalid_seed`.
    let page = r#"<div class="video-playlist-wrapper"><h4>T</h4><a href="//[x/watch?v=1">x</a></div>"#;
    let site = FakeServer::start(move |_| Answer::html(page)).await;
    let service = service(Db::connect_lazy("postgresql://nobody@127.0.0.1:1/none").unwrap(), &site);
    assert_eq!(service.add_seed("12488").await.unwrap_err(), ServiceError::Invalid);
}
