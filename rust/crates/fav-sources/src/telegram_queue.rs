//! The durable queue of Telegram messages to save.
//!
//! Python origin: `src/tool/telegram_queue.py`. A message is queued by the realtime
//! listener (`event`, high priority) or by a reconciliation over the channel's history
//! (`reconciliation`). The account's single worker claims the next due row with
//! `FOR UPDATE SKIP LOCKED`, marks it `processing` under its owner token, and then completes,
//! discards or reschedules it; a worker that starts again first returns rows a previous owner
//! left `processing`. The table comes from [`fav_db::Db::ensure_schema`]
//! (`ensure_telegram_media_queue_table` is not repeated per call, DIVERGENCES.md D6).
//!
//! A reported `unsupported` job stays `completed` while its report is open: it is checked
//! again until the message is saved after all or deleted.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use fav_db::Db;
use sqlx::Row as _;
use sqlx::postgres::PgRow;

use crate::pydecode::PyError;

/// What a queued message is: media of a configured type, a link to a post whose media is
/// fetched from its site, or a message with nothing to save that is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobType {
    Video,
    Image,
    Link,
    Unsupported,
}

impl JobType {
    pub const ALL: [Self; 4] = [Self::Video, Self::Image, Self::Link, Self::Unsupported];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Image => "image",
            Self::Link => "link",
            Self::Unsupported => "unsupported",
        }
    }

    /// The type a stored `media_type` names, if it is one (`_JOB_TYPES`).
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }

    /// The job type of a channel route's media type.
    pub fn of_media(media: fav_core::settings::TelegramMediaType) -> Self {
        match media {
            fav_core::settings::TelegramMediaType::Video => Self::Video,
            fav_core::settings::TelegramMediaType::Image => Self::Image,
        }
    }
}

/// Who queued a message (`TelegramQueueSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueSource {
    Event,
    Reconciliation,
}

impl QueueSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::Reconciliation => "reconciliation",
        }
    }

    /// `SOURCE_PRIORITY`.
    pub fn priority(self) -> i32 {
        match self {
            Self::Event => 100,
            Self::Reconciliation => 0,
        }
    }
}

/// One claimed row (`TelegramMediaJob`). `media_type` and `source` are the stored text,
/// whatever it is: a type no longer known is discarded by the worker, as in Python.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramMediaJob {
    pub account_name: String,
    pub channel_id: i64,
    pub message_id: i64,
    pub grouped_id: Option<i64>,
    pub media_type: String,
    pub title: String,
    pub source: String,
    pub priority: i64,
    pub attempt_count: i64,
}

/// The class and text psycopg would raise for a failed queue statement.
pub(crate) fn sql_error(error: sqlx::Error) -> PyError {
    let job = crate::db_job_error(&fav_db::DbError::from(error));
    PyError::new(job.class_name(), job.message())
}

/// `text[:n]` in code points.
pub(crate) fn head(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

/// `enqueue_telegram_media_job`: persist a job; whether a new row was inserted.
///
/// A message already queued keeps its row: its title and type are refreshed, a higher
/// priority (an event over a reconciliation) wins, and a pending row may only become due
/// earlier, never later.
#[allow(clippy::too_many_arguments)] // reason: Python's keyword arguments, one per column
pub async fn enqueue(
    db: &Db,
    account_name: &str,
    channel_id: i64,
    message_id: i64,
    grouped_id: Option<i64>,
    media_type: JobType,
    title: &str,
    source: QueueSource,
    available_delay_seconds: f64,
) -> Result<bool, PyError> {
    enqueue_at(
        db,
        account_name,
        channel_id,
        message_id,
        grouped_id,
        media_type,
        title,
        source,
        Utc::now() + seconds(available_delay_seconds.max(0.0)),
    )
    .await
}

/// A non-negative number of seconds as a duration, saturating far in the future.
fn seconds(value: f64) -> ChronoDuration {
    let micros = (value * 1_000_000.0).round();
    if micros >= 9.0e15 {
        ChronoDuration::days(36_500)
    } else {
        // In range of i64 by the check above; never negative.
        #[allow(clippy::cast_possible_truncation)] // reason: bounded by the check above
        ChronoDuration::microseconds(micros as i64)
    }
}

#[allow(clippy::too_many_arguments)] // reason: one argument per column, as `enqueue`
async fn enqueue_at(
    db: &Db,
    account_name: &str,
    channel_id: i64,
    message_id: i64,
    grouped_id: Option<i64>,
    media_type: JobType,
    title: &str,
    source: QueueSource,
    available_at: DateTime<Utc>,
) -> Result<bool, PyError> {
    let priority = source.priority();
    let inserted = sqlx::query(
        "INSERT INTO telegram_media_queue (
            account_name, channel_id, message_id, grouped_id, media_type, title, source, priority, available_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (account_name, channel_id, message_id) DO NOTHING
        RETURNING message_id",
    )
    .bind(account_name)
    .bind(channel_id)
    .bind(message_id)
    .bind(grouped_id)
    .bind(media_type.as_str())
    .bind(title)
    .bind(source.as_str())
    .bind(priority)
    .bind(available_at)
    .fetch_all(db.pool())
    .await
    .map_err(sql_error)?;
    if !inserted.is_empty() {
        return Ok(true);
    }
    sqlx::query(
        "UPDATE telegram_media_queue
        SET
            grouped_id = COALESCE(telegram_media_queue.grouped_id, $1),
            media_type = $2,
            title = $3,
            source = CASE WHEN priority < $4 THEN $5 ELSE source END,
            priority = GREATEST(priority, $4),
            available_at = CASE
                WHEN status = 'pending' AND priority <= $4 THEN LEAST(available_at, $6)
                ELSE available_at
            END,
            updated_at = CURRENT_TIMESTAMP
        WHERE account_name = $7 AND channel_id = $8 AND message_id = $9
          AND status IN ('pending', 'processing')",
    )
    .bind(grouped_id)
    .bind(media_type.as_str())
    .bind(title)
    .bind(priority)
    .bind(source.as_str())
    .bind(available_at)
    .bind(account_name)
    .bind(channel_id)
    .bind(message_id)
    .execute(db.pool())
    .await
    .map_err(sql_error)?;
    Ok(false)
}

fn job_from_row(row: &PgRow) -> Result<TelegramMediaJob, sqlx::Error> {
    Ok(TelegramMediaJob {
        account_name: row.try_get("account_name")?,
        channel_id: row.try_get("channel_id")?,
        message_id: row.try_get("message_id")?,
        grouped_id: row.try_get("grouped_id")?,
        media_type: row.try_get("media_type")?,
        title: row.try_get("title")?,
        source: row.try_get("source")?,
        priority: i64::from(row.try_get::<i32, _>("priority")?),
        // The row as claimed, before this claim's increment.
        attempt_count: i64::from(row.try_get::<i32, _>("attempt_count")?) + 1,
    })
}

/// `claim_next_telegram_media_job`: the account's next due job, now `processing` under
/// `owner_token`, in one transaction. Higher priority first, then the earliest due.
pub async fn claim_next(db: &Db, account_name: &str, owner_token: &str) -> Result<Option<TelegramMediaJob>, PyError> {
    let mut transaction = db.pool().begin().await.map_err(sql_error)?;
    let row = sqlx::query(
        "SELECT account_name, channel_id, message_id, grouped_id, media_type, title, source, priority, attempt_count
        FROM telegram_media_queue
        WHERE account_name = $1
          AND status = 'pending'
          AND available_at <= CURRENT_TIMESTAMP
        ORDER BY priority DESC, available_at, created_at, channel_id, message_id
        FOR UPDATE SKIP LOCKED
        LIMIT 1",
    )
    .bind(account_name)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(sql_error)?;
    let Some(row) = row else {
        transaction.commit().await.map_err(sql_error)?;
        return Ok(None);
    };
    let job = job_from_row(&row).map_err(sql_error)?;
    sqlx::query(
        "UPDATE telegram_media_queue
        SET status = 'processing', owner_token = $1, attempt_count = attempt_count + 1, updated_at = CURRENT_TIMESTAMP
        WHERE account_name = $2 AND channel_id = $3 AND message_id = $4",
    )
    .bind(owner_token)
    .bind(&job.account_name)
    .bind(job.channel_id)
    .bind(job.message_id)
    .execute(&mut *transaction)
    .await
    .map_err(sql_error)?;
    transaction.commit().await.map_err(sql_error)?;
    Ok(Some(job))
}

/// `reset_processing_telegram_media_jobs`: hand every row a previous owner of the account
/// left `processing` back to the queue; how many there were.
pub async fn reset_processing(db: &Db, account_name: &str) -> Result<usize, PyError> {
    let rows = sqlx::query(
        "UPDATE telegram_media_queue
        SET
            status = 'pending',
            owner_token = NULL,
            available_at = CURRENT_TIMESTAMP,
            last_error = CASE
                WHEN last_error = '' THEN 'Recovered after account owner restart'
                ELSE last_error
            END,
            updated_at = CURRENT_TIMESTAMP
        WHERE account_name = $1 AND status = 'processing'
        RETURNING message_id",
    )
    .bind(account_name)
    .fetch_all(db.pool())
    .await
    .map_err(sql_error)?;
    Ok(rows.len())
}

/// `mark_telegram_media_job_completed`.
pub async fn mark_completed(db: &Db, job: &TelegramMediaJob, owner_token: &str) -> Result<bool, PyError> {
    finish(db, job, owner_token, "completed", "").await
}

/// `mark_telegram_media_job_discarded`.
pub async fn mark_discarded(db: &Db, job: &TelegramMediaJob, owner_token: &str, error: &str) -> Result<bool, PyError> {
    finish(db, job, owner_token, "discarded", error).await
}

/// `_finish_job`: only the owner that claimed the row may finish it.
async fn finish(db: &Db, job: &TelegramMediaJob, owner_token: &str, status: &str, error: &str) -> Result<bool, PyError> {
    let rows = sqlx::query(
        "UPDATE telegram_media_queue
        SET status = $1, owner_token = NULL, last_error = $2, completed_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP
        WHERE account_name = $3 AND channel_id = $4 AND message_id = $5
          AND status = 'processing' AND owner_token = $6
        RETURNING message_id",
    )
    .bind(status)
    .bind(head(error, 1000))
    .bind(&job.account_name)
    .bind(job.channel_id)
    .bind(job.message_id)
    .bind(owner_token)
    .fetch_all(db.pool())
    .await
    .map_err(sql_error)?;
    Ok(!rows.is_empty())
}

/// `mark_telegram_media_job_retry`: back to `pending`, due in `delay_seconds`.
pub async fn mark_retry(db: &Db, job: &TelegramMediaJob, owner_token: &str, error: &str, delay_seconds: f64) -> Result<bool, PyError> {
    let rows = sqlx::query(
        "UPDATE telegram_media_queue
        SET status = 'pending', owner_token = NULL, last_error = $1,
            available_at = CURRENT_TIMESTAMP + ($2 * INTERVAL '1 second'), updated_at = CURRENT_TIMESTAMP
        WHERE account_name = $3 AND channel_id = $4 AND message_id = $5
          AND status = 'processing' AND owner_token = $6
        RETURNING message_id",
    )
    .bind(head(error, 1000))
    .bind(delay_seconds.max(0.0))
    .bind(&job.account_name)
    .bind(job.channel_id)
    .bind(job.message_id)
    .bind(owner_token)
    .fetch_all(db.pool())
    .await
    .map_err(sql_error)?;
    Ok(!rows.is_empty())
}

/// `telegram_media_retry_delay`: 30 s, doubling per attempt, capped at 30 minutes.
pub fn retry_delay(attempt_count: i64) -> f64 {
    let exponent = u32::try_from(attempt_count.saturating_sub(1).max(0)).unwrap_or(u32::MAX);
    // 30 * 2^6 is past the cap already.
    if exponent >= 6 {
        return 1800.0;
    }
    (30.0 * f64::from(1_u32 << exponent)).min(1800.0)
}

/// `open_unsupported_reports`: the ids of a channel's messages reported as having nothing
/// to save whose report is still open, oldest first, at most `limit`.
pub async fn open_unsupported_reports(db: &Db, account_name: &str, channel_id: i64, limit: i64) -> Result<Vec<i64>, PyError> {
    let rows = sqlx::query(
        "SELECT message_id FROM telegram_media_queue
        WHERE account_name = $1 AND channel_id = $2 AND media_type = 'unsupported' AND status = 'completed'
        ORDER BY message_id
        LIMIT $3",
    )
    .bind(account_name)
    .bind(channel_id)
    .bind(limit)
    .fetch_all(db.pool())
    .await
    .map_err(sql_error)?;
    rows.iter()
        .map(|row| row.try_get::<i64, _>("message_id").map_err(sql_error))
        .collect()
}

/// `requeue_unsupported_report`: queue a reported message again as something saveable.
pub async fn requeue_unsupported_report(
    db: &Db,
    account_name: &str,
    channel_id: i64,
    message_id: i64,
    media_type: JobType,
    title: &str,
) -> Result<bool, PyError> {
    let rows = sqlx::query(
        "UPDATE telegram_media_queue
        SET media_type = $1, title = $2, status = 'pending', attempt_count = 0, owner_token = NULL,
            last_error = '', available_at = CURRENT_TIMESTAMP, completed_at = NULL, updated_at = CURRENT_TIMESTAMP
        WHERE account_name = $3 AND channel_id = $4 AND message_id = $5
          AND media_type = 'unsupported' AND status = 'completed'
        RETURNING message_id",
    )
    .bind(media_type.as_str())
    .bind(title)
    .bind(account_name)
    .bind(channel_id)
    .bind(message_id)
    .fetch_all(db.pool())
    .await
    .map_err(sql_error)?;
    Ok(!rows.is_empty())
}

/// `close_unsupported_report`: close the report of a message that is gone; `false` when
/// there was no open report for it.
pub async fn close_unsupported_report(
    db: &Db,
    account_name: &str,
    channel_id: i64,
    message_id: i64,
    reason: &str,
) -> Result<bool, PyError> {
    let rows = sqlx::query(
        "UPDATE telegram_media_queue
        SET status = 'discarded', last_error = $1, updated_at = CURRENT_TIMESTAMP
        WHERE account_name = $2 AND channel_id = $3 AND message_id = $4
          AND media_type = 'unsupported' AND status = 'completed'
        RETURNING message_id",
    )
    .bind(head(reason, 1000))
    .bind(account_name)
    .bind(channel_id)
    .bind(message_id)
    .fetch_all(db.pool())
    .await
    .map_err(sql_error)?;
    Ok(!rows.is_empty())
}

#[cfg(test)]
mod tests;
