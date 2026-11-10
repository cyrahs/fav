//! Everything that makes requests: the source snapshots, the resource validation, the
//! origin client's options and the origin probe.
//!
//! Python takes a synchronous `httpx.Client`; here the requests go through [`Fetch`], which
//! [`HttpClient`] implements and tests replace (as Python's tests use `MockTransport`).
//! Per-request timeouts Python passes (`timeout=` on `client.request`) are the client's own
//! here, except the probe's exit-address lookup, which keeps its 20 s bound.

use std::collections::HashSet;
use std::time::Duration;

use fav_core::pytext::py_strip;
use fav_net::client::BuildError;
use fav_net::connect_to::ConnectTo;
use fav_net::reqwest::Method;
use fav_net::{ClientOptions, ErrorClass, HttpClient, Request, TransportError};
use fav_runtime::BoxFuture;

use super::catalog::{build_azurlane_model_catalog, l2d_su_primary_ship_index_url, l2d_su_ship_index_url};
use super::health::build_azurlane_l2d_health_report;
use super::model::{
    AssetKind, AvailabilityState, AzurLaneL2DHealthReport, AzurLaneModelCatalog, AzurLaneResourceValidationReport, AzurLaneSourceSnapshots, HTTP_ERROR_MIN,
    L2DSuIndexData, L2DSuOriginProbe, L2DSuSourceSnapshot, ModelAvailability, ModelEntry, ModelKind, ModelResourceSummary,
    NagamiSourceSnapshot, ResourceCandidateValidation, ResourceCheck, ResourceValidationEntry, SourceErrorKind, SourceFetchMetadata,
    SourceSnapshotError, SpineResourceManifest, ValidationSource, ValidationStatus,
};
use super::parse::{
    merge_l2d_su_english_names, parse_l2d_su_ship_index, parse_live2d_model3, parse_nagami_mapping, parse_spine_atlas_texture_urls,
    same_name_display_info_url, spine_parts_manifest_url, spine_resource_manifest,
};
use super::{
    L2D_SU_ENGLISH_REGION, L2D_SU_HOST, L2D_SU_PRIMARY_REGION, NAGAMI_MAPPING_URL, SOURCE_PARSE_ERROR, SOURCE_SCHEMA_ERROR,
    request_headers, unique_non_empty,
};
use crate::PyError;
use crate::azurlane::protected;

/// A response as the sources read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedResponse {
    pub status: u16,
    /// `str(response.url)`: the final URL after redirects.
    pub url: String,
    /// Header names in lowercase, values as httpx decodes them.
    pub headers: Vec<(String, String)>,
    pub content: Vec<u8>,
}

impl FetchedResponse {
    /// `response.headers.get(name, '')`.
    pub fn header(&self, name: &str) -> String {
        let name = name.to_ascii_lowercase();
        let values: Vec<&str> = self
            .headers
            .iter()
            .filter(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
            .collect();
        values.join(", ")
    }

    /// `response.text`: UTF-8 with replacement (DIVERGENCES.md: a declared charset other
    /// than UTF-8 is not honoured).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.content).into_owned()
    }
}

/// `client.request(method, url)`.
pub trait Fetch: Sync {
    /// `timeout` bounds this request on top of the client's own timeouts.
    fn request<'a>(
        &'a self,
        method: &'a str,
        url: &'a str,
        timeout: Option<Duration>,
    ) -> BoxFuture<'a, Result<FetchedResponse, TransportError>>;
}

impl Fetch for HttpClient {
    fn request<'a>(
        &'a self,
        method: &'a str,
        url: &'a str,
        timeout: Option<Duration>,
    ) -> BoxFuture<'a, Result<FetchedResponse, TransportError>> {
        Box::pin(async move {
            let send = async {
                let method = Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET);
                let response = self.send(Request::new(method, url)).await?;
                let status = response.status();
                let final_url = response.url().to_string();
                let mut headers = Vec::new();
                for name in response.headers().keys() {
                    if let Some(value) = response.header(name.as_str()) {
                        headers.push((name.as_str().to_owned(), value));
                    }
                }
                let content = response.bytes().await?.to_vec();
                Ok(FetchedResponse {
                    status,
                    url: final_url,
                    headers,
                    content,
                })
            };
            match timeout {
                None => send.await,
                Some(limit) => tokio::time::timeout(limit, send).await.unwrap_or_else(|_| {
                    Err(TransportError {
                        class: ErrorClass::ReadTimeout,
                        host: url::Url::parse(url)
                            .ok()
                            .and_then(|url| url.host_str().map(str::to_owned))
                            .unwrap_or_default(),
                        message: "timed out".to_owned(),
                    })
                }),
            }
        })
    }
}

