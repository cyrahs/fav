//! Zhihu read through a signed-in Chromium profile: the 登录态 login that fills the
//! profile, and the reader that opens one post in it.
//!
//! No Python origin. Zhihu answers the cluster's addresses with HTTP 403 for every page and
//! for its `/api/v4` JSON unless the visitor is signed in, and a plain HTTP client never gets
//! past the script check its pages open with. TikHub, which used to look posts up for fav,
//! failed for days at a time (DIVERGENCES.md D186), so a post is read in the same kind of
//! browser the RedNote crawl uses: Chromium on a profile of its own, signed in once on the
//! 登录态 page, presented the way [`crate::rednote::prepare_page`] presents RedNote's, and
//! leaving through the same proxy.
//!
//! A post's page carries its data in `<script id="js-initialData">`: the store of Zhihu's
//! own front end, whose `initialState.entities` holds the answer, article or pin under its
//! id, keyed in camelCase. [`READ_SCRIPT`] takes that entry out, resolves an author or
//! question given as a reference, and renames the keys to the API's `snake_case`, so what
//! comes back reads like the API's own answer for the post. A page without the entry is
//! asked the API instead, from inside the page, signed in as the page is.

use std::path::{Path, PathBuf};
use std::time::Duration;

use fav_core::logging;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::driver::{BrowserContext, DriverError, Page};
use crate::login::{LoginBrowser, LoginCheck, LoginError};
use crate::profile::{clear_stale_profile_locks, resolve_path};
use crate::proxy::ProxyError;
use crate::rednote::note_browser::{NoteLaunch, build_launch_options};
use crate::rednote::{EXIT_IP_URL, HINTS_RETRY, WEBDRIVER_INIT_SCRIPT, present_mac_user_agent_via, truthy};

/// The `site_sessions` key and 登录态 site.
pub const SESSION_SITE: &str = "zhihu";
pub const WEB_ORIGIN: &str = "https://www.zhihu.com";
pub const LOGIN_URL: &str = "https://www.zhihu.com/signin?next=%2F";
/// The origins whose cookies make up the login: answers and pins live on www, column
/// articles on zhuanlan.
pub const COOKIE_URLS: [&str; 2] = ["https://www.zhihu.com", "https://zhuanlan.zhihu.com"];
/// Set once the sign-in went through.
pub const SESSION_COOKIE: &str = "z_c0";
/// The profile's directory, next to the RedNote profile on the same volume.
pub const PROFILE_DIR_NAME: &str = "zhihu-profile";

fn log() -> logging::Logger {
    logging::get("zhihu")
}

/// The profile next to RedNote's: `<parent of the RedNote profile>/zhihu-profile`.
pub fn profile_dir(rednote_profile: &Path) -> PathBuf {
    rednote_profile
        .parent()
        .map_or_else(|| PathBuf::from(PROFILE_DIR_NAME), |parent| parent.join(PROFILE_DIR_NAME))
}

/// The advisory lock on the profile, held by a read and by a 登录态 login alike: two
/// Chromiums cannot open one profile.
pub fn profile_lock_name(profile: &Path) -> String {
    format!("zhihu-profile:{}", resolve_path(profile).display())
}

/// Asks the signed-in page who it is. `/api/v4/me` answers the page's own cookies; a
/// page on another origin, or a fetch that fails, says nothing either way.
pub const ME_SCRIPT: &str = r"async () => {
    if (location.hostname !== 'www.zhihu.com') { return { status: 0, origin: location.hostname }; }
    try {
        const response = await fetch('/api/v4/me', { credentials: 'include' });
        let body = {};
        try { body = await response.json(); } catch (error) { body = {}; }
        return { status: response.status, name: String(body.name || ''), url_token: String(body.url_token || '') };
    } catch (error) {
        return { status: 0, error: String(error) };
    }
}";

