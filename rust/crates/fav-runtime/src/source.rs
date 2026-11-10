//! What a source is to the worker, and the registry of the ones that exist in Rust.
//!
//! Python origin: the duck type `run.py` relies on (`JobSpec.factory()` returns an object
//! with `async update()` and an optional `async aclose()`), the exception attributes it
//! reads (`notification_dedupe_key`, the request host of an httpx transport error), and
//! the `Telegram` singleton that stays resident in the worker between runs.
//!
//! Sources are ported one work package at a time. [`Registry`] holds the ones that are;
//! a job without an entry has no runner, which the worker already handles the way
//! `run.py` handles a missing runner (not scheduled, a trigger is rejected).

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::{BoxFuture, StopSignal};

/// Why a run failed, carrying what `run.py` reads off a Python exception.
///
/// `class_name` is the Python exception class the source would have raised; it appears in
/// the failure notification (`RuntimeError: boom`) and its payload (`error_class`), so a
/// port keeps the Python name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobError {
    class_name: String,
    message: String,
    notification_dedupe_key: Option<String>,
    request_host: Option<String>,
}

impl JobError {
    /// An exception of class `class_name` whose `str()` is `message`.
    pub fn new(class_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            class_name: class_name.into(),
            message: message.into(),
            notification_dedupe_key: None,
            request_host: None,
        }
    }

    /// The exception's own `notification_dedupe_key`: a failure narrower than the run (one
    /// video, one parser) that gets its own notification row.
    #[must_use]
    pub fn with_notification_dedupe_key(mut self, key: impl Into<String>) -> Self {
        self.notification_dedupe_key = Some(key.into());
        self
    }

    /// The host an HTTP transport error was talking to (httpx `RequestError.request.url.host`).
    /// Used only when the message is empty, as for a connect or read timeout.
    #[must_use]
    pub fn with_request_host(mut self, host: impl Into<String>) -> Self {
        self.request_host = Some(host.into());
        self
    }

    pub fn class_name(&self) -> &str {
        &self.class_name
    }

    /// `str(exc)`.
    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn notification_dedupe_key(&self) -> Option<&str> {
        self.notification_dedupe_key.as_deref()
    }

    /// `_format_exception`: `Class: message`, the request host for a messageless transport
    /// error (never the URL, which can carry tokens), or the bare class name.
    pub fn formatted(&self) -> String {
        let message = if self.message.is_empty() {
            self.request_host
                .as_deref()
                .filter(|host| !host.is_empty())
                .map(|host| format!("while requesting {host}"))
                .unwrap_or_default()
        } else {
            self.message.clone()
        };
        if message.is_empty() {
            self.class_name.clone()
        } else {
            format!("{}: {message}", self.class_name)
        }
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.formatted())
    }
}

impl std::error::Error for JobError {}

/// A source the worker creates for one run, runs and closes.
pub trait Source: Send {
    /// `await update()`: one crawl.
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>>;

    /// `await aclose()`: release what the run held. Failures are logged, never raised.
    fn close(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async { Ok(()) })
    }
}

/// A source that stays resident in the worker (Telegram's listeners and queue workers):
/// started once, runs alongside the scheduler, and serves scheduled runs as well.
pub trait ResidentSource: Send + Sync {
    /// Run until `stop` is set. Returning early is a failure of the whole worker.
    fn run(&self, stop: StopSignal) -> BoxFuture<'_, Result<(), JobError>>;

    /// Resolve once every account is connected.
    fn wait_until_ready(&self) -> BoxFuture<'_, Result<(), JobError>>;

    /// One scheduled or triggered run.
    fn update(&self) -> BoxFuture<'_, Result<(), JobError>>;

    fn close(&self) -> BoxFuture<'_, Result<(), JobError>>;
}

/// `JobSpec.factory` for a source created per run.
pub type SourceFactory = Arc<dyn Fn() -> Box<dyn Source> + Send + Sync>;

/// `JobSpec.factory` for a source that can stay resident.
pub type ResidentFactory = Arc<dyn Fn() -> Arc<dyn ResidentSource> + Send + Sync>;

/// How a job's source is made.
#[derive(Clone)]
pub enum SourceKind {
    PerRun(SourceFactory),
    Resident(ResidentFactory),
}

impl fmt::Debug for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PerRun(_) => "PerRun",
            Self::Resident(_) => "Resident",
        })
    }
}

/// The sources that have a Rust implementation, by job key.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    sources: HashMap<String, SourceKind>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a source created for each run.
    pub fn per_run(&mut self, key: &str, factory: impl Fn() -> Box<dyn Source> + Send + Sync + 'static) -> &mut Self {
        self.sources.insert(key.to_owned(), SourceKind::PerRun(Arc::new(factory)));
        self
    }

    /// Register a source that can stay resident in the worker.
    pub fn resident(&mut self, key: &str, factory: impl Fn() -> Arc<dyn ResidentSource> + Send + Sync + 'static) -> &mut Self {
        self.sources.insert(key.to_owned(), SourceKind::Resident(Arc::new(factory)));
        self
    }

    pub fn get(&self, key: &str) -> Option<&SourceKind> {
        self.sources.get(key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.sources.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::JobError;

    // py: tests/test_run_control_requests.py::test_format_exception_names_the_host_for_a_messageless_transport_error
    #[test]
    fn format_exception_names_the_host_for_a_messageless_transport_error() {
        let error = JobError::new("ConnectTimeout", "").with_request_host("vault.example.com");
        let formatted = error.formatted();
        assert_eq!(formatted, "ConnectTimeout: while requesting vault.example.com");
        assert!(!formatted.contains("secret-vault-id"));
        assert_eq!(JobError::new("ConnectTimeout", "").formatted(), "ConnectTimeout");
        assert_eq!(JobError::new("RuntimeError", "boom").formatted(), "RuntimeError: boom");
        // A message wins over the host.
        assert_eq!(
            JobError::new("ReadError", "reset").with_request_host("h").formatted(),
            "ReadError: reset"
        );
    }
}
