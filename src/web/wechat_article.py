"""Parsing for 公众号 (WeChat Official Account) articles forwarded to the agent's account.

A shared article reaches the FavInbox bridge as an app message (type 49) whose
content is the ``<msg><appmsg>`` XML WeChat sends; ``<type>5</type>`` inside it
marks a link share. WAuxiliary reports these as ``app``, not ``link``, so the XML
is what identifies them.

The article page itself is public: a plain GET returns the HTML with the title,
account name and publish time in inline script variables, and the body images
as ``data-src`` attributes pointing at mmbiz.qpic.cn. Those URLs end in a width
(``/640``); ``/0`` asks for the original. Image posts (the 小绿书 layout) carry
their pictures in ``picture_page_info_list`` instead of the body.

The URL has to be the one in the message, with its ``chksm``: the bare
``__biz``/``mid``/``idx``/``sn`` form is answered with a captcha page.

Image posts (贴图, ``<itemshowtype>8</itemshowtype>``) are the exception: their
share link never carries a ``chksm``, and the site answers it with Tencent's
slider captcha whatever the client. The share itself names the first picture
at full size (``coverpicimageurl``) and how many there are (``piccount``), so
that one picture is what can be saved.
"""

from __future__ import annotations

import html
import re
from dataclasses import dataclass, field
from datetime import UTC, datetime
from urllib.parse import parse_qs, urlsplit

_APPMSG_RE = re.compile(r'<appmsg\b.*?</appmsg>', re.DOTALL)
_ARTICLE_HOST = 'mp.weixin.qq.com'
_LINK_SHARE_TYPE = '5'
IMAGE_POST = '8'
# Paths the site redirects a request it does not trust to.
CAPTCHA_PATHS = ('/mp/wappoc_appmsgcaptcha', '/mp/verifycode')

_IMAGE_HOST_RE = r'https?://mmbiz\.qpic\.cn/[^"\'\s<>]+'
_BODY_IMAGE_RE = re.compile(rf'<img\b[^>]*?\bdata-src="({_IMAGE_HOST_RE})"', re.IGNORECASE)
_PICTURE_PAGE_RE = re.compile(rf'cdn_url\s*:\s*[\'"]({_IMAGE_HOST_RE})[\'"]')
_WIDTH_SEGMENT_RE = re.compile(r'/\d+(?=(\?|$))')
_EXTENSIONS = {'jpeg': 'jpg', 'jpg': 'jpg', 'png': 'png', 'gif': 'gif', 'webp': 'webp', 'bmp': 'bmp'}


@dataclass(frozen=True, slots=True)
class ArticleLink:
    url: str
    title: str
    description: str = ''
    account: str = ''
    # From the share's <mmreadershare>: 8 marks an image post.
    show_type: str = ''
    picture_count: int = 0
    cover_url: str = ''


@dataclass(frozen=True, slots=True)
class ArticlePage:
    title: str
    account: str
    published_at: datetime | None
    images: list[str] = field(default_factory=list)
    # How many pictures the post has, when only some of them could be read.
    picture_count: int = 0


def _tag(xml: str, name: str) -> str:
    """The text of the first ``<name>`` element, with CDATA and entities undone."""
    match = re.search(rf'<{name}>(.*?)</{name}>', xml, re.DOTALL)
    if match is None:
        return ''
    text = match.group(1).strip()
    if text.startswith('<![CDATA[') and text.endswith(']]>'):
        text = text[9:-3]
    return html.unescape(text).strip()


def parse_article_link(content: str) -> ArticleLink | None:
    """The 公众号 article a message shares, or None for anything else."""
    appmsg = _APPMSG_RE.search(content or '')
    if appmsg is None:
        return None
    xml = appmsg.group(0)
    # The first <type> belongs to appmsg itself; nested elements come later.
    if _tag(xml, 'type') != _LINK_SHARE_TYPE:
        return None
    url = _tag(xml, 'url')
    parts = urlsplit(url)
    if parts.scheme not in {'http', 'https'} or parts.hostname != _ARTICLE_HOST:
        return None
    if parts.scheme == 'http':
        url = 'https' + url[4:]
    cover = _tag(xml, 'coverpicimageurl')
    count = _tag(xml, 'piccount')
    return ArticleLink(
        url=url,
        title=_tag(xml, 'title'),
        description=_tag(xml, 'des'),
        account=_tag(xml, 'sourcedisplayname'),
        show_type=_tag(xml, 'itemshowtype'),
        picture_count=int(count) if count.isdigit() else 0,
        cover_url=cover if re.fullmatch(_IMAGE_HOST_RE, cover) else '',
    )


def cover_only_page(link: ArticleLink) -> ArticlePage | None:
    """What can be saved of an image post whose page is behind the captcha: its first picture."""
    if link.show_type != IMAGE_POST or not link.cover_url:
        return None
    return ArticlePage(
        title=link.title,
        account=link.account,
        published_at=None,
        images=[link.cover_url],
        picture_count=max(link.picture_count, 1),
    )


def canonical_url(url: str) -> str:
    """The article's URL without the sharer's tracking parameters, for links and display.

    Short links (``/s/<id>``) are already canonical. Fetching should still use the
    URL from the message, which carries the ``chksm`` the site checks.
    """
    parts = urlsplit(url)
    query = parse_qs(parts.query)
    keys = ('__biz', 'mid', 'idx', 'sn')
    if parts.path != '/s' or not all(query.get(key) for key in keys):
        return f'https://{_ARTICLE_HOST}{parts.path}'
    params = '&'.join(f'{key}={query[key][0]}' for key in keys)
    return f'https://{_ARTICLE_HOST}/s?{params}'


def _script_string(page: str, pattern: str) -> str:
    match = re.search(pattern, page)
    return html.unescape(match.group(1)).strip() if match else ''


def parse_article_page(page: str) -> ArticlePage:
    title = _script_string(page, r"var msg_title = '(.*?)'") or _script_string(page, r'<meta property="og:title" content="(.*?)"')
    account = _script_string(page, r'var nickname = htmlDecode\("(.*?)"\)')
    published = re.search(r'var ct = "(\d+)"', page)
    published_at = datetime.fromtimestamp(int(published.group(1)), tz=UTC) if published else None

    start = page.find('id="js_content"')
    body = page[start:] if start >= 0 else ''
    found = _BODY_IMAGE_RE.findall(body) or _PICTURE_PAGE_RE.findall(page)
    images: list[str] = []
    for raw in found:
        url = html.unescape(raw)
        if url not in images:
            images.append(url)
    return ArticlePage(title=title, account=account, published_at=published_at, images=images)


def original_image_url(url: str) -> str:
    """Ask mmbiz for the full-size image rather than the 640px rendition."""
    parts = urlsplit(url)
    path = _WIDTH_SEGMENT_RE.sub('/0', parts.path, count=1)
    return parts._replace(path=path).geturl()


def image_extension(url: str, content_type: str) -> str:
    kind = content_type.split(';', 1)[0].strip().lower()
    if kind.startswith('image/') and kind[6:] in _EXTENSIONS:
        return _EXTENSIONS[kind[6:]]
    fmt = parse_qs(urlsplit(url).query).get('wx_fmt', [''])[0].lower()
    return _EXTENSIONS.get(fmt, 'jpg')


def is_captcha(url: str) -> bool:
    return urlsplit(url).path in CAPTCHA_PATHS
