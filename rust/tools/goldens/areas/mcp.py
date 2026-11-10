"""The MCP endpoint of `src/api/mcp_server.py` and the queries of `src/api/ops.py` (WP4).

`protocol.json` replays HTTP exchanges through the real FastAPI app (`create_app`) and so
through FastMCP's own streamable-HTTP transport: `initialize` and its version
negotiation, `tools/list` with every tool's name, description, input and output schema,
the transport's header checks and error envelopes, and the argument validation and error
texts of `tools/call`.

`calls.json` replays tool calls over stored state. The API service is WP3's real
`FavApiService` over the storage fakes of `areas/api.py`; the `ops` queries go to a fake
`database.query_db` that answers each fixed statement of `ops.py` from the scenario's
rows (it refuses any statement it does not know, so a changed query cannot pass
unnoticed), and records the statements and parameters Python sent. The clock is frozen
at `FIXED_NOW`, the bootstrap secrets (`API_TOKEN`, the DSN password) are the scenario's,
and the log files are written to a temporary directory from the scenario. Each scenario
names that state (`setup`), so the Rust replay can seed a real database with the same
rows; a field the database fills in itself is listed under `volatile`. `after` holds the
rows a writing tool left behind, and `logged` the `fav-mcp` log lines each call wrote.

`ops.json` calls the pure helpers directly: `parse_since`, `truncate`, `bounded`, the
redactor, `_merge`, and `read_logs` over files with fixed modification times.
"""

from __future__ import annotations

import contextlib
import json
import logging
import os
import tempfile
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from types import SimpleNamespace
from typing import TYPE_CHECKING, Any

from fastapi.testclient import TestClient
from src.api import ops
from src.api.app import create_app
from src.api.browser_login import BrowserLoginManager, login_sites
from src.api.mcp_server import _merge
from src.api.models import ApiConfig
from src.core import logger as logger_module
from src.tool import control_queue
from src.tool import database as database_module

from areas import api as api_area

if TYPE_CHECKING:
    from collections.abc import Iterator

TOKEN = api_area.TOKEN
FIXED_NOW = datetime(2026, 3, 2, 12, 0, tzinfo=UTC)
DSN = 'postgresql://fav:dsn-password-42@postgresql.media.svc:5432/fav'
JSON_HEADERS = {'accept': 'application/json, text/event-stream', 'content-type': 'application/json'}
_RESPONSE_HEADERS = ('content-type', 'allow', 'location', 'mcp-session-id')


# ---------- frozen clock ----------


class _FrozenDatetime(datetime):
    """`datetime` with `now()` pinned to `FIXED_NOW`, for `ops` (`parse_since`, `next_fire_time`)."""

    @classmethod
    def now(cls, tz: Any = None) -> datetime:
        moment = cls.fromtimestamp(FIXED_NOW.timestamp(), UTC)
        return moment.astimezone(tz) if tz is not None else moment.replace(tzinfo=None)


# ---------- the fake database behind ops ----------


@dataclass
class _Tables:
    requests: list[Any] = field(default_factory=list)
    wechat_events: list[dict[str, Any]] = field(default_factory=list)
    wechat_state: dict[str, str] = field(default_factory=dict)
    notifications: list[dict[str, Any]] = field(default_factory=list)


def _norm(query: str) -> str:
    return ' '.join(query.split())


def _run_row(request: Any) -> dict[str, Any]:
    return {
        'id': request.request_id,
        'kind': request.kind,
        'target': request.target,
        'status': request.status,
        'requested_at': request.requested_at,
        'started_at': request.started_at,
        'finished_at': request.finished_at,
        'result': request.result,
        'error': request.error,
    }


def _newest_runs(rows: list[Any]) -> list[Any]:
    return sorted(rows, key=lambda item: (item.requested_at, item.request_id), reverse=True)


