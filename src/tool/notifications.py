# ruff: noqa: PLR0913, S608, TC003

from __future__ import annotations

import asyncio
import json
from collections.abc import Mapping
from dataclasses import dataclass, replace
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

from src.core import settings

from . import database, telegram_bot

STATUS_READ = 'read'
STATUS_UNREAD = 'unread'

DELIVERY_PENDING = 'pending'
DELIVERY_SENDING = 'sending'
DELIVERY_DELIVERED = 'delivered'
DELIVERY_FAILED = 'failed'

WEBHOOK_ACTION_SEND = 'send'
WEBHOOK_ACTION_UPSERT = 'upsert'
WEBHOOK_ACTION_RESOLVE = 'resolve'

# ``job``: the whole job raised, queued by the runner; cleared when the job next
# succeeds. ``item``: one unit of work inside a job (a video, a parser), which the
# owning module clears itself once that unit goes through.
SCOPE_ITEM = 'item'
SCOPE_JOB = 'job'

_SENDING_LEASE_SECONDS = 600
# Telegram only wants ')' and '\' escaped inside the (...) part of an inline link.
_MARKDOWN_V2_URL_SPECIAL_CHARS = frozenset('\\)')
_SCHEMA_READY = False
_SCHEMA_LOCK = asyncio.Lock()

_SELECT_FIELDS = """
    id,
    kind,
    source,
    header,
    title,
    body,
    link_url,
    image_url,
    payload,
    dedupe_key,
    status,
    created_at,
    read_at,
    markdown,
    disable_web_page_preview,
    disable_notification,
    pin,
    webhook_action,
    occurrence_count,
    event_version,
    delivery_status,
    attempt_count,
    next_attempt_at,
    delivered_at,
    last_error,
    scope,
    telegram_message_id,
    telegram_pinned_message_id
"""

_ENSURE_NOTIFICATIONS_SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS notifications (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL,
    source TEXT NOT NULL,
    header TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    link_url TEXT NOT NULL DEFAULT '',
    image_url TEXT NOT NULL DEFAULT '',
    payload TEXT NOT NULL DEFAULT '{}',
    dedupe_key TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'unread',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    read_at TIMESTAMPTZ NULL,
    markdown TEXT NOT NULL DEFAULT '',
    disable_web_page_preview BOOLEAN NOT NULL DEFAULT TRUE,
    disable_notification BOOLEAN NOT NULL DEFAULT TRUE,
    pin BOOLEAN NOT NULL DEFAULT FALSE,
    webhook_action TEXT NOT NULL DEFAULT 'send',
    occurrence_count INTEGER NOT NULL DEFAULT 1,
    event_version INTEGER NOT NULL DEFAULT 1,
    delivery_status TEXT NOT NULL DEFAULT 'pending',
    attempt_count INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    delivered_at TIMESTAMPTZ NULL,
    last_error TEXT NOT NULL DEFAULT '',
    scope TEXT NOT NULL DEFAULT 'item',
    telegram_message_id BIGINT NULL,
    telegram_pinned_message_id BIGINT NULL
);

ALTER TABLE notifications ADD COLUMN IF NOT EXISTS dedupe_key TEXT NOT NULL DEFAULT '';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS header TEXT NOT NULL DEFAULT '';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS markdown TEXT NOT NULL DEFAULT '';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS disable_web_page_preview BOOLEAN NOT NULL DEFAULT TRUE;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS disable_notification BOOLEAN NOT NULL DEFAULT TRUE;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS pin BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS webhook_action TEXT NOT NULL DEFAULT 'send';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS occurrence_count INTEGER NOT NULL DEFAULT 1;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS event_version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS delivery_status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS attempt_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS delivered_at TIMESTAMPTZ NULL;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS last_error TEXT NOT NULL DEFAULT '';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT 'item';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS telegram_message_id BIGINT NULL;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS telegram_pinned_message_id BIGINT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS notifications_dedupe_key_unique
ON notifications (dedupe_key)
WHERE dedupe_key <> '';

