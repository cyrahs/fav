use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fav_core::pyjson::{self, Dict, DumpOptions, Value};
use fav_core::settings::{Defaults as _, Settings};
use fav_core::validate::PyPath;
use fav_db::{Db, SettingsStore};
use fav_net::client::Redirect;
use fav_net::retry::RecordingSleeper;
use fav_net::{ClientOptions, ErrorClass, HttpClient};
use fav_runtime::notifications::Outbox;
use serde_json::json;

use super::{
    ImageError, JandanImage, Run, TypeStats, build_fav_request, build_image_download_candidates, build_image_request_headers,
    build_sinaimg_shard_candidates, client_options, decrypt_data_field, detect_deleted_placeholder, encrypt_data_field, extract_fav_items,
    extract_last_pic_date, extract_pic_images, infer_image_extension, should_mark_unavailable_http, should_retry_jandan_api_exception,
    should_retry_jandan_api_status, summary_notification, zulu_to_offset,
};
use crate::testutil::{self, FakeWeb, Reply};

const PIC_A_ID: i64 = 6_095_815;
const PIC_B_ID: i64 = 6_095_816;

fn loads(text: &str) -> Value {
    pyjson::loads(text).expect("JSON")
}

fn compact(value: &Value) -> String {
    pyjson::dumps(value, &DumpOptions::compact()).expect("dumps")
}

/// A database that is never reached: for tests that only touch the web.
fn unreachable_db() -> Db {
    Db::connect_lazy("postgresql://nobody@127.0.0.1:1/none").expect("lazy pool")
}

struct Harness {
    run: Run,
    sleeps: RecordingSleeper,
}

fn harness(web: &FakeWeb, db: Db, configure: impl FnOnce(&mut fav_core::settings::Jandan)) -> Harness {
    let mut settings = Settings::defaults();
    configure(&mut settings.web.jandan);
    let cfg = settings.web.jandan.clone();
    let store = Arc::new(SettingsStore::pinned(settings));
    let options = ClientOptions {
        proxy: Some(web.proxy.clone()),
        timeout: Duration::from_millis(500),
        ..client_options()
    };
    let sleeps = RecordingSleeper::new();
    let run = Run {
        cfg,
        http: HttpClient::new(&options).expect("client"),
        outbox: Outbox::new(db.clone(), store),
        db,
        sleeper: sleeps.sleeper(),
    };
    Harness { run, sleeps }
}

fn sinaimg_image() -> JandanImage {
    JandanImage {
        fav_type: 1,
        pic_id: 1,
        content_index: 1,
        content_url: "http://wx4.sinaimg.cn/large/abc.jpg".to_owned(),
        content_md5: None,
        author: "tester".to_owned(),
        post_date: None,
        image_type: Some("jpg".to_owned()),
    }
}

fn api_reply(payload: &serde_json::Value) -> Reply {
    let encrypted = encrypt_data_field(&compact(&loads(&payload.to_string())));
    Reply::json(200, &json!({"code": 0, "data": encrypted}))
}

// py: tests/test_jandan.py::test_encrypt_and_decrypt_data_field_roundtrip
#[test]
fn encrypt_and_decrypt_data_field_roundtrip() {
    let plain = compact(&build_fav_request(42920, 1, 45, "get", None));
    let encrypted = encrypt_data_field(&plain);
    assert_eq!(decrypt_data_field(&encrypted).expect("decrypts"), plain);
}

// py: tests/test_jandan.py::test_build_fav_request_supports_cursor
#[test]
fn build_fav_request_supports_cursor() {
    let payload = build_fav_request(42920, 1, 45, "get", Some("2026-02-13T18:06:39.000-0800"));
    assert_eq!(
        compact(&payload),
        r#"{"fav":{"favType":"1","favNumLimit":45,"actionType":"get","userID":42920,"favStartDate":"2026-02-13T18:06:39.000-0800"}}"#
    );
}

// py: tests/test_jandan.py::test_zulu_to_offset_matches_appendix_example
#[test]
fn zulu_to_offset_matches_appendix_example() {
    assert_eq!(
        zulu_to_offset("2026-02-14T02:06:39.000Z").expect("parses"),
        "2026-02-13T18:06:39.000-0800"
    );
}

