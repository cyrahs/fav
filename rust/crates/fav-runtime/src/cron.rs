//! Cron triggers with APScheduler 3.11's `CronTrigger.from_crontab` semantics.
//!
//! Python origin: `apscheduler/triggers/cron/{__init__,fields,expressions}.py`, which
//! `run.py` (`_sync_scheduled_jobs`) and `src/api/ops.py` (`next_fire_time`) use. This is
//! not standard cron, and stored `cron` settings depend on the differences:
//!
//! - day-of-week 0 is Monday and 6 is Sunday (`* * * * 1` fires on Tuesdays); 7 is refused;
//! - month and weekday names match a prefix (`jan/3` is `jan`), and `last` in the day field
//!   is the month's last day;
//! - the search walks the fields the way APScheduler does, on Python's wall-clock datetime
//!   arithmetic, so DST gaps and repeats resolve exactly as in Python: a time in a gap
//!   keeps the offset from before the gap (`02:30-05:00` on a spring-forward day in New
//!   York), and a daily job in a fall-back hour fires at both the summer and the winter
//!   occurrence.
//!
//! [`ZonedTime`] is the small part of Python's aware `datetime` (PEP 495 `fold`, intra-zone
//! comparison by wall time) the algorithm relies on. `rust/goldens/scheduler/cron_next_fire.json`
//! holds the Python results this module is checked against.
//!
//! Two deliberate differences (`rust/docs/DIVERGENCES.md`): [`CronTrigger::next_fire_time_after`]
//! never returns a time that is not after the previous one, where APScheduler can repeat or
//! go back around a fall-back (D3); and numbers and names are ASCII only, where Python's
//! `\d` and case-insensitive `[a-z]` also take other scripts' digits and a few letters.

use std::cmp::Ordering;
use std::fmt;

use chrono::{
    DateTime, Datelike as _, Duration, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, Offset as _, TimeZone as _, Timelike as _, Utc,
};
use chrono_tz::Tz;
use fav_core::pyjson::py_isoformat_datetime;
use fav_core::pytext::py_split;

/// A cron expression APScheduler refuses; the message is Python's `ValueError` text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CronError(pub String);

// ---------- Python's aware datetime ----------

/// An aware datetime as Python holds one: wall-clock fields, a `fold` for the second
/// occurrence of a repeated hour, and a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZonedTime {
    naive: NaiveDateTime,
    fold: bool,
    zone: Tz,
}

impl ZonedTime {
    /// `datetime.fromtimestamp(ts, zone)` / `zone.fromutc(...)`: the wall time at an instant.
    pub fn from_utc(instant: DateTime<Utc>, zone: Tz) -> Self {
        let utc = instant.naive_utc();
        let offset = zone.offset_from_utc_datetime(&utc).fix();
        let naive = utc + Duration::seconds(i64::from(offset.local_minus_utc()));
        let fold = match zone.offset_from_local_datetime(&naive) {
            LocalResult::Ambiguous(first, second) => ambiguous_pair(first.fix(), second.fix()).1 == offset,
            _ => false,
        };
        Self { naive, fold, zone }
    }

    /// `datetime(..., tzinfo=zone, fold=fold)`.
    pub fn new(naive: NaiveDateTime, fold: bool, zone: Tz) -> Self {
        Self { naive, fold, zone }
    }

    /// `strftime('%Z')`: the zone's abbreviation at this time (`UTC`, `CST`, `+08`).
    pub fn tzname(&self) -> String {
        use chrono_tz::OffsetName as _;
        let offset = self.zone.offset_from_utc_datetime(&self.to_utc().naive_utc());
        offset.abbreviation().map_or_else(|| offset.fix().to_string(), str::to_owned)
    }

    pub fn now(zone: Tz) -> Self {
        Self::from_utc(Utc::now(), zone)
    }

    pub fn naive(&self) -> NaiveDateTime {
        self.naive
    }

