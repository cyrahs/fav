# ruff: noqa: RUF001
# (The full-width punctuation in this file is GameKee's own labels.)
"""Nikke (WP9): the API reader of `src/api/nikke.py` and its routes.

`api.json` replays requests through the real FastAPI app (`create_app`) with the real
`FavApiService` and a real `NikkeLibrary` over collection directories written from each
scenario's `files`, so the summary cache, the payload builders, the response models
(`model_validate` and their dump), the query filter, the sidebar sort and `ETag`, the
parameter validation and the error envelopes are all Python's. The Live2D view-override
overlay (`_apply_live2d_view_overrides`) runs over a store with no saved overrides, so
the detail carries the model ids it assigns. A request header whose value is
`$etag` is sent with the `ETag` of the scenario's previous response.

`helpers.json` calls the small helpers directly: the skin-index order of
`_layer_metadata_payload_for_detail` (a sort over a set, so CPython's set order decides
ties) and the sidebar sort keys of `src/api/routes.py`.
"""

from __future__ import annotations

import json
import tempfile
from pathlib import Path
from typing import Any

from fastapi.testclient import TestClient
from src.api import routes
from src.api import service as service_module
from src.api.app import create_app
from src.api.nikke import NikkeLibrary, _to_int

from areas.api import FIXED_NOW, TOKEN, _patched, _State

AUTH = {'Authorization': f'Bearer {TOKEN}'}
_RESPONSE_HEADERS = ('content-type', 'cache-control', 'etag')
_SHA_A = 'a' * 64
_SHA_B = 'b' * 64


class _NoOverrides:
    def list_for_character(self, *, source: str, content_id: int) -> list[dict[str, Any]]:  # noqa: ARG002
        return []


class _Closable:
    def close(self) -> None:
        return None


# ---------- fixtures ----------


def _json_file(path: str, payload: Any) -> dict[str, str]:
    return {'path': path, 'text': json.dumps(payload, ensure_ascii=False)}


def _text_file(path: str, text: str) -> dict[str, str]:
    return {'path': path, 'text': text}


def _context(label: str, *, row_index: int = 0, skin_index: int | None = 0, section: str = 'style', **extra: Any) -> dict[str, Any]:
    context: dict[str, Any] = {'section': section, 'row_index': row_index, 'label': label}
    if skin_index is not None:
        context['skin_index'] = skin_index
    context.update(extra)
    return context


def _asset(
    local_path: str, *contexts: dict[str, Any], kind: str = 'image', sha256: str = _SHA_A, size: Any = 4, **extra: Any
) -> dict[str, Any]:
    asset = {
        'url': f'https://cdn.example.test/{Path(local_path).name}',
        'kind': kind,
        'local_path': local_path,
        'sha256': sha256,
        'content_type': 'image/png' if kind == 'image' else 'application/octet-stream',
        'size': size,
        'status': 'downloaded',
        'error': '',
        'contexts': list(contexts),
    }
    asset.update(extra)
    return asset


def _text_info(value: str, row_index: int = 1) -> list[dict[str, Any]]:
    return [{'row_index': row_index, 'values': [{'type': 'image', 'value': 'x.png'}, {'type': 'text', 'value': value}]}]


def _model(stable_id: str, live2d_key: str, *, skin_index: Any = 0, **extra: Any) -> dict[str, Any]:
    model: dict[str, Any] = {
        'label': 'live2d（full）',
        'section': 'style',
        'row_index': 5,
        'skin_index': skin_index,
        'skin_name': 'Skin 1',
        'skin_title': 'Default',
        'key': live2d_key,
        'stable_id': stable_id,
        'live2d_key': live2d_key,
        'position': {'pc': {'large': {'x': 1}}},
        'urls': {'atlas': f'https://cdn.example.test/{live2d_key}/model.atlas'},
    }
    model.update(extra)
    return model


