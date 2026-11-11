//! The GameKee helpers of the NIKKE crawler that BD2 imports, and the file helpers Azur Lane
//! has copies of.
//!
//! Python origin: `src/web/nikke.py` (`Asset`, `add_asset`, `add_url_list`, `normalize_url`,
//! `asset_headers`, `response_json`, `parse_spine_atlas_textures`,
//! `original_filename_from_url` with NIKKE's own `extension_for_kind`, `context_hash`,
//! `is_cdn_rejection_body`, `validate_asset_response`, `materialize_blob`, `write_json`,
//! `file_sha256`, `_verify_file`), which `src/web/bd2.py` imports and `src/web/azurlane.py`
//! copies. WP18 folded NIKKE's and BD2's ports into this one module; what stays apart:
//! NIKKE's `Assets` (an indexed map the crawl and manifests share), its `add_asset` on it,
//! its infallible `context_hash` (NIKKE's `json_dumps` cannot fail) and `write_json`, and Azur
//! Lane's own `_validate_asset_response` (a different function in Python too).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use fav_core::filename::sanitize;
use fav_core::pycasefold::casefold;
use fav_core::pyjson::{self, Dict, DumpOptions, Value};
use fav_core::pypath;
use fav_core::pyurl::{unquote, urljoin, urlsplit};
use fav_runtime::pystr::repr_value;
use fav_runtime::source::JobError;
use sha2::{Digest as _, Sha256};

use crate::bd2_l2d_viewer::url_error;
use crate::pydecode::{PyError, json_loads, py_eq_int};
use crate::pyre;

pub(crate) const GAMEKEE_BASE_URL: &str = "https://www.gamekee.com";
pub(crate) const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
const DOWNLOAD_CHUNK_SIZE: usize = 1024 * 128;

static ATLAS_TEXTURE_RE: LazyLock<regex::Regex> = LazyLock::new(|| pyre::compile(r"^[^\s:]+\.(?:png|webp|jpe?g)$", true, false));

/// `Asset`: one URL in one role (`kind`), with every place on the page it came from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Asset {
    pub(crate) url: String,
    pub(crate) kind: String,
    pub(crate) local_path: String,
    pub(crate) content_type: String,
    pub(crate) size: i64,
    pub(crate) sha256: String,
    pub(crate) status: String,
    pub(crate) error: String,
    pub(crate) contexts: Vec<Dict>,
}

impl Asset {
    pub(crate) fn new(url: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            kind: kind.into(),
            local_path: String::new(),
            content_type: String::new(),
            size: 0,
            sha256: String::new(),
            status: "pending".to_owned(),
            error: String::new(),
            contexts: Vec::new(),
        }
    }

    /// `asdict(asset)`.
    #[cfg(test)]
    pub(crate) fn to_value(&self) -> Value {
        let mut dict = Dict::new();
        dict.insert("url", Value::Str(self.url.clone()));
        dict.insert("kind", Value::Str(self.kind.clone()));
        dict.insert("local_path", Value::Str(self.local_path.clone()));
        dict.insert("content_type", Value::Str(self.content_type.clone()));
        dict.insert("size", Value::Int(i128::from(self.size)));
        dict.insert("sha256", Value::Str(self.sha256.clone()));
        dict.insert("status", Value::Str(self.status.clone()));
        dict.insert("error", Value::Str(self.error.clone()));
        dict.insert("contexts", Value::List(self.contexts.iter().cloned().map(Value::Dict).collect()));
        Value::Dict(dict)
    }
}

/// `dict[tuple[str, str], Asset]`: assets by `(url, kind)` in insertion order.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Assets(Vec<Asset>);

impl Assets {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn position(&self, url: &str, kind: &str) -> Option<usize> {
        self.0.iter().position(|asset| asset.url == url && asset.kind == kind)
    }

