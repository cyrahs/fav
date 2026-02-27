# ruff: noqa: INP001, S101

import asyncio

import pytest

from src.tool import cloudflare


def test_insert_d1_batch_respects_max_bind_params(monkeypatch: pytest.MonkeyPatch) -> None:
    max_bind_params = 7
    expected_call_count = 5
    expected_params_per_call = 6
    calls: list[tuple[str, tuple[str, ...]]] = []

    async def _fake_query_d1(sql: str, params: tuple[str, ...] = ()) -> list[dict[str, str]]:
        calls.append((sql, params))
        return []

    monkeypatch.setattr(cloudflare, 'query_d1', _fake_query_d1)
    monkeypatch.setattr(cloudflare, '_DEFAULT_D1_MAX_BIND_PARAMS', max_bind_params)
    rows = [(str(idx), str(idx + 1), str(idx + 2)) for idx in range(10)]

    asyncio.run(
        cloudflare.insert_d1_batch(
            table='demo',
            columns=('a', 'b', 'c'),
            rows=rows,
            chunk_size=10,
        ),
    )

    assert len(calls) == expected_call_count
    assert [len(params) for _sql, params in calls] == [expected_params_per_call] * expected_call_count
    assert all(sql.count('?') <= max_bind_params for sql, _params in calls)


def test_query_d1_batch_chunks_by_max_bind_params(monkeypatch: pytest.MonkeyPatch) -> None:
    max_bind_params = 2
    expected_result_count = 5
    expected_chunk_count = 3
    calls: list[tuple[str, tuple[str, ...], int]] = []

    async def _fake_query_d1_multi(query: str, params: tuple[str, ...] = ()) -> list[list[dict[str, str]]]:
        statements = [statement for statement in query.split(';') if statement.strip()]
        calls.append((query, params, len(statements)))
        return [[] for _ in statements]

    monkeypatch.setattr(cloudflare, 'query_d1_multi', _fake_query_d1_multi)
    statements = [(f'SELECT ? AS c{idx}', (str(idx),)) for idx in range(5)]

    results = asyncio.run(cloudflare.query_d1_batch(statements, chunk_size=50, max_bind_params=max_bind_params))

    assert len(results) == expected_result_count
    assert len(calls) == expected_chunk_count
    assert [count for _query, _params, count in calls] == [2, 2, 1]
    assert [len(params) for _query, params, _count in calls] == [2, 2, 1]


def test_query_d1_batch_rejects_statement_with_too_many_bind_params() -> None:
    with pytest.raises(ValueError, match='exceeds max_bind_params'):
        asyncio.run(
            cloudflare.query_d1_batch(
                [('SELECT ?, ?, ?;', ('1', '2', '3'))],
                max_bind_params=2,
            ),
        )
