//! An HTTP client that behaves like the `httpx.AsyncClient` the sources build.
//!
//! Python origin: the `httpx.AsyncClient(...)` / `httpx.Client(...)` constructors in the
//! sources and what httpx 0.28 does around a request: default headers, separate connect
//! and read timeouts, a cookie jar, HTTP/1.1 only, redirects followed by httpx's rules
//! with the hops kept in `response.history`, `raise_for_status()` and its message, and
//! the exception classes a failure maps onto. reqwest does the transport; redirects are
//! followed here, one request per hop, because reqwest neither keeps the redirect
//! responses (Jandan reads their `Location` and `x-image-errno`) nor follows httpx's rules
//! for headers across hops.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use url::Url;

use crate::connect_to::{ConnectTo, Route, RouteOptions, RouteResponse};
use crate::{ErrorClass, client_builder};

/// httpx's `User-Agent` when a client sets none (`python-httpx/<version>`).
pub const HTTPX_USER_AGENT: &str = "python-httpx/0.28.1";
/// httpx's `max_redirects` default.
pub const MAX_REDIRECTS: usize = 20;

/// What a source passes to `httpx.AsyncClient(...)`.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientOptions {
    /// Default headers (`headers=`), sent with every request unless the request sets
    /// the same name.
    pub headers: Vec<(String, String)>,
    /// `timeout=`: httpx applies one value to connect, read, write and pool waits.
    pub timeout: Duration,
    /// `httpx.Timeout(t, connect=c)`: a separate connect timeout.
    pub connect_timeout: Option<Duration>,
    /// `follow_redirects=`.
    pub follow_redirects: bool,
    /// `proxy=`: the source's own proxy. Empty or `None` uses `HTTP_PROXY` /
    /// `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` from the environment, as httpx does.
    pub proxy: Option<String>,
    /// `limits=httpx.Limits(max_keepalive_connections=...)`.
    pub max_keepalive_connections: usize,
    /// `transport=ConnectToTransport(...)`: requests for one host go to another while the
    /// host does not resolve ([`crate::connect_to`]).
    pub connect_to: Option<ConnectTo>,
}

impl Default for ClientOptions {
    /// httpx's defaults: 5 s timeouts, no redirects, 20 idle connections.
    fn default() -> Self {
        Self {
            headers: Vec::new(),
            timeout: Duration::from_secs(5),
            connect_timeout: None,
            follow_redirects: false,
            proxy: None,
            max_keepalive_connections: 20,
            connect_to: None,
        }
    }
}

impl fmt::Debug for ClientOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Header values and the proxy URL can carry credentials.
        f.debug_struct("ClientOptions")
            .field("headers", &self.headers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>())
            .field("timeout", &self.timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("follow_redirects", &self.follow_redirects)
            .field("proxy", &self.proxy.as_ref().map(|_| "<redacted>"))
            .field("max_keepalive_connections", &self.max_keepalive_connections)
            .field("connect_to", &self.connect_to)
            .finish()
    }
}

/// The client could not be built (an unusable proxy URL or header).
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("Invalid header {0}")]
    Header(String),
    #[error("Invalid proxy URL")]
    Proxy(#[source] reqwest::Error),
    #[error("Could not build the HTTP client")]
    Client(#[source] reqwest::Error),
    /// A proxy URL the connect-to route cannot use (not kept: it can carry credentials).
    #[error("Invalid proxy URL")]
    ProxyUrl,
    /// The connect-to route's trust roots or server name.
    #[error("TLS setup failed: {0}")]
    Tls(String),
}

/// One request. Header names are matched case-insensitively against the defaults.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl Request {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn get(url: impl Into<String>) -> Self {
        Self::new(Method::GET, url)
    }

    /// `content=` bytes.
    pub fn post(url: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            body: Some(body.into()),
            ..Self::new(Method::POST, url)
        }
    }

    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &self.headers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>())
            .field("body", &self.body.as_ref().map(Vec::len))
            .finish()
    }
}

/// A transport failure, by the httpx exception class it stands for.
///
/// `message` is the innermost cause's text, which never contains the URL (Python's
/// `str(exc)` is httpcore's text for the same failure: `[Errno 111] Connection refused`
/// where this says `Connection refused (os error 111)`). `host` is the request's host,
/// which callers report when the message is empty, never the URL, which can carry tokens.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct TransportError {
    pub class: ErrorClass,
    pub host: String,
    pub message: String,
}

