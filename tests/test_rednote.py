# ruff: noqa: ANN001, ANN002, ANN003, ANN202, ARG001, ARG002, EM101, EM102, INP001, PLR2004, S101, S105, S106, SLF001, TRY003

import asyncio
import base64
import json
import logging
import time
import tomllib
from pathlib import Path

import httpx
import pytest

import src.service.jobs as jobs_module
import src.web.rednote as rednote_module
import src.web.rednote_browser as rednote_browser_module
from src.api.archive import ARCHIVE_SOURCES, _external_url
from src.api.schemas import JobRequestTarget
from src.core import settings
from src.core.settings import SECTION_MODELS
from src.web.rednote import (
    MediaUrlStaleError,
    NoteRef,
    RedNote,
    RedNoteError,
    RedNoteMedia,
    build_media_filename,
    build_note_url,
    decide_login_state,
    extract_like_page,
    extract_note_media,
    infer_image_extension,
    normalize_media_url,
    note_source_long_edge,
    parse_api_envelope,
    pick_video_url,
    user_id_from_probe,
)
from src.web.rednote_browser import (
    PlaywrightNoteBrowser,
    ProxyProbe,
    build_launch_options,
    build_proxy_settings,
    clear_stale_profile_locks,
    cursor_of,
    decode_qr_data_url,
    describe_shape,
    probe_proxy,
    redact_probe,
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
        initial_notes: list[dict] | None = None,
        notes: dict[str, dict] | None = None,
        user_agent: str = 'Mozilla/5.0 (Test) Chrome/149.0.0.0',
    ) -> None:
        self.probes = probes or [{'logged_in': True, 'user_id': 'me'}]
        self.pages = list(pages or [])
        self.initial_notes = list(initial_notes or [])
        self.notes = notes or {}
        self._user_agent = user_agent
        self.cookies = {'web_session': 'session-value'}
        self.started = False
        self.closed = False
        self.opened_likes: list[str] = []
        self.reloads = 0
        self.qr_refreshes = 0
        self.note_urls: list[str] = []

    async def start(self) -> None:
        self.started = True

    async def aclose(self) -> None:
        self.closed = True

    async def probe_login(self) -> dict:
        return self.probes[0] if len(self.probes) == 1 else self.probes.pop(0)

    async def cookie_dict(self) -> dict[str, str]:
        return dict(self.cookies)

    async def user_agent(self) -> str:
        return self._user_agent

    async def reload_login(self) -> None:
        self.reloads += 1

    async def refresh_qr(self) -> bool:
        self.qr_refreshes += 1
        return True

    async def open_likes(self, *, user_id: str) -> None:
        self.opened_likes.append(user_id)

    async def initial_like_notes(self) -> list[dict]:
        return list(self.initial_notes)

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
        self.missing: dict[str, tuple[int, str]] = {}
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
        if normalized.startswith('INSERT INTO rednote_missing'):
            note_id, run_id = params[0], params[1]
            runs, last_run = self.missing.get(note_id, (0, ''))
            if last_run != run_id:
                self.missing[note_id] = (runs + 1, run_id)
        if normalized.startswith('DELETE FROM rednote_missing'):
            self.missing.pop(params[0], None)
        if normalized.startswith('SELECT note_id FROM rednote_missing'):
            threshold = int(params[0])
            wanted = set(params[1:])
            return [{'note_id': n} for n, (runs, _r) in self.missing.items() if runs >= threshold and n in wanted]
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
    job._logged_card_shapes = set()
    job._warned_downscaled = False
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
        # The live shape: opaque, versioned bucket names, each variant stating its own
        # resolution. A signed-in note is offered every size up to the 4K source; a
        # signed-out one is capped at 1080p and never sees the 1440p/2160p variants.
        'video': {
            'media': {
                'stream': {
                    'EF4': [
                        {
                            'videoCodec': 'EF4',
                            'width': 720,
                            'height': 1280,
                            'videoBitrate': 3_000_000,
                            'masterUrl': 'https://sns-video.xhscdn.com/stream/259_720p.mp4',
                        },
                        {
                            'videoCodec': 'EF4',
                            'width': 1080,
                            'height': 1920,
                            'videoBitrate': 5_000_000,
                            'masterUrl': 'https://sns-video.xhscdn.com/stream/261_1080p.mp4',
                        },
                    ],
                    'EF6': [],
                    'EF5': [
                        {
                            'videoCodec': 'EF5',
                            'width': 1080,
                            'height': 1920,
                            'videoBitrate': 1_700_000,
                            'masterUrl': 'https://sns-video.xhscdn.com/stream/301_1080p.mp4',
                        },
                        {
                            'videoCodec': 'EF5',
                            'width': 2160,
                            'height': 3840,
                            'videoBitrate': 9_000_000,
                            'masterUrl': 'https://sns-video.xhscdn.com/stream/109_2160p.mp4',
                        },
                    ],
                },
            },
            # The upload's own resolution, stated as a JSON string as the page renders it.
            'mediaV2': '{"video": {"width": 2160, "height": 3840}}',
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


def test_the_launch_options_do_not_pin_a_user_agent() -> None:
    """The UA is corrected after launch instead, because it cannot be corrected here.

    The browser's own version is only readable once it is running, and a guessed one
    would be the mismatch that overriding a UA is usually blamed for. See
    `_present_mac_user_agent`, which reads the real string and swaps two tokens.
    """
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


def test_a_run_explains_its_payloads_without_repeating_their_contents() -> None:
    """The unknown here is the shape; the content is the user's own liked notes."""
    envelope = {
        'code': 0,
        'success': True,
        'data': {
            'has_more': True,
            'cursor': '65a1b2c3000000000e01f00d',
            'notes': [
                {'note_id': '64f1a2b3000000001e02c0de', 'xsec_token': 'AB-secret-token', 'display_title': '我的私人笔记'},
                {'note_id': 'second', 'xsec_token': 'also-secret', 'display_title': '另一条'},
            ],
        },
    }

    shape = describe_shape(envelope)
    rendered = json.dumps(shape, ensure_ascii=False)

    assert shape['data']['notes'][0] == 'list[2]'
    assert shape['data']['notes'][1] == {'note_id': 'str[24]', 'xsec_token': 'str[15]', 'display_title': 'str[6]'}
    assert shape['code'] == 'int'
    assert shape['success'] == 'bool'
    for secret in ('64f1a2b3000000001e02c0de', 'AB-secret-token', '我的私人笔记', '65a1b2c3000000000e01f00d'):
        assert secret not in rendered


def test_the_logged_login_probe_keeps_the_qr_and_the_account_id_out_of_it() -> None:
    # The QR is a live credential for as long as it lasts, and the id is the user's.
    probe = {
        'logged_in': False,
        'qr_stage': 'verify',
        'qr_src': 'data:image/png;base64,' + 'A' * 7000,
        'user_id': '6a7d8554000000001400c801',
        'modal_html': '<div class="r-captcha-modal">...</div>',
    }

    redacted = redact_probe(probe)
    rendered = json.dumps(redacted, ensure_ascii=False)

    assert redacted['qr_stage'] == 'verify'
    assert redacted['logged_in'] is False
    assert redacted['qr_src'] == 'str[7022]'
    assert 'modal_html' not in redacted
    assert 'AAAA' not in rendered
    assert '6a7d8554000000001400c801' not in rendered


def _probe(handler, proxy: str = 'http://user:pw@home.example:3128'):
    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        return probe_proxy(proxy, client=client)


def test_the_proxy_probe_names_each_way_an_egress_can_be_wrong() -> None:
    def refused(request: httpx.Request) -> httpx.Response:
        raise httpx.ProxyError('refused', request=request)

    def walled(request: httpx.Request) -> httpx.Response:
        # The captcha wall, which is the answer this button exists to catch early.
        return httpx.Response(200, text='198.51.100.9') if 'checkip' in str(request.url) else httpx.Response(461)

    def reachable(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text='203.0.113.7' if 'checkip' in str(request.url) else '<html></html>')

    refused_result = _probe(refused)
    assert (refused_result.ok, refused_result.code) == (False, 'proxy_error')

    walled_result = _probe(walled)
    assert (walled_result.ok, walled_result.code, walled_result.exit_ip) == (False, 'risk_control', '198.51.100.9')

    ok_result = _probe(reachable)
    assert (ok_result.ok, ok_result.code, ok_result.exit_ip, ok_result.direct) == (True, 'ok', '203.0.113.7', False)


def test_a_dead_proxy_is_told_apart_from_a_site_that_will_not_answer_it() -> None:
    """The two failures want opposite things done, so the button must not blur them.

    Both surface as a connection error on the same call; what separates them is
    whether anything at all came back through the proxy beforehand.
    """

    def nothing_gets_through(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError('no route to proxy', request=request)

    def site_only_is_null_routed(request: httpx.Request) -> httpx.Response:
        if 'checkip' in str(request.url):
            return httpx.Response(200, text='198.51.100.9')
        raise httpx.ConnectTimeout('timed out', request=request)

    dead_proxy = _probe(nothing_gets_through)
    assert (dead_proxy.ok, dead_proxy.code) == (False, 'proxy_error')
    assert 'proxy' in dead_proxy.message

    walled_exit = _probe(site_only_is_null_routed)
    assert (walled_exit.ok, walled_exit.code, walled_exit.exit_ip) == (False, 'unreachable', '198.51.100.9')


def test_the_proxy_probe_reports_a_bad_proxy_string_without_dialing_it() -> None:
    # Chromium would connect as nobody rather than fail, so this has to be caught here.
    dialled: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        dialled.append(str(request.url))
        return httpx.Response(200)

    result = _probe(handler, proxy='socks5://user:pw@home.example:1080')

    assert (result.ok, result.code, dialled) == (False, 'invalid', [])
    assert 'SOCKS' in result.message


def test_an_empty_proxy_probes_the_direct_egress_rather_than_refusing() -> None:
    # `allow_direct_connection` is a supported setup, so the button has to be able to
    # answer for it -- and the exit address is exactly what decides whether it is sane.
    def reachable(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text='203.0.113.7' if 'checkip' in str(request.url) else '')

    result = _probe(reachable, proxy='   ')

    assert (result.ok, result.direct, result.exit_ip) == (True, True, '203.0.113.7')


def test_an_unreachable_exit_ip_service_does_not_fail_the_probe() -> None:
    def site_only(request: httpx.Request) -> httpx.Response:
        if 'checkip' in str(request.url):
            raise httpx.ConnectError('no route', request=request)
        return httpx.Response(200)

    result = _probe(site_only)

    assert (result.ok, result.exit_ip) == (True, '')


def test_the_proxy_test_button_probes_the_draft_as_typed(monkeypatch) -> None:
    """Proxies reach the UI in plaintext now, so the draft is probed verbatim --
    no resolution against the stored section."""
    from src.api import service as service_module  # noqa: PLC0415

    section = SECTION_MODELS['web.rednote'](proxy='http://stored.example:3128')
    probed: list[str] = []

    def fake_probe(proxy: str):
        probed.append(proxy)
        return ProxyProbe(ok=True, code='ok', message='', exit_ip='203.0.113.7')

    monkeypatch.setattr(service_module, 'probe_rednote_proxy', fake_probe)
    service = service_module.FavApiService(
        dsn='postgresql://db.local/fav',
        token='t' * 32,
        hanime1_video_fetcher=lambda _dsn: [],
        job_provider=list,
        settings_section_getter=lambda _section: section,
    )

    result = service.test_rednote_proxy({'proxy': 'http://user:pw@home.example:3128'})

    assert probed == ['http://user:pw@home.example:3128']
    assert result == {'ok': True, 'code': 'ok', 'message': '', 'exit_ip': '203.0.113.7', 'direct': False}


def test_a_killed_chromium_does_not_lock_the_profile_forever(tmp_path) -> None:
    # The lock encodes hostname-pid, and in k8s both change with every pod, so a
    # leftover reads as "held by another machine" and the next run cannot start.
    (tmp_path / 'SingletonLock').symlink_to('some-host-1234')
    (tmp_path / 'SingletonCookie').write_text('x')

    cleared = clear_stale_profile_locks(tmp_path)

    assert sorted(cleared) == ['SingletonCookie', 'SingletonLock']
    assert not (tmp_path / 'SingletonLock').is_symlink()
    assert clear_stale_profile_locks(tmp_path) == []


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
    assert decide_login_state({'logged_in': True}, {'web_session': 'v'}) == 'logged_in'
    # The page's own answer outranks even a missing cookie, which is only ever a proxy for it.
    assert decide_login_state({'logged_in': True}, {}) == 'logged_in'
    assert decide_login_state({'logged_in': False}, {}) == 'logged_out'


def test_a_page_that_has_not_hydrated_yet_is_not_called_signed_out() -> None:
    assert decide_login_state({}, {'web_session': 'v'}) == 'unknown'


def test_the_profile_id_is_never_taken_from_a_signed_out_page() -> None:
    """A signed-out visitor is issued a guest id of exactly the same shape.

    Crawling it would find no likes and finish clean, so the wrong answer here is
    indistinguishable from the right one at every later step.
    """
    assert user_id_from_probe({'logged_in': True, 'user_id': '5ff'}) == '5ff'
    assert user_id_from_probe({'logged_in': True, 'profile_href': '/user/profile/5ff?tab=liked'}) == '5ff'
    # The guest identity, as the live signed-out page actually reports it.
    assert user_id_from_probe({'logged_in': False, 'guest': True, 'user_id': '6a7d8554000000001400c801'}) == ''
    assert user_id_from_probe({}) == ''


class _FakePage:
    """Just enough page to drive probe_login: a script of successive DOM samples."""

    url = 'https://www.xiaohongshu.com/explore'

    def __init__(self, samples: list[dict]) -> None:
        self.samples = samples
        self.calls = 0

    async def evaluate(self, script: str) -> dict:
        self.calls += 1
        return self.samples[min(self.calls, len(self.samples)) - 1]


def _probe_login(samples: list[dict], monkeypatch) -> tuple[dict, _FakePage]:
    monkeypatch.setattr(rednote_browser_module, '_LOGIN_RENDER_POLL_SECONDS', 0)
    browser = PlaywrightNoteBrowser.__new__(PlaywrightNoteBrowser)
    page = _FakePage(samples)
    browser._page = page
    return asyncio.run(browser.probe_login()), page


def test_the_login_probe_waits_for_the_page_to_mount_before_believing_it(monkeypatch) -> None:
    """This is the bug that took the first cluster run down.

    `domcontentloaded` resolves more than a second before this SPA renders anything,
    so the first sample shows no modal and no session. Read as "signed out, no QR",
    that used to trigger an immediate re-navigation, which both threw away the modal
    about to appear and aborted the in-flight load -- net::ERR_ABORTED, run over.
    """
    qr = 'data:image/png;base64,' + base64.b64encode(QR_PNG).decode()
    samples = [
        {},  # hydrating: __INITIAL_STATE__ not wired up yet
        {'has_login_modal': True, 'qr_src': ''},  # modal mounting, QR not drawn
        {'has_login_modal': True, 'qr_src': qr},
    ]

    probe, page = _probe_login(samples, monkeypatch)

    assert probe['qr_src'] == qr
    assert page.calls == 3


def test_the_login_probe_stops_waiting_once_the_page_says_it_is_signed_in(monkeypatch) -> None:
    probe, page = _probe_login([{'logged_in': True, 'user_id': '5ff'}], monkeypatch)

    assert (probe['logged_in'], page.calls) == (True, 1)


def test_the_login_probe_gives_up_rather_than_waiting_out_the_whole_run(monkeypatch) -> None:
    # A page that never offers either answer still has to return, so the caller's own
    # bounded wait -- not this one -- is what decides how long a signed-out run takes.
    monkeypatch.setattr(rednote_browser_module, '_LOGIN_RENDER_TIMEOUT_SECONDS', 0)

    probe, page = _probe_login([{'has_login_modal': False, 'qr_src': ''}], monkeypatch)

    assert (probe, page.calls) == ({'has_login_modal': False, 'qr_src': ''}, 1)


class _TimingOutPage(_FakePage):
    """A page whose first navigations stall past the timeout, like the live site once did."""

    def __init__(self, samples: list[dict], *, failures: int) -> None:
        super().__init__(samples)
        self.url = 'about:blank'
        self.failures = failures
        self.gotos: list[str] = []

    async def goto(self, url: str, wait_until: str = '') -> None:
        from playwright.async_api import TimeoutError as PlaywrightTimeoutError  # noqa: PLC0415

        self.gotos.append(url)
        if len(self.gotos) <= self.failures:
            msg = 'Page.goto: Timeout 30000ms exceeded.'
            raise PlaywrightTimeoutError(msg)
        self.url = url


def test_a_navigation_that_times_out_once_is_retried_rather_than_failing_the_run(monkeypatch) -> None:
    """The 2026-08-24 scheduled run: one slow homepage response cost the whole job.

    A navigation timeout says only that this particular response was slow, so the
    first one buys a retry; the probe then proceeds against the page it reached.
    """
    monkeypatch.setattr(rednote_browser_module, '_LOGIN_RENDER_POLL_SECONDS', 0)
    browser = PlaywrightNoteBrowser.__new__(PlaywrightNoteBrowser)
    page = _TimingOutPage([{'logged_in': True, 'user_id': '5ff'}], failures=1)
    browser._page = page

    probe = asyncio.run(browser.probe_login())

    assert (probe['logged_in'], len(page.gotos)) == (True, 2)


def test_a_navigation_that_keeps_timing_out_still_fails(monkeypatch) -> None:
    # A site that is actually unreachable must still be reported, not retried forever.
    from playwright.async_api import TimeoutError as PlaywrightTimeoutError  # noqa: PLC0415

    browser = PlaywrightNoteBrowser.__new__(PlaywrightNoteBrowser)
    page = _TimingOutPage([], failures=rednote_browser_module._GOTO_ATTEMPTS)
    browser._page = page

    with pytest.raises(PlaywrightTimeoutError):
        asyncio.run(browser.probe_login())
    assert len(page.gotos) == rednote_browser_module._GOTO_ATTEMPTS


def test_a_navigation_the_scan_interrupts_does_not_end_the_run(monkeypatch) -> None:
    """Scanning the QR makes the site navigate itself, aborting whatever we had in flight.

    Treating that as fatal would fail the run at the exact moment it succeeded.
    """
    qr = 'data:image/png;base64,' + base64.b64encode(QR_PNG).decode()

    async def _send_photo(*, photo, header='', caption='', require_enabled=True) -> int:
        return 1

    async def _send_text(*, header='', text, require_enabled=True) -> int:
        return 2

    monkeypatch.setattr(rednote_module, 'database', _FakeDatabase())
    monkeypatch.setattr(rednote_module.telegram_bot_tool, 'send_photo_now', _send_photo)
    monkeypatch.setattr(rednote_module.telegram_bot_tool, 'send_text_now', _send_text)
    monkeypatch.setattr(rednote_module, '_LOGIN_POLL_INTERVAL_SECONDS', 0)

    class _AbortingBrowser(_FakeBrowser):
        async def reload_login(self) -> None:
            self.reloads += 1
            raise RuntimeError('Page.goto: net::ERR_ABORTED at https://www.xiaohongshu.com/')

    browser = _AbortingBrowser(
        probes=[
            {'has_login_modal': True, 'qr_src': ''},
            {'has_login_modal': True, 'qr_src': qr},
            {'logged_in': True, 'user_id': 'me'},
        ],
    )
    job = _job(browser=browser)

    _run(job, job._await_login(browser))

    assert browser.reloads == 1
    assert job.user_id == 'me'


def test_both_scans_of_the_two_stage_login_are_sent(monkeypatch) -> None:
    """Signing in takes two QRs, and the second lives outside the login modal.

    Observed on the live site: the account QR is inside `.login-modal`, and the
    account-security QR that follows it is a separate captcha app on top. A selector
    scoped to the login modal sees only the first half, so a run would send one code,
    watch the user scan it, and then wait out its whole budget on a modal it cannot see.
    """
    sent: list[tuple[str, int]] = []
    login_qr = 'data:image/png;base64,' + base64.b64encode(QR_PNG).decode()
    verify_qr = 'data:image/png;base64,' + base64.b64encode(QR_PNG + b'\x01').decode()

    async def _send_photo(*, photo, header='', caption='', require_enabled=True) -> int:
        sent.append((caption, len(photo[1])))
        return 1

    async def _send_text(*, header='', text, require_enabled=True) -> int:
        return 2

    monkeypatch.setattr(rednote_module, 'database', _FakeDatabase())
    monkeypatch.setattr(rednote_module.telegram_bot_tool, 'send_photo_now', _send_photo)
    monkeypatch.setattr(rednote_module.telegram_bot_tool, 'send_text_now', _send_text)
    monkeypatch.setattr(rednote_module, '_LOGIN_POLL_INTERVAL_SECONDS', 0)

    browser = _FakeBrowser(
        probes=[
            {'has_login_modal': True, 'qr_stage': 'login', 'qr_src': login_qr},
            # Scanned: the verification modal opens on top, with its own code.
            {'has_login_modal': True, 'has_verify_modal': True, 'qr_stage': 'verify', 'qr_src': verify_qr},
            {'logged_in': True, 'user_id': 'me'},
        ],
    )
    job = _job(browser=browser)

    _run(job, job._await_login(browser))

    assert len(sent) == 2
    # The captions have to tell them apart: they are identical images in a chat, and
    # only the second one dies in a minute.
    assert '验证' in sent[1][0]
    assert job.user_id == 'me'


def test_the_verification_qr_is_reminted_on_a_timer(monkeypatch) -> None:
    """Its expiry leaves no mark: same image bytes, no class, only a sentence.

    And that sentence is in whatever language the profile runs in -- the pod pins
    zh-CN while the observation was made in English -- so the only durable trigger
    is the clock.
    """
    verify_qr = 'data:image/png;base64,' + base64.b64encode(QR_PNG).decode()

    async def _send_photo(*, photo, header='', caption='', require_enabled=True) -> int:
        return 1

    monkeypatch.setattr(rednote_module, 'database', _FakeDatabase())
    monkeypatch.setattr(rednote_module.telegram_bot_tool, 'send_photo_now', _send_photo)
    monkeypatch.setattr(rednote_module, '_LOGIN_POLL_INTERVAL_SECONDS', 0)
    monkeypatch.setattr(rednote_module, '_QR_REFRESH_SECONDS', 0)

    stuck = {'has_verify_modal': True, 'qr_stage': 'verify', 'qr_src': verify_qr}
    browser = _FakeBrowser(probes=[stuck, stuck, stuck, {'logged_in': True, 'user_id': 'me'}])
    job = _job(browser=browser)

    _run(job, job._await_login(browser))

    # The same src is never re-sent, but the page is told to mint a new one.
    assert browser.qr_refreshes >= 1


def test_a_scan_that_worked_is_noticed_rather_than_waited_out(monkeypatch) -> None:
    """The page does not update itself when the scan lands.

    Observed live: both modals close, the session cookie is set, and
    `__INITIAL_STATE__` still reports signed out because it is the snapshot the page
    was loaded with. That reads as UNKNOWN, and without a reload it reads as UNKNOWN
    for the rest of the budget -- failing a login that actually succeeded.
    """
    monkeypatch.setattr(rednote_module, 'database', _FakeDatabase())
    monkeypatch.setattr(rednote_module, '_LOGIN_POLL_INTERVAL_SECONDS', 0)
    monkeypatch.setattr(rednote_module, '_LOGIN_RELOAD_INTERVAL_SECONDS', 0)

    # No modal, no QR, no answer from the store -- and the cookie jar already has the
    # session the scan just created.
    after_scan = {'logged_in': False, 'has_login_modal': False, 'qr_src': ''}
    browser = _FakeBrowser(probes=[after_scan, after_scan, {'logged_in': True, 'user_id': 'me'}])
    job = _job(browser=browser)

    _run(job, job._await_login(browser))

    assert browser.reloads >= 1
    assert job.user_id == 'me'


def test_a_qr_is_sent_once_per_code_and_the_run_continues_after_the_scan(monkeypatch) -> None:
    sent: list[bytes] = []
    qr_a = 'data:image/png;base64,' + base64.b64encode(QR_PNG).decode()
    qr_b = 'data:image/png;base64,' + base64.b64encode(QR_PNG + b'\x00').decode()

    async def _send_photo(*, photo, header='', caption='', require_enabled=True) -> int:
        sent.append(photo[1])
        return 1

    async def _send_text(*, header='', text, require_enabled=True) -> int:
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
            {'logged_in': True, 'user_id': 'me'},
        ],
    )
    job = _job(browser=browser)

    _run(job, job._await_login(browser))

    assert len(sent) == 2
    assert job.user_id == 'me'


