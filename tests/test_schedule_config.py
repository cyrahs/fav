# ruff: noqa: INP001, S101, S105, S106

from pathlib import Path

import pytest
from pydantic import ValidationError

from src.core.settings import (
    Hanime1Ranking,
    Hanime1RankingDeepScan,
    ScheduleJob,
    Telegram,
    TelegramAccount,
    TelegramChannel,
    TelegramNotification,
)

_TELEGRAM_DEFAULT_SCAN_LIMIT = 50
_TELEGRAM_DEFAULT_DOWNLOAD_LIMIT_PER_CHANNEL = 2
_TELEGRAM_DEFAULT_DOWNLOAD_DELAY_SECONDS = 60.0
_TELEGRAM_DEFAULT_CHANNEL_COOLDOWN_SECONDS = 1800.0
_TELEGRAM_DEFAULT_HISTORY_WAIT_SECONDS = 1.0
_TELEGRAM_DEFAULT_FLOOD_SLEEP_THRESHOLD_SECONDS = 300
_DEEP_SCAN_DEFAULT_QUOTA = 0.25
_DEEP_SCAN_DEFAULT_MAX_EXTRA_PAGES = 5
_DEEP_SCAN_FRACTIONAL_QUOTA = 0.5


def test_schedule_job_accepts_five_field_cron() -> None:
    job = ScheduleJob(cron='*/5 * * * *')

    assert job.cron == '*/5 * * * *'
    # Defaults are off so an unconfigured deployment boots idle.
    assert job.enabled is False


def test_schedule_job_rejects_non_five_field_cron() -> None:
    with pytest.raises(ValidationError):
        ScheduleJob(cron='*/5 * * *')


def test_telegram_notification_requires_credentials_to_be_configured() -> None:
    cfg = TelegramNotification(enabled=True, bot_token=' 123:token ', chat_id=' -100123 ', message_thread_id=42)

    assert cfg.configured is True
    assert cfg.bot_token == '123:token'
    assert cfg.chat_id == '-100123'


def test_telegram_notification_rejects_invalid_message_thread_id() -> None:
    with pytest.raises(ValidationError):
        TelegramNotification(message_thread_id=0)


def test_telegram_notification_reports_missing_credentials() -> None:
    assert TelegramNotification(enabled=True).validate_runnable() == ['bot_token', 'chat_id']


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


def test_hanime1_ranking_deep_scan_defaults_off() -> None:
    ranking = Hanime1Ranking()

    assert ranking.deep_scan.enabled is False
    assert ranking.deep_scan.quota == _DEEP_SCAN_DEFAULT_QUOTA
    assert ranking.deep_scan.max_extra_pages == _DEEP_SCAN_DEFAULT_MAX_EXTRA_PAGES


def test_hanime1_ranking_deep_scan_accepts_fractional_quota() -> None:
    deep_scan = Hanime1RankingDeepScan(enabled=True, quota=_DEEP_SCAN_FRACTIONAL_QUOTA, max_extra_pages=3)

    assert deep_scan.quota == _DEEP_SCAN_FRACTIONAL_QUOTA


@pytest.mark.parametrize('quota', [0.0, -1.0, float('inf'), float('nan')])
def test_hanime1_ranking_deep_scan_rejects_invalid_quota(quota: float) -> None:
    with pytest.raises(ValidationError):
        Hanime1RankingDeepScan(quota=quota)


def test_hanime1_ranking_deep_scan_rejects_non_positive_max_extra_pages() -> None:
    with pytest.raises(ValidationError):
        Hanime1RankingDeepScan(max_extra_pages=0)


def test_telegram_reports_empty_accounts_as_not_runnable() -> None:
    # The settings form must be able to hold a half-filled section, so an empty
    # account list validates but is reported as not runnable.
    cfg = Telegram(accounts=[])

    assert cfg.validate_runnable() == ['accounts']


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


