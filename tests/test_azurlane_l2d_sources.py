# ruff: noqa: INP001, S101, PLR2004

from __future__ import annotations

import json
import os
from collections import defaultdict
from dataclasses import replace
from datetime import datetime
from typing import Any

import httpx
import pytest

from src.tool.azurlane_l2d_sources import (
    L2D_SU_ENGLISH_REGION,
    L2D_SU_PRIMARY_REGION,
    L2D_SU_STATIC_BASE_URL,
    NAGAMI_MAPPING_URL,
    AzurLaneModelCatalog,
    AzurLaneSourceSnapshots,
    L2DSuCharacterSnapshot,
    L2DSuModelSnapshot,
    L2DSuSourceSnapshot,
    ModelEntry,
    NagamiSourceSnapshot,
    SourceFetchMetadata,
    SourceSchemaError,
    SpineModelPart,
    apply_l2d_su_model_paths,
    apply_l2d_su_ship_classes,
    build_azurlane_l2d_health_report,
    build_azurlane_model_catalog,
    enumerate_azurlane_model_resources,
    fetch_azurlane_l2d_health_report,
    fetch_l2d_su_snapshot,
    fetch_nagami_snapshot,
    fetch_source_snapshots,
    l2d_su_character_fingerprint,
    l2d_su_ship_index_url,
    model_probe_urls,
    parse_l2d_su_ship_class,
    parse_l2d_su_ship_index,
    parse_l2d_su_ship_models,
    parse_l2d_su_ship_voices,
    parse_nagami_mapping,
    parse_spine_parts_manifest,
    probe_l2d_su_origin,
    spine_parts_manifest_url,
    spine_resource_manifest,
    validate_azurlane_model_catalog_resources,
)

_LIVE_SOURCE_SMOKE_ENV = 'FAV_RUN_LIVE_AZURLANE_SOURCE_SMOKE'
_PRIMARY_INDEX_URL = l2d_su_ship_index_url(L2D_SU_PRIMARY_REGION)
_ENGLISH_INDEX_URL = l2d_su_ship_index_url(L2D_SU_ENGLISH_REGION)


def _skip_unless_live_source_smoke_enabled() -> None:
    if os.getenv(_LIVE_SOURCE_SMOKE_ENV) != '1':
        pytest.skip(f'Set {_LIVE_SOURCE_SMOKE_ENV}=1 to run the live Azur Lane source smoke test.')


def _skin(  # noqa: PLR0913
    skin_id: int,
    name: str,
    *,
    key: str,
    kind: str = 'live2d',
    skin_type_name: str = 'Skin',
    live2d_plus: bool = False,
) -> dict[str, object]:
    feature_tag = 'spine' if kind == 'spine' else ('Live2D+' if live2d_plus else 'Live2D')
    return {
        'id': skin_id,
        'name': name,
        'groupIndex': 1,
        'skinType': 4,
        'skinTypeName': skin_type_name,
        'featureTags': [feature_tag],
        'painting': key,
        'prefab': key,
        'dynamicType': kind,
        'isLive2d': kind == 'live2d',
        'isLive2dPlus': live2d_plus,
        'isDynamic': True,
        'isSpine': kind == 'spine',
    }


def _static_skin(skin_id: int, name: str, *, key: str) -> dict[str, object]:
    return {
        'id': skin_id,
        'name': name,
        'skinTypeName': 'Default',
        'featureTags': [],
        'painting': key,
        'prefab': key,
        'dynamicType': 'painting',
        'isLive2d': False,
        'isLive2dPlus': False,
        'isDynamic': False,
        'isSpine': False,
        'paintingFaceIds': ['1', '2'],
        'assetPaths': {
            'squareIcon': f'squareicon/{key}',
            'shipyardIcon': f'shipyardicon/{key}',
            'qIcon': f'qicon/{key}',
            'painting': f'painting/{key}',
        },
    }


def _ship(  # noqa: PLR0913
    ship_group_id: int,
    resource_key: str,
    name: str,
    *,
    english_name: str = '',
    nation: str = 'Royal Navy',
    ship_type: str = 'Destroyer',
    rarity: str = 'SR',
    skins: list[dict[str, object]] | None = None,
) -> dict[str, object]:
    return {
        'shipGroupId': ship_group_id,
        'name': name,
        'englishName': english_name,
        'nationName': nation,
        'typeName': ship_type,
        'rarityName': rarity,
        'resourceKey': resource_key,
        'skins': skins or [],
    }


def _ship_index_payload(ships: list[dict[str, object]], *, region: str = 'CN', new_skins: list[dict[str, object]] | None = None) -> str:
    return json.dumps(
        {
            'generatedAt': '2026-08-12T04:33:18.041Z',
            'locale': region,
            'version': '9.7.295',
            'ships': ships,
            'newSkins': new_skins or [],
        },
        ensure_ascii=False,
    )


def _nagami_mapping_payload(mapping: dict[str, object]) -> str:
    return json.dumps(mapping, ensure_ascii=False)