    pub fn fold(&self) -> bool {
        self.fold
    }

    pub fn zone(&self) -> Tz {
        self.zone
    }

    /// `utcoffset()`, resolving a repeated or skipped wall time the way `zoneinfo` does.
    pub fn utc_offset(&self) -> FixedOffset {
        offset_for(self.zone, self.naive, self.fold)
    }

    /// The instant this wall time stands for.
    pub fn to_utc(&self) -> DateTime<Utc> {
        (self.naive - Duration::seconds(i64::from(self.utc_offset().local_minus_utc()))).and_utc()
    }

    /// `isoformat()`: `2026-11-01T01:15:00-05:00`, microseconds only when present.
    pub fn isoformat(&self) -> String {
        py_isoformat_datetime(&self.to_fixed())
    }

    /// The wall time with its offset.
    pub fn to_fixed(&self) -> DateTime<FixedOffset> {
        let offset = self.utc_offset();
        DateTime::from_naive_utc_and_offset(self.to_utc().naive_utc(), offset)
    }

    /// `astimezone(zone)`: unchanged (fold included) in its own zone, converted otherwise.
    #[must_use]
    pub fn astimezone(&self, zone: Tz) -> Self {
        if zone == self.zone {
            *self
        } else {
            Self::from_utc(self.to_utc(), zone)
        }
    }

    /// `self + timedelta(microseconds=micros)`: wall-clock arithmetic, `fold` reset.
    fn plus_micros(&self, micros: i64) -> Self {
        Self {
            naive: self.naive + Duration::microseconds(micros),
            fold: false,
            zone: self.zone,
        }
    }

    /// `datetime_ceil`: up to the next whole second, or unchanged on one.
    fn ceil(&self) -> Self {
        let micros = i64::from(self.naive.nanosecond() / 1000);
        if micros > 0 { self.plus_micros(1_000_000 - micros) } else { *self }
    }

    /// Python's comparison: by wall time within one zone (ignoring `fold`), by instant across zones.
    fn py_cmp(&self, other: &Self) -> Ordering {
        if self.zone == other.zone {
            self.naive.cmp(&other.naive)
        } else {
            self.to_utc().cmp(&other.to_utc())
        }
    }
}

/// The offsets of an ambiguous wall time, first occurrence first (the earlier instant,
/// which has the larger offset).
fn ambiguous_pair(a: FixedOffset, b: FixedOffset) -> (FixedOffset, FixedOffset) {
    if a.local_minus_utc() >= b.local_minus_utc() {
        (a, b)
    } else {
        (b, a)
    }
}

fn offset_for(zone: Tz, naive: NaiveDateTime, fold: bool) -> FixedOffset {
    match zone.offset_from_local_datetime(&naive) {
        LocalResult::Single(offset) => offset.fix(),
        LocalResult::Ambiguous(a, b) => {
            let (first, second) = ambiguous_pair(a.fix(), b.fix());
            if fold { second } else { first }
        }
        // A skipped wall time: `fold=0` takes the offset in force before the gap, `fold=1`
        // the one after. Offsets are under a day and transitions are months apart, so the
        // offsets a day either side are those two.
        LocalResult::None => {
            let probe = if fold {
                naive + Duration::days(1)
            } else {
                naive - Duration::days(1)
            };
            zone.offset_from_utc_datetime(&probe).fix()
        }
    }
}

// ---------- fields and expressions ----------

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// `CronTrigger.FIELD_NAMES`, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Year,
    Month,
    Day,
    Week,
    DayOfWeek,
    Hour,
    Minute,
    Second,
}

const FIELDS: [Field; 8] = [
    Field::Year,
    Field::Month,
    Field::Day,
    Field::Week,
    Field::DayOfWeek,
    Field::Hour,
    Field::Minute,
    Field::Second,
];

