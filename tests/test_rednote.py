# ruff: noqa: ANN001, ANN002, ANN003, ANN202, ARG001, ARG002, EM101, EM102, INP001, PLR2004, S101, S105, S106, SLF001, TRY003

import asyncio
import base64
import tomllib
from pathlib import Path

import httpx
import pytest

import src.service.jobs as jobs_module
import src.web.rednote as rednote_module
from src.api.archive import ARCHIVE_SOURCES, _external_url
from src.api.schemas import JobRequestTarget
from src.core import settings
from src.web.rednote import (
    MediaUnavailableError,
    MediaUrlStaleError,
    NoteRef,
    RedNote,
    RedNoteError,
    RedNoteMedia,
    VideoDownloadError,
    build_media_filename,
    build_note_url,
    build_ytdlp_command,
    decide_login_state,
    extract_like_page,
    extract_note_media,
    infer_image_extension,
    is_note_gone,
    normalize_media_url,
    parse_api_envelope,
    user_id_from_probe,
)
from src.web.rednote_browser import (
    browser_cookies_to_netscape,
    build_launch_options,
    build_proxy_settings,
    clear_stale_profile_locks,
    cursor_of,
    decode_qr_data_url,
)

# 2025-08-12 11:34:56 in the timezone the app displays.
NOTE_TIME_MS = 1754969696000
NOTE_ID = '64f1a2b3000000001e02c0de'
OTHER_NOTE_ID = '64f1a2b3000000001e02beef'
# A one-pixel PNG, so the QR helper is exercised on real bytes.
QR_PNG = base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==')


def _configure(**updates: object) -> settings.RedNote:
    """Mutate the pinned settings snapshot that RedNote() reads in __init__."""
    cfg = settings.load().web.rednote
    for key, value in updates.items():
        setattr(cfg, key, value)
    return cfg


def _runnable_cfg(**updates: object) -> settings.RedNote:
    updates.setdefault('sleep_request_seconds', 0.0)
    updates.setdefault('proxy', 'http://home.example:3128')
    return _configure(**updates)


async def _no_requests(request: httpx.Request) -> httpx.Response:
    raise AssertionError(f'unexpected request to {request.url}')


class _FakeBrowser:
    """The NoteBrowser protocol, scripted. No Playwright, no network."""

    def __init__(
        self,
        *,
        probes: list[dict] | None = None,
        pages: list[dict] | None = None,
        notes: dict[str, dict] | None = None,
        user_agent: str = 'Mozilla/5.0 (Test) Chrome/149.0.0.0',
    ) -> None:
        self.probes = probes or [{'has_login_modal': False, 'user_id': 'me'}]
        self.pages = list(pages or [])
        self.notes = notes or {}
        self._user_agent = user_agent
        self.cookies = {'web_session': 'session-value'}
        self.started = False
        self.closed = False
        self.opened_likes: list[str] = []
        self.reloads = 0
        self.note_urls: list[str] = []

    async def start(self) -> None:
        self.started = True

    async def aclose(self) -> None:
        self.closed = True

    async def probe_login(self) -> dict:
        return self.probes[0] if len(self.probes) == 1 else self.probes.pop(0)

    async def cookie_dict(self) -> dict[str, str]:
        return dict(self.cookies)

    async def netscape_cookies(self) -> str:
        return '# Netscape HTTP Cookie File'

    async def user_agent(self) -> str:
        return self._user_agent

    async def reload_login(self) -> None:
        self.reloads += 1

    async def open_likes(self, *, user_id: str) -> None:
        self.opened_likes.append(user_id)

    async def next_like_page(self, *, timeout_seconds: float) -> dict | None:
        return self.pages.pop(0) if self.pages else None

    async def note_state(self, *, note_url: str, note_id: str) -> dict:
        self.note_urls.append(note_url)
        return self.notes.get(note_id, _image_note_card())


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
        self.state: dict[str, str] = {}
        self.calls: list[tuple[str, tuple]] = []
        self.inserted: list[tuple[str, tuple[str, ...], list[tuple[str, ...]], str | None]] = []

    async def query_db(self, query: str, params: tuple = ()) -> list[dict]:
        self.calls.append((query, params))
        normalized = ' '.join(query.split())
        if normalized.startswith('SELECT DISTINCT note_id'):
            return [{'note_id': note_id} for note_id in params if note_id in self.known_note_ids]
        if normalized.startswith('SELECT note_id, media_index'):
            return list(self.pending_rows)
        if normalized.startswith('SELECT value FROM rednote_state'):
            key = params[0]
            if key == 'backfill_complete':
                return [{'value': '1'}] if self.backfill_complete else []
            return [{'value': self.state[key]}] if key in self.state else []
        if normalized.startswith('INSERT INTO rednote_state'):
            self.state[params[0]] = params[1]
        return []

    async def insert_db_batch(self, *, table: str, columns: tuple[str, ...], rows, on_conflict: str | None = None) -> None:
        self.inserted.append((table, columns, list(rows), on_conflict))
        self.pending_rows.extend(dict(zip(columns, row, strict=True)) for row in rows)

    def advisory_lock(self, name: str):
        from contextlib import asynccontextmanager  # noqa: PLC0415

        @asynccontextmanager
        async def _lock():
            yield True

        return _lock()

    def updates(self) -> list[str]:
        return [' '.join(query.split()) for query, _ in self.calls if query.strip().upper().startswith('UPDATE')]


