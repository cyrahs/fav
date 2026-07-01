# ruff: noqa: INP001, S101

from pathlib import Path

from src.tool.bd2_l2d_viewer import (
    compare_resources,
    gamekee_resources_from_detail_payload,
    gamekee_resources_from_manifest,
    parse_viewer_character_list,
    resource_stem_from_url,
    viewer_asset_paths_from_tree_payload,
    viewer_asset_url,
    viewer_resources_from_character_list,
)


def test_parse_viewer_character_list_handles_typescript_suffix() -> None:
    source = """
export default {
  "000101": {
    "charName": "Lathel",
    "costumeName": "Herb Tracker",
    "spine": "char000101",
    "cutscene": "cutscene_char000101",
    "dating": ""
  }
} as { [key: string]: {
  charName: string
} }
"""

    characters = parse_viewer_character_list(source)

    assert characters['000101']['spine'] == 'char000101'


def test_parse_viewer_character_list_handles_trailing_commas() -> None:
    source = """
export default {
  "000296": {
    "charName": "Justia",
    "costumeName": "Hot Summer Dream",
    "spine": "char000296",
    "cutscene": "cutscene_char000296",
    "dating": "illust_dating7",
    "datingUsesTracks": true,
  },
}
"""

    characters = parse_viewer_character_list(source)

    assert characters['000296']['datingUsesTracks'] is True
    assert characters['000296']['dating'] == 'illust_dating7'


def test_viewer_resources_use_tree_paths_and_report_missing_core_files() -> None:
    characters = {
        '000101': {
            'charName': 'Lathel',
            'costumeName': 'Herb Tracker',
            'spine': 'char000101',
            'cutscene': 'cutscene_char000101',
            'dating': 'illust_dating7',
        },
    }
    tree_payload = {
        'tree': [
            {'type': 'blob', 'path': 'src/assets/spines/000101/char000101.atlas'},
            {'type': 'blob', 'path': 'src/assets/spines/000101/char000101.skel'},
            {'type': 'blob', 'path': 'src/assets/spines/000101/char000101.png'},
            {'type': 'blob', 'path': 'src/assets/spines/000101/cutscene/cutscene_char000101.atlas'},
            {'type': 'blob', 'path': 'src/assets/spines/000101/dating/illust_dating7.png'},
            {'type': 'tree', 'path': 'src/assets/spines/000101/dating'},
        ],
    }

    resources = viewer_resources_from_character_list(characters, asset_paths=viewer_asset_paths_from_tree_payload(tree_payload))

    by_stem = {resource.stem: resource for resource in resources}
    assert by_stem['char000101'].files == (
        '000101/char000101.atlas',
        '000101/char000101.png',
        '000101/char000101.skel',
    )
    assert by_stem['cutscene_char000101'].missing_core_files == ('000101/cutscene/cutscene_char000101.skel',)
    assert by_stem['illust_dating7'].missing_core_files == (
        '000101/dating/illust_dating7.atlas',
        '000101/dating/illust_dating7.skel',
    )


def test_gamekee_manifest_resources_match_viewer_stems() -> None:
    manifest = {
        'content_id': 701014,
        'title': '斑鸠',
        'live2d_models': [
            {
                'live2d_key': 'xgp6bt8h',
                'urls': {
                    'atlas': 'https://cdnimg-v2.gamekee.com/wiki2.0/live2d/50118/xgp6bt8h/char021001.atlas',
                    'json': 'https://cdnimg-v2.gamekee.com/wiki2.0/live2d/50118/xgp6bt8h/char021001.json',
                    'image': ['https://cdnimg-v2.gamekee.com/wiki2.0/live2d/50118/xgp6bt8h/char021001.png'],
                },
            },
        ],
    }
    viewer = viewer_resources_from_character_list(
        {
            '021001': {
                'charName': 'Ikaruga',
                'costumeName': 'Noble Flame',
                'spine': 'char021001',
                'cutscene': '',
                'dating': '',
            },
        },
    )

    gamekee = gamekee_resources_from_manifest(manifest, source_path=Path('manifest.json').as_posix())
    result = compare_resources(gamekee_resources=gamekee, viewer_resources=viewer)

    assert result.summary()['matched_unique_stem_count'] == 1
    assert result.viewer_only == ()
    assert result.gamekee_only == ()


def test_resource_stem_from_url_strips_gamekee_nested_skel_marker() -> None:
    stem = resource_stem_from_url('https://cdn.example.com/live2d/char000102.skel.atlas')

    assert stem == 'char000102'


def test_viewer_asset_url_quotes_spine_path_segments() -> None:
    url = viewer_asset_url('061492/dating/illust dating12.atlas')

    assert url == 'https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/assets/spines/061492/dating/illust%20dating12.atlas'


def test_gamekee_detail_payload_extracts_nested_live2d_values() -> None:
    payload = {
        'data': {
            'id': 701014,
            'title': '斑鸠',
            'content_json': """
{
  "styleData": [
    {
      "data": [
        [
          {
            "type": "live2d",
            "value": {
              "live2dKey": "48w3h3yu",
              "atlas": "//cdnimg-v2.gamekee.com/wiki2.0/live2d/50118/48w3h3yu/cutscene_char021001.atlas",
              "json": "//cdnimg-v2.gamekee.com/wiki2.0/live2d/50118/48w3h3yu/cutscene_char021001.json",
              "image": "//cdnimg-v2.gamekee.com/wiki2.0/live2d/50118/48w3h3yu/cutscene_char021001.png"
            }
          }
        ]
      ]
    }
  ]
}
""",
        },
    }

    resources = gamekee_resources_from_detail_payload(payload)

    assert len(resources) == 1
    assert resources[0].stem == 'cutscene_char021001'
    assert resources[0].category == 'ultimate'
