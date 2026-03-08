from __future__ import annotations

import psycopg

from src.core.config import config as default_app_config

from .constants import _DEFAULT_BIND, _MAX_PORT
from .helpers import _normalize_ids
from .models import ApiConfig


def load_config_from_settings(settings=default_app_config) -> ApiConfig:  # noqa: ANN001
    dsn = str(settings.database.postgres_dsn).strip()
    token = str(settings.api.token).strip()
    bind = str(settings.api.bind).strip() or _DEFAULT_BIND
    port = int(settings.api.port)

    if not dsn:
        msg = 'database.postgres_dsn is required'
        raise ValueError(msg)
    if not token:
        msg = 'api.token is required'
        raise ValueError(msg)
    if not (0 < port <= _MAX_PORT):
        msg = f'api.port must be between 1 and {_MAX_PORT}'
        raise ValueError(msg)

    return ApiConfig(dsn=dsn, token=token, bind=bind, port=port)


def fetch_hanime1_downloaded_ids_from_db(dsn: str) -> list[str]:
    with psycopg.connect(dsn) as conn, conn.cursor() as cursor:
        cursor.execute('SELECT id FROM hanime1 ORDER BY id;')
        rows = cursor.fetchall()
    return _normalize_ids([str(row[0]) for row in rows if row])