    /// `assets[key] = asset` for a key that is not there yet.
    #[cfg(test)]
    pub(crate) fn push(&mut self, asset: Asset) {
        debug_assert!(self.position(&asset.url, &asset.kind).is_none());
        self.0.push(asset);
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, Asset> {
        self.0.iter()
    }

    pub(crate) fn iter_mut(&mut self) -> std::slice::IterMut<'_, Asset> {
        self.0.iter_mut()
    }

    pub(crate) fn as_slice(&self) -> &[Asset] {
        &self.0
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [Asset] {
        &mut self.0
    }

    /// Drop the assets `keep` refuses (`del assets[key]`).
    pub(crate) fn retain(&mut self, keep: impl FnMut(&Asset) -> bool) {
        self.0.retain(keep);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `[asdict(asset) for asset in assets.values()]`.
    #[cfg(test)]
    pub(crate) fn to_value(&self) -> Value {
        Value::List(self.0.iter().map(Asset::to_value).collect())
    }
}

/// `normalize_url(url)`: protocol-relative URLs get `https:`, root-relative ones GameKee's
/// origin.
pub(crate) fn normalize_url(url: &str) -> String {
    let value = fav_core::pytext::py_strip(url);
    if value.is_empty() {
        return String::new();
    }
    if value.starts_with("//") {
        return format!("https:{value}");
    }
    if value.starts_with('/') {
        // `urljoin` only rejects a bad network location, which a path that does not start
        // with `//` cannot have.
        return urljoin(GAMEKEE_BASE_URL, value).unwrap_or_else(|_| value.to_owned());
    }
    value.to_owned()
}

/// `add_asset(assets, url, kind, context)`.
pub(crate) fn add_asset(assets: &mut Assets, url: &str, kind: &str, context: Dict) {
    let normalized = normalize_url(url);
    if normalized.is_empty() {
        return;
    }
    if let Some(index) = assets.position(&normalized, kind) {
        assets.0[index].contexts.push(context);
    } else {
        let mut asset = Asset::new(normalized, kind);
        asset.contexts.push(context);
        assets.0.push(asset);
    }
}

/// `add_url_list(assets, raw_urls, kind, context)`: a comma-separated list of URLs.
pub(crate) fn add_url_list(assets: &mut Assets, raw_urls: &Value, kind: &str, context: &Dict) {
    let Value::Str(raw_urls) = raw_urls else { return };
    for raw_url in raw_urls.split(',') {
        add_asset(assets, raw_url, kind, context.clone());
    }
}

/// `asset_headers()`.
pub(crate) fn asset_headers() -> Vec<(String, String)> {
    vec![
        ("Accept".to_owned(), "*/*".to_owned()),
        ("Origin".to_owned(), GAMEKEE_BASE_URL.to_owned()),
        ("Referer".to_owned(), format!("{GAMEKEE_BASE_URL}/")),
        ("User-Agent".to_owned(), DEFAULT_USER_AGENT.to_owned()),
    ]
}

/// `response_json(response)` after `raise_for_status()` passed: an object whose `code` is 0.
pub(crate) fn response_json(body: &[u8]) -> Result<Dict, PyError> {
    let Value::Dict(data) = crate::pydecode::json_from_bytes(body)? else {
        return Err(PyError::new("TypeError", "GameKee returned a non-object JSON response"));
    };
    let code = data.get("code").cloned().unwrap_or(Value::Null);
    if !py_eq_int(&code, 0) {
        let msg = data.get("msg").cloned().unwrap_or(Value::Null);
        return Err(PyError::new(
            "RuntimeError",
            format!("GameKee API returned code={}, msg={}", repr_value(&code), repr_value(&msg)),
        ));
    }
    Ok(data)
}

/// `str.splitlines()`: Python's line boundaries, `\r\n` as one.
fn py_splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if matches!(
            ch,
            '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        ) {
            lines.push(&text[start..index]);
            let mut end = index + ch.len_utf8();
            if ch == '\r'
                && let Some(&(next_index, '\n')) = chars.peek()
            {
                chars.next();
                end = next_index + 1;
            }
            start = end;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// `parse_spine_atlas_textures(text)`: the page lines of a Spine atlas, first spelling of
/// each (compared casefolded).
pub(crate) fn parse_spine_atlas_textures(text: &str) -> Vec<String> {
    let mut textures = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in py_splitlines(text) {
        let value = fav_core::pytext::py_strip(line).trim_start_matches('\u{feff}');
        if value.is_empty() || value.contains(':') || !ATLAS_TEXTURE_RE.is_match(value) {
            continue;
        }
        if seen.insert(casefold(value)) {
            textures.push(value.to_owned());
        }
    }
    textures
}

/// NIKKE's `extension_for_kind` (no `audio`), which its `original_filename_from_url` uses.
pub(crate) fn nikke_extension_for_kind(kind: &str) -> &'static str {
    match kind {
        "video" => ".mp4",
        "live2d_atlas" => ".atlas",
        "live2d_skel" => ".skel",
        "live2d_json" => ".json",
        _ => ".bin",
    }
}

/// `original_filename_from_url(url, kind)`.
pub(crate) fn original_filename_from_url(url: &str, kind: &str) -> Result<String, PyError> {
    let split = urlsplit(url, "").map_err(|error| url_error(&error))?;
    let raw_name = unquote(pypath::name(&split.path));
    let fallback = || format!("{kind}{}", nikke_extension_for_kind(kind));
    if raw_name.is_empty() {
        return Ok(fallback());
    }
    let sanitized = sanitize(&raw_name, 180);
    Ok(if sanitized.is_empty() { fallback() } else { sanitized })
}

/// `_json_dumps(data)`: the compact, sorted text the hashes and JSONB columns use.
pub(crate) fn json_dumps(value: &Value) -> Result<String, JobError> {
    pyjson::dumps(value, &DumpOptions::hash()).map_err(|error| JobError::new("TypeError", error.to_string()))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes).iter().fold(String::with_capacity(64), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// `context_hash(context)`.
pub(crate) fn context_hash(context: &Dict) -> Result<String, JobError> {
    Ok(sha256_hex(json_dumps(&Value::Dict(context.clone()))?.as_bytes()))
}

/// `bytes.lstrip()`: ASCII whitespace.
pub(crate) fn lstrip_bytes(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// `is_cdn_rejection_body(content_type, body_prefix)`: an HTML page or an access-denied text
/// instead of the file.
pub(crate) fn is_cdn_rejection_body(content_type: &str, body_prefix: &[u8]) -> bool {
    if content_type.to_lowercase().contains("text/html") {
        return true;
    }
    let stripped = lstrip_bytes(body_prefix).to_ascii_lowercase();
    if stripped.starts_with(b"<!doctype html") || stripped.starts_with(b"<html") {
        return true;
    }
    [
        b"accessdenied".as_slice(),
        b"access denied",
        b"forbidden",
        b"<error>",
        b"error code",
    ]
    .iter()
    .any(|signature| contains_bytes(&stripped, signature))
}

/// `bytes.decode('utf-8', errors='ignore')`.
pub(crate) fn decode_utf8_ignore(bytes: &[u8]) -> String {
    bytes.utf8_chunks().map(|chunk| chunk.valid()).collect()
}

/// `validate_asset_response(kind=..., content_type=..., body_prefix=..., size=...)`: why the
/// body is not the file asked for, or `""`.
pub(crate) fn validate_asset_response(kind: &str, content_type: &str, body_prefix: &[u8], size: i64) -> &'static str {
    if size <= 0 {
        return "empty response body";
    }
    if is_cdn_rejection_body(content_type, body_prefix) {
        return "cdn rejection response";
    }
    let stripped = lstrip_bytes(body_prefix);
    if stripped.starts_with(b"{") && kind != "live2d_json" {
        let Ok(parsed) = json_loads(&decode_utf8_ignore(stripped)) else {
            return "";
        };
        if let Value::Dict(parsed) = parsed
            && ["code", "msg", "message", "error"].iter().any(|key| parsed.contains_key(key))
        {
            return "json error response";
        }
    }
    ""
}

/// `file_sha256(path)`.
pub(crate) fn file_sha256(path: &Path) -> std::io::Result<String> {
    use std::fmt::Write as _;
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; DOWNLOAD_CHUNK_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().iter().fold(String::with_capacity(64), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    }))
}

/// `_verify_file(path, sha256=..., size=...)`.
pub(crate) fn verify_file(path: &Path, sha256: &str, size: i64) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if i64::try_from(metadata.len()).ok() != Some(size) {
        return false;
    }
    file_sha256(path).is_ok_and(|digest| digest == sha256)
}

