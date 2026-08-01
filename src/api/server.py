from __future__ import annotations

import uvicorn

from src.core import logger
from src.core.env import env

from .app import create_app
from .config import (
    fetch_hanime1_downloaded_ids_from_db,
    fetch_hanime1_videos_from_db,
)
from .config import (
    load_config_from_env as _load_config_from_env,
)
from .models import ApiConfig
from .service import FavApiService

log = logger.get('fav-api')


def load_config_from_env() -> ApiConfig:
    return _load_config_from_env(env)


def main() -> None:
    config = load_config_from_env()
    log.info('Starting fav API on %s:%d', config.bind, config.port)
    uvicorn.run(create_app(config=config), host=config.bind, port=config.port, log_level='info')


__all__ = [
    'ApiConfig',
    'FavApiService',
    'create_app',
    'env',
    'fetch_hanime1_downloaded_ids_from_db',
    'fetch_hanime1_videos_from_db',
    'load_config_from_env',
    'main',
]
