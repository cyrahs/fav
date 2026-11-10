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
        }
    }
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
