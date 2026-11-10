"""BD2 (WP10): `src/web/bd2.py`, `src/tool/bd2_l2d_viewer.py`, `src/api/bd2.py` and
`src/api/live2d_overrides.py`.

`parsing.json` calls the pure helpers of the crawler and the viewer tool on inputs chosen
to reach their branches beyond what the tests cover.

`crawl.json` runs the real `BD2.update()` twice over a scripted GameKee (an
`httpx.MockTransport`) and a dictionary standing in for the `bd2_*` tables, with the
clock frozen and the retry jitter at zero. It records every file the runs write (the
manifests, `character.json`, the `raw/` documents byte for byte, every blob and asset by
size and sha256), the requests made, the notifications queued and the asset rows. The
Rust test answers the same script from its fake web and checks the same files against a
real database.

`api.json` replays requests through the real FastAPI app with the real `BD2Library` over a
fixture directory (its files are in the golden, so the Rust replay builds the same tree)
and an in-memory view-override store, so routing, validation, the response models'
coercion, the sidebar ETag and the error envelopes are all Python's.
"""

from __future__ import annotations

import asyncio
import contextlib
import hashlib
import json
from dataclasses import asdict, is_dataclass
from datetime import UTC, datetime
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import TYPE_CHECKING, Any

import httpx
from fastapi.testclient import TestClient
from src.api import live2d_overrides
from src.api.app import create_app
from src.api.bd2 import BD2Library
from src.api.service import FavApiService
from src.tool import bd2_l2d_viewer as viewer
from src.tool.bd2_l2d_viewer import ViewerResource
from src.web import bd2

if TYPE_CHECKING:
    from collections.abc import AsyncIterator

FIXED_NOW_ISO = '2026-05-01T00:00:00+00:00'
TOKEN = 'token-for-tests'  # noqa: S105
AUTH = {'Authorization': f'Bearer {TOKEN}'}


def _plain(value: Any) -> Any:
    if is_dataclass(value) and not isinstance(value, type):
        return {key: _plain(item) for key, item in asdict(value).items()}
    if isinstance(value, tuple | list | frozenset | set):
        items = [_plain(item) for item in value]
        return sorted(items) if isinstance(value, frozenset | set) else items
    if isinstance(value, dict):
        return {str(key) if not isinstance(key, tuple) else '\x1f'.join(key): _plain(item) for key, item in value.items()}
    if isinstance(value, Path):
        return value.as_posix()
    return value


def _attempt(call: Any) -> Any:
    try:
        return _plain(call())
    except Exception as exc:  # noqa: BLE001 - the error is the recorded answer
        return {'error': f'{exc.__class__.__name__}: {exc}'}


def _assets_plain(assets: dict[tuple[str, str], bd2.Asset]) -> list[dict[str, Any]]:
    return [_plain(asset) for asset in assets.values()]


def _assets_from(items: list[dict[str, Any]]) -> dict[tuple[str, str], bd2.Asset]:
    assets: dict[tuple[str, str], bd2.Asset] = {}
    for item in items:
        asset = bd2.Asset(**{**item, 'contexts': [dict(context) for context in item.get('contexts', [])]})
        assets[(asset.url, asset.kind)] = asset
    return assets


def _resource(  # noqa: PLR0913 - mirrors the Python dict fields
    entry_id: str, category: str, stem: str, char_name: str, costume_name: str, files: list[str], missing: list[str] | None = None
) -> dict[str, Any]:
    return {
        'entry_id': entry_id,
        'category': category,
        'stem': stem,
        'char_name': char_name,
        'costume_name': costume_name,
        'files': files,
        'missing_core_files': missing or [],
    }


def _viewer_resources(items: list[dict[str, Any]]) -> tuple[ViewerResource, ...]:
    return tuple(
        ViewerResource(**{**item, 'files': tuple(item['files']), 'missing_core_files': tuple(item['missing_core_files'])}) for item in items
    )


def _text(key: str, value: str) -> dict[str, Any]:
    return {'key': key, 'type': 'text', 'value': value}


def _image(key: str, value: str) -> dict[str, Any]:
    return {'key': key, 'type': 'image', 'value': value}


def _live2d(key: str, value: dict[str, Any]) -> dict[str, Any]:
    return {'key': key, 'type': 'live2d', 'value': value}


def _style_rows(*, title: str, category: str, live2d: dict[str, Any] | None, header: str = 'Live2D file') -> list[Any]:
    rows: list[Any] = [[_text(f'label_{index}', f'row-{index}'), _text(f'empty_{index}', '')] for index in range(16)]
    rows[0] = [_text('name_label', 'Costume name'), _text('costume_name', title)]
    rows[1] = [_text('category_label', 'Costume category'), _text('costume_category', category)]
    rows[2] = [
        _text('sprite_label', 'Costume sprite'),
        _image('bound_sprite', '/media/sprite.png'),
        _image('unbound_sprite', '//cdn.test/u.png'),
    ]
    rows[3] = [_text('portrait_label', 'Costume portrait'), _image('bound_portrait', '/media/portrait.png, /media/portrait2.png')]
    rows[5] = [_text('h', ''), _text('h1', header), _text('h2', 'Live2D icon'), _text('h3', 'Live2D 小人 file')]
    if live2d is not None:
        rows[6] = [_text('standing_label', 'Standing Live2D'), _live2d('bound_live2d', live2d), _image('icon', '/media/l2d-icon.png')]
    rows[8] = [
        _text('skill', 'Skill'),
        {'key': 'v', 'type': 'video', 'value': 'https://cdn.test/skill.mp4'},
        {'key': 'a', 'type': 'audio', 'value': '/a.mp3'},
    ]
    rows[9] = 'not a row'
    rows.append([{'key': 'odd', 'type': 'image', 'value': 7}, 'cell', {'type': 'gif', 'value': '/x.gif'}])
    return rows


_CONTENT_JSON = {
    'baseData': [
        [_text('name_label', '角色名称'), _text('name_cn', '英格利得'), _text('name_en', 'Ingrid')],
        [_text('rarity', 'Rarity'), _text('rarity_value', '5'), _image('rarity_icon', '/r.png')],
        [{'key': 'x', 'type': 'text', 'value': None}, _text('k', ' v ')],
        'junk',
        [],
    ],
    'styleData': [
        {
            'name': 'style-a',
            'data': _style_rows(
                title='Office Worker',
                category='Support',
                live2d={
                    'live2dKey': 'model-a',
                    'atlas': '/spine/char000101.atlas',
                    'skel': '/spine/char000101.skel',
                    'json': '',
                    'image': '/spine/char000101.png,,/spine/char000101_2.png',
                    'bg': '//cdn.test/bg.png',
                    'animation': 'idle',
                    'skin': 'default',
                    'limitAge': 1,
                    'position': {'pc': {'x': 1}},
                },
            ),
        },
        {'name': '', 'data': _style_rows(title='', category='', live2d=None, header='Live 2D 头像')},
        {'data': 'nope'},
        'junk',
    ],
}
_REVERSE_BIND = {
    'bound_sprite': 'stable-sprite',
    'bound_live2d': 'stable-live2d',
    'name_cn': 'stable-name',
    'rarity_value': 'stable-rarity',
}

_TREE = [
    {
        'id': 122323,
        'name': '5-star',
        'sort': '3',
        'content_id': 0,
        'child': [
            {
                'id': 1,
                'name': ' Alice ',
                'content_id': '1001',
                'sort': 2,
                'child': [{'id': 9, 'title': 'Nested', 'content_id': 1005, 'sort': None}],
            },
            {'id': 2, 'entry_name': 'Dup', 'content_id': 1001, 'sort': 1},
            {'id': 3, 'name': 'Zero', 'content_id': 0},
            {'id': 4, 'name': 'Neg', 'content_id': -5},
            {'id': 5, 'entryName': 'Str', 'content_id': ' 1003 ', 'sort': 'x'},
            {'id': 6, 'name': 'Bool', 'content_id': True},
        ],
    },
    {'id': '122318', 'name': '3-star', 'sort': 1, 'child': [{'id': 7, 'name': 'Bea', 'content_id': 1002, 'sort': 1}]},
    {'id': 189899, 'name': 'Equipment', 'child': [{'id': 8, 'name': 'Sword', 'content_id': 2001}]},
    'junk',
]

