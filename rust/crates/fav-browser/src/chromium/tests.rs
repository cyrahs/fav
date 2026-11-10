use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    ChromiumProcess, LaunchError, LaunchOptions, REMOTE_DEBUGGING_ARG, normalized_proxy_server, persistent_context_argv,
    playwright_chromium_executable, proxy_hostname,
};
use crate::proxy::build_proxy_settings;
use crate::testutil::golden_cases;
use crate::window::build_login_launch_options;

/// `rednote_browser.build_launch_options(...)` as far as the command line goes; the crawl
/// itself is WP17's.
fn rednote_launch(profile: &Path, proxy: &str, headless: bool) -> LaunchOptions {
    LaunchOptions {
        user_data_dir: profile.to_path_buf(),
        headless,
        proxy: build_proxy_settings(proxy).expect("a valid proxy"),
        args: vec![
            "--disable-dev-shm-usage".to_owned(),
            "--disable-blink-features=AutomationControlled".to_owned(),
        ],
        ignore_default_args: vec!["--enable-automation".to_owned()],
    }
}

#[test]
fn the_command_line_is_playwrights_own() {
    let profile = Path::new("<profile>");
    for case in golden_cases("browser_login/playwright_argv.json") {
        let proxy = case["proxy"].as_str().unwrap();
        let headless = case["headless"].as_bool().unwrap();
        let options = match case["launch"].as_str().unwrap() {
            "login" => build_login_launch_options(profile, proxy, 99).expect("valid").launch,
            "rednote" => rednote_launch(profile, proxy, headless),
            other => panic!("unknown launch {other}"),
        };
        assert_eq!(serde_json::json!(options.args), case["args"], "{case}");
        assert_eq!(
            serde_json::json!(options.ignore_default_args),
            case["ignore_default_args"],
            "{case}"
        );
        // The one argument that differs on purpose (D160).
        let argv: Vec<String> = persistent_context_argv(&options)
            .into_iter()
            .map(|arg| {
                if arg == REMOTE_DEBUGGING_ARG {
                    "--remote-debugging-pipe".to_owned()
                } else {
                    arg
                }
            })
            .collect();
        assert_eq!(serde_json::json!(argv), case["argv"], "{}/{headless}/{proxy}", case["launch"]);
    }
}

#[test]
fn proxy_servers_are_written_the_way_url_writes_them() {
    assert_eq!(normalized_proxy_server("http://proxy.example:80"), "http://proxy.example");
    assert_eq!(normalized_proxy_server("https://proxy.example:443"), "https://proxy.example");
    assert_eq!(normalized_proxy_server("http://proxy.example:3128"), "http://proxy.example:3128");
    assert_eq!(
        normalized_proxy_server("socks5://proxy.example:1080"),
        "socks5://proxy.example:1080"
    );
    assert_eq!(normalized_proxy_server("http://[::1]:80"), "http://[::1]");
    assert_eq!(proxy_hostname("socks5://proxy.example:1080"), "proxy.example");
    assert_eq!(proxy_hostname("socks5://[::1]:1080"), "[::1]");
    assert_eq!(proxy_hostname("socks5://proxy.example"), "proxy.example");
}

#[test]
fn the_executable_is_playwrights_bundled_build() {
    let executable = playwright_chromium_executable();
    let platform = if cfg!(target_arch = "aarch64") {
        "chrome-linux"
    } else {
        "chrome-linux64"
    };
    assert!(
        executable.ends_with(PathBuf::from("chromium-1228").join(platform).join("chrome")),
        "{}",
        executable.display()
    );
    assert!(executable.is_absolute());
}

/// A shell script standing in for Chromium.
fn script(directory: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = directory.join("chrome");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn a_missing_executable_is_named() {
    let error = ChromiumProcess::spawn(Path::new("/nonexistent/chrome"), &[], &[], Duration::from_secs(1))
        .await
        .expect_err("missing");
    assert!(
        matches!(&error, LaunchError::MissingExecutable(path) if path == "/nonexistent/chrome"),
        "{error:?}"
    );
    assert_eq!(error.to_string(), "Executable doesn't exist at /nonexistent/chrome");
}

#[tokio::test]
async fn a_browser_that_dies_at_start_reports_what_it_said() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = script(directory.path(), "echo 'cannot open display' >&2\nexit 1");

    let error = ChromiumProcess::spawn(&chrome, &[], &[], Duration::from_secs(10))
        .await
        .expect_err("exited");

    assert!(
        matches!(&error, LaunchError::Exited(output) if output.contains("cannot open display")),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_devtools_address_is_read_from_the_browsers_output_and_the_group_is_killed() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("env");
    let chrome = script(
        directory.path(),
        &format!(
            "echo \"$FAV_PROBE\" > {}\necho 'DevTools listening on ws://127.0.0.1:9/devtools/browser/x' >&2\nsleep 30 & wait",
            marker.display()
        ),
    );

    let mut process = ChromiumProcess::spawn(
        &chrome,
        &["--flag".to_owned()],
        &[("FAV_PROBE".to_owned(), "1".to_owned())],
        Duration::from_secs(10),
    )
    .await
    .expect("started");

    assert_eq!(process.devtools_url(), "ws://127.0.0.1:9/devtools/browser/x");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "1");
    assert!(!process.has_exited());
    process.kill().await;
    assert!(process.has_exited());
}

#[tokio::test]
async fn a_browser_that_never_opens_devtools_times_out() {
    let directory = tempfile::tempdir().unwrap();
    let chrome = script(directory.path(), "sleep 30");

    let error = ChromiumProcess::spawn(&chrome, &[], &[], Duration::from_millis(300))
        .await
        .expect_err("timeout");

    assert!(matches!(error, LaunchError::Timeout(300)), "{error:?}");
}
