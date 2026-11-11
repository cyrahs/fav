//! The signed-in Chromium profile behind the RedNote crawl.
//!
//! Python origin: `src/web/rednote_browser.py` (`NoteBrowser`, `PlaywrightNoteBrowser`,
//! `build_launch_options`, `summarize_pending`, `_url_head`, `describe_shape`,
//! `redact_probe`, `cursor_of`, `NoteGoneError`). The source (`fav_sources::rednote`)
//! talks to the [`NoteBrowser`] trait and never to the driver, so its tests script a fake.
//!
//! The browser is the point rather than an implementation detail: RedNote reads IP,
//! device fingerprint and behaviour together, so the crawl reads the site's *own* XHR
//! responses as a persistent, signed-in profile scrolls, and never computes a request
//! signature itself.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fav_core::logging;
use fav_core::pyjson::{self, Dict};
use fav_runtime::BoxFuture;
use futures_util::StreamExt as _;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::{WEB_ORIGIN, WEBDRIVER_INIT_SCRIPT, read_login_probe, truthy};
use crate::chromium::LaunchOptions;
use crate::driver::{BrowserContext, ContextOptions, DriverError, NetworkEvent, Page};
use crate::profile::clear_stale_profile_locks;
use crate::proxy::{ProxyError, build_proxy_settings};

/// The likes list's own XHR, whose responses the crawl reads.
pub const LIKE_PAGE_PATH: &str = "/api/sns/web/v1/note/like/page";
/// The liked tab as the profile page labels it (observed as 点赞; 赞过 is what the app
/// says and what this was first written against).
pub const LIKED_TAB_LABELS: [&str; 2] = ["点赞", "赞过"];
/// The captcha wall: the address is not one the site will serve.
pub const RISK_CONTROL_STATUS_CODES: [i64; 2] = [461, 471];
/// Where the site sends a note that no longer exists.
const NOT_FOUND_PATH: &str = "/404";
const SHAPE_MAX_KEYS: usize = 40;
const PENDING_LOG_LIMIT: usize = 8;
/// Attempts per navigation; one more absorbs a lone slow answer (2026-08-24).
pub const GOTO_ATTEMPTS: u32 = 2;
const PROBE_LENGTH_ONLY_FIELDS: [&str; 3] = ["qr_src", "user_id", "profile_href"];
const DEFAULT_VIEWPORT: (u32, u32) = (1280, 800);
const SCROLL_STEPS: u32 = 3;
const SCROLL_PIXELS: f64 = 900.0;
/// Where the pointer is parked before scrolling: the wheel is dispatched wherever the
/// cursor is, and it starts at (0, 0), over nothing.
const GRID_POINTER: (f64, f64) = (640.0, 500.0);
const LIKED_TAB_SELECTOR: &str = ".reds-tab-item";

fn log() -> logging::Logger {
    logging::get("rednote")
}

/// `_NOTE_STATE_SCRIPT`: one note out of `__INITIAL_STATE__`, narrowed inside the page.
pub const NOTE_STATE_SCRIPT: &str = r"(id) => {
    const state = window.__INITIAL_STATE__ || {};
    const map = (state.note && state.note.noteDetailMap) || {};
    const entry = map[id];
    // Deliberately no fallback to whatever note happens to be in the map: this is one
    // long-lived page, so a note that fails to load leaves the previous one there.
    return (entry && entry.note) || null;
}";

/// `_INITIAL_LIKES_SCRIPT`: the liked tab's first screenful, rendered into the page with
/// no request involved, renamed to the XHR envelope's field names.
pub const INITIAL_LIKES_SCRIPT: &str = r"() => {
    const unwrap = (v) => (v && typeof v === 'object' && '_value' in v) ? v._value : v;
    const user = (window.__INITIAL_STATE__ || {}).user || {};
    const buckets = unwrap(user.notes);
    if (!Array.isArray(buckets)) { return []; }
    const tab = unwrap(user.activeTab) || {};
    // The buckets are per tab and mostly empty; prefer the active one and fall back
    // to whichever has anything, rather than to a hardcoded index.
    const active = Array.isArray(buckets[tab.index]) && buckets[tab.index].length ? buckets[tab.index] : null;
    const bucket = active || buckets.find((b) => Array.isArray(b) && b.length) || [];
    return bucket.map((entry) => {
        const card = (entry && entry.noteCard) || {};
        return {
            note_id: String(card.noteId || (entry && entry.id) || ''),
            xsec_token: String((entry && entry.xsecToken) || card.xsecToken || ''),
            display_title: String(card.displayTitle || ''),
            type: String(card.type || ''),
        };
    }).filter((n) => n.note_id);
}";

