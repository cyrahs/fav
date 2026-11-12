use std::sync::Arc;

use fav_core::pyjson::{Dict, Value};
use fav_db::settings_store::default_settings;
use fav_db::testdb::TestDatabase;
use fav_db::{Db, SettingsStore};
use sqlx::Row as _;

use super::*;
use crate::testutil::{database, golden_json};

fn outbox(db: &Db) -> Outbox {
    Outbox::new(db.clone(), Arc::new(SettingsStore::pinned(default_settings())))
}

fn payload(entries: &[(&str, Value)]) -> Dict {
    let mut dict = Dict::new();
    for (key, value) in entries {
        dict.insert(*key, value.clone());
    }
    dict
}

fn sample_record(payload: &str) -> NotificationRecord {
    let now = Utc::now();
    NotificationRecord {
        notification_id: 1,
        kind: "download_completed".to_owned(),
        source: "bilibili".to_owned(),
        header: String::new(),
        title: "Title".to_owned(),
        body: "Body".to_owned(),
        link_url: String::new(),
        image_url: String::new(),
        payload: payload.to_owned(),
        dedupe_key: String::new(),
        status: STATUS_UNREAD.to_owned(),
        markdown: "*Title*\nBody".to_owned(),
        disable_web_page_preview: true,
        disable_notification: true,
        pin: false,
        webhook_action: WEBHOOK_ACTION_SEND.to_owned(),
        occurrence_count: 1,
        event_version: 1,
        delivery_status: DELIVERY_PENDING.to_owned(),
        attempt_count: 0,
        next_attempt_at: now,
        created_at: now,
        read_at: None,
        delivered_at: None,
        last_error: String::new(),
        scope: SCOPE_ITEM.to_owned(),
        telegram_message_id: None,
        telegram_pinned_message_id: None,
    }
}

async fn fetch(db: &Db, id: i64) -> NotificationRecord {
    let row = sqlx::query(AssertSqlSafe(format!("SELECT {SELECT_FIELDS} FROM notifications WHERE id = $1;")))
        .bind(id)
        .fetch_one(db.pool())
        .await
        .expect("row");
    NotificationRecord::from_row(&row).expect("decodes")
}

async fn count(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM notifications;")
        .fetch_one(db.pool())
        .await
        .expect("count")
}

fn render(kind: &str, header: &str, title: &str, body: &str, link_url: &str, action: &str, occurrences: i32) -> DeliveryFields {
    delivery_fields(&RenderInput {
        kind,
        header,
        title,
        body,
        link_url,
        webhook_action: action,
        occurrence_count: occurrences,
    })
}

async fn columns(database: &TestDatabase) -> Vec<(String, String, Option<String>, String)> {
    sqlx::query(
        "SELECT column_name, data_type, column_default, is_nullable FROM information_schema.columns
         WHERE table_name = 'notifications' ORDER BY ordinal_position;",
    )
    .fetch_all(database.db().pool())
    .await
    .expect("columns")
    .iter()
    .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
    .collect()
}

fn has_column(columns: &[(String, String, Option<String>, String)], name: &str, kind: &str, default: Option<&str>, nullable: bool) -> bool {
    columns.iter().any(|(column, data_type, column_default, is_nullable)| {
        column == name && data_type == kind && column_default.as_deref() == default && (is_nullable == "YES") == nullable
    })
}

// py: tests/test_notifications.py::test_ensure_notifications_table_runs_schema_migration
#[tokio::test]
async fn ensure_notifications_table_runs_schema_migration() {
    // The table comes from `ensure_schema` (D6); the `status = 'read'` backfill is a dropped
    // legacy migration (D12), so only the resulting columns and index are checked.
    let database = database().await;
    let columns = columns(&database).await;
    assert!(has_column(&columns, "dedupe_key", "text", Some("''::text"), false), "{columns:?}");
    assert!(has_column(&columns, "header", "text", Some("''::text"), false));
    assert!(has_column(&columns, "pin", "boolean", Some("false"), false));
    assert!(has_column(&columns, "event_version", "integer", Some("1"), false));
    assert!(has_column(&columns, "delivery_status", "text", Some("'pending'::text"), false));
    let index: Option<String> = sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE indexname = 'notifications_dedupe_key_unique';")
        .fetch_optional(database.db().pool())
        .await
        .expect("index");
    let index = index.expect("unique dedupe index");
    assert!(index.starts_with("CREATE UNIQUE INDEX"), "{index}");
    assert!(index.contains("WHERE (dedupe_key <> ''::text)"), "{index}");
    database.drop_database().await;
}

