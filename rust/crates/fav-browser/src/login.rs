//! One-off browser logins for the 登录态 page: at most one at a time, its cookies polled
//! until the site accepts them.
//!
//! Python origin: `LoginSite`, `LoginBrowser`, `BrowserLauncher`, `LoginAttempt` and
//! `BrowserLoginManager` in `src/api/browser_login.py`. The browser itself (Xvnc and a
//! Chromium on it) is [`crate::window`]; the sites the 登录态 page offers are built by
//! `fav-api` from the sources' verifiers, since those live in `fav-sources`, which
//! depends on this crate. Everything a login touches outside its browser (the
//! `site_sessions` table, the crawl's profile lock) comes in through [`SessionStore`] and
//! [`ProfileLocks`], so the manager is tested without a database, as Python's is.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use chrono::{DateTime, Utc};
use fav_core::logging;
use fav_core::pyjson::Value;
use fav_runtime::BoxFuture;
use fav_runtime::site_sessions::{StoredCookie, cookie_expiry, cookie_value, cookies_from_browser, without_values};
use tokio::sync::watch;

use crate::driver::{BrowserContext, DriverError, Page};

pub const STATE_STARTING: &str = "starting";
pub const STATE_WAITING: &str = "waiting";
pub const STATE_SAVED: &str = "saved";
pub const STATE_FAILED: &str = "failed";
pub const STATE_TIMEOUT: &str = "timeout";
pub const STATE_CANCELLED: &str = "cancelled";

/// `DEFAULT_PROFILE_ROOT`: where a site without a profile of its own keeps one.
pub const DEFAULT_PROFILE_ROOT: &str = "./data/login-profiles";
pub const DEFAULT_DISPLAY: u32 = 99;
pub const DEFAULT_VNC_PORT: u16 = 5999;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_mins(15);
pub const DEFAULT_POLL: Duration = Duration::from_secs(2);
/// A session cookie that stays the same is asked about again after this long: the site
/// may only accept it once the login's last step is done.
const REVERIFY: Duration = Duration::from_secs(15);
/// `_MESSAGE_MAX_CHARS`.
const MESSAGE_MAX_CHARS: usize = 300;

fn log() -> logging::Logger {
    logging::get("browser-login")
}

/// A login step failed. `class` is the Python exception it stands for, because a failed
/// attempt's message is `f'{exc.__class__.__name__}: {exc}'`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct LoginError {
    pub class: String,
    pub message: String,
}

impl LoginError {
    pub fn new(class: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            class: class.into(),
            message: message.into(),
        }
    }

    /// Python's `LoginError(message)`.
    pub fn login(message: impl Into<String>) -> Self {
        Self::new("LoginError", message)
    }

    /// What a failed attempt shows: `ClassName: message`, cut to 300 characters.
    pub fn attempt_message(&self) -> String {
        truncate(&format!("{}: {}", self.class, self.message))
    }
}

impl From<DriverError> for LoginError {
    fn from(error: DriverError) -> Self {
        Self::new(error.class_name(), error.to_string())
    }
}

impl From<crate::proxy::ProxyError> for LoginError {
    fn from(error: crate::proxy::ProxyError) -> Self {
        Self::new(error.class, error.message)
    }
}

fn truncate(text: &str) -> String {
    text.chars().take(MESSAGE_MAX_CHARS).collect()
}

/// Why a login could not start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    /// `UnknownLoginSiteError`: no such site on the page (404).
    #[error("{0}")]
    UnknownSite(String),
    /// `LoginBusyError`: another login, a crawl or the profile's lock is in the way (409).
    #[error("{0}")]
    Busy(String),
    /// Anything else (the database behind the lock, the settings behind the site list).
    #[error(transparent)]
    Failed(#[from] LoginError),
}

/// What a site said about a set of cookies (`LoginCheck`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoginCheck {
    pub ok: bool,
    pub account: String,
    pub message: String,
}