/// Where `SourceFetchMetadata.fetched_at` and `checked_at` come from (`_utc_now_iso`).
pub type Clock<'a> = &'a (dyn Fn() -> String + Send + Sync);

/// `origin_throttle`: waited for before each l2d.su origin request.
pub type OriginThrottle<'a> = &'a (dyn Fn() -> BoxFuture<'static, ()> + Send + Sync);

// ---------- source snapshots ----------

struct Fetched {
    response: Option<FetchedResponse>,
    metadata: SourceFetchMetadata,
    error: Option<SourceSnapshotError>,
}

fn source_error(kind: SourceErrorKind, message: String, metadata: &SourceFetchMetadata) -> SourceSnapshotError {
    SourceSnapshotError {
        kind,
        message,
        url: metadata.url.clone(),
        http_status: metadata.http_status,
    }
}

/// `_fetch_source`.
async fn fetch_source(client: &dyn Fetch, url: &str, clock: Clock<'_>) -> Fetched {
    let response = match client.request("GET", url, None).await {
        Ok(response) => response,
        Err(error) => {
            return Fetched {
                response: None,
                metadata: SourceFetchMetadata::for_url(url, clock()),
                error: Some(SourceSnapshotError {
                    kind: SourceErrorKind::Network,
                    message: format!("Failed to fetch {url}: {}", error.message),
                    url: url.to_owned(),
                    http_status: None,
                }),
            };
        }
    };
    let metadata = SourceFetchMetadata {
        url: if response.url.is_empty() {
            url.to_owned()
        } else {
            response.url.clone()
        },
        http_status: Some(i128::from(response.status)),
        etag: response.header("etag"),
        last_modified: response.header("last-modified"),
        fetched_at: clock(),
    };
    let error = (i128::from(response.status) >= HTTP_ERROR_MIN).then(|| {
        source_error(
            SourceErrorKind::Network,
            format!("HTTP {} while fetching {url}", response.status),
            &metadata,
        )
    });
    Fetched {
        response: Some(response),
        metadata,
        error,
    }
}

/// `protected_response_text`: the body as text, opened first if it is an AZP2 envelope for `url`.
pub fn protected_response_text(response: &FetchedResponse, url: &str) -> Result<String, PyError> {
    match protected::open_for_url(&response.content, url)? {
        None => Ok(response.text()),
        Some(content) => {
            String::from_utf8(content).map_err(|_| PyError::new(protected::CLASS, format!("Decrypted {url} is not UTF-8 text")))
        }
    }
}

/// `_with_content_type`: name a non-JSON content type, which says at a glance that the URL
/// now serves a web page.
fn with_content_type(message: String, response: &FetchedResponse) -> String {
    let header = response.header("content-type");
    let content_type = py_strip(header.split(';').next().unwrap_or_default());
    if content_type.is_empty() || content_type.contains("json") {
        return message;
    }
    format!("{message} (served as {content_type})")
}

type IndexFetch = (Option<L2DSuIndexData>, SourceFetchMetadata, Vec<SourceSnapshotError>);

/// `_fetch_l2d_su_index`. `Err` is an exception Python does not catch here.
async fn fetch_l2d_su_index(
    client: &dyn Fetch,
    region: &str,
    throttle: Option<OriginThrottle<'_>>,
    clock: Clock<'_>,
) -> Result<IndexFetch, PyError> {
    let url = l2d_su_ship_index_url(region);
    if let Some(throttle) = throttle {
        throttle().await;
    }
    let fetched = fetch_source(client, &url, clock).await;
    if let Some(error) = fetched.error {
        return Ok((None, fetched.metadata, vec![error]));
    }
    let Some(response) = fetched.response else {
        let error = source_error(
            SourceErrorKind::Network,
            "Fetch did not return a response".to_owned(),
            &fetched.metadata,
        );
        return Ok((None, fetched.metadata, vec![error]));
    };
    let parsed = protected_response_text(&response, &url).and_then(|text| parse_l2d_su_ship_index(&text, region));
    match parsed {
        Ok(parsed) => Ok((Some(parsed), fetched.metadata, Vec::new())),
        Err(error) if error.class == protected::CLASS || error.class == SOURCE_PARSE_ERROR => {
            let error = source_error(
                SourceErrorKind::Parse,
                with_content_type(error.message, &response),
                &fetched.metadata,
            );
            Ok((None, fetched.metadata, vec![error]))
        }
        Err(error) if error.class == SOURCE_SCHEMA_ERROR => {
            let error = source_error(SourceErrorKind::Schema, error.message, &fetched.metadata);
            Ok((None, fetched.metadata, vec![error]))
        }
        Err(error) => Err(error),
    }
}

/// `fetch_l2d_su_snapshot`: the primary region's index, with English names merged in from
/// `english_region` (empty to skip).
pub async fn fetch_l2d_su_snapshot(
    client: &dyn Fetch,
    region: &str,
    english_region: &str,
    throttle: Option<OriginThrottle<'_>>,
    clock: Clock<'_>,
) -> Result<L2DSuSourceSnapshot, PyError> {
    let (parsed, metadata, mut errors) = fetch_l2d_su_index(client, region, throttle, clock).await?;
    let Some(mut parsed) = parsed else {
        let mut snapshot = L2DSuSourceSnapshot::new(metadata);
        region.clone_into(&mut snapshot.region);
        snapshot.errors = errors;
        return Ok(snapshot);
    };
    if !english_region.is_empty() && english_region != region {
        let (english, _metadata, english_errors) = fetch_l2d_su_index(client, english_region, throttle, clock).await?;
        errors.extend(english_errors);
        if let Some(english) = english {
            parsed = merge_l2d_su_english_names(&parsed, &english);
        }
    }
    Ok(L2DSuSourceSnapshot {
        metadata,
        region: parsed.region,
        game_version: parsed.game_version,
        generated_at: parsed.generated_at,
        ship_count: parsed.ship_count,
        characters: parsed.characters,
        new_skins: parsed.new_skins,
        filters: parsed.filters,
        skin_update_history: parsed.skin_update_history,
        errors,
        source: "l2d.su",
    })
}

/// `fetch_nagami_snapshot`.
pub async fn fetch_nagami_snapshot(client: &dyn Fetch, url: &str, clock: Clock<'_>) -> Result<NagamiSourceSnapshot, PyError> {
    let fetched = fetch_source(client, url, clock).await;
    let mut snapshot = NagamiSourceSnapshot::new(fetched.metadata.clone());
    if let Some(error) = fetched.error {
        snapshot.errors = vec![error];
        return Ok(snapshot);
    }
    let Some(response) = fetched.response else {
        snapshot.errors = vec![source_error(
            SourceErrorKind::Network,
            "Fetch did not return a response".to_owned(),
            &fetched.metadata,
        )];
        return Ok(snapshot);
    };
    match parse_nagami_mapping(&response.text()) {
        Ok(parsed) => snapshot.entries = parsed.entries,
        Err(error) if error.class == SOURCE_PARSE_ERROR => {
            snapshot.errors = vec![source_error(SourceErrorKind::Parse, error.message, &fetched.metadata)];
        }
        Err(error) if error.class == SOURCE_SCHEMA_ERROR => {
            snapshot.errors = vec![source_error(SourceErrorKind::Schema, error.message, &fetched.metadata)];
        }
        Err(error) => return Err(error),
    }
    Ok(snapshot)
}

/// `fetch_source_snapshots`: both sources. `origin_client` serves l2d.su only, so a proxy
/// configured there never carries the CDN-hosted nagami mapping.
pub async fn fetch_source_snapshots(
    client: &dyn Fetch,
    origin_client: Option<&dyn Fetch>,
    throttle: Option<OriginThrottle<'_>>,
    clock: Clock<'_>,
) -> Result<AzurLaneSourceSnapshots, PyError> {
    let l2d_su = fetch_l2d_su_snapshot(
        origin_client.unwrap_or(client),
        L2D_SU_PRIMARY_REGION,
        L2D_SU_ENGLISH_REGION,
        throttle,
        clock,
    )
    .await?;
    let nagami = fetch_nagami_snapshot(client, NAGAMI_MAPPING_URL, clock).await?;
    Ok(AzurLaneSourceSnapshots { l2d_su, nagami })
}

/// The options of a client for the CDN and nagami (`httpx.Client(follow_redirects=True,
/// timeout=timeout, headers=_request_headers())`).
pub fn source_client_options(timeout: Duration) -> ClientOptions {
    ClientOptions {
        headers: request_headers(),
        timeout,
        follow_redirects: true,
        ..ClientOptions::default()
    }
}

/// `l2d_su_origin_transport` / `l2d_su_async_origin_transport`: the l2d.su origin client,
/// through `proxy`, and through `connect_host` while l2d.su has no DNS record.
pub fn l2d_su_origin_client_options(proxy: &str, connect_host: &str, timeout: Duration, max_keepalive_connections: usize) -> ClientOptions {
    ClientOptions {
        proxy: (!proxy.is_empty()).then(|| proxy.to_owned()),
        max_keepalive_connections,
        connect_to: (!connect_host.is_empty()).then(|| ConnectTo::new(L2D_SU_HOST, connect_host)),
        ..source_client_options(timeout)
    }
}

// ---------- the origin probe ----------

fn probe(ok: bool, code: &str, message: String, exit_ip: String) -> L2DSuOriginProbe {
    L2DSuOriginProbe {
        ok,
        code: code.to_owned(),
        message,
        exit_ip,
    }
}

/// `probe_l2d_su_origin`: check that the origin is reachable through `proxy`, saying why
/// it is not if it is not. A HEAD of the ship index, with connection reuse off, so a
/// freshly rotated exit is exercised too. `Err` is a client that cannot be built (a
/// malformed proxy URL), which Python raises.
pub async fn probe_l2d_su_origin(proxy: &str, connect_host: &str) -> Result<L2DSuOriginProbe, BuildError> {
    if py_strip(proxy).is_empty() {
        return Ok(probe(false, "incomplete", "No proxy configured.".to_owned(), String::new()));
    }
    let client = HttpClient::new(&origin_probe_client_options(proxy, connect_host))?;
    Ok(run_origin_probe(&client, &l2d_su_primary_ship_index_url()).await)
}

/// The probe's client: the crawler's own route (proxy, and `connect_host` while l2d.su has
/// no DNS record), so the settings page never reports a dead origin the crawler gets past.
pub(crate) fn origin_probe_client_options(proxy: &str, connect_host: &str) -> ClientOptions {
    l2d_su_origin_client_options(py_strip(proxy), py_strip(connect_host), Duration::from_secs(45), 0)
}

/// `_run_origin_probe`.
pub async fn run_origin_probe(client: &dyn Fetch, url: &str) -> L2DSuOriginProbe {
    let exit_ip = probe_exit_ip(client).await;
    match client.request("HEAD", url, None).await {
        Err(error) if error.class == ErrorClass::ProxyError => probe(
            false,
            "proxy_error",
            format!("Proxy refused the connection: {}", error.message),
            String::new(),
        ),
        // The origin null-routes blocked addresses, so this is what a banned exit looks like.
        Err(error) => probe(
            false,
            "unreachable",
            format!("Could not reach {url}: {}", error.message),
            String::new(),
        ),
        Ok(response) if i128::from(response.status) >= HTTP_ERROR_MIN => {
            probe(false, "http_error", format!("{url} returned HTTP {}.", response.status), exit_ip)
        }
        Ok(_) => probe(true, "ok", format!("Reached {url} through the proxy."), exit_ip),
    }
}

/// `_probe_exit_ip`: best-effort exit address, shown so an operator can tell residential
/// from datacenter.
async fn probe_exit_ip(client: &dyn Fetch) -> String {
    match client
        .request("GET", "https://checkip.amazonaws.com", Some(Duration::from_secs(20)))
        .await
    {
        Ok(response) if i128::from(response.status) < HTTP_ERROR_MIN => py_strip(&response.text()).to_owned(),
        _ => String::new(),
    }
}

// ---------- the health report ----------

/// `validate_entry_ids` of `fetch_azurlane_l2d_health_report`.
#[derive(Debug, Clone, Copy)]
pub enum HealthValidation<'a> {
    /// `None`: no resource validation.
    Skip,
    /// `'all'`.
    All,
    /// Only these entries; the rest are reported unchecked.
    Only(&'a HashSet<String>),
}

/// `fetch_azurlane_l2d_health_report`: fetch both sources, build the catalog, validate the
/// entries `validation` names, and report against `previous_report`.
pub async fn fetch_azurlane_l2d_health_report(
    client: &dyn Fetch,
    validation: HealthValidation<'_>,
    previous_report: Option<&AzurLaneL2DHealthReport>,
    clock: Clock<'_>,
) -> Result<AzurLaneL2DHealthReport, PyError> {
    let snapshots = fetch_source_snapshots(client, None, None, clock).await?;
    let mut catalog = build_azurlane_model_catalog(&snapshots)?;
    let entry_ids = match validation {
        HealthValidation::Skip => None,
        HealthValidation::All => Some(None),
        HealthValidation::Only(ids) => Some(Some(ids)),
    };
    let resource_validation = match entry_ids {
        None => None,
        Some(entry_ids) => {
            let report = validate_azurlane_model_catalog_resources(&catalog, client, entry_ids, clock()).await?;
            catalog = report.catalog.clone();
            Some(report)
        }
    };
    build_azurlane_l2d_health_report(&snapshots, Some(&catalog), resource_validation.as_ref(), previous_report, clock())
}

// ---------- resource validation ----------

/// `validate_azurlane_model_catalog_resources`: check every entry (or those in `entry_ids`),
/// falling back to its nagami copy where the primary fails. `Err` is an exception Python
/// lets escape.
pub async fn validate_azurlane_model_catalog_resources<S: std::hash::BuildHasher + Sync>(
    catalog: &AzurLaneModelCatalog,
    client: &dyn Fetch,
    entry_ids: Option<&HashSet<String, S>>,
    checked_at: String,
) -> Result<AzurLaneResourceValidationReport, PyError> {
    let mut updated_entries = Vec::with_capacity(catalog.entries.len());
    let mut validation_entries = Vec::with_capacity(catalog.entries.len());
    for entry in &catalog.entries {
        if entry_ids.is_some_and(|ids| !ids.contains(&entry.id)) {
            let mut unchecked = entry.clone();
            unchecked.availability = ModelAvailability {
                state: AvailabilityState::Unchecked,
                validated_url: String::new(),
                checked_at: checked_at.clone(),
                message: "Entry was not selected for validation".to_owned(),
            };
            validation_entries.push(resource_validation_entry(&unchecked, Vec::new()));
            updated_entries.push(unchecked);
            continue;
        }
        let (validated, validation) = validate_catalog_entry_resources(entry, &checked_at, client).await?;
        updated_entries.push(validated);
        validation_entries.push(validation);
    }
    let mut validated_catalog = catalog.clone();
    validated_catalog.entries = updated_entries;
    Ok(AzurLaneResourceValidationReport {
        checked_at,
        catalog: validated_catalog,
        entries: validation_entries,
    })
}

fn resource_validation_entry(entry: &ModelEntry, checks: Vec<ResourceCheck>) -> ResourceValidationEntry {
    ResourceValidationEntry {
        entry_id: entry.id.clone(),
        entry_type: entry.r#type,
        availability: entry.availability.clone(),
        resources: entry.resources.clone(),
        resource_summary: entry.resource_summary.clone(),
        checks,
    }
}

async fn validate_catalog_entry_resources(
    entry: &ModelEntry,
    checked_at: &str,
    client: &dyn Fetch,
) -> Result<(ModelEntry, ResourceValidationEntry), PyError> {
    let primary = validate_resource_candidate(entry, &entry.resources.primary_url, ValidationSource::Primary, client).await?;
    if primary.ok {
        let checks = primary.checks.clone();
        return Ok(validated_entry_result(
            entry,
            &primary,
            checks,
            AvailabilityState::Valid,
            checked_at,
        ));
    }
    let mut validations = vec![primary];
    let fallback_url = &entry.resources.fallback_url;
    if !fallback_url.is_empty() {
        let fallback = validate_resource_candidate(entry, fallback_url, ValidationSource::Fallback, client).await?;
        if fallback.ok {
            let mut checks = validations[0].checks.clone();
            checks.extend(fallback.checks.iter().cloned());
            return Ok(validated_entry_result(
                entry,
                &fallback,
                checks,
                AvailabilityState::FallbackOnly,
                checked_at,
            ));
        }
        validations.push(fallback);
    }
    let checks: Vec<ResourceCheck> = validations
        .iter()
        .flat_map(|validation| validation.checks.iter().cloned())
        .collect();
    let message = validations
        .iter()
        .filter(|validation| !validation.message.is_empty())
        .map(|validation| validation.message.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let mut broken = entry.clone();
    broken.availability = ModelAvailability {
        state: AvailabilityState::Broken,
        validated_url: String::new(),
        checked_at: checked_at.to_owned(),
        message: if message.is_empty() {
            "No valid resource URL found".to_owned()
        } else {
            message
        },
    };
    let validation = resource_validation_entry(&broken, checks);
    Ok((broken, validation))
}

fn validated_entry_result(
    entry: &ModelEntry,
    validation: &ResourceCandidateValidation,
    all_checks: Vec<ResourceCheck>,
    state: AvailabilityState,
    checked_at: &str,
) -> (ModelEntry, ResourceValidationEntry) {
    let mut updated = entry.clone();
    updated.resources.display_info_url.clone_from(&validation.display_info_url);
    updated.resource_summary = validation.resource_summary.clone();
    updated.availability = ModelAvailability {
        state,
        validated_url: validation.url.clone(),
        checked_at: checked_at.to_owned(),
        message: String::new(),
    };
    let validation_entry = resource_validation_entry(&updated, all_checks);
    (updated, validation_entry)
}

async fn validate_resource_candidate(
    entry: &ModelEntry,
    url: &str,
    source: ValidationSource,
    client: &dyn Fetch,
) -> Result<ResourceCandidateValidation, PyError> {
    match entry.r#type {
        ModelKind::Live2d => validate_live2d_candidate(url, source, client).await,
        ModelKind::Painting => {
            let check = probe_resource(url, AssetKind::PaintingImage, source, client).await;
            if !check.ok {
                return Ok(failed_candidate(url, check));
            }
            Ok(ResourceCandidateValidation {
                ok: true,
                url: url.to_owned(),
                resource_summary: ModelResourceSummary {
                    textures: vec![url.to_owned()],
                    ..ModelResourceSummary::default()
                },
                display_info_url: String::new(),
                checks: vec![check],
                message: String::new(),
            })
        }
        ModelKind::Spine => validate_spine_candidate(url, source, client).await,
    }
}

fn failed_candidate(url: &str, check: ResourceCheck) -> ResourceCandidateValidation {
    ResourceCandidateValidation {
        ok: false,
        url: url.to_owned(),
        resource_summary: ModelResourceSummary::default(),
        display_info_url: String::new(),
        message: check.message.clone(),
        checks: vec![check],
    }
}

/// `replace(check, ok=False, status=..., message=...)` for a file that did not parse.
fn unparsed(check: &ResourceCheck, error: &PyError) -> Option<ResourceCheck> {
    let status = match error.class.as_str() {
        SOURCE_PARSE_ERROR => ValidationStatus::Parse,
        SOURCE_SCHEMA_ERROR => ValidationStatus::Schema,
        _ => return None,
    };
    let mut failed = check.clone();
    failed.ok = false;
    failed.status = status;
    failed.message.clone_from(&error.message);
    Some(failed)
}

async fn validate_live2d_candidate(
    url: &str,
    source: ValidationSource,
    client: &dyn Fetch,
) -> Result<ResourceCandidateValidation, PyError> {
    let (response, model3_check) = request_resource("GET", url, AssetKind::Live2dModel3, source, client).await;
    let Some(response) = response.filter(|_| model3_check.ok) else {
        return Ok(failed_candidate(url, model3_check));
    };
    let manifest = match parse_live2d_model3(&response.text(), url) {
        Ok(manifest) => manifest,
        Err(error) => match unparsed(&model3_check, &error) {
            Some(failed) => return Ok(failed_candidate(url, failed)),
            None => return Err(error),
        },
    };
    let mut checks = vec![model3_check];
    checks.push(probe_resource(&manifest.moc3_url, AssetKind::Live2dMoc3, source, client).await);
    for texture_url in &manifest.texture_urls {
        checks.push(probe_resource(texture_url, AssetKind::Live2dTexture, source, client).await);
    }
    let mut display_info_url = if manifest.display_info_url.is_empty() {
        same_name_display_info_url(url)
    } else {
        manifest.display_info_url.clone()
    };
    if !display_info_url.is_empty() {
        let check = probe_resource(&display_info_url, AssetKind::Live2dDisplayInfo, source, client).await;
        if !check.ok {
            display_info_url = String::new();
        }
        checks.push(check);
    }
    let required_checks_ok = checks
        .iter()
        .filter(|check| {
            matches!(
                check.kind,
                AssetKind::Live2dModel3 | AssetKind::Live2dMoc3 | AssetKind::Live2dTexture
            )
        })
        .all(|check| check.ok);
    let resource_summary = ModelResourceSummary {
        moc3: manifest.moc3_url.clone(),
        skeleton: String::new(),
        textures: manifest.texture_urls.clone(),
        physics: manifest.physics_url.clone(),
        display_info: if display_info_url.is_empty() {
            manifest.display_info_url.clone()
        } else {
            display_info_url.clone()
        },
        motions: manifest.motion_names.clone(),
        expressions: manifest.expression_names.clone(),
        has_audio: manifest.has_audio,
        has_text: manifest.has_text,
        has_display_info: !display_info_url.is_empty(),
    };
    let message = if required_checks_ok {
        String::new()
    } else {
        resource_failure_message(&checks)
    };
    Ok(ResourceCandidateValidation {
        ok: required_checks_ok,
        url: url.to_owned(),
        resource_summary,
        display_info_url,
        checks,
        message,
    })
}

async fn validate_spine_candidate(url: &str, source: ValidationSource, client: &dyn Fetch) -> Result<ResourceCandidateValidation, PyError> {
    let (manifest, mut checks, checked_skeleton_urls) = resolve_spine_manifest_for_validation(url, source, client).await?;
    let mut texture_urls = Vec::new();
    for part in &manifest.parts {
        if !checked_skeleton_urls.contains(&part.skeleton_url) {
            checks.push(probe_resource(&part.skeleton_url, AssetKind::SpineSkel, source, client).await);
        }
        for atlas_url in &part.atlas_urls {
            let (atlas_check, part_texture_urls) = validate_spine_atlas(atlas_url, source, client).await?;
            checks.push(atlas_check);
            texture_urls.extend(part_texture_urls);
        }
    }
    let unique_texture_urls = unique_non_empty(texture_urls);
    for texture_url in &unique_texture_urls {
        checks.push(probe_resource(texture_url, AssetKind::SpineTexture, source, client).await);
    }
    let resource_summary = ModelResourceSummary {
        skeleton: manifest.parts.first().map(|part| part.skeleton_url.clone()).unwrap_or_default(),
        textures: unique_texture_urls,
        ..ModelResourceSummary::default()
    };
    let ok = checks.iter().all(|check| check.ok);
    let message = if ok { String::new() } else { resource_failure_message(&checks) };
    Ok(ResourceCandidateValidation {
        ok,
        url: url.to_owned(),
        resource_summary,
        display_info_url: String::new(),
        checks,
        message,
    })
}

async fn resolve_spine_manifest_for_validation(
    url: &str,
    source: ValidationSource,
    client: &dyn Fetch,
) -> Result<(SpineResourceManifest, Vec<ResourceCheck>, HashSet<String>), PyError> {
    let manifest = spine_resource_manifest(url, None)?;
    let single_part_url = manifest.parts[0].skeleton_url.clone();
    let single_part_check = probe_resource(&single_part_url, AssetKind::SpineSkel, source, client).await;
    if single_part_check.ok {
        return Ok((manifest, vec![single_part_check], HashSet::from([single_part_url])));
    }
    let parts_url = spine_parts_manifest_url(url)?;
    let (response, parts_check) = request_resource("GET", &parts_url, AssetKind::SpineParts, source, client).await;
    let Some(response) = response.filter(|_| parts_check.ok) else {
        return Ok((manifest, vec![single_part_check], HashSet::from([single_part_url])));
    };
    match spine_resource_manifest(url, Some(&response.text())) {
        Ok(parts_manifest) => Ok((parts_manifest, vec![parts_check], HashSet::new())),
        Err(error) => match unparsed(&parts_check, &error) {
            Some(failed) => {
                let empty = SpineResourceManifest {
                    parts: Vec::new(),
                    parts_manifest_url: manifest.parts_manifest_url,
                };
                Ok((empty, vec![single_part_check, failed], HashSet::new()))
            }
            None => Err(error),
        },
    }
}

async fn validate_spine_atlas(
    atlas_url: &str,
    source: ValidationSource,
    client: &dyn Fetch,
) -> Result<(ResourceCheck, Vec<String>), PyError> {
    let (response, atlas_check) = request_resource("GET", atlas_url, AssetKind::SpineAtlas, source, client).await;
    let Some(response) = response.filter(|_| atlas_check.ok) else {
        return Ok((atlas_check, Vec::new()));
    };
    match parse_spine_atlas_texture_urls(&response.text(), atlas_url) {
        Ok(urls) => Ok((atlas_check, urls)),
        Err(error) => match unparsed(&atlas_check, &error) {
            Some(failed) => Ok((failed, Vec::new())),
            None => Err(error),
        },
    }
}

fn resource_failure_message(checks: &[ResourceCheck]) -> String {
    checks
        .iter()
        .filter(|check| !check.ok)
        .map(|check| format!("{} {}: {}", check.kind.as_str(), check.status.as_str(), check.url))
        .collect::<Vec<_>>()
        .join("; ")
}

/// `_request_resource`.
async fn request_resource(
    method: &str,
    url: &str,
    kind: AssetKind,
    source: ValidationSource,
    client: &dyn Fetch,
) -> (Option<FetchedResponse>, ResourceCheck) {
    let response = match client.request(method, url, None).await {
        Ok(response) => response,
        Err(error) => {
            return (
                None,
                ResourceCheck {
                    kind,
                    url: url.to_owned(),
                    ok: false,
                    status: ValidationStatus::Network,
                    http_status: None,
                    message: format!("Failed to fetch {url}: {}", error.message),
                    source: Some(source),
                },
            );
        }
    };
    let status = i128::from(response.status);
    if status >= HTTP_ERROR_MIN {
        let check = ResourceCheck {
            kind,
            url: url.to_owned(),
            ok: false,
            status: ValidationStatus::Missing,
            http_status: Some(status),
            message: format!("HTTP {status} while fetching {url}"),
            source: Some(source),
        };
        return (Some(response), check);
    }
    let check = ResourceCheck {
        kind,
        url: response.url.clone(),
        ok: true,
        status: ValidationStatus::Ok,
        http_status: Some(status),
        message: String::new(),
        source: Some(source),
    };
    (Some(response), check)
}

/// `_probe_resource`: a HEAD, retried as a GET where the server does not do HEAD.
async fn probe_resource(url: &str, kind: AssetKind, source: ValidationSource, client: &dyn Fetch) -> ResourceCheck {
    let (response, check) = request_resource("HEAD", url, kind, source, client).await;
    if check.ok || !response.is_some_and(|response| matches!(response.status, 405 | 501)) {
        return check;
    }
    request_resource("GET", url, kind, source, client).await.1
}
