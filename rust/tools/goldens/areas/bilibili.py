"""`src/web/bilibili.py` and `src/web/bilibili_session.py` (WP7).

Three groups, every value produced by the real Python code:

* the five bilibili-api-python calls the crawl makes (watch-later list and clear,
  favourite list page, video info and detail), run through the library itself over an
  `httpx.MockTransport` session: the requests it sends, and what it makes of answers;
* `bilibili_session`'s signature, ticket bookkeeping, cookie merging, `Set-Cookie`
  parsing (httpx + `http.cookiejar`) and the renewal requests;
* `bilibili.py`'s own helpers: `verify_session`, notification texts, the yt-dlp command.

Nothing here leaves the process: every HTTP answer comes from a mock transport.
"""

from __future__ import annotations

import asyncio
import http.cookiejar
import subprocess
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest import mock
from urllib.parse import parse_qsl

import bilibili_api as api
import httpx
from bilibili_api.utils import network
from src.core import settings
from src.tool.site_sessions import StoredCookie
from src.web import bilibili, bilibili_session

NOW = 1_800_000_000.0

_CREDENTIAL = {'sessdata': 'sess,data', 'bili_jct': 'jct', 'buvid3': 'b3', 'dedeuserid': '42'}
# Floats from here up are recorded as their repr; the Rust test turns them back into floats.
_BIG_FLOAT = 1e20


def _exc(exc: BaseException) -> dict[str, str]:
    return {'error_class': type(exc).__name__, 'error': str(exc)}


def _request(request: httpx.Request) -> dict[str, Any]:
    return {
        'method': request.method,
        'url': str(request.url),
        'user_agent': request.headers.get('user-agent'),
        'referer': request.headers.get('referer'),
        'origin': request.headers.get('origin'),
        'cookie': request.headers.get('cookie'),
        'content_type': request.headers.get('content-type'),
        'body': request.content.decode(),
    }


def _run_api(call: Any, answers: list[httpx.Response]) -> dict[str, Any]:
    """Run one bilibili_api coroutine factory against scripted answers."""
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return answers[min(len(seen), len(answers)) - 1]

    async def main() -> Any:
        network.get_client()
        api.set_session(httpx.AsyncClient(transport=httpx.MockTransport(handler)))
        return await call(api.Credential(**_CREDENTIAL))

    try:
        result: dict[str, Any] = {'result': asyncio.run(main())}
    except Exception as exc:  # noqa: BLE001 - the exception is the golden
        result = _exc(exc)
    result['requests'] = [_request(request) for request in seen]
    return result


def _video(bvid: str) -> Any:
    return lambda credential: api.video.Video(bvid=bvid, credential=credential)


_OK_LIST = httpx.Response(200, json={'code': 0, 'message': '0', 'ttl': 1, 'data': {'count': 1, 'list': [{'bvid': 'BV1xx411c7mD'}]}})


def _api_requests() -> list[dict[str, Any]]:
    """The request each of the five calls makes."""
    calls = {
        'toview_list': lambda c: api.user.get_toview_list(credential=c),
        'toview_clear': lambda c: api.user.clear_toview_list(credential=c),
        'fav_content_page_1': lambda c: api.favorite_list.FavoriteList(media_id=123, credential=c).get_content(page=1),
        'fav_content_page_3': lambda c: api.favorite_list.FavoriteList(media_id=9876543210, credential=c).get_content(page=3),
        'video_info': lambda c: _video('BV1xx411c7mD')(c).get_info(),
        'video_detail': lambda c: _video('BV1GJ411x7h7')(c).get_detail(),
    }
    cases = [{'call': name, **_run_api(call, [_OK_LIST])} for name, call in calls.items()]
    # A SESSDATA without a '%' is URL-quoted before it is sent; one with a '%' is not.
    for sessdata in ('a b/c,d', 'already%2Cquoted', '值'):
        credential = {**_CREDENTIAL, 'sessdata': sessdata}

        async def main(credential: dict[str, str] = credential) -> list[httpx.Request]:
            seen: list[httpx.Request] = []
            network.get_client()
            api.set_session(httpx.AsyncClient(transport=httpx.MockTransport(lambda r: seen.append(r) or _OK_LIST)))  # noqa: B023 - the closure runs within this iteration
            await api.user.get_toview_list(credential=api.Credential(**credential))
            return seen

        cases.append({'call': 'toview_list', 'sessdata': sessdata, 'requests': [_request(r) for r in asyncio.run(main())]})
    return cases


def _answer(status: int, body: bytes, headers: list[tuple[str, str]] | None = None) -> httpx.Response:
    return httpx.Response(status, content=body, headers=headers or [('Content-Type', 'application/json')])


