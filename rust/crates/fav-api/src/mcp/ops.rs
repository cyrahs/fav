//! Runtime state and repair operations behind the MCP tools.
//!
//! Python origin: `src/api/ops.py`. Every query is a fixed statement, the same text as
//! Python's once its `?` placeholders are numbered (`rust/goldens/mcp/calls.json` records
//! the statements and parameters Python sent, and the replay compares them); there is
//! deliberately no way to run arbitrary SQL from here.
//!
//! An operation that refuses its arguments answers [`OpsError`]'s message, which the tool
//! shows as is; every other failure is the message of the exception Python would have
//! let through (`OverflowError`, a database error).

use std::collections::{BTreeSet, VecDeque};
use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use chrono::{DateTime, Datelike as _, FixedOffset, NaiveDate, NaiveDateTime, TimeDelta, Utc};
use chrono_tz::Tz;
use fav_core::pyjson::{self, Value};
use fav_core::pytext::{is_py_space, py_isdigit, py_strip};
use fav_db::Db;
use fav_runtime::control::{VALID_KINDS, VALID_STATUSES};
use fav_runtime::cron::{CronTrigger, ZonedTime};
use fav_runtime::jobs::ScheduledJob;
use fav_runtime::pystr::repr_str;
use regex::Regex;
use sqlx::Row as _;
use sqlx::postgres::PgRow;

use super::isoformat::{self, Parsed};
use crate::masking::MASK_SUFFIX;
use crate::{ApiState, json, masking};

pub(crate) const MAX_LIMIT: i128 = 500;
pub(crate) const MAX_LOG_RECORDS: i128 = 1000;
const TEXT_LIMIT: usize = 4000;
const LOG_MESSAGE_LIMIT: usize = 4000;
const MIN_SECRET_LENGTH: usize = 6;
const LOG_LEVELS: [(&str, i32); 6] = [
    ("DEBUG", 10),
    ("INFO", 20),
    ("NOTICE", 25),
    ("WARNING", 30),
    ("ERROR", 40),
    ("CRITICAL", 50),
];
static LOG_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}),(\d{3}) - (.+?) - ([A-Z]+) - (.*)$").expect("a valid pattern"));
/// One Unicode decimal digit (`\d` of a Python `str` pattern, `\p{Nd}` here).
static DECIMAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d$").expect("a valid pattern"));

pub(crate) const WECHAT_EVENT_STATUSES: [&str; 3] = ["pending", "done", "failed"];
pub(crate) const WECHAT_STATE_KEYS: [&str; 2] = ["events_after", "work_version"];
pub(crate) const NOTIFICATION_DELIVERY_STATUSES: [&str; 4] = ["pending", "sending", "delivered", "failed"];

/// A request the tools reject (`OpsError`), or the exception Python would have raised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpsError {
    /// `OpsError`: shown to the caller as is.
    Refused(String),
    /// Any other exception, by its Python type name and `str()`.
    Exception(&'static str, String),
}

impl OpsError {
    /// What the tool error shows after `Error executing tool <name>: `.
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Refused(message) | Self::Exception(_, message) => message,
        }
    }

    fn refused(message: impl Into<String>) -> Self {
        Self::Refused(message.into())
    }

    fn database(error: &sqlx::Error) -> Self {
        Self::Exception("DatabaseError", error.to_string())
    }
}

pub(crate) type OpsResult<T> = Result<T, OpsError>;

/// `bounded(limit, maximum)`: at least 1, at most `maximum`.
pub(crate) fn bounded(limit: i128, maximum: i128) -> i64 {
    // Both bounds fit 64 bits, so the clamped value does too.
    i64::try_from(limit.clamp(1, maximum)).unwrap_or(1)
}

/// `truncate(text, limit)`, counting code points as Python does.
pub(crate) fn truncate(text: &str, limit: usize) -> String {
    let length = text.chars().count();
    if length <= limit {
        return text.to_owned();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}… [{} more chars]", length - limit)
}

// ---------- time ----------

/// The value of one Unicode decimal digit. Unicode keeps every `Nd` digit in runs of ten
/// from its zero, so the value is the distance from the start of its run, modulo ten.
fn decimal_value(ch: char) -> Option<u32> {
    let is_decimal = |ch: char| DECIMAL.is_match(ch.encode_utf8(&mut [0; 4]));
    if !is_decimal(ch) {
        return None;
    }
    let mut start = u32::from(ch);
    while start > 0 && char::from_u32(start - 1).is_some_and(is_decimal) {
        start -= 1;
    }
    Some((u32::from(ch) - start) % 10)
}

/// `re.fullmatch(r'(\d+)\s*([mhd])', text)`: the amount (`None` when it is beyond 128
/// bits, which only matters for the overflow it causes) and the unit.
fn relative(text: &str) -> Option<(Option<i128>, char)> {
    let mut chars = text.chars().peekable();
    let mut amount: Option<i128> = Some(0);
    let mut digits = 0;
    while let Some(value) = chars.peek().copied().and_then(decimal_value) {
        amount = amount
            .and_then(|total| total.checked_mul(10))
            .and_then(|total| total.checked_add(i128::from(value)));
        digits += 1;
        chars.next();
    }
    if digits == 0 {
        return None;
    }
    while chars.peek().copied().is_some_and(is_py_space) {
        chars.next();
    }
    let unit = chars.next().filter(|unit| matches!(unit, 'm' | 'h' | 'd'))?;
    chars.next().is_none().then_some((amount, unit))
}

/// `datetime.now(UTC) - timedelta(<unit>=amount)`, with the errors `CPython` raises for an
/// amount the `timedelta` or the result cannot hold.
fn before(now: DateTime<Utc>, amount: Option<i128>, unit: char) -> OpsResult<DateTime<FixedOffset>> {
    let per_day = match unit {
        'm' => 1440,
        'h' => 24,
        _ => 1,
    };
    let days = amount.map(|amount| amount / per_day);
    let too_large = || OpsError::Exception("OverflowError", "Python int too large to convert to C int".to_owned());
    let days = days.filter(|days| *days <= i128::from(i32::MAX)).ok_or_else(too_large)?;
    if days > 999_999_999 {
        return Err(OpsError::Exception(
            "OverflowError",
            format!("days={days}; must have magnitude <= 999999999"),
        ));
    }
    // Under 10^9 days, so in range for both.
    let amount = i64::try_from(amount.unwrap_or_default()).map_err(|_| too_large())?;
    let delta = match unit {
        'm' => TimeDelta::try_minutes(amount),
        'h' => TimeDelta::try_hours(amount),
        _ => TimeDelta::try_days(amount),
    };
    let out_of_range = || OpsError::Exception("OverflowError", "date value out of range".to_owned());
    let moment = delta.and_then(|delta| now.checked_sub_signed(delta)).ok_or_else(out_of_range)?;
    if moment.year() < 1 {
        return Err(out_of_range());
    }
    Ok(moment.fixed_offset())
}

