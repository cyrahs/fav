# ruff: noqa: INP001, S101, PLR2004

from __future__ import annotations

import json
from collections import defaultdict
from dataclasses import replace
from datetime import datetime

import httpx
import pytest

from src.tool.azurlane_l2d_sources import (
    L2D_SU_CATALOG_URL,
    NAGAMI_MAPPING_BUNDLE_URL,
    AzurLaneModelCatalog,
    AzurLaneSourceSnapshots,
    L2DSuSourceSnapshot,
    ModelEntry,
    NagamiSourceSnapshot,
    SourceFetchMetadata,
    SourceSchemaError,
    build_azurlane_l2d_health_report,
    build_azurlane_model_catalog,
    enumerate_azurlane_model_resources,
    fetch_azurlane_l2d_health_report,
    fetch_l2d_su_snapshot,
    fetch_nagami_snapshot,
    fetch_source_snapshots,
    parse_l2d_su_catalog,
    parse_nagami_mapping_bundle,
    validate_azurlane_model_catalog_resources,
)


def _l2d_su_payload() -> str:
    return json.dumps(
        {
            'Master': [
                {
                    'gameId': 1,
                    'gameName': '碧蓝航线',
                    'character': [
                        {
                            'charId': 1,
                            'charKey': 'biaoqiang',
                            'charName': '标枪',
                            'charNameEn': 'Javelin',
                            'live2d': [
                                {
                                    'costumeId': 1,
                                    'costumeName': '默认',
                                    'costumeNameEn': 'Default',
                                    'path': 'https://static.l2d.su/live2d/azurlane/biaoqiang/biaoqiang.model3.json',
                                },
                            ],
                            'spine': [],
                        },
                        {
                            'charId': 2,
                            'charKey': 'yilisi',
                            'charName': '伊莉丝',
                            'charNameEn': 'yilisi',
                            'live2d': [],
                            'spine': [
                                {
                                    'costumeId': 2,
                                    'costumeName': '某位女神的午后',
                                    'costumeNameEn': '某位女神的午后',
                                    'path': 'https://static.l2d.su/live2d/azurlane/yilisi_2_doa',
                                },
                            ],
                        },
                    ],
                },
                {
                    'gameId': 2,
                    'gameName': 'unused',
                    'character': [],
                },
            ],
        },
        ensure_ascii=False,
    )


def _nagami_bundle() -> str:
    return 'const e=JSON.parse(`{"guanghui_7":"Illustrious - Our Private \\\\"Study\\\\" Session","z23":"Z23"}`);export{e as default};'


def _nagami_bundle_from_mapping(mapping: dict[str, str]) -> str:
    return f'const e=JSON.parse(`{json.dumps(mapping, ensure_ascii=False)}`);export{{e as default}};'


def _catalog_payload(characters: list[dict[str, object]]) -> str:
    return json.dumps(
        {
            'Master': [
                {
                    'gameId': 1,
                    'gameName': '碧蓝航线',
                    'character': characters,
                },
            ],
        },
        ensure_ascii=False,
    )


def _live2d_model3_payload(*, display_info: str | None = None) -> str:
    file_references: dict[str, object] = {
        'Moc': 'test_model.moc3',
        'Textures': [
            'textures/texture_00.webp',
            'textures/texture_01.webp',
        ],
        'Physics': 'test_model.physics3.json',
        'Expressions': [
            {
                'Name': 'smile',
                'File': 'expressions/smile.exp3.json',
            },
        ],
        'Motions': {
            'Idle': [
                {
                    'File': 'motions/idle.motion3.json',
                    'Sound': 'voice/idle.wav',
                },
            ],
            'TapBody': [
                {
                    'File': 'motions/tap_body.motion3.json',
                    'Text': 'tap-body',
                },
            ],
        },
    }
    if display_info is not None:
        file_references['DisplayInfo'] = display_info

    return json.dumps({'Version': 3, 'FileReferences': file_references})


def _source_snapshots(l2d_su_payload: str, nagami_mapping: dict[str, str]) -> AzurLaneSourceSnapshots:
    l2d_su = parse_l2d_su_catalog(l2d_su_payload)
    nagami = parse_nagami_mapping_bundle(_nagami_bundle_from_mapping(nagami_mapping))
    return AzurLaneSourceSnapshots(
        l2d_su=L2DSuSourceSnapshot(
            metadata=SourceFetchMetadata.for_url(L2D_SU_CATALOG_URL, http_status=200),
            game_id=l2d_su.game_id,
            game_name=l2d_su.game_name,
            source_game_count=l2d_su.source_game_count,
            characters=l2d_su.characters,
        ),
        nagami=NagamiSourceSnapshot(
            metadata=SourceFetchMetadata.for_url(NAGAMI_MAPPING_BUNDLE_URL, http_status=200),
            entries=nagami.entries,
        ),
    )


def _catalog_entry_by_costume_key(catalog: AzurLaneModelCatalog, costume_key: str) -> list[str]:
    return [entry.id for entry in catalog.entries if entry.costume.key == costume_key]


def _catalog_entry(catalog: AzurLaneModelCatalog, entry_id: str) -> ModelEntry:
    return {entry.id: entry for entry in catalog.entries}[entry_id]


def test_parse_l2d_su_catalog_returns_azur_lane_snapshot_data() -> None:
    parsed = parse_l2d_su_catalog(_l2d_su_payload())

    assert parsed.game_id == 1
    assert parsed.game_name == '碧蓝航线'
    assert parsed.source_game_count == 2
    assert parsed.summary() == {
        'character_count': 2,
        'live2d_count': 1,
        'spine_count': 1,
    }
    assert parsed.characters[0].live2d[0].path.endswith('/biaoqiang/biaoqiang.model3.json')
    assert parsed.characters[1].spine[0].kind == 'spine'


