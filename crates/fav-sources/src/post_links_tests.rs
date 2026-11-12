//! The Python tests of the post-link parsers: `tests/test_post_links.py`,
//! `tests/test_zhihu_post.py`, and the article, Tieba and NGA parsing tests of
//! `tests/test_wechat.py` that need nothing of the WeChat source itself.
//!
//! The fake web serves plain HTTP only, so the sites' `https://` URLs are `http://` here
//! where a request is made, and NGA's and Zhihu lens's fixed endpoints are pointed at it.

use std::sync::{Arc, Mutex};

use chrono::Datelike as _;
use fav_core::pyjson::{self, Dict, Value};
use fav_db::SettingsStore;
use fav_runtime::BoxFuture;
use fav_runtime::site_sessions::{SiteSession, StoredCookie};

use crate::nga_post::{floor_images, original_url, verify_session_at};
use crate::post_links::{self, Endpoints, Needs, PostClient, PostError, PostLinks, SessionStore};
use crate::testutil::{FakeWeb, Reply};
use crate::tieba_post::parse_tieba_link;
use crate::wechat_article::{ArticleLink, canonical_url, image_extension, original_image_url, parse_article_link, parse_article_page};
use crate::zhihu_post::{self, ZhihuError, ZhihuKind, ZhihuLink, ZhihuVideo};

fn json(text: &str) -> Value {
    pyjson::loads(text).expect("JSON")
}

fn object(text: &str) -> Dict {
    match json(text) {
        Value::Dict(dict) => dict,
        other => panic!("expected an object, got {other:?}"),
    }
}

fn text_reply(status: u16, body: &str) -> Reply {
    Reply::Respond {
        status,
        headers: vec![("content-type".to_owned(), "text/plain; charset=utf-8".to_owned())],
        body: body.as_bytes().to_vec(),
    }
}

fn json_reply(status: u16, body: &str) -> Reply {
    Reply::Respond {
        status,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: body.as_bytes().to_vec(),
    }
}

fn endpoints() -> Endpoints {
    Endpoints {
        nga_read: "http://bbs.nga.cn/read.php".to_owned(),
        lens: "http://lens.zhihu.com/api/v4/videos/".to_owned(),
        ..Endpoints::default()
    }
}

fn client(web: &FakeWeb) -> PostClient {
    PostClient::with(Some(&web.proxy), endpoints()).expect("client")
}

/// Site sessions with an optional NGA login.
struct Sessions {
    login: Option<Vec<StoredCookie>>,
    checks: Mutex<Vec<(bool, String)>>,
}

impl Sessions {
    fn none() -> Arc<Self> {
        Arc::new(Self {
            login: None,
            checks: Mutex::new(Vec::new()),
        })
    }
}

impl SessionStore for Sessions {
    fn load<'a>(&'a self, site: &'a str) -> BoxFuture<'a, Result<Option<SiteSession>, fav_runtime::Error>> {
        Box::pin(async move {
            Ok(self.login.clone().map(|cookies| SiteSession {
                site: site.to_owned(),
                cookies,
                account: String::new(),
                status: "valid".to_owned(),
                message: String::new(),
                expires_at: None,
                signed_in_at: None,
                checked_at: None,
                extra: Vec::new(),
            }))
        })
    }

    fn record_check<'a>(&'a self, _site: &'a str, ok: bool, message: &'a str) -> BoxFuture<'a, Result<(), fav_runtime::Error>> {
        self.checks.lock().expect("lock").push((ok, message.to_owned()));
        Box::pin(async { Ok(()) })
    }
}

fn links(web: &FakeWeb, tikhub_token: &str) -> PostLinks {
    let mut settings = fav_db::settings_store::default_settings();
    settings.services.tikhub.api_url = "http://api.tikhub.dev".to_owned();
    settings.services.tikhub.token = tikhub_token.to_owned();
    PostLinks::new(client(web), Arc::new(SettingsStore::pinned(settings)), Sessions::none())
}

fn link_for(url: &str) -> ArticleLink {
    post_links::link_for_url(url, "", "").expect("parses").expect("a post link")
}

fn link_error(result: Result<(crate::wechat_article::ArticlePage, String), PostError>) -> post_links::PostLinkError {
    match result {
        Err(PostError::Link(error)) => error,
        other => panic!("expected a post link error, got {other:?}"),
    }
}

// ---------- tests/test_post_links.py ----------

const ZHIHU_CARD: &str = r#"<msg><appmsg appid="" sdkver="0">
<title>高跟鞋配白丝真的很难看吗？</title><des>知乎用户的回答</des><type>5</type>
<url>https://www.zhihu.com/question/1981023166554989468/answer/2100311203096573240?share_code=x&amp;utm_psn=1</url>
</appmsg></msg>"#;

// py: tests/test_post_links.py::test_link_for_url_names_each_site_canonically
#[test]
fn link_for_url_names_each_site_canonically() {
    for (url, expected) in [
        (
            "https://tieba.baidu.com/p/9876543210?share=9",
            Some("https://tieba.baidu.com/p/9876543210"),
        ),
        (
            "https://bbs.nga.cn/read.php?tid=12345&page=2",
            Some("https://bbs.nga.cn/read.php?tid=12345"),
        ),
        (
            "https://ngabbs.com/read.php?tid=12345",
            Some("https://bbs.nga.cn/read.php?tid=12345"),
        ),
        ("https://zhuanlan.zhihu.com/p/42?utm=1", Some("https://zhuanlan.zhihu.com/p/42")),
        ("https://www.zhihu.com/pin/7", Some("https://www.zhihu.com/pin/7")),
        (
            "http://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=b&chksm=c",
            Some("https://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=b&chksm=c"),
        ),
        ("https://www.bilibili.com/video/BV1xx", None),
        ("ftp://tieba.baidu.com/p/1", None),
    ] {
        let link = post_links::link_for_url(url, "", "").expect("parses");
        assert_eq!(link.map(|link| link.url).as_deref(), expected, "{url}");
    }
}

