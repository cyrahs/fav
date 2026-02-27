from collections.abc import Sequence
from typing import Any

import httpx

from src.core import config, logger

cfg = config.cloudflare
log = logger.get('cloudflare')


async_client = httpx.AsyncClient(
    headers={
        'Authorization': f'Bearer {cfg.api_key}',
    },
    proxy=config.proxy or None,
    timeout=30,
)

client = httpx.Client(
    headers={
        'Authorization': f'Bearer {cfg.api_key}',
    },
    proxy=config.proxy or None,
    timeout=30,
)


def _d1_query_url() -> str:
    return f'https://api.cloudflare.com/client/v4/accounts/{cfg.account_id}/d1/database/{cfg.d1_id}/query'


def _parse_d1_entries(response: httpx.Response) -> list[dict[str, Any]]:
    data = response.json()
    if not data['success']:
        log.exception('Query failed: %s', data)
        msg = f'Query failed: {data}'
        raise ValueError(msg)

    result = data.get('result')
    if not isinstance(result, list):
        msg = f'Unexpected D1 result payload: {data}'
        raise TypeError(msg)
    return result


def _extract_statement_results(entries: list[dict[str, Any]]) -> list[list[dict[str, Any]]]:
    results: list[list[dict[str, Any]]] = []
    for entry in entries:
        if not entry.get('success'):
            log.exception('Query failed: %s', entry)
            msg = f'Query failed: {entry}'
            raise ValueError(msg)
        statement_rows = entry.get('results')
        if isinstance(statement_rows, list):
            results.append(statement_rows)
        else:
            results.append([])
    return results


async def query_d1_multi(query: str, params: tuple[str, ...] = ()) -> list[list[dict[str, Any]]]:
    url = _d1_query_url()
    res = await async_client.post(url, json={'sql': query, 'params': params})
    try:
        res.raise_for_status()
    except httpx.HTTPStatusError as e:
        msg = f'Failed to query database: {res.status_code}\n{res.text}'
        raise ValueError(msg) from e

    entries = _parse_d1_entries(res)
    return _extract_statement_results(entries)


async def query_d1(query: str, params: tuple[str, ...] = ()) -> list[dict[str, Any]]:
    results = await query_d1_multi(query, params)
    if not results:
        return []
    return results[0]


async def query_d1_batch(
    statements: Sequence[tuple[str, tuple[str, ...]]],
    *,
    chunk_size: int = 50,
) -> list[list[dict[str, Any]]]:
    if chunk_size <= 0:
        msg = 'chunk_size must be greater than 0'
        raise ValueError(msg)

    all_results: list[list[dict[str, Any]]] = []
    normalized_statements = [(sql.strip().rstrip(';'), params) for sql, params in statements if sql.strip()]
    for index in range(0, len(normalized_statements), chunk_size):
        chunk = normalized_statements[index : index + chunk_size]
        sql_batch = ';'.join(sql for sql, _params in chunk)
        params_batch = tuple(param for _sql, params in chunk for param in params)
        chunk_results = await query_d1_multi(f'{sql_batch};', params_batch)
        if len(chunk_results) != len(chunk):
            msg = f'D1 batch result mismatch: expected {len(chunk)} statements, got {len(chunk_results)}'
            raise ValueError(msg)
        all_results.extend(chunk_results)
    return all_results


async def insert_d1_batch(
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

    expected_col_count = len(columns)
    if any(len(row) != expected_col_count for row in rows):
        msg = 'all rows must have the same length as columns'
        raise ValueError(msg)

    columns_sql = ', '.join(columns)
    row_placeholder = f'({", ".join(["?"] * expected_col_count)})'
    conflict_sql = f' ON CONFLICT {on_conflict}' if on_conflict else ''

    for index in range(0, len(rows), chunk_size):
        chunk = rows[index : index + chunk_size]
        values_sql = ', '.join(row_placeholder for _ in chunk)
        sql = f'INSERT INTO {table} ({columns_sql}) VALUES {values_sql}{conflict_sql};'  # noqa: S608
        params = tuple(param for row in chunk for param in row)
        await query_d1(sql, params)


async def get_kv(kv_id: str, key: str | int) -> httpx.Response:
    url = f'https://api.cloudflare.com/client/v4/accounts/{cfg.account_id}/storage/kv/namespaces/{kv_id}/values/{key}'
    res = await async_client.get(url)
    try:
        res.raise_for_status()
    except httpx.HTTPStatusError as e:
        log.exception('Failed to get key: %s', res.text)
        msg = f'Failed to get key: {res.status_code}\n{res.text}'
        raise ValueError(msg) from e
    return res


def sync_get_kv(kv_id: str, key: str | int) -> httpx.Response:
    url = f'https://api.cloudflare.com/client/v4/accounts/{cfg.account_id}/storage/kv/namespaces/{kv_id}/values/{key}'
    res = client.get(url)
    try:
        res.raise_for_status()
    except httpx.HTTPStatusError as e:
        log.exception('Failed to get key: %s', res.text)
        msg = f'Failed to get key: {res.status_code}\n{res.text}'
        raise ValueError(msg) from e
    return res
