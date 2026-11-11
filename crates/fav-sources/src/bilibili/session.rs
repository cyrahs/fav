//! Keeps a Bilibili login captured on the 登录态 page alive without a browser.
//!
//! Python origin: `src/web/bilibili_session.py`. A browser signed in to bilibili.com does
//! two things a stored cookie jar does not: it fetches a fresh `bili_ticket` (valid three
//! days) from `GenWebTicket`, and when `/cookie/info` says so it trades the refresh token
//! (`ac_time_value` in localStorage) for a new `SESSDATA`/`bili_jct` pair and a new refresh
//! token, then confirms the trade, which retires the old pair. This module does both from
//! the stored cookies, so fav is the one writer of the session.
//!
//! Cookie values and the refresh token are credentials: nothing here logs them.

use std::fmt::Write as _;
use std::sync::LazyLock;
use std::time::Duration;

use fav_core::logging::{self, Logger};
use fav_core::pyjson::{Dict, Value};
use fav_net::httpx::HttpError;
use fav_net::reqwest::{self, Method, header};
use fav_runtime::pystr::str_value;
use fav_runtime::site_sessions::{StoredCookie, cookie_value};
use fav_runtime::source::JobError;
use hmac::{KeyInit, Mac};
use regex::Regex;
use rsa::pkcs8::DecodePublicKey;
use sha2::Sha256;

use super::cookiejar;
use crate::notifier::HttpJobError as _;
use crate::pydecode::{self, PyError, py_eq_int, urlencode};

/// The `extra` key the refresh token is stored under.
pub const REFRESH_TOKEN_KEY: &str = "refresh_token";

const TICKET_PATH: &str = "/bapis/bilibili.api.ticket.v1.Ticket/GenWebTicket";
const TICKET_KEY: &[u8] = b"XgwSnGZ1p";
const TICKET_KEY_ID: &str = "ec02";
/// Renew once less than this is left of the three days a ticket lasts.
const TICKET_RENEW_MARGIN_SECONDS: f64 = 2.0 * 86_400.0;
const TICKET_DEFAULT_TTL: i128 = 259_200;
const COOKIE_INFO_PATH: &str = "/x/passport-login/web/cookie/info";
const REFRESH_PATH: &str = "/x/passport-login/web/cookie/refresh";
const CONFIRM_PATH: &str = "/x/passport-login/web/confirm/refresh";
/// Where the refresh answer's `Set-Cookie` headers are read as coming from, whatever
/// origin the request went to (tests point it at a local server).
const REFRESH_URL: &str = "https://passport.bilibili.com/x/passport-login/web/cookie/refresh";
const CORRESPOND_KEY: &str = "-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDLgd2OAkcGVtoE3ThUREbio0Eg
Uc/prcajMKXvkCKFCWhJYJcLkcM2DKKcSeFpD/j6Boy538YXnR6VhcuUJOhH2x71
nzPjfdTcqMz7djHum0qSZA0AyCBDABUqCrfNgCiJ00Ra7GmRj+YCK1NJEuewlb40
JNrRuoEUXpabUzGB8QIDAQAB
-----END PUBLIC KEY-----";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const COOKIE_DOMAIN: &str = ".bilibili.com";
const TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_OK: u16 = 200;

static REFRESH_CSRF_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<div id="1-name">(.+?)</div>"#).expect("csrf pattern"));

fn log() -> Logger {
    logging::get("bilibili")
}

/// The three origins the renewal talks to; tests point them at a local server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub api: String,
    pub passport: String,
    pub www: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api: super::api::API_BASE.to_owned(),
            passport: "https://passport.bilibili.com".to_owned(),
            www: "https://www.bilibili.com".to_owned(),
        }
    }
}

impl Endpoints {
    /// All three at one origin (tests).
    pub fn at(origin: &str) -> Self {
        let origin = origin.trim_end_matches('/').to_owned();
        Self {
            api: origin.clone(),
            passport: origin.clone(),
            www: origin,
        }
    }
}

