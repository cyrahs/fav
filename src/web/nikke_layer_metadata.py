from __future__ import annotations

import hashlib
import json
import os
import secrets
from contextlib import suppress
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.parse import urljoin

GAMEKEE_BASE_URL = 'https://www.gamekee.com'
LAYER_MATCH_CONFIDENCE_VALUES = {'high', 'medium', 'low'}
LIVE2D_LAYER_METADATA_FIELDS = (
    'layer_order',
    'source_z_index',
    'source_layer_index',
    'is_primary',
    'layer_match_method',
    'layer_match_confidence',
)
LIVE2D_LAYER_CAPTURE_FIELD = 'live2d_layer_capture'
LAYER_CAPTURE_MATCH_METHOD = 'gamekee-runtime-container'
MIN_LAYER_GROUP_SIZE = 2
LAYER_CAPTURE_RAW_PATH = Path('raw/live2d-layer-capture.json')
MAX_SUMMARY_TEXT_LENGTH = 240
MAX_SUMMARY_WARNING_COUNT = 20
LayerChangePlan = tuple[int, dict[str, Any], dict[str, Any], bool]


@dataclass(slots=True)
class LayerMergePlan:
    report: dict[str, Any]
    live2d_models: list[dict[str, Any]]
    projected_models: list[dict[str, Any]]
    planned_changes: list[LayerChangePlan]
    blocked_group_keys: set[tuple[int | None, str]]
    dry_run: bool


@dataclass(frozen=True, slots=True)
class PreviousLayerCapture:
    payload: dict[str, Any]
    source: str


def _json_dumps(data: Any) -> str:
    return json.dumps(data, ensure_ascii=False, sort_keys=True, separators=(',', ':'), default=str)


def _pretty_json(data: Any) -> str:
    return json.dumps(data, ensure_ascii=False, indent=2, sort_keys=True, default=str) + '\n'


def _to_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    return None


def _optional_bool(value: Any) -> bool | None:
    return value if isinstance(value, bool) else None


def _first_int(data: dict[str, Any], *keys: str) -> int | None:
    for key in keys:
        value = _to_int(data.get(key))
        if value is not None:
            return value
    return None


def _first_text(data: dict[str, Any], *keys: str) -> str:
    for key in keys:
        value = data.get(key)
        if value is None:
            continue
        text = str(value).strip()
        if text:
            return text
    return ''


def normalize_layer_url(url: str) -> str:
    value = url.strip()
    if not value:
        return ''
    if value.startswith('//'):
        return f'https:{value}'
    if value.startswith('/'):
        return urljoin(GAMEKEE_BASE_URL, value)
    return value


def copy_live2d_layer_metadata(model: dict[str, Any], value: dict[str, Any]) -> None:
    layer_order = _first_int(value, 'layer_order', 'layerOrder')
    source_z_index = _first_int(value, 'source_z_index', 'sourceZIndex')
    source_layer_index = _first_int(value, 'source_layer_index', 'sourceLayerIndex')
    is_primary = _optional_bool(value.get('is_primary')) if 'is_primary' in value else _optional_bool(value.get('isPrimary'))
    layer_match_method = _first_text(value, 'layer_match_method', 'layerMatchMethod')
    layer_match_confidence = _first_text(value, 'layer_match_confidence', 'layerMatchConfidence')

    if layer_order is not None:
        model['layer_order'] = layer_order
    if source_z_index is not None:
        model['source_z_index'] = source_z_index
    if source_layer_index is not None:
        model['source_layer_index'] = source_layer_index
    if is_primary is not None:
        model['is_primary'] = is_primary
    if layer_match_method:
        model['layer_match_method'] = layer_match_method
    if layer_match_confidence in LAYER_MATCH_CONFIDENCE_VALUES:
        model['layer_match_confidence'] = layer_match_confidence


def _model_identity(model: dict[str, Any]) -> str:
    stable_id = str(model.get('stable_id') or '')
    live2d_key = str(model.get('live2d_key') or '')
    if stable_id and live2d_key:
        return f'{stable_id}/{live2d_key}'
    return stable_id or live2d_key or '<unknown>'


def _model_urls(model: dict[str, Any]) -> set[str]:
    urls = model.get('urls')
    if not isinstance(urls, dict):
        return set()

    out: set[str] = set()
    for value in urls.values():
        if isinstance(value, str) and value.strip():
            out.add(normalize_layer_url(value))
        elif isinstance(value, list):
            out.update(normalize_layer_url(item) for item in value if isinstance(item, str) and item.strip())
    return {url for url in out if url}


