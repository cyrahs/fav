//! The RedNote browser: its pure helpers against Python's goldens, and the real Chromium
//! against loopback pages shaped like the site. Nothing here opens RedNote.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use fav_core::golden;
use fav_core::logging::{self, Level};
use serde_json::json;

use super::*;
use crate::testutil::{Reply, chromium, golden_cases, serve};

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

// ---------- pure helpers ----------

#[test]
fn a_like_page_is_read_whichever_of_its_two_events_comes_first() {
    let mut matcher = LikePageMatcher::default();
    // The usual order: the status line, then the end of the body.
    assert_eq!(matcher.response("1".into(), "u1".into(), 200), None);
    assert_eq!(matcher.finished("1".into()), Some(("u1".to_owned(), 200)));
    // Merged from separate CDP listeners, the end can be seen first.
    assert_eq!(matcher.finished("2".into()), None);
    assert_eq!(matcher.response("2".into(), "u2".into(), 200), Some(("u2".to_owned(), 200)));
    // Each is delivered once.
    assert_eq!(matcher.finished("1".into()), None);
    assert_eq!(matcher.finished("2".into()), None);
    // A failed load is delivered without a body; a failure of a request it never saw is not.
    assert_eq!(matcher.response("3".into(), "u3".into(), 461), None);
    assert_eq!(matcher.failed("3"), Some(("u3".to_owned(), 461)));
    assert_eq!(matcher.failed("4"), None);
}

// py: tests/test_rednote.py::test_the_launch_options_ask_for_the_real_browser_not_the_headless_shell
#[test]
fn the_launch_options_ask_for_the_real_browser_not_the_headless_shell() {
    let options = build_launch_options(Path::new("/data/profile"), "", true).expect("no proxy");

    // `channel='chromium'`: the real build, never chromium-headless-shell.
    let executable = crate::chromium::playwright_chromium_executable();
    assert!(!executable.to_string_lossy().contains("headless_shell"), "{}", executable.display());
    assert_eq!(options.launch.user_data_dir, Path::new("/data/profile"));
    assert!(options.launch.headless);
    assert_eq!(options.context.locale.as_deref(), Some("zh-CN"));
    assert_eq!(options.context.timezone_id.as_deref(), Some("Asia/Shanghai"));
    assert_eq!(options.context.viewport, Some((1280, 800)));
    // /dev/shm is 64 MB in a container, and Chromium answers that with renderer crashes.
    assert!(options.launch.args.contains(&"--disable-dev-shm-usage".to_owned()));
    assert_eq!(options.launch.ignore_default_args, ["--enable-automation"]);
}

// py: tests/test_rednote.py::test_the_launch_options_do_not_pin_a_user_agent
#[test]
fn the_launch_options_do_not_pin_a_user_agent() {
    // The UA is corrected after launch, once the browser's own version can be read.
    let options = build_launch_options(Path::new("/p"), "", true).expect("no proxy");
    let argv = crate::chromium::persistent_context_argv(&options.launch);
    assert!(!argv.iter().any(|arg| arg.starts_with("--user-agent")), "{argv:?}");
}

#[test]
fn a_proxy_with_credentials_answers_its_challenge_over_cdp() {
    let options = build_launch_options(Path::new("/p"), "http://user:pw@home.example:3128", true).expect("valid");
    assert_eq!(
        options.launch.proxy.as_ref().map(|proxy| proxy.server.as_str()),
        Some("http://home.example:3128")
    );
    assert_eq!(options.context.proxy_credentials, Some(("user".to_owned(), "pw".to_owned())));
    assert!(build_launch_options(Path::new("/p"), "socks5://user:pw@home.example:1080", true).is_err());
}

