//! Helpers the WP6 sources (Stella Sora, Pixiv) share: httpx-shaped HTTP errors, the
//! notification seam, streamed downloads to `.part`.
//!
//! WP5 builds the same pieces in `fav-net` in parallel (client factory, `raise_for_status`
//! text, streamed downloads, retry ladder). Until both have merged this module keeps a
//! private copy, as `rust/docs/PLAN.md` asks; the cleanup note there says to fold it into
//! `fav-net` afterwards.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use fav_net::reqwest::{self, Response};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox, Resolution};
use fav_runtime::source::JobError;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

// ---------- HTTP errors as httpx names them ----------

/// An `httpx.HTTPError`: a transport failure or a `raise_for_status()` rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// A transport error, under httpx's class name (`ConnectError`, `ReadTimeout`, ...).
    Transport {
        class: &'static str,
        message: String,
        host: String,
    },
    /// `HTTPStatusError` from `raise_for_status()`.
    Status { status: u16, message: String },
}

impl HttpError {
    pub(crate) fn transport(error: &reqwest::Error) -> Self {
        let class = body_error_class(error).unwrap_or_else(|| fav_net::httpx_error_class(error));
        // httpx's timeouts carry no text; other transport errors carry the OS error's.
        let message = if error.is_timeout() {
            String::new()
        } else {
            innermost_message(error)
        };
        let host = error.url().and_then(|url| url.host_str()).unwrap_or_default().to_owned();
        Self::Transport { class, message, host }
    }

    /// The Python class name.
    pub(crate) fn class_name(&self) -> &'static str {
        match self {
            Self::Transport { class, .. } => class,
            Self::Status { .. } => "HTTPStatusError",
        }
    }

    /// `str(exc)`.
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Transport { message, .. } | Self::Status { message, .. } => message,
        }
    }

    /// One of httpx's `ConnectError`, `TimeoutException` subclasses or `RemoteProtocolError`.
    pub(crate) fn is_connect_timeout_or_protocol(&self) -> bool {
        matches!(
            self,
            Self::Transport {
                class: "ConnectError" | "ConnectTimeout" | "ReadTimeout" | "WriteTimeout" | "PoolTimeout" | "RemoteProtocolError",
                ..
            }
        )
    }

    pub(crate) fn into_job_error(self) -> JobError {
        match self {
            Self::Transport { class, message, host } => JobError::new(class, message).with_request_host(host),
            Self::Status { message, .. } => JobError::new("HTTPStatusError", message),
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

/// reqwest reports a failure while reading the body as a decode error; httpx calls a body
/// cut short `RemoteProtocolError` and a broken connection `ReadError`.
fn body_error_class(error: &reqwest::Error) -> Option<&'static str> {
    if !error.is_decode() && !error.is_body() {
        return None;
    }
    let mut current: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(source) = current {
        if let Some(hyper_error) = source.downcast_ref::<hyper::Error>() {
            if hyper_error.is_incomplete_message() {
                return Some("RemoteProtocolError");
            }
            if hyper_error.is_timeout() {
                return Some("ReadTimeout");
            }
        }
        if let Some(io_error) = source.downcast_ref::<std::io::Error>() {
            // hyper's length decoder reports a body cut short as an unexpected EOF.
            return Some(if io_error.kind() == std::io::ErrorKind::UnexpectedEof {
                "RemoteProtocolError"
            } else {
                "ReadError"
            });
        }
        current = source.source();
    }
    None
}

fn innermost_message(error: &reqwest::Error) -> String {
    let mut current: &dyn std::error::Error = error;
    while let Some(source) = current.source() {
        current = source;
    }
    current.to_string()
}

/// `response.raise_for_status()` with httpx 0.28's message.
pub(crate) fn raise_for_status(response: &Response) -> Result<(), HttpError> {
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
    match status_error_message(status, response.url().as_str(), location.as_deref()) {
        None => Ok(()),
        Some(message) => Err(HttpError::Status { status, message }),
    }
}

/// The text of the `HTTPStatusError` `raise_for_status()` raises, `None` for a 2xx.
pub(crate) fn status_error_message(status: u16, url: &str, location: Option<&str>) -> Option<String> {
    if (200..300).contains(&status) {
        return None;
    }
    let error_type = match status / 100 {
        1 => "Informational response",
        3 => "Redirect response",
        4 => "Client error",
        5 => "Server error",
        _ => "Invalid status code",
    };
    let redirect = match location {
        // `has_redirect_location`: a redirect status that names where to go.
        Some(location) if matches!(status, 301 | 302 | 303 | 307 | 308) => format!("Redirect location: '{location}'\n"),
        _ => String::new(),
    };
    Some(format!(
        "{error_type} '{status} {}' for url '{url}'\n{redirect}For more information check: https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/{status}",
        reason_phrase(status)
    ))
}

/// `httpx.codes.get_reason_phrase(status)`.
pub(crate) fn reason_phrase(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Request Entity Too Large",
        414 => "Request-URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Requested Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Entity",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => "",
    }
}

// ---------- notifications ----------

