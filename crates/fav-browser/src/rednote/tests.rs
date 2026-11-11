use std::sync::Mutex;
use std::time::Duration;

use fav_runtime::BoxFuture;
use serde_json::{Map, Value, json};

use super::{HINTS_ATTEMPTS, ProbePage, present_mac_user_agent, read_login_probe, user_id_from_probe};
use crate::driver::DriverError;
use crate::testutil::golden_cases;

fn probe(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

// py: tests/test_rednote.py::test_the_profile_id_is_never_taken_from_a_signed_out_page
#[test]
fn the_profile_id_is_never_taken_from_a_signed_out_page() {
    // A signed-out visitor is issued a guest id of exactly the same shape.
    assert_eq!(user_id_from_probe(&probe(json!({"logged_in": true, "user_id": "5ff"}))), "5ff");
    assert_eq!(
        user_id_from_probe(&probe(json!({"logged_in": true, "profile_href": "/user/profile/5ff?tab=liked"}))),
        "5ff"
    );
    // The guest identity, as the live signed-out page actually reports it.
    assert_eq!(
        user_id_from_probe(&probe(
            json!({"logged_in": false, "guest": true, "user_id": "6a7d8554000000001400c801"})
        )),
        ""
    );
    assert_eq!(user_id_from_probe(&Map::new()), "");
}

#[test]
fn user_ids_from_probes_match_python() {
    for case in golden_cases("browser_login/user_id_from_probe.json") {
        let expected = case["user_id"].as_str().expect("a string");
        assert_eq!(user_id_from_probe(&probe(case["probe"].clone())), expected, "{}", case["probe"]);
    }
}

const LINUX_HEADLESS_UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/140.0.0.0 Safari/537.36";

fn real_hints() -> Value {
    json!({"brands": [{"brand": "Chromium", "version": "140"}], "platform": "Linux", "uaFullVersion": "140.0.7339.80"})
}

/// A blank page whose neutral navigation fails, the way the proxy made it on 2026-09-09:
/// `goto` fails, Chromium commits its error page a beat later, and the first evaluate sent
/// in that beat finds its execution context gone.
struct ErrorPageRacePage {
    destroyed_reads: u32,
    hint_reads: Mutex<u32>,
    gotos: Mutex<Vec<String>>,
    sent: Mutex<Vec<(String, Value)>>,
}

impl ErrorPageRacePage {
    fn new(destroyed_reads: u32) -> Self {
        Self {
            destroyed_reads,
            hint_reads: Mutex::new(0),
            gotos: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
        }
    }
}

impl ProbePage for ErrorPageRacePage {
    fn evaluate<'a>(&'a self, script: &'a str) -> BoxFuture<'a, Result<Value, DriverError>> {
        Box::pin(async move {
            if script.ends_with("navigator.userAgent") {
                return Ok(json!(LINUX_HEADLESS_UA));
            }
            if script.contains("Boolean(navigator.userAgentData)") {
                return Ok(json!(false));
            }
            let mut reads = self.hint_reads.lock().unwrap();
            *reads += 1;
            if *reads <= self.destroyed_reads {
                return Err(DriverError::Evaluation(
                    "Execution context was destroyed, most likely because of a navigation".to_owned(),
                ));
            }
            Ok(real_hints())
        })
    }

    fn goto<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<(), DriverError>> {
        Box::pin(async move {
            self.gotos.lock().unwrap().push(url.to_owned());
            Err(DriverError::Navigation(format!("net::ERR_PROXY_CONNECTION_FAILED at {url}")))
        })
    }

    fn send<'a>(&'a self, method: &'a str, params: Value) -> BoxFuture<'a, Result<Value, DriverError>> {
        Box::pin(async move {
            self.sent.lock().unwrap().push((method.to_owned(), params));
            Ok(json!({}))
        })
    }
}

async fn present(page: &ErrorPageRacePage) -> Value {
    assert!(present_mac_user_agent(page, Duration::ZERO).await.expect("presented"));
    let sent = page.sent.lock().unwrap().clone();
    let [(method, override_params)] = <[_; 1]>::try_from(sent).expect("one override");
    assert_eq!(method, "Emulation.setUserAgentOverride");
    override_params
}

// py: tests/test_rednote.py::test_a_client_hints_read_cut_short_by_the_error_page_is_tried_again
#[tokio::test]
async fn a_client_hints_read_cut_short_by_the_error_page_is_tried_again() {
    // The 2026-09-09 scheduled run: the neutral page failed through the proxy and the run
    // died reading hints off a page that was being replaced under it.
    let page = ErrorPageRacePage::new(1);

    let override_params = present(&page).await;

    assert_eq!(*page.hint_reads.lock().unwrap(), 2);
    assert_eq!(*page.gotos.lock().unwrap(), [super::EXIT_IP_URL]);
    let user_agent = override_params["userAgent"].as_str().unwrap();
    assert!(!user_agent.contains("HeadlessChrome"), "{user_agent}");
    assert!(user_agent.contains("(Macintosh; Intel Mac OS X 10_15_7)"), "{user_agent}");
    // The real browser's brands and version, under the corrected platform.
    let metadata = &override_params["userAgentMetadata"];
    assert_eq!(metadata["brands"], real_hints()["brands"]);
    assert_eq!(metadata["fullVersion"], "140.0.7339.80");
    assert_eq!(metadata["platform"], "macOS");
    assert_eq!(metadata["majorVersion"], "140");
    assert_eq!(override_params["platform"], "MacIntel");
    assert_eq!(override_params["acceptLanguage"], "zh-CN,zh;q=0.9");
}

// py: tests/test_rednote.py::test_hints_that_never_come_back_cost_the_metadata_and_not_the_run
#[tokio::test]
async fn hints_that_never_come_back_cost_the_metadata_and_not_the_run() {
    // The UA string is the token the site refuses; the hints are the story around it.
    // Losing the story to a proxy hiccup is a degraded run, not a failed one.
    let page = ErrorPageRacePage::new(HINTS_ATTEMPTS);

    let override_params = present(&page).await;

    assert_eq!(*page.hint_reads.lock().unwrap(), HINTS_ATTEMPTS);
    assert!(
        override_params["userAgent"]
            .as_str()
            .unwrap()
            .contains("(Macintosh; Intel Mac OS X 10_15_7)")
    );
    assert!(override_params.get("userAgentMetadata").is_none(), "{override_params}");
}

/// A page that already says it is a Mac with a window.
struct MacPage;

impl ProbePage for MacPage {
    fn evaluate<'a>(&'a self, _script: &'a str) -> BoxFuture<'a, Result<Value, DriverError>> {
        Box::pin(async {
            Ok(json!(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36"
            ))
        })
    }

    fn goto<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, Result<(), DriverError>> {
        Box::pin(async { panic!("a Mac browser needs no neutral page") })
    }

    fn send<'a>(&'a self, _method: &'a str, _params: Value) -> BoxFuture<'a, Result<Value, DriverError>> {
        Box::pin(async { panic!("a Mac browser needs no override") })
    }
}

#[tokio::test]
async fn a_real_mac_browser_is_left_alone() {
    assert!(!present_mac_user_agent(&MacPage, Duration::ZERO).await.unwrap());
}

/// A page whose every evaluate fails, as one mid-navigation does.
struct NavigatingPage;

impl ProbePage for NavigatingPage {
    fn evaluate<'a>(&'a self, _script: &'a str) -> BoxFuture<'a, Result<Value, DriverError>> {
        Box::pin(async { Err(DriverError::Evaluation("Execution context was destroyed".to_owned())) })
    }

    fn goto<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, Result<(), DriverError>> {
        Box::pin(async { Ok(()) })
    }

    fn send<'a>(&'a self, _method: &'a str, _params: Value) -> BoxFuture<'a, Result<Value, DriverError>> {
        Box::pin(async { Ok(json!({})) })
    }
}

#[tokio::test]
async fn a_login_probe_that_lands_mid_navigation_reads_as_empty() {
    assert!(read_login_probe(&NavigatingPage).await.is_empty());
}
