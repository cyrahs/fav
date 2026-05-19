# ruff: noqa: INP001, S101

from __future__ import annotations

import asyncio
import hashlib
import json
from contextlib import asynccontextmanager
from pathlib import Path
from types import SimpleNamespace
from typing import TYPE_CHECKING, Any

import httpx
import pytest

import src.service.jobs as jobs_module
import src.web.azurlane as azurlane_module
from src.api.schemas import JobRequestTarget
from src.core.config import AzurLane as AzurLaneConfig
from src.tool.azurlane_l2d_sources import L2D_SU_CATALOG_URL, NAGAMI_MAPPING_BUNDLE_URL
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
        self.queries: list[str] = []

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
                'source_metadata': json.loads(str(source_metadata)),
                'active': True,
                'completed': False,
            }
            return []
        if sql.startswith('UPDATE azurlane_models SET fetched_at'):
            self.models[str(params[0])]['completed'] = False
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


def _job_cfg(*, enabled: bool = True) -> SimpleNamespace:
    return SimpleNamespace(cron='0 */6 * * *', enabled=enabled, run_on_start=False)


def _catalog_payload(characters: list[dict[str, object]]) -> str:
    return json.dumps(
        {
            'Master': [
                {
                    'gameId': 1,
                    'gameName': 'Azur Lane',
                    'character': characters,
                },
            ],
        },
    )


def _nagami_bundle(mapping: dict[str, str] | None = None) -> str:
    return f'const e=JSON.parse(`{json.dumps(mapping or {})}`);export{{e as default}};'


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
    assert cfg.run_on_start is False
    assert cfg.path == Path('./collection/azurlane')


