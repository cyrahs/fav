# ruff: noqa: INP001, S101, S105, S106, ANN001, ANN003, ANN202, ARG001, ARG002, ARG005, PLR2004, SLF001, EM101, ASYNC240, RET501, PLR1711

import asyncio
import base64
import json
from contextlib import asynccontextmanager
from pathlib import Path
from types import SimpleNamespace

import httpx
import pytest
from fastapi.testclient import TestClient
from pydantic import ValidationError

import src.tool.wechat_queue as queue_module
import src.web.wechat as wechat_module
from src.api import create_app
from src.api.archive import ARCHIVE_SOURCES
from src.api.errors import ApiError
from src.api.schemas import JobRequestTarget
from src.api.service import FavApiService
from src.api.settings_masking import MASK_SUFFIX, mask_section, unmask_section
from src.api.wechat_login import WeChatLoginManager
from src.core import settings
from src.core.settings import SECTION_MODELS, SENSITIVE_FIELDS, WeChat, WeChatAccount
from src.service.jobs import JOB_KEYS
from src.tool.wechat_ilink import (
    SESSION_EXPIRED_ERRCODE,
    CdnMedia,
    ILinkClient,
    ILinkError,
    InboundMessage,
    QrLogin,
    QrLoginStatus,
    UpdatesPage,
    build_cdn_download_url,
    decrypt_aes_ecb,
    decrypt_file_aes_ecb,
    encrypt_aes_ecb,
    hex_key_to_base64,
    parse_aes_key,
    parse_message,
    parse_updates,
)
from src.tool.wechat_queue import WeChatMediaJob

_KEY = bytes(range(16))
_KEY_HEX = _KEY.hex()
_KEY_B64_RAW = base64.b64encode(_KEY).decode()
_KEY_B64_HEX = base64.b64encode(_KEY_HEX.encode()).decode()
_PNG = b'\x89PNG\r\n\x1a\n' + b'\x00' * 40
_TOKEN = 'token-for-tests'


# ---------------------------------------------------------------------------
# protocol helpers
# ---------------------------------------------------------------------------


def test_parse_aes_key_accepts_both_encodings_seen_in_the_wild() -> None:
    assert parse_aes_key(_KEY_B64_RAW) == _KEY
    assert parse_aes_key(_KEY_B64_HEX) == _KEY
    # Unpadded base64 is tolerated.
    assert parse_aes_key(_KEY_B64_RAW.rstrip('=')) == _KEY
    assert hex_key_to_base64(_KEY_HEX) == _KEY_B64_RAW


@pytest.mark.parametrize('bad', ['', 'not base64!', base64.b64encode(b'short').decode()])
def test_parse_aes_key_rejects_garbage(bad: str) -> None:
    with pytest.raises(ValueError, match='aes_key'):
        parse_aes_key(bad)


def test_aes_ecb_roundtrip_and_streaming_decrypt_agree(tmp_path: Path) -> None:
    plaintext = bytes(range(256)) * 300 + b'tail'
    ciphertext = encrypt_aes_ecb(plaintext, _KEY)

    assert decrypt_aes_ecb(ciphertext, _KEY) == plaintext

    source = tmp_path / 'in.enc'
    source.write_bytes(ciphertext)
    destination = tmp_path / 'out.bin'
    # A chunk smaller than the file forces the multi-round path.
    written = decrypt_file_aes_ecb(source, destination, _KEY, chunk_size=1024)

    assert written == len(plaintext)
    assert destination.read_bytes() == plaintext


def test_decrypt_rejects_wrong_padding() -> None:
    with pytest.raises(ValueError, match='padding'):
        decrypt_aes_ecb(b'\x00' * 16, _KEY)
    with pytest.raises(ValueError, match='multiple'):
        decrypt_aes_ecb(b'\x00' * 15, _KEY)


