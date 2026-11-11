//! `curl --connect-to` for fav's HTTP client: reach a host whose own DNS record is gone.
//!
//! Python origin: `src/tool/connect_to.py` (`ConnectToTransport`, `AsyncConnectToTransport`,
//! `host_resolves`, `_ServerNameContext`).
//!
//! When a site behind a CDN loses its DNS record, the CDN usually keeps serving it: the edge
//! still routes by TLS SNI and `Host`, so a connection opened to another address of the same
//! zone (a sibling hostname that still resolves) reaches it fine. Only the connection target
//! changes; the TLS handshake, the certificate check and the `Host` header stay those of the
//! original host.
//!
//! Resolving the name to an address locally (`curl --resolve`) cannot do that through an HTTP
//! proxy, whose CONNECT target is a hostname the proxy resolves itself. So a request for the
//! host is sent to the substitute (the TCP connection, or the proxy's CONNECT, or the forward
//! proxy's request line names it), while the TLS server name stays the original host and the
//! `Host` header keeps the original authority. reqwest's own connector has no hook for that
//! (its TLS server name is always the URL's host), so these requests take a small client of
//! their own: hyper-util's pooled HTTP/1.1 client over the connector below, with the same
//! decompression reqwest applies (tower-http) and the [`HttpClient`](crate::HttpClient)'s
//! cookie jar, default headers, timeouts and redirect handling.
//!
//! The substitute is used only while the host does not resolve, so a restored record takes
//! over again on its own. The check runs once per client, which the crawler builds once per
//! run.

use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::ToSocketAddrs;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Uri};
use http_body_util::BodyExt;
use http_body_util::Full;
use http_body_util::combinators::BoxBody;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::{TokioExecutor, TokioIo};
use reqwest::cookie::CookieStore;
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;
use tower_http::decompression::Decompression;
use tower_service::Service;
use url::Url;

use crate::ErrorClass;
use crate::client::{BuildError, TransportError};

/// The HTTPS port `host_resolves` asks about (`_HTTPS_PORT`).
const HTTPS_PORT: u16 = 443;
/// What a body read waits for each chunk of a response at most is the client's read
/// timeout; this is the size of one chunk handed out.
const CHUNK_SIZE: usize = 64 * 1024;

/// Whether a host still resolves (`host_resolves`).
pub type Resolves = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// `host_resolves`: `getaddrinfo(host, 443)` returns at least one address; any lookup
/// failure (no record, a name the resolver refuses) is `false`.
pub fn host_resolves(host: &str) -> bool {
    (host, HTTPS_PORT)
        .to_socket_addrs()
        .is_ok_and(|mut addresses| addresses.next().is_some())
}

/// `ConnectToTransport(host=..., connect_host=..., resolves=...)`: send requests for `host`
/// to `connect_host` while `host` does not resolve.
#[derive(Clone)]
pub struct ConnectTo {
    host: String,
    connect_host: String,
    resolves: Resolves,
    roots: Option<Arc<RootCertStore>>,
}

impl ConnectTo {
    pub fn new(host: impl Into<String>, connect_host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            connect_host: connect_host.into(),
            resolves: Arc::new(host_resolves),
            roots: None,
        }
    }

    /// The resolver check (`resolves=`); tests pass one that never asks DNS.
    #[must_use]
    pub fn with_resolves(mut self, resolves: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        self.resolves = Arc::new(resolves);
        self
    }

    /// Trust these roots instead of `SSL_CERT_FILE` / `SSL_CERT_DIR` / the Mozilla bundle.
    /// Tests use it; Python's tests set `SSL_CERT_FILE`, which a Rust test cannot change.
    #[must_use]
    pub fn with_roots(mut self, roots: RootCertStore) -> Self {
        self.roots = Some(Arc::new(roots));
        self
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn connect_host(&self) -> &str {
        &self.connect_host
    }
}

impl PartialEq for ConnectTo {
    fn eq(&self, other: &Self) -> bool {
        self.host == other.host && self.connect_host == other.connect_host
    }
}

impl Eq for ConnectTo {}

impl fmt::Debug for ConnectTo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectTo")
            .field("host", &self.host)
            .field("connect_host", &self.connect_host)
            .finish_non_exhaustive()
    }
}

