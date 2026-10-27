# ruff: noqa: ANN001, ANN003, ANN202, ARG001, ARG002, INP001, PLR2004, S101, S105, SLF001

import asyncio
import threading
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

import pytest
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

import src.api.routes as routes_module
import src.tool.site_sessions as site_sessions_module
from src.api import browser_login
from src.api.app import create_app
from src.api.browser_login import (
    STATE_CANCELLED,
    STATE_FAILED,
    STATE_SAVED,
    STATE_TIMEOUT,
    STATE_WAITING,
    BrowserLoginManager,
    LoginBusyError,
    LoginSite,
    UnknownLoginSiteError,
    build_login_launch_options,
)
from src.api.errors import ApiError
from src.tool.site_sessions import SiteSession, StoredCookie, cookie_expiry, cookies_from_browser

API_TOKEN = 'test-token'
AUTH = {'Authorization': f'Bearer {API_TOKEN}'}


@dataclass
class _Check:
    ok: bool
    account: str = ''
    message: str = ''


class _FakeBrowser:
    """A login window whose cookies the test scripts, one list per poll."""

    def __init__(self, cookie_rounds: list[list[dict]]) -> None:
        self.cookie_rounds = cookie_rounds
        self.opened: list[str] = []
        self.closed_calls = 0
        self.closed = False

    async def open(self, url: str) -> None:
        self.opened.append(url)

    async def cookies(self, urls: tuple[str, ...]) -> list[dict]:
        return self.cookie_rounds.pop(0) if len(self.cookie_rounds) > 1 else self.cookie_rounds[0]

    async def close(self) -> None:
        self.closed_calls += 1


class _Launcher:
    def __init__(self, browser: _FakeBrowser | None = None, error: Exception | None = None) -> None:
        self.browser = browser
        self.error = error
        self.calls: list[dict] = []

    async def __call__(self, **kwargs) -> _FakeBrowser:
        self.calls.append(kwargs)
        if self.error is not None:
            raise self.error
        assert self.browser is not None
        return self.browser


class _Store:
    def __init__(self) -> None:
        self.saved: list[tuple] = []
        self.checks: list[tuple] = []
        self.sessions: dict[str, SiteSession] = {}

    async def save_signed_in(self, site, cookies, *, account, expires_at) -> None:
        self.saved.append((site, cookies, account, expires_at))

    async def record_check(self, site, *, ok, message='') -> None:
        self.checks.append((site, ok, message))

    async def load(self, site):
        return self.sessions.get(site)

    async def load_all(self):
        return dict(self.sessions)


def _cookie(name: str, value: str, *, expires: float = -1) -> dict:
    return {'name': name, 'value': value, 'domain': '.example.com', 'path': '/', 'expires': expires}


def _site(verify=None) -> LoginSite:
    async def _default_verify(cookies):
        value = next((c.value for c in cookies if c.name == 'SID'), '')
        return _Check(ok=value.startswith('user_'), account=value.removeprefix('user_'), message='' if value else 'none')

    return LoginSite(
        key='demo',
        label='Demo',
        login_url='https://example.com/login',
        cookie_urls=('https://example.com/',),
        session_cookie='SID',
        verify=verify or _default_verify,
    )


@pytest.fixture
def store(monkeypatch) -> _Store:
    fake = _Store()
    monkeypatch.setattr(browser_login, 'site_sessions', fake)
    monkeypatch.setattr(routes_module, 'site_sessions', fake)
    return fake


def _manager(launcher, **kwargs) -> BrowserLoginManager:
    kwargs.setdefault('poll_seconds', 0.01)
    sites = {'demo': _site(kwargs.pop('verify', None))}
    return BrowserLoginManager(sites, launcher=launcher, profile_root=Path('/data/profiles'), **kwargs)


async def _finish(manager: BrowserLoginManager, site: str = 'demo'):
    attempt = await manager.start(site)
    for _ in range(500):
        if not attempt.active:
            break
        await asyncio.sleep(0.01)
    return attempt


# ---------- storage helpers ----------


def test_browser_cookies_keep_expiry_and_drop_nameless_entries() -> None:
    cookies = cookies_from_browser([_cookie('SID', 'v', expires=1_800_000_000), _cookie('tmp', 'x'), {'value': 'orphan'}])

    assert [c.name for c in cookies] == ['SID', 'tmp']
    assert cookies[1].expires is None
    assert cookie_expiry(cookies, 'sid') == datetime.fromtimestamp(1_800_000_000, tz=UTC)
    assert cookie_expiry(cookies, 'tmp') is None


def test_stored_cookies_survive_the_round_trip() -> None:
    cookies = (StoredCookie(name='PHPSESSID', value='1_a', domain='.pixiv.net', expires=1.5e9),)

    decoded = site_sessions_module._decode_cookies(site_sessions_module._encode_cookies(cookies))

    assert decoded == cookies
    assert site_sessions_module._decode_cookies('not json') == ()


