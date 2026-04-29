# ruff: noqa: INP001, S101

import asyncio

import pytest

from src.tool import control_queue

EXPECTED_STALE_REQUEST_COUNT = 2


def test_fail_stale_running_control_requests_marks_rows(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[tuple[str, tuple[str, ...]]] = []

    async def _fake_ensure_control_requests_table() -> None:
        return None

    async def _fake_query_db(sql: str, params: tuple[str, ...] = ()) -> list[dict[str, int]]:
        calls.append((sql, params))
        return [{'id': 1}, {'id': 2}]

    monkeypatch.setattr(control_queue, 'ensure_control_requests_table', _fake_ensure_control_requests_table)
    monkeypatch.setattr(control_queue.database, 'query_db', _fake_query_db)

    count = asyncio.run(control_queue.fail_stale_running_control_requests(older_than_seconds=42))

    assert count == EXPECTED_STALE_REQUEST_COUNT
    assert len(calls) == 1
    sql, params = calls[0]
    assert 'UPDATE control_requests' in sql
    assert params == (
        control_queue.STATUS_FAILED,
        'Stale running control request after 42 seconds',
        control_queue.STATUS_RUNNING,
        '42',
    )
