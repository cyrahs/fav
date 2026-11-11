//! The MCP endpoint against FastMCP's own answers, over a real PostgreSQL
//! (`FAV_TEST_DATABASE_URL`).
//!
//! `goldens/mcp/protocol.json` replays HTTP exchanges through the Python app, so
//! every check of the transport, every validation text and the whole `tools/list` must
//! come back byte for byte. `calls.json` replays tool calls over stored state: each
//! scenario's rows are seeded into a fresh database, and the responses, the statements
//! `ops` ran (as Python sent them to its database), the `fav-mcp` log lines and the rows
//! left behind must match. `ops.json` covers the pure helpers. The tests after the
//! replays port `tests/test_mcp.py`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone as _, Utc};
use chrono_tz::Tz;
use fav_core::golden;
use fav_core::logging;
use fav_core::pyjson::{self, Dict, DumpOptions, Value};
use sqlx::Row as _;

use super::ops::{self, LogQuery, OpsError, Param, Redactor, RunQuery, Statement};
use super::tools;
use crate::tests::{Answer, Harness, TOKEN};

/// `FIXED_NOW` of the MCP goldens: the clock `ops` saw.
fn mcp_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 2, 12, 0, 0).single().expect("a valid time")
}

const DSN: &str = "postgresql://fav:dsn-password-42@postgresql.media.svc:5432/fav";
const JSON_HEADERS: [(&str, &str); 2] = [
    ("accept", "application/json, text/event-stream"),
    ("content-type", "application/json"),
];
/// The response headers Python's replay records (`_RESPONSE_HEADERS`).
const RECORDED_HEADERS: [&str; 4] = ["content-type", "allow", "location", "mcp-session-id"];

/// The loggers whose lines the goldens record (`_LOGGERS`).
const LOGGERS: [&str; 3] = ["embyx.fav-mcp", "mcp.server.transport_security", "mcp.server.lowlevel.server"];

/// Drive `future` to completion and return the lines it logged to [`LOGGERS`], formatted as
/// the golden's handler formats them (`<name> <level> <message>`).
async fn logged<F: Future>(future: F) -> (F::Output, Vec<String>) {
    // `capture` collects per thread: poll the future inside it.
    let mut future = std::pin::pin!(future);
    let mut records = Vec::new();
    loop {
        let mut output = None;
        let polled = logging::capture(|| {
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut context) {
                output = Some(value);
            }
        });
        records.extend(polled);
        if let Some(output) = output {
            let lines = records
                .iter()
                .filter(|record| LOGGERS.contains(&record.name.as_str()))
                .map(|record| format!("{} {} {}", record.name, record.level.name(), record.message))
                .collect();
            return (output, lines);
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

fn str_list(value: &Value) -> Vec<String> {
    golden::list(value).iter().map(golden::string).collect()
}

fn header_pairs(request: &Value) -> Vec<(String, String)> {
    golden::get(request, "headers")
        .as_dict()
        .expect("headers")
        .iter()
        .map(|(key, value)| (fav_runtime::pystr::str_key(key), golden::string(value)))
        .collect()
}

async fn send(harness: &Harness, method: &str, path: &str, headers: &[(String, String)], body: Option<&str>) -> (Answer, Vec<String>) {
    let refs: Vec<(&str, &str)> = headers.iter().map(|(key, value)| (key.as_str(), value.as_str())).collect();
    logged(harness.request(method, path, &refs, body)).await
}

/// One recorded response against ours: status, the recorded headers, and the body.
fn assert_response(context: &str, answer: &Answer, expected: &Value) {
    assert_eq!(
        i128::from(answer.status),
        golden::int(golden::get(expected, "status")),
        "{context}: {}",
        answer.text()
    );
    let expected_headers = golden::get(expected, "headers").as_dict().expect("headers");
    for header in RECORDED_HEADERS {
        assert_eq!(
            answer.header(header),
            expected_headers.get(header).and_then(Value::as_str),
            "{context}: header {header}"
        );
    }
    if expected.get("empty").is_some() {
        assert!(answer.body.is_empty(), "{context}: {}", answer.text());
    } else if let Some(json) = expected.get("json") {
        assert_eq!(&answer.json(), json, "{context}");
        assert_eq!(answer.text(), golden::string(golden::get(expected, "bytes")), "{context}");
    } else {
        assert_eq!(answer.text(), golden::string(golden::get(expected, "text")), "{context}");
    }
}

#[tokio::test]
async fn protocol_replays_python() {
    let document = golden::load("mcp/protocol.json");
    let mut harness = Harness::new().await;
    harness.state.clock = Arc::new(mcp_now);
    for case in golden::cases(&document) {
        for exchange in golden::list(golden::get(case, "exchanges")) {
            let request = golden::get(exchange, "request");
            let method = golden::string(golden::get(request, "method"));
            let path = golden::string(golden::get(request, "path"));
            let headers = header_pairs(request);
            let body = request.get("body").map(golden::string);
            let (answer, lines) = send(&harness, &method, &path, &headers, body.as_deref()).await;
            let context = format!("{method} {path} {headers:?} {body:?}");
            assert_response(&context, &answer, golden::get(exchange, "response"));
            assert_eq!(lines, str_list(golden::get(exchange, "logged")), "{context}");
        }
    }
    harness.finish().await;
}

// ---------- tool calls over stored state ----------

fn text(row: &Value, key: &str, default: &str) -> String {
    row.get(key)
        .filter(|value| !matches!(value, Value::Null))
        .map_or_else(|| default.to_owned(), golden::string)
}

fn int(row: &Value, key: &str, default: i64) -> i64 {
    row.get(key)
        .map_or(default, |value| i64::try_from(golden::int(value)).expect("a 64-bit value"))
}

fn time(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value).expect("an ISO time").to_utc()
}

fn optional_time(row: &Value, key: &str) -> Option<DateTime<Utc>> {
    row.get(key).and_then(Value::as_str).map(time)
}

async fn seed_mcp_tables(harness: &Harness, setup: &Value) {
    let pool = harness.state.db.pool();
    for row in setup.get("wechat_events").map(golden::list).unwrap_or_default() {
        let message = pyjson::dumps(row.get("message").unwrap_or(&Value::Dict(Dict::new())), &DumpOptions::plain()).expect("dumpable");
        sqlx::query(
            "INSERT INTO wechat_events (msg_id, create_time, work, seq, message, status, attempts, last_error, updated_at)
             VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, $8, $9);",
        )
        .bind(int(row, "msg_id", 0))
        .bind(int(row, "create_time", 0))
        .bind(text(row, "work", ""))
        .bind(int(row, "seq", 0))
        .bind(message)
        .bind(text(row, "status", "pending"))
        .bind(i32::try_from(int(row, "attempts", 0)).expect("attempts"))
        .bind(text(row, "last_error", ""))
        .bind(time(&text(row, "updated_at", "")).naive_utc())
        .execute(pool)
        .await
        .expect("a WeChat event");
    }
    if let Some(Value::Dict(state)) = setup.get("wechat_state") {
        for (key, value) in state.iter() {
            sqlx::query("INSERT INTO wechat_state (key, value) VALUES ($1, $2);")
                .bind(fav_runtime::pystr::str_key(key))
                .bind(golden::string(value))
                .execute(pool)
                .await
                .expect("a WeChat state value");
        }
    }
    let notifications = setup.get("notifications").map(golden::list).unwrap_or_default();
    for row in notifications {
        sqlx::query(
            "INSERT INTO notifications (id, kind, source, scope, dedupe_key, title, body, delivery_status, attempt_count,
                 occurrence_count, last_error, created_at, next_attempt_at, delivered_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14);",
        )
        .bind(int(row, "id", 0))
        .bind(text(row, "kind", ""))
        .bind(text(row, "source", ""))
        .bind(text(row, "scope", "item"))
        .bind(text(row, "dedupe_key", ""))
        .bind(text(row, "title", ""))
        .bind(text(row, "body", ""))
        .bind(text(row, "delivery_status", "pending"))
        .bind(i32::try_from(int(row, "attempt_count", 0)).expect("attempts"))
        .bind(i32::try_from(int(row, "occurrence_count", 1)).expect("occurrences"))
        .bind(text(row, "last_error", ""))
        .bind(optional_time(row, "created_at"))
        .bind(optional_time(row, "next_attempt_at"))
        .bind(optional_time(row, "delivered_at"))
        .execute(pool)
        .await
        .expect("a notification");
    }
    if !notifications.is_empty() {
        harness
            .execute(
                "SELECT setval(pg_get_serial_sequence('notifications', 'id'), (SELECT MAX(id) FROM notifications));",
                &[],
            )
            .await;
    }
}

