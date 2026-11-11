//! Python's text decoding and JSON parsing of response bodies, with Python's error texts:
//! `bytes.decode('utf-8')`, `json.loads(str)` and `json.loads(bytes)` (what httpx's
//! `Response.json()` does), plus the numeric and form helpers the Bilibili client needs.
//! Failures are [`PyError`]s, a class and a message; [`crate::pyerr`] gives the same
//! decoders a [`JobError`] face for sources that only report them.
//!
//! `fav-api`'s MCP endpoint keeps its own copy of the byte-encoding detection
//! (`docs/rewrite/PLAN.md`, cleanup notes).

use std::fmt::Write as _;

use fav_core::pyjson::{self, JsonEncoding, Value, detect_json_encoding};
use fav_runtime::source::JobError;

use crate::pyerr::type_name;

/// A Python exception: its class name and `str(exc)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyError {
    pub class: String,
    pub message: String,
}

impl PyError {
    pub(crate) fn new(class: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            class: class.into(),
            message: message.into(),
        }
    }

    /// The exception as the run reports it.
    pub(crate) fn into_job_error(self) -> JobError {
        JobError::new(self.class, self.message)
    }
}

/// `bytes.decode('utf-8')`: strict, with `UnicodeDecodeError`'s text.
pub(crate) fn decode_utf8(bytes: &[u8]) -> Result<&str, PyError> {
    std::str::from_utf8(bytes).map_err(|error| {
        let start = error.valid_up_to();
        let first = bytes[start];
        let (length, reason) = match error.error_len() {
            _ if matches!(first, 0x80..=0xC1 | 0xF5..=0xFF) => (1, "invalid start byte"),
            None => (bytes.len() - start, "unexpected end of data"),
            Some(length) => (length, "invalid continuation byte"),
        };
        let message = if length == 1 {
            format!("'utf-8' codec can't decode byte 0x{first:02x} in position {start}: {reason}")
        } else {
            format!(
                "'utf-8' codec can't decode bytes in position {start}-{}: {reason}",
                start + length - 1
            )
        };
        PyError::new("UnicodeDecodeError", message)
    })
}

/// `json.loads(text)`, failing with `JSONDecodeError`'s text.
pub(crate) fn json_loads(text: &str) -> Result<Value, PyError> {
    if text.starts_with('\u{feff}') {
        return Err(PyError::new(
            "JSONDecodeError",
            "Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)",
        ));
    }
    pyjson::loads(text).map_err(|error| PyError::new("JSONDecodeError", error.python_message(text)))
}

/// `json.loads(body)` for bytes: the encoding `json.detect_encoding` picks, then the parse.
/// Both failures are `ValueError`s in Python (`UnicodeDecodeError`, `JSONDecodeError`).
pub(crate) fn json_from_bytes(body: &[u8]) -> Result<Value, PyError> {
    let text = detect_and_decode(body)?;
    // A BOM json.loads accepts (`utf-8-sig`, UTF-16/32) is gone after decoding.
    pyjson::loads(&text).map_err(|error| PyError::new("JSONDecodeError", error.python_message(&text)))
}

fn detect_and_decode(body: &[u8]) -> Result<String, PyError> {
    match detect_json_encoding(body) {
        JsonEncoding::Utf8(bytes) => decode_utf8(bytes).map(str::to_owned),
        // D68: the codec's own text is not reproduced.
        JsonEncoding::Decoded(text) => {
            text.ok_or_else(|| PyError::new("UnicodeDecodeError", "the body is not valid in the encoding json.loads detected"))
        }
    }
}

/// `value == number` with Python's numeric equality: `0 == 0.0 == False`.
pub(crate) fn py_eq_int(value: &Value, number: i128) -> bool {
    match value {
        Value::Int(int) => *int == number,
        Value::Bool(flag) => i128::from(*flag) == number,
        #[allow(clippy::cast_precision_loss, clippy::float_cmp)] // reason: small numbers, compared exactly as Python does
        Value::Float(float) => *float == number as f64,
        _ => false,
    }
}

