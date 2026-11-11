//! Python built-ins the post-link parsers (WP13) apply to JSON values and strings, with
//! Python's exceptions: `str()`, `float()`, `int()` before `datetime.fromtimestamp(...,
//! tz=UTC)`, iterating a value, and `urlsplit` raising `ValueError`.
//!
//! The answers these parsers read come from other sites, so a field can hold any JSON
//! type; Python's built-ins then either convert it or raise, and a raise ends the read
//! with that exception. These helpers give the same verdict and text
//! (`rust/goldens/post_links/parsing.json` holds Python's answers).

use chrono::{DateTime, Utc};
use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::{py_decimal_value, py_strip};
use fav_core::pyurl::{SplitResult, urlsplit};

use crate::pydecode::{PyError, py_int};
use crate::pyerr::type_name;

/// `str(value)`.
pub(crate) fn py_str(value: &Value) -> String {
    fav_runtime::pystr::str_value(value)
}

/// `str(value)` of an optional dictionary entry (`str(d.get(key))`, `'None'` when absent).
pub(crate) fn py_str_opt(value: Option<&Value>) -> String {
    value.map_or_else(|| "None".to_owned(), py_str)
}

/// `d.get(key) or ''` rendered with `str()`: the empty string for a missing or falsy value.
pub(crate) fn str_or_empty(value: Option<&Value>) -> String {
    match value {
        Some(value) if value.is_truthy() => py_str(value),
        _ => String::new(),
    }
}

/// `d.get(a) or d.get(b)`: the first truthy value, else the last one looked up.
pub(crate) fn first_truthy<'a>(dict: &'a Dict, keys: &[&str]) -> Option<&'a Value> {
    let mut last = None;
    for key in keys {
        last = dict.get(key);
        if last.is_some_and(Value::is_truthy) {
            return last;
        }
    }
    last
}

/// `isinstance(value, str) and value.startswith(('http://', 'https://'))`.
pub(crate) fn is_web_url(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Str(text)) if text.starts_with("http://") || text.starts_with("https://"))
}

/// The string of `value` if it is one (`isinstance(value, str)`).
pub(crate) fn as_str(value: Option<&Value>) -> Option<&str> {
    match value {
        Some(Value::Str(text)) => Some(text.as_str()),
        _ => None,
    }
}

/// The dictionaries `for item in value or []` meets, skipping everything else
/// (`if not isinstance(item, dict): continue`).
///
/// A falsy value is `[]`. A string yields its characters and a dictionary its keys, none of
/// them dictionaries; a truthy number or boolean is not iterable, so Python raises
/// `TypeError` there.
pub(crate) fn iter_dicts(value: Option<&Value>) -> Result<Vec<&Dict>, PyError> {
    match value {
        Some(Value::List(items)) => Ok(items.iter().filter_map(Value::as_dict).collect()),
        Some(other) if !other.is_truthy() || matches!(other, Value::Str(_) | Value::Dict(_)) => Ok(Vec::new()),
        None => Ok(Vec::new()),
        Some(other) => Err(not_iterable(other)),
    }
}

/// `TypeError: '<type>' object is not iterable`.
pub(crate) fn not_iterable(value: &Value) -> PyError {
    PyError::new("TypeError", format!("'{}' object is not iterable", type_name(value)))
}

/// `urlsplit(url)`, whose only failure is `ValueError` (a malformed IPv6 host).
pub(crate) fn split(url: &str) -> Result<SplitResult, PyError> {
    urlsplit(url, "").map_err(|error| PyError::new("ValueError", error.0))
}

/// `urljoin(base, url)`, failing as `urlsplit` does.
pub(crate) fn join(base: &str, url: &str) -> Result<String, PyError> {
    fav_core::pyurl::urljoin(base, url).map_err(|error| PyError::new("ValueError", error.0))
}

/// `sys.get_int_max_str_digits()`.
const INT_MAX_STR_DIGITS: usize = 4300;

/// `int(text)` for a string `str.isdigit()` accepted, as the argument of
/// `datetime.fromtimestamp`: a superscript digit is `int()`'s `ValueError`, more than
/// 4300 digits its digit-limit `ValueError`, and a number past 128 bits is past `time_t`
/// too, so it is that `OverflowError` (`fromtimestamp` would raise it next).
pub(crate) fn digit_timestamp(text: &str) -> Result<i128, PyError> {
    let digits = text.chars().count();
    if digits > INT_MAX_STR_DIGITS {
        return Err(PyError::new(
            "ValueError",
            format!(
                "Exceeds the limit ({INT_MAX_STR_DIGITS} digits) for integer string conversion: value has {digits} digits; use sys.set_int_max_str_digits() to increase the limit"
            ),
        ));
    }
    match fav_core::pyint::parse_int(text) {
        Ok(value) => Ok(value),
        Err(fav_core::pyint::IntError::Overflow { .. }) => Err(time_t_overflow()),
        Err(fav_core::pyint::IntError::Invalid) => py_int(&Value::Str(text.to_owned())),
    }
}

fn time_t_overflow() -> PyError {
    PyError::new("OverflowError", "timestamp out of range for platform time_t")
}

