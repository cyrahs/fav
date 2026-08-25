# ruff: noqa: INP001, S101, S105

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


# ---------- migration of legacy inline credentials ----------


def _legacy_rows() -> dict:
    shared_creds = {'server_url': 'https://cc.example', 'uuid': 'shared-uuid', 'password': 'shared-pw'}
    return {
        'web.bilibili': {
            'accounts': [
                {'name': 'main', 'toview_enabled': True, 'cookiecloud': dict(shared_creds)},
                {
                    'name': 'alt',
                    'toview_enabled': True,
                    'cookiecloud': {'server_url': 'https://cc.alt', 'uuid': 'alt-uuid', 'password': 'alt-pw'},
                },
            ],
        },
        'web.twitter': {'username': 'me', 'cookiecloud': dict(shared_creds)},
        'web.pixiv': {'cookiecloud': {}},
    }


def test_migration_hoists_inline_credentials_into_the_shared_registry() -> None:
    rows = _legacy_rows()

    changed = settings.migrate_legacy_cookiecloud(rows)

    assert changed == {'cookiecloud', 'web.bilibili', 'web.twitter', 'web.pixiv'}
    # Identical credentials collapse into one entry named after the first consumer.
    registry = {entry['name']: entry for entry in rows['cookiecloud']['configs']}
    assert set(registry) == {'twitter', 'alt'}
    assert registry['twitter']['password'] == 'shared-pw'
    assert rows['web.twitter']['cookiecloud'] == 'twitter'
    assert rows['web.bilibili']['accounts'][0]['cookiecloud'] == 'twitter'
    assert rows['web.bilibili']['accounts'][1]['cookiecloud'] == 'alt'
    # Blank credentials become an empty reference, not a useless entry.
    assert rows['web.pixiv']['cookiecloud'] == ''

    # The migrated rows build a valid settings tree.
    snapshot = settings.build_settings(rows)
    assert snapshot.cookiecloud.get('twitter').uuid == 'shared-uuid'


def test_migration_is_idempotent() -> None:
    rows = _legacy_rows()
    settings.migrate_legacy_cookiecloud(rows)

    assert settings.migrate_legacy_cookiecloud(rows) == set()


def test_migration_avoids_name_collisions_with_existing_entries() -> None:
    rows = {
        'cookiecloud': {'configs': [{'name': 'main', 'server_url': 'https://cc.other', 'uuid': 'o', 'password': 'o'}]},
        'web.bilibili': {
            'accounts': [
                {'name': 'main', 'toview_enabled': True, 'cookiecloud': {'server_url': 'https://cc.main', 'uuid': 'm', 'password': 'm'}},
            ],
        },
    }

    settings.migrate_legacy_cookiecloud(rows)

    names = [entry['name'] for entry in rows['cookiecloud']['configs']]
    assert names == ['main', 'main-2']
    assert rows['web.bilibili']['accounts'][0]['cookiecloud'] == 'main-2'


def test_migration_leaves_already_migrated_rows_alone() -> None:
    rows = {
        'cookiecloud': {'configs': [{'name': 'main', 'server_url': 'https://cc', 'uuid': 'u', 'password': 'p'}]},
        'web.bilibili': {'accounts': [{'name': 'main', 'toview_enabled': True, 'cookiecloud': 'main'}]},
        'web.twitter': {'username': 'me', 'cookiecloud': 'main'},
    }

    assert settings.migrate_legacy_cookiecloud(rows) == set()
