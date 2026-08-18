# ruff: noqa: INP001, S101, S105, S106, ANN001, ARG005, PLR0913, PLR2004, TRY003, EM101

from datetime import UTC, datetime
from time import sleep
from types import SimpleNamespace

import pytest
from fastapi.testclient import TestClient
from pydantic import BaseModel

import src.api.server as api_server
import src.api.service as api_service_module
from src.api.app import create_app
from src.core import settings
from src.service.jobs import ScheduledJob
from src.tool.control_queue import ControlRequest
from src.tool.runtime_config import Hanime1ParserIncompatibleError, RuntimeSeriesSeed
from src.tool.telegram_bot import TelegramDeliveryError, TelegramDeliveryResult

_FIXED_NOW = datetime(2026, 3, 2, 0, 0, tzinfo=UTC)
_VALID_TOKEN = 'token-for-tests'
_PRIMARY_BIND = '127.0.0.3'
_PRIMARY_PORT = 18091


class _RuntimeService:
    def __init__(self, seeds: list[RuntimeSeriesSeed] | None = None) -> None:
        self.seeds = list(seeds or [])
        self.add_error: Exception | None = None
        self.added: list[str] = []
        self.deleted: list[str] = []
        self.readiness_ids: list[str] = []
        self.readiness_delay = 0.0
        self.readiness_calls = 0
        self.probe_errors: dict[str, Exception] = {}
        self.probe_delays: dict[str, float] = {}
        self.probe_calls: list[str] = []

    def list_seeds(self) -> list[dict]:
        return [
            {
                'video_id': seed.video_id,
                'title': seed.title,
                'label': seed.label,
                'added_from_video_id': seed.video_id,
                'video_count': 1,
                'created_at': _FIXED_NOW,
                'updated_at': _FIXED_NOW,
                'last_scanned_at': None,
                'last_scan_error': '',
                'watch_url': f'https://hanime1.me/watch?v={seed.video_id}',
            }
            for seed in self.seeds
        ]

    def delete_seed(self, canonical_video_id: str) -> bool:
        remaining = [seed for seed in self.seeds if seed.video_id != canonical_video_id]
        deleted = len(remaining) != len(self.seeds)
        self.seeds = remaining
        self.deleted.append(canonical_video_id)
        return deleted

    def add_seed(self, raw_seed: str) -> RuntimeSeriesSeed:
        self.added.append(raw_seed)
        if self.add_error is not None:
            raise self.add_error
        seed = RuntimeSeriesSeed(video_id='12488', title='屈辱')
        self.seeds.append(seed)
        return seed

    def readiness_seed_ids(self, *, limit: int = 3) -> list[str]:
        self.readiness_calls += 1
        sleep(self.readiness_delay)
        return self.readiness_ids[:limit]

    def probe_seed(self, seed_id: str, *, timeout_seconds: float = 10) -> None:
        assert timeout_seconds == 10
        self.probe_calls.append(seed_id)
        sleep(self.probe_delays.get(seed_id, 0))
        error = self.probe_errors.get(seed_id)
        if error is not None:
            raise error

    def close(self) -> None:
        return None


class _AuthorService:
    def __init__(self, authors: list[dict] | None = None) -> None:
        self.authors = list(authors or [])
        self.add_error: Exception | None = None
        self.added: list[str] = []
        self.deleted: list[str] = []

    def list_authors(self) -> list[dict]:
        return [dict(author) for author in self.authors]

    def add_author(self, raw_author: str) -> dict:
        self.added.append(raw_author)
        if self.add_error is not None:
            raise self.add_error
        return {
            'author_id': '202534',
            'name': 'Maplestar',
            'author_url': 'https://hanime1.me/user/202534/uploaded',
        }

    def delete_author(self, author_id: str) -> bool:
        remaining = [author for author in self.authors if author['author_id'] != author_id]
        deleted = len(remaining) != len(self.authors)
        self.authors = remaining
        self.deleted.append(author_id)
        return deleted

    def close(self) -> None:
        return None


def _job(*, key: str, enabled: bool = True, notify: bool = True) -> ScheduledJob:
    return ScheduledJob(
        key=key,
        name=key.title(),
        cron='*/30 * * * *',
        enabled=enabled,
        notify=notify,
        required_commands=(),
        factory=object,
        section=f'web.{key}',
    )


def _build_request(
    *,
    request_id: int = 99,
    target: str = 'bilibili',
    status: str = 'pending',
) -> ControlRequest:
    return ControlRequest(
        request_id=request_id,
        kind='trigger_job',
        target=target,
        payload='{}',
        status=status,
        requested_at=_FIXED_NOW,
        started_at=_FIXED_NOW if status != 'pending' else None,
        finished_at=_FIXED_NOW if status in {'succeeded', 'failed', 'rejected'} else None,
        result='ok' if status == 'succeeded' else '',
        error='boom' if status == 'failed' else '',
    )


