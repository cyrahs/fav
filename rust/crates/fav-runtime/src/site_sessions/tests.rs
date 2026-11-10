use fav_core::golden;
use fav_core::pyjson::{Key, Value};

use super::*;
use crate::testutil::database;

/// A cookie as the golden writes it.
fn golden_cookie(value: &Value) -> StoredCookie {
    let text = |key: &str| golden::string(golden::get(value, key));
    StoredCookie {
        name: text("name"),
        value: text("value"),
        domain: text("domain"),
        path: text("path"),
        expires: match golden::get(value, "expires") {
            Value::Null => None,
            Value::Float(float) => Some(*float),
            other => panic!("expires is a float or null, got {other:?}"),
        },
    }
}

fn golden_cookies(value: &Value) -> Vec<StoredCookie> {
    golden::list(value).iter().map(golden_cookie).collect()
}

/// `datetime.isoformat()` of an aware UTC time.
fn isoformat(when: DateTime<Utc>) -> String {
    if when.timestamp_subsec_micros() == 0 {
        when.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        when.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    }
}

#[test]
fn codec_matches_python() {
    let document = golden::load("site_sessions/codec.json");
    let mut checked = 0;
    for case in golden::cases(&document) {
        match golden::string(golden::get(case, "function")).as_str() {
            "cookies" => {
                let cookies = golden_cookies(golden::get(case, "cookies"));
                assert_eq!(
                    encode_cookies(&cookies),
                    golden::string(golden::get(case, "encoded")),
                    "{cookies:?}"
                );
                assert_eq!(to_netscape(&cookies), golden::string(golden::get(case, "netscape")), "{cookies:?}");
                assert_eq!(without_values(&cookies), golden_cookies(golden::get(case, "without_values")));
                for (name, expected) in golden::get(case, "cookie_value").as_dict().expect("dict").iter() {
                    let Key::Str(name) = name else { panic!("string key") };
                    assert_eq!(cookie_value(&cookies, name), golden::string(expected), "{name}");
                }
                for (name, expected) in golden::get(case, "cookie_expiry").as_dict().expect("dict").iter() {
                    let Key::Str(name) = name else { panic!("string key") };
                    let ours = cookie_expiry(&cookies, name).map(isoformat);
                    assert_eq!(ours, expected.as_str().map(str::to_owned), "{name}");
                }
                // The encoding reads back to the same cookies (an empty path reads as `/`).
                let reread: Vec<StoredCookie> = cookies
                    .iter()
                    .map(|cookie| StoredCookie {
                        path: if cookie.path.is_empty() {
                            "/".to_owned()
                        } else {
                            cookie.path.clone()
                        },
                        ..cookie.clone()
                    })
                    .collect();
                assert_eq!(decode_cookies(&encode_cookies(&cookies)), reread);
            }
            "cookies_from_browser" => {
                let raw = golden::list(golden::get(case, "raw"));
                assert_eq!(cookies_from_browser(raw), golden_cookies(golden::get(case, "cookies")), "{raw:?}");
            }
            "decode_cookies" => {
                let stored = golden::get(case, "stored").as_str().unwrap_or_default();
                assert_eq!(decode_cookies(stored), golden_cookies(golden::get(case, "cookies")), "{stored:?}");
            }
            "decode_extra" => {
                let stored = golden::get(case, "stored").as_str().unwrap_or_default();
                let expected: Vec<(String, String)> = golden::get(case, "extra")
                    .as_dict()
                    .expect("dict")
                    .iter()
                    .map(|(key, value)| (crate::pystr::str_key(key), golden::string(value)))
                    .collect();
                assert_eq!(decode_extra(stored), expected, "{stored:?}");
            }
            other => panic!("unknown function {other}"),
        }
        checked += 1;
    }
    assert!(checked > 25, "{checked}");
}

fn cookie(name: &str, value: &str) -> StoredCookie {
    StoredCookie {
        name: name.to_owned(),
        value: value.to_owned(),
        domain: ".example.com".to_owned(),
        path: "/".to_owned(),
        expires: Some(1_767_225_600.0),
    }
}

#[tokio::test]
async fn sessions_round_trip_through_the_table() {
    let database = database().await;
    let db = database.db();
    assert_eq!(load(db, "pixiv").await.expect("load"), None);
    let expires_at = DateTime::from_timestamp(1_767_225_600, 0);
    let extra = vec![("ac_time_value".to_owned(), "refresh".to_owned())];
    save_signed_in(db, "pixiv", &[cookie("PHPSESSID", "secret")], "user", expires_at, &extra)
        .await
        .expect("saved");
    let session = load(db, "pixiv").await.expect("load").expect("row");
    assert_eq!(session.cookie("phpsessid"), "secret");
    assert_eq!(session.account, "user");
    assert_eq!(session.status, STATUS_VALID);
    assert_eq!(session.expires_at, expires_at);
    assert!(session.signed_in_at.is_some() && session.checked_at.is_some());
    assert_eq!(session.extra_value("ac_time_value"), Some("refresh"));
    let stored: String = sqlx::query_scalar("SELECT extra FROM site_sessions WHERE site = 'pixiv';")
        .fetch_one(db.pool())
        .await
        .expect("extra");
    assert_eq!(stored, "{\"ac_time_value\": \"refresh\"}");
    // The Debug output never carries a credential.
    let debug = format!("{session:?}");
    assert!(!debug.contains("secret") && !debug.contains("refresh"), "{debug}");

    record_check(db, "pixiv", false, &"x".repeat(600)).await.expect("checked");
    let session = load(db, "pixiv").await.expect("load").expect("row");
    assert_eq!(session.status, STATUS_INVALID);
    assert_eq!(session.message.chars().count(), 500);
    let signed_in_at = session.signed_in_at;

    replace_cookies(db, "pixiv", &[cookie("PHPSESSID", "renewed")], None, &[])
        .await
        .expect("replaced");
    let session = load(db, "pixiv").await.expect("load").expect("row");
    assert_eq!(session.cookie("PHPSESSID"), "renewed");
    assert_eq!(session.status, STATUS_VALID);
    assert_eq!(session.message, "");
    assert_eq!(session.account, "user");
    assert_eq!(session.signed_in_at, signed_in_at);
    assert_eq!(session.expires_at, None);
    assert!(session.extra.is_empty());

    // A new login replaces the whole row and clears the message.
    record_check(db, "pixiv", false, "expired").await.expect("checked");
    save_signed_in(db, "pixiv", &[], "other", None, &[]).await.expect("saved");
    save_signed_in(db, "bilibili", &[], "", None, &[]).await.expect("saved");
    let all = load_all(db).await.expect("all");
    assert_eq!(all.keys().collect::<Vec<_>>(), ["bilibili", "pixiv"]);
    assert_eq!(all["pixiv"].account, "other");
    assert_eq!(all["pixiv"].message, "");
    assert!(all["pixiv"].cookies.is_empty());
    // Updating a site that has no row does nothing.
    record_check(db, "x", true, "").await.expect("no row");
    assert_eq!(load(db, "x").await.expect("load"), None);
    database.drop_database().await;
}

#[test]
fn timestamps_round_like_python() {
    let when = from_timestamp(1_700_000_000.123_456).expect("in range");
    assert_eq!(when.timestamp_subsec_micros(), 123_456);
    assert_eq!(from_timestamp(-1.5).expect("in range").timestamp(), -2);
    assert_eq!(from_timestamp(f64::NAN), None);
}
