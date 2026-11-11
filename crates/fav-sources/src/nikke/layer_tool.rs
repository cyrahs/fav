//! The layer metadata maintenance commands (`script/nikke_layer_metadata.py`), behind
//! `fav nikke-layers`: merge a capture into a page, merge a directory of captures,
//! validate, strip for rollback, and capture or reuse runtime layers for the pages that
//! need them.
//!
//! Each command returns the report the script prints (`json.dumps(report,
//! ensure_ascii=False, indent=2, sort_keys=True)`) and its exit status. What the script
//! raises (`SystemExit` with a message, or an uncaught error) is an `Err`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fav_core::pyjson::{Dict, Value};
use regex::Regex;

use super::runtime::{RuntimeCaptureRequest, RuntimeCapturer};
use crate::nikke_layer_metadata::{
    LAYER_CAPTURE_RAW_PATH, evaluate_layer_capture_reuse, has_multi_full_layer_groups, merge_layer_capture_files,
    merge_live2d_layer_captures, read_json_object, read_previous_layer_capture_artifact, strip_layer_metadata_files, to_int,
    validate_live2d_layer_metadata, write_json,
};
use crate::pydecode::PyError;

/// The files a write may change, in the order they are backed up and restored.
const TARGET_FILES: [&str; 3] = ["manifest.json", "character.json", LAYER_CAPTURE_RAW_PATH];

/// `merge_layer_capture_files`, injectable so a test can make the write fail.
pub type MergeFilesFn = fn(&Path, &Dict, bool) -> Result<Dict, PyError>;

/// A command's report and exit status.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub report: Dict,
    pub status: u8,
}

fn dict_of(entries: Vec<(&str, Value)>) -> Dict {
    let mut dict = Dict::new();
    for (key, value) in entries {
        dict.insert(key, value);
    }
    dict
}

