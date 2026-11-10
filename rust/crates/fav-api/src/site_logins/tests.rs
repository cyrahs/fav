use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_tungstenite::tungstenite::Message;
use async_tungstenite::tungstenite::client::IntoClientRequest as _;
use fav_browser::login::{
    BrowserLauncher, BrowserLoginManager, DbSessions, LaunchRequest, LoginBrowser, LoginCheck, LoginError, LoginSite, ManagerOptions,
    STATE_CANCELLED, SessionStore, Verify,
};
use fav_browser::profile::rednote_profile_lock_name;
use fav_core::golden;
use fav_core::pyjson::{self, Dict, Value};
use fav_core::settings::{Settings, build_settings};
use fav_runtime::BoxFuture;
use fav_runtime::site_sessions::StoredCookie;
use futures_util::StreamExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{LoadSettings, login_sites};
use crate::tests::{Harness, TOKEN, request_state};

fn settings_with(sections: &[(&str, &str)]) -> Arc<Settings> {
    let values: Vec<(&str, Value)> = sections
        .iter()
        .map(|(section, value)| (*section, pyjson::loads(value).expect("JSON")))
        .collect();
    Arc::new(build_settings(values.iter().map(|(section, value)| (*section, value))).expect("valid settings"))
}

fn fixed(settings: Arc<Settings>) -> LoadSettings {
    Arc::new(move || {
        let settings = Arc::clone(&settings);
        Box::pin(async move { Ok(settings) })
    })
}

fn sites_of(settings: &Arc<Settings>) -> Vec<LoginSite> {
    login_sites(settings, &fixed(Arc::clone(settings)))
}

fn site<'a>(sites: &'a [LoginSite], key: &str) -> &'a LoginSite {
    sites.iter().find(|site| site.key == key).unwrap_or_else(|| panic!("no site {key}"))
}

fn accounts(names: &[&str]) -> Arc<Settings> {
    let accounts: Vec<String> = names.iter().map(|name| format!("{{\"name\": \"{name}\"}}")).collect();
    settings_with(&[("web.bilibili", &format!("{{\"accounts\": [{}]}}", accounts.join(", ")))])
}

#[test]
fn login_sites_match_python() {
    let document = golden::load("api/login_sites.json");
    for case in golden::cases(&document) {
        let accounts: Vec<Value> = golden::list(golden::get(case, "bilibili_accounts"))
            .iter()
            .map(|name| crate::json::dict([("name", name.clone())]))
            .collect();
        let mut section = Dict::new();
        section.insert("accounts", Value::List(accounts));
        let section = Value::Dict(section);
        let settings = Arc::new(build_settings([("web.bilibili", &section)]).expect("valid settings"));
        let sites: Vec<Value> = sites_of(&settings)
            .into_iter()
            .map(|site| {
                crate::json::dict([
                    ("key", Value::Str(site.key)),
                    ("label", Value::Str(site.label)),
                    ("hint", Value::Str(site.hint)),
                    ("session_cookie", Value::Str(site.session_cookie)),
                ])
            })
            .collect();
        assert_eq!(Value::List(sites), *golden::get(case, "sites"), "{case:?}");
    }
}

#[test]
fn every_field_of_the_login_sites_matches_python() {
    let document = golden::load("browser_login/login_sites.json");
    for case in golden::cases(&document) {
        let names: Vec<String> = golden::list(golden::get(case, "bilibili_accounts"))
            .iter()
            .map(golden::string)
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let sites: Vec<Value> = sites_of(&accounts(&names))
            .into_iter()
            .map(|site| {
                crate::json::dict([
                    ("key", Value::Str(site.key.clone())),
                    ("label", Value::Str(site.label.clone())),
                    ("login_url", Value::Str(site.login_url.clone())),
                    ("cookie_urls", crate::json::str_list(&site.cookie_urls)),
                    ("session_cookie", Value::Str(site.session_cookie.clone())),
                    ("controlled", Value::Bool(site.controlled())),
                    ("store_cookie_values", Value::Bool(site.store_cookie_values)),
                    ("verifies_in_browser", Value::Bool(site.verify_in_browser.is_some())),
                    ("has_profile_dir", Value::Bool(site.profile_dir.is_some())),
                    ("has_lock_name", Value::Bool(site.lock_name.is_some())),
                    ("has_after_login", Value::Bool(site.after_login.is_some())),
                    ("hint", Value::Str(site.hint.clone())),
                ])
            })
            .collect();
        assert_eq!(Value::List(sites), *golden::get(case, "sites"), "{case:?}");
    }
}