// py: tests/test_notifications.py::test_schema_adds_scope_and_telegram_message_columns
#[tokio::test]
async fn schema_adds_scope_and_telegram_message_columns() {
    let database = database().await;
    let columns = columns(&database).await;
    assert!(has_column(&columns, "scope", "text", Some("'item'::text"), false), "{columns:?}");
    assert!(has_column(&columns, "telegram_message_id", "bigint", None, true));
    assert!(has_column(&columns, "telegram_pinned_message_id", "bigint", None, true));
    database.drop_database().await;
}

// py: tests/test_notifications.py::test_enqueue_notification_serializes_payload_and_renders_markdown
#[tokio::test]
async fn enqueue_notification_serializes_payload_and_renders_markdown() {
    let database = database().await;
    let db = database.db();
    let created = outbox(db)
        .enqueue(&NewNotification {
            kind: "job_failed".to_owned(),
            source: "worker".to_owned(),
            title: "Episode [1]!".to_owned(),
            body: "Path_(draft)".to_owned(),
            link_url: "https://example.com/watch?v=1".to_owned(),
            payload: Some(payload(&[("message_id", Value::Int(456))])),
            ..NewNotification::default()
        })
        .await
        .expect("enqueued")
        .expect("not muted");
    let markdown = "[*Episode \\[1\\]\\!*](https://example.com/watch?v=1)\nPath\\_\\(draft\\)";
    assert_eq!(created.payload_json(), payload(&[("message_id", Value::Int(456))]));
    assert_eq!(created.markdown, markdown);
    assert!(!created.disable_web_page_preview);
    assert!(!created.disable_notification);
    assert!(created.pin);

    let stored = fetch(db, created.notification_id).await;
    assert_eq!(stored.payload, "{\"message_id\":456}");
    assert_eq!(stored.markdown, markdown);
    assert_eq!(stored.header, "");
    assert_eq!(stored.dedupe_key, "");
    assert_eq!(stored.status, STATUS_UNREAD);
    assert_eq!(stored.webhook_action, WEBHOOK_ACTION_SEND);
    assert_eq!((stored.occurrence_count, stored.event_version, stored.attempt_count), (1, 1, 0));
    assert_eq!(stored.delivery_status, DELIVERY_PENDING);
    assert_eq!(stored.last_error, "");
    assert_eq!(stored.scope, SCOPE_ITEM);
    // Without a dedupe key every enqueue is its own row.
    outbox(db)
        .enqueue(&NewNotification {
            kind: "job_failed".to_owned(),
            source: "worker".to_owned(),
            ..NewNotification::default()
        })
        .await
        .expect("enqueued");
    assert_eq!(count(db).await, 2);
    database.drop_database().await;
}

// py: tests/test_notifications.py::test_enqueue_notification_drops_a_muted_source
#[tokio::test]
async fn enqueue_notification_drops_a_muted_source() {
    let database = database().await;
    let mut settings = default_settings();
    settings.web.bilibili.schedule.notify = false;
    let outbox = Outbox::new(database.db().clone(), Arc::new(SettingsStore::pinned(settings)));
    let created = outbox
        .enqueue(&NewNotification {
            kind: "download_completed".to_owned(),
            source: "bilibili".to_owned(),
            header: "Bilibili".to_owned(),
            title: "Episode 1".to_owned(),
            ..NewNotification::default()
        })
        .await
        .expect("no error");
    assert_eq!(created, None);
    assert_eq!(count(database.db()).await, 0);
    database.drop_database().await;
}

// py: tests/test_notifications.py::test_enqueue_notification_with_dedupe_key_uses_upsert_action
#[tokio::test]
async fn enqueue_notification_with_dedupe_key_uses_upsert_action() {
    let database = database().await;
    let db = database.db();
    let key = "job_failed:bilibili:bilibili:download:BV1TEST";
    let notification = NewNotification {
        kind: "job_failed".to_owned(),
        source: "worker".to_owned(),
        title: "Job failed: Bilibili".to_owned(),
        body: "DownloadError: temporary network error".to_owned(),
        payload: Some(payload(&[("job", Value::from("bilibili"))])),
        dedupe_key: format!("  {key} "),
        ..NewNotification::default()
    };
    let first = outbox(db).enqueue(&notification).await.expect("enqueued").expect("row");
    assert_eq!(first.webhook_action, WEBHOOK_ACTION_UPSERT);
    assert_eq!(first.dedupe_key, key);
    assert_eq!((first.occurrence_count, first.event_version), (1, 1));
    let created = outbox(db).enqueue(&notification).await.expect("enqueued").expect("row");
    assert_eq!(created.notification_id, first.notification_id);
    assert_eq!(created.webhook_action, WEBHOOK_ACTION_UPSERT);
    assert_eq!(created.occurrence_count, 2);
    assert_eq!(created.event_version, 2);
    assert!(created.pin);
    // The repeat is rendered silent, with its occurrence count.
    assert!(created.disable_notification);
    assert!(created.markdown.ends_with("Occurrences: 2"), "{}", created.markdown);
    assert_eq!(count(db).await, 1);
    database.drop_database().await;
}

