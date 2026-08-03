# ruff: noqa: INP001, S101

from src.api.settings_masking import MASK_SUFFIX, mask_section, unmask_section


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
