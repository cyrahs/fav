//! fav's logging: a colored `[name]message` console line and a dated file under `log/`.
//!
//! Python origin: `src/core/logger.py`. Loggers are named `embyx.<name>` (the name the
//! Python app has always used) and filtered by `LOG_LEVEL`; records from other crates
//! arrive through `tracing` and, like Python's root logger, only from WARNING up. The
//! file layout is parsed by the MCP log tools (`src/api/ops.py`), so it is kept exactly:
//! `2025-11-10 16:00:00,000 - embyx.run - INFO - message`. Never log secrets.

use std::cell::RefCell;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Local, TimeZone};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

/// The logger every fav logger is a child of.
pub const APP_LOGGER: &str = "embyx";

/// Python's logging levels plus fav's own NOTICE (25).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Debug = 10,
    Info = 20,
    Notice = 25,
    Warning = 30,
    Error = 40,
    Critical = 50,
}

impl Level {
    pub const ALL: [Self; 6] = [Self::Debug, Self::Info, Self::Notice, Self::Warning, Self::Error, Self::Critical];

    /// The level a `LOG_LEVEL` value names (already normalized by `Env`).
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Notice => "NOTICE",
            Self::Warning => "WARNING",
            Self::Error => "ERROR",
            Self::Critical => "CRITICAL",
        }
    }

    pub fn number(self) -> u8 {
        self as u8
    }

    /// colorlog's default colors, with NOTICE in cyan.
    fn color(self) -> &'static str {
        match self {
            Self::Debug => "\x1b[37m",
            Self::Info => "\x1b[32m",
            Self::Notice => "\x1b[36m",
            Self::Warning => "\x1b[33m",
            Self::Error => "\x1b[31m",
            Self::Critical => "\x1b[1;31m",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One log record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub level: Level,
    pub message: String,
    pub time: DateTime<chrono::FixedOffset>,
}

/// `%(asctime)s - %(name)s - %(levelname)s - %(message)s`.
///
/// The time is rendered the way Python 3.12 renders it, from a float of seconds: the
/// seconds are floored and the milliseconds are `int((ct - int(ct)) * 1000)` of that float,
/// which is often one below the exact value and near a boundary can disagree with them.
pub fn format_file_line(record: &Record) -> String {
    format!(
        "{} - {} - {} - {}",
        python_asctime(&record.time),
        record.name,
        record.level,
        record.message
    )
}

/// `%Y-%m-%d %H:%M:%S,mmm` from `time.time()`'s float, as `logging.Formatter` makes it.
// Copying Python's float arithmetic, losses included, is the point here.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn python_asctime(time: &DateTime<chrono::FixedOffset>) -> String {
    let nanos = time.timestamp_nanos_opt().unwrap_or(i64::MAX);
    let created = nanos as f64 / 1e9;
    let msecs = ((created - created.trunc()) * 1000.0) as i64;
    let seconds = created.floor() as i64;
    let whole = DateTime::from_timestamp(seconds, 0).map_or(*time, |utc| utc.with_timezone(time.offset()));
    format!("{},{msecs:03}", whole.format("%Y-%m-%d %H:%M:%S"))
}

/// `%(log_color)s[%(name)s]%(message)s` plus a reset, as colorlog renders it.
pub fn format_console_line(record: &Record, color: bool) -> String {
    if color {
        format!("{}[{}]{}\x1b[0m", record.level.color(), record.name, record.message)
    } else {
        format!("[{}]{}", record.name, record.message)
    }
}

/// A named logger: `get("jandan")` logs as `embyx.jandan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logger {
    name: String,
}

/// The child logger `embyx.<name>`.
pub fn get(name: &str) -> Logger {
    Logger {
        name: format!("{APP_LOGGER}.{name}"),
    }
}

impl Logger {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn enabled(&self, level: Level) -> bool {
        level >= app_level()
    }

    pub fn log(&self, level: Level, message: impl fmt::Display) {
        if self.enabled(level) {
            emit(&self.name, level, &message.to_string());
        }
    }

    pub fn debug(&self, message: impl fmt::Display) {
        self.log(Level::Debug, message);
    }

    pub fn info(&self, message: impl fmt::Display) {
        self.log(Level::Info, message);
    }

    pub fn notice(&self, message: impl fmt::Display) {
        self.log(Level::Notice, message);
    }

    pub fn warning(&self, message: impl fmt::Display) {
        self.log(Level::Warning, message);
    }

    pub fn error(&self, message: impl fmt::Display) {
        self.log(Level::Error, message);
    }

    pub fn critical(&self, message: impl fmt::Display) {
        self.log(Level::Critical, message);
    }
}

/// Where records go once [`init`] ran. Before that (and in tests) they are dropped,
/// except into an active [`capture`].
#[derive(Debug)]
struct Sink {
    level: Level,
    file: Option<Mutex<File>>,
    console: bool,
    color: bool,
}

static SINK: OnceLock<Sink> = OnceLock::new();

thread_local! {
    static CAPTURED: RefCell<Option<Vec<Record>>> = const { RefCell::new(None) };
}

fn app_level() -> Level {
    SINK.get().map_or(Level::Debug, |sink| sink.level)
}

