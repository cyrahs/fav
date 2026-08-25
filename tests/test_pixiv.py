# ruff: noqa: ANN001, ANN002, ANN003, ANN202, ARG002, EM101, INP001, PLR2004, S101, S105, SLF001, TRY003

import asyncio
import zipfile
from pathlib import Path

import httpx
import pytest
from PIL import Image

import src.service.jobs as jobs_module
import src.web.pixiv as pixiv_module
from src.api.archive import ARCHIVE_SOURCES, _external_url
from src.api.schemas import JobRequestTarget
from src.api.settings_masking import mask_section, unmask_section
from src.core import settings
from src.tool.cookiecloud import PROFILES
from src.web.pixiv import (
    Pixiv,
    PixivApiError,
    PixivError,
    PixivWork,
    derive_user_id,
    extract_phpsessid,
    infer_extension,
    parse_bookmark_works,
    parse_page_urls,
    parse_ugoira_meta,
    synthesize_ugoira_webp,
)


def _configure_pixiv(**updates: object) -> settings.Pixiv:
    """Mutate the pinned settings snapshot that Pixiv() reads in __init__."""
    cfg = settings.load().web.pixiv
    for key, value in updates.items():
        setattr(cfg, key, value)
    return cfg


def _register_cookiecloud(name: str = 'cc', **overrides: str) -> str:
    """Add a shared CookieCloud config to the pinned snapshot; returns its name."""
    fields = {'server_url': 'https://cc.example', 'uuid': 'u', 'password': 'pw'}
    fields.update(overrides)
    settings.load().cookiecloud.configs.append(settings.CookieCloudEntry(name=name, **fields))
    return name


def _runnable_cfg(**updates: object) -> settings.Pixiv:
    return _configure_pixiv(cookiecloud=_register_cookiecloud(), sleep_request_seconds=0.0, **updates)


def _work(**updates) -> PixivWork:
    fields = {
        'illust_id': 96461036,
        'title': 'a title',
        'author': 'the artist',
        'author_id': '50944445',
        'illust_type': 0,
        'page_count': 1,
        'masked': False,
    }
    fields.update(updates)
    return PixivWork(**fields)


def _bookmark_entry(**updates) -> dict:
    entry = {
        'id': '96461036',
        'title': 'a title',
        'userId': '50944445',
        'userName': 'the artist',
        'illustType': 0,
        'pageCount': 1,
        'isMasked': False,
    }
    entry.update(updates)
    return entry


class _FakeDatabase:
    """Records the SQL a run issues instead of talking to PostgreSQL."""

    def __init__(self) -> None:
        self.calls: list[tuple[str, tuple]] = []
        self.rows: list[dict] = []

    async def query_db(self, query: str, params: tuple = ()) -> list[dict]:
        self.calls.append((query, params))
        return self.rows if query.strip().upper().startswith('SELECT') else []

    async def query_db_batch(self, statements) -> list[list[dict]]:
        results = []
        for query, params in statements:
            self.calls.append((query, params))
            results.append([])
        return results

    async def insert_db_batch(self, *, table, columns, rows, on_conflict=None) -> None:
        self.calls.append((f'INSERT {table} {on_conflict or ""}', tuple(rows)))


def _with_mock_ajax(source: Pixiv, handler) -> None:
    """Swap the ajax client for one backed by a MockTransport, keeping the real headers."""
    source.client = httpx.AsyncClient(transport=httpx.MockTransport(handler), headers=source.client.headers)


def _with_mock_media(source: Pixiv, handler) -> None:
    source.media_client = httpx.AsyncClient(transport=httpx.MockTransport(handler), headers=source.media_client.headers)


# ---------- configuration ----------