/// `_server_name_context`'s trust roots: the same ones httpx picks for `verify=True`
/// (`SSL_CERT_FILE`, else `SSL_CERT_DIR`, else certifi's Mozilla bundle, which is what
/// `webpki-roots` carries). Verification stays fully on.
fn default_roots() -> Result<RootCertStore, BuildError> {
    let mut roots = RootCertStore::empty();
    if let Some(file) = std::env::var_os("SSL_CERT_FILE").filter(|value| !value.is_empty()) {
        add_pem_file(&mut roots, std::path::Path::new(&file))?;
    } else if let Some(dir) = std::env::var_os("SSL_CERT_DIR").filter(|value| !value.is_empty()) {
        let entries = std::fs::read_dir(&dir).map_err(|error| BuildError::Tls(error.to_string()))?;
        for entry in entries.flatten() {
            if entry.path().is_file() {
                // OpenSSL's capath skips files that are not certificates.
                let _ = add_pem_file(&mut roots, &entry.path());
            }
        }
    } else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    Ok(roots)
}

fn add_pem_file(roots: &mut RootCertStore, path: &std::path::Path) -> Result<(), BuildError> {
    let certificates: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
        .map_err(|error| BuildError::Tls(error.to_string()))?
        .collect::<Result<_, _>>()
        .map_err(|error| BuildError::Tls(error.to_string()))?;
    roots.add_parsable_certificates(certificates);
    Ok(())
}

// ---------- the proxy ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyScheme {
    Http,
    Https,
    Socks5,
}

/// `httpx.Proxy(url)`: scheme, address and the credentials in its user info.
#[derive(Clone)]
struct Proxy {
    scheme: ProxyScheme,
    host: String,
    port: u16,
    credentials: Option<(String, String)>,
}

impl Proxy {
    fn parse(raw: &str) -> Result<Self, BuildError> {
        let url = Url::parse(raw).map_err(|_| BuildError::ProxyUrl)?;
        let scheme = match url.scheme() {
            "http" => ProxyScheme::Http,
            "https" => ProxyScheme::Https,
            "socks5" | "socks5h" => ProxyScheme::Socks5,
            _ => return Err(BuildError::ProxyUrl),
        };
        let host = url.host_str().ok_or_else(|| BuildError::ProxyUrl)?.to_owned();
        let port = url.port_or_known_default().unwrap_or(match scheme {
            ProxyScheme::Http => 80,
            ProxyScheme::Https => 443,
            ProxyScheme::Socks5 => 1080,
        });
        let credentials = (!url.username().is_empty() || url.password().is_some()).then(|| {
            (
                fav_core::pyurl::unquote(url.username()),
                fav_core::pyurl::unquote(url.password().unwrap_or_default()),
            )
        });
        Ok(Self {
            scheme,
            host: host.trim_start_matches('[').trim_end_matches(']').to_owned(),
            port,
            credentials,
        })
    }

    /// `Proxy-Authorization: Basic ...` from the URL's user info, as httpx builds it.
    fn authorization(&self) -> Option<String> {
        self.credentials.as_ref().map(|(user, password)| {
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
            )
        })
    }
}

// ---------- the connector ----------

/// What a connection is made of: TCP, perhaps TLS to an HTTPS proxy, perhaps TLS to the
/// origin, layered as needed.
trait Io: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// One connection handed to hyper.
struct Conn {
    io: Box<dyn Io>,
    /// A plain connection to a forward proxy, so requests go out in absolute form.
    forward_proxy: bool,
}

impl AsyncRead for Conn {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Conn {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

impl Connection for Conn {
    fn connected(&self) -> Connected {
        Connected::new().proxy(self.forward_proxy)
    }
}

/// Why a connection could not be made, by the httpx class it stands for.
#[derive(Debug)]
struct ConnectFailure {
    class: ErrorClass,
    message: String,
}

impl fmt::Display for ConnectFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for ConnectFailure {}

impl ConnectFailure {
    fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }

    fn connect(error: &io::Error) -> Self {
        Self::new(
            if error.kind() == io::ErrorKind::TimedOut {
                ErrorClass::ConnectTimeout
            } else {
                ErrorClass::ConnectError
            },
            error.to_string(),
        )
    }
}

/// Opens connections to the substitute host, presenting the original host's name in TLS.
#[derive(Clone)]
struct Connector {
    /// The SNI and certificate name: the original host (`_ServerNameContext.server_name`).
    server_name: ServerName<'static>,
    tls: tokio_rustls::TlsConnector,
    proxy: Option<Arc<Proxy>>,
    connect_timeout: Duration,
}

impl Connector {
    async fn tcp(&self, host: &str, port: u16) -> Result<TcpStream, ConnectFailure> {
        match tokio::time::timeout(self.connect_timeout, TcpStream::connect((host, port))).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                Ok(stream)
            }
            Ok(Err(error)) => Err(ConnectFailure::connect(&error)),
            Err(_) => Err(ConnectFailure::new(ErrorClass::ConnectTimeout, "timed out")),
        }
    }

    async fn tls<S: Io + 'static>(&self, stream: S, name: ServerName<'static>) -> Result<Box<dyn Io>, ConnectFailure> {
        match tokio::time::timeout(self.connect_timeout, self.tls.connect(name, stream)).await {
            Ok(Ok(stream)) => Ok(Box::new(stream)),
            Ok(Err(error)) => Err(ConnectFailure::new(ErrorClass::ConnectError, error.to_string())),
            Err(_) => Err(ConnectFailure::new(
                ErrorClass::ConnectTimeout,
                "_ssl.c:989: The handshake operation timed out",
            )),
        }
    }

    /// A stream to the proxy itself (TLS for an `https://` proxy).
    async fn to_proxy(&self, proxy: &Proxy) -> Result<Box<dyn Io>, ConnectFailure> {
        let stream = self.tcp(&proxy.host, proxy.port).await?;
        if proxy.scheme == ProxyScheme::Https {
            let name = ServerName::try_from(proxy.host.clone())
                .map_err(|error| ConnectFailure::new(ErrorClass::ConnectError, error.to_string()))?;
            return self.tls(stream, name).await;
        }
        Ok(Box::new(stream))
    }

    async fn connect(self, uri: Uri) -> Result<Conn, ConnectFailure> {
        let https = uri.scheme_str() == Some("https");
        let host = uri
            .host()
            .unwrap_or_default()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        let (stream, forward_proxy): (Box<dyn Io>, bool) = match self.proxy.as_deref() {
            None => (Box::new(self.tcp(&host, port).await?), false),
            Some(proxy) if proxy.scheme == ProxyScheme::Socks5 => (Box::new(self.socks(proxy, &host, port).await?), false),
            // A plain request through an HTTP proxy is forwarded, not tunnelled.
            Some(proxy) if !https => (self.to_proxy(proxy).await?, true),
            Some(proxy) => {
                let stream = self.to_proxy(proxy).await?;
                (connect_tunnel(stream, proxy, &host, port).await?, false)
            }
        };
        let io = if https {
            self.tls(stream, self.server_name.clone()).await?
        } else {
            stream
        };
        Ok(Conn { io, forward_proxy })
    }

    async fn socks(&self, proxy: &Proxy, host: &str, port: u16) -> Result<TcpStream, ConnectFailure> {
        let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
        http.set_connect_timeout(Some(self.connect_timeout));
        http.set_nodelay(true);
        let proxy_uri: Uri = format!("http://{}:{}", bracketed(&proxy.host), proxy.port)
            .parse()
            .map_err(|_| ConnectFailure::new(ErrorClass::ConnectError, "Invalid proxy URL"))?;
        let mut socks = hyper_util::client::legacy::connect::proxy::SocksV5::new(proxy_uri, http).local_dns(false);
        if let Some((user, password)) = &proxy.credentials {
            socks = socks.with_auth(user.clone(), password.clone());
        }
        let target: Uri = format!("http://{}:{port}", bracketed(host))
            .parse()
            .map_err(|_| ConnectFailure::new(ErrorClass::ConnectError, "Invalid connect host"))?;
        // httpx raises `ProxyError` for every SOCKS failure.
        let stream = socks
            .call(target)
            .await
            .map_err(|error| ConnectFailure::new(ErrorClass::ProxyError, error.to_string()))?;
        Ok(stream.into_inner())
    }
}

fn bracketed(host: &str) -> String {
    if host.contains(':') { format!("[{host}]") } else { host.to_owned() }
}

