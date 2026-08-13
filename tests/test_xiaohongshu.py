# ruff: noqa: ANN001, ANN002, ANN003, ANN202, ARG001, EM101, EM102, INP001, PLR2004, S101, S105, S106, SLF001, TRY003

import asyncio
from pathlib import Path

import httpx
import pytest
from xhshow import Xhshow

import src.service.jobs as jobs_module
import src.web.xiaohongshu as xiaohongshu_module
from src.api.archive import ARCHIVE_SOURCES, _external_url
from src.api.schemas import JobRequestTarget
from src.core import settings
from src.tool.cookiecloud import PROFILES
from src.web.xiaohongshu import (
    MediaUnavailableError,
    NoteRef,
    VideoDownloadError,
    XhsMedia,
    Xiaohongshu,
    XiaohongshuError,
    build_media_filename,
    build_note_url,
    build_ytdlp_command,
    extract_like_page,
    extract_note_media,
    infer_image_extension,
    is_note_gone,
)

# 2025-08-12 11:34:56 in the timezone the app displays.
NOTE_TIME_MS = 1754969696000
NOTE_ID = '64f1a2b3000000001e02c0de'
OTHER_NOTE_ID = '64f1a2b3000000001e02beef'


def _configure(**updates: object) -> settings.Xiaohongshu:
    """Mutate the pinned settings snapshot that Xiaohongshu() reads in __init__."""
    cfg = settings.load().web.xiaohongshu
    for key, value in updates.items():
        setattr(cfg, key, value)
    return cfg


def _runnable_cfg(**updates: object) -> settings.Xiaohongshu:
    cookiecloud = settings.CookieCloud(server_url='https://cc.example', uuid='u', password='pw')
    updates.setdefault('sleep_request_seconds', 0.0)
    return _configure(cookiecloud=cookiecloud, **updates)


async def _no_requests(request: httpx.Request) -> httpx.Response:
    raise AssertionError(f'unexpected request to {request.url}')


def _job(handler=_no_requests, **updates) -> Xiaohongshu:
    """A source wired to a mock transport, with a session already loaded."""
    _runnable_cfg(**updates)
    job = Xiaohongshu.__new__(Xiaohongshu)
    job.cfg = settings.load().web.xiaohongshu
    job.client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
    job.signer = Xhshow()
    job.sign_formats = {}
    # A real a1: the signature is derived from it, and xhshow rejects a missing one.
    job.cookies = {'a1': Xhshow.generate_a1(), 'web_session': 'session-value'}
    job.user_id = '5ff0000000000000000abcde'
    return job


def _run(job: Xiaohongshu, coro):
    try:
        return asyncio.run(coro)
    finally:
        asyncio.run(job.client.aclose())


class _FakeDatabase:
    """Records the SQL a run issues instead of talking to PostgreSQL."""

    def __init__(
        self,
        *,
        known_note_ids: tuple[str, ...] = (),
        pending_rows: tuple[dict, ...] = (),
        backfill_complete: bool = True,
    ) -> None:
        self.known_note_ids = set(known_note_ids)
        self.pending_rows = list(pending_rows)
        self.backfill_complete = backfill_complete
        self.calls: list[tuple[str, tuple]] = []
        self.inserted: list[tuple[str, tuple[str, ...], list[tuple[str, ...]], str | None]] = []

    async def query_db(self, query: str, params: tuple = ()) -> list[dict]:
        self.calls.append((query, params))
        normalized = ' '.join(query.split())
        if normalized.startswith('SELECT DISTINCT note_id'):
            return [{'note_id': note_id} for note_id in params if note_id in self.known_note_ids]
        if normalized.startswith('SELECT note_id, media_index'):
            return list(self.pending_rows)
        if normalized.startswith('SELECT value FROM xiaohongshu_state'):
            return [{'value': '1'}] if self.backfill_complete else []
        return []

    async def insert_db_batch(self, *, table: str, columns: tuple[str, ...], rows, on_conflict: str | None = None) -> None:
        self.inserted.append((table, columns, list(rows), on_conflict))
        # Enough of an upsert for a run to read back what it just wrote.
        self.pending_rows.extend(dict(zip(columns, row, strict=True)) for row in rows)

    def updates(self) -> list[str]:
        return [' '.join(query.split()) for query, _ in self.calls if query.strip().upper().startswith('UPDATE')]


def _image_note_card(**updates) -> dict:
    payload = {
        'type': 'normal',
        'title': 'a caption',
        'time': NOTE_TIME_MS,
        'user': {'nickname': 'artist', 'user_id': '5ff0000000000000000fffff'},
        'image_list': [
            {
                'info_list': [
                    {'image_scene': 'WB_PRV', 'url': 'https://sns-webpic.xhscdn.com/1!nd_prv_wlteh_webp_3'},
                    {'image_scene': 'WB_DFT', 'url': 'https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3'},
                ],
            },
        ],
    }
    payload.update(updates)
    return payload


