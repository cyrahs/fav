# ruff: noqa: INP001, S101, SLF001

import asyncio
import hashlib
import json
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from pathlib import Path

import pytest

import src.service.jobs as jobs_module
import src.web.nikke as nikke_module
import src.web.nikke_layer_metadata as layer_metadata_module
from src.api.schemas import JobRequestTarget
from src.core import settings
from src.core.settings import Nikke as NikkeConfig
from src.web.nikke import (
    Asset,
    AssetProcessingError,
    Nikke,
    add_asset,
    assign_asset_paths,
    extract_resources,
    filter_nikke_rows,
    materialize_blob,
    parse_spine_atlas_textures,
    validate_asset_response,
)
from src.web.nikke_layer_metadata import (
    LIVE2D_LAYER_METADATA_FIELDS,
    live2d_layer_fingerprint,
    merge_layer_capture_files,
    merge_live2d_layer_captures,
    strip_layer_metadata_files,
    validate_live2d_layer_metadata,
)


def _configure_nikke(crawler: Nikke, **updates: object) -> None:
    """Nikke snapshots its settings in __init__, so patch the instance copy."""
    for key, value in updates.items():
        setattr(crawler.cfg, key, value)


def test_nikke_config_defaults_to_disabled_collection_nikke_path() -> None:
    expected_runtime_timeout_seconds = 60.0
    cfg = NikkeConfig()

    assert cfg.enabled is False
    assert cfg.cron == '0 */6 * * *'
    assert cfg.run_on_start is False
    assert cfg.path == Path('./collection/nikke')
    assert cfg.runtime_capture_enabled is False
    assert cfg.runtime_capture_timeout_seconds == expected_runtime_timeout_seconds
    assert cfg.runtime_capture_force_refresh is False


def test_scheduler_registration_includes_nikke() -> None:
    fake_config = settings.Settings()
    for attr in ('azurlane', 'bd2', 'bilibili', 'hanime1', 'jandan', 'stellasora', 'telegram'):
        getattr(fake_config.web, attr).enabled = True
    fake_config.web.nikke.enabled = False
    jobs = jobs_module.build_jobs(fake_config)
    nikke_job = next(job for job in jobs if job.key == 'nikke')

    assert nikke_job.name == 'Nikke'
    assert nikke_job.enabled is False
    assert nikke_job.required_commands == ()
    assert nikke_job.factory is jobs_module.Nikke


def test_api_job_enum_includes_nikke() -> None:
    assert JobRequestTarget.NIKKE.value == 'nikke'


def test_nikke_module_reexports_layer_metadata_helpers_for_compatibility() -> None:
    assert nikke_module.merge_live2d_layer_captures is layer_metadata_module.merge_live2d_layer_captures
    assert nikke_module.merge_layer_capture_files is layer_metadata_module.merge_layer_capture_files
    assert nikke_module.strip_layer_metadata_files is layer_metadata_module.strip_layer_metadata_files
    assert nikke_module.validate_live2d_layer_metadata is layer_metadata_module.validate_live2d_layer_metadata


def test_filter_nikke_rows_keeps_only_wrapped_rows_with_content_id() -> None:
    rows = [
        {'nikke': {'content_id': '691899', 'title': '海伦'}},
        {'stellasora': {'content_id': 1}},
        {'nikke': {'content_id': 'not-an-id', 'title': 'bad'}},
        {'nikke': {'id': 171728, 'title': '薇尔维特'}},
    ]

    filtered = filter_nikke_rows(rows)

    assert [row['title'] for row in filtered] == ['海伦', '薇尔维特']


def test_extract_resources_uses_bound_media_gate() -> None:
    expected_layer_order = 2
    expected_source_z_index = 9
    content_json = {
        'baseData': [
            [
                {'type': 'text', 'value': '头像'},
                {'type': 'image', 'key': 'bound_image', 'value': '/media/bound.png'},
                {'type': 'image', 'key': 'unbound_image', 'value': '/media/unbound.png'},
            ],
            [
                {'type': 'text', 'value': '视频'},
                {'type': 'video', 'key': 'bound_video', 'value': 'https://cdn.example.com/movie.mp4'},
            ],
            [
                {'type': 'text', 'value': '模型'},
                {
                    'type': 'live2d',
                    'key': 'bound_model',
                    'value': {
                        'live2dKey': 'model-a',
                        'atlas': '/spine/model.atlas',
                        'skel': '/spine/model.skel',
                        'image': '/spine/model.png',
                        'layerOrder': str(expected_layer_order),
                        'sourceZIndex': expected_source_z_index,
                        'sourceLayerIndex': 0,
                        'isPrimary': True,
                        'layerMatchMethod': 'gamekee-player-init',
                        'layerMatchConfidence': 'high',
                    },
                },
            ],
        ],
    }
    reverse_bind = {
        'bound_image': 'stable-image',
        'bound_video': 'stable-video',
        'bound_model': 'stable-model',
    }

    assets, models = extract_resources(
        content_json=content_json,
        tj_list_row={'icon': '/icons/face.png'},
        reverse_bind=reverse_bind,
    )

    asset_urls = {asset.url for asset in assets.values()}
    assert 'https://www.gamekee.com/media/bound.png' in asset_urls
    assert 'https://cdn.example.com/movie.mp4' in asset_urls
    assert 'https://www.gamekee.com/spine/model.atlas' in asset_urls
    assert 'https://www.gamekee.com/spine/model.skel' in asset_urls
    assert 'https://www.gamekee.com/spine/model.png' in asset_urls
    assert 'https://www.gamekee.com/icons/face.png' in asset_urls
    assert 'https://www.gamekee.com/media/unbound.png' not in asset_urls
    assert models[0]['live2d_key'] == 'model-a'
    assert models[0]['layer_order'] == expected_layer_order
    assert models[0]['source_z_index'] == expected_source_z_index
    assert models[0]['source_layer_index'] == 0
    assert models[0]['is_primary'] is True
    assert models[0]['layer_match_method'] == 'gamekee-player-init'
    assert models[0]['layer_match_confidence'] == 'high'


def _layered_model(stable_id: str, live2d_key: str, *, skin_index: int = 0) -> dict[str, object]:
    return {
        'label': 'live2d(full)',
        'section': 'style',
        'skin_index': skin_index,
        'stable_id': stable_id,
        'live2d_key': live2d_key,
        'urls': {
            'atlas': f'https://cdn.example.com/{live2d_key}/model.atlas',
            'skel': f'https://cdn.example.com/{live2d_key}/model.skel',
        },
    }


def _runtime_content_json() -> dict[str, object]:
    return {
        'styleData': [
            {
                'name': 'default',
                'data': [
                    [
                        {'type': 'text', 'value': 'Model'},
                        {
                            'type': 'live2d',
                            'key': 'main-cell',
                            'value': {
                                'live2dKey': 'main',
                                'atlas': 'https://cdn.example.com/main/model.atlas',
                                'skel': 'https://cdn.example.com/main/model.skel',
                            },
                        },
                        {
                            'type': 'live2d',
                            'key': 'back-cell',
                            'value': {
                                'live2dKey': 'back',
                                'atlas': 'https://cdn.example.com/back/model.atlas',
                                'skel': 'https://cdn.example.com/back/model.skel',
                            },
                        },
                    ],
                ],
            },
        ],
    }


