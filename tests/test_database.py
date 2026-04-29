# ruff: noqa: INP001, S101

import asyncio

import pytest

from src.tool import database


def test_advisory_lock_id_is_stable_signed_bigint() -> None:
    lock_id = database.advisory_lock_id('telegram:default:data/session')

    assert lock_id == database.advisory_lock_id('telegram:default:data/session')
    assert lock_id != database.advisory_lock_id('telegram:other:data/session')
    assert -(2**63) <= lock_id <= 2**63 - 1


def test_insert_db_batch_respects_max_bind_params(monkeypatch: pytest.MonkeyPatch) -> None:
    max_bind_params = 7
    expected_call_count = 5
    expected_params_per_call = 6
    calls: list[tuple[str, tuple[str, ...]]] = []

    async def _fake_query_db(sql: str, params: tuple[str, ...] = ()) -> list[dict[str, str]]:
        calls.append((sql, params))
        return []

    monkeypatch.setattr(database, 'query_db', _fake_query_db)
    monkeypatch.setattr(database, '_DEFAULT_DB_MAX_BIND_PARAMS', max_bind_params)
    rows = [(str(idx), str(idx + 1), str(idx + 2)) for idx in range(10)]

    asyncio.run(
        database.insert_db_batch(
            table='demo',
            columns=('a', 'b', 'c'),
            rows=rows,
            chunk_size=10,
        ),
    )

    assert len(calls) == expected_call_count
    assert [len(params) for _sql, params in calls] == [expected_params_per_call] * expected_call_count
    assert all(sql.count('?') <= max_bind_params for sql, _params in calls)


def test_query_db_batch_chunks_by_max_bind_params(monkeypatch: pytest.MonkeyPatch) -> None:
    max_bind_params = 2
    expected_result_count = 5
    expected_chunk_count = 3
    calls: list[int] = []

    async def _fake_query_statements(chunk: list[tuple[str, tuple[str, ...]]]) -> list[list[dict[str, str]]]:
        calls.append(len(chunk))
        return [[] for _ in chunk]

    monkeypatch.setattr(database, '_query_statements', _fake_query_statements)
    statements = [(f'SELECT ? AS c{idx}', (str(idx),)) for idx in range(5)]

    results = asyncio.run(database.query_db_batch(statements, chunk_size=50, max_bind_params=max_bind_params))

    assert len(results) == expected_result_count
    assert len(calls) == expected_chunk_count
    assert calls == [2, 2, 1]


def test_query_db_batch_rejects_statement_with_too_many_bind_params() -> None:
    with pytest.raises(ValueError, match='exceeds max_bind_params'):
        asyncio.run(
            database.query_db_batch(
                [('SELECT ?, ?, ?;', ('1', '2', '3'))],
                max_bind_params=2,
            ),
        )


def test_query_db_multi_rewrites_sql(monkeypatch: pytest.MonkeyPatch) -> None:
    expected_statement_count = 2
    expected_placeholder_count = 2
    captured_statements: list[tuple[str, tuple[str, ...]]] = []

    async def _fake_execute(statements: list[tuple[str, tuple[str, ...]]]) -> list[list[dict[str, str]]]:
        captured_statements.extend(statements)
        return [[{'name': 'id'}], [{'name': 'title'}]]

    monkeypatch.setattr(database, '_execute_prepared_statements', _fake_execute)

    results = asyncio.run(
        database.query_db_multi(
            "INSERT OR IGNORE INTO demo (id, title, note) VALUES (?, ?, '?'); PRAGMA table_info(demo);",
            ('100', 'hello'),
        ),
    )

    assert len(results) == expected_statement_count
    assert len(captured_statements) == expected_statement_count

    first_sql, first_params = captured_statements[0]
    assert first_sql.startswith('INSERT INTO demo')
    assert 'ON CONFLICT DO NOTHING' in first_sql
    assert first_sql.count('%s') == expected_placeholder_count
    assert first_params == ('100', 'hello')

    second_sql, second_params = captured_statements[1]
    assert 'FROM information_schema.columns' in second_sql
    assert second_params == ('demo',)


def test_query_db_batch_propagates_result_count_mismatch(monkeypatch: pytest.MonkeyPatch) -> None:
    mismatched_chunk_size = 2
    statements = [(f'SELECT ? AS c{idx}', (str(idx),)) for idx in range(5)]

    async def _fake_query_statements(
        chunk: list[tuple[str, tuple[str, ...]]],
    ) -> list[list[dict[str, str]]]:
        if len(chunk) == mismatched_chunk_size:
            return [[]]
        return [[] for _ in chunk]

    monkeypatch.setattr(database, '_query_statements', _fake_query_statements)

    with pytest.raises(ValueError, match='batch result mismatch'):
        asyncio.run(database.query_db_batch(statements, chunk_size=50, max_bind_params=2))
