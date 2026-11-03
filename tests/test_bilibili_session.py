# ruff: noqa: INP001, S101, ANN001, ANN202, PLR2004, S105, EM101, EM102, TRY003

import asyncio
from urllib.parse import parse_qs

import httpx
import pytest

from src.tool.site_sessions import StoredCookie, cookie_value
from src.web import bilibili_session

NOW = 1_800_000_000.0


def _jar(**extra: str) -> tuple[StoredCookie, ...]:
    values = {'SESSDATA': 'old-sess', 'bili_jct': 'old-jct', 'buvid3': 'b3', 'DedeUserID': '42', **extra}
    return tuple(StoredCookie(name=k, value=v, domain='.bilibili.com') for k, v in values.items())


def test_the_ticket_signature_matches_bilibili_api() -> None:
    # hmac_sha256("XgwSnGZ1p", "ts1800000000"), as bilibili_api computes it.
    import hashlib  # noqa: PLC0415
    import hmac  # noqa: PLC0415

    expected = hmac.new(b'XgwSnGZ1p', b'ts1800000000', hashlib.sha256).digest().hex()

    assert bilibili_session.ticket_hexsign(1_800_000_000) == expected


def test_the_correspond_path_is_rsa_oaep_hex() -> None:
    path = bilibili_session.correspond_path(1_700_000_000_000)

    assert len(path) == 256
    int(path, 16)


def test_a_ticket_with_days_to_go_is_kept() -> None:
    cookies = _jar(bili_ticket='t', bili_ticket_expires=str(int(NOW + 2.5 * 86400)))

    assert asyncio.run(bilibili_session.renew_ticket(cookies, transport=httpx.MockTransport(_fail), now=NOW)) is None


def _fail(request: httpx.Request) -> httpx.Response:
    raise AssertionError(f'unexpected request {request.url}')


def test_a_missing_or_expiring_ticket_is_renewed() -> None:
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return httpx.Response(200, json={'code': 0, 'data': {'ticket': 'fresh', 'created_at': int(NOW), 'ttl': 259200}})

    cookies = _jar(bili_ticket='stale', bili_ticket_expires=str(int(NOW + 3600)))

    renewed = asyncio.run(bilibili_session.renew_ticket(cookies, transport=httpx.MockTransport(handler), now=NOW))

    assert renewed is not None
    assert cookie_value(renewed, 'bili_ticket') == 'fresh'
    assert cookie_value(renewed, 'bili_ticket_expires') == str(int(NOW) + 259200)
    assert cookie_value(renewed, 'SESSDATA') == 'old-sess'
    assert seen[0].method == 'POST'
    assert seen[0].url.params['key_id'] == 'ec02'
    assert seen[0].url.params['context[ts]'] == str(int(NOW))
    assert 'SESSDATA=old-sess' in seen[0].headers['Cookie']


def test_a_ticket_bilibili_refuses_raises() -> None:
    transport = httpx.MockTransport(lambda _r: httpx.Response(200, json={'code': -400, 'message': 'bad'}))

    with pytest.raises(bilibili_session.RenewalError, match='-400'):
        asyncio.run(bilibili_session.renew_ticket(_jar(), transport=transport, now=NOW))


def test_no_refresh_when_bilibili_does_not_ask() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == '/x/passport-login/web/cookie/info'
        return httpx.Response(200, json={'code': 0, 'data': {'refresh': False}})

    async def _save(_renewal) -> None:
        raise AssertionError('nothing to save')

    result = asyncio.run(bilibili_session.refresh_session(_jar(), 'rt-old', save=_save, transport=httpx.MockTransport(handler)))

    assert result is None


