# ruff: noqa: INP001, S101, ANN001, ANN202, ANN204, ARG001, PLR2004, SLF001

import asyncio
from contextlib import asynccontextmanager
from datetime import UTC, datetime
from pathlib import Path

import src.tool.notifications as notifications_module
from src.tool.notifications import (
    DELIVERY_DELIVERED,
    DELIVERY_FAILED,
    DELIVERY_PENDING,
    DELIVERY_SENDING,
    WEBHOOK_ACTION_RESOLVE,
    WEBHOOK_ACTION_SEND,
    WEBHOOK_ACTION_UPSERT,
    NotificationRecord,
    claim_next_pending_notification,
    enqueue_notification,
    ensure_notifications_table,
    mark_notification_delivered,
    mark_notification_failed,
    mark_notification_retry,
    reset_stale_sending_notifications,
    resolve_notification,
    retry_delay_seconds,
)

_NOW = datetime(2026, 3, 2, 0, 0, tzinfo=UTC)


class _FakeAsyncCursor:
    def __init__(self, rows) -> None:
        self._rows = list(rows)
        self.executed: list[tuple[str, tuple[object, ...] | None]] = []

    async def __aenter__(self):
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        return None

    async def execute(self, sql: str, params: tuple[object, ...] | None = None) -> None:
        self.executed.append((sql, params))

    async def fetchone(self):
        if not self._rows:
            return None
        return self._rows.pop(0)


class _FakeAsyncTransaction:
    async def __aenter__(self):
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        return None


class _FakeAsyncConnection:
    def __init__(self, cursor: _FakeAsyncCursor) -> None:
        self._cursor = cursor

    async def __aenter__(self):
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        return None

    def transaction(self) -> _FakeAsyncTransaction:
        return _FakeAsyncTransaction()

    def cursor(self) -> _FakeAsyncCursor:
        return self._cursor


def _reset_schema_state() -> None:
    notifications_module._SCHEMA_READY = False


def test_ensure_notifications_table_runs_schema_migration(monkeypatch) -> None:
    captured: list[str] = []
    _reset_schema_state()

    async def _fake_query_db_multi(sql: str, params: tuple[object, ...] = ()) -> list[list[dict[str, object]]]:
        captured.append(sql)
        return []

    monkeypatch.setattr(notifications_module.database, 'query_db_multi', _fake_query_db_multi)

    asyncio.run(ensure_notifications_table())

    assert len(captured) == 1
    assert 'ALTER TABLE notifications ADD COLUMN IF NOT EXISTS dedupe_key TEXT NOT NULL DEFAULT' in captured[0]
    assert 'ALTER TABLE notifications ADD COLUMN IF NOT EXISTS header TEXT NOT NULL DEFAULT' in captured[0]
    assert 'ALTER TABLE notifications ADD COLUMN IF NOT EXISTS pin BOOLEAN NOT NULL DEFAULT FALSE' in captured[0]
    assert 'CREATE UNIQUE INDEX IF NOT EXISTS notifications_dedupe_key_unique' in captured[0]
    assert 'ALTER TABLE notifications ADD COLUMN IF NOT EXISTS event_version INTEGER NOT NULL DEFAULT 1' in captured[0]
    assert "ALTER TABLE notifications ADD COLUMN IF NOT EXISTS delivery_status TEXT NOT NULL DEFAULT 'pending'" in captured[0]
    assert "WHERE status = 'read'" in captured[0]


