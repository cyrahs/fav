from __future__ import annotations

import math
import re
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any, Self
from urllib.parse import urljoin

from src.web.nikke_layer_metadata import live2d_layer_fingerprint

GAMEKEE_BASE_URL = 'https://www.gamekee.com'
DEFAULT_RUNTIME_TIMEOUT_MS = 60_000
DEFAULT_RUNTIME_VIEWPORT = {'width': 1440, 'height': 1000}
LAYER_CAPTURE_SCHEMA = 1
LAYER_CAPTURE_MATCH_METHOD = 'gamekee-runtime-container'
LIVE2D_KEY_RE = re.compile(r'/live2d/[^/]+/([^/?#]+)/')


class RuntimeCaptureDependencyError(RuntimeError):
    pass


def _utc_now_iso() -> str:
    return datetime.now(UTC).isoformat()


def _to_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float) and math.isfinite(value) and value.is_integer():
        return int(value)
    if isinstance(value, str):
        match = re.match(r'^[+-]?\d+', value.strip())
        if match:
            return int(match.group(0))
    return None


def _to_float(value: Any) -> float | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int | float) and math.isfinite(value):
        return float(value)
    if isinstance(value, str):
        try:
            parsed = float(value.strip())
        except ValueError:
            return None
        return parsed if math.isfinite(parsed) else None
    return None


def _first_text(data: Mapping[str, Any], *keys: str) -> str:
    for key in keys:
        value = data.get(key)
        if value is None:
            continue
        text = str(value).strip()
        if text:
            return text
    return ''


def _normalize_url(url: str) -> str:
    value = url.strip()
    if not value:
        return ''
    if value.startswith('//'):
        return f'https:{value}'
    if value.startswith('/'):
        return urljoin(GAMEKEE_BASE_URL, value)
    return value


def _unique_in_order(values: Iterable[str]) -> list[str]:
    seen: set[str] = set()
    out: list[str] = []
    for raw_value in values:
        value = str(raw_value or '').strip()
        if not value or value in seen:
            continue
        seen.add(value)
        out.append(value)
    return out


def live2d_key_from_url(url: str) -> str:
    match = LIVE2D_KEY_RE.search(url)
    return match.group(1) if match else ''


def unique_live2d_keys(urls: Iterable[str]) -> list[str]:
    return _unique_in_order(live2d_key_from_url(url) for url in urls)


@dataclass(frozen=True, slots=True)
class RuntimeContainerSnapshot:
    index: int
    z_index: int | None
    raw_z_index: str = ''
    class_name: str = ''
    parent_class_name: str = ''
    canvas_count: int = 0
    rect: dict[str, float] = field(default_factory=dict)

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any], *, fallback_index: int) -> Self:
        raw_z_index = _first_text(raw, 'raw_z_index', 'rawZIndex')
        z_index = _to_int(raw.get('z_index', raw.get('zIndex')))
        if z_index is None:
            z_index = _to_int(raw_z_index)

        rect: dict[str, float] = {}
        raw_rect = raw.get('rect')
        if isinstance(raw_rect, Mapping):
            for key in ('x', 'y', 'width', 'height'):
                value = _to_float(raw_rect.get(key))
                if value is not None:
                    rect[key] = value

        return cls(
            index=_to_int(raw.get('index')) if _to_int(raw.get('index')) is not None else fallback_index,
            z_index=z_index,
            raw_z_index=raw_z_index,
            class_name=_first_text(raw, 'class_name', 'className'),
            parent_class_name=_first_text(raw, 'parent_class_name', 'parentClassName'),
            canvas_count=_to_int(raw.get('canvas_count', raw.get('canvasCount'))) or 0,
            rect=rect,
        )

    def to_raw_container(self) -> dict[str, Any]:
        return {
            'index': self.index,
            'zIndex': self.z_index,
            'rawZIndex': self.raw_z_index,
            'className': self.class_name,
            'parentClassName': self.parent_class_name,
            'canvasCount': self.canvas_count,
            'rect': dict(self.rect),
        }


@dataclass(frozen=True, slots=True)
class LayerCaptureBuildInput:
    content_id: int
    title: str = ''
    models: Sequence[Mapping[str, Any]] = ()
    containers: Sequence[Mapping[str, Any] | RuntimeContainerSnapshot] = ()
    requested_live2d_keys: Sequence[str] = ()
    captured_at: str = ''
    source_url: str = ''


@dataclass(frozen=True, slots=True)
class RuntimeCaptureRequest:
    content_id: int
    models: Sequence[Mapping[str, Any]]
    title: str = ''
    timeout_ms: int = DEFAULT_RUNTIME_TIMEOUT_MS
    headless: bool = True
    source_url: str = ''
    expected_layer_count: int | None = None