def _capture_resource_urls(capture: dict[str, Any]) -> set[str]:
    raw_urls = capture.get('resource_urls')
    if raw_urls is None:
        raw_urls = capture.get('urls')

    values: list[Any] = []
    if isinstance(raw_urls, dict):
        values.extend(raw_urls.values())
    elif isinstance(raw_urls, list):
        values.extend(raw_urls)

    out: set[str] = set()
    for value in values:
        if isinstance(value, str) and value.strip():
            out.add(normalize_layer_url(value))
        elif isinstance(value, list):
            out.update(normalize_layer_url(item) for item in value if isinstance(item, str) and item.strip())
    return {url for url in out if url}


def _fingerprint_resource_urls(model: dict[str, Any]) -> dict[str, Any]:
    urls = model.get('urls')
    if not isinstance(urls, dict):
        return {}

    out: dict[str, Any] = {}
    for field_name in ('atlas', 'skel', 'json', 'image'):
        value = urls.get(field_name)
        if isinstance(value, str) and value.strip():
            out[field_name] = normalize_layer_url(value)
        elif isinstance(value, list):
            normalized = sorted(normalize_layer_url(item) for item in value if isinstance(item, str) and item.strip())
            if normalized:
                out[field_name] = normalized
    return out


def live2d_layer_fingerprint(content_id: int, live2d_models: list[dict[str, Any]]) -> str:
    entries = [
        {
            'skin_index': _to_int(model.get('skin_index')),
            'stable_id': str(model.get('stable_id') or ''),
            'live2d_key': str(model.get('live2d_key') or ''),
            'resource_urls': _fingerprint_resource_urls(model),
        }
        for model in live2d_models
    ]
    entries.sort(key=lambda item: (item['skin_index'] or -1, item['stable_id'], item['live2d_key'], _json_dumps(item['resource_urls'])))
    payload = {
        'content_id': content_id,
        'models': entries,
    }
    return hashlib.sha256(_json_dumps(payload).encode('utf-8')).hexdigest()


def _multi_full_model_groups(live2d_models: list[dict[str, Any]]) -> list[list[dict[str, Any]]]:
    groups: dict[tuple[int | None, str], list[dict[str, Any]]] = {}
    for model in live2d_models:
        groups.setdefault(_layer_group_key(model), []).append(model)
    return [models for (_skin_index, model_kind), models in groups.items() if model_kind == 'full' and len(models) > 1]


def live2d_layer_fingerprint_candidates(content_id: int, live2d_models: list[dict[str, Any]]) -> list[str]:
    multi_full_groups = _multi_full_model_groups(live2d_models)
    fingerprints = [live2d_layer_fingerprint(content_id, live2d_models)]
    if multi_full_groups:
        fingerprints.append(live2d_layer_fingerprint(content_id, [model for group in multi_full_groups for model in group]))
    fingerprints.extend(live2d_layer_fingerprint(content_id, group) for group in multi_full_groups)
    return list(dict.fromkeys(fingerprints))


def _layer_group_payload(group_key: tuple[int | None, str]) -> dict[str, Any]:
    skin_index, model_kind = group_key
    return {'skin_index': skin_index, 'model_kind': model_kind}


def layer_capture_group_coverage(
    *,
    content_id: int,
    live2d_models: list[dict[str, Any]],
    capture_payload: dict[str, Any],
) -> dict[str, Any]:
    required_group_keys = {_layer_group_key(model) for group in _multi_full_model_groups(live2d_models) for model in group}
    covered_group_keys: set[tuple[int | None, str]] = set()
    unmatched_layers: list[dict[str, Any]] = []

    for layer in _normalized_layer_capture(capture_payload):
        layer_content_id = _to_int(layer.get('content_id'))
        if layer_content_id is not None and layer_content_id != content_id:
            unmatched_layers.append(
                {
                    'reason': 'content_id_mismatch',
                    'capture_content_id': layer_content_id,
                    'expected_content_id': content_id,
                    'stable_id': layer.get('stable_id') or '',
                    'live2d_key': layer.get('live2d_key') or '',
                },
            )
            continue

        candidates = [model for model in live2d_models if _capture_matches_model(layer, model)]
        if len(candidates) != 1:
            unmatched_layers.append(
                {
                    'reason': 'ambiguous_match' if candidates else 'no_match',
                    'candidate_count': len(candidates),
                    'stable_id': layer.get('stable_id') or '',
                    'live2d_key': layer.get('live2d_key') or '',
                },
            )
            continue

        group_key = _layer_group_key(candidates[0])
        if group_key in required_group_keys:
            covered_group_keys.add(group_key)

    missing_group_keys = required_group_keys - covered_group_keys
    return {
        'required_groups': [_layer_group_payload(group_key) for group_key in sorted(required_group_keys, key=str)],
        'covered_groups': [_layer_group_payload(group_key) for group_key in sorted(covered_group_keys, key=str)],
        'missing_groups': [_layer_group_payload(group_key) for group_key in sorted(missing_group_keys, key=str)],
        'unmatched_layers': unmatched_layers,
        'complete': not missing_group_keys,
    }


