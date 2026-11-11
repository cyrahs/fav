//! The consolidated schema, applied at every start.
//!
//! Python origin: every `_ensure_table()` / `_ensure_schema()` in the tree and
//! `CREATE_APP_SETTINGS_TABLE_SQL` in `src/core/settings.py`. Python creates each source's
//! tables the first time that source runs; Rust creates all of them at startup
//! (DIVERGENCES.md D6). `schema.sql` explains its layout.

use fav_core::logging;
use sqlx::{AssertSqlSafe, Connection as _, PgConnection};

use crate::lock::advisory_lock_id;
use crate::{Db, DbError};

/// The whole schema as one idempotent script.
pub const SCHEMA_SQL: &str = include_str!("schema.sql");

/// The advisory lock that serializes schema setup between processes starting together.
pub const SCHEMA_LOCK: &str = "fav:schema";

/// Python's lock around the bilibili table setup (`_BILIBILI_SCHEMA_LOCK_ID`).
pub const BILIBILI_SCHEMA_LOCK: &str = "bilibili:schema";

/// `_BILIBILI_BVID_ARBITER_SQL`: whether `bvid` has a usable unique index for ON CONFLICT.
const BILIBILI_BVID_ARBITER_SQL: &str = "
SELECT EXISTS (
    SELECT 1
    FROM pg_index AS idx
    JOIN pg_attribute AS attr
      ON attr.attrelid = idx.indrelid
     AND attr.attnum = idx.indkey[0]
    WHERE idx.indrelid = 'bilibili'::regclass
      AND idx.indisunique
      AND idx.indimmediate
      AND idx.indisvalid
      AND idx.indisready
      AND idx.indnkeyatts = 1
      AND idx.indpred IS NULL
      AND idx.indexprs IS NULL
      AND attr.attname = 'bvid'
) AS has_bvid_arbiter;
";

/// The statements of a script: split at semicolons outside quotes and comments, trimmed,
/// comment-only pieces dropped.
pub fn split_statements(script: &str) -> Vec<&str> {
    let bytes = script.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = script[index..].find('\n').map_or(bytes.len(), |offset| index + offset);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = script[index + 2..].find("*/").map_or(bytes.len(), |offset| index + 2 + offset + 2);
                continue;
            }
            quote @ (b'\'' | b'"') => {
                // A doubled quote inside is an escaped one, which this loop handles as two
                // adjacent quoted runs.
                index = script[index + 1..]
                    .find(char::from(quote))
                    .map_or(bytes.len(), |offset| index + 1 + offset);
            }
            b';' => {
                statements.push(&script[start..=index]);
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    statements.push(&script[start..]);
    statements
        .into_iter()
        .map(str::trim)
        .filter(|statement| !is_only_comments(statement))
        .collect()
}

fn is_only_comments(statement: &str) -> bool {
    statement.lines().all(|line| {
        let line = line.trim();
        line.is_empty() || line.starts_with("--")
    })
}

impl Db {
    /// Create every table, column and index that is missing.
    ///
    /// The worker and the API can start at the same moment against a fresh database, and
    /// `CREATE TABLE IF NOT EXISTS` is not atomic against a concurrent creator, so the
    /// statements run under a session advisory lock on a dedicated connection. Each one
    /// runs on its own in autocommit, like Python's: a failure is logged and the rest still
    /// run, and the call reports every failure at the end.
    pub async fn ensure_schema(&self) -> Result<(), DbError> {
        let mut connection = self.dedicated_connection().await?;
        let id = advisory_lock_id(SCHEMA_LOCK);
        sqlx::query("SELECT pg_advisory_lock($1);")
            .bind(id)
            .execute(&mut connection)
            .await?;
        let mut failures = Vec::new();
        for statement in split_statements(SCHEMA_SQL) {
            if let Err(error) = sqlx::raw_sql(AssertSqlSafe(statement)).execute(&mut connection).await {
                failures.push(failure(statement, &error));
            }
        }
        if let Err(error) = ensure_bilibili_bvid_arbiter(&mut connection).await {
            failures.push(failure("bilibili bvid unique constraint", &error));
        }
        // Closing the connection releases the lock even if the unlock fails.
        let _ = sqlx::query("SELECT pg_advisory_unlock($1);")
            .bind(id)
            .execute(&mut connection)
            .await;
        let _ = connection.close().await;
        if failures.is_empty() {
            Ok(())
        } else {
            Err(DbError::Schema(failures))
        }
    }
}

fn failure(statement: &str, error: &sqlx::Error) -> String {
    let head = statement
        .lines()
        .find(|line| !line.trim_start().starts_with("--"))
        .unwrap_or(statement)
        .trim();
    let message = format!("{head}: {error}");
    logging::get("database").error(format_args!("Schema statement failed: {message}"));
    message
}

/// `Bilibili._ensure_table`'s second half: a table from before `bvid` was the primary key
/// gets the unique constraint its ON CONFLICT needs, under Python's own lock so a Python
/// bilibili run starting at the same time cannot race it.
async fn ensure_bilibili_bvid_arbiter(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    let mut transaction = connection.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1);")
        .bind(advisory_lock_id(BILIBILI_SCHEMA_LOCK))
        .execute(&mut *transaction)
        .await?;
    sqlx::query("LOCK TABLE bilibili IN SHARE ROW EXCLUSIVE MODE;")
        .execute(&mut *transaction)
        .await?;
    let has_arbiter: bool = sqlx::query_scalar(BILIBILI_BVID_ARBITER_SQL).fetch_one(&mut *transaction).await?;
    if !has_arbiter {
        sqlx::query("ALTER TABLE bilibili ADD CONSTRAINT bilibili_bvid_unique UNIQUE (bvid);")
            .execute(&mut *transaction)
            .await?;
        logging::get("bilibili").info("Created bilibili bvid unique constraint");
    }
    transaction.commit().await
}

#[cfg(test)]
mod tests;