/// Why a renewal step failed.
#[derive(Debug, Clone, PartialEq)]
pub enum RenewError {
    /// `RenewalError`: Bilibili did not go along; the session is left as it was.
    Renewal(String),
    Http(HttpError),
    /// Another Python exception (a `ValueError` from a malformed number, say), which the
    /// crawl does not catch.
    Python(PyError),
    /// Storing the renewed session failed.
    Save(JobError),
}

impl RenewError {
    fn renewal(message: impl Into<String>) -> Self {
        Self::Renewal(message.into())
    }

    /// `RenewalError` or `httpx.HTTPError`: what `renew_session` logs and moves past.
    pub fn is_expected(&self) -> bool {
        matches!(self, Self::Renewal(_) | Self::Http(_))
    }

    pub fn into_job_error(self) -> JobError {
        match self {
            Self::Renewal(message) => JobError::new("RenewalError", message),
            Self::Http(error) => error.into_job_error(),
            Self::Python(error) => JobError::new(error.class, error.message),
            Self::Save(error) => error,
        }
    }
}

impl std::fmt::Display for RenewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Renewal(message) => f.write_str(message),
            Self::Http(error) => error.fmt(f),
            Self::Python(error) => f.write_str(&error.message),
            Self::Save(error) => f.write_str(error.message()),
        }
    }
}

impl From<HttpError> for RenewError {
    fn from(error: HttpError) -> Self {
        Self::Http(error)
    }
}

/// A refreshed login: the new jar and refresh token.
#[derive(Clone, PartialEq)]
pub struct Renewal {
    pub cookies: Vec<StoredCookie>,
    pub refresh_token: String,
    /// What changed, for the log: `SESSDATA`.
    pub renewed: Vec<String>,
}

impl std::fmt::Debug for Renewal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renewal")
            .field("cookies", &self.cookies)
            .field("renewed", &self.renewed)
            .finish_non_exhaustive()
    }
}

