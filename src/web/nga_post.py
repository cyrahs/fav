"""NGA forum threads forwarded to the agent's WeChat account.

Sharing an NGA thread into WeChat (from the NGA app, 恩基爱社区) sends the same
``<appmsg>`` link card as a 公众号 article (``<type>5</type>``), with the
thread's ``bbs.nga.cn/read.php?tid=<id>`` link and its title.

NGA shows most boards to signed-in members only: a guest gets 403 未登录 from
the web pages and ``code`` 12 from the app API alike. So the thread is read with
the login cookies of a browser session (``ngaPassportUid`` and
``ngaPassportCid``), through ``read.php``'s JSON output (``__output=11``).

Only the first floor (``lou`` 0), the post itself, is kept, as for Tieba. Its
pictures are BBCode ``[img]`` tags in the content, mostly relative to the
attachment host (``./mon_YYYYMM/DD/<name>.jpg``) and often pointing at a
rendition (``<name>.jpg.medium.jpg``); the upload itself is the name without
that suffix. Uploads the post lists as attachments but does not show inline
come after the inline ones.
"""

from __future__ import annotations

import html
import re
from datetime import UTC, datetime
from typing import TYPE_CHECKING, Any
from urllib.parse import parse_qs, urljoin, urlsplit

from src.web.wechat_article import ArticleLink, ArticlePage, appmsg_link

if TYPE_CHECKING:
    import httpx

# Every hostname NGA serves the forum under; the share card uses bbs.nga.cn.
NGA_HOSTS = frozenset({'bbs.nga.cn', 'nga.cn', 'ngabbs.com', 'nga.178.com', 'bbs.ngacn.cc', 'g.nga.cn'})
_READ_URL = 'https://bbs.nga.cn/read.php'
_ATTACHMENT_BASE = 'https://img.nga.178.com/attachments/'
LOGIN_COOKIES = ('ngaPassportUid', 'ngaPassportCid')
_TID_RE = re.compile(r'\d{1,12}')
_IMG_TAG_RE = re.compile(r'\[img\]\s*(.*?)\s*\[/img\]', re.IGNORECASE | re.DOTALL)
# The renditions NGA derives from an upload, appended to its name.
_RENDITION_RE = re.compile(r'(\.(?:jpe?g|png|gif|webp|bmp))\.(?:medium|thumb|thumb_s|thumb_ss)\.jpe?g$', re.IGNORECASE)
_FIRST_FLOOR = 0


class NgaError(RuntimeError):
    pass


class NgaLoginError(NgaError):
    """NGA wants a signed-in session the cookies do not give it."""


def nga_thread_id(url: str) -> str | None:
    """The thread id of an NGA ``read.php?tid=<id>`` link, or None for anything else."""
    parts = urlsplit(url)
    if parts.scheme not in {'http', 'https'} or (parts.hostname or '') not in NGA_HOSTS:
        return None
    if parts.path.rstrip('/') not in {'/read.php', '/read'}:
        return None
    tid = (parse_qs(parts.query).get('tid') or [''])[0]
    return tid if _TID_RE.fullmatch(tid) else None


def parse_nga_link(content: str) -> ArticleLink | None:
    """The NGA thread a message shares, under its canonical URL, or None for anything else."""
    link = appmsg_link(content)
    if link is None:
        return None
    tid = nga_thread_id(link.url)
    if tid is None:
        return None
    return ArticleLink(url=f'{_READ_URL}?tid={tid}', title=link.title, description=link.description)


def original_url(src: str) -> str | None:
    """The upload an ``[img]`` tag shows, at full size; None for something that is not a picture URL."""
    src = html.unescape(src).strip()
    if src.startswith('./'):
        src = urljoin(_ATTACHMENT_BASE, src[2:])
    elif src.startswith('/attachments/'):
        src = urljoin(_ATTACHMENT_BASE, src.removeprefix('/attachments/'))
    elif src.startswith('mon_'):
        src = urljoin(_ATTACHMENT_BASE, src)
    if urlsplit(src).scheme not in {'http', 'https'}:
        return None
    return _RENDITION_RE.sub(r'\1', src)


