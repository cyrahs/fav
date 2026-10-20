#!/usr/bin/env python3
"""Tiny read/ack HTTP API over the FavInbox directory on the oracle host.

The WAuxiliary FavInbox plugin drops `<msgId>.<ext>` + `<msgId>.json` (and, on a
failed download, `<msgId>.err`) into a directory the redroid container shares with
the host. This serves that directory to the fav crawler over the frpc tunnel:

    GET  /health            -> {"ok": true}
    GET  /items             -> [{msg_id, type, status, file, size, ...}, ...]
    GET  /file/<msg_id>     -> the image bytes
    POST /ack/<msg_id>      -> mark the item acked and delete its media file
    *    /wx/<path>         -> forwarded to the plugin's bridge API (see README)
    POST /link/open         -> open a link in WeChat, wait for it to load, close it
    POST /post/page         -> a 公众号 post's page as WeChat itself loaded it
    POST /finder/video      -> a 视频号 video as WeChat itself downloaded it

Items are listed from the JSON sidecars. `ack` keeps the JSON as a permanent
dedupe tombstone (status set to "acked") and deletes only the media file, so the
plugin's backfill never re-saves an item fav already has, while the disk is
reclaimed. `?include_acked=1` lists acked items too.

No third-party dependencies: the oracle host has only the standard library.
Auth is a bearer token from FAVINBOX_TOKEN; the service refuses to start without
one. It binds to 127.0.0.1 so only the frpc tunnel (and localhost) can reach it.

Env:
    FAVINBOX_DIR    directory to serve (required)
    FAVINBOX_TOKEN  bearer token required on every request (required)
    FAVINBOX_HOST   bind address (default 127.0.0.1)
    FAVINBOX_PORT   bind port (default 9970)
    FAVINBOX_BRIDGE_URL         the plugin's bridge inside redroid
                                (default http://172.17.0.2:9971)
    FAVINBOX_BRIDGE_TOKEN_FILE  the token the plugin wrote for its bridge
                                (default: FavBridge/token next to FAVINBOX_DIR)
    FAVINBOX_XWEB_CACHE         WeChat's XWeb cache, in the app's internal data
                                directory (default /home/ubuntu/redroid-data/data/
                                com.tencent.mm/cache/xweb_cache)
    FAVINBOX_WECHAT_DATA        WeChat's internal data directory, for /finder/video
                                (default /home/ubuntu/redroid-data/data/com.tencent.mm)

`/wx/*` takes the same bearer token as everything else here, then calls the
bridge with the plugin's own token, so fav never needs that second secret. The
token file is re-read on every request because the plugin creates it on first
load and may be reinstalled.

Opening something in WeChat is done the way a person does it, with taps: the
plugin's `POST /ui/open` goes back to the chat list, taps the chat the message
is in, finds the message's bubble and taps it; `POST /ui/close` taps Back until
the chat shows again, where the next message of that chat is a few swipes away
(after a failed open, until the chat list shows). Every request that may open something therefore
names the message: `"message": {"talker": "<wxid>", "msg_id": <id>, "texts":
["<what its bubble shows>", ...]}`. Nothing here starts WeChat's activities or
uses adb.

`/link/open` opens a shared link the way a reader would before fav fetches it:
the body is JSON, `{"url": "<the link>", "message": {...}, "wait": <seconds,
default 20>}`. The page counts as loaded once the web view's HTTP cache holds a
complete response for the link's host and path, written after it was opened;
the page then stays up a few seconds more, and is closed. The answer is
`{"loaded": true|false, "seconds": <how long it was open>, "steps": [...]}`; a
page that never reaches the cache (one served uncacheable, or redirected
elsewhere) is closed after `wait` and answers `loaded: false`. Errors: 400 for
anything but an http(s) URL of printable ASCII or a missing message, 502 when
the message could not be opened (the plugin's steps are in the error).

`/post/page` exists for image posts (贴图): mp.weixin.qq.com answers their share
links with a slider captcha for any outside client, but WeChat opens them
normally with its login. The body is JSON, `{"url": "<the share link>",
"message": {...}, "wait": <seconds, default 30>}`. If WeChat's web view has the
post in its HTTP cache already, that copy is returned; otherwise the message is
opened in WeChat and the cache is polled until the page lands there. The answer
is the page's HTML (decompressed), or a JSON error: 400 for a link that is not a
`mp.weixin.qq.com/s?__biz=...&mid=...&idx=...&sn=...` post, 404 when the page
never reached the cache (or is not cached and no message was named), 502 when
the message could not be opened.

`/finder/video` does the same for 视频号 videos, whose CDN addresses need
WeChat's login and per-video keys. The body is JSON, `{"object_id": "<the
share's objectId>", "message": {...}, "wait": <seconds, default 120>}`. WeChat's video player keeps what it downloads as plain MP4 files
under `cache/<account>/finder/video/`, and indexes them by feed in the
account's `FinderMM*.db` (table `FinderMediaCacheInfoV4`: feedId, mediaId =
file name, totalSize). If the video is cached complete already, that file is
returned; otherwise the message is opened in WeChat and the index is polled
until the file is whole. Opening a video also preloads a few
recommended ones, which is why the file is found through the index rather than
by what appeared. The answer is the MP4, or a JSON error: 400 for a bad id, 404
when the video never finished downloading, 502 when the message could not be
opened, 503 when no logged-in account has a 视频号 database.
"""

