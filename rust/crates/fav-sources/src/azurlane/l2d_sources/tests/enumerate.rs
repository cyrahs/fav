//! Resource enumeration of Live2D and spine models, and the spine parts manifest.

use std::collections::HashMap;

use fav_core::pyjson::Value;

use super::super::{
    AssetKind, EnumerationInputs, ModelKind, SOURCE_SCHEMA_ERROR, SpineModelPart, build_azurlane_model_catalog,
    enumerate_azurlane_model_resources, parse_spine_parts_manifest, spine_parts_manifest_url, spine_resource_manifest,
};
use super::{catalog_entry, character, json, model, source_snapshots, spine_parts_payload, spine_url};

fn text(value: &str) -> Value {
    Value::Str(value.to_owned())
}

fn kinds(assets: &[super::super::AzurLaneEnumeratedResource]) -> Vec<&'static str> {
    assets.iter().map(|asset| asset.kind.as_str()).collect()
}

fn part(name: &str, skeleton_url: String, atlas_url: String) -> SpineModelPart {
    SpineModelPart {
        name: name.to_owned(),
        skeleton_url,
        atlas_urls: vec![atlas_url],
    }
}

// py: tests/test_azurlane_l2d_sources.py::test_enumerate_live2d_model3_resources_with_paths_and_contexts
#[test]
fn live2d_resources_come_from_the_model3_with_paths_and_contexts() {
    let primary_url = "https://static.example/live2d/azurlane/guanghui_7/guanghui_7.model3.json";
    let snapshots = source_snapshots(
        vec![character(
            1,
            "guanghui",
            "光辉",
            "Illustrious",
            vec![model(ModelKind::Live2d, 7, "私人茶会", "Our Private Study Session", primary_url)],
            Vec::new(),
        )],
        &[("guanghui_7", "Illustrious - Our Private Study Session")],
    );
    let model3_source = json(&obj! {
        "Version" => 3,
        "FileReferences" => obj! {
            "Moc" => "guanghui_7.moc3",
            "Textures" => vec!["textures/texture_00.webp", "textures/texture_01.webp"],
            "Physics" => "guanghui_7.physics3.json",
            "Pose" => "guanghui_7.pose3.json",
            "DisplayInfo" => "display.cdi3.json",
            "Expressions" => list![obj! {"Name" => "smile", "File" => "expressions/smile.exp3.json"}],
            "Motions" => obj! {
                "Idle" => list![obj! {"File" => "motions/idle.motion3.json", "Sound" => "voice/idle.wav"}],
                // The older l2d.su layout: an absolute Audio URL plus an inline subtitle line
                // in Text (spoken dialogue, not a file path).
                "TapBody" => list![obj! {
                    "File" => "motions/tap_body.motion3.json",
                    "Audio" => "https://patchwiki.example/images/blhx/tap_body.mp3",
                    "Text" => "来跟我赌一把吧。",
                }],
            },
        },
    });

    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();
    let entry = catalog_entry(&catalog, "azurlane:live2d:guanghui:guanghui_7");
    let enumeration = enumerate_azurlane_model_resources(
        entry,
        EnumerationInputs {
            model3_source: Some(&model3_source),
            ..EnumerationInputs::default()
        },
    )
    .unwrap();

    assert_eq!(enumeration.model_id, entry.id);
    assert_eq!(enumeration.source_url, primary_url);
    assert_eq!(
        enumeration.fallback_url,
        "https://cdn.nagami.moe/live2d/guanghui_7/guanghui_7.model3.json"
    );
    assert_eq!(
        kinds(&enumeration.assets),
        [
            "live2d.model3",
            "live2d.moc3",
            "live2d.texture",
            "live2d.texture",
            "live2d.physics",
            "live2d.pose",
            "live2d.display-info",
            "live2d.expression",
            "live2d.motion",
            "live2d.motion",
            "live2d.audio",
            "live2d.audio",
        ]
    );

    // `{asset.context.get('live2d_field'): asset ...}`: a later asset replaces an earlier one.
    let mut by_field = HashMap::new();
    for asset in &enumeration.assets {
        if asset.kind != AssetKind::Live2dTexture {
            let field = asset.context().get("live2d_field").and_then(Value::as_str).map(str::to_owned);
            by_field.insert(field.unwrap_or_default(), asset);
        }
    }
    assert_eq!(
        by_field["model3"].local_path,
        "assets/live2d/guanghui_7/guanghui_7.model3.json"
    );
    assert_eq!(
        by_field["physics"].source_url,
        "https://static.example/live2d/azurlane/guanghui_7/guanghui_7.physics3.json"
    );
    assert_eq!(by_field["pose"].local_path, "assets/live2d/guanghui_7/guanghui_7.pose3.json");
    assert_eq!(by_field["expression"].context().get("expression_name"), Some(&text("smile")));
    let audio: Vec<_> = enumeration
        .assets
        .iter()
        .filter(|asset| asset.kind == AssetKind::Live2dAudio)
        .collect();
    assert_eq!(audio[0].fallback_url, "https://cdn.nagami.moe/live2d/guanghui_7/voice/idle.wav");
    // The absolute Audio URL passes through untouched and lands next to the model's other assets.
    assert_eq!(audio[1].source_url, "https://patchwiki.example/images/blhx/tap_body.mp3");
    assert_eq!(audio[1].local_path, "assets/live2d/guanghui_7/tap_body.mp3");
    assert_eq!(audio[1].context().get("motion_text"), Some(&text("来跟我赌一把吧。")));
    assert_eq!(by_field["motion"].context().get("motion_group"), Some(&text("TapBody")));
    // The subtitle stays metadata on the motion; no asset is enumerated for it.
    assert_eq!(by_field["motion"].context().get("motion_text"), Some(&text("来跟我赌一把吧。")));
    assert!(enumeration.assets.iter().all(|asset| asset.kind.as_str() != "live2d.text"));
    for asset in &enumeration.assets {
        let context = asset.context();
        assert_eq!(context.get("model_id"), Some(&text(&entry.id)));
        assert_eq!(context.get("character_key"), Some(&text("guanghui")));
        assert_eq!(context.get("costume_key"), Some(&text("guanghui_7")));
    }
}