/// `parse_since`: an ISO 8601 timestamp (naive ones are UTC) or a relative
/// `<n>m`/`<n>h`/`<n>d` before `now`; blank is `None`.
pub(crate) fn parse_since(value: &str, now: DateTime<Utc>) -> OpsResult<Option<DateTime<FixedOffset>>> {
    let text = py_strip(value);
    if text.is_empty() {
        return Ok(None);
    }
    if let Some((amount, unit)) = relative(text) {
        return before(now, amount, unit).map(Some);
    }
    match isoformat::fromisoformat(text) {
        Some(Parsed::Aware(moment)) => Ok(Some(moment)),
        Some(Parsed::Naive(naive)) => Ok(Some(naive.and_utc().fixed_offset())),
        None => Err(OpsError::refused(format!(
            "Unrecognised time {}: use ISO 8601 (2026-10-12T17:00:00Z) or a relative 30m/6h/2d.",
            repr_str(value)
        ))),
    }
}

/// `local_timezone()`: `TZ`, stripped, or UTC when it is unset, blank or unknown. The
/// name is what `get_runtime` reports (`ZoneInfo.key`).
pub fn local_timezone(tz: Option<&str>) -> (Tz, String) {
    let name = tz.map(py_strip).filter(|name| !name.is_empty()).unwrap_or("UTC");
    name.parse::<Tz>()
        .map_or_else(|_| (Tz::UTC, "UTC".to_owned()), |zone| (zone, name.to_owned()))
}

/// `serialize_datetime` of a naive value, which Python reads as local time (`TZ`).
fn naive_local_z(value: &NaiveDateTime, zone: Tz) -> String {
    json::utc_z(&ZonedTime::new(*value, false, zone).to_utc())
}

// ---------- secrets ----------

fn walk_secrets(raw: &Value, masked: Option<&Value>, found: &mut BTreeSet<String>) {
    match (raw, masked) {
        (Value::Dict(raw), Some(Value::Dict(masked))) => {
            for (key, value) in raw.iter() {
                walk_secrets(value, masked.get(&fav_runtime::pystr::str_key(key)), found);
            }
        }
        (Value::List(raw), Some(Value::List(masked))) => {
            for (raw, masked) in raw.iter().zip(masked) {
                walk_secrets(raw, Some(masked), found);
            }
        }
        (Value::Str(text), masked) if masked != Some(raw) => {
            found.insert(text.clone());
        }
        _ => {}
    }
}

/// `urlsplit(dsn).password`: the text between the first `:` and the last `@` of the
/// network location, undecoded; `None` without a `//` location or a `:` in its user part.
pub(crate) fn dsn_password(dsn: &str) -> Option<String> {
    const SCHEME_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+-.";
    let dsn: String = dsn
        .trim_matches(|ch: char| ch <= ' ')
        .chars()
        .filter(|ch| !matches!(ch, '\t' | '\r' | '\n'))
        .collect();
    let mut rest = dsn.as_str();
    if let Some(colon) = rest.find(':')
        && colon > 0
        && rest.chars().next().is_some_and(|ch| ch.is_ascii_alphabetic())
        && rest[..colon].chars().all(|ch| SCHEME_CHARS.contains(ch))
    {
        rest = &rest[colon + 1..];
    }
    let location = rest.strip_prefix("//")?;
    let end = location.find(['/', '?', '#']).unwrap_or(location.len());
    let (userinfo, _) = location[..end].rsplit_once('@')?;
    let (_, password) = userinfo.split_once(':')?;
    Some(password.to_owned())
}

/// `collect_secrets`: every value the settings UI masks, `API_TOKEN` and the DSN
/// password, longest first. Equal lengths are ordered by text (Python leaves them in set
/// order).
pub(crate) async fn collect_secrets(state: &ApiState) -> OpsResult<Vec<String>> {
    let settings = state
        .settings
        .load()
        .await
        .map_err(|error| OpsError::Exception("DatabaseError", error.to_string()))?;
    let mut found = BTreeSet::new();
    for section in fav_core::settings::SECTIONS {
        let Ok(model) = settings.section(section) else {
            continue;
        };
        let raw = model.dump();
        let masked = match &raw {
            Value::Dict(dict) => Value::Dict(masking::mask_section(section, dict)),
            other => other.clone(),
        };
        walk_secrets(&raw, Some(&masked), &mut found);
    }
    found.extend(state.mcp.bootstrap_secrets.iter().cloned());
    let mut secrets: Vec<String> = found
        .into_iter()
        .filter(|secret| secret.chars().count() >= MIN_SECRET_LENGTH)
        .collect();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.chars().count()));
    Ok(secrets)
}

/// `Redactor`: each secret of at least six characters becomes its first four and `••••`.
#[derive(Clone, Default)]
pub(crate) struct Redactor {
    secrets: Vec<String>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor").field("secrets", &self.secrets.len()).finish()
    }
}

impl Redactor {
    pub(crate) fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        Self {
            secrets: secrets
                .into_iter()
                .filter(|secret| secret.chars().count() >= MIN_SECRET_LENGTH)
                .collect(),
        }
    }

    pub(crate) fn apply(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for secret in &self.secrets {
            if text.contains(secret.as_str()) {
                let shown: String = secret.chars().take(4).collect();
                text = text.replace(secret.as_str(), &format!("{shown}{MASK_SUFFIX}"));
            }
        }
        text
    }
}

/// `await redactor()`: a redactor over the current settings and bootstrap secrets.
pub(crate) async fn redactor(state: &ApiState) -> OpsResult<Redactor> {
    Ok(Redactor::new(collect_secrets(state).await?))
}

// ---------- statements ----------

/// A bound parameter of a fixed statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Param {
    Text(String),
    Int(i64),
    Time(DateTime<FixedOffset>),
    Ints(Vec<i64>),
}

/// One fixed statement with its parameters.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Statement {
    pub(crate) sql: String,
    pub(crate) params: Vec<Param>,
}

