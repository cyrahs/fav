"""Liked notes on RedNote (小红书, xiaohongshu.com), images and videos.

``rednote`` is the name this repository uses -- for the module, the job key, the
tables and the settings section. The site's own domain is still xiaohongshu.com, so
that string survives in URLs and nowhere else.

There is no public read access to a likes list, and the list is only readable by the
account that owns it, so this source drives the user's own account and account
safety is the constraint everything else bends around. An earlier revision replayed
the session as signed HTTP from the cluster; RedNote answered with HTTP 461 and
invalidated the account's sessions everywhere, the user's phone included.

So the reading happens in a browser. ``src/web/rednote_browser.py`` keeps a
Chromium profile signed in on a volume, its traffic leaves through a residential
proxy, and the crawl harvests the site's *own* XHR responses as it scrolls. That
covers all three things RedNote's risk control looks at -- address, device
fingerprint, behaviour -- and it means the crawl never computes a request
signature, so there is nothing here to break when the site rotates one.

A run walks the likes list newest first, resolving each page's new notes into one
row per file as it goes, and then downloads every row still marked pending. The
database joins the two halves: a run that dies part-way resumes from the rows.

Two pieces of state make a run incremental:

* a note that already has rows is skipped, so the walk costs one place in a list
  page rather than a page load of its own;
* ``rednote_state`` records whether the first full walk ever finished. Until it
  has, runs walk to the end of the list so the history fills in; after that they
  stop once ``abort_after`` pages of nothing but archived notes come up in a row.

Images and the clips inside live photos are fetched straight from the CDN, with the
browser closed. Whole video notes go through yt-dlp, which re-resolves the note page
itself and can reach the untranscoded original.
"""

from __future__ import annotations

import asyncio
import contextlib
import secrets
import shutil
import subprocess
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Any
from urllib.parse import urlsplit, urlunsplit

import httpx
from tenacity import retry, retry_if_exception_type, stop_after_attempt, wait_exponential

from src.core import logger, settings
from src.tool import database, ensure_unique_path, format_media_filename, sanitize
from src.tool import telegram_bot as telegram_bot_tool
from src.tool.notifications import enqueue_notification
from src.web.rednote_browser import (
    RISK_CONTROL_STATUS_CODES,
    NoteGoneError,
    PlaywrightNoteBrowser,
    ProxyConfigurationError,
    cursor_of,
    decode_qr_data_url,
    describe_shape,
)

if TYPE_CHECKING:
    from collections.abc import Mapping, Sequence

    from tenacity import RetryCallState

    from src.web.rednote_browser import NoteBrowser

log = logger.get('rednote')

_WEB_ORIGIN = 'https://www.xiaohongshu.com'
# Where a note reached from a likes list is declared to have come from. It travels
# with the token in the archive's outgoing links and in what yt-dlp is handed.
_XSEC_SOURCE = 'pc_user'
_COOKIE_FILENAME = 'rednote-cookies.txt'
_VIDEO_CACHE_DIRNAME = 'videos'
_BACKFILL_STATE_KEY = 'backfill_complete'
_LOGIN_PROMPT_STATE_KEY = 'login_prompt_at'
_USER_ID_STATE_KEY = 'user_id'

_LOGGED_IN = 'logged_in'
_LOGGED_OUT = 'logged_out'
_LOGIN_UNKNOWN = 'unknown'
# How many separate runs have to find a note gone before it stops being retried.
# More than one because a single 404 can be a bad moment rather than a deletion.
_MISSING_RUNS_BEFORE_RETIRING = 3
_LOGIN_POLL_INTERVAL_SECONDS = 3.0
# The second scan, in the account-security modal that follows the first one.
_VERIFY_STAGE = 'verify'
# The verification code states a one-minute life, so it is reminted a little before
# that rather than after the user has already pointed a phone at a dead code.
_QR_REFRESH_SECONDS = 45.0
# A floor on how often the page is reloaded while waiting, so a page that is quiet
# for a reason other than a finished scan is not thrashed.
_LOGIN_RELOAD_INTERVAL_SECONDS = 5.0
# One photo per QR that RedNote actually mints. Past three the user is not
# coming, and a fourth is just noise in their chat.
_MAX_QR_MESSAGES = 3
# How long to wait for the site to produce the next page of the likes list while
# scrolling, before deciding it has stopped producing.
_LIKE_PAGE_TIMEOUT_SECONDS = 25.0

_SESSION_EXPIRED_CODES = {-100, -101}

# A CDN URL that has rotated answers 403, and one for a deleted note answers 404 --
# but so does a rotated one, so neither is proof on its own. Both are recorded and
# settled on the next run, when the browser is open and the note can be looked at.
_MEDIA_STALE_STATUS_CODES = {403, 404, 410}
_MEDIA_ACCEPT = 'image/*,video/*,*/*;q=0.8'
# Written into last_error so the next run can find these rows again.
_STALE_URL_MARKER = 'stale-url'

_VIDEO_NOTE_TYPE = 'video'
_IMAGE_MEDIA_TYPE = 'image'
_LIVE_MEDIA_TYPE = 'live'
_VIDEO_MEDIA_TYPE = 'video'
# Both are mp4, and both follow ``video_path`` when it is set.
_VIDEO_MEDIA_TYPES = frozenset({_LIVE_MEDIA_TYPE, _VIDEO_MEDIA_TYPE})

_PREFERRED_IMAGE_SCENE = 'WB_DFT'
_VIDEO_CODEC_PREFERENCE = ('h264', 'h265', 'av1')
_IMAGE_EXT_ALIASES = {
    'jpeg': 'jpg',
    'jpg': 'jpg',
    'png': 'png',
    'gif': 'gif',
    'webp': 'webp',
    'avif': 'avif',
    'heic': 'heic',
}
# RedNote states the format in a suffix on the last path segment rather than in
# a file extension: `.../1040g0083!nd_dft_wlteh_webp_3`.
_IMAGE_EXT_TOKENS = (('webp', 'webp'), ('avif', 'avif'), ('heic', 'heic'), ('png', 'png'), ('jpeg', 'jpg'), ('jpg', 'jpg'), ('gif', 'gif'))
_VIDEO_EXT = 'mp4'

# Note timestamps are epoch milliseconds; rendered in the timezone the app shows.
_DISPLAY_TIMEZONE = timezone(timedelta(hours=8))

_YTDLP_MAX_ATTEMPTS = 3
_YTDLP_BASE_DELAY_SECONDS = 5
# yt-dlp says this when the note page no longer carries a note. Anything else it
# reports is treated as retryable, so a bad night never marks a note gone forever.
_NOTE_GONE_MARKERS = ('当前笔记暂时无法浏览', '笔记不存在', '内容不存在', '已被删除')


class RedNoteError(RuntimeError):
    """A run-ending failure: the session, or RedNote refusing the whole crawl."""

    def __init__(self, message: str, *, notification_dedupe_key: str = '') -> None:
        super().__init__(message)
        self.notification_dedupe_key = notification_dedupe_key


