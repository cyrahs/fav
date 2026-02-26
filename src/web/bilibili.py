"""Provides functionality to interact with Bilibili API."""

import asyncio
import shutil
import subprocess
import tempfile
from collections.abc import Coroutine
from http.cookiejar import MozillaCookieJar
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

import bilibili_api as api
from tenacity import RetryCallState, retry, retry_if_exception_type, stop_after_attempt, wait_exponential
from tqdm import tqdm

from src.core import config, logger
from src.tool import CookieCloudClient, Notifier, cloudflare, ensure_unique_path, format_video_filename

log = logger.get('bilibili')
cfg = config.web.bilibili


class DownloadError(RuntimeError):
    """Raised when a download fails after retries."""


class Bilibili:
    """Class to interact with Bilibili API."""

    def __init__(self, notifier: Notifier | None = None) -> None:
        """Initialize Bilibili instance with main and sub credentials."""
        self.notifier = notifier
        self._tmp_dir = tempfile.TemporaryDirectory(prefix='fav-bilibili-')
        self.cache_dir = Path(self._tmp_dir.name)
        self.cookie_path = self.cache_dir / 'bilibili.txt'
        self.update_cookie_from_cookiecloud(self.cookie_path)
        self.credential = self.create_credential(self.cookie_path)
        self.user = api.user.User(uid=cfg.id, credential=self.credential)
        self.info_cache = {}
        log.debug('cache_dir: %s', self.cache_dir)

    def __del__(self) -> None:
        self._tmp_dir.cleanup()

    async def aclose(self) -> None:
        self._tmp_dir.cleanup()

    @staticmethod
    def _escape_markdown(text: str) -> str:
        escaped = text
        for ch in ('\\', '_', '*', '`', '[', ']'):
            escaped = escaped.replace(ch, f'\\{ch}')
        return escaped

    @staticmethod
    def _normalize_cover_url(url: str) -> str:
        if url.startswith('//'):
            return f'https:{url}'
        return url

    @staticmethod
    def _is_placeholder_cover_url(url: str) -> bool:
        path = urlsplit(url).path.lower()
        return path.endswith(('/transparent.png', '/transparent.gif')) or '/archive/transparent' in path

    async def get_video_cover_url(self, video: api.video.Video, detail: dict[str, Any] | None = None) -> str | None:
        """Get a video's cover URL from Bilibili API responses."""
        candidate: Any = None

        if detail is not None:
            view = detail.get('View')
            if isinstance(view, dict):
                candidate = view.get('pic')
            if not candidate:
                candidate = detail.get('pic')

        if isinstance(candidate, str) and candidate:
            normalized = self._normalize_cover_url(candidate)
            if not self._is_placeholder_cover_url(normalized):
                return normalized

        info = self.info_cache.get(video)
        if info is None:
            try:
                info = await video.get_info()
                self.info_cache[video] = info
            except Exception as exc:  # noqa: BLE001
                log.debug('Failed to fetch cover for %s: %s', video.get_bvid(), exc)
                return None

        candidate = info.get('pic')
        if isinstance(candidate, str) and candidate:
            normalized = self._normalize_cover_url(candidate)
            if not self._is_placeholder_cover_url(normalized):
                return normalized

        return None

    async def _notify_download(self, *, bvid: str, title: str, upper: str, fav_id: int, cover_url: str | None = None) -> None:
        notifier = getattr(self, 'notifier', None)
        if notifier is None:
            return

        source = 'toview' if fav_id == -1 else 'fav'
        url = f'https://www.bilibili.com/video/{bvid}'
        safe_title = self._escape_markdown(title)
        safe_upper = self._escape_markdown(upper)
        message = f'Bilibili ({source})\n*{safe_title}*\n{safe_upper}\n[视频链接]({url})'
        send_markdown = getattr(notifier, 'send_markdown', None)
        send_photo = getattr(notifier, 'send_photo', None)

        try:
            if callable(send_photo) and cover_url:
                await send_photo(photo=cover_url, caption=message, parse_mode='Markdown')
                return
            if callable(send_markdown):
                await send_markdown(message, disable_web_page_preview=True)
                return
            await notifier.send(f'Bilibili ({source})\n{title}\n{upper}\n视频链接: {url}')
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to send bilibili download notification for %s: %s', bvid, exc)

    def update_cookie_from_cookiecloud(self, save_path: Path) -> None:
        """Update cookie from cookiecloud."""
        cc_cfg = config.cookiecloud
        client = CookieCloudClient(cc_cfg.server_url, cc_cfg.uuid, cc_cfg.password, proxy=config.proxy or None)
        client.save_to_netscape_format('bilibili.com', save_path)

    def create_credential(self, cookie_path: Path) -> api.Credential:
        """Create credential from cookie file."""
        cookie_jar = MozillaCookieJar(cookie_path)
        cookie_jar.load()
        cookies = [cookie.__dict__ for cookie in cookie_jar]
        cookies = {cookies['name'].lower(): cookies['value'] for cookies in cookies}
        needed_cookies = ['sessdata', 'bili_jct', 'buvid3', 'dedeuserid']
        cookies = {k: cookies[k] for k in needed_cookies}
        if len(cookies) != len(needed_cookies):
            log.warning('Some cookies are missing: %s', cookies.keys())
        return api.Credential(**cookies)

    async def check_valid(self, v: api.video.Video) -> bool:
        """Check if the video is valid."""
        if v in self.info_cache:
            return True
        try:
            info = await v.get_info()
            self.info_cache[v] = info
        except Exception as e:  # noqa: BLE001
            log.warning('Video %s is invalid: %s', v.get_bvid(), e)
            return False
        # Check if the video is a paid video
        if info['is_upower_exclusive']:
            log.warning('Video %s is a paid video', v.get_bvid())
            return False
        return True

    async def limit_gather(self, *coros: Coroutine, limit: int = 5) -> list[Any]:
        """Limit the number of coroutines to run concurrently."""
        results = []
        while coros:
            results.extend(await asyncio.gather(*coros[:limit]))
            coros = coros[limit:]
            await asyncio.sleep(1)
        return results

    async def get_toviews(self) -> tuple[list[api.video.Video], bool]:
        """Get videos in the 'watch later' list.

        Returns:
            A tuple of (videos_to_download, has_any_toviews).
        """
        toview = await api.user.get_toview_list(credential=self.credential)
        if not toview['list']:
            return ([], False)
        exists_ids = await cloudflare.query_d1('SELECT bvid FROM bilibili WHERE fav_id = -1;')
        exists_ids = [i['bvid'] for i in exists_ids]
        result = [api.video.Video(bvid=v['bvid'], credential=self.credential) for v in toview['list']]
        log.info('Find %d toviews in total', len(result))
        for v in result.copy():
            if v.get_bvid() in exists_ids:
                result.remove(v)
        log.info('Find %d toviews to download', len(result))
        return (result, True)

    async def get_favs(self, fav_id: int) -> list[api.video.Video]:
        """Get the videos in the favorite list."""
        exists_ids = await cloudflare.query_d1('SELECT bvid FROM bilibili WHERE fav_id = ?;', (str(fav_id),))
        exists_ids = [i['bvid'] for i in exists_ids]
        favlist = api.favorite_list.FavoriteList(media_id=fav_id, credential=self.credential)
        page = 1
        has_more = True
        result = []
        while has_more:
            res = await favlist.get_content(page=page)
            has_more = res['has_more']
            page += 1
            result += [api.video.Video(bvid=media['bvid'], credential=self.credential) for media in res['medias']]
            # stop if the last video is already in the database
            if result[-1].get_bvid() in exists_ids:
                break
        log.info('Find %d favs in total', len(result))
        for video in result.copy():
            if video.get_bvid() in exists_ids:
                result.remove(video)
        log.info('Find %d favs to download', len(result))
        return result

    def _cleanup_dir(self, dirpath: Path) -> None:
        """Clear out temporary download directory."""
        if not dirpath.exists():
            return
        for entry in dirpath.iterdir():
            if entry.is_dir():
                shutil.rmtree(entry)
            else:
                entry.unlink()

    def download(self, url: str, bvid: str, dirpath: Path, max_attempts: int = 3, base_delay: int = 5) -> None:
        """Download a video from Bilibili with retries."""
        # Use simple filename template with just the video ID, we'll rename it properly later
        command = [
            'yt-dlp',
            '-o',
            str(dirpath / f'{bvid}.%(ext)s'),
            '--no-mtime',
            '--cookies',
            str(self.cookie_path),
            '-N',
            '8',
            '--retries',
            '15',
            '--fragment-retries',
            '15',
            '--socket-timeout',
            '30',
            url,
        ]
        if config.proxy:
            command.extend(['--proxy', config.proxy])

        def _on_retry_before_sleep(retry_state: RetryCallState) -> None:
            # Keep retry logs at debug level to avoid alert noise from transient failures.
            exc = retry_state.outcome.exception() if retry_state.outcome else None
            if exc is None:
                return
            sleep = retry_state.next_action.sleep if retry_state.next_action else 0.0
            log.debug(
                'Retrying download for %s (%d/%d), next wait %.1fs: %s',
                bvid,
                retry_state.attempt_number,
                max_attempts,
                sleep,
                exc,
            )

        @retry(
            reraise=True,
            stop=stop_after_attempt(max_attempts),
            wait=wait_exponential(multiplier=base_delay, min=base_delay, max=base_delay * 6),
            retry=retry_if_exception_type(DownloadError),
            before_sleep=_on_retry_before_sleep,
        )
        def _run_once() -> None:
            self._cleanup_dir(dirpath)
            result = subprocess.run(command, text=True, capture_output=True, check=False)  # noqa: S603
            if result.returncode == 0:
                if result.stderr:
                    log.debug('yt-dlp stderr: %s', result.stderr.strip())
                return
            message = result.stderr.strip() or result.stdout.strip() or f'yt-dlp exited with code {result.returncode}'
            msg = f'{url}: {message}'
            raise DownloadError(msg)

        _run_once()

    async def update_fav(self, fav_id: int, path: Path) -> None:
        await asyncio.to_thread(path.mkdir, parents=True, exist_ok=True)
        has_any_toviews = False
        # for toview
        if fav_id == -1:
            videos, has_any_toviews = await self.get_toviews()
        else:
            videos = await self.get_favs(fav_id)
        if videos:
            valid = await asyncio.gather(*[self.check_valid(v) for v in videos])
            videos = [v for v, vld in zip(videos, valid, strict=True) if vld]
            for video in tqdm(videos[::-1], desc='Scanning bilibili'):
                bvid = video.get_bvid()
                detail = await video.get_detail()
                title = detail['View']['title']
                upper = detail['Card']['card']['name']
                cover_url = await self.get_video_cover_url(video, detail=detail)
                url = f'https://www.bilibili.com/video/{bvid}'
                video_cache_dir = self.cache_dir / 'videos'
                await asyncio.to_thread(video_cache_dir.mkdir, exist_ok=True)
                log.info('Downloading [%s]%s [%s]', upper, title, bvid)
                self.download(url, bvid, video_cache_dir)
                saved_paths = []
                for v in sorted(video_cache_dir.iterdir()):
                    # Format the proper filename with sanitized title and uploader
                    proper_filename = format_video_filename(
                        title=title,
                        video_id=bvid,
                        uploader=upper,
                        ext=v.suffix,
                    )
                    dst_path = path / proper_filename
                    dst_path = ensure_unique_path(dst_path)
                    shutil.move(v, dst_path)
                    saved_paths.append(dst_path)
                await cloudflare.query_d1(
                    'INSERT INTO bilibili (bvid, fav_id, title, upper) VALUES (?, ?, ?, ?);',
                    (bvid, str(fav_id), title, upper),
                )
                await self._notify_download(
                    bvid=bvid,
                    title=title,
                    upper=upper,
                    fav_id=fav_id,
                    cover_url=cover_url,
                )
        else:
            log.info('No new videos')

        # Clear toview list only after the download pass completes successfully.
        if fav_id == -1 and has_any_toviews:
            log.info('Clearing toview list ...')
            await api.user.clear_toview_list(credential=self.credential)

    async def update(self) -> None:
        """Update the favorite list of the main account."""
        # Initialize table
        await cloudflare.query_d1("""
            CREATE TABLE IF NOT EXISTS bilibili (
                bvid TEXT PRIMARY KEY,
                fav_id INTEGER NOT NULL,
                title TEXT NOT NULL,
                upper TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
        """)
        log.debug('bilibili table initialized')

        await self.update_fav(cfg.fav_id, cfg.path / 'fav')
        await self.update_fav(-1, cfg.path / 'toview')