def test_enqueue_notification_serializes_payload_and_renders_markdown(monkeypatch) -> None:
    captured: dict[str, object] = {}
    _reset_schema_state()

    async def _fake_query_db_multi(sql: str, params: tuple[object, ...] = ()) -> list[list[dict[str, object]]]:
        return []

    async def _fake_query_db(sql: str, params: tuple[object, ...] = ()) -> list[dict[str, object]]:
        captured['sql'] = sql
        captured['params'] = params
        return [
            {
                'id': 7,
                'kind': 'job_failed',
                'source': 'worker',
                'title': 'Episode [1]!',
                'body': 'Path_(draft)',
                'link_url': 'https://example.com/watch?v=1',
                'image_url': '',
                'payload': '{"message_id":456}',
                'dedupe_key': '',
                'status': 'unread',
                'markdown': '[*Episode \\[1\\]\\!*](https://example.com/watch?v=1)\nPath\\_\\(draft\\)',
                'disable_web_page_preview': False,
                'disable_notification': False,
                'pin': True,
                'webhook_action': WEBHOOK_ACTION_SEND,
                'occurrence_count': 1,
                'event_version': 1,
                'delivery_status': 'pending',
                'attempt_count': 0,
                'next_attempt_at': _NOW,
                'created_at': _NOW,
                'read_at': None,
                'delivered_at': None,
                'last_error': '',
            },
        ]

    monkeypatch.setattr(notifications_module.database, 'query_db_multi', _fake_query_db_multi)
    monkeypatch.setattr(notifications_module.database, 'query_db', _fake_query_db)

    created = asyncio.run(
        enqueue_notification(
            kind='job_failed',
            source='worker',
            title='Episode [1]!',
            body='Path_(draft)',
            link_url='https://example.com/watch?v=1',
            payload={'message_id': 456},
        ),
    )

    assert isinstance(created, NotificationRecord)
    assert created.notification_id == 7
    assert created.payload_json == {'message_id': 456}
    assert 'ON CONFLICT (dedupe_key) WHERE dedupe_key <> ' in captured['sql']
    assert 'event_version = notifications.event_version + 1' in captured['sql']
    assert 'notifications.next_attempt_at > CURRENT_TIMESTAMP' in captured['sql']
    assert created.markdown == '[*Episode \\[1\\]\\!*](https://example.com/watch?v=1)\nPath\\_\\(draft\\)'
    assert created.disable_web_page_preview is False
    assert created.disable_notification is False
    assert created.pin is True
    assert captured['params'] == (
        'job_failed',
        'worker',
        '',
        'Episode [1]!',
        'Path_(draft)',
        'https://example.com/watch?v=1',
        '',
        '{"message_id":456}',
        '',
        'unread',
        '[*Episode \\[1\\]\\!*](https://example.com/watch?v=1)\nPath\\_\\(draft\\)',
        False,
        False,
        True,
        WEBHOOK_ACTION_SEND,
        1,
        1,
        'pending',
        0,
        '',
    )


def test_enqueue_notification_drops_a_muted_source(monkeypatch, pinned_settings) -> None:
    _reset_schema_state()
    pinned_settings.web.bilibili.notify = False

    statements: list[str] = []

    async def _record_query(sql, params=()):
        statements.append(sql)
        return []

    monkeypatch.setattr(notifications_module.database, 'query_db_multi', _record_query)
    monkeypatch.setattr(notifications_module.database, 'query_db', _record_query)

    created = asyncio.run(
        enqueue_notification(
            kind='download_completed',
            source='bilibili',
            header='Bilibili',
            title='Episode 1',
        ),
    )

    assert created is None
    # Not even the schema migration: a muted source never touches the outbox.
    assert statements == []


