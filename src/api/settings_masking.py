"""Masking for secrets stored in ``app_settings``.

The UI must be able to show a section without leaking the secrets inside it, and
must be able to save that same section back without wiping them. Reads replace
secret values with a short masked preview; writes treat a masked (or absent)
value as "keep what is already stored".

The rules are spelled out per section rather than derived from a path DSL: there
are only a handful of secrets, and the per-account ones have to be matched by
account name so that reordering or inserting accounts in the UI cannot shuffle
credentials between them.
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


def keep_secret(payload: dict[str, Any], key: str, stored_value: str) -> None:
    value = payload.get(key)
    if key not in payload or value is None or is_masked(value):
        payload[key] = stored_value


def _mask_scalar(payload: dict[str, Any], key: str) -> None:
    value = payload.get(key)
    if isinstance(value, str):
        payload[key] = mask_value(value)


def _accounts(payload: dict[str, Any]) -> list[dict[str, Any]]:
    accounts = payload.get('accounts')
    if not isinstance(accounts, list):
        return []
    return [account for account in accounts if isinstance(account, dict)]


def _cookiecloud(payload: dict[str, Any]) -> dict[str, Any] | None:
    """The nested CookieCloud block of an account or of a whole section."""
    nested = payload.get('cookiecloud')
    return nested if isinstance(nested, dict) else None


def mask_section(section: str, payload: dict[str, Any]) -> dict[str, Any]:
    masked = dict(payload)
    if section == 'notifications.telegram':
        _mask_scalar(masked, 'bot_token')
    elif section == 'web.telegram':
        masked['accounts'] = [{**account} for account in _accounts(masked)]
        for account in _accounts(masked):
            _mask_scalar(account, 'api_hash')
    elif section == 'web.bilibili':
        masked['accounts'] = [{**account} for account in _accounts(masked)]
        for account in _accounts(masked):
            nested = _cookiecloud(account)
            if nested is not None:
                account['cookiecloud'] = {**nested}
                _mask_scalar(account['cookiecloud'], 'password')
    elif section == 'web.twitter':
        nested = _cookiecloud(masked)
        if nested is not None:
            masked['cookiecloud'] = {**nested}
            _mask_scalar(masked['cookiecloud'], 'password')
    elif section == 'web.rednote':
        # The proxy URL is the secret here: it points at the user's own line and may
        # carry credentials.
        _mask_scalar(masked, 'proxy')
    return masked


def unmask_section(section: str, payload: dict[str, Any], stored: dict[str, Any]) -> dict[str, Any]:
    """Restore secrets the client sent back masked (or omitted entirely)."""
    merged = dict(payload)
    if section == 'notifications.telegram':
        keep_secret(merged, 'bot_token', str(stored.get('bot_token') or ''))
    elif section == 'web.telegram':
        # Matched by account name: the UI may reorder, insert, or drop accounts
        # between the GET and the PUT.
        stored_hashes = {str(account.get('name') or ''): str(account.get('api_hash') or '') for account in _accounts(stored)}
        merged['accounts'] = [{**account} for account in _accounts(merged)]
        for account in _accounts(merged):
            keep_secret(account, 'api_hash', stored_hashes.get(str(account.get('name') or ''), ''))
    elif section == 'web.bilibili':
        stored_passwords = {
            str(account.get('name') or ''): str((_cookiecloud(account) or {}).get('password') or '') for account in _accounts(stored)
        }
        merged['accounts'] = [{**account} for account in _accounts(merged)]
        for account in _accounts(merged):
            nested = _cookiecloud(account)
            if nested is None:
                continue
            account['cookiecloud'] = {**nested}
            keep_secret(account['cookiecloud'], 'password', stored_passwords.get(str(account.get('name') or ''), ''))
    elif section == 'web.twitter':
        nested = _cookiecloud(merged)
        if nested is not None:
            merged['cookiecloud'] = {**nested}
            keep_secret(merged['cookiecloud'], 'password', str((_cookiecloud(stored) or {}).get('password') or ''))
    elif section == 'web.rednote':
        keep_secret(merged, 'proxy', str(stored.get('proxy') or ''))
    return merged
