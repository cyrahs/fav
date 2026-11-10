//! What the RedNote crawl and its 登录态 login window share about the browser.
//!
//! Python origin: `WEB_ORIGIN`, `_LOGIN_PROBE_SCRIPT`, `_WEBDRIVER_INIT_SCRIPT`,
//! `_USER_AGENT_HINTS_SCRIPT`, `read_login_probe`, `present_mac_user_agent`,
//! `_user_agent_metadata`, `_read_user_agent_hints` and `prepare_page` in
//! `src/web/rednote_browser.py`, and `SESSION_SITE`, `LOGIN_URL`, `SESSION_COOKIE` and
//! `user_id_from_probe` in `src/web/rednote.py`. WP16 ports them because the login window
//! opens the crawl's profile presented the way the crawl presents it; the crawl itself
//! (`PlaywrightNoteBrowser`, the likes walk) is WP17's, and uses these.

use std::time::Duration;

use fav_core::logging;
use fav_runtime::BoxFuture;
use serde_json::{Map, Value, json};

use crate::driver::{BrowserContext, DriverError, Page};

pub const WEB_ORIGIN: &str = "https://www.xiaohongshu.com";
/// `rednote.SESSION_SITE`: the `site_sessions` key and 登录态 site.
pub const SESSION_SITE: &str = "rednote";
/// `rednote.LOGIN_URL`.
pub const LOGIN_URL: &str = "https://www.xiaohongshu.com/explore";
/// `rednote.SESSION_COOKIE`.
pub const SESSION_COOKIE: &str = "web_session";

/// What Chromium puts in its own UA when it has no window, and what the site refuses.
const HEADLESS_UA_TOKEN: &str = "HeadlessChrome";
/// The pod's real OS token, and the frozen Mac token that replaces it (with `arm` client
/// hints under an "Intel" UA, as genuine Chrome on Apple Silicon reports).
const LINUX_UA_OS_TOKEN: &str = "(X11; Linux x86_64)";
const MAC_UA_OS_TOKEN: &str = "(Macintosh; Intel Mac OS X 10_15_7)";
const MAC_NAVIGATOR_PLATFORM: &str = "MacIntel";
const MAC_HINT_PLATFORM: &str = "macOS";
const MAC_HINT_PLATFORM_VERSION: &str = "15.6.1";
const MAC_HINT_ARCHITECTURE: &str = "arm";
const ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.9";
/// The neutral page the client hints are read from when `about:blank` has none.
pub const EXIT_IP_URL: &str = "https://checkip.amazonaws.com";
/// Attempts at reading the hints, and the pause between them.
pub const HINTS_ATTEMPTS: u32 = 2;
pub const HINTS_RETRY: Duration = Duration::from_secs(1);
/// Playwright's default navigation timeout, which the neutral `goto` runs under.
const GOTO_TIMEOUT: Duration = Duration::from_secs(30);

fn log() -> logging::Logger {
    logging::get("rednote")
}

/// `_LOGIN_PROBE_SCRIPT`: one round trip that says what the page is showing; the
/// decision is made in Rust where it can be tested.
pub const LOGIN_PROBE_SCRIPT: &str = r#"() => {
    // __INITIAL_STATE__ is Vue's store, so most leaves are refs and the value is
    // behind _value. Reading them raw yields undefined, which used to make every
    // signed-in check fall through to the DOM.
    const unwrap = (value) => (value && typeof value === 'object' && '_value' in value) ? value._value : value;
    const state = window.__INITIAL_STATE__ || {};
    const user = state.user || {};
    const info = unwrap(user.userInfo) || {};
    // A signed-out visitor is still issued a userInfo, carrying a guest id shaped
    // exactly like a real profile id -- 24 hex characters. `guest` is what tells the
    // two apart, and crawling the wrong one would look exactly like success.
    // Only an explicit true disqualifies: `loggedIn` is the signal, and a signed-in
    // payload that simply omits `guest` must not be read as signed out.
    const guest = info.guest === true;
    const loggedIn = unwrap(user.loggedIn) === true && !guest;
    // Signing in takes two scans, and the second one lives in a different DOM tree:
    // the account QR is inside `.login-modal`, and the account-security QR that
    // follows it is inside a separate captcha app. Both use `img.qrcode-img`, so a
    // selector scoped to the login modal sees only the first half of the flow.
    // Anchored on the captcha *app*, not on the modal inside it: the wrapper varies
    // with the theme the site picks -- observed as `.r-captcha-modal` in one context
    // and absent in another, with the same `img.qrcode-img` underneath either way.
    const modal = document.querySelector('.login-modal');
    const verifyModal = document.querySelector('.fe-captcha-app');
    // `img.qrcode-img` is the one constant. Its container is not: observed as a
    // `.login-modal` overlay on /explore, as `.login-modal.full-page` on the
    // standalone /login page the site sometimes redirects to, and as a
    // `.fe-captcha-app` with no modal wrapper at all for the security scan.
    const verifyQr = document.querySelector('.fe-captcha-app img.qrcode-img');
    // The security scan sits on top and is the live step, so it wins when both exist.
    const qr = verifyQr || document.querySelector('img.qrcode-img');
    // Read off the element rather than off which query found it, so a fourth layout
    // still lands in the right stage instead of silently in the wrong one.
    const stage = qr ? (qr.closest('.fe-captcha-app') ? 'verify' : 'login') : '';
    // Scoped to the sidebar deliberately: an unscoped a[href^="/user/profile/"] matches
    // every note author in the feed -- about thirty of them, none of them this account.
    const own = document.querySelector('.side-bar a[href^="/user/profile/"]');
    return {
        logged_in: loggedIn,
        guest: guest,
        has_login_modal: Boolean(modal),
        has_verify_modal: Boolean(verifyModal),
        qr_stage: stage,
        qr_src: (qr && qr.getAttribute('src')) || '',
        user_id: loggedIn ? String(info.userId || '') : '',
        profile_href: (own && own.getAttribute('href')) || '',
        modal_html: (verifyModal || modal) ? (verifyModal || modal).outerHTML.slice(0, 4000) : '',
    };
}"#;