_VIEWER_LIST_SOURCE = """
// comment {not an object}
const x = 1;
export default {
  "000101": {"charName": "Lathel", "costumeName": "Herb \\"Tracker\\"", "spine": "char000101", "cutscene": "cutscene_char000101",
    "dating": "",},
  "000202": {"charName": "Odd", "costumeName": "{brace}", "spine": " char000202 ", "cutscene": "", "dating": "illust_dating2"},
  "minigame1": {"charName": "Minigame", "costumeName": "Slap Slap Pop", "spine": "RhythmHitAnim", "cutscene": "", "dating": ""},
  "skip": 5,
} as Record<string, unknown>;
"""
_VIEWER_TREE = {
    'tree': [
        {'type': 'blob', 'path': 'src/assets/spines/000101/char000101.atlas'},
        {'type': 'blob', 'path': 'src/assets/spines/000101/char000101.skel'},
        {'type': 'blob', 'path': 'src/assets/spines/000101/char000101.png'},
        {'type': 'blob', 'path': 'src/assets/spines/000101/char000101_2.png'},
        {'type': 'blob', 'path': 'src/assets/spines/000101/char0001010.png'},
        {'type': 'blob', 'path': 'src/assets/spines/000101/cutscene/cutscene_char000101.atlas'},
        {'type': 'blob', 'path': 'src/assets/spines/000101/cutscene/cutscene_char000101.json'},
        {'type': 'blob', 'path': 'src/assets/spines/000202/dating_nobg/illust_dating2.atlas'},
        {'type': 'blob', 'path': 'src/assets/spines/000202/dating_nobg/illust_dating2.skel'},
        {'type': 'blob', 'path': 'src/assets/spines/000202/dating/illust_dating2.png'},
        {'type': 'blob', 'path': 'src/assets/spines/minigame1/RhythmHitAnim.atlas'},
        {'type': 'blob', 'path': 'src/assets/spines/minigame1/RhythmHitAnim.skel'},
        {'type': 'blob', 'path': 'src/assets/spines/minigame1/RhythmHitAnim.txt'},
        {'type': 'tree', 'path': 'src/assets/spines/000101'},
        {'type': 'blob', 'path': 'README.md'},
        {'type': 'blob'},
        'junk',
    ],
}


def _viewer_cases(add: Any) -> None:
    add('parse_viewer_character_list', [_VIEWER_LIST_SOURCE], lambda: viewer.parse_viewer_character_list(_VIEWER_LIST_SOURCE))
    for source in [
        '{"a": {"x": 1}, "b": [1, 2,]}',
        'no braces',
        'export default {"a": 1',
        '[1, 2]',
        "export default {'a': 1}",
        'x {"a": "}"} y',
    ]:
        add('parse_viewer_character_list', [source], lambda source=source: viewer.parse_viewer_character_list(source))
    add('viewer_asset_paths_from_tree_payload', [_VIEWER_TREE], lambda: viewer.viewer_asset_paths_from_tree_payload(_VIEWER_TREE))
    add('viewer_asset_paths_from_tree_payload', [{'tree': {}}], lambda: viewer.viewer_asset_paths_from_tree_payload({'tree': {}}))
    characters = viewer.parse_viewer_character_list(_VIEWER_LIST_SOURCE)
    paths = viewer.viewer_asset_paths_from_tree_payload(_VIEWER_TREE)
    add(
        'viewer_resources_from_character_list',
        [characters, sorted(paths)],
        lambda: viewer.viewer_resources_from_character_list(characters, asset_paths=paths),
    )
    add('viewer_resources_from_character_list', [characters, None], lambda: viewer.viewer_resources_from_character_list(characters))
    for path in ['061492/dating/illust dating12.atlas', '/a//b/中.png/', '', '  /  ', 'a?b#c%/d']:
        add('viewer_asset_url', [path], lambda path=path: viewer.viewer_asset_url(path))
    for url in [
        'https://cdn.example.com/live2d/char000102.skel.atlas',
        'https://cdn.example.com/live2d/char000102.json.skel.json',
        'https://cdn.example.com/live2d/A%20B.PNG?x=1',
        'https://cdn.example.com/live2d/model.gif',
        'https://cdn.example.com/live2d/',
        '//cdn.example.com/x.webp',
        'relative/path/x.jpeg',
        '.atlas',
        'https://h/.skel.atlas',
    ]:
        add('resource_stem_from_url', [url], lambda url=url: viewer.resource_stem_from_url(url))
    for stem in ['cutscene_x', 'CUTSCENE_y', 'illust_dating1', 'Illust_Dating', 'char1', '']:
        add('category_from_stem', [stem], lambda stem=stem: viewer.category_from_stem(stem))
    for raw in ['', '  ', '//cdn/x', '/path', 'rel/x', 'https://a/b', 'https://a/b/../c']:
        add('normalize_resource_url', [raw], lambda raw=raw: viewer.normalize_resource_url(raw))
    manifest = {
        'content_id': '701014',
        'title': ' 斑鸠 ',
        'live2d_models': [
            {
                'live2d_key': 'xgp6bt8h',
                'urls': {
                    'json': 'https://c/char021001.json',
                    'atlas': 'https://c/char021001.atlas',
                    'image': ['https://c/char021001.png', 5, 'https://c/char021001.png'],
                    'atlas_textures': 'https://c/t1.webp,https://c/t2.webp',
                },
            },
            {'live2d_key': 'only-image', 'urls': {'image': '//c/illust_dating3.png', 'bg': '/bg.png'}},
            {'urls': {'bg': 'https://c/bg.jpg'}},
            {'urls': 'nope'},
            'junk',
        ],
    }
    add('gamekee_resources_from_manifest', [manifest], lambda: viewer.gamekee_resources_from_manifest(manifest, source_path='m.json'))
    add('gamekee_resources_from_manifest', [{'live2d_models': {}}], lambda: viewer.gamekee_resources_from_manifest({'live2d_models': {}}))
    detail = {
        'data': {
            'title': '',
            'name': 'Name',
            'content_json': json.dumps(
                {
                    'styleData': [
                        {'data': [[_live2d('k1', {'live2dKey': 'a', 'atlas': '//c/cutscene_x.atlas', 'skel': '//c/cutscene_x.skel'})]]},
                        {'data': [[_live2d('k2', {'live2dKey': 'b', 'atlas': '//c/cutscene_x.atlas', 'image': '//c/cutscene_x.png'})]]},
                        {'deep': {'type': 'live2d', 'value': {'live2dKey': 'c', 'image': '/char9.png'}}},
                        {'type': 'live2d', 'value': 'not a dict'},
                    ],
                },
            ),
        },
    }
    add(
        'gamekee_resources_from_detail_payload',
        [detail, {'content_id': 7}],
        lambda: viewer.gamekee_resources_from_detail_payload(detail, tree_row={'content_id': 7}),
    )
    add('gamekee_resources_from_detail_payload', [{'data': []}, None], lambda: viewer.gamekee_resources_from_detail_payload({'data': []}))
    add(
        'gamekee_resources_from_detail_payload',
        [{'data': {'content_json': '[1]', 'id': '3'}}, None],
        lambda: viewer.gamekee_resources_from_detail_payload({'data': {'content_json': '[1]', 'id': '3'}}),
    )
    add(
        'gamekee_resources_from_detail_payload',
        [{'data': {'content_json': '{bad'}}, None],
        lambda: viewer.gamekee_resources_from_detail_payload({'data': {'content_json': '{bad'}}),
    )
    gamekee = viewer.gamekee_resources_from_manifest(manifest, source_path='m.json')
    viewer_resources = viewer.viewer_resources_from_character_list(characters, asset_paths=paths)
    add(
        'compare_resources',
        [_plain(gamekee), _plain(viewer_resources)],
        lambda: viewer.compare_resources(gamekee_resources=gamekee, viewer_resources=viewer_resources).to_dict(),
    )