fn write_logs(directory: &Path, files: &[Value]) {
    for entry in files {
        let path = directory.join(golden::string(golden::get(entry, "name")));
        std::fs::write(&path, golden::string(golden::get(entry, "text"))).expect("a log file");
        let moment = time(&golden::string(golden::get(entry, "mtime")));
        let since_epoch = Duration::from_micros(u64::try_from(moment.timestamp_micros()).expect("after 1970"));
        let file = std::fs::File::options().write(true).open(&path).expect("the log file");
        file.set_modified(SystemTime::UNIX_EPOCH + since_epoch).expect("an mtime");
    }
}

/// What Python sent to `query_db`: the statement with `?` placeholders and collapsed
/// whitespace, and the parameters as `_plain` writes them.
fn statement_value(statement: &Statement) -> Value {
    static PLACEHOLDER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"\$\d+").expect("a pattern"));
    let sql = PLACEHOLDER
        .replace_all(&statement.sql, "?")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let params = statement
        .params
        .iter()
        .map(|param| match param {
            Param::Text(text) => Value::Str(text.clone()),
            Param::Int(int) => Value::from(*int),
            Param::Time(moment) => Value::Str(pyjson::py_isoformat_datetime(moment)),
            Param::Ints(ints) => Value::List(ints.iter().map(|int| Value::from(*int)).collect()),
        })
        .collect();
    crate::json::dict([("sql", Value::Str(sql)), ("params", Value::List(params))])
}

/// Times the database set itself (`CURRENT_TIMESTAMP`) moved to the moments Python's
/// fakes used: the API clock for queued runs, `FIXED_NOW` for retries.
async fn pin_database_times(harness: &Harness) {
    harness
        .execute(
            "UPDATE control_requests SET requested_at = '2026-03-02T00:00:00Z' WHERE requested_at BETWEEN now() - interval '10 minutes' AND now();",
            &[],
        )
        .await;
    harness
        .execute(
            "UPDATE notifications SET next_attempt_at = '2026-03-02T12:00:00Z' WHERE next_attempt_at BETWEEN now() - interval '10 minutes' AND now();",
            &[],
        )
        .await;
    harness
        .execute(
            "UPDATE wechat_events SET updated_at = '2026-03-02 12:00:00' WHERE updated_at BETWEEN LOCALTIMESTAMP - interval '10 minutes' AND LOCALTIMESTAMP;",
            &[],
        )
        .await;
}

/// A recorded time as UTC with microseconds, so both sides compare whatever their offset.
fn utc_text(value: &Value) -> Value {
    match value {
        Value::Str(text) => Value::Str(time(text).to_rfc3339_opts(chrono::SecondsFormat::Micros, true)),
        other => other.clone(),
    }
}

