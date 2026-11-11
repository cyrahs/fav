//! A browser-level DevTools connection that never touches a page.
//!
//! Python origin: `read_devtools_cookies`, `_devtools_call`, `read_devtools_local_storage`
//! and `_origin` in `src/api/browser_login.py`. The plain login window is a Chromium
//! nothing drives; fav only asks its browser target for the cookie jar
//! (`Storage.getCookies` needs no page session) and, once, opens a background tab of its
//! own to read one `localStorage` entry. The tab the user signs in with is never attached
//! to, and no domain is enabled on it.
//!
//! chromiumoxide is not used here on purpose: connecting it discovers every target and
//! attaches to each page to initialise it, which is exactly what the plain window must
//! not see. The messages are written by hand, byte for byte as Python's.

use std::time::{Duration, Instant};

use async_tungstenite::tungstenite::Message;
use async_tungstenite::tungstenite::protocol::WebSocketConfig;
use fav_net::{ClientOptions, HttpClient};
use futures_util::StreamExt as _;
use serde_json::{Map, Value, json};

/// A DevTools exchange failed. `class` is the Python exception it stands for, so a
/// failure message reads as Python's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DevToolsError {
    pub class: &'static str,
    pub message: String,
}

impl DevToolsError {
    fn new(class: &'static str, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }

    /// `LoginError`: DevTools answered with an error.
    fn refused(method: &str, error: &Value) -> Self {
        Self::new("LoginError", format!("DevTools refused {method}: {}", python_repr(error)))
    }

    fn timeout() -> Self {
        Self::new("TimeoutError", "")
    }
}

type Socket = async_tungstenite::WebSocketStream<async_tungstenite::tokio::ConnectStream>;

/// `GET http://127.0.0.1:<port>/json/version` (`trust_env=False`, 5 s), its
/// `webSocketDebuggerUrl`.
async fn browser_websocket_url(port: u16) -> Result<String, DevToolsError> {
    let client = HttpClient::new(&ClientOptions {
        trust_env: false,
        timeout: Duration::from_secs(5),
        ..ClientOptions::default()
    })
    .map_err(|error| DevToolsError::new("HTTPError", error.to_string()))?;
    let response = client
        .get(&format!("http://127.0.0.1:{port}/json/version"))
        .await
        .map_err(|error| DevToolsError::new(error.class.name(), error.message.clone()))?;
    let body = response
        .bytes()
        .await
        .map_err(|error| DevToolsError::new(error.class.name(), error.message.clone()))?;
    let version: Value = serde_json::from_slice(&body).map_err(|error| DevToolsError::new("JSONDecodeError", error.to_string()))?;
    match version.get("webSocketDebuggerUrl") {
        Some(Value::String(url)) => Ok(url.clone()),
        Some(other) => Ok(python_str(other)),
        None => Err(DevToolsError::new("KeyError", "'webSocketDebuggerUrl'")),
    }
}

/// `websockets.connect(url, max_size=None, proxy=None, open_timeout=5)`.
async fn connect(url: &str) -> Result<Socket, DevToolsError> {
    let config = WebSocketConfig::default().max_message_size(None).max_frame_size(None);
    let connected = tokio::time::timeout(
        Duration::from_secs(5),
        async_tungstenite::tokio::connect_async_with_config(url, Some(config)),
    )
    .await
    .map_err(|_| DevToolsError::timeout())?;
    let (socket, _) = connected.map_err(|error| DevToolsError::new("WebSocketException", error.to_string()))?;
    Ok(socket)
}

/// The next text message, within `timeout`, as JSON.
async fn receive(socket: &mut Socket, timeout: Duration) -> Result<Value, DevToolsError> {
    loop {
        let message = tokio::time::timeout(timeout, socket.next())
            .await
            .map_err(|_| DevToolsError::timeout())?
            .ok_or_else(|| DevToolsError::new("ConnectionClosedOK", "received 1000 (OK); then sent 1000 (OK)"))?
            .map_err(|error| DevToolsError::new("WebSocketException", error.to_string()))?;
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Message::Close(_) => return Err(DevToolsError::new("ConnectionClosedOK", "received 1000 (OK); then sent 1000 (OK)")),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
        };
        return serde_json::from_str(&text).map_err(|error| DevToolsError::new("JSONDecodeError", error.to_string()));
    }
}