// py: tests/test_jandan.py::test_extract_pic_images_keeps_only_image_contents
#[test]
fn extract_pic_images_keeps_only_image_contents() {
    let pics = loads(&format!(
        r#"[
            {{"id": {PIC_A_ID}, "date": "2026-02-25T02:14:46.000Z", "author": "author-a", "contents": [
                {{"contentType": "image", "content": "http://img.wangmoyu.com/mw600/demo-a.jpg", "md5": "abc", "imageType": "jpeg"}},
                {{"contentType": "video", "content": "http://img.wangmoyu.com/demo.mp4"}}
            ]}},
            {{"id": "{PIC_B_ID}", "date": "2026-02-25T02:16:46.000Z", "author": "", "contents": [
                {{"contentType": "image", "content": "//img.wangmoyu.com/mw600/demo-b.png", "imageType": "png"}}
            ]}}
        ]"#
    ));
    let Value::List(pics) = pics else { panic!("list") };
    let images = extract_pic_images(1, &pics).expect("extracts");
    assert_eq!(images.len(), 2);
    assert_eq!(images[0].pic_id, PIC_A_ID);
    assert_eq!(images[0].content_index, 1);
    assert_eq!(images[0].content_url, "http://img.wangmoyu.com/mw600/demo-a.jpg");
    assert_eq!(images[0].author, "author-a");
    assert_eq!(images[0].image_type.as_deref(), Some("jpeg"));
    assert_eq!(images[1].pic_id, PIC_B_ID);
    assert_eq!(images[1].content_url, "https://img.wangmoyu.com/mw600/demo-b.png");
    assert_eq!(images[1].author, "unknown");
}