impl Field {
    fn name(self) -> &'static str {
        match self {
            Self::Year => "year",
            Self::Month => "month",
            Self::Day => "day",
            Self::Week => "week",
            Self::DayOfWeek => "day_of_week",
            Self::Hour => "hour",
            Self::Minute => "minute",
            Self::Second => "second",
        }
    }

    /// `REAL`: whether the field is a component `datetime(...)` takes.
    fn real(self) -> bool {
        !matches!(self, Self::Week | Self::DayOfWeek)
    }

    fn min_value(self) -> i64 {
        match self {
            Self::Year => 1970,
            Self::Month | Self::Day | Self::Week => 1,
            Self::DayOfWeek | Self::Hour | Self::Minute | Self::Second => 0,
        }
    }

    /// `MAX_VALUES`, before the day field's per-month limit.
    fn max_value(self) -> i64 {
        match self {
            Self::Year => 9999,
            Self::Month => 12,
            Self::Day => 31,
            Self::Week => 53,
            Self::DayOfWeek => 6,
            Self::Hour => 23,
            Self::Minute | Self::Second => 59,
        }
    }

    fn max_at(self, date: &NaiveDateTime) -> i64 {
        match self {
            Self::Day => days_in_month(date.year(), date.month()),
            other => other.max_value(),
        }
    }

    fn value_at(self, date: &NaiveDateTime) -> i64 {
        match self {
            Self::Year => i64::from(date.year()),
            Self::Month => i64::from(date.month()),
            Self::Day => i64::from(date.day()),
            Self::Week => i64::from(date.iso_week().week()),
            Self::DayOfWeek => i64::from(date.weekday().num_days_from_monday()),
            Self::Hour => i64::from(date.hour()),
            Self::Minute => i64::from(date.minute()),
            Self::Second => i64::from(date.second()),
        }
    }
}

fn days_in_month(year: i32, month: u32) -> i64 {
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    match (
        NaiveDate::from_ymd_opt(year, month, 1),
        NaiveDate::from_ymd_opt(next_year, next_month, 1),
    ) {
        (Some(first), Some(next)) => (next - first).num_days(),
        _ => 31,
    }
}

/// How a range prints: as numbers, or as month or weekday names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Names {
    Numbers,
    Months,
    Weekdays,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Expression {
    /// `*` or `*/step`.
    All { step: Option<i64> },
    /// `first`, `first-last`, `first/step`, `first-last/step`, or a month or weekday range.
    Range {
        first: i64,
        last: Option<i64>,
        step: Option<i64>,
        names: Names,
    },
    /// `last` in the day field.
    LastDayOfMonth,
}

impl Expression {
    fn next_value(&self, field: Field, date: &NaiveDateTime) -> Option<i64> {
        let start = field.value_at(date);
        let min = field.min_value();
        let max = field.max_at(date);
        match self {
            Self::All { step } => {
                let start = start.max(min);
                let next = match step {
                    Some(step) => start + (step - (start - min)).rem_euclid(*step),
                    None => start,
                };
                (next <= max).then_some(next)
            }
            Self::Range { first, last, step, .. } => {
                let min = min.max(*first);
                let max = last.map_or(max, |last| max.min(last));
                let mut next = min.max(start);
                if let Some(step) = step {
                    next += (step - (next - min)).rem_euclid(*step);
                }
                (next <= max).then_some(next)
            }
            Self::LastDayOfMonth => Some(days_in_month(date.year(), date.month())),
        }
    }
}