async fn send(socket: &mut Socket, message: &Value) -> Result<(), DevToolsError> {
    socket
        .send(Message::text(message.to_string()))
        .await
        .map_err(|error| DevToolsError::new("WebSocketException", error.to_string()))
}

/// `read_devtools_cookies(port)`: the browser's whole cookie jar, asked of its
/// browser-level target. The reply is matched by its id; events in between are skipped.
pub async fn read_devtools_cookies(port: u16) -> Result<Vec<Value>, DevToolsError> {
    let url = browser_websocket_url(port).await?;
    let mut socket = connect(&url).await?;
    send(&mut socket, &json!({"id": 1, "method": "Storage.getCookies"})).await?;
    let reply = loop {
        let reply = receive(&mut socket, Duration::from_secs(5)).await?;
        if reply.get("id") == Some(&json!(1)) {
            break reply;
        }
    };
    let _ = socket.close(None).await;
    if let Some(error) = reply.get("error") {
        return Err(DevToolsError::refused("Storage.getCookies", error));
    }
    let cookies = reply.get("result").and_then(|result| result.get("cookies"));
    Ok(match cookies {
        Some(Value::Array(cookies)) => cookies.clone(),
        _ => Vec::new(),
    })
}

/// What `Page.navigate` answered: Chromium's `errorText` for a navigation it refused, and
/// the new document's `loaderId` (none for a same-document navigation).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Navigated {
    pub error_text: Option<String>,
    pub loader_id: Option<String>,
}

/// `Page.navigate` sent on a DevTools socket of the page target's own
/// (`{endpoint}/devtools/page/{target_id}`), answered once the navigation commits or
/// fails. chromiumoxide answers its own `Page.navigate` only after `load` has fired in
/// every frame, where Playwright's `goto(wait_until='domcontentloaded')` never waits for
/// `load`; a page whose subresources keep loading (RedNote through the home proxy,
/// 2026-11-11) then failed with chromiumoxide's 30 s request timeout. The socket has no
/// domain enabled, so nothing but the reply arrives on it; the driver's own session still
/// sees the navigation's events.
pub async fn navigate_page(endpoint: &str, target_id: &str, url: &str, timeout: Duration) -> Result<Navigated, DevToolsError> {
    let mut socket = connect(&format!("{endpoint}/devtools/page/{target_id}")).await?;
    let mut counter = 0;
    let result = devtools_call_within(&mut socket, &mut counter, "Page.navigate", json!({"url": url}), "", timeout).await;
    let _ = socket.close(None).await;
    let result = result?;
    let text = |key: &str| match result.get(key) {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    };
    Ok(Navigated {
        error_text: text("errorText"),
        loader_id: text("loaderId"),
    })
}

/// `_devtools_call(socket, counter, method, params, session)`: one command, its result.
async fn devtools_call(
    socket: &mut Socket,
    counter: &mut i64,
    method: &str,
    params: Value,
    session: &str,
) -> Result<Map<String, Value>, DevToolsError> {
    devtools_call_within(socket, counter, method, params, session, Duration::from_secs(10)).await
}

