"""Images, stickers, 公众号 articles, Tieba and NGA posts and 视频号 videos forwarded to the agent's WeChat account.

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
worth keeping -- images, stickers and shared articles received in a private chat -- in
``wechat_events``, so the cursor moves on even when saving an item fails. It then
works that queue. An image is fetched through the bridge, or from the inbox when
the bridge cannot. A sticker comes straight from WeChat's CDN (see
``wechat_emoji``) and is kept beside the images. A shared link is first opened
in the agent's WeChat, as its reader would before anything is saved: the service's ``POST /link/open`` opens
it, waits for it to load and closes it again. Then an article is fetched
straight from mp.weixin.qq.com with every image at full size. Image posts, which the site puts behind a captcha,
come from the service's ``POST /post/page`` instead: it has the agent's WeChat
open the post and returns the page from WeChat's web view cache. WeChat opens
nothing on command: the plugin taps through its screens to the message and taps
that, as a person would, so each of these requests names the message. A shared Tieba
post is saved the same way as an article, from the Tieba client API, with its
videos next to its pictures (see ``tieba_post``), and so is a shared NGA thread,
read with the NGA login of a CookieCloud vault (see ``nga_post``). A 视频号 video is not
opened in WeChat at all: TikHub looks it up and the file comes from WeChat's CDN, its
start decrypted here (see ``wechat_channels``).
A failed item is retried on later runs, up to ``_MAX_ATTEMPTS`` times. Last, the run drains the
inbox as it always has, which catches what the event log misses: the plugin only
logs while it is loaded.

Items are keyed on WeChat's local ``msg_id`` together with the message's
``create_time``. The id alone is a row number in the account's message database,
so it restarts if WeChat is reinstalled; the pair stays unique across that. The
``md5`` WeChat reports for an image does not hash the saved bytes, so it is kept for
reference and never used to deduplicate.
"""

from __future__ import annotations

import asyncio
import json
import shutil
from dataclasses import replace
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, NamedTuple

import httpx

from src.core import logger, settings
from src.tool import database
from src.tool.cookiecloud import NGA_PROFILE, CookieCloudClient
from src.tool.filename import ensure_unique_path, sanitize
from src.tool.notifications import enqueue_notification
from src.web.nga_post import LOGIN_COOKIES, NgaError, NgaLoginError, fetch_nga_post, nga_thread_id, parse_nga_link
from src.web.tieba_post import TiebaError, fetch_tieba_post, parse_tieba_link, thread_id
from src.web.wechat_article import (
    ArticleLink,
    ArticlePage,
    PostVideo,
    canonical_url,
    cover_only_page,
    image_extension,
    is_captcha,
    original_image_url,
    parse_article_link,
    parse_article_page,
)
from src.web.wechat_channels import (
    ENCRYPTED_BYTES,
    ChannelVideo,
    VideoSource,
    decrypt_head,
    parse_channel_video,
    parse_video_source,
    tikhub_lookup,
    video_title,
)
from src.web.wechat_emoji import Sticker, decrypt_sticker, matches, parse_sticker

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
# How long it waits for WeChat to load any other link it opened before closing it.
_LINK_WAIT_SECONDS = 20
# How long WeChat may take to reach a message before opening it: it scrolls the
# chat back as far as the message is, however old.
_TAP_OPEN_SECONDS = 1200
_CURSOR_KEY = 'events_after'
# What work_for() recognises; see there.
_WORK_VERSION = '7'
_WORK_VERSION_KEY = 'work_version'
_NGA_LOGIN_KEY = 'wechat:nga-login'
_TIKHUB_KEY = 'wechat:tikhub'
_TIKHUB_VIDEO_DETAIL = '/api/v1/wechat_channels/v2/fetch_video_detail'
# TikHub asks WeChat while the request waits and bills a lookup even when the
# client gave up on it, so give it a generous minute.
_TIKHUB_READ_SECONDS = 60
# TikHub's answers to a missing or spent key: settings to fix, not failed tries.
_TIKHUB_ACCOUNT_STATUSES = frozenset({401, 402, 403})
# How long a post's video may take to download from its CDN.
_POST_VIDEO_READ_SECONDS = 600
# A phone browser, as the share link would open in. mp.weixin.qq.com serves the
# full page to it without a login.
_ARTICLE_USER_AGENT = 'Mozilla/5.0 (Linux; Android 12) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36'

