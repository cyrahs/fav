//! The Azur Lane character library the API reads: the crawler's manifests, one summary
//! record per manifest, cached on disk and refreshed in the background.
//!
//! Python origin: `src/api/azurlane.py` (`AzurLaneLibrary`). Values are read the way
//! BD2's library reads them ([`crate::bd2::library`]: `_clean_text`, `_to_int`,
//! `_sort_timestamp` are the same functions there). The summaries come from the version-2
//! summary cache ([`crate::summary_cache`]): a stale cache is installed and served while a
//! background thread rebuilds only the manifests whose `(mtime, size)` changed; only a
//! first load without a usable cache builds before answering. `get_asset_file` has no
//! route (the static sidecar serves `/static/azurlane`), so it is not ported (D117).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use fav_core::pycasefold::casefold;
use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::py_strip;
use fav_runtime::pystr::str_value;
use percent_encoding::percent_decode_str;

use crate::bd2::library::{self as bd2, clean_text, directory_name, freshness_le, read_json_object, sort_timestamp, static_url};
use crate::summary_cache::{
    ManifestEntry, Signature, SummaryRecord, SummaryRecordsData, manifest_signature, read_summary_records, write_summary_records,
};

const STATIC_PREFIX: &str = "/static/azurlane";

/// `AzurLaneLibrary._normalize_asset_path(asset_path)`: a relative path under `assets/`,
/// or `None` (`AzurLaneAssetNotFoundError`). Stricter than BD2's: the raw string's own
/// `/`-separated parts are checked, so an empty or `.` segment (`assets//a`, `assets/./a`,
/// a trailing `/`) is rejected rather than collapsed.
pub(crate) fn normalize_asset_path(asset_path: &str) -> Option<String> {
    let decoded = percent_decode_str(asset_path).decode_utf8_lossy().replace('\\', "/");
    let raw = py_strip(&decoded);
    if raw.is_empty() || raw.starts_with('/') {
        return None;
    }
    let mut parts = raw.split('/');
    if parts.next() != Some("assets") || raw.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return None;
    }
    Some(raw.to_owned())
}

/// `_REPRESENTATIVE_ASSET_KINDS`.
const REPRESENTATIVE_ASSET_KINDS: [&str; 3] = ["live2d.texture", "spine.texture", "painting.image"];

/// `_ASSET_KIND_ORDER`: a kind's index, unknown kinds after all of them.
const ASSET_KIND_ORDER: [&str; 21] = [
    "live2d.model3",
    "live2d.moc3",
    "live2d.texture",
    "live2d.physics",
    "live2d.pose",
    "live2d.display-info",
    "live2d.expression",
    "live2d.motion",
    "live2d.audio",
    "spine.parts",
    "spine.skel",
    "spine.atlas",
    "spine.texture",
    "painting.index",
    "painting.image",
    "painting.layer",
    "painting.mesh",
    "painting.face",
    "icon.square",
    "icon.shipyard",
    "voice.audio",
];

pub(crate) fn log() -> fav_core::logging::Logger {
    fav_core::logging::get("fav-api.azurlane")
}

/// Why the library could not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LibraryError {
    /// `AzurLaneCharacterNotFoundError`.
    CharacterNotFound,
    /// `AzurLaneAssetNotFoundError` (the ship detail or the snapshot is missing).
    AssetNotFound,
    /// Any other exception, as `ClassName: message`.
    Failed(String),
}

impl From<bd2::LibraryError> for LibraryError {
    fn from(error: bd2::LibraryError) -> Self {
        match error {
            bd2::LibraryError::NotFound => Self::CharacterNotFound,
            bd2::LibraryError::Failed(message) => Self::Failed(message),
        }
    }
}

type Result<T> = std::result::Result<T, LibraryError>;

fn kind_order(kind: &str) -> usize {
    ASSET_KIND_ORDER
        .iter()
        .position(|known| *known == kind)
        .unwrap_or(ASSET_KIND_ORDER.len())
}

fn to_int(value: Option<&Value>) -> Result<Option<i128>> {
    Ok(bd2::to_int(value)?)
}

/// `_to_int(value) or 0`.
fn int_or_zero(value: Option<&Value>) -> Result<i128> {
    Ok(to_int(value)?.unwrap_or(0))
}

fn optional_int(value: Option<i128>) -> Value {
    value.map_or(Value::Null, Value::Int)
}

/// `_clean_text(value) or None`.
fn text_or_null(value: Option<&Value>) -> Value {
    let text = clean_text(value);
    if text.is_empty() { Value::Null } else { Value::Str(text) }
}

/// `_json_object(value)`.
fn json_object(value: Option<&Value>) -> Dict {
    match value {
        Some(Value::Dict(dict)) => dict.clone(),
        _ => Dict::new(),
    }
}

/// `[item for item in _json_list(value) if isinstance(item, dict)]`.
fn dicts(value: Option<&Value>) -> Vec<&Dict> {
    match value {
        Some(Value::List(items)) => items.iter().filter_map(Value::as_dict).collect(),
        _ => Vec::new(),
    }
}

/// `_iter_models(manifest)`.
fn models_of(manifest: &Dict) -> Vec<&Dict> {
    dicts(manifest.get("models"))
}

