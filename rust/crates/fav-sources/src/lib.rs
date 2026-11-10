//! The fav content sources, one module per entry of `JOB_SPECS`.
//!
//! Scope (filled in by WP5 to WP17, see `rust/docs/PLAN.md`). Every source
//! implements the runtime's `Source` trait and receives its HTTP client,
//! database, browser, Telegram client and subprocess runner by injection.

use std::sync::Arc;

use fav_db::{Db, SettingsStore};
use fav_runtime::source::Registry;

mod kit;
pub mod pixiv;
pub mod stellasora;
#[cfg(test)]
mod testkit;

pub use kit::{HttpError, Notifier, OutboxNotifier};

/// What every source's factory may use: the database and the settings store.
#[derive(Debug, Clone)]
pub struct SourceContext {
    pub db: Db,
    pub settings: Arc<SettingsStore>,
}

/// The sources that have a Rust implementation, for the worker.
///
/// Each source's work package registers it here. A job without an entry has no runner:
/// the worker does not schedule it and refuses to trigger it (DIVERGENCES.md D17).
pub fn registry(ctx: &SourceContext) -> Registry {
    let mut registry = Registry::new();
    stellasora::register(&mut registry, ctx);
    pixiv::register(&mut registry, ctx);
    registry
}