class _FakeDb:
    """Answers the fixed statements of `ops.py` from the scenario's rows."""

    def __init__(self, tables: _Tables) -> None:
        self.tables = tables
        self.calls: list[dict[str, Any]] = []

    async def query_db(self, query: str, params: tuple[Any, ...] = ()) -> list[dict[str, Any]]:
        sql = _norm(query)
        self.calls.append({'sql': sql, 'params': [_plain(value) for value in params]})
        return self._answer(sql, list(params))

    def _answer(self, sql: str, params: list[Any]) -> list[dict[str, Any]]:  # noqa: C901, PLR0911, PLR0912, PLR0915 - one branch per fixed statement
        tables = self.tables
        runs = 'SELECT id, kind, target, status, requested_at, started_at, finished_at, result, error FROM control_requests'
        if sql.startswith(f'{runs} WHERE id = ?'):
            return [_run_row(item) for item in tables.requests if item.request_id == params[0]]
        if sql.startswith(runs) and sql.endswith('ORDER BY requested_at DESC, id DESC LIMIT ?;'):
            *values, limit = params
            rows = list(tables.requests)
            clauses = sql.removeprefix(runs).split('ORDER BY')[0].strip().removeprefix('WHERE ').split(' AND ')
            for clause in [item for item in clauses if item]:
                value = values.pop(0)
                if clause == 'target = ?':
                    rows = [item for item in rows if item.target == value]
                elif clause == 'status = ?':
                    rows = [item for item in rows if item.status == value]
                elif clause == 'kind = ?':
                    rows = [item for item in rows if item.kind == value]
                elif clause == 'requested_at >= ?':
                    rows = [item for item in rows if item.requested_at >= value]
                else:
                    raise AssertionError(clause)
            return [_run_row(item) for item in _newest_runs(rows)[:limit]]
        if sql.startswith('SELECT DISTINCT ON (target)'):
            latest: dict[str, Any] = {}
            for item in sorted(tables.requests, key=lambda row: (row.target, -row.requested_at.timestamp(), -row.request_id)):
                latest.setdefault(item.target, item)
            return [_run_row(item) for _, item in sorted(latest.items())]
        if sql == 'SELECT key, value FROM wechat_state ORDER BY key;':
            return [{'key': key, 'value': value} for key, value in sorted(tables.wechat_state.items())]
        if sql == 'SELECT value FROM wechat_state WHERE key = ?;':
            return [{'value': tables.wechat_state[params[0]]}] if params[0] in tables.wechat_state else []
        if sql.startswith('INSERT INTO wechat_state'):
            tables.wechat_state[params[0]] = params[1]
            return []
        if sql.startswith('SELECT work, status, COUNT(*) AS count FROM wechat_events'):
            counts: dict[tuple[str, str], int] = {}
            for row in tables.wechat_events:
                counts[row['work'], row['status']] = counts.get((row['work'], row['status']), 0) + 1
            return [{'work': work, 'status': status, 'count': count} for (work, status), count in sorted(counts.items())]
        events = 'SELECT msg_id, create_time, work, seq, message, status, attempts, last_error, updated_at FROM wechat_events'
        if sql.startswith(f"{events} WHERE status = 'failed' OR (status = 'pending' AND attempts > 0)"):
            rows = [
                row for row in tables.wechat_events if row['status'] == 'failed' or (row['status'] == 'pending' and row['attempts'] > 0)
            ]
            rows.sort(key=lambda row: row['updated_at'], reverse=True)
            return [dict(row) for row in rows[: params[0]]]
        if sql.startswith(events) and sql.endswith('ORDER BY create_time DESC, msg_id DESC LIMIT ?;'):
            *values, limit = params
            rows = list(tables.wechat_events)
            clauses = sql.removeprefix(events).split('ORDER BY')[0].strip().removeprefix('WHERE ').split(' AND ')
            for clause in [item for item in clauses if item]:
                value = values.pop(0)
                if clause == 'status = ?':
                    rows = [row for row in rows if row['status'] == value]
                elif clause == 'work = ?':
                    rows = [row for row in rows if row['work'] == value]
                elif clause == 'msg_id = ANY(?)':
                    rows = [row for row in rows if row['msg_id'] in value]
                else:
                    raise AssertionError(clause)
            rows.sort(key=lambda row: (row['create_time'], row['msg_id']), reverse=True)
            return [dict(row) for row in rows[:limit]]
        if sql.startswith("UPDATE wechat_events SET status = 'pending', attempts = 0, last_error = '', updated_at = CURRENT_TIMESTAMP"):
            status, *rest = params
            changed = []
            for row in tables.wechat_events:
                if row['status'] == status and (not rest or row['msg_id'] in rest[0]):
                    row.update(status='pending', attempts=0, last_error='', updated_at=FIXED_NOW)
                    changed.append({'msg_id': row['msg_id']})
            return changed
        notifications = (
            'SELECT id, kind, source, scope, dedupe_key, title, body, delivery_status, attempt_count, occurrence_count, '
            'last_error, created_at, next_attempt_at, delivered_at FROM notifications'
        )
        if sql.startswith(notifications) and sql.endswith('ORDER BY id DESC LIMIT ?;'):
            *values, limit = params
            rows = list(tables.notifications)
            clauses = sql.removeprefix(notifications).split('ORDER BY')[0].strip().removeprefix('WHERE ').split(' AND ')
            for clause in [item for item in clauses if item]:
                value = values.pop(0)
                if clause == 'delivery_status = ?':
                    rows = [row for row in rows if row['delivery_status'] == value]
                elif clause == 'source = ?':
                    rows = [row for row in rows if row['source'] == value]
                else:
                    raise AssertionError(clause)
            rows.sort(key=lambda row: row['id'], reverse=True)
            return [dict(row) for row in rows[:limit]]
        if sql.startswith("UPDATE notifications SET delivery_status = 'pending', attempt_count = 0"):
            changed = []
            for row in tables.notifications:
                if row['id'] in params[0] and row['delivery_status'] == 'failed':
                    row.update(delivery_status='pending', attempt_count=0, last_error='', next_attempt_at=FIXED_NOW)
                    changed.append({'id': row['id']})
            return changed
        msg = f'ops sent a statement the fake does not know: {sql}'
        raise AssertionError(msg)


def _plain(value: Any) -> Any:
    if isinstance(value, datetime):
        return value.isoformat()
    if isinstance(value, list):
        return [_plain(item) for item in value]
    return value


def _parse(value: str | None) -> datetime | None:
    return datetime.fromisoformat(value) if value else None


def _jsonb(value: Any) -> Any:
    """What psycopg returns for a `jsonb` value: PostgreSQL stores object keys shortest first, then bytewise."""
    if isinstance(value, dict):
        return {key: _jsonb(value[key]) for key in sorted(value, key=lambda key: (len(key.encode()), key.encode()))}
    if isinstance(value, list):
        return [_jsonb(item) for item in value]
    return value


def _event_row(row: dict[str, Any]) -> dict[str, Any]:
    return {
        'msg_id': row['msg_id'],
        'create_time': row['create_time'],
        'work': row['work'],
        'seq': row.get('seq', 0),
        'message': _jsonb(row.get('message', {})),
        'status': row.get('status', 'pending'),
        'attempts': row.get('attempts', 0),
        'last_error': row.get('last_error', ''),
        'updated_at': _parse(row['updated_at']),
    }


def _notification_row(row: dict[str, Any]) -> dict[str, Any]:
    return {
        'id': row['id'],
        'kind': row['kind'],
        'source': row['source'],
        'scope': row.get('scope', 'item'),
        'dedupe_key': row.get('dedupe_key', ''),
        'title': row.get('title', ''),
        'body': row.get('body', ''),
        'delivery_status': row.get('delivery_status', 'pending'),
        'attempt_count': row.get('attempt_count', 0),
        'occurrence_count': row.get('occurrence_count', 1),
        'last_error': row.get('last_error', ''),
        'created_at': _parse(row['created_at']),
        'next_attempt_at': _parse(row.get('next_attempt_at')),
        'delivered_at': _parse(row.get('delivered_at')),
    }


def _dump_row(row: dict[str, Any]) -> dict[str, Any]:
    return {key: _plain(value) for key, value in row.items()}


# ---------- the app ----------


# fav's own MCP lines and the SDK's warnings with a fixed text (DIVERGENCES.md D38).
_LOGGERS = ['embyx.fav-mcp', 'mcp.server.transport_security', 'mcp.server.lowlevel.server']


class _Captured(logging.Handler):
    def __init__(self) -> None:
        super().__init__(logging.DEBUG)
        self.lines: list[str] = []

    def emit(self, record: logging.LogRecord) -> None:
        if record.name.startswith('mcp.') and record.levelno != logging.WARNING:
            return  # "Received exception from stream: ..." shows an object's address (D38).
        self.lines.append(f'{record.name} {record.levelname} {record.getMessage()}')


@contextlib.contextmanager
def _patched_ops(tables: _Tables, state: Any, log_dir: Path, env: SimpleNamespace) -> Iterator[_FakeDb]:
    fake = _FakeDb(tables)
    saved = (
        database_module.query_db,
        control_queue._table_ready,  # noqa: SLF001
        ops.site_sessions,
        ops.datetime,
        ops.env,
        logger_module.log_dir,
    )
    database_module.query_db = fake.query_db
    control_queue._table_ready = True  # noqa: SLF001
    ops.site_sessions = api_area._Sessions(state)  # noqa: SLF001
    ops.datetime = _FrozenDatetime
    ops.env = env
    logger_module.log_dir = log_dir
    try:
        yield fake
    finally:
        (
            database_module.query_db,
            control_queue._table_ready,  # noqa: SLF001
            ops.site_sessions,
            ops.datetime,
            ops.env,
            logger_module.log_dir,
        ) = saved


