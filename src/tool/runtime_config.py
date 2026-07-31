from __future__ import annotations

import html
import json
import re
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, urlsplit

import httpx
from opencc import OpenCC

from src.core import logger

log = logger.get('runtime-config')

_HANIME1_SEED_RE = re.compile(r'^(?:(?P<title>.*?)\s*)?\{id-(?P<id>\d+)\}\s*$', re.IGNORECASE)
_HANIME1_PLAYLIST_TITLE_RE = re.compile(
    r'<h4[^>]*>(?P<title>.*?)</h4>',
    re.IGNORECASE | re.DOTALL,
)
_HANIME1_PLAYLIST_LINK_TITLE_RE = re.compile(
    r'<a[^>]+href=["\'][^"\']*/playlist\?[^"\']*["\'][^>]*>(?P<title>.*?)</a>',
    re.IGNORECASE | re.DOTALL,
)
_HANIME1_PLAYLIST_VIDEO_TITLE_RE = re.compile(
    r'<h4[^>]+class=["\'][^"\']*\bvideo-title\b[^"\']*["\'][^>]*>(?P<title>.*?)</h4>',
    re.IGNORECASE | re.DOTALL,
)
_HANIME1_CARD_MOBILE_TITLE_RE = re.compile(
    r'<div[^>]+class=["\'][^"\']*\bcard-mobile-title\b[^"\']*["\'][^>]*>(?P<title>.*?)</div>',
    re.IGNORECASE | re.DOTALL,
)
_HANIME1_IMAGE_ALT_RE = re.compile(r'<img\b[^>]*\balt=["\'](?P<title>.*?)["\']', re.IGNORECASE | re.DOTALL)
_HANIME1_ANCHOR_RE = re.compile(r'<a\b[^>]*>(?P<title>.*?)</a>', re.IGNORECASE | re.DOTALL)
_HANIME1_WATCH_HREF_RE = re.compile(
    r'href=["\'](?P<url>(?:https?:)?//[^"\']+/watch\?v=[^"\'>\s]+|/watch\?v=[^"\'>\s]+|watch\?v=[^"\'>\s]+)["\']',
    re.IGNORECASE,
)
_HANIME1_CF_BLOCK_MARKERS = ('Attention Required! | Cloudflare', 'cf-error-details')
_HANIME1_USER_AGENT = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36'
_T2S_CONVERTER = OpenCC('t2s')


class Hanime1ParserIncompatibleError(RuntimeError):
    notification_dedupe_key = 'hanime1:parser:playlist-wrapper'


@dataclass(slots=True, frozen=True)
class RuntimeSeriesSeed:
    video_id: str
    title: str

    @property
    def label(self) -> str:
        return build_hanime1_seed(seed_id=self.video_id, title=self.title)


@dataclass(slots=True, frozen=True)
class Hanime1PlaylistEntry:
    video_id: str
    title: str | None = None


def to_simplified_chinese(text: str | None) -> str | None:
    if not text:
        return None
    converted = _T2S_CONVERTER.convert(text).strip()
    return converted or None


def seed_has_title(seed: str) -> bool:
    match = _HANIME1_SEED_RE.fullmatch(seed.strip())
    if not match:
        return False
    return bool((match.group('title') or '').strip())


def extract_hanime1_seed_id(seed: str) -> str | None:
    match = _HANIME1_SEED_RE.fullmatch(seed.strip())
    if not match:
        return None
    normalized = str(int(match.group('id')))
    if normalized == '0':
        return None
    return normalized


def normalize_hanime1_seed(raw: str, *, allow_id_only: bool = True) -> str | None:  # noqa: PLR0911
    text = raw.strip()
    if not text:
        return None

    if text.isdecimal():
        if not allow_id_only:
            return None
        seed_id = str(int(text))
        if seed_id == '0':
            return None
        return f'{{id-{seed_id}}}'

    match = _HANIME1_SEED_RE.fullmatch(text)
    if not match:
        return None

    seed_id = str(int(match.group('id')))
    if seed_id == '0':
        return None

    title = to_simplified_chinese((match.group('title') or '').strip() or None)
    if not title:
        if not allow_id_only:
            return None
        return f'{{id-{seed_id}}}'
    return f'{title} {{id-{seed_id}}}'


def normalize_hanime1_keywords(raw_keywords: list[Any]) -> list[str]:
    keywords: list[str] = []
    index_by_id: dict[str, int] = {}
    for raw in raw_keywords:
        if not isinstance(raw, str):
            continue
        keyword = normalize_hanime1_seed(raw, allow_id_only=False)
        if keyword is None:
            continue
        seed_id = extract_hanime1_seed_id(keyword)
        if seed_id is None:
            continue
        existing_idx = index_by_id.get(seed_id)
        if existing_idx is None:
            index_by_id[seed_id] = len(keywords)
            keywords.append(keyword)
            continue
        existing = keywords[existing_idx]
        if seed_has_title(existing):
            continue
        if seed_has_title(keyword):
            keywords[existing_idx] = keyword
    return keywords


