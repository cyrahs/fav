# ruff: noqa: INP001, S101, SLF001, ANN001, ASYNC240

import asyncio
from pathlib import Path

import src.web.stellasora as stellasora_module
from src.web.stellasora import DownloadStats, StellaSora, WikiFile


def _make_stellasora(*, path: Path) -> StellaSora:
    ss = StellaSora.__new__(StellaSora)
    ss.path = path
    return ss


def test_download_resolved_files_enqueues_notification(tmp_path, monkeypatch) -> None:
    ss = _make_stellasora(path=tmp_path)
    notifications: list[dict[str, object]] = []

    async def _fake_download_file(url: str, dst_path: Path, *, desc: str, max_attempts: int = 3) -> None:  # noqa: ARG001
        dst_path.parent.mkdir(parents=True, exist_ok=True)
        dst_path.write_bytes(b'img')

    def _fake_handle_existing_file(*, title: str, dst_dir: Path, existing_index: dict[str, list[Path]]) -> tuple[bool, int, int]:  # noqa: ARG001
        return (True, 0, 0)

    async def _fake_enqueue_notification(**payload) -> None:  # noqa: ANN003
        notifications.append(payload)

    monkeypatch.setattr(ss, '_download_file', _fake_download_file)
    monkeypatch.setattr(ss, '_handle_existing_file', _fake_handle_existing_file)
    monkeypatch.setattr(stellasora_module, 'enqueue_notification', _fake_enqueue_notification)

    stats = DownloadStats()
    resolved_by_key = {
        'File:Foo.png': WikiFile(
            title='File:Foo.png',
            url='https://example.com/Foo.png',
            description_url='https://wiki.example.com/Foo',
        ),
    }
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
    assert notifications == [
        {
            'kind': 'download_completed',
            'source': 'stellasora',
            'title': 'StellaSora: Foo.png',
            'body': 'disc/Foo.png',
            'link_url': 'https://wiki.example.com/Foo',
            'image_url': 'https://example.com/Foo.png',
            'payload': {
                'saved_path': str(tmp_path / 'disc' / 'Foo.png'),
                'image_path': str(tmp_path / 'disc' / 'Foo.png'),
            },
        },
    ]


def test_download_resolved_files_continues_when_notification_fails(tmp_path, monkeypatch) -> None:
    ss = _make_stellasora(path=tmp_path)

    async def _fake_download_file(url: str, dst_path: Path, *, desc: str, max_attempts: int = 3) -> None:  # noqa: ARG001
        dst_path.parent.mkdir(parents=True, exist_ok=True)
        dst_path.write_bytes(b'img')

    def _fake_handle_existing_file(*, title: str, dst_dir: Path, existing_index: dict[str, list[Path]]) -> tuple[bool, int, int]:  # noqa: ARG001
        return (True, 0, 0)

    async def _failing_enqueue_notification(**_payload) -> None:  # noqa: ANN003
        msg = 'notify failed'
        raise RuntimeError(msg)

    monkeypatch.setattr(ss, '_download_file', _fake_download_file)
    monkeypatch.setattr(ss, '_handle_existing_file', _fake_handle_existing_file)
    monkeypatch.setattr(stellasora_module, 'enqueue_notification', _failing_enqueue_notification)

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


def test_notify_download_uses_relative_path_from_stellasora_root(tmp_path, monkeypatch) -> None:
    ss = _make_stellasora(path=tmp_path)
    notifications: list[dict[str, object]] = []
    saved_path = tmp_path / 'Wraith' / 'Wraith_awakened_02.png'

    async def _fake_enqueue_notification(**payload) -> None:  # noqa: ANN003
        notifications.append(payload)

    monkeypatch.setattr(stellasora_module, 'enqueue_notification', _fake_enqueue_notification)

    asyncio.run(
        ss._notify_download(
            title='File:Wraith_awakened_02.png',
            image_url='https://example.com/Wraith_awakened_02.png',
            saved_path=saved_path,
        ),
    )

    assert notifications[0]['body'] == 'Wraith/Wraith_awakened_02.png'
