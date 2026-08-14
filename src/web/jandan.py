"""Jandan favorites scraper."""

from __future__ import annotations

import asyncio
import base64
import json
import re
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any
from urllib.parse import quote, unquote, urlsplit, urlunsplit

import httpx
from Crypto.Cipher import AES

from src.core import logger, settings
from src.tool import database, format_video_filename
from src.tool.notifications import enqueue_notification

log = logger.get('jandan')

_APP_VERSION = '2.6.0'
_USER_AGENT = f'jian dan/{_APP_VERSION} (iPhone; iOS 26.1; Scale/3.00)'
_ACCEPT_VERSION = '1.0.0'
_JD_KEY = b'jD8sW=!' + (b'\x00' * 25)
_JD_IV = b'\x00' * 16
_AES_BLOCK_SIZE = 16
_DEFAULT_OFFSET_HOURS = -8
_FAV_TYPE_DIR = {
    1: 'wuliao',
    2: 'snapshot',
    6: 'girls',
}
_IMAGE_ACCEPT = 'image/*,*/*;q=0.8'
_IMAGE_REFERER = 'https://jandan.net/'
_REFERER_HOST_SUFFIXES = ('wangmoyu.com',)
_EXT_ALIASES = {
    'jpeg': 'jpg',
    'jpg': 'jpg',
    'png': 'png',
    'gif': 'gif',
    'webp': 'webp',
    'bmp': 'bmp',
}
_PLACEHOLDER_IMAGE_PATH_MARKER = '/images/default_'
_UNAVAILABLE_STATUS_CODES = {404, 410}
_SINAIMG_FORBIDDEN_STATUS = 403
_LARGE_VARIANT_HOST_SUFFIXES = ('sinaimg.cn', 'wangmoyu.com')
_MW_VARIANT_SEGMENT_RE = re.compile(r'/mw\d+/')
# Sina serves one object from interchangeable numbered shards (wx1..wx4, ww1..ww4, tva1..tva4).
# A 403 is often shard-local, so the same path is worth retrying on its siblings.
_SINAIMG_SHARD_HOST_RE = re.compile(r'^(?P<prefix>[a-z]+)(?P<shard>[1-4])\.sinaimg\.cn$')
_SINAIMG_SHARDS = ('1', '2', '3', '4')
_FULL_HIT_STOP_PAGES = 2
_API_RETRY_DELAYS_SECONDS = (2.0, 5.0, 15.0, 30.0)
_API_RETRYABLE_STATUS_CODES = {429, 502, 503, 504}
_FAV_LIST_KEYS_BY_TYPE: dict[int, tuple[str, ...]] = {
    1: ('pics',),
    2: ('girls',),
    6: ('nzs',),
}


@dataclass(frozen=True, slots=True)
class JandanImage:
    fav_type: int
    pic_id: int
    content_index: int
    content_url: str
    content_md5: str | None
    author: str
    post_date: str | None
    image_type: str | None


class UnavailableImageError(RuntimeError):
    """Raised when an image URL is confirmed unavailable/deleted."""


def _pkcs7_unpad(data: bytes, block_size: int = _AES_BLOCK_SIZE) -> bytes:
    if not data or len(data) % block_size != 0:
        msg = 'invalid AES payload size'
        raise ValueError(msg)
    pad = data[-1]
    if pad < 1 or pad > block_size:
        msg = 'invalid PKCS7 padding length'
        raise ValueError(msg)
    if data[-pad:] != bytes([pad]) * pad:
        msg = 'invalid PKCS7 padding bytes'
        raise ValueError(msg)
    return data[:-pad]


def _pkcs7_pad(data: bytes, block_size: int = _AES_BLOCK_SIZE) -> bytes:
    pad = block_size - (len(data) % block_size)
    return data + bytes([pad]) * pad


def decrypt_data_field(data_value: str) -> str:
    raw_b64 = unquote(data_value)
    raw_b64 += '=' * ((4 - len(raw_b64) % 4) % 4)
    ciphertext = base64.b64decode(raw_b64)
    plain_padded = AES.new(_JD_KEY, AES.MODE_CBC, iv=_JD_IV).decrypt(ciphertext)
    plain = _pkcs7_unpad(plain_padded)
    return plain.decode('utf-8')


