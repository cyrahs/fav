use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::{VncChromium, build_login_launch_options, build_plain_chromium_command, cookies_for_urls, spawn_error};
use crate::chromium::LaunchOptions;
use crate::driver::{BrowserContext, ContextOptions};
use crate::login::{LoginBrowser, PreparePage};
use crate::testutil::{Reply, chromium, golden_cases, serve};

// py: tests/test_browser_login.py::test_the_login_window_is_headed_on_the_virtual_display
#[test]
fn the_login_window_is_headed_on_the_virtual_display() {
    let options = build_login_launch_options(Path::new("/data/p"), "", 99).expect("valid");

    assert!(!options.launch.headless);
    assert_eq!(options.env, [("DISPLAY".to_owned(), ":99".to_owned())]);
    // `channel='chromium'`: the real browser build, which is the only one fav starts.
    assert_eq!(options.launch.proxy, None);
    assert!(options.launch.ignore_default_args.contains(&"--enable-automation".to_owned()));
    assert_eq!(options.context.viewport, None);
    assert_eq!(options.context.locale.as_deref(), Some("zh-CN"));
    assert_eq!(options.context.timezone_id.as_deref(), Some("Asia/Shanghai"));
}

#[test]
fn the_login_window_answers_its_proxys_challenge_itself() {
    let options = build_login_launch_options(Path::new("/data/p"), "http://user:pw@proxy.example:3128", 99).expect("valid");

    assert_eq!(
        options.launch.proxy.as_ref().map(|proxy| proxy.server.as_str()),
        Some("http://proxy.example:3128")
    );
    assert_eq!(options.context.proxy_credentials, Some(("user".to_owned(), "pw".to_owned())));
    let refused = build_login_launch_options(Path::new("/data/p"), "socks5://user:pw@proxy.example:1080", 99).expect_err("SOCKS");
    assert_eq!(refused.class, "ProxyConfigurationError");
}

fn plain_command(proxy: &str) -> Result<Vec<String>, crate::login::LoginError> {
    build_plain_chromium_command(
        "/opt/chrome",
        Path::new("/data/profiles/x"),
        proxy,
        5998,
        "https://x.com/i/flow/login",
    )
}

// py: tests/test_browser_login.py::test_the_plain_window_is_not_an_automated_browser
#[test]
fn the_plain_window_is_not_an_automated_browser() {
    let command = plain_command("").expect("valid");

    assert_eq!(command[0], "/opt/chrome");
    assert_eq!(command.last().unwrap(), "https://x.com/i/flow/login");
    assert!(command.contains(&"--user-data-dir=/data/profiles/x".to_owned()));
    // DevTools on loopback only, and none of the switches that mark a driven browser.
    assert!(command.contains(&"--remote-debugging-port=5998".to_owned()));
    assert!(command.contains(&"--remote-debugging-address=127.0.0.1".to_owned()));
    assert!(!command.iter().any(|arg| {
        ["--enable-automation", "--headless", "--remote-debugging-pipe"]
            .iter()
            .any(|switch| arg.starts_with(switch))
    }));
    assert!(!command.iter().any(|arg| arg.starts_with("--proxy-server")));
}

// py: tests/test_browser_login.py::test_the_plain_window_uses_the_sites_proxy
#[test]
fn the_plain_window_uses_the_sites_proxy() {
    let command = plain_command("http://home.proxy.svc.cluster.local").expect("valid");
    assert!(command.contains(&"--proxy-server=http://home.proxy.svc.cluster.local".to_owned()));
}

// py: tests/test_browser_login.py::test_the_plain_window_refuses_a_proxy_with_credentials
#[test]
fn the_plain_window_refuses_a_proxy_with_credentials() {
    let error = plain_command("http://user:pass@proxy.example:3128").expect_err("refused");
    assert_eq!(error.class, "LoginError");
    assert!(error.message.contains("proxy"), "{error}");
}

#[test]
fn plain_commands_match_python() {
    for case in golden_cases("browser_login/plain_chromium_command.json") {
        let text = |key: &str| case[key].as_str().expect("a string").to_owned();
        let port = u16::try_from(case["devtools_port"].as_u64().expect("a port")).expect("a port");
        let built = build_plain_chromium_command(
            &text("executable"),
            Path::new(&text("profile_dir")),
            &text("proxy"),
            port,
            &text("url"),
        );
        match (built, case.get("command"), case.get("error")) {
            (Ok(command), Some(expected), None) => assert_eq!(json!(command), *expected, "{case}"),
            (Err(error), None, Some(raised)) => {
                assert_eq!(error.class, raised["class"], "{case}");
                assert_eq!(error.message, raised["message"], "{case}");
            }
            (built, _, _) => panic!("{case}: {built:?}"),
        }
    }
}

