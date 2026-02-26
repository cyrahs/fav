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
        self.photos: list[tuple[str, str | None, str | None]] = []
        self.markdown_calls: list[tuple[str, bool]] = []

    async def send(self, message: str) -> None:
        self.messages.append(message)

    async def send_markdown(self, message: str, *, disable_web_page_preview: bool = False) -> None:
        self.messages.append(message)
        self.markdown_calls.append((message, disable_web_page_preview))

    async def send_photo(self, *, photo: str, caption: str | None = None, parse_mode: str | None = None) -> None:
        self.photos.append((photo, caption, parse_mode))


class _FailingNotifier:
    async def send(self, message: str) -> None:  # noqa: ARG002
        msg = 'notify failed'
        raise RuntimeError(msg)

    async def send_markdown(self, message: str, *, disable_web_page_preview: bool = False) -> None:  # noqa: ARG002
        msg = 'notify failed'
        raise RuntimeError(msg)

    async def send_photo(self, *, photo: str, caption: str | None = None, parse_mode: str | None = None) -> None:  # noqa: ARG002
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
            'View': {
                'title': self._title,
                'pic': f'https://example.com/{self._bvid}.jpg',
                'dimension': {'height': 1080},
                'pubdate': 1704067200,
            },
            'Card': {'card': {'name': self._upper}},
        }

    async def get_info(self) -> dict:
        return {'pic': ''}


class _DummyVideoWithInfoCover:
    def __init__(self, bvid: str, cover_url: str = 'https://i0.hdslb.com/test-cover.jpg') -> None:
        self._bvid = bvid
        self._cover_url = cover_url
        self.info_calls = 0

    def get_bvid(self) -> str:
        return self._bvid

    async def get_info(self) -> dict:
        self.info_calls += 1
        return {'pic': self._cover_url}


class _DummyVideoWithoutCover:
    def __init__(self, bvid: str) -> None:
        self._bvid = bvid

    def get_bvid(self) -> str:
        return self._bvid

    async def get_info(self) -> dict:
        return {}


class _DummyVideoWithTransparentCover:
    def __init__(self, bvid: str) -> None:
        self._bvid = bvid

    def get_bvid(self) -> str:
        return self._bvid

    async def get_info(self) -> dict:
        return {'pic': 'https://i0.hdslb.com/bfs/archive/transparent.png'}


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

    assert len(notifier.photos) == 2
    assert (
        'https://example.com/BV1TEST1.jpg',
        'Bilibili (fav)\n*Title One*\nUploader One\n[视频链接](https://www.bilibili.com/video/BV1TEST1) | 1080p | 5 B | 2024-01-01',
        'Markdown',
    ) in notifier.photos
    assert (
        'https://example.com/BV1TEST2.jpg',
        'Bilibili (fav)\n*Title Two*\nUploader Two\n[视频链接](https://www.bilibili.com/video/BV1TEST2) | 1080p | 5 B | 2024-01-01',
        'Markdown',
    ) in notifier.photos
    assert notifier.markdown_calls == []
    assert sum('INSERT INTO bilibili' in sql for sql, _ in queries) == 2


def test_notify_download_escapes_trailing_underscore_in_upper(tmp_path) -> None:  # noqa: ANN001
    b = _make_bilibili(tmp_path)
    notifier = _RecordingNotifier()
    b.notifier = notifier

    asyncio.run(
        b._notify_download(
            bvid='BV1wM4m1S7oo',
            title='让你说话了吗？笨蛋',
            upper='孟程程_',
            fav_id=123,
            resolution='720p',
            file_size_bytes=20 * 1024 * 1024,
            release_date='2024-01-01',
            cover_url='https://example.com/cover.jpg',
        ),
    )

    assert notifier.photos == [
        (
            'https://example.com/cover.jpg',
            'Bilibili (fav)\n*让你说话了吗？笨蛋*\n孟程程\\_\n[视频链接](https://www.bilibili.com/video/BV1wM4m1S7oo) | 720p | 20.0 MB | 2024-01-01',
            'Markdown',
        ),
    ]


def test_update_fav_uses_markdown_without_preview_when_cover_missing(tmp_path, monkeypatch) -> None:  # noqa: ANN001
    b = _make_bilibili(tmp_path)
    notifier = _RecordingNotifier()
    b.notifier = notifier

    async def _fake_get_favs(_fav_id: int) -> list[_DummyVideo]:
        return [_DummyVideo('BV1TEST1', title='Title One', upper='Uploader One')]

    async def _always_valid(_video) -> bool:  # noqa: ANN001
        return True

    async def _no_cover(_video, detail=None):  # noqa: ANN001, ARG001
        return None

    def _fake_download(_url: str, bvid: str, dirpath: Path, *_args, **_kwargs) -> None:  # noqa: ANN001
        (dirpath / f'{bvid}.mp4').write_bytes(b'video')

    async def _fake_query_d1(_sql: str, _params: tuple | None = None) -> list[dict[str, str]]:  # noqa: ARG001
        return []

    def _no_tqdm(iterable, **_kwargs):  # noqa: ANN001
        return iterable

    monkeypatch.setattr(b, 'get_favs', _fake_get_favs)
    monkeypatch.setattr(b, 'check_valid', _always_valid)
    monkeypatch.setattr(b, 'get_video_cover_url', _no_cover)
    monkeypatch.setattr(b, 'download', _fake_download)
    monkeypatch.setattr(bilibili_module.cloudflare, 'query_d1', _fake_query_d1)
    monkeypatch.setattr(bilibili_module, 'tqdm', _no_tqdm)

    asyncio.run(b.update_fav(123, tmp_path / 'fav'))

    assert notifier.photos == []
    assert len(notifier.markdown_calls) == 1
    message, disable_preview = notifier.markdown_calls[0]
    assert disable_preview is True
    assert '[视频链接](https://www.bilibili.com/video/BV1TEST1) | 1080p | 5 B | 2024-01-01' in message


def test_get_video_cover_url_from_detail_view_pic(tmp_path) -> None:  # noqa: ANN001
    b = _make_bilibili(tmp_path)
    video = _DummyVideo('BV1TEST1')
    detail = {'View': {'pic': '//i0.hdslb.com/bfs/archive/test-cover.jpg'}}

    cover_url = asyncio.run(b.get_video_cover_url(video, detail=detail))

    assert cover_url == 'https://i0.hdslb.com/bfs/archive/test-cover.jpg'


def test_get_video_cover_url_falls_back_to_get_info_and_cache(tmp_path) -> None:  # noqa: ANN001
    b = _make_bilibili(tmp_path)
    video = _DummyVideoWithInfoCover('BV1TEST1')

    first = asyncio.run(b.get_video_cover_url(video))
    second = asyncio.run(b.get_video_cover_url(video))

    assert first == 'https://i0.hdslb.com/test-cover.jpg'
    assert second == 'https://i0.hdslb.com/test-cover.jpg'
    assert video.info_calls == 1


def test_get_video_cover_url_returns_none_when_missing(tmp_path) -> None:  # noqa: ANN001
    b = _make_bilibili(tmp_path)
    video = _DummyVideoWithoutCover('BV1TEST1')

    cover_url = asyncio.run(b.get_video_cover_url(video))

    assert cover_url is None


def test_get_video_cover_url_returns_none_for_placeholder_cover(tmp_path) -> None:  # noqa: ANN001
    b = _make_bilibili(tmp_path)
    video = _DummyVideoWithTransparentCover('BV1TEST1')

    cover_url = asyncio.run(b.get_video_cover_url(video))

    assert cover_url is None


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