from __future__ import annotations

import gzip
import hmac
import http.client
import json
import os
import re
import shutil
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time
import zlib
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

_MSG_ID_RE = re.compile(r'^\d+$')
_MEDIA_EXTS = ('jpg', 'png', 'gif', 'webp', 'wxgf', 'bin')
_CONTENT_TYPES = {
    'jpg': 'image/jpeg',
    'png': 'image/png',
    'gif': 'image/gif',
    'webp': 'image/webp',
    'wxgf': 'application/octet-stream',
    'bin': 'application/octet-stream',
}


def _inbox_dir() -> Path:
    raw = os.environ.get('FAVINBOX_DIR', '').strip()
    if not raw:
        sys.exit('FAVINBOX_DIR is required')
    path = Path(raw)
    if not path.is_dir():
        sys.exit(f'FAVINBOX_DIR is not a directory: {path}')
    return path


def _token() -> str:
    tok = os.environ.get('FAVINBOX_TOKEN', '')
    if not tok:
        sys.exit('FAVINBOX_TOKEN is required')
    return tok


DIR = _inbox_dir()
TOKEN = _token()
BRIDGE = urlsplit(os.environ.get('FAVINBOX_BRIDGE_URL', 'http://172.17.0.2:9971'))
BRIDGE_TOKEN_FILE = Path(os.environ.get('FAVINBOX_BRIDGE_TOKEN_FILE', '') or DIR.parent / 'FavBridge' / 'token')
# Long enough for an /events long-poll (60s at most) and a large image download.
BRIDGE_TIMEOUT_SECONDS = 180
# Long enough for the plugin to scroll a long chat back to an old message.
OPEN_TIMEOUT_SECONDS = 1200
_CHUNK = 65536

# Not beside FAVINBOX_DIR: the web view caches into WeChat's internal data
# directory (/data/data/com.tencent.mm), which redroid keeps under data/.
XWEB_CACHE = Path(os.environ.get('FAVINBOX_XWEB_CACHE', '') or '/home/ubuntu/redroid-data/data/com.tencent.mm/cache/xweb_cache')
WECHAT_DATA = Path(os.environ.get('FAVINBOX_WECHAT_DATA', '') or '/home/ubuntu/redroid-data/data/com.tencent.mm')
_POST_HOST = 'mp.weixin.qq.com'
_DEFAULT_WAIT_SECONDS = 30
_MAX_WAIT_SECONDS = 90
_POLL_SECONDS = 2
_LINK_RE = re.compile(r'https?://[\x21-\x7e]{1,2040}')
_DEFAULT_LINK_WAIT_SECONDS = 20
_MAX_LINK_WAIT_SECONDS = 60
# How long a loaded page stays up before Back closes it, as a reader would glance at it.
_LINK_DWELL_SECONDS = 3
# Only one post or video is opened at a time, so a page found in the cache is the one asked for.
_OPEN_LOCK = threading.Lock()
# After an open, WeChat is left in that chat so the next message of the chat is
# near. The chat it was left in (guarded by _OPEN_LOCK), and how long it may sit
# there unused before Back takes it to the chat list, so messages arriving later
# are not marked read.
_LEFT_IN_CHAT: dict[str, object] = {'talker': None, 'timer': None}
_IDLE_HOME_SECONDS = 120
_DEFAULT_VIDEO_WAIT_SECONDS = 120
_MAX_VIDEO_WAIT_SECONDS = 300
# A 视频号 object id is an unsigned 64-bit number in share messages and a signed one in WeChat.
_OBJECT_ID_RE = re.compile(r'-?\d{1,20}')
_TALKER_RE = re.compile(r'[A-Za-z0-9_@.-]{1,80}')
_MAX_TEXTS = 4
_MAX_TEXT_CHARS = 200
_ACCOUNT_RE = re.compile(r'[0-9a-f]{32}')
_MEDIA_ID_RE = re.compile(r'[A-Za-z0-9_]{1,128}')
_TABLE_RE = re.compile(r'FinderMediaCacheInfo\w*')
_MP4_BOX = struct.Struct('>I4s')
# The post's identity, checked before any of it goes near a shell.
_POST_FIELDS = {
    '__biz': re.compile(r'[A-Za-z0-9+/=]{4,64}'),
    'mid': re.compile(r'\d{1,20}'),
    'idx': re.compile(r'\d{1,4}'),
    'sn': re.compile(r'[0-9a-f]{8,64}'),
}

