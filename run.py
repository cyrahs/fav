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

from src.core import logger, settings
from src.service.jobs import ScheduledJob, build_jobs, resolve_trigger_jobs
from src.tool import telegram_bot
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
    format_job_failure_dedupe_key,
    mark_notification_delivered,
    mark_notification_failed,
    mark_notification_retry,
)
from src.web.telegram import Telegram

log = logger.get('main')
_CONTROL_REQUEST_POLL_INTERVAL_SECONDS = 1.0
_SETTINGS_POLL_INTERVAL_SECONDS = 15.0
_NOTIFICATION_DELIVERY_POLL_INTERVAL_SECONDS = 1.0
_SHUTDOWN_TIMEOUT_SECONDS = 10.0


@dataclass(frozen=True, slots=True)
class JobRunResult:
    job_key: str
    job_name: str
    success: bool
    error: str = ''
    cancelled: bool = False


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


def _load_telegram_bot_config() -> telegram_bot.TelegramBotConfig | None:
    """Resolve the enabled direct Telegram delivery configuration."""
    return telegram_bot.load_config()


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
    dedupe_key = _exception_notification_dedupe_key(job=job, exc=exc)
    try:
        await enqueue_notification(
            kind='job_failed',
            source='worker',
            title=f'Job failed: {job.name}',
            body=error_message,
            dedupe_key=dedupe_key,
            payload={
                'job': job.key,
                'dedupe_key': dedupe_key,
                'started_at': _serialize_datetime(started_at),
                'finished_at': _serialize_datetime(finished_at),
                'elapsed_seconds': round(elapsed_seconds, 3),
                'error_class': exc.__class__.__name__,
                'error_message': str(exc),
            },
        )
    except Exception as notify_exc:  # noqa: BLE001
        log.warning('Failed to enqueue job_failed notification for %s: %s', job.key, notify_exc)


def _exception_notification_dedupe_key(*, job: ScheduledJob, exc: BaseException) -> str:
    failure_key = getattr(exc, 'notification_dedupe_key', '')
    if not isinstance(failure_key, str):
        return ''
    return format_job_failure_dedupe_key(job_key=job.key, failure_key=failure_key)


def _format_exception(exc: BaseException) -> str:
    message = str(exc)
    if not message:
        return exc.__class__.__name__
    return f'{exc.__class__.__name__}: {message}'


async def _deliver_notification_to_telegram(
    *,
    notification: NotificationRecord,
    client: httpx.AsyncClient,
    telegram_config: telegram_bot.TelegramBotConfig,
) -> None:
    attempt_count = notification.attempt_count + 1
    try:
        result = await telegram_bot.deliver(notification=notification, client=client, config=telegram_config)
    except telegram_bot.TelegramDeliveryError as exc:
        error_message = str(exc)
        if exc.retryable:
            await mark_notification_retry(
                notification.notification_id,
                event_version=notification.event_version,
                attempt_count=attempt_count,
                error_message=error_message,
                retry_after_seconds=exc.retry_after_seconds,
            )
            log.warning('Telegram delivery retry scheduled for notification %s: %s', notification.notification_id, error_message)
            return
        await mark_notification_failed(
            notification.notification_id,
            event_version=notification.event_version,
            attempt_count=attempt_count,
            error_message=error_message,
        )
        log.warning('Telegram delivery permanently failed for notification %s: %s', notification.notification_id, error_message)
        return

    await mark_notification_delivered(notification.notification_id, event_version=notification.event_version)
    log.info('Delivered notification %s through Telegram (message %s)', notification.notification_id, result.message_id)


async def _deliver_next_notification(*, client: httpx.AsyncClient) -> bool:
    # Resolve settings per attempt so UI changes take effect without restarting.
    telegram_config = _load_telegram_bot_config()
    if telegram_config is None:
        return False

    notification = await claim_next_pending_notification()
    if notification is None:
        return False

    await _deliver_notification_to_telegram(
        notification=notification,
        client=client,
        telegram_config=telegram_config,
    )
    return True


async def _drain_pending_notifications(*, client: httpx.AsyncClient) -> int:
    delivered = 0
    while await _deliver_next_notification(client=client):
        delivered += 1
    return delivered


async def _consume_notification_deliveries(
    *,
    client: httpx.AsyncClient,
    stop_event: asyncio.Event,
) -> None:
    warned_unconfigured = False
    while not stop_event.is_set():
        try:
            processed = await _deliver_next_notification(client=client)
        except Exception as exc:  # noqa: BLE001
            log.warning('Notification delivery loop failed: %s', exc)
            processed = False

        if processed:
            warned_unconfigured = False
            continue

        if not warned_unconfigured and _load_telegram_bot_config() is None:
            log.warning('Telegram notifications are not configured; notifications stay queued until bot_token and chat_id are set')
            warned_unconfigured = True

        try:
            await asyncio.wait_for(stop_event.wait(), timeout=_NOTIFICATION_DELIVERY_POLL_INTERVAL_SECONDS)
        except TimeoutError:
            continue