def test_parse_l2d_su_catalog_rejects_schema_failures() -> None:
    with pytest.raises(SourceSchemaError, match='Master list'):
        parse_l2d_su_catalog(json.dumps({'Master': {}}))


def test_parse_l2d_su_catalog_treats_missing_model_lists_as_empty() -> None:
    payload = json.dumps(
        {
            'Master': [
                {
                    'gameId': 1,
                    'gameName': '碧蓝航线',
                    'character': [
                        {
                            'charId': 1,
                            'charKey': 'biaoqiang',
                            'charName': '标枪',
                            'charNameEn': 'Javelin',
                        },
                    ],
                },
            ],
        },
        ensure_ascii=False,
    )

    parsed = parse_l2d_su_catalog(payload)

    assert parsed.summary() == {
        'character_count': 1,
        'live2d_count': 0,
        'spine_count': 0,
    }


def test_parse_nagami_mapping_bundle_decodes_embedded_template_json() -> None:
    parsed = parse_nagami_mapping_bundle(_nagami_bundle())

    assert parsed.summary() == {'entry_count': 2}
    assert {entry.key: entry.name for entry in parsed.entries} == {
        'guanghui_7': 'Illustrious - Our Private "Study" Session',
        'z23': 'Z23',
    }


def test_fetch_l2d_su_snapshot_records_metadata_and_counts() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        assert str(request.url) == L2D_SU_CATALOG_URL
        return httpx.Response(
            200,
            headers={'ETag': '"abc"', 'Last-Modified': 'Thu, 14 May 2026 00:00:00 GMT'},
            text=_l2d_su_payload(),
        )

    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        snapshot = fetch_l2d_su_snapshot(client=client)

    assert snapshot.errors == ()
    assert snapshot.metadata.url == L2D_SU_CATALOG_URL
    assert snapshot.metadata.http_status == 200
    assert snapshot.metadata.etag == '"abc"'
    assert snapshot.metadata.last_modified == 'Thu, 14 May 2026 00:00:00 GMT'
    assert datetime.fromisoformat(snapshot.metadata.fetched_at).tzinfo is not None
    assert snapshot.summary()['character_count'] == 2


def test_fetch_nagami_snapshot_returns_network_error() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        message = 'boom'
        raise httpx.ConnectError(message, request=request)

    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        snapshot = fetch_nagami_snapshot(client=client)

    assert snapshot.entries == ()
    assert snapshot.errors[0].kind == 'network'
    assert snapshot.errors[0].http_status is None


def test_fetch_l2d_su_snapshot_returns_parse_error() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text='not json')

    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        snapshot = fetch_l2d_su_snapshot(client=client)

    assert snapshot.characters == ()
    assert snapshot.errors[0].kind == 'parse'
    assert snapshot.errors[0].http_status == 200


def test_fetch_nagami_snapshot_returns_schema_error() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text='const e=JSON.parse(`{"bad":""}`);')

    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        snapshot = fetch_nagami_snapshot(client=client)

    assert snapshot.entries == ()
    assert snapshot.errors[0].kind == 'schema'
    assert snapshot.errors[0].http_status == 200


def test_fetch_source_snapshots_live_counts_close_to_plan_baseline() -> None:
    snapshots = fetch_source_snapshots(timeout=30.0)
    errors = (*snapshots.l2d_su.errors, *snapshots.nagami.errors)
    network_errors = [error for error in errors if error.kind == 'network']
    if network_errors:
        pytest.skip(f'Live source unavailable: {network_errors[0].message}')

    assert snapshots.l2d_su.errors == ()
    assert snapshots.nagami.errors == ()
    assert snapshots.l2d_su.metadata.http_status == 200
    assert snapshots.nagami.metadata.http_status == 200

    l2d_su_summary = snapshots.l2d_su.summary()
    nagami_summary = snapshots.nagami.summary()
    assert 190 <= l2d_su_summary['character_count'] <= 240
    assert 190 <= l2d_su_summary['live2d_count'] <= 250
    assert 80 <= l2d_su_summary['spine_count'] <= 130
    assert 180 <= nagami_summary['entry_count'] <= 230
    assert snapshots.l2d_su.metadata.url == L2D_SU_CATALOG_URL
    assert snapshots.nagami.metadata.url == NAGAMI_MAPPING_BUNDLE_URL


def test_build_azurlane_model_catalog_merges_exact_nagami_fallback_and_prefers_l2d_su() -> None:
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 1,
                    'charKey': 'guanghui',
                    'charName': '光辉',
                    'charNameEn': 'Illustrious',
                    'live2d': [
                        {
                            'costumeId': 7,
                            'costumeName': '私人茶会',
                            'costumeNameEn': 'Our Private Study Session',
                            'path': 'https://static.l2d.su/live2d/azurlane/guanghui_7/guanghui_7.model3.json',
                        },
                    ],
                    'spine': [],
                },
                {
                    'charId': 2,
                    'charKey': 'yilisi',
                    'charName': '伊莉丝',
                    'charNameEn': 'Iris',
                    'live2d': [],
                    'spine': [
                        {
                            'costumeId': 2,
                            'costumeName': '某位女神的午后',
                            'costumeNameEn': 'Afternoon of a Goddess',
                            'path': 'https://static.l2d.su/live2d/azurlane/yilisi_2_doa',
                        },
                    ],
                },
            ],
        ),
        {
            'guanghui_7': 'Illustrious - Our Private Study Session',
            'shengluyisi_2': 'St. Louis - Blue and White Pottery',
        },
    )

    catalog = build_azurlane_model_catalog(snapshots)

    assert catalog.summary() == {
        'entry_count': 3,
        'by_type': {'live2d': 2, 'spine': 1},
        'by_source': {'l2d.su': 1, 'nagami': 1, 'merged': 1},
        'by_type_source': {
            'live2d': {'l2d.su': 0, 'nagami': 1, 'merged': 1},
            'spine': {'l2d.su': 1, 'nagami': 0, 'merged': 0},
        },
        'nagami_fallback_candidate_count': 2,
    }

    entries = {entry.id: entry for entry in catalog.entries}
    merged = entries['azurlane:live2d:guanghui:guanghui_7']
    assert merged.source == 'merged'
    assert merged.resources.primary_url == 'https://static.l2d.su/live2d/azurlane/guanghui_7/guanghui_7.model3.json'
    assert merged.resources.fallback_url == 'https://cdn.nagami.moe/live2d/guanghui_7/guanghui_7.model3.json'
    assert set(merged.to_dict()) == {'id', 'type', 'source', 'character', 'costume', 'resources', 'resource_summary', 'availability'}

    nagami_only = entries['azurlane:live2d:shengluyisi:shengluyisi_2']
    assert nagami_only.source == 'nagami'
    assert nagami_only.character.name_en == 'St. Louis'
    assert nagami_only.costume.name_en == 'Blue and White Pottery'

    spine = entries['azurlane:spine:yilisi:yilisi_2_doa']
    assert spine.source == 'l2d.su'
    assert spine.resources.fallback_url == ''


