//! Replays `rust/goldens/azurlane/crawl.json`: two `update()` runs over a scripted CDN.
//!
//! The generator ran Python's crawler over the same script and a fake of the tables; here
//! the crawler runs over the fake web and a real database, and every run is checked on its
//! error, requests, retry sleeps, files and rows, normalised the way the generator
//! normalised them (timestamps reduced to set or not, the temporary root as `<root>`).
//! JSONB hands objects back in its own key order, so dictionaries compare with their keys
//! sorted.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{TimeZone as _, Utc};
use fav_core::golden;
use fav_core::pyjson::{self, Dict, Value};
use fav_net::retry::RecordingSleeper;
use fav_net::{ClientOptions, HttpClient};
use fav_runtime::pystr::str_key;
use sha2::{Digest as _, Sha256};
use sqlx::Row as _;
use sqlx::postgres::PgRow;

use super::{AzurLaneCrawler, Clients, asset_client_options};
use crate::azurlane::l2d_sources::source_client_options;
use crate::testutil::{FakeWeb, Reply};

/// The keys `_normalize` drops at any depth.
const DROPPED_TIMESTAMPS: [&str; 6] = [
    "fetched_at",
    "last_attempt_at",
    "last_seen_at",
    "first_seen_at",
    "created_at",
    "updated_at",
];

/// `_normalize(value, root)`.
fn normalize(value: &Value, root: &str, key: &str) -> Value {
    match value {
        Value::Dict(dict) => Value::Dict(
            dict.iter()
                .map(|(item_key, item)| (str_key(item_key), item))
                .filter(|(item_key, _)| !DROPPED_TIMESTAMPS.contains(&item_key.as_str()))
                .map(|(item_key, item)| {
                    let normalized = normalize(item, root, &item_key);
                    (item_key, normalized)
                })
                .collect(),
        ),
        Value::List(items) => Value::List(items.iter().map(|item| normalize(item, root, "")).collect()),
        _ if key == "completed_at" || key == "next_retry_at" => {
            if value.is_truthy() {
                Value::Str("set".to_owned())
            } else {
                Value::Null
            }
        }
        Value::Str(text) => Value::Str(text.replace(root, "<root>")),
        other => other.clone(),
    }
}