def _spine_parts_payload(key: str, suffixes: tuple[str, ...]) -> str:
    return json.dumps(
        {
            'version': 1,
            'models': [
                {
                    'name': f'{key}{suffix}',
                    'skeleton': f'{key}{suffix}.skel',
                    'atlases': [f'{key}{suffix}.atlas'],
                    'animation': 'normal',
                    'loop': True,
                }
                for suffix in suffixes
            ],
        },
    )


def _live2d_url(key: str) -> str:
    return f'{L2D_SU_STATIC_BASE_URL}/live2d/{key}/{key}.model3.json'


def _spine_url(key: str) -> str:
    return f'{L2D_SU_STATIC_BASE_URL}/spinepainting/{key}'


def _model(kind: str, item: dict[str, Any]) -> L2DSuModelSnapshot:
    return L2DSuModelSnapshot(
        kind=kind,
        costume_id=item['costumeId'],
        costume_name=item['costumeName'],
        costume_name_en=item['costumeNameEn'],
        path=item['path'],
        model_key=item.get('modelKey', ''),
        skin_type=item.get('skinType', 'Skin'),
        feature_tags=tuple(item.get('featureTags', ())),
    )


def _characters(items: list[dict[str, Any]]) -> tuple[L2DSuCharacterSnapshot, ...]:
    return tuple(
        L2DSuCharacterSnapshot(
            char_id=item['charId'],
            char_key=item['charKey'],
            char_name=item['charName'],
            char_name_en=item['charNameEn'],
            nation=item.get('nation', ''),
            ship_type=item.get('shipType', ''),
            rarity=item.get('rarity', ''),
            live2d=tuple(_model('live2d', model) for model in item.get('live2d', [])),
            spine=tuple(_model('spine', model) for model in item.get('spine', [])),
        )
        for item in items
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


def _source_snapshots(characters: tuple[L2DSuCharacterSnapshot, ...], nagami_mapping: dict[str, str]) -> AzurLaneSourceSnapshots:
    nagami = parse_nagami_mapping(_nagami_mapping_payload(nagami_mapping))
    return AzurLaneSourceSnapshots(
        l2d_su=L2DSuSourceSnapshot(
            metadata=SourceFetchMetadata.for_url(_PRIMARY_INDEX_URL, http_status=200),
            region=L2D_SU_PRIMARY_REGION,
            game_version='9.7.295',
            ship_count=len(characters),
            characters=characters,
        ),
        nagami=NagamiSourceSnapshot(
            metadata=SourceFetchMetadata.for_url(NAGAMI_MAPPING_URL, http_status=200),
            entries=nagami.entries,
        ),
    )


def _catalog_entry_by_costume_key(catalog: AzurLaneModelCatalog, costume_key: str) -> list[str]:
    return [entry.id for entry in catalog.entries if entry.costume.key == costume_key]


def _catalog_entry(catalog: AzurLaneModelCatalog, entry_id: str) -> ModelEntry:
    return {entry.id: entry for entry in catalog.entries}[entry_id]


def test_parse_l2d_su_ship_index_derives_model_urls_from_skin_keys() -> None:
    payload = _ship_index_payload(
        [
            _ship(
                1,
                'biaoqiang',
                '标枪',
                english_name='HMS Javelin',
                skins=[
                    _static_skin(10, '标枪', key='biaoqiang'),
                    _skin(11, '默认', key='biaoqiang_2'),
                ],
            ),
            _ship(2, 'yilisi', '伊莉丝', skins=[_skin(20, '某位女神的午后', key='yilisi_2_doa', kind='spine')]),
        ],
        new_skins=[{'shipGroupId': 2, 'shipName': '伊莉丝', 'skinName': '某位女神的午后', 'skinType': 'Spine', 'skinIds': [20]}],
    )

    parsed = parse_l2d_su_ship_index(payload, region='CN')

    assert parsed.region == 'CN'
    assert parsed.game_version == '9.7.295'
    assert parsed.ship_count == 2
    assert parsed.summary() == {
        'character_count': 2,
        'live2d_count': 1,
        'spine_count': 1,
        'painting_count': 3,
    }
    assert parsed.characters[0].char_name_en == 'HMS Javelin'
    assert parsed.characters[0].live2d[0].path == 'https://static.l2d.su/azurlane/live2d/biaoqiang_2/biaoqiang_2.model3.json'
    assert parsed.characters[0].live2d[0].feature_tags == ('Live2D',)
    assert parsed.characters[1].spine[0].path == 'https://static.l2d.su/azurlane/spinepainting/yilisi_2_doa'
    assert parsed.characters[1].spine[0].model_key == 'yilisi_2_doa'
    assert parsed.new_skins[0].skin_ids == (20,)
    assert [painting.model_key for painting in parsed.characters[0].paintings] == ['biaoqiang', 'biaoqiang_2']
    assert parsed.characters[0].paintings[0].path == 'https://static.l2d.su/azurlane/painting/biaoqiang.webp'
    assert parsed.characters[0].paintings[0].face_ids == ('1', '2')
    assert parsed.characters[0].paintings[0].square_icon == 'squareicon/biaoqiang'


def test_parse_l2d_su_ship_index_rejects_html_and_schema_failures() -> None:
    with pytest.raises(SourceSchemaError, match='ships list'):
        parse_l2d_su_ship_index(json.dumps({'ships': {}}))


def test_parse_l2d_su_ship_index_treats_ships_without_model_skins_as_empty() -> None:
    payload = _ship_index_payload([_ship(1, 'biaoqiang', '标枪', skins=[_static_skin(10, '标枪', key='biaoqiang')])])

    parsed = parse_l2d_su_ship_index(payload)

    assert parsed.summary() == {
        'character_count': 1,
        'live2d_count': 0,
        'spine_count': 0,
        'painting_count': 1,
    }


def test_parse_l2d_su_ship_models_maps_costume_ids_to_absolute_urls() -> None:
    payload = json.dumps(
        {
            'ship': {
                'shipGroupId': 40505,
                'skins': [
                    {'id': 405050, 'model': {'type': 'spine', 'path': 'spinepainting/bisimaiZ', 'key': 'bisimaiZ'}},
                    {'id': 405051, 'model': {'type': 'live2d', 'path': 'live2d/Z23/Z23.model3.json', 'key': 'Z23'}},
                    {'id': 405052},
                    {'id': 405053, 'model': {'type': 'spine', 'path': 'https://mirror.example/spine/x'}},
                ],
            },
        },
    )

    assert parse_l2d_su_ship_models(payload) == {
        405050: 'https://static.l2d.su/azurlane/spinepainting/bisimaiZ',
        405051: 'https://static.l2d.su/azurlane/live2d/Z23/Z23.model3.json',
        405053: 'https://mirror.example/spine/x',
    }


def test_parse_l2d_su_ship_models_rejects_payloads_without_a_ship() -> None:
    with pytest.raises(SourceSchemaError, match='ship object'):
        parse_l2d_su_ship_models(json.dumps({'generatedAt': '2026-08-12'}))


def test_model_probe_urls_cover_both_spine_layouts() -> None:
    snapshots = _source_snapshots(
        _characters(
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
                            'costumeNameEn': 'Study Session',
                            'path': _live2d_url('guanghui_7'),
                        },
                    ],
                    'spine': [
                        {
                            'costumeId': 8,
                            'costumeName': '慵懒时光',
                            'costumeNameEn': 'Lazy Days',
                            'path': _spine_url('lafei_12'),
                        },
                    ],
                },
            ],
        ),
        {},
    )
    catalog = build_azurlane_model_catalog(snapshots)

    assert model_probe_urls(_catalog_entry(catalog, 'azurlane:live2d:guanghui:guanghui_7')) == (_live2d_url('guanghui_7'),)
    assert model_probe_urls(_catalog_entry(catalog, 'azurlane:spine:guanghui:lafei_12')) == (
        f'{_spine_url("lafei_12")}/lafei_12.skel',
        f'{_spine_url("lafei_12")}/lafei_12.json',
    )