_ANSWERS: list[tuple[int, bytes]] = [
    (200, b'{"code": 0, "message": "0", "data": {"bvid": "BV1xx411c7mD", "is_upower_exclusive": false}}'),
    (200, b'{"code": 0, "data": null, "result": {"from": "result"}}'),
    (200, b'{"code": 0}'),
    (200, b'{"OK": 1, "list": []}'),
    (200, b'{"OK": 0}'),
    (200, b'{"data": 1}'),
    (200, b'{"code": -404, "message": "\xe5\x95\xa5\xe9\x83\xbd\xe6\x9c\xa8\xe6\x9c\x89", "ttl": 1}'),
    (200, b'{"code": -400, "msg": "bad request", "message": "ignored"}'),
    (200, b'{"code": -101}'),
    (200, b'{"code": "0", "message": "string code"}'),
    (200, b'{"code": 62002, "message": "\\u7a3f\\u4ef6\\u4e0d\\u53ef\\u89c1", "data": {"nested": [1, 2.5, true, null, "s\'q"]}}'),
    (412, b'<html><body>blocked</body></html>'),
    (200, b''),
    (200, b'"just a string"'),
    (200, b'[1, 2]'),
    (200, b'\xff{"code": 0}'),
    (200, b'{"code": 0, "data": "\xe4\xb8"}'),
    (200, b'{"code": 0, "data": "\xe4\x00"}'),
    (200, b'{"code": 0, "data": 1} trailing'),
]


def _api_answers() -> list[dict[str, Any]]:
    """What `Video.get_info()` returns or raises for each answer."""
    cases: list[dict[str, Any]] = []
    for status, body in _ANSWERS:
        outcome = _run_api(lambda c: _video('BV1xx411c7mD')(c).get_info(), [_answer(status, body)])
        cases.append({'status': status, 'body': body.decode('latin-1'), 'attempts': len(outcome.pop('requests')), **outcome})
    # A zero Content-Length answer is no data at all, whatever the body.
    outcome = _run_api(lambda c: _video('BV1xx411c7mD')(c).get_info(), [_answer(200, b'', [('Content-Length', '0')])])
    cases.append({'status': 200, 'body': '', 'content_length_zero': True, 'attempts': len(outcome.pop('requests')), **outcome})
    # -403 is retried as a stale wbi key: three attempts, then WbiRetryTimesExceedException.
    for answers in (
        [_answer(200, b'{"code": -403, "message": "denied"}')],
        [_answer(200, b'{"code": -403, "message": "denied"}'), _answer(200, b'{"code": 0, "data": {"second": true}}')],
    ):
        outcome = _run_api(lambda c: _video('BV1xx411c7mD')(c).get_info(), answers)
        cases.append({'script': [r.content.decode() for r in answers], 'attempts': len(outcome.pop('requests')), **outcome})
    return cases


_BVIDS = [
    'BV1xx411c7mD',
    'BV1GJ411x7h7',
    'BV17x411w7KC',
    'BV1Q541167Qg',
    'BV1mK4y1C7Bz',
    'BV1wM4m1S7oo',
    'BV1TEST1',
    'BVEXIST',
    'av170001',
    'bv1xx411c7mD',
    'BV1xx411c7m',
    'BV1xx411c7mDD',
    'BV1xx411c7m!',
    'BV1Ox411c7mD',
    'BV10x411c7mD',
    'BVzzzzzzzzzz',
    'BV1xx411c7mD\n',
]


def _bvids() -> list[dict[str, Any]]:
    cases = []
    for bvid in _BVIDS:
        try:
            cases.append({'bvid': bvid, 'aid': api.video.Video(bvid=bvid).get_aid()})
        except Exception as exc:  # noqa: BLE001
            cases.append({'bvid': bvid, **_exc(exc)})
    return cases


# ---------- bilibili_session ----------


def _cookie(name: str, value: str, *, domain: str = '.bilibili.com', path: str = '/', expires: float | None = None) -> StoredCookie:
    return StoredCookie(name=name, value=value, domain=domain, path=path, expires=expires)


def _dump_cookies(cookies: Any) -> list[dict[str, Any]]:
    return [{'name': c.name, 'value': c.value, 'domain': c.domain, 'path': c.path, 'expires': c.expires} for c in cookies]


def _hexsigns() -> list[dict[str, Any]]:
    return [{'ts': ts, 'hexsign': bilibili_session.ticket_hexsign(ts)} for ts in (0, 1, 1_700_000_000, 1_800_000_000, 4_102_444_800)]


