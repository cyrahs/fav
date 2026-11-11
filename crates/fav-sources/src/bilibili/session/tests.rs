use std::sync::{Arc, Mutex};

use fav_core::golden;
use fav_core::logging::{self, Level};
use fav_core::pyjson::Value;
use fav_runtime::site_sessions::{StoredCookie, cookie_value};

use super::*;
use crate::testkit::{Answer as Reply, FakeServer, Received};

const NOW: f64 = 1_800_000_000.0;

fn cookie(name: &str, value: &str) -> StoredCookie {
    StoredCookie {
        name: name.to_owned(),
        value: value.to_owned(),
        domain: ".bilibili.com".to_owned(),
        path: "/".to_owned(),
        expires: None,
    }
}

fn jar(extra: &[(&str, &str)]) -> Vec<StoredCookie> {
    let mut cookies: Vec<StoredCookie> = [
        ("SESSDATA", "old-sess"),
        ("bili_jct", "old-jct"),
        ("buvid3", "b3"),
        ("DedeUserID", "42"),
    ]
    .iter()
    .map(|(name, value)| cookie(name, value))
    .collect();
    cookies.extend(extra.iter().map(|(name, value)| cookie(name, value)));
    cookies
}

fn float(value: &Value) -> Option<f64> {
    match value {
        Value::Float(float) => Some(*float),
        #[allow(clippy::cast_precision_loss)] // reason: test values are small
        Value::Int(int) => Some(*int as f64),
        _ => None,
    }
}

fn golden_cookies(value: &Value) -> Vec<StoredCookie> {
    golden::list(value)
        .iter()
        .map(|cookie| StoredCookie {
            name: golden::string(golden::get(cookie, "name")),
            value: golden::string(golden::get(cookie, "value")),
            domain: golden::string(golden::get(cookie, "domain")),
            path: golden::string(golden::get(cookie, "path")),
            expires: float(golden::get(cookie, "expires")),
        })
        .collect()
}

/// One received request against a recorded httpx one; the origin may differ.
fn assert_request(got: &Received, want: &Value, context: &str) {
    let url = golden::string(golden::get(want, "url"));
    let path_and_query = url.splitn(4, '/').nth(3).map(|rest| format!("/{rest}")).expect("a path");
    assert_eq!(got.method, golden::string(golden::get(want, "method")), "{context}");
    if !path_and_query.starts_with("/correspond/1/") {
        assert_eq!(got.path_and_query(), path_and_query, "{context}");
    }
    for header in ["user-agent", "referer", "origin", "cookie", "content-type"] {
        assert_eq!(
            got.header(header),
            golden::get(want, &header.replace('-', "_")).as_str(),
            "{context} {header}"
        );
    }
    assert_eq!(got.body_text(), golden::string(golden::get(want, "body")), "{context}");
}

#[test]
fn the_ticket_signature_matches_the_recorded_ones() {
    let document = golden::load("bilibili/ticket_hexsign.json");
    for case in golden::cases(&document) {
        let ts = i64::try_from(golden::int(golden::get(case, "ts"))).expect("ts");
        assert_eq!(ticket_hexsign(ts), golden::string(golden::get(case, "hexsign")), "{ts}");
    }
}

// py: tests/test_bilibili_session.py::test_the_ticket_signature_matches_bilibili_api
#[test]
fn the_ticket_signature_matches_bilibili_api() {
    assert_eq!(
        ticket_hexsign(1_800_000_000),
        "666e2d0e452c31230305bf40e3870452c7477e366bf38c37a3e362ce97f53916"
    );
}

// py: tests/test_bilibili_session.py::test_the_correspond_path_is_rsa_oaep_hex
#[test]
fn the_correspond_path_is_rsa_oaep_hex() {
    let path = correspond_path(1_700_000_000_000);
    assert_eq!(path.len(), 256);
    assert!(path.bytes().all(|byte| byte.is_ascii_hexdigit()));
    // OAEP pads with random bytes: two encryptions of one timestamp differ.
    assert_ne!(path, correspond_path(1_700_000_000_000));
}