#[tokio::test]
async fn a_key_is_open_from_its_enqueue_to_its_resolve() {
    let database = database().await;
    let db = database.db();
    let outbox = outbox(db);
    let notification = NewNotification {
        kind: "download_failed".to_owned(),
        source: "telegram".to_owned(),
        title: "A link".to_owned(),
        dedupe_key: "telegram:links:outage".to_owned(),
        ..NewNotification::default()
    };
    let key = notification.dedupe_key.as_str();
    assert!(!outbox.is_open(key).await.expect("asked"));
    outbox.enqueue(&notification).await.expect("enqueued").expect("row");
    assert!(outbox.is_open(key).await.expect("asked"));
    assert!(!outbox.is_open("telegram:links:other").await.expect("asked"));
    assert!(!outbox.is_open("  ").await.expect("asked"));

    outbox
        .resolve(key, &Resolution::job_recovered("Telegram", "ok", None))
        .await
        .expect("resolve")
        .expect("row");
    assert!(!outbox.is_open(key).await.expect("asked"));
    outbox.enqueue(&notification).await.expect("enqueued").expect("row");
    assert!(outbox.is_open(key).await.expect("asked"));
    database.drop_database().await;
}

#[tokio::test]
async fn an_upsert_keeps_a_pending_backoff_and_restarts_after_a_resolve() {
    let database = database().await;
    let db = database.db();
    let outbox = outbox(db);
    let notification = NewNotification {
        kind: "job_failed".to_owned(),
        source: "worker".to_owned(),
        title: "Job failed".to_owned(),
        dedupe_key: "job_failed:x:run".to_owned(),
        scope: Some(SCOPE_JOB.to_owned()),
        ..NewNotification::default()
    };
    let first = outbox.enqueue(&notification).await.expect("enqueued").expect("row");
    let claimed = outbox.claim_next_pending().await.expect("claim").expect("due");
    outbox
        .mark_retry(claimed.notification_id, claimed.event_version, 1, "HTTP 503", Some(120))
        .await
        .expect("retry");
    let repeat = outbox.enqueue(&notification).await.expect("enqueued").expect("row");
    // Still backing off: the attempt count, due time and error stay.
    assert_eq!(repeat.attempt_count, 1);
    assert_eq!(repeat.last_error, "HTTP 503");
    assert!(repeat.next_attempt_at > Utc::now() + Duration::seconds(60));
    assert_eq!(repeat.scope, SCOPE_JOB);

    let resolved = outbox
        .resolve(&notification.dedupe_key, &Resolution::job_recovered("X", "ok", None))
        .await
        .expect("resolve")
        .expect("row");
    assert_eq!(resolved.webhook_action, WEBHOOK_ACTION_RESOLVE);
    assert_eq!(resolved.attempt_count, 0);
    let again = outbox.enqueue(&notification).await.expect("enqueued").expect("row");
    assert_eq!(again.notification_id, first.notification_id);
    assert_eq!(again.occurrence_count, 1, "a failure after a resolve starts counting again");
    assert!(!again.disable_notification);
    database.drop_database().await;
}

// py: tests/test_notifications.py::test_notification_record_payload_json_handles_invalid_json
#[test]
fn notification_record_payload_json_handles_invalid_json() {
    let record = sample_record("not-json");
    assert_eq!(record.payload_json(), Dict::new());
    assert_eq!(record.local_image_path(), None);
    assert_eq!(sample_record("[1]").payload_json(), Dict::new());
    assert_eq!(sample_record("{\"image_path\": \"  \"}").local_image_path(), None);
    assert_eq!(sample_record("{\"image_path\": 3}").local_image_path(), None);
}

// py: tests/test_notifications.py::test_notification_record_resolves_local_image_path_from_payload
#[test]
fn notification_record_resolves_local_image_path_from_payload() {
    let record = sample_record("{\"image_path\":\" /images/demo.png \"}");
    assert_eq!(record.local_image_path(), Some(PathBuf::from("/images/demo.png")));
}

// py: tests/test_notifications.py::test_job_run_failure_dedupe_key_is_one_row_per_job
#[test]
fn job_run_failure_dedupe_key_is_one_row_per_job() {
    assert_eq!(format_job_run_failure_dedupe_key("Bilibili"), "job_failed:bilibili:run");
    assert_eq!(format_job_run_failure_dedupe_key(""), "");
}

