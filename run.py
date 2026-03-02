import argparse
import asyncio
import contextlib
import inspect
import os
import shutil
import signal
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from datetime import UTC, datetime, tzinfo
from time import perf_counter
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from apscheduler.schedulers.asyncio import AsyncIOScheduler
from apscheduler.triggers.cron import CronTrigger

from src.core import logger
from src.core.config import config
from src.tool import Notifier, TelegramRuntimeConfigBot, build_notifier
from src.web import Bilibili, Hanime1, Jandan, StellaSora, Telegram

log = logger.get('main')
_SHUTDOWN_TIMEOUT_SECONDS = 10.0


@dataclass(frozen=True, slots=True)
class ScheduledJob:
    key: str
    name: str
    cron: str
    enabled: bool
    run_on_start: bool
    required_commands: tuple[str, ...]
    factory: Callable[[Notifier], object]


type TriggerCallback = Callable[[str], Awaitable[str]]


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description='fav scheduler')
    parser.add_argument(
        '--trigger',
        metavar='TARGET',
        help='Run one job immediately and exit. Use a job key or "all".',
    )
    return parser.parse_args()


def _build_jobs() -> list[ScheduledJob]:
    bilibili_cfg = config.web.bilibili
    jandan_cfg = config.web.jandan
    telegram_cfg = config.web.telegram
    stellasora_cfg = config.web.stellasora
    hanime1_cfg = config.web.hanime1
    return [
        ScheduledJob(
            key='bilibili',
            name='Bilibili',
            cron=bilibili_cfg.cron,
            enabled=bilibili_cfg.enabled,
            run_on_start=bilibili_cfg.run_on_start,
            required_commands=('yt-dlp',),
            factory=lambda notifier: Bilibili(notifier=notifier),
        ),
        ScheduledJob(
            key='hanime1',
            name='Hanime1',
            cron=hanime1_cfg.cron,
            enabled=hanime1_cfg.enabled,
            run_on_start=hanime1_cfg.run_on_start,
            required_commands=('yt-dlp',),
            factory=lambda notifier: Hanime1(notifier=notifier),
        ),
        ScheduledJob(
            key='jandan',
            name='Jandan',
            cron=jandan_cfg.cron,
            enabled=jandan_cfg.enabled,
            run_on_start=jandan_cfg.run_on_start,
            required_commands=(),
            factory=lambda notifier: Jandan(notifier=notifier),
        ),
        ScheduledJob(
            key='telegram',
            name='Telegram',
            cron=telegram_cfg.cron,
            enabled=telegram_cfg.enabled,
            run_on_start=telegram_cfg.run_on_start,
            required_commands=(),
            factory=lambda notifier: Telegram(notifier=notifier),
        ),
        ScheduledJob(
            key='stellasora',
            name='StellaSora',
            cron=stellasora_cfg.cron,
            enabled=stellasora_cfg.enabled,
            run_on_start=stellasora_cfg.run_on_start,
            required_commands=(),
            factory=lambda notifier: StellaSora(notifier=notifier),
        ),
    ]


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


def _resolve_trigger_jobs(trigger_target: str, all_jobs: list[ScheduledJob]) -> list[ScheduledJob]:
    normalized = trigger_target.strip().lower()
    if not normalized:
        msg = 'Trigger target cannot be empty'
        raise SystemExit(msg)

    if normalized == 'all':
        selected_jobs = [job for job in all_jobs if job.enabled]
        if selected_jobs:
            return selected_jobs
        msg = 'No enabled jobs available for --trigger all'
        raise SystemExit(msg)

    job_by_key = {job.key: job for job in all_jobs}
    selected_job = job_by_key.get(normalized)
    if selected_job is not None:
        return [selected_job]

    valid_targets = ', '.join(['all', *sorted(job_by_key)])
    msg = f'Unknown --trigger target: {trigger_target!r}. Valid targets: {valid_targets}'
    raise SystemExit(msg)


def _resolve_scheduler_timezone() -> tzinfo:
    tz_name = os.getenv('TZ', 'UTC').strip() or 'UTC'
    try:
        return ZoneInfo(tz_name)
    except ZoneInfoNotFoundError:
        log.warning('Invalid TZ %r, fallback to UTC', tz_name)
        return UTC


def _build_runtime_config_bot(
    *,
    trigger_targets: list[tuple[str, str]],
    trigger_callback: TriggerCallback,
) -> TelegramRuntimeConfigBot:
    tg_cfg = config.telegram_bot
    return TelegramRuntimeConfigBot(
        token=tg_cfg.token,
        chat_id=tg_cfg.chat_id,
        run_config=config.run_config,
        api_base=tg_cfg.api_base,
        message_thread_id=tg_cfg.message_thread_id,
        trigger_targets=trigger_targets,
        trigger_callback=trigger_callback,
        proxy=config.proxy or None,
        hanime1_host=config.web.hanime1.host,
        hanime1_user_lang=config.web.hanime1.user_lang,
    )


def _build_trigger_controls(
    runners: list[tuple[ScheduledJob, Callable[[], object]]],
) -> tuple[list[tuple[str, str]], TriggerCallback]:
    enabled_runners = [(job, runner) for job, runner in runners if job.enabled]
    trigger_targets = [(job.key, job.name) for job, _runner in enabled_runners]
    runner_map = {job.key: runner for job, runner in enabled_runners}
    manual_tasks: set[asyncio.Task[None]] = set()

    def _spawn_manual_task(*, key: str, runner: Callable[[], object]) -> None:
        task = asyncio.create_task(runner(), name=f'manual-trigger-{key}')
        manual_tasks.add(task)
        task.add_done_callback(manual_tasks.discard)

    async def trigger_callback(target: str) -> str:
        if target == 'all':
            for key, runner in runner_map.items():
                _spawn_manual_task(key=key, runner=runner)
            return f'Triggered all jobs ({len(runner_map)}).'

        runner = runner_map.get(target)
        if runner is None:
            return f'Unknown job target: {target}'

        _spawn_manual_task(key=target, runner=runner)
        return f'Triggered {target}.'

    return trigger_targets, trigger_callback


