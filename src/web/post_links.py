"""Links to posts, saved the same way whichever source the link arrives through.

WeChat (a shared link card) and Telegram (a message's text or link preview) both
hand over links to the same kinds of post, and this module is the one place that
knows them: 公众号 articles (``wechat_article``), Tieba posts (``tieba_post``),
NGA threads (``nga_post``) and Zhihu answers, column articles and pins
(``zhihu_post``). It recognises a link, reads the post as an ``ArticlePage`` of
pictures and videos, and downloads those. Where the files go, and how the save is
recorded and announced, stays with each source.

Two of the kinds need something only a person can set up: an NGA thread is read
with the NGA login captured on the 登录态 page, and a Zhihu post is looked up
through TikHub with the key in ``services.tikhub``. Without either, the error
says so in ``needs``, so a source can wait for it rather than spend its tries.
A 公众号 image post is put behind a captcha whatever the client; that is
``PostCaptchaError``, which WeChat answers by opening the post in WeChat itself.
"""

from __future__ import annotations

import asyncio
import re
from typing import TYPE_CHECKING, Literal
from urllib.parse import urlsplit

import httpx

from src.core import logger, settings
from src.tool import site_sessions
from src.web import nga_post, tieba_post, zhihu_post
from src.web.nga_post import NgaError, NgaLoginError
from src.web.tieba_post import TiebaError
from src.web.wechat_article import (
    LINK_SHARE_TYPE,
    VIDEO_SHARE_TYPE,
    ArticleLink,
    ArticlePage,
    PostVideo,
    appmsg_link,
    canonical_url,
    image_extension,
    is_captcha,
    original_image_url,
    parse_article_link,
    parse_article_page,
)
from src.web.zhihu_post import ZhihuAccountError, ZhihuError

if TYPE_CHECKING:
    from pathlib import Path

log = logger.get('links')

# What a failure waits on: a login to capture or a key to set, not another try.
Needs = Literal['', 'nga-login', 'tikhub']

_ARTICLE_HOST = 'mp.weixin.qq.com'
_REQUEST_TIMEOUT_SECONDS = 60.0
# How long a post's video may take to download from its CDN.
VIDEO_READ_SECONDS = 600.0
# A phone browser, as a share link would open in. mp.weixin.qq.com serves the
# full page to it without a login.
USER_AGENT = 'Mozilla/5.0 (Linux; Android 12) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36'
_URL_RE = re.compile(r'https?://[^\s<>"\'\]\uff09\u3011]+', re.IGNORECASE)
_MAGIC_EXTENSIONS = ((b'\xff\xd8\xff', 'jpg'), (b'\x89PNG', 'png'), (b'GIF8', 'gif'))


class PostLinkError(RuntimeError):
    """A linked post that could not be read or saved."""

    def __init__(self, message: str, *, needs: Needs = '') -> None:
        super().__init__(message)
        self.needs = needs


class PostCaptchaError(PostLinkError):
    """mp.weixin.qq.com answered with its captcha, as it does every 公众号 image post."""


def new_client() -> httpx.AsyncClient:
    """The client posts are read and downloaded with.

    The cluster reaches these sites directly, so a deployment-wide proxy is not used.
    No Referer: mmbiz answers an image request carrying a foreign one with a 140x140
    placeholder.
    """
    return httpx.AsyncClient(
        headers={'User-Agent': USER_AGENT},
        timeout=_REQUEST_TIMEOUT_SECONDS,
        follow_redirects=True,
        trust_env=False,
    )


def link_for_url(url: str, *, title: str = '', description: str = '') -> ArticleLink | None:
    """The post a URL points at, under its canonical URL, or None for a site no source saves posts of.

    A 公众号 article keeps the URL as given: the site checks the ``chksm`` it carries.
    """
    url = url.strip()
    parts = urlsplit(url)
    if parts.scheme not in {'http', 'https'}:
        return None
    if parts.hostname == _ARTICLE_HOST:
        return ArticleLink(url='https' + url[4:] if url.startswith('http:') else url, title=title, description=description)
    if thread := tieba_post.thread_id(url):
        return ArticleLink(url=f'https://tieba.baidu.com/p/{thread}', title=title, description=description)
    if tid := nga_post.nga_thread_id(url):
        return ArticleLink(url=f'{nga_post.READ_URL}?tid={tid}', title=title, description=description)
    if zhihu := zhihu_post.parse_zhihu_url(url):
        return ArticleLink(url=zhihu.url, title=title, description=description)
    return None


def find_link(*texts: str) -> ArticleLink | None:
    """The first saveable post linked from any of ``texts``, such as a message and its link preview."""
    for text in texts:
        for match in _URL_RE.finditer(text or ''):
            link = link_for_url(match.group(0))
            if link is not None:
                return link
    return None


def shared_link(content: str) -> ArticleLink | None:
    """The post a WeChat link card shares, if it is one this module reads.

    A 公众号 article keeps everything its card says (an image post's picture count
    and cover among it); a post on another site is read from its link alone. The
    Tieba app shares a video post as a video card rather than a link card.
    """
    article = parse_article_link(content)
    if article is not None:
        return article
    card = appmsg_link(content, (LINK_SHARE_TYPE, VIDEO_SHARE_TYPE))
    if card is None or urlsplit(card.url).hostname == _ARTICLE_HOST:
        return None
    return link_for_url(card.url, title=card.title, description=card.description)


def site_name(link: ArticleLink) -> str:
    """Which site a link's post is on, for logs and messages."""
    if tieba_post.thread_id(link.url):
        return 'Tieba'
    if nga_post.nga_thread_id(link.url):
        return 'NGA'
    if zhihu_post.parse_zhihu_url(link.url):
        return 'Zhihu'
    return 'WeChat article'