// ---------- pure helpers ----------

/// `summarize_pending(entries, now=..., limit=...)`: the requests a stalled page is still
/// waiting on, oldest first, as one log line. Each entry is `(resource_type, url,
/// started)` in seconds; only the host and first path segment of a URL are kept.
pub fn summarize_pending(entries: &[(String, String, f64)], now: f64, limit: usize) -> String {
    let mut ordered: Vec<&(String, String, f64)> = entries.iter().collect();
    ordered.sort_by(|a, b| a.2.total_cmp(&b.2));
    if ordered.is_empty() {
        return "nothing".to_owned();
    }
    let mut parts: Vec<String> = ordered
        .iter()
        .take(limit)
        .map(|(resource_type, url, started)| format!("{resource_type} {} {:.1}s", url_head(url), now - started))
        .collect();
    if ordered.len() > limit {
        parts.push(format!("and {} more", ordered.len() - limit));
    }
    parts.join(", ")
}

/// `_url_head(url)`: `host/first-segment`, with nothing after it.
pub fn url_head(url: &str) -> String {
    match fav_core::pyurl::urlsplit(url, "") {
        Ok(parts) if !parts.netloc.is_empty() => {
            let segment = parts.path.trim_start_matches('/').split('/').next().unwrap_or_default();
            format!("{}/{segment}", parts.netloc)
        }
        _ => url.chars().take(40).collect(),
    }
}

/// `describe_shape(value, depth=5)`: key names and types, never values. A list is
/// described by its length and its first item.
pub fn describe_shape(value: &pyjson::Value, depth: u32) -> pyjson::Value {
    use pyjson::Value as V;
    if depth == 0 {
        return V::from("...");
    }
    match value {
        V::Dict(dict) => {
            let mut shape = Dict::new();
            for (key, item) in dict.iter().take(SHAPE_MAX_KEYS) {
                shape.insert(fav_runtime::pystr::str_key(key), describe_shape(item, depth - 1));
            }
            V::Dict(shape)
        }
        V::List(items) => match items.first() {
            Some(first) => V::List(vec![
                V::from(format!("list[{}]", items.len()).as_str()),
                describe_shape(first, depth - 1),
            ]),
            None => V::from("list[0]"),
        },
        V::Bool(_) => V::from("bool"),
        V::Str(text) => V::from(format!("str[{}]", text.chars().count()).as_str()),
        V::Null => V::from("null"),
        V::Int(_) => V::from("int"),
        V::Float(_) => V::from("float"),
    }
}

/// `redact_probe(probe)`: the login probe with the QR image and the account id reduced to
/// their lengths and the modal's HTML left out.
pub fn redact_probe(probe: &Map<String, Value>) -> Map<String, Value> {
    let mut redacted = Map::new();
    for (key, value) in probe {
        if PROBE_LENGTH_ONLY_FIELDS.contains(&key.as_str()) {
            let length = super::str_or_empty(Some(value)).chars().count();
            redacted.insert(key.clone(), json!(format!("str[{length}]")));
        } else if key != "modal_html" {
            redacted.insert(key.clone(), value.clone());
        }
    }
    redacted
}

/// `cursor_of(url)`: the `cursor` a likes-page response was fetched with
/// (`parse_qs(urlsplit(url).query).get('cursor') or ['']`, so an empty value is none).
pub fn cursor_of(url: &str) -> String {
    let query = fav_core::pyurl::urlsplit(url, "").map(|parts| parts.query).unwrap_or_default();
    let decode = |text: &str| fav_core::pyurl::unquote(&text.replace('+', " "));
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(_, value)| !value.is_empty())
        .find(|(name, _)| decode(name) == "cursor")
        .map(|(_, value)| decode(value))
        .unwrap_or_default()
}

/// `build_launch_options(user_data_dir=..., proxy=..., headless=...)`: what the profile is
/// launched with, as data. The real Chromium build (Playwright's `channel='chromium'`,
/// never the headless shell); no user agent pinned, because the browser's own version is
/// only readable once it runs ([`super::present_mac_user_agent`]); zh-CN in Shanghai at
/// a fixed 1280x800; no automation switch; a bigger shared-memory budget than a
/// container's `/dev/shm`.
#[derive(Debug, Clone)]
pub struct NoteLaunch {
    pub launch: LaunchOptions,
    pub context: ContextOptions,
}