def test_build_azurlane_model_catalog_keeps_l2d_su_path_variant_separate_from_nagami_fallback() -> None:
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 3,
                    'charKey': 'adaerbote',
                    'charName': '阿达尔伯特',
                    'charNameEn': 'Adalbert',
                    'live2d': [
                        {
                            'costumeId': 30,
                            'costumeName': '幻梦变体',
                            'costumeNameEn': 'Fantasy Variant',
                            'path': 'https://static.l2d.su/live2d/azurlane/adaerbote_3_fhx/adaerbote_3.model3.json',
                        },
                    ],
                    'spine': [],
                },
            ],
        ),
        {
            'adaerbote_3': 'Adalbert - Standard Costume',
        },
    )

    catalog = build_azurlane_model_catalog(snapshots)
    entries = {entry.id: entry for entry in catalog.entries}

    l2d_su_variant = entries['azurlane:live2d:adaerbote:adaerbote_3_fhx']
    assert l2d_su_variant.source == 'l2d.su'
    assert l2d_su_variant.costume.key == 'adaerbote_3_fhx'
    assert l2d_su_variant.resources.primary_url == 'https://static.l2d.su/live2d/azurlane/adaerbote_3_fhx/adaerbote_3.model3.json'
    assert l2d_su_variant.resources.fallback_url == ''

    nagami_candidate = entries['azurlane:live2d:adaerbote:adaerbote_3']
    assert nagami_candidate.source == 'nagami'
    assert nagami_candidate.resources.primary_url == 'https://cdn.nagami.moe/live2d/adaerbote_3/adaerbote_3.model3.json'
    assert [entry.id for entry in catalog.search('Fantasy Variant')] == [l2d_su_variant.id]


def test_catalog_ids_are_key_based_and_searches_character_and_costume_names() -> None:
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 1,
                    'charKey': 'guanghui',
                    'charName': '光辉',
                    'charNameEn': 'Illustrious',
                    'live2d': [
                        {
                            'costumeId': 7,
                            'costumeName': '丝绸与茶',
                            'costumeNameEn': 'Our Private Study Session',
                            'path': 'https://static.l2d.su/live2d/azurlane/guanghui_7/guanghui_7.model3.json',
                        },
                    ],
                    'spine': [],
                },
            ],
        ),
        {
            'guanghui_7': 'Illustrious - Our Private Study Session',
            'shengluyisi_2': 'St. Louis - Blue and White Pottery',
        },
    )

    catalog = build_azurlane_model_catalog(snapshots)
    entry = catalog.search('Illustrious')[0]

    assert entry.id == 'azurlane:live2d:guanghui:guanghui_7'
    assert 'Illustrious' not in entry.id
    assert '光辉' not in entry.id
    assert [result.id for result in catalog.search('Our Private')] == [entry.id]
    assert [result.id for result in catalog.search('丝绸')] == [entry.id]
    assert [result.id for result in catalog.search('blue pottery')] == ['azurlane:live2d:shengluyisi:shengluyisi_2']
    assert [result.id for result in catalog.search('guanghui', model_type='live2d')] == [entry.id]


def test_catalog_merges_duplicate_assets_and_keeps_different_assets_as_variants() -> None:
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 1,
                    'charKey': 'biaoqiang',
                    'charName': '标枪',
                    'charNameEn': 'Javelin',
                    'live2d': [
                        {
                            'costumeId': 1,
                            'costumeName': '默认',
                            'costumeNameEn': 'Default',
                            'path': 'https://static.l2d.su/live2d/azurlane/biaoqiang/biaoqiang.model3.json',
                        },
                        {
                            'costumeId': 1,
                            'costumeName': '默认',
                            'costumeNameEn': 'Default',
                            'path': 'https://static.l2d.su/live2d/azurlane/biaoqiang/biaoqiang.model3.json',
                        },
                        {
                            'costumeId': 1,
                            'costumeName': '默认',
                            'costumeNameEn': 'Default',
                            'path': 'https://mirror.example/live2d/azurlane/biaoqiang/biaoqiang.model3.json',
                        },
                    ],
                    'spine': [],
                },
            ],
        ),
        {},
    )

    catalog = build_azurlane_model_catalog(snapshots)

    assert len(catalog.entries) == 2
    base_ids = _catalog_entry_by_costume_key(catalog, 'biaoqiang')
    assert 'azurlane:live2d:biaoqiang:biaoqiang' in base_ids
    assert any(entry_id.startswith('azurlane:live2d:biaoqiang:biaoqiang:asset-') for entry_id in base_ids)
    assert {entry.resources.primary_url for entry in catalog.entries} == {
        'https://static.l2d.su/live2d/azurlane/biaoqiang/biaoqiang.model3.json',
        'https://mirror.example/live2d/azurlane/biaoqiang/biaoqiang.model3.json',
    }


