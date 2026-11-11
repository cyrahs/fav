//! The driver against a real Chromium and loopback pages only (moved here from the WP0
//! probe, `probes/api-coverage/src/cdp.rs`). CI sets `FAV_TEST_CHROMIUM`, so there they
//! always run; elsewhere they are skipped when no Chromium is installed.

use std::time::Duration;

use serde_json::json;

use super::{BrowserContext, ContextOptions, DriverError, filter_cookies};
use crate::chromium::LaunchOptions;
use crate::proxy::build_proxy_settings;
use crate::rednote::{WEBDRIVER_INIT_SCRIPT, present_mac_user_agent};
use crate::testutil::{Reply, chromium, serve};

const WAIT: Duration = Duration::from_secs(20);

/// Two tabs like RedNote's profile page, and a page tall enough to scroll.
const TABS_PAGE: &str = "<html><body style='margin:0'>\
    <div style='height:3000px'>\
    <span class='reds-tab-item' onclick='window.__tab=\"notes\"'>笔记</span>\
    <span class='reds-tab-item' onclick='window.__tab=\"liked\"'>赞过 12</span>\
    </div><script>window.__loads=(Number(sessionStorage.loads)||0)+1;sessionStorage.loads=window.__loads;</script></body></html>";

fn site(request: &str) -> Reply {
    if request.starts_with("get /tabs") {
        Reply::html(TABS_PAGE).with_header("Set-Cookie: tab_seen=1; Path=/tabs")
    } else {
        Reply::html("<html><body>home</body></html>").with_header("Set-Cookie: web_session=abc; Path=/")
    }
}

fn launch_options(profile: &std::path::Path, proxy: &str) -> LaunchOptions {
    LaunchOptions {
        user_data_dir: profile.to_path_buf(),
        headless: true,
        proxy: build_proxy_settings(proxy).expect("a valid proxy"),
        args: vec!["--disable-dev-shm-usage".to_owned()],
        ignore_default_args: vec!["--enable-automation".to_owned()],
    }
}