impl TransportError {
    /// A reqwest failure while talking to `url`.
    pub fn from_reqwest(error: &reqwest::Error, url: &Url) -> Self {
        Self {
            class: ErrorClass::of(error),
            host: url.host_str().unwrap_or_default().to_owned(),
            message: innermost_message(error),
        }
    }

    fn new(class: ErrorClass, url: &Url, message: impl Into<String>) -> Self {
        Self {
            class,
            host: url.host_str().unwrap_or_default().to_owned(),
            message: message.into(),
        }
    }
}

/// The text of the deepest error in a chain: an OS error or hyper's own words, without
/// the URL reqwest's top-level text names.
fn innermost_message(error: &(dyn std::error::Error + 'static)) -> String {
    let mut current = error;
    while let Some(source) = current.source() {
        current = source;
    }
    if std::ptr::addr_eq(current, error) {
        // No cause: reqwest's own text without the `for url (...)` part.
        let text = error.to_string();
        return text.split(" for url (").next().unwrap_or_default().to_owned();
    }
    current.to_string()
}

/// One redirect response that was followed (`response.history[i]`).
#[derive(Debug, Clone)]
pub struct Redirect {
    pub status: u16,
    pub url: Url,
    pub headers: HeaderMap,
}

/// Where a response came from: reqwest, or the connect-to route.
#[derive(Debug)]
enum Inner {
    Reqwest(reqwest::Response),
    Route(RouteResponse),
}

impl Inner {
    fn status(&self) -> StatusCode {
        match self {
            Self::Reqwest(response) => response.status(),
            Self::Route(response) => response.status,
        }
    }

    fn headers(&self) -> &HeaderMap {
        match self {
            Self::Reqwest(response) => response.headers(),
            Self::Route(response) => &response.headers,
        }
    }

    fn extensions(&self) -> &http::Extensions {
        match self {
            Self::Reqwest(response) => response.extensions(),
            Self::Route(response) => &response.extensions,
        }
    }

    async fn bytes(self, url: &Url) -> Result<bytes::Bytes, TransportError> {
        match self {
            Self::Reqwest(response) => response.bytes().await.map_err(|error| TransportError::from_reqwest(&error, url)),
            Self::Route(response) => response.bytes().await,
        }
    }
}

/// A response whose body has not been read yet.
#[derive(Debug)]
pub struct Response {
    inner: Inner,
    url: Url,
    history: Vec<Redirect>,
}

impl Response {
    pub fn status(&self) -> u16 {
        self.inner.status().as_u16()
    }

    /// `response.url`: the URL of the request that produced this response.
    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }

    /// A header as text: every value joined with ", ", each UTF-8 or else latin-1, like httpx.
    pub fn header(&self, name: &str) -> Option<String> {
        header_text(self.inner.headers(), name)
    }

    /// `response.history`: the redirects followed to get here, oldest first.
    pub fn history(&self) -> &[Redirect] {
        &self.history
    }

    /// `response.reason_phrase`: the server's own phrase when it sent an unusual one
    /// (HTTP/1.1), else the standard phrase, else empty.
    pub fn reason_phrase(&self) -> String {
        reason_phrase(self.inner.status(), self.inner.extensions())
    }

    /// `response.raise_for_status()`: `Err` for anything but a 2xx.
    pub fn error_for_status(&self) -> Result<(), StatusError> {
        if self.inner.status().is_success() {
            return Ok(());
        }
        Err(StatusError {
            status: self.status(),
            reason: self.reason_phrase(),
            url: self.url.to_string(),
            location: has_redirect_location(self.inner.status(), self.inner.headers())
                .then(|| self.header("location"))
                .flatten(),
        })
    }

    /// The next chunk of the (decoded) body; `None` at the end.
    pub async fn chunk(&mut self) -> Result<Option<bytes::Bytes>, TransportError> {
        match &mut self.inner {
            Inner::Reqwest(response) => {
                let url = self.url.clone();
                response.chunk().await.map_err(|error| TransportError::from_reqwest(&error, &url))
            }
            Inner::Route(response) => response.chunk().await,
        }
    }

    /// The whole (decoded) body.
    pub async fn bytes(self) -> Result<bytes::Bytes, TransportError> {
        self.inner.bytes(&self.url).await
    }
}