def _write_logs(directory: Path, files: list[dict[str, Any]]) -> None:
    for entry in files:
        path = directory / entry['name']
        path.write_text(entry['text'], encoding='utf-8')
        mtime = datetime.fromisoformat(entry['mtime']).timestamp()
        os.utime(path, (mtime, mtime))


@contextlib.contextmanager
def _client(setup: dict[str, Any]) -> Iterator[tuple[TestClient, _FakeDb, _Tables, Any, _Captured]]:
    state = api_area._State(  # noqa: SLF001
        rows={section: json.loads(json.dumps(value)) for section, value in setup.get('settings', {}).items()},
        requests=[api_area._request_row(row) for row in setup.get('control_requests', [])],  # noqa: SLF001
        sessions={row['site']: api_area._session_row(row) for row in setup.get('site_sessions', [])},  # noqa: SLF001
    )
    tables = _Tables(
        requests=state.requests,
        wechat_events=[_event_row(row) for row in setup.get('wechat_events', [])],
        wechat_state=dict(setup.get('wechat_state', {})),
        notifications=[_notification_row(row) for row in setup.get('notifications', [])],
    )
    env = SimpleNamespace(api_token=TOKEN, postgres_dsn=setup.get('dsn', DSN))
    config = ApiConfig(
        dsn=env.postgres_dsn,
        token=TOKEN,
        cors_origins=(),
        cors_allow_credentials=False,
        credential_clients='',
    )
    captured = _Captured()
    loggers = [logging.getLogger(name) for name in _LOGGERS]
    with tempfile.TemporaryDirectory() as raw_dir, api_area._patched(state):  # noqa: SLF001
        log_dir = Path(raw_dir)
        _write_logs(log_dir, setup.get('logs', []))
        with _patched_ops(tables, state, log_dir, env) as fake:
            service = api_area._service(state)  # noqa: SLF001
            app = create_app(config=config, service=service, browser_login=BrowserLoginManager(login_sites))
            for logger in loggers:
                logger.addHandler(captured)
            try:
                with TestClient(app, raise_server_exceptions=True, follow_redirects=False) as client:
                    yield client, fake, tables, state, captured
            finally:
                for logger in loggers:
                    logger.removeHandler(captured)


def _response(response: Any) -> dict[str, Any]:
    headers = {name: response.headers[name] for name in _RESPONSE_HEADERS if name in response.headers}
    result: dict[str, Any] = {'status': response.status_code, 'headers': headers}
    if not response.content:
        result['empty'] = True
    elif response.headers.get('content-type', '').startswith('application/json'):
        result['json'] = response.json()
        result['bytes'] = response.text
    else:
        result['text'] = response.text
    return result


# ---------- protocol ----------


def _post(body: Any, *, headers: dict[str, str] | None = None, path: str = '/mcp') -> dict[str, Any]:
    raw = body if isinstance(body, str) else json.dumps(body, ensure_ascii=False)
    return {'method': 'POST', 'path': path, 'headers': JSON_HEADERS if headers is None else headers, 'body': raw}


def _rpc(method: str, params: Any = None, *, rpc_id: Any = 1, headers: dict[str, str] | None = None) -> dict[str, Any]:
    message: dict[str, Any] = {'jsonrpc': '2.0', 'id': rpc_id, 'method': method}
    if params is not None:
        message['params'] = params
    return _post(message, headers=headers)


def _call(name: str, arguments: Any = None, **kwargs: Any) -> dict[str, Any]:
    params: dict[str, Any] = {'name': name}
    if arguments is not None:
        params['arguments'] = arguments
    return _rpc('tools/call', params, **kwargs)


def _init(version: Any = '2025-06-18') -> dict[str, Any]:
    params: dict[str, Any] = {'capabilities': {}, 'clientInfo': {'name': 'litellm', 'version': '1.0'}}
    if version is not None:
        params['protocolVersion'] = version
    return _rpc('initialize', params)


