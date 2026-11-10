"""The HTTP API of `src/api` (WP3): the OpenAPI document and request/response replays.

`openapi.json` is the document FastAPI builds for the baseline app, with every route; the
Rust build serves the same document for the routes it has ported, and lists the rest as
pending (`PENDING` in `rust/crates/fav-api/src/openapi.rs`).

`requests.json` replays requests through the real FastAPI app (`create_app`) with the
real `FavApiService`, so routing, the auth dependency, request validation, the error
envelope, masking and the settings guard are all Python's. Only storage is replaced:
`app_settings` is a dict behind the real `settings.load_section` / `save_section`
(`_fetch_rows_sync`, `ensure_table_sync` and `_connect` are swapped out), control
requests are a list, and the stored logins are a dict. Each scenario names that state
(`setup`), so the Rust replay can seed a real database with the same rows. A response
field the database fills in itself (a new row's id and timestamp) is listed under
`volatile`.

`masking.json`, `credential_clients.json`, `login_sites.json`, `archive.json` and
`docs.json` call the real helpers directly.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
from dataclasses import dataclass, field
from datetime import UTC, date, datetime
from typing import TYPE_CHECKING, Any, Self

from fastapi.testclient import TestClient
from src.api import app as app_module
from src.api import archive, credentials, routes
from src.api import service as service_module
from src.api.app import create_app
from src.api.browser_login import BrowserLoginManager, login_sites
from src.api.credentials import parse_clients
from src.api.models import ApiConfig
from src.api.settings_masking import mask_section, unmask_section
from src.core import settings
from src.service.jobs import build_jobs
from src.tool import site_sessions
from src.tool.azurlane_l2d_sources import L2DSuOriginProbe
from src.tool.control_queue import ControlRequest
from src.tool.site_sessions import SiteSession, StoredCookie
from src.tool.telegram_bot import send_test_notification
from src.web.rednote import build_note_url

if TYPE_CHECKING:
    from collections.abc import Iterator

    from fastapi import FastAPI

TOKEN = 'token-for-tests'  # noqa: S105
AUTH = {'Authorization': f'Bearer {TOKEN}'}
FIXED_NOW = datetime(2026, 3, 2, 0, 0, tzinfo=UTC)
CLIENT_TOKEN = 'rsshub-token'  # noqa: S105
CLIENT_DIGEST = hashlib.sha256(CLIENT_TOKEN.encode()).hexdigest()
_RESPONSE_HEADERS = ('content-type', 'www-authenticate', 'allow', 'cache-control', 'location', 'vary')


class _NeverCalled:
    """A dependency the scenario must not reach (the source-specific services)."""

    def close(self) -> None:
        return None


# ---------- storage fakes ----------


@dataclass
class _State:
    rows: dict[str, Any] = field(default_factory=dict)
    requests: list[ControlRequest] = field(default_factory=list)
    sessions: dict[str, SiteSession] = field(default_factory=dict)


class _FakeCursor:
    def __init__(self, state: _State) -> None:
        self._state = state

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_exc: object) -> None:
        return None

    def execute(self, sql: str, params: tuple[Any, ...]) -> None:
        assert 'INSERT INTO app_settings' in sql, sql  # noqa: S101 - only save_section reaches here
        section, value = params
        self._state.rows[section] = json.loads(value)


class _FakeConnection:
    def __init__(self, state: _State) -> None:
        self._state = state

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_exc: object) -> None:
        return None

    def cursor(self) -> _FakeCursor:
        return _FakeCursor(self._state)


class _Sessions:
    VALUE_IN_PROFILE = site_sessions.VALUE_IN_PROFILE

    def __init__(self, state: _State) -> None:
        self._state = state

    async def load(self, site: str) -> SiteSession | None:
        return self._state.sessions.get(site)

    async def load_all(self) -> dict[str, SiteSession]:
        return dict(sorted(self._state.sessions.items()))


def _api_only(app: FastAPI) -> None:
    app.state.web_dist_dir = None


def _origin_probe(_proxy: str, *, connect_host: str = '') -> L2DSuOriginProbe:  # noqa: ARG001
    return L2DSuOriginProbe(
        ok=False,
        code='http_error',
        message='https://static.l2d.su/al/data/ships-CN.json returned HTTP 403.',
        exit_ip='203.0.113.7',
    )


@contextlib.contextmanager
def _patched(state: _State) -> Iterator[None]:
    saved = (
        settings._fetch_rows_sync,  # noqa: SLF001
        settings.ensure_table_sync,
        settings._connect,  # noqa: SLF001
        routes.site_sessions,
        credentials.site_sessions,
        app_module._mount_web_ui,  # noqa: SLF001
        service_module.probe_l2d_su_origin,
    )
    settings._fetch_rows_sync = lambda: dict(state.rows)  # noqa: SLF001
    settings.ensure_table_sync = lambda: None
    settings._connect = lambda: _FakeConnection(state)  # noqa: SLF001
    routes.site_sessions = _Sessions(state)
    credentials.site_sessions = _Sessions(state)
    # The goldens describe the API alone, whether or not a local web/dist was built;
    # the Rust tests and rust/tools/smoke cover the front end.
    app_module._mount_web_ui = _api_only  # noqa: SLF001
    # The l2d.su origin is never probed for real; the Rust harness answers the same.
    service_module.probe_l2d_su_origin = _origin_probe
    settings.use(None)
    settings.invalidate_cache()
    try:
        yield
    finally:
        (
            settings._fetch_rows_sync,  # noqa: SLF001
            settings.ensure_table_sync,
            settings._connect,  # noqa: SLF001
            routes.site_sessions,
            credentials.site_sessions,
            app_module._mount_web_ui,  # noqa: SLF001
            service_module.probe_l2d_su_origin,
        ) = saved
        settings.invalidate_cache()
        settings.use(settings.Settings())


def _request_row(row: dict[str, Any]) -> ControlRequest:
    def parse(value: str | None) -> datetime | None:
        return datetime.fromisoformat(value) if value else None

    return ControlRequest(
        request_id=row['id'],
        kind=row['kind'],
        target=row['target'],
        payload='{}',
        status=row['status'],
        requested_at=parse(row['requested_at']),
        started_at=parse(row.get('started_at')),
        finished_at=parse(row.get('finished_at')),
        result=row.get('result', ''),
        error=row.get('error', ''),
    )


def _session_row(row: dict[str, Any]) -> SiteSession:
    def parse(value: str | None) -> datetime | None:
        return datetime.fromisoformat(value) if value else None

    return SiteSession(
        site=row['site'],
        cookies=tuple(StoredCookie(**cookie) for cookie in row['cookies']),
        account=row.get('account', ''),
        status=row.get('status', ''),
        message=row.get('message', ''),
        expires_at=parse(row.get('expires_at')),
        signed_in_at=parse(row.get('signed_in_at')),
        checked_at=parse(row.get('checked_at')),
        extra=dict(row.get('extra', {})),
    )


def _service(state: _State) -> service_module.FavApiService:
    def create(kind: str, target: str) -> ControlRequest:
        request = ControlRequest(
            request_id=max((item.request_id for item in state.requests), default=0) + 1,
            kind=kind,
            target=target,
            payload='{}',
            status='pending',
            requested_at=FIXED_NOW,
            started_at=None,
            finished_at=None,
            result='',
            error='',
        )
        state.requests.append(request)
        return request

    def get(request_id: int) -> ControlRequest | None:
        return next((item for item in state.requests if item.request_id == request_id), None)

    def listing(statuses: list[str] | None, limit: int) -> list[ControlRequest]:
        # The same order and filter as list_control_requests_sync's SQL.
        rows = [item for item in state.requests if not statuses or item.status in statuses]
        rows.sort(key=lambda item: (item.requested_at, item.request_id), reverse=True)
        return rows[: max(1, min(limit, 200))]

    never = _NeverCalled()
    return service_module.FavApiService(
        dsn='postgresql://goldens@127.0.0.1:1/never-connected',
        token=TOKEN,
        now_provider=lambda: FIXED_NOW,
        job_provider=build_jobs,
        control_request_creator=create,
        control_request_getter=get,
        control_request_lister=listing,
        telegram_notification_tester=send_test_notification,
        runtime_service=never,
        author_service=never,
        kemono_creator_resolver=never,
        archive_library=archive.ArchiveLibrary('postgresql://goldens@127.0.0.1:1/never-connected'),
        azurlane_library=never,
        nikke_library=never,
        bd2_library=never,
        live2d_view_override_store=never,
    )


# ---------- scenarios ----------


def _req(method: str, path: str, *, headers: dict[str, str] | None = None, body: str | None = None) -> dict[str, Any]:
    request: dict[str, Any] = {'method': method, 'path': path, 'headers': AUTH if headers is None else headers}
    if body is not None:
        request['body'] = body
    return request


def _json_req(method: str, path: str, payload: Any, *, headers: dict[str, str] | None = None) -> dict[str, Any]:
    merged = {**(AUTH if headers is None else headers), 'content-type': 'application/json'}
    return _req(method, path, headers=merged, body=json.dumps(payload, ensure_ascii=False))


_REQUESTS_ROWS = [
    {
        'id': 7,
        'kind': 'trigger_job',
        'target': 'jandan',
        'status': 'succeeded',
        'requested_at': '2026-03-01T10:00:00+00:00',
        'started_at': '2026-03-01T10:00:01.500000+00:00',
        'finished_at': '2026-03-01T10:02:00+00:00',
        'result': 'ok',
    },
    {
        'id': 8,
        'kind': 'scheduled_job',
        'target': 'kemono',
        'status': 'failed',
        'requested_at': '2026-03-01T11:00:00+00:00',
        'started_at': '2026-03-01T11:00:00+00:00',
        'finished_at': '2026-03-01T11:00:30+00:00',
        'error': 'RuntimeError: boom',
    },
    {'id': 9, 'kind': 'trigger_job', 'target': 'all', 'status': 'pending', 'requested_at': '2026-03-01T12:00:00+00:00'},
    {
        'id': 10,
        'kind': 'trigger_job',
        'target': 'bilibili',
        'status': 'rejected',
        'requested_at': '2026-03-01T12:00:00+00:00',
        'started_at': '2026-03-01T12:00:01+00:00',
        'finished_at': '2026-03-01T12:00:01+00:00',
        'error': 'Runner unavailable for job: bilibili',
    },
]

_SESSIONS = [
    {
        'site': 'bilibili-main',
        'cookies': [
            {'name': 'SESSDATA', 'value': 's%2Cecret', 'domain': '.bilibili.com', 'path': '/', 'expires': 2e9},
            {'name': 'bili_jct', 'value': 'csrf', 'domain': '.bilibili.com', 'path': '/', 'expires': 1767225600.5},
            {'name': 'buvid3', 'value': '', 'domain': '.bilibili.com', 'path': '/', 'expires': None},
        ],
        'account': '某人 (42)',
        'status': 'valid',
        'expires_at': '2026-12-01T00:00:00+00:00',
        'signed_in_at': '2026-11-03T00:00:00+00:00',
        'checked_at': '2026-11-04T05:06:07.250000+00:00',
        'extra': {'refresh_token': 'never-served'},
    },
    {
        'site': 'rednote',
        'cookies': [
            {'name': 'web_session', 'value': site_sessions.VALUE_IN_PROFILE, 'domain': '.xiaohongshu.com', 'path': '/', 'expires': None}
        ],
        'account': '小红书用户',
        'status': 'valid',
        'signed_in_at': '2026-10-01T08:00:00+00:00',
    },
    {
        'site': 'twitter',
        'cookies': [{'name': 'ct0', 'value': 'csrf', 'domain': '.x.com', 'path': '/', 'expires': None}],
        'account': 'someone',
        'status': 'invalid',
        'message': 'Sign in to X again.',
        'checked_at': '2026-11-05T00:00:00+00:00',
    },
    {
        'site': 'stale-site',
        'cookies': [{'name': 'a', 'value': 'b', 'domain': '.example.com', 'path': '/', 'expires': None}],
    },
]

_BILIBILI_COMPLETE = {
    'cron': '0 * * * *',
    'enabled': True,
    'accounts': [{'name': 'main', 'favorites': [{'fav_id': 99, 'path': 'collection/bilibili/fav'}]}],
}
_TELEGRAM_ACCOUNTS = {
    'enabled': False,
    'accounts': [
        {'name': 'alice', 'api_id': 1, 'api_hash': 'hash-ALICE-0123', 'channels': [{'id': -1001234, 'path': 'collection/telegram/a'}]},
        {'name': 'bob', 'api_id': 2, 'api_hash': 'bh', 'channels': []},
    ],
}


def _scenarios() -> list[dict[str, Any]]:
    scenarios: list[dict[str, Any]] = []

    def add(name: str, requests: list[dict[str, Any]], **setup: Any) -> None:
        scenarios.append({'name': name, 'setup': setup, 'requests': requests})

    add(
        'health and documents are public',
        [
            _req('GET', '/healthz', headers={}),
            _req('GET', '/healthz?x=1', headers={'Authorization': 'Bearer wrong'}),
            _req('POST', '/healthz', headers={}),
            _req('GET', '/api/v1/health', headers={}),
            _req('GET', '/nope', headers={}),
            _req('GET', '/api/v2/notifications', headers={}),
            _req('GET', '/api/v2/notifications'),
            _req('POST', '/api/v2/notifications/ack', headers={**AUTH, 'content-type': 'application/json'}, body='{"ids":[1]}'),
        ],
    )
    add(
        'authorization',
        [
            _req('GET', '/api/v2/jobs', headers={}),
            _req('GET', '/api/v2/jobs', headers={'Authorization': ''}),
            _req('GET', '/api/v2/jobs', headers={'Authorization': 'Basic abc'}),
            _req('GET', '/api/v2/jobs', headers={'Authorization': 'bearer token-for-tests'}),
            _req('GET', '/api/v2/jobs', headers={'Authorization': 'Bearer '}),
            _req('GET', '/api/v2/jobs', headers={'Authorization': 'Bearer    '}),
            _req('GET', '/api/v2/jobs', headers={'Authorization': 'Bearer wrong-token'}),
            _req('GET', '/api/v2/jobs', headers={'Authorization': 'Bearer  token-for-tests  '}),
            _req('GET', '/api/v2/settings', headers={'Authorization': 'Bearer token-for-tests-x'}),
            # The token check runs before query validation, but a body that is not JSON fails first.
            _req('GET', '/api/v2/job-requests?limit=0', headers={}),
            _req('POST', '/api/v2/job-requests', headers={'content-type': 'application/json'}, body='{bad'),
            _req('POST', '/api/v2/job-requests', headers={'content-type': 'application/json'}, body='{"target":"nope"}'),
            _req('PUT', '/api/v2/settings/BAD', headers={}),
            _req('GET', '/api/v2/archive/items', headers={'Authorization': 'Bearer wrong'}),
            # Wrong method and unknown paths are decided by routing, before the token.
            _req('DELETE', '/api/v2/jobs', headers={}),
            _req('PUT', '/api/v2/job-requests', headers={}),
            _req('PATCH', '/api/v2/settings/web.jandan', headers={}),
            _req('GET', '/api/v2/jobs/', headers={}),
        ],
    )
    add('jobs from default settings', [_req('GET', '/api/v2/jobs')])
    add(
        'jobs follow the stored settings',
        [_req('GET', '/api/v2/jobs')],
        settings={
            'web.bilibili': _BILIBILI_COMPLETE,
            'web.jandan': {'enabled': True, 'notify': False, 'cron': '5 4 * * 1'},
            'web.pixiv': {'enabled': True},
        },
    )
    add(
        'job requests',
        [
            _json_req('POST', '/api/v2/job-requests', {'target': 'telegram'}),
            _json_req('POST', '/api/v2/job-requests', {'target': 'azurlane'}),
            _json_req('POST', '/api/v2/job-requests', {'target': 'all'}),
            _json_req('POST', '/api/v2/job-requests', {'target': 'not-a-source'}),
            _json_req('POST', '/api/v2/job-requests', {'target': 'Jandan'}),
            _json_req('POST', '/api/v2/job-requests', {'target': 'jandan', 'extra': 1}),
            _json_req('POST', '/api/v2/job-requests', {}),
            _json_req('POST', '/api/v2/job-requests', [1]),
            _json_req('POST', '/api/v2/job-requests', 'jandan'),
            _json_req('POST', '/api/v2/job-requests', {'target': 5}),
            _json_req('POST', '/api/v2/job-requests', None),
            _req('POST', '/api/v2/job-requests'),
            _req('POST', '/api/v2/job-requests', headers={**AUTH, 'content-type': 'application/json'}, body=''),
            _req('POST', '/api/v2/job-requests', headers={**AUTH, 'content-type': 'application/json'}, body='{"target": "jandan",}'),
            _req('POST', '/api/v2/job-requests', headers={**AUTH, 'content-type': 'application/json'}, body='  '),
            _req('POST', '/api/v2/job-requests', headers={**AUTH, 'content-type': 'application/json'}, body='{"target":"jandan"} x'),
            _req(
                'POST',
                '/api/v2/job-requests',
                headers={**AUTH, 'content-type': 'application/json; charset=utf-8'},
                body='{"target":"jandan"}',
            ),
            _req('POST', '/api/v2/job-requests', headers={**AUTH, 'content-type': 'application/vnd.fav+json'}, body='{"target":"jandan"}'),
            _req('POST', '/api/v2/job-requests', headers={**AUTH, 'content-type': 'text/plain'}, body='{"target":"jandan"}'),
            _req('POST', '/api/v2/job-requests', headers=AUTH, body='{"target":"jandan"}'),
            _req(
                'POST',
                '/api/v2/job-requests',
                headers={**AUTH, 'content-type': 'application/json'},
                body='{"target":"jandan","target":"kemono"}',
            ),
            _req('POST', '/api/v2/job-requests', headers={**AUTH, 'content-type': 'application/json'}, body='{"target":NaN}'),
            _req('POST', '/api/v2/job-requests', headers={**AUTH, 'content-type': 'application/json'}, body='["a",'),
            _req('GET', '/api/v2/job-requests/1'),
            _req('GET', '/api/v2/job-requests/4'),
            _req('GET', '/api/v2/job-requests/0'),
            _req('GET', '/api/v2/job-requests/-3'),
            _req('GET', '/api/v2/job-requests/abc'),
            _req('GET', '/api/v2/job-requests/1.0'),
            _req('GET', '/api/v2/job-requests/%201'),
            _req('GET', '/api/v2/job-requests/99999999999999999999'),
        ],
    )
    add(
        'job request listing',
        [
            _req('GET', '/api/v2/job-requests'),
            _req('GET', '/api/v2/job-requests?limit=2'),
            _req('GET', '/api/v2/job-requests?status=failed&status=rejected&limit=10'),
            _req('GET', '/api/v2/job-requests?status=pending'),
            _req('GET', '/api/v2/job-requests?status=interrupted'),
            _req('GET', '/api/v2/job-requests?status=bogus'),
            _req('GET', '/api/v2/job-requests?status=bogus&status=failed&status=&limit=0'),
            _req('GET', '/api/v2/job-requests?limit=201'),
            _req('GET', '/api/v2/job-requests?limit=abc'),
            _req('GET', '/api/v2/job-requests?limit=5&limit=1'),
            _req('GET', '/api/v2/job-requests?limit=+3'),
            _req('GET', '/api/v2/job-requests?limit=2.0'),
            _req('GET', '/api/v2/job-requests?limit=2.5'),
            _req('GET', '/api/v2/job-requests?limit='),
            _req('GET', '/api/v2/job-requests/9'),
            _req('GET', '/api/v2/job-requests/8'),
            _req('GET', '/api/v2/job-requests/7'),
        ],
        control_requests=_REQUESTS_ROWS,
    )
    add(
        'settings from defaults',
        [
            _req('GET', '/api/v2/settings'),
            _req('GET', '/api/v2/settings/web.jandan'),
            _req('GET', '/api/v2/settings/web.nope'),
            _req('GET', '/api/v2/settings/cookiecloud'),
            _req('GET', '/api/v2/settings/Web.Jandan'),
            _req('GET', '/api/v2/settings/web..jandan'),
            _req('GET', '/api/v2/settings/' + 'a' * 65),
            _req('GET', '/api/v2/settings/web.jandan/extra'),
            _req('PUT', '/api/v2/settings/web.nope', headers={**AUTH, 'content-type': 'application/json'}, body='{}'),
        ],
    )
    add(
        'settings with secrets are masked',
        [
            _req('GET', '/api/v2/settings'),
            _req('GET', '/api/v2/settings/notifications.telegram'),
            _req('GET', '/api/v2/settings/web.telegram'),
            _req('GET', '/api/v2/settings/services.tikhub'),
            _req('GET', '/api/v2/settings/web.wechat'),
            _req('GET', '/api/v2/jobs'),
        ],
        settings={
            'notifications.telegram': {'enabled': True, 'bot_token': '123456:secret', 'chat_id': '-100123', 'message_thread_id': 7},
            'web.telegram': _TELEGRAM_ACCOUNTS,
            'services.tikhub': {'token': 'abc'},
            'web.wechat': {'token': 'wechat-token', 'api_url': 'http://favinbox:9970'},
            'web.rednote': {'proxy': 'http://user:pw@home.example:3128'},
        },
    )
    add(
        'settings updates',
        [
            # Masked and omitted secrets keep what is stored; a new value replaces it.
            _json_req(
                'PUT',
                '/api/v2/settings/notifications.telegram',
                {'enabled': True, 'bot_token': '1234••••', 'chat_id': '-100456', 'message_thread_id': None},
            ),
            _json_req('PUT', '/api/v2/settings/notifications.telegram', {'chat_id': '-100789'}),
            _json_req('PUT', '/api/v2/settings/notifications.telegram', {'bot_token': 'brand-new-token', 'chat_id': '-1'}),
            _req('GET', '/api/v2/settings/notifications.telegram'),
            _json_req(
                'PUT',
                '/api/v2/settings/web.telegram',
                {
                    'enabled': False,
                    'accounts': [
                        {'name': 'bob', 'api_id': 2, 'api_hash': 'bh••••', 'channels': []},
                        {'name': 'carol', 'api_id': 3, 'api_hash': 'carol-hash', 'channels': []},
                        {'name': 'alice', 'api_id': 1, 'api_hash': 'hash••••', 'channels': []},
                        {'name': 'dave', 'api_id': 4, 'channels': []},
                    ],
                },
            ),
            _json_req('PUT', '/api/v2/settings/web.telegram', {'enabled': False, 'accounts': 'not a list'}),
            # The enable guard and validation.
            _json_req('PUT', '/api/v2/settings/web.bilibili', {'cron': '0 * * * *', 'enabled': True}),
            _json_req('PUT', '/api/v2/settings/web.bilibili', {'cron': '0 * * * *', 'enabled': False}),
            _json_req('PUT', '/api/v2/settings/web.bilibili', _BILIBILI_COMPLETE),
            _json_req('PUT', '/api/v2/settings/web.bilibili', {'cron': 'bad'}),
            _json_req(
                'PUT', '/api/v2/settings/web.bilibili', {'cron': '* * * * *', 'enabled': 'yes please', 'accounts': [{'favorites': 1}]}
            ),
            _json_req('PUT', '/api/v2/settings/web.jandan', {'enabled': True, 'unknown_field': 1, 'cron': '1 2 3 4 5'}),
            _json_req('PUT', '/api/v2/settings/services.tikhub', {'token': 'tikhub-secret', 'api_url': 'https://api.example.com'}),
            _json_req('PUT', '/api/v2/settings/web.jandan', [1]),
            _json_req('PUT', '/api/v2/settings/web.jandan', 'text'),
            _json_req('PUT', '/api/v2/settings/web.jandan', None),
            _req('PUT', '/api/v2/settings/web.jandan'),
            _req('PUT', '/api/v2/settings/web.jandan', headers={**AUTH, 'content-type': 'text/plain'}, body='{}'),
            _req('GET', '/api/v2/settings'),
            _req('GET', '/api/v2/jobs'),
        ],
        settings={
            'notifications.telegram': {'enabled': False, 'bot_token': '123456:secret', 'chat_id': '-100123', 'message_thread_id': None},
            'web.telegram': _TELEGRAM_ACCOUNTS,
        },
    )
    add(
        'telegram notification test without a bot',
        [
            _req('POST', '/api/v2/notifications/telegram/test', headers={**AUTH, 'content-type': 'application/json'}, body='{}'),
            _req('POST', '/api/v2/notifications/telegram/test'),
            _req('GET', '/api/v2/notifications/telegram/test'),
        ],
        settings={'notifications.telegram': {'enabled': True, 'bot_token': '', 'chat_id': '-100'}},
    )
    add(
        'azurlane proxy test',
        [
            _json_req(
                'POST',
                '/api/v2/azurlane/proxy/test',
                {'origin_proxy': 'http://user:pass@proxy.example:8080', 'origin_connect_host': 'static.l2d.su'},
            ),
            _json_req('POST', '/api/v2/azurlane/proxy/test', {}),
            _json_req('POST', '/api/v2/azurlane/proxy/test', {'origin_proxy': 8080, 'origin_connect_host': None, 'extra': 1}),
            _json_req('POST', '/api/v2/azurlane/proxy/test', [1]),
            _json_req('POST', '/api/v2/azurlane/proxy/test', None),
            _req('POST', '/api/v2/azurlane/proxy/test'),
            _req('POST', '/api/v2/azurlane/proxy/test', headers={**AUTH, 'content-type': 'text/plain'}, body='{}'),
            _req('POST', '/api/v2/azurlane/proxy/test', headers={'content-type': 'application/json'}, body='{}'),
            _req('GET', '/api/v2/azurlane/proxy/test'),
        ],
    )
    add(
        'archive request validation',
        [
            _req('GET', '/api/v2/archive/items'),
            _req('GET', '/api/v2/archive/items?source=&q='),
            _req('GET', '/api/v2/archive/items?source=nope'),
            _req('GET', '/api/v2/archive/items?source=' + 'x' * 41),
            _req('GET', '/api/v2/archive/items?source=jandan&q=' + 'q' * 201),
            _req('GET', '/api/v2/archive/items?source=jandan&limit=0&offset=-1'),
            _req('GET', '/api/v2/archive/items?source=jandan&limit=201'),
            _req('GET', '/api/v2/archive/items?source=nope&limit=x'),
            _req('GET', '/api/v2/archive/items?source=Bilibili'),
        ],
    )
    add(
        'site sessions',
        [_req('GET', '/api/v2/site-sessions'), _req('GET', '/api/v2/site-sessions', headers={})],
        settings={'web.bilibili': {'accounts': [{'name': 'main'}, {'name': 'alt'}]}},
        site_sessions=_SESSIONS,
    )
    add(
        'site sessions with one bilibili account and none stored',
        [_req('GET', '/api/v2/site-sessions')],
        settings={'web.bilibili': {'accounts': [{'name': 'main'}]}},
    )
    client_auth = {'Authorization': f'Bearer {CLIENT_TOKEN}'}
    add(
        'credentials',
        [
            _req('GET', '/api/v2/credentials/bilibili-main', headers=client_auth),
            _req('GET', '/api/v2/credentials/bilibili-main', headers={}),
            _req('GET', '/api/v2/credentials/bilibili-main', headers={'Authorization': 'Basic x'}),
            _req('GET', '/api/v2/credentials/bilibili-main', headers={'Authorization': 'Bearer '}),
            _req('GET', '/api/v2/credentials/bilibili-main', headers={'Authorization': 'Bearer other'}),
            _req('GET', '/api/v2/credentials/bilibili-main'),
            _req('GET', '/api/v2/credentials/twitter', headers=client_auth),
            _req('GET', '/api/v2/credentials/rednote', headers=client_auth),
            _req('GET', '/api/v2/credentials/pixiv', headers=client_auth),
            _req('GET', '/api/v2/credentials/stale-site', headers={'Authorization': 'Bearer second-client'}),
            _req('GET', '/api/v2/credentials/bad%20site', headers=client_auth),
            _req('GET', '/api/v2/credentials/bad%20site', headers={}),
        ],
        site_sessions=_SESSIONS,
        credential_clients=(
            f'rsshub:{CLIENT_DIGEST}:bilibili-main,rednote,pixiv,twitter2; second:{hashlib.sha256(b"second-client").hexdigest()}:stale-site'
        ),
    )
    origin = 'https://game-view.s117.me'
    add(
        'cors',
        [
            _req(
                'OPTIONS',
                '/api/v2/jobs',
                headers={'Origin': origin, 'Access-Control-Request-Method': 'PUT', 'Access-Control-Request-Headers': 'Authorization'},
            ),
            _req(
                'OPTIONS',
                '/api/v2/jobs',
                headers={'Origin': 'https://evil.example', 'Access-Control-Request-Method': 'GET'},
            ),
            _req(
                'OPTIONS',
                '/api/v2/jobs',
                headers={'Origin': origin, 'Access-Control-Request-Method': 'PATCH', 'Access-Control-Request-Headers': 'X-Other'},
            ),
            _req('OPTIONS', '/api/v2/jobs', headers={'Origin': origin}),
            _req('GET', '/api/v2/jobs', headers={**AUTH, 'Origin': origin}),
            _req('GET', '/api/v2/jobs', headers={'Origin': origin}),
            _req('GET', '/healthz', headers={'Origin': 'https://evil.example'}),
        ],
        cors_origins=[origin, 'http://localhost:5173'],
        cors_allow_credentials=True,
    )
    add(
        'no cors by default',
        [
            _req(
                'OPTIONS',
                '/api/v2/jobs',
                headers={'Origin': origin, 'Access-Control-Request-Method': 'GET'},
            ),
            _req('GET', '/healthz', headers={'Origin': origin}),
        ],
    )
    return scenarios


def _replay(scenario: dict[str, Any]) -> list[dict[str, Any]]:
    setup = scenario['setup']
    state = _State(
        rows={section: json.loads(json.dumps(value)) for section, value in setup.get('settings', {}).items()},
        requests=[_request_row(row) for row in setup.get('control_requests', [])],
        sessions={row['site']: _session_row(row) for row in setup.get('site_sessions', [])},
    )
    config = ApiConfig(
        dsn='postgresql://goldens@127.0.0.1:1/never-connected',
        token=TOKEN,
        cors_origins=tuple(setup.get('cors_origins', ())),
        cors_allow_credentials=bool(setup.get('cors_allow_credentials', False)),
        credential_clients=setup.get('credential_clients', ''),
    )
    results: list[dict[str, Any]] = []
    with _patched(state):
        app = create_app(config=config, service=_service(state), browser_login=BrowserLoginManager(login_sites))
        with TestClient(app, raise_server_exceptions=True, follow_redirects=False) as client:
            for request in scenario['requests']:
                response = client.request(
                    request['method'],
                    request['path'],
                    headers=request['headers'],
                    content=request['body'].encode() if 'body' in request else None,
                )
                headers = {name: response.headers[name] for name in _RESPONSE_HEADERS if name in response.headers}
                headers.update({name: value for name, value in response.headers.items() if name.startswith('access-control-')})
                result: dict[str, Any] = {'status': response.status_code, 'headers': headers}
                if response.headers.get('content-type', '').startswith('application/json'):
                    result['json'] = response.json()
                else:
                    result['text'] = response.text
                if request['method'] == 'POST' and request['path'] == '/api/v2/job-requests' and response.status_code == 202:  # noqa: PLR2004
                    result['volatile'] = ['id', 'requested_at']
                results.append({'request': request, 'response': result})
    return results


def _requests() -> list[dict[str, Any]]:
    return [{'name': scenario['name'], 'setup': scenario['setup'], 'exchanges': _replay(scenario)} for scenario in _scenarios()]


def _openapi() -> list[dict[str, Any]]:
    state = _State()
    with _patched(state):
        app = create_app(service=_service(state), browser_login=BrowserLoginManager({}), credential_clients=())
        return [app.openapi()]


def _docs() -> list[dict[str, Any]]:
    state = _State()
    with _patched(state):
        app = create_app(service=_service(state), browser_login=BrowserLoginManager({}), credential_clients=())
        with TestClient(app) as client:
            cases = []
            for path in ('/docs', '/docs/oauth2-redirect', '/redoc'):
                response = client.get(path)
                cases.append(
                    {'path': path, 'status': response.status_code, 'content_type': response.headers['content-type'], 'body': response.text},
                )
            return cases


# ---------- helpers called directly ----------

_MASK_SECTIONS = [
    ('notifications.telegram', {'enabled': True, 'bot_token': '123456:secret', 'chat_id': '-100123', 'message_thread_id': None}),
    ('notifications.telegram', {'bot_token': 'abc'}),
    ('notifications.telegram', {'bot_token': 'abcd'}),
    ('notifications.telegram', {'bot_token': 'abcde'}),
    ('notifications.telegram', {'bot_token': ''}),
    ('notifications.telegram', {'bot_token': None}),
    ('notifications.telegram', {'bot_token': 5}),
    ('notifications.telegram', {'chat_id': '1'}),
    ('notifications.telegram', {'bot_token': '令牌令牌令牌'}),
    ('notifications.telegram', {'bot_token': '😀😀😀😀😀'}),
    ('web.wechat', {'api_url': 'http://favinbox', 'token': 'secret-token'}),
    ('services.tikhub', {'token': 'tk', 'api_url': 'x'}),
    ('web.telegram', {'accounts': [{'name': 'a', 'api_hash': 'hash-A'}, {'name': 'b', 'api_hash': ''}, 'junk', {'name': 'c'}]}),
    ('web.telegram', {'accounts': 'not a list'}),
    ('web.telegram', {}),
    ('web.rednote', {'user_id': '', 'proxy': 'http://user:pw@home.example:3128'}),
    ('cookiecloud', {'configs': [{'name': 'main', 'password': 'p'}]}),
]

_UNMASK_CASES: list[tuple[str, dict[str, Any], dict[str, Any]]] = [
    ('notifications.telegram', {'enabled': True, 'bot_token': '1234••••', 'chat_id': '-100456'}, {'bot_token': '123456:secret'}),
    ('notifications.telegram', {'bot_token': 'real••••'}, {'bot_token': 'real-token'}),
    ('notifications.telegram', {'chat_id': '-100'}, {'bot_token': 'real-token'}),
    ('notifications.telegram', {'bot_token': None}, {'bot_token': 'real-token'}),
    ('notifications.telegram', {'bot_token': ''}, {'bot_token': 'real-token'}),
    ('notifications.telegram', {'bot_token': 'brand-new'}, {'bot_token': 'real-token'}),
    ('notifications.telegram', {'bot_token': 'x••••'}, {}),
    ('notifications.telegram', {'bot_token': 'x••••'}, {'bot_token': None}),
    ('notifications.telegram', {'bot_token': 7}, {'bot_token': 'real-token'}),
    ('web.wechat', {'token': '••••'}, {'token': 'w'}),
    ('services.tikhub', {}, {'token': 'tk'}),
    (
        'web.telegram',
        {'accounts': [{'name': 'bob', 'api_hash': 'bh••••'}, {'name': 'alice', 'api_hash': 'hash••••'}, {'name': 'new'}, 'junk']},
        {'accounts': [{'name': 'alice', 'api_hash': 'hash-ALICE'}, {'name': 'bob', 'api_hash': 'hash-BOB'}]},
    ),
    ('web.telegram', {'accounts': 'not a list'}, {'accounts': [{'name': 'a', 'api_hash': 'h'}]}),
    ('web.telegram', {}, {'accounts': [{'name': 'a', 'api_hash': 'h'}]}),
    ('web.telegram', {'accounts': [{'api_hash': '••••'}]}, {'accounts': [{'api_hash': 'nameless'}]}),
    ('web.rednote', {'proxy': 'http://other.example:3128'}, {'proxy': 'http://user:pw@home.example:3128'}),
    ('web.jandan', {'enabled': True}, {'enabled': False}),
]


def _masking() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = [
        {'op': 'mask', 'section': section, 'payload': payload, 'result': mask_section(section, payload)}
        for section, payload in _MASK_SECTIONS
    ]
    cases.extend(
        {'op': 'unmask', 'section': section, 'payload': payload, 'stored': stored, 'result': unmask_section(section, payload, stored)}
        for section, payload, stored in _UNMASK_CASES
    )
    return cases


_CLIENT_VALUES = [
    '',
    '   ',
    f'rsshub:{CLIENT_DIGEST}:bilibili-main',
    f'rsshub:{CLIENT_DIGEST}:bilibili-main,bilibili-alt ; other:{"a" * 64}:pixiv',
    f'rsshub:{CLIENT_DIGEST.upper()}:a,,b,',
    f'a:{"b" * 64}:x\nb:{"c" * 64}:y\t;;c:{"d" * 64}:z',
    'rsshub',
    f'rsshub:{CLIENT_DIGEST}:',
    f'rsshub:{CLIENT_DIGEST}:,',
    'rsshub:nothex:bilibili-main',
    f'rs hub:{CLIENT_DIGEST}:x',
    f':{CLIENT_DIGEST}:x',
    f'rsshub:{CLIENT_DIGEST}:x:y',
    f'rsshub:{CLIENT_DIGEST}:bad site',
    f'rsshub:{CLIENT_DIGEST}:ok,b@d',
    f'name:{"a" * 63}:x',
    f'名字:{"a" * 64}:x',
]


def _credential_clients() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for raw in _CLIENT_VALUES:
        case: dict[str, Any] = {'raw': raw}
        try:
            case['clients'] = [{'name': c.name, 'token_sha256': c.token_sha256, 'sites': sorted(c.sites)} for c in parse_clients(raw)]
        except ValueError as exc:
            case['error'] = str(exc)
        cases.append(case)
    return cases


def _login_sites() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for accounts in ([], ['main'], ['main', 'alt'], ['a_b', 'X-2', 'third']):
        current = settings.Settings()
        current.web.bilibili = settings.Bilibili.model_validate({'accounts': [{'name': name} for name in accounts]})
        settings.use(current)
        try:
            sites = [
                {'key': key, 'label': site.label, 'hint': site.hint, 'session_cookie': site.session_cookie}
                for key, site in login_sites().items()
            ]
        finally:
            settings.use(settings.Settings())
        cases.append({'bilibili_accounts': accounts, 'sites': sites})
    return cases


def _typed(value: Any) -> Any:
    """How a row value is described to the Rust test, which inserts it into the real table."""
    if isinstance(value, datetime):
        return {'timestamp': value.isoformat()}
    if isinstance(value, date):
        return {'date': value.isoformat()}
    return value


_ARCHIVE_ROWS: dict[str, list[dict[str, Any]]] = {
    'bilibili': [
        {'bvid': 'BV1xx', 'title': '标题', 'upper': 'UP主', 'fav_id': 123, 'created_at': datetime(2026, 3, 1, 12, 0, 0)},  # noqa: DTZ001
        {'bvid': 'BV2yy', 'title': '', 'upper': '', 'fav_id': 0, 'created_at': None},
    ],
    'telegram': [
        {
            'account_name': 'main',
            'channel_id': -1001234,
            'message_id': 55,
            'title': 'clip',
            'channel_name': 'Channel',
            'media_type': 'photo',
            'created_at': datetime(2026, 3, 1, 12, 0, 0, 123456),  # noqa: DTZ001
        },
    ],
    'hanime1': [
        {
            'id': '12345',
            'title': 'Video',
            'uploader': None,
            'release_date': '2024-01-01',
            'created_at': datetime(2026, 1, 2, 3, 4, 5),  # noqa: DTZ001
        },
        {'id': '777', 'title': 'Other', 'uploader': '  spaced  ', 'release_date': '', 'created_at': datetime(2026, 1, 2, 3, 4, 6)},  # noqa: DTZ001
    ],
    'jandan': [
        {
            'fav_type': 1,
            'pic_id': 6000001,
            'content_index': 0,
            'author': 'someone',
            'content_url': 'https://img.example/a.jpg',
            'post_date': '2026-02-01',
            'local_path': 'collection/jandan/a.jpg',
            'downloaded': 1,
            'unavailable': 0,
            'last_error': '',
            'created_at': datetime(2026, 2, 1, 0, 0, 0),  # noqa: DTZ001
        },
        {
            'fav_type': 2,
            'pic_id': 6000002,
            'content_index': 3,
            'author': '',
            'content_url': '',
            'post_date': '',
            'local_path': '',
            'downloaded': 0,
            'unavailable': 1,
            'last_error': 'gone',
            'created_at': datetime(2026, 2, 1, 0, 0, 1),  # noqa: DTZ001
        },
    ],
    'twitter': [
        {
            'tweet_id': 1890000000000000001,
            'num': 2,
            'content': 'tweet text',
            'author': 'handle',
            'author_nick': 'Nick',
            'tweet_date': '2026-02-02 10:00',
            'media_type': 'video',
            'local_path': 'x/a.mp4',
            'created_at': datetime(2026, 2, 2, 10, 0, 0),  # noqa: DTZ001
        },
    ],
    'rednote': [
        {
            'note_id': '64abc',
            'media_index': 0,
            'title': '笔记',
            'author': '作者',
            'media_type': 'image',
            'published_at': '2026-02-03',
            'local_path': 'r/a.webp',
            'downloaded': 1,
            'unavailable': 0,
            'last_error': '',
            'xsec_token': 'ABC=',
            'created_at': datetime(2026, 2, 3, 0, 0, 0),  # noqa: DTZ001
        },
        {
            'note_id': '64def',
            'media_index': 1,
            'title': '',
            'author': '',
            'media_type': 'video',
            'published_at': '',
            'local_path': '',
            'downloaded': 0,
            'unavailable': 0,
            'last_error': '',
            'xsec_token': '',
            'created_at': datetime(2026, 2, 3, 0, 0, 1),  # noqa: DTZ001
        },
    ],
    'pixiv': [
        {
            'illust_id': 120000001,
            'num': 0,
            'title': 'art',
            'author': 'painter',
            'author_id': '42',
            'illust_type': 2,
            'local_path': 'p/a.webp',
            'downloaded': 1,
            'unavailable': 0,
            'last_error': '',
            'created_at': datetime(2026, 2, 4, 0, 0, 0),  # noqa: DTZ001
        },
    ],
    'wechat': [
        {
            'msg_id': 100,
            'create_time': 1767225600,
            'local_path': 'w/file.pdf',
            'sender': 'friend',
            'sent_at': '2026-01-01 08:00:00',
            'size': None,
            'created_at': datetime(2026, 2, 5, 0, 0, 0),  # noqa: DTZ001
        },
    ],
    'wechat_article': [
        {
            'msg_id': 101,
            'create_time': 1767225601,
            'title': '文章',
            'account': '公众号',
            'sender': '',
            'sent_at': '2026-01-01 08:00:01',
            'published_at': '',
            'image_count': 3,
            'local_path': 'w/a',
            'url': 'https://mp.weixin.qq.com/s/abc',
            'created_at': datetime(2026, 2, 5, 0, 0, 1),  # noqa: DTZ001
        },
        {
            'msg_id': 102,
            'create_time': 1767225602,
            'title': '',
            'account': '',
            'sender': '',
            'sent_at': '',
            'published_at': '',
            'image_count': 0,
            'local_path': '',
            'url': '',
            'created_at': datetime(2026, 2, 5, 0, 0, 2),  # noqa: DTZ001
        },
    ],
    'wechat_channel_video': [
        {
            'msg_id': 103,
            'create_time': 1767225603,
            'title': '视频',
            'author': '作者',
            'sender': 's',
            'sent_at': '2026-01-01',
            'duration': 61,
            'size': 1048576,
            'local_path': 'w/v.mp4',
            'object_id': 'obj1',
            'created_at': datetime(2026, 2, 5, 0, 0, 3),  # noqa: DTZ001
        },
    ],
    'kemono': [
        {
            'id': '9001',
            'title': 'post',
            'creator': 'artist',
            'service': 'fanbox',
            'user_id': '70050825',
            'published_at': '2026-01-05T00:00:00',
            'created_at': datetime(2026, 2, 6, 0, 0, 0),  # noqa: DTZ001
        },
        {
            'id': '9002',
            'title': 'post 2',
            'creator': None,
            'service': 'patreon',
            'user_id': '1',
            'published_at': None,
            'created_at': datetime(2026, 2, 6, 0, 0, 1),  # noqa: DTZ001
        },
    ],
}


def _archive() -> list[dict[str, Any]]:
    """The whitelist, and `_serialize` of rows as the real tables would return them."""
    cases: list[dict[str, Any]] = [
        {
            'sources': [
                {
                    'key': source.key,
                    'name': source.name,
                    'table': source.table,
                    'id_columns': list(source.id_columns),
                    'title_column': source.title_column,
                    'columns': list(source.columns),
                    'search_columns': list(source.search_columns),
                    'time_column': source.time_column,
                    'subtitle_columns': list(source.subtitle_columns),
                    'selected_columns': list(source.selected_columns),
                }
                for source in archive.ARCHIVE_SOURCES.values()
            ],
        },
    ]
    for key, rows in _ARCHIVE_ROWS.items():
        source = archive.ARCHIVE_SOURCES[key]
        for row in rows:
            ordered = {column: row.get(column) for column in source.selected_columns}
            cases.append(
                {
                    'source': key,
                    'row': {column: _typed(value) for column, value in ordered.items()},
                    'item': archive._serialize(source, ordered),  # noqa: SLF001
                },
            )
    cases.extend(
        {'note_id': note_id, 'xsec_token': token, 'url': build_note_url(note_id, token)}
        for note_id, token in [('64abc', ''), ('64abc', 'ABC='), ('id with space', 'a&b')]
    )
    return cases


GOLDENS = {
    'api/openapi.json': _openapi,
    'api/docs.json': _docs,
    'api/requests.json': _requests,
    'api/masking.json': _masking,
    'api/credential_clients.json': _credential_clients,
    'api/login_sites.json': _login_sites,
    'api/archive.json': _archive,
}
