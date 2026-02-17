# ruff: noqa: INP001, S101, SLF001, ANN001

import asyncio

from src.web.tangxin import Item, Tangxin


class _RecordingNotifier:
    def __init__(self) -> None:
        self.messages: list[str] = []

    async def send(self, message: str) -> None:
        self.messages.append(message)


class _FailingNotifier:
    async def send(self, message: str) -> None:  # noqa: ARG002
        msg = 'notify failed'
        raise RuntimeError(msg)


def _make_item() -> Item:
    return Item(id=123, title='Sample Title', upper='Sample Uploader')


def test_notify_download_sends_expected_message(tmp_path) -> None:
    recorder = _RecordingNotifier()
    tangxin = Tangxin.__new__(Tangxin)
    tangxin.notifier = recorder
    item = _make_item()
    dst_path = tmp_path / 'sample.mp4'

    asyncio.run(tangxin._notify_download(item=item, dst_path=dst_path))

    assert len(recorder.messages) == 1
    assert 'Tangxin download completed' in recorder.messages[0]
    assert 'ID: 123' in recorder.messages[0]
    assert 'Title: Sample Title' in recorder.messages[0]
    assert f'Path: {dst_path}' in recorder.messages[0]


def test_notify_download_swallows_notifier_errors(tmp_path) -> None:
    tangxin = Tangxin.__new__(Tangxin)
    tangxin.notifier = _FailingNotifier()
    item = _make_item()
    dst_path = tmp_path / 'sample.mp4'

    asyncio.run(tangxin._notify_download(item=item, dst_path=dst_path))
