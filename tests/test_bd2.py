# ruff: noqa: INP001, S101, SLF001

import asyncio
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from pathlib import Path
from types import SimpleNamespace

import httpx
import pytest

import src.service.jobs as jobs_module
import src.web.bd2 as bd2_module
from src.api.schemas import JobRequestTarget
from src.core.config import BD2
from src.web.bd2 import Asset, assign_asset_paths, extract_resources, filter_bd2_character_rows, retry_delay_seconds

_FIVE_STAR_GROUP_ID = 122323
_EXPECTED_RETRY_AFTER_SECONDS = 2.0
_UPDATE_ROW_COUNT = 2


def _job_cfg(*, enabled: bool = True) -> SimpleNamespace:
    return SimpleNamespace(cron='0 */6 * * *', enabled=enabled, run_on_start=False)


def _text(key: str, value: str) -> dict[str, object]:
    return {'key': key, 'type': 'text', 'value': value}


def _image(key: str, value: str) -> dict[str, object]:
    return {'key': key, 'type': 'image', 'value': value}


def _video(key: str, value: str) -> dict[str, object]:
    return {'key': key, 'type': 'video', 'value': value}


def _audio(key: str, value: str) -> dict[str, object]:
    return {'key': key, 'type': 'audio', 'value': value}


def _live2d(key: str, value: dict[str, object]) -> dict[str, object]:
    return {'key': key, 'type': 'live2d', 'value': value}


def _empty_row(index: int) -> list[dict[str, object]]:
    return [_text(f'label_{index}', f'row-{index}'), _text(f'empty_{index}', '')]


def _style_rows() -> list[list[dict[str, object]]]:
    rows = [_empty_row(index) for index in range(16)]
    rows[0] = [_text('name_label', 'Costume name'), _text('costume_name', 'Office Worker')]
    rows[1] = [_text('category_label', 'Costume category'), _text('costume_category', 'Support')]
    rows[2] = [
        _text('sprite_label', 'Costume sprite'),
        _image('bound_sprite', '/media/sprite.png'),
        _image('unbound_sprite', '/media/unbound-sprite.png'),
    ]
    rows[3] = [_text('portrait_label', 'Costume portrait'), _image('bound_portrait', '/media/portrait.png')]
    rows[4] = [_text('full_portrait_label', 'Full costume portrait'), _image('bound_full_portrait', '/media/full.png')]
    rows[5] = [_text('live2d_header_label', ''), _text('live2d_file_header', 'Live2D file'), _text('live2d_icon_header', 'Live2D icon')]
    rows[6] = [
        _text('standing_label', 'Standing Live2D'),
        _live2d(
            'bound_live2d',
            {
                'live2dKey': 'model-a',
                'atlas': '/spine/model.atlas',
                'json': '/spine/model.json',
                'skel': '',
                'image': '/spine/model.png,/spine/effect.webp',
                'bg': '/spine/background.png',
            },
        ),
        _image('bound_live2d_icon', '/media/live2d-icon.png'),
    ]
    rows[8] = [_text('skill_label', 'Skill Live2D'), _video('bound_skill_video', 'https://cdn.example.com/skill.mp4')]
    rows[15] = [_text('non_art_label', 'Non art'), _image('bound_non_art', '/media/non-art.png')]
    return rows


def test_bd2_config_defaults_to_disabled_collection_bd2_path() -> None:
    cfg = BD2()

    assert cfg.enabled is False
    assert cfg.cron == '0 */6 * * *'
    assert cfg.run_on_start is False
    assert cfg.path == Path('./collection/bd2')


