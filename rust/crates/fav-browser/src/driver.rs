//! The CDP driver that replaces Playwright.
//!
//! Python origin: the parts of Playwright's Python API fav calls
//! (`launch_persistent_context`, `context.pages` / `new_page`, `add_init_script`,
//! `context.cookies(urls)`, `context.on('page' | 'close')`, `page.goto` / `reload` /
//! `evaluate` / `mouse` / `locator(...).click()`, `context.new_cdp_session(page)`), built
//! on chromiumoxide 0.9.1 as a CDP client only (`API-COVERAGE.md`).
//!
//! Chromium is started by [`crate::chromium`] with Playwright's own command line and
//! attached to over its DevTools socket. Each page gets what Playwright applies to every
//! page of a context (`FrameSession._initialize` in `crPage.ts`): focus emulation, the
//! viewport (or none, for `no_viewport`), `Accept-Language` and the locale, the time zone,
//! Playwright's default media features, the proxy's credentials and the context's init
//! scripts. What Playwright does beyond that (its utility world and bindings, screencast,
//! file-chooser interception, default font families in headless mode,
//! `Page.setScreenSizeOverride`) is not reproduced (`DIVERGENCES.md` D161).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chromiumoxide::Browser;
use chromiumoxide::cdp::browser_protocol::browser::CloseParams as BrowserCloseParams;
use chromiumoxide::cdp::browser_protocol::emulation::{
    MediaFeature, SetDeviceMetricsOverrideParams, SetEmulatedMediaParams, SetFocusEmulationEnabledParams, SetLocaleOverrideParams,
    SetTimezoneOverrideParams, SetUserAgentOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::input::{DispatchMouseEventParams, DispatchMouseEventType, MouseButton};
use chromiumoxide::cdp::browser_protocol::page::{
    AddScriptToEvaluateOnNewDocumentParams, EventDomContentEventFired, NavigateParams, ReloadParams,
};
use chromiumoxide::cdp::browser_protocol::storage::GetCookiesParams;
use chromiumoxide::cdp::browser_protocol::target::{CloseTargetParams, EventTargetCreated, TargetId};
use chromiumoxide::cdp::js_protocol::runtime::{CallArgument, CallFunctionOnParams, EvaluateParams};
use chromiumoxide::error::CdpError;
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::handler::viewport::Viewport;
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::chromium::{ChromiumProcess, LaunchError, LaunchOptions, persistent_context_argv};

/// Playwright's default navigation timeout (30 s).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long Chromium gets to open DevTools after it was started: Playwright's launch
/// timeout (`DEFAULT_PLAYWRIGHT_LAUNCH_TIMEOUT_IN_MILLISECONDS`, 180 s).
const LAUNCH_TIMEOUT: Duration = Duration::from_mins(3);
/// How long a closing browser gets to exit before its process group is killed:
/// Playwright's `closeOrKill(DEFAULT_PLAYWRIGHT_TIMEOUT)`, 30 s. Chromium writes its cookie
/// store on the way out, so a profile-only login (RedNote) needs the whole of it.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a fresh persistent context is given to report the tab it opened.
const FIRST_PAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// A driver call failed.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error(transparent)]
    Launch(#[from] LaunchError),
    #[error("{0}")]
    Cdp(#[from] CdpError),
    /// A navigation Chromium refused (`net::ERR_...`), as Playwright reports it.
    #[error("{0}")]
    Navigation(String),
    /// Playwright's `TimeoutError`.
    #[error("Timeout {0}ms exceeded.")]
    Timeout(u128),
    /// A script threw; the text is the exception's description.
    #[error("{0}")]
    Evaluation(String),
    /// The target, page or browser is gone (Playwright's `TargetClosedError`).
    #[error("Target page, context or browser has been closed")]
    Closed,
    /// A locator matched nothing it could click within the timeout.
    #[error("{0}")]
    Locator(String),
}

impl DriverError {
    /// The Python class Playwright raises for this, for messages that name it.
    pub fn class_name(&self) -> &'static str {
        match self {
            Self::Timeout(_) => "TimeoutError",
            Self::Closed => "TargetClosedError",
            _ => "Error",
        }
    }
}

fn millis(duration: Duration) -> u128 {
    duration.as_millis()
}

/// The context options fav passes to `launch_persistent_context` beyond the command line.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ContextOptions {
    /// `locale=`: `Accept-Language` and `Emulation.setLocaleOverride`.
    pub locale: Option<String>,
    /// `timezone_id=`.
    pub timezone_id: Option<String>,
    /// `viewport=` as `(width, height)`; `None` is `no_viewport=True`.
    pub viewport: Option<(u32, u32)>,
    /// The proxy's `username` / `password`, answered on `Fetch.authRequired`.
    pub proxy_credentials: Option<(String, String)>,
}

