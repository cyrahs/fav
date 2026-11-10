"""`src/tool/azurlane_l2d_sources.py` and `src/tool/azurlane_protected.py` (WP11).

The source pipeline end to end over recorded exchanges (fetch both sources, build the catalog,
validate its resources, report health and drift), resource enumeration over model files chosen
to hit the enumeration branches, the ship detail parsers, the URL helpers, and the AZP2 envelope
with vectors for every error branch. tests/test_azurlane_*.py pin single behaviours; these pin
the full output shape (every field `to_dict` renders) against inputs those tests do not build.

Time is pinned (`_utc_now_iso`), requests go to an `httpx.MockTransport` whose every exchange is
recorded in the golden, and the Rust replay serves the same exchanges in the same order.
"""

from __future__ import annotations

import gzip
import hashlib
import json
import struct
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from dataclasses import asdict
from typing import Any

import httpx
from Crypto.Cipher import AES
from src.tool import azurlane_l2d_sources as sources
from src.tool import azurlane_protected as protected
from src.tool.azurlane_l2d_sources import (
    L2D_SU_ENGLISH_REGION,
    L2D_SU_PRIMARY_REGION,
    L2D_SU_STATIC_BASE_URL,
    NAGAMI_MAPPING_URL,
    ArchivedModel,
    AzurLaneSourceSnapshots,
    L2DSuSourceSnapshot,
    NagamiSourceSnapshot,
    SourceFetchMetadata,
    build_azurlane_l2d_health_report,
    build_azurlane_model_catalog,
    case_variant_urls,
    enumerate_azurlane_model_resources,
    fetch_source_snapshots,
    is_hashed_asset_url,
    l2d_su_asset_url_from_legacy,
    l2d_su_character_fingerprint,
    l2d_su_legacy_asset_urls,
    l2d_su_ship_index_url,
    model_entry_from_dict,
    model_probe_urls,
    painting_index_url,
    parse_l2d_su_ship_class,
    parse_l2d_su_ship_index,
    parse_l2d_su_ship_models,
    parse_l2d_su_ship_voices,
    parse_nagami_mapping,
    parse_painting_index,
    pin_archived_entries,
    spine_parts_manifest_url,
    spine_resource_manifest,
    validate_azurlane_model_catalog_resources,
)

FIXED_NOW_ISO = '2026-11-10T00:00:00+00:00'
PRIMARY_INDEX_URL = l2d_su_ship_index_url(L2D_SU_PRIMARY_REGION)
ENGLISH_INDEX_URL = l2d_su_ship_index_url(L2D_SU_ENGLISH_REGION)
BASE = L2D_SU_STATIC_BASE_URL
FRAGMENT_KEY = '5a' * 32
FRAGMENT_IDENTITY = 'c3' * 32
FRAGMENT = f'#azp2={FRAGMENT_KEY}.{FRAGMENT_IDENTITY}'


def _error(exc: Exception) -> dict[str, str]:
    return {'class': type(exc).__name__, 'message': str(exc)}


def _attempt(producer: Callable[[], Any]) -> dict[str, Any]:
    try:
        return {'ok': producer()}
    except (ValueError, TypeError) as exc:
        return {'error': _error(exc)}


@contextmanager
def _pinned_clock() -> Iterator[None]:
    saved = sources._utc_now_iso  # noqa: SLF001
    sources._utc_now_iso = lambda: FIXED_NOW_ISO  # noqa: SLF001
    try:
        yield
    finally:
        sources._utc_now_iso = saved  # noqa: SLF001


# ---------- AZP2 ----------


def _mask(body: bytes, seed: int) -> bytes:
    state = seed
    masked = bytearray(len(body))
    for index in range(len(body)):
        state ^= (state << 13) & 0xFFFFFFFF
        state ^= state >> 17
        state ^= (state << 5) & 0xFFFFFFFF
        masked[index] = (body[len(body) - 1 - index] + state) & 0xFF
    return bytes(masked)


def _seal(plaintext: bytes, key: str, identity: str, nonce: bytes = bytes(range(12))) -> bytes:
    cipher = AES.new(bytes.fromhex(key), AES.MODE_GCM, nonce=nonce)
    cipher.update(f'azp2:{identity}'.encode())
    ciphertext, tag = cipher.encrypt_and_digest(plaintext)
    return b'AZP2' + nonce + ciphertext + tag


def _envelope(  # noqa: PLR0913
    content: bytes,
    key: str = FRAGMENT_KEY,
    identity: str = FRAGMENT_IDENTITY,
    *,
    codec: int = 0,
    seed: int = 0x1234_5678,
    length: int | None = None,
    body: bytes | None = None,
) -> bytes:
    """An envelope as l2d.su builds it, with each field open to tampering."""
    if body is None:
        body = gzip.compress(content, mtime=0) if codec == 1 else content
    header = struct.pack('<IBI', seed, codec, len(content) if length is None else length)
    return _seal(header + _mask(body, seed), key, identity)


def _gzip_member(content: bytes) -> bytes:
    return gzip.compress(content, mtime=0)


def _data_envelope(path: str, content: bytes, *, codec: int = 0) -> bytes:
    descriptor = protected.data_descriptor(path)
    return _envelope(content, descriptor.key, descriptor.identity, codec=codec)


def _descriptor(descriptor: protected.Azp2Descriptor | None) -> dict[str, str] | None:
    return None if descriptor is None else {'key': descriptor.key, 'identity': descriptor.identity}


