# ruff: noqa: INP001, S101, S105, S106, ANN001

import json
from datetime import UTC, datetime
from http import HTTPStatus
from types import SimpleNamespace

import pytest

import src.api.server as api_server
from src.service.jobs import ScheduledJob
from src.tool.notifications import NotificationRecord
from src.tool.runtime_config import RuntimeSeriesSeed

_FIXED_NOW = datetime(2026, 3, 2, 0, 0, tzinfo=UTC)
_VALID_TOKEN = 'token-for-tests'
_PRIMARY_BIND = '127.0.0.3'
_PRIMARY_PORT = 18091


class _RuntimeService:
    def __init__(self, seeds: list[RuntimeSeriesSeed] | None = None) -> None:
        self.seeds = list(seeds or [])
        self.add_error: Exception | None = None
        self.delete_result: RuntimeSeriesSeed | None = None
        self.added: list[str] = []
        self.deleted: list[str] = []

    def list_seeds(self) -> list[RuntimeSeriesSeed]:
        return list(self.seeds)

    def add_seed(self, raw_seed: str) -> RuntimeSeriesSeed:
        self.added.append(raw_seed)
        if self.add_error is not None:
            raise self.add_error
        seed = RuntimeSeriesSeed(video_id='12488', title='屈辱')
        self.seeds.append(seed)
        return seed

    def delete_seed(self, video_id: str) -> RuntimeSeriesSeed | None:
        self.deleted.append(video_id)
        return self.delete_result

    def close(self) -> None:
        return None


def _notification(*, notification_id: int, status: str = 'unread') -> NotificationRecord:
    return NotificationRecord(
        notification_id=notification_id,
        kind='download_completed',
        source='bilibili',
        title=f'Notification {notification_id}',
        body='body',
        link_url='https://example.com',
        image_url='https://example.com/image.jpg',
        payload='{"bvid":"BV1TEST"}',
        status=status,
        created_at=_FIXED_NOW,
        read_at=_FIXED_NOW if status == 'read' else None,
    )


def _job(*, key: str, enabled: bool = True, run_on_start: bool = False) -> ScheduledJob:
    return ScheduledJob(
        key=key,
        name=key.title(),
        cron='*/30 * * * *',
        enabled=enabled,
        run_on_start=run_on_start,
        required_commands=(),
        factory=object,
    )


def _build_request(
    *,
    request_id: int = 99,
    kind: str = 'trigger_job',
    target: str = 'bilibili',
    status: str = 'pending',
) -> SimpleNamespace:
    return SimpleNamespace(
        request_id=request_id,
        kind=kind,
        target=target,
        status=status,
        requested_at=_FIXED_NOW,
        started_at=_FIXED_NOW if status != 'pending' else None,
        finished_at=_FIXED_NOW if status in {'succeeded', 'failed', 'rejected'} else None,
        result='ok' if status == 'succeeded' else '',
        error='boom' if status == 'failed' else '',
    )


def _build_service(
    *,
    fetcher,
    token: str,
    jobs: list[ScheduledJob] | None = None,
    request_creator=None,
    request_getter=None,
    runtime_service: _RuntimeService | None = None,
    notification_lister=None,
    notification_acker=None,
) -> api_server.FavApiService:
    runtime = runtime_service or _RuntimeService()
    return api_server.FavApiService(
        dsn='postgresql://db.local/fav',
        token=token,
        hanime1_fetcher=fetcher,
        now_provider=lambda: _FIXED_NOW,
        job_provider=(lambda: list(jobs or [])),
        control_request_creator=request_creator,
        control_request_getter=request_getter,
        runtime_service=runtime,
        notification_lister=notification_lister,
        notification_acker=notification_acker,
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
        path='/api/v1/runtime/hanime1/downloaded-ids',
        headers={},
    )

    assert response.status == HTTPStatus.UNAUTHORIZED
    assert response.headers['WWW-Authenticate'] == 'Bearer realm="fav-api"'
    assert _json_body(response) == {'error': 'missing_authorization'}


def test_hanime1_downloaded_ids_rejects_invalid_token() -> None:
    service = _build_service(fetcher=lambda _dsn: ['1001'], token=_VALID_TOKEN)

    response = service.handle_request(
        method='GET',
        path='/api/v1/runtime/hanime1/downloaded-ids',
        headers={'Authorization': 'Bearer wrong-token'},
    )

    assert response.status == HTTPStatus.FORBIDDEN
    assert _json_body(response) == {'error': 'invalid_token'}


def test_hanime1_downloaded_ids_returns_empty_payload_for_empty_table() -> None:
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN)

    response = service.handle_request(
        method='GET',
        path='/api/v1/runtime/hanime1/downloaded-ids',
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
        path='/api/v1/runtime/hanime1/downloaded-ids',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )
    etag = first.headers['ETag']

    second = service.handle_request(
        method='GET',
        path='/api/v1/runtime/hanime1/downloaded-ids',
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
        path='/api/v1/runtime/hanime1/downloaded-ids',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )

    payload = _json_body(response)
    body_text = response.body.decode('utf-8') if response.body else ''

    assert response.status == HTTPStatus.INTERNAL_SERVER_ERROR
    assert payload == {'error': 'internal_server_error'}
    assert 'password=my-secret' not in body_text