def normalize_hanime1_watch_title(title: str) -> str:
    normalized = html.unescape(re.sub(r'<[^>]+>', '', title))
    normalized = re.sub(r'\s+', ' ', normalized).strip()
    normalized = re.sub(r'\s*-\s*Hanime1\.me\s*$', '', normalized, flags=re.IGNORECASE).strip()
    return re.sub(r'\s*-\s*H動漫/裏番/線上看\s*$', '', normalized).strip()


def _extract_hanime1_div_blocks(page_html: str, pattern: re.Pattern[str]) -> list[str]:
    tag_pattern = re.compile(r'</?div\b[^>]*>', re.IGNORECASE)
    blocks: list[str] = []
    search_pos = 0
    while True:
        start_match = pattern.search(page_html, search_pos)
        if start_match is None:
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


def extract_hanime1_div_blocks_by_class(page_html: str, *, class_name: str) -> list[str]:
    pattern = re.compile(
        rf'<div\b(?=[^>]*\bclass=["\'](?:[^"\']*\s)?{re.escape(class_name)}(?:\s[^"\']*)?["\'])[^>]*>',
        re.IGNORECASE,
    )
    return _extract_hanime1_div_blocks(page_html, pattern)


def extract_hanime1_playlist_blocks(page_html: str) -> list[str]:
    pattern = re.compile(
        r'<div\b(?=[^>]*(?:'
        r'\bid=["\']video-playlist-wrapper["\']|'
        r'\bclass=["\'](?:[^"\']*\s)?video-playlist-wrapper(?:\s[^"\']*)?["\']'
        r'))[^>]*>',
        re.IGNORECASE,
    )
    return _extract_hanime1_div_blocks(page_html, pattern)


def extract_hanime1_playlist_title(playlist_html: str) -> str | None:
    match = _HANIME1_PLAYLIST_TITLE_RE.search(playlist_html)
    if match is None:
        return None
    title_html = match.group('title')
    link_match = _HANIME1_PLAYLIST_LINK_TITLE_RE.search(title_html)
    title = normalize_hanime1_watch_title(link_match.group('title') if link_match is not None else title_html)
    title = re.sub(r'^(?:清單|清单)\s*', '', title).strip()
    return title or None


def extract_hanime1_series_ids(playlist_html: str) -> list[str]:
    ids: list[str] = []
    seen: set[str] = set()
    normalized_html = html.unescape(playlist_html).replace('\\/', '/').replace('\\u0026', '&')
    for match in _HANIME1_WATCH_HREF_RE.finditer(normalized_html):
        query = parse_qs(urlsplit(match.group('url')).query)
        values = query.get('v')
        if not values:
            continue
        candidate = values[0].strip()
        if not candidate.isdecimal():
            continue
        normalized = str(int(candidate))
        if normalized == '0':
            continue
        if normalized in seen:
            continue
        seen.add(normalized)
        ids.append(normalized)

    return ids


def _extract_hanime1_playlist_item_title(item_html: str) -> str | None:
    for pattern in (_HANIME1_PLAYLIST_VIDEO_TITLE_RE, _HANIME1_CARD_MOBILE_TITLE_RE, _HANIME1_IMAGE_ALT_RE):
        match = pattern.search(item_html)
        if match is None:
            continue
        title = normalize_hanime1_watch_title(match.group('title'))
        if title:
            return to_simplified_chinese(title)
    return None


def _extract_hanime1_watch_link_title(item_html: str) -> str | None:
    for match in _HANIME1_ANCHOR_RE.finditer(item_html):
        video_ids = extract_hanime1_series_ids(match.group(0))
        if len(video_ids) != 1:
            continue
        title = normalize_hanime1_watch_title(match.group('title'))
        if title:
            return to_simplified_chinese(title) or title
    return None


