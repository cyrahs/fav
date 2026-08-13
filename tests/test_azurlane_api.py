# ruff: noqa: INP001, S101, S105, PLR0913, PLR2004, SLF001

from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import pytest
from fastapi.testclient import TestClient

import src.api.azurlane as azurlane_module
from src.api.app import create_app
from src.api.azurlane import AzurLaneAssetNotFoundError, AzurLaneCharacterNotFoundError, AzurLaneLibrary
from src.api.service import FavApiService

_VALID_TOKEN = 'token-for-tests'
_FIXED_NOW = datetime(2026, 5, 19, 0, 0, tzinfo=UTC)
_STATIC_CHARACTER_PREFIX = '/static/azurlane/javelin%20-%20Javelin'


class _RuntimeService:
    def close(self) -> None:
        return None


def _auth_headers(token: str = _VALID_TOKEN) -> dict[str, str]:
    return {'Authorization': f'Bearer {token}'}


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
            kind='live2d.audio',
            context=_model_context(model_id=live2d_model_id, model_type='live2d', costume_key='javelin', field='audio'),
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
        'asset_counts': {'live2d.model3': 1, 'live2d.moc3': 1, 'live2d.texture': 1, 'live2d.audio': 1},
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
            'live2d.audio': 1,
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


def _add_minimal_azurlane_character(
    root: Path,
    *,
    character_key: str,
    name_en: str,
    completed_at: str,
    source_id: int,
) -> None:
    _write_json(
        root / f'{character_key} - {name_en}' / 'manifest.json',
        {
            'schema_version': 1,
            'source': 'azurlane',
            'character_key': character_key,
            'source_id': source_id,
            'name_zh': '',
            'name_en': name_en,
            'source_metadata': {'sources': ['l2d.su']},
            'fetched_at': completed_at,
            'completed_at': completed_at,
            'active': True,
            'model_counts': {'live2d': 0, 'spine': 0, 'total': 0},
            'asset_counts': {},
            'models': [],
        },
    )


def _add_painting_azurlane_character(root: Path, *, default_skin_id: int | None) -> None:
    """Character with two painting skins, each carrying a square icon, for summary icon selection."""
    character_root = root / 'yuzhang - Yuzhang'
    models = []
    for costume_id, costume_key in ((21, 'yuzhang'), (22, 'yuzhang_2')):
        icon_path = f'assets/painting/{costume_key}/squareicon/{costume_key}.webp'
        _write_asset(character_root / icon_path, b'icon')
        model_id = f'azurlane:painting:yuzhang:{costume_key}'
        models.append(
            {
                'model_id': model_id,
                'type': 'painting',
                'source': 'l2d.su',
                'character_key': 'yuzhang',
                'costume': {'key': costume_key, 'id': costume_id, 'name_zh': '', 'name_en': costume_key},
                'source_urls': {'primary': '', 'fallback': '', 'display_info': ''},
                'availability': {'archive_state': 'complete'},
                'source_metadata': {'id': model_id},
                'assets': [
                    _asset(
                        icon_path,
                        body=b'icon',
                        kind='icon.square',
                        context={'model_id': model_id, 'model_type': 'painting', 'costume_key': costume_key},
                        sha256=f'{costume_id}' * 32,
                        content_type='image/webp',
                    ),
                ],
                'asset_counts': {'icon.square': 1},
            },
        )

    metadata: dict[str, Any] = {'sources': ['l2d.su'], 'skin_series': ['Default', 'Swimsuits']}
    if default_skin_id is not None:
        metadata['default_skin_id'] = default_skin_id
    _write_json(
        character_root / 'manifest.json',
        {
            'schema_version': 1,
            'source': 'azurlane',
            'character_key': 'yuzhang',
            'source_id': 2,
            'name_zh': '',
            'name_en': 'Yuzhang',
            'source_metadata': metadata,
            'active': True,
            'model_counts': {'live2d': 0, 'spine': 0, 'painting': 2, 'total': 2},
            'asset_counts': {'icon.square': 2},
            'models': models,
        },
    )


def test_azurlane_summary_icon_prefers_the_default_skin_square_icon(tmp_path: Path) -> None:
    _add_painting_azurlane_character(tmp_path, default_skin_id=22)
    library = AzurLaneLibrary(tmp_path)

    character = next(item for item in library.list_characters() if item['character_key'] == 'yuzhang')

    assert character['icon']['kind'] == 'icon.square'
    assert character['icon']['available'] is True
    assert 'yuzhang_2/squareicon/yuzhang_2.webp' in character['icon']['url']
    assert character['source_metadata']['skin_series'] == ['Default', 'Swimsuits']


def test_azurlane_summary_icon_falls_back_when_no_default_skin_is_recorded(tmp_path: Path) -> None:
    _add_painting_azurlane_character(tmp_path, default_skin_id=None)
    library = AzurLaneLibrary(tmp_path)

    character = next(item for item in library.list_characters() if item['character_key'] == 'yuzhang')

    # Without a default skin id any downloaded square icon is still a usable avatar.
    assert character['icon']['kind'] == 'icon.square'
    assert character['icon']['available'] is True


def _build_service(root: Path) -> FavApiService:
    return FavApiService(
        dsn='postgresql://db.local/fav',
        token=_VALID_TOKEN,
        hanime1_video_fetcher=lambda _dsn: [],
        now_provider=lambda: _FIXED_NOW,
        job_provider=list,
        runtime_service=_RuntimeService(),
        azurlane_library=AzurLaneLibrary(root),
    )


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
    assert live2d['files']['model3']['url'] == f'{_STATIC_CHARACTER_PREFIX}/assets/live2d/javelin/javelin.model3.json?v=' + ('b' * 64)
    assert live2d['files']['textures'][0]['content_type'] == 'image/webp'
    assert live2d['files']['textures'][0]['available'] is True
    assert live2d['files']['textures'][0]['field'] == 'texture'
    assert live2d['files']['audio'][0]['path'] == 'assets/live2d/javelin/escape.txt'
    assert payload['spine_models'][0]['files']['atlas']['path'] == 'assets/spine/javelin_spine/javelin_spine.atlas'
    assert len(payload['assets']) == 7

    with pytest.raises(AzurLaneCharacterNotFoundError):
        library.get_character('missing')


