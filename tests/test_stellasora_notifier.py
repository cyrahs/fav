# ruff: noqa: INP001, S101, SLF001, ANN001, ASYNC240

import asyncio
from pathlib import Path

from src.web.stellasora import DownloadStats, StellaSora, WikiFile


class _RecordingNotifier:
    def __init__(self) -> None:
        self.messages: list[str] = []
        self.photos: list[tuple[str, str | None, str | None]] = []

    async def send(self, message: str) -> None:
        self.messages.append(message)

    async def send_photo(self, *, photo: str, caption: str | None = None, parse_mode: str | None = None) -> None:
        self.photos.append((photo, caption, parse_mode))


class _FailingNotifier:
    async def send(self, message: str) -> None:  # noqa: ARG002
        msg = 'notify failed'
        raise RuntimeError(msg)

    async def send_photo(self, *, photo: str, caption: str | None = None, parse_mode: str | None = None) -> None:  # noqa: ARG002
        msg = 'notify failed'
        raise RuntimeError(msg)


class _MarkdownOnlyNotifier:
    def __init__(self) -> None:
        self.messages: list[str] = []

    async def send_markdown(self, message: str) -> None:
        self.messages.append(message)

    async def send(self, message: str) -> None:  # noqa: ARG002
        msg = 'send() should not be called when send_markdown exists'
        raise AssertionError(msg)


def _make_stellasora(notifier: object) -> StellaSora:
    ss = StellaSora.__new__(StellaSora)
    ss.notifier = notifier
    return ss


def test_download_resolved_files_sends_notification(tmp_path, monkeypatch) -> None:
    recorder = _RecordingNotifier()
    ss = _make_stellasora(recorder)

    async def _fake_download_file(url: str, dst_path: Path, *, desc: str, max_attempts: int = 3) -> None:  # noqa: ARG001
        dst_path.parent.mkdir(parents=True, exist_ok=True)
        dst_path.write_bytes(b'img')

    def _fake_handle_existing_file(*, title: str, dst_dir: Path, existing_index: dict[str, list[Path]]) -> tuple[bool, int, int]:  # noqa: ARG001
        return (True, 0, 0)

    monkeypatch.setattr(ss, '_download_file', _fake_download_file)
    monkeypatch.setattr(ss, '_handle_existing_file', _fake_handle_existing_file)

    stats = DownloadStats()
    resolved_by_key = {'File:Foo.png': WikiFile(title='File:Foo.png', url='https://example.com/Foo.png')}
    title_to_dir = {'File:Foo.png': tmp_path / 'disc'}
    existing_index: dict[str, list[Path]] = {}

    asyncio.run(
        ss._download_resolved_files(
            resolved_by_key=resolved_by_key,
            title_to_dir=title_to_dir,
            existing_index=existing_index,
            stats=stats,
        ),
    )

    assert stats.downloaded == 1
    assert len(recorder.photos) == 1
    photo, caption, parse_mode = recorder.photos[0]
    assert photo == 'https://example.com/Foo.png'
    assert caption is not None
    assert 'StellaSora' in caption
    assert '*File:Foo.png*' in caption
    assert parse_mode == 'Markdown'


def test_download_resolved_files_continues_when_notification_fails(tmp_path, monkeypatch) -> None:
    ss = _make_stellasora(_FailingNotifier())

    async def _fake_download_file(url: str, dst_path: Path, *, desc: str, max_attempts: int = 3) -> None:  # noqa: ARG001
        dst_path.parent.mkdir(parents=True, exist_ok=True)
        dst_path.write_bytes(b'img')

    def _fake_handle_existing_file(*, title: str, dst_dir: Path, existing_index: dict[str, list[Path]]) -> tuple[bool, int, int]:  # noqa: ARG001
        return (True, 0, 0)

    monkeypatch.setattr(ss, '_download_file', _fake_download_file)
    monkeypatch.setattr(ss, '_handle_existing_file', _fake_handle_existing_file)

    stats = DownloadStats()
    resolved_by_key = {'File:Foo.png': WikiFile(title='File:Foo.png', url='https://example.com/Foo.png')}
    title_to_dir = {'File:Foo.png': tmp_path / 'disc'}
    existing_index: dict[str, list[Path]] = {}

    asyncio.run(
        ss._download_resolved_files(
            resolved_by_key=resolved_by_key,
            title_to_dir=title_to_dir,
            existing_index=existing_index,
            stats=stats,
        ),
    )

    assert stats.downloaded == 1


def test_download_resolved_files_uses_markdown_fallback_when_photo_not_supported(tmp_path, monkeypatch) -> None:
    notifier = _MarkdownOnlyNotifier()
    ss = _make_stellasora(notifier)

    async def _fake_download_file(url: str, dst_path: Path, *, desc: str, max_attempts: int = 3) -> None:  # noqa: ARG001
        dst_path.parent.mkdir(parents=True, exist_ok=True)
        dst_path.write_bytes(b'img')

    def _fake_handle_existing_file(*, title: str, dst_dir: Path, existing_index: dict[str, list[Path]]) -> tuple[bool, int, int]:  # noqa: ARG001
        return (True, 0, 0)

    monkeypatch.setattr(ss, '_download_file', _fake_download_file)
    monkeypatch.setattr(ss, '_handle_existing_file', _fake_handle_existing_file)

    stats = DownloadStats()
    resolved_by_key = {'File:Foo.png': WikiFile(title='File:Foo.png', url='https://example.com/Foo.png')}
    title_to_dir = {'File:Foo.png': tmp_path / 'disc'}
    existing_index: dict[str, list[Path]] = {}

    asyncio.run(
        ss._download_resolved_files(
            resolved_by_key=resolved_by_key,
            title_to_dir=title_to_dir,
            existing_index=existing_index,
            stats=stats,
        ),
    )

    assert stats.downloaded == 1
    assert len(notifier.messages) == 1
    assert 'StellaSora' in notifier.messages[0]
    assert '*File:Foo.png*' in notifier.messages[0]
