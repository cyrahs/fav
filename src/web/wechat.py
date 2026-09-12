"""WeChat iLink bot receiver.

Nothing here crawls. The user forwards or sends media to an iLink bot from
WeChat; each configured bot is long-polled by one listener task, every media
item is written to a durable queue the moment it is seen, and one serial worker
per account downloads, decrypts and archives the rows. The cursor the server
hands back (``get_updates_buf``) is only persisted after the queue rows are, so
a crash between the two replays the page instead of dropping it.
"""

from __future__ import annotations

import asyncio
import contextlib
import hashlib
import json
import shutil
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING
from uuid import uuid4

import httpx

from src.core import logger, settings
from src.tool import database, ensure_unique_path, format_media_filename, sanitize
from src.tool.notifications import enqueue_notification
from src.tool.wechat_ilink import CdnMedia, ILinkClient, ILinkError, InboundMessage, MediaItem
from src.tool.wechat_link import build_client as build_link_client
from src.tool.wechat_link import download_link_images, sniff_image_extension
from src.tool.wechat_queue import (
    WeChatMediaJob,
    claim_next_wechat_media_job,
    count_pending_wechat_media_jobs,
    enqueue_wechat_media_job,
    ensure_wechat_media_queue_table,
    mark_wechat_media_job_completed,
    mark_wechat_media_job_discarded,
    mark_wechat_media_job_retry,
    release_pending_wechat_media_jobs,
    reset_processing_wechat_media_jobs,
    wechat_media_retry_delay,
)
from src.tool.wechat_webwx import WebWxClient

if TYPE_CHECKING:
    from collections.abc import Callable

    from src.core.settings import WeChatAccount, WeChatMediaType

log = logger.get('wechat')

_WECHAT_SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS wechat (
    account_name TEXT NOT NULL,
    message_id BIGINT NOT NULL,
    item_index INTEGER NOT NULL,
    media_type TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    from_user_id TEXT NOT NULL DEFAULT '',
    local_path TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (account_name, message_id, item_index)
);