/// `_iter_assets(model)`.
fn assets_of(model: &Dict) -> Vec<&Dict> {
    dicts(model.get("assets"))
}

/// `_status_counts(items)`: the non-empty items counted, by key.
fn status_counts(items: impl IntoIterator<Item = String>) -> BTreeMap<String, i128> {
    let mut counts = BTreeMap::new();
    for item in items.into_iter().filter(|item| !item.is_empty()) {
        *counts.entry(item).or_insert(0) += 1;
    }
    counts
}

fn counts_dict(counts: BTreeMap<String, i128>) -> Dict {
    let mut dict = Dict::new();
    for (key, count) in counts {
        dict.insert(key, Value::Int(count));
    }
    dict
}

/// `_model_counts(models)`.
fn model_counts(models: &[&Dict]) -> Dict {
    let mut counts = [("live2d", 0_i128), ("spine", 0), ("painting", 0), ("total", 0)];
    for model in models {
        let model_type = clean_text(model.get("type"));
        if let Some(entry) = counts.iter_mut().find(|(name, _)| *name == model_type) {
            entry.1 += 1;
        }
        counts[3].1 += 1;
    }
    let mut dict = Dict::new();
    for (name, count) in counts {
        dict.insert(name, Value::Int(count));
    }
    dict
}

/// `_manifest_character_key(manifest_path, manifest)`: the manifest's key, else what
/// precedes ` - ` in the directory name.
fn manifest_character_key(manifest_path: &Path, manifest: &Dict) -> String {
    let key = clean_text(manifest.get("character_key"));
    if !key.is_empty() {
        return key;
    }
    let name = directory_name(manifest_path);
    py_strip(name.split(" - ").next().unwrap_or_default()).to_owned()
}

/// `_record_freshness_key(manifest, mtime_ns=...)`.
fn freshness(manifest: &Dict, mtime_ns: i128) -> Result<(f64, f64, i128)> {
    Ok((
        sort_timestamp(manifest.get("fetched_at"))?,
        sort_timestamp(manifest.get("completed_at"))?,
        mtime_ns,
    ))
}

/// `_asset_sort_key(asset)`.
fn asset_sort_key(asset: &Dict) -> (usize, String, String) {
    (
        kind_order(&clean_text(asset.get("kind"))),
        clean_text(asset.get("local_path")),
        clean_text(asset.get("url")),
    )
}

/// `_model_sort_key(model)`.
fn model_sort_key(model: &Dict) -> (String, String, String) {
    let costume = json_object(model.get("costume"));
    (
        clean_text(model.get("type")),
        clean_text(costume.get("key")),
        clean_text(model.get("model_id")),
    )
}

/// `_skin_update_entry(item)`: one skin of the update feed, `snake_case` whichever spelling
/// the snapshot used.
fn skin_update_entry(item: &Dict) -> Result<Value> {
    let either = |snake: &str, camel: &str| {
        if item.contains_key(snake) {
            item.get(snake)
        } else {
            item.get(camel)
        }
    };
    let mut skin_ids = Vec::new();
    if let Some(Value::List(values)) = either("skin_ids", "skinIds") {
        for value in values {
            if let Some(id) = to_int(Some(value))? {
                skin_ids.push(Value::Int(id));
            }
        }
    }
    let mut entry = Dict::new();
    entry.insert("ship_group_id", Value::Int(int_or_zero(either("ship_group_id", "shipGroupId"))?));
    entry.insert("ship_name", Value::Str(clean_text(either("ship_name", "shipName"))));
    entry.insert("skin_name", Value::Str(clean_text(either("skin_name", "skinName"))));
    entry.insert("skin_type", Value::Str(clean_text(either("skin_type", "skinType"))));
    entry.insert("skin_ids", Value::List(skin_ids));
    Ok(Value::Dict(entry))
}

// ---------- records ----------

/// `_AzurLaneRecordEntry`.
#[derive(Debug, Clone)]
struct RecordEntry {
    character_key: String,
    root: PathBuf,
    manifest_path: PathBuf,
    directory_name: String,
}

impl RecordEntry {
    fn new(character_key: &str, manifest_path: &Path) -> Self {
        Self {
            character_key: character_key.to_owned(),
            root: manifest_path.parent().map(Path::to_path_buf).unwrap_or_default(),
            manifest_path: manifest_path.to_path_buf(),
            directory_name: directory_name(manifest_path),
        }
    }
}

/// `_AzurLaneRecord`: an entry with its manifest read.
#[derive(Debug, Clone)]
struct Record {
    character_key: String,
    root: PathBuf,
    manifest_path: PathBuf,
    directory_name: String,
    manifest: Dict,
}

/// What the library has installed (`_signature`, `_records`, `_record_entries`,
/// `_summaries`).
#[derive(Debug, Default)]
struct CacheState {
    signature: Option<Signature>,
    records: Vec<SummaryRecord>,
    record_entries: HashMap<String, RecordEntry>,
    summaries: Vec<Dict>,
    disk_cache_loaded: bool,
}

