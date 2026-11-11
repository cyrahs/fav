//! The on-disk summary cache of a game collection's list routes.
//!
//! Python origin: `src/api/summary_cache.py`. Version 1 (`read_summary_cache`,
//! `write_summary_cache`, `manifest_signature`) keeps the list summaries next to the
//! collection (`<root>/_api/summary-cache.json`) with the signature of the manifests they
//! were built from, so a restarted API answers without reading every manifest again.
//! Version 2 (`read_summary_records`, `write_summary_records`) keeps one record per
//! manifest instead, and is read whatever its signature: Azur Lane rebuilds a stale cache
//! incrementally from it.
//!
//! The file is the one Python writes (`json.dumps(..., ensure_ascii=False,
//! sort_keys=True, separators=(',', ':'))` and a newline, replaced atomically), so either
//! implementation reads the other's cache.

use std::path::{Path, PathBuf};

use fav_core::logging::Logger;
use fav_core::pyjson::{self, Dict, DumpOptions, Value};

/// `SUMMARY_CACHE_VERSION`.
pub(crate) const SUMMARY_CACHE_VERSION: i128 = 1;

/// `SUMMARY_RECORDS_VERSION`.
pub(crate) const SUMMARY_RECORDS_VERSION: i128 = 2;

/// `RecordFreshness`: what decides between two manifests of one key (`float`, `float`,
/// `int`).
pub(crate) type Freshness = (f64, f64, i128);

/// `SummaryRecord`: one manifest's summary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SummaryRecord {
    pub key: String,
    pub manifest_path: String,
    pub freshness: Freshness,
    pub summary: Dict,
}

/// `SummaryRecordsData`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SummaryRecordsData {
    pub signature: Signature,
    pub records: Vec<SummaryRecord>,
}

/// `ManifestEntry`: a manifest's path, `st_mtime_ns` and `st_size`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestEntry {
    pub path: PathBuf,
    pub mtime_ns: i128,
    pub size: i128,
}

impl ManifestEntry {
    /// The entry for a manifest, from its metadata.
    pub(crate) fn from_metadata(path: PathBuf, metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            path,
            mtime_ns: i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()),
            size: i128::from(metadata.size()),
        }
    }
}

/// `ManifestSignature`: `(path.as_posix(), mtime_ns, size)` per manifest.
pub(crate) type Signature = Vec<(String, i128, i128)>;

/// `manifest_signature`.
pub(crate) fn manifest_signature(entries: &[ManifestEntry]) -> Signature {
    entries
        .iter()
        .map(|entry| (entry.path.to_string_lossy().into_owned(), entry.mtime_ns, entry.size))
        .collect()
}

/// `SummaryCacheData`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SummaryCacheData {
    pub signature: Signature,
    pub records: Vec<Dict>,
    pub summaries: Vec<Dict>,
}

/// `read_summary_cache`: the cache at `path` when it is readable, well formed and was
/// built from `expected_signature`. A missing file is silently `None`; an unreadable one
/// is logged first.
pub(crate) fn read_summary_cache(path: &Path, expected_signature: &Signature, log: &Logger, label: &str) -> Option<SummaryCacheData> {
    decode_summary_cache_payload(&read_payload(path, log, label)?).filter(|data| data.signature == *expected_signature)
}

/// `read_summary_records`: the version-2 cache at `path`, whatever manifests it was built
/// from (a stale cache is still the base of an incremental rebuild).
pub(crate) fn read_summary_records(path: &Path, log: &Logger, label: &str) -> Option<SummaryRecordsData> {
    decode_summary_records_payload(&read_payload(path, log, label)?)
}

/// `write_summary_records`: replace the cache atomically; a failure is logged, not raised.
pub(crate) fn write_summary_records(path: &Path, data: &SummaryRecordsData, log: &Logger, label: &str) {
    let records = data
        .records
        .iter()
        .map(|record| {
            let mut item = Dict::new();
            item.insert("key", Value::Str(record.key.clone()));
            item.insert("manifest_path", Value::Str(record.manifest_path.clone()));
            let (first, second, third) = record.freshness;
            item.insert(
                "freshness",
                Value::List(vec![Value::Float(first), Value::Float(second), Value::Int(third)]),
            );
            item.insert("summary", Value::Dict(record.summary.clone()));
            Value::Dict(item)
        })
        .collect();
    let mut payload = Dict::new();
    payload.insert("version", Value::Int(SUMMARY_RECORDS_VERSION));
    payload.insert("signature", encode_signature(&data.signature));
    payload.insert("records", Value::List(records));
    write_cache_payload(path, &Value::Dict(payload), log, label);
}