def _video_note_card(**updates) -> dict:
    payload = {
        'type': 'video',
        'title': 'a clip',
        'time': NOTE_TIME_MS,
        'user': {'nickname': 'artist', 'user_id': '5ff0000000000000000fffff'},
        # A video note also carries a cover image; it is deliberately not archived.
        'image_list': [{'info_list': [{'image_scene': 'WB_DFT', 'url': 'https://sns-webpic.xhscdn.com/cover!nd_dft'}]}],
        'video': {
            'media': {
                'stream': {
                    'h264': [{'master_url': 'https://sns-video.xhscdn.com/stream/h264.mp4'}],
                    'h265': [{'master_url': 'https://sns-video.xhscdn.com/stream/h265.mp4'}],
                },
            },
        },
    }
    payload.update(updates)
    return payload


def _media(**updates) -> XhsMedia:
    fields = {
        'note_id': NOTE_ID,
        'media_index': 1,
        'media_type': 'image',
        'media_url': 'https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3',
        'title': 'a caption',
        'author': 'artist',
        'author_id': '5ff0000000000000000fffff',
        'note_type': 'normal',
        'published_at': '2025-08-12 11:34:56',
        'xsec_token': 'TOKEN',
    }
    fields.update(updates)
    return XhsMedia(**fields)


def _api_response(data: dict, *, code: int = 0) -> httpx.Response:
    return httpx.Response(200, json={'code': code, 'success': code == 0, 'msg': 'ok', 'data': data})


# ---------- configuration ----------


def test_missing_cookiecloud_fields_are_named_individually() -> None:
    cfg = _configure(cookiecloud=settings.CookieCloud(server_url='https://cc.example'))

    assert cfg.validate_runnable() == ['cookiecloud.uuid', 'cookiecloud.password']


def test_a_source_with_a_vault_is_runnable_without_a_user_id() -> None:
    # The profile id is resolved from the session, so it is not a required field.
    assert _runnable_cfg().validate_runnable() == []


def test_an_empty_video_path_is_not_read_as_the_working_directory() -> None:
    # Path('') is Path('.'), which would put every video in the process's cwd.
    assert settings.Xiaohongshu(video_path='').video_path is None
    assert settings.Xiaohongshu(video_path='   ').video_path is None


def test_the_request_interval_cannot_be_negative() -> None:
    with pytest.raises(ValueError, match='sleep_request_seconds'):
        settings.Xiaohongshu(sleep_request_seconds=-1)


def test_abort_after_must_leave_room_for_at_least_one_page() -> None:
    with pytest.raises(ValueError, match='abort_after'):
        settings.Xiaohongshu(abort_after=0)


def test_the_cookie_profile_names_what_signing_and_the_session_need() -> None:
    profile = PROFILES['xiaohongshu']

    # a1 is what requests are signed with; web_session is what makes them the user's.
    assert profile.required_cookies == ('a1', 'web_session')
    assert 'xiaohongshu.com' in profile.domains


# ---------- reading the likes list ----------


def test_a_likes_page_yields_its_notes_with_the_cursor_for_the_next_one() -> None:
    notes, cursor, has_more = extract_like_page(
        {
            'notes': [
                {'note_id': NOTE_ID, 'xsec_token': 'TOKEN', 'type': 'normal'},
                {'id': OTHER_NOTE_ID, 'xsec_token': 'OTHER'},
            ],
            'cursor': 'CURSOR-2',
            'has_more': True,
        },
    )

    assert [note.note_id for note in notes] == [NOTE_ID, OTHER_NOTE_ID]
    assert notes[0].xsec_token == 'TOKEN'
    assert cursor == 'CURSOR-2'
    assert has_more is True


def test_an_entry_without_an_id_is_skipped_rather_than_crashing_the_page() -> None:
    notes, cursor, has_more = extract_like_page({'notes': [{'xsec_token': 'T'}, 'not-a-dict', {'note_id': NOTE_ID}]})

    assert [note.note_id for note in notes] == [NOTE_ID]
    assert cursor == ''
    assert has_more is False


# ---------- reading one note ----------


def test_an_image_note_prefers_the_full_size_variant() -> None:
    media = extract_note_media(_image_note_card(), note_id=NOTE_ID, xsec_token='TOKEN')

    assert len(media) == 1
    assert media[0].media_type == 'image'
    assert media[0].media_url.endswith('nd_dft_wlteh_webp_3')
    assert media[0].media_index == 1
    assert media[0].author == 'artist'
    assert media[0].published_at == '2025-08-12 11:34:56'
    assert media[0].xsec_token == 'TOKEN'


def test_an_image_without_the_preferred_variant_still_yields_a_file() -> None:
    card = _image_note_card(image_list=[{'info_list': [{'image_scene': 'WB_PRV', 'url': 'https://cdn/only-preview'}]}])

    media = extract_note_media(card, note_id=NOTE_ID, xsec_token='T')

    assert [item.media_url for item in media] == ['https://cdn/only-preview']


