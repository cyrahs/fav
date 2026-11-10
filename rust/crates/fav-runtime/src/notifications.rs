//! The notification outbox: rows queued by sources and the runner, delivered by the worker.
//!
//! Python origin: `src/tool/notifications.py`. Every message follows one template (a
//! `FAV · <source>` header line, the title carrying the link, then the body), rendered in
//! Telegram MarkdownV2 from the stored columns and re-rendered when the worker claims the
//! row. A dedupe key turns repeats of one failure into one row whose occurrence count
//! grows; only the first occurrence rings, the repeats arrive silently and take over the
//! pin. A resolve re-opens the row as a quiet "recovered" message whose delivery unpins
//! the failure. The table comes from [`fav_db::Db::ensure_schema`] (DIVERGENCES.md D6, D12).

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use fav_core::pyjson::{self, Dict, DumpOptions, Value};
use fav_core::pytext::py_strip;
use fav_db::{Db, SettingsStore};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, Postgres, Row as _};

use crate::Error;
use crate::telegram_bot::{escape_markdown_v2, format_header_line};

pub const STATUS_READ: &str = "read";
pub const STATUS_UNREAD: &str = "unread";

pub const DELIVERY_PENDING: &str = "pending";
pub const DELIVERY_SENDING: &str = "sending";
pub const DELIVERY_DELIVERED: &str = "delivered";
pub const DELIVERY_FAILED: &str = "failed";

pub const WEBHOOK_ACTION_SEND: &str = "send";
pub const WEBHOOK_ACTION_UPSERT: &str = "upsert";
pub const WEBHOOK_ACTION_RESOLVE: &str = "resolve";

/// `job`: the whole job raised, queued by the runner; cleared when the job next succeeds.
/// `item`: one unit of work inside a job (a video, a parser), which the owning module
/// clears itself once that unit goes through.
pub const SCOPE_ITEM: &str = "item";
pub const SCOPE_JOB: &str = "job";

/// How long a claimed row may stay `sending` before another claim takes it back.
pub const SENDING_LEASE_SECONDS: i64 = 600;
pub const JOB_RUN_FAILURE_KEY: &str = "run";
/// A message a source received but could not save anything from: pinned until the source
/// resolves it, when the message is saved after all or deleted.
pub const KIND_UNSUPPORTED_MESSAGE: &str = "unsupported_message";
pub const KIND_UNSUPPORTED_RESOLVED: &str = "unsupported_resolved";
/// What stays pinned until it is resolved.
pub const PINNED_KINDS: [&str; 2] = ["job_failed", KIND_UNSUPPORTED_MESSAGE];

const LEASE_EXPIRED_ERROR: &str = "Notification delivery lease expired before completion";

const SELECT_FIELDS: &str = "id, kind, source, header, title, body, link_url, image_url, payload, dedupe_key, status, created_at, \
     read_at, markdown, disable_web_page_preview, disable_notification, pin, webhook_action, occurrence_count, event_version, \
     delivery_status, attempt_count, next_attempt_at, delivered_at, last_error, scope, telegram_message_id, telegram_pinned_message_id";

/// One row of `notifications`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationRecord {
    pub notification_id: i64,
    pub kind: String,
    pub source: String,
    pub header: String,
    pub title: String,
    pub body: String,
    pub link_url: String,
    pub image_url: String,
    pub payload: String,
    pub dedupe_key: String,
    pub status: String,
    pub markdown: String,
    pub disable_web_page_preview: bool,
    pub disable_notification: bool,
    pub pin: bool,
    pub webhook_action: String,
    pub occurrence_count: i32,
    pub event_version: i32,
    pub delivery_status: String,
    pub attempt_count: i32,
    pub next_attempt_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub read_at: Option<DateTime<Utc>>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub last_error: String,
    pub scope: String,
    /// The last Telegram message this row produced.
    pub telegram_message_id: Option<i64>,
    /// The Telegram message this row still keeps pinned, so a later delivery (a repeat or
    /// the resolve) can unpin it.
    pub telegram_pinned_message_id: Option<i64>,
}

