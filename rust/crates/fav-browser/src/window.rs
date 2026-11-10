//! The login window: Xvnc on loopback with a headed Chromium on it.
//!
//! Python origin: the "real browser" and "plain browser" parts of
//! `src/api/browser_login.py` (`_reap_stale_display`, `_wait_for_port`, `_stop_process`,
//! `build_login_launch_options`, `_VncChromium`, `_prepare_profile_and_display`,
//! `_start_xvnc`, `launch_vnc_browser`, `launch_vnc_chromium`,
//! `build_plain_chromium_command`, `cookies_for_urls`, `_PlainChromium`,
//! `_stop_process_group`, `launch_plain_chromium`).
//!
//! Most sites get a plain Chromium nothing drives: no automation switch, no driver
//! attached to its pages; fav only reads its cookie jar at the browser level
//! ([`crate::devtools`]). X refuses sign-ins from a driven window outright. RedNote is the
//! exception (`LoginSite::controlled`): its crawl is itself a driven browser on the same
//! profile, so its window is one too, presented the way the crawl presents it.

use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use fav_core::logging;
use fav_core::pyjson;
use fav_runtime::BoxFuture;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::chromium::{LaunchOptions, playwright_chromium_executable};
use crate::devtools::{DevToolsError, read_devtools_cookies, read_devtools_local_storage};
use crate::driver::{BrowserContext, ContextOptions, Page};
use crate::login::{BrowserLauncher, LaunchRequest, LoginBrowser, LoginError, PreparePage};
use crate::profile::clear_stale_profile_locks;
use crate::proxy::build_proxy_settings;

/// `DEFAULT_DEVTOOLS_PORT`: the plain window's DevTools, on loopback.
pub const DEFAULT_DEVTOOLS_PORT: u16 = 5998;
const SCREEN_WIDTH: u32 = 1280;
const SCREEN_HEIGHT: u32 = 800;
const XVNC_READY_TIMEOUT: Duration = Duration::from_secs(10);
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// `_GOTO_TIMEOUT_MS`: a login page behind a slow proxy is still usable by hand.
const GOTO_TIMEOUT: Duration = Duration::from_mins(1);
const RELOAD_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `read_devtools_local_storage` waits for the background tab.
const LOCAL_STORAGE_TIMEOUT: Duration = Duration::from_secs(10);

fn log() -> logging::Logger {
    logging::get("browser-login")
}

/// Python's `str(OSError)` for a failed spawn: `[Errno 2] No such file or directory: 'Xvnc'`.
fn spawn_error(program: &str, error: &std::io::Error) -> LoginError {
    let class = match error.kind() {
        std::io::ErrorKind::NotFound => "FileNotFoundError",
        std::io::ErrorKind::PermissionDenied => "PermissionError",
        _ => "OSError",
    };
    let message = match error.raw_os_error() {
        Some(errno) => {
            let text = nix::errno::Errno::from_raw(errno).desc();
            format!("[Errno {errno}] {text}: {}", fav_runtime::pystr::repr_str(program))
        }
        None => error.to_string(),
    };
    LoginError::new(class, message)
}

/// `process.returncode`: the exit code, or minus the signal that ended it.
fn return_code(status: std::process::ExitStatus) -> i32 {
    status.code().or_else(|| status.signal().map(|signal| -signal)).unwrap_or(-1)
}

fn exited(child: &mut Child) -> Option<i32> {
    match child.try_wait() {
        Ok(Some(status)) => Some(return_code(status)),
        Ok(None) => None,
        Err(_) => Some(-1),
    }
}

fn pid_of(child: &Child) -> Option<Pid> {
    child.id().and_then(|pid| i32::try_from(pid).ok()).map(Pid::from_raw)
}

