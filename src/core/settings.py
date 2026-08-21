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
from pydantic import BaseModel, Field, ValidationInfo, field_validator, model_validator

from src.core import logger
from src.core.env import env

_CRON_FIELDS = 5
_ACCOUNT_NAME_RE = re.compile(r'^[A-Za-z0-9_-]+$')
_TWITTER_USERNAME_RE = re.compile(r'^[A-Za-z0-9_]+$')
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
    # Notifications default on: a source that runs is expected to report what it
    # found, so staying quiet is the deliberate choice rather than the accident.
    notify: bool = True

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


class CookieCloud(BaseModel):
    server_url: str = ''
    uuid: str = ''
    password: str = ''

    def validate_runnable(self) -> list[str]:
        return [name for name in ('server_url', 'uuid', 'password') if not getattr(self, name).strip()]

    @property
    def configured(self) -> bool:
        return not self.validate_runnable()


class BilibiliFavorite(BaseModel):
    """One favourite list and the directory its videos land in."""

    fav_id: int
    path: Path

    @field_validator('fav_id')
    @classmethod
    def validate_fav_id(cls, value: int) -> int:
        if value <= 0:
            msg = 'bilibili fav_id must be a positive favourite list id'
            raise ValueError(msg)
        return value


class BilibiliAccount(BaseModel):
    """One logged-in Bilibili account, its favourite lists, and its watch-later list.

    Each account carries its own CookieCloud credentials -- they are what identifies
    the account, since Bilibili's watch-later list is always the logged-in user's.
    """

    name: str
    favorites: list[BilibiliFavorite] = Field(default_factory=list)
    toview_enabled: bool = False
    toview_path: Path = Path('./collection/bilibili/toview')
    cookiecloud: CookieCloud = Field(default_factory=CookieCloud)

    @field_validator('name')
    @classmethod
    def normalize_name(cls, value: str) -> str:
        normalized = value.strip()
        if not normalized:
            msg = 'bilibili account name cannot be empty'
            raise ValueError(msg)
        if not _ACCOUNT_NAME_RE.fullmatch(normalized):
            msg = 'bilibili account name must contain only ASCII letters, digits, underscores, or hyphens'
            raise ValueError(msg)
        return normalized

    @field_validator('favorites')
    @classmethod
    def validate_favorites(cls, value: list[BilibiliFavorite]) -> list[BilibiliFavorite]:
        seen: set[int] = set()
        for favorite in value:
            if favorite.fav_id in seen:
                msg = f'duplicate bilibili favourite list id: {favorite.fav_id}'
                raise ValueError(msg)
            seen.add(favorite.fav_id)
        return value

    def validate_runnable(self) -> list[str]:
        missing = [f'cookiecloud.{name}' for name in self.cookiecloud.validate_runnable()]
        # An account that collects neither favourites nor watch-later has nothing to do.
        if not self.favorites and not self.toview_enabled:
            missing.append('favorites')
        return missing


class Bilibili(ScheduleJob):
    cron: str = '*/30 * * * *'
    # Egress for the yt-dlp downloads only; the favourites API stays direct. Bilibili
    # assigns the CDN mirror by egress IP, and from a US address the playurl sometimes
    # lands on the Akamai overseas mirror, which throttles each connection to ~1MB/s
    # server-side. A Hong Kong exit consistently gets the fast Tencent COS mirror, so
    # the point of this proxy is steering the mirror assignment, not a faster route.
    proxy: str = ''
    accounts: list[BilibiliAccount] = Field(default_factory=list)

    @field_validator('proxy')
    @classmethod
    def normalize_proxy(cls, value: str) -> str:
        return value.strip()

    @model_validator(mode='after')
    def validate_accounts(self) -> Self:
        account_names: set[str] = set()
        for account in self.accounts:
            normalized = account.name.casefold()
            if normalized in account_names:
                msg = f'duplicate bilibili account name: {account.name}'
                raise ValueError(msg)
            account_names.add(normalized)
        return self

    def validate_runnable(self) -> list[str]:
        if not self.accounts:
            return ['accounts']
        missing: list[str] = []
        for index, account in enumerate(self.accounts):
            missing.extend(f'accounts[{index}].{field_name}' for field_name in account.validate_runnable())
        return missing


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
        if not _ACCOUNT_NAME_RE.fullmatch(normalized):
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
    # The l2d.su origin blocks datacenter IPs outright, so its index and per-ship detail
    # requests can be routed through a proxy. Assets live on a CDN and never use it.
    origin_proxy: str = ''
    # Spacing between l2d.su origin requests. With a rotating proxy every request already gets
    # its own exit IP, so this paces the aggregate load on the origin rather than protecting any
    # single address. Below the round-trip time (~2s through a proxy) it stops having an effect.
    origin_request_interval_seconds: float = 1.0

    @field_validator('origin_proxy')
    @classmethod
    def normalize_origin_proxy(cls, value: str) -> str:
        return value.strip()

    @field_validator('origin_request_interval_seconds')
    @classmethod
    def validate_origin_request_interval(cls, value: float) -> float:
        if value < 0:
            msg = 'origin_request_interval_seconds cannot be negative'
            raise ValueError(msg)
        return value

    def validate_runnable(self) -> list[str]:
        # The l2d.su origin null-routes datacenter IPs, so without a residential proxy the
        # crawler cannot read the catalog at all and would silently preserve stale state.
        return [] if self.origin_proxy else ['origin_proxy']


