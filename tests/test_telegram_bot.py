# ruff: noqa: INP001, S101, S105, S106, ANN001, PLR2004

import asyncio
import json
from datetime import UTC, datetime

import httpx
import pytest

from src.tool import telegram_bot
from src.tool.notifications import NotificationRecord


def _config() -> telegram_bot.TelegramBotConfig:
    return telegram_bot.TelegramBotConfig(bot_token='123:secret-token', chat_id='-100123', message_thread_id=42)


def _notification(**overrides: object) -> NotificationRecord:
    values: dict[str, object] = {
        'notification_id': 7,
        'kind': 'job_failed',
        'source': 'worker',
        'title': 'Job failed',
        'body': 'boom',
        'link_url': '',
        'image_url': '',
        'payload': '{}',
        'dedupe_key': 'job_failed:test:boom',
        'status': 'unread',
        'markdown': '*Job failed*\nboom',
        'disable_web_page_preview': True,
        'disable_notification': False,
        'webhook_action': 'upsert',
        'occurrence_count': 1,
        'event_version': 3,
        'delivery_status': 'sending',
        'attempt_count': 0,
        'next_attempt_at': datetime.now(tz=UTC),
        'created_at': datetime.now(tz=UTC),
        'pin': False,
    }
    values.update(overrides)
    return NotificationRecord(**values)  # type: ignore[arg-type]


def test_load_config_requires_enabled_for_worker_but_not_test(pinned_settings) -> None:
    cfg = pinned_settings.notifications.telegram
    cfg.bot_token = '123:token'
    cfg.chat_id = '-100123'

    assert telegram_bot.load_config() is None
    assert telegram_bot.load_config(require_enabled=False) == telegram_bot.TelegramBotConfig(
        bot_token='123:token',
        chat_id='-100123',
    )

    cfg.enabled = True
    assert telegram_bot.load_config() is not None


def test_deliver_sends_markdown_to_configured_topic() -> None:
    requests: list[dict[str, object]] = []

    def _handler(request: httpx.Request) -> httpx.Response:
        requests.append(json.loads(request.content))
        return httpx.Response(200, json={'ok': True, 'result': {'message_id': 99}})

    async def _run() -> telegram_bot.TelegramDeliveryResult:
        async with httpx.AsyncClient(transport=httpx.MockTransport(_handler)) as client:
            return await telegram_bot.deliver(notification=_notification(), client=client, config=_config())

    result = asyncio.run(_run())

    assert result.message_id == 99
    assert requests == [
        {
            'chat_id': '-100123',
            'message_thread_id': 42,
            'text': '*Job failed*\nboom',
            'parse_mode': 'MarkdownV2',
            'disable_notification': False,
            'link_preview_options': {'is_disabled': True},
        },
    ]


def test_deliver_falls_back_to_text_when_remote_photo_is_rejected() -> None:
    methods: list[str] = []

    def _handler(request: httpx.Request) -> httpx.Response:
        method = request.url.path.rsplit('/', 1)[-1]
        methods.append(method)
        if method == 'sendPhoto':
            return httpx.Response(200, json={'ok': False, 'error_code': 400, 'description': 'wrong file identifier'})
        return httpx.Response(200, json={'ok': True, 'result': {'message_id': 101}})

    async def _run() -> telegram_bot.TelegramDeliveryResult:
        async with httpx.AsyncClient(transport=httpx.MockTransport(_handler)) as client:
            return await telegram_bot.deliver(
                notification=_notification(image_url='https://images.example/test.png'),
                client=client,
                config=_config(),
            )

    result = asyncio.run(_run())

    assert methods == ['sendPhoto', 'sendMessage']
    assert result.message_id == 101
    assert result.media_status == 'none'


def test_deliver_exposes_retry_after_without_leaking_token() -> None:
    token = _config().bot_token

    def _handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            429,
            json={
                'ok': False,
                'error_code': 429,
                'description': f'retry for {token}',
                'parameters': {'retry_after': 17},
            },
        )

    async def _run() -> None:
        async with httpx.AsyncClient(transport=httpx.MockTransport(_handler)) as client:
            await telegram_bot.deliver(notification=_notification(), client=client, config=_config())

    with pytest.raises(telegram_bot.TelegramDeliveryError) as error:
        asyncio.run(_run())

    assert error.value.retryable is True
    assert error.value.retry_after_seconds == 17
    assert token not in str(error.value)


def test_pin_failure_is_best_effort() -> None:
    methods: list[str] = []

    def _handler(request: httpx.Request) -> httpx.Response:
        method = request.url.path.rsplit('/', 1)[-1]
        methods.append(method)
        if method == 'pinChatMessage':
            return httpx.Response(200, json={'ok': False, 'error_code': 403, 'description': 'not enough rights'})
        return httpx.Response(200, json={'ok': True, 'result': {'message_id': 99}})

    async def _run() -> telegram_bot.TelegramDeliveryResult:
        async with httpx.AsyncClient(transport=httpx.MockTransport(_handler)) as client:
            return await telegram_bot.deliver(notification=_notification(pin=True), client=client, config=_config())

    result = asyncio.run(_run())

    assert methods == ['sendMessage', 'pinChatMessage']
    assert result.message_id == 99
    assert result.warnings == ('Pinning failed: Telegram Bot API responded with error 403: not enough rights',)