fn posix(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn system_exit(message: impl Into<String>) -> PyError {
    PyError::new("SystemExit", message)
}

/// The `OSError` subclasses `except OSError` catches.
fn is_os_error(class: &str) -> bool {
    matches!(
        class,
        "OSError" | "FileNotFoundError" | "PermissionError" | "FileExistsError" | "IsADirectoryError" | "NotADirectoryError"
    )
}

/// `capture_content_id` / `manifest_content_id`: the file's `content_id`.
fn file_content_id(path: &Path) -> Result<Option<i128>, PyError> {
    Ok(to_int(read_json_object(path)?.get("content_id")))
}

/// `sorted(nikke_root.glob('*/manifest.json'))`.
fn manifest_paths(nikke_root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(nikke_root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("manifest.json"))
        .filter(|path| path.symlink_metadata().is_ok())
        .collect();
    paths.sort();
    paths
}

/// `find_character_root`: the one directory whose manifest has `content_id`.
pub fn find_character_root(nikke_root: &Path, content_id: i128) -> Result<PathBuf, PyError> {
    let mut matches: Vec<PathBuf> = Vec::new();
    for manifest_path in manifest_paths(nikke_root) {
        match file_content_id(&manifest_path) {
            Ok(found) if found == Some(content_id) => matches.push(manifest_path.parent().expect("a parent").to_path_buf()),
            Ok(_) => {}
            Err(error) if is_os_error(&error.class) || error.class == "JSONDecodeError" || error.class == "TypeError" => {}
            Err(error) => return Err(error),
        }
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(system_exit(format!(
            "No NIKKE manifest found for content_id={content_id} under {}",
            posix(nikke_root)
        ))),
        _ => Err(system_exit(format!(
            "Multiple NIKKE manifests found for content_id={content_id}: {}",
            matches.iter().map(|path| posix(path)).collect::<Vec<_>>().join(", ")
        ))),
    }
}

/// `roots_from_args`: the named directories and content ids, else every page.
pub fn roots_from_args(nikke_root: &Path, character_roots: &[PathBuf], content_ids: &[i128]) -> Result<Vec<PathBuf>, PyError> {
    let mut roots: Vec<PathBuf> = character_roots
        .iter()
        .filter(|path| !path.as_os_str().is_empty())
        .cloned()
        .collect();
    for content_id in content_ids {
        roots.push(find_character_root(nikke_root, *content_id)?);
    }
    if !roots.is_empty() {
        return Ok(roots.into_iter().collect::<BTreeSet<_>>().into_iter().collect());
    }
    Ok(manifest_paths(nikke_root)
        .into_iter()
        .map(|path| path.parent().expect("a parent").to_path_buf())
        .collect())
}

/// `target_root`: `--character-root`, else the page of `--content-id` or of the capture.
fn target_root(
    nikke_root: &Path,
    character_root: Option<&Path>,
    content_id: Option<i128>,
    capture_path: Option<&Path>,
) -> Result<PathBuf, PyError> {
    if let Some(root) = character_root.filter(|root| !root.as_os_str().is_empty()) {
        return Ok(root.to_path_buf());
    }
    let content_id = match (content_id, capture_path) {
        (Some(content_id), _) => Some(content_id),
        (None, Some(path)) => file_content_id(path)?,
        (None, None) => None,
    };
    let Some(content_id) = content_id else {
        return Err(system_exit(
            "Pass --character-root, --content-id, or a capture JSON with content_id.",
        ));
    };
    find_character_root(nikke_root, content_id)
}

/// `default_backup_dir`: `<root>/_layer-backups/<UTC timestamp>`.
pub fn default_backup_dir(nikke_root: &Path) -> PathBuf {
    nikke_root
        .join("_layer-backups")
        .join(chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string())
}

fn io_error(error: &std::io::Error, path: &Path) -> PyError {
    crate::nikke_layer_metadata::os_error(error, path)
}

/// `backup_files`: copy the page's files that exist into `<backup_dir>/<page>/`.
pub fn backup_files(root: &Path, backup_dir: &Path) -> Result<Dict, PyError> {
    let target_dir = backup_dir.join(root.file_name().unwrap_or_default());
    std::fs::create_dir_all(&target_dir).map_err(|error| io_error(&error, &target_dir))?;
    let mut copied = Dict::new();
    for filename in TARGET_FILES {
        let source = root.join(filename);
        if !source.exists() {
            continue;
        }
        let destination = target_dir.join(filename);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| io_error(&error, parent))?;
        }
        crate::gamekee::copy2(&source, &destination).map_err(|error| io_error(&error, &source))?;
        copied.insert(filename, Value::Str(posix(&destination)));
    }
    Ok(copied)
}

/// `write_target_snapshot`: which of the page's files exist.
fn write_target_snapshot(root: &Path) -> Vec<(&'static str, bool)> {
    TARGET_FILES
        .iter()
        .map(|filename| (*filename, root.join(filename).exists()))
        .collect()
}

/// `restore_backup_files`: put the backed-up files back, and remove the ones the failed
/// write created.
fn restore_backup_files(root: &Path, backup: &Dict, existed: &[(&'static str, bool)]) -> Result<Dict, PyError> {
    let mut restored = Dict::new();
    for (filename, existed_before) in existed {
        let target = root.join(filename);
        let backup_path = match backup.get(filename) {
            Some(Value::Str(path)) => Some(PathBuf::from(path)),
            _ => None,
        };
        if let Some(backup_path) = backup_path.filter(|path| path.exists()) {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|error| io_error(&error, parent))?;
            }
            crate::gamekee::copy2(&backup_path, &target).map_err(|error| io_error(&error, &backup_path))?;
            restored.insert(*filename, Value::from("restored"));
        } else if !existed_before {
            match std::fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(&error, &target)),
            }
            restored.insert(*filename, Value::from("removed"));
        }
    }
    Ok(restored)
}

