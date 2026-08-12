import asyncio
import hashlib
import re
import weakref
from collections.abc import AsyncIterator, Sequence
from contextlib import asynccontextmanager
from functools import lru_cache
from typing import Any

import psycopg
from psycopg.rows import dict_row
from psycopg_pool import AsyncConnectionPool

from src.core import logger
from src.core.env import env

log = logger.get('database')
_DEFAULT_DB_MAX_BIND_PARAMS = 65535
_POOL_MIN_SIZE = 1
_POOL_MAX_SIZE = 8
_POOL_TIMEOUT_SECONDS = 15.0
_SQL_CACHE_SIZE = 1024
_PRAGMA_TABLE_INFO_RE = re.compile(
    r'^\s*PRAGMA\s+table_info\s*\(\s*(?P<table>[A-Za-z_][A-Za-z0-9_]*)\s*\)\s*;?\s*$',
    re.IGNORECASE,
)
_PRAGMA_RE = re.compile(r'^\s*PRAGMA\b', re.IGNORECASE)
_INSERT_OR_IGNORE_RE = re.compile(r'^\s*INSERT\s+OR\s+IGNORE\b', re.IGNORECASE)


# Pools are keyed by event loop so callers that each use their own
# asyncio.run (tests, run.py --trigger) never share connections across loops.
_pools: weakref.WeakKeyDictionary[asyncio.AbstractEventLoop, AsyncConnectionPool] = weakref.WeakKeyDictionary()
_pool_locks: weakref.WeakKeyDictionary[asyncio.AbstractEventLoop, asyncio.Lock] = weakref.WeakKeyDictionary()


async def _get_pool() -> AsyncConnectionPool:
    loop = asyncio.get_running_loop()
    pool = _pools.get(loop)
    if pool is not None:
        return pool
    lock = _pool_locks.setdefault(loop, asyncio.Lock())
    async with lock:
        pool = _pools.get(loop)
        if pool is None:
            pool = AsyncConnectionPool(
                _postgres_dsn(),
                min_size=_POOL_MIN_SIZE,
                max_size=_POOL_MAX_SIZE,
                timeout=_POOL_TIMEOUT_SECONDS,
                kwargs={'autocommit': True, 'row_factory': dict_row},
                check=AsyncConnectionPool.check_connection,
                open=False,
                name='fav-db',
            )
            await pool.open()
            _pools[loop] = pool
    return pool


@asynccontextmanager
async def connection() -> AsyncIterator[psycopg.AsyncConnection[Any]]:
    """Yield a pooled autocommit connection with dict rows.

    Wrap multi-statement work in ``conn.transaction()`` for atomicity; the
    pool resets connection state when the block exits.
    """
    pool = await _get_pool()
    async with pool.connection() as conn:
        yield conn


async def close_pool() -> None:
    """Close the running loop's pool. Call once at process shutdown."""
    loop = asyncio.get_running_loop()
    pool = _pools.pop(loop, None)
    if pool is not None:
        await pool.close()


def advisory_lock_id(name: str) -> int:
    digest = hashlib.blake2b(name.encode(), digest_size=8).digest()
    return int.from_bytes(digest, byteorder='big', signed=True)


@asynccontextmanager
async def advisory_lock(name: str) -> AsyncIterator[bool]:
    # Advisory locks are session-scoped and held across a whole job run, so
    # they get a dedicated connection instead of starving the shared pool.
    lock_id = advisory_lock_id(name)
    dsn = _postgres_dsn()
    conn = await psycopg.AsyncConnection.connect(dsn, autocommit=True, row_factory=dict_row)
    acquired = False
    try:
        async with conn.cursor() as cursor:
            await cursor.execute('SELECT pg_try_advisory_lock(%s) AS acquired;', (lock_id,))
            row = await cursor.fetchone()
            acquired = bool(row and row['acquired'])
        yield acquired
    finally:
        if acquired:
            try:
                async with conn.cursor() as cursor:
                    await cursor.execute('SELECT pg_advisory_unlock(%s);', (lock_id,))
            except Exception as exc:  # noqa: BLE001
                log.warning('Failed to release advisory lock %s: %s', name, exc)
        await conn.close()