// py: tests/test_browser_login.py::test_pixiv_is_a_login_site
#[test]
fn pixiv_is_a_login_site() {
    let sites = sites_of(&accounts(&[]));
    let pixiv = site(&sites, "pixiv");

    assert_eq!(pixiv.session_cookie, "PHPSESSID");
    assert!(pixiv.login_url.starts_with("https://accounts.pixiv.net/login"));
}

// py: tests/test_browser_login.py::test_pixiv_and_x_sign_in_through_the_plain_window_and_rednote_does_not
#[test]
fn pixiv_and_x_sign_in_through_the_plain_window_and_rednote_does_not() {
    let sites = sites_of(&accounts(&[]));

    assert!(!site(&sites, "pixiv").controlled());
    assert!(!site(&sites, "twitter").controlled());
    assert!(site(&sites, "rednote").controlled());
    assert!(!site(&sites, "nga").controlled());
    // A profile site records which cookies exist, not their values.
    assert!(!site(&sites, "rednote").store_cookie_values);
    assert!(site(&sites, "pixiv").store_cookie_values);
}

// py: tests/test_browser_login.py::test_nga_is_a_login_site
#[tokio::test]
async fn nga_is_a_login_site() {
    let sites = sites_of(&accounts(&[]));
    let nga = site(&sites, "nga");

    assert_eq!(nga.session_cookie, "ngaPassportCid");
    assert!(nga.login_url.starts_with("https://bbs.nga.cn/nuke.php?__lib=login"));
    assert_eq!(nga.cookie_urls, ["https://bbs.nga.cn/"]);
    assert_eq!((nga.proxy)().await.unwrap(), "");
}

// py: tests/test_browser_login.py::test_every_bilibili_account_gets_its_own_plain_login
#[tokio::test]
async fn every_bilibili_account_gets_its_own_plain_login() {
    let sites = sites_of(&accounts(&["main", "alt"]));
    let keys: Vec<&str> = sites.iter().map(|site| site.key.as_str()).collect();

    for key in ["pixiv", "twitter", "rednote", "bilibili-alt"] {
        assert!(keys.contains(&key), "{keys:?}");
    }
    let main = site(&sites, "bilibili-main");
    assert_eq!(main.session_cookie, "SESSDATA");
    assert!(main.login_url.starts_with("https://passport.bilibili.com/"));
    assert!(!main.controlled());
    assert_eq!((main.proxy)().await.unwrap(), "");
    assert!(main.after_login.is_some());
    assert_eq!(main.label, "Bilibili（main）");
}

// py: tests/test_browser_login.py::test_a_single_bilibili_account_is_just_bilibili
#[test]
fn a_single_bilibili_account_is_just_bilibili() {
    assert_eq!(site(&sites_of(&accounts(&["main"])), "bilibili-main").label, "Bilibili");
}

#[test]
fn the_bilibili_refresh_token_is_stored_under_the_sources_own_key() {
    assert_eq!(
        fav_browser::site_checks::BILIBILI_REFRESH_TOKEN_KEY,
        fav_sources::bilibili::session::REFRESH_TOKEN_KEY
    );
}

