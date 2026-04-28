# ruff: noqa: INP001, S101, SLF001, ANN001, ANN002, ANN003, ANN202, S106, EM101

import asyncio
from datetime import UTC, datetime
from types import SimpleNamespace

import httpx
import pytest

import run as run_module
from src.service.jobs import ScheduledJob
from src.tool.control_queue import STATUS_FAILED, STATUS_REJECTED, STATUS_SUCCEEDED, ControlRequest
from src.tool.notifications import NotificationRecord


def _job(*, key: str, enabled: bool = True) -> ScheduledJob:
    return ScheduledJob(
        key=key,
        name=key.title(),
        cron='*/30 * * * *',
        enabled=enabled,
        run_on_start=False,
        required_commands=(),
        factory=object,
    )


def _request(*, target: str, kind: str = 'trigger_job') -> ControlRequest:
    return ControlRequest(
        request_id=1,
        kind=kind,
        target=target,
        payload='{}',
        status='running',
        requested_at=run_module.datetime.now(run_module.UTC),
    )


def _notification(*, notification_id: int = 7, attempt_count: int = 0) -> NotificationRecord:
    now = datetime(2026, 3, 2, tzinfo=UTC)
    return NotificationRecord(
        notification_id=notification_id,
        kind='job_failed',
        source='worker',
        title='Job failed: Bilibili',
        body='RuntimeError: boom',
        link_url='',
        image_url='',
        payload='{"job":"bilibili"}',
        status='unread',
        markdown='*Job failed: Bilibili*\nRuntimeError: boom',
        disable_web_page_preview=True,
        disable_notification=False,
        pin=True,
        delivery_status='sending',
        attempt_count=attempt_count,
        next_attempt_at=now,
        created_at=now,
    )


def test_execute_control_request_rejects_disabled_target(monkeypatch) -> None:
    updates: list[dict[str, str]] = []

    async def _fake_update(request_id: int, *, status: str, result: str = '', error: str = '') -> None:
        updates.append({'request_id': str(request_id), 'status': status, 'result': result, 'error': error})

    monkeypatch.setattr(run_module, 'update_control_request', _fake_update)

    asyncio.run(
        run_module._execute_control_request(
            _request(target='telegram'),
            all_jobs=[_job(key='telegram', enabled=False)],
            runner_by_key={},
        ),
    )

    assert updates == [{'request_id': '1', 'status': STATUS_REJECTED, 'result': '', 'error': 'Job is disabled: telegram'}]


def test_execute_control_request_marks_failed_runner(monkeypatch) -> None:
    updates: list[dict[str, str]] = []

    async def _fake_update(request_id: int, *, status: str, result: str = '', error: str = '') -> None:
        updates.append({'request_id': str(request_id), 'status': status, 'result': result, 'error': error})

    async def _fake_runner() -> run_module.JobRunResult:
        return run_module.JobRunResult(job_key='bilibili', job_name='Bilibili', success=False, error='RuntimeError: boom')

    monkeypatch.setattr(run_module, 'update_control_request', _fake_update)

    asyncio.run(
        run_module._execute_control_request(
            _request(target='bilibili'),
            all_jobs=[_job(key='bilibili', enabled=True)],
            runner_by_key={'bilibili': _fake_runner},
        ),
    )

    assert updates == [{'request_id': '1', 'status': STATUS_FAILED, 'result': '', 'error': 'RuntimeError: boom'}]


def test_execute_control_request_runs_all_enabled_jobs(monkeypatch) -> None:
    updates: list[dict[str, str]] = []
    invoked: list[str] = []

    async def _fake_update(request_id: int, *, status: str, result: str = '', error: str = '') -> None:
        updates.append({'request_id': str(request_id), 'status': status, 'result': result, 'error': error})

    async def _runner_factory(key: str) -> run_module.JobRunResult:
        invoked.append(key)
        return run_module.JobRunResult(job_key=key, job_name=key.title(), success=True)

    monkeypatch.setattr(run_module, 'update_control_request', _fake_update)

    asyncio.run(
        run_module._execute_control_request(
            _request(target='all'),
            all_jobs=[_job(key='bilibili', enabled=True), _job(key='telegram', enabled=False), _job(key='hanime1', enabled=True)],
            runner_by_key={
                'bilibili': lambda: _runner_factory('bilibili'),
                'hanime1': lambda: _runner_factory('hanime1'),
            },
        ),
    )

    assert invoked == ['bilibili', 'hanime1']
    assert updates == [
        {
            'request_id': '1',
            'status': STATUS_SUCCEEDED,
            'result': 'bilibili=ok, hanime1=ok',
            'error': '',
        },
    ]


