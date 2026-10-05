# ruff: noqa: INP001, S101

import asyncio

import pytest

from src.tool import control_queue

EXPECTED_INTERRUPTED_REQUEST_COUNT = 2
EXPECTED_SCHEDULED_REQUEST_ID = 5


def test_record_scheduled_run_start_inserts_running_row(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[tuple[str, tuple[str, ...]]] = []

    async def _fake_ensure_control_requests_table() -> None:
        return None

    async def _fake_query_db(sql: str, params: tuple[str, ...] = ()) -> list[dict[str, int]]:
        calls.append((sql, params))
        return [{'id': EXPECTED_SCHEDULED_REQUEST_ID}]

    monkeypatch.setattr(control_queue, 'ensure_control_requests_table', _fake_ensure_control_requests_table)
    monkeypatch.setattr(control_queue.database, 'query_db', _fake_query_db)

    request_id = asyncio.run(control_queue.record_scheduled_run_start('bilibili'))

    assert request_id == EXPECTED_SCHEDULED_REQUEST_ID
    assert len(calls) == 1
    sql, params = calls[0]
    assert 'INSERT INTO control_requests' in sql
    assert params == (control_queue.KIND_SCHEDULED_JOB, 'bilibili', control_queue.STATUS_RUNNING)


def test_record_scheduled_run_start_raises_when_insert_returns_nothing(monkeypatch: pytest.MonkeyPatch) -> None:
    async def _fake_ensure_control_requests_table() -> None:
        return None

    async def _fake_query_db(_sql: str, _params: tuple[str, ...] = ()) -> list[dict[str, int]]:
        return []

    monkeypatch.setattr(control_queue, 'ensure_control_requests_table', _fake_ensure_control_requests_table)
    monkeypatch.setattr(control_queue.database, 'query_db', _fake_query_db)

    with pytest.raises(RuntimeError, match='Failed to record scheduled run'):
        asyncio.run(control_queue.record_scheduled_run_start('bilibili'))


def test_list_control_requests_sync_rejects_unknown_status() -> None:
    # Validation fires before any database connection is attempted.
    with pytest.raises(ValueError, match='Unsupported control request status: bogus'):
        control_queue.list_control_requests_sync('postgresql://db.local/fav', statuses=['succeeded', 'bogus'])


def test_interrupt_running_control_requests_marks_rows(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[tuple[str, tuple[str, ...]]] = []

    async def _fake_ensure_control_requests_table() -> None:
        return None

    async def _fake_query_db(sql: str, params: tuple[str, ...] = ()) -> list[dict[str, int]]:
        calls.append((sql, params))
        return [{'id': 1}, {'id': 2}]

    monkeypatch.setattr(control_queue, 'ensure_control_requests_table', _fake_ensure_control_requests_table)
    monkeypatch.setattr(control_queue.database, 'query_db', _fake_query_db)

    count = asyncio.run(control_queue.interrupt_running_control_requests())

    assert count == EXPECTED_INTERRUPTED_REQUEST_COUNT
    assert len(calls) == 1
    sql, params = calls[0]
    assert 'UPDATE control_requests' in sql
    # Every running row, however recent: at startup none of them can still be running.
    assert 'started_at' not in sql
    assert params == (
        control_queue.STATUS_INTERRUPTED,
        control_queue.INTERRUPTED_BY_RESTART_ERROR,
        control_queue.STATUS_RUNNING,
    )