async def _run_job(*, job: ScheduledJob, worker: object | None = None, close_worker: bool = True) -> JobRunResult:
    started_at = datetime.now(tz=UTC)
    started_perf = perf_counter()
    log.notice('Job started: %s', job.name)
    selected_worker = worker if worker is not None else job.factory()
    try:
        await selected_worker.update()
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
        if close_worker:
            await _safe_close(job.name, selected_worker)


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


def _missing_commands(job: ScheduledJob) -> list[str]:
    return [command for command in job.required_commands if shutil.which(command) is None]


def _sync_scheduled_jobs(
    *,
    scheduler: AsyncIOScheduler,
    jobs: list[ScheduledJob],
    runner_by_key: dict[str, Callable[[], object]],
    timezone: tzinfo,
) -> int:
    """Bring the scheduler in line with `jobs`. Returns the number of changes made."""
    changes = 0
    for job in jobs:
        existing = scheduler.get_job(job.key)
        runner = runner_by_key.get(job.key)

        if not job.enabled or runner is None:
            if existing is not None:
                scheduler.remove_job(job.key)
                log.info('Unscheduled %s', job.name)
                changes += 1
            continue

        missing = _missing_commands(job)
        if missing:
            # Enabled from the UI on a host that cannot run it; keep it parked
            # rather than taking the whole worker down.
            log.error('%s requires command(s) %s in PATH; not scheduling', job.name, ', '.join(missing))
            if existing is not None:
                scheduler.remove_job(job.key)
                changes += 1
            continue

        try:
            trigger = CronTrigger.from_crontab(job.cron, timezone=timezone)
        except ValueError:
            log.exception('Invalid cron for %s (%s); not scheduling', job.name, job.cron)
            if existing is not None:
                scheduler.remove_job(job.key)
                changes += 1
            continue

        if existing is None:
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
            changes += 1
            continue

        if str(existing.trigger) != str(trigger):
            scheduler.reschedule_job(job.key, trigger=trigger)
            log.info('Rescheduled %s with cron %r', job.name, job.cron)
            changes += 1
    return changes


async def _watch_settings(
    *,
    scheduler: AsyncIOScheduler,
    runner_by_key: dict[str, Callable[[], object]],
    timezone: tzinfo,
    stop_event: asyncio.Event,
) -> None:
    """Reschedule jobs when the web UI edits their enabled/cron settings.

    Deliberately independent of the control-request loop, which runs jobs
    serially and would delay reschedules behind a long crawl.
    """
    last_version: str | None = None
    while not stop_event.is_set():
        try:
            version = await asyncio.to_thread(settings.settings_version_sync)
            if version != last_version:
                if last_version is not None:
                    await asyncio.to_thread(settings.load, force=True)
                    changes = _sync_scheduled_jobs(
                        scheduler=scheduler,
                        jobs=build_jobs(),
                        runner_by_key=runner_by_key,
                        timezone=timezone,
                    )
                    log.info('Applied settings change (%d schedule update(s))', changes)
                last_version = version
        except Exception as exc:  # noqa: BLE001
            log.warning('Settings watcher failed: %s', exc)

        try:
            await asyncio.wait_for(stop_event.wait(), timeout=_SETTINGS_POLL_INTERVAL_SECONDS)
        except TimeoutError:
            continue