WORK_IMAGE = 'image'
WORK_STICKER = 'sticker'
WORK_ARTICLE = 'article'
WORK_CHANNEL_VIDEO = 'channel_video'


class _FetchedVideo(NamedTuple):
    """A post's video, downloaded beside the folder it goes in, and its cover."""

    file: Path
    cover: tuple[bytes, str] | None


class _FetchedMedia(NamedTuple):
    """An article's images and videos as downloaded, in page order; None for one that failed."""

    images: list[tuple[bytes, str] | None]
    videos: list[_FetchedVideo | None]


class _SavedArticle(NamedTuple):
    """Where an article was written, with an entry per image and per video."""

    local_path: Path
    images: list[dict[str, Any]]
    videos: list[dict[str, Any]]


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
    """What a bridge event asks of this source: an image, a sticker, an article, a 视频号 video, or nothing.

    Articles are 公众号 articles, Tieba posts and NGA threads. Whenever this learns a new kind
    of message, bump ``_WORK_VERSION`` so the event log is read again from the
    start and messages skipped before are picked up.

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
    content = str(message.get('content') or '')
    # A large sticker is an app message rather than an emoji one, so the content decides.
    if parse_sticker(content) is not None:
        work = WORK_STICKER
    elif shared_link(content) is not None:
        work = WORK_ARTICLE
    else:
        work = WORK_CHANNEL_VIDEO if parse_channel_video(content) is not None else None
    return work


def shared_link(content: str) -> ArticleLink | None:
    """The article or forum post a link card shares, if it is one this source saves."""
    return parse_article_link(content) or parse_tieba_link(content) or parse_nga_link(content)


def queued_message(message: dict[str, Any], work: str) -> dict[str, Any]:
    """The part of a message event worth keeping in the queue.

    An image message's content and ``image`` bean carry its CDN address and AES
    key; the bridge finds the image by id, so neither is stored.
    """
    kept = {key: message.get(key) for key in ('msg_id', 'create_time', 'type', 'kind', 'talker', 'sender')}
    if work in {WORK_ARTICLE, WORK_CHANNEL_VIDEO, WORK_STICKER}:
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


def video_path(message: dict[str, Any], video: ChannelVideo) -> Path:
    """Where a 视频号 video is stored: named like an article's folder, as one file."""
    when = sent_at(message['create_time'])
    name = ' '.join(
        part for part in (f'{when:%Y-%m-%d %H%M%S}', sanitize(video_title(video), max_bytes=120), f'[{message["msg_id"]}]') if part
    )
    return Path(when.strftime('%Y-%m')) / f'{name}.mp4'


def open_target(message: dict[str, Any], texts: tuple[str, ...]) -> dict[str, Any]:
    """The message the FavInbox service has WeChat tap open, with what its bubble may show."""
    return {
        'talker': str(message.get('talker') or ''),
        'msg_id': message['msg_id'],
        'create_time': message['create_time'],
        'texts': [text for text in dict.fromkeys(texts) if text],
    }


def _file_head(path: Path) -> bytes:
    with path.open('rb') as fh:
        return fh.read(16)


def _decrypt_file_head(path: Path, decode_key: int) -> None:
    """Undo WeChat's encryption of the start of a downloaded 视频号 video, in place."""
    with path.open('r+b') as file:
        head = file.read(ENCRYPTED_BYTES)
        file.seek(0)
        file.write(decrypt_head(head, decode_key))


def _is_mp4(head: bytes) -> bool:
    return head[4:8] == b'ftyp'


async def _write_stream(response: httpx.Response, path: Path) -> None:
    """Write a streamed response body to ``path``, creating its folder."""
    await asyncio.to_thread(path.parent.mkdir, parents=True, exist_ok=True)
    fh = await asyncio.to_thread(path.open, 'wb')
    try:
        async for chunk in response.aiter_bytes():
            await asyncio.to_thread(fh.write, chunk)
    finally:
        await asyncio.to_thread(fh.close)


