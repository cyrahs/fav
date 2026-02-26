from __future__ import annotations

from pathlib import Path
from typing import Protocol

import httpx

from src.core import config, logger

log = logger.get('notifier')
_MAX_MESSAGE_LENGTH = 4096
_MAX_CAPTION_LENGTH = 1024
_TRUNCATE_SUFFIX = '\n...'


class Notifier(Protocol):
    async def send(self, message: str) -> None:
        """Send a notification message."""

    async def aclose(self) -> None:
        """Release notifier resources."""


class TelegramBotNotifier:
    def __init__(  # noqa: PLR0913
        self,
        *,
        token: str,
        chat_id: int | str,
        api_base: str = 'https://api.telegram.org',
        disable_notification: bool = False,
        message_thread_id: int | None = None,
        timeout: float = 20.0,
        proxy: str | None = None,
    ) -> None:
        base = f'{api_base.rstrip("/")}/bot{token}'
        self._send_message_url = f'{base}/sendMessage'
        self._send_photo_url = f'{base}/sendPhoto'
        self._chat_id = str(chat_id)
        self._disable_notification = disable_notification
        self._message_thread_id = message_thread_id
        self._client = httpx.AsyncClient(timeout=timeout, proxy=proxy)

    @staticmethod
    def _fit_text(message: str, limit: int) -> str:
        if len(message) <= limit:
            return message
        head = limit - len(_TRUNCATE_SUFFIX)
        return f'{message[:head]}{_TRUNCATE_SUFFIX}'

    @staticmethod
    def _fit_message(message: str) -> str:
        return TelegramBotNotifier._fit_text(message, _MAX_MESSAGE_LENGTH)

    @staticmethod
    def _fit_caption(caption: str) -> str:
        return TelegramBotNotifier._fit_text(caption, _MAX_CAPTION_LENGTH)

    def _base_payload(self) -> dict[str, int | str | bool]:
        payload: dict[str, int | str | bool] = {
            'chat_id': self._chat_id,
            'disable_notification': self._disable_notification,
        }
        if self._message_thread_id is not None:
            payload['message_thread_id'] = self._message_thread_id
        return payload

    @staticmethod
    def _as_form_payload(payload: dict[str, int | str | bool]) -> dict[str, str]:
        form_data: dict[str, str] = {}
        for key, value in payload.items():
            if isinstance(value, bool):
                form_data[key] = 'true' if value else 'false'
            else:
                form_data[key] = str(value)
        return form_data

    @staticmethod
    def _raise_for_telegram_error(response: httpx.Response) -> None:
        response.raise_for_status()
        data = response.json()
        if not data.get('ok'):
            description = data.get('description', 'unknown Telegram API error')
            msg = f'Telegram API returned not-ok response: {description}'
            raise RuntimeError(msg)

    async def send(self, message: str) -> None:
        text = self._fit_message(message)
        payload = self._base_payload()
        payload['text'] = text
        response = await self._client.post(self._send_message_url, json=payload)
        self._raise_for_telegram_error(response)

    async def send_markdown(self, message: str, *, disable_web_page_preview: bool = False) -> None:
        text = self._fit_message(message)
        payload = self._base_payload()
        payload['text'] = text
        payload['parse_mode'] = 'Markdown'
        if disable_web_page_preview:
            payload['disable_web_page_preview'] = True
        response = await self._client.post(self._send_message_url, json=payload)
        self._raise_for_telegram_error(response)

    async def send_photo(self, *, photo: str | Path, caption: str | None = None, parse_mode: str | None = None) -> None:
        payload = self._base_payload()
        if caption:
            payload['caption'] = self._fit_caption(caption)
        if parse_mode:
            payload['parse_mode'] = parse_mode

        if isinstance(photo, Path):
            with photo.open('rb') as image_file:
                files = {'photo': (photo.name, image_file)}
                response = await self._client.post(self._send_photo_url, data=self._as_form_payload(payload), files=files)
        else:
            payload['photo'] = photo
            response = await self._client.post(self._send_photo_url, json=payload)

        self._raise_for_telegram_error(response)

    async def aclose(self) -> None:
        await self._client.aclose()


def build_notifier() -> Notifier:
    tg_cfg = config.telegram_bot
    log.debug('Telegram bot notifier enabled')
    return TelegramBotNotifier(
        token=tg_cfg.token,
        chat_id=tg_cfg.chat_id,
        api_base=tg_cfg.api_base,
        message_thread_id=tg_cfg.message_thread_id,
        proxy=config.proxy or None,
    )