def _job(handler=_no_requests, browser: _FakeBrowser | None = None, **updates) -> RedNote:
    _runnable_cfg(**updates)
    job = RedNote.__new__(RedNote)
    job.cfg = settings.load().web.rednote
    job.client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
    job.user_id = 'me'
    job.user_agent = 'Mozilla/5.0 (Test) Chrome/149.0.0.0'
    job._browser_factory = lambda: browser or _FakeBrowser()
    return job


def _run(job: RedNote, coro):
    try:
        return asyncio.run(coro)
    finally:
        asyncio.run(job.client.aclose())


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


def _page_note_card(**updates) -> dict:
    """The same note as __INITIAL_STATE__ spells it: camelCase, http URLs."""
    payload = {
        'type': 'normal',
        'title': 'a caption',
        'time': NOTE_TIME_MS,
        'user': {'nickname': 'artist', 'userId': '5ff0000000000000000fffff'},
        'imageList': [
            {
                'infoList': [
                    {'imageScene': 'WB_PRV', 'url': 'http://sns-webpic.xhscdn.com/1!nd_prv_wlteh_webp_3'},
                    {'imageScene': 'WB_DFT', 'url': 'http://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3'},
                ],
                'urlDefault': 'http://sns-webpic.xhscdn.com/1!nd_dft_fallback',
                'livePhoto': False,
                'stream': {},
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


def _media(**updates) -> RedNoteMedia:
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
    return RedNoteMedia(**fields)


def _envelope(data: dict, *, code: int = 0) -> dict:
    return {'code': code, 'success': code == 0, 'msg': 'ok', 'data': data}


def _like_page(notes: list[dict], *, cursor: str = '', has_more: bool = True, status: int = 200) -> dict:
    url = f'https://edith.xiaohongshu.com/api/sns/web/v1/note/like/page?user_id=me&num=30&cursor={cursor}'
    return {'url': url, 'status': status, 'body': _envelope({'notes': notes, 'cursor': cursor, 'has_more': has_more})}


# ---------- configuration ----------


def test_a_datacenter_run_has_to_be_asked_for() -> None:
    # The previous revision defaulted to going out over the pod's own address and it
    # cost the account its sessions, so this is now a decision rather than a default.
    assert _configure(proxy='', allow_direct_connection=False).validate_runnable() == ['proxy']
    assert _configure(proxy='', allow_direct_connection=True).validate_runnable() == []
    assert _configure(proxy='http://home.example:3128', allow_direct_connection=False).validate_runnable() == []


def test_an_empty_video_path_is_not_read_as_the_working_directory() -> None:
    # Path('') is Path('.'), which would put every video in the process's cwd.
    assert settings.RedNote(video_path='').video_path is None
    assert settings.RedNote(video_path='   ').video_path is None


def test_the_request_interval_cannot_be_negative() -> None:
    with pytest.raises(ValueError, match='sleep_request_seconds'):
        settings.RedNote(sleep_request_seconds=-1)


def test_the_page_limits_must_leave_room_for_one_page() -> None:
    with pytest.raises(ValueError, match='abort_after'):
        settings.RedNote(abort_after=0)
    with pytest.raises(ValueError, match='max_pages_per_run'):
        settings.RedNote(max_pages_per_run=0)


def test_the_profile_defaults_onto_the_persistent_directory() -> None:
    # ./data is the repository's convention for state that has to survive a restart;
    # a profile anywhere else means a QR scan after every deploy.
    assert settings.RedNote().profile_path == Path('./data/rednote-profile')


def test_xhshow_is_gone_and_should_stay_gone() -> None:
    """The signing library is what this rewrite exists to remove.

    Reaching for it again would mean issuing requests from outside the browser, which
    is the shape that got the account flagged in the first place.
    """
    with (Path(__file__).resolve().parents[1] / 'pyproject.toml').open('rb') as handle:
        dependencies = tomllib.load(handle)['project']['dependencies']

    assert not [name for name in dependencies if 'xhshow' in name]


# ---------- launching the browser ----------


def test_the_launch_options_ask_for_the_real_browser_not_the_headless_shell() -> None:
    options = build_launch_options(user_data_dir=Path('/data/profile'), proxy='', headless=True)

    # A bare headless=True prefers chromium-headless-shell, the most identifiable
    # build in the stack.
    assert options['channel'] == 'chromium'
    assert options['user_data_dir'] == '/data/profile'
    assert options['locale'] == 'zh-CN'
    assert options['timezone_id'] == 'Asia/Shanghai'
    # /dev/shm is 64 MB in a container, and Chromium answers that with renderer
    # crashes that read like site failures.
    assert '--disable-dev-shm-usage' in options['args']
    assert options['ignore_default_args'] == ['--enable-automation']


def test_the_launch_options_leave_the_user_agent_alone() -> None:
    # Playwright does not regenerate Sec-CH-UA for an overridden UA, so pinning one
    # creates a UA/client-hint mismatch that is louder than the default ever was.
    assert 'user_agent' not in build_launch_options(user_data_dir=Path('/p'), proxy='', headless=True)


def test_proxy_credentials_are_split_out_of_the_url() -> None:
    # Chromium ignores userinfo in --proxy-server.
    assert build_proxy_settings('http://user:pw@home.example:3128') == {
        'server': 'http://home.example:3128',
        'username': 'user',
        'password': 'pw',
    }
    assert build_proxy_settings('home.example:3128') == {'server': 'http://home.example:3128'}
    assert build_proxy_settings('  ') is None


def test_an_authenticated_socks_proxy_is_refused_rather_than_silently_anonymous() -> None:
    with pytest.raises(ValueError, match='SOCKS'):
        build_proxy_settings('socks5://user:pw@home.example:1080')

    # Without credentials SOCKS is fine.
    assert build_proxy_settings('socks5://home.example:1080') == {'server': 'socks5://home.example:1080'}


def test_a_killed_chromium_does_not_lock_the_profile_forever(tmp_path) -> None:
    # The lock encodes hostname-pid, and in k8s both change with every pod, so a
    # leftover reads as "held by another machine" and the next run cannot start.
    (tmp_path / 'SingletonLock').symlink_to('some-host-1234')
    (tmp_path / 'SingletonCookie').write_text('x')

    cleared = clear_stale_profile_locks(tmp_path)

    assert sorted(cleared) == ['SingletonCookie', 'SingletonLock']
    assert not (tmp_path / 'SingletonLock').is_symlink()
    assert clear_stale_profile_locks(tmp_path) == []


def test_browser_cookies_become_a_jar_yt_dlp_can_read() -> None:
    text = browser_cookies_to_netscape(
        [
            {'name': 'web_session', 'value': 'v', 'domain': '.xiaohongshu.com', 'path': '/', 'secure': True, 'expires': 1893456000},
            {'name': 'session_only', 'value': 's', 'domain': 'www.xiaohongshu.com', 'path': '/', 'secure': False, 'expires': -1},
        ],
    )

    lines = text.splitlines()
    assert lines[0] == '# Netscape HTTP Cookie File'
    assert lines[3].split('\t') == ['.xiaohongshu.com', 'TRUE', '/', 'TRUE', '1893456000', 'web_session', 'v']
    # A session cookie comes back as -1; the file format spells that 0.
    assert lines[4].split('\t')[4] == '0'


def test_the_login_qr_is_decoded_straight_out_of_the_dom() -> None:
    src = 'data:image/png;base64,' + base64.b64encode(QR_PNG).decode()

    assert decode_qr_data_url(src) == QR_PNG

    with pytest.raises(ValueError, match='base64 QR'):
        decode_qr_data_url('https://example.com/qr.png')


def test_a_page_is_identified_by_the_cursor_it_was_fetched_with() -> None:
    assert cursor_of('https://edith.xiaohongshu.com/api/sns/web/v1/note/like/page?num=30&cursor=C1') == 'C1'
    assert cursor_of('https://edith.xiaohongshu.com/api/sns/web/v1/note/like/page?num=30') == ''


# ---------- login ----------


def test_the_login_modal_outranks_a_stale_cookie() -> None:
    # web_session survives the account revoking the session elsewhere, so trusting it
    # alone would walk a run confidently into a login screen.
    assert decide_login_state({'has_login_modal': True}, {'web_session': 'still-here'}) == 'logged_out'
    assert decide_login_state({'has_login_modal': False, 'user_id': 'me'}, {}) == 'logged_out'
    assert decide_login_state({'has_login_modal': False, 'user_id': 'me'}, {'web_session': 'v'}) == 'logged_in'


def test_a_page_that_has_not_hydrated_yet_is_not_called_signed_out() -> None:
    assert decide_login_state({'has_login_modal': False}, {'web_session': 'v'}) == 'unknown'


def test_the_profile_id_comes_from_the_page_or_its_own_profile_link() -> None:
    assert user_id_from_probe({'user_id': '5ff'}) == '5ff'
    assert user_id_from_probe({'profile_href': '/user/profile/5ff?tab=liked'}) == '5ff'
    assert user_id_from_probe({}) == ''


def test_a_qr_is_sent_once_per_code_and_the_run_continues_after_the_scan(monkeypatch) -> None:
    sent: list[bytes] = []
    qr_a = 'data:image/png;base64,' + base64.b64encode(QR_PNG).decode()
    qr_b = 'data:image/png;base64,' + base64.b64encode(QR_PNG + b'\x00').decode()

    async def _send_photo(*, photo, caption='', require_enabled=True) -> int:
        sent.append(photo[1])
        return 1

    async def _send_text(*, text, require_enabled=True) -> int:
        return 2

    monkeypatch.setattr(rednote_module, 'database', _FakeDatabase())
    monkeypatch.setattr(rednote_module.telegram_bot_tool, 'send_photo_now', _send_photo)
    monkeypatch.setattr(rednote_module.telegram_bot_tool, 'send_text_now', _send_text)
    monkeypatch.setattr(rednote_module, '_LOGIN_POLL_INTERVAL_SECONDS', 0)

    browser = _FakeBrowser(
        probes=[
            {'has_login_modal': True, 'qr_src': qr_a},
            # The same code again: re-sending it would be noise.
            {'has_login_modal': True, 'qr_src': qr_a},
            # RedNote minted a new one, so the src changed.
            {'has_login_modal': True, 'qr_src': qr_b},
            {'has_login_modal': False, 'user_id': 'me'},
        ],
    )
    job = _job(browser=browser)

    _run(job, job._await_login(browser))

    assert len(sent) == 2
    assert job.user_id == 'me'


def test_a_login_that_is_never_scanned_ends_the_run_with_one_deduped_failure(monkeypatch) -> None:
    async def _send_photo(*, photo, caption='', require_enabled=True) -> int:
        return 1

    monkeypatch.setattr(rednote_module, 'database', _FakeDatabase())
    monkeypatch.setattr(rednote_module.telegram_bot_tool, 'send_photo_now', _send_photo)
    monkeypatch.setattr(rednote_module, '_LOGIN_POLL_INTERVAL_SECONDS', 0)

    qr = 'data:image/png;base64,' + base64.b64encode(QR_PNG).decode()
    browser = _FakeBrowser(probes=[{'has_login_modal': True, 'qr_src': qr}])
    job = _job(browser=browser, login_wait_seconds=1)

    with pytest.raises(RedNoteError) as excinfo:
        _run(job, job._await_login(browser))

    assert excinfo.value.notification_dedupe_key == 'rednote:login'


def test_a_second_run_inside_the_cooldown_does_not_send_another_qr(monkeypatch) -> None:
    # A QR is dead within minutes, so a 04:00 cron sending one is pure noise.
    sent: list[bytes] = []

    async def _send_photo(*, photo, caption='', require_enabled=True) -> int:
        sent.append(photo[1])
        return 1

    fake_db = _FakeDatabase()
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    monkeypatch.setattr(rednote_module.telegram_bot_tool, 'send_photo_now', _send_photo)
    monkeypatch.setattr(rednote_module, '_LOGIN_POLL_INTERVAL_SECONDS', 0)

    qr = 'data:image/png;base64,' + base64.b64encode(QR_PNG).decode()
    job = _job(login_wait_seconds=1, login_prompt_cooldown_seconds=3600)

    for _ in range(2):
        browser = _FakeBrowser(probes=[{'has_login_modal': True, 'qr_src': qr}])
        with pytest.raises(RedNoteError):
            asyncio.run(job._await_login(browser))
    asyncio.run(job.client.aclose())

    assert len(sent) == 1


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


def test_a_signed_out_response_is_reported_as_a_login_failure() -> None:
    with pytest.raises(RedNoteError) as excinfo:
        parse_api_envelope(_envelope({}, code=-101))

    assert excinfo.value.notification_dedupe_key == 'rednote:login'


def test_any_other_error_code_is_an_ordinary_failure() -> None:
    assert parse_api_envelope(_envelope({'notes': []})) == {'notes': []}
    with pytest.raises(ValueError, match='-510001'):
        parse_api_envelope(_envelope({}, code=-510001))
    with pytest.raises(TypeError):
        parse_api_envelope('not an object')


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


def test_the_page_state_parses_to_exactly_what_the_api_did() -> None:
    """__INITIAL_STATE__ is camelCase where the API was snake_case.

    Same note, same rows: this is the cheap proof that moving to the browser did not
    shift ``media_index``, which is what ``_refresh_stale_media`` looks rows up by.
    """
    from_api = extract_note_media(_image_note_card(), note_id=NOTE_ID, xsec_token='T')
    from_page = extract_note_media(_page_note_card(), note_id=NOTE_ID, xsec_token='T')

    assert from_page == from_api


def test_the_page_state_url_is_upgraded_to_https() -> None:
    # __INITIAL_STATE__ hands out http:// CDN links.
    assert normalize_media_url('http://sns-webpic.xhscdn.com/x') == 'https://sns-webpic.xhscdn.com/x'
    assert extract_note_media(_page_note_card(), note_id=NOTE_ID, xsec_token='T')[0].media_url.startswith('https://')


def test_an_image_with_only_a_default_url_still_yields_a_file() -> None:
    card = _page_note_card(imageList=[{'urlDefault': 'http://cdn/only-default'}])

    assert [item.media_url for item in extract_note_media(card, note_id=NOTE_ID, xsec_token='T')] == ['https://cdn/only-default']


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

    # Indices are handed out in file order, the clip counting as its own file.
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
    card = _video_note_card(video={'media': {'stream': {'h265': [{'masterUrl': 'https://cdn/h265.mp4'}]}}})

    assert extract_note_media(card, note_id=NOTE_ID, xsec_token='T')[0].media_url == 'https://cdn/h265.mp4'


def test_a_video_note_yields_a_row_even_with_no_readable_stream() -> None:
    # yt-dlp downloads it from the note page, so an unreadable stream block is not a
    # reason to drop the note on the floor.
    media = extract_note_media(_video_note_card(video={}), note_id=NOTE_ID, xsec_token='T')

    assert [(item.media_index, item.media_type, item.media_url) for item in media] == [(1, 'video', '')]


def test_a_note_without_a_time_still_produces_rows() -> None:
    assert extract_note_media(_image_note_card(time=None), note_id=NOTE_ID, xsec_token='T')[0].published_at == ''


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
    assert build_media_filename(_media(media_type='video', media_url='')).endswith('.mp4')
    assert build_media_filename(_media(media_type='live', media_index=3, media_url='https://cdn/x')).endswith('.mp4')


def test_a_note_link_carries_the_token_that_makes_it_readable() -> None:
    assert build_note_url(NOTE_ID, 'TOKEN') == f'https://www.xiaohongshu.com/explore/{NOTE_ID}?xsec_token=TOKEN&xsec_source=pc_user'
    assert build_note_url(NOTE_ID) == f'https://www.xiaohongshu.com/explore/{NOTE_ID}'


# ---------- yt-dlp invocation ----------


def test_the_video_command_leaves_through_the_same_proxy_as_the_browser() -> None:
    # yt-dlp re-fetches the note page carrying the account's cookies, so skipping the
    # proxy here would put the session straight back on the pod's own address.
    command = build_ytdlp_command(
        note_url=build_note_url(NOTE_ID, 'TOKEN'),
        cookie_path=Path('/run/cookies/c.txt'),
        output_template=Path('/cache') / f'{NOTE_ID}.%(ext)s',
        proxy='http://home.example:3128',
        user_agent='Mozilla/5.0 (Test) Chrome/149.0.0.0',
    )

    assert command[command.index('--proxy') + 1] == 'http://home.example:3128'
    assert command[command.index('--cookies') + 1] == '/run/cookies/c.txt'
    assert command[command.index('-o') + 1] == f'/cache/{NOTE_ID}.%(ext)s'
    # Read off the live browser rather than pinned, so the two provably agree.
    assert command[command.index('--add-header') + 1] == 'User-Agent:Mozilla/5.0 (Test) Chrome/149.0.0.0'
    assert command[-1].endswith('?xsec_token=TOKEN&xsec_source=pc_user')


def test_the_video_command_omits_what_it_was_not_given() -> None:
    command = build_ytdlp_command(note_url='https://x', cookie_path=Path('/c'), output_template=Path('/o.%(ext)s'))

    assert '--proxy' not in command
    assert '--add-header' not in command


def test_the_installed_yt_dlp_still_claims_rednote_note_pages() -> None:
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


# ---------- walking the likes list ----------


def _resolved_note_ids(fake_db: _FakeDatabase) -> list[str]:
    return [row[0] for _table, _columns, rows, _conflict in fake_db.inserted for row in rows]


def test_the_crawl_stops_once_it_has_caught_up(monkeypatch) -> None:
    fake_db = _FakeDatabase(known_note_ids=('old-1', 'old-2'))
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    browser = _FakeBrowser(
        pages=[
            _like_page([{'note_id': 'new-1', 'xsec_token': 'T'}], cursor=''),
            _like_page([{'note_id': 'old-1', 'xsec_token': 'T'}], cursor='C1'),
            _like_page([{'note_id': 'old-2', 'xsec_token': 'T'}], cursor='C2'),
            # Reaching this page would mean the stop rule never fired.
            _like_page([{'note_id': 'new-2', 'xsec_token': 'T'}], cursor='C3'),
        ],
    )
    job = _job(browser=browser, abort_after=2)

    assert _run(job, job._crawl(browser)) == 1
    assert _resolved_note_ids(fake_db) == ['new-1']


def test_a_page_with_anything_new_resets_the_stop_counter(monkeypatch) -> None:
    fake_db = _FakeDatabase(known_note_ids=('old-1', 'old-2', 'old-3'))
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    browser = _FakeBrowser(
        pages=[
            _like_page([{'note_id': 'old-1', 'xsec_token': 'T'}], cursor=''),
            _like_page([{'note_id': 'new-1', 'xsec_token': 'T'}], cursor='C1'),
            _like_page([{'note_id': 'old-2', 'xsec_token': 'T'}], cursor='C2'),
            _like_page([{'note_id': 'old-3', 'xsec_token': 'T'}], cursor='C3', has_more=False),
        ],
    )
    job = _job(browser=browser, abort_after=2)

    _run(job, job._crawl(browser))

    assert _resolved_note_ids(fake_db) == ['new-1']


def test_the_page_the_site_fetched_twice_is_only_counted_once(monkeypatch) -> None:
    # The site's own client sometimes fires the same request twice per scroll. Left
    # counted, the repeat would satisfy the stop rule a page early and the run would
    # never reach what is below it.
    fake_db = _FakeDatabase(known_note_ids=('old-1', 'old-2'))
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    browser = _FakeBrowser(
        pages=[
            _like_page([{'note_id': 'old-1', 'xsec_token': 'T'}], cursor='C1'),
            _like_page([{'note_id': 'old-1', 'xsec_token': 'T'}], cursor='C1'),
            _like_page([{'note_id': 'old-2', 'xsec_token': 'T'}], cursor='C2'),
            _like_page([{'note_id': 'new-1', 'xsec_token': 'T'}], cursor='C3', has_more=False),
        ],
    )
    job = _job(browser=browser, abort_after=3)

    _run(job, job._crawl(browser))

    assert _resolved_note_ids(fake_db) == ['new-1']


def test_the_first_run_walks_past_notes_it_has_already_stored(monkeypatch) -> None:
    # A backfill that died part-way leaves an archived prefix. Stopping on it would
    # put everything below wherever that run got to out of reach for good.
    fake_db = _FakeDatabase(known_note_ids=('old-1', 'old-2', 'old-3'), backfill_complete=False)
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    browser = _FakeBrowser(
        pages=[
            _like_page([{'note_id': 'old-1', 'xsec_token': 'T'}], cursor=''),
            _like_page([{'note_id': 'old-2', 'xsec_token': 'T'}], cursor='C1'),
            _like_page([{'note_id': 'old-3', 'xsec_token': 'T'}], cursor='C2'),
            _like_page([{'note_id': 'new-1', 'xsec_token': 'T'}], cursor='C3', has_more=False),
        ],
    )
    job = _job(browser=browser, abort_after=2)

    _run(job, job._crawl(browser))

    assert _resolved_note_ids(fake_db) == ['new-1']
    assert any('INSERT OR IGNORE INTO rednote_state' in query for query, _ in fake_db.calls)


def test_a_walk_that_stopped_early_does_not_claim_the_backfill_finished(monkeypatch) -> None:
    fake_db = _FakeDatabase(known_note_ids=('old-1',), backfill_complete=False)
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    # Scrolling stopped producing, which is not the same as reaching the end.
    browser = _FakeBrowser(pages=[_like_page([{'note_id': 'old-1', 'xsec_token': 'T'}], cursor='')])
    job = _job(browser=browser, abort_after=2)

    _run(job, job._crawl(browser))

    assert not any('INSERT OR IGNORE INTO rednote_state' in query for query, _ in fake_db.calls)


def test_the_page_cap_leaves_the_backfill_open_for_the_next_run(monkeypatch) -> None:
    fake_db = _FakeDatabase(backfill_complete=False)
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    browser = _FakeBrowser(pages=[_like_page([{'note_id': f'n-{index}', 'xsec_token': 'T'}], cursor=f'C{index}') for index in range(5)])
    job = _job(browser=browser, abort_after=2, max_pages_per_run=2)

    _run(job, job._crawl(browser))

    assert _resolved_note_ids(fake_db) == ['n-0', 'n-1']
    assert not any('INSERT OR IGNORE INTO rednote_state' in query for query, _ in fake_db.calls)


def test_the_captcha_wall_ends_the_run_instead_of_being_scrolled_past(monkeypatch) -> None:
    monkeypatch.setattr(rednote_module, 'database', _FakeDatabase())
    browser = _FakeBrowser(pages=[_like_page([], status=461)])
    job = _job(browser=browser)

    with pytest.raises(RedNoteError) as excinfo:
        _run(job, job._crawl(browser))

    assert excinfo.value.notification_dedupe_key == 'rednote:risk'


def test_resolving_a_note_writes_one_row_per_file(monkeypatch) -> None:
    fake_db = _FakeDatabase()
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    browser = _FakeBrowser(notes={NOTE_ID: _page_note_card()})
    job = _job(browser=browser)

    assert _run(job, job._resolve_notes([NoteRef(note_id=NOTE_ID, xsec_token='TOKEN')], browser)) == 1

    table, _columns, rows, on_conflict = fake_db.inserted[0]
    assert table == 'rednote'
    assert rows[0][:3] == (NOTE_ID, '1', 'image')
    # The note is opened with its token, or the page only loads for its author.
    assert browser.note_urls == [f'https://www.xiaohongshu.com/explore/{NOTE_ID}?xsec_token=TOKEN&xsec_source=pc_user']
    assert 'DO UPDATE SET media_url = excluded.media_url' in (on_conflict or '')
    assert 'downloaded' not in (on_conflict or '')


def test_a_note_that_cannot_be_read_does_not_end_the_run(monkeypatch) -> None:
    monkeypatch.setattr(rednote_module, 'database', _FakeDatabase())

    class _PartlyBrokenBrowser(_FakeBrowser):
        async def note_state(self, *, note_url, note_id):
            if note_id == NOTE_ID:
                raise RuntimeError('navigation failed')
            return _page_note_card()

    browser = _PartlyBrokenBrowser()
    job = _job(browser=browser)
    notes = [NoteRef(note_id=NOTE_ID, xsec_token='T'), NoteRef(note_id=OTHER_NOTE_ID, xsec_token='T')]

    assert _run(job, job._resolve_notes(notes, browser)) == 1


# ---------- where files land ----------


def test_an_unset_video_path_keeps_everything_in_one_place() -> None:
    job = _job(path=Path('/data/rednote'), video_path=None)

    assert job._destination_root('video') == Path('/data/rednote')
    assert job._destination_root('image') == Path('/data/rednote')
    asyncio.run(job.client.aclose())


def test_videos_and_live_clips_go_to_the_video_path_while_images_stay() -> None:
    job = _job(path=Path('/data/rednote'), video_path=Path('/media/rednote-videos'))

    assert job._destination_root('video') == Path('/media/rednote-videos')
    # A live photo's clip is an mp4 like any other.
    assert job._destination_root('live') == Path('/media/rednote-videos')
    assert job._destination_root('image') == Path('/data/rednote')
    assert job._build_output_path(_media()) == Path('/data/rednote/artist') / f'[artist]2025-08-12 [{NOTE_ID}_1].webp'
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


def test_an_expired_url_is_recorded_rather_than_guessed_at(tmp_path) -> None:
    # The browser is closed by now, and a 404 on a CDN URL says as much about
    # rotation as about deletion, so the row waits for a fresh look at the note.
    async def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(404)

    job = _job(_handler, path=tmp_path)

    with pytest.raises(MediaUrlStaleError, match='stale-url'):
        _run(job, job._download_file(_media()))


def test_downloading_marks_each_row_and_keeps_going_past_a_dead_url(monkeypatch, tmp_path) -> None:
    fake_db = _FakeDatabase()
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    dead_url = 'https://sns-webpic.xhscdn.com/dead!nd_dft_wlteh_webp_3'

    async def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(403) if str(request.url) == dead_url else httpx.Response(200, content=b'bytes')

    job = _job(_handler, path=tmp_path)
    pending = [_media(media_index=1, media_url=dead_url), _media(media_index=2)]

    downloaded = _run(job, job._download_pending(pending, cookie_path=tmp_path / 'c.txt', cache_dir=tmp_path / 'cache'))

    assert downloaded == 1
    updates = fake_db.updates()
    assert any(update.startswith('UPDATE rednote SET failed_count') for update in updates)
    assert any(update.startswith('UPDATE rednote SET downloaded = 1') for update in updates)


def test_the_cdn_is_not_paced_like_the_site(monkeypatch, tmp_path) -> None:
    # The request interval exists for RedNote's risk control; charging it per
    # image would add hours of pure waiting to a first backfill.
    sleeps: list[float] = []

    async def _fake_sleep(delay: float) -> None:
        sleeps.append(delay)

    async def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=b'bytes')

    monkeypatch.setattr(rednote_module, 'database', _FakeDatabase())
    monkeypatch.setattr(rednote_module.asyncio, 'sleep', _fake_sleep)
    job = _job(_handler, path=tmp_path, sleep_request_seconds=3.0)
    pending = [_media(media_index=1), _media(media_index=2)]

    assert _run(job, job._download_pending(pending, cookie_path=tmp_path / 'c.txt', cache_dir=tmp_path / 'cache')) == 2
    assert sleeps == []


def test_an_expired_url_is_refreshed_on_the_next_run_with_the_browser_open(monkeypatch, tmp_path) -> None:
    fresh_url = 'https://sns-webpic.xhscdn.com/fresh!nd_dft_wlteh_webp_3'
    row = {
        'note_id': NOTE_ID,
        'media_index': '1',
        'media_type': 'image',
        'media_url': 'https://sns-webpic.xhscdn.com/expired!nd_dft_wlteh_webp_3',
        'title': 'a caption',
        'author': 'artist',
        'author_id': 'a',
        'note_type': 'normal',
        'published_at': '2025-08-12 11:34:56',
        'xsec_token': 'TOKEN',
        'last_error': 'stale-url http 403',
    }
    fake_db = _FakeDatabase(pending_rows=(row,))
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    card = _page_note_card(imageList=[{'infoList': [{'imageScene': 'WB_DFT', 'url': fresh_url}]}])
    browser = _FakeBrowser(notes={NOTE_ID: card})
    job = _job(browser=browser)

    assert _run(job, job._refresh_stale_media(browser)) == 1
    assert any('UPDATE rednote SET media_url' in update for update in fake_db.updates())


def test_a_file_the_note_no_longer_offers_is_retired(monkeypatch) -> None:
    row = {
        'note_id': NOTE_ID,
        'media_index': '4',
        'media_type': 'image',
        'media_url': 'https://cdn/gone',
        'title': '',
        'author': 'artist',
        'author_id': '',
        'note_type': 'normal',
        'published_at': '',
        'xsec_token': 'T',
        'last_error': 'stale-url http 404',
    }
    fake_db = _FakeDatabase(pending_rows=(row,))
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    # The note now has one image, so index 4 is gone for good.
    browser = _FakeBrowser(notes={NOTE_ID: _page_note_card()})
    job = _job(browser=browser)

    _run(job, job._refresh_stale_media(browser))

    assert any(update.startswith('UPDATE rednote SET unavailable = 1') for update in fake_db.updates())


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
    monkeypatch.setattr(rednote_module.subprocess, 'run', run_command)
    job = _job(path=tmp_path / 'images', video_path=tmp_path / 'videos')

    dst_path = _run(
        job,
        job._download_video(_media(media_type='video', media_url=''), cookie_path=tmp_path / 'c.txt', cache_dir=tmp_path / 'cache'),
    )

    assert dst_path == tmp_path / 'videos' / 'artist' / f'[artist]2025-08-12 [{NOTE_ID}_1].mp4'
    assert dst_path.read_bytes() == b'video-bytes'
    assert list((tmp_path / 'cache').iterdir()) == []
    assert calls[0][-1].startswith(f'https://www.xiaohongshu.com/explore/{NOTE_ID}?xsec_token=')


def test_a_video_yt_dlp_saved_as_something_else_keeps_that_extension(monkeypatch, tmp_path) -> None:
    run_command, _ = _fake_ytdlp(suffix='.mkv')
    monkeypatch.setattr(rednote_module.subprocess, 'run', run_command)
    job = _job(path=tmp_path)

    dst_path = _run(job, job._download_video(_media(media_type='video'), cookie_path=tmp_path / 'c.txt', cache_dir=tmp_path / 'cache'))

    assert dst_path.suffix == '.mkv'


def test_a_deleted_video_note_is_not_retried(monkeypatch, tmp_path) -> None:
    run_command, calls = _fake_ytdlp(exit_code=1, stderr='ERROR: [XiaoHongShu] 当前笔记暂时无法浏览')
    monkeypatch.setattr(rednote_module.subprocess, 'run', run_command)
    job = _job(path=tmp_path)

    with pytest.raises(MediaUnavailableError):
        _run(job, job._download_video(_media(media_type='video'), cookie_path=tmp_path / 'c.txt', cache_dir=tmp_path / 'cache'))

    assert len(calls) == 1


def test_a_video_that_merely_failed_is_reported_as_retryable(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(rednote_module, '_YTDLP_MAX_ATTEMPTS', 1)
    run_command, calls = _fake_ytdlp(exit_code=1, stderr='ERROR: unable to download webpage: timed out')
    monkeypatch.setattr(rednote_module.subprocess, 'run', run_command)
    job = _job(path=tmp_path)

    with pytest.raises(VideoDownloadError):
        _run(job, job._download_video(_media(media_type='video'), cookie_path=tmp_path / 'c.txt', cache_dir=tmp_path / 'cache'))

    assert len(calls) == 1


# ---------- run ----------


def test_a_run_signs_in_walks_the_list_and_saves_the_files(monkeypatch, tmp_path) -> None:
    fake_db = _FakeDatabase()
    notifications: list[dict] = []

    async def _notify(**payload) -> None:
        notifications.append(payload)

    async def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=b'image-bytes')

    monkeypatch.setattr(rednote_module, 'database', fake_db)
    monkeypatch.setattr(rednote_module, 'enqueue_notification', _notify)

    browser = _FakeBrowser(
        pages=[_like_page([{'note_id': NOTE_ID, 'xsec_token': 'TOKEN'}], cursor='', has_more=False)],
        notes={NOTE_ID: _page_note_card()},
    )
    job = _job(_handler, browser=browser, path=tmp_path, profile_path=tmp_path / 'profile')

    _run(job, job.update())

    assert browser.started
    assert browser.closed
    assert (tmp_path / 'artist' / f'[artist]2025-08-12 [{NOTE_ID}_1].webp').read_bytes() == b'image-bytes'
    assert any(update.startswith('UPDATE rednote SET downloaded = 1') for update in fake_db.updates())
    assert notifications[0]['source'] == 'rednote'
    # Resolved once and remembered, so later runs skip the lookup.
    assert fake_db.state.get('user_id') == 'me'


def test_the_browser_is_closed_even_when_the_walk_fails(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(rednote_module, 'database', _FakeDatabase())

    class _ExplodingBrowser(_FakeBrowser):
        async def open_likes(self, *, user_id):
            raise RuntimeError('navigation failed')

    browser = _ExplodingBrowser()
    job = _job(browser=browser, path=tmp_path, profile_path=tmp_path / 'profile')

    with pytest.raises(RuntimeError, match='navigation failed'):
        _run(job, job.update())

    # Chromium costs hundreds of megabytes; leaking one per failed run would take the
    # whole worker down with it.
    assert browser.closed


def test_an_unconfigured_source_does_nothing_rather_than_opening_a_browser(monkeypatch) -> None:
    _configure(proxy='', allow_direct_connection=False)

    def _explode(*_args, **_kwargs):
        raise AssertionError('should not touch the database or the browser')

    monkeypatch.setattr(rednote_module, 'database', _explode)
    job = RedNote.__new__(RedNote)
    job.cfg = settings.load().web.rednote

    asyncio.run(job.update())


# ---------- registration ----------


def test_the_job_is_registered_and_parked_until_configured() -> None:
    fake_config = settings.Settings()
    fake_config.web.rednote.enabled = True

    job = next(job for job in jobs_module.build_jobs(fake_config) if job.key == 'rednote')

    assert job.section == 'web.rednote'
    assert job.required_commands == ('yt-dlp',)
    assert job.factory is jobs_module.RedNote
    assert job.enabled is False
    assert 'proxy' in job.missing_fields


def test_api_job_enum_includes_rednote() -> None:
    assert JobRequestTarget.REDNOTE.value == 'rednote'


def test_the_archive_links_a_row_to_a_note_that_actually_opens() -> None:
    source = ARCHIVE_SOURCES['rednote']

    url = _external_url(source, {'note_id': NOTE_ID, 'media_index': 1, 'xsec_token': 'TOKEN'})

    # Without the token the note is only readable by its author.
    assert url == f'https://www.xiaohongshu.com/explore/{NOTE_ID}?xsec_token=TOKEN&xsec_source=pc_user'
    assert 'xsec_token' in source.columns