def _crawler_cases(add: Any) -> None:  # noqa: C901, PLR0912, PLR0915 - one block per function under test
    for target in [
        '1001',
        ' 1001 ',
        'https://www.gamekee.com/zsca2/tj/1001.html',
        'https://www.gamekee.com/zsca2/tj/1001.html?x=1#y',
        '/tj/55.html',
        'https://www.gamekee.com/zsca2/77.html',
        'https://www.gamekee.com/v1/content/detail/88?a',
        'https://www.gamekee.com/zsca2/tj/1001.htm',
        '¹²',
        '',
        'abc',
    ]:
        add('parse_content_id', [target], lambda target=target: bd2.parse_content_id(target))
    for content_id in [None, 0, 5]:
        add('request_headers', [content_id], lambda content_id=content_id: bd2.request_headers(content_id))
    add('cdn_request_headers', [], bd2.cdn_request_headers)
    add('asset_headers', [], bd2.asset_headers)
    for url in ['', '  ', '//a/b', '/x y', ' https://a/b ', 'rel', 'http://h/p']:
        add('normalize_url', [url], lambda url=url: bd2.normalize_url(url))
    for value in [None, True, 3, -3, ' 42 ', '4.2', '¹', '٣', '', [1]]:
        add('_to_int', [value], lambda value=value: bd2._to_int(value))  # noqa: SLF001
    for payload in [
        {'content': {'a': 1}},
        {'content': json.dumps({'styleData': []})},
        {'content': '[1]'},
        {'content': '{bad'},
        {'content': 5},
        {'baseData': []},
        {'styleData': {}},
        {},
    ]:
        add('content_json_from_cdn_payload', [payload], lambda payload=payload: bd2.content_json_from_cdn_payload(payload))
    for detail in [{'content_json': '{"a": 1}'}, {'content_json': {'b': 2}}, {'content_json': ' '}, {'content_json': 3}, {}]:
        add('parse_content_json', [detail], lambda detail=detail: bd2.parse_content_json(detail))
    for detail, field in [({'c': ' //x/y.json '}, 'c'), ({'c': ''}, 'c'), ({'c': 4}, 'c'), ({}, 'c')]:
        add('cdn_json_url', [detail, field], lambda detail=detail, field=field: bd2.cdn_json_url(detail, field))
    for payload in [{'entry_data_bind': {'s': 'd', 1: 2}}, {'entry_data_bind': '{"s": "d"}'}, {'s2': 'd2'}, {'entry_data_bind': 'bad'}]:
        add('reverse_bind_from_cdn_payload', [_plain(payload)], lambda payload=payload: bd2.reverse_bind_from_cdn_payload(payload))
    add('reverse_bind_map', [{'entry_data_bind': '{"s": 7}'}], lambda: bd2.reverse_bind_map({'entry_data_bind': '{"s": 7}'}))
    summary = bd2.summarize_content(_CONTENT_JSON, _REVERSE_BIND)
    add('summarize_content', [_CONTENT_JSON, _REVERSE_BIND], lambda: summary)
    add(
        'summarize_content',
        [{'baseData': 'x', 'styleData': None}, {}],
        lambda: bd2.summarize_content({'baseData': 'x', 'styleData': None}, {}),
    )
    add('base_info_from_summary', [summary], lambda: bd2.base_info_from_summary(summary))
    odd_summary = {
        'base_rows': [
            'x',
            {'label': 3, 'cells': []},
            {'label': 'L', 'cells': 'x'},
            {'label': 'L', 'row_index': 2, 'cells': [{}, {'is_bound': True, 'value': 0}, {'is_bound': True, 'value': ''}, 'z']},
        ]
    }
    add('base_info_from_summary', [odd_summary], lambda: bd2.base_info_from_summary(odd_summary))
    add('filter_bd2_character_rows', [_TREE], lambda: bd2.filter_bd2_character_rows(_TREE))
    add('filter_bd2_character_rows', [{'a': 1}], lambda: bd2.filter_bd2_character_rows({'a': 1}))
    for detail, row in [
        ({'title': ' T '}, None),
        ({'title': '', 'name': 'N'}, None),
        ({}, {'entryName': 'E'}),
        ({}, {}),
        ({'title': 5}, {'name': ' '}),
    ]:
        add('detail_name', [detail, 9, row], lambda detail=detail, row=row: bd2.detail_name(detail, 9, row))

    tree_row = {'icon': '/tree/icon.png, /tree/icon2.png', 'icon_small': ''}
    assets, models = bd2.extract_resources(content_json=_CONTENT_JSON, tree_row=tree_row, reverse_bind=_REVERSE_BIND)
    add('extract_resources', [_CONTENT_JSON, tree_row, _REVERSE_BIND], lambda: {'assets': _assets_plain(assets), 'models': models})

    viewer_items = [
        _resource(
            '000101',
            'character',
            'char000101',
            'Ingrid',
            'Office Worker',
            ['000101/char000101.atlas', '000101/char000101.skel', '000101/char000101.png'],
        ),
        _resource(
            '000101',
            'dating',
            'illust_dating7',
            'Ingrid',
            'Office Worker',
            ['000101/dating/illust_dating7.atlas', '000101/dating/illust_dating7.json'],
        ),
        _resource(
            '000101_c',
            'character',
            'char000101_c',
            'Ingrid',
            'Office Worker (C)',
            ['000101_c/char000101_c.atlas', '000101_c/char000101_c.skel'],
        ),
        _resource(
            '000101',
            'ultimate',
            'cutscene_char000101',
            'Ingrid',
            'Office Worker',
            ['000101/cutscene/cutscene_char000101.atlas'],
            ['000101/cutscene/cutscene_char000101.skel'],
        ),
        _resource('063001', 'character', 'char063001', 'Ingrid', 'Other', ['063001/char063001.atlas', '063001/char063001.skel']),
        _resource(
            'minigame1',
            'character',
            'RhythmHitAnim',
            'Minigame',
            'Slap Slap Pop',
            ['minigame1/RhythmHitAnim.atlas', 'minigame1/RhythmHitAnim.skel'],
        ),
        _resource('Minigame2', 'character', '', '', '', ['Minigame2/x.atlas', 'Minigame2/x.skel']),
        _resource('minigame3', 'character', 'm3', 'M', 'C', ['minigame3/m3.atlas'], ['minigame3/m3.skel']),
    ]
    resources = _viewer_resources(viewer_items)
    prefer_assets = _assets_from(_assets_plain(assets))
    prefer_models = json.loads(json.dumps(models))
    preferred = bd2.prefer_live2d_model_assets_from_viewer(assets=prefer_assets, live2d_models=prefer_models, viewer_resources=resources)
    add(
        'prefer_live2d_model_assets_from_viewer',
        [_assets_plain(assets), models, viewer_items],
        lambda: {'count': preferred, 'assets': _assets_plain(prefer_assets), 'models': prefer_models},
    )
    supplement_assets = _assets_from(_assets_plain(prefer_assets))
    supplement_models = json.loads(json.dumps(prefer_models))
    added = bd2.supplement_live2d_models_from_viewer(
        assets=supplement_assets,
        live2d_models=supplement_models,
        viewer_resources=resources,
        content_json=_CONTENT_JSON,
        reverse_bind=_REVERSE_BIND,
    )
    add(
        'supplement_live2d_models_from_viewer',
        [_assets_plain(prefer_assets), prefer_models, viewer_items, _CONTENT_JSON, _REVERSE_BIND],
        lambda: {'count': added, 'assets': _assets_plain(supplement_assets), 'models': supplement_models},
    )
    # The anchor path without page data, and the page path with mismatched owners.
    anchor_assets = _assets_from(_assets_plain(assets))
    anchor_models = json.loads(json.dumps(models))
    anchor_added = bd2.supplement_live2d_models_from_viewer(assets=anchor_assets, live2d_models=anchor_models, viewer_resources=resources)
    add(
        'supplement_live2d_models_from_viewer',
        [_assets_plain(assets), models, viewer_items, None, None],
        lambda: {'count': anchor_added, 'assets': _assets_plain(anchor_assets), 'models': anchor_models},
    )
    mismatch_items = [
        _resource('004202', 'character', 'char000101', 'Palette', 'Miracle Violet', ['004202/char000101.atlas', '004202/char000101.skel']),
        _resource('063001', 'character', 'char063001', 'ingrid ', "Kardis' Bullet", ['063001/char063001.atlas', '063001/char063001.json']),
        _resource(
            '063002', 'dating', 'illust_dating9', 'Ingrid', 'x', ['063002/dating/illust_dating9.atlas', '063002/dating/illust_dating9.skel']
        ),
    ]
    mismatch_assets = _assets_from(_assets_plain(assets))
    mismatch_models = json.loads(json.dumps(models))
    mismatch_added = bd2.supplement_live2d_models_from_viewer(
        assets=mismatch_assets,
        live2d_models=mismatch_models,
        viewer_resources=_viewer_resources(mismatch_items),
        content_json=_CONTENT_JSON,
        reverse_bind=_REVERSE_BIND,
    )
    add(
        'supplement_live2d_models_from_viewer',
        [_assets_plain(assets), models, mismatch_items, _CONTENT_JSON, _REVERSE_BIND],
        lambda: {'count': mismatch_added, 'assets': _assets_plain(mismatch_assets), 'models': mismatch_models},
    )
    add('viewer_extra_resources', [viewer_items], lambda: bd2.viewer_extra_resources(resources))
    for item in viewer_items[5:]:
        resource = _viewer_resources([item])[0]
        add('viewer_extra_title', [item], lambda resource=resource: bd2.viewer_extra_title(resource))
        add('viewer_extra_root', ['base', item], lambda resource=resource: bd2.viewer_extra_root(Path('base'), resource))
        add('viewer_extra_live2d_model', [item], lambda resource=resource: bd2.viewer_extra_live2d_model(resource))

    for kind in ['audio', 'video', 'live2d_atlas', 'live2d_skel', 'live2d_json', 'image', 'live2d_texture']:
        add('extension_for_kind', [kind], lambda kind=kind: bd2.extension_for_kind(kind))
    for url, kind in [
        ('https://a/b/%E4%B8%AD%20x.png?y=1', 'image'),
        ('https://a/b/', 'video'),
        ('https://a/b/c<d>.atlas', 'live2d_atlas'),
        ('https://a/' + 'n' * 200 + '.png', 'image'),
        ('https://a/.hidden', 'image'),
    ]:
        add('original_filename_from_url', [url, kind], lambda url=url, kind=kind: bd2.original_filename_from_url(url, kind))
    for url, kind, contexts in [
        ('https://a/b/x.png', 'image', [{'field': 'costume_sprite'}]),
        ('https://a/b/x', 'video', [{'stable_id': ' s '}]),
        ('https://a/b/x.PNG.', 'image', [{'label': 'L', 'key': ''}]),
        ('https://a/b/x.tar.gz', 'audio', [{'row_index': 4}]),
        ('https://a/b/x.png', 'image', []),
        ('https://a/b/x.atlas', 'live2d_atlas', [{'field': 'f'}]),
        ('https://a/b/x.' + 'e' * 40, 'image', [{'column_role': 'Role / Name'}]),
        ('https://a/b/', 'image', [{'live2d_field': 'bg'}]),
    ]:
        add(
            'filename_from_url',
            [url, kind, contexts, 7],
            lambda url=url, kind=kind, contexts=contexts: bd2.filename_from_url(url, kind, contexts, 7),
        )
    path_items = [
        {'url': 'https://a/1/x.png', 'kind': 'image', 'contexts': [{'field': 'f'}]},
        {'url': 'https://a/2/x.png', 'kind': 'image', 'contexts': [{'field': 'f'}]},
        {'url': 'https://a/1/m.atlas', 'kind': 'live2d_atlas', 'contexts': [{'live2d_key': 'k/1'}]},
        {'url': 'https://a/2/m.atlas', 'kind': 'live2d_atlas', 'contexts': [{'live2d_key': 'k/1'}]},
        {'url': 'https://a/3/m.atlas', 'kind': 'live2d_atlas', 'contexts': [{'live2d_key': 'k/1'}]},
        {'url': 'https://a/1/m.skel', 'kind': 'live2d_skel', 'contexts': [{'stable_id': ''}, {'key': 'kk'}]},
        {'url': 'https://a/1/m.json', 'kind': 'live2d_json', 'contexts': []},
        {'url': 'https://a/v.mp4', 'kind': 'video', 'contexts': [{}]},
        {'url': 'https://a/v.mp3', 'kind': 'audio', 'contexts': [{}]},
        {'url': 'https://a/1/x.png', 'kind': 'live2d_texture', 'contexts': [{'live2d_key': 'k/1'}]},
    ]
    path_assets = _assets_from(path_items)
    bd2.assign_asset_paths(path_assets, content_id=3)
    add('assign_asset_paths', [path_items, 3], lambda: _assets_plain(path_assets))
    add('asset_counts', [path_items], lambda: bd2.asset_counts(path_assets))
    add('output_root', ['base', {'title': 'A/B: C?'}, 5, None], lambda: bd2.output_root(Path('base'), {'title': 'A/B: C?'}, 5))
    add('output_root', ['base', {}, 5, {'name': '...'}], lambda: bd2.output_root(Path('base'), {}, 5, {'name': '...'}))
    add('live2d_atlas_texture_context', [models[0]], lambda: bd2.live2d_atlas_texture_context(models[0]))
    add('live2d_atlas_texture_context', [{}], lambda: bd2.live2d_atlas_texture_context({}))
    for context in [{}, {'b': 1, 'a': [1, 'é']}, {'x': 1.5, 'y': None}]:
        add('context_hash', [context], lambda context=context: bd2.context_hash(context))
    add(
        'content_hash',
        [{'data': {'t': 'é'}}, {'a': [1, 2.5]}, None],
        lambda: bd2.content_hash(detail_response={'data': {'t': 'é'}}, content_json={'a': [1, 2.5]}, tree_row=None),
    )
    now = datetime(2026, 5, 1, 0, 0, tzinfo=UTC)
    for value in [
        None,
        '',
        ' ',
        '2',
        ' 7 ',
        '999',
        '1.5',
        '-1',
        'Fri, 01 May 2026 00:00:30 GMT',
        'Fri, 01 May 2026 00:10:00 GMT',
        'Thu, 30 Apr 2026 23:00:00 GMT',
        'Fri, 01 May 2026 00:00:30',
        'garbage',
    ]:
        headers = {} if value is None else {'Retry-After': value}
        response = httpx.Response(429, headers=headers)
        add('retry_after_delay_seconds', [value], lambda response=response: bd2.retry_after_delay_seconds(response, now=now))
    for status in [400, 403, 404, 406, 408, 429, 500, 503, 567, 599, 301]:
        add('max_attempts_for_status', [status], lambda status=status: bd2.max_attempts_for_status(status))
    for count in [-1, 0, 1, 2, 3, 4, 99]:
        add('video_retry_cooldown_days', [count], lambda count=count: bd2.video_retry_cooldown_days(count))
    for url, kind, error in [
        ('https://cdn.test/v.mp4', 'video', ''),
        ('https://cdnimg-v2.gamekee.com/a.png', 'live2d_texture', 'HTTP 404'),
        ('https://cdnimg-v2.gamekee.com/a.png', 'live2d_texture', 'HTTP 567'),
        ('https://cdnimg-v2.gamekee.com/a.png', 'live2d_texture', 'HTTP 400'),
        ('https://cdnimg-v2.gamekee.com/a.png', 'live2d_texture', 'HTTP 500'),
        ('https://cdnimg-v2.gamekee.com/a.png', 'live2d_texture', 'HTTP 4040'),
        ('https://other.gamekee.com/a.png', 'live2d_texture', 'HTTP 404'),
        ('https://cdnimg-v2.gamekee.com/a.png', 'image', 'HTTP 404'),
    ]:
        asset = bd2.Asset(url=url, kind=kind, error=error)
        add('is_nonblocking_failed_asset', [url, kind, error], lambda asset=asset: bd2.is_nonblocking_failed_asset(asset))
    for kind, content_type, prefix, size in [
        ('image', 'image/png', 'PNG', 3),
        ('image', 'image/png', '', 0),
        ('image', 'Text/HTML; charset=utf-8', 'x', 1),
        ('image', '', '  <!DOCTYPE HTML>', 5),
        ('image', '', '<HTML>', 5),
        ('image', '', 'xx AccessDenied yy', 5),
        ('image', '', 'error code: 1020', 5),
        ('image', '', '{"code": 1}', 5),
        ('image', '', '{"data": 1}', 5),
        ('image', '', '{bad json', 5),
        ('live2d_json', 'application/json', '{"error": 1}', 5),
        ('image', '', '[{"code": 1}]', 5),
        ('image', '', '\xff\xfe{"msg": "x"}', 5),
    ]:
        add(
            'validate_asset_response',
            [kind, content_type, prefix, size],
            lambda kind=kind, content_type=content_type, prefix=prefix, size=size: bd2.validate_asset_response(
                kind=kind,
                content_type=content_type,
                body_prefix=prefix.encode('latin-1'),
                size=size,
            ),
        )
    for text in [
        'texture.png\nsize: 1,1\nformat: RGBA8888\n',
        '﻿A.PNG\r\n\nb.webp\nB.webp\na.png\nwith space.png\nc.jpeg\nd.jpg\ne.gif\nf.png:\n x.png \nregion\n  bounds: 1',
        '',
    ]:
        add('parse_spine_atlas_textures', [text], lambda text=text: bd2.parse_spine_atlas_textures(text))


