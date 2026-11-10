//! Replays of the goldens `rust/tools/goldens/areas/azurlane.py` writes: the source pipeline
//! over recorded exchanges, resource enumeration, the ship detail parsers, the URL helpers,
//! and catalog round trips.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use fav_core::golden;
use fav_core::pyjson::Value;
use fav_net::{ErrorClass, TransportError};
use fav_runtime::BoxFuture;

use super::super::{
    ArchivedModel, AzurLaneL2DHealthReport, AzurLaneModelCatalog, AzurLaneSourceSnapshots, EnumerationInputs, Fetch, FetchedResponse,
    L2D_SU_PRIMARY_REGION, L2DSuSourceSnapshot, ModelKind, NagamiSourceSnapshot, SourceFetchMetadata, ToPy, build_azurlane_l2d_health_report,
    build_azurlane_model_catalog, case_variant_urls, enumerate_azurlane_model_resources, fetch_source_snapshots, is_hashed_asset_url,
    l2d_su_asset_url_from_legacy, l2d_su_character_fingerprint, l2d_su_legacy_asset_urls, l2d_su_ship_index_url, model_entry_from_dict,
    model_probe_urls, painting_index_url, parse_l2d_su_ship_class, parse_l2d_su_ship_index, parse_l2d_su_ship_models, parse_l2d_su_ship_voices,
    parse_nagami_mapping, parse_painting_index, pin_archived_entries, spine_parts_manifest_url, spine_resource_manifest,
    validate_azurlane_model_catalog_resources,
};
use super::{FETCHED_AT, clock};
use crate::PyError;

/// The first place `actual` and `expected` differ, as a path, or `None` when equal.
pub(crate) fn difference(actual: &Value, expected: &Value, path: &str) -> Option<String> {
    match (actual, expected) {
        (Value::Dict(actual), Value::Dict(expected)) => {
            let actual_keys: Vec<String> = actual.iter().map(|(key, _)| format!("{key:?}")).collect();
            let expected_keys: Vec<String> = expected.iter().map(|(key, _)| format!("{key:?}")).collect();
            if actual_keys != expected_keys {
                return Some(format!("{path}: keys {actual_keys:?} != {expected_keys:?}"));
            }
            actual
                .iter()
                .zip(expected.iter())
                .find_map(|((key, left), (_, right))| difference(left, right, &format!("{path}.{key:?}")))
        }
        (Value::List(actual), Value::List(expected)) => {
            if actual.len() != expected.len() {
                return Some(format!("{path}: {} items != {}", actual.len(), expected.len()));
            }
            actual
                .iter()
                .zip(expected)
                .enumerate()
                .find_map(|(index, (left, right))| difference(left, right, &format!("{path}[{index}]")))
        }
        _ if actual == expected => None,
        _ => Some(format!("{path}: {actual:?} != {expected:?}")),
    }
}

#[track_caller]
pub(crate) fn assert_same(actual: &Value, expected: &Value, context: &str) {
    if let Some(difference) = difference(actual, expected, context) {
        panic!("{difference}");
    }
}

fn error_value(error: &PyError) -> Value {
    obj! {"class" => error.class.clone(), "message" => error.message.clone()}
}

/// `_attempt(producer)`: `{"ok": value}` or `{"error": {"class", "message"}}`.
fn attempt(result: Result<Value, PyError>) -> Value {
    match result {
        Ok(value) => obj! {"ok" => value},
        Err(error) => obj! {"error" => error_value(&error)},
    }
}

fn strings(values: &[String]) -> Value {
    Value::List(values.iter().map(|value| Value::Str(value.clone())).collect())
}

fn key_text(key: &fav_core::pyjson::Key) -> String {
    match key {
        fav_core::pyjson::Key::Str(text) => text.clone(),
        other => panic!("not a string key: {other:?}"),
    }
}

fn text(value: &Value) -> String {
    golden::string(value)
}

/// Serves recorded exchanges in their recorded order.
struct Replay {
    exchanges: Mutex<VecDeque<Value>>,
}

impl Replay {
    fn new(exchanges: &[Value]) -> Self {
        Self {
            exchanges: Mutex::new(exchanges.iter().cloned().collect()),
        }
    }