CREATE TABLE IF NOT EXISTS wechat_account_state (
    account_name TEXT PRIMARY KEY,
    get_updates_buf TEXT NOT NULL DEFAULT '',
    paused_until TIMESTAMPTZ NULL,
    last_error TEXT NOT NULL DEFAULT '',
    last_message_at TIMESTAMPTZ NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

ALTER TABLE wechat_account_state ADD COLUMN IF NOT EXISTS webwx_session TEXT NOT NULL DEFAULT '';
"""
_QUEUE_IDLE_POLL_SECONDS = 1.0
_POLL_RETRY_INITIAL_SECONDS = 2.0
_POLL_RETRY_MAX_SECONDS = 60.0
_RECONNECT_INITIAL_SECONDS = 5.0
_RECONNECT_MAX_SECONDS = 300.0
_PAUSE_POLL_SECONDS = 60.0
_ONE_SHOT_POLL_TIMEOUT_SECONDS = 10.0
_MD5_CHUNK_BYTES = 1 << 20
_SNIFF_BYTES = 16


async def ensure_wechat_tables() -> None:
    await database.query_db_multi(_WECHAT_SCHEMA_SQL)
    await ensure_wechat_media_queue_table()
    log.debug('WeChat archive, account state, and media queue tables initialized')


async def get_webwx_session(account_name: str) -> str:
    rows = await database.query_db('SELECT webwx_session FROM wechat_account_state WHERE account_name = ?;', (account_name,))
    return str(rows[0]['webwx_session'] or '') if rows else ''


async def save_webwx_session(account_name: str, snapshot: str) -> None:
    """Store the web session (cookies, tokens, SyncKey); also lifts any pause, since a fresh scan lands here."""
    await database.query_db(
        """
        INSERT INTO wechat_account_state (account_name, webwx_session, last_error)
        VALUES (?, ?, '')
        ON CONFLICT (account_name)
        DO UPDATE SET
            webwx_session = EXCLUDED.webwx_session,
            paused_until = CASE
                WHEN EXCLUDED.webwx_session <> wechat_account_state.webwx_session THEN NULL
                ELSE wechat_account_state.paused_until
            END,
            last_error = '',
            updated_at = CURRENT_TIMESTAMP;
        """,
        (account_name, snapshot),
    )


@dataclass(frozen=True, slots=True)
class WeChatAccountState:
    get_updates_buf: str
    pause_remaining_seconds: float = 0.0
    last_error: str = ''


class WeChat:
    def __init__(self, *, client_factory: Callable[[WeChatAccount], ILinkClient] | None = None) -> None:
        self._tmp_dir = tempfile.TemporaryDirectory(prefix='fav-wechat-')
        self.cache_dir = Path(self._tmp_dir.name)
        self._client_factory = client_factory or self._default_client
        self._clients: dict[str, ILinkClient] = {}
        self._worker_wake_events: dict[str, asyncio.Event] = {}
        self._account_ready_events: dict[str, asyncio.Event] = {}
        self._close_event = asyncio.Event()
        self._running = False
        self._closed = False
        # Public HTTP for the pages behind link cards; unrelated to either transport's session.
        self._link_client = build_link_client()

    @property
    def cfg(self) -> settings.WeChat:
        # Long-lived runtime: resolve per access so UI edits apply without a restart.
        return settings.load().web.wechat

    def _default_client(self, account: WeChatAccount) -> ILinkClient | WebWxClient:
        if account.transport == 'filehelper':
            name = account.name
            return WebWxClient(
                session_loader=lambda: self.get_webwx_session(name),
                session_saver=lambda snapshot: self.save_webwx_session(name, snapshot),
            )
        return ILinkClient(base_url=account.base_url, cdn_base_url=account.cdn_base_url, bot_token=account.bot_token)

    def __del__(self) -> None:
        with contextlib.suppress(Exception):
            self._tmp_dir.cleanup()

    async def aclose(self) -> None:
        if self._closed:
            return
        self._closed = True
        self._close_event.set()
        for client in list(self._clients.values()):
            with contextlib.suppress(Exception):
                await client.aclose()
        self._clients.clear()
        with contextlib.suppress(Exception):
            await self._link_client.aclose()
        self._tmp_dir.cleanup()

    async def _initialize_tables(self) -> None:
        await ensure_wechat_tables()

    # ---- account state -----------------------------------------------------

    @staticmethod
    async def get_account_state(account_name: str) -> WeChatAccountState:
        rows = await database.query_db(
            """
            SELECT
                get_updates_buf,
                last_error,
                CASE
                    WHEN paused_until IS NULL OR paused_until <= CURRENT_TIMESTAMP THEN 0
                    ELSE EXTRACT(EPOCH FROM (paused_until - CURRENT_TIMESTAMP))
                END AS pause_remaining_seconds
            FROM wechat_account_state
            WHERE account_name = ?;
            """,
            (account_name,),
        )
        if not rows:
            return WeChatAccountState(get_updates_buf='')
        row = rows[0]
        return WeChatAccountState(
            get_updates_buf=str(row['get_updates_buf'] or ''),
            pause_remaining_seconds=float(row['pause_remaining_seconds'] or 0),
            last_error=str(row['last_error'] or ''),
        )

    @staticmethod
    async def save_updates_buf(account_name: str, get_updates_buf: str) -> None:
        await database.query_db(
            """
            INSERT INTO wechat_account_state (account_name, get_updates_buf, last_error)
            VALUES (?, ?, '')
            ON CONFLICT (account_name)
            DO UPDATE SET
                get_updates_buf = EXCLUDED.get_updates_buf,
                last_error = '',
                updated_at = CURRENT_TIMESTAMP;
            """,
            (account_name, get_updates_buf),
        )

    @staticmethod
    async def set_account_pause(account_name: str, seconds: float, *, error: str = '') -> None:
        await database.query_db(
            """
            INSERT INTO wechat_account_state (account_name, paused_until, last_error)
            VALUES (?, CURRENT_TIMESTAMP + (? * INTERVAL '1 second'), ?)
            ON CONFLICT (account_name)
            DO UPDATE SET
                paused_until = CURRENT_TIMESTAMP + (? * INTERVAL '1 second'),
                last_error = EXCLUDED.last_error,
                updated_at = CURRENT_TIMESTAMP;
            """,
            (account_name, max(0.0, seconds), error[:500], max(0.0, seconds)),
        )

    @staticmethod
    async def get_webwx_session(account_name: str) -> str:
        return await get_webwx_session(account_name)

    @staticmethod
    async def save_webwx_session(account_name: str, snapshot: str) -> None:
        await save_webwx_session(account_name, snapshot)

    @staticmethod
    async def mark_account_message(account_name: str) -> None:
        await database.query_db(
            """
            INSERT INTO wechat_account_state (account_name, last_message_at)
            VALUES (?, CURRENT_TIMESTAMP)
            ON CONFLICT (account_name)
            DO UPDATE SET last_message_at = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP;
            """,
            (account_name,),
        )

    @staticmethod
    async def is_downloaded(account_name: str, message_id: int, item_index: int) -> bool:
        rows = await database.query_db(
            'SELECT 1 FROM wechat WHERE account_name = ? AND message_id = ? AND item_index = ? LIMIT 1;',
            (account_name, message_id, item_index),
        )
        return bool(rows)

    # ---- inbound messages --------------------------------------------------

    @staticmethod
    def select_media(message: InboundMessage, account: WeChatAccount) -> list[MediaItem]:
        """The items of ``message`` this account archives; empty when the message is not for us.

        Only the user who scanned the QR code is trusted: the bot id is not a
        secret, and an unexpected sender would otherwise fill the archive.
        """
        if not message.is_user_message or message.group_id:
            return []
        # A 文件传输助手 conversation only ever carries the user's own messages, and
        # the web protocol's sender ids are per-login, so the filter is iLink-only.
        if account.transport == 'ilink' and account.user_id and message.from_user_id != account.user_id:
            log.warning(
                'WeChat account %s ignored message %s from unexpected sender %s', account.name, message.message_id, message.from_user_id
            )
            return []
        wanted = set(account.media_types)
        return [item for item in message.media if item.kind in wanted]

    async def _enqueue_message(self, account: WeChatAccount, message: InboundMessage) -> int:
        items = self.select_media(message, account)
        if not items:
            if message.media:
                log.info(
                    'WeChat account %s skipped message %s (%s)',
                    account.name,
                    message.message_id,
                    ', '.join(item.kind for item in message.media),
                )
            return 0
        inserted = 0
        for item in items:
            media_type: WeChatMediaType = item.kind  # type: ignore[assignment]
            created = await enqueue_wechat_media_job(
                account_name=account.name,
                message_id=message.message_id,
                item_index=item.index,
                media_type=media_type,
                title=message.text,
                file_name=item.file_name,
                from_user_id=message.from_user_id,
                context_token=message.context_token,
                encrypt_query_param=item.media.encrypt_query_param,
                aes_key=item.media.aes_key,
                media_size=item.size,
                media_md5=item.md5,
            )
            inserted += int(created)
        if inserted:
            await self.mark_account_message(account.name)
            log.info('WeChat queued %d/%d media for account=%s message=%s', inserted, len(items), account.name, message.message_id)
        return inserted

    async def _handle_session_expired(self, account: WeChatAccount, exc: ILinkError) -> None:
        pause = self.cfg.session_pause_seconds
        await self.set_account_pause(account.name, pause, error=f'session expired (errcode {exc.errcode})')
        log.error(
            'WeChat bot session for account %s has expired; pausing %.0fs. Scan the QR code again from the settings page.',
            account.name,
            pause,
        )
        try:
            await enqueue_notification(
                kind='session_expired',
                source='wechat',
                header='WeChat',
                title='WeChat bot session expired',
                body=f'Account {account.name}: scan the QR code again from the settings page.',
                dedupe_key=f'wechat:session-expired:{account.name}',
                payload={'account_name': account.name, 'errcode': exc.errcode},
            )
        except Exception as notify_exc:  # noqa: BLE001
            log.warning('Failed to enqueue WeChat session notification for account %s: %s', account.name, notify_exc)

    async def _poll_once(self, account: WeChatAccount, client: ILinkClient, *, timeout_seconds: float) -> float | None:
        """One getupdates round: persist what came in, then advance the cursor.

        Returns the timeout the server suggests for the next round, if any.
        """
        state = await self.get_account_state(account.name)
        page = await client.get_updates(state.get_updates_buf, timeout_seconds=timeout_seconds)
        inserted = 0
        for message in page.messages:
            inserted += await self._enqueue_message(account, message)
        if page.get_updates_buf and page.get_updates_buf != state.get_updates_buf:
            await self.save_updates_buf(account.name, page.get_updates_buf)
        if inserted:
            self._worker_wake_events.setdefault(account.name, asyncio.Event()).set()
        return page.long_poll_timeout_seconds

    async def _poll_account(self, account: WeChatAccount, client: ILinkClient, stop_event: asyncio.Event) -> None:
        timeout_seconds = self.cfg.long_poll_timeout_seconds
        retry_delay = _POLL_RETRY_INITIAL_SECONDS
        while not stop_event.is_set() and not self._close_event.is_set():
            state = await self.get_account_state(account.name)
            if state.pause_remaining_seconds > 0:
                await self._sleep(min(state.pause_remaining_seconds, _PAUSE_POLL_SECONDS), stop_event)
                continue
            try:
                suggested = await self._poll_once(account, client, timeout_seconds=timeout_seconds)
            except ILinkError as exc:
                if exc.session_expired:
                    await self._handle_session_expired(account, exc)
                    continue
                log.warning('WeChat getupdates failed for account %s; retry in %.0fs: %s', account.name, retry_delay, exc)
                await self._sleep(retry_delay, stop_event)
                retry_delay = min(retry_delay * 2, _POLL_RETRY_MAX_SECONDS)
                continue
            except (httpx.HTTPError, OSError) as exc:
                log.warning('WeChat getupdates error for account %s; retry in %.0fs: %s', account.name, retry_delay, exc)
                await self._sleep(retry_delay, stop_event)
                retry_delay = min(retry_delay * 2, _POLL_RETRY_MAX_SECONDS)
                continue
            retry_delay = _POLL_RETRY_INITIAL_SECONDS
            if suggested is not None and suggested > 0:
                timeout_seconds = suggested

    # ---- downloads ---------------------------------------------------------

    @staticmethod
    def _sniff_image_extension(path: Path) -> str:
        try:
            with path.open('rb') as handle:
                head = handle.read(_SNIFF_BYTES)
        except OSError:
            return 'jpg'
        return sniff_image_extension(head)

    @classmethod
    def build_filename(cls, job: WeChatMediaJob, downloaded: Path) -> str:
        """``<caption or kind> [<message id>].<ext>``; files keep their own name and extension."""
        media_id = str(job.message_id) if job.item_index == 0 else f'{job.message_id}-{job.item_index}'
        if job.media_type == 'file':
            original = Path(sanitize(job.file_name)) if job.file_name.strip() else Path()
            ext = original.suffix.lstrip('.') or 'bin'
            title = original.stem or job.title.strip() or 'file'
            return format_media_filename(title=title, media_id=media_id, ext=ext)
        ext = 'mp4' if job.media_type == 'video' else cls._sniff_image_extension(downloaded)
        title = job.title.strip() or job.media_type
        return format_media_filename(title=title, media_id=media_id, ext=ext)

    @staticmethod
    def _file_md5(path: Path) -> str:
        digest = hashlib.md5()  # noqa: S324 -- integrity check against the md5 WeChat reports, not a security use
        with path.open('rb') as handle:
            while chunk := handle.read(_MD5_CHUNK_BYTES):
                digest.update(chunk)
        return digest.hexdigest()

    async def _notify_download(self, *, account: WeChatAccount, job: WeChatMediaJob, saved_path: Path) -> None:
        payload = {
            'account_name': account.name,
            'message_id': job.message_id,
            'item_index': job.item_index,
            'saved_path': str(saved_path),
        }
        if job.media_type == 'image':
            payload['image_path'] = str(saved_path)
        try:
            await enqueue_notification(
                kind='download_completed',
                source='wechat',
                header='WeChat',
                title=saved_path.name,
                body=f'Account {account.name} | Message ID {job.message_id}',
                payload=payload,
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue wechat download notification for message %s: %s', job.message_id, exc)

    async def _archive_link_job(self, *, account: WeChatAccount, job: WeChatMediaJob) -> Path:
        """A link card: fetch the page and keep its pictures in one folder per link."""
        locator = json.loads(job.encrypt_query_param)
        url = str(locator.get('url') or '')
        if not url:
            msg = 'link card carries no URL'
            raise ILinkError(msg, retryable=False)
        title = sanitize(job.title.strip() or str(locator.get('title') or '').strip() or 'link', max_bytes=120)
        folder = ensure_unique_path(account.path / f'{title} [{job.message_id}]')
        saved = await download_link_images(self._link_client, url, folder)
        if not saved:
            with contextlib.suppress(OSError):
                folder.rmdir()
            msg = f'no images found behind {url}'
            raise ILinkError(msg, retryable=False)
        await database.query_db(
            """
            INSERT INTO wechat (account_name, message_id, item_index, media_type, title, from_user_id, local_path)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (account_name, message_id, item_index) DO NOTHING;
            """,
            (account.name, job.message_id, job.item_index, job.media_type, job.title, job.from_user_id, str(folder)),
        )
        try:
            await enqueue_notification(
                kind='download_completed',
                source='wechat',
                header='WeChat',
                title=job.title.strip() or url,
                body=f'{len(saved)} image(s) | Account {account.name} | Message ID {job.message_id}',
                link_url=url,
                payload={
                    'account_name': account.name,
                    'message_id': job.message_id,
                    'item_index': job.item_index,
                    'saved_path': str(folder),
                    'image_path': str(saved[0]),
                    'image_count': len(saved),
                    'url': url,
                },
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue wechat link notification for message %s: %s', job.message_id, exc)
        return folder

    async def _archive_job(self, *, account: WeChatAccount, client: ILinkClient, job: WeChatMediaJob) -> Path:
        if job.media_type == 'link':
            return await self._archive_link_job(account=account, job=job)
        stem = f'{account.name}-{job.message_id}-{job.item_index}'
        scratch = self.cache_dir / f'{stem}.enc'
        partial = self.cache_dir / f'{stem}.part'
        media = CdnMedia(encrypt_query_param=job.encrypt_query_param, aes_key=job.aes_key)
        try:
            size = await client.download_media(media, partial, scratch=scratch)
            if size <= 0:
                msg = 'CDN returned an empty file'
                raise ILinkError(msg)
            if job.media_md5:
                actual = await asyncio.to_thread(self._file_md5, partial)
                if actual.lower() != job.media_md5.lower():
                    log.warning(
                        'WeChat md5 mismatch for account=%s message=%s: reported %s, got %s',
                        account.name,
                        job.message_id,
                        job.media_md5,
                        actual,
                    )
            filename = self.build_filename(job, partial)
            account.path.mkdir(parents=True, exist_ok=True)
            destination = ensure_unique_path(account.path / filename)
            await asyncio.to_thread(shutil.move, str(partial), str(destination))
        finally:
            partial.unlink(missing_ok=True)
            scratch.unlink(missing_ok=True)
        await database.query_db(
            """
            INSERT INTO wechat (account_name, message_id, item_index, media_type, title, from_user_id, local_path)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (account_name, message_id, item_index) DO NOTHING;
            """,
            (account.name, job.message_id, job.item_index, job.media_type, job.title, job.from_user_id, str(destination)),
        )
        await self._notify_download(account=account, job=job, saved_path=destination)
        return destination

    async def _process_job(  # noqa: PLR0911
        self,
        *,
        account: WeChatAccount,
        client: ILinkClient,
        job: WeChatMediaJob,
        owner_token: str,
    ) -> bool:
        if job.media_type not in account.media_types:
            await mark_wechat_media_job_discarded(job, owner_token, error='Media type is no longer configured')
            return False
        if await self.is_downloaded(account.name, job.message_id, job.item_index):
            await mark_wechat_media_job_completed(job, owner_token)
            log.info('WeChat queue completed already archived message %s/%s', job.message_id, job.item_index)
            return False
        try:
            saved = await self._archive_job(account=account, client=client, job=job)
        except asyncio.CancelledError:
            raise
        except ILinkError as exc:
            await self._fail_job(job, owner_token, error=f'{exc.__class__.__name__}: {exc}', retryable=exc.retryable)
            return False
        except ValueError as exc:
            # A key that does not open the object or padding that does not check
            # out will not change on retry.
            await self._fail_job(job, owner_token, error=f'{exc.__class__.__name__}: {exc}', retryable=False)
            return False
        except (httpx.HTTPError, OSError) as exc:
            await self._fail_job(job, owner_token, error=f'{exc.__class__.__name__}: {exc}', retryable=True)
            return False
        except Exception as exc:
            log.exception('Unexpected WeChat queue error account=%s message=%s', account.name, job.message_id)
            await self._fail_job(job, owner_token, error=f'{exc.__class__.__name__}: {exc}', retryable=True)
            return False
        await mark_wechat_media_job_completed(job, owner_token)
        log.notice('WeChat queue completed account=%s message=%s: %s', account.name, job.message_id, saved)
        return True

    async def _fail_job(self, job: WeChatMediaJob, owner_token: str, *, error: str, retryable: bool) -> None:
        if not retryable or job.attempt_count >= self.cfg.max_download_attempts:
            await mark_wechat_media_job_discarded(job, owner_token, error=error)
            log.warning(
                'WeChat queue discarded account=%s message=%s after %d attempt(s): %s',
                job.account_name,
                job.message_id,
                job.attempt_count,
                error,
            )
            return
        delay = wechat_media_retry_delay(job.attempt_count)
        await mark_wechat_media_job_retry(job, owner_token, error=error, delay_seconds=delay)
        log.warning('WeChat queue retry account=%s message=%s in %.0fs: %s', job.account_name, job.message_id, delay, error)

    async def _consume_account_queue(
        self,
        *,
        account: WeChatAccount,
        client: ILinkClient,
        owner_token: str,
        stop_event: asyncio.Event,
        drain_only: bool = False,
    ) -> None:
        wake_event = self._worker_wake_events.setdefault(account.name, asyncio.Event())
        delay_before_next_download = False
        while not stop_event.is_set():
            job = await claim_next_wechat_media_job(account.name, owner_token)
            if job is None:
                if drain_only:
                    return
                wake_event.clear()
                with contextlib.suppress(TimeoutError):
                    await asyncio.wait_for(wake_event.wait(), timeout=_QUEUE_IDLE_POLL_SECONDS)
                continue
            if delay_before_next_download:
                await self._sleep(self.cfg.download_delay_seconds, stop_event)
            downloaded = await self._process_job(account=account, client=client, job=job, owner_token=owner_token)
            delay_before_next_download = downloaded

    # ---- lifecycle ---------------------------------------------------------

    async def _sleep(self, seconds: float, stop_event: asyncio.Event) -> None:
        if seconds <= 0:
            return
        stopped = asyncio.create_task(stop_event.wait())
        closed = asyncio.create_task(self._close_event.wait())
        try:
            await asyncio.wait({stopped, closed}, timeout=seconds, return_when=asyncio.FIRST_COMPLETED)
        finally:
            for task in (stopped, closed):
                task.cancel()
            await asyncio.gather(stopped, closed, return_exceptions=True)

    @staticmethod
    def _account_lock_name(account: WeChatAccount) -> str:
        return f'wechat-account:{account.name}'

    async def _run_account_once(self, account: WeChatAccount, stop_event: asyncio.Event) -> None:
        owner_token = uuid4().hex
        client = self._client_factory(account)
        tasks: list[asyncio.Task[None]] = []
        try:
            recovered_count = await reset_processing_wechat_media_jobs(account.name)
            if recovered_count:
                log.warning('Recovered %d WeChat queue jobs for account %s', recovered_count, account.name)
            self._clients[account.name] = client
            self._account_ready_events.setdefault(account.name, asyncio.Event()).set()
            tasks.append(
                asyncio.create_task(
                    self._consume_account_queue(account=account, client=client, owner_token=owner_token, stop_event=stop_event),
                    name=f'wechat-worker-{account.name}',
                ),
            )
            tasks.append(asyncio.create_task(self._poll_account(account, client, stop_event), name=f'wechat-poller-{account.name}'))
            log.notice('WeChat listener started for account %s', account.name)
            stopped = asyncio.create_task(stop_event.wait(), name=f'wechat-stop-{account.name}')
            closed = asyncio.create_task(self._close_event.wait(), name=f'wechat-close-{account.name}')
            done, _pending = await asyncio.wait({*tasks, stopped, closed}, return_when=asyncio.FIRST_COMPLETED)
            for waiter in (stopped, closed):
                if waiter not in done:
                    waiter.cancel()
            await asyncio.gather(stopped, closed, return_exceptions=True)
            for task in tasks:
                if task in done:
                    # A listener task that returns or raises is a failure either
                    # way; surface it so the outer loop reconnects.
                    await task
                    msg = f'WeChat listener task {task.get_name()} exited unexpectedly'
                    raise RuntimeError(msg)
        finally:
            self._account_ready_events.setdefault(account.name, asyncio.Event()).clear()
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            self._clients.pop(account.name, None)
            with contextlib.suppress(Exception):
                await client.aclose()
            log.info('WeChat listener stopped for account %s', account.name)

    async def _run_account_forever(self, account: WeChatAccount, stop_event: asyncio.Event) -> None:
        reconnect_delay = _RECONNECT_INITIAL_SECONDS
        while not stop_event.is_set() and not self._close_event.is_set():
            async with database.advisory_lock(self._account_lock_name(account)) as acquired:
                if not acquired:
                    log.warning('WeChat account lock is held for account %s; retry in %.0fs', account.name, reconnect_delay)
                else:
                    try:
                        await self._run_account_once(account, stop_event)
                        reconnect_delay = _RECONNECT_INITIAL_SECONDS
                    except asyncio.CancelledError:
                        raise
                    except Exception as exc:  # noqa: BLE001
                        log.warning('WeChat listener for account %s failed; restart in %.0fs: %s', account.name, reconnect_delay, exc)
            if stop_event.is_set() or self._close_event.is_set():
                return
            await self._sleep(reconnect_delay, stop_event)
            reconnect_delay = min(reconnect_delay * 2, _RECONNECT_MAX_SECONDS)

    async def run(self, stop_event: asyncio.Event) -> None:
        """Run one long-poll listener and one serial download worker per logged-in account."""
        if self._running:
            msg = 'WeChat runtime is already running'
            raise RuntimeError(msg)
        self._running = True
        await self._initialize_tables()
        try:
            async with asyncio.TaskGroup() as task_group:
                for account in self.cfg.resolved_accounts():
                    task_group.create_task(self._run_account_forever(account, stop_event), name=f'wechat-listener-{account.name}')
        finally:
            self._running = False

    async def wait_until_ready(self) -> None:
        await asyncio.gather(
            *(self._account_ready_events.setdefault(account.name, asyncio.Event()).wait() for account in self.cfg.resolved_accounts()),
        )

    async def update(self) -> None:
        """Cron and 立即运行 entry point.

        With the listener running there is nothing to fetch -- messages arrive
        as they are sent -- so this re-releases every backed-off download.
        Without it (``--trigger wechat``) it polls once and drains the queue.
        """
        await self._initialize_tables()
        if self._running:
            for account in self.cfg.resolved_accounts():
                released = await release_pending_wechat_media_jobs(account.name)
                pending = await count_pending_wechat_media_jobs(account.name)
                self._worker_wake_events.setdefault(account.name, asyncio.Event()).set()
                log.info('WeChat account %s: released %d backed-off download(s), %d pending', account.name, released, pending)
            return
        for account in self.cfg.resolved_accounts():
            async with database.advisory_lock(self._account_lock_name(account)) as acquired:
                if not acquired:
                    log.warning('WeChat account %s is already running; skip one-shot poll', account.name)
                    continue
                await self._run_one_shot_account(account)

    async def _run_one_shot_account(self, account: WeChatAccount) -> None:
        owner_token = uuid4().hex
        client = self._client_factory(account)
        stop_event = asyncio.Event()
        try:
            await reset_processing_wechat_media_jobs(account.name)
            state = await self.get_account_state(account.name)
            if state.pause_remaining_seconds > 0:
                log.warning(
                    'WeChat account %s is paused for %.0fs more (%s); downloading what is already queued',
                    account.name,
                    state.pause_remaining_seconds,
                    state.last_error,
                )
            else:
                try:
                    await self._poll_once(
                        account, client, timeout_seconds=min(self.cfg.long_poll_timeout_seconds, _ONE_SHOT_POLL_TIMEOUT_SECONDS)
                    )
                except ILinkError as exc:
                    if not exc.session_expired:
                        raise
                    await self._handle_session_expired(account, exc)
            await self._consume_account_queue(
                account=account, client=client, owner_token=owner_token, stop_event=stop_event, drain_only=True
            )
        finally:
            await client.aclose()