_TICKET_JARS: list[tuple[StoredCookie, ...]] = [
    (),
    (_cookie('SESSDATA', 's'),),
    (_cookie('bili_ticket', 't'),),
    (_cookie('bili_ticket', 't'), _cookie('bili_ticket_expires', str(int(NOW + 2.5 * 86400)))),
    (_cookie('bili_ticket', 't'), _cookie('bili_ticket_expires', str(int(NOW + 2 * 86400)))),
    (_cookie('bili_ticket', 't'), _cookie('bili_ticket_expires', str(int(NOW + 2 * 86400 + 1)))),
    (_cookie('bili_ticket', 't'), _cookie('bili_ticket_expires', str(int(NOW + 3600)))),
    (_cookie('bili_ticket', 't'), _cookie('bili_ticket_expires', ' 1800200000')),
    (_cookie('bili_ticket', 't'), _cookie('bili_ticket_expires', '१८००९००००००')),
    (_cookie('bili_ticket', 't', expires=NOW + 3 * 86400), _cookie('bili_ticket_expires', 'soon')),
    (_cookie('bili_ticket', 't', expires=NOW + 86400),),
    (_cookie('BILI_TICKET', 't', expires=NOW + 3 * 86400),),
    (_cookie('bili_ticket', '', expires=NOW + 3 * 86400), _cookie('bili_ticket', 't2')),
    (_cookie('bili_ticket', 't', expires=0.0),),
    (_cookie('bili_ticket_expires', str(int(NOW + 3 * 86400))),),
]


def _tickets() -> list[dict[str, Any]]:
    return [
        {
            'cookies': _dump_cookies(jar),
            'expires_at': bilibili_session.ticket_expires_at(jar),
            'due': bilibili_session.ticket_due(jar, now=NOW),
        }
        for jar in _TICKET_JARS
    ]


_MERGES: list[tuple[tuple[StoredCookie, ...], list[StoredCookie]]] = [
    (
        (_cookie('SESSDATA', 'old-sess'), _cookie('bili_jct', 'old-jct'), _cookie('buvid3', 'b3'), _cookie('DedeUserID', '42')),
        [_cookie('SESSDATA', 'n'), _cookie('buvid3', 'x', expires=1.0), _cookie('b_nut', '9')],
    ),
    ((_cookie('a', '1'), _cookie('b', '')), [_cookie('c', '3', expires=NOW + 10), _cookie('c', '4', expires=NOW + 20)]),
    ((_cookie('a', '1', expires=NOW - 1), _cookie('A', '2')), [_cookie('a', '', expires=NOW + 10)]),
    ((), []),
    ((_cookie('x', '1'),), [_cookie('X', '2')]),
    ((_cookie('x', '1'), _cookie('x', '2')), [_cookie('x', '3')]),
]


def _merges() -> list[dict[str, Any]]:
    with mock.patch.object(bilibili_session.time, 'time', return_value=NOW):
        return [
            {
                'cookies': _dump_cookies(jar),
                'updates': _dump_cookies(updates),
                'merged': _dump_cookies(bilibili_session.merge_cookies(jar, updates)),
            }
            for jar, updates in _MERGES
        ]


_SET_COOKIES: list[tuple[str, list[str]]] = [
    (
        'https://passport.bilibili.com/x/passport-login/web/cookie/refresh',
        [
            'SESSDATA=new-sess; Path=/; Domain=bilibili.com; Expires=Wed, 01 Jan 2031 00:00:00 GMT; HttpOnly',
            'bili_jct=new-jct; Path=/; Domain=bilibili.com; Expires=Wed, 01 Jan 2031 00:00:00 GMT',
            'sid=new-sid; Path=/; Domain=bilibili.com',
            'DedeUserID=42; Path=/; Domain=.bilibili.com; Max-Age=15552000',
        ],
    ),
    (
        'https://passport.bilibili.com/x/passport-login/web/cookie/refresh',
        [
            'nodomain=1',
            'nopath=2; Domain=passport.bilibili.com',
            'other=3; Domain=example.com',
            'nodot=4; Domain=com',
            'gone=5; Max-Age=0',
            'past=6; Expires=Thu, 01 Jan 1970 00:00:01 GMT',
            'badage=7; Max-Age=soon',
            'baddate=8; Expires=not a date',
            '$dollar=9',
            'flag',
            '=novalue',
            ' spaced = value with spaces ; path = /x ',
            'quoted="a b"; Domain=".bilibili.com"',
            'twice=1; Domain=a.bilibili.com; Domain=b.bilibili.com',
            'emptypath=10; Path=',
            'emptydomain=11; Domain=',
            'novaluedomain=12; Domain',
            'ver=13; Version=1',
            'badver=14; Version=x',
            'port=15; Port="443,80"',
            'later=16; Expires=Wed, 01 Jan 2031 00:00:00 GMT; Max-Age=60',
            'agefirst=17; Max-Age=60; Expires=Thu, 01 Jan 1970 00:00:01 GMT',
        ],
    ),
    (
        'https://passport.bilibili.com/x/passport-login/web/cookie/refresh',
        [
            'rfc850=1; Expires=Wednesday, 01-Jan-31 00:00:00 GMT',
            'short=2; Expires=01-Jan-2031 00:00:00',
            'numeric=3; Expires=01 Jan 2031 08:00:00 +0800',
            'zone=4; Expires=01 Jan 2031 00:00:00 UTC',
            'unknownzone=5; Expires=01 Jan 2031 00:00:00 PST',
            'ampm=6; Expires=01 Jan 2031 00:00:00 AM',
            'noclock=7; Expires=1 Jan 2031',
            'twodigit=8; Expires=Thu, 01-Jan-70 00:00:01 GMT',
            'monthnum=9; Expires=01-01-2031 00:00:00',
            'parens=10; Expires=01 Jan 2031 00:00:00 GMT (UTC)',
            'lower=11; Expires=wed, 01 jan 2031 00:00:00 gmt',
            'bigyear=12; Expires=01 Jan 99999 00:00:00 GMT',
            'quoteddate=13; Expires="Wed, 01 Jan 2031 00:00:00 GMT"',
        ],
    ),
    ('https://www.bilibili.com/correspond/1/abc', ['deep=1', 'up=2; Path=/correspond']),
    ('http://localhost/x', ['local=1; Domain=localhost', 'local2=2; Domain=.localhost', 'plain=3']),
    ('https://passport.bilibili.com/', ['a=1; Domain=.BiliBili.COM', 'a=2; Domain=bilibili.com', 'b=1', 'b=2']),
    # int(None) is a TypeError, which make_cookies swallows by dropping every Set-Cookie of the answer.
    ('https://passport.bilibili.com/x', ['keep=1', 'broken=2; Max-Age', 'after=3']),
    ('https://passport.bilibili.com:8443/x', ['p1=1; Port="8443"', 'p2=2; Port=80, 443', 'p3=3; Port', 'p4=4; Port=x', 'p5=5; Port=443']),
    ('https://passport.bilibili.com/a%7eb/c d', ['esc=1', 'pathesc=2; Path=/%c3%bc/%7e x[y]']),
    (
        'https://passport.bilibili.com/x',
        [
            'v=1; Version="1"; Path=/',
            'w="quoted value"; secure; HttpOnly; SameSite=Lax',
            'age=1; Max-Age=+60',
            'neg=1; Max-Age=-1',
            'u=1; Max-Age=1_0',
        ],
    ),
]


