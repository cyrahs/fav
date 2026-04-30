# ruff: noqa: INP001, S101, S105, PLR2004, RUF001

import json
import shutil
from datetime import UTC, datetime
from pathlib import Path

from fastapi.testclient import TestClient

from src.api.app import create_app
from src.api.nikke import NikkeLibrary
from src.api.service import FavApiService

_VALID_TOKEN = 'token-for-tests'
_FIXED_NOW = datetime(2026, 4, 30, 0, 0, tzinfo=UTC)
_ROW_INDEX_BY_LABEL = {
    '时装图（切换）': 2,
    '时装立绘': 8,
    'SD模型': 9,
    '爆裂动画': 10,
}


class _RuntimeService:
    def close(self) -> None:
        return None


def _auth_headers(token: str = _VALID_TOKEN) -> dict[str, str]:
    return {'Authorization': f'Bearer {token}'}


def _write_json(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, ensure_ascii=False), encoding='utf-8')


def _write_asset(path: Path, body: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(body)


def _asset(path: str, *, label: str, body: bytes, kind: str = 'image', extra_context: dict | None = None) -> dict:
    context = {'section': 'style', 'row_index': _ROW_INDEX_BY_LABEL.get(label, 0), 'label': label, 'skin_index': 0}
    if extra_context:
        context.update(extra_context)
    return {
        'url': f'https://cdn.example.test/{Path(path).name}',
        'kind': kind,
        'local_path': path,
        'sha256': 'a' * 64,
        'content_type': 'image/png' if kind == 'image' else 'application/octet-stream',
        'size': len(body),
        'status': 'downloaded',
        'error': '',
        'contexts': [context],
    }


def _create_nikke_fixture(root: Path) -> Path:
    character_root = root / '101 - Test NIKKE'
    files = {
        'assets/images/icon.png': b'icon',
        'assets/images/thumb.webp': b'thumb',
        'assets/images/portrait.png': b'portrait',
        'assets/images/sd.gif': b'sd',
        'assets/images/burst.gif': b'burst',
        'assets/live2d/model-a/model.atlas': b'atlas',
        'assets/live2d/model-a/model.skel': b'skel',
        'assets/live2d/model-a/model.png': b'texture',
    }
    for relative_path, body in files.items():
        _write_asset(character_root / relative_path, body)

    icon_asset = {
        'url': 'https://cdn.example.test/icon.png',
        'kind': 'image',
        'local_path': 'assets/images/icon.png',
        'sha256': 'b' * 64,
        'content_type': 'image/png',
        'size': len(files['assets/images/icon.png']),
        'status': 'downloaded',
        'error': '',
        'contexts': [
            {'section': 'tj_list', 'field': 'icon', 'label': 'icon'},
            {'section': 'base', 'row_index': 15, 'label': '部队成员1'},
        ],
    }
    assets = [
        icon_asset,
        _asset('assets/images/thumb.webp', label='时装图（切换）', body=files['assets/images/thumb.webp']),
        _asset('assets/images/portrait.png', label='时装立绘', body=files['assets/images/portrait.png']),
        _asset('assets/images/sd.gif', label='SD模型', body=files['assets/images/sd.gif']),
        _asset('assets/images/burst.gif', label='爆裂动画', body=files['assets/images/burst.gif']),
        _asset(
            'assets/live2d/model-a/model.atlas',
            label='live2d（full）',
            body=files['assets/live2d/model-a/model.atlas'],
            kind='live2d_atlas',
            extra_context={'live2d_key': 'model-a', 'live2d_field': 'atlas'},
        ),
        _asset(
            'assets/live2d/model-a/model.skel',
            label='live2d（full）',
            body=files['assets/live2d/model-a/model.skel'],
            kind='live2d_skel',
            extra_context={'live2d_key': 'model-a', 'live2d_field': 'skel'},
        ),
        _asset(
            'assets/live2d/model-a/model.png',
            label='live2d（full）',
            body=files['assets/live2d/model-a/model.png'],
            kind='live2d_texture',
            extra_context={'live2d_key': 'model-a', 'live2d_field': 'atlas_texture'},
        ),
    ]
    base_info = {
        '稀有度': [{'row_index': 1, 'values': [{'type': 'text', 'value': 'SSR'}]}],
        '企业': [{'row_index': 5, 'values': [{'type': 'text', 'value': 'Elysion'}]}],
        '属性': [{'row_index': 8, 'values': [{'type': 'text', 'value': 'Fire'}]}],
        '职业': [{'row_index': 9, 'values': [{'type': 'text', 'value': 'Attacker'}]}],
        '武器': [{'row_index': 10, 'values': [{'type': 'text', 'value': 'Rifle'}]}],
        'CV': [{'row_index': 2, 'values': [{'type': 'text', 'value': 'Test CV'}]}],
        '实装日期': [{'row_index': 20, 'values': [{'type': 'text', 'value': '2024/01/01'}]}],
    }
    skins = [
        {
            'skin_index': 0,
            'name': 'Skin 1',
            'title': 'Default',
            'series': '/',
            'obtain': '/',
            'is_collection_skin': False,
            'rows': [
                {'row_index': 0, 'label': '时装名称', 'cells': [{'type': 'text', 'value': 'Default'}]},
                {
                    'row_index': 12,
                    'label': 'Greeting',
                    'cells': [
                        {'type': 'text', 'value': 'Greeting'},
                        {'type': 'text', 'value': 'Hello, Commander.'},
                        {'type': 'audio', 'value': '//cdn.example.test/voice.mp3'},
                    ],
                },
            ],
        },
    ]
    live2d_models = [
        {
            'label': 'live2d（full）',
            'section': 'style',
            'row_index': 5,
            'skin_index': 0,
            'skin_name': 'Skin 1',
            'skin_title': 'Default',
            'live2d_key': 'model-a',
            'position': {'pc': {'large': {'x': 1}}},
            'urls': {'atlas': 'https://cdn.example.test/model.atlas'},
        },
    ]
    manifest = {
        'source_url': 'https://www.gamekee.com/nikke/tj/101.html',
        'fetched_at': '2026-04-30T00:00:00+00:00',
        'content_id': 101,
        'title': 'Test NIKKE',
        'updated_at': 123,
        'tj_list': {'content_id': 101, 'name': 'Test NIKKE', 'level': 'SSR', 'qy': 'Elysion'},
        'base_info': base_info,
        'content_summary': {'skins': skins},
        'live2d_models': live2d_models,
        'assets': assets,
        'asset_counts': {'image': 5, 'live2d_atlas': 1, 'live2d_skel': 1, 'live2d_texture': 1},
    }
    character = {
        'content_id': 101,
        'title': 'Test NIKKE',
        'tj_list': manifest['tj_list'],
        'base_info': base_info,
        'skins': skins,
        'live2d_models': live2d_models,
    }
    _write_json(character_root / 'manifest.json', manifest)
    _write_json(character_root / 'character.json', character)
    _write_json(root / '_blobs' / 'manifest.json', {'content_id': 999, 'title': 'Ignored'})
    return root


def _build_service(root: Path) -> FavApiService:
    return FavApiService(
        dsn='postgresql://db.local/fav',
        token=_VALID_TOKEN,
        hanime1_video_fetcher=lambda _dsn: [],
        now_provider=lambda: _FIXED_NOW,
        job_provider=list,
        runtime_service=_RuntimeService(),
        nikke_library=NikkeLibrary(root),
    )


def test_list_nikke_characters_returns_lightweight_summaries(tmp_path: Path) -> None:
    root = _create_nikke_fixture(tmp_path / 'nikke')
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/nikke/characters?q=elysion&limit=1', headers=_auth_headers())

    assert response.status_code == 200
    assert response.json()['total'] == 1
    assert response.json()['limit'] == 1
    assert response.json()['offset'] == 0
    item = response.json()['items'][0]
    assert item['content_id'] == 101
    assert item['title'] == 'Test NIKKE'
    assert item['tags']['rarity'] == 'SSR'
    assert item['tags']['company'] == 'Elysion'
    assert item['icon']['url'] == '/api/v2/nikke/assets/101/assets/images/icon.png?v=' + ('b' * 64)
    assert item['portrait']['path'] == 'assets/images/portrait.png'
    assert item['skin_count'] == 1
    assert item['live2d_model_count'] == 1


def test_list_nikke_sidebar_characters_returns_sorted_light_payload(tmp_path: Path) -> None:
    root = _create_nikke_fixture(tmp_path / 'nikke')
    recent_root = root / '202 - Recent NIKKE'
    recent_icon_path = 'assets/images/recent-icon.png'
    _write_asset(recent_root / recent_icon_path, b'recent-icon')
    _write_json(
        recent_root / 'manifest.json',
        {
            'source_url': 'https://www.gamekee.com/nikke/tj/202.html',
            'fetched_at': '2026-04-30T00:00:01+00:00',
            'content_id': 202,
            'title': 'Recent NIKKE',
            'updated_at': 999,
            'tj_list': {'content_id': 202, 'name': 'Recent NIKKE', 'level': 'SSR', 'qy': 'Missilis'},
            'base_info': {
                '稀有度': [{'row_index': 1, 'values': [{'type': 'text', 'value': 'SSR'}]}],
                '企业': [{'row_index': 5, 'values': [{'type': 'text', 'value': 'Missilis'}]}],
                '实装日期': [{'row_index': 20, 'values': [{'type': 'text', 'value': '2025/02/03'}]}],
            },
            'content_summary': {'skins': []},
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
                    'contexts': [{'section': 'tj_list', 'field': 'icon', 'label': 'icon'}],
                },
            ],
        },
    )
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/nikke/sidebar/characters', headers=_auth_headers())
        query_response = client.get('/api/v2/nikke/sidebar/characters?q=elysion', headers=_auth_headers())
        cached_response = client.get(
            '/api/v2/nikke/sidebar/characters',
            headers={**_auth_headers(), 'If-None-Match': response.headers['etag']},
        )

    assert response.status_code == 200
    assert response.headers['cache-control'] == 'public, max-age=300'
    assert response.headers['etag'].startswith('"')
    payload = response.json()
    assert set(payload) == {'items', 'total'}
    assert payload['total'] == 2
    assert [item['content_id'] for item in payload['items']] == [202, 101]
    item = payload['items'][0]
    assert set(item) == {'content_id', 'title', 'icon', 'implemented_at', 'updated_at', 'fetched_at'}
    assert item['title'] == 'Recent NIKKE'
    assert item['implemented_at'] == '2025/02/03'
    assert item['updated_at'] == 999
    assert item['fetched_at'] == '2026-04-30T00:00:01+00:00'
    assert item['icon'] == {
        'url': '/api/v2/nikke/assets/202/assets/images/recent-icon.png?v=' + ('c' * 64),
        'available': True,
        'sha256': 'c' * 64,
        'content_type': 'image/png',
    }
    assert query_response.status_code == 200
    assert query_response.json()['total'] == 1
    assert query_response.json()['items'][0]['content_id'] == 101
    assert cached_response.status_code == 304
    assert cached_response.content == b''
    assert cached_response.headers['cache-control'] == 'public, max-age=300'
    assert cached_response.headers['etag'] == response.headers['etag']