def test_a_live_photo_is_kept_as_both_the_still_and_the_clip() -> None:
    card = _image_note_card(
        image_list=[
            {'info_list': [{'image_scene': 'WB_DFT', 'url': 'https://cdn/plain-1'}]},
            {
                'info_list': [{'image_scene': 'WB_DFT', 'url': 'https://cdn/live-still'}],
                'stream': {'h264': [{'master_url': 'https://cdn/live-clip.mp4'}]},
            },
            {'info_list': [{'image_scene': 'WB_DFT', 'url': 'https://cdn/plain-2'}]},
        ],
    )

    media = extract_note_media(card, note_id=NOTE_ID, xsec_token='T')

    # Indices are handed out in file order, the clip counting as its own file: the
    # refresh path looks a row back up by this number.
    assert [(item.media_index, item.media_type) for item in media] == [
        (1, 'image'),
        (2, 'image'),
        (3, 'live'),
        (4, 'image'),
    ]
    assert media[2].media_url == 'https://cdn/live-clip.mp4'


def test_a_video_note_is_one_file_and_its_cover_is_not_archived() -> None:
    media = extract_note_media(_video_note_card(), note_id=NOTE_ID, xsec_token='T')

    assert [(item.media_index, item.media_type) for item in media] == [(1, 'video')]
    assert media[0].media_url == 'https://sns-video.xhscdn.com/stream/h264.mp4'


def test_a_video_note_falls_back_to_the_next_codec() -> None:
    card = _video_note_card(video={'media': {'stream': {'h265': [{'master_url': 'https://cdn/h265.mp4'}]}}})

    media = extract_note_media(card, note_id=NOTE_ID, xsec_token='T')

    assert media[0].media_url == 'https://cdn/h265.mp4'


def test_a_video_note_yields_a_row_even_with_no_readable_stream() -> None:
    # yt-dlp downloads it from the note page, so an unreadable stream block is not
    # a reason to drop the note on the floor.
    media = extract_note_media(_video_note_card(video={}), note_id=NOTE_ID, xsec_token='T')

    assert [(item.media_index, item.media_type, item.media_url) for item in media] == [(1, 'video', '')]


def test_a_note_without_a_time_still_produces_rows() -> None:
    media = extract_note_media(_image_note_card(time=None), note_id=NOTE_ID, xsec_token='T')

    assert media[0].published_at == ''


# ---------- naming and links ----------


def test_the_format_is_read_out_of_the_url_when_there_is_no_extension() -> None:
    assert infer_image_extension('https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3') == 'webp'
    # The host is `sns-webpic`, so reading the whole URL would call every image a webp.
    assert infer_image_extension('https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_avif_3') == 'avif'
    assert infer_image_extension('https://sns-webpic.xhscdn.com/photo.jpeg') == 'jpg'
    assert infer_image_extension('https://sns-webpic.xhscdn.com/opaque') == 'jpg'


def test_a_filename_carries_the_date_the_note_was_posted() -> None:
    assert build_media_filename(_media()) == f'[artist]2025-08-12 [{NOTE_ID}_1].webp'


def test_videos_and_live_clips_are_named_as_mp4() -> None:
    assert build_media_filename(_media(media_type='video', media_index=1, media_url='')).endswith('.mp4')
    assert build_media_filename(_media(media_type='live', media_index=3, media_url='https://cdn/x')).endswith('.mp4')


def test_a_note_link_carries_the_token_that_makes_it_readable() -> None:
    assert build_note_url(NOTE_ID, 'TOKEN') == f'https://www.xiaohongshu.com/explore/{NOTE_ID}?xsec_token=TOKEN&xsec_source=pc_user'
    assert build_note_url(NOTE_ID) == f'https://www.xiaohongshu.com/explore/{NOTE_ID}'


# ---------- yt-dlp invocation ----------


def test_the_video_command_hands_yt_dlp_the_session_and_the_note_page() -> None:
    command = build_ytdlp_command(
        note_url=build_note_url(NOTE_ID, 'TOKEN'),
        cookie_path=Path('/run/cookies/c.txt'),
        output_template=Path('/cache') / f'{NOTE_ID}.%(ext)s',
    )

    assert command[0] == 'yt-dlp'
    assert command[command.index('--cookies') + 1] == '/run/cookies/c.txt'
    assert command[command.index('-o') + 1] == f'/cache/{NOTE_ID}.%(ext)s'
    assert command[command.index('--referer') + 1] == 'https://www.xiaohongshu.com/'
    # The same User-Agent the API requests are signed as.
    assert command[command.index('--add-header') + 1] == f'User-Agent:{xiaohongshu_module._USER_AGENT}'
    # Without the token yt-dlp gets a page that only opens for the note's author.
    assert command[-1].endswith('?xsec_token=TOKEN&xsec_source=pc_user')