/// `_WEBDRIVER_INIT_SCRIPT`.
pub const WEBDRIVER_INIT_SCRIPT: &str = "Object.defineProperty(navigator, 'webdriver', {get: () => undefined});";

/// `_USER_AGENT_HINTS_SCRIPT`.
const USER_AGENT_HINTS_SCRIPT: &str = "async () => {
    const data = navigator.userAgentData;
    if (!data) { return {}; }
    const high = await data.getHighEntropyValues(['architecture', 'model', 'platformVersion', 'uaFullVersion']);
    return {
        brands: data.brands.map(b => ({brand: b.brand, version: b.version})),
        mobile: data.mobile,
        platform: data.platform,
        ...high,
    };
}";

/// What the RedNote helpers need from a page: real ([`Page`]) or scripted in tests.
pub trait ProbePage: Send + Sync {
    fn evaluate<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<Value, DriverError>>;
    fn goto<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<(), DriverError>>;
    /// `cdp.send(method, params)` on the page's session.
    fn send<'a>(&'a self, method: &'a str, params: Value) -> BoxFuture<'a, Result<Value, DriverError>>;
}

impl ProbePage for Page {
    fn evaluate<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<Value, DriverError>> {
        Box::pin(Page::evaluate(self, script))
    }

    fn goto<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<(), DriverError>> {
        Box::pin(Page::goto(self, url, GOTO_TIMEOUT))
    }

    fn send<'a>(&'a self, method: &'a str, params: Value) -> BoxFuture<'a, Result<Value, DriverError>> {
        Box::pin(Page::send(self, method, params))
    }
}

/// `read_login_probe(page)`: one sample, or an empty one if the page moved under it (an
/// evaluate that lands during a navigation fails with `Execution context was destroyed`,
/// a reason to look again on the next poll). A non-object answer is empty too.
pub async fn read_login_probe(page: &dyn ProbePage) -> Map<String, Value> {
    match page.evaluate(LOGIN_PROBE_SCRIPT).await {
        Ok(Value::Object(probe)) => probe,
        Ok(_) => Map::new(),
        Err(error) => {
            log().debug(format!("RedNote login probe landed mid-navigation: {error}"));
            Map::new()
        }
    }
}

/// Python truthiness of a value `json.loads` could have produced.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// `str(value or '')`.
pub(crate) fn str_or_empty(value: Option<&Value>) -> String {
    if !truthy(value) {
        return String::new();
    }
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) => fav_runtime::pystr::str_value(&fav_core::pyjson::Value::from_serde(other)),
        None => String::new(),
    }
}

/// `user_id_from_probe(probe)`: the signed-in account's own profile id, gated on being
/// signed in, because a signed-out page offers a guest id in the same place and shape.
pub fn user_id_from_probe(probe: &Map<String, Value>) -> String {
    if !truthy(probe.get("logged_in")) {
        return String::new();
    }
    let user_id = str_or_empty(probe.get("user_id"));
    let user_id = fav_core::pytext::py_strip(&user_id);
    if !user_id.is_empty() {
        return user_id.to_owned();
    }
    let href = str_or_empty(probe.get("profile_href"));
    let href = fav_core::pytext::py_strip(&href);
    let href = href.strip_prefix("/user/profile/").unwrap_or(href);
    href.split('?').next().unwrap_or_default().to_owned()
}

/// `prepare_page(context, page)`: make a fresh page look the way the crawl's does. The
/// init script goes on the whole context; the UA override on the page's session, which
/// the page keeps for its life.
pub async fn prepare_page(context: &BrowserContext, page: &Page) -> Result<(), DriverError> {
    context.add_init_script(WEBDRIVER_INIT_SCRIPT).await?;
    present_mac_user_agent(page, HINTS_RETRY).await?;
    Ok(())
}

/// `present_mac_user_agent(context, page)`: present the browser as desktop Chrome on a
/// Mac at its real version (the `HeadlessChrome` token removed, the Linux OS token
/// replaced, `navigator.platform` and the client hints telling the same story). Returns
/// whether an override was sent: a real Mac browser with a window needs none.
pub async fn present_mac_user_agent(page: &dyn ProbePage, hints_retry: Duration) -> Result<bool, DriverError> {
    present_mac_user_agent_via(page, EXIT_IP_URL, hints_retry).await
}