/// [`devtools_call`] waiting up to `timeout` for each message instead of 10 s.
async fn devtools_call_within(
    socket: &mut Socket,
    counter: &mut i64,
    method: &str,
    params: Value,
    session: &str,
    timeout: Duration,
) -> Result<Map<String, Value>, DevToolsError> {
    *counter += 1;
    let mut message = json!({"id": *counter, "method": method, "params": params});
    if !session.is_empty() {
        message["sessionId"] = json!(session);
    }
    send(socket, &message).await?;
    let reply = loop {
        let reply = receive(socket, timeout).await?;
        if reply.get("id") == Some(&json!(*counter)) {
            break reply;
        }
    };
    if let Some(error) = reply.get("error") {
        return Err(DevToolsError::refused(method, error));
    }
    Ok(match reply.get("result") {
        Some(Value::Object(result)) => result.clone(),
        _ => Map::new(),
    })
}

/// `_origin(url)`: `scheme://netloc`.
pub fn origin(url: &str) -> String {
    match fav_core::pyurl::urlsplit(url, "") {
        Ok(parts) => format!("{}://{}", parts.scheme, parts.netloc),
        Err(_) => String::new(),
    }
}

/// `read_devtools_local_storage(port, url, key, timeout=10)`: one `localStorage` entry of
/// `url`'s origin, read in a background tab opened for it (best a static file of the site,
/// such as `robots.txt`) and closed again. An entry that is not there is `''`.
pub async fn read_devtools_local_storage(port: u16, url: &str, key: &str, timeout: Duration) -> Result<String, DevToolsError> {
    let browser = browser_websocket_url(port).await?;
    let mut socket = connect(&browser).await?;
    let mut counter = 0;
    let created = devtools_call(
        &mut socket,
        &mut counter,
        "Target.createTarget",
        json!({"url": url, "background": true}),
        "",
    )
    .await?;
    let target = created
        .get("targetId")
        .cloned()
        .ok_or_else(|| DevToolsError::new("KeyError", "'targetId'"))?;
    let result = read_in_target(&mut socket, &mut counter, &target, url, key, timeout).await;
    // Closed whatever happened; a failure to close is not worth reporting over the read.
    let _ = devtools_call(&mut socket, &mut counter, "Target.closeTarget", json!({"targetId": target}), "").await;
    let _ = socket.close(None).await;
    result
}

async fn read_in_target(
    socket: &mut Socket,
    counter: &mut i64,
    target: &Value,
    url: &str,
    key: &str,
    timeout: Duration,
) -> Result<String, DevToolsError> {
    let attached = devtools_call(
        socket,
        counter,
        "Target.attachToTarget",
        json!({"targetId": target, "flatten": true}),
        "",
    )
    .await?;
    let session = match attached.get("sessionId") {
        Some(Value::String(session)) => session.clone(),
        Some(other) => python_str(other),
        None => return Err(DevToolsError::new("KeyError", "'sessionId'")),
    };
    let expression = format!(
        "document.readyState === 'complete' && location.origin === {} ? (localStorage.getItem({}) || \"\") : null",
        python_json_string(&origin(url)),
        python_json_string(key),
    );
    let deadline = Instant::now() + timeout;
    loop {
        let result = devtools_call(
            socket,
            counter,
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true}),
            &session,
        )
        .await?;
        let value = result.get("result").and_then(|result| result.get("value"));
        if let Some(Value::String(found)) = value {
            return Ok(found.clone());
        }
        if Instant::now() >= deadline {
            return Ok(String::new());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// `json.dumps(text)` (ASCII-escaped, as Python's default).
fn python_json_string(text: &str) -> String {
    fav_core::pyjson::dumps(
        &fav_core::pyjson::Value::Str(text.to_owned()),
        &fav_core::pyjson::DumpOptions::python_default(),
    )
    .unwrap_or_else(|_| format!("{text:?}"))
}

/// `str(value)` of a value `json.loads` produced.
fn python_str(value: &Value) -> String {
    fav_runtime::pystr::str_value(&fav_core::pyjson::Value::from_serde(value))
}

/// `repr(value)` of a value `json.loads` produced (the `{reply["error"]}` in an f-string).
fn python_repr(value: &Value) -> String {
    fav_runtime::pystr::str_value(&fav_core::pyjson::Value::from_serde(value))
}

#[cfg(test)]
mod tests;
