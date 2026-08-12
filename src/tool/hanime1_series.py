from __future__ import annotations

from typing import Any

import httpx
import psycopg
from psycopg.rows import dict_row

from src.tool import database
from src.tool.runtime_config import (
    Hanime1ParserIncompatibleError,
    RuntimeSeriesSeed,
    build_hanime1_seed,
    extract_hanime1_seed_id,
    normalize_hanime1_seed,
    parse_hanime1_playlist,
    select_hanime1_canonical_id,
    to_simplified_chinese,
)

USER_AGENT = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36'
CF_BLOCK_MARKERS = ('Attention Required! | Cloudflare', 'cf-error-details')

CREATE_HANIME1_SERIES_TABLES_SQL = """
    CREATE TABLE IF NOT EXISTS hanime1_series (
        canonical_video_id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        added_from_video_id TEXT NOT NULL,
        created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
        updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
        last_scanned_at TIMESTAMPTZ,
        last_scan_error TEXT NOT NULL DEFAULT '',

        CHECK (canonical_video_id ~ '^[1-9][0-9]*$'),
        CHECK (added_from_video_id ~ '^[1-9][0-9]*$'),
        CHECK (btrim(title) <> '')
    );

    CREATE TABLE IF NOT EXISTS hanime1_series_video (
        video_id TEXT PRIMARY KEY,
        canonical_video_id TEXT NOT NULL REFERENCES hanime1_series(canonical_video_id) ON DELETE CASCADE,
        discovered_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,

        CHECK (video_id ~ '^[1-9][0-9]*$')
    );

    CREATE INDEX IF NOT EXISTS idx_hanime1_series_video_series
        ON hanime1_series_video (canonical_video_id);

    CREATE TABLE IF NOT EXISTS hanime1_ranking_scan_state (
        id SMALLINT PRIMARY KEY DEFAULT 1,
        quota_debt DOUBLE PRECISION NOT NULL DEFAULT 0,
        updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,

        CHECK (id = 1)
    );
"""


async def ensure_hanime1_series_tables() -> None:
    await database.query_db(CREATE_HANIME1_SERIES_TABLES_SQL)