# Chromium's Simple Cache: an entry file `<hash>_0` starts with this header and the
# key (the URL, after a cache-partition prefix), then the response body exactly as
# received, an EOF record, the response headers and a final EOF record.
_SIMPLE_MAGIC = 0xFCFB6D1BA7725C30
_SIMPLE_HEADER = struct.Struct('<QIIII')
_SIMPLE_EOF = struct.pack('<Q', 0xF4FA6F45970D41D8)
_EOF_RECORD_SIZE = 24
_MAX_KEY_BYTES = 8192


def _post_id(url: str) -> dict[str, str] | None:
    """`__biz`, `mid`, `idx` and `sn` of a post link, or None if it is not one."""
    parts = urlsplit(url)
    if parts.scheme not in {'http', 'https'} or parts.hostname != _POST_HOST or parts.path != '/s':
        return None
    query = parse_qs(parts.query)
    post = {name: query.get(name, [''])[0] for name in _POST_FIELDS}
    if not all(pattern.fullmatch(post[name]) for name, pattern in _POST_FIELDS.items()):
        return None
    return post


def _same_post(key_url: str, post: dict[str, str]) -> bool:
    found = _post_id(key_url.replace('http://', 'https://', 1)) if '__biz=' in key_url else None
    if found is None:
        # The web view may load the page under a URL without `sn`; the rest still identifies it.
        query = parse_qs(urlsplit(key_url).query)
        found = {name: query.get(name, [''])[0] for name in ('__biz', 'mid', 'idx')}
    return all(found.get(name) == post[name] for name in ('__biz', 'mid', 'idx'))


def _cache_key(path: Path) -> str | None:
    with path.open('rb') as fh:
        header = fh.read(_SIMPLE_HEADER.size)
        if len(header) < _SIMPLE_HEADER.size:
            return None
        magic, _version, key_length, _hash, _pad = _SIMPLE_HEADER.unpack(header)
        if magic != _SIMPLE_MAGIC or not 0 < key_length <= _MAX_KEY_BYTES:
            return None
        return fh.read(key_length).decode('utf-8', 'replace')


def _decompress(body: bytes, encoding: str) -> bytes:
    if encoding in {'', 'identity'}:
        return body
    if encoding == 'gzip':
        return gzip.decompress(body)
    if encoding == 'deflate':
        return zlib.decompress(body)
    if encoding == 'br':
        try:
            import brotli  # noqa: PLC0415  (python3-brotli, not in the standard library)
        except ImportError:
            try:
                return subprocess.run(['brotli', '-d', '-c'], input=body, capture_output=True, check=True, timeout=30).stdout  # noqa: S607
            except (OSError, subprocess.SubprocessError) as exc:
                msg = 'the page is brotli-compressed; install python3-brotli on this host'
                raise ValueError(msg) from exc
        return brotli.decompress(body)
    msg = f'unknown content-encoding {encoding}'
    raise ValueError(msg)


def _cache_page(path: Path, key_length: int) -> str | None:
    """The decompressed page in one cache entry, or None while the entry is incomplete."""
    data = path.read_bytes()
    body_start = _SIMPLE_HEADER.size + key_length
    body_end = data.find(_SIMPLE_EOF, body_start)
    if body_end < 0:
        return None
    # The response headers follow the body's EOF record, as Chromium pickled them.
    headers = data[body_end + _EOF_RECORD_SIZE :].lower()
    encoding = re.search(rb'content-encoding:\s*([a-z0-9-]+)', headers)
    try:
        page = _decompress(data[body_start:body_end], encoding.group(1).decode() if encoding else '')
    except (OSError, EOFError, zlib.error):
        return None
    text = page.decode('utf-8', 'replace')
    return text if '</html>' in text[-4096:].lower() else None


def _cached_post(post: dict[str, str]) -> tuple[str, str] | None:
    """(entry name, page) of the newest complete cached copy of the post."""
    if not XWEB_CACHE.is_dir():
        return None
    matches: list[tuple[float, Path, int]] = []
    # One Cache_Data per browser profile; WeChat has used both `Default` and `Profile 1`.
    for directory in XWEB_CACHE.glob('*/HTTP Cache/Cache_Data'):
        for path in directory.glob('*_0'):
            try:
                key = _cache_key(path)
                if key is None or _POST_HOST not in key:
                    continue
                # The key is `<partition prefix> <url>`.
                if _same_post(key.rsplit(' ', 1)[-1], post):
                    matches.append((path.stat().st_mtime, path, len(key.encode('utf-8'))))
            except OSError:
                continue
    for _mtime, path, key_length in sorted(matches, reverse=True):
        try:
            page = _cache_page(path, key_length)
        except OSError:
            continue
        if page is not None:
            return path.name, page
    return None


