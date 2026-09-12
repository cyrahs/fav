"""Client for the web File Transfer Helper (网页版文件传输助手) protocol.

``filehelper.weixin.qq.com`` is the classic web-WeChat protocol scoped to one
conversation: the user's own 文件传输助手. That conversation is the one bot-like
target the WeChat client lets you 转发 to, which is what makes it useful here.

The shapes handed back mirror :mod:`src.tool.wechat_ilink` so the receiver in
``src/web/wechat.py`` runs either transport unchanged: ``get_updates`` yields
``InboundMessage`` objects whose media carry a JSON locator in
``CdnMedia.encrypt_query_param`` (no key -- web WeChat serves media in the
clear behind the session cookies), and ``download_media`` resolves it.

Protocol notes, gathered from the reference implementations rather than any
public document: login is ``jslogin`` → QR → ``login`` poll → ``webwxnewloginpage``
(skey/sid/uin/pass_ticket in XML) → ``webwxinit`` (SyncKey). Receiving is
``synccheck`` (the server holds the request ~25s) followed by ``webwxsync`` when
the selector is non-zero. Session state is cookies plus those tokens and the
rolling SyncKey; it is persisted through the injected saver after every sync.
"""

from __future__ import annotations

import asyncio
import html
import json
import re
import secrets
import time
from dataclasses import asdict, dataclass, field
from typing import TYPE_CHECKING, Any, Self
from urllib.parse import parse_qs, quote, urlparse

import httpx

from src.tool.wechat_ilink import (
    MESSAGE_TYPE_USER,
    SESSION_EXPIRED_ERRCODE,
    CdnMedia,
    ILinkError,
    InboundMessage,
    MediaItem,
    QrLoginStatus,
    UpdatesPage,
)

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Awaitable, Callable
    from pathlib import Path

DEFAULT_ENTRY_HOST = 'szfilehelper.weixin.qq.com'
MMWEB_APPID = 'wx_webfilehelper'
FILEHELPER_USER = 'filehelper'
LOGIN_QR_BASE = 'https://login.weixin.qq.com/l/'
_LANG = 'zh_CN'
_DEFAULT_TIMEOUT_SECONDS = 15.0
_DEFAULT_LONG_POLL_TIMEOUT_SECONDS = 25.0
_LONG_POLL_GRACE_SECONDS = 10.0
_DEVICE_ID_DIGITS = 15
_USER_AGENT = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36'
_USER_AGENT = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36'

# web WeChat message types
MSG_TYPE_TEXT = 1
MSG_TYPE_IMAGE = 3
MSG_TYPE_VIDEO = 43
MSG_TYPE_APP = 49
APP_MSG_TYPE_FILE = 6

_LOGIN_CODE_CONFIRMED = 200
_LOGIN_CODE_SCANNED = 201
_LOGIN_CODE_WAIT = 408
# synccheck retcodes that mean the session is gone.
_LOGOUT_RETCODES = {'1100', '1101', '1102'}
_SYNC_RET_SESSION_GONE = 1101

_UUID_RE = re.compile(r'window\.QRLogin\.uuid\s*=\s*"([^"]+)"')
_LOGIN_CODE_RE = re.compile(r'window\.code\s*=\s*(\d+)')
_REDIRECT_RE = re.compile(r'window\.redirect_uri\s*=\s*"([^"]+)"')
_SYNCCHECK_RE = re.compile(r'retcode\s*:\s*"?(\d+)"?\s*,\s*selector\s*:\s*"?(\d+)"?')


class WebWxError(ILinkError):
    """Raised for web-WeChat failures; ``session_expired`` means scan again."""


def resolve_hosts(entry_host: str) -> tuple[str, str]:
    """The login and file hosts paired with an entry host, as the web page does."""
    if 'cmfilehelper.weixin' in entry_host:
        return 'login.wx8.qq.com', 'file.wx8.qq.com'
    if 'szfilehelper.weixin.qq.com' in entry_host:
        return 'login.wx2.qq.com', 'file.wx2.qq.com'
    return 'login.wx.qq.com', 'file.wx.qq.com'


