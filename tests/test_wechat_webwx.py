# ruff: noqa: INP001, S101, S105, S106, ANN001, ANN002, ANN003, ANN202, ARG001, ARG002, ARG005, PLR2004, SLF001, EM101

import asyncio
import json
from pathlib import Path
from types import SimpleNamespace

import httpx
import pytest

import src.web.wechat as wechat_module
from src.api.errors import ApiError
from src.api.settings_masking import MASK_SUFFIX
from src.api.wechat_login import WeChatLoginManager
from src.core.settings import WeChat, WeChatAccount
from src.tool.wechat_ilink import CdnMedia, ILinkError, QrLoginStatus
from src.tool.wechat_webwx import (
    FILEHELPER_USER,
    MMWEB_APPID,
    WebWxClient,
    WebWxError,
    WebWxSession,
    parse_sync_message,
    resolve_hosts,
)

_NEWLOGIN_XML = (
    '<error><ret>0</ret><skey>@crypt_skey</skey><wxsid>sid-1</wxsid><wxuin>1234567</wxuin><pass_ticket>pt%2F1</pass_ticket></error>'
)
_SYNCKEY = {'Count': 2, 'List': [{'Key': 1, 'Val': 100}, {'Key': 2, 'Val': 200}]}


def _session(**overrides) -> WebWxSession:
    session = WebWxSession(
        device_id='e123', skey='@crypt_skey', sid='sid-1', uin='1234567', pass_ticket='pt', user_name='@me', synckey=dict(_SYNCKEY)
    )
    for key, value in overrides.items():
        setattr(session, key, value)
    return session


def _raw(msg_type: int, **fields) -> dict:
    base = {'MsgId': '7001', 'FromUserName': '@me', 'ToUserName': FILEHELPER_USER, 'MsgType': msg_type, 'CreateTime': 1700000000}
    base.update(fields)
    return base


# ---------------------------------------------------------------------------
# protocol helpers
# ---------------------------------------------------------------------------


def test_hosts_follow_the_entry_host_like_the_web_page_does() -> None:
    assert resolve_hosts('szfilehelper.weixin.qq.com') == ('login.wx2.qq.com', 'file.wx2.qq.com')
    assert resolve_hosts('cmfilehelper.weixin.qq.com') == ('login.wx8.qq.com', 'file.wx8.qq.com')
    assert resolve_hosts('filehelper.weixin.qq.com') == ('login.wx.qq.com', 'file.wx.qq.com')


def test_session_roundtrips_through_json_and_formats_synccheck_key() -> None:
    session = _session(cookies=[{'name': 'webwx_data_ticket', 'value': 't', 'domain': '.qq.com', 'path': '/'}])

    restored = WebWxSession.from_json(session.to_json())

    assert restored == session
    assert restored.synccheck_key() == '1_100|2_200'
    assert restored.base_request() == {'Uin': 1234567, 'Sid': 'sid-1', 'Skey': '@crypt_skey', 'DeviceID': 'e123'}
    assert WebWxSession.from_json('').authenticated is False


def test_parse_sync_message_keeps_only_the_filehelper_conversation() -> None:
    image = parse_sync_message(_raw(3))
    assert image is not None
    assert image.message_id == 7001
    assert image.from_user_id == FILEHELPER_USER
    assert image.is_user_message is True
    assert image.create_time_ms == 1700000000000
    assert [item.kind for item in image.media] == ['image']
    assert json.loads(image.media[0].media.encrypt_query_param) == {'kind': 'image', 'msg_id': 7001}
    assert image.media[0].media.aes_key == ''

    video = parse_sync_message(_raw(43))
    assert video is not None
    assert json.loads(video.media[0].media.encrypt_query_param)['kind'] == 'video'

    file_msg = parse_sync_message(_raw(49, AppMsgType=6, FileName='报告.pdf', FileSize='2048', MediaId='@mid', EncryFileName='enc'))
    assert file_msg is not None
    item = file_msg.media[0]
    assert item.kind == 'file'
    assert item.file_name == '报告.pdf'
    assert item.size == 2048
    assert json.loads(item.media.encrypt_query_param) == {
        'kind': 'file',
        'msg_id': 7001,
        'media_id': '@mid',
        'sender': '@me',
        'encry_filename': 'enc',
    }

    text = parse_sync_message(_raw(1, Content='hi &amp; bye'))
    assert text is not None
    assert text.text == 'hi & bye'
    assert text.media == ()

    # A link card without any URL is not media; a message in another chat is not ours.
    assert parse_sync_message(_raw(49, AppMsgType=5)).media == ()
    assert parse_sync_message(_raw(3, FromUserName='@friend', ToUserName='@me')) is None


# ---------------------------------------------------------------------------
# client
# ---------------------------------------------------------------------------