/// What a login needs from a browser. Implemented by [`crate::window`], faked in tests.
pub trait LoginBrowser: Send + Sync {
    /// Whether the window is gone (closed by hand, or its display died).
    fn closed(&self) -> BoxFuture<'_, bool>;
    fn open<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<(), LoginError>>;
    /// The cookies a browser would send to any of `urls`, in Playwright's shape.
    fn cookies<'a>(&'a self, urls: &'a [String]) -> BoxFuture<'a, Result<Vec<Value>, LoginError>>;
    fn evaluate<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<serde_json::Value, LoginError>>;
    fn reload(&self) -> BoxFuture<'_, Result<(), LoginError>>;
    /// One `localStorage` entry of `url`'s origin; `''` when it is not there.
    fn local_storage<'a>(&'a self, url: &'a str, key: &'a str) -> BoxFuture<'a, Result<String, LoginError>>;
    fn close(&self) -> BoxFuture<'_, Result<(), LoginError>>;
}

/// `verify`: asks the site, from the cookies alone, whether they are a real login.
pub type Verify = Arc<dyn Fn(Vec<StoredCookie>) -> BoxFuture<'static, Result<LoginCheck, LoginError>> + Send + Sync>;
/// `verify_in_browser`: asks the login window itself instead.
pub type VerifyInBrowser =
    Arc<dyn for<'a> Fn(&'a dyn LoginBrowser, &'a [StoredCookie]) -> BoxFuture<'a, Result<LoginCheck, LoginError>> + Send + Sync>;
/// A setting read when the login starts (`proxy`, `lock_name`).
pub type SettingFn = Arc<dyn Fn() -> BoxFuture<'static, Result<String, LoginError>> + Send + Sync>;
/// `profile_dir`.
pub type PathFn = Arc<dyn Fn() -> BoxFuture<'static, Result<PathBuf, LoginError>> + Send + Sync>;
/// `prepare_page`: run on the page before the login URL opens, so the window presents the
/// way the crawl does. What it sets lives on the page's own session.
pub type PreparePage = Arc<dyn for<'a> Fn(&'a BrowserContext, &'a Page) -> BoxFuture<'a, Result<(), DriverError>> + Send + Sync>;
/// `after_login`: read from the window once the login verified, stored as `extra`.
pub type AfterLogin = Arc<dyn for<'a> Fn(&'a dyn LoginBrowser) -> BoxFuture<'a, Result<Vec<(String, String)>, LoginError>> + Send + Sync>;
/// `busy_check`: a reason to refuse the login right now, or `''`.
pub type BusyCheck = Arc<dyn Fn() -> BoxFuture<'static, String> + Send + Sync>;

fn no_proxy() -> SettingFn {
    Arc::new(|| Box::pin(async { Ok(String::new()) }))
}

/// One site the 登录态 page can sign in to (`LoginSite`). Build it with [`LoginSite::builder`]
/// and [`LoginSiteBuilder::build`], which refuses a site with no way to verify a login.
#[derive(Clone)]
pub struct LoginSite {
    pub key: String,
    pub label: String,
    pub login_url: String,
    /// URLs whose cookies are read back from the browser.
    pub cookie_urls: Vec<String>,
    /// The cookie whose arrival means a login may have happened.
    pub session_cookie: String,
    pub verify: Option<Verify>,
    pub verify_in_browser: Option<VerifyInBrowser>,
    /// The egress the source crawls through, so the login comes from the same place.
    pub proxy: SettingFn,
    /// The profile the window opens. Unset, the login page keeps one of its own.
    pub profile_dir: Option<PathFn>,
    /// An advisory lock held while the window is open, for a profile a crawl opens too.
    pub lock_name: Option<SettingFn>,
    pub prepare_page: Option<PreparePage>,
    /// One line for the page while the window is open: how this site's login goes.
    pub hint: String,
    /// False for a site whose session lives in its browser profile: `site_sessions` then
    /// records which cookies exist and when they expire, not their values.
    pub store_cookie_values: bool,
    pub after_login: Option<AfterLogin>,
}