// py: tests/test_notifications.py::test_first_failure_rings_and_repeats_stay_silent
#[test]
fn first_failure_rings_and_repeats_stay_silent() {
    let first = render(
        "job_failed",
        "Bilibili",
        "Job failed",
        "KeyError: sessdata",
        "",
        WEBHOOK_ACTION_UPSERT,
        1,
    );
    let repeat = render(
        "job_failed",
        "Bilibili",
        "Job failed",
        "KeyError: sessdata",
        "",
        WEBHOOK_ACTION_UPSERT,
        4,
    );
    assert!(!first.disable_notification);
    assert!(first.pin);
    assert!(!first.markdown.contains("Occurrences"));
    // Still delivered and still pinned, just without the alert sound.
    assert!(repeat.disable_notification);
    assert!(repeat.pin);
    assert!(repeat.markdown.ends_with("Occurrences: 4"));
}

// py: tests/test_notifications.py::test_an_unsupported_message_stays_pinned_until_it_is_resolved
#[test]
fn an_unsupported_message_stays_pinned_until_it_is_resolved() {
    let fields = |kind: &str, action: &str| {
        render(
            kind,
            "Telegram",
            "fav #130",
            "Link not supported: https://example.com/x",
            "https://t.me/c/1/130",
            action,
            1,
        )
    };
    let open = fields(KIND_UNSUPPORTED_MESSAGE, WEBHOOK_ACTION_UPSERT);
    assert_eq!((open.disable_notification, open.pin), (false, true));
    assert!(!fields(KIND_UNSUPPORTED_RESOLVED, WEBHOOK_ACTION_RESOLVE).pin);
}

// py: tests/test_notifications.py::test_image_url_is_not_rendered_as_redundant_caption_link
#[test]
fn image_url_is_not_rendered_as_redundant_caption_link() {
    // The image URL is not an input of the template at all.
    let fields = render("download_completed", "", "Image", "disc/Foo.png", "", WEBHOOK_ACTION_SEND, 1);
    assert_eq!(
        fields,
        DeliveryFields {
            markdown: "*Image*\ndisc/Foo\\.png".to_owned(),
            disable_web_page_preview: true,
            disable_notification: true,
            pin: false,
        }
    );
}

// py: tests/test_notifications.py::test_link_url_is_rendered_as_the_title_hyperlink
#[test]
fn link_url_is_rendered_as_the_title_hyperlink() {
    let fields = render(
        "download_completed",
        "",
        "Video [01]",
        "Uploader | 1080P | 12.3 MiB | 2026-03-01",
        "https://www.bilibili.com/video/BV1TEST",
        WEBHOOK_ACTION_SEND,
        1,
    );
    assert_eq!(
        fields.markdown,
        "[*Video \\[01\\]*](https://www.bilibili.com/video/BV1TEST)\nUploader \\| 1080P \\| 12\\.3 MiB \\| 2026\\-03\\-01"
    );
    assert!(!fields.disable_web_page_preview);
    assert!(fields.disable_notification);
    assert!(!fields.pin);
}

// py: tests/test_notifications.py::test_header_is_rendered_above_the_linked_title
#[test]
fn header_is_rendered_above_the_linked_title() {
    let fields = render(
        "download_completed",
        "Bilibili (toview)",
        "- Video [01]",
        "Uploader | 1080P | 12.3 MiB | 2026-03-01",
        "https://www.bilibili.com/video/BV1TEST",
        WEBHOOK_ACTION_SEND,
        1,
    );
    assert_eq!(
        fields.markdown,
        "FAV · Bilibili \\(toview\\)\n[*\\- Video \\[01\\]*](https://www.bilibili.com/video/BV1TEST)\nUploader \\| 1080P \\| 12\\.3 MiB \\| 2026\\-03\\-01"
    );
    assert!(!fields.disable_web_page_preview);
}

// py: tests/test_notifications.py::test_link_url_escapes_only_the_characters_telegram_forbids_in_a_url
#[test]
fn link_url_escapes_only_the_characters_telegram_forbids_in_a_url() {
    let fields = render(
        "download_completed",
        "",
        "Title",
        "",
        "https://example.com/a_(b)?q=1.2",
        WEBHOOK_ACTION_SEND,
        1,
    );
    assert_eq!(fields.markdown, "[*Title*](https://example.com/a_(b\\)?q=1.2)");
}

// py: tests/test_notifications.py::test_titleless_notification_keeps_the_link_on_its_own_line
#[test]
fn titleless_notification_keeps_the_link_on_its_own_line() {
    let fields = render(
        "download_completed",
        "",
        "",
        "Body",
        "https://example.com/video",
        WEBHOOK_ACTION_SEND,
        1,
    );
    assert_eq!(fields.markdown, "Body\nhttps://example\\.com/video");
}