// py: tests/test_rednote.py::test_a_run_explains_its_payloads_without_repeating_their_contents
#[test]
fn a_run_explains_its_payloads_without_repeating_their_contents() {
    let envelope = pyjson::loads(
        r#"{"code": 0, "success": true, "data": {"has_more": true, "cursor": "65a1b2c3000000000e01f00d", "notes": [
            {"note_id": "64f1a2b3000000001e02c0de", "xsec_token": "AB-secret-token", "display_title": "我的私人笔记"},
            {"note_id": "second", "xsec_token": "also-secret", "display_title": "另一条"}]}}"#,
    )
    .unwrap();

    let shape = describe_shape(&envelope, 5);
    let rendered = fav_runtime::pystr::repr_value(&shape);

    let notes = shape
        .get("data")
        .and_then(|data| data.get("notes"))
        .and_then(pyjson::Value::as_list)
        .unwrap();
    assert_eq!(notes[0], pyjson::Value::from("list[2]"));
    assert_eq!(
        notes[1],
        pyjson::loads(r#"{"note_id": "str[24]", "xsec_token": "str[15]", "display_title": "str[6]"}"#).unwrap()
    );
    assert_eq!(shape.get("code"), Some(&pyjson::Value::from("int")));
    assert_eq!(shape.get("success"), Some(&pyjson::Value::from("bool")));
    for secret in [
        "64f1a2b3000000001e02c0de",
        "AB-secret-token",
        "我的私人笔记",
        "65a1b2c3000000000e01f00d",
    ] {
        assert!(!rendered.contains(secret), "{rendered}");
    }
}

// py: tests/test_rednote.py::test_the_logged_login_probe_keeps_the_qr_and_the_account_id_out_of_it
#[test]
fn the_logged_login_probe_keeps_the_qr_and_the_account_id_out_of_it() {
    let probe = object(json!({
        "logged_in": false,
        "qr_stage": "verify",
        "qr_src": format!("data:image/png;base64,{}", "A".repeat(7000)),
        "user_id": "6a7d8554000000001400c801",
        "modal_html": "<div class=\"r-captcha-modal\">...</div>",
    }));

    let redacted = redact_probe(&probe);
    let rendered = Value::Object(redacted.clone()).to_string();

    assert_eq!(redacted["qr_stage"], json!("verify"));
    assert_eq!(redacted["logged_in"], json!(false));
    assert_eq!(redacted["qr_src"], json!("str[7022]"));
    assert!(!redacted.contains_key("modal_html"));
    assert!(!rendered.contains("AAAA"));
    assert!(!rendered.contains("6a7d8554000000001400c801"));
}

// py: tests/test_rednote.py::test_a_page_is_identified_by_the_cursor_it_was_fetched_with
#[test]
fn a_page_is_identified_by_the_cursor_it_was_fetched_with() {
    assert_eq!(
        cursor_of("https://edith.xiaohongshu.com/api/sns/web/v1/note/like/page?num=30&cursor=C1"),
        "C1"
    );
    assert_eq!(cursor_of("https://edith.xiaohongshu.com/api/sns/web/v1/note/like/page?num=30"), "");
}

// py: tests/test_rednote.py::test_pending_requests_are_summarized_oldest_first_without_their_ids
#[test]
fn pending_requests_are_summarized_oldest_first_without_their_ids() {
    let entries = [
        (
            "xhr".to_owned(),
            "https://edith.xiaohongshu.com/api/sns/web/v1/note/like/page?user_id=5ff&cursor=abc".to_owned(),
            7.0,
        ),
        (
            "document".to_owned(),
            "https://www.xiaohongshu.com/explore?exSource=null".to_owned(),
            1.0,
        ),
        (
            "script".to_owned(),
            "https://fe-static.xhscdn.com/formula-static/xhs-pc-web/vendor.js".to_owned(),
            2.0,
        ),
    ];

    assert_eq!(
        summarize_pending(&entries, 31.0, 2),
        "document www.xiaohongshu.com/explore 30.0s, script fe-static.xhscdn.com/formula-static 29.0s, and 1 more"
    );
    assert_eq!(summarize_pending(&[], 0.0, PENDING_LOG_LIMIT), "nothing");
}

