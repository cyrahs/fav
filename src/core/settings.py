"""Database-backed application settings.

Settings live in the ``app_settings`` table, one row per section, and are edited
through the web UI. Every section must be constructible from its defaults so a
freshly provisioned database can boot; per-source requirements are enforced by
``validate_runnable`` only when a source is enabled.
"""

from __future__ import annotations

import json
import math
import re
import threading
from pathlib import Path
from time import monotonic
from typing import Any, Literal, Self

import psycopg
from psycopg.rows import dict_row
from pydantic import BaseModel, Field, field_validator, model_validator

from src.core import logger
from src.core.env import env

_CRON_FIELDS = 5
_TELEGRAM_ACCOUNT_NAME_RE = re.compile(r'^[A-Za-z0-9_-]+$')
TelegramMediaType = Literal['video', 'image']

CREATE_APP_SETTINGS_TABLE_SQL = """
CREATE TABLE IF NOT EXISTS app_settings (
    section    TEXT PRIMARY KEY,
    value      JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);
"""

_CACHE_TTL_SECONDS = 2.0

log = logger.get('settings')


class ScheduleJob(BaseModel):
    cron: str
    # Defaults are off so an unconfigured deployment starts up idle instead of
    # crawling with placeholder credentials.
    enabled: bool = False
    run_on_start: bool = False

    @field_validator('cron')
    @classmethod
    def validate_cron(cls, value: str) -> str:
        # We use standard crontab format: minute hour day month day_of_week.
        if len(value.split()) != _CRON_FIELDS:
            msg = 'cron must contain exactly 5 fields'
            raise ValueError(msg)
        return value

    def validate_runnable(self) -> list[str]:
        """Return the field names that must be filled in before this job can run."""
        return []


class Bilibili(ScheduleJob):
    id: int = 0
    fav_id: int = 0
    path: Path = Path('./collection/bilibili')
    cron: str = '*/30 * * * *'

    def validate_runnable(self) -> list[str]:
        missing: list[str] = []
        if self.id <= 0:
            missing.append('id')
        if self.fav_id <= 0:
            missing.append('fav_id')
        return missing


class CookieCloud(BaseModel):
    server_url: str = ''
    uuid: str = ''
    password: str = ''

    def validate_runnable(self) -> list[str]:
        return [name for name in ('server_url', 'uuid', 'password') if not getattr(self, name).strip()]


def _normalize_telegram_channel_id(channel_id: int) -> int:
    if channel_id < 0 and str(channel_id).startswith('-100'):
        channel_id = int(str(channel_id).removeprefix('-100'))
    if channel_id <= 0:
        msg = 'telegram channel id must be a positive Telethon channel id or a -100 Bot API channel id'
        raise ValueError(msg)
    return channel_id


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
    # Left editable in a half-filled state so the settings form can add an account
    # first and its channels afterwards; completeness is checked by validate_runnable.
    channels: list[TelegramChannel] = Field(default_factory=list)
    api_id: int = 0
    api_hash: str = ''
    session_path: Path = Path('./data/telethon-session')

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
        seen_routes: set[tuple[int, TelegramMediaType]] = set()
        for channel in value:
            for media_type in channel.media_types:
                route = (channel.id, media_type)
                if route in seen_routes:
                    msg = f'duplicate telegram media route for channel {channel.id}: {media_type}'
                    raise ValueError(msg)
                seen_routes.add(route)
        return value

    def channel_routes(self) -> dict[int, dict[TelegramMediaType, TelegramChannel]]:
        routes: dict[int, dict[TelegramMediaType, TelegramChannel]] = {}
        for channel in self.channels:
            media_routes = routes.setdefault(channel.id, {})
            for media_type in channel.media_types:
                media_routes[media_type] = channel
        return routes

    def validate_runnable(self) -> list[str]:
        missing: list[str] = []
        if self.api_id <= 0:
            missing.append('api_id')
        if not self.api_hash.strip():
            missing.append('api_hash')
        if not self.channels:
            missing.append('channels')
        return missing


class Telegram(ScheduleJob):
    accounts: list[TelegramAccount] = Field(default_factory=list)
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

    def validate_runnable(self) -> list[str]:
        if not self.accounts:
            return ['accounts']
        missing: list[str] = []
        for index, account in enumerate(self.accounts):
            missing.extend(f'accounts[{index}].{field}' for field in account.validate_runnable())
        return missing