def test_apply_l2d_su_model_paths_corrects_l2d_su_entries_only() -> None:
    snapshots = _source_snapshots(
        _characters(
            [
                {
                    'charId': 40505,
                    'charKey': 'bisimaiz',
                    'charName': '俾斯麦Zwei',
                    'charNameEn': 'Bismarck Zwei',
                    'spine': [
                        {
                            'costumeId': 405050,
                            'costumeName': '俾斯麦Zwei',
                            'costumeNameEn': 'Bismarck Zwei',
                            'path': _spine_url('bisimaiz'),
                        },
                    ],
                },
            ],
        ),
        {'shengluyisi_2': 'St. Louis - Blue and White Pottery'},
    )
    catalog = build_azurlane_model_catalog(snapshots)
    corrected = _spine_url('bisimaiZ')

    patched = apply_l2d_su_model_paths(catalog, {405050: corrected, 999: 'https://ignored.example/x'})

    assert _catalog_entry(patched, 'azurlane:spine:bisimaiz:bisimaiz').resources.primary_url == corrected
    nagami_entry = _catalog_entry(patched, 'azurlane:live2d:shengluyisi:shengluyisi_2')
    assert nagami_entry.resources.primary_url == 'https://cdn.nagami.moe/live2d/shengluyisi_2/shengluyisi_2.model3.json'
    assert apply_l2d_su_model_paths(catalog, {}) is catalog