def _xml_tag(text: str, tag: str) -> str:
    match = re.search(rf'<{tag}>(.*?)</{tag}>', text, re.DOTALL)
    return match.group(1).strip() if match else ''


def _int(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


@dataclass(slots=True)
class WebWxSession:
    entry_host: str = DEFAULT_ENTRY_HOST
    device_id: str = ''
    skey: str = ''
    sid: str = ''
    uin: str = ''
    pass_ticket: str = ''
    user_name: str = ''
    synckey: dict[str, Any] = field(default_factory=lambda: {'Count': 0, 'List': []})
    cookies: list[dict[str, str]] = field(default_factory=list)

    @property
    def authenticated(self) -> bool:
        return bool(self.skey and self.sid and self.uin and self.pass_ticket)

    def to_json(self) -> str:
        return json.dumps(asdict(self), ensure_ascii=False, separators=(',', ':'))

    @classmethod
    def from_json(cls, text: str) -> WebWxSession:
        data = json.loads(text) if text.strip() else {}
        session = cls()
        for key in ('entry_host', 'device_id', 'skey', 'sid', 'uin', 'pass_ticket', 'user_name'):
            value = data.get(key)
            if isinstance(value, str) and value:
                setattr(session, key, value)
        if isinstance(data.get('synckey'), dict):
            session.synckey = data['synckey']
        if isinstance(data.get('cookies'), list):
            session.cookies = [c for c in data['cookies'] if isinstance(c, dict)]
        return session

    def synccheck_key(self) -> str:
        items = self.synckey.get('List') or []
        return '|'.join(f'{item.get("Key")}_{item.get("Val")}' for item in items if 'Key' in item and 'Val' in item)

    def base_request(self) -> dict[str, Any]:
        return {
            'Uin': int(self.uin) if self.uin.isdigit() else self.uin,
            'Sid': self.sid,
            'Skey': self.skey,
            'DeviceID': self.device_id,
        }


def _new_device_id() -> str:
    return ''.join(str(secrets.randbelow(10)) for _ in range(_DEVICE_ID_DIGITS))


def _locator(kind: str, **fields: Any) -> CdnMedia:
    return CdnMedia(encrypt_query_param=json.dumps({'kind': kind, **fields}, separators=(',', ':')), aes_key='')


def parse_sync_message(item: dict[str, Any]) -> InboundMessage | None:
    """Map one ``AddMsgList`` entry to the transport-neutral shape; ``None`` when it is not ours.

    Only the 文件传输助手 conversation is of interest. Messages forwarded from the
    phone arrive as sent by the user *to* ``filehelper``; the sender is therefore
    normalized to that constant rather than the per-login ``@...`` user name.
    """
    from_user = str(item.get('FromUserName') or '')
    to_user = str(item.get('ToUserName') or '')
    if FILEHELPER_USER not in (from_user, to_user):
        return None
    msg_id = _int(item.get('MsgId'))
    if msg_id <= 0:
        return None
    msg_type = _int(item.get('MsgType'))
    app_type = _int(item.get('AppMsgType'))
    text = ''
    media: list[MediaItem] = []
    if msg_type == MSG_TYPE_TEXT:
        text = html.unescape(str(item.get('Content') or '')).strip()
    elif msg_type == MSG_TYPE_IMAGE:
        media.append(MediaItem(index=0, kind='image', media=_locator('image', msg_id=msg_id)))
    elif msg_type == MSG_TYPE_VIDEO:
        media.append(MediaItem(index=0, kind='video', media=_locator('video', msg_id=msg_id)))
    elif msg_type == MSG_TYPE_APP and app_type == APP_MSG_TYPE_FILE:
        file_name = str(item.get('FileName') or '')
        media.append(
            MediaItem(
                index=0,
                kind='file',
                media=_locator(
                    'file',
                    msg_id=msg_id,
                    media_id=str(item.get('MediaId') or ''),
                    sender=from_user,
                    encry_filename=str(item.get('EncryFileName') or ''),
                ),
                file_name=file_name,
                size=_int(item.get('FileSize')),
            ),
        )
    return InboundMessage(
        message_id=msg_id,
        from_user_id=FILEHELPER_USER,
        to_user_id=FILEHELPER_USER,
        message_type=MESSAGE_TYPE_USER,
        context_token='',
        create_time_ms=_int(item.get('CreateTime')) * 1000,
        group_id='',
        text=text,
        media=tuple(media),
        raw=item,
    )


class WebWxClient:
    """Async client for one web 文件传输助手 session.

    ``session_loader`` / ``session_saver`` let the worker keep the rolling session
    in PostgreSQL; the login manager instead drives ``get_login_uuid`` /
    ``poll_login`` on a fresh instance and reads ``session_snapshot`` at the end.
    """

    def __init__(  # noqa: PLR0913
        self,
        *,
        entry_host: str = DEFAULT_ENTRY_HOST,
        session: WebWxSession | None = None,
        session_loader: Callable[[], Awaitable[str]] | None = None,
        session_saver: Callable[[str], Awaitable[None]] | None = None,
        timeout_seconds: float = _DEFAULT_TIMEOUT_SECONDS,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        self._session = session
        self._session_loader = session_loader
        self._session_saver = session_saver
        self._entry_host = entry_host.strip() or DEFAULT_ENTRY_HOST
        self._timeout_seconds = timeout_seconds
        self._client = httpx.AsyncClient(
            timeout=httpx.Timeout(timeout_seconds, read=60.0),
            transport=transport,
            follow_redirects=True,
            headers={'User-Agent': _USER_AGENT},
        )
        self._uuid = ''
        if session is not None:
            self._restore_cookies(session)

    async def aclose(self) -> None:
        await self._client.aclose()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self.aclose()

    # ---- session -----------------------------------------------------------

    @property
    def session(self) -> WebWxSession | None:
        return self._session

    @property
    def entry_host(self) -> str:
        return self._session.entry_host if self._session is not None else self._entry_host

    @property
    def login_host(self) -> str:
        return resolve_hosts(self.entry_host)[0]

    @property
    def file_host(self) -> str:
        return resolve_hosts(self.entry_host)[1]

    def _restore_cookies(self, session: WebWxSession) -> None:
        for cookie in session.cookies:
            name = cookie.get('name')
            if not name:
                continue
            self._client.cookies.set(name, cookie.get('value', ''), domain=cookie.get('domain', ''), path=cookie.get('path', '/'))

    def _capture_cookies(self, session: WebWxSession) -> None:
        session.cookies = [
            {'name': cookie.name, 'value': cookie.value, 'domain': cookie.domain or '', 'path': cookie.path or '/'}
            for cookie in self._client.cookies.jar
        ]

    def session_snapshot(self) -> str | None:
        """The session as JSON, cookies included, or ``None`` before login."""
        if self._session is None:
            return None
        self._capture_cookies(self._session)
        return self._session.to_json()

    async def _ensure_session(self) -> WebWxSession:
        if self._session is None and self._session_loader is not None:
            text = await self._session_loader()
            session = WebWxSession.from_json(text) if text else WebWxSession()
            self._session = session
            self._restore_cookies(session)
        if self._session is None or not self._session.authenticated:
            msg = 'web WeChat session is not logged in; scan the QR code from the settings page'
            raise WebWxError(msg, errcode=SESSION_EXPIRED_ERRCODE, retryable=False)
        return self._session

    async def _persist(self) -> None:
        if self._session_saver is None or self._session is None:
            return
        snapshot = self.session_snapshot()
        if snapshot is not None:
            await self._session_saver(snapshot)

    # ---- login -------------------------------------------------------------

    async def get_login_uuid(self) -> str:
        redirect = quote(f'https://{self._entry_host}/cgi-bin/mmwebwx-bin/webwxnewloginpage', safe='')
        login_host = resolve_hosts(self._entry_host)[0]
        url = f'https://{login_host}/jslogin?appid={MMWEB_APPID}&redirect_uri={redirect}&fun=new&lang={_LANG}&_={int(time.time() * 1000)}'
        response = await self._client.get(url)
        if response.status_code >= httpx.codes.BAD_REQUEST:
            msg = f'jslogin answered HTTP {response.status_code}'
            raise WebWxError(msg)
        match = _UUID_RE.search(response.text)
        if match is None:
            msg = f'jslogin answered without a uuid: {response.text[:120]}'
            raise WebWxError(msg)
        self._uuid = match.group(1)
        return self._uuid

    @staticmethod
    def login_qr_content(uuid: str) -> str:
        return f'{LOGIN_QR_BASE}{uuid}'

    async def poll_login(self, uuid: str, *, timeout_seconds: float = _DEFAULT_LONG_POLL_TIMEOUT_SECONDS) -> QrLoginStatus:
        """One ``login`` poll; the server holds it until something happens or ~25s pass.

        A confirmed scan completes the login here, so the returned status carries
        the uin and this instance holds a live session.
        """
        login_host = resolve_hosts(self._entry_host)[0]
        now = int(time.time() * 1000)
        url = (
            f'https://{login_host}/cgi-bin/mmwebwx-bin/login'
            f'?loginicon=true&uuid={quote(uuid, safe="")}&tip=0&r={~int(time.time())}&_={now}&appid={MMWEB_APPID}'
        )
        try:
            response = await self._client.get(url, timeout=httpx.Timeout(timeout_seconds + _LONG_POLL_GRACE_SECONDS))
        except httpx.TimeoutException:
            return QrLoginStatus(status='wait')
        if response.status_code >= httpx.codes.BAD_REQUEST:
            msg = f'login poll answered HTTP {response.status_code}'
            raise WebWxError(msg)
        match = _LOGIN_CODE_RE.search(response.text)
        code = int(match.group(1)) if match else 0
        if code == _LOGIN_CODE_WAIT:
            return QrLoginStatus(status='wait')
        if code == _LOGIN_CODE_SCANNED:
            return QrLoginStatus(status='scaned')
        if code != _LOGIN_CODE_CONFIRMED:
            return QrLoginStatus(status='expired')
        redirect = _REDIRECT_RE.search(response.text)
        if redirect is None:
            msg = 'login confirmed without a redirect_uri'
            raise WebWxError(msg)
        session = await self.complete_login(redirect.group(1))
        return QrLoginStatus(status='confirmed', bot_token='', bot_id=session.user_name, base_url='', user_id=session.uin)

    async def complete_login(self, redirect_uri: str) -> WebWxSession:
        parsed = urlparse(redirect_uri)
        query = parse_qs(parsed.query)
        entry_host = parsed.netloc or self._entry_host
        url = f'https://{entry_host}/cgi-bin/mmwebwx-bin/webwxnewloginpage'
        params = {
            'fun': 'new',
            'version': 'v2',
            'ticket': (query.get('ticket') or [''])[0],
            'uuid': (query.get('uuid') or [self._uuid])[0],
            'lang': (query.get('lang') or [_LANG])[0],
            'scan': (query.get('scan') or [''])[0],
        }
        response = await self._client.get(url, params=params, headers={'mmweb_appid': MMWEB_APPID})
        if response.status_code >= httpx.codes.BAD_REQUEST:
            msg = f'webwxnewloginpage answered HTTP {response.status_code}'
            raise WebWxError(msg)
        xml = response.text
        session = WebWxSession(
            entry_host=entry_host,
            device_id=_new_device_id(),
            skey=_xml_tag(xml, 'skey'),
            sid=_xml_tag(xml, 'wxsid'),
            uin=_xml_tag(xml, 'wxuin'),
            pass_ticket=_xml_tag(xml, 'pass_ticket'),
        )
        if not session.authenticated:
            msg = f'webwxnewloginpage answered without auth fields: {xml[:200]}'
            raise WebWxError(msg)
        self._session = session
        self._entry_host = entry_host
        await self._webwxinit()
        await self._persist()
        return session

    async def _webwxinit(self) -> None:
        session = self._session
        if session is None:
            msg = 'no session to initialize'
            raise WebWxError(msg)
        url = f'https://{session.entry_host}/cgi-bin/mmwebwx-bin/webwxinit'
        params = {'r': ~int(time.time() * 1000), 'lang': _LANG, 'pass_ticket': session.pass_ticket}
        response = await self._client.post(
            url, params=params, json={'BaseRequest': session.base_request()}, headers={'mmweb_appid': MMWEB_APPID}
        )
        data = self._json(response, 'webwxinit')
        ret = _int((data.get('BaseResponse') or {}).get('Ret'))
        if ret != 0:
            msg = f'webwxinit answered Ret={ret}'
            raise WebWxError(msg, errcode=SESSION_EXPIRED_ERRCODE if ret == _SYNC_RET_SESSION_GONE else None)
        user = data.get('User') or {}
        session.user_name = str(user.get('UserName') or session.user_name)
        if user.get('Uin'):
            session.uin = str(user['Uin'])
        if isinstance(data.get('SyncKey'), dict):
            session.synckey = data['SyncKey']

    @staticmethod
    def _json(response: httpx.Response, label: str) -> dict[str, Any]:
        if response.status_code >= httpx.codes.BAD_REQUEST:
            msg = f'{label} answered HTTP {response.status_code}'
            raise WebWxError(msg, retryable=response.status_code >= httpx.codes.INTERNAL_SERVER_ERROR)
        try:
            data = response.json()
        except ValueError as exc:
            msg = f'{label} answered non-JSON: {response.text[:120]}'
            raise WebWxError(msg) from exc
        if not isinstance(data, dict):
            msg = f'{label} answered a non-object payload'
            raise WebWxError(msg)
        return data

    # ---- receiving ---------------------------------------------------------

    async def _synccheck(self, session: WebWxSession, *, timeout_seconds: float) -> str:
        """``wait``, ``hasMsg`` or raise; a client-side timeout is ``wait``."""
        url = f'https://{session.entry_host}/cgi-bin/mmwebwx-bin/synccheck'
        params = {
            'r': int(time.time() * 1000),
            'skey': session.skey,
            'sid': session.sid,
            'uin': session.uin,
            'deviceid': session.device_id,
            'synckey': session.synccheck_key(),
            'mmweb_appid': MMWEB_APPID,
        }
        try:
            response = await self._client.get(url, params=params, timeout=httpx.Timeout(timeout_seconds + _LONG_POLL_GRACE_SECONDS))
        except httpx.TimeoutException:
            return 'wait'
        if response.status_code >= httpx.codes.BAD_REQUEST:
            msg = f'synccheck answered HTTP {response.status_code}'
            raise WebWxError(msg)
        match = _SYNCCHECK_RE.search(response.text)
        if match is None:
            msg = f'synccheck answered an unexpected body: {response.text[:120]}'
            raise WebWxError(msg)
        retcode, selector = match.group(1), match.group(2)
        if retcode in _LOGOUT_RETCODES:
            msg = f'web WeChat session ended (synccheck retcode {retcode})'
            raise WebWxError(msg, errcode=SESSION_EXPIRED_ERRCODE, retryable=False)
        if retcode != '0':
            msg = f'synccheck answered retcode {retcode}'
            raise WebWxError(msg)
        return 'hasMsg' if selector != '0' else 'wait'

    async def _webwxsync(self, session: WebWxSession) -> list[InboundMessage]:
        url = f'https://{session.entry_host}/cgi-bin/mmwebwx-bin/webwxsync'
        params = {'sid': session.sid, 'skey': session.skey, 'pass_ticket': session.pass_ticket}
        payload = {'BaseRequest': session.base_request(), 'SyncKey': session.synckey, 'rr': ~int(time.time() * 1000)}
        response = await self._client.post(url, params=params, json=payload, headers={'mmweb_appid': MMWEB_APPID})
        data = self._json(response, 'webwxsync')
        ret = _int((data.get('BaseResponse') or {}).get('Ret'))
        if ret == _SYNC_RET_SESSION_GONE:
            msg = 'web WeChat session ended (webwxsync Ret=1101)'
            raise WebWxError(msg, errcode=SESSION_EXPIRED_ERRCODE, retryable=False)
        if ret != 0:
            msg = f'webwxsync answered Ret={ret}'
            raise WebWxError(msg)
        if isinstance(data.get('SyncKey'), dict) and data['SyncKey'].get('List'):
            session.synckey = data['SyncKey']
        messages: list[InboundMessage] = []
        for item in data.get('AddMsgList') or []:
            if not isinstance(item, dict):
                continue
            parsed = parse_sync_message(item)
            if parsed is not None:
                messages.append(parsed)
        return messages

    async def get_updates(self, get_updates_buf: str, *, timeout_seconds: float = _DEFAULT_LONG_POLL_TIMEOUT_SECONDS) -> UpdatesPage:  # noqa: ARG002
        """One synccheck round, plus a webwxsync when it reports messages.

        The cursor argument is the iLink one and is ignored: web WeChat keeps its
        cursor (SyncKey) inside the session, which is persisted after each sync.
        """
        session = await self._ensure_session()
        status = await self._synccheck(session, timeout_seconds=timeout_seconds)
        if status != 'hasMsg':
            return UpdatesPage(messages=(), get_updates_buf='', long_poll_timeout_seconds=None)
        messages = await self._webwxsync(session)
        await self._persist()
        return UpdatesPage(messages=tuple(messages), get_updates_buf='', long_poll_timeout_seconds=None)

    # ---- media -------------------------------------------------------------

    def _media_request(self, session: WebWxSession, media: CdnMedia) -> tuple[str, dict[str, str]]:
        try:
            locator = json.loads(media.encrypt_query_param)
        except ValueError as exc:
            msg = 'media locator is not JSON'
            raise ValueError(msg) from exc
        kind = locator.get('kind')
        appid = f'&mmweb_appid={MMWEB_APPID}'
        skey = quote(session.skey, safe='')
        if kind == 'image':
            return f'https://{session.entry_host}/cgi-bin/mmwebwx-bin/webwxgetmsgimg?MsgID={locator["msg_id"]}&skey={skey}{appid}', {}
        if kind == 'video':
            url = f'https://{session.entry_host}/cgi-bin/mmwebwx-bin/webwxgetvideo?msgid={locator["msg_id"]}&skey={skey}{appid}'
            return url, {'Range': 'bytes=0-'}
        if kind == 'file':
            ticket = next((c.value for c in self._client.cookies.jar if c.name == 'webwx_data_ticket'), '')
            url = (
                f'https://{self.file_host}/cgi-bin/mmwebwx-bin/webwxgetmedia'
                f'?sender={quote(str(locator.get("sender") or ""), safe="")}'
                f'&mediaid={quote(str(locator.get("media_id") or ""), safe="")}'
                f'&encryfilename={quote(str(locator.get("encry_filename") or ""), safe="")}'
                f'&fromuser={quote(session.uin, safe="")}'
                f'&pass_ticket={quote(session.pass_ticket, safe="")}'
                f'&webwx_data_ticket={quote(ticket, safe="")}'
                f'&sid={quote(session.sid, safe="")}'
                f'{appid}'
            )
            return url, {}
        msg = f'unknown media locator kind {kind!r}'
        raise ValueError(msg)

    async def stream_media(self, media: CdnMedia) -> AsyncIterator[bytes]:
        session = await self._ensure_session()
        url, headers = self._media_request(session, media)
        async with self._client.stream('GET', url, headers=headers, timeout=httpx.Timeout(self._timeout_seconds, read=120.0)) as response:
            content_type = response.headers.get('content-type', '')
            if response.status_code >= httpx.codes.BAD_REQUEST or content_type.startswith(('text/', 'application/json')):
                body = (await response.aread())[:200]
                msg = f'media download answered HTTP {response.status_code} {content_type}: {body!r}'
                raise WebWxError(msg, retryable=response.status_code >= httpx.codes.INTERNAL_SERVER_ERROR)
            async for chunk in response.aiter_bytes():
                yield chunk

    async def download_media(self, media: CdnMedia, destination: Path, *, scratch: Path) -> int:  # noqa: ARG002
        """Stream the object straight to ``destination``; web WeChat media is not encrypted."""
        await asyncio.to_thread(destination.parent.mkdir, parents=True, exist_ok=True)
        handle = await asyncio.to_thread(destination.open, 'wb')
        size = 0
        try:
            async for chunk in self.stream_media(media):
                handle.write(chunk)
                size += len(chunk)
        finally:
            handle.close()
        return size
