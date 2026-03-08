from __future__ import annotations

import logging
from collections.abc import Callable, Mapping
from datetime import datetime
from http import HTTPStatus

from src.tool.runtime_config import Hanime1RuntimeConfigService

from .constants import (
    _CACHE_CONTROL,
    _ENDPOINT_CONTROL_HANIME1_SEEDS,
    _ENDPOINT_RUNTIME_HANIME1_DOWNLOADED_IDS,
    _HEADER_ALLOW,
    _HEADER_ETAG,
    _HEADER_IF_NONE_MATCH,
)
from .helpers import (
    _build_etag,
    _etag_matches,
    _header_value,
    _normalize_ids,
    _serialize_seed,
    _utc_now_iso_z,
)
from .models import ApiResponse

log = logging.getLogger('fav-api')

type DatetimeProvider = Callable[[], datetime]
type Hanime1IdFetcher = Callable[[str], list[str]]
type Authenticator = Callable[[Mapping[str, str]], ApiResponse | None]
type JsonDecoder = Callable[[bytes | None], tuple[dict[str, object] | None, ApiResponse | None]]


class Hanime1ApiResource:
    def __init__(
        self,
        *,
        dsn: str,
        id_fetcher: Hanime1IdFetcher,
        now_provider: DatetimeProvider,
        runtime_service: Hanime1RuntimeConfigService,
        authenticate: Authenticator,
        decode_json_body: JsonDecoder,
        json_response,  # noqa: ANN001
    ) -> None:
        self._dsn = dsn
        self._id_fetcher = id_fetcher
        self._now_provider = now_provider
        self._runtime_service = runtime_service
        self._authenticate = authenticate
        self._decode_json_body = decode_json_body
        self._json_response = json_response

    def _handle_downloaded_ids(self, headers: Mapping[str, str]) -> ApiResponse:
        auth_error = self._authenticate(headers)
        if auth_error is not None:
            return auth_error

        try:
            ids = _normalize_ids(self._id_fetcher(self._dsn))
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
                    _HEADER_ETAG: etag,
                    'Cache-Control': _CACHE_CONTROL,
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

    def _handle_list_seeds(self, headers: Mapping[str, str]) -> ApiResponse:
        auth_error = self._authenticate(headers)
        if auth_error is not None:
            return auth_error
        seeds = self._runtime_service.list_seeds()
        serialized = [_serialize_seed(seed) for seed in seeds]
        return self._json_response(status=HTTPStatus.OK, payload={'seeds': serialized, 'count': len(serialized)})

    def _handle_add_seed(self, headers: Mapping[str, str], body: bytes | None) -> ApiResponse:
        auth_error = self._authenticate(headers)
        if auth_error is not None:
            return auth_error

        payload, error_response = self._decode_json_body(body)
        if error_response is not None:
            return error_response
        assert payload is not None

        seed = payload.get('seed')
        if not isinstance(seed, str) or not seed.strip():
            return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'missing_seed'})

        try:
            created_seed = self._runtime_service.add_seed(seed)
        except ValueError:
            return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_seed'})
        except FileExistsError:
            return self._json_response(status=HTTPStatus.CONFLICT, payload={'error': 'duplicate_seed'})
        except LookupError:
            return self._json_response(status=HTTPStatus.UNPROCESSABLE_ENTITY, payload={'error': 'seed_resolve_failed'})
        except Exception:
            log.exception('Failed to add Hanime1 runtime seed')
            return self._json_response(status=HTTPStatus.INTERNAL_SERVER_ERROR, payload={'error': 'internal_server_error'})

        return self._json_response(status=HTTPStatus.CREATED, payload=_serialize_seed(created_seed))

    def _handle_delete_seed(self, headers: Mapping[str, str], video_id: str) -> ApiResponse:
        auth_error = self._authenticate(headers)
        if auth_error is not None:
            return auth_error
        if not video_id.isdecimal():
            return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_seed_id'})

        try:
            removed_seed = self._runtime_service.delete_seed(video_id)
        except Exception:
            log.exception('Failed to delete Hanime1 runtime seed %s', video_id)
            return self._json_response(status=HTTPStatus.INTERNAL_SERVER_ERROR, payload={'error': 'internal_server_error'})
        if removed_seed is None:
            return self._json_response(status=HTTPStatus.NOT_FOUND, payload={'error': 'not_found'})
        return self._json_response(status=HTTPStatus.OK, payload=_serialize_seed(removed_seed))

    def handle_request(
        self,
        *,
        method: str,
        path: str,
        headers: Mapping[str, str],
        body: bytes | None = None,
    ) -> ApiResponse | None:
        if path == _ENDPOINT_RUNTIME_HANIME1_DOWNLOADED_IDS:
            if method != 'GET':
                return self._json_response(
                    status=HTTPStatus.METHOD_NOT_ALLOWED,
                    payload={'error': 'method_not_allowed'},
                    headers={_HEADER_ALLOW: 'GET'},
                )
            return self._handle_downloaded_ids(headers)

        if path == _ENDPOINT_CONTROL_HANIME1_SEEDS:
            if method == 'GET':
                return self._handle_list_seeds(headers)
            if method == 'POST':
                return self._handle_add_seed(headers, body)
            return self._json_response(
                status=HTTPStatus.METHOD_NOT_ALLOWED,
                payload={'error': 'method_not_allowed'},
                headers={_HEADER_ALLOW: 'GET, POST'},
            )

        if path.startswith(f'{_ENDPOINT_CONTROL_HANIME1_SEEDS}/'):
            if method != 'DELETE':
                return self._json_response(
                    status=HTTPStatus.METHOD_NOT_ALLOWED,
                    payload={'error': 'method_not_allowed'},
                    headers={_HEADER_ALLOW: 'DELETE'},
                )
            video_id = path.removeprefix(f'{_ENDPOINT_CONTROL_HANIME1_SEEDS}/')
            return self._handle_delete_seed(headers, video_id)

        return None