/// `report_errors`: the report's manifest quality issues of severity `error`.
fn report_errors(report: &Dict) -> Vec<Value> {
    let Some(Value::Dict(manifest)) = report.get("manifest") else {
        return Vec::new();
    };
    let Some(Value::List(issues)) = manifest.get("quality_issues") else {
        return Vec::new();
    };
    issues
        .iter()
        .filter(|issue| matches!(issue, Value::Dict(issue) if issue.get("severity") == Some(&Value::from("error"))))
        .cloned()
        .collect()
}

// ---------- merge, merge-dir, validate, strip ----------

/// `merge`'s arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeArgs {
    pub nikke_root: PathBuf,
    pub character_root: Option<PathBuf>,
    pub content_id: Option<i128>,
    pub capture: PathBuf,
    pub write: bool,
    pub backup_dir: Option<PathBuf>,
}

/// `command_merge`.
pub fn command_merge(args: &MergeArgs) -> Result<Outcome, PyError> {
    let capture_payload = read_json_object(&args.capture)?;
    let root = target_root(
        &args.nikke_root,
        args.character_root.as_deref(),
        args.content_id,
        Some(&args.capture),
    )?;
    let mut backup = Dict::new();
    if args.write {
        let backup_dir = args.backup_dir.clone().unwrap_or_else(|| default_backup_dir(&args.nikke_root));
        backup = backup_files(&root, &backup_dir)?;
    }
    let mut report = merge_layer_capture_files(&root, &capture_payload, !args.write)?;
    report.insert("backup", Value::Dict(backup));
    let status = u8::from(!report_errors(&report).is_empty());
    Ok(Outcome { report, status })
}

/// `merge_capture_path`: one capture of `merge-dir`; a failure is reported, not raised.
fn merge_capture_path(nikke_root: &Path, capture_path: &Path, write: bool, backup_dir: Option<&Path>) -> Dict {
    let failed = |error: String| {
        dict_of(vec![
            ("capture_path", Value::Str(posix(capture_path))),
            ("ok", Value::Bool(false)),
            ("error", Value::Str(error)),
        ])
    };
    let attempt = || -> Result<Result<(Dict, Dict), String>, PyError> {
        let capture_payload = read_json_object(capture_path)?;
        let Some(content_id) = file_content_id(capture_path)? else {
            return Ok(Err("capture JSON is missing content_id".to_owned()));
        };
        let root = find_character_root(nikke_root, content_id)?;
        let backup = match backup_dir {
            Some(backup_dir) if write => backup_files(&root, backup_dir)?,
            _ => Dict::new(),
        };
        Ok(Ok((merge_layer_capture_files(&root, &capture_payload, !write)?, backup)))
    };
    match attempt() {
        Ok(Ok((mut report, backup))) => {
            report.insert("backup", Value::Dict(backup));
            let ok = report_errors(&report).is_empty();
            dict_of(vec![
                ("capture_path", Value::Str(posix(capture_path))),
                ("ok", Value::Bool(ok)),
                ("report", Value::Dict(report)),
            ])
        }
        Ok(Err(message)) => failed(message),
        // `except (OSError, json.JSONDecodeError, TypeError, ValueError, SystemExit)`:
        // everything this code raises.
        Err(error) => failed(error.message),
    }
}

/// `merge-dir`'s arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeDirArgs {
    pub nikke_root: PathBuf,
    pub capture_dir: PathBuf,
    pub pattern: String,
    pub write: bool,
    pub backup_dir: Option<PathBuf>,
}