def test_the_installed_yt_dlp_still_claims_xiaohongshu_note_pages() -> None:
    """Guard the unattended upgrade path.

    yt-dlp is bumped and merged by Dependabot without a human looking. Every other
    test here only checks the strings this module builds; this one asks the installed
    yt-dlp whether it still has an extractor for the URL they are built around.
    """
    from yt_dlp.extractor import gen_extractor_classes  # noqa: PLC0415

    url = build_note_url(NOTE_ID, 'TOKEN')
    matched = [extractor for extractor in gen_extractor_classes() if extractor.suitable(url) and extractor.IE_NAME != 'generic']

    assert [extractor.IE_NAME for extractor in matched] == ['XiaoHongShu']


def test_a_deleted_note_is_told_apart_from_a_bad_night() -> None:
    assert is_note_gone('ERROR: [XiaoHongShu] 当前笔记暂时无法浏览') is True
    assert is_note_gone('ERROR: unable to download webpage: timed out') is False


# ---------- signed requests ----------


def test_a_request_is_signed_and_carries_the_session() -> None:
    seen: list[httpx.Request] = []

    async def _handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return _api_response({'notes': [], 'cursor': '', 'has_more': False})

    job = _job(_handler)

    _run(job, job._fetch_like_page(cursor='CURSOR-1'))

    request = seen[0]
    assert request.url.path == '/api/sns/web/v1/note/like/page'
    assert dict(request.url.params) == {'user_id': job.user_id, 'num': '30', 'cursor': 'CURSOR-1'}
    assert request.headers['x-s']
    assert request.headers['x-s-common']
    assert request.headers['x-t']
    assert 'web_session=session-value' in request.headers['cookie']


def test_the_feed_request_asks_for_the_note_the_way_the_web_client_does() -> None:
    import json  # noqa: PLC0415

    seen: list[httpx.Request] = []

    async def _handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return _api_response({'items': [{'note_card': _image_note_card()}]})

    job = _job(_handler)

    note_card = _run(job, job._fetch_note_card(note_id=NOTE_ID, xsec_token='TOKEN'))

    body = json.loads(seen[0].content)
    assert body['source_note_id'] == NOTE_ID
    assert body['xsec_token'] == 'TOKEN'
    assert body['image_formats'] == ['jpg', 'webp', 'avif']
    # The feed endpoint is one of the risk-controlled ones.
    assert seen[0].headers['x-rap-param']
    assert note_card['title'] == 'a caption'


def test_a_rejected_signature_is_retried_in_the_other_envelope() -> None:
    attempts = 0

    async def _handler(request: httpx.Request) -> httpx.Response:
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            return httpx.Response(406)
        return _api_response({'notes': [], 'cursor': '', 'has_more': False})

    job = _job(_handler)
    started_as = job._sign_format_for(xiaohongshu_module._LIKE_PAGE_URI)

    _run(job, job._fetch_like_page(cursor=''))

    assert attempts == 2
    # Remembered per endpoint: the feed does not have to agree with the likes list,
    # and one shared setting would flip-flop and pay a rejection every request.
    assert job._sign_format_for(xiaohongshu_module._LIKE_PAGE_URI) != started_as
    assert job._sign_format_for(xiaohongshu_module._FEED_URI) == started_as


def test_the_captcha_wall_ends_the_run_instead_of_being_hammered() -> None:
    attempts = 0

    async def _handler(request: httpx.Request) -> httpx.Response:
        nonlocal attempts
        attempts += 1
        return httpx.Response(461)

    job = _job(_handler)

    with pytest.raises(XiaohongshuError) as excinfo:
        _run(job, job._fetch_like_page(cursor=''))

    assert excinfo.value.notification_dedupe_key == 'xiaohongshu:risk'
    assert attempts == 1


def test_a_signed_out_session_is_reported_as_an_auth_failure() -> None:
    async def _handler(request: httpx.Request) -> httpx.Response:
        return _api_response({}, code=-101)

    job = _job(_handler)

    with pytest.raises(XiaohongshuError) as excinfo:
        _run(job, job._fetch_like_page(cursor=''))

    assert excinfo.value.notification_dedupe_key == 'xiaohongshu:auth'


def test_throttling_is_retried_with_a_growing_delay(monkeypatch) -> None:
    sleeps: list[float] = []
    attempts = 0

    async def _fake_sleep(delay: float) -> None:
        sleeps.append(delay)

    async def _handler(request: httpx.Request) -> httpx.Response:
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            return httpx.Response(429)
        return _api_response({'notes': [], 'cursor': '', 'has_more': False})

    monkeypatch.setattr(xiaohongshu_module.asyncio, 'sleep', _fake_sleep)
    job = _job(_handler)

    _run(job, job._fetch_like_page(cursor=''))

    assert attempts == 2
    assert sleeps == [2.0]


def test_the_profile_id_is_resolved_from_the_session_when_it_is_not_configured() -> None:
    async def _handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == '/api/sns/web/v2/user/me'
        return _api_response({'user_id': 'resolved-id', 'nickname': 'me'})

    job = _job(_handler)

    assert _run(job, job._resolve_user_id()) == 'resolved-id'


