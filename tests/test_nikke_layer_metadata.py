# ruff: noqa: FBT001, INP001, S101

import copy
import json
from pathlib import Path

import pytest

from src.web.nikke_layer_metadata import (
    LIVE2D_LAYER_CAPTURE_FIELD,
    evaluate_layer_capture_reuse,
    layer_capture_manifest_summary,
    live2d_layer_fingerprint,
    live2d_layer_fingerprint_candidates,
    merge_live2d_layer_captures,
    read_previous_layer_capture,
)


def _model(stable_id: str, live2d_key: str, *, atlas_suffix: str = '') -> dict[str, object]:
    return {
        'label': 'live2d(full)',
        'skin_index': 0,
        'stable_id': stable_id,
        'live2d_key': live2d_key,
        'urls': {
            'atlas': f'https://cdn.example.com/{live2d_key}/model.atlas{atlas_suffix}',
            'skel': f'https://cdn.example.com/{live2d_key}/model.skel',
            'json': f'https://cdn.example.com/{live2d_key}/model.json',
            'image': [
                f'https://cdn.example.com/{live2d_key}/texture-b.png',
                f'https://cdn.example.com/{live2d_key}/texture-a.png',
            ],
        },
    }


def _models() -> list[dict[str, object]]:
    return [
        _model('stable-main', 'main'),
        _model('stable-front', 'front'),
        _model('stable-back', 'back'),
    ]


def _success_capture(content_id: int, models: list[dict[str, object]]) -> dict[str, object]:
    return {
        'content_id': content_id,
        'status': 'success',
        'fingerprint': live2d_layer_fingerprint(content_id, models),
        'layers': [
            {
                'stable_id': 'stable-main',
                'live2d_key': 'main',
                'layer_order': 2,
                'source_z_index': 9,
                'source_layer_index': 0,
                'is_primary': True,
                'layer_match_confidence': 'high',
            },
            {
                'stable_id': 'stable-front',
                'live2d_key': 'front',
                'layer_order': 3,
                'source_z_index': 10,
                'source_layer_index': 1,
                'is_primary': False,
                'layer_match_confidence': 'high',
            },
            {
                'stable_id': 'stable-back',
                'live2d_key': 'back',
                'layer_order': 1,
                'source_z_index': 8,
                'source_layer_index': 2,
                'is_primary': False,
                'layer_match_confidence': 'high',
            },
        ],
    }


def test_live2d_layer_fingerprint_changes_when_identity_or_urls_change() -> None:
    content_id = 711133
    models = _models()
    same_models_different_order = [models[2], models[0], models[1]]
    changed_url_models = [
        _model('stable-main', 'main'),
        _model('stable-front', 'front', atlas_suffix='?v=2'),
        _model('stable-back', 'back'),
    ]
    changed_skin_models = copy.deepcopy(models)
    changed_skin_models[1]['skin_index'] = 1
    changed_stable_id_models = copy.deepcopy(models)
    changed_stable_id_models[1]['stable_id'] = 'stable-front-v2'
    changed_live2d_key_models = copy.deepcopy(models)
    changed_live2d_key_models[1]['live2d_key'] = 'front-v2'

    fingerprint = live2d_layer_fingerprint(content_id, models)

    assert fingerprint == live2d_layer_fingerprint(content_id, same_models_different_order)
    assert fingerprint != live2d_layer_fingerprint(content_id, changed_url_models)
    assert fingerprint != live2d_layer_fingerprint(content_id, changed_skin_models)
    assert fingerprint != live2d_layer_fingerprint(content_id, changed_stable_id_models)
    assert fingerprint != live2d_layer_fingerprint(content_id, changed_live2d_key_models)
    assert fingerprint != live2d_layer_fingerprint(600940, models)


def test_live2d_layer_fingerprint_candidates_include_multi_full_groups() -> None:
    content_id = 711133
    models = [
        _model('skin0-main', 'skin0-main'),
        _model('skin0-back', 'skin0-back'),
        {**_model('skin1-main', 'skin1-main'), 'skin_index': 1},
        {**_model('skin1-back', 'skin1-back'), 'skin_index': 1},
    ]

    candidates = live2d_layer_fingerprint_candidates(content_id, models)

    assert live2d_layer_fingerprint(content_id, models) in candidates
    assert live2d_layer_fingerprint(content_id, models[:2] + models[2:]) in candidates
    assert live2d_layer_fingerprint(content_id, models[:2]) in candidates
    assert live2d_layer_fingerprint(content_id, models[2:]) in candidates
    assert candidates[0] == live2d_layer_fingerprint(content_id, models)