def _build_service(
    *,
    token: str,
    hanime1_video_fetcher=None,
    jobs: list[ScheduledJob] | None = None,
    request_creator=None,
    request_getter=None,
    runtime_service: _RuntimeService | None = None,
    author_service: _AuthorService | None = None,
    telegram_notification_tester=None,
) -> api_server.FavApiService:
    runtime = runtime_service or _RuntimeService()
    # Readiness probes Hanime1, which is disabled by default on a fresh install.
    settings.load().web.hanime1.enabled = True
    return api_server.FavApiService(
        dsn='postgresql://db.local/fav',
        token=token,
        hanime1_video_fetcher=hanime1_video_fetcher,
        now_provider=lambda: _FIXED_NOW,
        job_provider=(lambda: list(jobs or [])),
        control_request_creator=request_creator,
        control_request_getter=request_getter,
        runtime_service=runtime,
        author_service=author_service or _AuthorService(),
        telegram_notification_tester=telegram_notification_tester,
    )


def _mock_env(
    *,
    dsn: str,
    token: str,
    bind: str = _PRIMARY_BIND,
    port: int = _PRIMARY_PORT,
    cors_origins: tuple[str, ...] = (),
    cors_allow_credentials: bool = False,
) -> SimpleNamespace:
    return SimpleNamespace(
        postgres_dsn=dsn,
        api_token=token,
        api_bind=bind,
        api_port=port,
        cors_origins=cors_origins,
        api_cors_allow_credentials=cors_allow_credentials,
    )


def _auth_headers(token: str = _VALID_TOKEN) -> dict[str, str]:
    return {'Authorization': f'Bearer {token}'}


def test_health_endpoint_returns_ok_without_authorization() -> None:
    service = _build_service(token=_VALID_TOKEN)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/healthz')

    assert response.status_code == 200
    assert response.json() == {'status': 'ok', 'generated_at': '2026-03-02T00:00:00Z'}


def test_readiness_endpoint_returns_ok_when_any_hanime1_probe_succeeds() -> None:
    runtime = _RuntimeService()
    runtime.readiness_ids = ['100', '200', '300']
    runtime.probe_errors = {
        '100': Hanime1ParserIncompatibleError('bad structure'),
        '200': RuntimeError('upstream failed'),
    }
    service = _build_service(token=_VALID_TOKEN, runtime_service=runtime)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/readyz')

    assert response.status_code == 200
    assert response.json()['status'] == 'ok'
    assert response.json()['checks']['hanime1'] == {
        'status': 'ok',
        'code': 'ok',
        'message': 'Hanime1 playlist parsing is healthy.',
        'sampled_targets': 3,
    }
    assert sorted(runtime.probe_calls) == ['100', '200', '300']


def test_readiness_endpoint_returns_on_first_success(monkeypatch) -> None:
    runtime = _RuntimeService()
    runtime.readiness_ids = ['fast', 'slow']
    runtime.probe_delays['slow'] = 0.2
    monkeypatch.setattr(api_service_module, '_READINESS_TOTAL_TIMEOUT_SECONDS', 0.05)
    service = _build_service(token=_VALID_TOKEN, runtime_service=runtime)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/readyz')

    assert response.status_code == 200
    assert response.json()['status'] == 'ok'


def test_readiness_total_budget_includes_target_loading(monkeypatch) -> None:
    runtime = _RuntimeService()
    runtime.readiness_delay = 0.05
    monkeypatch.setattr(api_service_module, '_READINESS_TOTAL_TIMEOUT_SECONDS', 0.01)
    service = _build_service(token=_VALID_TOKEN, runtime_service=runtime)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/readyz')

    assert response.status_code == 503
    assert response.json()['checks']['hanime1']['code'] == 'upstream_timeout'
    assert response.json()['checks']['hanime1']['sampled_targets'] == 0


def test_readiness_endpoint_returns_degraded_for_incompatible_parser() -> None:
    runtime = _RuntimeService()
    runtime.readiness_ids = ['100', '200']
    runtime.probe_errors = {
        '100': Hanime1ParserIncompatibleError('bad structure'),
        '200': Hanime1ParserIncompatibleError('bad structure'),
    }
    service = _build_service(token=_VALID_TOKEN, runtime_service=runtime)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/readyz')

    assert response.status_code == 503
    assert response.json()['status'] == 'degraded'
    assert response.json()['checks']['hanime1']['code'] == 'parser_incompatible'
    assert response.json()['checks']['hanime1']['sampled_targets'] == 2


