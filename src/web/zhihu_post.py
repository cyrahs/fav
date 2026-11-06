"""Zhihu answers, column articles and pins shared as links, read through TikHub.

Zhihu answers the cluster's addresses with HTTP 403 for every page and for its
``/api/v4`` JSON (error 40353, "please sign in"), so the post itself is looked
up through TikHub (https://tikhub.io), which signs and proxies the request:
``fetch_answer_detail``, ``fetch_column_article_detail`` and
``fetch_pin_detail``, each billed at about $0.001 a call, with the key in
``services.tikhub`` that 视频号 lookups use too.

Everything after that is public. An answer's or article's ``content`` is HTML:
each picture is an ``<img>`` whose ``data-original`` is the full-size upload on
``*.zhimg.com``, and each video is an ``<a class="video-box">`` carrying the
video's ``data-lens-id``. A pin's ``content`` is a list of items instead, its
pictures carrying ``original_url`` and its videos a ``video_id``. A video id is
resolved with ``lens.zhihu.com/api/v4/videos/<id>``, which answers without a
login with signed MP4 URLs on ``vdn*.vzuu.com`` for each quality; the widest one
is kept. Those URLs expire within the hour, so a video is downloaded as soon as
it is resolved.
"""

from __future__ import annotations

import html
import re
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import TYPE_CHECKING, Any, Literal
from urllib.parse import unquote, urlsplit

import httpx

from src.core import logger
from src.web.wechat_article import ArticlePage, PostVideo

if TYPE_CHECKING:
    from collections.abc import Mapping

log = logger.get('links')

ZhihuKind = Literal['answer', 'article', 'pin']

_TIKHUB_ENDPOINTS: dict[ZhihuKind, tuple[str, str]] = {
    'answer': ('/api/v1/zhihu/web/fetch_answer_detail', 'answer_id'),
    'article': ('/api/v1/zhihu/web/fetch_column_article_detail', 'article_id'),
    'pin': ('/api/v1/zhihu/web/fetch_pin_detail', 'pin_id'),
}
# TikHub's answers to a missing, invalid or spent key.
_TIKHUB_ACCOUNT_STATUSES = frozenset({401, 402, 403})
_LENS_API = 'https://lens.zhihu.com/api/v4/videos/{video_id}'
USER_AGENT = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0 Safari/537.36'

_ANSWER_PATH_RE = re.compile(r'/(?:question/\d+/)?answer/(\d+)')
_ARTICLE_PATH_RE = re.compile(r'/p/(\d+)')
_PIN_PATH_RE = re.compile(r'/pin/(\d+)')
_VIDEO_BOX_RE = re.compile(r'<a\b[^>]*\bclass="[^"]*\bvideo-box\b[^"]*"[^>]*>.*?</a>', re.IGNORECASE | re.DOTALL)
_LENS_ID_RE = re.compile(r'\bdata-lens-id="(\d+)"')
_VIDEO_TARGET_RE = re.compile(r'zhihu\.com/video/(\d+)')
_VIDEO_POSTER_RE = re.compile(r'\bdata-poster="([^"]+)"')
_IMG_RE = re.compile(r'<img\b[^>]*>', re.IGNORECASE)
_IMG_ATTRS = ('data-original', 'data-actualsrc', 'src')
_TAG_RE = re.compile(r'<[^>]+>')
_PIN_TITLE_BYTES = 80


class ZhihuError(RuntimeError):
    """A Zhihu post that could not be read; retried later."""


class ZhihuAccountError(ZhihuError):
    """TikHub refused the key: settings to fix rather than a failed try."""


@dataclass(frozen=True, slots=True)
class ZhihuLink:
    kind: ZhihuKind
    id: str

    @property
    def url(self) -> str:
        if self.kind == 'answer':
            return f'https://www.zhihu.com/answer/{self.id}'
        if self.kind == 'article':
            return f'https://zhuanlan.zhihu.com/p/{self.id}'
        return f'https://www.zhihu.com/pin/{self.id}'


@dataclass(frozen=True, slots=True)
class ZhihuVideo:
    id: str
    # A play URL the post itself carried (pins do); used when lens has none.
    fallback_url: str = ''
    cover_url: str = ''


@dataclass(frozen=True, slots=True)
class ZhihuPost:
    link: ZhihuLink
    title: str
    author: str
    published_at: datetime | None
    images: list[str] = field(default_factory=list)
    videos: list[ZhihuVideo] = field(default_factory=list)


def _is_zhihu_host(host: str) -> bool:
    return host == 'zhihu.com' or host.endswith('.zhihu.com')


