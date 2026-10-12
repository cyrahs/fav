"""Runtime state and repair operations behind the MCP tools.

The web UI never needed these: they answer the questions a developer asks while
debugging a deployment (which commit is running, what the last runs of a job
did, what the logs say, where the WeChat source stands) and perform the small
repairs that used to need a shell in the pod (retrying failed items, moving a
cursor). Every query is a fixed statement; there is deliberately no way to run
arbitrary SQL from here.
"""

from __future__ import annotations

import heapq
import json
import os
import re
from collections import deque
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta, tzinfo
from functools import reduce
from pathlib import Path
from typing import TYPE_CHECKING, Any
from urllib.parse import urlsplit
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

from apscheduler.triggers.cron import CronTrigger

from src.core import settings
from src.core.env import env
from src.core.settings import SECTION_MODELS
from src.tool import database
from src.tool.control_queue import VALID_KINDS, VALID_STATUSES, ensure_control_requests_table

from .helpers import serialize_datetime
from .settings_masking import MASK_SUFFIX, mask_section

if TYPE_CHECKING:
    from collections.abc import Callable, Iterable, Iterator

    from src.service.jobs import ScheduledJob

API_STARTED_AT = datetime.now(UTC)

MAX_LIMIT = 500
MAX_LOG_RECORDS = 1000
_TEXT_LIMIT = 4000
_LOG_MESSAGE_LIMIT = 4000
_MIN_SECRET_LENGTH = 6
_LOG_LINE_RE = re.compile(r'^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}),(\d{3}) - (.+?) - ([A-Z]+) - (.*)$')
_LOG_LEVELS = {'DEBUG': 10, 'INFO': 20, 'NOTICE': 25, 'WARNING': 30, 'ERROR': 40, 'CRITICAL': 50}
_WORKER_COMMAND = 'run.py'

WECHAT_EVENT_STATUSES = ('pending', 'done', 'failed')
WECHAT_STATE_KEYS = ('events_after', 'work_version')
NOTIFICATION_DELIVERY_STATUSES = ('pending', 'sending', 'delivered', 'failed')


class OpsError(ValueError):
    """A request the tools reject; the message is shown to the caller as is."""


def bounded(limit: int, maximum: int = MAX_LIMIT) -> int:
    return max(1, min(int(limit), maximum))


def truncate(text: str, limit: int = _TEXT_LIMIT) -> str:
    if len(text) <= limit:
        return text
    return f'{text[:limit]}… [{len(text) - limit} more chars]'


def parse_since(value: str | None) -> datetime | None:
    """Accept an ISO 8601 timestamp or a relative ``<n>m``/``<n>h``/``<n>d``."""
    if value is None or not value.strip():
        return None
    text = value.strip()
    match = re.fullmatch(r'(\d+)\s*([mhd])', text)
    if match:
        amount = int(match.group(1))
        unit = {'m': 'minutes', 'h': 'hours', 'd': 'days'}[match.group(2)]
        return datetime.now(UTC) - timedelta(**{unit: amount})
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        msg = f'Unrecognised time {value!r}: use ISO 8601 (2026-10-12T17:00:00Z) or a relative 30m/6h/2d.'
        raise OpsError(msg) from None
    return parsed if parsed.tzinfo else parsed.replace(tzinfo=UTC)


def local_timezone() -> tzinfo:
    """The zone the worker schedules in and the log files are stamped in (``TZ``)."""
    name = os.getenv('TZ', 'UTC').strip() or 'UTC'
    try:
        return ZoneInfo(name)
    except ZoneInfoNotFoundError:
        return UTC


# ---------- secrets ----------


def _walk_secrets(raw: Any, masked: Any) -> Iterator[str]:
    if isinstance(raw, dict) and isinstance(masked, dict):
        for key, value in raw.items():
            yield from _walk_secrets(value, masked.get(key))
    elif isinstance(raw, list) and isinstance(masked, list):
        for raw_item, masked_item in zip(raw, masked, strict=False):
            yield from _walk_secrets(raw_item, masked_item)
    elif isinstance(raw, str) and raw != masked:
        yield raw