// py: tests/test_azurlane_l2d_sources.py::test_enumerate_spine_resources_parses_atlas_texture_pages
#[test]
fn spine_resources_take_the_atlas_texture_pages() {
    let spine_base_url = "https://static.example/live2d/azurlane/yilisi_2_doa";
    let snapshots = source_snapshots(
        vec![character(
            2,
            "yilisi",
            "伊莉丝",
            "Iris",
            Vec::new(),
            vec![model(ModelKind::Spine, 2, "某位女神的午后", "Afternoon of a Goddess", spine_base_url)],
        )],
        &[],
    );
    let atlas_source = "\nyilisi_2_doa.webp\nsize: 4096,4096\nformat: RGBA8888\n\neffects/glow.png\nsize: 1024,1024\nformat: RGBA8888\n";

    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();
    let entry = catalog_entry(&catalog, "azurlane:spine:yilisi:yilisi_2_doa");
    let atlas_sources = HashMap::from([(format!("{spine_base_url}/yilisi_2_doa.atlas"), atlas_source.to_owned())]);
    let enumeration = enumerate_azurlane_model_resources(
        entry,
        EnumerationInputs {
            atlas_sources: Some(&atlas_sources),
            ..EnumerationInputs::default()
        },
    )
    .unwrap();

    assert_eq!(
        kinds(&enumeration.assets),
        ["spine.skel", "spine.atlas", "spine.texture", "spine.texture"]
    );
    let urls: Vec<&str> = enumeration.assets.iter().map(|asset| asset.source_url.as_str()).collect();
    assert_eq!(
        urls,
        [
            "https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.skel",
            "https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.atlas",
            "https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.webp",
            "https://static.example/live2d/azurlane/yilisi_2_doa/effects/glow.png",
        ]
    );
    let paths: Vec<&str> = enumeration.assets.iter().map(|asset| asset.local_path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "assets/spine/yilisi_2_doa/yilisi_2_doa.skel",
            "assets/spine/yilisi_2_doa/yilisi_2_doa.atlas",
            "assets/spine/yilisi_2_doa/yilisi_2_doa.webp",
            "assets/spine/yilisi_2_doa/effects/glow.png",
        ]
    );
    let last = enumeration.assets.last().unwrap().context();
    assert_eq!(last.get("atlas_page"), Some(&text("effects/glow.png")));
    assert_eq!(last.get("page_index"), Some(&Value::Int(1)));
}