def _normalized_layer_capture(payload: dict[str, Any]) -> list[dict[str, Any]]:
    raw_layers = payload.get('layers', payload.get('captures'))
    if not isinstance(raw_layers, list):
        return []

    content_id = _to_int(payload.get('content_id'))
    captured_at = _first_text(payload, 'captured_at', 'capturedAt')
    out: list[dict[str, Any]] = []
    for raw_layer in raw_layers:
        if not isinstance(raw_layer, dict):
            continue
        layer = dict(raw_layer)
        if content_id is not None and _to_int(layer.get('content_id')) is None:
            layer['content_id'] = content_id
        if captured_at and not _first_text(layer, 'captured_at', 'capturedAt'):
            layer['captured_at'] = captured_at

        layer_order = _first_int(layer, 'layer_order', 'layerOrder')
        source_z_index = _first_int(layer, 'source_z_index', 'sourceZIndex')
        source_layer_index = _first_int(layer, 'source_layer_index', 'sourceLayerIndex')
        skin_index = _first_int(layer, 'skin_index', 'skinIndex')
        is_primary = _optional_bool(layer.get('is_primary')) if 'is_primary' in layer else _optional_bool(layer.get('isPrimary'))
        confidence = _first_text(layer, 'layer_match_confidence', 'layerMatchConfidence')
        method = _first_text(layer, 'layer_match_method', 'layerMatchMethod') or LAYER_CAPTURE_MATCH_METHOD

        normalized: dict[str, Any] = {
            'content_id': _to_int(layer.get('content_id')),
            'skin_index': skin_index,
            'stable_id': _first_text(layer, 'stable_id', 'stableId'),
            'live2d_key': _first_text(layer, 'live2d_key', 'live2dKey'),
            'layer_order': layer_order,
            'source_z_index': source_z_index,
            'source_layer_index': source_layer_index,
            'is_primary': is_primary,
            'layer_match_method': method,
            'layer_match_confidence': confidence if confidence in LAYER_MATCH_CONFIDENCE_VALUES else 'low',
            'captured_at': _first_text(layer, 'captured_at', 'capturedAt'),
            'resource_urls': sorted(_capture_resource_urls(layer)),
        }
        raw_container = layer.get('raw_container', layer.get('rawContainer'))
        if isinstance(raw_container, dict):
            normalized['raw_container'] = raw_container
        out.append(normalized)

    _fill_missing_capture_layer_orders(out)
    return out


def _fill_missing_capture_layer_orders(layers: list[dict[str, Any]]) -> None:
    grouped: dict[tuple[int | None, int | None], list[dict[str, Any]]] = {}
    for layer in layers:
        if _to_int(layer.get('layer_order')) is not None:
            continue
        if _to_int(layer.get('source_z_index')) is None:
            continue
        grouped.setdefault((_to_int(layer.get('content_id')), _to_int(layer.get('skin_index'))), []).append(layer)

    for group in grouped.values():
        if len(group) < MIN_LAYER_GROUP_SIZE:
            continue
        for order, layer in enumerate(
            sorted(
                group,
                key=lambda item: (
                    _to_int(item.get('source_z_index')) or 0,
                    _to_int(item.get('source_layer_index')) or 0,
                    str(item.get('stable_id') or ''),
                    str(item.get('live2d_key') or ''),
                ),
            ),
            start=1,
        ):
            layer['layer_order'] = order


def _layer_capture_updates(capture: dict[str, Any]) -> dict[str, Any]:
    updates: dict[str, Any] = {}
    for field_name in LIVE2D_LAYER_METADATA_FIELDS:
        value = capture.get(field_name)
        if field_name in {'layer_order', 'source_z_index', 'source_layer_index'}:
            value = _to_int(value)
        elif field_name == 'is_primary':
            value = _optional_bool(value)
        elif field_name == 'layer_match_confidence':
            value = value if value in LAYER_MATCH_CONFIDENCE_VALUES else None
        elif isinstance(value, str):
            value = value.strip()
        else:
            value = None
        if value is not None and value != '':
            updates[field_name] = value
    return updates


