import asyncio
import contextlib
import inspect
import os
import shutil
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
from src.web import Bilibili, Hanime1, StellaSora, Tangxin, Telegram

log = logger.get('main')


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


def _build_jobs() -> list[ScheduledJob]:
    tangxin_cfg = config.web.tangxin
    bilibili_cfg = config.web.bilibili
    telegram_cfg = config.web.telegram
    stellasora_cfg = config.web.stellasora
    hanime1_cfg = config.web.hanime1
    return [
        ScheduledJob(
            key='tangxin',
            name='Tangxin',
            cron=tangxin_cfg.cron,
            enabled=tangxin_cfg.enabled,
            run_on_start=tangxin_cfg.run_on_start,
            required_commands=('ffmpeg',),
            factory=lambda notifier: Tangxin(notifier=notifier),
        ),
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


def _validate_commands(jobs: list[ScheduledJob]) -> None:
    missing: list[tuple[str, str]] = []
    for job in jobs:
        if not job.enabled:
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
    )


def _build_trigger_controls(
    runners: list[tuple[ScheduledJob, Callable[[], object]]],
) -> tuple[list[tuple[str, str]], TriggerCallback]:
    trigger_targets = [(job.key, job.name) for job, _runner in runners]
    runner_map = {job.key: runner for job, runner in runners}
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
        with contextlib.suppress(asyncio.CancelledError):
            await task
    if bot is not None:
        await bot.aclose()


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


async def main() -> None:  # noqa: C901
    notifier = build_notifier()
    runtime_config_bot: TelegramRuntimeConfigBot | None = None
    runtime_config_task: asyncio.Task[None] | None = None
    all_jobs = _build_jobs()
    jobs = [job for job in all_jobs if job.enabled]
    timezone = _resolve_scheduler_timezone()
    scheduler = AsyncIOScheduler(timezone=timezone)
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

            if not job.enabled:
                log.info('%s is disabled in schedule, manual trigger only', job.name)
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

        await asyncio.Event().wait()
    except (KeyboardInterrupt, asyncio.CancelledError):
        log.info('Shutdown signal received')
    finally:
        await _shutdown_runtime_config_bot(runtime_config_bot, runtime_config_task)
        if scheduler.running:
            scheduler.shutdown(wait=False)
        await notifier.aclose()


if __name__ == '__main__':
    asyncio.run(main())
