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

`/wx/*` takes the same bearer token as everything else here, then calls the
bridge with the plugin's own token, so fav never needs that second secret. The
token file is re-read on every request because the plugin creates it on first
load and may be reinstalled.
"""

from __future__ import annotations

import hmac
import http.client
import json
import os
import re
import sys
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
