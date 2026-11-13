use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::{PageState, ReadTimings, ZhihuBrowser, classify, login_check, profile_dir, profile_lock_name};
use crate::testutil::{Reply, chromium, serve};

fn probe(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

#[test]
fn the_profile_sits_next_to_the_rednote_one() {
    assert_eq!(profile_dir(Path::new("data/rednote-profile")), Path::new("data/zhihu-profile"));
    assert_eq!(
        profile_dir(Path::new("/app/data/rednote-profile")),
        Path::new("/app/data/zhihu-profile")
    );
    assert!(profile_lock_name(Path::new("/nonexistent-root/zhihu-profile")).starts_with("zhihu-profile:/"));
}

#[test]
fn a_login_is_named_by_the_account_the_page_reports() {
    let check = login_check(&probe(json!({"status": 200, "name": "某人", "url_token": "some-one"})));
    assert!(check.ok);
    assert_eq!(check.account, "某人 (some-one)");
    // A page that could not be asked leaves the session cookie to speak.
    let check = login_check(&probe(json!({"status": 0, "origin": "zhuanlan.zhihu.com"})));
    assert!(check.ok);
    assert_eq!(check.account, "");
    let check = login_check(&probe(json!({"status": 401})));
    assert!(!check.ok);
    assert_eq!(check.message, "Zhihu refused the new session (HTTP 401).");
}

#[test]
fn a_page_is_classified_by_what_it_shows() {
    let post = json!({"content": "<p>hi</p>"});
    assert_eq!(
        classify(&probe(
            json!({"href": "https://www.zhihu.com/question/1/answer/2", "post": post.clone()})
        )),
        PageState::Post(post)
    );
    assert_eq!(
        classify(&probe(
            json!({"href": "https://www.zhihu.com/signin?next=%2Fanswer%2F2", "post": null, "has_initial_data": true})
        )),
        PageState::SignedOut
    );
    assert_eq!(
        classify(&probe(json!({"href": "https://www.zhihu.com/answer/2", "api_status": 401}))),
        PageState::SignedOut
    );
    assert_eq!(
        classify(&probe(
            json!({"href": "https://www.zhihu.com/answer/2", "api_status": 403, "api_error": "请先登录"})
        )),
        PageState::SignedOut
    );
    assert_eq!(
        classify(&probe(
            json!({"href": "https://www.zhihu.com/account/unhuman?type=U4E3Z1&need_login=true", "post": null})
        )),
        PageState::Challenge("https://www.zhihu.com/account/unhuman?type=U4E3Z1&need_login=true".to_owned())
    );
    assert!(matches!(
        classify(&probe(
            json!({"href": "https://www.zhihu.com/answer/2", "text": "请完成验证后继续访问"})
        )),
        PageState::Challenge(_)
    ));
    assert_eq!(
        classify(&probe(json!({
            "href": "https://www.zhihu.com/answer/2", "title": "知乎", "text": "你似乎来到了没有知识存在的荒原",
            "has_initial_data": true,
        }))),
        PageState::Missing("\"知乎\" at https://www.zhihu.com/answer/2".to_owned())
    );
    assert_eq!(
        classify(&probe(
            json!({"href": "https://www.zhihu.com/pin/3", "title": "", "api_status": 404, "api_error": "资源不存在"})
        )),
        PageState::Missing("\"\" at https://www.zhihu.com/pin/3, no initialData, API HTTP 404, 资源不存在".to_owned())
    );
    // An empty post is no post, and a page that shows nothing known is still loading.
    assert_eq!(
        classify(&probe(
            json!({"href": "https://www.zhihu.com/answer/2", "title": "知乎", "post": {}})
        )),
        PageState::Unknown("\"知乎\" at https://www.zhihu.com/answer/2, no initialData".to_owned())
    );
}

/// An answer's page as Zhihu's front end renders it: the store in `js-initialData`, the
/// author given as a reference into `users`, keys in camelCase.
const ANSWER_PAGE: &str = r#"<!doctype html><html><head><title>问题 - 知乎</title></head><body>
<div>answer</div>
<script id="js-initialData" type="text/json">{"initialState":{"entities":{
"users":{"some-one":{"name":"某人","urlToken":"some-one"}},
"questions":{"11":{"title":"一个问题","id":11}},
"answers":{"22":{"id":22,"content":"<p><img data-original=\"https://pic1.zhimg.com/v2-a_r.jpg\"></p>","question":11,"author":"some-one","createdTime":1700000000,"thumbnailInfo":{"thumbnails":[{"videoId":"9"}]}}},
"pins":{}}}}</script></body></html>"#;

fn fast() -> ReadTimings {
    ReadTimings {
        goto: Duration::from_secs(20),
        settle: Duration::from_secs(2),
        poll: Duration::from_millis(100),
        hints_retry: Duration::from_millis(50),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_post_is_read_from_the_page_store_in_api_keys() {
    let Some(executable) = chromium() else { return };
    let (addr, _) = serve(|request| {
        if request.starts_with("get /question/11/answer/22") {
            Reply::html(ANSWER_PAGE)
        } else {
            Reply::html("<html><body>neutral</body></html>")
        }
    })
    .await;
    let profile = tempfile::tempdir().unwrap();
    let browser = ZhihuBrowser::new(executable, profile.path(), "", true)
        .unwrap()
        .with_hints_url(&format!("http://{addr}/neutral"))
        .with_timings(fast());

    let state = browser
        .read(&format!("http://{addr}/question/11/answer/22"), "answer", "22")
        .await
        .expect("read");

    let PageState::Post(post) = state else {
        panic!("no post: {state:?}")
    };
    assert_eq!(
        post["content"],
        json!("<p><img data-original=\"https://pic1.zhimg.com/v2-a_r.jpg\"></p>")
    );
    assert_eq!(post["question"], json!({"title": "一个问题", "id": 11}));
    assert_eq!(post["author"], json!({"name": "某人", "url_token": "some-one"}));
    assert_eq!(post["created_time"], json!(1_700_000_000));
    assert_eq!(post["thumbnail_info"], json!({"thumbnails": [{"video_id": "9"}]}));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_without_the_post_says_what_it_shows() {
    let Some(executable) = chromium() else { return };
    let (addr, _) = serve(|request| {
        if request.starts_with("get /pin/3") {
            Reply::html("<html><head><title>知乎</title></head><body>你似乎来到了没有知识存在的荒原</body></html>")
        } else {
            Reply::html("<html><body>neutral</body></html>")
        }
    })
    .await;
    let profile = tempfile::tempdir().unwrap();
    let browser = ZhihuBrowser::new(executable, profile.path(), "", true)
        .unwrap()
        .with_hints_url(&format!("http://{addr}/neutral"))
        .with_timings(fast());

    let state = browser.read(&format!("http://{addr}/pin/3"), "pin", "3").await.expect("read");

    assert!(
        matches!(&state, PageState::Missing(summary) if summary.starts_with(&format!("\"知乎\" at http://{addr}/pin/3"))),
        "{state:?}"
    );
}