impl NotificationRecord {
    /// The payload as a dictionary; anything that is not a JSON object reads as `{}`.
    pub fn payload_json(&self) -> Dict {
        match pyjson::loads(&self.payload) {
            Ok(Value::Dict(dict)) => dict,
            _ => Dict::new(),
        }
    }

    /// `payload["image_path"]`, stripped, when it is a non-empty string.
    pub fn local_image_path(&self) -> Option<PathBuf> {
        match self.payload_json().get("image_path") {
            Some(Value::Str(path)) => {
                let path = py_strip(path);
                (!path.is_empty()).then(|| PathBuf::from(path))
            }
            _ => None,
        }
    }

    fn from_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        let text = |name: &str| row.try_get::<String, _>(name);
        let or_default = |value: String, default: &str| if value.is_empty() { default.to_owned() } else { value };
        Ok(Self {
            notification_id: row.try_get("id")?,
            kind: text("kind")?,
            source: text("source")?,
            header: text("header")?,
            title: text("title")?,
            body: text("body")?,
            link_url: text("link_url")?,
            image_url: text("image_url")?,
            payload: or_default(text("payload")?, "{}"),
            dedupe_key: text("dedupe_key")?,
            status: or_default(text("status")?, STATUS_UNREAD),
            markdown: text("markdown")?,
            disable_web_page_preview: row.try_get("disable_web_page_preview")?,
            disable_notification: row.try_get("disable_notification")?,
            pin: row.try_get("pin")?,
            webhook_action: or_default(text("webhook_action")?, WEBHOOK_ACTION_SEND),
            // `int(row.get(...) or 1)`: a zero reads as one.
            occurrence_count: nonzero_or(row.try_get("occurrence_count")?, 1),
            event_version: nonzero_or(row.try_get("event_version")?, 1),
            delivery_status: or_default(text("delivery_status")?, DELIVERY_PENDING),
            attempt_count: row.try_get("attempt_count")?,
            next_attempt_at: row.try_get("next_attempt_at")?,
            created_at: row.try_get("created_at")?,
            read_at: row.try_get("read_at")?,
            delivered_at: row.try_get("delivered_at")?,
            last_error: text("last_error")?,
            scope: or_default(text("scope")?, SCOPE_ITEM),
            telegram_message_id: row.try_get("telegram_message_id")?,
            telegram_pinned_message_id: row.try_get("telegram_pinned_message_id")?,
        })
    }

    /// `_with_rendered_delivery_fields`: the record with its template fields re-rendered
    /// from its own columns.
    fn rendered(mut self) -> Self {
        let fields = delivery_fields(&RenderInput {
            kind: &self.kind,
            header: &self.header,
            title: &self.title,
            body: &self.body,
            link_url: &self.link_url,
            webhook_action: &self.webhook_action,
            occurrence_count: self.occurrence_count,
        });
        self.markdown = fields.markdown;
        self.disable_web_page_preview = fields.disable_web_page_preview;
        self.disable_notification = fields.disable_notification;
        self.pin = fields.pin;
        self
    }
}

fn nonzero_or(value: i32, default: i32) -> i32 {
    if value == 0 { default } else { value }
}

// ---------- rendering ----------

/// What the template is rendered from (`_notification_delivery_fields`'s arguments; the
/// image URL is accepted there and ignored, so it is not here).
#[derive(Debug, Clone, Copy)]
pub struct RenderInput<'a> {
    pub kind: &'a str,
    pub header: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub link_url: &'a str,
    pub webhook_action: &'a str,
    pub occurrence_count: i32,
}

/// The rendered message and its delivery flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryFields {
    pub markdown: String,
    pub disable_web_page_preview: bool,
    pub disable_notification: bool,
    pub pin: bool,
}

