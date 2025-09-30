from typing import Any

import httpx

from src.core import config, logger

cfg = config.cloudflare
log = logger.get('cloudflare')


async_client = httpx.AsyncClient(
    headers={
        'Authorization': f'Bearer {cfg.api_key}',
    },
    proxy=config.proxy if config.proxy else None,
    timeout=30,
)

client = httpx.Client(
    headers={
        'Authorization': f'Bearer {cfg.api_key}',
    },
    proxy=config.proxy if config.proxy else None,
    timeout=30,
)


async def query_d1(query: str, params: tuple[str, ...] = ()) -> list[dict[str, Any]]:
    url = f'https://api.cloudflare.com/client/v4/accounts/{cfg.account_id}/d1/database/{cfg.d1_id}/query'
    res = await async_client.post(url, json={'sql': query, 'params': params})
    try:
        res.raise_for_status()
    except httpx.HTTPStatusError as e:
        msg = f'Failed to query database: {res.status_code}\n{res.text}'
        raise ValueError(msg) from e

    data = res.json()
    if not data['success']:
        log.exception('Query failed: %s', data)
        msg = f'Query failed: {data}'
        raise ValueError(msg)
    result = data['result'][0]
    if not result['success']:
        log.exception('Query failed: %s', result)
        msg = f'Query failed: {result}'
        raise ValueError(msg)
    return result['results']


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