impl std::fmt::Debug for LoginSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginSite")
            .field("key", &self.key)
            .field("label", &self.label)
            .field("login_url", &self.login_url)
            .field("cookie_urls", &self.cookie_urls)
            .field("session_cookie", &self.session_cookie)
            .field("controlled", &self.controlled())
            .field("store_cookie_values", &self.store_cookie_values)
            .finish_non_exhaustive()
    }
}

impl LoginSite {
    /// Start a site; [`LoginSiteBuilder::build`] finishes it.
    pub fn builder(key: &str, label: &str, login_url: &str, cookie_urls: &[&str], session_cookie: &str) -> LoginSiteBuilder {
        LoginSiteBuilder(Self {
            key: key.to_owned(),
            label: label.to_owned(),
            login_url: login_url.to_owned(),
            cookie_urls: cookie_urls.iter().map(|url| (*url).to_owned()).collect(),
            session_cookie: session_cookie.to_owned(),
            verify: None,
            verify_in_browser: None,
            proxy: no_proxy(),
            profile_dir: None,
            lock_name: None,
            prepare_page: None,
            hint: String::new(),
            store_cookie_values: true,
            after_login: None,
        })
    }

    /// Whether the window has to be a driven browser: only when the login needs its page.
    pub fn controlled(&self) -> bool {
        self.verify_in_browser.is_some() || self.prepare_page.is_some()
    }
}

/// The optional parts of a [`LoginSite`].
#[derive(Debug)]
pub struct LoginSiteBuilder(LoginSite);

impl LoginSiteBuilder {
    #[must_use]
    pub fn verify(mut self, verify: Verify) -> Self {
        self.0.verify = Some(verify);
        self
    }

    #[must_use]
    pub fn verify_in_browser(mut self, verify: VerifyInBrowser) -> Self {
        self.0.verify_in_browser = Some(verify);
        self
    }

    #[must_use]
    pub fn proxy(mut self, proxy: SettingFn) -> Self {
        self.0.proxy = proxy;
        self
    }

    #[must_use]
    pub fn profile_dir(mut self, profile_dir: PathFn) -> Self {
        self.0.profile_dir = Some(profile_dir);
        self
    }

    #[must_use]
    pub fn lock_name(mut self, lock_name: SettingFn) -> Self {
        self.0.lock_name = Some(lock_name);
        self
    }

    #[must_use]
    pub fn prepare_page(mut self, prepare_page: PreparePage) -> Self {
        self.0.prepare_page = Some(prepare_page);
        self
    }

    #[must_use]
    pub fn hint(mut self, hint: &str) -> Self {
        hint.clone_into(&mut self.0.hint);
        self
    }

    #[must_use]
    pub fn store_cookie_values(mut self, store: bool) -> Self {
        self.0.store_cookie_values = store;
        self
    }

    #[must_use]
    pub fn after_login(mut self, after_login: AfterLogin) -> Self {
        self.0.after_login = Some(after_login);
        self
    }

    /// `__post_init__`: a site needs a way to verify a login (`ValueError` otherwise).
    pub fn build(self) -> Result<LoginSite, LoginError> {
        if self.0.verify.is_none() && self.0.verify_in_browser.is_none() {
            return Err(LoginError::new(
                "ValueError",
                format!("Login site {} needs a way to verify a login", self.0.key),
            ));
        }
        Ok(self.0)
    }
}

/// What the launcher is asked for (the keyword arguments of `launch_vnc_browser`).
#[derive(Clone)]
pub struct LaunchRequest {
    pub profile_dir: PathBuf,
    pub proxy: String,
    pub display: u32,
    pub vnc_port: u16,
    pub prepare_page: Option<PreparePage>,
    pub controlled: bool,
}

impl std::fmt::Debug for LaunchRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchRequest")
            .field("profile_dir", &self.profile_dir)
            // The proxy URL may carry credentials.
            .field("proxy", &if self.proxy.is_empty() { "" } else { "<set>" })
            .field("display", &self.display)
            .field("vnc_port", &self.vnc_port)
            .field("prepare_page", &self.prepare_page.is_some())
            .field("controlled", &self.controlled)
            .finish()
    }
}