def collect_secrets() -> list[str]:
    """Values the settings UI masks, plus the bootstrap credentials from the environment."""
    snapshot = settings.load()
    found: set[str] = set()
    for section in SECTION_MODELS:
        raw = json.loads(reduce(getattr, section.split('.'), snapshot).model_dump_json())
        found.update(_walk_secrets(raw, mask_section(section, raw)))
    found.add(str(env.api_token or ''))
    password = urlsplit(str(env.postgres_dsn or '')).password
    if password:
        found.add(password)
    return sorted((value for value in found if len(value) >= _MIN_SECRET_LENGTH), key=len, reverse=True)


class Redactor:
    def __init__(self, secrets: Iterable[str]) -> None:
        self._secrets = [secret for secret in secrets if len(secret) >= _MIN_SECRET_LENGTH]

    def __call__(self, text: str) -> str:
        for secret in self._secrets:
            if secret in text:
                text = text.replace(secret, f'{secret[:4]}{MASK_SUFFIX}')
        return text


# ---------- runtime ----------


def _boot_time() -> float | None:
    try:
        for line in Path('/proc/stat').read_text().splitlines():
            if line.startswith('btime '):
                return float(line.split()[1])
    except OSError:
        return None
    return None


def process_started_at(pid: int) -> datetime | None:
    boot = _boot_time()
    if boot is None:
        return None
    try:
        stat = Path(f'/proc/{pid}/stat').read_text()
    except OSError:
        return None
    # Field 22 (starttime) counts clock ticks since boot; the command name in
    # field 2 may contain spaces, so split after its closing parenthesis.
    fields = stat.rsplit(')', 1)[-1].split()
    try:
        ticks = int(fields[19])
    except (IndexError, ValueError):
        return None
    return datetime.fromtimestamp(boot + ticks / os.sysconf('SC_CLK_TCK'), UTC)


def find_worker_pid() -> int | None:
    for entry in Path('/proc').iterdir():
        if not entry.name.isdigit():
            continue
        try:
            argv = (entry / 'cmdline').read_bytes().split(b'\0')
        except OSError:
            continue
        if any(arg.decode(errors='replace').endswith(_WORKER_COMMAND) for arg in argv[1:]):
            return int(entry.name)
    return None


def runtime_info() -> dict[str, Any]:
    worker_pid = find_worker_pid()
    return {
        'git_commit': os.getenv('FAV_GIT_COMMIT', '') or None,
        'build_time': os.getenv('FAV_BUILD_TIME', '') or None,
        'pod': os.getenv('HOSTNAME', '') or None,
        'container_started_at': serialize_datetime(process_started_at(1)),
        'api_started_at': serialize_datetime(API_STARTED_AT),
        'worker_running': worker_pid is not None,
        'worker_started_at': serialize_datetime(process_started_at(worker_pid)) if worker_pid else None,
        'timezone': getattr(local_timezone(), 'key', 'UTC'),
        'now': serialize_datetime(datetime.now(UTC)),
    }


def next_fire_time(cron: str, *, now: datetime | None = None) -> str | None:
    try:
        trigger = CronTrigger.from_crontab(cron, timezone=local_timezone())
    except ValueError:
        return None
    return serialize_datetime(trigger.get_next_fire_time(None, now or datetime.now(UTC)))


# ---------- logs ----------


@dataclass(slots=True)
class LogRecord:
    time: datetime
    logger: str
    level: str
    message: str

    def as_dict(self) -> dict[str, str]:
        return {
            'time': serialize_datetime(self.time) or '',
            'logger': self.logger,
            'level': self.level,
            'message': truncate(self.message, _LOG_MESSAGE_LIMIT),
        }


def _iter_log_records(path: Path, zone: tzinfo) -> Iterator[LogRecord]:
    current: LogRecord | None = None
    with path.open(encoding='utf-8', errors='replace') as handle:
        for raw_line in handle:
            line = raw_line.rstrip('\n')
            match = _LOG_LINE_RE.match(line)
            if match is None:
                # Tracebacks and other multi-line messages continue the record above.
                if current is not None:
                    current.message += f'\n{line}'
                continue
            if current is not None:
                yield current
            stamp = datetime.strptime(match.group(1), '%Y-%m-%d %H:%M:%S').replace(
                microsecond=int(match.group(2)) * 1000,
                tzinfo=zone,
            )
            current = LogRecord(time=stamp.astimezone(UTC), logger=match.group(3), level=match.group(4), message=match.group(5))
    if current is not None:
        yield current


