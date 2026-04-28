"""Hanime1 downloader.

This module focuses on the downloader workflow:
- Discover candidate IDs from watch-page series playlists using configured seed IDs.
- Resolve stream URLs from page HTML when a direct stream URL is missing.
- Download with `yt-dlp`, then rename using filename helpers.
"""

from __future__ import annotations

import asyncio
import html
import re
import shutil
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, quote_plus, urlsplit

import httpx
from opencc import OpenCC
from tenacity import RetryCallState, retry, retry_if_exception_type, stop_after_attempt, wait_exponential

from src.core import config, logger
from src.tool import database, ensure_unique_path, format_video_filename, sanitize
from src.tool.hanime1_series import ensure_hanime1_series_tables
from src.tool.notifications import enqueue_notification
from src.tool.runtime_config import RuntimeSeriesSeed, parse_runtime_series_seed, select_hanime1_canonical_id

log = logger.get('hanime1')
cfg = config.web.hanime1

USER_AGENT = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36'
_STREAM_URL_RE = re.compile(r'(?:https?:)?//[^\'"\s<>]+?\.(?:m3u8|mp4)(?:\?[^\'"\s<>]*)?', re.IGNORECASE)
_OG_TITLE_RES = (
    re.compile(r'<meta[^>]+property=["\']og:title["\'][^>]+content=["\'](?P<title>.*?)["\']', re.IGNORECASE | re.DOTALL),
    re.compile(r'<meta[^>]+content=["\'](?P<title>.*?)["\'][^>]+property=["\']og:title["\']', re.IGNORECASE | re.DOTALL),
)
_OG_IMAGE_RES = (
    re.compile(
        r'<meta[^>]+(?:property|name)=["\'](?:og:image|twitter:image)["\'][^>]+content=["\'](?P<url>.*?)["\']',
        re.IGNORECASE | re.DOTALL,
    ),
    re.compile(
        r'<meta[^>]+content=["\'](?P<url>.*?)["\'][^>]+(?:property|name)=["\'](?:og:image|twitter:image)["\']',
        re.IGNORECASE | re.DOTALL,
    ),
)
_TITLE_RE = re.compile(r'<title[^>]*>(?P<title>.*?)</title>', re.IGNORECASE | re.DOTALL)
_CF_BLOCK_MARKERS = ('Attention Required! | Cloudflare', 'cf-error-details')
_DOWNLOAD_DATA_URL_RE = re.compile(r'data-url=["\'](?P<url>https?://[^"\']+)["\']', re.IGNORECASE)
_WATCH_PLOT_RE = re.compile(r'<div[^>]+class="[^"]*video-caption-text[^"]*"[^>]*>(?P<plot>.*?)</div>', re.IGNORECASE | re.DOTALL)
_WATCH_UPLOADER_RE = re.compile(
    r'<a[^>]+id=["\']video-artist-name["\'][^>]*>(?P<uploader>.*?)</a>',
    re.IGNORECASE | re.DOTALL,
)
_DIV_TEXT_RE = re.compile(r'<div[^>]*>(?P<text>[^<]*)</div>', re.IGNORECASE | re.DOTALL)
_SEARCH_WATCH_HREF_RE = re.compile(
    r'href=["\'](?P<url>(?:https?:)?//[^"\']+/watch\?v=[^"\'>\s]+|/watch\?v=[^"\'>\s]+|watch\?v=[^"\'>\s]+)["\']',
    re.IGNORECASE,
)
_PLAYLIST_TITLE_RE = re.compile(r'<h4[^>]*>(?P<title>.*?)</h4>', re.IGNORECASE | re.DOTALL)
_SEARCH_ALLOWED_GENRES = (
    ('裏番', '里番'),
    ('泡麵番', '泡面番'),
)
_RANKING_GENRE = '裏番'
_RANKING_SORT_BY_PERIOD = {
    'weekly': '本週排行',
    'monthly': '本月排行',
}
_IGNORED_TITLE_MARKERS = (
    '[新番预告]',
    '[中字后补]',
)
_FILE_SIZE_UNIT = 1024
_K_LABEL_TO_P = {
    2: 1440,
    4: 2160,
    8: 4320,
}
_K_FALLBACK_MULTIPLIER = 540
_DOWNLOAD_MAX_ATTEMPTS = 3
_DOWNLOAD_BASE_DELAY = 5
_T2S_CONVERTER = OpenCC('t2s')


class DownloadError(RuntimeError):
    """Raised when a download fails after retries."""


class IgnoredVideoError(RuntimeError):
    """Raised when a Hanime1 item is intentionally skipped."""

    def __init__(self, item_id: str, *, marker: str, title: str, source: str) -> None:
        super().__init__(f'Skipping Hanime1 item {item_id} because {source} title contains ignored marker {marker}: {title}')
        self.item_id = item_id
        self.marker = marker
        self.title = title
        self.source = source


@dataclass(slots=True)
class HanimeRecord:
    id: str
    title: str
    page_url: str | None
    stream_url: str | None
    keyword: str | None = None
    uploader: str | None = None


@dataclass(slots=True)
class DownloadResult:
    item_id: str
    title: str
    stream_url: str
    final_path: Path
    resolution: str | None = None
    uploader: str | None = None
    release_date: str | None = None
    plot: str | None = None
    cover_url: str | None = None


@dataclass(slots=True, frozen=True)
class WatchMetadata:
    title: str | None = None
    uploader: str | None = None
    release_date: str | None = None
    plot: str | None = None
    cover_url: str | None = None


@dataclass(slots=True, frozen=True)
class WatchSeries:
    name: str | None = None
    video_ids: tuple[str, ...] = ()


@dataclass(slots=True, frozen=True)
class StreamDownloadTask:
    stream_url: str
    video_id: str
    referer: str | None
    cookie_header: str | None = None


