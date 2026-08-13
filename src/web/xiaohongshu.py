"""Liked notes on Xiaohongshu (RED), images and videos.

Xiaohongshu sells no read access to a likes list, so the crawl calls the same web
endpoints the browser client does: ``note/like/page`` to walk the list and ``feed``
to resolve one note into its files. Both are signed with ``xhshow``; the session
comes from CookieCloud so it tracks the browser instead of going stale here.

A run walks the likes list newest first, resolving each page's new notes into one
row per file as it goes, and then downloads every row still marked pending. The
database is what joins the two halves: a run that dies part-way resumes from the
rows rather than from the API, which matters because every API request is metered
against risk control.

Two pieces of state make a run incremental:

* a note that already has rows is skipped, so the walk costs one place in a list
  page rather than a request of its own;
* ``xiaohongshu_state`` records whether the first full walk ever finished. Until it
  has, runs walk to the end of the list so the history fills in; after that they
  stop once ``abort_after`` pages of nothing but archived notes come up in a row.

Images and the clips inside live photos are fetched straight from the CDN. Whole
video notes go through yt-dlp instead: it re-resolves the note page itself, so it
is immune to a stream URL expiring, and it can reach the untranscoded original.
"""

from __future__ import annotations

import asyncio
import shutil
import subprocess
import tempfile
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import TYPE_CHECKING, Any, Literal
from urllib.parse import urlsplit

import httpx
from tenacity import retry, retry_if_exception_type, stop_after_attempt, wait_exponential
from xhshow import Xhshow

from src.core import logger, settings
from src.tool import CookieCloudClient, database, ensure_unique_path, format_media_filename, sanitize
from src.tool.cookiecloud import XIAOHONGSHU_PROFILE
from src.tool.notifications import enqueue_notification

if TYPE_CHECKING:
    from collections.abc import Sequence

    from tenacity import RetryCallState

log = logger.get('xiaohongshu')

_API_ORIGIN = 'https://edith.xiaohongshu.com'
_LIKE_PAGE_URI = '/api/sns/web/v1/note/like/page'
_FEED_URI = '/api/sns/web/v1/feed'
_USER_ME_URI = '/api/sns/web/v2/user/me'
_WEB_ORIGIN = 'https://www.xiaohongshu.com'
# Pinned rather than read from anywhere: the signature encodes a browser-shaped
# client, and a User-Agent that disagrees with it is a risk-control signal.
_USER_AGENT = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36'
_PAGE_SIZE = 30
_IMAGE_FORMATS = ('jpg', 'webp', 'avif')
# Where a note reached from a likes list is declared to have come from. It travels
# with the token in both the feed request and the archive's outgoing links.
_XSEC_SOURCE = 'pc_user'
_COOKIE_FILENAME = 'xiaohongshu-cookies.txt'
_VIDEO_CACHE_DIRNAME = 'videos'
_BACKFILL_STATE_KEY = 'backfill_complete'

# xhshow's two signature envelopes. Xiaohongshu answers the wrong one with 406, and
# which one the data endpoints want has already changed once, so a run rotates to
# the other rather than failing the whole source.
_SIGN_FORMATS: tuple[Literal['xyw', 'xys'], ...] = ('xyw', 'xys')
_SIGN_REJECTED_STATUS = 406

_API_RETRY_DELAYS_SECONDS = (2.0, 5.0, 15.0, 30.0)
_API_RETRYABLE_STATUS_CODES = {429, 502, 503, 504}
# The captcha wall. Retrying it is how a session gets locked, so it ends the run.
_RISK_CONTROL_STATUS_CODES = {461, 471}
_SESSION_EXPIRED_CODES = {-100, -101}

# A CDN URL that has rotated answers 403; one that is gone answers 404/410.
_MEDIA_STALE_STATUS_CODES = {403, 404, 410}
_MEDIA_UNAVAILABLE_STATUS_CODES = {404, 410}
_MEDIA_ACCEPT = 'image/*,video/*,*/*;q=0.8'

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
# Xiaohongshu states the format in a suffix on the last path segment rather than in
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


class XiaohongshuError(RuntimeError):
    """A run-ending failure: the session, or Xiaohongshu refusing the whole crawl."""

    def __init__(self, message: str, *, notification_dedupe_key: str = '') -> None:
        super().__init__(message)
        self.notification_dedupe_key = notification_dedupe_key


