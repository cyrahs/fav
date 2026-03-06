from __future__ import annotations

import asyncio
import html
import json
import re
from typing import TYPE_CHECKING, Any
from urllib.parse import parse_qs, urlsplit

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
_HANIME1_SEED_RE = re.compile(r'^(?:(?P<title>.*?)\s*)?\{id-(?P<id>\d+)\}\s*$', re.IGNORECASE)
_HANIME1_PLAYLIST_TITLE_RE = re.compile(
    r'<div[^>]+id=["\']video-playlist-wrapper["\'][^>]*>.*?<h4[^>]*>(?P<title>.*?)</h4>',
    re.IGNORECASE | re.DOTALL,
)
_HANIME1_WATCH_HREF_RE = re.compile(
    r'href=["\'](?P<url>(?:https?:)?//[^"\']+/watch\?v=[^"\'>\s]+|/watch\?v=[^"\'>\s]+|watch\?v=[^"\'>\s]+)["\']',
    re.IGNORECASE,
)
_HANIME1_CF_BLOCK_MARKERS = ('Attention Required! | Cloudflare', 'cf-error-details')
_HANIME1_USER_AGENT = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36'


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
        hanime1_host: str = 'https://hanime1.me',
        hanime1_user_lang: str = 'zhs',
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
        self._hanime1_host = hanime1_host.rstrip('/')
        self._hanime1_user_lang = hanime1_user_lang

    @staticmethod
    def _normalize_keywords(raw_keywords: list[Any]) -> list[str]:
        keywords: list[str] = []
        index_by_id: dict[str, int] = {}
        for raw in raw_keywords:
            if not isinstance(raw, str):
                continue
            keyword = TelegramRuntimeConfigBot._normalize_hanime1_seed(raw, allow_id_only=False)
            if keyword is None:
                continue
            seed_id = TelegramRuntimeConfigBot._extract_hanime1_seed_id(keyword)
            if seed_id is None:
                continue
            existing_idx = index_by_id.get(seed_id)
            if existing_idx is None:
                index_by_id[seed_id] = len(keywords)
                keywords.append(keyword)
                continue
            existing = keywords[existing_idx]
            if TelegramRuntimeConfigBot._seed_has_title(existing):
                continue
            if TelegramRuntimeConfigBot._seed_has_title(keyword):
                keywords[existing_idx] = keyword
        return keywords

    @staticmethod
    def _seed_has_title(seed: str) -> bool:
        match = _HANIME1_SEED_RE.fullmatch(seed.strip())
        if not match:
            return False
        return bool((match.group('title') or '').strip())

    @staticmethod
    def _extract_hanime1_seed_id(seed: str) -> str | None:
        match = _HANIME1_SEED_RE.fullmatch(seed.strip())
        if not match:
            return None
        normalized = str(int(match.group('id')))
        if normalized == '0':
            return None
        return normalized

    @staticmethod
    def _normalize_hanime1_seed(raw: str, *, allow_id_only: bool = True) -> str | None:
        text = raw.strip()
        if not text:
            return None

        if text.isdecimal():
            if not allow_id_only:
                return None
            seed_id = str(int(text))
            if seed_id == '0':
                return None
            return f'{{id-{seed_id}}}'

        match = _HANIME1_SEED_RE.fullmatch(text)
        if not match:
            return None

        seed_id = str(int(match.group('id')))
        if seed_id == '0':
            return None

        title = (match.group('title') or '').strip()
        if not title:
            if not allow_id_only:
                return None
            return f'{{id-{seed_id}}}'
        return f'{title} {{id-{seed_id}}}'

    @staticmethod
    def _build_hanime1_seed(*, seed_id: str, title: str | None) -> str:
        normalized_title = (title or '').strip()
        if not normalized_title:
            msg = 'title is required for hanime1 seed'
            raise ValueError(msg)
        return f'{normalized_title} {{id-{seed_id}}}'

    @staticmethod
    def _normalize_hanime1_watch_title(title: str) -> str:
        normalized = re.sub(r'<[^>]+>', '', title)
        normalized = re.sub(r'\s+', ' ', normalized).strip()
        normalized = re.sub(r'\s*-\s*Hanime1\.me\s*$', '', normalized, flags=re.IGNORECASE).strip()
        normalized = re.sub(r'\s*-\s*H動漫/裏番/線上看\s*$', '', normalized).strip()
        return normalized

    @staticmethod
    def _extract_hanime1_div_block_by_id(page_html: str, *, block_id: str) -> str | None:
        pattern = re.compile(rf'<div[^>]+id=["\']{re.escape(block_id)}["\'][^>]*>', re.IGNORECASE)
        tag_pattern = re.compile(r'</?div\b[^>]*>', re.IGNORECASE)
        start_match = pattern.search(page_html)
        if not start_match:
            return None

        depth = 1
        end_pos: int | None = None
        for tag_match in tag_pattern.finditer(page_html, start_match.end()):
            tag = tag_match.group(0).lstrip().lower()
            if tag.startswith('</div'):
                depth -= 1
            else:
                depth += 1
            if depth == 0:
                end_pos = tag_match.end()
                break

        if end_pos is None:
            return None
        return page_html[start_match.start() : end_pos]

    @staticmethod
    def _extract_hanime1_series_ids(playlist_html: str, *, fallback_id: str) -> list[str]:
        ids: list[str] = []
        seen: set[str] = set()
        normalized_html = html.unescape(playlist_html).replace('\\/', '/').replace('\\u0026', '&')
        for match in _HANIME1_WATCH_HREF_RE.finditer(normalized_html):
            query = parse_qs(urlsplit(match.group('url')).query)
            values = query.get('v')
            if not values:
                continue
            candidate = values[0].strip()
            if not candidate.isdecimal():
                continue
            normalized = str(int(candidate))
            if normalized == '0':
                continue
            key = normalized.casefold()
            if key in seen:
                continue
            seen.add(key)
            ids.append(normalized)

        if fallback_id.casefold() not in seen:
            ids.append(fallback_id)
        return ids

    @staticmethod
    def _select_hanime1_canonical_id(series_ids: list[str]) -> str | None:
        if not series_ids:
            return None
        try:
            return str(min(int(item) for item in series_ids))
        except ValueError:
            return None

    async def _resolve_hanime1_series(self, seed_id: str) -> tuple[str, list[str]] | None:
        client = getattr(self, '_client', None)
        if client is None or not hasattr(client, 'get'):
            return None

        host = getattr(self, '_hanime1_host', 'https://hanime1.me').rstrip('/')
        user_lang = str(getattr(self, '_hanime1_user_lang', 'zhs') or 'zhs')
        watch_url = f'{host}/watch?v={seed_id}'
        headers = {
            'Referer': f'{host}/',
            'User-Agent': _HANIME1_USER_AGENT,
            'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8',
            'Accept-Language': 'zh-CN,zh;q=0.9,en;q=0.8',
            'Cookie': f'user_lang={user_lang}',
        }

        try:
            response = await client.get(watch_url, headers=headers)
            response.raise_for_status()
        except Exception as exc:  # noqa: BLE001
            log.debug('Failed to resolve Hanime1 seed title for %s: %s', seed_id, exc)
            return None

        page_html = response.text
        if all(marker in page_html for marker in _HANIME1_CF_BLOCK_MARKERS):
            log.debug('Blocked by Cloudflare while resolving Hanime1 seed title for %s', seed_id)
            return None

        playlist_html = self._extract_hanime1_div_block_by_id(page_html, block_id='video-playlist-wrapper')
        if not playlist_html:
            return None

        match = _HANIME1_PLAYLIST_TITLE_RE.search(playlist_html)
        if not match:
            return None
        title = self._normalize_hanime1_watch_title(match.group('title'))
        if not title:
            return None

        series_ids = self._extract_hanime1_series_ids(playlist_html, fallback_id=seed_id)
        if not series_ids:
            return None
        return title, series_ids

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
        lines = ['Hanime1 series seeds:']
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
                    {'text': 'Add seed', 'callback_data': _CALLBACK_ADD},
                    {'text': 'Delete seed', 'callback_data': _CALLBACK_DELETE},
                ],
            ],
        }

    @staticmethod
    def _config_keyboard() -> dict[str, list[list[dict[str, str]]]]:
        return {'inline_keyboard': [[{'text': 'Hanime1 series seeds', 'callback_data': _CALLBACK_CONFIG_HANIME1}]]}

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
        await self._send_message(
            chat_id=chat_id,
            text='Choose a Hanime1 action:',
            message_thread_id=message_thread_id,
            reply_markup=self._hanime1_keyboard(),
        )

    async def _send_hanime1_keywords_panel(self, *, chat_id: str, message_thread_id: int | None) -> None:
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
        normalized_seed = self._normalize_hanime1_seed(text, allow_id_only=True)
        if not normalized_seed:
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text='Invalid seed. Use `12488` or `屈辱 {id-12488}`. Send /cancel to abort.',
            )
            return

        keywords = self._read_hanime1_keywords()
        keyword_id = self._extract_hanime1_seed_id(normalized_seed)
        if keyword_id is None:
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text='Invalid seed id.',
            )
            return

        resolved = await self._resolve_hanime1_series(keyword_id)
        if not resolved:
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text=f'Failed to resolve series for id {keyword_id}. Seed not added.',
            )
            return

        resolved_title, series_ids = resolved
        canonical_id = self._select_hanime1_canonical_id(series_ids)
        if not canonical_id:
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text=f'Failed to select canonical id for series {keyword_id}. Seed not added.',
            )
            return

        final_seed = self._build_hanime1_seed(seed_id=canonical_id, title=resolved_title)

        existing_by_id: dict[str, int] = {}
        for idx, item in enumerate(keywords):
            item_id = self._extract_hanime1_seed_id(item)
            if item_id is None:
                continue
            existing_by_id[item_id] = idx

        existing_idx = existing_by_id.get(canonical_id)
        if existing_idx is not None:
            self._states.pop(state_key, None)
            await self._send_message(
                chat_id=chat_id,
                message_thread_id=message_thread_id,
                text=f'Duplicate seed: series already exists as {keywords[existing_idx]}',
            )
            await self._send_hanime1_panel(chat_id=chat_id, message_thread_id=message_thread_id)
            return

        keywords.append(final_seed)
        self._write_hanime1_keywords(keywords)
        self._states.pop(state_key, None)
        await self._send_message(
            chat_id=chat_id,
            message_thread_id=message_thread_id,
            text=f'Added seed: {final_seed}',
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
                text='No seeds to delete.',
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
            text=f'Deleted seed: {removed}',
        )
        await self._send_hanime1_keywords_panel(chat_id=chat_id, message_thread_id=message_thread_id)

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
                text='Send seed to add: `12488` or `屈辱 {id-12488}`. Send /cancel to abort.',
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
                text='No seeds to delete.',
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
