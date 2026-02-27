from pathlib import Path
from typing import Literal

from pydantic import AliasChoices, BaseModel, Field, field_validator
from pydantic_settings import BaseSettings, PydanticBaseSettingsSource, SettingsConfigDict, TomlConfigSettingsSource

from . import logger

_CRON_FIELDS = 5


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


class Telegram(ScheduleJob):
    channels: list[int]
    api_id: int
    api_hash: str
    path: Path
    session_path: Path
    cron: str = '*/30 * * * *'


class TelegramBot(BaseModel):
    token: str
    chat_id: int | str
    api_base: str = 'https://api.telegram.org'
    message_thread_id: int | None = None


class Stellasora(ScheduleJob):
    path: Path = Path('./collection/stellasora')
    cron: str = '0 */6 * * *'


class Hanime1(ScheduleJob):
    path: Path = Path('./collection/hanime1')
    host: str = 'https://hanime1.me'
    cron: str = '0 */6 * * *'
    user_lang: Literal['zhs', 'zht'] = 'zhs'


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


class Config(BaseSettings):
    proxy: str
    run_config: Path = Path('./data/config.json')
    web: Web
    database: Database
    cookiecloud: CookieCloud
    telegram_bot: TelegramBot

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