impl fmt::Display for Expression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::All { step: Some(step) } => write!(f, "*/{step}"),
            Self::All { step: None } => f.write_str("*"),
            Self::LastDayOfMonth => f.write_str("last"),
            Self::Range { first, last, step, names } => {
                let label = |value: i64| match names {
                    Names::Numbers => value.to_string(),
                    Names::Months => usize::try_from(value - 1)
                        .ok()
                        .and_then(|index| MONTHS.get(index))
                        .map_or_else(|| value.to_string(), |name| (*name).to_owned()),
                    Names::Weekdays => usize::try_from(value)
                        .ok()
                        .and_then(|index| WEEKDAYS.get(index))
                        .map_or_else(|| value.to_string(), |name| (*name).to_owned()),
                };
                match last {
                    Some(last) if last != first => write!(f, "{}-{}", label(*first), label(*last))?,
                    _ => f.write_str(&label(*first))?,
                }
                // Name ranges cannot carry a step (their pattern has none).
                if let Some(step) = step {
                    write!(f, "/{step}")?;
                }
                Ok(())
            }
        }
    }
}

/// A run of ASCII digits as Python's `int()` reads it; numbers beyond `i64` are refused.
fn parse_number(digits: &str) -> Result<i64, CronError> {
    digits
        .parse::<i64>()
        .map_err(|_| CronError(format!("Number too large in cron expression: {digits}")))
}

/// `(?P<first>\d+)(?:-(?P<last>\d+))?(?:/(?P<step>\d+))?$`
fn match_range(expr: &str) -> Option<(&str, Option<&str>, Option<&str>)> {
    let digits = |text: &str| text.bytes().take_while(u8::is_ascii_digit).count();
    let first_len = digits(expr);
    if first_len == 0 {
        return None;
    }
    let (first, mut rest) = expr.split_at(first_len);
    let mut last = None;
    if let Some(after) = rest.strip_prefix('-') {
        let len = digits(after);
        if len > 0 {
            last = Some(&after[..len]);
            rest = &after[len..];
        }
    }
    let mut step = None;
    if let Some(after) = rest.strip_prefix('/') {
        let len = digits(after);
        if len > 0 {
            step = Some(&after[..len]);
            rest = &after[len..];
        }
    }
    rest.is_empty().then_some((first, last, step))
}

/// `\*(?:/(?P<step>\d+))?$`: `None` when it does not match, else the optional step.
#[allow(clippy::option_option)] // reason: "no match" and "matched without a step" are both meaningful, as in the regex
fn match_all(expr: &str) -> Option<Option<&str>> {
    let rest = expr.strip_prefix('*')?;
    if rest.is_empty() {
        return Some(None);
    }
    let step = rest.strip_prefix('/')?;
    (!step.is_empty() && step.bytes().all(|b| b.is_ascii_digit())).then_some(Some(step))
}

/// `(?P<first>[a-z]+)(?:-(?P<last>[a-z]+))?` with IGNORECASE, matched at the start only.
fn match_names(expr: &str) -> Option<(&str, Option<&str>)> {
    let letters = |text: &str| text.bytes().take_while(u8::is_ascii_alphabetic).count();
    let first_len = letters(expr);
    if first_len == 0 {
        return None;
    }
    let (first, rest) = expr.split_at(first_len);
    let last = rest.strip_prefix('-').and_then(|after| {
        let len = letters(after);
        (len > 0).then(|| &after[..len])
    });
    Some((first, last))
}

/// `AllExpression.__init__` and `validate_range`.
fn all_expression(step: Option<&str>, field: Field) -> Result<Expression, ValidationStage> {
    let step = step.map(parse_number).transpose().map_err(ValidationStage::Construct)?;
    if step == Some(0) {
        return Err(ValidationStage::Construct(CronError("Increment must be higher than 0".to_owned())));
    }
    let range = field.max_value() - field.min_value();
    if let Some(step) = step
        && step > range
    {
        return Err(ValidationStage::Validate(format!(
            "the step value ({step}) is higher than the total range of the expression ({range})"
        )));
    }
    Ok(Expression::All { step })
}