fn rednote_context() -> ContextOptions {
    ContextOptions {
        locale: Some("zh-CN".to_owned()),
        timezone_id: Some("Asia/Shanghai".to_owned()),
        viewport: Some((1280, 800)),
        proxy_credentials: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn drives_a_local_page_like_the_rednote_browser() {
    let Some(executable) = chromium() else { return };
    let (addr, seen) = serve(site).await;
    let profile = tempfile::tempdir().unwrap();
    let context = BrowserContext::launch(&executable, &launch_options(profile.path(), ""), &[], rednote_context())
        .await
        .expect("launched");

    let page = context.first_page().await.expect("the first tab");
    context.add_init_script(WEBDRIVER_INIT_SCRIPT).await.unwrap();
    context.add_init_script("window.__probe = 41 + 1;").await.unwrap();
    page.goto(&format!("http://{addr}/"), WAIT).await.expect("loaded");
    // Loopback is a secure context, so the hints are read without the neutral page.
    assert!(present_mac_user_agent(&page, Duration::ZERO).await.expect("presented"));
    page.reload(WAIT).await.unwrap();

    assert_eq!(page.evaluate("() => window.__probe").await.unwrap(), json!(42));
    assert_eq!(page.evaluate("navigator.webdriver === undefined").await.unwrap(), json!(true));
    assert_eq!(
        page.evaluate("() => navigator.userAgentData.platform").await.unwrap(),
        json!("macOS")
    );
    assert_eq!(page.evaluate("() => navigator.platform").await.unwrap(), json!("MacIntel"));
    let user_agent = page.evaluate("() => navigator.userAgent").await.unwrap();
    let user_agent = user_agent.as_str().unwrap();
    assert!(
        !user_agent.contains("HeadlessChrome") && user_agent.contains("Macintosh"),
        "{user_agent}"
    );
    assert_eq!(
        page.evaluate("() => Intl.DateTimeFormat().resolvedOptions().timeZone")
            .await
            .unwrap(),
        json!("Asia/Shanghai")
    );
    assert_eq!(page.evaluate("() => [innerWidth, innerHeight]").await.unwrap(), json!([1280, 800]));
    assert_eq!(
        page.evaluate("() => matchMedia('(prefers-color-scheme: light)').matches")
            .await
            .unwrap(),
        json!(true)
    );
    // The first load asked in the context's locale, the reload in the presented one.
    let requests = seen.lock().unwrap().clone();
    assert!(requests[0].contains("accept-language: zh-cn\r\n"), "{}", requests[0]);
    // Chromium adds its own q-value to the list it is given, so the presented header reads
    // `zh-CN,zh;q=0.9;q=0.9`; Python's crawl sends exactly the same override.
    assert!(requests.last().unwrap().contains("accept-language: zh-cn,zh;q=0.9"), "{requests:?}");
    assert!(page.url().await.unwrap().starts_with(&format!("http://{addr}/")));

    let cookies = context.cookies(&[]).await.unwrap();
    assert!(
        cookies
            .iter()
            .any(|cookie| cookie["name"] == "web_session" && cookie["value"] == "abc"),
        "{cookies:?}"
    );
    let thrown = page.evaluate("() => { throw new Error('boom'); }").await.expect_err("thrown");
    assert!(
        matches!(&thrown, DriverError::Evaluation(text) if text.contains("boom")),
        "{thrown:?}"
    );

    context.close().await;
    assert!(context.is_closed().await);
}

/// An HTTP proxy on loopback that demands Basic auth (`fav:secret`) and serves every
/// proxied request itself, so no name is ever resolved and nothing leaves the host.
fn authenticating_proxy(request: &str) -> Reply {
    // base64("fav:secret"), lowercased with the rest of the request.
    if request.contains("proxy-authorization: basic zmf2onnly3jlda==") {
        Reply::html("<html><body>proxied</body></html>")
    } else {
        Reply {
            status: "407 Proxy Authentication Required",
            content_type: "text/plain",
            headers: "Proxy-Authenticate: Basic realm=\"fav\"\r\n".to_owned(),
            body: String::new(),
            delay: std::time::Duration::ZERO,
        }
    }
}

/// A page whose image never finishes: `DOMContentLoaded` fires at once, `load` not for a
/// minute.
fn slow_subresource_site(request: &str) -> Reply {
    if request.starts_with("get /slow.png") {
        Reply::html("").after(Duration::from_mins(1))
    } else if request.starts_with("get /blocking.js") {
        Reply::html("").after(Duration::from_secs(1))
    } else if request.starts_with("get /late.js") {
        Reply::html("").after(Duration::from_secs(3))
    } else if request.starts_with("get /loading") {
        Reply::html("<html><body><script src='/blocking.js'></script>old</body></html>")
    } else if request.starts_with("get /late") {
        Reply::html("<html><body><script src='/late.js'></script>late</body></html>").after(Duration::from_secs(2))
    } else if request.starts_with("get /next") {
        Reply::html("<html><body>next</body></html>")
    } else {
        Reply::html("<html><body><img src='/slow.png'>ready</body></html>")
    }
}

/// `goto` waits for `DOMContentLoaded` as Playwright does, not for `load`: chromiumoxide's
/// own `Page.navigate` answers only after `load` in every frame, which made RedNote fail
/// with "Request timed out." in production (2026-11-11). A second navigation right after
/// is not held behind the first one's `load` either; a refused navigation still fails
/// with Chromium's error text, and a same-document one is done when it commits.
#[tokio::test(flavor = "multi_thread")]
async fn goto_waits_for_dom_content_loaded_not_load() {
    let Some(executable) = chromium() else { return };
    let (addr, _) = serve(slow_subresource_site).await;
    let profile = tempfile::tempdir().unwrap();
    let context = BrowserContext::launch(&executable, &launch_options(profile.path(), ""), &[], rednote_context())
        .await
        .expect("launched");
    let page = context.first_page().await.unwrap();

    let started = std::time::Instant::now();
    page.goto(&format!("http://{addr}/"), Duration::from_secs(10))
        .await
        .expect("done at DOMContentLoaded");
    assert_eq!(page.evaluate("() => document.readyState").await.unwrap(), json!("interactive"));
    page.goto(&format!("http://{addr}/next"), Duration::from_secs(10))
        .await
        .expect("the next navigation is not queued behind load");
    assert_eq!(page.evaluate("() => document.body.textContent").await.unwrap(), json!("next"));
    assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());

    page.goto(&format!("http://{addr}/next#part"), Duration::from_secs(10))
        .await
        .expect("a same-document navigation");
    assert_eq!(page.evaluate("() => location.hash").await.unwrap(), json!("#part"));

    // The document being left is still loading, and fires its own `DOMContentLoaded`
    // before the new one has parsed: that one must not end the wait.
    page.evaluate("() => { location.href = '/loading'; }").await.unwrap();
    page.goto(&format!("http://{addr}/late"), Duration::from_secs(10))
        .await
        .expect("the new document's DOMContentLoaded");
    assert_ne!(page.evaluate("() => document.readyState").await.unwrap(), json!("loading"));
    assert_eq!(page.evaluate("() => document.body.textContent").await.unwrap(), json!("late"));

    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let closed = format!("http://{closed}/");
    let refused = page.goto(&closed, Duration::from_secs(10)).await.unwrap_err();
    assert!(
        matches!(&refused, DriverError::Navigation(text) if *text == format!("net::ERR_CONNECTION_REFUSED at {closed}")),
        "{refused:?}"
    );

    context.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn answers_proxy_authentication_like_playwright() {
    let Some(executable) = chromium() else { return };
    let (proxy, _) = serve(authenticating_proxy).await;
    let profile = tempfile::tempdir().unwrap();
    let launch = launch_options(profile.path(), &format!("http://fav:secret@{proxy}"));
    let settings = launch.proxy.clone().expect("a proxy");
    let context = BrowserContext::launch(
        &executable,
        &launch,
        &[],
        ContextOptions {
            proxy_credentials: Some((settings.username.unwrap(), settings.password.unwrap())),
            ..ContextOptions::default()
        },
    )
    .await
    .expect("launched");

    let page = context.first_page().await.unwrap();
    page.goto("http://fav.probe.invalid/", WAIT).await.expect("proxied");
    assert_eq!(page.evaluate("() => document.body.textContent").await.unwrap(), json!("proxied"));

    context.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn clicks_scrolls_reloads_and_filters_cookies_like_the_liked_tab_flow() {
    let Some(executable) = chromium() else { return };
    let (addr, _) = serve(site).await;
    let profile = tempfile::tempdir().unwrap();
    let context = BrowserContext::launch(&executable, &launch_options(profile.path(), ""), &[], rednote_context())
        .await
        .expect("launched");
    let page = context.new_page().await.unwrap();

    page.goto(&format!("http://{addr}/"), WAIT).await.unwrap();
    page.goto(&format!("http://{addr}/tabs"), WAIT).await.unwrap();

    page.click_first_with_text(".reds-tab-item", "赞过", WAIT).await.expect("clicked");
    assert_eq!(page.evaluate("() => window.__tab").await.unwrap(), json!("liked"));
    page.click_last_with_exact_text("笔记", WAIT).await.expect("clicked");
    assert_eq!(page.evaluate("() => window.__tab").await.unwrap(), json!("notes"));
    let missing = page
        .click_first_with_text(".reds-tab-item", "收藏", Duration::from_millis(300))
        .await
        .expect_err("a missing tab must be an error, not a silent no-op");
    assert!(matches!(missing, DriverError::Locator(_)), "{missing:?}");

    page.mouse_move(640.0, 400.0).await.unwrap();
    page.mouse_wheel(640.0, 400.0, 0.0, 900.0).await.unwrap();
    let scrolled = page
        .wait_for_function("() => window.scrollY > 0", WAIT, Duration::from_millis(50))
        .await;
    assert!(scrolled.is_ok(), "the wheel did not scroll the page");

    page.reload(WAIT).await.unwrap();
    assert_eq!(page.evaluate("() => window.__loads").await.unwrap(), json!(2));

    let names = |cookies: Vec<serde_json::Value>| -> Vec<String> {
        cookies.iter().map(|cookie| cookie["name"].as_str().unwrap().to_owned()).collect()
    };
    let on_tabs = names(context.cookies(&[&format!("http://{addr}/tabs")]).await.unwrap());
    assert!(
        on_tabs.contains(&"tab_seen".to_owned()) && on_tabs.contains(&"web_session".to_owned()),
        "{on_tabs:?}"
    );
    let on_root = names(context.cookies(&[&format!("http://{addr}/")]).await.unwrap());
    assert!(!on_root.contains(&"tab_seen".to_owned()), "{on_root:?}");

    page.close().await.unwrap();
    context.close().await;
}

#[test]
fn cookies_are_filtered_like_playwright() {
    let jar = vec![
        json!({"name": "root", "domain": ".example.com", "path": "/", "secure": false}),
        json!({"name": "host", "domain": "www.example.com", "path": "/", "secure": false}),
        json!({"name": "deep", "domain": ".example.com", "path": "/deep", "secure": false}),
        json!({"name": "secure", "domain": ".example.com", "path": "/", "secure": true}),
        json!({"name": "local", "domain": "localhost", "path": "/", "secure": true}),
        json!({"name": "other", "domain": ".example.org", "path": "/", "secure": false}),
    ];
    let names = |urls: &[&str]| -> Vec<String> {
        filter_cookies(jar.clone(), urls)
            .iter()
            .map(|cookie| cookie["name"].as_str().unwrap().to_owned())
            .collect()
    };

    assert_eq!(names(&["http://www.example.com/"]), ["root", "host"]);
    assert_eq!(names(&["https://www.example.com/deep/x"]), ["root", "host", "deep", "secure"]);
    // localhost counts as secure even over plain HTTP.
    assert_eq!(names(&["http://localhost/"]), ["local"]);
    assert_eq!(names(&[]).len(), jar.len());
}

/// A page that fetches JSON once loaded, follows a redirect and fails a request.
const NETWORK_PAGE: &str = "<html><body><button class='one'>a</button><button class='two'>b</button><button class='two'>c</button>\
    <script>window.__clicked = [];\
    document.querySelectorAll('button').forEach((b) => b.addEventListener('click', () => window.__clicked.push(b.textContent)));\
    fetch('/api/data?cursor=1').then((r) => r.json()).then((j) => { window.__data = j; });\
    fetch('/moved');\
    fetch('http://127.0.0.1:1/refused').catch(() => {});</script></body></html>";

fn network_site(request: &str) -> Reply {
    if request.starts_with("get /api/data") {
        Reply {
            status: "200 OK",
            content_type: "application/json",
            headers: String::new(),
            body: r#"{"code": 0, "data": {"notes": [1, 2]}, "big": 12345678901234567890123}"#.to_owned(),
            delay: std::time::Duration::ZERO,
        }
    } else if request.starts_with("get /moved") {
        Reply {
            status: "302 Found",
            content_type: "text/plain",
            headers: "Location: /landed\r\n".to_owned(),
            body: String::new(),
            delay: std::time::Duration::ZERO,
        }
    } else if request.starts_with("get /landed") {
        Reply {
            status: "200 OK",
            content_type: "text/plain",
            headers: String::new(),
            body: "here".to_owned(),
            delay: std::time::Duration::ZERO,
        }
    } else {
        Reply::html(NETWORK_PAGE)
    }
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // reason: one page load walks every event kind in order; split, each half would reload the page
async fn reports_network_traffic_and_reads_response_bodies() {
    use futures_util::StreamExt as _;

    use super::NetworkEvent;

    let Some(executable) = chromium() else { return };
    let (addr, _seen) = serve(network_site).await;
    let context = BrowserContext::launch_browser(
        &executable,
        &launch_options(std::path::Path::new("/unused"), ""),
        &[],
        ContextOptions {
            viewport: Some((1440, 1000)),
            ..ContextOptions::default()
        },
    )
    .await
    .expect("launched");
    let page = context.new_page().await.expect("a page");
    page.add_init_script("window.__init = 'ran';").await.unwrap();
    let mut events = page.network_events().await.unwrap();
    page.goto(&format!("http://{addr}/"), WAIT).await.expect("loaded");
    assert_eq!(page.evaluate("() => window.__init").await.unwrap(), json!("ran"));
    assert_eq!(page.evaluate("() => [innerWidth, innerHeight]").await.unwrap(), json!([1440, 1000]));

    let mut seen = Vec::new();
    let collected = tokio::time::timeout(WAIT, async {
        while let Some(event) = events.next().await {
            seen.push(event);
            let finished_data = seen
                .iter()
                .any(|event| matches!(event, NetworkEvent::Response { url, .. } if url.contains("/api/data")))
                && seen.iter().filter(|event| matches!(event, NetworkEvent::Finished { .. })).count() >= 3;
            let failed = seen.iter().any(|event| matches!(event, NetworkEvent::Failed { .. }));
            if finished_data && failed {
                break;
            }
        }
    })
    .await;
    assert!(collected.is_ok(), "events so far: {seen:?}");

    let document = seen
        .iter()
        .find(|event| matches!(event, NetworkEvent::Request { resource_type, .. } if resource_type == "document"));
    assert!(document.is_some(), "{seen:?}");
    let data_id = seen
        .iter()
        .find_map(|event| match event {
            NetworkEvent::Request { id, url, resource_type } if url.ends_with("/api/data?cursor=1") => {
                assert_eq!(resource_type, "fetch");
                Some(id.clone())
            }
            _ => None,
        })
        .expect("the fetch was seen");
    assert!(seen.contains(&NetworkEvent::Finished { id: data_id.clone() }), "{seen:?}");
    let body = page.response_body(&data_id).await.expect("the body");
    assert_eq!(
        String::from_utf8(body).unwrap(),
        r#"{"code": 0, "data": {"notes": [1, 2]}, "big": 12345678901234567890123}"#
    );
    // The redirect: the 302 hop's response, then the next hop's request under the same id.
    let hop = seen
        .iter()
        .position(|event| matches!(event, NetworkEvent::Response { url, status: 302, .. } if url.ends_with("/moved")))
        .expect("the redirect's response");
    let redirect_id = seen[hop].id().to_owned();
    assert!(
        seen[hop + 1..]
            .iter()
            .any(|event| matches!(event, NetworkEvent::Request { id, url, .. } if *id == redirect_id && url.ends_with("/landed")))
    );
    assert!(
        seen.iter()
            .any(|event| matches!(event, NetworkEvent::Failed { error, .. } if error.starts_with("net::ERR_"))),
        "{seen:?}"
    );

    // Strict mode: one match is clicked, two are refused at once.
    page.click_strict("button.one", WAIT).await.expect("one match");
    let refused = page.click_strict("button.two", WAIT).await.expect_err("two matches");
    assert!(
        refused
            .to_string()
            .starts_with("strict mode violation: locator(\"button.two\") resolved to 2 elements"),
        "{refused}"
    );
    assert_eq!(page.evaluate("() => window.__clicked").await.unwrap(), json!(["a"]));
    let waited = page
        .wait_for_function_with_arg(
            "(count) => document.querySelectorAll('button').length >= count",
            json!(3),
            WAIT,
            Duration::from_millis(50),
        )
        .await
        .unwrap();
    assert_eq!(waited, json!(true));
    let timed_out = page
        .wait_for_function_with_arg(
            "(count) => document.querySelectorAll('button').length >= count",
            json!(4),
            Duration::from_millis(200),
            Duration::from_millis(50),
        )
        .await
        .expect_err("never true");
    assert!(matches!(timed_out, DriverError::Timeout(200)));

    let profile = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(crate::chromium::LAUNCH_PROFILE_PREFIX)
        })
        .count();
    assert!(profile >= 1);
    context.close().await;
    assert!(context.is_closed().await);
}