def test_a_login_that_is_never_scanned_ends_the_run_with_one_deduped_failure(monkeypatch) -> None:
    async def _send_photo(*, photo, header='', caption='', require_enabled=True) -> int:
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

    async def _send_photo(*, photo, header='', caption='', require_enabled=True) -> int:
        sent.append(photo[1])
        return 1

    fake_db = _FakeDatabase()
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    monkeypatch.setattr(rednote_module.telegram_bot_tool, 'send_photo_now', _send_photo)
    monkeypatch.setattr(rednote_module, '_LOGIN_POLL_INTERVAL_SECONDS', 0)

    qr = 'data:image/png;base64,' + base64.b64encode(QR_PNG).decode()
    job = _job(login_wait_seconds=1, login_prompt_cooldown_seconds=3600)

    for attempt in range(2):
        browser = _FakeBrowser(probes=[{'has_login_modal': True, 'qr_src': qr}])
        with pytest.raises(RedNoteError) as excinfo:
            asyncio.run(job._await_login(browser))
        if attempt == 1:
            # The suppressed run must not claim it just sent one, nor blame Telegram.
            assert 'still work' in str(excinfo.value)
    asyncio.run(job.client.aclose())

    assert len(sent) == 1


def test_a_rerun_after_the_qr_expired_sends_a_fresh_one_despite_the_cooldown(monkeypatch) -> None:
    # The cooldown is capped at the QR's lifetime: past it the old code is dead, and
    # suppression would fail the run while pointing at a code the app rejects.
    sent: list[bytes] = []

    async def _send_photo(*, photo, header='', caption='', require_enabled=True) -> int:
        sent.append(photo[1])
        return 1

    fake_db = _FakeDatabase()
    monkeypatch.setattr(rednote_module, 'database', fake_db)
    monkeypatch.setattr(rednote_module.telegram_bot_tool, 'send_photo_now', _send_photo)
    monkeypatch.setattr(rednote_module, '_LOGIN_POLL_INTERVAL_SECONDS', 0)

    qr = 'data:image/png;base64,' + base64.b64encode(QR_PNG).decode()
    job = _job(login_wait_seconds=1, login_prompt_cooldown_seconds=3600)
    expired = int(time.time()) - rednote_module._LOGIN_QR_LIFETIME_SECONDS - 60
    fake_db.state['login_prompt_at'] = str(expired)

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
                # The live shape, captured from a real live photo: opaque buckets, most
                # of them empty, and the one variant that exists names no codec at all.
                # This is the case that made the codec fallback necessary rather than
                # decorative -- without it the clip resolves to nothing and is dropped
                # while its still is archived and the row is marked done.
                'stream': {
                    'EF6': [],
                    'EF5': [],
                    'EF7': [],
                    'EF4': [{'masterUrl': 'https://cdn/live-clip.mp4', 'backupUrls': ['https://cdn/backup.mp4'], 'qualityType': 'HD'}],
                },
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
    # The 4K variant, not one of the smaller ones the web player streams.
    assert media[0].media_url == 'https://sns-video.xhscdn.com/stream/109_2160p.mp4'