def encrypt_data_field(plain_json: str) -> str:
    plaintext = plain_json.encode('utf-8')
    padded = _pkcs7_pad(plaintext)
    ciphertext = AES.new(_JD_KEY, AES.MODE_CBC, iv=_JD_IV).encrypt(padded)
    b64 = base64.b64encode(ciphertext).decode('ascii')
    return quote(b64, safe='')


def build_fav_request(
    *,
    user_id: int,
    fav_type: int,
    fav_num_limit: int = 45,
    action_type: str = 'get',
    fav_start_date: str | None = None,
) -> dict[str, Any]:
    payload: dict[str, Any] = {
        'fav': {
            'favType': str(fav_type),
            'favNumLimit': fav_num_limit,
            'actionType': action_type,
            'userID': user_id,
        },
    }
    if fav_start_date:
        payload['fav']['favStartDate'] = fav_start_date
    return payload


def zulu_to_offset(date_z: str, offset_hours: int = _DEFAULT_OFFSET_HOURS) -> str:
    normalized = f'{date_z[:-1]}+00:00' if date_z.endswith('Z') else date_z
    dt_utc = datetime.fromisoformat(normalized)
    tz = timezone(timedelta(hours=offset_hours))
    local = dt_utc.astimezone(tz)
    return local.strftime('%Y-%m-%dT%H:%M:%S.%f')[:-3] + local.strftime('%z')


def _to_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value)
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    return None


def _normalize_image_url(url: str) -> str:
    normalized = url.strip()
    if normalized.startswith('//'):
        return f'https:{normalized}'
    return normalized


def infer_image_extension(content_url: str, image_type: str | None = None) -> str:
    if image_type:
        normalized_type = image_type.strip().lower()
        mapped = _EXT_ALIASES.get(normalized_type)
        if mapped:
            return mapped

    suffix = Path(urlsplit(content_url).path).suffix.lower().removeprefix('.')
    mapped = _EXT_ALIASES.get(suffix)
    if mapped:
        return mapped
    return 'jpg'


def extract_last_pic_date(pics: list[dict[str, Any]]) -> str | None:
    for pic in reversed(pics):
        date_value = pic.get('date')
        if isinstance(date_value, str) and date_value.strip():
            return date_value.strip()
    return None


def extract_pic_images(*, fav_type: int, pics: list[dict[str, Any]]) -> list[JandanImage]:
    images: list[JandanImage] = []
    for pic in pics:
        pic_id = _to_int(pic.get('id'))
        if pic_id is None:
            continue

        author = str(pic.get('author') or 'unknown').strip() or 'unknown'
        post_date = str(pic.get('date') or '').strip() or None
        contents = pic.get('contents')
        if not isinstance(contents, list):
            continue

        for index, content in enumerate(contents, start=1):
            if not isinstance(content, dict):
                continue
            if str(content.get('contentType') or '').strip().lower() != 'image':
                continue

            content_url = content.get('content')
            if not isinstance(content_url, str) or not content_url.strip():
                continue

            md5_value = content.get('md5')
            md5_text = str(md5_value).strip() if isinstance(md5_value, str) and md5_value.strip() else None
            image_type_value = content.get('imageType')
            image_type = str(image_type_value).strip().lower() if isinstance(image_type_value, str) and image_type_value.strip() else None

            images.append(
                JandanImage(
                    fav_type=fav_type,
                    pic_id=pic_id,
                    content_index=index,
                    content_url=_normalize_image_url(content_url),
                    content_md5=md5_text,
                    author=author,
                    post_date=post_date,
                    image_type=image_type,
                ),
            )
    return images


def extract_fav_items(*, fav_type: int, fav: dict[str, Any]) -> list[dict[str, Any]]:
    list_keys: list[str] = []
    for key in _FAV_LIST_KEYS_BY_TYPE.get(fav_type, ()):
        if key not in list_keys:
            list_keys.append(key)
    if 'pics' not in list_keys:
        list_keys.append('pics')

    for key in list_keys:
        items = fav.get(key)
        if isinstance(items, list):
            return [item for item in items if isinstance(item, dict)]

    for items in fav.values():
        if isinstance(items, list):
            return [item for item in items if isinstance(item, dict)]
    return []


