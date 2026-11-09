//! chromiumoxide covering what `src/web/rednote_browser.py`, `src/web/nikke_runtime.py`
//! and `src/api/browser_login.py` do with Playwright.

use std::path::Path;
use std::time::Duration;

use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::emulation::{
    SetDeviceMetricsOverrideParams, SetLocaleOverrideParams, SetTimezoneOverrideParams, SetUserAgentOverrideParams, UserAgentBrandVersion,
    UserAgentMetadata,
};
use chromiumoxide::cdp::browser_protocol::network::{
    EventLoadingFailed, EventLoadingFinished, EventRequestWillBeSent, EventResponseReceived, GetResponseBodyParams,
};
use chromiumoxide::cdp::browser_protocol::page::{AddScriptToEvaluateOnNewDocumentParams, EventDomContentEventFired, NavigateParams};
use chromiumoxide::cdp::browser_protocol::storage::GetCookiesParams;
use chromiumoxide::error::CdpError;
use chromiumoxide::handler::Handler;
use chromiumoxide::page::Page;
use futures::StreamExt;

/// The arguments Playwright (1.61.0 in `uv.lock`) passes to Chromium for a persistent context,
/// minus `--enable-automation` (`ignore_default_args`), plus the two fav adds.
/// WP16 copies the full list from the pinned Playwright release; this subset is
/// what the probe needs to show an exact, ordered argument list reaches Chromium.
pub const LAUNCH_ARGS: &[&str] = &[
    "--disable-field-trial-config",
    "--disable-background-networking",
    "--no-first-run",
    "--no-default-browser-check",
    "--no-sandbox",
    "--disable-dev-shm-usage",
    "--disable-blink-features=AutomationControlled",
    "--lang=zh-CN",
];

/// The full argv for `launch_persistent_context(user_data_dir=..., headless=..., args=...,
/// ignore_default_args=[...], proxy=...)`.
///
/// `chromiumoxide::BrowserConfig` is deliberately not used to launch: it keeps
/// arguments in a `HashMap` (so their order is random), always appends
/// `--disable-extensions`, and joins repeated keys with commas. fav spawns
/// Chromium itself with this exact argv and attaches over the DevTools socket.
pub fn launch_argv(user_data_dir: &Path, headless: bool, proxy: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = LAUNCH_ARGS.iter().map(|arg| (*arg).to_owned()).collect();
    if headless {
        argv.extend(["--headless".to_owned(), "--hide-scrollbars".to_owned(), "--mute-audio".to_owned()]);
    }
    if let Some(proxy) = proxy {
        argv.push(format!("--proxy-server={proxy}"));
    }
    argv.push(format!("--user-data-dir={}", user_data_dir.display()));
    argv.push("--remote-debugging-port=0".to_owned());
    argv.push("about:blank".to_owned());
    argv
}

/// A Chromium fav started itself, attached over CDP.
#[derive(Debug)]
pub struct LaunchedBrowser {
    pub browser: Browser,
    pub process: tokio::process::Child,
    pub handler: tokio::task::JoinHandle<()>,
}

/// Spawn Chromium with `argv`, wait for "DevTools listening on ws://...", attach.
pub async fn launch(executable: &Path, argv: &[String], timeout: Duration) -> Result<LaunchedBrowser, CdpError> {
    use tokio::io::AsyncBufReadExt;

    let mut process = tokio::process::Command::new(executable)
        .args(argv)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stderr = process.stderr.take().ok_or_else(|| CdpError::msg("no stderr"))?;
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    let websocket = tokio::time::timeout(timeout, async {
        while let Some(line) = lines.next_line().await? {
            if let Some(url) = line.strip_prefix("DevTools listening on ") {
                return Ok(url.trim().to_owned());
            }
        }
        Err(CdpError::msg("Chromium exited before opening DevTools"))
    })
    .await
    .map_err(|_| CdpError::Timeout)??;
    // Keep draining stderr so Chromium never blocks on a full pipe.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    let (browser, handler) = attach(&websocket).await?;
    Ok(LaunchedBrowser { browser, process, handler })
}

