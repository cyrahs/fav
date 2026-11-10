"""`src/api/azurlane.py` and the `/azurlane/*` reading routes (WP12).

Requests replayed through the real FastAPI app over a fixture tree of manifests (the files are
in the golden, so the Rust replay builds the same tree). The tree holds what
tests/test_azurlane_api.py builds plus the branches it does not reach: duplicate keys, keys from
the directory name, unreadable manifests, unsafe asset paths, fallbacks for missing fields,
case-folded sorting and both spellings of the skin-update feed. A second root adds a manifest
whose counts fail the response models and has no snapshot, so the 500 and 404 answers are
pinned too. Each root is read by a fresh library without a disk cache, so the summaries are
built before the first answer and no background refresh races the replay.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

from fastapi.testclient import TestClient
from src.api.app import create_app
from src.api.azurlane import AzurLaneLibrary
from src.api.service import FavApiService

TOKEN = 'token-for-tests'  # noqa: S105
AUTH = {'Authorization': f'Bearer {TOKEN}'}
_RESPONSE_HEADERS = ('content-type', 'cache-control', 'etag', 'allow')


def _asset(  # noqa: PLR0913 - mirrors the manifest asset fields
    path: str,
    kind: str,
    context: dict[str, Any],
    *,
    sha256: str = 'a' * 64,
    content_type: str = 'application/octet-stream',
    **extra: Any,
) -> dict[str, Any]:
    name = path.rsplit('/', 1)[-1]
    asset: dict[str, Any] = {
        'url': f'https://static.example/azurlane/{name}',
        'normalized_url': f'https://static.example/azurlane/{name}',
        'downloaded_url': f'https://static.example/azurlane/{name}',
        'fallback_url': f'https://fallback.example/azurlane/{name}',
        'source_urls': {
            'primary': f'https://static.example/azurlane/{name}',
            'fallback': f'https://fallback.example/azurlane/{name}',
            'downloaded': f'https://static.example/azurlane/{name}',
        },
        'kind': kind,
        'local_path': path,
        'original_filename': name,
        'sha256': sha256,
        'size': 4,
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
    asset.update(extra)
    return asset


def _context(model_id: str, model_type: str, costume_key: str, field: str, **extra: Any) -> dict[str, Any]:
    context = {
        'model_id': model_id,
        'model_type': model_type,
        'character_key': 'javelin',
        'costume_key': costume_key,
        'catalog_source': 'merged' if model_type == 'live2d' else 'l2d.su',
        f'{model_type}_field': field,
    }
    context.update(extra)
    return context


def _javelin(files: dict[str, str]) -> None:
    root = 'javelin - Javelin'
    for path in (
        'assets/live2d/javelin/javelin.model3.json',
        'assets/live2d/javelin/javelin.moc3',
        'assets/live2d/javelin/textures/texture_00.webp',
        'assets/live2d/javelin/motions/idle.motion3.json',
        'assets/spine/javelin_spine/javelin_spine.skel',
        'assets/spine/javelin_spine/javelin_spine.atlas',
        'assets/spine/javelin_spine/javelin_spine.webp',
        'assets/spine/javelin_spine/javelin_spine2.webp',
        'assets/painting/javelin_2/javelin_2.webp',
        'assets/painting/javelin_2/squareicon/javelin_2.webp',
        'assets/painting/javelin/squareicon/javelin.webp',
    ):
        files[f'{root}/{path}'] = 'body'
    live2d_id = 'azurlane:live2d:javelin:javelin'
    spine_id = 'azurlane:spine:javelin:javelin_spine'
    painting_id = 'azurlane:painting:javelin:javelin_2'
    default_painting_id = 'azurlane:painting:javelin:javelin'
    live2d_assets = [
        _asset(
            'assets/live2d/javelin/textures/texture_00.webp',
            'live2d.texture',
            _context(live2d_id, 'live2d', 'javelin', 'texture'),
            sha256='c' * 64,
            content_type='image/webp',
        ),
        _asset(
            'assets/live2d/javelin/javelin.model3.json',
            'live2d.model3',
            _context(live2d_id, 'live2d', 'javelin', 'model3'),
            sha256='b' * 64,
            content_type='application/json',
        ),
        _asset('assets/live2d/javelin/javelin.moc3', 'live2d.moc3', _context(live2d_id, 'live2d', 'javelin', 'moc3')),
        _asset('assets/live2d/javelin/motions/idle.motion3.json', 'live2d.motion', _context(live2d_id, 'live2d', 'javelin', 'motion')),
        # Not on disk: listed, unavailable.
        _asset('assets/live2d/javelin/missing.physics3.json', 'live2d.physics', _context(live2d_id, 'live2d', 'javelin', 'physics')),
        # Unsafe and odd paths: served as written, without a URL.
        _asset('../outside.txt', 'live2d.audio', _context(live2d_id, 'live2d', 'javelin', 'audio'), sha256=''),
        _asset('assets/%2e%2e/outside.txt', 'live2d.audio', {'model_id': live2d_id}, sha256=''),
        _asset('', 'live2d.expression', {}, sha256=''),
        {'kind': 'live2d.pose', 'local_path': 'assets/live2d/javelin/pose.json', 'url': 'https://static.example/pose.json'},
        'not an asset',
    ]
    spine_assets = [
        _asset('assets/spine/javelin_spine/javelin_spine.webp', 'spine.texture', _context(spine_id, 'spine', 'javelin_spine', 'texture')),
        _asset('assets/spine/javelin_spine/javelin_spine2.webp', 'spine.texture', _context(spine_id, 'spine', 'javelin_spine', 'texture')),
        _asset('assets/spine/javelin_spine/javelin_spine.atlas', 'spine.atlas', _context(spine_id, 'spine', 'javelin_spine', 'atlas')),
        _asset('assets/spine/javelin_spine/javelin_spine.skel', 'spine.skel', _context(spine_id, 'spine', 'javelin_spine', 'skel')),
    ]
    painting_assets = [
        _asset(
            'assets/painting/javelin_2/squareicon/javelin_2.webp',
            'icon.square',
            {'model_id': painting_id, 'painting_field': 'square_icon'},
            sha256='d' * 64,
        ),
        _asset('assets/painting/javelin_2/javelin_2.webp', 'painting.image', {'model_id': painting_id, 'painting_field': 'image'}),
        _asset('assets/painting/javelin_2/voice/1.ogg', 'voice.audio', {'model_id': painting_id}, size='12', failed_count='2'),
    ]
    manifest = {
        'schema_version': 1,
        'source': 'azurlane',
        'character_key': 'javelin',
        'source_id': '101',
        'name_zh': '标枪',
        'name_en': 'Javelin',
        'source_metadata': {'model_ids': [live2d_id, spine_id], 'sources': ['merged', 'l2d.su'], 'default_skin_id': 1, 'note': 'Ä ß'},
        'fetched_at': '2026-05-19T00:00:00+00:00',
        'completed_at': '2026-05-19T00:00:00Z',
        'active': 1,
        'model_counts': {'live2d': 1, 'spine': 1, 'painting': 2, 'total': 4.0},
        'asset_counts': {'live2d.model3': 1, 'live2d.texture': True},
        'models': [
            {
                'model_id': painting_id,
                'type': 'painting',
                'source': 'l2d.su',
                'costume': {'key': 'javelin_2', 'id': '2', 'name_zh': '', 'name_en': 'Swimsuit'},
                'source_urls': {'primary': '', 'fallback': ''},
                'availability': {'archive_state': 'complete'},
                'source_metadata': {'painting_index': {'layers': [{'name': 'javelin_2', 'raw': True}]}},
                'assets': painting_assets,
            },
            {
                'model_id': spine_id,
                'type': 'spine',
                'source': 'l2d.su',
                'character_key': 'javelin',
                'costume': {'key': 'javelin_spine', 'id': 3, 'name_zh': '动态', 'name_en': 'Dynamic'},
                'source_urls': {'primary': 'https://static.example/azurlane/javelin_spine', 'fallback': ''},
                'availability': ['not', 'a', 'dict'],
                'fetched_at': '2026-05-19T00:00:00+00:00',
                'completed_at': '',
                'assets': spine_assets,
                'asset_counts': {'spine.skel': 1, 'spine.atlas': 1, 'spine.texture': 2},
            },
            {
                'model_id': live2d_id,
                'type': 'live2d',
                'source': 'merged',
                'character_key': 'javelin',
                'costume': {'key': 'javelin', 'id': 1, 'name_zh': '默认', 'name_en': 'Default'},
                'source_urls': {'primary': 'https://static.example/azurlane/javelin.model3.json', 'fallback': '', 'display_info': ''},
                'availability': {'archive_state': 'complete', 'asset_count': 9},
                'source_metadata': {'id': live2d_id},
                'fetched_at': '2026-05-19T00:00:00+00:00',
                'completed_at': '2026-05-19T00:00:00+00:00',
                'assets': live2d_assets,
            },
            {
                'model_id': default_painting_id,
                'type': 'painting',
                'source': 'l2d.su',
                'costume': {'key': 'javelin', 'id': 1},
                'assets': [
                    _asset(
                        'assets/painting/javelin/squareicon/javelin.webp',
                        'icon.square',
                        {'model_id': default_painting_id},
                        sha256='e' * 64,
                    ),
                ],
            },
            'not a model',
        ],
    }
    files[f'{root}/manifest.json'] = json.dumps(manifest, ensure_ascii=False)
    files[f'{root}/detail.json'] = json.dumps({'ship_id': 101, 'skins': [{'id': 1, 'name': '默认'}], 'stats': {'hp': 1.5}}, ensure_ascii=False)


def _minimal(character_key: str, name_en: str, completed_at: str, source_id: int, **extra: Any) -> str:
    manifest: dict[str, Any] = {
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
    }
    manifest.update(extra)
    return json.dumps(manifest, ensure_ascii=False)


def _icons(files: dict[str, str]) -> None:
    """Two painting skins with square icons; the default skin's is not on disk."""
    root = 'yuzhang - Yuzhang'
    models = []
    for costume_id, costume_key, on_disk in ((21, 'yuzhang', True), (22, 'yuzhang_2', False)):
        icon_path = f'assets/painting/{costume_key}/squareicon/{costume_key}.webp'
        if on_disk:
            files[f'{root}/{icon_path}'] = 'icon'
        models.append(
            {
                'model_id': f'azurlane:painting:yuzhang:{costume_key}',
                'type': 'painting',
                'source': 'l2d.su',
                'character_key': 'yuzhang',
                'costume': {'key': costume_key, 'id': costume_id, 'name_zh': '', 'name_en': costume_key},
                'assets': [_asset(icon_path, 'icon.square', {'model_type': 'painting'}, sha256=f'{costume_id}' * 32)],
            },
        )
    files[f'{root}/manifest.json'] = json.dumps(
        {
            'character_key': 'yuzhang',
            'source_id': 2,
            'name_zh': '豫章',
            'source_metadata': {'sources': ['l2d.su'], 'skin_series': ['Default', 'Swimsuits'], 'default_skin_id': 22},
            'models': models,
        },
        ensure_ascii=False,
    )