def test_build_cdn_download_url_percent_encodes_the_param() -> None:
    url = build_cdn_download_url('https://cdn.example/c2c/', 'a+b/c=')

    assert url == 'https://cdn.example/c2c/download?encrypted_query_param=a%2Bb%2Fc%3D'


def _raw_message(**overrides) -> dict:
    base = {
        'message_id': 42,
        'from_user_id': 'user@im.wechat',
        'to_user_id': 'bot@im.bot',
        'message_type': 1,
        'context_token': 'ctx',
        'create_time_ms': 1700000000000,
        'item_list': [
            {'type': 1, 'text_item': {'text': ' caption '}},
            {
                'type': 2,
                'image_item': {'media': {'encrypt_query_param': 'img-param', 'aes_key': 'ignored'}, 'aeskey': _KEY_HEX, 'hd_size': 12},
            },
            {'type': 3, 'voice_item': {'media': {'encrypt_query_param': 'voice-param', 'aes_key': _KEY_B64_HEX}}},
            {
                'type': 4,
                'file_item': {
                    'media': {'encrypt_query_param': 'file-param', 'aes_key': _KEY_B64_HEX},
                    'file_name': 'a.pdf',
                    'len': '99',
                    'md5': 'abc',
                },
            },
            {
                'type': 5,
                'video_item': {
                    'media': {'encrypt_query_param': 'video-param', 'aes_key': _KEY_B64_HEX},
                    'video_size': 7,
                    'video_md5': 'def',
                },
            },
        ],
    }
    base.update(overrides)
    return base


def test_parse_message_flattens_text_and_media_and_prefers_the_hex_image_key() -> None:
    message = parse_message(_raw_message())

    assert message.message_id == 42
    assert message.text == 'caption'
    assert message.is_user_message is True
    kinds = [(item.index, item.kind) for item in message.media]
    # The voice item is not archivable and is dropped; the text is not media.
    assert kinds == [(1, 'image'), (3, 'file'), (4, 'video')]
    image, file_item, video = message.media
    assert image.media.aes_key == _KEY_B64_RAW
    assert image.size == 12
    assert file_item.file_name == 'a.pdf'
    assert file_item.size == 99
    assert file_item.md5 == 'abc'
    assert video.size == 7
    assert video.md5 == 'def'


def test_parse_updates_validates_the_envelope() -> None:
    page = parse_updates({'ret': 0, 'msgs': [_raw_message()], 'get_updates_buf': 'cursor-2', 'longpolling_timeout_ms': 20000})

    assert len(page.messages) == 1
    assert page.get_updates_buf == 'cursor-2'
    assert page.long_poll_timeout_seconds == 20.0

    with pytest.raises(ILinkError) as excinfo:
        parse_updates({'ret': -1, 'errcode': SESSION_EXPIRED_ERRCODE, 'errmsg': 'expired'})
    assert excinfo.value.session_expired is True


def test_ilink_client_sends_bearer_and_cursor_and_treats_timeout_as_idle() -> None:
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        if len(seen) == 1:
            return httpx.Response(200, json={'ret': 0, 'msgs': [], 'get_updates_buf': 'next'})
        raise httpx.ReadTimeout('slow', request=request)

    async def _run() -> tuple[UpdatesPage, UpdatesPage]:
        async with ILinkClient(base_url='https://ilink.test/', bot_token=' tok ', transport=httpx.MockTransport(handler)) as client:
            first = await client.get_updates('prev', timeout_seconds=1)
            second = await client.get_updates('next', timeout_seconds=1)
        return first, second

    first, second = asyncio.run(_run())

    request = seen[0]
    assert str(request.url) == 'https://ilink.test/ilink/bot/getupdates'
    assert request.headers['Authorization'] == 'Bearer tok'
    assert request.headers['AuthorizationType'] == 'ilink_bot_token'
    assert request.headers['X-WECHAT-UIN']
    body = json.loads(request.content)
    assert body['get_updates_buf'] == 'prev'
    assert body['base_info']['channel_version']
    assert first.get_updates_buf == 'next'
    # The idle timeout is not an error: the caller just polls again with the same cursor.
    assert second.messages == ()
    assert second.get_updates_buf == 'next'


