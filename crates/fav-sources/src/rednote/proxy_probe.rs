//! The settings form's RedNote proxy test.
//!
//! Python origin: `probe_proxy`, `_run_proxy_probe` and `_probe_exit_ip` in
//! `src/web/rednote_browser.py`. The request is anonymous: no profile, no cookies, nothing a
//! session could be lost over. It answers whether the address is one the site serves or
//! one it walls off, and shows the exit address, which is how an operator tells a home
//! line from a datacenter range. Chromium is not launched, so it proves the address, not
//! the browser; the proxy string is still parsed exactly the way Chromium needs it.

use std::time::Duration;

use fav_browser::proxy::build_proxy_settings;
use fav_browser::rednote::note_browser::RISK_CONTROL_STATUS_CODES;
use fav_core::pytext::py_strip;
use fav_net::{ClientOptions, ErrorClass, HttpClient};
use fav_runtime::BoxFuture;
use fav_runtime::pystr::repr_str;

use crate::azurlane::l2d_sources::fetch::Fetch;

const WEB_ORIGIN: &str = fav_browser::rednote::WEB_ORIGIN;
/// Short, because an operator is watching it spin.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_IP_TIMEOUT: Duration = Duration::from_secs(15);
const EXIT_IP_URL: &str = "https://checkip.amazonaws.com";
const HTTP_ERROR_MIN: u16 = 400;
/// httpx drops a port that is its scheme's default when it normalises a URL.
const HTTPX_DEFAULT_PORTS: [(&str, &str); 5] = [("ftp", "21"), ("http", "80"), ("https", "443"), ("ws", "80"), ("wss", "443")];

/// `ProxyProbe`: what the form shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyProbe {
    pub ok: bool,
    pub code: String,
    pub message: String,
    pub exit_ip: String,
    /// Nothing was configured and the request left on the pod's own address. A reachable
    /// direct egress is still only usable with `allow_direct_connection`.
    pub direct: bool,
}

fn answer(ok: bool, code: &str, message: String, exit_ip: String, direct: bool) -> ProxyProbe {
    ProxyProbe {
        ok,
        code: code.to_owned(),
        message,
        exit_ip,
        direct,
    }
}

/// `repr(httpx.URL(url))`: lowercase scheme and host, a default port dropped, and the
/// password masked as `[secure]`. httpx's percent-encoding and IDNA normalisation are not
/// reproduced (DIVERGENCES.md D177).
pub fn httpx_url_repr(url: &str) -> String {
    let Ok(parts) = fav_core::pyurl::urlsplit(url, "") else {
        return format!("URL({})", repr_str(url));
    };
    let (userinfo, host_info) = match parts.netloc.rsplit_once('@') {
        Some((userinfo, host_info)) => (Some(userinfo), host_info),
        None => (None, parts.netloc.as_str()),
    };
    let (host, port) = match host_info.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, Some(port)),
        _ => (host_info, None),
    };
    let port = port.filter(|port| {
        !port.is_empty()
            && !HTTPX_DEFAULT_PORTS
                .iter()
                .any(|(scheme, default)| *scheme == parts.scheme && port.trim_start_matches('0') == *default)
    });
    let mut authority = String::new();
    if let Some(userinfo) = userinfo.filter(|userinfo| !userinfo.is_empty()) {
        if let Some((name, _)) = userinfo.split_once(':') {
            authority.push_str(name);
            authority.push_str(":[secure]@");
        } else {
            authority.push_str(userinfo);
            authority.push('@');
        }
    }
    authority.push_str(&host.to_lowercase());
    if let Some(port) = port {
        authority.push(':');
        authority.push_str(port.trim_start_matches('0'));
    }
    let mut rendered = String::new();
    if !parts.scheme.is_empty() {
        rendered.push_str(&parts.scheme);
        rendered.push(':');
    }
    if !authority.is_empty() {
        rendered.push_str("//");
        rendered.push_str(&authority);
    }
    rendered.push_str(&parts.path);
    if !parts.query.is_empty() {
        rendered.push('?');
        rendered.push_str(&parts.query);
    }
    if !parts.fragment.is_empty() {
        rendered.push('#');
        rendered.push_str(&parts.fragment);
    }
    format!("URL({})", repr_str(&rendered))
}

/// httpx's `Proxy(url)` check: `Err` is its `ValueError` text for a scheme it cannot dial.
pub fn check_httpx_proxy(proxy: &str) -> Result<(), String> {
    if fav_net::proxy::is_httpx_proxy(proxy) {
        Ok(())
    } else {
        Err(format!("Unknown scheme for proxy URL {}", httpx_url_repr(proxy)))
    }
}

/// The probe's own client: through `dialable`, never through the environment's proxy
/// (Chromium does not read `HTTP_PROXY` either, so an empty setting must report the
/// pod's own egress), with connection reuse off.
fn probe_client_options(dialable: Option<&str>) -> ClientOptions {
    ClientOptions {
        headers: vec![("Accept-Language".to_owned(), "zh-CN,zh;q=0.9".to_owned())],
        timeout: PROBE_TIMEOUT,
        follow_redirects: true,
        proxy: dialable.map(str::to_owned),
        max_keepalive_connections: 0,
        trust_env: false,
        ..ClientOptions::default()
    }
}

