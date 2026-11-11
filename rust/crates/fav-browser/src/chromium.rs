//! Starting Chromium the way Playwright does, without Playwright.
//!
//! Python origin: what `playwright.chromium.launch_persistent_context(...)` (Playwright
//! 1.61.0, the version in `uv.lock`) does before it talks to the browser: find the
//! bundled Chromium build in the browser registry, put together the command line
//! (`chromiumSwitches`, `_innerDefaultArgs`, `defaultArgs` in `playwright-core`), and
//! start it in a process group of its own. `rust/goldens/browser_login/playwright_argv.json`
//! holds the command lines Python's Playwright builds for fav's launch options, and the
//! tests compare them argument by argument.
//!
//! One argument differs on purpose (`DIVERGENCES.md` D160): Playwright talks to the
//! browser over `--remote-debugging-pipe` (file descriptors 3 and 4), which a child can
//! only be given with `unsafe` code here; fav asks for `--remote-debugging-port=0`
//! instead, which binds DevTools to a free loopback port and prints its address.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::AsyncBufReadExt as _;

use crate::proxy::ProxySettings;

/// The Playwright release fav's Chromium build comes from (`uv.lock`).
pub const PLAYWRIGHT_VERSION: &str = "1.61.0";
/// The `chromium` revision of that release (`browsers.json` in its driver package).
pub const CHROMIUM_REVISION: &str = "1228";

/// What replaces Playwright's `--remote-debugging-pipe` (D160).
pub const REMOTE_DEBUGGING_ARG: &str = "--remote-debugging-port=0";

/// The features Playwright 1.61 disables (`disabledFeatures` in `chromiumSwitches.ts`).
const DISABLED_FEATURES: [&str; 16] = [
    "AvoidUnnecessaryBeforeUnloadCheckSync",
    "BoundaryEventDispatchTracksNodeRemoval",
    "DestroyProfileOnBrowserClose",
    "DialMediaRouteProvider",
    "GlobalMediaControls",
    "HttpsUpgrades",
    "LensOverlay",
    "MediaRouter",
    "PaintHolding",
    "ThirdPartyStoragePartitioning",
    "Translate",
    "AutoDeElevate",
    "RenderDocument",
    "OptimizationHints",
    "msForceBrowserSignIn",
    "msEdgeUpdateLaunchServicesPreferredVersion",
];

/// `chromiumSwitches()`: the switches every Playwright Chromium starts with, in order.
pub fn chromium_switches() -> Vec<String> {
    let mut switches: Vec<String> = [
        "--disable-field-trial-config",
        "--disable-background-networking",
        "--disable-background-timer-throttling",
        "--disable-backgrounding-occluded-windows",
        "--disable-back-forward-cache",
        "--disable-breakpad",
        "--disable-client-side-phishing-detection",
        "--disable-component-extensions-with-background-pages",
        "--disable-component-update",
        "--no-default-browser-check",
        "--disable-default-apps",
        "--disable-dev-shm-usage",
        "--disable-edgeupdater",
        "--disable-extensions",
    ]
    .map(str::to_owned)
    .to_vec();
    switches.push(format!("--disable-features={}", DISABLED_FEATURES.join(",")));
    // Playwright leaves this out only under PLAYWRIGHT_LEGACY_SCREENSHOT, which fav never sets.
    switches.push("--enable-features=CDPScreenshotNewSurface".to_owned());
    switches.extend(
        [
            "--allow-pre-commit-input",
            "--disable-hang-monitor",
            "--disable-ipc-flooding-protection",
            "--disable-popup-blocking",
            "--disable-prompt-on-repost",
            "--disable-renderer-backgrounding",
            "--force-color-profile=srgb",
            "--metrics-recording-only",
            "--no-first-run",
            "--password-store=basic",
            "--use-mock-keychain",
            "--no-service-autorun",
            "--export-tagged-pdf",
            "--disable-search-engine-choice-screen",
            "--unsafely-disable-devtools-self-xss-warnings",
            "--edge-skip-compat-layer-relaunch",
            "--disable-infobars",
            "--disable-search-engine-choice-screen",
            "--disable-sync",
        ]
        .map(str::to_owned),
    );
    switches
}

