import argparse
import asyncio
import contextlib
import inspect
import os
import shutil
import signal
from collections.abc import Callable
from dataclasses import dataclass
from datetime import UTC, datetime, tzinfo
from time import perf_counter
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

import httpx
from apscheduler.schedulers.asyncio import AsyncIOScheduler
from apscheduler.triggers.cron import CronTrigger

from src.core import logger
from src.core.config import config as app_config
from src.service.jobs import ScheduledJob, build_jobs, resolve_trigger_jobs
from src.tool.control_queue import (
    STATUS_FAILED,
    STATUS_REJECTED,
    STATUS_SUCCEEDED,
    ControlRequest,
    claim_next_control_request,
    ensure_control_requests_table,
    fail_stale_running_control_requests,
    update_control_request,
)
from src.tool.notifications import (
    NotificationRecord,
    claim_next_pending_notification,
    enqueue_notification,
    mark_notification_delivered,
    mark_notification_failed,
    mark_notification_retry,
)

log = logger.get('main')
_CONTROL_REQUEST_POLL_INTERVAL_SECONDS = 1.0
_NOTIFICATION_DELIVERY_POLL_INTERVAL_SECONDS = 1.0
_SHUTDOWN_TIMEOUT_SECONDS = 10.0
_WEBHOOK_TIMEOUT_SECONDS = 10.0
_NOTIFICATION_WEBHOOK_PATH = '/api/v2/notifications/webhook'
_HTTP_STATUS_SUCCESS_MIN = 200
_HTTP_STATUS_SUCCESS_MAX = 299
_HTTP_STATUS_SERVER_ERROR_MIN = 500
_HTTP_STATUS_SERVER_ERROR_MAX = 599


@dataclass(frozen=True, slots=True)
class JobRunResult:
    job_key: str
    job_name: str
    success: bool
    error: str = ''
    cancelled: bool = False


@dataclass(frozen=True, slots=True)
class NotificationWebhookConfig:
    url: str
    token: str


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description='fav scheduler')
    parser.add_argument(
        '--trigger',
        metavar='TARGET',
        help='Run one job immediately and exit. Use a job key or "all".',
    )
    return parser.parse_args()


def _validate_commands(jobs: list[ScheduledJob], *, respect_enabled: bool = True) -> None:
    missing: list[tuple[str, str]] = []
    for job in jobs:
        if respect_enabled and not job.enabled:
            continue
        for cmd in job.required_commands:
            if shutil.which(cmd):
                continue
            missing.append((job.name, cmd))

    if not missing:
        return

    for job_name, cmd in missing:
        log.error('%s requires command %r in PATH', job_name, cmd)
    raise SystemExit(1)


def _resolve_scheduler_timezone() -> tzinfo:
    tz_name = os.getenv('TZ', 'UTC').strip() or 'UTC'
    try:
        return ZoneInfo(tz_name)
    except ZoneInfoNotFoundError:
        log.warning('Invalid TZ %r, fallback to UTC', tz_name)
        return UTC


def _load_notification_webhook_config() -> NotificationWebhookConfig:
    base_url = str(app_config.notifications.webhook_base_url).strip().rstrip('/')
    token = str(app_config.notifications.webhook_token).strip()
    if not base_url:
        msg = 'notifications.webhook_base_url is required'
        raise ValueError(msg)
    if not token:
        msg = 'notifications.webhook_token is required'
        raise ValueError(msg)
    return NotificationWebhookConfig(url=f'{base_url}{_NOTIFICATION_WEBHOOK_PATH}', token=token)


async def _shutdown_task(task: asyncio.Task[None] | None, *, name: str) -> None:
    if task is None:
        return
    task.cancel()
    try:
        await asyncio.wait_for(task, timeout=_SHUTDOWN_TIMEOUT_SECONDS)
    except TimeoutError:
        log.warning('Timed out while waiting %s to stop', name)
    except asyncio.CancelledError:
        pass
    except Exception as exc:  # noqa: BLE001
        log.warning('%s stop failed: %s', name, exc)