    #[track_caller]
    fn assert_drained(&self) {
        let left = self.exchanges.lock().unwrap();
        assert!(left.is_empty(), "{} recorded exchanges were never requested", left.len());
    }
}

fn recorded_response(spec: &Value, url: &str) -> Result<FetchedResponse, TransportError> {
    if let Some(message) = spec.get("error") {
        return Err(TransportError {
            class: ErrorClass::ConnectError,
            host: String::new(),
            message: text(message),
        });
    }
    let content = match (spec.get("hex"), spec.get("text")) {
        (Some(hex), _) => decode_hex(&text(hex)),
        (None, Some(body)) => text(body).into_bytes(),
        (None, None) => Vec::new(),
    };
    let headers = spec
        .get("headers")
        .and_then(Value::as_dict)
        .map(|headers| {
            headers
                .iter()
                .map(|(name, value)| (key_text(name).to_ascii_lowercase(), text(value)))
                .collect()
        })
        .unwrap_or_default();
    let status = spec.get("status").map_or(200, golden::int);
    Ok(FetchedResponse {
        status: u16::try_from(status).expect("an HTTP status"),
        url: url.to_owned(),
        headers,
        content,
    })
}

impl Fetch for Replay {
    fn request<'a>(
        &'a self,
        method: &'a str,
        url: &'a str,
        _timeout: Option<std::time::Duration>,
    ) -> BoxFuture<'a, Result<FetchedResponse, TransportError>> {
        let exchange = self
            .exchanges
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("unrecorded request {method} {url}"));
        // httpx records the URL it sent, percent-encoded the way `url::Url` encodes it.
        let sent = url::Url::parse(url).map_or_else(|_| url.to_owned(), String::from);
        let recorded = text(golden::get(&exchange, "url"));
        assert_eq!(
            (method, sent.as_str()),
            (text(golden::get(&exchange, "method")).as_str(), recorded.as_str()),
            "requests left the recorded order"
        );
        let response = recorded_response(golden::get(&exchange, "response"), &recorded);
        Box::pin(async move { response })
    }
}

pub(crate) fn decode_hex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("hex"))
        .collect()
}

struct Pipeline {
    snapshots: AzurLaneSourceSnapshots,
    catalog: AzurLaneModelCatalog,
    health: AzurLaneL2DHealthReport,
}

/// `_run_pipeline`, checked against the golden `case` as it goes.
async fn run_pipeline(exchanges: &[Value], previous: Option<&AzurLaneL2DHealthReport>, case: Option<&Value>) -> Pipeline {
    let client = Replay::new(exchanges);
    let snapshots = fetch_source_snapshots(&client, None, None, &clock).await.unwrap();
    let catalog = build_azurlane_model_catalog(&snapshots).unwrap();
    let validation = validate_azurlane_model_catalog_resources(&catalog, &client, None::<&std::collections::HashSet<String>>, clock())
        .await
        .unwrap();
    client.assert_drained();
    let health = build_azurlane_l2d_health_report(&snapshots, Some(&validation.catalog), Some(&validation), previous, clock()).unwrap();
    if let Some(case) = case {
        let name = text(golden::get(case, "name"));
        assert_same(&snapshots.to_dict(), golden::get(case, "snapshots"), &format!("{name}: snapshots"));
        assert_same(&catalog.to_dict(), golden::get(case, "catalog"), &format!("{name}: catalog"));
        assert_same(
            &Value::Dict(catalog.summary()),
            golden::get(case, "catalog_summary"),
            &format!("{name}: catalog summary"),
        );
        let fingerprints: Vec<Value> = snapshots
            .l2d_su
            .characters
            .iter()
            .map(|character| list![character.char_id, l2d_su_character_fingerprint(character)])
            .collect();
        assert_same(&Value::List(fingerprints), golden::get(case, "fingerprints"), &format!("{name}: fingerprints"));
        assert_same(&validation.to_dict(), golden::get(case, "validation"), &format!("{name}: validation"));
        assert_same(
            &Value::Dict(validation.summary()),
            golden::get(case, "validation_summary"),
            &format!("{name}: validation summary"),
        );
        assert_same(&health.to_dict(), golden::get(case, "health"), &format!("{name}: health"));
    }
    Pipeline {
        snapshots,
        catalog,
        health,
    }
}

