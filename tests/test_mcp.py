# ruff: noqa: INP001, S101, S105, ANN001, ARG001, PLR2004

import asyncio
import json
import os
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from fastapi.testclient import TestClient

import src.tool.database as database_module
from src.api import ops
from src.api.app import create_app
from src.api.errors import ApiError
from src.api.mcp_server import _merge
from src.service.jobs import ScheduledJob

_HEADERS = {'accept': 'application/json, text/event-stream', 'content-type': 'application/json'}


class _FakeService:
    def __init__(self) -> None:
        self.job_requests: list[str] = []
        self.saved: list[tuple[str, dict[str, Any]]] = []
        self.stored = {'enabled': True, 'cron': '*/15 * * * *', 'token': 'abcd••••', 'proxy': {'url': 'http://a', 'timeout': 5}}

    def scheduled_jobs(self) -> list[ScheduledJob]:
        return [
            ScheduledJob(key='wechat', name='WeChat', cron='*/15 * * * *', enabled=True, required_commands=(), factory=object),
            ScheduledJob(key='pixiv', name='Pixiv', cron='0 1 * * *', enabled=False, required_commands=(), factory=object),
        ]

    def create_job_request(self, target: str) -> dict[str, object]:
        if target not in {'wechat', 'all'}:
            raise ApiError(status_code=400, code='invalid_target', message='Unsupported job target.', details={'allowed_values': ['all']})
        self.job_requests.append(target)
        return {'id': 7, 'target': target, 'status': 'pending'}

    def get_settings_section(self, section: str) -> dict[str, object]:
        return {'section': section, 'value': dict(self.stored), 'missing_fields': []}

    def update_settings_section(self, section: str, payload: dict[str, Any]) -> dict[str, object]:
        self.saved.append((section, payload))
        return {'section': section, 'value': payload, 'missing_fields': []}

    def close(self) -> None:
        return None


class _FakeDb:
    def __init__(self, responses: list[list[dict[str, Any]]] | None = None) -> None:
        self.calls: list[tuple[str, tuple[Any, ...]]] = []
        self.responses = list(responses or [])

    async def query_db(self, query: str, params: tuple[Any, ...] = ()) -> list[dict[str, Any]]:
        self.calls.append((' '.join(query.split()), params))
        if query.lstrip().startswith('CREATE TABLE'):
            return []
        return self.responses.pop(0) if self.responses else []


@pytest.fixture
def fake_db(monkeypatch) -> _FakeDb:
    db = _FakeDb()
    monkeypatch.setattr(database_module, 'query_db', db.query_db)
    monkeypatch.setattr('src.tool.control_queue._table_ready', True)
    return db