def _runtime(prefix: str, **overrides: Any) -> dict[str, Any]:
    animations = {
        'idle': {
            'animation': f'{prefix}-idle',
            'enabled': True,
            'loop': True,
            'match_method': 'gamekee-runtime-player',
            'match_confidence': 'high',
        },
        'click': {
            'animation': f'{prefix}-click',
            'enabled': True,
            'loop': False,
            'match_method': 'gamekee-runtime-player-event',
            'match_confidence': 'high',
            'duration_ms': 1200,
        },
    }
    animations.update(overrides)
    return animations


def _rich_character() -> list[dict[str, str]]:
    """A full page: profile, skins with voice lines, Live2D models in layers, assets."""
    root = '101 - Test NIKKE'
    skins = [
        {
            'skin_index': 0,
            'name': 'Skin 1',
            'title': ' Default\r\n',
            'series': '/',
            'obtain': '/',
            'is_collection_skin': 0,
            'rows': [
                {'row_index': 0, 'label': '时装名称', 'cells': [{'type': 'text', 'value': 'Default'}]},
                {
                    'row_index': 12,
                    'label': 'Greeting',
                    'cells': [
                        {'type': 'text', 'value': 'Greeting'},
                        {'type': 'text', 'value': 'Hello, Commander.'},
                        {'type': 'audio', 'value': '//cdn.example.test/voice.mp3'},
                        {'type': 'audio', 'value': ' https://cdn.example.test/voice-2.mp3 '},
                    ],
                },
                {
                    'row_index': 13,
                    'label': '',
                    'cells': [{'type': 'text', 'value': 'Fallback'}, {'type': 'audio', 'value': '//cdn.example.test/v3.mp3'}],
                },
                {'row_index': 14, 'label': 'Silent', 'cells': [{'type': 'text', 'value': 'No audio'}]},
                {'row_index': 15, 'cells': 'not cells'},
            ],
        },
        {
            'skin_index': '1',
            'name': 'Skin 2',
            'title': 'Summer',
            'series': 'Beach',
            'obtain': 'Event',
            'is_collection_skin': True,
            'rows': [],
        },
        'not a skin',
    ]
    models = [
        _model(
            'stable-main',
            'main',
            layer_order=2,
            source_z_index=9,
            source_layer_index=0,
            is_primary=True,
            layer_match_method='gamekee-player-init',
            layer_match_confidence='high',
            runtime_animations=_runtime('main'),
            runtime_animation_match_method='gamekee-runtime-player',
            runtime_animation_match_confidence='high',
        ),
        _model(
            'stable-back',
            'back',
            layer_order=1,
            source_z_index=8,
            source_layer_index=1,
            is_primary=False,
            layer_match_confidence=' high ',
            runtime_animations=_runtime('back', click={'animation': '', 'enabled': False, 'matchConfidence': 'high', 'durationMs': -5}),
            bg_position={'x': 3},
        ),
        _model(
            'stable-front',
            'front',
            layer_order='3',
            is_primary=False,
            layer_match_confidence='low',
            runtime_animations=_runtime(
                'front', idle={'animation': ' ', 'matchMethod': 'camel', 'matchConfidence': 'high', 'loop': 'no'}, click=None
            ),
        ),
        _model('', 'aim', label='live2d（aim）', layer_order=4),
        _model('cover-1', '', label='cover', urls={'skel': ['https://cdn.example.test/cover/x.skel']}),
        _model('summer', 'summer', skin_index='1', layer_order=1, is_primary=True, layer_match_confidence='high', limit_age=1),
        'not a model',
    ]
    assets = [
        _asset(
            'assets/images/icon.png',
            {'section': 'tj_list', 'field': 'icon', 'label': 'icon'},
            {'section': 'base', 'row_index': 15, 'label': '部队成员1'},
            sha256=_SHA_B,
        ),
        _asset('assets/images/thumb.webp', _context('时装图（切换）', row_index=2)),
        _asset('assets/images/portrait.png', _context('时装立绘', row_index=8)),
        _asset('assets/images/sd.gif', _context('SD模型', row_index=9)),
        _asset('assets/images/burst.gif', _context('爆裂动画', row_index=10)),
        _asset(
            'assets/images/summer thumb.webp',
            _context('时装图（切换）', row_index=2, skin_index=1),
            _context('时装图（切换）', row_index=1, skin_index=1),
        ),
        _asset('assets/images/company.png', _context('企业', section='base', row_index=5, skin_index=None)),
        _asset('assets/images/weapon.png', _context('武器', section='base', row_index=10, skin_index=None), sha256='', size='12'),
        _asset('assets/images/missing.png', _context('时装立绘', row_index=8, skin_index=1)),
        _asset('assets/images/../escape.png', _context('gallery', row_index=3), size=True),
        _asset(
            'assets/live2d/main/model.atlas',
            _context('live2d（full）', row_index=5, live2d_key='main', live2d_field='atlas'),
            kind='live2d_atlas',
        ),
        _asset(
            'assets/live2d/main/model.skel',
            _context('live2d（full）', row_index=5, live2d_key='main', live2d_field='skel'),
            kind='live2d_skel',
        ),
        _asset(
            'assets/live2d/main/model.json',
            _context('live2d（full）', row_index=5, live2d_key='main', live2d_field='json'),
            kind='live2d_json',
        ),
        _asset(
            'assets/live2d/main/model2.png',
            _context('live2d（full）', row_index=5, live2d_key='main', live2d_field='atlas_texture'),
            kind='live2d_texture',
        ),
        _asset(
            'assets/live2d/main/model.png',
            _context('live2d（full）', row_index=5, live2d_key='main', live2d_field='atlas_texture'),
            kind='live2d_texture',
        ),
        _asset(
            'assets/live2d/back/model.skel',
            _context('live2d（full）', row_index=5, live2d_key='back', live2d_field='skel'),
            kind='live2d_skel',
        ),
        _asset(
            'assets/live2d/back/model.json',
            _context('live2d（full）', row_index=5, live2d_key='back', live2d_field='json'),
            kind='live2d_json',
        ),
        _asset(
            'assets/live2d/back/%6Dodel.atlas',
            _context('live2d（full）', row_index=5, live2d_key='back', live2d_field='atlas'),
            kind='live2d_atlas',
        ),
        'not an asset',
    ]
    base_info = {
        '稀有度': _text_info('SSR'),
        '企业': _text_info('Elysion', 5),
        '阶段': [{'row_index': 6, 'values': [{'type': 'text', 'value': '  '}]}, {'row_index': 7, 'values': 'x'}, 'row'],
        '属性': _text_info('Fire', 8),
        '职业': _text_info('Attacker', 9),
        'CV': _text_info('Test CV', 2),
        '实装日期': _text_info('2024/01/01', 20),
    }
    capture = {
        'schema': 2,
        'status': 'success',
        'captured_at': '2026-04-30T00:00:02+00:00',
        'attempted_at': '',
        'capture_hash': 'capture-hash',
        'fingerprint': 'fingerprint',
        'retryable': 'yes',
        'warnings': ['  safe warning ', 3, None],
        'quality_issues': [
            {
                'severity': 'error',
                'code': 'global_issue',
                'models': ['stable-main/main', 7, 1.5, True, None],
                'raw_container': {'secret': True},
                'orders': 'x',
            },
            {'severity': 'warning', 'code': 'skin_one', 'skin_index': '1', 'model_kind': 'cover', 'player_index': 2.5},
            {'skin_index': 'abc', 'code': 'bad_index', 'states': ['idle']},
            {'unknown': 1},
            'not an issue',
        ],
        'skipped': [{'code': 'skipped_group', 'reason': 'single_full', 'skin_index': 0}],
        'blocked': 'not a list',
    }
    manifest = {
        'source_url': 'https://www.gamekee.com/nikke/tj/101.html ',
        'fetched_at': '2026-04-30T00:00:00+00:00',
        'content_id': 101,
        'title': 'Test NIKKE',
        'updated_at': 123,
        'tj_list': {'content_id': 101, 'name': 'Test NIKKE', 'level': 'SSR', 'qy': 'Elysion', 'jd': 'III', 'wq': 'AR', 'bd': ''},
        'base_info': base_info,
        'content_summary': {'skins': skins},
        'live2d_models': models,
        'assets': assets,
        'asset_counts': {'image': 9, 'live2d_atlas': '2', 'live2d_skel': 2.0, 'live2d_texture': True},
        'live2d_layer_capture': capture,
    }
    character = {
        'content_id': 101,
        'title': 'Test NIKKE',
        'tj_list': manifest['tj_list'],
        'base_info': base_info,
        'skins': skins,
        'live2d_models': models,
        'live2d_layer_capture': capture,
    }
    files = [_json_file(f'{root}/manifest.json', manifest), _json_file(f'{root}/character.json', character)]
    files.extend(
        _text_file(f'{root}/{path}', 'x')
        for path in (
            'assets/images/icon.png',
            'assets/images/thumb.webp',
            'assets/images/portrait.png',
            'assets/images/sd.gif',
            'assets/images/burst.gif',
            'assets/images/summer thumb.webp',
            'assets/images/company.png',
            'assets/escape.png',
            'assets/live2d/main/model.atlas',
            'assets/live2d/main/model.png',
            'assets/live2d/back/model.atlas',
        )
    )
    files.append(_text_file(f'{root}/assets/live2d/main/model.skel', 'hash 4.1.24 skel'))
    files.append(_text_file(f'{root}/assets/live2d/main/model.json', '{"skeleton":{"spine":"4.2.11"}}'))
    files.append(_text_file(f'{root}/assets/live2d/back/model.skel', 'no version here'))
    files.append(_text_file(f'{root}/assets/live2d/back/model.json', '{"skeleton":{"spine":"4.0.64"}}'))
    return files


