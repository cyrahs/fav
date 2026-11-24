use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use fav_core::settings::{Schedule, Zhihu, ZhihuCollection, ZhihuCollectionOwner};
use fav_core::validate::PyPath;
use fav_db::Db;
use fav_net::retry::RecordingSleeper;
use fav_runtime::BoxFuture;
use serde_json::{Value as Json, json};
use sqlx::Row;

use super::{
    CollectionInfo, MAX_ATTEMPTS, ZhihuApiOpener, ZhihuApiSession, ZhihuCollections, api_answer, list_collections, listed, read_collections,
};
use crate::post_links::{Endpoints, PostClient};
use crate::saved_links::{self, NewSavedLink};
use crate::testkit::RecordingNotifier;
use crate::testutil::{FakeWeb, Reply, database, sign_in_to_zhihu};
use crate::zhihu_post::ZhihuError;

/// Zhihu's API as scripted: an answer per path, and every path asked.
#[derive(Default)]
struct FakeApi {
    pages: Mutex<HashMap<String, Result<Json, ZhihuError>>>,
    calls: Mutex<Vec<String>>,
    opened: Mutex<usize>,
    closed: Mutex<usize>,
}

impl FakeApi {
    fn answer(&self, path: &str, page: Json) {
        self.pages.lock().unwrap().insert(path.to_owned(), Ok(page));
    }

    fn fail(&self, path: &str, error: ZhihuError) {
        self.pages.lock().unwrap().insert(path.to_owned(), Err(error));
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

struct FakeSession(Arc<FakeApi>);

impl ZhihuApiSession for FakeSession {
    fn get<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<fav_core::pyjson::Dict, ZhihuError>> {
        Box::pin(async move {
            self.0.calls.lock().unwrap().push(path.to_owned());
            let answer = self.0.pages.lock().unwrap().get(path).cloned();
            match answer {
                Some(Ok(page)) => api_answer(path, fav_browser::zhihu::ApiAnswer::Json { status: 200, body: page }),
                Some(Err(error)) => Err(error),
                None => api_answer(
                    path,
                    fav_browser::zhihu::ApiAnswer::Json {
                        status: 404,
                        body: json!({"error": {"message": "资源不存在"}}),
                    },
                ),
            }
        })
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        *self.0.closed.lock().unwrap() += 1;
        Box::pin(async {})
    }
}

struct FakeOpener(Arc<FakeApi>);

impl ZhihuApiOpener for FakeOpener {
    fn open(&self) -> BoxFuture<'_, Result<Box<dyn ZhihuApiSession>, ZhihuError>> {
        *self.0.opened.lock().unwrap() += 1;
        Box::pin(async move { Ok(Box::new(FakeSession(Arc::clone(&self.0))) as Box<dyn ZhihuApiSession>) })
    }
}

fn items_path(id: i64, offset: usize) -> String {
    format!("/api/v4/collections/{id}/items?offset={offset}&limit=20")
}

fn page(items: Vec<Json>, is_end: bool) -> Json {
    json!({"paging": {"is_end": is_end, "totals": items.len()}, "data": items.into_iter().map(|content| json!({"content": content, "created": "2026-11-01T00:00:00+08:00"})).collect::<Vec<_>>()})
}

fn answer(id: u64, images: &[&str]) -> Json {
    let html: String = images.iter().fold(String::new(), |html, url| {
        html + &format!("<figure><img src=\"data:x\" data-original=\"{url}\"></figure>")
    });
    json!({
        "type": "answer", "id": id, "url": format!("https://www.zhihu.com/question/1/answer/{id}"),
        "created_time": 1_790_000_000, "author": {"name": "答主"}, "question": {"title": format!("问题 {id}"), "id": "1"},
        "content": format!("<p>text</p>{html}"),
    })
}

fn pin(id: u64, image: &str) -> Json {
    json!({
        "type": "pin", "id": id.to_string(), "url": format!("https://www.zhihu.com/pin/{id}?native=0"), "created": 1_790_000_000,
        "author": {"name": "某人"}, "excerpt_title": "一条想法",
        "content": [{"type": "text", "content": "<p>一条想法</p>"}, {"type": "image", "url": image, "original_url": image}],
    })
}

fn zvideo(id: u64) -> Json {
    json!({
        "type": "zvideo", "id": id.to_string(), "title": "一个视频", "author": {"name": "UP"},
        "url": format!("https://www.zhihu.com/zvideo/{id}"),
        "video": {"video_id": "", "thumbnail": "http://pic1.zhimg.com/cover.jpg", "playlist": {
            "fhd": {"play_url": "http://vdn3.vzuu.com/FHD/v.mp4?auth_key=1", "width": 1920, "height": 1080, "bitrate": 1156},
            "sd": {"play_url": "http://vdn3.vzuu.com/SD/v.mp4?auth_key=1", "width": 640, "height": 360, "bitrate": 300},
        }},
    })
}

fn media_web() -> impl Fn(&crate::testutil::Seen) -> Reply + Send + Sync + 'static {
    |seen| {
        if seen.url.contains("broken") {
            Reply::status(500)
        } else if seen.url.contains("vzuu.com") {
            Reply::Respond {
                status: 200,
                headers: vec![("content-type".to_owned(), "video/mp4".to_owned())],
                body: b"\x00\x00\x00\x18ftypmp42video".to_vec(),
            }
        } else {
            Reply::Respond {
                status: 200,
                headers: vec![("content-type".to_owned(), "image/jpeg".to_owned())],
                body: b"\xff\xd8\xffjpeg".to_vec(),
            }
        }
    }
}