def test_a_configured_profile_id_skips_the_lookup() -> None:
    job = _job(user_id='configured-id')

    assert _run(job, job._resolve_user_id()) == 'configured-id'


# ---------- walking the likes list ----------


def _like_page_handler(pages: dict[str, dict]):
    """Serves the likes list, plus a one-image note for whatever the walk resolves."""

    async def _handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == xiaohongshu_module._FEED_URI:
            return _api_response({'items': [{'note_card': _image_note_card()}]})
        return _api_response(pages[request.url.params.get('cursor', '')])

    return _handler


def _resolved_note_ids(fake_db: _FakeDatabase) -> list[str]:
    return [row[0] for _table, _columns, rows, _conflict in fake_db.inserted for row in rows]


def test_the_crawl_stops_once_it_has_caught_up(monkeypatch) -> None:
    pages = {
        '': {'notes': [{'note_id': 'new-1', 'xsec_token': 'T1'}], 'cursor': 'C1', 'has_more': True},
        'C1': {'notes': [{'note_id': 'old-1', 'xsec_token': 'T2'}], 'cursor': 'C2', 'has_more': True},
        'C2': {'notes': [{'note_id': 'old-2', 'xsec_token': 'T3'}], 'cursor': 'C3', 'has_more': True},
        # Reaching this page would mean the stop rule never fired.
        'C3': {'notes': [{'note_id': 'new-2', 'xsec_token': 'T4'}], 'cursor': 'C4', 'has_more': True},
    }
    fake_db = _FakeDatabase(known_note_ids=('old-1', 'old-2'))
    monkeypatch.setattr(xiaohongshu_module, 'database', fake_db)
    job = _job(_like_page_handler(pages), abort_after=2)

    assert _run(job, job._crawl()) == 1
    assert _resolved_note_ids(fake_db) == ['new-1']


def test_a_page_with_anything_new_resets_the_stop_counter(monkeypatch) -> None:
    pages = {
        '': {'notes': [{'note_id': 'old-1', 'xsec_token': 'T'}], 'cursor': 'C1', 'has_more': True},
        'C1': {'notes': [{'note_id': 'new-1', 'xsec_token': 'T'}], 'cursor': 'C2', 'has_more': True},
        'C2': {'notes': [{'note_id': 'old-2', 'xsec_token': 'T'}], 'cursor': 'C3', 'has_more': True},
        'C3': {'notes': [{'note_id': 'old-3', 'xsec_token': 'T'}], 'cursor': 'C4', 'has_more': False},
    }
    fake_db = _FakeDatabase(known_note_ids=('old-1', 'old-2', 'old-3'))
    monkeypatch.setattr(xiaohongshu_module, 'database', fake_db)
    job = _job(_like_page_handler(pages), abort_after=2)

    _run(job, job._crawl())

    assert _resolved_note_ids(fake_db) == ['new-1']


def test_the_first_run_walks_past_notes_it_has_already_stored(monkeypatch) -> None:
    # A backfill that died part-way leaves an archived prefix. Stopping on it would
    # put everything below wherever that run got to out of reach for good.
    pages = {
        '': {'notes': [{'note_id': 'old-1', 'xsec_token': 'T'}], 'cursor': 'C1', 'has_more': True},
        'C1': {'notes': [{'note_id': 'old-2', 'xsec_token': 'T'}], 'cursor': 'C2', 'has_more': True},
        'C2': {'notes': [{'note_id': 'old-3', 'xsec_token': 'T'}], 'cursor': 'C3', 'has_more': True},
        'C3': {'notes': [{'note_id': 'new-1', 'xsec_token': 'T'}], 'cursor': 'C4', 'has_more': False},
    }
    fake_db = _FakeDatabase(known_note_ids=('old-1', 'old-2', 'old-3'), backfill_complete=False)
    monkeypatch.setattr(xiaohongshu_module, 'database', fake_db)
    job = _job(_like_page_handler(pages), abort_after=2)

    _run(job, job._crawl())

    assert _resolved_note_ids(fake_db) == ['new-1']
    # Reaching the end of the list is what lets later runs stop early.
    assert any('INSERT OR IGNORE INTO xiaohongshu_state' in query for query, _ in fake_db.calls)


def test_a_walk_that_stopped_early_does_not_claim_the_backfill_finished(monkeypatch) -> None:
    pages = {
        '': {'notes': [{'note_id': 'old-1', 'xsec_token': 'T'}], 'cursor': 'C1', 'has_more': True},
        'C1': {'notes': [{'note_id': 'old-2', 'xsec_token': 'T'}], 'cursor': '', 'has_more': True},
    }
    fake_db = _FakeDatabase(known_note_ids=('old-1', 'old-2'), backfill_complete=False)
    monkeypatch.setattr(xiaohongshu_module, 'database', fake_db)
    job = _job(_like_page_handler(pages), abort_after=2)

    # The cursor guard ends this walk, which is not the same as reaching the end.
    _run(job, job._crawl())

    assert not any('INSERT OR IGNORE INTO xiaohongshu_state' in query for query, _ in fake_db.calls)