def test_evaluate_layer_capture_reuse_accepts_matching_success_capture_without_mutating_models() -> None:
    content_id = 711133
    models = _models()
    original_models = copy.deepcopy(models)
    capture = _success_capture(content_id, models)

    decision = evaluate_layer_capture_reuse(content_id=content_id, live2d_models=models, previous_capture=capture)

    assert decision['reusable'] is True
    assert decision['action'] == 'reuse'
    assert decision['reason'] == 'fingerprint_match'
    assert decision['merge_report']['skipped'] == []
    assert decision['merge_report']['quality_issues'] == []
    assert models == original_models


def test_layer_capture_manifest_summary_excludes_raw_container_evidence() -> None:
    content_id = 711133
    models = _models()
    capture = _success_capture(content_id, models)
    capture['runtime'] = {
        'requested_live2d_keys': ['main', 'front', 'back'],
        'matched_live2d_keys': ['main', 'front', 'back'],
        'container_count': 3,
        'raw_browser_state': {'path': '/var/lib/chromium-profile'},
    }
    capture['warnings'] = ['safe warning', {'raw': 'object'}, 'path /var/lib/chromium-profile']
    layers = capture['layers']
    assert isinstance(layers, list)
    layers[0]['raw_container'] = {'class_name': 'spine-player-container', 'style': 'z-index: 9'}
    report = merge_live2d_layer_captures(models, capture, content_id=content_id)

    summary = layer_capture_manifest_summary(capture, report)

    assert capture['layers'][0]['raw_container']
    assert summary['capture_hash']
    assert all('raw_container' not in layer for layer in summary['layers'])
    assert summary['runtime'] == {
        'requested_live2d_keys': ['main', 'front', 'back'],
        'matched_live2d_keys': ['main', 'front', 'back'],
        'container_count': 3,
    }
    assert summary['warnings'] == ['safe warning', 'path <path>']


def test_evaluate_layer_capture_reuse_accepts_matching_multi_full_group_fingerprint() -> None:
    content_id = 711133
    models = [
        _model('skin0-single', 'skin0-single'),
        {**_model('stable-main', 'main'), 'skin_index': 1},
        {**_model('stable-front', 'front'), 'skin_index': 1},
        {**_model('stable-back', 'back'), 'skin_index': 1},
    ]
    group_models = models[1:]
    capture = _success_capture(content_id, group_models)

    decision = evaluate_layer_capture_reuse(content_id=content_id, live2d_models=models, previous_capture=capture)

    assert decision['reusable'] is True
    assert decision['action'] == 'reuse'
    assert decision['reason'] == 'fingerprint_match'
    assert decision['previous_fingerprint'] != decision['current_fingerprint']


def test_evaluate_layer_capture_reuse_accepts_all_multi_full_groups_fingerprint_with_extra_models() -> None:
    content_id = 711133
    models = [
        _model('base-single', 'base-single'),
        {**_model('skin0-main', 'skin0-main'), 'skin_index': 1},
        {**_model('skin0-back', 'skin0-back'), 'skin_index': 1},
        {**_model('skin1-main', 'skin1-main'), 'skin_index': 2},
        {**_model('skin1-back', 'skin1-back'), 'skin_index': 2},
    ]
    captured_models = models[1:]
    capture = {
        'content_id': content_id,
        'status': 'success',
        'fingerprint': live2d_layer_fingerprint(content_id, captured_models),
        'layers': [
            {
                'stable_id': 'skin0-main',
                'live2d_key': 'skin0-main',
                'layer_order': 2,
                'source_z_index': 9,
                'source_layer_index': 0,
                'is_primary': True,
                'layer_match_confidence': 'high',
            },
            {
                'stable_id': 'skin0-back',
                'live2d_key': 'skin0-back',
                'layer_order': 1,
                'source_z_index': 8,
                'source_layer_index': 1,
                'is_primary': False,
                'layer_match_confidence': 'high',
            },
            {
                'stable_id': 'skin1-main',
                'live2d_key': 'skin1-main',
                'layer_order': 2,
                'source_z_index': 9,
                'source_layer_index': 0,
                'is_primary': True,
                'layer_match_confidence': 'high',
            },
            {
                'stable_id': 'skin1-back',
                'live2d_key': 'skin1-back',
                'layer_order': 1,
                'source_z_index': 8,
                'source_layer_index': 1,
                'is_primary': False,
                'layer_match_confidence': 'high',
            },
        ],
    }

    decision = evaluate_layer_capture_reuse(content_id=content_id, live2d_models=models, previous_capture=capture)

    assert decision['reusable'] is True
    assert decision['reason'] == 'fingerprint_match'
    assert decision['group_coverage']['complete'] is True
    assert decision['previous_fingerprint'] != decision['current_fingerprint']


