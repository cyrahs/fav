"""Hanime1 author subscriptions.

Pure HTML parsers for the author "uploaded" listing page plus the synchronous
service the API uses to manage subscription rows. The worker-side enumeration
lives in `src.web.hanime1`.
"""

from __future__ import annotations

import html
import re
from dataclasses import dataclass
from typing import Any
from urllib.parse import parse_qs, urlsplit

import httpx
import psycopg
from psycopg.rows import dict_row

from src.tool import database
from src.tool.runtime_config import to_simplified_chinese

USER_AGENT = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36'
CF_BLOCK_MARKERS = ('Attention Required! | Cloudflare', 'cf-error-details')

CREATE_HANIME1_AUTHOR_TABLES_SQL = """
    CREATE TABLE IF NOT EXISTS hanime1_author (
        author_id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        video_count INTEGER NOT NULL DEFAULT 0,
        created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
        updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
        last_scanned_at TIMESTAMPTZ,
        last_scan_error TEXT NOT NULL DEFAULT '',

        CHECK (author_id ~ '^[1-9][0-9]*$'),
        CHECK (btrim(name) <> '')
    );
"""

_AUTHOR_DISPLAY_NAME_RE = re.compile(
    r'<h1[^>]+class=["\'][^"\']*profile-display-name[^"\']*["\'][^>]*>(?P<name>.*?)</h1>',
    re.IGNORECASE | re.DOTALL,
)
_AUTHOR_PAGE_TITLE_RE = re.compile(r'<title[^>]*>(?P<name>.*?)\s*-\s*影片\s*-', re.IGNORECASE | re.DOTALL)
_AUTHOR_VIDEO_LINK_RE = re.compile(
    r'<a\b(?P<attrs>[^>]*\bclass=["\'][^"\']*\bvideo-link\b[^"\']*["\'][^>]*)>(?P<body>.*?)</a>',
    re.IGNORECASE | re.DOTALL,
)
_HREF_RE = re.compile(r'href=["\'](?P<url>[^"\']+)["\']', re.IGNORECASE)
_ENTRY_TITLE_RE = re.compile(r'<div[^>]+class=["\'][^"\']*\btitle\b[^"\']*["\'][^>]*>(?P<title>.*?)</div>', re.IGNORECASE | re.DOTALL)


@dataclass(slots=True, frozen=True)
class Hanime1Author:
    author_id: str
    name: str


@dataclass(slots=True, frozen=True)
class Hanime1AuthorVideoEntry:
    video_id: str
    title: str | None = None


async def ensure_hanime1_author_tables() -> None:
    await database.query_db(CREATE_HANIME1_AUTHOR_TABLES_SQL)


def _clean_fragment_text(fragment: str) -> str | None:
    text = html.unescape(fragment)
    text = re.sub(r'<[^>]+>', '', text)
    text = re.sub(r'\s+', ' ', text).strip()
    return text or None


def extract_author_display_name(page_html: str) -> str | None:
    match = _AUTHOR_DISPLAY_NAME_RE.search(page_html)
    if match:
        name = _clean_fragment_text(match.group('name'))
        if name:
            return name

    match = _AUTHOR_PAGE_TITLE_RE.search(page_html)
    if not match:
        return None
    return _clean_fragment_text(match.group('name'))


def _extract_watch_video_id(url: str) -> str | None:
    query = parse_qs(urlsplit(html.unescape(url)).query)
    video_ids = query.get('v')
    if not video_ids:
        return None
    video_id = video_ids[0].strip()
    if not video_id.isdecimal() or int(video_id) <= 0:
        return None
    return video_id


def extract_author_video_entries(page_html: str) -> list[Hanime1AuthorVideoEntry]:
    entries: list[Hanime1AuthorVideoEntry] = []
    seen_ids: set[str] = set()
    for link_match in _AUTHOR_VIDEO_LINK_RE.finditer(page_html):
        href_match = _HREF_RE.search(link_match.group('attrs'))
        if not href_match:
            continue
        video_id = _extract_watch_video_id(href_match.group('url'))
        if not video_id or video_id in seen_ids:
            continue
        seen_ids.add(video_id)

        title: str | None = None
        title_match = _ENTRY_TITLE_RE.search(link_match.group('body'))
        if title_match:
            title = _clean_fragment_text(title_match.group('title'))
        entries.append(Hanime1AuthorVideoEntry(video_id=video_id, title=title))
    return entries