def _parsing() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []

    def add(fn: str, args: list[Any], call: Any) -> None:
        cases.append({'fn': fn, 'args': _plain(args), 'result': _attempt(call)})

    _viewer_cases(add)
    _crawler_cases(add)
    return cases


# ---------- crawl ----------


class _FakeDatabase:
    """The `bd2_*` tables as dictionaries, answering the statements `BD2` runs."""

    def __init__(self) -> None:
        self.assets: dict[str, dict[str, Any]] = {}
        self.blobs: dict[tuple[str, int], dict[str, Any]] = {}
        self.pages: dict[int, dict[str, Any]] = {}
        self.page_assets: dict[int, list[tuple[str, str, str, str]]] = {}

    @contextlib.asynccontextmanager
    async def advisory_lock(self, _name: str) -> AsyncIterator[bool]:
        yield True

    async def query_db_multi(self, _sql: str) -> None:
        return None

    async def query_db_transaction(self, statements: list[tuple[str, tuple[Any, ...]]]) -> list[list[dict[str, Any]]]:
        for sql, params in statements:
            await self.query_db(sql, params)
        return [[] for _statement in statements]

    async def query_db(self, sql: str, params: tuple[Any, ...] = ()) -> list[dict[str, Any]]:  # noqa: C901, PLR0912
        text = ' '.join(sql.split())
        if text.startswith('UPDATE bd2_pages SET active = FALSE'):
            for page in self.pages.values():
                page['active'] = False
        elif text.startswith('INSERT INTO bd2_pages'):
            page = self.pages.setdefault(int(params[0]), {})
            page.update({'name': params[1], 'active': True})
        elif text.startswith('INSERT INTO bd2_assets (url, normalized_url, status, first_seen_at'):
            row = self.assets.get(params[0])
            if row is None:
                self.assets[params[0]] = {
                    'status': 'pending',
                    'sha256': '',
                    'size': 0,
                    'content_type': '',
                    'failed_count': 0,
                    'last_error': '',
                }
            elif row['status'] != 'downloaded':
                row['status'] = 'pending'
        elif text.startswith('SELECT failed_count, last_error, next_retry_at FROM bd2_assets'):
            return []
        elif text.startswith('SELECT a.sha256, a.size'):
            row = self.assets.get(params[0])
            if row is None or row['status'] != 'downloaded' or not row['sha256']:
                return []
            blob = self.blobs.get((row['sha256'], row['size']))
            if blob is None:
                return []
            return [
                {
                    'sha256': row['sha256'],
                    'size': row['size'],
                    'content_type': row['content_type'] or blob['content_type'],
                    'blob_path': blob['blob_path'],
                }
            ]
        elif text.startswith('UPDATE bd2_assets SET sha256 = ?'):
            row = self.assets[params[3]]
            row.update(
                {
                    'sha256': params[0],
                    'size': params[1],
                    'content_type': params[2],
                    'status': 'downloaded',
                    'failed_count': 0,
                    'last_error': '',
                }
            )
        elif text.startswith('INSERT INTO bd2_assets ( url, normalized_url, status, failed_count'):
            row = self.assets.setdefault(
                params[0], {'status': 'pending', 'sha256': '', 'size': 0, 'content_type': '', 'failed_count': 0, 'last_error': ''}
            )
            row.update({'status': 'failed', 'failed_count': row['failed_count'] + 1, 'last_error': params[2]})
        elif text.startswith('SELECT blob_path FROM bd2_blobs'):
            blob = self.blobs.get((params[0], params[1]))
            return [{'blob_path': blob['blob_path']}] if blob else []
        elif text.startswith('UPDATE bd2_blobs SET last_verified_at'):
            pass
        elif text.startswith('INSERT INTO bd2_blobs'):
            existing = self.blobs.get((params[0], params[1]))
            content_type = params[2] or (existing or {}).get('content_type', '')
            self.blobs[(params[0], params[1])] = {'content_type': content_type, 'blob_path': params[3]}
        elif text.startswith('DELETE FROM bd2_page_assets'):
            self.page_assets[int(params[0])] = []
        elif text.startswith('INSERT INTO bd2_page_assets'):
            self.page_assets.setdefault(int(params[0]), []).append((params[1], params[2], params[3], params[4]))
        elif text.startswith('UPDATE bd2_pages SET completed_at'):
            self.pages.setdefault(int(params[0]), {})['completed'] = True
        elif text.startswith("UPDATE bd2_assets SET status = 'missing'"):
            self.assets[params[0]].update({'status': 'missing', 'last_error': 'blob missing or failed verification'})
        else:
            msg = f'unexpected statement: {text[:80]}'
            raise AssertionError(msg)
        return []