fn emit(name: &str, level: Level, message: &str) {
    let record = Record {
        name: name.to_owned(),
        level,
        message: message.to_owned(),
        time: Local::now().fixed_offset(),
    };
    let captured = CAPTURED.with(|captured| {
        let mut captured = captured.borrow_mut();
        captured.as_mut().map(|records| records.push(record.clone())).is_some()
    });
    if captured {
        return;
    }
    let Some(sink) = SINK.get() else { return };
    if sink.console {
        let line = format_console_line(&record, sink.color);
        // Like tqdm.write: one write per line, errors ignored.
        let _ = writeln!(io::stdout().lock(), "{line}");
    }
    if let Some(file) = &sink.file {
        let line = format_file_line(&record);
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }
}

/// Run `body` and return the records it logged on this thread instead of writing them.
pub fn capture(body: impl FnOnce()) -> Vec<Record> {
    let previous = CAPTURED.with(|captured| captured.replace(Some(Vec::new())));
    body();
    CAPTURED
        .with(|captured| std::mem::replace(&mut *captured.borrow_mut(), previous))
        .unwrap_or_default()
}

/// The file the process logs to: `<dir>/YYYYMMDD.log`, dated when it starts.
pub fn log_file_path(dir: &Path, now: &DateTime<impl TimeZone<Offset: fmt::Display>>) -> PathBuf {
    dir.join(format!("{}.log", now.format("%Y%m%d")))
}

/// Install the console and file outputs and route `tracing` events into them.
///
/// `dir` is `./log` in production. Colors follow colorlog: on unless `NO_COLOR` is set,
/// `FORCE_COLOR` winning. Calling it twice keeps the first setup.
pub fn init(level: Level, dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file_path(dir, &Local::now()))?;
    let color = std::env::var_os("FORCE_COLOR").is_some() || std::env::var_os("NO_COLOR").is_none();
    let _ = SINK.set(Sink {
        level,
        file: Some(Mutex::new(file)),
        console: true,
        color,
    });
    // Another subscriber may already be installed (tests); the fav loggers work regardless.
    let _ = tracing_subscriber::registry().with(Bridge).try_init();
    Ok(())
}

/// Forwards other crates' `tracing` events from WARNING up, named by their target.
#[derive(Debug, Clone, Copy)]
struct Bridge;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Bridge {
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _: Context<'_, S>) -> bool {
        *metadata.level() <= tracing::Level::WARN
    }

    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let level = if *event.metadata().level() == tracing::Level::ERROR {
            Level::Error
        } else {
            Level::Warning
        };
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        emit(event.metadata().target(), level, &visitor.finish());
    }
}

#[derive(Debug, Default)]
struct MessageVisitor {
    message: String,
    fields: Vec<String>,
}

impl MessageVisitor {
    fn finish(self) -> String {
        if self.fields.is_empty() {
            self.message
        } else {
            format!("{} {}", self.message, self.fields.join(" "))
        }
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields.push(format!("{}={value:?}", field.name()));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            value.clone_into(&mut self.message);
        } else {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden;

    fn record_from(case: &crate::pyjson::Value) -> Record {
        let nanos = golden::int(golden::get(case, "time_ns"));
        let time = DateTime::from_timestamp_nanos(i64::try_from(nanos).expect("fits")).fixed_offset();
        Record {
            name: golden::string(golden::get(case, "name")),
            level: Level::from_name(&golden::string(golden::get(case, "level"))).expect("known level"),
            message: golden::string(golden::get(case, "message")),
            time,
        }
    }

    #[test]
    fn lines_match_python_formatters() {
        let document = golden::load("logging/format.json");
        for case in golden::cases(&document) {
            let record = record_from(case);
            assert_eq!(format_file_line(&record), golden::string(golden::get(case, "file")));
            assert_eq!(format_console_line(&record, true), golden::string(golden::get(case, "console")));
        }
    }

    #[test]
    fn level_numbers_match_python() {
        let document = golden::load("logging/levels.json");
        let cases = golden::cases(&document);
        assert_eq!(cases.len(), Level::ALL.len());
        for case in cases {
            let level = Level::from_name(&golden::string(golden::get(case, "name"))).expect("known level");
            assert_eq!(u32::from(level.number()), golden::uint(golden::get(case, "number")));
        }
        for name in crate::env::LOG_LEVELS {
            assert!(Level::from_name(name).is_some(), "{name}");
        }
    }

    #[test]
    fn loggers_are_children_of_embyx() {
        assert_eq!(get("jandan").name(), "embyx.jandan");
    }

    #[test]
    fn capture_collects_records_on_this_thread() {
        let log = get("capture");
        let records = capture(|| {
            log.notice("Saved a.mp4");
            log.info(format_args!("{} items", 3));
        });
        let summary: Vec<(String, Level, String)> = records
            .into_iter()
            .map(|record| (record.name, record.level, record.message))
            .collect();
        assert_eq!(
            summary,
            [
                ("embyx.capture".to_owned(), Level::Notice, "Saved a.mp4".to_owned()),
                ("embyx.capture".to_owned(), Level::Info, "3 items".to_owned())
            ]
        );
    }

    #[test]
    fn log_file_is_named_by_start_date() {
        let time = DateTime::from_timestamp(1_762_790_400, 0).expect("valid");
        assert_eq!(log_file_path(Path::new("log"), &time), Path::new("log/20251110.log"));
    }
}
