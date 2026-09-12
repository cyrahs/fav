"""Images behind a link card forwarded to 文件传输助手.

A forwarded 公众号 article or web page arrives as an app message with a URL and
nothing else; the pictures live on the page. This fetches the page, lifts the
image URLs out of it and downloads the ones that are worth keeping.

公众号 pages get special handling because they are the common case: content
images are lazy-loaded through ``data-src``, sized by a path segment
(``/640?wx_fmt=jpeg``) that ``/0`` turns into the original, and 图片消息 pages
list their pictures in a script block rather than in ``<img>`` tags.
"""

from __future__ import annotations

import asyncio
import html
import re
from typing import TYPE_CHECKING
from urllib.parse import parse_qsl, urlencode, urljoin, urlsplit, urlunsplit

import httpx

from src.core import logger
from src.tool import ensure_unique_path

if TYPE_CHECKING:
    from pathlib import Path

log = logger.get('wechat-link')

USER_AGENT = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36'
MIN_IMAGE_BYTES = 8 * 1024
MAX_IMAGES_PER_LINK = 300
_MP_HOSTS = {'mp.weixin.qq.com'}
_MMBIZ_HOST_SUFFIX = 'qpic.cn'
_IMG_TAG_RE = re.compile(r'<img\b[^>]*>', re.IGNORECASE)
_ATTR_RE = re.compile(r"""(data-src|data-original|src)\s*=\s*["']([^"']+)["']""", re.IGNORECASE)
_OG_IMAGE_RE = re.compile(
    r"""<meta\b[^>]*(?:property|name)=["']og:image["'][^>]*content=["']([^"']+)["']|<meta\b[^>]*content=["']([^"']+)["'][^>]*(?:property|name)=["']og:image["']""",
    re.IGNORECASE,
)
_PICTURE_PAGE_RE = re.compile(r"""cdn_url\s*:\s*['"]([^'"]+)['"]""")
_SKIP_SCHEMES = ('data:', 'javascript:', 'blob:')
_SKIP_SUFFIXES = ('.svg',)
# Query keys that only pick a delivery format (webp) or track the referrer; the
# same object without them is the picture as uploaded.
_MMBIZ_DROP_PARAMS = {'tp', 'wxfrom', 'wx_lazy', 'wx_co', 'retryload'}
_MMBIZ_SIZE_SEGMENT_RE = re.compile(r'/(\d+)$')
_IMAGE_SIGNATURES: tuple[tuple[bytes, str], ...] = (
    (b'\xff\xd8\xff', 'jpg'),
    (b'\x89PNG\r\n\x1a\n', 'png'),
    (b'GIF87a', 'gif'),
    (b'GIF89a', 'gif'),
)
_WEBP_RIFF = b'RIFF'
_WEBP_TAG = b'WEBP'
_WEBP_TAG_OFFSET = 8
_SNIFF_BYTES = 16
_CONTENT_TYPE_EXT = {'image/jpeg': 'jpg', 'image/png': 'png', 'image/gif': 'gif', 'image/webp': 'webp', 'image/bmp': 'bmp'}


def sniff_image_extension(head: bytes, *, fallback: str = 'jpg') -> str:
    for signature, ext in _IMAGE_SIGNATURES:
        if head.startswith(signature):
            return ext
    if head.startswith(_WEBP_RIFF) and head[_WEBP_TAG_OFFSET : _WEBP_TAG_OFFSET + len(_WEBP_TAG)] == _WEBP_TAG:
        return 'webp'
    return fallback


def is_mp_article(url: str) -> bool:
    return (urlsplit(url).hostname or '').lower() in _MP_HOSTS


def _normalize_mmbiz(url: str) -> str:
    """Ask the 公众号 image host for the original: ``/640?wx_fmt=jpeg`` → ``/0?wx_fmt=jpeg``."""
    parts = urlsplit(url)
    if not (parts.hostname or '').endswith(_MMBIZ_HOST_SUFFIX):
        return url
    path = _MMBIZ_SIZE_SEGMENT_RE.sub('/0', parts.path)
    query = urlencode([(k, v) for k, v in parse_qsl(parts.query, keep_blank_values=True) if k not in _MMBIZ_DROP_PARAMS])
    return urlunsplit((parts.scheme, parts.netloc, path, query, ''))


