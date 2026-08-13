# ruff: noqa: INP001, S101, ANN001, ANN202, EM101, TRY003, PLR2004

import httpx
import pytest

from src.tool import cookiecloud as cookiecloud_tool


def _install_cookies(monkeypatch: pytest.MonkeyPatch, cookies) -> None:
    """Make CookieCloudClient.get_cookies return a value or raise, without any network."""

    def _fake_get_cookies(_self):
        if isinstance(cookies, Exception):
            raise cookies
        return cookies

    monkeypatch.setattr(cookiecloud_tool.CookieCloudClient, 'get_cookies', _fake_get_cookies)


def _bilibili_cookies(names) -> dict:
    return {'bilibili.com': [{'name': name, 'value': 'v'} for name in names]}


def test_probe_reports_incomplete_config_without_touching_the_network(monkeypatch) -> None:
    def _explode(_self):
        raise AssertionError('should not reach the network')

    monkeypatch.setattr(cookiecloud_tool.CookieCloudClient, 'get_cookies', _explode)

    result = cookiecloud_tool.probe('https://cc.example', '', 'pw')

    assert result.ok is False
    assert result.code == 'incomplete'
    assert 'uuid' in result.message


def test_probe_succeeds_when_every_required_cookie_is_present(monkeypatch) -> None:
    _install_cookies(monkeypatch, {**_bilibili_cookies(cookiecloud_tool.BILIBILI_PROFILE.required_cookies), 'other.com': []})

    result = cookiecloud_tool.probe('https://cc.example', 'u', 'pw')

    assert result.ok is True
    assert result.code == 'ok'
    assert result.domain_count == 2
    assert result.domain_cookie_count == 4


def test_probe_matches_required_cookie_names_case_insensitively(monkeypatch) -> None:
    _install_cookies(monkeypatch, _bilibili_cookies(['SESSDATA', 'bili_jct', 'buvid3', 'DedeUserID']))

    assert cookiecloud_tool.probe('https://cc.example', 'u', 'pw').ok is True


def test_probe_flags_a_vault_missing_bilibili_cookies(monkeypatch) -> None:
    _install_cookies(monkeypatch, {'example.com': [{'name': 'a', 'value': 'b'}]})

    result = cookiecloud_tool.probe('https://cc.example', 'u', 'pw')

    assert result.ok is False
    assert result.code == 'no_domain_cookies'
    assert result.domain_count == 1


def test_probe_lists_the_specific_missing_cookies(monkeypatch) -> None:
    _install_cookies(monkeypatch, _bilibili_cookies(['sessdata', 'buvid3']))

    result = cookiecloud_tool.probe('https://cc.example', 'u', 'pw')

    assert result.ok is False
    assert result.code == 'missing_cookies'
    assert result.missing_cookies == ('bili_jct', 'dedeuserid')


def test_probe_separates_an_unreachable_server_from_a_bad_password(monkeypatch) -> None:
    _install_cookies(monkeypatch, ConnectionError('Failed to connect to CookieCloud server: timeout'))

    result = cookiecloud_tool.probe('https://cc.example', 'u', 'pw')

    assert result.ok is False
    assert result.code == 'unreachable'


def test_probe_reports_a_failed_decrypt_as_a_credential_problem(monkeypatch) -> None:
    # A wrong password surfaces as an unpad/JSON error, not a network error.
    _install_cookies(monkeypatch, ValueError('Padding is incorrect.'))

    result = cookiecloud_tool.probe('https://cc.example', 'u', 'wrong-pw')

    assert result.ok is False
    assert result.code == 'decrypt_failed'
    assert 'wrong-pw' not in result.message


def test_probe_reports_an_http_error_with_its_status(monkeypatch) -> None:
    request = httpx.Request('GET', 'https://cc.example/get/u')
    response = httpx.Response(404, request=request)
    _install_cookies(monkeypatch, httpx.HTTPStatusError('not found', request=request, response=response))

    result = cookiecloud_tool.probe('https://cc.example', 'u', 'pw')

    assert result.ok is False
    assert result.code == 'http_error'
    assert '404' in result.message


def test_probe_accepts_an_x_session_stored_under_either_hostname(monkeypatch) -> None:
    # Which hostname the extension syncs under depends on when it last ran.
    _install_cookies(
        monkeypatch,
        {
            'twitter.com': [{'name': 'auth_token', 'value': 'v'}],
            'x.com': [{'name': 'ct0', 'value': 'v'}],
        },
    )

    result = cookiecloud_tool.probe('https://cc.example', 'u', 'pw', profile=cookiecloud_tool.TWITTER_PROFILE)

    assert result.ok is True
    assert result.domain_cookie_count == 2


def test_probe_names_the_x_cookie_that_is_missing(monkeypatch) -> None:
    _install_cookies(monkeypatch, {'x.com': [{'name': 'auth_token', 'value': 'v'}]})

    result = cookiecloud_tool.probe('https://cc.example', 'u', 'pw', profile=cookiecloud_tool.TWITTER_PROFILE)

    assert result.ok is False
    assert result.code == 'missing_cookies'
    assert result.missing_cookies == ('ct0',)


def test_a_bilibili_only_vault_is_not_mistaken_for_an_x_session(monkeypatch) -> None:
    _install_cookies(monkeypatch, _bilibili_cookies(cookiecloud_tool.BILIBILI_PROFILE.required_cookies))

    result = cookiecloud_tool.probe('https://cc.example', 'u', 'pw', profile=cookiecloud_tool.TWITTER_PROFILE)

    assert result.ok is False
    assert result.code == 'no_domain_cookies'