/// `_reap_stale_display(display)`: stop an Xvnc an earlier API process left on our display
/// and drop its lock files. The display number is fav's own, so anything still holding it
/// is a leftover login.
pub fn reap_stale_display(display: u32) {
    let lock = PathBuf::from(format!("/tmp/.X{display}-lock"));
    if let Ok(text) = std::fs::read_to_string(&lock)
        && let Ok(pid) = fav_core::pytext::py_strip(&text).parse::<i32>()
        && let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline"))
    {
        let program = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
        if program.ends_with(b"Xvnc") {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }
    }
    for path in [lock, PathBuf::from(format!("/tmp/.X11-unix/X{display}"))] {
        let _ = std::fs::remove_file(path);
    }
}

/// `_wait_for_port(port, process, timeout)`: until Xvnc accepts on its VNC port.
async fn wait_for_port(port: u16, process: &mut Child, timeout: Duration) -> Result<(), LoginError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(code) = exited(process) {
            return Err(LoginError::login(format!("Xvnc exited with code {code} before listening")));
        }
        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => {
                drop(stream);
                return Ok(());
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
    Err(LoginError::login(format!(
        "Xvnc did not listen on port {port} within {}s",
        timeout.as_secs_f64().round()
    )))
}

/// `_stop_process(process)`: SIGTERM, five seconds, then SIGKILL.
async fn stop_process(process: &mut Child) {
    if exited(process).is_some() {
        return;
    }
    if let Some(pid) = pid_of(process) {
        let _ = kill(pid, Signal::SIGTERM);
    }
    if tokio::time::timeout(PROCESS_STOP_TIMEOUT, process.wait()).await.is_err() {
        let _ = process.start_kill();
        let _ = process.wait().await;
    }
}

/// `_stop_process_group(process)`: the same for a whole process group, and a last SIGKILL
/// to the group for helpers that outlive the main process for a moment.
async fn stop_process_group(process: &mut Child) {
    let pid = pid_of(process);
    if exited(process).is_none() {
        if let Some(pid) = pid {
            let _ = killpg(pid, Signal::SIGTERM);
        }
        if tokio::time::timeout(PROCESS_STOP_TIMEOUT, process.wait()).await.is_err() {
            if let Some(pid) = pid {
                let _ = killpg(pid, Signal::SIGKILL);
            }
            let _ = process.wait().await;
        }
    }
    if let Some(pid) = pid {
        let _ = killpg(pid, Signal::SIGKILL);
    }
}

/// The driven window's launch, as data (`build_login_launch_options`): the real Chromium
/// build, headed on the virtual display, no automation banner, no stealth patches, the
/// window filling the screen (with no window manager nothing resizes it).
#[derive(Debug, Clone)]
pub struct LoginLaunch {
    pub launch: LaunchOptions,
    pub context: ContextOptions,
    /// Added to the API process's environment.
    pub env: Vec<(String, String)>,
}

pub fn build_login_launch_options(profile_dir: &Path, proxy: &str, display: u32) -> Result<LoginLaunch, LoginError> {
    let proxy = build_proxy_settings(proxy)?;
    let proxy_credentials = proxy.as_ref().filter(|proxy| proxy.has_credentials()).map(|proxy| {
        (
            proxy.username.clone().unwrap_or_default(),
            proxy.password.clone().unwrap_or_default(),
        )
    });
    Ok(LoginLaunch {
        launch: LaunchOptions {
            user_data_dir: profile_dir.to_path_buf(),
            headless: false,
            proxy,
            args: [
                "--disable-dev-shm-usage".to_owned(),
                "--disable-blink-features=AutomationControlled".to_owned(),
                format!("--window-size={SCREEN_WIDTH},{SCREEN_HEIGHT}"),
                "--window-position=0,0".to_owned(),
                "--no-first-run".to_owned(),
                "--no-default-browser-check".to_owned(),
            ]
            .to_vec(),
            ignore_default_args: vec!["--enable-automation".to_owned()],
        },
        context: ContextOptions {
            locale: Some("zh-CN".to_owned()),
            timezone_id: Some("Asia/Shanghai".to_owned()),
            viewport: None,
            proxy_credentials,
        },
        env: vec![("DISPLAY".to_owned(), format!(":{display}"))],
    })
}