impl CacheState {
    /// `_install_records(signature, records)`.
    fn install(&mut self, signature: Signature, records: Vec<SummaryRecord>) {
        self.record_entries = records
            .iter()
            .map(|record| (record.key.clone(), RecordEntry::new(&record.key, Path::new(&record.manifest_path))))
            .collect();
        let mut summaries: Vec<Dict> = records.iter().map(|record| record.summary.clone()).collect();
        summaries.sort_by_cached_key(|summary| {
            let text = |key: &str| {
                summary
                    .get(key)
                    .filter(|value| value.is_truthy())
                    .map(str_value)
                    .unwrap_or_default()
            };
            (casefold(&text("display_name")), text("character_key"))
        });
        self.signature = Some(signature);
        self.records = records;
        self.summaries = summaries;
    }
}

/// Hooks the tests use where Python's monkeypatch them in (`_build_records`,
/// `_read_json_object`).
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHooks {
    pub before_build: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    pub manifest_reads: Mutex<Vec<PathBuf>>,
}

/// `AzurLaneLibrary`.
pub struct AzurLaneLibrary {
    root: PathBuf,
    cache: Mutex<CacheState>,
    /// `_refresh_thread`.
    refresh: Mutex<Option<JoinHandle<()>>>,
    #[cfg(test)]
    pub(crate) hooks: TestHooks,
}

impl std::fmt::Debug for AzurLaneLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AzurLaneLibrary").field("root", &self.root).finish_non_exhaustive()
    }
}

