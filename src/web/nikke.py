from __future__ import annotations

import asyncio
import hashlib
import json
import os
import re
import secrets
import shutil
import tempfile
from contextlib import asynccontextmanager, suppress
from dataclasses import dataclass, field
from datetime import UTC, datetime
from email.utils import parsedate_to_datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any
from urllib.parse import unquote, urljoin, urlsplit

import httpx

from src.core import config, logger
from src.tool import database
from src.tool.filename import sanitize
from src.web import nikke_layer_metadata as layer_metadata
from src.web.nikke_runtime import RuntimeCaptureRequest, capture_gamekee_runtime_layers

log = logger.get('nikke')
cfg = config.web.nikke

if TYPE_CHECKING:
    from collections.abc import AsyncIterator

LAYER_CAPTURE_MATCH_METHOD = layer_metadata.LAYER_CAPTURE_MATCH_METHOD
LAYER_MATCH_CONFIDENCE_VALUES = layer_metadata.LAYER_MATCH_CONFIDENCE_VALUES
LIVE2D_LAYER_CAPTURE_FIELD = layer_metadata.LIVE2D_LAYER_CAPTURE_FIELD
LIVE2D_LAYER_METADATA_FIELDS = layer_metadata.LIVE2D_LAYER_METADATA_FIELDS
_copy_live2d_layer_metadata = layer_metadata.copy_live2d_layer_metadata
_layer_capture_manifest_summary = layer_metadata.layer_capture_manifest_summary
layer_capture_manifest_summary = layer_metadata.layer_capture_manifest_summary
merge_layer_capture_files = layer_metadata.merge_layer_capture_files
merge_live2d_layer_captures = layer_metadata.merge_live2d_layer_captures
remove_live2d_layer_metadata = layer_metadata.remove_live2d_layer_metadata
strip_layer_metadata_files = layer_metadata.strip_layer_metadata_files
validate_live2d_layer_metadata = layer_metadata.validate_live2d_layer_metadata

GAMEKEE_BASE_URL = 'https://www.gamekee.com'
DEFAULT_USER_AGENT = 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36'
CDN_USER_AGENT = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36'
MEDIA_CELL_TYPES = {'image', 'video', 'live2d'}
IMAGE_LIST_FIELDS = ('icon', 'icon1', 'icon2')
LIVE2D_URL_FIELDS = ('atlas', 'skel', 'json')
ATLAS_TEXTURE_RE = r'^[^\s:]+\.(?:png|webp|jpe?g)$'
SKIN_NAME_LABEL = '时装名称'
SKIN_SERIES_LABEL = '时装系列'
SKIN_OBTAIN_LABEL = '获取方式'
COLLECTION_TERMS = ('珍藏品', '收藏品')
FAVORITE_MODEL_MARKER = 'favorite_'

_API_REQUEST_INTERVAL_SECONDS = 0.5
_CDN_REQUEST_INTERVAL_SECONDS = 0.2
_CDN_CONCURRENCY = 3
_ASSET_PROCESS_CONCURRENCY = 3
_MAX_RETRIES = 3
_LIMITED_RETRY_ATTEMPTS = 2
_RETRY_BASE_DELAY_SECONDS = 0.75
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
_HTTP_TENCENT_EDGE_RESTRICTED = 567