async def main(*, trigger_target: str | None = None) -> None:  # noqa: C901, PLR0912, PLR0915
    control_request_task: asyncio.Task[None] | None = None
    notification_delivery_task: asyncio.Task[None] | None = None
    settings_watcher_task: asyncio.Task[None] | None = None
    telegram_runtime_task: asyncio.Task[None] | None = None
    telegram_ready_task: asyncio.Task[None] | None = None
    telegram_runtime: Telegram | None = None
    stop_event = asyncio.Event()
    main_task = asyncio.current_task()
    if main_task is None:
        msg = 'Failed to resolve main task for signal handling'
        raise RuntimeError(msg)
    remove_signal_handlers = _install_signal_handlers(stop_event=stop_event, main_task=main_task)
    notification_client = telegram_bot.build_client()
    all_jobs = build_jobs()
    jobs = [job for job in all_jobs if job.enabled]
    timezone = _resolve_scheduler_timezone()
    scheduler = AsyncIOScheduler(timezone=timezone)
    if trigger_target is None:
        # Missing commands no longer abort startup: enabling a source is a UI
        # action now, and a bad toggle must not crashloop the worker.
        # _sync_scheduled_jobs logs and parks those jobs instead.
        log.info('Scheduler timezone: %s', getattr(timezone, 'key', str(timezone)))

    runner_by_key: dict[str, Callable[[], object]] = {}
    telegram_job = next((job for job in all_jobs if job.key == 'telegram'), None)
    if trigger_target is None and telegram_job is not None and telegram_job.enabled:
        telegram_runtime = Telegram()

    try:
        for job in all_jobs:
            lock = asyncio.Lock()

            async def runner(*, _job: ScheduledJob = job, _lock: asyncio.Lock = lock) -> JobRunResult:
                if _lock.locked():
                    log.warning('%s is still running, skip this trigger', _job.name)
                    return JobRunResult(job_key=_job.key, job_name=_job.name, success=False, error='Job is already running')
                async with _lock:
                    if _job.key == 'telegram' and telegram_runtime is not None:
                        return await _run_job(job=_job, worker=telegram_runtime, close_worker=False)
                    return await _run_job(job=_job)

            runner_by_key[job.key] = runner

        if trigger_target is None:
            for job in all_jobs:
                if not job.enabled:
                    log.info('%s is disabled; skipping schedule', job.name)
            _sync_scheduled_jobs(
                scheduler=scheduler,
                jobs=all_jobs,
                runner_by_key=runner_by_key,
                timezone=timezone,
            )

        if trigger_target is not None:
            trigger_jobs = resolve_trigger_jobs(trigger_target, all_jobs)
            _validate_commands(trigger_jobs, respect_enabled=False)
            for job in trigger_jobs:
                if not job.enabled:
                    log.warning('%s is disabled in config; running once due to --trigger', job.name)
                await runner_by_key[job.key]()
            await _drain_pending_notifications(client=notification_client)
            return

        if not jobs:
            log.warning('No jobs enabled. Waiting indefinitely.')

        if telegram_runtime is not None:
            telegram_runtime_task = asyncio.create_task(
                telegram_runtime.run(stop_event),
                name='telegram-runtime',
            )
            telegram_ready_task = asyncio.create_task(telegram_runtime.wait_until_ready(), name='telegram-runtime-ready')
            done, _ = await asyncio.wait({telegram_runtime_task, telegram_ready_task}, return_when=asyncio.FIRST_COMPLETED)
            if telegram_runtime_task in done:
                telegram_ready_task.cancel()
                await asyncio.gather(telegram_ready_task, return_exceptions=True)
                await telegram_runtime_task
                msg = 'Telegram runtime exited before all accounts became ready'
                raise RuntimeError(msg)
            await telegram_ready_task
            log.info('Telegram event listeners and queue workers enabled')

        control_request_task = asyncio.create_task(
            _consume_control_requests(all_jobs=all_jobs, runner_by_key=runner_by_key, stop_event=stop_event),
            name='control-request-consumer',
        )
        notification_delivery_task = asyncio.create_task(
            _consume_notification_deliveries(client=notification_client, stop_event=stop_event),
            name='notification-delivery-consumer',
        )
        settings_watcher_task = asyncio.create_task(
            _watch_settings(
                scheduler=scheduler,
                runner_by_key=runner_by_key,
                timezone=timezone,
                stop_event=stop_event,
            ),
            name='settings-watcher',
        )
        log.info('Control request consumer enabled')
        log.info('Notification delivery consumer enabled')
        log.info('Settings watcher enabled (poll every %ss)', int(_SETTINGS_POLL_INTERVAL_SECONDS))

        scheduler.start()
        for job in jobs:
            if job.run_on_start:
                log.info('Run-on-start enabled for %s', job.name)
                await runner_by_key[job.key]()

        if telegram_runtime_task is None:
            await stop_event.wait()
        else:
            stop_task = asyncio.create_task(stop_event.wait(), name='main-stop-wait')
            done, pending = await asyncio.wait({stop_task, telegram_runtime_task}, return_when=asyncio.FIRST_COMPLETED)
            for task in pending:
                if task is stop_task:
                    task.cancel()
            if telegram_runtime_task in done and not stop_event.is_set():
                await telegram_runtime_task
                msg = 'Telegram runtime exited unexpectedly'
                raise RuntimeError(msg)
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
        await _shutdown_task(settings_watcher_task, name='settings watcher')
        if telegram_runtime is not None:
            await telegram_runtime.aclose()
        await _shutdown_task(telegram_ready_task, name='telegram runtime readiness')
        await _shutdown_task(telegram_runtime_task, name='telegram runtime')
        await notification_client.aclose()
        if scheduler.running:
            scheduler.shutdown(wait=False)


if __name__ == '__main__':
    args = _parse_args()
    asyncio.run(main(trigger_target=args.trigger))
