"""Public pixiv bookmarks, crawled through the www.pixiv.net ajax API.

pixiv's web API refuses the bookmarks listing without a login session, so the
crawl authenticates with the PHPSESSID cookie pulled from CookieCloud — the
same session the browser holds, tracked instead of going stale. The logged-in
user's id is embedded in that cookie's value (``{user_id}_{hash}``), so no
account id has to be configured. The image originals on i.pximg.net are gated
on the Referer header alone and need no cookie.

Three work types are archived: illustrations and manga as their original
files, one row per page, and ugoira (animated works) synthesized from their
frame zip into a single animated webp with Pillow.

Incrementality is jandan-shaped: every output file is a row whose
``downloaded``/``unavailable`` flags say whether it still needs work, and a
run stops once it has seen ``_FULL_HIT_STOP_PAGES`` consecutive bookmark pages
whose every work is already settled. A run that dies mid-way leaves pending
rows behind, and the next run picks them up straight from the database.
"""

from __future__ import annotations

import asyncio
import shutil
import tempfile
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

import httpx
from PIL import Image

from src.core import logger, settings
from src.tool import CookieCloudClient, database, format_media_filename, sanitize
from src.tool.cookiecloud import PIXIV_PROFILE
from src.tool.notifications import enqueue_notification

log = logger.get('pixiv')

_BOOKMARKS_URL = 'https://www.pixiv.net/ajax/user/{user_id}/illusts/bookmarks'
_PAGES_URL = 'https://www.pixiv.net/ajax/illust/{illust_id}/pages'
_UGOIRA_META_URL = 'https://www.pixiv.net/ajax/illust/{illust_id}/ugoira_meta'
_REFERER = 'https://www.pixiv.net/'
_USER_AGENT = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36'
_PAGE_LIMIT = 48
_FULL_HIT_STOP_PAGES = 2
_API_RETRY_DELAYS_SECONDS = (2.0, 5.0, 15.0, 30.0)
_API_RETRYABLE_STATUS_CODES = {429, 502, 503, 504}
# What the bookmarks endpoint answers when the session is missing or expired.
_AUTH_STATUS_CODES = {400, 401, 403}
_ILLUST_TYPE_UGOIRA = 2
# 0 = illustration, 1 = manga, 2 = ugoira. Novels never appear on this endpoint,
# but the filter is kept defensive in case pixiv adds a type.
_ILLUST_TYPES = {0, 1, _ILLUST_TYPE_UGOIRA}
_MASKED_REASON = 'masked in bookmarks'
_ERROR_REASON_MAX_CHARS = 500
_EXT_FALLBACK = 'jpg'
# Pillow's animated-webp knobs: lossy q90 is visually clean on drawn frames,
# method 4 keeps encode time sane on long ugoira.
_UGOIRA_WEBP_QUALITY = 90
_UGOIRA_WEBP_METHOD = 4


class PixivError(RuntimeError):
    def __init__(self, message: str, *, notification_dedupe_key: str = '') -> None:
        super().__init__(message)
        self.notification_dedupe_key = notification_dedupe_key


class PixivApiError(RuntimeError):
    """The ajax envelope said ``error: true``.

    On the bookmarks endpoint this is a rejected session; on a work endpoint it
    usually means the work was deleted or made private after being bookmarked.
    """

    def __init__(self, message: str, *, status_code: int) -> None:
        super().__init__(message)
        self.status_code = status_code


@dataclass(frozen=True, slots=True)
class PixivWork:
    illust_id: int
    title: str
    author: str
    author_id: str
    illust_type: int
    page_count: int
    masked: bool


def _to_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    return None


def derive_user_id(phpsessid: str) -> str:
    """``'12345678_AbCdEf...'`` -> ``'12345678'``; empty when the prefix is not digits."""
    prefix, separator, _ = phpsessid.partition('_')
    if separator and prefix.isdigit():
        return prefix
    return ''