def _layered(files: dict[str, str]) -> None:
    root = 'xiafei - Richelieu'
    model_id = 'azurlane:painting:xiafei:xiafei_4'
    entries = (
        ('assets/painting/xiafei_4/xiafei_4.json', 'painting.index'),
        ('assets/painting/xiafei_4/xiafei_4.webp', 'painting.image'),
        ('assets/painting/xiafei_4/painting/xiafei_4_rw.webp', 'painting.layer'),
        ('assets/painting/xiafei_4/painting/xiafei_4_rw-mesh.obj', 'painting.mesh'),
        ('assets/painting/xiafei_4/face/1.webp', 'painting.face'),
        ('assets/painting/xiafei_4/shipyardicon/xiafei_4.webp', 'icon.shipyard'),
    )
    assets = []
    for path, kind in entries:
        files[f'{root}/{path}'] = 'x'
        assets.append(_asset(path, kind, {'model_id': model_id, 'painting_field': kind.split('.')[1]}))
    files[f'{root}/manifest.json'] = json.dumps(
        {
            'schema_version': '1',
            'source': '',
            'character_key': 'xiafei',
            'name_en': '  Richelieu\r\n',
            'source_metadata': {'sources': ['l2d.su']},
            'active': False,
            'model_counts': {},
            'models': [
                {
                    'model_id': model_id,
                    'type': 'painting',
                    'source': 'l2d.su',
                    'costume': {'key': 'xiafei_4', 'id': 10},
                    'source_metadata': {'painting_index': {'layers': [{'name': 'xiafei_4', 'size': [4574.0, 2866.0]}]}},
                    'assets': assets,
                },
                {'model_id': 'azurlane:spine:xiafei:odd', 'type': 'other', 'assets': []},
            ],
        },
        ensure_ascii=False,
    )