/// `ticket_hexsign`: HMAC-SHA256 of `ts{timestamp}` under the web ticket key.
pub fn ticket_hexsign(timestamp: i64) -> String {
    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(TICKET_KEY).expect("HMAC takes any key length");
    mac.update(format!("ts{timestamp}").as_bytes());
    hex(&mac.finalize().into_bytes())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// `correspond_path`: `refresh_{ms}` encrypted with RSA-OAEP (SHA-256) under Bilibili's
/// public key, as hex. Random padding makes every call different.
pub fn correspond_path(timestamp_ms: i64) -> String {
    let key = rsa::PublicKey::from_public_key_pem(CORRESPOND_KEY).expect("the embedded key parses");
    let encrypted = key
        .oaep_encrypt::<Sha256>(format!("refresh_{timestamp_ms}").as_bytes())
        .expect("a short message encrypts");
    hex(&encrypted)
}

/// `ticket_expires_at`: when the stored `bili_ticket` runs out, from its companion cookie
/// or its own expiry. `float(raw)` of digits that are not decimals (`²`) raises.
pub fn ticket_expires_at(cookies: &[StoredCookie]) -> Result<Option<f64>, PyError> {
    if cookie_value(cookies, "bili_ticket").is_empty() {
        return Ok(None);
    }
    let raw = cookie_value(cookies, "bili_ticket_expires");
    if fav_core::pytext::py_isdigit(raw) {
        return fav_core::pyint::decimal_digits_to_float(raw).map(Some).ok_or_else(|| {
            PyError::new(
                "ValueError",
                format!("could not convert string to float: {}", fav_runtime::pystr::repr_str(raw)),
            )
        });
    }
    Ok(cookies
        .iter()
        .find(|cookie| cookie.name == "bili_ticket" && cookie.expires.is_some_and(|expires| expires != 0.0))
        .and_then(|cookie| cookie.expires))
}

/// `ticket_due`: no ticket, or less than two days left.
pub fn ticket_due(cookies: &[StoredCookie], now: f64) -> Result<bool, PyError> {
    Ok(ticket_expires_at(cookies)?.is_none_or(|expires| expires - now < TICKET_RENEW_MARGIN_SECONDS))
}

/// `merge_cookies`: the jar with each update replacing the cookie of the same name (an
/// update for a new name is added at the end), then without empty or expired cookies.
pub fn merge_cookies(cookies: &[StoredCookie], updates: &[StoredCookie], now: f64) -> Vec<StoredCookie> {
    // `{c.name: c for c in updates}`: the last update of a name, at its first position.
    let mut by_name: Vec<(String, Option<StoredCookie>)> = Vec::new();
    for update in updates {
        match by_name.iter_mut().find(|(name, _)| *name == update.name) {
            Some(slot) => slot.1 = Some(update.clone()),
            None => by_name.push((update.name.clone(), Some(update.clone()))),
        }
    }
    let mut merged: Vec<StoredCookie> = cookies
        .iter()
        .map(|cookie| {
            by_name
                .iter_mut()
                .find(|(name, _)| *name == cookie.name)
                .and_then(|(_, update)| update.take())
                .unwrap_or_else(|| cookie.clone())
        })
        .collect();
    merged.extend(by_name.into_iter().filter_map(|(_, update)| update));
    merged
        .into_iter()
        .filter(|cookie| !cookie.value.is_empty() && cookie.expires.is_none_or(|expires| expires > now))
        .collect()
}

// ---------- the client ----------

/// What one answer carried.
#[derive(Debug)]
struct Answer {
    status: u16,
    set_cookie: Vec<String>,
    body: Vec<u8>,
}

/// `_Client`: httpx with the stored jar sent as one `Cookie` header and nothing remembered
/// between calls.
#[derive(Debug)]
struct Client {
    client: reqwest::Client,
}

impl Client {
    fn new() -> Result<Self, HttpError> {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::REFERER, header::HeaderValue::from_static("https://www.bilibili.com/"));
        headers.insert(header::ORIGIN, header::HeaderValue::from_static("https://www.bilibili.com"));
        let client = fav_net::client_builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .connect_timeout(TIMEOUT)
            .read_timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| HttpError::transport(&error))?;
        Ok(Self { client })
    }

    async fn request(
        &self,
        method: Method,
        url: &str,
        cookies: &[StoredCookie],
        params: &[(&str, String)],
        form: Option<&[(&str, String)]>,
    ) -> Result<Answer, HttpError> {
        let mut url = url.to_owned();
        if !params.is_empty() {
            url.push('?');
            url.push_str(&urlencode(params));
        }
        let mut request = self.client.request(method, url);
        let cookie_header = cookies
            .iter()
            .filter(|cookie| !cookie.value.is_empty())
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect::<Vec<_>>()
            .join("; ");
        if !cookie_header.is_empty() {
            request = request.header(header::COOKIE, cookie_header);
        }
        if let Some(form) = form {
            request = request
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(urlencode(form));
        }
        let response = request.send().await.map_err(|error| HttpError::transport(&error))?;
        let status = response.status().as_u16();
        let set_cookie = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| header_text(value.as_bytes()))
            .collect();
        let body = response.bytes().await.map_err(|error| HttpError::transport(&error))?.to_vec();
        Ok(Answer { status, set_cookie, body })
    }
}

/// A header value as httpx decodes it: ASCII, else UTF-8, else Latin-1.
fn header_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes.iter().map(|byte| char::from(*byte)).collect(),
    }
}

