# ruff: noqa: INP001, S101, PLR2004

from __future__ import annotations

import json
from datetime import datetime

import httpx
import pytest

from src.tool.azurlane_l2d_sources import (
    L2D_SU_CATALOG_URL,
    NAGAMI_MAPPING_BUNDLE_URL,
    SourceSchemaError,
    fetch_l2d_su_snapshot,
    fetch_nagami_snapshot,
    fetch_source_snapshots,
    parse_l2d_su_catalog,
    parse_nagami_mapping_bundle,
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
