//! The clients, the origin pacing and the rate limiter.

use std::sync::Arc;
use std::time::{Duration, Instant};

use fav_core::settings::Settings;
use fav_net::retry::{RecordingSleeper, tokio_sleeper};

use super::super::{
    ASSET_CONNECT_TIMEOUT, ASSET_READ_TIMEOUT, Clients, RateLimiter, asset_client_options, crawler_from_settings, origin_client_options,
};
use super::{Harness, Web, common_reply, javelin_catalog, live2d_url, model3_payload, ok, test_pacing, unused_db};

const ORIGIN_PROXY: &str = "http://user:pass@proxy.example:8080";

// py: tests/test_azurlane.py::test_azurlane_asset_client_fails_fast_on_a_stalled_connection
#[test]
fn azurlane_asset_client_fails_fast_on_a_stalled_connection() {
    // Models are archived one at a time, so a single asset waiting out the read timeout idles
    // every other slot; a handshake that never completes is the other half of the same stall.
    let options = asset_client_options();
    assert_eq!(options.timeout, ASSET_READ_TIMEOUT);
    assert!(options.timeout < Duration::from_mins(1));
    assert_eq!(options.connect_timeout, Some(ASSET_CONNECT_TIMEOUT));
    assert!(ASSET_CONNECT_TIMEOUT < Duration::from_secs(20));
}

// py: tests/test_azurlane.py::test_azurlane_origin_client_disables_connection_reuse
#[test]
fn azurlane_origin_client_disables_connection_reuse() {
    // An HTTPS request through a proxy lives in a CONNECT tunnel pinned to one exit IP, so a
    // reused connection would push the whole backfill through a single address.
    let origin = origin_client_options(ORIGIN_PROXY, "static.l2d.su");
    assert_eq!(origin.max_keepalive_connections, 0);
    assert_eq!(origin.proxy.as_deref(), Some(ORIGIN_PROXY));
    assert!(asset_client_options().max_keepalive_connections > 0);
    let clients = Clients::build(ORIGIN_PROXY, "static.l2d.su").expect("clients");
    assert!(clients.origin.is_some());
    assert!(clients.origin_source.is_some());
}

// py: tests/test_azurlane.py::test_azurlane_origin_client_falls_back_to_the_configured_connect_host
#[test]
fn azurlane_origin_client_falls_back_to_the_configured_connect_host() {
    let origin = origin_client_options(ORIGIN_PROXY, "static.l2d.su");
    let connect_to = origin.connect_to.expect("a connect-to transport");
    assert_eq!((connect_to.host(), connect_to.connect_host()), ("l2d.su", "static.l2d.su"));
}

// py: tests/test_azurlane.py::test_azurlane_origin_client_has_no_fallback_when_the_connect_host_is_empty
#[test]
fn azurlane_origin_client_has_no_fallback_when_the_connect_host_is_empty() {
    let origin = origin_client_options(ORIGIN_PROXY, "");
    assert!(origin.connect_to.is_none());
    assert_eq!(origin.max_keepalive_connections, 0);
}

// py: tests/test_azurlane.py::test_azurlane_origin_client_is_the_direct_client_when_no_proxy_is_configured
#[test]
fn azurlane_origin_client_is_the_direct_client_when_no_proxy_is_configured() {
    let clients = Clients::build("", "static.l2d.su").expect("clients");
    // Without its own clients the run sends the ship data through the direct ones.
    assert!(clients.origin.is_none());
    assert!(clients.origin_source.is_none());
}

// py: tests/test_azurlane.py::test_azurlane_reads_the_origin_request_interval_from_settings
#[tokio::test]
async fn azurlane_reads_the_origin_request_interval_from_settings() {
    let configured_interval = 7.5;
    let mut settings = Settings::default();
    settings.web.azurlane.origin_request_interval_seconds = configured_interval;
    let crawler = crawler_from_settings(&settings.web.azurlane, unused_db()).expect("crawler");
    // One limiter shared by every origin request, index and detail: the pacing is global.
    assert!((crawler.pacing.origin_request_interval_seconds - configured_interval).abs() < f64::EPSILON);
    assert!((crawler.origin_limiter.interval - configured_interval).abs() < f64::EPSILON);
}

// py: tests/test_azurlane.py::test_azurlane_index_requests_pass_through_the_origin_throttle
#[tokio::test]
async fn azurlane_index_requests_pass_through_the_origin_throttle() {
    // Python counts the throttle's waits; here the origin interval is on and the recorded
    // sleeps show them: the first wait passes, the second (EN index) waits a full interval,
    // and the nagami mapping (CDN) never waits.
    let harness = Harness::new().await;
    let catalog = javelin_catalog();
    harness.seed_ship_detail(&catalog, None).await;
    let answer = catalog.clone();
    let web = Web::start(move |_, url| {
        if let Some(reply) = common_reply(&answer, url) {
            return reply;
        }
        if url == live2d_url("javelin") {
            return ok(model3_payload(), "application/json");
        }
        ok("bytes", "application/octet-stream")
    })
    .await;
    let sleeper = RecordingSleeper::new();
    let interval = 600.0;
    let crawler = harness
        .crawler_with(
            web.clients(),
            super::super::Pacing {
                origin_request_interval_seconds: interval,
                ..test_pacing()
            },
        )
        .with_sleeper(sleeper.sleeper());
    crawler.update().await.expect("update");
    let origin_waits: Vec<f64> = sleeper.delays().into_iter().filter(|delay| *delay > interval / 2.0).collect();
    assert_eq!(origin_waits.len(), 1, "{:?}", sleeper.delays());
}

// py: tests/test_azurlane.py::test_source_rate_limiter_spaces_successive_requests
#[tokio::test]
async fn source_rate_limiter_spaces_successive_requests() {
    let interval = 0.05;
    let limiter = RateLimiter::new(interval, 0.0);
    let sleeper = tokio_sleeper();
    let uniform: super::super::Uniform = Arc::new(|_| 0.0);
    let start = Instant::now();
    limiter.wait(&sleeper, &uniform).await; // the first call returns at once
    limiter.wait(&sleeper, &uniform).await; // the second waits one interval
    assert!(start.elapsed().as_secs_f64() >= interval);
}