#[test]
fn the_helpers_answer_as_python_does() {
    let document = golden::load("rednote/browser_helpers.json");
    let typed = golden::cases(&document);
    let cases = golden_cases("rednote/browser_helpers.json");
    assert!(cases.len() > 20);
    for (case, typed) in cases.iter().zip(typed) {
        let function = case["function"].as_str().unwrap();
        let expected = &case["result"];
        match function {
            "describe_shape" => {
                let depth = u32::try_from(case["depth"].as_u64().unwrap()).unwrap();
                assert_eq!(
                    describe_shape(golden::get(typed, "value"), depth),
                    *golden::get(typed, "result"),
                    "{case}"
                );
            }
            "redact_probe" => {
                let probe = object(case["probe"].clone());
                assert_eq!(&Value::Object(redact_probe(&probe)), expected, "{case}");
            }
            "summarize_pending" => {
                let entries: Vec<(String, String, f64)> = case["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entry| {
                        (
                            entry[0].as_str().unwrap().to_owned(),
                            entry[1].as_str().unwrap().to_owned(),
                            entry[2].as_f64().unwrap(),
                        )
                    })
                    .collect();
                let limit = usize::try_from(case["limit"].as_u64().unwrap()).unwrap();
                assert_eq!(
                    json!(summarize_pending(&entries, case["now"].as_f64().unwrap(), limit)),
                    *expected,
                    "{case}"
                );
            }
            "cursor_of" => assert_eq!(json!(cursor_of(case["url"].as_str().unwrap())), *expected, "{case}"),
            "url_head" => assert_eq!(json!(url_head(case["url"].as_str().unwrap())), *expected, "{case}"),
            other => panic!("unknown helper {other}"),
        }
    }
}

// ---------- the real browser on loopback ----------

const WAIT: Duration = Duration::from_secs(20);

fn quick() -> NoteTimings {
    NoteTimings {
        goto: Duration::from_secs(10),
        login_render: Duration::from_secs(10),
        login_render_poll: Duration::from_millis(100),
        liked_tab: Duration::from_secs(5),
        page_poll: Duration::from_millis(200),
        scroll_pause: Duration::from_millis(50),
        hints_retry: Duration::ZERO,
    }
}

fn browser(executable: PathBuf, profile: &Path, addr: std::net::SocketAddr, timings: NoteTimings) -> ChromiumNoteBrowser {
    // The neutral page is on another origin than the site, as the live one is.
    ChromiumNoteBrowser::new(executable, profile, "", true)
        .expect("no proxy")
        .with_site(NoteSite {
            origin: format!("http://{addr}"),
            hints_url: format!("http://localhost:{}/neutral", addr.port()),
        })
        .with_timings(timings)
}

fn json_reply(body: &Value) -> Reply {
    Reply {
        content_type: "application/json",
        body: body.to_string(),
        ..Reply::html("")
    }
}

fn page_with_state(state: &Value, body: &str) -> Reply {
    Reply::html(&format!(
        "<html><body>{body}<script>window.__INITIAL_STATE__ = {state};</script></body></html>"
    ))
}

const SIGNED_IN: &str = r#"{"user": {"loggedIn": {"_value": true}, "userInfo": {"_value": {"userId": "me", "guest": false}}}}"#;

/// The liked tab: its first screenful in the page, the first XHR page on the tab click,
/// one more per scroll, and the end of the list after that.
const PROFILE_BODY: &str = "<div style='height:4000px'>\
    <span class='reds-tab-item'>笔记</span><span class='reds-tab-item' id='liked'>点赞 3</span></div>\
    <script>\
    let fetched = 0;\
    const more = () => { if (fetched >= 2) { return; } fetched += 1;\
      fetch('/api/sns/web/v1/note/like/page?num=30&cursor=' + (fetched === 1 ? '' : 'c1')); };\
    document.getElementById('liked').addEventListener('click', more);\
    window.addEventListener('wheel', more);\
    </script>";