DATA_PATHS = [
    'ships-CN.json',
    'ships/CN/10117.json',
    '  ships-EN.json  ',
    '中文/名字.json',
    '',
    '   ',
    '/ships-CN.json',
    'ships//CN.json',
    'ships/./CN.json',
    'ships/../CN.json',
    'ships\\CN.json',
    'ships/CN/',
]

PROTECTED_URLS = [
    f'{BASE}/a1/b2.model3.json{FRAGMENT}',
    f'{BASE}/a1/b2.webp',
    'https://static.l2d.su/al/data/ships-CN.json',
    'https://static.l2d.su/al/data/ships/CN/10117.json?v=2',
    'https://static.l2d.su/al/data/',
    'https://static.l2d.su/al/data/ships/../x.json',
    'https://static.l2d.su/al/data/x.json#azp2=short',
    f'https://static.l2d.su/al/x.json#azp2={"AB" * 32}.{"cd" * 32}',
    f'https://static.l2d.su/al/x.json#azp2={"ab" * 32}.{"cd" * 32}&more',
    'https://static.l2d.su/al/x.json#other',
    'https://static.l2d.su/al/x.json#',
    'https://[::1/data/x.json',
    '',
]


def _protected_urls() -> list[dict[str, Any]]:
    return [
        {
            'url': url,
            'fragment_descriptor': _attempt(lambda url=url: _descriptor(protected.fragment_descriptor(url))),
            'data_url_descriptor': _attempt(lambda url=url: _descriptor(protected.data_url_descriptor(url))),
            'is_protected_url': _attempt(lambda url=url: protected.is_protected_url(url)),
            'strip_fragment': protected.strip_fragment(url),
        }
        for url in PROTECTED_URLS
    ]


def _protected_cases() -> list[dict[str, Any]]:
    content = b'{"Version": 3, "FileReferences": {"Moc": "a.moc3"}, "Text": "\xe4\xb8\xad"}' * 4
    fragment_url = f'{BASE}/a1/b2.model3.json{FRAGMENT}'
    data_url = 'https://static.l2d.su/al/data/ships/CN/10117.json'
    two_members = _gzip_member(b'first ') + b'\0\0\0' + _gzip_member(b'second')
    corrupt_crc = bytearray(_gzip_member(content))
    corrupt_crc[-8] ^= 0xFF
    named = bytearray(_gzip_member(content))
    named[3] |= 0x08 | 0x10 | 0x02
    named[10:10] = b'name.json\0comment\0\xab\xcd'
    vectors: list[tuple[str, str, bytes]] = [
        ('raw codec via the fragment', fragment_url, _envelope(content)),
        ('gzip codec via the fragment', fragment_url, _envelope(content, codec=1)),
        ('gzip with name, comment and header crc', fragment_url, _envelope(content, codec=1, body=bytes(named))),
        ('derived data descriptor', data_url, _data_envelope('ships/CN/10117.json', content)),
        ('derived data descriptor, gzip', data_url, _data_envelope('ships/CN/10117.json', content, codec=1)),
        ('empty body', fragment_url, _envelope(b'')),
        ('seed zero', fragment_url, _envelope(content, seed=0)),
        ('seed all ones', fragment_url, _envelope(content, seed=0xFFFF_FFFF)),
        ('clear text where a fragment promises an envelope', fragment_url, content),
        ('clear text from data passes through', data_url, content),
        ('an unprotected URL passes anything through', f'{BASE}/a1/b2.webp', _envelope(content)),
        ('wrong identity', fragment_url, _envelope(content, identity='d4' * 32)),
        ('wrong data descriptor', data_url, _data_envelope('ships/CN/10118.json', content)),
        ('too short', fragment_url, b'AZP2' + bytes(36)),
        ('magic only', fragment_url, b'AZP2'),
        ('tampered tag', fragment_url, _envelope(content)[:-1] + b'\0'),
        ('header cut short', fragment_url, _seal(b'\1\2\3\4\0\1', FRAGMENT_KEY, FRAGMENT_IDENTITY)),
        ('length over the limit', fragment_url, _envelope(content, length=protected.AZP2_MAX_PLAINTEXT_SIZE + 1)),
        ('raw length mismatch', fragment_url, _envelope(content, length=len(content) + 1)),
        ('unknown codec', fragment_url, _envelope(content, codec=7)),
        ('gzip size trailer mismatch', fragment_url, _envelope(content, codec=1, length=len(content) - 1)),
        ('gzip body too short', fragment_url, _envelope(b'', codec=1, body=b'\x1f\x8b\x08\0\0\0\0\0\0\0\0\0\0\0\0\0', length=0)),
        (
            'not a gzip member',
            fragment_url,
            _envelope(content, codec=1, body=b'PK' + bytes(16) + struct.pack('<I', len(content))),
        ),
        (
            'unknown gzip method',
            fragment_url,
            _envelope(content, codec=1, body=b'\x1f\x8b\x07' + bytes(15) + struct.pack('<I', len(content))),
        ),
        ('gzip crc mismatch', fragment_url, _envelope(content, codec=1, body=bytes(corrupt_crc))),
        (
            'two gzip members with padding',
            fragment_url,
            _envelope(b'first second', codec=1, body=two_members, length=len(b'second')),
        ),
        (
            'gzip member cut before its trailer',
            fragment_url,
            _envelope(content, codec=1, body=_gzip_member(content)[:-12] + struct.pack('<I', len(content))),
        ),
    ]
    return [
        {
            'name': name,
            'url': url,
            'content_hex': envelope.hex(),
            'result': _attempt(lambda envelope=envelope, url=url: protected.decrypt_for_url(envelope, url).hex()),
        }
        for name, url, envelope in vectors
    ]


