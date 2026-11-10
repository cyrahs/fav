use std::sync::{Arc, Mutex};

use rcgen::{CertificateParams, Issuer, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::{ClientOptions, HttpClient};

const HOST: &str = "l2d.su";

type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// Reads one request head; returns its request target and `Host` header.
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Option<(String, String)> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let target = text.split(' ').nth(1).unwrap_or_default().to_owned();
    let host = text
        .lines()
        .find_map(|line| line.strip_prefix("host: ").or_else(|| line.strip_prefix("Host: ")))
        .unwrap_or_default()
        .to_owned();
    Some((target, host))
}

async fn answer<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, seen: Seen) {
    if let Some(request) = read_head(&mut stream).await {
        seen.lock().unwrap().push(request);
        let body = br#"{"ships": []}"#;
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(body).await;
        let _ = stream.shutdown().await;
    }
}

/// A plain HTTP server that records the target and `Host` of each request.
async fn http_server() -> (u16, Seen) {
    http_server_on("127.0.0.1").await
}

/// [`http_server`] on another loopback address.
async fn http_server_on(address: &str) -> (u16, Seen) {
    let listener = TcpListener::bind((address, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Seen::default();
    let record = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(answer(stream, Arc::clone(&record)));
        }
    });
    (port, seen)
}

fn options(connect_to: ConnectTo, proxy: Option<String>) -> ClientOptions {
    ClientOptions {
        timeout: Duration::from_secs(10),
        proxy,
        connect_to: Some(connect_to),
        ..ClientOptions::default()
    }
}

// py: tests/test_connect_to.py::test_requests_move_to_the_connect_host_while_the_host_does_not_resolve
#[tokio::test]
async fn requests_move_to_the_connect_host_while_the_host_does_not_resolve() {
    let (port, seen) = http_server().await;
    // Elsewhere than the substitute, so a request wrongly moved to it finds no server.
    let (other_port, other_seen) = http_server_on("127.0.0.2").await;
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&lookups);
    let config = ConnectTo::new(HOST, "127.0.0.1").with_resolves(move |host| {
        record.lock().unwrap().push(host.to_owned());
        false
    });
    let client = HttpClient::new(&options(config, None)).unwrap();
    client.get(&format!("http://{HOST}:{port}/data/ships-CN.json")).await.unwrap();
    client.get(&format!("http://{HOST}:{port}/data/ships/CN/10000.json")).await.unwrap();
    client.get(&format!("http://127.0.0.2:{other_port}/")).await.unwrap();

    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter().map(|(target, _)| target.as_str()).collect::<Vec<_>>(),
        ["/data/ships-CN.json", "/data/ships/CN/10000.json"]
    );
    // The origin still has to see its own name, or the CDN cannot route the request.
    let host = format!("{HOST}:{port}");
    assert_eq!(
        seen.iter().map(|(_, host)| host.as_str()).collect::<Vec<_>>(),
        [host.as_str(), host.as_str()]
    );
    assert_eq!(other_seen.lock().unwrap().len(), 1);
    // One lookup per client: the crawler opens a client per run.
    assert_eq!(*lookups.lock().unwrap(), [HOST]);
}

// py: tests/test_connect_to.py::test_requests_are_untouched_once_the_host_resolves_again
#[tokio::test]
async fn requests_are_untouched_once_the_host_resolves_again() {
    let (port, seen) = http_server().await;
    // `localhost` resolves, so its requests keep going wherever DNS sends them.
    let config = ConnectTo::new("localhost", "does-not-exist.invalid").with_resolves(|_| true);
    let client = HttpClient::new(&options(config, None)).unwrap();
    let response = client.get(&format!("http://localhost:{port}/data/ships-CN.json")).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(seen.lock().unwrap()[0].1, format!("localhost:{port}"));
}

// py: tests/test_connect_to.py::test_the_async_transport_moves_requests_the_same_way
#[tokio::test]
async fn the_async_transport_moves_requests_the_same_way() {
    // One client serves both of Python's transports.
    let (port, seen) = http_server().await;
    let config = ConnectTo::new(HOST, "127.0.0.1").with_resolves(|_| false);
    let client = HttpClient::new(&options(config, None)).unwrap();
    client.get(&format!("http://{HOST}:{port}/data/ships-CN.json")).await.unwrap();

    assert_eq!(
        seen.lock().unwrap()[0],
        ("/data/ships-CN.json".to_owned(), format!("{HOST}:{port}"))
    );
}