def test_the_highest_resolution_stream_is_picked_across_all_buckets() -> None:
    """The buckets are opaque and versioned, and the codec name is no basis to rank on.

    RedNote renamed the codec field from `h264`/`h265` to `EF4`/`EF5`, which a picker
    keyed on codec name matched against nothing -- silently returning whatever URL it
    saw first, a 720p transcode of a 4K upload. Resolution is the stable signal: the
    largest pixel count wins wherever it sits.
    """
    # The 2160p variant sits in EF5 beside a 1080p one, and EF4 carries only smaller
    # sizes; the pick reaches across all of them for the largest.
    assert pick_video_url(_video_note_card()['video']['media']['stream']).endswith('109_2160p.mp4')


def test_a_stream_tie_on_resolution_breaks_to_the_higher_bitrate() -> None:
    # Same frame size, so the less-compressed copy is the one worth keeping.
    stream = {
        'EF5': [
            {'width': 1080, 'height': 1920, 'videoBitrate': 2_000_000, 'masterUrl': 'https://cdn/low.mp4'},
            {'width': 1080, 'height': 1920, 'videoBitrate': 6_000_000, 'masterUrl': 'https://cdn/high.mp4'},
        ],
    }
    assert pick_video_url(stream) == 'https://cdn/high.mp4'


