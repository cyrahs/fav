from __future__ import annotations

from pathlib import Path  # noqa: TC003
from typing import TYPE_CHECKING, Any

from pydantic import BaseModel
from pydantic_settings import BaseSettings, SettingsConfigDict, TomlConfigSettingsSource

from . import logger

if TYPE_CHECKING:
    from pydantic_settings.sources import PydanticBaseSettingsSource


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
