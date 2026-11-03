"""Keeps a Bilibili login captured on the 登录态 page alive without a browser.

A browser signed in to bilibili.com does two things on its own that a stored
cookie jar does not:

- it fetches a fresh ``bili_ticket`` (valid for three days) from
  ``GenWebTicket``; requests without a current one are the first to be met with
  risk control (code -352), which is what RSSHub's Bilibili routes run into;
- when ``/cookie/info`` says so, it trades the refresh token kept in
  localStorage (``ac_time_value``) for a new ``SESSDATA``/``bili_jct`` pair and
  a new refresh token, then confirms the trade, which retires the old pair.

This module does both from the stored cookies, so fav is the one writer of the
session and everything else (the RSSHub proxy) only reads it. The protocol is the
one bilibili-API-collect documents and ``bilibili_api`` implements
(``Credential.refresh``); it is done here with httpx so that every cookie the
refresh sets is kept, not only the four a Credential holds.

Cookie values and the refresh token are credentials: nothing here logs them.
"""

from __future__ import annotations

import binascii
import hashlib
import hmac
import re
import time
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Self

import httpx
from Crypto.Cipher import PKCS1_OAEP
from Crypto.Hash import SHA256
from Crypto.PublicKey import RSA

from src.core import logger
from src.tool.site_sessions import StoredCookie, cookie_value

if TYPE_CHECKING:
    from collections.abc import Awaitable, Callable
    from http.cookiejar import CookieJar

log = logger.get('bilibili')

REFRESH_TOKEN_KEY = 'refresh_token'  # noqa: S105 - a key name, not a token
# The localStorage entry the login page keeps the refresh token in.
REFRESH_TOKEN_STORAGE_KEY = 'ac_time_value'  # noqa: S105 - a key name, not a token

_TICKET_URL = 'https://api.bilibili.com/bapis/bilibili.api.ticket.v1.Ticket/GenWebTicket'
_TICKET_KEY = b'XgwSnGZ1p'
_TICKET_KEY_ID = 'ec02'
# Renew once less than this is left of the three days a ticket lasts, so a crawl
# that runs at least daily always leaves one with a day or more to go.
_TICKET_RENEW_MARGIN_SECONDS = 2 * 86400
_COOKIE_INFO_URL = 'https://passport.bilibili.com/x/passport-login/web/cookie/info'
_CORRESPOND_URL = 'https://www.bilibili.com/correspond/1/{path}'
_REFRESH_URL = 'https://passport.bilibili.com/x/passport-login/web/cookie/refresh'
_CONFIRM_URL = 'https://passport.bilibili.com/x/passport-login/web/confirm/refresh'
_REFRESH_CSRF_PATTERN = re.compile(r'<div id="1-name">(.+?)</div>')
_CORRESPOND_KEY = """-----BEGIN PUBLIC KEY-----
MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDLgd2OAkcGVtoE3ThUREbio0Eg
Uc/prcajMKXvkCKFCWhJYJcLkcM2DKKcSeFpD/j6Boy538YXnR6VhcuUJOhH2x71
nzPjfdTcqMz7djHum0qSZA0AyCBDABUqCrfNgCiJ00Ra7GmRj+YCK1NJEuewlb40
JNrRuoEUXpabUzGB8QIDAQAB
-----END PUBLIC KEY-----"""
_USER_AGENT = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36'
_COOKIE_DOMAIN = '.bilibili.com'
_HTTP_OK = 200


class RenewalError(RuntimeError):
    """Bilibili did not go along with a renewal step; the session is left as it was."""


@dataclass
class Renewal:
    cookies: tuple[StoredCookie, ...]
    refresh_token: str
    # What changed, for the log: 'bili_ticket', 'SESSDATA'.
    renewed: list[str] = field(default_factory=list)


def ticket_hexsign(timestamp: int) -> str:
    return hmac.new(_TICKET_KEY, f'ts{timestamp}'.encode(), hashlib.sha256).hexdigest()


def correspond_path(timestamp_ms: int) -> str:
    cipher = PKCS1_OAEP.new(RSA.import_key(_CORRESPOND_KEY), SHA256)
    return binascii.hexlify(cipher.encrypt(f'refresh_{timestamp_ms}'.encode())).decode()


def ticket_expires_at(cookies: tuple[StoredCookie, ...]) -> float | None:
    """When the stored bili_ticket runs out, from its companion cookie or its own expiry."""
    if not cookie_value(cookies, 'bili_ticket'):
        return None
    raw = cookie_value(cookies, 'bili_ticket_expires')
    if raw.isdigit():
        return float(raw)
    for cookie in cookies:
        if cookie.name == 'bili_ticket' and cookie.expires:
            return cookie.expires
    return None


def ticket_due(cookies: tuple[StoredCookie, ...], *, now: float) -> bool:
    expires = ticket_expires_at(cookies)
    return expires is None or expires - now < _TICKET_RENEW_MARGIN_SECONDS


def merge_cookies(cookies: tuple[StoredCookie, ...], updates: list[StoredCookie]) -> tuple[StoredCookie, ...]:
    """The jar with each update replacing the cookie of the same name; an expired update deletes it."""
    now = time.time()
    by_name = {c.name: c for c in updates}
    merged = [by_name.pop(c.name, c) for c in cookies]
    merged.extend(by_name.values())
    return tuple(c for c in merged if c.value and (c.expires is None or c.expires > now))


def _from_jar(jar: CookieJar) -> list[StoredCookie]:
    return [
        StoredCookie(
            name=c.name,
            value=c.value or '',
            domain=c.domain,
            path=c.path or '/',
            expires=float(c.expires) if c.expires else None,
        )
        for c in jar
    ]


