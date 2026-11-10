//! The ship index, ship detail and nagami mapping parsers.

use std::collections::{BTreeMap, HashMap};

use fav_core::pyjson::Value;

use super::super::{
    L2D_SU_AUDIO_BASE_URL, L2D_SU_LEGACY_STATIC_BASE_URL, L2D_SU_STATIC_BASE_URL, SOURCE_SCHEMA_ERROR, apply_l2d_su_model_paths,
    apply_l2d_su_ship_classes, build_azurlane_model_catalog, l2d_su_asset_url_from_legacy, l2d_su_character_fingerprint,
    l2d_su_legacy_asset_urls, parse_l2d_su_ship_class, parse_l2d_su_ship_index, parse_l2d_su_ship_models, parse_l2d_su_ship_voices,
    parse_nagami_mapping,
};
use super::{index_payload, json, set, ship, ship_index_payload, skin, source_snapshots, static_skin};

// py: tests/test_azurlane_l2d_sources.py::test_parse_l2d_su_ship_index_derives_model_urls_from_skin_keys
#[test]
fn the_ship_index_derives_model_urls_from_skin_keys() {
    let payload = ship_index_payload(
        vec![
            ship(
                1,
                "biaoqiang",
                "标枪",
                "HMS Javelin",
                vec![static_skin(10, "标枪", "biaoqiang"), skin(11, "默认", "biaoqiang_2", "live2d")],
            ),
            ship(2, "yilisi", "伊莉丝", "", vec![skin(20, "某位女神的午后", "yilisi_2_doa", "spine")]),
        ],
        "CN",
        vec![obj! {"shipGroupId" => 2, "shipName" => "伊莉丝", "skinName" => "某位女神的午后", "skinType" => "Spine", "skinIds" => vec![20]}],
    );

    let parsed = parse_l2d_su_ship_index(&payload, "CN").unwrap();

    assert_eq!(parsed.region, "CN");
    assert_eq!(parsed.game_version, "9.7.295");
    assert_eq!(parsed.ship_count, 2);
    assert_eq!(
        Value::Dict(parsed.summary()),
        obj! {"character_count" => 2, "live2d_count" => 1, "spine_count" => 1, "painting_count" => 3}
    );
    let javelin = &parsed.characters[0];
    assert_eq!(javelin.char_name_en, "HMS Javelin");
    assert_eq!(
        javelin.live2d[0].path,
        "https://static.l2d.su/al/assets/live2d/biaoqiang_2/biaoqiang_2.model3.json"
    );
    assert_eq!(javelin.live2d[0].feature_tags, ["Live2D"]);
    assert_eq!(
        parsed.characters[1].spine[0].path,
        "https://static.l2d.su/al/assets/spinepainting/yilisi_2_doa"
    );
    assert_eq!(parsed.characters[1].spine[0].model_key, "yilisi_2_doa");
    assert_eq!(parsed.new_skins[0].skin_ids, [20]);
    let painting_keys: Vec<&str> = javelin.paintings.iter().map(|painting| painting.model_key.as_str()).collect();
    assert_eq!(painting_keys, ["biaoqiang", "biaoqiang_2"]);
    assert_eq!(javelin.paintings[0].path, "https://static.l2d.su/al/assets/painting/biaoqiang.webp");
    assert_eq!(javelin.paintings[0].face_ids, ["1", "2"]);
    assert_eq!(javelin.paintings[0].square_icon, "squareicon/biaoqiang");
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_l2d_su_ship_index_rejects_html_and_schema_failures
#[test]
fn the_ship_index_rejects_a_payload_without_a_ships_list() {
    let error = parse_l2d_su_ship_index(&json(&obj! {"ships" => obj! {}}), "CN").unwrap_err();

    assert_eq!(error.class, SOURCE_SCHEMA_ERROR);
    assert!(error.message.contains("ships list"), "{}", error.message);
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_l2d_su_ship_index_treats_ships_without_model_skins_as_empty
#[test]
fn ships_without_model_skins_have_only_paintings() {
    let payload = index_payload(vec![ship(1, "biaoqiang", "标枪", "", vec![static_skin(10, "标枪", "biaoqiang")])]);

    let parsed = parse_l2d_su_ship_index(&payload, "CN").unwrap();

    assert_eq!(
        Value::Dict(parsed.summary()),
        obj! {"character_count" => 1, "live2d_count" => 0, "spine_count" => 0, "painting_count" => 1}
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_l2d_su_ship_models_maps_costume_ids_to_absolute_urls
#[test]
fn ship_detail_models_map_costume_ids_to_absolute_urls() {
    let payload = json(&obj! {
        "ship" => obj! {
            "shipGroupId" => 40505,
            "skins" => list![
                obj! {"id" => 405_050, "model" => obj! {"type" => "spine", "path" => "spinepainting/bisimaiZ", "key" => "bisimaiZ"}},
                obj! {"id" => 405_051, "model" => obj! {"type" => "live2d", "path" => "live2d/Z23/Z23.model3.json", "key" => "Z23"}},
                obj! {"id" => 405_052},
                obj! {"id" => 405_053, "model" => obj! {"type" => "spine", "path" => "https://mirror.example/spine/x"}},
            ],
        },
    });

    assert_eq!(
        parse_l2d_su_ship_models(&payload).unwrap(),
        [
            (405_050, "https://static.l2d.su/al/assets/spinepainting/bisimaiZ".to_owned()),
            (405_051, "https://static.l2d.su/al/assets/live2d/Z23/Z23.model3.json".to_owned()),
            (405_053, "https://mirror.example/spine/x".to_owned()),
        ]
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_l2d_su_ship_models_rejects_payloads_without_a_ship
#[test]
fn ship_detail_models_need_a_ship_object() {
    let error = parse_l2d_su_ship_models(&json(&obj! {"generatedAt" => "2026-08-12"})).unwrap_err();

    assert_eq!(error.class, SOURCE_SCHEMA_ERROR);
    assert!(error.message.contains("ship object"), "{}", error.message);
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_l2d_su_ship_index_aggregates_skin_series_and_default_skin
#[test]
fn the_ship_index_aggregates_skin_series_and_the_default_skin() {
    let dressed = set(skin(11, "泳装", "biaoqiang_2", "live2d"), "shopTypeName", "Swimsuits");
    let repeat = set(skin(12, "另一件泳装", "biaoqiang_3", "live2d"), "shopTypeName", "Swimsuits");
    let javelin = ship(1, "biaoqiang", "标枪", "", vec![static_skin(10, "标枪", "biaoqiang"), dressed, repeat]);
    let javelin = set(javelin, "defaultSkinId", 10);

    let parsed = parse_l2d_su_ship_index(&index_payload(vec![javelin]), "CN").unwrap();

    let character = &parsed.characters[0];
    assert_eq!(character.default_skin_id, Some(10));
    // Shopless skins fall back to their skin type, which is how l2d.su's own skinSeries filter is built.
    assert_eq!(character.skin_series, ["Default", "Swimsuits"]);
}

// py: tests/test_azurlane_l2d_sources.py::test_apply_l2d_su_ship_classes_attaches_detail_only_class_names
#[test]
fn ship_classes_from_the_detail_attach_to_every_entry_of_the_ship() {
    let payload = index_payload(vec![ship(1, "biaoqiang", "标枪", "", vec![skin(10, "默认", "biaoqiang", "live2d")])]);
    let characters = parse_l2d_su_ship_index(&payload, "CN").unwrap().characters;
    let catalog = build_azurlane_model_catalog(&source_snapshots(characters, &[])).unwrap();
    assert!(catalog.entries.iter().all(|entry| entry.character.class_name.is_empty()));

    let classes = HashMap::from([(1, "J Class".to_owned()), (999, "Ignored Class".to_owned())]);
    let patched = apply_l2d_su_ship_classes(&catalog, &classes);

    assert!(!patched.entries.is_empty());
    assert!(patched.entries.iter().all(|entry| entry.character.class_name == "J Class"));
    assert_eq!(apply_l2d_su_ship_classes(&catalog, &HashMap::new()), catalog);
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_l2d_su_ship_class_reads_class_name
#[test]
fn the_ship_class_comes_from_the_detail() {
    let detail = json(&obj! {"ship" => obj! {"shipGroupId" => 1, "className" => "J Class", "skins" => list![]}});

    assert_eq!(parse_l2d_su_ship_class(&detail).unwrap(), "J Class");
    assert_eq!(parse_l2d_su_ship_class(&json(&obj! {"ship" => obj! {"shipGroupId" => 1}})).unwrap(), "");
    let error = parse_l2d_su_ship_class(&json(&obj! {"version" => 1})).unwrap_err();
    assert_eq!(error.class, SOURCE_SCHEMA_ERROR);
    assert!(error.message.contains("ship object"), "{}", error.message);
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_l2d_su_ship_voices_reads_words_and_extra_words
#[test]
fn ship_voices_read_words_and_extra_words() {
    let payload = json(&obj! {
        "ship" => obj! {
            "shipGroupId" => 960_007,
            "skins" => list![
                obj! {
                    "id" => 9_600_070,
                    "words" => list![obj! {
                        "key" => "battle",
                        "voiceName" => "旗舰开战",
                        "resourceKey" => "warcry",
                        "voicePath" => "cue/cv-960007-battle/warcry",
                        "l2dAction" => "battle",
                        "spineAction" => "attack",
                        "faceId" => "5",
                        "text" => "风暴啊，请赐予我力量。",
                    }],
                    "extraWords" => list![obj! {"key" => "detail", "voicePath" => "cue/cv-960007/detail_ex1100", "text" => "手伸出来……"}],
                },
                obj! {"id" => 9_600_071, "words" => list![]},
            ],
        },
    });

    let voices = parse_l2d_su_ship_voices(&payload).unwrap();

    let ids: Vec<i128> = voices.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, [9_600_070]);
    let [battle, extra] = voices[0].1.as_slice() else { panic!("two lines") };
    assert_eq!(battle.voice_path, "cue/cv-960007-battle/warcry");
    assert_eq!(battle.face_id, "5");
    assert_eq!(battle.l2d_action, "battle");
    assert!(!battle.is_extra);
    assert_eq!(extra.voice_path, "cue/cv-960007/detail_ex1100");
    assert!(extra.is_extra);
}

// py: tests/test_azurlane_l2d_sources.py::test_l2d_su_character_fingerprint_tracks_skin_changes
#[test]
fn the_character_fingerprint_tracks_skin_changes() {
    let payload = index_payload(vec![ship(1, "biaoqiang", "标枪", "", vec![static_skin(10, "标枪", "biaoqiang")])]);
    let changed = index_payload(vec![ship(1, "biaoqiang", "标枪", "", vec![static_skin(10, "新皮肤", "biaoqiang")])]);
    let first = |source: &str| parse_l2d_su_ship_index(source, "CN").unwrap().characters.remove(0);

    let baseline = l2d_su_character_fingerprint(&first(&payload));

    assert_eq!(l2d_su_character_fingerprint(&first(&payload)), baseline);
    assert_ne!(l2d_su_character_fingerprint(&first(&changed)), baseline);
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_l2d_su_ship_index_prefers_painting_over_prefab_for_the_model_key
#[test]
fn the_painting_key_wins_over_the_prefab_for_the_model_key() {
    let yunlong = set(skin(307_172, "苍空的凯歌", "yunlong_3", "spine"), "prefab", "yunlong_2");
    let payload = index_payload(vec![ship(30717, "yunlong", "云龙", "", vec![yunlong])]);

    let parsed = parse_l2d_su_ship_index(&payload, "CN").unwrap();

    assert_eq!(parsed.characters[0].spine[0].model_key, "yunlong_3");
    assert_eq!(
        parsed.characters[0].spine[0].path,
        "https://static.l2d.su/al/assets/spinepainting/yunlong_3"
    );
}

fn entries_by_key(source: &str) -> BTreeMap<String, (String, String)> {
    parse_nagami_mapping(source)
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| (entry.key, (entry.name, entry.background)))
        .collect()
}

fn pair(name: &str, background: &str) -> (String, String) {
    (name.to_owned(), background.to_owned())
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_nagami_mapping_reads_name_and_background
#[test]
fn the_nagami_mapping_reads_name_and_background() {
    let source = json(&obj! {
        "guanghui_7" => obj! {"name" => "Illustrious - Our Private \"Study\" Session", "bg" => "145"},
        "z23" => "Z23",
    });

    assert_eq!(Value::Dict(parse_nagami_mapping(&source).unwrap().summary()), obj! {"entry_count" => 2});
    assert_eq!(
        entries_by_key(&source),
        BTreeMap::from([
            ("guanghui_7".to_owned(), pair("Illustrious - Our Private \"Study\" Session", "145")),
            ("z23".to_owned(), pair("Z23", "")),
        ])
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_nagami_mapping_reads_ship_grouped_layout
#[test]
fn the_nagami_mapping_reads_the_ship_grouped_layout() {
    let source = json(&obj! {
        "10117" => obj! {
            "names" => obj! {"EN" => "Laffey", "CN" => "拉菲", "JP" => "ラフィー"},
            "skins" => list![obj! {
                "prefab" => "lafei_4",
                "names" => obj! {"EN" => "White Rabbit Welcomes the Spring", "CN" => "白兔迎春"},
                "servers" => vec!["EN", "CN", "JP"],
                "bg" => "102",
                "id" => "101173",
                "tags" => vec!["live2d", "bg"],
            }],
        },
        "20101" => obj! {
            "names" => obj! {"EN" => "", "CN" => "标枪"},
            "skins" => list![obj! {"prefab" => "biaoqiang_2", "names" => obj! {"CN" => "闪耀的白刃"}, "bg" => ""}],
        },
    });

    assert_eq!(
        entries_by_key(&source),
        BTreeMap::from([
            ("lafei_4".to_owned(), pair("Laffey - White Rabbit Welcomes the Spring", "102")),
            ("biaoqiang_2".to_owned(), pair("标枪 - 闪耀的白刃", "")),
        ])
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_parse_nagami_mapping_rejects_skin_without_prefab
#[test]
fn a_nagami_skin_needs_a_prefab() {
    let source = json(&obj! {"10117" => obj! {"names" => obj! {"EN" => "Laffey"}, "skins" => list![obj! {"names" => obj! {"EN" => "X"}}]}});

    let error = parse_nagami_mapping(&source).unwrap_err();

    assert_eq!(error.class, SOURCE_SCHEMA_ERROR);
    assert!(error.message.contains("prefab"), "{}", error.message);
}

// py: tests/test_azurlane_l2d_sources.py::test_l2d_su_asset_urls_map_to_and_from_the_old_cdn_roots
#[test]
fn asset_urls_map_to_and_from_the_old_cdn_roots() {
    let texture = format!("{L2D_SU_STATIC_BASE_URL}/live2d/z23/textures/texture_00.webp");
    assert_eq!(
        l2d_su_legacy_asset_urls(&texture),
        [
            "https://static.l2d.su/al-b/assets/live2d/z23/textures/texture_00.webp".to_owned(),
            "https://static.l2d.su/al/assets/webp/live2d/z23/textures/texture_00.webp".to_owned(),
            "https://static.l2d.su/al/assets/original/live2d/z23/textures/texture_00.webp".to_owned(),
            format!("{L2D_SU_LEGACY_STATIC_BASE_URL}/live2d/z23/textures/texture_00.webp"),
        ]
    );
    assert!(
        l2d_su_legacy_asset_urls(&format!("{L2D_SU_AUDIO_BASE_URL}/cue/cv-1/detail.ogg"))
            .contains(&"https://static.l2d.su/al/assets/original/cue/cv-1/detail.ogg".to_owned())
    );
    assert!(l2d_su_legacy_asset_urls("https://cdn.nagami.moe/live2d/z23/z23.model3.json").is_empty());
    for legacy_texture in l2d_su_legacy_asset_urls(&texture) {
        assert_eq!(l2d_su_asset_url_from_legacy(&legacy_texture), texture);
    }
    assert_eq!(l2d_su_asset_url_from_legacy(&texture), texture);
}

// py: tests/test_azurlane_l2d_sources.py::test_apply_l2d_su_model_paths_corrects_l2d_su_entries_only
#[test]
fn model_paths_from_the_detail_correct_l2d_su_entries_only() {
    use super::{catalog_entry, character, model, spine_url};
    use crate::azurlane::l2d_sources::ModelKind;

    let snapshots = source_snapshots(
        vec![character(
            40505,
            "bisimaiz",
            "俾斯麦Zwei",
            "Bismarck Zwei",
            Vec::new(),
            vec![model(ModelKind::Spine, 405_050, "俾斯麦Zwei", "Bismarck Zwei", &spine_url("bisimaiz"))],
        )],
        &[("shengluyisi_2", "St. Louis - Blue and White Pottery")],
    );
    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();
    let corrected = spine_url("bisimaiZ");

    let paths = HashMap::from([(405_050, corrected.clone()), (999, "https://ignored.example/x".to_owned())]);
    let patched = apply_l2d_su_model_paths(&catalog, &paths);

    assert_eq!(
        catalog_entry(&patched, "azurlane:spine:bisimaiz:bisimaiz").resources.primary_url,
        corrected
    );
    let nagami_entry = catalog_entry(&patched, "azurlane:live2d:shengluyisi:shengluyisi_2");
    assert_eq!(
        nagami_entry.resources.primary_url,
        "https://cdn.nagami.moe/live2d/shengluyisi_2/shengluyisi_2.model3.json"
    );
    assert_eq!(apply_l2d_su_model_paths(&catalog, &HashMap::new()), catalog);
}