// py: tests/test_post_links.py::test_find_link_reads_text_then_preview
#[test]
fn find_link_reads_text_then_preview() {
    let found = |texts: &[&str]| post_links::find_link(texts).expect("parses").map(|link| link.url);
    assert_eq!(
        found(&["看这个（https://zhuanlan.zhihu.com/p/42）"]).as_deref(),
        Some("https://zhuanlan.zhihu.com/p/42")
    );
    assert_eq!(
        found(&["nothing here", "https://tieba.baidu.com/p/5"]).as_deref(),
        Some("https://tieba.baidu.com/p/5")
    );
    assert_eq!(found(&["https://example.com/a", ""]), None);
}

// py: tests/test_post_links.py::test_a_wechat_card_for_a_zhihu_answer_is_a_shared_link
#[test]
fn a_wechat_card_for_a_zhihu_answer_is_a_shared_link() {
    let link = post_links::shared_link(ZHIHU_CARD).expect("parses").expect("a shared link");
    assert_eq!(link.url, "https://www.zhihu.com/answer/2100311203096573240");
    assert_eq!(link.title, "高跟鞋配白丝真的很难看吗？");
    assert_eq!(post_links::site_name(&link).expect("parses"), "Zhihu");
}

// py: tests/test_post_links.py::test_a_zhihu_post_is_read_with_the_tikhub_service_key
#[tokio::test]
async fn a_zhihu_post_is_read_with_the_tikhub_service_key() {
    let web = FakeWeb::start(|_| {
        json_reply(
            200,
            r#"{"data": {"title": "T", "content": "<img data-original=\"https://pic1.zhimg.com/v2-a_r.jpg\"/>"}}"#,
        )
    })
    .await;
    let (page, html) = links(&web, "service-key")
        .fetch_page(&link_for("https://zhuanlan.zhihu.com/p/42"))
        .await
        .expect("read");
    assert_eq!(
        (page.title.as_str(), page.images.clone(), html.as_str()),
        ("T", vec!["https://pic1.zhimg.com/v2-a_r.jpg".to_owned()], "")
    );
    let seen = web.seen();
    assert_eq!(seen[0].host(), "api.tikhub.dev");
    assert_eq!(seen[0].header("authorization"), Some("Bearer service-key"));
}

// py: tests/test_post_links.py::test_a_zhihu_post_without_a_tikhub_key_waits_for_one
#[tokio::test]
async fn a_zhihu_post_without_a_tikhub_key_waits_for_one() {
    let web = FakeWeb::start(|_| Reply::status(500)).await;
    let error = link_error(links(&web, "").fetch_page(&link_for("https://www.zhihu.com/pin/7")).await);
    assert_eq!(error.needs, Needs::TikHub);
}

// py: tests/test_post_links.py::test_an_nga_thread_without_a_login_waits_for_one
#[tokio::test]
async fn an_nga_thread_without_a_login_waits_for_one() {
    let web = FakeWeb::start(|_| Reply::status(500)).await;
    let error = link_error(links(&web, "").fetch_page(&link_for("https://bbs.nga.cn/read.php?tid=1")).await);
    assert_eq!(error.needs, Needs::NgaLogin);
}

// py: tests/test_post_links.py::test_a_wechat_image_post_behind_the_captcha_is_its_own_error
#[tokio::test]
async fn a_wechat_image_post_behind_the_captcha_is_its_own_error() {
    let web = FakeWeb::start(|seen| {
        if seen.path() == "/s" {
            Reply::Respond {
                status: 302,
                headers: vec![(
                    "location".to_owned(),
                    "http://mp.weixin.qq.com/mp/wappoc_appmsgcaptcha?x=1".to_owned(),
                )],
                body: Vec::new(),
            }
        } else {
            text_reply(200, "captcha")
        }
    })
    .await;
    // `link_for_url` would make the article https, which the fake web does not serve.
    let link = ArticleLink::new("http://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=b", "", "");
    let error = link_error(links(&web, "").fetch_page(&link).await);
    assert!(error.captcha);
    assert_eq!(error.class_name(), "PostCaptchaError");
}

// py: tests/test_post_links.py::test_fetch_image_skips_tiebas_expired_grant_logo
#[tokio::test]
async fn fetch_image_skips_tiebas_expired_grant_logo() {
    let web = FakeWeb::start(|seen| {
        if seen.url.contains("expired") {
            Reply::Respond {
                status: 200,
                headers: vec![
                    ("content-type".to_owned(), "image/png".to_owned()),
                    ("error-message".to_owned(), "expired".to_owned()),
                ],
                body: b"\x89PNGlogo".to_vec(),
            }
        } else {
            Reply::Respond {
                status: 200,
                headers: vec![("content-type".to_owned(), "image/jpeg".to_owned())],
                body: b"\xff\xd8\xffjpeg".to_vec(),
            }
        }
    })
    .await;
    let client = client(&web);
    let image = client.fetch_image("http://pic1.zhimg.com/v2-a_r.jpg").await.expect("no error");
    assert_eq!(
        image.map(|(body, extension)| (body.to_vec(), extension)),
        Some((b"\xff\xd8\xffjpeg".to_vec(), "jpg"))
    );
    assert_eq!(
        client.fetch_image("http://tiebapic.baidu.com/expired.jpg").await.expect("no error"),
        None
    );
}

// ---------- tests/test_zhihu_post.py ----------