fn client(web: &FakeWeb) -> PostClient {
    let endpoints = Endpoints {
        lens: "http://lens.zhihu.com/api/v4/videos/".to_owned(),
        ..Endpoints::default()
    };
    PostClient::with(Some(&web.proxy), endpoints).expect("client")
}

fn config(folder: &std::path::Path, ids: &[i64], max_posts_per_run: i64) -> Zhihu {
    Zhihu {
        schedule: Schedule {
            cron: "40 */3 * * *".to_owned(),
            enabled: true,
            notify: true,
        },
        collections: ids
            .iter()
            .map(|id| ZhihuCollection {
                id: *id,
                title: format!("收藏 {id}"),
                owner: ZhihuCollectionOwner::Mine,
                path: PyPath::new(&folder.join(id.to_string()).to_string_lossy()),
            })
            .collect(),
        max_posts_per_run,
    }
}

struct Run {
    db: Db,
    api: Arc<FakeApi>,
    notifier: Arc<RecordingNotifier>,
    sleeper: RecordingSleeper,
    web: FakeWeb,
}

impl Run {
    async fn new(db: Db) -> Self {
        Self {
            db,
            api: Arc::new(FakeApi::default()),
            notifier: Arc::new(RecordingNotifier::default()),
            sleeper: RecordingSleeper::new(),
            web: FakeWeb::start(media_web()).await,
        }
    }

    async fn run(&self, config: Zhihu) -> Result<(), fav_runtime::source::JobError> {
        let mut source = ZhihuCollections::new(
            self.db.clone(),
            self.notifier.clone(),
            Arc::new(FakeOpener(Arc::clone(&self.api))),
            client(&self.web),
            config,
        )
        .with_sleeper(self.sleeper.sleeper());
        source.update().await
    }
}

async fn statuses(db: &Db) -> Vec<(String, String, i32)> {
    sqlx::query("SELECT url, status, attempts FROM zhihu_collection_item ORDER BY url")
        .fetch_all(db.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get("url"), row.get("status"), row.get("attempts")))
        .collect()
}