/// Every dictionary's keys sorted, so key order does not count.
fn sorted_keys(value: &Value) -> Value {
    match value {
        Value::Dict(dict) => {
            let mut entries: Vec<(String, Value)> = dict.iter().map(|(key, item)| (str_key(key), sorted_keys(item))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Dict(entries.into_iter().collect())
        }
        Value::List(items) => Value::List(items.iter().map(sorted_keys).collect()),
        other => other.clone(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// `_tree_files(root)`: JSON outside the assets and blobs as normalised data, the rest by
/// size and hash.
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
    let root_text = root.to_string_lossy().into_owned();
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
            let parsed = pyjson::loads(std::str::from_utf8(&data).expect("utf-8")).expect("JSON file");
            entry.insert("json", normalize(&parsed, &root_text, ""));
        } else {
            entry.insert("size", Value::Int(i128::try_from(data.len()).expect("size")));
            entry.insert("sha256", Value::Str(sha256_hex(&data)));
        }
        files.insert(name, Value::Dict(entry));
    }
    files
}

fn text(row: &PgRow, column: &str) -> Value {
    Value::Str(row.get(column))
}

fn json(row: &PgRow, column: &str) -> Value {
    pyjson::loads(&row.get::<String, _>(column)).expect("JSON column")
}

fn int(row: &PgRow, column: &str) -> Value {
    match row.try_get::<Option<i64>, _>(column) {
        Ok(value) => value.map_or(Value::Null, |value| Value::Int(i128::from(value))),
        Err(_) => Value::Int(i128::from(row.get::<i32, _>(column))),
    }
}

fn boolean(row: &PgRow, column: &str) -> Value {
    Value::Bool(row.get(column))
}

async fn fetch(db: &fav_db::Db, sql: &str) -> Vec<PgRow> {
    sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_all(db.pool())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
}

/// A recorded column and how to read it.
type Field = (&'static str, fn(&PgRow, &str) -> Value);

fn record(row: &PgRow, fields: &[Field]) -> Value {
    let mut dict = Dict::new();
    for (field, read) in fields {
        dict.insert(*field, read(row, field));
    }
    Value::Dict(dict)
}

/// One table as `{key: {field: value}}`.
async fn table(db: &fav_db::Db, sql: &str, key: fn(&PgRow) -> String, fields: &[Field]) -> Value {
    let mut dict = Dict::new();
    for row in fetch(db, sql).await {
        dict.insert(key(&row), record(&row, fields));
    }
    Value::Dict(dict)
}

const CHARACTER_FIELDS: [Field; 7] = [
    ("source_id", int),
    ("name_zh", text),
    ("name_en", text),
    ("manifest_path", text),
    ("active", boolean),
    ("completed", boolean),
    ("source_metadata", json),
];

const MODEL_FIELDS: [Field; 17] = [
    ("model_type", text),
    ("source", text),
    ("character_key", text),
    ("costume_key", text),
    ("costume_id", int),
    ("costume_name_zh", text),
    ("costume_name_en", text),
    ("primary_url", text),
    ("fallback_url", text),
    ("display_info_url", text),
    ("availability_state", text),
    ("active", boolean),
    ("failed_count", int),
    ("last_error", text),
    ("source_metadata", json),
    ("completed", boolean),
    ("cooling", boolean),
];

const ASSET_FIELDS: [Field; 11] = [
    ("normalized_url", text),
    ("kind", text),
    ("fallback_url", text),
    ("status", text),
    ("sha256", text),
    ("size", int),
    ("content_type", text),
    ("downloaded_url", text),
    ("failed_count", int),
    ("last_error", text),
    ("cooling", boolean),
];

const RELATION_FIELDS: [Field; 8] = [
    ("model_id", text),
    ("url", text),
    ("kind", text),
    ("context_hash", text),
    ("local_path", text),
    ("original_filename", text),
    ("fallback_url", text),
    ("context_json", json),
];

/// `_rows(database, root)`, read from the tables.
async fn rows(db: &fav_db::Db, root: &str) -> Value {
    let mut all = Dict::new();
    all.insert(
        "characters",
        table(
            db,
            "SELECT character_key, source_id, name_zh, name_en, manifest_path, active,
                completed_at IS NOT NULL AS completed, source_metadata::text AS source_metadata
            FROM azurlane_characters ORDER BY character_key COLLATE \"C\"",
            |row| row.get("character_key"),
            &CHARACTER_FIELDS,
        )
        .await,
    );
    all.insert(
        "models",
        table(
            db,
            "SELECT model_id, model_type, source, character_key, costume_key, costume_id, costume_name_zh,
                costume_name_en, primary_url, fallback_url, display_info_url, availability_state, active,
                failed_count, last_error, source_metadata::text AS source_metadata,
                completed_at IS NOT NULL AS completed, COALESCE(next_retry_at > NOW(), FALSE) AS cooling
            FROM azurlane_models ORDER BY model_id COLLATE \"C\"",
            |row| row.get("model_id"),
            &MODEL_FIELDS,
        )
        .await,
    );
    all.insert(
        "assets",
        table(
            db,
            "SELECT url, normalized_url, kind, fallback_url, status, sha256, size, content_type, downloaded_url,
                failed_count, last_error, COALESCE(next_retry_at > NOW(), FALSE) AS cooling
            FROM azurlane_assets ORDER BY url COLLATE \"C\"",
            |row| row.get("url"),
            &ASSET_FIELDS,
        )
        .await,
    );
    let relations = fetch(
        db,
        "SELECT model_id, url, kind, context_hash, local_path, original_filename, fallback_url,
            context_json::text AS context_json
        FROM azurlane_model_assets
        ORDER BY model_id COLLATE \"C\", url COLLATE \"C\", kind COLLATE \"C\", context_hash COLLATE \"C\"",
    )
    .await;
    all.insert(
        "model_assets",
        Value::List(relations.iter().map(|row| record(row, &RELATION_FIELDS)).collect()),
    );
    all.insert(
        "ship_details",
        table(
            db,
            "SELECT ship_group_id, region, fingerprint, payload::text AS payload FROM azurlane_ship_details
            ORDER BY ship_group_id",
            |row| row.get::<i64, _>("ship_group_id").to_string(),
            &[("region", text), ("fingerprint", text), ("payload", json)],
        )
        .await,
    );
    all.insert(
        "blobs",
        table(
            db,
            "SELECT sha256, size, content_type, blob_path FROM azurlane_blobs ORDER BY sha256, size",
            |row| format!("{}:{}", row.get::<String, _>("sha256"), row.get::<i64, _>("size")),
            &[("content_type", text), ("blob_path", text)],
        )
        .await,
    );
    normalize(&Value::Dict(all), root, "")
}

/// One answer of the scripted web.
fn reply(answer: &Value) -> Reply {
    let status = u16::try_from(golden::int(golden::get(answer, "status"))).expect("status");
    let headers = match golden::get(answer, "headers") {
        Value::Dict(headers) => headers.iter().map(|(name, value)| (str_key(name), golden::string(value))).collect(),
        _ => Vec::new(),
    };
    Reply::Respond {
        status,
        headers,
        body: golden::string(golden::get(answer, "body")).into_bytes(),
    }
}

#[derive(Debug, Default)]
struct Served {
    counts: std::collections::HashMap<String, usize>,
    requests: Vec<[String; 5]>,
}

/// Compares two values whose dictionaries may differ in key order, naming the first
/// differing entry of a dictionary.
fn assert_same(actual: &Value, expected: &Value, context: &str) {
    let (actual, expected) = (sorted_keys(actual), sorted_keys(expected));
    if actual == expected {
        return;
    }
    if let (Value::Dict(have), Value::Dict(want)) = (&actual, &expected) {
        for (key, item) in want.iter() {
            let key = str_key(key);
            assert_eq!(have.get(&key), Some(item), "{context}: {key}");
        }
        let extra: Vec<String> = have
            .iter()
            .map(|(key, _)| str_key(key))
            .filter(|key| !want.contains_key(key))
            .collect();
        panic!("{context}: unexpected entries {extra:?}");
    }
    assert_eq!(actual, expected, "{context}");
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Two runs, each checked on every recorded side.
async fn crawl_matches_the_baseline() {
    let document = golden::load("azurlane/crawl.json");
    let case = &golden::cases(&document)[0];
    let database = crate::testutil::database().await;
    let db = database.db().clone();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("azurlane");
    let root_text = root.to_string_lossy().into_owned();

    for (index, run) in golden::list(golden::get(case, "runs")).iter().enumerate() {
        let web = golden::get(run, "web").as_dict().expect("web").clone();
        let served = Arc::new(Mutex::new(Served::default()));
        let recorder = Arc::clone(&served);
        let fake = FakeWeb::start(move |seen| {
            let url = seen
                .url
                .strip_prefix("http://")
                .map_or_else(|| seen.url.clone(), |rest| format!("https://{rest}"));
            let mut served = recorder.lock().expect("lock");
            let header = |name: &str| seen.header(name).unwrap_or_default().to_owned();
            served.requests.push([
                seen.method.clone(),
                url.clone(),
                header("accept"),
                header("referer"),
                header("user-agent"),
            ]);
            let count = served.counts.entry(url.clone()).or_insert(0);
            let served_index = *count;
            *count += 1;
            match web.get(&url) {
                Some(Value::List(answers)) => reply(&answers[served_index.min(answers.len() - 1)]),
                _ => Reply::status(404),
            }
        })
        .await;
        let client = |options: ClientOptions| {
            HttpClient::new(&ClientOptions {
                proxy: Some(fake.proxy.clone()),
                ..options
            })
            .expect("client")
        };
        let clients = Clients {
            assets: client(asset_client_options()),
            source: client(source_client_options(Duration::from_secs(30))),
            origin_source: None,
            origin: None,
        };
        let sleeper = RecordingSleeper::new();
        let crawler = AzurLaneCrawler::new(&root_text, clients, db.clone(), super::tests::test_pacing())
            .with_sleeper(sleeper.sleeper())
            .with_jitter(Arc::new(|| 0), Arc::new(|_| 0.0))
            .with_clock(Arc::new(|| Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).single().expect("time")))
            .with_plain_http();
        let error = match crawler.update().await {
            Ok(()) => String::new(),
            Err(error) => format!("{}: {}", error.class_name(), error.message()),
        };
        assert_eq!(error, golden::string(golden::get(run, "error")), "run {index}: error");

        let mut requests = served.lock().expect("lock").requests.clone();
        requests.sort_by(|a, b| (&a[1], &a[0]).cmp(&(&b[1], &b[0])));
        let requests: Vec<Value> = requests
            .into_iter()
            .map(|[method, url, accept, referer, user_agent]| {
                let mut dict = Dict::new();
                dict.insert("method", Value::Str(method));
                dict.insert("url", Value::Str(url));
                dict.insert("accept", Value::Str(accept));
                dict.insert("referer", Value::Str(referer));
                dict.insert("user_agent", Value::Str(user_agent));
                Value::Dict(dict)
            })
            .collect();
        assert_eq!(
            requests.as_slice(),
            golden::list(golden::get(run, "requests")),
            "run {index}: requests"
        );

        let sleeps: Vec<Value> = sleeper
            .delays()
            .into_iter()
            .filter(|delay| *delay > 0.0)
            .map(Value::Float)
            .collect();
        assert_eq!(sleeps.as_slice(), golden::list(golden::get(run, "sleeps")), "run {index}: sleeps");

        let files = tree_files(&root);
        let expected_files = golden::get(run, "files").as_dict().expect("files");
        let names = |dict: &Dict| dict.iter().map(|(key, _)| str_key(key)).collect::<Vec<_>>();
        assert_eq!(names(&files), names(expected_files), "run {index}: file names");
        for (name, expected) in expected_files.iter() {
            let name = str_key(name);
            assert_same(files.get(&name).expect("listed"), expected, &format!("run {index}: {name}"));
        }

        let rows = rows(&db, &root_text).await;
        let expected_rows = golden::get(run, "rows");
        for table in ["characters", "models", "assets", "model_assets", "ship_details", "blobs"] {
            assert_same(
                golden::get(&rows, table),
                golden::get(expected_rows, table),
                &format!("run {index}: {table}"),
            );
        }

        // `database.lapse_cooldowns()`: every retry wait is over before the next run.
        sqlx::raw_sql(
            "UPDATE azurlane_models SET next_retry_at = NOW() - INTERVAL '1 minute' WHERE next_retry_at IS NOT NULL;
            UPDATE azurlane_assets SET next_retry_at = NOW() - INTERVAL '1 minute' WHERE next_retry_at IS NOT NULL;",
        )
        .execute(db.pool())
        .await
        .expect("lapse cooldowns");
    }
}
