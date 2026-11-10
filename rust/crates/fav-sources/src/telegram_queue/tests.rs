//! `tests/test_telegram_queue.py`, against a real database: Python's tests read the SQL it
//! sends to a fake; these run that SQL and check what it does.

use sqlx::Row as _;

use super::{
    JobType, QueueSource, TelegramMediaJob, claim_next, enqueue, mark_completed, mark_discarded, mark_retry, reset_processing, retry_delay,
};
use crate::testutil::database;

fn job() -> TelegramMediaJob {
    TelegramMediaJob {
        account_name: "default".to_owned(),
        channel_id: 123,
        message_id: 456,
        grouped_id: Some(99),
        media_type: "image".to_owned(),
        title: "Album-1".to_owned(),
        source: "event".to_owned(),
        priority: 100,
        attempt_count: 2,
    }
}

async fn row(db: &fav_db::Db, message_id: i64) -> sqlx::postgres::PgRow {
    sqlx::query("SELECT * FROM telegram_media_queue WHERE account_name = 'default' AND channel_id = 123 AND message_id = $1")
        .bind(message_id)
        .fetch_one(db.pool())
        .await
        .expect("row")
}

// py: tests/test_telegram_queue.py::test_queue_schema_creates_durable_table_and_claim_index
#[tokio::test]
async fn queue_schema_creates_durable_table_and_claim_index() {
    let database = database().await;
    let db = database.db();
    let primary: Vec<String> = sqlx::query_scalar(
        "SELECT a.attname FROM pg_index i
        JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
        WHERE i.indrelid = 'telegram_media_queue'::regclass AND i.indisprimary
        ORDER BY array_position(i.indkey, a.attnum)",
    )
    .fetch_all(db.pool())
    .await
    .expect("primary key");
    assert_eq!(primary, ["account_name", "channel_id", "message_id"]);
    let predicate: String = sqlx::query_scalar(
        "SELECT pg_get_expr(i.indpred, i.indrelid) FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        WHERE c.relname = 'telegram_media_queue_claim_idx'",
    )
    .fetch_one(db.pool())
    .await
    .expect("claim index");
    assert_eq!(predicate, "(status = 'pending'::text)");
    // Durable: an ordinary (logged) table, not a temporary or unlogged one.
    let persistence: String = sqlx::query_scalar("SELECT relpersistence::text FROM pg_class WHERE relname = 'telegram_media_queue'")
        .fetch_one(db.pool())
        .await
        .expect("table");
    assert_eq!(persistence, "p");
}

// py: tests/test_telegram_queue.py::test_enqueue_is_idempotent_and_event_has_higher_priority
#[tokio::test]
async fn enqueue_is_idempotent_and_event_has_higher_priority() {
    let database = database().await;
    let db = database.db();
    let inserted = enqueue(db, "default", 123, 456, None, JobType::Image, "Image", QueueSource::Event, 0.0)
        .await
        .expect("enqueue");
    let duplicate = enqueue(
        db,
        "default",
        123,
        456,
        None,
        JobType::Image,
        "Image",
        QueueSource::Reconciliation,
        0.0,
    )
    .await
    .expect("enqueue");
    assert!(inserted);
    assert!(!duplicate);
    let stored = row(db, 456).await;
    assert_eq!(stored.get::<i32, _>("priority"), 100);
    assert_eq!(stored.get::<String, _>("source"), "event");
    assert_eq!(QueueSource::Event.priority(), 100);
    assert_eq!(QueueSource::Reconciliation.priority(), 0);

    // The refresh touches only rows still pending or processing.
    sqlx::query("UPDATE telegram_media_queue SET status = 'completed'")
        .execute(db.pool())
        .await
        .expect("complete");
    let again = enqueue(db, "default", 123, 456, None, JobType::Video, "Renamed", QueueSource::Event, 0.0)
        .await
        .expect("enqueue");
    assert!(!again);
    let stored = row(db, 456).await;
    assert_eq!(stored.get::<String, _>("title"), "Image");
    assert_eq!(stored.get::<String, _>("media_type"), "image");
}

// py: tests/test_telegram_queue.py::test_claim_uses_priority_skip_locked_and_sets_owner_token
#[tokio::test]
async fn claim_uses_priority_skip_locked_and_sets_owner_token() {
    let database = database().await;
    let db = database.db();
    enqueue(
        db,
        "default",
        123,
        455,
        None,
        JobType::Image,
        "Low",
        QueueSource::Reconciliation,
        0.0,
    )
    .await
    .expect("enqueue");
    enqueue(
        db,
        "default",
        123,
        456,
        Some(99),
        JobType::Image,
        "Album-1",
        QueueSource::Event,
        0.0,
    )
    .await
    .expect("enqueue");
    enqueue(db, "default", 123, 457, None, JobType::Image, "Locked", QueueSource::Event, 0.0)
        .await
        .expect("enqueue");
    enqueue(
        db,
        "other",
        123,
        458,
        None,
        JobType::Image,
        "Other account",
        QueueSource::Event,
        0.0,
    )
    .await
    .expect("enqueue");
    // Another worker holds 457.
    let mut holder = db.pool().begin().await.expect("transaction");
    sqlx::query("SELECT 1 FROM telegram_media_queue WHERE message_id = 457 FOR UPDATE")
        .execute(&mut *holder)
        .await
        .expect("lock");
    // An earlier claim of 456 went back to the queue, so this is its second attempt.
    sqlx::query("UPDATE telegram_media_queue SET attempt_count = 1 WHERE message_id = 456")
        .execute(db.pool())
        .await
        .expect("attempt");

    let claimed = claim_next(db, "default", "owner-token").await.expect("claim");
    assert_eq!(claimed, Some(job()));
    let stored = row(db, 456).await;
    assert_eq!(stored.get::<String, _>("status"), "processing");
    assert_eq!(stored.get::<Option<String>, _>("owner_token").as_deref(), Some("owner-token"));
    assert_eq!(stored.get::<i32, _>("attempt_count"), 2);

    // 457 is locked and skipped; the low-priority job is next.
    let next = claim_next(db, "default", "owner-token").await.expect("claim").expect("a job");
    assert_eq!(next.message_id, 455);
    holder.rollback().await.expect("rollback");
    let last = claim_next(db, "default", "owner-token").await.expect("claim").expect("a job");
    assert_eq!(last.message_id, 457);
    assert_eq!(claim_next(db, "default", "owner-token").await.expect("claim"), None);
}