def extract_image_urls(page_html: str, page_url: str) -> list[str]:
    """Image URLs on the page, in document order, absolute, deduplicated."""
    found: list[str] = []
    seen: set[str] = set()

    def add(raw: str) -> None:
        candidate = html.unescape(raw).strip()
        if not candidate or candidate.lower().startswith(_SKIP_SCHEMES):
            return
        absolute = urljoin(page_url, candidate)
        parts = urlsplit(absolute)
        if parts.scheme not in ('http', 'https') or parts.path.lower().endswith(_SKIP_SUFFIXES):
            return
        absolute = _normalize_mmbiz(urlunsplit((parts.scheme, parts.netloc, parts.path, parts.query, '')))
        if absolute in seen:
            return
        seen.add(absolute)
        found.append(absolute)

    if is_mp_article(page_url):
        for match in _PICTURE_PAGE_RE.finditer(page_html):
            add(match.group(1))
    for tag in _IMG_TAG_RE.findall(page_html):
        attrs = {key.lower(): value for key, value in _ATTR_RE.findall(tag)}
        source = attrs.get('data-src') or attrs.get('data-original') or attrs.get('src')
        if source:
            add(source)
    for match in _OG_IMAGE_RE.finditer(page_html):
        add(match.group(1) or match.group(2) or '')
    return found[:MAX_IMAGES_PER_LINK]


def build_client() -> httpx.AsyncClient:
    return httpx.AsyncClient(follow_redirects=True, headers={'User-Agent': USER_AGENT}, timeout=httpx.Timeout(20.0, read=90.0))


async def fetch_page(client: httpx.AsyncClient, url: str) -> tuple[str, str, str]:
    """Return ``(final_url, content_type, text)``; a non-HTML answer has empty text."""
    response = await client.get(url)
    response.raise_for_status()
    content_type = response.headers.get('content-type', '').split(';')[0].strip().lower()
    if content_type.startswith('image/'):
        return str(response.url), content_type, ''
    return str(response.url), content_type, response.text


async def download_link_images(
    client: httpx.AsyncClient,
    url: str,
    destination_dir: Path,
    *,
    min_bytes: int = MIN_IMAGE_BYTES,
) -> list[Path]:
    """Save every image the page at ``url`` shows into ``destination_dir``; returns the saved paths.

    Icons, spacers and tracking pixels are dropped by size. A link that points
    straight at an image is saved as that one image.
    """
    final_url, content_type, text = await fetch_page(client, url)
    candidates = [final_url] if content_type.startswith('image/') else extract_image_urls(text, final_url)
    if not candidates:
        return []
    await asyncio.to_thread(destination_dir.mkdir, parents=True, exist_ok=True)
    saved: list[Path] = []
    for image_url in candidates:
        partial = destination_dir / f'.{len(saved) + 1:03d}.part'
        try:
            size, ext = await _download_image(client, image_url, partial, referer=final_url)
        except (httpx.HTTPError, OSError) as exc:
            log.warning('Skipped image %s from %s: %s', image_url, url, exc)
            partial.unlink(missing_ok=True)
            continue
        if size < min_bytes or ext is None:
            partial.unlink(missing_ok=True)
            continue
        target = ensure_unique_path(destination_dir / f'{len(saved) + 1:03d}.{ext}')
        partial.replace(target)
        saved.append(target)
    return saved


async def _download_image(client: httpx.AsyncClient, image_url: str, partial: Path, *, referer: str) -> tuple[int, str | None]:
    async with client.stream('GET', image_url, headers={'Referer': referer}) as response:
        if response.status_code >= httpx.codes.BAD_REQUEST:
            return 0, None
        content_type = response.headers.get('content-type', '').split(';')[0].strip().lower()
        if content_type and not content_type.startswith('image/'):
            return 0, None
        size = 0
        head = b''
        with partial.open('wb') as handle:
            async for chunk in response.aiter_bytes():
                if len(head) < _SNIFF_BYTES:
                    head += chunk[: _SNIFF_BYTES - len(head)]
                handle.write(chunk)
                size += len(chunk)
    return size, sniff_image_extension(head, fallback=_CONTENT_TYPE_EXT.get(content_type, 'jpg'))
