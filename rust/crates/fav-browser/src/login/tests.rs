use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use fav_core::pyjson::Value;
use fav_runtime::BoxFuture;
use fav_runtime::site_sessions::StoredCookie;
use serde_json::json;

use super::*;

/// A login window whose cookies the test scripts, one list per poll (the last one repeats).
#[derive(Default)]
struct FakeBrowser {
    cookie_rounds: Mutex<Vec<Vec<Value>>>,
    opened: Mutex<Vec<String>>,
    closed_calls: Mutex<u32>,
    closed: bool,
    storage: HashMap<(String, String), String>,
    storage_reads: Mutex<Vec<(String, String)>>,
}

impl FakeBrowser {
    fn new(cookie_rounds: Vec<Vec<Value>>) -> Self {
        Self {
            cookie_rounds: Mutex::new(cookie_rounds),
            ..Self::default()
        }
    }

    fn closed_calls(&self) -> u32 {
        *self.closed_calls.lock().unwrap()
    }
}

/// The browser the manager drives, shared with the test that inspects it afterwards.
struct Shared(Arc<FakeBrowser>);

impl LoginBrowser for Shared {
    fn closed(&self) -> BoxFuture<'_, bool> {
        Box::pin(async { self.0.closed })
    }

    fn open<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            self.0.opened.lock().unwrap().push(url.to_owned());
            Ok(())
        })
    }

    fn cookies<'a>(&'a self, _urls: &'a [String]) -> BoxFuture<'a, Result<Vec<Value>, LoginError>> {
        Box::pin(async {
            let mut rounds = self.0.cookie_rounds.lock().unwrap();
            Ok(if rounds.len() > 1 { rounds.remove(0) } else { rounds[0].clone() })
        })
    }

    fn evaluate<'a>(&'a self, _script: &'a str) -> BoxFuture<'a, Result<serde_json::Value, LoginError>> {
        Box::pin(async { Ok(json!({})) })
    }

    fn reload(&self) -> BoxFuture<'_, Result<(), LoginError>> {
        Box::pin(async { Ok(()) })
    }

    fn local_storage<'a>(&'a self, url: &'a str, key: &'a str) -> BoxFuture<'a, Result<String, LoginError>> {
        Box::pin(async move {
            self.0.storage_reads.lock().unwrap().push((url.to_owned(), key.to_owned()));
            Ok(self.0.storage.get(&(url.to_owned(), key.to_owned())).cloned().unwrap_or_default())
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), LoginError>> {
        Box::pin(async {
            *self.0.closed_calls.lock().unwrap() += 1;
            Ok(())
        })
    }
}

#[derive(Debug, Clone)]
struct Call {
    profile_dir: PathBuf,
    prepare_page: bool,
    controlled: bool,
}

struct Launcher {
    browser: Option<Arc<FakeBrowser>>,
    error: Option<LoginError>,
    calls: Mutex<Vec<Call>>,
}

impl Launcher {
    fn with(browser: &Arc<FakeBrowser>) -> Arc<Self> {
        Arc::new(Self {
            browser: Some(Arc::clone(browser)),
            error: None,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn failing(error: LoginError) -> Arc<Self> {
        Arc::new(Self {
            browser: None,
            error: Some(error),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl BrowserLauncher for Launcher {
    fn launch(&self, request: LaunchRequest) -> BoxFuture<'_, Result<Box<dyn LoginBrowser>, LoginError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(Call {
                profile_dir: request.profile_dir,
                prepare_page: request.prepare_page.is_some(),
                controlled: request.controlled,
            });
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            Ok(Box::new(Shared(Arc::clone(self.browser.as_ref().expect("a browser")))) as Box<dyn LoginBrowser>)
        })
    }
}

#[derive(Debug, Clone)]
struct Saved {
    site: String,
    cookies: Vec<StoredCookie>,
    account: String,
    expires_at: Option<DateTime<Utc>>,
    extra: Vec<(String, String)>,
}

#[derive(Default)]
struct Store {
    saved: Mutex<Vec<Saved>>,
}

impl Store {
    fn saved(&self) -> Vec<Saved> {
        self.saved.lock().unwrap().clone()
    }
}

impl SessionStore for Store {
    fn save_signed_in<'a>(
        &'a self,
        site: &'a str,
        cookies: &'a [StoredCookie],
        account: &'a str,
        expires_at: Option<DateTime<Utc>>,
        extra: &'a [(String, String)],
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            self.saved.lock().unwrap().push(Saved {
                site: site.to_owned(),
                cookies: cookies.to_vec(),
                account: account.to_owned(),
                expires_at,
                extra: extra.to_vec(),
            });
            Ok(())
        })
    }
}