/// `build_plain_chromium_command(...)`: the command line of a login window nothing drives.
/// The only automation surface is a DevTools port on loopback, used at the browser level
/// for the cookie jar and never to attach to a page. Without a driver there is nobody to
/// answer a proxy's credential challenge, so a proxy that needs one is refused.
pub fn build_plain_chromium_command(
    executable: &str,
    profile_dir: &Path,
    proxy: &str,
    devtools_port: u16,
    url: &str,
) -> Result<Vec<String>, LoginError> {
    let mut command = vec![
        executable.to_owned(),
        format!("--user-data-dir={}", profile_dir.display()),
        format!("--remote-debugging-port={devtools_port}"),
        "--remote-debugging-address=127.0.0.1".to_owned(),
        // The pod runs as root, where Chromium refuses to start sandboxed.
        "--no-sandbox".to_owned(),
        "--disable-dev-shm-usage".to_owned(),
        "--no-first-run".to_owned(),
        "--no-default-browser-check".to_owned(),
        // Playwright's choice too, so a profile it wrote before still decrypts.
        "--password-store=basic".to_owned(),
        // Stopping the window is how every login ends; don't offer to restore it.
        "--hide-crash-restore-bubble".to_owned(),
        "--lang=zh-CN".to_owned(),
        format!("--window-size={SCREEN_WIDTH},{SCREEN_HEIGHT}"),
        "--window-position=0,0".to_owned(),
    ];
    match build_proxy_settings(proxy)? {
        Some(settings) if settings.has_credentials() => {
            return Err(LoginError::login(
                "The login window cannot authenticate to a proxy; use one without credentials for this site.",
            ));
        }
        Some(settings) => command.push(format!("--proxy-server={}", settings.server)),
        // Direct means direct, whatever proxy the environment happens to carry.
        None => command.push("--no-proxy-server".to_owned()),
    }
    command.push(url.to_owned());
    Ok(command)
}

/// `str(value or '')` of a JSON value.
fn str_or_empty(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) if crate::rednote::truthy(Some(other)) => fav_runtime::pystr::str_value(&pyjson::Value::from_serde(other)),
        Some(_) => String::new(),
    }
}

/// `cookies_for_urls(cookies, urls)`: the cookies a browser would send to any of `urls`,
/// by domain alone (DevTools returns the whole jar).
pub fn cookies_for_urls(cookies: &[Value], urls: &[String]) -> Vec<Value> {
    let hosts: Vec<String> = urls
        .iter()
        .map(|url| {
            fav_core::pyurl::urlsplit(url, "")
                .ok()
                .and_then(|parts| parts.hostname())
                .unwrap_or_default()
        })
        .collect();
    cookies
        .iter()
        .filter(|cookie| {
            let domain = str_or_empty(cookie.get("domain"));
            let bare = domain.trim_start_matches('.');
            hosts
                .iter()
                .any(|host| host == bare || (domain.starts_with('.') && host.ends_with(&format!(".{bare}"))))
        })
        .cloned()
        .collect()
}

fn to_python(cookies: &[Value]) -> Vec<pyjson::Value> {
    cookies.iter().map(pyjson::Value::from_serde).collect()
}

/// `_prepare_profile_and_display(profile_dir, display)`.
async fn prepare_profile_and_display(profile_dir: &Path, display: u32) -> Result<(), LoginError> {
    tokio::fs::create_dir_all(profile_dir)
        .await
        .map_err(|error| LoginError::new("OSError", error.to_string()))?;
    let cleared = clear_stale_profile_locks(profile_dir).map_err(|error| LoginError::new("OSError", error.to_string()))?;
    if !cleared.is_empty() {
        log().info(format_args!("Cleared stale profile locks {}", cleared.join(", ")));
    }
    reap_stale_display(display);
    Ok(())
}

