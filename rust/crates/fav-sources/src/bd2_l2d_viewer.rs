//! The BD2-L2D-Viewer project's character list and asset tree, and its comparison with
//! GameKee's Live2D resources.
//!
//! Python origin: `src/tool/bd2_l2d_viewer.py`. The BD2 crawler ([`crate::bd2`]) reads the
//! viewer's `character_list.ts` and repository tree ([`fetch_viewer_resources`]) to prefer
//! or supplement Live2D files; the pure helpers below (stems, categories, asset URLs, the
//! manifest and detail readers, [`compare_resources`]) are the module's own and are ported
//! with it. The script-only fetchers (`fetch_gamekee_resources`,
//! `fetch_gamekee_character_rows`, `gamekee_headers`, `_select_gamekee_rows`,
//! `load_gamekee_resources_from_manifests`) are not (DIVERGENCES.md D94).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use fav_core::pycasefold::casefold;
use fav_core::pyjson::{Dict, Value};
use fav_core::pypath;
use fav_core::pytext::{is_py_space, py_int_decimal, py_isdigit, py_strip};
use fav_core::pyurl::{UrlError, quote, unquote, urljoin, urlsplit};
use fav_net::{HttpClient, Request};
use fav_runtime::pystr::repr_str;

use crate::pydecode::{PyError, json_from_bytes, json_loads};
use crate::pyre;

pub const VIEWER_PAGE_URL: &str = "https://jelosus2.github.io/BD2-L2D-Viewer/";
pub const VIEWER_CHARACTER_LIST_URL: &str = "https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/utils/character_list.ts";
pub const VIEWER_REPO_TREE_URL: &str = "https://api.github.com/repos/Jelosus2/BD2-L2D-Viewer/git/trees/main?recursive=1";
pub const VIEWER_SPINES_TREE_PREFIX: &str = "src/assets/spines/";
pub const VIEWER_RAW_SPINES_BASE_URL: &str = "https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/assets/spines/";
pub const GAMEKEE_BASE_URL: &str = "https://www.gamekee.com";

const RESOURCE_SUFFIXES: [&str; 7] = [".atlas", ".json", ".skel", ".png", ".webp", ".jpg", ".jpeg"];
const SKELETON_SUFFIXES: [&str; 2] = [".skel", ".json"];
const VIEWER_CORE_SUFFIXES: [&str; 2] = [".atlas", ".skel"];
const GAMEKEE_URL_FIELDS: [&str; 4] = ["atlas", "skel", "json", "bg"];

static EXPORT_DEFAULT_RE: LazyLock<regex::Regex> = LazyLock::new(|| pyre::compile(r"\bexport\s+default\b", false, false));

/// `ResourceCategory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Character,
    Ultimate,
    Dating,
    Unknown,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Character => "character",
            Self::Ultimate => "ultimate",
            Self::Dating => "dating",
            Self::Unknown => "unknown",
        }
    }

    /// The literal back from its text (tests build resources from goldens).
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "character" => Self::Character,
            "ultimate" => Self::Ultimate,
            "dating" => Self::Dating,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }
}

/// `ViewerResource`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerResource {
    pub entry_id: String,
    pub category: Category,
    pub stem: String,
    pub char_name: String,
    pub costume_name: String,
    pub files: Vec<String>,
    pub missing_core_files: Vec<String>,
}

impl ViewerResource {
    /// `asdict(resource)`.
    pub fn to_value(&self) -> Value {
        let mut dict = Dict::new();
        dict.insert("entry_id", Value::Str(self.entry_id.clone()));
        dict.insert("category", Value::Str(self.category.as_str().to_owned()));
        dict.insert("stem", Value::Str(self.stem.clone()));
        dict.insert("char_name", Value::Str(self.char_name.clone()));
        dict.insert("costume_name", Value::Str(self.costume_name.clone()));
        dict.insert("files", strings(&self.files));
        dict.insert("missing_core_files", strings(&self.missing_core_files));
        Value::Dict(dict)
    }
}