/// `database.advisory_lock`, in memory: names held, and the ones taken.
#[derive(Default)]
struct Locks {
    held: Arc<Mutex<BTreeSet<String>>>,
    taken: Mutex<Vec<String>>,
}

impl Locks {
    fn holding(name: &str) -> Arc<Self> {
        let locks = Self::default();
        locks.held.lock().unwrap().insert(name.to_owned());
        Arc::new(locks)
    }

    fn held(&self) -> BTreeSet<String> {
        self.held.lock().unwrap().clone()
    }
}

struct Held {
    name: String,
    held: Arc<Mutex<BTreeSet<String>>>,
}

impl ProfileLock for Held {
    fn release(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            self.held.lock().unwrap().remove(&self.name);
        })
    }
}

impl ProfileLocks for Locks {
    fn try_lock<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Box<dyn ProfileLock>>, LoginError>> {
        Box::pin(async move {
            if !self.held.lock().unwrap().insert(name.to_owned()) {
                return Ok(None);
            }
            self.taken.lock().unwrap().push(name.to_owned());
            Ok(Some(Box::new(Held {
                name: name.to_owned(),
                held: Arc::clone(&self.held),
            }) as Box<dyn ProfileLock>))
        })
    }
}

fn cookie(name: &str, value: &str, expires: f64) -> Value {
    Value::from_serde(&json!({"name": name, "value": value, "domain": ".example.com", "path": "/", "expires": expires}))
}

fn default_verify() -> Verify {
    Arc::new(|cookies: Vec<StoredCookie>| {
        Box::pin(async move {
            let value = cookies
                .iter()
                .find(|cookie| cookie.name == "SID")
                .map(|cookie| cookie.value.clone())
                .unwrap_or_default();
            Ok(LoginCheck {
                ok: value.starts_with("user_"),
                account: value.strip_prefix("user_").unwrap_or_default().to_owned(),
                message: if value.is_empty() { "none".to_owned() } else { String::new() },
            })
        })
    })
}

fn site() -> LoginSite {
    LoginSite::builder("demo", "Demo", "https://example.com/login", &["https://example.com/"], "SID")
        .verify(default_verify())
        .build()
        .expect("a valid site")
}

struct Harness {
    manager: BrowserLoginManager,
    store: Arc<Store>,
    locks: Arc<Locks>,
}

fn options() -> ManagerOptions {
    ManagerOptions {
        profile_root: PathBuf::from("/data/profiles"),
        poll: Duration::from_millis(10),
        ..ManagerOptions::default()
    }
}

fn harness_with(sites: Vec<LoginSite>, launcher: Arc<Launcher>, locks: Arc<Locks>, options: ManagerOptions) -> Harness {
    let store = Arc::new(Store::default());
    let manager = BrowserLoginManager::new(
        Arc::new(sites),
        launcher,
        Arc::clone(&store) as Arc<dyn SessionStore>,
        Arc::clone(&locks) as Arc<dyn ProfileLocks>,
        options,
    );
    Harness { manager, store, locks }
}

fn harness(launcher: Arc<Launcher>) -> Harness {
    harness_with(vec![site()], launcher, Arc::default(), options())
}