/// `command_merge_dir`.
pub fn command_merge_dir(args: &MergeDirArgs) -> Result<Outcome, PyError> {
    let capture_paths: Vec<PathBuf> = glob(&args.capture_dir, &args.pattern)?
        .into_iter()
        .filter(|path| path.is_file())
        .collect();
    let backup_root = args.backup_dir.clone().unwrap_or_else(|| default_backup_dir(&args.nikke_root));
    let results: Vec<Dict> = capture_paths
        .iter()
        .map(|path| merge_capture_path(&args.nikke_root, path, args.write, Some(&backup_root)))
        .collect();
    let failed = results
        .iter()
        .filter(|result| !result.get("ok").is_some_and(Value::is_truthy))
        .count();
    let report = dict_of(vec![
        ("dry_run", Value::Bool(!args.write)),
        ("capture_count", Value::Int(capture_paths.len() as i128)),
        ("failed_count", Value::Int(failed as i128)),
        ("results", Value::List(results.into_iter().map(Value::Dict).collect())),
    ]);
    Ok(Outcome {
        report,
        status: u8::from(failed > 0),
    })
}

/// `validate`'s arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidateArgs {
    pub nikke_root: PathBuf,
    pub character_root: Vec<PathBuf>,
    pub content_id: Vec<i128>,
    pub no_fail: bool,
}

fn issue(code: &str, message: String) -> Value {
    Value::Dict(dict_of(vec![
        ("severity", Value::from("error")),
        ("code", Value::from(code)),
        ("message", Value::Str(message)),
    ]))
}

/// The models of a manifest as `validate_live2d_layer_metadata` reads them: a model that
/// is not an object fails as `model.get(...)` does.
fn dict_models(models: &[Value]) -> Result<Vec<Dict>, PyError> {
    models
        .iter()
        .map(|model| match model {
            Value::Dict(model) => Ok(model.clone()),
            other => Err(PyError::new(
                "AttributeError",
                format!("'{}' object has no attribute 'get'", crate::pyerr::type_name(other)),
            )),
        })
        .collect()
}

fn title_or_name(manifest: &Dict, root: &Path) -> Value {
    match manifest.get("title") {
        Some(title) if title.is_truthy() => title.clone(),
        _ => Value::Str(root.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()),
    }
}

/// `command_validate`.
pub fn command_validate(args: &ValidateArgs) -> Result<Outcome, PyError> {
    let roots = roots_from_args(&args.nikke_root, &args.character_root, &args.content_id)?;
    let mut entries: Vec<Value> = Vec::new();
    let mut error_count: i128 = 0;
    for root in &roots {
        let (manifest, issues) = match read_json_object(&root.join("manifest.json")) {
            Ok(manifest) => {
                let issues = match manifest.get("live2d_models") {
                    Some(Value::List(models)) => validate_live2d_layer_metadata(&dict_models(models)?),
                    _ => vec![issue("invalid_manifest_models", "manifest live2d_models is not a list".to_owned())],
                };
                (manifest, issues)
            }
            Err(error) if is_os_error(&error.class) || error.class == "JSONDecodeError" || error.class == "TypeError" => (
                dict_of(vec![
                    ("content_id", Value::Null),
                    ("title", Value::Str(posix(Path::new(root.file_name().unwrap_or_default())))),
                ]),
                vec![issue("manifest_read_failed", error.message)],
            ),
            Err(error) => return Err(error),
        };
        error_count += issues
            .iter()
            .filter(|issue| matches!(issue, Value::Dict(issue) if issue.get("severity") == Some(&Value::from("error"))))
            .count() as i128;
        if !issues.is_empty() {
            entries.push(Value::Dict(dict_of(vec![
                ("root", Value::Str(posix(root))),
                ("content_id", manifest.get("content_id").cloned().unwrap_or(Value::Null)),
                ("title", title_or_name(&manifest, root)),
                ("issues", Value::List(issues)),
            ])));
        }
    }
    let report = dict_of(vec![
        ("checked", Value::Int(roots.len() as i128)),
        ("error_count", Value::Int(error_count)),
        ("entries", Value::List(entries)),
    ]);
    Ok(Outcome {
        report,
        status: u8::from(!args.no_fail && error_count != 0),
    })
}