/// `json.loads(path.read_text(encoding='utf-8'))`: a missing file is silently `None`; an
/// unreadable one is logged first.
fn read_payload(path: &Path, log: &Logger, label: &str) -> Option<Value> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            log.warning(format!("Skipping unreadable {label} summary cache {}: {error}", path.display()));
            return None;
        }
    };
    let payload = std::str::from_utf8(&bytes)
        .map_err(|error| format!("'utf-8' codec can't decode bytes: {error}"))
        .and_then(|text| {
            if text.starts_with('\u{feff}') {
                return Err("Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)".to_owned());
            }
            pyjson::loads(text).map_err(|error| error.python_message(text))
        });
    match payload {
        Ok(payload) => Some(payload),
        Err(message) => {
            log.warning(format!("Skipping unreadable {label} summary cache {}: {message}", path.display()));
            None
        }
    }
}

/// `write_summary_cache`: replace the cache atomically; a failure is logged, not raised.
pub(crate) fn write_summary_cache(path: &Path, data: &SummaryCacheData, log: &Logger, label: &str) {
    let mut payload = Dict::new();
    payload.insert("version", Value::Int(SUMMARY_CACHE_VERSION));
    payload.insert("signature", encode_signature(&data.signature));
    payload.insert("records", Value::List(data.records.iter().cloned().map(Value::Dict).collect()));
    payload.insert("summaries", Value::List(data.summaries.iter().cloned().map(Value::Dict).collect()));
    write_cache_payload(path, &Value::Dict(payload), log, label);
}

/// `_write_cache_payload`.
fn write_cache_payload(path: &Path, payload: &Value, log: &Logger, label: &str) {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp-{}-{}", std::process::id(), token_hex(4)));
    let result = (|| -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        // A key mix `sort_keys` cannot order is Python's `TypeError`, caught the same way.
        let text = pyjson::dumps(payload, &DumpOptions::hash()).map_err(|error| error.to_string())?;
        std::fs::write(&tmp, format!("{text}\n")).map_err(|error| error.to_string())?;
        std::fs::rename(&tmp, path).map_err(|error| error.to_string())
    })();
    if let Err(message) = result {
        log.warning(format!("Failed to write {label} summary cache {}: {message}", path.display()));
    }
    let _ = std::fs::remove_file(&tmp);
}

/// `secrets.token_hex(n)`: `n` random bytes as hex, from the standard library's seeded
/// hasher (the name only has to differ between concurrent writers).
fn token_hex(bytes: usize) -> String {
    use std::fmt::Write as _;
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut out = String::with_capacity(bytes * 2 + 16);
    while out.len() < bytes * 2 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_usize(out.len());
        let _ = write!(out, "{:016x}", hasher.finish());
    }
    out.truncate(bytes * 2);
    out
}

/// `payload.get('version') != SUMMARY_CACHE_VERSION`, with Python's `==` (`1.0` and
/// `True` equal `1`).
fn is_version(value: Option<&Value>, version: i128) -> bool {
    match value {
        Some(Value::Int(int)) => *int == version,
        Some(Value::Bool(flag)) => i128::from(*flag) == version,
        #[allow(clippy::cast_precision_loss, clippy::float_cmp)] // Python compares the exact values; the version is small.
        Some(Value::Float(float)) => *float == version as f64,
        _ => false,
    }
}

/// `_decode_summary_cache_payload`.
fn decode_summary_cache_payload(payload: &Value) -> Option<SummaryCacheData> {
    let Value::Dict(payload) = payload else {
        return None;
    };
    if !is_version(payload.get("version"), SUMMARY_CACHE_VERSION) {
        return None;
    }
    let signature = decode_signature(payload.get("signature"))?;
    let (Some(Value::List(records)), Some(Value::List(summaries))) = (payload.get("records"), payload.get("summaries")) else {
        return None;
    };
    let dicts = |items: &[Value]| -> Option<Vec<Dict>> {
        items
            .iter()
            .map(|item| match item {
                Value::Dict(dict) => Some(dict.clone()),
                _ => None,
            })
            .collect()
    };
    Some(SummaryCacheData {
        signature,
        records: dicts(records)?,
        summaries: dicts(summaries)?,
    })
}

/// `_decode_summary_records_payload`: every record must decode, or the cache is ignored.
fn decode_summary_records_payload(payload: &Value) -> Option<SummaryRecordsData> {
    let Value::Dict(payload) = payload else {
        return None;
    };
    if !is_version(payload.get("version"), SUMMARY_RECORDS_VERSION) {
        return None;
    }
    let signature = decode_signature(payload.get("signature"))?;
    let Some(Value::List(items)) = payload.get("records") else {
        return None;
    };
    let records = items.iter().map(decode_summary_record).collect::<Option<Vec<_>>>()?;
    Some(SummaryRecordsData { signature, records })
}