async fn finish(manager: &BrowserLoginManager, site: &str) -> Arc<LoginAttempt> {
    let attempt = manager.start(site).await.expect("started");
    for _ in 0..500 {
        if !attempt.active() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    attempt
}

// ---------- the login flow ----------

// py: tests/test_browser_login.py::test_a_verified_login_is_saved_and_the_browser_closed
#[tokio::test]
async fn a_verified_login_is_saved_and_the_browser_closed() {
    let browser = Arc::new(FakeBrowser::new(vec![
        vec![],
        vec![cookie("SID", "anon", -1.0)],
        vec![cookie("SID", "user_42", 1_900_000_000.0)],
    ]));
    let launcher = Launcher::with(&browser);
    let harness = harness(Arc::clone(&launcher));

    let attempt = finish(&harness.manager, "demo").await;

    let progress = attempt.progress();
    assert_eq!(progress.state, STATE_SAVED);
    assert_eq!(progress.account, "42");
    assert_eq!(*browser.opened.lock().unwrap(), ["https://example.com/login"]);
    assert_eq!(browser.closed_calls(), 1);
    assert_eq!(launcher.calls()[0].profile_dir, PathBuf::from("/data/profiles/demo"));
    let saved = &harness.store.saved()[0];
    assert_eq!((saved.site.as_str(), saved.account.as_str()), ("demo", "42"));
    assert_eq!(saved.cookies[0].value, "user_42");
    assert_eq!(saved.expires_at, DateTime::from_timestamp(1_900_000_000, 0));
}

// py: tests/test_browser_login.py::test_a_login_that_never_completes_times_out_and_closes
#[tokio::test]
async fn a_login_that_never_completes_times_out_and_closes() {
    let browser = Arc::new(FakeBrowser::new(vec![vec![cookie("SID", "anon", -1.0)]]));
    let harness = harness_with(
        vec![site()],
        Launcher::with(&browser),
        Arc::default(),
        ManagerOptions {
            timeout: Duration::from_millis(50),
            ..options()
        },
    );

    let attempt = finish(&harness.manager, "demo").await;

    let progress = attempt.progress();
    assert_eq!(progress.state, STATE_TIMEOUT);
    assert!(["none", ""].contains(&progress.message.as_str()), "{progress:?}");
    assert_eq!(browser.closed_calls(), 1);
    assert!(harness.store.saved().is_empty());
}

// py: tests/test_browser_login.py::test_closing_the_window_ends_the_login
#[tokio::test]
async fn closing_the_window_ends_the_login() {
    let browser = Arc::new(FakeBrowser {
        closed: true,
        ..FakeBrowser::new(vec![vec![]])
    });

    let attempt = finish(&harness(Launcher::with(&browser)).manager, "demo").await;

    assert_eq!(attempt.progress().state, STATE_CANCELLED);
    assert_eq!(attempt.progress().message, "The browser window was closed.");
    assert_eq!(browser.closed_calls(), 1);
}

// py: tests/test_browser_login.py::test_a_cancelled_login_saves_nothing
#[tokio::test]
async fn a_cancelled_login_saves_nothing() {
    let browser = Arc::new(FakeBrowser::new(vec![vec![]]));
    let harness = harness(Launcher::with(&browser));

    let attempt = harness.manager.start("demo").await.expect("started");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(attempt.progress().state, STATE_WAITING);
    harness.manager.cancel().await;

    assert_eq!(attempt.progress().state, STATE_CANCELLED);
    assert!(!attempt.active());
    assert_eq!(browser.closed_calls(), 1);
    assert!(harness.store.saved().is_empty());
}

// py: tests/test_browser_login.py::test_a_browser_that_cannot_start_reports_the_failure
#[tokio::test]
async fn a_browser_that_cannot_start_reports_the_failure() {
    let harness = harness(Launcher::failing(LoginError::new("RuntimeError", "no display")));

    let attempt = finish(&harness.manager, "demo").await;

    let progress = attempt.progress();
    assert_eq!(progress.state, STATE_FAILED);
    assert_eq!(progress.message, "RuntimeError: no display");
}

#[tokio::test]
async fn a_failure_message_is_cut_to_300_characters() {
    let harness = harness(Launcher::failing(LoginError::new("RuntimeError", "界".repeat(400))));

    let attempt = finish(&harness.manager, "demo").await;

    assert_eq!(attempt.progress().message.chars().count(), 300);
}

// py: tests/test_browser_login.py::test_only_one_login_runs_at_a_time
#[tokio::test]
async fn only_one_login_runs_at_a_time() {
    let harness = harness(Launcher::with(&Arc::new(FakeBrowser::new(vec![vec![]]))));

    harness.manager.start("demo").await.expect("started");
    let busy = harness.manager.start("demo").await.expect_err("busy");
    assert_eq!(
        busy,
        StartError::Busy("A demo login is already open; finish or cancel it first.".to_owned())
    );
    harness.manager.cancel().await;
}

// py: tests/test_browser_login.py::test_a_busy_check_refuses_the_login_before_anything_starts
#[tokio::test]
async fn a_busy_check_refuses_the_login_before_anything_starts() {
    let launcher = Launcher::with(&Arc::new(FakeBrowser::new(vec![vec![]])));
    let harness = harness(Arc::clone(&launcher));
    let manager = harness
        .manager
        .with_busy_check(Arc::new(|| Box::pin(async { "RedNote is crawling".to_owned() })));

    let refused = manager.start("demo").await.expect_err("refused");

    assert!(
        matches!(&refused, StartError::Busy(reason) if reason.contains("RedNote")),
        "{refused:?}"
    );
    assert!(launcher.calls().is_empty());
}

// py: tests/test_browser_login.py::test_an_unknown_site_is_rejected
#[tokio::test]
async fn an_unknown_site_is_rejected() {
    let harness = harness(Launcher::failing(LoginError::login("unused")));

    let refused = harness.manager.start("nope").await.expect_err("unknown");

    assert_eq!(refused, StartError::UnknownSite("Unknown login site: nope".to_owned()));
}

// py: tests/test_browser_login.py::test_the_ticket_only_opens_the_running_login
#[tokio::test]
async fn the_ticket_only_opens_the_running_login() {
    let harness = harness(Launcher::with(&Arc::new(FakeBrowser::new(vec![vec![]]))));

    let attempt = harness.manager.start("demo").await.expect("started");
    // `secrets.token_urlsafe(32)`: 43 URL-safe characters.
    assert_eq!(attempt.ticket.len(), 43);
    assert!(
        attempt
            .ticket
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    );
    assert!(harness.manager.ticket_matches(&attempt.ticket));
    assert!(!harness.manager.ticket_matches("guess"));
    assert!(!harness.manager.ticket_matches(""));
    harness.manager.cancel().await;
    assert!(!harness.manager.ticket_matches(&attempt.ticket));
}

// py: tests/test_browser_login.py::test_a_site_without_a_page_hook_gets_the_plain_window
#[tokio::test]
async fn a_site_without_a_page_hook_gets_the_plain_window() {
    let launcher = Launcher::with(&Arc::new(FakeBrowser::new(vec![vec![cookie("SID", "user_1", -1.0)]])));

    finish(&harness(Arc::clone(&launcher)).manager, "demo").await;

    assert!(!site().controlled());
    assert!(!launcher.calls()[0].controlled);
}

// py: tests/test_browser_login.py::test_what_after_login_reads_is_saved_with_the_session
#[tokio::test]
async fn what_after_login_reads_is_saved_with_the_session() {
    let mut browser = FakeBrowser::new(vec![vec![cookie("SID", "user_42", -1.0)]]);
    browser
        .storage
        .insert(("https://example.com/robots.txt".to_owned(), "tok".to_owned()), "rt-1".to_owned());
    let browser = Arc::new(browser);
    let mut demo = site();
    demo.after_login = Some(Arc::new(|window: &dyn LoginBrowser| {
        Box::pin(async move {
            let token = window.local_storage("https://example.com/robots.txt", "tok").await?;
            Ok(vec![("refresh_token".to_owned(), token)])
        })
    }));
    let harness = harness_with(
        vec![demo],
        Launcher::with(&browser),
        Arc::default(),
        ManagerOptions {
            profile_root: PathBuf::from("/p"),
            ..options()
        },
    );

    let attempt = finish(&harness.manager, "demo").await;

    assert_eq!(attempt.progress().state, STATE_SAVED);
    assert_eq!(harness.store.saved()[0].extra, [("refresh_token".to_owned(), "rt-1".to_owned())]);
}

// py: tests/test_browser_login.py::test_a_failing_after_login_still_saves_the_login
#[tokio::test]
async fn a_failing_after_login_still_saves_the_login() {
    let browser = Arc::new(FakeBrowser::new(vec![vec![cookie("SID", "user_42", -1.0)]]));
    let mut demo = site();
    demo.after_login = Some(Arc::new(|_window: &dyn LoginBrowser| {
        Box::pin(async { Err(LoginError::new("RuntimeError", "devtools gone")) })
    }));
    let harness = harness_with(vec![demo], Launcher::with(&browser), Arc::default(), options());

    let attempt = finish(&harness.manager, "demo").await;

    assert_eq!(attempt.progress().state, STATE_SAVED);
    assert!(harness.store.saved()[0].extra.is_empty());
}

/// A site list that follows a value the test changes.
struct Following(Mutex<Vec<LoginSite>>);

impl LoginSites for Following {
    fn sites(&self) -> BoxFuture<'_, Result<Vec<LoginSite>, LoginError>> {
        Box::pin(async { Ok(self.0.lock().unwrap().clone()) })
    }
}