// py: tests/test_connect_to.py::test_the_response_keeps_the_url_that_was_asked_for
#[tokio::test]
async fn the_response_keeps_the_url_that_was_asked_for() {
    let (port, _seen) = http_server().await;
    let config = ConnectTo::new(HOST, "127.0.0.1").with_resolves(|_| false);
    let client = HttpClient::new(&options(config, None)).unwrap();
    let url = format!("http://{HOST}:{port}/data/ships-CN.json");
    let response = client.get(&url).await.unwrap();

    assert_eq!(response.url().as_str(), url);
    assert_eq!(&response.bytes().await.unwrap()[..], br#"{"ships": []}"#);
}

// py: tests/test_connect_to.py::test_host_resolves_reports_a_missing_record_as_false
#[test]
fn host_resolves_reports_a_missing_record_as_false() {
    assert!(!host_resolves("does-not-exist.invalid"));
}

// ---- Real TLS: a server holding a certificate only for l2d.su, reached by another address ----

struct Tls {
    roots: RootCertStore,
    server: Arc<rustls::ServerConfig>,
}

fn tls() -> Tls {
    crate::install_crypto_provider();
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "fav test CA");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![HOST.to_owned()])
        .unwrap()
        .signed_by(&key, &Issuer::from_params(&ca_params, &ca_key))
        .unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(ca.der().to_vec())).unwrap();
    let server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(leaf.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
    Tls {
        roots,
        server: Arc::new(server),
    }
}

/// An HTTPS server that records the SNI of each handshake and the target and `Host` of each
/// request it serves.
async fn https_server(config: Arc<rustls::ServerConfig>) -> (u16, Arc<Mutex<Vec<String>>>, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let sni = Arc::new(Mutex::new(Vec::new()));
    let seen = Seen::default();
    let (record_sni, record) = (Arc::clone(&sni), Arc::clone(&seen));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (config, sni, seen) = (Arc::clone(&config), Arc::clone(&record_sni), Arc::clone(&record));
            tokio::spawn(async move {
                let acceptor = tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream);
                let Ok(start) = acceptor.await else { return };
                sni.lock()
                    .unwrap()
                    .push(start.client_hello().server_name().unwrap_or_default().to_owned());
                if let Ok(stream) = start.into_stream(config).await {
                    answer(stream, seen).await;
                }
            });
        }
    });
    (port, sni, seen)
}