def _runtime_content_json_with_extra_skin() -> dict[str, object]:
    content_json = _runtime_content_json()
    skins = content_json['styleData']
    assert isinstance(skins, list)
    skins.append(
        {
            'name': 'extra',
            'data': [
                [
                    {'type': 'text', 'value': 'Model'},
                    {
                        'type': 'live2d',
                        'key': 'extra-main-cell',
                        'value': {
                            'live2dKey': 'extra-main',
                            'atlas': 'https://cdn.example.com/extra-main/model.atlas',
                            'skel': 'https://cdn.example.com/extra-main/model.skel',
                        },
                    },
                    {
                        'type': 'live2d',
                        'key': 'extra-back-cell',
                        'value': {
                            'live2dKey': 'extra-back',
                            'atlas': 'https://cdn.example.com/extra-back/model.atlas',
                            'skel': 'https://cdn.example.com/extra-back/model.skel',
                        },
                    },
                ],
            ],
        },
    )
    return content_json


def _runtime_reverse_bind() -> dict[str, str]:
    return {
        'main-cell': 'stable-main',
        'back-cell': 'stable-back',
        'extra-main-cell': 'extra-main',
        'extra-back-cell': 'extra-back',
    }


def _runtime_capture_payload(content_id: int, models: list[dict[str, object]]) -> dict[str, object]:
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
                'stable_id': 'stable-back',
                'live2d_key': 'back',
                'layer_order': 1,
                'source_z_index': 8,
                'source_layer_index': 1,
                'is_primary': False,
                'layer_match_confidence': 'high',
            },
        ],
    }


def _capture_hash(payload: dict[str, object]) -> str:
    data = json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(',', ':'), default=str)
    return hashlib.sha256(data.encode('utf-8')).hexdigest()


def test_merge_live2d_layer_captures_projects_dry_run_quality() -> None:
    expected_changed_count = 3
    models = [
        _layered_model('stable-main', 'main'),
        _layered_model('stable-front', 'front'),
        _layered_model('stable-back', 'back'),
    ]
    capture = {
        'content_id': 711133,
        'layers': [
            {
                'stable_id': 'stable-main',
                'live2dKey': 'main',
                'sourceZIndex': 9,
                'sourceLayerIndex': 0,
                'isPrimary': True,
                'layerMatchConfidence': 'high',
            },
            {
                'stable_id': 'stable-front',
                'live2dKey': 'front',
                'sourceZIndex': 10,
                'sourceLayerIndex': 1,
                'isPrimary': False,
                'layerMatchConfidence': 'high',
            },
            {
                'stable_id': 'stable-back',
                'live2dKey': 'back',
                'sourceZIndex': 8,
                'sourceLayerIndex': 2,
                'isPrimary': False,
                'layerMatchConfidence': 'high',
            },
        ],
    }

    dry_run_report = merge_live2d_layer_captures(models, capture, content_id=711133)

    assert dry_run_report['changed'] == expected_changed_count
    assert dry_run_report['quality_issues'] == []
    assert all('layer_order' not in model for model in models)

    write_report = merge_live2d_layer_captures(models, capture, content_id=711133, dry_run=False)

    assert write_report['quality_issues'] == []
    assert [
        (model['stable_id'], model['layer_order'], model['source_z_index'], model['source_layer_index'], model['is_primary'])
        for model in models
    ] == [
        ('stable-main', 2, 9, 0, True),
        ('stable-front', 3, 10, 1, False),
        ('stable-back', 1, 8, 2, False),
    ]
    assert {model['layer_match_method'] for model in models} == {'gamekee-runtime-container'}
    assert {model['layer_match_confidence'] for model in models} == {'high'}


def test_merge_live2d_layer_captures_treats_missing_confidence_as_low() -> None:
    models = [
        _layered_model('stable-main', 'main'),
        _layered_model('stable-back', 'back'),
    ]
    capture = {
        'content_id': 711133,
        'layers': [
            {
                'stable_id': 'stable-main',
                'live2d_key': 'main',
                'layer_order': 2,
                'source_z_index': 9,
                'source_layer_index': 0,
                'is_primary': True,
            },
            {
                'stable_id': 'stable-back',
                'live2d_key': 'back',
                'layer_order': 1,
                'source_z_index': 8,
                'source_layer_index': 1,
                'is_primary': False,
            },
        ],
    }

    report = merge_live2d_layer_captures(models, capture, content_id=711133, dry_run=False)

    assert {issue['code'] for issue in report['quality_issues']} == {'low_confidence_layer_match'}
    assert {issue['skin_index'] for issue in report['quality_issues']} == {0}
    assert {issue['model_kind'] for issue in report['quality_issues']} == {'full'}
    assert report['changed'] == 0
    assert {item['reason'] for item in report['blocked']} == {'quality_gates_failed'}
    assert {item['skin_index'] for item in report['blocked']} == {0}
    assert {item['model_kind'] for item in report['blocked']} == {'full'}
    assert all('layer_order' in item['fields'] for item in report['blocked'])
    assert all(field_name not in model for model in models for field_name in LIVE2D_LAYER_METADATA_FIELDS)