def _protocol_requests() -> list[dict[str, Any]]:
    long_text = 'x' * 80
    return [
        _init(),
        _init('2024-11-05'),
        _init('2025-03-26'),
        _init('2025-11-25'),
        _init('2099-01-01'),
        _init(None),
        _rpc('notifications/initialized'),
        _post({'jsonrpc': '2.0', 'method': 'notifications/initialized'}),
        _post({'jsonrpc': '2.0', 'method': 'notifications/cancelled', 'params': {'requestId': 1}}),
        _post({'jsonrpc': '2.0', 'id': 5, 'result': {}}),
        _rpc('tools/list'),
        _rpc('tools/list', {}, rpc_id='list-1'),
        _rpc('tools/list', headers={**JSON_HEADERS, 'mcp-protocol-version': '2025-06-18'}),
        _rpc('tools/list', headers={**JSON_HEADERS, 'mcp-protocol-version': '1999-01-01'}),
        _rpc('ping'),
        _rpc('resources/list'),
        _rpc('resources/templates/list'),
        _rpc('prompts/list'),
        _rpc('resources/read', {'uri': 'file:///x'}),
        _rpc('prompts/get', {'name': 'x'}),
        _rpc('logging/setLevel', {'level': 'info'}),
        _rpc('completion/complete', {'ref': {'type': 'ref/prompt', 'name': 'x'}, 'argument': {'name': 'a', 'value': 'b'}}),
        _rpc('no/such/method'),
        _post('{"jsonrpc": "2.0", "id": 1, "method": "ping"'),
        _post(''),
        _post({'jsonrpc': '2.0'}),
        _post([{'jsonrpc': '2.0', 'id': 1, 'method': 'ping'}]),
        _post({'jsonrpc': '1.0', 'id': 1, 'method': 'ping'}),
        _rpc('ping', headers={'content-type': 'application/json'}),
        _rpc('ping', headers={'accept': 'text/event-stream', 'content-type': 'application/json'}),
        _rpc('ping', headers={'accept': 'application/json', 'content-type': 'application/json'}),
        _rpc('ping', headers={'accept': '*/*', 'content-type': 'application/json'}),
        _rpc('ping', headers={'accept': 'application/json', 'content-type': 'text/plain'}),
        _rpc('ping', headers={'accept': 'application/json', 'content-type': 'application/json; charset=utf-8'}),
        _rpc('ping', headers={'accept': 'application/json'}),
        _rpc('ping', rpc_id=None),
        _rpc('ping', rpc_id=1.5),
        _rpc('ping', rpc_id='abc'),
        _rpc('ping', rpc_id=True),
        _rpc('ping', rpc_id=12345678901234567890),
        # JSONRPCMessage: each member's errors, in member order.
        _post({'jsonrpc': '2.0', 'id': True, 'method': 5}),
        _post({'jsonrpc': '2.0', 'method': 'x', 'params': [1]}),
        _post({'jsonrpc': '2.0', 'id': None, 'error': 'x'}),
        _post({'jsonrpc': '2.0', 'id': 'a', 'error': {'code': 'x', 'message': 5}}),
        _post({'jsonrpc': '2.0', 'id': 1, 'error': {'code': -1, 'message': 'm'}}),
        _post({'jsonrpc': '2.0', 'id': 1.0, 'error': {'code': '-1', 'message': 'm', 'data': [1]}}),
        _post({'jsonrpc': 2, 'id': {}, 'result': []}),
        _post({'jsonrpc': '2.0', 'id': long_text, 'method': 'ping', 'params': {'note': long_text}}),
        _post({'jsonrpc': '2.0', 'id': 'é' * 30, 'error': 5}),
        _post('"str"'),
        _post('5'),
        _post('null'),
        _post({}),
        _post('{} {}'),
        _post('{"a": "\\x"}'),
        _post('{"a": "\u0001"}'),
        _post('{"a" 1}'),
        _post('{1: 2}'),
        _post('{"a": "b'),
        _post('\n\n  [1,\n 2'),
        _post('\ufeff{"jsonrpc": "2.0", "id": 1, "method": "ping"}'),
        # ClientRequest: parameters each method checks.
        _rpc('initialize', {'protocolVersion': True, 'capabilities': {}, 'clientInfo': {'name': 'a', 'version': '1'}}),
        _rpc('initialize', {'protocolVersion': 1.5, 'capabilities': {}, 'clientInfo': {'name': 'a', 'version': '1'}}),
        _rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {'roots': 5}, 'clientInfo': {'name': 'a', 'version': '1'}}),
        _rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': [], 'clientInfo': {'name': 'a', 'version': '1'}}),
        _rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'a'}}),
        _rpc(
            'initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'a', 'version': '1'}}, rpc_id='init'
        ),
        _rpc('ping', {'_meta': 5}),
        _rpc('ping', {'_meta': {'progressToken': 1.5}}),
        _rpc('ping', {'_meta': {'progressToken': 'p'}, 'extra': 1}),
        _rpc('ping', {'task': {'ttl': '5'}}),
        _rpc('tools/list', {'cursor': 5}),
        _rpc('tools/list', {'cursor': None}),
        _rpc('resources/read', {'uri': 'HTTP://Example.COM'}),
        _rpc('resources/read', {'uri': 'https://a.com/é?q=1 2'}),
        _rpc('resources/read', {'uri': 'not a url'}),
        _rpc('resources/read', {'uri': 5}),
        _rpc('resources/subscribe', {'uri': 'file:///x'}),
        _rpc('prompts/get', {'name': 'x', 'arguments': {'a': 1}}),
        _rpc('prompts/get', {'name': 'x', 'arguments': {'a': 'b'}}),
        _rpc('logging/setLevel', {'level': 'loud'}),
        _rpc('completion/complete', {'ref': {'type': 'ref/other'}, 'argument': {'name': 'a', 'value': 'b'}}),
        _rpc('tasks/get', {'taskId': 't'}),
        _rpc('tasks/get', {'taskId': 5}),
        _rpc('tasks/list'),
        _rpc('notifications/initialized', {}),
        _rpc('ping', headers={**JSON_HEADERS, 'mcp-protocol-version': '2024-11-05'}),
        _post({'jsonrpc': '2.0', 'method': 'notifications/initialized'}, headers={**JSON_HEADERS, 'mcp-protocol-version': 'x'}),
        _rpc('ping', headers={'accept': 'application/jsonx', 'content-type': 'Application/JSON'}),
        _rpc('ping', headers={'accept': 'application/json', 'content-type': 'application/json, text/plain'}),
        _rpc('ping', headers={'accept': 'application/json', 'content-type': 'application/jsonx'}),
        {'method': 'GET', 'path': '/mcp', 'headers': {'accept': 'text/event-stream', 'mcp-protocol-version': 'x'}},
        {'method': 'GET', 'path': '/mcp', 'headers': {'accept': 'application/json'}},
        {'method': 'DELETE', 'path': '/mcp', 'headers': {}},
        {'method': 'PUT', 'path': '/mcp', 'headers': {}},
        {'method': 'PATCH', 'path': '/mcp', 'headers': JSON_HEADERS, 'body': '{}'},
        {'method': 'OPTIONS', 'path': '/mcp', 'headers': {}},
        {'method': 'HEAD', 'path': '/mcp', 'headers': {}},
        _post({'jsonrpc': '2.0', 'id': 1, 'method': 'ping'}, path='/mcp/'),
        {'method': 'GET', 'path': '/mcp/', 'headers': {}},
        {'method': 'GET', 'path': '/mcp/x', 'headers': {}},
        # tools/call: unknown tools, malformed params and argument validation.
        _call('no_such_tool'),
        _call('get_run'),
        _call('get_run', {}),
        _call('get_run', {'run_id': 'abc'}),
        _call('get_run', {'run_id': 2.5}),
        _call('get_run', {'run_id': True}),
        _call('get_run', {'run_id': None}),
        _call('get_run', {'run_id': [1]}),
        _call('get_run', {'run_id': long_text}),
        _call('get_run', {'run_id': {'a': long_text}}),
        _call('get_run', [1]),
        # model_dump(mode='json') writes non-finite floats in the parameters as None.
        _post('{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "get_run", "arguments": {"run_id": NaN}}}'),
        _post('{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "list_runs", "arguments": {"limit": [Infinity]}}}'),
        _post('{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "list_runs", "arguments": {"job": -Infinity}}}'),
        _post('{"jsonrpc": "2.0", "id": NaN, "method": "ping"}'),
        _rpc('tools/call', {'arguments': {}}),
        _rpc('tools/call'),
        _call('list_runs', {'limit': 'many', 'job': 5, 'status': ['failed']}),
        _call('list_runs', {'limit': '1e3'}),
        _call('list_runs', {'limit': 1e20}),
        _call('list_runs', {'limit': 99999999999999999999999}),
        _call('read_logs', {'since': 5}),
        _call('read_logs', {'level': 'LOUD'}),
        _call('read_logs', {'since': 'yesterday'}),
        _call('update_settings', {'section': 'web.jandan'}),
        _call('update_settings', {'section': 'web.jandan', 'values': [1]}),
        _call('update_settings', {'section': 'web.jandan', 'values': '{"cron": 5'}),
        _call('update_settings', {'section': 'web.jandan', 'values': '[1]'}),
        _call('list_wechat_events', {'msg_ids': 'abc'}),
        _call('list_wechat_events', {'msg_ids': '7'}),
        _call('list_wechat_events', {'msg_ids': ['1', 2.0, 'x']}),
        _call('list_wechat_events', {'include_message': 'maybe'}),
        _call('list_wechat_events', {'status': 'gone'}),
        _call('retry_wechat_events', {'msg_ids': 'null', 'status': 'done'}),
        _call('retry_notifications', {}),
        _call('retry_notifications', {'ids': []}),
        _call('set_wechat_state', {'key': 'events_after'}),
        _call('set_wechat_state', {'key': 'events_after', 'value': 7}),
        _call('set_wechat_state', {'key': 'cursor', 'value': '7'}),
        _call('set_wechat_state', {'key': 'events_after', 'value': '-1'}),
        _call('run_job', {'job': 'nope'}),
        _call('run_job', {'job': ''}),
        _call('get_settings', {'section': 'web.nope'}),
        _call('search_archive', {'source': 'nope'}),
        _call('list_notifications', {'delivery_status': 'lost'}),
        _call('list_runs', {'kind': 'cron'}),
        _call('list_runs', {'status': 'gone'}),
        _call('list_runs', {'since': '2026-13-01'}),
        _call('get_run', {'run_id': 404}),
        _call('send_test_notification'),
    ]