/// Attach to a Chromium the login flow started itself with `--remote-debugging-port`.
pub async fn attach(websocket_url: &str) -> Result<(Browser, tokio::task::JoinHandle<()>), CdpError> {
    let (browser, handler) = Browser::connect(websocket_url).await?;
    Ok((browser, tokio::spawn(drive(handler))))
}

async fn drive(mut handler: Handler) {
    while handler.next().await.is_some() {}
}

/// `present_mac_user_agent`: `Emulation.setUserAgentOverride` with client hints.
pub async fn present_user_agent(page: &Page, user_agent: &str, major: &str, full: &str) -> chromiumoxide::Result<()> {
    let brand = |name: &str, version: &str| UserAgentBrandVersion::new(name.to_owned(), version.to_owned());
    let metadata = UserAgentMetadata::builder()
        .brand(brand("Chromium", major))
        .brand(brand("Not.A/Brand", "99"))
        .full_version_lists([brand("Chromium", full), brand("Not.A/Brand", "99.0.0.0")])
        .platform("macOS")
        .platform_version("15.5.0")
        .architecture("arm")
        .model("")
        .mobile(false)
        .build()
        .map_err(CdpError::msg)?;
    let params = SetUserAgentOverrideParams::builder()
        .user_agent(user_agent)
        .accept_language("zh-CN,zh;q=0.9")
        .platform("MacIntel")
        .user_agent_metadata(metadata)
        .build()
        .map_err(CdpError::msg)?;
    page.execute(params).await?;
    Ok(())
}

/// `locale='zh-CN'`, `timezone_id='Asia/Shanghai'`, `viewport={...}`.
pub async fn emulate_context(page: &Page, width: i64, height: i64) -> chromiumoxide::Result<()> {
    page.execute(SetLocaleOverrideParams::builder().locale("zh-CN").build()).await?;
    page.execute(SetTimezoneOverrideParams::new("Asia/Shanghai")).await?;
    page.execute(SetDeviceMetricsOverrideParams::new(width, height, 1.0, false)).await?;
    Ok(())
}

/// `proxy={'server': ..., 'username': ..., 'password': ...}`: Chromium ignores userinfo in
/// `--proxy-server`, so the credentials answer `Fetch.authRequired` (`build_proxy_settings`).
pub async fn authenticate_proxy(page: &Page, username: &str, password: &str) -> chromiumoxide::Result<()> {
    page.authenticate(chromiumoxide::auth::Credentials {
        username: username.to_owned(),
        password: password.to_owned(),
    })
    .await
}

/// `page.locator(selector, has_text=label).first.click()`: the first match whose text
/// contains `label`, scrolled into view and clicked at its centre. No match is an error,
/// never a silent no-op (a missed click once reached a live account, `rednote_browser.py`).
/// Playwright's actionability waits (visible, stable, receives events) are WP16's to add.
pub async fn click_first_with_text(page: &Page, selector: &str, label: &str) -> Result<(), CdpError> {
    for element in page.find_elements(selector).await? {
        if element.inner_text().await?.is_some_and(|text| text.contains(label)) {
            element.scroll_into_view().await?;
            element.click().await?;
            return Ok(());
        }
    }
    Err(CdpError::msg(format!("no {selector} element contains {label:?}")))
}

/// `page.mouse.move(x, y)` then `page.mouse.wheel(0, dy)`.
pub async fn wheel_at(page: &Page, x: f64, y: f64, delta_y: f64) -> Result<(), CdpError> {
    use chromiumoxide::cdp::browser_protocol::input::{DispatchMouseEventParams, DispatchMouseEventType};
    page.move_mouse(chromiumoxide::layout::Point::new(x, y)).await?;
    let wheel = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseWheel)
        .x(x)
        .y(y)
        .delta_x(0.0)
        .delta_y(delta_y)
        .build()
        .map_err(CdpError::msg)?;
    page.execute(wheel).await?;
    Ok(())
}