#[tokio::test]
async fn the_account_lists_its_own_collections_then_the_followed_ones() {
    let api = Arc::new(FakeApi::default());
    api.answer("/api/v4/me", json!({"name": "某人", "url_token": "some-one"}));
    api.answer(
        "/api/v4/people/some-one/collections?offset=0&limit=20",
        json!({"paging": {"is_end": false}, "data": [
            {"id": 11, "title": "fav", "item_count": 5, "is_public": false, "creator": {"name": "某人"}},
        ]}),
    );
    api.answer(
        "/api/v4/people/some-one/collections?offset=20&limit=20",
        json!({"paging": {"is_end": true}, "data": [{"id": "12", "title": " 杂 ", "item_count": 56, "is_public": true}]}),
    );
    api.answer(
        "/api/v4/members/some-one/following-favlists?offset=0&limit=20",
        json!({"paging": {"is_end": true}, "data": [{"id": 22, "title": "别人的", "is_public": true}, {"id": 11, "title": "dup"}, {"title": "no id"}]}),
    );

    let lists = read_collections(&FakeOpener(Arc::clone(&api))).await.expect("listed");

    assert_eq!(lists.account, "某人 (some-one)");
    let summary: Vec<(i64, &str, ZhihuCollectionOwner, Option<i64>, bool)> = lists
        .collections
        .iter()
        .map(|info: &CollectionInfo| (info.id, info.title.as_str(), info.owner, info.item_count, info.is_public))
        .collect();
    assert_eq!(
        summary,
        [
            (11, "fav", ZhihuCollectionOwner::Mine, Some(5), false),
            (12, "杂", ZhihuCollectionOwner::Mine, Some(56), true),
            (22, "别人的", ZhihuCollectionOwner::Followed, None, true),
        ]
    );
    assert_eq!(*api.closed.lock().unwrap(), 1);
}

#[tokio::test]
async fn a_signed_out_page_is_a_login_to_redo() {
    let api = Arc::new(FakeApi::default());
    api.answer("/api/v4/me", json!({"name": "", "url_token": ""}));
    let session = FakeSession(Arc::clone(&api));
    assert!(matches!(list_collections(&session).await, Err(ZhihuError::Login(_))));
    assert!(matches!(
        api_answer("/api/v4/me", fav_browser::zhihu::ApiAnswer::SignedOut),
        Err(ZhihuError::Login(_))
    ));
    assert!(matches!(
        api_answer(
            "/x",
            fav_browser::zhihu::ApiAnswer::Json {
                status: 403,
                body: json!({})
            }
        ),
        Err(ZhihuError::Outage(_))
    ));
}

#[test]
fn a_listed_post_is_read_as_a_shared_link_would_be() {
    let value = |json: Json| fav_core::pyjson::Value::from_serde(&json!({"content": json}));
    let item = listed(&value(answer(5, &["http://pic1.zhimg.com/v2-a_r.jpg"]))).unwrap().unwrap();
    assert_eq!(item.url, "https://www.zhihu.com/answer/5");
    assert_eq!(item.title, "问题 5");
    assert_eq!(item.author, "答主");
    assert!(item.has_media());

    let item = listed(&value(pin(7, "http://pic1.zhimg.com/p.jpg"))).unwrap().unwrap();
    assert_eq!(item.url, "https://www.zhihu.com/pin/7");
    assert_eq!(item.title, "一条想法");

    let item = listed(&value(zvideo(9))).unwrap().unwrap();
    assert_eq!(item.url, "https://www.zhihu.com/zvideo/9");
    assert_eq!(item.title, "一个视频");
    assert!(item.has_media());

    let text_only = listed(&value(answer(6, &[]))).unwrap().unwrap();
    assert!(!text_only.has_media());

    let other = listed(&value(
        json!({"type": "question", "id": 3, "url": "https://www.zhihu.com/question/3", "title": "问"}),
    ))
    .unwrap()
    .unwrap();
    assert!(other.media.is_none());
    assert!(listed(&value(json!({"type": "answer"}))).unwrap().is_none());
}