def test_enqueue_notification_with_dedupe_key_uses_upsert_action(monkeypatch) -> None:
    captured: dict[str, object] = {}
    _reset_schema_state()

    async def _fake_query_db_multi(sql: str, params: tuple[object, ...] = ()) -> list[list[dict[str, object]]]:
        return []

    async def _fake_query_db(sql: str, params: tuple[object, ...] = ()) -> list[dict[str, object]]:
        captured['sql'] = sql
        captured['params'] = params
        return [
            {
                'id': 8,
                'kind': 'job_failed',
                'source': 'worker',
                'title': 'Job failed: Bilibili',
                'body': 'DownloadError: temporary network error',
                'link_url': '',
                'image_url': '',
                'payload': '{"job":"bilibili"}',
                'dedupe_key': 'job_failed:bilibili:bilibili:download:BV1TEST',
                'status': 'unread',
                'markdown': '*Job failed: Bilibili*\nDownloadError: temporary network error\nOccurrences: 2',
                'disable_web_page_preview': True,
                'disable_notification': False,
                'pin': True,
                'webhook_action': WEBHOOK_ACTION_UPSERT,
                'occurrence_count': 2,
                'event_version': 3,
                'delivery_status': 'pending',
                'attempt_count': 0,
                'next_attempt_at': _NOW,
                'created_at': _NOW,
                'read_at': None,
                'delivered_at': None,
                'last_error': '',
            },
        ]

    monkeypatch.setattr(notifications_module.database, 'query_db_multi', _fake_query_db_multi)
    monkeypatch.setattr(notifications_module.database, 'query_db', _fake_query_db)

    created = asyncio.run(
        enqueue_notification(
            kind='job_failed',
            source='worker',
            title='Job failed: Bilibili',
            body='DownloadError: temporary network error',
            payload={'job': 'bilibili'},
            dedupe_key='job_failed:bilibili:bilibili:download:BV1TEST',
        ),
    )

    assert 'ON CONFLICT (dedupe_key) WHERE dedupe_key <> ' in captured['sql']
    assert captured['params'][8] == 'job_failed:bilibili:bilibili:download:BV1TEST'
    assert captured['params'][14:18] == (WEBHOOK_ACTION_UPSERT, 1, 1, DELIVERY_PENDING)
    assert created.notification_id == 8
    assert created.webhook_action == WEBHOOK_ACTION_UPSERT
    assert created.dedupe_key == 'job_failed:bilibili:bilibili:download:BV1TEST'
    assert created.occurrence_count == 2
    assert created.event_version == 3
    assert created.pin is True


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
        dedupe_key='',
        status='unread',
        markdown='*Title*\nBody',
        disable_web_page_preview=True,
        disable_notification=True,
        webhook_action=WEBHOOK_ACTION_SEND,
        occurrence_count=1,
        event_version=1,
        delivery_status='pending',
        attempt_count=0,
        next_attempt_at=_NOW,
        created_at=_NOW,
    )

    assert record.payload_json == {}
    assert record.local_image_path is None


def test_notification_record_resolves_local_image_path_from_payload() -> None:
    record = NotificationRecord(
        notification_id=1,
        kind='download_completed',
        source='stellasora',
        title='Title',
        body='Body',
        link_url='https://example.com/page',
        image_url='https://example.com/image.png',
        payload='{"image_path":"/images/demo.png"}',
        dedupe_key='',
        status='unread',
        markdown='*Title*\nBody',
        disable_web_page_preview=False,
        disable_notification=True,
        webhook_action=WEBHOOK_ACTION_SEND,
        occurrence_count=1,
        event_version=1,
        delivery_status='pending',
        attempt_count=0,
        next_attempt_at=_NOW,
        created_at=_NOW,
    )

    assert record.local_image_path == Path('/images/demo.png')


def test_image_url_is_not_rendered_as_redundant_caption_link() -> None:
    markdown, disable_web_page_preview, disable_notification, pin = notifications_module._notification_delivery_fields(
        kind='download_completed',
        title='Image',
        body='disc/Foo.png',
        link_url='',
        image_url='https://example.com/Foo.png',
        webhook_action=WEBHOOK_ACTION_SEND,
        occurrence_count=1,
    )

    assert markdown == '*Image*\ndisc/Foo\\.png'
    assert disable_web_page_preview is True
    assert disable_notification is True
    assert pin is False


def test_link_url_is_rendered_as_the_title_hyperlink() -> None:
    markdown, disable_web_page_preview, disable_notification, pin = notifications_module._notification_delivery_fields(
        kind='download_completed',
        title='Video [01]',
        body='Uploader | 1080P | 12.3 MiB | 2026-03-01',
        link_url='https://www.bilibili.com/video/BV1TEST',
        image_url='',
        webhook_action=WEBHOOK_ACTION_SEND,
        occurrence_count=1,
    )

    assert markdown == ('[*Video \\[01\\]*](https://www.bilibili.com/video/BV1TEST)\nUploader \\| 1080P \\| 12\\.3 MiB \\| 2026\\-03\\-01')
    assert disable_web_page_preview is False
    assert disable_notification is True
    assert pin is False