def _atlas(*textures: str) -> str:
    lines: list[str] = []
    for texture in textures:
        lines.extend([texture, 'size: 2,2', 'format: RGBA8888', 'region', '  bounds: 0,0,1,1'])
    return '\n'.join(lines) + '\n'


def _detail_1001(*, second_run: bool) -> dict[str, Any]:
    style_a = _style_rows(
        title='Office Worker',
        category='Support',
        live2d={
            'live2dKey': 'model-a',
            'atlas': '/spine/char000101.atlas',
            'skel': '/spine/char000101.skel',
            'image': '/spine/char000101.png',
            'bg': '//cdn.test/bg.png',
        },
    )
    styles = [{'name': 'style-a', 'data': style_a}]
    if second_run:
        styles.append({'name': 'style-b', 'data': _style_rows(title='Summer', category='Attacker', live2d=None)})
    content = {
        'baseData': [
            [_text('name_label', '角色名称'), _text('name_cn', '英格利得'), _text('name_en', 'Ingrid')],
            [_text('rarity', 'Rarity'), _text('rarity_value', '5'), _image('rarity_icon', '/r.png')],
        ],
        'styleData': styles,
    }
    return {
        'code': 0,
        'msg': 'ok',
        'data': {
            'title': 'Ingrid',
            'entry_id': 50118,
            'updated_at': 1714521600,
            'content_json': json.dumps(content, ensure_ascii=False),
            'entry_data_bind': json.dumps({'stable-sprite': 'bound_sprite', 'stable-live2d': 'bound_live2d', 'stable-name': 'name_cn'}),
        },
    }


def _detail_1002() -> dict[str, Any]:
    return {
        'code': 0,
        'data': {
            'name': 'Bea',
            'updated_at': '2026-04-30',
            'content_json': '',
            'content_cdn': '//cdnimg-v2.gamekee.com/content/1002.json',
            'entry_data_bind_cdn': '//cdnimg-v2.gamekee.com/bind/1002.json',
        },
    }


def _web(*, second_run: bool) -> dict[str, list[dict[str, Any]]]:
    """URL -> the answers, in order (the last one repeats)."""
    tree = {
        'code': 0,
        'data': [
            {
                'id': 122323,
                'name': '5-star',
                'child': [{'id': 1, 'name': 'Ingrid', 'content_id': 1001, 'sort': 1, 'icon': '/tree/1001.png'}],
            },
            {'id': 122318, 'name': '3-star', 'child': [{'id': 2, 'name': 'Bea', 'content_id': '1002', 'sort': 2}]},
        ],
    }
    content_1002 = {
        'styleData': [
            {
                'name': 'bea-style',
                'data': [
                    [_text('n', 'Costume name'), _text('t', 'Bea Default')],
                    [_text('c', 'Costume category'), _text('cc', 'Tank')],
                    [_text('s', 'Costume sprite'), _image('bs', 'https://cdnimg-v2.gamekee.com/bea/sprite.png')],
                ],
            },
        ],
    }

    def ok(body: str, content_type: str = 'application/octet-stream', **headers: str) -> dict[str, Any]:
        return {'status': 200, 'headers': {'content-type': content_type, **headers}, 'body': body}

    def status(code: int, **headers: str) -> dict[str, Any]:
        return {'status': code, 'headers': dict(headers), 'body': ''}

    def json_ok(payload: Any) -> dict[str, Any]:
        return ok(json.dumps(payload, ensure_ascii=False), 'application/json')

    web: dict[str, list[dict[str, Any]]] = {
        'https://www.gamekee.com/v1/entry/treesByPid?pid=194491': [json_ok(tree)],
        'https://www.gamekee.com/v1/content/detail/1001': [json_ok(_detail_1001(second_run=second_run))],
        'https://www.gamekee.com/v1/content/detail/1002': [status(503, **{'retry-after': '1'}), json_ok(_detail_1002())],
        'https://cdnimg-v2.gamekee.com/content/1002.json': [json_ok({'content': json.dumps(content_1002)})],
        'https://cdnimg-v2.gamekee.com/bind/1002.json': [json_ok({'entry_data_bind': {'stable-bea-sprite': 'bs'}})],
        'https://www.gamekee.com/media/sprite.png': [ok('sprite-bytes', 'image/png')],
        'https://cdn.test/u.png': [status(500), status(502), ok('unbound', 'image/png')],
        'https://www.gamekee.com/media/portrait.png': [ok('portrait-bytes', 'image/png')],
        'https://www.gamekee.com/media/portrait2.png': [ok('portrait-bytes', 'image/png')],
        'https://www.gamekee.com/media/l2d-icon.png': [ok('<html>blocked</html>', 'text/html'), ok('icon-bytes', 'image/png')],
        'https://www.gamekee.com/spine/char000101.atlas': [ok(_atlas('char000101.png', 'char000101_2.png'), 'text/plain')],
        'https://www.gamekee.com/spine/char000101.skel': [ok('skel 4.1.24 bytes')],
        'https://www.gamekee.com/spine/char000101.png': [ok('tex-1', 'image/png')],
        'https://www.gamekee.com/spine/char000101_2.png': [ok('tex-2', 'image/png')],
        'https://cdn.test/bg.png': [ok('bg-bytes', 'image/png')],
        'https://cdn.test/skill.mp4': [status(403)],
        'https://www.gamekee.com/a.mp3': [ok('mp3-bytes', 'audio/mpeg')],
        'https://www.gamekee.com/r.png': [ok('{"code": 403, "msg": "denied"}', 'application/json')],
        'https://www.gamekee.com/tree/1001.png': [ok('tree-icon', 'image/png')],
        'https://cdnimg-v2.gamekee.com/bea/sprite.png': [status(404)],
        'https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/assets/spines/minigame1/RhythmHitAnim.atlas': [
            ok(_atlas('RhythmHitAnim.png'), 'text/plain'),
        ],
        'https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/assets/spines/minigame1/RhythmHitAnim.skel': [
            ok('rhythm-skel')
        ],
        'https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/assets/spines/minigame1/RhythmHitAnim.png': [
            ok('rhythm-png', 'image/png')
        ],
        'https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/assets/spines/000101_c/char000101_c.atlas': [
            ok(_atlas('char000101_c.png'), 'text/plain')
        ],
        'https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/assets/spines/000101_c/char000101_c.skel': [
            ok('censored-skel')
        ],
        'https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/assets/spines/000101_c/char000101_c.png': [
            ok('censored-png', 'image/png')
        ],
    }
    if second_run:
        # Already downloaded files are not fetched again; the red-icon JSON error is retried.
        web['https://www.gamekee.com/r.png'] = [ok('rarity-icon', 'image/png')]
    return web