def _fallback_characters() -> list[dict[str, str]]:
    """Pages without some of their files or fields."""
    files: list[dict[str, str]] = []
    # No content id or title in the manifest: both come from the directory name. The
    # character file is broken, so the detail falls back to the manifest.
    files.append(
        _json_file(
            '202 - From Directory/manifest.json',
            {
                'updated_at': '2025-01-02T03:04:05Z',
                'fetched_at': '2025-01-02T03:04:06',
                'tj_list': {'level': 'SR', 'qy': 'Missilis', 'attr': 'Water', 'zy': 'Defender'},
                'content_summary': {'skins': [{'skin_index': 0, 'name': 'Only', 'rows': [{'row_index': 1}]}]},
                'live2d_models': [_model('a', 'a', skin_index=None), _model('b', 'b', skin_index=None, is_primary=True)],
                'assets': [
                    _asset('assets/images/icon1.png', {'section': 'tj_list', 'field': 'icon1', 'label': 'x'}, sha256=''),
                    _asset('assets/images/face.png', {'section': 'base', 'row_index': 2, 'label': '部队成员1'}),
                ],
                'live2d_layer_capture': {'status': 'failed', 'reason': 'timeout', 'error_class': 'TimeoutError', 'schema': '2'},
            },
        )
    )
    files.append(_text_file('202 - From Directory/character.json', '{not json'))
    files.append(_text_file('202 - From Directory/assets/images/icon1.png', 'x'))
    # The content id is a digit string; the title is empty, and so is the directory's suffix.
    files.append(
        _json_file(
            '303 -  /manifest.json',
            {
                'content_id': ' 303 ',
                'title': '  ',
                'updated_at': True,
                'fetched_at': None,
                'asset_counts': 'none',
                'content_summary': {'skins': 'not a list'},
                'live2d_models': 'not a list',
                'base_info': {'实装日期': _text_info('2023年5月7日')},
            },
        )
    )
    files.append(_json_file('303 -  /character.json', ['not', 'an', 'object']))
    # Skin indexes that tie under `value or -1`: None, 0 and -1, with layer issues in each.
    files.append(
        _json_file(
            '404 - Ties/manifest.json',
            {
                'content_id': 404,
                'title': 'ties',
                'updated_at': 1.0,
                'live2d_models': [
                    _model('n1', 'n1', skin_index=None),
                    _model('z1', 'z1', skin_index=0),
                    _model('m1', 'm1', skin_index=-1),
                    _model('n2', 'n2', skin_index=None),
                    _model('z2', 'z2', skin_index='0'),
                    _model('m2', 'm2', skin_index=-1),
                    _model('t1', 't1', skin_index=2, layer_order=1, is_primary=True, layer_match_confidence='high'),
                    _model('t2', 't2', skin_index=2, layer_order=1, is_primary=True, layer_match_confidence='medium'),
                ],
                'live2d_layer_capture': {'status': 'incomplete', 'layer_capture_schema': 1},
                'base_info': {'实装日期': _text_info('2024-02-30')},
            },
        )
    )
    # Ignored: an underscore directory, a directory without a manifest, a file, an
    # unreadable manifest, a manifest without any id, and a second page with id 101's
    # neighbour, 505, whose later directory wins.
    files.append(_json_file('_blobs/manifest.json', {'content_id': 999, 'title': 'Ignored'}))
    files.append(_text_file('606 - No Manifest/character.json', '{}'))
    files.append(_text_file('notes.txt', 'x'))
    files.append(_text_file('707 - Broken/manifest.json', '[1, 2'))
    files.append(_json_file('Nameless/manifest.json', {'title': 'No id'}))
    files.append(_json_file('505 - First/manifest.json', {'content_id': 505, 'title': 'Zeta first', 'updated_at': 5}))
    files.append(_json_file('505 - Second/manifest.json', {'content_id': 505, 'title': 'alpha second', 'updated_at': 5}))
    return files


