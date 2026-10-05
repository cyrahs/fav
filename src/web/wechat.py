"""Images forwarded to the agent's WeChat account, drained from the FavInbox API.

WeChat runs in redroid on another host. The WAuxiliary FavInbox plugin saves the
original of every image a private chat receives, and a small HTTP service next to
it (script/wechat_favinbox/favinbox_server.py) serves that inbox:

* ``GET /items`` lists the pending items from their JSON sidecars;
* ``GET /file/<msg_id>`` returns one image;
* ``POST /ack/<msg_id>`` deletes the image there and keeps the sidecar as a
  tombstone, so the plugin's backfill never saves it again.

A run lists the inbox, downloads every image it has not stored yet, records it,
and only then acknowledges it. A run that dies between the insert and the ack
leaves the item pending; the next run finds the row and only acknowledges it.

Items are keyed on WeChat's local ``msg_id`` together with the message's
``create_time``. The id alone is a row number in the account's message database,
so it restarts if WeChat is reinstalled; the pair stays unique across that. The
``md5`` the plugin reports is WeChat's own value and does not hash the saved
bytes, so it is kept for reference and never used to deduplicate.
"""

from __future__ import annotations

import asyncio
import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import httpx

from src.core import logger, settings
from src.tool import database
from src.tool.filename import ensure_unique_path
from src.tool.notifications import enqueue_notification

log = logger.get('wechat')

# The extensions the plugin writes after sniffing the bytes. ``wxgf`` is WeChat's
# own animated format and ``bin`` an unrecognised one; both are still the original
# bytes, so they are stored as they are.
_KNOWN_EXTENSIONS = frozenset({'jpg', 'png', 'gif', 'webp', 'wxgf', 'bin'})
_CONTENT_TYPE_EXTENSIONS = {
    'image/jpeg': 'jpg',
    'image/png': 'png',
    'image/gif': 'gif',
    'image/webp': 'webp',
}
_REQUEST_TIMEOUT_SECONDS = 60.0


class WechatInboxError(RuntimeError):
    def __init__(self, message: str, *, notification_dedupe_key: str = '') -> None:
        super().__init__(message)
        self.notification_dedupe_key = notification_dedupe_key


def pending_images(items: list[Any]) -> list[dict[str, Any]]:
    """The listed items this source should download, oldest first.

    Only images the plugin saved successfully and whose file is still there.
    Videos and files are only recorded by the plugin for now, and an item without
    a usable ``msg_id``/``create_time`` cannot be keyed, so both are left out.
    """
    selected: list[dict[str, Any]] = []
    for item in items:
        if not isinstance(item, dict):
            continue
        if item.get('type') != 'image' or not item.get('has_file'):
            continue
        if not str(item.get('status') or '').startswith('ok'):
            continue
        if not isinstance(item.get('msg_id'), int) or not isinstance(item.get('create_time'), int):
            continue
        selected.append(item)
    return sorted(selected, key=lambda item: (item['create_time'], item['msg_id']))


def file_extension(item: dict[str, Any], content_type: str) -> str:
    """The extension to store an item under: the plugin's, else the response's."""
    suffix = Path(str(item.get('file') or '')).suffix.lstrip('.').lower()
    if suffix in _KNOWN_EXTENSIONS:
        return suffix
    return _CONTENT_TYPE_EXTENSIONS.get(content_type.split(';', 1)[0].strip().lower(), 'bin')


def sent_at(create_time_ms: int) -> datetime:
    return datetime.fromtimestamp(create_time_ms / 1000, tz=UTC)


def relative_path(item: dict[str, Any], ext: str) -> Path:
    """Where an item is stored, relative to the collection directory.

    Grouped by month so the directory stays browsable, and named by send time so
    a listing sorts chronologically, with the id in brackets as elsewhere in the
    collection.
    """
    when = sent_at(item['create_time'])
    return Path(when.strftime('%Y-%m')) / f'{when:%Y-%m-%d %H%M%S} [{item["msg_id"]}].{ext}'