def _client(handler, **kwargs) -> WebWxClient:
    return WebWxClient(transport=httpx.MockTransport(handler), **kwargs)


def test_login_flow_parses_uuid_and_completes_on_confirmation() -> None:
    seen: list[str] = []
    saved: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        seen.append(url)
        if '/jslogin' in url:
            assert request.url.params['appid'] == MMWEB_APPID
            return httpx.Response(200, text='window.QRLogin.code = 200; window.QRLogin.uuid = "uuid-1";')
        if '/cgi-bin/mmwebwx-bin/login' in url:
            assert request.url.params['uuid'] == 'uuid-1'
            return httpx.Response(
                200,
                text='window.code=200;\nwindow.redirect_uri="https://szfilehelper.weixin.qq.com/cgi-bin/mmwebwx-bin/webwxnewloginpage?ticket=T&uuid=uuid-1&lang=zh_CN&scan=1";',
            )
        if '/webwxnewloginpage' in url:
            assert request.url.params['fun'] == 'new'
            assert request.url.params['ticket'] == 'T'
            assert request.headers['mmweb_appid'] == MMWEB_APPID
            return httpx.Response(
                200, text=_NEWLOGIN_XML, headers={'set-cookie': 'webwx_data_ticket=ticket-1; Path=/; Domain=szfilehelper.weixin.qq.com'}
            )
        if '/webwxinit' in url:
            body = json.loads(request.content)
            assert body['BaseRequest']['Sid'] == 'sid-1'
            return httpx.Response(200, json={'BaseResponse': {'Ret': 0}, 'User': {'UserName': '@me', 'Uin': 1234567}, 'SyncKey': _SYNCKEY})
        raise AssertionError(url)

    async def _save(snapshot: str) -> None:
        saved.append(snapshot)

    async def _run() -> tuple[str, QrLoginStatus, WebWxClient]:
        client = _client(handler, session_saver=_save)
        uuid = await client.get_login_uuid()
        assert client.login_qr_content(uuid) == 'https://login.weixin.qq.com/l/uuid-1'
        status = await client.poll_login(uuid, timeout_seconds=1)
        return uuid, status, client

    uuid, status, client = asyncio.run(_run())

    assert uuid == 'uuid-1'
    assert status.status == 'confirmed'
    assert status.user_id == '1234567'
    assert status.bot_id == '@me'
    session = client.session
    assert session is not None
    assert session.authenticated
    assert session.pass_ticket == 'pt%2F1'
    assert session.synckey == _SYNCKEY
    assert len(session.device_id) == 15
    # The saver saw the completed session, cookies included.
    restored = WebWxSession.from_json(saved[0])
    assert {c['name'] for c in restored.cookies} == {'webwx_data_ticket'}


@pytest.mark.parametrize(
    ('body', 'expected'), [('window.code=408;', 'wait'), ('window.code=201;', 'scaned'), ('window.code=400;', 'expired')]
)
def test_login_poll_maps_web_codes(body: str, expected: str) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text=body)

    status = asyncio.run(_client(handler).poll_login('u', timeout_seconds=1))

    assert status.status == expected


def test_get_updates_syncs_when_synccheck_reports_messages_and_persists_the_session() -> None:
    saved: list[str] = []
    calls: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        if '/synccheck' in url:
            calls.append('synccheck')
            # The first round carries the stored SyncKey; the second, the one webwxsync handed back.
            assert request.url.params['synckey'] == ('1_100|2_200' if len(calls) == 1 else '1_101|2_200')
            assert request.url.params['deviceid'] == 'e123'
            selector = '2' if len(calls) == 1 else '0'
            return httpx.Response(200, text=f'window.synccheck={{retcode:"0",selector:"{selector}"}}')
        if '/webwxsync' in url:
            calls.append('webwxsync')
            body = json.loads(request.content)
            assert body['SyncKey'] == _SYNCKEY
            return httpx.Response(
                200,
                json={
                    'BaseResponse': {'Ret': 0},
                    'SyncKey': {'Count': 2, 'List': [{'Key': 1, 'Val': 101}, {'Key': 2, 'Val': 200}]},
                    'AddMsgList': [
                        _raw(3),
                        _raw(1, MsgId='7002', Content='hello'),
                        _raw(3, MsgId='7003', FromUserName='@friend', ToUserName='@me'),
                    ],
                },
            )
        raise AssertionError(url)

    async def _save(snapshot: str) -> None:
        saved.append(snapshot)

    async def _run():
        client = _client(handler, session=_session(), session_saver=_save)
        first = await client.get_updates('', timeout_seconds=1)
        second = await client.get_updates('', timeout_seconds=1)
        return first, second, client

    first, second, client = asyncio.run(_run())

    assert calls == ['synccheck', 'webwxsync', 'synccheck']
    assert [m.message_id for m in first.messages] == [7001, 7002]
    assert first.get_updates_buf == ''
    assert second.messages == ()
    assert client.session.synckey['List'][0]['Val'] == 101
    assert WebWxSession.from_json(saved[0]).synckey['List'][0]['Val'] == 101