UPDATE notifications
SET delivery_status = 'delivered',
    delivered_at = COALESCE(delivered_at, read_at, created_at)
WHERE status = 'read'
  AND delivery_status <> 'delivered';

-- Rows the runner queued before ``scope`` existed: only it records ``error_class``.
-- Written without LIKE: psycopg rejects a bare percent sign in parameterless SQL.
UPDATE notifications
SET scope = 'job'
WHERE scope = 'item'
  AND source = 'worker'
  AND strpos(dedupe_key, 'job_failed:') = 1
  AND strpos(payload, '"error_class"') > 0;
"""


@dataclass(frozen=True, slots=True)
class NotificationRecord:
    notification_id: int
    kind: str
    source: str
    title: str
    body: str
    link_url: str
    image_url: str
    payload: str
    dedupe_key: str
    status: str
    markdown: str
    disable_web_page_preview: bool
    disable_notification: bool
    webhook_action: str
    occurrence_count: int
    event_version: int
    delivery_status: str
    attempt_count: int
    next_attempt_at: datetime
    created_at: datetime
    read_at: datetime | None = None
    delivered_at: datetime | None = None
    last_error: str = ''
    pin: bool = False
    header: str = ''
    scope: str = SCOPE_ITEM
    # Telegram ids of the last message this row produced and of the one it still
    # keeps pinned, so a later delivery (a repeat or the resolve) can unpin it.
    telegram_message_id: int | None = None
    telegram_pinned_message_id: int | None = None

    @property
    def payload_json(self) -> dict[str, Any]:
        try:
            parsed = json.loads(self.payload)
        except json.JSONDecodeError:
            return {}
        return parsed if isinstance(parsed, dict) else {}

    @property
    def local_image_path(self) -> Path | None:
        image_path = self.payload_json.get('image_path')
        if not isinstance(image_path, str):
            return None
        normalized_path = image_path.strip()
        return Path(normalized_path) if normalized_path else None


def format_job_failure_dedupe_key(*, job_key: str, failure_key: str) -> str:
    normalized_job_key = job_key.strip().lower()
    normalized_failure_key = failure_key.strip()
    if not normalized_job_key or not normalized_failure_key:
        return ''
    return f'job_failed:{normalized_job_key}:{normalized_failure_key}'


def _escape_markdown_v2(value: str) -> str:
    # Kept as a local name so the call sites below stay readable; the escaping
    # itself belongs next to the senders that need it.
    return telegram_bot.escape_markdown_v2(value)


def _escape_markdown_v2_url(value: str) -> str:
    escaped: list[str] = []
    for char in value:
        if char in _MARKDOWN_V2_URL_SPECIAL_CHARS:
            escaped.append('\\')
        escaped.append(char)
    return ''.join(escaped)


def _notification_delivery_fields(
    *,
    kind: str,
    header: str = '',
    title: str,
    body: str,
    link_url: str,
    image_url: str,
    webhook_action: str,
    occurrence_count: int,
) -> tuple[str, bool, bool, bool]:
    del image_url
    parts: list[str] = []
    normalized_header = header.strip()
    normalized_title = title.strip()
    normalized_body = body.strip()
    normalized_link_url = link_url.strip()
    is_active_job_failure = kind == 'job_failed' and webhook_action != WEBHOOK_ACTION_RESOLVE

    # The header names the app and the source, so the title line is free to be the item itself.
    if normalized_header:
        parts.append(telegram_bot.format_header_line(normalized_header))
    if normalized_title:
        title_markdown = f'*{_escape_markdown_v2(normalized_title)}*'
        if normalized_link_url:
            title_markdown = f'[{title_markdown}]({_escape_markdown_v2_url(normalized_link_url)})'
        parts.append(title_markdown)
    if normalized_body:
        parts.append(_escape_markdown_v2(normalized_body))
    if is_active_job_failure and occurrence_count > 1:
        parts.append(_escape_markdown_v2(f'Occurrences: {occurrence_count}'))
    # A titleless notification has nothing to carry the link, so it still gets its own line.
    if normalized_link_url and not normalized_title:
        parts.append(_escape_markdown_v2(normalized_link_url))

    return '\n'.join(parts), not bool(normalized_link_url), not is_active_job_failure, is_active_job_failure


def _from_row(row: Mapping[str, Any]) -> NotificationRecord:
    return NotificationRecord(
        notification_id=int(row['id']),
        kind=str(row['kind']),
        source=str(row['source']),
        header=str(row.get('header') or ''),
        title=str(row.get('title') or ''),
        body=str(row.get('body') or ''),
        link_url=str(row.get('link_url') or ''),
        image_url=str(row.get('image_url') or ''),
        payload=str(row.get('payload') or '{}'),
        dedupe_key=str(row.get('dedupe_key') or ''),
        status=str(row.get('status') or STATUS_UNREAD),
        markdown=str(row.get('markdown') or ''),
        disable_web_page_preview=bool(row.get('disable_web_page_preview', True)),
        disable_notification=bool(row.get('disable_notification', True)),
        pin=bool(row.get('pin', False)),
        webhook_action=str(row.get('webhook_action') or WEBHOOK_ACTION_SEND),
        occurrence_count=int(row.get('occurrence_count') or 1),
        event_version=int(row.get('event_version') or 1),
        delivery_status=str(row.get('delivery_status') or DELIVERY_PENDING),
        attempt_count=int(row.get('attempt_count') or 0),
        next_attempt_at=row['next_attempt_at'],
        created_at=row['created_at'],
        read_at=row.get('read_at'),
        delivered_at=row.get('delivered_at'),
        last_error=str(row.get('last_error') or ''),
        scope=str(row.get('scope') or SCOPE_ITEM),
        telegram_message_id=_optional_int(row.get('telegram_message_id')),
        telegram_pinned_message_id=_optional_int(row.get('telegram_pinned_message_id')),
    )


def _optional_int(value: object) -> int | None:
    if value is None or isinstance(value, bool):
        return None
    try:
        return int(value)  # type: ignore[call-overload]
    except (TypeError, ValueError):
        return None


def _with_rendered_delivery_fields(notification: NotificationRecord) -> NotificationRecord:
    markdown, disable_web_page_preview, disable_notification, pin = _notification_delivery_fields(
        kind=notification.kind,
        header=notification.header,
        title=notification.title,
        body=notification.body,
        link_url=notification.link_url,
        image_url=notification.image_url,
        webhook_action=notification.webhook_action,
        occurrence_count=notification.occurrence_count,
    )
    if (
        notification.markdown == markdown
        and notification.disable_web_page_preview == disable_web_page_preview
        and notification.disable_notification == disable_notification
        and notification.pin == pin
    ):
        return notification
    return replace(
        notification,
        markdown=markdown,
        disable_web_page_preview=disable_web_page_preview,
        disable_notification=disable_notification,
        pin=pin,
    )


async def ensure_notifications_table() -> None:
    global _SCHEMA_READY
    if _SCHEMA_READY:
        return

    async with _SCHEMA_LOCK:
        if _SCHEMA_READY:
            return
        await database.query_db_multi(_ENSURE_NOTIFICATIONS_SCHEMA_SQL)
        _SCHEMA_READY = True


async def enqueue_notification(
    *,
    kind: str,
    source: str,
    header: str = '',
    title: str = '',
    body: str = '',
    link_url: str = '',
    image_url: str = '',
    payload: Mapping[str, Any] | None = None,
    dedupe_key: str = '',
    scope: str = SCOPE_ITEM,
) -> NotificationRecord | None:
    """Queue one notification, unless the source has its ``notify`` toggle off.

    A muted source is dropped here rather than at delivery: the outbox exists to
    get messages out, so a row nobody will ever send is not worth keeping. The
    single gate also covers sources added later, which is why it lives here
    instead of at each call site.
    """
    if not settings.notify_enabled(source):
        return None

    await ensure_notifications_table()
    normalized_dedupe_key = dedupe_key.strip()
    webhook_action = WEBHOOK_ACTION_UPSERT if normalized_dedupe_key else WEBHOOK_ACTION_SEND
    payload_text = json.dumps(payload or {}, ensure_ascii=False, separators=(',', ':'))
    markdown, disable_web_page_preview, disable_notification, pin = _notification_delivery_fields(
        kind=kind,
        header=header,
        title=title,
        body=body,
        link_url=link_url,
        image_url=image_url,
        webhook_action=webhook_action,
        occurrence_count=1,
    )
    rows = await database.query_db(
        f"""
        INSERT INTO notifications (
            kind,
            source,
            header,
            title,
            body,
            link_url,
            image_url,
            payload,
            dedupe_key,
            status,
            markdown,
            disable_web_page_preview,
            disable_notification,
            pin,
            webhook_action,
            occurrence_count,
            event_version,
            delivery_status,
            attempt_count,
            next_attempt_at,
            last_error,
            scope
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, ?, ?)
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
        RETURNING {_SELECT_FIELDS};
        """,
        (
            kind,
            source,
            header,
            title,
            body,
            link_url,
            image_url,
            payload_text,
            normalized_dedupe_key,
            STATUS_UNREAD,
            markdown,
            disable_web_page_preview,
            disable_notification,
            pin,
            webhook_action,
            1,
            1,
            DELIVERY_PENDING,
            0,
            '',
            scope,
        ),
    )
    if not rows:
        msg = 'Failed to create notification'
        raise RuntimeError(msg)
    return _with_rendered_delivery_fields(_from_row(rows[0]))


_RESOLVE_SET_SQL = """
        SET kind = ?,
            source = ?,
            header = ?,
            title = ?,
            body = ?,
            link_url = ?,
            image_url = ?,
            payload = ?,
            status = ?,
            read_at = NULL,
            markdown = ?,
            disable_web_page_preview = ?,
            disable_notification = ?,
            pin = ?,
            webhook_action = ?,
            event_version = event_version + 1,
            delivery_status = ?,
            attempt_count = ?,
            next_attempt_at = CURRENT_TIMESTAMP,
            delivered_at = NULL,
            last_error = ''