def test_readiness_endpoint_skips_when_no_targets_and_caches_for_five_minutes(monkeypatch) -> None:
    now = [0.0]
    monkeypatch.setattr(api_service_module, 'monotonic', lambda: now[0])
    runtime = _RuntimeService()
    service = _build_service(token=_VALID_TOKEN, runtime_service=runtime)

    with TestClient(create_app(service=service)) as client:
        first = client.get('/readyz')
        runtime.readiness_ids = ['100']
        now[0] = 299
        second = client.get('/readyz')
        now[0] = 300
        third = client.get('/readyz')

    assert first.status_code == 200
    assert first.json()['checks']['hanime1']['code'] == 'no_targets'
    assert second.json() == first.json()
    assert third.json()['checks']['hanime1']['code'] == 'ok'
    assert runtime.readiness_calls == 2
    assert runtime.probe_calls == ['100']


def test_readiness_endpoint_skips_when_hanime1_is_disabled() -> None:
    runtime = _RuntimeService()
    runtime.readiness_ids = ['100']
    service = _build_service(token=_VALID_TOKEN, runtime_service=runtime)
    settings.load().web.hanime1.enabled = False

    with TestClient(create_app(service=service)) as client:
        response = client.get('/readyz')

    assert response.status_code == 200
    assert response.json()['checks']['hanime1']['code'] == 'disabled'
    assert runtime.probe_calls == []


def test_docs_and_openapi_are_public_and_v2_only() -> None:
    service = _build_service(token=_VALID_TOKEN, jobs=[_job(key='bilibili')])

    with TestClient(create_app(service=service)) as client:
        docs_response = client.get('/docs')
        openapi_response = client.get('/openapi.json')
        v1_response = client.get('/api/v1/health')

    assert docs_response.status_code == 200
    assert openapi_response.status_code == 200
    payload = openapi_response.json()
    assert '/api/v2/azurlane/characters' in payload['paths']
    assert '/api/v2/azurlane/sidebar/characters' in payload['paths']
    assert '/api/v2/azurlane/characters/{character_key}' in payload['paths']
    assert '304' in payload['paths']['/api/v2/azurlane/sidebar/characters']['get']['responses']
    assert '/api/v2/azurlane/assets/{character_key}/{asset_path}' not in payload['paths']
    assert '/api/v2/azurlane/characters/{character_key}/live2d-models/{model_id}/view-overrides/{profile}' not in payload['paths']
    assert '/api/v2/bd2/characters' in payload['paths']
    assert '/api/v2/bd2/characters/{content_id}' in payload['paths']
    assert '/api/v2/bd2/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}' in payload['paths']
    assert set(payload['paths']['/api/v2/bd2/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}']) == {
        'delete',
        'get',
        'put',
    }
    assert '/api/v2/bd2/assets/{content_id}/{asset_path}' not in payload['paths']
    assert '304' in payload['paths']['/api/v2/bd2/sidebar/characters']['get']['responses']
    assert '/api/v2/nikke/assets/{content_id}/{asset_path}' not in payload['paths']
    assert '/api/v2/nikke/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}' in payload['paths']
    assert set(payload['paths']['/api/v2/nikke/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}']) == {
        'delete',
        'get',
        'put',
    }
    assert '304' in payload['paths']['/api/v2/nikke/sidebar/characters']['get']['responses']
    assert '/api/v2/jobs' in payload['paths']
    assert '/api/v2/hanime1/videos' in payload['paths']
    assert '/api/v2/hanime1/seeds' in payload['paths']
    assert set(payload['paths']['/api/v2/hanime1/seeds']) == {'get', 'post'}
    assert set(payload['paths']['/api/v2/hanime1/seeds/{canonical_video_id}']) == {'delete'}
    assert set(payload['paths']['/api/v2/hanime1/authors']) == {'get', 'post'}
    assert set(payload['paths']['/api/v2/hanime1/authors/{author_id}']) == {'delete'}
    assert '/api/v2/settings' in payload['paths']
    assert set(payload['paths']['/api/v2/settings/{section}']) == {'get', 'put'}
    assert set(payload['paths']['/api/v2/notifications/telegram/test']) == {'post'}
    assert '/api/v2/archive/sources' in payload['paths']
    assert '/api/v2/archive/items' in payload['paths']
    assert '/api/v2/hanime1/seeds/{video_id}' not in payload['paths']
    assert '/api/v2/notifications' not in payload['paths']
    assert '/api/v2/notifications/ack' not in payload['paths']
    assert '/api/v1/health' not in payload['paths']
    assert '/readyz' in payload['paths']
    assert payload['paths']['/api/v2/jobs']['get']['operationId'] == 'listJobs'
    assert v1_response.status_code == 404
    assert v1_response.json() == {
        'error': {
            'code': 'not_found',
            'message': 'Not Found',
            'details': None,
        },
    }