def _capture_matches_model(capture: dict[str, Any], model: dict[str, Any]) -> bool:
    stable_id = str(capture.get('stable_id') or '')
    live2d_key = str(capture.get('live2d_key') or '')
    if stable_id and str(model.get('stable_id') or '') != stable_id:
        return False
    if live2d_key and str(model.get('live2d_key') or '') != live2d_key:
        return False
    if stable_id or live2d_key:
        return True

    capture_urls = set(capture.get('resource_urls') or [])
    return bool(capture_urls and capture_urls.intersection(_model_urls(model)))


def _layer_group_key(model: dict[str, Any]) -> tuple[int | None, str]:
    return (_to_int(model.get('skin_index')), _infer_live2d_model_kind(model))


def has_multi_full_layer_groups(live2d_models: list[dict[str, Any]]) -> bool:
    return bool(_multi_full_model_groups(live2d_models))


def _blocked_layer_group_keys(quality_issues: list[dict[str, Any]]) -> set[tuple[int | None, str]]:
    return {
        (_to_int(issue.get('skin_index')), str(issue.get('model_kind') or 'full'))
        for issue in quality_issues
        if issue.get('severity') == 'error'
    }


def _skipped_layer_group_keys(skipped: list[dict[str, Any]]) -> set[tuple[int | None, str]]:
    return {
        (_to_int(item.get('skin_index')), str(item.get('model_kind') or 'full'))
        for item in skipped
        if _to_int(item.get('skin_index')) is not None
    }


def _planned_layer_group_keys(
    projected_models: list[dict[str, Any]],
    planned_changes: list[LayerChangePlan],
) -> set[tuple[int | None, str]]:
    return {_layer_group_key(projected_models[model_index]) for model_index, _updates, _change, _has_changed_fields in planned_changes}


def _quality_issues_for_groups(
    quality_issues: list[dict[str, Any]],
    group_keys: set[tuple[int | None, str]],
) -> list[dict[str, Any]]:
    return [issue for issue in quality_issues if (_to_int(issue.get('skin_index')), str(issue.get('model_kind') or 'full')) in group_keys]


def _clear_live2d_layer_metadata(model: dict[str, Any]) -> None:
    for field_name in LIVE2D_LAYER_METADATA_FIELDS:
        model.pop(field_name, None)


def _missing_capture_layer_issues(
    projected_models: list[dict[str, Any]],
    planned_changes: list[LayerChangePlan],
) -> list[dict[str, Any]]:
    planned_model_indexes_by_group: dict[tuple[int | None, str], set[int]] = {}
    for model_index, _updates, _change, _has_changed_fields in planned_changes:
        planned_model_indexes_by_group.setdefault(_layer_group_key(projected_models[model_index]), set()).add(model_index)

    issues: list[dict[str, Any]] = []
    for group_key, planned_model_indexes in planned_model_indexes_by_group.items():
        skin_index, model_kind = group_key
        if model_kind != 'full':
            continue
        group_model_indexes = {index for index, model in enumerate(projected_models) if _layer_group_key(model) == group_key}
        if len(group_model_indexes) <= 1:
            continue
        missing_model_indexes = group_model_indexes - planned_model_indexes
        if missing_model_indexes:
            issues.append(
                {
                    'severity': 'error',
                    'code': 'missing_capture_layer',
                    'skin_index': skin_index,
                    'model_kind': model_kind,
                    'models': [_model_identity(projected_models[index]) for index in sorted(missing_model_indexes)],
                },
            )
    return issues


def _blocked_layer_change(projected_model: dict[str, Any], updates: dict[str, Any], change: dict[str, Any]) -> dict[str, Any]:
    return {
        'reason': 'quality_gates_failed',
        'model': change['model'],
        'stable_id': change['stable_id'],
        'live2d_key': change['live2d_key'],
        'skin_index': _to_int(projected_model.get('skin_index')),
        'model_kind': _infer_live2d_model_kind(projected_model),
        'fields': sorted(updates),
    }


def _clear_blocked_layer_groups(
    *,
    live2d_models: list[dict[str, Any]],
    projected_models: list[dict[str, Any]],
    blocked_group_keys: set[tuple[int | None, str]],
) -> None:
    for model_index, model in enumerate(projected_models):
        if _layer_group_key(model) in blocked_group_keys:
            _clear_live2d_layer_metadata(live2d_models[model_index])