def extract_hanime1_author_id(raw: str) -> str | None:
    text = raw.strip()
    if not text:
        return None
    if text.isdecimal():
        return text if int(text) > 0 else None

    path_segments = [segment for segment in urlsplit(text).path.split('/') if segment]
    for index, segment in enumerate(path_segments):
        if segment != 'user' or index + 1 >= len(path_segments):
            continue
        candidate = path_segments[index + 1]
        if candidate.isdecimal() and int(candidate) > 0:
            return candidate
    return None


class Hanime1AuthorService:
    def __init__(
        self,
        *,
        dsn: str,
        host: str,
        user_lang: str,
        client: httpx.Client | None = None,
    ) -> None:
        self._dsn = dsn
        self._host = host.rstrip('/')
        self._user_lang = user_lang
        self._owns_client = client is None
        self._client = client or httpx.Client(timeout=40, follow_redirects=True)

    def add_author(self, raw_author: str) -> dict[str, Any]:
        author_id = extract_hanime1_author_id(raw_author)
        if author_id is None:
            msg = 'invalid_author'
            raise ValueError(msg)

        display_name = self._resolve_author_display_name(author_id)
        if display_name is None:
            msg = 'author_resolve_failed'
            raise LookupError(msg)

        name = to_simplified_chinese(display_name) or display_name
        self._insert_author(author_id=author_id, name=name)
        return {
            'author_id': author_id,
            'name': name,
            'author_url': self._author_page_url(author_id),
        }

    def list_authors(self) -> list[dict[str, Any]]:
        with self._connect() as conn, conn.cursor() as cursor:
            self._ensure_tables_sync(cursor)
            cursor.execute(
                """
                SELECT author_id,
                       name,
                       video_count,
                       created_at,
                       updated_at,
                       last_scanned_at,
                       last_scan_error
                FROM hanime1_author
                ORDER BY created_at, author_id;
                """,
            )
            rows = cursor.fetchall()

        authors: list[dict[str, Any]] = []
        for row in rows:
            author_id = str(row['author_id'])
            authors.append(
                {
                    'author_id': author_id,
                    'name': str(row['name']),
                    'video_count': int(row['video_count'] or 0),
                    'created_at': row['created_at'],
                    'updated_at': row['updated_at'],
                    'last_scanned_at': row['last_scanned_at'],
                    'last_scan_error': str(row['last_scan_error'] or ''),
                    'author_url': self._author_page_url(author_id),
                },
            )
        return authors

    def delete_author(self, author_id: str) -> bool:
        normalized = author_id.strip()
        if not normalized.isdecimal() or int(normalized) <= 0:
            msg = 'invalid_author'
            raise ValueError(msg)
        with self._connect() as conn, conn.cursor() as cursor:
            self._ensure_tables_sync(cursor)
            cursor.execute(
                'DELETE FROM hanime1_author WHERE author_id = %s RETURNING author_id;',
                (str(int(normalized)),),
            )
            deleted = cursor.fetchone() is not None
            conn.commit()
        return deleted

    def close(self) -> None:
        if self._owns_client:
            self._client.close()

    def _author_page_url(self, author_id: str) -> str:
        return f'{self._host}/user/{author_id}/uploaded'

    def _connect(self) -> psycopg.Connection:
        return psycopg.connect(self._dsn, row_factory=dict_row)

    def _ensure_tables_sync(self, cursor: psycopg.Cursor) -> None:
        for statement in database._split_sql_statements(CREATE_HANIME1_AUTHOR_TABLES_SQL):  # noqa: SLF001
            cursor.execute(statement)

    def _resolve_author_display_name(self, author_id: str) -> str | None:
        headers = {
            'Referer': f'{self._host}/',
            'User-Agent': USER_AGENT,
            'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8',
            'Accept-Language': 'zh-CN,zh;q=0.9,en;q=0.8',
            'Cookie': f'user_lang={self._user_lang}',
        }
        try:
            response = self._client.get(self._author_page_url(author_id), headers=headers)
            response.raise_for_status()
        except Exception:  # noqa: BLE001
            return None
        page_html = response.text
        if all(marker in page_html for marker in CF_BLOCK_MARKERS):
            return None
        return extract_author_display_name(page_html)

    def _insert_author(self, *, author_id: str, name: str) -> None:
        with self._connect() as conn, conn.cursor() as cursor:
            self._ensure_tables_sync(cursor)
            cursor.execute(
                """
                INSERT INTO hanime1_author (author_id, name)
                VALUES (%s, %s)
                ON CONFLICT (author_id) DO NOTHING;
                """,
                (author_id, name),
            )
            if cursor.rowcount == 0:
                msg = 'duplicate_author'
                raise FileExistsError(msg)
            conn.commit()
