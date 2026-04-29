# ruff: noqa: INP001, S101, SLF001, ANN001

import asyncio
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

import pytest

import src.web.hanime1 as hanime1_module
from src.web.hanime1 import (
    DownloadResult,
    Hanime1,
    HanimeCandidate,
    HanimeRecord,
    IgnoredVideoError,
    RuntimeSeriesSeed,
    WatchMetadata,
    WatchSeries,
    WatchSeriesVideo,
)


class _DummyTmpDir:
    def cleanup(self) -> None:
        return None


def _make_hanime1(tmp_path: Path) -> Hanime1:
    h = Hanime1.__new__(Hanime1)
    h._tmp_dir = _DummyTmpDir()
    h.cache_dir = tmp_path / 'cache'
    h.cache_dir.mkdir(parents=True, exist_ok=True)
    return h


def test_extract_stream_urls_prefers_m3u8_and_unescapes() -> None:
    page_html = r"""
    <script>
      const v = "https:\/\/video.example.com\/master.m3u8?token=abc\u0026quality=1080";
      const backup = "//cdn.example.com/path/video.mp4?x=1";
    </script>
    """

    urls = Hanime1.extract_stream_urls(page_html)

    assert urls == [
        'https://video.example.com/master.m3u8?token=abc&quality=1080',
        'https://cdn.example.com/path/video.mp4?x=1',
    ]


def test_parse_title_uses_og_title_first() -> None:
    page_html = """
    <html>
      <head>
        <meta property="og:title" content="Sample &amp; Title" />
        <title>Fallback Title</title>
      </head>
    </html>
    """

    assert Hanime1.parse_title(page_html) == 'Sample & Title'


def test_extract_download_urls_prefers_highest_quality_and_unescapes() -> None:
    page_html = """
    <a data-url="https://vdownload.example.com/18580-sc-480p.mp4?token=a&amp;expires=1">low</a>
    <a data-url="https://vdownload.example.com/18580-sc-720p.mp4?token=b&amp;expires=1">high</a>
    """

    urls = Hanime1.extract_download_urls(page_html)

    assert urls == [
        'https://vdownload.example.com/18580-sc-720p.mp4?token=b&expires=1',
        'https://vdownload.example.com/18580-sc-480p.mp4?token=a&expires=1',
    ]


def test_extract_download_urls_supports_k_quality_labels() -> None:
    page_html = """
    <a data-url="https://vdownload.example.com/18580-sc-1080p.mp4?token=a&amp;expires=1">1080p</a>
    <a data-url="https://vdownload.example.com/18580-sc-4k.mp4?token=b&amp;expires=1">4k</a>
    <a data-url="https://vdownload.example.com/18580-sc-8K.mp4?token=c&amp;expires=1">8k</a>
    """

    urls = Hanime1.extract_download_urls(page_html)

    assert urls == [
        'https://vdownload.example.com/18580-sc-8K.mp4?token=c&expires=1',
        'https://vdownload.example.com/18580-sc-4k.mp4?token=b&expires=1',
        'https://vdownload.example.com/18580-sc-1080p.mp4?token=a&expires=1',
    ]


def test_extract_search_video_ids_dedupes_watch_links() -> None:
    page_html = """
    <a href="/watch?v=18580">a</a>
    <a href="watch?v=18580">dup</a>
    <a href="//hanime1.me/watch?v=20000&amp;src=1">b</a>
    <a href="https://hanime1.me/watch?v=30000">c</a>
    <a href="/download?v=18580">ignore</a>
    """

    ids = Hanime1.extract_search_video_ids(page_html)

    assert ids == ['18580', '20000', '30000']


def test_extract_watch_series_reads_playlist_ids_and_title() -> None:
    page_html = """
    <div id="video-playlist-wrapper">
      <h4>屈辱</h4>
      <div class="related-watch-wrap multiple-link-wrapper">
        <a class="overlay" href="https://hanime1.me/watch?v=13253"></a>
        <div class="card-mobile-title">屈辱 1</div>
      </div>
      <div class="related-watch-wrap multiple-link-wrapper">
        <a class="overlay" href="/watch?v=12488"></a>
        <img src="/thumbnail/12488.jpg" alt="屈辱 2" />
      </div>
      <a style="text-decoration: none;" href="https://hanime1.me/search?query=%E5%B1%88%E8%BE%B1">
        <div class="load-more-related-link related-watch-wrap">More studio videos</div>
      </a>
    </div>
    <div id="video-playlist-wrapper">
      <h4>屈辱</h4>
      <a class="overlay" href="https://hanime1.me/watch?v=12488"></a>
      <a class="overlay" href="https://hanime1.me/watch?v=12496"></a>
    </div>
    """

    series = Hanime1.extract_watch_series(page_html)

    assert series is not None
    assert series.name == '屈辱'
    assert list(series.video_ids) == ['13253', '12488', '12496']
    assert list(series.videos) == [
        WatchSeriesVideo(video_id='13253', title='屈辱 1', position=1),
        WatchSeriesVideo(video_id='12488', title='屈辱 2', position=2),
        WatchSeriesVideo(video_id='12496', title=None, position=3),
    ]