/// A plain HTTP CONNECT proxy that records the tunnel targets it was asked for.
async fn connect_proxy() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let targets = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&targets);
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let record = Arc::clone(&record);
            tokio::spawn(async move {
                let Some((target, _)) = read_head(&mut client).await else { return };
                record.lock().unwrap().push(target.clone());
                let Ok(mut upstream) = TcpStream::connect(target).await else {
                    return;
                };
                let _ = client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await;
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
    (url, targets)
}

async fn tls_is_verified_against_the_original_host(through_proxy: bool) {
    let tls = tls();
    let (port, sni, seen) = https_server(Arc::clone(&tls.server)).await;
    let (proxy, targets) = connect_proxy().await;
    // The certificate names only l2d.su, so connecting to 127.0.0.1 succeeds only when the
    // handshake is made, and verified, as l2d.su.
    let config = ConnectTo::new(HOST, "127.0.0.1").with_resolves(|_| false).with_roots(tls.roots);
    let client = HttpClient::new(&options(config, through_proxy.then_some(proxy))).unwrap();
    let response = client.get(&format!("https://{HOST}:{port}/data/ships-CN.json")).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(&response.bytes().await.unwrap()[..], br#"{"ships": []}"#);
    assert_eq!(*sni.lock().unwrap(), [HOST]);
    assert_eq!(seen.lock().unwrap()[0].1, format!("{HOST}:{port}"));
    let expected: Vec<String> = if through_proxy { vec![format!("127.0.0.1:{port}")] } else { vec![] };
    assert_eq!(*targets.lock().unwrap(), expected);
}

// py: tests/test_connect_to.py::test_tls_is_verified_against_the_original_host
#[tokio::test]
async fn tls_is_verified_against_the_original_host_direct() {
    tls_is_verified_against_the_original_host(false).await;
}

// py: tests/test_connect_to.py::test_tls_is_verified_against_the_original_host
#[tokio::test]
async fn tls_is_verified_against_the_original_host_proxy() {
    tls_is_verified_against_the_original_host(true).await;
}

// py: tests/test_connect_to.py::test_the_async_transport_verifies_tls_against_the_original_host_through_a_proxy
#[tokio::test]
async fn the_async_transport_verifies_tls_against_the_original_host_through_a_proxy() {
    // One client serves both of Python's transports; this is the proxied case once more,
    // with a second request on the same client reusing the decision.
    let tls = tls();
    let (port, sni, _seen) = https_server(Arc::clone(&tls.server)).await;
    let (proxy, targets) = connect_proxy().await;
    let config = ConnectTo::new(HOST, "127.0.0.1").with_resolves(|_| false).with_roots(tls.roots);
    let client = HttpClient::new(&options(config, Some(proxy))).unwrap();
    let response = client.get(&format!("https://{HOST}:{port}/data/ships-CN.json")).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(*sni.lock().unwrap(), [HOST]);
    assert_eq!(*targets.lock().unwrap(), [format!("127.0.0.1:{port}")]);
}

#[tokio::test]
async fn https_requests_for_the_substitute_itself_present_the_host_name() {
    // `_ServerNameContext` swaps the server name on every handshake made to the substitute,
    // so the origin's own URLs on it (static.l2d.su) are verified as l2d.su too, even while
    // l2d.su resolves. `Host` stays the substitute's.
    let tls = tls();
    let (port, sni, seen) = https_server(Arc::clone(&tls.server)).await;
    let config = ConnectTo::new(HOST, "127.0.0.1").with_resolves(|_| true).with_roots(tls.roots);
    let client = HttpClient::new(&options(config, None)).unwrap();
    let response = client
        .get(&format!("https://127.0.0.1:{port}/al/data/ships-CN.json"))
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(*sni.lock().unwrap(), [HOST]);
    assert_eq!(
        seen.lock().unwrap()[0],
        ("/al/data/ships-CN.json".to_owned(), format!("127.0.0.1:{port}"))
    );
}

// py: tests/test_connect_to.py::test_a_certificate_for_another_name_is_still_rejected
#[tokio::test]
async fn a_certificate_for_another_name_is_still_rejected() {
    // Pointing a different host at the server must fail verification: the swap only ever
    // presents the configured host's name, it does not switch checking off.
    let tls = tls();
    let (port, _sni, _seen) = https_server(Arc::clone(&tls.server)).await;
    let config = ConnectTo::new("other.example", "127.0.0.1")
        .with_resolves(|_| false)
        .with_roots(tls.roots);
    let client = HttpClient::new(&options(config, None)).unwrap();
    let error = client.get(&format!("https://other.example:{port}/")).await.unwrap_err();

    assert_eq!(error.class, ErrorClass::ConnectError);
    assert!(error.message.contains("certificate"), "{}", error.message);
}

#[tokio::test]
async fn a_refused_tunnel_is_a_proxy_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://user:secret@127.0.0.1:{}", listener.local_addr().unwrap().port());
    let authorization = Arc::new(Mutex::new(String::new()));
    let record = Arc::clone(&authorization);
    tokio::spawn(async move {
        let (mut client, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") && client.read(&mut byte).await.unwrap() == 1 {
            head.push(byte[0]);
        }
        *record.lock().unwrap() = String::from_utf8_lossy(&head).into_owned();
        let _ = client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await;
    });
    let config = ConnectTo::new(HOST, "127.0.0.1").with_resolves(|_| false);
    let client = HttpClient::new(&options(config, Some(proxy))).unwrap();
    let error = client.get("https://l2d.su/data/ships-CN.json").await.unwrap_err();

    assert_eq!(error.class, ErrorClass::ProxyError);
    assert_eq!(error.message, "403 Forbidden");
    let head = authorization.lock().unwrap().clone();
    assert!(head.starts_with("CONNECT 127.0.0.1:443 HTTP/1.1\r\n"), "{head}");
    assert!(head.contains("Proxy-Authorization: Basic dXNlcjpzZWNyZXQ=\r\n"), "{head}");
}