def _fixture() -> dict[str, str]:
    files: dict[str, str] = {}
    _javelin(files)
    _icons(files)
    _layered(files)
    files['z23 - Z23/manifest.json'] = _minimal('z23', 'Z23', '2026-05-19T00:01:00+00:00', 23)
    # Duplicate keys: the fresher manifest wins whichever directory sorts first.
    files['ayanami - Ayanami/manifest.json'] = _minimal('ayanami', 'Ayanami', '2026-05-18T00:00:00+00:00', 30)
    files['ayanami-retrofit - Ayanami Kai/manifest.json'] = _minimal('ayanami', 'Ayanami Kai', '2026-05-19T00:00:00+00:00', 31)
    files['Ayanami-old - Ayanami Old/manifest.json'] = _minimal('ayanami', 'Ayanami Old', '2026-05-17T00:00:00+00:00', 32)
    # The key from the directory name, the display name from its suffix.
    files['Échelle - Shiranui/manifest.json'] = json.dumps({'name_en': '', 'name_zh': '', 'models': []})
    files['laffey/manifest.json'] = json.dumps({'character_key': 'laffey', 'source_id': 0, 'completed_at': 1747612800})
    files['straße - Straße/manifest.json'] = _minimal('strasse', 'Straße', '2026-05-16T00:00:00', 40)
    files['Zuikaku - zuikaku/manifest.json'] = _minimal(
        'zuikaku', 'zuikaku', '2026-05-19T00:01:00+00:00', 41, tags='x', model_counts={'live2d': 2}
    )
    files['unicorn - Unicorn/manifest.json'] = _minimal('unicorn', 'Unicorn', '2026-05-15T00:00:00+00:00', 42)
    files['unicorn - Unicorn/detail.json'] = '[1, 2]'
    files['hammann - Hammann/manifest.json'] = _minimal('hammann', 'Hammann', 'yesterday', 43)
    files['hammann - Hammann/detail.json'] = '{"broken": '
    # Skipped: unreadable, not an object, no key, hidden.
    files['broken - Broken/manifest.json'] = '{"character_key": '
    files['list - List/manifest.json'] = '[1, 2, 3]'
    files[' - /manifest.json'] = json.dumps({'name_en': 'Nameless'})
    files['_api/manifest.json'] = json.dumps({'character_key': 'hidden'})
    files['_source/manifest.json'] = json.dumps({'character_key': 'hidden-source'})
    files['notes/readme.txt'] = 'no manifest here'
    files['_source/l2d-su-snapshot.json'] = json.dumps(
        {
            'game_version': '9.7.323',
            'region': 'CN',
            'generated_at': '2026-08-13T05:33:57.726Z',
            'characters': [{'char_id': 1}],
            'new_skins': [
                {'ship_group_id': 10720, 'ship_name': '本宁顿', 'skin_name': '盛夏', 'skin_type': 'Live2D+', 'skin_ids': [107201, '107202', 'x']},
                {'shipGroupId': '20', 'ship_group_id': None, 'shipName': 'camel', 'skinIds': None},
                'junk',
            ],
            'skin_update_history': [
                {
                    'date': '2026-08-13T03:57:01.516Z',
                    'version': 9.7,
                    'skins': [{'shipGroupId': 20712, 'shipName': '不挠', 'skinName': '领航员', 'skinType': 'Live2D+', 'skinIds': [207122]}],
                },
                {'skins': 'none'},
                7,
            ],
        },
        ensure_ascii=False,
    )
    return files