def test_telegram_notification_test_endpoint_returns_delivery_result() -> None:
    async def _test_notification() -> TelegramDeliveryResult:
        return TelegramDeliveryResult(message_id=77, media_status='none', warnings=('Pinning failed',))

    service = _build_service(token=_VALID_TOKEN, telegram_notification_tester=_test_notification)

    with TestClient(create_app(service=service)) as client:
        response = client.post('/api/v2/notifications/telegram/test', headers=_auth_headers(), json={})

    assert response.status_code == 200
    assert response.json() == {'status': 'delivered', 'message_id': 77, 'warnings': ['Pinning failed']}


def test_telegram_notification_test_endpoint_returns_sanitized_delivery_error() -> None:
    async def _test_notification() -> TelegramDeliveryResult:
        raise TelegramDeliveryError('Telegram Bot API responded with error 401: Unauthorized', retryable=False)

    service = _build_service(token=_VALID_TOKEN, telegram_notification_tester=_test_notification)

    with TestClient(create_app(service=service)) as client:
        response = client.post('/api/v2/notifications/telegram/test', headers=_auth_headers(), json={})

    assert response.status_code == 502
    assert response.json()['error'] == {
        'code': 'telegram_delivery_failed',
        'message': 'Telegram Bot API responded with error 401: Unauthorized',
        'details': None,
    }


def test_configured_cors_allows_frontend_origin_preflight() -> None:
    service = _build_service(token=_VALID_TOKEN, jobs=[_job(key='bilibili')])
    config = api_server.ApiConfig(
        dsn='postgresql://db.local/fav',
        token=_VALID_TOKEN,
        bind=_PRIMARY_BIND,
        port=_PRIMARY_PORT,
        cors_origins=('https://game-view.s117.me',),
        cors_allow_credentials=True,
    )

    with TestClient(create_app(config=config, service=service)) as client:
        response = client.options(
            '/api/v2/jobs',
            headers={
                'Origin': 'https://game-view.s117.me',
                'Access-Control-Request-Method': 'PUT',
                'Access-Control-Request-Headers': 'Authorization',
            },
        )

    assert response.status_code == 200
    assert response.headers['access-control-allow-origin'] == 'https://game-view.s117.me'
    assert response.headers['access-control-allow-credentials'] == 'true'
    assert 'Authorization' in response.headers['access-control-allow-headers']


def test_jobs_endpoint_requires_authorization_header() -> None:
    service = _build_service(token=_VALID_TOKEN)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/jobs')

    assert response.status_code == 401
    assert response.headers['WWW-Authenticate'] == 'Bearer realm="fav-api"'
    assert response.json() == {
        'error': {
            'code': 'missing_authorization',
            'message': 'Authorization header is required.',
            'details': None,
        },
    }


def test_jobs_endpoint_rejects_invalid_token() -> None:
    service = _build_service(token=_VALID_TOKEN)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/jobs', headers=_auth_headers('wrong-token'))

    assert response.status_code == 403
    assert response.json() == {
        'error': {
            'code': 'invalid_token',
            'message': 'Bearer token is invalid.',
            'details': None,
        },
    }


def test_jobs_endpoint_returns_registered_jobs() -> None:
    service = _build_service(
        token=_VALID_TOKEN,
        jobs=[_job(key='bilibili', enabled=True), _job(key='telegram', enabled=False, notify=False)],
    )

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/jobs', headers=_auth_headers())

    assert response.status_code == 200
    assert response.json() == {
        'items': [
            {
                'key': 'bilibili',
                'name': 'Bilibili',
                'enabled': True,
                'notify': True,
                'cron': '*/30 * * * *',
                'section': 'web.bilibili',
                'missing_fields': [],
            },
            {
                'key': 'telegram',
                'name': 'Telegram',
                'enabled': False,
                'notify': False,
                'cron': '*/30 * * * *',
                'section': 'web.telegram',
                'missing_fields': [],
            },
        ],
        'total': 2,
    }


def test_create_job_request_accepts_disabled_target() -> None:
    created = _build_request(target='telegram', status='pending')
    service = _build_service(
        token=_VALID_TOKEN,
        jobs=[_job(key='telegram', enabled=False)],
        request_creator=lambda kind, target: created,
    )

    with TestClient(create_app(service=service)) as client:
        response = client.post('/api/v2/job-requests', headers=_auth_headers(), json={'target': 'telegram'})

    assert response.status_code == 202
    assert response.json() == {
        'id': 99,
        'target': 'telegram',
        'kind': 'trigger_job',
        'status': 'pending',
        'requested_at': '2026-03-02T00:00:00Z',
        'started_at': None,
        'finished_at': None,
        'result': '',
        'error': '',
    }


def test_create_job_request_accepts_azurlane_target() -> None:
    created = _build_request(target='azurlane', status='pending')
    service = _build_service(
        token=_VALID_TOKEN,
        jobs=[_job(key='azurlane', enabled=False)],
        request_creator=lambda kind, target: created,
    )

    with TestClient(create_app(service=service)) as client:
        response = client.post('/api/v2/job-requests', headers=_auth_headers(), json={'target': 'azurlane'})

    assert response.status_code == 202
    assert response.json()['target'] == 'azurlane'


