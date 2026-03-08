from __future__ import annotations

import json
import logging
import secrets
from collections.abc import Callable, Mapping
from datetime import UTC, datetime
from http import HTTPStatus
from urllib.parse import parse_qs, urlsplit

from src.core.config import config as app_config
from src.service.jobs import ScheduledJob, build_jobs
from src.tool.control_queue import ControlRequest, create_control_request_sync, get_control_request_sync
from src.tool.notifications import NotificationRecord, ack_notifications_sync, list_notifications_sync
from src.tool.runtime_config import Hanime1RuntimeConfigService

from .config import fetch_hanime1_downloaded_ids_from_db
from .constants import (
    _AUTH_PREFIX,
    _CACHE_CONTROL,
    _CONTENT_TYPE_JSON,
    _ENDPOINT_CONTROL_JOBS,
    _ENDPOINT_CONTROL_REQUESTS,
    _ENDPOINT_HEALTH,
    _ENDPOINT_NOTIFICATIONS,
    _ENDPOINT_NOTIFICATIONS_ACK,
    _HEADER_ALLOW,
    _HEADER_AUTHORIZATION,
    _HEADER_CACHE_CONTROL,
    _HEADER_CONTENT_TYPE,
    _HEADER_ETAG,
    _HEADER_IF_NONE_MATCH,
    _HEADER_WWW_AUTHENTICATE,
)
from .hanime1 import Hanime1ApiResource, Hanime1IdFetcher
from .helpers import (
    _header_value,
    _serialize_control_request,
    _serialize_notification,
    _utc_now_iso_z,
)
from .models import ApiResponse

log = logging.getLogger('fav-api')

type DatetimeProvider = Callable[[], datetime]
type JobProvider = Callable[[], list[ScheduledJob]]
type ControlRequestCreator = Callable[[str, str], ControlRequest]
type ControlRequestGetter = Callable[[int], ControlRequest | None]
type NotificationLister = Callable[[str, int, int | None], list[NotificationRecord]]
type NotificationAcker = Callable[[list[int]], int]


