"""视频号 (WeChat Channels) videos forwarded to the agent's WeChat account.

Forwarding a 视频号 video sends a type-51 message whose ``<finderFeed>`` names the
feed: ``objectId`` (an unsigned 64-bit number), the ``objectNonceId`` WeChat
needs to open it, the author's nickname, the caption, and per media item its
``mediaType`` (4 for a video), size, duration and cover. The video address in
it carries no token or key, so it cannot be downloaded as it is.

TikHub's ``fetch_video_detail`` looks the feed up by ``objectId`` with WeChat
accounts of its own and answers with a signed CDN address and a ``decode_key``.
The file behind that address is the original MP4 with only its first
``ENCRYPTED_BYTES`` encrypted: XORed with an ISAAC-64 keystream seeded with the
``decode_key``, each 64-bit output written big-endian, starting from the top of
the generator's first block as its ``rand()`` hands them out. Every lookup
returns a fresh address and key, so the two must come from the same answer.
"""

from __future__ import annotations

import html
import re
from dataclasses import dataclass
from typing import TYPE_CHECKING
from urllib.parse import urlsplit

if TYPE_CHECKING:
    from collections.abc import Iterator

_FEED_RE = re.compile(r'<finderFeed>.*?</finderFeed>', re.DOTALL)
_MEDIA_RE = re.compile(r'<media>.*?</media>', re.DOTALL)
_VIDEO_MEDIA = '4'
_OBJECT_ID_RE = re.compile(r'\d{1,20}')
_NONCE_ID_RE = re.compile(r'[A-Za-z0-9_-]{1,200}')
# TikHub takes only a numeric nonce; older shares carry a longer composite one.
_NUMERIC_NONCE_RE = re.compile(r'\d{1,32}')
_DECODE_KEY_RE = re.compile(r'\d{1,20}')
# How much of a video file WeChat encrypts.
ENCRYPTED_BYTES = 131072
_MASK = (1 << 64) - 1
_GOLDEN_RATIO = 0x9E3779B97F4A7C13


@dataclass(frozen=True)
class ChannelVideo:
    object_id: str
    nonce_id: str
    author: str
    description: str
    duration: int
    width: int
    height: int
    cover_url: str


def _tag(xml: str, name: str) -> str:
    """The text of the first ``<name>`` element, with CDATA and entities undone."""
    match = re.search(rf'<{name}>(.*?)</{name}>', xml, re.DOTALL)
    if match is None:
        return ''
    text = match.group(1).strip()
    if text.startswith('<![CDATA[') and text.endswith(']]>'):
        text = text[9:-3]
    return html.unescape(text).strip()


def _number(text: str) -> int:
    return int(text) if text.isdigit() else 0


def parse_channel_video(content: str) -> ChannelVideo | None:
    """The 视频号 video a message shares, or None for anything else: image feeds and lives included."""
    feed = _FEED_RE.search(content or '')
    if feed is None:
        return None
    xml = feed.group(0)
    # The feed's own ids come first; a quoted feed nested further down has its own.
    object_id, nonce_id = _tag(xml, 'objectId'), _tag(xml, 'objectNonceId')
    if not _OBJECT_ID_RE.fullmatch(object_id) or int(object_id) >= 1 << 64 or not _NONCE_ID_RE.fullmatch(nonce_id):
        return None
    if _tag(xml, 'liveId') not in {'', '0'}:
        return None
    media = _MEDIA_RE.search(xml)
    if media is None or _tag(media.group(0), 'mediaType') != _VIDEO_MEDIA:
        return None
    item = media.group(0)
    cover = _tag(item, 'coverUrl') or _tag(item, 'thumbUrl')
    return ChannelVideo(
        object_id=object_id,
        nonce_id=nonce_id,
        author=_tag(xml, 'nickname'),
        description=_tag(xml, 'desc'),
        duration=_number(_tag(item, 'videoPlayDuration')),
        width=_number(_tag(item, 'width')),
        height=_number(_tag(item, 'height')),
        cover_url=cover if urlsplit(cover).scheme in {'http', 'https'} else '',
    )


@dataclass(frozen=True)
class VideoSource:
    """Where TikHub says the video file is, and the key that decrypts its start."""

    url: str
    decode_key: int


def tikhub_lookup(video: ChannelVideo) -> dict[str, object]:
    """The body of a ``fetch_video_detail`` request for this video."""
    body: dict[str, object] = {'object_id': video.object_id, 'raw': False}
    if _NUMERIC_NONCE_RE.fullmatch(video.nonce_id):
        body['object_nonce_id'] = video.nonce_id
    return body