def test_parse_l2d_su_ship_index_aggregates_skin_series_and_default_skin() -> None:
    dressed = _skin(11, '泳装', key='biaoqiang_2')
    dressed['shopTypeName'] = 'Swimsuits'
    repeat = _skin(12, '另一件泳装', key='biaoqiang_3')
    repeat['shopTypeName'] = 'Swimsuits'
    ship = _ship(1, 'biaoqiang', '标枪', skins=[_static_skin(10, '标枪', key='biaoqiang'), dressed, repeat])
    ship['defaultSkinId'] = 10

    parsed = parse_l2d_su_ship_index(_ship_index_payload([ship]))

    character = parsed.characters[0]
    assert character.default_skin_id == 10
    # Shopless skins fall back to their skin type, which is how l2d.su's own skinSeries filter is built.
    assert character.skin_series == ('Default', 'Swimsuits')


def test_apply_l2d_su_ship_classes_attaches_detail_only_class_names() -> None:
    payload = _ship_index_payload([_ship(1, 'biaoqiang', '标枪', skins=[_skin(10, '默认', key='biaoqiang')])])
    catalog = build_azurlane_model_catalog(_source_snapshots(parse_l2d_su_ship_index(payload).characters, {}))
    assert all(entry.character.class_name == '' for entry in catalog.entries)

    patched = apply_l2d_su_ship_classes(catalog, {1: 'J Class', 999: 'Ignored Class'})

    assert {entry.character.class_name for entry in patched.entries} == {'J Class'}
    assert apply_l2d_su_ship_classes(catalog, {}) is catalog


def _origin_probe(handler: Any) -> Any:
    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        return probe_l2d_su_origin('http://user:pass@proxy.example:8080', client=client)


def test_probe_l2d_su_origin_reports_each_failure_mode() -> None:
    def blocked(request: httpx.Request) -> httpx.Response:
        message = 'null routed'
        raise httpx.ConnectError(message, request=request)

    def rejected(request: httpx.Request) -> httpx.Response:
        return httpx.Response(403 if '/data/' in str(request.url) else 200, text='1.2.3.4')

    def reachable(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text='203.0.113.7' if 'checkip' in str(request.url) else '')

    # An unconfigured proxy is reported as such rather than attempted.
    assert probe_l2d_su_origin('  ').code == 'incomplete'

    blocked_result = _origin_probe(blocked)
    assert (blocked_result.ok, blocked_result.code) == (False, 'unreachable')

    rejected_result = _origin_probe(rejected)
    assert (rejected_result.ok, rejected_result.code, rejected_result.exit_ip) == (False, 'http_error', '1.2.3.4')

    ok_result = _origin_probe(reachable)
    assert (ok_result.ok, ok_result.code, ok_result.exit_ip) == (True, 'ok', '203.0.113.7')


def test_parse_l2d_su_ship_class_reads_class_name() -> None:
    detail = json.dumps({'ship': {'shipGroupId': 1, 'className': 'J Class', 'skins': []}})

    assert parse_l2d_su_ship_class(detail) == 'J Class'
    assert parse_l2d_su_ship_class(json.dumps({'ship': {'shipGroupId': 1}})) == ''
    with pytest.raises(SourceSchemaError, match='ship object'):
        parse_l2d_su_ship_class(json.dumps({'version': 1}))


def test_parse_l2d_su_ship_voices_reads_words_and_extra_words() -> None:
    payload = json.dumps(
        {
            'ship': {
                'shipGroupId': 960007,
                'skins': [
                    {
                        'id': 9600070,
                        'words': [
                            {
                                'key': 'battle',
                                'voiceName': '旗舰开战',
                                'resourceKey': 'warcry',
                                'voicePath': 'cue/cv-960007-battle/warcry',
                                'l2dAction': 'battle',
                                'spineAction': 'attack',
                                'faceId': '5',
                                'text': '风暴啊，请赐予我力量。',  # noqa: RUF001
                            },
                        ],
                        'extraWords': [
                            {'key': 'detail', 'voicePath': 'cue/cv-960007/detail_ex1100', 'text': '手伸出来……'},
                        ],
                    },
                    {'id': 9600071, 'words': []},
                ],
            },
        },
        ensure_ascii=False,
    )

    voices = parse_l2d_su_ship_voices(payload)

    assert set(voices) == {9600070}
    battle, extra = voices[9600070]
    assert battle.voice_path == 'cue/cv-960007-battle/warcry'
    assert battle.face_id == '5'
    assert battle.l2d_action == 'battle'
    assert battle.is_extra is False
    assert extra.voice_path == 'cue/cv-960007/detail_ex1100'
    assert extra.is_extra is True


def test_l2d_su_character_fingerprint_tracks_skin_changes() -> None:
    payload = _ship_index_payload([_ship(1, 'biaoqiang', '标枪', skins=[_static_skin(10, '标枪', key='biaoqiang')])])
    changed = _ship_index_payload([_ship(1, 'biaoqiang', '标枪', skins=[_static_skin(10, '新皮肤', key='biaoqiang')])])

    baseline = l2d_su_character_fingerprint(parse_l2d_su_ship_index(payload).characters[0])

    assert l2d_su_character_fingerprint(parse_l2d_su_ship_index(payload).characters[0]) == baseline
    assert l2d_su_character_fingerprint(parse_l2d_su_ship_index(changed).characters[0]) != baseline