async def _nga_cookies() -> dict[str, str]:
    stored = await site_sessions.load(nga_post.SESSION_SITE)
    login = nga_post.login_cookies(stored.cookies) if stored is not None else {}
    if len(login) < len(nga_post.LOGIN_COOKIES):
        msg = f'An NGA thread needs an NGA login, and NGA is not signed in. {nga_post.RELOGIN_HINT}'
        raise PostLinkError(msg, needs='nga-login')
    return login


async def _nga_page(client: httpx.AsyncClient, link: ArticleLink) -> ArticlePage:
    try:
        page = await nga_post.fetch_nga_post(client, link, await _nga_cookies())
    except NgaLoginError as exc:
        await site_sessions.record_check(nga_post.SESSION_SITE, ok=False, message=str(exc))
        raise PostLinkError(str(exc), needs='nga-login') from exc
    except NgaError as exc:
        raise PostLinkError(str(exc)) from exc
    await site_sessions.record_check(nga_post.SESSION_SITE, ok=True)
    return page


async def _zhihu_page(client: httpx.AsyncClient, link: ArticleLink) -> ArticlePage:
    zhihu = zhihu_post.parse_zhihu_url(link.url)
    if zhihu is None:
        msg = f'{link.url} is not a Zhihu post'
        raise PostLinkError(msg)
    tikhub = settings.load().services.tikhub
    try:
        return await zhihu_post.fetch_page(client, zhihu, api_url=tikhub.api_url, token=tikhub.token)
    except ZhihuAccountError as exc:
        raise PostLinkError(str(exc), needs='tikhub') from exc
    except ZhihuError as exc:
        raise PostLinkError(str(exc)) from exc


async def fetch_page(client: httpx.AsyncClient, link: ArticleLink) -> tuple[ArticlePage, str]:
    """The linked post as pictures and videos, with the page's HTML where one was read."""
    if tieba_post.thread_id(link.url) is not None:
        try:
            return await tieba_post.fetch_tieba_post(client, link), ''
        except TiebaError as exc:
            raise PostLinkError(str(exc)) from exc
    if nga_post.nga_thread_id(link.url) is not None:
        return await _nga_page(client, link), ''
    if zhihu_post.parse_zhihu_url(link.url) is not None:
        return await _zhihu_page(client, link), ''
    try:
        response = await client.get(link.url)
    except httpx.HTTPError as exc:
        msg = f'Could not read {canonical_url(link.url)}: {exc}'
        raise PostLinkError(msg) from exc
    if is_captcha(str(response.url)):
        msg = f'mp.weixin.qq.com answered {canonical_url(link.url)} with a captcha page'
        raise PostCaptchaError(msg)
    if response.status_code != httpx.codes.OK:
        msg = f'Could not read {canonical_url(link.url)}: HTTP {response.status_code}'
        raise PostLinkError(msg)
    return parse_article_page(response.text), response.text


def sniff_extension(content: bytes) -> str | None:
    for magic, ext in _MAGIC_EXTENSIONS:
        if content.startswith(magic):
            return ext
    if content[:4] == b'RIFF' and content[8:12] == b'WEBP':
        return 'webp'
    return None


async def fetch_image(client: httpx.AsyncClient, url: str) -> tuple[bytes, str] | None:
    """One of a post's pictures at full size, else as the page shows it, with its extension; None when neither loads."""
    for candidate in dict.fromkeys((original_image_url(url), url)):
        try:
            response = await client.get(candidate)
            response.raise_for_status()
        except httpx.HTTPError as exc:
            log.info('Post image %s failed: %s', candidate, exc)
            continue
        content_type = response.headers.get('content-type', '')
        if response.headers.get('error-message'):
            # Tieba's answer to an expired grant: its logo, with this header.
            log.info('Post image %s refused: %s', candidate, response.headers['error-message'])
            continue
        if response.content and content_type.lower().startswith('image/'):
            return response.content, sniff_extension(response.content) or image_extension(candidate, content_type)
    return None


def file_head(path: Path) -> bytes:
    with path.open('rb') as fh:
        return fh.read(16)


def is_mp4(head: bytes) -> bool:
    return head[4:8] == b'ftyp'


async def write_stream(response: httpx.Response, path: Path) -> None:
    """Write a streamed response body to ``path``, creating its folder."""
    await asyncio.to_thread(path.parent.mkdir, parents=True, exist_ok=True)
    fh = await asyncio.to_thread(path.open, 'wb')
    try:
        async for chunk in response.aiter_bytes():
            await asyncio.to_thread(fh.write, chunk)
    finally:
        await asyncio.to_thread(fh.close)


async def fetch_video(client: httpx.AsyncClient, video: PostVideo, partial: Path) -> bool:
    """Download one of a post's videos to ``partial``; False, with nothing left behind, when it does not load as an MP4."""
    try:
        async with client.stream('GET', video.url, timeout=httpx.Timeout(_REQUEST_TIMEOUT_SECONDS, read=VIDEO_READ_SECONDS)) as response:
            response.raise_for_status()
            await write_stream(response, partial)
    except httpx.HTTPError as exc:
        log.info('Post video %s failed: %s', video.url, exc)
        await asyncio.to_thread(partial.unlink, missing_ok=True)
        return False
    if not is_mp4(await asyncio.to_thread(file_head, partial)):
        log.info('Post video %s is not an MP4', video.url)
        await asyncio.to_thread(partial.unlink, missing_ok=True)
        return False
    return True