/// Telegram only wants `)` and `\` escaped inside the `(...)` part of an inline link.
pub fn escape_markdown_v2_url(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | ')') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// `_notification_delivery_fields`: the template every message follows.
pub fn delivery_fields(input: &RenderInput<'_>) -> DeliveryFields {
    let header = py_strip(input.header);
    let title = py_strip(input.title);
    let body = py_strip(input.body);
    let link_url = py_strip(input.link_url);
    let is_active_job_failure = input.kind == "job_failed" && input.webhook_action != WEBHOOK_ACTION_RESOLVE;
    let pins = PINNED_KINDS.contains(&input.kind) && input.webhook_action != WEBHOOK_ACTION_RESOLVE;

    let mut parts: Vec<String> = Vec::new();
    // The header names the app and the source, so the title line is free to be the item itself.
    if !header.is_empty() {
        parts.push(format_header_line(header));
    }
    if !title.is_empty() {
        let mut title_markdown = format!("*{}*", escape_markdown_v2(title));
        if !link_url.is_empty() {
            title_markdown = format!("[{title_markdown}]({})", escape_markdown_v2_url(link_url));
        }
        parts.push(title_markdown);
    }
    if !body.is_empty() {
        parts.push(escape_markdown_v2(body));
    }
    if is_active_job_failure && input.occurrence_count > 1 {
        parts.push(escape_markdown_v2(&format!("Occurrences: {}", input.occurrence_count)));
    }
    // A titleless notification has nothing to carry the link, so it still gets its own line.
    if !link_url.is_empty() && title.is_empty() {
        parts.push(escape_markdown_v2(link_url));
    }
    DeliveryFields {
        markdown: parts.join("\n"),
        disable_web_page_preview: link_url.is_empty(),
        // Only the first occurrence of a failure rings the phone: the repeats still arrive
        // and take over the pin, without a daily alert for something already known.
        disable_notification: !pins || input.occurrence_count > 1,
        pin: pins,
    }
}

/// `format_job_failure_dedupe_key`: `job_failed:<job>:<failure>`, or `""` when either is blank.
pub fn format_job_failure_dedupe_key(job_key: &str, failure_key: &str) -> String {
    let job_key = py_strip(job_key).to_lowercase();
    let failure_key = py_strip(failure_key);
    if job_key.is_empty() || failure_key.is_empty() {
        return String::new();
    }
    format!("job_failed:{job_key}:{failure_key}")
}

/// The key every run-level failure of one job shares: a repeat bumps the occurrence count
/// and re-pins instead of stacking a new row (and a new pin) per run.
pub fn format_job_run_failure_dedupe_key(job_key: &str) -> String {
    format_job_failure_dedupe_key(job_key, JOB_RUN_FAILURE_KEY)
}

/// `_like_literal`: escape `\`, `%` and `_` for a LIKE pattern.
pub fn like_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// `retry_delay_seconds`: 30 s doubling per attempt, capped at ten minutes.
pub fn retry_delay_seconds(next_attempt_count: i32) -> i64 {
    if next_attempt_count <= 0 {
        return 0;
    }
    let exponent = u32::try_from(next_attempt_count - 1).unwrap_or(u32::MAX).min(10);
    (30_i64 << exponent).min(600)
}

fn payload_text(payload: Option<&Dict>) -> Result<String, Error> {
    pyjson::dumps(&Value::Dict(payload.cloned().unwrap_or_default()), &DumpOptions::compact()).map_err(|error| Error::Invalid(error.to_string()))
}

// ---------- the outbox ----------

/// A notification to queue (`enqueue_notification`'s keyword arguments).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewNotification {
    pub kind: String,
    /// The job key of the source, so its `notify` toggle applies; `worker` for the runner.
    pub source: String,
    /// The source's `JOB_SPECS` name, shown as `FAV · <header>`.
    pub header: String,
    pub title: String,
    pub body: String,
    pub link_url: String,
    pub image_url: String,
    pub payload: Option<Dict>,
    pub dedupe_key: String,
    /// [`SCOPE_ITEM`] unless set.
    pub scope: Option<String>,
}