def _install_signal_handlers(  # noqa: C901
    *,
    stop_event: asyncio.Event,
    main_task: asyncio.Task[None],
) -> Callable[[], None]:
    restored_handlers: dict[signal.Signals, object] = {}

    def _request_shutdown(signal_name: str) -> None:
        if stop_event.is_set():
            log.info('Received %s again, shutdown already in progress', signal_name)
            return

        log.info('Received %s, starting graceful shutdown', signal_name)
        stop_event.set()
        if not main_task.done():
            main_task.cancel()

    def _signal_handler(signum: int, _frame: object) -> None:
        is_first_signal = not stop_event.is_set()
        try:
            signal_name = signal.Signals(signum).name
        except ValueError:
            signal_name = f'SIGNAL-{signum}'

        _request_shutdown(signal_name)
        if is_first_signal:
            raise KeyboardInterrupt

    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            restored_handlers[sig] = signal.getsignal(sig)
            signal.signal(sig, _signal_handler)
        except ValueError as exc:
            log.warning('Failed to install %s handler: %s', sig.name, exc)

    def _remove_handlers() -> None:
        for sig, previous in restored_handlers.items():
            with contextlib.suppress(Exception):
                signal.signal(sig, previous)

    return _remove_handlers


def _serialize_datetime(value: datetime) -> str:
    return value.astimezone(UTC).replace(microsecond=0).isoformat().replace('+00:00', 'Z')


async def _safe_close(job_name: str, worker: object) -> None:
    aclose = getattr(worker, 'aclose', None)
    if not callable(aclose):
        return
    try:
        maybe_awaitable = aclose()
        if inspect.isawaitable(maybe_awaitable):
            await maybe_awaitable
    except Exception as exc:  # noqa: BLE001
        log.warning('Failed to close %s resources: %s', job_name, exc)


async def _enqueue_job_failed_notification(
    *,
    job: ScheduledJob,
    started_at: datetime,
    finished_at: datetime,
    elapsed_seconds: float,
    exc: BaseException,
) -> None:
    error_message = _format_exception(exc)
    try:
        await enqueue_notification(
            kind='job_failed',
            source='worker',
            title=f'Job failed: {job.name}',
            body=error_message,
            payload={
                'job': job.key,
                'started_at': _serialize_datetime(started_at),
                'finished_at': _serialize_datetime(finished_at),
                'elapsed_seconds': round(elapsed_seconds, 3),
                'error_class': exc.__class__.__name__,
                'error_message': str(exc),
            },
        )
    except Exception as notify_exc:  # noqa: BLE001
        log.warning('Failed to enqueue job_failed notification for %s: %s', job.key, notify_exc)


def _format_exception(exc: BaseException) -> str:
    message = str(exc)
    if not message:
        return exc.__class__.__name__
    return f'{exc.__class__.__name__}: {message}'


def _notification_webhook_headers(token: str) -> dict[str, str]:
    return {'Authorization': f'Bearer {token}'}


def _notification_error_message(*, status_code: int, response_text: str) -> str:
    detail = response_text.strip()
    if detail:
        return f'Webhook responded with HTTP {status_code}: {detail[:200]}'
    return f'Webhook responded with HTTP {status_code}'


def _is_retryable_status_code(status_code: int) -> bool:
    return status_code in {408, 429} or _HTTP_STATUS_SERVER_ERROR_MIN <= status_code <= _HTTP_STATUS_SERVER_ERROR_MAX


