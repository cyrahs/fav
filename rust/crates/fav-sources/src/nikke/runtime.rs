//! The runtime layer capture (`src/web/nikke_runtime.py`).
//!
//! `capture_gamekee_runtime_layers` opens a character page in headless Chromium, switches
//! to each stacked skin, waits for its Live2D players to draw, clicks the stage once and
//! reads the players' stacking order and animation calls; [`payload`] turns that into the
//! layers `merge_live2d_layer_captures` takes.
//!
//! The browser is reached through [`CaptureLauncher`] / [`CaptureBrowser`] /
//! [`CapturePage`], the calls Python makes on Playwright, so the flow is tested against a
//! scripted page and, separately, against real Chromium on a local fake page. Production
//! uses [`BrowserCapturer`], Chromium from Playwright's registry started the way
//! `chromium.launch(headless=...)` starts it.

pub mod payload;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fav_browser::chromium::{LaunchOptions, playwright_chromium_executable};
use fav_browser::driver::{BrowserContext, ContextOptions, DriverError, NetworkEvent, Page};
use fav_core::pyjson::{Dict, Value};
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use serde_json::json;

use self::payload::{
    DEFAULT_SKIN_TITLE, PayloadRequest, RuntimeContainerSnapshot, RuntimePlayerSnapshot, capture_payload, containers_from_raw,
    default_source_url, first_skin_model_group, model_skin_title, multi_full_model_groups, players_from_raw, to_float, to_int,
};
pub use self::payload::{RuntimeGroupCapture, live2d_key_from_url, unique_live2d_keys};
use crate::pydecode::PyError;

pub const DEFAULT_RUNTIME_TIMEOUT_MS: i64 = 60_000;
/// `DEFAULT_RUNTIME_VIEWPORT`.
pub const DEFAULT_RUNTIME_VIEWPORT: (u32, u32) = (1440, 1000);
/// `GAMEKEE_RUNTIME_CAPTURE_SCRIPT`, verbatim: patches GameKee's Spine runtime as it loads
/// and records every player and animation call.
pub const GAMEKEE_RUNTIME_CAPTURE_SCRIPT: &str = include_str!("runtime/gamekee_capture.js");
/// The wardrobe button and the timeout of each click on the way to a skin.
pub const CLOTHES_BUTTON_SELECTOR: &str = r#"button.action-item[data-report-key*="clothes"]"#;
const SKIN_CLICK_TIMEOUT_MS: u64 = 10_000;
const SKIN_SETTLE_MS: u64 = 500;
const PLAYER_SETTLE_MS: u64 = 1500;
/// How often `wait_for_function` looks again (Playwright polls on animation frames).
const WAIT_POLL: Duration = Duration::from_millis(100);
pub const CANVAS_COUNT_SCRIPT: &str = r#"count => document.querySelectorAll(".spine-player-container canvas").length >= count"#;
pub const CONTAINERS_SCRIPT: &str = r#"() => [...document.querySelectorAll(".spine-player-container")].map((element, index) => {
            const rect = element.getBoundingClientRect();
            const style = getComputedStyle(element);
            return {
                index,
                zIndex: Number.parseInt(style.zIndex, 10),
                rawZIndex: style.zIndex,
                className: element.className,
                parentClassName: element.parentElement?.className ?? "",
                canvasCount: element.querySelectorAll("canvas").length,
                rect: {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                },
            };
        })"#;
pub const EVENT_COUNT_SCRIPT: &str = "() => window.__gvRuntimeCapture?.eventCount?.() ?? 0";
pub const PLAYERS_SCRIPT: &str = "() => window.__gvRuntimeCapture?.snapshot?.() ?? []";
pub const STAGE_POINT_SCRIPT: &str = r#"() => {
            const element = document.querySelector(".live2d-stage")
                || document.querySelector(".live2d-container")
                || document.querySelector(".spine-player-container");
            if (!element) return null;
            const rect = element.getBoundingClientRect();
            return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
        }"#;

/// `RuntimeCaptureRequest`.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeCaptureRequest {
    pub content_id: i128,
    pub models: Vec<Dict>,
    pub title: String,
    pub timeout_ms: i64,
    pub headless: bool,
    pub source_url: String,
    pub expected_layer_count: Option<i64>,
}

impl RuntimeCaptureRequest {
    /// The defaults of the Python dataclass for the fields callers leave out.
    pub fn new(content_id: i128, models: Vec<Dict>, title: impl Into<String>, timeout_ms: i64) -> Self {
        Self {
            content_id,
            models,
            title: title.into(),
            timeout_ms,
            headless: true,
            source_url: String::new(),
            expected_layer_count: None,
        }
    }

    fn payload_request(&self) -> PayloadRequest<'_> {
        PayloadRequest {
            content_id: self.content_id,
            title: &self.title,
            source_url: &self.source_url,
            models: &self.models,
        }
    }
}

