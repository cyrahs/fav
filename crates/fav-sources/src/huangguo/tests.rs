use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aes::Aes128;
use aes::cipher::{Array, BlockCipherEncrypt as _, KeyInit as _};
use fav_core::settings::{self, Defaults as _};
use fav_core::validate::PyPath;
use fav_net::retry::RecordingSleeper;
use fav_net::subprocess::{BoxFuture as SubprocessFuture, Completed, LineSink, Runner};
use fav_net::{ClientOptions, HttpClient};
use serde_json::{Value as Json, json};

use super::*;
use crate::testkit::RecordingNotifier;
use crate::testutil::{FakeWeb, Reply, Seen, database, test_roots};

const KEY: &[u8; 16] = b"f5d965df75336270";
const IV: &[u8; 16] = b"97b60394abc2fbe1";
const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00poster";
const PROTOCOL_JS: &str = r#";(function (window) {
  'use strict'
  window.CryptoImageProtocol = Object.freeze({"decryptEnabled": true, "decryptKey": "f5d965df75336270", "decryptIV": "97b60394abc2fbe1"})
})(window)"#;

/// AES-128-CBC with PKCS#7, the way the site's CDN stores covers.
fn encrypt(plain: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(&Array::from(*KEY));
    let pad = 16 - plain.len() % 16;
    let mut data = plain.to_vec();
    data.extend(std::iter::repeat_n(u8::try_from(pad).expect("pad"), pad));
    let mut previous = *IV;
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

fn ok(data: &Json) -> Json {
    json!({"data": data, "status": 1, "msg": "", "req_time": 1_795_600_000})
}

/// When the runs happen: 2026-11-26 00:00 in Beijing.
fn now() -> DateTime<Utc> {
    site_time("2026-11-26 00:00:00").expect("time")
}

fn listed(id: i64, title: &str, tags: &[&str], episodes: i64) -> Json {
    listed_at(id, title, tags, episodes, "2026-11-25 14:59:54")
}

fn listed_at(id: i64, title: &str, tags: &[&str], episodes: i64, created_at: &str) -> Json {
    json!({"id": id, "title": title, "tags": tags, "episode_count": episodes, "duration": "300", "is_original": 0, "created_at": created_at})
}

fn series_json(id: i64, title: &str, tags: &[&str], episodes: &[(i64, i64)]) -> Json {
    json!({
        "id": id,
        "title": title,
        "description": "她 & 他 <夜色>\n",
        "cover": format!("https://pic.example.cn/upload/{id}.jpg?auth_key=1"),
        "created_at": "2026-11-25 14:59:54",
        "author": {"id": 1, "name": "黄果官方"},
        "tags": tags,
        "score": 9.5,
        "is_finished": false,
        "duration": "484",
        "episodes": episodes.iter().map(|(ep, status)| json!({"ep_num": ep, "status": status, "duration": 480, "title": format!("第{ep}集")})).collect::<Vec<_>>(),
    })
}

/// The session value the fake site takes as a live login.
const GOOD_SESSION: &str = "good-session";

/// The site as the tests script it: a category listing, the rankings, the favourites of the
/// signed-in account and the series behind them.
#[derive(Default)]
struct Site {
    listing: Mutex<Vec<Json>>,
    rankings: Mutex<HashMap<String, Vec<Json>>>,
    favorites: Mutex<Vec<Json>>,
    series: Mutex<HashMap<i64, Json>>,
    refuse_playback: bool,
}

fn unauthorized() -> Reply {
    Reply::Respond {
        status: 401,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: br#"{"data":[],"status":0,"msg":"unauthorized"}"#.to_vec(),
    }
}

fn signed_in(seen: &Seen) -> bool {
    seen.header("cookie") == Some(&format!("{SESSION_COOKIE}={GOOD_SESSION}"))
}

fn answer(site: &Site, seen: &Seen) -> Reply {
    let path = seen.path();
    if path == FAVORITES_PATH {
        if !signed_in(seen) {
            return unauthorized();
        }
        let items = site.favorites.lock().expect("favorites").clone();
        return Reply::json(200, &ok(&json!({"items": items, "pagination": {"page": 1, "pages": 1}})));
    }
    if path == ACCOUNT_PATH {
        if !signed_in(seen) {
            return unauthorized();
        }
        return Reply::json(200, &ok(&json!({"id": 7, "username": "tester", "nickname": "测试员"})));
    }
    if let Some(kind) = path.strip_prefix("/api/ranks/") {
        return site.rankings.lock().expect("rankings").get(kind).map_or_else(
            || Reply::json(200, &json!({"data": [], "status": 0, "msg": "榜单不存在"})),
            |items| Reply::json(200, &ok(&json!({"items": items, "type": kind}))),
        );
    }
    if path == "/api/site/categories/video" {
        return Reply::json(
            200,
            &ok(&json!({"items": [{"id": 24, "name": "短剧", "route": "ai-duanju"}, {"id": 25, "name": "漫剧", "route": "ai-manju"}]})),
        );
    }
    if path == "/api/videos/category/ai-duanju" {
        return Reply::json(
            200,
            &ok(&json!({"items": *site.listing.lock().expect("listing"), "pagination": {"page": 1, "pages": 1, "size": 50, "total": 0}})),
        );
    }
    if path == IMAGE_PROTOCOL_PATH {
        return Reply::bytes(200, PROTOCOL_JS);
    }
    if path.starts_with("/upload/") {
        return Reply::bytes(200, encrypt(JPEG));
    }
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        ["api", "videos", id] => site
            .series
            .lock()
            .expect("series")
            .get(&id.parse::<i64>().expect("id"))
            .map_or_else(
                || Reply::json(200, &json!({"data": [], "status": 0, "msg": "内容已下线"})),
                |series| Reply::json(200, &ok(series)),
            ),
        ["api", "videos", id, "episodes", ep, "playback"] if seen.method == "POST" => {
            if site.refuse_playback {
                return Reply::json(200, &json!({"data": [], "status": 0, "msg": "视频不存在或不可播放"}));
            }
            Reply::json(
                200,
                &ok(&json!({"mode": 1, "id": id, "ep": ep, "streams": {
                    "main": format!("https://hls.example.cn/videos5/{id}/{ep}.m3u8?auth_key=1"),
                    "hevc": "",
                }})),
            )
        }
        _ => Reply::status(404),
    }
}

