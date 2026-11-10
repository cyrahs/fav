//! The health report: both sources' endpoints, the catalog, the resource checks, and the
//! drift since the previous report (`build_azurlane_l2d_health_report`).

use std::collections::BTreeSet;

use fav_core::pyjson::Value;

use super::catalog::{build_azurlane_model_catalog, l2d_su_source_entry_ids, nagami_source_entry_ids};
use super::model::{
    AvailabilityState, AzurLaneL2DHealthReport, AzurLaneModelCatalog, AzurLaneResourceValidationReport, AzurLaneSourceSnapshots,
    CatalogHealthReport, DriftSummary, L2DSuSourceSnapshot, NagamiSourceSnapshot, ResourceHealthReport, SourceDrift, SourceEndpointHealth,
    SourceHealthReport,
};
use crate::PyError;

/// `build_azurlane_l2d_health_report`, stamped `checked_at`.
pub fn build_azurlane_l2d_health_report(
    snapshots: &AzurLaneSourceSnapshots,
    catalog: Option<&AzurLaneModelCatalog>,
    resource_validation: Option<&AzurLaneResourceValidationReport>,
    previous_report: Option<&AzurLaneL2DHealthReport>,
    checked_at: String,
) -> Result<AzurLaneL2DHealthReport, PyError> {
    let current_catalog = match catalog {
        Some(catalog) => catalog.clone(),
        None => build_azurlane_model_catalog(snapshots)?,
    };
    let source_health = SourceHealthReport {
        l2d_su: l2d_su_endpoint_health(&snapshots.l2d_su)?,
        nagami: nagami_endpoint_health(&snapshots.nagami),
    };
    let catalog_health = catalog_health_report(&current_catalog, previous_report);
    let resource_health = resource_health_report(&current_catalog, resource_validation, previous_report);
    let drift_summary = drift_summary(&source_health, snapshots, &catalog_health, &resource_health, previous_report)?;
    Ok(AzurLaneL2DHealthReport {
        checked_at,
        source_health,
        catalog_health,
        resource_health,
        drift_summary,
        snapshots: Some(snapshots.clone()),
        catalog: Some(current_catalog),
        resource_validation: resource_validation.cloned(),
    })
}

fn count(items: usize) -> i128 {
    i128::try_from(items).unwrap_or(i128::MAX)
}

fn l2d_su_endpoint_health(snapshot: &L2DSuSourceSnapshot) -> Result<SourceEndpointHealth, PyError> {
    Ok(SourceEndpointHealth {
        source: "l2d.su",
        url: snapshot.metadata.url.clone(),
        http_status: snapshot.metadata.http_status,
        etag: snapshot.metadata.etag.clone(),
        last_modified: snapshot.metadata.last_modified.clone(),
        fetched_at: snapshot.metadata.fetched_at.clone(),
        entry_count: snapshot.characters.iter().map(|character| count(character.models().count())).sum(),
        entry_ids: l2d_su_source_entry_ids(snapshot)?,
        errors: snapshot.errors.clone(),
    })
}

fn nagami_endpoint_health(snapshot: &NagamiSourceSnapshot) -> SourceEndpointHealth {
    SourceEndpointHealth {
        source: "nagami",
        url: snapshot.metadata.url.clone(),
        http_status: snapshot.metadata.http_status,
        etag: snapshot.metadata.etag.clone(),
        last_modified: snapshot.metadata.last_modified.clone(),
        fetched_at: snapshot.metadata.fetched_at.clone(),
        entry_count: count(snapshot.entries.len()),
        entry_ids: nagami_source_entry_ids(snapshot),
        errors: snapshot.errors.clone(),
    }
}

/// `_sorted_delta`: `sorted(set(current) - set(previous))`.
fn sorted_delta(current: &[String], previous: &[String]) -> Vec<String> {
    let previous: BTreeSet<&String> = previous.iter().collect();
    let delta: BTreeSet<&String> = current.iter().filter(|id| !previous.contains(id)).collect();
    delta.into_iter().cloned().collect()
}

fn catalog_health_report(catalog: &AzurLaneModelCatalog, previous_report: Option<&AzurLaneL2DHealthReport>) -> CatalogHealthReport {
    let summary = catalog.summary();
    let entry_ids: Vec<String> = catalog.entries.iter().map(|entry| entry.id.clone()).collect();
    let previous_ids = previous_report.map_or(&entry_ids, |report| &report.catalog_health.entry_ids);
    let dict_of = |key: &str| match summary.get(key) {
        Some(Value::Dict(dict)) => dict.clone(),
        _ => fav_core::pyjson::Dict::new(),
    };
    CatalogHealthReport {
        entry_count: count(entry_ids.len()),
        by_type: dict_of("by_type"),
        by_source: dict_of("by_source"),
        new_entry_ids: sorted_delta(&entry_ids, previous_ids),
        removed_entry_ids: sorted_delta(previous_ids, &entry_ids),
        entry_ids,
    }
}

