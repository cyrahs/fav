# ruff: noqa: INP001, S101, ANN001

import asyncio
from datetime import UTC, datetime

import src.tool.notifications as notifications_module
from src.tool.notifications import NotificationRecord, ack_notifications_sync, enqueue_notification, list_notifications_sync

_NOW = datetime(2026, 3, 2, 0, 0, tzinfo=UTC)


class _FakeCursor:
    def __init__(self, *, rows=None, rowcount: int = 0) -> None:
        self._rows = list(rows or [])
        self.rowcount = rowcount
        self.executed: list[tuple[str, tuple[object, ...] | None]] = []

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        return None

    def execute(self, sql: str, params: tuple[object, ...] | None = None) -> None:
        self.executed.append((sql, params))

    def fetchall(self):
        return list(self._rows)


class _FakeConnection:
    def __init__(self, cursor: _FakeCursor) -> None:
        self._cursor = cursor

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        return None

    def cursor(self) -> _FakeCursor:
        return self._cursor


def test_list_notifications_sync_filters_unread(monkeypatch) -> None:
    cursor = _FakeCursor(
        rows=[
            {
                'id': 10,
                'kind': 'download_completed',
                'source': 'bilibili',
                'title': 'Title',
                'body': 'Body',
                'link_url': 'https://example.com',
                'image_url': '',
                'payload': '{"bvid":"BV1TEST"}',
                'status': 'unread',
                'created_at': _NOW,
                'read_at': None,
            },
        ],
    )
    monkeypatch.setattr(notifications_module.psycopg, 'connect', lambda *args, **kwargs: _FakeConnection(cursor))

    rows = list_notifications_sync('postgresql://db.local/fav', status='unread', limit=25, after_id=9)

    assert rows == [
        NotificationRecord(
            notification_id=10,
            kind='download_completed',
            source='bilibili',
            title='Title',
            body='Body',
            link_url='https://example.com',
            image_url='',
            payload='{"bvid":"BV1TEST"}',
            status='unread',
            created_at=_NOW,
            read_at=None,
        ),
    ]
    select_sql, select_params = cursor.executed[-1]
    assert 'WHERE status = %s AND id > %s' in select_sql
    assert select_params == ('unread', 9, 25)


def test_ack_notifications_sync_marks_unread_as_read(monkeypatch) -> None:
    cursor = _FakeCursor(rowcount=2)
    monkeypatch.setattr(notifications_module.psycopg, 'connect', lambda *args, **kwargs: _FakeConnection(cursor))

    updated = ack_notifications_sync('postgresql://db.local/fav', [3, 1, 3, -1])

    assert updated == 2
    update_sql, update_params = cursor.executed[-1]
    assert 'UPDATE notifications' in update_sql
    assert update_params == ('read', 'unread', [1, 3])


def test_notification_record_payload_json_handles_invalid_json() -> None:
    record = NotificationRecord(
        notification_id=1,
        kind='download_completed',
        source='bilibili',
        title='Title',
        body='Body',
        link_url='',
        image_url='',
        payload='not-json',
        status='unread',
        created_at=_NOW,
        read_at=None,
    )

    assert record.payload_json == {}


def test_enqueue_notification_serializes_payload(monkeypatch) -> None:
    captured: dict[str, object] = {}

    async def _fake_query_db(sql: str, params: tuple[object, ...] = ()) -> list[dict[str, object]]:
        captured['sql'] = sql
        captured['params'] = params
        return [
            {
                'id': 7,
                'kind': 'download_completed',
                'source': 'telegram',
                'title': 'Telegram: Demo',
                'body': 'body',
                'link_url': '',
                'image_url': '',
                'payload': '{"message_id":456}',
                'status': 'unread',
                'created_at': _NOW,
                'read_at': None,
            },
        ]

    monkeypatch.setattr(notifications_module.database, 'query_db', _fake_query_db)

    created = asyncio.run(
        enqueue_notification(
            kind='download_completed',
            source='telegram',
            title='Telegram: Demo',
            body='body',
            payload={'message_id': 456},
        ),
    )

    assert isinstance(created, NotificationRecord)
    assert created.notification_id == 7
    assert created.payload_json == {'message_id': 456}
    assert captured['params'] == (
        'download_completed',
        'telegram',
        'Telegram: Demo',
        'body',
        '',
        '',
        '{"message_id":456}',
        'unread',
    )
