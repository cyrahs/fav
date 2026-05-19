from __future__ import annotations

import asyncio
import hashlib
import json
import os
import secrets
import shutil
import tempfile
from contextlib import asynccontextmanager, suppress
from dataclasses import dataclass, field
from datetime import UTC, datetime
from email.utils import parsedate_to_datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any
from urllib.parse import urlsplit

import httpx

from src.core import config, logger
from src.tool import database
from src.tool.azurlane_l2d_sources import (
    DEFAULT_USER_AGENT,
    AzurLaneEnumeratedResource,
    AzurLaneModelCatalog,
    AzurLaneModelResourceEnumeration,
    AzurLaneSourceSnapshots,
    ModelEntry,
    SourceSchemaError,
    build_azurlane_l2d_health_report,
    build_azurlane_model_catalog,
    enumerate_azurlane_model_resources,
    fetch_source_snapshots,
)
from src.tool.filename import sanitize

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Iterable

log = logger.get('azurlane')
cfg = config.web.azurlane

_SECOND_FAILURE_COUNT = 2
_API_REQUEST_INTERVAL_SECONDS = 0.5
_CDN_REQUEST_INTERVAL_SECONDS = 0.2
_CDN_CONCURRENCY = 3
_ASSET_PROCESS_CONCURRENCY = 3
_MAX_RETRIES = 3
_LIMITED_RETRY_ATTEMPTS = 2
_RETRY_BASE_DELAY_SECONDS = 0.75
_RETRY_AFTER_MAX_DELAY_SECONDS = 300.0
_CIRCUIT_LIMITED_FAILURES = 5
_CIRCUIT_PAUSE_SECONDS = 30.0
_DOWNLOAD_CHUNK_SIZE = 1024 * 128
_BODY_PREFIX_LIMIT = 4096
_HTTP_CLIENT_ERROR_MIN = 400
_HTTP_SERVER_ERROR_MIN = 500
_HTTP_NOT_FOUND = 404
_HTTP_FORBIDDEN = 403
_HTTP_NOT_ACCEPTABLE = 406
_HTTP_REQUEST_TIMEOUT = 408
_HTTP_TOO_MANY_REQUESTS = 429
_HTTP_SERVICE_UNAVAILABLE = 503
_ASSET_RETRY_COOLDOWN_DAYS = (1, 3, 7)
_JSON_ASSET_KINDS = {
    'live2d.model3',
    'live2d.physics',
    'live2d.pose',
    'live2d.display-info',
    'live2d.expression',
    'live2d.motion',
}
_MANIFEST_SCHEMA_VERSION = 1
_ASSET_AVAILABLE_STATUS = 'downloaded'
_ASSET_KIND_ORDER = {
    kind: index
    for index, kind in enumerate(
        (
            'live2d.model3',
            'live2d.moc3',
            'live2d.texture',
            'live2d.physics',
            'live2d.pose',
            'live2d.display-info',
            'live2d.expression',
            'live2d.motion',
            'live2d.audio',
            'live2d.text',
            'spine.skel',
            'spine.atlas',
            'spine.texture',
        ),
    )
}

