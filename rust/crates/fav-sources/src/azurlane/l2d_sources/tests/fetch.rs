//! The source snapshots, the origin probe, and the health report fetch.

use std::sync::{Arc, Mutex};

use fav_runtime::BoxFuture;

use super::super::fetch::origin_probe_client_options;
use super::super::{
    HealthValidation, L2D_SU_ENGLISH_REGION, L2D_SU_PRIMARY_REGION, NAGAMI_MAPPING_URL, SourceErrorKind, fetch_azurlane_l2d_health_report,
    fetch_l2d_su_snapshot, fetch_nagami_snapshot, l2d_su_primary_ship_index_url, probe_l2d_su_origin, run_origin_probe,
};
use super::{
    FETCHED_AT, FakeFetch, clock, connect_error, english_index_url, index_payload, json, primary_index_url, respond, response_with, ship,
    ship_index_payload, skin,
};

fn javelin(costume_name: &str, ship_name: &str) -> fav_core::pyjson::Value {
    ship(1, "biaoqiang", ship_name, "", vec![skin(11, costume_name, "biaoqiang_2", "live2d")])
}

// py: tests/test_azurlane_l2d_sources.py::test_fetch_l2d_su_snapshot_merges_english_names_and_records_metadata
#[tokio::test]
async fn the_l2d_su_snapshot_merges_english_names_and_records_metadata() {
    let primary = index_payload(vec![javelin("闪耀的白刃", "标枪")]);
    let english = ship_index_payload(vec![javelin("Gleaming White Blade", "Javelin")], "EN", Vec::new());
    let client = FakeFetch::new(move |_, url| {
        if url == english_index_url() {
            return respond(200, &english);
        }
        Ok(response_with(
            200,
            &primary,
            &[("ETag", "\"abc\""), ("Last-Modified", "Thu, 14 May 2026 00:00:00 GMT")],
        ))
    });

    let snapshot = fetch_l2d_su_snapshot(&client, L2D_SU_PRIMARY_REGION, L2D_SU_ENGLISH_REGION, None, &clock)
        .await
        .unwrap();

    let urls: Vec<String> = client.requests().into_iter().map(|(_, url)| url).collect();
    assert_eq!(urls, [primary_index_url(), english_index_url()]);
    assert!(snapshot.errors.is_empty());
    assert_eq!(snapshot.metadata.url, primary_index_url());
    assert_eq!(snapshot.metadata.http_status, Some(200));
    assert_eq!(snapshot.metadata.etag, "\"abc\"");
    assert_eq!(snapshot.metadata.last_modified, "Thu, 14 May 2026 00:00:00 GMT");
    // `datetime.fromisoformat(fetched_at).tzinfo is not None`: the clock stamps it.
    assert_eq!(snapshot.metadata.fetched_at, FETCHED_AT);
    assert_eq!(snapshot.summary().get("character_count"), Some(&fav_core::pyjson::Value::Int(1)));
    let character = &snapshot.characters[0];
    assert_eq!(character.char_name, "标枪");
    assert_eq!(character.char_name_en, "Javelin");
    assert_eq!(character.live2d[0].costume_name, "闪耀的白刃");
    assert_eq!(character.live2d[0].costume_name_en, "Gleaming White Blade");
}