// py: tests/test_browser_login.py::test_the_manager_follows_a_site_list_that_changes
#[tokio::test]
async fn the_manager_follows_a_site_list_that_changes() {
    let current = Arc::new(Following(Mutex::new(vec![site()])));
    let manager = BrowserLoginManager::new(
        Arc::clone(&current) as Arc<dyn LoginSites>,
        Launcher::with(&Arc::new(FakeBrowser::new(vec![vec![]]))),
        Arc::new(Store::default()),
        Arc::new(Locks::default()),
        ManagerOptions::default(),
    );
    let keys = |sites: Vec<LoginSite>| sites.into_iter().map(|site| site.key).collect::<Vec<_>>();

    assert_eq!(keys(manager.sites().await.unwrap()), ["demo"]);
    let mut other = site();
    other.key = "other".to_owned();
    current.0.lock().unwrap().push(other);
    assert_eq!(keys(manager.sites().await.unwrap()), ["demo", "other"]);
}

// ---------- a site whose crawl is a browser (RedNote) ----------

// py: tests/test_browser_login.py::test_a_site_needs_some_way_to_verify
#[test]
fn a_site_needs_some_way_to_verify() {
    let error = LoginSite::builder("x", "X", "https://x", &[], "S").build().expect_err("refused");
    assert_eq!(error.class, "ValueError");
    assert!(error.message.contains("verify"), "{error}");
}