def extract_hanime1_playlist_entries(
    page_html: str,
    *,
    require_current_titles: bool = False,
) -> list[Hanime1PlaylistEntry]:
    playlist_blocks = extract_hanime1_playlist_blocks(page_html)
    ordered_ids: list[str] = []
    seen_ids: set[str] = set()
    explicit_titles: dict[str, str] = {}
    fallback_titles: dict[str, str] = {}
    has_current_cards = False

    for playlist_html in playlist_blocks:
        for video_id in extract_hanime1_series_ids(playlist_html):
            if video_id not in seen_ids:
                seen_ids.add(video_id)
                ordered_ids.append(video_id)

        current_blocks = extract_hanime1_div_blocks_by_class(playlist_html, class_name='playlist-video-card')
        allow_link_fallback = not current_blocks
        if not current_blocks:
            current_blocks = extract_hanime1_div_blocks_by_class(playlist_html, class_name='video-item-container')
        has_current_cards |= bool(current_blocks)
        legacy_blocks = extract_hanime1_div_blocks_by_class(playlist_html, class_name='related-watch-wrap')

        for item_html in [*current_blocks, *legacy_blocks]:
            video_ids = extract_hanime1_series_ids(item_html)
            title = _extract_hanime1_playlist_item_title(item_html)
            title_map = explicit_titles
            if not title and allow_link_fallback and item_html in current_blocks:
                title = _extract_hanime1_watch_link_title(item_html)
                title_map = fallback_titles
            if title:
                for video_id in video_ids:
                    title_map.setdefault(video_id, title)

    titles = fallback_titles | explicit_titles

    if require_current_titles and has_current_cards and not titles:
        msg = 'Hanime1 playlist cards did not expose compatible item titles'
        raise Hanime1ParserIncompatibleError(msg)

    return [Hanime1PlaylistEntry(video_id=video_id, title=titles.get(video_id)) for video_id in ordered_ids]


def parse_hanime1_playlist(
    page_html: str,
    *,
    seed_id: str | None = None,
    require_current_titles: bool = False,
) -> tuple[str, list[str]] | None:
    playlist_blocks = extract_hanime1_playlist_blocks(page_html)
    title = next((title for block in playlist_blocks if (title := extract_hanime1_playlist_title(block))), None)
    if not title:
        return None

    entries = extract_hanime1_playlist_entries(page_html, require_current_titles=require_current_titles)
    series_ids = [entry.video_id for entry in entries]
    if not series_ids:
        return None
    if seed_id is not None and seed_id not in set(series_ids):
        series_ids.append(seed_id)
    return title, series_ids


def select_hanime1_canonical_id(series_ids: list[str]) -> str | None:
    if not series_ids:
        return None
    try:
        return str(min(int(item) for item in series_ids))
    except ValueError:
        return None


def build_hanime1_seed(*, seed_id: str, title: str | None) -> str:
    normalized_title = to_simplified_chinese(title)
    if not normalized_title:
        msg = 'title is required for hanime1 seed'
        raise ValueError(msg)
    return f'{normalized_title} {{id-{seed_id}}}'


def parse_runtime_series_seed(raw: Any) -> RuntimeSeriesSeed | None:
    if not isinstance(raw, str):
        return None
    text = raw.strip()
    if not text:
        return None

    match = _HANIME1_SEED_RE.fullmatch(text)
    if not match:
        return None

    video_id = extract_hanime1_seed_id(text)
    if not video_id:
        return None

    title = to_simplified_chinese((match.group('title') or '').strip() or None)
    if not title:
        return None
    return RuntimeSeriesSeed(video_id=video_id, title=title)