impl Statement {
    fn new(sql: impl Into<String>, params: Vec<Param>) -> Self {
        Self { sql: sql.into(), params }
    }
}

/// The statements this thread ran, for the tests that compare them with Python's.
#[cfg(test)]
pub(crate) mod journal {
    use std::cell::RefCell;

    use super::Statement;

    thread_local! {
        static JOURNAL: RefCell<Vec<Statement>> = const { RefCell::new(Vec::new()) };
    }

    pub(crate) fn record(statement: &Statement) {
        JOURNAL.with(|journal| journal.borrow_mut().push(statement.clone()));
    }

    pub(crate) fn take() -> Vec<Statement> {
        JOURNAL.with(|journal| std::mem::take(&mut *journal.borrow_mut()))
    }
}

async fn fetch(db: &Db, statement: Statement) -> OpsResult<Vec<PgRow>> {
    #[cfg(test)]
    journal::record(&statement);
    let mut query = sqlx::query(sqlx::AssertSqlSafe(statement.sql));
    for param in statement.params {
        query = match param {
            Param::Text(text) => query.bind(text),
            Param::Int(int) => query.bind(int),
            Param::Time(moment) => query.bind(moment.to_utc()),
            Param::Ints(ints) => query.bind(ints),
        };
    }
    query.fetch_all(db.pool()).await.map_err(|error| OpsError::database(&error))
}

fn column<'r, T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>>(row: &'r PgRow, name: &str) -> OpsResult<T> {
    row.try_get(name).map_err(|error| OpsError::database(&error))
}

/// `WHERE a = $1 AND b = $2`, or nothing.
fn where_clause(clauses: &[String]) -> String {
    if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    }
}

/// The 64-bit ids of a list; a larger one names no row (Python sends it as `numeric`).
fn int_ids(values: &[i128]) -> Vec<i64> {
    values.iter().filter_map(|value| i64::try_from(*value).ok()).collect()
}

fn list_of_ints(rows: &[PgRow], name: &str) -> OpsResult<Vec<Value>> {
    let mut ids = rows.iter().map(|row| column::<i64>(row, name)).collect::<OpsResult<Vec<_>>>()?;
    ids.sort_unstable();
    Ok(ids.into_iter().map(Value::from).collect())
}

// ---------- runtime ----------

/// Linux's `USER_HZ`, what `os.sysconf('SC_CLK_TCK')` answers on every architecture fav
/// runs on (reading it needs `libc`, and `unsafe` is not allowed here).
const CLOCK_TICKS_PER_SECOND: f64 = 100.0;

fn boot_time() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

/// `process_started_at(pid)`: field 22 of `/proc/<pid>/stat` after the boot time.
pub(crate) fn process_started_at(pid: u32) -> Option<DateTime<Utc>> {
    let boot = boot_time()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name in field 2 may contain spaces: split after its closing parenthesis.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let ticks: u64 = fields.get(19)?.parse().ok()?;
    // Python computes in floats; seconds since 1970 stay far below 2^53.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let micros = ((boot + ticks as f64 / CLOCK_TICKS_PER_SECOND) * 1e6).round() as i64;
    DateTime::from_timestamp_micros(micros)
}

/// Where the worker runs, for `get_runtime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerProcess {
    /// `fav serve`: the worker runs in this process.
    InProcess,
    /// `fav api`: a separate `fav worker` (or `fav serve`) process, found in `/proc`
    /// as Python finds its `run.py`.
    Separate,
}

/// `find_worker_pid`: another `fav` process running the worker (`worker`, `serve` or no
/// command, which is `serve`).
fn find_worker_pid() -> Option<u32> {
    let own = std::process::id();
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        if pid == own {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let argv: Vec<String> = raw
            .split(|byte| *byte == 0)
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        let is_fav = argv
            .first()
            .is_some_and(|program| Path::new(program).file_name().is_some_and(|name| name == "fav"));
        let command = argv.get(1).map_or("", String::as_str);
        if is_fav && matches!(command, "" | "worker" | "serve") {
            return Some(pid);
        }
    }
    None
}

fn env_or_none(name: &str) -> Value {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or(Value::Null, Value::Str)
}

/// `runtime_info`.
pub(crate) fn runtime_info(state: &ApiState) -> Value {
    let worker_started = match state.mcp.worker {
        WorkerProcess::InProcess => Some(process_started_at(std::process::id())),
        WorkerProcess::Separate => find_worker_pid().map(process_started_at),
    };
    json::dict([
        ("git_commit", env_or_none("FAV_GIT_COMMIT")),
        ("build_time", env_or_none("FAV_BUILD_TIME")),
        ("pod", env_or_none("HOSTNAME")),
        ("container_started_at", json::utc_z_opt(process_started_at(1).as_ref())),
        ("api_started_at", Value::Str(json::utc_z(&state.mcp.started_at))),
        ("worker_running", Value::Bool(worker_started.is_some())),
        ("worker_started_at", json::utc_z_opt(worker_started.flatten().as_ref())),
        ("timezone", Value::Str(state.mcp.timezone_name.clone())),
        ("now", Value::Str(json::utc_z(&(state.clock)()))),
    ])
}

/// `next_fire_time(cron)`: the next run in the scheduler's zone, or `None` for an
/// expression APScheduler refuses.
pub(crate) fn next_fire_time(cron: &str, zone: Tz, now: DateTime<Utc>) -> Value {
    let Ok(trigger) = CronTrigger::from_crontab(cron, zone) else {
        return Value::Null;
    };
    json::utc_z_opt(
        trigger
            .next_fire_time(None, &ZonedTime::from_utc(now, zone))
            .map(|next| next.to_utc())
            .as_ref(),
    )
}

// ---------- logs ----------

#[derive(Debug, Clone)]
struct LogRecord {
    time: DateTime<Utc>,
    logger: String,
    level: String,
    message: String,
}

fn level_number(level: &str) -> Option<i32> {
    LOG_LEVELS.iter().find(|(name, _)| *name == level).map(|(_, number)| *number)
}

fn ascii_digits(text: &str) -> String {
    text.chars()
        .map(|ch| match decimal_value(ch).and_then(|value| char::from_digit(value, 10)) {
            Some(digit) => digit,
            None => ch,
        })
        .collect()
}

