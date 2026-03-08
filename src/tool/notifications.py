from __future__ import annotations

import json
from collections.abc import Mapping
from dataclasses import dataclass
from datetime import datetime
from typing import Any

import psycopg
from psycopg.rows import dict_row

from . import database

STATUS_READ = 'read'
STATUS_UNREAD = 'unread'
VALID_LIST_STATUSES = {STATUS_UNREAD, 'all'}

_CREATE_NOTIFICATIONS_TABLE_SQL = """
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
    read_at TIMESTAMPTZ NULL
);
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
    created_at: datetime
    read_at: datetime | None = None

    @property
    def payload_json(self) -> dict[str, Any]:
        try:
            parsed = json.loads(self.payload)
        except json.JSONDecodeError:
            return {}
        return parsed if isinstance(parsed, dict) else {}


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
        created_at=row['created_at'],
        read_at=row.get('read_at'),
    )


def ensure_notifications_table_sync(dsn: str) -> None:
    with psycopg.connect(dsn, autocommit=True) as conn, conn.cursor() as cursor:
        cursor.execute(_CREATE_NOTIFICATIONS_TABLE_SQL)


def list_notifications_sync(
    dsn: str,
    *,
    status: str = STATUS_UNREAD,
    limit: int = 50,
    after_id: int | None = None,
) -> list[NotificationRecord]:
    if status not in VALID_LIST_STATUSES:
        msg = f'Unsupported notification list status: {status}'
        raise ValueError(msg)
    if limit <= 0:
        msg = 'limit must be greater than 0'
        raise ValueError(msg)
    if after_id is not None and after_id < 0:
        msg = 'after_id must be greater than or equal to 0'
        raise ValueError(msg)

    ensure_notifications_table_sync(dsn)

    clauses: list[str] = []
    params: list[Any] = []
    if status == STATUS_UNREAD:
        clauses.append('status = %s')
        params.append(STATUS_UNREAD)
    if after_id is not None:
        clauses.append('id > %s')
        params.append(after_id)

    where_sql = f'WHERE {" AND ".join(clauses)}' if clauses else ''
    sql = f"""
        SELECT id, kind, source, title, body, link_url, image_url, payload, status, created_at, read_at
        FROM notifications
        {where_sql}
        ORDER BY id ASC
        LIMIT %s;
    """
    params.append(limit)

    with psycopg.connect(dsn, autocommit=True, row_factory=dict_row) as conn, conn.cursor() as cursor:
        cursor.execute(sql, tuple(params))
        rows = cursor.fetchall()
    return [_from_row(row) for row in rows]


def ack_notifications_sync(dsn: str, ids: list[int]) -> int:
    normalized_ids = sorted({notification_id for notification_id in ids if notification_id > 0})
    if not normalized_ids:
        return 0

    ensure_notifications_table_sync(dsn)
    with psycopg.connect(dsn, autocommit=True) as conn, conn.cursor() as cursor:
        cursor.execute(
            """
            UPDATE notifications
            SET status = %s,
                read_at = CURRENT_TIMESTAMP
            WHERE status = %s
              AND id = ANY(%s);
            """,
            (STATUS_READ, STATUS_UNREAD, normalized_ids),
        )
        updated = cursor.rowcount
    return max(updated, 0)


async def ensure_notifications_table() -> None:
    await database.query_db(_CREATE_NOTIFICATIONS_TABLE_SQL)


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
    rows = await database.query_db(
        """
        INSERT INTO notifications (kind, source, title, body, link_url, image_url, payload, status)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        RETURNING id, kind, source, title, body, link_url, image_url, payload, status, created_at, read_at;
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
        ),
    )
    if not rows:
        msg = 'Failed to create notification'
        raise RuntimeError(msg)
    return _from_row(rows[0])
