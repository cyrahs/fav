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
slider captcha whatever the client. WeChat itself opens them with its login,
so the FavInbox service has the agent's WeChat open the post and hands back the
page from its web view's cache. Failing that, the share names the first picture
at full size (``coverpicimageurl``) and how many there are (``piccount``), so
that one picture is what can be saved.

An image post's page lists its pictures in ``picture_page_info_list``, a script
array of objects. Each object's own ``cdn_url`` is the picture; objects nested
in it (``watermark_info`` and the like) carry other ``cdn_url`` values, which
is why the list is read by structure rather than by pattern.
"""

from __future__ import annotations

import html
import re
from dataclasses import dataclass, field, replace
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
_PICTURE_LIST_RE = re.compile(r'picture_page_info_list[\'"]?\s*[:=]\s*\[')
_CDN_URL_RE = re.compile(r'(?<![\w$])[\'"]?cdn_url[\'"]?\s*:\s*([\'"])(.*?)(?<!\\)\1', re.DOTALL)
_JS_ESCAPE_RE = re.compile(r'\\(x[0-9a-fA-F]{2}|u[0-9a-fA-F]{4}|.)', re.DOTALL)
# Script source split into strings, brackets and everything else, so brackets
# inside strings do not count.
_JS_TOKEN_RE = re.compile(r"""'(?:\\.|[^'\\])*'|"(?:\\.|[^"\\])*"|`(?:\\.|[^`\\])*`|[{}\[\]]|[^'"`{}\[\]]+|.""", re.DOTALL)
# Inside the array (1) and one of its elements (2).
_ELEMENT_DEPTH = 2
# Far more than any page's picture list; bounds the scan on a malformed page.
_MAX_LIST_CHARS = 4_000_000
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


def appmsg_link(content: str) -> ArticleLink | None:
    """The link a link-card message shares, whatever site it points at, or None for anything else."""
    appmsg = _APPMSG_RE.search(content or '')
    if appmsg is None:
        return None
    xml = appmsg.group(0)
    # The first <type> belongs to appmsg itself; nested elements come later.
    if _tag(xml, 'type') != _LINK_SHARE_TYPE:
        return None
    url = _tag(xml, 'url')
    if urlsplit(url).scheme not in {'http', 'https'}:
        return None
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


def parse_article_link(content: str) -> ArticleLink | None:
    """The 公众号 article a message shares, or None for anything else."""
    link = appmsg_link(content)
    if link is None or urlsplit(link.url).hostname != _ARTICLE_HOST:
        return None
    if link.url.startswith('http:'):
        return replace(link, url='https' + link.url[4:])
    return link


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
    if parts.hostname != _ARTICLE_HOST:
        return f'{parts.scheme}://{parts.netloc}{parts.path}'
    query = parse_qs(parts.query)
    keys = ('__biz', 'mid', 'idx', 'sn')
    if parts.path != '/s' or not all(query.get(key) for key in keys):
        return f'https://{_ARTICLE_HOST}{parts.path}'
    params = '&'.join(f'{key}={query[key][0]}' for key in keys)
    return f'https://{_ARTICLE_HOST}/s?{params}'


def _script_string(page: str, pattern: str) -> str:
    match = re.search(pattern, page)
    return html.unescape(match.group(1)).strip() if match else ''


def _js_unescape(text: str) -> str:
    def replace(match: re.Match[str]) -> str:
        escape = match.group(1)
        if len(escape) > 1:
            return chr(int(escape[1:], 16))
        return {'n': '\n', 't': '\t', 'r': '\r'}.get(escape, escape)

    return html.unescape(_JS_ESCAPE_RE.sub(replace, text))


def _top_level_objects(page: str, start: int) -> list[str]:
    """The text of each object in the script array opening at ``start``, nested values left out."""
    objects: list[str] = []
    current: list[str] = []
    depth = 1  # inside the array
    for token in _JS_TOKEN_RE.finditer(page, start, min(len(page), start + _MAX_LIST_CHARS)):
        text = token.group()
        if text in {'{', '['}:
            depth += 1
            current = [] if depth == _ELEMENT_DEPTH else current
        elif text in {'}', ']'}:
            depth -= 1
            if depth == _ELEMENT_DEPTH - 1:
                objects.append(''.join(current))
            elif depth == 0:
                break
        elif depth == _ELEMENT_DEPTH:
            current.append(text)
    return objects


def picture_list(page: str) -> list[str]:
    """The pictures of an image post, in order, from its ``picture_page_info_list``."""
    for match in _PICTURE_LIST_RE.finditer(page):
        urls: list[str] = []
        for item in _top_level_objects(page, match.end()):
            found = _CDN_URL_RE.search(item)
            url = _js_unescape(found.group(2)).strip() if found else ''
            if re.fullmatch(_IMAGE_HOST_RE, url) and url not in urls:
                urls.append(url)
        if urls:
            return urls
    return []


def parse_article_page(page: str) -> ArticlePage:
    title = _script_string(page, r"var msg_title = '(.*?)'") or _script_string(page, r'<meta property="og:title" content="(.*?)"')
    account = _script_string(page, r'var nickname = htmlDecode\("(.*?)"\)')
    published = re.search(r'var ct = "(\d+)"', page)
    published_at = datetime.fromtimestamp(int(published.group(1)), tz=UTC) if published else None

    # An image post's body is just its caption; the pictures are in the list.
    images = picture_list(page)
    if not images:
        start = page.find('id="js_content"')
        body = page[start:] if start >= 0 else ''
        for raw in _BODY_IMAGE_RE.findall(body):
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