def _invalid_detail_characters() -> list[dict[str, str]]:
    """Details the response model refuses, and one the library cannot build."""
    return [
        _json_file(
            '801 - Bad Rows/manifest.json',
            {'content_id': 801, 'title': 'Bad Rows', 'content_summary': {'skins': [{'skin_index': 0, 'rows': ['x']}]}},
        ),
        _json_file('802 - Bad Tj/manifest.json', {'content_id': 802, 'title': 'Bad Tj', 'tj_list': ['x']}),
        _json_file('803 - Null Skins/manifest.json', {'content_id': 803, 'title': 'Null Skins', 'content_summary': {'skins': None}}),
        _json_file('804 - Number Skins/manifest.json', {'content_id': 804, 'title': 'Number Skins', 'content_summary': {'skins': 4}}),
        _json_file('805 - Dict Skins/manifest.json', {'content_id': 805, 'title': 'Dict Skins', 'content_summary': {'skins': {'a': {}}}}),
    ]


def _invalid_summary_characters() -> list[dict[str, str]]:
    """A summary the list's response model refuses (`updated_at` is a fraction)."""
    return [
        _json_file('901 - Good/manifest.json', {'content_id': 901, 'title': 'Good'}),
        _json_file('902 - Fraction/manifest.json', {'content_id': 902, 'title': 'Fraction', 'updated_at': 1.5}),
    ]


