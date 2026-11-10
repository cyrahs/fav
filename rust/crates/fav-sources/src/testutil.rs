//! A scripted web for the sources' tests, standing in for httpx's `MockTransport`.
//!
//! The sources' clients are pointed at it as their HTTP proxy, so a request for
//! `http://wx4.sinaimg.cn/large/abc.jpg` arrives here with its full URL and the handler
//! answers for any host, as Python's mock transport does. It speaks HTTP/1.1 by hand so a
//! handler can also drop the connection or stall, which is how transport errors are made.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fav_db::testdb::TestDatabase;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A temporary database with fav's schema applied.
pub(crate) async fn database() -> TestDatabase {
    let database = TestDatabase::create().await;
    database.db().ensure_schema().await.expect("schema applies");
    database
}

/// One request as the fake web saw it.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub method: String,
    /// The absolute URL (`http://host/path?query`).
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Seen {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn host(&self) -> &str {
        let rest = self.url.split_once("://").map_or(self.url.as_str(), |(_, rest)| rest);
        rest.split(['/', '?', ':']).next().unwrap_or_default()
    }

    pub(crate) fn path(&self) -> &str {
        let rest = self.url.split_once("://").map_or(self.url.as_str(), |(_, rest)| rest);
        let path = rest.find('/').map_or("", |index| &rest[index..]);
        path.split(['?', '#']).next().unwrap_or_default()
    }

    pub(crate) fn query(&self) -> &str {
        self.url
            .split_once('?')
            .map_or("", |(_, query)| query.split('#').next().unwrap_or_default())
    }
}

/// What the fake web does with a request.
#[derive(Debug, Clone)]
pub(crate) enum Reply {
    Respond {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// Close the connection without answering (httpx: `RemoteProtocolError`).
    Close,
    /// Answer nothing for a while, then close (a read timeout on the client side).
    Stall(Duration),
    /// Reset the connection (TCP RST) without answering (httpx: `ReadError`).
    Reset,
    /// Answer with a `content-length` of `length` but send only `sent`, then close
    /// (httpx: `RemoteProtocolError`, "peer closed connection without sending complete
    /// message body").
    Truncated { status: u16, sent: Vec<u8>, length: usize },
}

impl Reply {
    pub(crate) fn status(status: u16) -> Self {
        Self::Respond {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub(crate) fn bytes(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self::Respond {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub(crate) fn json(status: u16, body: &serde_json::Value) -> Self {
        Self::Respond {
            status,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: serde_json::to_vec(body).expect("JSON"),
        }
    }
}

type Handler = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

/// The fake web: a proxy address and the requests it served.
#[derive(Clone)]
pub(crate) struct FakeWeb {
    pub proxy: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl std::fmt::Debug for FakeWeb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeWeb").field("proxy", &self.proxy).finish_non_exhaustive()
    }
}

impl FakeWeb {
    pub(crate) async fn start(handler: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler: Handler = Arc::new(handler);
        let recorded = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    serve(stream, handler, recorded).await;
                });
            }
        });
        Self {
            proxy: format!("http://{address}"),
            seen,
        }
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("lock").clone()
    }

    pub(crate) fn urls(&self) -> Vec<String> {
        self.seen().into_iter().map(|seen| seen.url).collect()
    }
}

async fn serve(mut stream: TcpStream, handler: Handler, seen: Arc<Mutex<Vec<Seen>>>) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
        }
    }
    let url = if target.starts_with("http://") || target.starts_with("https://") {
        target
    } else {
        let host = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        format!("http://{host}{target}")
    };
    let request = Seen {
        method,
        url,
        headers,
        body,
    };
    let reply = handler(&request);
    seen.lock().expect("lock").push(request);
    match reply {
        Reply::Close => {}
        Reply::Reset => {
            let _ = stream.set_zero_linger();
            drop(stream);
        }
        Reply::Truncated { status, sent, length } => {
            let head = format!("HTTP/1.1 {status} OK\r\nconnection: close\r\ncontent-length: {length}\r\n\r\n");
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(&sent).await;
            let _ = stream.flush().await;
        }
        Reply::Stall(duration) => tokio::time::sleep(duration).await,
        Reply::Respond { status, headers, body } => {
            let reason = axum::http::StatusCode::from_u16(status)
                .ok()
                .and_then(|status| status.canonical_reason())
                .unwrap_or("");
            let mut response = format!(
                "HTTP/1.1 {status} {reason}\r\nconnection: close\r\ncontent-length: {}\r\n",
                body.len()
            );
            for (name, value) in headers {
                let _ = write!(response, "{name}: {value}\r\n");
            }
            response.push_str("\r\n");
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.write_all(&body).await;
            let _ = stream.flush().await;
        }
    }
}