/// [`present_mac_user_agent`] with the neutral page the hints are read from named: the
/// crawl's tests park the page on a loopback page instead of [`EXIT_IP_URL`].
pub async fn present_mac_user_agent_via(page: &dyn ProbePage, hints_url: &str, hints_retry: Duration) -> Result<bool, DriverError> {
    let user_agent = str_or_empty(Some(&page.evaluate("() => navigator.userAgent").await?));
    let cleaned = user_agent
        .replace(HEADLESS_UA_TOKEN, "Chrome")
        .replace(LINUX_UA_OS_TOKEN, MAC_UA_OS_TOKEN);
    if cleaned == user_agent && user_agent.contains(MAC_UA_OS_TOKEN) {
        return Ok(false);
    }
    let mut override_params = json!({
        "userAgent": cleaned,
        "acceptLanguage": ACCEPT_LANGUAGE,
        "platform": MAC_NAVIGATOR_PLATFORM,
    });
    let mut metadata = user_agent_metadata(page, &cleaned, hints_url, hints_retry).await?;
    if truthy(metadata.get("brands")) {
        metadata.insert("platform".to_owned(), json!(MAC_HINT_PLATFORM));
        metadata.insert("platformVersion".to_owned(), json!(MAC_HINT_PLATFORM_VERSION));
        metadata.insert("architecture".to_owned(), json!(MAC_HINT_ARCHITECTURE));
        metadata.insert("model".to_owned(), json!(""));
        metadata.insert("mobile".to_owned(), json!(false));
        override_params["userAgentMetadata"] = Value::Object(metadata);
    }
    page.send("Emulation.setUserAgentOverride", override_params).await?;
    log().info("RedNote presented the browser as Chrome on macOS");
    Ok(true)
}

/// `_user_agent_metadata(page, user_agent)`: the client hints as the browser reports
/// them, with the version it reports.
async fn user_agent_metadata(
    page: &dyn ProbePage,
    user_agent: &str,
    hints_url: &str,
    hints_retry: Duration,
) -> Result<Map<String, Value>, DriverError> {
    let hints = read_user_agent_hints(page, hints_url, hints_retry).await?;
    let from_ua = user_agent
        .rsplit_once("Chrome/")
        .map_or(user_agent, |(_, rest)| rest)
        .split('.')
        .next()
        .unwrap_or_default()
        .to_owned();
    let full = str_or_empty(hints.get("uaFullVersion"));
    let major = full.split('.').next().unwrap_or_default();
    let version = if major.is_empty() { from_ua } else { major.to_owned() };
    let or_empty_string = |key: &str| if truthy(hints.get(key)) { hints[key].clone() } else { json!("") };
    let mut metadata = Map::new();
    metadata.insert(
        "brands".to_owned(),
        if truthy(hints.get("brands")) {
            hints["brands"].clone()
        } else {
            json!([])
        },
    );
    metadata.insert("fullVersion".to_owned(), or_empty_string("uaFullVersion"));
    metadata.insert("platform".to_owned(), or_empty_string("platform"));
    metadata.insert("platformVersion".to_owned(), or_empty_string("platformVersion"));
    metadata.insert("architecture".to_owned(), or_empty_string("architecture"));
    metadata.insert("model".to_owned(), or_empty_string("model"));
    metadata.insert("mobile".to_owned(), json!(truthy(hints.get("mobile"))));
    metadata.insert("majorVersion".to_owned(), json!(version));
    Ok(metadata)
}

/// `_read_user_agent_hints(page)`: the raw client hints, or nothing when the neutral page
/// will not give them. `about:blank` is not a secure context and has no
/// `navigator.userAgentData`, so the page is first parked on a neutral origin; a read that
/// lands while Chromium commits an error page is tried again after `retry`.
async fn read_user_agent_hints(page: &dyn ProbePage, hints_url: &str, retry: Duration) -> Result<Map<String, Value>, DriverError> {
    if !truthy(Some(&page.evaluate("() => Boolean(navigator.userAgentData)").await?))
        && let Err(error) = page.goto(hints_url).await
    {
        log().warning(format!("RedNote could not open {hints_url} to read the client hints: {error}"));
    }
    for attempt in 1..=HINTS_ATTEMPTS {
        match page.evaluate(USER_AGENT_HINTS_SCRIPT).await {
            Ok(Value::Object(hints)) => return Ok(hints),
            Ok(_) => return Ok(Map::new()),
            Err(error) if attempt < HINTS_ATTEMPTS => {
                log().debug(format!("RedNote client hints read landed mid-navigation, retrying: {error}"));
                tokio::time::sleep(retry).await;
            }
            Err(error) => {
                log().warning(format!(
                    "RedNote could not read the client hints; presenting the UA without them: {error}"
                ));
                return Ok(Map::new());
            }
        }
    }
    Ok(Map::new())
}

pub mod note_browser;

#[cfg(test)]
mod tests;