def _sidebar_order_characters() -> list[dict[str, str]]:
    """Every sidebar sort key: implementation date, update time, fetch time, id."""
    pages = [
        (1, 'Same date older', '2024/01/01', 100, '2026-01-01T00:00:00Z'),
        (2, 'Same date newer', '2024/1/1', 200, None),
        (3, 'No date', None, '2026-05-01T00:00:00+08:00', None),
        (4, 'Bad date', '2024/13/01', ' 300 ', None),
        (5, 'Two numbers', '2024/01', None, '2026-05-01 01:02:03.5'),
        (6, 'Tie a', '2023/06/06', None, None),
        (7, 'Tie b', '2023/06/06', None, None),
        (8, 'Text time', '2022/02/02', 'yesterday', '2026-05-01T00:00:00Z'),
    ]
    files = []
    for content_id, title, implemented_at, updated_at, fetched_at in pages:
        manifest: dict[str, Any] = {'content_id': content_id, 'title': title, 'updated_at': updated_at, 'fetched_at': fetched_at}
        if implemented_at is not None:
            manifest['base_info'] = {'实装日期': _text_info(implemented_at)}
        files.append(_json_file(f'{content_id} - {title}/manifest.json', manifest))
    return files


def _list_requests() -> list[dict[str, Any]]:
    return [
        _req('/api/v2/nikke/characters'),
        _req('/api/v2/nikke/characters?q=elysion&limit=1'),
        _req('/api/v2/nikke/characters?q=MISSILIS'),
        _req('/api/v2/nikke/characters?q=101'),
        _req('/api/v2/nikke/characters?q=nothing-matches'),
        _req('/api/v2/nikke/characters?limit=2&offset=1'),
        _req('/api/v2/nikke/characters?offset=99999999999999999999999'),
        _req('/api/v2/nikke/characters?q=&limit=0&offset=-1'),
        _req('/api/v2/nikke/characters?limit=501'),
        _req('/api/v2/nikke/characters?limit=abc'),
        _req('/api/v2/nikke/characters', headers={}),
        _req('/api/v2/nikke/sidebar/characters'),
        _req('/api/v2/nikke/sidebar/characters', headers={**AUTH, 'If-None-Match': '$etag'}),
        _req('/api/v2/nikke/sidebar/characters', headers={**AUTH, 'If-None-Match': 'W/"x", *'}),
        _req('/api/v2/nikke/sidebar/characters', headers={**AUTH, 'If-None-Match': '"other"'}),
        _req('/api/v2/nikke/sidebar/characters?q=ssr'),
        _req('/api/v2/nikke/sidebar/characters?q='),
    ]