/// `_start_xvnc(display, vnc_port)`: `TigerVNC`'s Xvnc, listening on loopback only, without
/// authentication (the relay in front of it is gated by the login's ticket).
async fn start_xvnc(display: u32, vnc_port: u16) -> Result<Child, LoginError> {
    let mut xvnc = Command::new("Xvnc")
        .args([
            format!(":{display}"),
            "-geometry".to_owned(),
            format!("{SCREEN_WIDTH}x{SCREEN_HEIGHT}"),
            "-depth".to_owned(),
            "24".to_owned(),
            "-rfbport".to_owned(),
            vnc_port.to_string(),
            "-interface".to_owned(),
            "127.0.0.1".to_owned(),
            "-localhost".to_owned(),
            "-SecurityTypes".to_owned(),
            "None".to_owned(),
            "-AlwaysShared".to_owned(),
            "-nolisten".to_owned(),
            "tcp".to_owned(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| spawn_error("Xvnc", &error))?;
    if let Err(error) = wait_for_port(vnc_port, &mut xvnc, XVNC_READY_TIMEOUT).await {
        stop_process(&mut xvnc).await;
        return Err(error);
    }
    Ok(xvnc)
}

/// The driven window (`_VncChromium`): a [`BrowserContext`] on the virtual display.
pub struct VncChromium {
    xvnc: Mutex<Option<Child>>,
    context: Arc<BrowserContext>,
    prepare_page: Option<PreparePage>,
    page: Mutex<Option<Page>>,
    tab_closer: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for VncChromium {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VncChromium")
            .field("context", &self.context)
            .field("prepare_page", &self.prepare_page.is_some())
            .finish_non_exhaustive()
    }
}

impl VncChromium {
    /// A window over `context`; `xvnc` is the display it runs on (none in tests that run
    /// the browser headless).
    pub fn new(xvnc: Option<Child>, context: BrowserContext, prepare_page: Option<PreparePage>) -> Self {
        Self {
            xvnc: Mutex::new(xvnc),
            context: Arc::new(context),
            prepare_page,
            page: Mutex::new(None),
            tab_closer: Mutex::new(None),
        }
    }

    async fn page(&self) -> Result<Page, LoginError> {
        self.page
            .lock()
            .await
            .clone()
            .ok_or_else(|| LoginError::new("AttributeError", "'NoneType' object has no attribute 'evaluate'"))
    }

    /// The override is per tab, so a second tab (a `target=_blank` link, a popup) would
    /// present the session with the real Linux UA beside the "Mac" one. One tab is all a
    /// login needs; any other is closed as it opens.
    async fn keep_to_one_tab(&self, own: chromiumoxide::cdp::browser_protocol::target::TargetId) -> Result<(), LoginError> {
        let mut created = self.context.page_created().await?;
        let context = Arc::clone(&self.context);
        let closer = tokio::spawn(async move {
            use futures_util::StreamExt as _;
            while let Some(target) = created.next().await {
                if target == own {
                    continue;
                }
                log().info("Closing a second tab the login window opened");
                let _ = context.close_target(target).await;
            }
        });
        if let Some(previous) = self.tab_closer.lock().await.replace(closer) {
            previous.abort();
        }
        Ok(())
    }
}

impl Drop for VncChromium {
    fn drop(&mut self) {
        if let Some(closer) = self.tab_closer.get_mut().take() {
            closer.abort();
        }
    }
}

impl LoginBrowser for VncChromium {
    fn closed(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            if self.context.is_closed().await {
                return true;
            }
            match self.xvnc.lock().await.as_mut() {
                Some(xvnc) => exited(xvnc).is_some(),
                None => false,
            }
        })
    }

    fn open<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            let page = self.context.first_page().await?;
            *self.page.lock().await = Some(page.clone());
            if let Some(prepare_page) = &self.prepare_page {
                prepare_page(&self.context, &page).await?;
                self.keep_to_one_tab(page.target_id().clone()).await?;
            }
            if let Err(error) = page.goto(url, GOTO_TIMEOUT).await {
                // A slow page is still usable by hand.
                log().warning(format_args!("Login page did not finish loading: {}", error.class_name()));
            }
            Ok(())
        })
    }

    fn cookies<'a>(&'a self, urls: &'a [String]) -> BoxFuture<'a, Result<Vec<pyjson::Value>, LoginError>> {
        Box::pin(async move {
            if self.context.is_closed().await {
                return Ok(Vec::new());
            }
            let urls: Vec<&str> = urls.iter().map(String::as_str).collect();
            Ok(to_python(&self.context.cookies(&urls).await?))
        })
    }

    fn evaluate<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<Value, LoginError>> {
        Box::pin(async move { Ok(self.page().await?.evaluate(script).await?) })
    }

    fn reload(&self) -> BoxFuture<'_, Result<(), LoginError>> {
        Box::pin(async move { Ok(self.page().await?.reload(RELOAD_TIMEOUT).await?) })
    }

    fn local_storage<'a>(&'a self, _url: &'a str, _key: &'a str) -> BoxFuture<'a, Result<String, LoginError>> {
        Box::pin(async {
            Err(LoginError::login(
                "Reading localStorage is only implemented for the plain login window",
            ))
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), LoginError>> {
        Box::pin(async move {
            if let Some(closer) = self.tab_closer.lock().await.take() {
                closer.abort();
            }
            if !self.context.is_closed().await {
                self.context.close().await;
            }
            if let Some(xvnc) = self.xvnc.lock().await.as_mut() {
                stop_process(xvnc).await;
            }
            Ok(())
        })
    }
}