def _protocol() -> list[dict[str, Any]]:
    exchanges = []
    with _client({}) as (client, _fake, _tables, _state, captured):
        for request in _protocol_requests():
            response = client.request(
                request['method'],
                request['path'],
                headers=request['headers'],
                content=request['body'].encode() if 'body' in request else None,
            )
            exchanges.append({'request': request, 'response': _response(response), 'logged': captured.lines[:]})
            captured.lines.clear()
    return [{'name': 'protocol', 'exchanges': exchanges}]


# ---------- tool calls over stored state ----------

_RUNS = [
    {
        'id': 1,
        'kind': 'scheduled_job',
        'target': 'wechat',
        'status': 'succeeded',
        'requested_at': '2026-03-01T10:00:00+00:00',
        'started_at': '2026-03-01T10:00:00.400000+00:00',
        'finished_at': '2026-03-01T10:00:12.460000+00:00',
        'result': 'saved 3 items',
    },
    {
        'id': 2,
        'kind': 'trigger_job',
        'target': 'wechat',
        'status': 'failed',
        'requested_at': '2026-03-02T09:00:00+00:00',
        'started_at': '2026-03-02T09:00:01+00:00',
        'finished_at': '2026-03-02T09:01:01+00:00',
        'error': 'RuntimeError: token supersecret-wechat rejected by https://bridge?token=supersecret-wechat ' + 'e' * 4100,
        'result': 'r' * 600,
    },
    {
        'id': 3,
        'kind': 'scheduled_job',
        'target': 'jandan',
        'status': 'running',
        'requested_at': '2026-03-02T11:30:00+00:00',
        'started_at': '2026-03-02T11:30:00+00:00',
    },
    {
        'id': 4,
        'kind': 'trigger_job',
        'target': 'all',
        'status': 'pending',
        'requested_at': '2026-03-02T11:30:00+00:00',
    },
    {
        'id': 5,
        'kind': 'trigger_job',
        'target': 'bilibili',
        'status': 'rejected',
        'requested_at': '2026-02-20T00:00:00+00:00',
        'started_at': '2026-02-20T00:00:00+00:00',
        'finished_at': '2026-02-20T00:00:00+00:00',
        'error': 'Runner unavailable for job: bilibili',
    },
]

_SETTINGS = {
    'web.wechat': {'enabled': True, 'cron': '*/15 * * * *', 'token': 'supersecret-wechat', 'bridge_url': 'http://bridge:9970'},
    'web.jandan': {'enabled': True, 'notify': False, 'cron': '5 4 * * 1'},
    'notifications.telegram': {'enabled': True, 'bot_token': '123456:telegram-bot-secret', 'chat_id': '-1001'},
    'services.tikhub': {'tikhub_token': 'tik', 'tikhub_api_url': 'https://api.tikhub.io'},
}

_EVENTS = [
    {
        'msg_id': 101,
        'create_time': 1772400000,
        'work': 'image',
        'seq': 9,
        'message': {'kind': 'image', 'talker': 'wxid_a', 'sender': 'wxid_b', 'content': '', 'media': ['k1'], 'size': 1.5},
        'status': 'failed',
        'attempts': 5,
        'last_error': 'download failed: token=supersecret-wechat',
        'updated_at': '2026-03-02T08:00:00+00:00',
    },
    {
        'msg_id': 102,
        'create_time': 1772400100,
        'work': 'article',
        'seq': 10,
        'message': {'kind': 'link', 'talker': '文件传输助手', 'content': '<xml/>'},
        'status': 'pending',
        'attempts': 2,
        'last_error': 'timeout',
        'updated_at': '2026-03-02T09:00:00+00:00',
    },
    {
        'msg_id': 103,
        'create_time': 1772400100,
        'work': 'image',
        'seq': 11,
        'message': 'not json',
        'status': 'done',
        'attempts': 1,
        'updated_at': '2026-03-02T09:30:00+00:00',
    },
    {
        'msg_id': 104,
        'create_time': 1772300000,
        'work': 'channel_video',
        'seq': 0,
        'message': [1, 2],
        'status': 'pending',
        'attempts': 0,
        'updated_at': '2026-03-01T00:00:00+00:00',
    },
    {
        'msg_id': 105,
        'create_time': 1772400200,
        'work': 'image',
        'seq': 12,
        'message': {'kind': 'image', 'talker': None},
        'status': 'failed',
        'attempts': 3,
        'last_error': '',
        'updated_at': '2026-03-02T10:00:00+00:00',
    },
    {
        'msg_id': 106,
        'create_time': 1772000000,
        'work': 'sticker',
        'seq': 1,
        'message': '{"sender": "wxid_c", "kind": "emoji"}',
        'status': 'done',
        'attempts': 1,
        'updated_at': '2026-02-25T00:00:00+00:00',
    },
]

_NOTIFICATIONS = [
    {
        'id': 11,
        'kind': 'job_failed',
        'source': 'worker',
        'scope': 'job',
        'dedupe_key': 'job_failed:wechat',
        'title': 'wechat failed',
        'body': 'token supersecret-wechat ' + 'b' * 1100,
        'delivery_status': 'failed',
        'attempt_count': 5,
        'occurrence_count': 3,
        'last_error': 'Bad Request: chat not found 123456:telegram-bot-secret',
        'created_at': '2026-03-01T00:00:00+00:00',
        'next_attempt_at': '2026-03-01T01:00:00+00:00',
    },
    {
        'id': 12,
        'kind': 'new_items',
        'source': 'jandan',
        'title': '煎蛋',
        'body': '3 new',
        'delivery_status': 'delivered',
        'attempt_count': 1,
        'created_at': '2026-03-02T00:00:00+00:00',
        'next_attempt_at': '2026-03-02T00:00:00+00:00',
        'delivered_at': '2026-03-02T00:00:05.700000+00:00',
    },
    {
        'id': 13,
        'kind': 'new_items',
        'source': 'jandan',
        'title': 'x',
        'delivery_status': 'failed',
        'created_at': '2026-03-02T01:00:00+00:00',
        'next_attempt_at': '2026-03-02T01:00:00+00:00',
    },
]