class FavApiService:
    def __init__(
        self,
        *,
        dsn: str,
        token: str,
        hanime1_fetcher: Hanime1IdFetcher | None = None,
        now_provider: DatetimeProvider | None = None,
        job_provider: JobProvider | None = None,
        control_request_creator: ControlRequestCreator | None = None,
        control_request_getter: ControlRequestGetter | None = None,
        runtime_service: Hanime1RuntimeConfigService | None = None,
        notification_lister: NotificationLister | None = None,
        notification_acker: NotificationAcker | None = None,
    ) -> None:
        self._dsn = dsn
        self._token = token
        self._hanime1_fetcher = hanime1_fetcher or fetch_hanime1_downloaded_ids_from_db
        self._now_provider = now_provider or (lambda: datetime.now(UTC))
        self._job_provider = job_provider or build_jobs
        self._control_request_creator = control_request_creator or (
            lambda kind, target: create_control_request_sync(self._dsn, kind=kind, target=target)
        )
        self._control_request_getter = control_request_getter or (lambda request_id: get_control_request_sync(self._dsn, request_id))
        self._notification_lister = notification_lister or (
            lambda status, limit, after_id: list_notifications_sync(self._dsn, status=status, limit=limit, after_id=after_id)
        )
        self._notification_acker = notification_acker or (lambda ids: ack_notifications_sync(self._dsn, ids))
        self._runtime_service = runtime_service or Hanime1RuntimeConfigService(
            run_config=app_config.run_config,
            host=app_config.web.hanime1.host,
            user_lang=app_config.web.hanime1.user_lang,
            proxy=app_config.proxy or None,
        )
        self._hanime1_api = Hanime1ApiResource(
            dsn=self._dsn,
            id_fetcher=self._hanime1_fetcher,
            now_provider=self._now_provider,
            runtime_service=self._runtime_service,
            authenticate=self._authenticate,
            decode_json_body=self._decode_json_body,
            json_response=self._json_response,
        )

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

    def close(self) -> None:
        close = getattr(self._runtime_service, 'close', None)
        if callable(close):
            close()

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
        provided_token = authorization[len(_AUTH_PREFIX) :].strip()
        if not provided_token:
            return self._json_response(
                status=HTTPStatus.UNAUTHORIZED,
                payload={'error': 'missing_bearer_token'},
                headers={_HEADER_WWW_AUTHENTICATE: 'Bearer realm="fav-api"'},
            )
        if not secrets.compare_digest(provided_token, self._token):
            return self._json_response(
                status=HTTPStatus.FORBIDDEN,
                payload={'error': 'invalid_token'},
            )
        return None

    def _decode_json_body(self, body: bytes | None) -> tuple[dict[str, object] | None, ApiResponse | None]:
        if not body:
            return None, self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'missing_json_body'})
        try:
            payload = json.loads(body.decode('utf-8'))
        except (UnicodeDecodeError, json.JSONDecodeError):
            return None, self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_json'})
        if not isinstance(payload, dict):
            return None, self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_json'})
        return payload, None

    def _handle_health(self) -> ApiResponse:
        return self._json_response(
            status=HTTPStatus.OK,
            payload={'status': 'ok', 'generated_at': _utc_now_iso_z(self._now_provider)},
        )

    def _handle_control_jobs(self, headers: Mapping[str, str]) -> ApiResponse:
        auth_error = self._authenticate(headers)
        if auth_error is not None:
            return auth_error
        jobs = [job.as_public_dict() for job in self._job_provider()]
        return self._json_response(status=HTTPStatus.OK, payload={'jobs': jobs, 'count': len(jobs)})

    def _handle_create_control_request(self, headers: Mapping[str, str], body: bytes | None) -> ApiResponse:
        auth_error = self._authenticate(headers)
        if auth_error is not None:
            return auth_error

        payload, error_response = self._decode_json_body(body)
        if error_response is not None:
            return error_response
        assert payload is not None

        kind = str(payload.get('kind') or '').strip()
        target = str(payload.get('target') or '').strip().lower()
        if not kind:
            return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'missing_kind'})
        if kind != 'trigger_job':
            return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_kind'})
        if not target:
            return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'missing_target'})

        valid_targets = {job.key for job in self._job_provider()}
        valid_targets.add('all')
        if target not in valid_targets:
            return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_target'})

        try:
            request = self._control_request_creator(kind, target)
        except Exception:
            log.exception('Failed to create control request kind=%s target=%s', kind, target)
            return self._json_response(status=HTTPStatus.INTERNAL_SERVER_ERROR, payload={'error': 'internal_server_error'})
        return self._json_response(status=HTTPStatus.ACCEPTED, payload=_serialize_control_request(request))

    def _handle_get_control_request(self, headers: Mapping[str, str], request_id: str) -> ApiResponse:
        auth_error = self._authenticate(headers)
        if auth_error is not None:
            return auth_error
        if not request_id.isdecimal():
            return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_request_id'})

        request = self._control_request_getter(int(request_id))
        if request is None:
            return self._json_response(status=HTTPStatus.NOT_FOUND, payload={'error': 'not_found'})
        return self._json_response(status=HTTPStatus.OK, payload=_serialize_control_request(request))

    def _handle_list_notifications(self, headers: Mapping[str, str], query_params: Mapping[str, list[str]]) -> ApiResponse:
        auth_error = self._authenticate(headers)
        if auth_error is not None:
            return auth_error

        status = str(query_params.get('status', ['unread'])[0] or 'unread').strip().lower()
        if status not in {'unread', 'all'}:
            return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_status'})

        raw_limit = str(query_params.get('limit', ['50'])[0] or '50').strip()
        if not raw_limit.isdecimal():
            return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_limit'})
        limit = int(raw_limit)
        if not (0 < limit <= 200):
            return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_limit'})

        after_id: int | None = None
        raw_after_id = str(query_params.get('after_id', [''])[0] or '').strip()
        if raw_after_id:
            if not raw_after_id.isdecimal():
                return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_after_id'})
            after_id = int(raw_after_id)

        try:
            notifications = self._notification_lister(status, limit, after_id)
        except Exception:
            log.exception('Failed to list notifications')
            return self._json_response(status=HTTPStatus.INTERNAL_SERVER_ERROR, payload={'error': 'internal_server_error'})

        payload = [_serialize_notification(notification) for notification in notifications]
        return self._json_response(status=HTTPStatus.OK, payload={'notifications': payload, 'count': len(payload)})

    def _handle_ack_notifications(self, headers: Mapping[str, str], body: bytes | None) -> ApiResponse:
        auth_error = self._authenticate(headers)
        if auth_error is not None:
            return auth_error

        payload, error_response = self._decode_json_body(body)
        if error_response is not None:
            return error_response
        assert payload is not None

        ids = payload.get('ids')
        if not isinstance(ids, list):
            return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_ids'})

        normalized_ids: list[int] = []
        seen_ids: set[int] = set()
        for raw_id in ids:
            if isinstance(raw_id, bool) or not isinstance(raw_id, int) or raw_id <= 0:
                return self._json_response(status=HTTPStatus.BAD_REQUEST, payload={'error': 'invalid_ids'})
            if raw_id in seen_ids:
                continue
            seen_ids.add(raw_id)
            normalized_ids.append(raw_id)

        try:
            updated = self._notification_acker(normalized_ids)
        except Exception:
            log.exception('Failed to ack notifications')
            return self._json_response(status=HTTPStatus.INTERNAL_SERVER_ERROR, payload={'error': 'internal_server_error'})
        return self._json_response(status=HTTPStatus.OK, payload={'updated': updated})

    def handle_request(
        self,
        *,
        method: str,
        path: str,
        headers: Mapping[str, str],
        body: bytes | None = None,
    ) -> ApiResponse:
        request_parts = urlsplit(path)
        request_path = request_parts.path.rstrip('/') or '/'
        query_params = parse_qs(request_parts.query, keep_blank_values=True)

        if request_path == _ENDPOINT_HEALTH:
            if method != 'GET':
                return self._json_response(
                    status=HTTPStatus.METHOD_NOT_ALLOWED,
                    payload={'error': 'method_not_allowed'},
                    headers={_HEADER_ALLOW: 'GET'},
                )
            return self._handle_health()

        hanime1_response = self._hanime1_api.handle_request(method=method, path=request_path, headers=headers, body=body)
        if hanime1_response is not None:
            return hanime1_response

        if request_path == _ENDPOINT_CONTROL_JOBS:
            if method != 'GET':
                return self._json_response(
                    status=HTTPStatus.METHOD_NOT_ALLOWED,
                    payload={'error': 'method_not_allowed'},
                    headers={_HEADER_ALLOW: 'GET'},
                )
            return self._handle_control_jobs(headers)

        if request_path == _ENDPOINT_CONTROL_REQUESTS:
            if method != 'POST':
                return self._json_response(
                    status=HTTPStatus.METHOD_NOT_ALLOWED,
                    payload={'error': 'method_not_allowed'},
                    headers={_HEADER_ALLOW: 'POST'},
                )
            return self._handle_create_control_request(headers, body)

        if request_path.startswith(f'{_ENDPOINT_CONTROL_REQUESTS}/'):
            if method != 'GET':
                return self._json_response(
                    status=HTTPStatus.METHOD_NOT_ALLOWED,
                    payload={'error': 'method_not_allowed'},
                    headers={_HEADER_ALLOW: 'GET'},
                )
            request_id = request_path.removeprefix(f'{_ENDPOINT_CONTROL_REQUESTS}/')
            return self._handle_get_control_request(headers, request_id)

        if request_path == _ENDPOINT_NOTIFICATIONS:
            if method != 'GET':
                return self._json_response(
                    status=HTTPStatus.METHOD_NOT_ALLOWED,
                    payload={'error': 'method_not_allowed'},
                    headers={_HEADER_ALLOW: 'GET'},
                )
            return self._handle_list_notifications(headers, query_params)

        if request_path == _ENDPOINT_NOTIFICATIONS_ACK:
            if method != 'POST':
                return self._json_response(
                    status=HTTPStatus.METHOD_NOT_ALLOWED,
                    payload={'error': 'method_not_allowed'},
                    headers={_HEADER_ALLOW: 'POST'},
                )
            return self._handle_ack_notifications(headers, body)

        return self._json_response(
            status=HTTPStatus.NOT_FOUND,
            payload={'error': 'not_found'},
        )
