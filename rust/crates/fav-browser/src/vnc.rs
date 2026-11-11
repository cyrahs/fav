//! VNC over WebSocket: noVNC in the web UI talks to the login window's loopback VNC port.
//!
//! Python origin: `bridge_vnc` and `_close_bridge` in `src/api/browser_login.py`. The
//! ticket check and the WebSocket handshake are the API's (`fav-api`); this is the relay
//! once the socket is accepted: client frames (binary, or text as its UTF-8 bytes) go to
//! the VNC server, its bytes come back as binary frames, and whichever side ends first
//! ends both.

use async_tungstenite::WebSocketStream;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::tungstenite::protocol::CloseFrame;
use async_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use futures_util::StreamExt as _;
use futures_util::io::{AsyncRead, AsyncWrite};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// `_VNC_READ_BYTES`.
const VNC_READ_BYTES: usize = 65536;

/// Relay `socket` and `vnc` until either side closes, then close both.
pub async fn bridge<S>(socket: WebSocketStream<S>, vnc: TcpStream)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (mut to_client, mut from_client) = socket.split();
    let (mut from_vnc, mut to_vnc) = vnc.into_split();

    let client_to_vnc = async {
        while let Some(Ok(message)) = from_client.next().await {
            let data = match message {
                Message::Binary(bytes) => bytes.to_vec(),
                Message::Text(text) => text.as_bytes().to_vec(),
                Message::Close(_) => return,
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
            if !data.is_empty() && to_vnc.write_all(&data).await.is_err() {
                return;
            }
        }
    };
    let vnc_to_client = async {
        let mut buffer = vec![0_u8; VNC_READ_BYTES];
        loop {
            match from_vnc.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    if to_client.send(Message::binary(buffer[..read].to_vec())).await.is_err() {
                        return;
                    }
                }
            }
        }
    };
    tokio::select! {
        () = client_to_vnc => {}
        () = vnc_to_client => {}
    }
    let _ = to_vnc.shutdown().await;
    drop(to_vnc);
    drop(from_vnc);
    // `websocket.close()`: code 1000.
    let _ = to_client
        .close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        }))
        .await;
}

#[cfg(test)]
mod tests;
