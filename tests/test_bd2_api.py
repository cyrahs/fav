# ruff: noqa: INP001, S101, S105, PLR0913, PLR2004, SLF001

import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import pytest
from fastapi.testclient import TestClient

import src.api.bd2 as bd2_module
from src.api.app import create_app
from src.api.bd2 import BD2Library
from src.api.service import FavApiService

_VALID_TOKEN = 'token-for-tests'
_FIXED_NOW = datetime(2026, 5, 1, 0, 0, tzinfo=UTC)
_STATIC_CHARACTER_PREFIX = '/static/bd2/101%20-%20Test%20BD2'


class _RuntimeService:
    def close(self) -> None:
        return None


class _Live2DViewOverrideStore:
    def __init__(self) -> None:
        self._rows: dict[tuple[str, int, str, str], dict[str, Any]] = {}

    def list_for_character(self, *, source: str, content_id: int) -> list[dict[str, Any]]:
        return [row for key, row in sorted(self._rows.items()) if key[0] == source and key[1] == content_id]

    def get(self, *, source: str, content_id: int, model_id: str, profile: str) -> dict[str, Any] | None:
        return self._rows.get((source, content_id, model_id, profile))

    def upsert(
        self,
        *,
        source: str,
        content_id: int,
        model_id: str,
        profile: str,
        position: dict[str, float],
        scale: float,
        background_position: dict[str, float] | None,
        background_scale: float | None,
    ) -> dict[str, Any]:
        key = (source, content_id, model_id, profile)
        existing = self._rows.get(key)
        row = {
            'source': source,
            'content_id': content_id,
            'model_id': model_id,
            'profile': profile,
            'position': dict(position),
            'scale': scale,
            'background_position': dict(background_position) if background_position is not None else None,
            'background_scale': background_scale,
            'created_at': existing['created_at'] if existing is not None else '2026-05-01T00:00:00Z',
            'updated_at': '2026-05-01T00:00:00Z',
        }
        self._rows[key] = row
        return row

    def delete(self, *, source: str, content_id: int, model_id: str, profile: str) -> bool:
        return self._rows.pop((source, content_id, model_id, profile), None) is not None


def _auth_headers(token: str = _VALID_TOKEN) -> dict[str, str]:
    return {'Authorization': f'Bearer {token}'}


def _write_json(path: Path, payload: dict) -> None:
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
    context: dict,
    sha256: str = 'a' * 64,
    content_type: str = 'application/octet-stream',
) -> dict:
    return {
        'url': f'https://cdn.example.test/{Path(path).name}',
        'kind': kind,
        'local_path': path,
        'sha256': sha256,
        'content_type': content_type,
        'size': len(body),
        'status': 'downloaded',
        'error': '',
        'contexts': [context],
    }


def _style_context(*, row_index: int, label: str, field: str = '', kind_role: str = '') -> dict:
    return {
        'section': 'style',
        'style_index': 0,
        'style_name': 'style-a',
        'costume_title': 'Office Worker',
        'costume_category': 'Support',
        'row_index': row_index,
        'column_index': 1,
        'label': label,
        'field': field,
        'is_art_row': bool(field),
        'column_name': 'Office Worker',
        'column_category': 'Support',
        'column_role': kind_role,
    }


