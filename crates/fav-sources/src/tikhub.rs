//! The TikHub account behind `services.tikhub`: whether its key works, and what is left on it.
//!
//! Python origin: `src/tool/tikhub.py`.
//!
//! TikHub (<https://tikhub.io>) bills each lookup against one balance (视频号 videos in
//! WeChat; Zhihu posts are read in the 知乎 profile since D187), so the settings page
//! checks the key and shows that balance in one place.

use std::time::Duration;

use fav_core::pyjson::Value;
use fav_core::settings::TikHub;
use fav_net::{ClientOptions, HttpClient, Request};

use crate::post_links::Fetched;
use crate::pydecode::json_from_bytes;
use crate::pyval::{float, py_str, str_or_empty};

const USER_INFO: &str = "/api/v1/tikhub/user/get_user_info";
/// TikHub's answers to a missing, invalid or spent key.
pub const ACCOUNT_STATUSES: [u16; 3] = [401, 402, 403];
/// How the message of the wrapper TikHub puts around a request it could not carry out starts.
pub const REQUEST_FAILED: &str = "Request failed. Please retry";

/// Why an answer without what was asked for is TikHub's failure and not the request's: a
/// rate limit, a server error, or the wrapper TikHub puts around a request it could not
/// carry out (`{"detail": {"code": 400, "message": "Request failed. Please retry. ..."}}`,
/// which it sends for every lookup while one of its APIs is down). `None` for any other
/// answer, such as a post that is gone. Whoever looks something up through TikHub asks
/// this, so that what it stops can wait the outage out ([`crate::link_retry`],
/// DIVERGENCES.md D185).
pub fn outage_reason(status: u16, detail: Option<&Value>) -> Option<String> {
    let message = detail.map(|detail| match detail {
        Value::Dict(fields) => fields
            .get("message")
            .filter(|message| message.is_truthy())
            .map_or_else(|| py_str(detail), py_str),
        _ => py_str(detail),
    });
    if status == 429 || status >= 500 {
        return Some(message.unwrap_or_else(|| "no data".to_owned()));
    }
    message.filter(|message| message.starts_with(REQUEST_FAILED))
}

/// [`outage_reason`] of an answer's body, which need not be JSON.
pub fn body_outage_reason(status: u16, body: &[u8]) -> Option<String> {
    let payload = json_from_bytes(body).ok();
    let detail = match &payload {
        Some(Value::Dict(payload)) => payload.get("detail").filter(|detail| detail.is_truthy()),
        _ => None,
    };
    outage_reason(status, detail)
}

/// `TikHubError`, with the code the API answers with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct TikHubError {
    pub message: String,
    /// `tikhub_not_configured`, `tikhub_unreachable` or `tikhub_key_refused`.
    pub code: &'static str,
}

impl TikHubError {
    fn new(message: impl Into<String>, code: &'static str) -> Self {
        Self {
            message: message.into(),
            code,
        }
    }
}

/// `TikHubAccount`.
#[derive(Debug, Clone, PartialEq)]
pub struct TikHubAccount {
    pub key_name: String,
    pub balance: f64,
    pub free_credit: f64,
}

/// `_number`: `float(value)`, or 0.0 where it raises.
fn number(value: Option<&Value>) -> f64 {
    value.and_then(|value| float(value).ok()).unwrap_or(0.0)
}

/// `check_account`: ask TikHub who the key belongs to; free, and billed to nothing.
pub async fn check_account(cfg: &TikHub) -> Result<TikHubAccount, TikHubError> {
    check_account_via(cfg, None).await
}

/// [`check_account`] through a proxy, for tests.
pub async fn check_account_via(cfg: &TikHub, proxy: Option<&str>) -> Result<TikHubAccount, TikHubError> {
    if cfg.token.is_empty() {
        return Err(TikHubError::new("Save a TikHub API key first.", "tikhub_not_configured"));
    }
    let unreachable =
        |class: &str, message: &str| TikHubError::new(format!("TikHub did not answer: {class}: {message}"), "tikhub_unreachable");
    let client = HttpClient::new(&ClientOptions {
        timeout: Duration::from_secs(30),
        proxy: proxy.map(str::to_owned),
        ..ClientOptions::default()
    })
    .map_err(|error| unreachable("UnsupportedProtocol", &error.to_string()))?;
    let request = Request::get(format!("{}{USER_INFO}", cfg.api_url)).header("Authorization", format!("Bearer {}", cfg.token));
    // An `api_url` httpx cannot parse (`InvalidURL`, not an `httpx.HTTPError`) is reported
    // the same way (D127).
    let fetched = Fetched::send(&client, request)
        .await
        .map_err(|error| unreachable(&error.class, &error.message))?;
    if ACCOUNT_STATUSES.contains(&fetched.status) {
        return Err(TikHubError::new(
            format!("TikHub refused the key (HTTP {}).", fetched.status),
            "tikhub_key_refused",
        ));
    }
    let payload = json_from_bytes(&fetched.body).ok();
    let payload = match &payload {
        Some(Value::Dict(payload)) => Some(payload),
        _ => None,
    };
    let user = payload.and_then(|payload| payload.get("user_data")).and_then(Value::as_dict);
    let (Some(payload), Some(user)) = (payload.filter(|_| fetched.status == 200), user) else {
        return Err(TikHubError::new(
            format!("TikHub answered HTTP {} without account details.", fetched.status),
            "tikhub_unreachable",
        ));
    };
    let key_name = match payload.get("api_key_data") {
        Some(Value::Dict(key)) => str_or_empty(key.get("api_key_name")),
        _ => String::new(),
    };
    Ok(TikHubAccount {
        key_name,
        balance: number(user.get("balance")),
        free_credit: number(user.get("free_credit")),
    })
}
