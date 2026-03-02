# ruff: noqa: INP001, S101, S105, S106

import json
from datetime import UTC, datetime
from http import HTTPStatus
from types import SimpleNamespace

import pytest

import src.api.server as api_server

_FIXED_NOW = datetime(2026, 3, 2, 0, 0, tzinfo=UTC)
_VALID_TOKEN = 'token-for-tests'
_PRIMARY_BIND = '127.0.0.3'
_PRIMARY_PORT = 18091


def _build_service(*, fetcher, token: str) -> api_server.FavApiService:  # noqa: ANN001
    return api_server.FavApiService(
        dsn='postgresql://db.local/fav',
        token=token,
        hanime1_fetcher=fetcher,
        now_provider=lambda: _FIXED_NOW,
    )


def _json_body(response: api_server.ApiResponse) -> dict[str, object]:
    assert response.body is not None
    return json.loads(response.body.decode('utf-8'))


def _mock_app_config(*, dsn: str, token: str, bind: str = _PRIMARY_BIND, port: int = _PRIMARY_PORT) -> SimpleNamespace:
    return SimpleNamespace(
        database=SimpleNamespace(postgres_dsn=dsn),
        api=SimpleNamespace(token=token, bind=bind, port=port),
    )


def test_health_endpoint_returns_ok() -> None:
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN)

    response = service.handle_request(method='GET', path='/api/v1/health', headers={})

    assert response.status == HTTPStatus.OK
    assert _json_body(response) == {'status': 'ok', 'generated_at': '2026-03-02T00:00:00Z'}


def test_hanime1_downloaded_ids_requires_authorization_header() -> None:
    service = _build_service(fetcher=lambda _dsn: ['1001'], token=_VALID_TOKEN)

    response = service.handle_request(
        method='GET',
        path='/api/v1/hanime1/downloaded-ids',
        headers={},
    )

    assert response.status == HTTPStatus.UNAUTHORIZED
    assert response.headers['WWW-Authenticate'] == 'Bearer realm="fav-api"'
    assert _json_body(response) == {'error': 'missing_authorization'}


def test_hanime1_downloaded_ids_rejects_invalid_token() -> None:
    service = _build_service(fetcher=lambda _dsn: ['1001'], token=_VALID_TOKEN)

    response = service.handle_request(
        method='GET',
        path='/api/v1/hanime1/downloaded-ids',
        headers={'Authorization': 'Bearer wrong-token'},
    )

    assert response.status == HTTPStatus.FORBIDDEN
    assert _json_body(response) == {'error': 'invalid_token'}


def test_hanime1_downloaded_ids_returns_empty_payload_for_empty_table() -> None:
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN)

    response = service.handle_request(
        method='GET',
        path='/api/v1/hanime1/downloaded-ids',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )

    payload = _json_body(response)

    assert response.status == HTTPStatus.OK
    assert response.headers['Cache-Control'] == 'private, max-age=60'
    assert response.headers['Content-Type'] == 'application/json; charset=utf-8'
    assert response.headers['ETag'].startswith('"')
    assert payload == {
        'ids': [],
        'count': 0,
        'generated_at': '2026-03-02T00:00:00Z',
    }


def test_hanime1_downloaded_ids_returns_304_when_etag_matches() -> None:
    service = _build_service(fetcher=lambda _dsn: ['1001', '1002'], token=_VALID_TOKEN)

    first = service.handle_request(
        method='GET',
        path='/api/v1/hanime1/downloaded-ids',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )
    etag = first.headers['ETag']

    second = service.handle_request(
        method='GET',
        path='/api/v1/hanime1/downloaded-ids',
        headers={
            'Authorization': f'Bearer {_VALID_TOKEN}',
            'If-None-Match': etag,
        },
    )

    assert second.status == HTTPStatus.NOT_MODIFIED
    assert second.headers['ETag'] == etag
    assert second.body is None


def test_hanime1_downloaded_ids_hides_internal_error_details() -> None:
    def _raise_db_error(_dsn: str) -> list[str]:
        msg = 'password=my-secret and host=internal-db'
        raise RuntimeError(msg)

    service = _build_service(fetcher=_raise_db_error, token=_VALID_TOKEN)

    response = service.handle_request(
        method='GET',
        path='/api/v1/hanime1/downloaded-ids',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )

    payload = _json_body(response)
    body_text = response.body.decode('utf-8') if response.body else ''

    assert response.status == HTTPStatus.INTERNAL_SERVER_ERROR
    assert payload == {'error': 'internal_server_error'}
    assert 'password=my-secret' not in body_text


def test_load_config_from_settings_reads_database_postgres_dsn_and_api_fields(monkeypatch) -> None:  # noqa: ANN001
    monkeypatch.setattr(
        api_server,
        'app_config',
        _mock_app_config(
            dsn='postgresql://cfg-user:cfg-pass@127.0.0.1:5432/fav',
            token='abc123',
            bind=_PRIMARY_BIND,
            port=_PRIMARY_PORT,
        ),
    )

    config = api_server.load_config_from_settings()

    assert config.dsn == 'postgresql://cfg-user:cfg-pass@127.0.0.1:5432/fav'
    assert config.token == 'abc123'
    assert config.bind == _PRIMARY_BIND
    assert config.port == _PRIMARY_PORT


def test_load_config_from_settings_rejects_empty_database_postgres_dsn(monkeypatch) -> None:  # noqa: ANN001
    monkeypatch.setattr(
        api_server,
        'app_config',
        _mock_app_config(dsn='', token='abc123'),
    )

    with pytest.raises(ValueError, match=r'database\.postgres_dsn is required'):
        api_server.load_config_from_settings()


def test_load_config_from_settings_rejects_empty_api_token(monkeypatch) -> None:  # noqa: ANN001
    monkeypatch.setattr(
        api_server,
        'app_config',
        _mock_app_config(dsn='postgresql://db.local/fav', token=''),
    )

    with pytest.raises(ValueError, match=r'api\.token is required'):
        api_server.load_config_from_settings()