/// A resolve message (`resolve_notification`'s keyword arguments besides the key).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resolution {
    pub kind: String,
    pub source: String,
    pub header: String,
    pub title: String,
    pub body: String,
    pub link_url: String,
    pub image_url: String,
    pub payload: Option<Dict>,
}

impl Resolution {
    /// `resolve_job_failure_notifications`'s defaults: a `job_recovered` message from the
    /// runner, titled `Job recovered`.
    pub fn job_recovered(header: &str, body: &str, payload: Option<Dict>) -> Self {
        Self {
            kind: "job_recovered".to_owned(),
            source: "worker".to_owned(),
            header: header.to_owned(),
            title: "Job recovered".to_owned(),
            body: body.to_owned(),
            payload,
            ..Self::default()
        }
    }
}

/// The outbox: the database plus the settings that gate it.
#[derive(Debug, Clone)]
pub struct Outbox {
    db: Db,
    settings: Arc<SettingsStore>,
}

const RESOLVE_SET_SQL: &str = "
        SET kind = $1,
            source = $2,
            header = $3,
            title = $4,
            body = $5,
            link_url = $6,
            image_url = $7,
            payload = $8,
            status = $9,
            read_at = NULL,
            markdown = $10,
            disable_web_page_preview = $11,
            disable_notification = $12,
            pin = $13,
            webhook_action = $14,
            event_version = event_version + 1,
            delivery_status = $15,
            attempt_count = $16,
            next_attempt_at = CURRENT_TIMESTAMP,
            delivered_at = NULL,
            last_error = ''
";

/// A resolve only ever re-opens an active failure, or retries one whose resolve message
/// never got out; a row already resolved and delivered stays as it is.
const RESOLVE_WHERE_ACTIVE_SQL: &str = "(webhook_action <> $18 OR delivery_status = $19)";

type PgQuery<'q> = sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments>;

