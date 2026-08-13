"""Read-only browsing of the per-source archive tables.

Every table, column and sort order used here comes from the ``ARCHIVE_SOURCES``
whitelist. Client input only ever reaches PostgreSQL as a bound parameter.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

import psycopg
from psycopg import sql
from psycopg.rows import dict_row

from src.core import settings
from src.web.rednote import build_note_url

if TYPE_CHECKING:
    from collections.abc import Callable

MAX_LIMIT = 200
DEFAULT_LIMIT = 50


@dataclass(frozen=True, slots=True)
class ArchiveSource:
    key: str
    name: str
    table: str
    id_columns: tuple[str, ...]
    title_column: str
    columns: tuple[str, ...]
    search_columns: tuple[str, ...]
    time_column: str = 'created_at'
    subtitle_columns: tuple[str, ...] = ()

    @property
    def selected_columns(self) -> tuple[str, ...]:
        seen: list[str] = []
        for column in (*self.id_columns, self.title_column, *self.columns, self.time_column):
            if column and column not in seen:
                seen.append(column)
        return tuple(seen)


ARCHIVE_SOURCES: dict[str, ArchiveSource] = {
    source.key: source
    for source in (
        ArchiveSource(
            key='bilibili',
            name='Bilibili',
            table='bilibili',
            id_columns=('bvid',),
            title_column='title',
            columns=('upper', 'fav_id'),
            search_columns=('bvid', 'title', 'upper'),
            subtitle_columns=('upper',),
        ),
        ArchiveSource(
            key='telegram',
            name='Telegram',
            table='telegram',
            id_columns=('account_name', 'channel_id', 'message_id'),
            title_column='title',
            columns=('channel_name', 'media_type'),
            search_columns=('title', 'channel_name'),
            subtitle_columns=('channel_name', 'media_type'),
        ),
        ArchiveSource(
            key='hanime1',
            name='Hanime1',
            table='hanime1',
            id_columns=('id',),
            title_column='title',
            columns=('uploader', 'release_date'),
            search_columns=('id', 'title', 'uploader'),
            subtitle_columns=('uploader', 'release_date'),
        ),
        ArchiveSource(
            key='jandan',
            name='Jandan',
            table='jandan',
            id_columns=('fav_type', 'pic_id', 'content_index'),
            title_column='author',
            columns=('content_url', 'post_date', 'local_path', 'downloaded', 'unavailable', 'last_error'),
            search_columns=('author', 'content_url'),
            subtitle_columns=('post_date',),
        ),
        ArchiveSource(
            key='twitter',
            name='X',
            table='twitter',
            id_columns=('tweet_id', 'num'),
            title_column='content',
            columns=('author', 'author_nick', 'tweet_date', 'media_type', 'local_path'),
            search_columns=('author', 'author_nick', 'content'),
            subtitle_columns=('author', 'tweet_date'),
        ),
        ArchiveSource(
            key='rednote',
            name='RedNote',
            table='rednote',
            id_columns=('note_id', 'media_index'),
            title_column='title',
            # xsec_token is not decoration: without it the note only opens for its author.
            columns=('author', 'media_type', 'published_at', 'local_path', 'downloaded', 'unavailable', 'last_error', 'xsec_token'),
            search_columns=('note_id', 'title', 'author'),
            subtitle_columns=('author', 'published_at'),
        ),
        ArchiveSource(
            key='kemono',
            name='Kemono',
            table='kemono',
            id_columns=('id', 'service', 'user_id'),
            title_column='title',
            columns=('creator', 'service', 'published_at'),
            search_columns=('id', 'title', 'creator'),
            subtitle_columns=('creator', 'service'),
        ),
    )
}


class UnknownArchiveSourceError(ValueError):
    def __init__(self, source: str) -> None:
        super().__init__(f'Unknown archive source: {source}')
        self.source = source


def _hanime1_url(row: dict[str, Any]) -> str:
    host = settings.load().web.hanime1.host.rstrip('/')
    return f'{host}/watch?v={row["id"]}'


_EXTERNAL_URL_BUILDERS: dict[str, Callable[[dict[str, Any]], str | None]] = {
    'bilibili': lambda row: f'https://www.bilibili.com/video/{row["bvid"]}',
    'hanime1': _hanime1_url,
    'kemono': lambda row: f'https://kemono.cr/{row["service"]}/user/{row["user_id"]}/post/{row["id"]}',
    'jandan': lambda row: str(row['content_url']) if row.get('content_url') else None,
    # /i/status resolves without knowing the author's current handle.
    'twitter': lambda row: f'https://x.com/i/status/{row["tweet_id"]}',
    'rednote': lambda row: build_note_url(str(row['note_id']), str(row.get('xsec_token') or '')),
}


def _external_url(source: ArchiveSource, row: dict[str, Any]) -> str | None:
    builder = _EXTERNAL_URL_BUILDERS.get(source.key)
    return builder(row) if builder is not None else None


def _item_id(source: ArchiveSource, row: dict[str, Any]) -> str:
    return ':'.join(str(row[column]) for column in source.id_columns)


def _subtitle(source: ArchiveSource, row: dict[str, Any]) -> str:
    parts = [str(row[column]).strip() for column in source.subtitle_columns if row.get(column)]
    return ' · '.join(part for part in parts if part)


def _serialize(source: ArchiveSource, row: dict[str, Any]) -> dict[str, Any]:
    created_at = row.get(source.time_column)
    return {
        'source': source.key,
        'id': _item_id(source, row),
        'title': str(row.get(source.title_column) or ''),
        'subtitle': _subtitle(source, row),
        'created_at': created_at.isoformat() if created_at is not None else None,
        'url': _external_url(source, row),
        'fields': {
            column: (value.isoformat() if hasattr(value, 'isoformat') else value)
            for column, value in row.items()
            if column != source.time_column
        },
    }


class ArchiveLibrary:
    def __init__(self, dsn: str) -> None:
        self._dsn = dsn

    def _connect(self) -> psycopg.Connection:
        return psycopg.connect(self._dsn, autocommit=True, row_factory=dict_row)

    def list_sources(self) -> list[dict[str, Any]]:
        stats: list[dict[str, Any]] = []
        with self._connect() as conn, conn.cursor() as cursor:
            for source in ARCHIVE_SOURCES.values():
                # A source table only exists once its crawler has run at least once.
                cursor.execute('SELECT to_regclass(%s) AS table_oid;', (source.table,))
                row = cursor.fetchone()
                if row is None or row['table_oid'] is None:
                    stats.append({'source': source.key, 'name': source.name, 'total': 0, 'latest_at': None})
                    continue
                cursor.execute(
                    sql.SQL('SELECT COUNT(*) AS total, MAX({time_column}) AS latest_at FROM {table};').format(
                        time_column=sql.Identifier(source.time_column),
                        table=sql.Identifier(source.table),
                    ),
                )
                counts = cursor.fetchone() or {}
                latest_at = counts.get('latest_at')
                stats.append(
                    {
                        'source': source.key,
                        'name': source.name,
                        'total': int(counts.get('total') or 0),
                        'latest_at': latest_at.isoformat() if latest_at is not None else None,
                    },
                )
        return stats

    def list_items(
        self,
        *,
        source_key: str,
        query: str = '',
        limit: int = DEFAULT_LIMIT,
        offset: int = 0,
    ) -> dict[str, Any]:
        source = ARCHIVE_SOURCES.get(source_key)
        if source is None:
            raise UnknownArchiveSourceError(source_key)

        bounded_limit = max(1, min(int(limit), MAX_LIMIT))
        bounded_offset = max(0, int(offset))
        normalized_query = query.strip()

        table = sql.Identifier(source.table)
        where_sql = sql.SQL('')
        params: list[Any] = []
        if normalized_query:
            clauses = sql.SQL(' OR ').join(
                sql.SQL('{column}::text ILIKE %s').format(column=sql.Identifier(column)) for column in source.search_columns
            )
            where_sql = sql.SQL(' WHERE ({clauses})').format(clauses=clauses)
            params.extend([f'%{normalized_query}%'] * len(source.search_columns))

        columns_sql = sql.SQL(', ').join(sql.Identifier(column) for column in source.selected_columns)
        order_sql = sql.SQL(', ').join(
            [
                sql.SQL('{column} DESC NULLS LAST').format(column=sql.Identifier(source.time_column)),
                *(sql.SQL('{column} DESC').format(column=sql.Identifier(column)) for column in source.id_columns),
            ],
        )

        with self._connect() as conn, conn.cursor() as cursor:
            cursor.execute('SELECT to_regclass(%s) AS table_oid;', (source.table,))
            table_row = cursor.fetchone()
            if table_row is None or table_row['table_oid'] is None:
                return {'items': [], 'total': 0, 'limit': bounded_limit, 'offset': bounded_offset}

            cursor.execute(
                sql.SQL('SELECT COUNT(*) AS total FROM {table}{where};').format(table=table, where=where_sql),
                tuple(params),
            )
            total = int((cursor.fetchone() or {}).get('total') or 0)

            cursor.execute(
                sql.SQL('SELECT {columns} FROM {table}{where} ORDER BY {order} LIMIT %s OFFSET %s;').format(
                    columns=columns_sql,
                    table=table,
                    where=where_sql,
                    order=order_sql,
                ),
                (*params, bounded_limit, bounded_offset),
            )
            rows = cursor.fetchall()

        return {
            'items': [_serialize(source, row) for row in rows],
            'total': total,
            'limit': bounded_limit,
            'offset': bounded_offset,
        }
