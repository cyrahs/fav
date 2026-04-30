# ruff: noqa: INP001, S101, ANN001, ANN002, ANN003, ANN202, EM101, SLF001, TRY003

import asyncio
from contextlib import asynccontextmanager
from pathlib import Path
from types import SimpleNamespace

import pytest

import src.web.telegram as telegram_module
from src.web.telegram import Telegram, TelegramMediaEntry, TelegramSessionUnauthorizedError

_FALLBACK_DOWNLOADED_MESSAGE_ID = 789


class _DummyClient:
    async def get_entity(self, _peer: object) -> SimpleNamespace:
        return SimpleNamespace(username='demo_channel')


class _IterClient(_DummyClient):
    def __init__(self, messages: list[SimpleNamespace]) -> None:
        self._messages = messages

    def iter_messages(
        self,
        _channel: object,
        *,
        limit: int | None = None,
        min_id: int = 0,
        reverse: bool = False,
        wait_time: float | None = None,  # noqa: ARG002
    ):
        messages = [msg for msg in self._messages if int(getattr(msg, 'id', 0) or 0) > min_id]
        if not reverse:
            messages.reverse()
        if limit is not None:
            messages = messages[:limit]

        async def _iter_messages():
            for msg in messages:
                yield msg

        return _iter_messages()


class _DummyTmpDir:
    def cleanup(self) -> None:
        return None


def _make_telegram() -> Telegram:
    tg = Telegram.__new__(Telegram)
    tg._tmp_dir = _DummyTmpDir()
    tg.client = _DummyClient()
    return tg


def _make_account_telegram() -> Telegram:
    tg = Telegram.__new__(Telegram)
    tg._tmp_dir = _DummyTmpDir()
    tg.client = None
    return tg


def _account(name: str = 'default', channel_path: Path | None = None) -> telegram_module.TelegramAccount:
    if channel_path is None:
        channel_path = Path('./collection/telegram/demo_channel')
    return telegram_module.TelegramAccount(
        name=name,
        channels=[telegram_module.TelegramChannel(id=123, path=channel_path)],
        api_id=1,
        api_hash='hash',
        session_path=Path('./session'),
    )


def _media_entry(msg: object, filename: str = 'My Video', media_type: telegram_module.TelegramMediaType = 'video') -> TelegramMediaEntry:
    return TelegramMediaEntry(msg=msg, filename=filename, media_type=media_type)


def _scan(
    *items: tuple[object, str, telegram_module.TelegramMediaType],
    max_message_id: int | None = None,
) -> telegram_module.TelegramChannelScan:
    entries = [_media_entry(msg, filename, media_type) for msg, filename, media_type in items]
    if max_message_id is None:
        max_message_id = max((int(msg.id) for msg, _, _ in items), default=0)
    return telegram_module.TelegramChannelScan(
        media=entries,
        max_message_id=max_message_id,
        scanned_message_count=len(items),
    )


def _message(
    message_id: int,
    **updates: object,
) -> SimpleNamespace:
    data: dict[str, object] = {
        'message': '',
        'grouped_id': None,
        'media': None,
        'video': False,
        'photo': False,
        'document': None,
        'sticker': False,
    }
    data.update(updates)
    return SimpleNamespace(id=message_id, **data)


def _document_media(mime_type: str, attributes: list[object] | None = None) -> telegram_module.MessageMediaDocument:
    document = SimpleNamespace(mime_type=mime_type, attributes=attributes or [])
    return telegram_module.MessageMediaDocument(document=document)


