from pathlib import Path
from typing import Any

from pydantic import BaseModel
from pydantic_settings import BaseSettings, PydanticBaseSettingsSource, SettingsConfigDict, TomlConfigSettingsSource

from . import logger


class Bilibili(BaseModel):
    id: int
    fav_id: int
    path: Path


class Tx(BaseModel):
    path: Path
    host: str


class Cloudflare(BaseModel):
    account_id: str
    api_key: str
    d1_id: str
    kv_id: dict[str, str]


class CookieCloud(BaseModel):
    server_url: str
    uuid: str
    password: str


class Telegram(BaseModel):
    channels: list[int]
    api_id: int
    api_hash: str
    path: Path
    session_path: Path


class Config(BaseSettings):
    proxy: str
    bilibili: Bilibili
    tx: Tx
    cloudflare: Cloudflare
    cookiecloud: CookieCloud
    telegram: Telegram

    model_config = SettingsConfigDict(toml_file='./data/config.toml')

    @classmethod
    def settings_customise_sources(cls, settings_cls: type[BaseSettings], *_: Any, **__: Any) -> tuple[PydanticBaseSettingsSource, ...]:
        return (TomlConfigSettingsSource(settings_cls),)


log = logger.get('config')

config = Config()