#[derive(Debug, Default)]
struct FakeRunner {
    calls: Mutex<Vec<Vec<String>>>,
    fail: bool,
}

impl FakeRunner {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().expect("calls").clone()
    }
}

impl Runner for FakeRunner {
    fn run(&self, argv: Vec<String>) -> SubprocessFuture<'_, std::io::Result<Completed>> {
        Box::pin(async move {
            self.calls.lock().expect("calls").push(argv.clone());
            if self.fail {
                return Ok(Completed {
                    returncode: 1,
                    stdout: String::new(),
                    stderr: "ERROR: HTTP Error 403: Forbidden\n".to_owned(),
                });
            }
            let template = &argv[argv.iter().position(|arg| arg == "-o").expect("-o") + 1];
            std::fs::write(template.replace("%(ext)s", "mp4"), b"video")?;
            Ok(Completed::default())
        })
    }

    fn run_lines<'a>(&'a self, argv: Vec<String>, _sink: LineSink<'a>) -> SubprocessFuture<'a, std::io::Result<i32>> {
        panic!("Huangguo never streams yt-dlp's lines: {argv:?}")
    }
}

struct Harness {
    site: Arc<Site>,
    web: FakeWeb,
    db: fav_db::testdb::TestDatabase,
    root: tempfile::TempDir,
    meta: tempfile::TempDir,
    cache: tempfile::TempDir,
    notifier: Arc<RecordingNotifier>,
    sleeper: RecordingSleeper,
}

fn config(root: &Path) -> settings::Huangguo {
    let mut cfg = settings::Huangguo::defaults();
    cfg.path = PyPath::new(&root.to_string_lossy());
    cfg.categories = vec!["ai-duanju".to_owned()];
    cfg
}

/// A ranking or favourites entry, the way `/api/ranks/<type>` writes one.
fn entry(id: i64, title: &str, tags: &[&str], episodes: i64) -> Json {
    json!({"video_id": id, "title": title, "tags": tags, "episode_count": episodes, "cate_name": "短剧", "rank": 1})
}

impl Harness {
    async fn start(site: Site) -> Self {
        let site = Arc::new(site);
        let shared = Arc::clone(&site);
        let web = FakeWeb::start(move |seen| answer(&shared, seen)).await;
        Self {
            site,
            web,
            db: database().await,
            root: tempfile::tempdir().expect("root"),
            meta: tempfile::tempdir().expect("meta"),
            cache: tempfile::tempdir().expect("cache"),
            notifier: Arc::new(RecordingNotifier::default()),
            sleeper: RecordingSleeper::new(),
        }
    }

    fn client(&self) -> HttpClient {
        HttpClient::new(&ClientOptions {
            proxy: Some(self.web.proxy.clone()),
            roots: test_roots(),
            ..client_options("")
        })
        .expect("client")
    }

    fn run(&self, cfg: settings::Huangguo, runner: &Arc<FakeRunner>) -> Run {
        Run::new(
            cfg,
            self.client(),
            self.client(),
            self.db.db().clone(),
            Arc::clone(&self.notifier) as SharedNotifier,
            Arc::clone(runner) as Arc<dyn Runner>,
            self.sleeper.sleeper(),
            self.cache.path().to_path_buf(),
            now(),
        )
    }