/// `context.cookies([WEB_ORIGIN])`: only the cookies that would be sent to those URLs.
pub async fn cookies_for(page: &Page, urls: &[&str]) -> Result<Vec<(String, String)>, CdpError> {
    use chromiumoxide::cdp::browser_protocol::network::GetCookiesParams as NetworkGetCookies;
    let params = NetworkGetCookies::builder().urls(urls.iter().map(|url| (*url).to_owned())).build();
    let cookies = page.execute(params).await?;
    Ok(cookies
        .result
        .cookies
        .into_iter()
        .map(|cookie| (cookie.name, cookie.value))
        .collect())
}

/// `add_init_script(...)`.
pub async fn add_init_script(page: &Page, source: &str) -> chromiumoxide::Result<()> {
    page.execute(AddScriptToEvaluateOnNewDocumentParams::new(source)).await?;
    Ok(())
}

/// `goto(url, wait_until='domcontentloaded')`.
pub async fn goto_dom_content_loaded(page: &Page, url: &str, timeout: Duration) -> chromiumoxide::Result<()> {
    let mut fired = page.event_listener::<EventDomContentEventFired>().await?;
    page.execute(NavigateParams::new(url)).await?;
    tokio::time::timeout(timeout, fired.next()).await.map_err(|_| CdpError::Timeout)?;
    Ok(())
}

/// `page.on('request' | 'response' | 'requestfinished' | 'requestfailed')` and `response.json()`.
pub async fn network_listeners(
    page: &Page,
) -> chromiumoxide::Result<(
    chromiumoxide::listeners::EventStream<EventRequestWillBeSent>,
    chromiumoxide::listeners::EventStream<EventResponseReceived>,
    chromiumoxide::listeners::EventStream<EventLoadingFinished>,
    chromiumoxide::listeners::EventStream<EventLoadingFailed>,
)> {
    Ok((
        page.event_listener().await?,
        page.event_listener().await?,
        page.event_listener().await?,
        page.event_listener().await?,
    ))
}

/// `response.json()` once the response has finished loading.
pub async fn response_json(page: &Page, finished: &EventLoadingFinished) -> chromiumoxide::Result<serde_json::Value> {
    let body = page.execute(GetResponseBodyParams::new(finished.request_id.clone())).await?;
    serde_json::from_str(&body.result.body).map_err(CdpError::Serde)
}

