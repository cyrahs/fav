# ruff: noqa: INP001, S101, SLF001, ANN001, ANN002, ANN003, ANN202, S106, EM101, PLR2004, TRY003

import asyncio
from datetime import UTC, datetime

import run as run_module
from src.service.jobs import ScheduledJob
from src.tool import telegram_bot
from src.tool.control_queue import STATUS_FAILED, STATUS_REJECTED, STATUS_SUCCEEDED, ControlRequest
from src.tool.notifications import WEBHOOK_ACTION_UPSERT, NotificationRecord
from src.tool.runtime_config import Hanime1ParserIncompatibleError


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


def _notification(
    *,
    notification_id: int = 7,
    attempt_count: int = 0,
    payload: str = '{"job":"bilibili"}',
    image_url: str = '',
) -> NotificationRecord:
    now = datetime(2026, 3, 2, tzinfo=UTC)
    return NotificationRecord(
        notification_id=notification_id,
        kind='job_failed',
        source='worker',
        title='Job failed: Bilibili',
        body='RuntimeError: boom',
        link_url='',
        image_url=image_url,
        payload=payload,
        dedupe_key='job_failed:bilibili:bilibili:download:BV1TEST',
        status='unread',
        markdown='*Job failed: Bilibili*\nRuntimeError: boom',
        disable_web_page_preview=True,
        disable_notification=False,
        webhook_action=WEBHOOK_ACTION_UPSERT,
        occurrence_count=1,
        event_version=3,
        pin=True,
        delivery_status='sending',
        attempt_count=attempt_count,
        next_attempt_at=now,
        created_at=now,
    )


def _telegram_config() -> telegram_bot.TelegramBotConfig:
    return telegram_bot.TelegramBotConfig(bot_token='123:token', chat_id='-100123')


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


def test_execute_control_request_marks_failed_cancelled_runner(monkeypatch) -> None:
    updates: list[dict[str, str]] = []

    async def _fake_update(request_id: int, *, status: str, result: str = '', error: str = '') -> None:
        updates.append({'request_id': str(request_id), 'status': status, 'result': result, 'error': error})

    async def _fake_runner() -> run_module.JobRunResult:
        raise asyncio.CancelledError

    monkeypatch.setattr(run_module, 'update_control_request', _fake_update)

    asyncio.run(
        run_module._execute_control_request(
            _request(target='telegram'),
            all_jobs=[_job(key='telegram', enabled=True)],
            runner_by_key={'telegram': _fake_runner},
        ),
    )

    assert updates == [{'request_id': '1', 'status': STATUS_FAILED, 'result': '', 'error': 'CancelledError'}]


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