def _req(path: str, *, headers: dict[str, str] | None = None, method: str = 'GET') -> dict[str, Any]:
    return {'method': method, 'path': path, 'headers': AUTH if headers is None else headers}


def _detail_requests(*content_ids: str) -> list[dict[str, Any]]:
    return [_req(f'/api/v2/nikke/characters/{content_id}') for content_id in content_ids]


def _scenarios() -> list[dict[str, Any]]:
    return [
        {
            'name': 'rich page',
            'files': _rich_character(),
            'requests': [*_list_requests(), *_detail_requests('101', '0', '-3', 'abc', '102', '99999999999999999999999')],
        },
        {
            'name': 'fallbacks, ties and ignored directories',
            'files': _fallback_characters(),
            'requests': [*_list_requests(), *_detail_requests('202', '303', '404', '505', '707', '999')],
        },
        {
            'name': 'details the model refuses',
            'files': _invalid_detail_characters(),
            'requests': [_req('/api/v2/nikke/characters'), *_detail_requests('801', '802', '803', '804', '805')],
        },
        {
            'name': 'a summary the model refuses',
            'files': _invalid_summary_characters(),
            'requests': [
                _req('/api/v2/nikke/characters'),
                _req('/api/v2/nikke/characters?q=good'),
                _req('/api/v2/nikke/sidebar/characters'),
            ],
        },
        {
            'name': 'sidebar order',
            'files': _sidebar_order_characters(),
            'requests': [_req('/api/v2/nikke/sidebar/characters'), _req('/api/v2/nikke/sidebar/characters?q=tie')],
        },
        {'name': 'no collection', 'files': [], 'requests': [_req('/api/v2/nikke/characters'), *_detail_requests('1')]},
    ]


def _service(root: Path) -> service_module.FavApiService:
    service = service_module.FavApiService(
        dsn='postgresql://goldens@127.0.0.1:1/never-connected',
        token=TOKEN,
        hanime1_video_fetcher=lambda _dsn: [],
        now_provider=lambda: FIXED_NOW,
        job_provider=list,
        runtime_service=_Closable(),
        nikke_library=NikkeLibrary(root),
        live2d_view_override_store=_NoOverrides(),
    )
    return service


