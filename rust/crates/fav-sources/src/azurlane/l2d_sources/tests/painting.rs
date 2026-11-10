//! Paintings: the layer index, the case variants of their URLs, and their resources.

use std::collections::{BTreeMap, HashMap};

use fav_core::pyjson::Value;

use super::super::{
    AssetKind, AzurLaneEnumeratedResource, AzurLaneModelCatalog, EnumerationInputs, L2D_SU_AUDIO_BASE_URL, L2D_SU_STATIC_BASE_URL,
    SOURCE_SCHEMA_ERROR, apply_l2d_su_model_paths, build_azurlane_model_catalog, case_variant_urls, enumerate_azurlane_model_resources,
    model_probe_urls, painting_index_url, parse_l2d_su_ship_index, parse_l2d_su_ship_voices, parse_painting_index,
};
use super::{catalog_entry, index_payload, json, live2d_url, ship, skin, source_snapshots, static_skin};

fn base() -> &'static str {
    L2D_SU_STATIC_BASE_URL
}

/// `build_azurlane_model_catalog(_source_snapshots(parse_l2d_su_ship_index(payload).characters, {}))`.
pub(super) fn catalog_of(payload: &str) -> AzurLaneModelCatalog {
    let parsed = parse_l2d_su_ship_index(payload, "CN").unwrap();
    build_azurlane_model_catalog(&source_snapshots(parsed.characters, &[])).unwrap()
}

/// `by_kind[asset.kind].append(asset)`.
pub(super) fn by_kind(assets: &[AzurLaneEnumeratedResource]) -> BTreeMap<&'static str, Vec<&AzurLaneEnumeratedResource>> {
    let mut by_kind: BTreeMap<&'static str, Vec<&AzurLaneEnumeratedResource>> = BTreeMap::new();
    for asset in assets {
        by_kind.entry(asset.kind.as_str()).or_default().push(asset);
    }
    by_kind
}

pub(super) fn source_urls(assets: Option<&Vec<&AzurLaneEnumeratedResource>>) -> Vec<String> {
    assets
        .map(|assets| assets.iter().map(|asset| asset.source_url.clone()).collect())
        .unwrap_or_default()
}

fn biaoqiang_catalog() -> AzurLaneModelCatalog {
    catalog_of(&index_payload(vec![ship(1, "biaoqiang", "标枪", "", vec![static_skin(10, "标枪", "biaoqiang")])]))
}

// py: tests/test_azurlane_l2d_sources.py::test_enumerate_painting_resources_covers_image_faces_icons_and_voices
#[test]
fn painting_resources_cover_the_image_faces_icons_and_voices() {
    let catalog = biaoqiang_catalog();
    let entry = catalog_entry(&catalog, "azurlane:painting:biaoqiang:biaoqiang");
    let detail = json(&obj! {
        "ship" => obj! {"shipGroupId" => 1, "skins" => list![obj! {"id" => 10, "words" => list![obj! {"key" => "detail", "voicePath" => "cue/cv-1/detail"}]}]},
    });
    let voices = parse_l2d_su_ship_voices(&detail).unwrap();
    let voice_lines = &voices.iter().find(|(id, _)| *id == 10).unwrap().1;

    let enumeration = enumerate_azurlane_model_resources(
        entry,
        EnumerationInputs {
            voice_lines,
            ..EnumerationInputs::default()
        },
    )
    .unwrap();

    let by_kind = by_kind(&enumeration.assets);
    // No icon.q: the index advertises qicon/<key> for every skin but the CDN hosts none of them.
    assert_eq!(
        by_kind.keys().copied().collect::<Vec<_>>(),
        ["icon.shipyard", "icon.square", "painting.face", "painting.image", "voice.audio"]
    );
    let image = by_kind["painting.image"][0];
    assert_eq!(image.source_url, format!("{}/painting/biaoqiang.webp", base()));
    assert_eq!(image.local_path, "assets/painting/biaoqiang/biaoqiang.webp");
    assert_eq!(
        source_urls(by_kind.get("painting.face")),
        [
            format!("{}/paintingface/biaoqiang/1.webp", base()),
            format!("{}/paintingface/biaoqiang/2.webp", base()),
        ]
    );
    assert_eq!(
        by_kind["painting.face"][0].local_path,
        "assets/painting/biaoqiang/paintingface/biaoqiang/1.webp"
    );
    assert_eq!(by_kind["icon.square"][0].source_url, format!("{}/squareicon/biaoqiang.webp", base()));
    let voice = by_kind["voice.audio"][0];
    assert_eq!(voice.source_url, format!("{L2D_SU_AUDIO_BASE_URL}/cue/cv-1/detail.ogg"));
    assert_eq!(voice.local_path, "assets/painting/biaoqiang/cue/cv-1/detail.ogg");
    assert_eq!(voice.context().get("text"), Some(&Value::Str(String::new())));
    assert_eq!(voice.context().get("key"), Some(&Value::Str("detail".to_owned())));
}