def _create_bd2_fixture(root: Path) -> Path:
    character_root = root / '101 - Test BD2'
    files = {
        'assets/images/icon.png': b'icon',
        'assets/images/sprite.png': b'sprite',
        'assets/images/other-portrait.png': b'other-portrait',
        'assets/images/portrait.png': b'portrait',
        'assets/images/full.png': b'full',
        'assets/videos/skill.mp4': b'video',
        'assets/audio/voice.mp3': b'voice',
        'assets/live2d/model-a/model.atlas': b'atlas',
        'assets/live2d/model-a/model.json': b'{"skeleton":{"spine":"4.0.64"}}',
        'assets/live2d/model-a/model.png': b'texture',
        'assets/live2d/model-a/background.png': b'background',
    }
    for relative_path, body in files.items():
        _write_asset(character_root / relative_path, body)

    live2d_context = {
        **_style_context(row_index=6, label='Standing Live2D', field='standing_live2d', kind_role='Live2D file'),
        'live2d_key': 'model-a',
    }
    assets = [
        _asset(
            'assets/images/icon.png',
            body=files['assets/images/icon.png'],
            kind='image',
            context={'section': 'entry_tree', 'field': 'icon', 'label': 'icon'},
            sha256='b' * 64,
            content_type='image/png',
        ),
        _asset(
            'assets/images/sprite.png',
            body=files['assets/images/sprite.png'],
            kind='image',
            context=_style_context(row_index=2, label='Costume sprite', field='costume_sprite', kind_role='Costume sprite'),
            content_type='image/png',
        ),
        _asset(
            'assets/images/other-portrait.png',
            body=files['assets/images/other-portrait.png'],
            kind='image',
            context={
                **_style_context(row_index=3, label='Costume portrait', field='costume_portrait', kind_role='Costume portrait'),
                'column_name': 'Other Costume',
                'column_category': 'Attacker',
                'costume_title': 'Other Costume',
                'costume_category': 'Attacker',
            },
            content_type='image/png',
        ),
        _asset(
            'assets/images/portrait.png',
            body=files['assets/images/portrait.png'],
            kind='image',
            context=_style_context(row_index=3, label='Costume portrait', field='costume_portrait', kind_role='Costume portrait'),
            content_type='image/png',
        ),
        _asset(
            'assets/images/full.png',
            body=files['assets/images/full.png'],
            kind='image',
            context=_style_context(
                row_index=4,
                label='Full costume portrait',
                field='costume_full_portrait',
                kind_role='Full costume portrait',
            ),
            content_type='image/png',
        ),
        _asset(
            'assets/videos/skill.mp4',
            body=files['assets/videos/skill.mp4'],
            kind='video',
            context=_style_context(row_index=8, label='Skill animation', field='skill_live2d_1', kind_role='Skill animation'),
            content_type='video/mp4',
        ),
        _asset(
            'assets/audio/voice.mp3',
            body=files['assets/audio/voice.mp3'],
            kind='audio',
            context={**_style_context(row_index=16, label='Voice', kind_role='Voice'), 'field': ''},
            content_type='audio/mpeg',
        ),
        _asset(
            'assets/live2d/model-a/model.atlas',
            body=files['assets/live2d/model-a/model.atlas'],
            kind='live2d_atlas',
            context={**live2d_context, 'live2d_field': 'atlas'},
        ),
        _asset(
            'assets/live2d/model-a/model.json',
            body=files['assets/live2d/model-a/model.json'],
            kind='live2d_json',
            context={**live2d_context, 'live2d_field': 'json'},
        ),
        _asset(
            'assets/live2d/model-a/model.png',
            body=files['assets/live2d/model-a/model.png'],
            kind='live2d_texture',
            context={**live2d_context, 'live2d_field': 'atlas_texture'},
            content_type='image/png',
        ),
        _asset(
            'assets/live2d/model-a/background.png',
            body=files['assets/live2d/model-a/background.png'],
            kind='live2d_background',
            context={**live2d_context, 'live2d_field': 'bg'},
            content_type='image/png',
        ),
    ]
    base_info = {
        'Rarity': [{'row_index': 1, 'values': [{'type': 'text', 'value': '5-star'}]}],
        'Class': [{'row_index': 2, 'values': [{'type': 'text', 'value': 'Support'}]}],
    }
    costume = {
        'style_index': 0,
        'style_name': 'style-a',
        'title': 'Office Worker',
        'category': 'Support',
        'rows': [{'row_index': 0, 'label': 'Costume name', 'cells': [{'type': 'text', 'value': 'Office Worker'}]}],
    }
    live2d_models = [
        {
            **live2d_context,
            'key': 'bound-live2d',
            'stable_id': 'stable-live2d',
            'animation': 'idle',
            'skin': 'office',
            'limit_age': False,
            'source': 'bd2_l2d_viewer',
            'variant': 'censored',
            'supplement_reason': 'censored_variant',
            'viewer_entry_id': '101101_c',
            'viewer_stem': 'char101101_c',
            'source_page_url': 'https://jelosus2.github.io/BD2-L2D-Viewer/',
            'position': {'pc': {'x': 1}},
            'bg_position': {'pc': {'x': 2}},
            'urls': {'atlas': 'https://cdn.example.test/model.atlas', 'bg': 'https://cdn.example.test/background.png'},
        },
    ]
    manifest = {
        'source_url': 'https://www.gamekee.com/zsca2/tj/101.html',
        'detail_api_url': 'https://www.gamekee.com/v1/content/detail/101',
        'fetched_at': '2026-05-01T00:00:00+00:00',
        'content_id': 101,
        'title': 'Test BD2',
        'updated_at': 123,
        'tree_row': {'content_id': 101, 'name': 'Test BD2', '_bd2_group_name': '5-star'},
        'base_info': base_info,
        'content_summary': {'costumes': [costume]},
        'costumes': [costume],
        'live2d_models': live2d_models,
        'assets': assets,
        'asset_counts': {'image': 5, 'video': 1, 'audio': 1, 'live2d_atlas': 1, 'live2d_texture': 1},
    }
    character = {
        'content_id': 101,
        'title': 'Test BD2',
        'tree_row': manifest['tree_row'],
        'base_info': base_info,
        'costumes': [costume],
        'live2d_models': live2d_models,
    }
    _write_json(character_root / 'manifest.json', manifest)
    _write_json(character_root / 'character.json', character)
    _write_json(root / '_blobs' / 'manifest.json', {'content_id': 999, 'title': 'Ignored'})
    return root


