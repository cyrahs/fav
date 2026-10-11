"""视频号 (WeChat Channels) videos forwarded to the agent's WeChat account.

Forwarding a 视频号 video sends a type-51 message whose ``<finderFeed>`` names the
feed: ``objectId`` (an unsigned 64-bit number), the ``objectNonceId`` WeChat
needs to open it, the author's nickname, the caption, and per media item its
``mediaType`` (4 for a video), size, duration and cover. The video address in
it carries no token or key, so it cannot be downloaded from outside WeChat;
the FavInbox service has the agent's WeChat open the feed and hands over the
MP4 its player cached (``POST /finder/video``).
"""

from __future__ import annotations

import html
import re
from dataclasses import dataclass
from urllib.parse import urlsplit

_FEED_RE = re.compile(r'<finderFeed>.*?</finderFeed>', re.DOTALL)
_MEDIA_RE = re.compile(r'<media>.*?</media>', re.DOTALL)
_VIDEO_MEDIA = '4'
_OBJECT_ID_RE = re.compile(r'\d{1,20}')
_NONCE_ID_RE = re.compile(r'[A-Za-z0-9_-]{1,200}')


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


def video_title(video: ChannelVideo) -> str:
    """A name for the video: its caption's first line, else its author."""
    caption = video.description.splitlines()[0].strip() if video.description.strip() else ''
    return caption or video.author or video.object_id
