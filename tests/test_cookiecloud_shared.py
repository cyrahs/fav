# ruff: noqa: INP001, S101

import pytest
from pydantic import ValidationError

from src.core import settings

# ---------- the shared registry model ----------


def test_config_names_must_be_unique_case_insensitively() -> None:
    with pytest.raises(ValidationError, match='duplicate cookiecloud config name'):
        settings.CookieCloudConfigs(
            configs=[
                settings.CookieCloudEntry(name='Main', server_url='https://a'),
                settings.CookieCloudEntry(name='main', server_url='https://b'),
            ],
        )


def test_config_names_are_restricted_to_safe_characters() -> None:
    with pytest.raises(ValidationError, match='cookiecloud config name'):
        settings.CookieCloudEntry(name='主力', server_url='https://a')
    with pytest.raises(ValidationError, match='cookiecloud config name'):
        settings.CookieCloudEntry(name='', server_url='https://a')


def test_get_matches_case_insensitively() -> None:
    registry = settings.CookieCloudConfigs(configs=[settings.CookieCloudEntry(name='Main')])

    assert registry.get('main') is registry.configs[0]
    assert registry.get('ghost') is None


def test_resolve_cookiecloud_names_the_missing_config() -> None:
    with pytest.raises(ValueError, match="'ghost'"):
        settings.resolve_cookiecloud('ghost')


def test_the_shared_section_is_registered_for_the_ui() -> None:
    assert settings.SECTION_MODELS['cookiecloud'] is settings.CookieCloudConfigs
    assert settings.SENSITIVE_FIELDS['cookiecloud'] == ('configs[].password',)
    # Rendered last so the registry sits below the sources that reference it.
    assert next(reversed(settings.SECTION_MODELS)) == 'cookiecloud'


# ---------- leftovers of sources that left CookieCloud ----------


def test_leftover_cookiecloud_fields_still_build_settings() -> None:
    # Bilibili, X and pixiv sign in on the 登录态 page now; whatever CookieCloud
    # fields their rows still carry, inline or by name, are ignored.
    creds = {'server_url': 'https://cc.example', 'uuid': 'u', 'password': 'p'}
    rows = {
        'cookiecloud': {'configs': [{'name': 'main', **creds}]},
        'web.bilibili': {
            'accounts': [
                {'name': 'main', 'toview_enabled': True, 'cookiecloud': 'main'},
                {'name': 'alt', 'toview_enabled': True, 'cookiecloud': dict(creds)},
            ],
        },
        'web.twitter': {'username': 'me', 'cookiecloud': dict(creds)},
        'web.pixiv': {'cookiecloud': 'main'},
    }

    snapshot = settings.build_settings(rows)

    assert [account.name for account in snapshot.web.bilibili.accounts] == ['main', 'alt']
    assert snapshot.cookiecloud.get('main').uuid == 'u'