/// `GameKeeResource`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameKeeResource {
    pub content_id: i128,
    pub title: String,
    pub category: Category,
    pub stem: String,
    pub model_key: String,
    pub urls: Vec<String>,
    pub source_path: String,
}

impl GameKeeResource {
    /// `asdict(resource)`.
    pub fn to_value(&self) -> Value {
        let mut dict = Dict::new();
        dict.insert("content_id", Value::Int(self.content_id));
        dict.insert("title", Value::Str(self.title.clone()));
        dict.insert("category", Value::Str(self.category.as_str().to_owned()));
        dict.insert("stem", Value::Str(self.stem.clone()));
        dict.insert("model_key", Value::Str(self.model_key.clone()));
        dict.insert("urls", strings(&self.urls));
        dict.insert("source_path", Value::Str(self.source_path.clone()));
        Value::Dict(dict)
    }
}

fn strings(values: &[String]) -> Value {
    Value::List(values.iter().cloned().map(Value::Str).collect())
}

/// `ComparisonResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparisonResult {
    pub viewer_resources: Vec<ViewerResource>,
    pub gamekee_resources: Vec<GameKeeResource>,
    pub matched_stems: Vec<String>,
    pub viewer_only: Vec<ViewerResource>,
    pub gamekee_only: Vec<GameKeeResource>,
}

impl ComparisonResult {
    /// `summary()`.
    pub fn summary(&self) -> Dict {
        let viewer_stems: HashSet<String> = self.viewer_resources.iter().map(|resource| casefold(&resource.stem)).collect();
        let gamekee_stems: HashSet<String> = self.gamekee_resources.iter().map(|resource| casefold(&resource.stem)).collect();
        let count = |value: usize| Value::Int(i128::try_from(value).unwrap_or(i128::MAX));
        let mut dict = Dict::new();
        dict.insert("viewer_resource_count", count(self.viewer_resources.len()));
        dict.insert("viewer_unique_stem_count", count(viewer_stems.len()));
        dict.insert("gamekee_resource_count", count(self.gamekee_resources.len()));
        dict.insert("gamekee_unique_stem_count", count(gamekee_stems.len()));
        dict.insert("matched_unique_stem_count", count(self.matched_stems.len()));
        dict.insert("viewer_only_resource_count", count(self.viewer_only.len()));
        dict.insert("gamekee_only_resource_count", count(self.gamekee_only.len()));
        dict.insert(
            "viewer_missing_core_file_count",
            count(self.viewer_resources.iter().map(|resource| resource.missing_core_files.len()).sum()),
        );
        dict
    }

    /// `to_dict()`.
    pub fn to_value(&self) -> Value {
        let mut dict = Dict::new();
        dict.insert("summary", Value::Dict(self.summary()));
        dict.insert("matched_stems", strings(&self.matched_stems));
        dict.insert(
            "viewer_only",
            Value::List(self.viewer_only.iter().map(ViewerResource::to_value).collect()),
        );
        dict.insert(
            "gamekee_only",
            Value::List(self.gamekee_only.iter().map(GameKeeResource::to_value).collect()),
        );
        dict.insert(
            "viewer_missing_core_files",
            Value::List(
                self.viewer_resources
                    .iter()
                    .filter(|resource| !resource.missing_core_files.is_empty())
                    .map(ViewerResource::to_value)
                    .collect(),
            ),
        );
        Value::Dict(dict)
    }
}

/// A `ValueError` from `urllib.parse`.
pub(crate) fn url_error(error: &UrlError) -> PyError {
    PyError::new("ValueError", error.to_string())
}

/// `_to_int(value)`: an `int` (not a `bool`) or a string `str.isdigit()` accepts. A digit
/// `int()` refuses (a superscript) raises `ValueError`, as in Python.
pub(crate) fn to_int(value: Option<&Value>) -> Result<Option<i128>, PyError> {
    match value {
        Some(Value::Int(int)) => Ok(Some(*int)),
        Some(Value::Str(text)) => {
            let stripped = py_strip(text);
            if !py_isdigit(stripped) {
                return Ok(None);
            }
            // Beyond `i128` (Python has no limit) reads as the same `ValueError`; no
            // GameKee field comes close.
            py_int_decimal(stripped).map(Some).ok_or_else(|| invalid_int_literal(stripped))
        }
        _ => Ok(None),
    }
}

