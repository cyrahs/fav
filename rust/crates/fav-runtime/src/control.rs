//! The control-request queue: manual triggers from the UI, and the bookkeeping rows of
//! scheduled runs.
//!
//! Python origin: `src/tool/control_queue.py`. The API inserts a `pending` row; the worker
//! claims it with `FOR UPDATE SKIP LOCKED`, runs it and records the outcome. A cron-fired
//! run is recorded as a row born `running`, so the claim loop never picks it up. The table
//! itself comes from [`fav_db::Db::ensure_schema`]; Python's per-call `CREATE TABLE IF NOT
//! EXISTS` is not repeated (DIVERGENCES.md D6).

use chrono::{DateTime, Utc};
use fav_core::pyjson::{self, Dict, DumpOptions, Value};
use fav_db::Db;
use sqlx::{AssertSqlSafe, Row as _};
use sqlx::postgres::PgRow;

use crate::Error;

pub const STATUS_FAILED: &str = "failed";
pub const STATUS_INTERRUPTED: &str = "interrupted";
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_REJECTED: &str = "rejected";
pub const STATUS_RUNNING: &str = "running";
pub const STATUS_SUCCEEDED: &str = "succeeded";
pub const VALID_STATUSES: [&str; 6] = [
    STATUS_PENDING,
    STATUS_RUNNING,
    STATUS_SUCCEEDED,
    STATUS_FAILED,
    STATUS_REJECTED,
    STATUS_INTERRUPTED,
];
pub const KIND_TRIGGER_JOB: &str = "trigger_job";
pub const KIND_SCHEDULED_JOB: &str = "scheduled_job";
pub const VALID_KINDS: [&str; 2] = [KIND_TRIGGER_JOB, KIND_SCHEDULED_JOB];
pub const INTERRUPTED_BY_RESTART_ERROR: &str = "Interrupted: the worker stopped before this run finished";

const COLUMNS: &str = "id, kind, target, payload, status, requested_at, started_at, finished_at, result, error";

const CLAIM_NEXT_REQUEST_SQL: &str = "
WITH next_request AS (
    SELECT id
    FROM control_requests
    WHERE status = $1
    ORDER BY requested_at ASC, id ASC
    FOR UPDATE SKIP LOCKED
    LIMIT 1
)
UPDATE control_requests AS requests
SET status = $2,
    started_at = CURRENT_TIMESTAMP,
    finished_at = NULL,
    result = '',
    error = ''
FROM next_request
WHERE requests.id = next_request.id
RETURNING requests.id, requests.kind, requests.target, requests.payload, requests.status, requests.requested_at,
          requests.started_at, requests.finished_at, requests.result, requests.error;
";

/// One row of `control_requests`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlRequest {
    pub request_id: i64,
    pub kind: String,
    pub target: String,
    pub payload: String,
    pub status: String,
    pub requested_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub result: String,
    pub error: String,
}

impl ControlRequest {
    /// The payload as a dictionary; anything that is not a JSON object reads as `{}`.
    pub fn payload_json(&self) -> Dict {
        match pyjson::loads(&self.payload) {
            Ok(Value::Dict(dict)) => dict,
            _ => Dict::new(),
        }
    }

    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        let payload: String = row.try_get("payload")?;
        Ok(Self {
            request_id: row.try_get("id")?,
            kind: row.try_get("kind")?,
            target: row.try_get("target")?,
            payload: if payload.is_empty() { "{}".to_owned() } else { payload },
            status: row.try_get("status")?,
            requested_at: row.try_get("requested_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
            result: row.try_get("result")?,
            error: row.try_get("error")?,
        })
    }
}

fn check_status(status: &str) -> Result<(), Error> {
    if VALID_STATUSES.contains(&status) {
        Ok(())
    } else {
        Err(Error::Invalid(format!("Unsupported control request status: {status}")))
    }
}

/// `create_control_request_sync`: queue a request as `pending`.
pub async fn create(db: &Db, kind: &str, target: &str, payload: Option<&Dict>) -> Result<ControlRequest, Error> {
    if !VALID_KINDS.contains(&kind) {
        return Err(Error::Invalid(format!("Unsupported control request kind: {kind}")));
    }
    let payload = Value::Dict(payload.cloned().unwrap_or_default());
    let payload_text = pyjson::dumps(&payload, &DumpOptions::compact()).map_err(|error| Error::Invalid(error.to_string()))?;
    let row = sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO control_requests (kind, target, payload, status) VALUES ($1, $2, $3, $4) RETURNING {COLUMNS};"
    )))
    .bind(kind)
    .bind(target)
    .bind(payload_text)
    .bind(STATUS_PENDING)
    .fetch_optional(db.pool())
    .await?;
    let row = row.ok_or_else(|| Error::Failed("Failed to create control request".to_owned()))?;
    Ok(ControlRequest::from_row(&row)?)
}