/// The plain window (`_PlainChromium`): Chromium started on the display when the login
/// page is opened, read only through its browser-level DevTools.
#[derive(Debug)]
pub struct PlainChromium {
    xvnc: Mutex<Child>,
    executable: String,
    profile_dir: PathBuf,
    proxy: String,
    display: u32,
    devtools_port: u16,
    chromium: Mutex<Option<Child>>,
}

impl PlainChromium {
    async fn is_closed(&self) -> bool {
        if exited(&mut *self.xvnc.lock().await).is_some() {
            return true;
        }
        match self.chromium.lock().await.as_mut() {
            Some(chromium) => exited(chromium).is_some(),
            None => false,
        }
    }
}

impl LoginBrowser for PlainChromium {
    fn closed(&self) -> BoxFuture<'_, bool> {
        Box::pin(self.is_closed())
    }

    fn open<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            let command = build_plain_chromium_command(&self.executable, &self.profile_dir, &self.proxy, self.devtools_port, url)?;
            // Its own process group, so stopping it reaches the renderers and helpers too.
            let child = Command::new(&command[0])
                .args(&command[1..])
                .env("DISPLAY", format!(":{}", self.display))
                .env("TZ", "Asia/Shanghai")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| spawn_error(&self.executable, &error))?;
            *self.chromium.lock().await = Some(child);
            Ok(())
        })
    }

    fn cookies<'a>(&'a self, urls: &'a [String]) -> BoxFuture<'a, Result<Vec<pyjson::Value>, LoginError>> {
        Box::pin(async move {
            if self.is_closed().await {
                return Ok(Vec::new());
            }
            match read_devtools_cookies(self.devtools_port).await {
                Ok(jar) => Ok(to_python(&cookies_for_urls(&jar, urls))),
                // DevTools refusing the call is a real failure; anything else means it is
                // still starting, or between windows, and the next poll asks again.
                Err(error) if error.class == "LoginError" => Err(devtools_failure(error)),
                Err(error) => {
                    log().debug(format_args!("DevTools not answering yet: {}", error.class));
                    Ok(Vec::new())
                }
            }
        })
    }

    fn evaluate<'a>(&'a self, _script: &'a str) -> BoxFuture<'a, Result<Value, LoginError>> {
        Box::pin(async { Err(LoginError::login("A plain login window has no page to evaluate in")) })
    }

    fn reload(&self) -> BoxFuture<'_, Result<(), LoginError>> {
        Box::pin(async { Err(LoginError::login("A plain login window cannot be reloaded from here")) })
    }

    fn local_storage<'a>(&'a self, url: &'a str, key: &'a str) -> BoxFuture<'a, Result<String, LoginError>> {
        Box::pin(async move {
            if self.is_closed().await {
                return Ok(String::new());
            }
            read_devtools_local_storage(self.devtools_port, url, key, LOCAL_STORAGE_TIMEOUT)
                .await
                .map_err(devtools_failure)
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), LoginError>> {
        Box::pin(async move {
            if let Some(chromium) = self.chromium.lock().await.as_mut() {
                stop_process_group(chromium).await;
            }
            stop_process(&mut *self.xvnc.lock().await).await;
            Ok(())
        })
    }
}