def read_logs(  # noqa: PLR0913
    log_dir: Path,
    *,
    since: datetime | None,
    until: datetime | None = None,
    contains: str = '',
    logger_name: str = '',
    min_level: str = 'DEBUG',
    limit: int = 200,
    redact: Callable[[str], str] = str,
) -> dict[str, Any]:
    """Return the newest ``limit`` records in the window, oldest first.

    A process keeps writing to the file named after the day it started, so a file
    name says nothing about what it covers; the modification time does.
    """
    level_floor = _LOG_LEVELS.get(min_level.strip().upper())
    if level_floor is None:
        msg = f'Unknown level {min_level!r}; use one of {", ".join(_LOG_LEVELS)}.'
        raise OpsError(msg)
    zone = local_timezone()
    needle = contains.casefold()
    logger_needle = logger_name.casefold()
    capacity = bounded(limit, MAX_LOG_RECORDS)
    candidates = sorted(log_dir.glob('*.log')) if log_dir.is_dir() else []
    files = [path for path in candidates if since is None or datetime.fromtimestamp(path.stat().st_mtime, UTC) >= since]

    matched: deque[LogRecord] = deque(maxlen=capacity)
    total = 0
    # Each file is chronological, so merging streams keeps memory to the matches kept.
    records = heapq.merge(*(_iter_log_records(path, zone) for path in files), key=lambda item: item.time)
    for record in records:
        if since is not None and record.time < since:
            continue
        if until is not None and record.time > until:
            continue
        if _LOG_LEVELS.get(record.level, 0) < level_floor:
            continue
        if logger_needle and logger_needle not in record.logger.casefold():
            continue
        if needle and needle not in record.message.casefold():
            continue
        total += 1
        matched.append(record)

    entries = []
    for record in matched:
        entry = record.as_dict()
        entry['message'] = redact(entry['message'])
        entries.append(entry)
    return {
        'files': [path.name for path in files],
        'matched': total,
        'returned': len(entries),
        'truncated': total > len(entries),
        'records': entries,
    }


# ---------- control requests ----------

_RUN_FIELDS = 'id, kind, target, status, requested_at, started_at, finished_at, result, error'


def _serialize_run(row: dict[str, Any], redact: Callable[[str], str], *, full: bool) -> dict[str, Any]:
    started = row.get('started_at')
    finished = row.get('finished_at')
    error = redact(str(row.get('error') or ''))
    result = redact(str(row.get('result') or ''))
    return {
        'id': int(row['id']),
        'job': row['target'],
        'kind': row['kind'],
        'status': row['status'],
        'requested_at': serialize_datetime(row.get('requested_at')),
        'started_at': serialize_datetime(started),
        'finished_at': serialize_datetime(finished),
        'duration_seconds': round((finished - started).total_seconds(), 1) if started and finished else None,
        'result': result if full else truncate(result, 500),
        'error': error if full else truncate(error),
    }


async def list_runs(  # noqa: PLR0913
    *,
    job: str = '',
    status: str = '',
    kind: str = '',
    since: datetime | None = None,
    limit: int = 20,
    redact: Callable[[str], str] = str,
) -> list[dict[str, Any]]:
    clauses: list[str] = []
    params: list[Any] = []
    if job.strip():
        clauses.append('target = ?')
        params.append(job.strip().lower())
    if status.strip():
        if status not in VALID_STATUSES:
            msg = f'Unknown status {status!r}; use one of {", ".join(sorted(VALID_STATUSES))}.'
            raise OpsError(msg)
        clauses.append('status = ?')
        params.append(status)
    if kind.strip():
        if kind not in VALID_KINDS:
            msg = f'Unknown kind {kind!r}; use one of {", ".join(sorted(VALID_KINDS))}.'
            raise OpsError(msg)
        clauses.append('kind = ?')
        params.append(kind)
    if since is not None:
        clauses.append('requested_at >= ?')
        params.append(since)
    where = f'WHERE {" AND ".join(clauses)}' if clauses else ''
    await ensure_control_requests_table()
    rows = await database.query_db(
        f'SELECT {_RUN_FIELDS} FROM control_requests {where} ORDER BY requested_at DESC, id DESC LIMIT ?;',  # noqa: S608 - fixed clauses
        (*params, bounded(limit)),
    )
    return [_serialize_run(row, redact, full=False) for row in rows]


async def get_run(run_id: int, *, redact: Callable[[str], str] = str) -> dict[str, Any] | None:
    await ensure_control_requests_table()
    rows = await database.query_db(f'SELECT {_RUN_FIELDS} FROM control_requests WHERE id = ?;', (int(run_id),))  # noqa: S608
    return _serialize_run(rows[0], redact, full=True) if rows else None