class Hanime1RankingDeepScan(BaseModel):
    enabled: bool = False
    quota: float = 0.25
    max_extra_pages: int = 5

    @field_validator('quota')
    @classmethod
    def validate_quota(cls, value: float) -> float:
        if not math.isfinite(value) or value <= 0:
            msg = 'quota must be finite and greater than 0'
            raise ValueError(msg)
        return value

    @field_validator('max_extra_pages')
    @classmethod
    def validate_max_extra_pages(cls, value: int) -> int:
        if value < 1:
            msg = 'max_extra_pages must be greater than or equal to 1'
            raise ValueError(msg)
        return value


class Hanime1Ranking(BaseModel):
    enabled: bool = False
    periods: list[Literal['weekly', 'monthly']] = Field(default_factory=lambda: ['weekly', 'monthly'])
    pages: int = 1
    deep_scan: Hanime1RankingDeepScan = Field(default_factory=Hanime1RankingDeepScan)

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
    # kemono.party -> .su -> .cr -> dead; pawchive already advertises .st beside .pw.
    # The next domain move should be a settings edit, not a deploy.
    base_url: str = 'https://pawchive.pw'
    # Attachments live on a separate host (the main domain 404s on /data paths).
    file_base_url: str = 'https://file.pawchive.pw'
    # Applied before every API and file request. Raise it via settings if the site
    # starts throttling; no redeploy needed.
    sleep_request_seconds: float = 1.0
    creators: list[KemonoCreator] = Field(default_factory=list)

    @field_validator('base_url', 'file_base_url')
    @classmethod
    def normalize_base_url(cls, value: str) -> str:
        normalized = value.strip().rstrip('/')
        if not normalized.startswith(('http://', 'https://')):
            msg = 'base URLs must start with http:// or https://'
            raise ValueError(msg)
        return normalized

    @field_validator('sleep_request_seconds')
    @classmethod
    def validate_sleep_request_seconds(cls, value: float) -> float:
        if value < 0:
            msg = 'sleep_request_seconds cannot be negative'
            raise ValueError(msg)
        return value

    def validate_runnable(self) -> list[str]:
        return [] if self.creators else ['creators']