pub fn build_launch_options(user_data_dir: &Path, proxy: &str, headless: bool) -> Result<NoteLaunch, ProxyError> {
    let proxy = build_proxy_settings(proxy)?;
    let proxy_credentials = proxy.as_ref().filter(|proxy| proxy.has_credentials()).map(|proxy| {
        (
            proxy.username.clone().unwrap_or_default(),
            proxy.password.clone().unwrap_or_default(),
        )
    });
    Ok(NoteLaunch {
        launch: LaunchOptions {
            user_data_dir: user_data_dir.to_path_buf(),
            headless,
            proxy,
            args: vec![
                "--disable-dev-shm-usage".to_owned(),
                "--disable-blink-features=AutomationControlled".to_owned(),
            ],
            ignore_default_args: vec!["--enable-automation".to_owned()],
        },
        context: ContextOptions {
            locale: Some("zh-CN".to_owned()),
            timezone_id: Some("Asia/Shanghai".to_owned()),
            viewport: Some(DEFAULT_VIEWPORT),
            proxy_credentials,
        },
    })
}

// ---------- the protocol ----------

/// One captured response of the likes list: `{'url', 'status', 'body'}`, the body as
/// `response.json()` read it (`None` when it was not JSON or could not be read).
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedPage {
    pub url: String,
    pub status: i64,
    pub body: Option<pyjson::Value>,
}

/// A browser call failed.
#[derive(Debug, thiserror::Error)]
pub enum NoteBrowserError {
    #[error(transparent)]
    Driver(#[from] DriverError),
    /// Clearing the profile's stale locks failed (`OSError`).
    #[error("{message}")]
    Io { class: &'static str, message: String },
    /// `NoteGoneError`: the site itself said the note does not exist (a redirect to
    /// `/404`), never a timeout or an empty page.
    #[error("{0}")]
    Gone(String),
}

impl NoteBrowserError {
    /// The Python class for messages and job errors.
    pub fn class_name(&self) -> &'static str {
        match self {
            Self::Driver(error) => error.class_name(),
            Self::Io { class, .. } => class,
            Self::Gone(_) => "NoteGoneError",
        }
    }
}

/// What the source needs from a browser (`NoteBrowser`). Implemented for real by
/// [`ChromiumNoteBrowser`], scripted in the source's tests.
pub trait NoteBrowser: Send {
    fn start(&mut self) -> BoxFuture<'_, Result<(), NoteBrowserError>>;
    fn aclose(&mut self) -> BoxFuture<'_, ()>;
    /// The login probe once the page has decided what it shows.
    fn probe_login(&mut self) -> BoxFuture<'_, Result<Map<String, Value>, NoteBrowserError>>;
    /// The site's cookies in Playwright's shape, for the 登录态 page's record.
    fn cookies(&mut self) -> BoxFuture<'_, Result<Vec<Value>, NoteBrowserError>>;
    /// Every cookie as `name -> value`.
    fn cookie_dict(&mut self) -> BoxFuture<'_, Result<HashMap<String, String>, NoteBrowserError>>;
    fn user_agent(&mut self) -> BoxFuture<'_, Result<String, NoteBrowserError>>;
    fn reload_login(&mut self) -> BoxFuture<'_, Result<(), NoteBrowserError>>;
    /// Open the profile's liked tab; `false` if the tab was not found.
    fn open_likes<'a>(&'a mut self, user_id: &'a str) -> BoxFuture<'a, Result<bool, NoteBrowserError>>;
    /// The liked notes the page was served with, before any request.
    fn initial_like_notes(&mut self) -> BoxFuture<'_, Result<Vec<pyjson::Value>, NoteBrowserError>>;
    /// The next likes-list response, scrolling for it; `None` once `timeout` passes.
    fn next_like_page(&mut self, timeout: Duration) -> BoxFuture<'_, Result<Option<CapturedPage>, NoteBrowserError>>;
    /// One note's state; [`NoteBrowserError::Gone`] when the site redirects it to 404,
    /// and an empty dict when the page has no such note.
    fn note_state<'a>(&'a mut self, note_url: &'a str, note_id: &'a str) -> BoxFuture<'a, Result<pyjson::Value, NoteBrowserError>>;
}