_CREATE_SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS azurlane_characters (
    character_key TEXT PRIMARY KEY,
    source_id BIGINT,
    name_zh TEXT NOT NULL DEFAULT '',
    name_en TEXT NOT NULL DEFAULT '',
    manifest_path TEXT NOT NULL DEFAULT '',
    active BOOLEAN NOT NULL DEFAULT TRUE,
    source_metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    fetched_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS azurlane_models (
    model_id TEXT PRIMARY KEY,
    model_type TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT '',
    character_key TEXT NOT NULL REFERENCES azurlane_characters(character_key) ON DELETE CASCADE,
    costume_key TEXT NOT NULL DEFAULT '',
    costume_id BIGINT,
    costume_name_zh TEXT NOT NULL DEFAULT '',
    costume_name_en TEXT NOT NULL DEFAULT '',
    primary_url TEXT NOT NULL DEFAULT '',
    fallback_url TEXT NOT NULL DEFAULT '',
    display_info_url TEXT NOT NULL DEFAULT '',
    availability_state TEXT NOT NULL DEFAULT 'unchecked',
    active BOOLEAN NOT NULL DEFAULT TRUE,
    source_metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    fetched_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS azurlane_assets (
    url TEXT PRIMARY KEY,
    normalized_url TEXT NOT NULL DEFAULT '',
    downloaded_url TEXT NOT NULL DEFAULT '',
    fallback_url TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL DEFAULT '',
    sha256 TEXT NOT NULL DEFAULT '',
    size BIGINT NOT NULL DEFAULT 0,
    content_type TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending',
    failed_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    last_attempt_at TIMESTAMPTZ,
    next_retry_at TIMESTAMPTZ,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS azurlane_blobs (
    sha256 TEXT NOT NULL,
    size BIGINT NOT NULL,
    content_type TEXT NOT NULL DEFAULT '',
    blob_path TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_verified_at TIMESTAMPTZ,
    PRIMARY KEY (sha256, size)
);

CREATE TABLE IF NOT EXISTS azurlane_model_assets (
    model_id TEXT NOT NULL REFERENCES azurlane_models(model_id) ON DELETE CASCADE,
    url TEXT NOT NULL REFERENCES azurlane_assets(url) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    context_hash TEXT NOT NULL,
    local_path TEXT NOT NULL DEFAULT '',
    original_filename TEXT NOT NULL DEFAULT '',
    fallback_url TEXT NOT NULL DEFAULT '',
    context_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (model_id, url, kind, context_hash)
);

CREATE INDEX IF NOT EXISTS azurlane_models_character_key_idx ON azurlane_models (character_key);
CREATE INDEX IF NOT EXISTS azurlane_assets_sha256_size_idx ON azurlane_assets (sha256, size);
CREATE INDEX IF NOT EXISTS azurlane_assets_status_retry_idx ON azurlane_assets (status, next_retry_at);
CREATE INDEX IF NOT EXISTS azurlane_model_assets_model_id_idx ON azurlane_model_assets (model_id);
ALTER TABLE azurlane_assets ADD COLUMN IF NOT EXISTS downloaded_url TEXT NOT NULL DEFAULT '';
ALTER TABLE azurlane_assets ADD COLUMN IF NOT EXISTS fallback_url TEXT NOT NULL DEFAULT '';
ALTER TABLE azurlane_assets ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT '';
ALTER TABLE azurlane_assets ADD COLUMN IF NOT EXISTS last_attempt_at TIMESTAMPTZ;
ALTER TABLE azurlane_assets ADD COLUMN IF NOT EXISTS next_retry_at TIMESTAMPTZ;
ALTER TABLE azurlane_model_assets ADD COLUMN IF NOT EXISTS fallback_url TEXT NOT NULL DEFAULT '';
"""


class InvalidAssetResponseError(RuntimeError):
    pass


class AssetProcessingError(RuntimeError):
    pass


class CrawlRunError(RuntimeError):
    pass


@dataclass(slots=True)
class AzurLaneAsset:
    url: str
    kind: str
    fallback_url: str = ''
    local_path: str = ''
    original_filename: str = ''
    content_type: str = ''
    size: int = 0
    sha256: str = ''
    status: str = 'pending'
    error: str = ''
    downloaded_url: str = ''
    contexts: list[dict[str, Any]] = field(default_factory=list)


@dataclass(frozen=True, slots=True)
class BlobRef:
    sha256: str
    size: int
    content_type: str
    path: Path
    downloaded_url: str = ''


@dataclass(frozen=True, slots=True)
class TempBlob:
    path: Path
    sha256: str
    size: int
    content_type: str
    downloaded_url: str


class _RateLimiter:
    def __init__(self, min_interval_seconds: float) -> None:
        self._min_interval_seconds = min_interval_seconds
        self._lock = asyncio.Lock()
        self._last_start = 0.0

    async def wait(self) -> None:
        if self._min_interval_seconds <= 0:
            return
        async with self._lock:
            loop = asyncio.get_running_loop()
            now = loop.time()
            delay = self._min_interval_seconds - (now - self._last_start)
            if delay > 0:
                await asyncio.sleep(delay)
            self._last_start = loop.time()


class _CircuitBreaker:
    def __init__(self) -> None:
        self._limited_failures = 0

    async def record_limited_failure(self) -> None:
        self._limited_failures += 1
        if self._limited_failures < _CIRCUIT_LIMITED_FAILURES:
            return
        log.warning('Azur Lane CDN returned repeated throttling responses; pausing this run for %.0fs', _CIRCUIT_PAUSE_SECONDS)
        await asyncio.sleep(_CIRCUIT_PAUSE_SECONDS)
        self._limited_failures = 0

    def record_success(self) -> None:
        self._limited_failures = 0


def _json_dumps(data: Any) -> str:
    return json.dumps(data, ensure_ascii=False, sort_keys=True, separators=(',', ':'), default=str)


def _pretty_json(data: Any) -> str:
    return json.dumps(data, ensure_ascii=False, indent=2, sort_keys=True, default=str) + '\n'


def _write_json(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f'.{path.name}.tmp-{os.getpid()}-{secrets.token_hex(4)}')
    try:
        tmp.write_text(_pretty_json(data), encoding='utf-8')
        tmp.replace(path)
    finally:
        with suppress(FileNotFoundError):
            tmp.unlink()


def _asset_key(url: str, kind: str) -> tuple[str, str]:
    return url, kind


def _context_hash(context: dict[str, Any]) -> str:
    return hashlib.sha256(_json_dumps(context).encode('utf-8')).hexdigest()


def _asset_headers() -> dict[str, str]:
    return {
        'Accept': '*/*',
        'Referer': 'https://l2d.su/',
        'User-Agent': DEFAULT_USER_AGENT,
    }


def _retry_after_seconds(response: httpx.Response | None) -> float | None:
    if response is None:
        return None
    value = response.headers.get('Retry-After')
    if not value:
        return None
    if value.strip().isdigit():
        return float(value.strip())
    try:
        retry_at = parsedate_to_datetime(value)
    except (TypeError, ValueError):
        return None
    if retry_at.tzinfo is None:
        retry_at = retry_at.replace(tzinfo=UTC)
    return max(0.0, (retry_at - datetime.now(UTC)).total_seconds())


def _retry_delay_seconds(attempt: int, response: httpx.Response | None = None) -> float:
    retry_after = _retry_after_seconds(response)
    if retry_after is not None:
        return min(retry_after, _RETRY_AFTER_MAX_DELAY_SECONDS)
    jitter = secrets.randbelow(250) / 1000
    return (_RETRY_BASE_DELAY_SECONDS * (2 ** max(0, attempt - 1))) + jitter


def _max_attempts_for_status(status_code: int) -> int:
    if status_code == _HTTP_NOT_FOUND:
        return 1
    if status_code in {_HTTP_FORBIDDEN, _HTTP_NOT_ACCEPTABLE}:
        return _LIMITED_RETRY_ATTEMPTS
    if status_code in {_HTTP_REQUEST_TIMEOUT, _HTTP_TOO_MANY_REQUESTS} or status_code >= _HTTP_SERVER_ERROR_MIN:
        return _MAX_RETRIES
    return 1


def _retry_cooldown_days(failed_count: int) -> int:
    if failed_count <= 1:
        return _ASSET_RETRY_COOLDOWN_DAYS[0]
    if failed_count == _SECOND_FAILURE_COUNT:
        return _ASSET_RETRY_COOLDOWN_DAYS[1]
    return _ASSET_RETRY_COOLDOWN_DAYS[2]


def _is_cdn_rejection_body(content_type: str, body_prefix: bytes) -> bool:
    lowered_type = content_type.lower()
    stripped = body_prefix.lstrip().lower()
    if 'text/html' in lowered_type:
        return True
    if stripped.startswith((b'<!doctype html', b'<html')):
        return True
    rejection_signatures = (b'accessdenied', b'access denied', b'forbidden', b'<error>', b'error code')
    return any(signature in stripped for signature in rejection_signatures)


def _validate_asset_response(*, kind: str, content_type: str, body_prefix: bytes, size: int) -> str:
    if size <= 0:
        return 'empty response body'
    if _is_cdn_rejection_body(content_type, body_prefix):
        return 'cdn rejection response'

    stripped = body_prefix.lstrip()
    if kind in _JSON_ASSET_KINDS and stripped and not stripped.startswith(b'{'):
        return 'expected JSON object response'
    if stripped.startswith(b'{'):
        try:
            parsed = json.loads(stripped.decode('utf-8', errors='ignore'))
        except json.JSONDecodeError:
            return ''
        if isinstance(parsed, dict) and ({'code', 'msg', 'message', 'error'} & set(parsed)):
            return 'json error response'
    return ''


def _http_status_error(response: httpx.Response) -> httpx.HTTPStatusError:
    message = f'HTTP {response.status_code}'
    return httpx.HTTPStatusError(message, request=response.request, response=response)


def _raise_invalid_asset_response(reason: str) -> None:
    raise InvalidAssetResponseError(reason)


def _file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open('rb') as file:
        for chunk in iter(lambda: file.read(_DOWNLOAD_CHUNK_SIZE), b''):
            digest.update(chunk)
    return digest.hexdigest()


def _verify_file(path: Path, *, sha256: str, size: int) -> bool:
    try:
        if path.stat().st_size != size:
            return False
        return _file_sha256(path) == sha256
    except OSError:
        return False


def _materialize_blob(*, blob_path: Path, destination: Path, sha256: str, size: int) -> None:
    if destination.exists() and _verify_file(destination, sha256=sha256, size=size):
        return

    destination.parent.mkdir(parents=True, exist_ok=True)
    tmp = destination.with_name(f'.{destination.name}.tmp-{os.getpid()}-{secrets.token_hex(4)}')
    try:
        try:
            os.link(blob_path, tmp)
        except OSError:
            shutil.copy2(blob_path, tmp)
        if not _verify_file(tmp, sha256=sha256, size=size):
            msg = f'Blob materialization verification failed: {destination}'
            raise InvalidAssetResponseError(msg)
        tmp.replace(destination)
    finally:
        with suppress(FileNotFoundError):
            tmp.unlink()


def _character_root(base_dir: Path, entry: ModelEntry) -> Path:
    name = entry.character.name_en or entry.character.name_zh or entry.character.key
    safe_name = sanitize(name, max_bytes=120) or entry.character.key
    return base_dir / f'{entry.character.key} - {safe_name}'


def _model3_seed_asset(entry: ModelEntry) -> AzurLaneAsset:
    filename = Path(urlsplit(entry.resources.primary_url).path).name or f'{entry.costume.key}.model3.json'
    model_key = sanitize(entry.costume.key, max_bytes=120) or 'unknown'
    return AzurLaneAsset(
        url=entry.resources.primary_url,
        kind='live2d.model3',
        fallback_url=entry.resources.fallback_url,
        local_path=(Path('assets/live2d') / model_key / filename).as_posix(),
        original_filename=filename,
        contexts=[
            {
                'model_id': entry.id,
                'model_type': entry.type,
                'character_key': entry.character.key,
                'costume_key': entry.costume.key,
                'catalog_source': entry.source,
                'source_model_url': entry.resources.primary_url,
                'fallback_model_url': entry.resources.fallback_url,
                'live2d_field': 'model3',
            },
        ],
    )


def _asset_from_resource(resource: AzurLaneEnumeratedResource) -> AzurLaneAsset:
    return AzurLaneAsset(
        url=resource.source_url,
        kind=resource.kind,
        fallback_url=resource.fallback_url,
        local_path=resource.local_path,
        original_filename=resource.original_filename,
        contexts=list(resource.contexts),
    )


def _assets_from_enumeration(enumeration: AzurLaneModelResourceEnumeration) -> dict[tuple[str, str], AzurLaneAsset]:
    assets: dict[tuple[str, str], AzurLaneAsset] = {}
    for resource in enumeration.assets:
        asset = _asset_from_resource(resource)
        key = _asset_key(asset.url, asset.kind)
        existing = assets.get(key)
        if existing is None:
            assets[key] = asset
            continue
        existing.contexts.extend(asset.contexts)
        if not existing.fallback_url:
            existing.fallback_url = asset.fallback_url
        if not existing.original_filename:
            existing.original_filename = asset.original_filename
        if not existing.local_path:
            existing.local_path = asset.local_path
    return assets


def _primary_source_model_count(snapshots: AzurLaneSourceSnapshots) -> int:
    return sum(len(character.live2d) + len(character.spine) for character in snapshots.l2d_su.characters)


def _source_snapshot_errors(snapshots: AzurLaneSourceSnapshots) -> tuple[Any, ...]:
    return (*snapshots.l2d_su.errors, *snapshots.nagami.errors)


def _source_snapshots_complete(snapshots: AzurLaneSourceSnapshots) -> bool:
    return not _source_snapshot_errors(snapshots) and _primary_source_model_count(snapshots) > 0


def _row_text(row: dict[str, Any], key: str) -> str:
    value = row.get(key)
    if value is None:
        return ''
    return str(value).strip()


def _row_int(row: dict[str, Any], key: str) -> int | None:
    value = row.get(key)
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    return None


def _timestamp_value(value: Any) -> str | None:
    if value is None:
        return None
    if isinstance(value, datetime):
        return value.isoformat()
    text = str(value).strip()
    return text or None


def _json_object(value: Any) -> dict[str, Any]:
    if isinstance(value, dict):
        return value
    if not isinstance(value, str) or not value.strip():
        return {}
    try:
        parsed = json.loads(value)
    except json.JSONDecodeError:
        return {}
    return parsed if isinstance(parsed, dict) else {}


def _character_manifest_root(base_dir: Path, row: dict[str, Any]) -> Path:
    manifest_path = _row_text(row, 'manifest_path')
    if manifest_path:
        return Path(manifest_path).parent

    character_key = _row_text(row, 'character_key')
    name = _row_text(row, 'name_en') or _row_text(row, 'name_zh') or character_key
    safe_name = sanitize(name, max_bytes=120) or character_key
    return base_dir / f'{character_key} - {safe_name}'


def _status_counts(items: Iterable[str]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for item in items:
        counts[item] = counts.get(item, 0) + 1
    return {key: counts[key] for key in sorted(counts)}


def _asset_available(asset: dict[str, Any]) -> bool:
    return (
        asset['status'] == _ASSET_AVAILABLE_STATUS
        and bool(asset['local_path'])
        and bool(asset['sha256'])
        and int(asset['size']) > 0
    )


def _archive_state(assets: list[dict[str, Any]]) -> str:
    if not assets:
        return 'no-assets'
    if all(_asset_available(asset) for asset in assets):
        return 'complete'
    if any(_asset_available(asset) for asset in assets):
        return 'partial'
    return 'unavailable'


def _asset_payload_from_row(row: dict[str, Any]) -> dict[str, Any]:
    url = _row_text(row, 'url')
    fallback_url = _row_text(row, 'relation_fallback_url') or _row_text(row, 'asset_fallback_url')
    status = _row_text(row, 'status') or 'pending'
    return {
        'url': url,
        'normalized_url': _row_text(row, 'normalized_url') or url,
        'downloaded_url': _row_text(row, 'downloaded_url'),
        'fallback_url': fallback_url,
        'source_urls': {
            'primary': url,
            'fallback': fallback_url,
            'downloaded': _row_text(row, 'downloaded_url'),
        },
        'kind': _row_text(row, 'kind'),
        'local_path': _row_text(row, 'local_path'),
        'original_filename': _row_text(row, 'original_filename'),
        'sha256': _row_text(row, 'sha256'),
        'size': _row_int(row, 'size') or 0,
        'content_type': _row_text(row, 'content_type'),
        'status': status,
        'available': status == _ASSET_AVAILABLE_STATUS,
        'failed_count': _row_int(row, 'failed_count') or 0,
        'error': _row_text(row, 'last_error'),
        'last_attempt_at': _timestamp_value(row.get('last_attempt_at')),
        'next_retry_at': _timestamp_value(row.get('next_retry_at')),
        'last_seen_at': _timestamp_value(row.get('last_seen_at')),
        'context_hashes': [],
        'contexts': [],
    }


def _merge_asset_row(assets_by_key: dict[tuple[str, str, str], dict[str, Any]], row: dict[str, Any]) -> None:
    key = (_row_text(row, 'url'), _row_text(row, 'kind'), _row_text(row, 'local_path'))
    asset = assets_by_key.setdefault(key, _asset_payload_from_row(row))
    context_hash = _row_text(row, 'context_hash')
    context = _json_object(row.get('context_json'))
    if context_hash and context_hash not in asset['context_hashes']:
        asset['context_hashes'].append(context_hash)
    if context and context not in asset['contexts']:
        asset['contexts'].append(context)


def _asset_sort_key(row: dict[str, Any]) -> tuple[int, str, str, str]:
    return (
        _ASSET_KIND_ORDER.get(_row_text(row, 'kind'), len(_ASSET_KIND_ORDER)),
        _row_text(row, 'local_path'),
        _row_text(row, 'url'),
        _row_text(row, 'context_hash'),
    )


def _model_assets(asset_rows: Iterable[dict[str, Any]]) -> list[dict[str, Any]]:
    assets_by_key: dict[tuple[str, str, str], dict[str, Any]] = {}
    for row in sorted(asset_rows, key=_asset_sort_key):
        _merge_asset_row(assets_by_key, row)

    assets = list(assets_by_key.values())
    for asset in assets:
        asset['context_hashes'] = sorted(asset['context_hashes'])
        asset['contexts'] = sorted(asset['contexts'], key=_json_dumps)
        asset['available'] = _asset_available(asset)
    return sorted(assets, key=_asset_sort_key)


def _model_payload(row: dict[str, Any], assets: list[dict[str, Any]]) -> dict[str, Any]:
    status_counts = _status_counts(asset['status'] for asset in assets)
    model_id = _row_text(row, 'model_id')
    primary_url = _row_text(row, 'primary_url')
    fallback_url = _row_text(row, 'fallback_url')
    display_info_url = _row_text(row, 'display_info_url')
    return {
        'model_id': model_id,
        'type': _row_text(row, 'model_type'),
        'source': _row_text(row, 'source'),
        'character_key': _row_text(row, 'character_key'),
        'costume': {
            'key': _row_text(row, 'costume_key'),
            'id': _row_int(row, 'costume_id'),
            'name_zh': _row_text(row, 'costume_name_zh'),
            'name_en': _row_text(row, 'costume_name_en'),
        },
        'source_urls': {
            'primary': primary_url,
            'fallback': fallback_url,
            'display_info': display_info_url,
        },
        'availability': {
            'source_state': _row_text(row, 'availability_state') or 'unchecked',
            'archive_state': _archive_state(assets),
            'asset_status_counts': status_counts,
            'available_asset_count': sum(1 for asset in assets if _asset_available(asset)),
            'asset_count': len(assets),
            'completed_at': _timestamp_value(row.get('completed_at')),
        },
        'source_metadata': _json_object(row.get('source_metadata')),
        'fetched_at': _timestamp_value(row.get('fetched_at')),
        'completed_at': _timestamp_value(row.get('completed_at')),
        'assets': assets,
        'asset_counts': _status_counts(asset['kind'] for asset in assets),
    }


def _manifest_asset_counts(models: Iterable[dict[str, Any]]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for model in models:
        for asset in model['assets']:
            kind = str(asset.get('kind') or '')
            if not kind:
                continue
            counts[kind] = counts.get(kind, 0) + 1
    return {key: counts[key] for key in sorted(counts)}


def _model_counts(models: Iterable[dict[str, Any]]) -> dict[str, int]:
    counts = {'live2d': 0, 'spine': 0, 'total': 0}
    for model in models:
        model_type = str(model.get('type') or '')
        if model_type in counts:
            counts[model_type] += 1
        counts['total'] += 1
    return counts


def _character_payload(row: dict[str, Any], models: list[dict[str, Any]], root: Path) -> dict[str, Any]:
    character_key = _row_text(row, 'character_key')
    source_id = _row_int(row, 'source_id')
    return {
        'schema_version': _MANIFEST_SCHEMA_VERSION,
        'source': 'azurlane',
        'character_key': character_key,
        'source_id': source_id,
        'name_zh': _row_text(row, 'name_zh'),
        'name_en': _row_text(row, 'name_en'),
        'directory_name': root.name,
        'manifest_path': (root / 'manifest.json').as_posix(),
        'source_metadata': _json_object(row.get('source_metadata')),
        'fetched_at': _timestamp_value(row.get('fetched_at')),
        'completed_at': _timestamp_value(row.get('completed_at')),
        'active': bool(row.get('active', True)),
        'model_counts': _model_counts(models),
        'asset_counts': _manifest_asset_counts(models),
        'models': models,
    }


def _character_summary_payload(manifest: dict[str, Any]) -> dict[str, Any]:
    models = manifest['models']
    return {
        'schema_version': manifest['schema_version'],
        'source': manifest['source'],
        'character_key': manifest['character_key'],
        'source_id': manifest['source_id'],
        'name_zh': manifest['name_zh'],
        'name_en': manifest['name_en'],
        'directory_name': manifest['directory_name'],
        'source_metadata': manifest['source_metadata'],
        'fetched_at': manifest['fetched_at'],
        'completed_at': manifest['completed_at'],
        'model_counts': manifest['model_counts'],
        'asset_counts': manifest['asset_counts'],
        'models': [
            {
                'model_id': model['model_id'],
                'type': model['type'],
                'source': model['source'],
                'costume': model['costume'],
                'source_urls': model['source_urls'],
                'availability': model['availability'],
            }
            for model in models
        ],
    }


class AzurLane:
    def __init__(  # noqa: PLR0913
        self,
        *,
        path: Path | None = None,
        client: httpx.AsyncClient | None = None,
        source_client: httpx.Client | None = None,
        source_timeout: float = 30.0,
        api_request_interval_seconds: float = _API_REQUEST_INTERVAL_SECONDS,
        cdn_request_interval_seconds: float = _CDN_REQUEST_INTERVAL_SECONDS,
        asset_process_concurrency: int = _ASSET_PROCESS_CONCURRENCY,
    ) -> None:
        self.path = Path(path or cfg.path)
        self._client = client
        self._source_client = source_client
        self._source_timeout = source_timeout
        self._api_limiter = _RateLimiter(api_request_interval_seconds)
        self._cdn_limiter = _RateLimiter(cdn_request_interval_seconds)
        self._cdn_semaphore = asyncio.Semaphore(_CDN_CONCURRENCY)
        self._asset_process_concurrency = asset_process_concurrency
        self._circuit_breaker = _CircuitBreaker()

    @asynccontextmanager
    async def _http_client(self) -> AsyncIterator[httpx.AsyncClient]:
        if self._client is not None:
            yield self._client
            return

        timeout = httpx.Timeout(60.0, connect=20.0)
        async with httpx.AsyncClient(follow_redirects=True, headers=_asset_headers(), timeout=timeout) as client:
            yield client

    async def _ensure_schema(self) -> None:
        await database.query_db_multi(_CREATE_SCHEMA_SQL)

    async def _fetch_source_snapshots(self) -> AzurLaneSourceSnapshots:
        await self._api_limiter.wait()
        if self._source_client is not None:
            return fetch_source_snapshots(timeout=self._source_timeout, client=self._source_client)
        return await asyncio.to_thread(fetch_source_snapshots, timeout=self._source_timeout)

    def _write_source_artifacts(self, *, snapshots: AzurLaneSourceSnapshots, catalog: AzurLaneModelCatalog) -> None:
        health_report = build_azurlane_l2d_health_report(snapshots=snapshots, catalog=catalog)
        source_root = self.path / '_source'
        _write_json(source_root / 'l2d-su-snapshot.json', snapshots.l2d_su.to_dict())
        _write_json(source_root / 'nagami-snapshot.json', snapshots.nagami.to_dict())
        _write_json(source_root / 'health-report.json', health_report.to_dict())

    async def _manifest_character_rows(self) -> list[dict[str, Any]]:
        return await database.query_db(
            """
            SELECT
                character_key,
                source_id,
                name_zh,
                name_en,
                manifest_path,
                active,
                source_metadata,
                fetched_at,
                completed_at
            FROM azurlane_characters
            WHERE active = TRUE
            ORDER BY character_key;
            """,
        )

    async def _manifest_model_rows(self) -> list[dict[str, Any]]:
        return await database.query_db(
            """
            SELECT
                model_id,
                model_type,
                source,
                character_key,
                costume_key,
                costume_id,
                costume_name_zh,
                costume_name_en,
                primary_url,
                fallback_url,
                display_info_url,
                availability_state,
                active,
                source_metadata,
                fetched_at,
                completed_at
            FROM azurlane_models
            WHERE active = TRUE
            ORDER BY character_key, model_type, costume_key, model_id;
            """,
        )

    async def _manifest_asset_rows(self, model_ids: list[str]) -> list[dict[str, Any]]:
        if not model_ids:
            return []
        return await database.query_db(
            """
            SELECT
                ma.model_id,
                ma.url,
                ma.kind,
                ma.context_hash,
                ma.local_path,
                ma.original_filename,
                ma.fallback_url AS relation_fallback_url,
                ma.context_json,
                a.normalized_url,
                a.downloaded_url,
                a.fallback_url AS asset_fallback_url,
                a.sha256,
                a.size,
                a.content_type,
                a.status,
                a.failed_count,
                a.last_error,
                a.last_attempt_at,
                a.next_retry_at,
                a.last_seen_at
            FROM azurlane_model_assets AS ma
            JOIN azurlane_assets AS a ON a.url = ma.url
            WHERE ma.model_id = ANY(?)
            ORDER BY ma.model_id, ma.kind, ma.local_path, ma.url, ma.context_hash;
            """,
            (model_ids,),
        )

    async def _write_backend_manifests(self) -> None:
        character_rows = await self._manifest_character_rows()
        if not character_rows:
            return

        model_rows = await self._manifest_model_rows()
        asset_rows = await self._manifest_asset_rows([_row_text(row, 'model_id') for row in model_rows])

        assets_by_model_id: dict[str, list[dict[str, Any]]] = {}
        for row in asset_rows:
            assets_by_model_id.setdefault(_row_text(row, 'model_id'), []).append(row)

        models_by_character_key: dict[str, list[dict[str, Any]]] = {}
        for row in model_rows:
            model_id = _row_text(row, 'model_id')
            assets = _model_assets(assets_by_model_id.get(model_id, []))
            models_by_character_key.setdefault(_row_text(row, 'character_key'), []).append(_model_payload(row, assets))

        for row in sorted(character_rows, key=lambda item: _row_text(item, 'character_key')):
            root = _character_manifest_root(self.path, row)
            models = sorted(
                models_by_character_key.get(_row_text(row, 'character_key'), []),
                key=lambda item: (item['type'], item['costume']['key'], item['model_id']),
            )
            manifest = _character_payload(row, models, root)
            _write_json(root / 'manifest.json', manifest)
            _write_json(root / 'character.json', _character_summary_payload(manifest))

    async def _upsert_catalog_state(self, catalog: AzurLaneModelCatalog) -> None:
        character_metadata: dict[str, dict[str, Any]] = {}
        character_rows: dict[str, tuple[int | None, str, str, str]] = {}
        statements: list[tuple[str, tuple[Any, ...]]] = [
            ('UPDATE azurlane_characters SET active = FALSE;', ()),
            ('UPDATE azurlane_models SET active = FALSE;', ()),
        ]

        for entry in catalog.entries:
            metadata = character_metadata.setdefault(entry.character.key, {'model_ids': [], 'sources': []})
            metadata['model_ids'].append(entry.id)
            metadata['sources'].append(entry.source)
            existing = character_rows.get(entry.character.key)
            if existing is None or existing[0] is None:
                character_rows[entry.character.key] = (
                    entry.character.id,
                    entry.character.name_zh,
                    entry.character.name_en,
                    (_character_root(self.path, entry) / 'manifest.json').as_posix(),
                )

        for character_key in sorted(character_rows):
            source_id, name_zh, name_en, manifest_path = character_rows[character_key]
            metadata = character_metadata[character_key]
            metadata['model_ids'] = sorted(set(metadata['model_ids']))
            metadata['sources'] = sorted(set(metadata['sources']))
            statements.append(
                (
                    """
                    INSERT INTO azurlane_characters (
                        character_key, source_id, name_zh, name_en, manifest_path, active,
                        source_metadata, first_seen_at, last_seen_at, fetched_at
                    )
                    VALUES (?, ?, ?, ?, ?, TRUE, ?::jsonb, NOW(), NOW(), NOW())
                    ON CONFLICT (character_key) DO UPDATE SET
                        source_id = COALESCE(excluded.source_id, azurlane_characters.source_id),
                        name_zh = COALESCE(NULLIF(excluded.name_zh, ''), azurlane_characters.name_zh),
                        name_en = COALESCE(NULLIF(excluded.name_en, ''), azurlane_characters.name_en),
                        manifest_path = excluded.manifest_path,
                        active = TRUE,
                        source_metadata = excluded.source_metadata,
                        last_seen_at = NOW(),
                        fetched_at = NOW();
                    """,
                    (character_key, source_id, name_zh, name_en, manifest_path, _json_dumps(metadata)),
                ),
            )

        for entry in catalog.entries:
            statements.append(  # noqa: PERF401
                (
                    """
                    INSERT INTO azurlane_models (
                        model_id, model_type, source, character_key, costume_key, costume_id,
                        costume_name_zh, costume_name_en, primary_url, fallback_url, display_info_url,
                        availability_state, active, source_metadata, first_seen_at, last_seen_at, fetched_at
                    )
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, TRUE, ?::jsonb, NOW(), NOW(), NOW())
                    ON CONFLICT (model_id) DO UPDATE SET
                        model_type = excluded.model_type,
                        source = excluded.source,
                        character_key = excluded.character_key,
                        costume_key = excluded.costume_key,
                        costume_id = excluded.costume_id,
                        costume_name_zh = excluded.costume_name_zh,
                        costume_name_en = excluded.costume_name_en,
                        primary_url = excluded.primary_url,
                        fallback_url = excluded.fallback_url,
                        display_info_url = excluded.display_info_url,
                        availability_state = excluded.availability_state,
                        active = TRUE,
                        source_metadata = excluded.source_metadata,
                        last_seen_at = NOW(),
                        fetched_at = NOW();
                    """,
                    (
                        entry.id,
                        entry.type,
                        entry.source,
                        entry.character.key,
                        entry.costume.key,
                        entry.costume.id,
                        entry.costume.name_zh,
                        entry.costume.name_en,
                        entry.resources.primary_url,
                        entry.resources.fallback_url,
                        entry.resources.display_info_url,
                        entry.availability.state,
                        _json_dumps(entry.to_dict()),
                    ),
                ),
            )

        await database.query_db_transaction(statements)

    async def _mark_model_fetch_started(self, entry: ModelEntry) -> None:
        await database.query_db(
            """
            UPDATE azurlane_models
            SET fetched_at = NOW(), completed_at = NULL, last_seen_at = NOW()
            WHERE model_id = ?;
            """,
            (entry.id,),
        )

    async def _mark_character_completed(self, entry: ModelEntry) -> None:
        await database.query_db(
            """
            UPDATE azurlane_characters
            SET completed_at = NOW(), last_seen_at = NOW()
            WHERE character_key = ?;
            """,
            (entry.character.key,),
        )

    async def _upsert_asset_seen(self, asset: AzurLaneAsset) -> None:
        await database.query_db(
            """
            INSERT INTO azurlane_assets (
                url, normalized_url, kind, fallback_url, status, first_seen_at, last_seen_at
            )
            VALUES (?, ?, ?, ?, 'pending', NOW(), NOW())
            ON CONFLICT (url) DO UPDATE SET
                normalized_url = excluded.normalized_url,
                kind = COALESCE(NULLIF(excluded.kind, ''), azurlane_assets.kind),
                fallback_url = COALESCE(NULLIF(excluded.fallback_url, ''), azurlane_assets.fallback_url),
                last_seen_at = NOW(),
                status = CASE
                    WHEN azurlane_assets.status = 'downloaded' THEN azurlane_assets.status
                    WHEN azurlane_assets.status = 'failed' AND azurlane_assets.next_retry_at > NOW() THEN azurlane_assets.status
                    ELSE excluded.status
                END,
                next_retry_at = CASE
                    WHEN azurlane_assets.status = 'failed' AND azurlane_assets.next_retry_at > NOW() THEN azurlane_assets.next_retry_at
                    ELSE NULL
                END;
            """,
            (asset.url, asset.url, asset.kind, asset.fallback_url),
        )

    async def _asset_retry_cooldown(self, asset: AzurLaneAsset) -> dict[str, Any] | None:
        rows = await database.query_db(
            """
            SELECT failed_count, last_error, next_retry_at
            FROM azurlane_assets
            WHERE url = ? AND status = 'failed' AND next_retry_at > NOW()
            LIMIT 1;
            """,
            (asset.url,),
        )
        return rows[0] if rows else None

    def _resolve_blob_path(self, blob_path: str) -> Path:
        path = Path(blob_path)
        if path.is_absolute():
            return path
        return self.path / path

    async def _completed_blob_for_url(self, url: str) -> BlobRef | None:
        rows = await database.query_db(
            """
            SELECT
                a.sha256,
                a.size,
                COALESCE(NULLIF(a.content_type, ''), b.content_type) AS content_type,
                a.downloaded_url,
                b.blob_path
            FROM azurlane_assets AS a
            JOIN azurlane_blobs AS b ON b.sha256 = a.sha256 AND b.size = a.size
            WHERE a.url = ? AND a.status = 'downloaded' AND a.sha256 <> ''
            LIMIT 1;
            """,
            (url,),
        )
        if not rows:
            return None
        row = rows[0]
        sha256 = str(row.get('sha256') or '')
        size = int(row.get('size') or 0)
        blob_path = self._resolve_blob_path(str(row.get('blob_path') or ''))
        if not sha256 or size <= 0 or not _verify_file(blob_path, sha256=sha256, size=size):
            await database.query_db(
                "UPDATE azurlane_assets SET status = 'missing', last_error = 'blob missing or failed verification' WHERE url = ?;",
                (url,),
            )
            return None
        return BlobRef(
            sha256=sha256,
            size=size,
            content_type=str(row.get('content_type') or ''),
            path=blob_path,
            downloaded_url=str(row.get('downloaded_url') or ''),
        )

    async def _mark_asset_downloaded(self, asset: AzurLaneAsset, blob: BlobRef, *, downloaded_url: str) -> None:
        await database.query_db(
            """
            UPDATE azurlane_assets
            SET sha256 = ?, size = ?, content_type = ?, downloaded_url = ?, status = 'downloaded',
                failed_count = 0, last_error = '', last_attempt_at = NOW(), next_retry_at = NULL, last_seen_at = NOW()
            WHERE url = ?;
            """,
            (blob.sha256, blob.size, blob.content_type, downloaded_url, asset.url),
        )

    async def _mark_asset_failed(self, asset: AzurLaneAsset, reason: str) -> None:
        first_retry_days = _retry_cooldown_days(1)
        second_retry_days = _retry_cooldown_days(2)
        later_retry_days = _retry_cooldown_days(3)
        await database.query_db(
            """
            INSERT INTO azurlane_assets (
                url, normalized_url, kind, fallback_url, status, failed_count, last_error,
                last_attempt_at, next_retry_at, first_seen_at, last_seen_at
            )
            VALUES (?, ?, ?, ?, 'failed', 1, ?, NOW(), NOW() + (?::integer * INTERVAL '1 day'), NOW(), NOW())
            ON CONFLICT (url) DO UPDATE SET
                kind = COALESCE(NULLIF(excluded.kind, ''), azurlane_assets.kind),
                fallback_url = COALESCE(NULLIF(excluded.fallback_url, ''), azurlane_assets.fallback_url),
                status = 'failed',
                failed_count = azurlane_assets.failed_count + 1,
                last_error = excluded.last_error,
                last_attempt_at = NOW(),
                next_retry_at = NOW() + (
                    CASE
                        WHEN azurlane_assets.failed_count + 1 <= 1 THEN ?::integer * INTERVAL '1 day'
                        WHEN azurlane_assets.failed_count + 1 = 2 THEN ?::integer * INTERVAL '1 day'
                        ELSE ?::integer * INTERVAL '1 day'
                    END
                ),
                last_seen_at = NOW();
            """,
            (
                asset.url,
                asset.url,
                asset.kind,
                asset.fallback_url,
                reason[:500],
                first_retry_days,
                first_retry_days,
                second_retry_days,
                later_retry_days,
            ),
        )

    def _blob_relative_path(self, sha256: str) -> Path:
        return Path('_blobs/sha256') / sha256[:2] / sha256

    async def _register_temp_blob(self, temp_blob: TempBlob) -> BlobRef:
        rows = await database.query_db(
            """
            SELECT blob_path, content_type
            FROM azurlane_blobs
            WHERE sha256 = ? AND size = ?
            LIMIT 1;
            """,
            (temp_blob.sha256, temp_blob.size),
        )
        for row in rows:
            existing = self._resolve_blob_path(str(row.get('blob_path') or ''))
            if _verify_file(existing, sha256=temp_blob.sha256, size=temp_blob.size):
                temp_blob.path.unlink(missing_ok=True)
                await database.query_db(
                    'UPDATE azurlane_blobs SET last_verified_at = NOW() WHERE sha256 = ? AND size = ?;',
                    (temp_blob.sha256, temp_blob.size),
                )
                return BlobRef(
                    sha256=temp_blob.sha256,
                    size=temp_blob.size,
                    content_type=temp_blob.content_type or str(row.get('content_type') or ''),
                    path=existing,
                    downloaded_url=temp_blob.downloaded_url,
                )

        relative_path = self._blob_relative_path(temp_blob.sha256)
        blob_path = self.path / relative_path
        blob_path.parent.mkdir(parents=True, exist_ok=True)
        if blob_path.exists() and _verify_file(blob_path, sha256=temp_blob.sha256, size=temp_blob.size):
            temp_blob.path.unlink(missing_ok=True)
        else:
            temp_blob.path.replace(blob_path)

        await database.query_db(
            """
            INSERT INTO azurlane_blobs (sha256, size, content_type, blob_path, created_at, last_verified_at)
            VALUES (?, ?, ?, ?, NOW(), NOW())
            ON CONFLICT (sha256, size) DO UPDATE SET
                content_type = COALESCE(NULLIF(excluded.content_type, ''), azurlane_blobs.content_type),
                blob_path = excluded.blob_path,
                last_verified_at = NOW();
            """,
            (temp_blob.sha256, temp_blob.size, temp_blob.content_type, relative_path.as_posix()),
        )
        return BlobRef(
            sha256=temp_blob.sha256,
            size=temp_blob.size,
            content_type=temp_blob.content_type,
            path=blob_path,
            downloaded_url=temp_blob.downloaded_url,
        )

    async def _download_candidate_to_temp(  # noqa: C901
        self,
        client: httpx.AsyncClient,
        asset: AzurLaneAsset,
        *,
        url: str,
    ) -> TempBlob:
        tmp_dir = self.path / '_tmp'
        tmp_dir.mkdir(parents=True, exist_ok=True)
        tmp_path: Path | None = None
        async with self._cdn_semaphore:
            await self._cdn_limiter.wait()
            async with client.stream('GET', url, headers=_asset_headers()) as stream_response:
                if stream_response.status_code >= _HTTP_CLIENT_ERROR_MIN:
                    if stream_response.status_code in {
                        _HTTP_FORBIDDEN,
                        _HTTP_NOT_ACCEPTABLE,
                        _HTTP_TOO_MANY_REQUESTS,
                        _HTTP_SERVICE_UNAVAILABLE,
                    }:
                        await self._circuit_breaker.record_limited_failure()
                    raise _http_status_error(stream_response)

                fd, raw_tmp_path = tempfile.mkstemp(prefix='.azurlane-download-', dir=tmp_dir)
                tmp_path = Path(raw_tmp_path)
                digest = hashlib.sha256()
                size = 0
                prefix = bytearray()
                try:
                    with os.fdopen(fd, 'wb') as file:
                        async for chunk in stream_response.aiter_bytes(_DOWNLOAD_CHUNK_SIZE):
                            if not chunk:
                                continue
                            size += len(chunk)
                            digest.update(chunk)
                            if len(prefix) < _BODY_PREFIX_LIMIT:
                                prefix.extend(chunk[: _BODY_PREFIX_LIMIT - len(prefix)])
                            file.write(chunk)

                    content_type = stream_response.headers.get('content-type', '')
                    sha256 = digest.hexdigest()
                    invalid_reason = _validate_asset_response(
                        kind=asset.kind,
                        content_type=content_type,
                        body_prefix=bytes(prefix),
                        size=size,
                    )
                    if invalid_reason:
                        if invalid_reason == 'cdn rejection response':
                            await self._circuit_breaker.record_limited_failure()
                        _raise_invalid_asset_response(invalid_reason)
                    if not _verify_file(tmp_path, sha256=sha256, size=size):
                        msg = 'temporary file failed sha256 verification'
                        _raise_invalid_asset_response(msg)
                    self._circuit_breaker.record_success()
                    return TempBlob(path=tmp_path, sha256=sha256, size=size, content_type=content_type, downloaded_url=url)
                except Exception:
                    if tmp_path is not None:
                        tmp_path.unlink(missing_ok=True)
                    raise

    async def _download_asset_to_temp(self, client: httpx.AsyncClient, asset: AzurLaneAsset) -> TempBlob:
        candidates = [asset.url]
        if asset.fallback_url and asset.fallback_url not in candidates:
            candidates.append(asset.fallback_url)

        last_exc: Exception | None = None
        for url in candidates:
            for attempt in range(1, _MAX_RETRIES + 1):
                response: httpx.Response | None = None
                try:
                    return await self._download_candidate_to_temp(client, asset, url=url)
                except httpx.HTTPStatusError as exc:
                    last_exc = exc
                    response = exc.response
                    if attempt >= _max_attempts_for_status(exc.response.status_code):
                        break
                except (httpx.RequestError, httpx.TimeoutException, InvalidAssetResponseError) as exc:
                    last_exc = exc

                if attempt < _MAX_RETRIES:
                    await asyncio.sleep(_retry_delay_seconds(attempt, response))

        if last_exc is not None:
            raise last_exc
        msg = f'Failed to download {asset.url}'
        raise RuntimeError(msg)

    async def _process_asset(self, *, client: httpx.AsyncClient, root: Path, asset: AzurLaneAsset) -> bool:
        await self._upsert_asset_seen(asset)
        cooldown = await self._asset_retry_cooldown(asset)
        if cooldown is not None:
            asset.status = 'failed'
            asset.error = str(cooldown.get('last_error') or 'retry cooldown active')
            log.info('Skipping Azur Lane asset during retry cooldown until %s: %s', cooldown.get('next_retry_at'), asset.url)
            return False

        try:
            blob = await self._completed_blob_for_url(asset.url)
            if blob is None:
                temp_blob = await self._download_asset_to_temp(client, asset)
                blob = await self._register_temp_blob(temp_blob)
                asset.status = 'downloaded'
                asset.downloaded_url = temp_blob.downloaded_url
            else:
                asset.status = 'reused'
                asset.downloaded_url = blob.downloaded_url or asset.url

            _materialize_blob(blob_path=blob.path, destination=root / asset.local_path, sha256=blob.sha256, size=blob.size)
            asset.sha256 = blob.sha256
            asset.size = blob.size
            asset.content_type = blob.content_type
            asset.error = ''
            await self._mark_asset_downloaded(asset, blob, downloaded_url=asset.downloaded_url or blob.downloaded_url or asset.url)
        except Exception as exc:  # noqa: BLE001
            error = str(exc) or exc.__class__.__name__
            asset.status = 'failed'
            asset.error = error
            log.warning('Failed to download Azur Lane asset %s: %s', asset.url, error)
            await self._mark_asset_failed(asset, error)
            return False
        else:
            return True

    async def _process_assets(
        self,
        *,
        client: httpx.AsyncClient,
        root: Path,
        assets: dict[tuple[str, str], AzurLaneAsset],
    ) -> list[AzurLaneAsset]:
        if not assets:
            return []

        assets_by_url: dict[str, list[AzurLaneAsset]] = {}
        for asset in assets.values():
            assets_by_url.setdefault(asset.url, []).append(asset)

        semaphore = asyncio.Semaphore(self._asset_process_concurrency)

        async def _process_url_group(group: list[AzurLaneAsset]) -> None:
            async with semaphore:
                for asset in group:
                    await self._process_asset(client=client, root=root, asset=asset)

        await asyncio.gather(*(_process_url_group(group) for group in assets_by_url.values()))
        return [asset for asset in assets.values() if asset.status == 'failed']

    async def _seed_asset_text(self, *, client: httpx.AsyncClient, asset: AzurLaneAsset) -> str:
        await self._upsert_asset_seen(asset)
        blob = await self._completed_blob_for_url(asset.url)
        if blob is None:
            temp_blob = await self._download_asset_to_temp(client, asset)
            blob = await self._register_temp_blob(temp_blob)
            asset.status = 'downloaded'
            asset.downloaded_url = temp_blob.downloaded_url
        else:
            asset.status = 'reused'
            asset.downloaded_url = blob.downloaded_url or asset.url

        asset.sha256 = blob.sha256
        asset.size = blob.size
        asset.content_type = blob.content_type
        asset.error = ''
        await self._mark_asset_downloaded(asset, blob, downloaded_url=asset.downloaded_url or blob.downloaded_url or asset.url)
        return blob.path.read_text(encoding='utf-8', errors='ignore')

    async def _live2d_enumeration(self, *, client: httpx.AsyncClient, entry: ModelEntry) -> AzurLaneModelResourceEnumeration:
        seed_asset = _model3_seed_asset(entry)
        try:
            model3_source = await self._seed_asset_text(client=client, asset=seed_asset)
            return enumerate_azurlane_model_resources(entry, model3_source=model3_source)
        except Exception as exc:
            error = str(exc) or exc.__class__.__name__
            seed_asset.status = 'failed'
            seed_asset.error = error
            await self._mark_asset_failed(seed_asset, error)
            raise

    async def _spine_enumeration(self, *, client: httpx.AsyncClient, root: Path, entry: ModelEntry) -> AzurLaneModelResourceEnumeration:
        seed_enumeration = enumerate_azurlane_model_resources(entry)
        seed_assets = _assets_from_enumeration(seed_enumeration)
        atlas_asset = next((asset for asset in seed_assets.values() if asset.kind == 'spine.atlas'), None)
        if atlas_asset is None:
            msg = f'Spine entry {entry.id} did not enumerate an atlas asset'
            raise SourceSchemaError(msg)
        if not await self._process_asset(client=client, root=root, asset=atlas_asset):
            return seed_enumeration
        atlas_source = (root / atlas_asset.local_path).read_text(encoding='utf-8', errors='ignore')
        return enumerate_azurlane_model_resources(entry, atlas_source=atlas_source)

    async def _model_enumeration(self, *, client: httpx.AsyncClient, root: Path, entry: ModelEntry) -> AzurLaneModelResourceEnumeration:
        if entry.type == 'live2d':
            return await self._live2d_enumeration(client=client, entry=entry)
        return await self._spine_enumeration(client=client, root=root, entry=entry)

    async def _replace_model_assets(
        self,
        *,
        model_id: str,
        assets: dict[tuple[str, str], AzurLaneAsset],
        completed: bool,
    ) -> None:
        statements: list[tuple[str, tuple[Any, ...]]] = [('DELETE FROM azurlane_model_assets WHERE model_id = ?;', (model_id,))]
        for asset in assets.values():
            contexts = asset.contexts or [{}]
            for context in contexts:
                statements.append(  # noqa: PERF401
                    (
                        """
                        INSERT INTO azurlane_model_assets (
                            model_id, url, kind, context_hash, local_path, original_filename,
                            fallback_url, context_json, created_at, updated_at
                        )
                        VALUES (?, ?, ?, ?, ?, ?, ?, ?::jsonb, NOW(), NOW())
                        ON CONFLICT (model_id, url, kind, context_hash) DO UPDATE SET
                            local_path = excluded.local_path,
                            original_filename = excluded.original_filename,
                            fallback_url = excluded.fallback_url,
                            context_json = excluded.context_json,
                            updated_at = NOW();
                        """,
                        (
                            model_id,
                            asset.url,
                            asset.kind,
                            _context_hash(context),
                            asset.local_path,
                            asset.original_filename,
                            asset.fallback_url,
                            _json_dumps(context),
                        ),
                    ),
                )
        if completed:
            statements.append(('UPDATE azurlane_models SET completed_at = NOW(), last_seen_at = NOW() WHERE model_id = ?;', (model_id,)))
        await database.query_db_transaction(statements)

    async def _archive_model(self, *, client: httpx.AsyncClient, entry: ModelEntry) -> None:
        root = _character_root(self.path, entry)
        await self._mark_model_fetch_started(entry)
        try:
            enumeration = await self._model_enumeration(client=client, root=root, entry=entry)
        except Exception as exc:
            msg = f'Failed to enumerate Azur Lane model {entry.id}: {exc}'
            raise AssetProcessingError(msg) from exc

        assets = _assets_from_enumeration(enumeration)
        failed_assets = await self._process_assets(client=client, root=root, assets=assets)
        await self._replace_model_assets(model_id=entry.id, assets=assets, completed=not failed_assets)
        if failed_assets:
            examples = ', '.join(asset.url for asset in failed_assets[:3])
            msg = f'{len(failed_assets)} Azur Lane assets failed'
            if examples:
                msg = f'{msg}: {examples}'
            raise AssetProcessingError(msg)
        await self._mark_character_completed(entry)

    async def download_models(
        self,
        entries: Iterable[ModelEntry],
        *,
        client: httpx.AsyncClient,
    ) -> list[str]:
        failed_model_ids: list[str] = []
        for entry in entries:
            try:
                await self._archive_model(client=client, entry=entry)
            except Exception:
                log.exception('Failed to crawl Azur Lane model_id=%s', entry.id)
                failed_model_ids.append(entry.id)
        return failed_model_ids

    async def update(self) -> None:
        async with database.advisory_lock('azurlane') as acquired:
            if not acquired:
                log.info('Azur Lane update skipped because another run holds the advisory lock')
                return

            await self._ensure_schema()
            self.path.mkdir(parents=True, exist_ok=True)
            snapshots = await self._fetch_source_snapshots()
            catalog = build_azurlane_model_catalog(snapshots)
            self._write_source_artifacts(snapshots=snapshots, catalog=catalog)
            if not catalog.entries:
                log.warning('Azur Lane source catalog returned no model entries')
                return
            if not _source_snapshots_complete(snapshots):
                errors = _source_snapshot_errors(snapshots)
                if errors:
                    examples = '; '.join(f'{error.url} {error.kind}: {error.message}' for error in errors[:3])
                    log.warning('Azur Lane source snapshots incomplete; preserving existing catalog state: %s', examples)
                else:
                    log.warning('Azur Lane primary source returned no model entries; preserving existing catalog state')
                return

            await self._upsert_catalog_state(catalog)
            log.info('Found %d Azur Lane model entries', len(catalog.entries))
            async with self._http_client() as client:
                failed_model_ids = await self.download_models(catalog.entries, client=client)
            await self._write_backend_manifests()
            if failed_model_ids:
                examples = ', '.join(failed_model_ids[:5])
                msg = f'{len(failed_model_ids)} Azur Lane models failed'
                if examples:
                    msg = f'{msg}: {examples}'
                raise CrawlRunError(msg)
