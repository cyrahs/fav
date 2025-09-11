from __future__ import annotations

from pathlib import Path  # noqa: TC003

from pydantic import BaseModel
from pydantic_settings import BaseSettings, SettingsConfigDict, TomlConfigSettingsSource

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


class Config(BaseSettings):
    proxy: str
    bilibili: Bilibili
    tx: Tx
    cloudflare: Cloudflare
    cookiecloud: CookieCloud

    model_config = SettingsConfigDict(toml_file='config.toml')

    @classmethod
    def settings_customise_sources(cls, s, **_):  # noqa: ANN001, ANN003, ANN206
        return (TomlConfigSettingsSource(s),)


log = logger.get('config')

config = Config()