impl std::fmt::Debug for ContextOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextOptions")
            .field("locale", &self.locale)
            .field("timezone_id", &self.timezone_id)
            .field("viewport", &self.viewport)
            .field("proxy_credentials", &self.proxy_credentials.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// A persistent browser context: one Chromium on one profile.
pub struct BrowserContext {
    browser: Arc<Browser>,
    process: Mutex<ChromiumProcess>,
    closed: Arc<AtomicBool>,
    handler: tokio::task::JoinHandle<()>,
    options: ContextOptions,
    init_scripts: Arc<Mutex<Vec<String>>>,
    watcher: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for BrowserContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserContext")
            .field("closed", &self.closed.load(Ordering::SeqCst))
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl BrowserContext {
    /// `launch_persistent_context(...)`: start `executable` with Playwright's command line
    /// for `launch` plus `env`, attach over CDP and apply `options` to every page.
    pub async fn launch(
        executable: &std::path::Path,
        launch: &LaunchOptions,
        env: &[(String, String)],
        options: ContextOptions,
    ) -> Result<Self, DriverError> {
        let argv = persistent_context_argv(launch);
        let mut process = ChromiumProcess::spawn(executable, &argv, env, LAUNCH_TIMEOUT).await?;
        let config = HandlerConfig {
            // Playwright's defaults: certificate errors are errors, and the viewport is
            // applied by this driver (or not at all) rather than chromiumoxide's 800x600.
            ignore_https_errors: false,
            viewport: None::<Viewport>,
            ..HandlerConfig::default()
        };
        let (browser, mut handler) = match Browser::connect_with_config(process.devtools_url().to_owned(), config).await {
            Ok(connected) => connected,
            Err(error) => {
                process.kill().await;
                return Err(error.into());
            }
        };
        let browser = Arc::new(browser);
        let closed = Arc::new(AtomicBool::new(false));
        let handler_closed = Arc::clone(&closed);
        let handler = tokio::spawn(async move {
            while handler.next().await.is_some() {}
            // chromiumoxide 0.9.1's handler stays pending when the socket drops, so this is
            // reached only rarely; `close()` sets the flag, and a browser that exited on its
            // own is noticed through its process (`is_closed`).
            handler_closed.store(true, Ordering::SeqCst);
        });
        let init_scripts = Arc::new(Mutex::new(Vec::new()));
        let watcher = spawn_page_watcher(&browser, options.clone(), Arc::clone(&init_scripts)).await;
        Ok(Self {
            browser,
            process: Mutex::new(process),
            closed,
            handler,
            options,
            init_scripts,
            watcher,
        })
    }

    /// Whether the browser is gone (`context.on('close')` fired).
    pub async fn is_closed(&self) -> bool {
        if self.closed.load(Ordering::SeqCst) {
            return true;
        }
        let exited = self.process.lock().await.has_exited();
        if exited {
            self.closed.store(true, Ordering::SeqCst);
        }
        exited
    }

    /// `context.pages`.
    pub async fn pages(&self) -> Result<Vec<Page>, DriverError> {
        let pages = self.browser.pages().await?;
        Ok(pages.into_iter().map(Page::new).collect())
    }

    /// `context.pages[0] if context.pages else await context.new_page()`: the tab a
    /// persistent context opens with (`about:blank`), once Chromium has reported it.
    pub async fn first_page(&self) -> Result<Page, DriverError> {
        let deadline = Instant::now() + FIRST_PAGE_TIMEOUT;
        loop {
            if let Some(page) = self.browser.pages().await?.into_iter().next() {
                let page = Page::new(page);
                page.prepare(&self.options, &self.init_scripts.lock().await).await?;
                return Ok(page);
            }
            if Instant::now() >= deadline {
                return self.new_page().await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// `context.new_page()`.
    pub async fn new_page(&self) -> Result<Page, DriverError> {
        let page = Page::new(self.browser.new_page("about:blank").await?);
        page.prepare(&self.options, &self.init_scripts.lock().await).await?;
        Ok(page)
    }

    /// `context.add_init_script(source)`: on every page from now on, and on the open ones
    /// for their next document.
    pub async fn add_init_script(&self, source: &str) -> Result<(), DriverError> {
        self.init_scripts.lock().await.push(source.to_owned());
        for page in self.browser.pages().await? {
            page.execute(AddScriptToEvaluateOnNewDocumentParams::new(source)).await?;
        }
        Ok(())
    }

    /// `context.cookies(urls)`: the context's whole jar (`Storage.getCookies` on the
    /// browser), narrowed to `urls` as Playwright's `filterCookies` does, in Playwright's
    /// shape (`name`, `value`, `domain`, `path`, `expires`, `httpOnly`, `secure`, `sameSite`).
    pub async fn cookies(&self, urls: &[&str]) -> Result<Vec<Value>, DriverError> {
        let jar = self.browser.execute(GetCookiesParams::default()).await?;
        let cookies: Vec<Value> = jar
            .result
            .cookies
            .iter()
            .map(|cookie| {
                let mut entry = json!({
                    "name": cookie.name,
                    "value": cookie.value,
                    "domain": cookie.domain,
                    "path": cookie.path,
                    "expires": cookie.expires,
                    "httpOnly": cookie.http_only,
                    "secure": cookie.secure,
                });
                if let Some(same_site) = &cookie.same_site {
                    entry["sameSite"] = json!(same_site.as_ref());
                }
                entry
            })
            .collect();
        Ok(filter_cookies(cookies, urls))
    }

    /// Pages the browser opens from now on (`context.on('page')`), as target ids.
    pub async fn page_created(&self) -> Result<impl futures_util::Stream<Item = TargetId> + Unpin + use<>, DriverError> {
        let events = self.browser.event_listener::<EventTargetCreated>().await?;
        Ok(events.filter_map(|event| {
            futures_util::future::ready((event.target_info.r#type == "page").then(|| event.target_info.target_id.clone()))
        }))
    }

    /// Close one tab by its target id (`page.close()`).
    pub async fn close_target(&self, target: TargetId) -> Result<(), DriverError> {
        self.browser.execute(CloseTargetParams::new(target)).await?;
        Ok(())
    }

    /// `context.close()`: ask the browser to close and give it [`CLOSE_TIMEOUT`] in all to
    /// exit (`closeOrKill`), then kill what is left of its process group.
    pub async fn close(&self) {
        self.watcher.abort();
        let deadline = Instant::now() + CLOSE_TIMEOUT;
        if !self.closed.load(Ordering::SeqCst) {
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, self.browser.execute(BrowserCloseParams::default())).await;
        }
        let mut process = self.process.lock().await;
        let _ = process.wait_for_exit(deadline.saturating_duration_since(Instant::now())).await;
        process.kill().await;
        self.closed.store(true, Ordering::SeqCst);
        self.handler.abort();
    }
}

impl Drop for BrowserContext {
    fn drop(&mut self) {
        self.watcher.abort();
        self.handler.abort();
    }
}

/// Apply the context's options to the pages the site opens itself (popups, `_blank` links).
async fn spawn_page_watcher(
    browser: &Arc<Browser>,
    options: ContextOptions,
    scripts: Arc<Mutex<Vec<String>>>,
) -> tokio::task::JoinHandle<()> {
    let Ok(mut events) = browser.event_listener::<EventTargetCreated>().await else {
        return tokio::spawn(async {});
    };
    let browser_pages = Arc::clone(browser);
    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            if event.target_info.r#type != "page" || event.target_info.opener_id.is_none() {
                continue;
            }
            if let Ok(page) = browser_pages.get_page(event.target_info.target_id.clone()).await {
                let scripts = scripts.lock().await.clone();
                let _ = Page::new(page).prepare(&options, &scripts).await;
            }
        }
    })
}

/// Playwright's `filterCookies(cookies, urls)`: a cookie whose domain covers the URL's
/// host, whose path is a prefix of the URL's path, and that is not `secure` on plain HTTP
/// (except `localhost`). No URLs keeps the whole jar.
pub fn filter_cookies(cookies: Vec<Value>, urls: &[&str]) -> Vec<Value> {
    let parsed: Vec<url::Url> = urls.iter().filter_map(|url| url::Url::parse(url).ok()).collect();
    if urls.is_empty() {
        return cookies;
    }
    cookies
        .into_iter()
        .filter(|cookie| {
            let domain = cookie["domain"].as_str().unwrap_or_default();
            let domain = if domain.starts_with('.') {
                domain.to_owned()
            } else {
                format!(".{domain}")
            };
            let path = cookie["path"].as_str().unwrap_or_default();
            let secure = cookie["secure"].as_bool().unwrap_or(false);
            parsed.iter().any(|url| {
                let host = url.host_str().unwrap_or_default();
                if !format!(".{host}").ends_with(&domain) || !url.path().starts_with(path) {
                    return false;
                }
                let local = host == "localhost" || host.ends_with(".localhost");
                !(url.scheme() != "https" && !local && secure)
            })
        })
        .collect()
}

/// One tab.
#[derive(Debug, Clone)]
pub struct Page {
    inner: chromiumoxide::Page,
}

impl Page {
    fn new(inner: chromiumoxide::Page) -> Self {
        Self { inner }
    }

    /// The chromiumoxide page, for CDP calls this wrapper does not name.
    pub fn cdp(&self) -> &chromiumoxide::Page {
        &self.inner
    }

    /// The page's target id.
    pub fn target_id(&self) -> &TargetId {
        self.inner.target_id()
    }

    /// What Playwright sends to every page of a context, in its order.
    async fn prepare(&self, options: &ContextOptions, scripts: &[String]) -> Result<(), DriverError> {
        self.inner.execute(SetFocusEmulationEnabledParams::new(true)).await?;
        if let Some((width, height)) = options.viewport {
            self.inner
                .execute(SetDeviceMetricsOverrideParams::new(i64::from(width), i64::from(height), 1.0, false))
                .await?;
        }
        if let Some(locale) = &options.locale {
            self.inner
                .execute(
                    SetUserAgentOverrideParams::builder()
                        .user_agent("")
                        .accept_language(locale)
                        .build()
                        .map_err(CdpError::msg)?,
                )
                .await?;
            if let Err(error) = self.inner.execute(SetLocaleOverrideParams::builder().locale(locale).build()).await
                && !error.to_string().contains("Another locale override is already in effect")
            {
                return Err(error.into());
            }
        }
        if let Some(zone) = &options.timezone_id
            && let Err(error) = self.inner.execute(SetTimezoneOverrideParams::new(zone)).await
            && !error.to_string().contains("Timezone override is already in effect")
        {
            return Err(error.into());
        }
        let feature = |name: &str, value: &str| MediaFeature::new(name, value);
        self.inner
            .execute(
                SetEmulatedMediaParams::builder()
                    .media("")
                    .features(vec![
                        feature("prefers-color-scheme", "light"),
                        feature("prefers-reduced-motion", "no-preference"),
                        feature("forced-colors", "none"),
                        feature("prefers-contrast", "no-preference"),
                    ])
                    .build(),
            )
            .await?;
        for source in scripts {
            self.inner
                .execute(
                    AddScriptToEvaluateOnNewDocumentParams::builder()
                        .source(source)
                        .run_immediately(true)
                        .build()
                        .map_err(CdpError::msg)?,
                )
                .await?;
        }
        if let Some((username, password)) = &options.proxy_credentials {
            self.inner
                .authenticate(chromiumoxide::auth::Credentials {
                    username: username.clone(),
                    password: password.clone(),
                })
                .await?;
        }
        Ok(())
    }

    /// `page.url`.
    pub async fn url(&self) -> Result<String, DriverError> {
        Ok(self.inner.url().await?.unwrap_or_default())
    }

    /// `page.goto(url, wait_until='domcontentloaded', timeout=...)`.
    pub async fn goto(&self, url: &str, timeout: Duration) -> Result<(), DriverError> {
        let mut fired = self.inner.event_listener::<EventDomContentEventFired>().await?;
        let started = async {
            let navigated = self.inner.execute(NavigateParams::new(url)).await?;
            if let Some(error) = navigated.result.error_text.as_deref().filter(|text| !text.is_empty()) {
                return Err(DriverError::Navigation(format!("{error} at {url}")));
            }
            fired.next().await.ok_or(DriverError::Closed)?;
            Ok(())
        };
        tokio::time::timeout(timeout, started)
            .await
            .map_err(|_| DriverError::Timeout(millis(timeout)))?
    }

    /// `page.reload(wait_until='domcontentloaded', timeout=...)`.
    pub async fn reload(&self, timeout: Duration) -> Result<(), DriverError> {
        let mut fired = self.inner.event_listener::<EventDomContentEventFired>().await?;
        let reloaded = async {
            self.inner.execute(ReloadParams::default()).await?;
            fired.next().await.ok_or(DriverError::Closed)?;
            Ok(())
        };
        tokio::time::timeout(timeout, reloaded)
            .await
            .map_err(|_| DriverError::Timeout(millis(timeout)))?
    }

    /// `page.evaluate(script)`: an expression, or a function (`() => ...`, `async () =>
    /// ...`) that is called; promises are awaited and the result comes back as JSON
    /// (`undefined` as `null`).
    pub async fn evaluate(&self, script: &str) -> Result<Value, DriverError> {
        let params = EvaluateParams::builder()
            .expression(script)
            .eval_as_function_fallback(true)
            .await_promise(true)
            .return_by_value(true)
            .build()
            .map_err(CdpError::msg)?;
        match self.inner.evaluate(params).await {
            Ok(result) => Ok(result.value().cloned().unwrap_or(Value::Null)),
            Err(CdpError::JavascriptException(details)) => Err(DriverError::Evaluation(exception_text(&details))),
            Err(error) => Err(error.into()),
        }
    }

    /// `page.evaluate(function, arg)`: `function` called with one JSON argument.
    pub async fn evaluate_with_arg(&self, function: &str, arg: Value) -> Result<Value, DriverError> {
        let context = self.inner.execution_context().await?;
        let mut builder = CallFunctionOnParams::builder()
            .function_declaration(function)
            .argument(CallArgument::builder().value(arg).build())
            .await_promise(true)
            .return_by_value(true);
        if let Some(context) = context {
            builder = builder.execution_context_id(context);
        }
        match self.inner.evaluate_function(builder.build().map_err(CdpError::msg)?).await {
            Ok(result) => Ok(result.value().cloned().unwrap_or(Value::Null)),
            Err(CdpError::JavascriptException(details)) => Err(DriverError::Evaluation(exception_text(&details))),
            Err(error) => Err(error.into()),
        }
    }

    /// `page.wait_for_function(script, timeout=...)`: poll until it returns a truthy value.
    pub async fn wait_for_function(&self, script: &str, timeout: Duration, poll: Duration) -> Result<Value, DriverError> {
        let deadline = Instant::now() + timeout;
        loop {
            let value = self.evaluate(script).await?;
            if truthy(&value) {
                return Ok(value);
            }
            if Instant::now() >= deadline {
                return Err(DriverError::Timeout(millis(timeout)));
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// `page.mouse.move(x, y)`.
    pub async fn mouse_move(&self, x: f64, y: f64) -> Result<(), DriverError> {
        self.dispatch_mouse(DispatchMouseEventType::MouseMoved, x, y, None, 0).await
    }

    /// `page.mouse.wheel(delta_x, delta_y)` at the pointer's last position.
    pub async fn mouse_wheel(&self, x: f64, y: f64, delta_x: f64, delta_y: f64) -> Result<(), DriverError> {
        let params = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseWheel)
            .x(x)
            .y(y)
            .delta_x(delta_x)
            .delta_y(delta_y)
            .build()
            .map_err(CdpError::msg)?;
        self.inner.execute(params).await?;
        Ok(())
    }

    /// `page.mouse.click(x, y)`: move, press, release with the left button.
    pub async fn mouse_click(&self, x: f64, y: f64) -> Result<(), DriverError> {
        self.mouse_move(x, y).await?;
        self.dispatch_mouse(DispatchMouseEventType::MousePressed, x, y, Some(MouseButton::Left), 1)
            .await?;
        self.dispatch_mouse(DispatchMouseEventType::MouseReleased, x, y, Some(MouseButton::Left), 1)
            .await
    }

    async fn dispatch_mouse(
        &self,
        kind: DispatchMouseEventType,
        x: f64,
        y: f64,
        button: Option<MouseButton>,
        clicks: i64,
    ) -> Result<(), DriverError> {
        let mut builder = DispatchMouseEventParams::builder().r#type(kind).x(x).y(y);
        if let Some(button) = button {
            builder = builder.button(button).click_count(clicks);
        }
        self.inner.execute(builder.build().map_err(CdpError::msg)?).await?;
        Ok(())
    }

    /// `locator(selector, has_text=text).first.click(timeout=...)`: the first element in
    /// document order matching `selector` whose text contains `text` (case-insensitive,
    /// whitespace collapsed, as Playwright's `has_text`), clicked at its centre once it is
    /// actionable. A locator that never becomes clickable is an error, never a no-op.
    pub async fn click_first_with_text(&self, selector: &str, text: &str, timeout: Duration) -> Result<(), DriverError> {
        self.click_located(
            FIND_FIRST_WITH_TEXT,
            json!([selector, text]),
            timeout,
            &format!("locator({selector:?}, has_text={text:?})"),
        )
        .await
    }

    /// `get_by_text(text, exact=True).last.click(timeout=...)`: the last element whose own
    /// text, whitespace collapsed, is exactly `text`.
    pub async fn click_last_with_exact_text(&self, text: &str, timeout: Duration) -> Result<(), DriverError> {
        self.click_located(
            FIND_LAST_WITH_EXACT_TEXT,
            json!([text]),
            timeout,
            &format!("get_by_text({text:?}, exact=True).last"),
        )
        .await
    }

    /// Playwright's actionability checks, then the click: the element must be attached,
    /// visible, enabled, stable (the same box over two animation frames) and the one that
    /// receives the pointer at its centre. Retried until `timeout`.
    async fn click_located(&self, finder: &str, args: Value, timeout: Duration, description: &str) -> Result<(), DriverError> {
        let script = format!("(args) => {{ const find = {finder}; return ({ACTIONABILITY})(find(...args)); }}");
        let deadline = Instant::now() + timeout;
        let mut last = String::from("no element");
        loop {
            match self.evaluate_with_arg(&script, args.clone()).await {
                Ok(answer) if answer["state"] == "ready" => {
                    let (x, y) = (answer["x"].as_f64().unwrap_or(0.0), answer["y"].as_f64().unwrap_or(0.0));
                    return self.mouse_click(x, y).await;
                }
                Ok(answer) => answer["state"].as_str().unwrap_or("unknown").clone_into(&mut last),
                // A navigation replaced the document under the check; look again.
                Err(DriverError::Evaluation(text) | DriverError::Cdp(CdpError::ChromeMessage(text))) => last = text,
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(DriverError::Locator(format!(
                    "Timeout {}ms exceeded waiting for {description} to be clickable (last: {last})",
                    millis(timeout)
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `cdp.send(method, params)` on the page's own session, the params sent as given
    /// (`context.new_cdp_session(page)`; the session is the page's, so an override sent
    /// here lives as long as the page).
    pub async fn send(&self, method: &str, params: Value) -> Result<Value, DriverError> {
        let response = self
            .inner
            .execute(RawCommand {
                method: method.to_owned(),
                params,
            })
            .await?;
        Ok(response.result)
    }

    /// `page.close()`.
    pub async fn close(self) -> Result<(), DriverError> {
        self.inner.close().await?;
        Ok(())
    }
}

/// A CDP command with hand-written params, for the calls whose exact payload matters.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(transparent)]
struct RawCommand {
    #[serde(skip)]
    method: String,
    params: Value,
}

impl chromiumoxide::Method for RawCommand {
    fn identifier(&self) -> chromiumoxide::types::MethodId {
        self.method.clone().into()
    }
}

impl chromiumoxide::Command for RawCommand {
    type Response = Value;
}

fn exception_text(details: &chromiumoxide::cdp::js_protocol::runtime::ExceptionDetails) -> String {
    details
        .exception
        .as_ref()
        .and_then(|exception| exception.description.clone())
        .unwrap_or_else(|| details.text.clone())
}

/// JavaScript truthiness of a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0 && !number.is_nan()),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

const FIND_FIRST_WITH_TEXT: &str = "(selector, text) => {
    const norm = (s) => String(s || '').replace(/\\s+/g, ' ').trim().toLowerCase();
    const wanted = norm(text);
    return [...document.querySelectorAll(selector)].find((el) => norm(el.textContent).includes(wanted)) || null;
}";

const FIND_LAST_WITH_EXACT_TEXT: &str = "(text) => {
    const norm = (s) => String(s || '').replace(/\\s+/g, ' ').trim();
    const wanted = norm(text);
    const all = [...document.querySelectorAll('body *')].filter((el) => norm(el.textContent) === wanted);
    // The innermost matches: an element whose child matches too is a wrapper.
    const own = all.filter((el) => ![...el.children].some((child) => norm(child.textContent) === wanted));
    return own.length ? own[own.length - 1] : null;
}";

const ACTIONABILITY: &str = "async (el) => {
    if (!el) { return {state: 'no element'}; }
    if (!el.isConnected) { return {state: 'detached'}; }
    el.scrollIntoView({block: 'center', inline: 'center', behavior: 'instant'});
    const frame = () => new Promise((resolve) => requestAnimationFrame(() => resolve()));
    const box = () => { const r = el.getBoundingClientRect(); return [r.left, r.top, r.width, r.height]; };
    await frame();
    const first = box();
    await frame();
    const second = box();
    const style = getComputedStyle(el);
    if (second[2] <= 0 || second[3] <= 0 || style.visibility === 'hidden') { return {state: 'not visible'}; }
    if (first.some((value, index) => value !== second[index])) { return {state: 'not stable'}; }
    if (el.disabled || el.getAttribute('aria-disabled') === 'true') { return {state: 'disabled'}; }
    const x = second[0] + second[2] / 2;
    const y = second[1] + second[3] / 2;
    const hit = document.elementFromPoint(x, y);
    if (!hit || !(hit === el || el.contains(hit))) { return {state: 'another element would receive the click'}; }
    return {state: 'ready', x, y};
}";

#[cfg(test)]
mod tests;