def test_enumerate_live2d_model3_resources_with_paths_and_contexts() -> None:
    primary_url = 'https://static.example/live2d/azurlane/guanghui_7/guanghui_7.model3.json'
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 1,
                    'charKey': 'guanghui',
                    'charName': '光辉',
                    'charNameEn': 'Illustrious',
                    'live2d': [
                        {
                            'costumeId': 7,
                            'costumeName': '私人茶会',
                            'costumeNameEn': 'Our Private Study Session',
                            'path': primary_url,
                        },
                    ],
                    'spine': [],
                },
            ],
        ),
        {'guanghui_7': 'Illustrious - Our Private Study Session'},
    )
    model3_source = json.dumps(
        {
            'Version': 3,
            'FileReferences': {
                'Moc': 'guanghui_7.moc3',
                'Textures': ['textures/texture_00.webp', 'textures/texture_01.webp'],
                'Physics': 'guanghui_7.physics3.json',
                'Pose': 'guanghui_7.pose3.json',
                'DisplayInfo': 'display.cdi3.json',
                'Expressions': [{'Name': 'smile', 'File': 'expressions/smile.exp3.json'}],
                'Motions': {
                    'Idle': [{'File': 'motions/idle.motion3.json', 'Sound': 'voice/idle.wav', 'Text': 'texts/idle.txt'}],
                    'TapBody': [{'File': 'motions/tap_body.motion3.json'}],
                },
            },
        },
    )

    catalog = build_azurlane_model_catalog(snapshots)
    entry = _catalog_entry(catalog, 'azurlane:live2d:guanghui:guanghui_7')
    enumeration = enumerate_azurlane_model_resources(entry, model3_source=model3_source)

    assert enumeration.model_id == entry.id
    assert enumeration.source_url == primary_url
    assert enumeration.fallback_url == 'https://cdn.nagami.moe/live2d/guanghui_7/guanghui_7.model3.json'
    assert [asset.kind for asset in enumeration.assets] == [
        'live2d.model3',
        'live2d.moc3',
        'live2d.texture',
        'live2d.texture',
        'live2d.physics',
        'live2d.pose',
        'live2d.display-info',
        'live2d.expression',
        'live2d.motion',
        'live2d.motion',
        'live2d.audio',
        'live2d.text',
    ]

    assets_by_field = {asset.context.get('live2d_field'): asset for asset in enumeration.assets if asset.kind != 'live2d.texture'}
    assert assets_by_field['model3'].local_path == 'assets/live2d/guanghui_7/guanghui_7.model3.json'
    assert assets_by_field['physics'].source_url == 'https://static.example/live2d/azurlane/guanghui_7/guanghui_7.physics3.json'
    assert assets_by_field['pose'].local_path == 'assets/live2d/guanghui_7/guanghui_7.pose3.json'
    assert assets_by_field['expression'].context['expression_name'] == 'smile'
    assert assets_by_field['audio'].fallback_url == 'https://cdn.nagami.moe/live2d/guanghui_7/voice/idle.wav'
    assert assets_by_field['text'].local_path == 'assets/live2d/guanghui_7/texts/idle.txt'
    assert assets_by_field['motion'].context['motion_group'] == 'TapBody'
    assert all(asset.context['model_id'] == entry.id for asset in enumeration.assets)
    assert all(asset.context['character_key'] == 'guanghui' for asset in enumeration.assets)
    assert all(asset.context['costume_key'] == 'guanghui_7' for asset in enumeration.assets)


def test_enumerate_spine_resources_parses_atlas_texture_pages() -> None:
    spine_base_url = 'https://static.example/live2d/azurlane/yilisi_2_doa'
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 2,
                    'charKey': 'yilisi',
                    'charName': '伊莉丝',
                    'charNameEn': 'Iris',
                    'live2d': [],
                    'spine': [
                        {
                            'costumeId': 2,
                            'costumeName': '某位女神的午后',
                            'costumeNameEn': 'Afternoon of a Goddess',
                            'path': spine_base_url,
                        },
                    ],
                },
            ],
        ),
        {},
    )
    atlas_source = """
yilisi_2_doa.webp
size: 4096,4096
format: RGBA8888

effects/glow.png
size: 1024,1024
format: RGBA8888
"""

    catalog = build_azurlane_model_catalog(snapshots)
    entry = _catalog_entry(catalog, 'azurlane:spine:yilisi:yilisi_2_doa')
    enumeration = enumerate_azurlane_model_resources(entry, atlas_source=atlas_source)

    assert [asset.kind for asset in enumeration.assets] == ['spine.skel', 'spine.atlas', 'spine.texture', 'spine.texture']
    assert [asset.source_url for asset in enumeration.assets] == [
        'https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.skel',
        'https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.atlas',
        'https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.webp',
        'https://static.example/live2d/azurlane/yilisi_2_doa/effects/glow.png',
    ]
    assert [asset.local_path for asset in enumeration.assets] == [
        'assets/spine/yilisi_2_doa/yilisi_2_doa.skel',
        'assets/spine/yilisi_2_doa/yilisi_2_doa.atlas',
        'assets/spine/yilisi_2_doa/yilisi_2_doa.webp',
        'assets/spine/yilisi_2_doa/effects/glow.png',
    ]
    assert enumeration.assets[-1].context['atlas_page'] == 'effects/glow.png'
    assert enumeration.assets[-1].context['page_index'] == 1