def _protected() -> list[dict[str, Any]]:
    return [
        {'data_descriptor': [{'path': path, 'result': _attempt(lambda path=path: _descriptor(protected.data_descriptor(path)))} for path in DATA_PATHS]},
        {'urls': _protected_urls()},
        {'decrypt_for_url': _protected_cases()},
    ]


# ---------- index fixtures ----------


def _skin(  # noqa: PLR0913
    skin_id: object,
    name: str,
    key: str,
    *,
    kind: str = 'live2d',
    live2d_plus: bool = False,
    shop: str | None = None,
    extra: dict[str, object] | None = None,
) -> dict[str, object]:
    feature_tag = 'spine' if kind == 'spine' else ('Live2D+' if live2d_plus else 'Live2D')
    skin: dict[str, object] = {
        'id': skin_id,
        'name': name,
        'groupIndex': 1,
        'skinType': 4,
        'skinTypeName': 'Skin',
        'featureTags': [feature_tag] if kind != 'painting' else [],
        'painting': key,
        'prefab': key,
        'dynamicType': kind,
        'isLive2d': kind == 'live2d',
        'isLive2dPlus': live2d_plus,
        'isDynamic': kind != 'painting',
        'isSpine': kind == 'spine',
        'paintingFaceIds': ['0', '1', 1, ''],
        'assetPaths': {
            'squareIcon': f'squareicon/{key}',
            'shipyardIcon': f'shipyardicon/{key}',
            'qIcon': f'qicon/{key}',
            'painting': f'painting/{key}',
        },
    }
    if shop is not None:
        skin['shopTypeName'] = shop
    skin.update(extra or {})
    return skin


def _hashed_skin(skin_id: int, kind: str, model_path: str) -> dict[str, object]:
    key = hashlib.md5(str(skin_id).encode(), usedforsecurity=False).hexdigest()
    skin: dict[str, object] = {
        'id': skin_id,
        'name': f'Hashed {skin_id}',
        'skinTypeName': 'Skin',
        'featureTags': ['Live2D'],
        'painting': key,
        'prefab': key,
        'paintingKeys': [key],
        'dynamicType': kind,
        'isLive2d': kind == 'live2d',
        'isSpine': kind == 'spine',
        'paintingFaceIds': ['1', '2'],
        'directResources': True,
        'assetPaths': {
            'squareIcon': f'aa11/{key}.webp',
            'shipyardIcon': f'bb22/{key}.webp',
            'qIcon': '',
            'painting': f'cc33/{key}.webp',
            'paintingPaths': [f'cc33/{"0" * 32}.json{FRAGMENT}', f'cc33/{key}.json{FRAGMENT}'],
            'paintingFaces': [{'id': '1', 'url': f'dd44/{key}-1.webp'}, {'id': '2', 'url': f'{BASE}/dd44/{key}-2.webp'}, {'id': 3}],
        },
    }
    if model_path:
        skin['model'] = {'type': kind, 'path': model_path}
    return skin


def _ship(  # noqa: PLR0913
    ship_group_id: object,
    resource_key: str,
    name: str,
    skins: list[object],
    *,
    english_name: str = '',
    extra: dict[str, object] | None = None,
) -> dict[str, object]:
    ship: dict[str, object] = {
        'shipGroupId': ship_group_id,
        'name': name,
        'englishName': english_name,
        'nationName': 'Royal Navy',
        'typeName': 'Destroyer',
        'rarityName': 'SR',
        'resourceKey': resource_key,
        'skins': skins,
    }
    ship.update(extra or {})
    return ship


def _index(ships: list[object], *, region: str = 'CN', **extra: object) -> str:
    payload: dict[str, object] = {
        'generatedAt': '2026-11-01T04:33:18.041Z',
        'locale': region,
        'version': '9.7.416',
        'ships': ships,
    }
    payload.update(extra)
    return json.dumps(payload, ensure_ascii=False)


def _primary_ships() -> list[object]:
    return [
        _ship(
            20101,
            'biaoqiang',
            '标枪',
            [
                _skin(201010, '标枪', 'biaoqiang', kind='painting'),
                _skin(201011, '闪耀的白刃', 'biaoqiang_2', shop='Swimsuits'),
                _skin(201012, '  礼服  ', 'biaoqiang_3', live2d_plus=True, shop='Formal'),
                _skin(201013, '动态', 'biaoqiang_4', kind='spine'),
                _skin(201015, '无键', '', kind='live2d'),
            ],
            english_name='HMS Javelin',
            extra={'defaultSkinId': 201010, 'className': 'J Class'},
        ),
        _ship(
            10117,
            'lafei',
            '拉菲',
            [
                _skin(101170, '拉菲', 'lafei', kind='painting'),
                _skin(101174, '白兔迎春', 'lafei_4', extra={'prefab': 'lafei_4_prefab'}),
                _skin(101172, '慵懒时光', 'lafei_12', kind='spine'),
            ],
            extra={'defaultSkinId': True},
        ),
        _ship(
            90001,
            ' Ｚ２３ ',
            'Ｚ２３',
            [_skin(900010, 'Ｚ２３', 'Ｚ２３_2'), _skin(900011, 'Z23 Ｂ', 'z23_3', kind='spine')],
            extra={'nationName': 'Iron Blood', 'typeName': ' Destroyer '},
        ),
        _ship(90002, 'twin', '双子A', [_skin(900020, '一', 'twin_2')]),
        _ship(90003, 'twin', '双子B', [_skin(900030, '二', 'twin_2')]),
        _ship(
            10117 + 900000,
            'bd433116e13fbc136f40679616583bfc',
            '新拉菲',
            [
                _hashed_skin(910170, 'live2d', f'ab12/{"1" * 32}.model3.json{FRAGMENT}'),
                _hashed_skin(910171, 'spine', f'cd34/{"2" * 32}.json{FRAGMENT}'),
                _hashed_skin(910172, 'painting', ''),
            ],
        ),
    ]


