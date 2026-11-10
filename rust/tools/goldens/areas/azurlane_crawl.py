"""`src/web/azurlane.py`: two `AzurLane.update()` runs over a scripted CDN (WP12).

The crawler runs over an `httpx.MockTransport` and the fake `azurlane_*` tables of
tests/test_azurlane.py, made to answer the retry cooldowns, the upserts' COALESCE and
`completed_at` as PostgreSQL does. The clock is frozen, the retry jitter is zero and every
pacing interval is off. The first run sees a texture that answers 503 and then 404, so a model
fails and the run raises; between the runs every retry wait lapses; the second run is served
the texture and completes, reusing everything it already has.

Each run records the requests (method, URL and the headers the clients send), the retry
sleeps, the error, every file (JSON outside the assets as data, the rest by size and sha256)
and the rows. Timestamps differ by nature between the fake and PostgreSQL, so `completed_at`
is recorded as set or not, `next_retry_at` as a running cooldown or not, and the other
timestamps not at all; the temporary root is written `<root>`. The Rust test answers the
same script from its fake web and checks the same things against a real database.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

import httpx
from src.tool import azurlane_l2d_sources as sources
from src.web import azurlane
from tests.test_azurlane import FakeAzurLaneDatabase

FIXED_NOW_ISO = '2026-05-01T00:00:00+00:00'
STATIC = sources.L2D_SU_STATIC_BASE_URL
_FUTURE = 'future'
_PAST = 'past'
_DROPPED_TIMESTAMPS = frozenset({'fetched_at', 'last_attempt_at', 'last_seen_at', 'first_seen_at', 'created_at', 'updated_at'})


class _Database(FakeAzurLaneDatabase):
    """The test fake, with PostgreSQL's cooldowns, COALESCEs and kept `completed_at`."""

    async def query_db(self, query: str, params: tuple[Any, ...] = ()) -> list[dict[str, Any]]:  # noqa: C901, PLR0911, PLR0912
        sql = ' '.join(query.split())
        if 'SELECT failed_count, last_error, next_retry_at FROM azurlane_assets' in sql:
            row = self.assets.get(str(params[0]))
            if row and row.get('status') == 'failed' and row.get('next_retry_at') == _FUTURE:
                return [{'failed_count': row['failed_count'], 'last_error': row['last_error'], 'next_retry_at': _FUTURE}]
            return []
        if sql.startswith('SELECT model_id, next_retry_at, (next_retry_at > NOW()) AS in_cooldown'):
            return [
                {
                    'model_id': model_id,
                    'next_retry_at': row.get('next_retry_at'),
                    'in_cooldown': row.get('next_retry_at') == _FUTURE if row.get('next_retry_at') else None,
                }
                for model_id, row in sorted(self.models.items())
                if int(row.get('failed_count') or 0) > 0
            ]
        if 'INSERT INTO azurlane_assets' in sql and "'pending'" in sql:
            url, normalized_url, kind, fallback_url = params
            row = self.assets.get(str(url))
            if row is None:
                self.assets[str(url)] = {
                    'url': url,
                    'normalized_url': normalized_url,
                    'kind': kind,
                    'fallback_url': fallback_url,
                    'status': 'pending',
                    'failed_count': 0,
                    'last_error': '',
                    'next_retry_at': None,
                }
                return []
            cooling = row.get('status') == 'failed' and row.get('next_retry_at') == _FUTURE
            row['normalized_url'] = normalized_url
            row['kind'] = kind or row.get('kind', '')
            row['fallback_url'] = fallback_url or row.get('fallback_url', '')
            if row.get('status') != 'downloaded' and not cooling:
                row['status'] = 'pending'
            if not cooling:
                row['next_retry_at'] = None
            return []
        if 'INSERT INTO azurlane_assets' in sql and "'failed'" in sql:
            url, normalized_url, kind, fallback_url, last_error, *_retry_days = params
            row = self.assets.setdefault(
                str(url),
                {'url': url, 'normalized_url': normalized_url, 'kind': '', 'fallback_url': '', 'failed_count': 0},
            )
            row.update(
                {
                    'kind': kind or row.get('kind', ''),
                    'fallback_url': fallback_url or row.get('fallback_url', ''),
                    'status': 'failed',
                    'failed_count': int(row.get('failed_count') or 0) + 1,
                    'last_error': last_error,
                    'next_retry_at': _FUTURE,
                },
            )
            return []
        if sql.startswith('UPDATE azurlane_assets SET sha256'):
            await super().query_db(query, params)
            self.assets[str(params[-1])]['next_retry_at'] = None
            return []
        if 'INSERT INTO azurlane_blobs' in sql:
            sha256, size, content_type, blob_path = params
            stored = self.blobs.get((str(sha256), int(size)), {})
            self.blobs[(str(sha256), int(size))] = {
                'sha256': sha256,
                'size': size,
                'content_type': content_type or stored.get('content_type', ''),
                'blob_path': blob_path,
            }
            return []
        if 'INSERT INTO azurlane_characters' in sql:
            stored = dict(self.characters.get(str(params[0]), {}))
            await super().query_db(query, params)
            row = self.characters[str(params[0])]
            row['completed'] = stored.get('completed', False)
            if row['source_id'] is None:
                row['source_id'] = stored.get('source_id')
            row['name_zh'] = row['name_zh'] or stored.get('name_zh', '')
            row['name_en'] = row['name_en'] or stored.get('name_en', '')
            return []
        if 'INSERT INTO azurlane_models' in sql:
            stored = dict(self.models.get(str(params[0]), {}))
            await super().query_db(query, params)
            self.models[str(params[0])]['completed'] = stored.get('completed', False)
            return []
        return await super().query_db(query, params)

    def lapse_cooldowns(self) -> None:
        for row in [*self.models.values(), *self.assets.values()]:
            if row.get('next_retry_at'):
                row['next_retry_at'] = _PAST