def test_enumerate_painting_resources_covers_image_faces_icons_and_voices() -> None:
    payload = _ship_index_payload([_ship(1, 'biaoqiang', '标枪', skins=[_static_skin(10, '标枪', key='biaoqiang')])])
    parsed = parse_l2d_su_ship_index(payload, region='CN')
    catalog = build_azurlane_model_catalog(_source_snapshots(parsed.characters, {}))
    entry = _catalog_entry(catalog, 'azurlane:painting:biaoqiang:biaoqiang')
    detail = json.dumps(
        {'ship': {'shipGroupId': 1, 'skins': [{'id': 10, 'words': [{'key': 'detail', 'voicePath': 'cue/cv-1/detail'}]}]}},
    )

    enumeration = enumerate_azurlane_model_resources(entry, voice_lines=parse_l2d_su_ship_voices(detail)[10])

    by_kind = defaultdict(list)
    for asset in enumeration.assets:
        by_kind[asset.kind].append(asset)
    assert sorted(by_kind) == ['icon.q', 'icon.shipyard', 'icon.square', 'painting.face', 'painting.image', 'voice.audio']
    assert by_kind['painting.image'][0].source_url == f'{L2D_SU_STATIC_BASE_URL}/painting/biaoqiang.webp'
    assert by_kind['painting.image'][0].local_path == 'assets/painting/biaoqiang/biaoqiang.webp'
    assert [asset.source_url for asset in by_kind['painting.face']] == [
        f'{L2D_SU_STATIC_BASE_URL}/paintingface/biaoqiang/1.webp',
        f'{L2D_SU_STATIC_BASE_URL}/paintingface/biaoqiang/2.webp',
    ]
    assert by_kind['painting.face'][0].local_path == 'assets/painting/biaoqiang/paintingface/biaoqiang/1.webp'
    assert by_kind['icon.square'][0].source_url == f'{L2D_SU_STATIC_BASE_URL}/squareicon/biaoqiang.webp'
    assert by_kind['voice.audio'][0].source_url == f'{L2D_SU_STATIC_BASE_URL}/cue/cv-1/detail.ogg'
    assert by_kind['voice.audio'][0].local_path == 'assets/painting/biaoqiang/cue/cv-1/detail.ogg'
    assert by_kind['voice.audio'][0].context['text'] == ''
    assert by_kind['voice.audio'][0].context['key'] == 'detail'


def test_painting_entries_keep_their_paths_when_model_paths_are_applied() -> None:
    payload = _ship_index_payload([_ship(1, 'biaoqiang', '标枪', skins=[_skin(10, '默认', key='biaoqiang')])])
    parsed = parse_l2d_su_ship_index(payload, region='CN')
    catalog = build_azurlane_model_catalog(_source_snapshots(parsed.characters, {}))
    resolved_model_url = _live2d_url('Biaoqiang')

    patched = apply_l2d_su_model_paths(catalog, {10: resolved_model_url})

    assert _catalog_entry(patched, 'azurlane:live2d:biaoqiang:biaoqiang').resources.primary_url == resolved_model_url
    painting = _catalog_entry(patched, 'azurlane:painting:biaoqiang:biaoqiang')
    assert painting.resources.primary_url == f'{L2D_SU_STATIC_BASE_URL}/painting/biaoqiang.webp'
    assert model_probe_urls(painting) == (painting.resources.primary_url,)


def test_parse_l2d_su_ship_index_prefers_painting_over_prefab_for_the_model_key() -> None:
    skin = _skin(307172, '苍空的凯歌', key='yunlong_3', kind='spine')
    skin['prefab'] = 'yunlong_2'
    payload = _ship_index_payload([_ship(30717, 'yunlong', '云龙', skins=[skin])])

    parsed = parse_l2d_su_ship_index(payload)

    assert parsed.characters[0].spine[0].model_key == 'yunlong_3'
    assert parsed.characters[0].spine[0].path == 'https://static.l2d.su/azurlane/spinepainting/yunlong_3'


def test_parse_nagami_mapping_reads_name_and_background() -> None:
    parsed = parse_nagami_mapping(
        _nagami_mapping_payload(
            {
                'guanghui_7': {'name': 'Illustrious - Our Private "Study" Session', 'bg': '145'},
                'z23': 'Z23',
            },
        ),
    )

    assert parsed.summary() == {'entry_count': 2}
    assert {entry.key: (entry.name, entry.background) for entry in parsed.entries} == {
        'guanghui_7': ('Illustrious - Our Private "Study" Session', '145'),
        'z23': ('Z23', ''),
    }