def test_create_job_request_rejects_invalid_target_as_validation_error() -> None:
    service = _build_service(token=_VALID_TOKEN, jobs=[_job(key='bilibili')])

    with TestClient(create_app(service=service)) as client:
        response = client.post('/api/v2/job-requests', headers=_auth_headers(), json={'target': 'not-a-source'})

    assert response.status_code == 422
    assert response.json()['error']['code'] == 'validation_error'


@pytest.mark.parametrize('status', ['pending', 'running', 'succeeded', 'failed', 'rejected'])
def test_get_job_request_returns_status_payload(status: str) -> None:
    service = _build_service(
        token=_VALID_TOKEN,
        request_getter=lambda _request_id: _build_request(status=status),
    )

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/job-requests/99', headers=_auth_headers())

    assert response.status_code == 200
    assert response.json()['id'] == 99
    assert response.json()['status'] == status


def test_hanime1_seed_list_and_delete_endpoints_are_available() -> None:
    runtime_service = _RuntimeService(seeds=[RuntimeSeriesSeed(video_id='12488', title='屈辱')])
    service = _build_service(token=_VALID_TOKEN, runtime_service=runtime_service)

    with TestClient(create_app(service=service)) as client:
        list_response = client.get('/api/v2/hanime1/seeds', headers=_auth_headers())
        delete_response = client.delete('/api/v2/hanime1/seeds/12488', headers=_auth_headers())
        missing_response = client.delete('/api/v2/hanime1/seeds/99999', headers=_auth_headers())

    assert list_response.status_code == 200
    payload = list_response.json()
    assert payload['total'] == 1
    assert payload['items'][0]['video_id'] == '12488'
    assert payload['items'][0]['label'] == '屈辱 {id-12488}'
    assert delete_response.status_code == 204
    assert runtime_service.deleted == ['12488', '99999']
    assert missing_response.status_code == 404


def test_list_hanime1_videos_returns_video_objects() -> None:
    service = _build_service(
        token=_VALID_TOKEN,
        hanime1_video_fetcher=lambda _dsn: [
            {
                'video_id': '1001',
                'title': 'Video One',
                'downloaded': True,
                'uploader': 'Uploader One',
                'release_date': '2024-01-01',
                'plot': 'Plot One',
                'watch_url': 'https://hanime1.me/watch?v=1001',
            },
            {
                'video_id': '1002',
                'title': 'Video Two',
                'downloaded': True,
                'uploader': None,
                'release_date': None,
                'plot': None,
                'watch_url': 'https://hanime1.me/watch?v=1002',
            },
        ],
    )

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/hanime1/videos', headers=_auth_headers())

    assert response.status_code == 200
    assert response.json() == {
        'items': [
            {
                'video_id': '1001',
                'title': 'Video One',
                'downloaded': True,
                'uploader': 'Uploader One',
                'release_date': '2024-01-01',
                'plot': 'Plot One',
                'watch_url': 'https://hanime1.me/watch?v=1001',
            },
            {
                'video_id': '1002',
                'title': 'Video Two',
                'downloaded': True,
                'uploader': None,
                'release_date': None,
                'plot': None,
                'watch_url': 'https://hanime1.me/watch?v=1002',
            },
        ],
        'total': 2,
    }


def test_add_hanime1_seed_returns_created_seed() -> None:
    runtime_service = _RuntimeService()
    service = _build_service(token=_VALID_TOKEN, runtime_service=runtime_service)

    with TestClient(create_app(service=service)) as client:
        response = client.post('/api/v2/hanime1/seeds', headers=_auth_headers(), json={'seed': '12488'})

    assert response.status_code == 201
    assert runtime_service.added == ['12488']
    assert response.json() == {'video_id': '12488', 'title': '屈辱', 'label': '屈辱 {id-12488}'}


def test_add_hanime1_seed_rejects_duplicate_seed() -> None:
    runtime_service = _RuntimeService()
    runtime_service.add_error = FileExistsError('duplicate_seed')
    service = _build_service(token=_VALID_TOKEN, runtime_service=runtime_service)

    with TestClient(create_app(service=service)) as client:
        response = client.post('/api/v2/hanime1/seeds', headers=_auth_headers(), json={'seed': '12488'})

    assert response.status_code == 409
    assert response.json() == {
        'error': {
            'code': 'duplicate_seed',
            'message': 'Hanime1 seed already exists.',
            'details': None,
        },
    }