def _english_ships() -> list[object]:
    return [
        _ship(
            20101,
            'biaoqiang',
            'Javelin',
            [_skin(201011, 'Gleaming White Blade', 'biaoqiang_2'), _skin(201013, 'Dynamic', 'biaoqiang_4', kind='spine')],
        ),
        _ship(10117, 'lafei', 'Laffey', [_skin(101174, 'White Rabbit Welcomes the Spring', 'lafei_4')]),
        _ship(55555, 'englishonly', 'Only English', [_skin(555550, 'Only', 'englishonly_2')]),
    ]


def _nagami_mapping() -> str:
    return json.dumps(
        {
            '20101': {
                'names': {'EN': 'Javelin', 'CN': '标枪', 'JP': 'ジャベリン'},
                'skins': [
                    {'prefab': 'biaoqiang_2', 'names': {'EN': 'Gleaming White Blade'}, 'bg': '12'},
                    {'prefab': 'biaoqiang_9', 'names': {'CN': '只在镜像'}, 'bg': ''},
                ],
            },
            '10117': {
                'names': {'EN': '', 'CN': '拉菲'},
                'skins': [{'prefab': 'lafei_4', 'names': {'JP': '白ウサギ'}, 'bg': 7}],
            },
            'shengluyisi_2': {'name': 'St. Louis - Blue and White Pottery', 'bg': '145'},
            'z23_3': 'Z23 - B',
            'twin_2': 'Twin - One',
            'plain': 'NoDash',
        },
        ensure_ascii=False,
    )


def _model3(**overrides: object) -> str:
    references: dict[str, object] = {
        'Moc': 'model.moc3',
        'Textures': ['textures/texture_00.webp', 'textures/texture_01.webp'],
        'Physics': 'model.physics3.json',
        'Expressions': [{'Name': 'smile', 'File': 'expressions/smile.exp3.json'}],
        'Motions': {
            'Idle': [{'File': 'motions/idle.motion3.json', 'Sound': 'voice/idle.wav'}],
            'TapBody': [{'File': 'motions/tap.motion3.json', 'Text': 'line'}],
        },
    }
    references.update(overrides)
    return json.dumps({'Version': 3, 'FileReferences': references}, ensure_ascii=False)


def _parts(key: str, suffixes: tuple[str, ...]) -> str:
    return json.dumps(
        {'version': 1, 'models': [{'name': f'{key}{s}', 'skeleton': f'{key}{s}.skel', 'atlases': [f'{key}{s}.atlas']} for s in suffixes]},
    )


def _atlas(*pages: str) -> str:
    return ''.join(f'\n{page}\nsize: 1024,1024\nformat: RGBA8888\nfilter: Linear,Linear\nrepeat: none\nregion\n  rotate: false\n' for page in pages)


# ---------- the recorded pipeline ----------


def _response(spec: dict[str, Any], request: httpx.Request) -> httpx.Response:
    if 'error' in spec:
        raise httpx.ConnectError(spec['error'], request=request)
    content = bytes.fromhex(spec['hex']) if 'hex' in spec else spec.get('text', '').encode()
    return httpx.Response(spec.get('status', 200), content=content, headers=spec.get('headers', {}))


class _Recorder:
    """A MockTransport handler over `rules` (first match wins), recording each exchange."""

    def __init__(self, rules: list[tuple[Callable[[str, str], bool], dict[str, Any]]]) -> None:
        self.rules = rules
        self.exchanges: list[dict[str, Any]] = []

    def __call__(self, request: httpx.Request) -> httpx.Response:
        method, url = request.method, str(request.url)
        spec: dict[str, Any] = {'status': 404}
        for matches, candidate in self.rules:
            if matches(method, url):
                spec = candidate
                break
        self.exchanges.append({'method': method, 'url': url, 'response': spec})
        return _response(spec, request)


def _exact(method: str, url: str) -> Callable[[str, str], bool]:
    return lambda m, u: m == method and u == url


def _suffix(method: str, suffix: str) -> Callable[[str, str], bool]:
    return lambda m, u: m == method and u.split('#', 1)[0].endswith(suffix)


def _contains(method: str, part: str) -> Callable[[str, str], bool]:
    return lambda m, u: m == method and part in u


def _source_rules(primary: dict[str, Any], english: dict[str, Any], nagami: dict[str, Any]) -> list[Any]:
    return [
        (_exact('GET', PRIMARY_INDEX_URL), primary),
        (_exact('GET', ENGLISH_INDEX_URL), english),
        (_exact('GET', NAGAMI_MAPPING_URL), nagami),
    ]