def test_update_channel_sends_notification(tmp_path, monkeypatch) -> None:
    tg = _make_telegram()
    notifications: list[dict[str, object]] = []
    account = _account(channel_path=tmp_path / 'demo_channel')

    msg = SimpleNamespace(id=456)
    queries: list[tuple[str, tuple | None]] = []

    async def _fake_scan_media(*_args, **_kwargs) -> telegram_module.TelegramChannelScan:
        return _scan((msg, 'My Video', 'video'))

    async def _fake_get_downloaded_ids(_account_name: str, _channel_id: int, *, min_message_id: int = 0) -> list[int]:  # noqa: ARG001
        return []

    async def _fake_download(_msg: object, _dst: Path, _title: str, media_type: telegram_module.TelegramMediaType = 'video') -> Path:
        assert media_type == 'video'
        return tmp_path / 'demo_channel' / 'My Video [456].mp4'

    async def _fake_query_db(sql: str, params: tuple | None = None) -> list[dict[str, str]]:
        queries.append((sql, params))
        return []

    monkeypatch.setattr(tg, 'scan_media', _fake_scan_media)
    monkeypatch.setattr(tg, 'get_downloaded_ids', _fake_get_downloaded_ids)
    monkeypatch.setattr(tg, 'download', _fake_download)
    monkeypatch.setattr(telegram_module.database, 'query_db', _fake_query_db)

    async def _fake_enqueue_notification(**payload) -> None:
        notifications.append(payload)

    monkeypatch.setattr(telegram_module, 'enqueue_notification', _fake_enqueue_notification)

    asyncio.run(tg.update_channel(account.channels[0], account))

    assert notifications == [
        {
            'kind': 'download_completed',
            'source': 'telegram',
            'title': 'Telegram: My Video',
            'body': 'Channel demo_channel | Message ID 456',
            'payload': {
                'account_name': 'default',
                'channel_name': 'demo_channel',
                'message_id': 456,
                'saved_path': str(tmp_path / 'demo_channel' / 'My Video [456].mp4'),
            },
        },
    ]
    assert sum('INSERT INTO telegram (account_name' in sql for sql, _ in queries) == 1
    insert_queries = [params for sql, params in queries if 'INSERT INTO telegram (account_name' in sql]
    assert insert_queries == [('default', 456, 123, 'My Video', 'demo_channel', 'video')]


def test_update_channel_continues_when_notification_fails(tmp_path, monkeypatch) -> None:
    tg = _make_telegram()
    account = _account(channel_path=tmp_path / 'demo_channel')

    msg = SimpleNamespace(id=456)
    queries: list[tuple[str, tuple | None]] = []

    async def _fake_scan_media(*_args, **_kwargs) -> telegram_module.TelegramChannelScan:
        return _scan((msg, 'My Video', 'video'))

    async def _fake_get_downloaded_ids(_account_name: str, _channel_id: int, *, min_message_id: int = 0) -> list[int]:  # noqa: ARG001
        return []

    async def _fake_download(_msg: object, _dst: Path, _title: str, media_type: telegram_module.TelegramMediaType = 'video') -> Path:
        assert media_type == 'video'
        return tmp_path / 'demo_channel' / 'My Video [456].mp4'

    async def _fake_query_db(sql: str, params: tuple | None = None) -> list[dict[str, str]]:
        queries.append((sql, params))
        return []

    monkeypatch.setattr(tg, 'scan_media', _fake_scan_media)
    monkeypatch.setattr(tg, 'get_downloaded_ids', _fake_get_downloaded_ids)
    monkeypatch.setattr(tg, 'download', _fake_download)
    monkeypatch.setattr(telegram_module.database, 'query_db', _fake_query_db)

    async def _failing_enqueue_notification(**_payload) -> None:
        msg = 'notify failed'
        raise RuntimeError(msg)

    monkeypatch.setattr(telegram_module, 'enqueue_notification', _failing_enqueue_notification)

    asyncio.run(tg.update_channel(account.channels[0], account))

    assert sum('INSERT INTO telegram (account_name' in sql for sql, _ in queries) == 1


