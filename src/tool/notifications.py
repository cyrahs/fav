# ruff: noqa: PLR0913, S608, TC003

from __future__ import annotations

import asyncio
import json
from collections.abc import Mapping
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from typing import Any

import psycopg
from psycopg.rows import dict_row

from src.core.config import config

from . import database

STATUS_READ = 'read'
STATUS_UNREAD = 'unread'

DELIVERY_PENDING = 'pending'
DELIVERY_SENDING = 'sending'
DELIVERY_DELIVERED = 'delivered'
DELIVERY_FAILED = 'failed'

_MARKDOWN_V2_SPECIAL_CHARS = frozenset('\\_*[]()~`>#+-=|{}.!')
_SCHEMA_READY = False
_SCHEMA_LOCK = asyncio.Lock()

_SELECT_FIELDS = """
    id,
    kind,
    source,
    title,
    body,
    link_url,
    image_url,
    payload,
    status,
    created_at,
    read_at,
    markdown,
    disable_web_page_preview,
    disable_notification,
    delivery_status,
    attempt_count,
    next_attempt_at,
    delivered_at,
    last_error
"""

_ENSURE_NOTIFICATIONS_SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS notifications (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL,
    source TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    link_url TEXT NOT NULL DEFAULT '',
    image_url TEXT NOT NULL DEFAULT '',
    payload TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'unread',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    read_at TIMESTAMPTZ NULL,
    markdown TEXT NOT NULL DEFAULT '',
    disable_web_page_preview BOOLEAN NOT NULL DEFAULT TRUE,
    disable_notification BOOLEAN NOT NULL DEFAULT TRUE,
    delivery_status TEXT NOT NULL DEFAULT 'pending',
    attempt_count INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    delivered_at TIMESTAMPTZ NULL,
    last_error TEXT NOT NULL DEFAULT ''
);

ALTER TABLE notifications ADD COLUMN IF NOT EXISTS markdown TEXT NOT NULL DEFAULT '';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS disable_web_page_preview BOOLEAN NOT NULL DEFAULT TRUE;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS disable_notification BOOLEAN NOT NULL DEFAULT TRUE;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS delivery_status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS attempt_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS delivered_at TIMESTAMPTZ NULL;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS last_error TEXT NOT NULL DEFAULT '';

UPDATE notifications
SET delivery_status = 'delivered',
    delivered_at = COALESCE(delivered_at, read_at, created_at)
WHERE status = 'read'
  AND delivery_status <> 'delivered';
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
    status: str
    markdown: str
    disable_web_page_preview: bool
    disable_notification: bool
    delivery_status: str
    attempt_count: int
    next_attempt_at: datetime
    created_at: datetime
    read_at: datetime | None = None
    delivered_at: datetime | None = None
    last_error: str = ''

    @property
    def payload_json(self) -> dict[str, Any]:
        try:
            parsed = json.loads(self.payload)
        except json.JSONDecodeError:
            return {}
        return parsed if isinstance(parsed, dict) else {}

    @property
    def webhook_payload(self) -> dict[str, Any]:
        return {
            'markdown': self.markdown,
            'disable_web_page_preview': self.disable_web_page_preview,
            'disable_notification': self.disable_notification,
        }


def _postgres_dsn() -> str:
    dsn = str(config.database.postgres_dsn).strip()
    if not dsn:
        msg = 'database.postgres_dsn is required'
        raise ValueError(msg)
    return dsn


def _escape_markdown_v2(value: str) -> str:
    escaped: list[str] = []
    for char in value:
        if char in _MARKDOWN_V2_SPECIAL_CHARS:
            escaped.append('\\')
        escaped.append(char)
    return ''.join(escaped)


def _notification_delivery_fields(
    *,
    kind: str,
    title: str,
    body: str,
    link_url: str,
    image_url: str,
) -> tuple[str, bool, bool]:
    preview_url = link_url.strip() or image_url.strip()
    parts: list[str] = []
    normalized_title = title.strip()
    normalized_body = body.strip()
    normalized_preview_url = preview_url.strip()

    if normalized_title:
        parts.append(f'*{_escape_markdown_v2(normalized_title)}*')
    if normalized_body:
        parts.append(_escape_markdown_v2(normalized_body))
    if normalized_preview_url:
        parts.append(_escape_markdown_v2(normalized_preview_url))

    return '\n'.join(parts), not bool(normalized_preview_url), kind != 'job_failed'