def test_ilink_client_download_media_decrypts_keyed_objects(tmp_path: Path) -> None:
    plaintext = b'hello wechat' * 100

    def handler(request: httpx.Request) -> httpx.Response:
        assert request.url.params['encrypted_query_param'] == 'p'
        return httpx.Response(200, content=encrypt_aes_ecb(plaintext, _KEY))

    async def _run() -> int:
        async with ILinkClient(cdn_base_url='https://cdn.test/c2c', transport=httpx.MockTransport(handler)) as client:
            return await client.download_media(CdnMedia('p', _KEY_B64_HEX), tmp_path / 'out' / 'file.bin', scratch=tmp_path / 'scratch.enc')

    size = asyncio.run(_run())

    assert size == len(plaintext)
    assert (tmp_path / 'out' / 'file.bin').read_bytes() == plaintext
    assert not (tmp_path / 'scratch.enc').exists()


def test_ilink_client_download_media_surfaces_cdn_errors(tmp_path: Path) -> None:
    async def _run() -> None:
        transport = httpx.MockTransport(lambda request: httpx.Response(403, text='denied'))
        async with ILinkClient(transport=transport) as client:
            await client.download_media(CdnMedia('p', ''), tmp_path / 'file.bin', scratch=tmp_path / 's.enc')

    with pytest.raises(ILinkError) as excinfo:
        asyncio.run(_run())
    assert excinfo.value.retryable is False


# ---------------------------------------------------------------------------
# settings and registries
# ---------------------------------------------------------------------------


def test_wechat_is_registered_everywhere_a_source_has_to_be() -> None:
    assert 'wechat' in JOB_KEYS
    assert JobRequestTarget.WECHAT.value == 'wechat'
    assert SECTION_MODELS['web.wechat'] is WeChat
    assert SENSITIVE_FIELDS['web.wechat'] == ('accounts[].bot_token',)
    assert ARCHIVE_SOURCES['wechat'].table == 'wechat'
    assert settings.Settings().web.wechat.enabled is False


def test_wechat_account_defaults_and_normalization() -> None:
    account = WeChatAccount(name=' main ', bot_token=' tok ', base_url='https://ilink.test/', media_types=['image', 'image', 'video'])

    assert account.name == 'main'
    assert account.bot_token == 'tok'
    assert account.base_url == 'https://ilink.test'
    assert account.media_types == ['image', 'video']
    assert account.logged_in is True
    assert WeChatAccount(name='x').media_types == ['video', 'image', 'file', 'link']


@pytest.mark.parametrize(
    'payload',
    [
        {'name': ''},
        {'name': 'bad name'},
        {'name': 'x', 'media_types': []},
        {'name': 'x', 'base_url': 'ilink.test'},
    ],
)
def test_wechat_account_rejects_invalid_values(payload: dict) -> None:
    with pytest.raises(ValidationError):
        WeChatAccount.model_validate(payload)


def test_wechat_validate_runnable_names_the_missing_pieces() -> None:
    assert WeChat().validate_runnable() == ['accounts']
    cfg = WeChat(accounts=[WeChatAccount(name='a'), WeChatAccount(name='b', bot_token='t')])
    assert cfg.validate_runnable() == ['accounts[0].bot_token']
    assert [account.name for account in cfg.resolved_accounts()] == ['b']
    assert cfg.get_account(' B ') is cfg.accounts[1]

    with pytest.raises(ValidationError, match='duplicate'):
        WeChat(accounts=[WeChatAccount(name='a'), WeChatAccount(name='A')])
    with pytest.raises(ValidationError):
        WeChat(long_poll_timeout_seconds=0)
    with pytest.raises(ValidationError):
        WeChat(max_download_attempts=0)


