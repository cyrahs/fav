"""Images and 公众号 articles forwarded to the agent's WeChat account.

WeChat runs in redroid on another host, with the WAuxiliary FavInbox plugin
inside it (script/wechat_favinbox). A small HTTP service next to it answers two
APIs at one address, behind one token:

* the bridge, under ``/wx/``: the plugin's view of WeChat itself.
  ``/wx/events`` is the log of every message the account sees, numbered by
  ``seq``; ``/wx/image`` returns the original of an image message.
* the inbox: the originals the plugin saved on its own as each image arrived.
  ``GET /items`` lists them, ``GET /file/<msg_id>`` returns one, and
  ``POST /ack/<msg_id>`` deletes it there and keeps its sidecar as a tombstone,
  so the plugin's backfill never saves it again.

A run reads the events after its cursor in ``wechat_state`` and queues the ones
worth keeping -- images and shared articles received in a private chat -- in
``wechat_events``, so the cursor moves on even when saving an item fails. It then
works that queue. An image is fetched through the bridge, or from the inbox when
the bridge cannot; an article is fetched straight from mp.weixin.qq.com with
every image at full size. Image posts, which the site puts behind a captcha,
come from the service's ``POST /post/page`` instead: it has the agent's WeChat
open the post and returns the page from WeChat's web view cache. A failed item
is retried on later runs, up to ``_MAX_ATTEMPTS`` times. Last, the run drains the
inbox as it always has, which catches what the event log misses: the plugin only
logs while it is loaded.

Items are keyed on WeChat's local ``msg_id`` together with the message's
``create_time``. The id alone is a row number in the account's message database,
so it restarts if WeChat is reinstalled; the pair stays unique across that. The
``md5`` WeChat reports does not hash the saved bytes, so it is kept for
reference and never used to deduplicate.
"""

from __future__ import annotations

import asyncio
import json
import shutil
from dataclasses import replace
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import httpx

from src.core import logger, settings
from src.tool import database
from src.tool.filename import ensure_unique_path, sanitize
from src.tool.notifications import enqueue_notification
from src.web.wechat_article import (
    ArticleLink,
    ArticlePage,
    canonical_url,
    cover_only_page,
    image_extension,
    is_captcha,
    original_image_url,
    parse_article_link,
    parse_article_page,
)

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
_MAGIC_EXTENSIONS = ((b'\xff\xd8\xff', 'jpg'), (b'\x89PNG', 'png'), (b'GIF8', 'gif'), (b'wxgf', 'wxgf'))
_REQUEST_TIMEOUT_SECONDS = 60.0
_EVENT_PAGE_SIZE = 200
_MAX_ATTEMPTS = 5
# How long the FavInbox service waits for WeChat to load an image post it opened.
_OPEN_WAIT_SECONDS = 30
_CURSOR_KEY = 'events_after'
# A phone browser, as the share link would open in. mp.weixin.qq.com serves the
# full page to it without a login.
_ARTICLE_USER_AGENT = 'Mozilla/5.0 (Linux; Android 12) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36'

WORK_IMAGE = 'image'
WORK_ARTICLE = 'article'


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


def work_for(event: Any) -> str | None:
    """What a bridge event asks of this source: an image, an article, or nothing.

    Only messages the account received in a private chat count, the same as the
    inbox: group chats and 公众号 pushes are not things anyone forwarded.
    """
    if not isinstance(event, dict) or event.get('kind') != 'message':
        return None
    message = event.get('data')
    if not isinstance(message, dict) or message.get('is_send') or message.get('chat') != 'private':
        return None
    if not isinstance(message.get('msg_id'), int) or not isinstance(message.get('create_time'), int):
        return None
    if message.get('kind') == 'image':
        return WORK_IMAGE
    if parse_article_link(str(message.get('content') or '')) is not None:
        return WORK_ARTICLE
    return None


