import asyncio
import hashlib
import os
import re
import tempfile
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from email.utils import parsedate_to_datetime
from pathlib import Path
from typing import Any

import httpx
from tqdm import tqdm

from src.core import logger, settings
from src.core.settings import KemonoCreator
from src.tool import database, sanitize
from src.tool.notifications import enqueue_notification

log = logger.get('kemono')

API_PREFIX = '/api/v1'
PAGE_SIZE = 50
# Pawchive's operator asked downloader tools (2026-09-01 notice) to identify
# themselves instead of pretending to be a browser, and to keep to 1 file request
# per second with at most 5 active downloads. The file host's ddos-guard now
# answers a browser UA that is not a real browser fetch with 403 on any file its
# edge cache has not seen, which is every new attachment. Honest tool UAs pass;
# `_pace` and the sequential download loop keep us well inside the rate limits.
DEFAULT_USER_AGENT = 'fav/1.0 (personal archiver; read your file-server notice and respecting it, thank you)'
_API_RETRY_DELAYS_SECONDS = (2.0, 5.0, 15.0, 30.0)
# 403 is deliberately absent: behind Cloudflare it means blocked, and retrying a
# block only feeds the risk-control score.
_RETRYABLE_STATUS_CODES = {429, 502, 503, 504}
_RETRY_AFTER_MAX_DELAY_SECONDS = 300.0
_HTTP_OK = 200
_HTTP_NOT_FOUND = 404
_SHA256_STEM_RE = re.compile(r'^[0-9a-f]{64}$')
# Gaps in days between rechecks for a preview-only attachment's original file.
# The first check runs a day after the preview is archived; a check that still
# finds no original schedules the next gap, and the last gap repeats forever —
# a late original is picked up no matter when it is uploaded.
_ORIGINAL_RECHECK_GAP_DAYS = (1, 3, 5, 7)


class CrawlRunError(RuntimeError):
    pass


@dataclass
class Attachment:
    name: str
    path: str
    # Pawchive marks attachments whose original was never imported: the file
    # host has nothing for them, only a compressed copy on the thumbnail host.
    preview_only: bool = False


@dataclass
class Post:
    id: str
    user: str
    service: str
    title: str
    published: str | None
    attachments: list[Attachment]


@dataclass
class _CreatorStats:
    new_posts: int = 0
    downloaded_files: int = 0
    preview_files: int = 0
    missing_files: int = 0
    creator_missing: bool = False


@dataclass
class _DownloadOutcome:
    status: str  # 'ok' | 'missing' | 'retry'
    detail: str = ''
    retry_after: float | None = None


def should_retry_status(status_code: int) -> bool:
    return status_code in _RETRYABLE_STATUS_CODES


def should_retry_exception(exc: Exception) -> bool:
    return isinstance(exc, (httpx.ConnectError, httpx.TimeoutException, httpx.RemoteProtocolError))


def retry_after_seconds(response: httpx.Response | None) -> float | None:
    if response is None:
        return None
    value = response.headers.get('Retry-After')
    if not value:
        return None
    if value.strip().isdigit():
        return float(value.strip())
    try:
        retry_at = parsedate_to_datetime(value)
    except (TypeError, ValueError):
        return None
    if retry_at.tzinfo is None:
        retry_at = retry_at.replace(tzinfo=UTC)
    return max(0.0, (retry_at - datetime.now(UTC)).total_seconds())


def retry_delay_seconds(attempt: int, retry_after: float | None = None) -> float:
    if retry_after is not None:
        return min(retry_after, _RETRY_AFTER_MAX_DELAY_SECONDS)
    index = min(attempt, len(_API_RETRY_DELAYS_SECONDS)) - 1
    return _API_RETRY_DELAYS_SECONDS[index]


def sha256_from_path(path: str) -> str | None:
    # Kemono-style archives name stored files by their sha256, e.g.
    # /65/9e/659e1483...jpeg, which makes downloads verifiable for free.
    stem = Path(path).stem.lower()
    return stem if _SHA256_STEM_RE.fullmatch(stem) else None


def attachment_filename(name: str, idx: int) -> str:
    ext = Path(name).suffix
    stem = sanitize(Path(name).stem, max_bytes=150)
    return f'{stem or f"file_{idx}"}{ext}'


def build_file_url(file_base_url: str, path: str) -> str:
    return f'{file_base_url.rstrip("/")}/data{path}'


def build_thumbnail_url(thumbnail_base_url: str, path: str) -> str:
    return f'{thumbnail_base_url.rstrip("/")}/thumbnail/data{path}'