def test_wechat_bot_token_is_masked_and_restored_by_account_name() -> None:
    stored = {'accounts': [{'name': 'a', 'bot_token': 'secret-a', 'path': 'x'}, {'name': 'b', 'bot_token': 'secret-b', 'path': 'y'}]}

    masked = mask_section('web.wechat', stored)
    assert masked['accounts'][0]['bot_token'] == f'secr{MASK_SUFFIX}'
    assert stored['accounts'][0]['bot_token'] == 'secret-a'

    # Reordered and one omitted token: each is matched by name, never by position.
    edited = {'accounts': [{'name': 'b', 'path': 'y2'}, {'name': 'a', 'bot_token': f'secr{MASK_SUFFIX}', 'path': 'x'}]}
    merged = unmask_section('web.wechat', edited, stored)
    assert merged['accounts'][0]['bot_token'] == 'secret-b'
    assert merged['accounts'][1]['bot_token'] == 'secret-a'


# ---------------------------------------------------------------------------
# queue
# ---------------------------------------------------------------------------


def test_queue_schema_keys_on_message_and_item(monkeypatch) -> None:
    statements: list[str] = []

    async def _fake_multi(sql: str):
        statements.append(sql)
        return []

    monkeypatch.setattr(queue_module.database, 'query_db_multi', _fake_multi)
    asyncio.run(queue_module.ensure_wechat_media_queue_table())

    assert 'PRIMARY KEY (account_name, message_id, item_index)' in statements[0]
    assert "WHERE status = 'pending'" in statements[0]


def test_enqueue_reports_whether_a_row_was_inserted(monkeypatch) -> None:
    results = [[{'message_id': 1}], []]
    params: list[tuple] = []

    async def _fake_query_db(sql: str, args: tuple):
        params.append(args)
        return results.pop(0)

    monkeypatch.setattr(queue_module.database, 'query_db', _fake_query_db)
    kwargs = {
        'account_name': 'a',
        'message_id': 1,
        'item_index': 0,
        'media_type': 'image',
        'title': 't',
        'file_name': '',
        'from_user_id': 'u',
        'context_token': 'c',
        'encrypt_query_param': 'p',
        'aes_key': 'k',
        'media_size': 3,
        'media_md5': '',
    }

    assert asyncio.run(queue_module.enqueue_wechat_media_job(**kwargs)) is True
    assert asyncio.run(queue_module.enqueue_wechat_media_job(**kwargs)) is False
    assert params[0][:3] == ('a', 1, 0)


def test_retry_delay_backs_off_and_caps() -> None:
    assert queue_module.wechat_media_retry_delay(1) == 30.0
    assert queue_module.wechat_media_retry_delay(2) == 60.0
    assert queue_module.wechat_media_retry_delay(50) == 1800.0


# ---------------------------------------------------------------------------
# receiver
# ---------------------------------------------------------------------------


def _account(**overrides) -> WeChatAccount:
    payload = {'name': 'main', 'bot_token': 'tok', 'user_id': 'user@im.wechat', 'path': './collection/wechat/main'}
    payload.update(overrides)
    return WeChatAccount.model_validate(payload)


def _message(**overrides) -> InboundMessage:
    return parse_message(_raw_message(**overrides))


def _job(**overrides) -> WeChatMediaJob:
    payload = {
        'account_name': 'main',
        'message_id': 42,
        'item_index': 0,
        'media_type': 'image',
        'title': 'caption',
        'file_name': '',
        'from_user_id': 'user@im.wechat',
        'context_token': 'ctx',
        'encrypt_query_param': 'p',
        'aes_key': _KEY_B64_RAW,
        'media_size': 0,
        'media_md5': '',
        'attempt_count': 1,
    }
    payload.update(overrides)
    return WeChatMediaJob(**payload)