def _hostile() -> dict[str, str]:
    """A manifest whose counts fail the summary models (`total` is no int)."""
    return {
        'zz-hostile - Hostile/manifest.json': _minimal(
            'hostile', 'Hostile', '2026-05-20T00:00:00+00:00', 99, model_counts={'total': 'two'}
        ),
        'zz-hostile - Hostile/detail.json': json.dumps({'ok': True}),
    }


def _req(method: str, path: str, *, headers: dict[str, str] | None = None) -> dict[str, Any]:
    return {'method': method, 'path': path, 'headers': AUTH if headers is None else headers}


def _requests() -> list[dict[str, Any]]:
    characters = '/api/v2/azurlane/characters'
    sidebar = '/api/v2/azurlane/sidebar/characters'
    return [
        _req('GET', characters, headers={}),
        _req('GET', characters, headers={'Authorization': 'Bearer wrong-token'}),
        _req('GET', characters),
        _req('GET', f'{characters}?limit=2&offset=1'),
        _req('GET', f'{characters}?offset=99'),
        _req('GET', f'{characters}?q=JAVELIN'),
        _req('GET', f'{characters}?q=%E6%A0%87%E6%9E%AA'),
        _req('GET', f'{characters}?q=merged'),
        _req('GET', f'{characters}?q=Swimsuits'),
        _req('GET', f'{characters}?q=%22l2d.su%22%5D'),
        _req('GET', f'{characters}?q=%C3%A4%20ss'),
        _req('GET', f'{characters}?q=spine'),
        _req('GET', f'{characters}?q=101'),
        _req('GET', f'{characters}?q=Kai'),
        _req('GET', f'{characters}?q=nothing-matches'),
        _req('GET', f'{characters}?q='),
        _req('GET', f'{characters}?limit=0&offset=-1'),
        _req('GET', f'{characters}?limit=501'),
        _req('GET', f'{characters}?limit=x'),
        _req('GET', sidebar),
        _req('GET', f'{sidebar}?q=%E6%A0%87%E6%9E%AA'),
        _req('GET', f'{sidebar}?q='),
        _req('GET', sidebar, headers={**AUTH, 'If-None-Match': '"nope", *'}),
        _req('GET', '/api/v2/azurlane/skin-updates'),
        _req('GET', '/api/v2/azurlane/skin-updates', headers={**AUTH, 'If-None-Match': '*'}),
        _req('GET', f'{characters}/javelin'),
        _req('GET', f'{characters}/%20javelin%20'),
        _req('GET', f'{characters}/yuzhang'),
        _req('GET', f'{characters}/xiafei'),
        _req('GET', f'{characters}/ayanami'),
        _req('GET', f'{characters}/%C3%89chelle'),
        _req('GET', f'{characters}/laffey'),
        _req('GET', f'{characters}/missing'),
        _req('GET', f'{characters}/' + 'k' * 201),
        _req('POST', f'{characters}/javelin'),
        _req('GET', f'{characters}/javelin/ship-detail'),
        _req('GET', f'{characters}/z23/ship-detail'),
        _req('GET', f'{characters}/unicorn/ship-detail'),
        _req('GET', f'{characters}/hammann/ship-detail'),
        _req('GET', f'{characters}/missing/ship-detail'),
        _req('GET', '/api/v2/azurlane/assets/javelin/assets/live2d/javelin/javelin.moc3'),
    ]


