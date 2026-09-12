"""QR-code binding of a WeChat iLink bot, driven from the settings page.

The flow is two calls: ``start`` fetches a fresh QR code from iLink and renders
it as a PNG for the browser; ``poll`` asks iLink whether it has been scanned.
Once confirmed, the ``bot_token`` and ids are written straight into the stored
``web.wechat`` section -- the browser only ever sees the masked token, so the
form draft can be saved afterwards without clobbering it.
"""

from __future__ import annotations

import asyncio
import base64
import io
import json
import secrets
from dataclasses import dataclass, field
from time import monotonic
from typing import TYPE_CHECKING, Any

import qrcode

from src.core import logger
from src.core.settings import WeChatAccount, WeChatTransport
from src.tool.wechat_ilink import DEFAULT_BASE_URL, ILinkClient, ILinkError
from src.tool.wechat_webwx import WebWxClient
from src.web.wechat import ensure_wechat_tables, save_webwx_session

from .errors import ApiError
from .settings_masking import mask_section

if TYPE_CHECKING:
    from collections.abc import Awaitable, Callable

    from pydantic import BaseModel

log = logger.get('wechat-login')

SECTION = 'web.wechat'
# iLink QR codes go stale after a few minutes; the plugin gives up at five.
LOGIN_TTL_SECONDS = 300.0
DEFAULT_POLL_TIMEOUT_SECONDS = 20.0
_QR_BOX_SIZE = 6
_QR_BORDER = 2


@dataclass(slots=True)
class LoginSession:
    account_name: str
    base_url: str
    qrcode: str
    qrcode_url: str
    # Draft values from the form, applied to the account when the scan lands.
    path: str = ''
    media_types: list[str] = field(default_factory=list)
    transport: WeChatTransport = 'ilink'
    # The web protocol needs the cookies from the login poll to finish the login,
    # so its client outlives the request that started the scan.
    webwx_client: WebWxClient | None = None
    started_at: float = field(default_factory=monotonic)

    def expired(self, now: float) -> bool:
        return now - self.started_at > LOGIN_TTL_SECONDS


def render_qr_png_data_url(content: str) -> str:
    """Render ``content`` as a PNG data URL; the browser drops it straight into an ``<img>``."""
    qr = qrcode.QRCode(box_size=_QR_BOX_SIZE, border=_QR_BORDER)
    qr.add_data(content)
    qr.make(fit=True)
    image = qr.make_image(fill_color='black', back_color='white')
    buffer = io.BytesIO()
    image.save(buffer, format='PNG')
    return 'data:image/png;base64,' + base64.b64encode(buffer.getvalue()).decode('ascii')


def _section_dict(model: BaseModel) -> dict[str, Any]:
    return json.loads(model.model_dump_json())


async def _persist_webwx_session(account_name: str, snapshot: str) -> None:
    """The worker owns these tables, but a scan can land before it ever ran."""
    await ensure_wechat_tables()
    await save_webwx_session(account_name, snapshot)


