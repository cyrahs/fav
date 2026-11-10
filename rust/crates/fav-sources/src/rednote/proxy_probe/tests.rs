use std::sync::Mutex;
use std::time::Duration;

use fav_core::golden;
use fav_core::pyjson::Value;
use fav_net::{ErrorClass, TransportError};
use fav_runtime::BoxFuture;

use super::*;
use crate::azurlane::l2d_sources::fetch::FetchedResponse;

type Handler = Box<dyn Fn(&str) -> Result<FetchedResponse, TransportError> + Send + Sync>;

/// `httpx.MockTransport(handler)`: answers by URL and records every URL asked for.
struct FakeFetch {
    handler: Handler,
    dialled: Mutex<Vec<String>>,
}

impl FakeFetch {
    fn new(handler: impl Fn(&str) -> Result<FetchedResponse, TransportError> + Send + Sync + 'static) -> Self {
        Self {
            handler: Box::new(handler),
            dialled: Mutex::new(Vec::new()),
        }
    }

    fn dialled(&self) -> Vec<String> {
        self.dialled.lock().expect("the log").clone()
    }
}

impl Fetch for FakeFetch {
    fn request<'a>(
        &'a self,
        _method: &'a str,
        url: &'a str,
        _timeout: Option<Duration>,
    ) -> BoxFuture<'a, Result<FetchedResponse, TransportError>> {
        self.dialled.lock().expect("the log").push(url.to_owned());
        let reply = (self.handler)(url);
        Box::pin(async move { reply })
    }
}

#[allow(clippy::unnecessary_wraps)] // reason: the fake's handlers answer a `Result`
fn response(status: u16, text: &str) -> Result<FetchedResponse, TransportError> {
    Ok(FetchedResponse {
        status,
        url: String::new(),
        headers: Vec::new(),
        content: text.as_bytes().to_vec(),
    })
}

fn failure(class: ErrorClass, message: &str) -> Result<FetchedResponse, TransportError> {
    Err(TransportError {
        class,
        host: String::new(),
        message: message.to_owned(),
    })
}

async fn probe(fetch: &FakeFetch, proxy: &str) -> ProxyProbe {
    probe_proxy_through(proxy, fetch).await.expect("a proxy string urlsplit can read")
}

const PROXY: &str = "http://user:pw@home.example:3128";

// py: tests/test_rednote.py::test_the_proxy_probe_names_each_way_an_egress_can_be_wrong
#[tokio::test]
async fn the_proxy_probe_names_each_way_an_egress_can_be_wrong() {
    let refused = FakeFetch::new(|_| failure(ErrorClass::ProxyError, "refused"));
    let result = probe(&refused, PROXY).await;
    assert_eq!((result.ok, result.code.as_str()), (false, "proxy_error"));

    // The captcha wall, which is the answer this button exists to catch early.
    let walled = FakeFetch::new(|url| {
        if url.contains("checkip") {
            response(200, "198.51.100.9")
        } else {
            response(461, "")
        }
    });
    let result = probe(&walled, PROXY).await;
    assert_eq!(
        (result.ok, result.code.as_str(), result.exit_ip.as_str()),
        (false, "risk_control", "198.51.100.9")
    );

    let reachable = FakeFetch::new(|url| response(200, if url.contains("checkip") { "203.0.113.7" } else { "<html></html>" }));
    let result = probe(&reachable, PROXY).await;
    assert_eq!(
        (result.ok, result.code.as_str(), result.exit_ip.as_str(), result.direct),
        (true, "ok", "203.0.113.7", false)
    );
    assert_eq!(result.message, "Reached https://www.xiaohongshu.com through the proxy.");
    assert_eq!(
        reachable.dialled(),
        ["https://checkip.amazonaws.com", "https://www.xiaohongshu.com"]
    );

    let refusing_site = FakeFetch::new(|url| {
        if url.contains("checkip") {
            response(200, " 198.51.100.9\n")
        } else {
            response(503, "")
        }
    });
    let result = probe(&refusing_site, PROXY).await;
    assert_eq!(
        (result.code.as_str(), result.message.as_str(), result.exit_ip.as_str()),
        ("http_error", "https://www.xiaohongshu.com returned HTTP 503.", "198.51.100.9")
    );
}

// py: tests/test_rednote.py::test_a_dead_proxy_is_told_apart_from_a_site_that_will_not_answer_it
#[tokio::test]
async fn a_dead_proxy_is_told_apart_from_a_site_that_will_not_answer_it() {
    let nothing_gets_through = FakeFetch::new(|_| failure(ErrorClass::ConnectError, "no route to proxy"));
    let dead_proxy = probe(&nothing_gets_through, PROXY).await;
    assert_eq!((dead_proxy.ok, dead_proxy.code.as_str()), (false, "proxy_error"));
    assert!(dead_proxy.message.contains("proxy"), "{}", dead_proxy.message);

    let site_only_is_null_routed = FakeFetch::new(|url| {
        if url.contains("checkip") {
            response(200, "198.51.100.9")
        } else {
            failure(ErrorClass::ConnectTimeout, "timed out")
        }
    });
    let walled_exit = probe(&site_only_is_null_routed, PROXY).await;
    assert_eq!(
        (walled_exit.ok, walled_exit.code.as_str(), walled_exit.exit_ip.as_str()),
        (false, "unreachable", "198.51.100.9")
    );
    assert_eq!(walled_exit.message, "Could not reach https://www.xiaohongshu.com: timed out");
}

