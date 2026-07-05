"""Provides functionality to interact with Bilibili API."""

import asyncio
import shutil
import subprocess
import tempfile
from collections.abc import Coroutine
from datetime import UTC, datetime
from http.cookiejar import MozillaCookieJar
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

import bilibili_api as api
from tenacity import RetryCallState, retry, retry_if_exception_type, stop_after_attempt, wait_exponential
from tqdm import tqdm

from src.core import config, logger
from src.tool import CookieCloudClient, database, ensure_unique_path, format_video_filename
from src.tool.notifications import enqueue_notification, format_job_failure_dedupe_key, resolve_notification

log = logger.get('bilibili')
cfg = config.web.bilibili
_KIBIBYTE = 1024
_BILIBILI_SCHEMA_LOCK_ID = database.advisory_lock_id('bilibili:schema')
_BILIBILI_CREATE_TABLE_SQL = """
CREATE TABLE IF NOT EXISTS bilibili (
    bvid TEXT PRIMARY KEY,
    fav_id BIGINT NOT NULL,
    title TEXT NOT NULL,
    upper TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
"""
_BILIBILI_BVID_ARBITER_SQL = """
SELECT EXISTS (
    SELECT 1
    FROM pg_index AS idx
    JOIN pg_attribute AS attr
      ON attr.attrelid = idx.indrelid
     AND attr.attnum = idx.indkey[0]
    WHERE idx.indrelid = 'bilibili'::regclass
      AND idx.indisunique
      AND idx.indimmediate
      AND idx.indisvalid
      AND idx.indisready
      AND idx.indnkeyatts = 1
      AND idx.indpred IS NULL
      AND idx.indexprs IS NULL
      AND attr.attname = 'bvid'
) AS has_bvid_arbiter;
"""
_BILIBILI_ADD_BVID_ARBITER_SQL = 'ALTER TABLE bilibili ADD CONSTRAINT bilibili_bvid_unique UNIQUE (bvid);'


class DownloadError(RuntimeError):
    """Raised when a download fails after retries."""

    def __init__(self, message: str, *, notification_dedupe_key: str = '') -> None:
        super().__init__(message)
        self.notification_dedupe_key = notification_dedupe_key