/// `datetime.fromtimestamp(seconds, tz=UTC)` for an integer, with `CPython`'s failures on
/// Linux: past `time_t` an `OverflowError`, a year `gmtime` cannot hold in `tm_year`
/// (`EOVERFLOW`) an `OSError`, and a year outside 1..=9999 `datetime`'s `ValueError`.
pub(crate) fn fromtimestamp_utc(seconds: i128) -> Result<DateTime<Utc>, PyError> {
    let seconds = i64::try_from(seconds).map_err(|_| time_t_overflow())?;
    let days = seconds.div_euclid(86_400);
    let year = civil_year(days);
    if i32::try_from(year - 1900).is_err() {
        return Err(PyError::new("OSError", "[Errno 75] Value too large for defined data type"));
    }
    if !(1..=9999).contains(&year) {
        return Err(PyError::new("ValueError", format!("year {year} is out of range")));
    }
    DateTime::from_timestamp(seconds, 0).ok_or_else(|| PyError::new("ValueError", format!("year {year} is out of range")))
}

/// The proleptic Gregorian year of a day count from 1970-01-01 (Howard Hinnant's
/// `civil_from_days`), in 128 bits so no `time_t` overflows it.
fn civil_year(days: i64) -> i128 {
    let z = i128::from(days) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    yoe + era * 400 + i128::from(month <= 2)
}

/// `float(text)`: Python's whitespace stripped, Unicode decimal digits read as ASCII,
/// single underscores between digits dropped, then the float grammar (`inf`,
/// `infinity` and `nan` in any case, with a sign).
fn float_str(text: &str) -> Option<f64> {
    let stripped = py_strip(text);
    let chars: Vec<char> = stripped
        .chars()
        .map(|ch| py_decimal_value(ch).and_then(|digit| char::from_digit(digit, 10)).unwrap_or(ch))
        .collect();
    let mut cleaned = String::with_capacity(chars.len());
    for (index, &ch) in chars.iter().enumerate() {
        if ch == '_' {
            let between_digits = index > 0 && chars[index - 1].is_ascii_digit() && chars.get(index + 1).is_some_and(char::is_ascii_digit);
            if !between_digits {
                return None;
            }
            continue;
        }
        if !ch.is_ascii() {
            return None;
        }
        cleaned.push(ch);
    }
    cleaned.parse().ok()
}

/// `float(value)` for a JSON value.
pub(crate) fn float(value: &Value) -> Result<f64, PyError> {
    match value {
        Value::Bool(flag) => Ok(if *flag { 1.0 } else { 0.0 }),
        #[allow(clippy::cast_precision_loss)] // reason: `float(int)` rounds to the nearest double, as this does
        Value::Int(int) => Ok(*int as f64),
        Value::Float(float) => Ok(*float),
        Value::Str(text) => float_str(text).ok_or_else(|| {
            PyError::new(
                "ValueError",
                format!("could not convert string to float: {}", fav_runtime::pystr::repr_str(text)),
            )
        }),
        other => Err(PyError::new(
            "TypeError",
            format!("float() argument must be a string or a real number, not '{}'", type_name(other)),
        )),
    }
}

/// `list(dict.fromkeys(items))`: the items in order, each once.
pub(crate) fn unique(items: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::with_capacity(items.len());
    items.into_iter().filter(|item| seen.insert(item.clone())).collect()
}

#[cfg(test)]
mod tests {
    use fav_core::pyjson::Value;

    use super::{float, fromtimestamp_utc};

    #[test]
    fn fromtimestamp_fails_like_cpython_on_linux() {
        let message = |seconds: i128| fromtimestamp_utc(seconds).map_err(|error| format!("{}: {}", error.class, error.message));
        assert_eq!(message(0).map(|time| time.to_rfc3339()), Ok("1970-01-01T00:00:00+00:00".to_owned()));
        assert_eq!(
            message(253_402_300_799).map(|time| time.to_rfc3339()),
            Ok("9999-12-31T23:59:59+00:00".to_owned())
        );
        assert_eq!(message(253_402_300_800), Err("ValueError: year 10000 is out of range".to_owned()));
        assert_eq!(message(1_000_000_000_000), Err("ValueError: year 33658 is out of range".to_owned()));
        assert_eq!(message(-100_000_000_000), Err("ValueError: year -1199 is out of range".to_owned()));
        assert_eq!(message(-62_135_596_801), Err("ValueError: year 0 is out of range".to_owned()));
        assert_eq!(
            message(-62_135_596_800).map(|time| time.to_rfc3339()),
            Ok("0001-01-01T00:00:00+00:00".to_owned())
        );
        assert_eq!(
            message(100_000_000_000_000_000),
            Err("OSError: [Errno 75] Value too large for defined data type".to_owned())
        );
        assert_eq!(
            message(i128::from(i64::MAX)),
            Err("OSError: [Errno 75] Value too large for defined data type".to_owned())
        );
        assert_eq!(
            message(i128::from(i64::MAX) + 1),
            Err("OverflowError: timestamp out of range for platform time_t".to_owned())
        );
    }

    #[test]
    fn float_reads_strings_as_python() {
        let read = |text: &str| float(&Value::Str(text.to_owned())).ok();
        assert_eq!(read(" 1_000.5 "), Some(1000.5));
        assert_eq!(read("\u{661}\u{662}.5"), Some(12.5));
        assert_eq!(read("-1e3"), Some(-1000.0));
        assert_eq!(read("1__0"), None);
        assert_eq!(read("_1"), None);
        assert_eq!(read("1_"), None);
        assert_eq!(read("abc"), None);
        assert!(read("-Infinity").is_some_and(f64::is_infinite));
    }
}
