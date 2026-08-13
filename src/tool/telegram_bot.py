"""Direct notification delivery through the Telegram Bot API."""

from __future__ import annotations

import asyncio
import mimetypes
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

import httpx

from src.core import logger, settings

if TYPE_CHECKING:
    from pathlib import Path

log = logger.get('telegram-bot')

API_BASE_URL = 'https://api.telegram.org'
MAX_IMAGE_BYTES = 9_500_000
MAX_CAPTION_LENGTH = 1024
MAX_MESSAGE_LENGTH = 4096
MARKDOWN_V2_SPECIAL_CHARS = frozenset('\\_*[]()~`>#+-=|{}.!')

CONNECT_TIMEOUT_SECONDS = 5.0
WRITE_TIMEOUT_SECONDS = 30.0
READ_TIMEOUT_SECONDS = 90.0

_RETRYABLE_STATUS_CODES = frozenset({408, 409, 425, 429})
_SERVER_ERROR_MIN = 500


def escape_markdown_v2(value: str) -> str:
    """Escape text for the parse mode every send here uses.

    Lives beside the senders because forgetting it is not a formatting bug: Telegram
    rejects the whole message with a 400, so a single unescaped '.' loses whatever
    was being sent.
    """
    escaped: list[str] = []
    for char in value:
        if char in MARKDOWN_V2_SPECIAL_CHARS:
            escaped.append('\\')
        escaped.append(char)
    return ''.join(escaped)


@dataclass(frozen=True, slots=True)
class TelegramBotConfig:
    bot_token: str
    chat_id: str
    message_thread_id: int | None = None


@dataclass(frozen=True, slots=True)
class TelegramDeliveryResult:
    message_id: int | None
    media_status: str
    warnings: tuple[str, ...] = ()


class TelegramDeliveryError(RuntimeError):
    def __init__(self, message: str, *, retryable: bool, retry_after_seconds: int | None = None) -> None:
        super().__init__(message)
        self.retryable = retryable
        self.retry_after_seconds = retry_after_seconds


class TelegramNotConfiguredError(TelegramDeliveryError):
    def __init__(self) -> None:
        super().__init__('Telegram bot_token and chat_id are required', retryable=False)


def load_config(*, require_enabled: bool = True) -> TelegramBotConfig | None:
    cfg = settings.load().notifications.telegram
    if require_enabled and not cfg.enabled:
        return None
    if not cfg.configured:
        return None
    return TelegramBotConfig(bot_token=cfg.bot_token, chat_id=cfg.chat_id, message_thread_id=cfg.message_thread_id)


def build_client() -> httpx.AsyncClient:
    return httpx.AsyncClient(
        timeout=httpx.Timeout(
            connect=CONNECT_TIMEOUT_SECONDS,
            write=WRITE_TIMEOUT_SECONDS,
            read=READ_TIMEOUT_SECONDS,
            pool=CONNECT_TIMEOUT_SECONDS,
        ),
    )


def _method_url(config: TelegramBotConfig, method: str) -> str:
    return f'{API_BASE_URL}/bot{config.bot_token}/{method}'


def _common_payload(config: TelegramBotConfig) -> dict[str, object]:
    payload: dict[str, object] = {'chat_id': config.chat_id}
    if config.message_thread_id is not None:
        payload['message_thread_id'] = config.message_thread_id
    return payload


def _retryable_status(status_code: int) -> bool:
    return status_code in _RETRYABLE_STATUS_CODES or status_code >= _SERVER_ERROR_MIN


def _response_error(response: httpx.Response, payload: object, *, token: str) -> TelegramDeliveryError:
    error_code = response.status_code
    description = ''
    retry_after: int | None = None
    if isinstance(payload, dict):
        raw_error_code = payload.get('error_code')
        if isinstance(raw_error_code, int):
            error_code = raw_error_code
        raw_description = payload.get('description')
        if isinstance(raw_description, str):
            description = raw_description.replace(token, '[redacted]')
        parameters = payload.get('parameters')
        if isinstance(parameters, dict) and isinstance(parameters.get('retry_after'), int):
            retry_after = parameters['retry_after']
    detail = f': {description[:200]}' if description else ''
    return TelegramDeliveryError(
        f'Telegram Bot API responded with error {error_code}{detail}',
        retryable=_retryable_status(error_code),
        retry_after_seconds=retry_after,
    )