/// `datetime.strptime(stamp, '%Y-%m-%d %H:%M:%S')` of a stamp the line pattern matched:
/// its field ranges, then the date's own checks, with Python's messages.
fn parse_stamp(stamp: &str, millis: &str) -> OpsResult<NaiveDateTime> {
    let ascii = ascii_digits(stamp);
    let number = |range: std::ops::Range<usize>| ascii.get(range).and_then(|text| text.parse::<u32>().ok()).unwrap_or(u32::MAX);
    let (year, month, day) = (number(0..4), number(5..7), number(8..10));
    let (hour, minute, second) = (number(11..13), number(14..16), number(17..19));
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 61 {
        return Err(OpsError::Exception(
            "ValueError",
            format!("time data {} does not match format '%Y-%m-%d %H:%M:%S'", repr_str(stamp)),
        ));
    }
    let year = i32::try_from(year).unwrap_or_default();
    let date = NaiveDate::from_ymd_opt(year, month, day);
    let Some(date) = date.filter(|_| year >= 1) else {
        let message = if year < 1 {
            format!("year {year} is out of range")
        } else {
            "day is out of range for month".to_owned()
        };
        return Err(OpsError::Exception("ValueError", message));
    };
    if second > 59 {
        return Err(OpsError::Exception("ValueError", "second must be in 0..59".to_owned()));
    }
    let millis = ascii_digits(millis).parse::<u32>().unwrap_or_default();
    date.and_hms_milli_opt(hour, minute, second, millis)
        .ok_or_else(|| OpsError::Exception("ValueError", "second must be in 0..59".to_owned()))
}

/// The lines of a text-mode file: universal newlines (`\n`, `\r\n`, `\r`), invalid UTF-8
/// replaced, read a line at a time.
struct Lines {
    reader: std::io::BufReader<std::fs::File>,
    pending: VecDeque<String>,
}

impl Lines {
    fn next_line(&mut self) -> std::io::Result<Option<String>> {
        loop {
            if let Some(line) = self.pending.pop_front() {
                return Ok(Some(line));
            }
            let mut chunk = Vec::new();
            if self.reader.read_until(b'\n', &mut chunk)? == 0 {
                return Ok(None);
            }
            let text = String::from_utf8_lossy(&chunk).into_owned();
            let (body, terminated) = match text.strip_suffix('\n') {
                Some(body) => (body.strip_suffix('\r').unwrap_or(body), true),
                None => (text.as_str(), false),
            };
            let mut pieces: Vec<String> = body.split('\r').map(str::to_owned).collect();
            if !terminated && body.ends_with('\r') {
                pieces.pop();
            }
            self.pending.extend(pieces);
        }
    }
}

/// `_iter_log_records`: one file's records; lines that are not a record continue the one
/// above, and those before the first record are dropped.
struct FileRecords {
    lines: Lines,
    zone: Tz,
    current: Option<LogRecord>,
    done: bool,
}

impl FileRecords {
    fn open(path: &Path, zone: Tz) -> OpsResult<Self> {
        let file = std::fs::File::open(path).map_err(|error| OpsError::Exception("OSError", error.to_string()))?;
        Ok(Self {
            lines: Lines {
                reader: std::io::BufReader::new(file),
                pending: VecDeque::new(),
            },
            zone,
            current: None,
            done: false,
        })
    }

    fn next_record(&mut self) -> OpsResult<Option<LogRecord>> {
        if self.done {
            return Ok(None);
        }
        loop {
            let line = self
                .lines
                .next_line()
                .map_err(|error| OpsError::Exception("OSError", error.to_string()))?;
            let Some(line) = line else {
                self.done = true;
                return Ok(self.current.take());
            };
            let Some(captures) = LOG_LINE.captures(&line) else {
                if let Some(current) = &mut self.current {
                    current.message.push('\n');
                    current.message.push_str(&line);
                }
                continue;
            };
            let naive = parse_stamp(&captures[1], &captures[2])?;
            let record = LogRecord {
                time: ZonedTime::new(naive, false, self.zone).to_utc(),
                logger: captures[3].to_owned(),
                level: captures[4].to_owned(),
                message: captures[5].to_owned(),
            };
            if let Some(previous) = self.current.replace(record) {
                return Ok(Some(previous));
            }
        }
    }
}

/// `read_logs`'s filters.
#[derive(Debug, Clone, Default)]
pub(crate) struct LogQuery {
    pub(crate) since: Option<DateTime<FixedOffset>>,
    pub(crate) until: Option<DateTime<FixedOffset>>,
    pub(crate) contains: String,
    pub(crate) logger_name: String,
    pub(crate) min_level: String,
    pub(crate) limit: i128,
}

/// A file's modification time as `datetime.fromtimestamp(st_mtime, UTC)`: microseconds,
/// rounded half to even.
fn modified(path: &Path) -> OpsResult<DateTime<Utc>> {
    let metadata = std::fs::metadata(path).map_err(|error| OpsError::Exception("OSError", error.to_string()))?;
    let time = metadata
        .modified()
        .map_err(|error| OpsError::Exception("OSError", error.to_string()))?;
    let nanos = match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => i128::try_from(after.as_nanos()).unwrap_or(i128::MAX),
        Err(before) => -i128::try_from(before.duration().as_nanos()).unwrap_or(i128::MAX),
    };
    let (micros, rest) = (nanos.div_euclid(1000), nanos.rem_euclid(1000));
    let micros = if rest > 500 || (rest == 500 && micros % 2 != 0) {
        micros + 1
    } else {
        micros
    };
    Ok(i64::try_from(micros)
        .ok()
        .and_then(DateTime::from_timestamp_micros)
        .unwrap_or_default())
}

