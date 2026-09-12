"""Client for the WeChat iLink bot protocol (the ClawBot plugin API).

iLink is the official WeChat channel for personal-account bots: the user scans a
QR code once, the server hands back a ``bot_token``, and from then on the bot
long-polls ``getupdates`` for messages the user sends it. Media never arrives
inline -- each item carries a CDN reference plus an AES-128-ECB key, and the
bytes are fetched and decrypted here.

Everything is JSON over HTTP under ``ilinkai.weixin.qq.com``; the shapes mirror
the reference plugin (``tencent-weixin/openclaw-weixin``). Bytes fields travel
as base64 strings.
"""

from __future__ import annotations

import asyncio
import base64
import binascii
import contextlib
import secrets
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Literal, Self
from urllib.parse import quote

import httpx
from Crypto.Cipher import AES

if TYPE_CHECKING:
    from collections.abc import AsyncIterator
    from pathlib import Path

DEFAULT_BASE_URL = 'https://ilinkai.weixin.qq.com'
DEFAULT_CDN_BASE_URL = 'https://novac2c.cdn.weixin.qq.com/c2c'
# The bot_type the ClawBot plugin registers as; the QR code encodes it.
DEFAULT_BOT_TYPE = '3'
# Sent as base_info.channel_version on every call. Mirrors the reference plugin
# version line rather than inventing a new identifier the server has never seen.
CHANNEL_VERSION = '1.0.2'
# Returned by getupdates when the bot_token is no longer valid.
SESSION_EXPIRED_ERRCODE = -14

_AES_BLOCK_SIZE = 16
_AES_KEY_BYTES = 16
_HEX_KEY_LENGTH = 32
_DEFAULT_TIMEOUT_SECONDS = 15.0
_DEFAULT_LONG_POLL_TIMEOUT_SECONDS = 35.0
_LONG_POLL_GRACE_SECONDS = 5.0

MessageItemKind = Literal['text', 'image', 'voice', 'file', 'video', 'link']
QrStatus = Literal['wait', 'scaned', 'confirmed', 'expired']

ITEM_TYPE_TEXT = 1
ITEM_TYPE_IMAGE = 2
ITEM_TYPE_VOICE = 3
ITEM_TYPE_FILE = 4
ITEM_TYPE_VIDEO = 5
MESSAGE_TYPE_USER = 1

_ITEM_KINDS: dict[int, MessageItemKind] = {
    ITEM_TYPE_TEXT: 'text',
    ITEM_TYPE_IMAGE: 'image',
    ITEM_TYPE_VOICE: 'voice',
    ITEM_TYPE_FILE: 'file',
    ITEM_TYPE_VIDEO: 'video',
}


class ILinkError(RuntimeError):
    """The iLink API answered with an error or unusable payload."""

    def __init__(self, message: str, *, errcode: int | None = None, retryable: bool = True) -> None:
        super().__init__(message)
        self.errcode = errcode
        self.retryable = retryable

    @property
    def session_expired(self) -> bool:
        return self.errcode == SESSION_EXPIRED_ERRCODE


@dataclass(frozen=True, slots=True)
class CdnMedia:
    encrypt_query_param: str
    # base64 of either the raw 16-byte key or its 32-char hex spelling; empty
    # means the CDN object is stored in the clear.
    aes_key: str = ''


@dataclass(frozen=True, slots=True)
class MediaItem:
    index: int
    kind: MessageItemKind
    media: CdnMedia
    file_name: str = ''
    size: int = 0
    md5: str = ''


@dataclass(frozen=True, slots=True)
class InboundMessage:
    message_id: int
    from_user_id: str
    to_user_id: str
    message_type: int
    context_token: str
    create_time_ms: int
    group_id: str
    text: str
    media: tuple[MediaItem, ...]
    raw: dict[str, Any] = field(repr=False)

    @property
    def is_user_message(self) -> bool:
        return self.message_type == MESSAGE_TYPE_USER


@dataclass(frozen=True, slots=True)
class UpdatesPage:
    messages: tuple[InboundMessage, ...]
    get_updates_buf: str
    long_poll_timeout_seconds: float | None


@dataclass(frozen=True, slots=True)
class QrLogin:
    qrcode: str
    qrcode_url: str


