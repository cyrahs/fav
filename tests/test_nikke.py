# ruff: noqa: INP001, S101, SLF001

import asyncio
import hashlib
from pathlib import Path
from types import SimpleNamespace

import pytest

import src.service.jobs as jobs_module
import src.web.nikke as nikke_module
from src.api.schemas import JobRequestTarget
from src.core.config import Nikke as NikkeConfig
from src.web.nikke import (
    Asset,
    AssetProcessingError,
    Nikke,
    add_asset,
    assign_asset_paths,
    extract_resources,
    filter_nikke_rows,
    materialize_blob,
    parse_spine_atlas_textures,
    validate_asset_response,
)


def _job_cfg(*, enabled: bool = True) -> SimpleNamespace:
    return SimpleNamespace(cron='0 */6 * * *', enabled=enabled, run_on_start=False)


def test_nikke_config_defaults_to_disabled_collection_nikke_path() -> None:
    cfg = NikkeConfig()

    assert cfg.enabled is False
    assert cfg.cron == '0 */6 * * *'
    assert cfg.run_on_start is False
    assert cfg.path == Path('./collection/nikke')


def test_scheduler_registration_includes_nikke(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_config = SimpleNamespace(
        web=SimpleNamespace(
            bd2=_job_cfg(),
            bilibili=_job_cfg(),
            hanime1=_job_cfg(),
            jandan=_job_cfg(),
            nikke=_job_cfg(enabled=False),
            stellasora=_job_cfg(),
            telegram=_job_cfg(),
        ),
    )
    monkeypatch.setattr(jobs_module, 'config', fake_config)

    jobs = jobs_module.build_jobs()
    nikke_job = next(job for job in jobs if job.key == 'nikke')

    assert nikke_job.name == 'Nikke'
    assert nikke_job.enabled is False
    assert nikke_job.required_commands == ()
    assert nikke_job.factory is jobs_module.Nikke


def test_api_job_enum_includes_nikke() -> None:
    assert JobRequestTarget.NIKKE.value == 'nikke'


def test_filter_nikke_rows_keeps_only_wrapped_rows_with_content_id() -> None:
    rows = [
        {'nikke': {'content_id': '691899', 'title': '海伦'}},
        {'stellasora': {'content_id': 1}},
        {'nikke': {'content_id': 'not-an-id', 'title': 'bad'}},
        {'nikke': {'id': 171728, 'title': '薇尔维特'}},
    ]

    filtered = filter_nikke_rows(rows)

    assert [row['title'] for row in filtered] == ['海伦', '薇尔维特']


def test_extract_resources_uses_bound_media_gate() -> None:
    content_json = {
        'baseData': [
            [
                {'type': 'text', 'value': '头像'},
                {'type': 'image', 'key': 'bound_image', 'value': '/media/bound.png'},
                {'type': 'image', 'key': 'unbound_image', 'value': '/media/unbound.png'},
            ],
            [
                {'type': 'text', 'value': '视频'},
                {'type': 'video', 'key': 'bound_video', 'value': 'https://cdn.example.com/movie.mp4'},
            ],
            [
                {'type': 'text', 'value': '模型'},
                {
                    'type': 'live2d',
                    'key': 'bound_model',
                    'value': {
                        'live2dKey': 'model-a',
                        'atlas': '/spine/model.atlas',
                        'skel': '/spine/model.skel',
                        'image': '/spine/model.png',
                    },
                },
            ],
        ],
    }
    reverse_bind = {
        'bound_image': 'stable-image',
        'bound_video': 'stable-video',
        'bound_model': 'stable-model',
    }

    assets, models = extract_resources(
        content_json=content_json,
        tj_list_row={'icon': '/icons/face.png'},
        reverse_bind=reverse_bind,
    )

    asset_urls = {asset.url for asset in assets.values()}
    assert 'https://www.gamekee.com/media/bound.png' in asset_urls
    assert 'https://cdn.example.com/movie.mp4' in asset_urls
    assert 'https://www.gamekee.com/spine/model.atlas' in asset_urls
    assert 'https://www.gamekee.com/spine/model.skel' in asset_urls
    assert 'https://www.gamekee.com/spine/model.png' in asset_urls
    assert 'https://www.gamekee.com/icons/face.png' in asset_urls
    assert 'https://www.gamekee.com/media/unbound.png' not in asset_urls
    assert models[0]['live2d_key'] == 'model-a'


def test_add_asset_preserves_same_url_different_kinds() -> None:
    assets: dict[tuple[str, str], Asset] = {}

    add_asset(assets, 'https://cdn.example.com/shared.png', 'image', {'field': 'icon'})
    add_asset(assets, 'https://cdn.example.com/shared.png', 'live2d_texture', {'live2d_field': 'image'})

    assert set(assets) == {
        ('https://cdn.example.com/shared.png', 'image'),
        ('https://cdn.example.com/shared.png', 'live2d_texture'),
    }
    assert sorted(asset.kind for asset in assets.values()) == ['image', 'live2d_texture']


def test_parse_spine_atlas_textures_dedupes_texture_lines() -> None:
    atlas_text = """
\ufeffmodel.png
size: 1024,1024
format: RGBA8888
region
  rotate: false
effect.webp
model.png
"""

    assert parse_spine_atlas_textures(atlas_text) == ['model.png', 'effect.webp']


def test_validate_asset_response_rejects_empty_and_html() -> None:
    assert validate_asset_response(kind='image', content_type='image/png', body_prefix=b'', size=0) == 'empty response body'
    assert (
        validate_asset_response(
            kind='image',
            content_type='text/html; charset=utf-8',
            body_prefix=b'<!doctype html><html>Forbidden</html>',
            size=42,
        )
        == 'cdn rejection response'
    )


def test_assign_asset_paths_splits_live2d_dir_on_same_basename_collision() -> None:
    assets = {
        'https://cdn.example.com/a/model.atlas': Asset(
            url='https://cdn.example.com/a/model.atlas',
            kind='live2d_atlas',
            contexts=[{'live2d_key': 'same-model'}],
        ),
        'https://cdn.example.com/b/model.atlas': Asset(
            url='https://cdn.example.com/b/model.atlas',
            kind='live2d_atlas',
            contexts=[{'live2d_key': 'same-model'}],
        ),
    }

    assign_asset_paths(assets, content_id=1)
    paths = [asset.local_path for asset in assets.values()]

    assert paths[0] == 'assets/live2d/same-model/model.atlas'
    assert paths[1].startswith('assets/live2d/same-model-')
    assert paths[1].endswith('/model.atlas')


def test_materialize_blob_falls_back_to_copy_when_hardlink_fails(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    blob = tmp_path / 'blob'
    data = b'nikke-asset'
    blob.write_bytes(data)
    destination = tmp_path / 'character/assets/images/file.bin'

    def _fail_link(_src: Path, _dst: Path) -> None:
        raise OSError

    monkeypatch.setattr(nikke_module.os, 'link', _fail_link)

    materialize_blob(
        blob_path=blob,
        destination=destination,
        sha256=hashlib.sha256(data).hexdigest(),
        size=len(data),
    )

    assert destination.read_bytes() == data


def test_register_temp_blob_reuses_existing_hash_blob(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    data = b'same-content'
    sha256 = hashlib.sha256(data).hexdigest()
    existing_relative = Path('_blobs/sha256') / sha256[:2] / sha256
    existing = tmp_path / existing_relative
    existing.parent.mkdir(parents=True)
    existing.write_bytes(data)
    temp = tmp_path / 'download.tmp'
    temp.write_bytes(data)
    queries: list[str] = []

    async def _fake_query_db(sql: str, _params: tuple = ()) -> list[dict[str, object]]:
        queries.append(sql)
        if 'SELECT blob_path' in sql:
            return [{'blob_path': existing_relative.as_posix()}]
        return []

    monkeypatch.setattr(nikke_module.database, 'query_db', _fake_query_db)
    crawler = Nikke(path=tmp_path)

    blob = asyncio.run(
        crawler._register_temp_blob(
            nikke_module.TempBlob(
                path=temp,
                sha256=sha256,
                size=len(data),
                content_type='image/png',
            ),
        ),
    )

    assert blob.path == existing
    assert not temp.exists()
    assert any('last_verified_at' in sql for sql in queries)


def test_completed_blob_for_url_marks_missing_when_db_blob_is_gone(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    updates: list[tuple[str, tuple]] = []

    async def _fake_query_db(sql: str, params: tuple = ()) -> list[dict[str, object]]:
        if 'FROM nikke_assets AS a' in sql:
            return [
                {
                    'sha256': 'a' * 64,
                    'size': 10,
                    'content_type': 'image/png',
                    'blob_path': '_blobs/sha256/aa/' + ('a' * 64),
                },
            ]
        updates.append((sql, params))
        return []

    monkeypatch.setattr(nikke_module.database, 'query_db', _fake_query_db)
    crawler = Nikke(path=tmp_path)

    blob = asyncio.run(crawler._completed_blob_for_url('https://cdn.example.com/missing.png'))

    assert blob is None
    assert any("status = 'missing'" in sql for sql, _params in updates)


def test_process_assets_raises_when_any_asset_failed(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    crawler = Nikke(path=tmp_path)
    assets = {
        ('https://cdn.example.com/a.png', 'image'): Asset(url='https://cdn.example.com/a.png', kind='image'),
        ('https://cdn.example.com/b.png', 'image'): Asset(url='https://cdn.example.com/b.png', kind='image'),
    }

    async def _fake_process_asset(*, client: object, root: Path, asset: Asset) -> bool:
        _ = client, root
        if asset.url.endswith('b.png'):
            asset.status = 'failed'
            asset.error = 'boom'
            return False
        asset.status = 'downloaded'
        return True

    monkeypatch.setattr(crawler, '_process_asset', _fake_process_asset)

    with pytest.raises(AssetProcessingError, match='1 Nikke assets failed'):
        asyncio.run(crawler._process_assets(client=object(), root=tmp_path, assets=assets))


def test_process_assets_limits_db_backed_asset_processing(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    crawler = Nikke(path=tmp_path)
    assets = {
        (f'https://cdn.example.com/{index}.png', 'image'): Asset(url=f'https://cdn.example.com/{index}.png', kind='image')
        for index in range(12)
    }
    active = 0
    max_active = 0

    async def _fake_process_asset(*, client: object, root: Path, asset: Asset) -> bool:
        nonlocal active, max_active
        _ = client, root
        active += 1
        max_active = max(max_active, active)
        await asyncio.sleep(0.01)
        asset.status = 'downloaded'
        active -= 1
        return True

    monkeypatch.setattr(crawler, '_process_asset', _fake_process_asset)

    asyncio.run(crawler._process_assets(client=object(), root=tmp_path, assets=assets))

    assert max_active == nikke_module._ASSET_PROCESS_CONCURRENCY


def test_upsert_page_fetch_start_clears_completed_at(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    captured: list[str] = []

    async def _fake_query_db(sql: str, _params: tuple = ()) -> list[dict[str, object]]:
        captured.append(sql)
        return []

    monkeypatch.setattr(nikke_module.database, 'query_db', _fake_query_db)
    crawler = Nikke(path=tmp_path)

    asyncio.run(
        crawler._upsert_page_fetch_start(
            content_id=1,
            name='Nikke',
            row={},
            detail_response={'data': {'title': 'Nikke'}},
            manifest_path=tmp_path / 'manifest.json',
            hash_value='abc',
        ),
    )

    assert any('completed_at = NULL' in sql for sql in captured)


def test_replace_page_assets_and_complete_uses_single_transaction(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    captured: list[list[str]] = []

    async def _fake_transaction(statements: list[tuple[str, tuple]]) -> list[list[dict[str, object]]]:
        captured.append([sql for sql, _params in statements])
        return [[] for _sql, _params in statements]

    monkeypatch.setattr(nikke_module.database, 'query_db_transaction', _fake_transaction)
    crawler = Nikke(path=tmp_path)
    assets = {
        ('https://cdn.example.com/a.png', 'image'): Asset(
            url='https://cdn.example.com/a.png',
            kind='image',
            local_path='assets/images/a.png',
            contexts=[{'field': 'icon'}],
        ),
    }

    asyncio.run(crawler._replace_page_assets_and_mark_completed(content_id=1, assets=assets))

    assert len(captured) == 1
    assert captured[0][0].startswith('DELETE FROM nikke_page_assets')
    assert any('INSERT INTO nikke_page_assets' in sql for sql in captured[0])
    assert captured[0][-1].startswith('UPDATE nikke_pages SET completed_at = NOW()')


def test_expand_atlas_textures_uses_blob_pipeline_not_direct_get(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    atlas = tmp_path / '_blobs/sha256/aa/atlas'
    atlas.parent.mkdir(parents=True)
    atlas.write_text('texture.png\nsize: 1,1\n', encoding='utf-8')
    crawler = Nikke(path=tmp_path)
    assets = {
        ('https://cdn.example.com/model.atlas', 'live2d_atlas'): Asset(
            url='https://cdn.example.com/model.atlas',
            kind='live2d_atlas',
            local_path='assets/live2d/model/model.atlas',
            contexts=[{'live2d_key': 'model'}],
        ),
    }
    models = [{'urls': {'atlas': 'https://cdn.example.com/model.atlas'}, 'live2d_key': 'model'}]
    processed: list[str] = []

    async def _fake_process_asset(*, client: object, root: Path, asset: Asset) -> bool:
        _ = client, root
        processed.append(asset.url)
        asset.status = 'downloaded'
        return True

    async def _fake_completed_blob_for_url(_url: str) -> nikke_module.BlobRef:
        return nikke_module.BlobRef(sha256='a' * 64, size=atlas.stat().st_size, content_type='text/plain', path=atlas)

    async def _unexpected_request(*_args: object, **_kwargs: object) -> None:
        raise AssertionError

    monkeypatch.setattr(crawler, '_process_asset', _fake_process_asset)
    monkeypatch.setattr(crawler, '_completed_blob_for_url', _fake_completed_blob_for_url)
    monkeypatch.setattr(crawler, '_request', _unexpected_request)

    asyncio.run(crawler._expand_atlas_textures(client=object(), root=tmp_path, assets=assets, live2d_models=models))

    assert processed == ['https://cdn.example.com/model.atlas']
    assert ('https://cdn.example.com/texture.png', 'live2d_texture') in assets
    assert models[0]['urls']['atlas_textures'] == ['https://cdn.example.com/texture.png']