/// `int(text)`'s `ValueError` for a literal it refuses.
pub(crate) fn invalid_int_literal(text: &str) -> PyError {
    PyError::new("ValueError", format!("invalid literal for int() with base 10: {}", repr_str(text)))
}

/// `_string_value(value)`.
fn string_value(value: Option<&Value>) -> String {
    match value {
        Some(Value::Str(text)) => py_strip(text).to_owned(),
        _ => String::new(),
    }
}

/// `normalize_resource_url(raw_url)`.
pub fn normalize_resource_url(raw_url: &str) -> Result<String, PyError> {
    let stripped = py_strip(raw_url);
    if stripped.is_empty() {
        return Ok(String::new());
    }
    if stripped.starts_with("//") {
        return Ok(format!("https:{stripped}"));
    }
    urljoin(GAMEKEE_BASE_URL, stripped).map_err(|error| url_error(&error))
}

/// `viewer_asset_url(path)`.
pub fn viewer_asset_url(path: &str) -> String {
    let cleaned = py_strip(path).trim_matches('/');
    if cleaned.is_empty() {
        return String::new();
    }
    let quoted: Vec<String> = cleaned
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| quote(part, ""))
        .collect();
    format!("{VIEWER_RAW_SPINES_BASE_URL}{}", quoted.join("/"))
}

fn has_suffix_in(path: &str, suffixes: &[&str]) -> bool {
    let suffix = casefold(pypath::suffix(path));
    suffixes.contains(&suffix.as_str())
}

/// `resource_stem_from_url(url)`.
pub fn resource_stem_from_url(url: &str) -> Result<String, PyError> {
    let path = unquote(&urlsplit(url, "").map_err(|error| url_error(&error))?.path);
    let name = pypath::name(&path);
    if name.is_empty() || !has_suffix_in(name, &RESOURCE_SUFFIXES) {
        return Ok(String::new());
    }
    let mut stem = pypath::stem(name).to_owned();
    while has_suffix_in(&stem, &SKELETON_SUFFIXES) {
        stem = pypath::stem(&stem).to_owned();
    }
    Ok(stem)
}

/// `category_from_stem(stem)`.
pub fn category_from_stem(stem: &str) -> Category {
    let normalized = casefold(stem);
    if normalized.starts_with("cutscene_") {
        Category::Ultimate
    } else if normalized.starts_with("illust_dating") {
        Category::Dating
    } else if !normalized.is_empty() {
        Category::Character
    } else {
        Category::Unknown
    }
}

/// `parse_viewer_character_list(source)`: the object after `export default`, trailing
/// commas allowed, as an ordered map of entry id to its object.
pub fn parse_viewer_character_list(source: &str) -> Result<Vec<(String, Dict)>, PyError> {
    let search_start = EXPORT_DEFAULT_RE.find(source).map_or(0, |found| found.end());
    let object_source = extract_balanced_object(source, search_start)?;
    let parsed = json_loads(&strip_trailing_commas(object_source))?;
    let Value::Dict(parsed) = parsed else {
        return Err(PyError::new("TypeError", "BD2 L2D Viewer character list did not contain an object"));
    };
    let mut characters: Vec<(String, Dict)> = Vec::new();
    for (key, value) in parsed.iter() {
        if let Value::Dict(dict) = value {
            let entry_id = fav_runtime::pystr::str_key(key);
            // `characters[str(entry_id)] = value`: a repeated id keeps its first place.
            if let Some(slot) = characters.iter_mut().find(|(existing, _)| *existing == entry_id) {
                slot.1 = dict.clone();
            } else {
                characters.push((entry_id, dict.clone()));
            }
        }
    }
    Ok(characters)
}