impl AzurLaneLibrary {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cache: Mutex::new(CacheState::default()),
            refresh: Mutex::new(None),
            #[cfg(test)]
            hooks: TestHooks::default(),
        }
    }

    /// The collection root it reads.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `list_characters()`: every summary, by display name.
    pub(crate) fn list_characters(self: &Arc<Self>) -> Result<Vec<Dict>> {
        self.ensure_summary_cache()?;
        Ok(self.lock().summaries.clone())
    }

    /// `get_character(character_key)`: the summary with the models and their assets.
    pub(crate) fn get_character(self: &Arc<Self>, character_key: &str) -> Result<Dict> {
        let record = self.get_record(character_key)?;
        let mut models_in_order = models_of(&record.manifest);
        models_in_order.sort_by_cached_key(|model| model_sort_key(model));
        let mut models = Vec::with_capacity(models_in_order.len());
        for model in models_in_order {
            models.push(self.model_payload(&record, model)?);
        }
        let of_type = |wanted: &str| -> Value {
            Value::List(
                models
                    .iter()
                    .filter(|model| matches!(model.get("type"), Some(Value::Str(kind)) if kind == wanted))
                    .cloned()
                    .map(Value::Dict)
                    .collect(),
            )
        };
        let live2d = of_type("live2d");
        let spine = of_type("spine");
        let painting = of_type("painting");
        let assets: Vec<Value> = models
            .iter()
            .filter_map(|model| match model.get("assets") {
                Some(Value::List(assets)) => Some(assets.iter().cloned()),
                _ => None,
            })
            .flatten()
            .collect();
        let mut payload = self.summary_payload(&record)?;
        payload.insert("active", Value::Bool(record.manifest.get("active").is_none_or(Value::is_truthy)));
        payload.insert("models", Value::List(models.into_iter().map(Value::Dict).collect()));
        payload.insert("live2d_models", live2d);
        payload.insert("spine_models", spine);
        payload.insert("painting_models", painting);
        payload.insert("assets", Value::List(assets));
        Ok(payload)
    }

    /// `get_ship_detail(character_key)`: the character's `detail.json`.
    pub(crate) fn get_ship_detail(self: &Arc<Self>, character_key: &str) -> Result<Dict> {
        let record = self.get_record(character_key)?;
        let detail_path = record.manifest_path.with_file_name("detail.json");
        if !detail_path.is_file() {
            return Err(LibraryError::AssetNotFound);
        }
        read_json_object(&detail_path).map_err(|_| LibraryError::AssetNotFound)
    }

    /// `get_skin_updates()`: the source's own "what changed" feed, lifted out of the
    /// archived l2d.su snapshot.
    pub(crate) fn get_skin_updates(&self) -> Result<Dict> {
        let snapshot_path = self.root.join("_source").join("l2d-su-snapshot.json");
        if !snapshot_path.is_file() {
            return Err(LibraryError::AssetNotFound);
        }
        let snapshot = read_json_object(&snapshot_path).map_err(|_| LibraryError::AssetNotFound)?;
        let mut new_skins = Vec::new();
        for item in dicts(snapshot.get("new_skins")) {
            new_skins.push(skin_update_entry(item)?);
        }
        let mut history = Vec::new();
        for entry in dicts(snapshot.get("skin_update_history")) {
            let mut skins = Vec::new();
            for item in dicts(entry.get("skins")) {
                skins.push(skin_update_entry(item)?);
            }
            let mut item = Dict::new();
            item.insert("date", Value::Str(clean_text(entry.get("date"))));
            item.insert("version", Value::Str(clean_text(entry.get("version"))));
            item.insert("skins", Value::List(skins));
            history.push(Value::Dict(item));
        }
        let mut payload = Dict::new();
        payload.insert("game_version", Value::Str(clean_text(snapshot.get("game_version"))));
        payload.insert("region", Value::Str(clean_text(snapshot.get("region"))));
        payload.insert("generated_at", Value::Str(clean_text(snapshot.get("generated_at"))));
        payload.insert("new_skins", Value::List(new_skins));
        payload.insert("skin_update_history", Value::List(history));
        Ok(payload)
    }

    /// `_manifest_entries()`: `*/manifest.json` under the root, skipping `_*`
    /// directories and anything resolving outside the root, by case-folded directory name.
    fn manifest_entries(&self) -> Vec<ManifestEntry> {
        if !self.root.is_dir() {
            return Vec::new();
        }
        let root_resolved = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let children = match std::fs::read_dir(&self.root).and_then(Iterator::collect::<std::io::Result<Vec<_>>>) {
            Ok(children) => children,
            Err(error) => {
                log().error(format!(
                    "Failed to list Azur Lane collection root: {}\n{}",
                    self.root.display(),
                    bd2::os_error_text(&error, &self.root)
                ));
                return Vec::new();
            }
        };
        let mut entries = Vec::new();
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            let child = self.root.join(&name);
            if name.starts_with('_') || !child.is_dir() {
                continue;
            }
            let child_resolved = std::fs::canonicalize(&child).unwrap_or_else(|_| child.clone());
            if !child_resolved.starts_with(&root_resolved) {
                log().warning(format!(
                    "Skipping Azur Lane character directory outside collection root: {}",
                    child.display()
                ));
                continue;
            }
            let manifest_path = child.join("manifest.json");
            if !manifest_path.is_file() {
                continue;
            }
            let (Ok(manifest_resolved), Ok(stat)) = (std::fs::canonicalize(&manifest_path), std::fs::metadata(&manifest_path)) else {
                log().warning(format!("Failed to stat Azur Lane manifest: {}", manifest_path.display()));
                continue;
            };
            if !manifest_resolved.starts_with(&child_resolved) || !manifest_resolved.starts_with(&root_resolved) {
                log().warning(format!(
                    "Skipping Azur Lane manifest outside collection root: {}",
                    manifest_path.display()
                ));
                continue;
            }
            entries.push(ManifestEntry::from_metadata(manifest_path, &stat));
        }
        entries.sort_by_cached_key(|entry| casefold(&directory_name(&entry.path)));
        entries
    }

    /// `_ensure_summary_cache()`: install the disk cache on first use, then serve what is
    /// installed while a background refresh catches up with changed manifests; build
    /// before answering only when nothing is installed yet.
    fn ensure_summary_cache(self: &Arc<Self>) -> Result<()> {
        let entries = self.manifest_entries();
        let signature = manifest_signature(&entries);
        let mut cache = self.lock();
        if cache.signature.as_ref() == Some(&signature) {
            return Ok(());
        }
        if !cache.disk_cache_loaded {
            cache.disk_cache_loaded = true;
            if let Some(cached) = read_summary_records(&self.summary_cache_path(), &log(), "Azur Lane") {
                let matches = cached.signature == signature;
                cache.install(cached.signature, cached.records);
                if matches {
                    return Ok(());
                }
            }
        }
        if cache.signature.is_some() {
            // Serve the (possibly stale) installed summaries and refresh in the background:
            // a full rebuild parses every manifest and can take minutes on a large
            // collection, which must not block requests.
            drop(cache);
            self.spawn_refresh();
            return Ok(());
        }
        let records = self.build_records(&entries, None)?;
        cache.install(signature, records);
        self.write_summary_cache(&cache);
        Ok(())
    }

    /// `_spawn_refresh_locked()`: one refresh at a time.
    fn spawn_refresh(self: &Arc<Self>) {
        let mut refresh = self.refresh.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if refresh.as_ref().is_some_and(|thread| !thread.is_finished()) {
            return;
        }
        let library = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("azurlane-summary-refresh".to_owned())
            .spawn(move || library.refresh_summary_cache());
        match spawned {
            Ok(thread) => *refresh = Some(thread),
            Err(error) => log().error(format!("Azur Lane summary cache refresh failed\n{error}")),
        }
    }

    /// `_refresh_summary_cache()`: rebuild outside the lock, reusing the records of
    /// unchanged manifests, then install.
    fn refresh_summary_cache(&self) {
        let entries = self.manifest_entries();
        let signature = manifest_signature(&entries);
        let previous = {
            let cache = self.lock();
            if cache.signature.as_ref() == Some(&signature) {
                return;
            }
            SummaryRecordsData {
                signature: cache.signature.clone().unwrap_or_default(),
                records: cache.records.clone(),
            }
        };
        match self.build_records(&entries, Some(&previous)) {
            Ok(records) => {
                let mut cache = self.lock();
                cache.install(signature, records);
                self.write_summary_cache(&cache);
            }
            Err(LibraryError::Failed(error)) => log().error(format!("Azur Lane summary cache refresh failed\n{error}")),
            Err(other) => log().error(format!("Azur Lane summary cache refresh failed\n{other:?}")),
        }
    }

    /// Wait for the background refresh, if one runs (the tests' `_join_refresh_thread`).
    #[cfg(test)]
    pub(crate) fn join_refresh(&self) -> bool {
        let thread = self.refresh.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        thread.is_some_and(|thread| thread.join().is_ok())
    }

    /// `_build_records(entries, previous)`: one record per character key, the freshest
    /// manifest winning; a manifest whose `(mtime, size)` is unchanged keeps its record.
    fn build_records(&self, entries: &[ManifestEntry], previous: Option<&SummaryRecordsData>) -> Result<Vec<SummaryRecord>> {
        #[cfg(test)]
        if let Some(hook) = self
            .hooks
            .before_build
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            hook();
        }
        let mut previous_records: HashMap<&str, &SummaryRecord> = HashMap::new();
        let mut previous_stats: HashMap<&str, (i128, i128)> = HashMap::new();
        if let Some(previous) = previous {
            previous_records = previous
                .records
                .iter()
                .map(|record| (record.manifest_path.as_str(), record))
                .collect();
            previous_stats = previous
                .signature
                .iter()
                .map(|(path, mtime_ns, size)| (path.as_str(), (*mtime_ns, *size)))
                .collect();
        }
        let mut order: Vec<String> = Vec::new();
        let mut winners: HashMap<String, SummaryRecord> = HashMap::new();
        for entry in entries {
            let path = entry.path.to_string_lossy().into_owned();
            let record = match previous_records.get(path.as_str()) {
                Some(reusable) if previous_stats.get(path.as_str()) == Some(&(entry.mtime_ns, entry.size)) => (*reusable).clone(),
                _ => match self.read_summary_record(&entry.path, entry.mtime_ns)? {
                    Some(record) => record,
                    None => continue,
                },
            };
            if let Some(existing) = winners.get(&record.key) {
                if freshness_le(record.freshness, existing.freshness) {
                    log().warning(format!(
                        "Skipping stale duplicate Azur Lane manifest for character_key={}: {}",
                        record.key,
                        entry.path.display()
                    ));
                    continue;
                }
                log().warning(format!(
                    "Replacing stale duplicate Azur Lane manifest for character_key={}: {} -> {}",
                    record.key,
                    existing.manifest_path,
                    entry.path.display()
                ));
            } else {
                order.push(record.key.clone());
            }
            winners.insert(record.key.clone(), record);
        }
        Ok(order.into_iter().filter_map(|key| winners.remove(&key)).collect())
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))] // The tests record the reads through `self`.
    fn read_manifest(&self, path: &Path) -> std::result::Result<Dict, String> {
        #[cfg(test)]
        self.hooks
            .manifest_reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(path.to_path_buf());
        read_json_object(path)
    }

    /// `_read_summary_record(manifest_path, mtime_ns=...)`.
    fn read_summary_record(&self, manifest_path: &Path, mtime_ns: i128) -> Result<Option<SummaryRecord>> {
        let manifest = match self.read_manifest(manifest_path) {
            Ok(manifest) => manifest,
            Err(error) => {
                log().warning(format!(
                    "Skipping unreadable Azur Lane manifest {}: {error}",
                    manifest_path.display()
                ));
                return Ok(None);
            }
        };
        let character_key = manifest_character_key(manifest_path, &manifest);
        if character_key.is_empty() {
            log().warning(format!(
                "Skipping Azur Lane manifest without character key: {}",
                manifest_path.display()
            ));
            return Ok(None);
        }
        let entry = RecordEntry::new(&character_key, manifest_path);
        let record = Record {
            character_key: entry.character_key,
            root: entry.root,
            manifest_path: entry.manifest_path,
            directory_name: entry.directory_name,
            manifest,
        };
        Ok(Some(SummaryRecord {
            key: character_key,
            manifest_path: manifest_path.to_string_lossy().into_owned(),
            freshness: freshness(&record.manifest, mtime_ns)?,
            summary: self.summary_payload(&record)?,
        }))
    }

    fn summary_cache_path(&self) -> PathBuf {
        self.root.join("_api").join("summary-cache.json")
    }

    /// `_write_summary_cache_locked()`: the records by key.
    fn write_summary_cache(&self, cache: &CacheState) {
        if !self.root.is_dir() {
            return;
        }
        let mut records = cache.records.clone();
        records.sort_by(|left, right| left.key.cmp(&right.key));
        write_summary_records(
            &self.summary_cache_path(),
            &SummaryRecordsData {
                signature: cache.signature.clone().unwrap_or_default(),
                records,
            },
            &log(),
            "Azur Lane",
        );
    }

    /// `_get_record(character_key)`: the entry with its manifest read again.
    fn get_record(self: &Arc<Self>, character_key: &str) -> Result<Record> {
        self.ensure_summary_cache()?;
        let key = clean_text(Some(&Value::Str(character_key.to_owned())));
        let entry = self
            .lock()
            .record_entries
            .get(&key)
            .cloned()
            .ok_or(LibraryError::CharacterNotFound)?;
        let manifest = self.read_manifest(&entry.manifest_path).map_err(|error| {
            log().warning(format!(
                "Skipping unreadable Azur Lane manifest {}: {error}",
                entry.manifest_path.display()
            ));
            LibraryError::CharacterNotFound
        })?;
        Ok(Record {
            character_key: entry.character_key,
            root: entry.root,
            manifest_path: entry.manifest_path,
            directory_name: entry.directory_name,
            manifest,
        })
    }

    /// `_summary_payload(record)`.
    fn summary_payload(&self, record: &Record) -> Result<Dict> {
        let manifest = &record.manifest;
        let models = models_of(manifest);
        let source_counts = status_counts(models.iter().map(|model| clean_text(model.get("source"))));
        let model_types: BTreeSet<String> = models
            .iter()
            .map(|model| clean_text(model.get("type")))
            .filter(|kind| !kind.is_empty())
            .collect();
        let source_names: Vec<String> = source_counts.keys().cloned().collect();
        let mut counts = json_object(manifest.get("model_counts"));
        if counts.is_empty() {
            counts = model_counts(&models);
        }
        let mut asset_counts = json_object(manifest.get("asset_counts"));
        if asset_counts.is_empty() {
            asset_counts = counts_dict(status_counts(
                models
                    .iter()
                    .flat_map(|model| assets_of(model))
                    .map(|asset| clean_text(asset.get("kind"))),
            ));
        }
        let model_count = match to_int(counts.get("total"))? {
            Some(total) if total != 0 => total,
            _ => i128::try_from(models.len()).unwrap_or(i128::MAX),
        };
        let display_name = safe_display_name(record);
        let source = clean_text(manifest.get("source"));
        let mut tags = Dict::new();
        tags.insert("model_types", Value::Str(model_types.into_iter().collect::<Vec<_>>().join(", ")));
        tags.insert("sources", Value::Str(source_names.join(", ")));
        let mut payload = Dict::new();
        payload.insert("schema_version", Value::Int(int_or_zero(manifest.get("schema_version"))?));
        payload.insert("source", Value::Str(if source.is_empty() { "azurlane".to_owned() } else { source }));
        payload.insert("character_key", Value::Str(record.character_key.clone()));
        payload.insert("source_id", optional_int(to_int(manifest.get("source_id"))?));
        payload.insert("title", Value::Str(display_name.clone()));
        payload.insert("display_name", Value::Str(display_name));
        payload.insert("name_zh", Value::Str(clean_text(manifest.get("name_zh"))));
        payload.insert("name_en", Value::Str(clean_text(manifest.get("name_en"))));
        payload.insert("directory_name", Value::Str(record.directory_name.clone()));
        payload.insert("source_metadata", Value::Dict(json_object(manifest.get("source_metadata"))));
        payload.insert("fetched_at", text_or_null(manifest.get("fetched_at")));
        payload.insert("completed_at", text_or_null(manifest.get("completed_at")));
        payload.insert("model_counts", Value::Dict(counts));
        payload.insert("asset_counts", Value::Dict(asset_counts));
        payload.insert("source_counts", Value::Dict(counts_dict(source_counts)));
        payload.insert("tags", Value::Dict(tags));
        payload.insert("representative_asset", self.pick_representative_asset(record)?);
        payload.insert("icon", self.pick_icon_asset(record, &models)?);
        payload.insert("model_count", Value::Int(model_count));
        Ok(payload)
    }

    /// `_pick_icon_asset(record, models)`: the default skin's square icon, else any
    /// downloaded one, else the first.
    fn pick_icon_asset(&self, record: &Record, models: &[&Dict]) -> Result<Value> {
        let default_skin_id = to_int(json_object(record.manifest.get("source_metadata")).get("default_skin_id"))?;
        let mut refs: Vec<(bool, Dict)> = Vec::new();
        for model in models {
            if clean_text(model.get("type")) != "painting" {
                continue;
            }
            let costume_id = to_int(json_object(model.get("costume")).get("id"))?;
            let is_default = costume_id.is_some() && costume_id == default_skin_id;
            for asset in assets_of(model) {
                if clean_text(asset.get("kind")) == "icon.square" {
                    refs.push((is_default, self.asset_ref(record, asset)?));
                }
            }
        }
        if refs.is_empty() {
            return Ok(Value::Null);
        }
        for wanted_default in [true, false] {
            if let Some((_, found)) = refs
                .iter()
                .find(|(is_default, item)| *is_default == wanted_default && item.get("available") == Some(&Value::Bool(true)))
            {
                return Ok(Value::Dict(found.clone()));
            }
        }
        Ok(Value::Dict(refs.swap_remove(0).1))
    }

    /// `_model_payload(record, model)`.
    fn model_payload(&self, record: &Record, model: &Dict) -> Result<Dict> {
        let mut sorted = assets_of(model);
        sorted.sort_by_cached_key(|asset| asset_sort_key(asset));
        let mut assets = Vec::with_capacity(sorted.len());
        for asset in sorted {
            assets.push(self.asset_ref(record, asset)?);
        }
        let model_type = clean_text(model.get("type"));
        let character_key = clean_text(model.get("character_key"));
        let costume = json_object(model.get("costume"));
        let mut costume_payload = Dict::new();
        costume_payload.insert("key", Value::Str(clean_text(costume.get("key"))));
        costume_payload.insert("id", optional_int(to_int(costume.get("id"))?));
        costume_payload.insert("name_zh", Value::Str(clean_text(costume.get("name_zh"))));
        costume_payload.insert("name_en", Value::Str(clean_text(costume.get("name_en"))));
        let mut asset_counts = json_object(model.get("asset_counts"));
        if asset_counts.is_empty() {
            asset_counts = counts_dict(status_counts(assets.iter().map(|asset| asset_kind(asset).to_owned())));
        }
        let mut payload = Dict::new();
        payload.insert("model_id", Value::Str(clean_text(model.get("model_id"))));
        payload.insert("type", Value::Str(model_type.clone()));
        payload.insert("source", Value::Str(clean_text(model.get("source"))));
        payload.insert(
            "character_key",
            Value::Str(if character_key.is_empty() {
                record.character_key.clone()
            } else {
                character_key
            }),
        );
        payload.insert("costume", Value::Dict(costume_payload));
        payload.insert("source_urls", Value::Dict(json_object(model.get("source_urls"))));
        payload.insert("availability", Value::Dict(json_object(model.get("availability"))));
        payload.insert("source_metadata", Value::Dict(json_object(model.get("source_metadata"))));
        payload.insert("fetched_at", text_or_null(model.get("fetched_at")));
        payload.insert("completed_at", text_or_null(model.get("completed_at")));
        payload.insert("asset_counts", Value::Dict(asset_counts));
        payload.insert("files", Value::Dict(model_files(&model_type, &assets)));
        payload.insert("assets", Value::List(assets.into_iter().map(Value::Dict).collect()));
        Ok(payload)
    }

    /// `_pick_representative_asset(record)`: the first available texture or painting by
    /// kind and path, else the first available asset, else the first asset.
    fn pick_representative_asset(&self, record: &Record) -> Result<Value> {
        let mut candidates = Vec::new();
        for model in models_of(&record.manifest) {
            for asset in assets_of(model) {
                candidates.push(self.asset_ref(record, asset)?);
            }
        }
        let first_by_kind = |items: Vec<&Dict>| {
            items
                .into_iter()
                .min_by(|left, right| {
                    let key = |item: &Dict| (kind_order(asset_kind(item)), text_field(item, "path").to_owned());
                    key(left).cmp(&key(right))
                })
                .cloned()
        };
        let available: Vec<&Dict> = candidates
            .iter()
            .filter(|asset| asset.get("available") == Some(&Value::Bool(true)))
            .collect();
        let representative: Vec<&Dict> = available
            .iter()
            .copied()
            .filter(|asset| REPRESENTATIVE_ASSET_KINDS.contains(&asset_kind(asset)))
            .collect();
        let picked = first_by_kind(representative)
            .or_else(|| first_by_kind(available))
            .or_else(|| candidates.first().cloned());
        Ok(picked.map_or(Value::Null, Value::Dict))
    }

    /// `_asset_ref(record, asset)`: the asset as the API serves it.
    fn asset_ref(&self, record: &Record, asset: &Dict) -> Result<Dict> {
        let local_path = clean_text(asset.get("local_path"));
        let mut normalized_path = String::new();
        let mut available = false;
        if !local_path.is_empty() {
            normalized_path = normalize_asset_path(&local_path).unwrap_or_default();
            if !normalized_path.is_empty() {
                available = self
                    .resolved_asset_path(record, &normalized_path)
                    .is_some_and(|target| target.is_file());
            }
        }
        let sha256 = clean_text(asset.get("sha256"));
        let contexts: Vec<Value> = dicts(asset.get("contexts")).into_iter().cloned().map(Value::Dict).collect();
        let primary = contexts.first().and_then(Value::as_dict).cloned().unwrap_or_default();
        let context = |key: &str| clean_text(primary.get(key));
        let mut source_urls = json_object(asset.get("source_urls"));
        if source_urls.is_empty() {
            source_urls.insert("primary", Value::Str(clean_text(asset.get("url"))));
            source_urls.insert("fallback", Value::Str(clean_text(asset.get("fallback_url"))));
            source_urls.insert("downloaded", Value::Str(clean_text(asset.get("downloaded_url"))));
        }
        let url = if normalized_path.is_empty() {
            String::new()
        } else {
            static_url(STATIC_PREFIX, &record.directory_name, &normalized_path, &sha256)
        };
        let field = [context("live2d_field"), context("spine_field"), context("painting_field")]
            .into_iter()
            .find(|value| !value.is_empty())
            .unwrap_or_default();
        let context_hashes: Vec<Value> = match asset.get("context_hashes") {
            Some(Value::List(items)) => items
                .iter()
                .map(|item| clean_text(Some(item)))
                .filter(|item| !item.is_empty())
                .map(Value::Str)
                .collect(),
            _ => Vec::new(),
        };
        let mut payload = Dict::new();
        payload.insert("kind", Value::Str(clean_text(asset.get("kind"))));
        payload.insert(
            "path",
            Value::Str(if normalized_path.is_empty() { local_path } else { normalized_path }),
        );
        payload.insert("url", Value::Str(url));
        payload.insert("source_url", Value::Str(clean_text(asset.get("url"))));
        payload.insert("normalized_url", Value::Str(clean_text(asset.get("normalized_url"))));
        payload.insert("downloaded_url", Value::Str(clean_text(asset.get("downloaded_url"))));
        payload.insert("fallback_url", Value::Str(clean_text(asset.get("fallback_url"))));
        payload.insert("source_urls", Value::Dict(source_urls));
        payload.insert("original_filename", Value::Str(clean_text(asset.get("original_filename"))));
        payload.insert("content_type", Value::Str(clean_text(asset.get("content_type"))));
        payload.insert("size", Value::Int(int_or_zero(asset.get("size"))?));
        payload.insert("sha256", Value::Str(sha256));
        payload.insert("status", Value::Str(clean_text(asset.get("status"))));
        payload.insert("available", Value::Bool(available));
        payload.insert("failed_count", Value::Int(int_or_zero(asset.get("failed_count"))?));
        payload.insert("error", Value::Str(clean_text(asset.get("error"))));
        payload.insert("last_attempt_at", text_or_null(asset.get("last_attempt_at")));
        payload.insert("next_retry_at", text_or_null(asset.get("next_retry_at")));
        payload.insert("last_seen_at", text_or_null(asset.get("last_seen_at")));
        payload.insert("model_id", Value::Str(context("model_id")));
        payload.insert("model_type", Value::Str(context("model_type")));
        payload.insert("character_key", Value::Str(context("character_key")));
        payload.insert("costume_key", Value::Str(context("costume_key")));
        payload.insert("field", Value::Str(field));
        payload.insert("catalog_source", Value::Str(context("catalog_source")));
        payload.insert("context_hashes", Value::List(context_hashes));
        payload.insert("contexts", Value::List(contexts));
        Ok(payload)
    }

    /// `_resolved_asset_path(record, normalized_path)`: the target with every symlink
    /// resolved, when it stays under the record's `assets/` and the collection root. A
    /// path that does not exist resolves to nothing, as no such path is a file.
    fn resolved_asset_path(&self, record: &Record, normalized_path: &str) -> Option<PathBuf> {
        let collection_root = std::fs::canonicalize(&self.root).ok()?;
        let record_root = std::fs::canonicalize(&record.root).ok()?;
        let assets_root = std::fs::canonicalize(record.root.join("assets")).ok()?;
        let target = std::fs::canonicalize(record.root.join(normalized_path)).ok()?;
        let inside = record_root.starts_with(&collection_root)
            && assets_root.starts_with(&record_root)
            && assets_root.starts_with(&collection_root)
            && target.starts_with(&assets_root)
            && target.starts_with(&collection_root);
        inside.then_some(target)
    }
}