_LOG_TEXT = (
    '2026-03-02 10:00:00,100 - embyx.wechat - INFO - run started\n'
    '2026-03-02 10:00:01,200 - embyx.wechat - ERROR - save failed token=supersecret-wechat\n'
    'Traceback (most recent call last):\n'
    '  File "x.py", line 1\n'
    '\n'
    '2026-03-02 10:00:02,300 - embyx.pixiv - WARNING - slow\n'
    '2026-03-02 11:00:00,000 - embyx.fav-api - NOTICE - 煎蛋 ok\n'
    '2026-03-02 11:30:00,999 - embyx.wechat - DEBUG - noisy\n'
    '2026-03-02 11:45:00,000 - apscheduler.scheduler - CUSTOM - odd level\n'
    '2026-03-02 11:50:00,000 - embyx.Wechat - CRITICAL - ' + 'm' * 4100 + '\n'
)

_LOGS = [
    {'name': '20260302.log', 'text': _LOG_TEXT, 'mtime': '2026-03-02T11:55:00+00:00'},
    {
        'name': '20260301.log',
        'text': (
            'continuation before any record\n'
            '2026-03-01 23:59:59,500 - embyx.wechat - INFO - yesterday\n'
            '2026-03-02 10:00:00,100 - embyx.wechat - INFO - same instant, other file\n'
        ),
        'mtime': '2026-03-02T10:00:00+00:00',
    },
    {'name': '20260201.log', 'text': '2026-02-01 00:00:00,000 - embyx.wechat - ERROR - old\n', 'mtime': '2026-02-01T00:00:00+00:00'},
    {
        'name': 'notes.txt',
        'text': '2026-03-02 10:00:00,000 - embyx.wechat - ERROR - not a log file\n',
        'mtime': '2026-03-02T10:00:00+00:00',
    },
]

_SESSIONS = [
    {
        'site': 'pixiv',
        'cookies': [
            {'name': 'PHPSESSID', 'value': 'secret-pixiv', 'domain': '.pixiv.net', 'path': '/', 'expires': 2e9},
            {'name': 'device_token', 'value': 'x', 'domain': '.pixiv.net', 'path': '/', 'expires': None},
            {'name': 'PHPSESSID', 'value': 'dup', 'domain': 'www.pixiv.net', 'path': '/', 'expires': None},
        ],
        'account': 'someone (42)',
        'status': 'valid',
        'message': '',
        'expires_at': '2026-12-01T00:00:00+00:00',
        'signed_in_at': '2026-11-03T00:00:00.250000+00:00',
        'checked_at': '2026-11-04T05:06:07+00:00',
    },
    {'site': 'rednote', 'cookies': [], 'status': 'invalid', 'message': 'Sign in again.'},
]


def _scenarios() -> list[dict[str, Any]]:
    scenarios: list[dict[str, Any]] = []

    def add(name: str, calls: list[tuple[str, dict[str, Any]]], **setup: Any) -> None:
        scenarios.append({'name': name, 'setup': setup, 'calls': [{'tool': tool, 'arguments': args} for tool, args in calls]})

    add('jobs from default settings', [('list_jobs', {})])
    add('jobs with runs', [('list_jobs', {})], settings=_SETTINGS, control_requests=_RUNS)
    add(
        'runs',
        [
            ('list_runs', {}),
            ('list_runs', {'job': ' WeChat ', 'limit': 1}),
            ('list_runs', {'status': 'failed'}),
            ('list_runs', {'kind': 'scheduled_job', 'limit': 0}),
            ('list_runs', {'since': '2h'}),
            ('list_runs', {'since': '2026-03-02T09:00:00Z'}),
            ('list_runs', {'since': '2026-03-02T10:00:00'}),
            ('list_runs', {'since': '2026-03-02 09:00:00+01:00', 'limit': 5000}),
            ('list_runs', {'since': '1d', 'job': 'jandan', 'status': 'running', 'kind': 'scheduled_job'}),
            ('list_runs', {'status': ' failed'}),
            ('list_runs', {'job': '   '}),
            ('get_run', {'run_id': 2}),
            ('get_run', {'run_id': '3'}),
            ('get_run', {'run_id': 99}),
        ],
        settings=_SETTINGS,
        control_requests=_RUNS,
    )
    add(
        'runs redact the DSN password and the API token',
        [('list_runs', {}), ('get_run', {'run_id': 1})],
        control_requests=[
            {
                'id': 1,
                'kind': 'trigger_job',
                'target': 'jandan',
                'status': 'failed',
                'requested_at': '2026-03-02T00:00:00+00:00',
                'error': f'connect {DSN} failed; Authorization: Bearer {TOKEN}; short pw abc',
                'result': 'dsn-password-42 dsn-password-42',
            },
        ],
    )
    add(
        'a DSN without a password',
        [('get_run', {'run_id': 1})],
        dsn='host=db user=fav password=kv-password-1',
        control_requests=[
            {
                'id': 1,
                'kind': 'trigger_job',
                'target': 'jandan',
                'status': 'failed',
                'requested_at': '2026-03-02T00:00:00+00:00',
                'error': 'kv-password-1',
            },
        ],
    )
    add(
        'logs',
        [
            ('read_logs', {}),
            ('read_logs', {'since': '', 'level': 'debug'}),
            ('read_logs', {'since': '1d', 'logger_name': 'WECHAT', 'level': ' warning '}),
            ('read_logs', {'since': '', 'contains': 'TRACEBACK'}),
            ('read_logs', {'since': '2026-03-02T10:00:01Z', 'until': '2026-03-02T11:00:00Z', 'level': 'DEBUG'}),
            ('read_logs', {'since': '', 'level': 'DEBUG', 'limit': 2}),
            ('read_logs', {'since': '', 'level': 'DEBUG', 'limit': 0}),
            ('read_logs', {'since': '30d', 'level': 'ERROR', 'limit': 5000}),
            ('read_logs', {'since': '5m'}),
            ('read_logs', {'since': '', 'level': 'NOTICE', 'contains': '煎蛋'}),
        ],
        settings=_SETTINGS,
        logs=_LOGS,
    )
    add('no log files', [('read_logs', {}), ('read_logs', {'since': ''})])
    add(
        'settings',
        [
            ('get_settings', {'section': 'web.wechat'}),
            ('get_settings', {}),
            ('update_settings', {'section': 'web.wechat', 'values': {'cron': '*/30 * * * *'}}),
            ('update_settings', {'section': 'notifications.telegram', 'values': {'chat_id': '-1002', 'bot_token': '1234••••'}}),
            ('update_settings', {'section': 'web.jandan', 'values': '{"notify": true}'}),
            ('update_settings', {'section': 'web.jandan', 'values': {'cron': 5}}),
            ('update_settings', {'section': 'web.bilibili', 'values': {'enabled': True}}),
            ('update_settings', {'section': 'web.nope', 'values': {}}),
            ('update_settings', {'section': 'web.jandan', 'values': {'unknown_field': 1}}),
            ('get_settings', {'section': 'web.jandan'}),
        ],
        settings=_SETTINGS,
    )
    add(
        'run_job',
        [
            ('run_job', {'job': 'jandan'}),
            ('run_job', {'job': ' ALL '}),
            ('run_job', {'job': 'not-a-job'}),
            ('list_runs', {'limit': 2}),
        ],
        settings=_SETTINGS,
        control_requests=_RUNS,
    )
    add('send_test_notification without a bot', [('send_test_notification', {})])
    add('site sessions', [('site_sessions', {})], site_sessions=_SESSIONS)
    add('no site sessions', [('site_sessions', {})])
    add(
        'wechat',
        [
            ('wechat_status', {}),
            ('wechat_status', {'recent_failures': 1}),
            ('wechat_status', {'recent_failures': 0}),
            ('list_wechat_events', {}),
            ('list_wechat_events', {'status': 'failed', 'include_message': True}),
            ('list_wechat_events', {'work': 'image', 'limit': 2}),
            ('list_wechat_events', {'msg_ids': [104, 102, 999], 'include_message': 'yes'}),
            ('list_wechat_events', {'msg_ids': '[103]', 'include_message': 1}),
            ('list_wechat_events', {'msg_ids': []}),
            ('list_wechat_events', {'msg_ids': [106, 103, 101], 'include_message': True}),
        ],
        settings=_SETTINGS,
        wechat_events=_EVENTS,
        wechat_state={'work_version': '3', 'events_after': '120'},
    )
    add('wechat with no state', [('wechat_status', {}), ('list_wechat_events', {})])
    add(
        'retry wechat events',
        [
            ('retry_wechat_events', {'msg_ids': [101, 103]}),
            ('retry_wechat_events', {}),
            ('retry_wechat_events', {'status': 'done', 'msg_ids': [103]}),
            ('retry_wechat_events', {'status': 'done'}),
            ('retry_wechat_events', {'status': 'pending', 'msg_ids': []}),
        ],
        wechat_events=_EVENTS,
    )
    add(
        'wechat state',
        [
            ('set_wechat_state', {'key': 'events_after', 'value': ' 0 '}),
            ('set_wechat_state', {'key': 'work_version', 'value': 'anything'}),
            ('set_wechat_state', {'key': 'work_version', 'value': '4'}),
            ('set_wechat_state', {'key': 'events_after', 'value': '١٢'}),
            ('set_wechat_state', {'key': 'events_after', 'value': ''}),
            ('wechat_status', {}),
        ],
        wechat_state={'events_after': '120'},
    )
    add(
        'notifications',
        [
            ('list_notifications', {}),
            ('list_notifications', {'delivery_status': 'failed'}),
            ('list_notifications', {'source': 'jandan', 'limit': 1}),
            ('retry_notifications', {'ids': [13, 12, 11, 99]}),
            ('retry_notifications', {'ids': [11]}),
            ('list_notifications', {'delivery_status': 'pending'}),
        ],
        settings=_SETTINGS,
        notifications=_NOTIFICATIONS,
    )
    add('archive sources need the database', [('search_archive', {'source': 'nope', 'query': 'x'})])
    return scenarios