def test_add_hanime1_seed_returns_resolve_error() -> None:
    runtime_service = _RuntimeService()
    runtime_service.add_error = LookupError('seed_resolve_failed')
    service = _build_service(token=_VALID_TOKEN, runtime_service=runtime_service)

    with TestClient(create_app(service=service)) as client:
        response = client.post('/api/v2/hanime1/seeds', headers=_auth_headers(), json={'seed': '12488'})

    assert response.status_code == 422
    assert response.json() == {
        'error': {
            'code': 'seed_resolve_failed',
            'message': 'Unable to resolve Hanime1 seed.',
            'details': None,
        },
    }


def test_hanime1_author_list_and_delete_endpoints_are_available() -> None:
    author_service = _AuthorService(
        authors=[
            {
                'author_id': '202534',
                'name': 'Maplestar',
                'author_url': 'https://hanime1.me/user/202534/uploaded',
                'video_count': 34,
                'created_at': _FIXED_NOW,
                'updated_at': _FIXED_NOW,
                'last_scanned_at': None,
                'last_scan_error': '',
            },
        ],
    )
    service = _build_service(token=_VALID_TOKEN, author_service=author_service)

    with TestClient(create_app(service=service)) as client:
        list_response = client.get('/api/v2/hanime1/authors', headers=_auth_headers())
        delete_response = client.delete('/api/v2/hanime1/authors/202534', headers=_auth_headers())
        missing_response = client.delete('/api/v2/hanime1/authors/99999', headers=_auth_headers())
        invalid_response = client.delete('/api/v2/hanime1/authors/not-a-number', headers=_auth_headers())

    assert list_response.status_code == 200
    payload = list_response.json()
    assert payload['total'] == 1
    assert payload['items'][0] == {
        'author_id': '202534',
        'name': 'Maplestar',
        'author_url': 'https://hanime1.me/user/202534/uploaded',
        'video_count': 34,
        'created_at': '2026-03-02T00:00:00Z',
        'updated_at': '2026-03-02T00:00:00Z',
        'last_scanned_at': None,
        'last_scan_error': '',
    }
    assert delete_response.status_code == 204
    assert author_service.deleted == ['202534', '99999']
    assert missing_response.status_code == 404
    assert invalid_response.status_code == 422


def test_add_hanime1_author_returns_created_author() -> None:
    author_service = _AuthorService()
    service = _build_service(token=_VALID_TOKEN, author_service=author_service)

    with TestClient(create_app(service=service)) as client:
        response = client.post(
            '/api/v2/hanime1/authors',
            headers=_auth_headers(),
            json={'author': 'https://hanime1.me/user/202534/uploaded'},
        )

    assert response.status_code == 201
    assert author_service.added == ['https://hanime1.me/user/202534/uploaded']
    assert response.json() == {
        'author_id': '202534',
        'name': 'Maplestar',
        'author_url': 'https://hanime1.me/user/202534/uploaded',
    }


@pytest.mark.parametrize(
    ('error', 'expected_status', 'expected_code', 'expected_message'),
    [
        (FileExistsError('duplicate_author'), 409, 'duplicate_author', 'Hanime1 author already exists.'),
        (LookupError('author_resolve_failed'), 422, 'author_resolve_failed', 'Unable to resolve Hanime1 author.'),
        (ValueError('invalid_author'), 422, 'invalid_author', 'Author id or URL is invalid.'),
    ],
)
def test_add_hanime1_author_maps_errors(error, expected_status, expected_code, expected_message) -> None:
    author_service = _AuthorService()
    author_service.add_error = error
    service = _build_service(token=_VALID_TOKEN, author_service=author_service)

    with TestClient(create_app(service=service)) as client:
        response = client.post('/api/v2/hanime1/authors', headers=_auth_headers(), json={'author': '202534'})

    assert response.status_code == expected_status
    assert response.json() == {
        'error': {
            'code': expected_code,
            'message': expected_message,
            'details': None,
        },
    }


def test_notifications_endpoints_are_removed() -> None:
    service = _build_service(token=_VALID_TOKEN)

    with TestClient(create_app(service=service)) as client:
        list_response = client.get('/api/v2/notifications', headers=_auth_headers())
        ack_response = client.post('/api/v2/notifications/ack', headers=_auth_headers(), json={'ids': [1]})

    assert list_response.status_code == 404
    assert ack_response.status_code == 404


def test_load_config_from_env_reads_bootstrap_values(monkeypatch) -> None:
    monkeypatch.setattr(
        api_server,
        'env',
        _mock_env(
            dsn='postgresql://cfg-user:cfg-pass@127.0.0.1:5432/fav',
            token='abc123',
            bind=_PRIMARY_BIND,
            port=_PRIMARY_PORT,
        ),
    )

    config = api_server.load_config_from_env()

    assert config.dsn == 'postgresql://cfg-user:cfg-pass@127.0.0.1:5432/fav'
    assert config.token == 'abc123'
    assert config.bind == _PRIMARY_BIND
    assert config.port == _PRIMARY_PORT
    assert config.cors_origins == ()
    assert config.cors_allow_credentials is False