def post_video_name(index: int, count: int) -> str:
    """A post's video file name without its extension: ``video``, or ``video-01`` and on when there are several."""
    return 'video' if count == 1 else f'video-{index:02d}'


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
        self._sender_names: dict[str, str] = {}
        self._nga_login: dict[str, str] | None = None

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
            CREATE TABLE IF NOT EXISTS wechat_channel_video (
                msg_id BIGINT NOT NULL,
                create_time BIGINT NOT NULL,
                talker TEXT NOT NULL DEFAULT '',
                sender TEXT NOT NULL DEFAULT '',
                sent_at TEXT NOT NULL DEFAULT '',
                object_id TEXT NOT NULL DEFAULT '',
                title TEXT NOT NULL DEFAULT '',
                author TEXT NOT NULL DEFAULT '',
                duration INTEGER NOT NULL DEFAULT 0,
                size BIGINT,
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
        await self._save_state(_CURSOR_KEY, str(seq))

    async def _save_state(self, key: str, value: str) -> None:
        await database.query_db(
            'INSERT INTO wechat_state (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value;',
            (key, value),
        )

    async def _rescan_for_new_work(self) -> None:
        """Read the event log from the start once work_for() recognises more than it did."""
        rows = await database.query_db('SELECT value FROM wechat_state WHERE key = ?;', (_WORK_VERSION_KEY,))
        if rows and rows[0]['value'] == _WORK_VERSION:
            return
        if await self._cursor():
            # Already-queued messages keep their row: the queue's key makes a re-read harmless.
            log.info('WeChat source learned new kinds of messages; reading the bridge event log again from the start')
            await self._save_cursor(0)
        await self._save_state(_WORK_VERSION_KEY, _WORK_VERSION)

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

    async def _queue(self, event: dict[str, Any], work: str) -> bool:
        """Queue one message; False when it was queued before."""
        message = event['data']
        rows = await database.query_db(
            """
            INSERT INTO wechat_events (msg_id, create_time, work, seq, message)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT (msg_id, create_time) DO NOTHING
            RETURNING msg_id;
            """,
            (message['msg_id'], message['create_time'], work, event['seq'], json.dumps(queued_message(message, work), ensure_ascii=False)),
        )
        return bool(rows)

    async def _read_events(self) -> int:
        """Queue the new events worth keeping and move the cursor past all of them."""
        await self._rescan_for_new_work()
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
                if work is not None and await self._queue(event, work):
                    queued += 1
            cursor = max(event['seq'] for event in events)
            await self._save_cursor(cursor)
            if len(events) < _EVENT_PAGE_SIZE or cursor >= last_seq:
                return queued

    async def _pending_work(self) -> list[dict[str, Any]]:
        return await database.query_db(
            "SELECT msg_id, create_time, work, message, attempts FROM wechat_events WHERE status = 'pending' ORDER BY create_time, msg_id;",
        )

    async def _finish_work(self, row: dict[str, Any], error: str = '', *, counted: bool = True) -> bool:
        """Record the outcome of one queued item; True when it will not be tried again.

        An error that is not ``counted`` leaves the attempts alone: the item waits
        for a setting to be fixed rather than running out of tries.
        """
        attempts = int(row['attempts']) + (1 if error and counted else 0)
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
                elif row['work'] == WORK_CHANNEL_VIDEO:
                    await self._save_channel_video(message)
                elif row['work'] == WORK_STICKER:
                    await self._save_sticker(message)
                else:
                    await self._save_bridge_image(message)
            except (httpx.HTTPError, OSError, WechatInboxError) as exc:
                error = str(exc) or type(exc).__name__
                log.warning('Failed to save WeChat %s %s: %s', row['work'], row['msg_id'], error)
                # A missing NGA login or TikHub key is a setting to fix, not a failed try.
                waits = isinstance(exc, WechatInboxError) and exc.notification_dedupe_key in {_NGA_LOGIN_KEY, _TIKHUB_KEY}
                gave_up = await self._finish_work(row, error, counted=not waits)
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

    async def _sender_name(self, wxid: str) -> str:
        """How the account knows a sender: the remark it gave them, else their nickname, else the wxid."""
        if not wxid:
            return 'unknown'
        if wxid not in self._sender_names:
            name = ''
            try:
                response = await self.client.get(self._url('/wx/contact'), params={'wxid': wxid})
                if response.status_code == httpx.codes.OK:
                    contact = response.json()
                    if isinstance(contact, dict):
                        name = next((str(contact[key]).strip() for key in ('remark_name', 'name', 'nick_name') if contact.get(key)), '')
            except (httpx.HTTPError, ValueError) as exc:
                log.debug('Could not look up WeChat contact %s: %s', wxid, exc)
            self._sender_names[wxid] = name or wxid
        return self._sender_names[wxid]

    async def _notify_download(self, item: dict[str, Any], local_path: Path) -> None:
        """One message per image, sent as the photo itself, as the Telegram source does."""
        saved_path = self.cfg.path / local_path
        sender = await self._sender_name(str(item.get('sender') or item.get('talker') or ''))
        try:
            await enqueue_notification(
                kind='download_completed',
                source='wechat',
                header='WeChat',
                title=local_path.name,
                body=f'From {sender} | Message ID {item["msg_id"]}',
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

    # ---------- stickers ----------

    async def _sticker_file(self, sticker: Sticker) -> tuple[bytes, str]:
        """The sticker as WeChat's CDN serves it, else its encrypted copy decrypted."""
        reasons: list[str] = []
        if sticker.cdn_url:
            try:
                response = await self.web.get(sticker.cdn_url)
                response.raise_for_status()
            except httpx.HTTPError as exc:
                reasons.append(f'cdnurl: {exc}')
            else:
                if matches(sticker, response.content):
                    return response.content, response.headers.get('content-type', '')
                reasons.append('cdnurl: the file does not match its md5')
        if sticker.encrypted_url and sticker.aes_key:
            try:
                response = await self.web.get(sticker.encrypted_url)
                response.raise_for_status()
                content = decrypt_sticker(response.content, sticker.aes_key)
            except httpx.HTTPError as exc:
                reasons.append(f'encrypturl: {exc}')
            except ValueError:
                reasons.append('encrypturl: the key does not decrypt the file')
            else:
                if matches(sticker, content):
                    return content, ''
                reasons.append('encrypturl: the file does not match its md5')
        msg = f'Could not download the sticker: {"; ".join(reasons)}'
        raise WechatInboxError(msg)

    async def _save_sticker(self, message: dict[str, Any]) -> None:
        sticker = parse_sticker(str(message.get('content') or ''))
        if sticker is None:
            msg = f'Message {message["msg_id"]} names no sticker file'
            raise WechatInboxError(msg)
        item = {**image_item(message), 'type': 'sticker', 'md5': sticker.md5}
        if await self._already_stored(item):
            return
        content, content_type = await self._sticker_file(sticker)
        await self._keep_image(item, content, content_type)

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
            if response.headers.get('error-message'):
                # Tieba's answer to an expired grant: its logo, with this header.
                log.info('Article image %s refused: %s', candidate, response.headers['error-message'])
                continue
            if response.content and content_type.lower().startswith('image/'):
                return response.content, sniff_extension(response.content) or image_extension(candidate, content_type)
        return None

    def _write_article(
        self,
        message: dict[str, Any],
        link: ArticleLink,
        page: ArticlePage,
        page_html: str,
        media: _FetchedMedia,
    ) -> _SavedArticle:
        """Write the images, the videos, the page and a summary into the article's folder, all or nothing."""
        title = page.title or link.title
        destination = _unused_directory(self.cfg.path / article_directory(message, title))
        partial = destination.with_name(f'{destination.name}.part')
        if partial.exists():
            shutil.rmtree(partial)
        partial.mkdir(parents=True)
        width = max(2, len(str(len(media.images))))
        entries: list[dict[str, Any]] = []
        for index, (source, fetched) in enumerate(zip(page.images, media.images, strict=True), start=1):
            entry: dict[str, Any] = {'url': source}
            if fetched is not None:
                content, ext = fetched
                entry['file'] = f'{index:0{width}d}.{ext}'
                (partial / entry['file']).write_bytes(content)
            entries.append(entry)
        video_entries: list[dict[str, Any]] = []
        for index, (video, fetched_video) in enumerate(zip(page.videos, media.videos, strict=True), start=1):
            video_entry: dict[str, Any] = {'url': video.url}
            if fetched_video is not None:
                name = post_video_name(index, len(media.videos))
                video_entry['file'] = f'{name}.mp4'
                fetched_video.file.replace(partial / video_entry['file'])
                if fetched_video.cover is not None:
                    content, ext = fetched_video.cover
                    video_entry['cover'] = f'{name}.{ext}'
                    (partial / video_entry['cover']).write_bytes(content)
            video_entries.append(video_entry)
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
        if video_entries:
            summary['videos'] = video_entries
        (partial / 'article.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding='utf-8')
        partial.replace(destination)
        return _SavedArticle(destination.relative_to(self.cfg.path), entries, video_entries)

    async def _open_link(self, message: dict[str, Any], link: ArticleLink) -> None:
        """Open the link in the agent's WeChat, let it load and close it again, as its reader would."""
        try:
            response = await self.client.post(
                self._url('/link/open'),
                json={'url': link.url, 'message': open_target(message, (link.title,)), 'wait': _LINK_WAIT_SECONDS},
                timeout=httpx.Timeout(_REQUEST_TIMEOUT_SECONDS, read=_TAP_OPEN_SECONDS + _LINK_WAIT_SECONDS + _REQUEST_TIMEOUT_SECONDS),
            )
        except httpx.HTTPError as exc:
            msg = f'FavInbox could not open the link in WeChat: {exc}'
            raise WechatInboxError(msg) from exc
        if response.status_code == httpx.codes.UNAUTHORIZED:
            raise self._unauthorized()
        if response.status_code != httpx.codes.OK:
            msg = f'FavInbox could not open the link in WeChat: HTTP {response.status_code}{_error_detail(response)}'
            raise WechatInboxError(msg)
        try:
            loaded = bool(response.json().get('loaded'))
        except (ValueError, AttributeError):
            loaded = False
        if not loaded:
            # Only a missing cache entry, not a failure: some pages are never cached.
            log.info('WeChat opened %s but did not report it loaded within %ds', canonical_url(link.url), _LINK_WAIT_SECONDS)

    async def _page_in_wechat(self, message: dict[str, Any], link: ArticleLink) -> tuple[ArticlePage, str]:
        """An image post as the agent's WeChat loads it, through the FavInbox service."""
        try:
            response = await self.client.post(
                self._url('/post/page'),
                json={'url': link.url, 'message': open_target(message, (link.title,)), 'wait': _OPEN_WAIT_SECONDS},
                timeout=httpx.Timeout(_REQUEST_TIMEOUT_SECONDS, read=_TAP_OPEN_SECONDS + _OPEN_WAIT_SECONDS + _REQUEST_TIMEOUT_SECONDS),
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

    async def _image_post(self, message: dict[str, Any], link: ArticleLink, *, final_attempt: bool) -> tuple[ArticlePage, str]:
        """An image post the site put behind its captcha: all of it from WeChat, else its first picture."""
        cover = cover_only_page(link)
        if cover is None:
            msg = f'mp.weixin.qq.com answered {canonical_url(link.url)} with a captcha page'
            raise WechatInboxError(msg)
        try:
            return await self._page_in_wechat(message, link)
        except WechatInboxError as exc:
            # Retried while attempts remain; the first picture beats nothing at all.
            if not final_attempt or exc.notification_dedupe_key == 'wechat:auth':
                raise
            log.warning('WeChat image post %s: %s; saving its first picture only', canonical_url(link.url), exc)
            return cover, ''

    async def _article_page(self, message: dict[str, Any], link: ArticleLink, *, final_attempt: bool) -> tuple[ArticlePage, str]:
        """The shared page and its HTML, if any was kept: a 公众号 article, a Tieba post or an NGA thread."""
        if thread_id(link.url) is not None:
            try:
                return await fetch_tieba_post(self.web, link), ''
            except TiebaError as exc:
                raise WechatInboxError(str(exc)) from exc
        if nga_thread_id(link.url) is not None:
            try:
                return await fetch_nga_post(self.web, link, await self._nga_cookies()), ''
            except NgaLoginError as exc:
                raise WechatInboxError(str(exc), notification_dedupe_key=_NGA_LOGIN_KEY) from exc
            except NgaError as exc:
                raise WechatInboxError(str(exc)) from exc
        response = await self.web.get(link.url)
        if is_captcha(str(response.url)):
            return await self._image_post(message, link, final_attempt=final_attempt)
        response.raise_for_status()
        return parse_article_page(response.text), response.text

    def _read_nga_cookies(self) -> dict[str, str]:
        name = self.cfg.nga_cookiecloud.strip()
        if not name:
            msg = 'An NGA thread needs an NGA login: pick the CookieCloud config that carries it in the WeChat settings'
            raise WechatInboxError(msg, notification_dedupe_key=_NGA_LOGIN_KEY)
        try:
            vault = settings.resolve_cookiecloud(name)
        except ValueError as exc:
            raise WechatInboxError(str(exc), notification_dedupe_key=_NGA_LOGIN_KEY) from exc
        client = CookieCloudClient(vault.server_url, vault.uuid, vault.password)
        try:
            jar = client.get_cookies()
        except (ConnectionError, ValueError, KeyError) as exc:
            msg = f'Could not read CookieCloud config {name!r}: {exc}'
            raise WechatInboxError(msg, notification_dedupe_key=_NGA_LOGIN_KEY) from exc
        finally:
            client.client.close()
        wanted = {cookie.lower(): cookie for cookie in LOGIN_COOKIES}
        cookies: dict[str, str] = {}
        for domain, entries in jar.items():
            if domain.lstrip('.') not in NGA_PROFILE.domains:
                continue
            for entry in entries:
                cookie = wanted.get(str(entry.get('name') or '').lower()) if isinstance(entry, dict) else None
                if cookie and entry.get('value'):
                    cookies.setdefault(cookie, str(entry['value']))
        if len(cookies) < len(LOGIN_COOKIES):
            msg = f'CookieCloud config {name!r} has no NGA login; sign in to NGA in the browser and re-sync the extension'
            raise WechatInboxError(msg, notification_dedupe_key=_NGA_LOGIN_KEY)
        return cookies

    async def _nga_cookies(self) -> dict[str, str]:
        """The NGA login from CookieCloud, read once a run, so it tracks the browser."""
        if self._nga_login is None:
            self._nga_login = await asyncio.to_thread(self._read_nga_cookies)
        return self._nga_login

    async def _save_article(self, message: dict[str, Any], *, final_attempt: bool = False) -> None:
        content = str(message.get('content') or '')
        link = shared_link(content)
        if link is None:
            msg = f'message {message["msg_id"]} is not an article share'
            raise WechatInboxError(msg)
        if await self._article_stored(message, link):
            return
        await self._open_link(message, link)
        page, page_html = await self._article_page(message, link, final_attempt=final_attempt)
        if not page.title and not page.images and not page.videos:
            msg = f'{canonical_url(link.url)} has no title, images or videos; the article may have been deleted'
            raise WechatInboxError(msg)

        images = [await self._article_image(url) for url in page.images]
        saved = sum(1 for image in images if image is not None)
        if page.images and not saved:
            msg = f'none of the {len(page.images)} images of {canonical_url(link.url)} could be downloaded'
            raise WechatInboxError(msg)
        staging = self.cfg.path / article_directory(message, page.title or link.title)
        videos: list[_FetchedVideo | None] = []
        try:
            for index, video in enumerate(page.videos, start=1):
                videos.append(await self._post_video(video, staging.with_name(f'{staging.name}.video-{index}.part')))
            if page.videos and not any(videos):
                msg = f'none of the {len(page.videos)} videos of {canonical_url(link.url)} could be downloaded'
                raise WechatInboxError(msg)
            article = await asyncio.to_thread(self._write_article, message, link, page, page_html, _FetchedMedia(images, videos))
        finally:
            # Whatever the folder did not take in.
            for fetched in videos:
                if fetched is not None:
                    await asyncio.to_thread(fetched.file.unlink, missing_ok=True)
        await self._record_article(message, link, page, article)
        counts = f'{saved} of {len(images)} images'
        if videos:
            counts += f', {sum(1 for video in videos if video is not None)} of {len(videos)} videos'
        log.info('Saved WeChat article %s (%s) as %s', message['msg_id'], counts, article.local_path)
        await self._notify_article(message, link, page, article)

    async def _post_video(self, video: PostVideo, partial: Path) -> _FetchedVideo | None:
        """Download one of a post's videos to ``partial``, with its cover; None when it does not load as an MP4."""
        try:
            async with self.web.stream(
                'GET', video.url, timeout=httpx.Timeout(_REQUEST_TIMEOUT_SECONDS, read=_POST_VIDEO_READ_SECONDS)
            ) as response:
                response.raise_for_status()
                await _write_stream(response, partial)
        except httpx.HTTPError as exc:
            log.info('Post video %s failed: %s', video.url, exc)
            await asyncio.to_thread(partial.unlink, missing_ok=True)
            return None
        if not _is_mp4(await asyncio.to_thread(_file_head, partial)):
            log.info('Post video %s is not an MP4', video.url)
            await asyncio.to_thread(partial.unlink, missing_ok=True)
            return None
        cover = await self._article_image(video.cover_url) if video.cover_url else None
        return _FetchedVideo(partial, cover)

    async def _record_article(
        self,
        message: dict[str, Any],
        link: ArticleLink,
        page: ArticlePage,
        article: _SavedArticle,
    ) -> None:
        entries, video_entries = article.images, article.videos
        metadata: dict[str, Any] = {
            'share_url': link.url,
            'description': link.description,
            'picture_count': max(page.picture_count, len(entries)),
            'images': entries,
        }
        if video_entries:
            metadata['videos'] = video_entries
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
                str(article.local_path),
                json.dumps(metadata, ensure_ascii=False),
            ),
        )

    async def _notify_article(
        self,
        message: dict[str, Any],
        link: ArticleLink,
        page: ArticlePage,
        article: _SavedArticle,
    ) -> None:
        """One message per article, linking to it and showing its first image, else its first video's cover."""
        local_path, entries, video_entries = article
        files = [entry['file'] for entry in entries if 'file' in entry]
        folder = self.cfg.path / local_path
        counts: list[str] = []
        total = max(page.picture_count, len(entries))
        if total or not video_entries:
            noun = 'image' if total == 1 else 'images'
            count = f'{len(files)} {noun}' if len(files) == total else f'{len(files)} of {total} {noun}'
            if page.picture_count > len(entries):
                count += ' (WeChat could not open the rest)'
            counts.append(count)
        if video_entries:
            videos = sum(1 for entry in video_entries if 'file' in entry)
            noun = 'video' if len(video_entries) == 1 else 'videos'
            counts.append(f'{videos} {noun}' if videos == len(video_entries) else f'{videos} of {len(video_entries)} {noun}')
        sender = str(message.get('sender') or message.get('talker') or '')
        payload: dict[str, Any] = {'msg_id': message['msg_id'], 'sender': sender, 'saved_path': str(folder)}
        name = await self._sender_name(sender)
        covers = [entry['cover'] for entry in video_entries if 'cover' in entry]
        if files or covers:
            payload['image_path'] = str(folder / (files or covers)[0])
        try:
            await enqueue_notification(
                kind='download_completed',
                source='wechat',
                header='WeChat',
                title=page.title or link.title or local_path.name,
                link_url=canonical_url(link.url),
                body=' | '.join(part for part in (page.account or link.account, *counts, f'From {name}') if part),
                payload=payload,
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue wechat article notification for message %s: %s', message['msg_id'], exc)

    # ---------- 视频号 videos ----------

    async def _video_stored(self, message: dict[str, Any], video: ChannelVideo) -> bool:
        """Whether this message, or an earlier forward of the same video, was saved already."""
        rows = await database.query_db(
            'SELECT 1 FROM wechat_channel_video WHERE (msg_id = ? AND create_time = ?) OR object_id = ?;',
            (message['msg_id'], message['create_time'], video.object_id),
        )
        return bool(rows)

    async def _video_source(self, video: ChannelVideo) -> VideoSource:
        """Where the video file is and how to decrypt it, as TikHub looks it up."""
        cfg = self.cfg
        if not cfg.tikhub_token:
            msg = '视频号 videos need a TikHub API key (web.wechat.tikhub_token)'
            raise WechatInboxError(msg, notification_dedupe_key=_TIKHUB_KEY)
        failure = f'TikHub could not look up 视频号 video {video.object_id}'
        try:
            response = await self.web.post(
                f'{cfg.tikhub_api_url}{_TIKHUB_VIDEO_DETAIL}',
                json=tikhub_lookup(video),
                headers={'Authorization': f'Bearer {cfg.tikhub_token}'},
                timeout=httpx.Timeout(_REQUEST_TIMEOUT_SECONDS, read=_TIKHUB_READ_SECONDS),
            )
        except httpx.HTTPError as exc:
            msg = f'{failure}: {exc}'
            raise WechatInboxError(msg) from exc
        if response.status_code in _TIKHUB_ACCOUNT_STATUSES:
            msg = f'{failure}: HTTP {response.status_code}{_error_detail(response)}; check the TikHub key and balance'
            raise WechatInboxError(msg, notification_dedupe_key=_TIKHUB_KEY)
        if response.status_code != httpx.codes.OK:
            msg = f'{failure}: HTTP {response.status_code}{_error_detail(response)}'
            raise WechatInboxError(msg)
        try:
            source = parse_video_source(response.json())
        except ValueError:
            source = None
        if source is None:
            msg = f'{failure}: the answer names no video file and decode key'
            raise WechatInboxError(msg)
        return source

    async def _download_video(self, video: ChannelVideo, partial: Path) -> int:
        """Download the video into ``partial``, decrypt its start, and return its size."""
        source = await self._video_source(video)
        failure = f'Could not download 视频号 video {video.object_id}'
        try:
            async with self.web.stream(
                'GET', source.url, timeout=httpx.Timeout(_REQUEST_TIMEOUT_SECONDS, read=_POST_VIDEO_READ_SECONDS)
            ) as response:
                response.raise_for_status()
                await _write_stream(response, partial)
        except httpx.HTTPError as exc:
            # A transfer cut short leaves a partial file nothing else would clean up.
            await asyncio.to_thread(partial.unlink, missing_ok=True)
            msg = f'{failure}: {exc}'
            raise WechatInboxError(msg) from exc
        await asyncio.to_thread(_decrypt_file_head, partial, source.decode_key)
        if not _is_mp4(await asyncio.to_thread(_file_head, partial)):
            await asyncio.to_thread(partial.unlink, missing_ok=True)
            msg = f'{failure}: it is not an MP4 once decrypted'
            raise WechatInboxError(msg)
        return (await asyncio.to_thread(partial.stat)).st_size

    async def _video_cover(self, video: ChannelVideo) -> tuple[bytes, str] | None:
        if not video.cover_url:
            return None
        try:
            response = await self.web.get(video.cover_url)
            response.raise_for_status()
        except httpx.HTTPError as exc:
            log.info('视频号 cover of %s failed: %s', video.object_id, exc)
            return None
        ext = sniff_extension(response.content)
        if not response.content or ext is None:
            return None
        return response.content, ext

    async def _save_channel_video(self, message: dict[str, Any]) -> None:
        video = parse_channel_video(str(message.get('content') or ''))
        if video is None:
            msg = f'message {message["msg_id"]} is not a 视频号 video share'
            raise WechatInboxError(msg)
        if await self._video_stored(message, video):
            return
        destination = await asyncio.to_thread(ensure_unique_path, self.cfg.path / video_path(message, video))
        partial = destination.with_name(f'{destination.name}.part')
        size = await self._download_video(video, partial)
        await asyncio.to_thread(partial.replace, destination)
        local_path = destination.relative_to(self.cfg.path)
        cover_path: Path | None = None
        cover = await self._video_cover(video)
        if cover is not None:
            cover_path = destination.with_suffix(f'.{cover[1]}')
            await asyncio.to_thread(cover_path.write_bytes, cover[0])
        await self._record_video(message, video, local_path, size)
        log.info('Saved WeChat 视频号 video %s (%s, %d bytes) as %s', message['msg_id'], video.object_id, size, local_path)
        await self._notify_video(message, video, destination, size, cover_path)

    async def _record_video(self, message: dict[str, Any], video: ChannelVideo, local_path: Path, size: int) -> None:
        await database.query_db(
            """
            INSERT INTO wechat_channel_video (
                msg_id, create_time, talker, sender, sent_at, object_id, title, author, duration, size, local_path, metadata
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (msg_id, create_time) DO NOTHING;
            """,
            (
                message['msg_id'],
                message['create_time'],
                str(message.get('talker') or ''),
                str(message.get('sender') or message.get('talker') or ''),
                sent_at(message['create_time']).strftime('%Y-%m-%d %H:%M:%S'),
                video.object_id,
                video_title(video),
                video.author,
                video.duration,
                size,
                str(local_path),
                json.dumps(
                    {'nonce_id': video.nonce_id, 'description': video.description, 'width': video.width, 'height': video.height},
                    ensure_ascii=False,
                ),
            ),
        )

    async def _notify_video(self, message: dict[str, Any], video: ChannelVideo, saved: Path, size: int, cover: Path | None) -> None:
        """One message per video, showing its cover."""
        sender = str(message.get('sender') or message.get('talker') or '')
        payload: dict[str, Any] = {'msg_id': message['msg_id'], 'sender': sender, 'saved_path': str(saved)}
        if cover is not None:
            payload['image_path'] = str(cover)
        parts = (
            video.author,
            f'{video.duration}s' if video.duration else '',
            f'{size / 1_000_000:.1f} MB',
            f'From {await self._sender_name(sender)}',
        )
        try:
            await enqueue_notification(
                kind='download_completed',
                source='wechat',
                header='WeChat',
                title=video_title(video),
                body=' | '.join(part for part in parts if part),
                payload=payload,
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue wechat video notification for message %s: %s', message['msg_id'], exc)

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