/// `read_logs`: the newest `limit` records in the window, oldest first. A process keeps
/// writing to the file named after the day it started, so the files are chosen by their
/// modification time, not their names.
pub(crate) fn read_logs(log_dir: &Path, query: &LogQuery, zone: Tz, redact: &Redactor) -> OpsResult<Value> {
    let Some(level_floor) = level_number(&py_strip(&query.min_level).to_uppercase()) else {
        let names: Vec<&str> = LOG_LEVELS.iter().map(|(name, _)| *name).collect();
        return Err(OpsError::refused(format!(
            "Unknown level {}; use one of {}.",
            repr_str(&query.min_level),
            names.join(", ")
        )));
    };
    let needle = casefold(&query.contains);
    let logger_needle = casefold(&query.logger_name);
    let capacity = usize::try_from(bounded(query.limit, MAX_LOG_RECORDS)).unwrap_or(1);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if log_dir.is_dir() {
        let entries = std::fs::read_dir(log_dir).map_err(|error| OpsError::Exception("OSError", error.to_string()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension == "log") && path.is_file() {
                candidates.push(path);
            }
        }
    }
    candidates.sort();
    let mut files = Vec::new();
    for path in candidates {
        if query.since.is_none_or(|since| modified(&path).is_ok_and(|time| time >= since)) {
            files.push(path);
        }
    }

    // Each file is chronological: merge the streams, keeping only the matches kept, as
    // `heapq.merge` does (ties go to the earlier file).
    let mut streams = files
        .iter()
        .map(|path| FileRecords::open(path, zone))
        .collect::<OpsResult<Vec<_>>>()?;
    let mut heads = streams.iter_mut().map(FileRecords::next_record).collect::<OpsResult<Vec<_>>>()?;
    let mut matched: VecDeque<LogRecord> = VecDeque::with_capacity(capacity.min(1024));
    let mut total: i64 = 0;
    loop {
        let next = heads
            .iter()
            .enumerate()
            .filter_map(|(index, head)| head.as_ref().map(|record| (record.time, index)))
            .min();
        let Some((_, index)) = next else {
            break;
        };
        let Some(record) = heads[index].take() else {
            break;
        };
        heads[index] = streams[index].next_record()?;
        if query.since.is_some_and(|since| record.time < since)
            || query.until.is_some_and(|until| record.time > until)
            || level_number(&record.level).unwrap_or(0) < level_floor
            || (!logger_needle.is_empty() && !casefold(&record.logger).contains(&logger_needle))
            || (!needle.is_empty() && !casefold(&record.message).contains(&needle))
        {
            continue;
        }
        total += 1;
        if matched.len() == capacity {
            matched.pop_front();
        }
        matched.push_back(record);
    }

    let records: Vec<Value> = matched
        .iter()
        .map(|record| {
            json::dict([
                ("time", Value::Str(json::utc_z(&record.time))),
                ("logger", Value::Str(record.logger.clone())),
                ("level", Value::Str(record.level.clone())),
                ("message", Value::Str(redact.apply(&truncate(&record.message, LOG_MESSAGE_LIMIT)))),
            ])
        })
        .collect();
    let returned = i64::try_from(records.len()).unwrap_or(i64::MAX);
    Ok(json::dict([
        (
            "files",
            Value::List(
                files
                    .iter()
                    .map(|path| Value::Str(path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()))
                    .collect(),
            ),
        ),
        ("matched", Value::from(total)),
        ("returned", Value::from(returned)),
        ("truncated", Value::Bool(total > returned)),
        ("records", Value::List(records)),
    ]))
}

/// `str.casefold()`: full case folding, which for the text logs hold is lowercasing plus
/// the few characters whose folds differ from their lowercase forms.
fn casefold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            'ß' | 'ẞ' => out.push_str("ss"),
            'ς' => out.push('σ'),
            'ſ' => out.push('s'),
            'ﬅ' | 'ﬆ' => out.push_str("st"),
            _ => out.extend(ch.to_lowercase()),
        }
    }
    out
}

// ---------- control requests ----------

const RUN_FIELDS: &str = "id, kind, target, status, requested_at, started_at, finished_at, result, error";

/// `round(seconds, 1)`: Rust's precision formatting rounds the exact binary value half to
/// even, as Python's `round` does.
fn round_tenth(seconds: f64) -> f64 {
    format!("{seconds:.1}").parse().unwrap_or(seconds)
}

/// `_serialize_run`.
fn serialize_run(row: &PgRow, redact: Option<&Redactor>, full: bool) -> OpsResult<Value> {
    let started: Option<DateTime<Utc>> = column(row, "started_at")?;
    let finished: Option<DateTime<Utc>> = column(row, "finished_at")?;
    let apply = |text: String| redact.map_or_else(|| text.clone(), |redact| redact.apply(&text));
    let error = apply(column::<String>(row, "error")?);
    let result = apply(column::<String>(row, "result")?);
    let duration = match (started, finished) {
        (Some(started), Some(finished)) => {
            let micros = (finished - started).num_microseconds().unwrap_or(i64::MAX);
            // Python's total_seconds() divides the microseconds as a float.
            #[allow(clippy::cast_precision_loss)]
            let seconds = micros as f64 / 1e6;
            Value::Float(round_tenth(seconds))
        }
        _ => Value::Null,
    };
    let requested: DateTime<Utc> = column(row, "requested_at")?;
    Ok(json::dict([
        ("id", Value::from(column::<i64>(row, "id")?)),
        ("job", Value::Str(column(row, "target")?)),
        ("kind", Value::Str(column(row, "kind")?)),
        ("status", Value::Str(column(row, "status")?)),
        ("requested_at", Value::Str(json::utc_z(&requested))),
        ("started_at", json::utc_z_opt(started.as_ref())),
        ("finished_at", json::utc_z_opt(finished.as_ref())),
        ("duration_seconds", duration),
        ("result", Value::Str(if full { result } else { truncate(&result, 500) })),
        ("error", Value::Str(if full { error } else { truncate(&error, TEXT_LIMIT) })),
    ]))
}

/// `list_runs`'s filters.
#[derive(Debug, Clone, Default)]
pub(crate) struct RunQuery {
    pub(crate) job: String,
    pub(crate) status: String,
    pub(crate) kind: String,
    pub(crate) since: Option<DateTime<FixedOffset>>,
    pub(crate) limit: i128,
}

fn sorted_names(names: &[&str]) -> String {
    let mut names = names.to_vec();
    names.sort_unstable();
    names.join(", ")
}