/// `capture_gamekee_runtime_layers`: the capture payload for a page, or the error.
pub trait RuntimeCapturer: Send + Sync {
    fn capture(&self, request: RuntimeCaptureRequest) -> BoxFuture<'_, Result<Dict, PyError>>;
}

/// A Playwright timeout in milliseconds as a duration: `0` is no timeout at all.
pub fn playwright_timeout(timeout_ms: i64) -> Duration {
    match u64::try_from(timeout_ms) {
        Ok(0) | Err(_) => Duration::MAX,
        Ok(ms) => Duration::from_millis(ms),
    }
}

/// The page calls the capture makes (Playwright's `Page`).
pub trait CapturePage: Send + Sync {
    /// `page.add_init_script(script)`.
    fn add_init_script<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<(), DriverError>>;
    /// `page.on('request', ...)`: record every request URL from now on.
    fn record_requests(&self) -> BoxFuture<'_, Result<(), DriverError>>;
    /// The URLs recorded so far, in order.
    fn requested_urls(&self) -> Vec<String>;
    /// `page.goto(url, wait_until='domcontentloaded', timeout=timeout_ms)`.
    fn goto<'a>(&'a self, url: &'a str, timeout_ms: i64) -> BoxFuture<'a, Result<(), DriverError>>;
    /// `page.locator(selector).click(timeout=timeout_ms)`.
    fn click<'a>(&'a self, selector: &'a str, timeout_ms: u64) -> BoxFuture<'a, Result<(), DriverError>>;
    /// `page.get_by_text(text, exact=True).last.click(timeout=timeout_ms)`.
    fn click_last_exact_text<'a>(&'a self, text: &'a str, timeout_ms: u64) -> BoxFuture<'a, Result<(), DriverError>>;
    /// `page.wait_for_function(function, arg=arg, timeout=timeout_ms)`.
    fn wait_for_function<'a>(
        &'a self,
        function: &'a str,
        arg: serde_json::Value,
        timeout_ms: i64,
    ) -> BoxFuture<'a, Result<(), DriverError>>;
    /// `page.wait_for_timeout(ms)`.
    fn wait_for_timeout(&self, ms: u64) -> BoxFuture<'_, ()>;
    /// `page.evaluate(script)`.
    fn evaluate<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<serde_json::Value, DriverError>>;
    /// `page.mouse.click(x, y)`.
    fn mouse_click(&self, x: f64, y: f64) -> BoxFuture<'_, Result<(), DriverError>>;
}

/// `chromium.launch(...)`'s browser.
pub trait CaptureBrowser: Send + Sync {
    /// `browser.new_page(viewport=...)`.
    fn new_page(&self, viewport: (u32, u32)) -> BoxFuture<'_, Result<Box<dyn CapturePage>, DriverError>>;
    /// `browser.close()`.
    fn close(&self) -> BoxFuture<'_, ()>;
}

/// `playwright.chromium.launch(headless=...)`.
pub trait CaptureLauncher: Send + Sync {
    fn launch(&self, headless: bool) -> BoxFuture<'_, Result<Box<dyn CaptureBrowser>, DriverError>>;
}

/// A driver failure as the Python exception Playwright would have raised.
fn driver_error(error: &DriverError) -> PyError {
    PyError::new(error.class_name(), error.to_string())
}

/// `_select_runtime_skin`: open the wardrobe and pick the skin, unless it is the default.
async fn select_runtime_skin(page: &dyn CapturePage, skin_title: &str) -> Result<(), DriverError> {
    if skin_title.is_empty() || skin_title == DEFAULT_SKIN_TITLE {
        return Ok(());
    }
    page.click(CLOTHES_BUTTON_SELECTOR, SKIN_CLICK_TIMEOUT_MS).await?;
    page.click_last_exact_text(skin_title, SKIN_CLICK_TIMEOUT_MS).await?;
    page.wait_for_timeout(SKIN_SETTLE_MS).await;
    Ok(())
}

fn evaluated_list(value: &serde_json::Value) -> Vec<Value> {
    match Value::from_serde(value) {
        Value::List(items) => items,
        _ => Vec::new(),
    }
}

/// `_read_runtime_containers`.
async fn read_runtime_containers(page: &dyn CapturePage) -> Result<Vec<RuntimeContainerSnapshot>, DriverError> {
    Ok(containers_from_raw(&evaluated_list(&page.evaluate(CONTAINERS_SCRIPT).await?)))
}

/// `_runtime_event_count`.
async fn runtime_event_count(page: &dyn CapturePage) -> Result<i128, DriverError> {
    let count = Value::from_serde(&page.evaluate(EVENT_COUNT_SCRIPT).await?);
    Ok(to_int(Some(&count)).filter(|count| *count != 0).unwrap_or(0))
}