def test_run_job_enqueues_job_failed_notification(monkeypatch) -> None:
    captured: dict[str, object] = {}

    class _FailingWorker:
        async def update(self) -> None:
            msg = 'boom'
            raise RuntimeError(msg)

    async def _fake_enqueue_notification(**payload) -> None:
        captured.update(payload)

    monkeypatch.setattr(run_module, 'enqueue_notification', _fake_enqueue_notification)

    result = asyncio.run(
        run_module._run_job(
            job=ScheduledJob(
                key='bilibili',
                name='Bilibili',
                cron='*/30 * * * *',
                enabled=True,
                run_on_start=False,
                required_commands=(),
                factory=_FailingWorker,
            ),
        ),
    )

    assert result.success is False
    assert result.error == 'RuntimeError: boom'
    assert captured['kind'] == 'job_failed'
    assert captured['source'] == 'worker'
    assert captured['title'] == 'Job failed: Bilibili'
    assert captured['body'] == 'RuntimeError: boom'
    assert captured['payload']['job'] == 'bilibili'
    assert captured['payload']['error_class'] == 'RuntimeError'
    assert captured['payload']['error_message'] == 'boom'


def test_load_notification_webhook_config_requires_values(monkeypatch) -> None:
    monkeypatch.setattr(run_module, 'app_config', SimpleNamespace(notifications=SimpleNamespace(webhook_base_url='', webhook_token='')))

    with pytest.raises(ValueError, match=r'notifications\.webhook_base_url is required'):
        run_module._load_notification_webhook_config()


def test_deliver_next_notification_marks_delivered(monkeypatch) -> None:
    delivered: list[int] = []
    posted_payloads: list[dict[str, object]] = []

    async def _fake_claim() -> NotificationRecord | None:
        return _notification()

    async def _fake_mark_delivered(notification_id: int) -> None:
        delivered.append(notification_id)

    class _FakeClient:
        async def post(self, *_args, **kwargs):
            posted_payloads.append(kwargs['json'])
            return SimpleNamespace(status_code=204, text='')

    monkeypatch.setattr(run_module, 'claim_next_pending_notification', _fake_claim)
    monkeypatch.setattr(run_module, 'mark_notification_delivered', _fake_mark_delivered)

    processed = asyncio.run(
        run_module._deliver_next_notification(
            client=_FakeClient(),
            webhook_config=run_module.NotificationWebhookConfig(
                url='https://hooks.example.com/api/v2/notifications/webhook',
                token='token',
            ),
        ),
    )

    assert processed is True
    assert delivered == [7]
    assert posted_payloads[0]['pin'] is True


def test_deliver_next_notification_retries_request_error(monkeypatch) -> None:
    retried: list[tuple[int, int, str]] = []

    async def _fake_claim() -> NotificationRecord | None:
        return _notification(attempt_count=2)

    async def _fake_mark_retry(notification_id: int, *, attempt_count: int, error_message: str) -> None:
        retried.append((notification_id, attempt_count, error_message))

    class _FakeClient:
        async def post(self, *_args, **_kwargs):
            request = httpx.Request('POST', 'https://hooks.example.com/api/v2/notifications/webhook')
            raise httpx.RequestError('boom', request=request)

    monkeypatch.setattr(run_module, 'claim_next_pending_notification', _fake_claim)
    monkeypatch.setattr(run_module, 'mark_notification_retry', _fake_mark_retry)

    processed = asyncio.run(
        run_module._deliver_next_notification(
            client=_FakeClient(),
            webhook_config=run_module.NotificationWebhookConfig(
                url='https://hooks.example.com/api/v2/notifications/webhook',
                token='token',
            ),
        ),
    )

    assert processed is True
    assert retried == [(7, 3, 'RequestError: boom')]


def test_main_starts_notification_consumer_and_closes_client(monkeypatch) -> None:
    calls: list[str] = []

    class _FakeScheduler:
        def __init__(self, *_args, **_kwargs) -> None:
            self.running = False

        def start(self) -> None:
            self.running = True

        def shutdown(self, *, wait: bool = False) -> None:
            calls.append(f'shutdown:{wait}')
            self.running = False

    class _FakeClient:
        def __init__(self, *_args, **_kwargs) -> None:
            self.closed = False

        async def aclose(self) -> None:
            self.closed = True
            calls.append('client_closed')

    async def _fake_consume_control_requests(*, stop_event, **_kwargs) -> None:
        await stop_event.wait()

    async def _fake_consume_notifications(*, stop_event, **_kwargs) -> None:
        calls.append('notification_consumer_started')
        stop_event.set()

    monkeypatch.setattr(run_module, 'build_jobs', list)
    monkeypatch.setattr(
        run_module,
        '_load_notification_webhook_config',
        lambda: run_module.NotificationWebhookConfig(url='https://hooks.example.com/api/v2/notifications/webhook', token='token'),
    )
    monkeypatch.setattr(run_module, '_validate_commands', lambda *_args, **_kwargs: None)
    monkeypatch.setattr(run_module, 'AsyncIOScheduler', _FakeScheduler)
    monkeypatch.setattr(run_module, '_consume_control_requests', _fake_consume_control_requests)
    monkeypatch.setattr(run_module, '_consume_notification_deliveries', _fake_consume_notifications)
    monkeypatch.setattr(run_module.httpx, 'AsyncClient', _FakeClient)

    asyncio.run(run_module.main())

    assert 'notification_consumer_started' in calls
    assert 'client_closed' in calls
