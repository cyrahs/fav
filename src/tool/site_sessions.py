"""Signed-in site sessions captured on the 登录态 page, stored for the crawlers.

A source that needs a login used to pull its cookies from CookieCloud, which only
works while some desktop browser keeps syncing them. Here the cookies come from a
one-off browser the user signs in to from the web UI (``src/api/browser_login.py``):
once the login is captured, the session lives on the site's servers and the browser
is closed again, so nothing stays resident between logins. The crawler reads the
cookies back from this table and records whether they still worked, which is what
the page shows next to each site.

Cookie values are credentials. Nothing here logs them, and the API and MCP views
only ever report names, counts and expiry times.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Any

from src.tool import database

STATUS_VALID = 'valid'
STATUS_INVALID = 'invalid'

_DDL = """
CREATE TABLE IF NOT EXISTS site_sessions (
    site TEXT PRIMARY KEY,
    cookies TEXT NOT NULL DEFAULT '[]',
    account TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT '',
    message TEXT NOT NULL DEFAULT '',
    expires_at TIMESTAMPTZ NULL,
    signed_in_at TIMESTAMPTZ NULL,
    checked_at TIMESTAMPTZ NULL
);
"""

_MESSAGE_MAX_CHARS = 500


@dataclass(frozen=True)
class StoredCookie:
    name: str
    value: str
    domain: str
    path: str = '/'
    # Unix seconds; None for a cookie that ends with the browser session.
    expires: float | None = None


@dataclass(frozen=True)
class SiteSession:
    site: str
    cookies: tuple[StoredCookie, ...]
    account: str
    status: str
    message: str
    expires_at: datetime | None
    signed_in_at: datetime | None
    checked_at: datetime | None

    def cookie(self, name: str) -> str:
        """The value of one cookie, matched case-insensitively; empty when absent."""
        return cookie_value(self.cookies, name)


def cookie_value(cookies: tuple[StoredCookie, ...] | list[StoredCookie], name: str) -> str:
    wanted = name.lower()
    for cookie in cookies:
        if cookie.name.lower() == wanted and cookie.value:
            return cookie.value
    return ''


def cookies_from_browser(raw: list[dict[str, Any]]) -> tuple[StoredCookie, ...]:
    """Playwright's cookie dicts as stored cookies; ``expires == -1`` means a session cookie."""
    cookies: list[StoredCookie] = []
    for item in raw:
        name = str(item.get('name') or '')
        if not name:
            continue
        expires = item.get('expires')
        cookies.append(
            StoredCookie(
                name=name,
                value=str(item.get('value') or ''),
                domain=str(item.get('domain') or ''),
                path=str(item.get('path') or '/'),
                expires=float(expires) if isinstance(expires, int | float) and expires > 0 else None,
            )
        )
    return tuple(cookies)


def cookie_expiry(cookies: tuple[StoredCookie, ...], name: str) -> datetime | None:
    """When the named cookie expires, or None for a session cookie or a missing one."""
    wanted = name.lower()
    for cookie in cookies:
        if cookie.name.lower() == wanted and cookie.value and cookie.expires is not None:
            return datetime.fromtimestamp(cookie.expires, tz=UTC)
    return None


def _encode_cookies(cookies: tuple[StoredCookie, ...]) -> str:
    return json.dumps(
        [{'name': c.name, 'value': c.value, 'domain': c.domain, 'path': c.path, 'expires': c.expires} for c in cookies],
        ensure_ascii=False,
    )


def _decode_cookies(raw: Any) -> tuple[StoredCookie, ...]:
    try:
        items = json.loads(raw) if isinstance(raw, str) and raw else []
    except ValueError:
        items = []
    cookies: list[StoredCookie] = []
    for item in items if isinstance(items, list) else []:
        if not isinstance(item, dict) or not item.get('name'):
            continue
        expires = item.get('expires')
        cookies.append(
            StoredCookie(
                name=str(item['name']),
                value=str(item.get('value') or ''),
                domain=str(item.get('domain') or ''),
                path=str(item.get('path') or '/'),
                expires=float(expires) if isinstance(expires, int | float) else None,
            )
        )
    return tuple(cookies)


def _row_to_session(row: dict[str, Any]) -> SiteSession:
    return SiteSession(
        site=str(row['site']),
        cookies=_decode_cookies(row.get('cookies')),
        account=str(row.get('account') or ''),
        status=str(row.get('status') or ''),
        message=str(row.get('message') or ''),
        expires_at=row.get('expires_at'),
        signed_in_at=row.get('signed_in_at'),
        checked_at=row.get('checked_at'),
    )


async def ensure_table() -> None:
    await database.query_db(_DDL)


async def load(site: str) -> SiteSession | None:
    await ensure_table()
    rows = await database.query_db('SELECT * FROM site_sessions WHERE site = ?', (site,))
    return _row_to_session(rows[0]) if rows else None


async def load_all() -> dict[str, SiteSession]:
    await ensure_table()
    rows = await database.query_db('SELECT * FROM site_sessions ORDER BY site')
    return {str(row['site']): _row_to_session(row) for row in rows}


async def save_signed_in(site: str, cookies: tuple[StoredCookie, ...], *, account: str, expires_at: datetime | None) -> None:
    """Replace a site's session with a freshly verified login."""
    await ensure_table()
    await database.query_db(
        """
        INSERT INTO site_sessions (site, cookies, account, status, message, expires_at, signed_in_at, checked_at)
        VALUES (?, ?, ?, ?, '', ?, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
        ON CONFLICT (site) DO UPDATE SET
            cookies = EXCLUDED.cookies,
            account = EXCLUDED.account,
            status = EXCLUDED.status,
            message = '',
            expires_at = EXCLUDED.expires_at,
            signed_in_at = EXCLUDED.signed_in_at,
            checked_at = EXCLUDED.checked_at
        """,
        (site, _encode_cookies(cookies), account, STATUS_VALID, expires_at),
    )


async def record_check(site: str, *, ok: bool, message: str = '') -> None:
    """Note whether the stored session still worked, as a crawl or a manual check found it."""
    await ensure_table()
    await database.query_db(
        'UPDATE site_sessions SET status = ?, message = ?, checked_at = CURRENT_TIMESTAMP WHERE site = ?',
        (STATUS_VALID if ok else STATUS_INVALID, message[:_MESSAGE_MAX_CHARS], site),
    )
