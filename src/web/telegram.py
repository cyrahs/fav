import asyncio
import shutil
import tempfile
from collections import defaultdict
from pathlib import Path

from telethon import TelegramClient
from telethon.tl.types import Channel, DocumentAttributeVideo, Message, PeerChannel
from tqdm import tqdm

from src.core import config, logger
from src.tool import Notifier, cloudflare, format_video_filename, sanitize

log = logger.get('telegram')
cfg = config.web.telegram


class Telegram:
    def __init__(self, notifier: Notifier | None = None) -> None:
        self.notifier = notifier
        self._tmp_dir = tempfile.TemporaryDirectory(prefix='fav-telegram-')
        self.cache_dir = Path(self._tmp_dir.name)
        self.client = TelegramClient(cfg.session_path, cfg.api_id, cfg.api_hash)

    def __del__(self) -> None:
        self._tmp_dir.cleanup()

    async def aclose(self) -> None:
        if self.client.is_connected():
            await self.client.disconnect()
        self._tmp_dir.cleanup()

    async def _notify_download(self, *, channel_name: str, message_id: int, title: str, saved_path: Path) -> None:
        notifier = getattr(self, 'notifier', None)
        if notifier is None:
            return

        message = f'Telegram download completed\nChannel: {channel_name}\nTitle: {title}\nMessage ID: {message_id}\nPath: {saved_path}'

        try:
            await notifier.send(message)
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to send telegram download notification for message %s: %s', message_id, exc)

    @staticmethod
    async def get_downloaded_ids(channel_id: int) -> list[int]:
        exists_ids = await cloudflare.query_d1('SELECT message_id FROM telegram WHERE channel_id = ?;', (str(channel_id),))
        return [int(i['message_id']) for i in exists_ids]

    @staticmethod
    def _is_video_message(msg: Message) -> bool:
        if getattr(msg, 'video', None):
            return True
        attrs = getattr(getattr(msg, 'document', None), 'attributes', None)
        if not attrs:
            return False
        return any(isinstance(attr, DocumentAttributeVideo) for attr in attrs)

    async def _collect_videos_and_group_captions(self, channel: Channel) -> tuple[list[Message], dict[int, str]]:
        videos: list[Message] = []
        group_captions: dict[int, str] = {}

        async for msg in self.client.iter_messages(channel, reverse=True):
            if self._is_video_message(msg):
                videos.append(msg)

            grouped_id = getattr(msg, 'grouped_id', None)
            caption = (msg.message or '').strip()
            if grouped_id and caption and grouped_id not in group_captions:
                group_captions[grouped_id] = caption

        return videos, group_captions

    @staticmethod
    def _group_videos(videos: list[Message]) -> dict[int, list[Message]]:
        video_groups: dict[int, list[Message]] = defaultdict(list)
        for msg in videos:
            grouped_id = getattr(msg, 'grouped_id', None)
            if grouped_id:
                video_groups[grouped_id].append(msg)
        return video_groups

    @staticmethod
    def _build_group_entries(group_videos: list[Message], group_caption: str | None) -> list[dict[str, Message | str]]:
        if not group_caption:
            return [{'msg': video_msg, 'filename': f'video_{video_msg.id}'} for video_msg in group_videos]
        if len(group_videos) == 1:
            return [{'msg': group_videos[0], 'filename': group_caption}]
        return [{'msg': video_msg, 'filename': f'{group_caption}-{idx}'} for idx, video_msg in enumerate(group_videos, start=1)]

    @staticmethod
    def _build_standalone_entry(msg: Message) -> dict[str, Message | str]:
        caption = (msg.message or '').strip()
        filename = caption or f'video_{msg.id}'
        return {'msg': msg, 'filename': filename}

    async def get_videos(self, channel: Channel) -> list[dict[str, Message | str]]:
        """Get all video messages with pre-calculated filenames.

        Returns:
            List of dicts with 'msg' (Message) and 'filename' (str - base title without extension/ID)
        """
        videos, group_captions = await self._collect_videos_and_group_captions(channel)
        video_groups = self._group_videos(videos)
        result: list[dict[str, Message | str]] = []
        processed_group_ids: set[int] = set()

        for msg in videos:
            grouped_id = getattr(msg, 'grouped_id', None)
            if not grouped_id:
                result.append(self._build_standalone_entry(msg))
                continue
            if grouped_id in processed_group_ids:
                continue
            group_videos = video_groups[grouped_id]
            group_caption = group_captions.get(grouped_id)
            result.extend(self._build_group_entries(group_videos, group_caption))
            processed_group_ids.add(grouped_id)

        return result

    async def download(self, msg: Message, dst_dir: Path, title: str) -> Path | None:
        """Download a video message with specified title."""
        if await asyncio.to_thread(dst_dir.is_file):
            error_msg = f'{dst_dir} is a file'
            raise ValueError(error_msg)

        display_title = f'{sanitize(title, max_bytes=50)} [{msg.id}]'
        with tqdm(total=0, unit='B', unit_scale=True, desc=display_title, dynamic_ncols=True) as pbar:
            tmp_path = self.cache_dir / f'{msg.id}'

            def _cb(current: int, total: int) -> None:
                pbar.total = total
                pbar.update(current - pbar.n)

            downloaded_path = await msg.download_media(file=str(tmp_path), progress_callback=_cb)
        if downloaded_path:
            downloaded_path = Path(downloaded_path)
            filename = format_video_filename(
                title=title,
                video_id=str(msg.id),
                uploader=None,
                ext=downloaded_path.suffix,
            )
            dst_path = dst_dir / filename
            await asyncio.to_thread(dst_dir.mkdir, parents=True, exist_ok=True)
            shutil.move(downloaded_path, dst_path)
            return dst_path
        return None

    async def update_channel(self, channel_id: int) -> None:
        channel = await self.client.get_entity(PeerChannel(channel_id))
        ch_name = getattr(channel, 'username', None) or getattr(channel, 'title', str(channel_id)) or str(channel_id)
        ch_name = sanitize(ch_name)
        dst = cfg.path / ch_name
        dst.mkdir(parents=True, exist_ok=True)

        video_list = await self.get_videos(channel)
        downloaded_ids = await self.get_downloaded_ids(channel_id)

        # Filter out already downloaded videos
        undownloaded = [v for v in video_list if v['msg'].id not in downloaded_ids]

        if not undownloaded:
            log.info('No new videos')
            return

        total_videos = len(undownloaded)

        for idx, video_data in enumerate(undownloaded, start=1):
            msg = video_data['msg']
            filename = video_data['filename']

            log.info('Downloading videos from %s (%d/%d)', ch_name, idx, total_videos)

            result = await self.download(msg, dst, filename)
            if result:
                log.notice('Saved %s', result.name)
                await cloudflare.query_d1(
                    'INSERT INTO telegram (message_id, channel_id, title, channel_name) VALUES (?, ?, ?, ?);',
                    (str(msg.id), str(channel_id), filename, ch_name),
                )
                await self._notify_download(
                    channel_name=ch_name,
                    message_id=msg.id,
                    title=filename,
                    saved_path=result,
                )
            else:
                log.error('Failed to download message %s', msg.id)

    async def update(self) -> None:
        # Initialize table
        await cloudflare.query_d1("""
            CREATE TABLE IF NOT EXISTS telegram (
                message_id INTEGER PRIMARY KEY,
                channel_id INTEGER NOT NULL,
                title TEXT NOT NULL,
                channel_name TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
        """)
        log.debug('telegram table initialized')

        await self.client.start()
        for channel_id in cfg.channels:
            await self.update_channel(channel_id)
        await self.client.disconnect()
