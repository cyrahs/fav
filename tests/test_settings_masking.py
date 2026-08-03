# ruff: noqa: INP001, S101, S105

from src.api.settings_masking import MASK_SUFFIX, mask_section, unmask_section


def test_a_nonsecret_field_is_left_alone() -> None:
    masked = mask_section('web.bilibili', {'accounts': [{'name': 'main', 'cookiecloud': {'uuid': 'u', 'password': 'p'}}]})

    assert masked['accounts'][0]['cookiecloud']['uuid'] == 'u'


def test_telegram_bot_token_is_masked() -> None:
    masked = mask_section(
        'notifications.telegram',
        {'enabled': True, 'bot_token': '123456:secret', 'chat_id': '-100123', 'message_thread_id': None},
    )

    assert masked['bot_token'] == f'1234{MASK_SUFFIX}'
    assert masked['chat_id'] == '-100123'


def test_masked_telegram_bot_token_is_preserved_on_save() -> None:
    stored = {'enabled': False, 'bot_token': '123456:secret', 'chat_id': '-100123', 'message_thread_id': None}
    payload = {'enabled': True, 'bot_token': f'1234{MASK_SUFFIX}', 'chat_id': '-100456', 'message_thread_id': 42}

    merged = unmask_section('notifications.telegram', payload, stored)

    assert merged == {
        'enabled': True,
        'bot_token': '123456:secret',
        'chat_id': '-100456',
        'message_thread_id': 42,
    }


def test_short_secret_reveals_no_prefix() -> None:
    assert mask_section('notifications.telegram', {'bot_token': 'abc'})['bot_token'] == MASK_SUFFIX


def test_empty_secret_stays_empty_rather_than_looking_configured() -> None:
    assert mask_section('notifications.telegram', {'bot_token': ''})['bot_token'] == ''


def test_masking_does_not_mutate_the_input_payload() -> None:
    payload = {'accounts': [{'name': 'a', 'api_hash': 'hash-A'}]}

    mask_section('web.telegram', payload)

    assert payload['accounts'][0]['api_hash'] == 'hash-A'


def test_writing_back_a_masked_secret_keeps_the_stored_value() -> None:
    stored = {'bot_token': 'real-token'}

    merged = unmask_section('notifications.telegram', {'bot_token': f'real{MASK_SUFFIX}'}, stored)

    assert merged['bot_token'] == 'real-token'


def test_omitting_a_secret_keeps_the_stored_value() -> None:
    merged = unmask_section('notifications.telegram', {'chat_id': '-100'}, {'bot_token': 'real-token'})

    assert merged['bot_token'] == 'real-token'


def test_a_new_secret_replaces_the_stored_value() -> None:
    merged = unmask_section('notifications.telegram', {'bot_token': 'brand-new'}, {'bot_token': 'real-token'})

    assert merged['bot_token'] == 'brand-new'


def test_telegram_secrets_follow_the_account_name_across_a_reorder() -> None:
    stored = {'accounts': [{'name': 'alice', 'api_hash': 'hash-ALICE'}, {'name': 'bob', 'api_hash': 'hash-BOB'}]}
    masked = mask_section('web.telegram', stored)
    # The UI reorders the accounts and sends the masked values straight back.
    reordered = {'accounts': list(reversed(masked['accounts']))}

    merged = unmask_section('web.telegram', reordered, stored)

    by_name = {account['name']: account['api_hash'] for account in merged['accounts']}
    assert by_name == {'alice': 'hash-ALICE', 'bob': 'hash-BOB'}


def test_bilibili_per_account_cookiecloud_password_is_masked() -> None:
    payload = {'accounts': [{'name': 'main', 'cookiecloud': {'uuid': 'u', 'password': 'super-secret'}}]}

    masked = mask_section('web.bilibili', payload)

    assert masked['accounts'][0]['cookiecloud']['password'] == f'supe{MASK_SUFFIX}'
    assert payload['accounts'][0]['cookiecloud']['password'] == 'super-secret'


def test_bilibili_secrets_follow_the_account_name_across_a_reorder() -> None:
    stored = {
        'accounts': [
            {'name': 'main', 'cookiecloud': {'uuid': 'u1', 'password': 'pw-MAIN'}},
            {'name': 'alt', 'cookiecloud': {'uuid': 'u2', 'password': 'pw-ALT'}},
        ],
    }
    masked = mask_section('web.bilibili', stored)
    reordered = {'accounts': list(reversed(masked['accounts']))}

    merged = unmask_section('web.bilibili', reordered, stored)

    by_name = {account['name']: account['cookiecloud']['password'] for account in merged['accounts']}
    assert by_name == {'main': 'pw-MAIN', 'alt': 'pw-ALT'}


def test_bilibili_account_without_a_cookiecloud_override_is_left_alone() -> None:
    payload = {'accounts': [{'name': 'main', 'favorites': []}]}

    assert mask_section('web.bilibili', payload) == payload
    assert unmask_section('web.bilibili', payload, {}) == payload