def test_update_channel_uses_explicit_channel_path(tmp_path, monkeypatch) -> None:
    tg = _make_telegram()

    msg = SimpleNamespace(id=456)
    channel = telegram_module.TelegramChannel(id=123, path=tmp_path / 'custom-channel')
    account = telegram_module.TelegramAccount(
        name='alt',
        channels=[channel],
        api_id=1,
        api_hash='hash',
        session_path=Path('./session'),
    )

    async def _fake_scan_media(*_args, **_kwargs) -> telegram_module.TelegramChannelScan:
        return _scan((msg, 'My Video', 'video'))

    async def _fake_get_downloaded_ids(_account_name: str, _channel_id: int, *, min_message_id: int = 0) -> list[int]:  # noqa: ARG001
        return []

    download_calls: list[tuple[object, Path, str, telegram_module.TelegramMediaType]] = []

    async def _fake_download(_msg: object, dst: Path, title: str, media_type: telegram_module.TelegramMediaType = 'video') -> Path:
        download_calls.append((_msg, dst, title, media_type))
        return dst / 'My Video [456].mp4'

    async def _fake_query_db(_sql: str, _params: tuple | None = None) -> list[dict[str, str]]:
        return []

    async def _fake_enqueue_notification(**_payload) -> None:
        return None

    monkeypatch.setattr(tg, 'scan_media', _fake_scan_media)
    monkeypatch.setattr(tg, 'get_downloaded_ids', _fake_get_downloaded_ids)
    monkeypatch.setattr(tg, 'download', _fake_download)
    monkeypatch.setattr(telegram_module.database, 'query_db', _fake_query_db)
    monkeypatch.setattr(telegram_module, 'enqueue_notification', _fake_enqueue_notification)

    asyncio.run(tg.update_channel(channel, account))

    assert download_calls == [(msg, tmp_path / 'custom-channel', 'My Video', 'video')]


def test_get_media_uses_configured_media_types() -> None:
    tg = _make_telegram()
    tg.client = _IterClient(
        [
            _message(message_id=1, message='Video Caption', video=True),
            _message(message_id=2, message='Image Caption', media=telegram_module.MessageMediaPhoto(photo=object())),
        ],
    )

    video_only = asyncio.run(tg.get_media(SimpleNamespace(), ['video']))
    video_and_image = asyncio.run(tg.get_media(SimpleNamespace(), ['video', 'image']))

    assert [(item.msg.id, item.filename, item.media_type) for item in video_only] == [(1, 'Video Caption', 'video')]
    assert [(item.msg.id, item.filename, item.media_type) for item in video_and_image] == [
        (1, 'Video Caption', 'video'),
        (2, 'Image Caption', 'image'),
    ]


def test_get_media_supports_image_documents_and_skips_stickers() -> None:
    sticker_attr = type('DocumentAttributeSticker', (), {})()
    tg = _make_telegram()
    tg.client = _IterClient(
        [
            _message(message_id=1, media=_document_media('image/png')),
            _message(message_id=2, media=_document_media('image/webp', [sticker_attr])),
            _message(message_id=3, media=_document_media('application/octet-stream')),
        ],
    )

    media = asyncio.run(tg.get_media(SimpleNamespace(), ['image']))

    assert [(item.msg.id, item.filename, item.media_type) for item in media] == [(1, 'image_1', 'image')]


def test_get_media_uses_album_caption_for_images() -> None:
    tg = _make_telegram()
    tg.client = _IterClient(
        [
            _message(message_id=1, message='Album Caption', grouped_id=99, media=telegram_module.MessageMediaPhoto(photo=object())),
            _message(message_id=2, grouped_id=99, media=_document_media('image/png')),
        ],
    )

    media = asyncio.run(tg.get_media(SimpleNamespace(), ['image']))

    assert [(item.msg.id, item.filename, item.media_type) for item in media] == [
        (1, 'Album Caption-1', 'image'),
        (2, 'Album Caption-2', 'image'),
    ]


def test_get_media_skips_web_preview_photos() -> None:
    tg = _make_telegram()
    tg.client = _IterClient(
        [
            _message(message_id=1, message='Link preview', photo=object()),
        ],
    )

    media = asyncio.run(tg.get_media(SimpleNamespace(), ['image']))

    assert media == []


def test_get_media_skips_web_preview_image_documents() -> None:
    tg = _make_telegram()
    tg.client = _IterClient(
        [
            _message(message_id=1, message='Link preview', document=SimpleNamespace(mime_type='image/png', attributes=[])),
        ],
    )

    media = asyncio.run(tg.get_media(SimpleNamespace(), ['image']))

    assert media == []


