use std::time::Duration;

use async_tungstenite::tungstenite::Message;
use futures_util::StreamExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::bridge;

/// A VNC server that greets, reads the client's 12-byte answer, and hangs up.
async fn vnc_server() -> (u16, tokio::sync::oneshot::Receiver<Vec<u8>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, received) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        socket.write_all(b"RFB 003.008\n").await.unwrap();
        let mut answer = vec![0_u8; 12];
        socket.read_exact(&mut answer).await.unwrap();
        let _ = sender.send(answer);
    });
    (port, received)
}

#[tokio::test]
async fn bytes_are_relayed_both_ways_until_the_vnc_server_hangs_up() {
    let (vnc_port, received) = vnc_server().await;
    let relay = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_port = relay.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = relay.accept().await.unwrap();
        let socket = async_tungstenite::tokio::accept_async(stream).await.unwrap();
        let vnc = tokio::net::TcpStream::connect(("127.0.0.1", vnc_port)).await.unwrap();
        bridge(socket, vnc).await;
    });

    let (mut client, _) = async_tungstenite::tokio::connect_async(format!("ws://127.0.0.1:{relay_port}/"))
        .await
        .expect("connected");
    let greeting = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(greeting, Message::binary(b"RFB 003.008\n".to_vec()));
    // A text frame is relayed as its bytes, as Python's `message.get('text').encode()`.
    client.send(Message::text("RFB 003.008\n")).await.unwrap();
    assert_eq!(received.await.unwrap(), b"RFB 003.008\n");

    // The server hung up: the relay closes the socket normally.
    let closing = tokio::time::timeout(Duration::from_secs(5), client.next()).await.unwrap();
    match closing {
        Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 1000),
        other => panic!("expected a close frame, got {other:?}"),
    }
}