/// `viewer_asset_paths_from_tree_payload(payload)`.
pub fn viewer_asset_paths_from_tree_payload(payload: &Dict) -> Result<BTreeSet<String>, PyError> {
    let Some(Value::List(tree)) = payload.get("tree") else {
        return Err(PyError::new("TypeError", "GitHub tree payload did not contain a tree list"));
    };
    let mut paths = BTreeSet::new();
    for item in tree {
        let Value::Dict(item) = item else { continue };
        if item.get("type").and_then(Value::as_str) != Some("blob") {
            continue;
        }
        if let Some(Value::Str(path)) = item.get("path")
            && let Some(rest) = path.strip_prefix(VIEWER_SPINES_TREE_PREFIX)
        {
            paths.insert(rest.to_owned());
        }
    }
    Ok(paths)
}

/// `viewer_resources_from_character_list(characters, asset_paths=...)`.
pub fn viewer_resources_from_character_list(characters: &[(String, Dict)], asset_paths: Option<&BTreeSet<String>>) -> Vec<ViewerResource> {
    let mut resources = Vec::new();
    for (entry_id, character) in characters {
        let char_name = string_value(character.get("charName"));
        let costume_name = string_value(character.get("costumeName"));
        for (category, field_name) in [
            (Category::Character, "spine"),
            (Category::Ultimate, "cutscene"),
            (Category::Dating, "dating"),
        ] {
            let stem = string_value(character.get(field_name));
            if stem.is_empty() {
                continue;
            }
            let files = viewer_resource_files(entry_id, category, &stem, asset_paths);
            let missing_core_files = viewer_missing_core_files(entry_id, category, &stem, asset_paths);
            resources.push(ViewerResource {
                entry_id: entry_id.clone(),
                category,
                stem,
                char_name: char_name.clone(),
                costume_name: costume_name.clone(),
                files,
                missing_core_files,
            });
        }
    }
    resources.sort_by(|a, b| (&a.entry_id, a.category.as_str(), &a.stem).cmp(&(&b.entry_id, b.category.as_str(), &b.stem)));
    resources
}

/// `gamekee_resources_from_manifest(manifest, source_path=...)`.
pub fn gamekee_resources_from_manifest(manifest: &Dict, source_path: &str) -> Result<Vec<GameKeeResource>, PyError> {
    let content_id = to_int(manifest.get("content_id"))?.unwrap_or(0);
    let title = string_value(manifest.get("title"));
    let Some(Value::List(models)) = manifest.get("live2d_models") else {
        return Ok(Vec::new());
    };
    let mut resources = Vec::new();
    for model in models {
        let Value::Dict(model) = model else { continue };
        let urls = urls_from_model(model.get("urls"))?;
        let stem = resource_stem_from_urls(&urls)?;
        if stem.is_empty() {
            continue;
        }
        resources.push(GameKeeResource {
            content_id,
            title: title.clone(),
            category: category_from_stem(&stem),
            stem,
            model_key: string_value(model.get("live2d_key")),
            urls,
            source_path: source_path.to_owned(),
        });
    }
    Ok(resources)
}

/// Python `a or b` on optional dictionary values: the first truthy one, else the last.
fn first_truthy<'a>(values: &[Option<&'a Value>]) -> Option<&'a Value> {
    let mut last = None;
    for value in values {
        last = *value;
        if value.is_some_and(Value::is_truthy) {
            return *value;
        }
    }
    last
}

