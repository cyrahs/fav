# ruff: noqa: INP001, S101, S105

import asyncio

import pytest

import src.tool.notifier as notifier_module
from src.tool.notifier import TelegramBotNotifier


class _DummyResponse:
    def __init__(self, payload: dict[str, object]) -> None:
        self._payload = payload

    def raise_for_status(self) -> None:
        return None

    def json(self) -> dict[str, object]:
        return self._payload


class _DummyAsyncClient:
    def __init__(self, *args, **kwargs) -> None:  # noqa: ANN002, ANN003
        self.args = args
        self.kwargs = kwargs
        self.calls: list[tuple[str, dict[str, object]]] = []

    async def post(self, url: str, json: dict[str, object]) -> _DummyResponse:
        self.calls.append((url, json))
        return _DummyResponse({'ok': True})

    async def aclose(self) -> None:
        return None


def test_telegram_bot_notifier_sends_expected_payload(monkeypatch) -> None:  # noqa: ANN001
    holder: dict[str, _DummyAsyncClient] = {}
    test_token = 'test-token'

    def _factory(*args, **kwargs) -> _DummyAsyncClient:  # noqa: ANN002, ANN003
        client = _DummyAsyncClient(*args, **kwargs)
        holder['client'] = client
        return client

    monkeypatch.setattr(notifier_module.httpx, 'AsyncClient', _factory)

    notifier = TelegramBotNotifier(
        token=test_token,
        chat_id='-1001234567',
        api_base='https://api.telegram.org',
        disable_notification=True,
        message_thread_id=9,
        proxy='http://127.0.0.1:7890',
    )

    asyncio.run(notifier.send('hello'))

    client = holder['client']
    assert client.kwargs['proxy'] == 'http://127.0.0.1:7890'
    assert client.calls == [
        (
            'https://api.telegram.org/bottest-token/sendMessage',
            {
                'chat_id': '-1001234567',
                'text': 'hello',
                'disable_notification': True,
                'message_thread_id': 9,
            },
        ),
    ]


def test_telegram_bot_notifier_truncates_long_message(monkeypatch) -> None:  # noqa: ANN001
    test_token = 'test-token'

    class _CaptureClient(_DummyAsyncClient):
        async def post(self, url: str, json: dict[str, object]) -> _DummyResponse:
            self.calls.append((url, json))
            return _DummyResponse({'ok': True})

    holder: dict[str, _CaptureClient] = {}

    def _factory(*args, **kwargs) -> _CaptureClient:  # noqa: ANN002, ANN003
        client = _CaptureClient(*args, **kwargs)
        holder['client'] = client
        return client

    monkeypatch.setattr(notifier_module.httpx, 'AsyncClient', _factory)

    notifier = TelegramBotNotifier(token=test_token, chat_id='1001')

    msg = 'x' * 5000
    asyncio.run(notifier.send(msg))

    sent = holder['client'].calls[0][1]['text']
    assert isinstance(sent, str)
    assert len(sent) == notifier_module._MAX_MESSAGE_LENGTH  # noqa: SLF001
    assert sent.endswith('\n...')


def test_telegram_bot_notifier_raises_for_non_ok_response(monkeypatch) -> None:  # noqa: ANN001
    test_token = 'test-token'

    class _FailClient(_DummyAsyncClient):
        async def post(self, url: str, json: dict[str, object]) -> _DummyResponse:
            self.calls.append((url, json))
            return _DummyResponse({'ok': False, 'description': 'chat not found'})

    def _factory(*args, **kwargs) -> _FailClient:  # noqa: ANN002, ANN003
        return _FailClient(*args, **kwargs)

    monkeypatch.setattr(notifier_module.httpx, 'AsyncClient', _factory)

    notifier = TelegramBotNotifier(token=test_token, chat_id='1001')

    with pytest.raises(RuntimeError):
        asyncio.run(notifier.send('hello'))
