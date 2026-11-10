//! Replays `rust/goldens/bd2/parsing.json` (every pure function of the crawler and the
//! viewer tool, errors included as `{"error": "Class: message"}`) and
//! `rust/goldens/bd2/crawl.json` (two `update()` runs over a scripted GameKee).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{TimeZone as _, Utc};
use fav_core::golden;
use fav_core::pyjson::{Dict, Value};
use fav_net::retry::RecordingSleeper;
use fav_net::{ClientOptions, HttpClient};
use fav_runtime::source::JobError;
use sha2::{Digest as _, Sha256};
use sqlx::Row as _;

use super::extract;
use crate::PyError;
use crate::bd2_l2d_viewer::{self, Category, GameKeeResource, ViewerResource};
use crate::gamekee::{self, Asset, Assets};
use crate::testkit::RecordingNotifier;
use crate::testutil::{FakeWeb, Reply};

fn error_value(class: &str, message: &str) -> Value {
    let mut dict = Dict::new();
    dict.insert("error", Value::Str(format!("{class}: {message}")));
    Value::Dict(dict)
}

fn job<T>(value: Result<T, JobError>, encode: impl FnOnce(T) -> Value) -> Value {
    match value {
        Ok(value) => encode(value),
        Err(error) => error_value(error.class_name(), error.message()),
    }
}

fn py<T>(value: Result<T, PyError>, encode: impl FnOnce(T) -> Value) -> Value {
    match value {
        Ok(value) => encode(value),
        Err(error) => error_value(&error.class, &error.message),
    }
}

fn arg_str(args: &[Value], index: usize) -> String {
    golden::string(&args[index])
}

fn arg_dict(args: &[Value], index: usize) -> Dict {
    match &args[index] {
        Value::Dict(dict) => dict.clone(),
        other => panic!("expected an object, got {other:?}"),
    }
}

fn arg_opt_dict(args: &[Value], index: usize) -> Option<Dict> {
    match &args[index] {
        Value::Null => None,
        _ => Some(arg_dict(args, index)),
    }
}

fn arg_dicts(args: &[Value], index: usize) -> Vec<Dict> {
    golden::list(&args[index])
        .iter()
        .map(|value| value.as_dict().expect("object").clone())
        .collect()
}

fn strings(values: impl IntoIterator<Item = String>) -> Value {
    Value::List(values.into_iter().map(Value::Str).collect())
}

fn text_list(value: &Value) -> Vec<String> {
    golden::list(value).iter().map(golden::string).collect()
}

/// `Asset(**item)`: the fields given, the rest defaulted.
fn asset_from(value: &Value) -> Asset {
    let dict = value.as_dict().expect("object");
    let field = |key: &str| dict.get(key).unwrap_or(&Value::Null);
    let text = |key: &str| match field(key) {
        Value::Null => None,
        value => Some(golden::string(value)),
    };
    let mut asset = Asset::new(text("url").unwrap_or_default(), text("kind").unwrap_or_default());
    if let Some(local_path) = text("local_path") {
        asset.local_path = local_path;
    }
    if let Some(content_type) = text("content_type") {
        asset.content_type = content_type;
    }
    if let Value::Int(size) = field("size") {
        asset.size = i64::try_from(*size).expect("size");
    }
    if let Some(sha256) = text("sha256") {
        asset.sha256 = sha256;
    }
    if let Some(status) = text("status") {
        asset.status = status;
    }
    if let Some(error) = text("error") {
        asset.error = error;
    }
    if let Value::List(contexts) = field("contexts") {
        asset.contexts = contexts.iter().map(|context| context.as_dict().expect("object").clone()).collect();
    }
    asset
}

fn assets_from(value: &Value) -> Assets {
    let mut assets = Assets::new();
    for item in golden::list(value) {
        assets.push(asset_from(item));
    }
    assets
}

fn viewer_resource(value: &Value) -> ViewerResource {
    ViewerResource {
        entry_id: golden::string(golden::get(value, "entry_id")),
        category: Category::parse(&golden::string(golden::get(value, "category"))).expect("category"),
        stem: golden::string(golden::get(value, "stem")),
        char_name: golden::string(golden::get(value, "char_name")),
        costume_name: golden::string(golden::get(value, "costume_name")),
        files: text_list(golden::get(value, "files")),
        missing_core_files: text_list(golden::get(value, "missing_core_files")),
    }
}