def parse_video_source(payload: object) -> VideoSource | None:
    """The file address and decode key in a ``fetch_video_detail`` answer, or None without them."""
    data = payload.get('data') if isinstance(payload, dict) else None
    # The parsed video is ``data``; some answers wrap it once more.
    if isinstance(data, dict) and 'media' not in data and isinstance(data.get('data'), dict):
        data = data['data']
    media = data.get('media') if isinstance(data, dict) else None
    if isinstance(media, list):
        media = media[0] if media else None
    if not isinstance(media, dict):
        return None
    url = str(media.get('full_url') or '')
    if not url and media.get('url'):
        url = f'{media["url"]}{media.get("url_token") or ""}'
    key = str(media.get('decode_key') or '').strip()
    if urlsplit(url).scheme not in {'http', 'https'} or not _DECODE_KEY_RE.fullmatch(key) or int(key) >= 1 << 64:
        return None
    return VideoSource(url=url, decode_key=int(key))


def _mix(x: list[int]) -> None:
    a, b, c, d, e, f, g, h = x
    a = (a - e) & _MASK
    f ^= h >> 9
    h = (h + a) & _MASK
    b = (b - f) & _MASK
    g ^= (a << 9) & _MASK
    a = (a + b) & _MASK
    c = (c - g) & _MASK
    h ^= b >> 23
    b = (b + c) & _MASK
    d = (d - h) & _MASK
    a ^= (c << 15) & _MASK
    c = (c + d) & _MASK
    e = (e - a) & _MASK
    b ^= d >> 14
    d = (d + e) & _MASK
    f = (f - b) & _MASK
    c ^= (e << 20) & _MASK
    e = (e + f) & _MASK
    g = (g - c) & _MASK
    d ^= f >> 17
    f = (f + g) & _MASK
    h = (h - d) & _MASK
    e ^= (g << 14) & _MASK
    g = (g + h) & _MASK
    x[:] = a, b, c, d, e, f, g, h


class _Isaac64:
    """Bob Jenkins' ISAAC-64, seeded with one 64-bit word and zeros."""

    def __init__(self, seed: int) -> None:
        self.results = [seed] + [0] * 255
        self.memory = [0] * 256
        self.a = self.b = self.c = 0
        x = [_GOLDEN_RATIO] * 8
        for _ in range(4):
            _mix(x)
        for source in (self.results, self.memory):
            for i in range(0, 256, 8):
                x = [(x[j] + source[i + j]) & _MASK for j in range(8)]
                _mix(x)
                self.memory[i : i + 8] = x
        self._refill()

    def _refill(self) -> None:
        memory, results = self.memory, self.results
        self.c = (self.c + 1) & _MASK
        a, b = self.a, (self.b + self.c) & _MASK
        for i in range(256):
            step = i & 3
            if step == 0:
                a = ~(a ^ (a << 21)) & _MASK
            elif step == 1:
                a ^= a >> 5
            elif step == 2:  # noqa: PLR2004
                a ^= (a << 12) & _MASK
            else:
                a ^= a >> 33
            a = (a + memory[(i + 128) & 255]) & _MASK
            x = memory[i]
            y = (memory[(x >> 3) & 255] + a + b) & _MASK
            memory[i] = y
            b = (memory[(y >> 11) & 255] + x) & _MASK
            results[i] = b
        self.a, self.b = a, b

    def blocks(self) -> Iterator[list[int]]:
        """Each block of outputs in the order ``rand()`` hands them out: from the top down."""
        while True:
            yield self.results[::-1]
            self._refill()


def keystream(decode_key: int, size: int = ENCRYPTED_BYTES) -> bytes:
    """The bytes WeChat XORs over the start of a video encrypted with ``decode_key``."""
    out = bytearray()
    for block in _Isaac64(decode_key).blocks():
        out += b''.join(word.to_bytes(8, 'big') for word in block)
        if len(out) >= size:
            return bytes(out[:size])
    raise AssertionError  # pragma: no cover - the generator never ends


def decrypt_head(head: bytes, decode_key: int) -> bytes:
    """``head``, the first bytes of a video file, with WeChat's encryption undone."""
    stream = keystream(decode_key, min(len(head), ENCRYPTED_BYTES))
    return bytes(byte ^ key for byte, key in zip(head, stream, strict=False)) + head[len(stream) :]


def video_title(video: ChannelVideo) -> str:
    """A name for the video: its caption's first line, else its author."""
    caption = video.description.splitlines()[0].strip() if video.description.strip() else ''
    return caption or video.author or video.object_id
