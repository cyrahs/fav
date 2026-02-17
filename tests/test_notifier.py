# ruff: noqa: INP001, S101, S105

import asyncio
from pathlib import Path

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

    async def post(self, url: str, **kwargs: object) -> _DummyResponse:
        self.calls.append((url, kwargs))
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
                'json': {
                    'chat_id': '-1001234567',
                    'text': 'hello',
                    'disable_notification': True,
                    'message_thread_id': 9,
                },
            },
        ),
    ]


def test_telegram_bot_notifier_truncates_long_message(monkeypatch) -> None:  # noqa: ANN001
    test_token = 'test-token'

    class _CaptureClient(_DummyAsyncClient):
        async def post(self, url: str, **kwargs: object) -> _DummyResponse:
            self.calls.append((url, kwargs))
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

    sent = holder['client'].calls[0][1]['json']['text']
    assert isinstance(sent, str)
    assert len(sent) == notifier_module._MAX_MESSAGE_LENGTH  # noqa: SLF001
    assert sent.endswith('\n...')


def test_telegram_bot_notifier_raises_for_non_ok_response(monkeypatch) -> None:  # noqa: ANN001
    test_token = 'test-token'

    class _FailClient(_DummyAsyncClient):
        async def post(self, url: str, **kwargs: object) -> _DummyResponse:
            self.calls.append((url, kwargs))
            return _DummyResponse({'ok': False, 'description': 'chat not found'})

    def _factory(*args, **kwargs) -> _FailClient:  # noqa: ANN002, ANN003
        return _FailClient(*args, **kwargs)

    monkeypatch.setattr(notifier_module.httpx, 'AsyncClient', _factory)

    notifier = TelegramBotNotifier(token=test_token, chat_id='1001')

    with pytest.raises(RuntimeError):
        asyncio.run(notifier.send('hello'))


def test_telegram_bot_notifier_sends_photo_by_url(monkeypatch) -> None:  # noqa: ANN001
    holder: dict[str, _DummyAsyncClient] = {}

    def _factory(*args, **kwargs) -> _DummyAsyncClient:  # noqa: ANN002, ANN003
        client = _DummyAsyncClient(*args, **kwargs)
        holder['client'] = client
        return client

    monkeypatch.setattr(notifier_module.httpx, 'AsyncClient', _factory)

    notifier = TelegramBotNotifier(token='test-token', chat_id='1001')
    asyncio.run(notifier.send_photo(photo='https://example.com/foo.png', caption='photo caption'))

    client = holder['client']
    assert client.calls == [
        (
            'https://api.telegram.org/bottest-token/sendPhoto',
            {
                'json': {
                    'chat_id': '1001',
                    'disable_notification': False,
                    'caption': 'photo caption',
                    'photo': 'https://example.com/foo.png',
                },
            },
        ),
    ]


def test_telegram_bot_notifier_sends_photo_by_file(monkeypatch, tmp_path) -> None:  # noqa: ANN001
    holder: dict[str, _DummyAsyncClient] = {}

    def _factory(*args, **kwargs) -> _DummyAsyncClient:  # noqa: ANN002, ANN003
        client = _DummyAsyncClient(*args, **kwargs)
        holder['client'] = client
        return client

    monkeypatch.setattr(notifier_module.httpx, 'AsyncClient', _factory)

    image_path = tmp_path / 'foo.png'
    image_path.write_bytes(b'img')

    notifier = TelegramBotNotifier(token='test-token', chat_id='1001', disable_notification=True)
    asyncio.run(notifier.send_photo(photo=image_path, caption='photo caption'))

    client = holder['client']
    assert len(client.calls) == 1
    call_url, payload = client.calls[0]
    assert call_url == 'https://api.telegram.org/bottest-token/sendPhoto'
    assert payload['data'] == {
        'chat_id': '1001',
        'disable_notification': 'true',
        'caption': 'photo caption',
    }
    files = payload['files']
    assert isinstance(files, dict)
    photo_part = files.get('photo')
    assert isinstance(photo_part, tuple)
    assert photo_part[0] == Path('foo.png').name