def test_load_config_from_env_reads_cors_fields(monkeypatch) -> None:
    monkeypatch.setattr(
        api_server,
        'env',
        _mock_env(
            dsn='postgresql://cfg-user:cfg-pass@127.0.0.1:5432/fav',
            token='abc123',
            cors_origins=('https://game-view.s117.me',),
            cors_allow_credentials=True,
        ),
    )

    config = api_server.load_config_from_env()

    assert config.cors_origins == ('https://game-view.s117.me',)
    assert config.cors_allow_credentials is True


def test_load_config_from_env_rejects_empty_postgres_dsn(monkeypatch) -> None:
    monkeypatch.setattr(api_server, 'env', _mock_env(dsn='', token='abc123'))

    with pytest.raises(ValueError, match=r'POSTGRES_DSN is required'):
        api_server.load_config_from_env()


def test_load_config_from_env_rejects_empty_api_token(monkeypatch) -> None:
    monkeypatch.setattr(api_server, 'env', _mock_env(dsn='postgresql://db.local/fav', token=''))

    with pytest.raises(ValueError, match=r'API_TOKEN is required'):
        api_server.load_config_from_env()


def _settings_service(*, saved: list[tuple[str, dict]]) -> api_server.FavApiService:
    """Service wired to in-memory settings so the enable guard can be exercised."""

    def getter(section: str) -> BaseModel:
        return settings.SECTION_MODELS[section]()

    def saver(section: str, payload: dict) -> BaseModel:
        saved.append((section, payload))
        return settings.SECTION_MODELS[section].model_validate(payload)

    return api_server.FavApiService(
        dsn='postgresql://db.local/fav',
        token=_VALID_TOKEN,
        now_provider=lambda: _FIXED_NOW,
        settings_section_getter=getter,
        settings_section_saver=saver,
    )


def test_update_settings_section_rejects_enabling_an_incomplete_source() -> None:
    saved: list[tuple[str, dict]] = []
    service = _settings_service(saved=saved)

    with pytest.raises(api_service_module.ApiError) as excinfo:
        # Bilibili needs at least one account before it can run.
        service.update_settings_section('web.bilibili', {'cron': '0 * * * *', 'enabled': True})

    assert excinfo.value.status_code == 422
    assert excinfo.value.code == 'incomplete_settings'
    assert excinfo.value.details == {'missing_fields': ['accounts']}
    # The rejected payload must not have been persisted.
    assert saved == []


def test_update_settings_section_allows_an_incomplete_source_while_disabled() -> None:
    saved: list[tuple[str, dict]] = []
    service = _settings_service(saved=saved)

    result = service.update_settings_section('web.bilibili', {'cron': '0 * * * *', 'enabled': False})

    assert result['missing_fields'] == ['accounts']
    assert len(saved) == 1


def test_update_settings_section_accepts_enabling_a_complete_source() -> None:
    saved: list[tuple[str, dict]] = []
    service = _settings_service(saved=saved)

    result = service.update_settings_section(
        'web.bilibili',
        {
            'cron': '0 * * * *',
            'enabled': True,
            'accounts': [
                {
                    'name': 'main',
                    'favorites': [{'fav_id': 99, 'path': 'collection/bilibili/fav'}],
                    'cookiecloud': {'server_url': 'https://cc.example', 'uuid': 'u', 'password': 'pw'},
                },
            ],
        },
    )

    assert result['missing_fields'] == []
    assert saved[0][1]['enabled'] is True


def _cookiecloud_service(*, sections: dict) -> tuple[api_server.FavApiService, list[tuple[str, str, str]]]:
    """Service whose settings come from `sections`, recording what the probe was handed."""
    probed: list[tuple[str, str, str]] = []

    def getter(section: str) -> BaseModel:
        return settings.SECTION_MODELS[section].model_validate(sections.get(section, {}))

    service = api_server.FavApiService(
        dsn='postgresql://db.local/fav',
        token=_VALID_TOKEN,
        now_provider=lambda: _FIXED_NOW,
        settings_section_getter=getter,
    )
    return service, probed


def _stub_probe(monkeypatch, probed: list[tuple[str, str, str]]) -> None:
    def _fake_probe(server_url: str, uuid: str, password: str, **_kwargs: object) -> api_service_module.cookiecloud_tool.CookieCloudProbe:
        probed.append((server_url, uuid, password))
        return api_service_module.cookiecloud_tool.CookieCloudProbe(ok=True, code='ok', message='OK')

    monkeypatch.setattr(api_service_module.cookiecloud_tool, 'probe', _fake_probe)


_BILIBILI_WITH_CC = {
    'accounts': [
        {
            'name': 'main',
            'toview_enabled': True,
            'cookiecloud': {'server_url': 'https://cc.main', 'uuid': 'main-uuid', 'password': 'main-pw'},
        },
    ],
}