/// `gamekee_resources_from_detail_payload(payload, tree_row=...)`.
pub fn gamekee_resources_from_detail_payload(payload: &Dict, tree_row: Option<&Dict>) -> Result<Vec<GameKeeResource>, PyError> {
    let Some(Value::Dict(detail)) = payload.get("data") else {
        return Err(PyError::new("TypeError", "GameKee detail payload did not contain data"));
    };
    let mut content_id = 0;
    for value in [
        detail.get("content_id"),
        detail.get("id"),
        tree_row.and_then(|row| row.get("content_id")),
    ] {
        if let Some(found) = to_int(value)?.filter(|found| *found != 0) {
            content_id = found;
            break;
        }
    }
    let title = string_value(first_truthy(&[
        detail.get("title"),
        detail.get("name"),
        tree_row.and_then(|row| row.get("name")),
    ]));
    let content_json = coerce_content_json(detail.get("content_json"))?;
    let mut resources = Vec::new();
    for value in iter_live2d_values(&Value::Dict(content_json)) {
        let urls = urls_from_live2d_value(value)?;
        let stem = resource_stem_from_urls(&urls)?;
        if stem.is_empty() {
            continue;
        }
        resources.push(GameKeeResource {
            content_id,
            title: title.clone(),
            category: category_from_stem(&stem),
            stem,
            model_key: string_value(value.get("live2dKey")),
            urls,
            source_path: String::new(),
        });
    }
    let mut resources = dedupe_gamekee_resources(resources);
    sort_gamekee(&mut resources);
    Ok(resources)
}

/// `fetch_viewer_resources(character_list_url=..., tree_url=...)`, with the client
/// injected (Python builds `httpx.Client(follow_redirects=True, timeout=60.0)`).
pub async fn fetch_viewer_resources(client: &HttpClient, character_list_url: &str, tree_url: &str) -> Result<Vec<ViewerResource>, PyError> {
    let transport = |error: fav_net::TransportError| PyError::new(error.class.name(), error.message.clone());
    let status = |error: fav_net::StatusError| PyError::new("HTTPStatusError", error.to_string());
    let character_response = client.send(Request::get(character_list_url)).await.map_err(transport)?;
    character_response.error_for_status().map_err(status)?;
    let character_body = character_response.bytes().await.map_err(transport)?;
    let tree_response = client.send(Request::get(tree_url)).await.map_err(transport)?;
    tree_response.error_for_status().map_err(status)?;
    let tree_body = tree_response.bytes().await.map_err(transport)?;

    // `response.text`: the body as UTF-8 with replacement characters.
    let characters = parse_viewer_character_list(&String::from_utf8_lossy(&character_body))?;
    let Value::Dict(tree_payload) = json_from_bytes(&tree_body)? else {
        return Err(PyError::new("TypeError", "GitHub tree response did not contain an object"));
    };
    let asset_paths = viewer_asset_paths_from_tree_payload(&tree_payload)?;
    Ok(viewer_resources_from_character_list(&characters, Some(&asset_paths)))
}

/// `compare_resources(gamekee_resources=..., viewer_resources=...)`.
pub fn compare_resources(gamekee_resources: &[GameKeeResource], viewer_resources: &[ViewerResource]) -> ComparisonResult {
    let gamekee_stems: HashSet<String> = gamekee_resources.iter().map(|resource| casefold(&resource.stem)).collect();
    let viewer_stems: HashSet<String> = viewer_resources.iter().map(|resource| casefold(&resource.stem)).collect();
    let matched: BTreeSet<String> = gamekee_stems.intersection(&viewer_stems).cloned().collect();
    ComparisonResult {
        viewer_resources: viewer_resources.to_vec(),
        gamekee_resources: gamekee_resources.to_vec(),
        matched_stems: matched.into_iter().collect(),
        viewer_only: viewer_resources
            .iter()
            .filter(|resource| !gamekee_stems.contains(&casefold(&resource.stem)))
            .cloned()
            .collect(),
        gamekee_only: gamekee_resources
            .iter()
            .filter(|resource| !viewer_stems.contains(&casefold(&resource.stem)))
            .cloned()
            .collect(),
    }
}

// ---------- character list text ----------

/// `_extract_balanced_object(source, search_start)`: from the first `{` at or after
/// `search_start` (a byte offset on a character boundary) to its matching `}`.
fn extract_balanced_object(source: &str, search_start: usize) -> Result<&str, PyError> {
    let Some(offset) = source[search_start..].find('{') else {
        return Err(PyError::new(
            "ValueError",
            "Could not find object start in BD2 L2D Viewer character list",
        ));
    };
    let object_start = search_start + offset;
    let mut depth = 0_i64;
    let mut quote_char: Option<char> = None;
    let mut escaped = false;
    for (index, ch) in source[object_start..].char_indices() {
        if let Some(open) = quote_char {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == open {
                quote_char = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote_char = Some(ch),
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = object_start + index + ch.len_utf8();
                    return Ok(&source[object_start..end]);
                }
            }
            _ => {}
        }
    }
    Err(PyError::new(
        "ValueError",
        "Could not find object end in BD2 L2D Viewer character list",
    ))
}