#[tokio::test]
async fn the_source_pipeline_matches_the_python_baseline() {
    let document = golden::load("azurlane/sources.json");
    for case in golden::cases(&document) {
        let name = text(golden::get(case, "name"));
        let previous = match golden::get(case, "previous_exchanges") {
            Value::Null => None,
            exchanges => Some(run_pipeline(golden::list(exchanges), None, None).await.health),
        };
        let pipeline = run_pipeline(golden::list(golden::get(case, "exchanges")), previous.as_ref(), Some(case)).await;
        assert_eq!(pipeline.snapshots.l2d_su.metadata.fetched_at, FETCHED_AT);
        let Value::Dict(searches) = golden::get(case, "searches") else { panic!("searches") };
        for (query, expected) in searches.iter() {
            let query = key_text(query);
            let query = query.as_str();
            let model_type = match query {
                "z23" => Some(ModelKind::Spine),
                "twin" => Some(ModelKind::Live2d),
                _ => None,
            };
            let ids: Vec<String> = pipeline
                .catalog
                .search(query, model_type)
                .iter()
                .map(|entry| entry.id.clone())
                .collect();
            assert_same(&strings(&ids), expected, &format!("{name}: search {query:?}"));
        }
    }
}

/// `_enumeration_catalog(index, nagami)`.
fn enumeration_catalog(index: &str, nagami: &str) -> AzurLaneModelCatalog {
    let parsed = parse_l2d_su_ship_index(index, L2D_SU_PRIMARY_REGION).unwrap();
    let metadata = SourceFetchMetadata::for_url(l2d_su_ship_index_url(L2D_SU_PRIMARY_REGION), clock());
    let mut l2d_su = L2DSuSourceSnapshot::new(metadata.clone());
    l2d_su.characters = parsed.characters;
    let mut nagami_snapshot = NagamiSourceSnapshot::new(metadata);
    nagami_snapshot.entries = parse_nagami_mapping(nagami).unwrap().entries;
    build_azurlane_model_catalog(&AzurLaneSourceSnapshots {
        l2d_su,
        nagami: nagami_snapshot,
    })
    .unwrap()
}

fn optional_text(case: &Value, key: &str) -> Option<String> {
    case.get(key).filter(|value| **value != Value::Null).map(text)
}

#[test]
fn resource_enumeration_matches_the_python_baseline() {
    let document = golden::load("azurlane/enumeration.json");
    for case in golden::cases(&document) {
        let name = text(golden::get(case, "name"));
        let catalog = enumeration_catalog(&text(golden::get(case, "index")), &text(golden::get(case, "nagami")));
        let entry_id = text(golden::get(case, "entry_id"));
        let entry = catalog.entries.iter().find(|entry| entry.id == entry_id).expect("the entry");
        let voices = optional_text(case, "detail")
            .map(|detail| parse_l2d_su_ship_voices(&detail).unwrap())
            .unwrap_or_default();
        let voice_lines = voices
            .iter()
            .find(|(id, _)| Some(*id) == entry.costume.id.or(Some(-1)))
            .map(|(_, lines)| lines.clone())
            .unwrap_or_default();
        let painting_index = optional_text(case, "painting_index").map(|source| {
            parse_painting_index(&source, &entry.resources.primary_url, &entry.resources.painting_index_url).unwrap()
        });
        let model3 = optional_text(case, "model3");
        let spine_parts = optional_text(case, "spine_parts");
        let atlases: Option<HashMap<String, String>> = case
            .get("atlases")
            .and_then(Value::as_dict)
            .map(|atlases| atlases.iter().map(|(url, atlas)| (key_text(url), text(atlas))).collect());
        let result = enumerate_azurlane_model_resources(
            entry,
            EnumerationInputs {
                model3_source: model3.as_deref(),
                spine_parts_source: spine_parts.as_deref(),
                atlas_sources: atlases.as_ref(),
                voice_lines: &voice_lines,
                painting_index: painting_index.as_ref(),
            },
        )
        .map(|enumeration| enumeration.to_dict());
        assert_same(&attempt(result), golden::get(case, "result"), &name);
    }
}