/// `_read_runtime_players`.
async fn read_runtime_players(page: &dyn CapturePage) -> Result<Vec<RuntimePlayerSnapshot>, DriverError> {
    Ok(players_from_raw(&evaluated_list(&page.evaluate(PLAYERS_SCRIPT).await?)))
}

/// `_click_runtime_stage`: click the middle of the stage, if there is one.
async fn click_runtime_stage(page: &dyn CapturePage) -> Result<(), DriverError> {
    let Value::Dict(point) = Value::from_serde(&page.evaluate(STAGE_POINT_SCRIPT).await?) else {
        return Ok(());
    };
    let (Some(x), Some(y)) = (to_float(point.get("x")), to_float(point.get("y"))) else {
        return Ok(());
    };
    page.mouse_click(x, y).await
}

/// `_capture_runtime_groups`: one pass over the page per stacked skin (or the first skin).
pub async fn capture_runtime_groups(
    launcher: &dyn CaptureLauncher,
    request: &RuntimeCaptureRequest,
) -> Result<Vec<RuntimeGroupCapture>, PyError> {
    let source_url = if request.source_url.is_empty() {
        default_source_url(request.content_id)
    } else {
        request.source_url.clone()
    };
    let mut groups = multi_full_model_groups(&request.models);
    if groups.is_empty() {
        groups = vec![first_skin_model_group(&request.models)];
    }
    groups.retain(|group| !group.is_empty());

    let browser = launcher.launch(request.headless).await.map_err(|error| driver_error(&error))?;
    let captured = capture_on(&*browser, request, &source_url, &groups).await;
    browser.close().await;
    captured.map_err(|error| driver_error(&error))
}

async fn capture_on(
    browser: &dyn CaptureBrowser,
    request: &RuntimeCaptureRequest,
    source_url: &str,
    groups: &[Vec<Dict>],
) -> Result<Vec<RuntimeGroupCapture>, DriverError> {
    let page = browser.new_page(DEFAULT_RUNTIME_VIEWPORT).await?;
    page.add_init_script(GAMEKEE_RUNTIME_CAPTURE_SCRIPT).await?;
    page.record_requests().await?;
    page.goto(source_url, request.timeout_ms).await?;
    let mut captures = Vec::with_capacity(groups.len());
    for (group_index, group) in groups.iter().enumerate() {
        let skin_title = group.first().map(model_skin_title).unwrap_or_default();
        let is_initial_group = group_index == 0 && (skin_title.is_empty() || skin_title == DEFAULT_SKIN_TITLE);
        let start_request_index = if is_initial_group { 0 } else { page.requested_urls().len() };
        select_runtime_skin(&*page, &skin_title).await?;
        let expected = request
            .expected_layer_count
            .unwrap_or_else(|| i64::try_from(group.len()).unwrap_or(i64::MAX));
        page.wait_for_function(CANVAS_COUNT_SCRIPT, json!(expected), request.timeout_ms)
            .await?;
        page.wait_for_timeout(PLAYER_SETTLE_MS).await;
        let click_start_sequence = runtime_event_count(&*page).await?;
        click_runtime_stage(&*page).await?;
        page.wait_for_timeout(PLAYER_SETTLE_MS).await;
        let containers = read_runtime_containers(&*page).await?;
        let urls = page.requested_urls();
        let requested_live2d_keys = unique_live2d_keys(urls.iter().skip(start_request_index).map(String::as_str));
        let runtime_players = read_runtime_players(&*page).await?;
        captures.push(RuntimeGroupCapture {
            models: group.clone(),
            containers,
            requested_live2d_keys,
            runtime_players,
            runtime_click_start_sequence: Some(click_start_sequence),
        });
    }
    Ok(captures)
}

/// `capture_gamekee_runtime_layers(request)` over `launcher`, `utc_now` standing in for
/// `_utc_now_iso`.
pub async fn capture_gamekee_runtime_layers(
    launcher: &dyn CaptureLauncher,
    request: &RuntimeCaptureRequest,
    utc_now: &(dyn Fn() -> String + Sync),
) -> Result<Dict, PyError> {
    let groups = capture_runtime_groups(launcher, request).await?;
    Ok(capture_payload(&request.payload_request(), &groups, utc_now))
}

// ---------- the real browser ----------

/// [`CaptureLauncher`] over [`fav_browser::driver`]: Chromium at `executable`, started
/// with `chromium.launch(headless=...)`'s command line.
#[derive(Debug, Clone)]
pub struct ChromiumLauncher {
    pub executable: PathBuf,
}

