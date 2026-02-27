from __future__ import annotations

import asyncio
import json
from typing import TYPE_CHECKING, Any

import httpx

from src.core import logger

if TYPE_CHECKING:
    from collections.abc import Awaitable, Callable
    from pathlib import Path

log = logger.get('runtime-config-bot')

_LONG_POLL_TIMEOUT = 30
_REQUEST_TIMEOUT = 40
_RETRY_DELAY_SECONDS = 3
_CALLBACK_CONFIG_PREFIX = 'config:'
_CALLBACK_CONFIG_HANIME1 = 'config:hanime1'
_CALLBACK_ADD = 'hanime1:add'
_CALLBACK_DELETE = 'hanime1:delete'
_CALLBACK_TRIGGER_PREFIX = 'trigger:'
_STATE_WAIT_ADD = 'wait_add'
_STATE_WAIT_DELETE = 'wait_delete'
_COMMAND_CONFIG = {'command': 'config', 'description': 'Manage runtime config'}
_COMMAND_TRIGGER = {'command': 'trigger', 'description': 'Trigger web jobs now'}
_COMMAND_CANCEL = {'command': 'cancel', 'description': 'Cancel current action'}
_TRIGGER_KEYBOARD_COLUMNS = 2


class TelegramRuntimeConfigBot:
    def __init__(  # noqa: PLR0913
        self,
        *,
        token: str,
        chat_id: int | str,
        run_config: Path,
        api_base: str = 'https://api.telegram.org',
        message_thread_id: int | None = None,
        trigger_targets: list[tuple[str, str]] | None = None,
        trigger_callback: Callable[[str], Awaitable[str]] | None = None,
        proxy: str | None = None,
    ) -> None:
        base = f'{api_base.rstrip("/")}/bot{token}'
        self._get_updates_url = f'{base}/getUpdates'
        self._set_my_commands_url = f'{base}/setMyCommands'
        self._send_message_url = f'{base}/sendMessage'
        self._answer_callback_query_url = f'{base}/answerCallbackQuery'
        self._allowed_chat_id = str(chat_id)
        self._allowed_message_thread_id = message_thread_id
        self._run_config = run_config
        self._client = httpx.AsyncClient(timeout=_REQUEST_TIMEOUT, proxy=proxy)
        self._offset = 0
        self._states: dict[str, str] = {}
        self._closed = False
        self._commands_registered = False
        self._trigger_targets = trigger_targets or []
        self._trigger_callback = trigger_callback

    @staticmethod
    def _normalize_keywords(raw_keywords: list[Any]) -> list[str]:
        keywords: list[str] = []
        seen: set[str] = set()
        for raw in raw_keywords:
            if not isinstance(raw, str):
                continue
            keyword = raw.strip()
            if not keyword:
                continue
            key = keyword.casefold()
            if key in seen:
                continue
            seen.add(key)
            keywords.append(keyword)
        return keywords

    def _build_bot_commands(self) -> list[dict[str, str]]:
        return [_COMMAND_CONFIG, _COMMAND_TRIGGER, _COMMAND_CANCEL]

    def _read_runtime_config(self) -> dict[str, Any]:
        if not self._run_config.exists():
            return {}
        try:
            payload = json.loads(self._run_config.read_text(encoding='utf-8'))
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to read runtime config %s: %s', self._run_config, exc)
            return {}
        if not isinstance(payload, dict):
            return {}
        return payload

    def _write_runtime_config(self, payload: dict[str, Any]) -> None:
        self._run_config.parent.mkdir(parents=True, exist_ok=True)
        self._run_config.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2),
            encoding='utf-8',
        )

    def _read_hanime1_keywords(self) -> list[str]:
        payload = self._read_runtime_config()
        hanime1 = payload.get('hanime1')
        if not isinstance(hanime1, dict):
            return []
        raw_keywords = hanime1.get('keywords')
        if not isinstance(raw_keywords, list):
            return []
        return self._normalize_keywords(raw_keywords)

    def _write_hanime1_keywords(self, keywords: list[str]) -> None:
        payload = self._read_runtime_config()
        hanime1 = payload.get('hanime1')
        if not isinstance(hanime1, dict):
            hanime1 = {}
            payload['hanime1'] = hanime1
        hanime1['keywords'] = keywords
        self._write_runtime_config(payload)

    @staticmethod
    def _render_hanime1_keywords(keywords: list[str]) -> str:
        lines = ['Hanime1 keywords:']
        if not keywords:
            lines.append('(empty)')
            return '\n'.join(lines)
        lines.extend(f'{idx}. {keyword}' for idx, keyword in enumerate(keywords, start=1))
        return '\n'.join(lines)

    @staticmethod
    def _hanime1_keyboard() -> dict[str, list[list[dict[str, str]]]]:
        return {
            'inline_keyboard': [
                [
                    {'text': 'Add keyword', 'callback_data': _CALLBACK_ADD},
                    {'text': 'Delete keyword', 'callback_data': _CALLBACK_DELETE},
                ],
            ],
        }

    @staticmethod
    def _config_keyboard() -> dict[str, list[list[dict[str, str]]]]:
        return {'inline_keyboard': [[{'text': 'Hanime1 keywords', 'callback_data': _CALLBACK_CONFIG_HANIME1}]]}

    def _trigger_keyboard(self) -> dict[str, list[list[dict[str, str]]]] | None:
        if not self._trigger_targets:
            return None

        rows: list[list[dict[str, str]]] = []
        row: list[dict[str, str]] = []
        for key, label in self._trigger_targets:
            row.append({'text': label, 'callback_data': f'{_CALLBACK_TRIGGER_PREFIX}{key}'})
            if len(row) == _TRIGGER_KEYBOARD_COLUMNS:
                rows.append(row)
                row = []
        if row:
            rows.append(row)
        rows.append([{'text': 'All', 'callback_data': f'{_CALLBACK_TRIGGER_PREFIX}all'}])
        return {'inline_keyboard': rows}

    def _state_key(self, *, chat_id: str, message_thread_id: int | None) -> str:
        return f'{chat_id}:{message_thread_id or 0}'

    def _is_allowed_context(self, *, chat_id: str, message_thread_id: int | None) -> bool:
        if chat_id != self._allowed_chat_id:
            return False
        if self._allowed_message_thread_id is None:
            return True
        return self._allowed_message_thread_id == message_thread_id

    async def _send_message(
        self,
        *,
        chat_id: str,
        text: str,
        message_thread_id: int | None,
        reply_markup: dict[str, Any] | None = None,
    ) -> None:
        payload: dict[str, Any] = {
            'chat_id': chat_id,
            'text': text,
        }
        if message_thread_id is not None:
            payload['message_thread_id'] = message_thread_id
        if reply_markup is not None:
            payload['reply_markup'] = reply_markup

        response = await self._client.post(self._send_message_url, json=payload)
        response.raise_for_status()
        data = response.json()
        if not data.get('ok'):
            msg = f'Failed to send Telegram message: {data.get("description", "unknown")}'
            raise RuntimeError(msg)

    async def _answer_callback_query(self, callback_query_id: str, *, text: str | None = None) -> None:
        payload: dict[str, Any] = {'callback_query_id': callback_query_id}
        if text:
            payload['text'] = text
        response = await self._client.post(self._answer_callback_query_url, json=payload)
        response.raise_for_status()
        data = response.json()
        if not data.get('ok'):
            msg = f'Failed to answer callback query: {data.get("description", "unknown")}'
            raise RuntimeError(msg)

    async def _set_my_commands(self) -> None:
        scopes: tuple[dict[str, Any] | None, ...] = (
            None,
            {'type': 'chat', 'chat_id': self._allowed_chat_id},
        )
        commands = self._build_bot_commands()
        for scope in scopes:
            payload: dict[str, Any] = {'commands': commands}
            if scope is not None:
                payload['scope'] = scope
            response = await self._client.post(self._set_my_commands_url, json=payload)
            response.raise_for_status()
            data = response.json()
            if not data.get('ok'):
                msg = f'Failed to set bot commands: {data.get("description", "unknown")}'
                raise RuntimeError(msg)

    async def _send_config_panel(self, *, chat_id: str, message_thread_id: int | None) -> None:
        await self._send_message(
            chat_id=chat_id,
            message_thread_id=message_thread_id,
            text='Choose a config option:',
            reply_markup=self._config_keyboard(),
        )

    async def _send_trigger_panel(self, *, chat_id: str, message_thread_id: int | None) -> None:
        keyboard = self._trigger_keyboard()
        if keyboard is None:
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text='No trigger targets available.',
            )
            return
        await self._send_message(
            chat_id=chat_id,
            message_thread_id=message_thread_id,
            text='Choose a job to trigger now:',
            reply_markup=keyboard,
        )

    async def _send_hanime1_panel(self, *, chat_id: str, message_thread_id: int | None) -> None:
        keywords = self._read_hanime1_keywords()
        await self._send_message(
            chat_id=chat_id,
            text=self._render_hanime1_keywords(keywords),
            message_thread_id=message_thread_id,
            reply_markup=self._hanime1_keyboard(),
        )

    async def _handle_wait_add(
        self,
        *,
        state_key: str,
        chat_id: str,
        message_thread_id: int | None,
        text: str,
    ) -> None:
        keyword = text.strip()
        if not keyword:
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text='Keyword cannot be empty. Send a keyword or /cancel.',
            )
            return

        keywords = self._read_hanime1_keywords()
        if keyword.casefold() in {item.casefold() for item in keywords}:
            self._states.pop(state_key, None)
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text=f'Keyword already exists: {keyword}',
            )
            await self._send_hanime1_panel(chat_id=chat_id, message_thread_id=message_thread_id)
            return

        keywords.append(keyword)
        self._write_hanime1_keywords(keywords)
        self._states.pop(state_key, None)
        await self._send_message(
            chat_id=chat_id,
            message_thread_id=message_thread_id,
            text=f'Added keyword: {keyword}',
        )
        await self._send_hanime1_panel(chat_id=chat_id, message_thread_id=message_thread_id)

    async def _handle_wait_delete(
        self,
        *,
        state_key: str,
        chat_id: str,
        message_thread_id: int | None,
        text: str,
    ) -> None:
        keywords = self._read_hanime1_keywords()
        if not keywords:
            self._states.pop(state_key, None)
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text='No keywords to delete.',
            )
            await self._send_hanime1_panel(chat_id=chat_id, message_thread_id=message_thread_id)
            return

        try:
            idx = int(text.strip())
        except ValueError:
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text='Please send a valid number.',
            )
            return

        if idx < 1 or idx > len(keywords):
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text=f'Number out of range. Please send 1-{len(keywords)}.',
            )
            return

        removed = keywords.pop(idx - 1)
        self._write_hanime1_keywords(keywords)
        self._states.pop(state_key, None)
        await self._send_message(
            chat_id=chat_id,
            message_thread_id=message_thread_id,
            text=f'Deleted keyword: {removed}',
        )
        await self._send_hanime1_panel(chat_id=chat_id, message_thread_id=message_thread_id)

    async def _handle_message(self, message: dict[str, Any]) -> None:
        text = message.get('text')
        if not isinstance(text, str):
            return
        chat = message.get('chat')
        if not isinstance(chat, dict):
            return
        chat_id = str(chat.get('id', ''))
        message_thread_id = message.get('message_thread_id')
        if not self._is_allowed_context(chat_id=chat_id, message_thread_id=message_thread_id):
            return

        state_key = self._state_key(chat_id=chat_id, message_thread_id=message_thread_id)
        command = text.strip()
        command_token = command.split(maxsplit=1)[0]
        if command_token.startswith('/config'):
            self._states.pop(state_key, None)
            await self._send_config_panel(chat_id=chat_id, message_thread_id=message_thread_id)
        elif command_token.startswith('/trigger'):
            self._states.pop(state_key, None)
            await self._send_trigger_panel(chat_id=chat_id, message_thread_id=message_thread_id)
        elif command_token.startswith('/cancel'):
            self._states.pop(state_key, None)
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text='Cancelled.',
            )
        else:
            await self._handle_waiting_state(
                state_key=state_key,
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                command=command,
            )

    async def _handle_trigger_callback(
        self,
        *,
        chat_id: str,
        message_thread_id: int | None,
        callback_query_id: str,
        data: str,
    ) -> None:
        await self._answer_callback_query(callback_query_id)
        if self._trigger_callback is None:
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text='Trigger callback is unavailable.',
            )
            return

        target = data.removeprefix(_CALLBACK_TRIGGER_PREFIX)
        try:
            result_text = await self._trigger_callback(target)
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to trigger %r: %s', target, exc)
            result_text = f'Trigger failed for {target}: {exc}'

        await self._send_message(
            chat_id=chat_id,
            message_thread_id=message_thread_id,
            text=result_text,
        )

    async def _handle_config_callback(
        self,
        *,
        chat_id: str,
        message_thread_id: int | None,
        callback_query_id: str,
        data: str,
    ) -> None:
        await self._answer_callback_query(callback_query_id)
        option = data.removeprefix(_CALLBACK_CONFIG_PREFIX)
        if option == 'hanime1':
            await self._send_hanime1_panel(chat_id=chat_id, message_thread_id=message_thread_id)
        else:
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text=f'Unknown config option: {option}',
            )

    async def _handle_hanime1_callback(
        self,
        *,
        state_key: str,
        chat_id: str,
        message_thread_id: int | None,
        callback_query_id: str,
        data: str,
    ) -> None:
        if data == _CALLBACK_ADD:
            self._states[state_key] = _STATE_WAIT_ADD
            await self._answer_callback_query(callback_query_id)
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text='Send the keyword to add. Send /cancel to abort.',
            )
            return

        if data != _CALLBACK_DELETE:
            await self._answer_callback_query(callback_query_id)
            return

        keywords = self._read_hanime1_keywords()
        if not keywords:
            self._states.pop(state_key, None)
            await self._answer_callback_query(callback_query_id)
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text='No keywords to delete.',
            )
            return

        self._states[state_key] = _STATE_WAIT_DELETE
        await self._answer_callback_query(callback_query_id)
        await self._send_message(
            chat_id=chat_id,
            message_thread_id=message_thread_id,
            text=f'{self._render_hanime1_keywords(keywords)}\nSend the number to delete.',
        )

    async def _handle_waiting_state(
        self,
        *,
        state_key: str,
        chat_id: str,
        message_thread_id: int | None,
        command: str,
    ) -> None:
        state = self._states.get(state_key)
        if state == _STATE_WAIT_ADD:
            await self._handle_wait_add(
                state_key=state_key,
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text=command,
            )
        elif state == _STATE_WAIT_DELETE:
            await self._handle_wait_delete(
                state_key=state_key,
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text=command,
            )

    async def _handle_callback_query(self, callback_query: dict[str, Any]) -> None:
        callback_query_id = str(callback_query.get('id', ''))
        data = callback_query.get('data')
        message = callback_query.get('message')
        if not callback_query_id or not isinstance(data, str):
            return
        if not isinstance(message, dict):
            await self._answer_callback_query(callback_query_id)
        else:
            chat = message.get('chat')
            if not isinstance(chat, dict):
                await self._answer_callback_query(callback_query_id)
            else:
                chat_id = str(chat.get('id', ''))
                message_thread_id = message.get('message_thread_id')
                if not self._is_allowed_context(chat_id=chat_id, message_thread_id=message_thread_id):
                    await self._answer_callback_query(callback_query_id, text='Not allowed in this chat.')
                else:
                    state_key = self._state_key(chat_id=chat_id, message_thread_id=message_thread_id)
                    if data.startswith(_CALLBACK_CONFIG_PREFIX):
                        await self._handle_config_callback(
                            chat_id=chat_id,
                            message_thread_id=message_thread_id,
                            callback_query_id=callback_query_id,
                            data=data,
                        )
                    elif data.startswith(_CALLBACK_TRIGGER_PREFIX):
                        await self._handle_trigger_callback(
                            chat_id=chat_id,
                            message_thread_id=message_thread_id,
                            callback_query_id=callback_query_id,
                            data=data,
                        )
                    elif data in {_CALLBACK_ADD, _CALLBACK_DELETE}:
                        await self._handle_hanime1_callback(
                            state_key=state_key,
                            chat_id=chat_id,
                            message_thread_id=message_thread_id,
                            callback_query_id=callback_query_id,
                            data=data,
                        )
                    else:
                        await self._answer_callback_query(callback_query_id)

    async def _handle_update(self, update: dict[str, Any]) -> None:
        message = update.get('message')
        if isinstance(message, dict):
            await self._handle_message(message)
            return
        callback_query = update.get('callback_query')
        if isinstance(callback_query, dict):
            await self._handle_callback_query(callback_query)

    async def _fetch_updates(self) -> list[dict[str, Any]]:
        params = {
            'timeout': str(_LONG_POLL_TIMEOUT),
            'offset': str(self._offset),
            'allowed_updates': json.dumps(['message', 'callback_query']),
        }
        response = await self._client.get(self._get_updates_url, params=params)
        response.raise_for_status()
        data = response.json()
        if not data.get('ok'):
            msg = f'Failed to fetch updates: {data.get("description", "unknown")}'
            raise RuntimeError(msg)
        result = data.get('result')
        if not isinstance(result, list):
            return []
        return [item for item in result if isinstance(item, dict)]

    async def run(self) -> None:
        while not self._closed:
            try:
                if not self._commands_registered:
                    await self._set_my_commands()
                    self._commands_registered = True
                updates = await self._fetch_updates()
                for update in updates:
                    update_id = update.get('update_id')
                    if isinstance(update_id, int):
                        self._offset = max(self._offset, update_id + 1)
                    await self._handle_update(update)
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001
                log.warning('Telegram runtime config bot poll failed: %s', exc)
                if not self._closed:
                    await asyncio.sleep(_RETRY_DELAY_SECONDS)

    async def aclose(self) -> None:
        self._closed = True
        await self._client.aclose()
