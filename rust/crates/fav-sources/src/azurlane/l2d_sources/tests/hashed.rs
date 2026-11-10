//! l2d.su's hashed /al/ build (2026-10-31): digests instead of names, assets listed in the data.

use std::collections::{BTreeMap, HashMap};

use fav_core::pyjson::{Value, loads};

use super::super::{
    ArchivedModel, AzurLaneModelCatalog, AzurLaneSourceSnapshots, CatalogSource, EnumerationInputs, L2D_SU_STATIC_BASE_URL,
    L2DSuSourceSnapshot, ModelEntry, ModelKind, NagamiMappingEntry, NagamiSourceSnapshot, SourceFetchMetadata, build_azurlane_model_catalog,
    entry_painting_index_url, enumerate_azurlane_model_resources, is_hashed_asset_url, l2d_su_legacy_asset_urls, model_entry_from_dict,
    parse_l2d_su_ship_index, parse_l2d_su_ship_models, parse_painting_index, pin_archived_entries, spine_parts_manifest_url,
};
use super::{clock, json, set};

const SHIP_KEY: &str = "bd433116e13fbc136f40679616583bfc";
const SKIN_KEY: &str = "5088f0d1b553fe6461ddf5b5e03bb6c2";

fn fragment() -> String {
    format!("#azp2={}.{}", "ab".repeat(32), "cd".repeat(32))
}

fn model3() -> String {
    format!("4306804565fc75b8/b0df7f36418ce73850e8aaee4165af22.model3.json{}", fragment())
}

fn spine_parts() -> String {
    format!("c84e225694059f5f/8b43a515c992728c1182aa75ad133adb.json{}", fragment())
}

const PAINTING: &str = "18266872a5d491dc/ed096ace846cb2be8c686f0ce3a03852.webp";

fn painting_index() -> String {
    format!("18266872a5d491dc/{SKIN_KEY}.json{}", fragment())
}

fn root() -> &'static str {
    L2D_SU_STATIC_BASE_URL
}

/// `_hashed_skin(skin_id, kind=kind, model_path=model_path)`.
fn hashed_skin(skin_id: i32, kind: &str, model_path: &str) -> Value {
    let skin = obj! {
        "id" => skin_id,
        "name" => format!("Skin {skin_id}"),
        "skinTypeName" => "Skin",
        "featureTags" => vec!["Live2D"],
        "painting" => SKIN_KEY,
        "prefab" => SKIN_KEY,
        "paintingKeys" => vec![SKIN_KEY],
        "dynamicType" => kind,
        "isLive2d" => kind == "live2d",
        "isSpine" => kind == "spine",
        "paintingFaceIds" => vec!["1"],
        "directResources" => true,
        "assetPaths" => obj! {
            "squareIcon" => "bca59ce9d387212a/fa375e338bed201a0f0a81e080ceee9f.webp",
            "shipyardIcon" => "3289730e1676a627/2fb0e0f5b369c2dded016ec39776f674.webp",
            "qIcon" => "",
            "painting" => PAINTING,
            "paintingPaths" => vec![format!("18266872a5d491dc/{}.json{}", "ff".repeat(16), fragment()), painting_index()],
            "paintingFaces" => list![obj! {"id" => "1", "url" => "085fb1c95594c290/e0d09c7c886776d47299ab061afa1883.webp"}],
        },
    };
    if model_path.is_empty() {
        skin
    } else {
        set(skin, "model", obj! {"type" => kind, "path" => model_path})
    }
}

/// `_hashed_index(*skins)`.
fn hashed_index(skins: Vec<Value>) -> String {
    let ship = obj! {"shipGroupId" => 10117, "name" => "拉菲", "englishName" => "Laffey", "resourceKey" => SHIP_KEY, "skins" => skins};
    json(&obj! {"locale" => "CN", "version" => "9.7.416", "ships" => list![ship]})
}