/// Starts a login window (`BrowserLauncher`).
pub trait BrowserLauncher: Send + Sync {
    fn launch(&self, request: LaunchRequest) -> BoxFuture<'_, Result<Box<dyn LoginBrowser>, LoginError>>;
}

/// The sites offered right now; asked again on every use, so they follow the settings.
pub trait LoginSites: Send + Sync {
    fn sites(&self) -> BoxFuture<'_, Result<Vec<LoginSite>, LoginError>>;
}

impl LoginSites for Vec<LoginSite> {
    fn sites(&self) -> BoxFuture<'_, Result<Vec<LoginSite>, LoginError>> {
        let sites = self.clone();
        Box::pin(async move { Ok(sites) })
    }
}

/// Where a verified login is stored (`site_sessions.save_signed_in`).
pub trait SessionStore: Send + Sync {
    fn save_signed_in<'a>(
        &'a self,
        site: &'a str,
        cookies: &'a [StoredCookie],
        account: &'a str,
        expires_at: Option<DateTime<Utc>>,
        extra: &'a [(String, String)],
    ) -> BoxFuture<'a, Result<(), LoginError>>;
}

/// A held profile lock, released once the window is down.
pub trait ProfileLock: Send {
    fn release(self: Box<Self>) -> BoxFuture<'static, ()>;
}

/// `database.advisory_lock(name)`: `None` when someone else holds it.
pub trait ProfileLocks: Send + Sync {
    fn try_lock<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Box<dyn ProfileLock>>, LoginError>>;
}

/// The real store and locks: `site_sessions` and PostgreSQL advisory locks.
#[derive(Debug, Clone)]
pub struct DbSessions {
    pub db: fav_db::Db,
}

impl SessionStore for DbSessions {
    fn save_signed_in<'a>(
        &'a self,
        site: &'a str,
        cookies: &'a [StoredCookie],
        account: &'a str,
        expires_at: Option<DateTime<Utc>>,
        extra: &'a [(String, String)],
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            fav_runtime::site_sessions::save_signed_in(&self.db, site, cookies, account, expires_at, extra)
                .await
                .map_err(|error| LoginError::new("DatabaseError", error.to_string()))
        })
    }
}

struct HeldLock(fav_db::lock::AdvisoryLock);

impl ProfileLock for HeldLock {
    fn release(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(self.0.release())
    }
}

impl ProfileLocks for DbSessions {
    fn try_lock<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Box<dyn ProfileLock>>, LoginError>> {
        Box::pin(async move {
            let held = self
                .db
                .try_advisory_lock(name)
                .await
                .map_err(|error| LoginError::new("DatabaseError", error.to_string()))?;
            Ok(held.map(|lock| Box::new(HeldLock(lock)) as Box<dyn ProfileLock>))
        })
    }
}

/// How a login went so far (`LoginAttempt`'s changing fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptProgress {
    pub state: &'static str,
    pub message: String,
    pub account: String,
    pub finished_at: Option<DateTime<Utc>>,
}

/// One login (`LoginAttempt`): the running one, or the last one, so the page can say how
/// it ended. Its `Debug` leaves the ticket out: the ticket opens the VNC socket.
pub struct LoginAttempt {
    pub site: String,
    pub ticket: String,
    pub started_at: DateTime<Utc>,
    pub deadline: DateTime<Utc>,
    progress: Mutex<AttemptProgress>,
    cancel: watch::Sender<bool>,
    finished: watch::Sender<bool>,
}

impl std::fmt::Debug for LoginAttempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginAttempt")
            .field("site", &self.site)
            .field("ticket", &"<redacted>")
            .field("started_at", &self.started_at)
            .field("deadline", &self.deadline)
            .field("progress", &self.progress())
            .finish_non_exhaustive()
    }
}

impl LoginAttempt {
    fn new(site: &str, started_at: DateTime<Utc>, deadline: DateTime<Utc>) -> Self {
        Self {
            site: site.to_owned(),
            ticket: new_ticket(),
            started_at,
            deadline,
            progress: Mutex::new(AttemptProgress {
                state: STATE_STARTING,
                message: String::new(),
                account: String::new(),
                finished_at: None,
            }),
            cancel: watch::Sender::new(false),
            finished: watch::Sender::new(false),
        }
    }