def test_fetch_l2d_su_snapshot_merges_english_names_and_records_metadata() -> None:
    primary = _ship_index_payload([_ship(1, 'biaoqiang', '标枪', skins=[_skin(11, '闪耀的白刃', key='biaoqiang_2')])])
    english = _ship_index_payload(
        [_ship(1, 'biaoqiang', 'Javelin', skins=[_skin(11, 'Gleaming White Blade', key='biaoqiang_2')])],
        region='EN',
    )
    requested_urls: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        requested_urls.append(url)
        if url == _ENGLISH_INDEX_URL:
            return httpx.Response(200, text=english)
        return httpx.Response(
            200,
            headers={'ETag': '"abc"', 'Last-Modified': 'Thu, 14 May 2026 00:00:00 GMT'},
            text=primary,
        )

    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        snapshot = fetch_l2d_su_snapshot(client=client)

    assert requested_urls == [_PRIMARY_INDEX_URL, _ENGLISH_INDEX_URL]
    assert snapshot.errors == ()
    assert snapshot.metadata.url == _PRIMARY_INDEX_URL
    assert snapshot.metadata.http_status == 200
    assert snapshot.metadata.etag == '"abc"'
    assert snapshot.metadata.last_modified == 'Thu, 14 May 2026 00:00:00 GMT'
    assert datetime.fromisoformat(snapshot.metadata.fetched_at).tzinfo is not None
    assert snapshot.summary()['character_count'] == 1
    assert snapshot.characters[0].char_name == '标枪'
    assert snapshot.characters[0].char_name_en == 'Javelin'
    assert snapshot.characters[0].live2d[0].costume_name == '闪耀的白刃'
    assert snapshot.characters[0].live2d[0].costume_name_en == 'Gleaming White Blade'


def test_fetch_l2d_su_snapshot_invokes_origin_throttle_before_each_index_request() -> None:
    primary = _ship_index_payload([_ship(1, 'biaoqiang', '标枪', skins=[_skin(11, '闪耀的白刃', key='biaoqiang_2')])])
    english = _ship_index_payload([_ship(1, 'biaoqiang', 'Javelin', skins=[_skin(11, 'Blade', key='biaoqiang_2')])], region='EN')
    events: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        events.append(f'GET {request.url}')
        return httpx.Response(200, text=english if str(request.url) == _ENGLISH_INDEX_URL else primary)

    def throttle() -> None:
        events.append('throttle')

    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        fetch_l2d_su_snapshot(client=client, origin_throttle=throttle)

    # Each origin index request is immediately preceded by a throttle acquisition.
    assert events == ['throttle', f'GET {_PRIMARY_INDEX_URL}', 'throttle', f'GET {_ENGLISH_INDEX_URL}']


def test_fetch_l2d_su_snapshot_reports_english_region_failure_without_dropping_models() -> None:
    primary = _ship_index_payload([_ship(1, 'biaoqiang', '标枪', skins=[_skin(11, '闪耀的白刃', key='biaoqiang_2')])])

    def handler(request: httpx.Request) -> httpx.Response:
        if str(request.url) == _ENGLISH_INDEX_URL:
            return httpx.Response(503)
        return httpx.Response(200, text=primary)

    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        snapshot = fetch_l2d_su_snapshot(client=client)

    assert snapshot.summary()['live2d_count'] == 1
    assert snapshot.characters[0].live2d[0].costume_name_en == ''
    assert [(error.kind, error.url, error.http_status) for error in snapshot.errors] == [('network', _ENGLISH_INDEX_URL, 503)]


def test_fetch_nagami_snapshot_returns_network_error() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        message = 'boom'
        raise httpx.ConnectError(message, request=request)

    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        snapshot = fetch_nagami_snapshot(client=client)

    assert snapshot.entries == ()
    assert snapshot.errors[0].kind == 'network'
    assert snapshot.errors[0].http_status is None


def test_fetch_l2d_su_snapshot_returns_parse_error_for_spa_html() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text='<!doctype html><html><body><div id="root"></div></body></html>')

    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        snapshot = fetch_l2d_su_snapshot(client=client)

    assert snapshot.characters == ()
    assert snapshot.errors[0].kind == 'parse'
    assert snapshot.errors[0].http_status == 200


def test_fetch_nagami_snapshot_returns_schema_error() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text=_nagami_mapping_payload({'bad': {'name': ''}}))

    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        snapshot = fetch_nagami_snapshot(client=client)

    assert snapshot.entries == ()
    assert snapshot.errors[0].kind == 'schema'
    assert snapshot.errors[0].http_status == 200


def test_fetch_source_snapshots_live_counts_close_to_plan_baseline() -> None:
    _skip_unless_live_source_smoke_enabled()
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
    assert 700 <= l2d_su_summary['character_count'] <= 1200
    assert 200 <= l2d_su_summary['live2d_count'] <= 400
    assert 150 <= l2d_su_summary['spine_count'] <= 300
    assert 180 <= nagami_summary['entry_count'] <= 300
    assert snapshots.l2d_su.metadata.url == _PRIMARY_INDEX_URL
    assert snapshots.nagami.metadata.url == NAGAMI_MAPPING_URL