class MediaUnavailableError(RuntimeError):
    """The note or one of its files is gone, rather than temporarily failing."""


class MediaUrlStaleError(RuntimeError):
    """The stored CDN URL no longer serves; the note has to be looked at again."""


class VideoDownloadError(RuntimeError):
    """yt-dlp failed in a way that is worth trying again."""


@dataclass(frozen=True, slots=True)
class NoteRef:
    """One entry of the likes list. The token is what makes the note readable."""

    note_id: str
    xsec_token: str


@dataclass(frozen=True, slots=True)
class RedNoteMedia:
    """One file of one note, and the row that tracks it."""

    note_id: str
    media_index: int
    media_type: str
    media_url: str
    title: str
    author: str
    author_id: str
    note_type: str
    published_at: str
    xsec_token: str
    # Carried so a run can find the rows a previous one recorded as expired.
    last_error: str = ''


def _to_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value)
    if isinstance(value, str) and value.strip().lstrip('-').isdigit():
        return int(value.strip())
    return None


def format_published_at(value: Any) -> str:
    milliseconds = _to_int(value)
    if milliseconds is None or milliseconds <= 0:
        return ''
    return datetime.fromtimestamp(milliseconds / 1000, tz=_DISPLAY_TIMEZONE).strftime('%Y-%m-%d %H:%M:%S')


def infer_image_extension(url: str) -> str:
    parts = urlsplit(url)
    name = Path(parts.path).name
    mapped = _IMAGE_EXT_ALIASES.get(Path(name).suffix.lower().removeprefix('.'))
    if mapped:
        return mapped
    # Only the last path segment and the query, never the whole URL: the CDN host
    # is `sns-webpic...`, which would read as webp for every image.
    marker = f'{name}?{parts.query}'.lower()
    return next((ext for token, ext in _IMAGE_EXT_TOKENS if token in marker), 'jpg')


def _field(source: Any, *names: str) -> Any:
    """Read a field that the page and the API spell differently.

    ``window.__INITIAL_STATE__`` is camelCase where the JSON API was snake_case, but
    the structure underneath is identical. Tolerating both spellings here is what
    lets every parsing test written against API payloads keep passing unchanged --
    which is the cheap proof that ``media_index`` numbering did not shift, and a
    shifted index is the worst bug this module could have.
    """
    if not isinstance(source, dict):
        return None
    return next((source[name] for name in names if source.get(name) is not None), None)


def normalize_media_url(url: str) -> str:
    """The page hands out ``http://`` CDN links; take the encrypted one."""
    value = url.strip()
    if not value:
        return ''
    parts = urlsplit(value)
    if parts.scheme == 'http':
        return urlunsplit(('https', parts.netloc, parts.path, parts.query, parts.fragment))
    return value


def pick_image_url(entry: Any) -> str:
    """The full-size variant of one image, falling back to whatever is offered."""
    info_list = _field(entry, 'infoList', 'info_list')
    entries = [item for item in info_list if isinstance(item, dict)] if isinstance(info_list, list) else []
    preferred = [item for item in entries if str(_field(item, 'imageScene', 'image_scene') or '').strip().upper() == _PREFERRED_IMAGE_SCENE]
    for item in (*preferred, *entries):
        url = normalize_media_url(str(item.get('url') or ''))
        if url:
            return url
    # Only the page state carries these two, and only as a last resort: they are the
    # same image at whatever size the feed happened to want.
    for name in ('urlDefault', 'urlPre'):
        url = normalize_media_url(str(_field(entry, name) or ''))
        if url:
            return url
    return ''


def pick_video_url(stream: Any) -> str:
    """The best playable stream in a note's ``stream`` block, by codec preference.

    The keys under ``stream`` are opaque and versioned: this expected ``h264`` /
    ``h265`` / ``av1`` and the live site answers with ``EF4`` / ``EF5`` / ``EF6`` /
    ``EF7``, each holding variants that name their own codec in a ``videoCodec``
    field. So the bucket name is ignored and the codec is read off the variant.

    Getting this wrong was quiet in both places it is used. A video note still yields
    a row, because yt-dlp re-resolves it from the note page -- but a live photo's clip
    is fetched straight from this URL, so an empty one meant the still was archived
    and the moving half was silently dropped.
    """
    if not isinstance(stream, dict):
        return ''
    ranked: dict[str, str] = {}
    fallback = ''
    for bucket in stream.values():
        for variant in bucket if isinstance(bucket, list) else []:
            if not isinstance(variant, dict):
                continue
            url = normalize_media_url(str(_field(variant, 'masterUrl', 'master_url') or ''))
            if not url:
                continue
            fallback = fallback or url
            ranked.setdefault(str(_field(variant, 'videoCodec', 'video_codec') or '').lower(), url)
    # A named preference when the codec is stated, and anything playable when it is not.
    return next((ranked[codec] for codec in _VIDEO_CODEC_PREFERENCE if codec in ranked), fallback)


def build_note_url(note_id: str, xsec_token: str = '') -> str:
    """The page a note lives on. Without the token it only opens for its author."""
    url = f'{_WEB_ORIGIN}/explore/{note_id}'
    if not xsec_token:
        return url
    return f'{url}?xsec_token={xsec_token}&xsec_source={_XSEC_SOURCE}'


def build_media_filename(media: RedNoteMedia, *, ext: str = '') -> str:
    """``[nickname]2026-08-12 [<note id>_<n>].webp``, the repository's shape.

    The note title is deliberately absent from the name: it is multi-line, mutable,
    and already in the database for the archive UI to search. The date takes its
    place, because a likes list is read chronologically.
    """
    if not ext:
        ext = _VIDEO_EXT if media.media_type in _VIDEO_MEDIA_TYPES else infer_image_extension(media.media_url)
    return format_media_filename(
        title=media.published_at[:10],
        media_id=f'{media.note_id}_{media.media_index}',
        uploader=media.author,
        ext=ext,
    )


def build_ytdlp_command(
    *,
    note_url: str,
    cookie_path: Path,
    output_template: Path,
    proxy: str = '',
    user_agent: str = '',
) -> list[str]:
    """The yt-dlp invocation for one video note.

    The output template is the note id alone: yt-dlp cannot know the repository's
    filename shape, so the file is renamed once it is on disk.

    ``proxy`` matters as much here as it does for the browser. yt-dlp re-fetches the
    note page from xiaohongshu.com carrying the account's cookies, so leaving it on
    the pod's own address would put the session back on exactly the network that got
    it flagged. ``user_agent`` is read out of the live browser rather than pinned, so
    the two provably agree instead of merely claiming to.
    """
    command = [
        'yt-dlp',
        '-o',
        str(output_template),
        '--no-mtime',
        '--cookies',
        str(cookie_path),
        '--referer',
        f'{_WEB_ORIGIN}/',
    ]
    if user_agent:
        command.extend(['--add-header', f'User-Agent:{user_agent}'])
    if proxy:
        command.extend(['--proxy', proxy])
    command.extend(
        [
            '-N',
            '4',
            '--retries',
            '15',
            '--fragment-retries',
            '15',
            '--socket-timeout',
            '30',
            note_url,
        ],
    )
    return command