/// `RangeExpression.__init__` and `validate_range`.
fn range_expression(first: i64, last: Option<i64>, step: Option<i64>, names: Names, field: Field) -> Result<Expression, ValidationStage> {
    if step == Some(0) {
        return Err(ValidationStage::Construct(CronError("Increment must be higher than 0".to_owned())));
    }
    let last = if last.is_none() && step.is_none() { Some(first) } else { last };
    if let Some(last) = last
        && first > last
    {
        return Err(ValidationStage::Construct(CronError(
            "The minimum value in a range must not be higher than the maximum".to_owned(),
        )));
    }
    let (min, max) = (field.min_value(), field.max_value());
    // AllExpression.validate_range runs first, against the field's whole range.
    if let Some(step) = step
        && step > max - min
    {
        return Err(ValidationStage::Validate(format!(
            "the step value ({step}) is higher than the total range of the expression ({})",
            max - min
        )));
    }
    if first < min {
        return Err(ValidationStage::Validate(format!(
            "the first value ({first}) is lower than the minimum value ({min})"
        )));
    }
    if let Some(last) = last
        && last > max
    {
        return Err(ValidationStage::Validate(format!(
            "the last value ({last}) is higher than the maximum value ({max})"
        )));
    }
    // `(self.last or MAX_VALUES[...])`: a last value of 0 counts as absent.
    let range = last.filter(|last| *last != 0).unwrap_or(max) - first;
    if let Some(step) = step
        && step > range
    {
        return Err(ValidationStage::Validate(format!(
            "the step value ({step}) is higher than the total range of the expression ({range})"
        )));
    }
    Ok(Expression::Range { first, last, step, names })
}

/// Where an expression was refused: in its constructor (message as is) or in
/// `validate_range` (wrapped by `compile_expression`).
enum ValidationStage {
    Construct(CronError),
    Validate(String),
}

fn name_index(table: &[&str], name: &str) -> Option<usize> {
    let lowered = name.to_ascii_lowercase();
    table.iter().position(|candidate| *candidate == lowered)
}

fn named_range(first: &str, last: Option<&str>, field: Field) -> Result<Expression, ValidationStage> {
    let (table, label, offset, names): (&[&str], &str, i64, Names) = if field == Field::Month {
        (&MONTHS, "month", 1, Names::Months)
    } else {
        (&WEEKDAYS, "weekday", 0, Names::Weekdays)
    };
    let lookup = |name: &str| {
        name_index(table, name)
            .and_then(|index| i64::try_from(index).ok())
            .map(|index| index + offset)
            .ok_or_else(|| ValidationStage::Construct(CronError(format!("Invalid {label} name \"{name}\""))))
    };
    let first = lookup(first)?;
    let last = last.map(lookup).transpose()?;
    range_expression(first, last, None, names, field)
}

/// `BaseField.compile_expression`: the first pattern that matches decides.
fn compile_expression(expr: &str, field: Field) -> Result<Expression, CronError> {
    let attempt = if let Some(step) = match_all(expr) {
        Some(all_expression(step, field))
    } else if let Some((first, last, step)) = match_range(expr) {
        let numbers = (|| {
            Ok((
                parse_number(first)?,
                last.map(parse_number).transpose()?,
                step.map(parse_number).transpose()?,
            ))
        })();
        Some(match numbers {
            Ok((first, last, step)) => range_expression(first, last, step, Names::Numbers, field),
            Err(error) => Err(ValidationStage::Construct(error)),
        })
    } else {
        match field {
            Field::Month | Field::DayOfWeek => match_names(expr).map(|(first, last)| named_range(first, last, field)),
            // `last` matches as a prefix (re.match, no `$`); the weekday-position pattern
            // needs a space, which a crontab field cannot hold.
            Field::Day if expr.get(..4).is_some_and(|head| head.eq_ignore_ascii_case("last")) => Some(Ok(Expression::LastDayOfMonth)),
            _ => None,
        }
    };
    match attempt {
        Some(Ok(expression)) => Ok(expression),
        Some(Err(ValidationStage::Construct(error))) => Err(error),
        Some(Err(ValidationStage::Validate(message))) => Err(CronError(format!("Error validating expression '{expr}': {message}"))),
        None => Err(CronError(format!(
            "Unrecognized expression \"{expr}\" for field \"{}\"",
            field.name()
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompiledField {
    field: Field,
    expressions: Vec<Expression>,
    is_default: bool,
}

impl CompiledField {
    fn compile(field: Field, exprs: &str, is_default: bool) -> Result<Self, CronError> {
        let expressions = exprs
            .trim()
            .split(',')
            .map(|expr| compile_expression(expr.trim_matches(' '), field))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            field,
            expressions,
            is_default,
        })
    }

    /// The smallest next value any expression allows.
    fn next_value(&self, date: &NaiveDateTime) -> Option<i64> {
        self.expressions
            .iter()
            .filter_map(|expression| expression.next_value(self.field, date))
            .min()
    }
}

impl fmt::Display for CompiledField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.expressions.iter().map(ToString::to_string).collect();
        f.write_str(&parts.join(","))
    }
}

// ---------- the trigger ----------

/// A parsed crontab line bound to a zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronTrigger {
    fields: Vec<CompiledField>,
    zone: Tz,
}