def _set_cookies() -> list[dict[str, Any]]:
    cases = []
    with mock.patch.object(http.cookiejar.time, 'time', return_value=NOW):
        for url, headers in _SET_COOKIES:
            response = httpx.Response(200, headers=[('Set-Cookie', value) for value in headers], request=httpx.Request('POST', url))
            jar = bilibili_session._from_jar(response.cookies.jar)  # noqa: SLF001 - the real parser
            cases.append({'url': url, 'now': NOW, 'set_cookie': headers, 'cookies': _dump_cookies(jar)})
    return cases


def _jar(**extra: str) -> tuple[StoredCookie, ...]:
    values = {'SESSDATA': 'old-sess', 'bili_jct': 'old-jct', 'buvid3': 'b3', 'DedeUserID': '42', **extra}
    return tuple(_cookie(k, v) for k, v in values.items())


def _renew_ticket_cases() -> list[dict[str, Any]]:
    scripts: list[tuple[tuple[StoredCookie, ...], httpx.Response]] = [
        (_jar(), httpx.Response(200, json={'code': 0, 'data': {'ticket': 'fresh', 'created_at': int(NOW) - 100, 'ttl': 3600}})),
        (_jar(), httpx.Response(200, json={'code': 0, 'data': {'ticket': 'fresh'}})),
        (_jar(), httpx.Response(200, json={'code': 0, 'data': {'ticket': 'fresh', 'created_at': 0, 'ttl': 0}})),
        (_jar(), httpx.Response(200, json={'code': 0, 'data': {'ticket': 'fresh', 'created_at': '1800000000', 'ttl': '10'}})),
        (_jar(), httpx.Response(200, json={'code': 0, 'data': {'ticket': ''}})),
        (_jar(), httpx.Response(200, json={'code': 0, 'data': []})),
        (_jar(), httpx.Response(200, json={'code': 0})),
        (_jar(), httpx.Response(200, json={'code': -400, 'message': 'bad'})),
        (_jar(), httpx.Response(200, json=[1])),
        (_jar(), httpx.Response(503, text='<html>busy</html>')),
        (_jar(), httpx.Response(200, json={'code': '0'})),
        (_jar(), httpx.Response(200, json={'code': 0, 'data': {'ticket': 5, 'created_at': 1.5, 'ttl': 2.5}})),
        (_jar(bili_ticket='old', bili_ticket_expires=str(int(NOW + 3 * 86400))), httpx.Response(500)),
        (_jar(bili_ticket='old', bili_ticket_expires=str(int(NOW + 600))), httpx.Response(200, json={'code': 0, 'data': {'ticket': 'n'}})),
        (
            (_cookie('SESSDATA', 's'), _cookie('empty', ''), _cookie('buvid3', 'b')),
            httpx.Response(200, json={'code': 0, 'data': {'ticket': 'n', 'ttl': 259200}}),
        ),
    ]
    cases = []
    for cookies, answer in scripts:
        seen: list[httpx.Request] = []

        def handler(request: httpx.Request, answer: httpx.Response = answer) -> httpx.Response:
            seen.append(request)  # noqa: B023 - the closure runs within this iteration
            return answer

        try:
            with mock.patch.object(bilibili_session.time, 'time', return_value=NOW):
                renewed = asyncio.run(bilibili_session.renew_ticket(cookies, transport=httpx.MockTransport(handler), now=NOW))
            outcome: dict[str, Any] = {'renewed': None if renewed is None else _dump_cookies(renewed)}
        except Exception as exc:  # noqa: BLE001
            outcome = _exc(exc)
        cases.append(
            {'cookies': _dump_cookies(cookies), 'answer': _response_dump(answer), 'requests': [_request(r) for r in seen], **outcome}
        )
    return cases