/// `strip`'s arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct StripArgs {
    pub nikke_root: PathBuf,
    pub character_root: Option<PathBuf>,
    pub content_id: Option<i128>,
    pub write: bool,
    pub backup_dir: Option<PathBuf>,
}

/// `command_strip`.
pub fn command_strip(args: &StripArgs) -> Result<Outcome, PyError> {
    let root = target_root(&args.nikke_root, args.character_root.as_deref(), args.content_id, None)?;
    let mut backup = Dict::new();
    if args.write {
        let backup_dir = args.backup_dir.clone().unwrap_or_else(|| default_backup_dir(&args.nikke_root));
        backup = backup_files(&root, &backup_dir)?;
    }
    let mut report = strip_layer_metadata_files(&root, !args.write)?;
    report.insert("backup", Value::Dict(backup));
    Ok(Outcome { report, status: 0 })
}

// ---------- capture-missing, backfill-runtime-layers ----------

/// The arguments `capture-missing` and `backfill-runtime-layers` share.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeCaptureArgs {
    pub nikke_root: PathBuf,
    pub character_root: Vec<PathBuf>,
    pub content_id: Vec<i128>,
    pub write: bool,
    pub backup_dir: Option<PathBuf>,
    pub limit: i64,
    pub timeout_seconds: f64,
    pub headful: bool,
    pub no_fail: bool,
}

/// `manifest_models`: the manifest and its models, which must all be objects.
fn manifest_models(root: &Path) -> Result<(Dict, Vec<Dict>), PyError> {
    let manifest = read_json_object(&root.join("manifest.json"))?;
    let models = match manifest.get("live2d_models") {
        Some(Value::List(models)) if models.iter().all(|model| matches!(model, Value::Dict(_))) => dict_models(models)?,
        _ => {
            return Err(PyError::new(
                "TypeError",
                format!("{} manifest live2d_models is not a list of objects", posix(root)),
            ));
        }
    };
    Ok((manifest, models))
}

/// `projected_layer_metadata_report`: what merging the capture would leave behind.
fn projected_layer_metadata_report(content_id: i128, models: &[Dict], capture_payload: &Dict) -> Dict {
    let mut projected_models = models.to_vec();
    let merge_report = merge_live2d_layer_captures(&mut projected_models, capture_payload, Some(content_id), false);
    let validation_issues = validate_live2d_layer_metadata(&projected_models);
    let has_errors = validation_issues
        .iter()
        .any(|issue| matches!(issue, Value::Dict(issue) if issue.get("severity") == Some(&Value::from("error"))));
    let complete = !merge_report.get("skipped").is_some_and(Value::is_truthy)
        && !merge_report.get("quality_issues").is_some_and(Value::is_truthy)
        && !has_errors;
    dict_of(vec![
        ("merge_report", Value::Dict(merge_report)),
        ("validation_issues", Value::List(validation_issues)),
        ("complete", Value::Bool(complete)),
    ])
}

fn merged(result: &Dict, entries: Vec<(&str, Value)>) -> Dict {
    let mut out = result.clone();
    for (key, value) in entries {
        out.insert(key, value);
    }
    out
}

fn failure(result: &Dict, error: &PyError, extra: Vec<(&str, Value)>) -> Dict {
    let mut entries = vec![
        ("ok", Value::Bool(false)),
        ("action", Value::from("failed")),
        ("error_class", Value::Str(error.class.clone())),
        ("error", Value::Str(error.message.clone())),
    ];
    entries.extend(extra);
    merged(result, entries)
}

/// The runtime capture commands, with the browser capture and the file merge injected.
pub struct LayerTool {
    pub capturer: Arc<dyn RuntimeCapturer>,
    pub merge_files: MergeFilesFn,
}

impl std::fmt::Debug for LayerTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayerTool").finish_non_exhaustive()
    }
}