/// `_safe_display_name(record)`: the English name, the Chinese name, the directory
/// name's suffix, or the key.
fn safe_display_name(record: &Record) -> String {
    for key in ["name_en", "name_zh"] {
        let name = clean_text(record.manifest.get(key));
        if !name.is_empty() {
            return name;
        }
    }
    let suffix = py_strip(
        record
            .directory_name
            .split_once(" - ")
            .map_or(record.directory_name.as_str(), |(_, rest)| rest),
    );
    if suffix.is_empty() {
        record.character_key.clone()
    } else {
        suffix.to_owned()
    }
}

fn text_field<'a>(item: &'a Dict, key: &str) -> &'a str {
    match item.get(key) {
        Some(Value::Str(text)) => text,
        _ => "",
    }
}

fn asset_kind(asset: &Dict) -> &str {
    text_field(asset, "kind")
}

/// `_model_files(model_type, assets)`: the assets a viewer needs, by role.
fn model_files(model_type: &str, assets: &[Dict]) -> Dict {
    let first = |kind: &str| -> Value {
        assets
            .iter()
            .find(|asset| asset_kind(asset) == kind)
            .cloned()
            .map_or(Value::Null, Value::Dict)
    };
    let all = |kind: &str| -> Value {
        Value::List(
            assets
                .iter()
                .filter(|asset| asset_kind(asset) == kind)
                .cloned()
                .map(Value::Dict)
                .collect(),
        )
    };
    let entries: Vec<(&str, Value)> = match model_type {
        "live2d" => vec![
            ("model3", first("live2d.model3")),
            ("moc3", first("live2d.moc3")),
            ("textures", all("live2d.texture")),
            ("physics", first("live2d.physics")),
            ("pose", first("live2d.pose")),
            ("display_info", first("live2d.display-info")),
            ("expressions", all("live2d.expression")),
            ("motions", all("live2d.motion")),
            ("audio", all("live2d.audio")),
        ],
        "spine" => vec![
            ("parts", first("spine.parts")),
            ("skel", first("spine.skel")),
            ("skeletons", all("spine.skel")),
            ("atlas", first("spine.atlas")),
            ("atlases", all("spine.atlas")),
            ("textures", all("spine.texture")),
        ],
        // The sheets are tight-packed, so reassembling one needs its mesh. Pairing is by
        // basename: layer `x_rw.webp` is unpacked by mesh `x_rw-mesh.obj`.
        "painting" => vec![
            ("index", first("painting.index")),
            ("image", first("painting.image")),
            ("layers", all("painting.layer")),
            ("meshes", all("painting.mesh")),
            ("faces", all("painting.face")),
            ("square_icon", first("icon.square")),
            ("shipyard_icon", first("icon.shipyard")),
            ("voices", all("voice.audio")),
        ],
        _ => Vec::new(),
    };
    let mut files = Dict::new();
    for (name, value) in entries {
        files.insert(name, value);
    }
    files
}