/// `.{name}.tmp-{pid}-{token_hex(4)}` beside `path`.
fn temp_sibling(path: &Path) -> PathBuf {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.tmp-{}-{}", std::process::id(), rand_token()))
}

/// `secrets.token_hex(4)`: eight hex digits that differ between calls of this process (the
/// pid in the name tells processes apart).
fn rand_token() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:08x}", nanos.rotate_left(13) ^ count.wrapping_mul(0x9e37_79b9))
}

/// `OSError` as the run reports it (`str(exc)` names the file).
pub(crate) fn os_error(error: &std::io::Error, path: &Path) -> JobError {
    let py = fav_net::download::PyOsError::from_io(error, Some(path));
    JobError::new(py.class, py.to_string())
}

/// `os.link`, replaceable in tests.
pub(crate) type LinkFn = fn(&Path, &Path) -> std::io::Result<()>;

/// `os.link(src, dst)`.
pub(crate) fn hard_link(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::hard_link(src, dst)
}

/// `shutil.copy2(src, dst)`: contents and times.
pub(crate) fn copy2(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst)?;
    let metadata = std::fs::metadata(src)?;
    let times = std::fs::FileTimes::new()
        .set_accessed(metadata.accessed()?)
        .set_modified(metadata.modified()?);
    std::fs::File::options().write(true).open(dst)?.set_times(times)
}