def _resource_rules() -> list[Any]:
    """The CDN as validation sees it: most files there, a few gone, some HEADs refused."""
    return [
        (_contains('HEAD', 'texture_01'), {'status': 404}),
        (_contains('HEAD', 'nagami.moe'), {'status': 405}),
        (_contains('GET', 'shengluyisi_2.model3.json'), {'text': _model3(DisplayInfo='display.cdi3.json')}),
        (_contains('GET', 'nagami.moe/live2d/biaoqiang_2'), {'text': _model3()}),
        (_contains('GET', 'nagami.moe'), {'status': 200}),
        (_suffix('GET', 'biaoqiang_3.model3.json'), {'text': '<!doctype html>'}),
        (_suffix('GET', 'Ｚ２３_2.model3.json'), {'error': 'connection reset'}),
        (_suffix('GET', '.model3.json'), {'text': _model3(DisplayInfo='display.cdi3.json')}),
        (_suffix('HEAD', 'lafei_12.skel'), {'status': 404}),
        (_suffix('GET', 'lafei_12.json'), {'text': _parts('lafei_12', ('B', 'T'))}),
        (_suffix('GET', 'z23_3.json'), {'text': '{"models": {}}'}),
        (_suffix('HEAD', 'z23_3.skel'), {'status': 404}),
        (_suffix('GET', 'lafei_12B.atlas'), {'text': _atlas('lafei_12B.webp', 'lafei_12B_2.webp')}),
        (_suffix('GET', 'lafei_12T.atlas'), {'text': _atlas('lafei_12T.webp', 'lafei_12B_2.webp')}),
        (_suffix('GET', '.atlas'), {'text': _atlas('page.webp')}),
        (_suffix('HEAD', 'lafei.webp'), {'status': 501}),
        (_suffix('GET', 'lafei.webp'), {'status': 200}),
        (_contains('HEAD', 'twin_2'), {'status': 500}),
        (lambda m, _u: m == 'HEAD', {'status': 200}),
    ]


def _run_pipeline(rules: list[Any], previous: Any = None) -> tuple[dict[str, Any], Any]:
    recorder = _Recorder(rules)
    with httpx.Client(transport=httpx.MockTransport(recorder)) as client:
        snapshots = fetch_source_snapshots(client=client)
        catalog = build_azurlane_model_catalog(snapshots)
        validation = validate_azurlane_model_catalog_resources(catalog, client=client)
    health = build_azurlane_l2d_health_report(
        snapshots=snapshots,
        catalog=validation.catalog,
        resource_validation=validation,
        previous_report=previous,
    )
    output = {
        'exchanges': recorder.exchanges,
        'snapshots': snapshots.to_dict(),
        'catalog': catalog.to_dict(),
        'catalog_summary': catalog.summary(),
        'fingerprints': [[c.char_id, l2d_su_character_fingerprint(c)] for c in snapshots.l2d_su.characters],
        'validation': validation.to_dict(),
        'validation_summary': validation.summary(),
        'health': health.to_dict(),
        'searches': {
            query: [entry.id for entry in catalog.search(query, model_type=model_type)]
            for query, model_type in (
                ('javelin', None),
                ('标枪', None),
                ('Ｚ２３', None),
                ('z23', 'spine'),
                ('blue  pottery', None),
                ('', None),
                ('nothing at all', None),
                ('twin', 'live2d'),
            )
        },
    }
    return output, health


def _scenario(name: str, rules: list[Any], previous_rules: list[Any] | None = None) -> dict[str, Any]:
    with _pinned_clock():
        previous_output = None
        previous_health = None
        if previous_rules is not None:
            previous_output, previous_health = _run_pipeline(previous_rules)
        output, _health = _run_pipeline(rules, previous_health)
    return {
        'name': name,
        'previous_exchanges': None if previous_output is None else previous_output['exchanges'],
        **output,
    }


def _sources() -> list[dict[str, Any]]:
    primary = {
        'text': _index(
            _primary_ships(),
            newSkins=[
                {'shipGroupId': True, 'shipName': '标枪', 'skinName': '礼服', 'skinType': 'Live2D', 'skinIds': [201012, 'x', 201011]},
                {'shipGroupId': 'x', 'skinIds': []},
                'junk',
            ],
            filters={'nation': ['Royal Navy', 'Iron Blood']},
            skinUpdateHistory=[{'version': '9.7.416', 'skinIds': [201012]}, 'junk'],
        ),
        'headers': {'ETag': '"v2"', 'Last-Modified': 'Sun, 01 Nov 2026 04:33:18 GMT', 'content-type': 'application/json'},
    }
    previous_primary = {
        'text': _index(_primary_ships()[:2]),
        'headers': {'ETag': '"v1"', 'Last-Modified': 'Sat, 31 Oct 2026 04:33:18 GMT'},
    }
    english = {'text': _index(_english_ships(), region='EN')}
    nagami = {'text': _nagami_mapping(), 'headers': {'etag': 'W/"n1"'}}
    protected_index = _data_envelope('ships-CN.json', _index(_primary_ships()[:2]).encode(), codec=1)
    return [
        _scenario(
            'full pipeline with drift',
            _source_rules(primary, english, nagami) + _resource_rules(),
            _source_rules(previous_primary, {'status': 503}, {'text': json.dumps({'plain': 'NoDash', 'gone_2': 'Gone - Away'})})
            + _resource_rules(),
        ),
        _scenario(
            'protected index, english missing',
            _source_rules({'hex': protected_index.hex()}, {'error': 'english down'}, nagami) + _resource_rules(),
        ),
        _scenario(
            'spa shell and broken mapping',
            _source_rules(
                {'text': '<!doctype html><html></html>', 'headers': {'content-type': 'text/html; charset=utf-8'}},
                english,
                {'text': json.dumps({'bad': {'name': ''}})},
            ),
        ),
        _scenario(
            'schema failure and nagami down',
            _source_rules({'text': json.dumps({'ships': {}})}, english, {'error': 'nagami unavailable'}),
        ),
        _scenario(
            'clear text where an envelope is expected',
            _source_rules({'hex': (b'AZP2' + bytes(40)).hex()}, english, {'text': '[]'}),
        ),
        _scenario(
            'a skin id that is not an integer',
            _source_rules({'text': _index([_ship(1, 'a', 'A', [_skin('1', 'x', 'a_2')])])}, english, {'text': '{}'}),
        ),
        _scenario(
            'a ship that is not an object',
            _source_rules({'text': _index(['not a ship'])}, english, {'text': '{}'}),
        ),
        _scenario(
            'invalid json',
            _source_rules({'text': '{"ships": [1, 2,'}, {'status': 200, 'text': ''}, {'text': 'not json'}),
        ),
    ]