class Stellasora(ScheduleJob):
    path: Path = Path('./collection/stellasora')
    cron: str = '0 */6 * * *'


class Nikke(ScheduleJob):
    path: Path = Path('./collection/nikke')
    cron: str = '0 */6 * * *'
    runtime_capture_enabled: bool = False
    runtime_capture_timeout_seconds: float = 60.0
    runtime_capture_force_refresh: bool = False


class BD2(ScheduleJob):
    path: Path = Path('./collection/bd2')
    cron: str = '0 */6 * * *'


class AzurLane(ScheduleJob):
    path: Path = Path('./collection/azurlane')
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

    def validate_runnable(self) -> list[str]:
        return [] if self.user_id > 0 else ['user_id']


class KemonoCreator(BaseModel):
    service: str
    id: str
    name: str


class Kemono(ScheduleJob):
    path: Path = Path('./collection/kemono')
    cron: str = '0 */6 * * *'
    creators: list[KemonoCreator] = Field(default_factory=list)

    def validate_runnable(self) -> list[str]:
        return [] if self.creators else ['creators']


class TelegramNotification(BaseModel):
    enabled: bool = False
    bot_token: str = ''
    chat_id: str = ''
    message_thread_id: int | None = None

    @field_validator('bot_token', 'chat_id')
    @classmethod
    def normalize_text(cls, value: str) -> str:
        return value.strip()

    @field_validator('message_thread_id')
    @classmethod
    def validate_message_thread_id(cls, value: int | None) -> int | None:
        if value is not None and value <= 0:
            msg = 'message_thread_id must be a positive integer'
            raise ValueError(msg)
        return value

    @property
    def configured(self) -> bool:
        return bool(self.bot_token and self.chat_id)

    def validate_runnable(self) -> list[str]:
        return [name for name in ('bot_token', 'chat_id') if not getattr(self, name)]


class Notifications(BaseModel):
    telegram: TelegramNotification = Field(default_factory=TelegramNotification)


class Web(BaseModel):
    bilibili: Bilibili = Field(default_factory=Bilibili)
    telegram: Telegram = Field(default_factory=Telegram)
    stellasora: Stellasora = Field(default_factory=Stellasora)
    nikke: Nikke = Field(default_factory=Nikke)
    bd2: BD2 = Field(default_factory=BD2)
    azurlane: AzurLane = Field(default_factory=AzurLane)
    hanime1: Hanime1 = Field(default_factory=Hanime1)
    jandan: Jandan = Field(default_factory=Jandan)
    kemono: Kemono = Field(default_factory=Kemono)


class Settings(BaseModel):
    web: Web = Field(default_factory=Web)
    cookiecloud: CookieCloud = Field(default_factory=CookieCloud)
    notifications: Notifications = Field(default_factory=Notifications)


SECTION_MODELS: dict[str, type[BaseModel]] = {
    'web.bilibili': Bilibili,
    'web.telegram': Telegram,
    'web.stellasora': Stellasora,
    'web.nikke': Nikke,
    'web.bd2': BD2,
    'web.azurlane': AzurLane,
    'web.hanime1': Hanime1,
    'web.jandan': Jandan,
    'web.kemono': Kemono,
    'cookiecloud': CookieCloud,
    'notifications.telegram': TelegramNotification,
}

# Written by the UI, never echoed back in full. See src/api/settings_masking.py.
SENSITIVE_FIELDS: dict[str, tuple[str, ...]] = {
    'web.telegram': ('accounts[].api_hash',),
    'cookiecloud': ('password',),
    'notifications.telegram': ('bot_token',),
}


class UnknownSectionError(ValueError):
    def __init__(self, section: str) -> None:
        super().__init__(f'Unknown settings section: {section}')
        self.section = section


def _section_container(payload: dict[str, Any], section: str) -> dict[str, Any]:
    """Place a section payload at its dotted path inside a Settings-shaped dict."""
    parts = section.split('.')
    cursor = payload
    for part in parts[:-1]:
        cursor = cursor.setdefault(part, {})
    return cursor


