//! The VNC relay's WebSocket: `GET /api/v2/site-sessions/vnc?ticket=...` upgraded.
//!
//! Python origin: `site_login_vnc` in `src/api/app.py` and the ticket check of
//! `bridge_vnc` in `src/api/browser_login.py`, as uvicorn serves them. The route sits
//! outside the API router on purpose: a browser cannot put a bearer token on a WebSocket,
//! so it is gated by the ticket of the login it belongs to. A WebSocket closed before it
//! is accepted is answered `403` by uvicorn, which is what a wrong ticket, a VNC port that
//! is not listening, and a WebSocket to any other path get. Once accepted, the relay is
//! `fav_browser::vnc::bridge`.

use std::sync::Arc;

use async_tungstenite::WebSocketStream;
use async_tungstenite::tokio::TokioAdapter;
use async_tungstenite::tungstenite::handshake::derive_accept_key;
use async_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use fav_core::logging;

use crate::ApiState;

/// `f'{API_V2_PREFIX}/site-sessions/vnc'`.
pub(crate) const VNC_PATH: &str = "/api/v2/site-sessions/vnc";

/// uvicorn's default `ws_max_size`.
const WS_MAX_SIZE: usize = 16 * 1024 * 1024;

fn header_tokens(headers: &HeaderMap, name: header::HeaderName) -> impl Iterator<Item = String> + '_ {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
}

/// uvicorn's test for a WebSocket request: `Upgrade: websocket` with `Connection: upgrade`.
pub(crate) fn is_websocket(headers: &HeaderMap) -> bool {
    let upgrade = headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("websocket"));
    upgrade && header_tokens(headers, header::CONNECTION).any(|token| token.eq_ignore_ascii_case("upgrade"))
}

fn status(code: StatusCode, body: &'static str) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = code;
    if !body.is_empty() {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    }
    response
}

/// What uvicorn answers a WebSocket the app closed without accepting.
fn refused() -> Response {
    status(StatusCode::FORBIDDEN, "")
}

/// Answer a WebSocket request: refuse it, or switch protocols and relay it to the
/// running login's VNC port.
pub(crate) async fn answer(state: &Arc<ApiState>, mut parts: Parts, path: &str, query: &[(String, String)]) -> Response {
    // The `websockets` library checks the handshake before the app sees the request.
    let key = parts.headers.get("sec-websocket-key").map(HeaderValue::as_bytes);
    let version_ok = parts
        .headers
        .get("sec-websocket-version")
        .is_some_and(|version| version.as_bytes() == b"13");
    let (Some(key), true) = (key, version_ok) else {
        return status(
            StatusCode::BAD_REQUEST,
            "Failed to open a WebSocket connection: invalid handshake.\n",
        );
    };
    if path != VNC_PATH {
        return refused();
    }
    // `websocket.query_params.get('ticket', '')`: the last value given.
    let ticket = query
        .iter()
        .rev()
        .find(|(key, _)| key == "ticket")
        .map_or("", |(_, value)| value.as_str());
    if !state.browser_login.ticket_matches(ticket) {
        return refused();
    }
    let Ok(vnc) = tokio::net::TcpStream::connect(("127.0.0.1", state.browser_login.vnc_port())).await else {
        return refused();
    };
    let Some(on_upgrade) = parts.extensions.remove::<hyper::upgrade::OnUpgrade>() else {
        return status(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
    };
    let binary = header_tokens(&parts.headers, header::SEC_WEBSOCKET_PROTOCOL).any(|token| token == "binary");

    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let headers = response.headers_mut();
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    if let Ok(accept) = HeaderValue::from_str(&derive_accept_key(key)) {
        headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
    }
    if binary {
        headers.insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("binary"));
    }
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let io = TokioAdapter::new(hyper_util::rt::TokioIo::new(upgraded));
                // uvicorn's `ws_max_size`: 16 MiB a message.
                let config = WebSocketConfig::default().max_message_size(Some(WS_MAX_SIZE));
                let socket = WebSocketStream::from_raw_socket(io, Role::Server, Some(config)).await;
                fav_browser::vnc::bridge(socket, vnc).await;
            }
            Err(error) => logging::get("browser-login").debug(format!("The VNC WebSocket never upgraded: {error}")),
        }
    });
    response
}