#[tokio::test]
async fn a_run_saves_the_posts_with_media_and_settles_the_rest() {
    let database = database().await;
    let db = database.db().clone();
    sign_in_to_zhihu(&db).await;
    let run = Run::new(db.clone()).await;
    let folder = tempfile::tempdir().unwrap();
    run.api.answer(
        &items_path(11, 0),
        page(
            vec![
                answer(1, &["http://pic1.zhimg.com/v2-a_r.jpg", "http://pic2.zhimg.com/v2-b_r.jpg"]),
                answer(2, &[]),
                json!({"type": "question", "id": 3, "url": "https://www.zhihu.com/question/3", "title": "问"}),
            ],
            false,
        ),
    );
    run.api.answer(
        &items_path(11, 20),
        page(vec![pin(4, "http://pic1.zhimg.com/p.jpg"), zvideo(5)], true),
    );

    run.run(config(folder.path(), &[11], 100)).await.expect("run");

    assert_eq!(
        statuses(&db).await,
        [
            ("https://www.zhihu.com/answer/1".to_owned(), "saved".to_owned(), 0),
            ("https://www.zhihu.com/answer/2".to_owned(), "no_media".to_owned(), 0),
            ("https://www.zhihu.com/pin/4".to_owned(), "saved".to_owned(), 0),
            ("https://www.zhihu.com/question/3".to_owned(), "unsupported".to_owned(), 0),
            ("https://www.zhihu.com/zvideo/5".to_owned(), "saved".to_owned(), 0),
        ]
    );
    let mut files: Vec<String> = std::fs::read_dir(folder.path().join("11"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(files.len(), 4, "{files:?}");
    let extension = |name: &String| Path::new(name).extension().map(|ext| ext.to_string_lossy().into_owned());
    assert!(files.iter().any(|name| extension(name).as_deref() == Some("mp4")), "{files:?}");
    assert!(!files.iter().any(|name| extension(name).as_deref() == Some("part")), "{files:?}");
    // The widest of the zvideo's encodings.
    assert!(run.web.urls().iter().any(|url| url.contains("/FHD/")), "{:?}", run.web.urls());

    let saved = saved_links::find(&db, "https://www.zhihu.com/answer/1")
        .await
        .unwrap()
        .expect("recorded");
    assert_eq!(saved.source, "zhihu");
    assert_eq!(saved.item, "收藏夹 收藏 11");
    let sent = run.notifier.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].kind, "download_completed");
    assert_eq!(sent[0].title, "收藏夹「收藏 11」");
    assert_eq!(sent[0].link_url, "https://www.zhihu.com/collection/11");
    assert!(sent[0].body.starts_with("Saved 3 posts | 3 images, 1 videos"), "{}", sent[0].body);
    assert_eq!(run.sleeper.delays(), [1.0]);
    let complete: bool = sqlx::query_scalar("SELECT complete FROM zhihu_collection WHERE id = 11")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(complete);
    assert_eq!(*run.api.closed.lock().unwrap(), 1);
}

#[tokio::test]
async fn a_walked_collection_stops_at_the_first_page_it_knows() {
    let database = database().await;
    let db = database.db().clone();
    sign_in_to_zhihu(&db).await;
    let run = Run::new(db.clone()).await;
    let folder = tempfile::tempdir().unwrap();
    run.api
        .answer(&items_path(11, 0), page(vec![answer(1, &["http://pic1.zhimg.com/a.jpg"])], false));
    run.api
        .answer(&items_path(11, 20), page(vec![answer(2, &["http://pic1.zhimg.com/b.jpg"])], false));
    run.api.answer(&items_path(11, 40), page(vec![answer(3, &[])], true));
    run.run(config(folder.path(), &[11], 100)).await.expect("first run");
    assert_eq!(run.api.calls().len(), 3);

    // A post added on top: the first page is new, the second known, the third not asked.
    run.api.answer(
        &items_path(11, 0),
        page(
            vec![
                answer(9, &["http://pic1.zhimg.com/c.jpg"]),
                answer(1, &["http://pic1.zhimg.com/a.jpg"]),
            ],
            false,
        ),
    );
    run.api.calls.lock().unwrap().clear();
    run.run(config(folder.path(), &[11], 100)).await.expect("second run");

    assert_eq!(run.api.calls(), [items_path(11, 0), items_path(11, 20)]);
    assert_eq!(statuses(&db).await.len(), 4);
}