def test_scheduler_registration_includes_bd2(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_config = SimpleNamespace(
        web=SimpleNamespace(
            bd2=_job_cfg(enabled=False),
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
    bd2_job = next(job for job in jobs if job.key == 'bd2')

    assert bd2_job.name == 'BD2'
    assert bd2_job.enabled is False
    assert bd2_job.required_commands == ()
    assert bd2_job.factory is jobs_module.BD2


def test_api_job_enum_includes_bd2() -> None:
    assert JobRequestTarget.BD2.value == 'bd2'


def test_retry_delay_prefers_retry_after_header() -> None:
    response = httpx.Response(
        429,
        headers={'Retry-After': '2'},
        request=httpx.Request('GET', 'https://cdn.example.com/a.png'),
    )

    assert retry_delay_seconds(1, response) == _EXPECTED_RETRY_AFTER_SECONDS


def test_filter_bd2_character_rows_keeps_positive_character_descendants_only() -> None:
    tree = [
        {
            'id': _FIVE_STAR_GROUP_ID,
            'name': '5-star characters',
            'content_id': 0,
            'sort': 2,
            'child': [
                {'id': 1, 'name': 'Alice', 'content_id': '1001', 'sort': 2, 'icon': '/alice.png'},
                {'id': 2, 'name': 'Duplicate Alice', 'content_id': 1001, 'sort': 3},
                {'id': 3, 'name': 'Group placeholder', 'content_id': 0, 'sort': 4},
                {'id': 4, 'name': 'Nested', 'content_id': 0, 'sort': 5, 'child': [{'id': 5, 'name': 'Bea', 'content_id': 1002}]},
            ],
        },
        {'id': 189899, 'name': 'Equipment', 'content_id': 0, 'child': [{'id': 6, 'name': 'Sword', 'content_id': 2001}]},
    ]

    rows = filter_bd2_character_rows(tree)

    assert [row['name'] for row in rows] == ['Alice', 'Bea']
    assert [row['content_id'] for row in rows] == ['1001', 1002]
    assert all(row['_bd2_group_id'] == _FIVE_STAR_GROUP_ID for row in rows)
    assert all(row['_bd2_group_name'] == '5-star' for row in rows)
    assert all('child' not in row for row in rows)


def test_extract_resources_collects_all_page_media_and_column_contexts() -> None:
    content_json = {
        'baseData': [
            [_text('base_icon_label', 'Character icon'), _image('bound_base_icon', '/base/icon.png')],
            [_text('base_audio_label', 'Voice'), _audio('bound_base_audio', '/audio/voice.mp3')],
        ],
        'styleData': [
            {
                'name': 'style-a',
                'data': _style_rows(),
            },
        ],
    }
    reverse_bind = {
        'bound_base_icon': 'stable-base-icon',
        'bound_base_audio': 'stable-base-audio',
        'bound_sprite': 'stable-sprite',
        'bound_portrait': 'stable-portrait',
        'bound_full_portrait': 'stable-full',
        'bound_live2d': 'stable-live2d',
        'bound_live2d_icon': 'stable-live2d-icon',
        'bound_skill_video': 'stable-skill-video',
        'bound_non_art': 'stable-non-art',
    }

    assets, models = extract_resources(
        content_json=content_json,
        tree_row={'icon': '/tree/icon.png', 'icon_small': ''},
        reverse_bind=reverse_bind,
    )

    asset_urls = {asset.url for asset in assets.values()}
    assert 'https://www.gamekee.com/media/sprite.png' in asset_urls
    assert 'https://www.gamekee.com/media/portrait.png' in asset_urls
    assert 'https://www.gamekee.com/media/full.png' in asset_urls
    assert 'https://www.gamekee.com/base/icon.png' in asset_urls
    assert 'https://www.gamekee.com/audio/voice.mp3' in asset_urls
    assert 'https://www.gamekee.com/spine/model.atlas' in asset_urls
    assert 'https://www.gamekee.com/spine/model.json' in asset_urls
    assert 'https://www.gamekee.com/spine/model.png' in asset_urls
    assert 'https://www.gamekee.com/spine/effect.webp' in asset_urls
    assert 'https://www.gamekee.com/spine/background.png' in asset_urls
    assert 'https://cdn.example.com/skill.mp4' in asset_urls
    assert 'https://www.gamekee.com/tree/icon.png' in asset_urls
    assert 'https://www.gamekee.com/media/unbound-sprite.png' in asset_urls
    assert 'https://www.gamekee.com/media/non-art.png' in asset_urls

    assert models[0]['live2d_key'] == 'model-a'
    assert models[0]['costume_title'] == 'Office Worker'
    assert models[0]['field'] == 'standing_live2d'
    assert models[0]['column_index'] == 1
    assert models[0]['column_header'] == 'Live2D file'
    assert models[0]['urls']['bg'] == 'https://www.gamekee.com/spine/background.png'

    live2d_icon = next(asset for asset in assets.values() if asset.url == 'https://www.gamekee.com/media/live2d-icon.png')
    assert live2d_icon.contexts[0]['column_role'] == 'Live2D icon'
    assert live2d_icon.contexts[0]['is_art_row'] is True

    non_art = next(asset for asset in assets.values() if asset.url == 'https://www.gamekee.com/media/non-art.png')
    assert non_art.contexts[0]['field'] == ''
    assert non_art.contexts[0]['is_art_row'] is False

    unbound = next(asset for asset in assets.values() if asset.url == 'https://www.gamekee.com/media/unbound-sprite.png')
    assert unbound.contexts[0]['stable_id'] == ''


def test_assign_asset_paths_routes_audio_to_audio_directory() -> None:
    assets = {
        ('https://cdn.example.com/voice.mp3', 'audio'): Asset(
            url='https://cdn.example.com/voice.mp3',
            kind='audio',
            contexts=[{'section': 'base', 'field': 'voice'}],
        ),
    }

    assign_asset_paths(assets, content_id=1)

    assert assets[('https://cdn.example.com/voice.mp3', 'audio')].local_path == 'assets/audio/1_audio_voice_voice.mp3'


def test_expand_atlas_textures_uses_bd2_blob_pipeline(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    atlas = tmp_path / '_blobs/sha256/aa/atlas'
    atlas.parent.mkdir(parents=True)
    atlas.write_text('texture.png\nsize: 1,1\n', encoding='utf-8')
    crawler = bd2_module.BD2(path=tmp_path)
    assets = {
        ('https://cdn.example.com/model.atlas', 'live2d_atlas'): Asset(
            url='https://cdn.example.com/model.atlas',
            kind='live2d_atlas',
            local_path='assets/live2d/model/model.atlas',
            contexts=[{'live2d_key': 'model'}],
        ),
    }
    models = [{'urls': {'atlas': 'https://cdn.example.com/model.atlas'}, 'live2d_key': 'model', 'field': 'standing_live2d'}]
    processed: list[str] = []

    async def _fake_process_asset(*, client: object, root: Path, asset: Asset) -> bool:
        _ = client, root
        processed.append(asset.url)
        asset.status = 'downloaded'
        return True

    async def _fake_completed_blob_for_url(_url: str) -> bd2_module.BlobRef:
        return bd2_module.BlobRef(sha256='a' * 64, size=atlas.stat().st_size, content_type='text/plain', path=atlas)

    monkeypatch.setattr(crawler, '_process_asset', _fake_process_asset)
    monkeypatch.setattr(crawler, '_completed_blob_for_url', _fake_completed_blob_for_url)

    asyncio.run(crawler._expand_atlas_textures(client=object(), root=tmp_path, assets=assets, live2d_models=models))

    assert processed == ['https://cdn.example.com/model.atlas']
    assert ('https://cdn.example.com/texture.png', 'live2d_texture') in assets
    assert models[0]['urls']['atlas_textures'] == ['https://cdn.example.com/texture.png']


def test_expand_atlas_textures_fails_when_atlas_blob_is_unavailable(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    crawler = bd2_module.BD2(path=tmp_path)
    assets = {
        ('https://cdn.example.com/model.atlas', 'live2d_atlas'): Asset(
            url='https://cdn.example.com/model.atlas',
            kind='live2d_atlas',
            local_path='assets/live2d/model/model.atlas',
            contexts=[{'live2d_key': 'model'}],
        ),
    }
    models = [{'urls': {'atlas': 'https://cdn.example.com/model.atlas'}, 'live2d_key': 'model'}]

    async def _fake_process_asset(*, client: object, root: Path, asset: Asset) -> bool:
        _ = client, root
        asset.status = 'failed'
        return False

    monkeypatch.setattr(crawler, '_process_asset', _fake_process_asset)

    with pytest.raises(bd2_module.AssetProcessingError, match='Live2D atlas'):
        asyncio.run(crawler._expand_atlas_textures(client=object(), root=tmp_path, assets=assets, live2d_models=models))


def test_replace_page_assets_and_complete_uses_bd2_tables(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    captured: list[list[str]] = []

    async def _fake_transaction(statements: list[tuple[str, tuple]]) -> list[list[dict[str, object]]]:
        captured.append([sql for sql, _params in statements])
        return [[] for _sql, _params in statements]

    monkeypatch.setattr(bd2_module.database, 'query_db_transaction', _fake_transaction)
    crawler = bd2_module.BD2(path=tmp_path)
    assets = {
        ('https://cdn.example.com/a.png', 'image'): Asset(
            url='https://cdn.example.com/a.png',
            kind='image',
            local_path='assets/images/a.png',
            contexts=[{'field': 'costume_sprite'}],
        ),
    }

    asyncio.run(crawler._replace_page_assets_and_mark_completed(content_id=1, assets=assets))

    assert len(captured) == 1
    assert captured[0][0].startswith('DELETE FROM bd2_page_assets')
    assert any('INSERT INTO bd2_page_assets' in sql for sql in captured[0])
    assert captured[0][-1].startswith('UPDATE bd2_pages SET completed_at = NOW()')


def test_upsert_page_fetch_start_preserves_previous_completed_at(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    captured: list[str] = []

    async def _fake_query_db(sql: str, params: tuple[object, ...]) -> list[dict[str, object]]:
        _ = params
        captured.append(sql)
        return []

    monkeypatch.setattr(bd2_module.database, 'query_db', _fake_query_db)
    crawler = bd2_module.BD2(path=tmp_path)

    asyncio.run(
        crawler._upsert_page_fetch_start(
            content_id=1,
            name='Name',
            row={'content_id': 1},
            detail_response={'data': {'content_json': '{}'}},
            manifest_path=tmp_path / 'manifest.json',
            hash_value='hash',
        ),
    )

    assert len(captured) == 1
    assert 'completed_at = NULL' not in captured[0]


def test_upsert_tree_pages_can_skip_deactivating_missing_pages(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    captured: list[list[str]] = []

    async def _fake_transaction(statements: list[tuple[str, tuple]]) -> list[list[dict[str, object]]]:
        captured.append([sql for sql, _params in statements])
        return [[] for _sql, _params in statements]

    monkeypatch.setattr(bd2_module.database, 'query_db_transaction', _fake_transaction)
    crawler = bd2_module.BD2(path=tmp_path)

    asyncio.run(crawler._upsert_tree_pages([{'content_id': 1, 'name': 'Name'}], deactivate_missing=False))

    assert len(captured) == 1
    assert all(not sql.startswith('UPDATE bd2_pages SET active = FALSE') for sql in captured[0])


def test_download_character_raises_when_bd2_lock_is_held(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    lock_names: list[str] = []

    @asynccontextmanager
    async def _fake_lock(name: str) -> AsyncIterator[bool]:
        lock_names.append(name)
        yield False

    monkeypatch.setattr(bd2_module.database, 'advisory_lock', _fake_lock)
    crawler = bd2_module.BD2(path=tmp_path, client=object())

    with pytest.raises(RuntimeError, match='advisory lock'):
        asyncio.run(crawler.download_character('1', skip_assets=True))

    assert lock_names == ['bd2']


def test_update_reports_failures_after_attempting_remaining_pages(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    lock_names: list[str] = []
    crawled: list[int] = []

    @asynccontextmanager
    async def _fake_lock(name: str) -> AsyncIterator[bool]:
        lock_names.append(name)
        yield True

    async def _noop() -> None:
        return None

    async def _fake_fetch_tree_rows(_client: object) -> list[dict[str, object]]:
        return [{'content_id': 1, 'name': 'First'}, {'content_id': 2, 'name': 'Second'}]

    async def _fake_upsert_tree_pages(rows: list[dict[str, object]], *, deactivate_missing: bool) -> None:
        assert len(rows) == _UPDATE_ROW_COUNT
        assert deactivate_missing is True

    async def _fake_crawl_page(
        *,
        client: object,
        tree_row: dict[str, object],
        content_id: int,
    ) -> Path:
        _ = client, tree_row
        crawled.append(content_id)
        if content_id == 1:
            msg = 'boom'
            raise RuntimeError(msg)
        return tmp_path / str(content_id)

    monkeypatch.setattr(bd2_module.database, 'advisory_lock', _fake_lock)
    crawler = bd2_module.BD2(path=tmp_path, client=object())
    monkeypatch.setattr(crawler, '_ensure_schema', _noop)
    monkeypatch.setattr(crawler, '_fetch_tree_rows', _fake_fetch_tree_rows)
    monkeypatch.setattr(crawler, '_upsert_tree_pages', _fake_upsert_tree_pages)
    monkeypatch.setattr(crawler, '_crawl_page', _fake_crawl_page)

    with pytest.raises(bd2_module.CrawlRunError, match='1 BD2 pages failed: 1'):
        asyncio.run(crawler.update())

    assert lock_names == ['bd2']
    assert crawled == [1, 2]
