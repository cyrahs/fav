//! Resource validation of catalog entries against their primary and fallback URLs.

use std::collections::{BTreeSet, HashSet};

use super::super::{
    AvailabilityState, ModelKind, ValidationSource, build_azurlane_model_catalog, validate_azurlane_model_catalog_resources,
};
use super::{
    FETCHED_AT, FakeFetch, character, clock, live2d_model3_payload, model, respond, source_snapshots, spine_parts_payload, spine_url,
};

const NO_FILTER: Option<&HashSet<String>> = None;

fn check_kinds(checks: &[super::super::ResourceCheck]) -> BTreeSet<&'static str> {
    checks.iter().map(|check| check.kind.as_str()).collect()
}

fn guanghui(primary_url: &str, nagami: &[(&str, &str)]) -> super::super::AzurLaneSourceSnapshots {
    source_snapshots(
        vec![character(
            1,
            "guanghui",
            "光辉",
            "Illustrious",
            vec![model(ModelKind::Live2d, 7, "私人茶会", "Our Private Study Session", primary_url)],
            Vec::new(),
        )],
        nagami,
    )
}

// py: tests/test_azurlane_l2d_sources.py::test_validate_catalog_resources_marks_live2d_valid_and_populates_resource_summary
#[tokio::test]
async fn a_live2d_model_is_valid_and_its_resource_summary_filled() {
    let primary_url = "https://static.example/live2d/azurlane/guanghui_7/guanghui_7.model3.json";
    let head_urls: HashSet<&str> = HashSet::from([
        "https://static.example/live2d/azurlane/guanghui_7/test_model.moc3",
        "https://static.example/live2d/azurlane/guanghui_7/textures/texture_00.webp",
        "https://static.example/live2d/azurlane/guanghui_7/textures/texture_01.webp",
        "https://static.example/live2d/azurlane/guanghui_7/guanghui_7.cdi3.json",
    ]);
    let client = FakeFetch::new(move |method, url| {
        if method == "GET" && url == primary_url {
            return respond(200, &live2d_model3_payload(None));
        }
        if method == "HEAD" && head_urls.contains(url) {
            return respond(200, "");
        }
        respond(404, "")
    });

    let catalog = build_azurlane_model_catalog(&guanghui(primary_url, &[])).unwrap();
    let report = validate_azurlane_model_catalog_resources(&catalog, &client, NO_FILTER, clock())
        .await
        .unwrap();

    let validation = &report.entries[0];
    let entry = &report.catalog.entries[0];
    assert!(validation.has_available_resources());
    assert_eq!(validation.resource_summary, entry.resource_summary);
    assert_eq!(entry.availability.state, AvailabilityState::Valid);
    assert_eq!(entry.availability.validated_url, primary_url);
    assert_eq!(entry.availability.checked_at, FETCHED_AT);
    let summary = &entry.resource_summary;
    assert_eq!(summary.moc3, "https://static.example/live2d/azurlane/guanghui_7/test_model.moc3");
    assert_eq!(
        summary.textures,
        [
            "https://static.example/live2d/azurlane/guanghui_7/textures/texture_00.webp",
            "https://static.example/live2d/azurlane/guanghui_7/textures/texture_01.webp",
        ]
    );
    assert_eq!(
        summary.physics,
        "https://static.example/live2d/azurlane/guanghui_7/test_model.physics3.json"
    );
    assert_eq!(summary.motions, ["Idle", "TapBody"]);
    assert_eq!(summary.expressions, ["smile"]);
    assert!(summary.has_audio);
    assert!(summary.has_text);
    assert!(summary.has_display_info);
    assert_eq!(
        entry.resources.display_info_url,
        "https://static.example/live2d/azurlane/guanghui_7/guanghui_7.cdi3.json"
    );
    assert_eq!(
        check_kinds(&validation.checks),
        BTreeSet::from(["live2d.model3", "live2d.moc3", "live2d.texture", "live2d.display-info"])
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_validate_catalog_resources_marks_live2d_fallback_only_when_primary_is_broken
#[tokio::test]
async fn a_live2d_model_with_a_broken_primary_is_fallback_only() {
    let primary_url = "https://static.example/live2d/azurlane/guanghui_7/guanghui_7.model3.json";
    let fallback_url = "https://cdn.nagami.moe/live2d/guanghui_7/guanghui_7.model3.json";
    let head_urls: HashSet<&str> = HashSet::from([
        "https://cdn.nagami.moe/live2d/guanghui_7/test_model.moc3",
        "https://cdn.nagami.moe/live2d/guanghui_7/textures/texture_00.webp",
        "https://cdn.nagami.moe/live2d/guanghui_7/textures/texture_01.webp",
        "https://cdn.nagami.moe/live2d/guanghui_7/display.cdi3.json",
    ]);
    let client = FakeFetch::new(move |method, url| {
        if method == "GET" && url == primary_url {
            return respond(404, "");
        }
        if method == "GET" && url == fallback_url {
            return respond(200, &live2d_model3_payload(Some("display.cdi3.json")));
        }
        if method == "HEAD" && head_urls.contains(url) {
            return respond(200, "");
        }
        respond(404, "")
    });

    let catalog =
        build_azurlane_model_catalog(&guanghui(primary_url, &[("guanghui_7", "Illustrious - Our Private Study Session")])).unwrap();
    let report = validate_azurlane_model_catalog_resources(&catalog, &client, NO_FILTER, clock())
        .await
        .unwrap();

    let entry = &report.catalog.entries[0];
    assert_eq!(entry.availability.state, AvailabilityState::FallbackOnly);
    assert_eq!(entry.availability.validated_url, fallback_url);
    assert_eq!(
        entry.resource_summary.moc3,
        "https://cdn.nagami.moe/live2d/guanghui_7/test_model.moc3"
    );
    assert_eq!(
        entry.resources.display_info_url,
        "https://cdn.nagami.moe/live2d/guanghui_7/display.cdi3.json"
    );
    let sources: Vec<Option<ValidationSource>> = report.entries[0]
        .checks
        .iter()
        .filter(|check| check.kind.as_str() == "live2d.model3")
        .map(|check| check.source)
        .collect();
    assert_eq!(sources, [Some(ValidationSource::Primary), Some(ValidationSource::Fallback)]);
}

fn single_spine(
    char_id: i128,
    char_key: &str,
    char_name: &str,
    costume_id: i128,
    costume_name: &str,
    path: &str,
) -> super::super::AzurLaneSourceSnapshots {
    source_snapshots(
        vec![character(
            char_id,
            char_key,
            char_name,
            char_name,
            Vec::new(),
            vec![model(ModelKind::Spine, costume_id, costume_name, costume_name, path)],
        )],
        &[],
    )
}

// py: tests/test_azurlane_l2d_sources.py::test_validate_catalog_resources_marks_spine_valid_and_checks_atlas_texture
#[tokio::test]
async fn a_spine_model_is_valid_once_its_atlas_texture_answers() {
    let spine_base_url = "https://static.example/live2d/azurlane/yilisi_2_doa";
    let skel_url = "https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.skel";
    let atlas_url = "https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.atlas";
    let texture_url = "https://static.example/live2d/azurlane/yilisi_2_doa/yilisi_2_doa.webp";
    let client = FakeFetch::new(move |method, url| match (method, url) {
        ("HEAD", url) if url == skel_url || url == texture_url => respond(200, ""),
        ("GET", url) if url == atlas_url => respond(200, "\nyilisi_2_doa.webp\nsize: 4096,4096\nformat: RGBA8888\n"),
        _ => respond(404, ""),
    });

    let catalog = build_azurlane_model_catalog(&single_spine(2, "yilisi", "伊莉丝", 2, "某位女神的午后", spine_base_url)).unwrap();
    let report = validate_azurlane_model_catalog_resources(&catalog, &client, NO_FILTER, clock())
        .await
        .unwrap();

    let entry = &report.catalog.entries[0];
    assert_eq!(entry.availability.state, AvailabilityState::Valid);
    assert_eq!(entry.availability.validated_url, spine_base_url);
    assert_eq!(entry.resource_summary.skeleton, skel_url);
    assert_eq!(entry.resource_summary.textures, [texture_url]);
    assert_eq!(
        check_kinds(&report.entries[0].checks),
        BTreeSet::from(["spine.skel", "spine.atlas", "spine.texture"])
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_validate_catalog_resources_falls_back_to_the_spine_parts_manifest
#[tokio::test]
async fn spine_validation_falls_back_to_the_parts_manifest() {
    let spine_base_url = spine_url("lafei_12");
    let parts_url = format!("{spine_base_url}/lafei_12.json");
    let found: HashSet<String> = ["lafei_12B.skel", "lafei_12T.skel", "lafei_12B.webp", "lafei_12T.webp"]
        .iter()
        .map(|name| format!("{spine_base_url}/{name}"))
        .collect();
    let base = spine_base_url.clone();
    let parts = parts_url.clone();
    let client = FakeFetch::new(move |method, url| {
        if method == "HEAD" && found.contains(url) {
            return respond(200, "");
        }
        if method == "GET" && url == parts {
            return respond(200, &spine_parts_payload("lafei_12", &["B", "T"]));
        }
        for page in ["lafei_12B", "lafei_12T"] {
            if method == "GET" && url == format!("{base}/{page}.atlas") {
                return respond(200, &format!("\n{page}.webp\nsize: 4096,4096\nformat: RGBA8888\n"));
            }
        }
        respond(404, "")
    });

    let catalog = build_azurlane_model_catalog(&single_spine(10117, "lafei", "拉菲", 131_172, "慵懒时光", &spine_base_url)).unwrap();
    let report = validate_azurlane_model_catalog_resources(&catalog, &client, NO_FILTER, clock())
        .await
        .unwrap();

    let entry = &report.catalog.entries[0];
    assert_eq!(
        client.requests()[..2],
        [
            ("HEAD".to_owned(), format!("{spine_base_url}/lafei_12.skel")),
            ("GET".to_owned(), parts_url),
        ]
    );
    assert_eq!(entry.availability.state, AvailabilityState::Valid);
    assert_eq!(entry.resource_summary.skeleton, format!("{spine_base_url}/lafei_12B.skel"));
    let textures: BTreeSet<String> = entry.resource_summary.textures.iter().cloned().collect();
    assert_eq!(
        textures,
        BTreeSet::from([
            format!("{spine_base_url}/lafei_12B.webp"),
            format!("{spine_base_url}/lafei_12T.webp"),
        ])
    );
    assert_eq!(
        check_kinds(&report.entries[0].checks),
        BTreeSet::from(["spine.parts", "spine.skel", "spine.atlas", "spine.texture"])
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_validate_catalog_resources_checks_l2d_su_spine_suffix_assets_without_suffix
#[tokio::test]
async fn a_spine_suffixed_directory_is_checked_without_the_suffix() {
    let spine_base_url = "https://static.example/live2d/azurlane/aerbien_4-spine";
    let skel_url = "https://static.example/live2d/azurlane/aerbien_4-spine/aerbien_4.skel";
    let atlas_url = "https://static.example/live2d/azurlane/aerbien_4-spine/aerbien_4.atlas";
    let texture_url = "https://static.example/live2d/azurlane/aerbien_4-spine/aerbien_4.webp";
    let client = FakeFetch::new(move |method, url| match (method, url) {
        ("HEAD", url) if url == skel_url || url == texture_url => respond(200, ""),
        ("GET", url) if url == atlas_url => respond(200, "\naerbien_4.webp\nsize: 4096,4096\nformat: RGBA8888\n"),
        _ => respond(404, ""),
    });

    let snapshots = single_spine(3, "aerbien", "Albion", 4, "Silvermoon Faerie Princess", spine_base_url);
    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();
    let report = validate_azurlane_model_catalog_resources(&catalog, &client, NO_FILTER, clock())
        .await
        .unwrap();

    let entry = &report.catalog.entries[0];
    assert_eq!(entry.availability.state, AvailabilityState::Valid);
    assert_eq!(entry.availability.validated_url, spine_base_url);
    assert_eq!(entry.resource_summary.textures, [texture_url]);
    let checks: Vec<(&str, &str)> = report.entries[0]
        .checks
        .iter()
        .map(|check| (check.kind.as_str(), check.url.as_str()))
        .collect();
    assert_eq!(
        checks,
        [("spine.skel", skel_url), ("spine.atlas", atlas_url), ("spine.texture", texture_url)]
    );
    let owned = |method: &str, url: &str| (method.to_owned(), url.to_owned());
    assert_eq!(
        client.requests(),
        [owned("HEAD", skel_url), owned("GET", atlas_url), owned("HEAD", texture_url)]
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_validate_catalog_resources_records_broken_urls_and_leaves_unselected_entries_unchecked
#[tokio::test]
async fn broken_urls_are_recorded_and_unselected_entries_left_unchecked() {
    let broken_url = "https://static.example/live2d/azurlane/missing/missing.model3.json";
    let unchecked_url = "https://static.example/live2d/azurlane/unchecked/unchecked.model3.json";
    let snapshots = source_snapshots(
        vec![
            character(
                1,
                "missing",
                "失踪",
                "Missing",
                vec![model(ModelKind::Live2d, 1, "Missing", "Missing", broken_url)],
                Vec::new(),
            ),
            character(
                2,
                "unchecked",
                "未检查",
                "Unchecked",
                vec![model(ModelKind::Live2d, 2, "Unchecked", "Unchecked", unchecked_url)],
                Vec::new(),
            ),
        ],
        &[],
    );
    let client = FakeFetch::new(move |_, url| {
        assert_eq!(url, broken_url);
        respond(404, "")
    });

    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();
    let selected_id = "azurlane:live2d:missing:missing";
    let selected = HashSet::from([selected_id.to_owned()]);
    let report = validate_azurlane_model_catalog_resources(&catalog, &client, Some(&selected), clock())
        .await
        .unwrap();

    let entry = |entry_id: &str| report.entries.iter().find(|entry| entry.entry_id == entry_id).unwrap();
    assert_eq!(entry(selected_id).availability.state, AvailabilityState::Broken);
    assert_eq!(entry(selected_id).checks[0].url, broken_url);
    assert_eq!(entry(selected_id).checks[0].http_status, Some(404));
    let unchecked = entry("azurlane:live2d:unchecked:unchecked");
    assert_eq!(unchecked.availability.state, AvailabilityState::Unchecked);
    assert!(unchecked.checks.is_empty());
    assert_eq!(
        report.summary().get("by_state"),
        Some(&obj! {"valid" => 0, "fallback-only" => 0, "broken" => 1, "unchecked" => 1})
    );
}
