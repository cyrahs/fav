# ruff: noqa: INP001, S101, S105, ANN001, ANN201, ANN202, ARG002, PLR2004

import hashlib
from datetime import UTC, datetime

import pytest
from fastapi.testclient import TestClient

from src.api import credentials
from src.api.app import create_app
from src.api.browser_login import BrowserLoginManager
from src.api.credentials import CredentialClient, parse_clients
from src.tool.site_sessions import VALUE_IN_PROFILE, SiteSession, StoredCookie

TOKEN = 'rsshub-token'
DIGEST = hashlib.sha256(TOKEN.encode()).hexdigest()


class _Service:
    def authenticate(self, header) -> None:
        return None

    def close(self) -> None:
        return None


class _Sessions:
    VALUE_IN_PROFILE = VALUE_IN_PROFILE

    def __init__(self, sessions: dict) -> None:
        self.sessions = sessions

    async def load(self, site):
        return self.sessions.get(site)


def _session(site: str, cookies) -> SiteSession:
    return SiteSession(
        site=site,
        cookies=cookies,
        account='某人 (42)',
        status='valid',
        message='',
        expires_at=None,
        signed_in_at=datetime(2026, 11, 3, tzinfo=UTC),
        checked_at=None,
        extra={'refresh_token': 'never-served'},
    )


@pytest.fixture
def client(monkeypatch):
    sessions = _Sessions(
        {
            'bilibili-main': _session('bilibili-main', (StoredCookie(name='SESSDATA', value='s', domain='.bilibili.com', expires=2e9),)),
            'rednote': _session('rednote', (StoredCookie(name='web_session', value=VALUE_IN_PROFILE, domain='.xiaohongshu.com'),)),
        }
    )
    monkeypatch.setattr(credentials, 'site_sessions', sessions)
    clients = (CredentialClient(name='rsshub', token_sha256=DIGEST, sites=frozenset({'bilibili-main', 'rednote', 'pixiv'})),)
    app = create_app(service=_Service(), browser_login=BrowserLoginManager({}), credential_clients=clients)
    with TestClient(app) as test_client:
        yield test_client


def test_a_client_reads_the_cookies_of_its_site(client) -> None:
    response = client.get('/api/v2/credentials/bilibili-main', headers={'Authorization': f'Bearer {TOKEN}'})

    assert response.status_code == 200
    assert response.headers['cache-control'] == 'no-store'
    body = response.json()
    assert body['account'] == '某人 (42)'
    assert body['cookies'] == [{'name': 'SESSDATA', 'value': 's', 'domain': '.bilibili.com', 'path': '/', 'expires': 2e9}]
    assert 'never-served' not in response.text


def test_the_api_token_is_not_a_credential_client(client) -> None:
    assert client.get('/api/v2/credentials/bilibili-main').status_code == 401
    assert client.get('/api/v2/credentials/bilibili-main', headers={'Authorization': 'Bearer other'}).status_code == 403


def test_a_client_is_limited_to_its_sites(client) -> None:
    response = client.get('/api/v2/credentials/twitter', headers={'Authorization': f'Bearer {TOKEN}'})

    assert response.status_code == 403
    assert response.json()['error']['code'] == 'site_not_allowed'


def test_sessions_without_values_or_rows_are_not_found(client) -> None:
    headers = {'Authorization': f'Bearer {TOKEN}'}

    assert client.get('/api/v2/credentials/rednote', headers=headers).status_code == 404
    assert client.get('/api/v2/credentials/pixiv', headers=headers).status_code == 404


def test_clients_parse_from_the_environment_value() -> None:
    raw = f'rsshub:{DIGEST}:bilibili-main,bilibili-alt ; other:{"a" * 64}:pixiv'

    clients = parse_clients(raw)

    assert [c.name for c in clients] == ['rsshub', 'other']
    assert clients[0].sites == frozenset({'bilibili-main', 'bilibili-alt'})
    assert parse_clients('') == ()


@pytest.mark.parametrize('raw', ['rsshub', f'rsshub:{DIGEST}:', 'rsshub:nothex:bilibili-main', f'rs hub:{DIGEST}:x'])
def test_malformed_clients_are_refused(raw) -> None:
    with pytest.raises(ValueError, match='CREDENTIAL_CLIENTS'):
        parse_clients(raw)
