import math
import re
from pathlib import Path
from typing import Literal, Self

from pydantic import AliasChoices, BaseModel, Field, field_validator, model_validator
from pydantic_settings import BaseSettings, PydanticBaseSettingsSource, SettingsConfigDict, TomlConfigSettingsSource

from . import logger

_CRON_FIELDS = 5
_TELEGRAM_ACCOUNT_NAME_RE = re.compile(r'^[A-Za-z0-9_-]+$')
TelegramMediaType = Literal['video', 'image']


class ScheduleJob(BaseModel):
    cron: str
    enabled: bool = True
    run_on_start: bool = False

    @field_validator('cron')
    @classmethod
    def validate_cron(cls, value: str) -> str:
        # We use standard crontab format: minute hour day month day_of_week.
        if len(value.split()) != _CRON_FIELDS:
            msg = 'cron must contain exactly 5 fields'
            raise ValueError(msg)
        return value


class Bilibili(ScheduleJob):
    id: int
    fav_id: int
    path: Path
    cron: str = '*/30 * * * *'


class CookieCloud(BaseModel):
    server_url: str
    uuid: str
    password: str


def _normalize_telegram_channel_id(channel_id: int) -> int:
    if channel_id < 0 and str(channel_id).startswith('-100'):
        channel_id = int(str(channel_id).removeprefix('-100'))
    if channel_id <= 0:
        msg = 'telegram channel id must be a positive Telethon channel id or a -100 Bot API channel id'
        raise ValueError(msg)
    return channel_id


def _dedupe_channels(value: list['TelegramChannel']) -> list['TelegramChannel']:
    deduped: list[TelegramChannel] = []
    seen_ids: set[int] = set()
    for channel in value:
        channel_id = channel.id
        if channel_id in seen_ids:
            continue
        seen_ids.add(channel_id)
        deduped.append(channel)
    return deduped


def _dedupe_telegram_media_types(value: list[TelegramMediaType]) -> list[TelegramMediaType]:
    deduped: list[TelegramMediaType] = []
    for media_type in value:
        if media_type in deduped:
            continue
        deduped.append(media_type)
    return deduped


class TelegramChannel(BaseModel):
    id: int
    path: Path
    media_types: list[TelegramMediaType] = Field(default_factory=lambda: ['video'])

    @field_validator('id')
    @classmethod
    def normalize_id(cls, value: int) -> int:
        return _normalize_telegram_channel_id(value)

    @field_validator('media_types')
    @classmethod
    def validate_media_types(cls, value: list[TelegramMediaType]) -> list[TelegramMediaType]:
        deduped = _dedupe_telegram_media_types(value)
        if not deduped:
            msg = 'telegram channel media_types cannot be empty'
            raise ValueError(msg)
        return deduped


class TelegramAccount(BaseModel):
    name: str
    channels: list[TelegramChannel]
    api_id: int
    api_hash: str
    session_path: Path

    @field_validator('name')
    @classmethod
    def normalize_name(cls, value: str) -> str:
        normalized = value.strip()
        if not normalized:
            msg = 'telegram account name cannot be empty'
            raise ValueError(msg)
        if not _TELEGRAM_ACCOUNT_NAME_RE.fullmatch(normalized):
            msg = 'telegram account name must contain only ASCII letters, digits, underscores, or hyphens'
            raise ValueError(msg)
        return normalized

    @field_validator('channels')
    @classmethod
    def validate_channels(cls, value: list[TelegramChannel]) -> list[TelegramChannel]:
        deduped = _dedupe_channels(value)
        if not deduped:
            msg = 'telegram account channels cannot be empty'
            raise ValueError(msg)
        return deduped

    @field_validator('api_hash')
    @classmethod
    def normalize_api_hash(cls, value: str) -> str:
        normalized = value.strip()
        if not normalized:
            msg = 'telegram account api_hash cannot be empty'
            raise ValueError(msg)
        return normalized


class Telegram(ScheduleJob):
    accounts: list[TelegramAccount]
    cron: str = '*/30 * * * *'
    scan_limit: int = 50
    download_limit_per_channel: int = 2
    download_delay_seconds: float = 60.0
    channel_cooldown_seconds: float = 1800.0
    history_wait_seconds: float = 1.0
    flood_sleep_threshold_seconds: int = 300

    @field_validator('scan_limit', 'download_limit_per_channel')
    @classmethod
    def validate_positive_limit(cls, value: int) -> int:
        if value < 1:
            msg = 'telegram limits must be greater than or equal to 1'
            raise ValueError(msg)
        return value

    @field_validator('download_delay_seconds', 'channel_cooldown_seconds', 'history_wait_seconds')
    @classmethod
    def validate_non_negative_seconds(cls, value: float) -> float:
        if not math.isfinite(value) or value < 0:
            msg = 'telegram delays must be finite and greater than or equal to 0'
            raise ValueError(msg)
        return value

    @field_validator('flood_sleep_threshold_seconds')
    @classmethod
    def validate_flood_sleep_threshold_seconds(cls, value: int) -> int:
        if value < 0:
            msg = 'telegram flood_sleep_threshold_seconds must be greater than or equal to 0'
            raise ValueError(msg)
        return value

    @model_validator(mode='after')
    def validate_accounts(self) -> Self:
        if not self.accounts:
            msg = 'web.telegram.accounts cannot be empty'
            raise ValueError(msg)
        account_names: set[str] = set()
        for account in self.accounts:
            normalized = account.name.casefold()
            if normalized in account_names:
                msg = f'duplicate telegram account name: {account.name}'
                raise ValueError(msg)
            account_names.add(normalized)
        return self

    def resolved_accounts(self) -> list[TelegramAccount]:
        return self.accounts