/// Insert a row the way it looks after some history.
async fn insert_row(db: &Db, dedupe_key: &str, scope: &str, action: &str, delivery_status: &str, event_version: i32) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO notifications (kind, source, header, title, body, dedupe_key, scope, webhook_action, delivery_status,
                                    event_version, occurrence_count, pin, telegram_pinned_message_id, attempt_count, last_error)
         VALUES ('job_failed', 'worker', 'X', 'Job failed', 'boom', $1, $2, $3, $4, $5, 3, TRUE, 4903, 2, 'HTTP 503')
         RETURNING id;",
    )
    .bind(dedupe_key)
    .bind(scope)
    .bind(action)
    .bind(delivery_status)
    .bind(event_version)
    .fetch_one(db.pool())
    .await
    .expect("insert")
}

// py: tests/test_notifications.py::test_resolve_notification_updates_existing_dedupe_key
#[tokio::test]
async fn resolve_notification_updates_existing_dedupe_key() {
    let database = database().await;
    let db = database.db();
    let key = "job_failed:bilibili:bilibili:download:BV1TEST";
    let id = insert_row(db, key, SCOPE_ITEM, WEBHOOK_ACTION_UPSERT, DELIVERY_DELIVERED, 8).await;
    let resolution = Resolution {
        kind: "job_recovered".to_owned(),
        source: "worker".to_owned(),
        title: "Job recovered: Bilibili".to_owned(),
        body: "Download succeeded: Title [BV1TEST]".to_owned(),
        link_url: "https://www.bilibili.com/video/BV1TEST".to_owned(),
        payload: Some(payload(&[("job", Value::from("bilibili")), ("bvid", Value::from("BV1TEST"))])),
        ..Resolution::default()
    };
    let outbox = outbox(db);
    let resolved = outbox.resolve(key, &resolution).await.expect("resolve").expect("row");
    assert_eq!(resolved.notification_id, id);
    assert_eq!(resolved.webhook_action, WEBHOOK_ACTION_RESOLVE);
    assert_eq!(resolved.event_version, 9);
    assert!(!resolved.pin);
    assert!(resolved.disable_notification);
    assert_eq!(resolved.delivery_status, DELIVERY_PENDING);
    assert_eq!(resolved.attempt_count, 0);
    assert_eq!(resolved.last_error, "");
    assert_eq!(resolved.payload, "{\"job\":\"bilibili\",\"bvid\":\"BV1TEST\"}");
    assert_eq!(
        resolved.markdown,
        "[*Job recovered: Bilibili*](https://www.bilibili.com/video/BV1TEST)\nDownload succeeded: Title \\[BV1TEST\\]"
    );
    // The pin survives until the resolve message is delivered.
    assert_eq!(resolved.telegram_pinned_message_id, Some(4903));

    // Delivered once, a resolve is final; a failed resolve can be retried.
    outbox.claim_next_pending().await.expect("claim").expect("due");
    let stored = fetch(db, id).await;
    outbox
        .mark_delivered(id, stored.event_version, Some(1), None)
        .await
        .expect("delivered");
    assert_eq!(outbox.resolve(key, &resolution).await.expect("resolve"), None);
    sqlx::query("UPDATE notifications SET delivery_status = 'failed' WHERE id = $1;")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("fail it");
    assert!(outbox.resolve(key, &resolution).await.expect("resolve").is_some());
    assert_eq!(outbox.resolve("  ", &resolution).await.expect("blank key"), None);
    assert_eq!(outbox.resolve("unknown", &resolution).await.expect("unknown key"), None);
    database.drop_database().await;
}

// py: tests/test_notifications.py::test_reset_stale_sending_notifications_restores_expired_claims
#[tokio::test]
async fn reset_stale_sending_notifications_restores_expired_claims() {
    let database = database().await;
    let db = database.db();
    let expired = insert_row(db, "a", SCOPE_ITEM, WEBHOOK_ACTION_UPSERT, DELIVERY_SENDING, 1).await;
    let with_error = insert_row(db, "b", SCOPE_ITEM, WEBHOOK_ACTION_UPSERT, DELIVERY_SENDING, 1).await;
    let leased = insert_row(db, "c", SCOPE_ITEM, WEBHOOK_ACTION_UPSERT, DELIVERY_SENDING, 1).await;
    sqlx::query("UPDATE notifications SET last_error = '', next_attempt_at = CURRENT_TIMESTAMP - INTERVAL '1 second' WHERE id = $1;")
        .bind(expired)
        .execute(db.pool())
        .await
        .expect("expire");
    sqlx::query("UPDATE notifications SET next_attempt_at = CURRENT_TIMESTAMP - INTERVAL '1 second' WHERE id = $1;")
        .bind(with_error)
        .execute(db.pool())
        .await
        .expect("expire");
    sqlx::query("UPDATE notifications SET next_attempt_at = CURRENT_TIMESTAMP + INTERVAL '5 minutes' WHERE id = $1;")
        .bind(leased)
        .execute(db.pool())
        .await
        .expect("lease");

    outbox(db).reset_stale_sending().await.expect("reset");
    let expired = fetch(db, expired).await;
    assert_eq!(expired.delivery_status, DELIVERY_PENDING);
    assert_eq!(expired.last_error, "Notification delivery lease expired before completion");
    let with_error = fetch(db, with_error).await;
    assert_eq!(with_error.delivery_status, DELIVERY_PENDING);
    assert_eq!(with_error.last_error, "HTTP 503");
    assert_eq!(fetch(db, leased).await.delivery_status, DELIVERY_SENDING);
    database.drop_database().await;
}