class Twitter(ScheduleJob):
    """Liked tweets on X, crawled through gallery-dl.

    X has no affordable API for reading likes, so the crawl runs gallery-dl against
    ``x.com/<username>/likes`` with the browser session this account is logged in
    with. The session comes from CookieCloud so it tracks the browser instead of
    going stale in the database.
    """

    path: Path = Path('./collection/twitter')
    # Where videos and GIFs land. Left unset they stay with the images under ``path``;
    # set it to keep them on a different disk. X stores GIFs as videos, so they follow
    # this too. See src/web/twitter.py for why the split happens after the download.
    video_path: Path | None = None
    cron: str = '0 */6 * * *'
    # Own screen name, without the leading @. The likes timeline is only readable
    # for the logged-in user, so this has to match the CookieCloud session.
    username: str = ''
    cookiecloud: CookieCloud = Field(default_factory=CookieCloud)
    # Spacing between X API requests. X throttles hard on the likes timeline; below
    # about a second a large backfill starts collecting 429s.
    sleep_request_seconds: float = 2.0
    # Consecutive already-archived files that end an incremental run. Only applies
    # once the first full backfill has finished; see src/web/twitter.py.
    abort_after: int = 20
    proxy: str = ''
    # A liked retweet is still a liked post, so retweets are kept by default and
    # filed under the original author.
    include_retweets: bool = True
    # Videos and GIFs as well as photos. This is gallery-dl's own default, stated
    # here so that turning it off is a setting rather than a code change.
    include_videos: bool = True

    @field_validator('video_path', mode='before')
    @classmethod
    def normalize_video_path(cls, value: object) -> object:
        # The form sends '' for "keep them with the images". Path('') is Path('.'),
        # which would quietly route every video into the working directory.
        if isinstance(value, str) and not value.strip():
            return None
        return value

    @field_validator('username')
    @classmethod
    def normalize_username(cls, value: str) -> str:
        normalized = value.strip().lstrip('@')
        if normalized and not _TWITTER_USERNAME_RE.fullmatch(normalized):
            msg = 'username must contain only ASCII letters, digits, or underscores'
            raise ValueError(msg)
        return normalized

    @field_validator('proxy')
    @classmethod
    def normalize_proxy(cls, value: str) -> str:
        return value.strip()

    @field_validator('sleep_request_seconds')
    @classmethod
    def validate_sleep_request_seconds(cls, value: float) -> float:
        if value < 0:
            msg = 'sleep_request_seconds cannot be negative'
            raise ValueError(msg)
        return value

    @field_validator('abort_after')
    @classmethod
    def validate_abort_after(cls, value: int) -> int:
        if value < 1:
            msg = 'abort_after must be greater than or equal to 1'
            raise ValueError(msg)
        return value

    def validate_runnable(self) -> list[str]:
        missing = [] if self.username else ['username']
        missing.extend(f'cookiecloud.{name}' for name in self.cookiecloud.validate_runnable())
        return missing


class Pixiv(ScheduleJob):
    """Public pixiv bookmarks, crawled through the www.pixiv.net ajax API.

    The session is a PHPSESSID cookie pulled from CookieCloud so it tracks the
    browser. The logged-in user's id is normally derived from that cookie's value
    (``{user_id}_{hash}``); ``user_id`` only overrides it for a vault whose cookie
    does not carry that shape.
    """

    path: Path = Path('./collection/pixiv')
    cron: str = '0 */6 * * *'
    cookiecloud: CookieCloud = Field(default_factory=CookieCloud)
    # Optional override; empty means "derive from the PHPSESSID cookie".
    user_id: str = ''
    # Spacing between ajax API requests. Image downloads hit the CDN and are not paced.
    sleep_request_seconds: float = 1.0
    proxy: str = ''

    @field_validator('user_id')
    @classmethod
    def normalize_user_id(cls, value: str) -> str:
        normalized = value.strip()
        if normalized and not normalized.isdigit():
            msg = 'user_id must contain only digits'
            raise ValueError(msg)
        return normalized

    @field_validator('proxy')
    @classmethod
    def normalize_proxy(cls, value: str) -> str:
        return value.strip()

    @field_validator('sleep_request_seconds')
    @classmethod
    def validate_sleep_request_seconds(cls, value: float) -> float:
        if value < 0:
            msg = 'sleep_request_seconds cannot be negative'
            raise ValueError(msg)
        return value

    def validate_runnable(self) -> list[str]:
        return [f'cookiecloud.{name}' for name in self.cookiecloud.validate_runnable()]


