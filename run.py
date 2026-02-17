import asyncio
import shutil
from datetime import UTC, datetime
from time import perf_counter

from src.core import logger
from src.tool import Notifier, build_notifier
from src.web import Bilibili, StellaSora, Tangxin, Telegram

log = logger.get('main')

# verify ffmpeg is available
if not shutil.which('ffmpeg'):
    log.error('ffmpeg command not found in PATH. Please install ffmpeg.')
    raise SystemExit(1)

# verify yt-dlp is available
if not shutil.which('yt-dlp'):
    log.error('yt-dlp command not found in PATH. Please install yt-dlp.')
    raise SystemExit(1)


async def _safe_notify(notifier: Notifier, message: str) -> None:
    try:
        await notifier.send(message)
    except Exception as exc:  # noqa: BLE001
        log.warning('Failed to send notification: %s', exc)


async def main() -> None:
    notifier = build_notifier()
    started_at = datetime.now(tz=UTC)
    results: list[str] = []

    jobs = [
        ('Tangxin', Tangxin().update),
        ('Bilibili', Bilibili().update),
        ('Telegram', Telegram().update),
        ('StellaSora', StellaSora().update),
    ]

    try:
        for name, update in jobs:
            begin = perf_counter()
            await update()
            duration = perf_counter() - begin
            results.append(f'- {name}: success ({duration:.1f}s)')
    except Exception as exc:
        finished_at = datetime.now(tz=UTC)
        elapsed = (finished_at - started_at).total_seconds()
        await _safe_notify(
            notifier,
            (
                f'fav job failed at {finished_at.strftime("%Y-%m-%d %H:%M:%S %Z")}\n'
                f'Elapsed: {elapsed:.1f}s\n'
                f'Error: {exc.__class__.__name__}: {exc}'
            ),
        )
        raise
    else:
        finished_at = datetime.now(tz=UTC)
        elapsed = (finished_at - started_at).total_seconds()
        summary = '\n'.join(results) if results else '- no jobs executed'
        await _safe_notify(
            notifier,
            (f'fav job completed at {finished_at.strftime("%Y-%m-%d %H:%M:%S %Z")}\nElapsed: {elapsed:.1f}s\n{summary}'),
        )
    finally:
        await notifier.aclose()


if __name__ == '__main__':
    asyncio.run(main())