def test_only_the_cookiecloud_reference_gates_runnability() -> None:
    cfg = _configure_pixiv(cookiecloud=_register_cookiecloud(uuid='', password=''))

    assert cfg.validate_runnable() == ['cookiecloud.uuid', 'cookiecloud.password']

    cfg.cookiecloud = 'ghost'
    assert cfg.validate_runnable() == ["cookiecloud (no config named 'ghost')"]

    settings.load().cookiecloud.configs.clear()
    assert _runnable_cfg().validate_runnable() == []


def test_a_user_id_override_must_be_digits() -> None:
    assert settings.Pixiv(user_id=' 21029043 ').user_id == '21029043'
    assert settings.Pixiv(user_id='').user_id == ''
    with pytest.raises(ValueError, match='user_id'):
        settings.Pixiv(user_id='not-a-uid')


def test_a_negative_request_interval_is_rejected() -> None:
    with pytest.raises(ValueError, match='sleep_request_seconds'):
        settings.Pixiv(sleep_request_seconds=-1)


# ---------- session parsing ----------


def test_the_user_id_is_derived_from_the_session_cookie() -> None:
    assert derive_user_id('21029043_AbCdEf0123456789AbCdEf0123456789') == '21029043'
    assert derive_user_id('no-underscore') == ''
    assert derive_user_id('prefix_notdigits') == ''
    assert derive_user_id('') == ''


def test_the_session_cookie_is_found_whatever_hostname_the_vault_filed_it_under() -> None:
    cookie = {'name': 'PHPSESSID', 'value': '21029043_hash'}

    assert extract_phpsessid({'pixiv.net': [cookie]}) == '21029043_hash'
    assert extract_phpsessid({'.pixiv.net': [cookie]}) == '21029043_hash'
    assert extract_phpsessid({'www.pixiv.net': [{'name': 'phpsessid', 'value': 'v'}]}) == 'v'
    assert extract_phpsessid({'pixiv.net': [{'name': 'device_token', 'value': 'x'}]}) == ''
    assert extract_phpsessid({'example.com': [cookie]}) == ''


# ---------- payload parsing ----------


def test_a_bookmark_page_becomes_typed_works() -> None:
    body = {'works': [_bookmark_entry(), _bookmark_entry(id='2', illustType=2, pageCount=1, title='anim')]}

    works = parse_bookmark_works(body)

    assert [work.illust_id for work in works] == [96461036, 2]
    assert works[0].author == 'the artist'
    assert works[0].author_id == '50944445'
    assert works[1].illust_type == 2


def test_a_masked_bookmark_is_kept_even_though_its_id_is_an_int() -> None:
    # pixiv sends a deleted/private bookmark with an int id and a placeholder title.
    body = {'works': [{'id': 130954211, 'title': '-----', 'isMasked': True}]}

    works = parse_bookmark_works(body)

    assert len(works) == 1
    assert works[0].illust_id == 130954211
    assert works[0].masked is True


def test_an_unknown_work_type_is_skipped_rather_than_downloaded_wrong() -> None:
    body = {'works': [_bookmark_entry(illustType=7), _bookmark_entry(id='5')]}

    assert [work.illust_id for work in parse_bookmark_works(body)] == [5]


def test_page_urls_keep_their_order() -> None:
    body = [
        {'urls': {'original': 'https://i.pximg.net/img-original/img/x/1_p0.jpg'}},
        {'urls': {'original': 'https://i.pximg.net/img-original/img/x/1_p1.png'}},
    ]

    assert parse_page_urls(body) == [
        'https://i.pximg.net/img-original/img/x/1_p0.jpg',
        'https://i.pximg.net/img-original/img/x/1_p1.png',
    ]
    assert parse_page_urls(None) == []


def test_ugoira_meta_yields_the_zip_and_the_frame_delays() -> None:
    body = {
        'originalSrc': 'https://i.pximg.net/img-zip-ugoira/img/x/2_ugoira1920x1080.zip',
        'frames': [{'file': '000000.jpg', 'delay': 45}, {'file': '000001.jpg', 'delay': 90}],
    }

    zip_url, frames = parse_ugoira_meta(body)

    assert zip_url.endswith('_ugoira1920x1080.zip')
    assert frames == [('000000.jpg', 45), ('000001.jpg', 90)]