fn site(request: &str) -> Reply {
    let line = request.lines().next().unwrap_or_default();
    let path = line.split(' ').nth(1).unwrap_or_default();
    if path.starts_with("/neutral") {
        Reply::html("<html><body>neutral</body></html>")
    } else if path.starts_with("/user/profile/me") {
        let state = json!({
            "user": {
                "notes": {"_value": [[], [], [
                    {"id": "n1", "xsecToken": "t1", "noteCard": {"noteId": "n1", "displayTitle": "newest", "type": "normal"}},
                    {"noteCard": {"displayTitle": "no id"}},
                ]]},
                "activeTab": {"_value": {"index": 2}},
            }
        });
        page_with_state(&state, PROFILE_BODY)
    } else if path.starts_with(LIKE_PAGE_PATH) {
        let body = if path.ends_with("cursor=c1") {
            json!({"code": 0, "data": {"notes": [], "cursor": "", "has_more": false}})
        } else {
            json!({"code": 0, "data": {"notes": [{"note_id": "n2", "xsec_token": "t2"}], "cursor": "c1", "has_more": true}})
        };
        json_reply(&body)
    } else if path.starts_with("/explore/gone") {
        Reply {
            status: "302 Found",
            headers: "Location: /404?source=note\r\n".to_owned(),
            ..Reply::html("")
        }
    } else if path.starts_with("/404") {
        Reply::html("<html><body>404</body></html>")
    } else if path.starts_with("/explore/n1") {
        let state = json!({"note": {"noteDetailMap": {"n1": {"note": {"type": "normal", "title": "a caption"}}}}});
        page_with_state(&state, "note")
    } else if path.starts_with("/explore/n2") {
        // Loaded fine, but the note is not in the map: missing means missing.
        let state = json!({"note": {"noteDetailMap": {"other": {"note": {"type": "normal"}}}}});
        page_with_state(&state, "note")
    } else {
        page_with_state(&serde_json::from_str(SIGNED_IN).unwrap(), "<div class='side-bar'></div>")
            .with_header("Set-Cookie: web_session=abc; Path=/")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn walks_a_local_likes_list_like_the_site() {
    let Some(executable) = chromium() else { return };
    let (addr, _seen) = serve(site).await;
    let profile = tempfile::tempdir().unwrap();
    // A lock a killed Chromium left behind, stamped with another pod's hostname.
    std::os::unix::fs::symlink("other-host-1234", profile.path().join("SingletonLock")).unwrap();
    let mut browser = browser(executable, profile.path(), addr, quick());

    browser.start().await.expect("started");
    let probe = browser.probe_login().await.expect("probed");
    assert_eq!(probe["logged_in"], json!(true));
    assert_eq!(crate::rednote::user_id_from_probe(&probe), "me");
    assert_eq!(
        browser.cookie_dict().await.unwrap().get("web_session").map(String::as_str),
        Some("abc")
    );
    assert_eq!(browser.cookies().await.unwrap().len(), 1);
    let user_agent = browser.user_agent().await.unwrap();
    assert!(
        user_agent.contains("Macintosh") && !user_agent.contains("HeadlessChrome"),
        "{user_agent}"
    );
    browser.reload_login().await.expect("reloaded");

    assert!(browser.open_likes("me").await.expect("opened"));
    let initial = browser.initial_like_notes().await.unwrap();
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].get("note_id"), Some(&pyjson::Value::from("n1")));
    assert_eq!(initial[0].get("xsec_token"), Some(&pyjson::Value::from("t1")));

    // The tab click fetched the first page; scrolling fetches the next one.
    let first = browser.next_like_page(WAIT).await.unwrap().expect("the first page");
    assert_eq!((first.status, cursor_of(&first.url)), (200, String::new()));
    assert_eq!(
        first
            .body
            .as_ref()
            .and_then(|body| body.get("data"))
            .and_then(|data| data.get("cursor")),
        Some(&pyjson::Value::from("c1"))
    );
    let second = browser.next_like_page(WAIT).await.unwrap().expect("the second page");
    assert_eq!(cursor_of(&second.url), "c1");
    // Nothing more comes, however much it scrolls.
    assert_eq!(browser.next_like_page(Duration::from_millis(600)).await.unwrap(), None);

    let origin = format!("http://{addr}");
    let note = browser
        .note_state(&format!("{origin}/explore/n1?xsec_token=t1"), "n1")
        .await
        .unwrap();
    assert_eq!(note.get("title"), Some(&pyjson::Value::from("a caption")));
    let missing = browser.note_state(&format!("{origin}/explore/n2"), "n2").await.unwrap();
    assert_eq!(missing, pyjson::Value::Dict(Dict::new()));
    match browser.note_state(&format!("{origin}/explore/gone"), "gone").await {
        Err(NoteBrowserError::Gone(message)) => assert_eq!(message, "RedNote redirected note gone to 404"),
        other => panic!("expected the note to be gone, got {other:?}"),
    }
    browser.aclose().await;
    assert!(!profile.path().join("SingletonLock").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_profile_without_the_liked_tab_is_reported_not_crawled() {
    let Some(executable) = chromium() else { return };
    let (addr, _seen) = serve(|request: &str| {
        if request.starts_with("get /user/profile/") {
            Reply::html("<html><body><span class='reds-tab-item'>笔记</span></body></html>")
        } else {
            Reply::html("<html><body>neutral</body></html>")
        }
    })
    .await;
    let profile = tempfile::tempdir().unwrap();
    let mut timings = quick();
    timings.liked_tab = Duration::from_millis(300);
    let mut browser = browser(executable, profile.path(), addr, timings);
    browser.start().await.expect("started");

    assert!(!browser.open_likes("me").await.expect("the page loaded"));
    browser.aclose().await;
}

/// A page that renders its answer late, as the live SPA does after `DOMContentLoaded`.
fn late_page(script: &'static str) -> impl Fn(&str) -> Reply + Send + Sync + 'static {
    move |request: &str| {
        if request.starts_with("get /neutral") || request.starts_with("get /favicon") {
            Reply::html("<html><body>neutral</body></html>")
        } else {
            Reply::html(&format!("<html><body><script>{script}</script></body></html>"))
        }
    }
}

// py: tests/test_rednote.py::test_the_login_probe_waits_for_the_page_to_mount_before_believing_it
#[tokio::test(flavor = "multi_thread")]
async fn the_login_probe_waits_for_the_page_to_mount_before_believing_it() {
    let Some(executable) = chromium() else { return };
    // Hydrating, then the modal with no QR drawn yet, then the QR.
    let (addr, _seen) = serve(late_page(
        "setTimeout(() => { window.__INITIAL_STATE__ = {user: {}}; \
           const modal = document.createElement('div'); modal.className = 'login-modal'; document.body.appendChild(modal); \
           setTimeout(() => { modal.innerHTML = \"<img class='qrcode-img' src='data:image/png;base64,QR'>\"; }, 400); }, 400);",
    ))
    .await;
    let profile = tempfile::tempdir().unwrap();
    let mut browser = browser(executable, profile.path(), addr, quick());
    browser.start().await.expect("started");

    let probe = browser.probe_login().await.expect("probed");

    assert_eq!(probe["qr_src"], json!("data:image/png;base64,QR"));
    assert_eq!(probe["has_login_modal"], json!(true));
    assert_eq!(probe["qr_stage"], json!("login"));
    browser.aclose().await;
}

// py: tests/test_rednote.py::test_the_login_probe_stops_waiting_once_the_page_says_it_is_signed_in
#[tokio::test(flavor = "multi_thread")]
async fn the_login_probe_stops_waiting_once_the_page_says_it_is_signed_in() {
    let Some(executable) = chromium() else { return };
    let (addr, _seen) = serve(site).await;
    let profile = tempfile::tempdir().unwrap();
    let mut timings = quick();
    timings.login_render = Duration::from_mins(1);
    let mut browser = browser(executable, profile.path(), addr, timings);
    browser.start().await.expect("started");

    let started = Instant::now();
    let probe = browser.probe_login().await.expect("probed");

    assert_eq!(probe["logged_in"], json!(true));
    assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
    browser.aclose().await;
}

// py: tests/test_rednote.py::test_the_login_probe_gives_up_rather_than_waiting_out_the_whole_run
#[tokio::test(flavor = "multi_thread")]
async fn the_login_probe_gives_up_rather_than_waiting_out_the_whole_run() {
    let Some(executable) = chromium() else { return };
    // Never signed in, never a QR: the probe still returns what it saw.
    let (addr, _seen) = serve(late_page("window.__INITIAL_STATE__ = {user: {}};")).await;
    let profile = tempfile::tempdir().unwrap();
    let mut timings = quick();
    timings.login_render = Duration::ZERO;
    let mut browser = browser(executable, profile.path(), addr, timings);
    browser.start().await.expect("started");

    let probe = browser.probe_login().await.expect("probed");

    assert_eq!(probe["logged_in"], json!(false));
    assert_eq!(probe["has_login_modal"], json!(false));
    assert_eq!(probe["qr_src"], json!(""));
    browser.aclose().await;
}

/// The site's front page, stalled past the navigation timeout `stalls` times.
fn stalling_site(stalls: usize) -> (impl Fn(&str) -> Reply + Send + Sync + 'static, Arc<AtomicUsize>) {
    let homepage_requests = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&homepage_requests);
    let answer = move |request: &str| {
        if request.starts_with("get / ") {
            let seen = counted.fetch_add(1, Ordering::SeqCst);
            let page = page_with_state(&serde_json::from_str(SIGNED_IN).unwrap(), "home");
            return if seen < stalls { page.after(Duration::from_secs(5)) } else { page };
        }
        Reply::html("<html><body>neutral</body></html>")
    };
    (answer, homepage_requests)
}

fn stalling_timings() -> NoteTimings {
    NoteTimings {
        goto: Duration::from_millis(1500),
        ..quick()
    }
}

/// Run a test body on a current-thread runtime, so every record its tasks log is captured.
fn with_captured_log(body: impl std::future::Future<Output = ()>) -> Vec<logging::Record> {
    logging::capture(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(body);
    })
}

// py: tests/test_rednote.py::test_a_navigation_that_times_out_once_is_retried_rather_than_failing_the_run
// py: tests/test_rednote.py::test_the_retry_after_a_timeout_runs_in_a_relaunched_browser
#[test]
fn a_navigation_that_times_out_once_is_retried_in_a_relaunched_browser() {
    let Some(executable) = chromium() else { return };
    let records = with_captured_log(async move {
        let (answer, homepage_requests) = stalling_site(1);
        let (addr, _seen) = serve(answer).await;
        let profile = tempfile::tempdir().unwrap();
        let mut browser = browser(executable, profile.path(), addr, stalling_timings());
        browser.start().await.expect("started");

        let probe = browser.probe_login().await.expect("the retry got through");

        assert_eq!(probe["logged_in"], json!(true));
        assert_eq!(homepage_requests.load(Ordering::SeqCst), 2);
        // The retry runs on a fresh socket pool, not on the connection that stalled.
        assert_eq!(browser.relaunches(), 1);
        browser.aclose().await;
    });
    let warnings: Vec<&str> = records
        .iter()
        .filter(|record| record.level == Level::Warning)
        .map(|record| record.message.as_str())
        .collect();
    assert!(
        warnings
            .iter()
            .any(|message| message.contains("timed out (attempt 1)") && message.contains("still waiting on: document 127.0.0.1:")),
        "{warnings:?}"
    );
    assert!(
        warnings.iter().any(|message| message.contains("relaunching the browser")),
        "{warnings:?}"
    );
}

// py: tests/test_rednote.py::test_a_navigation_that_keeps_timing_out_still_fails
#[tokio::test(flavor = "multi_thread")]
async fn a_navigation_that_keeps_timing_out_still_fails() {
    let Some(executable) = chromium() else { return };
    let (answer, homepage_requests) = stalling_site(usize::MAX);
    let (addr, _seen) = serve(answer).await;
    let profile = tempfile::tempdir().unwrap();
    let mut browser = browser(executable, profile.path(), addr, stalling_timings());
    browser.start().await.expect("started");

    let failure = browser.probe_login().await.expect_err("an unreachable site is still reported");

    assert_eq!(failure.class_name(), "TimeoutError");
    assert_eq!(homepage_requests.load(Ordering::SeqCst), GOTO_ATTEMPTS as usize);
    // Relaunched between attempts, not after the last one: aclose owns that.
    assert_eq!(browser.relaunches(), GOTO_ATTEMPTS - 1);
    browser.aclose().await;
}