/// A header as text, the way httpx reads it: every value joined with ", ", each decoded as UTF-8, else latin-1.
pub fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    // httpx decodes header values as ASCII, else UTF-8, else latin-1, and joins repeated
    // headers with ", ".
    let values: Vec<String> = headers
        .get_all(name)
        .iter()
        .map(|value| match std::str::from_utf8(value.as_bytes()) {
            Ok(text) => text.to_owned(),
            Err(_) => value.as_bytes().iter().map(|&byte| char::from(byte)).collect(),
        })
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

fn reason_phrase(status: StatusCode, extensions: &http::Extensions) -> String {
    if let Some(phrase) = extensions.get::<hyper::ext::ReasonPhrase>() {
        return phrase
            .as_bytes()
            .iter()
            .filter(|byte| byte.is_ascii())
            .map(|&byte| char::from(byte))
            .collect();
    }
    status.canonical_reason().unwrap_or_default().to_owned()
}

fn has_redirect_location(status: StatusCode, headers: &HeaderMap) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) && headers.contains_key("location")
}

/// `httpx.HTTPStatusError`, with httpx's message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct StatusError {
    pub status: u16,
    pub reason: String,
    pub url: String,
    /// The `Location` of a redirect status that was not followed.
    pub location: Option<String>,
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let error_type = match self.status / 100 {
            1 => "Informational response",
            3 => "Redirect response",
            4 => "Client error",
            5 => "Server error",
            _ => "Invalid status code",
        };
        writeln!(f, "{error_type} '{} {}' for url '{}'", self.status, self.reason, self.url)?;
        if let Some(location) = &self.location {
            writeln!(f, "Redirect location: '{location}'")?;
        }
        write!(
            f,
            "For more information check: https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/{}",
            self.status
        )
    }
}

/// `httpx.AsyncClient`.
#[derive(Debug, Clone)]
pub struct HttpClient {
    client: reqwest::Client,
    headers: HeaderMap,
    follow_redirects: bool,
    /// The cookie jar, shared by reqwest and the connect-to route.
    jar: Arc<reqwest::cookie::Jar>,
    connect_to: Option<Arc<Route>>,
}

fn header_pair(name: &str, value: &str) -> Result<(HeaderName, HeaderValue), BuildError> {
    let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| BuildError::Header(name.to_owned()))?;
    let value = HeaderValue::from_str(value).map_err(|_| BuildError::Header(name.to_string()))?;
    Ok((name, value))
}

impl HttpClient {
    pub fn new(options: &ClientOptions) -> Result<Self, BuildError> {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::USER_AGENT, HeaderValue::from_static(HTTPX_USER_AGENT));
        for (name, value) in &options.headers {
            let (name, value) = header_pair(name, value)?;
            headers.insert(name, value);
        }
        let jar = Arc::new(reqwest::cookie::Jar::default());
        let mut builder = client_builder()
            .http1_only()
            .cookie_provider(Arc::clone(&jar))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(options.connect_timeout.unwrap_or(options.timeout))
            .read_timeout(options.timeout)
            .pool_max_idle_per_host(options.max_keepalive_connections);
        if let Some(proxy) = options.proxy.as_deref().filter(|proxy| !proxy.is_empty()) {
            builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(BuildError::Proxy)?);
        }
        let connect_to = options
            .connect_to
            .as_ref()
            .map(|config| {
                Route::new(
                    config,
                    &RouteOptions {
                        proxy: options.proxy.as_deref(),
                        connect_timeout: options.connect_timeout.unwrap_or(options.timeout),
                        read_timeout: options.timeout,
                        max_keepalive_connections: options.max_keepalive_connections,
                    },
                )
            })
            .transpose()?
            .map(Arc::new);
        Ok(Self {
            client: builder.build().map_err(BuildError::Client)?,
            headers,
            follow_redirects: options.follow_redirects,
            jar,
            connect_to,
        })
    }

    /// `client.get(url)`.
    pub async fn get(&self, url: &str) -> Result<Response, TransportError> {
        self.send(Request::get(url)).await
    }

    /// `client.send(request)`, following redirects when the client does.
    pub async fn send(&self, request: Request) -> Result<Response, TransportError> {
        let mut url = parse_url(&request.url)?;
        let mut method = request.method.clone();
        let mut headers = self.headers.clone();
        for (name, value) in &request.headers {
            let (name, value) =
                header_pair(name, value).map_err(|error| TransportError::new(ErrorClass::LocalProtocolError, &url, error.to_string()))?;
            headers.insert(name, value);
        }
        let mut body = request.body.clone();
        let mut history: Vec<Redirect> = Vec::new();
        loop {
            if history.len() > MAX_REDIRECTS {
                return Err(TransportError::new(
                    ErrorClass::TooManyRedirects,
                    &url,
                    "Exceeded maximum allowed redirects.",
                ));
            }
            let response = match &self.connect_to {
                Some(route) if route.applies(&url).await => {
                    Inner::Route(route.send(method.clone(), &url, &headers, body.clone(), &self.jar).await?)
                }
                _ => {
                    let mut builder = self.client.request(method.clone(), url.clone()).headers(headers.clone());
                    if let Some(bytes) = &body {
                        builder = builder.body(bytes.clone());
                    }
                    Inner::Reqwest(builder.send().await.map_err(|error| TransportError::from_reqwest(&error, &url))?)
                }
            };
            let status = response.status();
            if !self.follow_redirects || !has_redirect_location(status, response.headers()) {
                return Ok(Response {
                    inner: response,
                    url,
                    history,
                });
            }
            let next_url = redirect_url(&url, &header_text(response.headers(), "location").unwrap_or_default())?;
            let next_method = redirect_method(&method, status.as_u16());
            redirect_headers(&mut headers, &url, &next_url, next_method != method);
            if next_method != method {
                body = None;
            }
            history.push(Redirect {
                status: status.as_u16(),
                url: url.clone(),
                headers: response.headers().clone(),
            });
            // The redirect's own body is read and dropped, as httpx does (`aread()`); a
            // failure reading it ends the request.
            response.bytes(&url).await?;
            url = next_url;
            method = next_method;
        }
    }
}

