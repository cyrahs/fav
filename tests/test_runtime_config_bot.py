# ruff: noqa: INP001, S101, SLF001, ANN001

import asyncio
import json
from pathlib import Path

from src.tool.runtime_config_bot import TelegramRuntimeConfigBot


def _make_bot(tmp_path: Path) -> TelegramRuntimeConfigBot:
    bot = TelegramRuntimeConfigBot.__new__(TelegramRuntimeConfigBot)
    bot._allowed_chat_id = '123'
    bot._allowed_message_thread_id = None
    bot._run_config = tmp_path / 'config.json'
    bot._states = {}
    bot._trigger_targets = [('bilibili', 'Bilibili'), ('hanime1', 'Hanime1')]
    bot._trigger_callback = None
    bot._commands_registered = False
    bot._set_my_commands_url = 'https://api.telegram.org/botTEST/setMyCommands'
    return bot


def test_hanime1_keywords_read_write_preserves_other_fields(tmp_path) -> None:
    bot = _make_bot(tmp_path)
    bot._write_runtime_config(
        {
            'other': {'enabled': True},
            'hanime1': {'keywords': ['{id-12488}', '屈辱 {id-12488}', '', '12345', 'bad-seed', 123]},
        },
    )

    keywords = bot._read_hanime1_keywords()
    assert keywords == ['屈辱 {id-12488}']

    bot._write_hanime1_keywords(['k1', 'k2'])
    payload = json.loads(bot._run_config.read_text(encoding='utf-8'))
    assert payload['other'] == {'enabled': True}
    assert payload['hanime1']['keywords'] == ['k1', 'k2']


def test_hanime1_add_keyword_flow(tmp_path) -> None:
    bot = _make_bot(tmp_path)
    bot._write_hanime1_keywords(['旧系列 {id-100}'])

    sent_texts: list[str] = []

    async def _fake_send_message(*, chat_id, text, message_thread_id, reply_markup=None) -> None:  # noqa: ARG001
        sent_texts.append(text)

    async def _fake_answer_callback_query(callback_query_id, *, text=None) -> None:  # noqa: ARG001
        return None

    async def _fake_resolve(seed_id: str) -> tuple[str, list[str]] | None:
        if seed_id == '12488':
            return ('屈辱', ['12488', '12496'])
        return None

    bot._send_message = _fake_send_message
    bot._answer_callback_query = _fake_answer_callback_query
    bot._resolve_hanime1_series = _fake_resolve

    async def _run() -> None:
        await bot._handle_message({'text': '/config', 'chat': {'id': '123'}})
        await bot._handle_callback_query(
            {
                'id': 'cb0',
                'data': 'config:hanime1',
                'message': {
                    'chat': {'id': '123'},
                },
            },
        )
        await bot._handle_callback_query(
            {
                'id': 'cb1',
                'data': 'hanime1:add',
                'message': {
                    'chat': {'id': '123'},
                },
            },
        )
        await bot._handle_message({'text': '12488', 'chat': {'id': '123'}})

    asyncio.run(_run())

    assert bot._read_hanime1_keywords() == ['旧系列 {id-100}', '屈辱 {id-12488}']
    assert any('Choose a Hanime1 action:' in text for text in sent_texts)
    assert any('Added seed: 屈辱 {id-12488}' in text for text in sent_texts)
    assert not any(text.startswith('Hanime1 series seeds:') for text in sent_texts)