// py: tests/test_browser_login.py::test_rednote_is_a_login_site_on_the_crawl_profile
#[tokio::test]
async fn rednote_is_a_login_site_on_the_crawl_profile() {
    let defaults = accounts(&[]);
    let sites = sites_of(&defaults);
    let rednote = site(&sites, "rednote");
    let profile_path = defaults.web.rednote.profile_path.as_path().to_path_buf();

    assert_eq!(rednote.session_cookie, "web_session");
    assert_eq!((rednote.profile_dir.as_ref().unwrap())().await.unwrap(), profile_path);
    // The very lock the crawl takes, or the two could open the profile at once.
    assert_eq!(
        (rednote.lock_name.as_ref().unwrap())().await.unwrap(),
        rednote_profile_lock_name(&profile_path)
    );

    // No proxy and no deliberate direct connection: refused, never silently direct.
    let error = (rednote.proxy)().await.expect_err("refused");
    assert_eq!(error.class, "LoginError");
    assert!(error.message.contains("proxy"), "{error}");
    let configured = settings_with(&[("web.rednote", r#"{"proxy": "http://home.example:3128"}"#)]);
    let sites = sites_of(&configured);
    assert_eq!((site(&sites, "rednote").proxy)().await.unwrap(), "http://home.example:3128");
}

// ---------- API ----------

/// A login window that never signs in.
struct IdleBrowser;

impl LoginBrowser for IdleBrowser {
    fn closed(&self) -> BoxFuture<'_, bool> {
        Box::pin(async { false })
    }

    fn open<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async { Ok(()) })
    }

    fn cookies<'a>(&'a self, _urls: &'a [String]) -> BoxFuture<'a, Result<Vec<Value>, LoginError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn evaluate<'a>(&'a self, _script: &'a str) -> BoxFuture<'a, Result<serde_json::Value, LoginError>> {
        Box::pin(async { Ok(serde_json::json!({})) })
    }

    fn reload(&self) -> BoxFuture<'_, Result<(), LoginError>> {
        Box::pin(async { Ok(()) })
    }

    fn local_storage<'a>(&'a self, _url: &'a str, _key: &'a str) -> BoxFuture<'a, Result<String, LoginError>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), LoginError>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct Launcher(Mutex<u32>);

impl BrowserLauncher for Launcher {
    fn launch(&self, _request: LaunchRequest) -> BoxFuture<'_, Result<Box<dyn LoginBrowser>, LoginError>> {
        Box::pin(async {
            *self.0.lock().unwrap() += 1;
            Ok(Box::new(IdleBrowser) as Box<dyn LoginBrowser>)
        })
    }
}

fn demo_site() -> LoginSite {
    let verify: Verify = Arc::new(|_cookies: Vec<StoredCookie>| Box::pin(async { Ok(LoginCheck::default()) }));
    LoginSite::builder("demo", "Demo", "https://example.com/login", &["https://example.com/"], "SID")
        .verify(verify)
        .build()
        .unwrap()
}

/// `_manager(_Launcher(...))` on the harness's database.
fn demo_manager(harness: &Harness, vnc_port: u16) -> Arc<BrowserLoginManager> {
    let sessions = Arc::new(DbSessions {
        db: harness.state.db.clone(),
    });
    Arc::new(BrowserLoginManager::new(
        Arc::new(vec![demo_site()]),
        Arc::new(Launcher::default()),
        Arc::clone(&sessions) as Arc<dyn SessionStore>,
        sessions,
        ManagerOptions {
            profile_root: PathBuf::from("/data/profiles"),
            poll: Duration::from_millis(10),
            vnc_port,
            ..ManagerOptions::default()
        },
    ))
}

fn auth() -> String {
    format!("Bearer {TOKEN}")
}

// py: tests/test_browser_login.py::test_the_api_lists_sites_and_never_exposes_cookie_values
#[tokio::test]
async fn the_api_lists_sites_and_never_exposes_cookie_values() {
    let mut harness = Harness::new().await;
    harness.state.browser_login = demo_manager(&harness, 5999);
    harness
        .execute(
            "INSERT INTO site_sessions (site, cookies, account, status, message, expires_at, signed_in_at, checked_at, extra) \
             VALUES ('demo', $1, '7', 'valid', '', '2027-01-01T00:00:00Z', '2026-10-27T00:00:00Z', NULL, '');",
            &[r#"[{"name": "SID", "value": "user_7-secret", "domain": ".example.com", "path": "/", "expires": null}]"#],
        )
        .await;

    let answer = harness
        .request("GET", "/api/v2/site-sessions", &[("Authorization", &auth())], None)
        .await;

    assert_eq!(answer.status, 200);
    let body = answer.json();
    assert_eq!(*golden::get(&body, "login"), Value::Null);
    let expected = pyjson::loads(
        r#"{"site": "demo", "label": "Demo", "signed_in": true, "hint": "", "account": "7", "status": "valid", "message": "",
            "cookie_count": 1, "expires_at": "2027-01-01T00:00:00Z", "signed_in_at": "2026-10-27T00:00:00Z", "checked_at": null}"#,
    )
    .unwrap();
    assert_eq!(golden::list(golden::get(&body, "items"))[0], expected);
    assert!(!answer.text().contains("secret"));
    harness.finish().await;
}