/// `list_runs`: newest first.
pub(crate) async fn list_runs(db: &Db, query: &RunQuery, redact: &Redactor) -> OpsResult<Vec<Value>> {
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    let mut add = |clause: &str, param: Param| {
        params.push(param);
        clauses.push(format!("{clause} ${}", params.len()));
    };
    let job = py_strip(&query.job);
    if !job.is_empty() {
        add("target =", Param::Text(job.to_lowercase()));
    }
    if !py_strip(&query.status).is_empty() {
        if !VALID_STATUSES.contains(&query.status.as_str()) {
            return Err(OpsError::refused(format!(
                "Unknown status {}; use one of {}.",
                repr_str(&query.status),
                sorted_names(&VALID_STATUSES)
            )));
        }
        add("status =", Param::Text(query.status.clone()));
    }
    if !py_strip(&query.kind).is_empty() {
        if !VALID_KINDS.contains(&query.kind.as_str()) {
            return Err(OpsError::refused(format!(
                "Unknown kind {}; use one of {}.",
                repr_str(&query.kind),
                sorted_names(&VALID_KINDS)
            )));
        }
        add("kind =", Param::Text(query.kind.clone()));
    }
    if let Some(since) = query.since {
        add("requested_at >=", Param::Time(since));
    }
    let limit_position = params.len() + 1;
    params.push(Param::Int(bounded(query.limit, MAX_LIMIT)));
    let sql = format!(
        "SELECT {RUN_FIELDS} FROM control_requests {} ORDER BY requested_at DESC, id DESC LIMIT ${limit_position};",
        where_clause(&clauses)
    );
    let rows = fetch(db, Statement::new(sql, params)).await?;
    rows.iter().map(|row| serialize_run(row, Some(redact), false)).collect()
}

/// `get_run`: one run with its whole result and error, if it exists. An id beyond 64
/// bits names no row.
pub(crate) async fn get_run(db: &Db, run_id: i128, redact: &Redactor) -> OpsResult<Option<Value>> {
    let Ok(run_id) = i64::try_from(run_id) else {
        return Ok(None);
    };
    let sql = format!("SELECT {RUN_FIELDS} FROM control_requests WHERE id = $1;");
    let rows = fetch(db, Statement::new(sql, vec![Param::Int(run_id)])).await?;
    rows.first().map(|row| serialize_run(row, Some(redact), true)).transpose()
}

/// `last_runs_by_job`: each target's newest run, unredacted.
pub(crate) async fn last_runs_by_job(db: &Db) -> OpsResult<Vec<(String, Value)>> {
    let sql = format!("SELECT DISTINCT ON (target) {RUN_FIELDS} FROM control_requests ORDER BY target, requested_at DESC, id DESC;");
    let rows = fetch(db, Statement::new(sql, Vec::new())).await?;
    rows.iter()
        .map(|row| Ok((column::<String>(row, "target")?, serialize_run(row, None, false)?)))
        .collect()
}

/// `describe_jobs`: each job with its commands, next run and latest run.
pub(crate) fn describe_jobs(jobs: &[ScheduledJob], last_runs: &[(String, Value)], zone: Tz, now: DateTime<Utc>) -> Vec<Value> {
    jobs.iter()
        .map(|job| {
            let mut entry = match job.as_public_dict() {
                Value::Dict(dict) => dict,
                _ => pyjson::Dict::new(),
            };
            entry.insert("required_commands", json::str_list(&job.required_commands));
            entry.insert(
                "next_run_at",
                if job.enabled {
                    next_fire_time(&job.cron, zone, now)
                } else {
                    Value::Null
                },
            );
            let last = last_runs.iter().find(|(target, _)| *target == job.key).map(|(_, run)| run);
            let last = last.map_or(Value::Null, |run| {
                let mut picked = pyjson::Dict::new();
                for key in ["id", "kind", "status", "started_at", "finished_at", "error"] {
                    picked.insert(key, run.get(key).cloned().unwrap_or(Value::Null));
                }
                Value::Dict(picked)
            });
            entry.insert("last_run", last);
            Value::Dict(entry)
        })
        .collect()
}

// ---------- WeChat ----------

const EVENT_FIELDS: &str = "msg_id, create_time, work, seq, message, status, attempts, last_error, updated_at";

/// A `jsonb` value as the JSON text PostgreSQL prints, which is what psycopg receives and
/// `json.loads` reads.
struct JsonbText(String);