/// `list_control_requests_sync`: newest first, optionally only some statuses; `limit` is
/// clamped to 1..=200. Unknown statuses are refused before the database is touched.
pub async fn list(db: &Db, statuses: Option<&[&str]>, limit: i64) -> Result<Vec<ControlRequest>, Error> {
    let statuses = statuses.unwrap_or_default();
    let mut invalid: Vec<&str> = statuses.iter().copied().filter(|status| !VALID_STATUSES.contains(status)).collect();
    if !invalid.is_empty() {
        invalid.sort_unstable();
        invalid.dedup();
        return Err(Error::Invalid(format!("Unsupported control request status: {}", invalid.join(", "))));
    }
    let bounded = limit.clamp(1, 200);
    let rows = if statuses.is_empty() {
        sqlx::query(AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM control_requests ORDER BY requested_at DESC, id DESC LIMIT $1;"
        )))
        .bind(bounded)
        .fetch_all(db.pool())
        .await?
    } else {
        sqlx::query(AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM control_requests WHERE status = ANY($1) ORDER BY requested_at DESC, id DESC LIMIT $2;"
        )))
        .bind(statuses.iter().map(|status| (*status).to_owned()).collect::<Vec<_>>())
        .bind(bounded)
        .fetch_all(db.pool())
        .await?
    };
    Ok(rows.iter().map(ControlRequest::from_row).collect::<Result<_, _>>()?)
}

/// `get_control_request_sync`.
pub async fn get(db: &Db, request_id: i64) -> Result<Option<ControlRequest>, Error> {
    let row = sqlx::query(AssertSqlSafe(format!("SELECT {COLUMNS} FROM control_requests WHERE id = $1;")))
        .bind(request_id)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.as_ref().map(ControlRequest::from_row).transpose()?)
}

/// `claim_next_control_request`: the oldest pending request, now `running`.
pub async fn claim_next(db: &Db) -> Result<Option<ControlRequest>, Error> {
    let row = sqlx::query(CLAIM_NEXT_REQUEST_SQL)
        .bind(STATUS_PENDING)
        .bind(STATUS_RUNNING)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.as_ref().map(ControlRequest::from_row).transpose()?)
}

/// `record_scheduled_run_start`: a bookkeeping row for a cron-fired run, born `running`
/// so the claim loop can never execute it a second time.
pub async fn record_scheduled_run_start(db: &Db, target: &str) -> Result<i64, Error> {
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO control_requests (kind, target, status, started_at)
         VALUES ($1, $2, $3, CURRENT_TIMESTAMP)
         RETURNING id;",
    )
    .bind(KIND_SCHEDULED_JOB)
    .bind(target)
    .bind(STATUS_RUNNING)
    .fetch_optional(db.pool())
    .await?;
    id.ok_or_else(|| Error::Failed("Failed to record scheduled run".to_owned()))
}

/// `update_control_request`: record how a request ended.
pub async fn update(db: &Db, request_id: i64, status: &str, result: &str, error: &str) -> Result<(), Error> {
    check_status(status)?;
    sqlx::query(
        "UPDATE control_requests
         SET status = $1,
             finished_at = CURRENT_TIMESTAMP,
             result = $2,
             error = $3
         WHERE id = $4;",
    )
    .bind(status)
    .bind(result)
    .bind(error)
    .bind(request_id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// `interrupt_running_control_requests`: close out every `running` row a stopped worker
/// left behind, however recent.
///
/// Only call this at worker startup, before anything can claim or record a run: the
/// deployment runs a single worker, so at that point no row can be running for real.
pub async fn interrupt_running(db: &Db) -> Result<usize, Error> {
    let rows = sqlx::query(
        "UPDATE control_requests
         SET status = $1,
             finished_at = CURRENT_TIMESTAMP,
             error = CASE WHEN error = '' THEN $2 ELSE error END
         WHERE status = $3
         RETURNING id;",
    )
    .bind(STATUS_INTERRUPTED)
    .bind(INTERRUPTED_BY_RESTART_ERROR)
    .bind(STATUS_RUNNING)
    .fetch_all(db.pool())
    .await?;
    Ok(rows.len())
}

#[cfg(test)]
mod tests;