def test_an_empty_ugoira_meta_raises_instead_of_encoding_nothing() -> None:
    with pytest.raises(ValueError, match='ugoira_meta'):
        parse_ugoira_meta({'originalSrc': '', 'frames': []})


def test_the_extension_comes_from_the_original_url() -> None:
    assert infer_extension('https://i.pximg.net/img-original/img/x/1_p0.png') == 'png'
    assert infer_extension('https://i.pximg.net/no-extension') == 'jpg'


# ---------- ugoira synthesis ----------


def _write_frames(directory: Path, colors: list[str]) -> list[Path]:
    paths = []
    for index, color in enumerate(colors):
        path = directory / f'{index:06d}.png'
        Image.new('RGB', (8, 8), color).save(path)
        paths.append(path)
    return paths


def test_frames_become_an_animated_webp_honoring_each_delay(tmp_path) -> None:
    frame_paths = _write_frames(tmp_path, ['red', 'lime', 'blue'])
    dst = tmp_path / 'out.webp'

    synthesize_ugoira_webp(frame_paths, [100, 200, 300], dst)

    with Image.open(dst) as image:
        assert image.format == 'WEBP'
        assert image.is_animated
        assert image.n_frames == 3
        durations = []
        for index in range(image.n_frames):
            image.seek(index)
            image.load()
            durations.append(image.info['duration'])
    assert durations == [100, 200, 300]


def test_the_zip_pipeline_extracts_encodes_and_moves_into_place(tmp_path) -> None:
    frame_paths = _write_frames(tmp_path, ['red', 'blue'])
    zip_path = tmp_path / 'src.zip'
    with zipfile.ZipFile(zip_path, 'w') as archive:
        for path in frame_paths:
            archive.write(path, path.name)
    dst = tmp_path / 'nested' / 'out.webp'
    dst.parent.mkdir()

    Pixiv._synthesize_from_zip(zip_path.read_bytes(), [('000000.png', 50), ('000001.png', 50)], dst)

    with Image.open(dst) as image:
        assert image.is_animated
        assert image.n_frames == 2


def test_a_zip_missing_a_listed_frame_fails_loudly(tmp_path) -> None:
    zip_path = tmp_path / 'src.zip'
    with zipfile.ZipFile(zip_path, 'w') as archive:
        archive.writestr('000000.png', b'x')

    with pytest.raises(PixivError, match='missing frame'):
        Pixiv._synthesize_from_zip(zip_path.read_bytes(), [('000001.png', 50)], tmp_path / 'out.webp')


# ---------- filenames ----------


def test_files_land_under_the_author_with_the_repo_filename_shape() -> None:
    _runnable_cfg(path=Path('/data/pixiv'))
    source = Pixiv()

    illust = source._build_output_path(_work(page_count=2), num=1, ext='png')
    assert illust == Path('/data/pixiv/the artist/[the artist]a title [96461036_p1].png')

    ugoira = source._build_output_path(_work(illust_type=2), num=0, ext='webp')
    assert ugoira == Path('/data/pixiv/the artist/[the artist]a title [96461036_ugoira].webp')


def test_an_author_without_a_name_falls_back_to_the_id_directory() -> None:
    _runnable_cfg(path=Path('/data/pixiv'))
    source = Pixiv()

    path = source._build_output_path(_work(author='', author_id='42'), num=0, ext='jpg')

    assert path.parent == Path('/data/pixiv/42')
    # No empty [] prefix either: an unnamed author drops the uploader segment.
    assert path.name == 'a title [96461036_p0].jpg'


# ---------- clients ----------