/// `build_proxy_settings` as the probe runs it first: a string Chromium cannot use is the
/// probe's `invalid` answer (an authenticated SOCKS proxy would otherwise connect as
/// nobody), and `Err` is the `ValueError` Python lets out of the function (a port that is
/// not a number, say), which the API turns into a 500.
fn refuse_for_chromium(raw: &str) -> Result<Option<ProxyProbe>, String> {
    match build_proxy_settings(raw) {
        Ok(_) => Ok(None),
        Err(error) if error.class == "ProxyConfigurationError" => Ok(Some(answer(false, "invalid", error.message, String::new(), false))),
        Err(error) => Err(error.message),
    }
}

/// `probe_proxy(proxy)`: check the egress the source would use, without saving it and
/// without the account.
pub async fn probe_proxy(proxy: &str) -> Result<ProxyProbe, String> {
    let raw = py_strip(proxy);
    if let Some(refused) = refuse_for_chromium(raw)? {
        return Ok(refused);
    }
    // Chromium infers a scheme where httpx demands one, and `build_proxy_settings`
    // infers the same one, so the address dialled here is the address a run would use.
    let dialable = (!raw.is_empty()).then(|| {
        if raw.contains("://") {
            raw.to_owned()
        } else {
            format!("http://{raw}")
        }
    });
    let refused = |message: String| answer(false, "invalid", format!("Not a usable proxy URL: {message}"), String::new(), false);
    if let Some(dialable) = &dialable
        && let Err(message) = check_httpx_proxy(dialable)
    {
        // Chromium would refuse it too, so it is named here rather than surfacing as a 500.
        return Ok(refused(message));
    }
    match HttpClient::new(&probe_client_options(dialable.as_deref())) {
        Ok(client) => Ok(run_proxy_probe(&client, raw.is_empty()).await),
        Err(error) => Ok(refused(error.to_string())),
    }
}

/// `probe_proxy(proxy, client=client)`: the same checks, over a client the caller made.
pub async fn probe_proxy_through(proxy: &str, client: &dyn Fetch) -> Result<ProxyProbe, String> {
    let raw = py_strip(proxy);
    if let Some(refused) = refuse_for_chromium(raw)? {
        return Ok(refused);
    }
    Ok(run_proxy_probe(client, raw.is_empty()).await)
}

/// `_run_proxy_probe(client, direct=...)`.
pub async fn run_proxy_probe(client: &dyn Fetch, direct: bool) -> ProxyProbe {
    // Asked first, and not only for the readout: whether this call got through is what
    // tells a dead proxy apart from a working proxy the site will not answer.
    let exit_ip = probe_exit_ip(client).await;
    let response = match client.request("GET", WEB_ORIGIN, None).await {
        Ok(response) => response,
        Err(error) if error.class == ErrorClass::ProxyError => {
            return answer(
                false,
                "proxy_error",
                format!("The proxy refused the connection: {}", error.message),
                String::new(),
                direct,
            );
        }
        Err(error) if !direct && exit_ip.is_empty() => {
            // Everything goes through the proxy, and nothing has come back through it.
            return answer(
                false,
                "proxy_error",
                format!("Could not reach the proxy: {}", error.message),
                String::new(),
                direct,
            );
        }
        Err(error) => {
            return answer(
                false,
                "unreachable",
                format!("Could not reach {WEB_ORIGIN}: {}", error.message),
                exit_ip,
                direct,
            );
        }
    };
    if RISK_CONTROL_STATUS_CODES.contains(&i64::from(response.status)) {
        let message = format!(
            "The site answered HTTP {}: this address is behind the captcha wall.",
            response.status
        );
        return answer(false, "risk_control", message, exit_ip, direct);
    }
    if response.status >= HTTP_ERROR_MIN {
        let message = format!("{WEB_ORIGIN} returned HTTP {}.", response.status);
        return answer(false, "http_error", message, exit_ip, direct);
    }
    let how = if direct { "directly" } else { "through the proxy" };
    answer(true, "ok", format!("Reached {WEB_ORIGIN} {how}."), exit_ip, direct)
}

/// `_probe_exit_ip`: best-effort exit address.
async fn probe_exit_ip(client: &dyn Fetch) -> String {
    match client.request("GET", EXIT_IP_URL, Some(EXIT_IP_TIMEOUT)).await {
        Ok(response) if response.status < HTTP_ERROR_MIN => py_strip(&response.text()).to_owned(),
        _ => String::new(),
    }
}

/// [`probe_proxy`] as a service the API holds, so its tests can stand in for it.
pub trait ProbeRedNoteProxy: Send + Sync {
    fn probe<'a>(&'a self, proxy: &'a str) -> BoxFuture<'a, Result<ProxyProbe, String>>;
}

/// The real probe.
#[derive(Debug, Clone, Copy, Default)]
pub struct RedNoteProxyProbe;

impl ProbeRedNoteProxy for RedNoteProxyProbe {
    fn probe<'a>(&'a self, proxy: &'a str) -> BoxFuture<'a, Result<ProxyProbe, String>> {
        Box::pin(probe_proxy(proxy))
    }
}

#[cfg(test)]
mod tests;