/// httpcore's `TunnelHTTPConnection`: `CONNECT host:port` with `Host` and `Accept: */*`, plus
/// `Proxy-Authorization` from the proxy URL; any status outside 2xx is `ProxyError("<status>
/// <reason>")`.
async fn connect_tunnel(stream: Box<dyn Io>, proxy: &Proxy, host: &str, port: u16) -> Result<Box<dyn Io>, ConnectFailure> {
    let target = format!("{}:{port}", bracketed(host));
    let mut head = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nAccept: */*\r\n");
    if let Some(authorization) = proxy.authorization() {
        head = format!("{head}Proxy-Authorization: {authorization}\r\n");
    }
    head.push_str("\r\n");
    // Buffered, so nothing the proxy sends after its answer is lost; the reader stays the stream.
    let mut stream = BufReader::new(stream);
    stream
        .get_mut()
        .write_all(head.as_bytes())
        .await
        .map_err(|error| ConnectFailure::new(ErrorClass::ConnectError, error.to_string()))?;
    let mut status_line = String::new();
    let read = stream
        .read_line(&mut status_line)
        .await
        .map_err(|error| ConnectFailure::new(ErrorClass::ReadError, error.to_string()))?;
    if read == 0 {
        return Err(ConnectFailure::new(
            ErrorClass::RemoteProtocolError,
            "Server disconnected without sending a response.",
        ));
    }
    loop {
        let mut line = String::new();
        let read = stream
            .read_line(&mut line)
            .await
            .map_err(|error| ConnectFailure::new(ErrorClass::ReadError, error.to_string()))?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut parts = status_line.trim_end_matches(['\r', '\n']).splitn(3, ' ');
    let _version = parts.next();
    let status: u16 = parts
        .next()
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| ConnectFailure::new(ErrorClass::RemoteProtocolError, "illegal status line"))?;
    if !(200..=299).contains(&status) {
        let reason: String = parts.next().unwrap_or_default().chars().filter(char::is_ascii).collect();
        return Err(ConnectFailure::new(ErrorClass::ProxyError, format!("{status} {reason}")));
    }
    Ok(Box::new(stream))
}

impl Service<Uri> for Connector {
    type Response = TokioIo<Conn>;
    type Error = ConnectFailure;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let connector = self.clone();
        Box::pin(async move { connector.connect(uri).await.map(TokioIo::new) })
    }
}

// ---------- the route ----------

type RouteService = Decompression<Client<Connector, Full<Bytes>>>;
type RouteBody = BoxBody<Bytes, Box<dyn StdError + Send + Sync>>;

/// The part of an [`HttpClient`](crate::HttpClient) that serves requests for the host.
pub(crate) struct Route {
    host: String,
    connect_host: String,
    resolves: Resolves,
    redirect: tokio::sync::OnceCell<bool>,
    service: RouteService,
    forward_proxy_authorization: Option<HeaderValue>,
    connect_timeout: Duration,
    read_timeout: Duration,
}

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Route")
            .field("host", &self.host)
            .field("connect_host", &self.connect_host)
            .field("redirect", &self.redirect.get())
            .finish_non_exhaustive()
    }
}

/// Settings of the client a route belongs to.
pub(crate) struct RouteOptions<'a> {
    pub proxy: Option<&'a str>,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub max_keepalive_connections: usize,
}

impl Route {
    pub(crate) fn new(config: &ConnectTo, options: &RouteOptions<'_>) -> Result<Self, BuildError> {
        crate::install_crypto_provider();
        let roots = match &config.roots {
            Some(roots) => (**roots).clone(),
            None => default_roots()?,
        };
        let mut tls = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        // httpx speaks HTTP/1.1 only and offers nothing else in ALPN.
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let server_name = ServerName::try_from(config.host.clone()).map_err(|error| BuildError::Tls(error.to_string()))?;
        let proxy = options.proxy.filter(|proxy| !proxy.is_empty()).map(Proxy::parse).transpose()?;
        let forward_proxy_authorization = proxy
            .as_ref()
            .filter(|proxy| proxy.scheme != ProxyScheme::Socks5)
            .and_then(Proxy::authorization)
            .map(|value| HeaderValue::from_str(&value).map_err(|_| BuildError::Header("Proxy-Authorization".to_owned())))
            .transpose()?;
        let connector = Connector {
            server_name,
            tls: tokio_rustls::TlsConnector::from(Arc::new(tls)),
            proxy: proxy.map(Arc::new),
            connect_timeout: options.connect_timeout,
        };
        let client = Client::builder(TokioExecutor::new())
            // Title-cased names, as `HttpClient`'s reqwest side writes them (D182).
            .http1_title_case_headers(true)
            .pool_max_idle_per_host(options.max_keepalive_connections)
            .build(connector);
        Ok(Self {
            host: config.host.clone(),
            connect_host: config.connect_host.clone(),
            resolves: Arc::clone(&config.resolves),
            redirect: tokio::sync::OnceCell::new(),
            service: Decompression::new(client),
            forward_proxy_authorization,
            connect_timeout: options.connect_timeout,
            read_timeout: options.read_timeout,
        })
    }

    /// Whether a request takes the route: a request for the host while the host does not
    /// resolve (`handle_request`'s check, asked once, on the first request for it), and any
    /// HTTPS request for the substitute itself, because `_ServerNameContext` presents the
    /// host's name on every TLS connection made to the substitute, whatever the URL asked for.
    pub(crate) async fn applies(&self, url: &Url) -> bool {
        if url.scheme() == "https" && url.host_str() == Some(self.connect_host.as_str()) {
            return true;
        }
        if url.host_str() != Some(self.host.as_str()) {
            return false;
        }
        *self
            .redirect
            .get_or_init(|| async {
                let check = Arc::clone(&self.resolves);
                let host = self.host.clone();
                let resolved = tokio::task::spawn_blocking(move || check(&host)).await.unwrap_or(false);
                if !resolved {
                    fav_core::logging::get("connect_to").warning(format!(
                        "{} does not resolve; connecting through {} instead",
                        self.host, self.connect_host
                    ));
                }
                !resolved
            })
            .await
    }

    /// `_redirected`: the request goes to the substitute host; `Host` keeps the original
    /// authority, so the origin still sees its own name.
    pub(crate) async fn send(
        &self,
        method: Method,
        url: &Url,
        headers: &HeaderMap,
        body: Option<Vec<u8>>,
        jar: &reqwest::cookie::Jar,
    ) -> Result<RouteResponse, TransportError> {
        let failed = |class: ErrorClass, message: String| TransportError {
            class,
            host: url.host_str().unwrap_or_default().to_owned(),
            message,
        };
        let mut target = url.clone();
        target.set_fragment(None);
        target
            .set_host(Some(&self.connect_host))
            .map_err(|error| failed(ErrorClass::InvalidUrl, format!("Invalid URL: {error}")))?;
        let uri: Uri = target
            .as_str()
            .parse()
            .map_err(|error: http::uri::InvalidUri| failed(ErrorClass::InvalidUrl, error.to_string()))?;
        let mut request = http::Request::new(Full::new(Bytes::from(body.unwrap_or_default())));
        *request.method_mut() = method;
        *request.uri_mut() = uri;
        let request_headers = request.headers_mut();
        request_headers.clone_from(headers);
        if !request_headers.contains_key(http::header::HOST) {
            let authority = match url.port() {
                Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
                None => url.host_str().unwrap_or_default().to_owned(),
            };
            let value = HeaderValue::from_str(&authority).map_err(|error| failed(ErrorClass::LocalProtocolError, error.to_string()))?;
            request_headers.insert(http::header::HOST, value);
        }
        // reqwest's defaults, so both paths of one client send the same request.
        if !request_headers.contains_key(http::header::ACCEPT) {
            request_headers.insert(http::header::ACCEPT, HeaderValue::from_static("*/*"));
        }
        if !request_headers.contains_key(http::header::COOKIE)
            && let Some(cookies) = jar.cookies(url)
        {
            request_headers.insert(http::header::COOKIE, cookies);
        }
        if url.scheme() == "http"
            && let Some(authorization) = &self.forward_proxy_authorization
        {
            request_headers.insert(http::header::PROXY_AUTHORIZATION, authorization.clone());
        }

        let mut service = self.service.clone();
        let sent = async {
            std::future::poll_fn(|cx| service.poll_ready(cx)).await?;
            service.call(request).await
        };
        // The connector bounds the connection itself; the answer gets the read timeout on top.
        let response = match tokio::time::timeout(self.connect_timeout + self.read_timeout, sent).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                let (class, message) = classify(&error);
                return Err(failed(class, message));
            }
            Err(_) => return Err(failed(ErrorClass::ReadTimeout, "timed out".to_owned())),
        };
        let (parts, body) = response.into_parts();
        let mut set_cookies = parts.headers.get_all(http::header::SET_COOKIE).iter();
        jar.set_cookies(&mut set_cookies, url);
        let mut headers = parts.headers;
        if headers.contains_key(http::header::CONTENT_ENCODING) {
            // Decoded below, as reqwest drops both once it decodes a body.
            headers.remove(http::header::CONTENT_ENCODING);
            headers.remove(http::header::CONTENT_LENGTH);
        }
        Ok(RouteResponse {
            status: parts.status,
            headers,
            extensions: parts.extensions,
            body: body.map_err(Into::into).boxed(),
            pending: Bytes::new(),
            read_timeout: self.read_timeout,
            host: url.host_str().unwrap_or_default().to_owned(),
        })
    }
}

