import asyncio
import contextlib
import shutil
import tempfile
from collections import defaultdict
from dataclasses import dataclass, replace
from pathlib import Path
from uuid import uuid4

from telethon import TelegramClient, events
from telethon.errors import FloodWaitError, MessageIdInvalidError, RPCError
from telethon.tl.types import Channel, DocumentAttributeVideo, Message, MessageMediaDocument, MessageMediaPhoto, PeerChannel
from tqdm import tqdm

from src.core import config, logger
from src.core.config import TelegramAccount, TelegramChannel, TelegramMediaType
from src.tool import database, format_media_filename, sanitize
from src.tool.notifications import enqueue_notification
from src.tool.telegram_queue import (
    TelegramMediaJob,
    claim_next_telegram_media_job,
    enqueue_telegram_media_job,
    ensure_telegram_media_queue_table,
    mark_telegram_media_job_completed,
    mark_telegram_media_job_discarded,
    mark_telegram_media_job_retry,
    reset_processing_telegram_media_jobs,
    telegram_media_retry_delay,
)

log = logger.get('telegram')
cfg = config.web.telegram

_TELEGRAM_SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS telegram (
    account_name TEXT NOT NULL,
    message_id BIGINT NOT NULL,
    channel_id BIGINT NOT NULL,
    title TEXT NOT NULL,
    channel_name TEXT NOT NULL,
    media_type TEXT NOT NULL DEFAULT 'video',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (account_name, channel_id, message_id)
);
ALTER TABLE telegram ADD COLUMN IF NOT EXISTS media_type TEXT NOT NULL DEFAULT 'video';

CREATE TABLE IF NOT EXISTS telegram_channel_state (
    account_name TEXT NOT NULL,
    channel_id BIGINT NOT NULL,
    last_scanned_message_id BIGINT NOT NULL DEFAULT 0,
    last_download_at TIMESTAMPTZ NULL,
    cooldown_until TIMESTAMPTZ NULL,
    last_error TEXT NOT NULL DEFAULT '',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (account_name, channel_id)
);