async def _deliver_notification_via_webhook(
    *,
    notification: NotificationRecord,
    client: httpx.AsyncClient,
    webhook_config: NotificationWebhookConfig,
) -> None:
    response = await client.post(
        webhook_config.url,
        headers=_notification_webhook_headers(webhook_config.token),
        json=notification.webhook_payload,
    )
    if _HTTP_STATUS_SUCCESS_MIN <= response.status_code <= _HTTP_STATUS_SUCCESS_MAX:
        await mark_notification_delivered(notification.notification_id)
        log.info('Delivered notification %s', notification.notification_id)
        return

    attempt_count = notification.attempt_count + 1
    error_message = _notification_error_message(status_code=response.status_code, response_text=response.text)
    if _is_retryable_status_code(response.status_code):
        await mark_notification_retry(
            notification.notification_id,
            attempt_count=attempt_count,
            error_message=error_message,
        )
        log.warning('Webhook delivery retry scheduled for notification %s: %s', notification.notification_id, error_message)
        return

    await mark_notification_failed(
        notification.notification_id,
        attempt_count=attempt_count,
        error_message=error_message,
    )
    log.warning('Webhook delivery permanently failed for notification %s: %s', notification.notification_id, error_message)


async def _deliver_next_notification(
    *,
    client: httpx.AsyncClient,
    webhook_config: NotificationWebhookConfig,
) -> bool:
    notification = await claim_next_pending_notification()
    if notification is None:
        return False

    attempt_count = notification.attempt_count + 1
    try:
        await _deliver_notification_via_webhook(
            notification=notification,
            client=client,
            webhook_config=webhook_config,
        )
    except (httpx.RequestError, httpx.TimeoutException) as exc:
        error_message = f'{exc.__class__.__name__}: {exc}'
        await mark_notification_retry(
            notification.notification_id,
            attempt_count=attempt_count,
            error_message=error_message,
        )
        log.warning('Webhook request failed for notification %s: %s', notification.notification_id, error_message)
    return True


async def _drain_pending_notifications(
    *,
    client: httpx.AsyncClient,
    webhook_config: NotificationWebhookConfig,
) -> int:
    delivered = 0
    while await _deliver_next_notification(client=client, webhook_config=webhook_config):
        delivered += 1
    return delivered


async def _consume_notification_deliveries(
    *,
    client: httpx.AsyncClient,
    webhook_config: NotificationWebhookConfig,
    stop_event: asyncio.Event,
) -> None:
    while not stop_event.is_set():
        try:
            processed = await _deliver_next_notification(client=client, webhook_config=webhook_config)
        except Exception as exc:  # noqa: BLE001
            log.warning('Notification delivery loop failed: %s', exc)
            processed = False

        if processed:
            continue

        try:
            await asyncio.wait_for(stop_event.wait(), timeout=_NOTIFICATION_DELIVERY_POLL_INTERVAL_SECONDS)
        except TimeoutError:
            continue


async def _run_job(*, job: ScheduledJob) -> JobRunResult:
    started_at = datetime.now(tz=UTC)
    started_perf = perf_counter()
    log.notice('Job started: %s', job.name)
    worker = job.factory()
    try:
        await worker.update()
    except asyncio.CancelledError as exc:
        elapsed = perf_counter() - started_perf
        finished_at = datetime.now(tz=UTC)
        await _enqueue_job_failed_notification(
            job=job,
            started_at=started_at,
            finished_at=finished_at,
            elapsed_seconds=elapsed,
            exc=exc,
        )
        log.exception('Job cancelled: %s', job.name)
        return JobRunResult(job_key=job.key, job_name=job.name, success=False, error=_format_exception(exc), cancelled=True)
    except Exception as exc:
        elapsed = perf_counter() - started_perf
        finished_at = datetime.now(tz=UTC)
        await _enqueue_job_failed_notification(
            job=job,
            started_at=started_at,
            finished_at=finished_at,
            elapsed_seconds=elapsed,
            exc=exc,
        )
        log.exception('Job failed: %s', job.name)
        return JobRunResult(job_key=job.key, job_name=job.name, success=False, error=_format_exception(exc))
    else:
        elapsed = perf_counter() - started_perf
        log.notice('Job completed: %s (%.1fs)', job.name, elapsed)
        return JobRunResult(job_key=job.key, job_name=job.name, success=True)
    finally:
        await _safe_close(job.name, worker)