@dataclass(frozen=True, slots=True)
class QrLoginStatus:
    status: QrStatus
    bot_token: str = ''
    bot_id: str = ''
    base_url: str = ''
    user_id: str = ''


def random_wechat_uin() -> str:
    """The ``X-WECHAT-UIN`` header: a random uint32, as decimal text, base64-encoded."""
    value = secrets.randbits(32)
    return base64.b64encode(str(value).encode('ascii')).decode('ascii')


def parse_aes_key(aes_key_base64: str) -> bytes:
    """Recover the raw 16-byte key from the two encodings seen in the wild.

    Images carry ``base64(raw 16 bytes)``; files, voice and video carry
    ``base64(32-char hex string)``, which decodes to ASCII hex that still has to
    be unhexed. An unpadded base64 string is tolerated.
    """
    text = aes_key_base64.strip()
    if not text:
        msg = 'aes_key is empty'
        raise ValueError(msg)
    padded = text + '=' * ((4 - len(text) % 4) % 4)
    try:
        decoded = base64.b64decode(padded, validate=True)
    except (binascii.Error, ValueError) as exc:
        msg = f'aes_key is not valid base64: {exc}'
        raise ValueError(msg) from exc
    if len(decoded) == _AES_KEY_BYTES:
        return decoded
    if len(decoded) == _HEX_KEY_LENGTH:
        try:
            return bytes.fromhex(decoded.decode('ascii'))
        except (UnicodeDecodeError, ValueError) as exc:
            msg = 'aes_key decodes to 32 bytes that are not a hex string'
            raise ValueError(msg) from exc
    msg = f'aes_key must decode to 16 raw bytes or a 32-char hex string, got {len(decoded)} bytes'
    raise ValueError(msg)


def hex_key_to_base64(hex_key: str) -> str:
    """``image_item.aeskey`` is plain hex; normalize it to the base64 form the rest of the code expects."""
    return base64.b64encode(bytes.fromhex(hex_key.strip())).decode('ascii')


def build_cdn_download_url(cdn_base_url: str, encrypt_query_param: str) -> str:
    return f'{cdn_base_url.rstrip("/")}/download?encrypted_query_param={quote(encrypt_query_param, safe="")}'


def decrypt_aes_ecb(ciphertext: bytes, key: bytes) -> bytes:
    """One-shot AES-128-ECB decrypt with PKCS7 unpadding."""
    if not ciphertext or len(ciphertext) % _AES_BLOCK_SIZE != 0:
        msg = f'ciphertext length {len(ciphertext)} is not a multiple of {_AES_BLOCK_SIZE}'
        raise ValueError(msg)
    plain = AES.new(key, AES.MODE_ECB).decrypt(ciphertext)
    return _pkcs7_unpad(plain)


def encrypt_aes_ecb(plaintext: bytes, key: bytes) -> bytes:
    """AES-128-ECB with PKCS7 padding; the inverse of :func:`decrypt_aes_ecb`, used by tests."""
    pad = _AES_BLOCK_SIZE - (len(plaintext) % _AES_BLOCK_SIZE)
    return AES.new(key, AES.MODE_ECB).encrypt(plaintext + bytes([pad]) * pad)


def _pkcs7_unpad(data: bytes) -> bytes:
    if not data:
        msg = 'cannot unpad an empty payload'
        raise ValueError(msg)
    pad = data[-1]
    if pad < 1 or pad > _AES_BLOCK_SIZE or data[-pad:] != bytes([pad]) * pad:
        msg = 'invalid PKCS7 padding'
        raise ValueError(msg)
    return data[:-pad]