def _postgres_dsn() -> str:
    dsn = env.postgres_dsn
    if not dsn:
        msg = 'POSTGRES_DSN is required'
        raise ValueError(msg)
    return dsn


@asynccontextmanager
async def transaction_cursor() -> AsyncIterator[Any]:
    async with connection() as conn, conn.transaction(), conn.cursor() as cursor:
        yield cursor


@lru_cache(maxsize=_SQL_CACHE_SIZE)
def _split_sql_statements(query: str) -> tuple[str, ...]:  # noqa: C901, PLR0912, PLR0915
    statements: list[str] = []
    buf: list[str] = []
    in_single = False
    in_double = False
    in_line_comment = False
    in_block_comment = False
    idx = 0

    while idx < len(query):
        ch = query[idx]
        nxt = query[idx + 1] if idx + 1 < len(query) else ''

        if in_line_comment:
            buf.append(ch)
            if ch == '\n':
                in_line_comment = False
            idx += 1
            continue

        if in_block_comment:
            buf.append(ch)
            if ch == '*' and nxt == '/':
                buf.append(nxt)
                idx += 2
                in_block_comment = False
                continue
            idx += 1
            continue

        if in_single:
            buf.append(ch)
            if ch == "'":
                if nxt == "'":
                    buf.append(nxt)
                    idx += 2
                    continue
                in_single = False
            idx += 1
            continue

        if in_double:
            buf.append(ch)
            if ch == '"':
                if nxt == '"':
                    buf.append(nxt)
                    idx += 2
                    continue
                in_double = False
            idx += 1
            continue

        if ch == '-' and nxt == '-':
            buf.extend((ch, nxt))
            idx += 2
            in_line_comment = True
            continue
        if ch == '/' and nxt == '*':
            buf.extend((ch, nxt))
            idx += 2
            in_block_comment = True
            continue
        if ch == "'":
            buf.append(ch)
            idx += 1
            in_single = True
            continue
        if ch == '"':
            buf.append(ch)
            idx += 1
            in_double = True
            continue

        if ch == ';':
            statement = ''.join(buf).strip()
            if statement:
                statements.append(statement)
            buf = []
            idx += 1
            continue

        buf.append(ch)
        idx += 1

    tail = ''.join(buf).strip()
    if tail:
        statements.append(tail)
    return tuple(statements)


def _rewrite_insert_or_ignore(sql: str) -> str:
    if not _INSERT_OR_IGNORE_RE.match(sql):
        return sql

    normalized = sql.rstrip()
    has_semicolon = normalized.endswith(';')
    if has_semicolon:
        normalized = normalized[:-1]
    normalized = _INSERT_OR_IGNORE_RE.sub('INSERT', normalized, count=1)
    if 'ON CONFLICT' not in normalized.upper():
        normalized = f'{normalized} ON CONFLICT DO NOTHING'
    if has_semicolon:
        normalized = f'{normalized};'
    return normalized


def _convert_qmark_placeholders(sql: str) -> tuple[str, int]:  # noqa: C901, PLR0912, PLR0915
    result: list[str] = []
    placeholder_count = 0
    in_single = False
    in_double = False
    in_line_comment = False
    in_block_comment = False
    idx = 0

    while idx < len(sql):
        ch = sql[idx]
        nxt = sql[idx + 1] if idx + 1 < len(sql) else ''

        if in_line_comment:
            result.append(ch)
            if ch == '\n':
                in_line_comment = False
            idx += 1
            continue

        if in_block_comment:
            result.append(ch)
            if ch == '*' and nxt == '/':
                result.append(nxt)
                idx += 2
                in_block_comment = False
                continue
            idx += 1
            continue

        if in_single:
            result.append(ch)
            if ch == "'":
                if nxt == "'":
                    result.append(nxt)
                    idx += 2
                    continue
                in_single = False
            idx += 1
            continue

        if in_double:
            result.append(ch)
            if ch == '"':
                if nxt == '"':
                    result.append(nxt)
                    idx += 2
                    continue
                in_double = False
            idx += 1
            continue

        if ch == '-' and nxt == '-':
            result.extend((ch, nxt))
            idx += 2
            in_line_comment = True
            continue
        if ch == '/' and nxt == '*':
            result.extend((ch, nxt))
            idx += 2
            in_block_comment = True
            continue
        if ch == "'":
            result.append(ch)
            idx += 1
            in_single = True
            continue
        if ch == '"':
            result.append(ch)
            idx += 1
            in_double = True
            continue

        if ch == '?':
            result.append('%s')
            placeholder_count += 1
            idx += 1
            continue

        result.append(ch)
        idx += 1

    return ''.join(result), placeholder_count