def _ok(body: str, content_type: str = 'application/octet-stream', **headers: str) -> dict[str, Any]:
    return {'status': 200, 'headers': {'content-type': content_type, **headers}, 'body': body}


def _status(code: int, **headers: str) -> dict[str, Any]:
    return {'status': code, 'headers': dict(headers), 'body': ''}


def _json(payload: Any) -> dict[str, Any]:
    return _ok(json.dumps(payload, ensure_ascii=False), 'application/json')


def _skin(  # noqa: PLR0913 - the test helper's fields
    skin_id: int, name: str, key: str, *, kind: str = 'live2d', face_ids: list[str] | None = None, icons: bool = False
) -> dict[str, Any]:
    payload: dict[str, Any] = {
        'id': skin_id,
        'name': name,
        'skinTypeName': 'Skin',
        'featureTags': ['spine' if kind == 'spine' else 'Live2D'],
        'painting': key,
        'prefab': key,
        'dynamicType': kind,
        'isLive2d': kind == 'live2d',
        'isLive2dPlus': False,
        'isDynamic': True,
        'isSpine': kind == 'spine',
    }
    if face_ids is not None:
        payload['paintingFaceIds'] = face_ids
    if icons:
        payload['assetPaths'] = {'squareIcon': f'squareicon/{key}', 'shipyardIcon': f'shipyardicon/{key}', 'qIcon': f'qicon/{key}'}
    return payload


def _ship(ship_group_id: int, resource_key: str, name: str, skins: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        'shipGroupId': ship_group_id,
        'name': name,
        'englishName': name,
        'nationName': 'Royal Navy',
        'typeName': 'Destroyer',
        'rarityName': 'SR',
        'resourceKey': resource_key,
        'skins': skins,
    }


_MODEL3 = json.dumps({'Version': 3, 'FileReferences': {'Moc': 'model.moc3', 'Textures': ['textures/texture_00.webp']}})
_MESH = 'g mesh\nv 0 0 0\n'