def test_a_stream_that_states_no_resolution_still_yields_a_url() -> None:
    # A variant with no width/height must not be dropped: an unranked URL is still a
    # file, and returning nothing would archive a note's cover without its video.
    unsized = _video_note_card(video={'media': {'stream': {'EF7': [{'masterUrl': 'https://cdn/only.mp4'}]}}})
    assert extract_note_media(unsized, note_id=NOTE_ID, xsec_token='T')[0].media_url == 'https://cdn/only.mp4'

    # And the pre-rename shape still resolves, so a rollback of the site does not break it.
    legacy = _video_note_card(
        video={'media': {'stream': {'h264': [{'width': 720, 'height': 1280, 'master_url': 'https://cdn/legacy.mp4'}]}}}
    )
    assert extract_note_media(legacy, note_id=NOTE_ID, xsec_token='T')[0].media_url == 'https://cdn/legacy.mp4'


def test_a_video_note_yields_a_row_even_with_no_readable_stream() -> None:
    # The row is what the next run looks the note up by, so an unreadable video block
    # is not a reason to drop the note on the floor.
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


# ---------- walking the likes list ----------


def _resolved_note_ids(fake_db: _FakeDatabase) -> list[str]:
    return [row[0] for _table, _columns, rows, _conflict in fake_db.inserted for row in rows]