def test_select_media_only_trusts_the_bound_sender_and_configured_types() -> None:
    account = _account(media_types=['image', 'file'])

    kinds = [item.kind for item in wechat_module.WeChat.select_media(_message(), account)]
    assert kinds == ['image', 'file']

    assert wechat_module.WeChat.select_media(_message(from_user_id='stranger@im.wechat'), account) == []
    assert wechat_module.WeChat.select_media(_message(group_id='g1'), account) == []
    assert wechat_module.WeChat.select_media(_message(message_type=2), account) == []
    # No bound sender means no filter.
    open_account = _account(user_id='')
    assert len(wechat_module.WeChat.select_media(_message(from_user_id='anyone@im.wechat'), open_account)) == 3


def test_poll_once_persists_queue_rows_before_advancing_the_cursor(monkeypatch) -> None:
    events: list[str] = []

    class _Client:
        async def get_updates(self, buf: str, *, timeout_seconds: float) -> UpdatesPage:
            events.append(f'poll:{buf}')
            return UpdatesPage(messages=(_message(),), get_updates_buf='cursor-2', long_poll_timeout_seconds=12.0)

    async def _enqueue(**kwargs):
        events.append(f'enqueue:{kwargs["message_id"]}:{kwargs["item_index"]}')
        return True

    async def _get_state(account_name: str):
        return wechat_module.WeChatAccountState(get_updates_buf='cursor-1')

    async def _save_buf(account_name: str, buf: str) -> None:
        events.append(f'save:{buf}')

    async def _mark(account_name: str) -> None:
        events.append('mark')

    receiver = wechat_module.WeChat(client_factory=lambda account: _Client())
    monkeypatch.setattr(wechat_module, 'enqueue_wechat_media_job', _enqueue)
    monkeypatch.setattr(receiver, 'get_account_state', _get_state)
    monkeypatch.setattr(receiver, 'save_updates_buf', _save_buf)
    monkeypatch.setattr(receiver, 'mark_account_message', _mark)

    suggested = asyncio.run(receiver._poll_once(_account(), _Client(), timeout_seconds=35))

    assert suggested == 12.0
    assert events == ['poll:cursor-1', 'enqueue:42:1', 'enqueue:42:3', 'enqueue:42:4', 'mark', 'save:cursor-2']
    assert receiver._worker_wake_events['main'].is_set()


def test_build_filename_sniffs_images_and_keeps_file_names(tmp_path: Path) -> None:
    png = tmp_path / 'x'
    png.write_bytes(_PNG)
    assert wechat_module.WeChat.build_filename(_job(title='caption'), png) == 'caption [42].png'
    assert wechat_module.WeChat.build_filename(_job(title='', item_index=2), png) == 'image [42-2].png'
    assert wechat_module.WeChat.build_filename(_job(media_type='video', title=''), png) == 'video [42].mp4'
    assert wechat_module.WeChat.build_filename(_job(media_type='file', file_name='报告 v2.pdf'), png) == '报告 v2 [42].pdf'
    assert wechat_module.WeChat.build_filename(_job(media_type='file', file_name='', title=''), png) == 'file [42].bin'