/// `verify` for the 登录态 login: called once `z_c0` is set, which Zhihu does only for a
/// finished sign-in. The page's own `/api/v4/me` names the account; one that refuses the
/// cookies (401, 403) is no login, and one that cannot be asked leaves the cookie to speak.
pub async fn verify_login(browser: &dyn LoginBrowser) -> Result<LoginCheck, LoginError> {
    let me = match browser.evaluate(ME_SCRIPT).await {
        Ok(Value::Object(me)) => me,
        Ok(_) => Map::new(),
        Err(error) => {
            log().info(format_args!("The Zhihu login page could not be asked who it is: {}", error.class));
            Map::new()
        }
    };
    Ok(login_check(&me))
}

/// The decision [`verify_login`] makes from what [`ME_SCRIPT`] returned.
pub fn login_check(me: &Map<String, Value>) -> LoginCheck {
    let status = me.get("status").and_then(Value::as_i64).unwrap_or(0);
    if status == 401 || status == 403 {
        return LoginCheck {
            ok: false,
            account: String::new(),
            message: format!("Zhihu refused the new session (HTTP {status})."),
        };
    }
    let text = |key: &str| me.get(key).and_then(Value::as_str).unwrap_or_default().trim().to_owned();
    let name = text("name");
    let token = text("url_token");
    let account = match (name.is_empty(), token.is_empty()) {
        (false, false) => format!("{name} ({token})"),
        (false, true) => name,
        (true, _) => token,
    };
    LoginCheck {
        ok: true,
        account,
        message: String::new(),
    }
}

/// One look at a post's page, with `{kind, id, api}` as its argument: the post's data
/// when the page has it, and what the page shows otherwise. With `api` set, a page without
/// the post asks the API for it.
pub const READ_SCRIPT: &str = r"async (arg) => {
    const kind = String(arg.kind);
    const id = String(arg.id);
    const snake = (key) => /^[a-z][A-Za-z0-9]*$/.test(key) ? key.replace(/[A-Z]/g, (c) => '_' + c.toLowerCase()) : key;
    const convert = (value) => {
        if (Array.isArray(value)) { return value.map(convert); }
        if (value && typeof value === 'object') {
            const out = {};
            for (const [key, item] of Object.entries(value)) { out[snake(key)] = convert(item); }
            return out;
        }
        return value;
    };
    const body = document.body ? (document.body.innerText || '') : '';
    const result = {
        href: location.href,
        title: document.title || '',
        text: body.slice(0, 300),
        has_initial_data: false,
        post: null,
        source: '',
        api_status: 0,
        api_error: '',
    };
    const node = document.getElementById('js-initialData');
    let entities = null;
    if (node) {
        try {
            const data = JSON.parse(node.textContent || '{}') || {};
            entities = (data.initialState || {}).entities || null;
            result.has_initial_data = true;
        } catch (error) {
            result.api_error = 'initialData: ' + String(error);
        }
    }
    const table = { answer: 'answers', article: 'articles', pin: 'pins' }[kind];
    const found = entities && entities[table] ? entities[table][id] : null;
    if (found && typeof found === 'object') {
        const post = Object.assign({}, found);
        const users = entities.users || {};
        if (typeof post.author === 'string' && users[post.author]) { post.author = users[post.author]; }
        const questions = entities.questions || {};
        if (post.question != null && typeof post.question !== 'object' && questions[post.question]) {
            post.question = questions[post.question];
        } else if (post.question && typeof post.question === 'object' && !post.question.title && questions[post.question.id]) {
            post.question = Object.assign({}, post.question, questions[post.question.id]);
        }
        result.post = convert(post);
        result.source = 'page';
        return result;
    }
    if (!arg.api) { return result; }
    // Asked on the page's own origin, which its cookies go to without CORS: a column
    // article's page is on zhuanlan, whose API has the article at /api/articles/<id>.
    const zhuanlan = location.hostname === 'zhuanlan.zhihu.com';
    const origin = /(^|\.)zhihu\.com$/.test(location.hostname) ? location.origin : 'https://www.zhihu.com';
    const paths = {
        answer: '/api/v4/answers/' + id + '?include=content,question,author,created_time',
        article: (zhuanlan ? '/api/articles/' : '/api/v4/articles/') + id,
        pin: '/api/v4/pins/' + id,
    };
    try {
        const response = await fetch(origin + paths[kind], { credentials: 'include' });
        result.api_status = response.status;
        let data = null;
        try { data = await response.json(); } catch (error) { data = null; }
        if (response.ok && data && typeof data === 'object' && !data.error) {
            result.post = data;
            result.source = 'api';
        } else if (data && data.error) {
            result.api_error = String(data.error.message || JSON.stringify(data.error)).slice(0, 200);
        }
    } catch (error) {
        result.api_error = String(error).slice(0, 200);
    }
    return result;
}";