// py: tests/test_browser_login.py::test_the_api_requires_the_token
#[tokio::test]
async fn the_api_requires_the_token() {
    let mut harness = Harness::new().await;
    harness.state.browser_login = demo_manager(&harness, 5999);

    assert_eq!(harness.request("GET", "/api/v2/site-sessions", &[], None).await.status, 401);
    assert_eq!(
        harness.request("POST", "/api/v2/site-sessions/demo/login", &[], None).await.status,
        401
    );
    assert_eq!(
        harness.request("DELETE", "/api/v2/site-sessions/login", &[], None).await.status,
        401
    );
    harness.finish().await;
}

// py: tests/test_browser_login.py::test_the_api_starts_a_login_and_hands_out_its_ticket
#[tokio::test]
async fn the_api_starts_a_login_and_hands_out_its_ticket() {
    let mut harness = Harness::new().await;
    harness.state.browser_login = demo_manager(&harness, 5999);
    let authorization = auth();
    let headers = [("Authorization", authorization.as_str())];

    let started = harness.request("POST", "/api/v2/site-sessions/demo/login", &headers, None).await;
    let busy = harness.request("POST", "/api/v2/site-sessions/demo/login", &headers, None).await;
    let listed = harness.request("GET", "/api/v2/site-sessions", &headers, None).await.json();
    let cancelled = harness.request("DELETE", "/api/v2/site-sessions/login", &headers, None).await;

    assert_eq!(started.status, 202);
    let started = started.json();
    assert_eq!(*golden::get(&started, "active"), Value::Bool(true));
    assert_eq!(*golden::get(&started, "state"), Value::from("starting"));
    let ticket = golden::string(golden::get(&started, "ticket"));
    assert_eq!(ticket.len(), 43);
    assert_eq!(busy.status, 409);
    assert_eq!(busy.error_code(), "login_busy");
    assert_eq!(golden::string(golden::get(golden::get(&listed, "login"), "ticket")), ticket);
    assert_eq!(cancelled.status, 200);
    let cancelled = cancelled.json();
    assert_eq!(*golden::get(&cancelled, "state"), Value::from(STATE_CANCELLED));
    assert_eq!(*golden::get(&cancelled, "ticket"), Value::from(""));
    assert_eq!(*golden::get(&cancelled, "active"), Value::Bool(false));
    assert!(golden::get(&cancelled, "finished_at").as_str().is_some_and(|at| at.ends_with('Z')));
    // Nothing to cancel any more: the last attempt again, still cancelled.
    let again = harness
        .request("DELETE", "/api/v2/site-sessions/login", &headers, None)
        .await
        .json();
    assert_eq!(*golden::get(&again, "state"), Value::from(STATE_CANCELLED));
    harness.finish().await;
}

#[tokio::test]
async fn nothing_to_cancel_is_null() {
    let mut harness = Harness::new().await;
    harness.state.browser_login = demo_manager(&harness, 5999);

    let answer = harness
        .request("DELETE", "/api/v2/site-sessions/login", &[("Authorization", &auth())], None)
        .await;

    assert_eq!((answer.status, answer.text().as_str()), (200, "null"));
    harness.finish().await;
}

