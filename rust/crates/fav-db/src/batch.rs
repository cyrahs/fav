//! Running many statements in chunks, and multi-row inserts.
//!
//! Python origin: `query_db_batch`, `query_db_transaction` and `insert_db_batch` in
//! `src/tool/database.py`. Python's statements were SQLite-flavoured with `?` placeholders
//! and translated on the fly; here they are native PostgreSQL with `$n` placeholders, and
//! each statement numbers its own. Error messages are Python's.

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::query::Query;
use sqlx::{AssertSqlSafe, Postgres, Row as _};

use crate::{Db, DbError};

/// PostgreSQL's limit on bind parameters per statement.
pub const DEFAULT_MAX_BIND_PARAMS: usize = 65535;
/// `query_db_batch`'s default number of statements per connection checkout.
pub const DEFAULT_QUERY_CHUNK_SIZE: usize = 50;
/// `insert_db_batch`'s default number of rows per statement.
pub const DEFAULT_INSERT_CHUNK_SIZE: usize = 200;

/// A bind parameter. `None` binds a NULL of that type.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Bool(Option<bool>),
    Int(Option<i64>),
    Float(Option<f64>),
    Text(Option<String>),
    Bytes(Option<Vec<u8>>),
    Timestamp(Option<DateTime<Utc>>),
}

impl From<&str> for Param {
    fn from(value: &str) -> Self {
        Self::Text(Some(value.to_owned()))
    }
}

impl From<String> for Param {
    fn from(value: String) -> Self {
        Self::Text(Some(value))
    }
}

impl From<i64> for Param {
    fn from(value: i64) -> Self {
        Self::Int(Some(value))
    }
}

impl From<bool> for Param {
    fn from(value: bool) -> Self {
        Self::Bool(Some(value))
    }
}

impl From<f64> for Param {
    fn from(value: f64) -> Self {
        Self::Float(Some(value))
    }
}

/// One statement and its parameters. The SQL is the caller's own text; values only ever
/// travel as parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub sql: String,
    pub params: Vec<Param>,
}

impl Statement {
    pub fn new(sql: impl Into<String>, params: Vec<Param>) -> Self {
        Self { sql: sql.into(), params }
    }

    fn query(&self) -> Query<'_, Postgres, PgArguments> {
        self.params
            .iter()
            .fold(sqlx::query(AssertSqlSafe(self.sql.as_str())), |query, param| match param {
                Param::Bool(value) => query.bind(*value),
                Param::Int(value) => query.bind(*value),
                Param::Float(value) => query.bind(*value),
                Param::Text(value) => query.bind(value.clone()),
                Param::Bytes(value) => query.bind(value.clone()),
                Param::Timestamp(value) => query.bind(*value),
            })
    }
}

fn invalid(message: impl Into<String>) -> DbError {
    DbError::Invalid(message.into())
}

/// Python's normalization: surrounding whitespace and trailing `;` go, empty statements are dropped.
fn normalized(statements: Vec<Statement>) -> Vec<Statement> {
    statements
        .into_iter()
        .filter_map(|statement| {
            let sql = statement.sql.trim();
            (!sql.is_empty()).then(|| Statement {
                sql: sql.trim_end_matches(';').to_owned(),
                params: statement.params,
            })
        })
        .collect()
}

/// Run `statements` in chunks of at most `chunk_size` statements and `max_bind_params`
/// parameters, each chunk through `run`, and return one result per statement.
pub async fn query_batch_with<R>(
    statements: Vec<Statement>,
    chunk_size: usize,
    max_bind_params: usize,
    mut run: impl AsyncFnMut(Vec<Statement>) -> Result<Vec<R>, DbError>,
) -> Result<Vec<R>, DbError> {
    if chunk_size == 0 {
        return Err(invalid("chunk_size must be greater than 0"));
    }
    if max_bind_params == 0 {
        return Err(invalid("max_bind_params must be greater than 0"));
    }
    let mut results = Vec::new();
    let mut chunk: Vec<Statement> = Vec::new();
    let mut chunk_params = 0;
    let mut flush = async |chunk: Vec<Statement>, results: &mut Vec<R>| -> Result<(), DbError> {
        let expected = chunk.len();
        let chunk_results = run(chunk).await?;
        if chunk_results.len() != expected {
            return Err(invalid(format!(
                "Database batch result mismatch: expected {expected} statements, got {}",
                chunk_results.len()
            )));
        }
        results.extend(chunk_results);
        Ok(())
    };
    for statement in normalized(statements) {
        let count = statement.params.len();
        if count > max_bind_params {
            return Err(invalid(format!(
                "Statement has {count} bind params, exceeds max_bind_params={max_bind_params}"
            )));
        }
        if !chunk.is_empty() && (chunk.len() >= chunk_size || chunk_params + count > max_bind_params) {
            flush(std::mem::take(&mut chunk), &mut results).await?;
            chunk_params = 0;
        }
        chunk.push(statement);
        chunk_params += count;
    }
    if !chunk.is_empty() {
        flush(chunk, &mut results).await?;
    }
    Ok(results)
}