class MediaUnavailableError(RuntimeError):
    """The note or one of its files is gone, rather than temporarily failing."""


class VideoDownloadError(RuntimeError):
    """yt-dlp failed in a way that is worth trying again."""


@dataclass(frozen=True, slots=True)
class NoteRef:
    """One entry of the likes list. The token is what makes the note readable."""

    note_id: str
    xsec_token: str


@dataclass(frozen=True, slots=True)
class XhsMedia:
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


def pick_image_url(info_list: Any) -> str:
    """The full-size variant of one image, falling back to whatever is offered."""
    entries = [entry for entry in info_list if isinstance(entry, dict)] if isinstance(info_list, list) else []
    preferred = [entry for entry in entries if str(entry.get('image_scene') or '').strip().upper() == _PREFERRED_IMAGE_SCENE]
    for entry in (*preferred, *entries):
        url = str(entry.get('url') or '').strip()
        if url:
            return url
    return ''


def pick_video_url(stream: Any) -> str:
    """The best playable stream in a note's ``stream`` block, by codec preference."""
    if not isinstance(stream, dict):
        return ''
    for codec in _VIDEO_CODEC_PREFERENCE:
        variants = stream.get(codec)
        for variant in variants if isinstance(variants, list) else []:
            url = str(variant.get('master_url') or '').strip() if isinstance(variant, dict) else ''
            if url:
                return url
    return ''


def build_note_url(note_id: str, xsec_token: str = '') -> str:
    """The page a note lives on. Without the token it only opens for its author."""
    url = f'{_WEB_ORIGIN}/explore/{note_id}'
    if not xsec_token:
        return url
    return f'{url}?xsec_token={xsec_token}&xsec_source={_XSEC_SOURCE}'


def build_media_filename(media: XhsMedia, *, ext: str = '') -> str:
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


def build_ytdlp_command(*, note_url: str, cookie_path: Path, output_template: Path) -> list[str]:
    """The yt-dlp invocation for one video note.

    The output template is the note id alone: yt-dlp cannot know the repository's
    filename shape, so the file is renamed once it is on disk.
    """
    return [
        'yt-dlp',
        '-o',
        str(output_template),
        '--no-mtime',
        '--cookies',
        str(cookie_path),
        '--referer',
        f'{_WEB_ORIGIN}/',
        '--add-header',
        f'User-Agent:{_USER_AGENT}',
        '-N',
        '4',
        '--retries',
        '15',
        '--fragment-retries',
        '15',
        '--socket-timeout',
        '30',
        note_url,
    ]


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


def extract_note_media(note_card: dict[str, Any], *, note_id: str, xsec_token: str) -> list[XhsMedia]:
    """One row per file in a note.

    Indices are handed out in the order the note lists its files, an image and the
    clip of a live photo counting separately. The numbering has to be reproducible
    from the same note: refreshing an expired URL looks the row up by it.

    A video note yields a row even when no stream URL can be read out of it, because
    yt-dlp downloads it from the note page rather than from that URL.
    """
    user = note_card.get('user')
    user = user if isinstance(user, dict) else {}
    note_type = str(note_card.get('type') or '').strip()
    shared = {
        'note_id': note_id,
        'title': str(note_card.get('title') or '').strip(),
        'author': str(user.get('nickname') or '').strip() or 'unknown',
        'author_id': str(user.get('user_id') or '').strip(),
        'note_type': note_type,
        'published_at': format_published_at(note_card.get('time')),
        'xsec_token': xsec_token,
    }

    if note_type == _VIDEO_NOTE_TYPE:
        video = note_card.get('video')
        media_block = video.get('media') if isinstance(video, dict) else None
        stream = media_block.get('stream') if isinstance(media_block, dict) else None
        return [XhsMedia(media_index=1, media_type=_VIDEO_MEDIA_TYPE, media_url=pick_video_url(stream), **shared)]

    image_list = note_card.get('image_list')
    media: list[XhsMedia] = []
    for entry in image_list if isinstance(image_list, list) else []:
        if not isinstance(entry, dict):
            continue
        image_url = pick_image_url(entry.get('info_list'))
        if image_url:
            media.append(XhsMedia(media_index=len(media) + 1, media_type=_IMAGE_MEDIA_TYPE, media_url=image_url, **shared))
        # An image with a stream attached is a live photo: the still and the clip
        # are both kept, as two files.
        live_url = pick_video_url(entry.get('stream'))
        if live_url:
            media.append(XhsMedia(media_index=len(media) + 1, media_type=_LIVE_MEDIA_TYPE, media_url=live_url, **shared))
    return media