def _replay(scenario: dict[str, Any]) -> dict[str, Any]:
    results = []
    with _client(scenario['setup']) as (client, fake, tables, state, captured):
        for rpc_id, call in enumerate(scenario['calls'], start=1):
            fake.calls.clear()
            response = client.post(
                '/mcp',
                headers=JSON_HEADERS,
                json={
                    'jsonrpc': '2.0',
                    'id': rpc_id,
                    'method': 'tools/call',
                    'params': {'name': call['tool'], 'arguments': call['arguments']},
                },
            )
            assert response.status_code == 200, response.text  # noqa: PLR2004, S101
            entry: dict[str, Any] = {
                **call,
                'response': response.json(),
                'bytes': response.text,
                'queries': list(fake.calls),
                'logged': captured.lines[:],
            }
            if call['tool'] == 'run_job':
                entry['volatile'] = ['id', 'requested_at']
            results.append(entry)
            captured.lines.clear()
        after = {
            'control_requests': [_dump_row(_run_row(item)) for item in sorted(state.requests, key=lambda item: item.request_id)],
            'wechat_events': [_dump_row(row) for row in sorted(tables.wechat_events, key=lambda row: row['msg_id'])],
            'wechat_state': dict(sorted(tables.wechat_state.items())),
            'notifications': [_dump_row(row) for row in sorted(tables.notifications, key=lambda row: row['id'])],
            'settings': dict(sorted(state.rows.items())),
        }
    return {**scenario, 'calls': results, 'after': after}


def _calls() -> list[dict[str, Any]]:
    return [_replay(scenario) for scenario in _scenarios()]


# ---------- pure helpers ----------


