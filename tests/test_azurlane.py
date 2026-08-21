# ruff: noqa: INP001, S101, SLF001

from __future__ import annotations

import asyncio
import hashlib
import json
import time
from contextlib import asynccontextmanager
from pathlib import Path
from types import SimpleNamespace
from typing import TYPE_CHECKING, Any

import httpx
import pytest

import src.service.jobs as jobs_module
import src.web.azurlane as azurlane_module
from src.api.schemas import JobRequestTarget
from src.core import settings
from src.core.settings import AzurLane as AzurLaneConfig
from src.tool.azurlane_l2d_sources import (
    L2D_SU_ENGLISH_REGION,
    L2D_SU_PRIMARY_REGION,
    L2D_SU_STATIC_BASE_URL,
    NAGAMI_MAPPING_URL,
    l2d_su_character_fingerprint,
    l2d_su_ship_index_url,
    parse_l2d_su_ship_index,
)
from src.web import AzurLane

if TYPE_CHECKING:
    from collections.abc import AsyncIterator


class FakeAzurLaneDatabase:
    def __init__(self) -> None:
        self.schema_created = False
        self.characters: dict[str, dict[str, Any]] = {}
        self.models: dict[str, dict[str, Any]] = {}
        self.assets: dict[str, dict[str, Any]] = {}
        self.blobs: dict[tuple[str, int], dict[str, Any]] = {}
        self.model_assets: list[dict[str, Any]] = []
        self.ship_details: dict[int, dict[str, Any]] = {}
        self.queries: list[str] = []
        # What a failed model's cooldown is stamped with. Set it to None to play out the run
        # that happens once the wait has lapsed.
        self.model_retry_at: str | None = 'future'

    @asynccontextmanager
    async def advisory_lock(self, _name: str) -> AsyncIterator[bool]:
        yield True

    async def query_db_multi(self, query: str, params: tuple[Any, ...] = ()) -> list[list[dict[str, Any]]]:
        assert params == ()
        assert 'CREATE TABLE IF NOT EXISTS azurlane_characters' in query
        self.schema_created = True
        return []

    async def query_db_transaction(self, statements: list[tuple[str, tuple[Any, ...]]]) -> list[list[dict[str, Any]]]:
        results: list[list[dict[str, Any]]] = []
        for sql, params in statements:
            results.append(await self.query_db(sql, params))
        return results

    async def query_db(self, query: str, params: tuple[Any, ...] = ()) -> list[dict[str, Any]]:  # noqa: C901, PLR0911, PLR0912, PLR0915
        sql = ' '.join(query.split())
        self.queries.append(sql)
        if sql.startswith('SELECT character_key, source_id, name_zh, name_en, manifest_path'):
            return [
                {
                    'character_key': key,
                    'source_id': row.get('source_id'),
                    'name_zh': row.get('name_zh', ''),
                    'name_en': row.get('name_en', ''),
                    'manifest_path': row.get('manifest_path', ''),
                    'active': row.get('active', True),
                    'source_metadata': row.get('source_metadata', {}),
                    'fetched_at': row.get('fetched_at'),
                    'completed_at': 'completed' if row.get('completed') else None,
                }
                for key, row in sorted(self.characters.items())
                if row.get('active', True)
            ]
        if sql.startswith('SELECT costume_id, primary_url'):
            prefix = str(params[0]).rstrip('%')
            return [
                {'costume_id': row.get('costume_id'), 'primary_url': row.get('primary_url', '')}
                for row in self.models.values()
                if row.get('costume_id') is not None
                and row.get('completed')
                and row.get('model_type') != 'painting'
                and str(row.get('primary_url', '')).startswith(prefix)
            ]
        if sql.startswith('SELECT ship_group_id, payload->'):
            rows: list[dict[str, Any]] = []
            for ship_id, row in sorted(self.ship_details.items()):
                ship = row.get('payload', {}).get('ship')
                class_name = ship.get('className') if isinstance(ship, dict) else None
                rows.append({'ship_group_id': ship_id, 'class_name': class_name})
            return rows
        if sql.startswith('SELECT ship_group_id, fingerprint FROM azurlane_ship_details'):
            return [
                {'ship_group_id': ship_id, 'fingerprint': row.get('fingerprint', '')} for ship_id, row in sorted(self.ship_details.items())
            ]
        if sql.startswith('SELECT payload FROM azurlane_ship_details'):
            row = self.ship_details.get(int(params[0]))
            return [{'payload': row.get('payload', {})}] if row else []
        if sql.startswith('SELECT payload, fetched_at FROM azurlane_ship_details'):
            row = self.ship_details.get(int(params[0]))
            return [{'payload': row.get('payload', {}), 'fetched_at': row.get('fetched_at')}] if row else []
        if 'INSERT INTO azurlane_ship_details' in sql:
            ship_id, region, fingerprint, payload = params
            self.ship_details[int(str(ship_id))] = {
                'ship_group_id': ship_id,
                'region': region,
                'fingerprint': fingerprint,
                'payload': json.loads(str(payload)),
            }
            return []
        if sql.startswith('SELECT model_id, model_type, source, character_key'):
            return [
                {
                    'model_id': model_id,
                    'model_type': row.get('model_type', ''),
                    'source': row.get('source', ''),
                    'character_key': row.get('character_key', ''),
                    'costume_key': row.get('costume_key', ''),
                    'costume_id': row.get('costume_id'),
                    'costume_name_zh': row.get('costume_name_zh', ''),
                    'costume_name_en': row.get('costume_name_en', ''),
                    'primary_url': row.get('primary_url', ''),
                    'fallback_url': row.get('fallback_url', ''),
                    'display_info_url': row.get('display_info_url', ''),
                    'availability_state': row.get('availability_state', 'unchecked'),
                    'active': row.get('active', True),
                    'source_metadata': row.get('source_metadata', {}),
                    'fetched_at': row.get('fetched_at'),
                    'completed_at': 'completed' if row.get('completed') else None,
                }
                for model_id, row in sorted(
                    self.models.items(),
                    key=lambda item: (
                        str(item[1].get('character_key', '')),
                        str(item[1].get('model_type', '')),
                        str(item[1].get('costume_key', '')),
                        item[0],
                    ),
                )
                if row.get('active', True)
            ]
        if 'FROM azurlane_model_assets AS ma JOIN azurlane_assets AS a' in sql:
            model_ids = {str(model_id) for model_id in params[0]}
            rows: list[dict[str, Any]] = []
            for relation in self.model_assets:
                if relation['model_id'] not in model_ids:
                    continue
                asset = self.assets[relation['url']]
                rows.append(
                    {
                        'model_id': relation['model_id'],
                        'url': relation['url'],
                        'kind': relation['kind'],
                        'context_hash': relation['context_hash'],
                        'local_path': relation['local_path'],
                        'original_filename': relation['original_filename'],
                        'relation_fallback_url': relation['fallback_url'],
                        'context_json': relation['context_json'],
                        'normalized_url': asset.get('normalized_url', ''),
                        'downloaded_url': asset.get('downloaded_url', ''),
                        'asset_fallback_url': asset.get('fallback_url', ''),
                        'sha256': asset.get('sha256', ''),
                        'size': asset.get('size', 0),
                        'content_type': asset.get('content_type', ''),
                        'status': asset.get('status', 'pending'),
                        'failed_count': asset.get('failed_count', 0),
                        'last_error': asset.get('last_error', ''),
                        'last_attempt_at': asset.get('last_attempt_at'),
                        'next_retry_at': asset.get('next_retry_at'),
                        'last_seen_at': asset.get('last_seen_at'),
                    },
                )
            return sorted(
                rows,
                key=lambda row: (row['model_id'], row['kind'], row['local_path'], row['url'], row['context_hash']),
            )
        if sql.startswith('UPDATE azurlane_characters SET active = FALSE'):
            for row in self.characters.values():
                row['active'] = False
            return []
        if sql.startswith('UPDATE azurlane_models SET active = FALSE'):
            for row in self.models.values():
                row['active'] = False
            return []
        if 'INSERT INTO azurlane_characters' in sql:
            character_key, source_id, name_zh, name_en, manifest_path, source_metadata = params
            self.characters[str(character_key)] = {
                'character_key': character_key,
                'source_id': source_id,
                'name_zh': name_zh,
                'name_en': name_en,
                'manifest_path': manifest_path,
                'source_metadata': json.loads(str(source_metadata)),
                'active': True,
            }
            return []
        if 'INSERT INTO azurlane_models' in sql:
            (
                model_id,
                model_type,
                source,
                character_key,
                costume_key,
                costume_id,
                costume_name_zh,
                costume_name_en,
                primary_url,
                fallback_url,
                display_info_url,
                availability_state,
                source_metadata,
            ) = params
            metadata = json.loads(str(source_metadata))
            stored = self.models.get(str(model_id), {})
            # Mirrors the upsert's CASE: geometry merged in during the download phase outlives
            # the catalog refresh that starts every run.
            stored_index = stored.get('source_metadata', {}).get('painting_index')
            if isinstance(stored_index, dict):
                metadata['painting_index'] = stored_index
            # And the other CASE: the retry cooldown survives the refresh unless the source
            # moved the URL it was waiting on.
            url_moved = bool(stored) and stored.get('primary_url') != primary_url
            self.models[str(model_id)] = {
                'model_id': model_id,
                'model_type': model_type,
                'source': source,
                'character_key': character_key,
                'costume_key': costume_key,
                'costume_id': costume_id,
                'costume_name_zh': costume_name_zh,
                'costume_name_en': costume_name_en,
                'primary_url': primary_url,
                'fallback_url': fallback_url,
                'display_info_url': display_info_url,
                'availability_state': availability_state,
                'source_metadata': metadata,
                'active': True,
                'completed': False,
                'failed_count': 0 if url_moved else stored.get('failed_count', 0),
                'last_error': stored.get('last_error', ''),
                'next_retry_at': None if url_moved else stored.get('next_retry_at'),
            }
            return []
        if sql.startswith('UPDATE azurlane_models SET fetched_at'):
            self.models[str(params[0])]['completed'] = False
            return []
        if sql.startswith('SELECT model_id, next_retry_at, (next_retry_at > NOW()) AS in_cooldown'):
            return [
                {
                    'model_id': model_id,
                    'next_retry_at': row.get('next_retry_at'),
                    'in_cooldown': bool(row.get('next_retry_at')),
                }
                for model_id, row in sorted(self.models.items())
                if int(row.get('failed_count') or 0) > 0
            ]
        if sql.startswith('UPDATE azurlane_models SET failed_count = failed_count + 1'):
            last_error, *_retry_days, model_id = params
            row = self.models[str(model_id)]
            row['failed_count'] = int(row.get('failed_count') or 0) + 1
            row['last_error'] = last_error
            row['next_retry_at'] = self.model_retry_at
            return []
        if sql.startswith('UPDATE azurlane_models SET failed_count = 0'):
            row = self.models[str(params[0])]
            row['failed_count'] = 0
            row['last_error'] = ''
            row['next_retry_at'] = None
            return []
        if sql.startswith('UPDATE azurlane_models SET source_metadata = source_metadata ||'):
            source_metadata, model_id = params
            self.models[str(model_id)].setdefault('source_metadata', {}).update(json.loads(str(source_metadata)))
            return []
        if sql.startswith('UPDATE azurlane_characters SET completed_at'):
            self.characters[str(params[0])]['completed'] = True
            return []
        if 'SELECT failed_count, last_error, next_retry_at FROM azurlane_assets' in sql:
            return []
        if 'FROM azurlane_assets AS a JOIN azurlane_blobs AS b' in sql:
            url = str(params[0])
            asset = self.assets.get(url)
            if asset is None or asset.get('status') != 'downloaded':
                return []
            key = (str(asset.get('sha256') or ''), int(asset.get('size') or 0))
            blob = self.blobs.get(key)
            if blob is None:
                return []
            return [
                {
                    'sha256': key[0],
                    'size': key[1],
                    'content_type': asset.get('content_type') or blob.get('content_type') or '',
                    'downloaded_url': asset.get('downloaded_url') or '',
                    'blob_path': blob['blob_path'],
                },
            ]
        if sql.startswith("UPDATE azurlane_assets SET status = 'missing'"):
            self.assets.setdefault(str(params[0]), {})['status'] = 'missing'
            return []
        if 'INSERT INTO azurlane_assets' in sql and "'pending'" in sql:
            url, normalized_url, kind, fallback_url = params
            row = self.assets.setdefault(str(url), {'failed_count': 0})
            row.update(
                {
                    'url': url,
                    'normalized_url': normalized_url,
                    'kind': kind,
                    'fallback_url': fallback_url,
                    'status': row.get('status') if row.get('status') == 'downloaded' else 'pending',
                },
            )
            return []
        if sql.startswith('UPDATE azurlane_assets SET sha256'):
            sha256, size, content_type, downloaded_url, url = params
            row = self.assets.setdefault(str(url), {'failed_count': 0})
            row.update(
                {
                    'sha256': sha256,
                    'size': size,
                    'content_type': content_type,
                    'downloaded_url': downloaded_url,
                    'status': 'downloaded',
                    'failed_count': 0,
                    'last_error': '',
                },
            )
            return []
        if 'INSERT INTO azurlane_assets' in sql and "'failed'" in sql:
            url, normalized_url, kind, fallback_url, last_error, *_retry_days = params
            row = self.assets.setdefault(str(url), {'failed_count': 0})
            row.update(
                {
                    'url': url,
                    'normalized_url': normalized_url,
                    'kind': kind,
                    'fallback_url': fallback_url,
                    'status': 'failed',
                    'failed_count': int(row.get('failed_count') or 0) + 1,
                    'last_error': last_error,
                    'next_retry_at': 'future',
                },
            )
            return []
        if 'SELECT blob_path, content_type FROM azurlane_blobs' in sql:
            sha256, size = params
            row = self.blobs.get((str(sha256), int(size)))
            return [row] if row is not None else []
        if sql.startswith('UPDATE azurlane_blobs SET last_verified_at'):
            return []
        if 'INSERT INTO azurlane_blobs' in sql:
            sha256, size, content_type, blob_path = params
            self.blobs[(str(sha256), int(size))] = {
                'sha256': sha256,
                'size': size,
                'content_type': content_type,
                'blob_path': blob_path,
            }
            return []
        if sql.startswith('DELETE FROM azurlane_model_assets'):
            model_id = str(params[0])
            self.model_assets = [row for row in self.model_assets if row['model_id'] != model_id]
            return []
        if 'INSERT INTO azurlane_model_assets' in sql:
            model_id, url, kind, hash_value, local_path, original_filename, fallback_url, context_json = params
            self.model_assets.append(
                {
                    'model_id': model_id,
                    'url': url,
                    'kind': kind,
                    'context_hash': hash_value,
                    'local_path': local_path,
                    'original_filename': original_filename,
                    'fallback_url': fallback_url,
                    'context_json': json.loads(str(context_json)),
                },
            )
            return []
        if sql.startswith('UPDATE azurlane_models SET completed_at'):
            self.models[str(params[0])]['completed'] = True
            return []
        raise AssertionError(sql)