async def _call_api(  # noqa: PLR0913
    *,
    client: httpx.AsyncClient,
    config: TelegramBotConfig,
    method: str,
    json_payload: dict[str, object] | None = None,
    data: dict[str, object] | None = None,
    files: dict[str, tuple[str, bytes, str]] | None = None,
) -> dict[str, Any]:
    try:
        response = await client.post(
            _method_url(config, method),
            json=json_payload,
            data=data,
            files=files,
        )
    except (httpx.RequestError, httpx.TimeoutException) as exc:
        message = f'Telegram Bot API request failed: {exc.__class__.__name__}'
        raise TelegramDeliveryError(message, retryable=True) from exc

    try:
        payload = response.json()
    except ValueError:
        payload = None
    if response.is_success and isinstance(payload, dict) and payload.get('ok') is True:
        result = payload.get('result')
        return result if isinstance(result, dict) else {}
    raise _response_error(response, payload, token=config.bot_token)


def _message_id(result: dict[str, Any]) -> int | None:
    value = result.get('message_id')
    return value if isinstance(value, int) else None


async def _send_message(
    *,
    client: httpx.AsyncClient,
    config: TelegramBotConfig,
    markdown: str,
    disable_notification: bool,
    disable_web_page_preview: bool,
) -> int | None:
    text = markdown.strip() or 'Empty notification'
    parse_mode: str | None = 'MarkdownV2'
    if len(text) > MAX_MESSAGE_LENGTH:
        plain_text: list[str] = []
        index = 0
        while index < len(text):
            if text[index] == '\\' and index + 1 < len(text) and text[index + 1] in MARKDOWN_V2_SPECIAL_CHARS:
                index += 1
            elif text[index] == '*':
                index += 1
                continue
            plain_text.append(text[index])
            index += 1
        text = ''.join(plain_text)
        text = f'{text[: MAX_MESSAGE_LENGTH - 1]}…'
        parse_mode = None
    payload = {
        **_common_payload(config),
        'text': text,
        'disable_notification': disable_notification,
        'link_preview_options': {'is_disabled': disable_web_page_preview},
    }
    if parse_mode is not None:
        payload['parse_mode'] = parse_mode
    return _message_id(await _call_api(client=client, config=config, method='sendMessage', json_payload=payload))


def _read_bounded_image(image_path: Path) -> tuple[str, bytes, str] | None:
    content_type = mimetypes.guess_type(image_path.name)[0] or ''
    if not content_type.startswith('image/'):
        return None
    try:
        with image_path.open('rb') as handle:
            image_bytes = handle.read(MAX_IMAGE_BYTES + 1)
    except OSError:
        return None
    if len(image_bytes) > MAX_IMAGE_BYTES:
        return None
    return image_path.name, image_bytes, content_type


async def _send_photo(
    *,
    client: httpx.AsyncClient,
    config: TelegramBotConfig,
    photo: str | tuple[str, bytes, str],
    caption: str,
    disable_notification: bool,
) -> int | None:
    common = _common_payload(config)
    include_caption = bool(caption) and len(caption) <= MAX_CAPTION_LENGTH
    if isinstance(photo, str):
        payload = {
            **common,
            'photo': photo,
            'disable_notification': disable_notification,
        }
        if include_caption:
            payload.update({'caption': caption, 'parse_mode': 'MarkdownV2'})
        result = await _call_api(client=client, config=config, method='sendPhoto', json_payload=payload)
    else:
        filename, image_bytes, content_type = photo
        data = {key: str(value) for key, value in common.items()}
        data['disable_notification'] = str(disable_notification).lower()
        if include_caption:
            data.update({'caption': caption, 'parse_mode': 'MarkdownV2'})
        result = await _call_api(
            client=client,
            config=config,
            method='sendPhoto',
            data=data,
            files={'photo': (filename, image_bytes, content_type)},
        )
    return _message_id(result)