fn utc_value(value: Option<DateTime<Utc>>) -> Value {
    value.map_or(Value::Null, |moment| {
        Value::Str(moment.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
    })
}

/// A dictionary with its keys sorted, recursively (`jsonb` has its own key order).
fn sorted(value: &Value) -> Value {
    match value {
        Value::Dict(dict) => {
            let mut entries: Vec<(String, Value)> = dict
                .iter()
                .map(|(key, item)| (fav_runtime::pystr::str_key(key), sorted(item)))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Dict(entries.into_iter().collect())
        }
        Value::List(items) => Value::List(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// The expected `after` rows with their times as UTC and keys sorted.
fn expected_rows(rows: &Value, time_keys: &[&str]) -> Value {
    Value::List(
        golden::list(rows)
            .iter()
            .map(|row| {
                let mut dict = row.as_dict().expect("a row").clone();
                for key in time_keys {
                    if let Some(value) = dict.get(key).map(utc_text) {
                        dict.insert(*key, value);
                    }
                }
                sorted(&Value::Dict(dict))
            })
            .collect(),
    )
}

#[allow(clippy::too_many_lines)] // One query and row shape per table of the golden's `after`.
async fn actual_after(harness: &Harness) -> Dict {
    let pool = harness.state.db.pool();
    let runs = sqlx::query(
        "SELECT id, kind, target, status, requested_at, started_at, finished_at, result, error FROM control_requests ORDER BY id;",
    )
    .fetch_all(pool)
    .await
    .expect("control requests");
    let runs = runs
        .iter()
        .map(|row| {
            sorted(&crate::json::dict([
                ("id", Value::from(row.get::<i64, _>("id"))),
                ("kind", Value::Str(row.get("kind"))),
                ("target", Value::Str(row.get("target"))),
                ("status", Value::Str(row.get("status"))),
                ("requested_at", utc_value(row.get("requested_at"))),
                ("started_at", utc_value(row.get("started_at"))),
                ("finished_at", utc_value(row.get("finished_at"))),
                ("result", Value::Str(row.get("result"))),
                ("error", Value::Str(row.get("error"))),
            ]))
        })
        .collect();
    let events = sqlx::query(
        "SELECT msg_id, create_time, work, seq, message::text AS message, status, attempts, last_error, updated_at
         FROM wechat_events ORDER BY msg_id;",
    )
    .fetch_all(pool)
    .await
    .expect("WeChat events");
    let events = events
        .iter()
        .map(|row| {
            let updated: Option<NaiveDateTime> = row.get("updated_at");
            sorted(&crate::json::dict([
                ("msg_id", Value::from(row.get::<i64, _>("msg_id"))),
                ("create_time", Value::from(row.get::<i64, _>("create_time"))),
                ("work", Value::Str(row.get("work"))),
                ("seq", Value::from(row.get::<i64, _>("seq"))),
                ("message", pyjson::loads(&row.get::<String, _>("message")).expect("JSON")),
                ("status", Value::Str(row.get("status"))),
                ("attempts", Value::from(i64::from(row.get::<i32, _>("attempts")))),
                ("last_error", Value::Str(row.get("last_error"))),
                ("updated_at", utc_value(updated.map(|naive| naive.and_utc()))),
            ]))
        })
        .collect();
    let state = sqlx::query("SELECT key, value FROM wechat_state ORDER BY key;")
        .fetch_all(pool)
        .await
        .expect("WeChat state");
    let state: Dict = state
        .iter()
        .map(|row| (row.get::<String, _>("key"), Value::Str(row.get("value"))))
        .collect();
    let notifications = sqlx::query(
        "SELECT id, kind, source, scope, dedupe_key, title, body, delivery_status, attempt_count, occurrence_count, last_error,
                created_at, next_attempt_at, delivered_at FROM notifications ORDER BY id;",
    )
    .fetch_all(pool)
    .await
    .expect("notifications");
    let notifications = notifications
        .iter()
        .map(|row| {
            sorted(&crate::json::dict([
                ("id", Value::from(row.get::<i64, _>("id"))),
                ("kind", Value::Str(row.get("kind"))),
                ("source", Value::Str(row.get("source"))),
                ("scope", Value::Str(row.get("scope"))),
                ("dedupe_key", Value::Str(row.get("dedupe_key"))),
                ("title", Value::Str(row.get("title"))),
                ("body", Value::Str(row.get("body"))),
                ("delivery_status", Value::Str(row.get("delivery_status"))),
                ("attempt_count", Value::from(i64::from(row.get::<i32, _>("attempt_count")))),
                ("occurrence_count", Value::from(i64::from(row.get::<i32, _>("occurrence_count")))),
                ("last_error", Value::Str(row.get("last_error"))),
                ("created_at", utc_value(row.get("created_at"))),
                ("next_attempt_at", utc_value(row.get("next_attempt_at"))),
                ("delivered_at", utc_value(row.get("delivered_at"))),
            ]))
        })
        .collect();
    let settings = sqlx::query("SELECT section, value::text AS value FROM app_settings ORDER BY section;")
        .fetch_all(pool)
        .await
        .expect("settings");
    let settings: Dict = settings
        .iter()
        .map(|row| {
            (
                row.get::<String, _>("section"),
                sorted(&pyjson::loads(&row.get::<String, _>("value")).expect("JSON")),
            )
        })
        .collect();
    let mut after = Dict::new();
    after.insert("control_requests", Value::List(runs));
    after.insert("wechat_events", Value::List(events));
    after.insert("wechat_state", Value::Dict(state));
    after.insert("notifications", Value::List(notifications));
    after.insert("settings", Value::Dict(settings));
    after
}

fn expected_after(after: &Value) -> Dict {
    let mut expected = Dict::new();
    expected.insert(
        "control_requests",
        expected_rows(
            golden::get(after, "control_requests"),
            &["requested_at", "started_at", "finished_at"],
        ),
    );
    expected.insert("wechat_events", expected_rows(golden::get(after, "wechat_events"), &["updated_at"]));
    expected.insert("wechat_state", golden::get(after, "wechat_state").clone());
    expected.insert(
        "notifications",
        expected_rows(
            golden::get(after, "notifications"),
            &["created_at", "next_attempt_at", "delivered_at"],
        ),
    );
    let settings: Dict = golden::get(after, "settings")
        .as_dict()
        .expect("settings")
        .iter()
        .map(|(key, value)| (fav_runtime::pystr::str_key(key), sorted(value)))
        .collect();
    expected.insert("settings", Value::Dict(settings));
    expected
}

/// The response with the fields the database chose (`volatile`) replaced by Python's.
fn pinned_text(answer: &Answer, expected: &Value, volatile: &[String]) -> String {
    let mut text = answer.text();
    let structured = |value: &Value| value.get("result").and_then(|result| result.get("structuredContent")).cloned();
    let (Some(ours), Some(theirs)) = (structured(&answer.json()), structured(expected)) else {
        return text;
    };
    for key in volatile {
        match (ours.get(key), theirs.get(key)) {
            (Some(Value::Str(ours)), Some(Value::Str(theirs))) => text = text.replace(ours.as_str(), theirs),
            (ours, theirs) => assert_eq!(ours, theirs, "volatile {key}"),
        }
    }
    text
}

#[tokio::test]
async fn tool_calls_replay_python() {
    let document = golden::load("mcp/calls.json");
    for case in golden::cases(&document) {
        let name = golden::string(golden::get(case, "name"));
        let setup = golden::get(case, "setup");
        let logs = tempfile::tempdir().expect("a log directory");
        write_logs(logs.path(), setup.get("logs").map(golden::list).unwrap_or_default());
        let mut harness = Harness::new().await;
        harness.seed(setup).await;
        seed_mcp_tables(&harness, setup).await;
        harness.state.clock = Arc::new(mcp_now);
        harness.state.mcp.log_dir = logs.path().to_path_buf();
        let dsn = setup.get("dsn").map_or_else(|| DSN.to_owned(), golden::string);
        harness.state.mcp.bootstrap_secrets = vec![TOKEN.to_owned()];
        harness.state.mcp.bootstrap_secrets.extend(ops::dsn_password(&dsn));
        for (index, call) in golden::list(golden::get(case, "calls")).iter().enumerate() {
            let tool = golden::string(golden::get(call, "tool"));
            let arguments = golden::get(call, "arguments");
            let mut params = Dict::new();
            params.insert("name", Value::Str(tool.clone()));
            params.insert("arguments", arguments.clone());
            let mut message = Dict::new();
            message.insert("jsonrpc", Value::from("2.0"));
            message.insert("id", Value::from(i64::try_from(index + 1).expect("an id")));
            message.insert("method", Value::from("tools/call"));
            message.insert("params", Value::Dict(params));
            let body = pyjson::dumps(&Value::Dict(message), &DumpOptions::plain()).expect("dumpable");
            let headers: Vec<(String, String)> = JSON_HEADERS
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect();
            ops::journal::take();
            let (answer, lines) = send(&harness, "POST", "/mcp", &headers, Some(&body)).await;
            let statements: Vec<Value> = ops::journal::take().iter().map(statement_value).collect();
            let context = format!("[{name}] {tool} {arguments:?}");
            assert_eq!(answer.status, 200, "{context}: {}", answer.text());
            let expected = golden::get(call, "response");
            let volatile = call.get("volatile").map(str_list).unwrap_or_default();
            let text = pinned_text(&answer, expected, &volatile);
            assert_eq!(&pyjson::loads(&text).expect("JSON"), expected, "{context}");
            assert_eq!(text, golden::string(golden::get(call, "bytes")), "{context}");
            assert_eq!(
                Value::List(statements),
                golden::get(call, "queries").clone(),
                "{context}: statements"
            );
            assert_eq!(lines, str_list(golden::get(call, "logged")), "{context}: logged");
            pin_database_times(&harness).await;
        }
        let after = actual_after(&harness).await;
        let expected = expected_after(golden::get(case, "after"));
        for (table, rows) in expected.iter() {
            let table = fav_runtime::pystr::str_key(table);
            assert_eq!(after.get(&table), Some(rows), "[{name}] after: {table}");
        }
        harness.finish().await;
    }
}

// ---------- pure helpers ----------

fn since_value(input: &Value) -> Value {
    let text = input.as_str().unwrap_or_default();
    match ops::parse_since(text, mcp_now()) {
        Ok(parsed) => crate::json::dict([(
            "value",
            parsed.map_or(Value::Null, |moment| Value::Str(pyjson::py_isoformat_datetime(&moment))),
        )]),
        Err(OpsError::Refused(message)) => crate::json::dict([("error", Value::Str(message))]),
        Err(OpsError::Exception(kind, message)) => crate::json::dict([("exception", Value::from(kind)), ("error", Value::Str(message))]),
    }
}

/// Whether an `isoformat()` ends in an offset with microseconds (`+05:30:15.500000`).
fn fractional_offset(text: &str) -> bool {
    text.rfind(['+', '-']).is_some_and(|at| text[at..].contains('.'))
}

/// The instant of `isoformat()` text, offset microseconds included, as UTC microseconds.
fn utc_micros(text: &str) -> i64 {
    let at = text.rfind(['+', '-']).expect("an offset");
    let (local, offset) = text.split_at(at);
    let local = NaiveDateTime::parse_from_str(local, "%Y-%m-%dT%H:%M:%S%.f").expect("a local time");
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let mut parts = offset[1..].split(':');
    let mut micros = 0_i64;
    for (unit, part) in [3_600_000_000_i64, 60_000_000, 1_000_000].into_iter().zip(&mut parts) {
        let value: f64 = part.parse().expect("an offset part");
        #[allow(clippy::cast_possible_truncation)] // An offset is under a day of microseconds.
        let part_micros = (value * 1_000_000.0).round() as i64;
        micros += part_micros * unit / 1_000_000;
    }
    local.and_utc().timestamp_micros() - sign * micros
}

fn aware(value: Option<&Value>) -> Option<DateTime<FixedOffset>> {
    value
        .and_then(Value::as_str)
        .map(|text| DateTime::parse_from_rfc3339(text).expect("an ISO time"))
}

#[test]
fn ops_helpers_replay_python() {
    let document = golden::load("mcp/ops.json");
    let calls = golden::load("mcp/calls.json");
    let log_files = golden::cases(&calls)
        .iter()
        .find(|case| golden::string(golden::get(case, "name")) == "logs")
        .and_then(|case| golden::get(case, "setup").get("logs"))
        .map(golden::list)
        .expect("the log files of the `logs` scenario");
    let logs = tempfile::tempdir().expect("a log directory");
    write_logs(logs.path(), log_files);
    for case in golden::cases(&document) {
        let kind = golden::string(golden::get(case, "kind"));
        let context = format!("{case:?}");
        match kind.as_str() {
            "parse_since" => {
                let mut expected = case.as_dict().expect("a case").clone();
                expected.remove("kind");
                expected.remove("input");
                let actual = since_value(golden::get(case, "input"));
                if let (Some(Value::Str(ours)), Some(Value::Str(theirs))) = (actual.get("value"), expected.get("value"))
                    && fractional_offset(theirs)
                {
                    // An offset with a fraction of a second keeps its instant (DIVERGENCES.md D35).
                    assert_eq!(utc_micros(ours), utc_micros(theirs), "{context}");
                    continue;
                }
                assert_eq!(actual, Value::Dict(expected), "{context}");
            }
            "truncate" => {
                let limit = usize::try_from(golden::int(golden::get(case, "limit"))).expect("a limit");
                let value = ops::truncate(&golden::string(golden::get(case, "input")), limit);
                assert_eq!(Value::Str(value), *golden::get(case, "value"), "{context}");
            }
            "bounded" => {
                let value = ops::bounded(golden::int(golden::get(case, "input")), golden::int(golden::get(case, "maximum")));
                assert_eq!(i128::from(value), golden::int(golden::get(case, "value")), "{context}");
            }
            "redact" => {
                let redactor = Redactor::new(str_list(golden::get(case, "secrets")));
                let value = redactor.apply(&golden::string(golden::get(case, "input")));
                assert_eq!(Value::Str(value), *golden::get(case, "value"), "{context}");
            }
            "merge" => {
                let base = golden::get(case, "base").as_dict().expect("a base");
                let changes = golden::get(case, "changes").as_dict().expect("changes");
                assert_eq!(Value::Dict(tools::merge(base, changes)), *golden::get(case, "value"), "{context}");
            }
            "read_logs" => {
                let arguments = golden::get(case, "arguments");
                let query = LogQuery {
                    since: aware(arguments.get("since")),
                    until: aware(arguments.get("until")),
                    contains: String::new(),
                    logger_name: arguments.get("logger_name").map(golden::string).unwrap_or_default(),
                    min_level: arguments.get("min_level").map_or_else(|| "DEBUG".to_owned(), golden::string),
                    limit: arguments.get("limit").map_or(200, golden::int),
                };
                let redactor = Redactor::new(["supersecret-wechat".to_owned()]);
                let value = match ops::read_logs(logs.path(), &query, Tz::UTC, &redactor) {
                    Ok(value) => value,
                    Err(error) => crate::json::dict([("error", Value::from(error.message()))]),
                };
                assert_eq!(value, *golden::get(case, "value"), "{context}");
            }
            "read_logs_missing_dir" => {
                let query = LogQuery {
                    min_level: "DEBUG".to_owned(),
                    limit: 200,
                    ..LogQuery::default()
                };
                let value = ops::read_logs(Path::new("/nonexistent-fav-log-dir"), &query, Tz::UTC, &Redactor::default()).expect("no files");
                assert_eq!(value, *golden::get(case, "value"), "{context}");
            }
            "next_fire_time" => {
                for entry in golden::list(golden::get(case, "cases")) {
                    let now = time(&golden::string(golden::get(entry, "now")));
                    let value = ops::next_fire_time(&golden::string(golden::get(entry, "cron")), Tz::UTC, now);
                    assert_eq!(value, *golden::get(entry, "value"), "{entry:?}");
                }
            }
            "zoned" => zoned_case_replays(case),
            "casefold" => casefold_replays(case),
            other => panic!("unknown case kind {other}"),
        }
    }
}

/// A `zoned` case of `ops.json`: log stamps and schedules read in one `TZ`.
fn zoned_case_replays(case: &Value) {
    let tz = golden::string(golden::get(case, "tz"));
    let (zone, _) = ops::local_timezone(Some(&tz));
    let logs = tempfile::tempdir().expect("a log directory");
    let text = golden::string(golden::get(case, "log_text"));
    std::fs::write(logs.path().join("20260308.log"), text).expect("a log file");
    let query = LogQuery {
        min_level: "DEBUG".to_owned(),
        limit: 200,
        ..LogQuery::default()
    };
    let value = ops::read_logs(logs.path(), &query, zone, &Redactor::default()).expect("the records");
    assert_eq!(value, *golden::get(case, "read_logs"), "{tz}");
    for entry in golden::list(golden::get(case, "next_fire_time")) {
        let now = time(&golden::string(golden::get(entry, "now")));
        let value = ops::next_fire_time(&golden::string(golden::get(entry, "cron")), zone, now);
        assert_eq!(value, *golden::get(entry, "value"), "{tz} {entry:?}");
    }
}

/// The `casefold` case: every character folds as Python 3.12 folds it.
fn casefold_replays(case: &Value) {
    let folds = golden::get(case, "value").as_dict().expect("the folds");
    let mut wrong = Vec::new();
    for ch in (0..=0x10_FFFF_u32).filter_map(char::from_u32) {
        let expected = folds.get(&ch.to_string()).map_or_else(|| ch.to_string(), golden::string);
        let ours = ops::casefold(&ch.to_string());
        if ours != expected {
            wrong.push(format!("U+{:04X} {ours:?} != {expected:?}", u32::from(ch)));
        }
    }
    assert!(wrong.is_empty(), "{} characters fold differently: {wrong:?}", wrong.len());
}

// ---------- tests/test_mcp.py ----------

fn rpc_body(method: &str, params: &Value) -> String {
    let mut message = Dict::new();
    message.insert("jsonrpc", Value::from("2.0"));
    message.insert("id", Value::from(1_i64));
    message.insert("method", Value::from(method));
    message.insert("params", params.clone());
    pyjson::dumps(&Value::Dict(message), &DumpOptions::plain()).expect("dumpable")
}

async fn rpc(harness: &Harness, method: &str, params: &Value) -> Value {
    let answer = harness
        .request("POST", "/mcp", &JSON_HEADERS, Some(&rpc_body(method, params)))
        .await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    answer.json()
}

async fn call_tool(harness: &Harness, name: &str, arguments: &str) -> Value {
    let params = crate::json::dict([("name", Value::from(name)), ("arguments", pyjson::loads(arguments).expect("JSON"))]);
    golden::get(&rpc(harness, "tools/call", &params).await, "result").clone()
}

/// `_tool_payload`: the structured result, unwrapped from `{"result": ...}`.
fn tool_payload(result: &Value) -> Value {
    assert_eq!(result.get("isError"), Some(&Value::Bool(false)), "{result:?}");
    let structured = golden::get(result, "structuredContent");
    match structured.as_dict() {
        Some(dict) if dict.len() == 1 && dict.contains_key("result") => golden::get(structured, "result").clone(),
        _ => structured.clone(),
    }
}

fn tool_names(listed: &Value) -> Vec<String> {
    golden::list(golden::get(golden::get(listed, "result"), "tools"))
        .iter()
        .map(|tool| golden::string(golden::get(tool, "name")))
        .collect()
}

/// Registry completeness (WP18): `tools/list` names Python's 18 tools, in Python's order.
#[tokio::test]
async fn the_contract_has_python_tools() {
    let document = golden::load("mcp/protocol.json");
    let exchanges = golden::list(golden::get(&golden::cases(&document)[0], "exchanges"));
    let listed = exchanges
        .iter()
        .find(|exchange| {
            golden::get(golden::get(exchange, "request"), "body")
                .as_str()
                .is_some_and(|body| body.contains("\"tools/list\""))
                && golden::get(golden::get(exchange, "response"), "json").get("result").is_some()
        })
        .expect("a tools/list exchange");
    let python = tool_names(golden::get(golden::get(listed, "response"), "json"));
    assert_eq!(python.len(), 18);

    let harness = Harness::new().await;
    let rust = tool_names(&rpc(&harness, "tools/list", &Value::Dict(Dict::new())).await);
    assert_eq!(rust, python);
    harness.finish().await;
}

// py: tests/test_mcp.py::test_mcp_endpoint_lists_tools_over_http
#[tokio::test]
async fn endpoint_lists_tools_over_http() {
    let harness = Harness::new().await;
    let init = rpc(
        &harness,
        "initialize",
        &pyjson::loads(r#"{"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "tests", "version": "0"}}"#)
            .expect("JSON"),
    )
    .await;
    assert_eq!(
        golden::string(golden::get(golden::get(golden::get(&init, "result"), "serverInfo"), "name")),
        "fav"
    );
    let names = tool_names(&rpc(&harness, "tools/list", &Value::Dict(Dict::new())).await);
    for name in [
        "get_runtime",
        "list_jobs",
        "list_runs",
        "read_logs",
        "run_job",
        "update_settings",
        "retry_wechat_events",
    ] {
        assert!(names.iter().any(|listed| listed == name), "{name} in {names:?}");
    }
    harness.finish().await;
}

// py: tests/test_mcp.py::test_mcp_endpoint_answers_for_a_cluster_host_name
#[tokio::test]
async fn endpoint_answers_for_a_cluster_host_name() {
    let harness = Harness::new().await;
    let body = rpc_body("tools/list", &Value::Dict(Dict::new()));
    let mut headers = JSON_HEADERS.to_vec();
    headers.push(("host", "fav.media.svc.cluster.local"));
    let answer = harness.request("POST", "/mcp", &headers, Some(&body)).await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    assert!(tool_names(&answer.json()).iter().any(|name| name == "run_job"));
    harness.finish().await;
}

// py: tests/test_mcp.py::test_run_job_queues_a_control_request
#[tokio::test]
async fn run_job_queues_a_control_request() {
    let harness = Harness::new().await;
    let payload = tool_payload(&call_tool(&harness, "run_job", r#"{"job": "wechat"}"#).await);
    let rejected = call_tool(&harness, "run_job", r#"{"job": "nope"}"#).await;

    let queued = sqlx::query("SELECT id, target, status FROM control_requests;")
        .fetch_all(harness.state.db.pool())
        .await
        .expect("control requests");
    assert_eq!(queued.len(), 1);
    assert_eq!(golden::int(golden::get(&payload, "id")), i128::from(queued[0].get::<i64, _>("id")));
    assert_eq!(queued[0].get::<String, _>("target"), "wechat");
    assert_eq!(queued[0].get::<String, _>("status"), "pending");
    assert_eq!(rejected.get("isError"), Some(&Value::Bool(true)));
    let text = golden::string(golden::get(&golden::list(golden::get(&rejected, "content"))[0], "text"));
    assert!(text.contains("Unsupported job target"), "{text}");
    harness.finish().await;
}

// py: tests/test_mcp.py::test_update_settings_merges_only_the_given_fields
#[tokio::test]
async fn update_settings_merges_only_the_given_fields() {
    async fn saved(harness: &Harness, section: &str) -> Value {
        let text: String = sqlx::query_scalar("SELECT value::text FROM app_settings WHERE section = $1;")
            .bind(section)
            .fetch_one(harness.state.db.pool())
            .await
            .expect("the stored section");
        pyjson::loads(&text).expect("JSON")
    }
    let harness = Harness::new().await;
    let stored =
        pyjson::loads(r#"{"enabled": true, "cron": "*/15 * * * *", "token": "wechat-secret-token", "api_url": "http://a"}"#).expect("JSON");
    harness.store_settings("web.wechat", &stored).await;
    tool_payload(
        &call_tool(
            &harness,
            "update_settings",
            r#"{"section": "web.wechat", "values": {"cron": "*/30 * * * *"}}"#,
        )
        .await,
    );
    let wechat = saved(&harness, "web.wechat").await;
    assert_eq!(wechat.get("cron"), Some(&Value::from("*/30 * * * *")));
    // The masked token the merge started from is kept as stored, and the rest is unchanged.
    assert_eq!(wechat.get("token"), Some(&Value::from("wechat-secret-token")));
    assert_eq!(wechat.get("api_url"), Some(&Value::from("http://a")));
    assert_eq!(wechat.get("enabled"), Some(&Value::Bool(true)));

    // Python's case: a nested object merges key by key (`proxy: {timeout}` there).
    let stored = pyjson::loads(
        r#"{"cron": "0 1 * * *", "ranking": {"enabled": true, "pages": 3, "deep_scan": {"enabled": true, "quota": 0.5, "max_extra_pages": 7}}}"#,
    )
    .expect("JSON");
    harness.store_settings("web.hanime1", &stored).await;
    tool_payload(
        &call_tool(
            &harness,
            "update_settings",
            r#"{"section": "web.hanime1", "values": {"cron": "0 2 * * *", "ranking": {"deep_scan": {"max_extra_pages": 9}}}}"#,
        )
        .await,
    );
    let hanime1 = saved(&harness, "web.hanime1").await;
    assert_eq!(hanime1.get("cron"), Some(&Value::from("0 2 * * *")));
    let ranking = hanime1.get("ranking").expect("ranking");
    assert_eq!(ranking.get("enabled"), Some(&Value::Bool(true)));
    assert_eq!(ranking.get("pages"), Some(&Value::from(3)));
    let deep_scan = ranking.get("deep_scan").expect("deep_scan");
    assert_eq!(deep_scan.get("enabled"), Some(&Value::Bool(true)));
    assert_eq!(deep_scan.get("quota"), Some(&Value::Float(0.5)));
    assert_eq!(deep_scan.get("max_extra_pages"), Some(&Value::from(9)));
    harness.finish().await;
}

// py: tests/test_mcp.py::test_list_jobs_reports_next_run_and_last_run
#[tokio::test]
async fn list_jobs_reports_next_run_and_last_run() {
    let harness = Harness::new().await;
    harness
        .store_settings(
            "web.wechat",
            &pyjson::loads(r#"{"enabled": true, "cron": "*/15 * * * *", "token": "favinbox-token"}"#).expect("JSON"),
        )
        .await;
    harness
        .store_settings(
            "web.pixiv",
            &pyjson::loads(r#"{"enabled": false, "cron": "0 1 * * *"}"#).expect("JSON"),
        )
        .await;
    harness
        .store_control_request(
            &pyjson::loads(
                r#"{"id": 3, "kind": "scheduled_job", "target": "wechat", "status": "failed", "requested_at": "2026-10-12T17:00:00+00:00",
                    "started_at": "2026-10-12T17:00:00+00:00", "finished_at": "2026-10-12T17:00:12+00:00", "error": "boom"}"#,
            )
            .expect("JSON"),
        )
        .await;
    let jobs = tool_payload(&call_tool(&harness, "list_jobs", "{}").await);

    let job = |key: &str| {
        golden::list(&jobs)
            .iter()
            .find(|job| golden::string(golden::get(job, "key")) == key)
            .cloned()
            .unwrap_or_else(|| panic!("job {key}"))
    };
    let wechat = job("wechat");
    assert_eq!(golden::string(golden::get(golden::get(&wechat, "last_run"), "status")), "failed");
    assert_ne!(golden::get(&wechat, "next_run_at"), &Value::Null);
    let pixiv = job("pixiv");
    assert_eq!(golden::get(&pixiv, "next_run_at"), &Value::Null);
    assert_eq!(golden::get(&pixiv, "last_run"), &Value::Null);
    harness.finish().await;
}

// py: tests/test_mcp.py::test_merge_replaces_lists_and_merges_objects
#[test]
fn merge_replaces_lists_and_merges_objects() {
    let dict = |text: &str| pyjson::loads(text).expect("JSON").as_dict().expect("a dict").clone();
    assert_eq!(
        tools::merge(
            &dict(r#"{"a": {"b": 1, "c": 2}, "l": [1, 2]}"#),
            &dict(r#"{"a": {"c": 3}, "l": [9]}"#)
        ),
        dict(r#"{"a": {"b": 1, "c": 3}, "l": [9]}"#)
    );
}

// py: tests/test_mcp.py::test_list_runs_filters_and_redacts
#[tokio::test]
async fn list_runs_filters_and_redacts() {
    let harness = Harness::new().await;
    harness
        .store_control_request(
            &pyjson::loads(
                r#"{"id": 1, "kind": "trigger_job", "target": "wechat", "status": "failed", "requested_at": "2026-10-12T00:00:00+00:00",
                    "started_at": "2026-10-12T00:00:00+00:00", "error": "token supersecret123 rejected"}"#,
            )
            .expect("JSON"),
        )
        .await;
    let now = Utc.with_ymd_and_hms(2026, 10, 12, 0, 0, 0).single().expect("a time").fixed_offset();
    let query = RunQuery {
        job: "WeChat".to_owned(),
        status: "failed".to_owned(),
        kind: String::new(),
        since: Some(now),
        limit: 5000,
    };
    ops::journal::take();
    let runs = ops::list_runs(&harness.state.db, &query, &Redactor::new(["supersecret123".to_owned()]))
        .await
        .expect("runs");

    let statements = ops::journal::take();
    let last = statements.last().expect("a statement");
    assert!(
        last.sql.contains("WHERE target = $1 AND status = $2 AND requested_at >= $3"),
        "{}",
        last.sql
    );
    assert_eq!(
        last.params,
        vec![
            Param::Text("wechat".to_owned()),
            Param::Text("failed".to_owned()),
            Param::Time(now),
            Param::Int(i64::try_from(ops::MAX_LIMIT).expect("a limit")),
        ]
    );
    assert_eq!(golden::string(golden::get(&runs[0], "error")), "token supe•••• rejected");
    harness.finish().await;
}

// py: tests/test_mcp.py::test_list_runs_rejects_unknown_status
#[tokio::test]
async fn list_runs_rejects_unknown_status() {
    let harness = Harness::new().await;
    let query = RunQuery {
        status: "broken".to_owned(),
        limit: 20,
        ..RunQuery::default()
    };
    let result = ops::list_runs(&harness.state.db, &query, &Redactor::default()).await;
    assert!(matches!(result, Err(OpsError::Refused(_))), "{result:?}");
    harness.finish().await;
}

async fn store_event(harness: &Harness, msg_id: i64, status: &str) {
    sqlx::query(
        "INSERT INTO wechat_events (msg_id, create_time, work, message, status, attempts, last_error)
         VALUES ($1, $1, 'image', '{}'::jsonb, $2, 5, 'boom');",
    )
    .bind(msg_id)
    .bind(status)
    .execute(harness.state.db.pool())
    .await
    .expect("a WeChat event");
}

// py: tests/test_mcp.py::test_retry_wechat_events_resets_attempts
#[tokio::test]
async fn retry_wechat_events_resets_attempts() {
    let harness = Harness::new().await;
    for msg_id in [2, 5, 7] {
        store_event(&harness, msg_id, "failed").await;
    }
    ops::journal::take();
    let retried = ops::retry_wechat_events(&harness.state.db, Some(&[2, 5]), "failed")
        .await
        .expect("retried");

    let statements = ops::journal::take();
    let last = statements.last().expect("a statement");
    assert!(last.sql.contains("SET status = 'pending', attempts = 0"), "{}", last.sql);
    assert!(last.sql.contains("msg_id = ANY($2)"), "{}", last.sql);
    assert_eq!(last.params, vec![Param::Text("failed".to_owned()), Param::Ints(vec![2, 5])]);
    assert_eq!(retried, vec![Value::from(2_i64), Value::from(5_i64)]);
    let attempts: Vec<(i64, String, i32)> = sqlx::query_as("SELECT msg_id, status, attempts FROM wechat_events ORDER BY msg_id;")
        .fetch_all(harness.state.db.pool())
        .await
        .expect("events");
    assert_eq!(
        attempts,
        vec![
            (2, "pending".to_owned(), 0),
            (5, "pending".to_owned(), 0),
            (7, "failed".to_owned(), 5)
        ]
    );
    harness.finish().await;
}

// py: tests/test_mcp.py::test_retry_wechat_events_refuses_to_redo_every_done_event
#[tokio::test]
async fn retry_wechat_events_refuses_to_redo_every_done_event() {
    let harness = Harness::new().await;
    let result = ops::retry_wechat_events(&harness.state.db, None, "done").await;
    assert!(matches!(result, Err(OpsError::Refused(_))), "{result:?}");
    harness.finish().await;
}

// py: tests/test_mcp.py::test_set_wechat_state_validates_the_cursor
#[tokio::test]
async fn set_wechat_state_validates_the_cursor() {
    let harness = Harness::new().await;
    let db = &harness.state.db;
    assert!(matches!(
        ops::set_wechat_state(db, "events_after", "abc").await,
        Err(OpsError::Refused(_))
    ));
    assert!(matches!(
        ops::set_wechat_state(db, "something_else", "1").await,
        Err(OpsError::Refused(_))
    ));

    harness
        .execute("INSERT INTO wechat_state (key, value) VALUES ('events_after', '120');", &[])
        .await;
    let result = ops::set_wechat_state(db, "events_after", "0").await.expect("set");
    assert_eq!(
        result,
        crate::json::dict([
            ("key", Value::from("events_after")),
            ("previous", Value::from("120")),
            ("value", Value::from("0"))
        ])
    );
    harness.finish().await;
}

// py: tests/test_mcp.py::test_parse_since_accepts_relative_and_iso
#[test]
fn parse_since_accepts_relative_and_iso() {
    let now = Utc::now();
    assert_eq!(ops::parse_since("", now), Ok(None));
    assert_eq!(
        ops::parse_since("2026-10-12T17:00:00Z", now),
        Ok(Some(
            Utc.with_ymd_and_hms(2026, 10, 12, 17, 0, 0)
                .single()
                .expect("a time")
                .fixed_offset()
        ))
    );
    let relative = ops::parse_since("2h", now).expect("parsed").expect("a time");
    let ago = Utc::now() - relative.to_utc();
    assert!(
        chrono::TimeDelta::minutes(119) < ago && ago < chrono::TimeDelta::minutes(121),
        "{ago}"
    );
    assert!(matches!(ops::parse_since("yesterday", now), Err(OpsError::Refused(_))));
}

// py: tests/test_mcp.py::test_read_logs_groups_tracebacks_filters_and_skips_stale_files
#[test]
fn read_logs_groups_tracebacks_filters_and_skips_stale_files() {
    let directory = tempfile::tempdir().expect("a directory");
    let entry = |name: &str, text: &str, mtime: &str| {
        crate::json::dict([
            ("name", Value::from(name)),
            ("text", Value::from(text)),
            ("mtime", Value::from(mtime)),
        ])
    };
    write_logs(
        directory.path(),
        &[
            entry(
                "20261012.log",
                "2026-10-12 17:00:00,100 - embyx.wechat - INFO - run started\n\
                 2026-10-12 17:00:01,200 - embyx.wechat - ERROR - save failed token=supersecret123\n\
                 Traceback (most recent call last):\n  File \"x.py\", line 1\n\
                 2026-10-12 17:00:02,300 - embyx.pixiv - WARNING - slow\n",
                "2026-10-12T17:00:03+00:00",
            ),
            entry(
                "20261001.log",
                "2026-10-01 00:00:00,000 - embyx.wechat - ERROR - old\n",
                "2026-10-01T00:00:00+00:00",
            ),
        ],
    );
    let since = Utc
        .with_ymd_and_hms(2026, 10, 12, 16, 0, 0)
        .single()
        .expect("a time")
        .fixed_offset();
    let query = LogQuery {
        since: Some(since),
        logger_name: "wechat".to_owned(),
        min_level: "INFO".to_owned(),
        limit: 200,
        ..LogQuery::default()
    };
    let result = ops::read_logs(directory.path(), &query, Tz::UTC, &Redactor::new(["supersecret123".to_owned()])).expect("logs");

    assert_eq!(str_list(golden::get(&result, "files")), ["20261012.log"]);
    let records = golden::list(golden::get(&result, "records"));
    let first_lines: Vec<String> = records
        .iter()
        .map(|record| {
            golden::string(golden::get(record, "message"))
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(first_lines, ["run started", "save failed token=supe••••"]);
    assert!(golden::string(golden::get(&records[1], "message")).contains("Traceback"));

    let query = LogQuery {
        min_level: "ERROR".to_owned(),
        limit: 1,
        ..LogQuery::default()
    };
    let errors = ops::read_logs(directory.path(), &query, Tz::UTC, &Redactor::default()).expect("logs");
    assert_eq!(golden::int(golden::get(&errors, "matched")), 2);
    assert_eq!(golden::get(&errors, "truncated"), &Value::Bool(true));
    let message = golden::string(golden::get(&golden::list(golden::get(&errors, "records"))[0], "message"));
    assert!(message.starts_with("save failed"), "{message}");
}

// py: tests/test_mcp.py::test_collect_secrets_includes_masked_settings_and_api_token
#[tokio::test]
async fn collect_secrets_includes_masked_settings_and_api_token() {
    let harness = Harness::new().await;
    harness
        .store_settings("web.wechat", &pyjson::loads(r#"{"token": "wechat-token-value"}"#).expect("JSON"))
        .await;
    let secrets = ops::collect_secrets(&harness.state).await.expect("secrets");
    assert!(secrets.iter().any(|secret| secret == "wechat-token-value"), "{}", secrets.len());
    assert!(secrets.iter().any(|secret| secret == TOKEN));
    harness.finish().await;
}

// ---------- what only the Rust side covers ----------

#[test]
fn bodies_decode_as_json_loads_detects_them() {
    let utf16: Vec<u8> = "\u{feff}{\"a\": 1}".encode_utf16().flat_map(u16::to_le_bytes).collect();
    assert_eq!(super::decode_body(&utf16).as_deref(), Some("{\"a\": 1}"));
    let utf16_be: Vec<u8> = "{}".encode_utf16().flat_map(u16::to_be_bytes).collect();
    assert_eq!(super::decode_body(&utf16_be).as_deref(), Some("{}"));
    let utf32: Vec<u8> = "[1]".chars().flat_map(|ch| u32::from(ch).to_le_bytes()).collect();
    assert_eq!(super::decode_body(&utf32).as_deref(), Some("[1]"));
    assert_eq!(super::decode_body(b"\xef\xbb\xbf{}").as_deref(), Some("{}"));
    assert_eq!(super::decode_body(b"{\"a\":\"\xe4\xb8x\"}"), None);
}

#[tokio::test]
async fn undecodable_and_oversized_bodies_are_refused() {
    // Python: 500 with no detail for a UnicodeDecodeError; 413 from the body limit.
    let harness = Harness::new().await;
    let request = |body: Vec<u8>| crate::router::Request {
        method: "POST".to_owned(),
        path: "/mcp".to_owned(),
        query_string: String::new(),
        query: Vec::new(),
        headers: JSON_HEADERS
            .iter()
            .map(|(name, value)| {
                (
                    axum::http::HeaderName::from_static(name),
                    axum::http::HeaderValue::from_static(value),
                )
            })
            .collect(),
        body: body.into(),
    };
    let response = super::handle(&harness.state, &request(b"{\"a\":\"\xe4\xb8x\"}".to_vec())).await;
    assert_eq!(response.status(), 500);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("a body");
    assert_eq!(
        body,
        r#"{"jsonrpc":"2.0","id":"server-error","error":{"code":-32603,"message":"Error handling POST request"}}"#
    );
    let response = super::handle(&harness.state, &request(vec![b' '; super::MAX_BODY_BYTES + 1])).await;
    assert_eq!(response.status(), 413);
    assert!(response.headers().get("content-type").is_none());
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("a body");
    assert_eq!(body, "Request body too large");
    harness.finish().await;
}

#[tokio::test]
async fn the_body_limit_matches_the_decoded_path() {
    // `RequestBodyLimitMiddleware` sits on the mount, which matches `scope["path"]`.
    let Harness { database, state } = Harness::new().await;
    let state = Arc::new(state);
    let body = vec![b' '; crate::router::MAX_BODY_BYTES + 1];
    for path in ["/mcp", "/%6Dcp"] {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(axum::body::Body::from(body.clone()))
            .expect("a request");
        let response = crate::router::serve(axum::extract::State(Arc::clone(&state)), request).await;
        // Past the router's own limit too: the MCP answer, not the API's 400.
        assert_eq!(response.status(), 413, "{path}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("a body");
        assert_eq!(body, "Request body too large", "{path}");
    }
    database.drop_database().await;
}

#[tokio::test]
async fn get_opens_an_event_stream_of_pings() {
    let harness = Harness::new().await;
    let answer_headers = |response: &axum::response::Response, name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("accept", axum::http::HeaderValue::from_static("text/event-stream"));
    let request = crate::router::Request {
        method: "GET".to_owned(),
        path: "/mcp".to_owned(),
        query_string: String::new(),
        query: Vec::new(),
        headers,
        body: axum::body::Bytes::new(),
    };
    let response = super::handle(&harness.state, &request).await;
    assert_eq!(response.status(), 200);
    assert_eq!(answer_headers(&response, "content-type").as_deref(), Some("text/event-stream"));
    assert_eq!(
        answer_headers(&response, "cache-control").as_deref(),
        Some("no-cache, no-transform")
    );
    assert_eq!(answer_headers(&response, "x-accel-buffering").as_deref(), Some("no"));
    // `if last_event_id := ...`: an empty Last-Event-ID is no replay request.
    let mut request = request;
    request.headers.insert("last-event-id", axum::http::HeaderValue::from_static(""));
    let response = super::handle(&harness.state, &request).await;
    assert_eq!(answer_headers(&response, "content-type").as_deref(), Some("text/event-stream"));
    harness.finish().await;
}