def test_control_jobs_returns_registered_jobs() -> None:
    service = _build_service(
        fetcher=lambda _dsn: [],
        token=_VALID_TOKEN,
        jobs=[_job(key='bilibili', enabled=True), _job(key='telegram', enabled=False, run_on_start=True)],
    )

    response = service.handle_request(
        method='GET',
        path='/api/v1/control/jobs',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )

    assert response.status == HTTPStatus.OK
    assert _json_body(response) == {
        'jobs': [
            {'key': 'bilibili', 'name': 'Bilibili', 'enabled': True, 'run_on_start': False},
            {'key': 'telegram', 'name': 'Telegram', 'enabled': False, 'run_on_start': True},
        ],
        'count': 2,
    }


def test_create_control_request_accepts_disabled_target() -> None:
    created = _build_request(target='telegram', status='pending')
    service = _build_service(
        fetcher=lambda _dsn: [],
        token=_VALID_TOKEN,
        jobs=[_job(key='telegram', enabled=False)],
        request_creator=lambda kind, target: created,
    )

    response = service.handle_request(
        method='POST',
        path='/api/v1/control/requests',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
        body=b'{"kind":"trigger_job","target":"telegram"}',
    )

    assert response.status == HTTPStatus.ACCEPTED
    assert _json_body(response)['target'] == 'telegram'
    assert _json_body(response)['status'] == 'pending'


def test_create_control_request_rejects_invalid_json() -> None:
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN)

    response = service.handle_request(
        method='POST',
        path='/api/v1/control/requests',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
        body=b'{bad',
    )

    assert response.status == HTTPStatus.BAD_REQUEST
    assert _json_body(response) == {'error': 'invalid_json'}


def test_create_control_request_rejects_unknown_kind() -> None:
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN, jobs=[_job(key='bilibili')])

    response = service.handle_request(
        method='POST',
        path='/api/v1/control/requests',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
        body=b'{"kind":"unknown","target":"bilibili"}',
    )

    assert response.status == HTTPStatus.BAD_REQUEST
    assert _json_body(response) == {'error': 'invalid_kind'}


def test_create_control_request_rejects_unknown_target() -> None:
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN, jobs=[_job(key='bilibili')])

    response = service.handle_request(
        method='POST',
        path='/api/v1/control/requests',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
        body=b'{"kind":"trigger_job","target":"kemono"}',
    )

    assert response.status == HTTPStatus.BAD_REQUEST
    assert _json_body(response) == {'error': 'invalid_target'}


@pytest.mark.parametrize('status', ['pending', 'running', 'succeeded', 'failed', 'rejected'])
def test_get_control_request_returns_status_payload(status: str) -> None:
    service = _build_service(
        fetcher=lambda _dsn: [],
        token=_VALID_TOKEN,
        request_getter=lambda _request_id: _build_request(status=status),
    )

    response = service.handle_request(
        method='GET',
        path='/api/v1/control/requests/99',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )

    payload = _json_body(response)

    assert response.status == HTTPStatus.OK
    assert payload['request_id'] == 99
    assert payload['status'] == status


def test_list_hanime1_seeds_returns_normalized_payload() -> None:
    runtime_service = _RuntimeService(seeds=[RuntimeSeriesSeed(video_id='12488', title='屈辱')])
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN, runtime_service=runtime_service)

    response = service.handle_request(
        method='GET',
        path='/api/v1/control/runtime/hanime1/seeds',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )

    assert response.status == HTTPStatus.OK
    assert _json_body(response) == {
        'seeds': [{'video_id': '12488', 'title': '屈辱', 'label': '屈辱 {id-12488}'}],
        'count': 1,
    }


def test_add_hanime1_seed_returns_created_seed() -> None:
    runtime_service = _RuntimeService()
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN, runtime_service=runtime_service)

    response = service.handle_request(
        method='POST',
        path='/api/v1/control/runtime/hanime1/seeds',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
        body=b'{"seed":"12488"}',
    )

    assert response.status == HTTPStatus.CREATED
    assert runtime_service.added == ['12488']
    assert _json_body(response) == {'video_id': '12488', 'title': '屈辱', 'label': '屈辱 {id-12488}'}


def test_add_hanime1_seed_rejects_duplicate_seed() -> None:
    runtime_service = _RuntimeService()
    runtime_service.add_error = FileExistsError('duplicate_seed')
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN, runtime_service=runtime_service)

    response = service.handle_request(
        method='POST',
        path='/api/v1/control/runtime/hanime1/seeds',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
        body=b'{"seed":"12488"}',
    )

    assert response.status == HTTPStatus.CONFLICT
    assert _json_body(response) == {'error': 'duplicate_seed'}


def test_add_hanime1_seed_returns_resolve_error() -> None:
    runtime_service = _RuntimeService()
    runtime_service.add_error = LookupError('seed_resolve_failed')
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN, runtime_service=runtime_service)

    response = service.handle_request(
        method='POST',
        path='/api/v1/control/runtime/hanime1/seeds',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
        body=b'{"seed":"12488"}',
    )

    assert response.status == HTTPStatus.UNPROCESSABLE_ENTITY
    assert _json_body(response) == {'error': 'seed_resolve_failed'}