def test_get_channel_state_falls_back_to_downloaded_id_when_state_cursor_is_zero(monkeypatch) -> None:
    tg = _make_telegram()
    queries: list[tuple[str, tuple | None]] = []

    async def _fake_query_db(sql: str, params: tuple | None = None) -> list[dict[str, object]]:
        queries.append((sql, params))
        if 'FROM telegram_channel_state' in sql:
            return [{'last_scanned_message_id': 0, 'cooldown_remaining_seconds': 0}]
        if 'MAX(message_id)' in sql:
            return [{'message_id': _FALLBACK_DOWNLOADED_MESSAGE_ID}]
        msg = f'unexpected query: {sql}'
        raise AssertionError(msg)

    monkeypatch.setattr(telegram_module.database, 'query_db', _fake_query_db)

    state = asyncio.run(tg.get_channel_state('default', 123))

    assert state.last_scanned_message_id == _FALLBACK_DOWNLOADED_MESSAGE_ID
    assert state.has_scan_state is True
    assert state.cooldown_remaining_seconds == 0
    assert [params for _, params in queries] == [('default', 123), ('default', 123)]


def test_update_channel_uses_incremental_scan_limit_and_download_cap(tmp_path, monkeypatch) -> None:
    tg = _make_telegram()
    account = _account(channel_path=tmp_path / 'demo_channel')
    msg_101 = SimpleNamespace(id=101)
    msg_102 = SimpleNamespace(id=102)
    msg_103 = SimpleNamespace(id=103)
    scan_calls: list[tuple[list[telegram_module.TelegramMediaType], int, int | None, float | None, bool]] = []
    download_calls: list[tuple[int, str, telegram_module.TelegramMediaType]] = []
    sleeps: list[float] = []
    cursors: list[int] = []

    monkeypatch.setattr(
        telegram_module,
        'cfg',
        telegram_module.cfg.model_copy(
            update={
                'scan_limit': 10,
                'download_limit_per_channel': 2,
                'download_delay_seconds': 5.0,
                'channel_cooldown_seconds': 0.0,
                'history_wait_seconds': 0.5,
            },
        ),
    )

    async def _fake_get_channel_state(_account_name: str, _channel_id: int) -> telegram_module.TelegramChannelState:
        return telegram_module.TelegramChannelState(last_scanned_message_id=100, has_scan_state=True)

    async def _fake_scan_media(
        _channel: object,
        media_types: list[telegram_module.TelegramMediaType],
        *,
        min_message_id: int = 0,
        limit: int | None = None,
        wait_time: float | None = None,
        newest_window: bool = False,
    ) -> telegram_module.TelegramChannelScan:
        scan_calls.append((media_types, min_message_id, limit, wait_time, newest_window))
        return _scan((msg_101, 'One', 'video'), (msg_102, 'Two', 'image'), (msg_103, 'Three', 'video'))

    async def _fake_get_downloaded_ids(_account_name: str, _channel_id: int, *, min_message_id: int = 0) -> list[int]:  # noqa: ARG001
        return []

    async def _fake_download(
        msg: object,
        dst: Path,
        title: str,
        media_type: telegram_module.TelegramMediaType = 'video',
    ) -> Path:
        download_calls.append((msg.id, title, media_type))
        return dst / f'{title} [{msg.id}].mp4'

    async def _fake_query_db(_sql: str, _params: tuple | None = None) -> list[dict[str, str]]:
        return []

    async def _fake_mark_channel_downloaded(_account_name: str, _channel_id: int) -> None:
        return None

    async def _fake_notify_download(**_kwargs) -> None:
        return None

    async def _fake_set_cursor(_account_name: str, _channel_id: int, message_id: int) -> None:
        cursors.append(message_id)

    async def _fake_sleep(seconds: float) -> None:
        sleeps.append(seconds)

    monkeypatch.setattr(tg, 'get_channel_state', _fake_get_channel_state)
    monkeypatch.setattr(tg, 'scan_media', _fake_scan_media)
    monkeypatch.setattr(tg, 'get_downloaded_ids', _fake_get_downloaded_ids)
    monkeypatch.setattr(tg, 'download', _fake_download)
    monkeypatch.setattr(tg, 'mark_channel_downloaded', _fake_mark_channel_downloaded)
    monkeypatch.setattr(tg, '_notify_download', _fake_notify_download)
    monkeypatch.setattr(tg, 'set_channel_last_scanned_message_id', _fake_set_cursor)
    monkeypatch.setattr(tg, '_sleep', _fake_sleep)
    monkeypatch.setattr(telegram_module.database, 'query_db', _fake_query_db)

    asyncio.run(tg.update_channel(account.channels[0], account))

    assert scan_calls == [(['video'], 100, 10, 0.5, False)]
    assert download_calls == [(101, 'One', 'video'), (102, 'Two', 'image')]
    assert sleeps == [5.0]
    assert cursors == [102]