    /// The settings with `metadata_path` pointing at the harness' media-server folder.
    fn config(&self) -> settings::Huangguo {
        let mut cfg = config(self.root.path());
        cfg.metadata_path = self.meta.path().to_string_lossy().into_owned();
        cfg
    }

    async fn sign_in(&self, session: &str) {
        let cookie = StoredCookie {
            name: SESSION_COOKIE.to_owned(),
            value: session.to_owned(),
            domain: "huangguoai.com".to_owned(),
            path: "/".to_owned(),
            expires: None,
        };
        site_sessions::save_signed_in(self.db.db(), SESSION_SITE, &[cookie], "测试员", None, &[])
            .await
            .expect("signed in");
    }

    async fn tiers(&self) -> Vec<(i64, i32, String)> {
        sqlx::query_as("SELECT video_id, ep_num, tier FROM huangguo ORDER BY video_id, ep_num")
            .fetch_all(self.db.db().pool())
            .await
            .expect("tiers")
    }

    fn paths(&self) -> Vec<String> {
        self.web.seen().iter().map(|seen| seen.path().to_owned()).collect()
    }

    async fn episodes(&self) -> Vec<(i64, i32, String)> {
        sqlx::query_as("SELECT video_id, ep_num, local_path FROM huangguo ORDER BY video_id, ep_num")
            .fetch_all(self.db.db().pool())
            .await
            .expect("episodes")
    }

    async fn series_row(&self, id: i64) -> (String, i32, bool, String) {
        sqlx::query_as("SELECT dir_name, episode_count, metadata_written, last_error FROM huangguo_series WHERE video_id = $1")
            .bind(id)
            .fetch_one(self.db.db().pool())
            .await
            .expect("series row")
    }
}

fn two_series_site() -> Site {
    Site {
        listing: Mutex::new(vec![
            listed(101, "巨乳妈妈", &["都市"], 2),
            listed(102, "兄弟情深", &["男同", "都市"], 3),
            listed(103, "长生录: 第一季", &["古风"], 2),
            // Went up long before the backfill window and never followed: not downloaded.
            listed_at(104, "老剧", &["都市"], 9, "2026-08-07 19:16:47"),
        ]),
        series: Mutex::new(HashMap::from([
            (101, series_json(101, "巨乳妈妈", &["都市", "乱伦"], &[(1, 1), (2, 1)])),
            (102, series_json(102, "兄弟情深", &["男同", "都市"], &[(1, 1)])),
            (103, series_json(103, "长生录: 第一季", &["古风"], &[(1, 1), (2, 2)])),
            (104, series_json(104, "老剧", &["都市"], &[(1, 1)])),
            (105, series_json(105, "热播老剧", &["都市"], &[(1, 1)])),
            (106, series_json(106, "收藏的男同剧", &["男同"], &[(1, 1)])),
        ])),
        ..Site::default()
    }
}

// ---------- parsers ----------

#[test]
fn a_listing_page_reads_its_series_and_page_count() {
    let data = json!({
        "items": [
            {"id": 12, "title": " 神瞳觉醒 第一季 ", "tags": ["超能力", ""], "episode_count": 21},
            {"id": "13", "title": "数字也行", "tags": [], "episode_count": "4"},
            {"title": "没有 id"},
        ],
        "pagination": {"page": 1, "pages": 89, "size": 20, "total": 1772},
    });
    let (items, pages) = parse_listing(&data);
    assert_eq!(pages, 89);
    assert_eq!(
        items,
        vec![
            Listed {
                id: 12,
                title: "神瞳觉醒 第一季".to_owned(),
                tags: vec!["超能力".to_owned()],
                episode_count: 21,
                created_at: None,
                category: None,
            },
            Listed {
                id: 13,
                title: "数字也行".to_owned(),
                tags: Vec::new(),
                episode_count: 4,
                created_at: None,
                category: None,
            },
        ]
    );
}

#[test]
fn a_series_reads_its_episodes_in_order() {
    let mut data = series_json(15941, "被同学强奸的巨乳妈妈第二季", &["都市"], &[(2, 1), (1, 1), (3, 0)]);
    data["score"] = json!(9);
    let series = parse_series(&data).expect("series");
    assert_eq!(series.id, 15941);
    assert_eq!(series.author, "黄果官方");
    assert_eq!(series.score, Some(9.0));
    assert_eq!(series.description, "她 & 他 <夜色>");
    assert_eq!(
        series
            .episodes
            .iter()
            .map(|episode| (episode.ep_num, episode.available))
            .collect::<Vec<_>>(),
        vec![(1, true), (2, true), (3, false)]
    );
    assert!(parse_series(&json!({"id": 1, "title": ""})).is_err());
    assert!(parse_series(&json!({"title": "x"})).is_err());
}

