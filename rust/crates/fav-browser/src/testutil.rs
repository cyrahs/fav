//! Shared helpers for this crate's tests.

use std::path::PathBuf;

/// A golden document read with `serde_json`, for values that are compared as CDP JSON.
pub(crate) fn golden_json(relative: &str) -> serde_json::Value {
    let path = fav_core::golden::dir().join(relative);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("cannot read golden {}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("golden {relative} is not JSON: {error}"))
}

/// The `cases` of a golden document read with [`golden_json`].
pub(crate) fn golden_cases(relative: &str) -> Vec<serde_json::Value> {
    match golden_json(relative).get("cases") {
        Some(serde_json::Value::Array(cases)) => cases.clone(),
        _ => panic!("golden {relative} has no cases"),
    }
}

/// Chromium to drive: `FAV_TEST_CHROMIUM`, else the cloud sessions' Playwright build.
/// CI sets the variable, so there a test that needs it always runs.
pub(crate) fn chromium() -> Option<PathBuf> {
    let found = std::env::var_os("FAV_TEST_CHROMIUM")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("/opt/pw-browsers/chromium")).filter(|path| path.exists()));
    if found.is_none() {
        assert!(std::env::var_os("CI").is_none(), "CI must set FAV_TEST_CHROMIUM");
    }
    found
}

/// One answer of the loopback test server.
#[derive(Debug, Clone)]
pub(crate) struct Reply {
    pub status: &'static str,
    pub content_type: &'static str,
    pub headers: String,
    pub body: String,
}

impl Reply {
    pub(crate) fn html(body: &str) -> Self {
        Self {
            status: "200 OK",
            content_type: "text/html; charset=utf-8",
            headers: String::new(),
            body: body.to_owned(),
        }
    }

    pub(crate) fn with_header(mut self, header: &str) -> Self {
        self.headers.push_str(header);
        self.headers.push_str("\r\n");
        self
    }
}

/// A request the test server saw: its request line and its headers, lowercased.
pub(crate) type Seen = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// An HTTP/1.1 server on loopback that answers every request with `answer(request head)`
/// and closes the connection. Nothing it serves ever leaves the host.
pub(crate) async fn serve(answer: impl Fn(&str) -> Reply + Send + Sync + 'static) -> (std::net::SocketAddr, Seen) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    let answer = std::sync::Arc::new(answer);
    let seen: Seen = Seen::default();
    let recorded = std::sync::Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let answer = std::sync::Arc::clone(&answer);
            let recorded = std::sync::Arc::clone(&recorded);
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => head.extend_from_slice(&buffer[..read]),
                    }
                }
                let request = String::from_utf8_lossy(&head).to_ascii_lowercase();
                recorded.lock().unwrap().push(request.clone());
                let reply = answer(&request);
                let response = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
                    reply.status,
                    reply.content_type,
                    reply.body.len(),
                    reply.headers,
                    reply.body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (addr, seen)
}