def test_execute_control_request_all_stops_after_cancelled_runner(monkeypatch) -> None:
    updates: list[dict[str, str]] = []
    invoked: list[str] = []

    async def _fake_update(request_id: int, *, status: str, result: str = '', error: str = '') -> None:
        updates.append({'request_id': str(request_id), 'status': status, 'result': result, 'error': error})

    async def _cancelled_runner() -> run_module.JobRunResult:
        invoked.append('telegram')
        raise asyncio.CancelledError

    async def _unexpected_runner() -> run_module.JobRunResult:
        invoked.append('bilibili')
        return run_module.JobRunResult(job_key='bilibili', job_name='Bilibili', success=True)

    monkeypatch.setattr(run_module, 'update_control_request', _fake_update)

    asyncio.run(
        run_module._execute_control_request(
            _request(target='all'),
            all_jobs=[_job(key='telegram', enabled=True), _job(key='bilibili', enabled=True)],
            runner_by_key={
                'telegram': _cancelled_runner,
                'bilibili': _unexpected_runner,
            },
        ),
    )

    assert invoked == ['telegram']
    assert updates == [
        {
            'request_id': '1',
            'status': STATUS_FAILED,
            'result': 'telegram=failed',
            'error': 'telegram: CancelledError',
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


def test_run_job_uses_exception_notification_dedupe_key(monkeypatch) -> None:
    captured: dict[str, object] = {}

    class _DownloadError(RuntimeError):
        notification_dedupe_key = 'bilibili:download:BV1TEST'

    class _FailingWorker:
        async def update(self) -> None:
            msg = 'temporary network error'
            raise _DownloadError(msg)

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
    assert captured['dedupe_key'] == 'job_failed:bilibili:bilibili:download:BV1TEST'
    assert captured['payload']['dedupe_key'] == 'job_failed:bilibili:bilibili:download:BV1TEST'


def test_run_job_dedupes_hanime1_parser_failure_notification(monkeypatch) -> None:
    captured: dict[str, object] = {}

    class _FailingWorker:
        async def update(self) -> None:
            msg = 'playlist structure changed'
            raise Hanime1ParserIncompatibleError(msg)

    async def _fake_enqueue_notification(**payload) -> None:
        captured.update(payload)

    monkeypatch.setattr(run_module, 'enqueue_notification', _fake_enqueue_notification)

    result = asyncio.run(run_module._run_job(job=_job(key='hanime1'), worker=_FailingWorker()))

    assert result.success is False
    assert captured['dedupe_key'] == 'job_failed:hanime1:hanime1:parser:playlist-wrapper'
    assert captured['payload']['error_class'] == 'Hanime1ParserIncompatibleError'


def test_run_job_handles_cancelled_error_and_closes_worker(monkeypatch) -> None:
    captured: dict[str, object] = {}
    closed: list[str] = []

    class _CancelledWorker:
        async def update(self) -> None:
            raise asyncio.CancelledError

        async def aclose(self) -> None:
            closed.append('closed')

    async def _fake_enqueue_notification(**payload) -> None:
        captured.update(payload)

    monkeypatch.setattr(run_module, 'enqueue_notification', _fake_enqueue_notification)

    result = asyncio.run(
        run_module._run_job(
            job=ScheduledJob(
                key='telegram',
                name='Telegram',
                cron='*/30 * * * *',
                enabled=True,
                run_on_start=False,
                required_commands=(),
                factory=_CancelledWorker,
            ),
        ),
    )

    assert result.success is False
    assert result.error == 'CancelledError'
    assert result.cancelled is True
    assert closed == ['closed']
    assert captured['kind'] == 'job_failed'
    assert captured['payload']['error_class'] == 'CancelledError'


def test_run_job_can_reuse_singleton_worker_without_closing_it() -> None:
    calls: list[str] = []

    class _Worker:
        async def update(self) -> None:
            calls.append('update')

        async def aclose(self) -> None:
            calls.append('close')

    def _unexpected_factory() -> object:
        msg = 'The scheduled Telegram trigger must reuse its runtime'
        raise AssertionError(msg)

    result = asyncio.run(
        run_module._run_job(
            job=ScheduledJob(
                key='telegram',
                name='Telegram',
                cron='*/30 * * * *',
                enabled=True,
                run_on_start=False,
                required_commands=(),
                factory=_unexpected_factory,
            ),
            worker=_Worker(),
            close_worker=False,
        ),
    )

    assert result.success is True
    assert calls == ['update']


def test_deliver_next_notification_uses_direct_telegram(monkeypatch) -> None:
    delivered: list[tuple[int, int]] = []
    delivery_configs: list[telegram_bot.TelegramBotConfig] = []

    async def _fake_claim() -> NotificationRecord | None:
        return _notification()

    async def _fake_deliver(*, notification, client, config):
        assert notification.notification_id == 7
        assert client is not None
        delivery_configs.append(config)
        return telegram_bot.TelegramDeliveryResult(message_id=99, media_status='none')

    async def _fake_mark_delivered(notification_id: int, *, event_version: int) -> None:
        delivered.append((notification_id, event_version))

    monkeypatch.setattr(run_module, 'claim_next_pending_notification', _fake_claim)
    monkeypatch.setattr(run_module, '_load_telegram_bot_config', _telegram_config)
    monkeypatch.setattr(run_module.telegram_bot, 'deliver', _fake_deliver)
    monkeypatch.setattr(run_module, 'mark_notification_delivered', _fake_mark_delivered)

    processed = asyncio.run(run_module._deliver_next_notification(client=object()))

    assert processed is True
    assert delivery_configs == [_telegram_config()]
    assert delivered == [(7, 3)]


def test_deliver_next_notification_leaves_queue_untouched_when_telegram_is_disabled(monkeypatch) -> None:
    async def _unexpected_claim() -> NotificationRecord | None:
        msg = 'The outbox must not be claimed without an enabled Telegram destination'
        raise AssertionError(msg)

    monkeypatch.setattr(run_module, '_load_telegram_bot_config', lambda: None)
    monkeypatch.setattr(run_module, 'claim_next_pending_notification', _unexpected_claim)

    assert asyncio.run(run_module._deliver_next_notification(client=object())) is False


def test_direct_telegram_retry_uses_server_retry_after(monkeypatch) -> None:
    retried: list[dict[str, object]] = []

    async def _fake_deliver(**_kwargs):
        raise telegram_bot.TelegramDeliveryError('Telegram rate limited', retryable=True, retry_after_seconds=120)

    async def _fake_mark_retry(notification_id: int, **kwargs) -> None:
        retried.append({'notification_id': notification_id, **kwargs})

    monkeypatch.setattr(run_module.telegram_bot, 'deliver', _fake_deliver)
    monkeypatch.setattr(run_module, 'mark_notification_retry', _fake_mark_retry)

    asyncio.run(
        run_module._deliver_notification_to_telegram(
            notification=_notification(attempt_count=2),
            client=object(),
            telegram_config=_telegram_config(),
        ),
    )

    assert retried == [
        {
            'notification_id': 7,
            'event_version': 3,
            'attempt_count': 3,
            'error_message': 'Telegram rate limited',
            'retry_after_seconds': 120,
        },
    ]


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
    monkeypatch.setattr(run_module, '_validate_commands', lambda *_args, **_kwargs: None)
    monkeypatch.setattr(run_module, 'AsyncIOScheduler', _FakeScheduler)
    monkeypatch.setattr(run_module, '_consume_control_requests', _fake_consume_control_requests)
    monkeypatch.setattr(run_module, '_consume_notification_deliveries', _fake_consume_notifications)
    monkeypatch.setattr(run_module.telegram_bot, 'build_client', _FakeClient)

    asyncio.run(run_module.main())

    assert 'notification_consumer_started' in calls
    assert 'client_closed' in calls