#[test]
fn ticket_expiry_is_read_like_python() {
    let document = golden::load("bilibili/tickets.json");
    for case in golden::cases(&document) {
        let cookies = golden_cookies(golden::get(case, "cookies"));
        assert_eq!(
            ticket_expires_at(&cookies),
            Ok(float(golden::get(case, "expires_at"))),
            "{cookies:?}"
        );
        assert_eq!(
            ticket_due(&cookies, NOW),
            Ok(golden::get(case, "due") == &Value::Bool(true)),
            "{cookies:?}"
        );
    }
}

#[test]
fn a_superscript_expiry_raises_like_float() {
    let cookies = vec![cookie("bili_ticket", "t"), cookie("bili_ticket_expires", "²")];
    assert_eq!(
        ticket_expires_at(&cookies).expect_err("not a decimal").message,
        "could not convert string to float: '²'"
    );
}

// py: tests/test_bilibili_session.py::test_merging_replaces_by_name_and_drops_expired_cookies
#[test]
fn merging_replaces_by_name_and_drops_expired_cookies() {
    let mut updates = vec![cookie("SESSDATA", "n"), cookie("buvid3", "x"), cookie("b_nut", "9")];
    updates[1].expires = Some(1.0);
    let merged = merge_cookies(&jar(&[]), &updates, NOW);
    let pairs: Vec<(&str, &str)> = merged.iter().map(|cookie| (cookie.name.as_str(), cookie.value.as_str())).collect();
    assert_eq!(
        pairs,
        [("SESSDATA", "n"), ("bili_jct", "old-jct"), ("DedeUserID", "42"), ("b_nut", "9")]
    );
}

#[test]
fn merging_matches_the_recorded_cases() {
    let document = golden::load("bilibili/merge_cookies.json");
    for case in golden::cases(&document) {
        let cookies = golden_cookies(golden::get(case, "cookies"));
        let updates = golden_cookies(golden::get(case, "updates"));
        assert_eq!(
            merge_cookies(&cookies, &updates, NOW),
            golden_cookies(golden::get(case, "merged")),
            "{updates:?}"
        );
    }
}

// py: tests/test_bilibili_session.py::test_a_ticket_with_days_to_go_is_kept
#[tokio::test]
async fn a_ticket_with_days_to_go_is_kept() {
    let server = FakeServer::start(|request| panic!("unexpected request {}", request.path)).await;
    let expires = format!("{}", 1_800_000_000 + 216_000);
    let cookies = jar(&[("bili_ticket", "t"), ("bili_ticket_expires", &expires)]);
    assert_eq!(renew_ticket(&cookies, &Endpoints::at(&server.base_url), NOW).await, Ok(None));
}

// py: tests/test_bilibili_session.py::test_a_missing_or_expiring_ticket_is_renewed
#[tokio::test]
async fn a_missing_or_expiring_ticket_is_renewed() {
    let server = FakeServer::start(|_| {
        Reply::json(
            200,
            r#"{"code": 0, "data": {"ticket": "fresh", "created_at": 1800000000, "ttl": 259200}}"#,
        )
    })
    .await;
    let cookies = jar(&[("bili_ticket", "stale"), ("bili_ticket_expires", "1800003600")]);
    let renewed = renew_ticket(&cookies, &Endpoints::at(&server.base_url), NOW)
        .await
        .expect("renewed")
        .expect("due");
    assert_eq!(cookie_value(&renewed, "bili_ticket"), "fresh");
    assert_eq!(cookie_value(&renewed, "bili_ticket_expires"), "1800259200");
    assert_eq!(cookie_value(&renewed, "SESSDATA"), "old-sess");
    let seen = server.received();
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].param("key_id"), Some("ec02"));
    assert_eq!(seen[0].param("context[ts]"), Some("1800000000"));
    assert!(seen[0].header("cookie").expect("a cookie header").contains("SESSDATA=old-sess"));
}