// py: tests/test_notifications.py::test_claim_next_pending_notification_uses_skip_locked_and_backfills_markdown
#[tokio::test]
async fn claim_next_pending_notification_uses_skip_locked_and_backfills_markdown() {
    let database = database().await;
    let db = database.db();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO notifications (kind, source, header, title, body, link_url, dedupe_key, markdown, webhook_action,
                                    occurrence_count, event_version, delivery_status)
         VALUES ('job_failed', 'worker', 'Bilibili', 'Video [01]', 'Uploader_(name)', 'https://example.com/video',
                 'job_failed:bilibili:run', 'stale', 'upsert', 3, 4, 'pending')
         RETURNING id;",
    )
    .fetch_one(db.pool())
    .await
    .expect("insert");
    // A row another worker holds locked is skipped, not waited for.
    let mut other = db.pool().begin().await.expect("transaction");
    let locked: i64 =
        sqlx::query_scalar("INSERT INTO notifications (kind, source, delivery_status) VALUES ('x', 'worker', 'pending') RETURNING id;")
            .fetch_one(&mut *other)
            .await
            .expect("insert locked");
    other.commit().await.expect("commit");
    let mut holder = db.pool().begin().await.expect("transaction");
    sqlx::query("SELECT id FROM notifications WHERE id = $1 FOR UPDATE;")
        .bind(locked)
        .execute(&mut *holder)
        .await
        .expect("lock");
    // A row due later is not claimed either.
    sqlx::query("INSERT INTO notifications (kind, source, delivery_status, next_attempt_at) VALUES ('x', 'worker', 'pending', CURRENT_TIMESTAMP + INTERVAL '1 hour');")
        .execute(db.pool())
        .await
        .expect("insert later");

    let outbox = outbox(db);
    let claimed = outbox.claim_next_pending().await.expect("claim").expect("due");
    let markdown = "FAV · Bilibili\n[*Video \\[01\\]*](https://example.com/video)\nUploader\\_\\(name\\)\nOccurrences: 3";
    assert_eq!(claimed.notification_id, id);
    assert_eq!(claimed.event_version, 5);
    assert_eq!(claimed.delivery_status, DELIVERY_SENDING);
    assert_eq!(claimed.markdown, markdown);
    // Third occurrence: still pinned, but re-rendered as silent at claim time.
    assert!(claimed.disable_notification);
    assert!(claimed.pin);
    assert!(!claimed.disable_web_page_preview);
    let stored = fetch(db, id).await;
    assert_eq!(stored.markdown, markdown);
    let lease = stored.next_attempt_at - Utc::now();
    assert!(
        lease > Duration::seconds(SENDING_LEASE_SECONDS - 30) && lease <= Duration::seconds(SENDING_LEASE_SECONDS),
        "{lease}"
    );

    assert_eq!(outbox.claim_next_pending().await.expect("claim"), None);
    holder.rollback().await.expect("release");
    assert_eq!(
        outbox.claim_next_pending().await.expect("claim").expect("now free").notification_id,
        locked
    );
    database.drop_database().await;
}

// py: tests/test_notifications.py::test_mark_notification_delivered_updates_status
#[tokio::test]
async fn mark_notification_delivered_updates_status() {
    let database = database().await;
    let db = database.db();
    let outbox = outbox(db);
    let id = insert_row(db, "a", SCOPE_ITEM, WEBHOOK_ACTION_UPSERT, DELIVERY_SENDING, 6).await;

    // A stale event version changes nothing.
    outbox.mark_delivered(id, 5, Some(1), Some(1)).await.expect("no-op");
    assert_eq!(fetch(db, id).await.delivery_status, DELIVERY_SENDING);

    outbox.mark_delivered(id, 6, None, None).await.expect("delivered");
    let stored = fetch(db, id).await;
    assert_eq!(stored.delivery_status, DELIVERY_DELIVERED);
    assert_eq!(stored.status, STATUS_READ);
    assert!(stored.read_at.is_some());
    assert!(stored.delivered_at.is_some());
    assert_eq!(stored.last_error, "");
    assert_eq!(stored.telegram_message_id, None);
    // Written as given: nothing pinned any more.
    assert_eq!(stored.telegram_pinned_message_id, None);

    sqlx::query("UPDATE notifications SET delivery_status = 'sending' WHERE id = $1;")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("reset");
    outbox.mark_delivered(id, 6, Some(501), Some(501)).await.expect("delivered");
    let stored = fetch(db, id).await;
    assert_eq!(stored.telegram_message_id, Some(501));
    assert_eq!(stored.telegram_pinned_message_id, Some(501));
    sqlx::query("UPDATE notifications SET delivery_status = 'sending' WHERE id = $1;")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("reset");
    // No message id keeps the last one.
    outbox.mark_delivered(id, 6, None, Some(501)).await.expect("delivered");
    assert_eq!(fetch(db, id).await.telegram_message_id, Some(501));
    database.drop_database().await;
}