def _apply_layer_change_plan(plan: LayerMergePlan) -> None:
    if not plan.dry_run:
        _clear_blocked_layer_groups(
            live2d_models=plan.live2d_models,
            projected_models=plan.projected_models,
            blocked_group_keys=plan.blocked_group_keys,
        )

    for model_index, updates, change, has_changed_fields in plan.planned_changes:
        projected_model = plan.projected_models[model_index]
        if _layer_group_key(projected_model) in plan.blocked_group_keys:
            plan.report['blocked'].append(_blocked_layer_change(projected_model, updates, change))
            continue

        plan.report['changes'].append(change)
        if has_changed_fields:
            plan.report['changed'] += 1
            if not plan.dry_run:
                plan.live2d_models[model_index].update(updates)
        else:
            plan.report['unchanged'] += 1


def merge_live2d_layer_captures(
    live2d_models: list[dict[str, Any]],
    capture_payload: dict[str, Any],
    *,
    content_id: int | None = None,
    dry_run: bool = True,
) -> dict[str, Any]:
    layers = _normalized_layer_capture(capture_payload)
    projected_models = [dict(model) for model in live2d_models]
    report: dict[str, Any] = {
        'content_id': content_id if content_id is not None else _to_int(capture_payload.get('content_id')),
        'dry_run': dry_run,
        'layer_count': len(layers),
        'matched': 0,
        'changed': 0,
        'unchanged': 0,
        'skipped': [],
        'blocked': [],
        'changes': [],
    }
    planned_changes: list[LayerChangePlan] = []
    for layer in layers:
        layer_content_id = _to_int(layer.get('content_id'))
        if content_id is not None and layer_content_id is not None and layer_content_id != content_id:
            report['skipped'].append(
                {
                    'reason': 'content_id_mismatch',
                    'capture_content_id': layer_content_id,
                    'expected_content_id': content_id,
                    'stable_id': layer.get('stable_id') or '',
                    'live2d_key': layer.get('live2d_key') or '',
                },
            )
            continue

        candidates = [(index, model) for index, model in enumerate(projected_models) if _capture_matches_model(layer, model)]
        if len(candidates) != 1:
            report['skipped'].append(
                {
                    'reason': 'ambiguous_match' if candidates else 'no_match',
                    'candidate_count': len(candidates),
                    'stable_id': layer.get('stable_id') or '',
                    'live2d_key': layer.get('live2d_key') or '',
                    'source_z_index': layer.get('source_z_index'),
                    'source_layer_index': layer.get('source_layer_index'),
                },
            )
            continue

        updates = _layer_capture_updates(layer)
        if not {'layer_order', 'is_primary'}.issubset(updates):
            model_index, model = candidates[0]
            report['skipped'].append(
                {
                    'reason': 'incomplete_layer_metadata',
                    'stable_id': layer.get('stable_id') or '',
                    'live2d_key': layer.get('live2d_key') or '',
                    'skin_index': _to_int(model.get('skin_index')),
                    'model_kind': _infer_live2d_model_kind(model),
                    'model_index': model_index,
                    'fields': sorted(updates),
                },
            )
            continue

        model_index, model = candidates[0]
        original_model = live2d_models[model_index]
        changed_fields = {key: value for key, value in updates.items() if original_model.get(key) != value}
        change = {
            'model': _model_identity(model),
            'stable_id': model.get('stable_id') or '',
            'live2d_key': model.get('live2d_key') or '',
            'fields': updates,
            'changed_fields': sorted(changed_fields),
        }
        report['matched'] += 1
        planned_changes.append((model_index, updates, change, bool(changed_fields)))
        model.update(updates)

    affected_group_keys = _planned_layer_group_keys(projected_models, planned_changes) | _skipped_layer_group_keys(report['skipped'])
    report['quality_issues'] = _quality_issues_for_groups(validate_live2d_layer_metadata(projected_models), affected_group_keys)
    report['quality_issues'].extend(_missing_capture_layer_issues(projected_models, planned_changes))
    blocked_group_keys = _blocked_layer_group_keys(report['quality_issues']) | _skipped_layer_group_keys(report['skipped'])
    _apply_layer_change_plan(
        LayerMergePlan(
            report=report,
            live2d_models=live2d_models,
            projected_models=projected_models,
            planned_changes=planned_changes,
            blocked_group_keys=blocked_group_keys,
            dry_run=dry_run,
        ),
    )
    return report