@dataclass(frozen=True, slots=True)
class RuntimeGroupCapture:
    models: Sequence[Mapping[str, Any]]
    containers: Sequence[RuntimeContainerSnapshot]
    requested_live2d_keys: Sequence[str]


def _model_live2d_key(model: Mapping[str, Any]) -> str:
    return _first_text(model, 'live2d_key', 'live2dKey')


def _model_stable_id(model: Mapping[str, Any]) -> str:
    return _first_text(model, 'stable_id', 'stableId')


def _model_skin_index(model: Mapping[str, Any]) -> int | None:
    return _to_int(model.get('skin_index', model.get('skinIndex')))


def _model_skin_title(model: Mapping[str, Any]) -> str:
    return _first_text(model, 'skin_title', 'skinTitle', 'skin_name', 'skinName')


def _model_resource_urls(model: Mapping[str, Any]) -> dict[str, Any]:
    raw_urls = model.get('resource_urls', model.get('resourceUrls'))
    if raw_urls is None:
        raw_urls = model.get('urls')
    if not isinstance(raw_urls, Mapping):
        return {}

    urls: dict[str, Any] = {}
    for key, value in raw_urls.items():
        field_name = str(key)
        if isinstance(value, str):
            normalized = _normalize_url(value)
            if normalized:
                urls[field_name] = normalized
        elif isinstance(value, Sequence) and not isinstance(value, str | bytes | bytearray):
            normalized_values = [_normalize_url(item) for item in value if isinstance(item, str)]
            urls[field_name] = [item for item in normalized_values if item]
    return urls


def layer_capture_fingerprint(content_id: int, models: Sequence[Mapping[str, Any]]) -> str:
    return live2d_layer_fingerprint(content_id, [dict(model) for model in models])


def _model_kind(model: Mapping[str, Any]) -> str:
    text_parts = [str(model.get('label') or ''), str(model.get('live2d_key') or ''), str(model.get('animation') or '')]
    urls = model.get('urls')
    if isinstance(urls, Mapping):
        for value in urls.values():
            if isinstance(value, str):
                text_parts.append(value)
            elif isinstance(value, Sequence) and not isinstance(value, str | bytes | bytearray):
                text_parts.extend(str(item) for item in value)
    normalized = ' '.join(text_parts).lower()
    has_aim = 'aim' in normalized
    has_cover = 'cover' in normalized
    if has_cover and not has_aim:
        return 'cover'
    if has_aim and not has_cover:
        return 'aim'
    return 'full'


def _group_models_by_skin_and_kind(models: Sequence[Mapping[str, Any]]) -> list[list[Mapping[str, Any]]]:
    groups: dict[tuple[int | None, str], list[Mapping[str, Any]]] = {}
    for model in models:
        groups.setdefault((_model_skin_index(model), _model_kind(model)), []).append(model)
    return list(groups.values())


def multi_full_model_groups(models: Sequence[Mapping[str, Any]]) -> list[list[Mapping[str, Any]]]:
    groups = [group for group in _group_models_by_skin_and_kind(models) if group and _model_kind(group[0]) == 'full' and len(group) > 1]
    return sorted(groups, key=lambda group: -1 if _model_skin_index(group[0]) is None else _model_skin_index(group[0]))