impl CronTrigger {
    /// `CronTrigger.from_crontab(expr, timezone=zone)`.
    pub fn from_crontab(expr: &str, zone: Tz) -> Result<Self, CronError> {
        let values: Vec<&str> = py_split(expr).collect();
        if values.len() != 5 {
            return Err(CronError(format!("Wrong number of fields; got {}, expected 5", values.len())));
        }
        let given = |field: Field| match field {
            Field::Minute => Some(values[0]),
            Field::Hour => Some(values[1]),
            Field::Day => Some(values[2]),
            Field::Month => Some(values[3]),
            Field::DayOfWeek => Some(values[4]),
            Field::Year | Field::Week | Field::Second => None,
        };
        let mut fields = Vec::with_capacity(FIELDS.len());
        for field in FIELDS {
            // Fields before the last given one default to `*`, the ones after it to their
            // minimum (`DEFAULT_VALUES`): only `second` is after `minute`.
            let compiled = match given(field) {
                Some(exprs) => CompiledField::compile(field, exprs, false)?,
                None if field == Field::Second => CompiledField::compile(field, "0", true)?,
                None => CompiledField::compile(field, "*", true)?,
            };
            fields.push(compiled);
        }
        Ok(Self { fields, zone })
    }

    pub fn zone(&self) -> Tz {
        self.zone
    }

    /// `get_next_fire_time(previous_fire_time, now)`, exactly as APScheduler computes it.
    pub fn next_fire_time(&self, previous: Option<&ZonedTime>, now: &ZonedTime) -> Option<ZonedTime> {
        let start = match previous {
            Some(previous) => {
                let bumped = previous.plus_micros(1);
                // min(now, bumped) keeps `now` unless `bumped` is smaller.
                let mut start = if bumped.py_cmp(now) == Ordering::Less { bumped } else { *now };
                if start.py_cmp(previous) == Ordering::Equal {
                    start = start.plus_micros(1);
                }
                start
            }
            None => *now,
        };
        let mut next = start.ceil().astimezone(self.zone);
        let mut fieldnum: isize = 0;
        while let Some(index) = usize::try_from(fieldnum).ok().filter(|index| *index < self.fields.len()) {
            let compiled = &self.fields[index];
            let current = compiled.field.value_at(&next.naive);
            match compiled.next_value(&next.naive) {
                None => (next, fieldnum) = self.increment_field_value(next, fieldnum - 1),
                Some(value) if value > current => {
                    if compiled.field.real() {
                        next = self.set_field_value(next, index, value);
                        fieldnum += 1;
                    } else {
                        (next, fieldnum) = self.increment_field_value(next, fieldnum);
                    }
                }
                Some(_) => fieldnum += 1,
            }
        }
        (fieldnum >= 0).then_some(next)
    }

