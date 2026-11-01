# ruff: noqa: INP001, S101

"""l2d.su's hashed /al/ build (2026-10-31): digests instead of names, assets listed in the data."""

from __future__ import annotations

import json
from dataclasses import replace

from src.tool.azurlane_l2d_sources import (
    L2D_SU_STATIC_BASE_URL,
    ArchivedModel,
    AzurLaneModelCatalog,
    AzurLaneSourceSnapshots,
    L2DSuSourceSnapshot,
    ModelEntry,
    NagamiMappingEntry,
    NagamiSourceSnapshot,
    SourceFetchMetadata,
    build_azurlane_model_catalog,
    entry_painting_index_url,
    enumerate_azurlane_model_resources,
    is_hashed_asset_url,
    l2d_su_legacy_asset_urls,
    model_entry_from_dict,
    parse_l2d_su_ship_index,
    parse_l2d_su_ship_models,
    parse_painting_index,
    pin_archived_entries,
    spine_parts_manifest_url,
)

_SHIP_KEY = 'bd433116e13fbc136f40679616583bfc'
_SKIN_KEY = '5088f0d1b553fe6461ddf5b5e03bb6c2'
_FRAGMENT = f'#azp2={"ab" * 32}.{"cd" * 32}'
_MODEL3 = f'4306804565fc75b8/b0df7f36418ce73850e8aaee4165af22.model3.json{_FRAGMENT}'
_SPINE_PARTS = f'c84e225694059f5f/8b43a515c992728c1182aa75ad133adb.json{_FRAGMENT}'
_PAINTING = '18266872a5d491dc/ed096ace846cb2be8c686f0ce3a03852.webp'
_PAINTING_INDEX = f'18266872a5d491dc/{_SKIN_KEY}.json{_FRAGMENT}'


def _hashed_skin(skin_id: int, *, kind: str = 'live2d', model_path: str = '') -> dict[str, object]:
    skin: dict[str, object] = {
        'id': skin_id,
        'name': f'Skin {skin_id}',
        'skinTypeName': 'Skin',
        'featureTags': ['Live2D'],
        'painting': _SKIN_KEY,
        'prefab': _SKIN_KEY,
        'paintingKeys': [_SKIN_KEY],
        'dynamicType': kind,
        'isLive2d': kind == 'live2d',
        'isSpine': kind == 'spine',
        'paintingFaceIds': ['1'],
        'directResources': True,
        'assetPaths': {
            'squareIcon': 'bca59ce9d387212a/fa375e338bed201a0f0a81e080ceee9f.webp',
            'shipyardIcon': '3289730e1676a627/2fb0e0f5b369c2dded016ec39776f674.webp',
            'qIcon': '',
            'painting': _PAINTING,
            'paintingPaths': [f'18266872a5d491dc/{"ff" * 16}.json{_FRAGMENT}', _PAINTING_INDEX],
            'paintingFaces': [{'id': '1', 'url': '085fb1c95594c290/e0d09c7c886776d47299ab061afa1883.webp'}],
        },
    }
    if model_path:
        skin['model'] = {'type': kind, 'path': model_path}
    return skin


def _hashed_index(*skins: dict[str, object]) -> str:
    ship = {'shipGroupId': 10117, 'name': '拉菲', 'englishName': 'Laffey', 'resourceKey': _SHIP_KEY, 'skins': list(skins)}
    return json.dumps({'locale': 'CN', 'version': '9.7.416', 'ships': [ship]})


def _catalog(index_payload: str, nagami: tuple[NagamiMappingEntry, ...] = ()) -> AzurLaneModelCatalog:
    parsed = parse_l2d_su_ship_index(index_payload)
    metadata = SourceFetchMetadata.for_url('https://example.test')
    return build_azurlane_model_catalog(
        AzurLaneSourceSnapshots(
            l2d_su=L2DSuSourceSnapshot(metadata=metadata, characters=parsed.characters),
            nagami=NagamiSourceSnapshot(metadata=metadata, entries=nagami),
        ),
    )