def first_skin_model_group(models: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    if not models:
        return []
    full_groups = [group for group in _group_models_by_skin_and_kind(models) if group and _model_kind(group[0]) == 'full']
    if not full_groups:
        return []
    return sorted(full_groups, key=lambda group: -1 if _model_skin_index(group[0]) is None else _model_skin_index(group[0]))[0]


def select_model_group(models: Sequence[Mapping[str, Any]], runtime_keys: Sequence[str]) -> list[Mapping[str, Any]]:
    if not models:
        return []

    runtime_key_set = set(runtime_keys)
    groups = [group for group in _group_models_by_skin_and_kind(models) if group and _model_kind(group[0]) == 'full']
    candidates = [
        group
        for group in groups
        if all(runtime_key and any(_model_live2d_key(model) == runtime_key for model in group) for runtime_key in runtime_keys)
    ]
    if len(candidates) == 1:
        return candidates[0]

    exact = []
    for group in groups:
        group_keys = {_model_live2d_key(model) for model in group if _model_live2d_key(model)}
        if group_keys and group_keys == runtime_key_set:
            exact.append(group)
    if len(exact) == 1:
        return exact[0]

    return list(models)


def _normalize_runtime_containers(
    containers: Sequence[Mapping[str, Any] | RuntimeContainerSnapshot],
) -> list[RuntimeContainerSnapshot]:
    out: list[RuntimeContainerSnapshot] = []
    for index, container in enumerate(containers):
        if isinstance(container, RuntimeContainerSnapshot):
            out.append(container)
        elif isinstance(container, Mapping):
            out.append(RuntimeContainerSnapshot.from_raw(container, fallback_index=index))
    return out


def _build_multi_group_layer_capture_payload(
    *,
    request: RuntimeCaptureRequest,
    group_captures: Sequence[RuntimeGroupCapture],
) -> dict[str, Any]:
    captured_at = _utc_now_iso()
    source_url = request.source_url or f'{GAMEKEE_BASE_URL}/nikke/tj/{request.content_id}.html'
    layers: list[dict[str, Any]] = []
    warnings: list[str] = []
    requested_live2d_keys: list[str] = []
    matched_live2d_keys: list[str] = []
    captured_models: list[Mapping[str, Any]] = []
    container_count = 0

    for group_capture in group_captures:
        payload = build_layer_capture_payload(
            LayerCaptureBuildInput(
                content_id=request.content_id,
                title=request.title,
                models=group_capture.models,
                containers=group_capture.containers,
                requested_live2d_keys=group_capture.requested_live2d_keys,
                captured_at=captured_at,
                source_url=source_url,
            ),
        )
        layers.extend(payload['layers'])
        warnings.extend(str(warning) for warning in payload.get('warnings', []))
        runtime = payload.get('runtime')
        if isinstance(runtime, Mapping):
            requested_live2d_keys.extend(str(key) for key in runtime.get('requested_live2d_keys', []) if key)
            matched_live2d_keys.extend(str(key) for key in runtime.get('matched_live2d_keys', []) if key)
            container_count += _to_int(runtime.get('container_count')) or 0
        captured_models.extend(group_capture.models)

    return {
        'content_id': request.content_id,
        'source_url': source_url,
        'title': request.title,
        'captured_at': captured_at,
        'status': 'success',
        'layer_capture_schema': LAYER_CAPTURE_SCHEMA,
        'runtime': {
            'requested_live2d_keys': _unique_in_order(requested_live2d_keys),
            'matched_live2d_keys': _unique_in_order(matched_live2d_keys),
            'container_count': container_count,
        },
        'fingerprint': layer_capture_fingerprint(request.content_id, captured_models),
        'warnings': _unique_in_order(warnings),
        'layers': layers,
    }


def build_layer_capture_payload(capture: LayerCaptureBuildInput) -> dict[str, Any]:
    captured_at = capture.captured_at or _utc_now_iso()
    source_url = capture.source_url or f'{GAMEKEE_BASE_URL}/nikke/tj/{capture.content_id}.html'
    containers = _normalize_runtime_containers(capture.containers)
    requested_live2d_keys = _unique_in_order(capture.requested_live2d_keys)
    model_keys = {_model_live2d_key(model) for model in capture.models if _model_live2d_key(model)}
    runtime_model_keys = [key for key in requested_live2d_keys if key in model_keys]
    model_group = select_model_group(capture.models, runtime_model_keys)
    model_by_runtime_key = {_model_live2d_key(model): model for model in model_group if _model_live2d_key(model)}
    matched_runtime_keys = [key for key in requested_live2d_keys if key in model_by_runtime_key]
    warnings: list[str] = []
    if len(matched_runtime_keys) < len(containers):
        warnings.append('fewer runtime live2d keys than visible containers')
    if len(containers) != len(model_group):
        warnings.append(f'visible container count {len(containers)} differs from full model count {len(model_group)}')

    layers: list[dict[str, Any]] = []
    for source_layer_index, container in enumerate(containers):
        runtime_key = matched_runtime_keys[source_layer_index] if source_layer_index < len(matched_runtime_keys) else ''
        model = model_by_runtime_key.get(runtime_key)
        if model is None and source_layer_index < len(model_group):
            model = model_group[source_layer_index]
        live2d_key = _model_live2d_key(model) if model is not None else runtime_key
        z_index = container.z_index
        has_high_confidence_match = model is not None and bool(runtime_key) and live2d_key == runtime_key and z_index is not None
        layer: dict[str, Any] = {
            'content_id': capture.content_id,
            'skin_index': _model_skin_index(model) if model is not None else None,
            'stable_id': _model_stable_id(model) if model is not None else '',
            'live2d_key': live2d_key,
            'resource_urls': _model_resource_urls(model) if model is not None else {},
            'source_layer_index': source_layer_index,
            'source_z_index': z_index,
            'is_primary': source_layer_index == 0,
            'layer_match_method': LAYER_CAPTURE_MATCH_METHOD,
            'layer_match_confidence': 'high' if has_high_confidence_match else 'low',
            'captured_at': captured_at,
            'raw_container': container.to_raw_container(),
        }
        layers.append(layer)

    ordered_layers = sorted(
        (layer for layer in layers if _to_int(layer.get('source_z_index')) is not None),
        key=lambda layer: (_to_int(layer.get('source_z_index')) or 0, _to_int(layer.get('source_layer_index')) or 0),
    )
    for layer_order, layer in enumerate(ordered_layers, start=1):
        layer['layer_order'] = layer_order

    return {
        'content_id': capture.content_id,
        'source_url': source_url,
        'title': capture.title,
        'captured_at': captured_at,
        'status': 'success',
        'layer_capture_schema': LAYER_CAPTURE_SCHEMA,
        'runtime': {
            'requested_live2d_keys': requested_live2d_keys,
            'matched_live2d_keys': matched_runtime_keys,
            'container_count': len(containers),
        },
        'fingerprint': layer_capture_fingerprint(capture.content_id, model_group),
        'warnings': warnings,
        'layers': layers,
    }


async def _select_runtime_skin(page: Any, skin_title: str) -> None:
    if not skin_title or skin_title == '基础时装':
        return

    await page.locator('button.action-item[data-report-key*="clothes"]').click(timeout=10_000)
    await page.get_by_text(skin_title, exact=True).last.click(timeout=10_000)
    await page.wait_for_timeout(500)


async def _read_runtime_containers(page: Any) -> list[RuntimeContainerSnapshot]:
    raw_containers = await page.evaluate(
        """() => [...document.querySelectorAll(".spine-player-container")].map((element, index) => {
            const rect = element.getBoundingClientRect();
            const style = getComputedStyle(element);
            return {
                index,
                zIndex: Number.parseInt(style.zIndex, 10),
                rawZIndex: style.zIndex,
                className: element.className,
                parentClassName: element.parentElement?.className ?? "",
                canvasCount: element.querySelectorAll("canvas").length,
                rect: {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                },
            };
        })""",
    )
    if not isinstance(raw_containers, list):
        raw_containers = []
    return [
        RuntimeContainerSnapshot.from_raw(item, fallback_index=index)
        for index, item in enumerate(raw_containers)
        if isinstance(item, Mapping)
    ]


async def _capture_runtime_containers(request: RuntimeCaptureRequest) -> tuple[list[RuntimeContainerSnapshot], list[str]]:
    captures = await _capture_runtime_groups(request)
    if not captures:
        return [], []
    first_capture = captures[0]
    return list(first_capture.containers), list(first_capture.requested_live2d_keys)


async def _capture_runtime_groups(request: RuntimeCaptureRequest) -> list[RuntimeGroupCapture]:
    try:
        from playwright.async_api import async_playwright  # noqa: PLC0415
    except ImportError as exc:
        msg = 'Install Playwright and a Chromium browser before running Nikke runtime capture.'
        raise RuntimeCaptureDependencyError(msg) from exc

    requested_urls: list[str] = []
    source_url = request.source_url or f'{GAMEKEE_BASE_URL}/nikke/tj/{request.content_id}.html'
    model_groups = multi_full_model_groups(request.models) or [first_skin_model_group(request.models)]
    model_groups = [group for group in model_groups if group]

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch(headless=request.headless)
        try:
            page = await browser.new_page(viewport=DEFAULT_RUNTIME_VIEWPORT)
            page.on('request', lambda browser_request: requested_urls.append(browser_request.url))
            await page.goto(source_url, wait_until='domcontentloaded', timeout=request.timeout_ms)
            captures: list[RuntimeGroupCapture] = []
            for group_index, group in enumerate(model_groups):
                skin_title = _model_skin_title(group[0])
                is_initial_group = group_index == 0 and (not skin_title or skin_title == '基础时装')
                start_request_index = 0 if is_initial_group else len(requested_urls)
                await _select_runtime_skin(page, skin_title)
                expected_layer_count = request.expected_layer_count if request.expected_layer_count is not None else len(group)
                await page.wait_for_function(
                    'count => document.querySelectorAll(".spine-player-container canvas").length >= count',
                    arg=expected_layer_count,
                    timeout=request.timeout_ms,
                )
                await page.wait_for_timeout(1500)
                captures.append(
                    RuntimeGroupCapture(
                        models=group,
                        containers=await _read_runtime_containers(page),
                        requested_live2d_keys=unique_live2d_keys(requested_urls[start_request_index:]),
                    ),
                )
        finally:
            await browser.close()

    return captures


async def capture_gamekee_runtime_layers(request: RuntimeCaptureRequest) -> dict[str, Any]:
    group_captures = await _capture_runtime_groups(request)
    if len(group_captures) > 1:
        return _build_multi_group_layer_capture_payload(request=request, group_captures=group_captures)
    containers = list(group_captures[0].containers) if group_captures else []
    requested_live2d_keys = list(group_captures[0].requested_live2d_keys) if group_captures else []
    models = group_captures[0].models if group_captures else request.models
    return build_layer_capture_payload(
        LayerCaptureBuildInput(
            content_id=request.content_id,
            title=request.title,
            models=models,
            containers=containers,
            requested_live2d_keys=requested_live2d_keys,
            source_url=request.source_url,
        ),
    )