class Stellasora(ScheduleJob):
    path: Path = Path('./collection/stellasora')
    cron: str = '0 */6 * * *'


class Hanime1Ranking(BaseModel):
    enabled: bool = False
    periods: list[Literal['weekly', 'monthly']] = Field(default_factory=lambda: ['weekly', 'monthly'])
    pages: int = 1

    @field_validator('periods')
    @classmethod
    def validate_periods(cls, value: list[Literal['weekly', 'monthly']]) -> list[Literal['weekly', 'monthly']]:
        if not value:
            msg = 'periods cannot be empty'
            raise ValueError(msg)
        deduped: list[Literal['weekly', 'monthly']] = []
        for period in value:
            if period in deduped:
                continue
            deduped.append(period)
        return deduped

    @field_validator('pages')
    @classmethod
    def validate_pages(cls, value: int) -> int:
        if value < 1:
            msg = 'pages must be greater than or equal to 1'
            raise ValueError(msg)
        return value


class Hanime1(ScheduleJob):
    path: Path = Path('./collection/hanime1')
    host: str = 'https://hanime1.me'
    cron: str = '0 */6 * * *'
    user_lang: Literal['zhs', 'zht'] = 'zhs'
    ranking: Hanime1Ranking = Field(default_factory=Hanime1Ranking)


class Jandan(ScheduleJob):
    path: Path = Path('./collection/jandan')
    api_url: str = 'https://joiningss.com/jd/api'
    user_id: int = 0
    fav_types: list[int] = Field(default_factory=lambda: [1, 2, 6])
    fav_num_limit: int = 45
    cron: str = '0 */6 * * *'

    @field_validator('fav_types')
    @classmethod
    def validate_fav_types(cls, value: list[int]) -> list[int]:
        allowed = {1, 2, 6}
        if not value:
            msg = 'fav_types cannot be empty'
            raise ValueError(msg)
        if any(fav_type not in allowed for fav_type in value):
            msg = f'fav_types must be in {sorted(allowed)}'
            raise ValueError(msg)
        deduped: list[int] = []
        for fav_type in value:
            if fav_type in deduped:
                continue
            deduped.append(fav_type)
        return deduped


class KemonoCreator(BaseModel):
    service: str
    id: str
    name: str


class Kemono(BaseModel):
    enabled: bool = False
    cron: str = '0 */6 * * *'
    path: Path
    creators: list[KemonoCreator] = []

    @field_validator('cron')
    @classmethod
    def validate_cron(cls, value: str) -> str:
        if len(value.split()) != _CRON_FIELDS:
            msg = 'cron must contain exactly 5 fields'
            raise ValueError(msg)
        return value


class Web(BaseModel):
    bilibili: Bilibili
    telegram: Telegram
    stellasora: Stellasora = Field(default_factory=Stellasora)
    hanime1: Hanime1 = Field(default_factory=Hanime1)
    jandan: Jandan = Field(default_factory=Jandan)
    kemono: Kemono


class Database(BaseModel):
    postgres_dsn: str = Field(validation_alias=AliasChoices('postgres_dsn', 'dsn', 'url'))

    @field_validator('postgres_dsn')
    @classmethod
    def normalize_postgres_dsn(cls, value: str) -> str:
        normalized = value.strip()
        if not normalized:
            msg = 'database.postgres_dsn cannot be empty'
            raise ValueError(msg)
        return normalized


class Api(BaseModel):
    token: str = ''
    bind: str = '127.0.0.1'
    port: int = 8091

    @field_validator('token')
    @classmethod
    def normalize_token(cls, value: str) -> str:
        return value.strip()

    @field_validator('bind')
    @classmethod
    def normalize_bind(cls, value: str) -> str:
        normalized = value.strip()
        return normalized or '127.0.0.1'

    @field_validator('port')
    @classmethod
    def validate_port(cls, value: int) -> int:
        max_port = 65535
        if not (0 < value <= max_port):
            msg = f'api.port must be between 1 and {max_port}'
            raise ValueError(msg)
        return value


class Notifications(BaseModel):
    webhook_base_url: str = ''
    webhook_token: str = ''

    @field_validator('webhook_base_url')
    @classmethod
    def normalize_webhook_base_url(cls, value: str) -> str:
        return value.strip().rstrip('/')

    @field_validator('webhook_token')
    @classmethod
    def normalize_webhook_token(cls, value: str) -> str:
        return value.strip()


class Config(BaseSettings):
    proxy: str
    run_config: Path = Path('./data/config.json')
    web: Web
    database: Database
    api: Api = Field(default_factory=Api)
    notifications: Notifications = Field(default_factory=Notifications)
    cookiecloud: CookieCloud

    model_config = SettingsConfigDict(
        toml_file='./config.toml',
        env_file='.env',
        env_nested_delimiter='__',
        extra='ignore',
    )

    @classmethod
    def settings_customise_sources(
        cls,
        settings_cls: type[BaseSettings],
        init_settings: PydanticBaseSettingsSource,
        env_settings: PydanticBaseSettingsSource,
        dotenv_settings: PydanticBaseSettingsSource,
        file_secret_settings: PydanticBaseSettingsSource,
    ) -> tuple[PydanticBaseSettingsSource, ...]:
        return (
            init_settings,
            env_settings,
            dotenv_settings,
            TomlConfigSettingsSource(settings_cls),
            file_secret_settings,
        )


log = logger.get('config')

config = Config()
