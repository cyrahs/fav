import asyncio
import shutil
import subprocess
from pathlib import Path

import httpx

from src.core import config, logger
from src.web import Bilibili, Tangxin

log = logger.get('main')

# verify ffmpeg is available
if not shutil.which('ffmpeg'):
    log.error('ffmpeg command not found in PATH. Please install ffmpeg.')
    raise SystemExit(1)

# update yt-dlp
PROXY = config.proxy if config.proxy else None
PROXY_ENV = {'http_proxy': PROXY, 'https_proxy': PROXY} if PROXY else None

ytdlp_path = Path('./bin/yt-dlp')

if not ytdlp_path.exists():
    ytdlp_path.parent.mkdir(exist_ok=True)

    log.info('yt-dlp not found. Downloading...')
    # Download yt-dlp
    subprocess.run(  # noqa: S603
        [  # noqa: S607
            'curl',
            '-L',
            'https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux',
            '-o',
            str(ytdlp_path),
        ],
        check=True,
        env=PROXY_ENV,
    )
    # Make it executable
    subprocess.run(['chmod', '+x', str(ytdlp_path)], check=True)  # noqa: S603, S607
    log.info('yt-dlp downloaded successfully.')

response = httpx.head('https://github.com/yt-dlp/yt-dlp/releases/latest', follow_redirects=False, proxy=PROXY, timeout=10)
latest_tag = response.headers['location'].split('/')[-1]
latest_tag = f'stable@{latest_tag}'
subprocess.run([str(ytdlp_path), '--update-to', latest_tag], check=True, env=PROXY_ENV)  # noqa: S603


async def main() -> None:
    await Tangxin().update()
    await Bilibili().update()


if __name__ == '__main__':
    asyncio.run(main())