@lru_cache(maxsize=_SQL_CACHE_SIZE)
def _translate_statement(sql: str) -> tuple[str, int]:
    """Translate one SQLite-flavoured statement to PostgreSQL, cached.

    The application issues a fixed set of literal SQL strings, so the
    character-by-character rewrite only needs to run once per statement.
    """
    return _convert_qmark_placeholders(_rewrite_insert_or_ignore(sql))


def _prepare_statement(
    sql: str,
    params: tuple[Any, ...],
    *,
    allow_partial_params: bool = False,
) -> tuple[str, tuple[Any, ...], int]:
    pragma_match = _PRAGMA_TABLE_INFO_RE.match(sql)
    if pragma_match:
        if params and not allow_partial_params:
            msg = f'PRAGMA table_info does not accept bind params: {sql}'
            raise ValueError(msg)
        table = pragma_match.group('table')
        translated_sql = (
            'SELECT column_name AS name '
            'FROM information_schema.columns '
            'WHERE table_schema = ANY(current_schemas(false)) AND table_name = %s '
            'ORDER BY ordinal_position'
        )
        return translated_sql, (table,), 0
    if _PRAGMA_RE.match(sql):
        # Ignore SQLite-specific PRAGMA statements in imported SQL dumps.
        return 'SELECT 1 WHERE FALSE', (), 0

    translated_sql, bind_count = _translate_statement(sql)
    if allow_partial_params:
        if bind_count > len(params):
            msg = f'Not enough bind params: expected {bind_count}, got {len(params)}'
            raise ValueError(msg)
        return translated_sql, params[:bind_count], bind_count

    if bind_count != len(params):
        msg = f'Bind params mismatch: expected {bind_count}, got {len(params)}'
        raise ValueError(msg)
    return translated_sql, params, bind_count


async def _execute_prepared_statements(
    statements: Sequence[tuple[str, tuple[Any, ...]]],
) -> list[list[dict[str, Any]]]:
    results: list[list[dict[str, Any]]] = []

    async with connection() as conn, conn.cursor() as cursor:
        for sql, params in statements:
            await cursor.execute(sql, params)
            rows = await cursor.fetchall() if cursor.description is not None else []
            results.append([dict(row) for row in rows])
    return results


async def _query_statements(
    statements: Sequence[tuple[str, tuple[Any, ...]]],
) -> list[list[dict[str, Any]]]:
    prepared: list[tuple[str, tuple[Any, ...]]] = []
    for sql, params in statements:
        normalized_sql = sql.strip()
        if not normalized_sql:
            continue
        translated_sql, translated_params, _ = _prepare_statement(normalized_sql, params)
        prepared.append((translated_sql, translated_params))
    if not prepared:
        return []
    return await _execute_prepared_statements(prepared)


async def query_db_multi(query: str, params: tuple[Any, ...] = ()) -> list[list[dict[str, Any]]]:
    raw_statements = _split_sql_statements(query)
    if not raw_statements:
        return []

    prepared: list[tuple[str, tuple[Any, ...]]] = []
    params_offset = 0
    for statement in raw_statements:
        translated_sql, translated_params, consumed = _prepare_statement(
            statement,
            params[params_offset:],
            allow_partial_params=True,
        )
        params_offset += consumed
        prepared.append((translated_sql, translated_params))

    if params_offset != len(params):
        msg = f'Unused bind params: consumed {params_offset}, got {len(params)}'
        raise ValueError(msg)
    return await _execute_prepared_statements(prepared)


async def query_db(query: str, params: tuple[Any, ...] = ()) -> list[dict[str, Any]]:
    results = await query_db_multi(query, params)
    if not results:
        return []
    return results[0]