/// What the page of a post showed.
#[derive(Debug, Clone, PartialEq)]
pub enum PageState {
    /// The post's data, keyed as the API keys it.
    Post(Value),
    /// The page wants a sign-in: the profile's session is gone.
    SignedOut,
    /// Zhihu put a security check in front of the page. The login window may not be shown
    /// it, so this is waited out as an outage rather than waited on as a setting.
    Challenge(String),
    /// The post is not there (deleted, or never was).
    Missing(String),
    /// Still loading, or a page this reader does not know.
    Unknown(String),
}

/// Words Zhihu's pages use for a post that is gone.
const MISSING_MARKERS: [&str; 5] = [
    "你似乎来到了没有知识存在的荒原",
    "该内容已被删除",
    "该回答已被删除",
    "内容不存在",
    "该内容暂时无法查看",
];
/// Words of its security check.
const CHALLENGE_MARKERS: [&str; 3] = ["安全验证", "系统监测到您的网络环境存在异常", "请完成验证"];

fn text<'a>(probe: &'a Map<String, Value>, key: &str) -> &'a str {
    probe.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// What [`READ_SCRIPT`]'s answer says the page is showing.
pub fn classify(probe: &Map<String, Value>) -> PageState {
    let href = text(probe, "href");
    let title = text(probe, "title");
    let body = text(probe, "text");
    if let Some(post) = probe
        .get("post")
        .filter(|post| post.as_object().is_some_and(|post| !post.is_empty()))
    {
        return PageState::Post(post.clone());
    }
    let path = href.split(['?', '#']).next().unwrap_or_default();
    if path.contains("/account/unhuman")
        || CHALLENGE_MARKERS
            .iter()
            .any(|marker| body.contains(marker) || title.contains(marker))
    {
        return PageState::Challenge(href.to_owned());
    }
    if path.ends_with("/signin") || path.contains("/signin/") {
        return PageState::SignedOut;
    }
    let api_status = probe.get("api_status").and_then(Value::as_i64).unwrap_or(0);
    // The API's refusal of a visitor with no session: 401, or a 403 whose message asks
    // for a sign-in (40353 `need_login`).
    if api_status == 401 || (api_status == 403 && text(probe, "api_error").contains("登录")) {
        return PageState::SignedOut;
    }
    let summary = summary(probe);
    if api_status == 404 || MISSING_MARKERS.iter().any(|marker| body.contains(marker) || title.contains(marker)) {
        return PageState::Missing(summary);
    }
    PageState::Unknown(summary)
}

/// The page in one line, for a message: its title, where it ended up, and the API's
/// answer when it was asked.
fn summary(probe: &Map<String, Value>) -> String {
    let mut parts = vec![format!("{:?} at {}", text(probe, "title"), text(probe, "href"))];
    if !truthy(probe.get("has_initial_data")) {
        parts.push("no initialData".to_owned());
    }
    let api_status = probe.get("api_status").and_then(Value::as_i64).unwrap_or(0);
    if api_status != 0 {
        parts.push(format!("API HTTP {api_status}"));
    }
    let api_error = text(probe, "api_error");
    if !api_error.is_empty() {
        parts.push(api_error.to_owned());
    }
    parts.join(", ")
}

/// How long a read waits for each step.
#[derive(Debug, Clone)]
pub struct ReadTimings {
    pub goto: Duration,
    /// How long the page gets to show the post (its script check reloads it once).
    pub settle: Duration,
    pub poll: Duration,
    pub hints_retry: Duration,
}

impl Default for ReadTimings {
    fn default() -> Self {
        Self {
            goto: Duration::from_secs(45),
            settle: Duration::from_secs(25),
            poll: Duration::from_millis(500),
            hints_retry: HINTS_RETRY,
        }
    }
}