class RedNote(ScheduleJob):
    """Liked notes on RedNote (小红书), read through a signed-in Chromium profile.

    There is no public read access to a likes list, and the list is only readable
    by the account itself, so this source has to drive the user's own account. An
    earlier revision replayed the session as plain signed HTTP and RedNote
    answered by invalidating the account everywhere, phone included. Risk control
    reads IP, device fingerprint and behaviour together, so the browser below is
    kept logged in and aged on a volume, its traffic leaves through ``proxy``, and
    the pacing fields are deliberately conservative.
    """

    path: Path = Path('./collection/rednote')
    # Where videos land, both whole video notes and the clips inside live photos.
    # Left unset they stay with the images under ``path``.
    video_path: Path | None = None
    cron: str = '0 */6 * * *'
    # The signed-in Chromium profile: cookies, localStorage, history and the device
    # identity RedNote hands out all live here. It has to be on a volume, or
    # every run starts from a QR scan.
    profile_path: Path = Path('./data/rednote-profile')
    # Egress for xiaohongshu.com. Most datacenter ranges answer HTTP 461, so from a
    # cluster this points at a residential line. Credentials have to be http(s):
    # Chromium cannot authenticate to a SOCKS proxy.
    proxy: str = ''
    # Deliberate opt-out, for a deployment whose own egress is already residential.
    # Running this from a datacenter address should be a decision, not a default.
    allow_direct_connection: bool = False
    # Send the media downloads through the proxy too. Off because they are
    # unauthenticated CDN traffic and a home line is not a CDN.
    proxy_media: bool = False
    # Own profile id. The likes list lives at /user/profile/<id>; left empty it is
    # read out of the signed-in page once and remembered.
    user_id: str = ''
    # Spacing between page loads. RedNote's risk control reads request rhythm,
    # and this is the field most likely to be worth raising rather than lowering.
    sleep_request_seconds: float = 3.0
    # Consecutive pages of already-archived notes that end an incremental run. The
    # likes list is ordered newest first, so two clean pages mean the run has caught
    # up with what the database already has.
    abort_after: int = 2
    # A memory valve for the first backfill: the likes grid only grows as it scrolls,
    # so a run stops after this many pages and the next one picks up where it left
    # off. Reaching it leaves the backfill marked unfinished, which is what makes
    # the next run walk past the part already archived.
    max_pages_per_run: int = 40
    # How long a run waits at the login screen before giving up. Signing in takes two
    # scans -- the account QR, then an account-security QR good for about a minute --
    # so this covers two Telegram round trips, not one. Still bounded, because the
    # control-request consumer runs one request at a time and a longer wait parks
    # every queued manual trigger behind it.
    login_wait_seconds: int = 300
    # Quiet period between login prompts, so back-to-back runs do not each drop a QR
    # photo in the chat. Capped at the QR's lifetime (about five minutes) at the
    # point of use: once the last code has expired, suppressing the next one would
    # fail the run while telling the user to scan a code their app rejects.
    login_prompt_cooldown_seconds: int = 3600
    headless: bool = True

    @field_validator('video_path', mode='before')
    @classmethod
    def normalize_video_path(cls, value: object) -> object:
        # The form sends '' for "keep them with the images". Path('') is Path('.'),
        # which would quietly route every video into the working directory.
        if isinstance(value, str) and not value.strip():
            return None
        return value

    @field_validator('user_id', 'proxy')
    @classmethod
    def normalize_text(cls, value: str) -> str:
        return value.strip()

    @field_validator('sleep_request_seconds')
    @classmethod
    def validate_sleep_request_seconds(cls, value: float) -> float:
        if value < 0:
            msg = 'sleep_request_seconds cannot be negative'
            raise ValueError(msg)
        return value

    @field_validator('abort_after', 'max_pages_per_run', 'login_wait_seconds')
    @classmethod
    def validate_positive(cls, value: int, info: ValidationInfo) -> int:
        if value < 1:
            msg = f'{info.field_name} must be greater than or equal to 1'
            raise ValueError(msg)
        return value

    @field_validator('login_prompt_cooldown_seconds')
    @classmethod
    def validate_login_prompt_cooldown_seconds(cls, value: int) -> int:
        if value < 0:
            msg = 'login_prompt_cooldown_seconds cannot be negative'
            raise ValueError(msg)
        return value

    def validate_runnable(self) -> list[str]:
        # Not a formality: the previous revision ran from a datacenter address and
        # cost the account its sessions. Going out that way now has to be asked for.
        return [] if self.proxy or self.allow_direct_connection else ['proxy']


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
    twitter: Twitter = Field(default_factory=Twitter)
    pixiv: Pixiv = Field(default_factory=Pixiv)
    rednote: RedNote = Field(default_factory=RedNote)


class Settings(BaseModel):
    web: Web = Field(default_factory=Web)
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
    'web.twitter': Twitter,
    'web.pixiv': Pixiv,
    'web.rednote': RedNote,
    'notifications.telegram': TelegramNotification,
}

# Written by the UI, never echoed back in full. See src/api/settings_masking.py.
# Proxy URLs are deliberately not in here: this is a single-user deployment, the
# UI needs to show and edit them as ordinary text, and masking them cost more in
# round-trip machinery than the credential inside was worth.
SENSITIVE_FIELDS: dict[str, tuple[str, ...]] = {
    'web.bilibili': ('accounts[].cookiecloud.password',),
    'web.twitter': ('cookiecloud.password',),
    'web.pixiv': ('cookiecloud.password',),
    'web.telegram': ('accounts[].api_hash',),
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


def notify_enabled(source: str) -> bool:
    """Whether ``source``'s notifications should be queued at all.

    ``source`` is a job key from ``JOB_SPECS``, which is also its field name on
    ``Web``. Anything else -- ``worker``, say -- is not a source toggle and is
    left alone; those call sites gate themselves where they know the job.
    """
    job = getattr(load().web, source.strip().lower(), None)
    return job.notify if isinstance(job, ScheduleJob) else True


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