fn profile_site(verify_in_browser: VerifyInBrowser) -> LoginSite {
    LoginSite::builder("demo", "Demo", "https://example.com/explore", &["https://example.com"], "SID")
        .verify_in_browser(verify_in_browser)
        .profile_dir(Arc::new(|| Box::pin(async { Ok(PathBuf::from("/data/crawl-profile")) })))
        .lock_name(Arc::new(|| Box::pin(async { Ok("demo-profile:/data/crawl-profile".to_owned()) })))
        .prepare_page(Arc::new(|_context, _page| Box::pin(async { Ok(()) })))
        .build()
        .expect("a valid site")
}

fn accept_as(account: &'static str) -> VerifyInBrowser {
    Arc::new(move |_browser, _cookies| {
        Box::pin(async move {
            Ok(LoginCheck {
                ok: true,
                account: account.to_owned(),
                message: String::new(),
            })
        })
    })
}

// py: tests/test_browser_login.py::test_a_profile_site_signs_the_crawl_profile_in_under_its_lock
#[tokio::test]
async fn a_profile_site_signs_the_crawl_profile_in_under_its_lock() {
    let locks = Arc::new(Locks::default());
    let held = Arc::clone(&locks.held);
    let seen_while_open = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&seen_while_open);
    let verify: VerifyInBrowser = Arc::new(move |_browser, _cookies| {
        seen.lock()
            .unwrap()
            .push(held.lock().unwrap().contains("demo-profile:/data/crawl-profile"));
        Box::pin(async {
            Ok(LoginCheck {
                ok: true,
                account: "me".to_owned(),
                message: String::new(),
            })
        })
    });
    let browser = Arc::new(FakeBrowser::new(vec![vec![cookie("SID", "v", 1_900_000_000.0)]]));
    let launcher = Launcher::with(&browser);
    let harness = harness_with(vec![profile_site(verify)], Arc::clone(&launcher), Arc::clone(&locks), options());

    let attempt = finish(&harness.manager, "demo").await;

    let progress = attempt.progress();
    assert_eq!(progress.state, STATE_SAVED);
    assert_eq!(progress.account, "me");
    let call = &launcher.calls()[0];
    assert_eq!(call.profile_dir, PathBuf::from("/data/crawl-profile"));
    assert!(call.prepare_page);
    assert!(call.controlled);
    assert_eq!(*seen_while_open.lock().unwrap(), [true]);
    // Released once the browser is down, so the next crawl can open the profile.
    assert!(harness.locks.held().is_empty());
    assert_eq!(harness.store.saved()[0].account, "me");
}