# ---------- enumeration ----------


def _enumeration_catalog(index: str, nagami: str) -> Any:
    parsed = parse_l2d_su_ship_index(index)
    metadata = SourceFetchMetadata.for_url(PRIMARY_INDEX_URL, fetched_at=FIXED_NOW_ISO)
    return build_azurlane_model_catalog(
        AzurLaneSourceSnapshots(
            l2d_su=L2DSuSourceSnapshot(metadata=metadata, characters=parsed.characters),
            nagami=NagamiSourceSnapshot(metadata=metadata, entries=parse_nagami_mapping(nagami).entries),
        ),
    )


def _enumerate(case: dict[str, Any]) -> dict[str, Any]:
    catalog = _enumeration_catalog(case['index'], case['nagami'])
    entry = next(entry for entry in catalog.entries if entry.id == case['entry_id'])
    voices = ()
    if case.get('detail') is not None:
        voices = parse_l2d_su_ship_voices(case['detail']).get(entry.costume.id or -1, ())
    painting_index = None
    if case.get('painting_index') is not None:
        painting_index = parse_painting_index(
            case['painting_index'],
            painting_url=entry.resources.primary_url,
            index_url=entry.resources.painting_index_url,
        )

    def run() -> dict[str, Any]:
        return enumerate_azurlane_model_resources(
            entry,
            model3_source=case.get('model3'),
            spine_parts_source=case.get('spine_parts'),
            atlas_sources=case.get('atlases'),
            voice_lines=voices,
            painting_index=painting_index,
        ).to_dict()

    return {**case, 'result': _attempt(run)}