def decrypt_file_aes_ecb(source: Path, destination: Path, key: bytes, *, chunk_size: int = 1 << 20) -> int:
    """Decrypt ``source`` into ``destination`` block by block; returns the plaintext size.

    A WeChat video can be hundreds of megabytes, so the ciphertext is never held
    in memory whole. ECB has no chaining, so any multiple of the block size can
    be decrypted independently; only the final block carries padding.
    """
    total = source.stat().st_size
    if total == 0 or total % _AES_BLOCK_SIZE != 0:
        msg = f'ciphertext length {total} is not a multiple of {_AES_BLOCK_SIZE}'
        raise ValueError(msg)
    chunk_size -= chunk_size % _AES_BLOCK_SIZE
    cipher = AES.new(key, AES.MODE_ECB)
    written = 0
    with source.open('rb') as reader, destination.open('wb') as writer:
        remaining = total
        while remaining > _AES_BLOCK_SIZE:
            take = min(chunk_size, remaining - _AES_BLOCK_SIZE)
            block = reader.read(take)
            plain = cipher.decrypt(block)
            writer.write(plain)
            written += len(plain)
            remaining -= take
        last = reader.read(remaining)
        plain = _pkcs7_unpad(cipher.decrypt(last))
        writer.write(plain)
        written += len(plain)
    return written


def _cdn_media(payload: Any) -> CdnMedia | None:
    if not isinstance(payload, dict):
        return None
    param = str(payload.get('encrypt_query_param') or '')
    if not param:
        return None
    return CdnMedia(encrypt_query_param=param, aes_key=str(payload.get('aes_key') or ''))