def test_the_newest_likes_arrive_with_the_page_and_are_not_skipped(monkeypatch) -> None:
    """The liked tab is served with its first screenful already rendered into it.

    So the first request the site makes is for what comes *after* those, and a crawl
    reading only requests starts below the newest likes -- the ones an incremental run
    exists to collect. It would then meet `abort_after` pages of already-archived
    notes and stop, having picked up none of them, on every run, forever.
    """
    database = _FakeDatabase(backfill_complete=True)
    monkeypatch.setattr(rednote_module, 'database', database)

    browser = _FakeBrowser(
        initial_notes=[{'note_id': 'newest-1', 'xsec_token': 'T1'}, {'note_id': 'newest-2', 'xsec_token': 'T2'}],
        pages=[_like_page([{'note_id': 'older-1', 'xsec_token': 'T3'}], cursor='c1')],
        notes={note: _image_note_card() for note in ('newest-1', 'newest-2', 'older-1')},
    )
    job = _job(browser=browser)

    _run(job, job._crawl(browser))

    archived = [row[0] for insert in database.inserted for row in insert[2]]
    assert 'newest-1' in archived
    assert 'newest-2' in archived
    # And the requested pages are still walked after them.
    assert 'older-1' in archived


def test_only_the_site_saying_404_counts_toward_retiring_a_note(monkeypatch) -> None:
    """A note is retired on evidence, and a failure to load is not evidence.

    Timeouts, navigation errors and a note that simply carries nothing all leave a
    live note looking exactly like a deleted one from here. Counting those would
    retire notes on a bad night and never look at them again.
    """
    database = _FakeDatabase()
    monkeypatch.setattr(rednote_module, 'database', database)

    class _Browser(_FakeBrowser):
        async def note_state(self, *, note_url: str, note_id: str) -> dict:
            if note_id == 'deleted':
                raise rednote_module.NoteGoneError('redirected to 404')
            if note_id == 'timed-out':
                raise TimeoutError('the page never settled')
            return {}  # read fine, carries nothing this source handles

    browser = _Browser()
    job = _job(browser=browser)
    notes = [NoteRef(note_id=n, xsec_token='T') for n in ('deleted', 'timed-out', 'empty')]

    _run(job, job._resolve_notes(notes, browser, run_id='run-1'))

    assert set(database.missing) == {'deleted'}