def _response_dump(response: httpx.Response) -> dict[str, Any]:
    return {
        'status': response.status_code,
        'headers': [[k, v] for k, v in response.headers.multi_items() if k.lower() in {'set-cookie', 'content-type'}],
        'body': response.content.decode(),
    }


def _refresh_answers(
    *,
    info: httpx.Response,
    correspond: httpx.Response | None = None,
    refresh: httpx.Response | None = None,
    confirm: httpx.Response | None = None,
) -> dict[str, httpx.Response]:
    return {
        'info': info,
        'correspond': correspond or httpx.Response(200, text='<html><div id="1-name">rcsrf</div></html>'),
        'refresh': refresh
        or httpx.Response(
            200,
            json={'code': 0, 'data': {'refresh_token': 'rt-new'}},
            headers=[
                ('Set-Cookie', 'SESSDATA=new-sess; Path=/; Domain=bilibili.com; Expires=Wed, 01 Jan 2031 00:00:00 GMT; HttpOnly'),
                ('Set-Cookie', 'bili_jct=new-jct; Path=/; Domain=bilibili.com; Expires=Wed, 01 Jan 2031 00:00:00 GMT'),
                ('Set-Cookie', 'sid=new-sid; Path=/; Domain=bilibili.com'),
            ],
        ),
        'confirm': confirm or httpx.Response(200, json={'code': 0}),
    }


_REFRESH_OK = httpx.Response(200, json={'code': 0, 'data': {'refresh': True}})


def _refresh_cases() -> list[dict[str, Any]]:
    scripts: list[tuple[str, dict[str, httpx.Response]]] = [
        ('not due', _refresh_answers(info=httpx.Response(200, json={'code': 0, 'data': {'refresh': False}}))),
        ('due, refreshed and confirmed', _refresh_answers(info=_REFRESH_OK)),
        ('info refused', _refresh_answers(info=httpx.Response(200, json={'code': -101, 'message': '账号未登录'}))),
        ('info not json', _refresh_answers(info=httpx.Response(200, text='nope'))),
        ('info refresh is a string', _refresh_answers(info=httpx.Response(200, json={'code': 0, 'data': {'refresh': 'no'}}))),
        ('correspond 404', _refresh_answers(info=_REFRESH_OK, correspond=httpx.Response(404, text='<div id="1-name">x</div>'))),
        ('correspond without csrf', _refresh_answers(info=_REFRESH_OK, correspond=httpx.Response(200, text='<div id="2-name">x</div>'))),
        (
            'correspond csrf spans lines',
            _refresh_answers(info=_REFRESH_OK, correspond=httpx.Response(200, text='<div id="1-name">a\nb</div><div id="1-name">c</div>')),
        ),
        ('refresh refused', _refresh_answers(info=_REFRESH_OK, refresh=httpx.Response(200, json={'code': -111, 'message': 'csrf'}))),
        (
            'refresh without a token',
            _refresh_answers(
                info=_REFRESH_OK,
                refresh=httpx.Response(200, json={'code': 0, 'data': {}}, headers=[('Set-Cookie', 'SESSDATA=n; Domain=bilibili.com')]),
            ),
        ),
        (
            'refresh without SESSDATA',
            _refresh_answers(info=_REFRESH_OK, refresh=httpx.Response(200, json={'code': 0, 'data': {'refresh_token': 'rt-new'}})),
        ),
        ('confirm refused', _refresh_answers(info=_REFRESH_OK, confirm=httpx.Response(200, json={'code': -101}))),
        ('confirm not json', _refresh_answers(info=_REFRESH_OK, confirm=httpx.Response(502, text='bad gateway'))),
    ]
    cases = []
    for name, answers in scripts:
        seen: list[httpx.Request] = []
        saved: list[dict[str, Any]] = []

        def handler(request: httpx.Request, answers: dict[str, httpx.Response] = answers) -> httpx.Response:
            seen.append(request)  # noqa: B023 - the closure runs within this iteration
            path = request.url.path
            if path.endswith('/cookie/info'):
                return answers['info']
            if path.startswith('/correspond/1/'):
                return answers['correspond']
            if path.endswith('/cookie/refresh'):
                return answers['refresh']
            return answers['confirm']

        async def save(renewal: bilibili_session.Renewal, saved: list[dict[str, Any]] = saved, seen: list[httpx.Request] = seen) -> None:
            saved.append({'after_requests': len(seen), 'cookies': _dump_cookies(renewal.cookies), 'refresh_token': renewal.refresh_token})

        logged: list[str] = []
        try:
            with (
                mock.patch.object(bilibili_session.time, 'time', return_value=NOW),
                mock.patch.object(http.cookiejar.time, 'time', return_value=NOW),
                mock.patch.object(bilibili_session, 'correspond_path', lambda ts: f'path-for-{ts}'),
                mock.patch.object(bilibili_session.log, 'warning', lambda msg, *args: logged.append(msg % args)),  # noqa: B023 - the closure runs within this iteration
            ):
                result = asyncio.run(bilibili_session.refresh_session(_jar(), 'rt-old', save=save, transport=httpx.MockTransport(handler)))
            outcome: dict[str, Any] = {
                'result': None
                if result is None
                else {'cookies': _dump_cookies(result.cookies), 'refresh_token': result.refresh_token, 'renewed': result.renewed}
            }
        except Exception as exc:  # noqa: BLE001
            outcome = _exc(exc)
        requests = []
        for request in seen:
            entry = _request(request)
            entry['form'] = parse_qsl(request.content.decode(), keep_blank_values=True)
            requests.append(entry)
        cases.append({'case': name, 'requests': requests, 'saved': saved, 'warnings': logged, **outcome})
    return cases