@pytest.mark.parametrize(
    ('capture_update', 'force_refresh', 'expected_reason'),
    [
        ({'fingerprint': 'different'}, False, 'fingerprint_changed'),
        ({'status': 'failed'}, False, 'previous_status_not_success'),
        ({}, True, 'force_refresh'),
    ],
)
def test_evaluate_layer_capture_reuse_refreshes_when_reuse_contract_fails(
    capture_update: dict[str, object],
    force_refresh: bool,
    expected_reason: str,
) -> None:
    content_id = 711133
    models = _models()
    capture = _success_capture(content_id, models)
    capture.update(capture_update)

    decision = evaluate_layer_capture_reuse(
        content_id=content_id,
        live2d_models=models,
        previous_capture=capture,
        force_refresh=force_refresh,
    )

    assert decision['reusable'] is False
    assert decision['action'] == 'refresh'
    assert decision['reason'] == expected_reason


@pytest.mark.parametrize(
    ('previous_capture', 'expected_reason'),
    [
        (None, 'no_previous_capture'),
        ({'status': 'success'}, 'missing_fingerprint'),
    ],
)
def test_evaluate_layer_capture_reuse_refreshes_when_previous_capture_is_absent_or_unfingerprinted(
    previous_capture: dict[str, object] | None,
    expected_reason: str,
) -> None:
    content_id = 711133

    decision = evaluate_layer_capture_reuse(content_id=content_id, live2d_models=_models(), previous_capture=previous_capture)

    assert decision['reusable'] is False
    assert decision['action'] == 'refresh'
    assert decision['reason'] == expected_reason


def test_evaluate_layer_capture_reuse_refreshes_when_previous_confidence_is_not_high() -> None:
    content_id = 711133
    models = _models()
    capture = _success_capture(content_id, models)
    layers = capture['layers']
    assert isinstance(layers, list)
    layers[1]['layer_match_confidence'] = 'low'

    decision = evaluate_layer_capture_reuse(content_id=content_id, live2d_models=models, previous_capture=capture)

    assert decision['reusable'] is False
    assert decision['action'] == 'refresh'
    assert decision['reason'] == 'previous_confidence_not_high'


def test_evaluate_layer_capture_reuse_refreshes_when_previous_confidence_is_missing() -> None:
    content_id = 711133
    models = _models()
    capture = _success_capture(content_id, models)
    layers = capture['layers']
    assert isinstance(layers, list)
    for layer in layers:
        layer.pop('layer_match_confidence')

    decision = evaluate_layer_capture_reuse(content_id=content_id, live2d_models=models, previous_capture=capture)

    assert decision['reusable'] is False
    assert decision['action'] == 'refresh'
    assert decision['reason'] == 'previous_confidence_not_high'


def test_evaluate_layer_capture_reuse_refreshes_when_merge_skips_layer() -> None:
    content_id = 711133
    models = _models()
    capture = _success_capture(content_id, models)
    layers = capture['layers']
    assert isinstance(layers, list)
    layers[1]['stable_id'] = 'missing-model'

    decision = evaluate_layer_capture_reuse(content_id=content_id, live2d_models=models, previous_capture=capture)

    assert decision['reusable'] is False
    assert decision['action'] == 'refresh'
    assert decision['reason'] == 'quality_gates_failed'
    assert decision['merge_report']['skipped'][0]['reason'] == 'no_match'


def test_evaluate_layer_capture_reuse_refreshes_when_quality_issues_remain_after_merge() -> None:
    content_id = 711133
    models = _models()
    capture = _success_capture(content_id, models)
    layers = capture['layers']
    assert isinstance(layers, list)
    layers[0]['layer_order'] = 1
    layers[1]['layer_order'] = 1

    decision = evaluate_layer_capture_reuse(content_id=content_id, live2d_models=models, previous_capture=capture)

    assert decision['reusable'] is False
    assert decision['action'] == 'refresh'
    assert decision['reason'] == 'quality_gates_failed'
    assert decision['merge_report']['skipped'] == []
    assert {issue['code'] for issue in decision['merge_report']['quality_issues']} == {'duplicate_layer_order'}


def test_read_previous_layer_capture_prefers_raw_capture_over_manifest_summary(tmp_path: Path) -> None:
    raw_capture = {'status': 'success', 'fingerprint': 'raw'}
    summary_capture = {'status': 'success', 'fingerprint': 'summary'}
    raw_path = tmp_path / 'raw/live2d-layer-capture.json'
    raw_path.parent.mkdir()
    raw_path.write_text(json.dumps(raw_capture), encoding='utf-8')
    (tmp_path / 'manifest.json').write_text(json.dumps({LIVE2D_LAYER_CAPTURE_FIELD: summary_capture}), encoding='utf-8')

    assert read_previous_layer_capture(tmp_path) == raw_capture

    raw_path.unlink()

    assert read_previous_layer_capture(tmp_path) == summary_capture