def _entry(catalog: AzurLaneModelCatalog, model_type: str, costume_id: int) -> ModelEntry:
    return next(entry for entry in catalog.entries if entry.type == model_type and entry.costume.id == costume_id)


def test_hashed_skins_are_keyed_by_id_and_carry_their_listed_assets() -> None:
    catalog = _catalog(_hashed_index(_hashed_skin(101170)))

    live2d = _entry(catalog, 'live2d', 101170)
    assert live2d.id == 'azurlane:live2d:ship-10117:skin-101170'
    # The index has no model path for a hashed skin; the ship detail supplies it.
    assert live2d.resources.primary_url == ''

    painting = _entry(catalog, 'painting', 101170)
    assert painting.id == 'azurlane:painting:ship-10117:skin-101170'
    assert painting.resources.primary_url == f'{L2D_SU_STATIC_BASE_URL}/{_PAINTING}'
    # The skin's own painting index, not the first one listed.
    assert entry_painting_index_url(painting) == f'{L2D_SU_STATIC_BASE_URL}/{_PAINTING_INDEX}'
    assert painting.costume.square_icon == f'{L2D_SU_STATIC_BASE_URL}/bca59ce9d387212a/fa375e338bed201a0f0a81e080ceee9f.webp'
    assert painting.costume.face_urls == (('1', f'{L2D_SU_STATIC_BASE_URL}/085fb1c95594c290/e0d09c7c886776d47299ab061afa1883.webp'),)


def test_ship_detail_model_paths_keep_their_descriptor() -> None:
    detail = json.dumps({'ship': {'shipGroupId': 10117, 'skins': [_hashed_skin(101170, model_path=_MODEL3)]}})
    assert parse_l2d_su_ship_models(detail) == {101170: f'{L2D_SU_STATIC_BASE_URL}/{_MODEL3}'}


def test_hashed_live2d_files_mirror_the_cdn_tree_so_relative_references_resolve() -> None:
    catalog = _catalog(_hashed_index(_hashed_skin(101170, model_path=_MODEL3)))
    entry = _entry(catalog, 'live2d', 101170)
    assert is_hashed_asset_url(entry.resources.primary_url)
    model3 = {
        'Version': 3,
        'FileReferences': {
            'Moc': 'b921dbfa437cc17c6ae2f8338b42842c.moc3',
            'Textures': ['../a0722ee10fbfd588/fe86be236d2eab564402a664392b92ae.webp'],
            'Physics': f'ea7d76f9ea0e8318374a29c3a2dd3f1b.physics3.json{_FRAGMENT}',
            'Motions': {'idle': [{'File': f'../0fc74ef667b8ae04/edd7d80d4fb79cd9483c1b3e205339f1.motion3.json{_FRAGMENT}'}]},
        },
    }
    enumeration = enumerate_azurlane_model_resources(entry, model3_source=json.dumps(model3))
    paths = {asset.kind: (asset.local_path, asset.source_url) for asset in enumeration.assets}

    root = 'assets/live2d/skin-101170'
    assert paths['live2d.model3'][0] == f'{root}/4306804565fc75b8/b0df7f36418ce73850e8aaee4165af22.model3.json'
    assert paths['live2d.texture'] == (
        f'{root}/a0722ee10fbfd588/fe86be236d2eab564402a664392b92ae.webp',
        f'{L2D_SU_STATIC_BASE_URL}/a0722ee10fbfd588/fe86be236d2eab564402a664392b92ae.webp',
    )
    # The descriptor of an encrypted reference travels with its URL, so the downloader can open it.
    assert paths['live2d.motion'][1].endswith(f'0fc74ef667b8ae04/edd7d80d4fb79cd9483c1b3e205339f1.motion3.json{_FRAGMENT}')
    assert paths['live2d.physics'][0] == f'{root}/4306804565fc75b8/ea7d76f9ea0e8318374a29c3a2dd3f1b.physics3.json'
    # Hashed names never existed under an older root.
    assert l2d_su_legacy_asset_urls(paths['live2d.texture'][1]) == ()


