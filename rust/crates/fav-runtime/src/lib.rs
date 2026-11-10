//! The worker runtime for fav.
//!
//! Python origin: `run.py` (the worker: scheduler, control-request consumer, notification
//! delivery, settings watcher, `--trigger`), `src/service/jobs.py` (the source registry),
//! `src/tool/control_queue.py`, `src/tool/notifications.py`, `src/tool/telegram_bot.py`
//! and `src/tool/site_sessions.py`, plus APScheduler's cron trigger and the part of its
//! scheduler the worker uses ([`cron`], [`scheduler`]).
//!
//! Everything external is injected: the database ([`fav_db::Db`]), the settings
//! ([`fav_db::SettingsStore`]), the Telegram Bot API base URL ([`telegram_bot::BotClient`])
//! and the sources themselves ([`source::Registry`]).

use std::future::Future;
use std::pin::Pin;

use fav_db::DbError;
use fav_db::settings_store::SettingsStoreError;
use tokio::sync::watch;

pub mod control;
pub mod cron;
pub mod jobs;
pub mod notifications;
pub mod scheduler;
pub mod site_sessions;
pub mod source;
pub mod telegram_bot;
pub mod worker;

mod pystr;
mod which;

#[cfg(test)]
mod testutil;

/// A boxed future, for the object-safe source traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Errors from the runtime's stores.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Settings(#[from] SettingsStoreError),
    /// A caller error Python raises as `ValueError`; the message is Python's.
    #[error("{0}")]
    Invalid(String),
    /// A failure Python raises as `RuntimeError`; the message is Python's.
    #[error("{0}")]
    Failed(String),
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(DbError::Sqlx(error))
    }
}

/// The worker's stop flag (`stop_event` in `run.py`): set once, seen by every holder.
#[derive(Debug, Clone)]
pub struct StopSignal {
    receiver: watch::Receiver<bool>,
}

/// The side that sets a [`StopSignal`].
#[derive(Debug, Clone)]
pub struct StopHandle {
    sender: watch::Sender<bool>,
}

/// A new stop flag, not set.
pub fn stop_signal() -> (StopHandle, StopSignal) {
    let (sender, receiver) = watch::channel(false);
    (StopHandle { sender }, StopSignal { receiver })
}

impl StopHandle {
    /// Set the flag; returns whether it was already set.
    pub fn stop(&self) -> bool {
        self.sender.send_replace(true)
    }

    pub fn is_stopped(&self) -> bool {
        *self.sender.borrow()
    }

    pub fn signal(&self) -> StopSignal {
        StopSignal {
            receiver: self.sender.subscribe(),
        }
    }
}

impl StopSignal {
    pub fn is_set(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Wait until the flag is set (or every handle is gone, which can only mean shutdown).
    pub async fn wait(&self) {
        let mut receiver = self.receiver.clone();
        let _ = receiver.wait_for(|stopped| *stopped).await;
    }

    /// `asyncio.wait_for(stop_event.wait(), timeout)`: whether the flag was set in time.
    pub async fn wait_timeout(&self, timeout: std::time::Duration) -> bool {
        tokio::time::timeout(timeout, self.wait()).await.is_ok()
    }
}