def test_process_job_downloads_decrypts_archives_and_notifies(monkeypatch, tmp_path: Path) -> None:
    account = _account(path=str(tmp_path / 'wechat'))
    queries: list[tuple[str, tuple]] = []
    marks: list[str] = []
    notifications: list[dict] = []

    class _Client:
        async def download_media(self, media: CdnMedia, destination: Path, *, scratch: Path) -> int:
            assert media.encrypt_query_param == 'p'
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(_PNG)
            return len(_PNG)

    async def _query_db(sql: str, params: tuple = ()):
        queries.append((sql, params))
        return []

    async def _completed(job, owner_token):
        marks.append('completed')
        return True

    async def _notify(**kwargs):
        notifications.append(kwargs)

    receiver = wechat_module.WeChat()
    monkeypatch.setattr(wechat_module, 'database', SimpleNamespace(query_db=_query_db))
    monkeypatch.setattr(wechat_module, 'mark_wechat_media_job_completed', _completed)
    monkeypatch.setattr(wechat_module, 'enqueue_notification', _notify)

    downloaded = asyncio.run(receiver._process_job(account=account, client=_Client(), job=_job(), owner_token='o'))

    assert downloaded is True
    saved = tmp_path / 'wechat' / 'caption [42].png'
    assert saved.read_bytes() == _PNG
    assert marks == ['completed']
    insert = next(sql for sql, _ in queries if 'INSERT INTO wechat ' in sql)
    assert 'ON CONFLICT (account_name, message_id, item_index) DO NOTHING' in insert
    assert notifications[0]['source'] == 'wechat'
    assert notifications[0]['header'] == 'WeChat'
    assert notifications[0]['title'] == 'caption [42].png'
    assert notifications[0]['payload']['image_path'] == str(saved)
    assert not list(receiver.cache_dir.iterdir())


def test_process_job_discards_unfixable_errors_and_retries_transient_ones(monkeypatch) -> None:
    account = _account()
    outcomes: list[tuple[str, str]] = []

    class _Client:
        def __init__(self, exc: Exception) -> None:
            self._exc = exc

        async def download_media(self, media, destination, *, scratch):
            raise self._exc

    async def _query_db(sql: str, params: tuple = ()):
        return []

    async def _discard(job, owner_token, *, error):
        outcomes.append(('discard', error))
        return True

    async def _retry(job, owner_token, *, error, delay_seconds):
        outcomes.append(('retry', f'{delay_seconds:.0f}'))
        return True

    receiver = wechat_module.WeChat()
    monkeypatch.setattr(wechat_module, 'database', SimpleNamespace(query_db=_query_db))
    monkeypatch.setattr(wechat_module, 'mark_wechat_media_job_discarded', _discard)
    monkeypatch.setattr(wechat_module, 'mark_wechat_media_job_retry', _retry)

    asyncio.run(receiver._process_job(account=account, client=_Client(ValueError('invalid PKCS7 padding')), job=_job(), owner_token='o'))
    asyncio.run(
        receiver._process_job(account=account, client=_Client(httpx.ConnectError('down')), job=_job(attempt_count=2), owner_token='o')
    )
    asyncio.run(
        receiver._process_job(account=account, client=_Client(httpx.ConnectError('down')), job=_job(attempt_count=8), owner_token='o')
    )
    asyncio.run(receiver._process_job(account=account, client=_Client(ILinkError('gone', retryable=False)), job=_job(), owner_token='o'))
    asyncio.run(receiver._process_job(account=_account(media_types=['video']), client=_Client(RuntimeError()), job=_job(), owner_token='o'))

    assert outcomes == [
        ('discard', 'ValueError: invalid PKCS7 padding'),
        ('retry', '60'),
        ('discard', 'ConnectError: down'),
        ('discard', 'ILinkError: gone'),
        ('discard', 'Media type is no longer configured'),
    ]


def test_session_expiry_pauses_the_account_and_notifies_once(monkeypatch) -> None:
    pauses: list[tuple[str, float, str]] = []
    notifications: list[dict] = []

    async def _pause(account_name: str, seconds: float, *, error: str = '') -> None:
        pauses.append((account_name, seconds, error))

    async def _notify(**kwargs):
        notifications.append(kwargs)

    receiver = wechat_module.WeChat()
    monkeypatch.setattr(receiver, 'set_account_pause', _pause)
    monkeypatch.setattr(wechat_module, 'enqueue_notification', _notify)
    settings.load().web.wechat.session_pause_seconds = 120.0

    asyncio.run(receiver._handle_session_expired(_account(), ILinkError('expired', errcode=SESSION_EXPIRED_ERRCODE)))

    assert pauses == [('main', 120.0, 'session expired (errcode -14)')]
    assert notifications[0]['kind'] == 'session_expired'
    assert notifications[0]['dedupe_key'] == 'wechat:session-expired:main'


