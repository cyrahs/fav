#!/usr/bin/env python3
"""Manual script to download Bilibili videos using CLI."""

import argparse
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import httpx

from src.core import config, logger
from src.tool import CookieCloudClient

log = logger.get('script.bilibili')


def main() -> None:
    """Main entry point for the CLI script."""
    parser = argparse.ArgumentParser(description='Download Bilibili videos using yt-dlp')
    parser.add_argument('bv', help='Bilibili video ID (BV number)')
    parser.add_argument('--playlist-items', help='Specify which items to download (passed to yt-dlp)')
    parser.add_argument('-o', '--output', type=Path, default=Path.cwd(), help='Output directory (default: current directory)')
    parser.add_argument('-N', help='concurrency', type=int, default=1)

    args = parser.parse_args()

    # Verify yt-dlp is available
    if not shutil.which('yt-dlp'):
        log.error('yt-dlp command not found in PATH. Please install yt-dlp.')
        sys.exit(1)

    # Normalize BV format (add BV prefix if not present)
    bv = args.bv
    if not bv.startswith('BV'):
        bv = f'BV{bv}'

    # Create URL
    url = f'https://www.bilibili.com/video/{bv}'

    # Create temporary directory for cookie file
    with tempfile.TemporaryDirectory(prefix='fav-bilibili-') as tmp_dir:
        cookie_path = Path(tmp_dir) / 'bilibili.txt'

        # Get cookies from CookieCloud
        try:
            cc_cfg = config.cookiecloud
            client = CookieCloudClient(cc_cfg.server_url, cc_cfg.uuid, cc_cfg.password, proxy=config.proxy or None)
            client.save_to_netscape_format('bilibili.com', cookie_path)
        except (ConnectionError, httpx.HTTPError, KeyError, OSError, ValueError):
            log.exception('Error fetching cookies from CookieCloud')
            sys.exit(1)

        # Ensure output directory exists
        args.output.mkdir(parents=True, exist_ok=True)

        # Build yt-dlp command
        command = [
            'yt-dlp',
            '-o',
            str(args.output / f'{bv}.%(ext)s'),
            '--no-mtime',
            '--cookies',
            str(cookie_path),
            '-N',
            str(args.N),
            '--retries',
            '15',
            '--fragment-retries',
            '15',
            '--socket-timeout',
            '30',
        ]

        # Add playlist-items flag if provided
        if args.playlist_items:
            command.extend(['--playlist-items', args.playlist_items])

        # Add proxy if configured
        if config.proxy:
            command.extend(['--proxy', config.proxy])

        # Add URL
        command.append(url)

        # Run yt-dlp
        log.info('Downloading %s to %s...', bv, args.output)
        result = subprocess.run(command, text=True, check=False)  # noqa: S603

        if result.returncode != 0:
            log.error('yt-dlp exited with code %d', result.returncode)
            sys.exit(result.returncode)

        log.notice('Successfully downloaded %s to %s', bv, args.output)


if __name__ == '__main__':
    main()