/// `_json`: the answer's `data` (a dict, else empty) when `code` is 0.
fn json_data(answer: &Answer, step: &str) -> Result<Dict, RenewError> {
    let status = answer.status;
    let Ok(payload) = pydecode::json_from_bytes(&answer.body) else {
        return Err(RenewError::renewal(format!("{step}: HTTP {status}, not JSON")));
    };
    let code = match &payload {
        Value::Dict(dict) => dict.get("code").cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    };
    if !py_eq_int(&code, 0) {
        return Err(RenewError::renewal(format!("{step}: HTTP {status}, code {}", str_value(&code))));
    }
    Ok(match payload.get("data") {
        Some(Value::Dict(data)) => data.clone(),
        _ => Dict::new(),
    })
}

fn truthy<'a>(data: &'a Dict, key: &str) -> Option<&'a Value> {
    data.get(key).filter(|value| value.is_truthy())
}

async fn new_ticket(client: &Client, endpoints: &Endpoints, cookies: &[StoredCookie], now: f64) -> Result<Vec<StoredCookie>, RenewError> {
    #[allow(clippy::cast_possible_truncation)] // reason: `int(now)`
    let ts = now.trunc() as i64;
    let params = [
        ("key_id", TICKET_KEY_ID.to_owned()),
        ("hexsign", ticket_hexsign(ts)),
        ("context[ts]", ts.to_string()),
        ("csrf", cookie_value(cookies, "bili_jct").to_owned()),
    ];
    let answer = client
        .request(Method::POST, &format!("{}{TICKET_PATH}", endpoints.api), cookies, &params, None)
        .await?;
    let data = json_data(&answer, "GenWebTicket")?;
    let ticket = truthy(&data, "ticket").map(str_value).unwrap_or_default();
    if ticket.is_empty() {
        return Err(RenewError::renewal("GenWebTicket: no ticket in the answer"));
    }
    let created = match truthy(&data, "created_at") {
        Some(value) => pydecode::py_int(value).map_err(RenewError::Python)?,
        None => i128::from(ts),
    };
    let ttl = match truthy(&data, "ttl") {
        Some(value) => pydecode::py_int(value).map_err(RenewError::Python)?,
        None => TICKET_DEFAULT_TTL,
    };
    #[allow(clippy::cast_precision_loss)] // reason: `float(created + ttl)`
    let expires = (created + ttl) as f64;
    let cookie = |name: &str, value: String| StoredCookie {
        name: name.to_owned(),
        value,
        domain: COOKIE_DOMAIN.to_owned(),
        path: "/".to_owned(),
        expires: Some(expires),
    };
    Ok(vec![
        cookie("bili_ticket", ticket),
        cookie("bili_ticket_expires", (created + ttl).to_string()),
    ])
}

async fn refresh_due(client: &Client, endpoints: &Endpoints, cookies: &[StoredCookie]) -> Result<bool, RenewError> {
    let params = [("csrf", cookie_value(cookies, "bili_jct").to_owned())];
    let answer = client
        .request(
            Method::GET,
            &format!("{}{COOKIE_INFO_PATH}", endpoints.passport),
            cookies,
            &params,
            None,
        )
        .await?;
    Ok(json_data(&answer, "cookie/info")?.get("refresh").is_some_and(Value::is_truthy))
}

