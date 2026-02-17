# ruff: noqa: S101

import asyncio
from pathlib import Path

import pytest

import src.web.bilibili as bilibili_module
from src.web.bilibili import Bilibili


class _DummyTmpDir:
    def cleanup(self) -> None:
        return None


class _RecordingNotifier:
    def __init__(self) -> None:
        self.messages: list[str] = []

    async def send(self, message: str) -> None:
        self.messages.append(message)


class _FailingNotifier:
    async def send(self, message: str) -> None:  # noqa: ARG002
        msg = 'notify failed'
        raise RuntimeError(msg)


class _DummyVideo:
    def __init__(self, bvid: str, title: str = 'Example Title', upper: str = 'Example Uploader') -> None:
        self._bvid = bvid
        self._title = title
        self._upper = upper

    def get_bvid(self) -> str:
        return self._bvid

    async def get_detail(self) -> dict:
        return {
            'View': {'title': self._title},
            'Card': {'card': {'name': self._upper}},
        }


def _make_bilibili(tmp_path: Path) -> Bilibili:
    b = Bilibili.__new__(Bilibili)
    b._tmp_dir = _DummyTmpDir()
    b.cache_dir = tmp_path / 'cache'
    b.cache_dir.mkdir(parents=True, exist_ok=True)
    b.notifier = None
    b.credential = object()
    b.info_cache = {}
    return b


def test_update_fav_sends_notification_for_each_video(tmp_path, monkeypatch) -> None:  # noqa: ANN001
    b = _make_bilibili(tmp_path)
    notifier = _RecordingNotifier()
    b.notifier = notifier

    async def _fake_get_favs(_fav_id: int) -> list[_DummyVideo]:
        return [
            _DummyVideo('BV1TEST1', title='Title One', upper='Uploader One'),
            _DummyVideo('BV1TEST2', title='Title Two', upper='Uploader Two'),
        ]

    async def _always_valid(_video) -> bool:  # noqa: ANN001
        return True

    def _fake_download(_url: str, bvid: str, dirpath: Path, *_args, **_kwargs) -> None:  # noqa: ANN001
        (dirpath / f'{bvid}.mp4').write_bytes(b'video')

    queries: list[tuple[str, tuple | None]] = []

    async def _fake_query_d1(sql: str, params: tuple | None = None) -> list[dict[str, str]]:
        queries.append((sql, params))
        return []

    def _no_tqdm(iterable, **_kwargs):  # noqa: ANN001
        return iterable

    monkeypatch.setattr(b, 'get_favs', _fake_get_favs)
    monkeypatch.setattr(b, 'check_valid', _always_valid)
    monkeypatch.setattr(b, 'download', _fake_download)
    monkeypatch.setattr(bilibili_module.cloudflare, 'query_d1', _fake_query_d1)
    monkeypatch.setattr(bilibili_module, 'tqdm', _no_tqdm)

    asyncio.run(b.update_fav(123, tmp_path / 'fav'))

    assert len(notifier.messages) == 2
    assert any('BV1TEST1' in message for message in notifier.messages)
    assert any('BV1TEST2' in message for message in notifier.messages)
    assert any('URL: https://www.bilibili.com/video/BV1TEST1' in message for message in notifier.messages)
    assert any('URL: https://www.bilibili.com/video/BV1TEST2' in message for message in notifier.messages)
    assert sum('INSERT INTO bilibili' in sql for sql, _ in queries) == 2


def test_update_fav_continues_when_notification_fails(tmp_path, monkeypatch) -> None:  # noqa: ANN001
    b = _make_bilibili(tmp_path)
    b.notifier = _FailingNotifier()

    async def _fake_get_favs(_fav_id: int) -> list[_DummyVideo]:
        return [_DummyVideo('BV1TEST1')]

    async def _always_valid(_video) -> bool:  # noqa: ANN001
        return True

    def _fake_download(_url: str, bvid: str, dirpath: Path, *_args, **_kwargs) -> None:  # noqa: ANN001
        (dirpath / f'{bvid}.mp4').write_bytes(b'video')

    queries: list[tuple[str, tuple | None]] = []

    async def _fake_query_d1(sql: str, params: tuple | None = None) -> list[dict[str, str]]:
        queries.append((sql, params))
        return []

    def _no_tqdm(iterable, **_kwargs):  # noqa: ANN001
        return iterable

    monkeypatch.setattr(b, 'get_favs', _fake_get_favs)
    monkeypatch.setattr(b, 'check_valid', _always_valid)
    monkeypatch.setattr(b, 'download', _fake_download)
    monkeypatch.setattr(bilibili_module.cloudflare, 'query_d1', _fake_query_d1)
    monkeypatch.setattr(bilibili_module, 'tqdm', _no_tqdm)

    asyncio.run(b.update_fav(123, tmp_path / 'fav'))

    assert sum('INSERT INTO bilibili' in sql for sql, _ in queries) == 1
    out_files = list((tmp_path / 'fav').glob('*.mp4'))
    assert len(out_files) == 1