class OpenError(RuntimeError):
    """WeChat could not be made to open the message."""


def _target(request: object) -> dict | None:
    """The message a request names for opening, validated, or None."""
    message = request.get('message') if isinstance(request, dict) else None
    if not isinstance(message, dict):
        return None
    talker, msg_id = message.get('talker'), message.get('msg_id')
    if not isinstance(talker, str) or not _TALKER_RE.fullmatch(talker) or not isinstance(msg_id, int) or isinstance(msg_id, bool):
        return None
    texts = message.get('texts') if isinstance(message.get('texts'), list) else []
    texts = [text.strip()[:_MAX_TEXT_CHARS] for text in texts if isinstance(text, str) and text.strip()][:_MAX_TEXTS]
    target = {'talker': talker, 'msg_id': msg_id, 'texts': texts}
    create_time = message.get('create_time')
    if isinstance(create_time, int) and not isinstance(create_time, bool) and create_time > 0:
        # Its send time (ms) tells the plugin which way to scroll the chat.
        target['create_time'] = create_time
    return target


def _bridge_call(path: str, payload: dict, timeout: float = BRIDGE_TIMEOUT_SECONDS) -> tuple[int, dict]:
    """POST to the plugin's bridge and return its status and JSON answer."""
    try:
        bridge_token = BRIDGE_TOKEN_FILE.read_text(encoding='utf-8').strip()
    except OSError as exc:
        msg = 'bridge token not found; is the plugin loaded?'
        raise OpenError(msg) from exc
    body = json.dumps(payload).encode('utf-8')
    conn = http.client.HTTPConnection(BRIDGE.hostname, BRIDGE.port or 80, timeout=timeout)
    try:
        conn.request('POST', path, body, {'Authorization': f'Bearer {bridge_token}', 'Content-Type': 'application/json'})
        response = conn.getresponse()
        raw = response.read()
    except OSError as exc:
        msg = f'bridge unreachable: {exc}'
        raise OpenError(msg) from exc
    finally:
        conn.close()
    try:
        answer = json.loads(raw or b'{}')
    except ValueError:
        answer = {}
    return response.status, answer if isinstance(answer, dict) else {}


def _open_message(target: dict) -> list[str]:
    """Have the plugin tap the message open in WeChat; the steps it took. Call with _OPEN_LOCK held."""
    left_in = _LEFT_IN_CHAT['talker']
    if left_in is not None and left_in != target['talker']:
        # The plugin cannot always tell which chat it is in, so leave another chat first.
        _close_message(home=True)
    started = time.monotonic()
    status, answer = _bridge_call('/ui/open', target, OPEN_TIMEOUT_SECONDS)
    steps = [str(step) for step in answer.get('steps') or []]
    sys.stderr.write(
        f'open {target["msg_id"]}: {"opened" if answer.get("opened") else "not opened"} '
        f'after {time.monotonic() - started:.0f}s ({"; ".join(steps) or "no steps"})\n'
    )
    if status != HTTPStatus.OK or not answer.get('opened'):
        # Whatever the taps got to, leave WeChat on its chat list.
        _close_message(home=True)
        detail = answer.get('error') or f'HTTP {status}'
        msg = f'WeChat did not open message {target["msg_id"]}: {detail} (steps: {"; ".join(steps) or "none"})'
        raise OpenError(msg)
    _LEFT_IN_CHAT['talker'] = target['talker']
    return steps


def _close_message(*, home: bool = False) -> None:
    """Back out of what was opened, to its chat (with `home`, to WeChat's chat list).

    Staying in the chat keeps the next message of that chat a few swipes away;
    back on the chat list, reopening the chat would start from its newest
    message again. A chat left unused for _IDLE_HOME_SECONDS is backed out of
    too. Call with _OPEN_LOCK held. A failure is only logged.
    """
    timer = _LEFT_IN_CHAT['timer']
    if isinstance(timer, threading.Timer):
        timer.cancel()
    _LEFT_IN_CHAT['timer'] = None
    if home:
        _LEFT_IN_CHAT['talker'] = None
    else:
        timer = threading.Timer(_IDLE_HOME_SECONDS, _leave_idle_chat)
        timer.daemon = True
        timer.start()
        _LEFT_IN_CHAT['timer'] = timer
    try:
        status, answer = _bridge_call('/ui/close', {'home': home})
    except OpenError as exc:
        sys.stderr.write(f'closing in WeChat failed: {exc}\n')
        return
    if status != HTTPStatus.OK:
        sys.stderr.write(f'closing in WeChat failed: HTTP {status} {answer.get("error", "")}\n')