def test_get_nikke_character_returns_skins_assets_and_live2d_refs(tmp_path: Path) -> None:
    root = _create_nikke_fixture(tmp_path / 'nikke')
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/nikke/characters/101', headers=_auth_headers())

    assert response.status_code == 200
    payload = response.json()
    assert payload['base_info']['CV'][0]['values'][0]['value'] == 'Test CV'
    assert len(payload['assets']) == 8
    skin = payload['skins'][0]
    assert skin['title'] == 'Default'
    assert skin['thumbnail']['path'] == 'assets/images/thumb.webp'
    assert skin['sd_model']['path'] == 'assets/images/sd.gif'
    assert skin['burst_animation']['path'] == 'assets/images/burst.gif'
    assert [asset['label'] for asset in skin['gallery']] == ['时装图（切换）', '时装立绘', 'SD模型', '爆裂动画']
    assert skin['voice_lines'] == [{'label': 'Greeting', 'text': 'Hello, Commander.', 'source_url': 'https://cdn.example.test/voice.mp3'}]
    model = skin['live2d_models'][0]
    assert model['live2d_key'] == 'model-a'
    assert model['assets']['atlas']['path'] == 'assets/live2d/model-a/model.atlas'
    assert model['assets']['skel']['path'] == 'assets/live2d/model-a/model.skel'
    assert model['assets']['textures'][0]['path'] == 'assets/live2d/model-a/model.png'


