"""Recognise Baidu Tieba picture links and fetch the pictures behind them.

A Tieba picture link looks like
``https://tiebapic.baidu.com/forum/w%3D1080%3Bq%3D90/sign=<sig>/<file>.jpg?tbpicau=<expiry>_<hash>``.
The ``w=...;q=...`` segment asks the CDN for a resized copy; ``/forum/pic/item/<file>`` names the
original upload. ``tbpicau`` is a time-limited grant: once it lapses the CDN still answers 200, but
with a 238x238 Tieba logo and an ``Error-Message: img not auth`` header instead of the picture.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import PurePosixPath
from typing import TYPE_CHECKING
from urllib.parse import urlsplit, urlunsplit

import httpx

if TYPE_CHECKING:
    from collections.abc import Iterable

TIEBA_IMAGE_HOSTS = frozenset({'tiebapic.baidu.com', 'imgsa.baidu.com', 'imgsrc.baidu.com'})

# ASCII only, so Chinese text written right after a link, without a space, ends it.
_URL_RE = re.compile(r'https?://[A-Za-z0-9\-._~:/?#\[\]@!$&()*+,;=%]+', re.IGNORECASE)
# Punctuation a sentence wraps around a pasted link; never the tail of a CDN path or query.
_TRAILING_PUNCTUATION = '.,;:!?)]'
_RESIZED_PATH_RE = re.compile(r'^/forum/[^/]+/sign=[0-9a-f]+/(?P<file>[^/]+)$', re.IGNORECASE)
_CONTENT_TYPE_EXTENSIONS = {
    'image/jpeg': '.jpg',
    'image/png': '.png',
    'image/gif': '.gif',
    'image/webp': '.webp',
}
_REQUEST_HEADERS = {
    'User-Agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0 Safari/537.36',
    'Referer': 'https://tieba.baidu.com/',
}


class TiebaImageUnavailableError(Exception):
    """The CDN refused the picture, usually because the link's ``tbpicau`` grant has expired."""


@dataclass(frozen=True, slots=True)
class TiebaImage:
    url: str
    content: bytes
    extension: str


def is_tieba_image_url(url: str) -> bool:
    parts = urlsplit(url)
    return parts.scheme in {'http', 'https'} and (parts.hostname or '').lower() in TIEBA_IMAGE_HOSTS and parts.path.startswith('/forum/')


def extract_tieba_image_urls(text: str, extra_urls: Iterable[str] = ()) -> list[str]:
    """Return the distinct Tieba picture links in ``text`` and ``extra_urls``, in order."""
    candidates = [match.group(0).rstrip(_TRAILING_PUNCTUATION) for match in _URL_RE.finditer(text or '')]
    candidates.extend(url.strip() for url in extra_urls if url)
    urls: list[str] = []
    for url in candidates:
        if is_tieba_image_url(url) and url not in urls:
            urls.append(url)
    return urls


def strip_tieba_image_urls(text: str) -> str:
    """Drop Tieba picture links from ``text`` so what is left can serve as a title."""
    kept = _URL_RE.sub(lambda match: '' if is_tieba_image_url(match.group(0).rstrip(_TRAILING_PUNCTUATION)) else match.group(0), text or '')
    return ' '.join(kept.split())


def original_image_url(url: str) -> str:
    """Point a resized ``/forum/<transform>/sign=<sig>/<file>`` link at the original upload."""
    parts = urlsplit(url)
    match = _RESIZED_PATH_RE.match(parts.path)
    if match is None:
        return url
    return urlunsplit(parts._replace(path=f'/forum/pic/item/{match.group("file")}'))


def _image_extension(url: str, content_type: str) -> str:
    mime_type = content_type.split(';', 1)[0].strip().lower()
    if mime_type in _CONTENT_TYPE_EXTENSIONS:
        return _CONTENT_TYPE_EXTENSIONS[mime_type]
    suffix = PurePosixPath(urlsplit(url).path).suffix.lower()
    return suffix if suffix in {'.jpg', '.jpeg', '.png', '.gif', '.webp'} else '.jpg'


async def _fetch(client: httpx.AsyncClient, url: str) -> TiebaImage:
    response = await client.get(url, headers=_REQUEST_HEADERS)
    response.raise_for_status()
    refusal = response.headers.get('Error-Message', '').strip()
    if refusal:
        msg = f'Tieba refused the picture ({refusal}); the link has most likely expired'
        raise TiebaImageUnavailableError(msg)
    content_type = response.headers.get('Content-Type', '')
    if not content_type.lower().startswith('image/'):
        msg = f'Tieba answered with {content_type or "no content type"} instead of a picture'
        raise TiebaImageUnavailableError(msg)
    return TiebaImage(url=url, content=response.content, extension=_image_extension(url, content_type))


async def fetch_tieba_image(client: httpx.AsyncClient, url: str) -> TiebaImage:
    """Fetch the original upload behind ``url``, falling back to the link exactly as it was shared."""
    original = original_image_url(url)
    if original != url:
        try:
            return await _fetch(client, original)
        except (TiebaImageUnavailableError, httpx.HTTPStatusError):
            pass
    return await _fetch(client, url)