// py: tests/test_telegram_queue.py::test_processing_jobs_are_recovered_for_new_account_owner
#[tokio::test]
async fn processing_jobs_are_recovered_for_new_account_owner() {
    let database = database().await;
    let db = database.db();
    for id in [1, 2, 3] {
        enqueue(db, "default", 123, id, None, JobType::Image, "Image", QueueSource::Event, 0.0)
            .await
            .expect("enqueue");
    }
    enqueue(db, "other", 123, 4, None, JobType::Image, "Image", QueueSource::Event, 0.0)
        .await
        .expect("enqueue");
    sqlx::query("UPDATE telegram_media_queue SET status = 'processing', owner_token = 'old' WHERE message_id IN (1, 2, 4)")
        .execute(db.pool())
        .await
        .expect("processing");

    let count = reset_processing(db, "default").await.expect("reset");
    assert_eq!(count, 2);
    for id in [1, 2] {
        let stored = row(db, id).await;
        assert_eq!(stored.get::<String, _>("status"), "pending");
        assert_eq!(stored.get::<Option<String>, _>("owner_token"), None);
        assert_eq!(stored.get::<String, _>("last_error"), "Recovered after account owner restart");
    }
    let other: String = sqlx::query_scalar("SELECT status FROM telegram_media_queue WHERE account_name = 'other'")
        .fetch_one(db.pool())
        .await
        .expect("other");
    assert_eq!(other, "processing");
}

// py: tests/test_telegram_queue.py::test_retry_and_discard_are_scoped_to_owner_token
#[tokio::test]
async fn retry_and_discard_are_scoped_to_owner_token() {
    let database = database().await;
    let db = database.db();
    enqueue(
        db,
        "default",
        123,
        456,
        Some(99),
        JobType::Image,
        "Album-1",
        QueueSource::Event,
        0.0,
    )
    .await
    .expect("enqueue");
    let claimed = claim_next(db, "default", "owner").await.expect("claim").expect("a job");

    // Another owner's token changes nothing.
    assert!(!mark_retry(db, &claimed, "intruder", "network", 30.0).await.expect("retry"));
    assert!(!mark_discarded(db, &claimed, "intruder", "deleted").await.expect("discard"));
    assert!(!mark_completed(db, &claimed, "intruder").await.expect("complete"));
    assert_eq!(row(db, 456).await.get::<String, _>("status"), "processing");

    let retried = mark_retry(db, &claimed, "owner", "network", 30.0).await.expect("retry");
    assert!(retried);
    let stored = row(db, 456).await;
    assert_eq!(stored.get::<String, _>("status"), "pending");
    assert_eq!(stored.get::<String, _>("last_error"), "network");
    let due_in: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM available_at - CURRENT_TIMESTAMP)::DOUBLE PRECISION FROM telegram_media_queue WHERE message_id = 456",
    )
    .fetch_one(db.pool())
    .await
    .expect("due");
    assert!((20.0..=31.0).contains(&due_in), "{due_in}");

    sqlx::query("UPDATE telegram_media_queue SET available_at = CURRENT_TIMESTAMP")
        .execute(db.pool())
        .await
        .expect("due now");
    let claimed = claim_next(db, "default", "owner").await.expect("claim").expect("a job");
    let discarded = mark_discarded(db, &claimed, "owner", "deleted").await.expect("discard");
    assert!(discarded);
    let stored = row(db, 456).await;
    assert_eq!(stored.get::<String, _>("status"), "discarded");
    assert_eq!(stored.get::<String, _>("last_error"), "deleted");
    assert_eq!(stored.get::<Option<String>, _>("owner_token"), None);
}

// py: tests/test_telegram_queue.py::test_retry_backoff_is_capped_at_thirty_minutes
#[test]
fn retry_backoff_is_capped_at_thirty_minutes() {
    assert!((retry_delay(1) - 30.0).abs() < f64::EPSILON);
    assert!((retry_delay(2) - 60.0).abs() < f64::EPSILON);
    assert!((retry_delay(20) - 1800.0).abs() < f64::EPSILON);
    assert!((retry_delay(0) - 30.0).abs() < f64::EPSILON);
    assert!((retry_delay(6) - 960.0).abs() < f64::EPSILON);
    assert!((retry_delay(7) - 1800.0).abs() < f64::EPSILON);
}