_PRIMARY_INDEX_URL = l2d_su_ship_index_url(L2D_SU_PRIMARY_REGION)
_ENGLISH_INDEX_URL = l2d_su_ship_index_url(L2D_SU_ENGLISH_REGION)


def _job_cfg(*, enabled: bool = True, cron: str = '0 */6 * * *') -> SimpleNamespace:
    return SimpleNamespace(cron=cron, enabled=enabled)


def _ship_index_payload(ships: list[dict[str, object]], *, region: str = L2D_SU_PRIMARY_REGION) -> str:
    return json.dumps({'locale': region, 'version': '9.7.295', 'ships': ships})


def _ship(ship_group_id: int, resource_key: str, name: str, *, skins: list[dict[str, object]]) -> dict[str, object]:
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


def _skin(  # noqa: PLR0913
    skin_id: int,
    name: str,
    *,
    key: str,
    kind: str = 'live2d',
    face_ids: list[str] | None = None,
    icons: bool = False,
) -> dict[str, object]:
    payload: dict[str, object] = {
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


def _nagami_mapping_payload(mapping: dict[str, str] | None = None) -> str:
    return json.dumps(mapping or {})


def _live2d_url(key: str) -> str:
    return f'{L2D_SU_STATIC_BASE_URL}/live2d/{key}/{key}.model3.json'


def _spine_url(key: str) -> str:
    return f'{L2D_SU_STATIC_BASE_URL}/spinepainting/{key}'


def _painting_url(key: str) -> str:
    return f'{L2D_SU_STATIC_BASE_URL}/painting/{key}.webp'


def _painting_index_payload(painting_key: str, *, layers: dict[str, dict[str, Any]] | None = None) -> str:
    payload: dict[str, Any] = layers or {painting_key: {'size': [1020, 992], 'rawSize': [1024, 682], 'position': [0, 0]}}
    payload['face'] = {'size': [100, 100], 'pivot': [0.5, 0.5], 'position': [0, 0]}
    return json.dumps(payload)


def _painting_response(url: str) -> httpx.Response | None:
    """The three files a painting is made of, as the CDN serves them."""
    prefix = f'{L2D_SU_STATIC_BASE_URL}/painting/'
    if not url.startswith(prefix):
        return None
    name = url.removeprefix(prefix)
    if name.endswith('-mesh.obj'):
        return httpx.Response(200, content=b'g mesh\nv 0 0 0\n', headers={'content-type': 'application/x-tgif'})
    if name.endswith('.webp'):
        return httpx.Response(200, content=b'painting-bytes', headers={'content-type': 'image/webp'})
    if name.endswith('.json'):
        payload = _painting_index_payload(name.removesuffix('.json'))
        return httpx.Response(200, text=payload, headers={'content-type': 'application/json'})
    return None


def _seed_ship_detail(fake_db: FakeAzurLaneDatabase, index_payload: str, *, payload: dict[str, Any] | None = None) -> None:
    """Store a ship detail row whose fingerprint matches the index, so update() skips the origin fetch."""
    for character in parse_l2d_su_ship_index(index_payload).characters:
        fake_db.ship_details[character.char_id] = {
            'ship_group_id': character.char_id,
            'region': L2D_SU_PRIMARY_REGION,
            'fingerprint': l2d_su_character_fingerprint(character),
            'payload': payload or {'ship': {'shipGroupId': character.char_id, 'skins': []}},
        }


def _source_index_response(payload: str, *, url: str, headers: dict[str, str] | None = None) -> httpx.Response | None:
    if url == _PRIMARY_INDEX_URL:
        return httpx.Response(200, text=payload, headers=headers)
    if url == _ENGLISH_INDEX_URL:
        return httpx.Response(200, text=payload, headers=headers)
    return None


def _live2d_model3_payload() -> str:
    return json.dumps(
        {
            'Version': 3,
            'FileReferences': {
                'Moc': 'javelin.moc3',
                'Textures': ['textures/texture_00.webp'],
            },
        },
    )


def _install_fake_database(monkeypatch: pytest.MonkeyPatch) -> FakeAzurLaneDatabase:
    fake = FakeAzurLaneDatabase()
    monkeypatch.setattr(azurlane_module, 'database', fake)
    return fake


def _seed_downloaded_blob(  # noqa: PLR0913
    *,
    fake_db: FakeAzurLaneDatabase,
    root: Path,
    url: str,
    kind: str,
    content: bytes,
    content_type: str,
) -> None:
    sha256 = hashlib.sha256(content).hexdigest()
    relative_path = Path('_blobs/sha256') / sha256[:2] / sha256
    blob_path = root / relative_path
    blob_path.parent.mkdir(parents=True, exist_ok=True)
    blob_path.write_bytes(content)
    fake_db.blobs[(sha256, len(content))] = {
        'sha256': sha256,
        'size': len(content),
        'content_type': content_type,
        'blob_path': relative_path.as_posix(),
    }
    fake_db.assets[url] = {
        'url': url,
        'normalized_url': url,
        'kind': kind,
        'status': 'downloaded',
        'sha256': sha256,
        'size': len(content),
        'content_type': content_type,
        'downloaded_url': url,
        'failed_count': 0,
        'last_error': '',
    }


def test_azurlane_config_defaults_to_disabled_collection_path() -> None:
    cfg = AzurLaneConfig()

    assert cfg.enabled is False
    assert cfg.cron == '0 */6 * * *'
    assert cfg.path == Path('./collection/azurlane')


def test_scheduler_registration_includes_azurlane() -> None:
    fake_config = settings.Settings()
    fake_config.web.azurlane.cron = '5 1 * * *'
    fake_config.web.azurlane.enabled = False

    jobs = jobs_module.build_jobs(fake_config)
    azurlane_job = next(job for job in jobs if job.key == 'azurlane')

    assert azurlane_job.name == 'Azur Lane'
    assert azurlane_job.cron == '5 1 * * *'
    assert azurlane_job.enabled is False
    assert azurlane_job.required_commands == ()
    assert azurlane_job.factory is jobs_module.AzurLane


def test_azurlane_job_stays_parked_until_an_origin_proxy_is_configured() -> None:
    fake_config = settings.Settings()
    fake_config.web.azurlane.enabled = True

    parked = next(job for job in jobs_module.build_jobs(fake_config) if job.key == 'azurlane')

    # Without a proxy the origin is unreachable, so running would only preserve stale state.
    assert parked.missing_fields == ('origin_proxy',)
    assert parked.enabled is False

    fake_config.web.azurlane.origin_proxy = 'http://user:pass@proxy.example:8080'
    ready = next(job for job in jobs_module.build_jobs(fake_config) if job.key == 'azurlane')

    assert ready.missing_fields == ()
    assert ready.enabled is True


def test_api_job_enum_includes_azurlane() -> None:
    assert JobRequestTarget.AZURLANE.value == 'azurlane'


def test_azurlane_update_downloads_live2d_assets(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    model3_url = _live2d_url('javelin')
    moc_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/javelin.moc3'
    texture_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp'
    catalog = _ship_index_payload([_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin')])])

    def handler(request: httpx.Request) -> httpx.Response:  # noqa: PLR0911
        url = str(request.url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        if url == model3_url:
            return httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'})
        if url == moc_url:
            return httpx.Response(200, content=b'moc-bytes', headers={'content-type': 'application/octet-stream'})
        if url == texture_url:
            return httpx.Response(200, content=b'webp-bytes', headers={'content-type': 'image/webp'})
        return httpx.Response(404)

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    root = tmp_path / 'javelin - Javelin'
    assert fake_db.schema_created is True
    assert fake_db.characters['javelin']['active'] is True
    assert fake_db.models['azurlane:live2d:javelin:javelin']['completed'] is True
    assert fake_db.models['azurlane:painting:javelin:javelin']['completed'] is True
    assert (root / 'assets/live2d/javelin/javelin.model3.json').exists()
    assert (root / 'assets/live2d/javelin/javelin.moc3').read_bytes() == b'moc-bytes'
    assert (root / 'assets/live2d/javelin/textures/texture_00.webp').read_bytes() == b'webp-bytes'
    assert (root / 'assets/painting/javelin/javelin.webp').read_bytes() == b'painting-bytes'
    assert {row['kind'] for row in fake_db.model_assets} == {
        'live2d.model3',
        'live2d.moc3',
        'live2d.texture',
        'painting.index',
        'painting.image',
        'painting.mesh',
    }
    assert fake_db.assets[texture_url]['status'] == 'downloaded'


def test_azurlane_update_resolves_model_path_from_ship_detail_when_derived_path_is_missing(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_db = _install_fake_database(monkeypatch)
    derived_url = _live2d_url('bisimaiz')
    real_url = _live2d_url('bisimaiZ')
    detail_url = 'https://l2d.su/data/ships/CN/40505.json'
    moc_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/bisimaiZ/javelin.moc3'
    texture_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/bisimaiZ/textures/texture_00.webp'
    catalog = _ship_index_payload([_ship(40505, 'bisimaiz', 'Bismarck Zwei', skins=[_skin(405050, 'Zwei', key='bisimaiz')])])
    detail_skin = {'id': 405050, 'model': {'type': 'live2d', 'path': 'live2d/bisimaiZ/bisimaiZ.model3.json'}}
    detail_payload = json.dumps({'ship': {'shipGroupId': 40505, 'skins': [detail_skin]}})
    requested: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:  # noqa: PLR0911
        url = str(request.url)
        requested.append(f'{request.method} {url}')
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        if url == detail_url:
            return httpx.Response(200, text=detail_payload, headers={'content-type': 'application/json'})
        if url == real_url:
            return httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'})
        if url == moc_url:
            return httpx.Response(200, content=b'moc-bytes', headers={'content-type': 'application/octet-stream'})
        if url == texture_url:
            return httpx.Response(200, content=b'webp-bytes', headers={'content-type': 'image/webp'})
        return httpx.Response(404)

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    model = fake_db.models['azurlane:live2d:bisimaiz:bisimaiz']
    assert model['primary_url'] == real_url
    assert model['completed'] is True
    assert f'HEAD {derived_url}' in requested
    assert requested.count(f'GET {detail_url}') == 1
    assert (tmp_path / 'bisimaiz - Bismarck Zwei/assets/live2d/bisimaiz/javelin.moc3').read_bytes() == b'moc-bytes'


def test_azurlane_update_reuses_stored_model_path_without_probing(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    real_url = _live2d_url('bisimaiZ')
    moc_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/bisimaiZ/javelin.moc3'
    texture_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/bisimaiZ/textures/texture_00.webp'
    fake_db.models['azurlane:live2d:bisimaiz:bisimaiz'] = {
        'model_id': 'azurlane:live2d:bisimaiz:bisimaiz',
        'costume_id': 405050,
        'primary_url': real_url,
        'completed': True,
        'active': True,
    }
    catalog = _ship_index_payload([_ship(40505, 'bisimaiz', 'Bismarck Zwei', skins=[_skin(405050, 'Zwei', key='bisimaiz')])])
    _seed_ship_detail(fake_db, catalog)
    requested: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:  # noqa: PLR0911
        url = str(request.url)
        requested.append(f'{request.method} {url}')
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        if url == real_url:
            return httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'})
        if url == moc_url:
            return httpx.Response(200, content=b'moc-bytes', headers={'content-type': 'application/octet-stream'})
        if url == texture_url:
            return httpx.Response(200, content=b'webp-bytes', headers={'content-type': 'image/webp'})
        return httpx.Response(404)

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    assert fake_db.models['azurlane:live2d:bisimaiz:bisimaiz']['primary_url'] == real_url
    assert not [entry for entry in requested if entry.startswith('HEAD ')]
    assert not [entry for entry in requested if '/data/ships/' in entry]


def test_azurlane_update_writes_backend_manifests_and_source_artifacts(  # noqa: PLR0915
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _install_fake_database(monkeypatch)
    model3_url = _live2d_url('javelin')
    moc_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/javelin.moc3'
    texture_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp'
    model3_payload = _live2d_model3_payload()
    catalog = _ship_index_payload([_ship(1, 'javelin', '标枪', skins=[_skin(1, '默认', key='javelin')])])
    english_catalog = _ship_index_payload(
        [_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin')])],
        region=L2D_SU_ENGLISH_REGION,
    )

    def handler(request: httpx.Request) -> httpx.Response:  # noqa: PLR0911
        url = str(request.url)
        if url == _PRIMARY_INDEX_URL:
            return httpx.Response(200, text=catalog, headers={'etag': '"l2d"'})
        if url == _ENGLISH_INDEX_URL:
            return httpx.Response(200, text=english_catalog)
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload({'javelin': 'Javelin'}), headers={'etag': '"nagami"'})
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        if url == model3_url:
            return httpx.Response(200, text=model3_payload, headers={'content-type': 'application/json'})
        if url == moc_url:
            return httpx.Response(200, content=b'moc-bytes', headers={'content-type': 'application/octet-stream'})
        if url == texture_url:
            return httpx.Response(200, content=b'webp-bytes', headers={'content-type': 'image/webp'})
        return httpx.Response(404)

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    source_root = tmp_path / '_source'
    l2d_snapshot = json.loads((source_root / 'l2d-su-snapshot.json').read_text(encoding='utf-8'))
    nagami_snapshot = json.loads((source_root / 'nagami-snapshot.json').read_text(encoding='utf-8'))
    health_report = json.loads((source_root / 'health-report.json').read_text(encoding='utf-8'))
    manifest = json.loads((tmp_path / 'javelin - Javelin/manifest.json').read_text(encoding='utf-8'))
    character = json.loads((tmp_path / 'javelin - Javelin/character.json').read_text(encoding='utf-8'))

    assert l2d_snapshot['metadata']['etag'] == '"l2d"'
    assert nagami_snapshot['metadata']['etag'] == '"nagami"'
    assert health_report['catalog_health']['entry_count'] == 2  # noqa: PLR2004
    assert manifest['schema_version'] == 1
    assert manifest['character_key'] == 'javelin'
    assert manifest['model_counts'] == {'live2d': 1, 'spine': 0, 'painting': 1, 'total': 2}
    assert character['models'][0]['model_id'] == 'azurlane:live2d:javelin:javelin'

    model = manifest['models'][0]
    assert model['source'] == 'merged'
    assert model['source_urls'] == {
        'primary': model3_url,
        'fallback': 'https://cdn.nagami.moe/live2d/javelin/javelin.model3.json',
        'display_info': '',
    }
    assert model['availability']['archive_state'] == 'complete'
    assert model['availability']['asset_status_counts'] == {'downloaded': 3}

    assets_by_kind = {asset['kind']: asset for asset in model['assets']}
    assert assets_by_kind['live2d.model3']['sha256'] == hashlib.sha256(model3_payload.encode()).hexdigest()
    assert assets_by_kind['live2d.model3']['fallback_url'] == 'https://cdn.nagami.moe/live2d/javelin/javelin.model3.json'
    assert assets_by_kind['live2d.moc3']['fallback_url'] == 'https://cdn.nagami.moe/live2d/javelin/javelin.moc3'
    assert assets_by_kind['live2d.texture']['content_type'] == 'image/webp'
    assert assets_by_kind['live2d.texture']['source_urls']['primary'] == texture_url
    assert assets_by_kind['live2d.texture']['available'] is True
    assert assets_by_kind['live2d.texture']['contexts'][0]['fallback_model_url'] == (
        'https://cdn.nagami.moe/live2d/javelin/javelin.model3.json'
    )


def test_azurlane_update_downloads_spine_assets(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    spine_url = _spine_url('iris_2')
    skel_url = f'{spine_url}/iris_2.skel'
    atlas_url = f'{spine_url}/iris_2.atlas'
    texture_url = f'{spine_url}/iris_2.webp'
    atlas_text = '\niris_2.webp\nsize: 4096,4096\nformat: RGBA8888\n'
    catalog = _ship_index_payload([_ship(2, 'iris', 'Iris', skins=[_skin(2, 'Afternoon', key='iris_2', kind='spine')])])

    def handler(request: httpx.Request) -> httpx.Response:  # noqa: PLR0911
        url = str(request.url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        if url == skel_url:
            return httpx.Response(200, content=b'skel-bytes', headers={'content-type': 'application/octet-stream'})
        if url == atlas_url:
            return httpx.Response(200, text=atlas_text, headers={'content-type': 'text/plain'})
        if url == texture_url:
            return httpx.Response(200, content=b'texture-bytes', headers={'content-type': 'image/webp'})
        return httpx.Response(404)

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    root = tmp_path / 'iris - Iris'
    assert fake_db.models['azurlane:spine:iris:iris_2']['completed'] is True
    assert (root / 'assets/spine/iris_2/iris_2.skel').read_bytes() == b'skel-bytes'
    assert (root / 'assets/spine/iris_2/iris_2.atlas').read_text(encoding='utf-8') == atlas_text
    assert (root / 'assets/spine/iris_2/iris_2.webp').read_bytes() == b'texture-bytes'
    assert {row['kind'] for row in fake_db.model_assets} == {
        'spine.skel',
        'spine.atlas',
        'spine.texture',
        'painting.index',
        'painting.image',
        'painting.mesh',
    }


def test_azurlane_update_downloads_multi_part_spine_model(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    spine_url = _spine_url('iris_2')
    parts_url = f'{spine_url}/iris_2.json'
    parts_payload = json.dumps(
        {
            'version': 1,
            'models': [
                {'name': 'iris_2B', 'skeleton': 'iris_2B.skel', 'atlases': ['iris_2B.atlas'], 'animation': 'normal', 'loop': True},
                {'name': 'iris_2T', 'skeleton': 'iris_2T.skel', 'atlases': ['iris_2T.atlas'], 'animation': 'normal', 'loop': True},
            ],
        },
    )
    atlas_texts = {
        f'{spine_url}/iris_2B.atlas': '\niris_2B.webp\nsize: 4096,4096\nformat: RGBA8888\n',
        f'{spine_url}/iris_2T.atlas': '\niris_2T.webp\nsize: 4096,4096\nformat: RGBA8888\n',
    }
    skeleton_urls = {f'{spine_url}/iris_2B.skel', f'{spine_url}/iris_2T.skel'}
    texture_urls = {f'{spine_url}/iris_2B.webp', f'{spine_url}/iris_2T.webp'}
    catalog = _ship_index_payload([_ship(2, 'iris', 'Iris', skins=[_skin(2, 'Afternoon', key='iris_2', kind='spine')])])

    def handler(request: httpx.Request) -> httpx.Response:  # noqa: PLR0911
        url = str(request.url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        if url == parts_url:
            return httpx.Response(200, text=parts_payload, headers={'content-type': 'application/json'})
        if url in skeleton_urls:
            return httpx.Response(200, content=b'skel-bytes', headers={'content-type': 'application/octet-stream'})
        if url in atlas_texts:
            return httpx.Response(200, text=atlas_texts[url], headers={'content-type': 'text/plain'})
        if url in texture_urls:
            return httpx.Response(200, content=b'texture-bytes', headers={'content-type': 'image/webp'})
        return httpx.Response(404)

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    root = tmp_path / 'iris - Iris'
    assert fake_db.models['azurlane:spine:iris:iris_2']['completed'] is True
    assert (root / 'assets/spine/iris_2/iris_2.json').read_text(encoding='utf-8') == parts_payload
    assert (root / 'assets/spine/iris_2/iris_2B.skel').read_bytes() == b'skel-bytes'
    assert (root / 'assets/spine/iris_2/iris_2T.webp').read_bytes() == b'texture-bytes'
    assert not (root / 'assets/spine/iris_2/iris_2.skel').exists()
    assert {row['kind'] for row in fake_db.model_assets} == {
        'spine.parts',
        'spine.skel',
        'spine.atlas',
        'spine.texture',
        'painting.index',
        'painting.image',
        'painting.mesh',
    }
    assert [row['kind'] for row in fake_db.model_assets].count('spine.skel') == len(skeleton_urls)


def test_azurlane_update_reuses_completed_blob_for_archived_url(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    model3_url = _live2d_url('javelin')
    moc_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/javelin.moc3'
    texture_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp'
    reused_texture = b'already-archived-texture'
    _seed_downloaded_blob(
        fake_db=fake_db,
        root=tmp_path,
        url=texture_url,
        kind='live2d.texture',
        content=reused_texture,
        content_type='image/webp',
    )
    catalog = _ship_index_payload([_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin')])])
    seen_asset_urls: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        seen_asset_urls.append(url)
        if url == texture_url:
            pytest.fail(reason='texture URL should have been reused from the archived blob')
        if url == model3_url:
            return httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'})
        if url == moc_url:
            return httpx.Response(200, content=b'moc-bytes', headers={'content-type': 'application/octet-stream'})
        return httpx.Response(404)

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    materialized = tmp_path / 'javelin - Javelin/assets/live2d/javelin/textures/texture_00.webp'
    assert materialized.read_bytes() == reused_texture
    assert texture_url not in seen_asset_urls
    assert fake_db.assets[texture_url]['status'] == 'downloaded'


def test_azurlane_update_records_failed_asset_state(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    model3_url = _live2d_url('javelin')
    moc_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/javelin.moc3'
    texture_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp'
    catalog = _ship_index_payload([_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin')])])

    def handler(request: httpx.Request) -> httpx.Response:  # noqa: PLR0911
        url = str(request.url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        if url == model3_url:
            return httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'})
        if url == moc_url:
            return httpx.Response(200, content=b'moc-bytes', headers={'content-type': 'application/octet-stream'})
        if url == texture_url:
            return httpx.Response(404)
        return httpx.Response(404)

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            with pytest.raises(azurlane_module.CrawlRunError):
                asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    failed = fake_db.assets[texture_url]
    assert failed['status'] == 'failed'
    assert failed['failed_count'] == 1
    assert 'HTTP 404' in failed['last_error']
    assert failed['next_retry_at'] == 'future'
    assert any(row['url'] == texture_url and row['kind'] == 'live2d.texture' for row in fake_db.model_assets)
    model = fake_db.models['azurlane:live2d:javelin:javelin']
    assert model['failed_count'] == 1
    assert model['next_retry_at'] == 'future'
    assert 'Azur Lane assets failed' in model['last_error']


_SECOND_MODEL_FAILURE = 2


def _run_javelin_missing_texture_crawl(*, tmp_path: Path, seen_asset_urls: list[str], texture_hosted: bool = False) -> None:
    """One update() over a Javelin whose texture the CDN does not host until it does."""
    model3_url = _live2d_url('javelin')
    moc_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/javelin.moc3'
    texture_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp'
    catalog = _ship_index_payload([_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin')])])

    hosted = {
        model3_url: (_live2d_model3_payload().encode('utf-8'), 'application/json'),
        moc_url: (b'moc-bytes', 'application/octet-stream'),
    }
    if texture_hosted:
        hosted[texture_url] = (b'texture-bytes', 'image/webp')

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        seen_asset_urls.append(url)
        body = hosted.get(url)
        if body is None:
            return httpx.Response(404)
        return httpx.Response(200, content=body[0], headers={'content-type': body[1]})

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())


def test_azurlane_update_stays_quiet_while_a_failed_model_is_in_cooldown(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    first_run_urls: list[str] = []
    with pytest.raises(azurlane_module.CrawlRunError):
        _run_javelin_missing_texture_crawl(tmp_path=tmp_path, seen_asset_urls=first_run_urls)
    assert first_run_urls

    # The source has not changed, so the next run has nothing to report -- and nothing to
    # download either. The cheap existence probe that would notice a corrected path survives;
    # re-fetching the model's assets to watch them fail again does not.
    second_run_urls: list[str] = []
    _run_javelin_missing_texture_crawl(tmp_path=tmp_path, seen_asset_urls=second_run_urls)
    texture_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp'
    moc_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/javelin.moc3'
    assert texture_url not in second_run_urls
    assert moc_url not in second_run_urls
    assert len(second_run_urls) < len(first_run_urls)
    assert fake_db.assets[texture_url]['failed_count'] == 1
    assert fake_db.models['azurlane:live2d:javelin:javelin']['failed_count'] == 1


def test_azurlane_update_reports_a_failed_model_again_once_its_cooldown_lapses(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_db = _install_fake_database(monkeypatch)
    with pytest.raises(azurlane_module.CrawlRunError):
        _run_javelin_missing_texture_crawl(tmp_path=tmp_path, seen_asset_urls=[])

    fake_db.model_retry_at = None
    fake_db.models['azurlane:live2d:javelin:javelin']['next_retry_at'] = None
    retry_urls: list[str] = []
    with pytest.raises(azurlane_module.CrawlRunError):
        _run_javelin_missing_texture_crawl(tmp_path=tmp_path, seen_asset_urls=retry_urls)
    assert f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp' in retry_urls
    assert fake_db.models['azurlane:live2d:javelin:javelin']['failed_count'] == _SECOND_MODEL_FAILURE


def test_azurlane_update_retries_a_failed_model_when_the_source_moves_its_url(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_db = _install_fake_database(monkeypatch)
    with pytest.raises(azurlane_module.CrawlRunError):
        _run_javelin_missing_texture_crawl(tmp_path=tmp_path, seen_asset_urls=[])

    model_id = 'azurlane:live2d:javelin:javelin'
    assert fake_db.models[model_id]['next_retry_at'] == 'future'
    fake_db.models[model_id]['primary_url'] = _live2d_url('javelinOld')
    retry_urls: list[str] = []
    with pytest.raises(azurlane_module.CrawlRunError):
        _run_javelin_missing_texture_crawl(tmp_path=tmp_path, seen_asset_urls=retry_urls)
    assert f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp' in retry_urls


def test_azurlane_update_clears_the_retry_state_once_the_source_serves_the_model(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_db = _install_fake_database(monkeypatch)
    with pytest.raises(azurlane_module.CrawlRunError):
        _run_javelin_missing_texture_crawl(tmp_path=tmp_path, seen_asset_urls=[])

    model_id = 'azurlane:live2d:javelin:javelin'
    fake_db.model_retry_at = None
    fake_db.models[model_id]['next_retry_at'] = None
    _run_javelin_missing_texture_crawl(tmp_path=tmp_path, seen_asset_urls=[], texture_hosted=True)
    assert fake_db.models[model_id]['failed_count'] == 0
    assert fake_db.models[model_id]['next_retry_at'] is None
    assert fake_db.models[model_id]['last_error'] == ''


def test_azurlane_update_recovers_a_painting_stored_under_a_different_case(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The index says painting/u47_5.webp; only painting/U47_5.webp exists on the CDN."""
    fake_db = _install_fake_database(monkeypatch)
    catalog = _ship_index_payload([_ship(1, 'u47', 'U-47', skins=[_skin(1, 'Default', key='u47_5')])])
    _seed_ship_detail(fake_db, catalog)
    indexed = f'{L2D_SU_STATIC_BASE_URL}/painting/u47_5.webp'
    stored = f'{L2D_SU_STATIC_BASE_URL}/painting/U47_5.webp'
    model3_url = _live2d_url('u47_5')
    served = {
        stored: httpx.Response(200, content=b'painting-bytes', headers={'content-type': 'image/webp'}),
        model3_url: httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'}),
        # Names come from _live2d_model3_payload, which the shared helper writes.
        f'{L2D_SU_STATIC_BASE_URL}/live2d/u47_5/javelin.moc3': httpx.Response(200, content=b'moc'),
        f'{L2D_SU_STATIC_BASE_URL}/live2d/u47_5/textures/texture_00.webp': httpx.Response(200, content=b'tex'),
    }
    requested: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        requested.append(url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        return served.get(url) or httpx.Response(404)

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=transport)
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    # The URL the source named is tried first and only then the case variant.
    assert requested.index(indexed) < requested.index(stored)
    asset = fake_db.assets[indexed]
    assert asset['status'] == 'downloaded'
    assert asset['downloaded_url'] == stored
    assert fake_db.models['azurlane:painting:u47:u47_5']['completed'] is True


def test_azurlane_update_completes_a_model_whose_optional_assets_are_missing(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The source advertises companion files it does not host; losing one must not fail the model."""
    fake_db = _install_fake_database(monkeypatch)
    catalog = _ship_index_payload(
        [_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin', face_ids=['1'], icons=True)])],
    )
    _seed_ship_detail(fake_db, catalog)
    model3_url = _live2d_url('javelin')
    moc_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/javelin.moc3'
    texture_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp'
    square_icon_url = f'{L2D_SU_STATIC_BASE_URL}/squareicon/javelin.webp'

    live2d_files = {
        model3_url: httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'}),
        moc_url: httpx.Response(200, content=b'moc-bytes', headers={'content-type': 'application/octet-stream'}),
        texture_url: httpx.Response(200, content=b'texture-bytes', headers={'content-type': 'image/webp'}),
    }

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        # Every icon and face 404s, exactly as the live CDN does for these skins.
        return live2d_files.get(url) or httpx.Response(404)

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=transport)
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    assert fake_db.models['azurlane:painting:javelin:javelin']['completed'] is True
    assert fake_db.assets[square_icon_url]['status'] == 'failed'
    # No qicon is ever requested: the index offers one for every skin and the CDN has none.
    assert not any('/qicon/' in url for url in fake_db.assets)


def test_azurlane_update_downloads_painting_faces_icons_and_voices(  # noqa: C901
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_db = _install_fake_database(monkeypatch)
    model3_url = _live2d_url('javelin')
    moc_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/javelin.moc3'
    texture_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp'
    detail_url = 'https://l2d.su/data/ships/CN/1.json'
    voice_url = f'{L2D_SU_STATIC_BASE_URL}/cue/cv-1/detail.ogg'
    face_url = f'{L2D_SU_STATIC_BASE_URL}/paintingface/javelin/1.webp'
    icon_urls = {
        f'{L2D_SU_STATIC_BASE_URL}/squareicon/javelin.webp',
        f'{L2D_SU_STATIC_BASE_URL}/shipyardicon/javelin.webp',
        f'{L2D_SU_STATIC_BASE_URL}/qicon/javelin.webp',
    }
    catalog = _ship_index_payload(
        [_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin', face_ids=['1'], icons=True)])],
    )
    detail_payload = json.dumps(
        {'ship': {'shipGroupId': 1, 'skins': [{'id': 1, 'words': [{'key': 'detail', 'text': '嗯', 'voicePath': 'cue/cv-1/detail'}]}]}},
        ensure_ascii=False,
    )
    requested: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:  # noqa: PLR0911
        url = str(request.url)
        requested.append(f'{request.method} {url}')
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        if url == detail_url:
            return httpx.Response(200, text=detail_payload, headers={'content-type': 'application/json'})
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        if url == face_url or url in icon_urls:
            return httpx.Response(200, content=b'webp-bytes', headers={'content-type': 'image/webp'})
        if url == voice_url:
            return httpx.Response(200, content=b'ogg-bytes', headers={'content-type': 'audio/ogg'})
        if url == model3_url:
            return httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'})
        if url == moc_url:
            return httpx.Response(200, content=b'moc-bytes', headers={'content-type': 'application/octet-stream'})
        if url == texture_url:
            return httpx.Response(200, content=b'webp-bytes', headers={'content-type': 'image/webp'})
        return httpx.Response(404)

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    root = tmp_path / 'javelin - Javelin'
    assert requested.count(f'GET {detail_url}') == 1
    assert fake_db.ship_details[1]['fingerprint']
    assert fake_db.ship_details[1]['payload']['ship']['shipGroupId'] == 1
    assert fake_db.models['azurlane:painting:javelin:javelin']['completed'] is True
    assert (root / 'assets/painting/javelin/javelin.webp').read_bytes() == b'painting-bytes'
    assert (root / 'assets/painting/javelin/paintingface/javelin/1.webp').read_bytes() == b'webp-bytes'
    assert (root / 'assets/painting/javelin/squareicon/javelin.webp').read_bytes() == b'webp-bytes'
    assert (root / 'assets/painting/javelin/cue/cv-1/detail.ogg').read_bytes() == b'ogg-bytes'
    assert (root / 'detail.json').exists()
    voice_rows = [row for row in fake_db.model_assets if row['kind'] == 'voice.audio']
    assert voice_rows[0]['context_json']['text'] == '嗯'
    assert voice_rows[0]['context_json']['voice_path'] == 'cue/cv-1/detail'


def _live2d_response(url: str) -> httpx.Response | None:
    """Whatever the live2d skin needs; these tests are about the painting beside it."""
    if not url.startswith(f'{L2D_SU_STATIC_BASE_URL}/live2d/'):
        return None
    if url.endswith('.model3.json'):
        return httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'})
    return httpx.Response(200, content=b'live2d-bytes', headers={'content-type': 'application/octet-stream'})


def _xiafei_layers() -> dict[str, dict[str, Any]]:
    return {
        'xiafei_4': {'size': [4574, 2866], 'pivot': [0.49, 0.66], 'position': [0, 0], 'rawSize': [2048, 1283], 'raw': True},
        'xiafei_4_rw': {'size': [2000, 2048], 'pivot': [0.49, 0.66], 'position': [10, 272], 'rawSize': [2000, 2048]},
    }


def test_azurlane_update_archives_painting_layers_and_meshes(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The packed sheet alone is not artwork: the index, the sibling layers and their meshes reassemble it."""
    fake_db = _install_fake_database(monkeypatch)
    index_url = f'{L2D_SU_STATIC_BASE_URL}/painting/xiafei_4.json'
    catalog = _ship_index_payload([_ship(1, 'xiafei', 'Richelieu', skins=[_skin(1, 'Skin', key='xiafei_4')])])
    _seed_ship_detail(fake_db, catalog)
    requested: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        requested.append(f'{request.method} {url}')
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        if url == index_url:
            payload = _painting_index_payload('xiafei_4', layers=_xiafei_layers())
            return httpx.Response(200, text=payload, headers={'content-type': 'application/json'})
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        live2d_response = _live2d_response(url)
        if live2d_response is not None:
            return live2d_response
        return httpx.Response(404)

    with httpx.Client(transport=httpx.MockTransport(handler)) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    root = tmp_path / 'xiafei - Richelieu'
    model = fake_db.models['azurlane:painting:xiafei:xiafei_4']
    assert model['completed'] is True
    assert (root / 'assets/painting/xiafei_4/xiafei_4.json').exists()
    assert (root / 'assets/painting/xiafei_4/xiafei_4.webp').read_bytes() == b'painting-bytes'
    assert (root / 'assets/painting/xiafei_4/painting/xiafei_4_rw.webp').read_bytes() == b'painting-bytes'
    assert (root / 'assets/painting/xiafei_4/painting/xiafei_4_rw-mesh.obj').read_bytes() == b'g mesh\nv 0 0 0\n'
    # xiafei_4 is `raw: true`, so it is stored unpacked and has no mesh to ask for.
    assert f'GET {L2D_SU_STATIC_BASE_URL}/painting/xiafei_4-mesh.obj' not in requested

    painting_index = model['source_metadata']['painting_index']
    assert painting_index['painting_key'] == 'xiafei_4'
    assert [layer['name'] for layer in painting_index['layers']] == ['xiafei_4', 'xiafei_4_rw']
    assert painting_index['layers'][0]['raw'] is True
    assert painting_index['layers'][1]['rawSize'] == [2000.0, 2048.0]
    assert painting_index['face']['size'] == [100.0, 100.0]

    manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    painting_model = next(item for item in manifest['models'] if item['type'] == 'painting')
    assert painting_model['source_metadata']['painting_index']['layers'][1]['position'] == [10.0, 272.0]
    assert painting_model['asset_counts']['painting.mesh'] == 1


def test_azurlane_update_rejects_a_mesh_answered_with_the_site_shell(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """A mesh the CDN does not host comes back as 200 text/html, which must never be stored as an .obj."""
    fake_db = _install_fake_database(monkeypatch)
    mesh_url = f'{L2D_SU_STATIC_BASE_URL}/painting/biaoqiang-mesh.obj'
    catalog = _ship_index_payload([_ship(1, 'biaoqiang', 'Javelin', skins=[_skin(1, 'Default', key='biaoqiang')])])
    _seed_ship_detail(fake_db, catalog)
    requested: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        requested.append(f'{request.method} {url}')
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        if url.endswith('-mesh.obj'):
            return httpx.Response(200, text='<!doctype html><html><body>l2d.su</body></html>', headers={'content-type': 'text/html'})
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        live2d_response = _live2d_response(url)
        if live2d_response is not None:
            return live2d_response
        return httpx.Response(404)

    with httpx.Client(transport=httpx.MockTransport(handler)) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    root = tmp_path / 'biaoqiang - Javelin'
    assert not (root / 'assets/painting/biaoqiang/painting/biaoqiang-mesh.obj').exists()
    assert fake_db.assets[mesh_url]['status'] == 'failed'
    assert 'Wavefront OBJ' in fake_db.assets[mesh_url]['last_error']
    # The answer is definite, so it is not retried -- and it must not read as a CDN block
    # either, or five missing meshes would pause the whole run.
    assert requested.count(f'GET {mesh_url}') == 1
    # A mesh is a companion file: losing it costs the reassembly, not the model.
    assert fake_db.models['azurlane:painting:biaoqiang:biaoqiang']['completed'] is True


def test_azurlane_update_keeps_a_stored_painting_index_when_the_cdn_copy_is_gone(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_db = _install_fake_database(monkeypatch)
    catalog = _ship_index_payload([_ship(1, 'biaoqiang', 'Javelin', skins=[_skin(1, 'Default', key='biaoqiang')])])
    _seed_ship_detail(fake_db, catalog)

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        live2d_response = _live2d_response(url)
        if live2d_response is not None:
            return live2d_response
        if url.startswith(f'{L2D_SU_STATIC_BASE_URL}/painting/') and url.endswith('.json'):
            return httpx.Response(404)
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        return httpx.Response(404)

    model_id = 'azurlane:painting:biaoqiang:biaoqiang'
    fake_db.models[model_id] = {
        'model_id': model_id,
        'model_type': 'painting',
        'character_key': 'biaoqiang',
        'costume_key': 'biaoqiang',
        'costume_id': 1,
        'primary_url': _painting_url('biaoqiang'),
        'source_metadata': {'painting_index': {'painting_key': 'biaoqiang', 'layers': [{'name': 'biaoqiang'}]}},
        'active': True,
        'completed': True,
    }

    with httpx.Client(transport=httpx.MockTransport(handler)) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    # The catalog refresh rewrites source_metadata every run; geometry it cannot refetch must survive.
    stored = fake_db.models[model_id]['source_metadata']
    assert stored['painting_index']['painting_key'] == 'biaoqiang'
    assert stored['id'] == model_id  # the rest of source_metadata is the refreshed catalog entry


def test_azurlane_update_uses_stored_detail_for_voices_without_origin_requests(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_db = _install_fake_database(monkeypatch)
    model3_url = _live2d_url('javelin')
    moc_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/javelin.moc3'
    texture_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp'
    voice_url = f'{L2D_SU_STATIC_BASE_URL}/cue/cv-1/detail.ogg'
    catalog = _ship_index_payload([_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin')])])
    _seed_ship_detail(
        fake_db,
        catalog,
        payload={'ship': {'shipGroupId': 1, 'skins': [{'id': 1, 'words': [{'key': 'detail', 'voicePath': 'cue/cv-1/detail'}]}]}},
    )
    requested: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:  # noqa: PLR0911
        url = str(request.url)
        requested.append(f'{request.method} {url}')
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        if url == voice_url:
            return httpx.Response(200, content=b'ogg-bytes', headers={'content-type': 'audio/ogg'})
        if url == model3_url:
            return httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'})
        if url == moc_url:
            return httpx.Response(200, content=b'moc-bytes', headers={'content-type': 'application/octet-stream'})
        if url == texture_url:
            return httpx.Response(200, content=b'webp-bytes', headers={'content-type': 'image/webp'})
        return httpx.Response(404)

    transport = httpx.MockTransport(handler)
    with httpx.Client(transport=transport) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    assert not [entry for entry in requested if '/data/ships/' in entry]
    assert (tmp_path / 'javelin - Javelin/assets/painting/javelin/cue/cv-1/detail.ogg').read_bytes() == b'ogg-bytes'


def test_azurlane_update_records_list_level_character_metadata(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    swimsuit = _skin(2, '泳装', key='javelin_2')
    swimsuit['shopTypeName'] = 'Swimsuits'
    ship = _ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin'), swimsuit])
    ship['defaultSkinId'] = 1
    catalog = _ship_index_payload([ship])
    _seed_ship_detail(fake_db, catalog, payload={'ship': {'shipGroupId': 1, 'className': 'J Class', 'skins': []}})

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        if url.endswith('.model3.json'):
            return httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'})
        return httpx.Response(200, content=b'bytes', headers={'content-type': 'application/octet-stream'})

    with httpx.Client(transport=httpx.MockTransport(handler)) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    metadata = fake_db.characters['javelin']['source_metadata']
    assert metadata['class_name'] == 'J Class'  # detail-only field, recovered from the stored payload
    assert metadata['default_skin_id'] == 1
    assert metadata['skin_series'] == ['Skin', 'Swimsuits']


def _origin_split_handlers(
    catalog: str,
    *,
    detail_payload: str,
    origin_urls: list[str],
    direct_urls: list[str],
    origin_failures: int = 0,
) -> tuple[Any, Any]:
    state = {'failures': origin_failures}

    def origin_handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        origin_urls.append(url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if '/data/ships/' in url:
            if state['failures'] > 0:
                state['failures'] -= 1
                return httpx.Response(503)
            return httpx.Response(200, text=detail_payload, headers={'content-type': 'application/json'})
        return httpx.Response(404)

    def direct_handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        direct_urls.append(url)
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        if url.endswith('.model3.json'):
            return httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'})
        return httpx.Response(200, content=b'bytes', headers={'content-type': 'application/octet-stream'})

    return origin_handler, direct_handler


def _run_with_origin_split(tmp_path: Path, origin_handler: Any, direct_handler: Any, **kwargs: Any) -> None:
    with (
        httpx.Client(transport=httpx.MockTransport(direct_handler)) as source_client,
        httpx.Client(transport=httpx.MockTransport(origin_handler)) as origin_source_client,
    ):
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(direct_handler))
        origin_client = httpx.AsyncClient(transport=httpx.MockTransport(origin_handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                origin_client=origin_client,
                origin_source_client=origin_source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
                **kwargs,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())
            asyncio.run(origin_client.aclose())


def test_azurlane_sends_only_l2d_su_origin_traffic_through_the_origin_client(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _install_fake_database(monkeypatch)
    catalog = _ship_index_payload([_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin')])])
    detail_payload = json.dumps({'ship': {'shipGroupId': 1, 'className': 'J Class', 'skins': []}})
    origin_urls: list[str] = []
    direct_urls: list[str] = []
    origin_handler, direct_handler = _origin_split_handlers(
        catalog,
        detail_payload=detail_payload,
        origin_urls=origin_urls,
        direct_urls=direct_urls,
    )

    _run_with_origin_split(tmp_path, origin_handler, direct_handler)

    # The origin client carries l2d.su and nothing else: routing CDN assets through a metered
    # proxy would multiply its bandwidth by orders of magnitude.
    assert origin_urls, 'expected the origin client to be used'
    assert all(url.startswith('https://l2d.su/') for url in origin_urls)
    assert not [url for url in direct_urls if url.startswith('https://l2d.su/')]
    assert _PRIMARY_INDEX_URL in origin_urls
    assert NAGAMI_MAPPING_URL in direct_urls
    assert [url for url in direct_urls if url.startswith(L2D_SU_STATIC_BASE_URL)]


def test_azurlane_retries_ship_detail_on_another_proxy_exit(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    catalog = _ship_index_payload([_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin')])])
    detail_payload = json.dumps({'ship': {'shipGroupId': 1, 'className': 'J Class', 'skins': []}})
    origin_urls: list[str] = []
    direct_urls: list[str] = []
    origin_handler, direct_handler = _origin_split_handlers(
        catalog,
        detail_payload=detail_payload,
        origin_urls=origin_urls,
        direct_urls=direct_urls,
        origin_failures=2,
    )

    _run_with_origin_split(tmp_path, origin_handler, direct_handler)

    # Two exits refused before a third served the payload; the detail still lands.
    expected_attempts = 3
    detail_requests = [url for url in origin_urls if '/data/ships/' in url]
    assert len(detail_requests) == expected_attempts
    assert fake_db.ship_details[1]['payload']['ship']['className'] == 'J Class'


def test_azurlane_gives_up_on_ship_detail_after_the_attempt_budget(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    catalog = _ship_index_payload([_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin')])])
    origin_urls: list[str] = []
    direct_urls: list[str] = []
    origin_handler, direct_handler = _origin_split_handlers(
        catalog,
        detail_payload='',
        origin_urls=origin_urls,
        direct_urls=direct_urls,
        origin_failures=99,
    )

    _run_with_origin_split(tmp_path, origin_handler, direct_handler, origin_attempts=2)

    # A ship whose detail never arrives must not block the rest of the run.
    configured_attempts = 2
    assert len([url for url in origin_urls if '/data/ships/' in url]) == configured_attempts
    assert fake_db.ship_details == {}
    assert fake_db.models['azurlane:painting:javelin:javelin']['completed'] is True


def test_azurlane_asset_client_fails_fast_on_a_stalled_connection(tmp_path: Path) -> None:
    """A stalled CDN connection must not hold the run hostage.

    Models are archived one at a time, so a single asset waiting out the read timeout idles every
    other concurrency slot. httpx applies this timeout per chunk, so a large file that is still
    arriving slowly is never cut off -- only a connection that stopped delivering.
    """
    crawler = AzurLane(path=tmp_path)

    async def check() -> None:
        async with crawler._http_client() as client:
            assert client.timeout.read == azurlane_module._ASSET_READ_TIMEOUT_SECONDS
            assert client.timeout.read < 60  # noqa: PLR2004
            # A handshake that never completes is the other half of the same stall, and it
            # outlived the read timeout because only the read side had been bounded.
            assert client.timeout.connect == azurlane_module._ASSET_CONNECT_TIMEOUT_SECONDS
            assert client.timeout.connect < 20  # noqa: PLR2004

    asyncio.run(check())


def test_azurlane_origin_client_disables_connection_reuse(tmp_path: Path) -> None:
    crawler = AzurLane(path=tmp_path, origin_proxy='http://user:pass@proxy.example:8080')

    async def check() -> None:
        async with crawler._http_client() as direct, crawler._origin_http_client(direct) as origin:
            assert origin is not direct
            # Reaching into httpx internals because keep-alive has no public accessor, and the
            # invariant matters: an HTTPS request through a proxy lives in a CONNECT tunnel that
            # is pinned to one exit IP, so a reused connection would push the whole backfill
            # through a single address and leave retries stuck on the same failing exit.
            assert origin._transport._pool._max_keepalive_connections == 0
            assert direct._transport._pool._max_keepalive_connections > 0

    asyncio.run(check())


def test_azurlane_origin_client_is_the_direct_client_when_no_proxy_is_configured(tmp_path: Path) -> None:
    crawler = AzurLane(path=tmp_path, origin_proxy='')

    async def check() -> None:
        async with crawler._http_client() as direct, crawler._origin_http_client(direct) as origin:
            assert origin is direct

    asyncio.run(check())


def test_azurlane_reads_the_origin_request_interval_from_settings(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    configured_interval = 7.5
    config = settings.Settings()
    config.web.azurlane.origin_request_interval_seconds = configured_interval
    monkeypatch.setattr(azurlane_module.settings, 'load', lambda: config)

    crawler = AzurLane(path=tmp_path)

    # One limiter shared by every origin request: the pacing is global, not per exit IP.
    assert crawler._origin_limiter._min_interval_seconds == configured_interval
    assert crawler._origin_source_limiter._min_interval_seconds == configured_interval


def test_azurlane_index_requests_pass_through_the_origin_throttle(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    catalog = _ship_index_payload([_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin')])])
    _seed_ship_detail(fake_db, catalog)
    throttle_calls = 0

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(200, text=_nagami_mapping_payload())
        painting_response = _painting_response(url)
        if painting_response is not None:
            return painting_response
        if url == _live2d_url('javelin'):
            return httpx.Response(200, text=_live2d_model3_payload(), headers={'content-type': 'application/json'})
        return httpx.Response(200, content=b'bytes', headers={'content-type': 'application/octet-stream'})

    with httpx.Client(transport=httpx.MockTransport(handler)) as source_client:
        async_client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
        try:
            crawler = AzurLane(
                path=tmp_path,
                client=async_client,
                source_client=source_client,
                api_request_interval_seconds=0,
                cdn_request_interval_seconds=0,
                origin_request_interval_seconds=0,
                asset_process_concurrency=1,
            )
            original = crawler._origin_source_limiter.wait

            def counting_wait() -> None:
                nonlocal throttle_calls
                throttle_calls += 1
                original()

            crawler._origin_source_limiter.wait = counting_wait  # type: ignore[method-assign]
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    # One throttle acquisition per l2d.su origin index request (CN + EN); nagami (CDN) is not throttled.
    expected_index_requests = 2
    assert throttle_calls == expected_index_requests


def test_source_rate_limiter_spaces_successive_requests() -> None:
    interval = 0.05
    limiter = azurlane_module._SourceRateLimiter(interval, jitter_seconds=0.0)
    start = time.monotonic()
    limiter.wait()  # first call returns immediately
    limiter.wait()  # second call waits one interval
    assert time.monotonic() - start >= interval


def test_azurlane_update_fails_loudly_when_the_catalog_comes_back_empty(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Regression: an empty catalog used to return early, hiding which source had failed.

    That is the branch this crawler sat in for two weeks -- nagami's hash-versioned bundle URL
    had died, emptying the catalog, which returned before the l2d.su error was ever logged.
    """
    _install_fake_database(monkeypatch)
    empty_catalog = _ship_index_payload([])

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        index_response = _source_index_response(empty_catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            return httpx.Response(404)
        pytest.fail(reason='no asset download should run without a catalog')

    with httpx.Client(transport=httpx.MockTransport(handler)) as source_client:
        crawler = AzurLane(
            path=tmp_path,
            source_client=source_client,
            api_request_interval_seconds=0,
            cdn_request_interval_seconds=0,
            origin_request_interval_seconds=0,
            asset_process_concurrency=1,
        )
        with pytest.raises(azurlane_module.CrawlRunError, match='no model entries') as failure:
            asyncio.run(crawler.update())

    assert NAGAMI_MAPPING_URL in str(failure.value)


def test_azurlane_update_preserves_catalog_state_when_source_snapshots_incomplete(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_db = _install_fake_database(monkeypatch)
    model_id = 'azurlane:live2d:javelin:javelin'
    primary_url = _live2d_url('javelin')
    fallback_url = 'https://cdn.nagami.moe/live2d/javelin/javelin.model3.json'
    source_metadata = {
        'id': model_id,
        'source': 'merged',
        'resources': {
            'primary_url': primary_url,
            'fallback_url': fallback_url,
        },
    }
    fake_db.characters['javelin'] = {
        'character_key': 'javelin',
        'active': True,
        'source_metadata': {'model_ids': [model_id], 'sources': ['merged']},
    }
    fake_db.models[model_id] = {
        'model_id': model_id,
        'source': 'merged',
        'primary_url': primary_url,
        'fallback_url': fallback_url,
        'source_metadata': source_metadata,
        'active': True,
        'completed': True,
    }
    catalog = _ship_index_payload([_ship(1, 'javelin', 'Javelin', skins=[_skin(1, 'Default', key='javelin')])])
    source_urls: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        source_urls.append(url)
        index_response = _source_index_response(catalog, url=url)
        if index_response is not None:
            return index_response
        if url == NAGAMI_MAPPING_URL:
            message = 'nagami unavailable'
            raise httpx.ConnectError(message, request=request)
        pytest.fail(reason='asset downloads should not run while source snapshots are incomplete')

    with httpx.Client(transport=httpx.MockTransport(handler)) as source_client:
        crawler = AzurLane(
            path=tmp_path,
            source_client=source_client,
            api_request_interval_seconds=0,
            cdn_request_interval_seconds=0,
            origin_request_interval_seconds=0,
            asset_process_concurrency=1,
        )
        # The catalog is preserved, but the run reports failure: a source that fails quietly
        # is how this crawler stayed broken for a month with every run recorded as completed.
        with pytest.raises(azurlane_module.CrawlRunError, match='preserving existing catalog state') as failure:
            asyncio.run(crawler.update())

    # The message names the source that actually failed, so the log says what to fix.
    assert NAGAMI_MAPPING_URL in str(failure.value)
    assert source_urls == [_PRIMARY_INDEX_URL, _ENGLISH_INDEX_URL, NAGAMI_MAPPING_URL]
    assert fake_db.schema_created is True
    assert fake_db.characters['javelin']['active'] is True
    assert fake_db.models[model_id]['active'] is True
    assert fake_db.models[model_id]['source'] == 'merged'
    assert fake_db.models[model_id]['fallback_url'] == fallback_url
    assert fake_db.models[model_id]['source_metadata'] == source_metadata
    assert all(not sql.startswith('UPDATE azurlane_characters SET active = FALSE') for sql in fake_db.queries)
    assert all(not sql.startswith('UPDATE azurlane_models SET active = FALSE') for sql in fake_db.queries)


def test_azurlane_backend_manifests_are_deterministically_ordered(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    manifest_path = tmp_path / 'javelin - Javelin/manifest.json'
    live2d_model_id = 'azurlane:live2d:javelin:javelin'
    spine_model_id = 'azurlane:spine:javelin:javelin_spine'
    model3_url = _live2d_url('javelin')
    moc_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/javelin.moc3'
    texture_url = f'{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp'

    fake_db.characters['javelin'] = {
        'character_key': 'javelin',
        'source_id': 1,
        'name_zh': '标枪',
        'name_en': 'Javelin',
        'manifest_path': manifest_path.as_posix(),
        'source_metadata': {'sources': ['l2d.su'], 'model_ids': [spine_model_id, live2d_model_id]},
        'active': True,
    }
    fake_db.models[spine_model_id] = {
        'model_id': spine_model_id,
        'model_type': 'spine',
        'source': 'l2d.su',
        'character_key': 'javelin',
        'costume_key': 'javelin_spine',
        'costume_id': 2,
        'costume_name_zh': '动态',
        'costume_name_en': 'Dynamic',
        'primary_url': _spine_url('javelin_spine'),
        'fallback_url': '',
        'availability_state': 'unchecked',
        'source_metadata': {'id': spine_model_id},
        'active': True,
    }
    fake_db.models[live2d_model_id] = {
        'model_id': live2d_model_id,
        'model_type': 'live2d',
        'source': 'l2d.su',
        'character_key': 'javelin',
        'costume_key': 'javelin',
        'costume_id': 1,
        'costume_name_zh': '默认',
        'costume_name_en': 'Default',
        'primary_url': model3_url,
        'fallback_url': '',
        'availability_state': 'unchecked',
        'source_metadata': {'id': live2d_model_id},
        'active': True,
        'completed': True,
    }
    for url, kind, local_path, content in (
        (texture_url, 'live2d.texture', 'assets/live2d/javelin/textures/texture_00.webp', b'texture'),
        (moc_url, 'live2d.moc3', 'assets/live2d/javelin/javelin.moc3', b'moc'),
        (model3_url, 'live2d.model3', 'assets/live2d/javelin/javelin.model3.json', b'model3'),
    ):
        sha256 = hashlib.sha256(content).hexdigest()
        fake_db.assets[url] = {
            'url': url,
            'normalized_url': url,
            'kind': kind,
            'status': 'downloaded',
            'sha256': sha256,
            'size': len(content),
            'content_type': 'application/octet-stream',
            'downloaded_url': url,
            'failed_count': 0,
            'last_error': '',
        }
        fake_db.model_assets.insert(
            0,
            {
                'model_id': live2d_model_id,
                'url': url,
                'kind': kind,
                'context_hash': f'hash-{kind}',
                'local_path': local_path,
                'original_filename': Path(local_path).name,
                'fallback_url': '',
                'context_json': {'kind': kind, 'model_id': live2d_model_id},
            },
        )

    crawler = AzurLane(path=tmp_path, api_request_interval_seconds=0, cdn_request_interval_seconds=0, asset_process_concurrency=1)

    asyncio.run(crawler._write_backend_manifests())
    first_manifest = manifest_path.read_text(encoding='utf-8')
    asyncio.run(crawler._write_backend_manifests())
    second_manifest = manifest_path.read_text(encoding='utf-8')

    assert second_manifest == first_manifest
    payload = json.loads(first_manifest)
    assert [model['model_id'] for model in payload['models']] == [live2d_model_id, spine_model_id]
    assert [asset['kind'] for asset in payload['models'][0]['assets']] == ['live2d.model3', 'live2d.moc3', 'live2d.texture']
    assert [asset['local_path'] for asset in payload['models'][0]['assets']] == [
        'assets/live2d/javelin/javelin.model3.json',
        'assets/live2d/javelin/javelin.moc3',
        'assets/live2d/javelin/textures/texture_00.webp',
    ]