def _build_service(root: Path, override_store: _Live2DViewOverrideStore | None = None) -> FavApiService:
    return FavApiService(
        dsn='postgresql://db.local/fav',
        token=_VALID_TOKEN,
        hanime1_video_fetcher=lambda _dsn: [],
        now_provider=lambda: _FIXED_NOW,
        job_provider=list,
        runtime_service=_RuntimeService(),
        bd2_library=BD2Library(root),
        live2d_view_override_store=override_store or _Live2DViewOverrideStore(),
    )


def test_list_bd2_characters_returns_lightweight_summaries(tmp_path: Path) -> None:
    root = _create_bd2_fixture(tmp_path / 'bd2')
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/bd2/characters?q=support&limit=1', headers=_auth_headers())

    assert response.status_code == 200
    assert response.json()['total'] == 1
    item = response.json()['items'][0]
    assert item['content_id'] == 101
    assert item['title'] == 'Test BD2'
    assert item['tags']['rarity_group'] == '5-star'
    assert item['icon']['url'] == f'{_STATIC_CHARACTER_PREFIX}/assets/images/icon.png?v=' + ('b' * 64)
    assert item['portrait']['path'] == 'assets/images/portrait.png'
    assert item['costume_count'] == 1
    assert item['live2d_model_count'] == 1
    assert 'Office Worker' in item['search_terms']

    with TestClient(create_app(service=service)) as client:
        costume_response = client.get('/api/v2/bd2/characters?q=office%20worker', headers=_auth_headers())

    assert costume_response.status_code == 200
    assert costume_response.json()['total'] == 1


