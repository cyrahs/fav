//! Kemono creator resolution: a pasted creator page URL (or a bare fanbox user id) as
//! the `{service, id, name}` stored in `web.kemono.creators`, the name from the site's
//! profile API.
//!
//! Python origin: `src/tool/kemono_creator.py`, used by the API's
//! `POST /api/v2/kemono/creators/resolve`. The crawl itself is [`crate::kemono`].

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use fav_core::pyjson::Value;
use fav_core::pytext::{py_decimal_value, py_isdecimal, py_strip};
use fav_core::pyurl::urlsplit;
use fav_db::SettingsStore;
use fav_net::{ClientOptions, HttpClient};
use fav_runtime::BoxFuture;
use fav_runtime::pystr::str_value;
use regex::Regex;

/// The browser `User-Agent` the resolver sends.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Bare numeric input defaults to fanbox; other services need the full URL.
pub const DEFAULT_SERVICE: &str = "fanbox";

static SERVICE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9]+$").expect("valid pattern"));
static CREATOR_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+$").expect("valid pattern"));

/// `extract_kemono_creator(raw)`: `(service, id)` from a creator page URL on any host
/// (only the `/{service}/user/{id}` path shape matters) or a bare positive id. An `Err`
/// is the `ValueError` of `urlsplit` or of `int()` (beyond 4300 digits), which the
/// resolver reports as invalid input too.
pub fn extract_kemono_creator(raw: &str) -> Result<Option<(String, String)>, String> {
    let text = py_strip(raw);
    if text.is_empty() {
        return Ok(None);
    }
    if py_isdecimal(text) {
        let digits = text.chars().count();
        if digits > INT_MAX_STR_DIGITS {
            return Err(format!(
                "Exceeds the limit ({INT_MAX_STR_DIGITS} digits) for integer string conversion: value has {digits} digits; use sys.set_int_max_str_digits() to increase the limit"
            ));
        }
        // `int(text) > 0`: a decimal string is positive when any digit is not zero.
        let positive = text.chars().any(|ch| py_decimal_value(ch).is_some_and(|digit| digit > 0));
        return Ok(positive.then(|| (DEFAULT_SERVICE.to_owned(), text.to_owned())));
    }
    let path = urlsplit(text, "").map_err(|error| error.0)?.path;
    let segments: Vec<&str> = path.split('/').filter(|segment| !segment.is_empty()).collect();
    for (index, segment) in segments.iter().enumerate() {
        if *segment != "user" || index == 0 || index + 1 >= segments.len() {
            continue;
        }
        let service = segments[index - 1].to_lowercase();
        let creator_id = segments[index + 1];
        if SERVICE.is_match(&service) && CREATOR_ID.is_match(creator_id) {
            return Ok(Some((service, creator_id.to_owned())));
        }
    }
    Ok(None)
}

/// `sys.get_int_max_str_digits()`: `int()` refuses longer decimal strings.
const INT_MAX_STR_DIGITS: usize = 4300;

/// A resolved creator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCreator {
    pub service: String,
    pub id: String,
    pub name: String,
}

/// Why a creator did not resolve, as the Python exception classes the API maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// `ValueError('invalid_creator')`: the input names no creator.
    Invalid,
    /// `LookupError('creator_not_found')`: the site answered 404.
    NotFound,
    /// `ConnectionError('creator_resolve_failed')`: no usable answer from the site.
    Unreachable,
    /// Anything else (the settings could not be read).
    Other(String),
}

/// What the API calls; tests put a fake behind it.
pub trait ResolveCreator: Send + Sync {
    fn resolve<'a>(&'a self, raw_creator: &'a str) -> BoxFuture<'a, Result<ResolvedCreator, ResolveError>>;
}

/// `KemonoCreatorResolver`: reads `web.kemono.base_url` on every call, so a settings
/// edit applies without a restart.
pub struct KemonoCreatorResolver {
    settings: Arc<SettingsStore>,
    http: HttpClient,
}

impl std::fmt::Debug for KemonoCreatorResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KemonoCreatorResolver").finish_non_exhaustive()
    }
}

/// `httpx.Client(timeout=30, follow_redirects=True, headers=...)`.
pub fn client_options() -> ClientOptions {
    ClientOptions {
        headers: vec![
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ("Accept".to_owned(), "application/json".to_owned()),
        ],
        timeout: Duration::from_secs(30),
        follow_redirects: true,
        ..ClientOptions::default()
    }
}

impl KemonoCreatorResolver {
    pub fn new(settings: Arc<SettingsStore>) -> Result<Self, String> {
        let http = HttpClient::new(&client_options()).map_err(|error| error.to_string())?;
        Ok(Self::with_client(settings, http))
    }

    pub fn with_client(settings: Arc<SettingsStore>, http: HttpClient) -> Self {
        Self { settings, http }
    }

    /// `resolve(raw_creator)`.
    pub async fn resolve_creator(&self, raw_creator: &str) -> Result<ResolvedCreator, ResolveError> {
        let Ok(Some((service, creator_id))) = extract_kemono_creator(raw_creator) else {
            return Err(ResolveError::Invalid);
        };
        let settings = self.settings.load().await.map_err(|error| ResolveError::Other(error.to_string()))?;
        let base_url = settings.web.kemono.base_url.trim_end_matches('/');
        let url = format!("{base_url}/api/v1/{service}/user/{creator_id}/profile");
        let response = self.http.get(&url).await.map_err(|_| ResolveError::Unreachable)?;
        match response.status() {
            200 => {}
            404 => return Err(ResolveError::NotFound),
            _ => return Err(ResolveError::Unreachable),
        }
        let body = response.bytes().await.map_err(|_| ResolveError::Unreachable)?;
        let profile = crate::pyerr::json_loads_bytes(&body).map_err(|_| ResolveError::Unreachable)?;
        let name = match &profile {
            Value::Dict(profile) => match profile.get("name").filter(|name| name.is_truthy()) {
                Some(name) => py_strip(&str_value(name)).to_owned(),
                None => String::new(),
            },
            _ => String::new(),
        };
        Ok(ResolvedCreator {
            name: if name.is_empty() { creator_id.clone() } else { name },
            service,
            id: creator_id,
        })
    }
}

impl ResolveCreator for KemonoCreatorResolver {
    fn resolve<'a>(&'a self, raw_creator: &'a str) -> BoxFuture<'a, Result<ResolvedCreator, ResolveError>> {
        Box::pin(self.resolve_creator(raw_creator))
    }
}

#[cfg(test)]
mod tests;