def test_update_channel_uses_latest_window_when_no_cursor(tmp_path, monkeypatch) -> None:
    tg = _make_telegram()
    account = _account(channel_path=tmp_path / 'demo_channel')
    scan_calls: list[tuple[int, bool]] = []
    cursors: list[int] = []

    async def _fake_get_channel_state(_account_name: str, _channel_id: int) -> telegram_module.TelegramChannelState:
        return telegram_module.TelegramChannelState(last_scanned_message_id=0, has_scan_state=False)

    async def _fake_scan_media(
        _channel: object,
        _media_types: list[telegram_module.TelegramMediaType],
        *,
        min_message_id: int = 0,
        limit: int | None = None,  # noqa: ARG001
        wait_time: float | None = None,  # noqa: ARG001
        newest_window: bool = False,
    ) -> telegram_module.TelegramChannelScan:
        scan_calls.append((min_message_id, newest_window))
        return telegram_module.TelegramChannelScan(media=[], max_message_id=456, scanned_message_count=10)

    async def _fake_set_cursor(_account_name: str, _channel_id: int, message_id: int) -> None:
        cursors.append(message_id)

    async def _fake_get_downloaded_ids(_account_name: str, _channel_id: int, *, min_message_id: int = 0) -> list[int]:  # noqa: ARG001
        return []

    monkeypatch.setattr(tg, 'get_channel_state', _fake_get_channel_state)
    monkeypatch.setattr(tg, 'scan_media', _fake_scan_media)
    monkeypatch.setattr(tg, 'set_channel_last_scanned_message_id', _fake_set_cursor)
    monkeypatch.setattr(tg, 'get_downloaded_ids', _fake_get_downloaded_ids)

    asyncio.run(tg.update_channel(account.channels[0], account))

    assert scan_calls == [(0, True)]
    assert cursors == [456]


def test_update_channel_skips_when_cooling_down(tmp_path, monkeypatch) -> None:
    tg = _make_telegram()
    account = _account(channel_path=tmp_path / 'demo_channel')

    async def _fake_get_channel_state(_account_name: str, _channel_id: int) -> telegram_module.TelegramChannelState:
        return telegram_module.TelegramChannelState(
            last_scanned_message_id=100,
            has_scan_state=True,
            cooldown_remaining_seconds=60,
        )

    async def _unexpected_scan_media(*_args, **_kwargs) -> telegram_module.TelegramChannelScan:
        raise AssertionError('scan_media should not be called during cooldown')

    monkeypatch.setattr(tg, 'get_channel_state', _fake_get_channel_state)
    monkeypatch.setattr(tg, 'scan_media', _unexpected_scan_media)

    asyncio.run(tg.update_channel(account.channels[0], account))