def remove_live2d_layer_metadata(live2d_models: list[dict[str, Any]], *, dry_run: bool = True) -> dict[str, Any]:
    report: dict[str, Any] = {'dry_run': dry_run, 'changed': 0, 'changes': []}
    for model in live2d_models:
        present = [field_name for field_name in LIVE2D_LAYER_METADATA_FIELDS if field_name in model]
        if not present:
            continue
        report['changed'] += 1
        report['changes'].append({'model': _model_identity(model), 'removed_fields': present})
        if not dry_run:
            for field_name in present:
                model.pop(field_name, None)
    return report


def _infer_live2d_model_kind(model: dict[str, Any]) -> str:
    text_parts = [
        str(model.get('label') or ''),
        str(model.get('live2d_key') or ''),
        str(model.get('animation') or ''),
    ]
    urls = model.get('urls')
    if isinstance(urls, dict):
        for value in urls.values():
            if isinstance(value, str):
                text_parts.append(value)
            elif isinstance(value, list):
                text_parts.extend(str(item) for item in value)
    normalized = ' '.join(text_parts).lower()
    has_aim = 'aim' in normalized
    has_cover = 'cover' in normalized
    if has_cover and not has_aim:
        return 'cover'
    if has_aim and not has_cover:
        return 'aim'
    return 'full'


def validate_live2d_layer_metadata(live2d_models: list[dict[str, Any]]) -> list[dict[str, Any]]:
    issues: list[dict[str, Any]] = []
    grouped: dict[tuple[int | None, str], list[dict[str, Any]]] = {}
    for model in live2d_models:
        grouped.setdefault((_to_int(model.get('skin_index')), _infer_live2d_model_kind(model)), []).append(model)

    for (skin_index, model_kind), models in sorted(grouped.items(), key=lambda item: ((item[0][0] or -1), item[0][1])):
        if model_kind != 'full' or len(models) <= 1:
            continue
        identities = [_model_identity(model) for model in models]
        orders = [_to_int(model.get('layer_order')) for model in models]
        missing_order = [identities[index] for index, order in enumerate(orders) if order is None]
        if missing_order:
            issues.append(
                {
                    'severity': 'error',
                    'code': 'missing_layer_order',
                    'skin_index': skin_index,
                    'model_kind': model_kind,
                    'models': missing_order,
                },
            )
        else:
            compact_orders = [order for order in orders if order is not None]
            if len(set(compact_orders)) != len(compact_orders):
                issues.append(
                    {
                        'severity': 'error',
                        'code': 'duplicate_layer_order',
                        'skin_index': skin_index,
                        'model_kind': model_kind,
                        'orders': compact_orders,
                        'models': identities,
                    },
                )

        primary_models = [_model_identity(model) for model in models if model.get('is_primary') is True]
        if len(primary_models) != 1:
            issues.append(
                {
                    'severity': 'error',
                    'code': 'invalid_primary_count',
                    'skin_index': skin_index,
                    'model_kind': model_kind,
                    'primary_count': len(primary_models),
                    'models': identities,
                },
            )

        low_confidence = [_model_identity(model) for model in models if model.get('layer_match_confidence') != 'high']
        if low_confidence:
            issues.append(
                {
                    'severity': 'error',
                    'code': 'low_confidence_layer_match',
                    'skin_index': skin_index,
                    'model_kind': model_kind,
                    'models': low_confidence,
                },
            )
    return issues


def _read_json_object(path: Path) -> dict[str, Any]:
    data = json.loads(path.read_text(encoding='utf-8'))
    if not isinstance(data, dict):
        msg = f'{path} does not contain a JSON object'
        raise TypeError(msg)
    return data


def read_previous_layer_capture_artifact(root: Path) -> PreviousLayerCapture | None:
    raw_capture_path = root / LAYER_CAPTURE_RAW_PATH
    if raw_capture_path.exists():
        return PreviousLayerCapture(payload=_read_json_object(raw_capture_path), source='raw')

    return read_previous_layer_capture_manifest_summary(root)


def read_previous_layer_capture_manifest_summary(root: Path) -> PreviousLayerCapture | None:
    manifest_path = root / 'manifest.json'
    if not manifest_path.exists():
        return None

    manifest = _read_json_object(manifest_path)
    summary = manifest.get(LIVE2D_LAYER_CAPTURE_FIELD)
    return PreviousLayerCapture(payload=dict(summary), source='manifest_summary') if isinstance(summary, dict) else None