def test_three_separate_runs_have_to_agree_before_a_note_is_retired(monkeypatch) -> None:
    database = _FakeDatabase()
    monkeypatch.setattr(rednote_module, 'database', database)

    class _Browser(_FakeBrowser):
        async def note_state(self, *, note_url: str, note_id: str) -> dict:
            raise rednote_module.NoteGoneError('redirected to 404')

    browser = _Browser()
    job = _job(browser=browser)
    note = [NoteRef(note_id='deleted', xsec_token='T')]

    # Twice inside one run counts once: a note can come round twice in a single walk.
    _run(job, job._resolve_notes(note * 2, browser, run_id='run-1'))
    assert database.missing['deleted'][0] == 1
    assert _run(job, job._retired_note_ids(['deleted'])) == set()

    _run(job, job._resolve_notes(note, browser, run_id='run-2'))
    assert _run(job, job._retired_note_ids(['deleted'])) == set()

    _run(job, job._resolve_notes(note, browser, run_id='run-3'))
    assert _run(job, job._retired_note_ids(['deleted'])) == {'deleted'}


def test_a_note_that_reads_again_stops_being_counted_as_gone(monkeypatch) -> None:
    """Otherwise two unlucky runs plus one later hiccup retires a live note."""
    database = _FakeDatabase()
    monkeypatch.setattr(rednote_module, 'database', database)

    state = {'gone': True}

    class _Browser(_FakeBrowser):
        async def note_state(self, *, note_url: str, note_id: str) -> dict:
            if state['gone']:
                raise rednote_module.NoteGoneError('redirected to 404')
            return _image_note_card()

    browser = _Browser()
    job = _job(browser=browser)
    note = [NoteRef(note_id='flaky', xsec_token='T')]

    _run(job, job._resolve_notes(note, browser, run_id='run-1'))
    _run(job, job._resolve_notes(note, browser, run_id='run-2'))
    assert database.missing['flaky'][0] == 2

    state['gone'] = False
    _run(job, job._resolve_notes(note, browser, run_id='run-3'))

    assert 'flaky' not in database.missing