def test_hashed_spine_skin_reads_its_parts_manifest_directly() -> None:
    catalog = _catalog(_hashed_index(_hashed_skin(100020, kind='spine', model_path=_SPINE_PARTS)))
    entry = _entry(catalog, 'spine', 100020)
    assert spine_parts_manifest_url(entry.resources.primary_url) == entry.resources.primary_url

    parts = {
        'models': [
            {
                'name': 'buli_super',
                'skeleton': '42697f73a2caf5733ec4fa07ec35a7db.skel',
                'atlases': [f'9b80cbed7cd0b0daa38850db3972caaf.atlas{_FRAGMENT}'],
            },
        ],
    }
    atlas_url = f'{L2D_SU_STATIC_BASE_URL}/c84e225694059f5f/9b80cbed7cd0b0daa38850db3972caaf.atlas{_FRAGMENT}'
    atlas = '\n81d3df702499daad4679596f9c74ad5c.webp\nsize: 4096,2048\nformat: RGBA8888\n'
    enumeration = enumerate_azurlane_model_resources(entry, spine_parts_source=json.dumps(parts), atlas_sources={atlas_url: atlas})
    local_paths = {asset.kind: asset.local_path for asset in enumeration.assets}
    assert local_paths == {
        'spine.parts': 'assets/spine/skin-100020/c84e225694059f5f/8b43a515c992728c1182aa75ad133adb.json',
        'spine.skel': 'assets/spine/skin-100020/c84e225694059f5f/42697f73a2caf5733ec4fa07ec35a7db.skel',
        'spine.atlas': 'assets/spine/skin-100020/c84e225694059f5f/9b80cbed7cd0b0daa38850db3972caaf.atlas',
        'spine.texture': 'assets/spine/skin-100020/c84e225694059f5f/81d3df702499daad4679596f9c74ad5c.webp',
    }


def test_hashed_painting_takes_sheets_and_meshes_from_its_index() -> None:
    catalog = _catalog(_hashed_index(_hashed_skin(101171)))
    entry = _entry(catalog, 'painting', 101171)
    index = {
        _SKIN_KEY: {
            'size': [762, 1024],
            'image': 'ed096ace846cb2be8c686f0ce3a03852.webp',
            'mesh': f'ac98db33fcf298b8d6f3740cf5fed3ce.obj{_FRAGMENT}',
        },
        'aa' * 16: {'size': [100, 100], 'image': '529101daf320b2c1d398e18c6ab5bcac.webp', 'raw': True},
        'face': {'size': [100, 100], 'pivot': [0.5, 0.5], 'position': [0, 0]},
    }
    painting_index = parse_painting_index(
        json.dumps(index),
        painting_url=entry.resources.primary_url,
        index_url=entry.resources.painting_index_url,
    )
    assert painting_index.painting_key == _SKIN_KEY
    assert painting_index.face is not None

    enumeration = enumerate_azurlane_model_resources(entry, painting_index=painting_index)
    by_kind: dict[str, list[str]] = {}
    for asset in enumeration.assets:
        by_kind.setdefault(asset.kind, []).append(asset.source_url)
    root = L2D_SU_STATIC_BASE_URL
    # The primary sheet is the painting image itself, so only the second layer is a layer of its own.
    assert by_kind['painting.layer'] == [f'{root}/18266872a5d491dc/529101daf320b2c1d398e18c6ab5bcac.webp']
    # A raw layer has no mesh.
    assert by_kind['painting.mesh'] == [f'{root}/18266872a5d491dc/ac98db33fcf298b8d6f3740cf5fed3ce.obj{_FRAGMENT}']
    assert by_kind['painting.face'] == [f'{root}/085fb1c95594c290/e0d09c7c886776d47299ab061afa1883.webp']
    assert by_kind['icon.square'] == [f'{root}/bca59ce9d387212a/fa375e338bed201a0f0a81e080ceee9f.webp']
    local = {asset.kind: asset.local_path for asset in enumeration.assets}
    assert local['painting.image'] == f'assets/painting/skin-101171/{_PAINTING}'
    assert local['painting.index'] == f'assets/painting/skin-101171/18266872a5d491dc/{_SKIN_KEY}.json'