def test_enumerate_live2d_missing_optional_resources_does_not_fail() -> None:
    primary_url = 'https://static.example/live2d/azurlane/minimal/minimal.model3.json'
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 3,
                    'charKey': 'minimal',
                    'charName': 'Minimal',
                    'charNameEn': 'Minimal',
                    'live2d': [
                        {
                            'costumeId': 1,
                            'costumeName': 'Minimal',
                            'costumeNameEn': 'Minimal',
                            'path': primary_url,
                        },
                    ],
                    'spine': [],
                },
            ],
        ),
        {},
    )
    model3_source = json.dumps(
        {
            'Version': 3,
            'FileReferences': {
                'Moc': 'minimal.moc3',
                'Textures': ['texture.webp'],
                'Physics': None,
                'Pose': {},
                'Expressions': {},
                'Motions': [],
            },
        },
    )

    entry = build_azurlane_model_catalog(snapshots).entries[0]
    enumeration = enumerate_azurlane_model_resources(entry, model3_source=model3_source)

    assert [asset.kind for asset in enumeration.assets] == ['live2d.model3', 'live2d.moc3', 'live2d.texture']
    assert [asset.local_path for asset in enumeration.assets] == [
        'assets/live2d/minimal/minimal.model3.json',
        'assets/live2d/minimal/minimal.moc3',
        'assets/live2d/minimal/texture.webp',
    ]


def test_enumerated_resource_paths_are_deterministic_and_variant_safe() -> None:
    primary_url = 'https://static.example/live2d/azurlane/biaoqiang/biaoqiang.model3.json'
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 4,
                    'charKey': 'biaoqiang',
                    'charName': '标枪',
                    'charNameEn': 'Javelin',
                    'live2d': [
                        {
                            'costumeId': 1,
                            'costumeName': '默认',
                            'costumeNameEn': 'Default',
                            'path': primary_url,
                        },
                    ],
                    'spine': [],
                },
            ],
        ),
        {},
    )
    model3_source = json.dumps(
        {
            'Version': 3,
            'FileReferences': {
                'Moc': 'biaoqiang.moc3',
                'Textures': ['textures/texture.webp'],
            },
        },
    )

    entry = build_azurlane_model_catalog(snapshots).entries[0]
    first = enumerate_azurlane_model_resources(entry, model3_source=model3_source)
    second = enumerate_azurlane_model_resources(entry, model3_source=model3_source)
    variant_resources = replace(entry.resources, primary_url=primary_url.replace('static', 'mirror'))
    variant = replace(entry, id=f'{entry.id}:asset-test', resources=variant_resources)
    variant_first = enumerate_azurlane_model_resources(variant, model3_source=model3_source)
    variant_second = enumerate_azurlane_model_resources(variant, model3_source=model3_source)

    assert [asset.local_path for asset in first.assets] == [asset.local_path for asset in second.assets]
    assert [asset.local_path for asset in variant_first.assets] == [asset.local_path for asset in variant_second.assets]
    assert first.assets[0].local_path == 'assets/live2d/biaoqiang/biaoqiang.model3.json'
    assert variant_first.assets[0].local_path.startswith('assets/live2d/biaoqiang-')
    assert variant_first.assets[0].local_path.endswith('/biaoqiang.model3.json')


def test_validate_catalog_resources_marks_live2d_valid_and_populates_resource_summary() -> None:
    primary_url = 'https://static.example/live2d/azurlane/guanghui_7/guanghui_7.model3.json'
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 1,
                    'charKey': 'guanghui',
                    'charName': '光辉',
                    'charNameEn': 'Illustrious',
                    'live2d': [
                        {
                            'costumeId': 7,
                            'costumeName': '私人茶会',
                            'costumeNameEn': 'Our Private Study Session',
                            'path': primary_url,
                        },
                    ],
                    'spine': [],
                },
            ],
        ),
        {},
    )
    expected_head_urls = {
        'https://static.example/live2d/azurlane/guanghui_7/test_model.moc3',
        'https://static.example/live2d/azurlane/guanghui_7/textures/texture_00.webp',
        'https://static.example/live2d/azurlane/guanghui_7/textures/texture_01.webp',
        'https://static.example/live2d/azurlane/guanghui_7/guanghui_7.cdi3.json',
    }

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        if request.method == 'GET' and url == primary_url:
            return httpx.Response(200, text=_live2d_model3_payload())
        if request.method == 'HEAD' and url in expected_head_urls:
            return httpx.Response(200)
        return httpx.Response(404)

    catalog = build_azurlane_model_catalog(snapshots)
    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        report = validate_azurlane_model_catalog_resources(catalog, client=client)

    validation = report.entries[0]
    entry = report.catalog.entries[0]
    assert validation.has_available_resources()
    assert validation.resource_summary == entry.resource_summary
    assert entry.availability.state == 'valid'
    assert entry.availability.validated_url == primary_url
    assert entry.resource_summary.moc3 == 'https://static.example/live2d/azurlane/guanghui_7/test_model.moc3'
    assert entry.resource_summary.textures == (
        'https://static.example/live2d/azurlane/guanghui_7/textures/texture_00.webp',
        'https://static.example/live2d/azurlane/guanghui_7/textures/texture_01.webp',
    )
    assert entry.resource_summary.physics == 'https://static.example/live2d/azurlane/guanghui_7/test_model.physics3.json'
    assert entry.resource_summary.motions == ('Idle', 'TapBody')
    assert entry.resource_summary.expressions == ('smile',)
    assert entry.resource_summary.has_audio is True
    assert entry.resource_summary.has_text is True
    assert entry.resource_summary.has_display_info is True
    assert entry.resources.display_info_url == 'https://static.example/live2d/azurlane/guanghui_7/guanghui_7.cdi3.json'
    assert {check.kind for check in validation.checks} == {
        'live2d.model3',
        'live2d.moc3',
        'live2d.texture',
        'live2d.display-info',
    }