def _can_fallback_from_photo(exc: TelegramDeliveryError) -> bool:
    return not exc.retryable and ('error 400' in str(exc) or 'error 413' in str(exc) or 'error 415' in str(exc))


async def _pin_message(*, client: httpx.AsyncClient, config: TelegramBotConfig, message_id: int) -> None:
    payload = {**_common_payload(config), 'message_id': message_id, 'disable_notification': True}
    await _call_api(client=client, config=config, method='pinChatMessage', json_payload=payload)


async def deliver(
    *,
    notification,  # noqa: ANN001 - NotificationRecord is kept out to avoid coupling the adapter to storage
    client: httpx.AsyncClient,
    config: TelegramBotConfig,
) -> TelegramDeliveryResult:
    """Deliver one outbox record, preferring a local image and then its remote URL."""
    media_status = 'none'
    message_id: int | None = None
    caption_fits = len(notification.markdown) <= MAX_CAPTION_LENGTH
    candidates: list[tuple[str, str | tuple[str, bytes, str]]] = []

    if notification.local_image_path is not None:
        local_image = await asyncio.to_thread(_read_bounded_image, notification.local_image_path)
        if local_image is not None:
            candidates.append(('local', local_image))
    if notification.image_url.strip():
        candidates.append(('remote', notification.image_url.strip()))

    for candidate_status, photo in candidates:
        try:
            message_id = await _send_photo(
                client=client,
                config=config,
                photo=photo,
                caption=notification.markdown,
                disable_notification=notification.disable_notification,
            )
        except TelegramDeliveryError as exc:
            if not _can_fallback_from_photo(exc):
                raise
            log.warning('Telegram rejected %s image for notification %s; trying fallback', candidate_status, notification.notification_id)
            continue
        media_status = candidate_status
        break

    if media_status == 'none' or not caption_fits:
        message_id = await _send_message(
            client=client,
            config=config,
            markdown=notification.markdown,
            disable_notification=notification.disable_notification,
            disable_web_page_preview=notification.disable_web_page_preview,
        )

    warnings: list[str] = []
    if notification.pin and message_id is not None:
        try:
            await _pin_message(client=client, config=config, message_id=message_id)
        except TelegramDeliveryError as exc:
            warning = f'Pinning failed: {exc}'
            warnings.append(warning)
            log.warning('Telegram notification %s was delivered but %s', notification.notification_id, warning)

    return TelegramDeliveryResult(message_id=message_id, media_status=media_status, warnings=tuple(warnings))


async def send_text_now(*, text: str, require_enabled: bool = True) -> int | None:
    """Send one message immediately, outside the notification outbox.

    The outbox is the right place for anything that can wait. This is for the one
    thing that cannot: a login prompt that has to reach the user *during* the run
    that is blocked on it. Queued notifications are only flushed after ``update()``
    returns, which for a login wait is far too late to be useful.
    """
    config = load_config(require_enabled=require_enabled)
    if config is None:
        raise TelegramNotConfiguredError
    client = build_client()
    try:
        return await _send_message(
            client=client,
            config=config,
            markdown=escape_markdown_v2(text),
            disable_notification=False,
            disable_web_page_preview=True,
        )
    finally:
        await client.aclose()


async def send_photo_now(*, photo: tuple[str, bytes, str], caption: str = '', require_enabled: bool = True) -> int | None:
    """Send one image immediately, outside the outbox. See ``send_text_now``."""
    config = load_config(require_enabled=require_enabled)
    if config is None:
        raise TelegramNotConfiguredError
    client = build_client()
    try:
        return await _send_photo(
            client=client,
            config=config,
            photo=photo,
            caption=escape_markdown_v2(caption),
            disable_notification=False,
        )
    finally:
        await client.aclose()


async def send_test_notification() -> TelegramDeliveryResult:
    config = load_config(require_enabled=False)
    if config is None:
        raise TelegramNotConfiguredError
    client = build_client()
    try:
        message_id = await _send_message(
            client=client,
            config=config,
            markdown='fav Telegram notification test',
            disable_notification=False,
            disable_web_page_preview=True,
        )
    finally:
        await client.aclose()
    return TelegramDeliveryResult(message_id=message_id, media_status='none')