def test_azurlane_character_routes_require_authorization(tmp_path: Path) -> None:
    root = _create_azurlane_fixture(tmp_path / 'azurlane')
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        missing_auth_response = client.get('/api/v2/azurlane/characters')
        invalid_token_response = client.get('/api/v2/azurlane/characters', headers=_auth_headers('wrong-token'))

    assert missing_auth_response.status_code == 401
    assert missing_auth_response.headers['WWW-Authenticate'] == 'Bearer realm="fav-api"'
    assert missing_auth_response.json()['error']['code'] == 'missing_authorization'
    assert invalid_token_response.status_code == 403
    assert invalid_token_response.json()['error']['code'] == 'invalid_token'


def test_list_azurlane_characters_supports_pagination_and_search(tmp_path: Path) -> None:
    root = _create_azurlane_fixture(tmp_path / 'azurlane')
    _add_minimal_azurlane_character(
        root,
        character_key='z23',
        name_en='Z23',
        completed_at='2026-05-19T00:01:00+00:00',
        source_id=23,
    )
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        page_response = client.get('/api/v2/azurlane/characters?limit=1&offset=1', headers=_auth_headers())
        search_response = client.get('/api/v2/azurlane/characters?q=javelin', headers=_auth_headers())
        source_response = client.get('/api/v2/azurlane/characters?q=merged', headers=_auth_headers())

    assert page_response.status_code == 200
    assert page_response.json()['total'] == 2
    assert page_response.json()['limit'] == 1
    assert page_response.json()['offset'] == 1
    assert [item['character_key'] for item in page_response.json()['items']] == ['z23']
    assert search_response.status_code == 200
    assert search_response.json()['total'] == 1
    assert search_response.json()['items'][0]['character_key'] == 'javelin'
    assert source_response.status_code == 200
    assert source_response.json()['total'] == 1
    assert source_response.json()['items'][0]['character_key'] == 'javelin'


def test_list_azurlane_sidebar_characters_returns_etagged_light_payload(tmp_path: Path) -> None:
    root = _create_azurlane_fixture(tmp_path / 'azurlane')
    _add_minimal_azurlane_character(
        root,
        character_key='z23',
        name_en='Z23',
        completed_at='2026-05-19T00:01:00+00:00',
        source_id=23,
    )
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/azurlane/sidebar/characters', headers=_auth_headers())
        query_response = client.get('/api/v2/azurlane/sidebar/characters?q=标枪', headers=_auth_headers())
        cached_response = client.get(
            '/api/v2/azurlane/sidebar/characters',
            headers={**_auth_headers(), 'If-None-Match': response.headers['etag']},
        )

    assert response.status_code == 200
    assert response.headers['cache-control'] == 'public, max-age=300'
    assert response.headers['etag'].startswith('"')
    payload = response.json()
    assert payload['total'] == 2
    assert [item['character_key'] for item in payload['items']] == ['z23', 'javelin']
    javelin = payload['items'][1]
    assert set(javelin) == {
        'character_key',
        'title',
        'display_name',
        'name_zh',
        'name_en',
        'representative_asset',
        'icon',
        'source_metadata',
        'model_count',
        'model_counts',
        'source_counts',
        'fetched_at',
        'completed_at',
    }
    assert javelin['representative_asset']['url'] == f'{_STATIC_CHARACTER_PREFIX}/assets/live2d/javelin/textures/texture_00.webp?v=' + (
        'c' * 64
    )
    assert query_response.status_code == 200
    assert query_response.json()['total'] == 1
    assert query_response.json()['items'][0]['character_key'] == 'javelin'
    assert cached_response.status_code == 304
    assert cached_response.content == b''
    assert cached_response.headers['cache-control'] == 'public, max-age=300'
    assert cached_response.headers['etag'] == response.headers['etag']


def test_get_azurlane_character_route_returns_models_and_assets(tmp_path: Path) -> None:
    root = _create_azurlane_fixture(tmp_path / 'azurlane')
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/azurlane/characters/javelin', headers=_auth_headers())

    assert response.status_code == 200
    payload = response.json()
    assert payload['character_key'] == 'javelin'
    assert payload['active'] is True
    assert payload['model_counts'] == {'live2d': 1, 'spine': 1, 'total': 2}
    assert [model['model_id'] for model in payload['models']] == [
        'azurlane:live2d:javelin:javelin',
        'azurlane:spine:javelin:javelin_spine',
    ]
    assert payload['live2d_models'][0]['files']['model3']['path'] == 'assets/live2d/javelin/javelin.model3.json'
    assert payload['spine_models'][0]['files']['atlas']['path'] == 'assets/spine/javelin_spine/javelin_spine.atlas'
    assert len(payload['assets']) == 7


def test_get_azurlane_character_route_returns_not_found(tmp_path: Path) -> None:
    root = _create_azurlane_fixture(tmp_path / 'azurlane')
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/azurlane/characters/missing', headers=_auth_headers())

    assert response.status_code == 404
    assert response.json() == {
        'error': {
            'code': 'azurlane_character_not_found',
            'message': 'Azur Lane character not found.',
            'details': None,
        },
    }


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