/// `_decode_summary_record`: a non-empty `key` and `manifest_path`, a dict `summary` and
/// a `freshness` of three numbers (booleans are not numbers here).
fn decode_summary_record(item: &Value) -> Option<SummaryRecord> {
    let Value::Dict(item) = item else {
        return None;
    };
    let (Some(Value::Str(key)), Some(Value::Str(manifest_path))) = (item.get("key"), item.get("manifest_path")) else {
        return None;
    };
    if key.is_empty() || manifest_path.is_empty() {
        return None;
    }
    let Some(Value::Dict(summary)) = item.get("summary") else {
        return None;
    };
    let Some(Value::List(freshness)) = item.get("freshness") else {
        return None;
    };
    let [first, second, third] = freshness.as_slice() else {
        return None;
    };
    #[allow(clippy::cast_precision_loss)] // `float(int)`, as Python converts.
    let float = |value: &Value| match value {
        Value::Int(int) => Some(*int as f64),
        Value::Float(float) => Some(*float),
        _ => None,
    };
    // `int(float)` truncates; Python's `int()` of an infinity or NaN raises, which no cache
    // Python wrote can hold, so such a record is not one.
    #[allow(clippy::cast_possible_truncation)] // Checked finite and in range first.
    let third = match third {
        Value::Int(int) => *int,
        Value::Float(float) if float.is_finite() && float.abs() < 1.0e38 => float.trunc() as i128,
        _ => return None,
    };
    Some(SummaryRecord {
        key: key.clone(),
        manifest_path: manifest_path.clone(),
        freshness: (float(first)?, float(second)?, third),
        summary: summary.clone(),
    })
}

/// `_encode_signature`.
fn encode_signature(signature: &Signature) -> Value {
    Value::List(
        signature
            .iter()
            .map(|(path, mtime_ns, size)| {
                let mut entry = Dict::new();
                entry.insert("path", Value::Str(path.clone()));
                entry.insert("mtime_ns", Value::Int(*mtime_ns));
                entry.insert("size", Value::Int(*size));
                Value::Dict(entry)
            })
            .collect(),
    )
}

