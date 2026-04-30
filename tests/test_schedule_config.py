# ruff: noqa: INP001, S101

from pathlib import Path

import pytest
from pydantic import ValidationError

from src.core.config import Hanime1Ranking, ScheduleJob, Telegram, TelegramAccount, TelegramChannel


def test_schedule_job_accepts_five_field_cron() -> None:
    job = ScheduleJob(cron='*/5 * * * *')

    assert job.cron == '*/5 * * * *'
    assert job.enabled is True
    assert job.run_on_start is False


def test_schedule_job_rejects_non_five_field_cron() -> None:
    with pytest.raises(ValidationError):
        ScheduleJob(cron='*/5 * * *')


def test_hanime1_ranking_dedupes_periods() -> None:
    ranking = Hanime1Ranking(enabled=True, periods=['weekly', 'weekly', 'monthly'], pages=1)

    assert ranking.enabled is True
    assert ranking.periods == ['weekly', 'monthly']
    assert ranking.pages == 1


def test_hanime1_ranking_rejects_empty_periods() -> None:
    with pytest.raises(ValidationError):
        Hanime1Ranking(periods=[])


def test_hanime1_ranking_rejects_non_positive_pages() -> None:
    with pytest.raises(ValidationError):
        Hanime1Ranking(pages=0)


def test_telegram_rejects_empty_accounts() -> None:
    with pytest.raises(ValidationError):
        Telegram(accounts=[])


def test_telegram_accounts_config_resolves_named_accounts() -> None:
    cfg = Telegram(
        accounts=[
            TelegramAccount(
                name='main',
                channels=[TelegramChannel(id=1, path=Path('./collection/telegram/main'))],
                api_id=123,
                api_hash='hash-1',
                session_path=Path('./data/main'),
            ),
            TelegramAccount(
                name='alt',
                channels=[TelegramChannel(id=2, path=Path('./collection/telegram/alt'))],
                api_id=456,
                api_hash='hash-2',
                session_path=Path('./data/alt'),
            ),
        ],
    )

    accounts = cfg.resolved_accounts()

    assert [account.name for account in accounts] == ['main', 'alt']
    assert [account.channels[0].id for account in accounts] == [1, 2]
    assert [account.channels[0].path for account in accounts] == [Path('./collection/telegram/main'), Path('./collection/telegram/alt')]


def test_telegram_channel_defaults_to_video_media_type() -> None:
    channel = TelegramChannel(id=1, path=Path('./collection/telegram/main'))

    assert channel.media_types == ['video']


def test_telegram_channel_accepts_image_media_type_and_dedupes() -> None:
    channel = TelegramChannel(id=1, path=Path('./collection/telegram/main'), media_types=['video', 'image', 'image'])

    assert channel.media_types == ['video', 'image']


def test_telegram_channel_rejects_empty_media_types() -> None:
    with pytest.raises(ValidationError):
        TelegramChannel(id=1, path=Path('./collection/telegram/main'), media_types=[])


def test_telegram_channel_rejects_unknown_media_type() -> None:
    with pytest.raises(ValidationError):
        TelegramChannel(id=1, path=Path('./collection/telegram/main'), media_types=['photo'])


def test_telegram_rejects_duplicate_account_names() -> None:
    with pytest.raises(ValidationError):
        Telegram(
            accounts=[
                TelegramAccount(
                    name='main',
                    channels=[TelegramChannel(id=1, path=Path('./collection/telegram/main'))],
                    api_id=123,
                    api_hash='hash-1',
                    session_path=Path('./data/main'),
                ),
                TelegramAccount(
                    name='MAIN',
                    channels=[TelegramChannel(id=2, path=Path('./collection/telegram/alt'))],
                    api_id=456,
                    api_hash='hash-2',
                    session_path=Path('./data/alt'),
                ),
            ],
        )


def test_telegram_rejects_unsafe_account_name() -> None:
    with pytest.raises(ValidationError):
        TelegramAccount(
            name='..',
            channels=[TelegramChannel(id=1, path=Path('./collection/telegram/main'))],
            api_id=123,
            api_hash='hash',
            session_path=Path('./data/main'),
        )


def test_telegram_normalizes_bot_api_channel_ids() -> None:
    account = TelegramAccount(
        name='main',
        channels=[
            TelegramChannel(id=-1002522897097, path=Path('./collection/telegram/main')),
            TelegramChannel(id=2522897097, path=Path('./collection/telegram/main')),
        ],
        api_id=123,
        api_hash='hash',
        session_path=Path('./data/main'),
    )

    assert [channel.id for channel in account.channels] == [2522897097]