def _ops() -> list[dict[str, Any]]:  # noqa: C901
    cases: list[dict[str, Any]] = []

    def since(value: Any) -> dict[str, Any]:
        try:
            parsed = ops.parse_since(value)
        except ops.OpsError as exc:
            return {'error': str(exc)}
        except (OverflowError, ValueError) as exc:
            return {'exception': type(exc).__name__, 'error': str(exc)}
        return {'value': parsed.isoformat() if parsed else None}

    saved = ops.datetime
    ops.datetime = _FrozenDatetime
    try:
        for value in [
            None,
            '',
            '  ',
            '30m',
            ' 6h ',
            '2d',
            '0m',
            '10 m',
            '1w',
            '2H',
            '٣h',
            '2026-10-12T17:00:00Z',
            '2026-10-12T17:00:00',
            '2026-10-12',
            '2026-10-12 17:00',
            '2026-10-12T17:00:00.123456+05:30',
            '2026-10-12T17:00:00,5Z',
            '20261012T170000Z',
            '2026-W41-1',
            'yesterday',
            "it's",
            '2026-02-30',
            '3 d',
            '007m',
            '999999999d',
            '1000000000d',
            '99999999999999999999m',
            '2026-10-12T17',
            '2026-10-12T1700',
            '2026-10-12T17:00:00.1234567',
            '2026-10-12T17:00+0530',
            '2026-10-12T17:00-05',
            '2026-10-12T17:00:00+05:30:15',
            '2026-10-12T17:00:00+05:30:15.5',
            '2026-10-12 24:00',
            '2026-10-12x17:00',
            '2026W411',
            '2026W41',
            '2026W417',
            '2026-W53-1',
            '2025-W53',
            '2026-W00',
            '2026-W41-8',
            '2026-10-12T17:00:00.5+00:00',
            '2026-10-12T17:00:00Z ',
            '٢٠٢٦-10-12',
            '2026-10-12T17:00:00+24:00',
            '2026-10-12T17:00:00-00:00',
            '2026-10-12T17:00:00.123+01',
            '2026-10-1217:00',
            '2026-10-12T',
            '0001-01-01',
            '0000-01-01',
            '9999-12-31T23:59:59',
            '2026-10-12T17:00:00,1234567890Z',
            '2026-10-12T17:00:00.12a',
            '+2026-10-12',
            '2026-1-12',
            '2026-10-12T7:00',
            '2026-10-12T17:0',
            '2026-10-12T17:00:60',
            '2026-10-12T17:00Z+01',
            '2026-10-12T17:00+01Z',
            '2026-10-12T17:00:00+1',
            '2026-10-12T17:00:00+123',
            '2026-10-12T17.5',
            '2026-10-12T17:00.5',
            '2026-10-12T1700:00',
            '20261012',
            '2026-1012',
            '202610-12',
            '2026-10-12T17:00:00.',
            '2026-10-12T17:00:00+05:30Z',
            '2026-10-12T17:00:00\u221205:00',
            '2026-10-12T17:00:00 +05:00',
            '2026-10-12\u300017:00',
        ]:
            cases.append({'kind': 'parse_since', 'input': value, **since(value)})  # noqa: PERF401 - the clock is frozen only here
    finally:
        ops.datetime = saved
    for text, limit in [('abc', 3), ('abcd', 3), ('', 0), ('日本語テキスト', 3), ('x' * 10, 4000)]:
        cases.append({'kind': 'truncate', 'input': text, 'limit': limit, 'value': ops.truncate(text, limit)})
    for limit, maximum in [(0, 500), (-5, 500), (1, 500), (500, 500), (501, 500), (2000, 1000)]:
        cases.append({'kind': 'bounded', 'input': limit, 'maximum': maximum, 'value': ops.bounded(limit, maximum)})
    secrets = ['short', 'abcdef', 'abcdefgh', '密码密码密码', 'abcdefghij']
    redact = ops.Redactor(secrets)
    cases.extend(
        {'kind': 'redact', 'secrets': secrets, 'input': text, 'value': redact(text)}
        for text in ['abcdefghij and abcdefgh and abcdef', 'short', '密码密码密码!', 'none here']
    )
    for base, changes in [
        ({'a': {'b': 1, 'c': 2}, 'l': [1, 2]}, {'a': {'c': 3}, 'l': [9]}),
        ({'a': 1}, {'a': {'b': 2}}),
        ({'a': {'b': 2}}, {'a': None}),
        ({'a': {'b': {'c': 1, 'd': 2}}}, {'a': {'b': {'d': 3}, 'e': 4}}),
        ({}, {'x': 1}),
    ]:
        cases.append({'kind': 'merge', 'base': base, 'changes': changes, 'value': _merge(base, changes)})
    with tempfile.TemporaryDirectory() as raw_dir:
        directory = Path(raw_dir)
        _write_logs(directory, _LOGS)
        for kwargs in [
            {'since': None},
            {'since': '2026-03-02T10:00:01+00:00', 'logger_name': 'wechat', 'min_level': 'INFO'},
            {'since': None, 'min_level': 'ERROR', 'limit': 1},
            {'since': None, 'until': '2026-03-02T10:00:01.200000+00:00'},
            {'since': None, 'min_level': 'notice'},
            {'since': None, 'min_level': 'nope'},
        ]:
            call = dict(kwargs)
            for key in ('since', 'until'):
                if call.get(key):
                    call[key] = datetime.fromisoformat(call[key])
            try:
                value: Any = ops.read_logs(directory, redact=ops.Redactor(['supersecret-wechat']), **call)
            except ops.OpsError as exc:
                value = {'error': str(exc)}
            cases.append({'kind': 'read_logs', 'arguments': kwargs, 'value': value})
    cases.append({'kind': 'read_logs_missing_dir', 'value': ops.read_logs(Path('/nonexistent-fav-log-dir'), since=None)})
    cases.append(
        {
            'kind': 'next_fire_time',
            'cases': [
                {'cron': cron, 'now': FIXED_NOW.isoformat(), 'value': ops.next_fire_time(cron, now=FIXED_NOW)}
                for cron in ['*/15 * * * *', '0 1 * * *', '5 4 * * 1', 'bad cron', '61 * * * *', '0 0 31 2 *']
            ],
        },
    )
    cases.extend(_zoned_ops())
    # str.casefold() of every character it changes; any other character folds to itself.
    folds = {chr(code): chr(code).casefold() for code in range(0x110000) if chr(code).casefold() != chr(code)}
    cases.append({'kind': 'casefold', 'value': folds})
    return cases


# Log lines and schedules around the 2026 daylight-saving changes of each zone (TZ).
_ZONED_LOG_TEXT = (
    '2026-03-08 01:59:59,999 - embyx.wechat - INFO - before spring forward\n'
    '2026-03-08 02:30:00,000 - embyx.wechat - INFO - a skipped local time\n'
    '2026-03-08 03:00:00,000 - embyx.wechat - INFO - after spring forward\n'
    '2026-11-01 01:30:00,000 - embyx.wechat - INFO - an ambiguous local time\n'
    '2026-11-01 02:00:00,000 - embyx.wechat - INFO - after fall back\n'
    '2026-10-25 02:30:00,000 - embyx.wechat - INFO - the European change\n'
)
_ZONED_NOWS = ['2026-03-07T12:00:00+00:00', '2026-03-08T06:59:00+00:00', '2026-10-31T12:00:00+00:00', '2026-11-01T05:59:00+00:00']
_ZONED_CRONS = ['30 2 * * *', '30 1 * * *', '0 * * * *', '*/15 * * * *', '0 3 * * 0']


def _zoned_ops() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    saved = os.environ.get('TZ')
    try:
        for zone in ['America/New_York', 'Europe/Berlin', 'Asia/Kolkata', 'Australia/Lord_Howe', 'No/Such_Zone', '']:
            os.environ['TZ'] = zone
            with tempfile.TemporaryDirectory() as raw_dir:
                directory = Path(raw_dir)
                _write_logs(directory, [{'name': '20260308.log', 'text': _ZONED_LOG_TEXT, 'mtime': '2026-11-02T00:00:00+00:00'}])
                value = ops.read_logs(directory, since=None, redact=ops.Redactor([]))
            fires = [
                {'cron': cron, 'now': now, 'value': ops.next_fire_time(cron, now=datetime.fromisoformat(now))}
                for now in _ZONED_NOWS
                for cron in _ZONED_CRONS
            ]
            cases.append({'kind': 'zoned', 'tz': zone, 'log_text': _ZONED_LOG_TEXT, 'read_logs': value, 'next_fire_time': fires})
    finally:
        if saved is None:
            os.environ.pop('TZ', None)
        else:
            os.environ['TZ'] = saved
    return cases


GOLDENS = {
    'mcp/protocol.json': _protocol,
    'mcp/calls.json': _calls,
    'mcp/ops.json': _ops,
}