    pub fn progress(&self) -> AttemptProgress {
        self.lock_progress().clone()
    }

    pub fn active(&self) -> bool {
        self.lock_progress().finished_at.is_none()
    }

    fn lock_progress(&self) -> std::sync::MutexGuard<'_, AttemptProgress> {
        self.progress.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn update(&self, change: impl FnOnce(&mut AttemptProgress)) {
        change(&mut self.lock_progress());
    }

    fn cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    async fn wait_cancelled(&self, timeout: Duration) {
        let mut cancel = self.cancel.subscribe();
        let _ = tokio::time::timeout(timeout, cancel.wait_for(|cancelled| *cancelled)).await;
    }

    async fn wait_finished(&self) {
        let mut finished = self.finished.subscribe();
        let _ = finished.wait_for(|finished| *finished).await;
    }
}

/// `secrets.token_urlsafe(32)`.
fn new_ticket() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random source is available");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `secrets.compare_digest`: equal-length inputs are compared in constant time.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter().zip(right).fold(0_u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// The knobs of [`BrowserLoginManager`], Python's keyword arguments and their defaults.
#[derive(Debug, Clone)]
pub struct ManagerOptions {
    pub profile_root: PathBuf,
    pub display: u32,
    pub vnc_port: u16,
    pub timeout: Duration,
    pub poll: Duration,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            profile_root: PathBuf::from(DEFAULT_PROFILE_ROOT),
            display: DEFAULT_DISPLAY,
            vnc_port: DEFAULT_VNC_PORT,
            timeout: DEFAULT_TIMEOUT,
            poll: DEFAULT_POLL,
        }
    }
}

/// What a login task needs, shared with the manager.
struct Services {
    launcher: Arc<dyn BrowserLauncher>,
    store: Arc<dyn SessionStore>,
    options: ManagerOptions,
}

/// At most one login at a time, owned by the API process (`BrowserLoginManager`).
pub struct BrowserLoginManager {
    sites: Arc<dyn LoginSites>,
    locks: Arc<dyn ProfileLocks>,
    busy_check: Option<BusyCheck>,
    services: Arc<Services>,
    starting: tokio::sync::Mutex<()>,
    current: Mutex<Option<Arc<LoginAttempt>>>,
}

impl std::fmt::Debug for BrowserLoginManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserLoginManager")
            .field("options", &self.services.options)
            .field("attempt", &self.attempt())
            .finish_non_exhaustive()
    }
}

impl BrowserLoginManager {
    pub fn new(
        sites: Arc<dyn LoginSites>,
        launcher: Arc<dyn BrowserLauncher>,
        store: Arc<dyn SessionStore>,
        locks: Arc<dyn ProfileLocks>,
        options: ManagerOptions,
    ) -> Self {
        Self {
            sites,
            locks,
            busy_check: None,
            services: Arc::new(Services { launcher, store, options }),
            starting: tokio::sync::Mutex::new(()),
            current: Mutex::new(None),
        }
    }

    #[must_use]
    pub fn with_busy_check(mut self, busy_check: BusyCheck) -> Self {
        self.busy_check = Some(busy_check);
        self
    }

    /// The loopback port the VNC relay connects to.
    pub fn vnc_port(&self) -> u16 {
        self.services.options.vnc_port
    }

    /// `manager.sites`: the sites offered right now, in the page's order.
    pub async fn sites(&self) -> Result<Vec<LoginSite>, LoginError> {
        self.sites.sites().await
    }

