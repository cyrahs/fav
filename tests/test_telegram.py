# ruff: noqa: INP001, S101, ANN001, SLF001

import asyncio
from pathlib import Path
from types import SimpleNamespace

import src.web.telegram as telegram_module
from src.web.telegram import Telegram


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