def test_get_nikke_asset_serves_known_manifest_asset(tmp_path: Path) -> None:
    root = _create_nikke_fixture(tmp_path / 'nikke')
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/nikke/assets/101/assets/images/icon.png', headers=_auth_headers())

    assert response.status_code == 200
    assert response.content == b'icon'
    assert response.headers['cache-control'] == 'public, max-age=31536000, immutable'
    assert response.headers['etag'] == '"' + ('b' * 64) + '"'


def test_get_nikke_asset_rejects_paths_outside_manifest_assets(tmp_path: Path) -> None:
    root = _create_nikke_fixture(tmp_path / 'nikke')
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        traversal = client.get('/api/v2/nikke/assets/101/assets/%2E%2E/character.json', headers=_auth_headers())
        missing = client.get('/api/v2/nikke/assets/101/assets/images/missing.png', headers=_auth_headers())

    assert traversal.status_code == 404
    assert traversal.json()['error']['code'] == 'nikke_asset_not_found'
    assert missing.status_code == 404
    assert missing.json()['error']['code'] == 'nikke_asset_not_found'


def test_get_nikke_asset_rejects_external_assets_root_symlink(tmp_path: Path) -> None:
    root = _create_nikke_fixture(tmp_path / 'nikke')
    character_root = root / '101 - Test NIKKE'
    external_assets = tmp_path / 'external-assets'
    _write_asset(external_assets / 'images' / 'icon.png', b'outside')
    shutil.rmtree(character_root / 'assets')
    (character_root / 'assets').symlink_to(external_assets, target_is_directory=True)
    service = _build_service(root)

    with TestClient(create_app(service=service)) as client:
        response = client.get('/api/v2/nikke/assets/101/assets/images/icon.png', headers=_auth_headers())

    assert response.status_code == 404
    assert response.json()['error']['code'] == 'nikke_asset_not_found'