def test_scheduler_registration_includes_azurlane(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_config = SimpleNamespace(
        web=SimpleNamespace(
            azurlane=_job_cfg(enabled=False),
            bd2=_job_cfg(),
            bilibili=_job_cfg(),
            hanime1=_job_cfg(),
            jandan=_job_cfg(),
            nikke=_job_cfg(),
            stellasora=_job_cfg(),
            telegram=_job_cfg(),
        ),
    )
    monkeypatch.setattr(jobs_module, 'config', fake_config)

    jobs = jobs_module.build_jobs()
    azurlane_job = next(job for job in jobs if job.key == 'azurlane')

    assert azurlane_job.name == 'Azur Lane'
    assert azurlane_job.enabled is False
    assert azurlane_job.required_commands == ()
    assert azurlane_job.factory is jobs_module.AzurLane


def test_api_job_enum_includes_azurlane() -> None:
    assert JobRequestTarget.AZURLANE.value == 'azurlane'


def test_azurlane_update_downloads_live2d_assets(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    model3_url = 'https://static.example/live2d/azurlane/javelin/javelin.model3.json'
    moc_url = 'https://static.example/live2d/azurlane/javelin/javelin.moc3'
    texture_url = 'https://static.example/live2d/azurlane/javelin/textures/texture_00.webp'
    catalog = _catalog_payload(
        [
            {
                'charId': 1,
                'charKey': 'javelin',
                'charName': 'Javelin',
                'charNameEn': 'Javelin',
                'live2d': [
                    {
                        'costumeId': 1,
                        'costumeName': 'Default',
                        'costumeNameEn': 'Default',
                        'path': model3_url,
                    },
                ],
                'spine': [],
            },
        ],
    )

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        if url == L2D_SU_CATALOG_URL:
            return httpx.Response(200, text=catalog)
        if url == NAGAMI_MAPPING_BUNDLE_URL:
            return httpx.Response(200, text=_nagami_bundle())
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
                asset_process_concurrency=1,
            )
            asyncio.run(crawler.update())
        finally:
            asyncio.run(async_client.aclose())

    root = tmp_path / 'javelin - Javelin'
    assert fake_db.schema_created is True
    assert fake_db.characters['javelin']['active'] is True
    assert fake_db.models['azurlane:live2d:javelin:javelin']['completed'] is True
    assert (root / 'assets/live2d/javelin/javelin.model3.json').exists()
    assert (root / 'assets/live2d/javelin/javelin.moc3').read_bytes() == b'moc-bytes'
    assert (root / 'assets/live2d/javelin/textures/texture_00.webp').read_bytes() == b'webp-bytes'
    assert {row['kind'] for row in fake_db.model_assets} == {'live2d.model3', 'live2d.moc3', 'live2d.texture'}
    assert fake_db.assets[texture_url]['status'] == 'downloaded'


def test_azurlane_update_downloads_spine_assets(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    spine_url = 'https://static.example/live2d/azurlane/iris_2'
    skel_url = 'https://static.example/live2d/azurlane/iris_2/iris_2.skel'
    atlas_url = 'https://static.example/live2d/azurlane/iris_2/iris_2.atlas'
    texture_url = 'https://static.example/live2d/azurlane/iris_2/iris_2.webp'
    atlas_text = '\niris_2.webp\nsize: 4096,4096\nformat: RGBA8888\n'
    catalog = _catalog_payload(
        [
            {
                'charId': 2,
                'charKey': 'iris',
                'charName': 'Iris',
                'charNameEn': 'Iris',
                'live2d': [],
                'spine': [
                    {
                        'costumeId': 2,
                        'costumeName': 'Afternoon',
                        'costumeNameEn': 'Afternoon',
                        'path': spine_url,
                    },
                ],
            },
        ],
    )

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        if url == L2D_SU_CATALOG_URL:
            return httpx.Response(200, text=catalog)
        if url == NAGAMI_MAPPING_BUNDLE_URL:
            return httpx.Response(200, text=_nagami_bundle())
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
    assert {row['kind'] for row in fake_db.model_assets} == {'spine.skel', 'spine.atlas', 'spine.texture'}


def test_azurlane_update_reuses_completed_blob_for_archived_url(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    fake_db = _install_fake_database(monkeypatch)
    model3_url = 'https://static.example/live2d/azurlane/javelin/javelin.model3.json'
    moc_url = 'https://static.example/live2d/azurlane/javelin/javelin.moc3'
    texture_url = 'https://static.example/live2d/azurlane/javelin/textures/texture_00.webp'
    reused_texture = b'already-archived-texture'
    _seed_downloaded_blob(
        fake_db=fake_db,
        root=tmp_path,
        url=texture_url,
        kind='live2d.texture',
        content=reused_texture,
        content_type='image/webp',
    )
    catalog = _catalog_payload(
        [
            {
                'charId': 1,
                'charKey': 'javelin',
                'charName': 'Javelin',
                'charNameEn': 'Javelin',
                'live2d': [
                    {
                        'costumeId': 1,
                        'costumeName': 'Default',
                        'costumeNameEn': 'Default',
                        'path': model3_url,
                    },
                ],
                'spine': [],
            },
        ],
    )
    seen_asset_urls: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        if url == L2D_SU_CATALOG_URL:
            return httpx.Response(200, text=catalog)
        if url == NAGAMI_MAPPING_BUNDLE_URL:
            return httpx.Response(200, text=_nagami_bundle())
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
    model3_url = 'https://static.example/live2d/azurlane/javelin/javelin.model3.json'
    moc_url = 'https://static.example/live2d/azurlane/javelin/javelin.moc3'
    texture_url = 'https://static.example/live2d/azurlane/javelin/textures/texture_00.webp'
    catalog = _catalog_payload(
        [
            {
                'charId': 1,
                'charKey': 'javelin',
                'charName': 'Javelin',
                'charNameEn': 'Javelin',
                'live2d': [
                    {
                        'costumeId': 1,
                        'costumeName': 'Default',
                        'costumeNameEn': 'Default',
                        'path': model3_url,
                    },
                ],
                'spine': [],
            },
        ],
    )

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        if url == L2D_SU_CATALOG_URL:
            return httpx.Response(200, text=catalog)
        if url == NAGAMI_MAPPING_BUNDLE_URL:
            return httpx.Response(200, text=_nagami_bundle())
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


def test_azurlane_update_preserves_catalog_state_when_source_snapshots_incomplete(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    fake_db = _install_fake_database(monkeypatch)
    model_id = 'azurlane:live2d:javelin:javelin'
    primary_url = 'https://static.example/live2d/azurlane/javelin/javelin.model3.json'
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
    catalog = _catalog_payload(
        [
            {
                'charId': 1,
                'charKey': 'javelin',
                'charName': 'Javelin',
                'charNameEn': 'Javelin',
                'live2d': [
                    {
                        'costumeId': 1,
                        'costumeName': 'Default',
                        'costumeNameEn': 'Default',
                        'path': primary_url,
                    },
                ],
                'spine': [],
            },
        ],
    )
    source_urls: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        source_urls.append(url)
        if url == L2D_SU_CATALOG_URL:
            return httpx.Response(200, text=catalog)
        if url == NAGAMI_MAPPING_BUNDLE_URL:
            message = 'nagami unavailable'
            raise httpx.ConnectError(message, request=request)
        pytest.fail(reason='asset downloads should not run while source snapshots are incomplete')

    with httpx.Client(transport=httpx.MockTransport(handler)) as source_client:
        crawler = AzurLane(
            path=tmp_path,
            source_client=source_client,
            api_request_interval_seconds=0,
            cdn_request_interval_seconds=0,
            asset_process_concurrency=1,
        )
        asyncio.run(crawler.update())

    assert source_urls == [L2D_SU_CATALOG_URL, NAGAMI_MAPPING_BUNDLE_URL]
    assert fake_db.schema_created is True
    assert fake_db.characters['javelin']['active'] is True
    assert fake_db.models[model_id]['active'] is True
    assert fake_db.models[model_id]['source'] == 'merged'
    assert fake_db.models[model_id]['fallback_url'] == fallback_url
    assert fake_db.models[model_id]['source_metadata'] == source_metadata
    assert all(not sql.startswith('UPDATE azurlane_characters SET active = FALSE') for sql in fake_db.queries)
    assert all(not sql.startswith('UPDATE azurlane_models SET active = FALSE') for sql in fake_db.queries)