// py: tests/test_bilibili_session.py::test_a_ticket_bilibili_refuses_raises
#[tokio::test]
async fn a_ticket_bilibili_refuses_raises() {
    let server = FakeServer::start(|_| Reply::json(200, r#"{"code": -400, "message": "bad"}"#)).await;
    let error = renew_ticket(&jar(&[]), &Endpoints::at(&server.base_url), NOW)
        .await
        .expect_err("refused");
    assert!(
        matches!(&error, RenewError::Renewal(message) if message.contains("-400")),
        "{error:?}"
    );
}

#[tokio::test]
async fn ticket_renewals_match_the_recorded_cases() {
    let document = golden::load("bilibili/renew_ticket.json");
    for case in golden::cases(&document) {
        let answer = golden::get(case, "answer");
        let status = u16::try_from(golden::int(golden::get(answer, "status"))).expect("status");
        let body = golden::string(golden::get(answer, "body"));
        let server = FakeServer::start(move |_| Reply::json(status, &body)).await;
        let cookies = golden_cookies(golden::get(case, "cookies"));
        let outcome = renew_ticket(&cookies, &Endpoints::at(&server.base_url), NOW).await;
        let context = golden::string(golden::get(answer, "body"));
        match case.get("renewed") {
            Some(Value::Null) => assert_eq!(outcome, Ok(None), "{context}"),
            Some(renewed) => assert_eq!(outcome, Ok(Some(golden_cookies(renewed))), "{context}"),
            None => {
                let error = outcome.expect_err("an error");
                assert_eq!(golden::string(golden::get(case, "error_class")), "RenewalError", "{context}");
                assert_eq!(error, RenewError::Renewal(golden::string(golden::get(case, "error"))), "{context}");
            }
        }
        let received = server.received();
        let expected = golden::list(golden::get(case, "requests"));
        assert_eq!(received.len(), expected.len(), "{context}");
        for (got, want) in received.iter().zip(expected) {
            assert_request(got, want, &context);
        }
    }
}

const REFRESHED: &str = r#"{"code": 0, "data": {"refresh_token": "rt-new"}}"#;
const REFRESH_COOKIES: [&str; 3] = [
    "SESSDATA=new-sess; Path=/; Domain=bilibili.com; Expires=Wed, 01 Jan 2031 00:00:00 GMT; HttpOnly",
    "bili_jct=new-jct; Path=/; Domain=bilibili.com; Expires=Wed, 01 Jan 2031 00:00:00 GMT",
    "sid=new-sid; Path=/; Domain=bilibili.com",
];

/// The answers of one recorded refresh script, by step.
#[derive(Clone)]
struct Script {
    info: Reply,
    correspond: Reply,
    refresh: Reply,
    confirm: Reply,
}

impl Script {
    fn new(info: Reply) -> Self {
        let mut refresh = Reply::json(200, REFRESHED);
        for header in REFRESH_COOKIES {
            refresh = refresh.with_header("set-cookie", header);
        }
        Self {
            info,
            correspond: Reply::html(r#"<html><div id="1-name">rcsrf</div></html>"#),
            refresh,
            confirm: Reply::json(200, r#"{"code": 0}"#),
        }
    }

    fn answer(&self, path: &str) -> Reply {
        if path.ends_with("/cookie/info") {
            self.info.clone()
        } else if path.starts_with("/correspond/1/") {
            self.correspond.clone()
        } else if path.ends_with("/cookie/refresh") {
            self.refresh.clone()
        } else {
            self.confirm.clone()
        }
    }
}

fn script(name: &str) -> Script {
    let due = || Reply::json(200, r#"{"code": 0, "data": {"refresh": true}}"#);
    match name {
        "not due" => Script::new(Reply::json(200, r#"{"code": 0, "data": {"refresh": false}}"#)),
        "due, refreshed and confirmed" => Script::new(due()),
        "info refused" => Script::new(Reply::json(200, r#"{"code": -101, "message": "账号未登录"}"#)),
        "info not json" => Script::new(Reply::text(200, "nope")),
        "info refresh is a string" => Script::new(Reply::json(200, r#"{"code": 0, "data": {"refresh": "no"}}"#)),
        "correspond 404" => Script {
            correspond: Reply::text(404, r#"<div id="1-name">x</div>"#),
            ..Script::new(due())
        },
        "correspond without csrf" => Script {
            correspond: Reply::text(200, r#"<div id="2-name">x</div>"#),
            ..Script::new(due())
        },
        "correspond csrf spans lines" => Script {
            correspond: Reply::text(200, "<div id=\"1-name\">a\nb</div><div id=\"1-name\">c</div>"),
            ..Script::new(due())
        },
        "refresh refused" => Script {
            refresh: Reply::json(200, r#"{"code": -111, "message": "csrf"}"#),
            ..Script::new(due())
        },
        "refresh without a token" => Script {
            refresh: Reply::json(200, r#"{"code": 0, "data": {}}"#).with_header("set-cookie", "SESSDATA=n; Domain=bilibili.com"),
            ..Script::new(due())
        },
        "refresh without SESSDATA" => Script {
            refresh: Reply::json(200, REFRESHED),
            ..Script::new(due())
        },
        "confirm refused" => Script {
            confirm: Reply::json(200, r#"{"code": -101}"#),
            ..Script::new(due())
        },
        "confirm not json" => Script {
            confirm: Reply::text(502, "bad gateway"),
            ..Script::new(due())
        },
        other => panic!("no script for {other}"),
    }
}

type Saved = Arc<Mutex<Vec<(usize, Renewal)>>>;

/// A refresh's outcome, the requests it made, what it saved (after how many requests),
/// and the warnings it logged.
type RefreshRun = (
    Result<Option<Renewal>, RenewError>,
    Vec<Received>,
    Vec<(usize, Renewal)>,
    Vec<String>,
);

/// Run one refresh against a script on a fresh runtime, capturing the warnings it logs.
fn run_refresh(script: Script) -> RefreshRun {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let mut outcome = None;
    let records = logging::capture(|| {
        outcome = Some(runtime.block_on(async move {
            let server = Arc::new(FakeServer::start(move |request| script.answer(&request.path)).await);
            let saved: Saved = Arc::default();
            let save = {
                let saved = Arc::clone(&saved);
                let server = Arc::clone(&server);
                async move |renewal: &Renewal| {
                    saved.lock().expect("saved").push((server.received().len(), renewal.clone()));
                    Ok(())
                }
            };
            let result = refresh_session(&jar(&[]), "rt-old", &Endpoints::at(&server.base_url), NOW, save).await;
            let saved = saved.lock().expect("saved").clone();
            (result, server.received(), saved)
        }));
    });
    let (result, received, saved) = outcome.expect("ran");
    let warnings = records
        .into_iter()
        .filter(|record| record.level == Level::Warning)
        .map(|record| record.message)
        .collect();
    (result, received, saved, warnings)
}

#[test]
fn refreshes_match_the_recorded_cases() {
    let document = golden::load("bilibili/refresh_session.json");
    for case in golden::cases(&document) {
        let name = golden::string(golden::get(case, "case"));
        let (result, received, saved, warnings) = run_refresh(script(&name));
        match case.get("result") {
            Some(Value::Null) => assert_eq!(result, Ok(None), "{name}"),
            Some(expected) => {
                let renewal = result.expect("refreshed").expect("due");
                assert_eq!(renewal.cookies, golden_cookies(golden::get(expected, "cookies")), "{name}");
                assert_eq!(
                    renewal.refresh_token,
                    golden::string(golden::get(expected, "refresh_token")),
                    "{name}"
                );
                let renewed: Vec<String> = golden::list(golden::get(expected, "renewed")).iter().map(golden::string).collect();
                assert_eq!(renewal.renewed, renewed, "{name}");
            }
            None => {
                assert_eq!(golden::string(golden::get(case, "error_class")), "RenewalError", "{name}");
                assert_eq!(
                    result,
                    Err(RenewError::Renewal(golden::string(golden::get(case, "error")))),
                    "{name}"
                );
            }
        }
        let expected = golden::list(golden::get(case, "requests"));
        assert_eq!(received.len(), expected.len(), "{name}");
        for (got, want) in received.iter().zip(expected) {
            assert_request(got, want, &name);
            let form: Vec<(String, String)> = golden::list(golden::get(want, "form"))
                .iter()
                .map(|pair| (golden::string(&golden::list(pair)[0]), golden::string(&golden::list(pair)[1])))
                .collect();
            assert_eq!(got.form(), form, "{name}");
        }
        let expected_saved = golden::list(golden::get(case, "saved"));
        assert_eq!(saved.len(), expected_saved.len(), "{name}");
        for ((after, renewal), want) in saved.iter().zip(expected_saved) {
            assert_eq!(
                i128::try_from(*after).expect("small"),
                golden::int(golden::get(want, "after_requests")),
                "{name}"
            );
            assert_eq!(renewal.cookies, golden_cookies(golden::get(want, "cookies")), "{name}");
            assert_eq!(renewal.refresh_token, golden::string(golden::get(want, "refresh_token")), "{name}");
        }
        let expected_warnings: Vec<String> = golden::list(golden::get(case, "warnings")).iter().map(golden::string).collect();
        assert_eq!(warnings, expected_warnings, "{name}");
    }
}

// py: tests/test_bilibili_session.py::test_no_refresh_when_bilibili_does_not_ask
#[test]
fn no_refresh_when_bilibili_does_not_ask() {
    let (result, received, saved, _) = run_refresh(script("not due"));
    assert_eq!(result, Ok(None));
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].path, "/x/passport-login/web/cookie/info");
    assert!(saved.is_empty());
}

// py: tests/test_bilibili_session.py::test_a_refresh_saves_the_new_pair_before_confirming_it
#[test]
fn a_refresh_saves_the_new_pair_before_confirming_it() {
    let (result, received, saved, _) = run_refresh(script("due, refreshed and confirmed"));
    let renewal = result.expect("refreshed").expect("due");
    assert_eq!(renewal.refresh_token, "rt-new");
    assert_eq!(cookie_value(&renewal.cookies, "SESSDATA"), "new-sess");
    assert_eq!(cookie_value(&renewal.cookies, "bili_jct"), "new-jct");
    assert_eq!(cookie_value(&renewal.cookies, "sid"), "new-sid");
    assert_eq!(cookie_value(&renewal.cookies, "buvid3"), "b3");
    assert_eq!(renewal.cookies.iter().filter(|cookie| cookie.name == "SESSDATA").count(), 1);
    // Saved after the refresh answered (info, correspond, refresh) and before the confirmation.
    assert_eq!(saved[0].0, 3);
    let refresh = &received[2];
    assert_eq!(
        refresh.form(),
        [
            ("csrf", "old-jct"),
            ("refresh_csrf", "rcsrf"),
            ("source", "main_web"),
            ("refresh_token", "rt-old")
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
    );
    let confirm = &received[3];
    assert!(confirm.path.ends_with("/confirm/refresh"));
    assert_eq!(
        confirm.form(),
        [("csrf", "new-jct"), ("refresh_token", "rt-old")].map(|(key, value)| (key.to_owned(), value.to_owned()))
    );
    let cookie_header = confirm.header("cookie").expect("cookies");
    assert!(cookie_header.contains("SESSDATA=new-sess"));
    assert!(!cookie_header.contains("old-sess"));
}

// py: tests/test_bilibili_session.py::test_a_failed_confirmation_keeps_the_refreshed_pair
#[test]
fn a_failed_confirmation_keeps_the_refreshed_pair() {
    let (result, _, _, warnings) = run_refresh(script("confirm refused"));
    let renewal = result.expect("refreshed").expect("due");
    assert_eq!(cookie_value(&renewal.cookies, "SESSDATA"), "new-sess");
    assert_eq!(warnings.len(), 1);
}

// py: tests/test_bilibili_session.py::test_a_refresh_without_a_csrf_page_raises
#[test]
fn a_refresh_without_a_csrf_page_raises() {
    let (result, _, saved, _) = run_refresh(Script {
        correspond: Reply::text(404, "gone"),
        refresh: Reply::text(404, "gone"),
        confirm: Reply::text(404, "gone"),
        ..script("due, refreshed and confirmed")
    });
    assert!(
        matches!(&result, Err(RenewError::Renewal(message)) if message.contains("correspond")),
        "{result:?}"
    );
    assert!(saved.is_empty());
}