_CREATE_SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS nikke_pages (
    content_id BIGINT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    source_url TEXT NOT NULL DEFAULT '',
    manifest_path TEXT NOT NULL DEFAULT '',
    content_hash TEXT NOT NULL DEFAULT '',
    active BOOLEAN NOT NULL DEFAULT TRUE,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    fetched_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    raw_list_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    raw_detail_json JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS nikke_assets (
    url TEXT PRIMARY KEY,
    normalized_url TEXT NOT NULL DEFAULT '',
    sha256 TEXT NOT NULL DEFAULT '',
    size BIGINT NOT NULL DEFAULT 0,
    content_type TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending',
    failed_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS nikke_blobs (
    sha256 TEXT NOT NULL,
    size BIGINT NOT NULL,
    content_type TEXT NOT NULL DEFAULT '',
    blob_path TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_verified_at TIMESTAMPTZ,
    PRIMARY KEY (sha256, size)
);

CREATE TABLE IF NOT EXISTS nikke_page_assets (
    content_id BIGINT NOT NULL REFERENCES nikke_pages(content_id) ON DELETE CASCADE,
    url TEXT NOT NULL REFERENCES nikke_assets(url) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    context_hash TEXT NOT NULL,
    local_path TEXT NOT NULL DEFAULT '',
    original_filename TEXT NOT NULL DEFAULT '',
    context_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (content_id, url, kind, context_hash)
);

CREATE INDEX IF NOT EXISTS nikke_assets_sha256_size_idx ON nikke_assets (sha256, size);
CREATE INDEX IF NOT EXISTS nikke_page_assets_content_id_idx ON nikke_page_assets (content_id);
"""


class InvalidAssetResponseError(RuntimeError):
    pass


class AssetProcessingError(RuntimeError):
    pass


@dataclass(slots=True)
class Asset:
    url: str
    kind: str
    local_path: str = ''
    content_type: str = ''
    size: int = 0
    sha256: str = ''
    status: str = 'pending'
    error: str = ''
    contexts: list[dict[str, Any]] = field(default_factory=list)


@dataclass(frozen=True, slots=True)
class SkinMeta:
    skin_index: int | None = None
    skin_name: str = ''
    skin_title: str = ''
    skin_series: str = ''
    skin_obtain: str = ''
    is_collection_skin: bool = False


@dataclass(frozen=True, slots=True)
class MediaRow:
    section: str
    row_index: int
    label: str
    cells: list[Any]
    skin_meta: SkinMeta = field(default_factory=SkinMeta)


@dataclass(frozen=True, slots=True)
class BlobRef:
    sha256: str
    size: int
    content_type: str
    path: Path


@dataclass(frozen=True, slots=True)
class TempBlob:
    path: Path
    sha256: str
    size: int
    content_type: str


class _RateLimiter:
    def __init__(self, min_interval_seconds: float) -> None:
        self._min_interval_seconds = min_interval_seconds
        self._lock = asyncio.Lock()
        self._last_start = 0.0

    async def wait(self) -> None:
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
        log.warning('Nikke CDN/API returned repeated throttling responses; pausing this run for %.0fs', _CIRCUIT_PAUSE_SECONDS)
        await asyncio.sleep(_CIRCUIT_PAUSE_SECONDS)
        self._limited_failures = 0

    def record_success(self) -> None:
        self._limited_failures = 0


def _json_dumps(data: Any) -> str:
    return json.dumps(data, ensure_ascii=False, sort_keys=True, separators=(',', ':'), default=str)


def _pretty_json(data: Any) -> str:
    return json.dumps(data, ensure_ascii=False, indent=2, sort_keys=True, default=str) + '\n'


def _utc_now_iso() -> str:
    return datetime.now(UTC).isoformat()


def _layer_capture_attempt_summary(
    content_id: int,
    outcome: dict[str, Any],
    reuse_decision: dict[str, Any] | None = None,
    error: BaseException | None = None,
    previous_capture_error: BaseException | None = None,
) -> dict[str, Any]:
    summary: dict[str, Any] = {
        'schema': 1,
        'content_id': content_id,
        'attempted_at': _utc_now_iso(),
    }
    summary.update(outcome)
    if reuse_decision:
        current_fingerprint = reuse_decision.get('current_fingerprint')
        previous_fingerprint = reuse_decision.get('previous_fingerprint')
        previous_status = reuse_decision.get('previous_status')
        if current_fingerprint:
            summary['fingerprint'] = current_fingerprint
        if previous_fingerprint:
            summary['previous_fingerprint'] = previous_fingerprint
        if previous_status:
            summary['previous_status'] = previous_status
    if error is not None:
        summary['error_class'] = error.__class__.__name__
        summary['error_message'] = _safe_layer_capture_error_message(error)
    if previous_capture_error is not None:
        summary['previous_capture_error_class'] = previous_capture_error.__class__.__name__
        summary['previous_capture_error_message'] = _safe_layer_capture_error_message(previous_capture_error)
    return summary


def _safe_layer_capture_error_message(error: BaseException, *, max_length: int = 240) -> str:
    text = ' '.join(str(error).split())
    if not text:
        return ''
    tokens = ['<path>' if '/' in token or '\\' in token else token for token in text.split(' ')]
    return ' '.join(tokens)[:max_length]


def _layer_metadata_preserve_key(model: dict[str, Any]) -> tuple[int | None, str, str]:
    return (_to_int(model.get('skin_index')), str(model.get('stable_id') or ''), str(model.get('live2d_key') or ''))


def _layer_metadata_snapshot(live2d_models: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [{field: model[field] for field in LIVE2D_LAYER_METADATA_FIELDS if field in model} for model in live2d_models]


def _restore_layer_metadata_snapshot(live2d_models: list[dict[str, Any]], snapshot: list[dict[str, Any]]) -> None:
    for model, fields in zip(live2d_models, snapshot, strict=False):
        for field_name in LIVE2D_LAYER_METADATA_FIELDS:
            model.pop(field_name, None)
        model.update(fields)


def _preserve_previous_manifest_layer_metadata(  # noqa: C901, PLR0911
    root: Path,
    live2d_models: list[dict[str, Any]],
) -> dict[str, Any]:
    manifest_path = root / 'manifest.json'
    if not manifest_path.exists():
        return {'preserved': 0, 'reason': 'no_previous_manifest'}

    try:
        previous_manifest = json.loads(manifest_path.read_text(encoding='utf-8'))
    except (OSError, json.JSONDecodeError) as exc:
        return {'preserved': 0, 'reason': 'previous_manifest_read_failed', 'error_class': exc.__class__.__name__}

    if not isinstance(previous_manifest, dict):
        return {'preserved': 0, 'reason': 'previous_manifest_invalid'}
    previous_summary = previous_manifest.get(LIVE2D_LAYER_CAPTURE_FIELD)
    if not isinstance(previous_summary, dict) or previous_summary.get('status') != 'success':
        return {'preserved': 0, 'reason': 'previous_layer_capture_not_success'}
    previous_models = previous_manifest.get('live2d_models')
    if not isinstance(previous_models, list) or not all(isinstance(model, dict) for model in previous_models):
        return {'preserved': 0, 'reason': 'previous_manifest_models_invalid'}

    previous_by_key = {_layer_metadata_preserve_key(model): model for model in previous_models}
    snapshot = _layer_metadata_snapshot(live2d_models)
    preserved = 0
    for model in live2d_models:
        previous_model = previous_by_key.get(_layer_metadata_preserve_key(model))
        if previous_model is None:
            continue
        before = {field: model.get(field) for field in LIVE2D_LAYER_METADATA_FIELDS}
        _copy_live2d_layer_metadata(model, previous_model)
        after = {field: model.get(field) for field in LIVE2D_LAYER_METADATA_FIELDS}
        if after != before:
            preserved += 1

    if preserved == 0:
        return {'preserved': 0, 'reason': 'no_previous_layer_metadata'}

    issues = validate_live2d_layer_metadata(live2d_models)
    errors = [issue for issue in issues if issue.get('severity') == 'error']
    if errors:
        _restore_layer_metadata_snapshot(live2d_models, snapshot)
        return {
            'preserved': 0,
            'reason': 'previous_layer_metadata_failed_validation',
            'issues': errors,
        }

    return {'preserved': preserved, 'summary': dict(previous_summary), 'issues': issues}


def _to_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    return None


def parse_content_id(target: str) -> int:
    raw = target.strip()
    if raw.isdigit():
        return int(raw)

    patterns = (
        r'/tj/(\d+)\.html(?:[?#].*)?$',
        r'/(\d+)\.html(?:[?#].*)?$',
        r'content/detail/(\d+)(?:[?#].*)?$',
    )
    for pattern in patterns:
        match = re.search(pattern, raw)
        if match:
            return int(match.group(1))

    msg = f'Cannot parse a GameKee content id from {target!r}'
    raise ValueError(msg)


def normalize_url(url: str) -> str:
    value = url.strip()
    if not value:
        return ''
    if value.startswith('//'):
        return f'https:{value}'
    if value.startswith('/'):
        return urljoin(GAMEKEE_BASE_URL, value)
    return value


def request_headers(content_id: int | None = None) -> dict[str, str]:
    referer = f'{GAMEKEE_BASE_URL}/nikke/'
    if content_id:
        referer = f'{GAMEKEE_BASE_URL}/nikke/tj/{content_id}.html'
    return {
        'Accept': 'application/json, text/plain, */*',
        'Game-Alias': 'nikke',
        'Lang': 'zh-cn',
        'Referer': referer,
        'User-Agent': DEFAULT_USER_AGENT,
        'X-Requested-With': 'XMLHttpRequest',
    }


def cdn_request_headers() -> dict[str, str]:
    return {
        'Accept': 'application/json, text/plain, */*',
        'Accept-Language': 'en,zh;q=0.9,zh-CN;q=0.8',
        'Cache-Control': 'no-cache',
        'Origin': GAMEKEE_BASE_URL,
        'Pragma': 'no-cache',
        'Priority': 'u=1, i',
        'Referer': f'{GAMEKEE_BASE_URL}/',
        'Sec-CH-UA': '"Chromium";v="148", "Google Chrome";v="148", "Not/A)Brand";v="99"',
        'Sec-CH-UA-Mobile': '?0',
        'Sec-CH-UA-Platform': '"macOS"',
        'Sec-Fetch-Dest': 'empty',
        'Sec-Fetch-Mode': 'cors',
        'Sec-Fetch-Site': 'same-site',
        'User-Agent': CDN_USER_AGENT,
    }


def asset_headers() -> dict[str, str]:
    return {
        'Accept': '*/*',
        'Origin': GAMEKEE_BASE_URL,
        'Referer': f'{GAMEKEE_BASE_URL}/',
        'User-Agent': DEFAULT_USER_AGENT,
    }


def response_json(response: httpx.Response) -> dict[str, Any]:
    response.raise_for_status()
    data = response.json()
    if not isinstance(data, dict):
        msg = 'GameKee returned a non-object JSON response'
        raise TypeError(msg)
    code = data.get('code')
    if code != 0:
        msg = f'GameKee API returned code={code!r}, msg={data.get("msg")!r}'
        raise RuntimeError(msg)
    return data


def filter_nikke_rows(rows: Any) -> list[dict[str, Any]]:
    if not isinstance(rows, list):
        return []

    filtered: list[dict[str, Any]] = []
    for item in rows:
        if not isinstance(item, dict):
            continue
        nikke = item.get('nikke')
        if not isinstance(nikke, dict):
            continue
        if row_content_id(nikke) is None:
            continue
        filtered.append(nikke)
    return filtered


def row_content_id(row: dict[str, Any]) -> int | None:
    for key in ('content_id', 'contentId', 'id'):
        content_id = _to_int(row.get(key))
        if content_id is not None:
            return content_id
    return None


def row_name(row: dict[str, Any]) -> str:
    for key in ('title', 'name', 'entry_name', 'entryName'):
        value = row.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    return ''


def detail_name(detail: dict[str, Any], content_id: int, row: dict[str, Any] | None = None) -> str:
    for key in ('title', 'name', 'entry_name', 'entryName'):
        value = detail.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    if row:
        name = row_name(row)
        if name:
            return name
    return str(content_id)


def _coerce_json_object(value: Any) -> dict[str, Any]:
    if isinstance(value, dict):
        return value
    if not isinstance(value, str) or not value.strip():
        return {}
    try:
        parsed = json.loads(value)
    except json.JSONDecodeError:
        return {}
    return parsed if isinstance(parsed, dict) else {}


def parse_content_json(detail: dict[str, Any]) -> dict[str, Any]:
    return _coerce_json_object(detail.get('content_json'))


def content_json_from_cdn_payload(payload: dict[str, Any]) -> dict[str, Any]:
    content = payload.get('content')
    if isinstance(content, dict):
        return content
    if isinstance(content, str):
        return _coerce_json_object(content)
    if isinstance(payload.get('baseData'), list) or isinstance(payload.get('styleData'), list):
        return payload
    return {}


def cdn_json_url(detail: dict[str, Any], field: str) -> str:
    value = detail.get(field)
    if not isinstance(value, str) or not value.strip():
        return ''
    return normalize_url(value)


def reverse_bind_from_object(entry_data_bind: Any) -> dict[str, str]:
    bind = _coerce_json_object(entry_data_bind)
    return {str(dynamic_key): str(stable_id) for stable_id, dynamic_key in bind.items()}


def reverse_bind_from_cdn_payload(payload: dict[str, Any]) -> dict[str, str]:
    return reverse_bind_from_object(payload.get('entry_data_bind', payload))


def reverse_bind_map(detail: dict[str, Any]) -> dict[str, str]:
    return reverse_bind_from_object(detail.get('entry_data_bind'))


def first_text(row: list[Any]) -> str:
    for cell in row:
        if not isinstance(cell, dict) or cell.get('type') != 'text':
            continue
        value = cell.get('value')
        if isinstance(value, str) and value.strip():
            return value.strip()
    return ''


def first_value_text(row: list[Any]) -> str:
    for cell in row[1:]:
        if not isinstance(cell, dict):
            continue
        value = cell.get('value')
        if isinstance(value, str) and value.strip():
            return value.strip()
    return ''


def row_value_by_label(rows: list[Any], label: str) -> str:
    for row in rows:
        if isinstance(row, list) and first_text(row) == label:
            return first_value_text(row)
    return ''


def has_favorite_model_marker(rows: list[Any]) -> bool:
    for row in rows:
        if not isinstance(row, list):
            continue
        for cell in row:
            if not isinstance(cell, dict) or cell.get('type') != 'live2d':
                continue
            value = cell.get('value')
            if not isinstance(value, dict):
                continue
            urls = [value.get(field_name) for field_name in (*LIVE2D_URL_FIELDS, 'image')]
            if any(isinstance(url, str) and FAVORITE_MODEL_MARKER in url for url in urls):
                return True
    return False


def skin_metadata(skin: dict[str, Any], skin_index: int) -> SkinMeta:
    rows = skin.get('data')
    row_list = rows if isinstance(rows, list) else []
    skin_name = str(skin.get('name') or f'skin-{skin_index + 1}')
    skin_title = row_value_by_label(row_list, SKIN_NAME_LABEL)
    skin_series = row_value_by_label(row_list, SKIN_SERIES_LABEL)
    skin_obtain = row_value_by_label(row_list, SKIN_OBTAIN_LABEL)
    searchable_text = f'{skin_title}{skin_series}{skin_obtain}'
    is_collection_skin = any(term in searchable_text for term in COLLECTION_TERMS) or has_favorite_model_marker(row_list)
    return SkinMeta(
        skin_index=skin_index,
        skin_name=skin_name,
        skin_title=skin_title,
        skin_series=skin_series,
        skin_obtain=skin_obtain,
        is_collection_skin=is_collection_skin,
    )


def cell_summary(cell: dict[str, Any], reverse_bind: dict[str, str]) -> dict[str, Any]:
    key = str(cell.get('key') or '')
    summary: dict[str, Any] = {
        'key': key,
        'stable_id': reverse_bind.get(key, ''),
        'is_bound': bool(reverse_bind.get(key, '')),
        'type': cell.get('type') or '',
        'value': cell.get('value'),
    }
    if 'isLimit' in cell:
        summary['is_limit'] = cell.get('isLimit')
    return summary


def summarize_rows(rows: list[Any], reverse_bind: dict[str, str]) -> list[dict[str, Any]]:
    summary: list[dict[str, Any]] = []
    for index, row in enumerate(rows):
        if not isinstance(row, list):
            continue
        cells = [cell_summary(cell, reverse_bind) for cell in row if isinstance(cell, dict)]
        summary.append(
            {
                'row_index': index,
                'label': first_text(row),
                'cells': cells,
            },
        )
    return summary


def summarize_content(content_json: dict[str, Any], reverse_bind: dict[str, str]) -> dict[str, Any]:
    base_rows = content_json.get('baseData')
    style_rows = content_json.get('styleData')
    summary: dict[str, Any] = {
        'base_rows': summarize_rows(base_rows if isinstance(base_rows, list) else [], reverse_bind),
        'skins': [],
    }

    if not isinstance(style_rows, list):
        return summary

    for skin_index, skin in enumerate(style_rows):
        if not isinstance(skin, dict):
            continue
        rows = skin.get('data')
        meta = skin_metadata(skin, skin_index)
        summary['skins'].append(
            {
                'skin_index': skin_index,
                'name': meta.skin_name,
                'title': meta.skin_title,
                'series': meta.skin_series,
                'obtain': meta.skin_obtain,
                'is_collection_skin': meta.is_collection_skin,
                'rows': summarize_rows(rows if isinstance(rows, list) else [], reverse_bind),
            },
        )
    return summary


def base_info_from_summary(content_summary: dict[str, Any]) -> dict[str, list[dict[str, Any]]]:
    base_info: dict[str, list[dict[str, Any]]] = {}
    for row in content_summary.get('base_rows', []):
        if not isinstance(row, dict):
            continue
        label = row.get('label')
        cells = row.get('cells')
        if not isinstance(label, str) or not label or not isinstance(cells, list):
            continue
        values = [cell for cell in cells[1:] if isinstance(cell, dict) and cell.get('is_bound') and cell.get('value') not in ('', None)]
        if values:
            base_info.setdefault(label, []).append({'row_index': row.get('row_index'), 'values': values})
    return base_info


def asset_key(url: str, kind: str) -> tuple[str, str]:
    return url, kind


def add_asset(assets: dict[tuple[str, str], Asset], url: str, kind: str, context: dict[str, Any]) -> None:
    normalized = normalize_url(url)
    if not normalized:
        return

    key = asset_key(normalized, kind)
    asset = assets.get(key)
    if asset is None:
        asset = Asset(url=normalized, kind=kind)
        assets[key] = asset
    asset.contexts.append(context)


def add_url_list(assets: dict[tuple[str, str], Asset], raw_urls: Any, kind: str, context: dict[str, Any]) -> None:
    if not isinstance(raw_urls, str):
        return
    for raw_url in raw_urls.split(','):
        add_asset(assets, raw_url, kind, context)


def media_context(row: MediaRow, cell: dict[str, Any], reverse_bind: dict[str, str]) -> dict[str, Any]:
    key = str(cell.get('key') or '')
    context: dict[str, Any] = {
        'section': row.section,
        'row_index': row.row_index,
        'label': row.label,
        'key': key,
        'stable_id': reverse_bind.get(key, ''),
        'cell_type': cell.get('type') or '',
    }
    if row.skin_meta.skin_index is not None:
        context['skin_index'] = row.skin_meta.skin_index
        context['skin_name'] = row.skin_meta.skin_name
        context['skin_title'] = row.skin_meta.skin_title
        context['skin_series'] = row.skin_meta.skin_series
        context['skin_obtain'] = row.skin_meta.skin_obtain
        context['is_collection_skin'] = row.skin_meta.is_collection_skin
    return context


def iter_base_rows(content_json: dict[str, Any]) -> list[tuple[int, list[Any]]]:
    rows = content_json.get('baseData')
    if not isinstance(rows, list):
        return []
    return [(index, row) for index, row in enumerate(rows) if isinstance(row, list)]


def iter_skin_rows(content_json: dict[str, Any]) -> list[tuple[SkinMeta, int, list[Any]]]:
    skins = content_json.get('styleData')
    if not isinstance(skins, list):
        return []

    out: list[tuple[SkinMeta, int, list[Any]]] = []
    for skin_index, skin in enumerate(skins):
        if not isinstance(skin, dict):
            continue
        meta = skin_metadata(skin, skin_index)
        rows = skin.get('data')
        if not isinstance(rows, list):
            continue
        for row_index, row in enumerate(rows):
            if isinstance(row, list):
                out.append((meta, row_index, row))
    return out


def iter_media_rows(content_json: dict[str, Any]) -> list[MediaRow]:
    rows = [
        MediaRow(section='base', row_index=row_index, label=first_text(row), cells=row) for row_index, row in iter_base_rows(content_json)
    ]
    rows.extend(
        MediaRow(
            section='style',
            row_index=row_index,
            label=first_text(row),
            cells=row,
            skin_meta=meta,
        )
        for meta, row_index, row in iter_skin_rows(content_json)
    )
    return rows


def extract_live2d_model(
    *,
    assets: dict[tuple[str, str], Asset],
    context: dict[str, Any],
    value: dict[str, Any],
) -> dict[str, Any]:
    live2d_key = str(value.get('live2dKey') or '')
    model: dict[str, Any] = {
        'label': context.get('label') or '',
        'section': context.get('section') or '',
        'row_index': context.get('row_index'),
        'skin_index': context.get('skin_index'),
        'skin_name': context.get('skin_name') or '',
        'skin_title': context.get('skin_title') or '',
        'skin_series': context.get('skin_series') or '',
        'skin_obtain': context.get('skin_obtain') or '',
        'is_collection_skin': bool(context.get('is_collection_skin')),
        'key': context.get('key') or '',
        'stable_id': context.get('stable_id') or '',
        'live2d_key': live2d_key,
        'animation': value.get('animation') or '',
        'skin': value.get('skin') or '',
        'limit_age': bool(value.get('limitAge')),
        'position': value.get('position') or {},
        'bg_position': value.get('bgPosition') or {},
        'urls': {},
    }
    _copy_live2d_layer_metadata(model, value)

    urls = model['urls']
    for field_name in LIVE2D_URL_FIELDS:
        raw_url = value.get(field_name)
        if isinstance(raw_url, str) and raw_url.strip():
            normalized = normalize_url(raw_url)
            urls[field_name] = normalized
            asset_context = {**context, 'live2d_key': live2d_key, 'live2d_field': field_name}
            add_asset(assets, normalized, f'live2d_{field_name}', asset_context)

    images = value.get('image')
    if isinstance(images, str) and images.strip():
        urls['image'] = [normalize_url(raw_url) for raw_url in images.split(',') if raw_url.strip()]
        for image_url in urls['image']:
            asset_context = {**context, 'live2d_key': live2d_key, 'live2d_field': 'image'}
            add_asset(assets, image_url, 'live2d_texture', asset_context)

    return model


def extract_media_row(
    *,
    row: MediaRow,
    assets: dict[tuple[str, str], Asset],
    live2d_models: list[dict[str, Any]],
    reverse_bind: dict[str, str],
) -> None:
    for cell in row.cells:
        if not isinstance(cell, dict) or cell.get('type') not in MEDIA_CELL_TYPES:
            continue
        if not reverse_bind.get(str(cell.get('key') or '')):
            continue
        context = media_context(row, cell, reverse_bind)
        value = cell.get('value')
        if cell.get('type') == 'live2d' and isinstance(value, dict):
            live2d_models.append(extract_live2d_model(assets=assets, context=context, value=value))
        elif isinstance(value, str):
            add_url_list(assets, value, str(cell.get('type')), context)


def extract_tj_list_assets(assets: dict[tuple[str, str], Asset], tj_list_row: dict[str, Any] | None) -> None:
    if not tj_list_row:
        return
    for field_name in IMAGE_LIST_FIELDS:
        value = tj_list_row.get(field_name)
        if isinstance(value, str) and value.strip():
            add_url_list(
                assets,
                value,
                'image',
                {
                    'section': 'tj_list',
                    'field': field_name,
                    'label': field_name,
                },
            )


def extract_resources(
    *,
    content_json: dict[str, Any],
    tj_list_row: dict[str, Any] | None,
    reverse_bind: dict[str, str],
) -> tuple[dict[tuple[str, str], Asset], list[dict[str, Any]]]:
    assets: dict[tuple[str, str], Asset] = {}
    live2d_models: list[dict[str, Any]] = []

    for row in iter_media_rows(content_json):
        extract_media_row(row=row, assets=assets, live2d_models=live2d_models, reverse_bind=reverse_bind)
    extract_tj_list_assets(assets, tj_list_row)

    return assets, live2d_models


def parse_spine_atlas_textures(text: str) -> list[str]:
    textures: list[str] = []
    seen: set[str] = set()
    for line in text.splitlines():
        value = line.strip().lstrip('\ufeff')
        if not value or ':' in value:
            continue
        if not re.match(ATLAS_TEXTURE_RE, value, re.IGNORECASE):
            continue
        key = value.casefold()
        if key in seen:
            continue
        seen.add(key)
        textures.append(value)
    return textures


def extension_for_kind(kind: str) -> str:
    if kind == 'video':
        return '.mp4'
    if kind == 'live2d_atlas':
        return '.atlas'
    if kind == 'live2d_skel':
        return '.skel'
    if kind == 'live2d_json':
        return '.json'
    return '.bin'


def original_filename_from_url(url: str, kind: str) -> str:
    split = urlsplit(url)
    raw_name = unquote(Path(split.path).name)
    if raw_name:
        return sanitize(raw_name, max_bytes=180) or f'{kind}{extension_for_kind(kind)}'
    return f'{kind}{extension_for_kind(kind)}'


def _safe_suffix(filename: str, kind: str) -> str:
    suffix = Path(filename).suffix or extension_for_kind(kind)
    if not suffix.startswith('.'):
        suffix = f'.{suffix}'
    sanitized = sanitize(suffix, max_bytes=24)
    if sanitized.startswith('.'):
        return sanitized
    return f'.{sanitized or "bin"}'


def _context_source_id(context: dict[str, Any]) -> str:
    for key in ('stable_id', 'field', 'live2d_field', 'key', 'label'):
        value = context.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    row_index = context.get('row_index')
    if row_index is not None:
        return f'row-{row_index}'
    return 'asset'


def filename_from_url(url: str, kind: str, contexts: list[dict[str, Any]], content_id: int) -> str:
    original = original_filename_from_url(url, kind)
    if kind.startswith('live2d_'):
        return original

    context = contexts[0] if contexts else {}
    suffix = _safe_suffix(original, kind)
    original_stem = Path(original).stem or kind
    source_id = _context_source_id(context)
    stem = sanitize(f'{content_id}_{kind}_{source_id}_{original_stem}', max_bytes=180) or f'{content_id}_{kind}'
    return f'{stem}{suffix}'


def asset_subdir(asset: Asset) -> Path:
    if asset.kind.startswith('live2d_'):
        model_key = next(
            (
                context.get('live2d_key') or context.get('stable_id') or context.get('key')
                for context in asset.contexts
                if context.get('live2d_key') or context.get('stable_id') or context.get('key')
            ),
            'unknown',
        )
        return Path('assets/live2d') / (sanitize(str(model_key), max_bytes=80) or 'unknown')
    if asset.kind == 'video':
        return Path('assets/videos')
    return Path('assets/images')


def assign_asset_paths(assets: dict[tuple[str, str], Asset], *, content_id: int) -> None:
    used: dict[str, str] = {}
    for asset in assets.values():
        subdir = asset_subdir(asset)
        filename = filename_from_url(asset.url, asset.kind, asset.contexts, content_id)
        relative = subdir / filename
        relative_key = relative.as_posix()
        if relative_key in used and used[relative_key] != asset.url:
            digest = hashlib.sha256(asset.url.encode('utf-8')).hexdigest()[:8]
            if asset.kind.startswith('live2d_'):
                relative = subdir.with_name(f'{subdir.name}-{digest}') / filename
            else:
                relative = relative.with_stem(f'{relative.stem}-{digest}')
            relative_key = relative.as_posix()
        while relative_key in used and used[relative_key] != asset.url:
            digest = hashlib.sha256(f'{asset.url}:{len(used)}'.encode()).hexdigest()[:8]
            relative = relative.with_stem(f'{relative.stem}-{digest}')
            relative_key = relative.as_posix()
        used[relative_key] = asset.url
        asset.local_path = relative_key


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open('rb') as file:
        for chunk in iter(lambda: file.read(_DOWNLOAD_CHUNK_SIZE), b''):
            digest.update(chunk)
    return digest.hexdigest()


def _verify_file(path: Path, *, sha256: str, size: int) -> bool:
    try:
        if path.stat().st_size != size:
            return False
        return file_sha256(path) == sha256
    except OSError:
        return False


def materialize_blob(*, blob_path: Path, destination: Path, sha256: str, size: int) -> None:
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


def write_json(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f'.{path.name}.tmp-{os.getpid()}-{secrets.token_hex(4)}')
    try:
        tmp.write_text(_pretty_json(data), encoding='utf-8')
        tmp.replace(path)
    finally:
        with suppress(FileNotFoundError):
            tmp.unlink()


def output_root(base_dir: Path, detail: dict[str, Any], content_id: int, row: dict[str, Any] | None = None) -> Path:
    title = detail_name(detail, content_id, row)
    safe_title = sanitize(title, max_bytes=120) or str(content_id)
    return base_dir / f'{content_id} - {safe_title}'


def asset_counts(assets: dict[tuple[str, str], Asset]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for asset in assets.values():
        counts[asset.kind] = counts.get(asset.kind, 0) + 1
    return counts


def relative_manifest_assets(assets: dict[tuple[str, str], Asset]) -> list[dict[str, Any]]:
    return [
        {
            'url': asset.url,
            'kind': asset.kind,
            'local_path': asset.local_path,
            'sha256': asset.sha256,
            'content_type': asset.content_type,
            'size': asset.size,
            'status': asset.status,
            'error': asset.error,
            'contexts': asset.contexts,
        }
        for asset in assets.values()
    ]


def context_hash(context: dict[str, Any]) -> str:
    return hashlib.sha256(_json_dumps(context).encode('utf-8')).hexdigest()


def content_hash(*, detail_response: dict[str, Any], content_json: dict[str, Any], tj_list_row: dict[str, Any] | None) -> str:
    payload = {
        'detail_response': detail_response,
        'content_json': content_json,
        'tj_list_row': tj_list_row,
    }
    return hashlib.sha256(_json_dumps(payload).encode('utf-8')).hexdigest()


def retry_after_seconds(response: httpx.Response | None) -> float | None:
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


def retry_delay_seconds(attempt: int, response: httpx.Response | None = None) -> float:
    retry_after = retry_after_seconds(response)
    if retry_after is not None:
        return min(retry_after, _CIRCUIT_PAUSE_SECONDS)
    jitter = secrets.randbelow(250) / 1000
    return (_RETRY_BASE_DELAY_SECONDS * (2 ** max(0, attempt - 1))) + jitter


def max_attempts_for_status(status_code: int) -> int:
    if status_code in {_HTTP_NOT_FOUND, _HTTP_TENCENT_EDGE_RESTRICTED}:
        return 1
    if status_code in {_HTTP_FORBIDDEN, _HTTP_NOT_ACCEPTABLE}:
        return _LIMITED_RETRY_ATTEMPTS
    if status_code in {_HTTP_REQUEST_TIMEOUT, _HTTP_TOO_MANY_REQUESTS} or status_code >= _HTTP_SERVER_ERROR_MIN:
        return _MAX_RETRIES
    return 1


def is_cdn_rejection_body(content_type: str, body_prefix: bytes) -> bool:
    lowered_type = content_type.lower()
    stripped = body_prefix.lstrip().lower()
    if 'text/html' in lowered_type:
        return True
    if stripped.startswith((b'<!doctype html', b'<html')):
        return True
    rejection_signatures = (b'accessdenied', b'access denied', b'forbidden', b'<error>', b'error code')
    return any(signature in stripped for signature in rejection_signatures)


def validate_asset_response(*, kind: str, content_type: str, body_prefix: bytes, size: int) -> str:
    if size <= 0:
        return 'empty response body'
    if is_cdn_rejection_body(content_type, body_prefix):
        return 'cdn rejection response'

    stripped = body_prefix.lstrip()
    if stripped.startswith(b'{') and kind != 'live2d_json':
        try:
            parsed = json.loads(stripped.decode('utf-8', errors='ignore'))
        except json.JSONDecodeError:
            return ''
        if isinstance(parsed, dict) and ({'code', 'msg', 'message', 'error'} & set(parsed)):
            return 'json error response'
    return ''


class Nikke:
    def __init__(self, *, path: Path | None = None, client: httpx.AsyncClient | None = None) -> None:
        self.path = Path(path or cfg.path)
        self._client = client
        self._api_limiter = _RateLimiter(_API_REQUEST_INTERVAL_SECONDS)
        self._cdn_limiter = _RateLimiter(_CDN_REQUEST_INTERVAL_SECONDS)
        self._cdn_semaphore = asyncio.Semaphore(_CDN_CONCURRENCY)
        self._circuit_breaker = _CircuitBreaker()

    @asynccontextmanager
    async def _http_client(self) -> AsyncIterator[httpx.AsyncClient]:
        if self._client is not None:
            yield self._client
            return

        timeout = httpx.Timeout(60.0, connect=20.0)
        async with httpx.AsyncClient(
            base_url=GAMEKEE_BASE_URL,
            follow_redirects=True,
            headers=request_headers(),
            timeout=timeout,
        ) as client:
            yield client

    async def _request(
        self,
        client: httpx.AsyncClient,
        method: str,
        url: str,
        *,
        headers: dict[str, str] | None = None,
        bucket: str,
    ) -> httpx.Response:
        limiter = self._api_limiter if bucket == 'api' else self._cdn_limiter
        last_exc: Exception | None = None
        response: httpx.Response | None = None

        for attempt in range(1, _MAX_RETRIES + 1):
            await limiter.wait()
            try:
                response = await client.request(method, url, headers=headers)
                if response.status_code < _HTTP_CLIENT_ERROR_MIN:
                    self._circuit_breaker.record_success()
                    return response

                attempts = max_attempts_for_status(response.status_code)
                if response.status_code in {
                    _HTTP_FORBIDDEN,
                    _HTTP_NOT_ACCEPTABLE,
                    _HTTP_TOO_MANY_REQUESTS,
                    _HTTP_SERVICE_UNAVAILABLE,
                }:
                    await self._circuit_breaker.record_limited_failure()
                if attempt >= attempts:
                    response.raise_for_status()
                last_exc = httpx.HTTPStatusError(
                    f'HTTP {response.status_code}',
                    request=response.request,
                    response=response,
                )
            except (httpx.RequestError, httpx.TimeoutException) as exc:
                last_exc = exc

            if attempt < _MAX_RETRIES:
                await asyncio.sleep(retry_delay_seconds(attempt, response))

        if last_exc is not None:
            raise last_exc
        msg = f'Failed to request {url}'
        raise RuntimeError(msg)

    async def _fetch_json(self, client: httpx.AsyncClient, path: str, *, content_id: int | None = None) -> dict[str, Any]:
        response = await self._request(
            client,
            'GET',
            path,
            headers=request_headers(content_id),
            bucket='api',
        )
        return response_json(response)

    async def _fetch_cdn_json(self, client: httpx.AsyncClient, url: str, *, content_id: int, label: str) -> dict[str, Any]:
        response = await self._request(
            client,
            'GET',
            url,
            headers=cdn_request_headers(),
            bucket='api',
        )
        data = response.json()
        if not isinstance(data, dict):
            msg = f'GameKee {label} returned a non-object JSON response for content_id={content_id}'
            raise TypeError(msg)
        return data

    async def _load_content_json(self, client: httpx.AsyncClient, detail: dict[str, Any], *, content_id: int) -> dict[str, Any]:
        content_json = parse_content_json(detail)
        if content_json:
            return content_json

        url = cdn_json_url(detail, 'content_cdn')
        if url:
            content_json = content_json_from_cdn_payload(
                await self._fetch_cdn_json(client, url, content_id=content_id, label='content_cdn'),
            )
            if content_json:
                return content_json
            msg = f'GameKee content_cdn is empty for content_id={content_id}'
            raise RuntimeError(msg)

        msg = f'GameKee detail content_json is empty for content_id={content_id}'
        raise RuntimeError(msg)

    async def _load_reverse_bind(self, client: httpx.AsyncClient, detail: dict[str, Any], *, content_id: int) -> dict[str, str]:
        reverse_bind = reverse_bind_map(detail)
        if reverse_bind:
            return reverse_bind

        url = cdn_json_url(detail, 'entry_data_bind_cdn')
        if not url:
            return {}

        return reverse_bind_from_cdn_payload(await self._fetch_cdn_json(client, url, content_id=content_id, label='entry_data_bind_cdn'))

    async def _fetch_tj_list_rows(self, client: httpx.AsyncClient) -> list[dict[str, Any]]:
        payload = await self._fetch_json(client, '/v1/entry/tj-list')
        return filter_nikke_rows(payload.get('data'))

    async def _fetch_detail_response(self, client: httpx.AsyncClient, content_id: int) -> dict[str, Any]:
        payload = await self._fetch_json(client, f'/v1/content/detail/{content_id}', content_id=content_id)
        detail = payload.get('data')
        if not isinstance(detail, dict):
            msg = f'GameKee detail payload is missing data for content_id={content_id}'
            raise TypeError(msg)
        return payload

    async def _ensure_schema(self) -> None:
        await database.query_db_multi(_CREATE_SCHEMA_SQL)

    async def _upsert_list_pages(self, rows: list[dict[str, Any]]) -> None:
        statements: list[tuple[str, tuple[Any, ...]]] = []
        for row in rows:
            content_id = row_content_id(row)
            if content_id is None:
                continue
            statements.append(
                (
                    """
                    INSERT INTO nikke_pages (
                        content_id, name, source_url, active, raw_list_json, first_seen_at, last_seen_at
                    )
                    VALUES (?, ?, ?, TRUE, ?::jsonb, NOW(), NOW())
                    ON CONFLICT (content_id) DO UPDATE SET
                        name = excluded.name,
                        source_url = excluded.source_url,
                        active = TRUE,
                        raw_list_json = excluded.raw_list_json,
                        last_seen_at = NOW();
                    """,
                    (
                        content_id,
                        row_name(row),
                        f'{GAMEKEE_BASE_URL}/nikke/tj/{content_id}.html',
                        _json_dumps(row),
                    ),
                ),
            )
        if not statements:
            return
        await database.query_db_transaction([('UPDATE nikke_pages SET active = FALSE;', ()), *statements])

    async def _upsert_page_fetch_start(  # noqa: PLR0913
        self,
        *,
        content_id: int,
        name: str,
        row: dict[str, Any] | None,
        detail_response: dict[str, Any],
        manifest_path: Path,
        hash_value: str,
    ) -> None:
        await database.query_db(
            """
            INSERT INTO nikke_pages (
                content_id, name, source_url, manifest_path, content_hash, active,
                first_seen_at, last_seen_at, fetched_at, raw_list_json, raw_detail_json
            )
            VALUES (?, ?, ?, ?, ?, TRUE, NOW(), NOW(), NOW(), ?::jsonb, ?::jsonb)
            ON CONFLICT (content_id) DO UPDATE SET
                name = excluded.name,
                source_url = excluded.source_url,
                manifest_path = excluded.manifest_path,
                content_hash = excluded.content_hash,
                active = TRUE,
                last_seen_at = NOW(),
                fetched_at = NOW(),
                completed_at = NULL,
                raw_list_json = excluded.raw_list_json,
                raw_detail_json = excluded.raw_detail_json;
            """,
            (
                content_id,
                name,
                f'{GAMEKEE_BASE_URL}/nikke/tj/{content_id}.html',
                manifest_path.as_posix(),
                hash_value,
                _json_dumps(row or {}),
                _json_dumps(detail_response),
            ),
        )

    async def _upsert_asset_seen(self, url: str) -> None:
        await database.query_db(
            """
            INSERT INTO nikke_assets (url, normalized_url, status, first_seen_at, last_seen_at)
            VALUES (?, ?, 'pending', NOW(), NOW())
            ON CONFLICT (url) DO UPDATE SET
                normalized_url = excluded.normalized_url,
                last_seen_at = NOW(),
                status = CASE
                    WHEN nikke_assets.status = 'downloaded' THEN nikke_assets.status
                    ELSE excluded.status
                END;
            """,
            (url, url),
        )

    def _resolve_blob_path(self, blob_path: str) -> Path:
        path = Path(blob_path)
        if path.is_absolute():
            return path
        return self.path / path

    async def _completed_blob_for_url(self, url: str) -> BlobRef | None:
        rows = await database.query_db(
            """
            SELECT a.sha256, a.size, COALESCE(NULLIF(a.content_type, ''), b.content_type) AS content_type, b.blob_path
            FROM nikke_assets AS a
            JOIN nikke_blobs AS b ON b.sha256 = a.sha256 AND b.size = a.size
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
                "UPDATE nikke_assets SET status = 'missing', last_error = 'blob missing or failed verification' WHERE url = ?;",
                (url,),
            )
            return None
        return BlobRef(sha256=sha256, size=size, content_type=str(row.get('content_type') or ''), path=blob_path)

    async def _mark_asset_downloaded(self, asset: Asset, blob: BlobRef) -> None:
        await database.query_db(
            """
            UPDATE nikke_assets
            SET sha256 = ?, size = ?, content_type = ?, status = 'downloaded',
                failed_count = 0, last_error = '', last_seen_at = NOW()
            WHERE url = ?;
            """,
            (blob.sha256, blob.size, blob.content_type, asset.url),
        )

    async def _mark_asset_failed(self, asset: Asset, reason: str) -> None:
        await database.query_db(
            """
            INSERT INTO nikke_assets (url, normalized_url, status, failed_count, last_error, first_seen_at, last_seen_at)
            VALUES (?, ?, 'failed', 1, ?, NOW(), NOW())
            ON CONFLICT (url) DO UPDATE SET
                status = 'failed',
                failed_count = nikke_assets.failed_count + 1,
                last_error = excluded.last_error,
                last_seen_at = NOW();
            """,
            (asset.url, asset.url, reason[:500]),
        )

    def _blob_relative_path(self, sha256: str) -> Path:
        return Path('_blobs/sha256') / sha256[:2] / sha256

    async def _register_temp_blob(self, temp_blob: TempBlob) -> BlobRef:
        rows = await database.query_db(
            """
            SELECT blob_path
            FROM nikke_blobs
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
                    'UPDATE nikke_blobs SET last_verified_at = NOW() WHERE sha256 = ? AND size = ?;',
                    (temp_blob.sha256, temp_blob.size),
                )
                return BlobRef(
                    sha256=temp_blob.sha256,
                    size=temp_blob.size,
                    content_type=temp_blob.content_type,
                    path=existing,
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
            INSERT INTO nikke_blobs (sha256, size, content_type, blob_path, created_at, last_verified_at)
            VALUES (?, ?, ?, ?, NOW(), NOW())
            ON CONFLICT (sha256, size) DO UPDATE SET
                content_type = COALESCE(NULLIF(excluded.content_type, ''), nikke_blobs.content_type),
                blob_path = excluded.blob_path,
                last_verified_at = NOW();
            """,
            (temp_blob.sha256, temp_blob.size, temp_blob.content_type, relative_path.as_posix()),
        )
        return BlobRef(sha256=temp_blob.sha256, size=temp_blob.size, content_type=temp_blob.content_type, path=blob_path)

    async def _download_asset_to_temp(self, client: httpx.AsyncClient, asset: Asset) -> TempBlob:  # noqa: C901, PLR0912
        tmp_dir = self.path / '_tmp'
        tmp_dir.mkdir(parents=True, exist_ok=True)
        last_exc: Exception | None = None
        response: httpx.Response | None = None

        for attempt in range(1, _MAX_RETRIES + 1):
            tmp_path: Path | None = None
            try:
                async with self._cdn_semaphore:
                    await self._cdn_limiter.wait()
                    async with client.stream('GET', asset.url, headers=asset_headers()) as stream_response:
                        response = stream_response
                        if stream_response.status_code >= _HTTP_CLIENT_ERROR_MIN:
                            attempts = max_attempts_for_status(stream_response.status_code)
                            if stream_response.status_code in {
                                _HTTP_FORBIDDEN,
                                _HTTP_NOT_ACCEPTABLE,
                                _HTTP_TOO_MANY_REQUESTS,
                                _HTTP_SERVICE_UNAVAILABLE,
                            }:
                                await self._circuit_breaker.record_limited_failure()
                            if attempt >= attempts:
                                stream_response.raise_for_status()
                            last_exc = httpx.HTTPStatusError(
                                f'HTTP {stream_response.status_code}',
                                request=stream_response.request,
                                response=stream_response,
                            )
                            raise last_exc

                        fd, raw_tmp_path = tempfile.mkstemp(prefix='.nikke-download-', dir=tmp_dir)
                        tmp_path = Path(raw_tmp_path)
                        digest = hashlib.sha256()
                        size = 0
                        prefix = bytearray()
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
                        invalid_reason = validate_asset_response(
                            kind=asset.kind,
                            content_type=content_type,
                            body_prefix=bytes(prefix),
                            size=size,
                        )
                        if invalid_reason:
                            if invalid_reason == 'cdn rejection response':
                                await self._circuit_breaker.record_limited_failure()
                            raise InvalidAssetResponseError(invalid_reason)  # noqa: TRY301
                        self._circuit_breaker.record_success()
                        return TempBlob(path=tmp_path, sha256=digest.hexdigest(), size=size, content_type=content_type)
            except (httpx.RequestError, httpx.TimeoutException, httpx.HTTPStatusError, InvalidAssetResponseError) as exc:
                last_exc = exc
                if tmp_path is not None:
                    tmp_path.unlink(missing_ok=True)

            if attempt < _MAX_RETRIES:
                await asyncio.sleep(retry_delay_seconds(attempt, response))

        if last_exc is not None:
            raise last_exc
        msg = f'Failed to download {asset.url}'
        raise RuntimeError(msg)

    async def _process_asset(self, *, client: httpx.AsyncClient, root: Path, asset: Asset) -> bool:
        await self._upsert_asset_seen(asset.url)
        try:
            blob = await self._completed_blob_for_url(asset.url)
            if blob is None:
                temp_blob = await self._download_asset_to_temp(client, asset)
                blob = await self._register_temp_blob(temp_blob)
                asset.status = 'downloaded'
            else:
                asset.status = 'reused'

            materialize_blob(blob_path=blob.path, destination=root / asset.local_path, sha256=blob.sha256, size=blob.size)
            asset.sha256 = blob.sha256
            asset.size = blob.size
            asset.content_type = blob.content_type
            asset.error = ''
            await self._mark_asset_downloaded(asset, blob)
        except Exception as exc:  # noqa: BLE001
            error = str(exc) or exc.__class__.__name__
            asset.status = 'failed'
            asset.error = error
            log.warning('Failed to download Nikke asset %s: %s', asset.url, error)
            await self._mark_asset_failed(asset, error)
            return False
        else:
            return True

    async def _process_assets(self, *, client: httpx.AsyncClient, root: Path, assets: dict[tuple[str, str], Asset]) -> None:
        if not assets:
            return

        assets_by_url: dict[str, list[Asset]] = {}
        for asset in assets.values():
            assets_by_url.setdefault(asset.url, []).append(asset)

        semaphore = asyncio.Semaphore(_ASSET_PROCESS_CONCURRENCY)

        async def _process_url_group(group: list[Asset]) -> None:
            async with semaphore:
                for asset in group:
                    await self._process_asset(client=client, root=root, asset=asset)

        await asyncio.gather(*(_process_url_group(group) for group in assets_by_url.values()))
        failed = [asset for asset in assets.values() if asset.status == 'failed']
        if failed:
            examples = ', '.join(asset.url for asset in failed[:3])
            msg = f'{len(failed)} Nikke assets failed'
            if examples:
                msg = f'{msg}: {examples}'
            raise AssetProcessingError(msg)

    async def _merge_live2d_layer_capture(  # noqa: C901
        self,
        *,
        root: Path,
        content_id: int,
        title: str,
        live2d_models: list[dict[str, Any]],
        allow_runtime_capture: bool,
    ) -> dict[str, Any]:
        if not layer_metadata.has_multi_full_layer_groups(live2d_models):
            return {
                'action': 'skipped',
                'reason': 'no_multi_full_groups',
                'summary': _layer_capture_attempt_summary(
                    content_id=content_id,
                    outcome={
                        'status': 'skipped',
                        'reason': 'no_multi_full_groups',
                        'retryable': False,
                    },
                ),
            }

        if not allow_runtime_capture:
            preserve_report = _preserve_previous_manifest_layer_metadata(root, live2d_models)
            if preserve_report.get('preserved'):
                summary = dict(preserve_report.get('summary') or {})
                summary['attempted_at'] = _utc_now_iso()
                summary['action'] = 'preserved'
                summary['reason'] = 'runtime_capture_not_allowed'
                return {
                    'action': 'preserved',
                    'reason': 'runtime_capture_not_allowed',
                    'preserve_report': preserve_report,
                    'summary': summary,
                }
            return {
                'action': 'skipped',
                'reason': 'runtime_capture_not_allowed',
                'preserve_report': preserve_report,
                'summary': _layer_capture_attempt_summary(
                    content_id=content_id,
                    outcome={
                        'status': 'skipped',
                        'reason': 'runtime_capture_not_allowed',
                        'retryable': False,
                    },
                ),
            }

        previous_capture_artifact: layer_metadata.PreviousLayerCapture | None = None
        previous_capture_error: BaseException | None = None
        try:
            previous_capture_artifact = layer_metadata.read_previous_layer_capture_artifact(root)
        except Exception as exc:  # noqa: BLE001
            previous_capture_error = exc
            log.warning('Failed to read previous Nikke runtime layer capture for %s: %s', content_id, exc)
            if (root / layer_metadata.LAYER_CAPTURE_RAW_PATH).exists():
                with suppress(Exception):
                    previous_capture_artifact = layer_metadata.read_previous_layer_capture_manifest_summary(root)
        previous_capture = previous_capture_artifact.payload if previous_capture_artifact is not None else None
        previous_capture_source = previous_capture_artifact.source if previous_capture_artifact is not None else ''
        reuse_decision = layer_metadata.evaluate_layer_capture_reuse(
            content_id=content_id,
            live2d_models=live2d_models,
            previous_capture=previous_capture,
            force_refresh=bool(getattr(cfg, 'runtime_capture_force_refresh', False)),
        )
        capture_payload: dict[str, Any] | None = None
        action = 'skipped'
        if reuse_decision.get('reusable') and previous_capture is not None:
            capture_payload = previous_capture
            action = 'reused'
        elif bool(getattr(cfg, 'runtime_capture_enabled', False)):
            timeout_ms = int(float(getattr(cfg, 'runtime_capture_timeout_seconds', 60.0)) * 1000)
            try:
                capture_payload = await capture_gamekee_runtime_layers(
                    RuntimeCaptureRequest(
                        content_id=content_id,
                        title=title,
                        models=live2d_models,
                        timeout_ms=timeout_ms,
                    ),
                )
            except Exception as exc:  # noqa: BLE001
                log.warning('Failed to capture Nikke runtime layers for %s: %s', content_id, exc)
                return {
                    'action': 'failed',
                    'reason': 'runtime_capture_failed',
                    'reuse_decision': reuse_decision,
                    'error_class': exc.__class__.__name__,
                    'error_message': _safe_layer_capture_error_message(exc),
                    'summary': _layer_capture_attempt_summary(
                        content_id=content_id,
                        outcome={
                            'status': 'failed',
                            'reason': 'runtime_capture_failed',
                            'retryable': True,
                        },
                        reuse_decision=reuse_decision,
                        error=exc,
                        previous_capture_error=previous_capture_error,
                    ),
                }
            action = 'captured'
        else:
            return {
                'action': action,
                'reason': 'runtime_capture_disabled',
                'reuse_decision': reuse_decision,
                'summary': _layer_capture_attempt_summary(
                    content_id=content_id,
                    outcome={
                        'status': 'skipped',
                        'reason': 'runtime_capture_disabled',
                        'retryable': False,
                    },
                    reuse_decision=reuse_decision,
                    previous_capture_error=previous_capture_error,
                ),
            }

        merge_report = layer_metadata.merge_live2d_layer_captures(live2d_models, capture_payload, content_id=content_id, dry_run=False)
        summary = layer_metadata.layer_capture_manifest_summary(capture_payload, merge_report)
        if action == 'reused' and previous_capture_source == 'manifest_summary':
            previous_capture_hash = capture_payload.get('capture_hash')
            if isinstance(previous_capture_hash, str) and previous_capture_hash.strip():
                summary['capture_hash'] = previous_capture_hash.strip()
        summary['attempted_at'] = _utc_now_iso()
        summary['action'] = action
        if previous_capture_error is not None:
            summary['previous_capture_error_class'] = previous_capture_error.__class__.__name__
            summary['previous_capture_error_message'] = _safe_layer_capture_error_message(previous_capture_error)
        return {
            'action': action,
            'reason': reuse_decision.get('reason') or '',
            'reuse_decision': reuse_decision,
            'merge_report': merge_report,
            'capture_payload': capture_payload,
            'capture_payload_source': previous_capture_source if action == 'reused' else 'runtime',
            'persist_raw_capture': action == 'captured',
            'summary': summary,
        }

    async def _read_known_atlas_text(self, atlas_url: str) -> str | None:
        blob = await self._completed_blob_for_url(atlas_url)
        if blob is None:
            return None
        try:
            return blob.path.read_text(encoding='utf-8', errors='ignore')
        except OSError:
            return None

    async def _expand_atlas_textures(
        self,
        *,
        client: httpx.AsyncClient,
        root: Path,
        assets: dict[tuple[str, str], Asset],
        live2d_models: list[dict[str, Any]],
    ) -> None:
        for model in live2d_models:
            urls = model.get('urls')
            if not isinstance(urls, dict):
                continue
            atlas_url = urls.get('atlas')
            if not isinstance(atlas_url, str) or not atlas_url:
                continue

            atlas_asset = assets.get(asset_key(atlas_url, 'live2d_atlas'))
            if atlas_asset is not None:
                await self._process_asset(client=client, root=root, asset=atlas_asset)

            atlas_text = await self._read_known_atlas_text(atlas_url)
            if atlas_text is None:
                log.warning('Failed to inspect atlas textures for %s: atlas blob is unavailable', atlas_url)
                continue

            textures = parse_spine_atlas_textures(atlas_text)
            if not textures:
                continue

            atlas_texture_urls = [normalize_url(urljoin(atlas_url, texture)) for texture in textures]
            urls['atlas_textures'] = atlas_texture_urls
            for texture_url in atlas_texture_urls:
                add_asset(
                    assets,
                    texture_url,
                    'live2d_texture',
                    {
                        'section': model.get('section') or '',
                        'row_index': model.get('row_index'),
                        'label': model.get('label') or '',
                        'key': model.get('key') or '',
                        'stable_id': model.get('stable_id') or '',
                        'skin_index': model.get('skin_index'),
                        'skin_name': model.get('skin_name') or '',
                        'skin_title': model.get('skin_title') or '',
                        'skin_series': model.get('skin_series') or '',
                        'skin_obtain': model.get('skin_obtain') or '',
                        'is_collection_skin': bool(model.get('is_collection_skin')),
                        'live2d_key': model.get('live2d_key') or '',
                        'live2d_field': 'atlas_texture',
                    },
                )

    async def _replace_page_assets_and_mark_completed(
        self,
        *,
        content_id: int,
        assets: dict[tuple[str, str], Asset],
    ) -> None:
        statements: list[tuple[str, tuple[Any, ...]]] = [('DELETE FROM nikke_page_assets WHERE content_id = ?;', (content_id,))]
        for asset in assets.values():
            original_filename = original_filename_from_url(asset.url, asset.kind)
            contexts = asset.contexts or [{}]
            for context in contexts:
                statements.append(  # noqa: PERF401
                    (
                        """
                        INSERT INTO nikke_page_assets (
                            content_id, url, kind, context_hash, local_path, original_filename, context_json, created_at, updated_at
                        )
                        VALUES (?, ?, ?, ?, ?, ?, ?::jsonb, NOW(), NOW())
                        ON CONFLICT (content_id, url, kind, context_hash) DO UPDATE SET
                            local_path = excluded.local_path,
                            original_filename = excluded.original_filename,
                            context_json = excluded.context_json,
                            updated_at = NOW();
                        """,
                        (
                            content_id,
                            asset.url,
                            asset.kind,
                            context_hash(context),
                            asset.local_path,
                            original_filename,
                            _json_dumps(context),
                        ),
                    ),
                )
        statements.append(('UPDATE nikke_pages SET completed_at = NOW() WHERE content_id = ?;', (content_id,)))
        await database.query_db_transaction(statements)

    async def _crawl_page(
        self,
        *,
        client: httpx.AsyncClient,
        tj_list_row: dict[str, Any] | None,
        content_id: int,
        skip_assets: bool = False,
        allow_runtime_capture: bool | None = None,
    ) -> Path:
        detail_response = await self._fetch_detail_response(client, content_id)
        detail = detail_response['data']
        content_json = await self._load_content_json(client, detail, content_id=content_id)
        reverse_bind = await self._load_reverse_bind(client, detail, content_id=content_id)
        content_summary = summarize_content(content_json, reverse_bind)
        base_info = base_info_from_summary(content_summary)
        assets, live2d_models = extract_resources(content_json=content_json, tj_list_row=tj_list_row, reverse_bind=reverse_bind)

        name = detail_name(detail, content_id, tj_list_row)
        root = output_root(self.path, detail, content_id, tj_list_row)
        manifest_path = root / 'manifest.json'
        hash_value = content_hash(detail_response=detail_response, content_json=content_json, tj_list_row=tj_list_row)
        await self._upsert_page_fetch_start(
            content_id=content_id,
            name=name,
            row=tj_list_row,
            detail_response=detail_response,
            manifest_path=manifest_path,
            hash_value=hash_value,
        )

        write_json(root / 'raw/detail.json', detail_response)
        write_json(root / 'raw/content.json', content_json)
        write_json(root / 'raw/tj-list-row.json', tj_list_row)

        assign_asset_paths(assets, content_id=content_id)
        if not skip_assets:
            await self._expand_atlas_textures(client=client, root=root, assets=assets, live2d_models=live2d_models)
            assign_asset_paths(assets, content_id=content_id)

        layer_capture_result = await self._merge_live2d_layer_capture(
            root=root,
            content_id=content_id,
            title=name,
            live2d_models=live2d_models,
            allow_runtime_capture=(not skip_assets) if allow_runtime_capture is None else (allow_runtime_capture and not skip_assets),
        )
        layer_capture_payload = layer_capture_result.get('capture_payload')
        if isinstance(layer_capture_payload, dict) and layer_capture_result.get('persist_raw_capture') is True:
            write_json(root / layer_metadata.LAYER_CAPTURE_RAW_PATH, layer_capture_payload)
        layer_capture_summary = layer_capture_result.get('summary')

        if not skip_assets:
            await self._process_assets(client=client, root=root, assets=assets)

        manifest = {
            'source_url': f'{GAMEKEE_BASE_URL}/nikke/tj/{content_id}.html',
            'detail_api_url': f'{GAMEKEE_BASE_URL}/v1/content/detail/{content_id}',
            'fetched_at': _utc_now_iso(),
            'content_id': content_id,
            'title': name,
            'entry_id': detail.get('entry_id'),
            'updated_at': detail.get('updated_at'),
            'tj_list': tj_list_row,
            'base_info': base_info,
            'content_summary': content_summary,
            'live2d_models': live2d_models,
            'assets': relative_manifest_assets(assets),
            'asset_counts': asset_counts(assets),
        }
        if isinstance(layer_capture_summary, dict):
            manifest[LIVE2D_LAYER_CAPTURE_FIELD] = layer_capture_summary
        write_json(root / 'manifest.json', manifest)
        character = {
            'content_id': content_id,
            'title': name,
            'entry_id': detail.get('entry_id'),
            'tj_list': tj_list_row,
            'base_info': base_info,
            'skins': content_summary.get('skins', []),
            'live2d_models': live2d_models,
        }
        if isinstance(layer_capture_summary, dict):
            character[LIVE2D_LAYER_CAPTURE_FIELD] = layer_capture_summary
        write_json(root / 'character.json', character)
        if not skip_assets:
            await self._replace_page_assets_and_mark_completed(content_id=content_id, assets=assets)
        return root

    async def download_character(self, target: str, *, skip_assets: bool = False) -> Path:
        content_id = parse_content_id(target)
        await self._ensure_schema()
        self.path.mkdir(parents=True, exist_ok=True)
        async with self._http_client() as client:
            rows = await self._fetch_tj_list_rows(client)
            row = next((item for item in rows if row_content_id(item) == content_id), {'content_id': content_id})
            await self._upsert_list_pages(rows or [row])
            return await self._crawl_page(
                client=client,
                tj_list_row=row,
                content_id=content_id,
                skip_assets=skip_assets,
                allow_runtime_capture=not skip_assets,
            )

    async def update(self) -> None:
        async with database.advisory_lock('nikke') as acquired:
            if not acquired:
                log.info('Nikke update skipped because another run holds the advisory lock')
                return

            await self._ensure_schema()
            self.path.mkdir(parents=True, exist_ok=True)
            async with self._http_client() as client:
                rows = await self._fetch_tj_list_rows(client)
                if not rows:
                    log.warning('GameKee NIKKE list returned no character rows')
                    return

                await self._upsert_list_pages(rows)
                total = len(rows)
                log.info('Found %d GameKee NIKKE character pages', total)
                for index, row in enumerate(rows, start=1):
                    content_id = row_content_id(row)
                    if content_id is None:
                        continue
                    try:
                        log.info('Crawling Nikke %s (%d/%d)', content_id, index, total)
                        await self._crawl_page(client=client, tj_list_row=row, content_id=content_id, allow_runtime_capture=False)
                    except Exception:
                        log.exception('Failed to crawl Nikke content_id=%s', content_id)