impl Outbox {
    pub fn new(db: Db, settings: Arc<SettingsStore>) -> Self {
        Self { db, settings }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// `enqueue_notification`: queue one notification, unless the source has its `notify`
    /// toggle off, in which case nothing is written and `None` comes back.
    pub async fn enqueue(&self, notification: &NewNotification) -> Result<Option<NotificationRecord>, Error> {
        if !self.settings.notify_enabled(&notification.source).await? {
            return Ok(None);
        }
        let dedupe_key = py_strip(&notification.dedupe_key);
        let webhook_action = if dedupe_key.is_empty() { WEBHOOK_ACTION_SEND } else { WEBHOOK_ACTION_UPSERT };
        let fields = delivery_fields(&RenderInput {
            kind: &notification.kind,
            header: &notification.header,
            title: &notification.title,
            body: &notification.body,
            link_url: &notification.link_url,
            webhook_action,
            occurrence_count: 1,
        });
        let sql = format!(
            "INSERT INTO notifications (
                kind, source, header, title, body, link_url, image_url, payload, dedupe_key, status, markdown,
                disable_web_page_preview, disable_notification, pin, webhook_action, occurrence_count, event_version,
                delivery_status, attempt_count, next_attempt_at, last_error, scope
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, CURRENT_TIMESTAMP, $20, $21)
            ON CONFLICT (dedupe_key) WHERE dedupe_key <> ''
            DO UPDATE SET
                kind = EXCLUDED.kind,
                source = EXCLUDED.source,
                header = EXCLUDED.header,
                title = EXCLUDED.title,
                body = EXCLUDED.body,
                link_url = EXCLUDED.link_url,
                image_url = EXCLUDED.image_url,
                payload = EXCLUDED.payload,
                status = EXCLUDED.status,
                read_at = NULL,
                markdown = EXCLUDED.markdown,
                disable_web_page_preview = EXCLUDED.disable_web_page_preview,
                disable_notification = EXCLUDED.disable_notification,
                pin = EXCLUDED.pin,
                webhook_action = EXCLUDED.webhook_action,
                scope = EXCLUDED.scope,
                occurrence_count = CASE
                    WHEN notifications.webhook_action = '{WEBHOOK_ACTION_RESOLVE}' THEN 1
                    ELSE notifications.occurrence_count + 1
                END,
                event_version = notifications.event_version + 1,
                delivery_status = EXCLUDED.delivery_status,
                attempt_count = CASE
                    WHEN notifications.delivery_status IN ('{DELIVERY_PENDING}', '{DELIVERY_FAILED}')
                         AND notifications.next_attempt_at > CURRENT_TIMESTAMP
                        THEN notifications.attempt_count
                    ELSE EXCLUDED.attempt_count
                END,
                next_attempt_at = CASE
                    WHEN notifications.delivery_status IN ('{DELIVERY_PENDING}', '{DELIVERY_FAILED}')
                         AND notifications.next_attempt_at > CURRENT_TIMESTAMP
                        THEN notifications.next_attempt_at
                    ELSE CURRENT_TIMESTAMP
                END,
                delivered_at = NULL,
                last_error = CASE
                    WHEN notifications.delivery_status IN ('{DELIVERY_PENDING}', '{DELIVERY_FAILED}')
                         AND notifications.next_attempt_at > CURRENT_TIMESTAMP
                        THEN notifications.last_error
                    ELSE EXCLUDED.last_error
                END
            RETURNING {SELECT_FIELDS};"
        );
        let row = sqlx::query(AssertSqlSafe(sql))
            .bind(&notification.kind)
            .bind(&notification.source)
            .bind(&notification.header)
            .bind(&notification.title)
            .bind(&notification.body)
            .bind(&notification.link_url)
            .bind(&notification.image_url)
            .bind(payload_text(notification.payload.as_ref())?)
            .bind(dedupe_key)
            .bind(STATUS_UNREAD)
            .bind(fields.markdown)
            .bind(fields.disable_web_page_preview)
            .bind(fields.disable_notification)
            .bind(fields.pin)
            .bind(webhook_action)
            .bind(1_i32)
            .bind(1_i32)
            .bind(DELIVERY_PENDING)
            .bind(0_i32)
            .bind("")
            .bind(notification.scope.as_deref().unwrap_or(SCOPE_ITEM))
            .fetch_optional(self.db.pool())
            .await?;
        let row = row.ok_or_else(|| Error::Failed("Failed to create notification".to_owned()))?;
        Ok(Some(NotificationRecord::from_row(&row)?.rendered()))
    }

    fn bind_resolution<'q>(query: PgQuery<'q>, resolution: &'q Resolution, link_url: &'q str, image_url: &'q str) -> Result<PgQuery<'q>, Error> {
        let fields = delivery_fields(&RenderInput {
            kind: &resolution.kind,
            header: &resolution.header,
            title: &resolution.title,
            body: &resolution.body,
            link_url,
            webhook_action: WEBHOOK_ACTION_RESOLVE,
            occurrence_count: 1,
        });
        Ok(query
            .bind(&resolution.kind)
            .bind(&resolution.source)
            .bind(&resolution.header)
            .bind(&resolution.title)
            .bind(&resolution.body)
            .bind(link_url)
            .bind(image_url)
            .bind(payload_text(resolution.payload.as_ref())?)
            .bind(STATUS_UNREAD)
            .bind(fields.markdown)
            .bind(fields.disable_web_page_preview)
            .bind(fields.disable_notification)
            .bind(fields.pin)
            .bind(WEBHOOK_ACTION_RESOLVE)
            .bind(DELIVERY_PENDING)
            .bind(0_i32))
    }