async def last_runs_by_job() -> dict[str, dict[str, Any]]:
    await ensure_control_requests_table()
    rows = await database.query_db(
        f"""
        SELECT DISTINCT ON (target) {_RUN_FIELDS}
        FROM control_requests
        ORDER BY target, requested_at DESC, id DESC;
        """,  # noqa: S608
    )
    return {str(row['target']): _serialize_run(row, str, full=False) for row in rows}


def describe_jobs(jobs: list[ScheduledJob], last_runs: dict[str, dict[str, Any]]) -> list[dict[str, Any]]:
    described = []
    for job in jobs:
        last = last_runs.get(job.key)
        described.append(
            {
                **job.as_public_dict(),
                'required_commands': list(job.required_commands),
                'next_run_at': next_fire_time(job.cron) if job.enabled else None,
                'last_run': ({key: last[key] for key in ('id', 'kind', 'status', 'started_at', 'finished_at', 'error')} if last else None),
            },
        )
    return described


# ---------- WeChat ----------


def _event_summary(row: dict[str, Any], *, include_message: bool, redact: Callable[[str], str]) -> dict[str, Any]:
    message = row.get('message')
    if isinstance(message, str):
        try:
            message = json.loads(message)
        except json.JSONDecodeError:
            message = {}
    message = message if isinstance(message, dict) else {}
    summary: dict[str, Any] = {
        'msg_id': int(row['msg_id']),
        'create_time': int(row['create_time']),
        'work': row['work'],
        'status': row['status'],
        'attempts': int(row['attempts']),
        'last_error': redact(str(row.get('last_error') or '')),
        'seq': int(row.get('seq') or 0),
        'kind': message.get('kind'),
        'talker': message.get('talker'),
        'sender': message.get('sender'),
        'updated_at': serialize_datetime(row.get('updated_at')),
    }
    if include_message:
        summary['message'] = message
    return summary


async def wechat_status(*, redact: Callable[[str], str] = str, recent_failures: int = 10) -> dict[str, Any]:
    state_rows = await database.query_db('SELECT key, value FROM wechat_state ORDER BY key;')
    count_rows = await database.query_db(
        'SELECT work, status, COUNT(*) AS count FROM wechat_events GROUP BY work, status ORDER BY work, status;',
    )
    failure_rows = await database.query_db(
        """
        SELECT msg_id, create_time, work, seq, message, status, attempts, last_error, updated_at
        FROM wechat_events
        WHERE status = 'failed' OR (status = 'pending' AND attempts > 0)
        ORDER BY updated_at DESC
        LIMIT ?;
        """,
        (bounded(recent_failures),),
    )
    return {
        'state': {str(row['key']): str(row['value']) for row in state_rows},
        'event_counts': [{'work': row['work'], 'status': row['status'], 'count': int(row['count'])} for row in count_rows],
        'recent_failures': [_event_summary(row, include_message=False, redact=redact) for row in failure_rows],
    }


async def list_wechat_events(  # noqa: PLR0913
    *,
    status: str = '',
    work: str = '',
    msg_ids: list[int] | None = None,
    include_message: bool = False,
    limit: int = 20,
    redact: Callable[[str], str] = str,
) -> list[dict[str, Any]]:
    clauses: list[str] = []
    params: list[Any] = []
    if status:
        if status not in WECHAT_EVENT_STATUSES:
            msg = f'Unknown status {status!r}; use one of {", ".join(WECHAT_EVENT_STATUSES)}.'
            raise OpsError(msg)
        clauses.append('status = ?')
        params.append(status)
    if work:
        clauses.append('work = ?')
        params.append(work)
    if msg_ids:
        clauses.append('msg_id = ANY(?)')
        params.append([int(value) for value in msg_ids])
    where = f'WHERE {" AND ".join(clauses)}' if clauses else ''
    rows = await database.query_db(
        f"""
        SELECT msg_id, create_time, work, seq, message, status, attempts, last_error, updated_at
        FROM wechat_events {where}
        ORDER BY create_time DESC, msg_id DESC
        LIMIT ?;
        """,  # noqa: S608 - fixed clauses
        (*params, bounded(limit)),
    )
    return [_event_summary(row, include_message=include_message, redact=redact) for row in rows]