fn devtools_failure(error: DevToolsError) -> LoginError {
    LoginError::new(error.class, error.message)
}

/// `launch_vnc_browser(...)` and its two halves: the real login windows.
#[derive(Debug, Clone)]
pub struct VncLauncher {
    /// The Chromium both windows run; Playwright's bundled build by default.
    pub executable: PathBuf,
    pub devtools_port: u16,
}

impl Default for VncLauncher {
    fn default() -> Self {
        Self {
            executable: playwright_chromium_executable(),
            devtools_port: DEFAULT_DEVTOOLS_PORT,
        }
    }
}

impl VncLauncher {
    /// `launch_plain_chromium(...)`.
    async fn launch_plain(&self, request: &LaunchRequest) -> Result<Box<dyn LoginBrowser>, LoginError> {
        let executable = self.executable.display().to_string();
        // Fails on a proxy the window cannot use before anything is started.
        build_plain_chromium_command(&executable, &request.profile_dir, &request.proxy, self.devtools_port, "about:blank")?;
        prepare_profile_and_display(&request.profile_dir, request.display).await?;
        let xvnc = start_xvnc(request.display, request.vnc_port).await?;
        Ok(Box::new(PlainChromium {
            xvnc: Mutex::new(xvnc),
            executable,
            profile_dir: request.profile_dir.clone(),
            proxy: request.proxy.clone(),
            display: request.display,
            devtools_port: self.devtools_port,
            chromium: Mutex::new(None),
        }))
    }

    /// `launch_vnc_chromium(...)`.
    async fn launch_controlled(&self, request: &LaunchRequest) -> Result<Box<dyn LoginBrowser>, LoginError> {
        let options = build_login_launch_options(&request.profile_dir, &request.proxy, request.display)?;
        prepare_profile_and_display(&request.profile_dir, request.display).await?;
        let mut xvnc = start_xvnc(request.display, request.vnc_port).await?;
        match BrowserContext::launch(&self.executable, &options.launch, &options.env, options.context).await {
            Ok(context) => Ok(Box::new(VncChromium::new(Some(xvnc), context, request.prepare_page.clone()))),
            Err(error) => {
                stop_process(&mut xvnc).await;
                Err(error.into())
            }
        }
    }
}

impl BrowserLauncher for VncLauncher {
    fn launch(&self, request: LaunchRequest) -> BoxFuture<'_, Result<Box<dyn LoginBrowser>, LoginError>> {
        Box::pin(async move {
            if request.controlled {
                self.launch_controlled(&request).await
            } else {
                self.launch_plain(&request).await
            }
        })
    }
}

#[cfg(test)]
mod tests;