    /// The next fire time after `previous` that is strictly later than it (D3).
    ///
    /// Where APScheduler would hand back `previous` again or an earlier time (a fall-back
    /// hour in a DST zone), the search restarts one second after `previous`.
    pub fn next_fire_time_after(&self, previous: &ZonedTime, now: &ZonedTime) -> Option<ZonedTime> {
        let next = self.next_fire_time(Some(previous), now)?;
        if next.to_utc() > previous.to_utc() {
            return Some(next);
        }
        let restart = ZonedTime::from_utc(previous.to_utc() + Duration::seconds(1), self.zone);
        self.next_fire_time(None, &restart)
    }

    /// `_increment_field_value`: bump field `fieldnum` (carrying into the ones before it)
    /// and reset the ones after it to their minimum.
    fn increment_field_value(&self, date: ZonedTime, mut fieldnum: isize) -> (ZonedTime, isize) {
        let mut values = [0_i64; 8];
        let mut i: isize = 0;
        while let Ok(index) = usize::try_from(i) {
            let Some(compiled) = self.fields.get(index) else { break };
            let field = compiled.field;
            if !field.real() {
                if i == fieldnum {
                    fieldnum -= 1;
                    i -= 1;
                } else {
                    i += 1;
                }
                continue;
            }
            match i.cmp(&fieldnum) {
                Ordering::Less => {
                    values[index] = field.value_at(&date.naive);
                    i += 1;
                }
                Ordering::Greater => {
                    values[index] = field.min_value();
                    i += 1;
                }
                Ordering::Equal => {
                    let value = field.value_at(&date.naive);
                    if value == field.max_at(&date.naive) {
                        fieldnum -= 1;
                        i -= 1;
                    } else {
                        values[index] = value + 1;
                        i += 1;
                    }
                }
            }
        }
        if fieldnum < 0 {
            // The year ran past 9999: there is no next fire time (Python walks on through
            // negative indices, but its caller stops on the negative field number all the same).
            return (date, -1);
        }
        let Some(target) = naive_from_fields(&values) else {
            return (date, -1);
        };
        let difference = target - date.naive;
        (ZonedTime::from_utc(date.to_utc() + difference, self.zone), fieldnum)
    }

    /// `_set_field_value`: set field `index`, keep the ones before, reset the ones after.
    fn set_field_value(&self, date: ZonedTime, index: usize, value: i64) -> ZonedTime {
        let mut values = [0_i64; 8];
        for (i, compiled) in self.fields.iter().enumerate() {
            if !compiled.field.real() {
                continue;
            }
            values[i] = match i.cmp(&index) {
                Ordering::Less => compiled.field.value_at(&date.naive),
                Ordering::Greater => compiled.field.min_value(),
                Ordering::Equal => value,
            };
        }
        let naive = naive_from_fields(&values).unwrap_or(date.naive);
        ZonedTime::new(naive, date.fold, self.zone)
    }
}

/// `datetime(year, month, day, hour, minute, second)` from the field values (by `FIELDS` index).
fn naive_from_fields(values: &[i64; 8]) -> Option<NaiveDateTime> {
    let part = |index: usize| u32::try_from(values[index]).ok();
    NaiveDate::from_ymd_opt(i32::try_from(values[0]).ok()?, part(1)?, part(2)?)?.and_hms_opt(part(5)?, part(6)?, part(7)?)
}

/// `str(trigger)`: `cron[month='*', day='*', day_of_week='*', hour='*/6', minute='0']`.
/// The worker compares these to decide whether a job needs rescheduling.
impl fmt::Display for CronTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let options: Vec<String> = self
            .fields
            .iter()
            .filter(|compiled| !compiled.is_default)
            .map(|compiled| format!("{}='{compiled}'", compiled.field.name()))
            .collect();
        write!(f, "cron[{}]", options.join(", "))
    }
}

#[cfg(test)]
mod tests;
