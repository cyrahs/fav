"""`src/tool/site_sessions.py`: how stored logins are encoded, decoded and exported (WP2).

The table holds cookies as `json.dumps(..., ensure_ascii=False)` text and `extra` as plain
`json.dumps`; the crawlers read them back through `_decode_cookies` / `_decode_extra` and
hand downloaders a Netscape cookies.txt. Every function here is the real Python one.
"""

from __future__ import annotations

from datetime import UTC
from typing import Any

from src.tool import site_sessions
from src.tool.site_sessions import StoredCookie

_COOKIE_SETS: list[tuple[StoredCookie, ...]] = [
    (),
    (StoredCookie(name='SESSDATA', value='abc%2C123', domain='.bilibili.com', path='/', expires=1767225600.0),),
    (
        StoredCookie(name='auth_token', value='tok', domain='.x.com', path='/', expires=1767225600.5),
        StoredCookie(name='ct0', value='', domain='.x.com', path='/', expires=None),
        StoredCookie(name='lang', value='en', domain='x.com', path='', expires=0.0),
        StoredCookie(name='twid', value='u=1', domain='', path='/', expires=1e10),
        StoredCookie(name='名字', value='值 "quoted" \\ back', domain='.example.cn', path='/a b', expires=1700000000.123456),
    ),
    (StoredCookie(name='PHPSESSID', value='x', domain='.pixiv.net', path='/', expires=1.5e9),),
]

_BROWSER_COOKIES: list[list[dict[str, Any]]] = [
    [],
    [{'name': 'a', 'value': 'b', 'domain': '.example.com', 'path': '/', 'expires': -1}],
    [{'name': 'a', 'value': 'b', 'domain': '.example.com', 'path': '/', 'expires': 1767225600.25}],
    [{'name': 'a', 'value': 'b', 'domain': '.example.com', 'path': '/', 'expires': 1767225600}],
    [{'name': '', 'value': 'skipped'}, {'value': 'no name'}, {'name': 'kept'}],
    [{'name': 'n', 'value': None, 'domain': None, 'path': None, 'expires': None}],
    [{'name': 'n', 'value': 5, 'domain': 'd', 'path': '', 'expires': 0}],
    [{'name': 'n', 'value': 'v', 'expires': True}],
    [{'name': 'n', 'value': 'v', 'expires': '1767225600'}],
]

_STORED_COOKIE_TEXTS: list[Any] = [
    None,
    '',
    'not json',
    '{}',
    '[]',
    '[1, "x", null, {}]',
    '[{"name": "a"}]',
    '[{"name": "a", "value": "v", "domain": ".d", "path": "/p", "expires": 1767225600}]',
    '[{"name": "a", "value": null, "domain": null, "path": "", "expires": 1.5}]',
    '[{"name": 7, "value": 8, "expires": "soon"}]',
    '[{"name": "a", "value": "v", "expires": true}]',
    '[{"name": "", "value": "v"}, {"name": 0}, {"name": "b"}]',
]

_STORED_EXTRA_TEXTS: list[Any] = [
    None,
    '',
    'nope',
    '[]',
    '{}',
    '{"ac_time_value": "abc", "empty": "", "zero": 0, "none": null, "n": 5, "f": false, "t": true, "l": [1]}',
]

_VALUE_NAMES = ('SESSDATA', 'sessdata', 'ct0', 'lang', 'missing')


def _cookie(cookie: StoredCookie) -> dict[str, Any]:
    return {'name': cookie.name, 'value': cookie.value, 'domain': cookie.domain, 'path': cookie.path, 'expires': cookie.expires}


def _codec() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for cookies in _COOKIE_SETS:
        expiry = {}
        for name in ('SESSDATA', 'sessdata', 'auth_token', 'ct0', 'lang', 'twid', 'missing'):
            when = site_sessions.cookie_expiry(cookies, name)
            expiry[name] = when.astimezone(UTC).isoformat() if when else None
        cases.append(
            {
                'function': 'cookies',
                'cookies': [_cookie(cookie) for cookie in cookies],
                'encoded': site_sessions._encode_cookies(cookies),  # noqa: SLF001
                'netscape': site_sessions.to_netscape(cookies),
                'without_values': [_cookie(cookie) for cookie in site_sessions.without_values(cookies)],
                'cookie_value': {name: site_sessions.cookie_value(cookies, name) for name in _VALUE_NAMES},
                'cookie_expiry': expiry,
            },
        )
    cases.extend(
        {
            'function': 'cookies_from_browser',
            'raw': raw,
            'cookies': [_cookie(cookie) for cookie in site_sessions.cookies_from_browser(raw)],
        }
        for raw in _BROWSER_COOKIES
    )
    cases.extend(
        {
            'function': 'decode_cookies',
            'stored': text,
            'cookies': [_cookie(cookie) for cookie in site_sessions._decode_cookies(text)],  # noqa: SLF001
        }
        for text in _STORED_COOKIE_TEXTS
    )
    cases.extend(
        {'function': 'decode_extra', 'stored': text, 'extra': site_sessions._decode_extra(text)}  # noqa: SLF001
        for text in _STORED_EXTRA_TEXTS
    )
    return cases


GOLDENS = {'site_sessions/codec.json': _codec}