async def query_db_batch(
    statements: Sequence[tuple[str, tuple[Any, ...]]],
    *,
    chunk_size: int = 50,
    max_bind_params: int = _DEFAULT_DB_MAX_BIND_PARAMS,
) -> list[list[dict[str, Any]]]:
    if chunk_size <= 0:
        msg = 'chunk_size must be greater than 0'
        raise ValueError(msg)
    if max_bind_params <= 0:
        msg = 'max_bind_params must be greater than 0'
        raise ValueError(msg)

    all_results: list[list[dict[str, Any]]] = []
    normalized_statements = [(sql.strip().rstrip(';'), params) for sql, params in statements if sql.strip()]
    chunk: list[tuple[str, tuple[Any, ...]]] = []
    chunk_params_count = 0

    for sql, params in normalized_statements:
        statement_params_count = len(params)
        if statement_params_count > max_bind_params:
            msg = f'Statement has {statement_params_count} bind params, exceeds max_bind_params={max_bind_params}'
            raise ValueError(msg)

        should_flush = chunk and (len(chunk) >= chunk_size or chunk_params_count + statement_params_count > max_bind_params)
        if should_flush:
            chunk_results = await _query_statements(chunk)
            if len(chunk_results) != len(chunk):
                msg = f'Database batch result mismatch: expected {len(chunk)} statements, got {len(chunk_results)}'
                raise ValueError(msg)
            all_results.extend(chunk_results)
            chunk = []
            chunk_params_count = 0

        chunk.append((sql, params))
        chunk_params_count += statement_params_count

    if chunk:
        chunk_results = await _query_statements(chunk)
        if len(chunk_results) != len(chunk):
            msg = f'Database batch result mismatch: expected {len(chunk)} statements, got {len(chunk_results)}'
            raise ValueError(msg)
        all_results.extend(chunk_results)
    return all_results


async def query_db_transaction(statements: Sequence[tuple[str, tuple[Any, ...]]]) -> list[list[dict[str, Any]]]:
    prepared: list[tuple[str, tuple[Any, ...]]] = []
    for sql, params in statements:
        normalized_sql = sql.strip().rstrip(';')
        if not normalized_sql:
            continue
        translated_sql, translated_params, _ = _prepare_statement(normalized_sql, params)
        prepared.append((translated_sql, translated_params))
    if not prepared:
        return []

    results: list[list[dict[str, Any]]] = []
    async with connection() as conn, conn.transaction(), conn.cursor() as cursor:
        for sql, params in prepared:
            await cursor.execute(sql, params)
            rows = await cursor.fetchall() if cursor.description is not None else []
            results.append([dict(row) for row in rows])
    return results


async def insert_db_batch(
    *,
    table: str,
    columns: tuple[str, ...],
    rows: Sequence[tuple[str, ...]],
    on_conflict: str | None = None,
    chunk_size: int = 200,
) -> None:
    if not rows:
        return
    if not columns:
        msg = 'columns cannot be empty'
        raise ValueError(msg)
    if chunk_size <= 0:
        msg = 'chunk_size must be greater than 0'
        raise ValueError(msg)
    max_bind_params = _DEFAULT_DB_MAX_BIND_PARAMS
    if max_bind_params <= 0:
        msg = 'default max_bind_params must be greater than 0'
        raise ValueError(msg)

    expected_col_count = len(columns)
    if any(len(row) != expected_col_count for row in rows):
        msg = 'all rows must have the same length as columns'
        raise ValueError(msg)

    max_rows_by_bind_params = max_bind_params // expected_col_count
    if max_rows_by_bind_params <= 0:
        msg = f'columns count ({expected_col_count}) exceeds max_bind_params={max_bind_params}; cannot build insert statement'
        raise ValueError(msg)

    effective_chunk_size = min(chunk_size, max_rows_by_bind_params)

    columns_sql = ', '.join(columns)
    row_placeholder = f'({", ".join(["?"] * expected_col_count)})'
    conflict_sql = f' ON CONFLICT {on_conflict}' if on_conflict else ''

    for index in range(0, len(rows), effective_chunk_size):
        chunk = rows[index : index + effective_chunk_size]
        values_sql = ', '.join(row_placeholder for _ in chunk)
        sql = f'INSERT INTO {table} ({columns_sql}) VALUES {values_sql}{conflict_sql};'  # noqa: S608
        params = tuple(param for row in chunk for param in row)
        await query_db(sql, params)
