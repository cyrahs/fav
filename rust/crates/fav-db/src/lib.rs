//! PostgreSQL access for fav.
//!
//! Python origin: `src/tool/database.py` (pool, advisory locks, batch helpers), the
//! `app_settings` half of `src/core/settings.py`, and every `_ensure_table()` in the
//! tree, which [`schema`] replaces with one consolidated script. SQL is native
//! PostgreSQL: there is no SQLite-dialect translation layer (DIVERGENCES.md).

pub mod batch;
pub mod lock;
pub mod pool;
pub mod schema;
pub mod settings_store;
pub mod testdb;

pub use batch::{Param, Statement};
pub use lock::{AdvisoryLock, advisory_lock_id};
pub use pool::Db;
pub use settings_store::SettingsStore;

/// Errors from the database layer.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    /// A caller error Python raises as `ValueError`; the message is Python's.
    #[error("{0}")]
    Invalid(String),
    /// A stored value is not JSON (the column is JSONB, so this means a driver problem).
    #[error("stored value for {what} is not valid JSON: {source}")]
    StoredJson {
        what: String,
        #[source]
        source: fav_core::pyjson::LoadError,
    },
    /// The store was pinned without a database (tests) and a call needed one.
    #[error("no database: this settings store is pinned to a snapshot")]
    NoDatabase,
}