"""

# A resolve only ever re-opens an active failure, or retries one whose resolve
# message never got out; a row already resolved and delivered stays as it is.
_RESOLVE_WHERE_ACTIVE_SQL = '(webhook_action <> ? OR delivery_status = ?)'


def _resolve_set_params(
    *,
    kind: str,
    source: str,
    header: str,
    title: str,
    body: str,
    link_url: str,
    image_url: str,
    payload: Mapping[str, Any] | None,
) -> tuple[object, ...]:
    payload_text = json.dumps(payload or {}, ensure_ascii=False, separators=(',', ':'))
    markdown, disable_web_page_preview, disable_notification, pin = _notification_delivery_fields(
        kind=kind,
        header=header,
        title=title,
        body=body,
        link_url=link_url,
        image_url=image_url,
        webhook_action=WEBHOOK_ACTION_RESOLVE,
        occurrence_count=1,
    )
    return (
        kind,
        source,
        header,
        title,
        body,
        link_url,
        image_url,
        payload_text,
        STATUS_UNREAD,
        markdown,
        disable_web_page_preview,
        disable_notification,
        pin,
        WEBHOOK_ACTION_RESOLVE,
        DELIVERY_PENDING,
        0,
    )


async def resolve_notification(
    *,
    dedupe_key: str,
    kind: str,
    source: str,
    header: str = '',
    title: str,
    body: str = '',
    link_url: str = '',
    image_url: str = '',
    payload: Mapping[str, Any] | None = None,
) -> NotificationRecord | None:
    """Close out a failure this source queued earlier.

    Deliberately not gated on ``notify``: this only ever updates a row that was
    already queued, and leaving a pinned failure standing because the source was
    muted in the meantime is worse than the one message it takes to clear it.

    The row keeps ``telegram_pinned_message_id`` until the resolve message is
    delivered; that delivery is what unpins the old failure message.
    """
    normalized_dedupe_key = dedupe_key.strip()
    if not normalized_dedupe_key:
        return None

    await ensure_notifications_table()
    rows = await database.query_db(
        f"""
        UPDATE notifications
        {_RESOLVE_SET_SQL}
        WHERE dedupe_key = ?
          AND {_RESOLVE_WHERE_ACTIVE_SQL}
        RETURNING {_SELECT_FIELDS};
        """,
        (
            *_resolve_set_params(
                kind=kind,
                source=source,
                header=header,
                title=title,
                body=body,
                link_url=link_url,
                image_url=image_url,
                payload=payload,
            ),
            normalized_dedupe_key,
            WEBHOOK_ACTION_RESOLVE,
            DELIVERY_FAILED,
        ),
    )
    if not rows:
        return None
    return _with_rendered_delivery_fields(_from_row(rows[0]))


def _like_literal(value: str) -> str:
    return value.replace('\\', '\\\\').replace('%', '\\%').replace('_', '\\_')


async def resolve_job_failure_notifications(
    *,
    job_key: str,
    kind: str = 'job_recovered',
    source: str = 'worker',
    header: str = '',
    title: str = 'Job recovered',
    body: str = '',
    payload: Mapping[str, Any] | None = None,
) -> list[NotificationRecord]:
    """Close every job-scoped failure queued for ``job_key``.

    Meant for the runner's success path: whatever ``update()`` raised last time
    is over once it returns cleanly, whichever dedupe key the exception carried.
    Item-scoped rows are left alone; a job can finish while one of its items is
    still broken, and only the module that owns the item knows when it recovers.
    """
    normalized_job_key = job_key.strip().lower()
    if not normalized_job_key:
        return []

    await ensure_notifications_table()
    prefix = format_job_failure_dedupe_key(job_key=normalized_job_key, failure_key='x')[:-1]
    rows = await database.query_db(
        f"""
        UPDATE notifications
        {_RESOLVE_SET_SQL}
        WHERE dedupe_key LIKE ?
          AND scope = ?
          AND {_RESOLVE_WHERE_ACTIVE_SQL}
        RETURNING {_SELECT_FIELDS};
        """,
        (
            *_resolve_set_params(
                kind=kind,
                source=source,
                header=header,
                title=title,
                body=body,
                link_url='',
                image_url='',
                payload=payload,
            ),
            f'{_like_literal(prefix)}%',
            SCOPE_JOB,
            WEBHOOK_ACTION_RESOLVE,
            DELIVERY_FAILED,
        ),
    )
    return [_with_rendered_delivery_fields(_from_row(row)) for row in rows]


async def claim_next_pending_notification() -> NotificationRecord | None:
    await ensure_notifications_table()
    await reset_stale_sending_notifications()

    async with (
        database.connection() as conn,
        conn.transaction(),
        conn.cursor() as cursor,
    ):
        await cursor.execute(
            f"""
            SELECT {_SELECT_FIELDS}
            FROM notifications
            WHERE delivery_status = %s
              AND next_attempt_at <= CURRENT_TIMESTAMP
            ORDER BY id ASC
            FOR UPDATE SKIP LOCKED
            LIMIT 1;
            """,
            (DELIVERY_PENDING,),
        )
        row = await cursor.fetchone()
        if row is None:
            return None

        notification = _from_row(row)
        markdown, disable_web_page_preview, disable_notification, pin = _notification_delivery_fields(
            kind=notification.kind,
            header=notification.header,
            title=notification.title,
            body=notification.body,
            link_url=notification.link_url,
            image_url=notification.image_url,
            webhook_action=notification.webhook_action,
            occurrence_count=notification.occurrence_count,
        )

        await cursor.execute(
            f"""
            UPDATE notifications
            SET delivery_status = %s,
                markdown = %s,
                disable_web_page_preview = %s,
                disable_notification = %s,
                pin = %s,
                event_version = event_version + 1,
                next_attempt_at = CURRENT_TIMESTAMP + (%s * INTERVAL '1 second')
            WHERE id = %s
              AND delivery_status = %s
              AND event_version = %s
            RETURNING {_SELECT_FIELDS};
            """,
            (
                DELIVERY_SENDING,
                markdown,
                disable_web_page_preview,
                disable_notification,
                pin,
                _SENDING_LEASE_SECONDS,
                notification.notification_id,
                DELIVERY_PENDING,
                notification.event_version,
            ),
        )
        claimed_row = await cursor.fetchone()

    if claimed_row is None:
        return None
    return _with_rendered_delivery_fields(_from_row(claimed_row))


async def reset_stale_sending_notifications() -> None:
    await ensure_notifications_table()
    await database.query_db(
        """
        UPDATE notifications
        SET delivery_status = ?,
            next_attempt_at = CURRENT_TIMESTAMP,
            last_error = CASE
                WHEN last_error = '' THEN ?
                ELSE last_error
            END
        WHERE delivery_status = ?
          AND next_attempt_at <= CURRENT_TIMESTAMP;
        """,
        (DELIVERY_PENDING, 'Notification delivery lease expired before completion', DELIVERY_SENDING),
    )


def retry_delay_seconds(next_attempt_count: int) -> int:
    if next_attempt_count <= 0:
        return 0
    return min(600, 30 * (2 ** (next_attempt_count - 1)))


async def mark_notification_delivered(
    notification_id: int,
    *,
    event_version: int,
    message_id: int | None = None,
    pinned_message_id: int | None = None,
) -> None:
    """Close a delivery, remembering which Telegram message (if any) it left pinned.

    ``pinned_message_id`` is written as given: ``None`` means the delivery left
    nothing pinned, which is what a resolve or an unpinned repeat should record.
    """
    await ensure_notifications_table()
    await database.query_db(
        """
        UPDATE notifications
        SET delivery_status = ?,
            delivered_at = CURRENT_TIMESTAMP,
            status = ?,
            read_at = COALESCE(read_at, CURRENT_TIMESTAMP),
            last_error = '',
            telegram_message_id = COALESCE(?, telegram_message_id),
            telegram_pinned_message_id = ?
        WHERE id = ?
          AND delivery_status = ?
          AND event_version = ?;
        """,
        (DELIVERY_DELIVERED, STATUS_READ, message_id, pinned_message_id, notification_id, DELIVERY_SENDING, event_version),
    )


async def mark_notification_retry(
    notification_id: int,
    *,
    event_version: int,
    attempt_count: int,
    error_message: str,
    retry_after_seconds: int | None = None,
) -> None:
    await ensure_notifications_table()
    delay_seconds = max(retry_delay_seconds(attempt_count), retry_after_seconds or 0)
    next_attempt_at = datetime.now(tz=UTC) + timedelta(seconds=delay_seconds)
    await database.query_db(
        """
        UPDATE notifications
        SET delivery_status = ?,
            attempt_count = ?,
            next_attempt_at = ?,
            last_error = ?
        WHERE id = ?
          AND delivery_status = ?
          AND event_version = ?;
        """,
        (DELIVERY_PENDING, attempt_count, next_attempt_at, error_message.strip(), notification_id, DELIVERY_SENDING, event_version),
    )


async def mark_notification_failed(
    notification_id: int,
    *,
    event_version: int,
    attempt_count: int,
    error_message: str,
) -> None:
    await ensure_notifications_table()
    await database.query_db(
        """
        UPDATE notifications
        SET delivery_status = ?,
            attempt_count = ?,
            next_attempt_at = CURRENT_TIMESTAMP,
            last_error = ?
        WHERE id = ?
          AND delivery_status = ?
          AND event_version = ?;
        """,
        (DELIVERY_FAILED, attempt_count, error_message.strip(), notification_id, DELIVERY_SENDING, event_version),
    )