def test_validate_catalog_resources_marks_live2d_fallback_only_when_primary_is_broken() -> None:
    primary_url = 'https://static.example/live2d/azurlane/guanghui_7/guanghui_7.model3.json'
    fallback_url = 'https://cdn.nagami.moe/live2d/guanghui_7/guanghui_7.model3.json'
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 1,
                    'charKey': 'guanghui',
                    'charName': '光辉',
                    'charNameEn': 'Illustrious',
                    'live2d': [
                        {
                            'costumeId': 7,
                            'costumeName': '私人茶会',
                            'costumeNameEn': 'Our Private Study Session',
                            'path': primary_url,
                        },
                    ],
                    'spine': [],
                },
            ],
        ),
        {'guanghui_7': 'Illustrious - Our Private Study Session'},
    )
    fallback_head_urls = {
        'https://cdn.nagami.moe/live2d/guanghui_7/test_model.moc3',
        'https://cdn.nagami.moe/live2d/guanghui_7/textures/texture_00.webp',
        'https://cdn.nagami.moe/live2d/guanghui_7/textures/texture_01.webp',
        'https://cdn.nagami.moe/live2d/guanghui_7/display.cdi3.json',
    }

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        if request.method == 'GET' and url == primary_url:
            return httpx.Response(404)
        if request.method == 'GET' and url == fallback_url:
            return httpx.Response(200, text=_live2d_model3_payload(display_info='display.cdi3.json'))
        if request.method == 'HEAD' and url in fallback_head_urls:
            return httpx.Response(200)
        return httpx.Response(404)

    catalog = build_azurlane_model_catalog(snapshots)
    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        report = validate_azurlane_model_catalog_resources(catalog, client=client)

    entry = report.catalog.entries[0]
    assert entry.availability.state == 'fallback-only'
    assert entry.availability.validated_url == fallback_url
    assert entry.resource_summary.moc3 == 'https://cdn.nagami.moe/live2d/guanghui_7/test_model.moc3'
    assert entry.resources.display_info_url == 'https://cdn.nagami.moe/live2d/guanghui_7/display.cdi3.json'
    assert [check.source for check in report.entries[0].checks if check.kind == 'live2d.model3'] == ['primary', 'fallback']


def test_validate_catalog_resources_marks_spine_valid_and_checks_atlas_texture() -> None:
    spine_base_url = 'https://static.example/live2d/azurlane/yilisi_2_doa'
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 2,
                    'charKey': 'yilisi',
                    'charName': '伊莉丝',
                    'charNameEn': 'Iris',
                    'live2d': [],
                    'spine': [
                        {
                            'costumeId': 2,
                            'costumeName': '某位女神的午后',
                            'costumeNameEn': 'Afternoon of a Goddess',
                            'path': spine_base_url,
                        },
                    ],
                },
            ],
        ),
        {},
    )
    skel_url = 'https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.skel'
    atlas_url = 'https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.atlas'
    texture_url = 'https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.webp'

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        if request.method == 'HEAD' and url == skel_url:
            return httpx.Response(200)
        if request.method == 'GET' and url == atlas_url:
            return httpx.Response(200, text='\nyilisi_2_doa.webp\nsize: 4096,4096\nformat: RGBA8888\n')
        if request.method == 'HEAD' and url == texture_url:
            return httpx.Response(200)
        return httpx.Response(404)

    catalog = build_azurlane_model_catalog(snapshots)
    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        report = validate_azurlane_model_catalog_resources(catalog, client=client)

    entry = report.catalog.entries[0]
    assert entry.availability.state == 'valid'
    assert entry.availability.validated_url == spine_base_url
    assert entry.resource_summary.textures == (texture_url,)
    assert {check.kind for check in report.entries[0].checks} == {'spine.skel', 'spine.atlas', 'spine.texture'}


def test_validate_catalog_resources_checks_l2d_su_spine_suffix_assets_without_suffix() -> None:
    spine_base_url = 'https://static.example/live2d/azurlane/aerbien_4-spine'
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 3,
                    'charKey': 'aerbien',
                    'charName': 'Albion',
                    'charNameEn': 'Albion',
                    'live2d': [],
                    'spine': [
                        {
                            'costumeId': 4,
                            'costumeName': 'Silvermoon Faerie Princess',
                            'costumeNameEn': 'Silvermoon Faerie Princess',
                            'path': spine_base_url,
                        },
                    ],
                },
            ],
        ),
        {},
    )
    skel_url = 'https://static.example/live2d/azurlane/aerbien_4-spine/aerbien_4.skel'
    atlas_url = 'https://static.example/live2d/azurlane/aerbien_4-spine/aerbien_4.atlas'
    texture_url = 'https://static.example/live2d/azurlane/aerbien_4-spine/aerbien_4.webp'
    seen_requests: list[tuple[str, str]] = []

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        seen_requests.append((request.method, url))
        if request.method == 'HEAD' and url == skel_url:
            return httpx.Response(200)
        if request.method == 'GET' and url == atlas_url:
            return httpx.Response(200, text='\naerbien_4.webp\nsize: 4096,4096\nformat: RGBA8888\n')
        if request.method == 'HEAD' and url == texture_url:
            return httpx.Response(200)
        return httpx.Response(404)

    catalog = build_azurlane_model_catalog(snapshots)
    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        report = validate_azurlane_model_catalog_resources(catalog, client=client)

    entry = report.catalog.entries[0]
    assert entry.availability.state == 'valid'
    assert entry.availability.validated_url == spine_base_url
    assert entry.resource_summary.textures == (texture_url,)
    assert [(check.kind, check.url) for check in report.entries[0].checks] == [
        ('spine.skel', skel_url),
        ('spine.atlas', atlas_url),
        ('spine.texture', texture_url),
    ]
    assert seen_requests == [
        ('HEAD', skel_url),
        ('GET', atlas_url),
        ('HEAD', texture_url),
    ]