#[test]
fn the_stream_is_the_main_one_else_hevc() {
    assert_eq!(
        parse_stream_url(&json!({"streams": {"main": "https://a/x.m3u8", "hevc": "https://a/h.m3u8"}})).as_deref(),
        Some("https://a/x.m3u8")
    );
    assert_eq!(
        parse_stream_url(&json!({"streams": {"main": "", "hevc": "https://a/h.m3u8"}})).as_deref(),
        Some("https://a/h.m3u8")
    );
    assert_eq!(parse_stream_url(&json!({"streams": {"main": ""}})), None);
}

#[test]
fn the_image_protocol_names_the_cover_key() {
    assert_eq!(
        parse_image_protocol(PROTOCOL_JS).expect("key"),
        ImageKey::Aes { key: *KEY, iv: *IV }
    );
    assert_eq!(
        parse_image_protocol(r#"Object.freeze({"decryptEnabled": false})"#).expect("plain"),
        ImageKey::Plain
    );
    assert!(parse_image_protocol(r#"{"decryptEnabled": true, "decryptKey": "short", "decryptIV": "97b60394abc2fbe1"}"#).is_err());
    assert!(parse_image_protocol("nothing here").is_err());
}

#[test]
fn covers_are_decrypted_and_plain_images_kept() {
    let key = ImageKey::Aes { key: *KEY, iv: *IV };
    assert_eq!(decode_cover(&encrypt(JPEG), &key), Ok((JPEG.to_vec(), "jpg")));
    assert_eq!(decode_cover(JPEG, &ImageKey::Plain), Ok((JPEG.to_vec(), "jpg")));
    assert!(decode_cover(&encrypt(b"not an image"), &key).is_err());
    assert!(decode_cover(b"0123456789", &key).is_err());
    assert!(decode_cover(&encrypt(JPEG), &ImageKey::Plain).is_err());
}

#[test]
fn names_follow_emby() {
    assert_eq!(dir_name_for("长生录: 第一季", 1), "长生录_ 第一季");
    assert_eq!(dir_name_for(" ... ", 7), "id-7");
    assert_eq!(category_dir_name(""), "未分类");
    assert_eq!(episode_file_name("长生录", 3, "mp4"), "长生录 S01E03.mp4");
    assert_eq!(episode_file_name("长生录", 120, "mp4"), "长生录 S01E120.mp4");
    assert_eq!(
        excluded_tag(&["都市".to_owned(), "男同".to_owned()], &["男同".to_owned()]),
        Some("男同")
    );
    assert_eq!(excluded_tag(&["都市".to_owned()], &["男同".to_owned()]), None);
}

#[test]
fn the_nfo_is_escaped_and_complete() {
    let series = parse_series(&series_json(5, "A&B <1>", &["都市", "乱伦"], &[(1, 1)])).expect("series");
    let nfo = tvshow_nfo(&series, "短剧");
    assert_eq!(
        nfo,
        "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<tvshow>\n  <title>A&amp;B &lt;1&gt;</title>\n  \
         <plot>她 &amp; 他 &lt;夜色&gt;</plot>\n  <premiered>2026-11-25</premiered>\n  <year>2026</year>\n  \
         <studio>黄果官方</studio>\n  <rating>9.5</rating>\n  <status>Continuing</status>\n  <genre>都市</genre>\n  \
         <genre>乱伦</genre>\n  <tag>短剧</tag>\n  <uniqueid type=\"huangguo\" default=\"true\">5</uniqueid>\n</tvshow>\n"
    );
}

#[test]
fn yt_dlp_goes_through_the_media_proxy_only_when_set() {
    let command = build_download_command(
        "https://a/x.m3u8",
        "/tmp/1-1.%(ext)s",
        "https://huangguoai.com/",
        "http://us.proxy.svc",
    );
    assert_eq!(&command[..3], ["yt-dlp", "-o", "/tmp/1-1.%(ext)s"]);
    assert!(command.windows(2).any(|pair| pair == ["--proxy", "http://us.proxy.svc"]));
    assert!(command.windows(2).any(|pair| pair == ["--referer", "https://huangguoai.com/"]));
    assert_eq!(command.last().map(String::as_str), Some("https://a/x.m3u8"));
    let direct = build_download_command("https://a/x.m3u8", "/tmp/1-1.%(ext)s", "https://huangguoai.com/", "");
    assert!(!direct.iter().any(|arg| arg == "--proxy"));
}

#[test]
fn the_settings_default_to_the_two_live_categories_without_gay_series() {
    let cfg = settings::Huangguo::defaults();
    assert_eq!(cfg.categories, ["ai-duanju", "ai-manju"]);
    assert_eq!(cfg.excluded_tags, ["男同"]);
    assert!(cfg.proxy_media);
    assert_eq!(cfg.max_episodes_per_run, 100);
    assert_eq!(cfg.backfill_days, 3);
    assert!(cfg.validate_runnable().is_empty());
    let mut empty = cfg.clone();
    empty.categories.clear();
    assert_eq!(empty.validate_runnable(), ["categories"]);
}

// ---------- runs ----------

#[tokio::test(flavor = "multi_thread")]
async fn a_run_saves_every_available_episode_for_emby_and_skips_excluded_tags() {
    let harness = Harness::start(two_series_site()).await;
    let runner = Arc::new(FakeRunner::default());
    let mut cfg = harness.config();
    cfg.proxy = "http://us.proxy.svc.cluster.local".to_owned();
    harness.run(cfg.clone(), &runner).update().await.expect("run");

    // Episodes go to the upload folder; the nfo and the poster to the media server's folder.
    let show = harness.root.path().join("短剧").join("巨乳妈妈");
    assert_eq!(std::fs::read(show.join("巨乳妈妈 S01E01.mp4")).expect("E01"), b"video");
    assert!(show.join("巨乳妈妈 S01E02.mp4").is_file());
    assert!(!show.join("poster.jpg").exists() && !show.join("tvshow.nfo").exists());
    let meta = harness.meta.path().join("短剧").join("巨乳妈妈");
    assert_eq!(std::fs::read(meta.join("poster.jpg")).expect("poster"), JPEG);
    let nfo = std::fs::read_to_string(meta.join("tvshow.nfo")).expect("nfo");
    assert!(nfo.contains("<title>巨乳妈妈</title>"), "{nfo}");
    assert!(!meta.join("巨乳妈妈 S01E01.mp4").exists());
    let other = harness.root.path().join("短剧").join("长生录_ 第一季");
    assert!(other.join("长生录_ 第一季 S01E01.mp4").is_file());
    assert!(!other.join("长生录_ 第一季 S01E02.mp4").exists(), "episode 2 is not playable");
    assert!(!harness.root.path().join("短剧").join("兄弟情深").exists());
    assert!(
        !harness.paths().iter().any(|path| path == "/api/videos/102"),
        "{:?}",
        harness.paths()
    );
    assert!(
        !harness.paths().iter().any(|path| path == "/api/videos/104"),
        "the back catalogue is never read"
    );
    assert!(!harness.root.path().join("短剧").join("老剧").exists());

    let episodes = harness.episodes().await;
    assert_eq!(
        episodes.iter().map(|(id, ep, _)| (*id, *ep)).collect::<Vec<_>>(),
        vec![(101, 1), (101, 2), (103, 1)]
    );
    assert!(episodes[0].2.ends_with("短剧/巨乳妈妈/巨乳妈妈 S01E01.mp4"));
    assert_eq!(harness.series_row(101).await, ("巨乳妈妈".to_owned(), 2, true, String::new()));
    assert_eq!(
        harness.series_row(103).await.1,
        2,
        "an unplayable episode does not keep the series open"
    );

    let calls = runner.calls();
    assert_eq!(calls.len(), 3);
    assert!(
        calls[0]
            .windows(2)
            .any(|pair| pair == ["--proxy", "http://us.proxy.svc.cluster.local"])
    );
    assert_eq!(
        calls[0].last().map(String::as_str),
        Some("https://hls.example.cn/videos5/101/1.m3u8?auth_key=1")
    );

    let sent = harness.notifier.sent();
    assert_eq!(
        sent.iter()
            .map(|sent| (sent.title.as_str(), sent.body.as_str()))
            .collect::<Vec<_>>(),
        vec![("巨乳妈妈", "短剧 | E01–E02 (2)"), ("长生录: 第一季", "短剧 | E01")]
    );
    assert_eq!(sent[0].link_url, "https://huangguoai.com/video/101/");
    assert_eq!(sent[0].header, "Huangguo");
    assert!(harness.sleeper.delays().iter().all(|delay| (*delay - 1.0).abs() < f64::EPSILON));

    // Nothing changed on the site: the next run only lists.
    let seen_before = harness.web.seen().len();
    harness.run(cfg, &runner).update().await.expect("second run");
    let later: Vec<String> = harness.web.seen()[seen_before..]
        .iter()
        .map(|seen| seen.path().to_owned())
        .collect();
    // Not signed in, so no favourites; the rankings this site has none of.
    assert_eq!(
        later,
        [
            "/api/ranks/hot",
            "/api/ranks/recommend",
            "/api/ranks/potential",
            "/api/site/categories/video",
            "/api/videos/category/ai-duanju"
        ]
    );
    assert_eq!(runner.calls().len(), 3);
    assert!(harness.web.seen().iter().all(|seen| seen.header("cookie").is_none()));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_episode_budget_carries_the_rest_over_to_the_next_run() {
    let harness = Harness::start(two_series_site()).await;
    let runner = Arc::new(FakeRunner::default());
    let mut cfg = config(harness.root.path());
    cfg.max_episodes_per_run = 1;
    harness.run(cfg.clone(), &runner).update().await.expect("first run");
    assert_eq!(harness.episodes().await.len(), 1);
    assert_eq!(harness.series_row(101).await.1, 0, "the series is not complete yet");

    harness.run(cfg.clone(), &runner).update().await.expect("second run");
    harness.run(cfg, &runner).update().await.expect("third run");
    assert_eq!(
        harness.episodes().await.iter().map(|(id, ep, _)| (*id, *ep)).collect::<Vec<_>>(),
        vec![(101, 1), (101, 2), (103, 1)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_saves_nothing_fails_and_remembers_why() {
    let harness = Harness::start(two_series_site()).await;
    let runner = Arc::new(FakeRunner {
        fail: true,
        ..FakeRunner::default()
    });
    let mut cfg = config(harness.root.path());
    cfg.proxy_media = false;
    cfg.proxy = "http://us.proxy.svc.cluster.local".to_owned();
    let error = harness.run(cfg, &runner).update().await.expect_err("fails");
    assert_eq!(error.class_name(), "DownloadError");
    assert!(error.message().contains("HTTP Error 403"), "{}", error.message());
    // Three attempts per episode, a fresh stream each time, no proxy for media.
    assert_eq!(runner.calls().len(), 9);
    assert!(runner.calls().iter().all(|call| !call.iter().any(|arg| arg == "--proxy")));
    let (_, count, _, last_error) = harness.series_row(101).await;
    assert_eq!(count, 0);
    assert!(last_error.contains("HTTP Error 403"), "{last_error}");
    assert!(harness.notifier.sent().is_empty());
    assert!(harness.episodes().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_playback_fails_the_run_when_nothing_is_saved() {
    let mut site = two_series_site();
    site.refuse_playback = true;
    let harness = Harness::start(site).await;
    let runner = Arc::new(FakeRunner::default());
    let error = harness
        .run(config(harness.root.path()), &runner)
        .update()
        .await
        .expect_err("nothing saved");
    assert!(error.message().contains("视频不存在或不可播放"), "{}", error.message());
    assert!(runner.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn two_series_with_one_title_get_two_directories() {
    let site = Site {
        listing: Mutex::new(vec![listed(201, "同名", &[], 1), listed(202, "同名", &[], 1)]),
        series: Mutex::new(HashMap::from([
            (201, series_json(201, "同名", &[], &[(1, 1)])),
            (202, series_json(202, "同名", &[], &[(1, 1)])),
        ])),
        ..Site::default()
    };
    let harness = Harness::start(site).await;
    let runner = Arc::new(FakeRunner::default());
    harness.run(config(harness.root.path()), &runner).update().await.expect("run");
    let category = harness.root.path().join("短剧");
    assert!(category.join("同名").join("同名 S01E01.mp4").is_file());
    assert!(category.join("同名 [202]").join("同名 [202] S01E01.mp4").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_followed_series_keeps_its_new_episodes_after_it_leaves_the_window() {
    let harness = Harness::start(two_series_site()).await;
    let runner = Arc::new(FakeRunner::default());
    let cfg = config(harness.root.path());
    harness.run(cfg.clone(), &runner).update().await.expect("first run");

    // A month later 巨乳妈妈 is long out of the window, but gets a third episode.
    {
        let mut listing = harness.site.listing.lock().expect("listing");
        listing[0] = listed(101, "巨乳妈妈", &["都市"], 3);
        let mut series = harness.site.series.lock().expect("series");
        series.insert(101, series_json(101, "巨乳妈妈", &["都市", "乱伦"], &[(1, 1), (2, 1), (3, 1)]));
    }
    let mut later = harness.run(cfg, &runner);
    later.now = now() + TimeDelta::days(30);
    later.update().await.expect("later run");
    assert!(
        harness
            .root
            .path()
            .join("短剧")
            .join("巨乳妈妈")
            .join("巨乳妈妈 S01E03.mp4")
            .is_file()
    );
    assert_eq!(
        harness.episodes().await.iter().map(|(id, ep, _)| (*id, *ep)).collect::<Vec<_>>(),
        vec![(101, 1), (101, 2), (101, 3), (103, 1)]
    );
}

#[test]
fn site_times_are_beijing_time() {
    assert_eq!(
        site_time("2026-11-25 14:59:54").map(|time| time.to_rfc3339()).as_deref(),
        Some("2026-11-25T06:59:54+00:00")
    );
    assert_eq!(site_time("yesterday"), None);
}

// ---------- favourites, rankings and the cache limits ----------

#[tokio::test(flavor = "multi_thread")]
async fn favourites_then_rankings_then_new_series_and_only_the_list_carries_the_login() {
    let site = two_series_site();
    // Both went up months ago: only a list the user cares about brings them in.
    *site.favorites.lock().expect("favorites") = vec![entry(104, "老剧", &["都市"], 1), entry(106, "收藏的男同剧", &["男同"], 1)];
    site.rankings.lock().expect("rankings").insert(
        "hot".to_owned(),
        vec![entry(105, "热播老剧", &["都市"], 1), entry(102, "兄弟情深", &["男同"], 3)],
    );
    let harness = Harness::start(site).await;
    harness.sign_in(GOOD_SESSION).await;
    let runner = Arc::new(FakeRunner::default());
    harness.run(harness.config(), &runner).update().await.expect("run");

    let streams: Vec<String> = runner.calls().iter().filter_map(|call| call.last().cloned()).collect();
    assert_eq!(
        streams
            .iter()
            .map(|url| url.split('/').nth(4).unwrap_or_default())
            .collect::<Vec<_>>(),
        ["104", "106", "105", "101", "101", "103"],
        "favourites first (a favourite is never excluded by tag), then the hot ranking, then new series"
    );
    assert_eq!(
        harness.tiers().await,
        vec![
            (101, 1, "new".to_owned()),
            (101, 2, "new".to_owned()),
            (103, 1, "new".to_owned()),
            (104, 1, "favorite".to_owned()),
            (105, 1, "ranking".to_owned()),
            (106, 1, "favorite".to_owned()),
        ]
    );
    assert!(
        !harness.paths().iter().any(|path| path == "/api/videos/102"),
        "excluded in a ranking"
    );
    let with_cookie: Vec<String> = harness
        .web
        .seen()
        .iter()
        .filter(|seen| seen.header("cookie").is_some())
        .map(|seen| seen.path().to_owned())
        .collect();
    assert_eq!(with_cookie, [FAVORITES_PATH], "only the favourites list is asked with the login");
    for call in runner.calls() {
        assert!(
            !call.iter().any(|arg| arg.contains(GOOD_SESSION)),
            "yt-dlp never gets the login: {call:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_login_skips_the_favourites_and_marks_the_session() {
    let site = two_series_site();
    *site.favorites.lock().expect("favorites") = vec![entry(104, "老剧", &["都市"], 1)];
    let harness = Harness::start(site).await;
    harness.sign_in("expired").await;
    let runner = Arc::new(FakeRunner::default());
    harness.run(harness.config(), &runner).update().await.expect("run");
    assert!(!harness.tiers().await.iter().any(|(id, _, _)| *id == 104));
    assert_eq!(harness.tiers().await.len(), 3, "the new series still come down");
    let stored = site_sessions::load(harness.db.db(), SESSION_SITE)
        .await
        .expect("load")
        .expect("session");
    assert_eq!(stored.status, "invalid");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_cache_holds_its_pool_back_until_the_upload_takes_files_away() {
    let site = two_series_site();
    *site.favorites.lock().expect("favorites") = vec![entry(104, "老剧", &["都市"], 1)];
    let harness = Harness::start(site).await;
    harness.sign_in(GOOD_SESSION).await;
    // 2 GiB of earlier downloads still wait for the upload, over a 1 GiB limit.
    let waiting = harness.root.path().join("waiting.mp4");
    std::fs::write(&waiting, b"x").expect("file");
    sqlx::query("INSERT INTO huangguo (video_id, ep_num, local_path, tier, size) VALUES (900, 1, $1, 'new', $2)")
        .bind(waiting.to_string_lossy().into_owned())
        .bind(2_i64 << 30)
        .execute(harness.db.db().pool())
        .await
        .expect("seed");
    let runner = Arc::new(FakeRunner::default());
    let mut cfg = harness.config();
    cfg.cache_limit_gb = 1;
    cfg.favorites_cache_limit_gb = 1;
    harness.run(cfg.clone(), &runner).update().await.expect("run");
    assert_eq!(
        harness
            .tiers()
            .await
            .iter()
            .filter(|(id, _, _)| *id != 900)
            .cloned()
            .collect::<Vec<_>>(),
        vec![(104, 1, "favorite".to_owned())],
        "favourites have their own limit; rankings and new series wait"
    );

    // The upload took the file away: it stops counting, and the rest comes down.
    std::fs::remove_file(&waiting).expect("uploaded");
    harness.run(cfg, &runner).update().await.expect("next run");
    assert_eq!(harness.tiers().await.len(), 5);
    let on_disk: bool = sqlx::query_scalar("SELECT on_disk FROM huangguo WHERE video_id = 900")
        .fetch_one(harness.db.db().pool())
        .await
        .expect("row");
    assert!(!on_disk);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_series_moves_up_to_favourite_and_never_back() {
    let harness = Harness::start(two_series_site()).await;
    harness.sign_in(GOOD_SESSION).await;
    let runner = Arc::new(FakeRunner::default());
    harness.run(harness.config(), &runner).update().await.expect("first run");
    {
        *harness.site.favorites.lock().expect("favorites") = vec![entry(101, "巨乳妈妈", &["都市"], 3)];
        harness
            .site
            .series
            .lock()
            .expect("series")
            .insert(101, series_json(101, "巨乳妈妈", &["都市", "乱伦"], &[(1, 1), (2, 1), (3, 1)]));
    }
    harness.run(harness.config(), &runner).update().await.expect("second run");
    harness.site.favorites.lock().expect("favorites").clear();
    harness.run(harness.config(), &runner).update().await.expect("third run");
    let tier: String = sqlx::query_scalar("SELECT tier FROM huangguo_series WHERE video_id = 101")
        .fetch_one(harness.db.db().pool())
        .await
        .expect("tier");
    assert_eq!(tier, "favorite");
    assert!(harness.tiers().await.contains(&(101, 3, "favorite".to_owned())));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_metadata_path_no_metadata_is_written() {
    let harness = Harness::start(two_series_site()).await;
    let runner = Arc::new(FakeRunner::default());
    harness.run(config(harness.root.path()), &runner).update().await.expect("run");
    assert!(!harness.paths().iter().any(|path| path == IMAGE_PROTOCOL_PATH));
    assert!(std::fs::read_dir(harness.meta.path()).expect("meta").next().is_none());
    assert!(!harness.root.path().join("短剧").join("巨乳妈妈").join("tvshow.nfo").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_login_page_checks_the_session_with_the_account_endpoint() {
    let web = FakeWeb::start(|seen| {
        if seen.path() == ACCOUNT_PATH && signed_in(seen) {
            Reply::json(200, &ok(&json!({"id": 7, "username": "tester", "nickname": "测试员"})))
        } else {
            unauthorized()
        }
    })
    .await;
    let cookie = |value: &str| StoredCookie {
        name: SESSION_COOKIE.to_owned(),
        value: value.to_owned(),
        domain: "huangguoai.com".to_owned(),
        path: "/".to_owned(),
        expires: None,
    };
    let host = "http://huangguoai.test";
    let live = verify_session_at(host, &[cookie(GOOD_SESSION)], &web.proxy).await;
    assert_eq!((live.ok, live.account.as_str()), (true, "测试员"));
    let stale = verify_session_at(host, &[cookie("expired")], &web.proxy).await;
    assert!(!stale.ok && stale.message.contains("401"), "{stale:?}");
    let before = web.seen().len();
    let none = verify_session_at(host, &[], &web.proxy).await;
    assert!(!none.ok);
    assert_eq!(web.seen().len(), before, "no request without a session cookie");
}

#[test]
fn rankings_and_favourites_read_their_entries() {
    let ranking = parse_ranking(&json!({"items": [
        {"video_id": 117, "title": "高三爱情故事", "tags": ["校园"], "episode_count": 48, "cate_name": "短剧", "rank": 1},
        {"title": "no id"},
    ], "type": "hot"}));
    assert_eq!(ranking.len(), 1);
    assert_eq!(
        (ranking[0].id, ranking[0].episode_count, ranking[0].category.as_deref()),
        (117, 48, Some("短剧"))
    );
    let (favorites, pages) =
        parse_favorites(&json!({"items": [{"id": 9, "video": {"id": 12, "title": "神瞳觉醒"}}], "pagination": {"pages": 2}}));
    assert_eq!((favorites[0].id, favorites[0].title.as_str(), pages), (12, "神瞳觉醒", 2));
    let (bare, pages) = parse_favorites(&json!([{"id": 5, "title": "x"}]));
    assert_eq!((bare[0].id, pages), (5, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_in_house_series_goes_into_the_folder_of_its_type() {
    let site = two_series_site();
    let mut original = series_json(117, "高三爱情故事", &["校园"], &[(1, 1)]);
    original["breadcrumb"] = json!([{"name": "黄果原创", "url": "/original/"}]);
    original["tracking"] = json!({"drama_type_id": "25", "drama_type_name": "AI成人漫剧"});
    site.series.lock().expect("series").insert(117, original);
    site.rankings
        .lock()
        .expect("rankings")
        .insert("hot".to_owned(), vec![entry(117, "高三爱情故事", &["校园"], 1)]);
    let harness = Harness::start(site).await;
    let runner = Arc::new(FakeRunner::default());
    harness.run(harness.config(), &runner).update().await.expect("run");
    let folder = harness.root.path().join("漫剧").join("高三爱情故事");
    assert!(
        folder.join("高三爱情故事 S01E01.mp4").is_file(),
        "the type id wins over the ranking's 短剧 and the breadcrumb"
    );
    assert!(harness.meta.path().join("漫剧").join("高三爱情故事").join("tvshow.nfo").is_file());
    assert!(!harness.root.path().join("黄果原创").exists());
}