# ---------- bilibili.py ----------


def _bare() -> bilibili.Bilibili:
    """A crawler without its temporary directory, for the helpers that need none."""
    crawler = bilibili.Bilibili.__new__(bilibili.Bilibili)
    crawler._tmp_dir = SimpleNamespace(cleanup=lambda: None)  # noqa: SLF001
    return crawler


def _stored(names: tuple[str, ...] = ('SESSDATA', 'bili_jct', 'buvid3', 'DedeUserID'), **values: str) -> tuple[StoredCookie, ...]:
    return tuple(_cookie(name, values.get(name, f'v-{name}')) for name in names)


def _verify_cases() -> list[dict[str, Any]]:
    scripts: list[tuple[tuple[StoredCookie, ...], Any]] = [
        (_stored(('SESSDATA', 'bili_jct', 'DedeUserID')), None),
        (_stored(()), None),
        (
            (_cookie('sessdata', 'x'), _cookie('BILI_JCT', 'y'), _cookie('Buvid3', 'z'), _cookie('dedeuserid', '1')),
            (200, {'code': 0, 'data': {'isLogin': True, 'uname': 'u', 'mid': 1}}),
        ),
        (_stored(), (200, {'code': 0, 'data': {'isLogin': True, 'uname': '某人', 'mid': 42}})),
        (_stored(), (200, {'code': 0, 'data': {'isLogin': True, 'uname': '', 'mid': 42}})),
        (_stored(), (200, {'code': 0, 'data': {'isLogin': True, 'uname': 'only', 'mid': 0}})),
        (_stored(), (200, {'code': 0, 'data': {'isLogin': True}})),
        (_stored(), (200, {'code': 0, 'data': {'isLogin': 1, 'uname': 7, 'mid': 1.5}})),
        (_stored(), (200, {'code': -101, 'message': '账号未登录', 'data': {'isLogin': False}})),
        (_stored(), (200, {'code': 0, 'data': {'isLogin': False}})),
        (_stored(), (200, {'code': 0})),
        (_stored(), (200, {'code': 0, 'data': 'x'})),
        (_stored(), (412, {'code': -412})),
        (_stored(), (412, {'code': 0, 'data': {'isLogin': True, 'uname': 'u'}})),
        (_stored(), (200, {'code': -352})),
        (_stored(), (200, [1])),
        (_stored(), (200, 'text')),
        (_stored(), (200, b'<html>')),
        (_stored(), (200, b'\xff\xfe')),
        ((*_stored(), _cookie('extra', ''), _cookie('more', 'm')), (200, {'code': 0, 'data': {'isLogin': True, 'uname': 'u', 'mid': 1}})),
    ]
    cases = []
    for cookies, answer in scripts:
        seen: list[httpx.Request] = []

        def handler(request: httpx.Request, answer: Any = answer) -> httpx.Response:
            seen.append(request)  # noqa: B023 - the closure runs within this iteration
            status, payload = answer
            if isinstance(payload, bytes):
                return httpx.Response(status, content=payload)
            return httpx.Response(status, json=payload)

        check = asyncio.run(bilibili.verify_session(cookies, transport=httpx.MockTransport(handler)))
        cases.append(
            {
                'cookies': _dump_cookies(cookies),
                'answer': None if answer is None else {'status': answer[0], 'body': _body_text(answer[1])},
                'requests': [_request(r) for r in seen],
                'check': {'ok': check.ok, 'account': check.account, 'message': check.message, 'rejected': check.rejected},
            }
        )
    return cases