// ---------- the real browser ----------

/// The waits of [`ChromiumNoteBrowser`], Python's constants by default; tests shorten them.
#[derive(Debug, Clone)]
pub struct NoteTimings {
    /// Playwright's default navigation timeout.
    pub goto: Duration,
    /// How long a loaded page is given to say whether it is signed in, and how often it
    /// is asked (`DOMContentLoaded` lands ~1.5 s before the login modal).
    pub login_render: Duration,
    pub login_render_poll: Duration,
    pub liked_tab: Duration,
    pub page_poll: Duration,
    pub scroll_pause: Duration,
    pub hints_retry: Duration,
}

impl Default for NoteTimings {
    fn default() -> Self {
        Self {
            goto: Duration::from_secs(30),
            login_render: Duration::from_secs(20),
            login_render_poll: Duration::from_millis(500),
            liked_tab: Duration::from_secs(15),
            page_poll: Duration::from_millis(500),
            scroll_pause: Duration::from_millis(350),
            hints_retry: super::HINTS_RETRY,
        }
    }
}

/// Where the browser goes: the site and the neutral page the client hints are read
/// from. Tests point both at loopback.
#[derive(Debug, Clone)]
pub struct NoteSite {
    pub origin: String,
    pub hints_url: String,
}

impl Default for NoteSite {
    fn default() -> Self {
        Self {
            origin: WEB_ORIGIN.to_owned(),
            hints_url: super::EXIT_IP_URL.to_owned(),
        }
    }
}

/// Request id -> (resource type, url, start), while in flight.
type Inflight = Arc<Mutex<HashMap<String, (String, String, Instant)>>>;

struct Open {
    context: BrowserContext,
    page: Page,
    capture: tokio::task::JoinHandle<()>,
}

/// `PlaywrightNoteBrowser`: a persistent Chromium context, opened for one crawl.
pub struct ChromiumNoteBrowser {
    executable: PathBuf,
    launch: NoteLaunch,
    site: NoteSite,
    timings: NoteTimings,
    open: Option<Open>,
    pages_tx: mpsc::UnboundedSender<CapturedPage>,
    pages_rx: mpsc::UnboundedReceiver<CapturedPage>,
    inflight: Inflight,
    logged_like_shape: Arc<std::sync::atomic::AtomicBool>,
    relaunches: u32,
}

impl std::fmt::Debug for ChromiumNoteBrowser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChromiumNoteBrowser")
            .field("executable", &self.executable)
            .field("launch", &self.launch)
            .field("site", &self.site)
            .field("open", &self.open.is_some())
            .finish_non_exhaustive()
    }
}

impl ChromiumNoteBrowser {
    /// `PlaywrightNoteBrowser(user_data_dir=..., proxy=..., headless=...)`; a proxy
    /// Chromium cannot use is refused here, before anything starts.
    pub fn new(executable: PathBuf, user_data_dir: &Path, proxy: &str, headless: bool) -> Result<Self, ProxyError> {
        let launch = build_launch_options(user_data_dir, proxy, headless)?;
        let (pages_tx, pages_rx) = mpsc::unbounded_channel();
        Ok(Self {
            executable,
            launch,
            site: NoteSite::default(),
            timings: NoteTimings::default(),
            open: None,
            pages_tx,
            pages_rx,
            inflight: Arc::default(),
            logged_like_shape: Arc::default(),
            relaunches: 0,
        })
    }

    /// Point the browser somewhere other than the live site (tests only ever use loopback).
    #[must_use]
    pub fn with_site(mut self, site: NoteSite) -> Self {
        self.site = site;
        self
    }

    #[must_use]
    pub fn with_timings(mut self, timings: NoteTimings) -> Self {
        self.timings = timings;
        self
    }

    /// How often a timed-out navigation relaunched the browser.
    pub fn relaunches(&self) -> u32 {
        self.relaunches
    }

    fn page(&self) -> Result<&Page, NoteBrowserError> {
        self.open
            .as_ref()
            .map(|open| &open.page)
            .ok_or(NoteBrowserError::Driver(DriverError::Closed))
    }

    fn context(&self) -> Result<&BrowserContext, NoteBrowserError> {
        self.open
            .as_ref()
            .map(|open| &open.context)
            .ok_or(NoteBrowserError::Driver(DriverError::Closed))
    }