class Bilibili:
    """Class to interact with Bilibili API."""

    def __init__(self) -> None:
        """Initialize Bilibili instance with main and sub credentials."""
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
    def _format_file_size(size_bytes: int | None) -> str | None:
        if size_bytes is None or size_bytes < 0:
            return None
        if size_bytes < _KIBIBYTE:
            return f'{size_bytes} B'

        size = float(size_bytes)
        units = ('KB', 'MB', 'GB', 'TB')
        for unit in units:
            size /= _KIBIBYTE
            if size < _KIBIBYTE or unit == units[-1]:
                return f'{size:.1f} {unit}'
        return None

    @staticmethod
    def _download_failure_key(bvid: str) -> str:
        return f'bilibili:download:{bvid}'

    @classmethod
    def _download_failure_notification_key(cls, bvid: str) -> str:
        return format_job_failure_dedupe_key(job_key='bilibili', failure_key=cls._download_failure_key(bvid))

    @staticmethod
    def _to_positive_int(value: Any) -> int | None:
        if isinstance(value, bool):
            return None
        if isinstance(value, (int, float)):
            ivalue = int(value)
            return ivalue if ivalue > 0 else None
        if isinstance(value, str):
            stripped = value.strip()
            if stripped.isdigit():
                ivalue = int(stripped)
                return ivalue if ivalue > 0 else None
        return None

    @classmethod
    def _extract_resolution_label(cls, detail: dict[str, Any]) -> str | None:
        view = detail.get('View')
        if not isinstance(view, dict):
            return None

        dimension = view.get('dimension')
        if isinstance(dimension, dict):
            height = cls._to_positive_int(dimension.get('height'))
            if height is not None:
                return f'{height}p'

        pages = view.get('pages')
        if not isinstance(pages, list):
            return None
        page_heights: list[int] = []
        for page in pages:
            if not isinstance(page, dict):
                continue
            page_dimension = page.get('dimension')
            if not isinstance(page_dimension, dict):
                continue
            height = cls._to_positive_int(page_dimension.get('height'))
            if height is not None:
                page_heights.append(height)
        if not page_heights:
            return None
        return f'{max(page_heights)}p'

    @classmethod
    def _extract_release_date(cls, detail: dict[str, Any]) -> str | None:
        view = detail.get('View')
        if not isinstance(view, dict):
            return None

        for key in ('pubdate', 'ctime'):
            ts = cls._to_positive_int(view.get(key))
            if ts is None:
                continue
            return datetime.fromtimestamp(ts, tz=UTC).strftime('%Y-%m-%d')
        return None

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

    async def _notify_download(  # noqa: PLR0913
        self,
        *,
        bvid: str,
        title: str,
        upper: str,
        fav_id: int,
        resolution: str | None = None,
        file_size_bytes: int | None = None,
        release_date: str | None = None,
        cover_url: str | None = None,
    ) -> bool:
        source = 'toview' if fav_id == -1 else 'fav'
        url = f'https://www.bilibili.com/video/{bvid}'
        resolution_label = resolution or 'unknown'
        file_size_label = self._format_file_size(file_size_bytes) or 'unknown'
        release_date_label = (release_date or 'unknown').strip() or 'unknown'
        body = f'{upper} | {resolution_label} | {file_size_label} | {release_date_label}'

        try:
            await enqueue_notification(
                kind='download_completed',
                source='bilibili',
                title=f'Bilibili ({source}): {title}',
                body=body,
                link_url=url,
                image_url=cover_url or '',
                payload={
                    'bvid': bvid,
                    'fav_id': fav_id,
                    'upper': upper,
                    'resolution': resolution,
                    'file_size_bytes': file_size_bytes,
                    'release_date': release_date,
                },
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue bilibili download notification for %s: %s', bvid, exc)

        return await self._resolve_download_failure_notification(
            bvid=bvid,
            title=title,
            upper=upper,
            fav_id=fav_id,
            resolution=resolution,
            file_size_bytes=file_size_bytes,
            release_date=release_date,
            cover_url=cover_url,
        )

    async def _resolve_download_failure_notification(  # noqa: PLR0913
        self,
        *,
        bvid: str,
        title: str,
        upper: str,
        fav_id: int,
        resolution: str | None = None,
        file_size_bytes: int | None = None,
        release_date: str | None = None,
        cover_url: str | None = None,
    ) -> bool:
        source = 'toview' if fav_id == -1 else 'fav'
        url = f'https://www.bilibili.com/video/{bvid}'
        resolution_label = resolution or 'unknown'
        file_size_label = self._format_file_size(file_size_bytes) or 'unknown'
        release_date_label = (release_date or 'unknown').strip() or 'unknown'
        metadata = f'{upper} | {source} | {resolution_label} | {file_size_label} | {release_date_label}'
        body = f'Download succeeded: {title} [{bvid}]\n{metadata}'

        try:
            await resolve_notification(
                dedupe_key=self._download_failure_notification_key(bvid),
                kind='job_recovered',
                source='worker',
                title='Job recovered: Bilibili',
                body=body,
                link_url=url,
                image_url=cover_url or '',
                payload={
                    'job': 'bilibili',
                    'bvid': bvid,
                    'fav_id': fav_id,
                    'upper': upper,
                    'resolution': resolution,
                    'file_size_bytes': file_size_bytes,
                    'release_date': release_date,
                },
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to resolve bilibili download failure notification for %s: %s', bvid, exc)
            return False
        return True

    async def _resolve_existing_download_failure_notification(self, video: api.video.Video, *, fav_id: int) -> bool:
        bvid = video.get_bvid()
        title = bvid
        upper = 'unknown'
        resolution: str | None = None
        release_date: str | None = None
        cover_url: str | None = None

        try:
            detail = await video.get_detail()
        except Exception as exc:  # noqa: BLE001
            log.debug('Failed to read detail for existing bilibili video %s before recovery notification: %s', bvid, exc)
        else:
            title = str(detail.get('View', {}).get('title') or bvid)
            upper = str(detail.get('Card', {}).get('card', {}).get('name') or 'unknown')
            resolution = self._extract_resolution_label(detail)
            release_date = self._extract_release_date(detail)
            cover_url = await self.get_video_cover_url(video, detail=detail)

        return await self._resolve_download_failure_notification(
            bvid=bvid,
            title=title,
            upper=upper,
            fav_id=fav_id,
            resolution=resolution,
            release_date=release_date,
            cover_url=cover_url,
        )

    @staticmethod
    async def _has_bvid_arbiter(cursor: Any) -> bool:
        await cursor.execute(_BILIBILI_BVID_ARBITER_SQL)
        row = await cursor.fetchone()
        return bool(row and row['has_bvid_arbiter'])

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

    async def get_toviews(self) -> tuple[list[api.video.Video], bool, bool]:
        """Get videos in the 'watch later' list.

        Returns:
            A tuple of (videos_to_download, has_any_toviews, recovery_notifications_succeeded).
        """
        toview = await api.user.get_toview_list(credential=self.credential)
        if not toview['list']:
            return ([], False, True)
        exists_ids = await database.query_db('SELECT bvid FROM bilibili;')
        exists_ids = [i['bvid'] for i in exists_ids]
        result = [api.video.Video(bvid=v['bvid'], credential=self.credential) for v in toview['list']]
        log.info('Find %d toviews in total', len(result))
        recovery_notifications_succeeded = True
        for v in result.copy():
            if v.get_bvid() in exists_ids:
                recovery_notifications_succeeded = (
                    await self._resolve_existing_download_failure_notification(v, fav_id=-1) and recovery_notifications_succeeded
                )
                result.remove(v)
        log.info('Find %d toviews to download', len(result))
        return (result, True, recovery_notifications_succeeded)

    async def get_favs(self, fav_id: int) -> tuple[list[api.video.Video], bool]:
        """Get the videos in the favorite list."""
        exists_ids = await database.query_db('SELECT bvid FROM bilibili;')
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
        log.info('Find %d favs in total', len(result))
        recovery_notifications_succeeded = True
        for video in result.copy():
            if video.get_bvid() in exists_ids:
                recovery_notifications_succeeded = (
                    await self._resolve_existing_download_failure_notification(video, fav_id=fav_id) and recovery_notifications_succeeded
                )
                result.remove(video)
        log.info('Find %d favs to download', len(result))
        return (result, recovery_notifications_succeeded)

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
            raise DownloadError(msg, notification_dedupe_key=self._download_failure_key(bvid))

        _run_once()

    async def update_fav(self, fav_id: int, path: Path) -> None:
        await asyncio.to_thread(path.mkdir, parents=True, exist_ok=True)
        has_any_toviews = False
        recovery_notifications_succeeded = True
        # for toview
        if fav_id == -1:
            videos, has_any_toviews, recovery_notifications_succeeded = await self.get_toviews()
        else:
            videos, recovery_notifications_succeeded = await self.get_favs(fav_id)
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
                resolution = self._extract_resolution_label(detail)
                file_size_bytes: int | None = None
                for saved_path in saved_paths:
                    try:
                        size_bytes = saved_path.stat().st_size
                    except OSError as exc:
                        log.debug('Failed to read file size for %s: %s', saved_path, exc)
                        continue
                    if file_size_bytes is None or size_bytes > file_size_bytes:
                        file_size_bytes = size_bytes
                release_date = self._extract_release_date(detail)
                await database.query_db(
                    """
                    INSERT INTO bilibili (bvid, fav_id, title, upper)
                    VALUES (?, ?, ?, ?)
                    ON CONFLICT (bvid) DO UPDATE SET
                        fav_id = EXCLUDED.fav_id,
                        title = EXCLUDED.title,
                        upper = EXCLUDED.upper;
                    """,
                    (bvid, fav_id, title, upper),
                )
                recovery_notifications_succeeded = (
                    await self._notify_download(
                        bvid=bvid,
                        title=title,
                        upper=upper,
                        fav_id=fav_id,
                        resolution=resolution,
                        file_size_bytes=file_size_bytes,
                        release_date=release_date,
                        cover_url=cover_url,
                    )
                    and recovery_notifications_succeeded
                )
        else:
            log.info('No new videos')

        # Clear toview list only after the download pass completes successfully.
        if fav_id == -1 and has_any_toviews and recovery_notifications_succeeded:
            log.info('Clearing toview list ...')
            await api.user.clear_toview_list(credential=self.credential)
        elif fav_id == -1 and has_any_toviews:
            log.warning('Skip clearing toview list because a download recovery notification did not enqueue successfully')

    async def _ensure_table(self) -> None:
        async with database.transaction_cursor() as cursor:
            await cursor.execute('SELECT pg_advisory_xact_lock(%s);', (_BILIBILI_SCHEMA_LOCK_ID,))
            await cursor.execute(_BILIBILI_CREATE_TABLE_SQL)
            await cursor.execute('LOCK TABLE bilibili IN SHARE ROW EXCLUSIVE MODE;')

            if not await self._has_bvid_arbiter(cursor):
                await cursor.execute(_BILIBILI_ADD_BVID_ARBITER_SQL)
                log.info('Created bilibili bvid unique constraint')

        log.debug('bilibili table initialized')

    async def update(self) -> None:
        """Update the favorite list of the main account."""
        await self._ensure_table()

        await self.update_fav(cfg.fav_id, cfg.path / 'fav')
        await self.update_fav(-1, cfg.path / 'toview')