def test_merge_live2d_layer_captures_commits_only_valid_skin_groups() -> None:
    expected_valid_changes = 2
    expected_main_layer_order = 2
    models = [
        _layered_model('skin0-main', 'skin0-main', skin_index=0),
        _layered_model('skin0-back', 'skin0-back', skin_index=0),
        _layered_model('skin1-main', 'skin1-main', skin_index=1),
        _layered_model('skin1-back', 'skin1-back', skin_index=1),
    ]
    capture = {
        'content_id': 711133,
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
                'layer_order': 1,
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

    report = merge_live2d_layer_captures(models, capture, content_id=711133, dry_run=False)

    assert report['changed'] == expected_valid_changes
    assert {issue['code'] for issue in report['quality_issues']} == {'duplicate_layer_order'}
    assert {issue['model_kind'] for issue in report['quality_issues']} == {'full'}
    assert {item['skin_index'] for item in report['blocked']} == {1}
    assert {item['model_kind'] for item in report['blocked']} == {'full'}
    assert all('layer_order' in item['fields'] for item in report['blocked'])
    assert models[0]['layer_order'] == expected_main_layer_order
    assert models[1]['layer_order'] == 1
    assert all(field_name not in model for model in models[2:] for field_name in LIVE2D_LAYER_METADATA_FIELDS)


def test_merge_live2d_layer_captures_clears_existing_fields_for_blocked_skin_group() -> None:
    models = [
        {
            **_layered_model('stable-main', 'main'),
            'layer_order': 7,
            'source_z_index': 99,
            'source_layer_index': 4,
            'is_primary': True,
            'layer_match_method': 'stale',
            'layer_match_confidence': 'high',
        },
        {
            **_layered_model('stable-back', 'back'),
            'layer_order': 8,
            'source_z_index': 98,
            'source_layer_index': 5,
            'is_primary': False,
            'layer_match_method': 'stale',
            'layer_match_confidence': 'high',
        },
    ]
    capture = {
        'content_id': 711133,
        'layers': [
            {
                'stable_id': 'stable-main',
                'live2d_key': 'main',
                'layer_order': 1,
                'source_z_index': 9,
                'source_layer_index': 0,
                'is_primary': True,
                'layer_match_confidence': 'high',
            },
            {
                'stable_id': 'stable-back',
                'live2d_key': 'back',
                'layer_order': 1,
                'source_z_index': 8,
                'source_layer_index': 1,
                'is_primary': False,
                'layer_match_confidence': 'high',
            },
        ],
    }

    report = merge_live2d_layer_captures(models, capture, content_id=711133, dry_run=False)

    assert {issue['code'] for issue in report['quality_issues']} == {'duplicate_layer_order'}
    assert report['blocked']
    assert all(field_name not in model for model in models for field_name in LIVE2D_LAYER_METADATA_FIELDS)


def test_merge_live2d_layer_captures_clears_uncaptured_siblings_in_blocked_skin_group() -> None:
    models = [
        {
            **_layered_model('stable-main', 'main'),
            'layer_order': 7,
            'source_z_index': 99,
            'source_layer_index': 4,
            'is_primary': True,
            'layer_match_method': 'stale',
            'layer_match_confidence': 'high',
        },
        {
            **_layered_model('stable-back', 'back'),
            'layer_order': 8,
            'source_z_index': 98,
            'source_layer_index': 5,
            'is_primary': False,
            'layer_match_method': 'stale',
            'layer_match_confidence': 'high',
        },
    ]
    capture = {
        'content_id': 711133,
        'layers': [
            {
                'stable_id': 'stable-main',
                'live2d_key': 'main',
                'layer_order': 1,
                'source_z_index': 9,
                'source_layer_index': 0,
                'is_primary': True,
                'layer_match_confidence': 'low',
            },
        ],
    }

    report = merge_live2d_layer_captures(models, capture, content_id=711133, dry_run=False)

    assert {issue['code'] for issue in report['quality_issues']} == {
        'low_confidence_layer_match',
        'missing_capture_layer',
    }
    assert {item['stable_id'] for item in report['blocked']} == {'stable-main'}
    assert all(field_name not in model for model in models for field_name in LIVE2D_LAYER_METADATA_FIELDS)


def test_merge_live2d_layer_captures_clears_stale_group_when_capture_omits_sibling() -> None:
    models = [
        {
            **_layered_model('stable-main', 'main'),
            'layer_order': 7,
            'source_z_index': 99,
            'source_layer_index': 4,
            'is_primary': True,
            'layer_match_method': 'stale',
            'layer_match_confidence': 'high',
        },
        {
            **_layered_model('stable-back', 'back'),
            'layer_order': 8,
            'source_z_index': 98,
            'source_layer_index': 5,
            'is_primary': False,
            'layer_match_method': 'stale',
            'layer_match_confidence': 'high',
        },
    ]
    capture = {
        'content_id': 711133,
        'layers': [
            {
                'stable_id': 'stable-main',
                'live2d_key': 'main',
                'layer_order': 1,
                'source_z_index': 9,
                'source_layer_index': 0,
                'is_primary': True,
                'layer_match_confidence': 'high',
            },
        ],
    }

    report = merge_live2d_layer_captures(models, capture, content_id=711133, dry_run=False)

    assert {issue['code'] for issue in report['quality_issues']} == {'missing_capture_layer'}
    assert report['quality_issues'][0]['models'] == ['stable-back/back']
    assert {item['stable_id'] for item in report['blocked']} == {'stable-main'}
    assert all(field_name not in model for model in models for field_name in LIVE2D_LAYER_METADATA_FIELDS)


def test_merge_live2d_layer_captures_clears_stale_group_when_matched_capture_is_incomplete() -> None:
    models = [
        {
            **_layered_model('stable-main', 'main'),
            'layer_order': 7,
            'source_z_index': 99,
            'source_layer_index': 4,
            'is_primary': True,
            'layer_match_method': 'stale',
            'layer_match_confidence': 'high',
        },
        {
            **_layered_model('stable-back', 'back'),
            'layer_order': 8,
            'source_z_index': 98,
            'source_layer_index': 5,
            'is_primary': False,
            'layer_match_method': 'stale',
            'layer_match_confidence': 'high',
        },
    ]
    capture = {
        'content_id': 711133,
        'layers': [
            {
                'stable_id': 'stable-main',
                'live2d_key': 'main',
                'source_z_index': 9,
                'source_layer_index': 0,
                'layer_match_confidence': 'high',
            },
        ],
    }

    report = merge_live2d_layer_captures(models, capture, content_id=711133, dry_run=False)

    assert report['quality_issues'] == []
    assert report['blocked'] == []
    assert report['skipped'][0]['reason'] == 'incomplete_layer_metadata'
    assert report['skipped'][0]['skin_index'] == 0
    assert report['skipped'][0]['model_kind'] == 'full'
    assert all(field_name not in model for model in models for field_name in LIVE2D_LAYER_METADATA_FIELDS)


def test_merge_live2d_layer_captures_skips_ambiguous_url_only_capture() -> None:
    models = [
        {
            **_layered_model('stable-a', 'a'),
            'urls': {'atlas': 'https://cdn.example.com/shared/model.atlas'},
        },
        {
            **_layered_model('stable-b', 'b'),
            'urls': {'atlas': 'https://cdn.example.com/shared/model.atlas'},
        },
    ]
    capture = {
        'content_id': 711133,
        'layers': [
            {
                'resource_urls': {'atlas': 'https://cdn.example.com/shared/model.atlas'},
                'layer_order': 1,
                'is_primary': True,
            },
        ],
    }

    report = merge_live2d_layer_captures(models, capture, content_id=711133, dry_run=False)

    assert report['changed'] == 0
    assert report['skipped'][0]['reason'] == 'ambiguous_match'
    assert all('layer_order' not in model for model in models)


def test_validate_live2d_layer_metadata_flags_incomplete_multi_full() -> None:
    models = [
        {**_layered_model('stable-a', 'a'), 'layer_order': 1, 'is_primary': True, 'layer_match_confidence': 'high'},
        {**_layered_model('stable-b', 'b')},
    ]

    issues = validate_live2d_layer_metadata(models)

    assert {issue['code'] for issue in issues} == {
        'missing_layer_order',
        'low_confidence_layer_match',
    }


def test_merge_layer_capture_files_updates_manifest_and_character(tmp_path: Path) -> None:
    expected_main_layer_order = 2
    root = tmp_path / '711133 - Test'
    root.mkdir()
    models = [_layered_model('stable-main', 'main'), _layered_model('stable-back', 'back')]
    manifest = {'content_id': 711133, 'title': 'Test', 'live2d_models': [dict(model) for model in models]}
    character = {'content_id': 711133, 'title': 'Test', 'live2d_models': [dict(model) for model in models]}
    (root / 'manifest.json').write_text(json.dumps(manifest), encoding='utf-8')
    (root / 'character.json').write_text(json.dumps(character), encoding='utf-8')
    capture = {
        'content_id': 711133,
        'status': 'reused-sentinel',
        'fingerprint': 'payload-fingerprint-sentinel',
        'layers': [
            {
                'stable_id': 'stable-main',
                'live2d_key': 'main',
                'layer_order': 2,
                'source_z_index': 9,
                'is_primary': True,
                'layer_match_confidence': 'high',
            },
            {
                'stable_id': 'stable-back',
                'live2d_key': 'back',
                'layer_order': 1,
                'source_z_index': 8,
                'is_primary': False,
                'layer_match_confidence': 'high',
            },
        ],
    }

    report = merge_layer_capture_files(root=root, capture_payload=capture, dry_run=False)

    assert report['manifest']['quality_issues'] == []
    saved_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    saved_character = json.loads((root / 'character.json').read_text(encoding='utf-8'))
    assert saved_manifest['live2d_models'][0]['layer_order'] == expected_main_layer_order
    assert saved_character['live2d_models'][1]['is_primary'] is False
    assert saved_manifest['live2d_layer_capture']['layer_count'] == expected_main_layer_order
    assert saved_manifest['live2d_layer_capture']['status'] == capture['status']
    assert saved_manifest['live2d_layer_capture']['fingerprint'] == capture['fingerprint']
    assert saved_manifest['live2d_layer_capture']['capture_hash']

    strip_report = strip_layer_metadata_files(root=root, dry_run=False)

    stripped_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    assert strip_report['capture_summary'] == {'manifest_removed': True, 'character_removed': True}
    assert 'live2d_layer_capture' not in stripped_manifest
    assert 'layer_order' not in stripped_manifest['live2d_models'][0]


def test_merge_layer_capture_files_marks_summary_incomplete_when_quality_gates_fail(tmp_path: Path) -> None:
    root = tmp_path / '711133 - Test'
    root.mkdir()
    models = [_layered_model('stable-main', 'main'), _layered_model('stable-back', 'back')]
    manifest = {'content_id': 711133, 'title': 'Test', 'live2d_models': [dict(model) for model in models]}
    character = {'content_id': 711133, 'title': 'Test', 'live2d_models': [dict(model) for model in models]}
    (root / 'manifest.json').write_text(json.dumps(manifest), encoding='utf-8')
    (root / 'character.json').write_text(json.dumps(character), encoding='utf-8')
    capture = {
        'content_id': 711133,
        'status': 'success',
        'fingerprint': 'payload-fingerprint-sentinel',
        'layers': [
            {'stable_id': 'stable-main', 'live2d_key': 'main', 'layer_order': 1, 'source_z_index': 9, 'is_primary': True},
            {'stable_id': 'stable-back', 'live2d_key': 'back', 'layer_order': 1, 'source_z_index': 8, 'is_primary': False},
        ],
    }

    report = merge_layer_capture_files(root=root, capture_payload=capture, dry_run=False)

    saved_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    saved_character = json.loads((root / 'character.json').read_text(encoding='utf-8'))
    assert report['manifest']['blocked']
    assert saved_manifest['live2d_layer_capture']['status'] == 'incomplete'
    assert saved_manifest['live2d_layer_capture']['blocked']
    assert saved_manifest['live2d_layer_capture']['quality_issues']
    assert all(field_name not in saved_manifest['live2d_models'][0] for field_name in LIVE2D_LAYER_METADATA_FIELDS)
    assert saved_character['live2d_layer_capture'] == saved_manifest['live2d_layer_capture']
    assert saved_character['live2d_models'] == saved_manifest['live2d_models']


def test_add_asset_preserves_same_url_different_kinds() -> None:
    assets: dict[tuple[str, str], Asset] = {}

    add_asset(assets, 'https://cdn.example.com/shared.png', 'image', {'field': 'icon'})
    add_asset(assets, 'https://cdn.example.com/shared.png', 'live2d_texture', {'live2d_field': 'image'})

    assert set(assets) == {
        ('https://cdn.example.com/shared.png', 'image'),
        ('https://cdn.example.com/shared.png', 'live2d_texture'),
    }
    assert sorted(asset.kind for asset in assets.values()) == ['image', 'live2d_texture']


def test_parse_spine_atlas_textures_dedupes_texture_lines() -> None:
    atlas_text = """
\ufeffmodel.png
size: 1024,1024
format: RGBA8888
region
  rotate: false
effect.webp
model.png
"""

    assert parse_spine_atlas_textures(atlas_text) == ['model.png', 'effect.webp']


def test_validate_asset_response_rejects_empty_and_html() -> None:
    assert validate_asset_response(kind='image', content_type='image/png', body_prefix=b'', size=0) == 'empty response body'
    assert (
        validate_asset_response(
            kind='image',
            content_type='text/html; charset=utf-8',
            body_prefix=b'<!doctype html><html>Forbidden</html>',
            size=42,
        )
        == 'cdn rejection response'
    )


def test_load_content_json_uses_content_cdn_when_inline_content_is_empty(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    crawler = Nikke(path=tmp_path)
    requests: list[tuple[str, int, str]] = []

    async def _fake_fetch_cdn_json(_client: object, url: str, *, content_id: int, label: str) -> dict[str, object]:
        requests.append((url, content_id, label))
        return {'content': json.dumps({'styleData': [{'name': 'default', 'data': []}]})}

    monkeypatch.setattr(crawler, '_fetch_cdn_json', _fake_fetch_cdn_json)

    content_json = asyncio.run(
        crawler._load_content_json(
            object(),
            {'content_json': '', 'content_cdn': '//api-cdn.example.test/content/1.json'},
            content_id=1,
        ),
    )

    assert content_json == {'styleData': [{'name': 'default', 'data': []}]}
    assert requests == [('https://api-cdn.example.test/content/1.json', 1, 'content_cdn')]


def test_load_content_json_rejects_empty_inline_content_without_cdn(tmp_path: Path) -> None:
    crawler = Nikke(path=tmp_path)

    with pytest.raises(RuntimeError, match='content_json is empty'):
        asyncio.run(crawler._load_content_json(object(), {'content_json': ''}, content_id=1))


def test_load_reverse_bind_uses_entry_data_bind_cdn(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    crawler = Nikke(path=tmp_path)

    async def _fake_fetch_cdn_json(_client: object, _url: str, *, content_id: int, label: str) -> dict[str, object]:
        _ = content_id, label
        return {'entry_data_bind': {'stable-image': 'dynamic-image'}}

    monkeypatch.setattr(crawler, '_fetch_cdn_json', _fake_fetch_cdn_json)

    reverse_bind = asyncio.run(
        crawler._load_reverse_bind(
            object(),
            {'entry_data_bind_cdn': '//api-cdn.example.test/entry_binding/entry_bind_data.json'},
            content_id=1,
        ),
    )

    assert reverse_bind == {'dynamic-image': 'stable-image'}


def _install_crawl_page_fakes(
    monkeypatch: pytest.MonkeyPatch,
    crawler: Nikke,
    *,
    content_json: dict[str, object],
    reverse_bind: dict[str, str],
    events: list[str],
) -> None:
    async def _fetch_detail_response(_client: object, content_id: int) -> dict[str, object]:
        return {'data': {'title': 'Test', 'entry_id': content_id, 'updated_at': '2026-07-07T00:00:00+00:00'}}

    async def _load_content_json(_client: object, _detail: dict[str, object], *, content_id: int) -> dict[str, object]:
        _ = content_id
        return content_json

    async def _load_reverse_bind(_client: object, _detail: dict[str, object], *, content_id: int) -> dict[str, str]:
        _ = content_id
        return reverse_bind

    async def _upsert_page_fetch_start(**_kwargs: object) -> None:
        return None

    async def _expand_atlas_textures(**_kwargs: object) -> None:
        events.append('expand')
        live2d_models = _kwargs.get('live2d_models')
        assert isinstance(live2d_models, list)
        for model in live2d_models:
            urls = model.get('urls')
            if isinstance(urls, dict):
                urls['atlas_textures'] = [f'https://cdn.example.com/{model.get("live2d_key")}/expanded.png']

    async def _process_assets(**_kwargs: object) -> None:
        events.append('process')

    async def _replace_page_assets_and_mark_completed(**_kwargs: object) -> None:
        events.append('complete')

    monkeypatch.setattr(crawler, '_fetch_detail_response', _fetch_detail_response)
    monkeypatch.setattr(crawler, '_load_content_json', _load_content_json)
    monkeypatch.setattr(crawler, '_load_reverse_bind', _load_reverse_bind)
    monkeypatch.setattr(crawler, '_upsert_page_fetch_start', _upsert_page_fetch_start)
    monkeypatch.setattr(crawler, '_expand_atlas_textures', _expand_atlas_textures)
    monkeypatch.setattr(crawler, '_process_assets', _process_assets)
    monkeypatch.setattr(crawler, '_replace_page_assets_and_mark_completed', _replace_page_assets_and_mark_completed)


def test_crawl_page_reuses_previous_layer_capture_before_manifest_write(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    expected_layer_count = 2
    content_id = 711133
    content_json = _runtime_content_json()
    reverse_bind = _runtime_reverse_bind()
    _assets, models = extract_resources(content_json=content_json, tj_list_row={'title': 'Test'}, reverse_bind=reverse_bind)
    root = tmp_path / '711133 - Test'
    raw_capture_path = root / 'raw/live2d-layer-capture.json'
    raw_capture_path.parent.mkdir(parents=True)
    previous_capture = _runtime_capture_payload(content_id, models)
    raw_capture_path.write_text(json.dumps(previous_capture), encoding='utf-8')
    events: list[str] = []
    crawler = Nikke(path=tmp_path)
    _install_crawl_page_fakes(monkeypatch, crawler, content_json=content_json, reverse_bind=reverse_bind, events=events)
    _configure_nikke(crawler, runtime_capture_enabled=False, runtime_capture_force_refresh=False, runtime_capture_timeout_seconds=60.0)

    async def _unexpected_capture(_request: object) -> dict[str, object]:
        raise AssertionError

    monkeypatch.setattr(nikke_module, 'capture_gamekee_runtime_layers', _unexpected_capture)

    asyncio.run(crawler._crawl_page(client=object(), tj_list_row={'title': 'Test'}, content_id=content_id))

    saved_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    saved_character = json.loads((root / 'character.json').read_text(encoding='utf-8'))
    saved_capture = json.loads(raw_capture_path.read_text(encoding='utf-8'))
    assert [(model['stable_id'], model.get('layer_order'), model.get('is_primary')) for model in saved_manifest['live2d_models']] == [
        ('stable-main', 2, True),
        ('stable-back', 1, False),
    ]
    assert saved_character['live2d_models'] == saved_manifest['live2d_models']
    assert saved_capture == previous_capture
    assert len(saved_capture['layers']) == expected_layer_count
    assert saved_capture['status'] == 'success'
    assert saved_manifest['live2d_layer_capture']['status'] == 'success'
    assert saved_manifest['live2d_layer_capture']['action'] == 'reused'
    assert saved_manifest['live2d_layer_capture']['fingerprint'] == saved_capture['fingerprint']
    assert saved_manifest['live2d_layer_capture']['capture_hash'] == _capture_hash(saved_capture)
    assert saved_manifest['live2d_layer_capture']['attempted_at']
    assert saved_character['live2d_layer_capture'] == saved_manifest['live2d_layer_capture']
    assert events == ['expand', 'process', 'complete']


def test_crawl_page_captures_runtime_layers_when_enabled(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    expected_layer_count = 2
    expected_timeout_ms = 12500
    expected_main_layer_order = 2
    content_id = 711133
    content_json = _runtime_content_json()
    reverse_bind = _runtime_reverse_bind()
    events: list[str] = []
    requests: list[object] = []
    captured_payloads: list[dict[str, object]] = []
    crawler = Nikke(path=tmp_path)
    _install_crawl_page_fakes(monkeypatch, crawler, content_json=content_json, reverse_bind=reverse_bind, events=events)
    _configure_nikke(crawler, runtime_capture_enabled=True, runtime_capture_force_refresh=False, runtime_capture_timeout_seconds=12.5)

    async def _fake_capture(request: object) -> dict[str, object]:
        events.append('capture')
        assert all('atlas_textures' in model['urls'] for model in request.models)
        requests.append(request)
        payload = _runtime_capture_payload(content_id, [dict(model) for model in request.models])
        payload['runtime'] = {
            'requested_live2d_keys': ['main', 'back'],
            'matched_live2d_keys': ['main', 'back'],
            'container_count': 2,
            'raw_browser_state': {'path': '/var/lib/chromium-profile'},
        }
        payload['warnings'] = ['safe warning', {'raw': 'object'}, 'path /var/lib/chromium-profile']
        layers = payload['layers']
        assert isinstance(layers, list)
        first_layer = layers[0]
        assert isinstance(first_layer, dict)
        first_layer['raw_container'] = {'class_name': 'spine-player-container', 'style': 'z-index: 9'}
        captured_payloads.append(payload)
        return payload

    monkeypatch.setattr(nikke_module, 'capture_gamekee_runtime_layers', _fake_capture)

    root = asyncio.run(crawler._crawl_page(client=object(), tj_list_row={'title': 'Test'}, content_id=content_id))

    saved_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    saved_character = json.loads((root / 'character.json').read_text(encoding='utf-8'))
    saved_capture = json.loads((root / 'raw/live2d-layer-capture.json').read_text(encoding='utf-8'))
    assert len(requests) == 1
    assert requests[0].timeout_ms == expected_timeout_ms
    assert saved_capture == captured_payloads[0]
    assert saved_manifest['live2d_models'][0]['layer_order'] == expected_main_layer_order
    assert saved_manifest['live2d_models'][1]['layer_order'] == 1
    assert saved_character['live2d_models'] == saved_manifest['live2d_models']
    assert len(saved_capture['layers']) == expected_layer_count
    assert saved_capture['layers'][0]['raw_container']['class_name'] == 'spine-player-container'
    assert saved_capture['status'] == 'success'
    assert saved_manifest['live2d_layer_capture']['status'] == 'success'
    assert saved_manifest['live2d_layer_capture']['action'] == 'captured'
    assert saved_manifest['live2d_layer_capture']['fingerprint'] == saved_capture['fingerprint']
    assert saved_manifest['live2d_layer_capture']['capture_hash'] == _capture_hash(saved_capture)
    assert all('raw_container' not in layer for layer in saved_manifest['live2d_layer_capture']['layers'])
    assert saved_manifest['live2d_layer_capture']['runtime'] == {
        'requested_live2d_keys': ['main', 'back'],
        'matched_live2d_keys': ['main', 'back'],
        'container_count': expected_layer_count,
    }
    assert saved_manifest['live2d_layer_capture']['warnings'] == ['safe warning', 'path <path>']
    assert saved_character['live2d_layer_capture'] == saved_manifest['live2d_layer_capture']
    assert events == ['expand', 'capture', 'process', 'complete']


def test_crawl_page_writes_failed_layer_capture_summary_without_blocking_manifest(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    content_id = 711133
    content_json = _runtime_content_json()
    reverse_bind = _runtime_reverse_bind()
    events: list[str] = []
    crawler = Nikke(path=tmp_path)
    _install_crawl_page_fakes(monkeypatch, crawler, content_json=content_json, reverse_bind=reverse_bind, events=events)
    _configure_nikke(crawler, runtime_capture_enabled=True, runtime_capture_force_refresh=False, runtime_capture_timeout_seconds=60.0)

    async def _failing_capture(_request: object) -> dict[str, object]:
        events.append('capture')
        message = 'browser unavailable at /var/lib/chromium-profile\nwith details'
        raise RuntimeError(message)

    monkeypatch.setattr(nikke_module, 'capture_gamekee_runtime_layers', _failing_capture)

    root = asyncio.run(crawler._crawl_page(client=object(), tj_list_row={'title': 'Test'}, content_id=content_id))

    saved_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    saved_character = json.loads((root / 'character.json').read_text(encoding='utf-8'))
    summary = saved_manifest['live2d_layer_capture']
    assert summary['status'] == 'failed'
    assert summary['reason'] == 'runtime_capture_failed'
    assert summary['retryable'] is True
    assert summary['error_class'] == 'RuntimeError'
    assert summary['error_message'] == 'browser unavailable at <path> with details'
    assert summary['fingerprint']
    assert saved_character['live2d_layer_capture'] == summary
    assert not (root / 'raw/live2d-layer-capture.json').exists()
    assert all('layer_order' not in model for model in saved_manifest['live2d_models'])
    assert events == ['expand', 'capture', 'process', 'complete']


def test_crawl_page_writes_skipped_layer_capture_summary_when_runtime_disabled(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    content_id = 711133
    content_json = _runtime_content_json()
    reverse_bind = _runtime_reverse_bind()
    events: list[str] = []
    crawler = Nikke(path=tmp_path)
    _install_crawl_page_fakes(monkeypatch, crawler, content_json=content_json, reverse_bind=reverse_bind, events=events)
    _configure_nikke(crawler, runtime_capture_enabled=False, runtime_capture_force_refresh=False, runtime_capture_timeout_seconds=60.0)

    async def _unexpected_capture(_request: object) -> dict[str, object]:
        raise AssertionError

    monkeypatch.setattr(nikke_module, 'capture_gamekee_runtime_layers', _unexpected_capture)

    root = asyncio.run(crawler._crawl_page(client=object(), tj_list_row={'title': 'Test'}, content_id=content_id))

    saved_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    saved_character = json.loads((root / 'character.json').read_text(encoding='utf-8'))
    summary = saved_manifest['live2d_layer_capture']
    assert summary['status'] == 'skipped'
    assert summary['reason'] == 'runtime_capture_disabled'
    assert summary['retryable'] is False
    assert summary['fingerprint']
    assert saved_character['live2d_layer_capture'] == summary
    assert saved_character['live2d_models'] == saved_manifest['live2d_models']
    assert not (root / 'raw/live2d-layer-capture.json').exists()
    assert events == ['expand', 'process', 'complete']


def test_crawl_page_does_not_backfill_raw_capture_when_runtime_not_allowed(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    content_id = 711133
    content_json = _runtime_content_json()
    reverse_bind = _runtime_reverse_bind()
    _assets, models = extract_resources(content_json=content_json, tj_list_row={'title': 'Test'}, reverse_bind=reverse_bind)
    root = tmp_path / '711133 - Test'
    raw_capture_path = root / 'raw/live2d-layer-capture.json'
    raw_capture_path.parent.mkdir(parents=True)
    previous_capture = _runtime_capture_payload(content_id, models)
    raw_capture_path.write_text(json.dumps(previous_capture), encoding='utf-8')
    events: list[str] = []
    crawler = Nikke(path=tmp_path)
    _install_crawl_page_fakes(monkeypatch, crawler, content_json=content_json, reverse_bind=reverse_bind, events=events)
    _configure_nikke(crawler, runtime_capture_enabled=True, runtime_capture_force_refresh=False, runtime_capture_timeout_seconds=60.0)

    async def _unexpected_capture(_request: object) -> dict[str, object]:
        raise AssertionError

    monkeypatch.setattr(nikke_module, 'capture_gamekee_runtime_layers', _unexpected_capture)

    asyncio.run(
        crawler._crawl_page(
            client=object(),
            tj_list_row={'title': 'Test'},
            content_id=content_id,
            allow_runtime_capture=False,
        ),
    )

    saved_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    summary = saved_manifest['live2d_layer_capture']
    assert summary['status'] == 'skipped'
    assert summary['reason'] == 'runtime_capture_not_allowed'
    assert all('layer_order' not in model for model in saved_manifest['live2d_models'])
    assert json.loads(raw_capture_path.read_text(encoding='utf-8')) == previous_capture
    assert events == ['expand', 'process', 'complete']


def test_crawl_page_preserves_committed_manifest_layer_metadata_when_runtime_not_allowed(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    expected_main_layer_order = 2
    content_id = 711133
    content_json = _runtime_content_json()
    reverse_bind = _runtime_reverse_bind()
    _assets, previous_models = extract_resources(content_json=content_json, tj_list_row={'title': 'Test'}, reverse_bind=reverse_bind)
    previous_capture = _runtime_capture_payload(content_id, previous_models)
    merge_report = merge_live2d_layer_captures(previous_models, previous_capture, content_id=content_id, dry_run=False)
    previous_summary = layer_metadata_module.layer_capture_manifest_summary(previous_capture, merge_report)
    root = tmp_path / '711133 - Test'
    root.mkdir(parents=True)
    (root / 'manifest.json').write_text(
        json.dumps(
            {
                'content_id': content_id,
                'title': 'Test',
                'live2d_models': previous_models,
                'live2d_layer_capture': previous_summary,
            },
        ),
        encoding='utf-8',
    )
    events: list[str] = []
    crawler = Nikke(path=tmp_path)
    _install_crawl_page_fakes(monkeypatch, crawler, content_json=content_json, reverse_bind=reverse_bind, events=events)
    _configure_nikke(crawler, runtime_capture_enabled=True, runtime_capture_force_refresh=False, runtime_capture_timeout_seconds=60.0)

    async def _unexpected_capture(_request: object) -> dict[str, object]:
        raise AssertionError

    monkeypatch.setattr(nikke_module, 'capture_gamekee_runtime_layers', _unexpected_capture)

    asyncio.run(
        crawler._crawl_page(
            client=object(),
            tj_list_row={'title': 'Test'},
            content_id=content_id,
            allow_runtime_capture=False,
        ),
    )

    saved_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    saved_character = json.loads((root / 'character.json').read_text(encoding='utf-8'))
    assert saved_manifest['live2d_models'][0]['layer_order'] == expected_main_layer_order
    assert saved_manifest['live2d_models'][1]['is_primary'] is False
    assert saved_character['live2d_models'] == saved_manifest['live2d_models']
    assert saved_manifest['live2d_layer_capture']['status'] == 'success'
    assert saved_manifest['live2d_layer_capture']['action'] == 'preserved'
    assert saved_manifest['live2d_layer_capture']['reason'] == 'runtime_capture_not_allowed'
    assert not (root / 'raw/live2d-layer-capture.json').exists()
    assert events == ['expand', 'process', 'complete']


def test_update_disables_runtime_capture_for_regular_crawler_cron(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    content_id = 711133
    crawler = Nikke(path=tmp_path)
    allow_runtime_capture_values: list[bool | None] = []

    @asynccontextmanager
    async def fake_advisory_lock(_name: str) -> AsyncIterator[bool]:
        yield True

    @asynccontextmanager
    async def fake_http_client() -> AsyncIterator[object]:
        yield object()

    async def fake_ensure_schema() -> None:
        return None

    async def fake_fetch_tj_list_rows(_client: object) -> list[dict[str, object]]:
        return [{'content_id': content_id, 'title': 'Test'}]

    async def fake_upsert_list_pages(_rows: list[dict[str, object]]) -> None:
        return None

    async def fake_crawl_page(**kwargs: object) -> Path:
        allow_runtime_capture_values.append(kwargs.get('allow_runtime_capture'))
        return tmp_path / '711133 - Test'

    monkeypatch.setattr(nikke_module.database, 'advisory_lock', fake_advisory_lock)
    monkeypatch.setattr(crawler, '_http_client', fake_http_client)
    monkeypatch.setattr(crawler, '_ensure_schema', fake_ensure_schema)
    monkeypatch.setattr(crawler, '_fetch_tj_list_rows', fake_fetch_tj_list_rows)
    monkeypatch.setattr(crawler, '_upsert_list_pages', fake_upsert_list_pages)
    monkeypatch.setattr(crawler, '_crawl_page', fake_crawl_page)

    asyncio.run(crawler.update())

    assert allow_runtime_capture_values == [False]


def test_crawl_page_ignores_corrupt_previous_layer_capture_without_blocking_manifest(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    content_id = 711133
    content_json = _runtime_content_json()
    reverse_bind = _runtime_reverse_bind()
    root = tmp_path / '711133 - Test'
    raw_capture_path = root / 'raw/live2d-layer-capture.json'
    raw_capture_path.parent.mkdir(parents=True)
    raw_capture_path.write_text('{not-json', encoding='utf-8')
    events: list[str] = []
    crawler = Nikke(path=tmp_path)
    _install_crawl_page_fakes(monkeypatch, crawler, content_json=content_json, reverse_bind=reverse_bind, events=events)
    _configure_nikke(crawler, runtime_capture_enabled=False, runtime_capture_force_refresh=False, runtime_capture_timeout_seconds=60.0)

    async def _unexpected_capture(_request: object) -> dict[str, object]:
        raise AssertionError

    monkeypatch.setattr(nikke_module, 'capture_gamekee_runtime_layers', _unexpected_capture)

    asyncio.run(crawler._crawl_page(client=object(), tj_list_row={'title': 'Test'}, content_id=content_id))

    saved_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    summary = saved_manifest['live2d_layer_capture']
    assert summary['status'] == 'skipped'
    assert summary['reason'] == 'runtime_capture_disabled'
    assert summary['previous_capture_error_class'] == 'JSONDecodeError'
    assert raw_capture_path.read_text(encoding='utf-8') == '{not-json'
    assert events == ['expand', 'process', 'complete']


def test_crawl_page_falls_back_to_manifest_summary_when_raw_capture_is_corrupt(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    expected_main_layer_order = 2
    content_id = 711133
    content_json = _runtime_content_json()
    reverse_bind = _runtime_reverse_bind()
    _assets, models = extract_resources(content_json=content_json, tj_list_row={'title': 'Test'}, reverse_bind=reverse_bind)
    previous_capture = _runtime_capture_payload(content_id, models)
    merge_report = merge_live2d_layer_captures([dict(model) for model in models], previous_capture, content_id=content_id)
    previous_summary = layer_metadata_module.layer_capture_manifest_summary(previous_capture, merge_report)
    root = tmp_path / '711133 - Test'
    raw_capture_path = root / 'raw/live2d-layer-capture.json'
    raw_capture_path.parent.mkdir(parents=True)
    raw_capture_path.write_text('{not-json', encoding='utf-8')
    (root / 'manifest.json').write_text(json.dumps({'live2d_layer_capture': previous_summary}), encoding='utf-8')
    events: list[str] = []
    crawler = Nikke(path=tmp_path)
    _install_crawl_page_fakes(monkeypatch, crawler, content_json=content_json, reverse_bind=reverse_bind, events=events)
    _configure_nikke(crawler, runtime_capture_enabled=False, runtime_capture_force_refresh=False, runtime_capture_timeout_seconds=60.0)

    async def _unexpected_capture(_request: object) -> dict[str, object]:
        raise AssertionError

    monkeypatch.setattr(nikke_module, 'capture_gamekee_runtime_layers', _unexpected_capture)

    asyncio.run(crawler._crawl_page(client=object(), tj_list_row={'title': 'Test'}, content_id=content_id))

    saved_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    summary = saved_manifest['live2d_layer_capture']
    assert saved_manifest['live2d_models'][0]['layer_order'] == expected_main_layer_order
    assert saved_manifest['live2d_models'][1]['layer_order'] == 1
    assert summary['status'] == 'success'
    assert summary['action'] == 'reused'
    assert summary['capture_hash'] == previous_summary['capture_hash']
    assert summary['previous_capture_error_class'] == 'JSONDecodeError'
    assert raw_capture_path.read_text(encoding='utf-8') == '{not-json'
    assert events == ['expand', 'process', 'complete']


def test_crawl_page_reuses_manifest_layer_summary_without_writing_raw_capture(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    content_id = 711133
    content_json = _runtime_content_json()
    reverse_bind = _runtime_reverse_bind()
    _assets, models = extract_resources(content_json=content_json, tj_list_row={'title': 'Test'}, reverse_bind=reverse_bind)
    previous_capture = _runtime_capture_payload(content_id, models)
    merge_report = merge_live2d_layer_captures([dict(model) for model in models], previous_capture, content_id=content_id)
    previous_summary = layer_metadata_module.layer_capture_manifest_summary(previous_capture, merge_report)
    root = tmp_path / '711133 - Test'
    root.mkdir(parents=True)
    (root / 'manifest.json').write_text(json.dumps({'live2d_layer_capture': previous_summary}), encoding='utf-8')
    events: list[str] = []
    crawler = Nikke(path=tmp_path)
    _install_crawl_page_fakes(monkeypatch, crawler, content_json=content_json, reverse_bind=reverse_bind, events=events)
    _configure_nikke(crawler, runtime_capture_enabled=False, runtime_capture_force_refresh=False, runtime_capture_timeout_seconds=60.0)

    async def _unexpected_capture(_request: object) -> dict[str, object]:
        raise AssertionError

    monkeypatch.setattr(nikke_module, 'capture_gamekee_runtime_layers', _unexpected_capture)

    asyncio.run(crawler._crawl_page(client=object(), tj_list_row={'title': 'Test'}, content_id=content_id))

    saved_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    assert saved_manifest['live2d_layer_capture']['status'] == 'success'
    assert saved_manifest['live2d_layer_capture']['action'] == 'reused'
    assert saved_manifest['live2d_layer_capture']['capture_hash'] == previous_summary['capture_hash']
    assert not (root / 'raw/live2d-layer-capture.json').exists()
    assert events == ['expand', 'process', 'complete']


def test_crawl_page_writes_no_multi_full_layer_capture_summary(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    content_id = 711133
    content_json = _runtime_content_json()
    style_data = content_json['styleData']
    assert isinstance(style_data, list)
    first_style = style_data[0]
    assert isinstance(first_style, dict)
    rows = first_style['data']
    assert isinstance(rows, list)
    first_row = rows[0]
    assert isinstance(first_row, list)
    del first_row[2:]
    reverse_bind = _runtime_reverse_bind()
    events: list[str] = []
    crawler = Nikke(path=tmp_path)
    _install_crawl_page_fakes(monkeypatch, crawler, content_json=content_json, reverse_bind=reverse_bind, events=events)
    _configure_nikke(crawler, runtime_capture_enabled=True, runtime_capture_force_refresh=False, runtime_capture_timeout_seconds=60.0)

    async def _unexpected_capture(_request: object) -> dict[str, object]:
        raise AssertionError

    monkeypatch.setattr(nikke_module, 'capture_gamekee_runtime_layers', _unexpected_capture)

    root = asyncio.run(crawler._crawl_page(client=object(), tj_list_row={'title': 'Test'}, content_id=content_id))

    saved_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    saved_character = json.loads((root / 'character.json').read_text(encoding='utf-8'))
    summary = saved_manifest['live2d_layer_capture']
    assert summary['status'] == 'skipped'
    assert summary['reason'] == 'no_multi_full_groups'
    assert summary['retryable'] is False
    assert saved_character['live2d_layer_capture'] == summary
    assert not (root / 'raw/live2d-layer-capture.json').exists()
    assert events == ['expand', 'process', 'complete']


def test_assign_asset_paths_splits_live2d_dir_on_same_basename_collision() -> None:
    assets = {
        'https://cdn.example.com/a/model.atlas': Asset(
            url='https://cdn.example.com/a/model.atlas',
            kind='live2d_atlas',
            contexts=[{'live2d_key': 'same-model'}],
        ),
        'https://cdn.example.com/b/model.atlas': Asset(
            url='https://cdn.example.com/b/model.atlas',
            kind='live2d_atlas',
            contexts=[{'live2d_key': 'same-model'}],
        ),
    }

    assign_asset_paths(assets, content_id=1)
    paths = [asset.local_path for asset in assets.values()]

    assert paths[0] == 'assets/live2d/same-model/model.atlas'
    assert paths[1].startswith('assets/live2d/same-model-')
    assert paths[1].endswith('/model.atlas')


def test_materialize_blob_falls_back_to_copy_when_hardlink_fails(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    blob = tmp_path / 'blob'
    data = b'nikke-asset'
    blob.write_bytes(data)
    destination = tmp_path / 'character/assets/images/file.bin'

    def _fail_link(_src: Path, _dst: Path) -> None:
        raise OSError

    monkeypatch.setattr(nikke_module.os, 'link', _fail_link)

    materialize_blob(
        blob_path=blob,
        destination=destination,
        sha256=hashlib.sha256(data).hexdigest(),
        size=len(data),
    )

    assert destination.read_bytes() == data


def test_register_temp_blob_reuses_existing_hash_blob(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    data = b'same-content'
    sha256 = hashlib.sha256(data).hexdigest()
    existing_relative = Path('_blobs/sha256') / sha256[:2] / sha256
    existing = tmp_path / existing_relative
    existing.parent.mkdir(parents=True)
    existing.write_bytes(data)
    temp = tmp_path / 'download.tmp'
    temp.write_bytes(data)
    queries: list[str] = []

    async def _fake_query_db(sql: str, _params: tuple = ()) -> list[dict[str, object]]:
        queries.append(sql)
        if 'SELECT blob_path' in sql:
            return [{'blob_path': existing_relative.as_posix()}]
        return []

    monkeypatch.setattr(nikke_module.database, 'query_db', _fake_query_db)
    crawler = Nikke(path=tmp_path)

    blob = asyncio.run(
        crawler._register_temp_blob(
            nikke_module.TempBlob(
                path=temp,
                sha256=sha256,
                size=len(data),
                content_type='image/png',
            ),
        ),
    )

    assert blob.path == existing
    assert not temp.exists()
    assert any('last_verified_at' in sql for sql in queries)


def test_completed_blob_for_url_marks_missing_when_db_blob_is_gone(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    updates: list[tuple[str, tuple]] = []

    async def _fake_query_db(sql: str, params: tuple = ()) -> list[dict[str, object]]:
        if 'FROM nikke_assets AS a' in sql:
            return [
                {
                    'sha256': 'a' * 64,
                    'size': 10,
                    'content_type': 'image/png',
                    'blob_path': '_blobs/sha256/aa/' + ('a' * 64),
                },
            ]
        updates.append((sql, params))
        return []

    monkeypatch.setattr(nikke_module.database, 'query_db', _fake_query_db)
    crawler = Nikke(path=tmp_path)

    blob = asyncio.run(crawler._completed_blob_for_url('https://cdn.example.com/missing.png'))

    assert blob is None
    assert any("status = 'missing'" in sql for sql, _params in updates)


def test_process_assets_raises_when_any_asset_failed(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    crawler = Nikke(path=tmp_path)
    assets = {
        ('https://cdn.example.com/a.png', 'image'): Asset(url='https://cdn.example.com/a.png', kind='image'),
        ('https://cdn.example.com/b.png', 'image'): Asset(url='https://cdn.example.com/b.png', kind='image'),
    }

    async def _fake_process_asset(*, client: object, root: Path, asset: Asset) -> bool:
        _ = client, root
        if asset.url.endswith('b.png'):
            asset.status = 'failed'
            asset.error = 'boom'
            return False
        asset.status = 'downloaded'
        return True

    monkeypatch.setattr(crawler, '_process_asset', _fake_process_asset)

    with pytest.raises(AssetProcessingError, match='1 Nikke assets failed'):
        asyncio.run(crawler._process_assets(client=object(), root=tmp_path, assets=assets))


def test_process_assets_limits_db_backed_asset_processing(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    crawler = Nikke(path=tmp_path)
    assets = {
        (f'https://cdn.example.com/{index}.png', 'image'): Asset(url=f'https://cdn.example.com/{index}.png', kind='image')
        for index in range(12)
    }
    active = 0
    max_active = 0

    async def _fake_process_asset(*, client: object, root: Path, asset: Asset) -> bool:
        nonlocal active, max_active
        _ = client, root
        active += 1
        max_active = max(max_active, active)
        await asyncio.sleep(0.01)
        asset.status = 'downloaded'
        active -= 1
        return True

    monkeypatch.setattr(crawler, '_process_asset', _fake_process_asset)

    asyncio.run(crawler._process_assets(client=object(), root=tmp_path, assets=assets))

    assert max_active == nikke_module._ASSET_PROCESS_CONCURRENCY


def test_upsert_page_fetch_start_clears_completed_at(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    captured: list[str] = []

    async def _fake_query_db(sql: str, _params: tuple = ()) -> list[dict[str, object]]:
        captured.append(sql)
        return []

    monkeypatch.setattr(nikke_module.database, 'query_db', _fake_query_db)
    crawler = Nikke(path=tmp_path)

    asyncio.run(
        crawler._upsert_page_fetch_start(
            content_id=1,
            name='Nikke',
            row={},
            detail_response={'data': {'title': 'Nikke'}},
            manifest_path=tmp_path / 'manifest.json',
            hash_value='abc',
        ),
    )

    assert any('completed_at = NULL' in sql for sql in captured)


def test_replace_page_assets_and_complete_uses_single_transaction(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    captured: list[list[str]] = []

    async def _fake_transaction(statements: list[tuple[str, tuple]]) -> list[list[dict[str, object]]]:
        captured.append([sql for sql, _params in statements])
        return [[] for _sql, _params in statements]

    monkeypatch.setattr(nikke_module.database, 'query_db_transaction', _fake_transaction)
    crawler = Nikke(path=tmp_path)
    assets = {
        ('https://cdn.example.com/a.png', 'image'): Asset(
            url='https://cdn.example.com/a.png',
            kind='image',
            local_path='assets/images/a.png',
            contexts=[{'field': 'icon'}],
        ),
    }

    asyncio.run(crawler._replace_page_assets_and_mark_completed(content_id=1, assets=assets))

    assert len(captured) == 1
    assert captured[0][0].startswith('DELETE FROM nikke_page_assets')
    assert any('INSERT INTO nikke_page_assets' in sql for sql in captured[0])
    assert captured[0][-1].startswith('UPDATE nikke_pages SET completed_at = NOW()')


def test_expand_atlas_textures_uses_blob_pipeline_not_direct_get(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    atlas = tmp_path / '_blobs/sha256/aa/atlas'
    atlas.parent.mkdir(parents=True)
    atlas.write_text('texture.png\nsize: 1,1\n', encoding='utf-8')
    crawler = Nikke(path=tmp_path)
    assets = {
        ('https://cdn.example.com/model.atlas', 'live2d_atlas'): Asset(
            url='https://cdn.example.com/model.atlas',
            kind='live2d_atlas',
            local_path='assets/live2d/model/model.atlas',
            contexts=[{'live2d_key': 'model'}],
        ),
    }
    models = [{'urls': {'atlas': 'https://cdn.example.com/model.atlas'}, 'live2d_key': 'model'}]
    processed: list[str] = []

    async def _fake_process_asset(*, client: object, root: Path, asset: Asset) -> bool:
        _ = client, root
        processed.append(asset.url)
        asset.status = 'downloaded'
        return True

    async def _fake_completed_blob_for_url(_url: str) -> nikke_module.BlobRef:
        return nikke_module.BlobRef(sha256='a' * 64, size=atlas.stat().st_size, content_type='text/plain', path=atlas)

    async def _unexpected_request(*_args: object, **_kwargs: object) -> None:
        raise AssertionError

    monkeypatch.setattr(crawler, '_process_asset', _fake_process_asset)
    monkeypatch.setattr(crawler, '_completed_blob_for_url', _fake_completed_blob_for_url)
    monkeypatch.setattr(crawler, '_request', _unexpected_request)

    asyncio.run(crawler._expand_atlas_textures(client=object(), root=tmp_path, assets=assets, live2d_models=models))

    assert processed == ['https://cdn.example.com/model.atlas']
    assert ('https://cdn.example.com/texture.png', 'live2d_texture') in assets
    assert models[0]['urls']['atlas_textures'] == ['https://cdn.example.com/texture.png']
