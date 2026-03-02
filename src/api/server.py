from __future__ import annotations

import hashlib
import json
import logging
import secrets
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from datetime import UTC, datetime
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

import psycopg

from src.core.config import config as app_config

log = logging.getLogger('fav-api')

_ENDPOINT_HANIME1_DOWNLOADED_IDS = '/api/v1/hanime1/downloaded-ids'
_ENDPOINT_HEALTH = '/api/v1/health'
_CACHE_CONTROL = 'private, max-age=60'
_CONTENT_TYPE_JSON = 'application/json; charset=utf-8'
_AUTH_PREFIX = 'Bearer '
_DEFAULT_BIND = '127.0.0.1'
_DEFAULT_PORT = 8091
_HEADER_AUTHORIZATION = 'authorization'
_HEADER_IF_NONE_MATCH = 'if-none-match'
_HEADER_CONTENT_TYPE = 'Content-Type'
_HEADER_CACHE_CONTROL = 'Cache-Control'
_HEADER_WWW_AUTHENTICATE = 'WWW-Authenticate'
_HEADER_ETAG = 'ETag'
_HEADER_ALLOW = 'Allow'
_HEADER_CONTENT_LENGTH = 'Content-Length'
_MAX_PORT = 65535

type Hanime1IdFetcher = Callable[[str], list[str]]
type DatetimeProvider = Callable[[], datetime]


@dataclass(frozen=True, slots=True)
class ApiConfig:
    dsn: str
    token: str
    bind: str = _DEFAULT_BIND
    port: int = _DEFAULT_PORT


@dataclass(frozen=True, slots=True)
class ApiResponse:
    status: HTTPStatus
    headers: dict[str, str]
    body: bytes | None


def _header_value(headers: Mapping[str, str], name: str) -> str | None:
    lowered_name = name.casefold()
    for key, value in headers.items():
        if key.casefold() != lowered_name:
            continue
        return value
    return None


def _normalize_ids(raw_ids: list[str]) -> list[str]:
    normalized: list[str] = []
    seen: set[str] = set()
    for raw in raw_ids:
        item_id = str(raw or '').strip()
        if not item_id:
            continue
        key = item_id.casefold()
        if key in seen:
            continue
        seen.add(key)
        normalized.append(item_id)
    return normalized


def _build_etag(ids: list[str]) -> str:
    digest = hashlib.sha256('\n'.join(ids).encode('utf-8')).hexdigest()
    return f'"{digest}"'


def _strip_weak_etag(raw_etag: str) -> str:
    if raw_etag.startswith('W/'):
        return raw_etag[2:]
    return raw_etag


def _etag_matches(if_none_match: str | None, current_etag: str) -> bool:
    if not if_none_match:
        return False
    candidate_values = [item.strip() for item in if_none_match.split(',') if item.strip()]
    if not candidate_values:
        return False
    if '*' in candidate_values:
        return True
    current = _strip_weak_etag(current_etag)
    return any(_strip_weak_etag(candidate) == current for candidate in candidate_values)


def _utc_now_iso_z(now_provider: DatetimeProvider) -> str:
    return now_provider().astimezone(UTC).replace(microsecond=0).isoformat().replace('+00:00', 'Z')


def load_config_from_settings() -> ApiConfig:
    dsn = str(app_config.database.postgres_dsn).strip()
    token = str(app_config.api.token).strip()
    bind = str(app_config.api.bind).strip() or _DEFAULT_BIND
    port = int(app_config.api.port)

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


