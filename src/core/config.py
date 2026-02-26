from pathlib import Path
from typing import Any, Literal

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


class Tangxin(ScheduleJob):
    path: Path
    host: str
    cron: str = '*/30 * * * *'


class Cloudflare(BaseModel):
    account_id: str
    api_key: str
    d1_id: str
    kv_id: dict[str, str]


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
    tangxin: Tangxin = Field(validation_alias=AliasChoices('tangxin', 'tx'))
    telegram: Telegram
    stellasora: Stellasora = Field(default_factory=Stellasora)
    hanime1: Hanime1 = Field(default_factory=Hanime1)
    kemono: Kemono

    @property
    def tx(self) -> Tangxin:
        # Backward-compatible alias for older call sites.
        return self.tangxin


class Config(BaseSettings):
    proxy: str
    run_config: Path = Path('./data/config.json')
    web: Web
    cloudflare: Cloudflare
    cookiecloud: CookieCloud
    telegram_bot: TelegramBot

    model_config = SettingsConfigDict(toml_file='./config.toml', extra='ignore')

    @classmethod
    def settings_customise_sources(cls, settings_cls: type[BaseSettings], *_: Any, **__: Any) -> tuple[PydanticBaseSettingsSource, ...]:
        return (TomlConfigSettingsSource(settings_cls),)


log = logger.get('config')

config = Config()
