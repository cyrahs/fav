//! JSON responses, and the date formats the API writes.
//!
//! Bodies go through `fav_core::pyjson` so stored values keep Python's distinctions (`1`
//! and `1.0` stay apart) and are written as Starlette's `JSONResponse` writes them
//! (`ensure_ascii=False`, compact separators).

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use chrono::{DateTime, FixedOffset, NaiveDateTime, SecondsFormat, Utc};
use fav_core::pyjson::{self, Dict, DumpOptions, Value};

/// pydantic's `mode='json'` dump, which every response body goes through: a non-finite
/// float becomes `null`.
fn finite(value: &Value) -> Value {
    match value {
        Value::Float(number) if !number.is_finite() => Value::Null,
        Value::List(items) => Value::List(items.iter().map(finite).collect()),
        Value::Dict(dict) => {
            let mut out = Dict::new();
            for (key, item) in dict.iter() {
                out.insert(key.clone(), finite(item));
            }
            Value::Dict(out)
        }
        other => other.clone(),
    }
}

/// A JSON response, written as Starlette's `JSONResponse` writes it.
pub(crate) fn response(status: StatusCode, value: &Value) -> Response {
    match pyjson::dumps(&finite(value), &DumpOptions::compact()) {
        Ok(text) => {
            let mut response = Response::new(Body::from(text));
            *response.status_mut() = status;
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            response
        }
        Err(error) => {
            fav_core::logging::get("fav-api").error(format!("Unhandled API error: ValueError: {error}"));
            crate::error::ApiError::internal().into_response_value()
        }
    }
}

/// `serialize_datetime`: UTC, whole seconds, `Z` suffix.
pub(crate) fn utc_z(value: &DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub(crate) fn utc_z_opt(value: Option<&DateTime<Utc>>) -> Value {
    value.map_or(Value::Null, |time| Value::Str(utc_z(time)))
}

/// Python's `datetime.isoformat()` of an aware value in UTC (`+00:00`, microseconds when set).
pub(crate) fn isoformat_utc(value: &DateTime<Utc>) -> String {
    pyjson::py_isoformat_datetime(&value.with_timezone(&FixedOffset::east_opt(0).expect("zero offset")))
}

/// Python's `datetime.isoformat()` of a naive value (`T` separator, microseconds when set).
pub(crate) fn isoformat_naive(value: &NaiveDateTime) -> String {
    pyjson::py_str_naive_datetime(value).replacen(' ', "T", 1)
}

/// A dictionary from `(key, value)` pairs, in order.
pub(crate) fn dict<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    let mut dict = Dict::new();
    for (key, value) in pairs {
        dict.insert(key, value);
    }
    Value::Dict(dict)
}

pub(crate) fn str_list<S: AsRef<str>>(items: &[S]) -> Value {
    Value::List(items.iter().map(|item| Value::Str(item.as_ref().to_owned())).collect())
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, TimeZone as _, Utc};

    use super::{isoformat_naive, isoformat_utc, utc_z};

    #[test]
    fn date_formats_match_python() {
        let whole = Utc.with_ymd_and_hms(2026, 3, 1, 10, 0, 1).unwrap();
        let fraction = whole + chrono::Duration::microseconds(500_000);
        assert_eq!(utc_z(&whole), "2026-03-01T10:00:01Z");
        // serialize_datetime drops the microseconds.
        assert_eq!(utc_z(&fraction), "2026-03-01T10:00:01Z");
        assert_eq!(isoformat_utc(&whole), "2026-03-01T10:00:01+00:00");
        assert_eq!(isoformat_utc(&fraction), "2026-03-01T10:00:01.500000+00:00");
        let naive = NaiveDate::from_ymd_opt(2026, 3, 1)
            .unwrap()
            .and_hms_micro_opt(12, 0, 0, 123_456)
            .unwrap();
        assert_eq!(isoformat_naive(&naive), "2026-03-01T12:00:00.123456");
        let naive = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap().and_hms_opt(12, 0, 0).unwrap();
        assert_eq!(isoformat_naive(&naive), "2026-03-01T12:00:00");
    }
}