def test_synccheck_logout_and_idle_timeout_are_distinguished() -> None:
    def logout(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text='window.synccheck={retcode:"1101",selector:"0"}')

    def timeout(request: httpx.Request) -> httpx.Response:
        raise httpx.ReadTimeout('slow', request=request)

    with pytest.raises(WebWxError) as excinfo:
        asyncio.run(_client(logout, session=_session()).get_updates('', timeout_seconds=1))
    assert excinfo.value.session_expired is True
    assert isinstance(excinfo.value, ILinkError)

    page = asyncio.run(_client(timeout, session=_session()).get_updates('', timeout_seconds=1))
    assert page.messages == ()


def test_get_updates_without_a_stored_session_asks_for_a_scan() -> None:
    async def _load() -> str:
        return ''

    with pytest.raises(WebWxError) as excinfo:
        asyncio.run(_client(lambda request: httpx.Response(500), session_loader=_load).get_updates('', timeout_seconds=1))
    assert excinfo.value.session_expired is True


def test_download_media_routes_each_kind_to_its_endpoint(tmp_path: Path) -> None:
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return httpx.Response(
            200, content=b'bytes-' + request.url.path.rsplit('/', 1)[-1].encode(), headers={'content-type': 'application/octet-stream'}
        )

    session = _session(cookies=[{'name': 'webwx_data_ticket', 'value': 'dt', 'domain': 'szfilehelper.weixin.qq.com', 'path': '/'}])

    async def _run() -> list[int]:
        client = _client(handler, session=session)
        sizes = []
        for kind, extra in (
            ('image', {'msg_id': 7001}),
            ('video', {'msg_id': 7001}),
            ('file', {'msg_id': 7001, 'media_id': '@mid', 'sender': '@me', 'encry_filename': 'enc'}),
        ):
            media = CdnMedia(encrypt_query_param=json.dumps({'kind': kind, **extra}), aes_key='')
            sizes.append(await client.download_media(media, tmp_path / f'{kind}.bin', scratch=tmp_path / 'scratch'))
        return sizes

    sizes = asyncio.run(_run())

    image, video, file_req = seen
    assert image.url.path.endswith('/webwxgetmsgimg')
    assert image.url.params['MsgID'] == '7001'
    assert 'type' not in image.url.params
    assert video.url.path.endswith('/webwxgetvideo')
    assert video.headers['Range'] == 'bytes=0-'
    assert file_req.url.host == 'file.wx2.qq.com'
    assert file_req.url.params['mediaid'] == '@mid'
    assert file_req.url.params['webwx_data_ticket'] == 'dt'
    assert file_req.url.params['fromuser'] == '1234567'
    assert sizes == [len(b'bytes-webwxgetmsgimg'), len(b'bytes-webwxgetvideo'), len(b'bytes-webwxgetmedia')]
    assert (tmp_path / 'file.bin').read_bytes() == b'bytes-webwxgetmedia'


def test_download_media_rejects_an_error_page(tmp_path: Path) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json={'BaseResponse': {'Ret': 1}}, headers={'content-type': 'application/json'})

    media = CdnMedia(encrypt_query_param=json.dumps({'kind': 'image', 'msg_id': 1}), aes_key='')
    with pytest.raises(WebWxError) as excinfo:
        asyncio.run(_client(handler, session=_session()).download_media(media, tmp_path / 'x', scratch=tmp_path / 's'))
    assert excinfo.value.retryable is False


# ---------------------------------------------------------------------------
# settings and receiver
# ---------------------------------------------------------------------------


def test_filehelper_account_is_bound_by_uin_not_token() -> None:
    account = WeChatAccount(name='fh', transport='filehelper')
    assert account.logged_in is False
    assert account.validate_runnable() == ['user_id']

    bound = WeChatAccount(name='fh', transport='filehelper', user_id='1234567')
    assert bound.logged_in is True
    assert WeChat(accounts=[bound]).validate_runnable() == []
    assert WeChatAccount(name='x').transport == 'ilink'


def test_receiver_builds_a_web_client_for_filehelper_accounts_and_skips_the_sender_filter() -> None:
    receiver = wechat_module.WeChat()
    account = WeChatAccount(name='fh', transport='filehelper', user_id='1234567', media_types=['image'])

    client = receiver._default_client(account)
    assert isinstance(client, WebWxClient)

    message = parse_sync_message(_raw(3))
    assert message is not None
    assert [item.kind for item in wechat_module.WeChat.select_media(message, account)] == ['image']