def extract_phpsessid(cookies: dict[str, list[dict]]) -> str:
    """Find the pixiv session cookie in a CookieCloud vault.

    The vault keys cookies by the hostname the browser held them under, so both
    profile domains are accepted, with or without a leading dot, and the cookie
    name is matched case-insensitively — mirroring ``cookiecloud.probe``.
    """
    for domain, domain_cookies in cookies.items():
        if domain.lstrip('.') not in PIXIV_PROFILE.domains:
            continue
        for cookie in domain_cookies:
            if not isinstance(cookie, dict) or str(cookie.get('name') or '').lower() != 'phpsessid':
                continue
            value = str(cookie.get('value') or '')
            if value:
                return value
    return ''


def parse_bookmark_works(body: dict[str, Any]) -> list[PixivWork]:
    """One page of ``body.works[]`` -> ``PixivWork`` entries.

    A masked entry is a bookmark whose work was deleted or made private; pixiv
    still lists it, with the title replaced by a placeholder and — unlike a live
    work — the id as an int rather than a string. Masked entries are kept so the
    crawl can settle them as unavailable.
    """
    raw_works = body.get('works')
    if not isinstance(raw_works, list):
        return []
    works: list[PixivWork] = []
    for raw in raw_works:
        if not isinstance(raw, dict):
            continue
        illust_id = _to_int(raw.get('id'))
        if illust_id is None:
            continue
        masked = bool(raw.get('isMasked'))
        illust_type = _to_int(raw.get('illustType'))
        if not masked and illust_type not in _ILLUST_TYPES:
            continue
        works.append(
            PixivWork(
                illust_id=illust_id,
                title=str(raw.get('title') or ''),
                author=str(raw.get('userName') or ''),
                author_id=str(raw.get('userId') or ''),
                illust_type=illust_type if illust_type is not None else 0,
                page_count=_to_int(raw.get('pageCount')) or 1,
                masked=masked,
            ),
        )
    return works


def parse_page_urls(body: Any) -> list[str]:
    """``/ajax/illust/{id}/pages`` body -> the original URL of every page, in order."""
    if not isinstance(body, list):
        return []
    urls: list[str] = []
    for entry in body:
        if not isinstance(entry, dict):
            continue
        entry_urls = entry.get('urls')
        original = entry_urls.get('original') if isinstance(entry_urls, dict) else None
        if isinstance(original, str) and original:
            urls.append(original)
    return urls


def parse_ugoira_meta(body: dict[str, Any]) -> tuple[str, list[tuple[str, int]]]:
    """``/ajax/illust/{id}/ugoira_meta`` body -> (frame zip URL, [(frame file, delay ms)])."""
    zip_url = str(body.get('originalSrc') or '')
    raw_frames = body.get('frames')
    frames: list[tuple[str, int]] = []
    if isinstance(raw_frames, list):
        for raw in raw_frames:
            if not isinstance(raw, dict):
                continue
            file = str(raw.get('file') or '')
            delay = _to_int(raw.get('delay'))
            if file and delay is not None and delay > 0:
                frames.append((file, delay))
    if not zip_url or not frames:
        msg = 'ugoira_meta payload is missing the frame zip or the frame list'
        raise ValueError(msg)
    return zip_url, frames


def infer_extension(url: str) -> str:
    suffix = Path(urlsplit(url).path).suffix.removeprefix('.').lower()
    return suffix or _EXT_FALLBACK


def synthesize_ugoira_webp(frame_paths: list[Path], delays_ms: list[int], dst_path: Path) -> None:
    """Encode ugoira frames into one animated webp, honoring per-frame delays.

    CPU-bound and synchronous — run it in a thread. Pillow deduplicates
    identical consecutive frames by extending their duration, so the encoded
    frame count may come out slightly below the source's.
    """
    with Image.open(frame_paths[0]) as first:
        first.save(
            dst_path,
            save_all=True,
            append_images=(Image.open(path) for path in frame_paths[1:]),
            duration=delays_ms,
            loop=0,
            quality=_UGOIRA_WEBP_QUALITY,
            method=_UGOIRA_WEBP_METHOD,
        )