async def _run_control_runner(*, job: ScheduledJob, runner: Callable[[], object]) -> JobRunResult:
    try:
        return await runner()
    except asyncio.CancelledError as exc:
        return JobRunResult(job_key=job.key, job_name=job.name, success=False, error=_format_exception(exc), cancelled=True)
    except Exception as exc:  # noqa: BLE001
        return JobRunResult(job_key=job.key, job_name=job.name, success=False, error=_format_exception(exc))


async def _execute_control_request(  # noqa: C901, PLR0911
    request: ControlRequest,
    *,
    all_jobs: list[ScheduledJob],
    runner_by_key: dict[str, Callable[[], object]],
) -> None:
    if request.kind != 'trigger_job':
        await update_control_request(
            request.request_id,
            status=STATUS_REJECTED,
            error=f'Unsupported control request kind: {request.kind}',
        )
        return

    jobs_by_key = {job.key: job for job in all_jobs}
    target = request.target.strip().lower()
    if target == 'all':
        enabled_jobs = [job for job in all_jobs if job.enabled]
        if not enabled_jobs:
            await update_control_request(
                request.request_id,
                status=STATUS_REJECTED,
                error='No enabled jobs available for target all',
            )
            return

        results: list[JobRunResult] = []
        for job in enabled_jobs:
            runner = runner_by_key.get(job.key)
            if runner is None:
                results.append(JobRunResult(job_key=job.key, job_name=job.name, success=False, error='Runner unavailable'))
                continue
            result = await _run_control_runner(job=job, runner=runner)
            results.append(result)
            if result.cancelled:
                break

        failures = [result for result in results if not result.success]
        summary = ', '.join(f'{result.job_key}={"ok" if result.success else "failed"}' for result in results)
        if failures:
            error = '; '.join(f'{result.job_key}: {result.error}' for result in failures)
            await update_control_request(request.request_id, status=STATUS_FAILED, result=summary, error=error)
            return

        await update_control_request(request.request_id, status=STATUS_SUCCEEDED, result=summary)
        return

    job = jobs_by_key.get(target)
    if job is None:
        await update_control_request(
            request.request_id,
            status=STATUS_REJECTED,
            error=f'Unknown job target: {target}',
        )
        return
    if not job.enabled:
        await update_control_request(
            request.request_id,
            status=STATUS_REJECTED,
            error=f'Job is disabled: {target}',
        )
        return

    runner = runner_by_key.get(target)
    if runner is None:
        await update_control_request(
            request.request_id,
            status=STATUS_REJECTED,
            error=f'Runner unavailable for job: {target}',
        )
        return

    result = await _run_control_runner(job=job, runner=runner)
    if result.success:
        await update_control_request(request.request_id, status=STATUS_SUCCEEDED, result=f'Completed {target}.')
        return
    await update_control_request(request.request_id, status=STATUS_FAILED, error=result.error)


async def _consume_control_requests(
    *,
    all_jobs: list[ScheduledJob],
    runner_by_key: dict[str, Callable[[], object]],
    stop_event: asyncio.Event,
) -> None:
    await ensure_control_requests_table()
    stale_count = await fail_stale_running_control_requests()
    if stale_count:
        log.warning('Marked %d stale control requests as failed', stale_count)
    while not stop_event.is_set():
        request = await claim_next_control_request()
        if request is None:
            try:
                await asyncio.wait_for(stop_event.wait(), timeout=_CONTROL_REQUEST_POLL_INTERVAL_SECONDS)
            except TimeoutError:
                continue
            break
        await _execute_control_request(request, all_jobs=all_jobs, runner_by_key=runner_by_key)