def test_header_is_rendered_above_the_linked_title() -> None:
    markdown, disable_web_page_preview, _, _ = notifications_module._notification_delivery_fields(
        kind='download_completed',
        header='Bilibili (toview)',
        title='- Video [01]',
        body='Uploader | 1080P | 12.3 MiB | 2026-03-01',
        link_url='https://www.bilibili.com/video/BV1TEST',
        image_url='',
        webhook_action=WEBHOOK_ACTION_SEND,
        occurrence_count=1,
    )

    assert markdown == (
        'FAV · Bilibili \\(toview\\)\n'
        '[*\\- Video \\[01\\]*](https://www.bilibili.com/video/BV1TEST)\n'
        'Uploader \\| 1080P \\| 12\\.3 MiB \\| 2026\\-03\\-01'
    )
    assert disable_web_page_preview is False


def test_link_url_escapes_only_the_characters_telegram_forbids_in_a_url() -> None:
    markdown, _, _, _ = notifications_module._notification_delivery_fields(
        kind='download_completed',
        title='Title',
        body='',
        link_url='https://example.com/a_(b)?q=1.2',
        image_url='',
        webhook_action=WEBHOOK_ACTION_SEND,
        occurrence_count=1,
    )

    assert markdown == '[*Title*](https://example.com/a_(b\\)?q=1.2)'


def test_titleless_notification_keeps_the_link_on_its_own_line() -> None:
    markdown, _, _, _ = notifications_module._notification_delivery_fields(
        kind='download_completed',
        title='',
        body='Body',
        link_url='https://example.com/video',
        image_url='',
        webhook_action=WEBHOOK_ACTION_SEND,
        occurrence_count=1,
    )

    assert markdown == 'Body\nhttps://example\\.com/video'


def test_resolve_notification_updates_existing_dedupe_key(monkeypatch) -> None:
    captured: dict[str, object] = {}
    _reset_schema_state()

    async def _fake_query_db_multi(sql: str, params: tuple[object, ...] = ()) -> list[list[dict[str, object]]]:
        return []

    async def _fake_query_db(sql: str, params: tuple[object, ...] = ()) -> list[dict[str, object]]:
        captured['sql'] = sql
        captured['params'] = params
        return [
            {
                'id': 8,
                'kind': 'job_recovered',
                'source': 'worker',
                'title': 'Job recovered: Bilibili',
                'body': 'Download succeeded: Title [BV1TEST]',
                'link_url': 'https://www.bilibili.com/video/BV1TEST',
                'image_url': '',
                'payload': '{"job":"bilibili","bvid":"BV1TEST"}',
                'dedupe_key': 'job_failed:bilibili:bilibili:download:BV1TEST',
                'status': 'unread',
                'markdown': (
                    '[*Job recovered: Bilibili*](https://www.bilibili.com/video/BV1TEST)\nDownload succeeded: Title \\[BV1TEST\\]'
                ),
                'disable_web_page_preview': False,
                'disable_notification': True,
                'pin': False,
                'webhook_action': WEBHOOK_ACTION_RESOLVE,
                'occurrence_count': 4,
                'event_version': 9,
                'delivery_status': 'pending',
                'attempt_count': 0,
                'next_attempt_at': _NOW,
                'created_at': _NOW,
                'read_at': None,
                'delivered_at': None,
                'last_error': '',
            },
        ]

    monkeypatch.setattr(notifications_module.database, 'query_db_multi', _fake_query_db_multi)
    monkeypatch.setattr(notifications_module.database, 'query_db', _fake_query_db)

    resolved = asyncio.run(
        resolve_notification(
            dedupe_key='job_failed:bilibili:bilibili:download:BV1TEST',
            kind='job_recovered',
            source='worker',
            title='Job recovered: Bilibili',
            body='Download succeeded: Title [BV1TEST]',
            link_url='https://www.bilibili.com/video/BV1TEST',
            payload={'job': 'bilibili', 'bvid': 'BV1TEST'},
        ),
    )

    assert resolved is not None
    assert 'UPDATE notifications' in captured['sql']
    assert 'WHERE dedupe_key = ?' in captured['sql']
    assert 'event_version = event_version + 1' in captured['sql']
    assert '(webhook_action <> ? OR delivery_status = ?)' in captured['sql']
    assert captured['params'][13:16] == (WEBHOOK_ACTION_RESOLVE, DELIVERY_PENDING, 0)
    assert captured['params'][16:] == ('job_failed:bilibili:bilibili:download:BV1TEST', WEBHOOK_ACTION_RESOLVE, DELIVERY_FAILED)
    assert resolved.webhook_action == WEBHOOK_ACTION_RESOLVE
    assert resolved.event_version == 9
    assert resolved.pin is False
    assert resolved.disable_notification is True