def test_hanime1_add_seed_resolves_title_immediately(tmp_path) -> None:
    bot = _make_bot(tmp_path)
    bot._write_hanime1_keywords([])

    sent_texts: list[str] = []

    async def _fake_send_message(*, chat_id, text, message_thread_id, reply_markup=None) -> None:  # noqa: ARG001
        sent_texts.append(text)

    async def _fake_answer_callback_query(callback_query_id, *, text=None) -> None:  # noqa: ARG001
        return None

    async def _fake_resolve(seed_id: str) -> tuple[str, list[str]] | None:
        assert seed_id == '12488'
        return ('屈辱', ['12488', '12496'])

    bot._send_message = _fake_send_message
    bot._answer_callback_query = _fake_answer_callback_query
    bot._resolve_hanime1_series = _fake_resolve

    async def _run() -> None:
        await bot._handle_callback_query(
            {
                'id': 'cb-title-1',
                'data': 'hanime1:add',
                'message': {
                    'chat': {'id': '123'},
                },
            },
        )
        await bot._handle_message({'text': '12488', 'chat': {'id': '123'}})

    asyncio.run(_run())

    assert bot._read_hanime1_keywords() == ['屈辱 {id-12488}']
    assert any('Added seed: 屈辱 {id-12488}' in text for text in sent_texts)
    assert not any(text.startswith('Hanime1 series seeds:') for text in sent_texts)


def test_hanime1_add_seed_selects_min_id_from_series_list(tmp_path) -> None:
    bot = _make_bot(tmp_path)
    bot._write_hanime1_keywords([])

    sent_texts: list[str] = []

    async def _fake_send_message(*, chat_id, text, message_thread_id, reply_markup=None) -> None:  # noqa: ARG001
        sent_texts.append(text)

    async def _fake_answer_callback_query(callback_query_id, *, text=None) -> None:  # noqa: ARG001
        return None

    async def _fake_resolve(seed_id: str) -> tuple[str, list[str]] | None:
        assert seed_id == '36345'
        return ('屈辱', ['39311', '39310', '36345', '36344'])

    bot._send_message = _fake_send_message
    bot._answer_callback_query = _fake_answer_callback_query
    bot._resolve_hanime1_series = _fake_resolve

    async def _run() -> None:
        await bot._handle_callback_query(
            {
                'id': 'cb-min-id',
                'data': 'hanime1:add',
                'message': {
                    'chat': {'id': '123'},
                },
            },
        )
        await bot._handle_message({'text': '36345', 'chat': {'id': '123'}})

    asyncio.run(_run())

    assert bot._read_hanime1_keywords() == ['屈辱 {id-36344}']
    assert any('Added seed: 屈辱 {id-36344}' in text for text in sent_texts)


def test_hanime1_add_seed_fails_when_title_resolve_fails(tmp_path) -> None:
    bot = _make_bot(tmp_path)
    bot._write_hanime1_keywords([])

    sent_texts: list[str] = []

    async def _fake_send_message(*, chat_id, text, message_thread_id, reply_markup=None) -> None:  # noqa: ARG001
        sent_texts.append(text)

    async def _fake_answer_callback_query(callback_query_id, *, text=None) -> None:  # noqa: ARG001
        return None

    async def _fake_resolve(seed_id: str) -> tuple[str, list[str]] | None:
        assert seed_id == '12488'
        return None

    bot._send_message = _fake_send_message
    bot._answer_callback_query = _fake_answer_callback_query
    bot._resolve_hanime1_series = _fake_resolve

    async def _run() -> None:
        await bot._handle_callback_query(
            {
                'id': 'cb-title-fail',
                'data': 'hanime1:add',
                'message': {
                    'chat': {'id': '123'},
                },
            },
        )
        await bot._handle_message({'text': '12488', 'chat': {'id': '123'}})

    asyncio.run(_run())

    assert bot._read_hanime1_keywords() == []
    assert any('Seed not added.' in text for text in sent_texts)