    /// The running login, or the last one, so the page can say how it ended.
    pub fn attempt(&self) -> Option<Arc<LoginAttempt>> {
        self.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Whether `ticket` opens the running login's VNC relay.
    pub fn ticket_matches(&self, ticket: &str) -> bool {
        match self.attempt() {
            Some(attempt) if attempt.active() && !ticket.is_empty() => constant_time_eq(ticket.as_bytes(), attempt.ticket.as_bytes()),
            _ => false,
        }
    }

    /// Open a window on `site_key`'s login page; the login runs in the background.
    pub async fn start(&self, site_key: &str) -> Result<Arc<LoginAttempt>, StartError> {
        let sites = self.sites.sites().await?;
        let Some(site) = sites.into_iter().find(|site| site.key == site_key) else {
            return Err(StartError::UnknownSite(format!("Unknown login site: {site_key}")));
        };
        let _starting = self.starting.lock().await;
        if let Some(attempt) = self.attempt().filter(|attempt| attempt.active()) {
            return Err(StartError::Busy(format!(
                "A {} login is already open; finish or cancel it first.",
                attempt.site
            )));
        }
        if let Some(busy_check) = &self.busy_check {
            let reason = busy_check().await;
            if !reason.is_empty() {
                return Err(StartError::Busy(reason));
            }
        }
        let lock = self.hold_profile_lock(&site).await?;
        let now = Utc::now();
        let deadline = now + chrono::Duration::from_std(self.services.options.timeout).unwrap_or(chrono::Duration::MAX);
        let attempt = Arc::new(LoginAttempt::new(&site.key, now, deadline));
        *self.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&attempt));
        log().info(format_args!("Browser login started site={}", site.key));
        tokio::spawn(run(Arc::clone(&self.services), site, Arc::clone(&attempt), lock));
        Ok(attempt)
    }

    /// Close the running login's window without saving anything, and wait until it is down.
    pub async fn cancel(&self) -> Option<Arc<LoginAttempt>> {
        let attempt = self.attempt();
        if let Some(attempt) = attempt.as_ref().filter(|attempt| attempt.active()) {
            attempt.cancel.send_replace(true);
            attempt.wait_finished().await;
        }
        attempt
    }

    /// On shutdown: what [`cancel`](Self::cancel) does.
    pub async fn close(&self) {
        self.cancel().await;
    }

    /// Take the profile's lock for the length of the login, or refuse the login. Two
    /// Chromiums cannot share a profile, and the launcher clears the singleton files a
    /// killed one leaves behind, which is only safe while nothing else can be using it.
    async fn hold_profile_lock(&self, site: &LoginSite) -> Result<Option<Box<dyn ProfileLock>>, StartError> {
        let Some(lock_name) = &site.lock_name else {
            return Ok(None);
        };
        let name = lock_name().await?;
        match self.locks.try_lock(&name).await? {
            Some(lock) => Ok(Some(lock)),
            None => Err(StartError::Busy(format!(
                "The {} browser profile is in use by a crawl right now; try again in a few minutes.",
                site.label
            ))),
        }
    }
}

/// Marks the attempt finished however the task ends, a panic included, so `cancel` never
/// waits forever.
struct FinishOnDrop(Arc<LoginAttempt>);

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        self.0.update(|progress| {
            if progress.finished_at.is_none() {
                progress.finished_at = Some(Utc::now());
            }
        });
        self.0.finished.send_replace(true);
    }
}

/// `_run`: launch, open the login page, wait for the login; then close the window and
/// release the profile lock whatever happened.
async fn run(services: Arc<Services>, site: LoginSite, attempt: Arc<LoginAttempt>, lock: Option<Box<dyn ProfileLock>>) {
    let finish = FinishOnDrop(Arc::clone(&attempt));
    let launched = launch(&services, &site).await;
    let outcome = match launched {
        Err(error) => Err(error),
        Ok(browser) => {
            attempt.update(|progress| progress.state = STATE_WAITING);
            let outcome = match browser.open(&site.login_url).await {
                Ok(()) => await_login(&services, &site, &attempt, browser.as_ref()).await,
                Err(error) => Err(error),
            };
            if let Err(error) = browser.close().await {
                log().error(format_args!("Closing the login browser failed site={}: {error}", site.key));
            }
            outcome
        }
    };
    if let Err(error) = outcome {
        log().error(format_args!("Browser login failed site={}: {}", site.key, error.attempt_message()));
        attempt.update(|progress| {
            progress.state = STATE_FAILED;
            progress.message = error.attempt_message();
        });
    }
    // Only once the browser is down: the lock is what keeps a crawl out of the profile.
    if let Some(lock) = lock {
        lock.release().await;
    }
    attempt.update(|progress| progress.finished_at = Some(Utc::now()));
    log().info(format_args!(
        "Browser login ended site={} state={}",
        site.key,
        attempt.progress().state
    ));
    drop(finish);
}