def test_a_retired_note_is_not_opened_again(monkeypatch) -> None:
    database = _FakeDatabase()
    database.missing['retired'] = (3, 'run-3')
    monkeypatch.setattr(rednote_module, 'database', database)

    browser = _FakeBrowser(notes={'fresh': _image_note_card()})
    job = _job(browser=browser)
    notes = [NoteRef(note_id='retired', xsec_token='T'), NoteRef(note_id='fresh', xsec_token='T')]

    _run(job, job._absorb_page(notes, browser=browser, seen=set(), page_index=1, run_id='run-4'))

    # The whole point of counting: it stops costing a page load on every run.
    assert all('retired' not in url for url in browser.note_urls)


def test_a_note_that_can_never_be_read_does_not_hold_the_stop_rule_open(monkeypatch) -> None:
    """The list churns: notes get deleted, and get unliked while being looked at.

    The old rule asked whether every id on a page was already known. A deleted note
    never gains rows, so it is never known, so a single one near the top reset the
    counter on every run -- the early stop became unreachable and every incremental
    run walked the whole list. Asking what the page *added* to the archive instead is
    indifferent both to that and to the list shifting underneath the walk.
    """
    database = _FakeDatabase(known_note_ids=('archived-1',), backfill_complete=True)
    monkeypatch.setattr(rednote_module, 'database', database)

    # 'gone' is offered by the list on every page and resolves to nothing, forever.
    browser = _FakeBrowser(
        initial_notes=[{'note_id': 'gone', 'xsec_token': 'T'}, {'note_id': 'archived-1', 'xsec_token': 'T'}],
        pages=[
            _like_page([{'note_id': 'gone', 'xsec_token': 'T'}, {'note_id': 'archived-1', 'xsec_token': 'T'}], cursor='c1'),
            _like_page([{'note_id': 'archived-1', 'xsec_token': 'T'}], cursor='c2'),
            _like_page([{'note_id': 'should-not-be-reached', 'xsec_token': 'T'}], cursor='c3'),
        ],
        notes={'gone': {}},
    )
    job = _job(browser=browser, abort_after=2)

    _run(job, job._crawl(browser))

    archived = [row[0] for insert in database.inserted for row in insert[2]]
    assert 'should-not-be-reached' not in archived


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

    assert _run(job, job._resolve_notes([NoteRef(note_id=NOTE_ID, xsec_token='TOKEN')], browser, run_id='run-1')) == 1

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

    assert _run(job, job._resolve_notes(notes, browser, run_id='run-1')) == 1


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

    downloaded = _run(job, job._download_pending(pending))

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

    assert _run(job, job._download_pending(pending)) == 2
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


