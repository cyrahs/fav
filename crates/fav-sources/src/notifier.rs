//! The notification seam sources queue their messages through (`enqueue_notification`,
//! `resolve_notification`), injectable so tests record or fail the calls. Moved here from
//! WP6's `kit` module in WP18; `kit`'s HTTP and file helpers went to `fav_net::httpx` and
//! `fav_net::download`.

use std::fmt;
use std::sync::Arc;

use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox, Resolution};
use fav_runtime::source::JobError;

pub use fav_net::httpx::HttpError;

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

/// `JobError` for an httpx error: a transport failure keeps the request's host, which the
/// run reports when the message is empty.
pub trait HttpJobError {
    fn into_job_error(self) -> JobError;
}

impl HttpJobError for HttpError {
    fn into_job_error(self) -> JobError {
        match self {
            Self::Transport { class, message, host } => JobError::new(class, message).with_request_host(host),
            Self::Status { message, .. } => JobError::new("HTTPStatusError", message),
        }
    }
}

/// `await asyncio.to_thread(work)`: file work (a move across file systems copies the whole
/// file) off the async threads.
pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, JobError> + Send + 'static) -> Result<T, JobError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| JobError::new("RuntimeError", error.to_string()))?
}
