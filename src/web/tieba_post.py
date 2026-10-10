"""Baidu Tieba posts forwarded to the agent's WeChat account.

Sharing a Tieba post into WeChat sends the same ``<appmsg>`` link card as a
公众号 article (``<type>5</type>``), with the post's ``tieba.baidu.com/p/<id>``
link and its title.

The web page answers the cluster with Baidu's security check, but the Android
client's API does not: ``c/f/pb/page`` returns the post as JSON. Its form is
signed with an MD5 of the sorted parameters and a constant salt, as every Tieba
client does. ``lz=1`` limits the floors to the thread author's own (只看楼主),
which is what is kept: the post and the author's continuation floors, not other
people's replies. Image items (``type`` 3) carry ``origin_src``, the original
upload on tiebapic.baidu.com with a fresh time-limited ``tbpicau`` grant.
"""

from __future__ import annotations

import hashlib
import re
from datetime import UTC, datetime
from typing import TYPE_CHECKING, Any
from urllib.parse import urlsplit

from src.web.wechat_article import ArticleLink, ArticlePage, appmsg_link

if TYPE_CHECKING:
    import httpx

_PAGE_API = 'http://c.tieba.baidu.com/c/f/pb/page'
_CLIENT_VERSION = '12.57.4.2'
_CLIENT_USER_AGENT = f'bdtb for Android {_CLIENT_VERSION}'
_SIGN_SALT = 'tiebaclient!!!'
_FLOORS_PER_PAGE = 30
# Far beyond any author's own floors in one thread; bounds a runaway loop.
_MAX_PAGES = 50
_IMAGE_ITEM = '3'
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
    link = appmsg_link(content)
    if link is None:
        return None
    thread = thread_id(link.url)
    if thread is None:
        return None
    return ArticleLink(url=f'https://tieba.baidu.com/p/{thread}', title=link.title, description=link.description)


def _signed(params: dict[str, str]) -> dict[str, str]:
    raw = ''.join(f'{key}={params[key]}' for key in sorted(params)) + _SIGN_SALT
    return {**params, 'sign': hashlib.md5(raw.encode('utf-8')).hexdigest().upper()}  # noqa: S324  (the API's own scheme)


async def _page(client: httpx.AsyncClient, thread: str, number: int) -> dict[str, Any]:
    params = {
        '_client_type': '2',
        '_client_version': _CLIENT_VERSION,
        'kz': thread,
        'lz': '1',
        'pn': str(number),
        'rn': str(_FLOORS_PER_PAGE),
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


def _author_id(post: dict[str, Any]) -> str:
    author = post.get('author')
    return str((author.get('id') if isinstance(author, dict) else None) or post.get('author_id') or '')


def _images(post: dict[str, Any]) -> list[str]:
    images: list[str] = []
    for item in post.get('content') or []:
        if not isinstance(item, dict) or str(item.get('type')) != _IMAGE_ITEM:
            continue
        url = item.get('origin_src') or item.get('big_cdn_src') or item.get('cdn_src')
        if isinstance(url, str) and url.startswith(('http://', 'https://')):
            images.append(url)
    return images


async def fetch_tieba_post(client: httpx.AsyncClient, link: ArticleLink) -> ArticlePage:
    """The thread author's floors of the post, as a page of their pictures in floor order."""
    thread = thread_id(link.url)
    if thread is None:
        msg = f'{link.url} is not a Tieba post'
        raise TiebaError(msg)
    first = await _page(client, thread, 1)
    info = first.get('thread') if isinstance(first.get('thread'), dict) else {}
    author = _author_id(info)
    forum = first.get('forum') if isinstance(first.get('forum'), dict) else {}
    created = str(info.get('create_time') or '')
    page_info = first.get('page') if isinstance(first.get('page'), dict) else {}
    total = min(int(page_info.get('total_page') or 1), _MAX_PAGES)

    images: list[str] = []
    payload = first
    for number in range(1, total + 1):
        if number > 1:
            payload = await _page(client, thread, number)
        for post in payload.get('post_list') or []:
            if not isinstance(post, dict) or (author and _author_id(post) != author):
                continue
            images.extend(url for url in _images(post) if url not in images)
    forum_name = str(forum.get('name') or '')
    return ArticlePage(
        title=str(info.get('title') or link.title),
        account=f'{forum_name}吧' if forum_name else '',
        published_at=datetime.fromtimestamp(int(created), tz=UTC) if created.isdigit() else None,
        images=images,
    )