def test_build_azurlane_model_catalog_merges_exact_nagami_fallback_and_prefers_l2d_su() -> None:
    snapshots = _source_snapshots(
        _characters(
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
        'by_type': {'live2d': 2, 'spine': 1, 'painting': 0},
        'by_source': {'l2d.su': 1, 'nagami': 1, 'merged': 1},
        'by_type_source': {
            'live2d': {'l2d.su': 0, 'nagami': 1, 'merged': 1},
            'spine': {'l2d.su': 1, 'nagami': 0, 'merged': 0},
            'painting': {'l2d.su': 0, 'nagami': 0, 'merged': 0},
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
        _characters(
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
        _characters(
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
        _characters(
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
        _characters(
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
        _characters(
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
    atlas_url = f'{spine_base_url}/yilisi_2_doa.atlas'
    enumeration = enumerate_azurlane_model_resources(entry, atlas_sources={atlas_url: atlas_source})

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


def test_spine_resource_manifest_defaults_to_a_single_part_named_after_the_directory() -> None:
    base_url = _spine_url('yuanchou_2')

    manifest = spine_resource_manifest(base_url)

    assert spine_parts_manifest_url(base_url) == f'{base_url}/yuanchou_2.json'
    assert manifest.parts_manifest_url == ''
    assert manifest.parts == (
        SpineModelPart(name='yuanchou_2', skeleton_url=f'{base_url}/yuanchou_2.skel', atlas_urls=(f'{base_url}/yuanchou_2.atlas',)),
    )
    assert spine_resource_manifest(f'{base_url}/yuanchou_2.atlas') == manifest


def test_parse_spine_parts_manifest_resolves_each_part_against_the_manifest_url() -> None:
    base_url = _spine_url('lafei_12')
    parts = parse_spine_parts_manifest(_spine_parts_payload('lafei_12', ('B', 'T')), parts_manifest_url=f'{base_url}/lafei_12.json')

    assert parts == (
        SpineModelPart(name='lafei_12B', skeleton_url=f'{base_url}/lafei_12B.skel', atlas_urls=(f'{base_url}/lafei_12B.atlas',)),
        SpineModelPart(name='lafei_12T', skeleton_url=f'{base_url}/lafei_12T.skel', atlas_urls=(f'{base_url}/lafei_12T.atlas',)),
    )


def test_parse_spine_parts_manifest_rejects_payloads_without_models() -> None:
    with pytest.raises(SourceSchemaError, match='models list'):
        parse_spine_parts_manifest(json.dumps({'version': 1}), parts_manifest_url='https://static.example/spine/x.json')


def test_enumerate_spine_resources_covers_every_part_of_a_multi_part_model() -> None:
    spine_base_url = _spine_url('lafei_12')
    snapshots = _source_snapshots(
        _characters(
            [
                {
                    'charId': 10117,
                    'charKey': 'lafei',
                    'charName': '拉菲',
                    'charNameEn': 'Laffey',
                    'spine': [
                        {
                            'costumeId': 131172,
                            'costumeName': '慵懒时光',
                            'costumeNameEn': 'Lazy Days',
                            'path': spine_base_url,
                        },
                    ],
                },
            ],
        ),
        {},
    )
    parts_source = _spine_parts_payload('lafei_12', ('B', 'T'))
    atlas_sources = {
        f'{spine_base_url}/lafei_12B.atlas': '\nlafei_12B.webp\nsize: 4096,4096\n',
        f'{spine_base_url}/lafei_12T.atlas': '\nlafei_12T.webp\nsize: 4096,4096\n',
    }

    catalog = build_azurlane_model_catalog(snapshots)
    entry = _catalog_entry(catalog, 'azurlane:spine:lafei:lafei_12')
    enumeration = enumerate_azurlane_model_resources(entry, spine_parts_source=parts_source, atlas_sources=atlas_sources)

    assert [(asset.kind, asset.source_url) for asset in enumeration.assets] == [
        ('spine.parts', f'{spine_base_url}/lafei_12.json'),
        ('spine.skel', f'{spine_base_url}/lafei_12B.skel'),
        ('spine.atlas', f'{spine_base_url}/lafei_12B.atlas'),
        ('spine.texture', f'{spine_base_url}/lafei_12B.webp'),
        ('spine.skel', f'{spine_base_url}/lafei_12T.skel'),
        ('spine.atlas', f'{spine_base_url}/lafei_12T.atlas'),
        ('spine.texture', f'{spine_base_url}/lafei_12T.webp'),
    ]
    assert [asset.local_path for asset in enumeration.assets][:2] == [
        'assets/spine/lafei_12/lafei_12.json',
        'assets/spine/lafei_12/lafei_12B.skel',
    ]
    assert enumeration.assets[1].context['spine_part'] == 'lafei_12B'


def test_enumerate_live2d_missing_optional_resources_does_not_fail() -> None:
    primary_url = 'https://static.example/live2d/azurlane/minimal/minimal.model3.json'
    snapshots = _source_snapshots(
        _characters(
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
        _characters(
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
        _characters(
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
        _characters(
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
        _characters(
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
    assert entry.resource_summary.skeleton == skel_url
    assert entry.resource_summary.textures == (texture_url,)
    assert {check.kind for check in report.entries[0].checks} == {'spine.skel', 'spine.atlas', 'spine.texture'}


def test_validate_catalog_resources_falls_back_to_the_spine_parts_manifest() -> None:
    spine_base_url = _spine_url('lafei_12')
    snapshots = _source_snapshots(
        _characters(
            [
                {
                    'charId': 10117,
                    'charKey': 'lafei',
                    'charName': '拉菲',
                    'charNameEn': 'Laffey',
                    'spine': [
                        {
                            'costumeId': 131172,
                            'costumeName': '慵懒时光',
                            'costumeNameEn': 'Lazy Days',
                            'path': spine_base_url,
                        },
                    ],
                },
            ],
        ),
        {},
    )
    parts_url = f'{spine_base_url}/lafei_12.json'
    part_skeletons = {f'{spine_base_url}/lafei_12B.skel', f'{spine_base_url}/lafei_12T.skel'}
    part_atlases = {f'{spine_base_url}/lafei_12B.atlas': 'lafei_12B.webp', f'{spine_base_url}/lafei_12T.atlas': 'lafei_12T.webp'}
    part_textures = {f'{spine_base_url}/lafei_12B.webp', f'{spine_base_url}/lafei_12T.webp'}
    requested: list[tuple[str, str]] = []

    def handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        requested.append((request.method, url))
        if request.method == 'HEAD' and url in part_skeletons | part_textures:
            return httpx.Response(200)
        if request.method == 'GET' and url == parts_url:
            return httpx.Response(200, text=_spine_parts_payload('lafei_12', ('B', 'T')))
        if request.method == 'GET' and url in part_atlases:
            return httpx.Response(200, text=f'\n{part_atlases[url]}\nsize: 4096,4096\nformat: RGBA8888\n')
        return httpx.Response(404)

    catalog = build_azurlane_model_catalog(snapshots)
    with httpx.Client(transport=httpx.MockTransport(handler)) as client:
        report = validate_azurlane_model_catalog_resources(catalog, client=client)

    entry = report.catalog.entries[0]
    assert requested[:2] == [('HEAD', f'{spine_base_url}/lafei_12.skel'), ('GET', parts_url)]
    assert entry.availability.state == 'valid'
    assert entry.resource_summary.skeleton == f'{spine_base_url}/lafei_12B.skel'
    assert set(entry.resource_summary.textures) == part_textures
    assert {check.kind for check in report.entries[0].checks} == {'spine.parts', 'spine.skel', 'spine.atlas', 'spine.texture'}


def test_validate_catalog_resources_checks_l2d_su_spine_suffix_assets_without_suffix() -> None:
    spine_base_url = 'https://static.example/live2d/azurlane/aerbien_4-spine'
    snapshots = _source_snapshots(
        _characters(
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
        _characters(
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
        _characters(
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
        _characters(
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
        l2d_su=replace(previous_snapshots.l2d_su, metadata=SourceFetchMetadata.for_url(_PRIMARY_INDEX_URL, http_status=200, etag='"old"')),
    )
    current_snapshots = replace(
        current_snapshots,
        l2d_su=replace(current_snapshots.l2d_su, metadata=SourceFetchMetadata.for_url(_PRIMARY_INDEX_URL, http_status=200, etag='"new"')),
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
        _characters(
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
        _PRIMARY_INDEX_URL,
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
        if str(request.url) == _PRIMARY_INDEX_URL:
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
    _skip_unless_live_source_smoke_enabled()
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

    shared_old_entry = _catalog_entry(catalog, 'azurlane:live2d:xingdengbao:xingdengbao_2')
    assert shared_old_entry.source == 'merged'
    assert shared_old_entry.resources.primary_url == _live2d_url('xingdengbao_2')
    assert shared_old_entry.resources.fallback_url == 'https://cdn.nagami.moe/live2d/xingdengbao_2/xingdengbao_2.model3.json'
    assert shared_old_entry.character.nation
    assert shared_old_entry.costume.feature_tags

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
        assert matches[0].resources.primary_url == _live2d_url(costume_key)

    assets_by_logical_key: defaultdict[tuple[str, str, str], set[str]] = defaultdict(set)
    ids_by_logical_key: defaultdict[tuple[str, str, str], set[str]] = defaultdict(set)
    for entry in catalog.entries:
        logical_key = (entry.type, entry.character.key, entry.costume.key)
        assets_by_logical_key[logical_key].add(entry.resources.primary_url)
        ids_by_logical_key[logical_key].add(entry.id)

    for logical_key, entry_ids in ids_by_logical_key.items():
        if len(entry_ids) > 1:
            assert len(assets_by_logical_key[logical_key]) == len(entry_ids)
