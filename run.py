import asyncio
import shutil

from src.core import logger
from src.web import Bilibili, Tangxin, Telegram

log = logger.get('main')

# verify ffmpeg is available
if not shutil.which('ffmpeg'):
    log.error('ffmpeg command not found in PATH. Please install ffmpeg.')
    raise SystemExit(1)

# verify yt-dlp is available
if not shutil.which('yt-dlp'):
    log.error('yt-dlp command not found in PATH. Please install yt-dlp.')
    raise SystemExit(1)


async def main() -> None:
    await Tangxin().update()
    await Bilibili().update()
    await Telegram().update()


if __name__ == '__main__':
    asyncio.run(main())