// py: tests/test_notifications.py::test_mark_notification_retry_updates_attempt_count
#[tokio::test]
async fn mark_notification_retry_updates_attempt_count() {
    let database = database().await;
    let db = database.db();
    let outbox = outbox(db);
    let id = insert_row(db, "a", SCOPE_ITEM, WEBHOOK_ACTION_UPSERT, DELIVERY_SENDING, 7).await;
    outbox.mark_retry(id, 7, 2, " HTTP 503 ", None).await.expect("retry");
    let stored = fetch(db, id).await;
    assert_eq!(stored.delivery_status, DELIVERY_PENDING);
    assert_eq!(stored.attempt_count, 2);
    assert_eq!(stored.last_error, "HTTP 503");
    let delay = stored.next_attempt_at - Utc::now();
    assert!(delay > Duration::seconds(50) && delay <= Duration::seconds(60), "{delay}");

    // The server's retry_after wins when it is longer.
    sqlx::query("UPDATE notifications SET delivery_status = 'sending' WHERE id = $1;")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("reset");
    outbox.mark_retry(id, 7, 1, "slow down", Some(300)).await.expect("retry");
    let delay = fetch(db, id).await.next_attempt_at - Utc::now();
    assert!(delay > Duration::seconds(290), "{delay}");
    database.drop_database().await;
}

// py: tests/test_notifications.py::test_mark_notification_failed_updates_terminal_state
#[tokio::test]
async fn mark_notification_failed_updates_terminal_state() {
    let database = database().await;
    let db = database.db();
    let id = insert_row(db, "a", SCOPE_ITEM, WEBHOOK_ACTION_UPSERT, DELIVERY_SENDING, 8).await;
    outbox(db).mark_failed(id, 8, 4, "HTTP 400").await.expect("failed");
    let stored = fetch(db, id).await;
    assert_eq!(stored.delivery_status, DELIVERY_FAILED);
    assert_eq!(stored.attempt_count, 4);
    assert_eq!(stored.last_error, "HTTP 400");
    // Nothing claims a failed row again.
    assert_eq!(outbox(db).claim_next_pending().await.expect("claim"), None);
    database.drop_database().await;
}

// py: tests/test_notifications.py::test_retry_delay_seconds_uses_exponential_backoff_with_cap
#[test]
fn retry_delay_seconds_uses_exponential_backoff_with_cap() {
    assert_eq!(retry_delay_seconds(1), 30);
    assert_eq!(retry_delay_seconds(2), 60);
    assert_eq!(retry_delay_seconds(3), 120);
    assert_eq!(retry_delay_seconds(6), 600);
    assert_eq!(retry_delay_seconds(10), 600);
    assert_eq!(retry_delay_seconds(i32::MAX), 600);
}

