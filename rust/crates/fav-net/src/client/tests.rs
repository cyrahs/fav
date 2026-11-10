//! The client against a local server: redirects, history, status errors, transport errors.

use std::net::SocketAddr;

use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::{ClientOptions, HttpClient, Request};
use crate::ErrorClass;

async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    address
}

fn redirect(status: StatusCode, location: &str, extra: &[(&'static str, &'static str)]) -> Response {
    let mut response = (status, [(header::LOCATION, location.to_owned())]).into_response();
    for (name, value) in extra {
        response.headers_mut().insert(*name, value.parse().expect("value"));
    }
    response
}

fn following() -> HttpClient {
    HttpClient::new(&ClientOptions {
        follow_redirects: true,
        headers: vec![("User-Agent".to_owned(), "fav-test".to_owned())],
        ..ClientOptions::default()
    })
    .expect("client")
}

#[tokio::test]
async fn redirects_are_followed_and_kept_in_history() {
    let router = Router::new()
        .route(
            "/a",
            get(|| async { redirect(StatusCode::MOVED_PERMANENTLY, "/b#101", &[("x-image-errno", "101")]) }),
        )
        .route("/b", get(|| async { redirect(StatusCode::FOUND, "//127.0.0.1:1/never", &[]) }))
        .route(
            "/ua",
            get(|headers: HeaderMap| async move {
                headers
                    .get("user-agent")
                    .map(|v| v.to_str().unwrap_or_default().to_owned())
                    .unwrap_or_default()
            }),
        );
    let address = serve(router.clone()).await;
    let client = following();

    let response = client.get(&format!("http://{address}/ua")).await.expect("response");
    assert_eq!(response.bytes().await.expect("body"), "fav-test");

    let no_follow = HttpClient::new(&ClientOptions::default()).expect("client");
    let response = no_follow.get(&format!("http://{address}/a")).await.expect("response");
    assert_eq!(response.status(), 301);
    assert!(response.history().is_empty());
    let error = response.error_for_status().expect_err("301 is not a success");
    assert_eq!(
        error.to_string(),
        format!(
            "Redirect response '301 Moved Permanently' for url 'http://{address}/a'\nRedirect location: '/b#101'\nFor more information check: https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/301"
        )
    );

    // The second hop points at a closed port: the failure names the host, not the URL.
    let error = client.get(&format!("http://{address}/a")).await.expect_err("refused");
    assert_eq!(error.class, ErrorClass::ConnectError);
    assert_eq!(error.host, "127.0.0.1");
    assert!(!error.message.contains("never"), "{}", error.message);
}

#[tokio::test]
async fn history_and_final_url_are_reported() {
    let router = Router::new()
        .route(
            "/start",
            get(|| async {
                redirect(
                    StatusCode::MOVED_PERMANENTLY,
                    "/images/default_x.gif#101",
                    &[("x-image-errno", "101")],
                )
            }),
        )
        .route("/images/default_x.gif", get(|| async { "GIF89a" }))
        .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
        .route("/loop", get(|| async { redirect(StatusCode::FOUND, "/loop", &[]) }));
    let address = serve(router).await;
    let client = following();

    let response = client.get(&format!("http://{address}/start")).await.expect("response");
    assert_eq!(response.status(), 200);
    assert_eq!(response.url().as_str(), format!("http://{address}/images/default_x.gif#101"));
    assert_eq!(response.history().len(), 1);
    assert_eq!(response.history()[0].status, 301);
    assert_eq!(response.history()[0].headers.get("x-image-errno").expect("errno"), "101");

    let response = client.get(&format!("http://{address}/missing")).await.expect("response");
    let error = response.error_for_status().expect_err("404");
    assert_eq!(
        error.to_string(),
        format!(
            "Client error '404 Not Found' for url 'http://{address}/missing'\nFor more information check: https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/404"
        )
    );

    let error = client.get(&format!("http://{address}/loop")).await.expect_err("loops");
    assert_eq!(error.class, ErrorClass::TooManyRedirects);
    assert_eq!(error.message, "Exceeded maximum allowed redirects.");
}

#[tokio::test]
async fn a_post_redirected_by_302_becomes_a_get() {
    let router = Router::new()
        .route("/form", axum::routing::post(|| async { redirect(StatusCode::FOUND, "/done", &[]) }))
        .route("/done", get(|| async { "got" }));
    let address = serve(router).await;
    let response = following()
        .send(Request::post(format!("http://{address}/form"), b"data=1".to_vec()))
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.expect("body"), "got");
}

/// Set in the child process of [`trust_env_false_ignores_the_environment_proxy`].
const TRUST_ENV_CHILD: &str = "FAV_NET_TRUST_ENV_CHILD";
const PROXY_VARIABLES: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// The half of the test that runs with `HTTP_PROXY` pointing at the parent's proxy: a
/// client that trusts the environment reaches a host through it, one with
/// `trust_env: false` goes direct (and finds no such host). Outside the child it does
/// nothing.
#[tokio::test]
async fn trust_env_child() {
    if std::env::var_os(TRUST_ENV_CHILD).is_none() {
        return;
    }
    let client = |trust_env: bool| {
        HttpClient::new(&ClientOptions {
            trust_env,
            ..ClientOptions::default()
        })
        .expect("client")
    };
    let proxied = client(true).get("http://fav-proxy-test.invalid/").await.expect("through the proxy");
    assert_eq!(proxied.bytes().await.expect("body").as_ref(), b"proxied");
    let direct = client(false).get("http://fav-proxy-test.invalid/").await;
    assert!(direct.is_err(), "a client that ignores the environment went through the proxy");
}

#[tokio::test]
async fn trust_env_false_ignores_the_environment_proxy() {
    if std::env::var_os(TRUST_ENV_CHILD).is_some() {
        return;
    }
    let proxy = serve(Router::new().fallback(|| async { "proxied" })).await;
    let child = tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
        command.args(["--exact", "client::tests::trust_env_child", "--quiet"]);
        for name in PROXY_VARIABLES {
            command.env_remove(name);
        }
        command.env("HTTP_PROXY", format!("http://{proxy}")).env(TRUST_ENV_CHILD, "1");
        command.output().expect("child runs")
    })
    .await
    .expect("joined");
    let output = String::from_utf8_lossy(&child.stdout).into_owned() + &String::from_utf8_lossy(&child.stderr);
    assert!(child.status.success(), "{output}");
    assert!(output.contains("1 passed"), "{output}");
}