class WeChatLoginManager:
    """Holds the in-flight QR sessions of this API process.

    A session is a few strings and dies with the process; there is nothing to
    persist because a QR code that outlives an API restart is stale anyway.
    """

    def __init__(  # noqa: PLR0913
        self,
        *,
        section_getter: Callable[[str], BaseModel],
        section_saver: Callable[[str, dict[str, Any]], BaseModel],
        client_factory: Callable[[str], ILinkClient] | None = None,
        webwx_client_factory: Callable[[], WebWxClient] | None = None,
        webwx_session_saver: Callable[[str, str], Awaitable[None]] | None = None,
        clock: Callable[[], float] = monotonic,
    ) -> None:
        self._section_getter = section_getter
        self._section_saver = section_saver
        self._client_factory = client_factory or (lambda base_url: ILinkClient(base_url=base_url))
        self._webwx_client_factory = webwx_client_factory or WebWxClient
        self._webwx_session_saver = webwx_session_saver or _persist_webwx_session
        self._clock = clock
        self._sessions: dict[str, LoginSession] = {}

    def _purge_expired(self) -> None:
        now = self._clock()
        for key in [key for key, session in self._sessions.items() if session.expired(now)]:
            self._drop(key)

    def _drop(self, session_key: str) -> None:
        session = self._sessions.pop(session_key, None)
        if session is not None and session.webwx_client is not None:
            client = session.webwx_client
            session.webwx_client = None
            asyncio.get_running_loop().create_task(client.aclose())

    async def _stored_section(self) -> dict[str, Any]:
        model = await asyncio.to_thread(self._section_getter, SECTION)
        return _section_dict(model)

    async def start(
        self,
        account_name: str,
        *,
        path: str = '',
        media_types: list[str] | None = None,
        transport: WeChatTransport = 'ilink',
    ) -> dict[str, Any]:
        draft: dict[str, Any] = {'name': account_name, 'transport': transport}
        if path.strip():
            draft['path'] = path.strip()
        if media_types:
            draft['media_types'] = list(media_types)
        try:
            validated = WeChatAccount.model_validate(draft)
        except ValueError as exc:
            raise ApiError(status_code=422, code='invalid_settings', message=str(exc)) from None
        name = validated.name

        stored = await self._stored_section()
        existing = next((acc for acc in stored.get('accounts') or [] if str(acc.get('name') or '').casefold() == name.casefold()), None)
        base_url = str((existing or {}).get('base_url') or DEFAULT_BASE_URL)

        session = LoginSession(
            account_name=name,
            base_url=base_url,
            qrcode='',
            qrcode_url='',
            path=str(validated.path) if 'path' in draft else '',
            media_types=list(validated.media_types) if 'media_types' in draft else [],
            transport=validated.transport,
        )
        if validated.transport == 'filehelper':
            webwx = self._webwx_client_factory()
            try:
                uuid = await webwx.get_login_uuid()
            except ILinkError as exc:
                await webwx.aclose()
                raise ApiError(status_code=502, code='wechat_login_failed', message=str(exc)) from None
            except Exception as exc:  # noqa: BLE001
                await webwx.aclose()
                raise ApiError(status_code=502, code='wechat_login_failed', message=f'Could not reach web WeChat: {exc}') from None
            session.qrcode = uuid
            session.qrcode_url = webwx.login_qr_content(uuid)
            session.webwx_client = webwx
        else:
            client = self._client_factory(base_url)
            try:
                login = await client.get_bot_qrcode()
            except ILinkError as exc:
                raise ApiError(status_code=502, code='wechat_login_failed', message=str(exc)) from None
            except Exception as exc:  # noqa: BLE001
                raise ApiError(status_code=502, code='wechat_login_failed', message=f'Could not reach iLink: {exc}') from None
            finally:
                await client.aclose()
            session.qrcode = login.qrcode
            session.qrcode_url = login.qrcode_url

        self._purge_expired()
        session_key = secrets.token_urlsafe(16)
        self._sessions[session_key] = session
        return {
            'session_key': session_key,
            'account': name,
            'transport': validated.transport,
            'qrcode_url': session.qrcode_url,
            'qrcode_image': render_qr_png_data_url(session.qrcode_url),
            'expires_in_seconds': int(LOGIN_TTL_SECONDS),
        }

    async def poll(self, session_key: str, *, timeout_seconds: float = DEFAULT_POLL_TIMEOUT_SECONDS) -> dict[str, Any]:
        self._purge_expired()
        session = self._sessions.get(session_key)
        if session is None:
            raise ApiError(status_code=404, code='unknown_login_session', message='No such login session; start a new one.')

        if session.transport == 'filehelper':
            return await self._poll_webwx(session_key, session, timeout_seconds=timeout_seconds)

        client = self._client_factory(session.base_url)
        try:
            status = await client.get_qrcode_status(session.qrcode, timeout_seconds=timeout_seconds)
        except ILinkError as exc:
            self._drop(session_key)
            raise ApiError(status_code=502, code='wechat_login_failed', message=str(exc)) from None
        except Exception as exc:  # noqa: BLE001
            self._drop(session_key)
            raise ApiError(status_code=502, code='wechat_login_failed', message=f'Could not reach iLink: {exc}') from None
        finally:
            await client.aclose()

        if status.status == 'expired':
            self._drop(session_key)
            return {'status': 'expired', 'account': None}
        if status.status != 'confirmed':
            return {'status': status.status, 'account': None}

        self._drop(session_key)
        if not status.bot_token or not status.bot_id:
            raise ApiError(status_code=502, code='wechat_login_failed', message='iLink confirmed the scan without returning a bot token.')
        account = await self._store_credentials(session, status.bot_token, status.bot_id, status.user_id, status.base_url)
        log.notice('WeChat bot %s bound to account %s', status.bot_id, session.account_name)
        return {'status': 'confirmed', 'account': account}

    async def _poll_webwx(self, session_key: str, session: LoginSession, *, timeout_seconds: float) -> dict[str, Any]:
        webwx = session.webwx_client
        if webwx is None:
            self._drop(session_key)
            raise ApiError(status_code=404, code='unknown_login_session', message='No such login session; start a new one.')
        try:
            status = await webwx.poll_login(session.qrcode, timeout_seconds=timeout_seconds)
        except ILinkError as exc:
            self._drop(session_key)
            raise ApiError(status_code=502, code='wechat_login_failed', message=str(exc)) from None
        except Exception as exc:  # noqa: BLE001
            self._drop(session_key)
            raise ApiError(status_code=502, code='wechat_login_failed', message=f'Could not reach web WeChat: {exc}') from None

        if status.status == 'expired':
            self._drop(session_key)
            return {'status': 'expired', 'account': None}
        if status.status != 'confirmed':
            return {'status': status.status, 'account': None}

        snapshot = webwx.session_snapshot()
        self._drop(session_key)
        if not snapshot or not status.user_id:
            raise ApiError(status_code=502, code='wechat_login_failed', message='web WeChat confirmed the scan without a usable session.')
        await self._webwx_session_saver(session.account_name, snapshot)
        account = await self._store_credentials(session, '', '', status.user_id, '')
        log.notice('WeChat 文件传输助手 session (uin %s) bound to account %s', status.user_id, session.account_name)
        return {'status': 'confirmed', 'account': account}

    async def _store_credentials(self, session: LoginSession, bot_token: str, bot_id: str, user_id: str, base_url: str) -> dict[str, Any]:
        stored = await self._stored_section()
        accounts = [dict(acc) for acc in stored.get('accounts') or [] if isinstance(acc, dict)]
        credentials: dict[str, Any] = {'transport': session.transport, 'user_id': user_id}
        if session.transport == 'ilink':
            credentials.update({'bot_token': bot_token, 'bot_id': bot_id})
        if base_url:
            credentials['base_url'] = base_url.rstrip('/')
        if session.path:
            credentials['path'] = session.path
        if session.media_types:
            credentials['media_types'] = list(session.media_types)
        index = next(
            (i for i, acc in enumerate(accounts) if str(acc.get('name') or '').casefold() == session.account_name.casefold()), None
        )
        if index is None:
            fresh = WeChatAccount(name=session.account_name, path=f'./collection/wechat/{session.account_name}')
            accounts.append({**_section_dict(fresh), **credentials})
            index = len(accounts) - 1
        else:
            accounts[index] = {**accounts[index], **credentials}
        saved = await asyncio.to_thread(self._section_saver, SECTION, {**stored, 'accounts': accounts})
        masked = mask_section(SECTION, _section_dict(saved))
        return dict(masked['accounts'][index])
