"""Baidu Tieba posts forwarded to the agent's WeChat account.

Sharing a Tieba post into WeChat sends the same ``<appmsg>`` link card as a
公众号 article (``<type>5</type>``), with the post's ``tieba.baidu.com/p/<id>``
link and its title. A video post is shared as a video card (``<type>4</type>``)
with the same link.

The web page answers the cluster with Baidu's security check, but the Android
client's API does not: ``c/f/pb/page`` returns the post as JSON. Its form is
signed with an MD5 of the sorted parameters and a constant salt, as every Tieba
client does. Only the first floor, the post itself, is kept: neither replies
nor the author's later floors. Image items (``type`` 3) carry ``origin_src``,
the original upload on tiebapic.baidu.com with a fresh time-limited ``tbpicau``
grant. Video items (``type`` 5) carry the MP4 in ``link``: a transcode on
tb-video.bdstatic.com, signed with a ``tbau`` grant that lasts about a day, so it
is downloaded as soon as it is read. The original upload is not signed for, and
the CDN answers it with 401.
"""

from __future__ import annotations

import hashlib
import re
from datetime import UTC, datetime
from typing import TYPE_CHECKING, Any
from urllib.parse import urlsplit

from src.web.wechat_article import LINK_SHARE_TYPE, VIDEO_SHARE_TYPE, ArticleLink, ArticlePage, PostVideo, appmsg_link

if TYPE_CHECKING:
    import httpx

_PAGE_API = 'http://c.tieba.baidu.com/c/f/pb/page'
_CLIENT_VERSION = '12.57.4.2'
_CLIENT_USER_AGENT = f'bdtb for Android {_CLIENT_VERSION}'
_SIGN_SALT = 'tiebaclient!!!'
# The first page of floors; only floor 1 is used.
_FLOORS = 2
_IMAGE_ITEM = '3'
_VIDEO_ITEM = '5'
_THREAD_PATH_RE = re.compile(r'/p/(\d+)')


class TiebaError(RuntimeError):
    pass


def thread_id(url: str) -> str | None:
    """The thread id of a ``tieba.baidu.com/p/<id>`` link, or None for anything else."""
    parts = urlsplit(url)
    host = parts.hostname or ''
    if parts.scheme not in {'http', 'https'} or not (host == 'tieba.baidu.com' or host.endswith('.tieba.baidu.com')):
        return None
    match = _THREAD_PATH_RE.fullmatch(parts.path.rstrip('/'))
    return match.group(1) if match else None


def parse_tieba_link(content: str) -> ArticleLink | None:
    """The Tieba post a message shares, under its canonical URL, or None for anything else."""
    link = appmsg_link(content, (LINK_SHARE_TYPE, VIDEO_SHARE_TYPE))
    if link is None:
        return None
    thread = thread_id(link.url)
    if thread is None:
        return None
    return ArticleLink(url=f'https://tieba.baidu.com/p/{thread}', title=link.title, description=link.description)


def _signed(params: dict[str, str]) -> dict[str, str]:
    raw = ''.join(f'{key}={params[key]}' for key in sorted(params)) + _SIGN_SALT
    return {**params, 'sign': hashlib.md5(raw.encode('utf-8')).hexdigest().upper()}  # noqa: S324  (the API's own scheme)


async def _first_page(client: httpx.AsyncClient, thread: str) -> dict[str, Any]:
    params = {
        '_client_type': '2',
        '_client_version': _CLIENT_VERSION,
        'kz': thread,
        'pn': '1',
        'rn': str(_FLOORS),
    }
    response = await client.post(_PAGE_API, data=_signed(params), headers={'User-Agent': _CLIENT_USER_AGENT})
    response.raise_for_status()
    try:
        payload = response.json()
    except ValueError as exc:
        msg = f'Tieba answered thread {thread} with something other than JSON'
        raise TiebaError(msg) from exc
    if not isinstance(payload, dict) or str(payload.get('error_code')) != '0':
        error = payload.get('error_msg') if isinstance(payload, dict) else None
        msg = f'Tieba refused thread {thread}: {error or "unknown error"}; it may have been deleted'
        raise TiebaError(msg)
    return payload


def _images(post: dict[str, Any]) -> list[str]:
    images: list[str] = []
    for item in post.get('content') or []:
        if not isinstance(item, dict) or str(item.get('type')) != _IMAGE_ITEM:
            continue
        url = item.get('origin_src') or item.get('big_cdn_src') or item.get('cdn_src')
        if isinstance(url, str) and url.startswith(('http://', 'https://')):
            images.append(url)
    return images


def _is_web_url(value: object) -> bool:
    return isinstance(value, str) and value.startswith(('http://', 'https://'))


def _videos(post: dict[str, Any], thread: dict[str, Any]) -> list[PostVideo]:
    """The post's videos; a video post's own one is also in the thread's ``video_info``."""
    videos: dict[str, PostVideo] = {}
    for item in post.get('content') or []:
        if isinstance(item, dict) and str(item.get('type')) == _VIDEO_ITEM and _is_web_url(item.get('link')):
            cover = item.get('src')
            videos.setdefault(item['link'], PostVideo(item['link'], cover if _is_web_url(cover) else ''))
    info = thread.get('video_info')
    if not videos and isinstance(info, dict) and _is_web_url(info.get('video_url')):
        cover = info.get('thumbnail_url')
        videos[info['video_url']] = PostVideo(info['video_url'], cover if _is_web_url(cover) else '')
    return list(videos.values())


async def fetch_tieba_post(client: httpx.AsyncClient, link: ArticleLink) -> ArticlePage:
    """The post's first floor, as a page of its pictures and videos."""
    thread = thread_id(link.url)
    if thread is None:
        msg = f'{link.url} is not a Tieba post'
        raise TiebaError(msg)
    payload = await _first_page(client, thread)
    info = payload.get('thread') if isinstance(payload.get('thread'), dict) else {}
    forum = payload.get('forum') if isinstance(payload.get('forum'), dict) else {}
    created = str(info.get('create_time') or '')
    posts = [post for post in payload.get('post_list') or [] if isinstance(post, dict)]
    first = next((post for post in posts if str(post.get('floor')) == '1'), None)
    if first is None:
        msg = f'Tieba returned no first floor for thread {thread}'
        raise TiebaError(msg)
    forum_name = str(forum.get('name') or '')
    return ArticlePage(
        title=str(info.get('title') or link.title),
        account=f'{forum_name}吧' if forum_name else '',
        published_at=datetime.fromtimestamp(int(created), tz=UTC) if created.isdigit() else None,
        images=list(dict.fromkeys(_images(first))),
        videos=_videos(first, info),
    )