    async fn open_context(&mut self) -> Result<(), NoteBrowserError> {
        let context = BrowserContext::launch(&self.executable, &self.launch.launch, &[], self.launch.context.clone()).await?;
        let opened = async {
            context.add_init_script(WEBDRIVER_INIT_SCRIPT).await?;
            let page = context.first_page().await?;
            let events = page.network_events().await?;
            Ok::<_, DriverError>((page, events))
        }
        .await;
        let (page, events) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                context.close().await;
                return Err(error.into());
            }
        };
        let capture = tokio::spawn(capture_traffic(
            events,
            page.clone(),
            self.pages_tx.clone(),
            Arc::clone(&self.inflight),
            Arc::clone(&self.logged_like_shape),
        ));
        self.open = Some(Open { context, page, capture });
        let page = self.page()?.clone();
        super::present_mac_user_agent_via(&page, &self.site.hints_url, self.timings.hints_retry).await?;
        Ok(())
    }

    async fn close_context(&mut self) {
        if let Some(open) = self.open.take() {
            open.capture.abort();
            open.context.close().await;
        }
        self.inflight.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
    }

    /// `_relaunch`: close Chromium and open the same profile again, for a fresh socket
    /// pool (a proxy tunnel gone silent stalls a retry on the same browser, 2026-09-29).
    async fn relaunch(&mut self) -> Result<(), NoteBrowserError> {
        self.relaunches += 1;
        self.close_context().await;
        self.open_context().await
    }

    async fn current_url(&self) -> String {
        match self.page() {
            Ok(page) => page.url().await.unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    /// `_on_site`: whether the page is on the site (not just not blank: reading the
    /// client hints parks it on a neutral origin first).
    async fn on_site(&self) -> bool {
        self.current_url().await.starts_with(&self.site.origin)
    }

    /// `_goto`: navigate, giving a timeout one more attempt in a relaunched browser. Only
    /// the timeout is retried; every timeout logs what the page was still waiting on.
    async fn goto(&mut self, url: &str) -> Result<(), NoteBrowserError> {
        for attempt in 1..=GOTO_ATTEMPTS {
            let result = self.page()?.goto(url, self.timings.goto).await;
            match result {
                Err(DriverError::Timeout(ms)) => {
                    let pending = self.pending_summary();
                    let at = url_head(&self.current_url().await);
                    log().warning(format!(
                        "RedNote navigation to {url} timed out (attempt {attempt}) on {at}, still waiting on: {pending}"
                    ));
                    if attempt >= GOTO_ATTEMPTS {
                        return Err(DriverError::Timeout(ms).into());
                    }
                    log().warning("RedNote relaunching the browser before retrying the navigation");
                    self.relaunch().await?;
                }
                Err(error) => return Err(error.into()),
                Ok(()) => return Ok(()),
            }
        }
        Ok(())
    }

    fn pending_summary(&self) -> String {
        let inflight = self.inflight.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = inflight.values().map(|(_, _, started)| *started).min().unwrap_or_else(Instant::now);
        let seconds = |at: Instant| at.saturating_duration_since(base).as_secs_f64();
        let entries: Vec<(String, String, f64)> = inflight
            .values()
            .map(|(kind, url, started)| (kind.clone(), url.clone(), seconds(*started)))
            .collect();
        summarize_pending(&entries, seconds(Instant::now()), PENDING_LOG_LIMIT)
    }

    async fn scroll_once(&self) -> Result<(), NoteBrowserError> {
        let page = self.page()?;
        let (x, y) = GRID_POINTER;
        page.mouse_move(x, y).await?;
        for _ in 0..SCROLL_STEPS {
            page.mouse_wheel(x, y, 0.0, SCROLL_PIXELS).await?;
            tokio::time::sleep(self.timings.scroll_pause).await;
        }
        Ok(())
    }
}

/// The page's traffic, for as long as the context lives: requests in flight (for the
/// timeout log line) and the likes-list responses, read once they have finished loading
/// (`Network.getResponseBody` has nothing before). A response can be reported after its
/// `loadingFinished` (D172), so finished ids are remembered too.
async fn capture_traffic(
    mut events: crate::driver::NetworkEvents,
    page: Page,
    pages: mpsc::UnboundedSender<CapturedPage>,
    inflight: Inflight,
    logged_shape: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut matcher = LikePageMatcher::default();
    // CORS preflights reach the Network domain as requests of their own (resource type
    // `preflight`, no body); Playwright never reports them as page responses.
    let mut preflights: HashSet<String> = HashSet::new();
    while let Some(event) = events.next().await {
        match event {
            NetworkEvent::Request { id, resource_type, url } if resource_type == "preflight" => {
                log().debug(format!("RedNote CORS preflight: {}", url.chars().take(160).collect::<String>()));
                if preflights.len() > LikePageMatcher::FINISHED_LIMIT {
                    preflights.clear();
                }
                preflights.insert(id);
            }
            NetworkEvent::Response { id, .. } | NetworkEvent::Finished { id } | NetworkEvent::Failed { id, .. }
                if preflights.contains(&id) => {}
            NetworkEvent::Request { id, resource_type, url } => {
                inflight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id, (resource_type, url, Instant::now()));
            }
            NetworkEvent::Response { id, url, status } => {
                if !url.contains(LIKE_PAGE_PATH) {
                    log().debug(format!("RedNote XHR: {status} {}", url.chars().take(160).collect::<String>()));
                    continue;
                }
                if let Some((url, status)) = matcher.response(id.clone(), url, status) {
                    let body = read_json_body(&page, &id).await;
                    deliver(&pages, &logged_shape, url, status, body);
                }
            }
            NetworkEvent::Finished { id } => {
                inflight.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
                if let Some((url, status)) = matcher.finished(id.clone()) {
                    let body = read_json_body(&page, &id).await;
                    deliver(&pages, &logged_shape, url, status, body);
                }
            }
            NetworkEvent::Failed { id, .. } => {
                inflight.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
                if let Some((url, status)) = matcher.failed(&id) {
                    deliver(&pages, &logged_shape, url, status, None);
                }
            }
        }
    }
}