def test_state_schema_adds_the_session_column_after_the_tables() -> None:
    sql = wechat_module._WECHAT_SCHEMA_SQL
    assert sql.index('CREATE TABLE IF NOT EXISTS wechat_account_state') < sql.index(
        'ALTER TABLE wechat_account_state ADD COLUMN IF NOT EXISTS webwx_session'
    )


# ---------------------------------------------------------------------------
# login manager
# ---------------------------------------------------------------------------


class _FakeWebWx:
    def __init__(self, statuses: list[QrLoginStatus]) -> None:
        self._statuses = statuses
        self.closed = 0

    async def get_login_uuid(self) -> str:
        return 'uuid-9'

    @staticmethod
    def login_qr_content(uuid: str) -> str:
        return f'https://login.weixin.qq.com/l/{uuid}'

    async def poll_login(self, uuid: str, *, timeout_seconds: float) -> QrLoginStatus:
        assert uuid == 'uuid-9'
        return self._statuses.pop(0)

    def session_snapshot(self) -> str | None:
        return _session().to_json()

    async def aclose(self) -> None:
        self.closed += 1


def _manager(statuses: list[QrLoginStatus]):
    saved: list[dict] = []
    sessions: list[tuple[str, str]] = []
    state = {'section': {'accounts': []}}
    fake = _FakeWebWx(statuses)

    def _getter(section: str):
        return WeChat.model_validate(state['section'])

    def _saver(section: str, payload: dict):
        validated = WeChat.model_validate(payload)
        state['section'] = json.loads(validated.model_dump_json())
        saved.append(payload)
        return validated

    async def _save_session(account_name: str, snapshot: str) -> None:
        sessions.append((account_name, snapshot))

    manager = WeChatLoginManager(
        section_getter=_getter,
        section_saver=_saver,
        client_factory=lambda base_url: SimpleNamespace(),
        webwx_client_factory=lambda: fake,
        webwx_session_saver=_save_session,
    )
    return manager, saved, sessions, fake


def test_filehelper_login_stores_the_session_and_marks_the_account_bound() -> None:
    confirmed = QrLoginStatus(status='confirmed', bot_id='@me', user_id='1234567')
    manager, saved, sessions, fake = _manager([QrLoginStatus(status='wait'), confirmed])

    async def _run():
        started = await manager.start('fh', path='collection/wechat/fh', media_types=['image', 'file'], transport='filehelper')
        key = started['session_key']
        first = await manager.poll(key)
        second = await manager.poll(key)
        await asyncio.sleep(0)
        return started, first, second

    started, first, second = asyncio.run(_run())

    assert started['transport'] == 'filehelper'
    assert started['qrcode_url'] == 'https://login.weixin.qq.com/l/uuid-9'
    assert started['qrcode_image'].startswith('data:image/png;base64,')
    assert first == {'status': 'wait', 'account': None}
    assert second['status'] == 'confirmed'
    account = second['account']
    assert account['transport'] == 'filehelper'
    assert account['user_id'] == '1234567'
    assert account['bot_token'] == ''
    assert account['path'] == 'collection/wechat/fh'
    assert account['media_types'] == ['image', 'file']
    assert sessions[0][0] == 'fh'
    assert WebWxSession.from_json(sessions[0][1]).uin == '1234567'
    assert saved[0]['accounts'][0]['transport'] == 'filehelper'
    assert fake.closed == 1


def test_filehelper_login_expiry_closes_the_web_client() -> None:
    manager, saved, sessions, fake = _manager([QrLoginStatus(status='expired')])

    async def _run():
        key = (await manager.start('fh', transport='filehelper'))['session_key']
        result = await manager.poll(key)
        await asyncio.sleep(0)
        return key, result

    key, result = asyncio.run(_run())

    assert result == {'status': 'expired', 'account': None}
    assert sessions == []
    assert saved == []
    assert fake.closed == 1
    with pytest.raises(ApiError):
        asyncio.run(manager.poll(key))


def test_ilink_login_still_masks_the_token() -> None:
    manager, _saved, sessions, _fake = _manager([])
    manager._client_factory = lambda base_url: SimpleNamespace(
        get_bot_qrcode=_async_value(SimpleNamespace(qrcode='q', qrcode_url='https://weixin.qq.com/x/q')),
        get_qrcode_status=lambda qrcode, *, timeout_seconds: _async_value(
            QrLoginStatus(status='confirmed', bot_token='secret-token', bot_id='b', user_id='u')
        )(),
        aclose=_async_value(None),
    )

    async def _run():
        key = (await manager.start('bot', transport='ilink'))['session_key']
        return await manager.poll(key)

    result = asyncio.run(_run())

    assert result['account']['transport'] == 'ilink'
    assert result['account']['bot_token'] == f'secr{MASK_SUFFIX}'
    assert sessions == []


def _async_value(value):
    async def _inner(*args, **kwargs):
        return value

    return _inner
