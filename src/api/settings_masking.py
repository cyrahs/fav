"""Masking for secrets stored in ``app_settings``.

The UI must be able to show a section without leaking the secrets inside it, and
must be able to save that same section back without wiping them. Reads replace
secret values with a short masked preview; writes treat a masked (or absent)
value as "keep what is already stored".

The rules are spelled out per section rather than derived from a path DSL: there
are only three secrets, and telegram's has to be matched by account name so that
reordering or inserting accounts in the UI cannot shuffle credentials between
them.
"""

from __future__ import annotations

from typing import Any

MASK_SUFFIX = '••••'
_VISIBLE_PREFIX_CHARS = 4


def mask_value(value: str) -> str:
    if not value:
        return ''
    if len(value) <= _VISIBLE_PREFIX_CHARS:
        return MASK_SUFFIX
    return f'{value[:_VISIBLE_PREFIX_CHARS]}{MASK_SUFFIX}'


def is_masked(value: Any) -> bool:
    return isinstance(value, str) and value.endswith(MASK_SUFFIX)


def _keep_secret(payload: dict[str, Any], key: str, stored_value: str) -> None:
    value = payload.get(key)
    if key not in payload or value is None or is_masked(value):
        payload[key] = stored_value


def _mask_scalar(payload: dict[str, Any], key: str) -> None:
    value = payload.get(key)
    if isinstance(value, str):
        payload[key] = mask_value(value)


def _telegram_accounts(payload: dict[str, Any]) -> list[dict[str, Any]]:
    accounts = payload.get('accounts')
    if not isinstance(accounts, list):
        return []
    return [account for account in accounts if isinstance(account, dict)]


def mask_section(section: str, payload: dict[str, Any]) -> dict[str, Any]:
    masked = dict(payload)
    if section == 'cookiecloud':
        _mask_scalar(masked, 'password')
    elif section == 'nasuchan':
        _mask_scalar(masked, 'token')
    elif section == 'web.telegram':
        masked['accounts'] = [{**account} for account in _telegram_accounts(masked)]
        for account in _telegram_accounts(masked):
            _mask_scalar(account, 'api_hash')
    return masked


def unmask_section(section: str, payload: dict[str, Any], stored: dict[str, Any]) -> dict[str, Any]:
    """Restore secrets the client sent back masked (or omitted entirely)."""
    merged = dict(payload)
    if section == 'cookiecloud':
        _keep_secret(merged, 'password', str(stored.get('password') or ''))
    elif section == 'nasuchan':
        _keep_secret(merged, 'token', str(stored.get('token') or ''))
    elif section == 'web.telegram':
        # Matched by account name: the UI may reorder, insert, or drop accounts
        # between the GET and the PUT.
        stored_hashes = {str(account.get('name') or ''): str(account.get('api_hash') or '') for account in _telegram_accounts(stored)}
        merged['accounts'] = [{**account} for account in _telegram_accounts(merged)]
        for account in _telegram_accounts(merged):
            _keep_secret(account, 'api_hash', stored_hashes.get(str(account.get('name') or ''), ''))
    return merged