// py: tests/test_browser_login.py::test_the_api_rejects_an_unknown_site
#[tokio::test]
async fn the_api_rejects_an_unknown_site() {
    let mut harness = Harness::new().await;
    harness.state.browser_login = demo_manager(&harness, 5999);
    let authorization = auth();
    let headers = [("Authorization", authorization.as_str())];

    let unknown = harness.request("POST", "/api/v2/site-sessions/nope/login", &headers, None).await;
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.error_code(), "not_found");
    assert_eq!(
        golden::string(golden::get(golden::get(&unknown.json(), "error"), "message")),
        "Unknown login site: nope"
    );
    // The path is validated like Python's `LoginSitePath`.
    let malformed = harness.request("POST", "/api/v2/site-sessions/a.b/login", &headers, None).await;
    assert_eq!(malformed.status, 422);
    harness.finish().await;
}

/// A VNC server that greets, reads the client's 12-byte answer, and hangs up.
async fn vnc_server() -> (u16, tokio::sync::oneshot::Receiver<Vec<u8>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, received) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket.write_all(b"RFB 003.008\n").await.unwrap();
        let mut answer = vec![0_u8; 12];
        socket.read_exact(&mut answer).await.unwrap();
        let _ = sender.send(answer);
    });
    (port, received)
}

fn http_status(error: &async_tungstenite::tungstenite::Error) -> Option<u16> {
    match error {
        async_tungstenite::tungstenite::Error::Http(response) => Some(response.status().as_u16()),
        _ => None,
    }
}

// py: tests/test_browser_login.py::test_the_vnc_socket_relays_bytes_for_the_ticket_holder
#[tokio::test(flavor = "multi_thread")]
async fn the_vnc_socket_relays_bytes_for_the_ticket_holder() {
    let (vnc_port, received) = vnc_server().await;
    let mut harness = Harness::new().await;
    harness.state.browser_login = demo_manager(&harness, vnc_port);
    let Harness { database, state } = harness;
    let state = Arc::new(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = crate::router::app(Arc::clone(&state));
    tokio::spawn(async move { axum::serve(listener, app).await });
    let authorization = auth();
    let headers = [("Authorization", authorization.as_str())];

    let started = request_state(&state, "POST", "/api/v2/site-sessions/demo/login", &headers, None).await;
    let ticket = golden::string(golden::get(&started.json(), "ticket"));

    // A wrong ticket, and any other path, never get a socket.
    // The other path carries the real ticket, so only the path check can refuse it.
    let elsewhere = format!("/api/v2/other?ticket={ticket}");
    for target in [
        "/api/v2/site-sessions/vnc?ticket=wrong",
        "/api/v2/site-sessions/vnc",
        elsewhere.as_str(),
    ] {
        let refused = async_tungstenite::tokio::connect_async(format!("ws://{address}{target}"))
            .await
            .expect_err("refused");
        assert_eq!(http_status(&refused), Some(403), "{target}: {refused}");
    }
    // A plain GET of the WebSocket path is not a route.
    assert_eq!(
        request_state(&state, "GET", "/api/v2/site-sessions/vnc", &headers, None)
            .await
            .status,
        404
    );

    let mut request = format!("ws://{address}/api/v2/site-sessions/vnc?ticket={ticket}")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert("sec-websocket-protocol", "binary".parse().unwrap());
    let (mut socket, response) = async_tungstenite::tokio::connect_async(request).await.expect("accepted");
    assert_eq!(
        response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|value| value.to_str().ok()),
        Some("binary")
    );
    let greeting = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(greeting, Message::binary(b"RFB 003.008\n".to_vec()));
    socket.send(Message::binary(b"RFB 003.008\n".to_vec())).await.unwrap();
    assert_eq!(received.await.unwrap(), b"RFB 003.008\n");
    // The VNC server hung up, so the socket is closed.
    match tokio::time::timeout(Duration::from_secs(5), socket.next()).await.unwrap() {
        Some(Ok(Message::Close(_))) | None => {}
        other => panic!("expected the socket to close, got {other:?}"),
    }

    request_state(&state, "DELETE", "/api/v2/site-sessions/login", &headers, None).await;
    // The ticket closed with the login.
    let closed = async_tungstenite::tokio::connect_async(format!("ws://{address}/api/v2/site-sessions/vnc?ticket={ticket}"))
        .await
        .expect_err("closed");
    assert_eq!(http_status(&closed), Some(403));
    database.drop_database().await;
}
