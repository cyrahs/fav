# ruff: noqa: S101

import base64
import json
import sys
from pathlib import Path

import httpx
import pytest
from Crypto.Cipher import AES
from Crypto.Hash import MD5
from Crypto.Util.Padding import pad

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from src.core.config import config as app_config
from src.tool.cookiecloud import CookieCloudClient


KEY_IV_LENGTH = 48
KEY_LENGTH = 32
IV_LENGTH = 16
SALT_PREFIX = b'Salted__'


def encrypt_payload(client: CookieCloudClient, plaintext: str, salt: bytes = b'12345678') -> str:
    key_iv = b''
    prev = b''
    while len(key_iv) < KEY_IV_LENGTH:
        prev = MD5.new(prev + client.key + salt).digest()  # noqa: S303
        key_iv += prev

    derived_key = key_iv[:KEY_LENGTH]
    iv = key_iv[KEY_LENGTH : KEY_LENGTH + IV_LENGTH]
    cipher = AES.new(derived_key, AES.MODE_CBC, iv)
    encrypted = cipher.encrypt(pad(plaintext.encode(), AES.block_size))
    return base64.b64encode(SALT_PREFIX + salt + encrypted).decode()


def test_get_cookies_decrypts_response() -> None:
    client = CookieCloudClient('https://cookiecloud.test', 'uuid123', 'pass123', user_agent='tester/1.0')
    cookies_payload = {'example.com': [{'name': 'sid', 'value': 'abc'}]}
    plaintext = json.dumps({'cookie_data': cookies_payload})
    encrypted = encrypt_payload(client, plaintext)

    class DummyResponse:
        def __init__(self, payload: str) -> None:
            self.payload = payload
            self.status_checked = False

        def raise_for_status(self) -> None:
            self.status_checked = True

        def json(self) -> dict[str, str]:
            return {'encrypted': self.payload}

    class DummyClient:
        def __init__(self, response: DummyResponse) -> None:
            self.response = response
            self.requested: dict[str, dict[str, str]] | None = None

        def get(self, url: str, headers: dict[str, str] | None = None) -> DummyResponse:
            self.requested = {'url': url, 'headers': headers or {}}
            return self.response

    response = DummyResponse(encrypted)
    dummy_client = DummyClient(response)
    client.client = dummy_client  # type: ignore[assignment]

    result = client.get_cookies()

    assert result == cookies_payload
    assert dummy_client.requested == {
        'url': 'https://cookiecloud.test/get/uuid123',
        'headers': {'User-Agent': 'tester/1.0'},
    }
    assert response.status_checked is True


def test_get_cookies_wraps_request_error() -> None:
    client = CookieCloudClient('https://cookiecloud.test', 'uuid123', 'pass123')
    request = httpx.Request('GET', 'https://cookiecloud.test/get/uuid123')

    class FailingClient:
        def get(self, url: str, headers: dict[str, str] | None = None) -> None:  # noqa: ARG002
            raise httpx.RequestError('boom', request=request)

    client.client = FailingClient()  # type: ignore[assignment]

    with pytest.raises(ConnectionError):
        client.get_cookies()


def test_get_cookies_raises_on_invalid_payload() -> None:
    client = CookieCloudClient('https://cookiecloud.test', 'uuid123', 'pass123')

    class DummyResponse:
        def raise_for_status(self) -> None:
            return None

        def json(self) -> dict[str, str]:
            invalid = base64.b64encode(b'not-salted').decode()
            return {'encrypted': invalid}

    class DummyClient:
        def __init__(self, response: DummyResponse) -> None:
            self.response = response

        def get(self, url: str, headers: dict[str, str] | None = None) -> DummyResponse:  # noqa: ARG002
            return self.response

    client.client = DummyClient(DummyResponse())  # type: ignore[assignment]

    with pytest.raises(ValueError):
        client.get_cookies()


def test_save_to_netscape_format_writes_file(tmp_path: Path) -> None:
    client = CookieCloudClient('https://cookiecloud.test', 'uuid123', 'pass123')
    domain = 'example.com'
    cookies_data = {
        domain: [
            {
                'name': 'sid',
                'value': 'abc',
                'domain': '.example.com',
                'path': '/',
                'secure': True,
                'hostOnly': False,
                'expirationDate': 1_700_000_000,
            },
            {
                'name': 'host',
                'value': '789',
                'domain': 'example.com',
                'path': '/account',
                'secure': False,
                'hostOnly': True,
                'expirationDate': 1_700_000_100,
            },
        ],
    }

    def fake_get_cookies() -> dict[str, list[dict]]:
        return cookies_data

    client.get_cookies = fake_get_cookies  # type: ignore[method-assign]
    output_file = tmp_path / 'cookies.txt'

    client.save_to_netscape_format(domain, output_file)

    lines = output_file.read_text().splitlines()
    assert lines[:3] == [
        '# Netscape HTTP Cookie File',
        '# https://curl.se/docs/http-cookies.html',
        '# This file was generated by CookieCloud',
    ]
    assert lines[3:] == [
        '.example.com\tTRUE\t/\tTRUE\t1700000000\tsid\tabc',
        'example.com\tFALSE\t/account\tFALSE\t1700000100\thost\t789',
    ]


def test_save_to_netscape_format_errors_for_missing_domain(tmp_path: Path) -> None:
    client = CookieCloudClient('https://cookiecloud.test', 'uuid123', 'pass123')

    def fake_get_cookies() -> dict[str, list[dict]]:
        return {}

    client.get_cookies = fake_get_cookies  # type: ignore[method-assign]

    with pytest.raises(ValueError):
        client.save_to_netscape_format('missing.com', tmp_path / 'cookies.txt')


def test_save_to_netscape_format_live_bilibili(tmp_path: Path) -> None:
    cfg = app_config.cookiecloud
    client = CookieCloudClient(cfg.server_url, cfg.uuid, cfg.password, proxy=app_config.proxy or None)
    output_file = tmp_path / 'bilibili_cookies.txt'

    try:
        client.save_to_netscape_format('bilibili.com', output_file)
    except ConnectionError as exc:
        pytest.skip(f'CookieCloud unavailable: {exc}')

    lines = output_file.read_text().splitlines()
    assert lines[0] == '# Netscape HTTP Cookie File'
    assert len(lines) > 3
    assert any('bilibili.com' in line for line in lines[3:])
