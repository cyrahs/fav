from __future__ import annotations

import hashlib
from collections.abc import Mapping
from datetime import UTC, datetime

from src.tool.control_queue import ControlRequest
from src.tool.notifications import NotificationRecord
from src.tool.runtime_config import RuntimeSeriesSeed


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


def _utc_now_iso_z(now_provider) -> str:  # noqa: ANN001
    return now_provider().astimezone(UTC).replace(microsecond=0).isoformat().replace('+00:00', 'Z')


def _serialize_datetime(value: datetime | None) -> str | None:
    if value is None:
        return None
    return value.astimezone(UTC).replace(microsecond=0).isoformat().replace('+00:00', 'Z')


def _serialize_control_request(request: ControlRequest) -> dict[str, object]:
    return {
        'request_id': request.request_id,
        'kind': request.kind,
        'target': request.target,
        'status': request.status,
        'requested_at': _serialize_datetime(request.requested_at),
        'started_at': _serialize_datetime(request.started_at),
        'finished_at': _serialize_datetime(request.finished_at),
        'result': request.result,
        'error': request.error,
    }


def _serialize_seed(seed: RuntimeSeriesSeed) -> dict[str, str]:
    return {
        'video_id': seed.video_id,
        'title': seed.title,
        'label': seed.label,
    }


def _serialize_notification(notification: NotificationRecord) -> dict[str, object]:
    return {
        'id': notification.notification_id,
        'kind': notification.kind,
        'source': notification.source,
        'title': notification.title,
        'body': notification.body,
        'link_url': notification.link_url,
        'image_url': notification.image_url,
        'payload': notification.payload_json,
        'status': notification.status,
        'created_at': _serialize_datetime(notification.created_at),
        'read_at': _serialize_datetime(notification.read_at),
    }