def _leave_idle_chat() -> None:
    """Take WeChat from the chat it was left in back to its chat list, unless something is being opened."""
    if not _OPEN_LOCK.acquire(blocking=False):
        return
    try:
        if _LEFT_IN_CHAT['talker'] is not None:
            _close_message(home=True)
    finally:
        _OPEN_LOCK.release()


def _post_page(post: dict[str, str], target: dict | None, wait: float) -> tuple[str, str, bool] | None:
    """(entry name, page, whether WeChat had to open it), or None if it never got cached."""
    with _OPEN_LOCK:
        cached = _cached_post(post)
        if cached is not None:
            return (*cached, False)
        if target is None:
            return None
        _open_message(target)
        try:
            deadline = time.monotonic() + wait
            while time.monotonic() < deadline:
                time.sleep(_POLL_SECONDS)
                cached = _cached_post(post)
                if cached is not None:
                    return (*cached, True)
            return None
        finally:
            _close_message()


def _entry_complete(path: Path, key_length: int) -> bool:
    """Whether a cache entry holds a whole response: its body's EOF record is there."""
    return path.read_bytes().find(_SIMPLE_EOF, _SIMPLE_HEADER.size + key_length) >= 0


def _cached_since(url: str, since: float) -> bool:
    """Whether the web view cached a whole response for the link's host and path after ``since``."""
    wanted = urlsplit(url)
    host, path = (wanted.hostname or '').lower(), wanted.path or '/'
    if not XWEB_CACHE.is_dir():
        return False
    for directory in XWEB_CACHE.glob('*/HTTP Cache/Cache_Data'):
        for entry in directory.glob('*_0'):
            try:
                if entry.stat().st_mtime < since:
                    continue
                key = _cache_key(entry)
                if key is None:
                    continue
                found = urlsplit(key.rsplit(' ', 1)[-1])
                if (
                    (found.hostname or '').lower() == host
                    and (found.path or '/') == path
                    and _entry_complete(entry, len(key.encode('utf-8')))
                ):
                    return True
            except (OSError, ValueError):
                continue
    return False


def _open_link(url: str, target: dict, wait: float) -> tuple[bool, float, list[str]]:
    """Open a shared link in WeChat, wait for it to load, close it: (loaded, seconds it was open, steps)."""
    with _OPEN_LOCK:
        # Whole seconds: cache files' mtimes may be truncated to them.
        since = int(time.time())
        steps = _open_message(target)
        started = time.monotonic()
        try:
            deadline = started + wait
            while time.monotonic() < deadline:
                time.sleep(_POLL_SECONDS)
                if _cached_since(url, since):
                    time.sleep(_LINK_DWELL_SECONDS)
                    return True, time.monotonic() - started, steps
            return False, time.monotonic() - started, steps
        finally:
            _close_message()


def _feed_id(object_id: str) -> int | None:
    """WeChat's signed form of a 视频号 object id, or None if it is not one."""
    if not _OBJECT_ID_RE.fullmatch(object_id):
        return None
    value = int(object_id)
    if value >= 1 << 63:
        value -= 1 << 64
    return value if -(1 << 63) <= value < 1 << 63 else None


def _finder_account() -> tuple[Path, Path]:
    """(视频号 database, video cache directory) of the account that used it last.

    A new login gets a new account directory and leaves the old one behind, so
    the account is the one whose database changed most recently.
    """
    newest: tuple[float, Path] | None = None
    for database in (WECHAT_DATA / 'MicroMsg').glob('*/FinderMM*.db'):
        if not _ACCOUNT_RE.fullmatch(database.parent.name):
            continue
        try:
            changed = max(path.stat().st_mtime for path in database.parent.glob(f'{database.name}*'))
        except (OSError, ValueError):
            continue
        if newest is None or changed > newest[0]:
            newest = (changed, database)
    if newest is None:
        msg = f'no WeChat account under {WECHAT_DATA} has a 视频号 database; is anyone logged in?'
        raise LookupError(msg)
    database = newest[1]
    return database, WECHAT_DATA / 'cache' / database.parent.name / 'finder' / 'video'


def _cache_entries(database: Path, feed_id: int) -> list[tuple[str, int]]:
    """(media id, total size) of the files WeChat cached for one feed, newest first.

    Read from a copy, with its write-ahead log, so WeChat's own writes are never
    blocked and the latest rows are included.
    """
    with tempfile.TemporaryDirectory() as tmp:
        for path in database.parent.glob(f'{database.name}*'):
            shutil.copy2(path, Path(tmp) / path.name)
        conn = sqlite3.connect(f'file:{Path(tmp) / database.name}?mode=ro', uri=True)
        try:
            tables = [
                row[0] for row in conn.execute("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'FinderMediaCacheInfo%'")
            ]
            entries: list[tuple[str, int]] = []
            # The newest version of the table first; its name only ever passes _TABLE_RE.
            for table in sorted((name for name in tables if _TABLE_RE.fullmatch(name)), reverse=True):
                columns = {row[1] for row in conn.execute(f'PRAGMA table_info({table})')}
                if not {'feedId', 'mediaId', 'totalSize'} <= columns:
                    continue
                rows = conn.execute(f'SELECT mediaId, totalSize FROM {table} WHERE feedId = ? ORDER BY rowid DESC', (feed_id,))  # noqa: S608
                entries.extend((str(media_id), int(total or 0)) for media_id, total in rows)
            return entries
        finally:
            conn.close()