def test_hanime1_add_seed_rejects_duplicate_series_by_canonical_id(tmp_path) -> None:
    bot = _make_bot(tmp_path)
    bot._write_hanime1_keywords(['屈辱 {id-36344}'])

    sent_texts: list[str] = []

    async def _fake_send_message(*, chat_id, text, message_thread_id, reply_markup=None) -> None:  # noqa: ARG001
        sent_texts.append(text)

    async def _fake_answer_callback_query(callback_query_id, *, text=None) -> None:  # noqa: ARG001
        return None

    async def _fake_resolve(seed_id: str) -> tuple[str, list[str]] | None:
        assert seed_id == '39310'
        return ('屈辱', ['39311', '39310', '36345', '36344'])

    bot._send_message = _fake_send_message
    bot._answer_callback_query = _fake_answer_callback_query
    bot._resolve_hanime1_series = _fake_resolve

    async def _run() -> None:
        await bot._handle_callback_query(
            {
                'id': 'cb-dup-series',
                'data': 'hanime1:add',
                'message': {
                    'chat': {'id': '123'},
                },
            },
        )
        await bot._handle_message({'text': '39310', 'chat': {'id': '123'}})

    asyncio.run(_run())

    assert bot._read_hanime1_keywords() == ['屈辱 {id-36344}']
    assert any('Duplicate seed:' in text for text in sent_texts)


def test_hanime1_delete_keyword_by_number_flow(tmp_path) -> None:
    bot = _make_bot(tmp_path)
    bot._write_hanime1_keywords(['A {id-1}', 'B {id-2}', 'C {id-3}'])

    sent_texts: list[str] = []

    async def _fake_send_message(*, chat_id, text, message_thread_id, reply_markup=None) -> None:  # noqa: ARG001
        sent_texts.append(text)

    async def _fake_answer_callback_query(callback_query_id, *, text=None) -> None:  # noqa: ARG001
        return None

    bot._send_message = _fake_send_message
    bot._answer_callback_query = _fake_answer_callback_query

    async def _run() -> None:
        await bot._handle_callback_query(
            {
                'id': 'cb2',
                'data': 'hanime1:delete',
                'message': {
                    'chat': {'id': '123'},
                },
            },
        )
        await bot._handle_message({'text': '2', 'chat': {'id': '123'}})

    asyncio.run(_run())

    assert bot._read_hanime1_keywords() == ['A {id-1}', 'C {id-3}']
    assert any('Send the number to delete.' in text for text in sent_texts)
    assert any('Deleted seed: B {id-2}' in text for text in sent_texts)
    assert any(text == 'Hanime1 series seeds:\n1. A {id-1}\n2. C {id-3}' for text in sent_texts)


def test_set_my_commands_registers_default_and_chat_scope(tmp_path) -> None:
    bot = _make_bot(tmp_path)

    class _DummyResponse:
        @staticmethod
        def raise_for_status() -> None:
            return None

        @staticmethod
        def json() -> dict[str, bool]:
            return {'ok': True}

    class _DummyClient:
        def __init__(self) -> None:
            self.posts: list[tuple[str, dict[str, object]]] = []

        async def post(self, url: str, json: dict[str, object]) -> _DummyResponse:
            self.posts.append((url, json))
            return _DummyResponse()

    dummy_client = _DummyClient()
    bot._client = dummy_client

    asyncio.run(bot._set_my_commands())

    expected_calls = 2
    assert len(dummy_client.posts) == expected_calls
    assert dummy_client.posts[0][0] == bot._set_my_commands_url
    assert dummy_client.posts[0][1]['commands'][0]['command'] == 'config'
    assert dummy_client.posts[0][1]['commands'][1]['command'] == 'trigger'
    assert dummy_client.posts[1][1]['scope'] == {'type': 'chat', 'chat_id': '123'}


def test_config_command_sends_config_panel(tmp_path) -> None:
    bot = _make_bot(tmp_path)
    sent_payloads: list[tuple[str, dict[str, object] | None]] = []

    async def _fake_send_message(*, chat_id, text, message_thread_id, reply_markup=None) -> None:  # noqa: ARG001
        sent_payloads.append((text, reply_markup))

    bot._send_message = _fake_send_message

    asyncio.run(bot._handle_message({'text': '/config', 'chat': {'id': '123'}}))

    assert sent_payloads
    text, reply_markup = sent_payloads[0]
    assert text == 'Choose a config option:'
    assert reply_markup is not None
    keyboard = reply_markup['inline_keyboard']
    labels = [btn['text'] for row in keyboard for btn in row]
    assert 'Hanime1 series seeds' in labels