// py: tests/test_notifications.py::test_resolve_job_failure_notifications_targets_job_scoped_rows
#[tokio::test]
async fn resolve_job_failure_notifications_targets_job_scoped_rows() {
    let database = database().await;
    let db = database.db();
    let run = insert_row(
        db,
        "job_failed:twitter:run",
        SCOPE_JOB,
        WEBHOOK_ACTION_UPSERT,
        DELIVERY_DELIVERED,
        1,
    )
    .await;
    let auth = insert_row(
        db,
        "job_failed:twitter:twitter:auth",
        SCOPE_JOB,
        WEBHOOK_ACTION_UPSERT,
        DELIVERY_PENDING,
        1,
    )
    .await;
    let item = insert_row(
        db,
        "job_failed:twitter:twitter:video:1",
        SCOPE_ITEM,
        WEBHOOK_ACTION_UPSERT,
        DELIVERY_DELIVERED,
        1,
    )
    .await;
    // `_` in the job key is a literal, not a LIKE wildcard.
    let other = insert_row(
        db,
        "job_failedXtwitterXrun",
        SCOPE_JOB,
        WEBHOOK_ACTION_UPSERT,
        DELIVERY_DELIVERED,
        1,
    )
    .await;
    let similar = insert_row(
        db,
        "job_failed:twitter2:run",
        SCOPE_JOB,
        WEBHOOK_ACTION_UPSERT,
        DELIVERY_DELIVERED,
        1,
    )
    .await;
    let done = insert_row(
        db,
        "job_failed:twitter:old",
        SCOPE_JOB,
        WEBHOOK_ACTION_RESOLVE,
        DELIVERY_DELIVERED,
        1,
    )
    .await;

    let resolved = outbox(db)
        .resolve_job_failures(
            "Twitter",
            &Resolution::job_recovered(
                "X",
                "X completed successfully again.",
                Some(payload(&[("job", Value::from("twitter"))])),
            ),
        )
        .await
        .expect("resolved");
    let mut ids: Vec<i64> = resolved.iter().map(|record| record.notification_id).collect();
    ids.sort_unstable();
    assert_eq!(ids, [run, auth]);
    for record in &resolved {
        assert_eq!(record.kind, "job_recovered");
        assert_eq!(record.source, "worker");
        assert_eq!(record.title, "Job recovered");
        assert_eq!(record.link_url, "");
        // The rendered resolve message is silent, unpinned, and carries no occurrence count.
        assert!(!record.pin);
        assert!(record.disable_notification);
        assert!(!record.markdown.contains("Occurrences"));
        assert_eq!(record.markdown, "FAV · X\n*Job recovered*\nX completed successfully again\\.");
        // The pinned message id survives the resolve so its delivery can unpin it.
        assert_eq!(record.telegram_pinned_message_id, Some(4903));
        assert_eq!(record.scope, SCOPE_JOB);
    }
    for untouched in [item, other, similar] {
        assert_eq!(fetch(db, untouched).await.webhook_action, WEBHOOK_ACTION_UPSERT);
    }
    assert_eq!(fetch(db, done).await.event_version, 1);
    database.drop_database().await;
}

// py: tests/test_notifications.py::test_resolve_job_failure_notifications_ignores_an_empty_job_key
#[tokio::test]
async fn resolve_job_failure_notifications_ignores_an_empty_job_key() {
    // A pool that cannot connect: no query may be attempted.
    let db = Db::connect_lazy("postgresql://nobody@127.0.0.1:1/none").expect("lazy pool");
    let resolved = outbox(&db)
        .resolve_job_failures("  ", &Resolution::job_recovered("", "", None))
        .await
        .expect("no query");
    assert!(resolved.is_empty());
}

#[test]
fn rendering_matches_python() {
    let golden = golden_json("notifications/render.json");
    let cases = golden["cases"].as_array().expect("cases");
    for case in cases {
        let text = |key: &str| case[key].as_str().expect("text").to_owned();
        let fields = render(
            &text("kind"),
            &text("header"),
            &text("title"),
            &text("body"),
            &text("link_url"),
            &text("webhook_action"),
            i32::try_from(case["occurrence_count"].as_i64().expect("count")).expect("i32"),
        );
        assert_eq!(fields.markdown, case["markdown"], "{case}");
        assert_eq!(fields.disable_web_page_preview, case["disable_web_page_preview"], "{case}");
        assert_eq!(fields.disable_notification, case["disable_notification"], "{case}");
        assert_eq!(fields.pin, case["pin"], "{case}");
    }
    assert_eq!(cases.len(), 5 * 3 * 3 * 12);
}

#[test]
fn keys_and_escapes_match_python() {
    let golden = golden_json("notifications/text.json");
    let mut checked = 0;
    for case in golden["cases"].as_array().expect("cases") {
        let text = |key: &str| case[key].as_str().expect("text").to_owned();
        match case["function"].as_str().expect("function") {
            "escape" => {
                assert_eq!(escape_markdown_v2_url(&text("input")), case["escape_markdown_v2_url"], "{case}");
                assert_eq!(like_literal(&text("input")), case["like_literal"], "{case}");
            }
            "format_job_failure_dedupe_key" => {
                assert_eq!(
                    format_job_failure_dedupe_key(&text("job_key"), &text("failure_key")),
                    case["result"],
                    "{case}"
                );
            }
            "format_job_run_failure_dedupe_key" => {
                assert_eq!(format_job_run_failure_dedupe_key(&text("job_key")), case["result"], "{case}");
            }
            "retry_delay_seconds" => {
                let attempt = i32::try_from(case["attempt"].as_i64().expect("attempt")).expect("i32");
                assert_eq!(retry_delay_seconds(attempt), case["result"].as_i64().expect("result"), "{case}");
            }
            _ => continue,
        }
        checked += 1;
    }
    assert!(checked > 50, "{checked}");
}