async def _shutdown_runtime_config_bot(
    bot: TelegramRuntimeConfigBot | None,
    task: asyncio.Task[None] | None,
) -> None:
    if task is not None:
        task.cancel()
        try:
            await asyncio.wait_for(task, timeout=_SHUTDOWN_TIMEOUT_SECONDS)
        except TimeoutError:
            log.warning('Timed out while waiting runtime config bot task to stop')
        except asyncio.CancelledError:
            pass
        except Exception as exc:  # noqa: BLE001
            log.warning('Runtime config bot task stop failed: %s', exc)
    if bot is not None:
        try:
            await asyncio.wait_for(bot.aclose(), timeout=_SHUTDOWN_TIMEOUT_SECONDS)
        except TimeoutError:
            log.warning('Timed out while closing runtime config bot client')
        except Exception as exc:  # noqa: BLE001
            log.warning('Runtime config bot close failed: %s', exc)


async def _shutdown_notifier(notifier: Notifier) -> None:
    try:
        await asyncio.wait_for(notifier.aclose(), timeout=_SHUTDOWN_TIMEOUT_SECONDS)
    except TimeoutError:
        log.warning('Timed out while closing notifier client')
    except Exception as exc:  # noqa: BLE001
        log.warning('Notifier close failed: %s', exc)


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


async def _safe_notify(notifier: Notifier, message: str) -> None:
    try:
        await notifier.send(message)
    except Exception as exc:  # noqa: BLE001
        log.warning('Failed to send notification: %s', exc)


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


async def _run_job(*, job: ScheduledJob, notifier: Notifier) -> None:
    started_at = datetime.now(tz=UTC)
    started_perf = perf_counter()
    log.notice('Job started: %s', job.name)
    worker = job.factory(notifier)
    try:
        await worker.update()
    except Exception as exc:
        elapsed = perf_counter() - started_perf
        finished_at = datetime.now(tz=UTC)
        await _safe_notify(
            notifier,
            (
                f'fav job failed\n'
                f'Job: {job.name}\n'
                f'Started: {started_at.strftime("%Y-%m-%d %H:%M:%S %Z")}\n'
                f'Finished: {finished_at.strftime("%Y-%m-%d %H:%M:%S %Z")}\n'
                f'Elapsed: {elapsed:.1f}s\n'
                f'Error: {exc.__class__.__name__}: {exc}'
            ),
        )
        log.exception('Job failed: %s', job.name)
    else:
        elapsed = perf_counter() - started_perf
        log.notice('Job completed: %s (%.1fs)', job.name, elapsed)
    finally:
        await _safe_close(job.name, worker)


async def main(*, trigger_target: str | None = None) -> None:  # noqa: C901, PLR0912, PLR0915
    notifier = build_notifier()
    runtime_config_bot: TelegramRuntimeConfigBot | None = None
    runtime_config_task: asyncio.Task[None] | None = None
    stop_event = asyncio.Event()
    main_task = asyncio.current_task()
    if main_task is None:
        msg = 'Failed to resolve main task for signal handling'
        raise RuntimeError(msg)
    remove_signal_handlers = _install_signal_handlers(stop_event=stop_event, main_task=main_task)
    all_jobs = _build_jobs()
    jobs = [job for job in all_jobs if job.enabled]
    timezone = _resolve_scheduler_timezone()
    scheduler = AsyncIOScheduler(timezone=timezone)
    if trigger_target is None:
        _validate_commands(jobs)
        log.info('Scheduler timezone: %s', getattr(timezone, 'key', str(timezone)))

    runners: list[tuple[ScheduledJob, Callable[[], object]]] = []
    runner_by_key: dict[str, Callable[[], object]] = {}

    try:
        for job in all_jobs:
            lock = asyncio.Lock()

            async def runner(*, _job: ScheduledJob = job, _lock: asyncio.Lock = lock) -> None:
                if _lock.locked():
                    log.warning('%s is still running, skip this trigger', _job.name)
                    return
                async with _lock:
                    await _run_job(job=_job, notifier=notifier)

            runners.append((job, runner))
            runner_by_key[job.key] = runner

            if trigger_target is not None:
                continue

            if not job.enabled:
                log.info('%s is disabled; skipping schedule and /trigger target', job.name)
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
            trigger_jobs = _resolve_trigger_jobs(trigger_target, all_jobs)
            _validate_commands(trigger_jobs, respect_enabled=False)
            for job in trigger_jobs:
                if not job.enabled:
                    log.warning('%s is disabled in config; running once due to --trigger', job.name)
                await runner_by_key[job.key]()
            return

        if not jobs:
            log.warning('No jobs enabled. Waiting indefinitely.')

        trigger_targets, trigger_callback = _build_trigger_controls(runners)
        runtime_config_bot = _build_runtime_config_bot(
            trigger_targets=trigger_targets,
            trigger_callback=trigger_callback,
        )
        runtime_config_task = asyncio.create_task(runtime_config_bot.run(), name='telegram-runtime-config-bot')
        log.info('Telegram runtime config bot enabled')

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
        await _shutdown_runtime_config_bot(runtime_config_bot, runtime_config_task)
        if scheduler.running:
            scheduler.shutdown(wait=False)
        await _shutdown_notifier(notifier)


if __name__ == '__main__':
    args = _parse_args()
    asyncio.run(main(trigger_target=args.trigger))