/// Trimmed from a real `fetch_answer_detail` answer: each picture appears twice (a
/// `<noscript>` copy and the lazy-loaded one), and a video box carries a poster image.
const ANSWER_HTML: &str = concat!(
    r#"<p data-pid="a">黑丝或者肉丝叭</p>"#,
    r#"<figure data-size="normal"><noscript><img src="https://pic1.zhimg.com/50/v2-aaa_720w.jpg?source=2c26e567" "#,
    r#"data-original="https://pica.zhimg.com/v2-aaa_r.jpg?source=2c26e567"/></noscript>"#,
    r#"<img src="data:image/svg+xml;utf8,&lt;svg&gt;" class="origin_image lazy" "#,
    r#"data-original="https://pica.zhimg.com/v2-aaa_r.jpg?source=2c26e567" "#,
    r#"data-actualsrc="https://pic1.zhimg.com/50/v2-aaa_720w.jpg?source=2c26e567"/></figure>"#,
    r#"<figure><img src="https://picx.zhimg.com/v2-bbb_720w.jpg"/></figure>"#,
    r#"<img src="https://www.zhihu.com/equation?tex=x"/>"#,
    r#"<a class="video-box" href="https://link.zhihu.com/?target=https%3A//www.zhihu.com/video/2091194612627121396" "#,
    r#"target="_blank" data-poster="https://pica.zhimg.com/v2-poster.jpg?source=ccfced1a" data-lens-id="2091194612627121396">"#,
    r#"<img class="thumbnail" src="https://pica.zhimg.com/v2-poster.jpg?source=ccfced1a"/></a>"#,
);

fn zhihu(kind: ZhihuKind, id: &str) -> ZhihuLink {
    ZhihuLink::new(kind, id)
}

// py: tests/test_zhihu_post.py::test_parse_zhihu_url
#[test]
fn parse_zhihu_url() {
    for (url, expected) in [
        (
            "https://www.zhihu.com/question/1981023166554989468/answer/2100311203096573240",
            Some(zhihu(ZhihuKind::Answer, "2100311203096573240")),
        ),
        (
            "https://www.zhihu.com/question/1/answer/2/?share_code=abc&utm_psn=1",
            Some(zhihu(ZhihuKind::Answer, "2")),
        ),
        (
            "https://www.zhihu.com/answer/2100311203096573240",
            Some(zhihu(ZhihuKind::Answer, "2100311203096573240")),
        ),
        (
            "https://zhuanlan.zhihu.com/p/669214677",
            Some(zhihu(ZhihuKind::Article, "669214677")),
        ),
        (
            "http://www.zhihu.com/pin/2100667138746983678",
            Some(zhihu(ZhihuKind::Pin, "2100667138746983678")),
        ),
        (
            "https://link.zhihu.com/?target=https%3A//zhuanlan.zhihu.com/p/42",
            Some(zhihu(ZhihuKind::Article, "42")),
        ),
        ("https://www.zhihu.com/question/1981023166554989468", None),
        ("https://zhihu.com.evil.example/answer/1", None),
        ("https://example.com/answer/1", None),
    ] {
        assert_eq!(zhihu_post::parse_zhihu_url(url).expect("parses"), expected, "{url}");
    }
}

// py: tests/test_zhihu_post.py::test_answer_content_keeps_full_size_pictures_once_and_skips_video_posters
#[test]
fn answer_content_keeps_full_size_pictures_once_and_skips_video_posters() {
    assert_eq!(
        zhihu_post::content_images(ANSWER_HTML).expect("parses"),
        [
            "https://pica.zhimg.com/v2-aaa_r.jpg?source=2c26e567",
            "https://picx.zhimg.com/v2-bbb_720w.jpg"
        ]
    );
    assert_eq!(
        zhihu_post::content_videos(ANSWER_HTML).expect("parses"),
        [ZhihuVideo {
            id: "2091194612627121396".to_owned(),
            fallback_url: String::new(),
            cover_url: "https://pica.zhimg.com/v2-poster.jpg?source=ccfced1a".to_owned(),
        }]
    );
}

fn answer_data(extra: &str) -> Dict {
    let mut data = object(extra);
    data.insert("content", Value::from(ANSWER_HTML));
    data
}

// py: tests/test_zhihu_post.py::test_parse_answer_uses_question_title_and_author
#[test]
fn parse_answer_uses_question_title_and_author() {
    let link = ZhihuLink::new(ZhihuKind::Answer, "2100311203096573240");
    let data = answer_data(
        r#"{"author": {"name": "知乎用户mYQRnd"}, "created_time": 1793533212, "question": {"title": "高跟鞋配白丝真的很难看吗？"}}"#,
    );
    let post = zhihu_post::parse_post(&link, &data).expect("parses");
    assert_eq!(post.title, "高跟鞋配白丝真的很难看吗？");
    assert_eq!(post.author, "知乎用户mYQRnd");
    assert!(post.published_at.is_some());
    assert_eq!(post.images.len(), 2);
    assert_eq!(
        post.videos.iter().map(|video| video.id.as_str()).collect::<Vec<_>>(),
        ["2091194612627121396"]
    );
}

// py: tests/test_zhihu_post.py::test_parse_pin_reads_items
#[test]
fn parse_pin_reads_items() {
    let link = ZhihuLink::new(ZhihuKind::Pin, "2100667138746983678");
    let data = object(
        r#"{
            "author": {"name": "someone"},
            "created": 1793533212,
            "excerpt_title": "穿搭小技巧 | <a class=\"hash_tag\" href=\"https://www.zhihu.com/topic/1\">#大衣穿搭</a>",
            "content": [
                {"type": "text", "own_text": "穿搭小技巧 | <a class=\"hash_tag\" href=\"https://www.zhihu.com/topic/1\">#大衣穿搭</a>"},
                {"type": "image", "url": "https://pic1.zhimg.com/100/v2-wm_720w.jpg", "original_url": "https://picx.zhimg.com/v2-orig.png?source=e3d01f54"},
                {
                    "type": "video",
                    "video_id": "2100665385288836251",
                    "thumbnail": "https://pic1.zhimg.com/80/v2-thumb",
                    "video_info": {
                        "playlist": [
                            {"quality": "ld", "width": 476, "height": 848, "url": "https://vdn3.vzuu.com/SD/x.mp4"},
                            {"quality": "hd", "width": 720, "height": 1280, "url": "https://vdn3.vzuu.com/HD/x.mp4"}
                        ]
                    }
                }
            ]
        }"#,
    );
    let post = zhihu_post::parse_post(&link, &data).expect("parses");
    assert_eq!(post.title, "穿搭小技巧 | #大衣穿搭");
    assert_eq!(post.images, ["https://picx.zhimg.com/v2-orig.png?source=e3d01f54"]);
    assert_eq!(
        post.videos,
        [ZhihuVideo {
            id: "2100665385288836251".to_owned(),
            fallback_url: "https://vdn3.vzuu.com/HD/x.mp4".to_owned(),
            cover_url: "https://pic1.zhimg.com/80/v2-thumb".to_owned(),
        }]
    );
}