def detect_deleted_placeholder(response: httpx.Response) -> tuple[bool, str | None]:
    final_path = response.url.path.lower()
    if _PLACEHOLDER_IMAGE_PATH_MARKER in final_path:
        return True, f'placeholder path: {response.url}'

    history_with_location = [r for r in response.history if r.headers.get('location')]
    for redirect in history_with_location:
        location = str(redirect.headers.get('location') or '').lower()
        if _PLACEHOLDER_IMAGE_PATH_MARKER in location:
            errno = (redirect.headers.get('x-image-errno') or '').strip()
            reason = f'redirected to placeholder: {location}'
            if errno:
                reason = f'{reason} (errno={errno})'
            return True, reason

    return False, None


def should_mark_unavailable_http(*, status_code: int, url_host: str | None) -> bool:
    if status_code in _UNAVAILABLE_STATUS_CODES:
        return True

    normalized_host = (url_host or '').strip().lower()
    return status_code == _SINAIMG_FORBIDDEN_STATUS and normalized_host.endswith('sinaimg.cn')


def build_image_download_candidates(content_url: str) -> list[str]:
    base_url = _normalize_image_url(content_url)
    parsed = urlsplit(base_url)
    host = (parsed.hostname or '').lower()
    path = parsed.path

    supports_large_variant = any(host.endswith(suffix) for suffix in _LARGE_VARIANT_HOST_SUFFIXES)
    if not supports_large_variant or not _MW_VARIANT_SEGMENT_RE.search(path):
        return [base_url]

    large_path = _MW_VARIANT_SEGMENT_RE.sub('/large/', path, count=1)
    large_url = urlunsplit((parsed.scheme, parsed.netloc, large_path, parsed.query, parsed.fragment))
    if large_url == base_url:
        return [base_url]
    return [large_url, base_url]


def build_image_request_headers(url: str) -> dict[str, str]:
    headers = {
        'Accept': _IMAGE_ACCEPT,
        'Connection': 'keep-alive',
    }
    host = (urlsplit(url).hostname or '').lower()
    if any(host.endswith(suffix) for suffix in _REFERER_HOST_SUFFIXES):
        headers['Referer'] = _IMAGE_REFERER
    return headers


def build_sinaimg_shard_candidates(url: str) -> list[str]:
    parsed = urlsplit(url)
    host = (parsed.hostname or '').lower()
    match = _SINAIMG_SHARD_HOST_RE.fullmatch(host)
    if match is None:
        return []

    prefix = match.group('prefix')
    current_shard = match.group('shard')
    return [
        urlunsplit((parsed.scheme, f'{prefix}{shard}.sinaimg.cn', parsed.path, parsed.query, parsed.fragment))
        for shard in _SINAIMG_SHARDS
        if shard != current_shard
    ]


def should_retry_jandan_api_status(status_code: int) -> bool:
    return status_code in _API_RETRYABLE_STATUS_CODES


def should_retry_jandan_api_exception(exc: Exception) -> bool:
    return isinstance(exc, (httpx.ConnectError, httpx.TimeoutException, httpx.RemoteProtocolError))


