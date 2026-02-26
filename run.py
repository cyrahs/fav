import asyncio
import inspect
import shutil
from collections.abc import Callable
from dataclasses import dataclass
from datetime import UTC, datetime
from time import perf_counter

from apscheduler.schedulers.asyncio import AsyncIOScheduler
from apscheduler.triggers.cron import CronTrigger

from src.core import logger
from src.core.config import config
from src.tool import Notifier, build_notifier
from src.web import Bilibili, StellaSora, Tangxin, Telegram

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


def _build_jobs() -> list[ScheduledJob]:
    tangxin_cfg = config.web.tangxin
    bilibili_cfg = config.web.bilibili
    telegram_cfg = config.web.telegram
    stellasora_cfg = config.web.stellasora
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


async def main() -> None:
    notifier = build_notifier()
    jobs = [job for job in _build_jobs() if job.enabled]
    scheduler = AsyncIOScheduler()
    _validate_commands(jobs)

    runners: list[tuple[ScheduledJob, Callable[[], object]]] = []

    try:
        for job in jobs:
            lock = asyncio.Lock()

            async def runner(*, _job: ScheduledJob = job, _lock: asyncio.Lock = lock) -> None:
                if _lock.locked():
                    log.warning('%s is still running, skip this trigger', _job.name)
                    return
                async with _lock:
                    await _run_job(job=_job, notifier=notifier)

            try:
                trigger = CronTrigger.from_crontab(job.cron)
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
            runners.append((job, runner))
            log.info('Scheduled %s with cron %r', job.name, job.cron)

        if not runners:
            log.warning('No jobs enabled. Waiting indefinitely.')

        scheduler.start()
        for job, runner in runners:
            if job.run_on_start:
                log.info('Run-on-start enabled for %s', job.name)
                await runner()

        await asyncio.Event().wait()
    except (KeyboardInterrupt, asyncio.CancelledError):
        log.info('Shutdown signal received')
    finally:
        if scheduler.running:
            scheduler.shutdown(wait=False)
        await notifier.aclose()


if __name__ == '__main__':
    asyncio.run(main())
