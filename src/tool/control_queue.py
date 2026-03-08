from __future__ import annotations

import json
from collections.abc import Mapping
from dataclasses import dataclass
from datetime import datetime
from typing import Any

import psycopg
from psycopg.rows import dict_row

from src.tool import database

STATUS_FAILED = 'failed'
STATUS_PENDING = 'pending'
STATUS_REJECTED = 'rejected'
STATUS_RUNNING = 'running'
STATUS_SUCCEEDED = 'succeeded'
VALID_STATUSES = {STATUS_PENDING, STATUS_RUNNING, STATUS_SUCCEEDED, STATUS_FAILED, STATUS_REJECTED}
VALID_KINDS = {'trigger_job'}

_CREATE_CONTROL_REQUESTS_TABLE_SQL = """
CREATE TABLE IF NOT EXISTS control_requests (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL,
    target TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    started_at TIMESTAMPTZ NULL,
    finished_at TIMESTAMPTZ NULL,
    result TEXT NOT NULL DEFAULT '',
    error TEXT NOT NULL DEFAULT ''
);
"""

_CLAIM_NEXT_REQUEST_SQL = """
WITH next_request AS (
    SELECT id
    FROM control_requests
    WHERE status = ?
    ORDER BY requested_at ASC, id ASC
    FOR UPDATE SKIP LOCKED
    LIMIT 1
)
UPDATE control_requests AS requests
SET status = ?,
    started_at = CURRENT_TIMESTAMP,
    finished_at = NULL,
    result = '',
    error = ''
FROM next_request
WHERE requests.id = next_request.id
RETURNING requests.id, requests.kind, requests.target, requests.payload, requests.status, requests.requested_at,
          requests.started_at, requests.finished_at, requests.result, requests.error;
"""


@dataclass(frozen=True, slots=True)
class ControlRequest:
    request_id: int
    kind: str
    target: str
    payload: str
    status: str
    requested_at: datetime
    started_at: datetime | None = None
    finished_at: datetime | None = None
    result: str = ''
    error: str = ''

    @property
    def payload_json(self) -> dict[str, Any]:
        try:
            payload = json.loads(self.payload)
        except json.JSONDecodeError:
            return {}
        return payload if isinstance(payload, dict) else {}


def _from_row(row: Mapping[str, Any]) -> ControlRequest:
    return ControlRequest(
        request_id=int(row['id']),
        kind=str(row['kind']),
        target=str(row['target']),
        payload=str(row.get('payload') or '{}'),
        status=str(row['status']),
        requested_at=row['requested_at'],
        started_at=row.get('started_at'),
        finished_at=row.get('finished_at'),
        result=str(row.get('result') or ''),
        error=str(row.get('error') or ''),
    )


def ensure_control_requests_table_sync(dsn: str) -> None:
    with psycopg.connect(dsn, autocommit=True) as conn, conn.cursor() as cursor:
        cursor.execute(_CREATE_CONTROL_REQUESTS_TABLE_SQL)


def create_control_request_sync(
    dsn: str,
    *,
    kind: str,
    target: str,
    payload: Mapping[str, Any] | None = None,
) -> ControlRequest:
    if kind not in VALID_KINDS:
        msg = f'Unsupported control request kind: {kind}'
        raise ValueError(msg)

    ensure_control_requests_table_sync(dsn)
    payload_text = json.dumps(payload or {}, ensure_ascii=False, separators=(',', ':'))
    with psycopg.connect(dsn, autocommit=True, row_factory=dict_row) as conn, conn.cursor() as cursor:
        cursor.execute(
            """
            INSERT INTO control_requests (kind, target, payload, status)
            VALUES (%s, %s, %s, %s)
            RETURNING id, kind, target, payload, status, requested_at, started_at, finished_at, result, error;
            """,
            (kind, target, payload_text, STATUS_PENDING),
        )
        row = cursor.fetchone()
    if row is None:
        msg = 'Failed to create control request'
        raise RuntimeError(msg)
    return _from_row(row)


def get_control_request_sync(dsn: str, request_id: int) -> ControlRequest | None:
    ensure_control_requests_table_sync(dsn)
    with psycopg.connect(dsn, autocommit=True, row_factory=dict_row) as conn, conn.cursor() as cursor:
        cursor.execute(
            """
            SELECT id, kind, target, payload, status, requested_at, started_at, finished_at, result, error
            FROM control_requests
            WHERE id = %s;
            """,
            (request_id,),
        )
        row = cursor.fetchone()
    if row is None:
        return None
    return _from_row(row)


async def ensure_control_requests_table() -> None:
    await database.query_db(_CREATE_CONTROL_REQUESTS_TABLE_SQL)


async def claim_next_control_request() -> ControlRequest | None:
    await ensure_control_requests_table()
    rows = await database.query_db(_CLAIM_NEXT_REQUEST_SQL, (STATUS_PENDING, STATUS_RUNNING))
    if not rows:
        return None
    return _from_row(rows[0])


async def update_control_request(
    request_id: int,
    *,
    status: str,
    result: str = '',
    error: str = '',
) -> None:
    if status not in VALID_STATUSES:
        msg = f'Unsupported control request status: {status}'
        raise ValueError(msg)

    await ensure_control_requests_table()
    await database.query_db(
        """
        UPDATE control_requests
        SET status = ?,
            finished_at = CURRENT_TIMESTAMP,
            result = ?,
            error = ?
        WHERE id = ?;
        """,
        (status, result, error, str(request_id)),
    )