fn parse_url(raw: &str) -> Result<Url, TransportError> {
    Url::parse(raw).map_err(|error| TransportError {
        class: if matches!(error, url::ParseError::RelativeUrlWithoutBase) {
            ErrorClass::UnsupportedProtocol
        } else {
            ErrorClass::InvalidUrl
        },
        host: String::new(),
        message: if matches!(error, url::ParseError::RelativeUrlWithoutBase) {
            "Request URL is missing an 'http://' or 'https://' protocol.".to_owned()
        } else {
            format!("Invalid URL: {error}")
        },
    })
}

/// `_redirect_url`: the `Location` resolved against the request URL, keeping the
/// request's fragment when the location has none.
fn redirect_url(current: &Url, location: &str) -> Result<Url, TransportError> {
    let mut next = current.join(location).map_err(|error| {
        TransportError::new(
            ErrorClass::RemoteProtocolError,
            current,
            format!("Invalid URL in location header: {error}."),
        )
    })?;
    if next.fragment().is_none()
        && let Some(fragment) = current.fragment()
    {
        next.set_fragment(Some(fragment));
    }
    Ok(next)
}

/// `_redirect_method`: 303 turns everything but HEAD into GET; 301 and 302 turn POST into GET.
fn redirect_method(method: &Method, status: u16) -> Method {
    if (status == 303 && *method != Method::HEAD) || (matches!(status, 301 | 302) && *method == Method::POST) {
        Method::GET
    } else {
        method.clone()
    }
}

/// `_redirect_headers`: no `Authorization` to another origin (unless it is the same host
/// upgraded to https), no body headers once the body is dropped. The cookie jar adds the
/// next hop's cookies itself.
fn redirect_headers(headers: &mut HeaderMap, current: &Url, next: &Url, method_changed: bool) {
    let same_origin = current.scheme() == next.scheme()
        && current.host_str() == next.host_str()
        && current.port_or_known_default() == next.port_or_known_default();
    let https_upgrade = current.scheme() == "http"
        && next.scheme() == "https"
        && current.host_str() == next.host_str()
        && current.port_or_known_default() == Some(80)
        && next.port_or_known_default() == Some(443);
    if !same_origin && !https_upgrade {
        headers.remove(reqwest::header::AUTHORIZATION);
    }
    if method_changed {
        headers.remove(reqwest::header::CONTENT_LENGTH);
        headers.remove(reqwest::header::TRANSFER_ENCODING);
    }
    headers.remove(reqwest::header::COOKIE);
}

#[cfg(test)]
mod tests;