def test_validate_catalog_resources_records_broken_urls_and_leaves_unselected_entries_unchecked() -> None:
    broken_url = 'https://static.example/live2d/azurlane/missing/missing.model3.json'
    unchecked_url = 'https://static.example/live2d/azurlane/unchecked/unchecked.model3.json'
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 1,
                    'charKey': 'missing',
                    'charName': '失踪',
                    'charNameEn': 'Missing',
                    'live2d': [
                        {
                            'costumeId': 1,
                            'costumeName': 'Missing',
                            'costumeNameEn': 'Missing',
                            'path': broken_url,
                        },
                    ],
                    'spine': [],
                },
                {
                    'charId': 2,
                    'charKey': 'unchecked',
                    'charName': '未检查',
                    'charNameEn': 'Unchecked',
                    'live2d': [
                        {
                            'costumeId': 2,
                            'costumeName': 'Unchecked',
                            'costumeNameEn': 'Unchecked',
                            'path': unchecked_url,
                        },
                    ],
                    'spine': [],
                },
            ],
        ),
        {},
    )

    def handler(request: httpx.Request) -> httpx.Response:
        assert str(request.url) == broken_url
        return httpx.Response(404)

    catalog = build_azurlane_model_catalog(snapshots)
    selected_id = 'azurlane:live2d:missing:missing'
    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        report = validate_azurlane_model_catalog_resources(catalog, client=client, entry_ids={selected_id})

    entries = {entry.entry_id: entry for entry in report.entries}
    assert entries[selected_id].availability.state == 'broken'
    assert entries[selected_id].checks[0].url == broken_url
    assert entries[selected_id].checks[0].http_status == 404
    assert entries['azurlane:live2d:unchecked:unchecked'].availability.state == 'unchecked'
    assert entries['azurlane:live2d:unchecked:unchecked'].checks == ()
    assert report.summary()['by_state'] == {
        'valid': 0,
        'fallback-only': 0,
        'broken': 1,
        'unchecked': 1,
    }


def test_health_report_detects_source_catalog_and_resource_drift() -> None:
    previous_url = 'https://static.example/live2d/azurlane/old/old.model3.json'
    recovered_url = 'https://static.example/live2d/azurlane/old/old.model3.json'
    new_url = 'https://static.example/live2d/azurlane/new/new.model3.json'
    previous_snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 1,
                    'charKey': 'old',
                    'charName': 'Old',
                    'charNameEn': 'Old',
                    'live2d': [
                        {
                            'costumeId': 1,
                            'costumeName': 'Old',
                            'costumeNameEn': 'Old',
                            'path': previous_url,
                        },
                    ],
                    'spine': [],
                },
            ],
        ),
        {},
    )
    current_snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 1,
                    'charKey': 'old',
                    'charName': 'Old',
                    'charNameEn': 'Old',
                    'live2d': [
                        {
                            'costumeId': 1,
                            'costumeName': 'Old',
                            'costumeNameEn': 'Old',
                            'path': recovered_url,
                        },
                    ],
                    'spine': [],
                },
                {
                    'charId': 2,
                    'charKey': 'new',
                    'charName': 'New',
                    'charNameEn': 'New',
                    'live2d': [
                        {
                            'costumeId': 2,
                            'costumeName': 'New',
                            'costumeNameEn': 'New',
                            'path': new_url,
                        },
                    ],
                    'spine': [],
                },
            ],
        ),
        {},
    )
    previous_snapshots = replace(
        previous_snapshots,
        l2d_su=replace(previous_snapshots.l2d_su, metadata=SourceFetchMetadata.for_url(L2D_SU_CATALOG_URL, http_status=200, etag='"old"')),
    )
    current_snapshots = replace(
        current_snapshots,
        l2d_su=replace(current_snapshots.l2d_su, metadata=SourceFetchMetadata.for_url(L2D_SU_CATALOG_URL, http_status=200, etag='"new"')),
    )

    previous_catalog = build_azurlane_model_catalog(previous_snapshots)

    def previous_handler(request: httpx.Request) -> httpx.Response:
        assert str(request.url) == previous_url
        return httpx.Response(404)

    with httpx.Client(transport=httpx.MockTransport(previous_handler)) as client:
        previous_validation = validate_azurlane_model_catalog_resources(previous_catalog, client=client)
    previous_health = build_azurlane_l2d_health_report(
        snapshots=previous_snapshots,
        catalog=previous_catalog,
        resource_validation=previous_validation,
    )

    current_catalog = build_azurlane_model_catalog(current_snapshots)
    recovered_head_urls = {
        'https://static.example/live2d/azurlane/old/test_model.moc3',
        'https://static.example/live2d/azurlane/old/textures/texture_00.webp',
        'https://static.example/live2d/azurlane/old/textures/texture_01.webp',
        'https://static.example/live2d/azurlane/old/old.cdi3.json',
    }

    def current_handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        if request.method == 'GET' and url == recovered_url:
            return httpx.Response(200, text=_live2d_model3_payload())
        if request.method == 'HEAD' and url in recovered_head_urls:
            return httpx.Response(200)
        if request.method == 'GET' and url == new_url:
            return httpx.Response(404)
        return httpx.Response(404)

    with httpx.Client(transport=httpx.MockTransport(current_handler)) as client:
        current_validation = validate_azurlane_model_catalog_resources(current_catalog, client=client)
    current_health = build_azurlane_l2d_health_report(
        snapshots=current_snapshots,
        catalog=current_catalog,
        resource_validation=current_validation,
        previous_report=previous_health,
    )

    assert current_health.drift_summary.l2d_su.etag_changed is True
    assert current_health.drift_summary.l2d_su.entry_count_delta == 1
    assert current_health.catalog_health.new_entry_ids == ('azurlane:live2d:new:new',)
    assert current_health.drift_summary.newly_added_resource_count == 1
    assert current_health.resource_health.broken_resource_count == 1
    assert current_health.resource_health.newly_broken_entry_ids == ('azurlane:live2d:new:new',)
    assert current_health.resource_health.recovered_entry_ids == ('azurlane:live2d:old:old',)
    assert set(current_health.to_dict()) == {
        'checked_at',
        'source_health',
        'catalog_health',
        'resource_health',
        'drift_summary',
        'snapshots',
        'catalog',
        'resource_validation',
    }


