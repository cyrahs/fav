"""Durable download queue for media received through WeChat iLink bots.

Mirrors ``telegram_queue``: a message is persisted the moment the poller sees
it, so the ``getupdates`` cursor can advance without losing anything, and one
serial worker per account drains the rows. Completed rows stay for dedupe.
"""

from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from typing import Literal

from src.core.settings import WeChatMediaType
from src.tool import database

WeChatQueueStatus = Literal['pending', 'processing', 'completed', 'discarded']

_QUEUE_SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS wechat_media_queue (
    account_name TEXT NOT NULL,
    message_id BIGINT NOT NULL,
    item_index INTEGER NOT NULL,
    media_type TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    file_name TEXT NOT NULL DEFAULT '',
    from_user_id TEXT NOT NULL DEFAULT '',
    context_token TEXT NOT NULL DEFAULT '',
    encrypt_query_param TEXT NOT NULL,
    aes_key TEXT NOT NULL DEFAULT '',
    media_size BIGINT NOT NULL DEFAULT 0,
    media_md5 TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending',
    attempt_count INTEGER NOT NULL DEFAULT 0,
    available_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    owner_token TEXT NULL,
    last_error TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMPTZ NULL,
    PRIMARY KEY (account_name, message_id, item_index)
);

CREATE INDEX IF NOT EXISTS wechat_media_queue_claim_idx
ON wechat_media_queue (account_name, status, available_at, created_at)
WHERE status = 'pending';
"""


@dataclass(frozen=True, slots=True)
class WeChatMediaJob:
    account_name: str
    message_id: int
    item_index: int
    media_type: WeChatMediaType
    title: str
    file_name: str
    from_user_id: str
    context_token: str
    encrypt_query_param: str
    aes_key: str
    media_size: int
    media_md5: str
    attempt_count: int


async def ensure_wechat_media_queue_table() -> None:
    await database.query_db_multi(_QUEUE_SCHEMA_SQL)


async def enqueue_wechat_media_job(  # noqa: PLR0913
    *,
    account_name: str,
    message_id: int,
    item_index: int,
    media_type: WeChatMediaType,
    title: str,
    file_name: str,
    from_user_id: str,
    context_token: str,
    encrypt_query_param: str,
    aes_key: str,
    media_size: int,
    media_md5: str,
) -> bool:
    """Persist a media job; returns whether a new row was inserted.

    The poller may see a message twice (a cursor that was not saved before a
    crash), so a duplicate is silently ignored rather than treated as an error.
    """
    rows = await database.query_db(
        """
        INSERT INTO wechat_media_queue (
            account_name,
            message_id,
            item_index,
            media_type,
            title,
            file_name,
            from_user_id,
            context_token,
            encrypt_query_param,
            aes_key,
            media_size,
            media_md5
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (account_name, message_id, item_index) DO NOTHING
        RETURNING message_id;
        """,
        (
            account_name,
            message_id,
            item_index,
            media_type,
            title,
            file_name,
            from_user_id,
            context_token,
            encrypt_query_param,
            aes_key,
            media_size,
            media_md5,
        ),
    )
    return bool(rows)


async def claim_next_wechat_media_job(account_name: str, owner_token: str) -> WeChatMediaJob | None:
    async with database.connection() as conn, conn.transaction(), conn.cursor() as cursor:
        await cursor.execute(
            """
            SELECT
                account_name,
                message_id,
                item_index,
                media_type,
                title,
                file_name,
                from_user_id,
                context_token,
                encrypt_query_param,
                aes_key,
                media_size,
                media_md5,
                attempt_count
            FROM wechat_media_queue
            WHERE account_name = %s
              AND status = 'pending'
              AND available_at <= CURRENT_TIMESTAMP
            ORDER BY available_at, created_at, message_id, item_index
            FOR UPDATE SKIP LOCKED
            LIMIT 1;
            """,
            (account_name,),
        )
        row = await cursor.fetchone()
        if row is None:
            return None
        await cursor.execute(
            """
            UPDATE wechat_media_queue
            SET
                status = 'processing',
                owner_token = %s,
                attempt_count = attempt_count + 1,
                updated_at = CURRENT_TIMESTAMP
            WHERE account_name = %s AND message_id = %s AND item_index = %s;
            """,
            (owner_token, row['account_name'], row['message_id'], row['item_index']),
        )

    return WeChatMediaJob(
        account_name=str(row['account_name']),
        message_id=int(row['message_id']),
        item_index=int(row['item_index']),
        media_type=row['media_type'],
        title=str(row['title'] or ''),
        file_name=str(row['file_name'] or ''),
        from_user_id=str(row['from_user_id'] or ''),
        context_token=str(row['context_token'] or ''),
        encrypt_query_param=str(row['encrypt_query_param']),
        aes_key=str(row['aes_key'] or ''),
        media_size=int(row['media_size'] or 0),
        media_md5=str(row['media_md5'] or ''),
        attempt_count=int(row['attempt_count']) + 1,
    )


async def reset_processing_wechat_media_jobs(account_name: str) -> int:
    rows = await database.query_db(
        """
        UPDATE wechat_media_queue
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


async def release_pending_wechat_media_jobs(account_name: str) -> int:
    """Make every backed-off row claimable now; what 立即运行 means for this source."""
    rows = await database.query_db(
        """
        UPDATE wechat_media_queue
        SET available_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP
        WHERE account_name = ? AND status = 'pending' AND available_at > CURRENT_TIMESTAMP
        RETURNING message_id;
        """,
        (account_name,),
    )
    return len(rows)


async def count_pending_wechat_media_jobs(account_name: str) -> int:
    rows = await database.query_db(
        "SELECT COUNT(*) AS pending FROM wechat_media_queue WHERE account_name = ? AND status IN ('pending', 'processing');",
        (account_name,),
    )
    return int(rows[0]['pending']) if rows else 0


async def mark_wechat_media_job_completed(job: WeChatMediaJob, owner_token: str) -> bool:
    return await _finish_job(job, owner_token, status='completed')


async def mark_wechat_media_job_discarded(job: WeChatMediaJob, owner_token: str, *, error: str) -> bool:
    return await _finish_job(job, owner_token, status='discarded', error=error)


async def _finish_job(
    job: WeChatMediaJob,
    owner_token: str,
    *,
    status: Literal['completed', 'discarded'],
    error: str = '',
) -> bool:
    rows = await database.query_db(
        """
        UPDATE wechat_media_queue
        SET
            status = ?,
            owner_token = NULL,
            last_error = ?,
            completed_at = CURRENT_TIMESTAMP,
            updated_at = CURRENT_TIMESTAMP
        WHERE account_name = ? AND message_id = ? AND item_index = ?
          AND status = 'processing' AND owner_token = ?
        RETURNING message_id;
        """,
        (status, error[:1000], job.account_name, job.message_id, job.item_index, owner_token),
    )
    return bool(rows)


async def mark_wechat_media_job_retry(
    job: WeChatMediaJob,
    owner_token: str,
    *,
    error: str,
    delay_seconds: float,
) -> bool:
    rows = await database.query_db(
        """
        UPDATE wechat_media_queue
        SET
            status = 'pending',
            owner_token = NULL,
            last_error = ?,
            available_at = CURRENT_TIMESTAMP + (? * INTERVAL '1 second'),
            updated_at = CURRENT_TIMESTAMP
        WHERE account_name = ? AND message_id = ? AND item_index = ?
          AND status = 'processing' AND owner_token = ?
        RETURNING message_id;
        """,
        (error[:1000], max(0, delay_seconds), job.account_name, job.message_id, job.item_index, owner_token),
    )
    return bool(rows)


def wechat_media_retry_delay(attempt_count: int) -> float:
    return min(30.0 * (2 ** max(0, attempt_count - 1)), 1800.0)


def wechat_available_at(delay_seconds: float) -> datetime:
    return datetime.now(tz=UTC) + timedelta(seconds=max(0, delay_seconds))
