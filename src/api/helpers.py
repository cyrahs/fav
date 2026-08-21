from __future__ import annotations

from datetime import UTC, datetime
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from src.service.jobs import ScheduledJob
    from src.tool.control_queue import ControlRequest
    from src.tool.runtime_config import RuntimeSeriesSeed


def utc_now_iso_z(now_provider) -> str:  # noqa: ANN001
    return now_provider().astimezone(UTC).replace(microsecond=0).isoformat().replace('+00:00', 'Z')


def serialize_datetime(value: datetime | None) -> str | None:
    if value is None:
        return None
    return value.astimezone(UTC).replace(microsecond=0).isoformat().replace('+00:00', 'Z')


def serialize_job(job: ScheduledJob) -> dict[str, object]:
    return {
        'key': job.key,
        'name': job.name,
        'enabled': job.enabled,
        'notify': job.notify,
        'cron': job.cron,
        'section': job.section,
        'missing_fields': list(job.missing_fields),
    }


def serialize_control_request(request: ControlRequest) -> dict[str, object]:
    return {
        'id': request.request_id,
        'target': request.target,
        'kind': request.kind,
        'status': request.status,
        'requested_at': serialize_datetime(request.requested_at),
        'started_at': serialize_datetime(request.started_at),
        'finished_at': serialize_datetime(request.finished_at),
        'result': request.result,
        'error': request.error,
    }


def serialize_seed(seed: RuntimeSeriesSeed) -> dict[str, str]:
    return {
        'video_id': seed.video_id,
        'title': seed.title,
        'label': seed.label,
    }