async def main(*, trigger_target: str | None = None) -> None:  # noqa: C901, PLR0912, PLR0915
    control_request_task: asyncio.Task[None] | None = None
    notification_delivery_task: asyncio.Task[None] | None = None
    stop_event = asyncio.Event()
    main_task = asyncio.current_task()
    if main_task is None:
        msg = 'Failed to resolve main task for signal handling'
        raise RuntimeError(msg)
    remove_signal_handlers = _install_signal_handlers(stop_event=stop_event, main_task=main_task)
    webhook_config = _load_notification_webhook_config()
    notification_client = httpx.AsyncClient(timeout=_WEBHOOK_TIMEOUT_SECONDS)
    all_jobs = build_jobs()
    jobs = [job for job in all_jobs if job.enabled]
    timezone = _resolve_scheduler_timezone()
    scheduler = AsyncIOScheduler(timezone=timezone)
    if trigger_target is None:
        _validate_commands(jobs)
        log.info('Scheduler timezone: %s', getattr(timezone, 'key', str(timezone)))

    runner_by_key: dict[str, Callable[[], object]] = {}

    try:
        for job in all_jobs:
            lock = asyncio.Lock()

            async def runner(*, _job: ScheduledJob = job, _lock: asyncio.Lock = lock) -> JobRunResult:
                if _lock.locked():
                    log.warning('%s is still running, skip this trigger', _job.name)
                    return JobRunResult(job_key=_job.key, job_name=_job.name, success=False, error='Job is already running')
                async with _lock:
                    return await _run_job(job=_job)

            runner_by_key[job.key] = runner

            if trigger_target is not None:
                continue

            if not job.enabled:
                log.info('%s is disabled; skipping schedule', job.name)
                continue

            try:
                trigger = CronTrigger.from_crontab(job.cron, timezone=timezone)
            except ValueError as exc:
                log.exception('Invalid cron for %s (%s)', job.name, job.cron)
                raise SystemExit(1) from exc

            scheduler.add_job(
                runner,
                trigger=trigger,
                id=job.key,
                name=job.name,
                coalesce=True,
                max_instances=1,
                misfire_grace_time=300,
            )
            log.info('Scheduled %s with cron %r', job.name, job.cron)

        if trigger_target is not None:
            trigger_jobs = resolve_trigger_jobs(trigger_target, all_jobs)
            _validate_commands(trigger_jobs, respect_enabled=False)
            for job in trigger_jobs:
                if not job.enabled:
                    log.warning('%s is disabled in config; running once due to --trigger', job.name)
                await runner_by_key[job.key]()
            await _drain_pending_notifications(client=notification_client, webhook_config=webhook_config)
            return

        if not jobs:
            log.warning('No jobs enabled. Waiting indefinitely.')

        control_request_task = asyncio.create_task(
            _consume_control_requests(all_jobs=all_jobs, runner_by_key=runner_by_key, stop_event=stop_event),
            name='control-request-consumer',
        )
        notification_delivery_task = asyncio.create_task(
            _consume_notification_deliveries(client=notification_client, webhook_config=webhook_config, stop_event=stop_event),
            name='notification-delivery-consumer',
        )
        log.info('Control request consumer enabled')
        log.info('Notification delivery consumer enabled')

        scheduler.start()
        for job in jobs:
            if job.run_on_start:
                log.info('Run-on-start enabled for %s', job.name)
                await runner_by_key[job.key]()

        await stop_event.wait()
    except KeyboardInterrupt:
        if not stop_event.is_set():
            stop_event.set()
            log.info('Shutdown signal received')
    except asyncio.CancelledError:
        if not stop_event.is_set():
            stop_event.set()
            log.info('Main task cancelled; shutdown signal received')
    finally:
        remove_signal_handlers()
        await _shutdown_task(control_request_task, name='control request consumer')
        await _shutdown_task(notification_delivery_task, name='notification delivery consumer')
        await notification_client.aclose()
        if scheduler.running:
            scheduler.shutdown(wait=False)


if __name__ == '__main__':
    args = _parse_args()
    asyncio.run(main(trigger_target=args.trigger))
