//! The few Python `str()`/`repr()` renderings the runtime's messages contain.

use std::fmt::Write as _;

use fav_core::pyjson::{Key, Value, float_repr};
use fav_core::pytext::is_py_space;

/// `repr(text)` for a `str`: single quotes unless the text has a `'` and no `"`.
///
/// Escapes backslashes, the chosen quote, `\t` `\n` `\r`, other control characters and
/// the Unicode separators Python does not print (`isprintable()` is false for them).
pub fn repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            ch if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            ch if ch.is_control() || (ch != ' ' && is_py_space(ch)) || matches!(ch, '\u{200b}'..='\u{200f}' | '\u{feff}') => {
                let code = u32::from(ch);
                let _ = if code <= 0xff {
                    write!(out, "\\x{code:02x}")
                } else if code <= 0xffff {
                    write!(out, "\\u{code:04x}")
                } else {
                    write!(out, "\\U{code:08x}")
                };
            }
            ch => out.push(ch),
        }
    }
    out.push(quote);
    out
}

/// `str(value)` for a value `json.loads` produced.
pub fn str_value(value: &Value) -> String {
    match value {
        Value::Str(text) => text.clone(),
        other => repr_value(other),
    }
}

/// `str(key)` for a dictionary key.
pub fn str_key(key: &Key) -> String {
    match key {
        Key::Str(text) => text.clone(),
        other => repr_key(other),
    }
}

/// `repr(value)` for a value `json.loads` produced.
pub fn repr_value(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Int(int) => int.to_string(),
        Value::Float(float) => float_repr(*float),
        Value::Str(text) => repr_str(text),
        Value::List(items) => format!("[{}]", items.iter().map(repr_value).collect::<Vec<_>>().join(", ")),
        Value::Dict(dict) => format!(
            "{{{}}}",
            dict.iter()
                .map(|(key, item)| format!("{}: {}", repr_key(key), repr_value(item)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn repr_key(key: &Key) -> String {
    match key {
        Key::Str(text) => repr_str(text),
        Key::Int(int) => int.to_string(),
        Key::Float(float) => float_repr(*float),
        Key::Bool(true) => "True".to_owned(),
        Key::Bool(false) => "False".to_owned(),
        Key::Null => "None".to_owned(),
    }
}

/// `str(timedelta)`: `0:05:01.250000`, `1 day, 2:00:00`, `-1 day, 23:59:59`.
pub(crate) fn timedelta(delta: chrono::TimeDelta) -> String {
    let micros_total = delta.num_microseconds().unwrap_or(i64::MAX);
    let days = micros_total.div_euclid(86_400_000_000);
    let rest = micros_total.rem_euclid(86_400_000_000);
    let (seconds, micros) = (rest / 1_000_000, rest % 1_000_000);
    let mut out = String::new();
    if days != 0 {
        let plural = if days.abs() == 1 { "" } else { "s" };
        let _ = write!(out, "{days} day{plural}, ");
    }
    let _ = write!(out, "{}:{:02}:{:02}", seconds / 3600, (seconds / 60) % 60, seconds % 60);
    if micros != 0 {
        let _ = write!(out, ".{micros:06}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{repr_str, timedelta};

    #[test]
    fn repr_quotes_like_python() {
        assert_eq!(repr_str("nope"), "'nope'");
        assert_eq!(repr_str("it's"), "\"it's\"");
        assert_eq!(repr_str("it's \"x\""), "'it\\'s \"x\"'");
        assert_eq!(repr_str("a\tb\\c\u{3000}d\u{7f}é"), "'a\\tb\\\\c\\u3000d\\x7fé'");
    }

    #[test]
    fn timedelta_prints_like_python() {
        assert_eq!(timedelta(chrono::TimeDelta::microseconds(301_250_000)), "0:05:01.250000");
        assert_eq!(timedelta(chrono::TimeDelta::hours(26)), "1 day, 2:00:00");
        assert_eq!(timedelta(chrono::TimeDelta::days(3)), "3 days, 0:00:00");
        assert_eq!(timedelta(chrono::TimeDelta::seconds(-1)), "-1 day, 23:59:59");
    }
}