fn resource_health_report(
    catalog: &AzurLaneModelCatalog,
    resource_validation: Option<&AzurLaneResourceValidationReport>,
    previous_report: Option<&AzurLaneL2DHealthReport>,
) -> ResourceHealthReport {
    // `{entry_id: state}`: a repeated id keeps its first place and its last state.
    let mut states: Vec<(&str, AvailabilityState)> = Vec::new();
    let pairs: Vec<(&str, AvailabilityState)> = match resource_validation {
        None => catalog
            .entries
            .iter()
            .map(|entry| (entry.id.as_str(), entry.availability.state))
            .collect(),
        Some(report) => report
            .entries
            .iter()
            .map(|entry| (entry.entry_id.as_str(), entry.availability.state))
            .collect(),
    };
    for (id, state) in pairs {
        if let Some(slot) = states.iter_mut().find(|(existing, _)| *existing == id) {
            slot.1 = state;
        } else {
            states.push((id, state));
        }
    }
    let checked_at = resource_validation.map(|report| report.checked_at.clone()).unwrap_or_default();
    let broken_ids: Vec<String> = states
        .iter()
        .filter(|(_, state)| *state == AvailabilityState::Broken)
        .map(|(id, _)| (*id).to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let previous_broken_ids = previous_report.map_or(&broken_ids, |report| &report.resource_health.broken_entry_ids);
    let of_state = |wanted: AvailabilityState| count(states.iter().filter(|(_, state)| *state == wanted).count());
    ResourceHealthReport {
        checked_at,
        entry_count: count(states.len()),
        broken_resource_count: count(broken_ids.len()),
        unchecked_resource_count: of_state(AvailabilityState::Unchecked),
        fallback_only_count: of_state(AvailabilityState::FallbackOnly),
        newly_broken_entry_ids: sorted_delta(&broken_ids, previous_broken_ids),
        recovered_entry_ids: sorted_delta(previous_broken_ids, &broken_ids),
        broken_entry_ids: broken_ids,
    }
}

fn drift_summary(
    source_health: &SourceHealthReport,
    snapshots: &AzurLaneSourceSnapshots,
    catalog_health: &CatalogHealthReport,
    resource_health: &ResourceHealthReport,
    previous_report: Option<&AzurLaneL2DHealthReport>,
) -> Result<DriftSummary, PyError> {
    let previous_catalog_count = previous_report.map(|report| report.catalog_health.entry_count);
    let previous_broken_count = previous_report.map(|report| report.resource_health.broken_resource_count);
    Ok(DriftSummary {
        l2d_su: source_drift(
            &source_health.l2d_su,
            previous_report.map(|report| &report.source_health.l2d_su),
            &l2d_su_source_entry_ids(&snapshots.l2d_su)?,
        ),
        nagami: source_drift(
            &source_health.nagami,
            previous_report.map(|report| &report.source_health.nagami),
            &nagami_source_entry_ids(&snapshots.nagami),
        ),
        previous_catalog_entry_count: previous_catalog_count,
        current_catalog_entry_count: catalog_health.entry_count,
        catalog_entry_count_delta: previous_catalog_count.map(|previous| catalog_health.entry_count - previous),
        newly_added_resource_count: count(catalog_health.new_entry_ids.len()),
        broken_resource_count_delta: previous_broken_count.map(|previous| resource_health.broken_resource_count - previous),
    })
}

fn source_drift(current: &SourceEndpointHealth, previous: Option<&SourceEndpointHealth>, current_entry_ids: &[String]) -> SourceDrift {
    let Some(previous) = previous else {
        return SourceDrift {
            source: current.source,
            previous_entry_count: None,
            current_entry_count: current.entry_count,
            entry_count_delta: None,
            added_entry_ids: Vec::new(),
            removed_entry_ids: Vec::new(),
            etag_changed: false,
            last_modified_changed: false,
            http_status_changed: false,
        };
    };
    SourceDrift {
        source: current.source,
        previous_entry_count: Some(previous.entry_count),
        current_entry_count: current.entry_count,
        entry_count_delta: Some(current.entry_count - previous.entry_count),
        added_entry_ids: sorted_delta(current_entry_ids, &previous.entry_ids),
        removed_entry_ids: sorted_delta(&previous.entry_ids, current_entry_ids),
        etag_changed: current.etag != previous.etag,
        last_modified_changed: current.last_modified != previous.last_modified,
        http_status_changed: current.http_status != previous.http_status,
    }
}