def parse_zhihu_url(url: str) -> ZhihuLink | None:  # noqa: PLR0911
    """The answer, column article or pin a Zhihu URL points at, or None for anything else."""
    parts = urlsplit(url.strip())
    host = (parts.hostname or '').lower()
    if parts.scheme not in {'http', 'https'}:
        return None
    if host == 'link.zhihu.com':
        # Zhihu's own redirector wraps the real target in ?target=.
        target = re.search(r'(?:^|&)target=([^&]+)', parts.query)
        return parse_zhihu_url(unquote(target.group(1))) if target else None
    if not _is_zhihu_host(host):
        return None
    path = parts.path.rstrip('/')
    if host == 'zhuanlan.zhihu.com':
        match = _ARTICLE_PATH_RE.fullmatch(path)
        return ZhihuLink('article', match.group(1)) if match else None
    if match := _ANSWER_PATH_RE.fullmatch(path):
        return ZhihuLink('answer', match.group(1))
    if match := _PIN_PATH_RE.fullmatch(path):
        return ZhihuLink('pin', match.group(1))
    return None


def _is_zhimg(url: str) -> bool:
    parts = urlsplit(url)
    host = (parts.hostname or '').lower()
    return parts.scheme in {'http', 'https'} and (host == 'zhimg.com' or host.endswith('.zhimg.com'))


def _attr(tag: str, name: str) -> str:
    match = re.search(rf'\b{re.escape(name)}="([^"]*)"', tag)
    return html.unescape(match.group(1)).strip() if match else ''


def content_images(content: str) -> list[str]:
    """Full-size picture URLs in an answer's or article's HTML, in order, without video posters."""
    body = _VIDEO_BOX_RE.sub('', content or '')
    images: list[str] = []
    for tag in _IMG_RE.findall(body):
        url = next((value for value in (_attr(tag, name) for name in _IMG_ATTRS) if _is_zhimg(value)), '')
        if url:
            images.append(url)
    return list(dict.fromkeys(images))


def content_videos(content: str) -> list[ZhihuVideo]:
    """The videos embedded in an answer's or article's HTML, in order."""
    videos: dict[str, ZhihuVideo] = {}
    for box in _VIDEO_BOX_RE.findall(content or ''):
        match = _LENS_ID_RE.search(box) or _VIDEO_TARGET_RE.search(box)
        if match is None:
            continue
        poster = _VIDEO_POSTER_RE.search(box)
        cover = html.unescape(poster.group(1)) if poster else ''
        videos.setdefault(match.group(1), ZhihuVideo(id=match.group(1), cover_url=cover if _is_zhimg(cover) else ''))
    return list(videos.values())


def _best_playlist_url(playlist: object) -> str:
    """The widest entry's URL of a lens ``playlist`` (a dict by quality) or a pin's (a list)."""
    entries = list(playlist.values()) if isinstance(playlist, dict) else playlist if isinstance(playlist, list) else []
    best_url, best_key = '', (-1, -1.0)
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        url = entry.get('play_url') or entry.get('url')
        if not isinstance(url, str) or not url.startswith(('http://', 'https://')):
            continue
        key = (int(entry.get('width') or 0) * int(entry.get('height') or 0), float(entry.get('bitrate') or 0))
        if key > best_key:
            best_url, best_key = url, key
    return best_url


def _pin_media(items: object) -> tuple[list[str], list[ZhihuVideo], str]:
    images: list[str] = []
    videos: list[ZhihuVideo] = []
    texts: list[str] = []
    for item in items if isinstance(items, list) else []:
        if not isinstance(item, dict):
            continue
        kind = item.get('type')
        if kind == 'image':
            url = next((value for value in (item.get('original_url'), item.get('url')) if isinstance(value, str) and _is_zhimg(value)), '')
            if url:
                images.append(url)
        elif kind == 'video' and str(item.get('video_id') or '').isdigit():
            info = item.get('video_info') if isinstance(item.get('video_info'), dict) else {}
            fallback = _best_playlist_url(info.get('playlist') or item.get('playlist'))
            cover = item.get('thumbnail') if isinstance(item.get('thumbnail'), str) else ''
            videos.append(ZhihuVideo(id=str(item['video_id']), fallback_url=fallback, cover_url=cover if _is_zhimg(cover) else ''))
        elif kind == 'text':
            texts.append(str(item.get('own_text') or item.get('content') or ''))
    text = html.unescape(_TAG_RE.sub('', ' '.join(texts))).strip()
    return list(dict.fromkeys(images)), videos, text


def _timestamp(value: object) -> datetime | None:
    try:
        seconds = int(value)  # type: ignore[call-overload]
    except (TypeError, ValueError):
        return None
    return datetime.fromtimestamp(seconds, tz=UTC) if seconds > 0 else None