/// What fav passes to `launch_persistent_context`, as far as the command line goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOptions {
    pub user_data_dir: PathBuf,
    pub headless: bool,
    /// `proxy=build_proxy_settings(...)`; the credentials are answered over CDP.
    pub proxy: Option<ProxySettings>,
    /// `args=[...]`.
    pub args: Vec<String>,
    /// `ignore_default_args=[...]`.
    pub ignore_default_args: Vec<String>,
}

/// `normalizeProxySettings`'s server: `protocol//host`, the way WHATWG `URL` writes it
/// (a scheme's default port dropped). `build_proxy_settings` has already lowercased the
/// host and given every server a scheme.
fn normalized_proxy_server(server: &str) -> String {
    let Some((scheme, rest)) = server.split_once("://") else {
        return server.to_owned();
    };
    let default_port = match scheme {
        "http" | "ws" => Some("80"),
        "https" | "wss" => Some("443"),
        "ftp" => Some("21"),
        _ => None,
    };
    match (default_port, rest.rsplit_once(':')) {
        (Some(default), Some((host, port))) if port == default => format!("{scheme}://{host}"),
        _ => server.to_owned(),
    }
}

/// `new URL(server).hostname`: the host without its port (IPv6 keeps its brackets).
fn proxy_hostname(server: &str) -> &str {
    let rest = server.split_once("://").map_or(server, |(_, rest)| rest);
    match rest.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) => host,
        _ => rest,
    }
}

/// `BrowserType._user_data_dir`: a relative profile path is resolved against `cwd`
/// (`Path(user_data_dir).resolve()`, no `~` expansion); an absolute one is only
/// normalised (`str(Path(p))`).
fn user_data_dir(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.components().collect();
    }
    crate::profile::resolve_path(&cwd.join(path))
}

/// `_innerDefaultArgs`: the switches, the headless set, the sandbox, the proxy and the
/// caller's `args`, before the profile and the DevTools transport.
fn inner_default_args(options: &LaunchOptions) -> Vec<String> {
    let mut argv = chromium_switches();
    argv.push("--enable-unsafe-swiftshader".to_owned());
    if options.headless {
        argv.extend(
            [
                "--headless",
                "--hide-scrollbars",
                "--mute-audio",
                "--blink-settings=primaryHoverType=2,availableHoverTypes=2,primaryPointerType=4,availablePointerTypes=4",
            ]
            .map(str::to_owned),
        );
    }
    // `chromium_sandbox` defaults to false.
    argv.push("--no-sandbox".to_owned());
    if let Some(proxy) = &options.proxy {
        let server = normalized_proxy_server(&proxy.server);
        if server.starts_with("socks5:") {
            let host = proxy_hostname(&server);
            argv.push(format!("--host-resolver-rules=\"MAP * ~NOTFOUND , EXCLUDE {host}\""));
        }
        argv.push(format!("--proxy-server={server}"));
        // No bypass list is ever given, so loopback is forced through the proxy too.
        argv.push("--proxy-bypass-list=<-loopback>".to_owned());
    }
    argv.extend(options.args.iter().cloned());
    argv
}

/// The command line `launch_persistent_context` gives Chromium, `--remote-debugging-pipe`
/// replaced by [`REMOTE_DEBUGGING_ARG`] (D160). The executable is not part of it.
pub fn persistent_context_argv(options: &LaunchOptions) -> Vec<String> {
    let mut argv = inner_default_args(options);
    let cwd = std::env::current_dir().unwrap_or_default();
    argv.push(format!("--user-data-dir={}", user_data_dir(&options.user_data_dir, &cwd).display()));
    argv.push(REMOTE_DEBUGGING_ARG.to_owned());
    argv.push("about:blank".to_owned());
    argv.retain(|arg| !options.ignore_default_args.contains(arg));
    argv
}

/// The command line `chromium.launch(...)` gives Chromium: the same defaults, the
/// throwaway profile Playwright makes for it (`options.user_data_dir`, a
/// `playwright_chromiumdev_profile-*` directory) and `--no-startup-window` where a
/// persistent context opens `about:blank`.
pub fn launch_argv(options: &LaunchOptions) -> Vec<String> {
    let mut argv = inner_default_args(options);
    argv.push(format!("--user-data-dir={}", options.user_data_dir.display()));
    argv.push(REMOTE_DEBUGGING_ARG.to_owned());
    argv.push("--no-startup-window".to_owned());
    argv.retain(|arg| !options.ignore_default_args.contains(arg));
    argv
}