def parse_post(data: dict[str, Any]) -> Post:
    attachments: list[Attachment] = []
    main_file = data.get('file') or {}
    if main_file.get('path'):
        name = main_file.get('name') or Path(main_file['path']).name
        attachments.append(Attachment(name=name, path=main_file['path'], preview_only=bool(main_file.get('preview_only'))))

    for item in data.get('attachments', []):
        if not item.get('path'):
            continue
        name = item.get('name') or Path(item['path']).name
        attachments.append(Attachment(name=name, path=item['path'], preview_only=bool(item.get('preview_only'))))

    return Post(
        id=str(data['id']),
        user=str(data['user']),
        service=str(data['service']),
        title=data.get('title') or 'untitled',
        published=data.get('published'),
        attachments=attachments,
    )


def collect_new_posts(page: list[Post], downloaded_ids: set[str]) -> tuple[list[Post], bool]:
    """Take the not-yet-archived prefix of a newest-first page.

    Returns the new posts in listing order and whether an already-archived ID
    was reached (which ends the pagination walk).
    """
    fresh: list[Post] = []
    for post in page:
        if post.id in downloaded_ids:
            return fresh, True
        fresh.append(post)
    return fresh, False


class Kemono:
    def __init__(self) -> None:
        self.cfg = settings.load().web.kemono
        self.client = httpx.AsyncClient(
            base_url=self.cfg.base_url,
            headers={
                'User-Agent': DEFAULT_USER_AGENT,
                'Accept': 'application/json',
            },
            timeout=httpx.Timeout(60, connect=15),
            follow_redirects=True,
            limits=httpx.Limits(max_keepalive_connections=10, max_connections=10),
        )

    async def aclose(self) -> None:
        await self.client.aclose()

    async def _pace(self) -> None:
        if self.cfg.sleep_request_seconds > 0:
            await asyncio.sleep(self.cfg.sleep_request_seconds)

    async def _ensure_table(self) -> None:
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS kemono (
                id TEXT NOT NULL,
                service TEXT NOT NULL,
                user_id TEXT NOT NULL,
                title TEXT NOT NULL,
                creator TEXT,
                published_at TEXT,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (id, service, user_id)
            );
            CREATE TABLE IF NOT EXISTS kemono_pending_originals (
                service TEXT NOT NULL,
                user_id TEXT NOT NULL,
                post_id TEXT NOT NULL,
                path TEXT NOT NULL,
                dst TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                next_check_at TIMESTAMPTZ NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (service, user_id, post_id, path)
            );
        """)

    async def _get_downloaded_ids(self, service: str, user_id: str) -> set[str]:
        rows = await database.query_db(
            'SELECT id FROM kemono WHERE service = ? AND user_id = ?;',
            (service, user_id),
        )
        return {row['id'] for row in rows}

    async def _get_api(self, path: str, params: dict[str, Any] | None = None) -> httpx.Response:
        """GET an API path, retrying throttling and transport errors.

        Returns the response on 200 and 404 (the caller decides what a missing
        resource means); raises for anything else.
        """
        last_exc: Exception | None = None
        attempts = len(_API_RETRY_DELAYS_SECONDS) + 1
        for attempt in range(1, attempts + 1):
            await self._pace()
            try:
                response = await self.client.get(path, params=params)
            except Exception as exc:
                if not should_retry_exception(exc) or attempt >= attempts:
                    raise
                last_exc = exc
                delay = retry_delay_seconds(attempt)
                log.warning(
                    'Kemono API request failed %s attempt=%s/%s, retry in %.1fs: %s: %s',
                    path,
                    attempt,
                    attempts,
                    delay,
                    exc.__class__.__name__,
                    exc,
                )
                await asyncio.sleep(delay)
                continue

            if should_retry_status(response.status_code) and attempt < attempts:
                delay = retry_delay_seconds(attempt, retry_after_seconds(response))
                log.warning(
                    'Kemono API returned HTTP %s for %s attempt=%s/%s, retry in %.1fs',
                    response.status_code,
                    path,
                    attempt,
                    attempts,
                    delay,
                )
                await response.aclose()
                await asyncio.sleep(delay)
                continue

            if response.status_code == _HTTP_NOT_FOUND:
                return response
            response.raise_for_status()
            return response

        if last_exc is not None:
            raise last_exc
        msg = 'Kemono API request failed without response'
        raise RuntimeError(msg)

    async def _fetch_new_posts(self, service: str, user_id: str, downloaded_ids: set[str]) -> list[Post] | None:
        """Collect newest-first posts, paging until the first already-archived ID.

        Returns None when the creator does not exist upstream.
        """
        new_posts: list[Post] = []
        offset = 0
        while True:
            response = await self._get_api(f'{API_PREFIX}/{service}/user/{user_id}', params={'o': offset})
            if response.status_code == _HTTP_NOT_FOUND:
                # At offset 0 the whole creator is missing; past it, the listing just ended.
                return None if offset == 0 else new_posts
            page = [parse_post(item) for item in response.json()]
            fresh, hit_known = collect_new_posts(page, downloaded_ids)
            new_posts.extend(fresh)
            if hit_known or len(page) < PAGE_SIZE:
                return new_posts
            offset += PAGE_SIZE

    async def _attempt_download(self, src_url: str, dst_path: Path, expected_sha256: str | None, desc: str) -> _DownloadOutcome:
        async with self.client.stream('GET', src_url) as response:
            if response.status_code == _HTTP_NOT_FOUND:
                return _DownloadOutcome('missing')
            if should_retry_status(response.status_code):
                return _DownloadOutcome(
                    'retry',
                    detail=f'HTTP {response.status_code}',
                    retry_after=retry_after_seconds(response),
                )
            response.raise_for_status()

            digest = hashlib.sha256()
            fd, tmp_name = await asyncio.to_thread(tempfile.mkstemp, prefix=f'.{desc}-', dir=dst_path.parent)
            tmp_path = Path(tmp_name)
            try:
                total = int(response.headers.get('content-length', 0))
                with os.fdopen(fd, 'wb') as f, tqdm(total=total, unit='B', unit_scale=True, desc=desc, dynamic_ncols=True) as pbar:
                    async for chunk in response.aiter_bytes():
                        f.write(chunk)
                        digest.update(chunk)
                        pbar.update(len(chunk))
            except BaseException:
                await asyncio.to_thread(tmp_path.unlink, missing_ok=True)
                raise

            # A truncated body shows up as a digest mismatch, so treat it as retryable.
            if expected_sha256 and digest.hexdigest() != expected_sha256:
                await asyncio.to_thread(tmp_path.unlink, missing_ok=True)
                return _DownloadOutcome('retry', detail=f'sha256 mismatch (got {digest.hexdigest()})')

            await asyncio.to_thread(tmp_path.replace, dst_path)
            return _DownloadOutcome('ok')

    async def _download_with_retries(self, src_url: str, dst_path: Path, expected_sha256: str | None, desc: str, label: str) -> bool:
        """Run the retry ladder around one download. True when the file landed,
        False when the host reports it missing (404); raises when attempts run out."""
        attempts = len(_API_RETRY_DELAYS_SECONDS) + 1
        for attempt in range(1, attempts + 1):
            await self._pace()
            try:
                outcome = await self._attempt_download(src_url, dst_path, expected_sha256, desc)
            except Exception as exc:
                if not should_retry_exception(exc) or attempt >= attempts:
                    raise
                delay = retry_delay_seconds(attempt)
                log.warning(
                    'Download failed %s attempt=%s/%s, retry in %.1fs: %s: %s',
                    label,
                    attempt,
                    attempts,
                    delay,
                    exc.__class__.__name__,
                    exc,
                )
                await asyncio.sleep(delay)
                continue

            if outcome.status == 'ok':
                return True
            if outcome.status == 'missing':
                log.warning('Attachment missing upstream (404): %s', label)
                return False
            if attempt >= attempts:
                msg = f'Download of {label} failed after {attempts} attempts: {outcome.detail}'
                raise RuntimeError(msg)
            delay = retry_delay_seconds(attempt, outcome.retry_after)
            log.warning(
                'Download retryable failure %s attempt=%s/%s, retry in %.1fs: %s',
                label,
                attempt,
                attempts,
                delay,
                outcome.detail,
            )
            await asyncio.sleep(delay)

        msg = f'Download of {label} failed'
        raise RuntimeError(msg)

    async def _download_attachment(self, attachment: Attachment, dst_dir: Path, post_id: str, idx: int) -> bool:
        """Download one attachment. True when the file is present locally,
        False when the archive never stored it (upstream 404)."""
        src_url = build_file_url(self.cfg.file_base_url, attachment.path)
        dst_path = dst_dir / attachment_filename(attachment.name, idx)
        # Safe again now that files are promoted atomically: an existing file is complete.
        if dst_path.exists():
            log.debug('File already exists, skip %s', dst_path.name)
            return True

        await asyncio.to_thread(dst_dir.mkdir, parents=True, exist_ok=True)
        return await self._download_with_retries(src_url, dst_path, sha256_from_path(attachment.path), f'{post_id}-{idx}', attachment.path)

    async def _queue_original_recheck(self, post: Post, attachment: Attachment, dst_path: Path) -> None:
        # INSERT OR IGNORE keeps the first schedule when a creator run resumes
        # over a post it already queued.
        next_check_at = datetime.now(UTC) + timedelta(days=_ORIGINAL_RECHECK_GAP_DAYS[0])
        await database.query_db(
            'INSERT OR IGNORE INTO kemono_pending_originals (service, user_id, post_id, path, dst, attempts, next_check_at)'
            ' VALUES (?, ?, ?, ?, ?, 0, ?);',
            (post.service, post.user, post.id, attachment.path, dst_path.relative_to(self.cfg.path).as_posix(), next_check_at),
        )

    async def _download_preview(self, attachment: Attachment, dst_dir: Path, post: Post, idx: int) -> bool:
        """Archive the compressed thumbnail of a preview-only attachment and queue
        rechecks for a late original upload. True when a preview is on disk.

        Never raises: the file host has nothing for these paths (it answers 404,
        or 403 while ddos-guard is in a mood), so a failed thumbnail must not
        abort the creator the way a real attachment failure does."""
        dst_path = dst_dir / attachment_filename(attachment.name, idx)
        await self._queue_original_recheck(post, attachment, dst_path)
        if dst_path.exists():
            log.debug('File already exists, skip %s', dst_path.name)
            return True

        await asyncio.to_thread(dst_dir.mkdir, parents=True, exist_ok=True)
        src_url = build_thumbnail_url(self.cfg.thumbnail_base_url, attachment.path)
        # The path stem is the original file's sha256, so the thumbnail cannot be verified against it.
        try:
            return await self._download_with_retries(src_url, dst_path, None, f'{post.id}-{idx}-preview', attachment.path)
        except Exception as exc:  # noqa: BLE001
            log.warning('Preview download failed %s: %s: %s', attachment.path, exc.__class__.__name__, exc)
            return False

    async def _download_post(self, post: Post, creator_dir: Path) -> tuple[int, int, int]:
        """Returns (downloaded, previews, missing) attachment counts. Transient failures
        propagate so the caller can abort the creator and resume here on the next run."""
        if not post.attachments:
            log.warning('Post %s has no downloadable attachments', post.id)
            return 0, 0, 0
        post_dirname = f'{sanitize(post.title, max_bytes=80) or "post"} [{post.id}]'
        post_dir = creator_dir / post_dirname
        downloaded = 0
        previews = 0
        missing = 0
        for idx, attachment in enumerate(post.attachments, start=1):
            if attachment.preview_only:
                if await self._download_preview(attachment, post_dir, post, idx):
                    previews += 1
                else:
                    missing += 1
            elif await self._download_attachment(attachment, post_dir, post.id, idx):
                downloaded += 1
            else:
                missing += 1
        return downloaded, previews, missing

    async def _recheck_pending_original(self, row: dict[str, Any]) -> bool:
        """Try once to replace an archived preview with its original. True when the
        original landed; otherwise advance the 1/3/5/7/7/7/... schedule (the 7-day
        gap repeats forever). Download errors never propagate."""
        key = (row['service'], row['user_id'], row['post_id'], row['path'])
        dst_path = self.cfg.path / row['dst']
        src_url = build_file_url(self.cfg.file_base_url, row['path'])
        await asyncio.to_thread(dst_path.parent.mkdir, parents=True, exist_ok=True)
        await self._pace()
        try:
            outcome = await self._attempt_download(src_url, dst_path, sha256_from_path(row['path']), f'original-{row["post_id"]}')
        except Exception as exc:  # noqa: BLE001
            outcome = _DownloadOutcome('retry', detail=f'{exc.__class__.__name__}: {exc}')

        if outcome.status == 'ok':
            await database.query_db(
                'DELETE FROM kemono_pending_originals WHERE service = ? AND user_id = ? AND post_id = ? AND path = ?;',
                key,
            )
            log.info('Original uploaded, replaced preview: %s', dst_path)
            return True

        attempts = int(row['attempts']) + 1
        gap_days = _ORIGINAL_RECHECK_GAP_DAYS[min(attempts, len(_ORIGINAL_RECHECK_GAP_DAYS) - 1)]
        next_check_at = datetime.now(UTC) + timedelta(days=gap_days)
        await database.query_db(
            'UPDATE kemono_pending_originals SET attempts = ?, next_check_at = ?'
            ' WHERE service = ? AND user_id = ? AND post_id = ? AND path = ?;',
            (attempts, next_check_at, *key),
        )
        log.info('Original still unavailable (%s), next check in %dd: %s', outcome.detail or 'missing', gap_days, row['path'])
        return False

    async def _promote_pending_originals(self) -> int:
        """Process due original-file rechecks; returns how many previews were replaced."""
        rows = await database.query_db(
            'SELECT service, user_id, post_id, path, dst, attempts FROM kemono_pending_originals WHERE next_check_at <= ?;',
            (datetime.now(UTC),),
        )
        promoted = 0
        for row in rows:
            try:
                if await self._recheck_pending_original(row):
                    promoted += 1
            except Exception:
                log.exception('Original recheck failed for %s/%s post %s', row.get('service'), row.get('user_id'), row.get('post_id'))
        return promoted

    async def _update_creator(self, creator: KemonoCreator) -> _CreatorStats:
        creator_name = creator.name or creator.id
        creator_dirname = f'{sanitize(creator_name, max_bytes=80)}_{creator.id}'
        creator_dir = self.cfg.path / creator.service / creator_dirname
        downloaded_ids = await self._get_downloaded_ids(creator.service, creator.id)

        stats = _CreatorStats()
        new_posts = await self._fetch_new_posts(creator.service, creator.id, downloaded_ids)
        if new_posts is None:
            log.warning('Creator %s (%s/%s) not found upstream', creator_name, creator.service, creator.id)
            stats.creator_missing = True
            return stats
        if not new_posts:
            log.info('No new posts for %s (%s)', creator_name, creator.service)
            return stats

        log.info('Found %d new posts for %s (%s)', len(new_posts), creator_name, creator.service)
        # Oldest first, and a post is only recorded once its files are on disk. A
        # transient failure therefore aborts the creator with nothing at or after
        # the failed post in the DB, and the next run resumes exactly there.
        for idx, post in enumerate(reversed(new_posts), start=1):
            log.info('Downloading post %s (%d/%d)', post.id, idx, len(new_posts))
            downloaded, previews, missing = await self._download_post(post, creator_dir)
            await database.query_db(
                'INSERT OR IGNORE INTO kemono (id, service, user_id, title, creator, published_at) VALUES (?, ?, ?, ?, ?, ?);',
                (post.id, post.service, post.user, post.title, creator_name, post.published or ''),
            )
            stats.new_posts += 1
            stats.downloaded_files += downloaded
            stats.preview_files += previews
            stats.missing_files += missing
        return stats

    async def _notify_summary(self, totals: _CreatorStats, missing_creators: list[str], failed: list[str], promoted: int) -> None:
        if totals.new_posts == 0 and totals.missing_files == 0 and promoted == 0 and not missing_creators and not failed:
            log.info('No new Kemono posts or warnings; summary notification skipped')
            return

        body = f'Archived {totals.new_posts} new posts with {totals.downloaded_files} files.'
        if totals.preview_files:
            body += f' {totals.preview_files} preview-only files saved as thumbnails.'
        if promoted:
            body += f' {promoted} originals replaced previews.'
        if totals.missing_files:
            body += f' {totals.missing_files} files missing upstream.'
        if missing_creators:
            body += f' Creators not found: {", ".join(missing_creators)}.'
        if failed:
            body += f' Creators failed: {", ".join(failed)}.'
        try:
            await enqueue_notification(
                kind='summary',
                source='kemono',
                header='Kemono',
                title='Update completed',
                body=body,
                payload={
                    'new_posts': totals.new_posts,
                    'downloaded_files': totals.downloaded_files,
                    'preview_files': totals.preview_files,
                    'promoted_originals': promoted,
                    'missing_files': totals.missing_files,
                    'missing_creators': missing_creators,
                    'failed_creators': failed,
                },
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue kemono summary notification: %s', exc)

    async def update(self) -> None:
        if not self.cfg.creators:
            log.info('No kemono creators configured')
            return

        self.cfg.path.mkdir(parents=True, exist_ok=True)
        await self._ensure_table()
        promoted = await self._promote_pending_originals()

        totals = _CreatorStats()
        missing_creators: list[str] = []
        failed: list[str] = []
        for creator in self.cfg.creators:
            label = f'{creator.service}/{creator.id}'
            try:
                stats = await self._update_creator(creator)
            except Exception:
                log.exception('Kemono creator %s (%s) failed', creator.name or creator.id, creator.service)
                failed.append(label)
                continue
            totals.new_posts += stats.new_posts
            totals.downloaded_files += stats.downloaded_files
            totals.preview_files += stats.preview_files
            totals.missing_files += stats.missing_files
            if stats.creator_missing:
                missing_creators.append(label)

        await self._notify_summary(totals, missing_creators, failed, promoted)
        if failed:
            msg = f'{len(failed)} creator(s) failed: {", ".join(failed)}'
            raise CrawlRunError(msg)