#[tokio::test]
async fn a_run_downloads_at_most_its_limit_and_walks_again_from_the_top() {
    let database = database().await;
    let db = database.db().clone();
    sign_in_to_zhihu(&db).await;
    let run = Run::new(db.clone()).await;
    let folder = tempfile::tempdir().unwrap();
    run.api
        .answer(&items_path(11, 0), page(vec![answer(1, &["http://pic1.zhimg.com/a.jpg"])], false));
    run.api
        .answer(&items_path(11, 20), page(vec![answer(2, &["http://pic1.zhimg.com/b.jpg"])], true));

    run.run(config(folder.path(), &[11], 1)).await.expect("first run");
    assert_eq!(statuses(&db).await.len(), 1);
    assert_eq!(run.api.calls(), [items_path(11, 0)]);

    run.run(config(folder.path(), &[11], 1)).await.expect("second run");
    let statuses = statuses(&db).await;
    assert_eq!(statuses.len(), 2);
    assert!(statuses.iter().all(|(_, status, _)| status == "saved"), "{statuses:?}");
}

#[tokio::test]
async fn a_post_saved_from_a_link_is_not_saved_again() {
    let database = database().await;
    let db = database.db().clone();
    sign_in_to_zhihu(&db).await;
    saved_links::record(
        &db,
        NewSavedLink {
            url: "https://www.zhihu.com/answer/1",
            source: "telegram",
            item: "fav #12",
            title: "问题 1",
            saved_path: "collection/x.jpg",
        },
    )
    .await
    .unwrap();
    let run = Run::new(db.clone()).await;
    let folder = tempfile::tempdir().unwrap();
    run.api
        .answer(&items_path(11, 0), page(vec![answer(1, &["http://pic1.zhimg.com/a.jpg"])], true));

    run.run(config(folder.path(), &[11], 100)).await.expect("run");

    assert_eq!(
        statuses(&db).await,
        [("https://www.zhihu.com/answer/1".to_owned(), "already_saved".to_owned(), 0)]
    );
    assert!(run.web.urls().is_empty());
    assert!(run.notifier.sent().is_empty());
}

#[tokio::test]
async fn a_post_whose_media_never_loads_is_given_up_after_its_tries() {
    let database = database().await;
    let db = database.db().clone();
    sign_in_to_zhihu(&db).await;
    let run = Run::new(db.clone()).await;
    let folder = tempfile::tempdir().unwrap();
    run.api.answer(
        &items_path(11, 0),
        page(vec![answer(1, &["http://pic1.zhimg.com/broken.jpg"])], true),
    );

    for attempt in 1..=MAX_ATTEMPTS {
        run.run(config(folder.path(), &[11], 100)).await.expect("run");
        let status = if attempt < MAX_ATTEMPTS { "failed" } else { "given_up" };
        assert_eq!(
            statuses(&db).await,
            [("https://www.zhihu.com/answer/1".to_owned(), status.to_owned(), attempt)]
        );
    }
    // Given up, it is not tried again.
    run.run(config(folder.path(), &[11], 100)).await.expect("run");
    assert_eq!(statuses(&db).await[0].2, MAX_ATTEMPTS);
    let sent = run.notifier.sent();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].kind, "download_failed");
    assert_eq!(sent[0].link_url, "https://www.zhihu.com/answer/1");
}

#[tokio::test]
async fn without_a_login_the_run_asks_for_one() {
    let database = database().await;
    let db = database.db().clone();
    let run = Run::new(db.clone()).await;
    let folder = tempfile::tempdir().unwrap();

    let error = run.run(config(folder.path(), &[11], 100)).await.expect_err("no login");
    assert_eq!(error.notification_dedupe_key(), Some("zhihu:login"));
    assert_eq!(*run.api.opened.lock().unwrap(), 0);

    sign_in_to_zhihu(&db).await;
    run.api
        .fail(&items_path(11, 0), ZhihuError::Login("Zhihu is not signed in any more".to_owned()));
    let error = run.run(config(folder.path(), &[11], 100)).await.expect_err("signed out");
    assert_eq!(error.notification_dedupe_key(), Some("zhihu:login"));
    assert_eq!(*run.api.closed.lock().unwrap(), 1);
    let status: String = sqlx::query_scalar("SELECT status FROM site_sessions WHERE site = 'zhihu'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(status, "invalid");
}