def test_delete_hanime1_seed_returns_deleted_seed() -> None:
    runtime_service = _RuntimeService()
    runtime_service.delete_result = RuntimeSeriesSeed(video_id='12488', title='屈辱')
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN, runtime_service=runtime_service)

    response = service.handle_request(
        method='DELETE',
        path='/api/v1/control/runtime/hanime1/seeds/12488',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )

    assert response.status == HTTPStatus.OK
    assert runtime_service.deleted == ['12488']
    assert _json_body(response) == {'video_id': '12488', 'title': '屈辱', 'label': '屈辱 {id-12488}'}


def test_delete_hanime1_seed_returns_not_found() -> None:
    runtime_service = _RuntimeService()
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN, runtime_service=runtime_service)

    response = service.handle_request(
        method='DELETE',
        path='/api/v1/control/runtime/hanime1/seeds/12488',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )

    assert response.status == HTTPStatus.NOT_FOUND
    assert _json_body(response) == {'error': 'not_found'}


def test_list_notifications_defaults_to_unread() -> None:
    captured: dict[str, object] = {}

    def _fake_lister(status: str, limit: int, after_id: int | None) -> list[NotificationRecord]:
        captured.update({'status': status, 'limit': limit, 'after_id': after_id})
        return [_notification(notification_id=101)]

    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN, notification_lister=_fake_lister)

    response = service.handle_request(
        method='GET',
        path='/api/v1/notifications',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )

    assert response.status == HTTPStatus.OK
    assert captured == {'status': 'unread', 'limit': 50, 'after_id': None}
    assert _json_body(response) == {
        'notifications': [
            {
                'id': 101,
                'kind': 'download_completed',
                'source': 'bilibili',
                'title': 'Notification 101',
                'body': 'body',
                'link_url': 'https://example.com',
                'image_url': 'https://example.com/image.jpg',
                'payload': {'bvid': 'BV1TEST'},
                'status': 'unread',
                'created_at': '2026-03-02T00:00:00Z',
                'read_at': None,
            },
        ],
        'count': 1,
    }


def test_list_notifications_supports_status_limit_and_after_id() -> None:
    captured: dict[str, object] = {}

    def _fake_lister(status: str, limit: int, after_id: int | None) -> list[NotificationRecord]:
        captured.update({'status': status, 'limit': limit, 'after_id': after_id})
        return [_notification(notification_id=102, status='read')]

    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN, notification_lister=_fake_lister)

    response = service.handle_request(
        method='GET',
        path='/api/v1/notifications?status=all&limit=25&after_id=100',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )

    assert response.status == HTTPStatus.OK
    assert captured == {'status': 'all', 'limit': 25, 'after_id': 100}
    assert _json_body(response)['notifications'][0]['status'] == 'read'


def test_list_notifications_rejects_invalid_limit() -> None:
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN)

    response = service.handle_request(
        method='GET',
        path='/api/v1/notifications?limit=201',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
    )

    assert response.status == HTTPStatus.BAD_REQUEST
    assert _json_body(response) == {'error': 'invalid_limit'}


def test_ack_notifications_marks_ids_as_read() -> None:
    captured: list[int] = []

    def _fake_acker(ids: list[int]) -> int:
        captured.extend(ids)
        return 2

    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN, notification_acker=_fake_acker)

    response = service.handle_request(
        method='POST',
        path='/api/v1/notifications/ack',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
        body=b'{"ids":[1,2,2,3]}',
    )

    assert response.status == HTTPStatus.OK
    assert captured == [1, 2, 3]
    assert _json_body(response) == {'updated': 2}


def test_ack_notifications_rejects_invalid_ids() -> None:
    service = _build_service(fetcher=lambda _dsn: [], token=_VALID_TOKEN)

    response = service.handle_request(
        method='POST',
        path='/api/v1/notifications/ack',
        headers={'Authorization': f'Bearer {_VALID_TOKEN}'},
        body=b'{"ids":[1,"2"]}',
    )

    assert response.status == HTTPStatus.BAD_REQUEST
    assert _json_body(response) == {'error': 'invalid_ids'}


def test_load_config_from_settings_reads_database_postgres_dsn_and_api_fields(monkeypatch) -> None:
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


def test_load_config_from_settings_rejects_empty_database_postgres_dsn(monkeypatch) -> None:
    monkeypatch.setattr(
        api_server,
        'app_config',
        _mock_app_config(dsn='', token='abc123'),
    )

    with pytest.raises(ValueError, match=r'database\.postgres_dsn is required'):
        api_server.load_config_from_settings()


def test_load_config_from_settings_rejects_empty_api_token(monkeypatch) -> None:
    monkeypatch.setattr(
        api_server,
        'app_config',
        _mock_app_config(dsn='postgresql://db.local/fav', token=''),
    )

    with pytest.raises(ValueError, match=r'api\.token is required'):
        api_server.load_config_from_settings()