def test_update_without_the_listener_polls_once_and_drains(monkeypatch) -> None:
    events: list[str] = []

    @asynccontextmanager
    async def _lock(name: str):
        events.append(f'lock:{name}')
        yield True

    async def _multi(sql: str):
        return []

    async def _reset(account_name: str) -> int:
        return 0

    async def _get_state(account_name: str):
        return wechat_module.WeChatAccountState(get_updates_buf='')

    async def _poll_once(account, client, *, timeout_seconds):
        events.append(f'poll:{timeout_seconds:.0f}')
        return None

    async def _claim(account_name: str, owner_token: str):
        events.append('claim')
        return None

    class _Client:
        async def aclose(self) -> None:
            events.append('close')

    receiver = wechat_module.WeChat(client_factory=lambda account: _Client())
    monkeypatch.setattr(wechat_module, 'database', SimpleNamespace(advisory_lock=_lock, query_db_multi=_multi))
    monkeypatch.setattr(wechat_module, 'ensure_wechat_media_queue_table', lambda: _multi(''))
    monkeypatch.setattr(wechat_module, 'reset_processing_wechat_media_jobs', _reset)
    monkeypatch.setattr(wechat_module, 'claim_next_wechat_media_job', _claim)
    monkeypatch.setattr(receiver, 'get_account_state', _get_state)
    monkeypatch.setattr(receiver, '_poll_once', _poll_once)
    settings.load().web.wechat.accounts = [_account()]

    asyncio.run(receiver.update())

    assert events == ['lock:wechat-account:main', 'poll:10', 'claim', 'close']


# ---------------------------------------------------------------------------
# QR login
# ---------------------------------------------------------------------------


class _FakeLoginClient:
    def __init__(self, statuses: list[QrLoginStatus]) -> None:
        self._statuses = statuses
        self.closed = 0

    async def get_bot_qrcode(self, *, bot_type: str = '3') -> QrLogin:
        return QrLogin(qrcode='qr-key', qrcode_url='https://weixin.qq.com/x/abc')

    async def get_qrcode_status(self, qrcode: str, *, timeout_seconds: float) -> QrLoginStatus:
        assert qrcode == 'qr-key'
        return self._statuses.pop(0)

    async def aclose(self) -> None:
        self.closed += 1


def _login_manager(statuses: list[QrLoginStatus], *, stored: dict | None = None):
    saved: list[dict] = []
    state = {'section': stored or {'accounts': []}}
    client = _FakeLoginClient(statuses)

    def _getter(section: str):
        assert section == 'web.wechat'
        return WeChat.model_validate(state['section'])

    def _saver(section: str, payload: dict):
        validated = WeChat.model_validate(payload)
        state['section'] = json.loads(validated.model_dump_json())
        saved.append(payload)
        return validated

    manager = WeChatLoginManager(section_getter=_getter, section_saver=_saver, client_factory=lambda base_url: client)
    return manager, saved, client


def test_login_start_renders_a_qr_image_and_rejects_bad_names() -> None:
    manager, _saved, client = _login_manager([])

    started = asyncio.run(manager.start(' main '))

    assert started['account'] == 'main'
    assert started['qrcode_url'] == 'https://weixin.qq.com/x/abc'
    assert started['qrcode_image'].startswith('data:image/png;base64,')
    assert client.closed == 1

    with pytest.raises(ApiError) as excinfo:
        asyncio.run(manager.start('bad name'))
    assert excinfo.value.status_code == 422