def test_the_media_client_carries_the_referer_but_never_the_session() -> None:
    _runnable_cfg()
    source = Pixiv()

    assert source.media_client.headers['Referer'] == 'https://www.pixiv.net/'
    assert 'Cookie' not in source.media_client.headers
    assert source.client.headers['Referer'] == 'https://www.pixiv.net/'


def test_the_bookmarks_request_asks_for_public_bookmarks_page_by_page() -> None:
    _runnable_cfg()
    source = Pixiv()
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return httpx.Response(200, json={'error': False, 'body': {'total': 1, 'works': []}})

    _with_mock_ajax(source, handler)

    body = asyncio.run(source._fetch_bookmark_page('21029043', 96))

    assert body == {'total': 1, 'works': []}
    request = seen[0]
    assert request.url.path == '/ajax/user/21029043/illusts/bookmarks'
    assert request.url.params['rest'] == 'show'
    assert request.url.params['limit'] == '48'
    assert request.url.params['offset'] == '96'
    assert request.url.params['tag'] == ''


def test_a_rejected_session_becomes_one_deduplicated_auth_alert() -> None:
    _runnable_cfg()
    source = Pixiv()
    _with_mock_ajax(source, lambda _request: httpx.Response(400, json={'error': True, 'message': 'invalid request', 'body': []}))

    with pytest.raises(PixivError) as excinfo:
        asyncio.run(source._fetch_bookmark_page('21029043', 0))

    assert excinfo.value.notification_dedupe_key == 'pixiv:auth'


def test_a_work_endpoint_error_keeps_its_message_for_the_unavailable_mark() -> None:
    _runnable_cfg()
    source = Pixiv()
    _with_mock_ajax(source, lambda _request: httpx.Response(404, json={'error': True, 'message': 'deleted', 'body': []}))

    with pytest.raises(PixivApiError, match='deleted') as excinfo:
        asyncio.run(source._get_body('https://www.pixiv.net/ajax/illust/1/pages'))

    assert excinfo.value.status_code == 404


def test_a_download_writes_the_original_and_skips_files_already_on_disk(tmp_path) -> None:
    _runnable_cfg()
    source = Pixiv()
    requests: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return httpx.Response(200, content=b'image-bytes')

    _with_mock_media(source, handler)
    dst = tmp_path / 'artist' / 'file.jpg'

    assert asyncio.run(source._download_image('https://i.pximg.net/img-original/img/x/1_p0.jpg', dst)) is True
    assert dst.read_bytes() == b'image-bytes'
    assert requests[0].headers['Referer'] == 'https://www.pixiv.net/'
    assert 'Cookie' not in requests[0].headers

    # Already on disk: no second request, no rewrite.
    assert asyncio.run(source._download_image('https://i.pximg.net/img-original/img/x/1_p0.jpg', dst)) is False
    assert len(requests) == 1


# ---------- crawl ----------


def test_the_crawl_stops_after_consecutive_fully_archived_pages(monkeypatch) -> None:
    _runnable_cfg()
    source = Pixiv()
    seen: list[int] = []

    def handler(request: httpx.Request) -> httpx.Response:
        offset = int(request.url.params['offset'])
        seen.append(offset)
        works = [_bookmark_entry(id=str(offset + index + 1)) for index in range(2)]
        return httpx.Response(200, json={'error': False, 'body': {'total': 1000, 'works': works}})

    _with_mock_ajax(source, handler)

    async def _all_processed(illust_ids: list[int]) -> set[int]:
        return set(illust_ids)

    monkeypatch.setattr(source, '_processed_work_ids', _all_processed)

    works, pages, total = asyncio.run(source._crawl_bookmarks('21029043'))

    assert works == []
    assert pages == 2
    assert total == 1000
    assert seen == [0, 2]


