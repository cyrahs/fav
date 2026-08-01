from __future__ import annotations

import psycopg

from src.core import settings
from src.core.env import env as default_env

from .constants import DEFAULT_BIND, MAX_PORT
from .models import ApiConfig


def load_config_from_env(env=default_env) -> ApiConfig:  # noqa: ANN001
    dsn = str(env.postgres_dsn).strip()
    token = str(env.api_token).strip()
    bind = str(env.api_bind).strip() or DEFAULT_BIND
    port = int(env.api_port)
    cors_origins = tuple(env.cors_origins)
    cors_allow_credentials = bool(env.api_cors_allow_credentials)

    if not dsn:
        msg = 'POSTGRES_DSN is required'
        raise ValueError(msg)
    if not token:
        msg = 'API_TOKEN is required'
        raise ValueError(msg)
    if not (0 < port <= MAX_PORT):
        msg = f'API_PORT must be between 1 and {MAX_PORT}'
        raise ValueError(msg)

    return ApiConfig(
        dsn=dsn,
        token=token,
        bind=bind,
        port=port,
        cors_origins=cors_origins,
        cors_allow_credentials=cors_allow_credentials,
    )


def fetch_hanime1_downloaded_ids_from_db(dsn: str) -> list[str]:
    with psycopg.connect(dsn) as conn, conn.cursor() as cursor:
        cursor.execute('SELECT id FROM hanime1 ORDER BY id;')
        rows = cursor.fetchall()

    normalized_ids: list[str] = []
    seen_ids: set[str] = set()
    for row in rows:
        if not row:
            continue
        item_id = str(row[0]).strip()
        if not item_id:
            continue
        lowered_item_id = item_id.casefold()
        if lowered_item_id in seen_ids:
            continue
        seen_ids.add(lowered_item_id)
        normalized_ids.append(item_id)
    return normalized_ids


def fetch_hanime1_videos_from_db(dsn: str, *, host: str | None = None) -> list[dict[str, str | None]]:
    resolved_host = (host or settings.load().web.hanime1.host).rstrip('/')
    with psycopg.connect(dsn) as conn, conn.cursor() as cursor:
        cursor.execute('SELECT id, title, uploader, release_date, plot FROM hanime1 ORDER BY id;')
        rows = cursor.fetchall()

    videos: list[dict[str, str | None]] = []
    seen_ids: set[str] = set()
    for row in rows:
        if not row:
            continue

        video_id = str(row[0]).strip()
        if not video_id:
            continue

        lowered_video_id = video_id.casefold()
        if lowered_video_id in seen_ids:
            continue
        seen_ids.add(lowered_video_id)

        title = str(row[1] or '')
        uploader = str(row[2] or '').strip() or None
        release_date = str(row[3] or '').strip() or None
        plot = str(row[4] or '').strip() or None
        videos.append(
            {
                'video_id': video_id,
                'title': title,
                'downloaded': True,
                'uploader': uploader,
                'release_date': release_date,
                'plot': plot,
                'watch_url': f'{resolved_host}/watch?v={video_id}',
            },
        )
    return videos
