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


def test_update_channel_sends_notification(tmp_path, monkeypatch) -> None:
    tg = _make_telegram()
    notifications: list[dict[str, object]] = []
    monkeypatch.setattr(telegram_module.cfg, 'path', tmp_path)

    msg = SimpleNamespace(id=456)
    queries: list[tuple[str, tuple | None]] = []

    async def _fake_get_videos(_channel: object) -> list[dict[str, object]]:
        return [{'msg': msg, 'filename': 'My Video'}]

    async def _fake_get_downloaded_ids(_channel_id: int) -> list[int]:
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

    asyncio.run(tg.update_channel(123))

    assert notifications == [
        {
            'kind': 'download_completed',
            'source': 'telegram',
            'title': 'Telegram: My Video',
            'body': 'Channel demo_channel | Message ID 456',
            'payload': {
                'channel_name': 'demo_channel',
                'message_id': 456,
                'saved_path': str(tmp_path / 'demo_channel' / 'My Video [456].mp4'),
            },
        },
    ]
    assert sum('INSERT INTO telegram' in sql for sql, _ in queries) == 1


def test_update_channel_continues_when_notification_fails(tmp_path, monkeypatch) -> None:
    tg = _make_telegram()
    monkeypatch.setattr(telegram_module.cfg, 'path', tmp_path)

    msg = SimpleNamespace(id=456)
    queries: list[tuple[str, tuple | None]] = []

    async def _fake_get_videos(_channel: object) -> list[dict[str, object]]:
        return [{'msg': msg, 'filename': 'My Video'}]

    async def _fake_get_downloaded_ids(_channel_id: int) -> list[int]:
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

    asyncio.run(tg.update_channel(123))

    assert sum('INSERT INTO telegram' in sql for sql, _ in queries) == 1