def _replay(scenario: dict[str, Any]) -> list[dict[str, Any]]:
    results: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory() as tmp, _patched(_State()):
        root = Path(tmp) / 'nikke'
        if scenario['files']:
            root.mkdir()
        for file in scenario['files']:
            path = root / file['path']
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(file['text'], encoding='utf-8')
        app = create_app(service=_service(root))
        etag = ''
        with TestClient(app, raise_server_exceptions=False) as client:
            for request in scenario['requests']:
                headers = {name: (etag if value == '$etag' else value) for name, value in request['headers'].items()}
                response = client.request(request['method'], request['path'], headers=headers)
                etag = response.headers.get('etag', etag)
                result: dict[str, Any] = {
                    'status': response.status_code,
                    'headers': {name: response.headers[name] for name in _RESPONSE_HEADERS if name in response.headers},
                }
                if response.headers.get('content-type', '').startswith('application/json'):
                    result['json'] = response.json()
                else:
                    result['text'] = response.text
                results.append({'request': request, 'response': result})
    return results


def _api() -> list[dict[str, Any]]:
    return [{'name': scenario['name'], 'files': scenario['files'], 'exchanges': _replay(scenario)} for scenario in _scenarios()]


# ---------- helpers ----------


def _skin_index_order(values: list[Any]) -> list[Any]:
    """The order `_layer_metadata_payload_for_detail` visits skins in."""
    return sorted({_to_int(value) for value in values}, key=lambda value: value or -1)


_SKIN_INDEX_CASES: list[list[Any]] = [
    [],
    [None],
    [0, None],
    [None, 0],
    [None, 0, -1],
    [-1, None, 0],
    [0, -1, None],
    ['0', None, '1', 2],
    [3, 2, 1, 0, None],
    [8, 16, 0, None, -1, 24],
    [None, *range(10)],
    [*range(10), None, -1],
    [*range(-3, 30, 2), None],
    [*range(40, -5, -3), None, 0],
    [2**61 - 1, 2**61, -(2**61), None, 0, -1],
    [4238894112, None, 0],
    [None, 4238894112, 0, 4238894112 + 8],
    [*range(100), None],
    [*range(200, 0, -7), None, -1, 0],
]


def _sort_keys() -> dict[str, Any]:
    timestamps = [
        None,
        True,
        0,
        12,
        1.5,
        '',
        '  ',
        ' 123 ',
        '١٢٣',
        '2026-05-01T00:00:00Z',
        '2026-05-01T00:00:00+08:00',
        '2026-05-01 01:02:03.5',
        '2026-05-01',
        '20260501T010203',
        '2026-05-01T00:00:00ZZ',
        'yesterday',
        '0001-01-01T00:00:00',
        [1],
    ]
    dates = [
        None,
        '',
        '2024/01/01',
        '2024/1/1',
        '2024年5月7日',
        '2024-02-29',
        '2023-02-29',
        '2024/13/01',
        '2024/01',
        '0/1/1',
        '9999/12/31',
        '10000/1/1',
        '٢٠٢٤/٠١/٠٢',
        'v1.2.3.4',
        12,
    ]
    return {
        'timestamps': [{'value': value, 'key': routes._sort_timestamp(value)} for value in timestamps],  # noqa: SLF001
        'dates': [{'value': value, 'key': routes._sort_date(value)} for value in dates],  # noqa: SLF001
    }


def _helpers() -> list[dict[str, Any]]:
    return [
        {'skin_index_order': [{'values': values, 'order': _skin_index_order(values)} for values in _SKIN_INDEX_CASES]},
        _sort_keys(),
    ]


GOLDENS = {
    'nikke/api.json': _api,
    'nikke/helpers.json': _helpers,
}
