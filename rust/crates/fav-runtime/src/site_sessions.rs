//! Signed-in site sessions captured on the 登录态 page, stored for the crawlers.
//!
//! Python origin: `src/tool/site_sessions.py`. The cookies come from a one-off browser
//! the user signs in to from the web UI; once the login is captured the browser is closed
//! again, and the crawler reads the cookies back from this table and records whether they
//! still worked, which is what the page shows next to each site.
//!
//! Cookie values are credentials. Nothing here logs them, and [`StoredCookie`]'s `Debug`
//! leaves them out. The table comes from [`fav_db::Db::ensure_schema`] (DIVERGENCES.md D6).

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use fav_core::pyjson::{self, Dict, DumpOptions, Value};
use fav_db::Db;
use sqlx::Row as _;
use sqlx::postgres::PgRow;

use crate::Error;
use crate::pystr::{str_key, str_value};

pub const STATUS_VALID: &str = "valid";
pub const STATUS_INVALID: &str = "invalid";

const MESSAGE_MAX_CHARS: usize = 500;
/// What a cookie's value is stored as for a site whose session lives in its browser
/// profile (RedNote): the row says which cookies exist and when they expire, and the
/// credential itself stays in the profile only.
pub const VALUE_IN_PROFILE: &str = "in-profile";

/// One stored cookie.
#[derive(Clone, PartialEq)]
pub struct StoredCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Unix seconds; `None` for a cookie that ends with the browser session.
    pub expires: Option<f64>,
}

impl fmt::Debug for StoredCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredCookie")
            .field("name", &self.name)
            .field("value", &if self.value.is_empty() { "" } else { "<redacted>" })
            .field("domain", &self.domain)
            .field("path", &self.path)
            .field("expires", &self.expires)
            .finish()
    }
}

/// One row of `site_sessions`.
#[derive(Clone, PartialEq)]
pub struct SiteSession {
    pub site: String,
    pub cookies: Vec<StoredCookie>,
    pub account: String,
    pub status: String,
    pub message: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub signed_in_at: Option<DateTime<Utc>>,
    pub checked_at: Option<DateTime<Utc>>,
    /// Site-specific secrets that are not cookies, such as Bilibili's refresh token
    /// (`ac_time_value` in the page's localStorage), in stored order. Never shown by any view.
    pub extra: Vec<(String, String)>,
}

impl fmt::Debug for SiteSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SiteSession")
            .field("site", &self.site)
            .field("cookies", &self.cookies)
            .field("account", &self.account)
            .field("status", &self.status)
            .field("message", &self.message)
            .field("expires_at", &self.expires_at)
            .field("signed_in_at", &self.signed_in_at)
            .field("checked_at", &self.checked_at)
            .field("extra", &self.extra.iter().map(|(key, _)| key).collect::<Vec<_>>())
            .finish()
    }
}

impl SiteSession {
    /// The value of one cookie, matched case-insensitively; empty when absent.
    pub fn cookie(&self, name: &str) -> &str {
        cookie_value(&self.cookies, name)
    }

    /// One `extra` entry, if stored.
    pub fn extra_value(&self, key: &str) -> Option<&str> {
        self.extra.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
    }

    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            site: row.try_get("site")?,
            cookies: decode_cookies(&row.try_get::<String, _>("cookies")?),
            account: row.try_get("account")?,
            status: row.try_get("status")?,
            message: row.try_get("message")?,
            expires_at: row.try_get("expires_at")?,
            signed_in_at: row.try_get("signed_in_at")?,
            checked_at: row.try_get("checked_at")?,
            extra: decode_extra(&row.try_get::<String, _>("extra")?),
        })
    }
}

/// `cookie_value`: the first non-empty value of the cookie named `name`, any case.
pub fn cookie_value<'a>(cookies: &'a [StoredCookie], name: &str) -> &'a str {
    let wanted = name.to_lowercase();
    cookies
        .iter()
        .find(|cookie| cookie.name.to_lowercase() == wanted && !cookie.value.is_empty())
        .map_or("", |cookie| cookie.value.as_str())
}

/// `isinstance(value, int | float)` as a float: a Python `bool` is an `int` too.
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
        // An `i128` from JSON fits an f64 the way Python's `float(int)` rounds it.
        #[allow(clippy::cast_precision_loss)] // reason: Python's float(int) rounds the same way
        Value::Int(int) => Some(*int as f64),
        Value::Float(float) => Some(*float),
        _ => None,
    }
}

/// `str(item.get(key) or default)`.
fn str_or(item: &Dict, key: &str, default: &str) -> String {
    match item.get(key) {
        Some(value) if value.is_truthy() => str_value(value),
        _ => default.to_owned(),
    }
}

