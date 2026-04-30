import asyncio
import shutil
import tempfile
from collections import defaultdict
from dataclasses import dataclass, replace
from pathlib import Path

from telethon import TelegramClient
from telethon.tl.types import Channel, DocumentAttributeVideo, Message, MessageMediaDocument, MessageMediaPhoto, PeerChannel
from tqdm import tqdm

from src.core import config, logger
from src.core.config import TelegramAccount, TelegramChannel, TelegramMediaType
from src.tool import database, format_media_filename, sanitize
from src.tool.notifications import enqueue_notification

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
"""
_TELETHON_SQLITE_SESSION_SUFFIX = '.session'


@dataclass(frozen=True, slots=True)
class TelegramMediaEntry:
    msg: Message
    filename: str
    media_type: TelegramMediaType


class Telegram:
    def __init__(self) -> None:
        self._tmp_dir = tempfile.TemporaryDirectory(prefix='fav-telegram-')
        self.cache_dir = Path(self._tmp_dir.name)
        self.client: TelegramClient | None = None

    def __del__(self) -> None:
        self._tmp_dir.cleanup()

    async def aclose(self) -> None:
        if self.client is not None and self.client.is_connected():
            await self.client.disconnect()
            self.client = None
        self._tmp_dir.cleanup()

    async def _notify_download(
        self,
        *,
        account_name: str,
        channel_name: str,
        message_id: int,
        title: str,
        saved_path: Path,
    ) -> None:
        try:
            await enqueue_notification(
                kind='download_completed',
                source='telegram',
                title=f'Telegram: {title}',
                body=f'Channel {channel_name} | Message ID {message_id}',
                payload={
                    'account_name': account_name,
                    'channel_name': channel_name,
                    'message_id': message_id,
                    'saved_path': str(saved_path),
                },
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue telegram download notification for message %s: %s', message_id, exc)

    @staticmethod
    async def get_downloaded_ids(account_name: str, channel_id: int) -> list[int]:
        exists_ids = await database.query_db(
            'SELECT message_id FROM telegram WHERE account_name = ? AND channel_id = ?;',
            (account_name, channel_id),
        )
        return [int(i['message_id']) for i in exists_ids]

    @staticmethod
    def _document_attrs(msg: Message) -> list[object]:
        return list(getattr(getattr(msg, 'document', None), 'attributes', None) or [])

    @staticmethod
    def _is_video_message(msg: Message) -> bool:
        if getattr(msg, 'video', None):
            return True
        attrs = Telegram._document_attrs(msg)
        return any(isinstance(attr, DocumentAttributeVideo) for attr in attrs)

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

    async def _collect_media_and_group_captions(
        self,
        channel: Channel,
        media_types: list[TelegramMediaType],
    ) -> tuple[list[TelegramMediaEntry], dict[int, str]]:
        media: list[TelegramMediaEntry] = []
        group_captions: dict[int, str] = {}
        enabled_types = set(media_types)

        if self.client is None:
            msg = 'telegram client is not connected'
            raise RuntimeError(msg)

        async for msg in self.client.iter_messages(channel, reverse=True):
            media_type = self._message_media_type(msg, enabled_types)
            if media_type is not None:
                media.append(TelegramMediaEntry(msg=msg, filename='', media_type=media_type))

            grouped_id = getattr(msg, 'grouped_id', None)
            caption = (msg.message or '').strip()
            if grouped_id and caption and grouped_id not in group_captions:
                group_captions[grouped_id] = caption

        return media, group_captions

    @staticmethod
    def _group_media(media: list[TelegramMediaEntry]) -> dict[int, list[TelegramMediaEntry]]:
        media_groups: dict[int, list[TelegramMediaEntry]] = defaultdict(list)
        for item in media:
            grouped_id = getattr(item.msg, 'grouped_id', None)
            if grouped_id:
                media_groups[grouped_id].append(item)
        return media_groups

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
        caption = (item.msg.message or '').strip()
        filename = caption or Telegram._fallback_title(item)
        return replace(item, filename=filename)

    async def get_media(self, channel: Channel, media_types: list[TelegramMediaType]) -> list[TelegramMediaEntry]:
        """Get all configured media messages with pre-calculated filenames.

        Returns:
            List of media entries with Message, media type, and base title without extension/ID.
        """
        media, group_captions = await self._collect_media_and_group_captions(channel, media_types)
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
            group_media = media_groups[grouped_id]
            group_caption = group_captions.get(grouped_id)
            result.extend(self._build_group_entries(group_media, group_caption))
            processed_group_ids.add(grouped_id)

        return result

    async def get_videos(self, channel: Channel) -> list[dict[str, Message | str]]:
        """Get all video messages with pre-calculated filenames."""
        media = await self.get_media(channel, ['video'])
        return [{'msg': item.msg, 'filename': item.filename} for item in media]

    async def download(self, msg: Message, dst_dir: Path, title: str, media_type: TelegramMediaType = 'video') -> Path | None:
        """Download a media message with specified title."""
        if await asyncio.to_thread(dst_dir.is_file):
            error_msg = f'{dst_dir} is a file'
            raise ValueError(error_msg)

        display_title = f'{sanitize(title, max_bytes=50)} [{msg.id}]'
        with tqdm(total=0, unit='B', unit_scale=True, desc=display_title, dynamic_ncols=True) as pbar:
            tmp_path = self.cache_dir / f'{media_type}_{msg.id}'

            def _cb(current: int, total: int) -> None:
                pbar.total = total
                pbar.update(current - pbar.n)

            downloaded_path = await msg.download_media(file=str(tmp_path), progress_callback=_cb)
        if downloaded_path:
            downloaded_path = Path(downloaded_path)
            filename = format_media_filename(
                title=title,
                media_id=str(msg.id),
                uploader=None,
                ext=downloaded_path.suffix,
            )
            dst_path = dst_dir / filename
            await asyncio.to_thread(dst_dir.mkdir, parents=True, exist_ok=True)
            shutil.move(downloaded_path, dst_path)
            return dst_path
        return None

    async def update_channel(self, channel_cfg: TelegramChannel, account: TelegramAccount | None = None) -> None:
        if self.client is None:
            msg = 'telegram client is not connected'
            raise RuntimeError(msg)
        if account is None:
            account = cfg.resolved_accounts()[0]

        channel_id = channel_cfg.id
        channel = await self.client.get_entity(PeerChannel(channel_id))
        ch_name = getattr(channel, 'username', None) or getattr(channel, 'title', str(channel_id)) or str(channel_id)
        ch_name = sanitize(ch_name)
        dst = channel_cfg.path
        dst.mkdir(parents=True, exist_ok=True)

        media_list = await self.get_media(channel, channel_cfg.media_types)
        downloaded_ids = set(await self.get_downloaded_ids(account.name, channel_id))

        undownloaded = [item for item in media_list if item.msg.id not in downloaded_ids]

        if not undownloaded:
            log.info('No new media')
            return

        total_media = len(undownloaded)

        for idx, item in enumerate(undownloaded, start=1):
            msg = item.msg
            filename = item.filename

            log.info('Downloading %s from %s (%d/%d)', item.media_type, ch_name, idx, total_media)

            result = await self.download(msg, dst, filename, media_type=item.media_type)
            if result:
                log.notice('Saved %s', result.name)
                await database.query_db(
                    """
                    INSERT INTO telegram (account_name, message_id, channel_id, title, channel_name, media_type)
                    VALUES (?, ?, ?, ?, ?, ?);
                    """,
                    (account.name, msg.id, channel_id, filename, ch_name, item.media_type),
                )
                await self._notify_download(
                    account_name=account.name,
                    channel_name=ch_name,
                    message_id=msg.id,
                    title=filename,
                    saved_path=result,
                )
            else:
                log.error('Failed to download message %s', msg.id)

    async def update_account(self, account: TelegramAccount) -> None:
        log.info('Updating telegram account %s', account.name)
        lock_name = self._account_lock_name(account)
        async with database.advisory_lock(lock_name) as acquired:
            if not acquired:
                log.warning('Telegram account %s is already running, skip this account', account.name)
                return
            await self._update_account_locked(account)

    async def update(self) -> None:
        await database.query_db_multi(_TELEGRAM_SCHEMA_SQL)
        log.debug('telegram table initialized')

        for account in cfg.resolved_accounts():
            await self.update_account(account)

    @staticmethod
    def _account_lock_name(account: TelegramAccount) -> str:
        return f'telegram-session:{Telegram._effective_session_file(account.session_path)}'

    @staticmethod
    def _effective_session_file(session_path: Path) -> Path:
        path_text = str(session_path)
        if not path_text.endswith(_TELETHON_SQLITE_SESSION_SUFFIX):
            path_text = f'{path_text}{_TELETHON_SQLITE_SESSION_SUFFIX}'
        return Path(path_text).expanduser().resolve()

    async def _update_account_locked(self, account: TelegramAccount) -> None:
        self.client = TelegramClient(account.session_path, account.api_id, account.api_hash)
        try:
            await self.client.connect()
            if not await self.client.is_user_authorized():
                raise TelegramSessionUnauthorizedError(account_name=account.name, session_path=account.session_path)
            for channel in account.channels:
                await self.update_channel(channel, account)
        finally:
            if self.client is not None:
                await self.client.disconnect()
                self.client = None


class TelegramSessionUnauthorizedError(RuntimeError):
    def __init__(self, *, account_name: str, session_path: Path) -> None:
        msg = f'Telegram session unauthorized: account={account_name}, session_path={session_path}'
        super().__init__(msg)