def test_a_video_note_is_fetched_from_the_cdn_like_every_other_file(tmp_path) -> None:
    """No second client, no exported session: a video is a URL and a write.

    This used to shell out to yt-dlp with a copy of the account's cookies, which
    re-read the note page from xiaohongshu.com under a different TLS fingerprint --
    the shape risk control reads as automation, and the thing that cost the account
    its sessions once already.
    """

    async def _handler(request: httpx.Request) -> httpx.Response:
        assert 'xiaohongshu.com' not in str(request.url)
        assert 'cookie' not in {name.lower() for name in request.headers}
        return httpx.Response(200, content=b'video-bytes')

    job = _job(_handler, path=tmp_path / 'images', video_path=tmp_path / 'videos')

    dst_path = _run(
        job, job._download_file(_media(media_type='video', media_url='https://sns-video-v28.xhscdn.com/stream/109_2160p.mp4?sign=abc'))
    )

    assert dst_path == tmp_path / 'videos' / 'artist' / f'[artist]2025-08-12 [{NOTE_ID}_1].mp4'
    assert dst_path.read_bytes() == b'video-bytes'


def test_the_4k_stream_outranks_the_smaller_ones_beside_it() -> None:
    """A signed-in note carries the full-resolution stream beside the web player's copies.

    Observed live: a 4K upload is offered as 720p/1080p/1440p/2160p variants, and the
    2160p one matches the untranscoded original frame for frame. Taking any but the
    largest would quietly archive a fraction of the pixels.
    """
    media = extract_note_media(_video_note_card(), note_id=NOTE_ID, xsec_token='T')

    assert media[0].media_url == 'https://sns-video.xhscdn.com/stream/109_2160p.mp4'


def test_a_note_offered_only_smaller_streams_still_yields_its_best() -> None:
    # A logged-out page (or a small upload) is served without the high-res variants;
    # the largest present is still better than waiting for a re-resolve that cannot help.
    capped = _video_note_card(
        video={
            'media': {
                'stream': {
                    'EF4': [{'width': 720, 'height': 1280, 'masterUrl': 'https://cdn/720.mp4'}],
                    'EF5': [{'width': 1080, 'height': 1920, 'masterUrl': 'https://cdn/1080.mp4'}],
                },
            },
        },
    )
    assert extract_note_media(capped, note_id=NOTE_ID, xsec_token='T')[0].media_url == 'https://cdn/1080.mp4'


def test_the_upload_resolution_is_read_off_mediav2_in_either_spelling() -> None:
    # The page renders mediaV2 as a JSON string; the API sends it already parsed. Both
    # carry the source's own dimensions, which is the yardstick for a downscale.
    assert note_source_long_edge({'mediaV2': '{"video": {"width": 2160, "height": 3840}}'}) == 3840
    assert note_source_long_edge({'media_v2': {'video': {'width': 1080, 'height': 1920}}}) == 1920
    # Nothing to measure against is not an error; it just disables the check.
    assert note_source_long_edge({}) == 0
    assert note_source_long_edge({'mediaV2': 'not json'}) == 0


def test_a_stream_short_of_the_upload_is_said_out_loud_once(caplog) -> None:
    """A run that quietly stops getting the full resolution still succeeds; pixels drop."""
    job = _job()
    downscaled = _video_note_card(
        video={
            # Only up to 1080p on offer, but the upload itself is 4K.
            'media': {'stream': {'EF5': [{'width': 1080, 'height': 1920, 'masterUrl': 'https://cdn/1080.mp4'}]}},
            'mediaV2': '{"video": {"width": 2160, "height": 3840}}',
        },
    )
    media = extract_note_media(downscaled, note_id=NOTE_ID, xsec_token='T')

    with caplog.at_level(logging.WARNING):
        job._warn_if_downscaled(downscaled, media)
        job._warn_if_downscaled(downscaled, media)

    assert sum('high-res streams may be gone' in record.message for record in caplog.records) == 1
    asyncio.run(job.client.aclose())

    # A note whose best stream matches its upload says nothing at all.
    job = _job()
    caplog.clear()
    full = _video_note_card()
    with caplog.at_level(logging.WARNING):
        job._warn_if_downscaled(full, extract_note_media(full, note_id=NOTE_ID, xsec_token='T'))

    assert caplog.records == []
    asyncio.run(job.client.aclose())


def test_a_download_that_dies_half_way_leaves_nothing_that_looks_finished(tmp_path) -> None:
    """The next run skips whatever is already on disk, so a partial file is forever."""

    async def _handler(request: httpx.Request) -> httpx.Response:
        msg = 'connection reset'
        raise httpx.ReadError(msg)

    job = _job(_handler, path=tmp_path)

    with pytest.raises(httpx.ReadError):
        _run(job, job._download_file(_media(media_type='video', media_url='https://sns-video-bd.xhscdn.com/k')))

    assert list((tmp_path / 'artist').iterdir()) == []


def test_an_expired_video_url_is_recorded_without_leaving_a_stub(tmp_path) -> None:
    async def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(403)

    job = _job(_handler, path=tmp_path)

    with pytest.raises(MediaUrlStaleError, match='stale-url'):
        _run(job, job._download_file(_media(media_type='video', media_url='https://sns-video-bd.xhscdn.com/k')))

    assert list((tmp_path / 'artist').iterdir()) == []


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
    # Nothing to shell out to any more: the browser reads, httpx downloads.
    assert job.required_commands == ()
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