/// A read failed before the page could say anything.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error(transparent)]
    Driver(#[from] DriverError),
    /// Clearing the profile's stale locks failed (`OSError`).
    #[error("{class}: {message}")]
    Io { class: &'static str, message: String },
}

/// Chromium on the Zhihu profile, opened for one post and closed again. The caller holds
/// [`profile_lock_name`]'s lock while it runs.
#[derive(Debug, Clone)]
pub struct ZhihuBrowser {
    executable: PathBuf,
    launch: NoteLaunch,
    hints_url: String,
    timings: ReadTimings,
}

impl ZhihuBrowser {
    /// A proxy Chromium cannot use is refused here, before anything starts.
    pub fn new(executable: PathBuf, profile: &Path, proxy: &str, headless: bool) -> Result<Self, ProxyError> {
        Ok(Self {
            executable,
            launch: build_launch_options(profile, proxy, headless)?,
            hints_url: EXIT_IP_URL.to_owned(),
            timings: ReadTimings::default(),
        })
    }

    /// Where the client hints are read (tests park the page on loopback instead).
    #[must_use]
    pub fn with_hints_url(mut self, hints_url: &str) -> Self {
        hints_url.clone_into(&mut self.hints_url);
        self
    }

    #[must_use]
    pub fn with_timings(mut self, timings: ReadTimings) -> Self {
        self.timings = timings;
        self
    }

    /// Open `url` and read the `kind` post `id` from it.
    pub async fn read(&self, url: &str, kind: &str, id: &str) -> Result<PageState, ReadError> {
        let profile = self.launch.launch.user_data_dir.clone();
        let cleared = tokio::task::spawn_blocking(move || clear_stale_profile_locks(&profile))
            .await
            .map_err(|error| ReadError::Io {
                class: "RuntimeError",
                message: error.to_string(),
            })?
            .map_err(|error| ReadError::Io {
                class: error.class,
                message: error.message,
            })?;
        if !cleared.is_empty() {
            log().info(format!("Cleared stale Chromium profile locks: {}", cleared.join(", ")));
        }
        let context = BrowserContext::launch(&self.executable, &self.launch.launch, &[], self.launch.context.clone()).await?;
        let read = self.read_in(&context, url, kind, id).await;
        context.close().await;
        read
    }

    async fn read_in(&self, context: &BrowserContext, url: &str, kind: &str, id: &str) -> Result<PageState, ReadError> {
        context.add_init_script(WEBDRIVER_INIT_SCRIPT).await?;
        let page = context.first_page().await?;
        present_mac_user_agent_via(&page, &self.hints_url, self.timings.hints_retry).await?;
        if let Err(error) = page.goto(url, self.timings.goto).await {
            // A page whose subresources keep it from DOMContentLoaded may still hold the
            // post; anything else is the read failing.
            if !matches!(error, DriverError::Timeout(_)) {
                return Err(error.into());
            }
            log().info(format_args!("Zhihu page {url} did not finish loading: {error}"));
        }
        Ok(self.watch(&page, kind, id).await)
    }

    /// Look at the page until it shows the post or anything else for sure, then once more
    /// asking the API.
    async fn watch(&self, page: &Page, kind: &str, id: &str) -> PageState {
        let deadline = Instant::now() + self.timings.settle;
        loop {
            let state = look(page, kind, id, false).await;
            if !matches!(state, PageState::Unknown(_)) {
                return state;
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(self.timings.poll).await;
        }
        look(page, kind, id, true).await
    }
}

/// One [`READ_SCRIPT`] round trip, classified. One that lands mid-navigation (the script
/// check reloading the page) is a page still loading.
async fn look(page: &Page, kind: &str, id: &str, api: bool) -> PageState {
    match page
        .evaluate_with_arg(READ_SCRIPT, json!({"kind": kind, "id": id, "api": api}))
        .await
    {
        Ok(Value::Object(probe)) => classify(&probe),
        Ok(other) => PageState::Unknown(format!("the page answered {other}")),
        Err(error) => {
            log().debug(format!("Zhihu read landed mid-navigation: {error}"));
            PageState::Unknown(error.to_string())
        }
    }
}

#[cfg(test)]
mod tests;