def build_settings(rows: dict[str, Any]) -> Settings:
    payload: dict[str, Any] = {}
    for section, value in rows.items():
        if section not in SECTION_MODELS:
            # Forward compatible: ignore sections written by a newer version.
            continue
        _section_container(payload, section)[section.rsplit('.', 1)[-1]] = value
    return Settings.model_validate(payload)


def _connect() -> psycopg.Connection:
    return psycopg.connect(env.postgres_dsn, autocommit=True, row_factory=dict_row)


_table_ready = False
_table_lock = threading.Lock()


def ensure_table_sync(*, force: bool = False) -> None:
    """Create app_settings if needed, tolerating a concurrent creator.

    The worker and the API start together and both reach for this table on a
    fresh database. ``CREATE TABLE IF NOT EXISTS`` is not atomic against another
    transaction doing the same thing, so it can still raise a duplicate-object
    error; that outcome means the table now exists, which is all we need.
    """
    global _table_ready
    with _table_lock:
        if _table_ready and not force:
            return
    try:
        with _connect() as conn, conn.cursor() as cursor:
            cursor.execute(CREATE_APP_SETTINGS_TABLE_SQL)
    except (psycopg.errors.UniqueViolation, psycopg.errors.DuplicateTable):
        log.debug('app_settings was created concurrently by another process')
    with _table_lock:
        _table_ready = True


def _fetch_rows_sync() -> dict[str, Any]:
    ensure_table_sync()
    with _connect() as conn, conn.cursor() as cursor:
        cursor.execute('SELECT section, value FROM app_settings;')
        rows = cursor.fetchall()
    return {str(row['section']): row['value'] for row in rows}


_cache_lock = threading.Lock()
_cache: tuple[float, Settings] | None = None
_override: Settings | None = None


def use(settings: Settings | None) -> None:
    """Pin a snapshot and bypass the database entirely.

    Intended for tests, which construct downloaders without a live PostgreSQL.
    Pass ``None`` to restore normal database-backed loading.
    """
    global _override
    _override = settings
    invalidate_cache()


def load(*, force: bool = False) -> Settings:
    """Return the current settings snapshot.

    Cached for a couple of seconds so a single job run does not re-query the
    database for every field access, while still picking up UI edits promptly.
    """
    global _cache
    if _override is not None:
        return _override
    with _cache_lock:
        cached = _cache
        if not force and cached is not None and monotonic() - cached[0] < _CACHE_TTL_SECONDS:
            return cached[1]
    settings = build_settings(_fetch_rows_sync())
    with _cache_lock:
        _cache = (monotonic(), settings)
    return settings


def invalidate_cache() -> None:
    global _cache
    with _cache_lock:
        _cache = None


def load_section(section: str) -> BaseModel:
    model = SECTION_MODELS.get(section)
    if model is None:
        raise UnknownSectionError(section)
    ensure_table_sync()
    with _connect() as conn, conn.cursor() as cursor:
        cursor.execute('SELECT value FROM app_settings WHERE section = %s;', (section,))
        row = cursor.fetchone()
    return model.model_validate(row['value'] if row else {})


def save_section(section: str, payload: dict[str, Any]) -> BaseModel:
    model = SECTION_MODELS.get(section)
    if model is None:
        raise UnknownSectionError(section)
    validated = model.model_validate(payload)
    serialized = json.loads(validated.model_dump_json())
    ensure_table_sync()
    with _connect() as conn, conn.cursor() as cursor:
        cursor.execute(
            """
            INSERT INTO app_settings (section, value, updated_at)
            VALUES (%s, %s, CURRENT_TIMESTAMP)
            ON CONFLICT (section) DO UPDATE
            SET value = EXCLUDED.value,
                updated_at = CURRENT_TIMESTAMP;
            """,
            (section, json.dumps(serialized, ensure_ascii=False)),
        )
    invalidate_cache()
    return validated


def settings_version_sync() -> str:
    ensure_table_sync()
    with _connect() as conn, conn.cursor() as cursor:
        cursor.execute('SELECT MAX(updated_at) AS version FROM app_settings;')
        row = cursor.fetchone()
    version = row['version'] if row else None
    return '' if version is None else version.isoformat()