async def retry_wechat_events(*, msg_ids: list[int] | None = None, status: str = 'failed') -> list[int]:
    """Queue events again with a fresh attempt budget; the next WeChat run picks them up."""
    if status not in WECHAT_EVENT_STATUSES:
        msg = f'Unknown status {status!r}; use one of {", ".join(WECHAT_EVENT_STATUSES)}.'
        raise OpsError(msg)
    if status == 'done' and not msg_ids:
        msg = 'Retrying every done event would redo the whole log; name the msg_ids instead.'
        raise OpsError(msg)
    params: list[Any] = [status]
    id_clause = ''
    if msg_ids:
        id_clause = ' AND msg_id = ANY(?)'
        params.append([int(value) for value in msg_ids])
    rows = await database.query_db(
        f"""
        UPDATE wechat_events
        SET status = 'pending', attempts = 0, last_error = '', updated_at = CURRENT_TIMESTAMP
        WHERE status = ?{id_clause}
        RETURNING msg_id;
        """,  # noqa: S608 - fixed clauses
        tuple(params),
    )
    return sorted(int(row['msg_id']) for row in rows)


async def set_wechat_state(key: str, value: str) -> dict[str, str | None]:
    if key not in WECHAT_STATE_KEYS:
        msg = f'Unknown key {key!r}; use one of {", ".join(WECHAT_STATE_KEYS)}.'
        raise OpsError(msg)
    if key == 'events_after' and not value.strip().isdigit():
        msg = 'events_after is a bridge sequence number (a non-negative integer).'
        raise OpsError(msg)
    previous = await database.query_db('SELECT value FROM wechat_state WHERE key = ?;', (key,))
    await database.query_db(
        'INSERT INTO wechat_state (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value;',
        (key, value.strip()),
    )
    return {'key': key, 'previous': str(previous[0]['value']) if previous else None, 'value': value.strip()}


# ---------- notifications ----------


async def list_notifications(
    *,
    delivery_status: str = '',
    source: str = '',
    limit: int = 20,
    redact: Callable[[str], str] = str,
) -> list[dict[str, Any]]:
    clauses: list[str] = []
    params: list[Any] = []
    if delivery_status:
        if delivery_status not in NOTIFICATION_DELIVERY_STATUSES:
            msg = f'Unknown delivery_status {delivery_status!r}; use one of {", ".join(NOTIFICATION_DELIVERY_STATUSES)}.'
            raise OpsError(msg)
        clauses.append('delivery_status = ?')
        params.append(delivery_status)
    if source:
        clauses.append('source = ?')
        params.append(source)
    where = f'WHERE {" AND ".join(clauses)}' if clauses else ''
    rows = await database.query_db(
        f"""
        SELECT id, kind, source, scope, dedupe_key, title, body, delivery_status, attempt_count, occurrence_count,
               last_error, created_at, next_attempt_at, delivered_at
        FROM notifications {where}
        ORDER BY id DESC
        LIMIT ?;
        """,  # noqa: S608 - fixed clauses
        (*params, bounded(limit)),
    )
    return [
        {
            'id': int(row['id']),
            'kind': row['kind'],
            'source': row['source'],
            'scope': row['scope'],
            'dedupe_key': row['dedupe_key'],
            'title': row['title'],
            'body': truncate(redact(str(row['body'] or '')), 1000),
            'delivery_status': row['delivery_status'],
            'attempt_count': int(row['attempt_count']),
            'occurrence_count': int(row['occurrence_count']),
            'last_error': redact(str(row['last_error'] or '')),
            'created_at': serialize_datetime(row['created_at']),
            'next_attempt_at': serialize_datetime(row['next_attempt_at']),
            'delivered_at': serialize_datetime(row['delivered_at']),
        }
        for row in rows
    ]


async def retry_notifications(ids: list[int]) -> list[int]:
    """Send failed notifications again from a fresh attempt budget."""
    if not ids:
        msg = 'Name the notification ids to retry.'
        raise OpsError(msg)
    rows = await database.query_db(
        """
        UPDATE notifications
        SET delivery_status = 'pending', attempt_count = 0, last_error = '', next_attempt_at = CURRENT_TIMESTAMP
        WHERE id = ANY(?) AND delivery_status = 'failed'
        RETURNING id;
        """,
        ([int(value) for value in ids],),
    )
    return sorted(int(row['id']) for row in rows)