def test_telegram_download_safety_defaults() -> None:
    cfg = Telegram(
        accounts=[
            TelegramAccount(
                name='main',
                channels=[TelegramChannel(id=1, path=Path('./collection/telegram/main'))],
                api_id=123,
                api_hash='hash-1',
                session_path=Path('./data/main'),
            ),
        ],
    )

    assert cfg.scan_limit == _TELEGRAM_DEFAULT_SCAN_LIMIT
    assert cfg.download_limit_per_channel == _TELEGRAM_DEFAULT_DOWNLOAD_LIMIT_PER_CHANNEL
    assert cfg.download_delay_seconds == _TELEGRAM_DEFAULT_DOWNLOAD_DELAY_SECONDS
    assert cfg.channel_cooldown_seconds == _TELEGRAM_DEFAULT_CHANNEL_COOLDOWN_SECONDS
    assert cfg.history_wait_seconds == _TELEGRAM_DEFAULT_HISTORY_WAIT_SECONDS
    assert cfg.flood_sleep_threshold_seconds == _TELEGRAM_DEFAULT_FLOOD_SLEEP_THRESHOLD_SECONDS


def test_telegram_rejects_invalid_download_safety_values() -> None:
    account = TelegramAccount(
        name='main',
        channels=[TelegramChannel(id=1, path=Path('./collection/telegram/main'))],
        api_id=123,
        api_hash='hash-1',
        session_path=Path('./data/main'),
    )

    with pytest.raises(ValidationError):
        Telegram(accounts=[account], scan_limit=0)
    with pytest.raises(ValidationError):
        Telegram(accounts=[account], download_limit_per_channel=0)
    with pytest.raises(ValidationError):
        Telegram(accounts=[account], download_delay_seconds=-1)
    with pytest.raises(ValidationError):
        Telegram(accounts=[account], download_delay_seconds=float('nan'))
    with pytest.raises(ValidationError):
        Telegram(accounts=[account], channel_cooldown_seconds=float('inf'))
    with pytest.raises(ValidationError):
        Telegram(accounts=[account], channel_cooldown_seconds=-1)
    with pytest.raises(ValidationError):
        Telegram(accounts=[account], history_wait_seconds=-1)
    with pytest.raises(ValidationError):
        Telegram(accounts=[account], flood_sleep_threshold_seconds=-1)


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


def test_telegram_account_supports_split_media_routes() -> None:
    account = TelegramAccount(
        name='main',
        channels=[
            TelegramChannel(id=3942401424, path=Path('./collection/image'), media_types=['image']),
            TelegramChannel(id=3942401424, path=Path('./collection/video'), media_types=['video']),
        ],
        api_id=123,
        api_hash='hash',
        session_path=Path('./data/main'),
    )

    assert [channel.media_types for channel in account.channels] == [['image'], ['video']]
    routes = account.channel_routes()
    assert list(routes) == [3942401424]
    assert routes[3942401424]['image'].path == Path('./collection/image')
    assert routes[3942401424]['video'].path == Path('./collection/video')


def test_telegram_account_rejects_duplicate_media_route() -> None:
    with pytest.raises(ValidationError, match='duplicate telegram media route for channel 1: image'):
        TelegramAccount(
            name='main',
            channels=[
                TelegramChannel(id=1, path=Path('./collection/one'), media_types=['image']),
                TelegramChannel(id=1, path=Path('./collection/two'), media_types=['image']),
            ],
            api_id=123,
            api_hash='hash',
            session_path=Path('./data/main'),
        )


def test_telegram_account_checks_routes_after_bot_api_id_normalization() -> None:
    with pytest.raises(ValidationError, match='duplicate telegram media route for channel 2522897097: video'):
        TelegramAccount(
            name='main',
            channels=[
                TelegramChannel(id=-1002522897097, path=Path('./collection/one')),
                TelegramChannel(id=2522897097, path=Path('./collection/two')),
            ],
            api_id=123,
            api_hash='hash',
            session_path=Path('./data/main'),
        )