/// `cookies_from_browser`: Playwright's cookie dicts as stored cookies; `expires == -1`
/// (anything not positive) means a session cookie.
pub fn cookies_from_browser(raw: &[Value]) -> Vec<StoredCookie> {
    raw.iter()
        .filter_map(Value::as_dict)
        .filter_map(|item| {
            let name = str_or(item, "name", "");
            if name.is_empty() {
                return None;
            }
            Some(StoredCookie {
                name,
                value: str_or(item, "value", ""),
                domain: str_or(item, "domain", ""),
                path: str_or(item, "path", "/"),
                expires: number(item.get("expires")).filter(|expires| *expires > 0.0),
            })
        })
        .collect()
}

/// `without_values`: each value replaced by [`VALUE_IN_PROFILE`]; names and expiry kept.
pub fn without_values(cookies: &[StoredCookie]) -> Vec<StoredCookie> {
    cookies
        .iter()
        .map(|cookie| StoredCookie {
            value: if cookie.value.is_empty() {
                String::new()
            } else {
                VALUE_IN_PROFILE.to_owned()
            },
            ..cookie.clone()
        })
        .collect()
}

/// `datetime.fromtimestamp(seconds, tz=UTC)`: microseconds rounded half to even.
fn from_timestamp(seconds: f64) -> Option<DateTime<Utc>> {
    let mut whole = seconds.trunc();
    let mut micros = ((seconds - whole) * 1e6).round_ties_even();
    if micros >= 1e6 {
        micros -= 1e6;
        whole += 1.0;
    } else if micros < 0.0 {
        micros += 1e6;
        whole -= 1.0;
    }
    if !whole.is_finite() || whole.abs() > 1e15 {
        return None;
    }
    // Both are whole numbers in range now.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // reason: whole, within 1e15, and micros in 0..1e6 just above
    let (whole, micros) = (whole as i64, micros as u32);
    DateTime::from_timestamp(whole, micros * 1000)
}

/// `cookie_expiry`: when the named cookie expires, or `None` for a session cookie or a
/// missing one.
pub fn cookie_expiry(cookies: &[StoredCookie], name: &str) -> Option<DateTime<Utc>> {
    let wanted = name.to_lowercase();
    cookies
        .iter()
        .find(|cookie| cookie.name.to_lowercase() == wanted && !cookie.value.is_empty() && cookie.expires.is_some())
        .and_then(|cookie| from_timestamp(cookie.expires?))
}

/// `to_netscape`: the cookies as a Netscape cookies.txt, for a downloader that reads its
/// session from a file. The secure flag is not stored, so every cookie is written as
/// secure: the sites these sessions belong to are only ever reached over HTTPS.
pub fn to_netscape(cookies: &[StoredCookie]) -> String {
    let mut lines = vec!["# Netscape HTTP Cookie File".to_owned(), String::new()];
    for cookie in cookies {
        if cookie.value.is_empty() || cookie.domain.is_empty() {
            continue;
        }
        let include_subdomains = if cookie.domain.starts_with('.') { "TRUE" } else { "FALSE" };
        // `str(int(expires)) if expires else '0'`.
        let expires = match cookie.expires {
            // `+ 0.0` turns the -0.0 that truncating -0.5 gives into 0.0: `int()` has no sign for zero.
            Some(expires) if expires != 0.0 => format!("{:.0}", expires.trunc() + 0.0),
            _ => "0".to_owned(),
        };
        let path = if cookie.path.is_empty() { "/" } else { cookie.path.as_str() };
        lines.push(
            [
                cookie.domain.as_str(),
                include_subdomains,
                path,
                "TRUE",
                &expires,
                &cookie.name,
                &cookie.value,
            ]
            .join("\t"),
        );
    }
    lines.join("\n") + "\n"
}

/// `_encode_cookies`: the stored JSON (`ensure_ascii=False`).
pub fn encode_cookies(cookies: &[StoredCookie]) -> String {
    let items = cookies
        .iter()
        .map(|cookie| {
            let mut item = Dict::new();
            item.insert("name", Value::from(cookie.name.as_str()));
            item.insert("value", Value::from(cookie.value.as_str()));
            item.insert("domain", Value::from(cookie.domain.as_str()));
            item.insert("path", Value::from(cookie.path.as_str()));
            item.insert("expires", cookie.expires.map_or(Value::Null, Value::Float));
            Value::Dict(item)
        })
        .collect();
    // Only strings, floats and null: nothing json.dumps can refuse.
    pyjson::dumps(&Value::List(items), &DumpOptions::plain()).unwrap_or_default()
}