def test_update_account_connects_without_interactive_start(tmp_path, monkeypatch) -> None:
    tg = _make_account_telegram()
    account = _account(channel_path=tmp_path / 'demo_channel')
    account = account.model_copy(update={'session_path': tmp_path / 'session'})
    calls: list[str] = []
    client_kwargs: list[dict[str, object]] = []

    class _FakeTelegramClient:
        def __init__(self, session_path: Path, api_id: int, api_hash: str, **kwargs) -> None:
            calls.append(f'init:{session_path}:{api_id}:{api_hash}')
            client_kwargs.append(kwargs)

        async def connect(self) -> None:
            calls.append('connect')

        async def is_user_authorized(self) -> bool:
            calls.append('authorized')
            return True

        async def start(self) -> None:
            raise AssertionError('start should not be called')

        async def disconnect(self) -> None:
            calls.append('disconnect')

    @asynccontextmanager
    async def _fake_lock(name: str):
        calls.append(f'lock:{name}')
        yield True

    async def _fake_update_channel(channel: object, update_account: telegram_module.TelegramAccount) -> None:
        calls.append(f'channel:{channel.id}:{update_account.name}')

    monkeypatch.setattr(telegram_module, 'TelegramClient', _FakeTelegramClient)
    monkeypatch.setattr(telegram_module.database, 'advisory_lock', _fake_lock)
    monkeypatch.setattr(tg, 'update_channel', _fake_update_channel)

    asyncio.run(tg.update_account(account))

    assert 'connect' in calls
    assert 'authorized' in calls
    assert 'disconnect' in calls
    assert client_kwargs == [{'flood_sleep_threshold': telegram_module.cfg.flood_sleep_threshold_seconds, 'receive_updates': False}]
    assert f'lock:telegram-session:{(tmp_path / "session.session").resolve()}' in calls
    assert calls[-2:] == ['channel:123:default', 'disconnect']


def test_account_lock_name_uses_effective_telethon_session_file(tmp_path) -> None:
    plain_account = _account('default', tmp_path / 'demo_channel').model_copy(update={'session_path': tmp_path / 'shared'})
    suffixed_account = _account('other', tmp_path / 'demo_channel').model_copy(update={'session_path': tmp_path / 'shared.session'})

    assert Telegram._account_lock_name(plain_account) == Telegram._account_lock_name(suffixed_account)
    assert Telegram._account_lock_name(plain_account) == f'telegram-session:{(tmp_path / "shared.session").resolve()}'


def test_update_account_raises_clear_error_when_session_unauthorized(tmp_path, monkeypatch) -> None:
    tg = _make_account_telegram()
    account = _account(channel_path=tmp_path / 'demo_channel')
    account = account.model_copy(update={'session_path': tmp_path / 'session'})
    calls: list[str] = []

    class _FakeTelegramClient:
        def __init__(self, *_args, **_kwargs) -> None:
            return None

        async def connect(self) -> None:
            calls.append('connect')

        async def is_user_authorized(self) -> bool:
            calls.append('authorized')
            return False

        async def start(self) -> None:
            raise AssertionError('start should not be called')

        async def disconnect(self) -> None:
            calls.append('disconnect')

    @asynccontextmanager
    async def _fake_lock(_name: str):
        yield True

    async def _unexpected_update_channel(*_args) -> None:
        raise AssertionError('update_channel should not be called')

    monkeypatch.setattr(telegram_module, 'TelegramClient', _FakeTelegramClient)
    monkeypatch.setattr(telegram_module.database, 'advisory_lock', _fake_lock)
    monkeypatch.setattr(tg, 'update_channel', _unexpected_update_channel)

    with pytest.raises(TelegramSessionUnauthorizedError, match=r'account=default'):
        asyncio.run(tg.update_account(account))

    assert calls == ['connect', 'authorized', 'disconnect']


def test_update_account_skips_when_session_lock_is_held(tmp_path, monkeypatch) -> None:
    tg = _make_account_telegram()
    account = _account(channel_path=tmp_path / 'demo_channel')

    class _UnexpectedTelegramClient:
        def __init__(self, *_args) -> None:
            raise AssertionError('TelegramClient should not be created when lock is held')

    @asynccontextmanager
    async def _fake_lock(_name: str):
        yield False

    monkeypatch.setattr(telegram_module, 'TelegramClient', _UnexpectedTelegramClient)
    monkeypatch.setattr(telegram_module.database, 'advisory_lock', _fake_lock)

    asyncio.run(tg.update_account(account))