_CRAWL_VIEWER = [
    _resource(
        '000101_c',
        'character',
        'char000101_c',
        'Ingrid',
        'Office Worker (Censored)',
        ['000101_c/char000101_c.atlas', '000101_c/char000101_c.skel'],
    ),
    _resource(
        'minigame1',
        'character',
        'RhythmHitAnim',
        'Minigame',
        'Slap Slap Pop',
        ['minigame1/RhythmHitAnim.atlas', 'minigame1/RhythmHitAnim.skel'],
    ),
]


def _tree_files(root: Path) -> dict[str, Any]:
    files: dict[str, Any] = {}
    for path in sorted(root.rglob('*')):
        if not path.is_file():
            continue
        relative = path.relative_to(root).as_posix()
        data = path.read_bytes()
        if path.suffix == '.json' and '/assets/' not in f'/{relative}' and not relative.startswith('_blobs/'):
            files[relative] = {'text': data.decode('utf-8')}
        else:
            files[relative] = {'size': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
    return files


def _crawl() -> list[dict[str, Any]]:
    database = _FakeDatabase()
    saved = (bd2.database, bd2._utc_now_iso, bd2.secrets.randbelow, bd2.asyncio.sleep, bd2.enqueue_notification)  # noqa: SLF001
    runs: list[dict[str, Any]] = []
    notifications: list[dict[str, Any]] = []
    real_sleep = asyncio.sleep

    async def no_sleep(_delay: float, *_args: Any) -> None:
        await real_sleep(0)

    async def enqueue(**payload: Any) -> None:
        notifications.append(payload)

    bd2.database = database
    bd2._utc_now_iso = lambda: FIXED_NOW_ISO  # noqa: SLF001
    bd2.secrets.randbelow = lambda _n: 0
    bd2.asyncio.sleep = no_sleep
    bd2.enqueue_notification = enqueue
    try:
        with TemporaryDirectory() as tmp:
            root = Path(tmp) / 'bd2'
            # An earlier title directory for 1001 is the discovery baseline, so the run that
            # finally succeeds announces the new skin.
            baseline = root / '1001 - Ingrid Old' / 'character.json'
            baseline.parent.mkdir(parents=True)
            baseline.write_text(json.dumps({'content_id': 1001, 'costumes': [{'style_name': 'style-a', 'title': 'Office Worker'}]}))
            for second_run in (False, True):
                web = _web(second_run=second_run)
                served: dict[str, int] = {}
                requests: list[dict[str, Any]] = []

                def handler(
                    request: httpx.Request,
                    web: dict[str, list[dict[str, Any]]] = web,
                    served: dict[str, int] = served,
                    requests: list[dict[str, Any]] = requests,
                ) -> httpx.Response:
                    url = str(request.url)
                    requests.append(
                        {
                            'method': request.method,
                            'url': url,
                            'referer': request.headers.get('referer', ''),
                            'user_agent': request.headers.get('user-agent', ''),
                        }
                    )
                    answers = web.get(url) or [{'status': 404, 'headers': {}, 'body': ''}]
                    index = served.get(url, 0)
                    served[url] = index + 1
                    answer = answers[min(index, len(answers) - 1)]
                    return httpx.Response(answer['status'], headers=answer['headers'], content=answer['body'].encode())

                notifications.clear()

                async def run(handler: Any = handler) -> str:
                    async with httpx.AsyncClient(
                        transport=httpx.MockTransport(handler),
                        base_url=bd2.GAMEKEE_BASE_URL,
                        follow_redirects=True,
                        headers=bd2.request_headers(),
                    ) as client:
                        crawler = bd2.BD2(path=root, client=client, viewer_resources=_viewer_resources(_CRAWL_VIEWER))
                        try:
                            await crawler.update()
                        except Exception as exc:  # noqa: BLE001 - the error is the recorded answer
                            return f'{exc.__class__.__name__}: {exc}'
                    return ''

                error = asyncio.run(run())
                runs.append(
                    {
                        'web': web,
                        'error': error,
                        'requests': sorted(requests, key=lambda item: (item['url'], item['method'])),
                        'files': _tree_files(root),
                        'notifications': [dict(item) for item in notifications],
                        'assets': {
                            url: {key: row[key] for key in ('status', 'sha256', 'size', 'failed_count', 'last_error')}
                            for url, row in sorted(database.assets.items())
                        },
                    },
                )
    finally:
        bd2.database, bd2._utc_now_iso, bd2.secrets.randbelow, bd2.asyncio.sleep, bd2.enqueue_notification = saved  # noqa: SLF001
    return [{'viewer_resources': _CRAWL_VIEWER, 'fetched_at': FIXED_NOW_ISO, 'runs': runs}]


# ---------- api ----------


def _ctx(row_index: int, label: str, field: str = '', **extra: Any) -> dict[str, Any]:
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
        'column_role': label,
        **extra,
    }


def _api_asset(  # noqa: PLR0913 - mirrors the manifest asset fields
    path: str,
    kind: str,
    context: dict[str, Any],
    *,
    sha256: str = 'a' * 64,
    content_type: str = 'image/png',
    size: Any = 4,
    contexts: Any = None,
) -> dict[str, Any]:
    return {
        'url': f'https://cdn.example.test/{Path(path).name}',
        'kind': kind,
        'local_path': path,
        'sha256': sha256,
        'content_type': content_type,
        'size': size,
        'status': 'downloaded',
        'error': '',
        'contexts': [context] if contexts is None else contexts,
    }