// py: tests/test_browser_login.py::test_the_jar_is_narrowed_to_the_sites_cookies
#[test]
fn the_jar_is_narrowed_to_the_sites_cookies() {
    let jar = [
        json!({"name": "auth_token", "domain": ".x.com"}),
        json!({"name": "host_only", "domain": "x.com"}),
        json!({"name": "sub_only", "domain": "api.x.com"}),
        json!({"name": "other", "domain": ".pixiv.net"}),
        json!({"name": "lookalike", "domain": ".notx.com"}),
    ];

    let names: Vec<Value> = cookies_for_urls(&jar, &["https://x.com/".to_owned()])
        .into_iter()
        .map(|cookie| cookie["name"].clone())
        .collect();

    assert_eq!(names, [json!("auth_token"), json!("host_only")]);
}

#[test]
fn narrowed_jars_match_python() {
    for case in golden_cases("browser_login/cookies_for_urls.json") {
        let jar = case["cookies"].as_array().expect("a list").clone();
        let urls: Vec<String> = case["urls"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|url| url.as_str().expect("a string").to_owned())
            .collect();
        assert_eq!(json!(cookies_for_urls(&jar, &urls)), case["result"], "{case}");
    }
}

#[test]
fn a_missing_program_reads_as_pythons_oserror() {
    let error = spawn_error("Xvnc", &std::io::Error::from_raw_os_error(2));
    assert_eq!(error.class, "FileNotFoundError");
    assert_eq!(error.message, "[Errno 2] No such file or directory: 'Xvnc'");
    assert_eq!(
        error.attempt_message(),
        "FileNotFoundError: [Errno 2] No such file or directory: 'Xvnc'"
    );
}

const EXPLORE: &str = "<html><body><a id='out' href='/elsewhere' target='_blank'>out</a></body></html>";

// py: tests/test_browser_login.py::test_the_disguised_login_window_keeps_to_one_tab
#[tokio::test(flavor = "multi_thread")]
async fn the_disguised_login_window_keeps_to_one_tab() {
    // The UA override is per tab: a second one would show the session as Linux.
    let Some(executable) = chromium() else { return };
    let (addr, _) = serve(|_| Reply::html(EXPLORE)).await;
    let profile = tempfile::tempdir().unwrap();
    let launch = LaunchOptions {
        user_data_dir: profile.path().to_path_buf(),
        headless: true,
        proxy: None,
        args: Vec::new(),
        ignore_default_args: Vec::new(),
    };
    let context = BrowserContext::launch(&executable, &launch, &[], ContextOptions::default())
        .await
        .expect("launched");
    let prepared = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&prepared);
    let prepare: PreparePage = Arc::new(move |_context, _page| {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    });
    let window = VncChromium::new(None, context, Some(prepare));

    let url = format!("http://{addr}/explore");
    window.open(&url).await.expect("opened");
    assert!(prepared.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(window.evaluate("() => location.pathname").await.unwrap(), json!("/explore"));
    let mut created = window.context.page_created().await.unwrap();
    window
        .evaluate("() => { window.open('/popup'); document.getElementById('out').click(); }")
        .await
        .unwrap();

    // Both extra tabs open, and are closed as they do; the login tab stays.
    for _ in 0..2 {
        use futures_util::StreamExt as _;
        tokio::time::timeout(Duration::from_secs(10), created.next())
            .await
            .expect("a tab opened");
    }
    let mut pages = 0;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        pages = window.context.pages().await.unwrap().len();
        if pages == 1 {
            break;
        }
    }
    assert_eq!(pages, 1);
    assert_eq!(window.evaluate("() => location.pathname").await.unwrap(), json!("/explore"));
    assert!(!window.closed().await);
    let error = window
        .local_storage(&url, "key")
        .await
        .expect_err("only the plain window reads storage");
    assert_eq!(error.class, "LoginError");

    window.close().await.unwrap();
    assert!(window.closed().await);
}