def _attachments(row: dict[str, Any]) -> list[str]:
    attachs = row.get('attachs')
    items = attachs.values() if isinstance(attachs, dict) else attachs if isinstance(attachs, list) else []
    urls: list[str] = []
    for item in items:
        if not isinstance(item, dict) or str(item.get('type') or 'img') != 'img':
            continue
        path = item.get('attachurl')
        if isinstance(path, str) and path:
            url = original_url(path if path.startswith(('http://', 'https://', './')) else f'./{path}')
            if url:
                urls.append(url)
    return urls


def floor_images(row: dict[str, Any]) -> list[str]:
    """The pictures of one floor, inline ones first, at full size and without repeats."""
    inline = [url for src in _IMG_TAG_RE.findall(str(row.get('content') or '')) if (url := original_url(src))]
    return list(dict.fromkeys(inline + _attachments(row)))


def _rows(data: dict[str, Any]) -> list[dict[str, Any]]:
    rows = data.get('__R')
    values = rows.values() if isinstance(rows, dict) else rows if isinstance(rows, list) else []
    return [row for row in values if isinstance(row, dict)]


def _error_text(payload: Any) -> str:
    if not isinstance(payload, dict):
        return ''
    error = payload.get('error')
    if isinstance(error, list):
        error = error[0] if error else ''
    if not error and isinstance(payload.get('data'), dict):
        error = payload['data'].get('__MESSAGE')
        if isinstance(error, dict):
            error = error.get('1') or next(iter(error.values()), '')
    text = re.sub(r'<[^>]+>', '', html.unescape(str(error or ''))).strip()
    # NGA numbers its messages: '1:未登录'.
    return re.sub(r'^\d+:', '', text)


def _timestamp(value: Any) -> datetime | None:
    text = str(value or '').strip()
    return datetime.fromtimestamp(int(text), tz=UTC) if text.isdigit() else None


async def fetch_nga_post(client: httpx.AsyncClient, link: ArticleLink, cookies: dict[str, str]) -> ArticlePage:
    """The thread's first floor, as a page of its pictures, read with a signed-in session."""
    tid = nga_thread_id(link.url)
    if tid is None:
        msg = f'{link.url} is not an NGA thread'
        raise NgaError(msg)
    cookie = '; '.join(f'{name}={cookies[name]}' for name in LOGIN_COOKIES if cookies.get(name))
    response = await client.get(_READ_URL, params={'tid': tid, '__output': '11'}, headers={'Cookie': cookie})
    try:
        payload = response.json()
    except ValueError:
        payload = None
    error = _error_text(payload)
    if '登录' in error or (not isinstance(payload, dict) and response.status_code in {401, 403}):
        reason = error or f'HTTP {response.status_code}'
        msg = f'NGA wants a signed-in session for thread {tid} ({reason}); sign in to NGA in the browser and re-sync CookieCloud'
        raise NgaLoginError(msg)
    data = payload.get('data') if isinstance(payload, dict) else None
    if not isinstance(data, dict) or error:
        msg = f'NGA refused thread {tid}: {error or f"HTTP {response.status_code}"}; it may have been deleted'
        raise NgaError(msg)
    first = next((row for row in _rows(data) if str(row.get('lou')) == str(_FIRST_FLOOR)), None)
    if first is None:
        msg = f'NGA returned no first floor for thread {tid}'
        raise NgaError(msg)
    thread = data.get('__T') if isinstance(data.get('__T'), dict) else {}
    forum = data.get('__F') if isinstance(data.get('__F'), dict) else {}
    title = html.unescape(str(thread.get('subject') or first.get('subject') or '')).strip()
    return ArticlePage(
        title=title or link.title,
        account=html.unescape(str(forum.get('name') or '')).strip(),
        published_at=_timestamp(thread.get('postdate')) or _timestamp(first.get('postdatetimestamp')),
        images=floor_images(first),
    )