def _complete_mp4(path: Path, expected_size: int) -> bool:
    """Whether a cached file is a whole MP4: its top-level boxes add up to its size."""
    size = path.stat().st_size
    if expected_size and size != expected_size:
        return False
    seen: set[bytes] = set()
    offset = 0
    with path.open('rb') as fh:
        while offset + _MP4_BOX.size <= size:
            fh.seek(offset)
            length, kind = _MP4_BOX.unpack(fh.read(_MP4_BOX.size))
            header = _MP4_BOX.size
            if length == 1:
                length = struct.unpack('>Q', fh.read(8))[0]
                header += 8
            elif length == 0:
                length = size - offset
            if length < header:
                return False
            seen.add(kind)
            offset += length
    return offset == size and {b'ftyp', b'moov', b'mdat'} <= seen


def _cached_video(feed_id: int) -> tuple[Path, str] | None:
    """(file, media id) of a complete cached copy of the feed's video."""
    database, video_dir = _finder_account()
    for media_id, total in _cache_entries(database, feed_id):
        if not _MEDIA_ID_RE.fullmatch(media_id):
            continue
        path = video_dir / media_id
        try:
            if path.is_file() and _complete_mp4(path, total):
                return path, media_id
        except OSError:
            continue
    return None


def _finder_video(feed_id: int, target: dict | None, wait: float) -> tuple[Path, str, bool] | None:
    """(file, media id, whether WeChat had to open it), or None if it never finished downloading."""
    with _OPEN_LOCK:
        cached = _cached_video(feed_id)
        if cached is not None:
            return (*cached, False)
        if target is None:
            return None
        _open_message(target)
        try:
            deadline = time.monotonic() + wait
            while time.monotonic() < deadline:
                time.sleep(_POLL_SECONDS)
                cached = _cached_video(feed_id)
                if cached is not None:
                    return (*cached, True)
            return None
        finally:
            _close_message()


def _load_meta(msg_id: str) -> dict | None:
    path = DIR / f'{msg_id}.json'
    try:
        with path.open('rb') as fh:
            return json.load(fh)
    except (OSError, ValueError):
        return None


def _media_path(msg_id: str) -> Path | None:
    for ext in _MEDIA_EXTS:
        candidate = DIR / f'{msg_id}.{ext}'
        if candidate.is_file():
            return candidate
    return None