// py: tests/test_rednote.py::test_the_proxy_probe_reports_a_bad_proxy_string_without_dialing_it
#[tokio::test]
async fn the_proxy_probe_reports_a_bad_proxy_string_without_dialing_it() {
    // Chromium would connect as nobody rather than fail, so this has to be caught here.
    let fetch = FakeFetch::new(|_| response(200, ""));
    let result = probe(&fetch, "socks5://user:pw@home.example:1080").await;
    assert_eq!((result.ok, result.code.as_str()), (false, "invalid"));
    assert!(fetch.dialled().is_empty());
    assert!(result.message.contains("SOCKS"), "{}", result.message);
}

// py: tests/test_rednote.py::test_an_empty_proxy_probes_the_direct_egress_rather_than_refusing
#[tokio::test]
async fn an_empty_proxy_probes_the_direct_egress_rather_than_refusing() {
    let reachable = FakeFetch::new(|url| response(200, if url.contains("checkip") { "203.0.113.7" } else { "" }));
    let result = probe(&reachable, "   ").await;
    assert_eq!((result.ok, result.direct, result.exit_ip.as_str()), (true, true, "203.0.113.7"));
    assert_eq!(result.message, "Reached https://www.xiaohongshu.com directly.");

    // Direct, a failure on the site is the site's: there is no proxy to blame.
    let unreachable = FakeFetch::new(|_| failure(ErrorClass::ConnectError, "no route"));
    let result = probe(&unreachable, "").await;
    assert_eq!((result.code.as_str(), result.direct), ("unreachable", true));
}

// py: tests/test_rednote.py::test_an_unreachable_exit_ip_service_does_not_fail_the_probe
#[tokio::test]
async fn an_unreachable_exit_ip_service_does_not_fail_the_probe() {
    let site_only = FakeFetch::new(|url| {
        if url.contains("checkip") {
            failure(ErrorClass::ConnectError, "no route")
        } else {
            response(200, "")
        }
    });
    let result = probe(&site_only, PROXY).await;
    assert_eq!((result.ok, result.exit_ip.as_str()), (true, ""));

    let erroring = FakeFetch::new(|url| {
        if url.contains("checkip") {
            response(502, "Bad gateway")
        } else {
            response(200, "")
        }
    });
    assert_eq!(probe(&erroring, PROXY).await.exit_ip, "");
}

#[tokio::test]
async fn proxy_strings_are_refused_as_python_refuses_them() {
    let document = golden::load("rednote/proxy_probe.json");
    for case in golden::cases(&document) {
        let proxy = golden::string(golden::get(case, "proxy"));
        let got = probe_proxy(&proxy).await;
        if let Some(Value::Dict(expected)) = case.as_dict().and_then(|case| case.get("result")) {
            let got = got.unwrap_or_else(|error| panic!("{proxy}: {error}"));
            let field = |name: &str| golden::string(expected.get(name).expect(name));
            assert_eq!(got.code, field("code"), "{proxy}");
            assert_eq!(got.message, field("message"), "{proxy}");
            assert!(!got.ok && got.exit_ip.is_empty() && !got.direct, "{proxy}");
        } else {
            let error = golden::get(case, "error");
            assert_eq!(golden::string(golden::get(error, "class")), "ValueError", "{proxy}");
            assert_eq!(got, Err(golden::string(golden::get(error, "message"))), "{proxy}");
        }
    }
}

#[tokio::test]
async fn a_proxy_nothing_listens_on_is_reported_as_the_proxy() {
    // Loopback only: the connection to the proxy fails, so nothing leaves the machine.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let port = listener.local_addr().expect("its address").port();
    drop(listener);
    let result = probe_proxy(&format!("127.0.0.1:{port}")).await.expect("a usable proxy string");
    assert_eq!((result.ok, result.code.as_str(), result.direct), (false, "proxy_error", false));
    assert!(result.exit_ip.is_empty());
}

#[test]
fn httpx_masks_the_password_and_drops_a_default_port() {
    assert_eq!(
        httpx_url_repr("ftp://u:p@Host.Example:21/x"),
        "URL('ftp://u:[secure]@host.example/x')"
    );
    assert_eq!(httpx_url_repr("socks4://[::1]:1080"), "URL('socks4://[::1]:1080')");
    assert_eq!(check_httpx_proxy("socks5h://h:1"), Ok(()));
    assert_eq!(
        check_httpx_proxy("host:3128"),
        Err("Unknown scheme for proxy URL URL('host:3128')".to_owned())
    );
}