class Pixiv:
    def __init__(self) -> None:
        self.cfg = settings.load().web.pixiv
        proxy = self.cfg.proxy or None
        self.client = httpx.AsyncClient(
            headers={'User-Agent': _USER_AGENT, 'Referer': _REFERER, 'Accept': 'application/json'},
            proxy=proxy,
            timeout=60,
            follow_redirects=True,
            limits=httpx.Limits(max_keepalive_connections=10, max_connections=10),
        )
        # Originals on i.pximg.net are gated on the Referer alone; no cookie.
        self.media_client = httpx.AsyncClient(
            headers={'User-Agent': _USER_AGENT, 'Referer': _REFERER},
            proxy=proxy,
            timeout=120,
            follow_redirects=True,
            limits=httpx.Limits(max_keepalive_connections=10, max_connections=10),
        )

    async def aclose(self) -> None:
        await self.client.aclose()
        await self.media_client.aclose()

    async def _pace(self) -> None:
        if self.cfg.sleep_request_seconds > 0:
            await asyncio.sleep(self.cfg.sleep_request_seconds)

    async def _ensure_table(self) -> None:
        # Keep the DDL in the order tables -> ALTERs -> indexes (see AGENTS.md);
        # a column added later arrives through an ALTER TABLE ADD COLUMN block
        # between the CREATE TABLE and any CREATE INDEX.
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS pixiv (
                illust_id BIGINT NOT NULL,
                num INTEGER NOT NULL,
                illust_type INTEGER NOT NULL DEFAULT 0,
                title TEXT NOT NULL DEFAULT '',
                author TEXT NOT NULL DEFAULT '',
                author_id TEXT NOT NULL DEFAULT '',
                page_count INTEGER NOT NULL DEFAULT 1,
                source_url TEXT NOT NULL DEFAULT '',
                local_path TEXT NOT NULL DEFAULT '',
                downloaded INTEGER NOT NULL DEFAULT 0,
                unavailable INTEGER NOT NULL DEFAULT 0,
                failed_count INTEGER NOT NULL DEFAULT 0,
                last_error TEXT NOT NULL DEFAULT '',
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (illust_id, num)
            );
        """)

    def _fetch_phpsessid(self) -> str:
        """Refresh the pixiv session from CookieCloud, so it tracks the browser."""
        client = CookieCloudClient(
            self.cfg.cookiecloud.server_url,
            self.cfg.cookiecloud.uuid,
            self.cfg.cookiecloud.password,
        )
        try:
            cookies = client.get_cookies()
        finally:
            client.client.close()
        phpsessid = extract_phpsessid(cookies)
        if not phpsessid:
            msg = 'CookieCloud has no PHPSESSID for pixiv.net. Sign in to pixiv in the browser and re-sync the extension.'
            raise PixivError(msg, notification_dedupe_key='pixiv:auth')
        return phpsessid

    async def _get_body(self, url: str, params: dict[str, str] | None = None) -> Any:
        """GET one ajax endpoint, unwrapping the ``{error, message, body}`` envelope.

        Throttling and gateway errors ride the retry table; an ``error: true``
        envelope becomes a ``PixivApiError`` so callers can tell a deleted work
        from a transport problem; anything else non-2xx raises ``HTTPStatusError``.
        """
        attempts = len(_API_RETRY_DELAYS_SECONDS) + 1
        for attempt in range(1, attempts + 1):
            await self._pace()
            try:
                response = await self.client.get(url, params=params)
            except (httpx.ConnectError, httpx.TimeoutException, httpx.RemoteProtocolError) as exc:
                if attempt >= attempts:
                    raise
                delay = _API_RETRY_DELAYS_SECONDS[attempt - 1]
                log.warning(
                    'Pixiv API request failed url=%s attempt=%s/%s, retry in %.1fs: %s: %s',
                    url,
                    attempt,
                    attempts,
                    delay,
                    exc.__class__.__name__,
                    exc,
                )
                await asyncio.sleep(delay)
                continue

            if response.status_code in _API_RETRYABLE_STATUS_CODES and attempt < attempts:
                delay = _API_RETRY_DELAYS_SECONDS[attempt - 1]
                log.warning(
                    'Pixiv API returned HTTP %s url=%s attempt=%s/%s, retry in %.1fs', response.status_code, url, attempt, attempts, delay
                )
                await response.aclose()
                await asyncio.sleep(delay)
                continue

            try:
                payload = response.json()
            except ValueError:
                payload = None
            if isinstance(payload, dict) and payload.get('error'):
                message = str(payload.get('message') or '').strip() or f'http {response.status_code}'
                raise PixivApiError(message, status_code=response.status_code)
            response.raise_for_status()
            if not isinstance(payload, dict):
                msg = f'Unexpected pixiv API payload from {url}'
                raise PixivError(msg)
            return payload.get('body')

        msg = 'Pixiv API request failed without response'
        raise RuntimeError(msg)

    async def _fetch_bookmark_page(self, user_id: str, offset: int) -> dict[str, Any]:
        params = {'tag': '', 'offset': str(offset), 'limit': str(_PAGE_LIMIT), 'rest': 'show'}
        try:
            body = await self._get_body(_BOOKMARKS_URL.format(user_id=user_id), params=params)
        except PixivApiError as exc:
            msg = f'pixiv rejected the bookmarks request ({exc}). Sign in again in the browser so CookieCloud picks up a fresh session.'
            raise PixivError(msg, notification_dedupe_key='pixiv:auth') from exc
        except httpx.HTTPStatusError as exc:
            if exc.response.status_code in _AUTH_STATUS_CODES:
                msg = (
                    f'pixiv answered the bookmarks request with HTTP {exc.response.status_code}. '
                    'Sign in again in the browser so CookieCloud picks up a fresh session.'
                )
                raise PixivError(msg, notification_dedupe_key='pixiv:auth') from exc
            raise
        if not isinstance(body, dict):
            msg = 'pixiv bookmarks payload has no body'
            raise PixivError(msg)
        return body

    async def _upsert_rows(self, work: PixivWork, entries: list[tuple[int, str]]) -> None:
        """One row per output file of one work; ``entries`` is ``[(num, source_url)]``."""
        rows = [
            (
                str(work.illust_id),
                str(num),
                str(work.illust_type),
                work.title,
                work.author,
                work.author_id,
                str(work.page_count),
                source_url,
            )
            for num, source_url in entries
        ]
        await database.insert_db_batch(
            table='pixiv',
            columns=('illust_id', 'num', 'illust_type', 'title', 'author', 'author_id', 'page_count', 'source_url'),
            rows=rows,
            on_conflict=(
                '(illust_id, num) DO UPDATE SET '
                'illust_type = excluded.illust_type, '
                'title = excluded.title, '
                'author = excluded.author, '
                'author_id = excluded.author_id, '
                'page_count = excluded.page_count, '
                'source_url = excluded.source_url'
            ),
        )

    @staticmethod
    async def _insert_stub_row(work: PixivWork) -> None:
        await database.query_db(
            'INSERT OR IGNORE INTO pixiv (illust_id, num, illust_type, title, author, author_id) VALUES (?, 0, ?, ?, ?, ?);',
            (str(work.illust_id), str(work.illust_type), work.title, work.author, work.author_id),
        )

    async def _mark_masked(self, illust_id: int) -> None:
        """Settle a bookmark whose work is gone (deleted or made private).

        Pending rows become unavailable; rows already downloaded are left alone —
        the file on disk is exactly what the archive is for.
        """
        await database.query_db(
            'INSERT OR IGNORE INTO pixiv (illust_id, num, unavailable, last_error) VALUES (?, 0, 1, ?);',
            (str(illust_id), _MASKED_REASON),
        )
        await database.query_db(
            'UPDATE pixiv SET unavailable = 1, last_error = ? WHERE illust_id = ? AND downloaded = 0 AND unavailable = 0;',
            (_MASKED_REASON, str(illust_id)),
        )

    async def _mark_work_unavailable(self, work: PixivWork, reason: str) -> None:
        await self._insert_stub_row(work)
        await database.query_db(
            'UPDATE pixiv SET unavailable = 1, failed_count = failed_count + 1, last_error = ? WHERE illust_id = ? AND downloaded = 0;',
            (reason[:_ERROR_REASON_MAX_CHARS], str(work.illust_id)),
        )

    async def _mark_work_failed(self, work: PixivWork, reason: str) -> None:
        # The stub keeps a work whose detail fetch failed visible as pending, so
        # the next run retries it even though no page rows exist yet.
        await self._insert_stub_row(work)
        await database.query_db(
            'UPDATE pixiv SET failed_count = failed_count + 1, last_error = ? WHERE illust_id = ? AND downloaded = 0 AND unavailable = 0;',
            (reason[:_ERROR_REASON_MAX_CHARS], str(work.illust_id)),
        )

    async def _mark_downloaded(self, illust_id: int, num: int, dst_path: Path) -> None:
        await database.query_db(
            """
            UPDATE pixiv
            SET downloaded = 1, local_path = ?, unavailable = 0, failed_count = 0, last_error = ''
            WHERE illust_id = ? AND num = ?;
            """,
            (str(dst_path), str(illust_id), str(num)),
        )

    async def _processed_work_ids(self, illust_ids: list[int]) -> set[int]:
        """Which of these works need no further work.

        Processed means at least one row exists and none is pending
        (``downloaded = 0 AND unavailable = 0``) — so a work whose pages were
        upserted but only partially downloaded still counts as unfinished.
        """
        if not illust_ids:
            return set()
        statements = [
            (
                """
                SELECT COUNT(*) AS total,
                       SUM(CASE WHEN downloaded = 0 AND unavailable = 0 THEN 1 ELSE 0 END) AS pending
                FROM pixiv WHERE illust_id = ?;
                """,
                (str(illust_id),),
            )
            for illust_id in illust_ids
        ]
        statement_results = await database.query_db_batch(statements)
        processed: set[int] = set()
        for illust_id, rows in zip(illust_ids, statement_results, strict=True):
            if not rows:
                continue
            total = _to_int(rows[0].get('total')) or 0
            pending = _to_int(rows[0].get('pending')) or 0
            if total > 0 and pending == 0:
                processed.add(illust_id)
        return processed

    async def _crawl_bookmarks(self, user_id: str) -> tuple[list[PixivWork], int, int]:
        """Walk the public bookmarks newest-first, collecting works that still need processing.

        Stops after ``_FULL_HIT_STOP_PAGES`` consecutive pages whose every work
        is already settled, or at the end of the listing. Masked bookmarks are
        settled immediately so they count toward a full hit.
        """
        offset = 0
        page_index = 0
        total_reported = 0
        full_hit_pages = 0
        collected: list[PixivWork] = []
        collected_ids: set[int] = set()

        while True:
            page_index += 1
            body = await self._fetch_bookmark_page(user_id, offset)
            total_reported = _to_int(body.get('total')) or total_reported
            works = parse_bookmark_works(body)
            if not works:
                log.info('Pixiv bookmarks reached end page=%s offset=%s', page_index, offset)
                break

            for work in works:
                if work.masked:
                    await self._mark_masked(work.illust_id)

            visible = [work for work in works if not work.masked]
            page_ids = list({work.illust_id for work in visible})
            processed_ids = await self._processed_work_ids(page_ids)
            if len(processed_ids) == len(page_ids):
                full_hit_pages += 1
                log.info(
                    'Pixiv bookmarks full-hit page=%s works=%s consecutive_full_hits=%s/%s',
                    page_index,
                    len(works),
                    full_hit_pages,
                    _FULL_HIT_STOP_PAGES,
                )
                if full_hit_pages >= _FULL_HIT_STOP_PAGES:
                    log.info('Pixiv bookmarks reached full-hit stop condition')
                    break
            else:
                full_hit_pages = 0
                for work in visible:
                    if work.illust_id in processed_ids or work.illust_id in collected_ids:
                        continue
                    collected_ids.add(work.illust_id)
                    collected.append(work)

            offset += len(works)
            if total_reported and offset >= total_reported:
                log.info('Pixiv bookmarks walked all %s entries', total_reported)
                break

        return collected, page_index, total_reported

    def _build_output_path(self, work: PixivWork, *, num: int, ext: str) -> Path:
        media_id = f'{work.illust_id}_ugoira' if work.illust_type == _ILLUST_TYPE_UGOIRA else f'{work.illust_id}_p{num}'
        folder = sanitize(work.author) or work.author_id or 'unknown'
        filename = format_media_filename(title=work.title, media_id=media_id, uploader=work.author or None, ext=ext)
        return self.cfg.path / folder / filename

    async def _download_image(self, url: str, dst_path: Path) -> bool:
        """Fetch one original into place; returns whether a new file was written."""
        await asyncio.to_thread(dst_path.parent.mkdir, parents=True, exist_ok=True)
        if await asyncio.to_thread(dst_path.exists):
            return False
        response = await self.media_client.get(url)
        response.raise_for_status()
        await asyncio.to_thread(dst_path.write_bytes, response.content)
        return True

    @staticmethod
    def _synthesize_from_zip(zip_bytes: bytes, frames: list[tuple[str, int]], dst_path: Path) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            zip_path = tmp_path / 'ugoira.zip'
            zip_path.write_bytes(zip_bytes)
            with zipfile.ZipFile(zip_path) as archive:
                archive.extractall(tmp_path)
            frame_paths: list[Path] = []
            delays: list[int] = []
            for name, delay in frames:
                frame_path = tmp_path / name
                if not frame_path.is_file():
                    msg = f'ugoira zip is missing frame {name}'
                    raise PixivError(msg)
                frame_paths.append(frame_path)
                delays.append(delay)
            # Encode beside the frames, then move, so a crash never leaves a
            # partial file at the destination.
            encoded_path = tmp_path / 'ugoira.webp'
            synthesize_ugoira_webp(frame_paths, delays, encoded_path)
            shutil.move(str(encoded_path), str(dst_path))

    async def _process_ugoira(self, work: PixivWork) -> int:
        body = await self._get_body(_UGOIRA_META_URL.format(illust_id=work.illust_id))
        if not isinstance(body, dict):
            msg = f'pixiv ugoira_meta payload has no body for {work.illust_id}'
            raise PixivError(msg)
        zip_url, frames = parse_ugoira_meta(body)
        await self._upsert_rows(work, [(0, zip_url)])

        dst_path = self._build_output_path(work, num=0, ext='webp')
        downloaded = 0
        await asyncio.to_thread(dst_path.parent.mkdir, parents=True, exist_ok=True)
        if not await asyncio.to_thread(dst_path.exists):
            response = await self.media_client.get(zip_url)
            response.raise_for_status()
            await asyncio.to_thread(self._synthesize_from_zip, response.content, frames, dst_path)
            downloaded = 1
        await self._mark_downloaded(work.illust_id, 0, dst_path)
        return downloaded

    async def _process_illust(self, work: PixivWork) -> int:
        body = await self._get_body(_PAGES_URL.format(illust_id=work.illust_id))
        urls = parse_page_urls(body)
        if not urls:
            msg = f'pixiv pages payload has no originals for {work.illust_id}'
            raise PixivError(msg)
        await self._upsert_rows(work, list(enumerate(urls)))

        downloaded = 0
        for num, url in enumerate(urls):
            dst_path = self._build_output_path(work, num=num, ext=infer_extension(url))
            if await self._download_image(url, dst_path):
                downloaded += 1
            await self._mark_downloaded(work.illust_id, num, dst_path)
        return downloaded

    async def _process_work(self, work: PixivWork, *, progress: str) -> int:
        """Fetch one work's detail, upsert its rows, download its files.

        A failure settles or defers the whole work rather than aborting the run:
        an ``error: true`` from the work endpoint means it was deleted or made
        private since it was bookmarked, anything else counts as a failed
        attempt that the next run retries.
        """
        try:
            if work.illust_type == _ILLUST_TYPE_UGOIRA:
                downloaded = await self._process_ugoira(work)
            else:
                downloaded = await self._process_illust(work)
        except PixivApiError as exc:
            reason = f'work endpoint error: {exc}'
            await self._mark_work_unavailable(work, reason)
            log.info('Marked unavailable pixiv work %s id=%s: %s', progress, work.illust_id, reason)
            return 0
        except Exception as exc:  # noqa: BLE001
            await self._mark_work_failed(work, f'{exc.__class__.__name__}: {exc}')
            log.warning('Failed to process pixiv work %s id=%s: %s', progress, work.illust_id, exc)
            return 0
        if downloaded:
            log.notice('Pixiv downloaded %s id=%s files=%s', progress, work.illust_id, downloaded)
        return downloaded

    async def _retry_pending_from_db(self, handled_ids: set[int]) -> int:
        """Re-process works earlier runs left pending, straight from the database.

        Goes back through the work endpoints rather than the stored source_url:
        a stub row from a failed detail fetch has no URL at all, and an ugoira's
        frame table is never stored.
        """
        rows = await database.query_db(
            """
            SELECT illust_id, num, illust_type, title, author, author_id, page_count
            FROM pixiv
            WHERE downloaded = 0 AND unavailable = 0
            ORDER BY created_at ASC;
            """,
        )
        pending_by_id: dict[int, dict[str, Any]] = {}
        for row in rows:
            illust_id = _to_int(row.get('illust_id'))
            if illust_id is None or illust_id in handled_ids or illust_id in pending_by_id:
                continue
            pending_by_id[illust_id] = row

        downloaded = 0
        total = len(pending_by_id)
        for index, (illust_id, row) in enumerate(pending_by_id.items(), start=1):
            work = PixivWork(
                illust_id=illust_id,
                title=str(row.get('title') or ''),
                author=str(row.get('author') or ''),
                author_id=str(row.get('author_id') or ''),
                illust_type=_to_int(row.get('illust_type')) or 0,
                page_count=_to_int(row.get('page_count')) or 1,
                masked=False,
            )
            downloaded += await self._process_work(work, progress=f'retry {index}/{total}')
        return downloaded

    async def _notify_summary(self, *, downloaded: int, works: int, total_reported: int) -> None:
        try:
            await enqueue_notification(
                kind='summary',
                source='pixiv',
                header='Pixiv',
                title='Bookmarks update completed',
                body=f'Downloaded {downloaded} files from {works} bookmarked works.',
                payload={'downloaded': downloaded, 'works': works, 'total_reported': total_reported},
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue pixiv summary notification: %s', exc)

    async def update(self) -> None:
        missing = self.cfg.validate_runnable()
        if missing:
            log.warning('Pixiv is not configured (missing %s), skip update', ', '.join(missing))
            return

        phpsessid = await asyncio.to_thread(self._fetch_phpsessid)
        user_id = self.cfg.user_id or derive_user_id(phpsessid)
        if not user_id:
            msg = 'Could not derive the pixiv user id from PHPSESSID; set web.pixiv.user_id explicitly.'
            raise PixivError(msg, notification_dedupe_key='pixiv:auth')
        self.client.headers['Cookie'] = f'PHPSESSID={phpsessid}'

        await self._ensure_table()
        await asyncio.to_thread(self.cfg.path.mkdir, parents=True, exist_ok=True)

        works, page_count, total_reported = await self._crawl_bookmarks(user_id)
        log.info('Pixiv bookmarks scanned pages=%s total_reported=%s works_to_process=%s', page_count, total_reported, len(works))

        downloaded = 0
        for index, work in enumerate(works, start=1):
            downloaded += await self._process_work(work, progress=f'{index}/{len(works)}')
        downloaded += await self._retry_pending_from_db({work.illust_id for work in works})

        log.info('Pixiv downloaded %d new files', downloaded)
        if downloaded:
            await self._notify_summary(downloaded=downloaded, works=len(works), total_reported=total_reported)
