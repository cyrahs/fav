from __future__ import annotations

from typing import Protocol

import httpx

from src.core import config, logger

log = logger.get('notifier')
_MAX_MESSAGE_LENGTH = 4096
_TRUNCATE_SUFFIX = '\n...'


class Notifier(Protocol):
    async def send(self, message: str) -> None:
        """Send a notification message."""

    async def aclose(self) -> None:
        """Release notifier resources."""


class NullNotifier:
    async def send(self, message: str) -> None:  # noqa: ARG002
        return

    async def aclose(self) -> None:
        return


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
        self._url = f'{api_base.rstrip("/")}/bot{token}/sendMessage'
        self._chat_id = str(chat_id)
        self._disable_notification = disable_notification
        self._message_thread_id = message_thread_id
        self._client = httpx.AsyncClient(timeout=timeout, proxy=proxy)

    @staticmethod
    def _fit_message(message: str) -> str:
        if len(message) <= _MAX_MESSAGE_LENGTH:
            return message
        head = _MAX_MESSAGE_LENGTH - len(_TRUNCATE_SUFFIX)
        return f'{message[:head]}{_TRUNCATE_SUFFIX}'

    async def send(self, message: str) -> None:
        text = self._fit_message(message)
        payload: dict[str, int | str | bool] = {
            'chat_id': self._chat_id,
            'text': text,
            'disable_notification': self._disable_notification,
        }
        if self._message_thread_id is not None:
            payload['message_thread_id'] = self._message_thread_id

        response = await self._client.post(self._url, json=payload)
        response.raise_for_status()

        data = response.json()
        if not data.get('ok'):
            description = data.get('description', 'unknown Telegram API error')
            msg = f'Telegram API returned not-ok response: {description}'
            raise RuntimeError(msg)

    async def aclose(self) -> None:
        await self._client.aclose()


def build_notifier() -> Notifier:
    notification_cfg = config.notification
    if not notification_cfg.enabled:
        return NullNotifier()

    tg_cfg = notification_cfg.telegram_bot
    if tg_cfg is None:
        log.warning('notification.enabled is true but notification.telegram_bot is missing; notifications are disabled')
        return NullNotifier()

    log.debug('Telegram bot notifier enabled')
    return TelegramBotNotifier(
        token=tg_cfg.token,
        chat_id=tg_cfg.chat_id,
        api_base=tg_cfg.api_base,
        disable_notification=tg_cfg.disable_notification,
        message_thread_id=tg_cfg.message_thread_id,
        proxy=config.proxy or None,
    )