def parse_api_envelope(payload: Any) -> dict[str, Any]:
    """Unwrap the ``{code, data}`` envelope every RedNote response carries.

    The only surviving piece of the HTTP layer this module used to have, kept
    because the codes inside it are how a signed-out session announces itself even
    when the page still looks logged in.
    """
    if not isinstance(payload, dict):
        msg = 'RedNote returned a body that is not an object'
        raise TypeError(msg)

    code = _to_int(payload.get('code'))
    message = str(payload.get('msg') or payload.get('message') or '').strip()
    if code in _SESSION_EXPIRED_CODES:
        msg = f'The RedNote session is signed out ({code} {message}). Scan the login QR to sign in again.'
        raise RedNoteError(msg, notification_dedupe_key='rednote:login')
    if code not in {None, 0}:
        detail = f' ({message})' if message else ''
        msg = f'RedNote returned error code: {code}{detail}'
        raise ValueError(msg)

    data = payload.get('data')
    return data if isinstance(data, dict) else {}


def decide_login_state(probe: Mapping[str, Any], cookies: Mapping[str, str]) -> str:
    """Whether the profile is signed in, from what the page and the jar both say.

    The page's own store is the authority when it has an answer: it distinguishes a
    signed-in account from the guest identity a signed-out visitor is also issued.
    Everything below it is a fallback. The modal wins over the cookie because
    ``web_session`` lags -- it survives the account revoking the session elsewhere, so
    trusting it alone would have a run walk confidently into a login screen.
    ``UNKNOWN`` means the page has not hydrated yet, which is a reason to look again
    rather than to give up.
    """
    if probe.get('logged_in'):
        return _LOGGED_IN
    if probe.get('has_login_modal') or probe.get('has_verify_modal') or probe.get('qr_src'):
        return _LOGGED_OUT
    if not str(cookies.get('web_session') or '').strip():
        return _LOGGED_OUT
    return _LOGIN_UNKNOWN


def user_id_from_probe(probe: Mapping[str, Any]) -> str:
    """The signed-in account's own profile id, which the likes URL is built from.

    Gated on being signed in, because a signed-out page offers a guest id in the same
    place and the same shape. Walking a guest's profile would find no likes and report
    it as an empty run -- a wrong answer that looks exactly like a right one.
    """
    if not probe.get('logged_in'):
        return ''
    user_id = str(probe.get('user_id') or '').strip()
    if user_id:
        return user_id
    return str(probe.get('profile_href') or '').strip().removeprefix('/user/profile/').split('?')[0]


def extract_like_page(data: dict[str, Any]) -> tuple[list[NoteRef], str, bool]:
    """The notes on one page of the likes list, plus the cursor for the next one."""
    raw_notes = data.get('notes')
    notes: list[NoteRef] = []
    for item in raw_notes if isinstance(raw_notes, list) else []:
        if not isinstance(item, dict):
            continue
        note_id = str(item.get('note_id') or item.get('id') or '').strip()
        if not note_id:
            continue
        notes.append(NoteRef(note_id=note_id, xsec_token=str(item.get('xsec_token') or '').strip()))
    return notes, str(data.get('cursor') or '').strip(), bool(data.get('has_more'))


def extract_note_media(note_card: dict[str, Any], *, note_id: str, xsec_token: str) -> list[RedNoteMedia]:
    """One row per file in a note.

    Indices are handed out in the order the note lists its files, an image and the
    clip of a live photo counting separately. The numbering has to be reproducible
    from the same note: refreshing an expired URL looks the row up by it.

    A video note yields a row even when no stream URL can be read out of it, because
    yt-dlp downloads it from the note page rather than from that URL.
    """
    user = _field(note_card, 'user')
    user = user if isinstance(user, dict) else {}
    note_type = str(note_card.get('type') or '').strip()
    shared = {
        'note_id': note_id,
        'title': str(note_card.get('title') or '').strip(),
        'author': str(_field(user, 'nickname') or '').strip() or 'unknown',
        'author_id': str(_field(user, 'userId', 'user_id') or '').strip(),
        'note_type': note_type,
        'published_at': format_published_at(note_card.get('time')),
        'xsec_token': xsec_token,
    }

    if note_type == _VIDEO_NOTE_TYPE:
        media_block = _field(note_card.get('video'), 'media')
        stream = _field(media_block, 'stream')
        return [RedNoteMedia(media_index=1, media_type=_VIDEO_MEDIA_TYPE, media_url=pick_video_url(stream), **shared)]

    image_list = _field(note_card, 'imageList', 'image_list')
    media: list[RedNoteMedia] = []
    for entry in image_list if isinstance(image_list, list) else []:
        if not isinstance(entry, dict):
            continue
        image_url = pick_image_url(entry)
        if image_url:
            media.append(RedNoteMedia(media_index=len(media) + 1, media_type=_IMAGE_MEDIA_TYPE, media_url=image_url, **shared))
        # An image with a stream attached is a live photo: the still and the clip
        # are both kept, as two files.
        live_url = pick_video_url(entry.get('stream'))
        if live_url:
            media.append(RedNoteMedia(media_index=len(media) + 1, media_type=_LIVE_MEDIA_TYPE, media_url=live_url, **shared))
    return media


def is_note_gone(message: str) -> bool:
    return any(marker in message for marker in _NOTE_GONE_MARKERS)


