//! The health report and its drift against the previous report.

use std::collections::{BTreeSet, HashSet};

use super::super::{
    AzurLaneSourceSnapshots, ModelKind, SourceFetchMetadata, build_azurlane_l2d_health_report, build_azurlane_model_catalog,
    validate_azurlane_model_catalog_resources,
};
use super::{FakeFetch, character, clock, keys, live2d_model3_payload, model, primary_index_url, respond, source_snapshots};

const NO_FILTER: Option<&HashSet<String>> = None;

fn live2d_character(char_id: i128, key: &str, name: &str, url: &str) -> super::super::L2DSuCharacterSnapshot {
    character(
        char_id,
        key,
        name,
        name,
        vec![model(ModelKind::Live2d, char_id, name, name, url)],
        Vec::new(),
    )
}

fn with_etag(mut snapshots: AzurLaneSourceSnapshots, etag: &str, last_modified: &str) -> AzurLaneSourceSnapshots {
    snapshots.l2d_su.metadata = SourceFetchMetadata {
        url: primary_index_url(),
        http_status: Some(200),
        etag: etag.to_owned(),
        last_modified: last_modified.to_owned(),
        fetched_at: clock(),
    };
    snapshots
}

// py: tests/test_azurlane_l2d_sources.py::test_health_report_detects_source_catalog_and_resource_drift
#[tokio::test]
async fn the_health_report_detects_source_catalog_and_resource_drift() {
    let previous_url = "https://static.example/live2d/azurlane/old/old.model3.json";
    let recovered_url = "https://static.example/live2d/azurlane/old/old.model3.json";
    let new_url = "https://static.example/live2d/azurlane/new/new.model3.json";
    let previous_snapshots = source_snapshots(vec![live2d_character(1, "old", "Old", previous_url)], &[]);
    let current_snapshots = source_snapshots(
        vec![
            live2d_character(1, "old", "Old", recovered_url),
            live2d_character(2, "new", "New", new_url),
        ],
        &[],
    );
    let previous_snapshots = with_etag(previous_snapshots, "\"old\"", "");
    let current_snapshots = with_etag(current_snapshots, "\"new\"", "");

    let previous_catalog = build_azurlane_model_catalog(&previous_snapshots).unwrap();
    let previous_client = FakeFetch::new(move |_, url| {
        assert_eq!(url, previous_url);
        respond(404, "")
    });
    let previous_validation = validate_azurlane_model_catalog_resources(&previous_catalog, &previous_client, NO_FILTER, clock())
        .await
        .unwrap();
    let previous_health = build_azurlane_l2d_health_report(
        &previous_snapshots,
        Some(&previous_catalog),
        Some(&previous_validation),
        None,
        clock(),
    )
    .unwrap();

    let current_catalog = build_azurlane_model_catalog(&current_snapshots).unwrap();
    let recovered_head_urls: HashSet<&str> = HashSet::from([
        "https://static.example/live2d/azurlane/old/test_model.moc3",
        "https://static.example/live2d/azurlane/old/textures/texture_00.webp",
        "https://static.example/live2d/azurlane/old/textures/texture_01.webp",
        "https://static.example/live2d/azurlane/old/old.cdi3.json",
    ]);
    let current_client = FakeFetch::new(move |method, url| {
        if method == "GET" && url == recovered_url {
            return respond(200, &live2d_model3_payload(None));
        }
        if method == "HEAD" && recovered_head_urls.contains(url) {
            return respond(200, "");
        }
        respond(404, "")
    });
    let current_validation = validate_azurlane_model_catalog_resources(&current_catalog, &current_client, NO_FILTER, clock())
        .await
        .unwrap();
    let current_health = build_azurlane_l2d_health_report(
        &current_snapshots,
        Some(&current_catalog),
        Some(&current_validation),
        Some(&previous_health),
        clock(),
    )
    .unwrap();

    assert!(current_health.drift_summary.l2d_su.etag_changed);
    assert_eq!(current_health.drift_summary.l2d_su.entry_count_delta, Some(1));
    assert_eq!(current_health.catalog_health.new_entry_ids, ["azurlane:live2d:new:new"]);
    assert_eq!(current_health.drift_summary.newly_added_resource_count, 1);
    assert_eq!(current_health.resource_health.broken_resource_count, 1);
    assert_eq!(current_health.resource_health.newly_broken_entry_ids, ["azurlane:live2d:new:new"]);
    assert_eq!(current_health.resource_health.recovered_entry_ids, ["azurlane:live2d:old:old"]);
    assert_eq!(
        keys(&current_health.to_dict()),
        BTreeSet::from(
            [
                "checked_at",
                "source_health",
                "catalog_health",
                "resource_health",
                "drift_summary",
                "snapshots",
                "catalog",
                "resource_validation",
            ]
            .map(str::to_owned)
        )
    );
}

// py: tests/test_azurlane_l2d_sources.py::test_health_report_marks_source_changed_when_only_entry_count_delta_changes
#[test]
fn a_source_changes_when_only_its_entry_count_does() {
    let snapshots = source_snapshots(
        vec![live2d_character(
            1,
            "same",
            "Same",
            "https://static.example/live2d/azurlane/same/same.model3.json",
        )],
        &[],
    );
    let snapshots = with_etag(snapshots, "\"same\"", "Thu, 14 May 2026 00:00:00 GMT");

    let mut previous_health = build_azurlane_l2d_health_report(&snapshots, None, None, None, clock()).unwrap();
    previous_health.source_health.l2d_su.entry_count += 1;

    let current_health = build_azurlane_l2d_health_report(&snapshots, None, None, Some(&previous_health), clock()).unwrap();

    assert_eq!(
        current_health.source_health.l2d_su.entry_ids,
        previous_health.source_health.l2d_su.entry_ids
    );
    let drift = &current_health.drift_summary.l2d_su;
    assert_eq!(drift.entry_count_delta, Some(-1));
    assert!(drift.added_entry_ids.is_empty());
    assert!(drift.removed_entry_ids.is_empty());
    assert!(!drift.etag_changed);
    assert!(!drift.last_modified_changed);
    assert!(!drift.http_status_changed);
    assert!(drift.changed());
}