    /// `resolve_notification`: close out a failure this source queued earlier.
    ///
    /// Deliberately not gated on `notify`: this only ever updates a row that was already
    /// queued, and leaving a pinned failure standing because the source was muted in the
    /// meantime is worse than the one message it takes to clear it. The row keeps
    /// `telegram_pinned_message_id` until the resolve message is delivered; that delivery
    /// is what unpins the old failure message.
    pub async fn resolve(&self, dedupe_key: &str, resolution: &Resolution) -> Result<Option<NotificationRecord>, Error> {
        let dedupe_key = py_strip(dedupe_key);
        if dedupe_key.is_empty() {
            return Ok(None);
        }
        let sql = format!(
            "UPDATE notifications {RESOLVE_SET_SQL} WHERE dedupe_key = $17 AND {RESOLVE_WHERE_ACTIVE_SQL} RETURNING {SELECT_FIELDS};"
        );
        let query = Self::bind_resolution(sqlx::query(AssertSqlSafe(sql)), resolution, &resolution.link_url, &resolution.image_url)?;
        let row = query
            .bind(dedupe_key)
            .bind(WEBHOOK_ACTION_RESOLVE)
            .bind(DELIVERY_FAILED)
            .fetch_optional(self.db.pool())
            .await?;
        Ok(row.as_ref().map(NotificationRecord::from_row).transpose()?.map(NotificationRecord::rendered))
    }

    /// `resolve_job_failure_notifications`: close every job-scoped failure queued for
    /// `job_key`. Item-scoped rows are left alone: a job can finish while one of its items
    /// is still broken, and only the module that owns the item knows when it recovers.
    pub async fn resolve_job_failures(&self, job_key: &str, resolution: &Resolution) -> Result<Vec<NotificationRecord>, Error> {
        let job_key = py_strip(job_key).to_lowercase();
        if job_key.is_empty() {
            return Ok(Vec::new());
        }
        let prefix = format_job_failure_dedupe_key(&job_key, "x");
        let prefix = &prefix[..prefix.len() - 1];
        let sql = format!(
            "UPDATE notifications {RESOLVE_SET_SQL}
             WHERE dedupe_key LIKE $17
               AND scope = $20
               AND {RESOLVE_WHERE_ACTIVE_SQL}
             RETURNING {SELECT_FIELDS};"
        );
        let query = Self::bind_resolution(sqlx::query(AssertSqlSafe(sql)), resolution, "", "")?;
        let rows = query
            .bind(format!("{}%", like_literal(prefix)))
            .bind(WEBHOOK_ACTION_RESOLVE)
            .bind(DELIVERY_FAILED)
            .bind(SCOPE_JOB)
            .fetch_all(self.db.pool())
            .await?;
        rows.iter()
            .map(|row| Ok(NotificationRecord::from_row(row)?.rendered()))
            .collect()
    }

