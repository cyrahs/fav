# ruff: noqa: INP001, S101, PLR0913, PLR2004, SLF001

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

import src.api.azurlane as azurlane_module
from src.api.azurlane import AzurLaneAssetNotFoundError, AzurLaneCharacterNotFoundError, AzurLaneLibrary

_STATIC_CHARACTER_PREFIX = '/static/azurlane/javelin%20-%20Javelin'


def _write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, ensure_ascii=False), encoding='utf-8')


def _write_asset(path: Path, body: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(body)


def _asset(
    path: str,
    *,
    body: bytes,
    kind: str,
    context: dict[str, Any],
    sha256: str = 'a' * 64,
    content_type: str = 'application/octet-stream',
) -> dict[str, Any]:
    return {
        'url': f'https://static.example/live2d/azurlane/{Path(path).name}',
        'normalized_url': f'https://static.example/live2d/azurlane/{Path(path).name}',
        'downloaded_url': f'https://static.example/live2d/azurlane/{Path(path).name}',
        'fallback_url': f'https://fallback.example/live2d/azurlane/{Path(path).name}',
        'source_urls': {
            'primary': f'https://static.example/live2d/azurlane/{Path(path).name}',
            'fallback': f'https://fallback.example/live2d/azurlane/{Path(path).name}',
            'downloaded': f'https://static.example/live2d/azurlane/{Path(path).name}',
        },
        'kind': kind,
        'local_path': path,
        'original_filename': Path(path).name,
        'sha256': sha256,
        'size': len(body),
        'content_type': content_type,
        'status': 'downloaded',
        'available': True,
        'failed_count': 0,
        'error': '',
        'last_attempt_at': None,
        'next_retry_at': None,
        'last_seen_at': '2026-05-19T00:00:00+00:00',
        'context_hashes': [f'hash-{kind}'],
        'contexts': [context],
    }


def _model_context(*, model_id: str, model_type: str, costume_key: str, field: str) -> dict[str, Any]:
    return {
        'model_id': model_id,
        'model_type': model_type,
        'character_key': 'javelin',
        'costume_key': costume_key,
        'catalog_source': 'merged' if model_type == 'live2d' else 'l2d.su',
        'source_model_url': f'https://static.example/live2d/azurlane/{costume_key}',
        'fallback_model_url': f'https://fallback.example/live2d/azurlane/{costume_key}',
        'live2d_field' if model_type == 'live2d' else 'spine_field': field,
    }


def _create_azurlane_fixture(root: Path) -> Path:
    character_root = root / 'javelin - Javelin'
    files = {
        'assets/live2d/javelin/javelin.model3.json': b'{"Version":3}',
        'assets/live2d/javelin/javelin.moc3': b'moc',
        'assets/live2d/javelin/textures/texture_00.webp': b'texture',
        'assets/spine/javelin_spine/javelin_spine.skel': b'skel',
        'assets/spine/javelin_spine/javelin_spine.atlas': b'atlas',
        'assets/spine/javelin_spine/javelin_spine.webp': b'spine-texture',
    }
    for relative_path, body in files.items():
        _write_asset(character_root / relative_path, body)

    live2d_model_id = 'azurlane:live2d:javelin:javelin'
    spine_model_id = 'azurlane:spine:javelin:javelin_spine'
    live2d_assets = [
        _asset(
            'assets/live2d/javelin/javelin.model3.json',
            body=files['assets/live2d/javelin/javelin.model3.json'],
            kind='live2d.model3',
            context=_model_context(model_id=live2d_model_id, model_type='live2d', costume_key='javelin', field='model3'),
            sha256='b' * 64,
            content_type='application/json',
        ),
        _asset(
            'assets/live2d/javelin/javelin.moc3',
            body=files['assets/live2d/javelin/javelin.moc3'],
            kind='live2d.moc3',
            context=_model_context(model_id=live2d_model_id, model_type='live2d', costume_key='javelin', field='moc3'),
        ),
        _asset(
            'assets/live2d/javelin/textures/texture_00.webp',
            body=files['assets/live2d/javelin/textures/texture_00.webp'],
            kind='live2d.texture',
            context=_model_context(model_id=live2d_model_id, model_type='live2d', costume_key='javelin', field='texture'),
            sha256='c' * 64,
            content_type='image/webp',
        ),
        _asset(
            'assets/live2d/javelin/escape.txt',
            body=b'outside',
            kind='live2d.text',
            context=_model_context(model_id=live2d_model_id, model_type='live2d', costume_key='javelin', field='text'),
            content_type='text/plain',
        ),
    ]
    spine_assets = [
        _asset(
            'assets/spine/javelin_spine/javelin_spine.skel',
            body=files['assets/spine/javelin_spine/javelin_spine.skel'],
            kind='spine.skel',
            context=_model_context(model_id=spine_model_id, model_type='spine', costume_key='javelin_spine', field='skel'),
        ),
        _asset(
            'assets/spine/javelin_spine/javelin_spine.atlas',
            body=files['assets/spine/javelin_spine/javelin_spine.atlas'],
            kind='spine.atlas',
            context=_model_context(model_id=spine_model_id, model_type='spine', costume_key='javelin_spine', field='atlas'),
        ),
        _asset(
            'assets/spine/javelin_spine/javelin_spine.webp',
            body=files['assets/spine/javelin_spine/javelin_spine.webp'],
            kind='spine.texture',
            context=_model_context(model_id=spine_model_id, model_type='spine', costume_key='javelin_spine', field='texture'),
            content_type='image/webp',
        ),
    ]
    live2d_model = {
        'model_id': live2d_model_id,
        'type': 'live2d',
        'source': 'merged',
        'character_key': 'javelin',
        'costume': {'key': 'javelin', 'id': 1, 'name_zh': '默认', 'name_en': 'Default'},
        'source_urls': {
            'primary': 'https://static.example/live2d/azurlane/javelin/javelin.model3.json',
            'fallback': 'https://fallback.example/live2d/azurlane/javelin/javelin.model3.json',
            'display_info': '',
        },
        'availability': {
            'source_state': 'available',
            'archive_state': 'complete',
            'asset_status_counts': {'downloaded': 4},
            'available_asset_count': 4,
            'asset_count': 4,
            'completed_at': '2026-05-19T00:00:00+00:00',
        },
        'source_metadata': {'id': live2d_model_id},
        'fetched_at': '2026-05-19T00:00:00+00:00',
        'completed_at': '2026-05-19T00:00:00+00:00',
        'assets': live2d_assets,
        'asset_counts': {'live2d.model3': 1, 'live2d.moc3': 1, 'live2d.texture': 1, 'live2d.text': 1},
    }
    spine_model = {
        'model_id': spine_model_id,
        'type': 'spine',
        'source': 'l2d.su',
        'character_key': 'javelin',
        'costume': {'key': 'javelin_spine', 'id': 2, 'name_zh': '动态', 'name_en': 'Dynamic'},
        'source_urls': {
            'primary': 'https://static.example/live2d/azurlane/javelin_spine',
            'fallback': '',
            'display_info': '',
        },
        'availability': {
            'source_state': 'available',
            'archive_state': 'complete',
            'asset_status_counts': {'downloaded': 3},
            'available_asset_count': 3,
            'asset_count': 3,
            'completed_at': '2026-05-19T00:00:00+00:00',
        },
        'source_metadata': {'id': spine_model_id},
        'fetched_at': '2026-05-19T00:00:00+00:00',
        'completed_at': '2026-05-19T00:00:00+00:00',
        'assets': spine_assets,
        'asset_counts': {'spine.skel': 1, 'spine.atlas': 1, 'spine.texture': 1},
    }
    manifest = {
        'schema_version': 1,
        'source': 'azurlane',
        'character_key': 'javelin',
        'source_id': 1,
        'name_zh': '标枪',
        'name_en': 'Javelin',
        'directory_name': 'javelin - Javelin',
        'manifest_path': (character_root / 'manifest.json').as_posix(),
        'source_metadata': {'model_ids': [live2d_model_id, spine_model_id], 'sources': ['merged', 'l2d.su']},
        'fetched_at': '2026-05-19T00:00:00+00:00',
        'completed_at': '2026-05-19T00:00:00+00:00',
        'active': True,
        'model_counts': {'live2d': 1, 'spine': 1, 'total': 2},
        'asset_counts': {
            'live2d.model3': 1,
            'live2d.moc3': 1,
            'live2d.texture': 1,
            'live2d.text': 1,
            'spine.skel': 1,
            'spine.atlas': 1,
            'spine.texture': 1,
        },
        'models': [spine_model, live2d_model],
    }
    _write_json(character_root / 'manifest.json', manifest)
    _write_json(
        character_root / 'character.json',
        {
            'schema_version': 1,
            'source': 'azurlane',
            'character_key': 'javelin',
            'name_zh': '标枪',
            'name_en': 'Javelin',
            'models': [
                {
                    'model_id': live2d_model_id,
                    'type': 'live2d',
                    'source': 'merged',
                    'costume': live2d_model['costume'],
                    'source_urls': live2d_model['source_urls'],
                    'availability': live2d_model['availability'],
                },
            ],
        },
    )
    return root


def test_azurlane_library_discovers_manifests_and_uses_summary_cache(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    root = _create_azurlane_fixture(tmp_path / 'azurlane')
    _write_json(root / '_source' / 'manifest.json', {'character_key': 'ignored-source'})
    external = tmp_path / 'external'
    _write_json(external / 'manifest.json', {'character_key': 'outside', 'name_en': 'Outside'})
    (root / 'outside - Outside').symlink_to(external, target_is_directory=True)

    expected = AzurLaneLibrary(root).list_characters()
    assert (root / '_api' / 'summary-cache.json').is_file()

    assert [character['character_key'] for character in expected] == ['javelin']
    summary = expected[0]
    assert summary['display_name'] == 'Javelin'
    assert summary['model_counts'] == {'live2d': 1, 'spine': 1, 'total': 2}
    assert summary['source_counts'] == {'l2d.su': 1, 'merged': 1}
    assert summary['tags'] == {'model_types': 'live2d, spine', 'sources': 'l2d.su, merged'}
    assert summary['representative_asset']['url'] == f'{_STATIC_CHARACTER_PREFIX}/assets/live2d/javelin/textures/texture_00.webp?v=' + (
        'c' * 64
    )

    original_read_json_object = azurlane_module._read_json_object

    def _fail_manifest_read(path: Path) -> dict[str, Any]:
        if path.name == 'manifest.json':
            msg = f'Unexpected manifest rebuild: {path}'
            raise AssertionError(msg)
        return original_read_json_object(path)

    monkeypatch.setattr(azurlane_module, '_read_json_object', _fail_manifest_read)

    assert AzurLaneLibrary(root).list_characters() == expected


def test_get_azurlane_character_shapes_models_and_asset_urls(tmp_path: Path) -> None:
    root = _create_azurlane_fixture(tmp_path / 'azurlane')
    library = AzurLaneLibrary(root)

    payload = library.get_character('javelin')

    assert payload['character_key'] == 'javelin'
    assert payload['title'] == 'Javelin'
    assert payload['name_zh'] == '标枪'
    assert payload['model_count'] == 2
    assert [model['model_id'] for model in payload['models']] == [
        'azurlane:live2d:javelin:javelin',
        'azurlane:spine:javelin:javelin_spine',
    ]
    live2d = payload['live2d_models'][0]
    assert live2d['costume'] == {'key': 'javelin', 'id': 1, 'name_zh': '默认', 'name_en': 'Default'}
    assert live2d['source_urls']['fallback'] == 'https://fallback.example/live2d/azurlane/javelin/javelin.model3.json'
    assert live2d['availability']['archive_state'] == 'complete'
    assert live2d['files']['model3']['path'] == 'assets/live2d/javelin/javelin.model3.json'
    assert live2d['files']['model3']['url'] == f'{_STATIC_CHARACTER_PREFIX}/assets/live2d/javelin/javelin.model3.json?v=' + (
        'b' * 64
    )
    assert live2d['files']['textures'][0]['content_type'] == 'image/webp'
    assert live2d['files']['textures'][0]['available'] is True
    assert live2d['files']['textures'][0]['field'] == 'texture'
    assert live2d['files']['text'][0]['path'] == 'assets/live2d/javelin/escape.txt'
    assert payload['spine_models'][0]['files']['atlas']['path'] == 'assets/spine/javelin_spine/javelin_spine.atlas'
    assert len(payload['assets']) == 7

    with pytest.raises(AzurLaneCharacterNotFoundError):
        library.get_character('missing')


def test_azurlane_asset_resolution_is_static_and_root_safe(tmp_path: Path) -> None:
    root = _create_azurlane_fixture(tmp_path / 'azurlane')
    outside = tmp_path / 'outside.txt'
    outside.write_text('outside', encoding='utf-8')
    unsafe_link = root / 'javelin - Javelin' / 'assets/live2d/javelin/escape.txt'
    unsafe_link.symlink_to(outside)

    library = AzurLaneLibrary(root)
    asset_file = library.get_asset_file('javelin', 'assets/live2d/javelin/textures/texture_00.webp')

    assert asset_file.path == (root / 'javelin - Javelin/assets/live2d/javelin/textures/texture_00.webp').resolve()
    assert asset_file.content_type == 'image/webp'
    assert asset_file.headers['Cache-Control'] == 'public, max-age=31536000, immutable'
    assert asset_file.headers['ETag'] == '"' + ('c' * 64) + '"'

    for unsafe_path in ('../outside.txt', '/assets/live2d/javelin/javelin.moc3', 'assets/../outside.txt', 'assets/%2e%2e/outside.txt'):
        with pytest.raises(AzurLaneAssetNotFoundError):
            library.get_asset_file('javelin', unsafe_path)

    with pytest.raises(AzurLaneAssetNotFoundError):
        library.get_asset_file('javelin', 'assets/live2d/javelin/escape.txt')

    escape_asset = next(
        asset for asset in library.get_character('javelin')['assets'] if asset['path'] == 'assets/live2d/javelin/escape.txt'
    )
    assert escape_asset['available'] is False
    assert escape_asset['url'] == f'{_STATIC_CHARACTER_PREFIX}/assets/live2d/javelin/escape.txt?v=' + ('a' * 64)