impl sqlx::Type<sqlx::Postgres> for JsonbText {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        sqlx::postgres::PgTypeInfo::with_name("jsonb")
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Postgres> for JsonbText {
    fn decode(value: sqlx::postgres::PgValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        let format = value.format();
        let bytes = value.as_bytes()?;
        // The binary form is a version byte (1) before the text.
        let text = match format {
            sqlx::postgres::PgValueFormat::Binary => bytes.strip_prefix(&[1]).ok_or("unsupported jsonb version")?,
            sqlx::postgres::PgValueFormat::Text => bytes,
        };
        Ok(Self(std::str::from_utf8(text)?.to_owned()))
    }
}

/// The `jsonb` column loaded the way `json.loads` loads it.
fn message_value(row: &PgRow) -> OpsResult<Value> {
    let text: JsonbText = column(row, "message")?;
    pyjson::loads(&text.0).map_err(|error| OpsError::Exception("JSONDecodeError", error.to_string()))
}

/// `_event_summary`.
fn event_summary(row: &PgRow, include_message: bool, redact: &Redactor, zone: Tz) -> OpsResult<Value> {
    let message = match message_value(row)? {
        Value::Str(text) => pyjson::loads(&text).unwrap_or_else(|_| Value::Dict(pyjson::Dict::new())),
        other => other,
    };
    let message = match message {
        Value::Dict(dict) => dict,
        _ => pyjson::Dict::new(),
    };
    let field = |key: &str| message.get(key).cloned().unwrap_or(Value::Null);
    let updated: Option<NaiveDateTime> = column(row, "updated_at")?;
    let mut summary = pyjson::Dict::new();
    summary.insert("msg_id", Value::from(column::<i64>(row, "msg_id")?));
    summary.insert("create_time", Value::from(column::<i64>(row, "create_time")?));
    summary.insert("work", Value::Str(column(row, "work")?));
    summary.insert("status", Value::Str(column(row, "status")?));
    summary.insert("attempts", Value::from(i64::from(column::<i32>(row, "attempts")?)));
    summary.insert("last_error", Value::Str(redact.apply(&column::<String>(row, "last_error")?)));
    summary.insert("seq", Value::from(column::<i64>(row, "seq")?));
    summary.insert("kind", field("kind"));
    summary.insert("talker", field("talker"));
    summary.insert("sender", field("sender"));
    summary.insert(
        "updated_at",
        updated.map_or(Value::Null, |updated| Value::Str(naive_local_z(&updated, zone))),
    );
    if include_message {
        summary.insert("message", Value::Dict(message));
    }
    Ok(Value::Dict(summary))
}

/// `wechat_status`: the source's state, event counts and latest failures.
pub(crate) async fn wechat_status(db: &Db, redact: &Redactor, recent_failures: i128, zone: Tz) -> OpsResult<Value> {
    let state_rows = fetch(db, Statement::new("SELECT key, value FROM wechat_state ORDER BY key;", Vec::new())).await?;
    let count_rows = fetch(
        db,
        Statement::new(
            "SELECT work, status, COUNT(*) AS count FROM wechat_events GROUP BY work, status ORDER BY work, status;",
            Vec::new(),
        ),
    )
    .await?;
    let failure_rows = fetch(
        db,
        Statement::new(
            format!(
                "SELECT {EVENT_FIELDS} FROM wechat_events WHERE status = 'failed' OR (status = 'pending' AND attempts > 0) ORDER BY updated_at DESC LIMIT $1;"
            ),
            vec![Param::Int(bounded(recent_failures, MAX_LIMIT))],
        ),
    )
    .await?;
    let mut state = pyjson::Dict::new();
    for row in &state_rows {
        state.insert(column::<String>(row, "key")?, Value::Str(column(row, "value")?));
    }
    let counts = count_rows
        .iter()
        .map(|row| {
            Ok(json::dict([
                ("work", Value::Str(column(row, "work")?)),
                ("status", Value::Str(column(row, "status")?)),
                ("count", Value::from(column::<i64>(row, "count")?)),
            ]))
        })
        .collect::<OpsResult<Vec<_>>>()?;
    let failures = failure_rows
        .iter()
        .map(|row| event_summary(row, false, redact, zone))
        .collect::<OpsResult<Vec<_>>>()?;
    Ok(json::dict([
        ("state", Value::Dict(state)),
        ("event_counts", Value::List(counts)),
        ("recent_failures", Value::List(failures)),
    ]))
}

fn unknown_event_status(status: &str) -> OpsError {
    OpsError::refused(format!(
        "Unknown status {}; use one of {}.",
        repr_str(status),
        WECHAT_EVENT_STATUSES.join(", ")
    ))
}

/// `list_wechat_events`'s filters.
#[derive(Debug, Clone, Default)]
pub(crate) struct EventQuery {
    pub(crate) status: String,
    pub(crate) work: String,
    pub(crate) msg_ids: Option<Vec<i128>>,
    pub(crate) include_message: bool,
    pub(crate) limit: i128,
}

/// `list_wechat_events`: newest first.
pub(crate) async fn list_wechat_events(db: &Db, query: &EventQuery, redact: &Redactor, zone: Tz) -> OpsResult<Vec<Value>> {
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    if !query.status.is_empty() {
        if !WECHAT_EVENT_STATUSES.contains(&query.status.as_str()) {
            return Err(unknown_event_status(&query.status));
        }
        params.push(Param::Text(query.status.clone()));
        clauses.push(format!("status = ${}", params.len()));
    }
    if !query.work.is_empty() {
        params.push(Param::Text(query.work.clone()));
        clauses.push(format!("work = ${}", params.len()));
    }
    if let Some(ids) = query.msg_ids.as_ref().filter(|ids| !ids.is_empty()) {
        params.push(Param::Ints(int_ids(ids)));
        clauses.push(format!("msg_id = ANY(${})", params.len()));
    }
    params.push(Param::Int(bounded(query.limit, MAX_LIMIT)));
    let sql = format!(
        "SELECT {EVENT_FIELDS} FROM wechat_events {} ORDER BY create_time DESC, msg_id DESC LIMIT ${};",
        where_clause(&clauses),
        params.len()
    );
    let rows = fetch(db, Statement::new(sql, params)).await?;
    rows.iter()
        .map(|row| event_summary(row, query.include_message, redact, zone))
        .collect()
}

/// `retry_wechat_events`: back to pending with a fresh attempt budget.
pub(crate) async fn retry_wechat_events(db: &Db, msg_ids: Option<&[i128]>, status: &str) -> OpsResult<Vec<Value>> {
    if !WECHAT_EVENT_STATUSES.contains(&status) {
        return Err(unknown_event_status(status));
    }
    let ids = msg_ids.filter(|ids| !ids.is_empty());
    if status == "done" && ids.is_none() {
        return Err(OpsError::refused(
            "Retrying every done event would redo the whole log; name the msg_ids instead.",
        ));
    }
    let mut params = vec![Param::Text(status.to_owned())];
    let mut id_clause = "";
    if let Some(ids) = ids {
        id_clause = " AND msg_id = ANY($2)";
        params.push(Param::Ints(int_ids(ids)));
    }
    let sql = format!(
        "UPDATE wechat_events SET status = 'pending', attempts = 0, last_error = '', updated_at = CURRENT_TIMESTAMP WHERE status = $1{id_clause} RETURNING msg_id;"
    );
    let rows = fetch(db, Statement::new(sql, params)).await?;
    list_of_ints(&rows, "msg_id")
}

/// `set_wechat_state`: the previous value and the new one.
pub(crate) async fn set_wechat_state(db: &Db, key: &str, value: &str) -> OpsResult<Value> {
    if !WECHAT_STATE_KEYS.contains(&key) {
        return Err(OpsError::refused(format!(
            "Unknown key {}; use one of {}.",
            repr_str(key),
            WECHAT_STATE_KEYS.join(", ")
        )));
    }
    let value = py_strip(value);
    if key == "events_after" && !py_isdigit(value) {
        return Err(OpsError::refused(
            "events_after is a bridge sequence number (a non-negative integer).",
        ));
    }
    let previous = fetch(
        db,
        Statement::new("SELECT value FROM wechat_state WHERE key = $1;", vec![Param::Text(key.to_owned())]),
    )
    .await?;
    fetch(
        db,
        Statement::new(
            "INSERT INTO wechat_state (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value;",
            vec![Param::Text(key.to_owned()), Param::Text(value.to_owned())],
        ),
    )
    .await?;
    let previous = previous.first().map(|row| column::<String>(row, "value")).transpose()?;
    Ok(json::dict([
        ("key", Value::from(key)),
        ("previous", previous.map_or(Value::Null, Value::Str)),
        ("value", Value::from(value)),
    ]))
}

// ---------- notifications ----------

/// `list_notifications`'s filters.
#[derive(Debug, Clone, Default)]
pub(crate) struct NotificationQuery {
    pub(crate) delivery_status: String,
    pub(crate) source: String,
    pub(crate) limit: i128,
}

/// `list_notifications`: the outbox, newest first.
pub(crate) async fn list_notifications(db: &Db, query: &NotificationQuery, redact: &Redactor) -> OpsResult<Vec<Value>> {
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    if !query.delivery_status.is_empty() {
        if !NOTIFICATION_DELIVERY_STATUSES.contains(&query.delivery_status.as_str()) {
            return Err(OpsError::refused(format!(
                "Unknown delivery_status {}; use one of {}.",
                repr_str(&query.delivery_status),
                NOTIFICATION_DELIVERY_STATUSES.join(", ")
            )));
        }
        params.push(Param::Text(query.delivery_status.clone()));
        clauses.push(format!("delivery_status = ${}", params.len()));
    }
    if !query.source.is_empty() {
        params.push(Param::Text(query.source.clone()));
        clauses.push(format!("source = ${}", params.len()));
    }
    params.push(Param::Int(bounded(query.limit, MAX_LIMIT)));
    let sql = format!(
        "SELECT id, kind, source, scope, dedupe_key, title, body, delivery_status, attempt_count, occurrence_count, last_error, created_at, next_attempt_at, delivered_at FROM notifications {} ORDER BY id DESC LIMIT ${};",
        where_clause(&clauses),
        params.len()
    );
    let rows = fetch(db, Statement::new(sql, params)).await?;
    rows.iter()
        .map(|row| {
            let created: DateTime<Utc> = column(row, "created_at")?;
            let next: DateTime<Utc> = column(row, "next_attempt_at")?;
            let delivered: Option<DateTime<Utc>> = column(row, "delivered_at")?;
            Ok(json::dict([
                ("id", Value::from(column::<i64>(row, "id")?)),
                ("kind", Value::Str(column(row, "kind")?)),
                ("source", Value::Str(column(row, "source")?)),
                ("scope", Value::Str(column(row, "scope")?)),
                ("dedupe_key", Value::Str(column(row, "dedupe_key")?)),
                ("title", Value::Str(column(row, "title")?)),
                ("body", Value::Str(truncate(&redact.apply(&column::<String>(row, "body")?), 1000))),
                ("delivery_status", Value::Str(column(row, "delivery_status")?)),
                ("attempt_count", Value::from(i64::from(column::<i32>(row, "attempt_count")?))),
                ("occurrence_count", Value::from(i64::from(column::<i32>(row, "occurrence_count")?))),
                ("last_error", Value::Str(redact.apply(&column::<String>(row, "last_error")?))),
                ("created_at", Value::Str(json::utc_z(&created))),
                ("next_attempt_at", Value::Str(json::utc_z(&next))),
                ("delivered_at", json::utc_z_opt(delivered.as_ref())),
            ]))
        })
        .collect()
}

/// `retry_notifications`: failed notifications back to pending with a fresh budget.
pub(crate) async fn retry_notifications(db: &Db, ids: &[i128]) -> OpsResult<Vec<Value>> {
    if ids.is_empty() {
        return Err(OpsError::refused("Name the notification ids to retry."));
    }
    let rows = fetch(
        db,
        Statement::new(
            "UPDATE notifications SET delivery_status = 'pending', attempt_count = 0, last_error = '', next_attempt_at = CURRENT_TIMESTAMP WHERE id = ANY($1) AND delivery_status = 'failed' RETURNING id;",
            vec![Param::Ints(int_ids(ids))],
        ),
    )
    .await?;
    list_of_ints(&rows, "id")
}

/// `site_sessions_summary`: stored logins without their cookie values.
pub(crate) async fn site_sessions_summary(db: &Db) -> OpsResult<Vec<Value>> {
    let stored = fav_runtime::site_sessions::load_all(db)
        .await
        .map_err(|error| OpsError::Exception("DatabaseError", error.to_string()))?;
    let iso = |value: Option<&DateTime<Utc>>| value.map_or(Value::Null, |value| Value::Str(json::isoformat_utc(value)));
    Ok(stored
        .values()
        .map(|session| {
            let names: BTreeSet<&str> = session.cookies.iter().map(|cookie| cookie.name.as_str()).collect();
            json::dict([
                ("site", Value::Str(session.site.clone())),
                ("account", Value::Str(session.account.clone())),
                ("status", Value::Str(session.status.clone())),
                ("message", Value::Str(session.message.clone())),
                ("cookie_names", json::str_list(&names.into_iter().collect::<Vec<_>>())),
                ("expires_at", iso(session.expires_at.as_ref())),
                ("signed_in_at", iso(session.signed_in_at.as_ref())),
                ("checked_at", iso(session.checked_at.as_ref())),
            ])
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{casefold, decimal_value, dsn_password, relative, round_tenth};

    #[test]
    fn decimal_digits_of_every_script_have_their_value() {
        assert_eq!(decimal_value('7'), Some(7));
        assert_eq!(decimal_value('٣'), Some(3));
        assert_eq!(decimal_value('０'), Some(0));
        // The mathematical digits are five runs of ten in a row.
        assert_eq!(decimal_value('\u{1D7D8}'), Some(0));
        assert_eq!(decimal_value('\u{1D7E1}'), Some(9));
        assert_eq!(decimal_value('²'), None);
        assert_eq!(decimal_value('x'), None);
    }

    #[test]
    fn relative_times_match_the_python_pattern() {
        assert_eq!(relative("10 m"), Some((Some(10), 'm')));
        assert_eq!(relative("10\u{1f}h"), Some((Some(10), 'h')));
        assert_eq!(relative("10mm"), None);
        assert_eq!(relative("m"), None);
        assert_eq!(relative("2H"), None);
    }

    #[test]
    fn dsn_passwords_are_found_as_urlsplit_finds_them() {
        assert_eq!(dsn_password("postgresql://fav:pw@db:5432/fav").as_deref(), Some("pw"));
        assert_eq!(dsn_password("postgresql://fav:p%40w:x@a@db/fav").as_deref(), Some("p%40w:x@a"));
        assert_eq!(dsn_password("postgresql://fav@db/fav"), None);
        assert_eq!(dsn_password("postgresql://db/fav?password=x"), None);
        assert_eq!(dsn_password("host=db user=fav password=kv"), None);
        assert_eq!(dsn_password("//u:pw@h").as_deref(), Some("pw"));
    }

    #[test]
    fn durations_round_like_python() {
        assert!((round_tenth(12.06) - 12.1).abs() < f64::EPSILON);
        assert!((round_tenth(0.25) - 0.2).abs() < f64::EPSILON);
        assert!((round_tenth(60.0) - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn casefold_folds_what_lowercase_does_not() {
        assert_eq!(casefold("Straße WeChat"), "strasse wechat");
    }
}