def queued_message(message: dict[str, Any], work: str) -> dict[str, Any]:
    """The part of a message event worth keeping in the queue.

    An image message's content and ``image`` bean carry its CDN address and AES
    key; the bridge finds the image by id, so neither is stored.
    """
    kept = {key: message.get(key) for key in ('msg_id', 'create_time', 'type', 'kind', 'talker', 'sender')}
    if work == WORK_ARTICLE:
        kept['content'] = message.get('content')
    image = message.get('image')
    if work == WORK_IMAGE and isinstance(image, dict):
        kept['md5'] = image.get('md5') or ''
    return kept


def image_item(message: dict[str, Any]) -> dict[str, Any]:
    """An image message from the queue, in the shape the inbox lists items."""
    return {
        'msg_id': message['msg_id'],
        'create_time': message['create_time'],
        'type': 'image',
        'talker': str(message.get('talker') or ''),
        'sender': str(message.get('sender') or message.get('talker') or ''),
        'md5': str(message.get('md5') or ''),
    }


def sniff_extension(content: bytes) -> str | None:
    for magic, ext in _MAGIC_EXTENSIONS:
        if content.startswith(magic):
            return ext
    if content[:4] == b'RIFF' and content[8:12] == b'WEBP':
        return 'webp'
    return None


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


def article_directory(message: dict[str, Any], title: str) -> Path:
    """The folder an article's images go in, named like an image but with the title."""
    when = sent_at(message['create_time'])
    name = ' '.join(part for part in (f'{when:%Y-%m-%d %H%M%S}', sanitize(title, max_bytes=120), f'[{message["msg_id"]}]') if part)
    return Path(when.strftime('%Y-%m')) / name


def _unused_directory(path: Path) -> Path:
    candidate, counter = path, 1
    while candidate.exists():
        candidate = path.with_name(f'{path.name} ({counter})')
        counter += 1
    return candidate