class Hanime1RuntimeConfigStore:
    def __init__(self, run_config: Path) -> None:
        self._run_config = run_config

    def read_payload(self) -> dict[str, Any]:
        if not self._run_config.exists():
            return {}
        try:
            payload = json.loads(self._run_config.read_text(encoding='utf-8'))
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to read runtime config %s: %s', self._run_config, exc)
            return {}
        if not isinstance(payload, dict):
            return {}
        return payload

    def write_payload(self, payload: dict[str, Any]) -> None:
        self._run_config.parent.mkdir(parents=True, exist_ok=True)
        tmp_path: Path | None = None
        try:
            with tempfile.NamedTemporaryFile(
                mode='w',
                encoding='utf-8',
                dir=self._run_config.parent,
                prefix=f'{self._run_config.name}.',
                suffix='.tmp',
                delete=False,
            ) as tmp_file:
                tmp_file.write(json.dumps(payload, ensure_ascii=False, indent=2))
                tmp_file.flush()
                tmp_path = Path(tmp_file.name)
            tmp_path.replace(self._run_config)
        finally:
            if tmp_path is not None and tmp_path.exists():
                tmp_path.unlink(missing_ok=True)

    def read_hanime1_keywords(self) -> list[str]:
        payload = self.read_payload()
        hanime1 = payload.get('hanime1')
        if not isinstance(hanime1, dict):
            return []
        raw_keywords = hanime1.get('keywords')
        if not isinstance(raw_keywords, list):
            return []
        return normalize_hanime1_keywords(raw_keywords)

    def write_hanime1_keywords(self, keywords: list[str]) -> None:
        payload = self.read_payload()
        hanime1 = payload.get('hanime1')
        if not isinstance(hanime1, dict):
            hanime1 = {}
            payload['hanime1'] = hanime1
        hanime1['keywords'] = keywords
        self.write_payload(payload)

    def read_hanime1_seeds(self) -> list[RuntimeSeriesSeed]:
        payload = self.read_payload()
        hanime1 = payload.get('hanime1')
        if not isinstance(hanime1, dict):
            return []
        raw_keywords = hanime1.get('keywords')
        if not isinstance(raw_keywords, list):
            return []

        deduped: list[RuntimeSeriesSeed] = []
        seen_ids: set[str] = set()
        for raw in raw_keywords:
            seed = parse_runtime_series_seed(raw)
            if seed is None:
                continue
            key = seed.video_id.casefold()
            if key in seen_ids:
                continue
            seen_ids.add(key)
            deduped.append(seed)
        return deduped

    def write_hanime1_seeds(self, seeds: list[RuntimeSeriesSeed]) -> None:
        self.write_hanime1_keywords([seed.label for seed in seeds])

    def delete_hanime1_seed(self, video_id: str) -> RuntimeSeriesSeed | None:
        normalized_video_id = extract_hanime1_seed_id(f'{{id-{video_id}}}')
        if normalized_video_id is None:
            return None
        seeds = self.read_hanime1_seeds()
        remaining = [seed for seed in seeds if seed.video_id != normalized_video_id]
        if len(remaining) == len(seeds):
            return None
        removed = next(seed for seed in seeds if seed.video_id == normalized_video_id)
        self.write_hanime1_seeds(remaining)
        return removed


class Hanime1RuntimeConfigService:
    def __init__(
        self,
        *,
        run_config: Path,
        host: str,
        user_lang: str,
        proxy: str | None = None,
        client: httpx.Client | None = None,
    ) -> None:
        self._store = Hanime1RuntimeConfigStore(run_config)
        self._host = host.rstrip('/')
        self._user_lang = user_lang
        self._owns_client = client is None
        self._client = client or httpx.Client(timeout=40, proxy=proxy)

    def list_seeds(self) -> list[RuntimeSeriesSeed]:
        return self._store.read_hanime1_seeds()

    def delete_seed(self, video_id: str) -> RuntimeSeriesSeed | None:
        return self._store.delete_hanime1_seed(video_id)

    def add_seed(self, raw_seed: str) -> RuntimeSeriesSeed:
        normalized_seed = normalize_hanime1_seed(raw_seed, allow_id_only=True)
        if normalized_seed is None:
            msg = 'invalid_seed'
            raise ValueError(msg)

        seed_id = extract_hanime1_seed_id(normalized_seed)
        if seed_id is None:
            msg = 'invalid_seed'
            raise ValueError(msg)

        resolved = self._resolve_hanime1_series(seed_id)
        if resolved is None:
            msg = 'seed_resolve_failed'
            raise LookupError(msg)

        resolved_title, series_ids = resolved
        canonical_id = select_hanime1_canonical_id(series_ids)
        if canonical_id is None:
            msg = 'seed_resolve_failed'
            raise LookupError(msg)

        final_seed = RuntimeSeriesSeed(video_id=canonical_id, title=to_simplified_chinese(resolved_title) or resolved_title)
        seeds = self._store.read_hanime1_seeds()
        if any(seed.video_id == final_seed.video_id for seed in seeds):
            msg = 'duplicate_seed'
            raise FileExistsError(msg)
        seeds.append(final_seed)
        self._store.write_hanime1_seeds(seeds)
        return final_seed

    def close(self) -> None:
        if self._owns_client:
            self._client.close()

    def _resolve_hanime1_series(self, seed_id: str) -> tuple[str, list[str]] | None:
        watch_url = f'{self._host}/watch?v={seed_id}'
        headers = {
            'Referer': f'{self._host}/',
            'User-Agent': _HANIME1_USER_AGENT,
            'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8',
            'Accept-Language': 'zh-CN,zh;q=0.9,en;q=0.8',
            'Cookie': f'user_lang={self._user_lang}',
        }

        try:
            response = self._client.get(watch_url, headers=headers)
            response.raise_for_status()
        except Exception as exc:  # noqa: BLE001
            log.debug('Failed to resolve Hanime1 seed title for %s: %s', seed_id, exc)
            return None

        page_html = response.text
        if all(marker in page_html for marker in _HANIME1_CF_BLOCK_MARKERS):
            log.debug('Blocked by Cloudflare while resolving Hanime1 seed title for %s', seed_id)
            return None

        return parse_hanime1_playlist(page_html, seed_id=seed_id)