// py: tests/test_jandan.py::test_extract_last_pic_date_returns_last_valid_date
#[test]
fn extract_last_pic_date_returns_last_valid_date() {
    let Value::List(pics) =
        loads(r#"[{"id": 1, "date": "2026-02-14T10:00:00.000Z"}, {"id": 2, "date": ""}, {"id": 3, "date": "2026-02-14T02:06:39.000Z"}]"#)
    else {
        panic!("list")
    };
    assert_eq!(extract_last_pic_date(&pics).as_deref(), Some("2026-02-14T02:06:39.000Z"));
}

fn dict(text: &str) -> Dict {
    match loads(text) {
        Value::Dict(dict) => dict,
        other => panic!("not an object: {other:?}"),
    }
}

// py: tests/test_jandan.py::test_extract_fav_items_by_fav_type_key
#[test]
fn extract_fav_items_by_fav_type_key() {
    let items = extract_fav_items(6, &dict(r#"{"nzs": [{"id": 1}, {"id": 2}]}"#));
    assert_eq!(compact(&Value::List(items)), r#"[{"id":1},{"id":2}]"#);
}

// py: tests/test_jandan.py::test_extract_fav_items_fallbacks_to_any_list
#[test]
fn extract_fav_items_fallbacks_to_any_list() {
    let items = extract_fav_items(1, &dict(r#"{"unexpected_key": [{"id": 10}, {"id": 20}], "meta": {"foo": "bar"}}"#));
    assert_eq!(compact(&Value::List(items)), r#"[{"id":10},{"id":20}]"#);
}

// py: tests/test_jandan.py::test_infer_image_extension_prefers_image_type_then_url_suffix
#[test]
fn infer_image_extension_prefers_image_type_then_url_suffix() {
    assert_eq!(
        infer_image_extension("https://img.example.com/path/abc.unknown", Some("jpeg")).unwrap(),
        "jpg"
    );
    assert_eq!(
        infer_image_extension("https://img.example.com/path/abc.gif?token=1", None).unwrap(),
        "gif"
    );
    assert_eq!(infer_image_extension("https://img.example.com/path/no-ext", None).unwrap(), "jpg");
}

fn header_map(pairs: &[(&'static str, &str)]) -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(*name, value.parse().expect("header value"));
    }
    headers
}

// py: tests/test_jandan.py::test_detect_deleted_placeholder_by_redirect_history
#[test]
fn detect_deleted_placeholder_by_redirect_history() {
    let source = url::Url::parse("http://wx1.sinaimg.cn/mw600/66b3de17ly1i3z3e7uk5uj216o1kw7g0.jpg").unwrap();
    let redirect = Redirect {
        status: 301,
        url: source,
        headers: header_map(&[
            ("location", "//wx1.sinaimg.cn/images/default_d_h_mw600.gif#101"),
            ("x-image-errno", "101"),
        ]),
    };
    let final_url = url::Url::parse("http://wx1.sinaimg.cn/images/default_d_h_mw600.gif#101").unwrap();
    let (is_deleted, reason) = detect_deleted_placeholder(&final_url, std::slice::from_ref(&redirect));
    assert!(is_deleted);
    assert!(reason.expect("reason").contains("placeholder"));

    // The redirect alone is enough when the final URL hides the marker.
    let elsewhere = url::Url::parse("http://wx1.sinaimg.cn/other.gif").unwrap();
    let (is_deleted, reason) = detect_deleted_placeholder(&elsewhere, &[redirect]);
    assert!(is_deleted);
    assert_eq!(
        reason.as_deref(),
        Some("redirected to placeholder: //wx1.sinaimg.cn/images/default_d_h_mw600.gif#101 (errno=101)")
    );
}

// py: tests/test_jandan.py::test_detect_deleted_placeholder_returns_false_for_normal_image
#[test]
fn detect_deleted_placeholder_returns_false_for_normal_image() {
    let url = url::Url::parse("https://img.wangmoyu.com/mw600/example.jpg").unwrap();
    assert_eq!(detect_deleted_placeholder(&url, &[]), (false, None));
}

// py: tests/test_jandan.py::test_should_mark_unavailable_http_supports_sinaimg_403
#[test]
fn should_mark_unavailable_http_supports_sinaimg_403() {
    assert!(should_mark_unavailable_http(404, Some("example.com")));
    assert!(should_mark_unavailable_http(410, Some("example.com")));
    assert!(should_mark_unavailable_http(403, Some("tva4.sinaimg.cn")));
    assert!(!should_mark_unavailable_http(403, Some("img.wangmoyu.com")));
    assert!(!should_mark_unavailable_http(429, Some("tva4.sinaimg.cn")));
}

// py: tests/test_jandan.py::test_should_retry_jandan_api_status_only_transient_errors
#[test]
fn should_retry_jandan_api_status_only_transient_errors() {
    for status in [429, 502, 503, 504] {
        assert!(should_retry_jandan_api_status(status));
    }
    for status in [400, 404] {
        assert!(!should_retry_jandan_api_status(status));
    }
}

// py: tests/test_jandan.py::test_should_retry_jandan_api_exception_only_transient_network_errors
#[test]
fn should_retry_jandan_api_exception_only_transient_network_errors() {
    for class in [
        ErrorClass::ConnectTimeout,
        ErrorClass::ReadTimeout,
        ErrorClass::ConnectError,
        ErrorClass::RemoteProtocolError,
    ] {
        assert!(should_retry_jandan_api_exception(class), "{class:?}");
    }
    // Python's `ValueError('bad payload')` is not a transport error at all; the closest
    // transport classes that are not retried:
    for class in [ErrorClass::ReadError, ErrorClass::LocalProtocolError, ErrorClass::DecodingError] {
        assert!(!should_retry_jandan_api_exception(class), "{class:?}");
    }
}

// py: tests/test_jandan.py::test_build_image_download_candidates_prefers_large_for_sinaimg_mw600
#[test]
fn build_image_download_candidates_prefers_large_for_sinaimg_mw600() {
    assert_eq!(
        build_image_download_candidates("http://wx1.sinaimg.cn/mw600/abc.jpg").unwrap(),
        ["http://wx1.sinaimg.cn/large/abc.jpg", "http://wx1.sinaimg.cn/mw600/abc.jpg"]
    );
}

// py: tests/test_jandan.py::test_build_image_download_candidates_prefers_large_for_wangmoyu_mw600
#[test]
fn build_image_download_candidates_prefers_large_for_wangmoyu_mw600() {
    assert_eq!(
        build_image_download_candidates("http://img.wangmoyu.com/mw600/abc.png").unwrap(),
        ["http://img.wangmoyu.com/large/abc.png", "http://img.wangmoyu.com/mw600/abc.png"]
    );
}

// py: tests/test_jandan.py::test_build_image_download_candidates_prefers_large_for_wangmoyu_mw1024
#[test]
fn build_image_download_candidates_prefers_large_for_wangmoyu_mw1024() {
    assert_eq!(
        build_image_download_candidates("http://img.wangmoyu.com/mw1024/abc.png").unwrap(),
        ["http://img.wangmoyu.com/large/abc.png", "http://img.wangmoyu.com/mw1024/abc.png"]
    );
}

// py: tests/test_jandan.py::test_build_image_download_candidates_preserves_query_for_mw1024
#[test]
fn build_image_download_candidates_preserves_query_for_mw1024() {
    assert_eq!(
        build_image_download_candidates("http://wx4.sinaimg.cn/mw1024/abc.jpg?x=1&y=2").unwrap(),
        [
            "http://wx4.sinaimg.cn/large/abc.jpg?x=1&y=2",
            "http://wx4.sinaimg.cn/mw1024/abc.jpg?x=1&y=2"
        ]
    );
}

// py: tests/test_jandan.py::test_build_image_download_candidates_keeps_single_when_not_mw600
#[test]
fn build_image_download_candidates_keeps_single_when_not_mw600() {
    assert_eq!(
        build_image_download_candidates("http://wx1.sinaimg.cn/large/abc.jpg").unwrap(),
        ["http://wx1.sinaimg.cn/large/abc.jpg"]
    );
}

// py: tests/test_jandan.py::test_build_image_download_candidates_keeps_single_for_unknown_host
#[test]
fn build_image_download_candidates_keeps_single_for_unknown_host() {
    assert_eq!(
        build_image_download_candidates("https://cdn.example.com/mw600/abc.jpg").unwrap(),
        ["https://cdn.example.com/mw600/abc.jpg"]
    );
}

fn referer(headers: &[(String, String)]) -> Option<&str> {
    headers.iter().find(|(name, _)| name == "Referer").map(|(_, value)| value.as_str())
}

// py: tests/test_jandan.py::test_build_image_request_headers_adds_referer_for_wangmoyu
#[test]
fn build_image_request_headers_adds_referer_for_wangmoyu() {
    let headers = build_image_request_headers("https://img.wangmoyu.com/mw1024/abc.jpg").unwrap();
    assert_eq!(referer(&headers), Some("https://jandan.net/"));
}

// py: tests/test_jandan.py::test_build_image_request_headers_omits_referer_for_other_hosts
#[test]
fn build_image_request_headers_omits_referer_for_other_hosts() {
    assert_eq!(
        referer(&build_image_request_headers("http://wx4.sinaimg.cn/large/abc.jpg").unwrap()),
        None
    );
    assert_eq!(
        referer(&build_image_request_headers("https://cdn.example.com/large/abc.jpg").unwrap()),
        None
    );
}

// py: tests/test_jandan.py::test_build_sinaimg_shard_candidates_rotates_siblings
#[test]
fn build_sinaimg_shard_candidates_rotates_siblings() {
    assert_eq!(
        build_sinaimg_shard_candidates("http://wx4.sinaimg.cn/large/abc.jpg?x=1").unwrap(),
        [
            "http://wx1.sinaimg.cn/large/abc.jpg?x=1",
            "http://wx2.sinaimg.cn/large/abc.jpg?x=1",
            "http://wx3.sinaimg.cn/large/abc.jpg?x=1",
        ]
    );
}

// py: tests/test_jandan.py::test_build_sinaimg_shard_candidates_keeps_multi_letter_prefix
#[test]
fn build_sinaimg_shard_candidates_keeps_multi_letter_prefix() {
    assert_eq!(
        build_sinaimg_shard_candidates("https://tvax3.sinaimg.cn/large/abc.jpg").unwrap(),
        [
            "https://tvax1.sinaimg.cn/large/abc.jpg",
            "https://tvax2.sinaimg.cn/large/abc.jpg",
            "https://tvax4.sinaimg.cn/large/abc.jpg",
        ]
    );
}

// py: tests/test_jandan.py::test_build_sinaimg_shard_candidates_ignores_other_hosts
#[test]
fn build_sinaimg_shard_candidates_ignores_other_hosts() {
    assert!(
        build_sinaimg_shard_candidates("https://img.wangmoyu.com/large/abc.jpg")
            .unwrap()
            .is_empty()
    );
    assert!(
        build_sinaimg_shard_candidates("https://sinaimg.cn/large/abc.jpg")
            .unwrap()
            .is_empty()
    );
}

fn path_setting(dir: &Path) -> PyPath {
    PyPath::new(dir.to_str().expect("UTF-8 path"))
}

// py: tests/test_jandan.py::test_download_image_recovers_forbidden_sinaimg_from_sibling_shard
#[tokio::test]
async fn download_image_recovers_forbidden_sinaimg_from_sibling_shard() {
    let tmp = tempfile::tempdir().unwrap();
    let web = FakeWeb::start(|seen| {
        if seen.host() == "wx2.sinaimg.cn" {
            Reply::bytes(200, "image-bytes")
        } else {
            Reply::status(403)
        }
    })
    .await;
    let harness = harness(&web, unreachable_db(), |cfg| cfg.path = path_setting(tmp.path()));
    let dst = harness.run.download_image(&sinaimg_image()).await.expect("downloads");
    assert_eq!(
        web.urls(),
        [
            "http://wx4.sinaimg.cn/large/abc.jpg",
            "http://wx1.sinaimg.cn/large/abc.jpg",
            "http://wx2.sinaimg.cn/large/abc.jpg",
        ]
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"image-bytes");
    // Nothing but the image is left behind in the folder.
    let names: Vec<_> = std::fs::read_dir(dst.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, [dst.file_name().unwrap().to_owned()]);
}

// py: tests/test_jandan.py::test_download_image_reraises_when_every_sinaimg_shard_is_forbidden
#[tokio::test]
async fn download_image_reraises_when_every_sinaimg_shard_is_forbidden() {
    let tmp = tempfile::tempdir().unwrap();
    let web = FakeWeb::start(|_| Reply::status(403)).await;
    let harness = harness(&web, unreachable_db(), |cfg| cfg.path = path_setting(tmp.path()));
    let error = harness.run.download_image(&sinaimg_image()).await.expect_err("forbidden");
    assert_eq!(web.urls().len(), 4);
    let ImageError::Status { error, host } = error else {
        panic!("not a status error: {error:?}")
    };
    assert_eq!(error.status, 403);
    assert_eq!(host.as_deref(), Some("wx4.sinaimg.cn"));
    assert!(should_mark_unavailable_http(403, Some("wx4.sinaimg.cn")));
}

// py: tests/test_jandan.py::test_download_image_skips_shard_fallback_for_404
#[tokio::test]
async fn download_image_skips_shard_fallback_for_404() {
    let tmp = tempfile::tempdir().unwrap();
    let web = FakeWeb::start(|_| Reply::status(404)).await;
    let harness = harness(&web, unreachable_db(), |cfg| cfg.path = path_setting(tmp.path()));
    let error = harness.run.download_image(&sinaimg_image()).await.expect_err("missing");
    assert!(
        matches!(error, ImageError::Status { ref error, .. } if error.status == 404),
        "{error:?}"
    );
    assert_eq!(web.urls(), ["http://wx4.sinaimg.cn/large/abc.jpg"]);
}

// py: tests/test_jandan.py::test_download_image_sends_referer_to_wangmoyu
#[tokio::test]
async fn download_image_sends_referer_to_wangmoyu() {
    let tmp = tempfile::tempdir().unwrap();
    let web = FakeWeb::start(|_| Reply::bytes(200, "image-bytes")).await;
    let harness = harness(&web, unreachable_db(), |cfg| cfg.path = path_setting(tmp.path()));
    let image = JandanImage {
        pic_id: 2,
        content_url: "http://img.wangmoyu.com/large/abc.jpg".to_owned(),
        ..sinaimg_image()
    };
    harness.run.download_image(&image).await.expect("downloads");
    let referers: Vec<_> = web.seen().iter().map(|seen| seen.header("referer").map(str::to_owned)).collect();
    assert_eq!(referers, [Some("https://jandan.net/".to_owned())]);
}

fn api_settings(cfg: &mut fav_core::settings::Jandan) {
    cfg.api_url = "http://i.jandan.net/api".to_owned();
    cfg.user_id = 42920;
    cfg.fav_num_limit = 45;
}

#[tokio::test]
async fn fetch_fav_page_does_not_retry_a_reset() {
    // httpx's `ReadError` is not in the API's retry set.
    let web = FakeWeb::start(|_| Reply::Reset).await;
    let harness = harness(&web, unreachable_db(), api_settings);
    let error = harness.run.fetch_fav_page(1, None).await.unwrap_err();
    assert_eq!(error.class_name(), "ReadError");
    assert_eq!(web.seen().len(), 1);
    assert!(harness.sleeps.delays().is_empty());
}

// py: tests/test_jandan.py::test_fetch_fav_page_retries_connect_timeout
#[tokio::test]
async fn fetch_fav_page_retries_connect_timeout() {
    // A connect timeout cannot be produced through the local fake proxy, so the
    // predicate is checked for it directly, and the ladder end to end with an answer cut
    // short (`RemoteProtocolError`), which takes the same branch.
    assert!(should_retry_jandan_api_exception(ErrorClass::ConnectTimeout));
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let web = FakeWeb::start(move |_| {
        if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Reply::Truncated {
                status: 200,
                sent: b"{\"code\":".to_vec(),
                length: 64,
            }
        } else {
            api_reply(&json!({"fav": {"pics": [{"id": 1}]}}))
        }
    })
    .await;
    let harness = harness(&web, unreachable_db(), api_settings);
    let result = harness.run.fetch_fav_page(1, None).await.expect("page");
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(harness.sleeps.delays(), [2.0]);
    assert_eq!(compact(&Value::List(result)), r#"[{"id":1}]"#);

    // The request itself: the encrypted form body and the app's headers.
    let seen = &web.seen()[1];
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.url, "http://i.jandan.net/api");
    assert_eq!(seen.header("accept-version"), Some("1.0.0"));
    assert_eq!(seen.header("content-type"), Some("application/x-www-form-urlencoded"));
    assert_eq!(seen.header("user-agent"), Some("jian dan/2.6.0 (iPhone; iOS 26.1; Scale/3.00)"));
    let body = String::from_utf8(seen.body.clone()).unwrap();
    let data = body.strip_prefix("data=").expect("form field");
    assert_eq!(
        decrypt_data_field(data).unwrap(),
        r#"{"fav":{"favType":"1","favNumLimit":45,"actionType":"get","userID":42920}}"#
    );
}

// py: tests/test_jandan.py::test_fetch_fav_page_retries_503_status
#[tokio::test]
async fn fetch_fav_page_retries_503_status() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let web = FakeWeb::start(move |_| {
        if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Reply::status(503)
        } else {
            api_reply(&json!({"fav": {"pics": [{"id": 2}]}}))
        }
    })
    .await;
    let harness = harness(&web, unreachable_db(), api_settings);
    let result = harness.run.fetch_fav_page(1, None).await.expect("page");
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(harness.sleeps.delays(), [2.0]);
    assert_eq!(compact(&Value::List(result)), r#"[{"id":2}]"#);
}

// py: tests/test_jandan.py::test_fetch_fav_page_exhausts_extended_transient_retry_delays
#[tokio::test]
async fn fetch_fav_page_exhausts_extended_transient_retry_delays() {
    let web = FakeWeb::start(|_| Reply::Stall(Duration::from_secs(2))).await;
    let harness = harness(&web, unreachable_db(), api_settings);
    let error = harness.run.fetch_fav_page(1, None).await.expect_err("times out");
    assert_eq!(error.class_name(), "ReadTimeout");
    assert_eq!(web.seen().len(), 5);
    assert_eq!(harness.sleeps.delays(), [2.0, 5.0, 15.0, 30.0]);
}

// py: tests/test_jandan.py::test_fetch_fav_page_does_not_retry_400
#[tokio::test]
async fn fetch_fav_page_does_not_retry_400() {
    let web = FakeWeb::start(|_| Reply::status(400)).await;
    let harness = harness(&web, unreachable_db(), api_settings);
    let error = harness.run.fetch_fav_page(1, None).await.expect_err("bad request");
    assert_eq!(error.class_name(), "HTTPStatusError");
    assert_eq!(
        error.message(),
        "Client error '400 Bad Request' for url 'http://i.jandan.net/api'\n\
         For more information check: https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/400"
    );
    assert_eq!(web.seen().len(), 1);
    assert!(harness.sleeps.delays().is_empty());
}

// py: tests/test_jandan.py::test_notify_summary_enqueues_structured_payload
#[test]
fn notify_summary_enqueues_structured_payload() {
    let stats = TypeStats {
        downloaded: 3,
        pages: 2,
        api_total: 5,
        collected: 4,
        pending: 9,
    };
    let notification = summary_notification(3, 8, &[(1, stats)]);
    assert_eq!(notification.kind, "summary");
    assert_eq!(notification.source, "jandan");
    assert_eq!(notification.header, "Jandan");
    assert_eq!(notification.title, "Update completed");
    assert_eq!(notification.body, "Downloaded 3 images from 8 API images.");
    assert_eq!(
        compact(&Value::Dict(notification.payload.expect("payload"))),
        r#"{"total_downloaded":3,"total_api_images":8,"fav_type_stats":[{"fav_type":1,"page_count":2,"api_total_count":5,"collected_count":4,"downloaded_count":3}]}"#
    );
}

// ---------- beyond the Python tests: one whole run against the database ----------

fn pic(id: i64, date: &str, url: &str) -> serde_json::Value {
    json!({"id": id, "date": date, "author": "someone", "contents": [{"contentType": "image", "content": url, "imageType": "jpeg"}]})
}

#[tokio::test]
async fn update_crawls_stores_downloads_and_notifies() {
    let database = testutil::database().await;
    let tmp = tempfile::tempdir().unwrap();
    let web = FakeWeb::start(|seen| {
        if seen.host() == "i.jandan.net" {
            let body = String::from_utf8(seen.body.clone()).unwrap();
            let request = decrypt_data_field(body.strip_prefix("data=").unwrap()).unwrap();
            return if request.contains("favStartDate") {
                api_reply(&json!({"fav": {"pics": []}}))
            } else {
                api_reply(&json!({"fav": {"pics": [
                    pic(10, "2026-02-14T02:06:39.000Z", "http://img.example.com/a.jpg"),
                    pic(11, "2026-02-14T01:00:00.000Z", "http://img.example.com/gone.jpg"),
                    pic(12, "2026-02-14T00:00:00.000Z", "http://img.example.com/broken.jpg"),
                ]}}))
            };
        }
        match seen.path() {
            "/a.jpg" => Reply::bytes(200, "jpeg"),
            "/gone.jpg" => Reply::status(404),
            _ => Reply::status(500),
        }
    })
    .await;
    let harness = harness(&web, database.db().clone(), |cfg| {
        api_settings(cfg);
        cfg.path = path_setting(tmp.path());
        cfg.fav_types = vec![1];
    });
    harness.run.update().await.expect("run");

    let cursors: Vec<String> = web
        .seen()
        .iter()
        .filter(|seen| seen.host() == "i.jandan.net")
        .map(|seen| decrypt_data_field(String::from_utf8(seen.body.clone()).unwrap().strip_prefix("data=").unwrap()).unwrap())
        .collect();
    assert_eq!(cursors.len(), 2);
    assert!(
        cursors[1].contains(r#""favStartDate":"2026-02-13T16:00:00.000-0800""#),
        "{cursors:?}"
    );

    let rows: Vec<(i32, i32, i32, i32, String)> =
        sqlx::query_as("SELECT pic_id, downloaded, unavailable, failed_count, last_error FROM jandan ORDER BY pic_id")
            .fetch_all(database.db().pool())
            .await
            .unwrap();
    assert_eq!(
        rows,
        [
            (10, 1, 0, 0, String::new()),
            (11, 0, 1, 1, "http 404".to_owned()),
            (12, 0, 0, 1, "http 500".to_owned()),
        ]
    );
    let downloaded = tmp.path().join("wuliao").join("[type1]someone [10-1].jpg");
    assert_eq!(std::fs::read(&downloaded).unwrap(), b"jpeg");

    let (kind, body): (String, String) = sqlx::query_as("SELECT kind, body FROM notifications")
        .fetch_one(database.db().pool())
        .await
        .unwrap();
    assert_eq!(kind, "summary");
    assert_eq!(body, "Downloaded 1 images from 3 API images.");
    database.drop_database().await;
}

// ---------- Python's answers for the pure helpers (`rust/goldens/jandan/`) ----------

fn answer<T>(result: Result<T, fav_runtime::source::JobError>, ok: impl FnOnce(T) -> Value) -> Value {
    match result {
        Ok(value) => ok(value),
        Err(error) => Value::Str(error.formatted()),
    }
}

fn str_list(items: Vec<String>) -> Value {
    Value::List(items.into_iter().map(Value::Str).collect())
}

#[test]
fn crypto_matches_python() {
    let document = fav_core::golden::load("jandan/crypto.json");
    for case in fav_core::golden::cases(&document) {
        let expected = fav_core::golden::get(case, "result");
        if let Some(plain) = case.get("encrypt") {
            let plain = fav_core::golden::string(plain);
            let encrypted = encrypt_data_field(&plain);
            assert_eq!(&Value::Str(encrypted.clone()), expected, "encrypt {plain:?}");
            assert_eq!(decrypt_data_field(&encrypted).unwrap(), plain);
        } else {
            let text = fav_core::golden::string(fav_core::golden::get(case, "decrypt"));
            assert_eq!(&answer(decrypt_data_field(&text), Value::Str), expected, "decrypt {text:?}");
        }
    }
}

#[test]
fn zulu_to_offset_matches_python() {
    let document = fav_core::golden::load("jandan/zulu_to_offset.json");
    for case in fav_core::golden::cases(&document) {
        let date = fav_core::golden::string(fav_core::golden::get(case, "date"));
        assert_eq!(
            &answer(zulu_to_offset(&date), Value::Str),
            fav_core::golden::get(case, "result"),
            "{date:?}"
        );
    }
}

fn image_value(image: &JandanImage) -> Value {
    let optional = |value: &Option<String>| value.clone().map_or(Value::Null, Value::Str);
    let mut dict = Dict::new();
    dict.insert("fav_type", Value::from(image.fav_type));
    dict.insert("pic_id", Value::from(image.pic_id));
    dict.insert("content_index", Value::from(image.content_index));
    dict.insert("content_url", Value::Str(image.content_url.clone()));
    dict.insert("content_md5", optional(&image.content_md5));
    dict.insert("author", Value::Str(image.author.clone()));
    dict.insert("post_date", optional(&image.post_date));
    dict.insert("image_type", optional(&image.image_type));
    Value::Dict(dict)
}

#[test]
fn parsers_match_python() {
    use fav_core::golden::{cases, get, load, string};
    let document = load("jandan/parsing.json");
    for case in cases(&document) {
        let null = Value::Null;
        let expected = case.get("result").unwrap_or(&null);
        if let Some(value) = case.get("to_int") {
            let actual = answer(super::to_int(value), |int| int.map_or(Value::Null, Value::from));
            assert_eq!(&actual, expected, "to_int {value:?}");
        } else if let Some(Value::List(pics)) = case.get("extract_pic_images") {
            let actual = answer(extract_pic_images(1, pics), |images| {
                Value::List(images.iter().map(image_value).collect())
            });
            assert_eq!(&actual, expected, "{pics:?}");
            assert_eq!(
                extract_last_pic_date(pics).map_or(Value::Null, Value::Str),
                *get(case, "last_date"),
                "{pics:?}"
            );
        } else if let Some(Value::Dict(fav)) = case.get("extract_fav_items") {
            let fav_type = i64::try_from(fav_core::golden::int(get(case, "fav_type"))).unwrap();
            assert_eq!(&Value::List(extract_fav_items(fav_type, fav)), expected, "{fav:?}");
        } else if let Some(url) = case.get("url") {
            let url = string(url);
            assert_eq!(
                answer(build_image_download_candidates(&url), str_list),
                *get(case, "candidates"),
                "{url}"
            );
            assert_eq!(
                answer(build_sinaimg_shard_candidates(&url), str_list),
                *get(case, "shards"),
                "{url}"
            );
            let headers = answer(build_image_request_headers(&url), |headers| {
                Value::List(
                    headers
                        .into_iter()
                        .map(|(name, value)| Value::List(vec![Value::Str(name), Value::Str(value)]))
                        .collect(),
                )
            });
            assert_eq!(headers, *get(case, "headers"), "{url}");
        } else {
            let url = string(get(case, "extension"));
            let image_type = case.get("image_type").and_then(Value::as_str);
            assert_eq!(
                &answer(infer_image_extension(&url, image_type), Value::Str),
                expected,
                "{url} {image_type:?}"
            );
        }
    }
}