def _rpc(client: TestClient, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
    response = client.post('/mcp', headers=_HEADERS, json={'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params or {}})
    assert response.status_code == 200, response.text
    return response.json()


def _call_tool(client: TestClient, name: str, arguments: dict[str, Any] | None = None) -> dict[str, Any]:
    return _rpc(client, 'tools/call', {'name': name, 'arguments': arguments or {}})['result']


def _tool_payload(result: dict[str, Any]) -> Any:
    assert not result.get('isError'), result
    structured = result.get('structuredContent')
    if structured is not None:
        # FastMCP wraps results that are not objects as {"result": ...}.
        return structured['result'] if set(structured) == {'result'} else structured
    return json.loads(result['content'][0]['text'])


def test_mcp_endpoint_lists_tools_over_http() -> None:
    with TestClient(create_app(service=_FakeService())) as client:
        init = _rpc(
            client,
            'initialize',
            {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'tests', 'version': '0'}},
        )
        assert init['result']['serverInfo']['name'] == 'fav'
        names = {tool['name'] for tool in _rpc(client, 'tools/list')['result']['tools']}

    assert {'get_runtime', 'list_jobs', 'list_runs', 'read_logs', 'run_job', 'update_settings', 'retry_wechat_events'} <= names


def test_mcp_endpoint_answers_for_a_cluster_host_name() -> None:
    with TestClient(create_app(service=_FakeService()), base_url='http://fav.media.svc.cluster.local') as client:
        names = {tool['name'] for tool in _rpc(client, 'tools/list')['result']['tools']}
    assert 'run_job' in names


def test_run_job_queues_a_control_request() -> None:
    service = _FakeService()
    with TestClient(create_app(service=service)) as client:
        payload = _tool_payload(_call_tool(client, 'run_job', {'job': 'wechat'}))
        rejected = _call_tool(client, 'run_job', {'job': 'nope'})

    assert payload['id'] == 7
    assert service.job_requests == ['wechat']
    assert rejected['isError'] is True
    assert 'Unsupported job target' in rejected['content'][0]['text']


def test_update_settings_merges_only_the_given_fields() -> None:
    service = _FakeService()
    with TestClient(create_app(service=service)) as client:
        values = {'cron': '*/30 * * * *', 'proxy': {'timeout': 9}}
        _tool_payload(_call_tool(client, 'update_settings', {'section': 'web.wechat', 'values': values}))

    assert service.saved == [
        ('web.wechat', {'enabled': True, 'cron': '*/30 * * * *', 'token': 'abcd••••', 'proxy': {'url': 'http://a', 'timeout': 9}}),
    ]


def test_list_jobs_reports_next_run_and_last_run(fake_db) -> None:
    started = datetime(2026, 10, 12, 17, 0, tzinfo=UTC)
    fake_db.responses = [
        [
            {
                'id': 3,
                'kind': 'scheduled_job',
                'target': 'wechat',
                'status': 'failed',
                'requested_at': started,
                'started_at': started,
                'finished_at': started + timedelta(seconds=12),
                'result': '',
                'error': 'boom',
            },
        ],
    ]
    with TestClient(create_app(service=_FakeService())) as client:
        jobs = _tool_payload(_call_tool(client, 'list_jobs'))

    by_key = {job['key']: job for job in jobs}
    assert by_key['wechat']['last_run']['status'] == 'failed'
    assert by_key['wechat']['next_run_at'] is not None
    assert by_key['pixiv']['next_run_at'] is None
    assert by_key['pixiv']['last_run'] is None


def test_merge_replaces_lists_and_merges_objects() -> None:
    assert _merge({'a': {'b': 1, 'c': 2}, 'l': [1, 2]}, {'a': {'c': 3}, 'l': [9]}) == {'a': {'b': 1, 'c': 3}, 'l': [9]}


def test_list_runs_filters_and_redacts(fake_db) -> None:
    now = datetime(2026, 10, 12, tzinfo=UTC)
    fake_db.responses = [
        [
            {
                'id': 1,
                'kind': 'trigger_job',
                'target': 'wechat',
                'status': 'failed',
                'requested_at': now,
                'started_at': now,
                'finished_at': None,
                'result': '',
                'error': 'token supersecret123 rejected',
            },
        ],
    ]
    runs = asyncio.run(ops.list_runs(job='WeChat', status='failed', since=now, limit=5000, redact=ops.Redactor(['supersecret123'])))

    query, params = fake_db.calls[-1]
    assert 'WHERE target = ? AND status = ? AND requested_at >= ?' in query
    assert params == ('wechat', 'failed', now, ops.MAX_LIMIT)
    assert runs[0]['error'] == 'token supe•••• rejected'


def test_list_runs_rejects_unknown_status(fake_db) -> None:
    with pytest.raises(ops.OpsError):
        asyncio.run(ops.list_runs(status='broken'))


def test_retry_wechat_events_resets_attempts(fake_db) -> None:
    fake_db.responses = [[{'msg_id': 5}, {'msg_id': 2}]]
    retried = asyncio.run(ops.retry_wechat_events(msg_ids=[2, 5]))

    query, params = fake_db.calls[-1]
    assert "SET status = 'pending', attempts = 0" in query
    assert 'msg_id = ANY(?)' in query
    assert params == ('failed', [2, 5])
    assert retried == [2, 5]


def test_retry_wechat_events_refuses_to_redo_every_done_event(fake_db) -> None:
    with pytest.raises(ops.OpsError):
        asyncio.run(ops.retry_wechat_events(status='done'))


def test_set_wechat_state_validates_the_cursor(fake_db) -> None:
    with pytest.raises(ops.OpsError):
        asyncio.run(ops.set_wechat_state('events_after', 'abc'))
    with pytest.raises(ops.OpsError):
        asyncio.run(ops.set_wechat_state('something_else', '1'))

    fake_db.responses = [[{'value': '120'}], []]
    result = asyncio.run(ops.set_wechat_state('events_after', '0'))
    assert result == {'key': 'events_after', 'previous': '120', 'value': '0'}


def test_parse_since_accepts_relative_and_iso() -> None:
    assert ops.parse_since('') is None
    assert ops.parse_since('2026-10-12T17:00:00Z') == datetime(2026, 10, 12, 17, tzinfo=UTC)
    relative = ops.parse_since('2h')
    assert relative is not None
    assert timedelta(hours=1, minutes=59) < datetime.now(UTC) - relative < timedelta(hours=2, minutes=1)
    with pytest.raises(ops.OpsError):
        ops.parse_since('yesterday')


def test_read_logs_groups_tracebacks_filters_and_skips_stale_files(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.setenv('TZ', 'UTC')
    current = tmp_path / '20261012.log'
    current.write_text(
        '2026-10-12 17:00:00,100 - embyx.wechat - INFO - run started\n'
        '2026-10-12 17:00:01,200 - embyx.wechat - ERROR - save failed token=supersecret123\n'
        'Traceback (most recent call last):\n'
        '  File "x.py", line 1\n'
        '2026-10-12 17:00:02,300 - embyx.pixiv - WARNING - slow\n',
        encoding='utf-8',
    )
    stale = tmp_path / '20261001.log'
    stale.write_text('2026-10-01 00:00:00,000 - embyx.wechat - ERROR - old\n', encoding='utf-8')
    old_mtime = datetime(2026, 10, 1, tzinfo=UTC).timestamp()
    os.utime(stale, (old_mtime, old_mtime))

    result = ops.read_logs(
        tmp_path,
        since=datetime(2026, 10, 12, 16, tzinfo=UTC),
        logger_name='wechat',
        min_level='INFO',
        redact=ops.Redactor(['supersecret123']),
    )

    assert result['files'] == ['20261012.log']
    assert [record['message'].splitlines()[0] for record in result['records']] == ['run started', 'save failed token=supe••••']
    assert 'Traceback' in result['records'][1]['message']

    errors = ops.read_logs(tmp_path, since=None, min_level='ERROR', limit=1)
    assert errors['matched'] == 2
    assert errors['truncated'] is True
    assert errors['records'][0]['message'].startswith('save failed')


def test_collect_secrets_includes_masked_settings_and_api_token(pinned_settings) -> None:
    pinned_settings.web.wechat.token = 'wechat-token-value'
    secrets = ops.collect_secrets()
    assert 'wechat-token-value' in secrets
    assert 'token-for-tests' in secrets