fn loads_or_empty(raw: &str) -> Option<Value> {
    if raw.is_empty() { None } else { pyjson::loads(raw).ok() }
}

/// `_decode_cookies`: what the column holds, forgiving anything malformed.
pub fn decode_cookies(raw: &str) -> Vec<StoredCookie> {
    let Some(Value::List(items)) = loads_or_empty(raw) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(Value::as_dict)
        .filter(|item| item.get("name").is_some_and(Value::is_truthy))
        .map(|item| StoredCookie {
            name: item.get("name").map(str_value).unwrap_or_default(),
            value: str_or(item, "value", ""),
            domain: str_or(item, "domain", ""),
            path: str_or(item, "path", "/"),
            expires: number(item.get("expires")),
        })
        .collect()
}

/// `_decode_extra`: the non-empty entries, as strings.
pub fn decode_extra(raw: &str) -> Vec<(String, String)> {
    let Some(Value::Dict(dict)) = loads_or_empty(raw) else {
        return Vec::new();
    };
    dict.iter()
        .filter(|(_, value)| value.is_truthy())
        .map(|(key, value)| (str_key(key), str_value(value)))
        .collect()
}

/// `json.dumps(extra or {})`.
fn encode_extra(extra: &[(String, String)]) -> String {
    let mut dict = Dict::new();
    for (key, value) in extra {
        dict.insert(key.as_str(), Value::from(value.as_str()));
    }
    pyjson::dumps(&Value::Dict(dict), &DumpOptions::python_default()).unwrap_or_default()
}

/// `load(site)`.
pub async fn load(db: &Db, site: &str) -> Result<Option<SiteSession>, Error> {
    let row = sqlx::query("SELECT * FROM site_sessions WHERE site = $1;")
        .bind(site)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.as_ref().map(SiteSession::from_row).transpose()?)
}

/// `load_all()`: every stored session by site.
pub async fn load_all(db: &Db) -> Result<BTreeMap<String, SiteSession>, Error> {
    let rows = sqlx::query("SELECT * FROM site_sessions ORDER BY site;")
        .fetch_all(db.pool())
        .await?;
    rows.iter()
        .map(|row| {
            let session = SiteSession::from_row(row)?;
            Ok((session.site.clone(), session))
        })
        .collect()
}

/// `save_signed_in`: replace a site's session with a freshly verified login.
pub async fn save_signed_in(
    db: &Db,
    site: &str,
    cookies: &[StoredCookie],
    account: &str,
    expires_at: Option<DateTime<Utc>>,
    extra: &[(String, String)],
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO site_sessions (site, cookies, account, status, message, expires_at, signed_in_at, checked_at, extra)
         VALUES ($1, $2, $3, $4, '', $5, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, $6)
         ON CONFLICT (site) DO UPDATE SET
             cookies = EXCLUDED.cookies,
             account = EXCLUDED.account,
             status = EXCLUDED.status,
             message = '',
             expires_at = EXCLUDED.expires_at,
             signed_in_at = EXCLUDED.signed_in_at,
             checked_at = EXCLUDED.checked_at,
             extra = EXCLUDED.extra;",
    )
    .bind(site)
    .bind(encode_cookies(cookies))
    .bind(account)
    .bind(STATUS_VALID)
    .bind(expires_at)
    .bind(encode_extra(extra))
    .execute(db.pool())
    .await?;
    Ok(())
}

/// `replace_cookies`: store cookies a crawler renewed itself, keeping who signed in and when.
pub async fn replace_cookies(
    db: &Db,
    site: &str,
    cookies: &[StoredCookie],
    expires_at: Option<DateTime<Utc>>,
    extra: &[(String, String)],
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE site_sessions
         SET cookies = $1, expires_at = $2, extra = $3, status = $4, message = '', checked_at = CURRENT_TIMESTAMP
         WHERE site = $5;",
    )
    .bind(encode_cookies(cookies))
    .bind(expires_at)
    .bind(encode_extra(extra))
    .bind(STATUS_VALID)
    .bind(site)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// `record_check`: whether the stored session still worked, as a crawl or a manual check
/// found it. The message is cut to 500 characters.
pub async fn record_check(db: &Db, site: &str, ok: bool, message: &str) -> Result<(), Error> {
    let message: String = message.chars().take(MESSAGE_MAX_CHARS).collect();
    sqlx::query("UPDATE site_sessions SET status = $1, message = $2, checked_at = CURRENT_TIMESTAMP WHERE site = $3;")
        .bind(if ok { STATUS_VALID } else { STATUS_INVALID })
        .bind(message)
        .bind(site)
        .execute(db.pool())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