/// The prefix of the throwaway profile `chromium.launch` creates in the temp directory.
pub const LAUNCH_PROFILE_PREFIX: &str = "playwright_chromiumdev_profile-";

/// The registry directory: `PLAYWRIGHT_BROWSERS_PATH`, else `$XDG_CACHE_HOME/ms-playwright`
/// (`~/.cache/ms-playwright`). Playwright's `0` (browsers inside the Python package) has no
/// package here, so it falls back to the default.
fn registry_directory() -> PathBuf {
    let from_env = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH").filter(|value| !value.is_empty() && value != "0");
    let directory = from_env.map_or_else(
        || {
            let cache = std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()).map_or_else(
                || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"),
                PathBuf::from,
            );
            cache.join("ms-playwright")
        },
        PathBuf::from,
    );
    std::path::absolute(&directory).unwrap_or(directory)
}

/// `playwright.chromium.executable_path`: the Chrome for Testing build of
/// [`CHROMIUM_REVISION`] in the registry (`chromium-1228/chrome-linux64/chrome` on x86-64).
pub fn playwright_chromium_executable() -> PathBuf {
    let platform_dir = if cfg!(target_arch = "aarch64") {
        "chrome-linux"
    } else {
        "chrome-linux64"
    };
    registry_directory()
        .join(format!("chromium-{CHROMIUM_REVISION}"))
        .join(platform_dir)
        .join("chrome")
}

/// Starting or stopping Chromium failed.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("Executable doesn't exist at {0}")]
    MissingExecutable(String),
    #[error("Failed to launch {executable}: {source}")]
    Spawn {
        executable: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Chromium exited before opening DevTools:\n{0}")]
    Exited(String),
    #[error("Timeout {0}ms exceeded while waiting for Chromium to open DevTools")]
    Timeout(u128),
}

/// A Chromium fav started itself, in its own process group.
#[derive(Debug)]
pub struct ChromiumProcess {
    child: tokio::process::Child,
    pid: Option<u32>,
    devtools_url: String,
}

impl ChromiumProcess {
    /// Start `executable argv...` with `env` added to the environment and wait for the
    /// `DevTools listening on ws://...` line. Its output is drained afterwards, so a full
    /// pipe never stalls the browser.
    pub async fn spawn(executable: &Path, argv: &[String], env: &[(String, String)], timeout: Duration) -> Result<Self, LaunchError> {
        if !executable.is_file() {
            return Err(LaunchError::MissingExecutable(executable.display().to_string()));
        }
        let mut command = tokio::process::Command::new(executable);
        command
            .args(argv)
            .envs(env.iter().map(|(name, value)| (name.as_str(), value.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|source| LaunchError::Spawn {
            executable: executable.display().to_string(),
            source,
        })?;
        let pid = child.id();
        let stderr = child.stderr.take().ok_or_else(|| LaunchError::Exited(String::new()))?;
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        let mut seen = Vec::new();
        let found = tokio::time::timeout(timeout, async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(url) = line.strip_prefix("DevTools listening on ") {
                    return Some(url.trim().to_owned());
                }
                if seen.len() < 40 {
                    seen.push(line);
                }
            }
            None
        })
        .await;
        let devtools_url = match found {
            Ok(Some(url)) => url,
            Ok(None) => {
                let mut process = Self {
                    child,
                    pid,
                    devtools_url: String::new(),
                };
                process.kill().await;
                return Err(LaunchError::Exited(seen.join("\n")));
            }
            Err(_) => {
                let mut process = Self {
                    child,
                    pid,
                    devtools_url: String::new(),
                };
                process.kill().await;
                return Err(LaunchError::Timeout(timeout.as_millis()));
            }
        };
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        Ok(Self { child, pid, devtools_url })
    }

    /// The browser's DevTools WebSocket (`ws://127.0.0.1:<port>/devtools/browser/<id>`).
    pub fn devtools_url(&self) -> &str {
        &self.devtools_url
    }

    /// Whether the browser process has exited.
    pub fn has_exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    /// Wait up to `timeout` for the browser to exit on its own.
    pub async fn wait_for_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        tokio::time::timeout(timeout, self.child.wait()).await.ok().and_then(Result::ok)
    }

    /// SIGKILL the whole process group (renderers, GPU and zygote helpers included).
    pub async fn kill(&mut self) {
        if let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

#[cfg(test)]
mod tests;