def _api_fixture() -> dict[str, str]:
    """Relative path -> file text, for the BD2 collection root the API reads."""
    files: dict[str, str] = {}

    def put(path: str, payload: Any) -> None:
        files[path] = payload if isinstance(payload, str) else json.dumps(payload, ensure_ascii=False)

    l2d = {**_ctx(6, 'Standing Live2D', 'standing_live2d'), 'live2d_key': 'model-a'}
    l2d_b = {**_ctx(7, 'Interaction Live2D', 'interaction_live2d'), 'live2d_key': 'model b/é'}
    root_a = '101 - Test BD2'
    for name, body in {
        'assets/images/icon.png': 'icon',
        'assets/images/sprite.png': 'sprite',
        'assets/images/portrait.png': 'portrait',
        'assets/videos/skill.mp4': 'video',
        'assets/live2d/model-a/model.atlas': 'atlas',
        'assets/live2d/model-a/model.skel': '\x00\x00skel 4.1.24 binary',
        'assets/live2d/model-a/model.json': '{"skeleton":{"spine":"4.0.64"}}',
        'assets/live2d/model-a/model.png': 'texture',
        'assets/live2d/model-a/bg.png': 'bg',
        'assets/live2d/model-b/m.json': '{"skeleton":{"spine":"3.8.99"}}',
        'outside.png': 'x',
    }.items():
        files[f'{root_a}/{name}'] = body
    assets = [
        _api_asset('assets/images/icon.png', 'image', {'section': 'entry_tree', 'field': 'icon', 'label': 'icon'}, sha256='b' * 64),
        _api_asset('assets/images/sprite.png', 'image', _ctx(2, 'Costume sprite', 'costume_sprite'), size='6'),
        _api_asset('assets/images/portrait.png', 'image', _ctx(3, 'Costume portrait', 'costume_portrait'), size=True),
        _api_asset(
            'assets/images/missing.png',
            'image',
            {**_ctx(3, 'Costume portrait', 'costume_portrait'), 'column_name': 'Other', 'style_index': '0'},
            sha256='',
        ),
        _api_asset('assets/videos/skill.mp4', 'video', _ctx(8, 'Skill', 'skill_live2d_1'), content_type='video/mp4'),
        _api_asset('assets/live2d/model-a/model.atlas', 'live2d_atlas', {**l2d, 'live2d_field': 'atlas'}),
        _api_asset('assets/live2d/model-a/model.skel', 'live2d_skel', {**l2d, 'live2d_field': 'skel'}),
        _api_asset('assets/live2d/model-a/model.json', 'live2d_json', {**l2d, 'live2d_field': 'json'}),
        _api_asset('assets/live2d/model-a/model.png', 'live2d_texture', {**l2d, 'live2d_field': 'atlas_texture'}),
        _api_asset('assets/live2d/model-a/bg.png', 'live2d_background', {**l2d, 'live2d_field': 'bg'}),
        _api_asset('assets/live2d/model-b/m.json', 'live2d_json', {**l2d_b, 'live2d_field': 'json'}),
        _api_asset('../outside.png', 'image', {'label': 'Escape'}),
        _api_asset('assets/../outside.png', 'image', {'label': 'Escape'}),
        _api_asset('', 'image', {'label': 'Empty'}, contexts=[{'label': 'Empty'}, 'junk', {'label': 'Empty', 'style_index': 1}]),
        _api_asset('assets/images/sprite.png', 'image', {'label': 'Again'}),
        'junk',
        {'kind': 'image', 'local_path': 'assets\\images\\icon.png', 'contexts': 'x', 'sha256': 'c d'},
    ]
    costume = {
        'style_index': 0,
        'style_name': 'style-a',
        'title': 'Office Worker',
        'category': 'Support',
        'rows': [{'row_index': 0, 'label': 'Costume name', 'cells': []}, {'row_index': 1, 'label': '  Office Worker  '}],
    }
    models = [
        {
            **l2d,
            'key': 'bound',
            'stable_id': 'stable-live2d',
            'animation': 'idle',
            'position': {'pc': {'x': 1}},
            'urls': {'atlas': 'https://cdn.example.test/model.atlas'},
            'limit_age': 1,
        },
        {**l2d_b, 'stable_id': '', 'source': 'bd2_l2d_viewer', 'variant': 'censored', 'position': [1], 'urls': 'x'},
        {**l2d_b, 'live2d_key': 'bad key!', 'style_index': 0, 'row_index': '7'},
        {**l2d_b, 'live2d_key': 'bad key!', 'style_index': 0, 'row_index': '7'},
        {'label': 'No ids', 'style_index': None},
        'junk',
    ]
    manifest_a = {
        'source_url': 'https://www.gamekee.com/zsca2/tj/101.html',
        'fetched_at': '2026-05-01T00:00:00+00:00',
        'content_id': 101,
        'title': 'Test BD2',
        'updated_at': 123,
        'tree_row': {'content_id': 101, 'name': 'Test BD2', '_bd2_group_name': ' 5-star '},
        'base_info': {
            'Rarity': [{'row_index': 1, 'values': [{'type': 'text', 'value': ' 5\r\nstar '}]}],
            'Class': [{'row_index': 2, 'values': [{'type': 'image', 'value': 'x'}, {'type': 'text', 'value': 'Support'}]}],
            'icon': [{'row_index': 3, 'values': [{'type': 'image', 'value': 'x'}]}],
            'Empty Row': [{'values': []}],
            'Élan Vital!': [{'values': [{'type': 'text', 'value': 7}]}],
        },
        'costumes': [costume, {'style_index': '1', 'title': 'Second', 'rows': 'x'}, 'junk'],
        'live2d_models': models,
        'assets': assets,
        'asset_counts': {'image': 5, 'video': '1', 'live2d_atlas': 1.0, 'flag': True},
    }
    character_a = {
        'content_id': 101,
        'title': 'Test BD2',
        'tree_row': manifest_a['tree_row'],
        'base_info': manifest_a['base_info'],
        'costumes': manifest_a['costumes'],
        'live2d_models': models,
    }
    put(f'{root_a}/manifest.json', manifest_a)
    put(f'{root_a}/character.json', character_a)

    root_b = '202 - Recent BD2'
    files[f'{root_b}/assets/images/recent-icon.png'] = 'recent-icon'
    put(
        f'{root_b}/manifest.json',
        {
            'fetched_at': '2026-05-01T00:00:01Z',
            'title': '',
            'updated_at': 999.0,
            'tree_row': [],
            'content_summary': {'costumes': [{'style_index': 0, 'title': 'Recent Costume', 'category': 'Attacker'}]},
            'live2d_models': {},
            'assets': [
                _api_asset(
                    'assets/images/recent-icon.png',
                    'image',
                    {'section': 'entry_tree', 'field': 'icon_small', 'label': 'icon_small'},
                    sha256='c' * 64,
                )
            ],
        },
    )
    put(f'{root_b}/character.json', '[1, 2]')

    put(
        '303 - Old Dup/manifest.json',
        {'content_id': 303, 'title': 'Old Dup', 'fetched_at': '2026-05-01T00:00:00+00:00', 'updated_at': '2026-04-01T00:00:00Z'},
    )
    put(
        '303 - New Dup/manifest.json',
        {'content_id': '303', 'title': 'New Dup', 'fetched_at': '2026-05-01T00:01:00+00:00', 'updated_at': 'later'},
    )
    put('404 - Broken/manifest.json', '{not json')
    put('405 - List/manifest.json', '[1]')
    put('No Id/manifest.json', {'title': 'No Id'})
    put('_extra/minigame1 - x/manifest.json', {'content_id': 999, 'title': 'Ignored'})
    put('506 - Float Update/manifest.json', {'content_id': 506, 'title': 'float update', 'updated_at': 1.5, 'fetched_at': 5})
    put(
        '507 - Bool Update/manifest.json',
        {'content_id': 507, 'title': 'Bool Update', 'updated_at': True, 'asset_counts': {'image': 'many'}},
    )
    files['empty-dir/.keep'] = ''
    files['loose.json'] = '{}'
    return files


