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
    POST /post/page         -> a 公众号 post's page as WeChat itself loaded it

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
    FAVINBOX_ADB                adb binary (default adb)
    FAVINBOX_ADB_SERIAL         the redroid device (default 127.0.0.1:5555)

`/wx/*` takes the same bearer token as everything else here, then calls the
bridge with the plugin's own token, so fav never needs that second secret. The
token file is re-read on every request because the plugin creates it on first
load and may be reinstalled.

`/post/page` exists for image posts (贴图): mp.weixin.qq.com answers their share
links with a slider captcha for any outside client, but WeChat opens them
normally with its login. The body is JSON, `{"url": "<the share link>",
"wait": <seconds, default 30>}`. If WeChat's web view has the post in its HTTP
cache already, that copy is returned; otherwise the post is opened in WeChat
over adb and the cache is polled until the page lands there. The answer is the
page's HTML (decompressed), or a JSON error: 400 for a link that is not a
`mp.weixin.qq.com/s?__biz=...&mid=...&idx=...&sn=...` post, 404 when the page
never reached the cache, 502 when adb failed.
"""

from __future__ import annotations

import gzip
import hmac
import http.client
import json
import os
import re
import struct
import subprocess
import sys
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
_CHUNK = 65536

# Not beside FAVINBOX_DIR: the web view caches into WeChat's internal data
# directory (/data/data/com.tencent.mm), which redroid keeps under data/.
XWEB_CACHE = Path(os.environ.get('FAVINBOX_XWEB_CACHE', '') or '/home/ubuntu/redroid-data/data/com.tencent.mm/cache/xweb_cache')
ADB = os.environ.get('FAVINBOX_ADB', '') or 'adb'
ADB_SERIAL = os.environ.get('FAVINBOX_ADB_SERIAL', '') or '127.0.0.1:5555'
_WEBVIEW = 'com.tencent.mm/.plugin.webview.ui.tools.WebViewUI'
_POST_HOST = 'mp.weixin.qq.com'
_DEFAULT_WAIT_SECONDS = 30
_MAX_WAIT_SECONDS = 90
_POLL_SECONDS = 2
# Only one post is opened at a time, so a page found in the cache is the one asked for.
_OPEN_LOCK = threading.Lock()
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


def _adb(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run([ADB, '-s', ADB_SERIAL, *args], capture_output=True, text=True, timeout=30, check=False)  # noqa: S603


def _open_in_wechat(post: dict[str, str]) -> None:
    url = f'https://{_POST_HOST}/s?' + '&'.join(f'{name}={post[name]}' for name in _POST_FIELDS)
    # The adb server forgets the network device whenever it restarts.
    subprocess.run([ADB, 'connect', ADB_SERIAL], capture_output=True, timeout=30, check=False)  # noqa: S603
    # Every part of the URL matched _POST_FIELDS, so it holds no quote or shell character.
    result = _adb('shell', f'su 0 -c "am start -n {_WEBVIEW} --es rawUrl \'{url}\'"')
    output = f'{result.stdout}\n{result.stderr}'.strip()
    if result.returncode != 0 or 'Error' in output:
        msg = f'adb could not open the post in WeChat: {output[-300:]}'
        raise RuntimeError(msg)


def _close_webview() -> None:
    _adb('shell', 'input', 'keyevent', '4')


def _post_page(post: dict[str, str], wait: float) -> tuple[str, str, bool] | None:
    """(entry name, page, whether WeChat had to open it), or None if it never got cached."""
    with _OPEN_LOCK:
        cached = _cached_post(post)
        if cached is not None:
            return (*cached, False)
        _open_in_wechat(post)
        try:
            deadline = time.monotonic() + wait
            while time.monotonic() < deadline:
                time.sleep(_POLL_SECONDS)
                cached = _cached_post(post)
                if cached is not None:
                    return (*cached, True)
            return None
        finally:
            _close_webview()


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
        if path == '/post/page':
            self._post_page()
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

    def _post_page(self) -> None:
        length = int(self.headers.get('Content-Length') or 0)
        try:
            request = json.loads(self.rfile.read(length) or b'{}')
            post = _post_id(str(request.get('url', '')))
            wait = min(float(request.get('wait', _DEFAULT_WAIT_SECONDS)), _MAX_WAIT_SECONDS)
        except (ValueError, AttributeError, TypeError):
            post, wait = None, 0
        if post is None:
            self._send_json(HTTPStatus.BAD_REQUEST, {'error': 'url must be a mp.weixin.qq.com/s link with __biz, mid, idx and sn'})
            return
        try:
            found = _post_page(post, wait)
        except (RuntimeError, OSError, subprocess.SubprocessError) as exc:
            self._send_json(HTTPStatus.BAD_GATEWAY, {'error': str(exc)})
            return
        except ValueError as exc:
            self._send_json(HTTPStatus.INTERNAL_SERVER_ERROR, {'error': str(exc)})
            return
        if found is None:
            self._send_json(HTTPStatus.NOT_FOUND, {'error': f'WeChat opened the post but it was not in the XWeb cache after {wait:.0f}s'})
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