class _Client:
    """httpx with the stored jar sent as one Cookie header and nothing remembered between calls."""

    def __init__(self, transport: httpx.AsyncBaseTransport | None) -> None:
        self._client = httpx.AsyncClient(
            headers={'User-Agent': _USER_AGENT, 'Referer': 'https://www.bilibili.com/', 'Origin': 'https://www.bilibili.com'},
            transport=transport,
            timeout=30,
        )

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self._client.aclose()

    async def request(self, method: str, url: str, cookies: tuple[StoredCookie, ...], **kwargs: object) -> httpx.Response:
        header = '; '.join(f'{c.name}={c.value}' for c in cookies if c.value)
        headers = {'Cookie': header} if header else {}
        try:
            response = await self._client.request(method, url, headers=headers, **kwargs)  # type: ignore[arg-type]
        finally:
            self._client.cookies.clear()
        return response


def _json(response: httpx.Response, step: str) -> dict:
    try:
        payload = response.json()
    except ValueError as exc:
        msg = f'{step}: HTTP {response.status_code}, not JSON'
        raise RenewalError(msg) from exc
    if not isinstance(payload, dict) or payload.get('code') != 0:
        code = payload.get('code') if isinstance(payload, dict) else None
        msg = f'{step}: HTTP {response.status_code}, code {code}'
        raise RenewalError(msg)
    data = payload.get('data')
    return data if isinstance(data, dict) else {}


async def _new_ticket(client: _Client, cookies: tuple[StoredCookie, ...], *, now: float) -> list[StoredCookie]:
    ts = int(now)
    params = {'key_id': _TICKET_KEY_ID, 'hexsign': ticket_hexsign(ts), 'context[ts]': str(ts), 'csrf': cookie_value(cookies, 'bili_jct')}
    data = _json(await client.request('POST', _TICKET_URL, cookies, params=params), 'GenWebTicket')
    ticket = str(data.get('ticket') or '')
    if not ticket:
        msg = 'GenWebTicket: no ticket in the answer'
        raise RenewalError(msg)
    created = int(data.get('created_at') or ts)
    expires = float(created + int(data.get('ttl') or 259200))
    return [
        StoredCookie(name='bili_ticket', value=ticket, domain=_COOKIE_DOMAIN, expires=expires),
        StoredCookie(name='bili_ticket_expires', value=str(int(expires)), domain=_COOKIE_DOMAIN, expires=expires),
    ]


async def _refresh_due(client: _Client, cookies: tuple[StoredCookie, ...]) -> bool:
    params = {'csrf': cookie_value(cookies, 'bili_jct')}
    data = _json(await client.request('GET', _COOKIE_INFO_URL, cookies, params=params), 'cookie/info')
    return bool(data.get('refresh'))


async def _refresh(client: _Client, cookies: tuple[StoredCookie, ...], refresh_token: str) -> tuple[tuple[StoredCookie, ...], str]:
    response = await client.request('GET', _CORRESPOND_URL.format(path=correspond_path(int(time.time() * 1000))), cookies)
    match = _REFRESH_CSRF_PATTERN.search(response.text) if response.status_code == _HTTP_OK else None
    if match is None:
        msg = f'correspond: HTTP {response.status_code}, no refresh_csrf'
        raise RenewalError(msg)
    data = {'csrf': cookie_value(cookies, 'bili_jct'), 'refresh_csrf': match.group(1), 'source': 'main_web', 'refresh_token': refresh_token}
    response = await client.request('POST', _REFRESH_URL, cookies, data=data)
    answer = _json(response, 'cookie/refresh')
    new_token = str(answer.get('refresh_token') or '')
    updates = _from_jar(response.cookies.jar)
    if not new_token or not any(c.name == 'SESSDATA' and c.value for c in updates):
        msg = 'cookie/refresh: no new SESSDATA or refresh token in the answer'
        raise RenewalError(msg)
    return merge_cookies(cookies, updates), new_token


async def _confirm(client: _Client, cookies: tuple[StoredCookie, ...], old_token: str) -> None:
    data = {'csrf': cookie_value(cookies, 'bili_jct'), 'refresh_token': old_token}
    _json(await client.request('POST', _CONFIRM_URL, cookies, data=data), 'confirm/refresh')


async def renew_ticket(
    cookies: tuple[StoredCookie, ...],
    *,
    transport: httpx.AsyncBaseTransport | None = None,
    now: float | None = None,
) -> tuple[StoredCookie, ...] | None:
    """A jar with a fresh bili_ticket, or None when the stored one has long enough to go."""
    moment = time.time() if now is None else now
    if not ticket_due(cookies, now=moment):
        return None
    async with _Client(transport) as client:
        return merge_cookies(cookies, await _new_ticket(client, cookies, now=moment))


async def refresh_session(
    cookies: tuple[StoredCookie, ...],
    refresh_token: str,
    *,
    save: Callable[[Renewal], Awaitable[None]],
    transport: httpx.AsyncBaseTransport | None = None,
) -> Renewal | None:
    """Trade the refresh token for a new SESSDATA when Bilibili asks for it; None when it does not.

    ``save(renewal)`` is awaited between the refresh and its confirmation: once
    the refresh answers, only the new cookies and token are any good, so they are
    stored before the old pair is retired.
    """
    async with _Client(transport) as client:
        if not await _refresh_due(client, cookies):
            return None
        new_cookies, new_token = await _refresh(client, cookies, refresh_token)
        renewal = Renewal(cookies=new_cookies, refresh_token=new_token, renewed=['SESSDATA'])
        await save(renewal)
        try:
            await _confirm(client, new_cookies, refresh_token)
        except (RenewalError, httpx.HTTPError) as exc:
            # The new pair already works; an unconfirmed refresh only leaves the old one alive longer.
            log.warning('Bilibili accepted the refresh but not its confirmation: %s', exc)
        return renewal