def read_previous_layer_capture(root: Path) -> dict[str, Any] | None:
    artifact = read_previous_layer_capture_artifact(root)
    return artifact.payload if artifact is not None else None


def _capture_payload_fingerprint(capture_payload: dict[str, Any]) -> str:
    return _first_text(capture_payload, 'fingerprint', 'captureFingerprint')


def _capture_payload_status(capture_payload: dict[str, Any]) -> str:
    return _first_text(capture_payload, 'status', 'captureStatus')


def _capture_payload_has_high_confidence(capture_payload: dict[str, Any]) -> bool:
    layers = _normalized_layer_capture(capture_payload)
    return bool(layers) and all(layer.get('layer_match_confidence') == 'high' for layer in layers)


def evaluate_layer_capture_reuse(
    *,
    content_id: int,
    live2d_models: list[dict[str, Any]],
    previous_capture: dict[str, Any] | None,
    force_refresh: bool = False,
) -> dict[str, Any]:
    current_fingerprints = live2d_layer_fingerprint_candidates(content_id, live2d_models)
    current_fingerprint = current_fingerprints[0]
    report: dict[str, Any] = {
        'content_id': content_id,
        'current_fingerprint': current_fingerprint,
        'current_fingerprints': current_fingerprints,
        'previous_fingerprint': '',
        'reusable': False,
        'action': 'refresh',
        'reason': '',
    }

    if force_refresh:
        reason = 'force_refresh'
    elif previous_capture is None:
        reason = 'no_previous_capture'
    else:
        previous_fingerprint = _capture_payload_fingerprint(previous_capture)
        report['previous_fingerprint'] = previous_fingerprint
        if previous_fingerprint not in current_fingerprints:
            reason = 'fingerprint_changed' if previous_fingerprint else 'missing_fingerprint'
        else:
            previous_status = _capture_payload_status(previous_capture)
            report['previous_status'] = previous_status
            if previous_status != 'success':
                reason = 'previous_status_not_success'
            elif not _capture_payload_has_high_confidence(previous_capture):
                reason = 'previous_confidence_not_high'
            else:
                group_coverage = layer_capture_group_coverage(
                    content_id=content_id,
                    live2d_models=live2d_models,
                    capture_payload=previous_capture,
                )
                report['group_coverage'] = group_coverage
                if not group_coverage['complete']:
                    reason = 'capture_missing_multi_full_groups'
                    report['missing_groups'] = group_coverage['missing_groups']
                    report['unmatched_layers'] = group_coverage['unmatched_layers']
                    report['reason'] = reason
                    return report

                merge_report = merge_live2d_layer_captures(live2d_models, previous_capture, content_id=content_id)
                report['merge_report'] = merge_report
                if merge_report.get('skipped') or merge_report.get('quality_issues'):
                    reason = 'quality_gates_failed'
                else:
                    reason = 'fingerprint_match'
                    report['reusable'] = True
                    report['action'] = 'reuse'

    report['reason'] = reason
    return report


def _write_json(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f'.{path.name}.tmp-{os.getpid()}-{secrets.token_hex(4)}')
    try:
        tmp.write_text(_pretty_json(data), encoding='utf-8')
        tmp.replace(path)
    finally:
        with suppress(FileNotFoundError):
            tmp.unlink()