def test_update_fav_clears_toview_after_download_pass(tmp_path, monkeypatch) -> None:  # noqa: ANN001
    b = _make_bilibili(tmp_path)

    cleared = {'count': 0}

    async def _fake_clear_toview_list(*, credential) -> None:  # noqa: ANN001, ARG001
        cleared['count'] += 1

    async def _fake_get_toviews() -> tuple[list[_DummyVideo], bool]:
        return ([_DummyVideo('BV1TEST1')], True)

    async def _always_valid(_video) -> bool:  # noqa: ANN001
        return True

    def _fake_download(_url: str, bvid: str, dirpath: Path, *_args, **_kwargs) -> None:  # noqa: ANN001
        (dirpath / f'{bvid}.mp4').write_bytes(b'video')

    queries: list[tuple[str, tuple | None]] = []

    async def _fake_query_d1(sql: str, params: tuple | None = None) -> list[dict[str, str]]:
        queries.append((sql, params))
        return []

    def _no_tqdm(iterable, **_kwargs):  # noqa: ANN001
        return iterable

    monkeypatch.setattr(bilibili_module.api.user, 'clear_toview_list', _fake_clear_toview_list)
    monkeypatch.setattr(b, 'get_toviews', _fake_get_toviews)
    monkeypatch.setattr(b, 'check_valid', _always_valid)
    monkeypatch.setattr(b, 'download', _fake_download)
    monkeypatch.setattr(bilibili_module.cloudflare, 'query_d1', _fake_query_d1)
    monkeypatch.setattr(bilibili_module, 'tqdm', _no_tqdm)

    asyncio.run(b.update_fav(-1, tmp_path / 'toview'))

    assert cleared['count'] == 1
    assert any('INSERT INTO bilibili' in sql for sql, _ in queries)

    out_dir = tmp_path / 'toview'
    out_files = list(out_dir.glob('*.mp4'))
    assert len(out_files) == 1
    assert (b.cache_dir / 'videos').exists()
    assert list((b.cache_dir / 'videos').iterdir()) == []


def test_update_fav_does_not_clear_toview_when_list_is_empty(tmp_path, monkeypatch) -> None:  # noqa: ANN001
    b = _make_bilibili(tmp_path)

    cleared = {'count': 0}

    async def _fake_clear_toview_list(*, credential) -> None:  # noqa: ANN001, ARG001
        cleared['count'] += 1

    async def _fake_get_toviews() -> tuple[list[_DummyVideo], bool]:
        return ([], False)

    async def _fake_query_d1(_sql: str, _params: tuple | None = None) -> list[dict[str, str]]:  # noqa: ARG001
        raise AssertionError('query_d1 should not be called when there are no downloads')

    monkeypatch.setattr(bilibili_module.api.user, 'clear_toview_list', _fake_clear_toview_list)
    monkeypatch.setattr(b, 'get_toviews', _fake_get_toviews)
    monkeypatch.setattr(bilibili_module.cloudflare, 'query_d1', _fake_query_d1)

    asyncio.run(b.update_fav(-1, tmp_path / 'toview'))

    assert cleared['count'] == 0


def test_download_retry_does_not_emit_warning_per_attempt(tmp_path, monkeypatch) -> None:  # noqa: ANN001
    b = _make_bilibili(tmp_path)
    b.cookie_path = tmp_path / 'cookies.txt'
    b.cookie_path.write_text('', encoding='utf-8')
    calls = {'count': 0}
    warnings: list[tuple[tuple, dict]] = []

    class _FailedResult:
        returncode = 1
        stdout = ''
        stderr = 'temporary network error'

    def _fake_run(*_args, **_kwargs):  # noqa: ANN001
        calls['count'] += 1
        return _FailedResult()

    def _capture_warning(*args, **kwargs):  # noqa: ANN001
        warnings.append((args, kwargs))

    monkeypatch.setattr(bilibili_module.subprocess, 'run', _fake_run)
    monkeypatch.setattr(bilibili_module.log, 'warning', _capture_warning)

    with pytest.raises(bilibili_module.DownloadError):
        b.download(
            url='https://www.bilibili.com/video/BV1TEST1',
            bvid='BV1TEST1',
            dirpath=tmp_path / 'videos',
            max_attempts=3,
            base_delay=0,
        )

    assert calls['count'] == 3
    assert warnings == []