def test_reset_stale_sending_notifications_restores_expired_claims(monkeypatch) -> None:
    captured: list[tuple[str, tuple[object, ...]]] = []
    _reset_schema_state()

    async def _fake_query_db_multi(sql: str, params: tuple[object, ...] = ()) -> list[list[dict[str, object]]]:
        return []

    async def _fake_query_db(sql: str, params: tuple[object, ...] = ()) -> list[dict[str, object]]:
        captured.append((sql, params))
        return []

    monkeypatch.setattr(notifications_module.database, 'query_db_multi', _fake_query_db_multi)
    monkeypatch.setattr(notifications_module.database, 'query_db', _fake_query_db)

    asyncio.run(reset_stale_sending_notifications())

    assert 'WHERE delivery_status = ?' in captured[-1][0]
    assert 'next_attempt_at <= CURRENT_TIMESTAMP' in captured[-1][0]
    assert captured[-1][1] == (DELIVERY_PENDING, 'Notification delivery lease expired before completion', DELIVERY_SENDING)


def test_claim_next_pending_notification_uses_skip_locked_and_backfills_markdown(monkeypatch) -> None:
    _reset_schema_state()
    cursor = _FakeAsyncCursor(
        [
            {
                'id': 10,
                'kind': 'job_failed',
                'source': 'worker',
                'header': 'Bilibili',
                'title': 'Video [01]',
                'body': 'Uploader_(name)',
                'link_url': 'https://example.com/video',
                'image_url': '',
                'payload': '{"bvid":"BV1TEST"}',
                'dedupe_key': 'job_failed:bilibili:bilibili:download:BV1TEST',
                'status': 'unread',
                'created_at': _NOW,
                'read_at': None,
                'markdown': '',
                'disable_web_page_preview': True,
                'disable_notification': False,
                'pin': True,
                'webhook_action': WEBHOOK_ACTION_UPSERT,
                'occurrence_count': 3,
                'event_version': 4,
                'delivery_status': 'pending',
                'attempt_count': 0,
                'next_attempt_at': _NOW,
                'delivered_at': None,
                'last_error': '',
            },
            {
                'id': 10,
                'kind': 'job_failed',
                'source': 'worker',
                'header': 'Bilibili',
                'title': 'Video [01]',
                'body': 'Uploader_(name)',
                'link_url': 'https://example.com/video',
                'image_url': '',
                'payload': '{"bvid":"BV1TEST"}',
                'dedupe_key': 'job_failed:bilibili:bilibili:download:BV1TEST',
                'status': 'unread',
                'created_at': _NOW,
                'read_at': None,
                'markdown': 'FAV · Bilibili\n[*Video \\[01\\]*](https://example.com/video)\nUploader\\_\\(name\\)\nOccurrences: 3',
                'disable_web_page_preview': False,
                'disable_notification': False,
                'pin': True,
                'webhook_action': WEBHOOK_ACTION_UPSERT,
                'occurrence_count': 3,
                'event_version': 5,
                'delivery_status': 'sending',
                'attempt_count': 0,
                'next_attempt_at': _NOW,
                'delivered_at': None,
                'last_error': '',
            },
        ],
    )

    async def _fake_query_db_multi(sql: str, params: tuple[object, ...] = ()) -> list[list[dict[str, object]]]:
        return []

    async def _fake_query_db(sql: str, params: tuple[object, ...] = ()) -> list[dict[str, object]]:
        return []

    @asynccontextmanager
    async def _fake_connection():
        yield _FakeAsyncConnection(cursor)

    monkeypatch.setattr(notifications_module.database, 'query_db_multi', _fake_query_db_multi)
    monkeypatch.setattr(notifications_module.database, 'query_db', _fake_query_db)
    monkeypatch.setattr(notifications_module.database, 'connection', _fake_connection)

    claimed = asyncio.run(claim_next_pending_notification())

    assert claimed is not None
    assert claimed.notification_id == 10
    assert claimed.event_version == 5
    assert claimed.delivery_status == DELIVERY_SENDING
    assert claimed.markdown == 'FAV · Bilibili\n[*Video \\[01\\]*](https://example.com/video)\nUploader\\_\\(name\\)\nOccurrences: 3'
    assert 'FOR UPDATE SKIP LOCKED' in cursor.executed[0][0]
    assert 'event_version = event_version + 1' in cursor.executed[1][0]
    assert cursor.executed[1][1] == (
        DELIVERY_SENDING,
        'FAV · Bilibili\n[*Video \\[01\\]*](https://example.com/video)\nUploader\\_\\(name\\)\nOccurrences: 3',
        False,
        False,
        True,
        notifications_module._SENDING_LEASE_SECONDS,
        10,
        DELIVERY_PENDING,
        4,
    )