def _hostile_requests() -> list[dict[str, Any]]:
    characters = '/api/v2/azurlane/characters'
    return [
        _req('GET', characters),
        _req('GET', f'{characters}?q=javelin'),
        _req('GET', '/api/v2/azurlane/sidebar/characters'),
        _req('GET', '/api/v2/azurlane/sidebar/characters?q=javelin'),
        _req('GET', f'{characters}/hostile'),
        _req('GET', f'{characters}/javelin'),
        _req('GET', f'{characters}/hostile/ship-detail'),
        _req('GET', '/api/v2/azurlane/skin-updates'),
    ]


def _exchange(client: TestClient, request: dict[str, Any]) -> dict[str, Any]:
    response = client.request(request['method'], request['path'], headers=request['headers'])
    result: dict[str, Any] = {
        'status': response.status_code,
        'headers': {name: response.headers[name] for name in _RESPONSE_HEADERS if name in response.headers},
    }
    if response.headers.get('content-type', '').startswith('application/json'):
        result['json'] = response.json()
    else:
        result['text'] = response.text
    return {'request': request, 'response': result}


def _write_files(root: Path, files: dict[str, str]) -> None:
    for relative, text in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding='utf-8')


class _Runtime:
    def close(self) -> None:
        return None


def _replay(root: Path, requests: list[dict[str, Any]]) -> list[dict[str, Any]]:
    service = FavApiService(
        dsn='postgresql://goldens@127.0.0.1:1/never-connected',
        token=TOKEN,
        hanime1_video_fetcher=lambda _dsn: [],
        now_provider=lambda: datetime(2026, 5, 1, tzinfo=UTC),
        job_provider=list,
        runtime_service=_Runtime(),
        azurlane_library=AzurLaneLibrary(root),
    )
    exchanges: list[dict[str, Any]] = []
    with TestClient(create_app(service=service), raise_server_exceptions=False) as client:
        etags: dict[str, str] = {}
        for request in requests:
            exchange = _exchange(client, request)
            path = request['path']
            if path in {'/api/v2/azurlane/sidebar/characters', '/api/v2/azurlane/skin-updates'} and exchange['response']['status'] == 200:  # noqa: PLR2004
                etags.setdefault(path, exchange['response']['headers']['etag'])
            exchanges.append(exchange)
        # The matching ETag (alone, and inside a list) answers 304 with the same headers.
        for path, etag in etags.items():
            exchanges.extend(_exchange(client, _req('GET', path, headers={**AUTH, 'If-None-Match': value})) for value in [etag, f'"other", {etag}'])
    return exchanges


def _api() -> list[dict[str, Any]]:
    files = _fixture()
    hostile_files = {path: text for path, text in files.items() if not path.startswith('_source/')}
    hostile_files.update(_hostile())
    with TemporaryDirectory() as tmp:
        root = Path(tmp) / 'azurlane'
        _write_files(root, files)
        exchanges = _replay(root, _requests())
        hostile_root = Path(tmp) / 'azurlane-hostile'
        _write_files(hostile_root, hostile_files)
        hostile_exchanges = _replay(hostile_root, _hostile_requests())
    return [{'files': files, 'exchanges': exchanges, 'hostile_files': hostile_files, 'hostile_exchanges': hostile_exchanges}]


GOLDENS = {
    'azurlane/api.json': _api,
}
