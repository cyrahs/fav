#![allow(clippy::format_collect, clippy::format_push_string)] // Fixture pages are built by formatting; allocations do not matter here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fav_core::settings::{Defaults as _, RankingPeriod, Settings};
use fav_core::validate::PyPath;
use fav_db::testdb::TestDatabase;
use fav_db::{Db, SettingsStore};
use fav_net::HttpClient;
use fav_net::retry::RecordingSleeper;
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox};
use sqlx::Row as _;

use super::{
    CommandOutput, CommandRunner, HanimeRecord, ItemError, Run, WatchSeriesVideo, build_download_command, build_ranking_page_url,
    client_options, extract_download_urls, extract_genre_from_watch_html, extract_search_video_ids, extract_stream_urls,
    extract_watch_metadata, extract_watch_series, format_file_size, format_playlist_archive_title, ignored_title_marker, parse_title,
    parser_failure_dedupe_key, playlist_item_titles_are_distinct, stream_resolution_label, strip_uploader_prefix,
};
use crate::kit::Notifier;
use crate::runtime_config::{RuntimeSeriesSeed, parse_hanime1_playlist, parse_runtime_series_seed};
use crate::testkit::{Answer, FakeServer, Received, RecordingNotifier};
use crate::testutil;

const HOST: &str = "https://hanime1.me";

const CURRENT_PLAYLIST_HTML: &str = r#"
<div class="desktop video-playlist-wrapper">
  <h4><span>清單</span><a href="/playlist?list=1">○○交配</a></h4>
  <div class="video-item-container">
    <div class="thumb-container"><a href="/watch?v=407017"><span>Preview 407017</span></a></div>
    <div class="video-info-container"></div>
  </div>
</div>
<div class="video-playlist-wrapper mobile">
  <h4><span>清單</span><a href="/playlist?list=1">○○交配</a></h4>
  <div class="playlist-video-card video-item-container">
    <div class="thumb-container"><a href="/watch?v=407017"></a></div>
    <div class="video-info-container"><h4 class="video-title">援助交配 11</h4></div>
  </div>
  <div class="playlist-video-card video-item-container">
    <div class="thumb-container"><a href="/watch?v=37176"></a></div>
    <div class="video-info-container"><h4 class="video-title">援助交配 1</h4></div>
  </div>
</div>
"#;

const LINK_TITLE_PLAYLIST_HTML: &str = r#"
<div class="video-playlist-wrapper">
  <h4><span>清單</span><a href="/playlist?list=2">ヌきヌき ずっぽしイズム</a></h4>
  <div class="video-item-container">
    <a href="/watch?v=407018"><span>射精深陷主義 1</span></a>
  </div>
</div>
"#;

const CF_BLOCKED: &str = r#"<html><title>Attention Required! | Cloudflare</title><div id="cf-error-details"></div></html>"#;

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn video(video_id: &str, title: Option<&str>, position: i64) -> WatchSeriesVideo {
    WatchSeriesVideo {
        video_id: video_id.to_owned(),
        title: title.map(str::to_owned),
        position,
    }
}

fn seed(video_id: &str, title: &str) -> RuntimeSeriesSeed {
    RuntimeSeriesSeed {
        video_id: video_id.to_owned(),
        title: title.to_owned(),
    }
}

// ---------- fakes ----------

/// The pages the fake site serves, keyed by `path?query` (decoded, in request order).
#[derive(Clone, Default)]
struct Pages(Arc<Mutex<HashMap<String, Answer>>>);

impl Pages {
    fn html(&self, key: &str, html: &str) {
        self.0.lock().unwrap().insert(key.to_owned(), Answer::html(html));
    }

    fn status(&self, key: &str, status: u16) {
        self.0.lock().unwrap().insert(key.to_owned(), Answer::bytes(status, b"error"));
    }

    fn answer(&self, received: &Received) -> Answer {
        self.0
            .lock()
            .unwrap()
            .get(&key(received))
            .cloned()
            .unwrap_or_else(|| Answer::bytes(404, b"not found"))
    }
}

fn key(received: &Received) -> String {
    if received.query.is_empty() {
        return received.path.clone();
    }
    let query: Vec<String> = received.query.iter().map(|(name, value)| format!("{name}={value}")).collect();
    format!("{}?{}", received.path, query.join("&"))
}