def should_retry_api_exception(exc: Exception) -> bool:
    return isinstance(exc, (httpx.ConnectError, httpx.TimeoutException, httpx.RemoteProtocolError))


def is_note_gone(message: str) -> bool:
    return any(marker in message for marker in _NOTE_GONE_MARKERS)


class Xiaohongshu:
    def __init__(self) -> None:
        self.cfg = settings.load().web.xiaohongshu
        self.client = httpx.AsyncClient(
            headers={
                'User-Agent': _USER_AGENT,
                'Accept': 'application/json, text/plain, */*',
                'Accept-Language': 'zh-CN,zh;q=0.9',
                'Origin': _WEB_ORIGIN,
                'Referer': f'{_WEB_ORIGIN}/',
            },
            timeout=60,
            follow_redirects=True,
            limits=httpx.Limits(max_keepalive_connections=10, max_connections=10),
        )
        self.signer = Xhshow()
        # Per endpoint, not per run: the list and the feed do not have to agree on
        # which envelope they accept, and one global setting would flip-flop between
        # them, paying a rejected request every time.
        self.sign_formats: dict[str, str] = {}
        self.cookies: dict[str, str] = {}
        self.user_id = ''

    async def aclose(self) -> None:
        await self.client.aclose()

    # ---------- state ----------

    async def _ensure_table(self) -> None:
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS xiaohongshu (
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
            CREATE TABLE IF NOT EXISTS xiaohongshu_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
        """)

    async def _backfill_complete(self) -> bool:
        rows = await database.query_db('SELECT value FROM xiaohongshu_state WHERE key = ?;', (_BACKFILL_STATE_KEY,))
        return bool(rows) and str(rows[0].get('value')) == '1'

    async def _mark_backfill_complete(self) -> None:
        await database.query_db(
            'INSERT OR IGNORE INTO xiaohongshu_state (key, value) VALUES (?, ?);',
            (_BACKFILL_STATE_KEY, '1'),
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
            f'SELECT DISTINCT note_id FROM xiaohongshu WHERE note_id IN ({placeholders});',  # noqa: S608 - placeholders only
            tuple(unique),
        )
        return {str(row.get('note_id') or '') for row in rows}

    async def _upsert_media(self, items: Sequence[XhsMedia]) -> None:
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
            table='xiaohongshu',
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
            # Refreshes what Xiaohongshu rotates without touching download state.
            on_conflict=(
                '(note_id, media_index) DO UPDATE SET '
                'media_url = excluded.media_url, '
                'xsec_token = excluded.xsec_token, '
                'title = excluded.title, '
                'author = excluded.author, '
                'published_at = excluded.published_at'
            ),
        )

    async def _pending_media(self) -> list[XhsMedia]:
        rows = await database.query_db("""
            SELECT note_id, media_index, media_type, media_url, title, author, author_id, note_type, published_at, xsec_token
            FROM xiaohongshu
            WHERE downloaded = 0 AND unavailable = 0
            ORDER BY created_at ASC;
        """)
        pending: list[XhsMedia] = []
        for row in rows:
            note_id = str(row.get('note_id') or '').strip()
            media_index = _to_int(row.get('media_index'))
            if not note_id or media_index is None:
                continue
            pending.append(
                XhsMedia(
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
                ),
            )
        return pending

    async def _mark_downloaded(self, media: XhsMedia, dst_path: Path) -> None:
        await database.query_db(
            """
            UPDATE xiaohongshu
            SET downloaded = 1, local_path = ?, unavailable = 0, failed_count = 0, last_error = ''
            WHERE note_id = ? AND media_index = ?;
            """,
            (str(dst_path), media.note_id, str(media.media_index)),
        )

    async def _mark_unavailable(self, media: XhsMedia, reason: str) -> None:
        await database.query_db(
            """
            UPDATE xiaohongshu
            SET unavailable = 1, failed_count = failed_count + 1, last_error = ?
            WHERE note_id = ? AND media_index = ?;
            """,
            (reason[:500], media.note_id, str(media.media_index)),
        )

    async def _mark_failed(self, media: XhsMedia, reason: str) -> None:
        await database.query_db(
            """
            UPDATE xiaohongshu
            SET failed_count = failed_count + 1, last_error = ?
            WHERE note_id = ? AND media_index = ?;
            """,
            (reason[:500], media.note_id, str(media.media_index)),
        )

    # ---------- session and signed requests ----------

    def _load_session(self, cookie_path: Path) -> dict[str, str]:
        """Refresh the Xiaohongshu session from CookieCloud, in both shapes a run needs.

        The dictionary signs the API requests; the file on disk is what yt-dlp reads.
        """
        client = CookieCloudClient(
            self.cfg.cookiecloud.server_url,
            self.cfg.cookiecloud.uuid,
            self.cfg.cookiecloud.password,
        )
        try:
            cookies = client.get_cookie_dict(XIAOHONGSHU_PROFILE.domains)
            client.save_to_netscape_format(XIAOHONGSHU_PROFILE.domains, cookie_path)
        except ValueError as exc:
            msg = f'CookieCloud carries no xiaohongshu.com cookies ({exc}). Sign in in the browser and let the extension sync.'
            raise XiaohongshuError(msg, notification_dedupe_key='xiaohongshu:auth') from exc
        finally:
            client.client.close()

        missing = [name for name in XIAOHONGSHU_PROFILE.required_cookies if not cookies.get(name)]
        if missing:
            msg = f'The CookieCloud vault has no {", ".join(missing)} for xiaohongshu.com. Sign in again so the extension re-syncs.'
            raise XiaohongshuError(msg, notification_dedupe_key='xiaohongshu:auth')
        return cookies

    def _signed_headers(
        self,
        *,
        method: Literal['GET', 'POST'],
        uri: str,
        params: dict[str, Any] | None = None,
        payload: dict[str, Any] | None = None,
        x_rap: bool = False,
    ) -> dict[str, str]:
        """The only place xhshow is called, so an upstream rename lands in one spot."""
        headers = self.signer.sign_headers(
            method,
            uri,
            self.cookies,
            params=params,
            payload=payload,
            sign_format=self._sign_format_for(uri),
            user_id=self.user_id or None,
            x_rap=x_rap,
        )
        headers['Cookie'] = '; '.join(f'{name}={value}' for name, value in self.cookies.items())
        return headers

    async def _send_signed(
        self,
        *,
        method: Literal['GET', 'POST'],
        uri: str,
        params: dict[str, Any] | None,
        payload: dict[str, Any] | None,
        x_rap: bool,
    ) -> httpx.Response:
        headers = self._signed_headers(method=method, uri=uri, params=params, payload=payload, x_rap=x_rap)
        if method == 'GET':
            # xhshow's own encoder, not httpx's: the signature is computed over this
            # exact query string, and a differently escaped one fails verification.
            return await self.client.get(self.signer.build_url(f'{_API_ORIGIN}{uri}', params), headers=headers)
        headers['Content-Type'] = 'application/json;charset=UTF-8'
        return await self.client.post(f'{_API_ORIGIN}{uri}', content=self.signer.build_json_body(payload or {}), headers=headers)

    def _sign_format_for(self, uri: str) -> str:
        return self.sign_formats.get(uri, _SIGN_FORMATS[0])

    def _rotate_sign_format(self, uri: str) -> str:
        previous = self._sign_format_for(uri)
        self.sign_formats[uri] = next(candidate for candidate in _SIGN_FORMATS if candidate != previous)
        return previous

    @staticmethod
    def _parse_body(response: httpx.Response) -> dict[str, Any]:
        payload = response.json()
        if not isinstance(payload, dict):
            msg = 'Xiaohongshu API returned a body that is not an object'
            raise TypeError(msg)

        code = _to_int(payload.get('code'))
        message = str(payload.get('msg') or payload.get('message') or '').strip()
        if code in _SESSION_EXPIRED_CODES:
            msg = (
                f'The Xiaohongshu session is no longer signed in ({code} {message}). Sign in again so CookieCloud picks up a fresh session.'
            )
            raise XiaohongshuError(msg, notification_dedupe_key='xiaohongshu:auth')
        if code not in {None, 0}:
            detail = f' ({message})' if message else ''
            msg = f'Xiaohongshu API returned error code: {code}{detail}'
            raise ValueError(msg)

        data = payload.get('data')
        return data if isinstance(data, dict) else {}

    async def _request_api(
        self,
        *,
        method: Literal['GET', 'POST'],
        uri: str,
        params: dict[str, Any] | None = None,
        payload: dict[str, Any] | None = None,
        x_rap: bool = False,
    ) -> dict[str, Any]:
        attempts = len(_API_RETRY_DELAYS_SECONDS) + 1
        rotated = False

        for attempt in range(1, attempts + 1):
            try:
                response = await self._send_signed(method=method, uri=uri, params=params, payload=payload, x_rap=x_rap)
            except Exception as exc:
                if not should_retry_api_exception(exc) or attempt >= attempts:
                    raise
                delay = _API_RETRY_DELAYS_SECONDS[attempt - 1]
                log.warning(
                    'Xiaohongshu %s failed attempt=%s/%s, retry in %.1fs: %s: %s', uri, attempt, attempts, delay, type(exc).__name__, exc
                )
                await asyncio.sleep(delay)
                continue

            status = response.status_code
            if status in _RISK_CONTROL_STATUS_CODES:
                await response.aclose()
                msg = f'Xiaohongshu answered {uri} with HTTP {status}: the account is behind a captcha. Clear it in the browser.'
                raise XiaohongshuError(msg, notification_dedupe_key='xiaohongshu:risk')
            if status == _SIGN_REJECTED_STATUS and not rotated:
                await response.aclose()
                rotated = True
                previous = self._rotate_sign_format(uri)
                log.warning(
                    'Xiaohongshu rejected the %s signature on %s (HTTP %s), retrying as %s',
                    previous,
                    uri,
                    status,
                    self._sign_format_for(uri),
                )
                continue
            if status in _API_RETRYABLE_STATUS_CODES and attempt < attempts:
                await response.aclose()
                delay = _API_RETRY_DELAYS_SECONDS[attempt - 1]
                log.warning('Xiaohongshu %s returned HTTP %s attempt=%s/%s, retry in %.1fs', uri, status, attempt, attempts, delay)
                await asyncio.sleep(delay)
                continue

            response.raise_for_status()
            return self._parse_body(response)

        msg = f'Xiaohongshu request to {uri} exhausted its retries'
        raise RuntimeError(msg)

    async def _sleep_between_requests(self) -> None:
        if self.cfg.sleep_request_seconds > 0:
            await asyncio.sleep(self.cfg.sleep_request_seconds)

    # ---------- API calls ----------

    async def _resolve_user_id(self) -> str:
        if self.cfg.user_id:
            return self.cfg.user_id
        data = await self._request_api(method='GET', uri=_USER_ME_URI)
        user_id = str(data.get('user_id') or '').strip()
        if not user_id:
            msg = 'Could not read the signed-in Xiaohongshu profile id from the session; set user_id in the settings to skip this lookup.'
            raise XiaohongshuError(msg, notification_dedupe_key='xiaohongshu:auth')
        return user_id

    async def _fetch_like_page(self, *, cursor: str) -> tuple[list[NoteRef], str, bool]:
        data = await self._request_api(
            method='GET',
            uri=_LIKE_PAGE_URI,
            params={'user_id': self.user_id, 'num': str(_PAGE_SIZE), 'cursor': cursor},
        )
        return extract_like_page(data)

    async def _fetch_note_card(self, *, note_id: str, xsec_token: str) -> dict[str, Any]:
        data = await self._request_api(
            method='POST',
            uri=_FEED_URI,
            payload={
                'source_note_id': note_id,
                'image_formats': list(_IMAGE_FORMATS),
                'extra': {'need_body_topic': 1},
                'xsec_source': _XSEC_SOURCE,
                'xsec_token': xsec_token,
            },
            # The feed endpoint carries its own risk-control header.
            x_rap=True,
        )
        items = data.get('items')
        for item in items if isinstance(items, list) else []:
            note_card = item.get('note_card') if isinstance(item, dict) else None
            if isinstance(note_card, dict):
                return note_card
        return {}

    # ---------- downloads ----------

    def _destination_root(self, media_type: str) -> Path:
        if self.cfg.video_path is not None and media_type in _VIDEO_MEDIA_TYPES:
            return self.cfg.video_path
        return self.cfg.path

    def _build_output_path(self, media: XhsMedia, *, ext: str = '') -> Path:
        author_dir = sanitize(media.author) or 'unknown'
        return self._destination_root(media.media_type) / author_dir / build_media_filename(media, ext=ext)

    async def _refresh_media_url(self, media: XhsMedia) -> str:
        """Re-resolve a note whose file URL has rotated, and store the new one.

        The row is matched back by index and type, so a note that has since been
        edited into a different shape refreshes nothing instead of the wrong file.
        """
        note_card = await self._fetch_note_card(note_id=media.note_id, xsec_token=media.xsec_token)
        if not note_card:
            return ''
        fresh = extract_note_media(note_card, note_id=media.note_id, xsec_token=media.xsec_token)
        match = next((item for item in fresh if item.media_index == media.media_index and item.media_type == media.media_type), None)
        if match is None or not match.media_url or match.media_url == media.media_url:
            return ''

        await database.query_db(
            'UPDATE xiaohongshu SET media_url = ? WHERE note_id = ? AND media_index = ?;',
            (match.media_url, media.note_id, str(media.media_index)),
        )
        log.info('Refreshed the file URL of note=%s index=%s', media.note_id, media.media_index)
        return match.media_url

    async def _fetch_file(self, url: str) -> httpx.Response:
        response = await self.client.get(url, headers={'Accept': _MEDIA_ACCEPT})
        response.raise_for_status()
        return response

    async def _download_file(self, media: XhsMedia) -> Path:
        """An image or a live photo's clip, straight from the CDN."""
        dst_path = self._build_output_path(media)
        await asyncio.to_thread(dst_path.parent.mkdir, parents=True, exist_ok=True)
        if await asyncio.to_thread(dst_path.exists):
            return dst_path
        if not media.media_url:
            msg = 'the note carries no URL for this file'
            raise MediaUnavailableError(msg)

        try:
            response = await self._fetch_file(media.media_url)
        except httpx.HTTPStatusError as exc:
            status = exc.response.status_code
            if status not in _MEDIA_STALE_STATUS_CODES:
                raise
            refreshed = await self._refresh_media_url(media)
            if not refreshed:
                if status in _MEDIA_UNAVAILABLE_STATUS_CODES:
                    msg = f'http {status} and the note no longer offers this file'
                    raise MediaUnavailableError(msg) from exc
                raise
            response = await self._fetch_file(refreshed)

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
        command = build_ytdlp_command(note_url=note_url, cookie_path=cookie_path, output_template=cache_dir / f'{note_id}.%(ext)s')

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

    def _move_download(self, *, cache_dir: Path, media: XhsMedia) -> Path:
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

    async def _download_video(self, media: XhsMedia, *, cookie_path: Path, cache_dir: Path) -> Path:
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

    async def _download_one(self, media: XhsMedia, *, cookie_path: Path, cache_dir: Path) -> Path:
        if media.media_type == _VIDEO_MEDIA_TYPE:
            # yt-dlp reads the note page off xiaohongshu.com itself, so it is paced
            # like an API request. The CDN the other files come from is not metered
            # that way, and pacing it would add hours to a first backfill.
            await self._sleep_between_requests()
            return await self._download_video(media, cookie_path=cookie_path, cache_dir=cache_dir)
        return await self._download_file(media)

    async def _download_pending(self, items: Sequence[XhsMedia], *, cookie_path: Path, cache_dir: Path) -> int:
        downloaded = 0
        total = len(items)
        for position, media in enumerate(items, start=1):
            log.info(
                'Xiaohongshu downloading progress=%s/%s note=%s index=%s type=%s',
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
            except httpx.HTTPStatusError as exc:
                await self._mark_failed(media, f'http {exc.response.status_code}')
                log.warning('Failed note=%s index=%s: http %s', media.note_id, media.media_index, exc.response.status_code)
                continue
            except XiaohongshuError:
                raise
            except Exception as exc:  # noqa: BLE001
                await self._mark_failed(media, f'{type(exc).__name__}: {exc}')
                log.warning('Failed note=%s index=%s: %s', media.note_id, media.media_index, exc)
                continue

            await self._mark_downloaded(media, dst_path)
            downloaded += 1
            log.notice('Xiaohongshu downloaded progress=%s/%s file=%s', position, total, dst_path.name)
        return downloaded

    # ---------- phases ----------

    async def _crawl(self) -> int:
        """Walk the likes list newest first, resolving each page's new notes as it goes.

        Rows land page by page rather than at the end of the walk, so a run that dies
        part-way leaves the next one with less to re-fetch: an already-resolved note
        costs one place in a list page instead of a request of its own.

        The early stop only applies once a full walk has finished, recorded in
        ``xiaohongshu_state``. Stopping at the top of the list before that would leave
        everything below wherever the first run got to permanently unreachable.
        """
        backfill_complete = await self._backfill_complete()
        seen_cursors: set[str] = set()
        seen_note_ids: set[str] = set()
        cursor = ''
        page_index = 0
        full_hit_pages = 0
        resolved = 0
        reached_end = False

        log.info('Xiaohongshu walking the likes list (backfill_complete=%s)', backfill_complete)
        while True:
            page_index += 1
            if page_index > 1:
                await self._sleep_between_requests()
            notes, next_cursor, has_more = await self._fetch_like_page(cursor=cursor)
            if not notes:
                log.info('Xiaohongshu likes list ended at page=%s', page_index)
                reached_end = True
                break

            known = await self._known_note_ids([note.note_id for note in notes])
            unknown = [note for note in notes if note.note_id not in known and note.note_id not in seen_note_ids]
            seen_note_ids.update(note.note_id for note in notes)
            log.info('Xiaohongshu likes page=%s notes=%s new=%s', page_index, len(notes), len(unknown))

            if unknown:
                full_hit_pages = 0
                resolved += await self._resolve_notes(unknown)
            else:
                full_hit_pages += 1
                log.info('Xiaohongshu page=%s is entirely archived, consecutive=%s/%s', page_index, full_hit_pages, self.cfg.abort_after)
                if backfill_complete and full_hit_pages >= self.cfg.abort_after:
                    log.info('Xiaohongshu reached the incremental stop condition')
                    break

            if not has_more:
                log.info('Xiaohongshu likes list reported no further pages')
                reached_end = True
                break
            if not next_cursor or next_cursor in seen_cursors:
                log.warning('Xiaohongshu returned cursor %r again, stop pagination', next_cursor)
                break
            seen_cursors.add(next_cursor)
            cursor = next_cursor

        if reached_end and not backfill_complete:
            await self._mark_backfill_complete()
            log.info('Xiaohongshu backfill finished; later runs stop after %d archived pages', self.cfg.abort_after)
        return resolved

    async def _resolve_notes(self, notes: Sequence[NoteRef]) -> int:
        """Turn each new note into its rows. One unreadable note must not end the run."""
        resolved = 0
        total = len(notes)
        for position, note in enumerate(notes, start=1):
            await self._sleep_between_requests()
            try:
                note_card = await self._fetch_note_card(note_id=note.note_id, xsec_token=note.xsec_token)
            except XiaohongshuError:
                raise
            except Exception as exc:  # noqa: BLE001
                log.warning('Failed to read note %s (%s/%s): %s', note.note_id, position, total, exc)
                continue

            media = extract_note_media(note_card, note_id=note.note_id, xsec_token=note.xsec_token)
            if not media:
                log.warning('Note %s carries no files, skipping', note.note_id)
                continue
            await self._upsert_media(media)
            resolved += len(media)
        return resolved

    async def _notify_summary(self, *, downloaded: int, resolved: int, pending: int) -> None:
        try:
            await enqueue_notification(
                kind='summary',
                source='xiaohongshu',
                title='Xiaohongshu update completed',
                body=f'Downloaded {downloaded} files, {resolved} of them newly liked.',
                payload={'downloaded': downloaded, 'resolved': resolved, 'pending': pending},
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue xiaohongshu summary notification: %s', exc)

    async def update(self) -> None:
        missing = self.cfg.validate_runnable()
        if missing:
            log.warning('Xiaohongshu is not configured (missing %s), skip update', ', '.join(missing))
            return

        await self._ensure_table()
        await asyncio.to_thread(self.cfg.path.mkdir, parents=True, exist_ok=True)
        if self.cfg.video_path is not None:
            await asyncio.to_thread(self.cfg.video_path.mkdir, parents=True, exist_ok=True)

        with tempfile.TemporaryDirectory() as tmp_dir:
            cookie_path = Path(tmp_dir) / _COOKIE_FILENAME
            cache_dir = Path(tmp_dir) / _VIDEO_CACHE_DIRNAME
            self.cookies = await asyncio.to_thread(self._load_session, cookie_path)
            self.user_id = await self._resolve_user_id()

            resolved = await self._crawl()
            pending = await self._pending_media()
            downloaded = await self._download_pending(pending, cookie_path=cookie_path, cache_dir=cache_dir)

        log.info('Xiaohongshu resolved %d new files and downloaded %d of %d pending', resolved, downloaded, len(pending))
        if downloaded:
            await self._notify_summary(downloaded=downloaded, resolved=resolved, pending=len(pending))
