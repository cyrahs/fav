//! Python exception classes and texts for failures the sources report.
//!
//! A run's failure reaches the failure notification and the job-request row as
//! `Class: message` (`run.py`'s `_format_exception`), and some sources store
//! `f'{exc.__class__.__name__}: {exc}'` in their own tables. These helpers name the class
//! Python would raise for the same failure and, where the text is Python's own (not a
//! driver's), reproduce it.

use fav_core::pyjson::Value;
use fav_net::download::DownloadError;
use fav_net::{StatusError, TransportError};
use fav_runtime::source::JobError;

/// psycopg's class for a database error: the SQLSTATE's own class where fav can meet it,
/// else the class of its SQLSTATE group; connection and pool failures are
/// `OperationalError`. The message is the server's (`str(exc)` in psycopg is too).
pub(crate) fn db_error(error: &fav_db::DbError) -> JobError {
    let fav_db::DbError::Sqlx(sqlx::Error::Database(database)) = error else {
        return JobError::new("OperationalError", error.to_string());
    };
    let code = database.code().map(std::borrow::Cow::into_owned).unwrap_or_default();
    let class = match code.as_str() {
        "22003" => "NumericValueOutOfRange",
        "22P02" => "InvalidTextRepresentation",
        "23505" => "UniqueViolation",
        "23502" => "NotNullViolation",
        "40P01" => "DeadlockDetected",
        "42P01" => "UndefinedTable",
        "42703" => "UndefinedColumn",
        "57014" => "QueryCanceled",
        _ => match code.get(..2) {
            Some("22") => "DataError",
            Some("23") => "IntegrityError",
            Some("42") => "ProgrammingError",
            Some("08" | "40" | "53" | "54" | "55" | "57" | "58") => "OperationalError",
            _ => "DatabaseError",
        },
    };
    JobError::new(class, database.message())
}

/// A transport failure: httpx's class, the cause's text, and the host for a run whose
/// message ends up empty.
pub(crate) fn transport_error(error: &TransportError) -> JobError {
    JobError::new(error.class.name(), error.message.clone()).with_request_host(error.host.clone())
}

/// `httpx.HTTPStatusError`.
pub(crate) fn status_error(error: &StatusError) -> JobError {
    JobError::new("HTTPStatusError", error.to_string())
}

pub(crate) fn download_error(error: &DownloadError) -> JobError {
    match error {
        DownloadError::Transport(error) => transport_error(error),
        DownloadError::Io { .. } => JobError::new(error.class_name(), error.to_string()),
    }
}

/// `type(value).__name__` for a value `json.loads` produced.
pub(crate) fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "str",
        Value::List(_) => "list",
        Value::Dict(_) => "dict",
    }
}

/// `AttributeError: '<type>' object has no attribute '<name>'`.
pub(crate) fn no_attribute(value: &Value, name: &str) -> JobError {
    JobError::new("AttributeError", format!("'{}' object has no attribute '{name}'", type_name(value)))
}

/// `bytes.decode('utf-8')` with Python's `UnicodeDecodeError` text on failure.
pub(crate) fn decode_utf8(bytes: &[u8]) -> Result<String, JobError> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.to_owned()),
        Err(error) => {
            let start = error.valid_up_to();
            let message = match error.error_len() {
                None if bytes.len() - start > 1 => format!(
                    "'utf-8' codec can't decode bytes in position {start}-{}: unexpected end of data",
                    bytes.len() - 1
                ),
                None => format!(
                    "'utf-8' codec can't decode byte 0x{:02x} in position {start}: unexpected end of data",
                    bytes[start]
                ),
                Some(_) => {
                    let byte = bytes[start];
                    let reason = if (0x80..=0xc1).contains(&byte) || byte >= 0xf5 {
                        "invalid start byte"
                    } else {
                        "invalid continuation byte"
                    };
                    format!("'utf-8' codec can't decode byte 0x{byte:02x} in position {start}: {reason}")
                }
            };
            Err(JobError::new("UnicodeDecodeError", message))
        }
    }
}

/// `json.loads(text)` with Python's `JSONDecodeError` text on failure.
pub(crate) fn json_loads(text: &str) -> Result<Value, JobError> {
    fav_core::pyjson::loads(text).map_err(|error| JobError::new("JSONDecodeError", error.python_message(text)))
}

/// `json.loads(body_bytes)` as httpx's `response.json()` does it: UTF-8, with or
/// without a byte-order mark.
pub(crate) fn json_loads_bytes(bytes: &[u8]) -> Result<Value, JobError> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    json_loads(&decode_utf8(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::decode_utf8;

    #[test]
    fn utf8_errors_read_like_python() {
        assert_eq!(
            decode_utf8(b"\xff").unwrap_err().message(),
            "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte"
        );
        assert_eq!(
            decode_utf8(b"ab\xe4A").unwrap_err().message(),
            "'utf-8' codec can't decode byte 0xe4 in position 2: invalid continuation byte"
        );
        assert_eq!(
            decode_utf8(b"a\xe4\xb8").unwrap_err().message(),
            "'utf-8' codec can't decode bytes in position 1-2: unexpected end of data"
        );
        assert_eq!(
            decode_utf8(b"a\xe4").unwrap_err().message(),
            "'utf-8' codec can't decode byte 0xe4 in position 1: unexpected end of data"
        );
        assert_eq!(decode_utf8("中".as_bytes()).unwrap(), "中");
    }
}
