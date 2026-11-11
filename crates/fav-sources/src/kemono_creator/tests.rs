use std::sync::Arc;

use fav_core::settings::{Defaults as _, Settings};
use fav_db::SettingsStore;
use fav_net::{ClientOptions, HttpClient};
use serde_json::json;

use super::{KemonoCreatorResolver, ResolveError, ResolvedCreator, client_options, extract_kemono_creator};
use crate::testutil::{FakeWeb, Reply, Seen};

fn pair(service: &str, id: &str) -> (String, String) {
    (service.to_owned(), id.to_owned())
}

// py: tests/test_kemono_creator.py::test_extract_kemono_creator_accepts_urls_and_bare_ids
#[test]
fn extract_kemono_creator_accepts_urls_and_bare_ids() {
    let cases = [
        ("70050825", ("fanbox", "70050825")),
        ("  70050825  ", ("fanbox", "70050825")),
        ("https://pawchive.pw/fanbox/user/70050825", ("fanbox", "70050825")),
        ("https://pawchive.pw/fanbox/user/70050825?o=50", ("fanbox", "70050825")),
        ("https://kemono.cr/patreon/user/9069743/posts", ("patreon", "9069743")),
        ("https://pawchive.pw/Fantia/user/12345", ("fantia", "12345")),
        ("fanbox/user/70050825", ("fanbox", "70050825")),
        ("https://coomer.st/onlyfans/user/some-creator_1", ("onlyfans", "some-creator_1")),
    ];
    for (raw, (service, id)) in cases {
        assert_eq!(extract_kemono_creator(raw).unwrap(), Some(pair(service, id)), "{raw}");
    }
}

// py: tests/test_kemono_creator.py::test_extract_kemono_creator_rejects_invalid_input
#[test]
fn extract_kemono_creator_rejects_invalid_input() {
    for raw in [
        "",
        "   ",
        "0",
        "maplestar",
        "https://pawchive.pw/posts",
        "https://pawchive.pw/user/70050825",
        "https://pawchive.pw/fanbox/user/",
        "https://pawchive.pw/fanbox/user/id with spaces",
    ] {
        assert_eq!(extract_kemono_creator(raw).unwrap(), None, "{raw}");
    }
    // `urlsplit`'s own refusal is an error, which the resolver reads as invalid input.
    assert_eq!(extract_kemono_creator("http://[::1/fanbox/user/1").unwrap_err(), "Invalid IPv6 URL");
}

async fn fake_resolver(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> (KemonoCreatorResolver, FakeWeb) {
    let web = FakeWeb::start(handler).await;
    let mut settings = Settings::defaults();
    settings.web.kemono.base_url = "http://pawchive.pw/".to_owned();
    let options = ClientOptions {
        proxy: Some(web.proxy.clone()),
        ..client_options()
    };
    let resolver = KemonoCreatorResolver::with_client(Arc::new(SettingsStore::pinned(settings)), HttpClient::new(&options).unwrap());
    (resolver, web)
}

// py: tests/test_kemono_creator.py::test_resolve_fetches_name_from_profile_api
#[tokio::test]
async fn resolve_fetches_name_from_profile_api() {
    let (resolver, web) = fake_resolver(|_| Reply::json(200, &json!({"id": "70050825", "name": "Maplestar", "service": "fanbox"}))).await;
    let creator = resolver.resolve_creator("https://pawchive.pw/fanbox/user/70050825").await.unwrap();
    assert_eq!(
        creator,
        ResolvedCreator {
            service: "fanbox".to_owned(),
            id: "70050825".to_owned(),
            name: "Maplestar".to_owned(),
        }
    );
    assert_eq!(web.urls(), ["http://pawchive.pw/api/v1/fanbox/user/70050825/profile"]);
    let seen = &web.seen()[0];
    assert_eq!(seen.header("accept"), Some("application/json"));
    assert_eq!(seen.header("user-agent"), Some(super::USER_AGENT));
}

// py: tests/test_kemono_creator.py::test_resolve_falls_back_to_id_when_name_is_blank
#[tokio::test]
async fn resolve_falls_back_to_id_when_name_is_blank() {
    let (resolver, _web) = fake_resolver(|_| Reply::json(200, &json!({"name": "  "}))).await;
    assert_eq!(resolver.resolve_creator("70050825").await.unwrap().name, "70050825");
}

// py: tests/test_kemono_creator.py::test_resolve_rejects_unparseable_input
#[tokio::test]
async fn resolve_rejects_unparseable_input() {
    let (resolver, web) = fake_resolver(|_| Reply::json(200, &json!({}))).await;
    assert_eq!(resolver.resolve_creator("not a creator").await, Err(ResolveError::Invalid));
    assert!(web.urls().is_empty());
}

// py: tests/test_kemono_creator.py::test_resolve_maps_missing_creator_to_lookup_error
#[tokio::test]
async fn resolve_maps_missing_creator_to_lookup_error() {
    let (resolver, _web) = fake_resolver(|_| Reply::status(404)).await;
    assert_eq!(resolver.resolve_creator("70050825").await, Err(ResolveError::NotFound));
}

// py: tests/test_kemono_creator.py::test_resolve_maps_upstream_failures_to_connection_error
#[tokio::test]
async fn resolve_maps_upstream_failures_to_connection_error() {
    let (resolver, _web) = fake_resolver(|_| Reply::status(403)).await;
    assert_eq!(resolver.resolve_creator("70050825").await, Err(ResolveError::Unreachable));
    let (resolver, _web) = fake_resolver(|_| Reply::bytes(200, "<html>not json</html>")).await;
    assert_eq!(resolver.resolve_creator("70050825").await, Err(ResolveError::Unreachable));
}

// py: tests/test_kemono_creator.py::test_resolve_maps_transport_errors_to_connection_error
#[tokio::test]
async fn resolve_maps_transport_errors_to_connection_error() {
    let (resolver, _web) = fake_resolver(|_| Reply::Close).await;
    assert_eq!(resolver.resolve_creator("70050825").await, Err(ResolveError::Unreachable));
}