// py: tests/test_browser_login.py::test_a_profile_site_stores_cookie_names_but_not_values
#[tokio::test]
async fn a_profile_site_stores_cookie_names_but_not_values() {
    let mut demo = profile_site(accept_as("me"));
    demo.store_cookie_values = false;
    let browser = Arc::new(FakeBrowser::new(vec![vec![cookie("SID", "secret", 1_900_000_000.0)]]));
    let harness = harness_with(vec![demo], Launcher::with(&browser), Arc::default(), options());

    finish(&harness.manager, "demo").await;

    let saved = &harness.store.saved()[0];
    let pairs: Vec<(&str, &str)> = saved
        .cookies
        .iter()
        .map(|cookie| (cookie.name.as_str(), cookie.value.as_str()))
        .collect();
    assert_eq!(pairs, [("SID", "in-profile")]);
    assert_eq!(saved.expires_at, DateTime::from_timestamp(1_900_000_000, 0));
    // The RedNote and pixiv sites' own flags are checked where the sites are built (fav-api).
}

// py: tests/test_browser_login.py::test_a_profile_a_crawl_holds_refuses_the_login
#[tokio::test]
async fn a_profile_a_crawl_holds_refuses_the_login() {
    let launcher = Launcher::with(&Arc::new(FakeBrowser::new(vec![vec![]])));
    let harness = harness_with(
        vec![profile_site(accept_as(""))],
        Arc::clone(&launcher),
        Locks::holding("demo-profile:/data/crawl-profile"),
        options(),
    );

    let refused = harness.manager.start("demo").await.expect_err("refused");

    assert!(
        matches!(&refused, StartError::Busy(reason) if reason.contains("in use by a crawl")),
        "{refused:?}"
    );
    assert!(launcher.calls().is_empty());
    assert!(harness.manager.attempt().is_none());
}

// py: tests/test_browser_login.py::test_a_failed_launch_still_releases_the_profile_lock
#[tokio::test]
async fn a_failed_launch_still_releases_the_profile_lock() {
    let harness = harness_with(
        vec![profile_site(accept_as(""))],
        Launcher::failing(LoginError::new("RuntimeError", "no display")),
        Arc::default(),
        options(),
    );

    let attempt = finish(&harness.manager, "demo").await;

    assert_eq!(attempt.progress().state, STATE_FAILED);
    assert_eq!(*harness.locks.taken.lock().unwrap(), ["demo-profile:/data/crawl-profile"]);
    assert!(harness.locks.held().is_empty());
}

#[tokio::test]
async fn a_proxy_the_site_refuses_to_give_fails_the_login_rather_than_going_direct() {
    // `_rednote_proxy` raises inside `_run`, so the attempt fails with its message.
    let mut demo = site();
    demo.proxy = Arc::new(|| Box::pin(async { Err(LoginError::login("set one before signing in.")) }));
    let launcher = Launcher::with(&Arc::new(FakeBrowser::new(vec![vec![]])));
    let harness = harness_with(vec![demo], Arc::clone(&launcher), Arc::default(), options());

    let attempt = finish(&harness.manager, "demo").await;

    assert_eq!(attempt.progress().state, STATE_FAILED);
    assert_eq!(attempt.progress().message, "LoginError: set one before signing in.");
    assert!(launcher.calls().is_empty());
}

#[test]
fn a_launch_request_never_prints_its_proxy() {
    let request = LaunchRequest {
        profile_dir: PathBuf::from("/p"),
        proxy: "http://user:secret@proxy.example:3128".to_owned(),
        display: 99,
        vnc_port: 5999,
        prepare_page: None,
        controlled: false,
    };
    let printed = format!("{request:?}");
    assert!(!printed.contains("secret") && !printed.contains("user"), "{printed}");
}