fn viewer_resources(value: &Value) -> Vec<ViewerResource> {
    golden::list(value).iter().map(viewer_resource).collect()
}

fn gamekee_resource(value: &Value) -> GameKeeResource {
    GameKeeResource {
        content_id: golden::int(golden::get(value, "content_id")),
        title: golden::string(golden::get(value, "title")),
        category: Category::parse(&golden::string(golden::get(value, "category"))).expect("category"),
        stem: golden::string(golden::get(value, "stem")),
        model_key: golden::string(golden::get(value, "model_key")),
        urls: text_list(golden::get(value, "urls")),
        source_path: golden::string(golden::get(value, "source_path")),
    }
}

fn resources_value(resources: &[ViewerResource]) -> Value {
    Value::List(resources.iter().map(ViewerResource::to_value).collect())
}

fn models_value(models: Vec<Dict>) -> Value {
    Value::List(models.into_iter().map(Value::Dict).collect())
}

fn counted(count: usize, assets: &Assets, models: Vec<Dict>) -> Value {
    let mut dict = Dict::new();
    dict.insert("count", Value::Int(i128::try_from(count).expect("count")));
    dict.insert("assets", assets.to_value());
    dict.insert("models", models_value(models));
    Value::Dict(dict)
}

fn headers_value(headers: Vec<(String, String)>) -> Value {
    let mut dict = Dict::new();
    for (name, value) in headers {
        dict.insert(name, Value::Str(value));
    }
    Value::Dict(dict)
}

fn float_value(value: Option<f64>) -> Value {
    value.map_or(Value::Null, Value::Float)
}