// py: tests/test_zhihu_post.py::test_fetch_post_calls_the_endpoint_for_the_kind
#[tokio::test]
async fn fetch_post_calls_the_endpoint_for_the_kind() {
    let body = pyjson::dumps(
        &Value::Dict({
            let mut payload = Dict::new();
            payload.insert("code", Value::Int(200));
            let mut data = object(r#"{"title": "Article", "author": {"name": "a"}}"#);
            data.insert("content", Value::from(ANSWER_HTML));
            payload.insert("data", Value::Dict(data));
            payload
        }),
        &pyjson::DumpOptions::compact(),
    )
    .expect("dumpable");
    let web = FakeWeb::start(move |_| json_reply(200, &body)).await;
    let post = zhihu_post::fetch_post(
        &client(&web),
        &ZhihuLink::new(ZhihuKind::Article, "42"),
        "http://api.tikhub.io/",
        "key",
    )
    .await
    .expect("read");
    assert_eq!(post.title, "Article");
    let seen = web.seen();
    assert_eq!(seen[0].path(), "/api/v1/zhihu/web/fetch_column_article_detail");
    assert_eq!(seen[0].query(), "article_id=42");
    assert_eq!(seen[0].header("Authorization"), Some("Bearer key"));
}

// py: tests/test_zhihu_post.py::test_fetch_post_failures
#[tokio::test]
async fn fetch_post_failures() {
    for (status, body, account) in [
        (402, r#"{"detail": "balance"}"#, true),
        (400, r#"{"detail": {"message": "not found"}}"#, false),
        (200, r#"{"code": 200, "data": {}}"#, false),
    ] {
        let web = FakeWeb::start(move |_| json_reply(status, body)).await;
        let error = zhihu_post::fetch_post(
            &client(&web),
            &ZhihuLink::new(ZhihuKind::Answer, "1"),
            "http://api.tikhub.io",
            "key",
        )
        .await
        .expect_err("fails");
        if account {
            assert!(matches!(error, ZhihuError::Account(_)), "{status}: {error:?}");
        } else {
            assert!(matches!(error, ZhihuError::Zhihu(_)), "{status}: {error:?}");
        }
    }
}

/// TikHub failing is told apart from a post it could not read (D185).
#[tokio::test]
async fn fetch_post_tells_an_outage_from_an_unreadable_post() {
    const REQUEST_FAILED: &str =
        r#"{"detail": {"code": 400, "message": "Request failed. Please retry. You won't be charged for this request."}}"#;
    for (reply, outage) in [
        (json_reply(400, REQUEST_FAILED), true),
        (json_reply(500, r#"{"detail": "upstream"}"#), true),
        (json_reply(429, r#"{"detail": {"message": "slow down"}}"#), true),
        (text_reply(502, "<html>Bad Gateway</html>"), true),
        (Reply::Close, true),
        (json_reply(400, r#"{"detail": {"code": 400, "message": "not found"}}"#), false),
        (json_reply(404, r#"{"detail": "gone"}"#), false),
        (text_reply(200, "not json"), false),
    ] {
        let label = format!("{reply:?}");
        let web = FakeWeb::start(move |_| reply.clone()).await;
        let error = zhihu_post::fetch_post(
            &client(&web),
            &ZhihuLink::new(ZhihuKind::Answer, "1"),
            "http://api.tikhub.io",
            "key",
        )
        .await
        .expect_err("fails");
        assert_eq!(matches!(error, ZhihuError::Outage(_)), outage, "{label}: {error:?}");
        assert_eq!(matches!(error, ZhihuError::Zhihu(_)), !outage, "{label}: {error:?}");
    }
    let web = FakeWeb::start(|_| json_reply(400, REQUEST_FAILED)).await;
    let error = zhihu_post::fetch_post(
        &client(&web),
        &ZhihuLink::new(ZhihuKind::Answer, "1"),
        "http://api.tikhub.io",
        "key",
    )
    .await
    .expect_err("fails");
    assert_eq!(
        error.to_string(),
        "TikHub is failing for https://www.zhihu.com/answer/1 (HTTP 400): Request failed. Please retry. You won't be charged for this request."
    );
}

// py: tests/test_zhihu_post.py::test_fetch_post_without_key_makes_no_request
#[tokio::test]
async fn fetch_post_without_key_makes_no_request() {
    let web = FakeWeb::start(|_| Reply::status(500)).await;
    let error = zhihu_post::fetch_post(&client(&web), &ZhihuLink::new(ZhihuKind::Answer, "1"), "http://api.tikhub.io", "")
        .await
        .expect_err("fails");
    assert!(matches!(error, ZhihuError::Account(_)), "{error:?}");
    assert!(web.seen().is_empty(), "no request without a key");
}

const LENS_TWO_QUALITIES: &str = r#"{"playlist": {
    "LD": {"width": 476, "height": 848, "bitrate": 379.5, "play_url": "https://vdn3.vzuu.com/SD/v.mp4"},
    "HD": {"width": 720, "height": 1280, "bitrate": 559.1, "play_url": "https://vdn3.vzuu.com/HD/v.mp4"}
}}"#;

// py: tests/test_zhihu_post.py::test_resolve_video_takes_widest_lens_entry_and_falls_back
#[tokio::test]
async fn resolve_video_takes_widest_lens_entry_and_falls_back() {
    let web = FakeWeb::start(|seen| {
        if seen.path().ends_with("/1") {
            text_reply(200, LENS_TWO_QUALITIES)
        } else {
            json_reply(403, r#"{"error": {"code": 40362}}"#)
        }
    })
    .await;
    let client = client(&web);
    let resolve = |id: &str, fallback: &str| {
        let video = ZhihuVideo {
            id: id.to_owned(),
            fallback_url: fallback.to_owned(),
            cover_url: String::new(),
        };
        let client = client.clone();
        async move { zhihu_post::resolve_video(&client, &video).await.expect("no error") }
    };
    assert_eq!(resolve("1", "").await, "https://vdn3.vzuu.com/HD/v.mp4");
    assert_eq!(
        resolve("2", "https://vdn3.vzuu.com/HD/p.mp4").await,
        "https://vdn3.vzuu.com/HD/p.mp4"
    );
    assert_eq!(resolve("2", "").await, "");
}

// py: tests/test_zhihu_post.py::test_fetch_page_resolves_videos_and_drops_ones_without_a_play_url
#[tokio::test]
async fn fetch_page_resolves_videos_and_drops_ones_without_a_play_url() {
    let content = format!("{ANSWER_HTML}{}", ANSWER_HTML.replace("2091194612627121396", "1"));
    let mut data = object(r#"{"author": {"name": "a"}, "question": {"title": "Q"}}"#);
    data.insert("content", Value::Str(content));
    let mut payload = Dict::new();
    payload.insert("data", Value::Dict(data));
    let answer = pyjson::dumps(&Value::Dict(payload), &pyjson::DumpOptions::compact()).expect("dumpable");
    let web = FakeWeb::start(move |seen| {
        if seen.host() == "api.tikhub.io" {
            json_reply(200, &answer)
        } else if seen.path().ends_with("/2091194612627121396") {
            json_reply(
                200,
                r#"{"playlist": {"HD": {"width": 720, "height": 1280, "play_url": "https://vdn3.vzuu.com/HD/v.mp4"}}}"#,
            )
        } else {
            json_reply(403, r#"{"error": {"code": 40362}}"#)
        }
    })
    .await;
    let page = zhihu_post::fetch_page(
        &client(&web),
        &ZhihuLink::new(ZhihuKind::Answer, "9"),
        "http://api.tikhub.io",
        "key",
    )
    .await
    .expect("read");
    assert_eq!((page.title.as_str(), page.account.as_str()), ("Q", "a"));
    assert_eq!(page.images.len(), 2);
    assert_eq!(
        page.videos.iter().map(|video| video.url.as_str()).collect::<Vec<_>>(),
        ["https://vdn3.vzuu.com/HD/v.mp4"]
    );
    assert_eq!(page.videos[0].cover_url, "https://pica.zhimg.com/v2-poster.jpg?source=ccfced1a");
}

// ---------- tests/test_wechat.py: articles, Tieba and NGA ----------

const SHARE_URL: &str = "http://mp.weixin.qq.com/s?__biz=MzA5==&amp;mid=2247&amp;idx=1&amp;sn=abc123&amp;chksm=feed#rd";
const FETCH_URL: &str = "https://mp.weixin.qq.com/s?__biz=MzA5==&mid=2247&idx=1&sn=abc123&chksm=feed";
const CANONICAL_URL: &str = "https://mp.weixin.qq.com/s?__biz=MzA5==&mid=2247&idx=1&sn=abc123";
const IMAGE_A: &str = "https://mmbiz.qpic.cn/sz_mmbiz_jpg/AAA/640?wx_fmt=jpeg&from=appmsg";
const IMAGE_B: &str = "https://mmbiz.qpic.cn/sz_mmbiz_png/BBB/640?wx_fmt=png";
const COVER: &str = "https://mmbiz.qpic.cn/mmbiz_jpg/COVER/0?wx_fmt=jpeg";
const IMAGE_POST_URL: &str =
    "https://mp.weixin.qq.com/s?__biz=MzA5==&amp;mid=2247&amp;idx=1&amp;sn=abc123&amp;scene=1&amp;from_masonry=1&amp;t=1#rd";
const TIEBA_SHARE_URL: &str = concat!(
    "https://tieba.baidu.com/p/11093034639?&amp;share=9105&amp;fr=sharewise&amp;is_video=false",
    "&amp;unique=F9676370&amp;st=1791654528&amp;client_type=1&amp;sfc=weixin&amp;share_from=post",
);
const TIEBA_URL: &str = "https://tieba.baidu.com/p/11093034639";
const NGA_ATTACH: &str = "https://img.nga.cn/attachments/mon_202510/11/";

fn page_fixture() -> String {
    let escaped = IMAGE_A.replace('&', "&amp;");
    format!(
        r#"<html><head><meta property="og:title" content="og title" /></head>
<script>var msg_title = '每日Coser分享'.html(false); var nickname = htmlDecode("小叶菌ovo"); var ct = "1759600000";</script>
<img class="avatar" data-src="https://mmbiz.qpic.cn/mmbiz_png/AVATAR/0?wx_fmt=png" />
<div class="rich_media_content" id="js_content">
<p><img class="rich_pages" data-src="{escaped}" /></p>
<p><img data-ratio="1" data-src="{IMAGE_B}" /></p>
<p><img data-src="{escaped}" /></p>
</div></html>"#
    )
}

fn share(url: &str, kind: &str) -> String {
    format!(
        concat!(
            r#"<msg><appmsg appid="" sdkver="0"><title>每日分享</title><des>今天的</des><action>view</action>"#,
            "<type>{kind}</type><url>{url}</url><sourcedisplayname>小叶菌ovo</sourcedisplayname>",
            "<refermsg><type>1</type></refermsg></appmsg><fromusername>wxid_sender</fromusername></msg>",
        ),
        kind = kind,
        url = url
    )
}

fn image_post_share(pictures: u32) -> String {
    share(IMAGE_POST_URL, "5").replace(
        "</appmsg>",
        &format!(
            "<mmreadershare><itemshowtype>8</itemshowtype><coverpicimageurl>{COVER}</coverpicimageurl><piccount>{pictures}</piccount></mmreadershare></appmsg>"
        ),
    )
}

/// As an image post's page writes a picture: its own `cdn_url`, escaped for a script
/// string, beside nested objects with `cdn_url`s of their own.
fn picture_item(name: &str) -> String {
    format!(
        concat!(
            "{{ width: '2560' * 1, height: '3413' * 1,",
            r" cdn_url: 'https:\x2f\x2fmmbiz.qpic.cn\x2fsz_mmbiz_jpg\x2f{name}\x2f0?wx_fmt=jpeg\x26amp;from=appmsg',",
            " watermark_info: {{ cdn_url: 'https://mmbiz.qpic.cn/sz_mmbiz_jpg/W{name}/0?wx_fmt=jpeg', width: '0' * 1,",
            r" title: 'a }} [ \' tricky' }}, live_photo: [], }}",
        ),
        name = name
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
        "<html><script>var msg_title = '九张图';\nvar nickname = htmlDecode(\"某号\");\nvar ct = \"1760000000\";\nwindow.picture_page_info_list = [{}];\nif (window.picture_page_info_list.length) {{}}</script><div id=\"js_content\">caption</div></html>",
        items.join(",")
    )
}

// py: tests/test_wechat.py::test_an_article_share_is_recognised_from_its_appmsg_xml
#[test]
fn an_article_share_is_recognised_from_its_appmsg_xml() {
    let link = parse_article_link(&share(SHARE_URL, "5")).expect("parses").expect("an article");
    assert_eq!(link.url, format!("{FETCH_URL}#rd"));
    assert_eq!(link.title, "每日分享");
    assert_eq!(link.description, "今天的");
    assert_eq!(link.account, "小叶菌ovo");
    assert_eq!(canonical_url(&link.url).expect("parses"), CANONICAL_URL);
    assert_eq!(parse_article_link(&share(SHARE_URL, "6")).expect("parses"), None);
    assert_eq!(parse_article_link(&share("https://example.com/s?a=1", "5")).expect("parses"), None);
    assert_eq!(parse_article_link("plain text").expect("parses"), None);
}

// py: tests/test_wechat.py::test_an_article_page_yields_its_body_images_once_each
#[test]
fn an_article_page_yields_its_body_images_once_each() {
    let page = parse_article_page(&page_fixture()).expect("parses");
    assert_eq!(page.title, "每日Coser分享");
    assert_eq!(page.account, "小叶菌ovo");
    assert_eq!(page.published_at.map(|time| time.year()), Some(2025));
    assert_eq!(page.images, [IMAGE_A, IMAGE_B]);
}

// py: tests/test_wechat.py::test_an_image_post_takes_its_pictures_from_the_picture_list
#[test]
fn an_image_post_takes_its_pictures_from_the_picture_list() {
    let page = parse_article_page(
        "var msg_title = 'pics'; picture_page_info_list = [{cdn_url: 'https://mmbiz.qpic.cn/mmbiz_jpg/P1/0?wx_fmt=jpeg'}];",
    )
    .expect("parses");
    assert_eq!(page.images, ["https://mmbiz.qpic.cn/mmbiz_jpg/P1/0?wx_fmt=jpeg"]);
}

// py: tests/test_wechat.py::test_a_picture_list_yields_each_picture_and_not_its_nested_urls
#[test]
fn a_picture_list_yields_each_picture_and_not_its_nested_urls() {
    let page = parse_article_page(&picture_page()).expect("parses");
    assert_eq!(page.title, "九张图");
    assert_eq!(page.images, pictures());
    let empty = parse_article_page(r#"picture_page_info_list = []; <div id="js_content"></div>"#).expect("parses");
    assert!(empty.images.is_empty());
}

/// The article assertions of `test_image_urls_ask_for_the_original_size`; that test stays
/// with WP14, whose `sniff_extension` also knows WeChat's own `wxgf` pictures.
#[test]
fn image_urls_ask_for_the_original_size() {
    assert_eq!(
        original_image_url(IMAGE_A).expect("parses"),
        "https://mmbiz.qpic.cn/sz_mmbiz_jpg/AAA/0?wx_fmt=jpeg&from=appmsg"
    );
    assert_eq!(
        original_image_url("https://mmbiz.qpic.cn/mmbiz_gif/C/640").expect("parses"),
        "https://mmbiz.qpic.cn/mmbiz_gif/C/0"
    );
    assert_eq!(image_extension(IMAGE_B, "").expect("parses"), "png");
    assert_eq!(image_extension(IMAGE_A, "image/webp").expect("parses"), "webp");
    assert_eq!(post_links::sniff_extension(b"\x89PNG\r\n\x1a\nfull-size"), Some("png"));
    assert_eq!(post_links::sniff_extension(b"????"), None);
}

// py: tests/test_wechat.py::test_an_image_post_share_names_its_cover_and_picture_count
#[test]
fn an_image_post_share_names_its_cover_and_picture_count() {
    let link = parse_article_link(&image_post_share(9)).expect("parses").expect("an article");
    assert_eq!(
        (link.show_type.as_str(), link.picture_count, link.cover_url.as_str()),
        ("8", 9, COVER)
    );
    let plain = parse_article_link(&share(SHARE_URL, "5")).expect("parses").expect("an article");
    assert_eq!(plain.cover_url, "");
}

// py: tests/test_wechat.py::test_a_tieba_share_names_its_post
#[test]
fn a_tieba_share_names_its_post() {
    let message = share(TIEBA_SHARE_URL, "5").replace("<title>每日分享</title>", "<title>其实HDR比dlss5重要</title>");
    let link = parse_tieba_link(&message).expect("parses").expect("a Tieba post");
    assert_eq!((link.url.as_str(), link.title.as_str()), (TIEBA_URL, "其实HDR比dlss5重要"));
    assert_eq!(canonical_url(&link.url).expect("parses"), TIEBA_URL);
    assert_eq!(parse_tieba_link(&share(SHARE_URL, "5")).expect("parses"), None);
    assert_eq!(parse_article_link(&message).expect("parses"), None);
    assert_eq!(
        parse_tieba_link(&share("https://tieba.baidu.com/f?kw=x", "5")).expect("parses"),
        None
    );
}

fn nga_first_floor() -> Dict {
    object(
        r#"{
            "lou": 0,
            "subject": "",
            "postdatetimestamp": 1791654000,
            "content": "text<br/>[img]./mon_202510/11/A.jpg.medium.jpg[/img][img]https://img.nga.178.com/attachments/mon_202510/11/B.jpg[/img][img]./mon_202510/11/A.jpg[/img]",
            "attachs": {"0": {"attachurl": "mon_202510/11/C.jpg", "type": "img"}, "1": {"attachurl": "mon_202510/11/x.zip", "type": "zip"}}
        }"#,
    )
}

// py: tests/test_wechat.py::test_nga_pictures_are_the_uploads_behind_the_first_floor_s_renditions
#[test]
fn nga_pictures_are_the_uploads_behind_the_first_floor_s_renditions() {
    let attach = |name: &str| format!("{NGA_ATTACH}{name}.jpg");
    assert_eq!(original_url("./mon_202510/11/A.jpg.medium.jpg").expect("parses"), Some(attach("A")));
    assert_eq!(
        original_url("./mon_202510/11/A.png.thumb_s.jpg").expect("parses").as_deref(),
        Some("https://img.nga.cn/attachments/mon_202510/11/A.png")
    );
    // The retired host of older posts is read from the one that still resolves.
    assert_eq!(
        original_url("http://img4.nga.178.com/attachments/mon_202510/11/A.jpg").expect("parses"),
        Some(attach("A"))
    );
    assert_eq!(original_url("javascript:alert(1)").expect("parses"), None);
    assert_eq!(floor_images(&nga_first_floor()).expect("parses"), ["A", "B", "C"].map(attach));
}

const NGA_CHECK_URL: &str = "http://bbs.nga.cn/nuke.php";

fn nga_cookies(login: &[(&str, &str)]) -> Vec<StoredCookie> {
    login
        .iter()
        .map(|(name, value)| StoredCookie {
            name: (*name).to_owned(),
            value: (*value).to_owned(),
            domain: ".nga.cn".to_owned(),
            path: "/".to_owned(),
            expires: None,
        })
        .collect()
}

const NGA_LOGIN: [(&str, &str); 2] = [("ngaPassportUid", "42"), ("ngaPassportCid", "session-cid")];

// py: tests/test_wechat.py::test_an_nga_login_is_verified_against_the_notification_list
#[tokio::test]
async fn an_nga_login_is_verified_against_the_notification_list() {
    let web = FakeWeb::start(|_| json_reply(200, r#"{"data": {"0": []}, "time": 1}"#)).await;
    let check = verify_session_at(NGA_CHECK_URL, Some(&web.proxy), &nga_cookies(&NGA_LOGIN)).await;
    assert_eq!((check.ok, check.account.as_str()), (true, "42"));
    let seen = web.seen();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].query().split('&').any(|pair| pair == "__lib=noti"), "{}", seen[0].url);
    assert_eq!(seen[0].header("Cookie"), Some("ngaPassportUid=42; ngaPassportCid=session-cid"));
}

// py: tests/test_wechat.py::test_nga_turning_the_session_away_is_not_a_login
#[tokio::test]
async fn nga_turning_the_session_away_is_not_a_login() {
    let web = FakeWeb::start(|_| json_reply(200, r#"{"error": ["你必须先登录论坛"], "time": 1}"#)).await;
    let check = verify_session_at(NGA_CHECK_URL, Some(&web.proxy), &nga_cookies(&NGA_LOGIN)).await;
    assert!(!check.ok);
    assert!(check.message.contains("登录"), "{}", check.message);
}

// py: tests/test_wechat.py::test_an_nga_login_needs_both_passport_cookies
#[tokio::test]
async fn an_nga_login_needs_both_passport_cookies() {
    let web = FakeWeb::start(|_| Reply::status(500)).await;
    let check = verify_session_at(NGA_CHECK_URL, Some(&web.proxy), &nga_cookies(&[("ngaPassportUid", "42")])).await;
    assert!(!check.ok);
    assert!(web.seen().is_empty(), "never reached");
}

// py: tests/test_wechat.py::test_nga_answering_oddly_does_not_hold_the_login_up
#[tokio::test]
async fn nga_answering_oddly_does_not_hold_the_login_up() {
    let web = FakeWeb::start(|_| text_reply(502, "<html>bad gateway</html>")).await;
    let check = verify_session_at(NGA_CHECK_URL, Some(&web.proxy), &nga_cookies(&NGA_LOGIN)).await;
    assert_eq!((check.ok, check.account.as_str()), (true, "42"));
}

// ---------- beyond Python's tests ----------

/// A signed-in NGA read records the session as working, and a refused one as not.
#[tokio::test]
async fn an_nga_read_records_whether_the_session_worked() {
    let thread = r#"{"data": {"__R": {"0": {"lou": 0, "content": "[img]./mon_202510/11/A.jpg[/img]"}}, "__T": {"subject": "S"}, "__F": {"name": "F"}}}"#;
    let web = FakeWeb::start(move |seen| {
        if seen.header("cookie").is_some_and(|cookie| cookie.contains("stale")) {
            json_reply(200, r#"{"error": ["1:未登录"]}"#)
        } else {
            json_reply(200, thread)
        }
    })
    .await;
    let settings = Arc::new(SettingsStore::pinned(fav_db::settings_store::default_settings()));
    let link = link_for("https://bbs.nga.cn/read.php?tid=1");
    for (cid, worked) in [("session-cid", true), ("stale", false)] {
        let sessions = Arc::new(Sessions {
            login: Some(nga_cookies(&[("ngaPassportUid", "42"), ("ngaPassportCid", cid)])),
            checks: Mutex::new(Vec::new()),
        });
        let links = PostLinks::new(client(&web), Arc::clone(&settings), Arc::clone(&sessions) as Arc<dyn SessionStore>);
        let result = links.fetch_page(&link).await;
        let checks = sessions.checks.lock().expect("lock").clone();
        if worked {
            let (page, _) = result.expect("read");
            assert_eq!((page.title.as_str(), page.account.as_str()), ("S", "F"));
            assert_eq!(checks, [(true, String::new())]);
        } else {
            let error = link_error(result);
            assert_eq!(error.needs, Needs::NgaLogin);
            assert_eq!(checks.len(), 1);
            assert!(!checks[0].0 && checks[0].1.contains("未登录"), "{checks:?}");
        }
    }
}

// ---------- transport failures (the goldens leave them to these tests) ----------

/// A WeChat page whose server hangs up is a post link error naming the canonical URL.
#[tokio::test]
async fn a_wechat_page_that_does_not_answer_could_not_be_read() {
    let web = FakeWeb::start(|_| Reply::Close).await;
    let link = ArticleLink::new("http://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=b&chksm=c", "", "");
    let error = link_error(links(&web, "").fetch_page(&link).await);
    assert!(
        error
            .message
            .starts_with("Could not read https://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=b: "),
        "{}",
        error.message
    );
    assert_eq!((error.needs, error.captcha), (Needs::Nothing, false));
}

/// TikHub hanging up on a Zhihu lookup names the post and httpx's class, and is an outage
/// rather than a post that could not be read (D185).
#[tokio::test]
async fn a_tikhub_lookup_that_does_not_answer_names_the_post() {
    let web = FakeWeb::start(|_| Reply::Close).await;
    let link = ZhihuLink::new(ZhihuKind::Answer, "1");
    let error = zhihu_post::fetch_post(&client(&web), &link, "http://api.tikhub.io", "key")
        .await
        .expect_err("fails");
    let ZhihuError::Outage(message) = error else {
        panic!("expected an outage, got {error:?}");
    };
    let prefix = format!("TikHub did not answer for {}: RemoteProtocolError: ", link.url());
    assert!(message.starts_with(&prefix), "{message}");
}

/// Zhihu's lens hanging up is no play URL: the video falls back to the one in the post.
#[tokio::test]
async fn a_lens_lookup_that_does_not_answer_falls_back() {
    let web = FakeWeb::start(|_| Reply::Close).await;
    let video = ZhihuVideo {
        id: "1".to_owned(),
        fallback_url: "https://vdn3.vzuu.com/HD/p.mp4".to_owned(),
        cover_url: String::new(),
    };
    let url = zhihu_post::resolve_video(&client(&web), &video).await.expect("no error");
    assert_eq!(url, "https://vdn3.vzuu.com/HD/p.mp4");
}

/// NGA hanging up on the session check says it was unreachable, with httpx's class.
#[tokio::test]
async fn an_nga_check_that_does_not_answer_is_unreachable() {
    let web = FakeWeb::start(|_| Reply::Close).await;
    let check = verify_session_at(NGA_CHECK_URL, Some(&web.proxy), &nga_cookies(&NGA_LOGIN)).await;
    assert!(!check.ok);
    assert_eq!(check.message, "NGA unreachable: RemoteProtocolError");
}

/// The settings page's key check reports a TikHub that hangs up as unreachable.
#[tokio::test]
async fn a_tikhub_key_check_that_does_not_answer_is_unreachable() {
    let web = FakeWeb::start(|_| Reply::Close).await;
    let cfg = fav_core::settings::TikHub {
        api_url: "http://api.tikhub.dev".to_owned(),
        token: "key".to_owned(),
    };
    let error = crate::tikhub::check_account_via(&cfg, Some(&web.proxy)).await.expect_err("fails");
    assert_eq!(error.code, "tikhub_unreachable");
    assert!(
        error.message.starts_with("TikHub did not answer: RemoteProtocolError: "),
        "{}",
        error.message
    );
}

/// A video download that breaks off, before or during the body, is no video: it logs
/// why and leaves no partial file behind.
#[test]
fn a_video_download_that_breaks_off_leaves_nothing_behind() {
    for (name, reply) in [
        ("hung up", Reply::Close),
        (
            "cut short",
            Reply::Truncated {
                status: 200,
                sent: b"\0\0\0\x18ftypmp42".to_vec(),
                length: 4096,
            },
        ),
    ] {
        let folder = tempfile::tempdir().expect("temporary folder");
        let partial = folder.path().join("video.mp4.part");
        std::fs::write(&partial, b"left from an earlier try").expect("written");
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let mut downloaded = None;
        let records = fav_core::logging::capture(|| {
            downloaded = Some(runtime.block_on(async {
                let web = FakeWeb::start(move |_| reply.clone()).await;
                client(&web)
                    .fetch_video(&crate::wechat_article::PostVideo::new("http://v.example/v.mp4", ""), &partial)
                    .await
            }));
        });
        assert_eq!(downloaded.expect("ran"), Ok(false), "{name}");
        assert!(!partial.exists(), "{name}: the partial file is gone");
        let lines: Vec<&str> = records.iter().map(|record| record.message.as_str()).collect();
        assert!(
            lines.len() == 1 && lines[0].starts_with("Post video http://v.example/v.mp4 failed: "),
            "{name}: {lines:?}"
        );
    }
}