/// `_strip_trailing_commas(source)`: a `,` followed only by whitespace and `}` or `]`
/// goes, outside quoted strings.
fn strip_trailing_commas(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut output = String::with_capacity(source.len());
    let mut quote_char: Option<char> = None;
    let mut escaped = false;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if let Some(open) = quote_char {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == open {
                quote_char = None;
            }
            index += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote_char = Some(ch);
            output.push(ch);
            index += 1;
            continue;
        }
        if ch == ',' {
            let mut next = index + 1;
            while next < chars.len() && is_py_space(chars[next]) {
                next += 1;
            }
            if next < chars.len() && matches!(chars[next], '}' | ']') {
                index += 1;
                continue;
            }
        }
        output.push(ch);
        index += 1;
    }
    output
}

// ---------- viewer files ----------

fn viewer_resource_dirs(entry_id: &str, category: Category) -> Vec<String> {
    match category {
        Category::Ultimate => vec![format!("{entry_id}/cutscene")],
        Category::Dating => vec![format!("{entry_id}/dating"), format!("{entry_id}/dating_nobg")],
        Category::Character | Category::Unknown => vec![entry_id.to_owned()],
    }
}

fn viewer_resource_files(entry_id: &str, category: Category, stem: &str, asset_paths: Option<&BTreeSet<String>>) -> Vec<String> {
    let Some(asset_paths) = asset_paths else {
        let directory = &viewer_resource_dirs(entry_id, category)[0];
        return VIEWER_CORE_SUFFIXES
            .iter()
            .map(|suffix| format!("{directory}/{stem}{suffix}"))
            .collect();
    };
    // A `BTreeSet` iterates sorted, which is `sorted(files)`.
    asset_paths
        .iter()
        .filter(|path| viewer_path_matches_resource(path, entry_id, category, stem))
        .cloned()
        .collect()
}

fn viewer_missing_core_files(entry_id: &str, category: Category, stem: &str, asset_paths: Option<&BTreeSet<String>>) -> Vec<String> {
    let Some(asset_paths) = asset_paths else {
        return Vec::new();
    };
    let existing_files = viewer_resource_files(entry_id, category, stem, Some(asset_paths));
    let mut existing_dirs: BTreeSet<String> = existing_files.iter().map(|path| dirname(path).to_owned()).collect();
    if existing_dirs.is_empty() {
        existing_dirs.insert(viewer_resource_dirs(entry_id, category)[0].clone());
    }
    let mut missing = Vec::new();
    for directory in &existing_dirs {
        let atlas = format!("{directory}/{stem}.atlas");
        if !asset_paths.contains(&atlas) {
            missing.push(atlas);
        }
        if !SKELETON_SUFFIXES
            .iter()
            .any(|suffix| asset_paths.contains(&format!("{directory}/{stem}{suffix}")))
        {
            missing.push(format!("{directory}/{stem}.skel"));
        }
    }
    missing
}

fn viewer_path_matches_resource(path: &str, entry_id: &str, category: Category, stem: &str) -> bool {
    let directory = dirname(path);
    if !viewer_resource_dirs(entry_id, category).iter().any(|dir| dir == directory) {
        return false;
    }
    let name = pypath::name(path);
    if !has_suffix_in(name, &RESOURCE_SUFFIXES) {
        return false;
    }
    let path_stem = pypath::stem(name);
    path_stem == stem || path_stem.starts_with(&format!("{stem}_"))
}

/// `_dirname(path)`.
fn dirname(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(head, _)| head)
}

// ---------- GameKee URLs ----------