class _OverrideStore:
    def __init__(self) -> None:
        self.rows: dict[tuple[str, int, str, str], dict[str, Any]] = {}

    def list_for_character(self, *, source: str, content_id: int) -> list[dict[str, Any]]:
        return [row for key, row in sorted(self.rows.items()) if key[0] == source and key[1] == content_id]

    def get(self, *, source: str, content_id: int, model_id: str, profile: str) -> dict[str, Any] | None:
        return self.rows.get((source, content_id, model_id, profile))

    def upsert(  # noqa: PLR0913 - mirrors the store signature
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
        existing = self.rows.get(key)
        row = {
            'source': source,
            'content_id': content_id,
            'model_id': model_id,
            'profile': profile,
            'position': dict(position),
            'scale': scale,
            'background_position': dict(background_position) if background_position is not None else None,
            'background_scale': background_scale,
            'created_at': existing['created_at'] if existing is not None else datetime(2026, 5, 1, 0, 0, 0, 500000, tzinfo=UTC),
            'updated_at': datetime(2026, 5, 2, 3, 4, 5, tzinfo=UTC),
        }
        self.rows[key] = row
        return row

    def delete(self, *, source: str, content_id: int, model_id: str, profile: str) -> bool:
        return self.rows.pop((source, content_id, model_id, profile), None) is not None


class _Runtime:
    def close(self) -> None:
        return None


def _req(method: str, path: str, *, headers: dict[str, str] | None = None, body: str | None = None) -> dict[str, Any]:
    request: dict[str, Any] = {'method': method, 'path': path, 'headers': AUTH if headers is None else headers}
    if body is not None:
        request['body'] = body
    return request


def _put(path: str, payload: Any) -> dict[str, Any]:
    return _req(
        'PUT', path, headers={**AUTH, 'content-type': 'application/json'}, body=payload if isinstance(payload, str) else json.dumps(payload)
    )


_OVERRIDE = '/api/v2/bd2/characters/101/live2d-models/stable-live2d/view-overrides/default'


def _api_requests() -> list[dict[str, Any]]:
    detail = '/api/v2/bd2/characters/101'
    return [
        _req('GET', '/api/v2/bd2/characters', headers={}),
        _req('GET', '/api/v2/bd2/characters'),
        _req('GET', '/api/v2/bd2/characters?q=support&limit=1'),
        _req('GET', '/api/v2/bd2/characters?q=OFFICE%20worker'),
        _req('GET', '/api/v2/bd2/characters?q=5%0Astar'),
        _req('GET', '/api/v2/bd2/characters?q=303'),
        _req('GET', '/api/v2/bd2/characters?q=recent%20costume'),
        _req('GET', '/api/v2/bd2/characters?limit=2&offset=1'),
        _req('GET', '/api/v2/bd2/characters?offset=99'),
        _req('GET', '/api/v2/bd2/characters?q='),
        _req('GET', '/api/v2/bd2/characters?limit=0&offset=-1'),
        _req('GET', '/api/v2/bd2/characters?limit=501'),
        _req('GET', '/api/v2/bd2/characters?limit=x'),
        _req('GET', '/api/v2/bd2/sidebar/characters'),
        _req('GET', '/api/v2/bd2/sidebar/characters?q=dup'),
        _req('GET', '/api/v2/bd2/sidebar/characters?q='),
        _req('GET', '/api/v2/bd2/sidebar/characters', headers={**AUTH, 'If-None-Match': '"nope", *'}),
        _req('GET', '/api/v2/bd2/sidebar/characters?q=nothing-matches'),
        _req('GET', detail),
        _req('GET', '/api/v2/bd2/characters/202'),
        _req('GET', '/api/v2/bd2/characters/303'),
        _req('GET', '/api/v2/bd2/characters/506'),
        _req('GET', '/api/v2/bd2/characters/507'),
        _req('GET', '/api/v2/bd2/characters/999'),
        _req('GET', '/api/v2/bd2/characters/0'),
        _req('GET', '/api/v2/bd2/characters/abc'),
        _req('POST', detail),
        _req('GET', '/api/v2/bd2/assets/101/assets/images/icon.png'),
        _req('GET', _OVERRIDE),
        _put(
            _OVERRIDE,
            {'position': {'x': 120.5, 'y': -32}, 'scale': 1.25, 'background_position': {'x': 0, 'y': '4'}, 'background_scale': 1.1},
        ),
        _req('GET', _OVERRIDE),
        _put(_OVERRIDE, {'position': {'x': 1, 'y': 2}, 'scale': '2'}),
        _put(
            '/api/v2/bd2/characters/101/live2d-models/stable-live2d/view-overrides/mobile',
            {'position': {'x': True, 'y': 0}, 'scale': 0.5, 'background_scale': None},
        ),
        _req('GET', detail),
        _put(_OVERRIDE, {'scale': 0}),
        _put(_OVERRIDE, {'position': {'x': 'a', 'y': None}, 'scale': -1, 'background_scale': 0}),
        _put(_OVERRIDE, {'position': {'x': 'inf', 'y': 1}, 'scale': 'Infinity'}),
        _put(_OVERRIDE, '{"position": {"x": NaN, "y": 1}, "scale": NaN}'),
        _put(_OVERRIDE, {'position': {'x': 1, 'y': 1, 'z': 1}, 'scale': 1, 'extra': True}),
        _put(_OVERRIDE, {'position': [1, 2], 'scale': 1, 'background_position': 'x'}),
        _put(_OVERRIDE, '[1]'),
        _put(_OVERRIDE, 'null'),
        _req('PUT', _OVERRIDE, headers={**AUTH, 'content-type': 'text/plain'}, body='{"scale": 1}'),
        _req('PUT', _OVERRIDE),
        _put('/api/v2/bd2/characters/101/live2d-models/missing-model/view-overrides/default', {'position': {'x': 1, 'y': 2}, 'scale': 1}),
        _put('/api/v2/bd2/characters/999/live2d-models/stable-live2d/view-overrides/default', {'position': {'x': 1, 'y': 2}, 'scale': 1}),
        _put('/api/v2/bd2/characters/0/live2d-models/bad%20id/view-overrides/bad.profile', {'scale': 1}),
        _req('GET', '/api/v2/bd2/characters/101/live2d-models/' + 'm' * 161 + '/view-overrides/' + 'p' * 65),
        _req('GET', '/api/v2/bd2/characters/101/live2d-models/live2d_key_3f0bd7cd6cbb8e46/view-overrides/default'),
        _req('GET', '/api/v2/bd2/characters/101/live2d-models/model%20b%2F%C3%A9/view-overrides/default'),
        _req('DELETE', '/api/v2/bd2/characters/101/live2d-models/stable-live2d/view-overrides/mobile'),
        _req('DELETE', _OVERRIDE),
        _req('DELETE', _OVERRIDE),
        _req('GET', _OVERRIDE),
        _req('DELETE', '/api/v2/bd2/characters/101/live2d-models/nope/view-overrides/default'),
        _req('GET', detail),
    ]


_RESPONSE_HEADERS = ('content-type', 'cache-control', 'etag', 'allow')
_HOSTILE_PREFIXES = ('506 - ', '507 - ')


def _api_hostile_requests() -> list[dict[str, Any]]:
    """Requests replayed after the manifests whose fields fail response validation are added."""
    return [
        _req('GET', '/api/v2/bd2/characters'),
        _req('GET', '/api/v2/bd2/characters?q=update'),
        _req('GET', '/api/v2/bd2/characters?q=test'),
        _req('GET', '/api/v2/bd2/sidebar/characters'),
        _req('GET', '/api/v2/bd2/sidebar/characters?q=test'),
        _req('GET', '/api/v2/bd2/characters/506'),
        _req('GET', '/api/v2/bd2/characters/507'),
    ]


def _exchange(client: TestClient, request: dict[str, Any]) -> dict[str, Any]:
    response = client.request(
        request['method'],
        request['path'],
        headers=request['headers'],
        content=request['body'].encode() if 'body' in request else None,
    )
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


def _api() -> list[dict[str, Any]]:
    files = _api_fixture()
    clean = {path: text for path, text in files.items() if not path.startswith(_HOSTILE_PREFIXES)}
    hostile = {path: text for path, text in files.items() if path.startswith(_HOSTILE_PREFIXES)}
    exchanges: list[dict[str, Any]] = []
    hostile_exchanges: list[dict[str, Any]] = []
    with TemporaryDirectory() as tmp:
        root = Path(tmp) / 'bd2'
        _write_files(root, clean)
        service = FavApiService(
            dsn='postgresql://goldens@127.0.0.1:1/never-connected',
            token=TOKEN,
            hanime1_video_fetcher=lambda _dsn: [],
            now_provider=lambda: datetime(2026, 5, 1, tzinfo=UTC),
            job_provider=list,
            runtime_service=_Runtime(),
            bd2_library=BD2Library(root),
            live2d_view_override_store=_OverrideStore(),
        )
        with TestClient(create_app(service=service), raise_server_exceptions=False) as client:
            etag = ''
            for request in _api_requests():
                exchange = _exchange(client, request)
                if request['path'] == '/api/v2/bd2/sidebar/characters' and exchange['response']['status'] == 200:  # noqa: PLR2004
                    etag = exchange['response']['headers']['etag']
                exchanges.append(exchange)
            # The matching ETag (alone, and inside a list) answers 304 with the same headers.
            exchanges.extend(
                _exchange(client, _req('GET', '/api/v2/bd2/sidebar/characters', headers={**AUTH, 'If-None-Match': value}))
                for value in [etag, f'"other", {etag}']
            )
            _write_files(root, hostile)
            hostile_exchanges.extend(_exchange(client, request) for request in _api_hostile_requests())
    return [{'files': clean, 'exchanges': exchanges, 'hostile_files': hostile, 'hostile_exchanges': hostile_exchanges}]


def _model_ids() -> list[dict[str, Any]]:
    """`live2d_model_id`, `assign_live2d_model_ids` and `apply_live2d_view_overrides`."""
    models = [
        {'stable_id': 'abc', 'live2d_key': 'k'},
        {'stable_id': ' ', 'live2d_key': 'key.with:colon-_'},
        {'stable_id': 'has space'},
        {'live2d_key': 'é'},
        {'live2d_key': 'x' * 161},
        {'live2d_key': 5},
        {},
        {'label': 'L', 'row_index': 1, 'source_urls': {'atlas': 'a'}, 'skin_index': 2},
    ]
    cases: list[dict[str, Any]] = [
        {'fn': 'live2d_model_id', 'args': [model], 'result': _attempt(lambda model=model: live2d_overrides.live2d_model_id(model))}
        for model in models
    ]
    character = {
        'live2d_models': [
            {'stable_id': 'same'},
            {'stable_id': 'same', 'field': 'other'},
            {'stable_id': 'same'},
            {'live2d_key': 'k' * 160, 'row_index': 1},
            {'live2d_key': 'k' * 160, 'row_index': 2},
            'junk',
        ],
        'costumes': [{'live2d_models': [{'stable_id': 'same', 'field': 'other'}, {}]}, 'junk', {'live2d_models': 'x'}],
        'skins': [{'live2d_models': [{'stable_id': 'skin'}]}],
    }
    overrides = [
        {
            'model_id': 'same',
            'profile': 'default',
            'position': {'x': 1, 'y': 2},
            'scale': 1.5,
            'background_position': None,
            'background_scale': None,
            'created_at': 'c',
            'updated_at': 'u',
        },
        {'model_id': 'same', 'profile': 'mobile', 'position': 'x', 'scale': 2, 'background_position': {'x': 0}, 'created_at': 'c2'},
        {'model_id': ' ', 'profile': 'p'},
        {'model_id': 'skin', 'profile': ''},
    ]
    applied = json.loads(json.dumps(character))
    live2d_overrides.apply_live2d_view_overrides(applied, overrides)
    cases.append({'fn': 'apply_live2d_view_overrides', 'args': [character, overrides], 'result': applied})
    cases.extend(
        {
            'fn': 'validate_live2d_source',
            'args': [source],
            'result': _attempt(lambda source=source: live2d_overrides.validate_live2d_source(source)),
        }
        for source in ['bd2', ' BD2 ', 'Nikke', 'azurlane', '']
    )
    cases.extend(
        {
            'fn': 'validate_live2d_profile',
            'args': [profile],
            'result': _attempt(lambda profile=profile: live2d_overrides.validate_live2d_profile(profile)),
        }
        for profile in ['default', ' mobile ', 'a-b_c', 'x' * 64, 'x' * 65, 'bad.profile', '', 'é']
    )
    return cases


GOLDENS = {
    'bd2/parsing.json': _parsing,
    'bd2/crawl.json': _crawl,
    'bd2/api.json': _api,
    'bd2/live2d_overrides.json': _model_ids,
}