class Wechat:
    def __init__(self) -> None:
        self.cfg = settings.load().web.wechat
        # trust_env off: the service is an in-cluster address, and a deployment-wide
        # HTTP_PROXY meant for the crawled sites would otherwise swallow it.
        self.client = httpx.AsyncClient(
            headers={'Authorization': f'Bearer {self.cfg.token}'},
            timeout=_REQUEST_TIMEOUT_SECONDS,
            trust_env=False,
        )

    async def aclose(self) -> None:
        await self.client.aclose()

    async def _ensure_table(self) -> None:
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS wechat (
                msg_id BIGINT NOT NULL,
                create_time BIGINT NOT NULL,
                talker TEXT NOT NULL DEFAULT '',
                sender TEXT NOT NULL DEFAULT '',
                sent_at TEXT NOT NULL DEFAULT '',
                md5 TEXT NOT NULL DEFAULT '',
                size BIGINT,
                local_path TEXT NOT NULL DEFAULT '',
                metadata JSONB,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (msg_id, create_time)
            );
        """)

    def _url(self, path: str) -> str:
        return f'{self.cfg.api_url}{path}'

    async def _list_items(self) -> list[Any]:
        try:
            response = await self.client.get(self._url('/items'))
        except httpx.HTTPError as exc:
            msg = f'FavInbox at {self.cfg.api_url} is unreachable: {exc}'
            raise WechatInboxError(msg, notification_dedupe_key='wechat:unreachable') from exc
        if response.status_code == httpx.codes.UNAUTHORIZED:
            msg = 'FavInbox rejected the token. Copy FAVINBOX_TOKEN from the host into the WeChat settings again.'
            raise WechatInboxError(msg, notification_dedupe_key='wechat:auth')
        if response.status_code != httpx.codes.OK:
            msg = f'FavInbox answered /items with HTTP {response.status_code}'
            raise WechatInboxError(msg, notification_dedupe_key='wechat:list')
        payload = response.json()
        if not isinstance(payload, list):
            msg = 'FavInbox /items did not return a list'
            raise WechatInboxError(msg, notification_dedupe_key='wechat:list')
        return payload

    async def _already_stored(self, item: dict[str, Any]) -> bool:
        rows = await database.query_db(
            'SELECT 1 FROM wechat WHERE msg_id = ? AND create_time = ?;',
            (item['msg_id'], item['create_time']),
        )
        return bool(rows)

    async def _download(self, item: dict[str, Any]) -> Path:
        """Fetch one image, write it into the collection, and return its relative path."""
        response = await self.client.get(self._url(f'/file/{item["msg_id"]}'))
        response.raise_for_status()
        if not response.content:
            msg = f'FavInbox returned an empty file for message {item["msg_id"]}'
            raise WechatInboxError(msg)
        ext = file_extension(item, response.headers.get('content-type', ''))
        relative = relative_path(item, ext)

        def write() -> Path:
            destination = ensure_unique_path(self.cfg.path / relative)
            destination.parent.mkdir(parents=True, exist_ok=True)
            partial = destination.with_name(f'{destination.name}.part')
            partial.write_bytes(response.content)
            partial.replace(destination)
            return destination

        destination = await asyncio.to_thread(write)
        return destination.relative_to(self.cfg.path)

    async def _record(self, item: dict[str, Any], local_path: Path, size: int) -> None:
        await database.query_db(
            """
            INSERT INTO wechat (
                msg_id, create_time, talker, sender, sent_at, md5, size, local_path, metadata
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (msg_id, create_time) DO NOTHING;
            """,
            (
                item['msg_id'],
                item['create_time'],
                str(item.get('talker') or ''),
                str(item.get('sender') or ''),
                sent_at(item['create_time']).strftime('%Y-%m-%d %H:%M:%S'),
                str(item.get('md5') or ''),
                size,
                str(local_path),
                json.dumps(item, ensure_ascii=False),
            ),
        )

    async def _ack(self, item: dict[str, Any]) -> None:
        """Tell the host it may drop its copy. A failure only delays that to the next run."""
        try:
            response = await self.client.post(self._url(f'/ack/{item["msg_id"]}'))
            response.raise_for_status()
        except httpx.HTTPError as exc:
            log.warning('Could not acknowledge WeChat message %s; the next run retries: %s', item['msg_id'], exc)

    async def _notify_summary(self, downloaded: int) -> None:
        try:
            await enqueue_notification(
                kind='summary',
                source='wechat',
                header='WeChat',
                title='Inbox update completed',
                body=f'Saved {downloaded} forwarded images.',
                payload={'downloaded': downloaded},
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue wechat summary notification: %s', exc)

    async def update(self) -> None:
        missing = self.cfg.validate_runnable()
        if missing:
            log.warning('WeChat is not configured (missing %s), skip update', ', '.join(missing))
            return

        await self._ensure_table()
        await asyncio.to_thread(self.cfg.path.mkdir, parents=True, exist_ok=True)

        items = pending_images(await self._list_items())
        downloaded = 0
        failed: list[int] = []
        for item in items:
            if await self._already_stored(item):
                # Stored by a run that died before its ack got through.
                await self._ack(item)
                continue
            try:
                local_path = await self._download(item)
            except (httpx.HTTPError, OSError, WechatInboxError) as exc:
                log.warning('Failed to download WeChat message %s: %s', item['msg_id'], exc)
                failed.append(item['msg_id'])
                continue
            size = (self.cfg.path / local_path).stat().st_size
            await self._record(item, local_path, size)
            await self._ack(item)
            downloaded += 1
            log.info('Saved WeChat message %s as %s', item['msg_id'], local_path)

        log.info('WeChat saved %d new images (%d pending, %d failed)', downloaded, len(items), len(failed))
        if downloaded:
            await self._notify_summary(downloaded)
        if failed:
            ids = ', '.join(str(msg_id) for msg_id in failed)
            msg = f'Could not download {len(failed)} WeChat image(s): {ids}'
            raise WechatInboxError(msg, notification_dedupe_key='wechat:download')