ALTER TABLE telegram_channel_state ADD COLUMN IF NOT EXISTS last_download_at TIMESTAMPTZ NULL;
ALTER TABLE telegram_channel_state ADD COLUMN IF NOT EXISTS cooldown_until TIMESTAMPTZ NULL;
ALTER TABLE telegram_channel_state ADD COLUMN IF NOT EXISTS last_error TEXT NOT NULL DEFAULT '';
ALTER TABLE telegram_channel_state ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP;
"""
_TELETHON_SQLITE_SESSION_SUFFIX = '.session'
_ALBUM_SETTLE_SECONDS = 2.0
_QUEUE_IDLE_POLL_SECONDS = 1.0
_RECONNECT_INITIAL_SECONDS = 5.0
_RECONNECT_MAX_SECONDS = 300.0


@dataclass(frozen=True, slots=True)
class TelegramMediaEntry:
    msg: Message
    filename: str
    media_type: TelegramMediaType


@dataclass(frozen=True, slots=True)
class TelegramChannelScan:
    media: list[TelegramMediaEntry]
    max_message_id: int
    scanned_message_count: int


@dataclass(frozen=True, slots=True)
class TelegramChannelState:
    last_scanned_message_id: int
    has_scan_state: bool
    cooldown_remaining_seconds: float = 0.0


class Telegram:
    def __init__(self) -> None:
        self._tmp_dir = tempfile.TemporaryDirectory(prefix='fav-telegram-')
        self.cache_dir = Path(self._tmp_dir.name)
        self.client: TelegramClient | None = None
        self._clients: dict[str, TelegramClient] = {}
        self._worker_wake_events: dict[str, asyncio.Event] = {}
        self._account_ready_events: dict[str, asyncio.Event] = {}
        self._reconciliation_locks: dict[str, asyncio.Lock] = {}
        self._close_event = asyncio.Event()
        self._running = False
        self._closed = False

    def __del__(self) -> None:
        with contextlib.suppress(Exception):
            self._tmp_dir.cleanup()

    async def aclose(self) -> None:
        if self._closed:
            return
        self._closed = True
        self._close_event.set()
        clients = list(self._clients.values())
        if self.client is not None and self.client not in clients:
            clients.append(self.client)
        for client in clients:
            with contextlib.suppress(Exception):
                if client.is_connected():
                    await client.disconnect()
        self._clients.clear()
        self.client = None
        self._tmp_dir.cleanup()

    async def _initialize_tables(self) -> None:
        await database.query_db_multi(_TELEGRAM_SCHEMA_SQL)
        await ensure_telegram_media_queue_table()
        log.debug('Telegram archive, channel state, and media queue tables initialized')

    async def _notify_download(  # noqa: PLR0913
        self,
        *,
        account_name: str,
        channel_name: str,
        message_id: int,
        title: str,
        media_type: TelegramMediaType,
        saved_path: Path,
    ) -> None:
        payload = {
            'account_name': account_name,
            'channel_name': channel_name,
            'message_id': message_id,
            'saved_path': str(saved_path),
        }
        if media_type == 'image':
            payload['image_path'] = str(saved_path)
        try:
            await enqueue_notification(
                kind='download_completed',
                source='telegram',
                title=f'Telegram: {title}',
                body=f'Channel {channel_name} | Message ID {message_id}',
                payload=payload,
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue telegram download notification for message %s: %s', message_id, exc)

    @staticmethod
    async def get_downloaded_ids(account_name: str, channel_id: int, *, min_message_id: int = 0) -> list[int]:
        rows = await database.query_db(
            'SELECT message_id FROM telegram WHERE account_name = ? AND channel_id = ? AND message_id > ?;',
            (account_name, channel_id, min_message_id),
        )
        return [int(row['message_id']) for row in rows]

    @staticmethod
    async def is_downloaded(account_name: str, channel_id: int, message_id: int) -> bool:
        rows = await database.query_db(
            'SELECT 1 FROM telegram WHERE account_name = ? AND channel_id = ? AND message_id = ? LIMIT 1;',
            (account_name, channel_id, message_id),
        )
        return bool(rows)

    @staticmethod
    async def get_latest_downloaded_id(account_name: str, channel_id: int) -> int:
        rows = await database.query_db(
            'SELECT COALESCE(MAX(message_id), 0) AS message_id FROM telegram WHERE account_name = ? AND channel_id = ?;',
            (account_name, channel_id),
        )
        if not rows:
            return 0
        return int(rows[0]['message_id'] or 0)

    async def get_channel_state(self, account_name: str, channel_id: int) -> TelegramChannelState:
        rows = await database.query_db(
            """
            SELECT
                last_scanned_message_id,
                CASE
                    WHEN cooldown_until IS NULL OR cooldown_until <= CURRENT_TIMESTAMP THEN 0
                    ELSE EXTRACT(EPOCH FROM (cooldown_until - CURRENT_TIMESTAMP))
                END AS cooldown_remaining_seconds
            FROM telegram_channel_state
            WHERE account_name = ? AND channel_id = ?;
            """,
            (account_name, channel_id),
        )
        if rows:
            row = rows[0]
            last_scanned_message_id = int(row['last_scanned_message_id'] or 0)
            if last_scanned_message_id <= 0:
                last_scanned_message_id = await self.get_latest_downloaded_id(account_name, channel_id)
            return TelegramChannelState(
                last_scanned_message_id=last_scanned_message_id,
                has_scan_state=True,
                cooldown_remaining_seconds=float(row['cooldown_remaining_seconds'] or 0),
            )
        latest_downloaded_id = await self.get_latest_downloaded_id(account_name, channel_id)
        return TelegramChannelState(last_scanned_message_id=latest_downloaded_id, has_scan_state=False)

    @staticmethod
    async def set_channel_last_scanned_message_id(account_name: str, channel_id: int, message_id: int) -> None:
        if message_id <= 0:
            return
        await database.query_db(
            """
            INSERT INTO telegram_channel_state (account_name, channel_id, last_scanned_message_id)
            VALUES (?, ?, ?)
            ON CONFLICT (account_name, channel_id)
            DO UPDATE SET
                last_scanned_message_id = GREATEST(
                    telegram_channel_state.last_scanned_message_id,
                    EXCLUDED.last_scanned_message_id
                ),
                updated_at = CURRENT_TIMESTAMP;
            """,
            (account_name, channel_id, message_id),
        )

    @staticmethod
    async def set_channel_cooldown(account_name: str, channel_id: int, seconds: float, *, error: str = '') -> None:
        if seconds <= 0:
            return
        await database.query_db(
            """
            INSERT INTO telegram_channel_state (account_name, channel_id, cooldown_until, last_error)
            VALUES (?, ?, CURRENT_TIMESTAMP + (? * INTERVAL '1 second'), ?)
            ON CONFLICT (account_name, channel_id)
            DO UPDATE SET
                cooldown_until = GREATEST(
                    COALESCE(telegram_channel_state.cooldown_until, CURRENT_TIMESTAMP),
                    EXCLUDED.cooldown_until
                ),
                last_error = EXCLUDED.last_error,
                updated_at = CURRENT_TIMESTAMP;
            """,
            (account_name, channel_id, seconds, error[:500]),
        )

    @staticmethod
    async def mark_channel_downloaded(account_name: str, channel_id: int) -> None:
        await database.query_db(
            """
            INSERT INTO telegram_channel_state (account_name, channel_id, last_download_at)
            VALUES (?, ?, CURRENT_TIMESTAMP)
            ON CONFLICT (account_name, channel_id)
            DO UPDATE SET
                last_download_at = CURRENT_TIMESTAMP,
                updated_at = CURRENT_TIMESTAMP;
            """,
            (account_name, channel_id),
        )

    @staticmethod
    def _document_attrs(msg: Message) -> list[object]:
        return list(getattr(getattr(msg, 'document', None), 'attributes', None) or [])

    @staticmethod
    def _is_video_message(msg: Message) -> bool:
        if getattr(msg, 'video', None):
            return True
        return any(isinstance(attr, DocumentAttributeVideo) for attr in Telegram._document_attrs(msg))

    @staticmethod
    def _is_sticker_attribute(attr: object) -> bool:
        return attr.__class__.__name__ in {'DocumentAttributeSticker', 'DocumentAttributeCustomEmoji'}

    @staticmethod
    def _is_image_message(msg: Message) -> bool:
        if getattr(msg, 'sticker', None):
            return False
        if isinstance(getattr(msg, 'media', None), MessageMediaPhoto):
            return True
        media = getattr(msg, 'media', None)
        if not isinstance(media, MessageMediaDocument):
            return False
        document = getattr(media, 'document', None)
        mime_type = getattr(document, 'mime_type', '') or ''
        if not mime_type.startswith('image/'):
            return False
        attrs = list(getattr(document, 'attributes', None) or [])
        return not any(Telegram._is_sticker_attribute(attr) for attr in attrs)

    @staticmethod
    def _message_media_type(msg: Message, media_types: set[TelegramMediaType]) -> TelegramMediaType | None:
        if 'video' in media_types and Telegram._is_video_message(msg):
            return 'video'
        if 'image' in media_types and Telegram._is_image_message(msg):
            return 'image'
        return None

    @staticmethod
    def _message_id(msg: Message) -> int:
        return int(getattr(msg, 'id', 0) or 0)

    @staticmethod
    def _message_channel_id(msg: Message) -> int | None:
        peer_id = getattr(msg, 'peer_id', None)
        channel_id = getattr(peer_id, 'channel_id', None)
        if channel_id is None:
            return None
        return int(channel_id)

    async def _collect_channel_messages(  # noqa: PLR0913
        self,
        channel: Channel,
        *,
        client: TelegramClient | None = None,
        min_message_id: int = 0,
        limit: int | None = None,
        wait_time: float | None = None,
        newest_window: bool = False,
    ) -> list[Message]:
        selected_client = client or self.client
        if selected_client is None:
            msg = 'telegram client is not connected'
            raise RuntimeError(msg)
        iterator = selected_client.iter_messages(
            channel,
            limit=limit,
            min_id=min_message_id,
            reverse=not newest_window,
            wait_time=wait_time,
        )
        messages = [msg async for msg in iterator]
        if newest_window:
            messages.sort(key=self._message_id)
        return messages

    def _build_scan_from_messages(
        self,
        messages: list[Message],
        media_types: list[TelegramMediaType],
    ) -> TelegramChannelScan:
        media: list[TelegramMediaEntry] = []
        group_captions: dict[int, str] = {}
        enabled_types = set(media_types)
        for msg in messages:
            media_type = self._message_media_type(msg, enabled_types)
            if media_type is not None:
                media.append(TelegramMediaEntry(msg=msg, filename='', media_type=media_type))
            grouped_id = getattr(msg, 'grouped_id', None)
            caption = (getattr(msg, 'message', '') or '').strip()
            if grouped_id and caption and grouped_id not in group_captions:
                group_captions[grouped_id] = caption

        media_groups = self._group_media(media)
        result: list[TelegramMediaEntry] = []
        processed_group_ids: set[int] = set()
        for item in media:
            grouped_id = getattr(item.msg, 'grouped_id', None)
            if not grouped_id:
                result.append(self._build_standalone_entry(item))
                continue
            if grouped_id in processed_group_ids:
                continue
            result.extend(self._build_group_entries(media_groups[grouped_id], group_captions.get(grouped_id)))
            processed_group_ids.add(grouped_id)
        max_message_id = max((self._message_id(msg) for msg in messages), default=0)
        return TelegramChannelScan(media=result, max_message_id=max_message_id, scanned_message_count=len(messages))

    async def scan_media(  # noqa: PLR0913
        self,
        channel: Channel,
        media_types: list[TelegramMediaType],
        *,
        client: TelegramClient | None = None,
        min_message_id: int = 0,
        limit: int | None = None,
        wait_time: float | None = None,
        newest_window: bool = False,
    ) -> TelegramChannelScan:
        messages = await self._collect_channel_messages(
            channel,
            client=client,
            min_message_id=min_message_id,
            limit=limit,
            wait_time=wait_time,
            newest_window=newest_window,
        )
        return self._build_scan_from_messages(messages, media_types)

    @staticmethod
    def _group_media(media: list[TelegramMediaEntry]) -> dict[int, list[TelegramMediaEntry]]:
        groups: dict[int, list[TelegramMediaEntry]] = defaultdict(list)
        for item in media:
            grouped_id = getattr(item.msg, 'grouped_id', None)
            if grouped_id:
                groups[grouped_id].append(item)
        return groups

    @staticmethod
    def _fallback_title(item: TelegramMediaEntry) -> str:
        return f'{item.media_type}_{item.msg.id}'

    @staticmethod
    def _build_group_entries(group_media: list[TelegramMediaEntry], group_caption: str | None) -> list[TelegramMediaEntry]:
        if not group_caption:
            return [replace(item, filename=Telegram._fallback_title(item)) for item in group_media]
        if len(group_media) == 1:
            return [replace(group_media[0], filename=group_caption)]
        return [replace(item, filename=f'{group_caption}-{idx}') for idx, item in enumerate(group_media, start=1)]

    @staticmethod
    def _build_standalone_entry(item: TelegramMediaEntry) -> TelegramMediaEntry:
        caption = (getattr(item.msg, 'message', '') or '').strip()
        return replace(item, filename=caption or Telegram._fallback_title(item))

    async def get_media(self, channel: Channel, media_types: list[TelegramMediaType]) -> list[TelegramMediaEntry]:
        scan = await self.scan_media(channel, media_types)
        return scan.media

    async def get_videos(self, channel: Channel) -> list[dict[str, Message | str]]:
        media = await self.get_media(channel, ['video'])
        return [{'msg': item.msg, 'filename': item.filename} for item in media]

    async def download(  # noqa: PLR0913
        self,
        msg: Message,
        dst_dir: Path,
        title: str,
        media_type: TelegramMediaType = 'video',
        *,
        account_name: str = '',
        channel_id: int = 0,
    ) -> Path | None:
        if await asyncio.to_thread(dst_dir.is_file):
            error_msg = f'{dst_dir} is a file'
            raise ValueError(error_msg)
        display_title = f'{sanitize(title, max_bytes=50)} [{msg.id}]'
        safe_account = sanitize(account_name or 'account')
        with tqdm(total=0, unit='B', unit_scale=True, desc=display_title, dynamic_ncols=True) as pbar:
            tmp_path = self.cache_dir / f'{safe_account}_{channel_id}_{media_type}_{msg.id}'

            def _cb(current: int, total: int) -> None:
                pbar.total = total
                pbar.update(current - pbar.n)

            downloaded_path = await msg.download_media(file=str(tmp_path), progress_callback=_cb)
        if not downloaded_path:
            return None
        downloaded_path = Path(downloaded_path)
        filename = format_media_filename(
            title=title,
            media_id=str(msg.id),
            uploader=None,
            ext=downloaded_path.suffix,
        )
        dst_path = dst_dir / filename
        await asyncio.to_thread(dst_dir.mkdir, parents=True, exist_ok=True)
        await asyncio.to_thread(shutil.move, downloaded_path, dst_path)
        return dst_path

    @staticmethod
    async def _sleep(seconds: float) -> None:
        if seconds > 0:
            await asyncio.sleep(seconds)

    @staticmethod
    def _media_batches(media: list[TelegramMediaEntry]) -> list[list[TelegramMediaEntry]]:
        batches: list[list[TelegramMediaEntry]] = []
        group_indexes: dict[int, int] = {}
        for item in media:
            grouped_id = getattr(item.msg, 'grouped_id', None)
            if grouped_id:
                index = group_indexes.get(grouped_id)
                if index is None:
                    group_indexes[grouped_id] = len(batches)
                    batches.append([item])
                else:
                    batches[index].append(item)
            else:
                batches.append([item])
        return batches

    async def _enqueue_entries(
        self,
        *,
        account: TelegramAccount,
        channel_cfg: TelegramChannel,
        entries: list[TelegramMediaEntry],
        source: str,
        available_delay_seconds: float = 0,
    ) -> int:
        inserted = 0
        for item in entries:
            msg_id = self._message_id(item.msg)
            if msg_id <= 0:
                continue
            was_inserted = await enqueue_telegram_media_job(
                account_name=account.name,
                channel_id=channel_cfg.id,
                message_id=msg_id,
                grouped_id=getattr(item.msg, 'grouped_id', None),
                media_type=item.media_type,
                title=item.filename,
                source=source,
                available_delay_seconds=available_delay_seconds,
            )
            inserted += int(was_inserted)
        if entries:
            wake_event = self._worker_wake_events.get(account.name)
            if wake_event is not None:
                wake_event.set()
        return inserted

    async def update_channel(
        self,
        channel_cfg: TelegramChannel,
        account: TelegramAccount | None = None,
        *,
        client: TelegramClient | None = None,
    ) -> None:
        """Reconcile channel history into the durable queue without downloading."""
        if account is None:
            account = cfg.resolved_accounts()[0]
        selected_client = client or self._clients.get(account.name) or self.client
        if selected_client is None:
            msg = 'telegram client is not connected'
            raise RuntimeError(msg)

        channel_id = channel_cfg.id
        channel = await selected_client.get_entity(PeerChannel(channel_id))
        channel_name = getattr(channel, 'username', None) or getattr(channel, 'title', str(channel_id)) or str(channel_id)
        channel_name = sanitize(channel_name)
        state = await self.get_channel_state(account.name, channel_id)
        if state.cooldown_remaining_seconds > 0:
            log.info('Telegram channel %s is cooling down for %.0fs; skip reconciliation', channel_name, state.cooldown_remaining_seconds)
            return

        scan = await self.scan_media(
            channel,
            channel_cfg.media_types,
            client=selected_client,
            min_message_id=state.last_scanned_message_id,
            limit=cfg.scan_limit,
            wait_time=cfg.history_wait_seconds,
            newest_window=state.last_scanned_message_id == 0,
        )
        if scan.max_message_id <= state.last_scanned_message_id:
            log.info('No new Telegram messages to reconcile in %s', channel_name)
            return

        downloaded_ids = set(await self.get_downloaded_ids(account.name, channel_id, min_message_id=state.last_scanned_message_id))
        inserted_count = 0
        processed_cursor = state.last_scanned_message_id
        limit_reached = False
        for batch in self._media_batches(scan.media):
            batch_ids = [self._message_id(item.msg) for item in batch]
            pending_entries = [item for item in batch if self._message_id(item.msg) not in downloaded_ids]
            if pending_entries:
                if inserted_count >= cfg.download_limit_per_channel:
                    limit_reached = True
                    break
                inserted_count += await self._enqueue_entries(
                    account=account,
                    channel_cfg=channel_cfg,
                    entries=pending_entries,
                    source='reconciliation',
                )
            processed_cursor = max(processed_cursor, *batch_ids)

        if not limit_reached:
            processed_cursor = max(processed_cursor, scan.max_message_id)
        if processed_cursor > state.last_scanned_message_id:
            await self.set_channel_last_scanned_message_id(account.name, channel_id, processed_cursor)
        log.info(
            'Telegram reconciliation for %s scanned %d messages and queued %d new media%s',
            channel_name,
            scan.scanned_message_count,
            inserted_count,
            ' (per-run limit reached)' if limit_reached else '',
        )

    async def _reconcile_account(self, account: TelegramAccount, client: TelegramClient) -> None:
        lock = self._reconciliation_locks.setdefault(account.name, asyncio.Lock())
        async with lock:
            for channel in account.channels:
                try:
                    await self.update_channel(channel, account, client=client)
                except FloodWaitError as exc:
                    wait_seconds = float(getattr(exc, 'seconds', 0) or 0)
                    cooldown_seconds = max(wait_seconds, cfg.channel_cooldown_seconds)
                    await self.set_account_cooldown(
                        account,
                        cooldown_seconds,
                        error=f'FloodWaitError: {wait_seconds:.0f}s',
                    )
                    log.warning(
                        'Telegram account %s hit FloodWait during reconciliation; cooling down %.0fs',
                        account.name,
                        cooldown_seconds,
                    )
                    break
                except Exception as exc:
                    await self.set_channel_cooldown(
                        account.name,
                        channel.id,
                        cfg.channel_cooldown_seconds,
                        error=f'{exc.__class__.__name__}: {exc}',
                    )
                    raise

    async def _enqueue_event_messages(
        self,
        *,
        account: TelegramAccount,
        messages: list[Message],
        album: bool,
    ) -> None:
        if not messages:
            return
        channel_id = self._message_channel_id(messages[0])
        if channel_id is None:
            return
        channel_cfg = next((channel for channel in account.channels if channel.id == channel_id), None)
        if channel_cfg is None:
            return
        entries = self._build_scan_from_messages(messages, channel_cfg.media_types).media
        if not entries:
            return
        inserted = await self._enqueue_entries(
            account=account,
            channel_cfg=channel_cfg,
            entries=entries,
            source='event',
            available_delay_seconds=_ALBUM_SETTLE_SECONDS if album else 0,
        )
        log.info(
            'Telegram event queued %d/%d media for account=%s channel=%s%s',
            inserted,
            len(entries),
            account.name,
            channel_id,
            ' album' if album else '',
        )

    def _register_event_handlers(self, account: TelegramAccount, client: TelegramClient) -> None:
        async def _new_message_handler(event: object) -> None:
            message = getattr(event, 'message', None)
            if message is None or getattr(message, 'grouped_id', None):
                return
            try:
                await self._enqueue_event_messages(account=account, messages=[message], album=False)
            except Exception:
                log.exception('Failed to persist Telegram NewMessage event for account %s', account.name)

        async def _album_handler(event: object) -> None:
            messages = list(getattr(event, 'messages', None) or [])
            try:
                await self._enqueue_event_messages(account=account, messages=messages, album=True)
            except Exception:
                log.exception('Failed to persist Telegram Album event for account %s', account.name)

        client.add_event_handler(_new_message_handler, events.NewMessage())
        client.add_event_handler(_album_handler, events.Album())

    async def _archive_job(
        self,
        *,
        account: TelegramAccount,
        channel_cfg: TelegramChannel,
        channel_name: str,
        job: TelegramMediaJob,
        message: Message,
    ) -> Path | None:
        result = await self.download(
            message,
            channel_cfg.path,
            job.title,
            media_type=job.media_type,
            account_name=account.name,
            channel_id=job.channel_id,
        )
        if result is None:
            return None
        await database.query_db(
            """
            INSERT INTO telegram (account_name, message_id, channel_id, title, channel_name, media_type)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT (account_name, channel_id, message_id) DO NOTHING;
            """,
            (account.name, job.message_id, job.channel_id, job.title, channel_name, job.media_type),
        )
        await self.mark_channel_downloaded(account.name, job.channel_id)
        await self._notify_download(
            account_name=account.name,
            channel_name=channel_name,
            message_id=job.message_id,
            title=job.title,
            media_type=job.media_type,
            saved_path=result,
        )
        return result

    async def _process_job(  # noqa: C901, PLR0911
        self,
        *,
        account: TelegramAccount,
        client: TelegramClient,
        job: TelegramMediaJob,
        owner_token: str,
    ) -> bool:
        channel_cfg = next((channel for channel in account.channels if channel.id == job.channel_id), None)
        if channel_cfg is None:
            await mark_telegram_media_job_discarded(job, owner_token, error='Channel is no longer configured')
            return False
        if await self.is_downloaded(account.name, job.channel_id, job.message_id):
            await mark_telegram_media_job_completed(job, owner_token)
            log.info('Telegram queue completed already archived message %s/%s', job.channel_id, job.message_id)
            return False

        state = await self.get_channel_state(account.name, job.channel_id)
        if state.cooldown_remaining_seconds > 0:
            await mark_telegram_media_job_retry(
                job,
                owner_token,
                error='Channel cooldown is active',
                delay_seconds=state.cooldown_remaining_seconds,
            )
            return False

        try:
            channel = await client.get_entity(PeerChannel(job.channel_id))
            channel_name = getattr(channel, 'username', None) or getattr(channel, 'title', str(job.channel_id)) or str(job.channel_id)
            channel_name = sanitize(channel_name)
            message = await client.get_messages(PeerChannel(job.channel_id), ids=job.message_id)
            if message is None:
                await mark_telegram_media_job_discarded(job, owner_token, error='Message was deleted or is unavailable')
                return False
            current_media_type = self._message_media_type(message, set(channel_cfg.media_types))
            if current_media_type is None or current_media_type != job.media_type:
                await mark_telegram_media_job_discarded(job, owner_token, error='Configured media is no longer available')
                return False
            result = await self._archive_job(
                account=account,
                channel_cfg=channel_cfg,
                channel_name=channel_name,
                job=job,
                message=message,
            )
            if result is None:
                delay = telegram_media_retry_delay(job.attempt_count)
                await mark_telegram_media_job_retry(
                    job,
                    owner_token,
                    error='Telegram returned no downloaded media path',
                    delay_seconds=delay,
                )
                return False
        except asyncio.CancelledError:
            raise
        except MessageIdInvalidError as exc:
            await mark_telegram_media_job_discarded(job, owner_token, error=f'{exc.__class__.__name__}: {exc}')
            return False
        except FloodWaitError as exc:
            wait_seconds = float(getattr(exc, 'seconds', 0) or 0)
            cooldown_seconds = max(wait_seconds, cfg.channel_cooldown_seconds)
            await self.set_account_cooldown(account, cooldown_seconds, error=f'FloodWaitError: {wait_seconds:.0f}s')
            await mark_telegram_media_job_retry(
                job,
                owner_token,
                error=f'FloodWaitError: {wait_seconds:.0f}s',
                delay_seconds=cooldown_seconds,
            )
            log.warning('Telegram queue hit FloodWait for account %s; retry in %.0fs', account.name, cooldown_seconds)
            return False
        except (RPCError, OSError, RuntimeError) as exc:
            delay = telegram_media_retry_delay(job.attempt_count)
            await self.set_channel_cooldown(
                account.name,
                job.channel_id,
                min(delay, cfg.channel_cooldown_seconds) if cfg.channel_cooldown_seconds > 0 else delay,
                error=f'{exc.__class__.__name__}: {exc}',
            )
            await mark_telegram_media_job_retry(
                job,
                owner_token,
                error=f'{exc.__class__.__name__}: {exc}',
                delay_seconds=delay,
            )
            log.warning('Telegram queue retry account=%s message=%s in %.0fs: %s', account.name, job.message_id, delay, exc)
            return False
        except Exception as exc:
            delay = telegram_media_retry_delay(job.attempt_count)
            await mark_telegram_media_job_retry(
                job,
                owner_token,
                error=f'{exc.__class__.__name__}: {exc}',
                delay_seconds=delay,
            )
            log.exception('Unexpected Telegram queue error account=%s message=%s', account.name, job.message_id)
            return False

        await mark_telegram_media_job_completed(job, owner_token)
        log.notice('Telegram queue completed account=%s channel=%s message=%s: %s', account.name, job.channel_id, job.message_id, result)
        return True

    async def _consume_account_queue(
        self,
        *,
        account: TelegramAccount,
        client: TelegramClient,
        owner_token: str,
        stop_event: asyncio.Event,
        drain_only: bool = False,
    ) -> None:
        wake_event = self._worker_wake_events.setdefault(account.name, asyncio.Event())
        delay_before_next_download = False
        while not stop_event.is_set():
            job = await claim_next_telegram_media_job(account.name, owner_token)
            if job is None:
                if drain_only:
                    return
                wake_event.clear()
                with contextlib.suppress(TimeoutError):
                    await asyncio.wait_for(wake_event.wait(), timeout=_QUEUE_IDLE_POLL_SECONDS)
                continue
            if delay_before_next_download:
                await self._sleep(cfg.download_delay_seconds)
            downloaded = await self._process_job(account=account, client=client, job=job, owner_token=owner_token)
            delay_before_next_download = downloaded

    async def _wait_for_disconnect_or_stop(
        self,
        client: TelegramClient,
        stop_event: asyncio.Event,
        worker_task: asyncio.Task[None],
    ) -> None:
        disconnected = asyncio.ensure_future(client.disconnected)
        stopped = asyncio.create_task(stop_event.wait())
        closed = asyncio.create_task(self._close_event.wait())
        done, pending = await asyncio.wait({disconnected, stopped, closed, worker_task}, return_when=asyncio.FIRST_COMPLETED)
        for task in pending:
            if task is not worker_task:
                task.cancel()
        await asyncio.gather(*(task for task in pending if task is not worker_task), return_exceptions=True)
        for task in done:
            if task is disconnected or task is worker_task:
                await task

    async def _run_account_once(self, account: TelegramAccount, stop_event: asyncio.Event) -> None:
        owner_token = uuid4().hex
        client = TelegramClient(
            account.session_path,
            account.api_id,
            account.api_hash,
            flood_sleep_threshold=cfg.flood_sleep_threshold_seconds,
            receive_updates=True,
            catch_up=True,
        )
        self._register_event_handlers(account, client)
        worker_task: asyncio.Task[None] | None = None
        try:
            await client.connect()
            if not await client.is_user_authorized():
                raise TelegramSessionUnauthorizedError(account_name=account.name, session_path=account.session_path)
            self._clients[account.name] = client
            recovered_count = await reset_processing_telegram_media_jobs(account.name)
            if recovered_count:
                log.warning('Recovered %d Telegram queue jobs for account %s', recovered_count, account.name)
            self._account_ready_events.setdefault(account.name, asyncio.Event()).set()
            worker_task = asyncio.create_task(
                self._consume_account_queue(
                    account=account,
                    client=client,
                    owner_token=owner_token,
                    stop_event=stop_event,
                ),
                name=f'telegram-worker-{account.name}',
            )
            log.notice('Telegram listener connected for account %s', account.name)
            await self._wait_for_disconnect_or_stop(client, stop_event, worker_task)
        finally:
            self._account_ready_events.setdefault(account.name, asyncio.Event()).clear()
            if worker_task is not None:
                worker_task.cancel()
                await asyncio.gather(worker_task, return_exceptions=True)
            self._clients.pop(account.name, None)
            with contextlib.suppress(Exception):
                await client.disconnect()
            log.info('Telegram listener disconnected for account %s', account.name)

    async def _run_account_forever(self, account: TelegramAccount, stop_event: asyncio.Event) -> None:
        reconnect_delay = _RECONNECT_INITIAL_SECONDS
        while not stop_event.is_set() and not self._close_event.is_set():
            async with database.advisory_lock(self._account_lock_name(account)) as acquired:
                if not acquired:
                    log.warning('Telegram session lock is held for account %s; retry in %.0fs', account.name, reconnect_delay)
                else:
                    try:
                        await self._run_account_once(account, stop_event)
                        reconnect_delay = _RECONNECT_INITIAL_SECONDS
                    except TelegramSessionUnauthorizedError:
                        raise
                    except asyncio.CancelledError:
                        raise
                    except Exception as exc:  # noqa: BLE001
                        log.warning(
                            'Telegram listener for account %s failed; reconnect in %.0fs: %s',
                            account.name,
                            reconnect_delay,
                            exc,
                        )
            if stop_event.is_set() or self._close_event.is_set():
                return
            try:
                await asyncio.wait_for(stop_event.wait(), timeout=reconnect_delay)
            except TimeoutError:
                reconnect_delay = min(reconnect_delay * 2, _RECONNECT_MAX_SECONDS)

    async def run(self, stop_event: asyncio.Event) -> None:
        """Run persistent listeners and serial queue workers for all configured accounts."""
        if self._running:
            msg = 'Telegram runtime is already running'
            raise RuntimeError(msg)
        self._running = True
        await self._initialize_tables()
        try:
            async with asyncio.TaskGroup() as task_group:
                for account in cfg.resolved_accounts():
                    task_group.create_task(
                        self._run_account_forever(account, stop_event),
                        name=f'telegram-listener-{account.name}',
                    )
        finally:
            self._running = False

    async def wait_until_ready(self) -> None:
        await asyncio.gather(
            *(self._account_ready_events.setdefault(account.name, asyncio.Event()).wait() for account in cfg.resolved_accounts()),
        )

    async def reconcile(self) -> None:
        await self._initialize_tables()
        if not self._running:
            await self._run_one_shot()
            return
        tasks = []
        for account in cfg.resolved_accounts():
            client = self._clients.get(account.name)
            if client is None:
                log.warning('Telegram account %s is not connected; reconciliation deferred', account.name)
                continue
            tasks.append(self._reconcile_account(account, client))
        await asyncio.gather(*tasks)

    async def update(self) -> None:
        await self.reconcile()

    async def update_account(self, account: TelegramAccount) -> None:
        await self._initialize_tables()
        async with database.advisory_lock(self._account_lock_name(account)) as acquired:
            if not acquired:
                log.warning('Telegram account %s is already running; skip one-shot reconciliation', account.name)
                return
            await self._run_one_shot_account(account)

    async def _run_one_shot(self) -> None:
        for account in cfg.resolved_accounts():
            async with database.advisory_lock(self._account_lock_name(account)) as acquired:
                if not acquired:
                    log.warning('Telegram account %s is already running; skip one-shot reconciliation', account.name)
                    continue
                await self._run_one_shot_account(account)

    async def _run_one_shot_account(self, account: TelegramAccount) -> None:
        owner_token = uuid4().hex
        client = TelegramClient(
            account.session_path,
            account.api_id,
            account.api_hash,
            flood_sleep_threshold=cfg.flood_sleep_threshold_seconds,
            receive_updates=False,
        )
        self.client = client
        stop_event = asyncio.Event()
        try:
            await client.connect()
            if not await client.is_user_authorized():
                raise TelegramSessionUnauthorizedError(account_name=account.name, session_path=account.session_path)
            await reset_processing_telegram_media_jobs(account.name)
            await self._reconcile_account(account, client)
            await self._consume_account_queue(
                account=account,
                client=client,
                owner_token=owner_token,
                stop_event=stop_event,
                drain_only=True,
            )
        finally:
            await client.disconnect()
            self.client = None

    @staticmethod
    def _account_lock_name(account: TelegramAccount) -> str:
        return f'telegram-session:{Telegram._effective_session_file(account.session_path)}'

    @staticmethod
    def _effective_session_file(session_path: Path) -> Path:
        path_text = str(session_path)
        if not path_text.endswith(_TELETHON_SQLITE_SESSION_SUFFIX):
            path_text = f'{path_text}{_TELETHON_SQLITE_SESSION_SUFFIX}'
        return Path(path_text).expanduser().resolve()

    async def set_account_cooldown(self, account: TelegramAccount, seconds: float, *, error: str = '') -> None:
        for channel in account.channels:
            await self.set_channel_cooldown(account.name, channel.id, seconds, error=error)


class TelegramSessionUnauthorizedError(RuntimeError):
    def __init__(self, *, account_name: str, session_path: Path) -> None:
        msg = f'Telegram session unauthorized: account={account_name}, session_path={session_path}'
        super().__init__(msg)
