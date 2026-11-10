//! The consolidated schema, applied at every start.
//!
//! Python origin: every `_ensure_table()` / `_ensure_schema()` in the tree and
//! `CREATE_APP_SETTINGS_TABLE_SQL` in `src/core/settings.py`. Python creates each source's
//! tables the first time that source runs; Rust creates all of them at startup
//! (DIVERGENCES.md). `schema.sql` explains its layout.

use sqlx::Connection as _;

use crate::lock::advisory_lock_id;
use crate::{Db, DbError};

/// The whole schema as one idempotent script.
pub const SCHEMA_SQL: &str = include_str!("schema.sql");

/// The advisory lock that serializes schema setup between processes starting together.
pub const SCHEMA_LOCK: &str = "fav:schema";

impl Db {
    /// Create every table, column and index that is missing.
    ///
    /// The worker and the API can start at the same moment against a fresh database, and
    /// `CREATE TABLE IF NOT EXISTS` is not atomic against a concurrent creator, so the
    /// script runs under a session advisory lock on a dedicated connection. Each statement
    /// commits on its own, so no table stays locked for longer than its own statement.
    pub async fn ensure_schema(&self) -> Result<(), DbError> {
        let mut connection = self.dedicated_connection().await?;
        let id = advisory_lock_id(SCHEMA_LOCK);
        sqlx::query("SELECT pg_advisory_lock($1);")
            .bind(id)
            .execute(&mut connection)
            .await?;
        let applied = sqlx::raw_sql(SCHEMA_SQL).execute(&mut connection).await;
        // Closing the connection releases the lock even if the unlock fails.
        let unlocked = sqlx::query("SELECT pg_advisory_unlock($1);")
            .bind(id)
            .execute(&mut connection)
            .await;
        let _ = connection.close().await;
        applied?;
        unlocked?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
