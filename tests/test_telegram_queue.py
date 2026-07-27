# ruff: noqa: INP001, S101, ANN001, ANN002, ANN003, ANN204, PLR2004

import asyncio

import src.tool.telegram_queue as queue_module
from src.tool.telegram_queue import (
    TelegramMediaJob,
    claim_next_telegram_media_job,
    enqueue_telegram_media_job,
    ensure_telegram_media_queue_table,
    mark_telegram_media_job_discarded,
    mark_telegram_media_job_retry,
    reset_processing_telegram_media_jobs,
    telegram_media_retry_delay,
)


def _job() -> TelegramMediaJob:
    return TelegramMediaJob(
        account_name='default',
        channel_id=123,
        message_id=456,
        grouped_id=99,
        media_type='image',
        title='Album-1',
        source='event',
        priority=100,
        attempt_count=2,
    )


def test_queue_schema_creates_durable_table_and_claim_index(monkeypatch) -> None:
    statements: list[str] = []

    async def _fake_query_db_multi(sql: str) -> list[dict[str, object]]:
        statements.append(sql)
        return []

    monkeypatch.setattr(queue_module.database, 'query_db_multi', _fake_query_db_multi)
    asyncio.run(ensure_telegram_media_queue_table())

    assert 'CREATE TABLE IF NOT EXISTS telegram_media_queue' in statements[0]
    assert 'PRIMARY KEY (account_name, channel_id, message_id)' in statements[0]
    assert "WHERE status = 'pending'" in statements[0]


def test_enqueue_is_idempotent_and_event_has_higher_priority(monkeypatch) -> None:
    queries: list[tuple[str, tuple[object, ...]]] = []
    results = [[{'message_id': 456}], [], []]

    async def _fake_query_db(sql: str, params: tuple[object, ...]) -> list[dict[str, object]]:
        queries.append((sql, params))
        return results.pop(0)

    monkeypatch.setattr(queue_module.database, 'query_db', _fake_query_db)

    inserted = asyncio.run(
        enqueue_telegram_media_job(
            account_name='default',
            channel_id=123,
            message_id=456,
            grouped_id=None,
            media_type='image',
            title='Image',
            source='event',
        ),
    )
    duplicate = asyncio.run(
        enqueue_telegram_media_job(
            account_name='default',
            channel_id=123,
            message_id=456,
            grouped_id=None,
            media_type='image',
            title='Image',
            source='reconciliation',
        ),
    )

    assert inserted is True
    assert duplicate is False
    assert queries[0][1][7] == 100
    assert queries[1][1][7] == 0
    assert "status IN ('pending', 'processing')" in queries[2][0]


class _FakeCursor:
    def __init__(self, row: dict[str, object]) -> None:
        self.row = row
        self.executed: list[tuple[str, tuple[object, ...]]] = []

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_args) -> None:
        return None

    async def execute(self, sql: str, params: tuple[object, ...]) -> None:
        self.executed.append((sql, params))

    async def fetchone(self) -> dict[str, object] | None:
        row = self.row
        self.row = {}
        return row or None


class _FakeConnection:
    def __init__(self, row: dict[str, object]) -> None:
        self.cursor_instance = _FakeCursor(row)
        self.commits = 0

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_args) -> None:
        return None

    def cursor(self) -> _FakeCursor:
        return self.cursor_instance

    async def commit(self) -> None:
        self.commits += 1


def test_claim_uses_priority_skip_locked_and_sets_owner_token(monkeypatch) -> None:
    connection = _FakeConnection(
        {
            'account_name': 'default',
            'channel_id': 123,
            'message_id': 456,
            'grouped_id': 99,
            'media_type': 'image',
            'title': 'Album-1',
            'source': 'event',
            'priority': 100,
            'attempt_count': 1,
        },
    )

    async def _connect(*_args, **_kwargs) -> _FakeConnection:
        return connection

    monkeypatch.setattr(queue_module, '_postgres_dsn', lambda: 'postgresql://test')
    monkeypatch.setattr(queue_module.psycopg.AsyncConnection, 'connect', _connect)

    job = asyncio.run(claim_next_telegram_media_job('default', 'owner-token'))

    assert job == _job()
    select_sql, select_params = connection.cursor_instance.executed[0]
    update_sql, update_params = connection.cursor_instance.executed[1]
    assert 'ORDER BY priority DESC' in select_sql
    assert 'FOR UPDATE SKIP LOCKED' in select_sql
    assert select_params == ('default',)
    assert "status = 'processing'" in update_sql
    assert update_params[0] == 'owner-token'
    assert connection.commits == 1


def test_processing_jobs_are_recovered_for_new_account_owner(monkeypatch) -> None:
    captured: list[tuple[str, tuple[object, ...]]] = []

    async def _fake_query_db(sql: str, params: tuple[object, ...]) -> list[dict[str, object]]:
        captured.append((sql, params))
        return [{'message_id': 1}, {'message_id': 2}]

    monkeypatch.setattr(queue_module.database, 'query_db', _fake_query_db)
    count = asyncio.run(reset_processing_telegram_media_jobs('default'))

    assert count == 2
    assert "status = 'pending'" in captured[0][0]
    assert "status = 'processing'" in captured[0][0]
    assert captured[0][1] == ('default',)


def test_retry_and_discard_are_scoped_to_owner_token(monkeypatch) -> None:
    captured: list[tuple[str, tuple[object, ...]]] = []

    async def _fake_query_db(sql: str, params: tuple[object, ...]) -> list[dict[str, object]]:
        captured.append((sql, params))
        return [{'message_id': 456}]

    monkeypatch.setattr(queue_module.database, 'query_db', _fake_query_db)

    retried = asyncio.run(mark_telegram_media_job_retry(_job(), 'owner', error='network', delay_seconds=30))
    discarded = asyncio.run(mark_telegram_media_job_discarded(_job(), 'owner', error='deleted'))

    assert retried is True
    assert discarded is True
    assert 'owner_token = ?' in captured[0][0]
    assert captured[0][1][-1] == 'owner'
    assert "status = 'discarded'" not in captured[1][0]
    assert captured[1][1][0] == 'discarded'
    assert captured[1][1][-1] == 'owner'


def test_retry_backoff_is_capped_at_thirty_minutes() -> None:
    assert telegram_media_retry_delay(1) == 30
    assert telegram_media_retry_delay(2) == 60
    assert telegram_media_retry_delay(20) == 1800