/// One page of a runtime capture command.
struct RootJob<'a> {
    root: &'a Path,
    write: bool,
    backup_dir: Option<&'a Path>,
    timeout_seconds: f64,
    headless: bool,
    force_refresh: bool,
}

impl LayerTool {
    pub fn new(capturer: Arc<dyn RuntimeCapturer>) -> Self {
        Self {
            capturer,
            merge_files: merge_layer_capture_files,
        }
    }

    /// Back the page up, run `write`, and restore the backup when it fails.
    fn write_with_backup(
        result: &mut Dict,
        job: &RootJob<'_>,
        write: impl FnOnce() -> Result<Dict, PyError>,
    ) -> Result<Result<Dict, Dict>, PyError> {
        let backup = match job.backup_dir {
            Some(backup_dir) => backup_files(job.root, backup_dir)?,
            None => Dict::new(),
        };
        result.insert("backup", Value::Dict(backup.clone()));
        let existed = write_target_snapshot(job.root);
        match write() {
            Ok(report) => Ok(Ok(report)),
            Err(error) => {
                let restored = restore_backup_files(job.root, &backup, &existed)?;
                Ok(Err(failure(result, &error, vec![("restored", Value::Dict(restored))])))
            }
        }
    }

    /// `process_runtime_capture_root`.
    async fn process_root(&self, job: &RootJob<'_>) -> Dict {
        let mut result = dict_of(vec![
            ("root", Value::Str(posix(job.root))),
            ("ok", Value::Bool(true)),
            ("dry_run", Value::Bool(!job.write)),
        ]);
        match self.process_root_inner(job, &mut result).await {
            Ok(done) => done,
            // `except Exception`.
            Err(error) => failure(&result, &error, Vec::new()),
        }
    }

    #[allow(clippy::too_many_lines)] // One return per action of `process_runtime_capture_root`, in its order.
    async fn process_root_inner(&self, job: &RootJob<'_>, result: &mut Dict) -> Result<Dict, PyError> {
        let root = job.root;
        let (manifest, models) = manifest_models(root)?;
        let Some(content_id) = file_content_id(&root.join("manifest.json"))? else {
            return Ok(merged(
                result,
                vec![
                    ("ok", Value::Bool(false)),
                    ("action", Value::from("failed")),
                    ("error", Value::from("manifest is missing content_id")),
                ],
            ));
        };
        result.insert("content_id", Value::Int(content_id));
        let title = title_or_name(&manifest, root);
        result.insert("title", title.clone());
        if !has_multi_full_layer_groups(&models) {
            return Ok(merged(
                result,
                vec![("action", Value::from("skipped")), ("reason", Value::from("no_multi_full_groups"))],
            ));
        }

        let previous_artifact = match read_previous_layer_capture_artifact(root) {
            Ok(artifact) => artifact,
            Err(error) => {
                result.insert("previous_capture_error", Value::Str(error.message));
                None
            }
        };
        let source = previous_artifact.as_ref().map_or("", |artifact| artifact.source);
        let previous_capture = previous_artifact.map(|artifact| artifact.payload);
        let reuse_decision = evaluate_layer_capture_reuse(content_id, &models, previous_capture.as_ref(), job.force_refresh);
        result.insert("reuse_decision", Value::Dict(reuse_decision.clone()));
        if reuse_decision.get("reusable").is_some_and(Value::is_truthy)
            && let Some(previous_capture) = &previous_capture
        {
            let projected = projected_layer_metadata_report(content_id, &models, previous_capture);
            let complete = projected.get("complete") == Some(&Value::Bool(true));
            result.insert("projected_report", Value::Dict(projected));
            if !complete {
                if !job.write {
                    return Ok(merged(
                        result,
                        vec![
                            ("action", Value::from("would_capture")),
                            ("reason", Value::from("incomplete_after_previous_capture_reuse")),
                        ],
                    ));
                }
            } else if !job.write {
                return Ok(merged(
                    result,
                    vec![("action", Value::from("would_reuse")), ("source", Value::from(source))],
                ));
            } else {
                return Ok(
                    match Self::write_with_backup(result, job, || (self.merge_files)(root, previous_capture, false))? {
                        Ok(merge_report) => {
                            let ok = report_errors(&merge_report).is_empty();
                            merged(
                                result,
                                vec![
                                    ("action", Value::from("reused")),
                                    ("source", Value::from(source)),
                                    ("merge_report", Value::Dict(merge_report)),
                                    ("ok", Value::Bool(ok)),
                                ],
                            )
                        }
                        Err(failed) => failed,
                    },
                );
            }
        }

        if !job.write {
            let reason = match reuse_decision.get("reason") {
                Some(reason) if reason.is_truthy() => reason.clone(),
                _ => Value::from("refresh_required"),
            };
            return Ok(merged(result, vec![("action", Value::from("would_capture")), ("reason", reason)]));
        }

        #[allow(clippy::cast_possible_truncation)] // reason: `int(timeout_seconds * 1000)` truncates, as here
        let timeout_ms = (job.timeout_seconds * 1000.0) as i64;
        let mut request = RuntimeCaptureRequest::new(content_id, models.clone(), fav_runtime::pystr::str_value(&title), timeout_ms);
        request.headless = job.headless;
        let capture_payload = self.capturer.capture(request).await?;
        let raw_path = root.join(LAYER_CAPTURE_RAW_PATH);
        let written = Self::write_with_backup(result, job, || {
            write_json(&raw_path, &Value::Dict(capture_payload.clone())).map_err(|error| io_error(&error, &raw_path))?;
            (self.merge_files)(root, &capture_payload, false)
        })?;
        Ok(match written {
            Ok(merge_report) => {
                let ok = report_errors(&merge_report).is_empty();
                merged(
                    result,
                    vec![
                        ("action", Value::from("captured")),
                        ("raw_capture_path", Value::Str(posix(&raw_path))),
                        ("merge_report", Value::Dict(merge_report)),
                        ("ok", Value::Bool(ok)),
                    ],
                )
            }
            Err(failed) => failed,
        })
    }