def _painting_index(key: str, layers: dict[str, Any] | None = None) -> str:
    payload: dict[str, Any] = layers or {key: {'size': [1020, 992], 'rawSize': [1024, 682], 'position': [0, 0]}}
    payload['face'] = {'size': [100, 100], 'pivot': [0.5, 0.5], 'position': [0, 0]}
    return json.dumps(payload)


def _live2d(web: dict[str, list[dict[str, Any]]], key: str) -> None:
    web[f'{STATIC}/live2d/{key}/{key}.model3.json'] = [_ok(_MODEL3, 'application/json')]
    web[f'{STATIC}/live2d/{key}/model.moc3'] = [_ok(f'moc-{key}')]
    web[f'{STATIC}/live2d/{key}/textures/texture_00.webp'] = [_ok(f'texture-{key}', 'image/webp')]


def _painting(web: dict[str, list[dict[str, Any]]], key: str, *, name: str | None = None, mesh: bool = True) -> None:
    web[f'{STATIC}/painting/{name or key}.webp'] = [_ok(f'painting-{key}', 'image/webp')]
    web[f'{STATIC}/painting/{key}.json'] = [_ok(_painting_index(key), 'application/json')]
    if mesh:
        web[f'{STATIC}/painting/{key}-mesh.obj'] = [_ok(_MESH, 'application/x-tgif')]


def _web(*, second_run: bool) -> dict[str, list[dict[str, Any]]]:
    """URL -> the answers, in order (the last one repeats)."""
    ships = [
        _ship(1, 'javelin', 'Javelin', [_skin(1, 'Default', 'javelin', face_ids=['1'], icons=True)]),
        _ship(2, 'iris', 'Iris', [_skin(2, 'Afternoon', 'iris_2', kind='spine')]),
        _ship(3, 'xiafei', 'Richelieu', [_skin(3, 'Skin', 'xiafei_4')]),
        _ship(4, 'u47', 'U-47', [_skin(4, 'Default', 'u47_5')]),
    ]
    web: dict[str, list[dict[str, Any]]] = {
        sources.l2d_su_ship_index_url(sources.L2D_SU_PRIMARY_REGION): [
            _ok(json.dumps({'locale': 'CN', 'version': '9.7.295', 'ships': ships}), 'application/json', etag='"l2d"'),
        ],
        sources.l2d_su_ship_index_url(sources.L2D_SU_ENGLISH_REGION): [
            _ok(json.dumps({'locale': 'EN', 'version': '9.7.295', 'ships': ships}), 'application/json'),
        ],
        sources.NAGAMI_MAPPING_URL: [_json({'javelin': 'Javelin'})],
        sources.l2d_su_ship_detail_url(1): [
            _json(
                {
                    'ship': {
                        'shipGroupId': 1,
                        'className': 'J Class',
                        'skins': [{'id': 1, 'words': [{'key': 'detail', 'text': '嗯', 'voicePath': 'cue/cv-1/detail'}]}],
                    },
                },
            ),
        ],
        sources.l2d_su_ship_detail_url(2): [_status(404)],
        sources.l2d_su_ship_detail_url(3): [_json({'ship': {'shipGroupId': 3, 'skins': []}})],
        sources.l2d_su_ship_detail_url(4): [_status(503), _json({'ship': {'shipGroupId': 4, 'className': 'Type VII', 'skins': []}})],
        f'{sources.L2D_SU_AUDIO_BASE_URL}/cue/cv-1/detail.ogg': [_ok('ogg-bytes', 'audio/ogg')],
        f'{STATIC}/paintingface/javelin/1.webp': [_ok('face-bytes', 'image/webp')],
        f'{STATIC}/squareicon/javelin.webp': [_ok('square-bytes', 'image/webp')],
        f'{STATIC}/spinepainting/iris_2/iris_2.skel': [_ok('skel-bytes')],
        f'{STATIC}/spinepainting/iris_2/iris_2.atlas': [_ok('\niris_2.webp\nsize: 4096,4096\nformat: RGBA8888\n', 'text/plain')],
        f'{STATIC}/spinepainting/iris_2/iris_2.webp': [_ok('spine-texture', 'image/webp')],
        # The site shell answers a mesh the CDN does not host.
        f'{STATIC}/painting/xiafei_4_rw-mesh.obj': [_ok('<!doctype html><html><body>l2d.su</body></html>', 'text/html')],
        f'{STATIC}/painting/xiafei_4_rw.webp': [_ok('layer-bytes', 'image/webp')],
    }
    for key in ('javelin', 'xiafei_4', 'u47_5'):
        _live2d(web, key)
    _painting(web, 'javelin')
    _painting(web, 'iris_2')
    _painting(web, 'xiafei_4', mesh=False)
    layers = {
        'xiafei_4': {'size': [4574, 2866], 'pivot': [0.49, 0.66], 'position': [0, 0], 'rawSize': [2048, 1283], 'raw': True},
        'xiafei_4_rw': {'size': [2000, 2048], 'pivot': [0.49, 0.66], 'position': [10, 272], 'rawSize': [2000, 2048]},
    }
    web[f'{STATIC}/painting/xiafei_4.json'] = [_ok(_painting_index('xiafei_4', layers), 'application/json')]
    # The index names painting/u47_5.webp; only the case variant is hosted.
    _painting(web, 'u47_5', name='U47_5', mesh=False)
    texture = f'{STATIC}/live2d/javelin/textures/texture_00.webp'
    if second_run:
        web[texture] = [_ok('texture-javelin', 'image/webp')]
    else:
        web[texture] = [_status(503, **{'retry-after': '1'}), _status(404)]
    return web