def test_test_cookiecloud_resolves_a_masked_password_from_storage(monkeypatch) -> None:
    service, probed = _cookiecloud_service(sections={'web.bilibili': _BILIBILI_WITH_CC})
    _stub_probe(monkeypatch, probed)

    result = service.test_cookiecloud(
        {'account': 'main', 'server_url': 'https://cc.main', 'uuid': 'main-uuid', 'password': 'main••••'},
    )

    assert result['ok'] is True
    assert probed == [('https://cc.main', 'main-uuid', 'main-pw')]


def test_test_cookiecloud_uses_an_unsaved_draft_password(monkeypatch) -> None:
    service, probed = _cookiecloud_service(sections={'web.bilibili': _BILIBILI_WITH_CC})
    _stub_probe(monkeypatch, probed)

    service.test_cookiecloud({'account': 'main', 'server_url': 'https://cc.new', 'uuid': 'new-uuid', 'password': 'typed-pw'})

    assert probed == [('https://cc.new', 'new-uuid', 'typed-pw')]


def test_test_cookiecloud_matches_the_stored_password_by_account_name(monkeypatch) -> None:
    sections = {
        'web.bilibili': {
            'accounts': [
                {
                    'name': 'main',
                    'toview_enabled': True,
                    'cookiecloud': {'server_url': 'https://cc.main', 'uuid': 'main-uuid', 'password': 'main-pw'},
                },
                {
                    'name': 'alt',
                    'toview_enabled': True,
                    'cookiecloud': {'server_url': 'https://cc.alt', 'uuid': 'alt-uuid', 'password': 'alt-pw'},
                },
            ],
        },
    }
    service, probed = _cookiecloud_service(sections=sections)
    _stub_probe(monkeypatch, probed)

    # The second account's mask must resolve to the second account's password.
    service.test_cookiecloud(
        {
            'account': 'alt',
            'server_url': 'https://cc.alt',
            'uuid': 'alt-uuid',
            'password': 'alt-••••',
        },
    )

    assert probed == [('https://cc.alt', 'alt-uuid', 'alt-pw')]


def test_test_cookiecloud_requires_an_account() -> None:
    service, _ = _cookiecloud_service(sections={})

    with pytest.raises(api_service_module.ApiError) as excinfo:
        service.test_cookiecloud({'server_url': 'https://cc.example', 'uuid': 'u', 'password': 'p'})

    assert excinfo.value.status_code == 422
    assert 'account' in excinfo.value.message


def test_test_cookiecloud_for_an_unknown_account_has_no_stored_password_to_fall_back_on(monkeypatch) -> None:
    service, probed = _cookiecloud_service(sections={'web.bilibili': _BILIBILI_WITH_CC})
    _stub_probe(monkeypatch, probed)

    service.test_cookiecloud({'account': 'ghost', 'server_url': 'https://cc.x', 'uuid': 'x', 'password': 'shor••••'})

    assert probed == [('https://cc.x', 'x', '')]


_TWITTER_WITH_CC = {
    'username': 'me',
    'cookiecloud': {'server_url': 'https://cc.x', 'uuid': 'x-uuid', 'password': 'x-pw'},
}


def test_test_cookiecloud_checks_x_cookies_when_the_source_says_twitter(monkeypatch) -> None:
    service, probed = _cookiecloud_service(sections={'web.twitter': _TWITTER_WITH_CC})
    profiles: list[object] = []

    def _fake_probe(server_url: str, uuid: str, password: str, **kwargs: object) -> api_service_module.cookiecloud_tool.CookieCloudProbe:
        probed.append((server_url, uuid, password))
        profiles.append(kwargs.get('profile'))
        return api_service_module.cookiecloud_tool.CookieCloudProbe(ok=True, code='ok', message='OK')

    monkeypatch.setattr(api_service_module.cookiecloud_tool, 'probe', _fake_probe)

    # No account: a twitter deployment holds exactly one vault, in its own section.
    result = service.test_cookiecloud(
        {'source': 'twitter', 'server_url': 'https://cc.x', 'uuid': 'x-uuid', 'password': 'x-pw••••'},
    )

    assert result['ok'] is True
    assert probed == [('https://cc.x', 'x-uuid', 'x-pw')]
    assert profiles == [api_service_module.cookiecloud_tool.TWITTER_PROFILE]


def test_test_cookiecloud_rejects_a_source_it_has_no_profile_for() -> None:
    service, _ = _cookiecloud_service(sections={})

    with pytest.raises(api_service_module.ApiError) as excinfo:
        service.test_cookiecloud({'source': 'myspace', 'server_url': 'https://cc.x', 'uuid': 'u', 'password': 'p'})

    assert excinfo.value.status_code == 422
    assert 'myspace' in excinfo.value.message