def test_bd2_library_loads_summary_from_disk_cache_without_rebuilding(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    root = _create_bd2_fixture(tmp_path / 'bd2')
    expected = BD2Library(root).list_characters()
    assert (root / '_api' / 'summary-cache.json').is_file()

    original_read_json_object = bd2_module._read_json_object

    def _fail_manifest_read(path: Path) -> dict:
        if path.name == 'manifest.json':
            msg = f'Unexpected manifest rebuild: {path}'
            raise AssertionError(msg)
        return original_read_json_object(path)

    monkeypatch.setattr(bd2_module, '_read_json_object', _fail_manifest_read)

    assert BD2Library(root).list_characters() == expected


def test_list_bd2_sidebar_characters_returns_sorted_light_payload(tmp_path: Path) -> None:
    root = _create_bd2_fixture(tmp_path / 'bd2')
    recent_root = root / '202 - Recent BD2'
    recent_icon_path = 'assets/images/recent-icon.png'
    _write_asset(recent_root / recent_icon_path, b'recent-icon')
    _write_json(
        recent_root / 'manifest.json',
        {
            'source_url': 'https://www.gamekee.com/zsca2/tj/202.html',
            'fetched_at': '2026-05-01T00:00:01+00:00',
            'content_id': 202,
            'title': 'Recent BD2',
            'updated_at': 999,
            'tree_row': {'content_id': 202, '_bd2_group_name': '4-star'},
            'base_info': {},
            'costumes': [],
            'live2d_models': [],
            'assets': [
                {
                    'url': 'https://cdn.example.test/recent-icon.png',
                    'kind': 'image',
                    'local_path': recent_icon_path,
                    'sha256': 'c' * 64,
                    'content_type': 'image/png',
                    'size': len(b'recent-icon'),
                    'status': 'downloaded',
                    'error': '',
                    'contexts': [{'section': 'entry_tree', 'field': 'icon', 'label': 'icon'}],
                },
            ],
        },
    )
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/bd2/sidebar/characters', headers=_auth_headers())
        query_response = client.get('/api/v2/bd2/sidebar/characters?q=test', headers=_auth_headers())
        costume_query_response = client.get('/api/v2/bd2/sidebar/characters?q=office%20worker', headers=_auth_headers())
        cached_response = client.get(
            '/api/v2/bd2/sidebar/characters',
            headers={**_auth_headers(), 'If-None-Match': response.headers['etag']},
        )

    assert response.status_code == 200
    assert response.headers['cache-control'] == 'public, max-age=300'
    payload = response.json()
    assert payload['total'] == 2
    assert [item['content_id'] for item in payload['items']] == [202, 101]
    assert payload['items'][0]['icon'] == {
        'url': '/static/bd2/202%20-%20Recent%20BD2/assets/images/recent-icon.png?v=' + ('c' * 64),
        'available': True,
        'sha256': 'c' * 64,
        'content_type': 'image/png',
    }
    assert query_response.status_code == 200
    assert query_response.json()['total'] == 1
    assert query_response.json()['items'][0]['content_id'] == 101
    assert costume_query_response.status_code == 200
    assert costume_query_response.json()['total'] == 1
    assert costume_query_response.json()['items'][0]['content_id'] == 101
    assert cached_response.status_code == 304
    assert cached_response.content == b''
    assert cached_response.headers['etag'] == response.headers['etag']


def test_get_bd2_character_returns_costumes_assets_and_live2d_refs(tmp_path: Path) -> None:
    root = _create_bd2_fixture(tmp_path / 'bd2')
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/bd2/characters/101', headers=_auth_headers())

    assert response.status_code == 200
    payload = response.json()
    assert payload['base_info']['Class'][0]['values'][0]['value'] == 'Support'
    assert len(payload['assets']) == 11
    costume = payload['costumes'][0]
    assert costume['title'] == 'Office Worker'
    assert costume['sprite']['path'] == 'assets/images/sprite.png'
    assert costume['portrait']['path'] == 'assets/images/portrait.png'
    assert costume['full_portrait']['path'] == 'assets/images/full.png'
    assert 'assets/images/other-portrait.png' in {asset['path'] for asset in costume['gallery']}
    assert costume['videos'][0]['path'] == 'assets/videos/skill.mp4'
    assert costume['audio'][0]['path'] == 'assets/audio/voice.mp3'
    model = costume['live2d_models'][0]
    assert model['live2d_key'] == 'model-a'
    assert model['source'] == 'bd2_l2d_viewer'
    assert model['variant'] == 'censored'
    assert model['supplement_reason'] == 'censored_variant'
    assert model['viewer_entry_id'] == '101101_c'
    assert model['viewer_stem'] == 'char101101_c'
    assert model['source_page_url'] == 'https://jelosus2.github.io/BD2-L2D-Viewer/'
    assert model['model_id'] == 'stable-live2d'
    assert model['view_overrides'] == {}
    assert model['assets']['atlas']['path'] == 'assets/live2d/model-a/model.atlas'
    assert model['assets']['atlas']['url'] == f'{_STATIC_CHARACTER_PREFIX}/assets/live2d/model-a/model.atlas?v=' + ('a' * 64)
    assert model['assets']['json']['path'] == 'assets/live2d/model-a/model.json'
    assert model['spine_version'] == '4.0.64'
    assert model['assets']['textures'][0]['path'] == 'assets/live2d/model-a/model.png'
    assert model['assets']['background']['path'] == 'assets/live2d/model-a/background.png'


def test_get_bd2_asset_endpoint_is_removed(tmp_path: Path) -> None:
    root = _create_bd2_fixture(tmp_path / 'bd2')
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/bd2/assets/101/assets/images/icon.png', headers=_auth_headers())

    assert response.status_code == 404
    assert response.json()['error']['code'] == 'not_found'


def test_bd2_live2d_view_override_crud_and_detail_overlay(tmp_path: Path) -> None:
    root = _create_bd2_fixture(tmp_path / 'bd2')
    override_store = _Live2DViewOverrideStore()
    service = _build_service(root, override_store=override_store)

    with TestClient(create_app(service=service)) as client:
        create_response = client.put(
            '/api/v2/bd2/characters/101/live2d-models/stable-live2d/view-overrides/default',
            headers=_auth_headers(),
            json={
                'position': {'x': 120.5, 'y': -32},
                'scale': 1.25,
                'background_position': {'x': 0, 'y': 4},
                'background_scale': 1.1,
            },
        )
        get_response = client.get(
            '/api/v2/bd2/characters/101/live2d-models/stable-live2d/view-overrides/default',
            headers=_auth_headers(),
        )
        detail_response = client.get('/api/v2/bd2/characters/101', headers=_auth_headers())
        delete_response = client.delete(
            '/api/v2/bd2/characters/101/live2d-models/stable-live2d/view-overrides/default',
            headers=_auth_headers(),
        )
        missing_response = client.get(
            '/api/v2/bd2/characters/101/live2d-models/stable-live2d/view-overrides/default',
            headers=_auth_headers(),
        )

    assert create_response.status_code == 200
    assert create_response.json() == {
        'source': 'bd2',
        'content_id': 101,
        'model_id': 'stable-live2d',
        'profile': 'default',
        'position': {'x': 120.5, 'y': -32.0},
        'scale': 1.25,
        'background_position': {'x': 0.0, 'y': 4.0},
        'background_scale': 1.1,
        'created_at': '2026-05-01T00:00:00Z',
        'updated_at': '2026-05-01T00:00:00Z',
    }
    assert get_response.status_code == 200
    assert get_response.json() == create_response.json()
    assert detail_response.status_code == 200
    model = detail_response.json()['costumes'][0]['live2d_models'][0]
    assert model['model_id'] == 'stable-live2d'
    assert model['view_overrides']['default']['position'] == {'x': 120.5, 'y': -32.0}
    assert model['view_overrides']['default']['scale'] == 1.25
    assert delete_response.status_code == 204
    assert delete_response.content == b''
    assert missing_response.status_code == 404
    assert missing_response.json()['error']['code'] == 'live2d_view_override_not_found'


def test_bd2_live2d_view_override_rejects_unknown_model(tmp_path: Path) -> None:
    root = _create_bd2_fixture(tmp_path / 'bd2')
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.put(
            '/api/v2/bd2/characters/101/live2d-models/missing-model/view-overrides/default',
            headers=_auth_headers(),
            json={'position': {'x': 1, 'y': 2}, 'scale': 1},
        )

    assert response.status_code == 404
    assert response.json()['error']['code'] == 'bd2_live2d_model_not_found'


def test_bd2_library_skips_symlinked_character_dirs_outside_root(tmp_path: Path) -> None:
    root = tmp_path / 'bd2'
    external = tmp_path / 'external-character'
    _write_json(external / 'manifest.json', {'content_id': 303, 'title': 'External BD2'})
    root.mkdir()
    (root / '303 - External BD2').symlink_to(external, target_is_directory=True)

    assert BD2Library(root).list_characters() == []


def test_bd2_library_prefers_newest_duplicate_content_id_manifest(tmp_path: Path) -> None:
    root = tmp_path / 'bd2'
    _write_json(
        root / '101 - Old BD2' / 'manifest.json',
        {'content_id': 101, 'title': 'Old BD2', 'fetched_at': '2026-05-01T00:00:00+00:00'},
    )
    _write_json(
        root / '101 - New BD2' / 'manifest.json',
        {'content_id': 101, 'title': 'New BD2', 'fetched_at': '2026-05-01T00:01:00+00:00'},
    )

    library = BD2Library(root)

    assert [character['title'] for character in library.list_characters()] == ['New BD2']
    assert library.get_character(101)['title'] == 'New BD2'