impl CaptureLauncher for ChromiumLauncher {
    fn launch(&self, headless: bool) -> BoxFuture<'_, Result<Box<dyn CaptureBrowser>, DriverError>> {
        Box::pin(async move {
            let options = LaunchOptions {
                user_data_dir: PathBuf::new(),
                headless,
                proxy: None,
                args: Vec::new(),
                ignore_default_args: Vec::new(),
            };
            let context = BrowserContext::launch_browser(&self.executable, &options, &[], ContextOptions::default()).await?;
            Ok(Box::new(ChromiumBrowser { context }) as Box<dyn CaptureBrowser>)
        })
    }
}

struct ChromiumBrowser {
    context: BrowserContext,
}

impl CaptureBrowser for ChromiumBrowser {
    fn new_page(&self, viewport: (u32, u32)) -> BoxFuture<'_, Result<Box<dyn CapturePage>, DriverError>> {
        Box::pin(async move {
            let page = self
                .context
                .new_page_with(&ContextOptions {
                    viewport: Some(viewport),
                    ..ContextOptions::default()
                })
                .await?;
            Ok(Box::new(ChromiumPage {
                page,
                urls: Arc::default(),
                recorder: Mutex::new(None),
            }) as Box<dyn CapturePage>)
        })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.context.close())
    }
}

struct ChromiumPage {
    page: Page,
    urls: Arc<Mutex<Vec<String>>>,
    recorder: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for ChromiumPage {
    fn drop(&mut self) {
        if let Some(recorder) = self.recorder.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
            recorder.abort();
        }
    }
}

impl CapturePage for ChromiumPage {
    fn add_init_script<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<(), DriverError>> {
        Box::pin(self.page.add_init_script(script))
    }

    fn record_requests(&self) -> BoxFuture<'_, Result<(), DriverError>> {
        Box::pin(async move {
            let mut events = self.page.network_events().await?;
            let urls = Arc::clone(&self.urls);
            let recorder = tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    // `page.on('request')` never reports a CORS preflight.
                    if let NetworkEvent::Request { url, resource_type, .. } = event
                        && resource_type != "preflight"
                    {
                        urls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(url);
                    }
                }
            });
            *self.recorder.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(recorder);
            Ok(())
        })
    }

    fn requested_urls(&self) -> Vec<String> {
        self.urls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    fn goto<'a>(&'a self, url: &'a str, timeout_ms: i64) -> BoxFuture<'a, Result<(), DriverError>> {
        Box::pin(self.page.goto(url, playwright_timeout(timeout_ms)))
    }

    fn click<'a>(&'a self, selector: &'a str, timeout_ms: u64) -> BoxFuture<'a, Result<(), DriverError>> {
        Box::pin(self.page.click_strict(selector, Duration::from_millis(timeout_ms)))
    }

    fn click_last_exact_text<'a>(&'a self, text: &'a str, timeout_ms: u64) -> BoxFuture<'a, Result<(), DriverError>> {
        Box::pin(self.page.click_last_with_exact_text(text, Duration::from_millis(timeout_ms)))
    }

    fn wait_for_function<'a>(
        &'a self,
        function: &'a str,
        arg: serde_json::Value,
        timeout_ms: i64,
    ) -> BoxFuture<'a, Result<(), DriverError>> {
        Box::pin(async move {
            self.page
                .wait_for_function_with_arg(function, arg, playwright_timeout(timeout_ms), WAIT_POLL)
                .await
                .map(drop)
        })
    }

    fn wait_for_timeout(&self, ms: u64) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(Duration::from_millis(ms)))
    }

    fn evaluate<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<serde_json::Value, DriverError>> {
        Box::pin(self.page.evaluate(script))
    }

    fn mouse_click(&self, x: f64, y: f64) -> BoxFuture<'_, Result<(), DriverError>> {
        Box::pin(self.page.mouse_click(x, y))
    }
}

/// The production [`RuntimeCapturer`]: [`capture_gamekee_runtime_layers`] in Chromium.
pub struct BrowserCapturer {
    launcher: Box<dyn CaptureLauncher>,
    utc_now: fn() -> String,
}

impl std::fmt::Debug for BrowserCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserCapturer").finish_non_exhaustive()
    }
}

impl BrowserCapturer {
    /// Chromium from Playwright's registry (`playwright.chromium.executable_path`).
    pub fn new() -> Self {
        Self::with_launcher(Box::new(ChromiumLauncher {
            executable: playwright_chromium_executable(),
        }))
    }

    /// Another launcher (a local Chromium in tests).
    pub fn with_launcher(launcher: Box<dyn CaptureLauncher>) -> Self {
        Self {
            launcher,
            utc_now: super::crawl::utc_now_iso,
        }
    }
}

impl Default for BrowserCapturer {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeCapturer for BrowserCapturer {
    fn capture(&self, request: RuntimeCaptureRequest) -> BoxFuture<'_, Result<Dict, PyError>> {
        Box::pin(async move { capture_gamekee_runtime_layers(&*self.launcher, &request, &self.utc_now).await })
    }
}

#[cfg(test)]
mod tests;