/// Pairs a likes-list response with its `loadingFinished`, in either order (D172): each
/// method answers the response's URL and status once its body can be read (or, for a
/// failure, once it never will be).
#[derive(Debug, Default)]
struct LikePageMatcher {
    /// Responses whose body is still loading.
    waiting: HashMap<String, (String, i64)>,
    /// Loads that finished before their response was reported.
    finished: HashSet<String>,
}

impl LikePageMatcher {
    /// Bounds `finished`, which also collects every request that is not a like page.
    const FINISHED_LIMIT: usize = 10_000;

    fn response(&mut self, id: String, url: String, status: i64) -> Option<(String, i64)> {
        if self.finished.remove(&id) {
            Some((url, status))
        } else {
            self.waiting.insert(id, (url, status));
            None
        }
    }

    fn finished(&mut self, id: String) -> Option<(String, i64)> {
        let ready = self.waiting.remove(&id);
        if ready.is_none() {
            if self.finished.len() > Self::FINISHED_LIMIT {
                self.finished.clear();
            }
            self.finished.insert(id);
        }
        ready
    }

    fn failed(&mut self, id: &str) -> Option<(String, i64)> {
        self.waiting.remove(id)
    }
}

/// `await response.json()`, `None` on any failure (logged, without the body).
async fn read_json_body(page: &Page, id: &str) -> Option<pyjson::Value> {
    let read = json_body(page.response_body(id).await);
    match read {
        Ok(value) => Some(value),
        Err(reason) => {
            log().warning(format!("RedNote could not read a likes-list body: {reason}"));
            None
        }
    }
}

/// The JSON of a response body read by [`Page::response_body`], or why there is none.
fn json_body(bytes: Result<Vec<u8>, DriverError>) -> Result<pyjson::Value, String> {
    let bytes = bytes.map_err(|error| format!("Network.getResponseBody failed: {error}"))?;
    let length = bytes.len();
    let text = String::from_utf8(bytes).map_err(|error| format!("{length} bytes, not UTF-8: {error}"))?;
    // `body.decode()` then `json.loads`: strict UTF-8, and a BOM is refused as Python refuses it.
    if text.starts_with('\u{feff}') {
        return Err(format!("{length} bytes starting with a BOM"));
    }
    match pyjson::loads(&text) {
        Ok(value) => Ok(value),
        // Python keeps a lone surrogate in a `str` (a title cut inside an emoji); a Rust
        // string cannot, so it becomes U+FFFD instead of losing the whole page.
        Err(pyjson::LoadError::LoneSurrogate(_)) => pyjson::loads(&replace_lone_surrogates(&text))
            .map_err(|error| format!("{length} bytes, not JSON: {}", error.python_message(&text))),
        Err(error) => Err(format!("{length} bytes, not JSON: {}", error.python_message(&text))),
    }
}