/// The Rust port's answer for one case, or `None` for a function it does not know.
#[allow(clippy::too_many_lines)] // One arm per baseline function the golden replays.
fn call(name: &str, args: &[Value]) -> Option<Value> {
    Some(match name {
        // ---------- the viewer tool ----------
        "parse_viewer_character_list" => py(bd2_l2d_viewer::parse_viewer_character_list(&arg_str(args, 0)), |entries| {
            let mut dict = Dict::new();
            for (key, value) in entries {
                dict.insert(key, Value::Dict(value));
            }
            Value::Dict(dict)
        }),
        "viewer_asset_paths_from_tree_payload" => py(bd2_l2d_viewer::viewer_asset_paths_from_tree_payload(&arg_dict(args, 0)), strings),
        "viewer_resources_from_character_list" => {
            let characters: Vec<(String, Dict)> = arg_dict(args, 0)
                .iter()
                .map(|(key, value)| (fav_runtime::pystr::str_key(key), value.as_dict().expect("object").clone()))
                .collect();
            let paths: Option<BTreeSet<String>> = match &args[1] {
                Value::Null => None,
                value => Some(text_list(value).into_iter().collect()),
            };
            resources_value(&bd2_l2d_viewer::viewer_resources_from_character_list(&characters, paths.as_ref()))
        }
        "viewer_asset_url" => Value::Str(bd2_l2d_viewer::viewer_asset_url(&arg_str(args, 0))),
        "resource_stem_from_url" => py(bd2_l2d_viewer::resource_stem_from_url(&arg_str(args, 0)), Value::Str),
        "category_from_stem" => Value::Str(bd2_l2d_viewer::category_from_stem(&arg_str(args, 0)).as_str().to_owned()),
        "normalize_resource_url" => py(bd2_l2d_viewer::normalize_resource_url(&arg_str(args, 0)), Value::Str),
        "gamekee_resources_from_manifest" => py(
            bd2_l2d_viewer::gamekee_resources_from_manifest(&arg_dict(args, 0), "m.json"),
            |resources| Value::List(resources.iter().map(GameKeeResource::to_value).collect()),
        ),
        "gamekee_resources_from_detail_payload" => py(
            bd2_l2d_viewer::gamekee_resources_from_detail_payload(&arg_dict(args, 0), arg_opt_dict(args, 1).as_ref()),
            |resources| Value::List(resources.iter().map(GameKeeResource::to_value).collect()),
        ),
        "compare_resources" => {
            let gamekee: Vec<GameKeeResource> = golden::list(&args[0]).iter().map(gamekee_resource).collect();
            bd2_l2d_viewer::compare_resources(&gamekee, &viewer_resources(&args[1])).to_value()
        }
        // ---------- the crawler ----------
        "parse_content_id" => job(extract::parse_content_id(&arg_str(args, 0)), Value::Int),
        "request_headers" => headers_value(extract::request_headers(match &args[0] {
            Value::Null => None,
            value => Some(golden::int(value)),
        })),
        "cdn_request_headers" => headers_value(extract::cdn_request_headers()),
        "asset_headers" => headers_value(gamekee::asset_headers()),
        "normalize_url" => Value::Str(gamekee::normalize_url(&arg_str(args, 0))),
        "_to_int" => extract::to_int(&args[0]).map_or(Value::Null, Value::Int),
        "content_json_from_cdn_payload" => Value::Dict(extract::content_json_from_cdn_payload(&arg_dict(args, 0))),
        "parse_content_json" => Value::Dict(extract::parse_content_json(&arg_dict(args, 0))),
        "cdn_json_url" => Value::Str(extract::cdn_json_url(&arg_dict(args, 0), &arg_str(args, 1))),
        "reverse_bind_from_cdn_payload" => Value::Dict(extract::reverse_bind_from_cdn_payload(&arg_dict(args, 0))),
        "reverse_bind_map" => Value::Dict(extract::reverse_bind_map(&arg_dict(args, 0))),
        "summarize_content" => Value::Dict(extract::summarize_content(&arg_dict(args, 0), &arg_dict(args, 1))),
        "base_info_from_summary" => Value::Dict(extract::base_info_from_summary(&arg_dict(args, 0))),
        "filter_bd2_character_rows" => models_value(extract::filter_bd2_character_rows(&args[0])),
        "detail_name" => Value::Str(extract::detail_name(
            &arg_dict(args, 0),
            golden::int(&args[1]),
            arg_opt_dict(args, 2).as_ref(),
        )),
        "extract_resources" => {
            let (assets, models) = extract::extract_resources(&arg_dict(args, 0), arg_opt_dict(args, 1).as_ref(), &arg_dict(args, 2));
            let mut dict = Dict::new();
            dict.insert("assets", assets.to_value());
            dict.insert("models", models_value(models));
            Value::Dict(dict)
        }
        "prefer_live2d_model_assets_from_viewer" => {
            let mut assets = assets_from(&args[0]);
            let mut models = arg_dicts(args, 1);
            job(
                extract::prefer_live2d_model_assets_from_viewer(&mut assets, &mut models, &viewer_resources(&args[2])),
                |count| counted(count, &assets, models.clone()),
            )
        }
        "supplement_live2d_models_from_viewer" => {
            let mut assets = assets_from(&args[0]);
            let mut models = arg_dicts(args, 1);
            let content_json = arg_opt_dict(args, 3);
            let reverse_bind = arg_opt_dict(args, 4);
            job(
                extract::supplement_live2d_models_from_viewer(
                    &mut assets,
                    &mut models,
                    &viewer_resources(&args[2]),
                    content_json.as_ref(),
                    reverse_bind.as_ref(),
                ),
                |count| counted(count, &assets, models.clone()),
            )
        }
        "viewer_extra_resources" => resources_value(&extract::viewer_extra_resources(&viewer_resources(&args[0]))),
        "viewer_extra_title" => Value::Str(extract::viewer_extra_title(&viewer_resource(&args[0]))),
        "viewer_extra_root" => Value::Str(extract::viewer_extra_root(&arg_str(args, 0), &viewer_resource(&args[1]))),
        "viewer_extra_live2d_model" => extract::viewer_extra_live2d_model(&viewer_resource(&args[0])).map_or(Value::Null, Value::Dict),
        "extension_for_kind" => Value::Str(extract::extension_for_kind(&arg_str(args, 0)).to_owned()),
        "original_filename_from_url" => py(
            gamekee::original_filename_from_url(&arg_str(args, 0), &arg_str(args, 1)),
            Value::Str,
        ),
        "filename_from_url" => job(
            extract::filename_from_url(&arg_str(args, 0), &arg_str(args, 1), &arg_dicts(args, 2), golden::int(&args[3])),
            Value::Str,
        ),
        "assign_asset_paths" => {
            let mut assets = assets_from(&args[0]);
            job(extract::assign_asset_paths(&mut assets, golden::int(&args[1])), |()| {
                assets.to_value()
            })
        }
        "asset_counts" => Value::Dict(extract::asset_counts(&assets_from(&args[0]))),
        "output_root" => Value::Str(extract::output_root(
            &arg_str(args, 0),
            &arg_dict(args, 1),
            golden::int(&args[2]),
            arg_opt_dict(args, 3).as_ref(),
        )),
        "live2d_atlas_texture_context" => Value::Dict(extract::live2d_atlas_texture_context(&arg_dict(args, 0))),
        "context_hash" => job(gamekee::context_hash(&arg_dict(args, 0)), Value::Str),
        "content_hash" => job(
            extract::content_hash(&arg_dict(args, 0), &arg_dict(args, 1), arg_opt_dict(args, 2).as_ref()),
            Value::Str,
        ),
        "retry_after_delay_seconds" => {
            let now = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).single().expect("time");
            let value = match &args[0] {
                Value::Null => None,
                value => Some(golden::string(value)),
            };
            float_value(extract::retry_after_delay_seconds(value.as_deref(), now))
        }
        "max_attempts_for_status" => Value::Int(i128::from(extract::max_attempts_for_status(
            u16::try_from(golden::int(&args[0])).expect("status"),
        ))),
        "video_retry_cooldown_days" => Value::Int(i128::from(extract::video_retry_cooldown_days(
            i64::try_from(golden::int(&args[0])).expect("count"),
        ))),
        "is_nonblocking_failed_asset" => {
            let mut asset = Asset::new(arg_str(args, 0), arg_str(args, 1));
            asset.error = arg_str(args, 2);
            Value::Bool(extract::is_nonblocking_failed_asset(&asset))
        }
        "validate_asset_response" => {
            // The generator encoded the prefix as Latin-1.
            let prefix: Vec<u8> = arg_str(args, 2)
                .chars()
                .map(|ch| u8::try_from(u32::from(ch)).expect("latin-1"))
                .collect();
            let size = i64::try_from(golden::int(&args[3])).expect("size");
            Value::Str(gamekee::validate_asset_response(&arg_str(args, 0), &arg_str(args, 1), &prefix, size).to_owned())
        }
        "parse_spine_atlas_textures" => strings(gamekee::parse_spine_atlas_textures(&arg_str(args, 0))),
        _ => return None,
    })
}