def test_the_crawl_stops_when_the_cursor_repeats_itself(monkeypatch) -> None:
    pages = {'': {'notes': [{'note_id': 'new-1', 'xsec_token': 'T'}], 'cursor': '', 'has_more': True}}
    fake_db = _FakeDatabase()
    monkeypatch.setattr(xiaohongshu_module, 'database', fake_db)
    job = _job(_like_page_handler(pages), abort_after=5)

    _run(job, job._crawl())

    assert _resolved_note_ids(fake_db) == ['new-1']


def test_resolving_a_note_writes_one_row_per_file(monkeypatch) -> None:
    fake_db = _FakeDatabase()
    monkeypatch.setattr(xiaohongshu_module, 'database', fake_db)

    async def _handler(request: httpx.Request) -> httpx.Response:
        return _api_response({'items': [{'note_card': _image_note_card()}]})

    job = _job(_handler)

    assert _run(job, job._resolve_notes([NoteRef(note_id=NOTE_ID, xsec_token='TOKEN')])) == 1

    table, _columns, rows, on_conflict = fake_db.inserted[0]
    assert table == 'xiaohongshu'
    assert rows[0][:3] == (NOTE_ID, '1', 'image')
    # A rotated URL or token refreshes without resetting what has been downloaded.
    assert 'DO UPDATE SET media_url = excluded.media_url' in (on_conflict or '')
    assert 'downloaded' not in (on_conflict or '')


def test_a_note_that_cannot_be_read_does_not_end_the_run(monkeypatch) -> None:
    fake_db = _FakeDatabase()
    monkeypatch.setattr(xiaohongshu_module, 'database', fake_db)

    async def _handler(request: httpx.Request) -> httpx.Response:
        import json  # noqa: PLC0415

        note_id = json.loads(request.content)['source_note_id']
        if note_id == NOTE_ID:
            return _api_response({}, code=-510001)
        return _api_response({'items': [{'note_card': _image_note_card()}]})

    job = _job(_handler)
    notes = [NoteRef(note_id=NOTE_ID, xsec_token='T'), NoteRef(note_id=OTHER_NOTE_ID, xsec_token='T')]

    assert _run(job, job._resolve_notes(notes)) == 1


# ---------- where files land ----------


def test_an_unset_video_path_keeps_everything_in_one_place() -> None:
    job = _job(path=Path('/data/xhs'), video_path=None)

    assert job._destination_root('video') == Path('/data/xhs')
    assert job._destination_root('image') == Path('/data/xhs')
    asyncio.run(job.client.aclose())


def test_videos_and_live_clips_go_to_the_video_path_while_images_stay() -> None:
    job = _job(path=Path('/data/xhs'), video_path=Path('/media/xhs-videos'))

    assert job._destination_root('video') == Path('/media/xhs-videos')
    # A live photo's clip is an mp4 like any other.
    assert job._destination_root('live') == Path('/media/xhs-videos')
    assert job._destination_root('image') == Path('/data/xhs')
    assert job._build_output_path(_media()) == Path('/data/xhs/artist') / f'[artist]2025-08-12 [{NOTE_ID}_1].webp'
    asyncio.run(job.client.aclose())


# ---------- downloading ----------


def test_an_image_is_written_where_the_row_says_it_belongs(tmp_path) -> None:
    async def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=b'image-bytes')

    job = _job(_handler, path=tmp_path)

    dst_path = _run(job, job._download_file(_media()))

    assert dst_path == tmp_path / 'artist' / f'[artist]2025-08-12 [{NOTE_ID}_1].webp'
    assert dst_path.read_bytes() == b'image-bytes'


def test_a_file_already_on_disk_is_not_fetched_again(tmp_path) -> None:
    job = _job(path=tmp_path)
    dst_path = tmp_path / 'artist' / f'[artist]2025-08-12 [{NOTE_ID}_1].webp'
    dst_path.parent.mkdir(parents=True)
    dst_path.write_bytes(b'already here')

    # The handler raises on any request, so reaching the network fails the test.
    assert _run(job, job._download_file(_media())) == dst_path


def test_a_rotated_url_is_refreshed_from_the_note_and_retried(monkeypatch, tmp_path) -> None:
    fake_db = _FakeDatabase()
    monkeypatch.setattr(xiaohongshu_module, 'database', fake_db)
    fresh_url = 'https://sns-webpic.xhscdn.com/fresh!nd_dft_wlteh_webp_3'

    async def _handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == 'edith.xiaohongshu.com':
            card = _image_note_card(image_list=[{'info_list': [{'image_scene': 'WB_DFT', 'url': fresh_url}]}])
            return _api_response({'items': [{'note_card': card}]})
        if str(request.url) == fresh_url:
            return httpx.Response(200, content=b'fresh-bytes')
        return httpx.Response(403)

    job = _job(_handler, path=tmp_path)

    dst_path = _run(job, job._download_file(_media()))

    assert dst_path.read_bytes() == b'fresh-bytes'
    # The new URL is stored, so the next run does not pay for the round trip again.
    assert any('UPDATE xiaohongshu SET media_url' in update for update in fake_db.updates())