/// `text` with every `\uXXXX` escape of a surrogate that is not half of a pair replaced
/// by `\ufffd`; everything else, other escapes included, is kept as it is.
fn replace_lone_surrogates(text: &str) -> String {
    fn escape_at(text: &str, at: usize) -> Option<u32> {
        let hex = text.get(at..at + 6)?.strip_prefix("\\u")?;
        u32::from_str_radix(hex, 16).ok()
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(offset) = text[at..].find('\\') {
        let start = at + offset;
        out.push_str(&text[at..start]);
        match escape_at(text, start) {
            Some(0xD800..0xDC00) if matches!(escape_at(text, start + 6), Some(0xDC00..0xE000)) => {
                out.push_str(&text[start..start + 12]);
                at = start + 12;
            }
            Some(0xD800..0xE000) => {
                out.push_str("\\ufffd");
                at = start + 6;
            }
            _ => {
                // Any other escape: the backslash and the character it escapes.
                let next = text[start + 1..].chars().next().map_or(0, char::len_utf8);
                out.push_str(&text[start..start + 1 + next]);
                at = start + 1 + next;
            }
        }
    }
    out.push_str(&text[at..]);
    out
}

fn deliver(
    pages: &mpsc::UnboundedSender<CapturedPage>,
    logged_shape: &std::sync::atomic::AtomicBool,
    url: String,
    status: i64,
    body: Option<pyjson::Value>,
) {
    // Once per run: the envelope this source is built around was inferred, not seen.
    if !logged_shape.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let shape = describe_shape(body.as_ref().unwrap_or(&pyjson::Value::Null), 5);
        log().debug(format!(
            "RedNote like/page {status} shape: {}",
            fav_runtime::pystr::repr_value(&shape)
        ));
    }
    let _ = pages.send(CapturedPage { url, status, body });
}

