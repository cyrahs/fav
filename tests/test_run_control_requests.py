# ruff: noqa: INP001, S101, SLF001, ANN001

import asyncio

import run as run_module
from src.service.jobs import ScheduledJob
from src.tool.control_queue import STATUS_FAILED, STATUS_REJECTED, STATUS_SUCCEEDED, ControlRequest


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

    async def _fake_enqueue_notification(**payload) -> None:  # noqa: ANN003
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