def test_health_report_marks_source_changed_when_only_entry_count_delta_changes() -> None:
    snapshots = _source_snapshots(
        _catalog_payload(
            [
                {
                    'charId': 1,
                    'charKey': 'same',
                    'charName': 'Same',
                    'charNameEn': 'Same',
                    'live2d': [
                        {
                            'costumeId': 1,
                            'costumeName': 'Same',
                            'costumeNameEn': 'Same',
                            'path': 'https://static.example/live2d/azurlane/same/same.model3.json',
                        },
                    ],
                    'spine': [],
                },
            ],
        ),
        {},
    )
    metadata = SourceFetchMetadata.for_url(
        L2D_SU_CATALOG_URL,
        http_status=200,
        etag='"same"',
        last_modified='Thu, 14 May 2026 00:00:00 GMT',
    )
    snapshots = replace(snapshots, l2d_su=replace(snapshots.l2d_su, metadata=metadata))

    previous_health = build_azurlane_l2d_health_report(snapshots=snapshots)
    previous_health = replace(
        previous_health,
        source_health=replace(
            previous_health.source_health,
            l2d_su=replace(previous_health.source_health.l2d_su, entry_count=previous_health.source_health.l2d_su.entry_count + 1),
        ),
    )

    current_health = build_azurlane_l2d_health_report(snapshots=snapshots, previous_report=previous_health)

    assert current_health.source_health.l2d_su.entry_ids == previous_health.source_health.l2d_su.entry_ids
    assert current_health.drift_summary.l2d_su.entry_count_delta == -1
    assert current_health.drift_summary.l2d_su.added_entry_ids == ()
    assert current_health.drift_summary.l2d_su.removed_entry_ids == ()
    assert current_health.drift_summary.l2d_su.etag_changed is False
    assert current_health.drift_summary.l2d_su.last_modified_changed is False
    assert current_health.drift_summary.l2d_su.http_status_changed is False
    assert current_health.drift_summary.l2d_su.changed is True


def test_fetch_health_report_records_source_errors_without_raising() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        if str(request.url) == L2D_SU_CATALOG_URL:
            message = 'l2d unavailable'
            raise httpx.ConnectError(message, request=request)
        message = 'nagami unavailable'
        raise httpx.ConnectError(message, request=request)

    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        report = fetch_azurlane_l2d_health_report(client=client)

    assert report.source_health.l2d_su.ok is False
    assert report.source_health.nagami.ok is False
    assert report.catalog_health.entry_count == 0
    assert report.resource_health.broken_resource_count == 0


def test_build_azurlane_model_catalog_live_counts_and_source_merges() -> None:
    snapshots = fetch_source_snapshots(timeout=30.0)
    errors = (*snapshots.l2d_su.errors, *snapshots.nagami.errors)
    network_errors = [error for error in errors if error.kind == 'network']
    if network_errors:
        pytest.skip(f'Live source unavailable: {network_errors[0].message}')

    assert snapshots.l2d_su.errors == ()
    assert snapshots.nagami.errors == ()

    catalog = build_azurlane_model_catalog(snapshots)
    catalog_summary = catalog.summary()
    l2d_su_summary = snapshots.l2d_su.summary()
    nagami_summary = snapshots.nagami.summary()

    assert catalog_summary['by_type']['live2d'] >= l2d_su_summary['live2d_count']
    assert catalog_summary['by_type']['spine'] == l2d_su_summary['spine_count']
    assert catalog_summary['by_source']['merged'] >= 180
    assert catalog_summary['nagami_fallback_candidate_count'] == nagami_summary['entry_count']

    shared_old_entry = catalog.search('Delirious Duel')[0]
    assert shared_old_entry.id == 'azurlane:live2d:xingdengbao:xingdengbao_2'
    assert shared_old_entry.source == 'merged'
    assert shared_old_entry.resources.primary_url.startswith('https://static.l2d.su/live2d/azurlane/xingdengbao_2/')
    assert shared_old_entry.resources.fallback_url == 'https://cdn.nagami.moe/live2d/xingdengbao_2/xingdengbao_2.model3.json'

    newer_l2d_su_costume_keys = {
        'Hindenburg': 'xingdengbao_3',
        'Implacable': 'yuanchou_3',
        'Nayoro': 'mingji_2',
        'Argus': 'baiyanjuren_4',
    }
    for costume_key in newer_l2d_su_costume_keys.values():
        matches = [entry for entry in catalog.entries if entry.type == 'live2d' and entry.costume.key == costume_key]
        assert len(matches) == 1
        assert matches[0].source != 'nagami'
        assert matches[0].resources.primary_url.startswith(f'https://static.l2d.su/live2d/azurlane/{costume_key}/')

    assets_by_logical_key: defaultdict[tuple[str, str, str], set[str]] = defaultdict(set)
    ids_by_logical_key: defaultdict[tuple[str, str, str], set[str]] = defaultdict(set)
    for entry in catalog.entries:
        logical_key = (entry.type, entry.character.key, entry.costume.key)
        assets_by_logical_key[logical_key].add(entry.resources.primary_url)
        ids_by_logical_key[logical_key].add(entry.id)

    for logical_key, entry_ids in ids_by_logical_key.items():
        if len(entry_ids) > 1:
            assert len(assets_by_logical_key[logical_key]) == len(entry_ids)
