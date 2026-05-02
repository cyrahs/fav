from __future__ import annotations

import asyncio
import hashlib
import json
import os
import re
import secrets
import tempfile
from contextlib import asynccontextmanager
from dataclasses import dataclass, field
from datetime import UTC, datetime
from email.utils import parsedate_to_datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any
from urllib.parse import urljoin

import httpx

from src.core import config, logger
from src.tool import database
from src.tool.bd2_l2d_viewer import VIEWER_PAGE_URL, ViewerResource, fetch_viewer_resources, resource_stem_from_url, viewer_asset_url
from src.tool.filename import sanitize
from src.web.nikke import (
    Asset,
    BlobRef,
    InvalidAssetResponseError,
    TempBlob,
    add_asset,
    add_url_list,
    asset_headers,
    asset_key,
    context_hash,
    materialize_blob,
    normalize_url,
    original_filename_from_url,
    parse_spine_atlas_textures,
    response_json,
    validate_asset_response,
    write_json,
)

if TYPE_CHECKING:
    from collections.abc import AsyncIterator

log = logger.get('bd2')
cfg = config.web.bd2

GAMEKEE_BASE_URL = 'https://www.gamekee.com'
GAME_ALIAS = 'zsca2'
TREE_ROOT_PID = 194491
CHARACTER_GROUPS = {
    122323: '5-star',
    122322: '4-star',
    122318: '3-star',
}
DEFAULT_USER_AGENT = 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36'
LIVE2D_URL_FIELDS = ('atlas', 'skel', 'json')
MEDIA_CELL_TYPES = {'audio', 'image', 'video', 'live2d'}

STYLE_COSTUME_NAME_ROW = 0
STYLE_COSTUME_CATEGORY_ROW = 1
STYLE_LIVE2D_HEADER_ROW = 5
STANDING_LIVE2D_ROW = 6
INTERACTION_LIVE2D_ROW = 7
SKILL_LIVE2D_1_ROW = 8

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
_VIDEO_RETRY_COOLDOWN_DAYS = (1, 3, 7)