def test_long_markdown_uses_truncated_plain_text() -> None:
    payloads: list[dict[str, object]] = []

    def _handler(request: httpx.Request) -> httpx.Response:
        payloads.append(json.loads(request.content))
        return httpx.Response(200, json={'ok': True, 'result': {'message_id': 99}})

    async def _run() -> None:
        async with httpx.AsyncClient(transport=httpx.MockTransport(_handler)) as client:
            await telegram_bot.deliver(
                notification=_notification(markdown=f'*Title*\n{"x" * telegram_bot.MAX_MESSAGE_LENGTH}'),
                client=client,
                config=_config(),
            )

    asyncio.run(_run())

    assert len(payloads[0]['text']) == telegram_bot.MAX_MESSAGE_LENGTH
    assert 'parse_mode' not in payloads[0]


def test_send_text_now_opens_with_the_same_header_line_as_the_outbox(pinned_settings, monkeypatch) -> None:
    cfg = pinned_settings.notifications.telegram
    cfg.bot_token = '123:token'
    cfg.chat_id = '-100123'
    cfg.enabled = True
    payloads: list[dict[str, object]] = []

    def _handler(request: httpx.Request) -> httpx.Response:
        payloads.append(json.loads(request.content))
        return httpx.Response(200, json={'ok': True, 'result': {'message_id': 5}})

    monkeypatch.setattr(telegram_bot, 'build_client', lambda: httpx.AsyncClient(transport=httpx.MockTransport(_handler)))

    message_id = asyncio.run(telegram_bot.send_text_now(header='RedNote', text='Signed in.'))

    assert message_id == 5
    assert payloads[0]['text'] == 'FAV · RedNote\nSigned in\\.'


def _pin_handler(*, unpin_response: httpx.Response | None = None) -> tuple[list[tuple[str, dict[str, object]]], object]:
    calls: list[tuple[str, dict[str, object]]] = []

    def _handler(request: httpx.Request) -> httpx.Response:
        method = request.url.path.rsplit('/', 1)[-1]
        calls.append((method, json.loads(request.content)))
        if method == 'unpinChatMessage' and unpin_response is not None:
            return unpin_response
        if method in {'pinChatMessage', 'unpinChatMessage'}:
            return httpx.Response(200, json={'ok': True, 'result': True})
        return httpx.Response(200, json={'ok': True, 'result': {'message_id': 99}})

    return calls, _handler


def _deliver(notification: NotificationRecord, handler: object) -> telegram_bot.TelegramDeliveryResult:
    async def _run() -> telegram_bot.TelegramDeliveryResult:
        async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
            return await telegram_bot.deliver(notification=notification, client=client, config=_config())

    return asyncio.run(_run())


def test_resolve_delivery_unpins_the_previous_failure_message() -> None:
    calls, handler = _pin_handler()

    result = _deliver(_notification(webhook_action='resolve', pin=False, telegram_pinned_message_id=55), handler)

    assert [method for method, _ in calls] == ['sendMessage', 'unpinChatMessage']
    assert calls[1][1] == {'chat_id': '-100123', 'message_thread_id': 42, 'message_id': 55}
    assert result.message_id == 99
    assert result.pinned_message_id is None
    assert result.warnings == ()


def test_pinned_repeat_replaces_the_previous_pin() -> None:
    calls, handler = _pin_handler()

    result = _deliver(_notification(pin=True, telegram_pinned_message_id=55), handler)

    assert [method for method, _ in calls] == ['sendMessage', 'pinChatMessage', 'unpinChatMessage']
    assert calls[1][1]['message_id'] == 99
    assert calls[2][1]['message_id'] == 55
    assert result.pinned_message_id == 99


def test_delivery_without_a_previous_pin_does_not_unpin() -> None:
    calls, handler = _pin_handler()

    result = _deliver(_notification(pin=True), handler)

    assert [method for method, _ in calls] == ['sendMessage', 'pinChatMessage']
    assert result.pinned_message_id == 99


def test_unpin_tolerates_a_message_that_is_already_gone() -> None:
    calls, handler = _pin_handler(
        unpin_response=httpx.Response(400, json={'ok': False, 'error_code': 400, 'description': 'message to unpin not found'}),
    )

    result = _deliver(_notification(webhook_action='resolve', telegram_pinned_message_id=55), handler)

    assert [method for method, _ in calls] == ['sendMessage', 'unpinChatMessage']
    assert result.pinned_message_id is None
    assert result.warnings == ()


def test_unpin_failure_keeps_the_previous_pin_for_a_later_attempt() -> None:
    calls, handler = _pin_handler(
        unpin_response=httpx.Response(200, json={'ok': False, 'error_code': 403, 'description': 'not enough rights'}),
    )

    result = _deliver(_notification(webhook_action='resolve', telegram_pinned_message_id=55), handler)

    assert [method for method, _ in calls] == ['sendMessage', 'unpinChatMessage']
    assert result.pinned_message_id == 55
    assert result.warnings == ('Unpinning message 55 failed: Telegram Bot API responded with error 403: not enough rights',)


def test_unpin_failure_does_not_override_a_new_pin() -> None:
    calls, handler = _pin_handler(
        unpin_response=httpx.Response(200, json={'ok': False, 'error_code': 403, 'description': 'not enough rights'}),
    )

    result = _deliver(_notification(pin=True, telegram_pinned_message_id=55), handler)

    assert [method for method, _ in calls] == ['sendMessage', 'pinChatMessage', 'unpinChatMessage']
    assert result.pinned_message_id == 99
    assert len(result.warnings) == 1