class Hanime1:
    def __init__(self) -> None:
        self.client = httpx.AsyncClient(
            headers={
                'User-Agent': USER_AGENT,
                'Accept-Language': 'en-US,en;q=0.9',
            },
            timeout=60,
            follow_redirects=True,
            limits=httpx.Limits(max_keepalive_connections=10, max_connections=10),
            proxy=config.proxy or None,
        )
        self._tmp_dir = tempfile.TemporaryDirectory(prefix='fav-hanime1-')
        self.cache_dir = Path(self._tmp_dir.name)

    def __del__(self) -> None:
        self._tmp_dir.cleanup()

    async def aclose(self) -> None:
        await self.client.aclose()
        self._tmp_dir.cleanup()

    @staticmethod
    def _normalize_stream_url(raw: str) -> str:
        url = raw.strip().strip('\'"')
        if url.startswith('//'):
            return f'https:{url}'
        return url

    @staticmethod
    def extract_stream_urls(page_html: str) -> list[str]:
        """Extract media stream URLs from page HTML.

        Supports both escaped and plain URLs and prefers m3u8 when available.
        """
        normalized_html = html.unescape(page_html).replace('\\/', '/').replace('\\u0026', '&')
        seen: set[str] = set()
        candidates: list[str] = []
        for raw in _STREAM_URL_RE.findall(normalized_html):
            url = Hanime1._normalize_stream_url(raw)
            if not url:
                continue
            key = url.casefold()
            if key in seen:
                continue
            seen.add(key)
            candidates.append(url)

        m3u8_urls = [url for url in candidates if urlsplit(url).path.lower().endswith('.m3u8')]
        other_urls = [url for url in candidates if url not in m3u8_urls]
        return [*m3u8_urls, *other_urls]

    @staticmethod
    def parse_title(page_html: str) -> str | None:
        """Parse page title from HTML metadata."""
        for pattern in _OG_TITLE_RES:
            match = pattern.search(page_html)
            if not match:
                continue
            title = html.unescape(match.group('title')).strip()
            if title:
                return title

        match = _TITLE_RE.search(page_html)
        if not match:
            return None
        title = html.unescape(match.group('title')).strip()
        return title or None

    @staticmethod
    def _normalize_download_title(title: str | None) -> str | None:
        if not title:
            return None
        normalized = title.strip()
        if normalized.endswith(' - Hanime1.me'):
            normalized = normalized.removesuffix(' - Hanime1.me').strip()
        for prefix in ('下載 ', '下载 '):
            if normalized.startswith(prefix):
                normalized = normalized.removeprefix(prefix).strip()
        return normalized or None

    @staticmethod
    def _normalize_watch_title(title: str | None) -> str | None:
        if not title:
            return None
        normalized = title.strip()
        normalized = re.sub(r'\s*-\s*Hanime1\.me\s*$', '', normalized, flags=re.IGNORECASE).strip()
        normalized = re.sub(r'\s*-\s*H動漫/裏番/線上看\s*$', '', normalized).strip()
        return normalized or None

    @staticmethod
    def _to_simplified_chinese(text: str | None) -> str | None:
        if not text:
            return None
        converted = _T2S_CONVERTER.convert(text).strip()
        return converted or None

    @staticmethod
    def _stream_quality(url: str) -> int:
        lowered = url.lower()
        match = re.search(r'(\d{3,4})p(?=\.mp4|[?&])', lowered)
        if match:
            return int(match.group(1))

        k_match = re.search(r'(\d{1,2})k(?=\.mp4|[?&])', lowered)
        if not k_match:
            return 0

        k_value = int(k_match.group(1))
        # Map common "k" labels to their practical vertical resolution.
        return _K_LABEL_TO_P.get(k_value, k_value * _K_FALLBACK_MULTIPLIER)

    @staticmethod
    def _stream_resolution_label(stream_url: str) -> str | None:
        quality = Hanime1._stream_quality(stream_url)
        if quality <= 0:
            return None
        return f'{quality}p'

    @staticmethod
    def _format_file_size(size_bytes: int | None) -> str | None:
        if size_bytes is None or size_bytes < 0:
            return None
        if size_bytes < _FILE_SIZE_UNIT:
            return f'{size_bytes} B'

        size = float(size_bytes)
        units = ('KB', 'MB', 'GB', 'TB')
        for unit in units:
            size /= _FILE_SIZE_UNIT
            if size < _FILE_SIZE_UNIT or unit == units[-1]:
                return f'{size:.1f} {unit}'
        return None

    @staticmethod
    def extract_download_urls(page_html: str) -> list[str]:
        """Extract direct download URLs from Hanime1 download page HTML."""
        normalized_html = html.unescape(page_html).replace('\\/', '/').replace('\\u0026', '&')
        seen: set[str] = set()
        urls: list[str] = []
        for match in _DOWNLOAD_DATA_URL_RE.finditer(normalized_html):
            url = Hanime1._normalize_stream_url(match.group('url'))
            if not url:
                continue
            key = url.casefold()
            if key in seen:
                continue
            seen.add(key)
            urls.append(url)

        if not urls:
            return []

        urls.sort(key=Hanime1._stream_quality, reverse=True)
        return urls

    @staticmethod
    def _extract_video_code(candidate_url: str | None) -> str | None:
        if not candidate_url:
            return None
        query = parse_qs(urlsplit(candidate_url).query)
        video_ids = query.get('v')
        if not video_ids:
            return None
        video_id = video_ids[0].strip()
        if not video_id:
            return None
        return sanitize(video_id, max_bytes=80)

    @staticmethod
    def _build_download_page_url(item: HanimeRecord) -> str:
        if item.page_url and '/download' in urlsplit(item.page_url).path:
            return item.page_url

        video_code = Hanime1._extract_video_code(item.page_url) or sanitize(item.id, max_bytes=80)
        return f'{cfg.host.rstrip("/")}/download?v={video_code}'

    @staticmethod
    def _build_watch_page_url(item: HanimeRecord) -> str:
        if item.page_url and '/watch' in urlsplit(item.page_url).path:
            return item.page_url

        video_code = Hanime1._extract_video_code(item.page_url) or sanitize(item.id, max_bytes=80)
        return f'{cfg.host.rstrip("/")}/watch?v={video_code}'

    @staticmethod
    def _normalize_page_url(candidate_url: str) -> str:
        normalized = html.unescape(candidate_url).strip()
        if normalized.startswith('//'):
            return f'https:{normalized}'
        if normalized.startswith('/'):
            return f'{cfg.host.rstrip("/")}{normalized}'
        if normalized.startswith(('http://', 'https://')):
            return normalized
        return f'{cfg.host.rstrip("/")}/{normalized.lstrip("/")}'

    @staticmethod
    def extract_search_video_ids(page_html: str) -> list[str]:
        normalized_html = html.unescape(page_html).replace('\\/', '/').replace('\\u0026', '&')
        ids: list[str] = []
        seen: set[str] = set()
        for match in _SEARCH_WATCH_HREF_RE.finditer(normalized_html):
            watch_url = Hanime1._normalize_page_url(match.group('url'))
            if '/watch' not in urlsplit(watch_url).path:
                continue
            video_id = Hanime1._extract_video_code(watch_url)
            if not video_id:
                continue
            key = video_id.casefold()
            if key in seen:
                continue
            seen.add(key)
            ids.append(video_id)
        return ids

    @staticmethod
    def _extract_div_blocks_by_id(page_html: str, *, block_id: str) -> list[str]:
        pattern = re.compile(rf'<div[^>]+id=["\']{re.escape(block_id)}["\'][^>]*>', re.IGNORECASE)
        tag_pattern = re.compile(r'</?div\b[^>]*>', re.IGNORECASE)
        blocks: list[str] = []
        search_pos = 0
        while True:
            start_match = pattern.search(page_html, search_pos)
            if not start_match:
                break

            depth = 1
            end_pos: int | None = None
            for tag_match in tag_pattern.finditer(page_html, start_match.end()):
                tag = tag_match.group(0).lstrip().lower()
                if tag.startswith('</div'):
                    depth -= 1
                else:
                    depth += 1
                if depth == 0:
                    end_pos = tag_match.end()
                    break

            if end_pos is None:
                break

            blocks.append(page_html[start_match.start() : end_pos])
            search_pos = end_pos
        return blocks

    @staticmethod
    def _extract_playlist_title(playlist_html: str) -> str | None:
        match = _PLAYLIST_TITLE_RE.search(playlist_html)
        if not match:
            return None
        text = html.unescape(match.group('title'))
        text = re.sub(r'<[^>]+>', '', text)
        text = re.sub(r'\s+', ' ', text).strip()
        if not text:
            return None
        return Hanime1._to_simplified_chinese(text)

    @staticmethod
    def extract_watch_series(page_html: str) -> WatchSeries | None:
        playlist_blocks = Hanime1._extract_div_blocks_by_id(page_html, block_id='video-playlist-wrapper')
        if not playlist_blocks:
            return None

        series_name: str | None = None
        ids: list[str] = []
        seen_ids: set[str] = set()

        for playlist_html in playlist_blocks:
            if not series_name:
                series_name = Hanime1._extract_playlist_title(playlist_html)
            for video_id in Hanime1.extract_search_video_ids(playlist_html):
                key = video_id.casefold()
                if key in seen_ids:
                    continue
                seen_ids.add(key)
                ids.append(video_id)

        if not ids:
            return None
        return WatchSeries(name=series_name, video_ids=tuple(ids))

    @staticmethod
    def _ignored_title_marker(title: str | None) -> str | None:
        normalized = Hanime1._to_simplified_chinese(title)
        if not normalized:
            return None
        for marker in _IGNORED_TITLE_MARKERS:
            if marker in normalized:
                return marker
        return None

    def _raise_if_ignored_title(self, *, item_id: str, title: str | None, source: str) -> None:
        marker = self._ignored_title_marker(title)
        if marker is None:
            return
        normalized = self._to_simplified_chinese(title) or title or item_id
        raise IgnoredVideoError(item_id=item_id, marker=marker, title=normalized, source=source)

    @staticmethod
    def _extract_release_date_from_watch_html(page_html: str) -> str | None:
        normalized = html.unescape(page_html).replace('&nbsp;', ' ')
        for marker in ('觀看次數', '观看次数'):
            idx = normalized.find(marker)
            if idx == -1:
                continue
            chunk = normalized[idx : idx + 300]
            match = re.search(r'\b\d{4}-\d{2}-\d{2}\b', chunk)
            if match:
                return match.group(0)
        return None

    @staticmethod
    def _extract_title_from_plot_context(page_html: str) -> str | None:
        plot_match = _WATCH_PLOT_RE.search(page_html)
        if not plot_match:
            return None
        context_start = max(0, plot_match.start() - 1200)
        context_html = page_html[context_start : plot_match.start()]
        context_html = html.unescape(context_html).replace('&nbsp;', ' ')
        candidates: list[str] = []
        for div_match in _DIV_TEXT_RE.finditer(context_html):
            text = re.sub(r'\s+', ' ', div_match.group('text')).strip()
            if not text:
                continue
            candidates.append(text)

        for candidate in reversed(candidates):
            if '觀看次數' in candidate or '观看次数' in candidate:
                continue
            if re.fullmatch(r'\d{4}-\d{2}-\d{2}', candidate):
                continue
            return Hanime1._normalize_watch_title(candidate)
        return None

    @staticmethod
    def _extract_release_date_from_plot_context(page_html: str) -> str | None:
        plot_match = _WATCH_PLOT_RE.search(page_html)
        if not plot_match:
            return None
        context_start = max(0, plot_match.start() - 1200)
        context_html = page_html[context_start : plot_match.start()]
        context_text = html.unescape(context_html).replace('&nbsp;', ' ')
        match = re.search(r'\b\d{4}-\d{2}-\d{2}\b', context_text)
        if not match:
            return None
        return match.group(0)

    @staticmethod
    def _extract_plot_from_watch_html(page_html: str) -> str | None:
        match = _WATCH_PLOT_RE.search(page_html)
        if not match:
            return None
        content = html.unescape(match.group('plot'))
        content = re.sub(r'<[^>]+>', '', content)
        content = re.sub(r'\s+', ' ', content).strip()
        return content or None

    @staticmethod
    def _extract_uploader_from_watch_html(page_html: str) -> str | None:
        match = _WATCH_UPLOADER_RE.search(page_html)
        if not match:
            return None
        content = html.unescape(match.group('uploader'))
        content = re.sub(r'<[^>]+>', '', content)
        content = re.sub(r'\s+', ' ', content).strip()
        return content or None

    @staticmethod
    def _extract_cover_url_from_watch_html(page_html: str) -> str | None:
        for pattern in _OG_IMAGE_RES:
            match = pattern.search(page_html)
            if not match:
                continue
            raw = html.unescape(match.group('url')).strip()
            if not raw or raw.startswith('data:'):
                continue
            return Hanime1._normalize_page_url(raw)
        return None

    @staticmethod
    def extract_watch_metadata(page_html: str) -> WatchMetadata:
        title = Hanime1._extract_title_from_plot_context(page_html)
        if not title:
            title = Hanime1._normalize_watch_title(Hanime1.parse_title(page_html))

        uploader = Hanime1._extract_uploader_from_watch_html(page_html)
        release_date = Hanime1._extract_release_date_from_plot_context(page_html)
        if not release_date:
            release_date = Hanime1._extract_release_date_from_watch_html(page_html)

        plot = Hanime1._extract_plot_from_watch_html(page_html)
        return WatchMetadata(
            title=Hanime1._to_simplified_chinese(title),
            uploader=Hanime1._to_simplified_chinese(uploader),
            release_date=release_date,
            plot=Hanime1._to_simplified_chinese(plot),
            cover_url=Hanime1._extract_cover_url_from_watch_html(page_html),
        )

    @staticmethod
    def _cleanup_dir(dirpath: Path) -> None:
        if not dirpath.exists():
            return
        for entry in dirpath.iterdir():
            if entry.is_dir():
                shutil.rmtree(entry)
            else:
                entry.unlink()

    @staticmethod
    def _build_download_command(
        *,
        stream_url: str,
        output_template: Path,
        referer: str | None,
        cookie_header: str | None,
    ) -> list[str]:
        command = [
            'yt-dlp',
            '-o',
            str(output_template),
            '--no-mtime',
            '--no-part',
            '-N',
            '8',
            '--retries',
            '15',
            '--fragment-retries',
            '15',
            '--socket-timeout',
            '30',
        ]
        if referer:
            command.extend(['--referer', referer])
        command.extend(['--add-header', f'User-Agent:{USER_AGENT}'])
        if cookie_header:
            command.extend(['--add-header', f'Cookie:{cookie_header}'])
        if config.proxy:
            command.extend(['--proxy', config.proxy])
        command.append(stream_url)
        return command

    @staticmethod
    def _derive_item_id(item: HanimeRecord) -> str:
        base = sanitize(item.id.strip(), max_bytes=80)
        if base:
            return base

        for candidate in (item.page_url, item.stream_url):
            if not candidate:
                continue
            path = urlsplit(candidate).path.strip('/')
            if not path:
                continue
            slug = sanitize(path.split('/')[-1], max_bytes=80)
            if slug:
                return slug
        return 'unknown'

    @staticmethod
    def _resolve_output_dir(keyword: str | None) -> Path:
        if not keyword:
            return cfg.path
        normalized = sanitize(keyword.strip(), max_bytes=120)
        if not normalized:
            return cfg.path
        return cfg.path / normalized

    @staticmethod
    def _get_cookie_header() -> str:
        # Keep subtitle language stable in download page resolution.
        return f'user_lang={cfg.user_lang}'

    async def resolve_stream_from_download_page(self, item: HanimeRecord) -> tuple[str, str | None]:
        download_page_url = self._build_download_page_url(item)
        referer = item.page_url or cfg.host.rstrip('/') + '/'
        headers = {
            'Referer': referer,
            'User-Agent': USER_AGENT,
            'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8',
            'Accept-Language': 'zh-CN,zh;q=0.9,en;q=0.8',
        }
        cookie_header = await asyncio.to_thread(self._get_cookie_header)
        if cookie_header:
            headers['Cookie'] = cookie_header

        res = await self.client.get(download_page_url, headers=headers)
        res.raise_for_status()
        page_html = res.text
        if all(marker in page_html for marker in _CF_BLOCK_MARKERS):
            msg = 'Blocked by Cloudflare while resolving Hanime1 download page.'
            raise ValueError(msg)

        stream_urls = self.extract_download_urls(page_html)
        if not stream_urls:
            msg = f'Could not find download URL from page: {download_page_url}'
            raise ValueError(msg)
        title = self._normalize_download_title(self.parse_title(page_html))
        return stream_urls[0], self._to_simplified_chinese(title)

    async def _build_page_headers(self, *, referer: str) -> dict[str, str]:
        headers = {
            'Referer': referer,
            'User-Agent': USER_AGENT,
            'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8',
            'Accept-Language': 'zh-CN,zh;q=0.9,en;q=0.8',
        }
        cookie_header = await asyncio.to_thread(self._get_cookie_header)
        if cookie_header:
            headers['Cookie'] = cookie_header
        return headers

    async def _request_watch_page_html(self, item: HanimeRecord) -> str:
        watch_page_url = self._build_watch_page_url(item)
        headers = await self._build_page_headers(referer=cfg.host.rstrip('/') + '/')
        res = await self.client.get(watch_page_url, headers=headers)
        res.raise_for_status()
        page_html = res.text
        if all(marker in page_html for marker in _CF_BLOCK_MARKERS):
            msg = 'Blocked by Cloudflare while resolving Hanime1 watch page.'
            raise ValueError(msg)
        return page_html

    async def search_video_ids(self, keyword: str) -> list[str]:
        query = keyword.strip()
        if not query:
            return []

        headers = await self._build_page_headers(referer=cfg.host.rstrip('/') + '/')

        collected_ids: list[str] = []
        seen_ids: set[str] = set()
        successful_genres = 0
        failures: list[str] = []

        for genre_value, genre_name in _SEARCH_ALLOWED_GENRES:
            search_url = f'{cfg.host.rstrip("/")}/search?query={quote_plus(query)}&genre={quote_plus(genre_value)}'
            try:
                res = await self.client.get(search_url, headers=headers)
                res.raise_for_status()
            except Exception as exc:  # noqa: BLE001
                failures.append(f'{genre_name}: {exc}')
                log.warning('Failed to search Hanime1 keyword %r in genre %s: %s', query, genre_name, exc)
                continue

            page_html = res.text
            if all(marker in page_html for marker in _CF_BLOCK_MARKERS):
                failures.append(f'{genre_name}: blocked by Cloudflare')
                log.warning('Blocked by Cloudflare while searching Hanime1 keyword %r in genre %s', query, genre_name)
                continue

            successful_genres += 1
            genre_ids = self.extract_search_video_ids(page_html)
            log.debug('Keyword %r genre %s matched %d videos', query, genre_name, len(genre_ids))
            for video_id in genre_ids:
                key = video_id.casefold()
                if key in seen_ids:
                    continue
                seen_ids.add(key)
                collected_ids.append(video_id)

        if successful_genres == 0 and failures:
            msg = f'Failed to search Hanime1 keyword: {query}. Details: {"; ".join(failures)}'
            raise ValueError(msg)

        return collected_ids

    @staticmethod
    def _build_ranking_page_url(*, period: str, page: int) -> str:
        sort_value = _RANKING_SORT_BY_PERIOD.get(period)
        if sort_value is None:
            msg = f'Unsupported Hanime1 ranking period: {period}'
            raise ValueError(msg)

        params = f'genre={quote_plus(_RANKING_GENRE)}&sort={quote_plus(sort_value)}'
        if page > 1:
            params = f'{params}&page={page}'
        return f'{cfg.host.rstrip("/")}/search?{params}'

    async def fetch_ranking_video_ids(self, *, period: str, page: int = 1) -> list[str]:
        ranking_url = self._build_ranking_page_url(period=period, page=page)
        headers = await self._build_page_headers(referer=cfg.host.rstrip('/') + '/')
        res = await self.client.get(ranking_url, headers=headers)
        res.raise_for_status()
        page_html = res.text
        if all(marker in page_html for marker in _CF_BLOCK_MARKERS):
            msg = f'Blocked by Cloudflare while fetching Hanime1 ranking page: {ranking_url}'
            raise ValueError(msg)
        return self.extract_search_video_ids(page_html)

    async def resolve_metadata_from_watch_page(self, item: HanimeRecord) -> WatchMetadata:
        page_html = await self._request_watch_page_html(item)
        return self.extract_watch_metadata(page_html)

    async def resolve_series_from_watch_page(self, item: HanimeRecord) -> WatchSeries | None:
        page_html = await self._request_watch_page_html(item)
        return self.extract_watch_series(page_html)

    async def resolve_stream_from_page(self, page_url: str) -> tuple[str, str | None]:
        headers = {
            'Referer': cfg.host.rstrip('/') + '/',
            'User-Agent': USER_AGENT,
        }
        cookie_header = await asyncio.to_thread(self._get_cookie_header)
        if cookie_header:
            headers['Cookie'] = cookie_header

        res = await self.client.get(page_url, headers=headers)
        res.raise_for_status()
        page_html = res.text
        if all(marker in page_html for marker in _CF_BLOCK_MARKERS):
            msg = 'Blocked by Cloudflare while resolving Hanime1 page. Provide direct stream_url or try again later.'
            raise ValueError(msg)

        stream_urls = self.extract_stream_urls(page_html)
        if not stream_urls:
            msg = f'Could not find stream URL from page: {page_url}'
            raise ValueError(msg)

        return stream_urls[0], self._to_simplified_chinese(self.parse_title(page_html))

    def download_stream(
        self,
        *,
        task: StreamDownloadTask,
        dirpath: Path,
    ) -> Path:
        """Download a Hanime1 stream URL with retries."""
        output_template = dirpath / f'{task.video_id}.%(ext)s'
        command = self._build_download_command(
            stream_url=task.stream_url,
            output_template=output_template,
            referer=task.referer,
            cookie_header=task.cookie_header,
        )

        def _on_retry_before_sleep(retry_state: RetryCallState) -> None:
            exc = retry_state.outcome.exception() if retry_state.outcome else None
            if exc is None:
                return
            sleep = retry_state.next_action.sleep if retry_state.next_action else 0.0
            log.debug(
                'Retrying Hanime1 download for %s (%d/%d), next wait %.1fs: %s',
                task.video_id,
                retry_state.attempt_number,
                _DOWNLOAD_MAX_ATTEMPTS,
                sleep,
                exc,
            )

        @retry(
            reraise=True,
            stop=stop_after_attempt(_DOWNLOAD_MAX_ATTEMPTS),
            wait=wait_exponential(
                multiplier=_DOWNLOAD_BASE_DELAY,
                min=_DOWNLOAD_BASE_DELAY,
                max=_DOWNLOAD_BASE_DELAY * 6,
            ),
            retry=retry_if_exception_type(DownloadError),
            before_sleep=_on_retry_before_sleep,
        )
        def _run_once() -> None:
            self._cleanup_dir(dirpath)
            result = subprocess.run(command, text=True, capture_output=True, check=False)  # noqa: S603
            if result.returncode == 0:
                if result.stderr:
                    log.debug('yt-dlp stderr: %s', result.stderr.strip())
                return
            message = result.stderr.strip() or result.stdout.strip() or f'yt-dlp exited with code {result.returncode}'
            raise DownloadError(message)

        _run_once()

        candidates = [entry for entry in dirpath.iterdir() if entry.is_file()]
        if not candidates:
            msg = f'No output file generated for {task.video_id}'
            raise DownloadError(msg)
        candidates.sort(key=lambda p: p.stat().st_size, reverse=True)
        return candidates[0]

    async def _notify_download(
        self,
        *,
        item: HanimeRecord,
        resolution: str | None = None,
        file_size_bytes: int | None = None,
        release_date: str | None = None,
        cover_url: str | None = None,
    ) -> None:
        title = item.title.strip() or item.id
        watch_url = self._build_watch_page_url(item)
        resolution_label = resolution or 'unknown'
        file_size_label = self._format_file_size(file_size_bytes) or 'unknown'
        release_date_label = (release_date or 'unknown').strip() or 'unknown'
        body = f'{resolution_label} | {file_size_label} | {release_date_label}'

        try:
            await enqueue_notification(
                kind='download_completed',
                source='hanime1',
                title=f'Hanime1: {title}',
                body=body,
                link_url=watch_url,
                image_url=cover_url or '',
                payload={
                    'video_id': item.id,
                    'resolution': resolution,
                    'file_size_bytes': file_size_bytes,
                    'release_date': release_date,
                },
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue hanime1 download notification for %s: %s', item.id, exc)

    @staticmethod
    def _normalize_runtime_video_id(raw: Any) -> str | None:
        text: str | None = None
        if isinstance(raw, bool):
            return None
        if isinstance(raw, int):
            if raw <= 0:
                return None
            text = str(raw).strip()
        elif isinstance(raw, str):
            text = raw.strip()
            if text:
                text = Hanime1._extract_video_code(text) or text

        if not text:
            return None
        normalized = sanitize(text, max_bytes=80)
        if not normalized or not normalized.isdecimal():
            return None
        if int(normalized) <= 0:
            return None
        return normalized

    @staticmethod
    def _parse_runtime_seed(raw: Any) -> RuntimeSeriesSeed | None:
        return parse_runtime_series_seed(raw)

    async def _load_runtime_series_seeds(self) -> list[RuntimeSeriesSeed]:
        rows = await database.query_db('SELECT canonical_video_id, title FROM hanime1_series ORDER BY created_at, canonical_video_id;')
        seeds: list[RuntimeSeriesSeed] = []
        for row in rows:
            video_id = str(row.get('canonical_video_id') or '').strip()
            title = str(row.get('title') or '').strip()
            if not video_id or not title:
                continue
            seeds.append(RuntimeSeriesSeed(video_id=video_id, title=title))
        return seeds

    async def _collect_ranking_video_ids(self) -> list[str]:
        ranking_cfg = cfg.ranking
        collected_ids: list[str] = []
        seen_ids: set[str] = set()

        for period in ranking_cfg.periods:
            for page in range(1, ranking_cfg.pages + 1):
                try:
                    ranking_ids = await self.fetch_ranking_video_ids(period=period, page=page)
                except Exception as exc:  # noqa: BLE001
                    log.warning('Failed to fetch Hanime1 %s ranking page %d: %s', period, page, exc)
                    continue

                log.info('Hanime1 %s ranking page %d returned %d videos', period, page, len(ranking_ids))
                for video_id in ranking_ids:
                    key = video_id.casefold()
                    if key in seen_ids:
                        continue
                    seen_ids.add(key)
                    collected_ids.append(video_id)

        return collected_ids

    async def _get_known_series_video_ids(self, video_ids: list[str]) -> set[str]:
        if not video_ids:
            return set()

        rows = await database.query_db(
            """
            SELECT video_id
            FROM hanime1_series_video
            WHERE video_id = ANY(?);
            """,
            (video_ids,),
        )
        known_ids: set[str] = set()
        for row in rows:
            video_id = str(row.get('video_id') or '').strip()
            if not video_id:
                continue
            known_ids.add(video_id.casefold())
        return known_ids

    @staticmethod
    def _series_member_ids_from_watch_series(*, seed_video_id: str, series: WatchSeries) -> list[str]:
        member_ids: list[str] = []
        seen_ids: set[str] = set()
        for raw_id in (seed_video_id, *series.video_ids):
            video_id = Hanime1._normalize_runtime_video_id(raw_id)
            if video_id is None:
                continue
            key = video_id.casefold()
            if key in seen_ids:
                continue
            seen_ids.add(key)
            member_ids.append(video_id)
        return member_ids

    async def _insert_ranking_series_seed(
        self,
        *,
        added_from_video_id: str,
        series: WatchSeries,
    ) -> tuple[RuntimeSeriesSeed | None, list[str]]:
        member_ids = self._series_member_ids_from_watch_series(seed_video_id=added_from_video_id, series=series)
        canonical_id = select_hanime1_canonical_id(member_ids)
        title = self._to_simplified_chinese(series.name) or series.name
        if canonical_id is None or not title:
            return None, member_ids

        known_member_ids = await self._get_known_series_video_ids(member_ids)
        if known_member_ids:
            return None, member_ids

        seed = RuntimeSeriesSeed(video_id=canonical_id, title=title)
        await database.query_db(
            """
            INSERT INTO hanime1_series (canonical_video_id, title, added_from_video_id)
            VALUES (?, ?, ?)
            ON CONFLICT (canonical_video_id) DO NOTHING;
            """,
            (seed.video_id, seed.title, added_from_video_id),
        )
        await self._upsert_series_members(canonical_video_id=seed.video_id, member_ids=member_ids)
        return seed, member_ids

    async def discover_ranking_series(self) -> list[RuntimeSeriesSeed]:
        if not cfg.ranking.enabled:
            return []

        ranking_video_ids = await self._collect_ranking_video_ids()
        if not ranking_video_ids:
            log.info('Hanime1 ranking discovery found no ranking videos')
            return []

        known_video_ids = await self._get_known_series_video_ids(ranking_video_ids)
        added_seeds: list[RuntimeSeriesSeed] = []
        skipped_known_count = 0

        for video_id in ranking_video_ids:
            if video_id.casefold() in known_video_ids:
                skipped_known_count += 1
                continue

            item = HanimeRecord(
                id=video_id,
                title='',
                page_url=f'{cfg.host.rstrip("/")}/watch?v={video_id}',
                stream_url=None,
            )
            try:
                series = await self.resolve_series_from_watch_page(item)
                if series is None or not series.name:
                    log.warning('Hanime1 ranking video %s did not expose a resolvable watch series', video_id)
                    continue

                seed, member_ids = await self._insert_ranking_series_seed(added_from_video_id=video_id, series=series)
            except Exception:
                log.exception('Failed to add Hanime1 ranking video %s as a series target', video_id)
                continue

            known_video_ids.update(member_id.casefold() for member_id in member_ids)
            if seed is None:
                skipped_known_count += 1
                continue

            added_seeds.append(seed)
            log.info(
                'Added Hanime1 ranking series target %s from ranking video %s: %s',
                seed.video_id,
                video_id,
                seed.title,
            )

        log.info(
            'Hanime1 ranking discovery added %d new series from %d ranking videos (%d already known)',
            len(added_seeds),
            len(ranking_video_ids),
            skipped_known_count,
        )
        return added_seeds

    @staticmethod
    def _format_scan_error(exc: Exception) -> str:
        return f'{exc.__class__.__name__}: {exc}'[:500]

    async def _upsert_series_members(self, *, canonical_video_id: str, member_ids: list[str]) -> None:
        seen_ids: set[str] = set()
        for video_id in member_ids:
            normalized_id = str(video_id).strip()
            if not normalized_id or normalized_id in seen_ids:
                continue
            seen_ids.add(normalized_id)
            await database.query_db(
                """
                INSERT INTO hanime1_series_video (video_id, canonical_video_id)
                VALUES (?, ?)
                ON CONFLICT (video_id) DO NOTHING;
                """,
                (normalized_id, canonical_video_id),
            )

    async def _mark_series_scan_success(self, canonical_video_id: str) -> None:
        await database.query_db(
            """
            UPDATE hanime1_series
            SET last_scanned_at = CURRENT_TIMESTAMP,
                last_scan_error = '',
                updated_at = CURRENT_TIMESTAMP
            WHERE canonical_video_id = ?;
            """,
            (canonical_video_id,),
        )

    async def _mark_series_scan_failure(self, canonical_video_id: str, exc: Exception) -> None:
        await database.query_db(
            """
            UPDATE hanime1_series
            SET last_scan_error = ?,
                updated_at = CURRENT_TIMESTAMP
            WHERE canonical_video_id = ?;
            """,
            (self._format_scan_error(exc), canonical_video_id),
        )

    async def _collect_ids_from_watch_series(
        self,
        seeds: list[RuntimeSeriesSeed],
    ) -> dict[str, tuple[str, str]]:
        collected: dict[str, tuple[str, str]] = {}

        for seed in seeds:
            seed_item = HanimeRecord(
                id=seed.video_id,
                title='',
                page_url=f'{cfg.host.rstrip("/")}/watch?v={seed.video_id}',
                stream_url=None,
            )

            try:
                series = await self.resolve_series_from_watch_page(seed_item)
            except Exception as exc:
                log.exception('Failed to resolve Hanime1 watch series from seed id %s', seed.video_id)
                await self._mark_series_scan_failure(seed.video_id, exc)
                continue

            series_name = seed.title
            candidate_ids: list[str] = [seed.video_id]
            if series is not None:
                if series.name:
                    series_name = series.name
                if series.video_ids:
                    candidate_ids = list(series.video_ids)

            source_name = self._to_simplified_chinese(series_name) or f'id-{seed.video_id}'
            await self._upsert_series_members(canonical_video_id=seed.video_id, member_ids=[seed.video_id, *candidate_ids])
            await self._mark_series_scan_success(seed.video_id)
            new_count = 0
            for video_id in candidate_ids:
                key = video_id.casefold()
                if key in collected:
                    continue
                collected[key] = (video_id, source_name)
                new_count += 1
            log.info(
                'Seed %s resolved %d series videos (%d unique) as %r',
                seed.video_id,
                len(candidate_ids),
                new_count,
                source_name,
            )

        return collected

    async def _get_downloaded_ids(self) -> set[str]:
        rows = await database.query_db('SELECT id FROM hanime1;')
        downloaded_ids: set[str] = set()
        for row in rows:
            item_id = str(row.get('id') or '').strip()
            if not item_id:
                continue
            downloaded_ids.add(item_id.casefold())
        return downloaded_ids

    async def get_items(self) -> list[HanimeRecord]:
        seeds = await self._load_runtime_series_seeds()
        if not seeds:
            return []

        candidate_map = await self._collect_ids_from_watch_series(seeds)
        if not candidate_map:
            return []

        downloaded_ids = await self._get_downloaded_ids()
        records: list[HanimeRecord] = []
        seen_ids = set(downloaded_ids)
        for key, (video_id, source_name) in candidate_map.items():
            if key in seen_ids:
                continue
            item = HanimeRecord(
                id=video_id,
                title='',
                keyword=source_name,
                uploader=None,
                page_url=f'{cfg.host.rstrip("/")}/watch?v={video_id}',
                stream_url=None,
            )
            try:
                watch_metadata = await self.resolve_metadata_from_watch_page(item)
            except Exception as exc:  # noqa: BLE001
                log.debug('Failed to resolve Hanime1 watch metadata for discovery item %s: %s', video_id, exc)
                watch_metadata = WatchMetadata()

            marker = self._ignored_title_marker(watch_metadata.title)
            if marker is not None:
                normalized_title = self._to_simplified_chinese(watch_metadata.title) or watch_metadata.title or video_id
                log.info(
                    'Skipping Hanime1 discovery item %s because watch title contains ignored marker %s: %s',
                    video_id,
                    marker,
                    normalized_title,
                )
                continue

            seen_ids.add(key)
            if watch_metadata.title:
                item.title = watch_metadata.title
            records.append(item)
        log.info('Hanime1 discovery path watch-series(ids) produced %d new videos', len(records))
        return records

    async def download_item(self, item: HanimeRecord) -> DownloadResult:
        item_id = self._derive_item_id(item)
        title = self._to_simplified_chinese(item.title) or item_id
        uploader = self._to_simplified_chinese(item.uploader)
        watch_metadata = WatchMetadata()
        stream_url = item.stream_url
        self._raise_if_ignored_title(item_id=item_id, title=item.title, source='item')
        if not stream_url:
            try:
                stream_url, parsed_title = await self.resolve_stream_from_download_page(item)
            except Exception as exc:
                log.debug('Failed to resolve download page for %s: %s', item_id, exc)
                if not item.page_url:
                    msg = f'Item {item_id} has neither valid download page nor page_url'
                    raise ValueError(msg) from exc
                stream_url, parsed_title = await self.resolve_stream_from_page(item.page_url)
            self._raise_if_ignored_title(item_id=item_id, title=parsed_title, source='page')
            if parsed_title:
                title = parsed_title
        try:
            watch_metadata = await self.resolve_metadata_from_watch_page(item)
        except Exception as exc:  # noqa: BLE001
            log.debug('Failed to resolve watch metadata for %s: %s', item_id, exc)
        self._raise_if_ignored_title(item_id=item_id, title=watch_metadata.title, source='watch metadata')
        if watch_metadata.title:
            title = watch_metadata.title
        if not uploader and watch_metadata.uploader:
            uploader = watch_metadata.uploader

        referer = item.page_url or self._build_download_page_url(item)
        cookie_header = await asyncio.to_thread(self._get_cookie_header)

        tmp_video_dir = self.cache_dir / 'videos'
        await asyncio.to_thread(tmp_video_dir.mkdir, parents=True, exist_ok=True)
        downloaded_path = self.download_stream(
            task=StreamDownloadTask(
                stream_url=stream_url,
                video_id=item_id,
                referer=referer,
                cookie_header=cookie_header,
            ),
            dirpath=tmp_video_dir,
        )

        filename = format_video_filename(
            title=title,
            video_id=item_id,
            ext=downloaded_path.suffix,
        )
        output_dir = self._resolve_output_dir(item.keyword)
        await asyncio.to_thread(output_dir.mkdir, parents=True, exist_ok=True)
        final_path = ensure_unique_path(output_dir / filename)
        shutil.move(downloaded_path, final_path)
        resolution = self._stream_resolution_label(stream_url)

        return DownloadResult(
            item_id=item.id,
            title=title,
            stream_url=stream_url,
            final_path=final_path,
            resolution=resolution,
            uploader=uploader,
            release_date=watch_metadata.release_date,
            plot=watch_metadata.plot,
            cover_url=watch_metadata.cover_url,
        )

    async def _ensure_table(self) -> None:
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS hanime1 (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '',
                uploader TEXT,
                release_date TEXT,
                plot TEXT,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
        """)
        columns = await database.query_db('PRAGMA table_info(hanime1);')
        column_names = {str(col.get('name') or '') for col in columns}
        if 'release_date' not in column_names:
            await database.query_db('ALTER TABLE hanime1 ADD COLUMN release_date TEXT;')
        if 'plot' not in column_names:
            await database.query_db('ALTER TABLE hanime1 ADD COLUMN plot TEXT;')
        if 'title' not in column_names:
            await database.query_db("ALTER TABLE hanime1 ADD COLUMN title TEXT NOT NULL DEFAULT '';")
        if 'uploader' not in column_names:
            await database.query_db('ALTER TABLE hanime1 ADD COLUMN uploader TEXT;')
        await ensure_hanime1_series_tables()

    async def update(self) -> None:
        await self._ensure_table()
        log.debug('hanime1 table initialized')
        await self.discover_ranking_series()

        items = await self.get_items()
        if not items:
            if not await self._load_runtime_series_seeds():
                log.info('No Hanime1 series seeds configured in database')
            else:
                log.info('No new videos')
            return

        await asyncio.to_thread(cfg.path.mkdir, parents=True, exist_ok=True)
        total = len(items)
        log.info('Found %d new videos', total)
        for idx, item in enumerate(items, start=1):
            log.info('Downloading Hanime1 %s (%d/%d)', item.id, idx, total)
            try:
                result = await self.download_item(item)
            except IgnoredVideoError as exc:
                log.info(str(exc))
                continue
            except Exception:
                log.exception('Failed to download Hanime1 item %s', item.id)
                continue

            await database.query_db(
                'INSERT OR IGNORE INTO hanime1 (id, title, uploader, release_date, plot) VALUES (?, ?, ?, ?, ?);',
                (
                    result.item_id,
                    result.title,
                    result.uploader or item.uploader or '',
                    result.release_date or '',
                    result.plot or '',
                ),
            )

            item.title = result.title
            item.uploader = result.uploader or item.uploader
            file_size_bytes: int | None = None
            try:
                file_size_bytes = result.final_path.stat().st_size
            except OSError as exc:
                log.debug('Failed to read file size for %s: %s', result.final_path, exc)
            await self._notify_download(
                item=item,
                resolution=result.resolution,
                file_size_bytes=file_size_bytes,
                release_date=result.release_date,
                cover_url=result.cover_url,
            )
            log.notice('Saved %s', result.final_path.name)