def test_saving_a_login_upserts_by_site(monkeypatch) -> None:
    calls: list[tuple[str, tuple]] = []

    class _Db:
        async def query_db(self, query, params=()):
            calls.append((query, params))
            return []

    monkeypatch.setattr(site_sessions_module, 'database', _Db())
    cookies = (StoredCookie(name='PHPSESSID', value='1_a', domain='.pixiv.net'),)

    asyncio.run(site_sessions_module.save_signed_in('pixiv', cookies, account='1', expires_at=None))

    upsert = calls[-1]
    assert 'ON CONFLICT (site) DO UPDATE' in upsert[0]
    assert upsert[1][0] == 'pixiv'
    assert upsert[1][2] == '1'
    assert upsert[1][3] == site_sessions_module.STATUS_VALID


# ---------- the login flow ----------


def test_a_verified_login_is_saved_and_the_browser_closed(store) -> None:
    browser = _FakeBrowser([[], [_cookie('SID', 'anon')], [_cookie('SID', 'user_42', expires=1_900_000_000)]])
    launcher = _Launcher(browser)

    attempt = asyncio.run(_finish(_manager(launcher)))

    assert attempt.state == STATE_SAVED
    assert attempt.account == '42'
    assert browser.opened == ['https://example.com/login']
    assert browser.closed_calls == 1
    assert launcher.calls[0]['profile_dir'] == Path('/data/profiles/demo')
    site, cookies, account, expires_at = store.saved[0]
    assert (site, account) == ('demo', '42')
    assert cookies[0].value == 'user_42'
    assert expires_at == datetime.fromtimestamp(1_900_000_000, tz=UTC)


def test_a_login_that_never_completes_times_out_and_closes(store) -> None:
    browser = _FakeBrowser([[_cookie('SID', 'anon')]])

    attempt = asyncio.run(_finish(_manager(_Launcher(browser), timeout_seconds=0.05)))

    assert attempt.state == STATE_TIMEOUT
    assert attempt.message in {'none', ''}
    assert browser.closed_calls == 1
    assert store.saved == []


def test_closing_the_window_ends_the_login(store) -> None:
    browser = _FakeBrowser([[]])
    browser.closed = True

    attempt = asyncio.run(_finish(_manager(_Launcher(browser))))

    assert attempt.state == STATE_CANCELLED
    assert browser.closed_calls == 1


def test_a_cancelled_login_saves_nothing(store) -> None:
    browser = _FakeBrowser([[]])
    manager = _manager(_Launcher(browser))

    async def scenario():
        attempt = await manager.start('demo')
        await asyncio.sleep(0.05)
        assert attempt.state == STATE_WAITING
        await manager.cancel()
        return attempt

    attempt = asyncio.run(scenario())

    assert attempt.state == STATE_CANCELLED
    assert attempt.active is False
    assert browser.closed_calls == 1
    assert store.saved == []


def test_a_browser_that_cannot_start_reports_the_failure(store) -> None:
    attempt = asyncio.run(_finish(_manager(_Launcher(error=RuntimeError('no display')))))

    assert attempt.state == STATE_FAILED
    assert 'no display' in attempt.message


def test_only_one_login_runs_at_a_time(store) -> None:
    manager = _manager(_Launcher(_FakeBrowser([[]])))

    async def scenario():
        await manager.start('demo')
        with pytest.raises(LoginBusyError):
            await manager.start('demo')
        await manager.cancel()

    asyncio.run(scenario())


def test_a_busy_check_refuses_the_login_before_anything_starts(store) -> None:
    launcher = _Launcher(_FakeBrowser([[]]))

    async def busy() -> str:
        return 'RedNote is crawling'

    manager = _manager(launcher, busy_check=busy)

    with pytest.raises(LoginBusyError, match='RedNote'):
        asyncio.run(manager.start('demo'))
    assert launcher.calls == []


def test_an_unknown_site_is_rejected(store) -> None:
    with pytest.raises(UnknownLoginSiteError):
        asyncio.run(_manager(_Launcher()).start('nope'))


def test_the_ticket_only_opens_the_running_login(store) -> None:
    manager = _manager(_Launcher(_FakeBrowser([[]])))

    async def scenario():
        attempt = await manager.start('demo')
        assert manager.ticket_matches(attempt.ticket)
        assert not manager.ticket_matches('guess')
        assert not manager.ticket_matches('')
        await manager.cancel()
        assert not manager.ticket_matches(attempt.ticket)

    asyncio.run(scenario())


def test_checking_a_stored_session_records_the_answer(store) -> None:
    store.sessions['demo'] = SiteSession(
        site='demo',
        cookies=(StoredCookie(name='SID', value='user_7', domain='.example.com'),),
        account='7',
        status='valid',
        message='',
        expires_at=None,
        signed_in_at=None,
        checked_at=None,
    )

    check = asyncio.run(_manager(_Launcher()).check_stored('demo'))

    assert check.ok is True
    assert store.checks == [('demo', True, '')]


def test_the_login_window_is_headed_on_the_virtual_display() -> None:
    options = build_login_launch_options(profile_dir=Path('/data/p'), proxy='', display=99)

    assert options['headless'] is False
    assert options['env']['DISPLAY'] == ':99'
    assert options['channel'] == 'chromium'
    assert options['proxy'] is None
    assert '--enable-automation' in options['ignore_default_args']