/// Floats compare by value; everything else exactly.
fn same(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Float(a), Value::Float(b)) => (a - b).abs() < 1e-9,
        (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => {
            #[allow(clippy::cast_precision_loss)] // reason: small test values
            let a = *a as f64;
            (a - b).abs() < 1e-9
        }
        _ => expected == actual,
    }
}

#[test]
fn parsing_matches_the_baseline() {
    let document = golden::load("bd2/parsing.json");
    let mut unknown = BTreeSet::new();
    let mut mismatches = Vec::new();
    for case in golden::cases(&document) {
        let name = golden::string(golden::get(case, "fn"));
        let args = golden::list(golden::get(case, "args"));
        let mut expected = golden::get(case, "result");
        // D97: Python's `_to_int` raises for a digit `int()` refuses; Rust reads it as no
        // number. The golden keeps Python's answer, so the divergence is stated here.
        let superscript = name == "_to_int" && matches!(args, [Value::Str(text)] if text == "¹");
        if superscript {
            assert_eq!(
                golden::get(expected, "error"),
                &Value::from("ValueError: invalid literal for int() with base 10: '¹'"),
                "the golden case D97 departs from has changed"
            );
            expected = &Value::Null;
        }
        match call(&name, args) {
            None => {
                unknown.insert(name);
            }
            Some(actual) if !same(expected, &actual) => {
                mismatches.push(format!("{name}{args:?}\n  expected {expected:?}\n  actual   {actual:?}"));
            }
            Some(_) => {}
        }
    }
    assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n"));
    assert!(unknown.is_empty(), "functions without a port: {unknown:?}");
}