    /// `runtime_capture_candidates`: pages with stacked full models, and pages whose
    /// manifest cannot be read (reported as failures).
    fn candidates(args: &RuntimeCaptureArgs) -> Result<Vec<PathBuf>, PyError> {
        let roots = roots_from_args(&args.nikke_root, &args.character_root, &args.content_id)?;
        let mut candidates = Vec::new();
        for root in roots {
            match manifest_models(&root) {
                Ok((_, models)) => {
                    if has_multi_full_layer_groups(&models) {
                        candidates.push(root);
                    }
                }
                Err(error) if is_os_error(&error.class) || error.class == "JSONDecodeError" || error.class == "TypeError" => {
                    candidates.push(root);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(candidates)
    }

    /// `run_runtime_capture_command`.
    pub async fn run_runtime_capture_command(&self, args: &RuntimeCaptureArgs, force_refresh: bool) -> Result<Dict, PyError> {
        let roots = Self::candidates(args)?;
        let limit = usize::try_from(args.limit.max(0)).unwrap_or(usize::MAX);
        let backup_dir = args
            .write
            .then(|| args.backup_dir.clone().unwrap_or_else(|| default_backup_dir(&args.nikke_root)));
        let mut processed: Vec<Dict> = Vec::new();
        for root in &roots {
            let counted = processed
                .iter()
                .filter(|item| {
                    matches!(item.get("action"), Some(Value::Str(action))
                        if ["captured", "reused", "would_capture", "would_reuse"].contains(&action.as_str()))
                })
                .count();
            if limit > 0 && counted >= limit {
                break;
            }
            let job = RootJob {
                root,
                write: args.write,
                backup_dir: backup_dir.as_deref(),
                timeout_seconds: args.timeout_seconds,
                headless: !args.headful,
                force_refresh,
            };
            processed.push(self.process_root(&job).await);
        }
        let failed = processed
            .iter()
            .filter(|item| !item.get("ok").is_some_and(Value::is_truthy))
            .count();
        Ok(dict_of(vec![
            ("dry_run", Value::Bool(!args.write)),
            ("checked", Value::Int(roots.len() as i128)),
            ("processed", Value::Int(processed.len() as i128)),
            ("failed_count", Value::Int(failed as i128)),
            ("results", Value::List(processed.into_iter().map(Value::Dict).collect())),
        ]))
    }

    /// `command_runtime_capture`.
    pub async fn command_runtime_capture(&self, args: &RuntimeCaptureArgs, force_refresh: bool) -> Result<Outcome, PyError> {
        let report = self.run_runtime_capture_command(args, force_refresh).await?;
        let failed = report.get("failed_count") != Some(&Value::Int(0));
        Ok(Outcome {
            report,
            status: u8::from(!args.no_fail && failed),
        })
    }
}

// ---------- pathlib's glob ----------

/// `fnmatch.translate` for one path segment: `*`, `?` and `[...]` never match `/`.
fn segment_regex(pattern: &str) -> Result<Regex, PyError> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::from("^");
    let mut index = 0;
    while index < chars.len() {
        let current = chars[index];
        index += 1;
        match current {
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            '[' => {
                let mut end = index;
                if end < chars.len() && chars[end] == '!' {
                    end += 1;
                }
                if end < chars.len() && chars[end] == ']' {
                    end += 1;
                }
                while end < chars.len() && chars[end] != ']' {
                    end += 1;
                }
                if end >= chars.len() {
                    out.push_str("\\[");
                } else {
                    let mut class: String = chars[index..end].iter().collect();
                    index = end + 1;
                    if let Some(rest) = class.strip_prefix('!') {
                        class = format!("^{rest}");
                    } else if class.starts_with('^') {
                        class = format!("\\{class}");
                    }
                    out.push('[');
                    out.push_str(&class.replace('\\', "\\\\"));
                    out.push(']');
                }
            }
            other => out.push_str(&regex::escape(&other.to_string())),
        }
    }
    out.push('$');
    Regex::new(&out).map_err(|error| PyError::new("ValueError", error.to_string()))
}

fn children(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|entry| entry.path()).collect();
    paths.sort();
    paths
}

fn walk_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    out.push(dir.to_path_buf());
    for child in children(dir) {
        if child.is_dir() && !child.is_symlink() {
            walk_dirs(&child, out);
        }
    }
}

/// `sorted(base.glob(pattern))` for relative patterns of `*`, `?`, `[...]` and `**`
/// segments.
pub fn glob(base: &Path, pattern: &str) -> Result<Vec<PathBuf>, PyError> {
    if pattern.is_empty() || pattern.starts_with('/') {
        return Err(PyError::new(
            "ValueError",
            format!("Unacceptable pattern: {}", fav_runtime::pystr::repr_str(pattern)),
        ));
    }
    let mut current: Vec<PathBuf> = vec![base.to_path_buf()];
    let segments: Vec<&str> = pattern
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect();
    for (position, segment) in segments.iter().enumerate() {
        let last = position + 1 == segments.len();
        let mut next: Vec<PathBuf> = Vec::new();
        if *segment == "**" {
            for dir in &current {
                walk_dirs(dir, &mut next);
            }
        } else {
            let regex = segment_regex(segment)?;
            for dir in &current {
                for child in children(dir) {
                    let name = child
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if regex.is_match(&name) && (last || child.is_dir()) {
                        next.push(child);
                    }
                }
            }
        }
        current = next;
    }
    let unique: BTreeSet<PathBuf> = current.into_iter().collect();
    Ok(unique.into_iter().collect())
}

#[cfg(test)]
mod tests;