def test_a_masked_bookmark_is_settled_as_unavailable_during_the_walk(monkeypatch) -> None:
    _runnable_cfg()
    source = Pixiv()
    fake_db = _FakeDatabase()
    monkeypatch.setattr(pixiv_module, 'database', fake_db)

    pages = iter(
        [
            {'total': 1, 'works': [{'id': 130954211, 'title': '-----', 'isMasked': True}]},
        ],
    )

    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json={'error': False, 'body': next(pages, {'total': 1, 'works': []})})

    _with_mock_ajax(source, handler)

    works, _pages, _total = asyncio.run(source._crawl_bookmarks('21029043'))

    assert works == []
    inserts = [call for call in fake_db.calls if 'INSERT OR IGNORE' in call[0]]
    updates = [call for call in fake_db.calls if call[0].strip().startswith('UPDATE')]
    assert inserts
    assert inserts[0][1][0] == '130954211'
    assert updates
    assert 'unavailable = 1' in updates[0][0]


def test_an_unconfigured_source_does_nothing_rather_than_touching_the_network(monkeypatch) -> None:
    _configure_pixiv(cookiecloud='')

    def _explode(*_args, **_kwargs):
        raise AssertionError('should not touch the database or the network')

    monkeypatch.setattr(pixiv_module, 'database', _explode)
    monkeypatch.setattr(pixiv_module, 'CookieCloudClient', _explode)

    asyncio.run(Pixiv().update())


def test_a_session_without_an_embedded_uid_asks_for_an_explicit_override(monkeypatch) -> None:
    _runnable_cfg(user_id='')
    source = Pixiv()
    monkeypatch.setattr(source, '_fetch_phpsessid', lambda: 'strange-cookie-shape')

    with pytest.raises(PixivError, match='user_id') as excinfo:
        asyncio.run(source.update())

    assert excinfo.value.notification_dedupe_key == 'pixiv:auth'


# ---------- registration ----------


def test_the_job_is_registered_and_parked_until_configured() -> None:
    fake_config = settings.Settings()
    fake_config.web.pixiv.enabled = True

    job = next(job for job in jobs_module.build_jobs(fake_config) if job.key == 'pixiv')

    assert job.name == 'Pixiv'
    assert job.section == 'web.pixiv'
    assert job.required_commands == ()
    assert job.factory is jobs_module.Pixiv
    # Enabled in the UI but with no credentials, so it stays out of the scheduler.
    assert job.enabled is False
    assert 'cookiecloud' in job.missing_fields


def test_api_job_enum_includes_pixiv() -> None:
    assert JobRequestTarget.PIXIV.value == 'pixiv'


def test_the_settings_section_and_cookiecloud_profile_are_registered() -> None:
    assert settings.SECTION_MODELS['web.pixiv'] is settings.Pixiv
    assert PROFILES['pixiv'].required_cookies == ('phpsessid',)
    assert 'pixiv.net' in PROFILES['pixiv'].domains


def test_the_archive_links_a_row_back_to_the_artwork() -> None:
    source = ARCHIVE_SOURCES['pixiv']

    assert source.table == 'pixiv'
    assert _external_url(source, {'illust_id': 96461036, 'num': 0}) == 'https://www.pixiv.net/artworks/96461036'


# ---------- masking ----------


def test_the_shared_cookiecloud_password_survives_the_masking_round_trip() -> None:
    # The credentials live in the shared `cookiecloud` section now; the pixiv
    # section carries only a plaintext reference to one of its entries.
    stored = {'configs': [{'name': 'cc', 'server_url': 'https://cc.example', 'uuid': 'u', 'password': 'secret-password'}]}

    masked = mask_section('cookiecloud', stored)
    assert masked['configs'][0]['password'] == 'secr••••'
    # Masking never mutates what is stored.
    assert stored['configs'][0]['password'] == 'secret-password'

    merged = unmask_section('cookiecloud', masked, stored)
    assert merged['configs'][0]['password'] == 'secret-password'

    section = {'cookiecloud': 'cc'}
    assert mask_section('web.pixiv', section) == section