// ---------- crawl ----------

/// One answer of the scripted web.
fn reply(answer: &Value) -> Reply {
    let status = u16::try_from(golden::int(golden::get(answer, "status"))).expect("status");
    let headers = match golden::get(answer, "headers") {
        Value::Dict(headers) => headers
            .iter()
            .map(|(name, value)| (fav_runtime::pystr::str_key(name), golden::string(value)))
            .collect(),
        _ => Vec::new(),
    };
    Reply::Respond {
        status,
        headers,
        body: golden::string(golden::get(answer, "body")).into_bytes(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// `_tree_files(root)`: JSON outside the assets and blobs as text, the rest by size and hash.
fn tree_files(root: &std::path::Path) -> Dict {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read dir").filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    let mut paths = Vec::new();
    walk(root, &mut paths);
    let mut relative: Vec<(String, std::path::PathBuf)> = paths
        .into_iter()
        .map(|path| (path.strip_prefix(root).expect("under root").to_string_lossy().into_owned(), path))
        .collect();
    // `sorted(root.rglob('*'))` orders paths part by part.
    relative.sort_by(|a, b| a.0.split('/').cmp(b.0.split('/')));
    let mut files = Dict::new();
    for (name, path) in relative {
        let data = std::fs::read(&path).expect("read");
        let mut entry = Dict::new();
        let is_json = std::path::Path::new(&name).extension().is_some_and(|extension| extension == "json");
        if is_json && !format!("/{name}").contains("/assets/") && !name.starts_with("_blobs/") {
            entry.insert("text", Value::Str(String::from_utf8(data).expect("utf-8")));
        } else {
            entry.insert("size", Value::Int(i128::try_from(data.len()).expect("size")));
            entry.insert("sha256", Value::Str(sha256_hex(&data)));
        }
        files.insert(name, Value::Dict(entry));
    }
    files
}

#[derive(Debug, Default)]
struct Served {
    counts: std::collections::HashMap<String, usize>,
    requests: Vec<(String, String, String, String)>,
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Two runs, each checked on every recorded side.
async fn crawl_matches_the_baseline() {
    let document = golden::load("bd2/crawl.json");
    let case = &golden::cases(&document)[0];
    let viewer = viewer_resources(golden::get(case, "viewer_resources"));
    let database = crate::testutil::database().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("bd2");
    let root_text = root.to_string_lossy().into_owned();
    let runs = golden::list(golden::get(case, "runs"));

    // The discovery baseline the generator seeded, byte for byte.
    let baseline_name = "1001 - Ingrid Old/character.json";
    let baseline = golden::string(golden::get(golden::get(golden::get(&runs[0], "files"), baseline_name), "text"));
    std::fs::create_dir_all(root.join("1001 - Ingrid Old")).expect("mkdir");
    std::fs::write(root.join(baseline_name), baseline).expect("write");

    for (index, run) in runs.iter().enumerate() {
        let web = golden::get(run, "web").as_dict().expect("web").clone();
        let served = Arc::new(Mutex::new(Served::default()));
        let recorder = Arc::clone(&served);
        let fake = FakeWeb::start(move |seen| {
            let url = seen
                .url
                .strip_prefix("http://")
                .map_or_else(|| seen.url.clone(), |rest| format!("https://{rest}"));
            let mut served = recorder.lock().expect("lock");
            served.requests.push((
                url.clone(),
                seen.method.clone(),
                seen.header("referer").unwrap_or_default().to_owned(),
                seen.header("user-agent").unwrap_or_default().to_owned(),
            ));
            let count = served.counts.entry(url.clone()).or_insert(0);
            let served_index = *count;
            *count += 1;
            match web.get(&url) {
                Some(Value::List(answers)) => reply(&answers[served_index.min(answers.len() - 1)]),
                _ => Reply::status(404),
            }
        })
        .await;
        let http = HttpClient::new(&ClientOptions {
            proxy: Some(fake.proxy.clone()),
            timeout: Duration::from_secs(10),
            ..super::client_options()
        })
        .expect("client");
        let notifier = Arc::new(RecordingNotifier::default());
        let sleeper = RecordingSleeper::new();
        let crawler = super::Bd2Crawler::new(&root_text, http.clone(), Ok(http), database.db().clone(), notifier.clone())
            .with_sleeper(sleeper.sleeper())
            .with_jitter(Arc::new(|| 0))
            .with_clock(Arc::new(|| Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).single().expect("time")))
            .with_viewer_resources(viewer.clone())
            .with_plain_http();
        let error = match crawler.update().await {
            Ok(()) => String::new(),
            Err(error) => format!("{}: {}", error.class_name(), error.message()),
        };
        assert_eq!(error, golden::string(golden::get(run, "error")), "run {index}: error");

        let mut requests = served.lock().expect("lock").requests.clone();
        requests.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        let requests: Vec<Value> = requests
            .into_iter()
            .map(|(url, method, referer, user_agent)| {
                let mut dict = Dict::new();
                dict.insert("method", Value::Str(method));
                dict.insert("url", Value::Str(url));
                dict.insert("referer", Value::Str(referer));
                dict.insert("user_agent", Value::Str(user_agent));
                Value::Dict(dict)
            })
            .collect();
        let expected_requests = golden::list(golden::get(run, "requests"));
        assert_eq!(requests.as_slice(), expected_requests, "run {index}: requests");

        let files = tree_files(&root);
        let expected_files = golden::get(run, "files").as_dict().expect("files");
        let names = |dict: &Dict| dict.iter().map(|(key, _)| fav_runtime::pystr::str_key(key)).collect::<Vec<_>>();
        assert_eq!(names(&files), names(expected_files), "run {index}: file names");
        for (name, expected) in expected_files.iter() {
            let name = fav_runtime::pystr::str_key(name);
            assert_eq!(files.get(&name), Some(expected), "run {index}: {name}");
        }

        let notifications: Vec<Value> = notifier
            .sent()
            .into_iter()
            .map(|sent| {
                let mut dict = Dict::new();
                dict.insert("kind", Value::Str(sent.kind));
                dict.insert("source", Value::Str(sent.source));
                dict.insert("header", Value::Str(sent.header));
                dict.insert("title", Value::Str(sent.title));
                dict.insert("body", Value::Str(sent.body));
                dict.insert("link_url", Value::Str(sent.link_url));
                dict.insert("payload", sent.payload.map_or(Value::Null, Value::Dict));
                Value::Dict(dict)
            })
            .collect();
        assert_eq!(
            notifications.as_slice(),
            golden::list(golden::get(run, "notifications")),
            "run {index}: notifications"
        );

        let rows = sqlx::query("SELECT url, status, sha256, size, failed_count, last_error FROM bd2_assets ORDER BY url")
            .fetch_all(database.db().pool())
            .await
            .expect("assets");
        let mut assets = Dict::new();
        for row in &rows {
            let mut dict = Dict::new();
            dict.insert("status", Value::Str(row.get("status")));
            dict.insert("sha256", Value::Str(row.get("sha256")));
            dict.insert("size", Value::Int(i128::from(row.get::<i64, _>("size"))));
            dict.insert("failed_count", Value::Int(i128::from(row.get::<i32, _>("failed_count"))));
            dict.insert("last_error", Value::Str(row.get("last_error")));
            assets.insert(row.get::<String, _>("url"), Value::Dict(dict));
        }
        assert_eq!(&Value::Dict(assets), golden::get(run, "assets"), "run {index}: assets");

        // The generator's fake database has no retry cooldown: clear it so the next run
        // retries the failed video as the baseline did.
        sqlx::query("UPDATE bd2_assets SET next_retry_at = NULL")
            .execute(database.db().pool())
            .await
            .expect("clear cooldown");
    }
}
