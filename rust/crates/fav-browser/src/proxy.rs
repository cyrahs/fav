//! A proxy setting split the way Chromium needs it.
//!
//! Python origin: `build_proxy_settings` and `ProxyConfigurationError` in
//! `src/web/rednote_browser.py`. Chromium ignores userinfo in `--proxy-server`, so the
//! credentials travel beside the server; it cannot authenticate to a SOCKS proxy at all.
//! The `urlsplit` reads (`username`, `password`, `port`) follow `CPython` 3.12 exactly,
//! errors included, and `rust/goldens/browser_login/proxy_settings.json` holds Python's
//! answers.

use fav_core::pyurl::{self, SplitResult};
use fav_runtime::pystr::repr_str;

/// `{'server': ..., 'username': ..., 'password': ...}`; the credentials only when present.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxySettings {
    pub server: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl ProxySettings {
    /// Whether the proxy needs credentials (`username or password` in Python).
    pub fn has_credentials(&self) -> bool {
        self.username.as_deref().is_some_and(|name| !name.is_empty()) || self.password.as_deref().is_some_and(|word| !word.is_empty())
    }
}

impl std::fmt::Debug for ProxySettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxySettings")
            .field("server", &self.server)
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Why a proxy setting cannot be given to Chromium, with the Python exception class it
/// stands for: `ProxyConfigurationError` from the function itself, `ValueError` from
/// `urlsplit` or its `port`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ProxyError {
    pub class: &'static str,
    pub message: String,
}

impl ProxyError {
    fn configuration(message: String) -> Self {
        Self {
            class: "ProxyConfigurationError",
            message,
        }
    }

    fn value(message: String) -> Self {
        Self {
            class: "ValueError",
            message,
        }
    }
}

/// `build_proxy_settings(proxy)`: `None` for an empty setting.
pub fn build_proxy_settings(proxy: &str) -> Result<Option<ProxySettings>, ProxyError> {
    let raw = fav_core::pytext::py_strip(proxy);
    if raw.is_empty() {
        return Ok(None);
    }
    let dialable = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("http://{raw}")
    };
    let parts = pyurl::urlsplit(&dialable, "").map_err(|error| ProxyError::value(error.0))?;
    let host = parts.hostname().unwrap_or_default();
    if host.is_empty() {
        return Err(ProxyError::configuration(format!(
            "Could not read a host out of the proxy setting: {}",
            repr_str(proxy)
        )));
    }
    let username = username(&parts);
    let password = password(&parts);
    if parts.scheme.starts_with("socks")
        && (username.as_deref().is_some_and(|name| !name.is_empty()) || password.as_deref().is_some_and(|word| !word.is_empty()))
    {
        return Err(ProxyError::configuration(
            "Chromium cannot authenticate to a SOCKS proxy; use an http:// or https:// proxy instead.".to_owned(),
        ));
    }
    let server = match port(&parts)? {
        Some(port) if port != 0 => format!("{}://{host}:{port}", parts.scheme),
        _ => format!("{}://{host}", parts.scheme),
    };
    Ok(Some(ProxySettings {
        server,
        username: username.filter(|name| !name.is_empty()).map(|name| pyurl::unquote(&name)),
        password: password.filter(|word| !word.is_empty()).map(|word| pyurl::unquote(&word)),
    }))
}

/// `SplitResult._userinfo`: what precedes the last `@` of the netloc, split at its first `:`.
fn userinfo(parts: &SplitResult) -> Option<(String, Option<String>)> {
    let (info, _) = parts.netloc.rsplit_once('@')?;
    Some(match info.split_once(':') {
        Some((name, word)) => (name.to_owned(), Some(word.to_owned())),
        None => (info.to_owned(), None),
    })
}

fn username(parts: &SplitResult) -> Option<String> {
    userinfo(parts).map(|(name, _)| name)
}

fn password(parts: &SplitResult) -> Option<String> {
    userinfo(parts).and_then(|(_, word)| word)
}

/// `SplitResult.port`: `None` without one; a `ValueError` for one that is not an ASCII
/// decimal number or not in 0..=65535.
fn port(parts: &SplitResult) -> Result<Option<u32>, ProxyError> {
    let host_info = parts.netloc.rsplit_once('@').map_or(parts.netloc.as_str(), |(_, host)| host);
    let port = match host_info.split_once('[') {
        Some((_, bracketed)) => bracketed
            .split_once(']')
            .map_or("", |(_, rest)| rest)
            .split_once(':')
            .map_or("", |(_, port)| port),
        None => host_info.split_once(':').map_or("", |(_, port)| port),
    };
    if port.is_empty() {
        return Ok(None);
    }
    if !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ProxyError::value(format!(
            "Port could not be cast to integer value as {}",
            repr_str(port)
        )));
    }
    let digits = port.trim_start_matches('0');
    match digits.parse::<u32>() {
        Ok(value) if value <= 65_535 => Ok(Some(value)),
        _ if digits.is_empty() => Ok(Some(0)),
        _ => Err(ProxyError::value("Port out of range 0-65535".to_owned())),
    }
}

#[cfg(test)]
mod tests;