def _summary_layers(layers: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [{key: value for key, value in layer.items() if key != 'raw_container'} for layer in layers]


def _safe_summary_text(value: Any, *, max_length: int = MAX_SUMMARY_TEXT_LENGTH) -> str:
    text = ' '.join(str(value).split())
    if not text:
        return ''
    tokens = ['<path>' if '/' in token or '\\' in token else token for token in text.split(' ')]
    return ' '.join(tokens)[:max_length]


def _runtime_summary(runtime: Any) -> dict[str, Any]:
    if not isinstance(runtime, dict):
        return {}

    summary: dict[str, Any] = {}
    for key in ('requested_live2d_keys', 'matched_live2d_keys'):
        value = runtime.get(key)
        if isinstance(value, list):
            keys = [_safe_summary_text(item, max_length=120) for item in value if isinstance(item, str | int)]
            summary[key] = [key for key in keys if key]

    container_count = _to_int(runtime.get('container_count'))
    if container_count is not None:
        summary['container_count'] = container_count
    return summary


def _warning_summary(warnings: Any) -> list[str]:
    if not isinstance(warnings, list):
        return []

    out: list[str] = []
    for warning in warnings:
        if not isinstance(warning, str | int):
            continue
        text = _safe_summary_text(warning)
        if text:
            out.append(text)
        if len(out) >= MAX_SUMMARY_WARNING_COUNT:
            break
    return out


def layer_capture_manifest_summary(capture_payload: dict[str, Any], merge_report: dict[str, Any]) -> dict[str, Any]:
    layers = _normalized_layer_capture(capture_payload)
    status = _first_text(capture_payload, 'status', 'captureStatus') or 'success'
    if merge_report.get('skipped') or merge_report.get('blocked') or merge_report.get('quality_issues'):
        status = 'incomplete'
    return {
        'schema': _to_int(capture_payload.get('layer_capture_schema')) or 1,
        'content_id': _to_int(capture_payload.get('content_id')),
        'source_url': _first_text(capture_payload, 'source_url', 'sourceUrl'),
        'captured_at': _first_text(capture_payload, 'captured_at', 'capturedAt'),
        'status': status,
        'fingerprint': _capture_payload_fingerprint(capture_payload),
        'capture_hash': hashlib.sha256(_json_dumps(capture_payload).encode('utf-8')).hexdigest(),
        'runtime': _runtime_summary(capture_payload.get('runtime')),
        'warnings': _warning_summary(capture_payload.get('warnings')),
        'layer_count': len(layers),
        'matched': merge_report.get('matched', 0),
        'changed': merge_report.get('changed', 0),
        'skipped': merge_report.get('skipped', []),
        'blocked': merge_report.get('blocked', []),
        'quality_issues': merge_report.get('quality_issues', []),
        'layers': _summary_layers(layers),
    }


def merge_layer_capture_files(
    *,
    root: Path,
    capture_payload: dict[str, Any],
    dry_run: bool = True,
) -> dict[str, Any]:
    manifest_path = root / 'manifest.json'
    character_path = root / 'character.json'
    manifest = _read_json_object(manifest_path)
    character = _read_json_object(character_path)
    live2d_models = manifest.get('live2d_models')
    character_models = character.get('live2d_models')
    if not isinstance(live2d_models, list) or not isinstance(character_models, list):
        msg = f'{root} is missing live2d_models in manifest.json or character.json'
        raise TypeError(msg)

    content_id = _to_int(manifest.get('content_id'))
    manifest_report = merge_live2d_layer_captures(live2d_models, capture_payload, content_id=content_id, dry_run=dry_run)
    character_report = merge_live2d_layer_captures(character_models, capture_payload, content_id=content_id, dry_run=dry_run)
    report = {
        'root': root.as_posix(),
        'dry_run': dry_run,
        'manifest': manifest_report,
        'character': character_report,
    }
    if dry_run:
        return report

    summary = layer_capture_manifest_summary(capture_payload, manifest_report)
    manifest[LIVE2D_LAYER_CAPTURE_FIELD] = summary
    character[LIVE2D_LAYER_CAPTURE_FIELD] = summary
    _write_json(manifest_path, manifest)
    _write_json(character_path, character)
    return report


def strip_layer_metadata_files(*, root: Path, dry_run: bool = True) -> dict[str, Any]:
    manifest_path = root / 'manifest.json'
    character_path = root / 'character.json'
    manifest = _read_json_object(manifest_path)
    character = _read_json_object(character_path)
    live2d_models = manifest.get('live2d_models')
    character_models = character.get('live2d_models')
    if not isinstance(live2d_models, list) or not isinstance(character_models, list):
        msg = f'{root} is missing live2d_models in manifest.json or character.json'
        raise TypeError(msg)

    report = {
        'root': root.as_posix(),
        'dry_run': dry_run,
        'manifest': remove_live2d_layer_metadata(live2d_models, dry_run=dry_run),
        'character': remove_live2d_layer_metadata(character_models, dry_run=dry_run),
    }
    manifest_has_capture = LIVE2D_LAYER_CAPTURE_FIELD in manifest
    character_has_capture = LIVE2D_LAYER_CAPTURE_FIELD in character
    report['capture_summary'] = {
        'manifest_removed': manifest_has_capture,
        'character_removed': character_has_capture,
    }
    if dry_run:
        return report

    manifest.pop(LIVE2D_LAYER_CAPTURE_FIELD, None)
    character.pop(LIVE2D_LAYER_CAPTURE_FIELD, None)
    _write_json(manifest_path, manifest)
    _write_json(character_path, character)
    return report


_copy_live2d_layer_metadata = copy_live2d_layer_metadata
_layer_capture_manifest_summary = layer_capture_manifest_summary