class Handler(BaseHTTPRequestHandler):
    server_version = 'favinbox/1.0'
    protocol_version = 'HTTP/1.1'

    def log_message(self, fmt: str, *args: object) -> None:
        sys.stderr.write(f'{self.address_string()} - {fmt % args}\n')

    def _authed(self) -> bool:
        header = self.headers.get('Authorization', '')
        prefix = 'Bearer '
        if not header.startswith(prefix):
            return False
        return hmac.compare_digest(header[len(prefix) :], TOKEN)

    def _send_json(self, status: int, payload: object) -> None:
        body = json.dumps(payload).encode('utf-8')
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        parsed = urlsplit(self.path)
        path = parsed.path
        if path == '/health':
            self._send_json(HTTPStatus.OK, {'ok': True})
            return
        if not self._authed():
            self._send_json(HTTPStatus.UNAUTHORIZED, {'error': 'unauthorized'})
            return
        if path.startswith('/wx/'):
            self._forward()
            return
        if path == '/items':
            include_acked = parse_qs(parsed.query).get('include_acked', ['0'])[0] == '1'
            self._send_json(HTTPStatus.OK, self._items(include_acked=include_acked))
            return
        if path.startswith('/file/'):
            self._serve_file(path[len('/file/') :])
            return
        self._send_json(HTTPStatus.NOT_FOUND, {'error': 'not found'})

    def do_POST(self) -> None:
        path = urlsplit(self.path).path
        if not self._authed():
            self._send_json(HTTPStatus.UNAUTHORIZED, {'error': 'unauthorized'})
            return
        if path.startswith('/ack/'):
            self._ack(path[len('/ack/') :])
            return
        if path == '/link/open':
            self._open_link()
            return
        if path == '/post/page':
            self._post_page()
            return
        if path == '/finder/video':
            self._finder_video()
            return
        if path.startswith('/wx/'):
            self._forward()
            return
        self._send_json(HTTPStatus.NOT_FOUND, {'error': 'not found'})

    def do_PUT(self) -> None:
        if not self._authed():
            self._send_json(HTTPStatus.UNAUTHORIZED, {'error': 'unauthorized'})
            return
        if urlsplit(self.path).path.startswith('/wx/'):
            self._forward()
            return
        self._send_json(HTTPStatus.NOT_FOUND, {'error': 'not found'})

    def _forward(self) -> None:
        """Relay this request to the plugin's bridge, streaming both bodies."""
        try:
            bridge_token = BRIDGE_TOKEN_FILE.read_text(encoding='utf-8').strip()
        except OSError:
            self._send_json(HTTPStatus.SERVICE_UNAVAILABLE, {'error': 'bridge token not found; is the plugin loaded?'})
            return
        target = self.path[len('/wx') :]
        length = int(self.headers.get('Content-Length') or 0)
        conn = http.client.HTTPConnection(BRIDGE.hostname, BRIDGE.port or 80, timeout=BRIDGE_TIMEOUT_SECONDS)
        try:
            conn.putrequest(self.command, target, skip_accept_encoding=True)
            conn.putheader('Authorization', f'Bearer {bridge_token}')
            conn.putheader('Content-Length', str(length))
            if self.headers.get('Content-Type'):
                conn.putheader('Content-Type', self.headers['Content-Type'])
            conn.endheaders()
            remaining = length
            while remaining > 0:
                chunk = self.rfile.read(min(_CHUNK, remaining))
                if not chunk:
                    break
                conn.send(chunk)
                remaining -= len(chunk)
            response = conn.getresponse()
        except OSError as exc:
            conn.close()
            self._send_json(HTTPStatus.BAD_GATEWAY, {'error': f'bridge unreachable: {exc}'})
            return
        try:
            self.send_response(response.status)
            self.send_header('Content-Type', response.getheader('Content-Type', 'application/octet-stream'))
            body_length = response.getheader('Content-Length')
            if body_length is not None:
                self.send_header('Content-Length', body_length)
            else:
                self.close_connection = True
            self.end_headers()
            while chunk := response.read(_CHUNK):
                self.wfile.write(chunk)
        finally:
            conn.close()

    def _open_link(self) -> None:
        length = int(self.headers.get('Content-Length') or 0)
        try:
            request = json.loads(self.rfile.read(length) or b'{}')
            url = str(request.get('url', ''))
            target = _target(request)
            wait = min(float(request.get('wait', _DEFAULT_LINK_WAIT_SECONDS)), _MAX_LINK_WAIT_SECONDS)
        except (ValueError, AttributeError, TypeError):
            url, target, wait = '', None, 0
        if not _LINK_RE.fullmatch(url) or target is None:
            self._send_json(HTTPStatus.BAD_REQUEST, {'error': 'url must be an http(s) URL of printable ASCII, and message must name it'})
            return
        try:
            loaded, seconds, steps = _open_link(url, target, wait)
        except (OpenError, OSError) as exc:
            self._send_json(HTTPStatus.BAD_GATEWAY, {'error': str(exc)})
            return
        self._send_json(HTTPStatus.OK, {'loaded': loaded, 'seconds': round(seconds, 1), 'steps': steps})

    def _post_page(self) -> None:
        length = int(self.headers.get('Content-Length') or 0)
        try:
            request = json.loads(self.rfile.read(length) or b'{}')
            post = _post_id(str(request.get('url', '')))
            target = _target(request)
            wait = min(float(request.get('wait', _DEFAULT_WAIT_SECONDS)), _MAX_WAIT_SECONDS)
        except (ValueError, AttributeError, TypeError):
            post, target, wait = None, None, 0
        if post is None:
            self._send_json(HTTPStatus.BAD_REQUEST, {'error': 'url must be a mp.weixin.qq.com/s link with __biz, mid, idx and sn'})
            return
        try:
            found = _post_page(post, target, wait)
        except (OpenError, OSError) as exc:
            self._send_json(HTTPStatus.BAD_GATEWAY, {'error': str(exc)})
            return
        except ValueError as exc:
            self._send_json(HTTPStatus.INTERNAL_SERVER_ERROR, {'error': str(exc)})
            return
        if found is None:
            error = (
                f'WeChat opened the post but it was not in the XWeb cache after {wait:.0f}s'
                if target
                else 'the post is not in the XWeb cache'
            )
            self._send_json(HTTPStatus.NOT_FOUND, {'error': error})
            return
        entry, page, opened = found
        body = page.encode('utf-8')
        self.send_response(HTTPStatus.OK)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Content-Length', str(len(body)))
        self.send_header('X-Cache-Entry', entry)
        self.send_header('X-Opened', '1' if opened else '0')
        self.end_headers()
        self.wfile.write(body)

    def _finder_video(self) -> None:
        length = int(self.headers.get('Content-Length') or 0)
        try:
            request = json.loads(self.rfile.read(length) or b'{}')
            feed_id = _feed_id(str(request.get('object_id', '')))
            target = _target(request)
            wait = min(float(request.get('wait', _DEFAULT_VIDEO_WAIT_SECONDS)), _MAX_VIDEO_WAIT_SECONDS)
        except (ValueError, AttributeError, TypeError):
            feed_id, target, wait = None, None, 0
        if feed_id is None:
            self._send_json(HTTPStatus.BAD_REQUEST, {'error': 'object_id must be a 视频号 object id'})
            return
        try:
            found = _finder_video(feed_id, target, wait)
        except LookupError as exc:
            self._send_json(HTTPStatus.SERVICE_UNAVAILABLE, {'error': str(exc)})
            return
        except (OpenError, OSError, sqlite3.Error) as exc:
            self._send_json(HTTPStatus.BAD_GATEWAY, {'error': str(exc)})
            return
        if found is None:
            error = (
                f'WeChat opened the video but had not downloaded all of it after {wait:.0f}s' if target else 'the video is not cached whole'
            )
            self._send_json(HTTPStatus.NOT_FOUND, {'error': error})
            return
        path, media_id, opened = found
        try:
            fh = path.open('rb')
            size = os.fstat(fh.fileno()).st_size
        except OSError as exc:
            self._send_json(HTTPStatus.INTERNAL_SERVER_ERROR, {'error': str(exc)})
            return
        with fh:
            self.send_response(HTTPStatus.OK)
            self.send_header('Content-Type', 'video/mp4')
            self.send_header('Content-Length', str(size))
            self.send_header('X-Media-Id', media_id)
            self.send_header('X-Opened', '1' if opened else '0')
            self.end_headers()
            remaining = size
            while remaining > 0 and (chunk := fh.read(min(_CHUNK, remaining))):
                self.wfile.write(chunk)
                remaining -= len(chunk)

    def _items(self, *, include_acked: bool) -> list[dict]:
        items: list[dict] = []
        for meta_path in sorted(DIR.glob('*.json')):
            try:
                with meta_path.open('rb') as fh:
                    meta = json.load(fh)
            except (OSError, ValueError):
                continue
            if not include_acked and str(meta.get('status', '')).startswith('acked'):
                continue
            meta['has_file'] = _media_path(str(meta.get('msg_id', ''))) is not None
            items.append(meta)
        return items

    def _serve_file(self, msg_id: str) -> None:
        if not _MSG_ID_RE.match(msg_id):
            self._send_json(HTTPStatus.BAD_REQUEST, {'error': 'bad msg_id'})
            return
        media = _media_path(msg_id)
        if media is None:
            self._send_json(HTTPStatus.NOT_FOUND, {'error': 'no media'})
            return
        try:
            data = media.read_bytes()
        except OSError as exc:
            self._send_json(HTTPStatus.INTERNAL_SERVER_ERROR, {'error': str(exc)})
            return
        ctype = _CONTENT_TYPES.get(media.suffix.lstrip('.'), 'application/octet-stream')
        self.send_response(HTTPStatus.OK)
        self.send_header('Content-Type', ctype)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _ack(self, msg_id: str) -> None:
        if not _MSG_ID_RE.match(msg_id):
            self._send_json(HTTPStatus.BAD_REQUEST, {'error': 'bad msg_id'})
            return
        meta = _load_meta(msg_id)
        if meta is None:
            self._send_json(HTTPStatus.NOT_FOUND, {'error': 'unknown msg_id'})
            return
        media = _media_path(msg_id)
        removed = False
        if media is not None:
            try:
                media.unlink()
                removed = True
            except OSError as exc:
                self._send_json(HTTPStatus.INTERNAL_SERVER_ERROR, {'error': str(exc)})
                return
        meta['status'] = 'acked'
        meta.pop('file', None)
        meta.pop('size', None)
        tmp = DIR / f'{msg_id}.json.tmp'
        try:
            with tmp.open('w', encoding='utf-8') as fh:
                json.dump(meta, fh)
            tmp.replace(DIR / f'{msg_id}.json')
        except OSError as exc:
            self._send_json(HTTPStatus.INTERNAL_SERVER_ERROR, {'error': str(exc)})
            return
        self._send_json(HTTPStatus.OK, {'msg_id': int(msg_id), 'acked': True, 'media_removed': removed})


def main() -> None:
    host = os.environ.get('FAVINBOX_HOST', '127.0.0.1')
    port = int(os.environ.get('FAVINBOX_PORT', '9970'))
    server = ThreadingHTTPServer((host, port), Handler)
    sys.stderr.write(f'favinbox serving {DIR} on {host}:{port}\n')
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        server.shutdown()


if __name__ == '__main__':
    main()