class FavApiService:
    def __init__(
        self,
        *,
        dsn: str,
        token: str,
        hanime1_fetcher: Hanime1IdFetcher | None = None,
        now_provider: DatetimeProvider | None = None,
    ) -> None:
        self._dsn = dsn
        self._token = token
        self._hanime1_fetcher = hanime1_fetcher or fetch_hanime1_downloaded_ids_from_db
        self._now_provider = now_provider or (lambda: datetime.now(UTC))

    @staticmethod
    def _json_response(
        *,
        status: HTTPStatus,
        payload: Mapping[str, object],
        headers: Mapping[str, str] | None = None,
    ) -> ApiResponse:
        body = json.dumps(payload, ensure_ascii=False, separators=(',', ':')).encode('utf-8')
        merged_headers = {
            _HEADER_CONTENT_TYPE: _CONTENT_TYPE_JSON,
            _HEADER_CACHE_CONTROL: _CACHE_CONTROL,
        }
        if headers:
            merged_headers.update(headers)
        return ApiResponse(status=status, headers=merged_headers, body=body)

    def _authenticate(self, headers: Mapping[str, str]) -> ApiResponse | None:
        authorization = _header_value(headers, _HEADER_AUTHORIZATION)
        if not authorization:
            return self._json_response(
                status=HTTPStatus.UNAUTHORIZED,
                payload={'error': 'missing_authorization'},
                headers={_HEADER_WWW_AUTHENTICATE: 'Bearer realm="fav-api"'},
            )
        if not authorization.startswith(_AUTH_PREFIX):
            return self._json_response(
                status=HTTPStatus.UNAUTHORIZED,
                payload={'error': 'invalid_authorization_scheme'},
                headers={_HEADER_WWW_AUTHENTICATE: 'Bearer realm="fav-api"'},
            )
        token = authorization[len(_AUTH_PREFIX) :].strip()
        if not token:
            return self._json_response(
                status=HTTPStatus.UNAUTHORIZED,
                payload={'error': 'missing_bearer_token'},
                headers={_HEADER_WWW_AUTHENTICATE: 'Bearer realm="fav-api"'},
            )
        if not secrets.compare_digest(token, self._token):
            return self._json_response(
                status=HTTPStatus.FORBIDDEN,
                payload={'error': 'invalid_token'},
            )
        return None

    def _handle_health(self) -> ApiResponse:
        return self._json_response(
            status=HTTPStatus.OK,
            payload={'status': 'ok', 'generated_at': _utc_now_iso_z(self._now_provider)},
        )

    def _handle_hanime1_downloaded_ids(self, headers: Mapping[str, str]) -> ApiResponse:
        auth_error = self._authenticate(headers)
        if auth_error is not None:
            return auth_error

        try:
            ids = _normalize_ids(self._hanime1_fetcher(self._dsn))
        except Exception:
            log.exception('Failed to query hanime1 downloaded ids')
            return self._json_response(
                status=HTTPStatus.INTERNAL_SERVER_ERROR,
                payload={'error': 'internal_server_error'},
            )

        etag = _build_etag(ids)
        if_none_match = _header_value(headers, _HEADER_IF_NONE_MATCH)
        if _etag_matches(if_none_match, etag):
            return ApiResponse(
                status=HTTPStatus.NOT_MODIFIED,
                headers={
                    _HEADER_CACHE_CONTROL: _CACHE_CONTROL,
                    _HEADER_ETAG: etag,
                },
                body=None,
            )

        return self._json_response(
            status=HTTPStatus.OK,
            payload={
                'ids': ids,
                'count': len(ids),
                'generated_at': _utc_now_iso_z(self._now_provider),
            },
            headers={_HEADER_ETAG: etag},
        )

    def handle_request(
        self,
        *,
        method: str,
        path: str,
        headers: Mapping[str, str],
    ) -> ApiResponse:
        request_path = urlsplit(path).path

        if request_path == _ENDPOINT_HEALTH:
            if method != 'GET':
                return self._json_response(
                    status=HTTPStatus.METHOD_NOT_ALLOWED,
                    payload={'error': 'method_not_allowed'},
                    headers={_HEADER_ALLOW: 'GET'},
                )
            return self._handle_health()

        if request_path == _ENDPOINT_HANIME1_DOWNLOADED_IDS:
            if method != 'GET':
                return self._json_response(
                    status=HTTPStatus.METHOD_NOT_ALLOWED,
                    payload={'error': 'method_not_allowed'},
                    headers={_HEADER_ALLOW: 'GET'},
                )
            return self._handle_hanime1_downloaded_ids(headers)

        return self._json_response(
            status=HTTPStatus.NOT_FOUND,
            payload={'error': 'not_found'},
        )


class FavApiRequestHandler(BaseHTTPRequestHandler):
    service: FavApiService
    protocol_version = 'HTTP/1.1'
    server_version = 'FavAPI/1.0'

    def _send(self) -> None:
        response = self.service.handle_request(
            method=self.command,
            path=self.path,
            headers=dict(self.headers.items()),
        )
        body = response.body or b''
        self.send_response(response.status.value)
        for key, value in response.headers.items():
            self.send_header(key, value)
        self.send_header(_HEADER_CONTENT_LENGTH, str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_DELETE(self) -> None:
        self._send()

    def do_GET(self) -> None:
        self._send()

    def do_PATCH(self) -> None:
        self._send()

    def do_POST(self) -> None:
        self._send()

    def do_PUT(self) -> None:
        self._send()

    def log_message(self, fmt: str, *args: object) -> None:
        log.info('%s - - %s', self.address_string(), fmt % args)


def build_handler(service: FavApiService) -> type[FavApiRequestHandler]:
    class _BoundHandler(FavApiRequestHandler):
        pass

    _BoundHandler.service = service
    return _BoundHandler


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