def _enumerations() -> list[dict[str, Any]]:
    index = _index(_primary_ships())
    nagami = _nagami_mapping()
    lafei_base = f'{BASE}/spinepainting/lafei_12'
    biaoqiang_4 = f'{BASE}/spinepainting/biaoqiang_4'
    hashed_key = hashlib.md5(b'910172', usedforsecurity=False).hexdigest()
    detail = json.dumps(
        {
            'ship': {
                'shipGroupId': 20101,
                'skins': [
                    {
                        'id': 201010,
                        'words': [
                            {'key': 'detail', 'voicePath': 'cue/cv-20101/detail', 'text': '你好', 'faceId': '1'},
                            {'key': 'touch', 'voicePath': 'cue/cv-20101/touch', 'l2dAction': 'tap'},
                            {'key': 'dup', 'voicePath': 'cue/cv-20101/detail'},
                            {'key': 'empty', 'voicePath': ''},
                        ],
                        'extraWords': [{'key': 'extra', 'voicePath': 'cue/cv-20101/extra_1'}],
                    },
                    {
                        'id': 910172,
                        'words': [{'key': 'detail', 'voicePath': 'cue/cv-91/detail'}],
                    },
                ],
            },
        },
        ensure_ascii=False,
    )
    rich_model3 = _model3(
        Moc='../shared/model.moc3',
        Textures=['textures/texture_00.webp', 'textures/texture_00.webp', 'https://mirror.example/t.webp', 7, ''],
        Physics=f'physics.physics3.json{FRAGMENT}',
        Pose='pose.pose3.json',
        DisplayInfo='display.cdi3.json',
        UserData='user.userdata3.json',
        Expressions=[{'Name': 'smile', 'File': 'expressions/smile.exp3.json'}, {'File': 'expressions/plain.exp3.json'}, 'junk', {'Name': 'x'}],
        Motions={
            'Idle': [
                {'File': 'motions/idle.motion3.json', 'Sound': 'voice/idle.wav'},
                {'File': 'motions/idle.motion3.json', 'Sound': 'voice/idle.wav'},
            ],
            '': [{'File': 'motions/blank.motion3.json'}],
            'Tap': [{'File': 'motions/tap.motion3.json', 'Audio': 'https://patch.example/a b.mp3', 'Text': '台词'}, 'junk', {}],
            'Broken': 'not a list',
        },
    )
    cases: list[dict[str, Any]] = [
        {'name': 'live2d rich model3', 'entry_id': 'azurlane:live2d:biaoqiang:biaoqiang_2', 'model3': rich_model3},
        {'name': 'live2d minimal model3', 'entry_id': 'azurlane:live2d:biaoqiang:biaoqiang_2', 'model3': json.dumps({'FileReferences': {'Moc': 'm.moc3'}})},
        {'name': 'live2d without model3', 'entry_id': 'azurlane:live2d:biaoqiang:biaoqiang_2'},
        {'name': 'live2d model3 not json', 'entry_id': 'azurlane:live2d:biaoqiang:biaoqiang_2', 'model3': '<html>'},
        {'name': 'live2d model3 without references', 'entry_id': 'azurlane:live2d:biaoqiang:biaoqiang_2', 'model3': json.dumps({'Version': 3})},
        {'name': 'live2d model3 without moc', 'entry_id': 'azurlane:live2d:biaoqiang:biaoqiang_2', 'model3': json.dumps({'FileReferences': {'Textures': []}})},
        {'name': 'live2d nagami only', 'entry_id': 'azurlane:live2d:shengluyisi:shengluyisi_2', 'model3': _model3()},
        {'name': 'live2d merged with fallback', 'entry_id': 'azurlane:live2d:biaoqiang:biaoqiang_2', 'model3': _model3(DisplayInfo='d.cdi3.json')},
        {'name': 'live2d shared key variant', 'entry_id': 'azurlane:live2d:twin:twin_2', 'model3': _model3()},
        {'name': 'live2d normalized key', 'entry_id': 'azurlane:live2d:Z23:Z23_2', 'model3': _model3()},
        {
            'name': 'live2d hashed',
            'entry_id': 'azurlane:live2d:ship-910117:skin-910170',
            'model3': _model3(Moc=f'm.moc3{FRAGMENT}', Textures=[f'../ee55/{"3" * 32}.webp']),
        },
        {'name': 'spine single part', 'entry_id': 'azurlane:spine:biaoqiang:biaoqiang_4', 'atlases': {f'{biaoqiang_4}/biaoqiang_4.atlas': _atlas('biaoqiang_4.webp', 'sub/biaoqiang_4_2.png', 'biaoqiang_4.webp')}},
        {'name': 'spine single part without atlas source', 'entry_id': 'azurlane:spine:biaoqiang:biaoqiang_4'},
        {
            'name': 'spine multi part',
            'entry_id': 'azurlane:spine:lafei:lafei_12',
            'spine_parts': _parts('lafei_12', ('B', 'T')),
            'atlases': {f'{lafei_base}/lafei_12B.atlas': _atlas('lafei_12B.webp'), f'{lafei_base}/lafei_12T.atlas': _atlas('shared.webp', 'lafei_12T.webp')},
        },
        {'name': 'spine parts not json', 'entry_id': 'azurlane:spine:lafei:lafei_12', 'spine_parts': 'nope'},
        {'name': 'spine parts without models', 'entry_id': 'azurlane:spine:lafei:lafei_12', 'spine_parts': json.dumps({'models': 'x'})},
        {
            'name': 'spine parts with odd models',
            'entry_id': 'azurlane:spine:lafei:lafei_12',
            'spine_parts': json.dumps({'models': [{'name': 'a', 'skeleton': 'a.skel', 'atlases': ['a.atlas', 3]}, 'junk', {'skeleton': ''}]}),
        },
        {
            'name': 'spine hashed',
            'entry_id': 'azurlane:spine:ship-910117:skin-910171',
            'spine_parts': json.dumps({'models': [{'name': 'h', 'skeleton': f'{"4" * 32}.skel', 'atlases': [f'{"5" * 32}.atlas{FRAGMENT}']}]}),
            'atlases': {f'{BASE}/cd34/{"5" * 32}.atlas{FRAGMENT}': _atlas(f'{"6" * 32}.webp')},
        },
        {'name': 'spine hashed without parts', 'entry_id': 'azurlane:spine:ship-910117:skin-910171'},
        {'name': 'painting with voices', 'entry_id': 'azurlane:painting:biaoqiang:biaoqiang', 'detail': detail},
        {
            'name': 'painting with index',
            'entry_id': 'azurlane:painting:biaoqiang:biaoqiang',
            'painting_index': json.dumps(
                {
                    'biaoqiang': {'size': [10, 20], 'pivot': [0.5, 0.5], 'position': [1, 2]},
                    'biaoqiang_rw': {'size': [5, 6], 'rawSize': [3, 4], 'pivot': [0, 1], 'position': [-1.5, 2.25]},
                    'biaoqiang_bg': {'size': [5, 6], 'raw': True},
                    'face': {'size': [1, 1], 'pivot': [0.5, 0.5], 'position': [0, 0]},
                },
            ),
        },
        {
            'name': 'painting hashed with index and voices',
            'entry_id': 'azurlane:painting:ship-910117:skin-910172',
            'detail': detail,
            'painting_index': json.dumps(
                {
                    hashed_key: {'size': [762, 1024], 'image': f'{hashed_key}.webp', 'mesh': f'{"7" * 32}.obj{FRAGMENT}'},
                    'ab' * 16: {'size': [100, 100], 'image': f'{"8" * 32}.webp', 'raw': True},
                    'cd' * 16: {'size': [100, 100], 'image': f'{"9" * 32}.webp', 'mesh': f'{"a" * 32}.obj'},
                    'face': {'size': [100, 100]},
                },
            ),
        },
        {'name': 'painting of a ship with odd faces', 'entry_id': 'azurlane:painting:Z23:Z23_2'},
    ]
    for case in cases:
        case.setdefault('index', index)
        case.setdefault('nagami', nagami)
    return [_enumerate(case) for case in cases]


# ---------- ship details, URLs, catalog round trips ----------