def _normalize(value: Any, root: str, key: str = '') -> Any:
    if isinstance(value, dict):
        return {item_key: _normalize(item, root, item_key) for item_key, item in value.items() if item_key not in _DROPPED_TIMESTAMPS}
    if isinstance(value, list):
        return [_normalize(item, root) for item in value]
    if key == 'completed_at':
        return 'set' if value else None
    if key == 'next_retry_at':
        return 'set' if value else None
    if isinstance(value, str):
        return value.replace(root, '<root>')
    return value


def _tree_files(root: Path) -> dict[str, Any]:
    files: dict[str, Any] = {}
    for path in sorted(root.rglob('*')):
        if not path.is_file():
            continue
        relative = path.relative_to(root).as_posix()
        data = path.read_bytes()
        if path.suffix == '.json' and '/assets/' not in f'/{relative}' and not relative.startswith('_blobs/'):
            files[relative] = {'json': _normalize(json.loads(data), root.as_posix())}
        else:
            files[relative] = {'size': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
    return files


def _rows(database: _Database, root: str) -> dict[str, Any]:
    def cooling(row: dict[str, Any]) -> bool:
        return row.get('next_retry_at') == _FUTURE

    characters = {
        key: {
            'source_id': row.get('source_id'),
            'name_zh': row.get('name_zh', ''),
            'name_en': row.get('name_en', ''),
            'manifest_path': row.get('manifest_path', ''),
            'active': row.get('active', True),
            'completed': bool(row.get('completed')),
            'source_metadata': row.get('source_metadata', {}),
        }
        for key, row in sorted(database.characters.items())
    }
    model_fields = (
        'model_type',
        'source',
        'character_key',
        'costume_key',
        'costume_id',
        'costume_name_zh',
        'costume_name_en',
        'primary_url',
        'fallback_url',
        'display_info_url',
        'availability_state',
        'active',
        'failed_count',
        'last_error',
        'source_metadata',
    )
    models = {
        model_id: {
            **{field: row.get(field) for field in model_fields},
            'completed': bool(row.get('completed')),
            'cooling': cooling(row),
        }
        for model_id, row in sorted(database.models.items())
    }
    asset_fields = ('normalized_url', 'kind', 'fallback_url', 'status', 'sha256', 'size', 'content_type', 'downloaded_url')
    assets = {
        url: {
            **{field: row.get(field, 0 if field == 'size' else '') for field in asset_fields},
            'failed_count': row.get('failed_count', 0),
            'last_error': row.get('last_error', ''),
            'cooling': cooling(row),
        }
        for url, row in sorted(database.assets.items())
    }
    relation_fields = ('model_id', 'url', 'kind', 'context_hash', 'local_path', 'original_filename', 'fallback_url', 'context_json')
    model_assets = sorted(
        ({field: row[field] for field in relation_fields} for row in database.model_assets),
        key=lambda row: (row['model_id'], row['url'], row['kind'], row['context_hash']),
    )
    ship_details = {
        str(ship_id): {'region': row['region'], 'fingerprint': row['fingerprint'], 'payload': row['payload']}
        for ship_id, row in sorted(database.ship_details.items())
    }
    blobs = {
        f'{sha256}:{size}': {'content_type': row['content_type'], 'blob_path': row['blob_path']}
        for (sha256, size), row in sorted(database.blobs.items())
    }
    return _normalize(
        {
            'characters': characters,
            'models': models,
            'assets': assets,
            'model_assets': model_assets,
            'ship_details': ship_details,
            'blobs': blobs,
        },
        root,
    )


def _crawl() -> list[dict[str, Any]]:
    database = _Database()
    saved = (azurlane.database, sources._utc_now_iso, azurlane.secrets.randbelow, azurlane.asyncio.sleep)  # noqa: SLF001
    real_sleep = asyncio.sleep
    runs: list[dict[str, Any]] = []
    sleeps: list[float] = []

    async def recording_sleep(delay: float, *_args: Any) -> None:
        if delay > 0:
            sleeps.append(delay)
        await real_sleep(0)

    azurlane.database = database
    sources._utc_now_iso = lambda: FIXED_NOW_ISO  # noqa: SLF001
    azurlane.secrets.randbelow = lambda _n: 0
    azurlane.asyncio.sleep = recording_sleep
    try:
        with TemporaryDirectory() as tmp:
            root = Path(tmp) / 'azurlane'
            for second_run in (False, True):
                web = _web(second_run=second_run)
                served: dict[str, int] = {}
                requests: list[dict[str, Any]] = []
                sleeps.clear()

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
                            'accept': request.headers.get('accept', ''),
                            'referer': request.headers.get('referer', ''),
                            'user_agent': request.headers.get('user-agent', ''),
                        }
                    )
                    answers = web.get(url) or [_status(404)]
                    index = served.get(url, 0)
                    served[url] = index + 1
                    answer = answers[min(index, len(answers) - 1)]
                    return httpx.Response(answer['status'], headers=answer['headers'], content=answer['body'].encode())

                async def run(handler: Any = handler) -> str:
                    async with httpx.AsyncClient(
                        transport=httpx.MockTransport(handler),
                        follow_redirects=True,
                        headers=azurlane._asset_headers(),  # noqa: SLF001
                    ) as client:
                        with httpx.Client(
                            transport=httpx.MockTransport(handler),
                            follow_redirects=True,
                            headers=sources._request_headers(),  # noqa: SLF001
                        ) as source_client:
                            crawler = azurlane.AzurLane(
                                path=root,
                                client=client,
                                source_client=source_client,
                                api_request_interval_seconds=0,
                                cdn_request_interval_seconds=0,
                                origin_request_interval_seconds=0,
                                asset_process_concurrency=1,
                            )
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
                        'sleeps': list(sleeps),
                        'files': _tree_files(root),
                        'rows': _rows(database, root.as_posix()),
                    },
                )
                database.lapse_cooldowns()
    finally:
        azurlane.database, sources._utc_now_iso, azurlane.secrets.randbelow, azurlane.asyncio.sleep = saved  # noqa: SLF001
    return [{'fetched_at': FIXED_NOW_ISO, 'runs': runs}]


GOLDENS = {
    'azurlane/crawl.json': _crawl,
}