    /// `claim_next_pending_notification`: the oldest due row, now `sending` under a lease,
    /// with its template re-rendered (so a repeat claimed later goes out silent).
    pub async fn claim_next_pending(&self) -> Result<Option<NotificationRecord>, Error> {
        self.reset_stale_sending().await?;
        let mut transaction = self.db.pool().begin().await?;
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {SELECT_FIELDS}
             FROM notifications
             WHERE delivery_status = $1
               AND next_attempt_at <= CURRENT_TIMESTAMP
             ORDER BY id ASC
             FOR UPDATE SKIP LOCKED
             LIMIT 1;"
        )))
        .bind(DELIVERY_PENDING)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.commit().await?;
            return Ok(None);
        };
        let notification = NotificationRecord::from_row(&row)?;
        let fields = delivery_fields(&RenderInput {
            kind: &notification.kind,
            header: &notification.header,
            title: &notification.title,
            body: &notification.body,
            link_url: &notification.link_url,
            webhook_action: &notification.webhook_action,
            occurrence_count: notification.occurrence_count,
        });
        let claimed = sqlx::query(AssertSqlSafe(format!(
            "UPDATE notifications
             SET delivery_status = $1,
                 markdown = $2,
                 disable_web_page_preview = $3,
                 disable_notification = $4,
                 pin = $5,
                 event_version = event_version + 1,
                 next_attempt_at = CURRENT_TIMESTAMP + ($6 * INTERVAL '1 second')
             WHERE id = $7
               AND delivery_status = $8
               AND event_version = $9
             RETURNING {SELECT_FIELDS};"
        )))
        .bind(DELIVERY_SENDING)
        .bind(fields.markdown)
        .bind(fields.disable_web_page_preview)
        .bind(fields.disable_notification)
        .bind(fields.pin)
        .bind(SENDING_LEASE_SECONDS)
        .bind(notification.notification_id)
        .bind(DELIVERY_PENDING)
        .bind(notification.event_version)
        .fetch_optional(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(claimed.as_ref().map(NotificationRecord::from_row).transpose()?.map(NotificationRecord::rendered))
    }

    /// `reset_stale_sending_notifications`: rows whose `sending` lease ran out go back to `pending`.
    pub async fn reset_stale_sending(&self) -> Result<(), Error> {
        sqlx::query(
            "UPDATE notifications
             SET delivery_status = $1,
                 next_attempt_at = CURRENT_TIMESTAMP,
                 last_error = CASE
                     WHEN last_error = '' THEN $2
                     ELSE last_error
                 END
             WHERE delivery_status = $3
               AND next_attempt_at <= CURRENT_TIMESTAMP;",
        )
        .bind(DELIVERY_PENDING)
        .bind(LEASE_EXPIRED_ERROR)
        .bind(DELIVERY_SENDING)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// `mark_notification_delivered`: close a delivery, remembering which Telegram message
    /// (if any) it left pinned. `pinned_message_id` is written as given: `None` means the
    /// delivery left nothing pinned.
    pub async fn mark_delivered(
        &self,
        notification_id: i64,
        event_version: i32,
        message_id: Option<i64>,
        pinned_message_id: Option<i64>,
    ) -> Result<(), Error> {
        sqlx::query(
            "UPDATE notifications
             SET delivery_status = $1,
                 delivered_at = CURRENT_TIMESTAMP,
                 status = $2,
                 read_at = COALESCE(read_at, CURRENT_TIMESTAMP),
                 last_error = '',
                 telegram_message_id = COALESCE($3, telegram_message_id),
                 telegram_pinned_message_id = $4
             WHERE id = $5
               AND delivery_status = $6
               AND event_version = $7;",
        )
        .bind(DELIVERY_DELIVERED)
        .bind(STATUS_READ)
        .bind(message_id)
        .bind(pinned_message_id)
        .bind(notification_id)
        .bind(DELIVERY_SENDING)
        .bind(event_version)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// `mark_notification_retry`: back to `pending`, due after the backoff or the server's
    /// `retry_after`, whichever is longer.
    pub async fn mark_retry(
        &self,
        notification_id: i64,
        event_version: i32,
        attempt_count: i32,
        error_message: &str,
        retry_after_seconds: Option<i64>,
    ) -> Result<(), Error> {
        let delay = retry_delay_seconds(attempt_count).max(retry_after_seconds.unwrap_or(0));
        let next_attempt_at = Utc::now() + Duration::seconds(delay);
        sqlx::query(
            "UPDATE notifications
             SET delivery_status = $1,
                 attempt_count = $2,
                 next_attempt_at = $3,
                 last_error = $4
             WHERE id = $5
               AND delivery_status = $6
               AND event_version = $7;",
        )
        .bind(DELIVERY_PENDING)
        .bind(attempt_count)
        .bind(next_attempt_at)
        .bind(py_strip(error_message))
        .bind(notification_id)
        .bind(DELIVERY_SENDING)
        .bind(event_version)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// `mark_notification_failed`: a permanent failure; the row stays `failed`.
    pub async fn mark_failed(&self, notification_id: i64, event_version: i32, attempt_count: i32, error_message: &str) -> Result<(), Error> {
        sqlx::query(
            "UPDATE notifications
             SET delivery_status = $1,
                 attempt_count = $2,
                 next_attempt_at = CURRENT_TIMESTAMP,
                 last_error = $3
             WHERE id = $4
               AND delivery_status = $5
               AND event_version = $6;",
        )
        .bind(DELIVERY_FAILED)
        .bind(attempt_count)
        .bind(py_strip(error_message))
        .bind(notification_id)
        .bind(DELIVERY_SENDING)
        .bind(event_version)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