def _details() -> list[dict[str, Any]]:
    payloads = [
        json.dumps(
            {
                'ship': {
                    'shipGroupId': 1,
                    'className': '  J Class ',
                    'skins': [
                        {'id': 10, 'model': {'type': 'live2d', 'path': 'live2d/a/a.model3.json'}, 'words': [{'key': 'k', 'voicePath': 'cue/a'}]},
                        {'id': 11, 'model': {'type': 'spine', 'path': ' https://mirror.example/x '}},
                        {'id': 12, 'model': {'path': ''}},
                        {'id': True, 'model': {'path': 'live2d/b/b.model3.json'}},
                        {'id': 13, 'model': 'junk', 'words': 'junk', 'extraWords': [{'voicePath': 'cue/e'}, 'junk', {'voicePath': 3}]},
                        {'model': {'path': 'live2d/c/c.model3.json'}},
                        'junk',
                    ],
                },
            },
        ),
        json.dumps({'ship': {'className': 7}}),
        json.dumps({'ship': {'skins': 'x'}}),
        json.dumps({'ship': []}),
        json.dumps([]),
        '<!doctype html>',
        '',
    ]

    def voices(payload: str) -> list[Any]:
        return [[skin_id, [asdict(line) for line in lines]] for skin_id, lines in parse_l2d_su_ship_voices(payload).items()]

    return [
        {
            'payload': payload,
            'models': _attempt(lambda payload=payload: [[k, v] for k, v in parse_l2d_su_ship_models(payload).items()]),
            'voices': _attempt(lambda payload=payload: voices(payload)),
            'class': _attempt(lambda payload=payload: parse_l2d_su_ship_class(payload)),
        }
        for payload in payloads
    ]


URLS = [
    f'{BASE}/painting/Z28_3.webp',
    f'{BASE}/painting/u47_5.webp',
    f'{BASE}/paintingface/Z28_3/1.webp',
    f'{BASE}/squareicon/U96_2.webp',
    f'{BASE}/painting/biaoqiang.webp',
    f'{BASE}/painting/ABC.webp',
    f'{BASE}/live2d/z23/z23.model3.json',
    f'{BASE}/live2d/Z23_g/z23_g.model3.json',
    f'{BASE}/spinepainting/lafei_12',
    f'{BASE}/spinepainting/lafei_12/',
    f'{BASE}/spinepainting/lafei_12/lafei_12.atlas',
    f'{BASE}/spinepainting/aerbien_4-spine',
    f'{BASE}/cd34/{"2" * 32}.json{FRAGMENT}',
    f'{BASE}/cc33/{"0" * 32}.webp',
    f'{BASE}/cue/cv-1/detail.ogg',
    'https://static.l2d.su/al-b/assets/live2d/z23/z23.model3.json',
    'https://static.l2d.su/al/assets/webp/painting/x.webp',
    'https://static.l2d.su/al/assets/original/painting/x.webp',
    'https://static.l2d.su/azurlane/painting/x.webp',
    'https://cdn.nagami.moe/live2d/z23/z23.model3.json',
    'https://cdn.nagami.moe/live2d/z23',
    'https://example.test/no-path',
    'https://example.test',
    'https://[::1/painting/x.webp',
    'painting/relative.webp',
    '',
]


def _urls() -> list[dict[str, Any]]:
    return [
        {
            'url': url,
            'case_variant_urls': _attempt(lambda url=url: list(case_variant_urls(url))),
            'legacy_asset_urls': list(l2d_su_legacy_asset_urls(url)),
            'asset_url_from_legacy': l2d_su_asset_url_from_legacy(url),
            'is_hashed_asset_url': is_hashed_asset_url(url),
            'spine_parts_manifest_url': _attempt(lambda url=url: spine_parts_manifest_url(url)),
            'spine_resource_manifest': _attempt(lambda url=url: asdict(spine_resource_manifest(url))),
            'painting_index_url': _attempt(lambda url=url: painting_index_url(url)),
        }
        for url in URLS
    ]


def _round_trips() -> list[dict[str, Any]]:
    """`model_entry_from_dict` over every entry, `model_probe_urls`, and `pin_archived_entries`."""
    index, legacy_index, nagami = _index(_primary_ships()), _index(_primary_ships()[:2]), _nagami_mapping()
    hashed_index = _index(
        [
            _ship(
                20101,
                'biaoqiang_hashed',
                '标枪',
                [_hashed_skin(201011, 'live2d', ''), _hashed_skin(201010, 'painting', ''), _hashed_skin(201099, 'live2d', '')],
            ),
        ],
    )
    catalog = _enumeration_catalog(index, nagami)
    entries = []
    for entry in catalog.entries:
        restored = model_entry_from_dict(json.loads(json.dumps(entry.to_dict())))
        entries.append(
            {
                'id': entry.id,
                'round_trips': restored == entry,
                'probe_urls': _attempt(lambda entry=entry: list(model_probe_urls(entry))),
            },
        )
    legacy = _enumeration_catalog(legacy_index, nagami)
    archived = [
        ArchivedModel(entry=entry, completed=index % 2 == 0)
        for index, entry in enumerate(legacy.entries)
        if entry.source != 'nagami' or entry.costume.key == 'shengluyisi_2'
    ]
    hashed_catalog = _enumeration_catalog(hashed_index, nagami)
    pinned = pin_archived_entries(hashed_catalog, archived)
    return [
        {'index': index, 'nagami': nagami, 'entries': entries},
        {
            'legacy_index': legacy_index,
            'hashed_index': hashed_index,
            'nagami': nagami,
            'pinned': {'archived': [{'entry_id': a.entry.id, 'completed': a.completed} for a in archived], 'catalog': pinned.to_dict()},
        },
    ]


GOLDENS = {
    'azurlane/protected.json': _protected,
    'azurlane/sources.json': _sources,
    'azurlane/enumeration.json': _enumerations,
    'azurlane/details.json': _details,
    'azurlane/urls.json': _urls,
    'azurlane/catalog_round_trips.json': _round_trips,
}