def _int(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def _parse_media_item(index: int, item: dict[str, Any]) -> MediaItem | None:  # noqa: PLR0911
    kind = _ITEM_KINDS.get(_int(item.get('type')))
    if kind == 'image':
        image = item.get('image_item') or {}
        media = _cdn_media(image.get('media'))
        if media is None:
            return None
        # image_item.aeskey (hex) is the key that actually opens the CDN object;
        # media.aes_key may be absent or in a different form.
        hex_key = str(image.get('aeskey') or '').strip()
        if hex_key:
            with contextlib.suppress(ValueError):
                media = CdnMedia(encrypt_query_param=media.encrypt_query_param, aes_key=hex_key_to_base64(hex_key))
        return MediaItem(index=index, kind='image', media=media, size=_int(image.get('hd_size') or image.get('mid_size')))
    if kind == 'video':
        video = item.get('video_item') or {}
        media = _cdn_media(video.get('media'))
        if media is None:
            return None
        return MediaItem(index=index, kind='video', media=media, size=_int(video.get('video_size')), md5=str(video.get('video_md5') or ''))
    if kind == 'file':
        file_item = item.get('file_item') or {}
        media = _cdn_media(file_item.get('media'))
        if media is None:
            return None
        return MediaItem(
            index=index,
            kind='file',
            media=media,
            file_name=str(file_item.get('file_name') or ''),
            size=_int(file_item.get('len')),
            md5=str(file_item.get('md5') or ''),
        )
    return None


def parse_message(payload: dict[str, Any]) -> InboundMessage:
    """Flatten one ``WeixinMessage`` into the fields the archive cares about."""
    items = payload.get('item_list') or []
    texts: list[str] = []
    media: list[MediaItem] = []
    for index, item in enumerate(items):
        if not isinstance(item, dict):
            continue
        if _int(item.get('type')) == ITEM_TYPE_TEXT:
            text = str((item.get('text_item') or {}).get('text') or '').strip()
            if text:
                texts.append(text)
            continue
        parsed = _parse_media_item(index, item)
        if parsed is not None:
            media.append(parsed)
    return InboundMessage(
        message_id=_int(payload.get('message_id')),
        from_user_id=str(payload.get('from_user_id') or ''),
        to_user_id=str(payload.get('to_user_id') or ''),
        message_type=_int(payload.get('message_type')),
        context_token=str(payload.get('context_token') or ''),
        create_time_ms=_int(payload.get('create_time_ms')),
        group_id=str(payload.get('group_id') or ''),
        text='\n'.join(texts),
        media=tuple(media),
        raw=payload,
    )


def parse_updates(payload: dict[str, Any]) -> UpdatesPage:
    """Validate a ``getupdates`` response and parse its messages.

    Raises :class:`ILinkError` on a non-zero ``ret``/``errcode``; the caller
    decides whether that is a pause (session expired) or a plain retry.
    """
    ret = _int(payload.get('ret'))
    errcode = _int(payload.get('errcode'))
    if ret != 0 or errcode != 0:
        code = errcode or ret
        errmsg = str(payload.get('errmsg') or '')
        msg = f'getupdates failed: ret={ret} errcode={errcode} {errmsg}'.rstrip()
        raise ILinkError(msg, errcode=code)
    messages = tuple(parse_message(item) for item in payload.get('msgs') or [] if isinstance(item, dict))
    timeout_ms = _int(payload.get('longpolling_timeout_ms'))
    return UpdatesPage(
        messages=messages,
        get_updates_buf=str(payload.get('get_updates_buf') or ''),
        long_poll_timeout_seconds=timeout_ms / 1000 if timeout_ms > 0 else None,
    )


class ILinkClient:
    """Thin async wrapper over the handful of iLink endpoints the archive needs.

    ``transport`` exists for tests; production passes nothing and gets a plain
    ``httpx.AsyncClient`` that honours ``HTTP_PROXY`` like the rest of the app.
    """

    def __init__(
        self,
        *,
        base_url: str = DEFAULT_BASE_URL,
        cdn_base_url: str = DEFAULT_CDN_BASE_URL,
        bot_token: str = '',
        timeout_seconds: float = _DEFAULT_TIMEOUT_SECONDS,
        transport: httpx.AsyncBaseTransport | None = None,
    ) -> None:
        self.base_url = base_url.rstrip('/')
        self.cdn_base_url = cdn_base_url.rstrip('/')
        self.bot_token = bot_token.strip()
        self._timeout_seconds = timeout_seconds
        self._client = httpx.AsyncClient(timeout=timeout_seconds, transport=transport, follow_redirects=True)

    async def aclose(self) -> None:
        await self._client.aclose()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self.aclose()

    def _headers(self) -> dict[str, str]:
        headers = {
            'Content-Type': 'application/json',
            'AuthorizationType': 'ilink_bot_token',
            'X-WECHAT-UIN': random_wechat_uin(),
        }
        if self.bot_token:
            headers['Authorization'] = f'Bearer {self.bot_token}'
        return headers

    async def _post(self, endpoint: str, body: dict[str, Any], *, timeout_seconds: float | None = None) -> dict[str, Any]:
        payload = {**body, 'base_info': {'channel_version': CHANNEL_VERSION}}
        timeout = httpx.Timeout(timeout_seconds if timeout_seconds is not None else self._timeout_seconds)
        response = await self._client.post(f'{self.base_url}/{endpoint}', json=payload, headers=self._headers(), timeout=timeout)
        if response.status_code >= httpx.codes.BAD_REQUEST:
            retryable = response.status_code >= httpx.codes.INTERNAL_SERVER_ERROR or response.status_code == httpx.codes.TOO_MANY_REQUESTS
            msg = f'{endpoint} answered HTTP {response.status_code}: {response.text[:200]}'
            raise ILinkError(msg, retryable=retryable)
        try:
            data = response.json()
        except ValueError as exc:
            msg = f'{endpoint} answered non-JSON: {response.text[:200]}'
            raise ILinkError(msg) from exc
        if not isinstance(data, dict):
            msg = f'{endpoint} answered a non-object payload'
            raise ILinkError(msg)
        return data

    # ---- login -------------------------------------------------------------

    async def get_bot_qrcode(self, *, bot_type: str = DEFAULT_BOT_TYPE) -> QrLogin:
        response = await self._client.get(f'{self.base_url}/ilink/bot/get_bot_qrcode', params={'bot_type': bot_type})
        if response.status_code >= httpx.codes.BAD_REQUEST:
            msg = f'get_bot_qrcode answered HTTP {response.status_code}: {response.text[:200]}'
            raise ILinkError(msg)
        data = response.json()
        qrcode = str(data.get('qrcode') or '')
        qrcode_url = str(data.get('qrcode_img_content') or '')
        if not qrcode or not qrcode_url:
            msg = 'get_bot_qrcode answered without qrcode/qrcode_img_content'
            raise ILinkError(msg)
        return QrLogin(qrcode=qrcode, qrcode_url=qrcode_url)

    async def get_qrcode_status(self, qrcode: str, *, timeout_seconds: float = _DEFAULT_LONG_POLL_TIMEOUT_SECONDS) -> QrLoginStatus:
        """Long-poll the scan state. A client-side timeout reads as ``wait``."""
        try:
            response = await self._client.get(
                f'{self.base_url}/ilink/bot/get_qrcode_status',
                params={'qrcode': qrcode},
                headers={'iLink-App-ClientVersion': '1'},
                timeout=httpx.Timeout(timeout_seconds + _LONG_POLL_GRACE_SECONDS),
            )
        except httpx.TimeoutException:
            return QrLoginStatus(status='wait')
        if response.status_code >= httpx.codes.BAD_REQUEST:
            msg = f'get_qrcode_status answered HTTP {response.status_code}: {response.text[:200]}'
            raise ILinkError(msg)
        data = response.json()
        status = str(data.get('status') or 'wait')
        if status not in ('wait', 'scaned', 'confirmed', 'expired'):
            msg = f'get_qrcode_status answered unknown status {status!r}'
            raise ILinkError(msg)
        return QrLoginStatus(
            status=status,  # type: ignore[arg-type]
            bot_token=str(data.get('bot_token') or ''),
            bot_id=str(data.get('ilink_bot_id') or ''),
            base_url=str(data.get('baseurl') or ''),
            user_id=str(data.get('ilink_user_id') or ''),
        )

    # ---- messages ----------------------------------------------------------

    async def get_updates(self, get_updates_buf: str, *, timeout_seconds: float = _DEFAULT_LONG_POLL_TIMEOUT_SECONDS) -> UpdatesPage:
        """Long-poll for new messages; the server holds the request until there are some.

        A client-side timeout is the normal idle outcome and comes back as an
        empty page carrying the cursor that was sent, so the caller just loops.
        """
        try:
            data = await self._post(
                'ilink/bot/getupdates',
                {'get_updates_buf': get_updates_buf},
                timeout_seconds=timeout_seconds + _LONG_POLL_GRACE_SECONDS,
            )
        except httpx.TimeoutException:
            return UpdatesPage(messages=(), get_updates_buf=get_updates_buf, long_poll_timeout_seconds=None)
        return parse_updates(data)

    async def send_text(self, *, to_user_id: str, context_token: str, text: str) -> None:
        """Reply with plain text. The bot can only speak inside a context the user opened."""
        await self._post(
            'ilink/bot/sendmessage',
            {
                'msg': {
                    'to_user_id': to_user_id,
                    'context_token': context_token,
                    'message_type': 2,
                    'message_state': 2,
                    'item_list': [{'type': ITEM_TYPE_TEXT, 'text_item': {'text': text}}],
                },
            },
        )

    # ---- media -------------------------------------------------------------

    async def stream_media(self, media: CdnMedia) -> AsyncIterator[bytes]:
        """Yield the raw CDN bytes (still encrypted when ``media.aes_key`` is set)."""
        url = build_cdn_download_url(self.cdn_base_url, media.encrypt_query_param)
        async with self._client.stream('GET', url, timeout=httpx.Timeout(self._timeout_seconds, read=120.0)) as response:
            if response.status_code >= httpx.codes.BAD_REQUEST:
                body = (await response.aread())[:200]
                retryable = response.status_code >= httpx.codes.INTERNAL_SERVER_ERROR
                msg = f'CDN download answered HTTP {response.status_code}: {body!r}'
                raise ILinkError(msg, retryable=retryable)
            async for chunk in response.aiter_bytes():
                yield chunk

    async def download_media(self, media: CdnMedia, destination: Path, *, scratch: Path) -> int:
        """Download ``media`` to ``destination``, decrypting through ``scratch`` when keyed.

        Returns the plaintext size. ``scratch`` holds the ciphertext for keyed
        media and is removed afterwards; plain media streams straight to
        ``destination``.
        """
        key = parse_aes_key(media.aes_key) if media.aes_key else None
        target = scratch if key is not None else destination
        await asyncio.to_thread(target.parent.mkdir, parents=True, exist_ok=True)
        handle = await asyncio.to_thread(target.open, 'wb')
        try:
            async for chunk in self.stream_media(media):
                handle.write(chunk)
        finally:
            handle.close()
        if key is None:
            return (await asyncio.to_thread(destination.stat)).st_size
        try:
            await asyncio.to_thread(destination.parent.mkdir, parents=True, exist_ok=True)
            return await asyncio.to_thread(decrypt_file_aes_ecb, scratch, destination, key)
        finally:
            await asyncio.to_thread(scratch.unlink, missing_ok=True)