class Hanime1SeriesService:
    def __init__(
        self,
        *,
        dsn: str,
        host: str,
        user_lang: str,
        proxy: str | None = None,
        client: httpx.Client | None = None,
    ) -> None:
        self._dsn = dsn
        self._host = host.rstrip('/')
        self._user_lang = user_lang
        self._owns_client = client is None
        self._client = client or httpx.Client(timeout=40, proxy=proxy)

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
        self._insert_series_seed(final_seed=final_seed, added_from_video_id=seed_id, series_ids=series_ids)
        return final_seed

    def list_seeds(self) -> list[dict[str, Any]]:
        with self._connect() as conn, conn.cursor() as cursor:
            self._ensure_tables_sync(cursor)
            cursor.execute(
                """
                SELECT series.canonical_video_id,
                       series.title,
                       series.added_from_video_id,
                       series.created_at,
                       series.updated_at,
                       series.last_scanned_at,
                       series.last_scan_error,
                       COUNT(video.video_id) AS video_count
                FROM hanime1_series AS series
                LEFT JOIN hanime1_series_video AS video
                  ON video.canonical_video_id = series.canonical_video_id
                GROUP BY series.canonical_video_id
                ORDER BY series.created_at, series.canonical_video_id;
                """,
            )
            rows = cursor.fetchall()

        seeds: list[dict[str, Any]] = []
        for row in rows:
            video_id = str(row['canonical_video_id'])
            title = str(row['title'])
            seeds.append(
                {
                    'video_id': video_id,
                    'title': title,
                    'label': build_hanime1_seed(seed_id=video_id, title=title),
                    'added_from_video_id': str(row['added_from_video_id']),
                    'video_count': int(row['video_count'] or 0),
                    'created_at': row['created_at'],
                    'updated_at': row['updated_at'],
                    'last_scanned_at': row['last_scanned_at'],
                    'last_scan_error': str(row['last_scan_error'] or ''),
                    'watch_url': f'{self._host}/watch?v={video_id}',
                },
            )
        return seeds

    def delete_seed(self, canonical_video_id: str) -> bool:
        """Delete a series seed. Its hanime1_series_video rows cascade."""
        normalized = canonical_video_id.strip()
        if not normalized.isdecimal() or int(normalized) <= 0:
            msg = 'invalid_seed'
            raise ValueError(msg)
        with self._connect() as conn, conn.cursor() as cursor:
            self._ensure_tables_sync(cursor)
            cursor.execute(
                'DELETE FROM hanime1_series WHERE canonical_video_id = %s RETURNING canonical_video_id;',
                (str(int(normalized)),),
            )
            deleted = cursor.fetchone() is not None
            conn.commit()
        return deleted

    def close(self) -> None:
        if self._owns_client:
            self._client.close()

    def _connect(self) -> psycopg.Connection:
        return psycopg.connect(self._dsn, row_factory=dict_row)

    def _ensure_tables_sync(self, cursor: psycopg.Cursor) -> None:
        for statement in database._split_sql_statements(CREATE_HANIME1_SERIES_TABLES_SQL):  # noqa: SLF001
            cursor.execute(statement)

    def readiness_seed_ids(self, *, limit: int = 3) -> list[str]:
        with self._connect() as conn, conn.cursor() as cursor:
            self._ensure_tables_sync(cursor)
            cursor.execute(
                """
                SELECT canonical_video_id
                FROM hanime1_series
                ORDER BY last_scanned_at DESC NULLS LAST, updated_at DESC, canonical_video_id
                LIMIT %s;
                """,
                (limit,),
            )
            return [str(row['canonical_video_id']) for row in cursor.fetchall()]

    def probe_seed(self, seed_id: str, *, timeout_seconds: float = 10) -> None:
        page_html = self._fetch_watch_page_html(seed_id, timeout_seconds=timeout_seconds)
        if parse_hanime1_playlist(page_html, seed_id=seed_id, require_current_titles=True) is None:
            msg = 'Hanime1 watch page did not expose a compatible playlist structure'
            raise Hanime1ParserIncompatibleError(msg)

    def _fetch_watch_page_html(self, seed_id: str, *, timeout_seconds: float | None = None) -> str:
        watch_url = f'{self._host}/watch?v={seed_id}'
        headers = {
            'Referer': f'{self._host}/',
            'User-Agent': USER_AGENT,
            'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8',
            'Accept-Language': 'zh-CN,zh;q=0.9,en;q=0.8',
            'Cookie': f'user_lang={self._user_lang}',
        }

        request_kwargs = {'timeout': timeout_seconds} if timeout_seconds is not None else {}
        response = self._client.get(watch_url, headers=headers, **request_kwargs)
        response.raise_for_status()
        page_html = response.text
        if all(marker in page_html for marker in CF_BLOCK_MARKERS):
            msg = 'Hanime1 watch page was blocked by Cloudflare'
            raise RuntimeError(msg)
        return page_html

    def _resolve_hanime1_series(self, seed_id: str) -> tuple[str, list[str]] | None:
        try:
            page_html = self._fetch_watch_page_html(seed_id)
        except Exception:  # noqa: BLE001
            return None
        try:
            return parse_hanime1_playlist(page_html, seed_id=seed_id, require_current_titles=True)
        except Hanime1ParserIncompatibleError:
            return None

    def _insert_series_seed(
        self,
        *,
        final_seed: RuntimeSeriesSeed,
        added_from_video_id: str,
        series_ids: list[str],
    ) -> None:
        with self._connect() as conn, conn.cursor() as cursor:
            self._ensure_tables_sync(cursor)
            cursor.execute(
                """
                SELECT canonical_video_id
                FROM hanime1_series_video
                WHERE video_id = ANY(%s)
                ORDER BY canonical_video_id
                LIMIT 1;
                """,
                (series_ids,),
            )
            if cursor.fetchone() is not None:
                msg = 'duplicate_seed'
                raise FileExistsError(msg)

            cursor.execute(
                """
                INSERT INTO hanime1_series (canonical_video_id, title, added_from_video_id)
                VALUES (%s, %s, %s)
                ON CONFLICT (canonical_video_id) DO NOTHING;
                """,
                (final_seed.video_id, final_seed.title, added_from_video_id),
            )
            if cursor.rowcount == 0:
                msg = 'duplicate_seed'
                raise FileExistsError(msg)

            for video_id in series_ids:
                cursor.execute(
                    """
                    INSERT INTO hanime1_series_video (video_id, canonical_video_id)
                    VALUES (%s, %s)
                    ON CONFLICT (video_id) DO NOTHING;
                    """,
                    (video_id, final_seed.video_id),
                )