def _body_text(payload: Any) -> str:
    if isinstance(payload, bytes):
        return payload.decode('latin-1')
    return httpx.Response(200, json=payload).content.decode()


def _helpers() -> list[dict[str, Any]]:
    sizes = [None, -1, 0, 1, 1023, 1024, 1536, 20 * 1024 * 1024, 1024**3 - 1, 5 * 1024**4, 3 * 1024**5, 10**20]
    ints = [None, True, False, 0, 1, -5, 1.9, 0.5, -0.5, '12', ' 34 ', '0', '-3', '1.5', '', '٣', '²', [1], 1e30]
    views: list[Any] = [
        {},
        {'View': None},
        {'View': {'dimension': {'height': 1080}, 'pubdate': 1704067200}},
        {'View': {'dimension': {'height': 0}, 'pages': [{'dimension': {'height': 720}}, {'dimension': {'height': '2160'}}, 'x', {}]}},
        {'View': {'dimension': 'x', 'pages': [{'dimension': None}]}},
        {'View': {'pages': None, 'ctime': '1700000000'}},
        {'View': {'pubdate': 0, 'ctime': 1700000000.7}},
        {'View': {'pubdate': True, 'ctime': None}},
        {'View': {'pubdate': 253402300800}},
        {'View': {'pubdate': -5, 'ctime': '٣'}},
    ]
    covers = [
        '//i0.hdslb.com/bfs/archive/a.jpg',
        'https://i0.hdslb.com/bfs/archive/transparent.png',
        'https://i0.hdslb.com/bfs/ARCHIVE/Transparent.GIF?x=1',
        'https://i0.hdslb.com/archive/transparent-old.jpg',
        'https://example.com/transparent.png.jpg',
        'http://x/a/transparent.gif#frag',
        '',
        '////x',
        'not a url',
    ]
    cases: list[dict[str, Any]] = [
        {'helper': 'format_file_size', 'input': size, 'output': bilibili.Bilibili._format_file_size(size)}  # noqa: SLF001
        for size in sizes
    ]
    for value in ints:
        try:
            output: Any = bilibili.Bilibili._to_positive_int(value)  # noqa: SLF001
        except Exception as exc:  # noqa: BLE001
            output = _exc(exc)
        cases.append(
            {
                'helper': 'to_positive_int',
                'input': value if not isinstance(value, float) or value < _BIG_FLOAT else str(value),
                'output': output,
            }
        )
    for detail in views:
        try:
            resolution: Any = bilibili.Bilibili._extract_resolution_label(detail)  # noqa: SLF001
        except Exception as exc:  # noqa: BLE001
            resolution = _exc(exc)
        try:
            release: Any = bilibili.Bilibili._extract_release_date(detail)  # noqa: SLF001
        except Exception as exc:  # noqa: BLE001
            release = _exc(exc)
        cases.append({'helper': 'detail', 'input': detail, 'resolution': resolution, 'release_date': release})
    for url in covers:
        normalized = bilibili.Bilibili._normalize_cover_url(url)  # noqa: SLF001
        cases.append(
            {
                'helper': 'cover',
                'input': url,
                'normalized': normalized,
                'placeholder': bilibili.Bilibili._is_placeholder_cover_url(normalized),  # noqa: SLF001
            }
        )
    return cases


def _notifications() -> list[dict[str, Any]]:
    """`_notify_download` / `_resolve_download_failure_notification`: the exact calls."""
    combos: list[dict[str, Any]] = [
        {
            'bvid': 'BV1wM4m1S7oo',
            'title': '让你说话了吗?笨蛋',
            'upper': '孟程程_',
            'fav_id': 123,
            'resolution': '720p',
            'file_size_bytes': 20 * 1024 * 1024,
            'release_date': '2024-01-01',
            'cover_url': 'https://example.com/cover.jpg',
        },
        {'bvid': 'BV1xx411c7mD', 'title': 't', 'upper': 'u', 'fav_id': -1},
        {
            'bvid': 'BV1xx411c7mD',
            'title': 't',
            'upper': 'u',
            'fav_id': -1,
            'resolution': '',
            'file_size_bytes': 0,
            'release_date': '   ',
            'cover_url': '',
        },
        {
            'bvid': 'BV1xx411c7mD',
            'title': 'multi\nline',
            'upper': 'u',
            'fav_id': 7,
            'file_size_bytes': 1023,
            'release_date': ' 2020-02-02 ',
        },
    ]
    cases = []
    for combo in combos:
        enqueued: list[dict[str, Any]] = []
        resolved: list[dict[str, Any]] = []

        async def enqueue(enqueued: list[dict[str, Any]] = enqueued, **kwargs: Any) -> None:
            enqueued.append(kwargs)

        async def resolve(resolved: list[dict[str, Any]] = resolved, **kwargs: Any) -> None:
            resolved.append(kwargs)

        crawler = _bare()
        with mock.patch.object(bilibili, 'enqueue_notification', enqueue), mock.patch.object(bilibili, 'resolve_notification', resolve):
            recovered = asyncio.run(crawler._notify_download(**combo))  # noqa: SLF001
        cases.append({'input': combo, 'enqueued': enqueued, 'resolved': resolved, 'recovered': recovered})
    return cases