impl NoteBrowser for ChromiumNoteBrowser {
    fn start(&mut self) -> BoxFuture<'_, Result<(), NoteBrowserError>> {
        Box::pin(async move {
            let profile = self.launch.launch.user_data_dir.clone();
            let cleared = tokio::task::spawn_blocking(move || clear_stale_profile_locks(&profile))
                .await
                .map_err(|error| NoteBrowserError::Io {
                    class: "RuntimeError",
                    message: error.to_string(),
                })?
                .map_err(|error| NoteBrowserError::Io {
                    class: error.class,
                    message: error.message,
                })?;
            if !cleared.is_empty() {
                log().info(format!("Cleared stale Chromium profile locks: {}", cleared.join(", ")));
            }
            if let Err(error) = self.open_context().await {
                self.close_context().await;
                return Err(error);
            }
            Ok(())
        })
    }

    fn aclose(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(self.close_context())
    }

    fn probe_login(&mut self) -> BoxFuture<'_, Result<Map<String, Value>, NoteBrowserError>> {
        Box::pin(async move {
            if !self.on_site().await {
                let origin = self.site.origin.clone();
                self.goto(&origin).await?;
            }
            let deadline = Instant::now() + self.timings.login_render;
            loop {
                let probe = read_login_probe(self.page()?).await;
                // Either answer is the page having spoken; anything else is it still loading.
                if truthy(probe.get("logged_in")) || truthy(probe.get("qr_src")) || Instant::now() >= deadline {
                    log().debug(format!(
                        "RedNote login probe: {}",
                        fav_runtime::pystr::repr_value(&pyjson::Value::from_serde(&Value::Object(redact_probe(&probe))))
                    ));
                    return Ok(probe);
                }
                tokio::time::sleep(self.timings.login_render_poll).await;
            }
        })
    }

    fn cookies(&mut self) -> BoxFuture<'_, Result<Vec<Value>, NoteBrowserError>> {
        Box::pin(async move { Ok(self.context()?.cookies(&[self.site.origin.as_str()]).await?) })
    }

    fn cookie_dict(&mut self) -> BoxFuture<'_, Result<HashMap<String, String>, NoteBrowserError>> {
        Box::pin(async move {
            let cookies = self.context()?.cookies(&[]).await?;
            let mut jar = HashMap::new();
            for cookie in &cookies {
                let name = super::str_or_empty(cookie.get("name"));
                if !name.is_empty() {
                    jar.insert(name, super::str_or_empty(cookie.get("value")));
                }
            }
            Ok(jar)
        })
    }

    fn user_agent(&mut self) -> BoxFuture<'_, Result<String, NoteBrowserError>> {
        Box::pin(async move {
            let value = self.page()?.evaluate("() => navigator.userAgent").await?;
            Ok(match value {
                Value::String(text) => text,
                other => fav_runtime::pystr::str_value(&pyjson::Value::from_serde(&other)),
            })
        })
    }

    fn reload_login(&mut self) -> BoxFuture<'_, Result<(), NoteBrowserError>> {
        // A reload, not a fresh goto: navigating to the URL the page is already on while
        // the SPA bootstraps is answered with net::ERR_ABORTED.
        Box::pin(async move {
            if !self.on_site().await {
                let origin = self.site.origin.clone();
                return self.goto(&origin).await;
            }
            Ok(self.page()?.reload(self.timings.goto).await?)
        })
    }

    fn open_likes<'a>(&'a mut self, user_id: &'a str) -> BoxFuture<'a, Result<bool, NoteBrowserError>> {
        Box::pin(async move {
            // `?tab=liked` is the site's own address for the tab; the click is the belt
            // to its braces, and the click is what fires the first like/page request.
            let url = format!("{}/user/profile/{user_id}?tab=liked", self.site.origin);
            self.goto(&url).await?;
            for label in LIKED_TAB_LABELS {
                match self
                    .page()?
                    .click_first_with_text(LIKED_TAB_SELECTOR, label, self.timings.liked_tab)
                    .await
                {
                    Ok(()) => {
                        log().debug(format!("RedNote opened the liked tab via {label}"));
                        return Ok(true);
                    }
                    Err(error) => {
                        let text = error.to_string();
                        log().debug(format!(
                            "RedNote found no liked tab labelled {label}: {}",
                            text.lines().next().unwrap_or_default()
                        ));
                    }
                }
            }
            log().warning(format!(
                "RedNote could not find the liked tab (tried {}) on the profile page",
                LIKED_TAB_LABELS.join(", ")
            ));
            Ok(false)
        })
    }

    fn initial_like_notes(&mut self) -> BoxFuture<'_, Result<Vec<pyjson::Value>, NoteBrowserError>> {
        Box::pin(async move {
            let result = self.page()?.evaluate(INITIAL_LIKES_SCRIPT).await?;
            Ok(match result {
                Value::Array(items) => items
                    .iter()
                    .filter(|item| item.is_object())
                    .map(pyjson::Value::from_serde)
                    .collect(),
                _ => Vec::new(),
            })
        })
    }

    fn next_like_page(&mut self, timeout: Duration) -> BoxFuture<'_, Result<Option<CapturedPage>, NoteBrowserError>> {
        Box::pin(async move {
            let deadline = Instant::now().checked_add(timeout);
            loop {
                if let Ok(Some(page)) = tokio::time::timeout(self.timings.page_poll, self.pages_rx.recv()).await {
                    return Ok(Some(page));
                }
                if deadline.is_some_and(|deadline| Instant::now() > deadline) {
                    return Ok(None);
                }
                self.scroll_once().await?;
            }
        })
    }

    fn note_state<'a>(&'a mut self, note_url: &'a str, note_id: &'a str) -> BoxFuture<'a, Result<pyjson::Value, NoteBrowserError>> {
        Box::pin(async move {
            let page = self.page()?;
            page.goto(note_url, self.timings.goto).await?;
            // A note that is gone redirects to /404 rather than rendering an empty one: the
            // only positive evidence of deletion, so the only thing reported as such.
            let landed = page.url().await?;
            let path = fav_core::pyurl::urlsplit(&landed, "").map(|parts| parts.path).unwrap_or_default();
            if path.contains(NOT_FOUND_PATH) {
                return Err(NoteBrowserError::Gone(format!("RedNote redirected note {note_id} to 404")));
            }
            let note = page.evaluate_with_arg(NOTE_STATE_SCRIPT, json!(note_id)).await?;
            Ok(match note {
                Value::Object(_) => pyjson::Value::from_serde(&note),
                _ => pyjson::Value::Dict(Dict::new()),
            })
        })
    }
}

#[cfg(test)]
mod tests;