def test_hanime1_config_callback_sends_action_panel_without_keywords(tmp_path) -> None:
    bot = _make_bot(tmp_path)
    bot._write_hanime1_keywords(['A {id-1}', 'B {id-2}'])
    sent_payloads: list[tuple[str, dict[str, object] | None]] = []

    async def _fake_send_message(*, chat_id, text, message_thread_id, reply_markup=None) -> None:  # noqa: ARG001
        sent_payloads.append((text, reply_markup))

    async def _fake_answer_callback_query(callback_query_id, *, text=None) -> None:  # noqa: ARG001
        return None

    bot._send_message = _fake_send_message
    bot._answer_callback_query = _fake_answer_callback_query

    asyncio.run(
        bot._handle_callback_query(
            {
                'id': 'cb-config-hanime1',
                'data': 'config:hanime1',
                'message': {
                    'chat': {'id': '123'},
                },
            },
        ),
    )

    assert sent_payloads
    text, reply_markup = sent_payloads[0]
    assert text == 'Choose a Hanime1 action:'
    assert reply_markup is not None
    labels = [btn['text'] for row in reply_markup['inline_keyboard'] for btn in row]
    assert 'Add seed' in labels
    assert 'Delete seed' in labels


def test_unknown_command_is_ignored(tmp_path) -> None:
    bot = _make_bot(tmp_path)
    sent_texts: list[str] = []

    async def _fake_send_message(*, chat_id, text, message_thread_id, reply_markup=None) -> None:  # noqa: ARG001
        sent_texts.append(text)

    bot._send_message = _fake_send_message

    asyncio.run(bot._handle_message({'text': '/unknown-command', 'chat': {'id': '123'}}))

    assert sent_texts == []


def test_trigger_command_sends_target_buttons(tmp_path) -> None:
    bot = _make_bot(tmp_path)
    sent_payloads: list[tuple[str, dict[str, object] | None]] = []

    async def _fake_send_message(*, chat_id, text, message_thread_id, reply_markup=None) -> None:  # noqa: ARG001
        sent_payloads.append((text, reply_markup))

    bot._send_message = _fake_send_message

    asyncio.run(bot._handle_message({'text': '/trigger', 'chat': {'id': '123'}}))

    assert sent_payloads
    text, reply_markup = sent_payloads[0]
    assert text == 'Choose a job to trigger now:'
    assert reply_markup is not None
    keyboard = reply_markup['inline_keyboard']
    labels = [btn['text'] for row in keyboard for btn in row]
    assert 'Bilibili' in labels
    assert 'Hanime1' in labels
    assert 'All' in labels


def test_trigger_callback_invokes_trigger_handler(tmp_path) -> None:
    bot = _make_bot(tmp_path)
    sent_texts: list[str] = []
    triggered_targets: list[str] = []

    async def _fake_send_message(*, chat_id, text, message_thread_id, reply_markup=None) -> None:  # noqa: ARG001
        sent_texts.append(text)

    async def _fake_answer_callback_query(callback_query_id, *, text=None) -> None:  # noqa: ARG001
        return None

    async def _fake_trigger_callback(target: str) -> str:
        triggered_targets.append(target)
        return f'Triggered {target}'

    bot._send_message = _fake_send_message
    bot._answer_callback_query = _fake_answer_callback_query
    bot._trigger_callback = _fake_trigger_callback

    asyncio.run(
        bot._handle_callback_query(
            {
                'id': 'cb3',
                'data': 'trigger:all',
                'message': {
                    'chat': {'id': '123'},
                },
            },
        ),
    )

    assert triggered_targets == ['all']
    assert 'Triggered all' in sent_texts[-1]
