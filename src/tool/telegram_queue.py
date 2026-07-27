from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from typing import Literal

import psycopg
from psycopg.rows import dict_row

from src.core import config
from src.core.config import TelegramMediaType
from src.tool import database

TelegramQueueSource = Literal['event', 'reconciliation']
TelegramQueueStatus = Literal['pending', 'processing', 'completed', 'discarded']

SOURCE_PRIORITY: dict[TelegramQueueSource, int] = {
    'event': 100,
    'reconciliation': 0,
}
_QUEUE_SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS telegram_media_queue (
    account_name TEXT NOT NULL,
    channel_id BIGINT NOT NULL,
    message_id BIGINT NOT NULL,
    grouped_id BIGINT NULL,
    media_type TEXT NOT NULL,
    title TEXT NOT NULL,
    source TEXT NOT NULL,
    priority INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'pending',
    attempt_count INTEGER NOT NULL DEFAULT 0,
    available_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    owner_token TEXT NULL,
    last_error TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMPTZ NULL,
    PRIMARY KEY (account_name, channel_id, message_id)
);

CREATE INDEX IF NOT EXISTS telegram_media_queue_claim_idx
ON telegram_media_queue (account_name, status, priority DESC, available_at, created_at)
WHERE status = 'pending';
"""


@dataclass(frozen=True, slots=True)
class TelegramMediaJob:
    account_name: str
    channel_id: int
    message_id: int
    grouped_id: int | None
    media_type: TelegramMediaType
    title: str
    source: TelegramQueueSource
    priority: int
    attempt_count: int


def _postgres_dsn() -> str:
    dsn = str(config.database.postgres_dsn).strip()
    if not dsn:
        msg = 'database.postgres_dsn is required'
        raise ValueError(msg)
    return dsn


async def ensure_telegram_media_queue_table() -> None:
    await database.query_db_multi(_QUEUE_SCHEMA_SQL)


async def enqueue_telegram_media_job(  # noqa: PLR0913
    *,
    account_name: str,
    channel_id: int,
    message_id: int,
    grouped_id: int | None,
    media_type: TelegramMediaType,
    title: str,
    source: TelegramQueueSource,
    available_delay_seconds: float = 0,
) -> bool:
    """Persist a media job and return whether a new row was inserted."""
    available_at = datetime.now(tz=UTC) + timedelta(seconds=max(0, available_delay_seconds))
    rows = await database.query_db(
        """
        INSERT INTO telegram_media_queue (
            account_name,
            channel_id,
            message_id,
            grouped_id,
            media_type,
            title,
            source,
            priority,
            available_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (account_name, channel_id, message_id) DO NOTHING
        RETURNING message_id;
        """,
        (
            account_name,
            channel_id,
            message_id,
            grouped_id,
            media_type,
            title,
            source,
            SOURCE_PRIORITY[source],
            available_at,
        ),
    )
    if rows:
        return True

    await database.query_db(
        """
        UPDATE telegram_media_queue
        SET
            grouped_id = COALESCE(telegram_media_queue.grouped_id, ?),
            media_type = ?,
            title = ?,
            source = CASE WHEN priority < ? THEN ? ELSE source END,
            priority = GREATEST(priority, ?),
            available_at = CASE
                WHEN status = 'pending' AND priority <= ? THEN LEAST(available_at, ?)
                ELSE available_at
            END,
            updated_at = CURRENT_TIMESTAMP
        WHERE account_name = ? AND channel_id = ? AND message_id = ?
          AND status IN ('pending', 'processing');
        """,
        (
            grouped_id,
            media_type,
            title,
            SOURCE_PRIORITY[source],
            source,
            SOURCE_PRIORITY[source],
            SOURCE_PRIORITY[source],
            available_at,
            account_name,
            channel_id,
            message_id,
        ),
    )
    return False


async def claim_next_telegram_media_job(account_name: str, owner_token: str) -> TelegramMediaJob | None:
    dsn = _postgres_dsn()
    async with await psycopg.AsyncConnection.connect(dsn, autocommit=False, row_factory=dict_row) as conn:
        async with conn.cursor() as cursor:
            await cursor.execute(
                """
                SELECT
                    account_name,
                    channel_id,
                    message_id,
                    grouped_id,
                    media_type,
                    title,
                    source,
                    priority,
                    attempt_count
                FROM telegram_media_queue
                WHERE account_name = %s
                  AND status = 'pending'
                  AND available_at <= CURRENT_TIMESTAMP
                ORDER BY priority DESC, available_at, created_at, channel_id, message_id
                FOR UPDATE SKIP LOCKED
                LIMIT 1;
                """,
                (account_name,),
            )
            row = await cursor.fetchone()
            if row is None:
                await conn.commit()
                return None
            await cursor.execute(
                """
                UPDATE telegram_media_queue
                SET
                    status = 'processing',
                    owner_token = %s,
                    attempt_count = attempt_count + 1,
                    updated_at = CURRENT_TIMESTAMP
                WHERE account_name = %s AND channel_id = %s AND message_id = %s;
                """,
                (owner_token, row['account_name'], row['channel_id'], row['message_id']),
            )
        await conn.commit()

    return TelegramMediaJob(
        account_name=str(row['account_name']),
        channel_id=int(row['channel_id']),
        message_id=int(row['message_id']),
        grouped_id=int(row['grouped_id']) if row['grouped_id'] is not None else None,
        media_type=row['media_type'],
        title=str(row['title']),
        source=row['source'],
        priority=int(row['priority']),
        attempt_count=int(row['attempt_count']) + 1,
    )


async def reset_processing_telegram_media_jobs(account_name: str) -> int:
    rows = await database.query_db(
        """
        UPDATE telegram_media_queue
        SET
            status = 'pending',
            owner_token = NULL,
            available_at = CURRENT_TIMESTAMP,
            last_error = CASE
                WHEN last_error = '' THEN 'Recovered after account owner restart'
                ELSE last_error
            END,
            updated_at = CURRENT_TIMESTAMP
        WHERE account_name = ? AND status = 'processing'
        RETURNING message_id;
        """,
        (account_name,),
    )
    return len(rows)


async def mark_telegram_media_job_completed(job: TelegramMediaJob, owner_token: str) -> bool:
    return await _finish_job(job, owner_token, status='completed')


async def mark_telegram_media_job_discarded(job: TelegramMediaJob, owner_token: str, *, error: str) -> bool:
    return await _finish_job(job, owner_token, status='discarded', error=error)


async def _finish_job(
    job: TelegramMediaJob,
    owner_token: str,
    *,
    status: Literal['completed', 'discarded'],
    error: str = '',
) -> bool:
    rows = await database.query_db(
        """
        UPDATE telegram_media_queue
        SET
            status = ?,
            owner_token = NULL,
            last_error = ?,
            completed_at = CURRENT_TIMESTAMP,
            updated_at = CURRENT_TIMESTAMP
        WHERE account_name = ? AND channel_id = ? AND message_id = ?
          AND status = 'processing' AND owner_token = ?
        RETURNING message_id;
        """,
        (
            status,
            error[:1000],
            job.account_name,
            job.channel_id,
            job.message_id,
            owner_token,
        ),
    )
    return bool(rows)


async def mark_telegram_media_job_retry(
    job: TelegramMediaJob,
    owner_token: str,
    *,
    error: str,
    delay_seconds: float,
) -> bool:
    rows = await database.query_db(
        """
        UPDATE telegram_media_queue
        SET
            status = 'pending',
            owner_token = NULL,
            last_error = ?,
            available_at = CURRENT_TIMESTAMP + (? * INTERVAL '1 second'),
            updated_at = CURRENT_TIMESTAMP
        WHERE account_name = ? AND channel_id = ? AND message_id = ?
          AND status = 'processing' AND owner_token = ?
        RETURNING message_id;
        """,
        (
            error[:1000],
            max(0, delay_seconds),
            job.account_name,
            job.channel_id,
            job.message_id,
            owner_token,
        ),
    )
    return bool(rows)


def telegram_media_retry_delay(attempt_count: int) -> float:
    return min(30.0 * (2 ** max(0, attempt_count - 1)), 1800.0)