def test_pixiv_is_a_login_site() -> None:
    site = browser_login.LOGIN_SITES['pixiv']

    assert site.session_cookie == 'PHPSESSID'
    assert site.login_url.startswith('https://accounts.pixiv.net/login')


# ---------- API ----------


class _Service:
    def authenticate(self, header):
        if header != f'Bearer {API_TOKEN}':
            raise ApiError(status_code=401, code='unauthorized', message='no')

    def close(self) -> None:
        return None


def _client(manager: BrowserLoginManager) -> TestClient:
    return TestClient(create_app(service=_Service(), browser_login=manager))


def test_the_api_lists_sites_and_never_exposes_cookie_values(store) -> None:
    store.sessions['demo'] = SiteSession(
        site='demo',
        cookies=(StoredCookie(name='SID', value='user_7-secret', domain='.example.com'),),
        account='7',
        status='valid',
        message='',
        expires_at=datetime(2027, 1, 1, tzinfo=UTC),
        signed_in_at=datetime(2026, 10, 27, tzinfo=UTC),
        checked_at=None,
    )

    with _client(_manager(_Launcher())) as client:
        response = client.get('/api/v2/site-sessions', headers=AUTH)

    assert response.status_code == 200
    body = response.json()
    assert body['login'] is None
    assert body['items'][0] == {
        'site': 'demo',
        'label': 'Demo',
        'signed_in': True,
        'account': '7',
        'status': 'valid',
        'message': '',
        'cookie_count': 1,
        'expires_at': '2027-01-01T00:00:00Z',
        'signed_in_at': '2026-10-27T00:00:00Z',
        'checked_at': None,
    }
    assert 'secret' not in response.text


def test_the_api_requires_the_token(store) -> None:
    with _client(_manager(_Launcher())) as client:
        assert client.get('/api/v2/site-sessions').status_code == 401
        assert client.post('/api/v2/site-sessions/demo/login').status_code == 401


def test_the_api_starts_a_login_and_hands_out_its_ticket(store) -> None:
    manager = _manager(_Launcher(_FakeBrowser([[]])))

    with _client(manager) as client:
        started = client.post('/api/v2/site-sessions/demo/login', headers=AUTH)
        busy = client.post('/api/v2/site-sessions/demo/login', headers=AUTH)
        listed = client.get('/api/v2/site-sessions', headers=AUTH).json()
        cancelled = client.delete('/api/v2/site-sessions/login', headers=AUTH)

    assert started.status_code == 202
    assert started.json()['active'] is True
    assert started.json()['ticket']
    assert busy.status_code == 409
    assert listed['login']['ticket'] == started.json()['ticket']
    assert cancelled.json()['state'] == STATE_CANCELLED
    assert cancelled.json()['ticket'] == ''


def test_the_api_rejects_an_unknown_site(store) -> None:
    with _client(_manager(_Launcher())) as client:
        assert client.post('/api/v2/site-sessions/nope/login', headers=AUTH).status_code == 404


def test_checking_without_a_stored_session_is_a_conflict(store) -> None:
    with _client(_manager(_Launcher())) as client:
        assert client.post('/api/v2/site-sessions/demo/check', headers=AUTH).status_code == 409


# ---------- VNC bridge ----------


def test_the_vnc_socket_relays_bytes_for_the_ticket_holder(store) -> None:
    received: list[bytes] = []

    async def vnc_server(reader, writer):
        writer.write(b'RFB 003.008\n')
        await writer.drain()
        received.append(await reader.read(12))
        writer.close()

    holder: dict = {}

    def run_server():
        loop = asyncio.new_event_loop()
        holder['loop'] = loop
        server = loop.run_until_complete(asyncio.start_server(vnc_server, '127.0.0.1', 0))
        holder['port'] = server.sockets[0].getsockname()[1]
        holder['ready'].set()
        loop.run_forever()

    holder['ready'] = threading.Event()
    thread = threading.Thread(target=run_server, daemon=True)
    thread.start()
    holder['ready'].wait(5)

    manager = _manager(_Launcher(_FakeBrowser([[]])), vnc_port=holder['port'])
    with _client(manager) as client:
        ticket = client.post('/api/v2/site-sessions/demo/login', headers=AUTH).json()['ticket']

        with pytest.raises(WebSocketDisconnect), client.websocket_connect('/api/v2/site-sessions/vnc?ticket=wrong') as ws:
            ws.receive_bytes()

        with client.websocket_connect(f'/api/v2/site-sessions/vnc?ticket={ticket}', subprotocols=['binary']) as ws:
            assert ws.accepted_subprotocol == 'binary'
            assert ws.receive_bytes() == b'RFB 003.008\n'
            ws.send_bytes(b'RFB 003.008\n')
            with pytest.raises(WebSocketDisconnect):
                ws.receive_bytes()

        client.delete('/api/v2/site-sessions/login', headers=AUTH)

    holder['loop'].call_soon_threadsafe(holder['loop'].stop)
    assert received == [b'RFB 003.008\n']
