# ruff: noqa: S101

import asyncio
from pathlib import Path

import src.web.bilibili as bilibili_module
from src.web.bilibili import Bilibili


class _DummyTmpDir:
    def cleanup(self) -> None:
        return None


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
    b.credential = object()
    b.info_cache = {}
    return b


def test_update_fav_clears_toview_after_download_pass(tmp_path, monkeypatch) -> None:  # noqa: ANN001
    b = _make_bilibili(tmp_path)

    cleared = {'count': 0}

    async def _fake_clear_toview_list(*, credential) -> None:  # noqa: ANN001, ARG001
        cleared['count'] += 1

    async def _fake_get_toviews() -> tuple[list[_DummyVideo], bool]:
        return ([ _DummyVideo('BV1TEST1') ], True)

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