_CREATE_SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS bd2_pages (
    content_id BIGINT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    source_url TEXT NOT NULL DEFAULT '',
    manifest_path TEXT NOT NULL DEFAULT '',
    content_hash TEXT NOT NULL DEFAULT '',
    rarity_group_id BIGINT NOT NULL DEFAULT 0,
    rarity_group_name TEXT NOT NULL DEFAULT '',
    active BOOLEAN NOT NULL DEFAULT TRUE,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    fetched_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    raw_tree_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    raw_detail_json JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS bd2_assets (
    url TEXT PRIMARY KEY,
    normalized_url TEXT NOT NULL DEFAULT '',
    sha256 TEXT NOT NULL DEFAULT '',
    size BIGINT NOT NULL DEFAULT 0,
    content_type TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending',
    failed_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    next_retry_at TIMESTAMPTZ,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS bd2_blobs (
    sha256 TEXT NOT NULL,
    size BIGINT NOT NULL,
    content_type TEXT NOT NULL DEFAULT '',
    blob_path TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_verified_at TIMESTAMPTZ,
    PRIMARY KEY (sha256, size)
);

CREATE TABLE IF NOT EXISTS bd2_page_assets (
    content_id BIGINT NOT NULL REFERENCES bd2_pages(content_id) ON DELETE CASCADE,
    url TEXT NOT NULL REFERENCES bd2_assets(url) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    context_hash TEXT NOT NULL,
    local_path TEXT NOT NULL DEFAULT '',
    original_filename TEXT NOT NULL DEFAULT '',
    context_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (content_id, url, kind, context_hash)
);

CREATE INDEX IF NOT EXISTS bd2_assets_sha256_size_idx ON bd2_assets (sha256, size);
CREATE INDEX IF NOT EXISTS bd2_page_assets_content_id_idx ON bd2_page_assets (content_id);
CREATE INDEX IF NOT EXISTS bd2_pages_rarity_group_id_idx ON bd2_pages (rarity_group_id);
ALTER TABLE bd2_assets ADD COLUMN IF NOT EXISTS next_retry_at TIMESTAMPTZ;
"""


class AssetProcessingError(RuntimeError):
    pass


class CrawlRunError(RuntimeError):
    pass


@dataclass(frozen=True, slots=True)
class ArtRow:
    row_index: int
    field: str


@dataclass(frozen=True, slots=True)
class CostumeMeta:
    style_index: int
    style_name: str = ''
    title: str = ''
    category: str = ''


@dataclass(frozen=True, slots=True)
class StyleRows:
    style_index: int
    style_name: str
    rows: list[Any]
    costume: CostumeMeta


@dataclass(slots=True)
class TreeWalkState:
    group_id: int = 0
    group_name: str = ''
    sort_path: list[int] = field(default_factory=list)


@dataclass(frozen=True, slots=True)
class ViewerSupplementTarget:
    row_index: int
    field: str
    label: str


ART_ROWS = {
    2: ArtRow(row_index=2, field='costume_sprite'),
    3: ArtRow(row_index=3, field='costume_portrait'),
    4: ArtRow(row_index=4, field='costume_full_portrait'),
    6: ArtRow(row_index=6, field='standing_live2d'),
    7: ArtRow(row_index=7, field='interaction_live2d'),
    8: ArtRow(row_index=8, field='skill_live2d_1'),
    9: ArtRow(row_index=9, field='skill_live2d_2'),
    10: ArtRow(row_index=10, field='story_live2d_1'),
    11: ArtRow(row_index=11, field='story_live2d_2'),
    12: ArtRow(row_index=12, field='story_live2d_3'),
    13: ArtRow(row_index=13, field='story_live2d_4'),
    14: ArtRow(row_index=14, field='story_live2d_5'),
}

_VIEWER_SUPPLEMENT_SOURCE = 'bd2_l2d_viewer'
_VIEWER_SUPPLEMENT_CORE_FIELD_BY_SUFFIX = {
    '.atlas': 'atlas',
    '.skel': 'skel',
    '.json': 'json',
}
_VIEWER_SUPPLEMENT_ANCHOR_CATEGORY_ORDER = ('character', 'ultimate', 'dating', 'unknown')
_VIEWER_SUPPLEMENT_TARGETS = {
    'character': ViewerSupplementTarget(row_index=STANDING_LIVE2D_ROW, field=ART_ROWS[STANDING_LIVE2D_ROW].field, label='Standing Live2D'),
    'dating': ViewerSupplementTarget(
        row_index=INTERACTION_LIVE2D_ROW,
        field=ART_ROWS[INTERACTION_LIVE2D_ROW].field,
        label='Interaction Live2D',
    ),
    'ultimate': ViewerSupplementTarget(row_index=SKILL_LIVE2D_1_ROW, field=ART_ROWS[SKILL_LIVE2D_1_ROW].field, label='Skill Live2D'),
}


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
        log.warning('BD2 CDN/API returned repeated throttling responses; pausing this run for %.0fs', _CIRCUIT_PAUSE_SECONDS)
        await asyncio.sleep(_CIRCUIT_PAUSE_SECONDS)
        self._limited_failures = 0

    def record_success(self) -> None:
        self._limited_failures = 0


def _json_dumps(data: Any) -> str:
    return json.dumps(data, ensure_ascii=False, sort_keys=True, separators=(',', ':'), default=str)


def _utc_now_iso() -> str:
    return datetime.now(UTC).isoformat()


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
        r'/zsca2/tj/(\d+)\.html(?:[?#].*)?$',
        r'/tj/(\d+)\.html(?:[?#].*)?$',
        r'/zsca2/(\d+)\.html(?:[?#].*)?$',
        r'content/detail/(\d+)(?:[?#].*)?$',
    )
    for pattern in patterns:
        match = re.search(pattern, raw)
        if match:
            return int(match.group(1))

    msg = f'Cannot parse a BD2 GameKee content id from {target!r}'
    raise ValueError(msg)


def request_headers(content_id: int | None = None) -> dict[str, str]:
    referer = f'{GAMEKEE_BASE_URL}/{GAME_ALIAS}/'
    if content_id:
        referer = f'{GAMEKEE_BASE_URL}/{GAME_ALIAS}/tj/{content_id}.html'
    return {
        'Accept': 'application/json, text/plain, */*',
        'Game-Alias': GAME_ALIAS,
        'Lang': 'zh-cn',
        'Referer': referer,
        'User-Agent': DEFAULT_USER_AGENT,
        'X-Requested-With': 'XMLHttpRequest',
    }


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


def reverse_bind_map(detail: dict[str, Any]) -> dict[str, str]:
    entry_data_bind = _coerce_json_object(detail.get('entry_data_bind'))
    return {str(dynamic_key): str(stable_id) for stable_id, dynamic_key in entry_data_bind.items()}


def cell_text(cell: Any) -> str:
    if not isinstance(cell, dict):
        return ''
    value = cell.get('value')
    if isinstance(value, str):
        return value.strip()
    return ''


def first_text(row: list[Any]) -> str:
    for cell in row:
        value = cell_text(cell)
        if value:
            return value
    return ''


def row_label(row: list[Any]) -> str:
    if not row:
        return ''
    return cell_text(row[0]) or first_text(row)


def row_value(row: list[Any], column_index: int) -> str:
    if column_index >= len(row):
        return ''
    return cell_text(row[column_index])


def style_rows(content_json: dict[str, Any]) -> list[StyleRows]:
    styles = content_json.get('styleData')
    if not isinstance(styles, list):
        return []

    out: list[StyleRows] = []
    for style_index, style in enumerate(styles):
        if not isinstance(style, dict):
            continue
        rows = style.get('data')
        if not isinstance(rows, list):
            continue
        style_name = str(style.get('name') or f'style-{style_index + 1}')
        costume = CostumeMeta(
            style_index=style_index,
            style_name=style_name,
            title=row_value(rows[STYLE_COSTUME_NAME_ROW], 1) if len(rows) > STYLE_COSTUME_NAME_ROW and isinstance(rows[0], list) else '',
            category=row_value(rows[STYLE_COSTUME_CATEGORY_ROW], 1)
            if len(rows) > STYLE_COSTUME_CATEGORY_ROW and isinstance(rows[1], list)
            else '',
        )
        out.append(StyleRows(style_index=style_index, style_name=style_name, rows=rows, costume=costume))
    return out


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
        summary.append({'row_index': index, 'label': row_label(row), 'cells': cells})
    return summary


def summarize_content(content_json: dict[str, Any], reverse_bind: dict[str, str]) -> dict[str, Any]:
    base_rows = content_json.get('baseData')
    summary: dict[str, Any] = {
        'base_rows': summarize_rows(base_rows if isinstance(base_rows, list) else [], reverse_bind),
        'costumes': [],
    }

    for style in style_rows(content_json):
        summary['costumes'].append(
            {
                'style_index': style.style_index,
                'style_name': style.style_name,
                'title': style.costume.title,
                'category': style.costume.category,
                'rows': summarize_rows(style.rows, reverse_bind),
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


def row_content_id(row: dict[str, Any]) -> int | None:
    content_id = _to_int(row.get('content_id'))
    if content_id is None or content_id <= 0:
        return None
    return content_id


def row_name(row: dict[str, Any]) -> str:
    for key in ('name', 'title', 'entry_name', 'entryName'):
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


def _walk_tree_rows(rows: Any, state: TreeWalkState | None = None) -> list[dict[str, Any]]:
    if not isinstance(rows, list):
        return []

    current_state = state or TreeWalkState()
    out: list[dict[str, Any]] = []
    for fallback_sort, item in enumerate(rows):
        if not isinstance(item, dict):
            continue
        node_id = _to_int(item.get('id')) or 0
        group_id = current_state.group_id
        group_name = current_state.group_name
        if node_id in CHARACTER_GROUPS:
            group_id = node_id
            group_name = CHARACTER_GROUPS[node_id]

        sort_value = _to_int(item.get('sort'))
        next_state = TreeWalkState(
            group_id=group_id,
            group_name=group_name,
            sort_path=[*current_state.sort_path, sort_value if sort_value is not None else fallback_sort],
        )

        content_id = row_content_id(item)
        if group_id in CHARACTER_GROUPS and content_id is not None:
            row = {**item, '_bd2_group_id': group_id, '_bd2_group_name': group_name, '_bd2_sort_path': next_state.sort_path}
            row.pop('child', None)
            out.append(row)

        children = item.get('child')
        out.extend(_walk_tree_rows(children, next_state))
    return out


def filter_bd2_character_rows(tree_rows: Any) -> list[dict[str, Any]]:
    rows = _walk_tree_rows(tree_rows)
    deduped: dict[int, dict[str, Any]] = {}
    for row in rows:
        content_id = row_content_id(row)
        if content_id is None or content_id in deduped:
            continue
        deduped[content_id] = row
    return sorted(deduped.values(), key=lambda item: tuple(item.get('_bd2_sort_path') or []))


def _style_column_header(style: StyleRows, column_index: int) -> str:
    if len(style.rows) <= STYLE_LIVE2D_HEADER_ROW:
        return ''
    header_row = style.rows[STYLE_LIVE2D_HEADER_ROW]
    if not isinstance(header_row, list):
        return ''
    return row_value(header_row, column_index)


def _style_column_name(style: StyleRows, column_index: int) -> str:
    if len(style.rows) <= STYLE_COSTUME_NAME_ROW:
        return ''
    name_row = style.rows[STYLE_COSTUME_NAME_ROW]
    if not isinstance(name_row, list):
        return ''
    return row_value(name_row, column_index)


def _style_column_category(style: StyleRows, column_index: int) -> str:
    if len(style.rows) <= STYLE_COSTUME_CATEGORY_ROW:
        return ''
    category_row = style.rows[STYLE_COSTUME_CATEGORY_ROW]
    if not isinstance(category_row, list):
        return ''
    return row_value(category_row, column_index)


def _style_column_role(style: StyleRows, column_index: int) -> str:
    return _style_column_header(style, column_index) or _style_column_name(style, column_index)


def _base_media_context(
    *,
    row: list[Any],
    cell: dict[str, Any],
    row_index: int,
    column_index: int,
    reverse_bind: dict[str, str],
) -> dict[str, Any]:
    key = str(cell.get('key') or '')
    return {
        'section': 'base',
        'row_index': row_index,
        'column_index': column_index,
        'label': row_label(row),
        'field': '',
        'key': key,
        'stable_id': reverse_bind.get(key, ''),
        'cell_type': cell.get('type') or '',
    }


def _style_media_context(  # noqa: PLR0913
    *,
    style: StyleRows,
    row: list[Any],
    cell: dict[str, Any],
    row_index: int,
    column_index: int,
    reverse_bind: dict[str, str],
) -> dict[str, Any]:
    key = str(cell.get('key') or '')
    art_row = ART_ROWS.get(row_index)
    column_name = _style_column_name(style, column_index)
    column_category = _style_column_category(style, column_index)
    return {
        'section': 'style',
        'style_index': style.style_index,
        'style_name': style.style_name,
        'costume_title': style.costume.title,
        'costume_category': style.costume.category,
        'row_index': row_index,
        'column_index': column_index,
        'label': row_label(row),
        'field': art_row.field if art_row is not None else '',
        'is_art_row': art_row is not None,
        'column_name': column_name,
        'column_category': column_category,
        'column_role': _style_column_role(style, column_index),
        'column_header': _style_column_header(style, column_index),
        'key': key,
        'stable_id': reverse_bind.get(key, ''),
        'cell_type': cell.get('type') or '',
    }


def extract_live2d_model(
    *,
    assets: dict[tuple[str, str], Asset],
    context: dict[str, Any],
    value: dict[str, Any],
) -> dict[str, Any]:
    live2d_key = str(value.get('live2dKey') or '')
    model: dict[str, Any] = {
        'section': context.get('section') or '',
        'style_index': context.get('style_index'),
        'style_name': context.get('style_name') or '',
        'costume_title': context.get('costume_title') or '',
        'costume_category': context.get('costume_category') or '',
        'row_index': context.get('row_index'),
        'column_index': context.get('column_index'),
        'label': context.get('label') or '',
        'field': context.get('field') or '',
        'is_art_row': bool(context.get('is_art_row')),
        'column_name': context.get('column_name') or '',
        'column_category': context.get('column_category') or '',
        'column_role': context.get('column_role') or '',
        'column_header': context.get('column_header') or '',
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

    urls = model['urls']
    for field_name in LIVE2D_URL_FIELDS:
        raw_url = value.get(field_name)
        if isinstance(raw_url, str) and raw_url.strip():
            normalized = normalize_url(raw_url)
            urls[field_name] = normalized
            add_asset(assets, normalized, f'live2d_{field_name}', {**context, 'live2d_key': live2d_key, 'live2d_field': field_name})

    images = value.get('image')
    if isinstance(images, str) and images.strip():
        urls['image'] = [normalize_url(raw_url) for raw_url in images.split(',') if raw_url.strip()]
        for image_url in urls['image']:
            add_asset(assets, image_url, 'live2d_texture', {**context, 'live2d_key': live2d_key, 'live2d_field': 'image'})

    bg = value.get('bg')
    if isinstance(bg, str) and bg.strip():
        normalized = normalize_url(bg)
        urls['bg'] = normalized
        add_asset(assets, normalized, 'live2d_background', {**context, 'live2d_key': live2d_key, 'live2d_field': 'bg'})

    return model


def _should_collect_media_cell(cell: dict[str, Any]) -> bool:
    return cell.get('type') in MEDIA_CELL_TYPES


def _extract_media_cell(
    *,
    assets: dict[tuple[str, str], Asset],
    live2d_models: list[dict[str, Any]],
    cell: dict[str, Any],
    context: dict[str, Any],
) -> None:
    value = cell.get('value')
    if cell.get('type') == 'live2d' and isinstance(value, dict):
        live2d_models.append(extract_live2d_model(assets=assets, context=context, value=value))
    elif isinstance(value, str):
        add_url_list(assets, value, str(cell.get('type')), context)


def _extract_base_resources(
    *,
    assets: dict[tuple[str, str], Asset],
    live2d_models: list[dict[str, Any]],
    content_json: dict[str, Any],
    reverse_bind: dict[str, str],
) -> None:
    rows = content_json.get('baseData')
    if not isinstance(rows, list):
        return
    for row_index, row in enumerate(rows):
        if not isinstance(row, list):
            continue
        for column_index, cell in enumerate(row):
            if not isinstance(cell, dict) or not _should_collect_media_cell(cell):
                continue
            context = _base_media_context(
                row=row,
                cell=cell,
                row_index=row_index,
                column_index=column_index,
                reverse_bind=reverse_bind,
            )
            _extract_media_cell(assets=assets, live2d_models=live2d_models, cell=cell, context=context)


def _extract_style_resources(
    *,
    assets: dict[tuple[str, str], Asset],
    live2d_models: list[dict[str, Any]],
    content_json: dict[str, Any],
    reverse_bind: dict[str, str],
) -> None:
    for style in style_rows(content_json):
        for row_index, row in enumerate(style.rows):
            if not isinstance(row, list):
                continue
            for column_index, cell in enumerate(row):
                if not isinstance(cell, dict) or not _should_collect_media_cell(cell):
                    continue
                context = _style_media_context(
                    style=style,
                    row=row,
                    cell=cell,
                    row_index=row_index,
                    column_index=column_index,
                    reverse_bind=reverse_bind,
                )
                _extract_media_cell(assets=assets, live2d_models=live2d_models, cell=cell, context=context)


def extract_resources(
    *,
    content_json: dict[str, Any],
    tree_row: dict[str, Any] | None,
    reverse_bind: dict[str, str],
) -> tuple[dict[tuple[str, str], Asset], list[dict[str, Any]]]:
    assets: dict[tuple[str, str], Asset] = {}
    live2d_models: list[dict[str, Any]] = []

    _extract_base_resources(assets=assets, live2d_models=live2d_models, content_json=content_json, reverse_bind=reverse_bind)
    _extract_style_resources(assets=assets, live2d_models=live2d_models, content_json=content_json, reverse_bind=reverse_bind)

    if tree_row:
        for field_name in ('icon', 'icon_small'):
            value = tree_row.get(field_name)
            if isinstance(value, str) and value.strip():
                add_url_list(assets, value, 'image', {'section': 'entry_tree', 'field': field_name, 'label': field_name})

    return assets, live2d_models


def _stem_key(stem: str) -> str:
    return stem.casefold()


def _live2d_model_urls(model: dict[str, Any]) -> tuple[str, ...]:
    raw_urls = model.get('urls')
    if not isinstance(raw_urls, dict):
        return ()

    urls: list[str] = []
    for value in raw_urls.values():
        if isinstance(value, str):
            urls.append(value)
        elif isinstance(value, list):
            urls.extend(item for item in value if isinstance(item, str))
    return tuple(urls)


def _live2d_models_by_stem(live2d_models: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    grouped: dict[str, list[dict[str, Any]]] = {}
    for model in live2d_models:
        model_stem_keys = {_stem_key(stem) for url in _live2d_model_urls(model) if (stem := resource_stem_from_url(url))}
        for stem_key in model_stem_keys:
            grouped.setdefault(stem_key, []).append(model)
    return grouped


def _viewer_resources_by_entry(viewer_resources: tuple[ViewerResource, ...]) -> dict[str, list[ViewerResource]]:
    grouped: dict[str, list[ViewerResource]] = {}
    for resource in viewer_resources:
        grouped.setdefault(resource.entry_id, []).append(resource)
    return grouped


def _viewer_anchor_entry_ids(resource: ViewerResource) -> tuple[str, ...]:
    entry_ids = [resource.entry_id]
    if resource.entry_id.endswith('_c'):
        entry_ids.append(resource.entry_id.removesuffix('_c'))
    return tuple(dict.fromkeys(entry_ids))


def _viewer_anchor_model(
    *,
    resource: ViewerResource,
    viewer_by_entry: dict[str, list[ViewerResource]],
    models_by_stem: dict[str, list[dict[str, Any]]],
) -> dict[str, Any] | None:
    for entry_id in _viewer_anchor_entry_ids(resource):
        siblings = viewer_by_entry.get(entry_id, [])
        for category in _VIEWER_SUPPLEMENT_ANCHOR_CATEGORY_ORDER:
            for sibling in siblings:
                if sibling.category != category:
                    continue
                models = models_by_stem.get(_stem_key(sibling.stem), [])
                if models:
                    return models[0]
    return None


def _is_viewer_censored_resource(resource: ViewerResource) -> bool:
    return resource.entry_id.endswith('_c') or resource.stem.endswith('_c')


def _viewer_supplement_target(resource: ViewerResource) -> ViewerSupplementTarget | None:
    return _VIEWER_SUPPLEMENT_TARGETS.get(resource.category)


def _viewer_core_urls(resource: ViewerResource) -> dict[str, str]:
    urls: dict[str, str] = {}
    for path in resource.files:
        field_name = _VIEWER_SUPPLEMENT_CORE_FIELD_BY_SUFFIX.get(Path(path).suffix.casefold())
        if field_name is None or field_name in urls:
            continue
        url = viewer_asset_url(path)
        if url:
            urls[field_name] = url
    return urls


def _has_required_live2d_core(urls: dict[str, str]) -> bool:
    return 'atlas' in urls and bool({'skel', 'json'} & set(urls))


def _same_live2d_slot(model: dict[str, Any], anchor: dict[str, Any], *, field_name: str) -> bool:
    return (
        model.get('style_index') == anchor.get('style_index')
        and model.get('column_index') == anchor.get('column_index')
        and model.get('field') == field_name
    )


def _viewer_slot_is_filled(
    *,
    live2d_models: list[dict[str, Any]],
    anchor: dict[str, Any],
    target: ViewerSupplementTarget,
    resource: ViewerResource,
) -> bool:
    if _is_viewer_censored_resource(resource) and target.field == ART_ROWS[STANDING_LIVE2D_ROW].field:
        return False
    return any(_same_live2d_slot(model, anchor, field_name=target.field) for model in live2d_models)


def _viewer_supplement_context(model: dict[str, Any], *, live2d_field: str) -> dict[str, Any]:
    context = live2d_atlas_texture_context(model)
    context['live2d_field'] = live2d_field
    return context


def _viewer_supplement_model(
    *,
    anchor: dict[str, Any],
    resource: ViewerResource,
    target: ViewerSupplementTarget,
) -> dict[str, Any] | None:
    urls = _viewer_core_urls(resource)
    if not _has_required_live2d_core(urls):
        return None

    variant = 'censored' if _is_viewer_censored_resource(resource) else ''
    return {
        'section': anchor.get('section') or 'style',
        'style_index': anchor.get('style_index'),
        'style_name': anchor.get('style_name') or '',
        'costume_title': anchor.get('costume_title') or '',
        'costume_category': anchor.get('costume_category') or '',
        'row_index': target.row_index,
        'column_index': anchor.get('column_index'),
        'label': target.label,
        'field': target.field,
        'is_art_row': True,
        'column_name': anchor.get('column_name') or '',
        'column_category': anchor.get('column_category') or '',
        'column_role': anchor.get('column_role') or '',
        'column_header': anchor.get('column_header') or '',
        'key': '',
        'stable_id': '',
        'live2d_key': f'bd2-l2d-viewer-{resource.entry_id}-{resource.stem}',
        'animation': '',
        'skin': '',
        'limit_age': False,
        'position': {},
        'bg_position': {},
        'source': _VIEWER_SUPPLEMENT_SOURCE,
        'variant': variant,
        'viewer_entry_id': resource.entry_id,
        'viewer_stem': resource.stem,
        'source_page_url': VIEWER_PAGE_URL,
        'urls': urls,
    }


def _add_viewer_model_assets(assets: dict[tuple[str, str], Asset], model: dict[str, Any]) -> None:
    raw_urls = model.get('urls')
    if not isinstance(raw_urls, dict):
        return

    for field_name in LIVE2D_URL_FIELDS:
        url = raw_urls.get(field_name)
        if isinstance(url, str) and url:
            add_asset(assets, url, f'live2d_{field_name}', _viewer_supplement_context(model, live2d_field=field_name))


def supplement_live2d_models_from_viewer(
    *,
    assets: dict[tuple[str, str], Asset],
    live2d_models: list[dict[str, Any]],
    viewer_resources: tuple[ViewerResource, ...],
) -> int:
    models_by_stem = _live2d_models_by_stem(live2d_models)
    existing_stem_keys = set(models_by_stem)
    viewer_by_entry = _viewer_resources_by_entry(viewer_resources)
    added = 0

    for resource in viewer_resources:
        stem_key = _stem_key(resource.stem)
        if stem_key in existing_stem_keys or resource.missing_core_files:
            continue
        target = _viewer_supplement_target(resource)
        if target is None:
            continue
        anchor = _viewer_anchor_model(resource=resource, viewer_by_entry=viewer_by_entry, models_by_stem=models_by_stem)
        if anchor is None or _viewer_slot_is_filled(live2d_models=live2d_models, anchor=anchor, target=target, resource=resource):
            continue
        model = _viewer_supplement_model(anchor=anchor, resource=resource, target=target)
        if model is None:
            continue
        _add_viewer_model_assets(assets, model)
        live2d_models.append(model)
        existing_stem_keys.add(stem_key)
        models_by_stem.setdefault(stem_key, []).append(model)
        added += 1

    return added


def extension_for_kind(kind: str) -> str:
    if kind == 'audio':
        return '.mp3'
    if kind == 'video':
        return '.mp4'
    if kind == 'live2d_atlas':
        return '.atlas'
    if kind == 'live2d_skel':
        return '.skel'
    if kind == 'live2d_json':
        return '.json'
    return '.bin'


def _safe_suffix(filename: str, kind: str) -> str:
    suffix = Path(filename).suffix or extension_for_kind(kind)
    if not suffix.startswith('.'):
        suffix = f'.{suffix}'
    sanitized = sanitize(suffix, max_bytes=24)
    if sanitized.startswith('.'):
        return sanitized
    return f'.{sanitized or "bin"}'


def _context_source_id(context: dict[str, Any]) -> str:
    for key in ('field', 'stable_id', 'live2d_field', 'column_role', 'key', 'label'):
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
    if asset.kind == 'audio':
        return Path('assets/audio')
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


def live2d_atlas_texture_context(model: dict[str, Any]) -> dict[str, Any]:
    return {
        'section': model.get('section') or '',
        'style_index': model.get('style_index'),
        'style_name': model.get('style_name') or '',
        'costume_title': model.get('costume_title') or '',
        'costume_category': model.get('costume_category') or '',
        'row_index': model.get('row_index'),
        'column_index': model.get('column_index'),
        'label': model.get('label') or '',
        'field': model.get('field') or '',
        'is_art_row': bool(model.get('is_art_row')),
        'column_name': model.get('column_name') or '',
        'column_category': model.get('column_category') or '',
        'column_role': model.get('column_role') or '',
        'column_header': model.get('column_header') or '',
        'key': model.get('key') or '',
        'stable_id': model.get('stable_id') or '',
        'live2d_key': model.get('live2d_key') or '',
        'live2d_field': 'atlas_texture',
        'source': model.get('source') or '',
        'variant': model.get('variant') or '',
        'viewer_entry_id': model.get('viewer_entry_id') or '',
        'viewer_stem': model.get('viewer_stem') or '',
        'source_page_url': model.get('source_page_url') or '',
    }


def content_hash(*, detail_response: dict[str, Any], content_json: dict[str, Any], tree_row: dict[str, Any] | None) -> str:
    payload = {
        'detail_response': detail_response,
        'content_json': content_json,
        'tree_row': tree_row,
    }
    return hashlib.sha256(_json_dumps(payload).encode('utf-8')).hexdigest()


def retry_after_delay_seconds(response: httpx.Response, *, now: datetime | None = None) -> float | None:
    value = response.headers.get('retry-after')
    if not value:
        return None

    stripped = value.strip()
    if not stripped:
        return None

    if stripped.isdecimal():
        return min(float(stripped), _RETRY_AFTER_MAX_DELAY_SECONDS)

    try:
        retry_at = parsedate_to_datetime(stripped)
    except (TypeError, ValueError):
        return None
    if retry_at.tzinfo is None:
        retry_at = retry_at.replace(tzinfo=UTC)

    delay = (retry_at - (now or datetime.now(UTC))).total_seconds()
    return min(max(delay, 0.0), _RETRY_AFTER_MAX_DELAY_SECONDS)


def retry_delay_seconds(attempt: int, response: httpx.Response | None = None) -> float:
    if response is not None:
        retry_after = retry_after_delay_seconds(response)
        if retry_after is not None:
            return retry_after

    jitter = secrets.randbelow(250) / 1000
    return (_RETRY_BASE_DELAY_SECONDS * (2 ** max(0, attempt - 1))) + jitter


def max_attempts_for_status(status_code: int) -> int:
    if status_code == _HTTP_NOT_FOUND:
        return 1
    if status_code in {_HTTP_FORBIDDEN, _HTTP_NOT_ACCEPTABLE}:
        return _LIMITED_RETRY_ATTEMPTS
    if status_code in {_HTTP_REQUEST_TIMEOUT, _HTTP_TOO_MANY_REQUESTS} or status_code >= _HTTP_SERVER_ERROR_MIN:
        return _MAX_RETRIES
    return 1


def video_retry_cooldown_days(failed_count: int) -> int:
    index = min(max(1, failed_count), len(_VIDEO_RETRY_COOLDOWN_DAYS)) - 1
    return _VIDEO_RETRY_COOLDOWN_DAYS[index]


def is_nonblocking_failed_asset(asset: Asset) -> bool:
    return asset.kind == 'video'


class BD2:
    def __init__(
        self,
        *,
        path: Path | None = None,
        client: httpx.AsyncClient | None = None,
        viewer_resources: tuple[ViewerResource, ...] | None = None,
    ) -> None:
        self.path = Path(path or cfg.path)
        self._client = client
        self._viewer_resources = viewer_resources
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
            retry_delay = retry_delay_seconds(attempt)
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
                retry_delay = retry_delay_seconds(attempt, response)
                last_exc = httpx.HTTPStatusError(
                    f'HTTP {response.status_code}',
                    request=response.request,
                    response=response,
                )
            except (httpx.RequestError, httpx.TimeoutException) as exc:
                last_exc = exc

            if attempt < _MAX_RETRIES:
                await asyncio.sleep(retry_delay)

        if last_exc is not None:
            raise last_exc
        msg = f'Failed to request {url}'
        raise RuntimeError(msg)

    async def _fetch_json(self, client: httpx.AsyncClient, path: str, *, content_id: int | None = None) -> dict[str, Any]:
        response = await self._request(client, 'GET', path, headers=request_headers(content_id), bucket='api')
        return response_json(response)

    async def _fetch_tree_rows(self, client: httpx.AsyncClient) -> list[dict[str, Any]]:
        payload = await self._fetch_json(client, f'/v1/entry/treesByPid?pid={TREE_ROOT_PID}')
        return filter_bd2_character_rows(payload.get('data'))

    async def _fetch_detail_response(self, client: httpx.AsyncClient, content_id: int) -> dict[str, Any]:
        payload = await self._fetch_json(client, f'/v1/content/detail/{content_id}', content_id=content_id)
        detail = payload.get('data')
        if not isinstance(detail, dict):
            msg = f'GameKee detail payload is missing data for content_id={content_id}'
            raise TypeError(msg)
        return payload

    async def _viewer_resources_for_supplement(self) -> tuple[ViewerResource, ...]:
        if self._viewer_resources is not None:
            return self._viewer_resources

        try:
            self._viewer_resources = await asyncio.to_thread(fetch_viewer_resources)
        except Exception as exc:  # noqa: BLE001
            log.warning('BD2 L2D Viewer supplement is unavailable for this run: %s', exc)
            self._viewer_resources = ()
        return self._viewer_resources

    async def _ensure_schema(self) -> None:
        await database.query_db_multi(_CREATE_SCHEMA_SQL)

    async def _upsert_tree_pages(self, rows: list[dict[str, Any]], *, deactivate_missing: bool) -> None:
        statements: list[tuple[str, tuple[Any, ...]]] = []
        for row in rows:
            content_id = row_content_id(row)
            if content_id is None:
                continue
            statements.append(
                (
                    """
                    INSERT INTO bd2_pages (
                        content_id, name, source_url, rarity_group_id, rarity_group_name,
                        active, raw_tree_json, first_seen_at, last_seen_at
                    )
                    VALUES (?, ?, ?, ?, ?, TRUE, ?::jsonb, NOW(), NOW())
                    ON CONFLICT (content_id) DO UPDATE SET
                        name = excluded.name,
                        source_url = excluded.source_url,
                        rarity_group_id = excluded.rarity_group_id,
                        rarity_group_name = excluded.rarity_group_name,
                        active = TRUE,
                        raw_tree_json = excluded.raw_tree_json,
                        last_seen_at = NOW();
                    """,
                    (
                        content_id,
                        row_name(row),
                        f'{GAMEKEE_BASE_URL}/{GAME_ALIAS}/tj/{content_id}.html',
                        _to_int(row.get('_bd2_group_id')) or 0,
                        str(row.get('_bd2_group_name') or ''),
                        _json_dumps(row),
                    ),
                ),
            )
        if not statements:
            return
        if deactivate_missing:
            statements.insert(0, ('UPDATE bd2_pages SET active = FALSE;', ()))
        await database.query_db_transaction(statements)

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
            INSERT INTO bd2_pages (
                content_id, name, source_url, manifest_path, content_hash, rarity_group_id, rarity_group_name,
                active, first_seen_at, last_seen_at, fetched_at, raw_tree_json, raw_detail_json
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, TRUE, NOW(), NOW(), NOW(), ?::jsonb, ?::jsonb)
            ON CONFLICT (content_id) DO UPDATE SET
                name = excluded.name,
                source_url = excluded.source_url,
                manifest_path = excluded.manifest_path,
                content_hash = excluded.content_hash,
                rarity_group_id = excluded.rarity_group_id,
                rarity_group_name = excluded.rarity_group_name,
                active = TRUE,
                last_seen_at = NOW(),
                fetched_at = NOW(),
                raw_tree_json = excluded.raw_tree_json,
                raw_detail_json = excluded.raw_detail_json;
            """,
            (
                content_id,
                name,
                f'{GAMEKEE_BASE_URL}/{GAME_ALIAS}/tj/{content_id}.html',
                manifest_path.as_posix(),
                hash_value,
                _to_int((row or {}).get('_bd2_group_id')) or 0,
                str((row or {}).get('_bd2_group_name') or ''),
                _json_dumps(row or {}),
                _json_dumps(detail_response),
            ),
        )

    async def _upsert_asset_seen(self, url: str) -> None:
        await database.query_db(
            """
            INSERT INTO bd2_assets (url, normalized_url, status, first_seen_at, last_seen_at)
            VALUES (?, ?, 'pending', NOW(), NOW())
            ON CONFLICT (url) DO UPDATE SET
                normalized_url = excluded.normalized_url,
                last_seen_at = NOW(),
                status = CASE
                    WHEN bd2_assets.status = 'downloaded' THEN bd2_assets.status
                    WHEN bd2_assets.status = 'failed' AND bd2_assets.next_retry_at > NOW() THEN bd2_assets.status
                    ELSE excluded.status
                END,
                next_retry_at = CASE
                    WHEN bd2_assets.status = 'failed' AND bd2_assets.next_retry_at > NOW() THEN bd2_assets.next_retry_at
                    ELSE NULL
                END;
            """,
            (url, url),
        )

    async def _video_retry_cooldown(self, asset: Asset) -> dict[str, Any] | None:
        if asset.kind != 'video':
            return None
        rows = await database.query_db(
            """
            SELECT failed_count, last_error, next_retry_at
            FROM bd2_assets
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
            SELECT a.sha256, a.size, COALESCE(NULLIF(a.content_type, ''), b.content_type) AS content_type, b.blob_path
            FROM bd2_assets AS a
            JOIN bd2_blobs AS b ON b.sha256 = a.sha256 AND b.size = a.size
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
                "UPDATE bd2_assets SET status = 'missing', last_error = 'blob missing or failed verification' WHERE url = ?;",
                (url,),
            )
            return None
        return BlobRef(sha256=sha256, size=size, content_type=str(row.get('content_type') or ''), path=blob_path)

    async def _mark_asset_downloaded(self, asset: Asset, blob: BlobRef) -> None:
        await database.query_db(
            """
            UPDATE bd2_assets
            SET sha256 = ?, size = ?, content_type = ?, status = 'downloaded',
                failed_count = 0, last_error = '', next_retry_at = NULL, last_seen_at = NOW()
            WHERE url = ?;
            """,
            (blob.sha256, blob.size, blob.content_type, asset.url),
        )

    async def _mark_asset_failed(self, asset: Asset, reason: str) -> None:
        first_retry_days = video_retry_cooldown_days(1)
        second_retry_days = video_retry_cooldown_days(2)
        later_retry_days = video_retry_cooldown_days(3)
        next_retry_days = first_retry_days if asset.kind == 'video' else None
        await database.query_db(
            """
            INSERT INTO bd2_assets (
                url, normalized_url, status, failed_count, last_error, next_retry_at, first_seen_at, last_seen_at
            )
            VALUES (?, ?, 'failed', 1, ?, NOW() + (?::integer * INTERVAL '1 day'), NOW(), NOW())
            ON CONFLICT (url) DO UPDATE SET
                status = 'failed',
                failed_count = bd2_assets.failed_count + 1,
                last_error = excluded.last_error,
                next_retry_at = CASE
                    WHEN ?::boolean THEN NOW() + (
                        CASE
                            WHEN bd2_assets.failed_count + 1 <= 1 THEN ?::integer * INTERVAL '1 day'
                            WHEN bd2_assets.failed_count + 1 = 2 THEN ?::integer * INTERVAL '1 day'
                            ELSE ?::integer * INTERVAL '1 day'
                        END
                    )
                    ELSE NULL
                END,
                last_seen_at = NOW();
            """,
            (
                asset.url,
                asset.url,
                reason[:500],
                next_retry_days,
                asset.kind == 'video',
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
            SELECT blob_path
            FROM bd2_blobs
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
                    'UPDATE bd2_blobs SET last_verified_at = NOW() WHERE sha256 = ? AND size = ?;',
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
            INSERT INTO bd2_blobs (sha256, size, content_type, blob_path, created_at, last_verified_at)
            VALUES (?, ?, ?, ?, NOW(), NOW())
            ON CONFLICT (sha256, size) DO UPDATE SET
                content_type = COALESCE(NULLIF(excluded.content_type, ''), bd2_blobs.content_type),
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

        for attempt in range(1, _MAX_RETRIES + 1):
            tmp_path: Path | None = None
            retry_delay = retry_delay_seconds(attempt)
            try:
                async with self._cdn_semaphore:
                    await self._cdn_limiter.wait()
                    async with client.stream('GET', asset.url, headers=asset_headers()) as stream_response:
                        if stream_response.status_code >= _HTTP_CLIENT_ERROR_MIN:
                            attempts = max_attempts_for_status(stream_response.status_code)
                            if stream_response.status_code in {
                                _HTTP_FORBIDDEN,
                                _HTTP_NOT_ACCEPTABLE,
                                _HTTP_TOO_MANY_REQUESTS,
                                _HTTP_SERVICE_UNAVAILABLE,
                            }:
                                await self._circuit_breaker.record_limited_failure()
                            last_exc = httpx.HTTPStatusError(
                                f'HTTP {stream_response.status_code}',
                                request=stream_response.request,
                                response=stream_response,
                            )
                            if attempt >= attempts:
                                raise last_exc
                            retry_delay = retry_delay_seconds(attempt, stream_response)
                            raise last_exc

                        fd, raw_tmp_path = tempfile.mkstemp(prefix='.bd2-download-', dir=tmp_dir)
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
                if isinstance(exc, httpx.HTTPStatusError) and attempt >= max_attempts_for_status(exc.response.status_code):
                    raise

            if attempt < _MAX_RETRIES:
                await asyncio.sleep(retry_delay)

        if last_exc is not None:
            raise last_exc
        msg = f'Failed to download {asset.url}'
        raise RuntimeError(msg)

    async def _process_asset(self, *, client: httpx.AsyncClient, root: Path, asset: Asset) -> bool:
        await self._upsert_asset_seen(asset.url)
        cooldown = await self._video_retry_cooldown(asset)
        if cooldown is not None:
            next_retry_at = str(cooldown.get('next_retry_at') or '')
            asset.status = 'failed'
            asset.error = str(cooldown.get('last_error') or 'retry cooldown active')
            log.info('Skipping BD2 video asset during retry cooldown until %s: %s', next_retry_at, asset.url)
            return False
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
            log.warning('Failed to download BD2 asset %s: %s', asset.url, error)
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
        blocking_failed = [asset for asset in failed if not is_nonblocking_failed_asset(asset)]
        if blocking_failed:
            examples = ', '.join(asset.url for asset in blocking_failed[:3])
            msg = f'{len(blocking_failed)} BD2 assets failed'
            if examples:
                msg = f'{msg}: {examples}'
            raise AssetProcessingError(msg)
        if failed:
            log.info('BD2 page kept %d unavailable video assets in manifest without failing the page', len(failed))

    async def _read_known_atlas_text(self, atlas_url: str) -> str | None:
        blob = await self._completed_blob_for_url(atlas_url)
        if blob is None:
            return None
        try:
            return blob.path.read_text(encoding='utf-8', errors='ignore')
        except OSError:
            return None

    async def _expand_model_atlas_textures(
        self,
        *,
        client: httpx.AsyncClient,
        root: Path,
        assets: dict[tuple[str, str], Asset],
        model: dict[str, Any],
    ) -> str | None:
        urls = model.get('urls')
        if not isinstance(urls, dict):
            return None
        atlas_url = urls.get('atlas')
        if not isinstance(atlas_url, str) or not atlas_url:
            return None

        atlas_asset = assets.get(asset_key(atlas_url, 'live2d_atlas'))
        failed_url: str | None = None
        if atlas_asset is None or not await self._process_asset(client=client, root=root, asset=atlas_asset):
            failed_url = atlas_url
        else:
            atlas_text = await self._read_known_atlas_text(atlas_url)
            if atlas_text is None:
                log.warning('Failed to inspect atlas textures for %s: atlas blob is unavailable', atlas_url)
                failed_url = atlas_url
            else:
                textures = parse_spine_atlas_textures(atlas_text)
                if textures:
                    atlas_texture_urls = [normalize_url(urljoin(atlas_url, texture)) for texture in textures]
                    urls['atlas_textures'] = atlas_texture_urls
                    context = live2d_atlas_texture_context(model)
                    for texture_url in atlas_texture_urls:
                        add_asset(assets, texture_url, 'live2d_texture', context)
        return failed_url

    async def _expand_atlas_textures(
        self,
        *,
        client: httpx.AsyncClient,
        root: Path,
        assets: dict[tuple[str, str], Asset],
        live2d_models: list[dict[str, Any]],
    ) -> None:
        failed_atlas_urls: list[str] = []
        for model in live2d_models:
            failed_url = await self._expand_model_atlas_textures(client=client, root=root, assets=assets, model=model)
            if failed_url is not None:
                failed_atlas_urls.append(failed_url)
        if failed_atlas_urls:
            examples = ', '.join(failed_atlas_urls[:3])
            msg = f'Failed to inspect {len(failed_atlas_urls)} BD2 Live2D atlas files'
            if examples:
                msg = f'{msg}: {examples}'
            raise AssetProcessingError(msg)

    async def _replace_page_assets_and_mark_completed(self, *, content_id: int, assets: dict[tuple[str, str], Asset]) -> None:
        statements: list[tuple[str, tuple[Any, ...]]] = [('DELETE FROM bd2_page_assets WHERE content_id = ?;', (content_id,))]
        for asset in assets.values():
            original_filename = original_filename_from_url(asset.url, asset.kind)
            contexts = asset.contexts or [{}]
            for context in contexts:
                statements.append(  # noqa: PERF401
                    (
                        """
                        INSERT INTO bd2_page_assets (
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
        statements.append(('UPDATE bd2_pages SET completed_at = NOW() WHERE content_id = ?;', (content_id,)))
        await database.query_db_transaction(statements)

    async def _crawl_page(
        self,
        *,
        client: httpx.AsyncClient,
        tree_row: dict[str, Any] | None,
        content_id: int,
        skip_assets: bool = False,
    ) -> Path:
        detail_response = await self._fetch_detail_response(client, content_id)
        detail = detail_response['data']
        content_json = parse_content_json(detail)
        reverse_bind = reverse_bind_map(detail)
        content_summary = summarize_content(content_json, reverse_bind)
        base_info = base_info_from_summary(content_summary)
        assets, live2d_models = extract_resources(content_json=content_json, tree_row=tree_row, reverse_bind=reverse_bind)
        viewer_added = supplement_live2d_models_from_viewer(
            assets=assets,
            live2d_models=live2d_models,
            viewer_resources=await self._viewer_resources_for_supplement(),
        )
        if viewer_added:
            log.info('Supplemented %d BD2 Live2D models from BD2-L2D-Viewer for content_id=%d', viewer_added, content_id)

        name = detail_name(detail, content_id, tree_row)
        root = output_root(self.path, detail, content_id, tree_row)
        manifest_path = root / 'manifest.json'
        hash_value = content_hash(detail_response=detail_response, content_json=content_json, tree_row=tree_row)
        await self._upsert_page_fetch_start(
            content_id=content_id,
            name=name,
            row=tree_row,
            detail_response=detail_response,
            manifest_path=manifest_path,
            hash_value=hash_value,
        )

        write_json(root / 'raw/detail.json', detail_response)
        write_json(root / 'raw/content.json', content_json)
        write_json(root / 'raw/tree-row.json', tree_row)

        assign_asset_paths(assets, content_id=content_id)
        if not skip_assets:
            await self._expand_atlas_textures(client=client, root=root, assets=assets, live2d_models=live2d_models)
            assign_asset_paths(assets, content_id=content_id)
            await self._process_assets(client=client, root=root, assets=assets)

        manifest = {
            'source_url': f'{GAMEKEE_BASE_URL}/{GAME_ALIAS}/tj/{content_id}.html',
            'detail_api_url': f'{GAMEKEE_BASE_URL}/v1/content/detail/{content_id}',
            'fetched_at': _utc_now_iso(),
            'content_id': content_id,
            'title': name,
            'entry_id': detail.get('entry_id'),
            'updated_at': detail.get('updated_at'),
            'tree_row': tree_row,
            'base_info': base_info,
            'content_summary': content_summary,
            'costumes': content_summary.get('costumes', []),
            'live2d_models': live2d_models,
            'assets': relative_manifest_assets(assets),
            'asset_counts': asset_counts(assets),
        }
        write_json(root / 'manifest.json', manifest)
        write_json(
            root / 'character.json',
            {
                'content_id': content_id,
                'title': name,
                'entry_id': detail.get('entry_id'),
                'tree_row': tree_row,
                'base_info': base_info,
                'costumes': content_summary.get('costumes', []),
                'live2d_models': live2d_models,
            },
        )
        if not skip_assets:
            await self._replace_page_assets_and_mark_completed(content_id=content_id, assets=assets)
        return root

    async def download_character(self, target: str, *, skip_assets: bool = False) -> Path:
        content_id = parse_content_id(target)
        async with database.advisory_lock('bd2') as acquired:
            if not acquired:
                msg = 'BD2 character download skipped because another run holds the advisory lock'
                raise RuntimeError(msg)

            await self._ensure_schema()
            self.path.mkdir(parents=True, exist_ok=True)
            async with self._http_client() as client:
                rows = await self._fetch_tree_rows(client)
                row = next((item for item in rows if row_content_id(item) == content_id), {'content_id': content_id})
                await self._upsert_tree_pages(rows or [row], deactivate_missing=False)
                return await self._crawl_page(client=client, tree_row=row, content_id=content_id, skip_assets=skip_assets)

    async def update(self) -> None:
        async with database.advisory_lock('bd2') as acquired:
            if not acquired:
                log.info('BD2 update skipped because another run holds the advisory lock')
                return

            await self._ensure_schema()
            self.path.mkdir(parents=True, exist_ok=True)
            async with self._http_client() as client:
                rows = await self._fetch_tree_rows(client)
                if not rows:
                    log.warning('GameKee BD2 tree returned no character rows')
                    return

                await self._upsert_tree_pages(rows, deactivate_missing=True)
                total = len(rows)
                log.info('Found %d GameKee BD2 character pages', total)
                failed_content_ids: list[int] = []
                for index, row in enumerate(rows, start=1):
                    content_id = row_content_id(row)
                    if content_id is None:
                        continue
                    try:
                        log.info('Crawling BD2 %s (%d/%d)', content_id, index, total)
                        await self._crawl_page(client=client, tree_row=row, content_id=content_id)
                    except Exception:
                        log.exception('Failed to crawl BD2 content_id=%s', content_id)
                        failed_content_ids.append(content_id)
                if failed_content_ids:
                    examples = ', '.join(str(content_id) for content_id in failed_content_ids[:5])
                    msg = f'{len(failed_content_ids)} BD2 pages failed'
                    if examples:
                        msg = f'{msg}: {examples}'
                    raise CrawlRunError(msg)
