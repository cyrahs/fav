# ruff: noqa: INP001, S101, S105

from src.api.settings_masking import MASK_SUFFIX, mask_section, unmask_section


def test_a_nonsecret_field_is_left_alone() -> None:
    masked = mask_section('cookiecloud', {'configs': [{'name': 'main', 'uuid': 'u', 'password': 'p'}]})

    assert masked['configs'][0]['uuid'] == 'u'


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


def test_shared_cookiecloud_password_is_masked() -> None:
    payload = {'configs': [{'name': 'main', 'server_url': 'https://cc.example', 'uuid': 'u', 'password': 'super-secret'}]}

    masked = mask_section('cookiecloud', payload)

    assert masked['configs'][0]['password'] == f'supe{MASK_SUFFIX}'
    assert masked['configs'][0]['uuid'] == 'u'
    assert payload['configs'][0]['password'] == 'super-secret'


def test_shared_cookiecloud_secrets_follow_the_config_name_across_a_reorder() -> None:
    stored = {
        'configs': [
            {'name': 'main', 'uuid': 'u1', 'password': 'pw-MAIN'},
            {'name': 'alt', 'uuid': 'u2', 'password': 'pw-ALT'},
        ],
    }
    masked = mask_section('cookiecloud', stored)
    reordered = {'configs': list(reversed(masked['configs']))}

    merged = unmask_section('cookiecloud', reordered, stored)

    by_name = {config['name']: config['password'] for config in merged['configs']}
    assert by_name == {'main': 'pw-MAIN', 'alt': 'pw-ALT'}


def test_saving_a_shared_config_with_a_masked_password_keeps_the_stored_one() -> None:
    stored = {'configs': [{'name': 'main', 'uuid': 'u', 'password': 'pw-REAL'}]}
    edited = mask_section('cookiecloud', stored)
    edited['configs'][0]['uuid'] = 'new-uuid'

    merged = unmask_section('cookiecloud', edited, stored)

    assert merged['configs'][0]['password'] == 'pw-REAL'
    assert merged['configs'][0]['uuid'] == 'new-uuid'


def test_the_consumer_sections_hold_only_a_reference_and_stay_plaintext() -> None:
    # The bilibili/twitter/pixiv sections now carry only the name of a shared
    # config, so nothing in them is masked.
    bilibili = {'accounts': [{'name': 'main', 'cookiecloud': 'main-vault'}]}
    twitter = {'username': 'me', 'cookiecloud': 'main-vault'}

    assert mask_section('web.bilibili', bilibili) == bilibili
    assert unmask_section('web.bilibili', bilibili, {}) == bilibili
    assert mask_section('web.twitter', twitter) == twitter
    assert unmask_section('web.twitter', twitter, {}) == twitter


def test_the_rednote_proxy_is_shown_and_saved_in_plaintext() -> None:
    # Deliberate: proxies are configuration, not credentials, in this single-user
    # deployment, so the UI shows and edits them as ordinary text.
    stored = {'user_id': '', 'proxy': 'http://user:pw@home.example:3128'}
    edited = mask_section('web.rednote', stored)

    assert edited['proxy'] == 'http://user:pw@home.example:3128'

    edited['proxy'] = 'http://other.example:3128'
    merged = unmask_section('web.rednote', edited, stored)

    assert merged['proxy'] == 'http://other.example:3128'


def test_cookiecloud_section_without_configs_normalizes_to_an_empty_list() -> None:
    payload: dict = {}

    assert mask_section('cookiecloud', payload) == {'configs': []}
    assert unmask_section('cookiecloud', payload, {}) == {'configs': []}
    assert payload == {}