class RedNote:
    def __init__(self) -> None:
        self.cfg = settings.load().web.rednote
        # Only ever talks to the CDN; everything that needs the account goes through
        # the browser. Proxying it is optional because these are unauthenticated
        # image fetches and a home line is not a CDN.
        self.client = httpx.AsyncClient(
            headers={'Accept-Language': 'zh-CN,zh;q=0.9'},
            timeout=60,
            follow_redirects=True,
            limits=httpx.Limits(max_keepalive_connections=10, max_connections=10),
            proxy=self.cfg.proxy or None if self.cfg.proxy_media else None,
        )
        self.user_id = ''
        self.user_agent = ''
        self._browser_factory = self._new_browser
        self._logged_card_shapes: set[str] = set()

    def _new_browser(self) -> PlaywrightNoteBrowser:
        return PlaywrightNoteBrowser(
            user_data_dir=self.cfg.profile_path,
            proxy=self.cfg.proxy,
            headless=self.cfg.headless,
        )

    async def aclose(self) -> None:
        await self.client.aclose()

    # ---------- state ----------

    async def _ensure_table(self) -> None:
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS rednote (
                note_id TEXT NOT NULL,
                media_index INTEGER NOT NULL,
                media_type TEXT NOT NULL DEFAULT '',
                media_url TEXT NOT NULL DEFAULT '',
                title TEXT NOT NULL DEFAULT '',
                author TEXT NOT NULL DEFAULT '',
                author_id TEXT NOT NULL DEFAULT '',
                note_type TEXT NOT NULL DEFAULT '',
                published_at TEXT NOT NULL DEFAULT '',
                xsec_token TEXT NOT NULL DEFAULT '',
                local_path TEXT NOT NULL DEFAULT '',
                downloaded INTEGER NOT NULL DEFAULT 0,
                unavailable INTEGER NOT NULL DEFAULT 0,
                failed_count INTEGER NOT NULL DEFAULT 0,
                last_error TEXT NOT NULL DEFAULT '',
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (note_id, media_index)
            );
        """)
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS rednote_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
        """)
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS rednote_missing (
                note_id TEXT PRIMARY KEY,
                runs INTEGER NOT NULL DEFAULT 0,
                last_run TEXT NOT NULL DEFAULT '',
                first_seen_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
                last_seen_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
        """)

    async def _backfill_complete(self) -> bool:
        rows = await database.query_db('SELECT value FROM rednote_state WHERE key = ?;', (_BACKFILL_STATE_KEY,))
        return bool(rows) and str(rows[0].get('value')) == '1'

    async def _mark_backfill_complete(self) -> None:
        await database.query_db(
            'INSERT OR IGNORE INTO rednote_state (key, value) VALUES (?, ?);',
            (_BACKFILL_STATE_KEY, '1'),
        )

    async def _read_state(self, key: str) -> str:
        rows = await database.query_db('SELECT value FROM rednote_state WHERE key = ?;', (key,))
        return str(rows[0].get('value') or '') if rows else ''

    async def _write_state(self, key: str, value: str) -> None:
        await database.query_db(
            'INSERT INTO rednote_state (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value;',
            (key, value),
        )

    async def _known_note_ids(self, note_ids: Sequence[str]) -> set[str]:
        """Which of these notes the database has already resolved into rows.

        Deliberately not "already downloaded": a file that keeps failing would
        otherwise hold the stop rule open and make every run walk the whole list.
        Pending rows are retried by the download phase regardless.
        """
        unique = list(dict.fromkeys(note_ids))
        if not unique:
            return set()
        placeholders = ', '.join(['?'] * len(unique))
        rows = await database.query_db(
            f'SELECT DISTINCT note_id FROM rednote WHERE note_id IN ({placeholders});',  # noqa: S608 - placeholders only
            tuple(unique),
        )
        return {str(row.get('note_id') or '') for row in rows}

    async def _upsert_media(self, items: Sequence[RedNoteMedia]) -> None:
        rows = [
            (
                media.note_id,
                str(media.media_index),
                media.media_type,
                media.media_url,
                media.title,
                media.author,
                media.author_id,
                media.note_type,
                media.published_at,
                media.xsec_token,
            )
            for media in items
        ]
        await database.insert_db_batch(
            table='rednote',
            columns=(
                'note_id',
                'media_index',
                'media_type',
                'media_url',
                'title',
                'author',
                'author_id',
                'note_type',
                'published_at',
                'xsec_token',
            ),
            rows=rows,
            # Refreshes what RedNote rotates without touching download state.
            on_conflict=(
                '(note_id, media_index) DO UPDATE SET '
                'media_url = excluded.media_url, '
                'xsec_token = excluded.xsec_token, '
                'title = excluded.title, '
                'author = excluded.author, '
                'published_at = excluded.published_at'
            ),
        )

    async def _pending_media(self) -> list[RedNoteMedia]:
        rows = await database.query_db("""
            SELECT note_id, media_index, media_type, media_url, title, author, author_id, note_type, published_at, xsec_token, last_error
            FROM rednote
            WHERE downloaded = 0 AND unavailable = 0
            ORDER BY created_at ASC;
        """)
        pending: list[RedNoteMedia] = []
        for row in rows:
            note_id = str(row.get('note_id') or '').strip()
            media_index = _to_int(row.get('media_index'))
            if not note_id or media_index is None:
                continue
            pending.append(
                RedNoteMedia(
                    note_id=note_id,
                    media_index=media_index,
                    media_type=str(row.get('media_type') or ''),
                    media_url=str(row.get('media_url') or ''),
                    title=str(row.get('title') or ''),
                    author=str(row.get('author') or 'unknown').strip() or 'unknown',
                    author_id=str(row.get('author_id') or ''),
                    note_type=str(row.get('note_type') or ''),
                    published_at=str(row.get('published_at') or ''),
                    xsec_token=str(row.get('xsec_token') or ''),
                    last_error=str(row.get('last_error') or ''),
                ),
            )
        return pending

    async def _mark_downloaded(self, media: RedNoteMedia, dst_path: Path) -> None:
        await database.query_db(
            """
            UPDATE rednote
            SET downloaded = 1, local_path = ?, unavailable = 0, failed_count = 0, last_error = ''
            WHERE note_id = ? AND media_index = ?;
            """,
            (str(dst_path), media.note_id, str(media.media_index)),
        )

    async def _mark_unavailable(self, media: RedNoteMedia, reason: str) -> None:
        await database.query_db(
            """
            UPDATE rednote
            SET unavailable = 1, failed_count = failed_count + 1, last_error = ?
            WHERE note_id = ? AND media_index = ?;
            """,
            (reason[:500], media.note_id, str(media.media_index)),
        )

    async def _mark_failed(self, media: RedNoteMedia, reason: str) -> None:
        await database.query_db(
            """
            UPDATE rednote
            SET failed_count = failed_count + 1, last_error = ?
            WHERE note_id = ? AND media_index = ?;
            """,
            (reason[:500], media.note_id, str(media.media_index)),
        )

    # ---------- the signed-in browser ----------

    async def _await_login(self, browser: NoteBrowser) -> None:
        """Block until the profile is signed in, sending the QR to Telegram.

        Bounded on purpose: the control-request consumer runs one request at a time,
        so waiting here parks every queued manual trigger behind this run.
        """
        deadline = time.monotonic() + self.cfg.login_wait_seconds
        # Per stage, because signing in takes two scans from two different modals and
        # a cap shared between them would spend the whole budget on the first one.
        sent: dict[str, set[str]] = {}
        first_seen: dict[str, float] = {}
        last_reload = 0.0
        self._login_modal_logged = False
        # Checked once for the whole wait, not per code: RedNote mints a new QR
        # every couple of minutes, and re-checking would let the cooldown block the
        # replacement for a code the user is still looking at.
        self._may_prompt = None

        while time.monotonic() < deadline:
            probe = await browser.probe_login()
            state = decide_login_state(probe, await browser.cookie_dict())
            if state == _LOGGED_IN:
                self.user_id = self.cfg.user_id or user_id_from_probe(probe)
                if sent:
                    await self._notify_login_complete()
                return

            qr_src = str(probe.get('qr_src') or '')
            stage = str(probe.get('qr_stage') or 'login')
            now = time.monotonic()

            if qr_src:
                if not await self._offer_qr(qr_src=qr_src, stage=stage, sent=sent):
                    break
                if stage == _VERIFY_STAGE and now - first_seen.setdefault(qr_src, now) >= _QR_REFRESH_SECONDS:
                    # The verification code is good for a minute and its expiry leaves
                    # no mark in the DOM -- no class, and the same image bytes as
                    # before. The only readout is a sentence, in whatever language the
                    # profile runs in. So it is reminted on the clock, not on a state.
                    first_seen[qr_src] = now
                    if await browser.refresh_qr():
                        log.info('RedNote reminted the expiring verification QR')
            elif now - last_reload >= _LOGIN_RELOAD_INTERVAL_SECONDS:
                last_reload = now
                await self._nudge_login_page(browser, probe=probe, state=state)

            await asyncio.sleep(_LOGIN_POLL_INTERVAL_SECONDS)

        msg = (
            'RedNote is signed out. A login QR was sent to Telegram; scan it with the app and run this source again.'
            if sent
            else 'RedNote is signed out and no login QR could be sent. Check the Telegram notification settings.'
        )
        raise RedNoteError(msg, notification_dedupe_key='rednote:login')

    async def _offer_qr(self, *, qr_src: str, stage: str, sent: dict[str, set[str]]) -> bool:
        """Send this code if it is new and still allowed. False means stop waiting."""
        stage_sent = sent.setdefault(stage, set())
        if qr_src in stage_sent:
            return True
        if len(stage_sent) >= _MAX_QR_MESSAGES:
            return False
        if self._may_prompt is None:
            self._may_prompt = await self._login_prompt_allowed()
        if not self._may_prompt:
            return False
        stage_sent.add(qr_src)
        await self._send_login_qr(decode_qr_data_url(qr_src), stage=stage, attempt=len(stage_sent))
        return True

    async def _nudge_login_page(self, browser: NoteBrowser, *, probe: Mapping[str, Any], state: str) -> None:
        """Reload a page that is showing neither a QR nor a session.

        This is the state a successful scan leaves behind: both modals closed, the
        session cookie set, and ``__INITIAL_STATE__`` still reporting signed out
        because it is the snapshot the page was loaded with. Without the reload that
        reads as UNKNOWN for the rest of the budget, failing a login that worked.
        """
        if state == _LOGGED_OUT and not self._login_modal_logged:
            self._login_modal_logged = True
            log.info('RedNote is signed out but showed no QR: %s', str(probe.get('modal_html') or '')[:800] or '(no modal)')
        await self._reload_login(browser)

    async def _reload_login(self, browser: NoteBrowser) -> None:
        """A navigation that gets interrupted here is not the run's problem.

        The site navigates itself the moment a QR is scanned, and that aborts whatever
        this had in flight -- ending the run at the exact moment it succeeded. Every
        other cause is answered by the next poll anyway, and the wait is bounded.
        """
        try:
            await browser.reload_login()
        except Exception as exc:  # noqa: BLE001
            log.info('RedNote login page reload was interrupted: %s', exc)

    async def _login_prompt_allowed(self) -> bool:
        """A QR dies within minutes, so a 04:00 cron sending one is pure noise."""
        if self.cfg.login_prompt_cooldown_seconds <= 0:
            return True
        last = _to_int(await self._read_state(_LOGIN_PROMPT_STATE_KEY)) or 0
        now = int(datetime.now(tz=_DISPLAY_TIMEZONE).timestamp())
        if now - last < self.cfg.login_prompt_cooldown_seconds:
            log.info('RedNote is signed out, but a login QR was already sent %ss ago', now - last)
            return False
        await self._write_state(_LOGIN_PROMPT_STATE_KEY, str(now))
        return True

    async def _send_login_qr(self, png: bytes, *, stage: str, attempt: int) -> None:
        """Straight to Telegram, not through the notification outbox.

        The outbox is drained after ``update()`` returns, which for a run blocked on
        this scan is long after the code has expired.
        """
        # Which of the two scans this is, because they look identical in a chat and
        # the second one is only good for a minute.
        if stage == _VERIFY_STAGE:
            caption = f'小红书安全验证（第 {attempt} 个码，约 1 分钟内有效）：用已登录的小红书 App 扫码。'  # noqa: RUF001 - Chinese punctuation
        else:
            caption = f'小红书需要登录（第 {attempt} 个二维码）：用手机小红书扫码，扫完还会有一个验证码。'  # noqa: RUF001 - Chinese punctuation
        try:
            await telegram_bot_tool.send_photo_now(photo=('rednote-login.png', png, 'image/png'), header='RedNote', caption=caption)
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to send the RedNote login QR: %s', exc)
            return
        log.notice('Sent a RedNote login QR to Telegram (attempt %s)', attempt)

    async def _notify_login_complete(self) -> None:
        with contextlib.suppress(Exception):
            await telegram_bot_tool.send_text_now(header='RedNote', text='小红书已登录，继续同步点赞。')  # noqa: RUF001 - Chinese punctuation

    async def _prepare_session(self, browser: NoteBrowser) -> None:
        """Sign in if needed, then settle what the rest of the run depends on."""
        await self._await_login(browser)
        if not self.user_id:
            self.user_id = self.cfg.user_id or await self._read_state(_USER_ID_STATE_KEY)
        if not self.user_id:
            msg = 'Signed in, but the profile id could not be read off the page; set user_id in the settings.'
            raise RedNoteError(msg, notification_dedupe_key='rednote:login')
        if not self.cfg.user_id:
            await self._write_state(_USER_ID_STATE_KEY, self.user_id)
        self.user_agent = await browser.user_agent()

    async def _sleep_between_requests(self) -> None:
        if self.cfg.sleep_request_seconds > 0:
            await asyncio.sleep(self.cfg.sleep_request_seconds)

    # ---------- reading the account ----------

    async def _initial_like_page(self, browser: NoteBrowser) -> tuple[list[NoteRef], str, bool] | None:
        """The likes the page was served with, as a page in the same shape as the rest.

        Carries no cursor, because it was never fetched with one -- and it needs none:
        the cursor is only used to recognise a page the site sent twice, and this one
        cannot be sent twice.
        """
        raw = await browser.initial_like_notes()
        notes, _cursor, _has_more = extract_like_page({'notes': raw})
        if not notes:
            log.debug('RedNote found no pre-rendered likes on the profile page')
            return None
        log.info('RedNote read %d likes rendered into the page itself', len(notes))
        return notes, '', True

    async def _next_like_page(self, browser: NoteBrowser) -> tuple[list[NoteRef], str, bool] | None:
        """One page of the likes list, taken from the browser's own XHR."""
        captured = await browser.next_like_page(timeout_seconds=_LIKE_PAGE_TIMEOUT_SECONDS)
        if captured is None:
            return None

        status = _to_int(captured.get('status')) or 0
        if status in RISK_CONTROL_STATUS_CODES:
            msg = f'RedNote answered the likes list with HTTP {status}: the account is behind a captcha. Clear it in the browser.'
            raise RedNoteError(msg, notification_dedupe_key='rednote:risk')

        notes, _cursor, has_more = extract_like_page(parse_api_envelope(captured.get('body')))
        # The cursor the page was *fetched* with identifies it, which also absorbs
        # the duplicate request the site's own client sometimes fires per scroll.
        return notes, cursor_of(str(captured.get('url') or '')), has_more

    async def _fetch_note_card(self, *, note_id: str, xsec_token: str, browser: NoteBrowser) -> dict[str, Any]:
        card = await browser.note_state(note_url=build_note_url(note_id, xsec_token), note_id=note_id)
        # Once per note type per run. The camelCase spelling of a note was transcribed
        # from the snake_case API rather than captured, and `media_index` is handed out
        # by walking these fields -- so a shifted key is a wrongly numbered row.
        note_type = str(_field(card, 'type') or 'unknown')
        if note_type not in self._logged_card_shapes:
            self._logged_card_shapes.add(note_type)
            log.debug('RedNote note card shape (type=%s): %s', note_type, describe_shape(card))
        return card

    # ---------- downloads ----------

    def _destination_root(self, media_type: str) -> Path:
        if self.cfg.video_path is not None and media_type in _VIDEO_MEDIA_TYPES:
            return self.cfg.video_path
        return self.cfg.path

    def _build_output_path(self, media: RedNoteMedia, *, ext: str = '') -> Path:
        author_dir = sanitize(media.author) or 'unknown'
        return self._destination_root(media.media_type) / author_dir / build_media_filename(media, ext=ext)

    async def _stale_media(self) -> list[RedNoteMedia]:
        """Pending rows whose CDN URL the download phase found expired."""
        return [media for media in await self._pending_media() if media.last_error.startswith(_STALE_URL_MARKER)]

    async def _refresh_stale_media(self, browser: NoteBrowser) -> int:
        """Re-resolve the notes behind expired URLs, while the browser is still open.

        The download phase runs with the browser closed, so it can only record that a
        URL went stale; this is where that gets acted on. A row is only ever declared
        unavailable here, after a fresh look at the note itself -- never from a bare
        404, which a rotated URL produces just as readily as a deleted one.
        """
        stale = await self._stale_media()
        if not stale:
            return 0

        log.info('RedNote re-resolving %d file(s) whose URL expired', len(stale))
        refreshed = 0
        for media in stale:
            await self._sleep_between_requests()
            try:
                note_card = await self._fetch_note_card(note_id=media.note_id, xsec_token=media.xsec_token, browser=browser)
            except RedNoteError:
                raise
            except Exception as exc:  # noqa: BLE001
                log.warning('Could not re-resolve note %s: %s', media.note_id, exc)
                continue

            fresh = extract_note_media(note_card, note_id=media.note_id, xsec_token=media.xsec_token) if note_card else []
            match = next((item for item in fresh if item.media_index == media.media_index and item.media_type == media.media_type), None)
            if match is None or not match.media_url:
                await self._mark_unavailable(media, 'the note no longer offers this file')
                continue
            if match.media_url == media.media_url:
                continue

            await database.query_db(
                "UPDATE rednote SET media_url = ?, last_error = '' WHERE note_id = ? AND media_index = ?;",
                (match.media_url, media.note_id, str(media.media_index)),
            )
            refreshed += 1
        return refreshed

    async def _fetch_file(self, url: str) -> httpx.Response:
        response = await self.client.get(url, headers={'Accept': _MEDIA_ACCEPT})
        response.raise_for_status()
        return response

    async def _download_file(self, media: RedNoteMedia) -> Path:
        """An image or a live photo's clip, straight from the CDN."""
        dst_path = self._build_output_path(media)
        await asyncio.to_thread(dst_path.parent.mkdir, parents=True, exist_ok=True)
        if await asyncio.to_thread(dst_path.exists):
            return dst_path
        if not media.media_url:
            msg = f'{_STALE_URL_MARKER} the row carries no URL'
            raise MediaUrlStaleError(msg)

        try:
            response = await self._fetch_file(media.media_url)
        except httpx.HTTPStatusError as exc:
            status = exc.response.status_code
            if status not in _MEDIA_STALE_STATUS_CODES:
                raise
            # Not resolved here: the browser is closed by now, and a 404 on a CDN
            # URL says as much about rotation as about deletion. The next run looks
            # at the note again and decides.
            msg = f'{_STALE_URL_MARKER} http {status}'
            raise MediaUrlStaleError(msg) from exc

        await asyncio.to_thread(dst_path.write_bytes, response.content)
        return dst_path

    @staticmethod
    def _cleanup_dir(dirpath: Path) -> None:
        if not dirpath.exists():
            return
        for entry in dirpath.iterdir():
            if entry.is_dir():
                shutil.rmtree(entry)
            else:
                entry.unlink()

    def _run_ytdlp(self, *, note_url: str, cookie_path: Path, cache_dir: Path, note_id: str) -> None:
        command = build_ytdlp_command(
            note_url=note_url,
            cookie_path=cookie_path,
            output_template=cache_dir / f'{note_id}.%(ext)s',
            proxy=self.cfg.proxy,
            user_agent=self.user_agent,
        )

        def _on_retry_before_sleep(retry_state: RetryCallState) -> None:
            exc = retry_state.outcome.exception() if retry_state.outcome else None
            if exc is None:
                return
            sleep = retry_state.next_action.sleep if retry_state.next_action else 0.0
            log.debug('Retrying note %s (%d/%d), next wait %.1fs: %s', note_id, retry_state.attempt_number, _YTDLP_MAX_ATTEMPTS, sleep, exc)

        @retry(
            reraise=True,
            stop=stop_after_attempt(_YTDLP_MAX_ATTEMPTS),
            wait=wait_exponential(multiplier=_YTDLP_BASE_DELAY_SECONDS, min=_YTDLP_BASE_DELAY_SECONDS, max=_YTDLP_BASE_DELAY_SECONDS * 6),
            retry=retry_if_exception_type(VideoDownloadError),
            before_sleep=_on_retry_before_sleep,
        )
        def _run_once() -> None:
            self._cleanup_dir(cache_dir)
            result = subprocess.run(command, text=True, capture_output=True, check=False)  # noqa: S603
            if result.returncode == 0:
                if result.stderr:
                    log.debug('yt-dlp stderr: %s', result.stderr.strip())
                return
            message = result.stderr.strip() or result.stdout.strip() or f'yt-dlp exited with code {result.returncode}'
            if is_note_gone(message):
                raise MediaUnavailableError(message)
            raise VideoDownloadError(message)

        _run_once()

    def _move_download(self, *, cache_dir: Path, media: RedNoteMedia) -> Path:
        """Rename what yt-dlp produced into the repository's filename shape."""
        entries = [entry for entry in sorted(cache_dir.iterdir()) if entry.is_file()] if cache_dir.exists() else []
        if not entries:
            msg = 'yt-dlp reported success but produced no file'
            raise VideoDownloadError(msg)

        # A note is one video; anything else next to it is a thumbnail or a fragment.
        source = max(entries, key=lambda entry: entry.stat().st_size)
        dst_path = self._build_output_path(media, ext=source.suffix.removeprefix('.') or _VIDEO_EXT)
        dst_path.parent.mkdir(parents=True, exist_ok=True)
        dst_path = ensure_unique_path(dst_path)
        # shutil.move, not rename: the video directory is usually a different mount.
        shutil.move(str(source), str(dst_path))
        self._cleanup_dir(cache_dir)
        return dst_path

    async def _download_video(self, media: RedNoteMedia, *, cookie_path: Path, cache_dir: Path) -> Path:
        dst_path = self._build_output_path(media)
        await asyncio.to_thread(dst_path.parent.mkdir, parents=True, exist_ok=True)
        if await asyncio.to_thread(dst_path.exists):
            return dst_path

        await asyncio.to_thread(cache_dir.mkdir, parents=True, exist_ok=True)
        # yt-dlp runs for minutes; keep it off the event loop so the worker's queues
        # and Telegram listeners stay responsive.
        await asyncio.to_thread(
            self._run_ytdlp,
            note_url=build_note_url(media.note_id, media.xsec_token),
            cookie_path=cookie_path,
            cache_dir=cache_dir,
            note_id=media.note_id,
        )
        return await asyncio.to_thread(self._move_download, cache_dir=cache_dir, media=media)

    async def _download_one(self, media: RedNoteMedia, *, cookie_path: Path, cache_dir: Path) -> Path:
        if media.media_type == _VIDEO_MEDIA_TYPE:
            # yt-dlp reads the note page off xiaohongshu.com itself, so it is paced
            # like an API request. The CDN the other files come from is not metered
            # that way, and pacing it would add hours to a first backfill.
            await self._sleep_between_requests()
            return await self._download_video(media, cookie_path=cookie_path, cache_dir=cache_dir)
        return await self._download_file(media)

    async def _download_pending(self, items: Sequence[RedNoteMedia], *, cookie_path: Path, cache_dir: Path) -> int:
        downloaded = 0
        total = len(items)
        for position, media in enumerate(items, start=1):
            log.info(
                'RedNote downloading progress=%s/%s note=%s index=%s type=%s',
                position,
                total,
                media.note_id,
                media.media_index,
                media.media_type,
            )
            try:
                dst_path = await self._download_one(media, cookie_path=cookie_path, cache_dir=cache_dir)
            except MediaUnavailableError as exc:
                reason = str(exc).strip() or 'confirmed unavailable'
                await self._mark_unavailable(media, reason)
                log.info('Marked unavailable note=%s index=%s: %s', media.note_id, media.media_index, reason)
                continue
            except MediaUrlStaleError as exc:
                # Recorded rather than resolved: the next run re-reads the note with
                # the browser open and either refreshes the URL or retires the row.
                await self._mark_failed(media, str(exc))
                log.info('RedNote URL expired note=%s index=%s: %s', media.note_id, media.media_index, exc)
                continue
            except httpx.HTTPStatusError as exc:
                await self._mark_failed(media, f'http {exc.response.status_code}')
                log.warning('Failed note=%s index=%s: http %s', media.note_id, media.media_index, exc.response.status_code)
                continue
            except RedNoteError:
                raise
            except Exception as exc:  # noqa: BLE001
                await self._mark_failed(media, f'{type(exc).__name__}: {exc}')
                log.warning('Failed note=%s index=%s: %s', media.note_id, media.media_index, exc)
                continue

            await self._mark_downloaded(media, dst_path)
            downloaded += 1
            log.notice('RedNote downloaded progress=%s/%s file=%s', position, total, dst_path.name)
        return downloaded

    # ---------- phases ----------

    async def _absorb_page(self, notes: Sequence[NoteRef], *, browser: NoteBrowser, seen: set[str], page_index: int, run_id: str) -> int:
        """Resolve whatever on this page is not archived yet, and say what that added."""
        note_ids = [note.note_id for note in notes]
        known = await self._known_note_ids(note_ids)
        # Notes enough runs have agreed are gone. Skipped rather than re-opened, which
        # is the whole point of counting: a deleted note otherwise costs a page load on
        # every run for as long as it stays in the list.
        retired = await self._retired_note_ids(note_ids)
        unknown = [note for note in notes if note.note_id not in known and note.note_id not in retired and note.note_id not in seen]
        seen.update(note_ids)

        added = 0
        if unknown:
            added = await self._resolve_notes(unknown, browser, run_id=run_id)
            # Reading a note navigates away from the likes list, so it has to be
            # reopened before the next page can be scrolled into view.
            await browser.open_likes(user_id=self.user_id)
        log.info('RedNote likes page=%s notes=%s new=%s added=%s', page_index, len(notes), len(unknown), added)
        return added

    async def _crawl(self, browser: NoteBrowser) -> int:
        """Walk the likes list newest first, resolving each page's new notes as it goes.

        The pages are the browser's own: the liked tab arrives with its first screenful
        already rendered in, and scrolling makes the site fetch the rest, whose
        responses are read as they go by. So nothing here reproduces a request
        signature, and nothing is taken on trust about which page is first.

        Rows land page by page rather than at the end of the walk, so a run that dies
        part-way leaves the next one with less to re-fetch: an already-resolved note
        costs one place in a list page instead of a page load of its own.

        The early stop only applies once a full walk has finished, recorded in
        ``rednote_state``. Stopping at the top of the list before that would leave
        everything below wherever the first run got to permanently unreachable.
        """
        backfill_complete = await self._backfill_complete()
        # Identifies this walk, so that a note seen twice in it counts once toward the
        # runs that have to agree before it is retired.
        run_id = secrets.token_hex(8)
        seen_cursors: set[str] = set()
        seen_note_ids: set[str] = set()
        page_index = 0
        full_hit_pages = 0
        resolved = 0
        reached_end = False

        log.info('RedNote walking the likes list (backfill_complete=%s)', backfill_complete)
        await browser.open_likes(user_id=self.user_id)
        # The page arrives with its first screenful of likes already rendered into it,
        # so the first request the site makes is for what comes *after* them. Reading
        # only the requests would skip the newest likes -- the ones an incremental run
        # exists for -- and then stop on `abort_after` having archived none of them.
        pending_first = await self._initial_like_page(browser)

        while page_index < self.cfg.max_pages_per_run:
            page_index += 1
            page, pending_first = (pending_first, None) if pending_first is not None else (await self._next_like_page(browser), None)
            if page is None:
                # Scrolling stopped producing. Not the same as reaching the end, so
                # the backfill stays unfinished and the next run walks again.
                log.info('RedNote likes list stopped producing at page=%s', page_index)
                break

            notes, cursor, has_more = page
            if not notes:
                log.info('RedNote likes list ended at page=%s', page_index)
                reached_end = True
                break
            if cursor and cursor in seen_cursors:
                log.debug('RedNote repeated the page at cursor %r, skipping it', cursor)
                page_index -= 1
                continue
            seen_cursors.add(cursor)

            added = await self._absorb_page(notes, browser=browser, seen=seen_note_ids, page_index=page_index, run_id=run_id)
            resolved += added

            # The stop rule asks what the page contributed, not whether every id on it
            # was already known. A note that cannot be resolved -- deleted, or gone
            # private -- never gains rows and so is never "known", and under the older
            # rule one of those sitting near the top of the list reset this counter on
            # every run, making the early stop unreachable and every run a full walk.
            # Asking about the archive instead is also indifferent to the list
            # shifting underneath it as things are liked and unliked.
            if added:
                full_hit_pages = 0
            else:
                full_hit_pages += 1
                log.info('RedNote page=%s added nothing, consecutive=%s/%s', page_index, full_hit_pages, self.cfg.abort_after)
                if backfill_complete and full_hit_pages >= self.cfg.abort_after:
                    log.info('RedNote reached the incremental stop condition')
                    break

            if not has_more:
                log.info('RedNote likes list reported no further pages')
                reached_end = True
                break
        else:
            log.info('RedNote stopped at the %d page cap; the next run continues from here', self.cfg.max_pages_per_run)

        if reached_end and not backfill_complete:
            await self._mark_backfill_complete()
            log.info('RedNote backfill finished; later runs stop after %d archived pages', self.cfg.abort_after)
        return resolved

    async def _resolve_notes(self, notes: Sequence[NoteRef], browser: NoteBrowser, *, run_id: str) -> int:
        """Turn each new note into its rows. One unreadable note must not end the run."""
        resolved = 0
        total = len(notes)
        for position, note in enumerate(notes, start=1):
            await self._sleep_between_requests()
            try:
                note_card = await self._fetch_note_card(note_id=note.note_id, xsec_token=note.xsec_token, browser=browser)
            except RedNoteError:
                raise
            except NoteGoneError:
                # The site said so, in the only way it says so: a redirect to /404.
                await self._record_missing(note.note_id, run_id=run_id)
                continue
            except Exception as exc:  # noqa: BLE001
                # Everything else is this run's problem, not the note's. Recording it
                # would let a bad night retire a note that is perfectly fine.
                log.warning('Failed to read note %s (%s/%s): %s', note.note_id, position, total, exc)
                continue

            media = extract_note_media(note_card, note_id=note.note_id, xsec_token=note.xsec_token)
            if not media:
                # Read fine, carries nothing this source handles. Not a deletion.
                log.warning('Note %s carries no files, skipping', note.note_id)
                continue
            await self._upsert_media(media)
            await self._clear_missing(note.note_id)
            resolved += len(media)
        return resolved

    async def _record_missing(self, note_id: str, *, run_id: str) -> None:
        """Count one run that found this note gone.

        Counted per run rather than per sighting: the same note can come round twice
        in one walk, and three sightings in one bad minute is not the evidence being
        asked for. ``last_run`` is what makes the second one in a run a no-op.
        """
        await database.query_db(
            """
            INSERT INTO rednote_missing (note_id, runs, last_run) VALUES (?, 1, ?)
            ON CONFLICT (note_id) DO UPDATE
            SET runs = rednote_missing.runs + 1, last_run = excluded.last_run, last_seen_at = CURRENT_TIMESTAMP
            WHERE rednote_missing.last_run <> excluded.last_run;
            """,
            (note_id, run_id),
        )
        log.info('RedNote note %s is gone; it is retried until %d runs agree', note_id, _MISSING_RUNS_BEFORE_RETIRING)

    async def _clear_missing(self, note_id: str) -> None:
        """Forget a note that read fine, so a past 404 can never retire a live note."""
        await database.query_db('DELETE FROM rednote_missing WHERE note_id = ?;', (note_id,))

    async def _retired_note_ids(self, note_ids: Sequence[str]) -> set[str]:
        """Notes that enough separate runs have found gone to stop asking about."""
        if not note_ids:
            return set()
        placeholders = ','.join('?' for _ in note_ids)
        rows = await database.query_db(
            f'SELECT note_id FROM rednote_missing WHERE runs >= ? AND note_id IN ({placeholders});',  # noqa: S608 - placeholders only
            (_MISSING_RUNS_BEFORE_RETIRING, *note_ids),
        )
        return {str(row['note_id']) for row in rows}

    async def _notify_summary(self, *, downloaded: int, resolved: int, pending: int) -> None:
        try:
            await enqueue_notification(
                kind='summary',
                source='rednote',
                header='RedNote',
                title='Update completed',
                body=f'Downloaded {downloaded} files, {resolved} of them newly liked.',
                payload={'downloaded': downloaded, 'resolved': resolved, 'pending': pending},
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue rednote summary notification: %s', exc)

    async def _read_account(self, cookie_path: Path) -> int:
        """Everything that needs the browser, in one session.

        Held open for as short a window as possible: a persistent Chromium context
        costs several hundred megabytes, and the download phase that follows can run
        for hours. Being OOM-killed here would take the whole worker with it.
        """
        try:
            browser = self._browser_factory()
        except ProxyConfigurationError as exc:
            raise RedNoteError(str(exc), notification_dedupe_key='rednote:proxy') from exc

        try:
            await browser.start()
            await self._prepare_session(browser)
            resolved = await self._crawl(browser)
            await self._refresh_stale_media(browser)
            await asyncio.to_thread(cookie_path.write_text, await browser.netscape_cookies(), encoding='utf-8')
        finally:
            await browser.aclose()
        return resolved

    async def update(self) -> None:
        missing = self.cfg.validate_runnable()
        if missing:
            log.warning('RedNote is not configured (missing %s), skip update', ', '.join(missing))
            return

        await self._ensure_table()
        await asyncio.to_thread(self.cfg.path.mkdir, parents=True, exist_ok=True)
        await asyncio.to_thread(self.cfg.profile_path.mkdir, parents=True, exist_ok=True)
        if self.cfg.video_path is not None:
            await asyncio.to_thread(self.cfg.video_path.mkdir, parents=True, exist_ok=True)

        with tempfile.TemporaryDirectory() as tmp_dir:
            cookie_path = Path(tmp_dir) / _COOKIE_FILENAME
            cache_dir = Path(tmp_dir) / _VIDEO_CACHE_DIRNAME
            # One writer per profile: a Chromium user-data-dir is single-owner, and
            # the worker, the API and a --trigger run are three plausible claimants.
            lock_name = f'rednote-profile:{self.cfg.profile_path.expanduser().resolve()}'
            async with database.advisory_lock(lock_name) as acquired:
                if not acquired:
                    log.warning('The RedNote browser profile is in use by another run; skip this one')
                    return
                resolved = await self._read_account(cookie_path)

            pending = await self._pending_media()
            downloaded = await self._download_pending(pending, cookie_path=cookie_path, cache_dir=cache_dir)

        log.info('RedNote resolved %d new files and downloaded %d of %d pending', resolved, downloaded, len(pending))
        if downloaded:
            await self._notify_summary(downloaded=downloaded, resolved=resolved, pending=len(pending))
