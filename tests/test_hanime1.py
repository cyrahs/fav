# ruff: noqa: INP001, S101, SLF001, ANN001

import asyncio
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

import src.web.hanime1 as hanime1_module
from src.web.hanime1 import DownloadResult, Hanime1, HanimeRecord, RuntimeSeriesSeed, WatchMetadata


class _DummyTmpDir:
    def cleanup(self) -> None:
        return None


def _make_hanime1(tmp_path: Path) -> Hanime1:
    h = Hanime1.__new__(Hanime1)
    h.notifier = None
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
      <a class="overlay" href="https://hanime1.me/watch?v=13253"></a>
      <a class="overlay" href="/watch?v=12488"></a>
      <a style="text-decoration: none;" href="https://hanime1.me/search?query=%E5%B1%88%E8%BE%B1">
        <div class="load-more-related-link related-watch-wrap">更多 屈辱 的视频</div>
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
    assert series.search_url == 'https://hanime1.me/search?query=%E5%B1%88%E8%BE%B1'


def test_extract_watch_series_returns_none_when_playlist_not_found() -> None:
    assert Hanime1.extract_watch_series('<div>no playlist</div>') is None


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
    assert len(calls) == 2


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
    monkeypatch.setattr(h, '_load_runtime_series_seeds', lambda: seeds)

    async def _fake_downloaded_ids() -> set[str]:
        return {'13253'}

    async def _fake_collect(_seeds: list[RuntimeSeriesSeed]) -> dict[str, tuple[str, str]]:
        assert _seeds == seeds
        return {
            '13253': ('13253', '屈辱'),
            '12488': ('12488', '屈辱'),
        }

    monkeypatch.setattr(h, '_get_downloaded_ids', _fake_downloaded_ids)
    monkeypatch.setattr(h, '_collect_ids_from_watch_series', _fake_collect)

    items = asyncio.run(h.get_items())

    assert [item.id for item in items] == ['12488']
    assert [item.keyword for item in items] == ['屈辱']
    assert all(item.page_url == f'{hanime1_module.cfg.host.rstrip("/")}/watch?v={item.id}' for item in items)


def test_load_runtime_series_seeds_reads_data_config_json(monkeypatch, tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    runtime_config_path = tmp_path / 'data' / 'config.json'
    runtime_config_path.parent.mkdir(parents=True, exist_ok=True)
    runtime_config_path.write_text(
        '{"hanime1":{"keywords":["屈辱 {id-12488}","12496","{id-12488}","bad-seed"]}}',
        encoding='utf-8',
    )
    monkeypatch.setattr(hanime1_module.config, 'run_config', runtime_config_path)

    seeds = h._load_runtime_series_seeds()

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
        title='',
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

    assert result.uploader == 'Watch Studio'
    assert result.resolution is None
    assert result.cover_url is None
    assert result.final_path == output_dir / 'tag-b' / 'Watch Title [video-234].mp4'


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
        assert file_size_bytes == 5
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


def test_notify_download_prefers_markdown_and_omits_id_path_page(tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    markdown_calls: list[tuple[str, bool]] = []
    send_calls: list[str] = []

    class _Notifier:
        async def send_markdown(self, message: str, *, disable_web_page_preview: bool = False) -> None:
            markdown_calls.append((message, disable_web_page_preview))

        async def send(self, message: str) -> None:
            send_calls.append(message)

    h.notifier = _Notifier()
    item = HanimeRecord(
        id='12345',
        title='公開便所 2',
        uploader='Uploader_One',
        page_url='https://hanime1.me/watch?v=12345',
        stream_url=None,
    )

    asyncio.run(h._notify_download(item=item, resolution='720p', file_size_bytes=1024 * 1024, release_date='2020-01-01'))

    assert len(markdown_calls) == 1
    message, disable_preview = markdown_calls[0]
    assert disable_preview is True
    assert message == 'Hanime1\n*公開便所 2*\n[视频链接](https://hanime1.me/watch?v=12345) | 720p | 1.0 MB | 2020-01-01'
    assert 'ID:' not in message
    assert 'Path:' not in message
    assert 'Page:' not in message
    assert not send_calls


def test_notify_download_prefers_photo_when_cover_available(tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    photo_calls: list[tuple[str, str, str]] = []
    markdown_calls: list[str] = []
    send_calls: list[str] = []

    class _Notifier:
        async def send_photo(self, *, photo: str, caption: str | None = None, parse_mode: str | None = None) -> None:
            photo_calls.append((photo, caption or '', parse_mode or ''))

        async def send_markdown(self, message: str, *, disable_web_page_preview: bool = False) -> None:
            markdown_calls.append(message)

        async def send(self, message: str) -> None:
            send_calls.append(message)

    h.notifier = _Notifier()
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

    assert photo_calls == [
        (
            'https://cdn.example.com/covers/12345.jpg',
            'Hanime1\n*公開便所 2*\n[视频链接](https://hanime1.me/watch?v=12345) | 720p | 1.0 MB | 2020-01-01',
            'Markdown',
        ),
    ]
    assert not markdown_calls
    assert not send_calls


def test_notify_download_falls_back_to_plain_text(tmp_path) -> None:
    h = _make_hanime1(tmp_path)
    send_calls: list[str] = []

    class _Notifier:
        async def send(self, message: str) -> None:
            send_calls.append(message)

    h.notifier = _Notifier()
    item = HanimeRecord(
        id='12345',
        title='公開便所 2',
        uploader='Uploader One',
        page_url='https://hanime1.me/watch?v=12345',
        stream_url=None,
    )

    asyncio.run(h._notify_download(item=item, resolution='720p', file_size_bytes=1024 * 1024, release_date='2020-01-01'))

    assert send_calls == ['Hanime1\n公開便所 2\n视频链接(https://hanime1.me/watch?v=12345) | 720p | 1.0 MB | 2020-01-01']


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