fn urls_from_model(raw_urls: Option<&Value>) -> Result<Vec<String>, PyError> {
    let Some(Value::Dict(raw_urls)) = raw_urls else {
        return Ok(Vec::new());
    };
    let mut urls = Vec::new();
    for field_name in GAMEKEE_URL_FIELDS.iter().chain(&["image", "atlas_textures"]) {
        urls.extend(coerce_url_list(raw_urls.get(field_name))?);
    }
    Ok(dedupe_strings(urls))
}

fn urls_from_live2d_value(value: &Dict) -> Result<Vec<String>, PyError> {
    let mut urls = Vec::new();
    for field_name in GAMEKEE_URL_FIELDS.iter().chain(&["image"]) {
        urls.extend(coerce_url_list(value.get(field_name))?);
    }
    Ok(dedupe_strings(urls))
}

fn coerce_url_list(value: Option<&Value>) -> Result<Vec<String>, PyError> {
    let raw: Vec<&str> = match value {
        Some(Value::List(items)) => items.iter().filter_map(Value::as_str).collect(),
        Some(Value::Str(text)) => text.split(',').collect(),
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    for item in raw {
        let normalized = normalize_resource_url(item)?;
        if !normalized.is_empty() {
            out.push(normalized);
        }
    }
    Ok(out)
}

fn resource_stem_from_urls(urls: &[String]) -> Result<String, PyError> {
    for suffix in [".atlas", ".skel", ".json", ".png", ".webp"] {
        for url in urls {
            let stem = resource_stem_from_url(url)?;
            if stem.is_empty() {
                continue;
            }
            let path = urlsplit(url, "").map_err(|error| url_error(&error))?.path;
            if casefold(&path).ends_with(suffix) {
                return Ok(stem);
            }
        }
    }
    Ok(String::new())
}

/// `_coerce_content_json(value)`: a dict, a JSON object in a non-blank string (a parse
/// error raises), else `{}`.
fn coerce_content_json(value: Option<&Value>) -> Result<Dict, PyError> {
    match value {
        Some(Value::Dict(dict)) => Ok(dict.clone()),
        Some(Value::Str(text)) if !py_strip(text).is_empty() => match json_loads(text)? {
            Value::Dict(dict) => Ok(dict),
            _ => Ok(Dict::new()),
        },
        _ => Ok(Dict::new()),
    }
}

/// `_iter_live2d_values(node)`: every `value` dict of a `{"type": "live2d"}` node, depth
/// first in document order.
fn iter_live2d_values(node: &Value) -> Vec<&Dict> {
    fn walk<'a>(current: &'a Value, values: &mut Vec<&'a Dict>) {
        match current {
            Value::Dict(dict) => {
                if dict.get("type").and_then(Value::as_str) == Some("live2d")
                    && let Some(Value::Dict(value)) = dict.get("value")
                {
                    values.push(value);
                }
                for (_, child) in dict.iter() {
                    walk(child, values);
                }
            }
            Value::List(items) => {
                for child in items {
                    walk(child, values);
                }
            }
            _ => {}
        }
    }
    let mut values = Vec::new();
    walk(node, &mut values);
    values
}

fn dedupe_gamekee_resources(resources: Vec<GameKeeResource>) -> Vec<GameKeeResource> {
    let mut order: Vec<(i128, String, Category)> = Vec::new();
    let mut by_key: HashMap<(i128, String, Category), GameKeeResource> = HashMap::new();
    for resource in resources {
        let key = (resource.content_id, casefold(&resource.stem), resource.category);
        if !by_key.contains_key(&key) {
            order.push(key.clone());
        }
        by_key.insert(key, resource);
    }
    order.into_iter().filter_map(|key| by_key.remove(&key)).collect()
}

fn dedupe_strings(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values.into_iter().filter(|value| seen.insert(value.clone())).collect()
}

fn sort_gamekee(resources: &mut [GameKeeResource]) {
    resources.sort_by(|a, b| (a.content_id, a.category.as_str(), &a.stem).cmp(&(b.content_id, b.category.as_str(), &b.stem)));
}

#[cfg(test)]
mod tests;