// py: tests/test_azurlane_l2d_sources.py::test_fetch_l2d_su_snapshot_invokes_origin_throttle_before_each_index_request
#[tokio::test]
async fn the_origin_throttle_runs_before_each_index_request() {
    let primary = index_payload(vec![javelin("闪耀的白刃", "标枪")]);
    let english = ship_index_payload(vec![javelin("Blade", "Javelin")], "EN", Vec::new());
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let handler_events = Arc::clone(&events);
    let client = FakeFetch::new(move |_, url| {
        handler_events.lock().unwrap().push(format!("GET {url}"));
        respond(200, if url == english_index_url() { &english } else { &primary })
    });
    let throttle_events = Arc::clone(&events);
    let throttle = move || -> BoxFuture<'static, ()> {
        let events = Arc::clone(&throttle_events);
        Box::pin(async move { events.lock().unwrap().push("throttle".to_owned()) })
    };

    fetch_l2d_su_snapshot(&client, L2D_SU_PRIMARY_REGION, L2D_SU_ENGLISH_REGION, Some(&throttle), &clock)
        .await
        .unwrap();

    // Each origin index request is immediately preceded by a throttle acquisition.
    assert_eq!(
        *events.lock().unwrap(),
        [
            "throttle".to_owned(),
            format!("GET {}", primary_index_url()),
            "throttle".to_owned(),
            format!("GET {}", english_index_url()),
        ]
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_fetch_l2d_su_snapshot_reports_english_region_failure_without_dropping_models
#[tokio::test]
async fn an_english_region_failure_is_reported_without_dropping_models() {
    let primary = index_payload(vec![javelin("闪耀的白刃", "标枪")]);
    let client = FakeFetch::new(move |_, url| {
        if url == english_index_url() {
            return respond(503, "");
        }
        respond(200, &primary)
    });

    let snapshot = fetch_l2d_su_snapshot(&client, L2D_SU_PRIMARY_REGION, L2D_SU_ENGLISH_REGION, None, &clock)
        .await
        .unwrap();

    assert_eq!(snapshot.summary().get("live2d_count"), Some(&fav_core::pyjson::Value::Int(1)));
    assert_eq!(snapshot.characters[0].live2d[0].costume_name_en, "");
    let errors: Vec<(SourceErrorKind, String, Option<i128>)> = snapshot
        .errors
        .iter()
        .map(|error| (error.kind, error.url.clone(), error.http_status))
        .collect();
    assert_eq!(errors, [(SourceErrorKind::Network, english_index_url(), Some(503))]);
}

// py: tests/test_azurlane_l2d_sources.py::test_fetch_nagami_snapshot_returns_network_error
#[tokio::test]
async fn a_nagami_network_failure_is_a_network_error() {
    let client = FakeFetch::new(|_, _| connect_error("boom"));

    let snapshot = fetch_nagami_snapshot(&client, NAGAMI_MAPPING_URL, &clock).await.unwrap();

    assert!(snapshot.entries.is_empty());
    assert_eq!(snapshot.errors[0].kind, SourceErrorKind::Network);
    assert_eq!(snapshot.errors[0].http_status, None);
}

// py: tests/test_azurlane_l2d_sources.py::test_fetch_l2d_su_snapshot_returns_parse_error_for_spa_html
#[tokio::test]
async fn the_spa_shell_is_a_parse_error_naming_its_content_type() {
    let client = FakeFetch::new(|_, _| {
        Ok(response_with(
            200,
            "<!doctype html><html><body><div id=\"root\"></div></body></html>",
            &[("content-type", "text/html; charset=utf-8")],
        ))
    });

    let snapshot = fetch_l2d_su_snapshot(&client, L2D_SU_PRIMARY_REGION, L2D_SU_ENGLISH_REGION, None, &clock)
        .await
        .unwrap();

    assert!(snapshot.characters.is_empty());
    assert_eq!(snapshot.errors[0].kind, SourceErrorKind::Parse);
    assert_eq!(snapshot.errors[0].http_status, Some(200));
    // The content type is what tells an operator the URL now serves a web page rather than data.
    assert!(
        snapshot.errors[0].message.ends_with("(served as text/html)"),
        "{}",
        snapshot.errors[0].message
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_fetch_nagami_snapshot_returns_schema_error
#[tokio::test]
async fn a_malformed_nagami_mapping_is_a_schema_error() {
    let body = json(&obj! {"bad" => obj! {"name" => ""}});
    let client = FakeFetch::new(move |_, _| respond(200, &body));

    let snapshot = fetch_nagami_snapshot(&client, NAGAMI_MAPPING_URL, &clock).await.unwrap();

    assert!(snapshot.entries.is_empty());
    assert_eq!(snapshot.errors[0].kind, SourceErrorKind::Schema);
    assert_eq!(snapshot.errors[0].http_status, Some(200));
}

// py: tests/test_azurlane_l2d_sources.py::test_probe_l2d_su_origin_reports_each_failure_mode
#[tokio::test]
async fn the_origin_probe_reports_each_failure_mode() {
    let url = l2d_su_primary_ship_index_url();
    // An unconfigured proxy is reported as such rather than attempted.
    assert_eq!(probe_l2d_su_origin("  ", "").await.unwrap().code, "incomplete");

    let blocked = FakeFetch::new(|_, _| connect_error("null routed"));
    let result = run_origin_probe(&blocked, &url).await;
    assert_eq!((result.ok, result.code.as_str()), (false, "unreachable"));

    let rejected = FakeFetch::new(|_, url| respond(if url.contains("/data/") { 403 } else { 200 }, "1.2.3.4"));
    let result = run_origin_probe(&rejected, &url).await;
    assert_eq!(
        (result.ok, result.code.as_str(), result.exit_ip.as_str()),
        (false, "http_error", "1.2.3.4")
    );

    let reachable = FakeFetch::new(|_, url| respond(200, if url.contains("checkip") { "203.0.113.7" } else { "" }));
    let result = run_origin_probe(&reachable, &url).await;
    assert_eq!(
        (result.ok, result.code.as_str(), result.exit_ip.as_str()),
        (true, "ok", "203.0.113.7")
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_probe_l2d_su_origin_uses_the_connect_host_fallback
#[test]
fn the_origin_probe_takes_the_crawlers_connect_host_route() {
    // The settings-page test has to take the same route the crawler does, or it reports a
    // dead origin while the crawler would get through.
    let routed = origin_probe_client_options("http://user:pass@proxy.example:8080", " static.l2d.su ");
    let connect_to = routed.connect_to.expect("a connect_to route");
    assert_eq!(connect_to.host(), "l2d.su");
    assert_eq!(connect_to.connect_host(), "static.l2d.su");
    assert_eq!(routed.proxy.as_deref(), Some("http://user:pass@proxy.example:8080"));
    assert_eq!(routed.max_keepalive_connections, 0);

    let direct = origin_probe_client_options("http://user:pass@proxy.example:8080", "");
    assert!(direct.connect_to.is_none());
}

// py: tests/test_azurlane_l2d_sources.py::test_fetch_health_report_records_source_errors_without_raising
#[tokio::test]
async fn the_health_report_records_source_errors_without_failing() {
    let client = FakeFetch::new(|_, url| {
        if url == primary_index_url() {
            return connect_error("l2d unavailable");
        }
        connect_error("nagami unavailable")
    });

    let report = fetch_azurlane_l2d_health_report(&client, HealthValidation::Skip, None, &clock)
        .await
        .unwrap();

    assert!(!report.source_health.l2d_su.ok());
    assert!(!report.source_health.nagami.ok());
    assert_eq!(report.catalog_health.entry_count, 0);
    assert_eq!(report.resource_health.broken_resource_count, 0);
}