/// `materialize_blob(blob_path=..., destination=..., sha256=..., size=...)`: a hard link to
/// the blob (`link`, a copy where linking fails), verified before it replaces the destination.
pub(crate) fn materialize_blob(blob_path: &Path, destination: &Path, sha256: &str, size: i64, link: LinkFn) -> Result<(), JobError> {
    if destination.exists() && verify_file(destination, sha256, size) {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| os_error(&error, parent))?;
    }
    let tmp = temp_sibling(destination);
    let result = (|| {
        if link(blob_path, &tmp).is_err() {
            copy2(blob_path, &tmp).map_err(|error| os_error(&error, blob_path))?;
        }
        if !verify_file(&tmp, sha256, size) {
            return Err(JobError::new(
                "InvalidAssetResponseError",
                format!("Blob materialization verification failed: {}", destination.display()),
            ));
        }
        std::fs::rename(&tmp, destination).map_err(|error| os_error(&error, &tmp))
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// `write_json(path, data)`: pretty, sorted JSON and a newline, through a temporary file.
pub(crate) fn write_json(path: &Path, data: &Value) -> Result<(), JobError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| os_error(&error, parent))?;
    }
    let mut text = pyjson::dumps(data, &DumpOptions::pretty()).map_err(|error| JobError::new("TypeError", error.to_string()))?;
    text.push('\n');
    let tmp = temp_sibling(path);
    let result = std::fs::write(&tmp, text.as_bytes())
        .map_err(|error| os_error(&error, &tmp))
        .and_then(|()| std::fs::rename(&tmp, path).map_err(|error| os_error(&error, &tmp)));
    let _ = std::fs::remove_file(&tmp);
    result
}