def _from_row(row: Mapping[str, Any]) -> NotificationRecord:
    return NotificationRecord(
        notification_id=int(row['id']),
        kind=str(row['kind']),
        source=str(row['source']),
        title=str(row.get('title') or ''),
        body=str(row.get('body') or ''),
        link_url=str(row.get('link_url') or ''),
        image_url=str(row.get('image_url') or ''),
        payload=str(row.get('payload') or '{}'),
        status=str(row.get('status') or STATUS_UNREAD),
        markdown=str(row.get('markdown') or ''),
        disable_web_page_preview=bool(row.get('disable_web_page_preview', True)),
        disable_notification=bool(row.get('disable_notification', True)),
        delivery_status=str(row.get('delivery_status') or DELIVERY_PENDING),
        attempt_count=int(row.get('attempt_count') or 0),
        next_attempt_at=row['next_attempt_at'],
        created_at=row['created_at'],
        read_at=row.get('read_at'),
        delivered_at=row.get('delivered_at'),
        last_error=str(row.get('last_error') or ''),
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
    title: str = '',
    body: str = '',
    link_url: str = '',
    image_url: str = '',
    payload: Mapping[str, Any] | None = None,
) -> NotificationRecord:
    await ensure_notifications_table()
    payload_text = json.dumps(payload or {}, ensure_ascii=False, separators=(',', ':'))
    markdown, disable_web_page_preview, disable_notification = _notification_delivery_fields(
        kind=kind,
        title=title,
        body=body,
        link_url=link_url,
        image_url=image_url,
    )
    rows = await database.query_db(
        f"""
        INSERT INTO notifications (
            kind,
            source,
            title,
            body,
            link_url,
            image_url,
            payload,
            status,
            markdown,
            disable_web_page_preview,
            disable_notification,
            delivery_status,
            attempt_count,
            next_attempt_at,
            last_error
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP, ?)
        RETURNING {_SELECT_FIELDS};
        """,
        (
            kind,
            source,
            title,
            body,
            link_url,
            image_url,
            payload_text,
            STATUS_UNREAD,
            markdown,
            disable_web_page_preview,
            disable_notification,
            DELIVERY_PENDING,
            0,
            '',
        ),
    )
    if not rows:
        msg = 'Failed to create notification'
        raise RuntimeError(msg)
    return _from_row(rows[0])


async def claim_next_pending_notification() -> NotificationRecord | None:
    await ensure_notifications_table()

    async with (
        await psycopg.AsyncConnection.connect(_postgres_dsn(), autocommit=False, row_factory=dict_row) as conn,
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
        markdown, disable_web_page_preview, disable_notification = _notification_delivery_fields(
            kind=notification.kind,
            title=notification.title,
            body=notification.body,
            link_url=notification.link_url,
            image_url=notification.image_url,
        )

        await cursor.execute(
            f"""
            UPDATE notifications
            SET delivery_status = %s,
                markdown = %s,
                disable_web_page_preview = %s,
                disable_notification = %s
            WHERE id = %s
            RETURNING {_SELECT_FIELDS};
            """,
            (
                DELIVERY_SENDING,
                markdown,
                disable_web_page_preview,
                disable_notification,
                notification.notification_id,
            ),
        )
        claimed_row = await cursor.fetchone()

    if claimed_row is None:
        return None
    return _from_row(claimed_row)


def retry_delay_seconds(next_attempt_count: int) -> int:
    if next_attempt_count <= 0:
        return 0
    return min(600, 30 * (2 ** (next_attempt_count - 1)))


async def mark_notification_delivered(notification_id: int) -> None:
    await ensure_notifications_table()
    await database.query_db(
        """
        UPDATE notifications
        SET delivery_status = ?,
            delivered_at = CURRENT_TIMESTAMP,
            status = ?,
            read_at = COALESCE(read_at, CURRENT_TIMESTAMP),
            last_error = ''
        WHERE id = ?;
        """,
        (DELIVERY_DELIVERED, STATUS_READ, notification_id),
    )


async def mark_notification_retry(
    notification_id: int,
    *,
    attempt_count: int,
    error_message: str,
) -> None:
    await ensure_notifications_table()
    next_attempt_at = datetime.now(tz=UTC) + timedelta(seconds=retry_delay_seconds(attempt_count))
    await database.query_db(
        """
        UPDATE notifications
        SET delivery_status = ?,
            attempt_count = ?,
            next_attempt_at = ?,
            last_error = ?
        WHERE id = ?;
        """,
        (DELIVERY_PENDING, attempt_count, next_attempt_at, error_message.strip(), notification_id),
    )


async def mark_notification_failed(
    notification_id: int,
    *,
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
        WHERE id = ?;
        """,
        (DELIVERY_FAILED, attempt_count, error_message.strip(), notification_id),
    )
