//! The model catalog: merging the two sources, entry ids, search, and probe URLs.

use std::collections::BTreeSet;

use fav_core::pyjson::Value;

use super::super::{CatalogSource, ModelKind, build_azurlane_model_catalog, model_probe_urls};
use super::{catalog_entry, character, entry_ids_with_costume_key, keys, live2d_url, model, source_snapshots, spine_url};

fn ids(entries: &[&super::super::ModelEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.id.clone()).collect()
}

// py: tests/test_azurlane_l2d_sources.py::test_model_probe_urls_cover_both_spine_layouts
#[test]
fn probe_urls_cover_both_spine_layouts() {
    let snapshots = source_snapshots(
        vec![character(
            1,
            "guanghui",
            "光辉",
            "Illustrious",
            vec![model(ModelKind::Live2d, 7, "私人茶会", "Study Session", &live2d_url("guanghui_7"))],
            vec![model(ModelKind::Spine, 8, "慵懒时光", "Lazy Days", &spine_url("lafei_12"))],
        )],
        &[],
    );
    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();

    assert_eq!(
        model_probe_urls(catalog_entry(&catalog, "azurlane:live2d:guanghui:guanghui_7")).unwrap(),
        [live2d_url("guanghui_7")]
    );
    assert_eq!(
        model_probe_urls(catalog_entry(&catalog, "azurlane:spine:guanghui:lafei_12")).unwrap(),
        [
            format!("{}/lafei_12.skel", spine_url("lafei_12")),
            format!("{}/lafei_12.json", spine_url("lafei_12")),
        ]
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_build_azurlane_model_catalog_merges_exact_nagami_fallback_and_prefers_l2d_su
#[test]
fn the_catalog_merges_an_exact_nagami_fallback_and_prefers_l2d_su() {
    let snapshots = source_snapshots(
        vec![
            character(
                1,
                "guanghui",
                "光辉",
                "Illustrious",
                vec![model(
                    ModelKind::Live2d,
                    7,
                    "私人茶会",
                    "Our Private Study Session",
                    "https://static.l2d.su/live2d/azurlane/guanghui_7/guanghui_7.model3.json",
                )],
                Vec::new(),
            ),
            character(
                2,
                "yilisi",
                "伊莉丝",
                "Iris",
                Vec::new(),
                vec![model(
                    ModelKind::Spine,
                    2,
                    "某位女神的午后",
                    "Afternoon of a Goddess",
                    "https://static.l2d.su/live2d/azurlane/yilisi_2_doa",
                )],
            ),
        ],
        &[
            ("guanghui_7", "Illustrious - Our Private Study Session"),
            ("shengluyisi_2", "St. Louis - Blue and White Pottery"),
        ],
    );

    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();

    assert_eq!(
        Value::Dict(catalog.summary()),
        obj! {
            "entry_count" => 3,
            "by_type" => obj! {"live2d" => 2, "spine" => 1, "painting" => 0},
            "by_source" => obj! {"l2d.su" => 1, "nagami" => 1, "merged" => 1},
            "by_type_source" => obj! {
                "live2d" => obj! {"l2d.su" => 0, "nagami" => 1, "merged" => 1},
                "spine" => obj! {"l2d.su" => 1, "nagami" => 0, "merged" => 0},
                "painting" => obj! {"l2d.su" => 0, "nagami" => 0, "merged" => 0},
            },
            "nagami_fallback_candidate_count" => 2,
        }
    );

    let merged = catalog_entry(&catalog, "azurlane:live2d:guanghui:guanghui_7");
    assert_eq!(merged.source, CatalogSource::Merged);
    assert_eq!(
        merged.resources.primary_url,
        "https://static.l2d.su/live2d/azurlane/guanghui_7/guanghui_7.model3.json"
    );
    assert_eq!(
        merged.resources.fallback_url,
        "https://cdn.nagami.moe/live2d/guanghui_7/guanghui_7.model3.json"
    );
    assert_eq!(
        keys(&merged.to_dict()),
        ["id", "type", "source", "character", "costume", "resources", "resource_summary", "availability"]
            .map(str::to_owned)
            .into()
    );

    let nagami_only = catalog_entry(&catalog, "azurlane:live2d:shengluyisi:shengluyisi_2");
    assert_eq!(nagami_only.source, CatalogSource::Nagami);
    assert_eq!(nagami_only.character.name_en, "St. Louis");
    assert_eq!(nagami_only.costume.name_en, "Blue and White Pottery");

    let spine = catalog_entry(&catalog, "azurlane:spine:yilisi:yilisi_2_doa");
    assert_eq!(spine.source, CatalogSource::L2dSu);
    assert_eq!(spine.resources.fallback_url, "");
}

// py: tests/test_azurlane_l2d_sources.py::test_build_azurlane_model_catalog_keeps_l2d_su_path_variant_separate_from_nagami_fallback
#[test]
fn an_l2d_su_path_variant_stays_apart_from_the_nagami_fallback() {
    let snapshots = source_snapshots(
        vec![character(
            3,
            "adaerbote",
            "阿达尔伯特",
            "Adalbert",
            vec![model(
                ModelKind::Live2d,
                30,
                "幻梦变体",
                "Fantasy Variant",
                "https://static.l2d.su/live2d/azurlane/adaerbote_3_fhx/adaerbote_3.model3.json",
            )],
            Vec::new(),
        )],
        &[("adaerbote_3", "Adalbert - Standard Costume")],
    );

    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();

    let variant = catalog_entry(&catalog, "azurlane:live2d:adaerbote:adaerbote_3_fhx");
    assert_eq!(variant.source, CatalogSource::L2dSu);
    assert_eq!(variant.costume.key, "adaerbote_3_fhx");
    assert_eq!(
        variant.resources.primary_url,
        "https://static.l2d.su/live2d/azurlane/adaerbote_3_fhx/adaerbote_3.model3.json"
    );
    assert_eq!(variant.resources.fallback_url, "");

    let nagami_candidate = catalog_entry(&catalog, "azurlane:live2d:adaerbote:adaerbote_3");
    assert_eq!(nagami_candidate.source, CatalogSource::Nagami);
    assert_eq!(
        nagami_candidate.resources.primary_url,
        "https://cdn.nagami.moe/live2d/adaerbote_3/adaerbote_3.model3.json"
    );
    assert_eq!(ids(&catalog.search("Fantasy Variant", None)), [variant.id.clone()]);
}

// py: tests/test_azurlane_l2d_sources.py::test_catalog_ids_are_key_based_and_searches_character_and_costume_names
#[test]
fn catalog_ids_are_key_based_and_search_covers_character_and_costume_names() {
    let snapshots = source_snapshots(
        vec![character(
            1,
            "guanghui",
            "光辉",
            "Illustrious",
            vec![model(
                ModelKind::Live2d,
                7,
                "丝绸与茶",
                "Our Private Study Session",
                "https://static.l2d.su/live2d/azurlane/guanghui_7/guanghui_7.model3.json",
            )],
            Vec::new(),
        )],
        &[
            ("guanghui_7", "Illustrious - Our Private Study Session"),
            ("shengluyisi_2", "St. Louis - Blue and White Pottery"),
        ],
    );

    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();
    let entry = catalog.search("Illustrious", None)[0];

    assert_eq!(entry.id, "azurlane:live2d:guanghui:guanghui_7");
    assert!(!entry.id.contains("Illustrious"));
    assert!(!entry.id.contains("光辉"));
    assert_eq!(ids(&catalog.search("Our Private", None)), [entry.id.clone()]);
    assert_eq!(ids(&catalog.search("丝绸", None)), [entry.id.clone()]);
    assert_eq!(
        ids(&catalog.search("blue pottery", None)),
        ["azurlane:live2d:shengluyisi:shengluyisi_2"]
    );
    assert_eq!(
        ids(&catalog.search("guanghui", Some(ModelKind::Live2d))),
        [entry.id.clone()]
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_catalog_merges_duplicate_assets_and_keeps_different_assets_as_variants
#[test]
fn duplicate_assets_merge_and_different_assets_stay_variants() {
    let default = |path: &str| model(ModelKind::Live2d, 1, "默认", "Default", path);
    let snapshots = source_snapshots(
        vec![character(
            1,
            "biaoqiang",
            "标枪",
            "Javelin",
            vec![
                default("https://static.l2d.su/live2d/azurlane/biaoqiang/biaoqiang.model3.json"),
                default("https://static.l2d.su/live2d/azurlane/biaoqiang/biaoqiang.model3.json"),
                default("https://mirror.example/live2d/azurlane/biaoqiang/biaoqiang.model3.json"),
            ],
            Vec::new(),
        )],
        &[],
    );

    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();

    assert_eq!(catalog.entries.len(), 2);
    let base_ids = entry_ids_with_costume_key(&catalog, "biaoqiang");
    assert!(base_ids.contains(&"azurlane:live2d:biaoqiang:biaoqiang".to_owned()));
    assert!(
        base_ids
            .iter()
            .any(|entry_id| entry_id.starts_with("azurlane:live2d:biaoqiang:biaoqiang:asset-"))
    );
    let urls: BTreeSet<&str> = catalog
        .entries
        .iter()
        .map(|entry| entry.resources.primary_url.as_str())
        .collect();
    assert_eq!(
        urls,
        BTreeSet::from([
            "https://static.l2d.su/live2d/azurlane/biaoqiang/biaoqiang.model3.json",
            "https://mirror.example/live2d/azurlane/biaoqiang/biaoqiang.model3.json",
        ])
    );
}