/// `int(value)` for a JSON value.
pub(crate) fn py_int(value: &Value) -> Result<i128, PyError> {
    match value {
        Value::Int(int) => Ok(*int),
        Value::Bool(flag) => Ok(i128::from(*flag)),
        Value::Float(float) => {
            if float.is_nan() {
                Err(PyError::new("ValueError", "cannot convert float NaN to integer"))
            } else if float.is_infinite() {
                Err(PyError::new("OverflowError", "cannot convert float infinity to integer"))
            } else {
                #[allow(clippy::cast_possible_truncation)] // reason: `int(float)` truncates; past i128 it saturates (D63)
                Ok(float.trunc() as i128)
            }
        }
        Value::Str(text) => fav_core::pyint::parse_int(text).map_err(|_| {
            PyError::new(
                "ValueError",
                format!("invalid literal for int() with base 10: {}", fav_runtime::pystr::repr_str(text)),
            )
        }),
        other => Err(PyError::new(
            "TypeError",
            format!(
                "int() argument must be a string, a bytes-like object or a real number, not '{}'",
                type_name(other)
            ),
        )),
    }
}

/// `urllib.parse.quote_plus(text)` (UTF-8, `safe=''`).
pub(crate) fn quote_plus(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b' ' => out.push('+'),
            _ if byte.is_ascii_alphanumeric() || b"_.-~".contains(&byte) => out.push(char::from(byte)),
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

/// `urlencode(pairs)`: what httpx sends for `params=` and `data=` dictionaries.
pub(crate) fn urlencode(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", quote_plus(key), quote_plus(value)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use fav_core::pyjson::Value;

    use super::{decode_utf8, json_from_bytes, json_loads, py_eq_int, py_int, quote_plus, urlencode};

    #[test]
    fn utf8_errors_read_like_python() {
        let message = |bytes: &[u8]| decode_utf8(bytes).expect_err("invalid").message;
        assert_eq!(
            message(b"\xff{"),
            "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte"
        );
        assert_eq!(
            message(b"ab\xe4\xb8\""),
            "'utf-8' codec can't decode bytes in position 2-3: invalid continuation byte"
        );
        assert_eq!(
            message(b"ab\xe4\x00"),
            "'utf-8' codec can't decode byte 0xe4 in position 2: invalid continuation byte"
        );
        assert_eq!(
            message(b"ab\xe4\xb8"),
            "'utf-8' codec can't decode bytes in position 2-3: unexpected end of data"
        );
    }

    #[test]
    fn json_reads_like_python() {
        assert_eq!(
            json_loads("\u{feff}{}").expect_err("bom").message.split(':').next(),
            Some("Unexpected UTF-8 BOM (decode using utf-8-sig)")
        );
        assert_eq!(json_from_bytes(b"\xef\xbb\xbf{}"), Ok(Value::Dict(fav_core::pyjson::Dict::new())));
        assert_eq!(json_from_bytes(b"\xff\xfe").expect_err("empty").class, "JSONDecodeError");
        assert_eq!(
            json_from_bytes(b"<html>").expect_err("html").message,
            "Expecting value: line 1 column 1 (char 0)"
        );
    }

    #[test]
    fn numbers_compare_and_convert_like_python() {
        assert!(py_eq_int(&Value::Float(0.0), 0));
        assert!(py_eq_int(&Value::Bool(false), 0));
        assert!(!py_eq_int(&Value::Str("0".to_owned()), 0));
        assert_eq!(py_int(&Value::Float(2.5)), Ok(2));
        assert_eq!(
            py_int(&Value::Str("x".to_owned())).expect_err("text").message,
            "invalid literal for int() with base 10: 'x'"
        );
    }

    #[test]
    fn forms_are_quoted_plus() {
        assert_eq!(quote_plus("a b*~/"), "a+b%2A~%2F");
        assert_eq!(
            urlencode(&[("context[ts]", "1".to_owned()), ("x", "值".to_owned())]),
            "context%5Bts%5D=1&x=%E5%80%BC"
        );
    }
}