#[test]
fn the_ship_detail_parsers_match_the_python_baseline() {
    let document = golden::load("azurlane/details.json");
    for case in golden::cases(&document) {
        let payload = text(golden::get(case, "payload"));
        let models = parse_l2d_su_ship_models(&payload).map(|models| {
            Value::List(
                models
                    .into_iter()
                    .map(|(id, url)| list![id, url])
                    .collect(),
            )
        });
        assert_same(&attempt(models), golden::get(case, "models"), &format!("{payload}: models"));
        let voices = parse_l2d_su_ship_voices(&payload).map(|voices| {
            Value::List(
                voices
                    .into_iter()
                    .map(|(id, lines)| list![id, lines.to_py()])
                    .collect(),
            )
        });
        assert_same(&attempt(voices), golden::get(case, "voices"), &format!("{payload}: voices"));
        let class = parse_l2d_su_ship_class(&payload).map(Value::Str);
        assert_same(&attempt(class), golden::get(case, "class"), &format!("{payload}: class"));
    }
}

#[test]
fn the_url_helpers_match_the_python_baseline() {
    let document = golden::load("azurlane/urls.json");
    for case in golden::cases(&document) {
        let url = text(golden::get(case, "url"));
        let check = |key: &str, actual: Value| assert_same(&actual, golden::get(case, key), &format!("{url}: {key}"));
        check("case_variant_urls", attempt(case_variant_urls(&url).map(|urls| strings(&urls))));
        check("legacy_asset_urls", strings(&l2d_su_legacy_asset_urls(&url)));
        check("asset_url_from_legacy", Value::Str(l2d_su_asset_url_from_legacy(&url)));
        check("is_hashed_asset_url", Value::Bool(is_hashed_asset_url(&url)));
        check("spine_parts_manifest_url", attempt(spine_parts_manifest_url(&url).map(Value::Str)));
        check(
            "spine_resource_manifest",
            attempt(spine_resource_manifest(&url, None).map(|manifest| manifest.to_py())),
        );
        check("painting_index_url", attempt(painting_index_url(&url).map(Value::Str)));
    }
}

#[test]
fn catalog_round_trips_match_the_python_baseline() {
    let document = golden::load("azurlane/catalog_round_trips.json");
    let [entries_case, pinned_case] = golden::cases(&document) else { panic!("two cases") };

    let catalog = enumeration_catalog(&text(golden::get(entries_case, "index")), &text(golden::get(entries_case, "nagami")));
    let expected = golden::list(golden::get(entries_case, "entries"));
    assert_eq!(catalog.entries.len(), expected.len());
    for (entry, expected) in catalog.entries.iter().zip(expected) {
        assert_eq!(Value::Str(entry.id.clone()), *golden::get(expected, "id"));
        let rendered = fav_core::pyjson::loads(&super::json(&entry.to_dict())).unwrap();
        let round_trips = model_entry_from_dict(&rendered).as_ref() == Some(entry);
        assert_eq!(Value::Bool(round_trips), *golden::get(expected, "round_trips"), "{}", entry.id);
        assert_same(
            &attempt(model_probe_urls(entry).map(|urls| strings(&urls))),
            golden::get(expected, "probe_urls"),
            &entry.id,
        );
    }

    let nagami = text(golden::get(pinned_case, "nagami"));
    let legacy = enumeration_catalog(&text(golden::get(pinned_case, "legacy_index")), &nagami);
    let hashed = enumeration_catalog(&text(golden::get(pinned_case, "hashed_index")), &nagami);
    let pinned = golden::get(pinned_case, "pinned");
    let archived: Vec<ArchivedModel> = golden::list(golden::get(pinned, "archived"))
        .iter()
        .map(|item| {
            let entry_id = text(golden::get(item, "entry_id"));
            ArchivedModel {
                entry: legacy.entries.iter().find(|entry| entry.id == entry_id).unwrap().clone(),
                completed: *golden::get(item, "completed") == Value::Bool(true),
            }
        })
        .collect();
    assert_same(
        &pin_archived_entries(&hashed, &archived).to_dict(),
        golden::get(pinned, "catalog"),
        "pinned catalog",
    );
}
