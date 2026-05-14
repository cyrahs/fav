# ruff: noqa: INP001, S101, PLR2004

from __future__ import annotations

import json
from collections import defaultdict
from datetime import datetime

import httpx
import pytest

from src.tool.azurlane_l2d_sources import (
    L2D_SU_CATALOG_URL,
    NAGAMI_MAPPING_BUNDLE_URL,
    AzurLaneModelCatalog,
    AzurLaneSourceSnapshots,
    L2DSuSourceSnapshot,
    NagamiSourceSnapshot,
    SourceFetchMetadata,
    SourceSchemaError,
    build_azurlane_model_catalog,
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


def test_validate_catalog_resources_marks_live2d_valid_and_populates_capabilities() -> None:
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
    assert validation.is_renderer_ready()
    assert entry.availability.state == 'valid'
    assert entry.availability.validated_url == primary_url
    assert entry.capabilities.moc3 == 'https://static.example/live2d/azurlane/guanghui_7/test_model.moc3'
    assert entry.capabilities.textures == (
        'https://static.example/live2d/azurlane/guanghui_7/textures/texture_00.webp',
        'https://static.example/live2d/azurlane/guanghui_7/textures/texture_01.webp',
    )
    assert entry.capabilities.physics == 'https://static.example/live2d/azurlane/guanghui_7/test_model.physics3.json'
    assert entry.capabilities.motions == ('Idle', 'TapBody')
    assert entry.capabilities.expressions == ('smile',)
    assert entry.capabilities.has_audio is True
    assert entry.capabilities.has_text is True
    assert entry.capabilities.has_display_info is True
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
    assert entry.capabilities.moc3 == 'https://cdn.nagami.moe/live2d/guanghui_7/test_model.moc3'
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
    assert entry.capabilities.textures == (texture_url,)
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
    assert entry.capabilities.textures == (texture_url,)
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
