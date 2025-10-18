import shutil
import tempfile
from collections import defaultdict
from pathlib import Path

from telethon import TelegramClient
from telethon.tl.types import Channel, DocumentAttributeVideo, Message, PeerChannel
from tqdm import tqdm

from src.core import config, logger
from src.tool import cloudflare, format_video_filename, sanitize

log = logger.get('telegram')
cfg = config.telegram


class Telegram:
    def __init__(self) -> None:
        self._tmp_dir = tempfile.TemporaryDirectory(prefix='fav-telegram-')
        self.cache_dir = Path(self._tmp_dir.name)
        self.client = TelegramClient(cfg.session_path, cfg.api_id, cfg.api_hash)

    def __del__(self) -> None:
        self._tmp_dir.cleanup()

    @staticmethod
    async def get_downloaded_ids(channel_id: int) -> list[int]:
        exists_ids = await cloudflare.query_d1('SELECT message_id FROM telegram WHERE channel_id = ?;', (str(channel_id),))
        return [int(i['message_id']) for i in exists_ids]

    async def get_videos(self, channel: Channel) -> list[dict]:
        """Get all video messages with pre-calculated filenames.
        
        Returns:
            List of dicts with 'msg' (Message) and 'filename' (str - base title without extension/ID)
        """
        videos = []
        group_captions = {}  # grouped_id -> caption
        
        # Collect videos and extract captions from all messages in groups
        async for msg in self.client.iter_messages(channel, reverse=True):
            # Check if this is a video
            is_video = False
            if getattr(msg, 'video', None):
                is_video = True
            elif getattr(msg, 'document', None) and getattr(msg.document, 'attributes', None):
                is_video = any(isinstance(attr, DocumentAttributeVideo) for attr in msg.document.attributes)
            
            if is_video:
                videos.append(msg)
            
            # Extract caption from any message in a media group
            grouped_id = getattr(msg, 'grouped_id', None)
            if grouped_id and msg.message and grouped_id not in group_captions:
                group_captions[grouped_id] = msg.message.strip()
        
        # Group videos by their grouped_id to determine indices
        video_groups = defaultdict(list)
        for msg in videos:
            grouped_id = getattr(msg, 'grouped_id', None)
            if grouped_id:
                video_groups[grouped_id].append(msg)
        
        # Build result list with pre-calculated filenames
        result = []
        processed_group_ids = set()
        
        for msg in videos:
            grouped_id = getattr(msg, 'grouped_id', None)
            
            if grouped_id:
                if grouped_id not in processed_group_ids:
                    # Process all videos in this group
                    group_videos = video_groups[grouped_id]
                    group_caption = group_captions.get(grouped_id)
                    
                    if group_caption:
                        # Has caption - use it as base filename with index
                        if len(group_videos) == 1:
                            # Single video in group - no index suffix
                            result.append({
                                'msg': group_videos[0],
                                'filename': group_caption
                            })
                        else:
                            # Multiple videos - add index suffix
                            for idx, video_msg in enumerate(group_videos, start=1):
                                result.append({
                                    'msg': video_msg,
                                    'filename': f'{group_caption}-{idx}'
                                })
                    else:
                        # No caption - each video uses its own ID
                        for video_msg in group_videos:
                            result.append({
                                'msg': video_msg,
                                'filename': f'video_{video_msg.id}'
                            })
                    
                    processed_group_ids.add(grouped_id)
            else:
                # Standalone video without group
                if msg.message and msg.message.strip():
                    # Has its own caption
                    base_filename = msg.message.strip()
                else:
                    # No caption - use video_{id} format
                    base_filename = f'video_{msg.id}'
                
                result.append({
                    'msg': msg,
                    'filename': base_filename
                })
        
        return result

    async def download(self, msg: Message, dst_dir: Path, title: str) -> Path | None:
        """Download a video message with specified title."""
        if not dst_dir.exists():
            dst_dir.mkdir(parents=True, exist_ok=True)
        elif dst_dir.is_file():
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