/// `context.cookies()` (the login flow reads them with `Storage.getCookies`).
pub async fn all_cookies(browser: &Browser) -> chromiumoxide::Result<Vec<(String, String, String)>> {
    let cookies = browser.execute(GetCookiesParams::default()).await?;
    Ok(cookies
        .result
        .cookies
        .into_iter()
        .map(|cookie| (cookie.domain, cookie.name, cookie.value))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// Chromium to drive: `FAV_TEST_CHROMIUM`, else the cloud sessions' Playwright build.
    /// CI sets the variable, so there the test always runs.
    fn chromium() -> Option<PathBuf> {
        std::env::var_os("FAV_TEST_CHROMIUM")
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from("/opt/pw-browsers/chromium")).filter(|path| path.exists()))
    }

    /// Two tabs like RedNote's profile page, and a page tall enough to scroll.
    const TABS_PAGE: &str = "<html><body style='margin:0'>\
        <div style='height:3000px'>\
        <span class='reds-tab-item' onclick='window.__tab=\"notes\"'>笔记</span>\
        <span class='reds-tab-item' onclick='window.__tab=\"liked\"'>赞过 12</span>\
        </div><script>window.__loads=(Number(sessionStorage.loads)||0)+1;sessionStorage.loads=window.__loads;</script></body></html>";

    /// A one-file HTTP server on loopback: `/` is a page that fetches `/like/page`.
    async fn serve() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 4096];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..read]);
                    let (content_type, body, cookie) = if request.starts_with("GET /like/page") {
                        ("application/json", r#"{"data":{"notes":[1,2]}}"#.to_owned(), "")
                    } else if request.starts_with("GET /tabs") {
                        (
                            "text/html; charset=utf-8",
                            TABS_PAGE.to_owned(),
                            "Set-Cookie: tab_seen=1; Path=/tabs\r\n",
                        )
                    } else {
                        (
                            "text/html",
                            "<html><body><script>fetch('/like/page')</script></body></html>".to_owned(),
                            "Set-Cookie: web_session=abc; Path=/\r\n",
                        )
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{cookie}Connection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        addr
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drives_a_local_page_like_the_rednote_browser() {
        let Some(executable) = chromium() else {
            assert!(std::env::var_os("CI").is_none(), "CI must set FAV_TEST_CHROMIUM");
            return;
        };
        let addr = serve().await;
        let profile = tempfile::tempdir().unwrap();
        let argv = launch_argv(profile.path(), true, None);
        let LaunchedBrowser {
            mut browser,
            mut process,
            handler,
        } = launch(&executable, &argv, Duration::from_secs(30)).await.unwrap();

        let page = browser.new_page("about:blank").await.unwrap();
        add_init_script(
            &page,
            "Object.defineProperty(navigator, 'webdriver', {get: () => undefined}); window.__probe = 41 + 1;",
        )
        .await
        .unwrap();
        emulate_context(&page, 1280, 800).await.unwrap();
        present_user_agent(
            &page,
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Chrome/140.0.0.0",
            "140",
            "140.0.0.0",
        )
        .await
        .unwrap();
        let (_requests, mut responses, mut finished, _failed) = network_listeners(&page).await.unwrap();

        goto_dom_content_loaded(&page, &format!("http://{addr}/"), Duration::from_secs(20))
            .await
            .unwrap();

        let like_request = loop {
            let response = tokio::time::timeout(Duration::from_secs(20), responses.next())
                .await
                .unwrap()
                .unwrap();
            if response.response.url.ends_with("/like/page") {
                break response.request_id.clone();
            }
        };
        let done = loop {
            let event = tokio::time::timeout(Duration::from_secs(20), finished.next())
                .await
                .unwrap()
                .unwrap();
            if event.request_id == like_request {
                break event;
            }
        };
        assert_eq!(
            response_json(&page, &done).await.unwrap(),
            serde_json::json!({"data": {"notes": [1, 2]}})
        );

        let probe: i64 = page.evaluate("() => window.__probe").await.unwrap().into_value().unwrap();
        assert_eq!(probe, 42);
        let webdriver_hidden: bool = page
            .evaluate("() => navigator.webdriver === undefined")
            .await
            .unwrap()
            .into_value()
            .unwrap();
        assert!(webdriver_hidden);
        let platform: String = page
            .evaluate("() => navigator.userAgentData.platform")
            .await
            .unwrap()
            .into_value()
            .unwrap();
        assert_eq!(platform, "macOS");
        let zone: String = page
            .evaluate("() => Intl.DateTimeFormat().resolvedOptions().timeZone")
            .await
            .unwrap()
            .into_value()
            .unwrap();
        assert_eq!(zone, "Asia/Shanghai");

        let cookies = all_cookies(&browser).await.unwrap();
        assert!(
            cookies.iter().any(|(_, name, value)| name == "web_session" && value == "abc"),
            "{cookies:?}"
        );

        browser.close().await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(10), process.wait()).await;
        handler.abort();
    }

    /// An HTTP proxy on loopback that demands Basic auth (`fav:secret`) and serves every
    /// proxied request itself, so no name is ever resolved and nothing leaves the host.
    async fn authenticating_proxy() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 8192];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..read]).to_ascii_lowercase();
                    let response = if request.contains("proxy-authorization: basic zmf2onnly3jldA==".to_ascii_lowercase().as_str()) {
                        let body = "<html><body>proxied</body></html>";
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    } else {
                        "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"fav\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        addr
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_proxy_authentication_like_playwright() {
        let Some(executable) = chromium() else {
            assert!(std::env::var_os("CI").is_none(), "CI must set FAV_TEST_CHROMIUM");
            return;
        };
        let proxy = authenticating_proxy().await;
        let profile = tempfile::tempdir().unwrap();
        let argv = launch_argv(profile.path(), true, Some(&format!("http://{proxy}")));
        let LaunchedBrowser {
            mut browser,
            mut process,
            handler,
        } = launch(&executable, &argv, Duration::from_secs(30)).await.unwrap();

        let page = browser.new_page("about:blank").await.unwrap();
        authenticate_proxy(&page, "fav", "secret").await.unwrap();
        goto_dom_content_loaded(&page, "http://fav.probe.invalid/", Duration::from_secs(20))
            .await
            .unwrap();
        let text: String = page
            .evaluate("() => document.body.textContent")
            .await
            .unwrap()
            .into_value()
            .unwrap();
        assert_eq!(text, "proxied");

        browser.close().await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(10), process.wait()).await;
        handler.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clicks_scrolls_reloads_and_filters_cookies_like_the_liked_tab_flow() {
        let Some(executable) = chromium() else {
            assert!(std::env::var_os("CI").is_none(), "CI must set FAV_TEST_CHROMIUM");
            return;
        };
        let addr = serve().await;
        let profile = tempfile::tempdir().unwrap();
        let argv = launch_argv(profile.path(), true, None);
        let LaunchedBrowser {
            mut browser,
            mut process,
            handler,
        } = launch(&executable, &argv, Duration::from_secs(30)).await.unwrap();
        let page = browser.new_page("about:blank").await.unwrap();
        emulate_context(&page, 1280, 800).await.unwrap();

        goto_dom_content_loaded(&page, &format!("http://{addr}/"), Duration::from_secs(20))
            .await
            .unwrap();
        goto_dom_content_loaded(&page, &format!("http://{addr}/tabs"), Duration::from_secs(20))
            .await
            .unwrap();

        click_first_with_text(&page, ".reds-tab-item", "赞过").await.unwrap();
        let tab: String = page.evaluate("() => window.__tab").await.unwrap().into_value().unwrap();
        assert_eq!(tab, "liked");
        let missing = click_first_with_text(&page, ".reds-tab-item", "收藏").await;
        assert!(missing.is_err(), "a missing tab must be an error, not a silent no-op");

        wheel_at(&page, 640.0, 400.0, 900.0).await.unwrap();
        let mut scrolled = 0.0_f64;
        for _ in 0..50 {
            scrolled = page.evaluate("() => window.scrollY").await.unwrap().into_value().unwrap();
            if scrolled > 0.0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(scrolled > 0.0, "the wheel did not scroll the page");

        page.reload().await.unwrap();
        let loads: i64 = page.evaluate("() => window.__loads").await.unwrap().into_value().unwrap();
        assert_eq!(loads, 2);

        let on_tabs = cookies_for(&page, &[&format!("http://{addr}/tabs")]).await.unwrap();
        assert!(
            on_tabs.iter().any(|(name, _)| name == "tab_seen") && on_tabs.iter().any(|(name, _)| name == "web_session"),
            "{on_tabs:?}"
        );
        let on_root = cookies_for(&page, &[&format!("http://{addr}/")]).await.unwrap();
        assert!(!on_root.iter().any(|(name, _)| name == "tab_seen"), "{on_root:?}");

        browser.close().await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(10), process.wait()).await;
        handler.abort();
    }
}
