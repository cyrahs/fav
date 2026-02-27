import asyncio
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import httpx
from tqdm import tqdm

from src.core import config, logger
from src.core.config import KemonoCreator
from src.tool import database, sanitize

log = logger.get('kemono')
cfg = config.web.kemono

BASE_URL = 'https://kemono.cr'
API_PREFIX = '/api/v1'


@dataclass
class Attachment:
    name: str
    path: str


@dataclass
class Post:
    id: str
    user: str
    service: str
    title: str
    published: str | None
    attachments: list[Attachment]


class Kemono:
    def __init__(self) -> None:
        if cfg is None:
            msg = 'Kemono config is missing'
            raise ValueError(msg)
        self.cfg = cfg
        self.client = httpx.AsyncClient(
            base_url=BASE_URL,
            headers={
                'Accept': 'text/css',
            },
            timeout=60,
            follow_redirects=True,
            limits=httpx.Limits(max_keepalive_connections=10, max_connections=10),
            proxy=config.proxy or None,
        )

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
        """)

    async def _get_downloaded_ids(self, service: str, user_id: str) -> set[str]:
        rows = await database.query_db(
            'SELECT id FROM kemono WHERE service = ? AND user_id = ?;',
            (service, user_id),
        )
        return {row['id'] for row in rows}

    def _parse_post(self, data: dict[str, Any]) -> Post:
        attachments: list[Attachment] = []
        main_file = data.get('file') or {}
        if main_file.get('path'):
            name = main_file.get('name') or Path(main_file['path']).name
            attachments.append(Attachment(name=name, path=main_file['path']))

        for item in data.get('attachments', []):
            if not item.get('path'):
                continue
            name = item.get('name') or Path(item['path']).name
            attachments.append(Attachment(name=name, path=item['path']))

        return Post(
            id=str(data['id']),
            user=str(data['user']),
            service=str(data['service']),
            title=data.get('title') or 'untitled',
            published=data.get('published'),
            attachments=attachments,
        )

    async def _fetch_posts(self, service: str, user_id: str) -> list[Post]:
        url = f'{API_PREFIX}/{service}/user/{user_id}/posts'
        res = await self.client.get(url)
        res.raise_for_status()
        return [self._parse_post(item) for item in res.json()]

    async def _download_attachment(self, attachment: Attachment, dst_dir: Path, post_id: str, idx: int) -> None:
        src_url = f'{BASE_URL}/data{attachment.path}'
        ext = Path(attachment.name).suffix
        stem = sanitize(Path(attachment.name).stem, max_bytes=150)
        filename = f'{stem or f"file_{idx}"}{ext}'
        dst_path = dst_dir / filename
        if dst_path.exists():
            log.debug('File already exists, skip %s', dst_path.name)
            return

        await asyncio.to_thread(dst_dir.mkdir, parents=True, exist_ok=True)
        desc = f'{post_id}-{idx}'
        with tqdm(total=0, unit='B', unit_scale=True, desc=desc, dynamic_ncols=True) as pbar:
            async with self.client.stream('GET', src_url) as res:
                res.raise_for_status()
                total = int(res.headers.get('content-length', 0))
                pbar.total = total
                with dst_path.open('wb') as f:
                    async for chunk in res.aiter_bytes():
                        f.write(chunk)
                        pbar.update(len(chunk))

    async def _download_post(self, post: Post, creator_dir: Path) -> None:
        if not post.attachments:
            log.warning('Post %s has no downloadable attachments', post.id)
            return
        post_dirname = f'{sanitize(post.title, max_bytes=80) or "post"} [{post.id}]'
        post_dir = creator_dir / post_dirname
        for idx, attachment in enumerate(post.attachments, start=1):
            try:
                await self._download_attachment(attachment, post_dir, post.id, idx)
            except httpx.HTTPError:
                log.exception('Failed to download %s', attachment.path)

    async def _update_creator(self, creator: KemonoCreator) -> None:
        creator_name = creator.name or creator.id
        creator_dirname = f'{sanitize(creator_name, max_bytes=80)}_{creator.id}'
        creator_dir = self.cfg.path / creator.service / creator_dirname
        downloaded_ids = await self._get_downloaded_ids(creator.service, creator.id)

        posts = await self._fetch_posts(creator.service, creator.id)
        if not posts:
            log.info('No posts found for %s (%s)', creator_name, creator.service)
            return

        new_posts: list[Post] = []
        for post in posts:
            if post.id in downloaded_ids:
                break
            new_posts.append(post)

        if not new_posts:
            log.info('No new posts for %s (%s)', creator_name, creator.service)
            return

        log.info('Found %d new posts for %s (%s)', len(new_posts), creator_name, creator.service)
        for idx, post in enumerate(reversed(new_posts), start=1):
            log.info('Downloading post %s (%d/%d)', post.id, idx, len(new_posts))
            await self._download_post(post, creator_dir)
            await database.query_db(
                'INSERT INTO kemono (id, service, user_id, title, creator, published_at) VALUES (?, ?, ?, ?, ?, ?);',
                (post.id, post.service, post.user, post.title, creator_name, post.published or ''),
            )

    async def update(self) -> None:
        if self.cfg is None:
            log.warning('Kemono config is missing, skip update')
            return
        if not self.cfg.creators:
            log.info('No kemono creators configured')
            return

        self.cfg.path.mkdir(parents=True, exist_ok=True)
        await self._ensure_table()

        for creator in self.cfg.creators:
            await self._update_creator(creator)

        await self.client.aclose()