// py: tests/test_azurlane_l2d_sources.py::test_spine_resource_manifest_defaults_to_a_single_part_named_after_the_directory
#[test]
fn a_spine_manifest_defaults_to_one_part_named_after_the_directory() {
    let base_url = spine_url("yuanchou_2");

    let manifest = spine_resource_manifest(&base_url, None).unwrap();

    assert_eq!(spine_parts_manifest_url(&base_url).unwrap(), format!("{base_url}/yuanchou_2.json"));
    assert_eq!(manifest.parts_manifest_url, "");
    assert_eq!(
        manifest.parts,
        [part(
            "yuanchou_2",
            format!("{base_url}/yuanchou_2.skel"),
            format!("{base_url}/yuanchou_2.atlas")
        )]
    );
    assert_eq!(
        spine_resource_manifest(&format!("{base_url}/yuanchou_2.atlas"), None).unwrap(),
        manifest
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_spine_parts_manifest_resolves_each_part_against_the_manifest_url
#[test]
fn each_spine_part_resolves_against_the_manifest_url() {
    let base_url = spine_url("lafei_12");

    let parts = parse_spine_parts_manifest(
        &spine_parts_payload("lafei_12", &["B", "T"]),
        &format!("{base_url}/lafei_12.json"),
    )
    .unwrap();

    assert_eq!(
        parts,
        [
            part(
                "lafei_12B",
                format!("{base_url}/lafei_12B.skel"),
                format!("{base_url}/lafei_12B.atlas")
            ),
            part(
                "lafei_12T",
                format!("{base_url}/lafei_12T.skel"),
                format!("{base_url}/lafei_12T.atlas")
            ),
        ]
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_spine_parts_manifest_rejects_payloads_without_models
#[test]
fn a_spine_parts_manifest_needs_a_models_list() {
    let error = parse_spine_parts_manifest(&json(&obj! {"version" => 1}), "https://static.example/spine/x.json").unwrap_err();

    assert_eq!(error.class, SOURCE_SCHEMA_ERROR);
    assert!(error.message.contains("models list"), "{}", error.message);
}

// py: tests/test_azurlane_l2d_sources.py::test_enumerate_spine_resources_covers_every_part_of_a_multi_part_model
#[test]
fn spine_resources_cover_every_part_of_a_multi_part_model() {
    let spine_base_url = spine_url("lafei_12");
    let snapshots = source_snapshots(
        vec![character(
            10117,
            "lafei",
            "拉菲",
            "Laffey",
            Vec::new(),
            vec![model(ModelKind::Spine, 131_172, "慵懒时光", "Lazy Days", &spine_base_url)],
        )],
        &[],
    );
    let parts_source = spine_parts_payload("lafei_12", &["B", "T"]);
    let atlas_sources = HashMap::from([
        (format!("{spine_base_url}/lafei_12B.atlas"), "\nlafei_12B.webp\nsize: 4096,4096\n".to_owned()),
        (format!("{spine_base_url}/lafei_12T.atlas"), "\nlafei_12T.webp\nsize: 4096,4096\n".to_owned()),
    ]);

    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();
    let entry = catalog_entry(&catalog, "azurlane:spine:lafei:lafei_12");
    let enumeration = enumerate_azurlane_model_resources(
        entry,
        EnumerationInputs {
            spine_parts_source: Some(&parts_source),
            atlas_sources: Some(&atlas_sources),
            ..EnumerationInputs::default()
        },
    )
    .unwrap();

    let pairs: Vec<(&str, String)> = enumeration
        .assets
        .iter()
        .map(|asset| (asset.kind.as_str(), asset.source_url.clone()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("spine.parts", format!("{spine_base_url}/lafei_12.json")),
            ("spine.skel", format!("{spine_base_url}/lafei_12B.skel")),
            ("spine.atlas", format!("{spine_base_url}/lafei_12B.atlas")),
            ("spine.texture", format!("{spine_base_url}/lafei_12B.webp")),
            ("spine.skel", format!("{spine_base_url}/lafei_12T.skel")),
            ("spine.atlas", format!("{spine_base_url}/lafei_12T.atlas")),
            ("spine.texture", format!("{spine_base_url}/lafei_12T.webp")),
        ]
    );
    let paths: Vec<&str> = enumeration.assets.iter().map(|asset| asset.local_path.as_str()).collect();
    assert_eq!(
        paths[..2],
        ["assets/spine/lafei_12/lafei_12.json", "assets/spine/lafei_12/lafei_12B.skel"]
    );
    assert_eq!(enumeration.assets[1].context().get("spine_part"), Some(&text("lafei_12B")));
}

// py: tests/test_azurlane_l2d_sources.py::test_enumerate_live2d_missing_optional_resources_does_not_fail
#[test]
fn missing_optional_live2d_resources_do_not_fail() {
    let primary_url = "https://static.example/live2d/azurlane/minimal/minimal.model3.json";
    let snapshots = source_snapshots(
        vec![character(
            3,
            "minimal",
            "Minimal",
            "Minimal",
            vec![model(ModelKind::Live2d, 1, "Minimal", "Minimal", primary_url)],
            Vec::new(),
        )],
        &[],
    );
    let model3_source = json(&obj! {
        "Version" => 3,
        "FileReferences" => obj! {
            "Moc" => "minimal.moc3",
            "Textures" => vec!["texture.webp"],
            "Physics" => Value::Null,
            "Pose" => obj! {},
            "Expressions" => obj! {},
            "Motions" => list![],
        },
    });

    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();
    let enumeration = enumerate_azurlane_model_resources(
        &catalog.entries[0],
        EnumerationInputs {
            model3_source: Some(&model3_source),
            ..EnumerationInputs::default()
        },
    )
    .unwrap();

    assert_eq!(kinds(&enumeration.assets), ["live2d.model3", "live2d.moc3", "live2d.texture"]);
    let paths: Vec<&str> = enumeration.assets.iter().map(|asset| asset.local_path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "assets/live2d/minimal/minimal.model3.json",
            "assets/live2d/minimal/minimal.moc3",
            "assets/live2d/minimal/texture.webp",
        ]
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_enumerated_resource_paths_are_deterministic_and_variant_safe
#[test]
fn resource_paths_are_deterministic_and_variant_safe() {
    let primary_url = "https://static.example/live2d/azurlane/biaoqiang/biaoqiang.model3.json";
    let snapshots = source_snapshots(
        vec![character(
            4,
            "biaoqiang",
            "标枪",
            "Javelin",
            vec![model(ModelKind::Live2d, 1, "默认", "Default", primary_url)],
            Vec::new(),
        )],
        &[],
    );
    let model3_source = json(&obj! {
        "Version" => 3,
        "FileReferences" => obj! {"Moc" => "biaoqiang.moc3", "Textures" => vec!["textures/texture.webp"]},
    });
    let inputs = EnumerationInputs {
        model3_source: Some(&model3_source),
        ..EnumerationInputs::default()
    };
    let paths = |enumeration: &super::super::AzurLaneModelResourceEnumeration| -> Vec<String> {
        enumeration.assets.iter().map(|asset| asset.local_path.clone()).collect()
    };

    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();
    let entry = &catalog.entries[0];
    let first = enumerate_azurlane_model_resources(entry, inputs).unwrap();
    let second = enumerate_azurlane_model_resources(entry, inputs).unwrap();
    let mut variant = entry.clone();
    variant.id = format!("{}:asset-test", entry.id);
    variant.resources.primary_url = primary_url.replace("static", "mirror");
    let variant_first = enumerate_azurlane_model_resources(&variant, inputs).unwrap();
    let variant_second = enumerate_azurlane_model_resources(&variant, inputs).unwrap();

    assert_eq!(paths(&first), paths(&second));
    assert_eq!(paths(&variant_first), paths(&variant_second));
    assert_eq!(first.assets[0].local_path, "assets/live2d/biaoqiang/biaoqiang.model3.json");
    assert!(variant_first.assets[0].local_path.starts_with("assets/live2d/biaoqiang-"));
    assert!(variant_first.assets[0].local_path.ends_with("/biaoqiang.model3.json"));
}