def test_a_file_the_note_no_longer_offers_is_marked_unavailable(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(xiaohongshu_module, 'database', _FakeDatabase())

    async def _handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == 'edith.xiaohongshu.com':
            return _api_response({'items': []})
        return httpx.Response(404)

    job = _job(_handler, path=tmp_path)

    with pytest.raises(MediaUnavailableError):
        _run(job, job._download_file(_media()))


def test_a_forbidden_file_the_note_still_offers_stays_retryable(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(xiaohongshu_module, 'database', _FakeDatabase())

    async def _handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == 'edith.xiaohongshu.com':
            # Same URL as the row already has: nothing to refresh.
            return _api_response(
                {
                    'items': [
                        {'note_card': _image_note_card(image_list=[{'info_list': [{'image_scene': 'WB_DFT', 'url': _media().media_url}]}])}
                    ]
                }
            )
        return httpx.Response(403)

    job = _job(_handler, path=tmp_path)

    with pytest.raises(httpx.HTTPStatusError):
        _run(job, job._download_file(_media()))


def test_downloading_marks_each_row_and_keeps_going_past_a_dead_file(monkeypatch, tmp_path) -> None:
    fake_db = _FakeDatabase()
    monkeypatch.setattr(xiaohongshu_module, 'database', fake_db)
    dead_url = 'https://sns-webpic.xhscdn.com/dead!nd_dft_wlteh_webp_3'

    async def _handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == 'edith.xiaohongshu.com':
            return _api_response({'items': []})
        if str(request.url) == dead_url:
            return httpx.Response(404)
        return httpx.Response(200, content=b'bytes')

    job = _job(_handler, path=tmp_path)
    pending = [_media(media_index=1, media_url=dead_url), _media(media_index=2)]

    downloaded = _run(job, job._download_pending(pending, cookie_path=tmp_path / 'c.txt', cache_dir=tmp_path / 'cache'))

    assert downloaded == 1
    updates = fake_db.updates()
    assert any(update.startswith('UPDATE xiaohongshu SET unavailable = 1') for update in updates)
    assert any(update.startswith('UPDATE xiaohongshu SET downloaded = 1') for update in updates)


def test_the_cdn_is_not_paced_like_the_api(monkeypatch, tmp_path) -> None:
    # The request interval exists for Xiaohongshu's risk control; charging it per
    # image would add hours of pure waiting to a first backfill.
    sleeps: list[float] = []

    async def _fake_sleep(delay: float) -> None:
        sleeps.append(delay)

    async def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=b'bytes')

    monkeypatch.setattr(xiaohongshu_module, 'database', _FakeDatabase())
    monkeypatch.setattr(xiaohongshu_module.asyncio, 'sleep', _fake_sleep)
    job = _job(_handler, path=tmp_path, sleep_request_seconds=3.0)
    pending = [_media(media_index=1), _media(media_index=2)]

    assert _run(job, job._download_pending(pending, cookie_path=tmp_path / 'c.txt', cache_dir=tmp_path / 'cache')) == 2
    assert sleeps == []


# ---------- video notes ----------


def _fake_ytdlp(*, exit_code: int = 0, stderr: str = '', suffix: str = '.mp4'):
    calls: list[list[str]] = []

    class _Result:
        def __init__(self, returncode: int) -> None:
            self.returncode = returncode
            self.stdout = ''
            self.stderr = stderr

    def _run_command(command, **_kwargs):
        calls.append(command)
        if exit_code == 0:
            template = Path(command[command.index('-o') + 1])
            output = template.with_name(template.name.replace('.%(ext)s', suffix))
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_bytes(b'video-bytes')
        return _Result(exit_code)

    return _run_command, calls


def test_a_video_note_is_downloaded_by_yt_dlp_and_renamed(monkeypatch, tmp_path) -> None:
    run_command, calls = _fake_ytdlp()
    monkeypatch.setattr(xiaohongshu_module.subprocess, 'run', run_command)
    job = _job(path=tmp_path / 'images', video_path=tmp_path / 'videos')
    media = _media(media_type='video', media_url='')

    dst_path = _run(job, job._download_video(media, cookie_path=tmp_path / 'c.txt', cache_dir=tmp_path / 'cache'))

    assert dst_path == tmp_path / 'videos' / 'artist' / f'[artist]2025-08-12 [{NOTE_ID}_1].mp4'
    assert dst_path.read_bytes() == b'video-bytes'
    # The cache directory is left clean for the next note.
    assert list((tmp_path / 'cache').iterdir()) == []
    assert calls[0][-1].startswith(f'https://www.xiaohongshu.com/explore/{NOTE_ID}?xsec_token=')


def test_a_video_yt_dlp_saved_as_something_else_keeps_that_extension(monkeypatch, tmp_path) -> None:
    run_command, _ = _fake_ytdlp(suffix='.mkv')
    monkeypatch.setattr(xiaohongshu_module.subprocess, 'run', run_command)
    job = _job(path=tmp_path)

    dst_path = _run(job, job._download_video(_media(media_type='video'), cookie_path=tmp_path / 'c.txt', cache_dir=tmp_path / 'cache'))

    assert dst_path.suffix == '.mkv'


def test_a_deleted_video_note_is_not_retried(monkeypatch, tmp_path) -> None:
    run_command, calls = _fake_ytdlp(exit_code=1, stderr='ERROR: [XiaoHongShu] 当前笔记暂时无法浏览')
    monkeypatch.setattr(xiaohongshu_module.subprocess, 'run', run_command)
    job = _job(path=tmp_path)

    with pytest.raises(MediaUnavailableError):
        _run(job, job._download_video(_media(media_type='video'), cookie_path=tmp_path / 'c.txt', cache_dir=tmp_path / 'cache'))

    assert len(calls) == 1


def test_a_video_that_merely_failed_is_reported_as_retryable(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(xiaohongshu_module, '_YTDLP_MAX_ATTEMPTS', 1)
    run_command, calls = _fake_ytdlp(exit_code=1, stderr='ERROR: unable to download webpage: timed out')
    monkeypatch.setattr(xiaohongshu_module.subprocess, 'run', run_command)
    job = _job(path=tmp_path)

    with pytest.raises(VideoDownloadError):
        _run(job, job._download_video(_media(media_type='video'), cookie_path=tmp_path / 'c.txt', cache_dir=tmp_path / 'cache'))

    assert len(calls) == 1


# ---------- run ----------


def test_a_run_walks_the_list_resolves_the_note_and_saves_its_files(monkeypatch, tmp_path) -> None:
    fake_db = _FakeDatabase()
    notifications: list[dict] = []

    async def _notify(**payload) -> None:
        notifications.append(payload)

    async def _handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == '/api/sns/web/v2/user/me':
            return _api_response({'user_id': 'me'})
        if request.url.path == '/api/sns/web/v1/note/like/page':
            return _api_response({'notes': [{'note_id': NOTE_ID, 'xsec_token': 'TOKEN'}], 'cursor': 'C1', 'has_more': False})
        if request.url.path == '/api/sns/web/v1/feed':
            return _api_response({'items': [{'note_card': _image_note_card()}]})
        return httpx.Response(200, content=b'image-bytes')

    monkeypatch.setattr(xiaohongshu_module, 'database', fake_db)
    monkeypatch.setattr(xiaohongshu_module, 'enqueue_notification', _notify)
    job = _job(_handler, path=tmp_path)
    job.user_id = ''
    # The vault is the one thing a test cannot stand in for over HTTP.
    job._load_session = lambda cookie_path: job.cookies  # noqa: ARG005

    _run(job, job.update())

    assert (tmp_path / 'artist' / f'[artist]2025-08-12 [{NOTE_ID}_1].webp').read_bytes() == b'image-bytes'
    assert any(update.startswith('UPDATE xiaohongshu SET downloaded = 1') for update in fake_db.updates())
    assert notifications[0]['source'] == 'xiaohongshu'


def test_an_unconfigured_source_does_nothing_rather_than_calling_the_api(monkeypatch) -> None:
    _configure(cookiecloud=settings.CookieCloud())

    def _explode(*_args, **_kwargs):
        raise AssertionError('should not touch the database or the network')

    monkeypatch.setattr(xiaohongshu_module, 'database', _explode)
    job = Xiaohongshu.__new__(Xiaohongshu)
    job.cfg = settings.load().web.xiaohongshu

    asyncio.run(job.update())


# ---------- registration ----------


def test_the_job_is_registered_and_parked_until_configured() -> None:
    fake_config = settings.Settings()
    fake_config.web.xiaohongshu.enabled = True

    job = next(job for job in jobs_module.build_jobs(fake_config) if job.key == 'xiaohongshu')

    assert job.section == 'web.xiaohongshu'
    assert job.required_commands == ('yt-dlp',)
    assert job.factory is jobs_module.Xiaohongshu
    # Enabled in the UI but with no credentials, so it stays out of the scheduler.
    assert job.enabled is False
    assert 'cookiecloud.server_url' in job.missing_fields


def test_api_job_enum_includes_xiaohongshu() -> None:
    assert JobRequestTarget.XIAOHONGSHU.value == 'xiaohongshu'


def test_the_archive_links_a_row_to_a_note_that_actually_opens() -> None:
    source = ARCHIVE_SOURCES['xiaohongshu']

    url = _external_url(source, {'note_id': NOTE_ID, 'media_index': 1, 'xsec_token': 'TOKEN'})

    # Without the token the note is only readable by its author.
    assert url == f'https://www.xiaohongshu.com/explore/{NOTE_ID}?xsec_token=TOKEN&xsec_source=pc_user'
    assert 'xsec_token' in source.columns