async fn refresh(
    client: &Client,
    endpoints: &Endpoints,
    cookies: &[StoredCookie],
    refresh_token: &str,
    now: f64,
) -> Result<(Vec<StoredCookie>, String), RenewError> {
    #[allow(clippy::cast_possible_truncation)] // reason: `int(time.time() * 1000)`
    let now_ms = (now * 1000.0).trunc() as i64;
    let url = format!("{}/correspond/1/{}", endpoints.www, correspond_path(now_ms));
    let answer = client.request(Method::GET, &url, cookies, &[], None).await?;
    let refresh_csrf = if answer.status == HTTP_OK {
        // `response.text`: the page decoded as UTF-8 with replacement characters.
        let text = String::from_utf8_lossy(&answer.body);
        REFRESH_CSRF_PATTERN
            .captures(&text)
            .and_then(|captures| captures.get(1))
            .map(|csrf| csrf.as_str().to_owned())
    } else {
        None
    };
    let Some(refresh_csrf) = refresh_csrf else {
        return Err(RenewError::renewal(format!("correspond: HTTP {}, no refresh_csrf", answer.status)));
    };
    let form = [
        ("csrf", cookie_value(cookies, "bili_jct").to_owned()),
        ("refresh_csrf", refresh_csrf),
        ("source", "main_web".to_owned()),
        ("refresh_token", refresh_token.to_owned()),
    ];
    let answer = client
        .request(
            Method::POST,
            &format!("{}{REFRESH_PATH}", endpoints.passport),
            cookies,
            &[],
            Some(&form),
        )
        .await?;
    let data = json_data(&answer, "cookie/refresh")?;
    let new_token = truthy(&data, "refresh_token").map(str_value).unwrap_or_default();
    let updates = cookiejar::cookies_from_headers(REFRESH_URL, &answer.set_cookie, now);
    if new_token.is_empty() || !updates.iter().any(|cookie| cookie.name == "SESSDATA" && !cookie.value.is_empty()) {
        return Err(RenewError::renewal(
            "cookie/refresh: no new SESSDATA or refresh token in the answer",
        ));
    }
    Ok((merge_cookies(cookies, &updates, now), new_token))
}

async fn confirm(client: &Client, endpoints: &Endpoints, cookies: &[StoredCookie], old_token: &str) -> Result<(), RenewError> {
    let form = [
        ("csrf", cookie_value(cookies, "bili_jct").to_owned()),
        ("refresh_token", old_token.to_owned()),
    ];
    let answer = client
        .request(
            Method::POST,
            &format!("{}{CONFIRM_PATH}", endpoints.passport),
            cookies,
            &[],
            Some(&form),
        )
        .await?;
    json_data(&answer, "confirm/refresh").map(|_| ())
}

/// `renew_ticket`: a jar with a fresh `bili_ticket`, or `None` when the stored one has long
/// enough to go.
pub async fn renew_ticket(cookies: &[StoredCookie], endpoints: &Endpoints, now: f64) -> Result<Option<Vec<StoredCookie>>, RenewError> {
    if !ticket_due(cookies, now).map_err(RenewError::Python)? {
        return Ok(None);
    }
    let client = Client::new()?;
    let ticket = new_ticket(&client, endpoints, cookies, now).await?;
    Ok(Some(merge_cookies(cookies, &ticket, now)))
}

/// `refresh_session`: trade the refresh token for a new `SESSDATA` when Bilibili asks for
/// it; `None` when it does not.
///
/// `save(renewal)` runs between the refresh and its confirmation: once the refresh
/// answers, only the new cookies and token are any good, so they are stored before the
/// old pair is retired. A failed confirmation is only logged.
pub async fn refresh_session(
    cookies: &[StoredCookie],
    refresh_token: &str,
    endpoints: &Endpoints,
    now: f64,
    save: impl AsyncFnOnce(&Renewal) -> Result<(), JobError>,
) -> Result<Option<Renewal>, RenewError> {
    let client = Client::new()?;
    if !refresh_due(&client, endpoints, cookies).await? {
        return Ok(None);
    }
    let (new_cookies, new_token) = refresh(&client, endpoints, cookies, refresh_token, now).await?;
    let renewal = Renewal {
        cookies: new_cookies,
        refresh_token: new_token,
        renewed: vec!["SESSDATA".to_owned()],
    };
    save(&renewal).await.map_err(RenewError::Save)?;
    if let Err(error) = confirm(&client, endpoints, &renewal.cookies, refresh_token).await {
        // The new pair already works; an unconfirmed refresh only leaves the old one alive longer.
        log().warning(format_args!("Bilibili accepted the refresh but not its confirmation: {error}"));
    }
    Ok(Some(renewal))
}

#[cfg(test)]
mod tests;