/// `_xiafei_index_payload`: a real multi-layer index; `_rw` is the character, `_bj` the
/// background, `raw` the flat sheet.
fn xiafei_index_payload() -> String {
    json(&obj! {
        "xiafei_4" => obj! {"size" => vec![4574, 2866], "pivot" => vec![0.490_676, 0.666_504], "position" => vec![0, 0], "rawSize" => vec![2048, 1283], "raw" => true},
        "xiafei_4_rw" => obj! {"size" => vec![2000, 2048], "pivot" => vec![0.490_676, 0.666_504], "position" => vec![10, 272], "rawSize" => vec![2000, 2048]},
        "xiafei_4_bj" => obj! {"size" => vec![4574, 2866], "pivot" => vec![0.490_676, 0.666_504], "position" => vec![-35, 478], "rawSize" => vec![2048, 1283]},
        "face" => obj! {"size" => vec![372, 374], "pivot" => vec![0.5, 0.5], "position" => vec![-406.4, 596.8]},
    })
}

// py: tests/test_azurlane_l2d_sources.py::test_painting_index_url_sits_beside_the_sheet
#[test]
fn the_painting_index_sits_beside_the_sheet() {
    assert_eq!(
        painting_index_url(&format!("{}/painting/xiafei_4.webp", base())).unwrap(),
        format!("{}/painting/xiafei_4.json", base())
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_painting_index_keeps_layer_order_and_splits_off_the_face_slot
#[test]
fn the_painting_index_keeps_layer_order_and_splits_off_the_face_slot() {
    let index = parse_painting_index(&xiafei_index_payload(), &format!("{}/painting/xiafei_4.webp", base()), "").unwrap();

    assert_eq!(index.painting_key, "xiafei_4");
    assert_eq!(index.index_url, format!("{}/painting/xiafei_4.json", base()));
    // Source order decides which layer draws over which, and a jsonb object would not keep it.
    let names: Vec<&str> = index.layers.iter().map(|layer| layer.name.as_str()).collect();
    assert_eq!(names, ["xiafei_4", "xiafei_4_rw", "xiafei_4_bj"]);
    assert!(index.layers[0].raw);
    assert!(!index.layers[1].raw);
    assert_eq!(index.layers[1].size, (2000.0, 2048.0));
    assert_eq!(index.layers[1].position, (10.0, 272.0));
    let face = index.face.expect("the face slot");
    assert_eq!(face.size, (372.0, 374.0));
    assert_eq!(face.position, (-406.4, 596.8));
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_painting_index_defaults_raw_size_to_the_declared_size
#[test]
fn a_layer_without_a_raw_size_takes_its_declared_size() {
    let payload = json(&obj! {"biaoqiang" => obj! {"size" => vec![1020, 992], "pivot" => vec![0.5, 0.5], "position" => vec![0, 0]}});

    let index = parse_painting_index(&payload, &format!("{}/painting/biaoqiang.webp", base()), "").unwrap();

    assert_eq!(index.layers[0].raw_size, (1020.0, 992.0));
    assert!(index.face.is_none());
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_painting_index_rejects_a_document_without_a_layer
#[test]
fn a_painting_index_without_a_layer_is_rejected() {
    let payload = json(&obj! {"face" => obj! {"size" => vec![100, 100], "pivot" => vec![0.5, 0.5], "position" => vec![0, 0]}});

    let error = parse_painting_index(&payload, &format!("{}/painting/biaoqiang.webp", base()), "").unwrap_err();

    assert_eq!(error.class, SOURCE_SCHEMA_ERROR);
}

// py: tests/test_azurlane_l2d_sources.py::test_enumerate_painting_resources_adds_layer_sheets_and_meshes_from_the_index
#[test]
fn painting_resources_add_layer_sheets_and_meshes_from_the_index() {
    let catalog = catalog_of(&index_payload(vec![ship(1, "xiafei", "霞飞", "", vec![static_skin(10, "霞飞", "xiafei_4")])]));
    let entry = catalog_entry(&catalog, "azurlane:painting:xiafei:xiafei_4");
    let index = parse_painting_index(&xiafei_index_payload(), &entry.resources.primary_url, "").unwrap();

    let enumeration = enumerate_azurlane_model_resources(
        entry,
        EnumerationInputs {
            painting_index: Some(&index),
            ..EnumerationInputs::default()
        },
    )
    .unwrap();

    let by_kind = by_kind(&enumeration.assets);
    assert_eq!(
        source_urls(by_kind.get("painting.index")),
        [format!("{}/painting/xiafei_4.json", base())]
    );
    // The primary sheet keeps its painting.image spec, so it is not repeated as a layer.
    assert_eq!(
        source_urls(by_kind.get("painting.layer")),
        [
            format!("{}/painting/xiafei_4_rw.webp", base()),
            format!("{}/painting/xiafei_4_bj.webp", base()),
        ]
    );
    // xiafei_4 is `raw: true`: it is stored unpacked, and asking for its mesh gets the SPA shell.
    assert_eq!(
        source_urls(by_kind.get("painting.mesh")),
        [
            format!("{}/painting/xiafei_4_rw-mesh.obj", base()),
            format!("{}/painting/xiafei_4_bj-mesh.obj", base()),
        ]
    );
    assert_eq!(by_kind["painting.index"][0].local_path, "assets/painting/xiafei_4/xiafei_4.json");
    assert_eq!(
        by_kind["painting.layer"][0].local_path,
        "assets/painting/xiafei_4/painting/xiafei_4_rw.webp"
    );
    assert_eq!(
        by_kind["painting.mesh"][0].local_path,
        "assets/painting/xiafei_4/painting/xiafei_4_rw-mesh.obj"
    );
    assert_eq!(
        by_kind["painting.mesh"][0].context().get("layer"),
        Some(&Value::Str("xiafei_4_rw".to_owned()))
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_enumerate_painting_resources_matches_the_primary_layer_across_a_case_disagreement
#[test]
fn the_primary_layer_matches_across_a_case_disagreement() {
    // The entry says Z28_3; the index that answered for it says z28_3. It is still one sheet.
    let catalog = catalog_of(&index_payload(vec![ship(1, "Z28", "Z28", "", vec![static_skin(10, "Z28", "Z28_3")])]));
    let entry = catalog_entry(&catalog, "azurlane:painting:Z28:Z28_3");
    let index_source = json(&obj! {"z28_3" => obj! {"size" => vec![1908, 2048], "pivot" => vec![0.5, 0.5], "position" => vec![0, 0]}});
    let index = parse_painting_index(&index_source, &entry.resources.primary_url, "").unwrap();

    let enumeration = enumerate_azurlane_model_resources(
        entry,
        EnumerationInputs {
            painting_index: Some(&index),
            ..EnumerationInputs::default()
        },
    )
    .unwrap();

    let by_kind = by_kind(&enumeration.assets);
    assert!(!by_kind.contains_key("painting.layer"));
    // The mesh keeps the index's spelling, which is the one the CDN actually stores.
    assert_eq!(
        source_urls(by_kind.get("painting.mesh")),
        [format!("{}/painting/z28_3-mesh.obj", base())]
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_enumerate_painting_resources_without_an_index_stays_at_the_packed_sheet
#[test]
fn painting_resources_without_an_index_stay_at_the_packed_sheet() {
    let catalog = biaoqiang_catalog();
    let entry = catalog_entry(&catalog, "azurlane:painting:biaoqiang:biaoqiang");

    let enumeration = enumerate_azurlane_model_resources(entry, EnumerationInputs::default()).unwrap();

    let kinds: Vec<AssetKind> = enumeration.assets.iter().map(|asset| asset.kind).collect();
    assert!(kinds.contains(&AssetKind::PaintingImage));
    assert!(
        kinds
            .iter()
            .all(|kind| !matches!(kind, AssetKind::PaintingIndex | AssetKind::PaintingLayer | AssetKind::PaintingMesh))
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_painting_index_to_dict_uses_the_origin_field_names
#[test]
fn the_painting_index_renders_with_the_origin_field_names() {
    let index = parse_painting_index(&xiafei_index_payload(), &format!("{}/painting/xiafei_4.webp", base()), "").unwrap();

    let Value::Dict(payload) = index.to_dict() else { panic!("a dict") };

    assert_eq!(payload.get("painting_key"), Some(&Value::Str("xiafei_4".to_owned())));
    let Some(Value::List(layers)) = payload.get("layers") else { panic!("layers") };
    assert_eq!(
        layers[1],
        obj! {
            "name" => "xiafei_4_rw",
            "size" => vec![2000.0, 2048.0],
            "rawSize" => vec![2000.0, 2048.0],
            "position" => vec![10.0, 272.0],
            "pivot" => vec![0.490_676, 0.666_504],
            "raw" => false,
        }
    );
    let Some(Value::Dict(face)) = payload.get("face") else { panic!("face") };
    assert_eq!(face.get("size"), Some(&list![372.0, 374.0]));
}

// py: tests/test_azurlane_l2d_sources.py::test_case_variants_cover_a_filename_in_both_directions
#[test]
fn case_variants_cover_a_filename_in_both_directions() {
    // The index disagrees with storage both ways: painting/Z28_3.webp is stored as z28_3.webp,
    // and painting/u47_5.webp as U47_5.webp.
    let lowered = case_variant_urls(&format!("{}/painting/Z28_3.webp", base())).unwrap();
    assert!(lowered.contains(&format!("{}/painting/z28_3.webp", base())));

    let capitalised = case_variant_urls(&format!("{}/painting/u47_5.webp", base())).unwrap();
    assert!(capitalised.contains(&format!("{}/painting/U47_5.webp", base())));
}

// py: tests/test_azurlane_l2d_sources.py::test_case_variants_also_vary_the_key_bearing_directory
#[test]
fn case_variants_also_vary_the_key_bearing_directory() {
    // A face lives under the painting key as a directory, so the key is not in the filename.
    assert_eq!(
        case_variant_urls(&format!("{}/paintingface/Z28_3/1.webp", base())).unwrap(),
        [format!("{}/paintingface/z28_3/1.webp", base())]
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_case_variants_leave_the_cdn_category_directory_alone
#[test]
fn case_variants_leave_the_cdn_category_directory_alone() {
    assert_eq!(
        case_variant_urls(&format!("{}/painting/u47_5.webp", base())).unwrap(),
        [format!("{}/painting/U47_5.webp", base())]
    );
    assert_eq!(
        case_variant_urls(&format!("{}/squareicon/U96_2.webp", base())).unwrap(),
        [format!("{}/squareicon/u96_2.webp", base())]
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_case_variants_never_repeat_the_original_url
#[test]
fn case_variants_never_repeat_the_original_url() {
    for url in [
        format!("{}/painting/biaoqiang.webp", base()),
        format!("{}/painting/Z28_3.webp", base()),
        format!("{}/paintingface/u47_5/1.webp", base()),
    ] {
        let variants = case_variant_urls(&url).unwrap();
        assert!(!variants.contains(&url));
        let mut unique = variants.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), variants.len());
    }
}

// py: tests/test_azurlane_l2d_sources.py::test_painting_resources_leave_case_handling_to_the_downloader
#[test]
fn painting_resources_leave_case_handling_to_the_downloader() {
    // Enumeration names only what the source named; variants are derived when a fetch fails.
    let catalog = catalog_of(&index_payload(vec![ship(1, "Z28", "Z28", "", vec![static_skin(10, "Z28", "Z28_3")])]));
    let entry = catalog_entry(&catalog, "azurlane:painting:Z28:Z28_3");

    let enumeration = enumerate_azurlane_model_resources(entry, EnumerationInputs::default()).unwrap();

    let by_kind = by_kind(&enumeration.assets);
    assert_eq!(by_kind["painting.image"][0].source_url, format!("{}/painting/Z28_3.webp", base()));
    assert_eq!(by_kind["painting.face"][0].source_url, format!("{}/paintingface/Z28_3/1.webp", base()));
    assert!(enumeration.assets.iter().all(|asset| asset.fallback_url.is_empty()));
}

// py: tests/test_azurlane_l2d_sources.py::test_painting_entries_keep_their_paths_when_model_paths_are_applied
#[test]
fn painting_entries_keep_their_paths_when_model_paths_are_applied() {
    let catalog = catalog_of(&index_payload(vec![ship(1, "biaoqiang", "标枪", "", vec![skin(10, "默认", "biaoqiang", "live2d")])]));
    let resolved_model_url = live2d_url("Biaoqiang");

    let patched = apply_l2d_su_model_paths(&catalog, &HashMap::from([(10, resolved_model_url.clone())]));

    assert_eq!(
        catalog_entry(&patched, "azurlane:live2d:biaoqiang:biaoqiang").resources.primary_url,
        resolved_model_url
    );
    let painting = catalog_entry(&patched, "azurlane:painting:biaoqiang:biaoqiang");
    assert_eq!(painting.resources.primary_url, format!("{}/painting/biaoqiang.webp", base()));
    assert_eq!(model_probe_urls(painting).unwrap(), [painting.resources.primary_url.clone()]);
}
