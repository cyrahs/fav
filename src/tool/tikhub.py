"""The TikHub account behind ``services.tikhub``: whether its key works, and what is left on it.

TikHub (https://tikhub.io) bills each lookup against one balance, whichever source
makes it (视频号 videos in WeChat, Zhihu posts linked from WeChat or Telegram), so
the settings page checks the key and shows that balance in one place.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

import httpx

if TYPE_CHECKING:
    from src.core.settings import TikHub

_USER_INFO = '/api/v1/tikhub/user/get_user_info'
# TikHub's answers to a missing, invalid or spent key.
ACCOUNT_STATUSES = frozenset({401, 402, 403})


class TikHubError(RuntimeError):
    def __init__(self, message: str, *, code: str) -> None:
        super().__init__(message)
        self.code = code


@dataclass(frozen=True, slots=True)
class TikHubAccount:
    key_name: str
    balance: float
    free_credit: float


def _number(value: object) -> float:
    try:
        return float(value)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return 0.0


async def check_account(cfg: TikHub, *, transport: httpx.AsyncBaseTransport | None = None) -> TikHubAccount:
    """Ask TikHub who the key belongs to; free, and billed to nothing."""
    if not cfg.token:
        msg = 'Save a TikHub API key first.'
        raise TikHubError(msg, code='tikhub_not_configured')
    try:
        async with httpx.AsyncClient(transport=transport, timeout=30.0) as client:
            response = await client.get(f'{cfg.api_url}{_USER_INFO}', headers={'Authorization': f'Bearer {cfg.token}'})
    except httpx.HTTPError as exc:
        msg = f'TikHub did not answer: {exc.__class__.__name__}: {exc}'
        raise TikHubError(msg, code='tikhub_unreachable') from exc
    if response.status_code in ACCOUNT_STATUSES:
        msg = f'TikHub refused the key (HTTP {response.status_code}).'
        raise TikHubError(msg, code='tikhub_key_refused')
    try:
        payload = response.json()
    except ValueError:
        payload = None
    user = payload.get('user_data') if isinstance(payload, dict) else None
    if response.status_code != httpx.codes.OK or not isinstance(user, dict):
        msg = f'TikHub answered HTTP {response.status_code} without account details.'
        raise TikHubError(msg, code='tikhub_unreachable')
    key = payload.get('api_key_data') if isinstance(payload.get('api_key_data'), dict) else {}
    return TikHubAccount(
        key_name=str(key.get('api_key_name') or ''),
        balance=_number(user.get('balance')),
        free_credit=_number(user.get('free_credit')),
    )
