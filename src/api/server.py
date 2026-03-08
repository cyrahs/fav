from __future__ import annotations

import logging
from http.server import ThreadingHTTPServer

from src.core.config import config as app_config

from .config import fetch_hanime1_downloaded_ids_from_db, load_config_from_settings as _load_config_from_settings
from .http import FavApiRequestHandler, build_handler
from .models import ApiConfig, ApiResponse
from .service import FavApiService

log = logging.getLogger('fav-api')


def load_config_from_settings() -> ApiConfig:
    return _load_config_from_settings(app_config)


def main() -> None:
    logging.basicConfig(level=logging.INFO, format='%(asctime)s %(levelname)s %(name)s: %(message)s')
    config = load_config_from_settings()
    service = FavApiService(dsn=config.dsn, token=config.token)
    server = ThreadingHTTPServer((config.bind, config.port), build_handler(service))
    log.info('Starting fav API on %s:%d', config.bind, config.port)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        log.info('Stopping fav API')
    finally:
        server.server_close()
        service.close()


__all__ = [
    'ApiConfig',
    'ApiResponse',
    'FavApiRequestHandler',
    'FavApiService',
    'app_config',
    'build_handler',
    'fetch_hanime1_downloaded_ids_from_db',
    'load_config_from_settings',
    'main',
]
