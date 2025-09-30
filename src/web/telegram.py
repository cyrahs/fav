from __future__ import annotations

import re
import shutil
import tempfile
from pathlib import Path

from pydantic_core.core_schema import NoneSchema
from telethon import TelegramClient
from telethon.tl.types import Channel, DocumentAttributeVideo, Message, PeerChannel
from tqdm import tqdm

from src.core import config, logger

log = logger.get('telegram')
cfg = config.telegram


INVALID_CHARS = r'[<>:"/\\|?*\n]'
MAX_FILENAME_BYTES = 200


class Telegram:
    def __init__(self) -> None:
        self._tmp_dir = tempfile.TemporaryDirectory(prefix='fav-telegram-')
        self.cache_dir = Path(self._tmp_dir.name)
        self.client = TelegramClient(cfg.session_path, cfg.api_id, cfg.api_hash)

    def __del__(self) -> None:
        self._tmp_dir.cleanup()

    @staticmethod
    def sanitize(name: str) -> str:
        base = re.sub(INVALID_CHARS, '_', name).strip()
        while base and len(base.encode('utf-8')) > MAX_FILENAME_BYTES:
            base = base[:-1]
        return base

    @staticmethod
    def get_downloaded_ids(dst: Path) -> list[int]:
        pattern = re.compile(r'.*?\[(\d+)\]\.[^.]+$')
        return [int(pattern.match(p.name).group(1)) for p in dst.glob('*')]

    async def get_videos(self, channel: Channel) -> list[Message]:
        videos = []
        async for msg in self.client.iter_messages(channel, reverse=True):
            # Filter to only video-like media
            is_video = False
            if getattr(msg, 'video', None):
                is_video = True
            elif getattr(msg, 'document', None) and getattr(msg.document, 'attributes', None):
                is_video = any(isinstance(attr, DocumentAttributeVideo) for attr in msg.document.attributes)
            if not is_video:
                continue
            videos.append(msg)
        return videos

    async def download(self, msg: Message, dst_dir: Path) -> Path | None:
        if not dst_dir.exists():
            dst_dir.mkdir(parents=True, exist_ok=True)
        elif dst_dir.is_file():
            msg = f'{dst_dir} is a file'
            raise ValueError(msg)
        title = (msg.message or '').strip() or f'video_{msg.id}'
        title = self.sanitize(title)
        title = f'{title} [{msg.id}]'
        with tqdm(total=0, unit='B', unit_scale=True, desc=title, dynamic_ncols=True) as pbar:
            tmp_path = self.cache_dir / f'{msg.id}'

            def _cb(current: int, total: int) -> None:
                pbar.total = total
                pbar.update(current - pbar.n)

            downloaded_path = await msg.download_media(file=str(tmp_path), progress_callback=_cb)
        if downloaded_path:
            downloaded_path = Path(downloaded_path)
            dst_path = (dst_dir / title).with_suffix(downloaded_path.suffix)
            shutil.move(downloaded_path, dst_path)
            return dst_path
        return NoneSchema

    async def update_channel(self, channel_id: str) -> None:
        channel = await self.client.get_entity(PeerChannel(channel_id))
        ch_name = getattr(channel, 'username', None) or getattr(channel, 'title', str(channel_id)) or str(channel_id)
        ch_name = self.sanitize(ch_name)
        dst = cfg.path / ch_name
        dst.mkdir(parents=True, exist_ok=True)

        videos = await self.get_videos(channel)
        undownloaded = [v for v in videos if v.id not in self.get_downloaded_ids(dst)]
        if not undownloaded:
            log.info('No new videos')
            return
        for idx, msg in enumerate(undownloaded):
            log.info('Downloading videos from %s (%d/%d)', ch_name, idx + 1, len(undownloaded))
            result = await self.download(msg, dst)
            if result:
                log.notice('Saved %s', result.name)
            else:
                log.error('Failed to download message %s', msg.id)

    async def update(self) -> None:
        await self.client.start()
        for channel_id in cfg.channels:
            await self.update_channel(channel_id)
        await self.client.disconnect()