def _author(data: Mapping[str, Any]) -> str:
    author = data.get('author')
    return str(author.get('name') or '').strip() if isinstance(author, dict) else ''


def _short(text: str, max_bytes: int) -> str:
    encoded = text.encode('utf-8')
    if len(encoded) <= max_bytes:
        return text
    return encoded[:max_bytes].decode('utf-8', errors='ignore').rstrip() + '…'


def parse_post(link: ZhihuLink, data: Mapping[str, Any]) -> ZhihuPost:
    """The pictures and videos of a TikHub detail answer's ``data``."""
    author = _author(data)
    if link.kind == 'pin':
        images, videos, text = _pin_media(data.get('content'))
        # excerpt_title carries the pin's opening text as HTML, topic links and all.
        excerpt = html.unescape(_TAG_RE.sub('', str(data.get('excerpt_title') or ''))).strip()
        title = _short(excerpt or text, _PIN_TITLE_BYTES) or f'Pin {link.id}'
        return ZhihuPost(link, title, author, _timestamp(data.get('created')), images, videos)
    content = str(data.get('content') or '')
    if link.kind == 'answer':
        question = data.get('question') if isinstance(data.get('question'), dict) else {}
        title = str(question.get('title') or '').strip() or f'Answer {link.id}'
        published = _timestamp(data.get('created_time'))
    else:
        title = str(data.get('title') or '').strip() or f'Article {link.id}'
        published = _timestamp(data.get('created'))
    return ZhihuPost(link, title, author, published, content_images(content), content_videos(content))


async def fetch_post(client: httpx.AsyncClient, link: ZhihuLink, *, api_url: str, token: str) -> ZhihuPost:
    """Look the post up through TikHub."""
    if not token:
        msg = 'Zhihu links need a TikHub API key (services.tikhub)'
        raise ZhihuAccountError(msg)
    path, param = _TIKHUB_ENDPOINTS[link.kind]
    try:
        response = await client.get(
            f'{api_url.rstrip("/")}{path}',
            params={param: link.id},
            headers={'Authorization': f'Bearer {token}'},
            timeout=httpx.Timeout(30.0, read=60.0),
        )
    except httpx.HTTPError as exc:
        msg = f'TikHub did not answer for {link.url}: {exc.__class__.__name__}: {exc}'
        raise ZhihuError(msg) from exc
    if response.status_code in _TIKHUB_ACCOUNT_STATUSES:
        msg = f'TikHub refused the API key (HTTP {response.status_code}); check the key in services.tikhub and the account balance'
        raise ZhihuAccountError(msg)
    try:
        payload = response.json()
    except ValueError as exc:
        msg = f'TikHub answered {link.url} with HTTP {response.status_code} and no JSON'
        raise ZhihuError(msg) from exc
    data = payload.get('data') if isinstance(payload, dict) else None
    if response.status_code != httpx.codes.OK or not isinstance(data, dict) or not data:
        detail = payload.get('detail') if isinstance(payload, dict) else None
        msg = f'TikHub could not read {link.url} (HTTP {response.status_code}): {detail or "no data"}; it may have been deleted'
        raise ZhihuError(msg)
    error = data.get('error')
    if isinstance(error, dict):
        msg = f'Zhihu refused {link.url}: {error.get("message") or error}'
        raise ZhihuError(msg)
    return parse_post(link, data)


async def resolve_video(client: httpx.AsyncClient, video: ZhihuVideo) -> str:
    """A fresh MP4 URL for the video at its best quality, or '' when Zhihu has none."""
    try:
        response = await client.get(
            _LENS_API.format(video_id=video.id),
            headers={'User-Agent': USER_AGENT, 'Referer': 'https://www.zhihu.com/'},
            timeout=30.0,
        )
        payload = response.json() if response.status_code == httpx.codes.OK else {}
    except (httpx.HTTPError, ValueError):
        payload = {}
    url = _best_playlist_url(payload.get('playlist') if isinstance(payload, dict) else None)
    return url or video.fallback_url


async def fetch_page(client: httpx.AsyncClient, link: ZhihuLink, *, api_url: str, token: str) -> ArticlePage:
    """The post as a page of pictures and videos, each video resolved to a URL to download now."""
    post = await fetch_post(client, link, api_url=api_url, token=token)
    videos: list[PostVideo] = []
    for video in post.videos:
        url = await resolve_video(client, video)
        if url:
            videos.append(PostVideo(url, video.cover_url))
        else:
            log.info('Zhihu video %s of %s has no play URL', video.id, link.url)
    return ArticlePage(title=post.title, account=post.author, published_at=post.published_at, images=post.images, videos=videos)