/// The httpx class and message for a failure of the route's client.
fn classify(error: &(dyn StdError + 'static)) -> (ErrorClass, String) {
    let mut current: Option<&(dyn StdError + 'static)> = Some(error);
    let mut innermost = error.to_string();
    let mut class = None;
    while let Some(cause) = current {
        if let Some(failure) = cause.downcast_ref::<ConnectFailure>() {
            return (failure.class, failure.message.clone());
        }
        if class.is_none() {
            if let Some(hyper) = cause.downcast_ref::<hyper::Error>() {
                if hyper.is_incomplete_message() || hyper.is_parse() || hyper.is_parse_status() {
                    class = Some(ErrorClass::RemoteProtocolError);
                } else if hyper.is_timeout() {
                    class = Some(ErrorClass::ReadTimeout);
                }
            } else if let Some(io) = cause.downcast_ref::<io::Error>() {
                class = Some(match io.kind() {
                    io::ErrorKind::TimedOut => ErrorClass::ReadTimeout,
                    io::ErrorKind::UnexpectedEof => ErrorClass::RemoteProtocolError,
                    io::ErrorKind::InvalidData => ErrorClass::DecodingError,
                    _ => ErrorClass::ReadError,
                });
            }
        }
        innermost = cause.to_string();
        current = cause.source();
    }
    (class.unwrap_or(ErrorClass::RemoteProtocolError), innermost)
}

/// A response that came through the route.
pub(crate) struct RouteResponse {
    pub(crate) status: http::StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) extensions: http::Extensions,
    body: RouteBody,
    pending: Bytes,
    read_timeout: Duration,
    host: String,
}

impl fmt::Debug for RouteResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

impl RouteResponse {
    /// The next chunk of the decoded body; `None` at the end. Each wait for data is bounded
    /// by the client's read timeout, as httpx bounds each read.
    pub(crate) async fn chunk(&mut self) -> Result<Option<Bytes>, TransportError> {
        if !self.pending.is_empty() {
            let take = self.pending.len().min(CHUNK_SIZE);
            return Ok(Some(self.pending.split_to(take)));
        }
        loop {
            let frame = match tokio::time::timeout(self.read_timeout.max(Duration::from_millis(1)), self.body.frame()).await {
                Err(_) => return Err(self.error(ErrorClass::ReadTimeout, "timed out".to_owned())),
                Ok(None) => return Ok(None),
                Ok(Some(Err(error))) => {
                    let (class, message) = classify(&*error);
                    return Err(self.error(class, message));
                }
                Ok(Some(Ok(frame))) => frame,
            };
            if let Ok(mut data) = frame.into_data() {
                if data.is_empty() {
                    continue;
                }
                if data.len() > CHUNK_SIZE {
                    self.pending = data.split_off(CHUNK_SIZE);
                }
                return Ok(Some(data));
            }
        }
    }

    pub(crate) async fn bytes(mut self) -> Result<Bytes, TransportError> {
        let mut all = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            all.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(all))
    }

    fn error(&self, class: ErrorClass, message: String) -> TransportError {
        TransportError {
            class,
            host: self.host.clone(),
            message,
        }
    }
}

#[cfg(test)]
mod tests;