def test_extract_watch_series_preserves_mixed_wrapped_and_plain_links() -> None:
    page_html = """
    <div id="video-playlist-wrapper">
      <h4>混合系列</h4>
      <div class="related-watch-wrap multiple-link-wrapper">
        <a class="overlay" href="https://hanime1.me/watch?v=10001"></a>
        <div class="card-mobile-title">混合系列 1</div>
      </div>
      <a class="overlay" href="https://hanime1.me/watch?v=10002"></a>
    </div>
    """

    series = Hanime1.extract_watch_series(page_html)

    assert series is not None
    assert list(series.videos) == [
        WatchSeriesVideo(video_id='10001', title='混合系列 1', position=1),
        WatchSeriesVideo(video_id='10002', title=None, position=2),
    ]


def test_collect_ids_adds_sequence_when_playlist_titles_are_not_distinct(monkeypatch, tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    seed = RuntimeSeriesSeed(video_id='404988', title='クール de M')

    async def _fake_resolve_series(_item: HanimeRecord) -> WatchSeries:
        return WatchSeries(
            name='クール de M',
            video_ids=('404988', '157875'),
            videos=(
                WatchSeriesVideo(video_id='404988', title='高冷的抖M', position=1),
                WatchSeriesVideo(video_id='157875', title='高冷的抖M', position=2),
            ),
        )

    async def _fake_query_db(_sql: str, _params: tuple[str, ...] = ()) -> list[dict[str, str]]:
        return []

    monkeypatch.setattr(h, 'resolve_series_from_watch_page', _fake_resolve_series)
    monkeypatch.setattr(hanime1_module.database, 'query_db', _fake_query_db)

    collected = asyncio.run(h._collect_ids_from_watch_series([seed]))

    assert collected == {
        '404988': HanimeCandidate(video_id='404988', source_name='クール de M', archive_title='クール de M 01'),
        '157875': HanimeCandidate(video_id='157875', source_name='クール de M', archive_title='クール de M 02'),
    }


def test_collect_ids_keeps_playlist_titles_with_sequence(monkeypatch, tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    seed = RuntimeSeriesSeed(video_id='13253', title='屈辱')

    async def _fake_resolve_series(_item: HanimeRecord) -> WatchSeries:
        return WatchSeries(
            name='屈辱',
            video_ids=('13253', '12488'),
            videos=(
                WatchSeriesVideo(video_id='13253', title='屈辱 1', position=1),
                WatchSeriesVideo(video_id='12488', title='屈辱 2', position=2),
            ),
        )

    async def _fake_query_db(_sql: str, _params: tuple[str, ...] = ()) -> list[dict[str, str]]:
        return []

    monkeypatch.setattr(h, 'resolve_series_from_watch_page', _fake_resolve_series)
    monkeypatch.setattr(hanime1_module.database, 'query_db', _fake_query_db)

    collected = asyncio.run(h._collect_ids_from_watch_series([seed]))

    assert collected == {
        '13253': HanimeCandidate(video_id='13253', source_name='屈辱', archive_title='屈辱 1'),
        '12488': HanimeCandidate(video_id='12488', source_name='屈辱', archive_title='屈辱 2'),
    }


def test_extract_watch_series_returns_none_when_playlist_not_found() -> None:
    assert Hanime1.extract_watch_series('<div>no playlist</div>') is None


def test_extract_watch_series_ignores_load_more_search_url() -> None:
    page_html = """
    <div id="video-playlist-wrapper">
      <h4>Studio Name</h4>
      <a style="text-decoration: none;" href="https://hanime1.me/search?query=studio">
        <div class="load-more-related-link related-watch-wrap">More studio videos</div>
      </a>
    </div>
    """

    assert Hanime1.extract_watch_series(page_html) is None


def test_extract_watch_metadata_prefers_primary_watch_title() -> None:
    fullwidth_tilde = '\N{FULLWIDTH TILDE}'
    primary_title = f'クール de M {fullwidth_tilde}崩れないオンナ{fullwidth_tilde} [中文字幕]'
    page_html = f"""
    <html>
      <head>
        <title>{primary_title}&nbsp;-&nbsp;H動漫/裏番/線上看&nbsp;-&nbsp;Hanime1.me</title>
      </head>
      <body>
        <h3 id="shareBtn-title" class="video-details-wrapper">{primary_title}</h3>
        <div>觀看次數:152.2萬次&nbsp;&nbsp;2026-03-27</div>
        <a id="video-artist-name" href="/search?query=nur">nur</a>
        <div>高冷的抖M</div>
        <div class="video-caption-text caption-ellipsis">劇情介紹。</div>
      </body>
    </html>
    """

    metadata = Hanime1.extract_watch_metadata(page_html)

    assert metadata.title == primary_title
    assert metadata.uploader == 'nur'
    assert metadata.release_date == '2026-03-27'
    assert metadata.plot == '剧情介绍。'


def test_ignored_title_marker_requires_exact_site_markers() -> None:
    assert Hanime1._ignored_title_marker('OVA Demo [新番預告]') == '[新番预告]'
    assert Hanime1._ignored_title_marker('OVA Demo [新番预告]') == '[新番预告]'
    assert Hanime1._ignored_title_marker('OVA Demo [中字後補]') == '[中字后补]'
    assert Hanime1._ignored_title_marker('OVA Demo [中字后补]') == '[中字后补]'
    assert Hanime1._ignored_title_marker('OVA Demo [中文後補]') == '[中文后补]'
    assert Hanime1._ignored_title_marker('OVA Demo [中文后补]') == '[中文后补]'
    assert Hanime1._ignored_title_marker('OVA Demo 新番预告') is None
    assert Hanime1._ignored_title_marker('OVA Demo 预告') is None


def test_search_video_ids_limits_to_allowed_genres_and_dedupes(tmp_path, monkeypatch) -> None:
    h = _make_hanime1(tmp_path)
    monkeypatch.setattr(h, '_get_cookie_header', lambda: 'user_lang=zhs')

    class _Response:
        def __init__(self, text: str) -> None:
            self.text = text

        def raise_for_status(self) -> None:
            return None

    calls: list[str] = []

    class _Client:
        async def get(self, url: str, *, headers: dict[str, str]) -> _Response:
            calls.append(url)
            genre = parse_qs(urlsplit(url).query).get('genre', [''])[0]
            assert headers.get('Cookie') == 'user_lang=zhs'
            if genre == '裏番':
                return _Response('<a href="/watch?v=10001">a</a><a href="/watch?v=10002">b</a>')
            if genre == '泡麵番':
                return _Response('<a href="/watch?v=10002">dup</a><a href="/watch?v=10003">c</a>')
            msg = f'unexpected genre: {genre}'
            raise AssertionError(msg)

    h.client = _Client()

    ids = asyncio.run(h.search_video_ids('催眠性指導'))

    assert ids == ['10001', '10002', '10003']
    assert len(calls) == len(hanime1_module._SEARCH_ALLOWED_GENRES)


def test_search_video_ids_returns_partial_results_when_a_genre_fails(tmp_path, monkeypatch) -> None:
    h = _make_hanime1(tmp_path)
    monkeypatch.setattr(h, '_get_cookie_header', lambda: None)

    class _Response:
        def __init__(self, text: str) -> None:
            self.text = text

        def raise_for_status(self) -> None:
            return None

    class _Client:
        async def get(self, url: str, *, headers: dict[str, str]) -> _Response:
            assert headers['User-Agent']
            genre = parse_qs(urlsplit(url).query).get('genre', [''])[0]
            if genre == '裏番':
                return _Response('<a href="/watch?v=20001">a</a>')
            if genre == '泡麵番':
                msg = 'network error'
                raise RuntimeError(msg)
            msg = f'unexpected genre: {genre}'
            raise AssertionError(msg)

    h.client = _Client()

    ids = asyncio.run(h.search_video_ids('keyword'))

    assert ids == ['20001']


def test_build_ranking_page_url_uses_traditional_query_values() -> None:
    weekly_url = Hanime1._build_ranking_page_url(period='weekly', page=1)
    weekly_query = parse_qs(urlsplit(weekly_url).query)

    assert weekly_url.startswith(f'{hanime1_module.cfg.host.rstrip("/")}/search?')
    assert weekly_query == {
        'genre': ['裏番'],
        'sort': ['本週排行'],
    }

    monthly_url = Hanime1._build_ranking_page_url(period='monthly', page=2)
    monthly_query = parse_qs(urlsplit(monthly_url).query)

    assert monthly_query == {
        'genre': ['裏番'],
        'sort': ['本月排行'],
        'page': ['2'],
    }


def test_fetch_ranking_video_ids_reads_search_page(tmp_path, monkeypatch) -> None:
    h = _make_hanime1(tmp_path)
    monkeypatch.setattr(h, '_get_cookie_header', lambda: 'user_lang=zhs')

    class _Response:
        text = """
        <a href="https://hanime1.me/watch?v=10001">a</a>
        <a href="/watch?v=10002">b</a>
        <a href="/watch?v=10001">dup</a>
        """

        def raise_for_status(self) -> None:
            return None

    calls: list[str] = []

    class _Client:
        async def get(self, url: str, *, headers: dict[str, str]) -> _Response:
            calls.append(url)
            assert headers.get('Cookie') == 'user_lang=zhs'
            query = parse_qs(urlsplit(url).query)
            assert query['genre'] == ['裏番']
            assert query['sort'] == ['本週排行']
            return _Response()

    h.client = _Client()

    ids = asyncio.run(h.fetch_ranking_video_ids(period='weekly'))

    assert ids == ['10001', '10002']
    assert len(calls) == 1


def test_discover_ranking_series_adds_new_series_targets(monkeypatch, tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    monkeypatch.setattr(hanime1_module.cfg.ranking, 'enabled', True)
    monkeypatch.setattr(hanime1_module.cfg.ranking, 'periods', ['weekly', 'monthly'])
    monkeypatch.setattr(hanime1_module.cfg.ranking, 'pages', 1)

    async def _fake_collect_ranking_video_ids() -> list[str]:
        return ['100', '200', '300']

    async def _fake_resolve_series(item: HanimeRecord) -> WatchSeries | None:
        if item.id == '100':
            return WatchSeries(name='測試系列', video_ids=('100', '101'))
        if item.id == '300':
            return WatchSeries(name='既存系列', video_ids=('200', '300'))
        msg = f'unexpected item: {item.id}'
        raise AssertionError(msg)

    known_video_ids = {'200'}
    queries: list[tuple[str, tuple[str, ...]]] = []

    async def _fake_query_db(sql: str, params: tuple[str, ...] = ()) -> list[dict[str, str]]:
        queries.append((sql, params))
        if 'SELECT video_id' in sql:
            requested_ids = {str(video_id) for video_id in params[0]}
            return [{'video_id': video_id} for video_id in sorted(known_video_ids & requested_ids)]
        if 'INSERT INTO hanime1_series_video' in sql:
            known_video_ids.add(params[0])
        return []

    monkeypatch.setattr(h, '_collect_ranking_video_ids', _fake_collect_ranking_video_ids)
    monkeypatch.setattr(h, 'resolve_series_from_watch_page', _fake_resolve_series)
    monkeypatch.setattr(hanime1_module.database, 'query_db', _fake_query_db)

    added = asyncio.run(h.discover_ranking_series())

    assert added == [RuntimeSeriesSeed(video_id='100', title='测试系列')]
    assert any('INSERT INTO hanime1_series ' in sql and params == ('100', '测试系列', '100') for sql, params in queries)
    assert any('INSERT INTO hanime1_series_video' in sql and params == ('100', '100') for sql, params in queries)
    assert any('INSERT INTO hanime1_series_video' in sql and params == ('101', '100') for sql, params in queries)
    assert not any('INSERT INTO hanime1_series ' in sql and params == ('200', '既存系列', '300') for sql, params in queries)


def test_update_runs_ranking_discovery_before_get_items(monkeypatch, tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    events: list[str] = []

    async def _fake_ensure_table() -> None:
        events.append('ensure')

    async def _fake_discover_ranking_series() -> list[RuntimeSeriesSeed]:
        events.append('discover')
        return []

    async def _fake_get_items() -> list[HanimeRecord]:
        events.append('get_items')
        return []

    async def _fake_load_runtime_series_seeds() -> list[RuntimeSeriesSeed]:
        return [RuntimeSeriesSeed(video_id='100', title='Seed')]

    monkeypatch.setattr(h, '_ensure_table', _fake_ensure_table)
    monkeypatch.setattr(h, 'discover_ranking_series', _fake_discover_ranking_series)
    monkeypatch.setattr(h, 'get_items', _fake_get_items)
    monkeypatch.setattr(h, '_load_runtime_series_seeds', _fake_load_runtime_series_seeds)

    asyncio.run(h.update())

    assert events == ['ensure', 'discover', 'get_items']


def test_extract_watch_metadata_parses_title_release_date_and_plot() -> None:
    page_html = """
    <html>
      <head>
        <meta property="og:title" content="Demo Title - Hanime1.me" />
        <meta property="og:image" content="//cdn.example.com/covers/demo.jpg" />
      </head>
      <body>
        <div>觀看次數:301.5萬次&nbsp;&nbsp;2013-06-28</div>
        <a id="video-artist-name" href="/search?query=測試工坊">測試工坊</a>
        <div>公開測試 2</div>
        <div class="video-caption-text caption-ellipsis">
          這是學生們的課程介紹。
        </div>
      </body>
    </html>
    """

    metadata = Hanime1.extract_watch_metadata(page_html)

    assert metadata.title == '公开测试 2'
    assert metadata.uploader == '测试工坊'
    assert metadata.release_date == '2013-06-28'
    assert metadata.plot == '这是学生们的课程介绍。'
    assert metadata.cover_url == 'https://cdn.example.com/covers/demo.jpg'


def test_build_download_command_includes_headers_and_proxy(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(hanime1_module.config, 'proxy', 'http://127.0.0.1:7890')

    command = Hanime1._build_download_command(
        stream_url='https://video.example.com/master.m3u8',
        output_template=tmp_path / 'video.%(ext)s',
        referer='https://hanime1.me/v/demo',
        cookie_header='cf_clearance=token',
    )

    assert command[0] == 'yt-dlp'
    assert '--referer' in command
    assert 'https://hanime1.me/v/demo' in command
    assert '--add-header' in command
    assert 'Cookie:cf_clearance=token' in command
    assert '--proxy' in command
    assert 'http://127.0.0.1:7890' in command


def test_get_cookie_header_uses_user_lang_only(tmp_path) -> None:
    h = _make_hanime1(tmp_path)

    cookie_header = h._get_cookie_header()

    assert cookie_header == 'user_lang=zhs'


def test_get_items_from_series_seeds_filters_downloaded_ids(monkeypatch, tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    seeds = [
        RuntimeSeriesSeed(video_id='12488', title='屈辱'),
    ]

    async def _fake_load_runtime_series_seeds() -> list[RuntimeSeriesSeed]:
        return seeds

    monkeypatch.setattr(h, '_load_runtime_series_seeds', _fake_load_runtime_series_seeds)

    async def _fake_downloaded_ids() -> set[str]:
        return {'13253'}

    async def _fake_collect(_seeds: list[RuntimeSeriesSeed]) -> dict[str, HanimeCandidate]:
        assert _seeds == seeds
        return {
            '13253': HanimeCandidate(video_id='13253', source_name='屈辱', archive_title='屈辱 01'),
            '12488': HanimeCandidate(video_id='12488', source_name='屈辱', archive_title='屈辱 02'),
        }

    async def _fake_resolve_metadata(item: HanimeRecord) -> WatchMetadata:
        return WatchMetadata(title=f'Title {item.id}')

    monkeypatch.setattr(h, '_get_downloaded_ids', _fake_downloaded_ids)
    monkeypatch.setattr(h, '_collect_ids_from_watch_series', _fake_collect)
    monkeypatch.setattr(h, 'resolve_metadata_from_watch_page', _fake_resolve_metadata)

    items = asyncio.run(h.get_items())

    assert [item.id for item in items] == ['12488']
    assert [item.title for item in items] == ['屈辱 02']
    assert [item.keyword for item in items] == ['屈辱']
    assert all(item.page_url == f'{hanime1_module.cfg.host.rstrip("/")}/watch?v={item.id}' for item in items)


def test_get_items_skips_candidates_with_ignored_watch_markers(monkeypatch, tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    seeds = [
        RuntimeSeriesSeed(video_id='12488', title='屈辱'),
    ]

    async def _fake_load_runtime_series_seeds() -> list[RuntimeSeriesSeed]:
        return seeds

    monkeypatch.setattr(h, '_load_runtime_series_seeds', _fake_load_runtime_series_seeds)

    async def _fake_downloaded_ids() -> set[str]:
        return set()

    async def _fake_collect(_seeds: list[RuntimeSeriesSeed]) -> dict[str, HanimeCandidate]:
        return {
            '404989': HanimeCandidate(video_id='404989', source_name='OVA Demo', archive_title='OVA Demo 01'),
            '143654': HanimeCandidate(video_id='143654', source_name='OVA Demo', archive_title='OVA Demo 02'),
            '404988': HanimeCandidate(video_id='404988', source_name='OVA Demo', archive_title='OVA Demo 03'),
            '157878': HanimeCandidate(video_id='157878', source_name='OVA Demo', archive_title='OVA Demo 04'),
        }

    async def _fake_resolve_metadata(item: HanimeRecord) -> WatchMetadata:
        mapping = {
            '404989': WatchMetadata(title='OVA Demo [新番預告]'),
            '143654': WatchMetadata(title='OVA Demo [中字後補]'),
            '404988': WatchMetadata(title='OVA Demo [中文後補]'),
            '157878': WatchMetadata(title='OVA Demo Episode 1'),
        }
        return mapping[item.id]

    monkeypatch.setattr(h, '_get_downloaded_ids', _fake_downloaded_ids)
    monkeypatch.setattr(h, '_collect_ids_from_watch_series', _fake_collect)
    monkeypatch.setattr(h, 'resolve_metadata_from_watch_page', _fake_resolve_metadata)

    items = asyncio.run(h.get_items())

    assert [item.id for item in items] == ['157878']
    assert [item.title for item in items] == ['OVA Demo 04']


def test_collect_ids_upserts_series_members_and_marks_success(monkeypatch, tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    seed = RuntimeSeriesSeed(video_id='12488', title='屈辱')

    async def _fake_resolve_series(_item: HanimeRecord) -> WatchSeries:
        return WatchSeries(name='屈辱', video_ids=('13253', '12488'))

    queries: list[tuple[str, tuple[str, ...]]] = []

    async def _fake_query_db(sql: str, params: tuple[str, ...] = ()) -> list[dict[str, str]]:
        queries.append((sql, params))
        return []

    monkeypatch.setattr(h, 'resolve_series_from_watch_page', _fake_resolve_series)
    monkeypatch.setattr(hanime1_module.database, 'query_db', _fake_query_db)

    collected = asyncio.run(h._collect_ids_from_watch_series([seed]))

    assert collected == {
        '13253': HanimeCandidate(video_id='13253', source_name='屈辱', archive_title='屈辱 01'),
        '12488': HanimeCandidate(video_id='12488', source_name='屈辱', archive_title='屈辱 02'),
    }
    assert any('INSERT INTO hanime1_series_video' in sql and params == ('12488', '12488') for sql, params in queries)
    assert any('INSERT INTO hanime1_series_video' in sql and params == ('13253', '12488') for sql, params in queries)
    assert any('SET last_scanned_at = CURRENT_TIMESTAMP' in sql and params == ('12488',) for sql, params in queries)


def test_collect_ids_marks_series_failure(monkeypatch, tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    seed = RuntimeSeriesSeed(video_id='12488', title='屈辱')

    async def _fake_resolve_series(_item: HanimeRecord) -> WatchSeries:
        msg = 'blocked'
        raise RuntimeError(msg)

    queries: list[tuple[str, tuple[str, ...]]] = []

    async def _fake_query_db(sql: str, params: tuple[str, ...] = ()) -> list[dict[str, str]]:
        queries.append((sql, params))
        return []

    monkeypatch.setattr(h, 'resolve_series_from_watch_page', _fake_resolve_series)
    monkeypatch.setattr(hanime1_module.database, 'query_db', _fake_query_db)

    collected = asyncio.run(h._collect_ids_from_watch_series([seed]))

    assert collected == {}
    assert any('SET last_scan_error = ?' in sql and params == ('RuntimeError: blocked', '12488') for sql, params in queries)


def test_load_runtime_series_seeds_reads_database(monkeypatch, tmp_path) -> None:
    h = _make_hanime1(tmp_path)

    async def _fake_query_db(sql: str, params: tuple[str, ...] = ()) -> list[dict[str, str]]:
        assert 'FROM hanime1_series' in sql
        assert params == ()
        return [
            {'canonical_video_id': '12488', 'title': '屈辱'},
            {'canonical_video_id': '', 'title': 'ignored'},
            {'canonical_video_id': '12496', 'title': ''},
        ]

    monkeypatch.setattr(hanime1_module.database, 'query_db', _fake_query_db)

    seeds = asyncio.run(h._load_runtime_series_seeds())

    assert seeds == [
        RuntimeSeriesSeed(video_id='12488', title='屈辱'),
    ]


def test_parse_runtime_seed_supports_id_and_title_id_formats() -> None:
    seed1 = Hanime1._parse_runtime_seed('12488')
    seed2 = Hanime1._parse_runtime_seed('屈辱 {id-12488}')

    assert seed1 is None
    assert seed2 == RuntimeSeriesSeed(video_id='12488', title='屈辱')


def test_download_item_renames_and_moves_file(tmp_path, monkeypatch) -> None:
    h = _make_hanime1(tmp_path)
    output_dir = tmp_path / 'hanime1'
    monkeypatch.setattr(hanime1_module.cfg, 'path', output_dir)

    item = HanimeRecord(
        id='video-123',
        title='',
        keyword='公開便所',
        uploader='Uploader',
        page_url='https://hanime1.me/v/demo',
        stream_url=None,
    )

    async def _fake_resolve_stream(_page_url: str) -> tuple[str, str | None]:
        return ('https://video.example.com/master.m3u8', 'Resolved Title')

    async def _fake_resolve_stream_from_download(_item: HanimeRecord) -> tuple[str, str | None]:
        return ('https://video.example.com/master.mp4?token=abc', 'Resolved Title')

    async def _fake_resolve_metadata(_item: HanimeRecord) -> WatchMetadata:
        return WatchMetadata(
            title='Watch Title',
            release_date='2020-01-01',
            plot='watch plot',
        )

    def _fake_download_stream(*, task, dirpath: Path) -> Path:
        assert task.stream_url == 'https://video.example.com/master.mp4?token=abc'
        assert task.video_id == 'video-123'
        assert task.referer == 'https://hanime1.me/v/demo'
        assert task.cookie_header is None
        dirpath.mkdir(parents=True, exist_ok=True)
        saved = dirpath / f'{task.video_id}.mp4'
        saved.write_bytes(b'video')
        return saved

    monkeypatch.setattr(h, 'resolve_stream_from_download_page', _fake_resolve_stream_from_download)
    monkeypatch.setattr(h, 'resolve_stream_from_page', _fake_resolve_stream)
    monkeypatch.setattr(h, 'resolve_metadata_from_watch_page', _fake_resolve_metadata)
    monkeypatch.setattr(h, 'download_stream', _fake_download_stream)
    monkeypatch.setattr(h, '_get_cookie_header', lambda: None)

    result = asyncio.run(h.download_item(item))

    assert result.title == 'Watch Title'
    assert result.archive_title == 'Watch Title'
    assert result.stream_url == 'https://video.example.com/master.mp4?token=abc'
    assert result.resolution is None
    assert result.release_date == '2020-01-01'
    assert result.plot == 'watch plot'
    assert result.cover_url is None
    assert result.final_path.exists()
    assert result.final_path == output_dir / '公開便所' / 'Watch Title [video-123].mp4'


def test_download_item_uses_uploader_from_watch_metadata_when_missing(tmp_path, monkeypatch) -> None:
    h = _make_hanime1(tmp_path)
    output_dir = tmp_path / 'hanime1'
    monkeypatch.setattr(hanime1_module.cfg, 'path', output_dir)

    item = HanimeRecord(
        id='video-234',
        title='Playlist Title 01',
        keyword='tag-b',
        uploader=None,
        page_url='https://hanime1.me/v/demo',
        stream_url=None,
    )

    async def _fake_resolve_stream_from_download(_item: HanimeRecord) -> tuple[str, str | None]:
        return ('https://video.example.com/master.mp4?token=abc', 'Resolved Title')

    async def _fake_resolve_stream(_page_url: str) -> tuple[str, str | None]:
        return ('https://video.example.com/master.m3u8', 'Resolved Title')

    async def _fake_resolve_metadata(_item: HanimeRecord) -> WatchMetadata:
        return WatchMetadata(
            title='Watch Title',
            uploader='Watch Studio',
            release_date='2020-01-01',
            plot='watch plot',
        )

    def _fake_download_stream(*, task, dirpath: Path) -> Path:
        dirpath.mkdir(parents=True, exist_ok=True)
        saved = dirpath / f'{task.video_id}.mp4'
        saved.write_bytes(b'video')
        return saved

    monkeypatch.setattr(h, 'resolve_stream_from_download_page', _fake_resolve_stream_from_download)
    monkeypatch.setattr(h, 'resolve_stream_from_page', _fake_resolve_stream)
    monkeypatch.setattr(h, 'resolve_metadata_from_watch_page', _fake_resolve_metadata)
    monkeypatch.setattr(h, 'download_stream', _fake_download_stream)
    monkeypatch.setattr(h, '_get_cookie_header', lambda: None)

    result = asyncio.run(h.download_item(item))

    assert result.title == 'Watch Title'
    assert result.archive_title == 'Playlist Title 01'
    assert result.uploader == 'Watch Studio'
    assert result.resolution is None
    assert result.cover_url is None
    assert result.final_path == output_dir / 'tag-b' / 'Playlist Title 01 [video-234].mp4'


def test_download_item_raises_for_ignored_site_markers(tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    item = HanimeRecord(
        id='video-ignored',
        title='OVA Demo [中字後補]',
        keyword='tag-b',
        uploader=None,
        page_url='https://hanime1.me/watch?v=video-ignored',
        stream_url='https://video.example.com/master.m3u8',
    )

    with pytest.raises(IgnoredVideoError, match=r'ignored marker \[中字后补\]'):
        asyncio.run(h.download_item(item))


def test_update_inserts_item_after_download(monkeypatch, tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    output_dir = tmp_path / 'hanime1'
    monkeypatch.setattr(hanime1_module.cfg, 'path', output_dir)
    item = HanimeRecord(
        id='video-123',
        title='Original Title',
        keyword='kw',
        uploader='Uploader',
        page_url='https://hanime1.me/v/demo',
        stream_url='https://video.example.com/master.m3u8',
    )

    async def _fake_get_items() -> list[HanimeRecord]:
        return [item]

    async def _fake_download_item(_item: HanimeRecord) -> DownloadResult:
        output_dir.mkdir(parents=True, exist_ok=True)
        final_path = output_dir / '[Uploader]Resolved Title [video-123].mp4'
        final_path.write_bytes(b'video')
        return DownloadResult(
            item_id='video-123',
            title='Resolved Title',
            stream_url='https://video.example.com/master.m3u8',
            final_path=final_path,
        )

    notify_calls: list[str] = []

    async def _fake_notify_download(
        *,
        item: HanimeRecord,
        resolution: str | None = None,
        file_size_bytes: int | None = None,
        release_date: str | None = None,
        cover_url: str | None = None,
    ) -> None:
        assert resolution is None
        assert file_size_bytes == len(b'video')
        assert release_date is None
        assert cover_url is None
        notify_calls.append(item.id)

    queries: list[tuple[str, tuple[str, ...]]] = []

    async def _fake_query_db(sql: str, params: tuple[str, ...] = ()) -> list[dict[str, str]]:
        queries.append((sql, params))
        return []

    monkeypatch.setattr(h, 'get_items', _fake_get_items)
    monkeypatch.setattr(h, 'download_item', _fake_download_item)
    monkeypatch.setattr(h, '_notify_download', _fake_notify_download)
    monkeypatch.setattr(hanime1_module.database, 'query_db', _fake_query_db)

    asyncio.run(h.update())

    assert any('CREATE TABLE IF NOT EXISTS hanime1' in sql for sql, _ in queries)
    assert (
        'INSERT OR IGNORE INTO hanime1 (id, title, uploader, release_date, plot) VALUES (?, ?, ?, ?, ?);',
        ('video-123', 'Resolved Title', 'Uploader', '', ''),
    ) in queries
    assert notify_calls == ['video-123']


def test_update_skips_ignored_items_without_db_insert(monkeypatch, tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    output_dir = tmp_path / 'hanime1'
    monkeypatch.setattr(hanime1_module.cfg, 'path', output_dir)
    item = HanimeRecord(
        id='video-ignored',
        title='OVA Demo [新番預告]',
        keyword='kw',
        uploader='Uploader',
        page_url='https://hanime1.me/watch?v=video-ignored',
        stream_url='https://video.example.com/master.m3u8',
    )

    async def _fake_get_items() -> list[HanimeRecord]:
        return [item]

    queries: list[tuple[str, tuple[str, ...]]] = []

    async def _fake_query_db(sql: str, params: tuple[str, ...] = ()) -> list[dict[str, str]]:
        queries.append((sql, params))
        if 'PRAGMA table_info(hanime1);' in sql:
            return [
                {'name': 'id'},
                {'name': 'title'},
                {'name': 'uploader'},
                {'name': 'release_date'},
                {'name': 'plot'},
            ]
        return []

    monkeypatch.setattr(h, 'get_items', _fake_get_items)
    monkeypatch.setattr(hanime1_module.database, 'query_db', _fake_query_db)

    asyncio.run(h.update())

    assert any('CREATE TABLE IF NOT EXISTS hanime1' in sql for sql, _ in queries)
    assert not any('INSERT OR IGNORE INTO hanime1' in sql for sql, _ in queries)


def test_notify_download_enqueues_structured_payload(tmp_path, monkeypatch) -> None:
    h = _make_hanime1(tmp_path)
    notifications: list[dict[str, object]] = []

    async def _fake_enqueue_notification(**payload) -> None:  # noqa: ANN003
        notifications.append(payload)

    monkeypatch.setattr(hanime1_module, 'enqueue_notification', _fake_enqueue_notification)
    item = HanimeRecord(
        id='12345',
        title='公開便所 2',
        uploader='Uploader_One',
        page_url='https://hanime1.me/watch?v=12345',
        stream_url=None,
    )

    asyncio.run(h._notify_download(item=item, resolution='720p', file_size_bytes=1024 * 1024, release_date='2020-01-01'))

    assert notifications == [
        {
            'kind': 'download_completed',
            'source': 'hanime1',
            'title': 'Hanime1: 公開便所 2',
            'body': '720p | 1.0 MB | 2020-01-01',
            'link_url': 'https://hanime1.me/watch?v=12345',
            'image_url': '',
            'payload': {
                'video_id': '12345',
                'resolution': '720p',
                'file_size_bytes': 1048576,
                'release_date': '2020-01-01',
            },
        },
    ]


def test_notify_download_keeps_cover_url_when_available(tmp_path, monkeypatch) -> None:
    h = _make_hanime1(tmp_path)
    notifications: list[dict[str, object]] = []

    async def _fake_enqueue_notification(**payload) -> None:  # noqa: ANN003
        notifications.append(payload)

    monkeypatch.setattr(hanime1_module, 'enqueue_notification', _fake_enqueue_notification)
    item = HanimeRecord(
        id='12345',
        title='公開便所 2',
        uploader='Uploader One',
        page_url='https://hanime1.me/watch?v=12345',
        stream_url=None,
    )

    asyncio.run(
        h._notify_download(
            item=item,
            resolution='720p',
            file_size_bytes=1024 * 1024,
            release_date='2020-01-01',
            cover_url='https://cdn.example.com/covers/12345.jpg',
        ),
    )

    assert notifications[0]['image_url'] == 'https://cdn.example.com/covers/12345.jpg'


def test_stream_resolution_label_parses_quality() -> None:
    assert Hanime1._stream_resolution_label('https://video.example.com/18580-sc-720p.mp4') == '720p'
    assert Hanime1._stream_resolution_label('https://video.example.com/18580-sc-4k.mp4?token=1') == '2160p'
    assert Hanime1._stream_resolution_label('https://video.example.com/master.m3u8') is None


def test_format_file_size_human_readable() -> None:
    assert Hanime1._format_file_size(999) == '999 B'
    assert Hanime1._format_file_size(1024) == '1.0 KB'
    assert Hanime1._format_file_size(1024 * 1024) == '1.0 MB'
    assert Hanime1._format_file_size(-1) is None
    assert Hanime1._format_file_size(None) is None