/// `enqueue_notification(...)`, injectable so tests can record or fail the calls.
pub trait Notifier: Send + Sync {
    /// Queue one notification; the error text is what the caller logs.
    fn enqueue(&self, notification: NewNotification) -> BoxFuture<'_, Result<(), String>>;

    /// `resolve_notification(dedupe_key, ...)`: mark an earlier failure resolved.
    fn resolve(&self, dedupe_key: String, resolution: Resolution) -> BoxFuture<'_, Result<(), String>>;
}

/// The real outbox.
#[derive(Debug, Clone)]
pub struct OutboxNotifier(pub Outbox);

impl Notifier for OutboxNotifier {
    fn enqueue(&self, notification: NewNotification) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move { self.0.enqueue(&notification).await.map(|_| ()).map_err(|error| error.to_string()) })
    }

    fn resolve(&self, dedupe_key: String, resolution: Resolution) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.0
                .resolve(&dedupe_key, &resolution)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }
}

impl fmt::Debug for dyn Notifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Notifier")
    }
}

pub(crate) type SharedNotifier = Arc<dyn Notifier>;

// ---------- downloads ----------

/// A failure while writing a streamed body: the HTTP side or the file side.
#[derive(Debug)]
pub(crate) enum DownloadError {
    Http(HttpError),
    Io(std::io::Error),
}

impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => error.fmt(f),
            Self::Io(error) => f.write_str(&py_os_error_text(error)),
        }
    }
}

/// The `OSError` subclass Python raises for an I/O error.
pub(crate) fn io_class_name(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => "FileNotFoundError",
        ErrorKind::PermissionDenied => "PermissionError",
        ErrorKind::AlreadyExists => "FileExistsError",
        ErrorKind::IsADirectory => "IsADirectoryError",
        ErrorKind::NotADirectory => "NotADirectoryError",
        _ => "OSError",
    }
}

/// `str(OSError)` without the file name: `[Errno 2] No such file or directory`.
pub(crate) fn py_os_error_text(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => {
            let text = error.to_string();
            let reason = text.split(" (os error").next().unwrap_or(&text).to_owned();
            format!("[Errno {code}] {reason}")
        }
        None => error.to_string(),
    }
}

/// Stream a response body into `part`, creating or truncating it. The caller renames it.
pub(crate) async fn stream_body_to(response: Response, part: &Path) -> Result<(), DownloadError> {
    let mut file = tokio::fs::File::create(part).await.map_err(DownloadError::Io)?;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| DownloadError::Http(HttpError::transport(&error)))?;
        file.write_all(&chunk).await.map_err(DownloadError::Io)?;
    }
    file.flush().await.map_err(DownloadError::Io)?;
    Ok(())
}

/// `shutil.move(src, dst)`: a rename, or copy (with times) and delete across file systems.
pub(crate) fn move_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(18) => {
            // EXDEV: another file system. `copy2` keeps the times; the copy goes to a
            // `.part` first so a crash never leaves a truncated file at `dst` (D52).
            let mut part = dst.as_os_str().to_owned();
            part.push(".part");
            let part = std::path::PathBuf::from(part);
            std::fs::copy(src, &part)?;
            let metadata = std::fs::metadata(src)?;
            let times = std::fs::FileTimes::new()
                .set_accessed(metadata.accessed()?)
                .set_modified(metadata.modified()?);
            std::fs::File::options().write(true).open(&part)?.set_times(times)?;
            std::fs::rename(&part, dst)?;
            std::fs::remove_file(src)
        }
        Err(error) => Err(error),
    }
}

/// `await asyncio.to_thread(work)`: file work (a move across file systems copies the whole
/// file) off the async threads.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, fav_runtime::source::JobError> + Send + 'static,
) -> Result<T, fav_runtime::source::JobError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| fav_runtime::source::JobError::new("RuntimeError", error.to_string()))?
}

#[cfg(test)]
mod tests {
    use fav_core::golden;

    use super::{py_os_error_text, reason_phrase, status_error_message};

    #[test]
    fn os_errors_read_like_python() {
        let error = std::io::Error::from_raw_os_error(2);
        assert_eq!(py_os_error_text(&error), "[Errno 2] No such file or directory");
    }

    #[test]
    fn reason_phrases_are_httpx_ones() {
        let document = golden::load("pyweb/httpx_reason_phrases.json");
        let cases = golden::cases(&document);
        assert_eq!(cases.len(), 500);
        for case in cases {
            let pair = golden::list(case);
            let status = u16::try_from(golden::int(&pair[0])).expect("a status");
            assert_eq!(reason_phrase(status), golden::string(&pair[1]), "{status}");
        }
    }

    #[test]
    fn status_errors_read_like_httpx() {
        let document = golden::load("pyweb/httpx_status_errors.json");
        for case in golden::cases(&document) {
            let status = u16::try_from(golden::int(golden::get(case, "status"))).expect("a status");
            let location = golden::get(case, "location").as_str();
            let expected = golden::get(case, "message").as_str();
            let url = golden::string(golden::get(case, "url"));
            assert_eq!(status_error_message(status, &url, location).as_deref(), expected, "{status}");
        }
    }
}