/// `_catalog(index_payload, nagami)`.
fn catalog_of(index_payload: &str, nagami: Vec<NagamiMappingEntry>) -> AzurLaneModelCatalog {
    let parsed = parse_l2d_su_ship_index(index_payload, "CN").unwrap();
    let metadata = SourceFetchMetadata::for_url("https://example.test", clock());
    let mut l2d_su = L2DSuSourceSnapshot::new(metadata.clone());
    l2d_su.characters = parsed.characters;
    let mut nagami_snapshot = NagamiSourceSnapshot::new(metadata);
    nagami_snapshot.entries = nagami;
    build_azurlane_model_catalog(&AzurLaneSourceSnapshots {
        l2d_su,
        nagami: nagami_snapshot,
    })
    .unwrap()
}

/// `_entry(catalog, model_type, costume_id)`.
fn entry_of(catalog: &AzurLaneModelCatalog, model_type: ModelKind, costume_id: i128) -> &ModelEntry {
    catalog
        .entries
        .iter()
        .find(|entry| entry.r#type == model_type && entry.costume.id == Some(costume_id))
        .unwrap_or_else(|| panic!("no {model_type} entry for {costume_id}"))
}

// py: tests/test_azurlane_hashed_build.py::test_hashed_skins_are_keyed_by_id_and_carry_their_listed_assets
#[test]
fn hashed_skins_are_keyed_by_id_and_carry_their_listed_assets() {
    let catalog = catalog_of(&hashed_index(vec![hashed_skin(101_170, "live2d", "")]), Vec::new());

    let live2d = entry_of(&catalog, ModelKind::Live2d, 101_170);
    assert_eq!(live2d.id, "azurlane:live2d:ship-10117:skin-101170");
    // The index has no model path for a hashed skin; the ship detail supplies it.
    assert_eq!(live2d.resources.primary_url, "");

    let painting = entry_of(&catalog, ModelKind::Painting, 101_170);
    assert_eq!(painting.id, "azurlane:painting:ship-10117:skin-101170");
    assert_eq!(painting.resources.primary_url, format!("{}/{PAINTING}", root()));
    // The skin's own painting index, not the first one listed.
    assert_eq!(
        entry_painting_index_url(painting).unwrap(),
        format!("{}/{}", root(), painting_index())
    );
    assert_eq!(
        painting.costume.square_icon,
        format!("{}/bca59ce9d387212a/fa375e338bed201a0f0a81e080ceee9f.webp", root())
    );
    assert_eq!(
        painting.costume.face_urls,
        [(
            "1".to_owned(),
            format!("{}/085fb1c95594c290/e0d09c7c886776d47299ab061afa1883.webp", root())
        )]
    );
}

// py: tests/test_azurlane_hashed_build.py::test_ship_detail_model_paths_keep_their_descriptor
#[test]
fn ship_detail_model_paths_keep_their_descriptor() {
    let detail = json(&obj! {"ship" => obj! {"shipGroupId" => 10117, "skins" => list![hashed_skin(101_170, "live2d", &model3())]}});

    assert_eq!(
        parse_l2d_su_ship_models(&detail).unwrap(),
        [(101_170, format!("{}/{}", root(), model3()))]
    );
}

// py: tests/test_azurlane_hashed_build.py::test_hashed_live2d_files_mirror_the_cdn_tree_so_relative_references_resolve
#[test]
fn hashed_live2d_files_mirror_the_cdn_tree_so_relative_references_resolve() {
    let catalog = catalog_of(&hashed_index(vec![hashed_skin(101_170, "live2d", &model3())]), Vec::new());
    let entry = entry_of(&catalog, ModelKind::Live2d, 101_170);
    assert!(is_hashed_asset_url(&entry.resources.primary_url));
    let fragment = fragment();
    let model3_source = json(&obj! {
        "Version" => 3,
        "FileReferences" => obj! {
            "Moc" => "b921dbfa437cc17c6ae2f8338b42842c.moc3",
            "Textures" => vec!["../a0722ee10fbfd588/fe86be236d2eab564402a664392b92ae.webp"],
            "Physics" => format!("ea7d76f9ea0e8318374a29c3a2dd3f1b.physics3.json{fragment}"),
            "Motions" => obj! {"idle" => list![obj! {"File" => format!("../0fc74ef667b8ae04/edd7d80d4fb79cd9483c1b3e205339f1.motion3.json{fragment}")}]},
        },
    });

    let enumeration = enumerate_azurlane_model_resources(
        entry,
        EnumerationInputs {
            model3_source: Some(&model3_source),
            ..EnumerationInputs::default()
        },
    )
    .unwrap();
    // `{asset.kind: (local_path, source_url)}`: the last asset of a kind wins.
    let paths: HashMap<&str, (String, String)> = enumeration
        .assets
        .iter()
        .map(|asset| (asset.kind.as_str(), (asset.local_path.clone(), asset.source_url.clone())))
        .collect();

    let local_root = "assets/live2d/skin-101170";
    assert_eq!(
        paths["live2d.model3"].0,
        format!("{local_root}/4306804565fc75b8/b0df7f36418ce73850e8aaee4165af22.model3.json")
    );
    assert_eq!(
        paths["live2d.texture"],
        (
            format!("{local_root}/a0722ee10fbfd588/fe86be236d2eab564402a664392b92ae.webp"),
            format!("{}/a0722ee10fbfd588/fe86be236d2eab564402a664392b92ae.webp", root()),
        )
    );
    // The descriptor of an encrypted reference travels with its URL, so the downloader can open it.
    assert!(
        paths["live2d.motion"]
            .1
            .ends_with(&format!("0fc74ef667b8ae04/edd7d80d4fb79cd9483c1b3e205339f1.motion3.json{fragment}"))
    );
    assert_eq!(
        paths["live2d.physics"].0,
        format!("{local_root}/4306804565fc75b8/ea7d76f9ea0e8318374a29c3a2dd3f1b.physics3.json")
    );
    // Hashed names never existed under an older root.
    assert!(l2d_su_legacy_asset_urls(&paths["live2d.texture"].1).is_empty());
}

// py: tests/test_azurlane_hashed_build.py::test_hashed_spine_skin_reads_its_parts_manifest_directly
#[test]
fn a_hashed_spine_skin_reads_its_parts_manifest_directly() {
    let catalog = catalog_of(&hashed_index(vec![hashed_skin(100_020, "spine", &spine_parts())]), Vec::new());
    let entry = entry_of(&catalog, ModelKind::Spine, 100_020);
    assert_eq!(
        spine_parts_manifest_url(&entry.resources.primary_url).unwrap(),
        entry.resources.primary_url
    );
    let fragment = fragment();
    let parts = json(&obj! {
        "models" => list![obj! {
            "name" => "buli_super",
            "skeleton" => "42697f73a2caf5733ec4fa07ec35a7db.skel",
            "atlases" => vec![format!("9b80cbed7cd0b0daa38850db3972caaf.atlas{fragment}")],
        }],
    });
    let atlas_url = format!("{}/c84e225694059f5f/9b80cbed7cd0b0daa38850db3972caaf.atlas{fragment}", root());
    let atlas_sources = HashMap::from([(
        atlas_url,
        "\n81d3df702499daad4679596f9c74ad5c.webp\nsize: 4096,2048\nformat: RGBA8888\n".to_owned(),
    )]);

    let enumeration = enumerate_azurlane_model_resources(
        entry,
        EnumerationInputs {
            spine_parts_source: Some(&parts),
            atlas_sources: Some(&atlas_sources),
            ..EnumerationInputs::default()
        },
    )
    .unwrap();

    let local_paths: BTreeMap<&str, &str> = enumeration
        .assets
        .iter()
        .map(|asset| (asset.kind.as_str(), asset.local_path.as_str()))
        .collect();
    assert_eq!(
        local_paths,
        BTreeMap::from([
            (
                "spine.parts",
                "assets/spine/skin-100020/c84e225694059f5f/8b43a515c992728c1182aa75ad133adb.json"
            ),
            (
                "spine.skel",
                "assets/spine/skin-100020/c84e225694059f5f/42697f73a2caf5733ec4fa07ec35a7db.skel"
            ),
            (
                "spine.atlas",
                "assets/spine/skin-100020/c84e225694059f5f/9b80cbed7cd0b0daa38850db3972caaf.atlas"
            ),
            (
                "spine.texture",
                "assets/spine/skin-100020/c84e225694059f5f/81d3df702499daad4679596f9c74ad5c.webp"
            ),
        ])
    );
}

// py: tests/test_azurlane_hashed_build.py::test_hashed_painting_takes_sheets_and_meshes_from_its_index
#[test]
fn a_hashed_painting_takes_sheets_and_meshes_from_its_index() {
    let catalog = catalog_of(&hashed_index(vec![hashed_skin(101_171, "live2d", "")]), Vec::new());
    let entry = entry_of(&catalog, ModelKind::Painting, 101_171);
    let fragment = fragment();
    let mut index = fav_core::pyjson::Dict::new();
    index.insert(
        SKIN_KEY,
        obj! {
            "size" => vec![762, 1024],
            "image" => "ed096ace846cb2be8c686f0ce3a03852.webp",
            "mesh" => format!("ac98db33fcf298b8d6f3740cf5fed3ce.obj{fragment}"),
        },
    );
    index.insert(
        "aa".repeat(16),
        obj! {"size" => vec![100, 100], "image" => "529101daf320b2c1d398e18c6ab5bcac.webp", "raw" => true},
    );
    index.insert(
        "face",
        obj! {"size" => vec![100, 100], "pivot" => vec![0.5, 0.5], "position" => vec![0, 0]},
    );
    let painting_index = parse_painting_index(
        &json(&Value::Dict(index)),
        &entry.resources.primary_url,
        &entry.resources.painting_index_url,
    )
    .unwrap();
    assert_eq!(painting_index.painting_key, SKIN_KEY);
    assert!(painting_index.face.is_some());

    let enumeration = enumerate_azurlane_model_resources(
        entry,
        EnumerationInputs {
            painting_index: Some(&painting_index),
            ..EnumerationInputs::default()
        },
    )
    .unwrap();

    let mut by_kind: HashMap<&str, Vec<String>> = HashMap::new();
    for asset in &enumeration.assets {
        by_kind.entry(asset.kind.as_str()).or_default().push(asset.source_url.clone());
    }
    // The primary sheet is the painting image itself, so only the second layer is a layer of its own.
    assert_eq!(
        by_kind["painting.layer"],
        [format!("{}/18266872a5d491dc/529101daf320b2c1d398e18c6ab5bcac.webp", root())]
    );
    // A raw layer has no mesh.
    assert_eq!(
        by_kind["painting.mesh"],
        [format!("{}/18266872a5d491dc/ac98db33fcf298b8d6f3740cf5fed3ce.obj{fragment}", root())]
    );
    assert_eq!(
        by_kind["painting.face"],
        [format!("{}/085fb1c95594c290/e0d09c7c886776d47299ab061afa1883.webp", root())]
    );
    assert_eq!(
        by_kind["icon.square"],
        [format!("{}/bca59ce9d387212a/fa375e338bed201a0f0a81e080ceee9f.webp", root())]
    );
    let local: HashMap<&str, &str> = enumeration
        .assets
        .iter()
        .map(|asset| (asset.kind.as_str(), asset.local_path.as_str()))
        .collect();
    assert_eq!(local["painting.image"], format!("assets/painting/skin-101171/{PAINTING}"));
    assert_eq!(
        local["painting.index"],
        format!("assets/painting/skin-101171/18266872a5d491dc/{SKIN_KEY}.json")
    );
}

/// `_archived(entry, completed=...)`: an archive row as the crawler reads it back, the
/// catalog entry round-tripped through JSON.
fn archived(entry: &ModelEntry, completed: bool) -> ArchivedModel {
    let restored = model_entry_from_dict(&loads(&json(&entry.to_dict())).unwrap()).expect("the entry round-trips");
    ArchivedModel {
        entry: restored,
        completed,
    }
}

/// `_legacy_entry(model_type, costume_id, key)`: as archived from /al-b/ before the hashed build.
fn legacy_entry(model_type: ModelKind, costume_id: i32, key: &str) -> ModelEntry {
    let legacy_index = json(&obj! {
        "locale" => "CN",
        "ships" => list![obj! {
            "shipGroupId" => 10117,
            "name" => "拉菲",
            "resourceKey" => "lafei",
            "skins" => list![obj! {
                "id" => costume_id,
                "name" => "Old",
                "painting" => key,
                "prefab" => key,
                "dynamicType" => "live2d",
                "isLive2d" => true,
                "paintingFaceIds" => vec!["1"],
                "assetPaths" => obj! {"squareIcon" => format!("squareicon/{key}"), "shipyardIcon" => format!("shipyardicon/{key}")},
            }],
        }],
    });
    let mut entry = entry_of(&catalog_of(&legacy_index, Vec::new()), model_type, i128::from(costume_id)).clone();
    entry.resources.primary_url = entry.resources.primary_url.replace("/al/", "/al-b/");
    entry
}

// py: tests/test_azurlane_hashed_build.py::test_completed_archive_models_keep_their_names_and_readable_files
#[test]
fn completed_archive_models_keep_their_names_and_readable_files() {
    let legacy_live2d = legacy_entry(ModelKind::Live2d, 101_170, "lafei_4");
    let legacy_painting = legacy_entry(ModelKind::Painting, 101_170, "lafei_4");
    assert_eq!(legacy_live2d.id, "azurlane:live2d:lafei:lafei_4");

    let catalog = catalog_of(
        &hashed_index(vec![hashed_skin(101_170, "live2d", ""), hashed_skin(101_179, "live2d", "")]),
        Vec::new(),
    );
    let pinned = pin_archived_entries(&catalog, &[archived(&legacy_live2d, true), archived(&legacy_painting, true)]);

    let live2d = entry_of(&pinned, ModelKind::Live2d, 101_170);
    assert_eq!(live2d.id, legacy_live2d.id);
    assert_eq!(live2d.resources, legacy_live2d.resources);
    assert_eq!(live2d.costume.name_zh, "Skin 101170");

    let painting = entry_of(&pinned, ModelKind::Painting, 101_170);
    assert_eq!(painting.id, legacy_painting.id);
    assert_eq!(painting.resources.primary_url, legacy_painting.resources.primary_url);
    assert_eq!(painting.costume.square_icon, "squareicon/lafei_4");
    assert!(painting.costume.face_urls.is_empty());

    // A new skin of the same ship joins its directory.
    let new_skin = entry_of(&pinned, ModelKind::Live2d, 101_179);
    assert_eq!(new_skin.id, "azurlane:live2d:lafei:skin-101179");
    assert_eq!(new_skin.character.key, "lafei");
}

// py: tests/test_azurlane_hashed_build.py::test_an_unfinished_archive_model_keeps_its_name_but_takes_the_hashed_files
#[test]
fn an_unfinished_archive_model_keeps_its_name_but_takes_the_hashed_files() {
    let legacy_painting = legacy_entry(ModelKind::Painting, 101_170, "lafei_4");
    let catalog = catalog_of(&hashed_index(vec![hashed_skin(101_170, "live2d", "")]), Vec::new());

    let pinned = pin_archived_entries(&catalog, &[archived(&legacy_painting, false)]);

    let painting = entry_of(&pinned, ModelKind::Painting, 101_170);
    assert_eq!(painting.id, legacy_painting.id);
    assert_eq!(painting.resources.primary_url, format!("{}/{PAINTING}", root()));
    assert!(!painting.costume.face_urls.is_empty());
}

// py: tests/test_azurlane_hashed_build.py::test_nagami_models_are_only_kept_where_already_archived_on_their_own
#[test]
fn nagami_models_are_only_kept_where_already_archived_on_their_own() {
    let nagami_entry = |key: &str, name: &str| NagamiMappingEntry {
        key: key.to_owned(),
        name: name.to_owned(),
        background: String::new(),
    };
    let nagami = vec![nagami_entry("lafei_4", "Laffey - Old"), nagami_entry("nagami_only", "Someone")];
    let catalog = catalog_of(&hashed_index(vec![hashed_skin(101_170, "live2d", "")]), nagami.clone());
    let nagami_entries: Vec<&ModelEntry> = catalog
        .entries
        .iter()
        .filter(|entry| entry.source == CatalogSource::Nagami)
        .collect();
    assert_eq!(nagami_entries.len(), nagami.len());
    let archived_only = nagami_entries
        .iter()
        .find(|entry| entry.costume.key == "nagami_only")
        .unwrap();

    let pinned = pin_archived_entries(&catalog, &[archived(archived_only, true)]);

    let kept: Vec<&str> = pinned
        .entries
        .iter()
        .filter(|entry| entry.source == CatalogSource::Nagami)
        .map(|entry| entry.id.as_str())
        .collect();
    assert_eq!(kept, [archived_only.id.as_str()]);
}