def _archived(entry: ModelEntry, *, completed: bool = True) -> ArchivedModel:
    """An archive row as the crawler reads it back: the catalog entry round-tripped through JSON."""
    restored = model_entry_from_dict(json.loads(json.dumps(entry.to_dict())))
    assert restored is not None
    return ArchivedModel(entry=restored, completed=completed)


def _legacy_entry(model_type: str, costume_id: int, key: str) -> ModelEntry:
    legacy_index = json.dumps(
        {
            'locale': 'CN',
            'ships': [
                {
                    'shipGroupId': 10117,
                    'name': '拉菲',
                    'resourceKey': 'lafei',
                    'skins': [
                        {
                            'id': costume_id,
                            'name': 'Old',
                            'painting': key,
                            'prefab': key,
                            'dynamicType': 'live2d',
                            'isLive2d': True,
                            'paintingFaceIds': ['1'],
                            'assetPaths': {'squareIcon': f'squareicon/{key}', 'shipyardIcon': f'shipyardicon/{key}'},
                        },
                    ],
                },
            ],
        },
    )
    entry = _entry(_catalog(legacy_index), model_type, costume_id)
    # As archived from /al-b/ before the hashed build.
    return replace(entry, resources=replace(entry.resources, primary_url=entry.resources.primary_url.replace('/al/', '/al-b/')))


def test_completed_archive_models_keep_their_names_and_readable_files() -> None:
    legacy_live2d = _legacy_entry('live2d', 101170, 'lafei_4')
    legacy_painting = _legacy_entry('painting', 101170, 'lafei_4')
    assert legacy_live2d.id == 'azurlane:live2d:lafei:lafei_4'

    catalog = _catalog(_hashed_index(_hashed_skin(101170), _hashed_skin(101179)))
    pinned = pin_archived_entries(catalog, [_archived(legacy_live2d), _archived(legacy_painting)])

    live2d = _entry(pinned, 'live2d', 101170)
    assert live2d.id == legacy_live2d.id
    assert live2d.resources == legacy_live2d.resources
    assert live2d.costume.name_zh == 'Skin 101170'

    painting = _entry(pinned, 'painting', 101170)
    assert painting.id == legacy_painting.id
    assert painting.resources.primary_url == legacy_painting.resources.primary_url
    assert painting.costume.square_icon == 'squareicon/lafei_4'
    assert painting.costume.face_urls == ()

    # A new skin of the same ship joins its directory.
    new_skin = _entry(pinned, 'live2d', 101179)
    assert new_skin.id == 'azurlane:live2d:lafei:skin-101179'
    assert new_skin.character.key == 'lafei'


def test_an_unfinished_archive_model_keeps_its_name_but_takes_the_hashed_files() -> None:
    legacy_painting = _legacy_entry('painting', 101170, 'lafei_4')
    catalog = _catalog(_hashed_index(_hashed_skin(101170)))
    pinned = pin_archived_entries(catalog, [_archived(legacy_painting, completed=False)])

    painting = _entry(pinned, 'painting', 101170)
    assert painting.id == legacy_painting.id
    assert painting.resources.primary_url == f'{L2D_SU_STATIC_BASE_URL}/{_PAINTING}'
    assert painting.costume.face_urls


def test_nagami_models_are_only_kept_where_already_archived_on_their_own() -> None:
    nagami = (
        NagamiMappingEntry(key='lafei_4', name='Laffey - Old'),
        NagamiMappingEntry(key='nagami_only', name='Someone'),
    )
    catalog = _catalog(_hashed_index(_hashed_skin(101170)), nagami=nagami)
    nagami_entries = {entry.id: entry for entry in catalog.entries if entry.source == 'nagami'}
    assert len(nagami_entries) == len(nagami)
    archived_only = next(entry for entry in nagami_entries.values() if entry.costume.key == 'nagami_only')

    pinned = pin_archived_entries(catalog, [_archived(archived_only)])
    assert [entry.id for entry in pinned.entries if entry.source == 'nagami'] == [archived_only.id]