/// `_decode_signature`: a list of `{path: str, mtime_ns: int, size: int}` (booleans are
/// not integers here).
fn decode_signature(value: Option<&Value>) -> Option<Signature> {
    let Some(Value::List(items)) = value else {
        return None;
    };
    items
        .iter()
        .map(|item| {
            let Value::Dict(item) = item else {
                return None;
            };
            match (item.get("path"), item.get("mtime_ns"), item.get("size")) {
                (Some(Value::Str(path)), Some(Value::Int(mtime_ns)), Some(Value::Int(size))) => Some((path.clone(), *mtime_ns, *size)),
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use fav_core::pyjson::{self, Dict, Value};

    use super::{
        ManifestEntry, SummaryCacheData, SummaryRecord, SummaryRecordsData, manifest_signature, read_summary_cache, read_summary_records,
        write_summary_cache, write_summary_records,
    };

    fn log() -> fav_core::logging::Logger {
        fav_core::logging::get("fav-api.test")
    }

    fn summary(content_id: i128) -> Dict {
        let mut dict = Dict::new();
        dict.insert("title", Value::from("Ä"));
        dict.insert("content_id", Value::Int(content_id));
        dict
    }

    #[test]
    fn a_written_cache_reads_back_only_for_its_signature() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("_api").join("summary-cache.json");
        let signature = manifest_signature(&[ManifestEntry {
            path: tmp.path().join("1 - A").join("manifest.json"),
            mtime_ns: 1_700_000_000_123_456_789,
            size: 42,
        }]);
        let data = SummaryCacheData {
            signature: signature.clone(),
            records: vec![summary(1)],
            summaries: vec![summary(1)],
        };
        write_summary_cache(&path, &data, &log(), "Nikke");

        let text = std::fs::read_to_string(&path).expect("cache");
        // Sorted keys, compact separators, non-ASCII kept, one trailing newline.
        assert!(text.starts_with(r#"{"records":[{"content_id":1,"title":"Ä"}],"signature":[{"mtime_ns":1700000000123456789,"#));
        assert!(text.ends_with("}\n"));
        assert_eq!(
            std::fs::read_dir(tmp.path().join("_api")).expect("dir").count(),
            1,
            "no temporary file left"
        );

        let read = read_summary_cache(&path, &signature, &log(), "Nikke").expect("cache");
        assert_eq!(read.signature, signature);
        assert_eq!(read.summaries.len(), 1);
        assert!(pyjson::dumps(&Value::Dict(read.summaries[0].clone()), &pyjson::DumpOptions::hash()).is_ok());
        assert_eq!(read_summary_cache(&path, &Vec::new(), &log(), "Nikke"), None);
    }

    #[test]
    fn malformed_caches_are_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("cache.json");
        assert_eq!(read_summary_cache(&path, &Vec::new(), &log(), "Nikke"), None, "missing");
        for (text, readable) in [
            ("not json", false),
            ("[]", false),
            (r#"{"version":2,"signature":[],"records":[],"summaries":[]}"#, false),
            (
                r#"{"version":1,"signature":[{"path":"a","mtime_ns":true,"size":1}],"records":[],"summaries":[]}"#,
                false,
            ),
            (r#"{"version":1,"signature":[],"records":[1],"summaries":[]}"#, false),
            (r#"{"version":1,"signature":[],"records":[],"summaries":{}}"#, false),
            (r#"{"version":1.0,"signature":[],"records":[],"summaries":[]}"#, true),
            (r#"{"version":true,"signature":[],"records":[],"summaries":[]}"#, true),
        ] {
            std::fs::write(&path, text).expect("write");
            assert_eq!(
                read_summary_cache(&path, &Vec::new(), &log(), "Nikke").is_some(),
                readable,
                "{text}"
            );
        }
    }

    #[test]
    fn records_read_back_whatever_their_signature() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("_api").join("summary-cache.json");
        let data = SummaryRecordsData {
            signature: vec![("a/manifest.json".to_owned(), 5, 6)],
            records: vec![SummaryRecord {
                key: "10001".to_owned(),
                manifest_path: "a/manifest.json".to_owned(),
                freshness: (1.5, 0.0, 7),
                // Keys in sorted order, as the file keeps them.
                summary: pyjson::loads(r#"{"content_id":1,"title":"Ä"}"#)
                    .expect("JSON")
                    .as_dict()
                    .cloned()
                    .expect("dict"),
            }],
        };
        write_summary_records(&path, &data, &log(), "Azur Lane");
        let text = std::fs::read_to_string(&path).expect("cache");
        assert!(text.starts_with(r#"{"records":[{"freshness":[1.5,0.0,7],"key":"10001","manifest_path":"a/manifest.json","#));
        assert!(text.ends_with("\"version\":2}\n"));
        assert_eq!(read_summary_records(&path, &log(), "Azur Lane"), Some(data));
        assert_eq!(
            read_summary_cache(&path, &Vec::new(), &log(), "Azur Lane"),
            None,
            "not a version-1 cache"
        );
    }

    #[test]
    fn malformed_records_are_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("cache.json");
        assert_eq!(read_summary_records(&path, &log(), "Azur Lane"), None, "missing");
        let record = |fields: &str| format!(r#"{{"version":2,"signature":[],"records":[{fields}]}}"#);
        for (text, readable) in [
            (
                record(r#"{"key":"k","manifest_path":"m","freshness":[1,2.5,3],"summary":{}}"#),
                true,
            ),
            (
                record(r#"{"key":"k","manifest_path":"m","freshness":[1,2,3.9],"summary":{}}"#),
                true,
            ),
            (record(r#"{"key":"","manifest_path":"m","freshness":[1,2,3],"summary":{}}"#), false),
            (record(r#"{"key":"k","manifest_path":"","freshness":[1,2,3],"summary":{}}"#), false),
            (record(r#"{"key":"k","manifest_path":"m","freshness":[1,2],"summary":{}}"#), false),
            (
                record(r#"{"key":"k","manifest_path":"m","freshness":[true,2,3],"summary":{}}"#),
                false,
            ),
            (
                record(r#"{"key":"k","manifest_path":"m","freshness":[1,"2",3],"summary":{}}"#),
                false,
            ),
            (record(r#"{"key":"k","manifest_path":"m","freshness":[1,2,3],"summary":[]}"#), false),
            (record("1"), false),
            (r#"{"version":1,"signature":[],"records":[]}"#.to_owned(), false),
            (r#"{"version":2,"signature":{},"records":[]}"#.to_owned(), false),
            (r#"{"version":2,"signature":[],"records":{}}"#.to_owned(), false),
        ] {
            std::fs::write(&path, &text).expect("write");
            assert_eq!(read_summary_records(&path, &log(), "Azur Lane").is_some(), readable, "{text}");
        }
        std::fs::write(
            &path,
            record(r#"{"key":"k","manifest_path":"m","freshness":[1,2,3.9],"summary":{}}"#),
        )
        .expect("write");
        let read = read_summary_records(&path, &log(), "Azur Lane").expect("cache");
        assert_eq!(read.records[0].freshness, (1.0, 2.0, 3));
    }
}