def _error_detail(response: httpx.Response) -> str:
    """The service's own ``{"error": ...}`` text, which says which side is down."""
    try:
        payload = response.json()
    except ValueError:
        return ''
    error = payload.get('error') if isinstance(payload, dict) else None
    return f': {error}' if error else ''


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
        # Articles are public pages, and the cluster reaches them directly. No
        # Referer: mmbiz answers an image request carrying a foreign one with a
        # 140x140 placeholder.
        self.web = httpx.AsyncClient(
            headers={'User-Agent': _ARTICLE_USER_AGENT},
            timeout=_REQUEST_TIMEOUT_SECONDS,
            follow_redirects=True,
            trust_env=False,
        )

    async def aclose(self) -> None:
        await self.client.aclose()
        await self.web.aclose()

    async def _ensure_table(self) -> None:
        # Not `wechat`: that name still holds the table of the earlier iLink /
        # 文件传输助手 source (removed in #66), with an unrelated schema.
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS wechat_inbox (
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
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS wechat_article (
                msg_id BIGINT NOT NULL,
                create_time BIGINT NOT NULL,
                talker TEXT NOT NULL DEFAULT '',
                sender TEXT NOT NULL DEFAULT '',
                sent_at TEXT NOT NULL DEFAULT '',
                url TEXT NOT NULL DEFAULT '',
                title TEXT NOT NULL DEFAULT '',
                account TEXT NOT NULL DEFAULT '',
                published_at TEXT NOT NULL DEFAULT '',
                image_count INTEGER NOT NULL DEFAULT 0,
                local_path TEXT NOT NULL DEFAULT '',
                metadata JSONB,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (msg_id, create_time)
            );
        """)
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS wechat_events (
                msg_id BIGINT NOT NULL,
                create_time BIGINT NOT NULL,
                work TEXT NOT NULL,
                seq BIGINT NOT NULL DEFAULT 0,
                message JSONB NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT NOT NULL DEFAULT '',
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (msg_id, create_time)
            );
        """)
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS wechat_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
        """)

    def _url(self, path: str) -> str:
        return f'{self.cfg.api_url}{path}'

    def _unauthorized(self) -> WechatInboxError:
        msg = 'FavInbox rejected the token. Copy FAVINBOX_TOKEN from the host into the WeChat settings again.'
        return WechatInboxError(msg, notification_dedupe_key='wechat:auth')

    # ---------- bridge events ----------

    async def _cursor(self) -> int:
        rows = await database.query_db('SELECT value FROM wechat_state WHERE key = ?;', (_CURSOR_KEY,))
        return int(rows[0]['value']) if rows else 0

    async def _save_cursor(self, seq: int) -> None:
        await database.query_db(
            'INSERT INTO wechat_state (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value;',
            (_CURSOR_KEY, str(seq)),
        )

    async def _fetch_events(self, after: int) -> tuple[int, list[dict[str, Any]]]:
        try:
            response = await self.client.get(self._url('/wx/events'), params={'after': after, 'limit': _EVENT_PAGE_SIZE})
        except httpx.HTTPError as exc:
            msg = f'FavInbox at {self.cfg.api_url} is unreachable: {exc}'
            raise WechatInboxError(msg, notification_dedupe_key='wechat:unreachable') from exc
        if response.status_code == httpx.codes.UNAUTHORIZED:
            raise self._unauthorized()
        if response.status_code != httpx.codes.OK:
            msg = f'The WeChat bridge answered /wx/events with HTTP {response.status_code}{_error_detail(response)}'
            raise WechatInboxError(msg, notification_dedupe_key='wechat:bridge')
        payload = response.json()
        if not isinstance(payload, dict) or not isinstance(payload.get('last_seq'), int) or not isinstance(payload.get('events'), list):
            msg = 'The WeChat bridge answered /wx/events with something other than an event page'
            raise WechatInboxError(msg, notification_dedupe_key='wechat:bridge')
        events = [event for event in payload['events'] if isinstance(event, dict) and isinstance(event.get('seq'), int)]
        return payload['last_seq'], events

    async def _queue(self, event: dict[str, Any], work: str) -> None:
        message = event['data']
        await database.query_db(
            """
            INSERT INTO wechat_events (msg_id, create_time, work, seq, message)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT (msg_id, create_time) DO NOTHING;
            """,
            (message['msg_id'], message['create_time'], work, event['seq'], json.dumps(queued_message(message, work), ensure_ascii=False)),
        )

    async def _read_events(self) -> int:
        """Queue the new events worth keeping and move the cursor past all of them."""
        cursor = await self._cursor()
        queued = 0
        while True:
            last_seq, events = await self._fetch_events(cursor)
            if last_seq < cursor:
                # The plugin numbers events from its log file, so a lost log
                # starts over at 1. The queue's key keeps a re-read harmless.
                log.warning(
                    'The WeChat bridge restarted its event numbering (last seq %d, cursor %d); reading from the start', last_seq, cursor
                )
                cursor = 0
                await self._save_cursor(cursor)
                continue
            if not events:
                return queued
            if events[0]['seq'] > cursor + 1:
                log.warning('WeChat bridge events %d-%d were rotated out before this source read them', cursor + 1, events[0]['seq'] - 1)
            for event in events:
                work = work_for(event)
                if work is not None:
                    await self._queue(event, work)
                    queued += 1
            cursor = max(event['seq'] for event in events)
            await self._save_cursor(cursor)
            if len(events) < _EVENT_PAGE_SIZE or cursor >= last_seq:
                return queued

    async def _pending_work(self) -> list[dict[str, Any]]:
        return await database.query_db(
            "SELECT msg_id, create_time, work, message, attempts FROM wechat_events WHERE status = 'pending' ORDER BY create_time, msg_id;",
        )

    async def _finish_work(self, row: dict[str, Any], error: str = '') -> bool:
        """Record the outcome of one queued item; True when it will not be tried again."""
        attempts = int(row['attempts']) + (1 if error else 0)
        status = 'done' if not error else ('failed' if attempts >= _MAX_ATTEMPTS else 'pending')
        await database.query_db(
            """
            UPDATE wechat_events SET status = ?, attempts = ?, last_error = ?, updated_at = CURRENT_TIMESTAMP
            WHERE msg_id = ? AND create_time = ?;
            """,
            (status, attempts, error[:1000], row['msg_id'], row['create_time']),
        )
        return status == 'failed'

    async def _work_queue(self) -> tuple[int, list[str]]:
        saved = 0
        failures: list[str] = []
        for row in await self._pending_work():
            message = row['message'] if isinstance(row['message'], dict) else json.loads(row['message'])
            try:
                if row['work'] == WORK_ARTICLE:
                    await self._save_article(message, final_attempt=int(row['attempts']) + 1 >= _MAX_ATTEMPTS)
                else:
                    await self._save_bridge_image(message)
            except (httpx.HTTPError, OSError, WechatInboxError) as exc:
                error = str(exc) or type(exc).__name__
                log.warning('Failed to save WeChat %s %s: %s', row['work'], row['msg_id'], error)
                gave_up = await self._finish_work(row, error)
                failures.append(f'{row["work"]} {row["msg_id"]}{" (gave up)" if gave_up else ""}: {error[:200]}')
                continue
            await self._finish_work(row)
            saved += 1
        return saved, failures

    # ---------- images ----------

    async def _already_stored(self, item: dict[str, Any]) -> bool:
        rows = await database.query_db(
            'SELECT 1 FROM wechat_inbox WHERE msg_id = ? AND create_time = ?;',
            (item['msg_id'], item['create_time']),
        )
        return bool(rows)

    async def _inbox_file(self, item: dict[str, Any]) -> tuple[bytes, str]:
        response = await self.client.get(self._url(f'/file/{item["msg_id"]}'))
        response.raise_for_status()
        if not response.content:
            msg = f'FavInbox returned an empty file for message {item["msg_id"]}'
            raise WechatInboxError(msg)
        return response.content, response.headers.get('content-type', '')

    async def _bridge_image(self, item: dict[str, Any]) -> tuple[bytes, str]:
        """The original through the bridge, or the inbox's copy when the bridge cannot get it."""
        params = {'talker': item['talker'], 'msg_id': item['msg_id'], 'create_time': item['create_time']}
        try:
            response = await self.client.get(self._url('/wx/image'), params=params)
            response.raise_for_status()
        except httpx.HTTPError as exc:
            reason = str(exc)
        else:
            if response.content:
                return response.content, response.headers.get('content-type', '')
            reason = 'an empty body'
        log.info('The WeChat bridge could not return image %s (%s); trying the inbox', item['msg_id'], reason)
        return await self._inbox_file(item)

    async def _store_image(self, item: dict[str, Any], content: bytes, content_type: str) -> Path:
        """Write one image into the collection and return its relative path."""
        ext = file_extension(item, content_type) if item.get('file') else (sniff_extension(content) or file_extension(item, content_type))
        relative = relative_path(item, ext)

        def write() -> Path:
            destination = ensure_unique_path(self.cfg.path / relative)
            destination.parent.mkdir(parents=True, exist_ok=True)
            partial = destination.with_name(f'{destination.name}.part')
            partial.write_bytes(content)
            partial.replace(destination)
            return destination

        destination = await asyncio.to_thread(write)
        return destination.relative_to(self.cfg.path)

    async def _record(self, item: dict[str, Any], local_path: Path, size: int) -> None:
        await database.query_db(
            """
            INSERT INTO wechat_inbox (
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

    async def _ack(self, item: dict[str, Any], *, missing_ok: bool = False) -> None:
        """Tell the host it may drop its copy. A failure only delays that to the next run."""
        try:
            response = await self.client.post(self._url(f'/ack/{item["msg_id"]}'))
            if missing_ok and response.status_code == httpx.codes.NOT_FOUND:
                return
            response.raise_for_status()
        except httpx.HTTPError as exc:
            log.warning('Could not acknowledge WeChat message %s; the next run retries: %s', item['msg_id'], exc)

    async def _notify_download(self, item: dict[str, Any], local_path: Path) -> None:
        """One message per image, sent as the photo itself, as the Telegram source does."""
        saved_path = self.cfg.path / local_path
        try:
            await enqueue_notification(
                kind='download_completed',
                source='wechat',
                header='WeChat',
                title=local_path.name,
                body=f'From {item.get("sender") or item.get("talker") or "unknown"} | Message ID {item["msg_id"]}',
                payload={
                    'msg_id': item['msg_id'],
                    'sender': str(item.get('sender') or ''),
                    'saved_path': str(saved_path),
                    'image_path': str(saved_path),
                },
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue wechat download notification for message %s: %s', item['msg_id'], exc)

    async def _keep_image(self, item: dict[str, Any], content: bytes, content_type: str) -> None:
        local_path = await self._store_image(item, content, content_type)
        await self._record(item, local_path, len(content))
        log.info('Saved WeChat message %s as %s', item['msg_id'], local_path)
        await self._notify_download(item, local_path)

    async def _save_bridge_image(self, message: dict[str, Any]) -> None:
        item = image_item(message)
        if not await self._already_stored(item):
            content, content_type = await self._bridge_image(item)
            await self._keep_image(item, content, content_type)
        # The plugin saved its own copy when the image arrived, unless it was off.
        await self._ack(item, missing_ok=True)

    async def _drain_inbox(self) -> None:
        try:
            response = await self.client.get(self._url('/items'))
        except httpx.HTTPError as exc:
            msg = f'FavInbox at {self.cfg.api_url} is unreachable: {exc}'
            raise WechatInboxError(msg, notification_dedupe_key='wechat:unreachable') from exc
        if response.status_code == httpx.codes.UNAUTHORIZED:
            raise self._unauthorized()
        if response.status_code != httpx.codes.OK:
            msg = f'FavInbox answered /items with HTTP {response.status_code}'
            raise WechatInboxError(msg, notification_dedupe_key='wechat:list')
        payload = response.json()
        if not isinstance(payload, list):
            msg = 'FavInbox /items did not return a list'
            raise WechatInboxError(msg, notification_dedupe_key='wechat:list')

        items = pending_images(payload)
        downloaded = 0
        failed: list[int] = []
        for item in items:
            if await self._already_stored(item):
                # Saved through the bridge, or by a run that died before its ack.
                await self._ack(item)
                continue
            try:
                content, content_type = await self._inbox_file(item)
                await self._keep_image(item, content, content_type)
            except (httpx.HTTPError, OSError, WechatInboxError) as exc:
                log.warning('Failed to download WeChat message %s: %s', item['msg_id'], exc)
                failed.append(item['msg_id'])
                continue
            await self._ack(item)
            downloaded += 1

        log.info('WeChat inbox: saved %d new images (%d pending, %d failed)', downloaded, len(items), len(failed))
        if failed:
            ids = ', '.join(str(msg_id) for msg_id in failed)
            msg = f'Could not download {len(failed)} WeChat image(s): {ids}'
            raise WechatInboxError(msg, notification_dedupe_key='wechat:download')

    # ---------- articles ----------

    async def _article_stored(self, message: dict[str, Any], link: ArticleLink) -> bool:
        """Whether this message, or an earlier forward of the same article, was saved already."""
        rows = await database.query_db(
            'SELECT 1 FROM wechat_article WHERE (msg_id = ? AND create_time = ?) OR url = ?;',
            (message['msg_id'], message['create_time'], canonical_url(link.url)),
        )
        return bool(rows)

    async def _article_image(self, url: str) -> tuple[bytes, str] | None:
        """One body image at full size, else as the page shows it; None when neither loads."""
        for candidate in dict.fromkeys((original_image_url(url), url)):
            try:
                response = await self.web.get(candidate)
                response.raise_for_status()
            except httpx.HTTPError as exc:
                log.info('Article image %s failed: %s', candidate, exc)
                continue
            content_type = response.headers.get('content-type', '')
            if response.content and content_type.lower().startswith('image/'):
                return response.content, sniff_extension(response.content) or image_extension(candidate, content_type)
        return None

    def _write_article(
        self,
        message: dict[str, Any],
        link: ArticleLink,
        page: ArticlePage,
        page_html: str,
        images: list[tuple[bytes, str] | None],
    ) -> tuple[Path, list[dict[str, Any]]]:
        """Write the images, the page and a summary into the article's folder, all or nothing."""
        title = page.title or link.title
        destination = _unused_directory(self.cfg.path / article_directory(message, title))
        partial = destination.with_name(f'{destination.name}.part')
        if partial.exists():
            shutil.rmtree(partial)
        partial.mkdir(parents=True)
        width = max(2, len(str(len(images))))
        entries: list[dict[str, Any]] = []
        for index, (source, fetched) in enumerate(zip(page.images, images, strict=True), start=1):
            entry: dict[str, Any] = {'url': source}
            if fetched is not None:
                content, ext = fetched
                entry['file'] = f'{index:0{width}d}.{ext}'
                (partial / entry['file']).write_bytes(content)
            entries.append(entry)
        if page_html:
            (partial / 'article.html').write_text(page_html, encoding='utf-8')
        summary = {
            'title': title,
            'account': page.account or link.account,
            'published_at': page.published_at.isoformat() if page.published_at else None,
            'url': canonical_url(link.url),
            'share_url': link.url,
            'description': link.description,
            'msg_id': message['msg_id'],
            'create_time': message['create_time'],
            'sender': message.get('sender') or message.get('talker') or '',
            'picture_count': max(page.picture_count, len(entries)),
            'images': entries,
        }
        (partial / 'article.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding='utf-8')
        partial.replace(destination)
        return destination.relative_to(self.cfg.path), entries

    async def _page_in_wechat(self, link: ArticleLink) -> tuple[ArticlePage, str]:
        """An image post as the agent's WeChat loads it, through the FavInbox service."""
        try:
            response = await self.client.post(
                self._url('/post/page'),
                json={'url': link.url, 'wait': _OPEN_WAIT_SECONDS},
                timeout=_OPEN_WAIT_SECONDS + _REQUEST_TIMEOUT_SECONDS,
            )
        except httpx.HTTPError as exc:
            msg = f'FavInbox could not open the image post in WeChat: {exc}'
            raise WechatInboxError(msg) from exc
        if response.status_code == httpx.codes.UNAUTHORIZED:
            raise self._unauthorized()
        if response.status_code != httpx.codes.OK:
            msg = f'FavInbox could not open the image post in WeChat: HTTP {response.status_code}{_error_detail(response)}'
            raise WechatInboxError(msg)
        page = parse_article_page(response.text)
        if not page.images:
            msg = 'the image post WeChat opened lists no pictures'
            raise WechatInboxError(msg)
        return replace(page, picture_count=max(link.picture_count, len(page.images))), response.text

    async def _image_post(self, link: ArticleLink, *, final_attempt: bool) -> tuple[ArticlePage, str]:
        """An image post the site put behind its captcha: all of it from WeChat, else its first picture."""
        cover = cover_only_page(link)
        if cover is None:
            msg = f'mp.weixin.qq.com answered {canonical_url(link.url)} with a captcha page'
            raise WechatInboxError(msg)
        try:
            return await self._page_in_wechat(link)
        except WechatInboxError as exc:
            # Retried while attempts remain; the first picture beats nothing at all.
            if not final_attempt or exc.notification_dedupe_key == 'wechat:auth':
                raise
            log.warning('WeChat image post %s: %s; saving its first picture only', canonical_url(link.url), exc)
            return cover, ''

    async def _save_article(self, message: dict[str, Any], *, final_attempt: bool = False) -> None:
        link = parse_article_link(str(message.get('content') or ''))
        if link is None:
            msg = f'message {message["msg_id"]} is not an article share'
            raise WechatInboxError(msg)
        if await self._article_stored(message, link):
            return
        response = await self.web.get(link.url)
        page_html = response.text
        if is_captcha(str(response.url)):
            page, page_html = await self._image_post(link, final_attempt=final_attempt)
        else:
            response.raise_for_status()
            page = parse_article_page(page_html)
        if not page.title and not page.images:
            msg = f'{canonical_url(link.url)} has no title or images; the article may have been deleted'
            raise WechatInboxError(msg)

        images = [await self._article_image(url) for url in page.images]
        saved = sum(1 for image in images if image is not None)
        if page.images and not saved:
            msg = f'none of the {len(page.images)} images of {canonical_url(link.url)} could be downloaded'
            raise WechatInboxError(msg)
        local_path, entries = await asyncio.to_thread(self._write_article, message, link, page, page_html, images)
        await self._record_article(message, link, page, local_path, entries)
        log.info('Saved WeChat article %s (%d of %d images) as %s', message['msg_id'], saved, len(images), local_path)
        await self._notify_article(message, link, page, local_path, entries)

    async def _record_article(
        self,
        message: dict[str, Any],
        link: ArticleLink,
        page: ArticlePage,
        local_path: Path,
        entries: list[dict[str, Any]],
    ) -> None:
        await database.query_db(
            """
            INSERT INTO wechat_article (
                msg_id, create_time, talker, sender, sent_at, url, title, account, published_at, image_count, local_path, metadata
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (msg_id, create_time) DO NOTHING;
            """,
            (
                message['msg_id'],
                message['create_time'],
                str(message.get('talker') or ''),
                str(message.get('sender') or message.get('talker') or ''),
                sent_at(message['create_time']).strftime('%Y-%m-%d %H:%M:%S'),
                canonical_url(link.url),
                page.title or link.title,
                page.account or link.account,
                page.published_at.strftime('%Y-%m-%d %H:%M:%S') if page.published_at else '',
                sum(1 for entry in entries if 'file' in entry),
                str(local_path),
                json.dumps(
                    {
                        'share_url': link.url,
                        'description': link.description,
                        'picture_count': max(page.picture_count, len(entries)),
                        'images': entries,
                    },
                    ensure_ascii=False,
                ),
            ),
        )

    async def _notify_article(
        self,
        message: dict[str, Any],
        link: ArticleLink,
        page: ArticlePage,
        local_path: Path,
        entries: list[dict[str, Any]],
    ) -> None:
        """One message per article, linking to it and showing its first image."""
        files = [entry['file'] for entry in entries if 'file' in entry]
        folder = self.cfg.path / local_path
        total = max(page.picture_count, len(entries))
        noun = 'image' if total == 1 else 'images'
        count = f'{len(files)} {noun}' if len(files) == total else f'{len(files)} of {total} {noun}'
        if page.picture_count > len(entries):
            count += ' (WeChat could not open the rest)'
        sender = message.get('sender') or message.get('talker') or 'unknown'
        payload: dict[str, Any] = {'msg_id': message['msg_id'], 'sender': str(sender), 'saved_path': str(folder)}
        if files:
            payload['image_path'] = str(folder / files[0])
        try:
            await enqueue_notification(
                kind='download_completed',
                source='wechat',
                header='WeChat',
                title=page.title or link.title or local_path.name,
                link_url=canonical_url(link.url),
                body=' | '.join(part for part in (page.account or link.account, count, f'From {sender}') if part),
                payload=payload,
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue wechat article notification for message %s: %s', message['msg_id'], exc)

    # ---------- run ----------

    async def update(self) -> None:
        missing = self.cfg.validate_runnable()
        if missing:
            log.warning('WeChat is not configured (missing %s), skip update', ', '.join(missing))
            return

        await self._ensure_table()
        await asyncio.to_thread(self.cfg.path.mkdir, parents=True, exist_ok=True)

        # Each step runs even when the one before it failed: the inbox still
        # holds every image when the bridge is down, and queued work does not
        # depend on reading new events.
        problems: list[WechatInboxError] = []
        try:
            queued = await self._read_events()
        except WechatInboxError as exc:
            log.warning('Could not read WeChat bridge events: %s', exc)
            problems.append(exc)
        else:
            log.info('WeChat bridge: queued %d new item(s)', queued)

        saved, failures = await self._work_queue()
        if saved or failures:
            log.info('WeChat bridge: saved %d item(s), %d failed', saved, len(failures))
        if failures:
            msg = f'Could not save {len(failures)} WeChat item(s): {", ".join(failures)}'
            problems.append(WechatInboxError(msg, notification_dedupe_key='wechat:work'))

        try:
            await self._drain_inbox()
        except WechatInboxError as exc:
            problems.append(exc)

        if problems:
            message = '; '.join(dict.fromkeys(str(problem) for problem in problems))
            raise WechatInboxError(message, notification_dedupe_key=problems[0].notification_dedupe_key)