/// The statements of a multi-row insert of `rows` into `table`, at most `chunk_size` rows
/// and `max_bind_params` parameters each. `on_conflict` follows `ON CONFLICT`.
pub fn insert_statements(
    table: &str,
    columns: &[&str],
    rows: Vec<Vec<Param>>,
    on_conflict: Option<&str>,
    chunk_size: usize,
    max_bind_params: usize,
) -> Result<Vec<Statement>, DbError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    if columns.is_empty() {
        return Err(invalid("columns cannot be empty"));
    }
    if chunk_size == 0 {
        return Err(invalid("chunk_size must be greater than 0"));
    }
    if max_bind_params == 0 {
        return Err(invalid("default max_bind_params must be greater than 0"));
    }
    let width = columns.len();
    if rows.iter().any(|row| row.len() != width) {
        return Err(invalid("all rows must have the same length as columns"));
    }
    let max_rows = max_bind_params / width;
    if max_rows == 0 {
        return Err(invalid(format!(
            "columns count ({width}) exceeds max_bind_params={max_bind_params}; cannot build insert statement"
        )));
    }
    let rows_per_statement = chunk_size.min(max_rows);
    let columns_sql = columns.join(", ");
    let conflict_sql = on_conflict
        .filter(|clause| !clause.is_empty())
        .map(|clause| format!(" ON CONFLICT {clause}"))
        .unwrap_or_default();
    let mut statements = Vec::new();
    let mut rows = rows.into_iter().peekable();
    while rows.peek().is_some() {
        let chunk: Vec<Vec<Param>> = rows.by_ref().take(rows_per_statement).collect();
        let values_sql = (0..chunk.len())
            .map(|row| {
                format!(
                    "({})",
                    (1..=width)
                        .map(|column| format!("${}", row * width + column))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        statements.push(Statement {
            sql: format!("INSERT INTO {table} ({columns_sql}) VALUES {values_sql}{conflict_sql};"),
            params: chunk.into_iter().flatten().collect(),
        });
    }
    Ok(statements)
}

impl Db {
    /// `query_db_batch`: each chunk runs on one pooled connection, statement by statement.
    pub async fn query_batch(
        &self,
        statements: Vec<Statement>,
        chunk_size: usize,
        max_bind_params: usize,
    ) -> Result<Vec<Vec<PgRow>>, DbError> {
        query_batch_with(statements, chunk_size, max_bind_params, async |chunk: Vec<Statement>| {
            let mut connection = self.pool().acquire().await?;
            let mut results = Vec::with_capacity(chunk.len());
            for statement in &chunk {
                results.push(statement.query().fetch_all(&mut *connection).await?);
            }
            Ok(results)
        })
        .await
    }

    /// `query_db_transaction`: all statements in one transaction.
    pub async fn query_transaction(&self, statements: Vec<Statement>) -> Result<Vec<Vec<PgRow>>, DbError> {
        let statements = normalized(statements);
        if statements.is_empty() {
            return Ok(Vec::new());
        }
        let mut transaction = self.pool().begin().await?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in &statements {
            results.push(statement.query().fetch_all(&mut *transaction).await?);
        }
        transaction.commit().await?;
        Ok(results)
    }

    /// `insert_db_batch`.
    pub async fn insert_batch(
        &self,
        table: &str,
        columns: &[&str],
        rows: Vec<Vec<Param>>,
        on_conflict: Option<&str>,
        chunk_size: usize,
    ) -> Result<(), DbError> {
        for statement in insert_statements(table, columns, rows, on_conflict, chunk_size, DEFAULT_MAX_BIND_PARAMS)? {
            statement.query().execute(self.pool()).await?;
        }
        Ok(())
    }
}

/// The text of column `name` in `row`, for tests and simple callers.
pub fn text_column(row: &PgRow, name: &str) -> Result<Option<String>, DbError> {
    Ok(row.try_get(name)?)
}

#[cfg(test)]
mod tests;