async fn launch(services: &Services, site: &LoginSite) -> Result<Box<dyn LoginBrowser>, LoginError> {
    let profile_dir = match &site.profile_dir {
        Some(profile_dir) => profile_dir().await?,
        None => services.options.profile_root.join(&site.key),
    };
    let proxy = (site.proxy)().await?;
    services
        .launcher
        .launch(LaunchRequest {
            profile_dir,
            proxy,
            display: services.options.display,
            vnc_port: services.options.vnc_port,
            prepare_page: site.prepare_page.clone(),
            controlled: site.controlled(),
        })
        .await
}

/// `_await_login`: poll the cookies until the session cookie appears and the site accepts
/// it, the window closes, the login is cancelled or its time runs out.
async fn await_login(services: &Services, site: &LoginSite, attempt: &LoginAttempt, browser: &dyn LoginBrowser) -> Result<(), LoginError> {
    let mut last_value = String::new();
    let mut last_checked: Option<Instant> = None;
    loop {
        if attempt.cancelled() {
            attempt.update(|progress| progress.state = STATE_CANCELLED);
            return Ok(());
        }
        if Utc::now() >= attempt.deadline {
            attempt.update(|progress| progress.state = STATE_TIMEOUT);
            return Ok(());
        }
        if browser.closed().await {
            attempt.update(|progress| {
                progress.state = STATE_CANCELLED;
                "The browser window was closed.".clone_into(&mut progress.message);
            });
            return Ok(());
        }

        let cookies = cookies_from_browser(&browser.cookies(&site.cookie_urls).await?);
        let value = cookie_value(&cookies, &site.session_cookie).to_owned();
        let stale = last_checked.is_none_or(|checked| checked.elapsed() >= REVERIFY);
        if !value.is_empty() && (value != last_value || stale) {
            last_value = value;
            last_checked = Some(Instant::now());
            let check = verify(site, browser, &cookies).await?;
            if check.ok {
                let stored = if site.store_cookie_values {
                    cookies.clone()
                } else {
                    without_values(&cookies)
                };
                let extra = after_login(site, browser).await;
                services
                    .store
                    .save_signed_in(
                        &site.key,
                        &stored,
                        &check.account,
                        cookie_expiry(&cookies, &site.session_cookie),
                        &extra,
                    )
                    .await?;
                attempt.update(|progress| {
                    progress.state = STATE_SAVED;
                    progress.account = check.account;
                    progress.message = String::new();
                });
                return Ok(());
            }
            attempt.update(|progress| progress.message = truncate(&check.message));
        }
        attempt.wait_cancelled(services.options.poll).await;
    }
}

async fn verify(site: &LoginSite, browser: &dyn LoginBrowser, cookies: &[StoredCookie]) -> Result<LoginCheck, LoginError> {
    if let Some(verify_in_browser) = &site.verify_in_browser {
        return verify_in_browser(browser, cookies).await;
    }
    match &site.verify {
        Some(verify) => verify(cookies.to_vec()).await,
        None => Err(LoginError::new(
            "ValueError",
            format!("Login site {} needs a way to verify a login", site.key),
        )),
    }
}

/// `_after_login`: a failure costs only what the login could have renewed by itself.
async fn after_login(site: &LoginSite, browser: &dyn LoginBrowser) -> Vec<(String, String)> {
    let Some(after_login) = &site.after_login else {
        return Vec::new();
    };
    match after_login(browser).await {
        Ok(extra) => extra,
        Err(error) => {
            log().error(format_args!(
                "Reading the extras of a login failed site={}: {}",
                site.key,
                error.attempt_message()
            ));
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests;