def _download_commands() -> list[dict[str, Any]]:
    cases = []
    for proxy in ('', 'http://gluetun-hk.proxy.svc.cluster.local:80', 'socks5://u:p@h:1'):
        crawler = _bare()
        crawler.cfg = settings.Bilibili(proxy=proxy)
        crawler.cookie_path = Path('/tmp/fav-bilibili-x/bilibili-main.txt')  # noqa: S108 - a path string only
        commands: list[list[str]] = []

        def run(command: list[str], commands: list[list[str]] = commands, **kwargs: Any) -> SimpleNamespace:
            commands.append(command)
            assert kwargs == {'text': True, 'capture_output': True, 'check': False}  # noqa: S101
            return SimpleNamespace(returncode=0, stdout='', stderr='')

        with mock.patch.object(subprocess, 'run', run):
            crawler.download('https://www.bilibili.com/video/BV1xx411c7mD', 'BV1xx411c7mD', Path('/tmp/fav-bilibili-x/videos'))  # noqa: S108
        cases.append({'proxy': proxy, 'command': commands[0]})
    return cases


def _failures() -> list[dict[str, Any]]:
    """The messages a yt-dlp failure becomes, and the retry log lines."""
    outcomes = [
        (1, '', 'ERROR: [BiliBili] BV1: Unable to download\n'),
        (2, 'stdout only\n', ''),
        (3, '', ''),
        (-9, '', '   '),
        (1, 'out', 'err\r\nmore\r'),
    ]
    cases = []
    for returncode, stdout, stderr in outcomes:
        crawler = _bare()
        crawler.cfg = settings.Bilibili()
        crawler.cookie_path = Path('/c.txt')
        debug: list[str] = []
        with (
            mock.patch.object(subprocess, 'run', lambda *_a, **_k: SimpleNamespace(returncode=returncode, stdout=stdout, stderr=stderr)),  # noqa: PT008, B023 - the closure runs within this iteration
            mock.patch.object(bilibili.log, 'debug', lambda msg, *args: debug.append(msg % args)),  # noqa: B023 - the closure runs within this iteration
            mock.patch('time.sleep'),
        ):
            try:
                crawler.download('https://www.bilibili.com/video/BV1x', 'BV1x', Path('/nonexistent-dir'), max_attempts=3, base_delay=5)
                outcome: dict[str, Any] = {'ok': True}
            except bilibili.DownloadError as exc:
                outcome = {**_exc(exc), 'dedupe_key': exc.notification_dedupe_key}
        cases.append({'returncode': returncode, 'stdout': stdout, 'stderr': stderr, 'debug': debug, **outcome})
    return cases


def _texts() -> list[dict[str, Any]]:
    return [
        {'name': 'session_site', 'input': 'main', 'output': bilibili.session_site('main')},
        {'name': 'relogin_hint', 'input': 'main', 'output': bilibili.relogin_hint('main')},
        {'name': 'download_failure_key', 'input': 'BV1x', 'output': bilibili.Bilibili._download_failure_key('BV1x')},  # noqa: SLF001
        {
            'name': 'download_failure_notification_key',
            'input': 'BV1x',
            'output': bilibili.Bilibili._download_failure_notification_key('BV1x'),  # noqa: SLF001
        },
        {'name': 'missing_cookies', 'input': ['SESSDATA', 'buvid3'], 'output': bilibili.missing_cookies(_stored(('SESSDATA', 'buvid3')))},
    ]


GOLDENS = {
    'bilibili/api_requests.json': _api_requests,
    'bilibili/api_answers.json': _api_answers,
    'bilibili/bvid.json': _bvids,
    'bilibili/ticket_hexsign.json': _hexsigns,
    'bilibili/tickets.json': _tickets,
    'bilibili/merge_cookies.json': _merges,
    'bilibili/set_cookie.json': _set_cookies,
    'bilibili/renew_ticket.json': _renew_ticket_cases,
    'bilibili/refresh_session.json': _refresh_cases,
    'bilibili/verify_session.json': _verify_cases,
    'bilibili/helpers.json': _helpers,
    'bilibili/notifications.json': _notifications,
    'bilibili/download_commands.json': _download_commands,
    'bilibili/download_failures.json': _failures,
    'bilibili/texts.json': _texts,
}