def _refresh_handler(seen: list[tuple[str, httpx.Request]], *, confirm_code: int = 0):
    def handler(request: httpx.Request) -> httpx.Response:
        path = request.url.path
        seen.append((path, request))
        if path.endswith('/cookie/info'):
            return httpx.Response(200, json={'code': 0, 'data': {'refresh': True}})
        if path.startswith('/correspond/1/'):
            return httpx.Response(200, text='<html><div id="1-name">rcsrf</div></html>')
        if path.endswith('/cookie/refresh'):
            headers = [
                ('Set-Cookie', 'SESSDATA=new-sess; Path=/; Domain=bilibili.com; Expires=Wed, 01 Jan 2031 00:00:00 GMT; HttpOnly'),
                ('Set-Cookie', 'bili_jct=new-jct; Path=/; Domain=bilibili.com; Expires=Wed, 01 Jan 2031 00:00:00 GMT'),
                ('Set-Cookie', 'sid=new-sid; Path=/; Domain=bilibili.com'),
            ]
            return httpx.Response(200, json={'code': 0, 'data': {'refresh_token': 'rt-new'}}, headers=headers)
        if path.endswith('/confirm/refresh'):
            return httpx.Response(200, json={'code': confirm_code})
        raise AssertionError(path)

    return handler


def test_a_refresh_saves_the_new_pair_before_confirming_it() -> None:
    seen: list[tuple[str, httpx.Request]] = []
    saved: list[tuple[int, object]] = []

    async def _save(renewal) -> None:
        saved.append((len(seen), renewal))

    result = asyncio.run(
        bilibili_session.refresh_session(_jar(), 'rt-old', save=_save, transport=httpx.MockTransport(_refresh_handler(seen)))
    )

    assert result is not None
    assert result.refresh_token == 'rt-new'
    assert cookie_value(result.cookies, 'SESSDATA') == 'new-sess'
    assert cookie_value(result.cookies, 'bili_jct') == 'new-jct'
    assert cookie_value(result.cookies, 'sid') == 'new-sid'
    assert cookie_value(result.cookies, 'buvid3') == 'b3'
    assert [c.name for c in result.cookies].count('SESSDATA') == 1
    # Saved after the refresh answered (info, correspond, refresh) and before the confirmation.
    assert saved[0][0] == 3
    assert [path.rsplit('/', 1)[-1] for path, _ in seen][-1] == 'refresh'
    refresh = parse_qs(seen[2][1].content.decode())
    assert refresh == {'csrf': ['old-jct'], 'refresh_csrf': ['rcsrf'], 'source': ['main_web'], 'refresh_token': ['rt-old']}
    confirm_path, confirm = seen[3]
    assert confirm_path.endswith('/confirm/refresh')
    assert parse_qs(confirm.content.decode()) == {'csrf': ['new-jct'], 'refresh_token': ['rt-old']}
    assert 'SESSDATA=new-sess' in confirm.headers['Cookie']
    assert 'old-sess' not in confirm.headers['Cookie']


def test_a_failed_confirmation_keeps_the_refreshed_pair() -> None:
    seen: list[tuple[str, httpx.Request]] = []

    async def _save(_renewal) -> None:
        return None

    transport = httpx.MockTransport(_refresh_handler(seen, confirm_code=-101))
    result = asyncio.run(bilibili_session.refresh_session(_jar(), 'rt-old', save=_save, transport=transport))

    assert result is not None
    assert cookie_value(result.cookies, 'SESSDATA') == 'new-sess'


def test_a_refresh_without_a_csrf_page_raises() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith('/cookie/info'):
            return httpx.Response(200, json={'code': 0, 'data': {'refresh': True}})
        return httpx.Response(404, text='gone')

    async def _save(_renewal) -> None:
        raise AssertionError('nothing to save')

    with pytest.raises(bilibili_session.RenewalError, match='correspond'):
        asyncio.run(bilibili_session.refresh_session(_jar(), 'rt', save=_save, transport=httpx.MockTransport(handler)))


def test_merging_replaces_by_name_and_drops_expired_cookies() -> None:
    jar = _jar()
    merged = bilibili_session.merge_cookies(
        jar,
        [
            StoredCookie(name='SESSDATA', value='n', domain='.bilibili.com'),
            StoredCookie(name='buvid3', value='x', domain='.bilibili.com', expires=1.0),
            StoredCookie(name='b_nut', value='9', domain='.bilibili.com'),
        ],
    )

    assert [(c.name, c.value) for c in merged] == [('SESSDATA', 'n'), ('bili_jct', 'old-jct'), ('DedeUserID', '42'), ('b_nut', '9')]