/// A playlist watch page: `<h4>` title and one plain watch link per id.
fn playlist_page(name: &str, ids: &[&str]) -> String {
    let links: String = ids.iter().map(|id| format!(r#"<a href="/watch?v={id}"></a>"#)).collect();
    format!(r#"<div class="video-playlist-wrapper"><h4>{name}</h4>{links}</div>"#)
}

/// A playlist whose cards carry their own titles.
fn titled_playlist_page(name: &str, videos: &[(&str, &str)]) -> String {
    let cards: String = videos
        .iter()
        .map(|(id, title)| {
            format!(
                r#"<div class="playlist-video-card"><a href="/watch?v={id}"></a><div class="video-info-container"><h4 class="video-title">{title}</h4></div></div>"#
            )
        })
        .collect();
    format!(r#"<div class="video-playlist-wrapper"><h4>{name}</h4>{cards}</div>"#)
}

/// A watch page with the metadata fields `extract_watch_metadata` reads.
fn watch_page(title: &str, uploader: Option<&str>, genre: Option<&str>, release_date: Option<&str>, plot: Option<&str>) -> String {
    let mut page = format!(r#"<h3 id="shareBtn-title">{title}</h3>"#);
    if let Some(release_date) = release_date {
        page.push_str(&format!("<div>觀看次數:1萬次&nbsp;&nbsp;{release_date}</div>"));
    }
    if let Some(uploader) = uploader {
        page.push_str(&format!(r#"<a id="video-artist-name" href="/search?query=x">{uploader}</a>"#));
    } else if genre.is_some() {
        page.push_str(r#"<a id="video-artist-name" href="/search?query=x"></a>"#);
    }
    if let Some(genre) = genre {
        page.push_str(&format!(r#"<a href="/search?genre={genre}">{genre}</a>"#));
    }
    if let Some(plot) = plot {
        page.push_str(&format!(r#"<div class="video-caption-text">{plot}</div>"#));
    }
    page
}

/// `yt-dlp` stand-in: records each argv and writes `<id>.mp4` where `-o` points, unless
/// a scripted failure is next.
#[derive(Default)]
struct FakeRunner {
    calls: Mutex<Vec<Vec<String>>>,
    failures: Mutex<Vec<CommandOutput>>,
}

impl FakeRunner {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    fn fail_next(&self, output: CommandOutput) {
        self.failures.lock().unwrap().push(output);
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, argv: Vec<String>) -> BoxFuture<'_, std::io::Result<CommandOutput>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(argv.clone());
            let failure = {
                let mut failures = self.failures.lock().unwrap();
                (!failures.is_empty()).then(|| failures.remove(0))
            };
            if let Some(failure) = failure {
                return Ok(failure);
            }
            let template = &argv[argv.iter().position(|arg| arg == "-o").unwrap() + 1];
            std::fs::write(template.replace("%(ext)s", "mp4"), b"video")?;
            Ok(CommandOutput {
                code: Some(0),
                ..CommandOutput::default()
            })
        })
    }
}

struct Harness {
    run: Run,
    site: FakeServer,
    pages: Pages,
    notifier: Arc<RecordingNotifier>,
    runner: Arc<FakeRunner>,
    sleeps: RecordingSleeper,
    root: PathBuf,
    _dirs: tempfile::TempDir,
}

impl Harness {
    fn url(&self, path: &str) -> String {
        self.site.url(path)
    }

    fn requests(&self) -> Vec<String> {
        self.site.received().iter().map(key).collect()
    }
}

async fn harness(db: Db, configure: impl FnOnce(&mut fav_core::settings::Hanime1)) -> Harness {
    let pages = Pages::default();
    let site = {
        let pages = pages.clone();
        FakeServer::start(move |received| pages.answer(received)).await
    };
    let dirs = tempfile::tempdir().unwrap();
    let root = dirs.path().join("hanime1");
    let cache = dirs.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let mut settings = Settings::defaults();
    let cfg = &mut settings.web.hanime1;
    cfg.host.clone_from(&site.base_url);
    cfg.path = PyPath::new(root.to_str().unwrap());
    configure(cfg);
    let cfg = settings.web.hanime1.clone();
    let notifier = Arc::new(RecordingNotifier::default());
    let runner = Arc::new(FakeRunner::default());
    let sleeps = RecordingSleeper::new();
    let run = Run::new(
        cfg,
        HttpClient::new(&client_options()).unwrap(),
        db.clone(),
        Outbox::new(db, Arc::new(SettingsStore::pinned(settings))),
        Arc::clone(&notifier) as Arc<dyn Notifier>,
        Arc::clone(&runner) as Arc<dyn CommandRunner>,
        sleeps.sleeper(),
        cache,
    );
    Harness {
        run,
        site,
        pages,
        notifier,
        runner,
        sleeps,
        root,
        _dirs: dirs,
    }
}

fn unreachable_db() -> Db {
    Db::connect_lazy("postgresql://nobody@127.0.0.1:1/none").expect("lazy pool")
}

async fn insert_series(database: &TestDatabase, canonical_video_id: &str, title: &str, members: &[&str]) {
    sqlx::query("INSERT INTO hanime1_series (canonical_video_id, title, added_from_video_id) VALUES ($1, $2, $1)")
        .bind(canonical_video_id)
        .bind(title)
        .execute(database.db().pool())
        .await
        .unwrap();
    for member in members {
        sqlx::query("INSERT INTO hanime1_series_video (video_id, canonical_video_id) VALUES ($1, $2)")
            .bind(member)
            .bind(canonical_video_id)
            .execute(database.db().pool())
            .await
            .unwrap();
    }
}

async fn insert_downloaded(database: &TestDatabase, id: &str) {
    sqlx::query("INSERT INTO hanime1 (id, title) VALUES ($1, 'x')")
        .bind(id)
        .execute(database.db().pool())
        .await
        .unwrap();
}

async fn insert_author(database: &TestDatabase, author_id: &str, name: &str) {
    sqlx::query("INSERT INTO hanime1_author (author_id, name) VALUES ($1, $2)")
        .bind(author_id)
        .bind(name)
        .execute(database.db().pool())
        .await
        .unwrap();
}

async fn series_rows(database: &TestDatabase) -> Vec<(String, String, String)> {
    sqlx::query("SELECT canonical_video_id, title, added_from_video_id FROM hanime1_series ORDER BY canonical_video_id")
        .fetch_all(database.db().pool())
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect()
}

async fn member_rows(database: &TestDatabase) -> Vec<(String, String)> {
    sqlx::query("SELECT video_id, canonical_video_id FROM hanime1_series_video ORDER BY video_id")
        .fetch_all(database.db().pool())
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

async fn series_scan_state(database: &TestDatabase, canonical_video_id: &str) -> (bool, String) {
    let row =
        sqlx::query("SELECT last_scanned_at IS NOT NULL AS scanned, last_scan_error FROM hanime1_series WHERE canonical_video_id = $1")
            .bind(canonical_video_id)
            .fetch_one(database.db().pool())
            .await
            .unwrap();
    (row.get("scanned"), row.get("last_scan_error"))
}

async fn saved_debt(database: &TestDatabase) -> Option<f64> {
    sqlx::query("SELECT quota_debt FROM hanime1_ranking_scan_state WHERE id = 1")
        .fetch_optional(database.db().pool())
        .await
        .unwrap()
        .map(|row| row.get(0))
}

async fn hanime1_rows(database: &TestDatabase) -> Vec<(String, String, String, String, String)> {
    sqlx::query("SELECT id, title, uploader, release_date, plot FROM hanime1 ORDER BY created_at, id")
        .fetch_all(database.db().pool())
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4)))
        .collect()
}

fn candidates(collected: super::Candidates) -> Vec<(String, String, String)> {
    collected
        .items
        .into_iter()
        .map(|(_, candidate)| (candidate.video_id, candidate.source_name, candidate.archive_title))
        .collect()
}

fn triple(video_id: &str, source_name: &str, archive_title: &str) -> (String, String, String) {
    (video_id.to_owned(), source_name.to_owned(), archive_title.to_owned())
}

// ---------- parsers ----------

// py: tests/test_hanime1.py::test_extract_stream_urls_prefers_m3u8_and_unescapes
#[test]
fn extract_stream_urls_prefers_m3u8_and_unescapes() {
    let page = r#"
    <script>
      const v = "https:\/\/video.example.com\/master.m3u8?token=abc&quality=1080";
      const backup = "//cdn.example.com/path/video.mp4?x=1";
    </script>
    "#;
    assert_eq!(
        extract_stream_urls(page).unwrap(),
        strings(&[
            "https://video.example.com/master.m3u8?token=abc&quality=1080",
            "https://cdn.example.com/path/video.mp4?x=1",
        ])
    );
}

// py: tests/test_hanime1.py::test_parse_title_uses_og_title_first
#[test]
fn parse_title_uses_og_title_first() {
    let page = r#"
    <html>
      <head>
        <meta property="og:title" content="Sample &amp; Title" />
        <title>Fallback Title</title>
      </head>
    </html>
    "#;
    assert_eq!(parse_title(page).as_deref(), Some("Sample & Title"));
}

// py: tests/test_hanime1.py::test_extract_download_urls_prefers_highest_quality_and_unescapes
#[test]
fn extract_download_urls_prefers_highest_quality_and_unescapes() {
    let page = r#"
    <a data-url="https://vdownload.example.com/18580-sc-480p.mp4?token=a&amp;expires=1">low</a>
    <a data-url="https://vdownload.example.com/18580-sc-720p.mp4?token=b&amp;expires=1">high</a>
    "#;
    assert_eq!(
        extract_download_urls(page),
        strings(&[
            "https://vdownload.example.com/18580-sc-720p.mp4?token=b&expires=1",
            "https://vdownload.example.com/18580-sc-480p.mp4?token=a&expires=1",
        ])
    );
}

// py: tests/test_hanime1.py::test_extract_download_urls_supports_k_quality_labels
#[test]
fn extract_download_urls_supports_k_quality_labels() {
    let page = r#"
    <a data-url="https://vdownload.example.com/18580-sc-1080p.mp4?token=a&amp;expires=1">1080p</a>
    <a data-url="https://vdownload.example.com/18580-sc-4k.mp4?token=b&amp;expires=1">4k</a>
    <a data-url="https://vdownload.example.com/18580-sc-8K.mp4?token=c&amp;expires=1">8k</a>
    "#;
    assert_eq!(
        extract_download_urls(page),
        strings(&[
            "https://vdownload.example.com/18580-sc-8K.mp4?token=c&expires=1",
            "https://vdownload.example.com/18580-sc-4k.mp4?token=b&expires=1",
            "https://vdownload.example.com/18580-sc-1080p.mp4?token=a&expires=1",
        ])
    );
}

// py: tests/test_hanime1.py::test_extract_search_video_ids_dedupes_watch_links
#[test]
fn extract_search_video_ids_dedupes_watch_links() {
    let page = r#"
    <a href="/watch?v=18580">a</a>
    <a href="watch?v=18580">dup</a>
    <a href="//hanime1.me/watch?v=20000&amp;src=1">b</a>
    <a href="https://hanime1.me/watch?v=30000">c</a>
    <a href="/download?v=18580">ignore</a>
    "#;
    assert_eq!(extract_search_video_ids(HOST, page).unwrap(), strings(&["18580", "20000", "30000"]));
}

// py: tests/test_hanime1.py::test_extract_watch_series_reads_playlist_ids_and_title
#[test]
fn extract_watch_series_reads_playlist_ids_and_title() {
    let page = r#"
    <div id="video-playlist-wrapper">
      <h4>屈辱</h4>
      <div class="related-watch-wrap multiple-link-wrapper">
        <a class="overlay" href="https://hanime1.me/watch?v=13253"></a>
        <div class="card-mobile-title">屈辱 1</div>
      </div>
      <div class="related-watch-wrap multiple-link-wrapper">
        <a class="overlay" href="/watch?v=12488"></a>
        <img src="/thumbnail/12488.jpg" alt="屈辱 2" />
      </div>
      <a style="text-decoration: none;" href="https://hanime1.me/search?query=%E5%B1%88%E8%BE%B1">
        <div class="load-more-related-link related-watch-wrap">More studio videos</div>
      </a>
    </div>
    <div id="video-playlist-wrapper">
      <h4>屈辱</h4>
      <a class="overlay" href="https://hanime1.me/watch?v=12488"></a>
      <a class="overlay" href="https://hanime1.me/watch?v=12496"></a>
    </div>
    "#;
    let series = extract_watch_series(page).unwrap().unwrap();
    assert_eq!(series.name.as_deref(), Some("屈辱"));
    assert_eq!(series.video_ids, strings(&["13253", "12488", "12496"]));
    assert_eq!(
        series.videos,
        [
            video("13253", Some("屈辱 1"), 1),
            video("12488", Some("屈辱 2"), 2),
            video("12496", None, 3)
        ]
    );
}

// py: tests/test_hanime1.py::test_extract_watch_series_preserves_mixed_wrapped_and_plain_links
#[test]
fn extract_watch_series_preserves_mixed_wrapped_and_plain_links() {
    let page = r#"
    <div id="video-playlist-wrapper">
      <h4>混合系列</h4>
      <div class="related-watch-wrap multiple-link-wrapper">
        <a class="overlay" href="https://hanime1.me/watch?v=10001"></a>
        <div class="card-mobile-title">混合系列 1</div>
      </div>
      <a class="overlay" href="https://hanime1.me/watch?v=10002"></a>
    </div>
    "#;
    let series = extract_watch_series(page).unwrap().unwrap();
    assert_eq!(series.videos, [video("10001", Some("混合系列 1"), 1), video("10002", None, 2)]);
}

// py: tests/test_hanime1.py::test_extract_watch_series_supports_current_class_wrapper_and_dedupes_layouts
#[test]
fn extract_watch_series_supports_current_class_wrapper_and_dedupes_layouts() {
    let page = r#"
    <div class="video-playlist-wrapper mobile">
      <h4><span>清單</span><a href="/playlist?list=1">屈辱</a></h4>
      <div class="related-watch-wrap"><a href="/watch?v=13253"></a><div class="card-mobile-title">屈辱 1</div></div>
      <div class="related-watch-wrap"><a href="/watch?v=12488"></a><div class="card-mobile-title">屈辱 2</div></div>
    </div>
    <div class="desktop video-playlist-wrapper">
      <h4><span>清单</span><a href="/playlist?list=1">屈辱</a></h4>
      <a href="/watch?v=13253"></a>
      <a href="/watch?v=12488"></a>
    </div>
    "#;
    let series = extract_watch_series(page).unwrap().unwrap();
    assert_eq!(series.name.as_deref(), Some("屈辱"));
    assert_eq!(series.video_ids, strings(&["13253", "12488"]));
    assert_eq!(
        parse_hanime1_playlist(page, None, false).unwrap(),
        Some(("屈辱".to_owned(), strings(&["13253", "12488"])))
    );
}

// py: tests/test_hanime1.py::test_extract_watch_series_reads_current_playlist_card_titles_and_dedupes_layouts
#[test]
fn extract_watch_series_reads_current_playlist_card_titles_and_dedupes_layouts() {
    let series = extract_watch_series(CURRENT_PLAYLIST_HTML).unwrap().unwrap();
    assert_eq!(series.name.as_deref(), Some("○○交配"));
    assert_eq!(series.video_ids, strings(&["407017", "37176"]));
    assert_eq!(
        series.videos,
        [video("407017", Some("援助交配 11"), 1), video("37176", Some("援助交配 1"), 2)]
    );
}

// py: tests/test_hanime1.py::test_extract_watch_series_reads_title_from_watch_link
#[test]
fn extract_watch_series_reads_title_from_watch_link() {
    let series = extract_watch_series(LINK_TITLE_PLAYLIST_HTML).unwrap().unwrap();
    assert_eq!(series.name.as_deref(), Some("ヌきヌき ずっぽしイズム"));
    assert_eq!(series.videos, [video("407018", Some("射精深陷主义 1"), 1)]);
}

// py: tests/test_hanime1.py::test_playlist_parser_requires_exact_wrapper_class_and_video_id
#[test]
fn playlist_parser_requires_exact_wrapper_class_and_video_id() {
    assert_eq!(
        parse_hanime1_playlist(
            r#"<div class="not-video-playlist-wrapper-copy"><h4>Series</h4><a href="/watch?v=12488"></a></div>"#,
            None,
            false
        )
        .unwrap(),
        None
    );
    assert_eq!(
        parse_hanime1_playlist(r#"<div class="video-playlist-wrapper"><h4>Series</h4></div>"#, None, false).unwrap(),
        None
    );
}

// py: tests/test_hanime1.py::test_playlist_item_title_distinctness
#[test]
fn playlist_item_title_distinctness() {
    for (titles, distinct) in [
        (vec![Some("First"), Some("Second")], true),
        (vec![Some("First"), None], false),
        (vec![Some("Title A"), Some(" title   a ")], false),
        (vec![Some("後編"), Some("后编")], false),
    ] {
        let videos: Vec<WatchSeriesVideo> = titles
            .iter()
            .zip(1..)
            .map(|(title, position)| video(&position.to_string(), *title, position))
            .collect();
        assert_eq!(playlist_item_titles_are_distinct(&videos), distinct, "{titles:?}");
    }
}

// py: tests/test_hanime1.py::test_playlist_item_title_is_authoritative_for_407015
#[test]
fn playlist_item_title_is_authoritative_for_407015() {
    let title = format_playlist_archive_title(
        Some("ラブはギャルから始まる運命"),
        &video("407015", Some("戀愛始於辣妹的命運 2"), 1),
        2,
        true,
    );
    assert_eq!(title, "恋爱始于辣妹的命运 2");
}

// py: tests/test_hanime1.py::test_single_item_without_title_uses_number_one
#[test]
fn single_item_without_title_uses_number_one() {
    assert_eq!(
        format_playlist_archive_title(Some("Series"), &video("1", None, 1), 1, false),
        "Series 01"
    );
}

// py: tests/test_hanime1.py::test_extract_watch_series_returns_none_when_playlist_not_found
#[test]
fn extract_watch_series_returns_none_when_playlist_not_found() {
    assert_eq!(extract_watch_series("<div>no playlist</div>").unwrap(), None);
}

// py: tests/test_hanime1.py::test_extract_watch_series_ignores_load_more_search_url
#[test]
fn extract_watch_series_ignores_load_more_search_url() {
    let page = r#"
    <div id="video-playlist-wrapper">
      <h4>Studio Name</h4>
      <a style="text-decoration: none;" href="https://hanime1.me/search?query=studio">
        <div class="load-more-related-link related-watch-wrap">More studio videos</div>
      </a>
    </div>
    "#;
    assert_eq!(extract_watch_series(page).unwrap(), None);
}

// py: tests/test_hanime1.py::test_extract_watch_metadata_prefers_primary_watch_title
#[test]
fn extract_watch_metadata_prefers_primary_watch_title() {
    let primary_title = "クール de M \u{FF5E}崩れないオンナ\u{FF5E} [中文字幕]";
    let page = format!(
        r#"
    <html>
      <head>
        <title>{primary_title}&nbsp;-&nbsp;H動漫/裏番/線上看&nbsp;-&nbsp;Hanime1.me</title>
      </head>
      <body>
        <h3 id="shareBtn-title" class="video-details-wrapper">{primary_title}</h3>
        <div>觀看次數:152.2萬次&nbsp;&nbsp;2026-03-27</div>
        <a id="video-artist-name" href="/search?query=nur">nur</a>
        <div>高冷的抖M</div>
        <div class="video-caption-text caption-ellipsis">劇情介紹。</div>
      </body>
    </html>
    "#
    );
    let metadata = extract_watch_metadata(HOST, &page);
    assert_eq!(metadata.title.as_deref(), Some(primary_title));
    assert_eq!(metadata.uploader.as_deref(), Some("nur"));
    assert_eq!(metadata.release_date.as_deref(), Some("2026-03-27"));
    assert_eq!(metadata.plot.as_deref(), Some("剧情介绍。"));
}

// py: tests/test_hanime1.py::test_ignored_title_marker_requires_exact_site_markers
#[test]
fn ignored_title_marker_requires_exact_site_markers() {
    assert_eq!(ignored_title_marker(Some("OVA Demo [新番預告]")), Some("[新番预告]"));
    assert_eq!(ignored_title_marker(Some("OVA Demo [新番预告]")), Some("[新番预告]"));
    assert_eq!(ignored_title_marker(Some("OVA Demo [中字後補]")), Some("[中字后补]"));
    assert_eq!(ignored_title_marker(Some("OVA Demo [中字后补]")), Some("[中字后补]"));
    assert_eq!(ignored_title_marker(Some("OVA Demo [中文後補]")), Some("[中文后补]"));
    assert_eq!(ignored_title_marker(Some("OVA Demo [中文后补]")), Some("[中文后补]"));
    assert_eq!(ignored_title_marker(Some("OVA Demo 新番预告")), None);
    assert_eq!(ignored_title_marker(Some("OVA Demo 预告")), None);
}

// py: tests/test_hanime1.py::test_build_ranking_page_url_uses_traditional_query_values
#[test]
fn build_ranking_page_url_uses_traditional_query_values() {
    let weekly = build_ranking_page_url("https://hanime1.me/", "weekly", 1).unwrap();
    assert!(weekly.starts_with("https://hanime1.me/search?"), "{weekly}");
    let query = |url: &str| -> Vec<(String, String)> { url::Url::parse(url).unwrap().query_pairs().into_owned().collect() };
    assert_eq!(
        query(&weekly),
        [("genre".into(), "裏番".into()), ("sort".into(), "本週排行".into())]
    );
    let monthly = build_ranking_page_url(HOST, "monthly", 2).unwrap();
    assert_eq!(
        query(&monthly),
        [
            ("genre".into(), "裏番".into()),
            ("sort".into(), "本月排行".into()),
            ("page".into(), "2".into())
        ]
    );
}

// py: tests/test_hanime1.py::test_extract_watch_metadata_parses_title_release_date_and_plot
#[test]
fn extract_watch_metadata_parses_title_release_date_and_plot() {
    let page = r#"
    <html>
      <head>
        <meta property="og:title" content="Demo Title - Hanime1.me" />
        <meta property="og:image" content="//cdn.example.com/covers/demo.jpg" />
      </head>
      <body>
        <div>觀看次數:301.5萬次&nbsp;&nbsp;2013-06-28</div>
        <a id="video-artist-name" href="/search?query=測試工坊">測試工坊</a>
        <div>公開測試 2</div>
        <div class="video-caption-text caption-ellipsis">
          這是學生們的課程介紹。
        </div>
      </body>
    </html>
    "#;
    let metadata = extract_watch_metadata(HOST, page);
    assert_eq!(metadata.title.as_deref(), Some("公开测试 2"));
    assert_eq!(metadata.uploader.as_deref(), Some("测试工坊"));
    assert_eq!(metadata.release_date.as_deref(), Some("2013-06-28"));
    assert_eq!(metadata.plot.as_deref(), Some("这是学生们的课程介绍。"));
    assert_eq!(metadata.cover_url.as_deref(), Some("https://cdn.example.com/covers/demo.jpg"));
}

// py: tests/test_hanime1.py::test_build_download_command_includes_headers
#[test]
fn build_download_command_includes_headers() {
    let command = build_download_command(
        "https://video.example.com/master.m3u8",
        "/tmp/video.%(ext)s",
        Some("https://hanime1.me/v/demo"),
        Some("cf_clearance=token"),
    );
    assert_eq!(command[0], "yt-dlp");
    for expected in [
        "--referer",
        "https://hanime1.me/v/demo",
        "--add-header",
        "Cookie:cf_clearance=token",
    ] {
        assert!(command.iter().any(|arg| arg == expected), "{expected}");
    }
    assert!(!command.iter().any(|arg| arg == "--proxy"));
}

// py: tests/test_hanime1.py::test_parse_runtime_seed_supports_id_and_title_id_formats
#[test]
fn parse_runtime_seed_supports_id_and_title_id_formats() {
    assert_eq!(parse_runtime_series_seed("12488").unwrap(), None);
    assert_eq!(parse_runtime_series_seed("屈辱 {id-12488}").unwrap(), Some(seed("12488", "屈辱")));
}

// py: tests/test_hanime1.py::test_extract_genre_from_watch_html_variants
#[test]
fn extract_genre_from_watch_html_variants() {
    let far = format!(
        r#"<a id="video-artist-name" href="/search?query=Maplestar">Maplestar</a><span>{}</span><a href="/search?genre=2D動畫">2D動畫</a>"#,
        "x".repeat(1200)
    );
    for (page, expected) in [
        (
            r#"<a id="video-artist-name" href="/search?query=Maplestar">Maplestar</a><a href="https://hanime1.me/search?genre=2D動畫">2D動畫</a>"#,
            Some("2D動畫"),
        ),
        (
            r#"<a id="video-artist-name" href="/search?query=メリー">メリー</a><a href="https://hanime1.me/search?genre=%E8%A3%8F%E7%95%AA">裏番</a>"#,
            Some("裏番"),
        ),
        // Nav-bar genre links without the artist anchor must not be picked up.
        (
            r#"<nav><a href="/search?genre=裏番">裏番</a><a href="/search?genre=MMD">MMD</a></nav>"#,
            None,
        ),
        // A genre link past the search window belongs to unrelated page areas.
        (far.as_str(), None),
    ] {
        assert_eq!(extract_genre_from_watch_html(page).as_deref(), expected, "{page}");
    }
}

// py: tests/test_hanime1.py::test_extract_watch_metadata_includes_simplified_genre
#[test]
fn extract_watch_metadata_includes_simplified_genre() {
    let page = r#"
    <html>
      <body>
        <div>觀看次數:43.4萬次&nbsp;&nbsp;2026-08-12</div>
        <a id="video-artist-name" href="https://hanime1.me/search?query=Maplestar">Maplestar</a>
        <a href="https://hanime1.me/search?genre=2D動畫">2D動畫</a>
        <div>SAKAMOTO DAYS FULL ANIMATION</div>
        <div class="video-caption-text">Title: SAKAMOTO DAYS FULL ANIMATION</div>
      </body>
    </html>
    "#;
    let metadata = extract_watch_metadata(HOST, page);
    assert_eq!(metadata.uploader.as_deref(), Some("Maplestar"));
    assert_eq!(metadata.genre.as_deref(), Some("2D动画"));
}

// py: tests/test_hanime1.py::test_strip_uploader_prefix
#[test]
fn strip_uploader_prefix_cases() {
    assert_eq!(
        strip_uploader_prefix("[Maplestar] SAKAMOTO DAYS", Some("Maplestar")),
        "SAKAMOTO DAYS"
    );
    assert_eq!(
        strip_uploader_prefix("[maplestar] SAKAMOTO DAYS", Some("Maplestar")),
        "SAKAMOTO DAYS"
    );
    assert_eq!(
        strip_uploader_prefix("[Other] SAKAMOTO DAYS", Some("Maplestar")),
        "[Other] SAKAMOTO DAYS"
    );
    assert_eq!(strip_uploader_prefix("SAKAMOTO DAYS", None), "SAKAMOTO DAYS");
    // A title that is nothing but the prefix keeps its original form.
    assert_eq!(strip_uploader_prefix("[Maplestar]", Some("Maplestar")), "[Maplestar]");
}

// py: tests/test_hanime1.py::test_stream_resolution_label_parses_quality
#[test]
fn stream_resolution_label_parses_quality() {
    assert_eq!(
        stream_resolution_label("https://video.example.com/18580-sc-720p.mp4").as_deref(),
        Some("720p")
    );
    assert_eq!(
        stream_resolution_label("https://video.example.com/18580-sc-4k.mp4?token=1").as_deref(),
        Some("2160p")
    );
    assert_eq!(stream_resolution_label("https://video.example.com/master.m3u8"), None);
}

// py: tests/test_hanime1.py::test_format_file_size_human_readable
#[test]
fn format_file_size_human_readable() {
    assert_eq!(format_file_size(Some(999)).as_deref(), Some("999 B"));
    assert_eq!(format_file_size(Some(1024)).as_deref(), Some("1.0 KB"));
    assert_eq!(format_file_size(Some(1024 * 1024)).as_deref(), Some("1.0 MB"));
    assert_eq!(format_file_size(Some(-1)), None);
    assert_eq!(format_file_size(None), None);
}

// py: tests/test_hanime1.py::test_get_cookie_header_uses_user_lang_only
#[tokio::test]
async fn get_cookie_header_uses_user_lang_only() {
    let harness = harness(unreachable_db(), |_| {}).await;
    assert_eq!(harness.run.cookie_header(), "user_lang=zhs");
}

// ---------- series collection ----------

// py: tests/test_hanime1.py::test_collect_ids_reverse_numbers_when_playlist_titles_are_not_distinct
#[tokio::test]
async fn collect_ids_reverse_numbers_when_playlist_titles_are_not_distinct() {
    let database = testutil::database().await;
    insert_series(&database, "157875", "クール de M", &[]).await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html(
        "/watch?v=157875",
        &titled_playlist_page("クール de M", &[("404988", "高冷的抖M"), ("157875", "高冷的抖M")]),
    );

    let collected = harness
        .run
        .collect_ids_from_watch_series(&[seed("157875", "クール de M")])
        .await
        .unwrap();

    assert_eq!(
        candidates(collected),
        [
            triple("404988", "クール de M", "クール de M 02"),
            triple("157875", "クール de M", "クール de M 01")
        ]
    );
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_collect_ids_keeps_playlist_titles_when_distinct
#[tokio::test]
async fn collect_ids_keeps_playlist_titles_when_distinct() {
    let database = testutil::database().await;
    insert_series(&database, "12488", "屈辱", &[]).await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html(
        "/watch?v=12488",
        &titled_playlist_page("屈辱", &[("13253", "屈辱 1"), ("12488", "屈辱 2")]),
    );

    let collected = harness.run.collect_ids_from_watch_series(&[seed("12488", "屈辱")]).await.unwrap();

    assert_eq!(
        candidates(collected),
        [triple("13253", "屈辱", "屈辱 1"), triple("12488", "屈辱", "屈辱 2")]
    );
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_collect_ids_uses_current_item_title_when_series_name_differs
#[tokio::test]
async fn collect_ids_uses_current_item_title_when_series_name_differs() {
    let database = testutil::database().await;
    insert_series(&database, "37176", "○○交配", &[]).await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html("/watch?v=37176", CURRENT_PLAYLIST_HTML);

    let collected = candidates(harness.run.collect_ids_from_watch_series(&[seed("37176", "○○交配")]).await.unwrap());

    assert_eq!(collected[0], triple("407017", "○○交配", "援助交配 11"));
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_collect_ids_uses_single_item_watch_link_title
#[tokio::test]
async fn collect_ids_uses_single_item_watch_link_title() {
    let database = testutil::database().await;
    insert_series(&database, "407018", "ヌきヌき ずっぽしイズム", &[]).await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html("/watch?v=407018", LINK_TITLE_PLAYLIST_HTML);

    let collected = candidates(
        harness
            .run
            .collect_ids_from_watch_series(&[seed("407018", "ヌきヌき ずっぽしイズム")])
            .await
            .unwrap(),
    );

    assert_eq!(collected, [triple("407018", "ヌきヌき ずっぽしイズム", "射精深陷主义 1")]);
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_collect_ids_upserts_series_members_and_marks_success
#[tokio::test]
async fn collect_ids_upserts_series_members_and_marks_success() {
    let database = testutil::database().await;
    insert_series(&database, "12488", "屈辱", &[]).await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html("/watch?v=12488", &playlist_page("屈辱", &["13253", "12488"]));

    let collected = harness.run.collect_ids_from_watch_series(&[seed("12488", "屈辱")]).await.unwrap();

    assert_eq!(
        candidates(collected),
        [triple("13253", "屈辱", "屈辱 02"), triple("12488", "屈辱", "屈辱 01")]
    );
    assert_eq!(
        member_rows(&database).await,
        [("12488".into(), "12488".into()), ("13253".into(), "12488".into())]
    );
    assert_eq!(series_scan_state(&database, "12488").await, (true, String::new()));
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_collect_ids_marks_series_failure
#[tokio::test]
async fn collect_ids_marks_series_failure() {
    let database = testutil::database().await;
    insert_series(&database, "12488", "屈辱", &[]).await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html("/watch?v=12488", CF_BLOCKED);

    let error = harness
        .run
        .collect_ids_from_watch_series(&[seed("12488", "屈辱")])
        .await
        .unwrap_err();

    assert_eq!(error.formatted(), "RuntimeError: Failed to resolve all 1 Hanime1 series targets");
    assert_eq!(
        series_scan_state(&database, "12488").await,
        (
            false,
            "ValueError: Blocked by Cloudflare while resolving Hanime1 watch page.".to_owned()
        )
    );
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_collect_ids_treats_empty_playlist_parse_as_incompatible
#[tokio::test]
async fn collect_ids_treats_empty_playlist_parse_as_incompatible() {
    let database = testutil::database().await;
    insert_series(&database, "12488", "屈辱", &[]).await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html("/watch?v=12488", "<div>no playlist</div>");

    let error = harness
        .run
        .collect_ids_from_watch_series(&[seed("12488", "屈辱")])
        .await
        .unwrap_err();

    assert_eq!(error.class_name(), "Hanime1ParserIncompatibleError");
    assert!(error.message().contains("playlist parsing failed"), "{}", error.message());
    assert_eq!(error.notification_dedupe_key(), Some("hanime1:parser:playlist-wrapper"));
    let (scanned, scan_error) = series_scan_state(&database, "12488").await;
    assert!(!scanned);
    assert_eq!(
        scan_error,
        "Hanime1ParserIncompatibleError: Hanime1 watch page did not expose a compatible playlist structure"
    );
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_collect_ids_does_not_label_mixed_failures_as_parser_incompatible
#[tokio::test]
async fn collect_ids_does_not_label_mixed_failures_as_parser_incompatible() {
    let database = testutil::database().await;
    insert_series(&database, "12488", "屈辱", &[]).await;
    insert_series(&database, "20000", "Other", &[]).await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html("/watch?v=12488", "<div>no playlist</div>");
    harness.pages.status("/watch?v=20000", 503);

    let error = harness
        .run
        .collect_ids_from_watch_series(&[seed("12488", "屈辱"), seed("20000", "Other")])
        .await
        .unwrap_err();

    assert_eq!(error.formatted(), "RuntimeError: Failed to resolve all 2 Hanime1 series targets");
    assert_eq!(error.notification_dedupe_key(), None);
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_load_runtime_series_seeds_reads_database
#[tokio::test]
async fn load_runtime_series_seeds_reads_database() {
    // The table's CHECKs keep empty ids and titles out, so only the order is left to see.
    let database = testutil::database().await;
    insert_series(&database, "12496", " 後宮 ", &[]).await;
    insert_series(&database, "12488", "屈辱", &[]).await;
    let harness = harness(database.db().clone(), |_| {}).await;

    let seeds = harness.run.load_runtime_series_seeds().await.unwrap();

    assert_eq!(seeds, [seed("12496", "後宮"), seed("12488", "屈辱")]);
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_get_items_from_series_seeds_filters_downloaded_ids
#[tokio::test]
async fn get_items_from_series_seeds_filters_downloaded_ids() {
    let database = testutil::database().await;
    insert_series(&database, "12488", "屈辱", &[]).await;
    insert_downloaded(&database, "13253").await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html("/watch?v=12488", &playlist_page("屈辱", &["12488", "13253"]));

    let items = harness.run.get_items(&[seed("12488", "屈辱")]).await.unwrap();

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, "12488");
    assert_eq!(items[0].title, "屈辱 02");
    assert_eq!(items[0].keyword.as_deref(), Some("屈辱"));
    assert_eq!(items[0].page_url.as_deref(), Some(harness.url("/watch?v=12488").as_str()));
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_get_items_skips_candidates_with_ignored_watch_markers
#[tokio::test]
async fn get_items_skips_candidates_with_ignored_watch_markers() {
    let database = testutil::database().await;
    insert_series(&database, "12488", "OVA Demo", &[]).await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html(
        "/watch?v=12488",
        &playlist_page("OVA Demo", &["157878", "404989", "143654", "404988"]),
    );
    for (id, title) in [
        ("404989", "OVA Demo [新番預告]"),
        ("143654", "OVA Demo [中字後補]"),
        ("404988", "OVA Demo [中文後補]"),
        ("157878", "OVA Demo Episode 1"),
    ] {
        harness
            .pages
            .html(&format!("/watch?v={id}"), &watch_page(title, None, None, None, None));
    }

    let items = harness.run.get_items(&[seed("12488", "OVA Demo")]).await.unwrap();

    let ids: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids, ["157878"]);
    assert_eq!(items[0].title, "OVA Demo 04");
    database.drop_database().await;
}

// ---------- ranking discovery ----------

/// Ranking pages, watch pages and known series for one discovery run.
async fn ranking_harness(database: &TestDatabase, quota: Option<f64>, max_extra_pages: i64) -> Harness {
    harness(database.db().clone(), |cfg| {
        cfg.ranking.enabled = true;
        cfg.ranking.periods = vec![RankingPeriod::Weekly, RankingPeriod::Monthly];
        cfg.ranking.pages = 1;
        cfg.ranking.deep_scan.enabled = quota.is_some();
        cfg.ranking.deep_scan.quota = quota.unwrap_or(0.25);
        cfg.ranking.deep_scan.max_extra_pages = max_extra_pages;
    })
    .await
}

fn ranking_key(period: &str, page: i64) -> String {
    let sort = if period == "weekly" { "本週排行" } else { "本月排行" };
    if page > 1 {
        format!("/search?genre=裏番&sort={sort}&page={page}")
    } else {
        format!("/search?genre=裏番&sort={sort}")
    }
}

fn ranking_page(ids: &[&str]) -> String {
    ids.iter().map(|id| format!(r#"<a href="/watch?v={id}">{id}</a>"#)).collect()
}

fn ranking_fetches(harness: &Harness) -> Vec<String> {
    harness
        .requests()
        .into_iter()
        .filter(|request| request.starts_with("/search"))
        .collect()
}

fn watch_fetches(harness: &Harness) -> Vec<String> {
    harness
        .requests()
        .into_iter()
        .filter(|request| request.starts_with("/watch"))
        .collect()
}

async fn set_debt(database: &TestDatabase, debt: f64) {
    sqlx::query("INSERT INTO hanime1_ranking_scan_state (id, quota_debt) VALUES (1, $1)")
        .bind(debt)
        .execute(database.db().pool())
        .await
        .unwrap();
}

// py: tests/test_hanime1.py::test_fetch_ranking_video_ids_reads_search_page
#[tokio::test]
async fn fetch_ranking_video_ids_reads_search_page() {
    let harness = harness(unreachable_db(), |_| {}).await;
    harness.pages.html(
        &ranking_key("weekly", 1),
        r#"
        <a href="https://hanime1.me/watch?v=10001">a</a>
        <a href="/watch?v=10002">b</a>
        <a href="/watch?v=10001">dup</a>
        "#,
    );

    let ids = harness.run.fetch_ranking_video_ids("weekly", 1).await.unwrap();

    assert_eq!(ids, strings(&["10001", "10002"]));
    let received = harness.site.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].header("cookie"), Some("user_lang=zhs"));
    assert_eq!(received[0].param("genre"), Some("裏番"));
    assert_eq!(received[0].param("sort"), Some("本週排行"));
}

// py: tests/test_hanime1.py::test_discover_ranking_series_adds_new_series_targets
#[tokio::test]
async fn discover_ranking_series_adds_new_series_targets() {
    let database = testutil::database().await;
    insert_series(&database, "200", "既存", &["200"]).await;
    let harness = ranking_harness(&database, None, 5).await;
    harness.pages.html(&ranking_key("weekly", 1), &ranking_page(&["100", "200"]));
    harness.pages.html(&ranking_key("monthly", 1), &ranking_page(&["300"]));
    harness.pages.html("/watch?v=100", &playlist_page("測試系列", &["100", "101"]));
    harness.pages.html("/watch?v=300", &playlist_page("既存系列", &["200", "300"]));

    let added = harness.run.discover_ranking_series().await.unwrap();

    assert_eq!(added, [seed("100", "测试系列")]);
    assert_eq!(
        series_rows(&database).await,
        [
            ("100".into(), "测试系列".into(), "100".into()),
            ("200".into(), "既存".into(), "200".into())
        ]
    );
    assert_eq!(
        member_rows(&database).await,
        [
            ("100".into(), "100".into()),
            ("101".into(), "100".into()),
            ("200".into(), "200".into())
        ]
    );
    // The known video is never resolved; deep scan is off, so no debt is kept.
    assert_eq!(watch_fetches(&harness), ["/watch?v=100", "/watch?v=300"]);
    assert_eq!(saved_debt(&database).await, None);
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_discover_ranking_series_skips_deep_scan_when_disabled
#[tokio::test]
async fn discover_ranking_series_skips_deep_scan_when_disabled() {
    let database = testutil::database().await;
    let harness = ranking_harness(&database, None, 5).await;

    let added = harness.run.discover_ranking_series().await.unwrap();

    assert!(added.is_empty());
    assert_eq!(ranking_fetches(&harness), [ranking_key("weekly", 1), ranking_key("monthly", 1)]);
    assert_eq!(saved_debt(&database).await, None);
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_deep_scan_accumulates_fractional_debt_without_fetching
#[tokio::test]
async fn deep_scan_accumulates_fractional_debt_without_fetching() {
    let database = testutil::database().await;
    set_debt(&database, 0.5).await;
    let harness = ranking_harness(&database, Some(0.25), 5).await;

    let added = harness.run.discover_ranking_series().await.unwrap();

    assert!(added.is_empty());
    assert_eq!(ranking_fetches(&harness), [ranking_key("weekly", 1), ranking_key("monthly", 1)]);
    assert_eq!(saved_debt(&database).await, Some(0.75));
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_deep_scan_triggers_at_full_debt_and_stops_at_quota
#[tokio::test]
async fn deep_scan_triggers_at_full_debt_and_stops_at_quota() {
    let database = testutil::database().await;
    set_debt(&database, 0.75).await;
    let harness = ranking_harness(&database, Some(0.25), 5).await;
    harness.pages.html(&ranking_key("weekly", 2), &ranking_page(&["400", "500"]));
    harness.pages.html("/watch?v=400", &playlist_page("新系列400", &["400", "401"]));
    harness.pages.html("/watch?v=500", &playlist_page("新系列500", &["500"]));

    let added = harness.run.discover_ranking_series().await.unwrap();

    assert_eq!(added, [seed("400", "新系列400")]);
    assert_eq!(&ranking_fetches(&harness)[2..], [ranking_key("weekly", 2)]);
    // The second unknown video on the page is not resolved once the quota is met.
    assert_eq!(watch_fetches(&harness), ["/watch?v=400"]);
    assert_eq!(saved_debt(&database).await, Some(0.0));
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_deep_scan_walks_extra_pages_page_major
#[tokio::test]
async fn deep_scan_walks_extra_pages_page_major() {
    let database = testutil::database().await;
    insert_series(&database, "900", "Known", &["900", "901"]).await;
    let harness = ranking_harness(&database, Some(1.0), 5).await;
    harness.pages.html(&ranking_key("weekly", 2), &ranking_page(&["900"]));
    harness.pages.html(&ranking_key("monthly", 2), &ranking_page(&["901"]));
    harness.pages.html(&ranking_key("weekly", 3), &ranking_page(&["400"]));
    harness.pages.html("/watch?v=400", &playlist_page("新系列400", &["400"]));

    let added = harness.run.discover_ranking_series().await.unwrap();

    assert_eq!(added, [seed("400", "新系列400")]);
    assert_eq!(
        &ranking_fetches(&harness)[2..],
        [ranking_key("weekly", 2), ranking_key("monthly", 2), ranking_key("weekly", 3)]
    );
    assert_eq!(saved_debt(&database).await, Some(0.0));
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_deep_scan_skips_when_base_pages_cover_quota
#[tokio::test]
async fn deep_scan_skips_when_base_pages_cover_quota() {
    let database = testutil::database().await;
    let harness = ranking_harness(&database, Some(1.0), 5).await;
    harness.pages.html(&ranking_key("weekly", 1), &ranking_page(&["100"]));
    harness.pages.html("/watch?v=100", &playlist_page("新系列100", &["100"]));

    let added = harness.run.discover_ranking_series().await.unwrap();

    assert_eq!(added, [seed("100", "新系列100")]);
    assert_eq!(ranking_fetches(&harness), [ranking_key("weekly", 1), ranking_key("monthly", 1)]);
    assert_eq!(saved_debt(&database).await, Some(0.0));
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_deep_scan_caps_accumulated_debt
#[tokio::test]
async fn deep_scan_caps_accumulated_debt() {
    let database = testutil::database().await;
    set_debt(&database, 100.0).await;
    let harness = ranking_harness(&database, Some(0.25), 5).await;
    harness.pages.html(&ranking_key("weekly", 2), &ranking_page(&["400", "500"]));
    harness.pages.html("/watch?v=400", &playlist_page("新系列400", &["400"]));
    harness.pages.html("/watch?v=500", &playlist_page("新系列500", &["500"]));

    let added = harness.run.discover_ranking_series().await.unwrap();

    // Debt is capped at max(1, ceil(quota)), so one new series settles it.
    assert_eq!(added, [seed("400", "新系列400")]);
    assert_eq!(&ranking_fetches(&harness)[2..], [ranking_key("weekly", 2)]);
    assert_eq!(saved_debt(&database).await, Some(0.0));
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_deep_scan_keeps_debt_when_periods_exhausted
#[tokio::test]
async fn deep_scan_keeps_debt_when_periods_exhausted() {
    let database = testutil::database().await;
    set_debt(&database, 0.5).await;
    let harness = ranking_harness(&database, Some(0.5), 5).await;
    harness.pages.html(&ranking_key("weekly", 2), "<div>no results</div>");
    harness.pages.status(&ranking_key("monthly", 2), 500);

    let added = harness.run.discover_ranking_series().await.unwrap();

    assert!(added.is_empty());
    assert_eq!(
        &ranking_fetches(&harness)[2..],
        [ranking_key("weekly", 2), ranking_key("monthly", 2)]
    );
    assert_eq!(saved_debt(&database).await, Some(1.0));
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_deep_scan_respects_max_extra_pages
#[tokio::test]
async fn deep_scan_respects_max_extra_pages() {
    let database = testutil::database().await;
    insert_series(&database, "900", "Known", &["900"]).await;
    let harness = ranking_harness(&database, Some(1.0), 2).await;
    for (period, page) in [("weekly", 2), ("monthly", 2), ("weekly", 3), ("monthly", 3)] {
        harness.pages.html(&ranking_key(period, page), &ranking_page(&["900"]));
    }
    harness.pages.html(&ranking_key("weekly", 4), &ranking_page(&["400"]));
    harness.pages.html("/watch?v=400", &playlist_page("新系列400", &["400"]));

    let added = harness.run.discover_ranking_series().await.unwrap();

    assert!(added.is_empty());
    assert_eq!(
        &ranking_fetches(&harness)[2..],
        [
            ranking_key("weekly", 2),
            ranking_key("monthly", 2),
            ranking_key("weekly", 3),
            ranking_key("monthly", 3)
        ]
    );
    assert_eq!(saved_debt(&database).await, Some(1.0));
    database.drop_database().await;
}

// ---------- notifications ----------

async fn notification_row(database: &TestDatabase, dedupe_key: &str) -> Option<(String, String, String)> {
    sqlx::query("SELECT header, title, webhook_action FROM notifications WHERE dedupe_key = $1")
        .bind(dedupe_key)
        .fetch_optional(database.db().pool())
        .await
        .unwrap()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
}

async fn queue_parser_failure(harness: &Harness) {
    harness
        .run
        .outbox
        .enqueue(&NewNotification {
            kind: "job_failed".to_owned(),
            source: "worker".to_owned(),
            header: "Hanime1".to_owned(),
            title: "Job failed".to_owned(),
            body: "Hanime1ParserIncompatibleError: bad structure".to_owned(),
            dedupe_key: parser_failure_dedupe_key(),
            ..NewNotification::default()
        })
        .await
        .unwrap();
}

// py: tests/test_hanime1.py::test_resolve_parser_failure_notification_uses_job_failure_dedupe_key
#[tokio::test]
async fn resolve_parser_failure_notification_uses_job_failure_dedupe_key() {
    let database = testutil::database().await;
    let harness = harness(database.db().clone(), |_| {}).await;
    assert_eq!(parser_failure_dedupe_key(), "job_failed:hanime1:hanime1:parser:playlist-wrapper");
    queue_parser_failure(&harness).await;

    harness.run.resolve_parser_failure_notification().await;

    assert_eq!(
        notification_row(&database, &parser_failure_dedupe_key()).await,
        Some(("Hanime1".to_owned(), "Job recovered".to_owned(), "resolve".to_owned()))
    );
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_update_resolves_parser_alert_after_scan_recovers
#[tokio::test]
async fn update_resolves_parser_alert_after_scan_recovers() {
    let database = testutil::database().await;
    insert_series(&database, "12488", "屈辱", &[]).await;
    let harness = harness(database.db().clone(), |_| {}).await;
    queue_parser_failure(&harness).await;
    let queued = notification_row(&database, &parser_failure_dedupe_key()).await;
    harness.pages.html("/watch?v=12488", "<div>no playlist</div>");

    let error = harness.run.update().await.unwrap_err();
    assert_eq!(error.class_name(), "Hanime1ParserIncompatibleError");
    // A failing scan leaves the alert alone.
    assert_eq!(notification_row(&database, &parser_failure_dedupe_key()).await, queued);

    harness.pages.html("/watch?v=12488", &playlist_page("屈辱", &["12488"]));
    insert_downloaded(&database, "12488").await;
    harness.run.update().await.unwrap();
    assert_eq!(
        notification_row(&database, &parser_failure_dedupe_key()).await.unwrap().2,
        "resolve"
    );
    database.drop_database().await;
}

fn notify_item() -> HanimeRecord {
    HanimeRecord {
        id: "12345".to_owned(),
        title: "公開便所 2".to_owned(),
        uploader: Some("Uploader_One".to_owned()),
        page_url: Some("https://hanime1.me/watch?v=12345".to_owned()),
        ..HanimeRecord::default()
    }
}

// py: tests/test_hanime1.py::test_notify_download_enqueues_structured_payload
#[tokio::test]
async fn notify_download_enqueues_structured_payload() {
    let harness = harness(unreachable_db(), |_| {}).await;

    harness
        .run
        .notify_download(&notify_item(), Some("720p"), Some(1024 * 1024), Some("2020-01-01"), None)
        .await
        .unwrap();

    let sent = harness.notifier.sent();
    assert_eq!(sent.len(), 1);
    let notification = &sent[0];
    assert_eq!(notification.kind, "download_completed");
    assert_eq!(notification.source, "hanime1");
    assert_eq!(notification.header, "Hanime1");
    assert_eq!(notification.title, "公開便所 2");
    assert_eq!(notification.body, "720p | 1.0 MB | 2020-01-01");
    assert_eq!(notification.link_url, "https://hanime1.me/watch?v=12345");
    assert_eq!(notification.image_url, "");
    let payload = fav_core::pyjson::dumps(
        &fav_core::pyjson::Value::Dict(notification.payload.clone().unwrap()),
        &fav_core::pyjson::DumpOptions::python_default(),
    )
    .unwrap();
    assert_eq!(
        payload,
        r#"{"video_id": "12345", "resolution": "720p", "file_size_bytes": 1048576, "release_date": "2020-01-01"}"#
    );
}

// py: tests/test_hanime1.py::test_notify_download_keeps_cover_url_when_available
#[tokio::test]
async fn notify_download_keeps_cover_url_when_available() {
    let harness = harness(unreachable_db(), |_| {}).await;

    harness
        .run
        .notify_download(
            &notify_item(),
            Some("720p"),
            Some(1024 * 1024),
            Some("2020-01-01"),
            Some("https://cdn.example.com/covers/12345.jpg"),
        )
        .await
        .unwrap();

    assert_eq!(harness.notifier.sent()[0].image_url, "https://cdn.example.com/covers/12345.jpg");
}

#[tokio::test]
async fn notify_download_labels_unknown_fields() {
    let harness = harness(unreachable_db(), |_| {}).await;
    let item = HanimeRecord {
        id: "12345".to_owned(),
        title: "  ".to_owned(),
        ..HanimeRecord::default()
    };

    harness.run.notify_download(&item, None, None, Some("  "), None).await.unwrap();

    let sent = harness.notifier.sent();
    assert_eq!(sent[0].title, "12345");
    assert_eq!(sent[0].body, "unknown | unknown | unknown");
    assert_eq!(sent[0].link_url, harness.url("/watch?v=12345"));
}

// ---------- downloads ----------

fn record(
    id: &str,
    title: &str,
    keyword: &str,
    uploader: Option<&str>,
    page_url: Option<String>,
    stream_url: Option<&str>,
) -> HanimeRecord {
    HanimeRecord {
        id: id.to_owned(),
        title: title.to_owned(),
        keyword: Some(keyword.to_owned()),
        uploader: uploader.map(str::to_owned),
        page_url,
        stream_url: stream_url.map(str::to_owned),
    }
}

fn relative(harness: &Harness, path: &Path) -> String {
    path.strip_prefix(&harness.root).unwrap().to_string_lossy().into_owned()
}

// py: tests/test_hanime1.py::test_download_item_renames_and_moves_file
#[tokio::test]
async fn download_item_renames_and_moves_file() {
    let harness = harness(unreachable_db(), |_| {}).await;
    let page_url = harness.url("/v/demo");
    let item = record("video-123", "", "公開便所", Some("Uploader"), Some(page_url.clone()), None);
    harness.pages.html(
        "/download?v=video-123",
        r#"<title>Resolved Title</title><a data-url="https://video.example.com/master.mp4?token=abc">dl</a>"#,
    );
    harness.pages.html(
        "/watch?v=video-123",
        &watch_page("Watch Title", None, Some("裏番"), Some("2020-01-01"), Some("watch plot")),
    );

    let result = harness.run.download_item(&item).await.unwrap();

    assert_eq!(result.title, "Watch Title");
    assert_eq!(result.archive_title, "Watch Title");
    assert_eq!(result.stream_url, "https://video.example.com/master.mp4?token=abc");
    assert_eq!(result.resolution, None);
    assert_eq!(result.release_date.as_deref(), Some("2020-01-01"));
    assert_eq!(result.plot.as_deref(), Some("watch plot"));
    assert_eq!(result.cover_url, None);
    assert!(result.final_path.exists());
    assert_eq!(relative(&harness, &result.final_path), "里番/公開便所/Watch Title [video-123].mp4");
    let calls = harness.runner.calls();
    assert_eq!(calls.len(), 1);
    let argv = &calls[0];
    assert_eq!(argv.last().unwrap(), "https://video.example.com/master.mp4?token=abc");
    assert!(argv.windows(2).any(|pair| pair == ["--referer", page_url.as_str()]));
    assert!(argv.windows(2).any(|pair| pair == ["--add-header", "Cookie:user_lang=zhs"]));
    assert!(argv[2].ends_with("/videos/video-123.%(ext)s"), "{}", argv[2]);
}

// py: tests/test_hanime1.py::test_download_item_uses_uploader_from_watch_metadata_when_missing
#[tokio::test]
async fn download_item_uses_uploader_from_watch_metadata_when_missing() {
    let harness = harness(unreachable_db(), |_| {}).await;
    let item = record("video-234", "Playlist Title 01", "tag-b", None, Some(harness.url("/v/demo")), None);
    harness.pages.html(
        "/download?v=video-234",
        r#"<title>Resolved Title</title><a data-url="https://video.example.com/master.mp4?token=abc">dl</a>"#,
    );
    harness.pages.html(
        "/watch?v=video-234",
        &watch_page(
            "Watch Title",
            Some("Watch Studio"),
            Some("里番"),
            Some("2020-01-01"),
            Some("watch plot"),
        ),
    );

    let result = harness.run.download_item(&item).await.unwrap();

    assert_eq!(result.title, "Watch Title");
    assert_eq!(result.archive_title, "Playlist Title 01");
    assert_eq!(result.uploader.as_deref(), Some("Watch Studio"));
    assert_eq!(result.resolution, None);
    assert_eq!(result.cover_url, None);
    assert_eq!(
        relative(&harness, &result.final_path),
        "里番/tag-b/Playlist Title 01 [video-234].mp4"
    );
}

// py: tests/test_hanime1.py::test_download_item_keeps_playlist_item_title_for_path_and_watch_title_for_metadata
#[tokio::test]
async fn download_item_keeps_playlist_item_title_for_path_and_watch_title_for_metadata() {
    let harness = harness(unreachable_db(), |_| {}).await;
    let item = record("407017", "援助交配 11", "○○交配", None, Some(harness.url("/watch?v=407017")), None);
    harness.pages.html(
        "/download?v=407017",
        r#"<title>援助交配 11</title><a data-url="https://video.example.com/407017-1080p.mp4">dl</a>"#,
    );
    harness.pages.html(
        "/watch?v=407017",
        &watch_page("○○交配 第十一話 前編 [中文字幕]", None, Some("里番"), None, None),
    );

    let result = harness.run.download_item(&item).await.unwrap();

    assert_eq!(result.title, "○○交配 第十一话 前编 [中文字幕]");
    assert_eq!(result.archive_title, "援助交配 11");
    assert_eq!(result.resolution.as_deref(), Some("1080p"));
    assert_eq!(relative(&harness, &result.final_path), "里番/○○交配/援助交配 11 [407017].mp4");
}

// py: tests/test_hanime1.py::test_download_item_raises_for_ignored_site_markers
#[tokio::test]
async fn download_item_raises_for_ignored_site_markers() {
    let harness = harness(unreachable_db(), |_| {}).await;
    let item = record(
        "video-ignored",
        "OVA Demo [中字後補]",
        "tag-b",
        None,
        Some(harness.url("/watch?v=video-ignored")),
        Some("https://video.example.com/master.m3u8"),
    );

    let Err(ItemError::Ignored(message)) = harness.run.download_item(&item).await else {
        panic!("expected an ignored item");
    };

    assert!(message.contains("ignored marker [中字后补]"), "{message}");
    assert!(harness.site.received().is_empty());
}

// py: tests/test_hanime1.py::test_download_item_archives_author_video_under_genre_and_author
#[tokio::test]
async fn download_item_archives_author_video_under_genre_and_author() {
    let harness = harness(unreachable_db(), |_| {}).await;
    let item = record(
        "407657",
        "SAKAMOTO DAYS FULL ANIMATION",
        "Maplestar",
        Some("Maplestar"),
        Some(harness.url("/watch?v=407657")),
        None,
    );
    harness.pages.html(
        "/download?v=407657",
        r#"<title>[Maplestar] SAKAMOTO DAYS FULL ANIMATION</title><a data-url="https://video.example.com/407657-1080p.mp4">dl</a>"#,
    );
    harness.pages.html(
        "/watch?v=407657",
        &watch_page(
            "[Maplestar] SAKAMOTO DAYS FULL ANIMATION",
            Some("Maplestar"),
            Some("2D動畫"),
            None,
            None,
        ),
    );

    let result = harness.run.download_item(&item).await.unwrap();

    assert_eq!(result.title, "[Maplestar] SAKAMOTO DAYS FULL ANIMATION");
    assert_eq!(result.archive_title, "SAKAMOTO DAYS FULL ANIMATION");
    assert_eq!(
        relative(&harness, &result.final_path),
        "2D动画/Maplestar/SAKAMOTO DAYS FULL ANIMATION [407657].mp4"
    );
}

// py: tests/test_hanime1.py::test_download_item_strips_author_prefix_when_item_title_missing
#[tokio::test]
async fn download_item_strips_author_prefix_when_item_title_missing() {
    let harness = harness(unreachable_db(), |_| {}).await;
    let item = record(
        "407657",
        "",
        "Maplestar",
        Some("Maplestar"),
        Some(harness.url("/watch?v=407657")),
        None,
    );
    harness.pages.html(
        "/download?v=407657",
        r#"<a data-url="https://video.example.com/407657-1080p.mp4">dl</a>"#,
    );
    harness.pages.html(
        "/watch?v=407657",
        &watch_page(
            "[Maplestar] SAKAMOTO DAYS FULL ANIMATION",
            Some("Maplestar"),
            Some("2D动画"),
            None,
            None,
        ),
    );

    let result = harness.run.download_item(&item).await.unwrap();

    assert_eq!(result.title, "[Maplestar] SAKAMOTO DAYS FULL ANIMATION");
    assert_eq!(result.archive_title, "SAKAMOTO DAYS FULL ANIMATION");
    assert_eq!(
        relative(&harness, &result.final_path),
        "2D动画/Maplestar/SAKAMOTO DAYS FULL ANIMATION [407657].mp4"
    );
}

// py: tests/test_hanime1.py::test_download_item_falls_back_to_uncategorized_without_genre
#[test]
fn download_item_falls_back_to_uncategorized_without_genre() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let mut outcome = None;
    let records = fav_core::logging::capture(|| {
        outcome = Some(runtime.block_on(async {
            let harness = harness(unreachable_db(), |_| {}).await;
            let item = record(
                "407657",
                "Some Title",
                "Maplestar",
                Some("Maplestar"),
                Some(harness.url("/watch?v=407657")),
                Some("https://video.example.com/407657-1080p.mp4"),
            );
            harness
                .pages
                .html("/watch?v=407657", &watch_page("Some Title", None, None, None, None));
            let result = harness.run.download_item(&item).await.unwrap();
            relative(&harness, &result.final_path)
        }));
    });
    assert_eq!(outcome.as_deref(), Some("未分类/Maplestar/Some Title [407657].mp4"));
    assert!(
        records.iter().any(|record| record.message.contains("no resolvable genre")),
        "{records:?}"
    );
}

#[tokio::test]
async fn download_item_falls_back_to_the_watch_page_stream() {
    let harness = harness(unreachable_db(), |_| {}).await;
    let item = record("12488", "Title", "Series", None, Some(harness.url("/watch?v=12488")), None);
    harness.pages.status("/download?v=12488", 500);
    harness.pages.html(
        "/watch?v=12488",
        r#"<title>Watch</title><script>var s = "https:\/\/cdn.example.com\/a-720p.mp4?x=1";</script>"#,
    );

    let result = harness.run.download_item(&item).await.unwrap();

    assert_eq!(result.stream_url, "https://cdn.example.com/a-720p.mp4?x=1");
    assert_eq!(result.resolution.as_deref(), Some("720p"));
    let without_page = record("12488", "Title", "Series", None, None, None);
    let Err(ItemError::Failed(error)) = harness.run.download_item(&without_page).await else {
        panic!("expected a failure");
    };
    assert_eq!(
        error.formatted(),
        "ValueError: Item 12488 has neither valid download page nor page_url"
    );
}

#[tokio::test]
async fn download_stream_retries_yt_dlp_twice() {
    let harness = harness(unreachable_db(), |_| {}).await;
    let item = record("1", "Title", "Series", None, None, Some("https://video.example.com/1-720p.mp4"));
    let failure = |stderr: &str| CommandOutput {
        code: Some(1),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    };
    harness.runner.fail_next(failure("ERROR: first"));
    harness.runner.fail_next(failure(""));

    harness.run.download_item(&item).await.unwrap();

    assert_eq!(harness.runner.calls().len(), 3);
    assert_eq!(harness.sleeps.delays(), [5.0, 10.0]);

    for stderr in ["ERROR: one", "ERROR: two", "ERROR: three"] {
        harness.runner.fail_next(failure(stderr));
    }
    let Err(ItemError::Failed(error)) = harness.run.download_item(&item).await else {
        panic!("expected a failure");
    };
    assert_eq!(error.formatted(), "DownloadError: ERROR: three");
    assert_eq!(harness.sleeps.delays(), [5.0, 10.0, 5.0, 10.0]);
}

// py: tests/test_hanime1.py::test_update_inserts_item_after_download
#[tokio::test]
async fn update_inserts_item_after_download() {
    let database = testutil::database().await;
    insert_series(&database, "12488", "系列", &[]).await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html("/watch?v=12488", &playlist_page("系列", &["12345"]));
    harness.pages.html(
        "/watch?v=12345",
        &watch_page("Resolved Title", Some("Uploader"), Some("里番"), None, None),
    );
    harness
        .pages
        .html("/download?v=12345", r#"<a data-url="https://video.example.com/master.m3u8">dl</a>"#);

    harness.run.update().await.unwrap();

    assert_eq!(
        hanime1_rows(&database).await,
        [(
            "12345".into(),
            "Resolved Title".into(),
            "Uploader".into(),
            String::new(),
            String::new()
        )]
    );
    let sent = harness.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].title, "Resolved Title");
    assert_eq!(sent[0].body, "unknown | 5 B | unknown");
    assert!(harness.root.join("里番/系列/系列 01 [12345].mp4").exists());
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_update_skips_ignored_items_without_db_insert
#[tokio::test]
async fn update_skips_ignored_items_without_db_insert() {
    let database = testutil::database().await;
    let harness = harness(database.db().clone(), |_| {}).await;
    let item = record(
        "12345",
        "OVA Demo [新番預告]",
        "kw",
        Some("Uploader"),
        Some(harness.url("/watch?v=12345")),
        Some("https://video.example.com/master.m3u8"),
    );

    harness.run.download_and_record(vec![item]).await.unwrap();

    assert!(hanime1_rows(&database).await.is_empty());
    assert!(harness.notifier.sent().is_empty());
    assert!(harness.runner.calls().is_empty());
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_update_runs_ranking_discovery_before_get_items
#[tokio::test]
async fn update_runs_ranking_discovery_before_get_items() {
    let database = testutil::database().await;
    insert_series(&database, "100", "Seed", &[]).await;
    let harness = harness(database.db().clone(), |cfg| {
        cfg.ranking.enabled = true;
        cfg.ranking.periods = vec![RankingPeriod::Weekly];
    })
    .await;
    harness.pages.html("/watch?v=100", &playlist_page("Seed", &["100"]));
    insert_downloaded(&database, "100").await;

    harness.run.update().await.unwrap();

    assert_eq!(harness.requests(), [ranking_key("weekly", 1), "/watch?v=100".to_owned()]);
    database.drop_database().await;
}

// ---------- authors ----------

const AUTHOR_LISTING: &str = r#"
<a href="https://hanime1.me/watch?v=407657" class="video-link">
  <div class="thumb-container"><div class="duration">03:14</div></div>
  <div class="title">SAKAMOTO DAYS FULL ANIMATION</div>
</a>
<a class="video-link" href="/watch?v=406852">
  <div class="title">Reze (Chainsaw Man) - Animation</div>
</a>
"#;

fn listing(entries: &[(&str, &str)]) -> String {
    entries
        .iter()
        .map(|(id, title)| format!(r#"<a class="video-link" href="/watch?v={id}"><div class="title">{title}</div></a>"#))
        .collect()
}

fn entry_ids(entries: &[crate::hanime1_author::Hanime1AuthorVideoEntry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.video_id.as_str()).collect()
}

// py: tests/test_hanime1.py::test_fetch_author_page_entries_builds_url_and_parses_listing
#[tokio::test]
async fn fetch_author_page_entries_builds_url_and_parses_listing() {
    let harness = harness(unreachable_db(), |_| {}).await;
    harness.pages.html("/user/202534/uploaded", AUTHOR_LISTING);
    harness.pages.html("/user/202534/uploaded?page=2", AUTHOR_LISTING);

    let entries = harness.run.fetch_author_page_entries("202534", 1).await.unwrap();
    harness.run.fetch_author_page_entries("202534", 2).await.unwrap();

    assert_eq!(entry_ids(&entries), ["407657", "406852"]);
    assert_eq!(entries[0].title.as_deref(), Some("SAKAMOTO DAYS FULL ANIMATION"));
    assert_eq!(entries[1].title.as_deref(), Some("Reze (Chainsaw Man) - Animation"));
    assert_eq!(harness.requests(), ["/user/202534/uploaded", "/user/202534/uploaded?page=2"]);
    assert!(
        harness
            .site
            .received()
            .iter()
            .all(|received| received.header("cookie") == Some("user_lang=zhs"))
    );
}

// py: tests/test_hanime1.py::test_fetch_author_page_entries_treats_404_as_empty
#[tokio::test]
async fn fetch_author_page_entries_treats_404_as_empty() {
    let harness = harness(unreachable_db(), |_| {}).await;

    assert!(harness.run.fetch_author_page_entries("202534", 9).await.unwrap().is_empty());
    assert_eq!(harness.requests(), ["/user/202534/uploaded?page=9"]);
    // Any other error status fails the page.
    harness.pages.status("/user/202534/uploaded?page=9", 500);
    assert_eq!(
        harness.run.fetch_author_page_entries("202534", 9).await.unwrap_err().class_name(),
        "HTTPStatusError"
    );
}

// py: tests/test_hanime1.py::test_fetch_all_author_entries_stops_on_empty_and_echo_pages
#[tokio::test]
async fn fetch_all_author_entries_stops_on_empty_and_echo_pages() {
    let harness = harness(unreachable_db(), |_| {}).await;
    harness.pages.html("/user/202534/uploaded", &listing(&[("1", "a"), ("2", "b")]));
    harness.pages.html("/user/202534/uploaded?page=2", &listing(&[("3", "c")]));
    harness.pages.html("/user/202534/uploaded?page=3", "");

    let entries = harness.run.fetch_all_author_entries("202534").await.unwrap();

    assert_eq!(entry_ids(&entries), ["1", "2", "3"]);
    let pages = [
        "/user/202534/uploaded",
        "/user/202534/uploaded?page=2",
        "/user/202534/uploaded?page=3",
    ];
    assert_eq!(harness.requests(), pages);

    // A page that only repeats known videos also terminates pagination.
    harness
        .pages
        .html("/user/202534/uploaded?page=3", &listing(&[("1", "a"), ("2", "b")]));
    let entries = harness.run.fetch_all_author_entries("202534").await.unwrap();
    assert_eq!(entry_ids(&entries), ["1", "2", "3"]);
    assert_eq!(harness.requests()[3..], pages);
}

// py: tests/test_hanime1.py::test_fetch_all_author_entries_respects_max_pages_cap
#[tokio::test]
async fn fetch_all_author_entries_respects_max_pages_cap() {
    let harness = harness(unreachable_db(), |_| {}).await;
    harness.pages.html("/user/202534/uploaded", &listing(&[("1", "page 1")]));
    for page in 2..=60 {
        harness.pages.html(
            &format!("/user/202534/uploaded?page={page}"),
            &listing(&[(&page.to_string(), "page")]),
        );
    }

    let entries = harness.run.fetch_all_author_entries("202534").await.unwrap();

    assert_eq!(entries.len(), 50);
    assert_eq!(entries.last().unwrap().video_id, "50");
    assert_eq!(harness.requests().len(), 50);
}

// py: tests/test_hanime1.py::test_collect_author_candidates_marks_failure_and_continues
#[tokio::test]
async fn collect_author_candidates_marks_failure_and_continues() {
    let database = testutil::database().await;
    insert_author(&database, "111", "Broken").await;
    insert_author(&database, "202534", "Maplestar").await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html("/user/111/uploaded", CF_BLOCKED);
    harness
        .pages
        .html("/user/202534/uploaded", &listing(&[("407657", "SAKAMOTO DAYS FULL ANIMATION")]));

    let authors = vec![
        ("111".to_owned(), "Broken".to_owned()),
        ("202534".to_owned(), "Maplestar".to_owned()),
    ];
    let collected = candidates(harness.run.collect_author_candidates(&authors).await.unwrap());

    assert_eq!(collected, [triple("407657", "Maplestar", "SAKAMOTO DAYS FULL ANIMATION")]);
    let rows: Vec<(String, i32, String, bool)> =
        sqlx::query("SELECT author_id, video_count, last_scan_error, last_scanned_at IS NOT NULL FROM hanime1_author ORDER BY author_id")
            .fetch_all(database.db().pool())
            .await
            .unwrap()
            .iter()
            .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
            .collect();
    assert_eq!(
        rows,
        [
            (
                "111".to_owned(),
                0,
                "ValueError: Blocked by Cloudflare while resolving Hanime1 author page.".to_owned(),
                false
            ),
            ("202534".to_owned(), 1, String::new(), true),
        ]
    );
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_get_author_items_filters_downloaded_and_ignored
#[tokio::test]
async fn get_author_items_filters_downloaded_and_ignored() {
    let database = testutil::database().await;
    insert_author(&database, "202534", "Maplestar").await;
    insert_downloaded(&database, "406852").await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness.pages.html(
        "/user/202534/uploaded",
        &listing(&[
            ("407657", "SAKAMOTO DAYS FULL ANIMATION"),
            ("406852", "Reze - Animation"),
            ("405944", "OVA Demo [新番預告]"),
        ]),
    );

    let items = harness.run.get_author_items().await.unwrap();

    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.id, "407657");
    assert_eq!(item.title, "SAKAMOTO DAYS FULL ANIMATION");
    assert_eq!(item.keyword.as_deref(), Some("Maplestar"));
    assert_eq!(item.uploader.as_deref(), Some("Maplestar"));
    assert_eq!(item.page_url.as_deref(), Some(harness.url("/watch?v=407657").as_str()));
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_update_downloads_series_then_author_items
#[tokio::test]
async fn update_downloads_series_then_author_items() {
    let database = testutil::database().await;
    insert_series(&database, "100", "系列", &[]).await;
    insert_author(&database, "202534", "Maplestar").await;
    let harness = harness(database.db().clone(), |_| {}).await;
    harness
        .pages
        .html("/watch?v=100", &titled_playlist_page("系列", &[("101", "Series Title")]));
    harness.pages.html("/user/202534/uploaded", &listing(&[("201", "Author Title")]));
    for id in ["101", "201"] {
        harness.pages.html(
            &format!("/download?v={id}"),
            &format!(r#"<a data-url="https://video.example.com/{id}-720p.mp4">dl</a>"#),
        );
    }

    harness.run.update().await.unwrap();

    let streams: Vec<String> = harness.runner.calls().iter().map(|argv| argv.last().unwrap().clone()).collect();
    assert_eq!(
        streams,
        ["https://video.example.com/101-720p.mp4", "https://video.example.com/201-720p.mp4"]
    );
    let ids: Vec<String> = hanime1_rows(&database).await.into_iter().map(|row| row.0).collect();
    assert_eq!(ids, ["101", "201"]);
    assert!(harness.root.join("未分类/系列/Series Title [101].mp4").exists());
    assert!(harness.root.join("未分类/Maplestar/Author Title [201].mp4").exists());
    database.drop_database().await;
}

// py: tests/test_hanime1.py::test_update_survives_author_enumeration_failure
#[tokio::test]
async fn update_survives_author_enumeration_failure() {
    let database = testutil::database().await;
    sqlx::query("DROP TABLE hanime1_author")
        .execute(database.db().pool())
        .await
        .unwrap();
    let harness = harness(database.db().clone(), |_| {}).await;

    harness.run.update().await.unwrap();

    database.drop_database().await;
}

#[test]
fn command_output_is_read_with_universal_newlines() {
    assert_eq!(super::output_text(b"a\r\nb\rc\n\xff"), "a\nb\nc\n\u{fffd}");
}

#[test]
fn strip_uploader_prefix_takes_any_uploader_length() {
    let uploader = "名".repeat(300_000);
    let title = format!("[ {uploader} ]  Episode 1");
    assert_eq!(strip_uploader_prefix(&title, Some(&uploader)), "Episode 1");
    assert_eq!(strip_uploader_prefix("[maplestar] Reze", Some(" MapleStar ")), "Reze");
    assert_eq!(strip_uploader_prefix("[Maple] Reze", Some("MapleStar")), "[Maple] Reze");
    assert_eq!(strip_uploader_prefix("[MapleStar]", Some("MapleStar")), "[MapleStar]");
}