def test_mark_notification_delivered_updates_status(monkeypatch) -> None:
    captured: list[tuple[str, tuple[object, ...]]] = []
    _reset_schema_state()

    async def _fake_query_db_multi(sql: str, params: tuple[object, ...] = ()) -> list[list[dict[str, object]]]:
        return []

    async def _fake_query_db(sql: str, params: tuple[object, ...] = ()) -> list[dict[str, object]]:
        captured.append((sql, params))
        return []

    monkeypatch.setattr(notifications_module.database, 'query_db_multi', _fake_query_db_multi)
    monkeypatch.setattr(notifications_module.database, 'query_db', _fake_query_db)

    asyncio.run(mark_notification_delivered(12, event_version=6))

    assert captured[-1][1] == (DELIVERY_DELIVERED, 'read', 12, DELIVERY_SENDING, 6)


def test_mark_notification_retry_updates_attempt_count(monkeypatch) -> None:
    captured: list[tuple[str, tuple[object, ...]]] = []
    _reset_schema_state()

    async def _fake_query_db_multi(sql: str, params: tuple[object, ...] = ()) -> list[list[dict[str, object]]]:
        return []

    async def _fake_query_db(sql: str, params: tuple[object, ...] = ()) -> list[dict[str, object]]:
        captured.append((sql, params))
        return []

    monkeypatch.setattr(notifications_module.database, 'query_db_multi', _fake_query_db_multi)
    monkeypatch.setattr(notifications_module.database, 'query_db', _fake_query_db)

    asyncio.run(mark_notification_retry(13, event_version=7, attempt_count=2, error_message='HTTP 503'))

    params = captured[-1][1]
    assert params[0] == DELIVERY_PENDING
    assert params[1] == 2
    assert params[3] == 'HTTP 503'
    assert params[4] == 13
    assert params[5] == DELIVERY_SENDING
    assert params[6] == 7


def test_mark_notification_failed_updates_terminal_state(monkeypatch) -> None:
    captured: list[tuple[str, tuple[object, ...]]] = []
    _reset_schema_state()

    async def _fake_query_db_multi(sql: str, params: tuple[object, ...] = ()) -> list[list[dict[str, object]]]:
        return []

    async def _fake_query_db(sql: str, params: tuple[object, ...] = ()) -> list[dict[str, object]]:
        captured.append((sql, params))
        return []

    monkeypatch.setattr(notifications_module.database, 'query_db_multi', _fake_query_db_multi)
    monkeypatch.setattr(notifications_module.database, 'query_db', _fake_query_db)

    asyncio.run(mark_notification_failed(14, event_version=8, attempt_count=4, error_message='HTTP 400'))

    assert captured[-1][1] == (DELIVERY_FAILED, 4, 'HTTP 400', 14, DELIVERY_SENDING, 8)


def test_retry_delay_seconds_uses_exponential_backoff_with_cap() -> None:
    assert retry_delay_seconds(1) == 30
    assert retry_delay_seconds(2) == 60
    assert retry_delay_seconds(3) == 120
    assert retry_delay_seconds(6) == 600
    assert retry_delay_seconds(10) == 600
