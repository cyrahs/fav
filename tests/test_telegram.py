# ruff: noqa: INP001, S101, ANN001, ANN002, ANN202, EM101, SLF001, TRY003

import asyncio
from contextlib import asynccontextmanager
from pathlib import Path
from types import SimpleNamespace

import pytest

import src.web.telegram as telegram_module
from src.web.telegram import Telegram, TelegramSessionUnauthorizedError


class _DummyClient:
    async def get_entity(self, _peer: object) -> SimpleNamespace:
        return SimpleNamespace(username='demo_channel')


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


def test_update_channel_sends_notification(tmp_path, monkeypatch) -> None:
    tg = _make_telegram()
    notifications: list[dict[str, object]] = []
    account = _account(channel_path=tmp_path / 'demo_channel')

    msg = SimpleNamespace(id=456)
    queries: list[tuple[str, tuple | None]] = []

    async def _fake_get_videos(_channel: object) -> list[dict[str, object]]:
        return [{'msg': msg, 'filename': 'My Video'}]

    async def _fake_get_downloaded_ids(_account_name: str, _channel_id: int) -> list[int]:
        return []

    async def _fake_download(_msg: object, _dst: Path, _title: str) -> Path:
        return tmp_path / 'demo_channel' / 'My Video [456].mp4'

    async def _fake_query_db(sql: str, params: tuple | None = None) -> list[dict[str, str]]:
        queries.append((sql, params))
        return []

    monkeypatch.setattr(tg, 'get_videos', _fake_get_videos)
    monkeypatch.setattr(tg, 'get_downloaded_ids', _fake_get_downloaded_ids)
    monkeypatch.setattr(tg, 'download', _fake_download)
    monkeypatch.setattr(telegram_module.database, 'query_db', _fake_query_db)

    async def _fake_enqueue_notification(**payload) -> None:  # noqa: ANN003
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
    assert sum('INSERT INTO telegram' in sql for sql, _ in queries) == 1
    assert queries[-1][1] == ('default', 456, 123, 'My Video', 'demo_channel')


def test_update_channel_continues_when_notification_fails(tmp_path, monkeypatch) -> None:
    tg = _make_telegram()
    account = _account(channel_path=tmp_path / 'demo_channel')

    msg = SimpleNamespace(id=456)
    queries: list[tuple[str, tuple | None]] = []

    async def _fake_get_videos(_channel: object) -> list[dict[str, object]]:
        return [{'msg': msg, 'filename': 'My Video'}]

    async def _fake_get_downloaded_ids(_account_name: str, _channel_id: int) -> list[int]:
        return []

    async def _fake_download(_msg: object, _dst: Path, _title: str) -> Path:
        return tmp_path / 'demo_channel' / 'My Video [456].mp4'

    async def _fake_query_db(sql: str, params: tuple | None = None) -> list[dict[str, str]]:
        queries.append((sql, params))
        return []

    monkeypatch.setattr(tg, 'get_videos', _fake_get_videos)
    monkeypatch.setattr(tg, 'get_downloaded_ids', _fake_get_downloaded_ids)
    monkeypatch.setattr(tg, 'download', _fake_download)
    monkeypatch.setattr(telegram_module.database, 'query_db', _fake_query_db)

    async def _failing_enqueue_notification(**_payload) -> None:  # noqa: ANN003
        msg = 'notify failed'
        raise RuntimeError(msg)

    monkeypatch.setattr(telegram_module, 'enqueue_notification', _failing_enqueue_notification)

    asyncio.run(tg.update_channel(account.channels[0], account))

    assert sum('INSERT INTO telegram' in sql for sql, _ in queries) == 1


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

    async def _fake_get_videos(_channel: object) -> list[dict[str, object]]:
        return [{'msg': msg, 'filename': 'My Video'}]

    async def _fake_get_downloaded_ids(_account_name: str, _channel_id: int) -> list[int]:
        return []

    download_calls: list[tuple[object, Path, str]] = []

    async def _fake_download(_msg: object, dst: Path, title: str) -> Path:
        download_calls.append((_msg, dst, title))
        return dst / 'My Video [456].mp4'

    async def _fake_query_db(_sql: str, _params: tuple | None = None) -> list[dict[str, str]]:
        return []

    async def _fake_enqueue_notification(**_payload) -> None:  # noqa: ANN003
        return None

    monkeypatch.setattr(tg, 'get_videos', _fake_get_videos)
    monkeypatch.setattr(tg, 'get_downloaded_ids', _fake_get_downloaded_ids)
    monkeypatch.setattr(tg, 'download', _fake_download)
    monkeypatch.setattr(telegram_module.database, 'query_db', _fake_query_db)
    monkeypatch.setattr(telegram_module, 'enqueue_notification', _fake_enqueue_notification)

    asyncio.run(tg.update_channel(channel, account))

    assert download_calls == [(msg, tmp_path / 'custom-channel', 'My Video')]


def test_update_account_connects_without_interactive_start(tmp_path, monkeypatch) -> None:
    tg = _make_account_telegram()
    account = _account(channel_path=tmp_path / 'demo_channel')
    account = account.model_copy(update={'session_path': tmp_path / 'session'})
    calls: list[str] = []

    class _FakeTelegramClient:
        def __init__(self, session_path: Path, api_id: int, api_hash: str) -> None:
            calls.append(f'init:{session_path}:{api_id}:{api_hash}')

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
        def __init__(self, *_args) -> None:
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