def test_login_poll_stores_the_token_server_side_and_returns_it_masked() -> None:
    confirmed = QrLoginStatus(
        status='confirmed', bot_token='secret-token', bot_id='bot@im.bot', base_url='https://ilink.test/', user_id='me@im.wechat'
    )
    manager, saved, _client = _login_manager([QrLoginStatus(status='wait'), QrLoginStatus(status='scaned'), confirmed])

    started = asyncio.run(manager.start('main', path='collection/wechat/main', media_types=['image']))
    key = started['session_key']

    assert asyncio.run(manager.poll(key)) == {'status': 'wait', 'account': None}
    assert asyncio.run(manager.poll(key)) == {'status': 'scaned', 'account': None}
    result = asyncio.run(manager.poll(key))

    assert result['status'] == 'confirmed'
    account = result['account']
    assert account['bot_token'] == f'secr{MASK_SUFFIX}'
    assert account['bot_id'] == 'bot@im.bot'
    assert account['user_id'] == 'me@im.wechat'
    assert account['base_url'] == 'https://ilink.test'
    assert account['path'] == 'collection/wechat/main'
    assert account['media_types'] == ['image']
    assert saved[0]['accounts'][0]['bot_token'] == 'secret-token'
    # The session is single-use.
    with pytest.raises(ApiError) as excinfo:
        asyncio.run(manager.poll(key))
    assert excinfo.value.status_code == 404


def test_login_poll_updates_an_existing_account_in_place() -> None:
    stored = {'accounts': [{'name': 'other', 'bot_token': 'keep'}, {'name': 'main', 'bot_token': 'old', 'path': 'p'}]}
    confirmed = QrLoginStatus(status='confirmed', bot_token='new', bot_id='b', user_id='u')
    manager, saved, _client = _login_manager([confirmed], stored=stored)

    key = asyncio.run(manager.start('MAIN'))['session_key']
    result = asyncio.run(manager.poll(key))

    assert result['account']['name'] == 'main'
    accounts = saved[0]['accounts']
    assert [acc['name'] for acc in accounts] == ['other', 'main']
    assert accounts[0]['bot_token'] == 'keep'
    assert accounts[1]['bot_token'] == 'new'
    assert accounts[1]['path'] == 'p'


def test_login_poll_drops_an_expired_qr() -> None:
    manager, saved, _client = _login_manager([QrLoginStatus(status='expired')])
    key = asyncio.run(manager.start('main'))['session_key']

    assert asyncio.run(manager.poll(key)) == {'status': 'expired', 'account': None}
    assert saved == []
    with pytest.raises(ApiError):
        asyncio.run(manager.poll(key))


def test_login_routes_delegate_to_the_manager() -> None:
    class _Manager:
        async def start(self, account: str, *, path: str = '', media_types=None, transport: str = 'ilink'):
            return {
                'session_key': 'k',
                'transport': transport,
                'account': account,
                'qrcode_url': 'u',
                'qrcode_image': 'data:image/png;base64,AA==',
                'expires_in_seconds': 300,
            }

        async def poll(self, session_key: str):
            assert session_key == 'k'
            return {'status': 'confirmed', 'account': {'name': 'main', 'bot_token': f'secr{MASK_SUFFIX}'}}

    service = FavApiService(dsn='postgresql://db.local/fav', token=_TOKEN, wechat_login_manager=_Manager())
    headers = {'Authorization': f'Bearer {_TOKEN}'}
    with TestClient(create_app(service=service)) as client:
        started = client.post('/api/v2/wechat/login/start', json={'account': 'main', 'media_types': ['image']}, headers=headers)
        polled = client.post('/api/v2/wechat/login/poll', json={'session_key': 'k'}, headers=headers)
        unauthenticated = client.post('/api/v2/wechat/login/poll', json={'session_key': 'k'})

    assert started.status_code == 200
    assert started.json()['session_key'] == 'k'
    assert polled.status_code == 200
    assert polled.json()['account']['bot_token'] == f'secr{MASK_SUFFIX}'
    assert unauthenticated.status_code == 401