class Jandan:
    def __init__(self) -> None:
        self.cfg = settings.load().web.jandan
        self.client = httpx.AsyncClient(
            headers={
                'User-Agent': _USER_AGENT,
                'Accept-Language': 'en-US;q=1, zh-Hans-US;q=0.9',
            },
            timeout=60,
            follow_redirects=True,
            limits=httpx.Limits(max_keepalive_connections=10, max_connections=10),
        )

    async def aclose(self) -> None:
        await self.client.aclose()

    async def _notify_summary(
        self,
        *,
        total_downloaded: int,
        total_api_images: int,
        fav_type_stats: list[tuple[int, int, int, int, int]],
    ) -> None:
        payload_stats = [
            {
                'fav_type': fav_type,
                'page_count': page_count,
                'api_total_count': api_total_count,
                'collected_count': collected_count,
                'downloaded_count': downloaded_count,
            }
            for fav_type, page_count, api_total_count, collected_count, downloaded_count in fav_type_stats
        ]
        try:
            await enqueue_notification(
                kind='summary',
                source='jandan',
                header='Jandan',
                title='Update completed',
                body=f'Downloaded {total_downloaded} images from {total_api_images} API images.',
                payload={
                    'total_downloaded': total_downloaded,
                    'total_api_images': total_api_images,
                    'fav_type_stats': payload_stats,
                },
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue jandan summary notification: %s', exc)

    async def _ensure_table(self) -> None:
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS jandan (
                fav_type INTEGER NOT NULL,
                pic_id INTEGER NOT NULL,
                content_index INTEGER NOT NULL,
                content_md5 TEXT,
                content_url TEXT NOT NULL,
                author TEXT NOT NULL DEFAULT '',
                post_date TEXT NOT NULL DEFAULT '',
                local_path TEXT NOT NULL DEFAULT '',
                downloaded INTEGER NOT NULL DEFAULT 0,
                unavailable INTEGER NOT NULL DEFAULT 0,
                failed_count INTEGER NOT NULL DEFAULT 0,
                last_error TEXT NOT NULL DEFAULT '',
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (fav_type, pic_id, content_index)
            );
        """)
        columns = await database.query_db('PRAGMA table_info(jandan);')
        column_names = {str(col.get('name') or '') for col in columns}
        alter_statements: list[tuple[str, tuple[str, ...]]] = []
        if 'unavailable' not in column_names:
            alter_statements.append(('ALTER TABLE jandan ADD COLUMN unavailable INTEGER NOT NULL DEFAULT 0;', ()))
        if 'failed_count' not in column_names:
            alter_statements.append(('ALTER TABLE jandan ADD COLUMN failed_count INTEGER NOT NULL DEFAULT 0;', ()))
        if 'last_error' not in column_names:
            alter_statements.append(("ALTER TABLE jandan ADD COLUMN last_error TEXT NOT NULL DEFAULT '';", ()))
        if alter_statements:
            await database.query_db_batch(alter_statements)

    def _build_output_path(self, image: JandanImage) -> Path:
        ext = infer_image_extension(image.content_url, image.image_type)
        file_id = f'{image.pic_id}-{image.content_index}'
        if image.content_md5:
            file_id = f'{file_id}-{image.content_md5[:8]}'
        filename = format_video_filename(
            title=image.author,
            video_id=file_id,
            uploader=f'type{image.fav_type}',
            ext=ext,
        )
        folder = _FAV_TYPE_DIR.get(image.fav_type, f'type-{image.fav_type}')
        return self.cfg.path / folder / filename

    async def _state_for_image(self, image: JandanImage) -> tuple[bool, bool]:
        rows = await database.query_db(
            """
            SELECT downloaded, unavailable
            FROM jandan
            WHERE fav_type = ? AND pic_id = ? AND content_index = ?
            LIMIT 1;
            """,
            (str(image.fav_type), str(image.pic_id), str(image.content_index)),
        )
        if not rows:
            return False, False
        downloaded = self._to_bool(rows[0].get('downloaded'))
        unavailable = self._to_bool(rows[0].get('unavailable'))
        return downloaded, unavailable

    @staticmethod
    def _image_key(image: JandanImage) -> tuple[int, int, int]:
        return image.fav_type, image.pic_id, image.content_index

    @staticmethod
    def _to_bool(value: Any) -> bool:
        if isinstance(value, bool):
            return value
        if isinstance(value, (int, float)):
            return int(value) == 1
        if isinstance(value, str):
            return value.strip() == '1'
        return False

    async def _mark_downloaded(self, image: JandanImage, dst_path: Path) -> None:
        await database.query_db(
            """
            UPDATE jandan
            SET downloaded = 1, local_path = ?, unavailable = 0, failed_count = 0, last_error = ''
            WHERE fav_type = ? AND pic_id = ? AND content_index = ?;
            """,
            (
                str(dst_path),
                str(image.fav_type),
                str(image.pic_id),
                str(image.content_index),
            ),
        )

    async def _mark_unavailable(self, image: JandanImage, reason: str) -> None:
        await database.query_db(
            """
            UPDATE jandan
            SET unavailable = 1, failed_count = failed_count + 1, last_error = ?
            WHERE fav_type = ? AND pic_id = ? AND content_index = ?;
            """,
            (
                reason[:500],
                str(image.fav_type),
                str(image.pic_id),
                str(image.content_index),
            ),
        )

    async def _mark_failed(self, image: JandanImage, reason: str) -> None:
        await database.query_db(
            """
            UPDATE jandan
            SET failed_count = failed_count + 1, last_error = ?
            WHERE fav_type = ? AND pic_id = ? AND content_index = ?;
            """,
            (
                reason[:500],
                str(image.fav_type),
                str(image.pic_id),
                str(image.content_index),
            ),
        )

    async def _upsert_images(self, images: list[JandanImage]) -> None:
        rows: list[tuple[str, ...]] = [
            (
                str(image.fav_type),
                str(image.pic_id),
                str(image.content_index),
                image.content_md5 or '',
                image.content_url,
                image.author,
                image.post_date or '',
            )
            for image in images
        ]
        await database.insert_db_batch(
            table='jandan',
            columns=(
                'fav_type',
                'pic_id',
                'content_index',
                'content_md5',
                'content_url',
                'author',
                'post_date',
            ),
            rows=rows,
            on_conflict=(
                '(fav_type, pic_id, content_index) DO UPDATE SET '
                'content_md5 = excluded.content_md5, '
                'content_url = excluded.content_url, '
                'author = excluded.author, '
                'post_date = excluded.post_date'
            ),
        )

    async def _processed_keys_for_page(self, *, page_keys: list[tuple[int, int, int]]) -> set[tuple[int, int, int]]:
        if not page_keys:
            return set()

        statements = [
            (
                """
                SELECT fav_type, pic_id, content_index
                FROM jandan
                WHERE fav_type = ? AND pic_id = ? AND content_index = ?
                  AND (downloaded = 1 OR unavailable = 1)
                LIMIT 1;
                """,
                (str(fav_type), str(pic_id), str(content_index)),
            )
            for fav_type, pic_id, content_index in page_keys
        ]
        statement_results = await database.query_db_batch(statements)
        keys: set[tuple[int, int, int]] = set()
        for key, rows in zip(page_keys, statement_results, strict=True):
            if not rows:
                continue
            keys.add(key)
        return keys

    async def _is_page_fully_processed(self, images: list[JandanImage]) -> tuple[bool, int, int]:
        page_keys = list({self._image_key(image) for image in images})
        if not page_keys:
            return False, 0, 0

        processed_keys = await self._processed_keys_for_page(page_keys=page_keys)
        processed_count = len(processed_keys.intersection(page_keys))
        total_count = len(page_keys)
        return processed_count == total_count, processed_count, total_count

    async def _pending_images_from_db(self, *, fav_type: int) -> list[JandanImage]:
        rows = await database.query_db(
            """
            SELECT fav_type, pic_id, content_index, content_url, content_md5, author, post_date
            FROM jandan
            WHERE fav_type = ? AND downloaded = 0 AND unavailable = 0
            ORDER BY created_at ASC;
            """,
            (str(fav_type),),
        )
        images: list[JandanImage] = []
        for row in rows:
            row_fav_type = _to_int(row.get('fav_type'))
            pic_id = _to_int(row.get('pic_id'))
            content_index = _to_int(row.get('content_index'))
            content_url = row.get('content_url')
            if row_fav_type is None or pic_id is None or content_index is None:
                continue
            if not isinstance(content_url, str) or not content_url.strip():
                continue
            content_md5 = row.get('content_md5')
            post_date = row.get('post_date')
            author = str(row.get('author') or 'unknown').strip() or 'unknown'
            images.append(
                JandanImage(
                    fav_type=row_fav_type,
                    pic_id=pic_id,
                    content_index=content_index,
                    content_url=_normalize_image_url(content_url),
                    content_md5=str(content_md5).strip() if isinstance(content_md5, str) and content_md5.strip() else None,
                    author=author,
                    post_date=str(post_date).strip() if isinstance(post_date, str) and post_date.strip() else None,
                    image_type=None,
                ),
            )
        return images

    async def _fetch_fav_page(self, *, fav_type: int, fav_start_date: str | None = None) -> list[dict[str, Any]]:
        request_payload = build_fav_request(
            user_id=self.cfg.user_id,
            fav_type=fav_type,
            fav_num_limit=self.cfg.fav_num_limit,
            fav_start_date=fav_start_date,
        )
        plain_payload = json.dumps(request_payload, ensure_ascii=False, separators=(',', ':'))
        data_value = encrypt_data_field(plain_payload)

        response = await self._post_fav_api(data_value=data_value, fav_type=fav_type)
        response.raise_for_status()

        payload = response.json()
        code = _to_int(payload.get('code'))
        if code not in {None, 0}:
            message = str(payload.get('msg') or payload.get('message') or '').strip()
            detail = f' ({message})' if message else ''
            msg = f'Jandan API returned error code: {code}{detail}'
            raise ValueError(msg)

        encrypted_data = payload.get('data')
        if not isinstance(encrypted_data, str) or not encrypted_data.strip():
            return []

        decoded = decrypt_data_field(encrypted_data)
        parsed = json.loads(decoded)
        fav = parsed.get('fav')
        if not isinstance(fav, dict):
            return []
        return extract_fav_items(fav_type=fav_type, fav=fav)

    async def _post_fav_api(self, *, data_value: str, fav_type: int) -> httpx.Response:
        headers = {
            'Accept-version': _ACCEPT_VERSION,
            'Content-Type': 'application/x-www-form-urlencoded',
        }
        last_exc: Exception | None = None
        attempts = len(_API_RETRY_DELAYS_SECONDS) + 1

        for attempt in range(1, attempts + 1):
            try:
                response = await self.client.post(
                    self.cfg.api_url,
                    content=f'data={data_value}',
                    headers=headers,
                )
            except Exception as exc:
                if not should_retry_jandan_api_exception(exc) or attempt >= attempts:
                    raise
                last_exc = exc
                delay = _API_RETRY_DELAYS_SECONDS[attempt - 1]
                log.warning(
                    'Jandan API request failed favType=%s attempt=%s/%s, retry in %.1fs: %s: %s',
                    fav_type,
                    attempt,
                    attempts,
                    delay,
                    exc.__class__.__name__,
                    exc,
                )
                await asyncio.sleep(delay)
                continue

            if not should_retry_jandan_api_status(response.status_code) or attempt >= attempts:
                return response

            delay = _API_RETRY_DELAYS_SECONDS[attempt - 1]
            log.warning(
                'Jandan API returned HTTP %s favType=%s attempt=%s/%s, retry in %.1fs',
                response.status_code,
                fav_type,
                attempt,
                attempts,
                delay,
            )
            await response.aclose()
            await asyncio.sleep(delay)

        if last_exc is not None:
            raise last_exc
        msg = 'Jandan API request failed without response'
        raise RuntimeError(msg)

    async def _fetch_image(self, url: str) -> httpx.Response:
        response = await self.client.get(url, headers=build_image_request_headers(url))
        response.raise_for_status()
        return response

    async def _fetch_from_sinaimg_shards(self, url: str) -> httpx.Response | None:
        for shard_url in build_sinaimg_shard_candidates(url):
            try:
                response = await self._fetch_image(shard_url)
            except (httpx.HTTPStatusError, httpx.TransportError):
                continue
            log.info('Recovered forbidden Jandan image from shard: %s', shard_url)
            return response
        return None

    async def _fetch_candidate(self, url: str) -> httpx.Response:
        try:
            return await self._fetch_image(url)
        except httpx.HTTPStatusError as exc:
            if exc.response.status_code != _SINAIMG_FORBIDDEN_STATUS:
                raise
            shard_response = await self._fetch_from_sinaimg_shards(url)
            if shard_response is None:
                raise
            return shard_response

    async def _download_image(self, image: JandanImage) -> Path:
        dst_path = self._build_output_path(image)
        await asyncio.to_thread(dst_path.parent.mkdir, parents=True, exist_ok=True)
        if await asyncio.to_thread(dst_path.exists):
            return dst_path

        candidates = build_image_download_candidates(image.content_url)
        placeholder_reason: str | None = None
        last_status_error: httpx.HTTPStatusError | None = None

        for idx, candidate_url in enumerate(candidates):
            is_last_candidate = idx == len(candidates) - 1
            try:
                response = await self._fetch_candidate(candidate_url)
            except httpx.HTTPStatusError as exc:
                last_status_error = exc
                if not is_last_candidate:
                    continue
                raise

            is_placeholder, reason = detect_deleted_placeholder(response)
            if is_placeholder:
                placeholder_reason = reason or f'placeholder response: {response.url}'
                if not is_last_candidate:
                    continue
                msg = placeholder_reason
                raise UnavailableImageError(msg)

            await asyncio.to_thread(dst_path.write_bytes, response.content)
            return dst_path

        if placeholder_reason:
            raise UnavailableImageError(placeholder_reason)
        if last_status_error is not None:
            raise last_status_error
        msg = f'Failed to download image from all candidates: {image.content_url}'
        raise RuntimeError(msg)

    async def _process_image(self, image: JandanImage) -> Path:
        dst_path = await self._download_image(image)
        await self._mark_downloaded(image, dst_path)
        return dst_path

    async def _download_pending_images(self, images: list[JandanImage]) -> int:
        downloaded_count = 0
        total_count = len(images)
        for current_index, image in enumerate(images, start=1):
            log.info(
                'Jandan downloading favType=%s progress=%s/%s pic=%s index=%s',
                image.fav_type,
                current_index,
                total_count,
                image.pic_id,
                image.content_index,
            )
            try:
                dst_path = await self._process_image(image)
            except UnavailableImageError as exc:
                reason = str(exc).strip() or 'confirmed unavailable'
                await self._mark_unavailable(image, reason)
                log.info(
                    'Marked unavailable Jandan item favType=%s progress=%s/%s pic=%s index=%s: %s',
                    image.fav_type,
                    current_index,
                    total_count,
                    image.pic_id,
                    image.content_index,
                    reason,
                )
                continue
            except httpx.HTTPStatusError as exc:
                status_code = exc.response.status_code
                request_host = exc.request.url.host if exc.request and exc.request.url else None
                if should_mark_unavailable_http(status_code=status_code, url_host=request_host):
                    reason = f'http {status_code}'
                    if status_code == _SINAIMG_FORBIDDEN_STATUS and request_host:
                        reason = f'{reason} on {request_host}'
                    await self._mark_unavailable(image, reason)
                    log.info(
                        'Marked unavailable Jandan item favType=%s progress=%s/%s pic=%s index=%s: %s',
                        image.fav_type,
                        current_index,
                        total_count,
                        image.pic_id,
                        image.content_index,
                        reason,
                    )
                    continue
                await self._mark_failed(image, f'http {status_code}')
                log.warning(
                    'Failed to process Jandan item favType=%s progress=%s/%s pic=%s index=%s: http %s',
                    image.fav_type,
                    current_index,
                    total_count,
                    image.pic_id,
                    image.content_index,
                    status_code,
                )
                continue
            except Exception as exc:  # noqa: BLE001
                await self._mark_failed(image, f'{exc.__class__.__name__}: {exc}')
                log.warning(
                    'Failed to process Jandan item favType=%s progress=%s/%s pic=%s index=%s: %s',
                    image.fav_type,
                    current_index,
                    total_count,
                    image.pic_id,
                    image.content_index,
                    exc,
                )
                continue
            downloaded_count += 1
            log.notice(
                'Jandan downloaded favType=%s progress=%s/%s file=%s',
                image.fav_type,
                current_index,
                total_count,
                dst_path.name,
            )
        return downloaded_count

    @staticmethod
    def _next_cursor_for_page(*, fav_type: int, pics: list[dict[str, Any]], seen_cursors: set[str]) -> str | None:
        last_date = extract_last_pic_date(pics)
        if not last_date:
            return None
        next_cursor = zulu_to_offset(last_date)
        if next_cursor in seen_cursors:
            log.warning('Jandan favType=%s repeated cursor %s, stop pagination', fav_type, next_cursor)
            return None
        seen_cursors.add(next_cursor)
        return next_cursor

    def _append_page_images(
        self,
        images: list[JandanImage],
        *,
        collected_images: list[JandanImage],
        collected_keys: set[tuple[int, int, int]],
    ) -> None:
        for image in images:
            key = self._image_key(image)
            if key in collected_keys:
                continue
            collected_keys.add(key)
            collected_images.append(image)

    async def _crawl_fav_type(self, *, fav_type: int) -> tuple[int, int, int, int, int]:
        fav_start_date: str | None = None
        page_count = 0
        seen_cursors: set[str] = set()
        page_index = 0
        api_total_images = 0
        full_hit_pages = 0
        collected_images: list[JandanImage] = []
        collected_keys: set[tuple[int, int, int]] = set()

        log.info(
            'Jandan stop rule favType=%s stop_after_full_hit_pages=%s',
            fav_type,
            _FULL_HIT_STOP_PAGES,
        )

        while True:
            page_index += 1
            requested_cursor = fav_start_date or '(first-page)'
            pics = await self._fetch_fav_page(fav_type=fav_type, fav_start_date=fav_start_date)
            if not pics:
                log.info('Jandan API reached end favType=%s page=%s', fav_type, page_index)
                break

            page_count = page_index
            images = extract_pic_images(fav_type=fav_type, pics=pics)
            if not images:
                break
            api_total_images += len(images)
            log.info(
                'Jandan API fetched favType=%s page=%s cursor=%s page_images=%s total_images=%s',
                fav_type,
                page_index,
                requested_cursor,
                len(images),
                api_total_images,
            )

            page_fully_processed, processed_count, total_count = await self._is_page_fully_processed(images)
            if page_fully_processed:
                full_hit_pages += 1
                log.info(
                    'Jandan API full-hit favType=%s page=%s matched=%s/%s consecutive_full_hits=%s/%s',
                    fav_type,
                    page_index,
                    processed_count,
                    total_count,
                    full_hit_pages,
                    _FULL_HIT_STOP_PAGES,
                )
                if full_hit_pages >= _FULL_HIT_STOP_PAGES:
                    log.info('Jandan favType=%s reached full-hit stop condition', fav_type)
                    break
            else:
                full_hit_pages = 0
                self._append_page_images(
                    images,
                    collected_images=collected_images,
                    collected_keys=collected_keys,
                )

            next_cursor = self._next_cursor_for_page(fav_type=fav_type, pics=pics, seen_cursors=seen_cursors)
            if next_cursor is None:
                break
            fav_start_date = next_cursor

        if collected_images:
            await self._upsert_images(collected_images)

        pending_images = await self._pending_images_from_db(fav_type=fav_type)
        downloaded_count = await self._download_pending_images(pending_images)
        return downloaded_count, page_count, api_total_images, len(collected_images), len(pending_images)

    async def update(self) -> None:
        if self.cfg.user_id <= 0:
            log.warning('Jandan user_id is not configured, skip update')
            return

        await self._ensure_table()
        await asyncio.to_thread(self.cfg.path.mkdir, parents=True, exist_ok=True)
        total_downloaded = 0
        total_api_images = 0
        fav_type_stats: list[tuple[int, int, int, int, int]] = []

        for fav_type in self.cfg.fav_types:
            downloaded_count, page_count, api_total_count, collected_count, pending_count = await self._crawl_fav_type(fav_type=fav_type)
            total_downloaded += downloaded_count
            total_api_images += api_total_count
            fav_type_stats.append((fav_type, page_count, api_total_count, collected_count, downloaded_count))
            log.info(
                'Jandan favType=%s scanned=%s pages api_total=%s collected=%s pending=%s downloaded=%s files',
                fav_type,
                page_count,
                api_total_count,
                collected_count,
                pending_count,
                downloaded_count,
            )

        log.info('Jandan API total images across fav_types=%s', total_api_images)

        if total_downloaded == 0:
            log.info('No new Jandan images')
            return

        await self._notify_summary(
            total_downloaded=total_downloaded,
            total_api_images=total_api_images,
            fav_type_stats=fav_type_stats,
        )
