# ruff: noqa: INP001, S101, ANN001, SLF001

import asyncio
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import src.web.telegram as telegram_module
from src.web.telegram import Telegram


class _RecordingNotifier:
    def __init__(self) -> None:
        self.messages: list[str] = []

    async def send(self, message: str) -> None:
        self.messages.append(message)


class _FailingNotifier:
    async def send(self, message: str) -> None:  # noqa: ARG002
        msg = 'notify failed'
        raise RuntimeError(msg)


class _DummyClient:
    async def get_entity(self, _peer: object) -> SimpleNamespace:
        return SimpleNamespace(username='demo_channel')


class _DummyTmpDir:
    def cleanup(self) -> None:
        return None


def _make_telegram(notifier: Any) -> Telegram:
    tg = Telegram.__new__(Telegram)
    tg.notifier = notifier
    tg._tmp_dir = _DummyTmpDir()
    tg.client = _DummyClient()
    return tg


def test_update_channel_sends_notification(tmp_path, monkeypatch) -> None:
    notifier = _RecordingNotifier()
    tg = _make_telegram(notifier)
    monkeypatch.setattr(telegram_module.cfg, 'path', tmp_path)

    msg = SimpleNamespace(id=456)
    queries: list[tuple[str, tuple | None]] = []

    async def _fake_get_videos(_channel: object) -> list[dict[str, object]]:
        return [{'msg': msg, 'filename': 'My Video'}]

    async def _fake_get_downloaded_ids(_channel_id: int) -> list[int]:
        return []

    async def _fake_download(_msg: object, _dst: Path, _title: str) -> Path:
        return tmp_path / 'demo_channel' / 'My Video [456].mp4'

    async def _fake_query_d1(sql: str, params: tuple | None = None) -> list[dict[str, str]]:
        queries.append((sql, params))
        return []

    monkeypatch.setattr(tg, 'get_videos', _fake_get_videos)
    monkeypatch.setattr(tg, 'get_downloaded_ids', _fake_get_downloaded_ids)
    monkeypatch.setattr(tg, 'download', _fake_download)
    monkeypatch.setattr(telegram_module.cloudflare, 'query_d1', _fake_query_d1)

    asyncio.run(tg.update_channel(123))

    assert len(notifier.messages) == 1
    assert 'Telegram download completed' in notifier.messages[0]
    assert 'Channel: demo_channel' in notifier.messages[0]
    assert 'Message ID: 456' in notifier.messages[0]
    assert sum('INSERT INTO telegram' in sql for sql, _ in queries) == 1


def test_update_channel_continues_when_notification_fails(tmp_path, monkeypatch) -> None:
    tg = _make_telegram(_FailingNotifier())
    monkeypatch.setattr(telegram_module.cfg, 'path', tmp_path)

    msg = SimpleNamespace(id=456)
    queries: list[tuple[str, tuple | None]] = []

    async def _fake_get_videos(_channel: object) -> list[dict[str, object]]:
        return [{'msg': msg, 'filename': 'My Video'}]

    async def _fake_get_downloaded_ids(_channel_id: int) -> list[int]:
        return []

    async def _fake_download(_msg: object, _dst: Path, _title: str) -> Path:
        return tmp_path / 'demo_channel' / 'My Video [456].mp4'

    async def _fake_query_d1(sql: str, params: tuple | None = None) -> list[dict[str, str]]:
        queries.append((sql, params))
        return []

    monkeypatch.setattr(tg, 'get_videos', _fake_get_videos)
    monkeypatch.setattr(tg, 'get_downloaded_ids', _fake_get_downloaded_ids)
    monkeypatch.setattr(tg, 'download', _fake_download)
    monkeypatch.setattr(telegram_module.cloudflare, 'query_d1', _fake_query_d1)

    asyncio.run(tg.update_channel(123))

    assert sum('INSERT INTO telegram' in sql for sql, _ in queries) == 1
