"""Tiny HTTP API over the FavInbox directory on the oracle host.

The WAuxiliary plugin (main.java) drops ``<msgId>.<ext>`` plus ``<msgId>.json`` into an
inbox directory; the JSON is written last and marks the item complete. This server lets
fav pull those items over an frp tunnel: list what is pending, download a file, and
acknowledge it so it moves out of the way.

Standard library only, so it runs on the host's ``python3`` with no virtualenv. It is
started by systemd as root because the inbox belongs to the Android app uid inside the
redroid bind mount (see install.sh).

Endpoints (all but ``/healthz`` need ``Authorization: Bearer <token>``):

- ``GET /healthz``                       -> ``{"ok": true}``
- ``GET /v1/items?after=<id>&limit=<n>`` -> ``{"items": [...]}`` pending items, msg_id ascending
- ``GET /v1/items/<id>/file``            -> the media bytes
- ``POST /v1/items/<id>/ack``            -> moves the item into ``delivered/``; 204

Acknowledged items are kept under ``delivered/`` for ``FAVINBOX_RETENTION_DAYS`` and then
deleted, so a failed import on the fav side can still be recovered by hand.
"""

from __future__ import annotations

import hmac
import json
import logging
import mimetypes
import os
import re
import shutil
import time
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

INBOX_DIR = Path(os.environ.get('FAVINBOX_DIR', '/home/ubuntu/redroid-data/media/0/Android/data/com.tencent.mm/files/FavInbox'))
TOKEN_FILE = Path(os.environ.get('FAVINBOX_TOKEN_FILE', '/etc/favinbox/token'))
BIND = os.environ.get('FAVINBOX_BIND', '127.0.0.1')
PORT = int(os.environ.get('FAVINBOX_PORT', '9910'))
RETENTION_DAYS = float(os.environ.get('FAVINBOX_RETENTION_DAYS', '7'))
DELIVERED_DIR_NAME = 'delivered'
MAX_LIMIT = 500
DEFAULT_LIMIT = 100

_ID_RE = re.compile(r'^[0-9]{1,20}$')
_ITEM_PATH_RE = re.compile(r'^/v1/items/([0-9]{1,20})/(file|ack)$')

log = logging.getLogger('favinbox')


def _load_token() -> str:
    token = TOKEN_FILE.read_text(encoding='utf-8').strip()
    if not token:
        msg = f'{TOKEN_FILE} is empty'
        raise RuntimeError(msg)
    return token


def _read_meta(path: Path) -> dict | None:
    try:
        data = json.loads(path.read_text(encoding='utf-8'))
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


def _pending_items(after: int, limit: int) -> list[dict]: