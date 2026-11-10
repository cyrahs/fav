//! Request validation the way FastAPI and pydantic do it.
//!
//! Python origin: FastAPI 0.135's request handling (`fastapi/routing.py`,
//! `fastapi/dependencies/utils.py`) and the pydantic-core checks it runs on path, query and
//! body values. Errors are collected per request, path parameters first, then query
//! parameters in declaration order, then the body, and returned together as one 422
//! (`RequestValidationError`). Every entry is pydantic's: `type`, `loc`, `msg`, `input` and,
//! where pydantic sets one, `ctx`; FastAPI leaves out the documentation URL.
//! `rust/goldens/api/requests.json` holds Python's answers to the cases tested here.

use fav_core::pyjson::{self, Dict, LoadError, Value};
use fav_core::validate::{self, LocItem};
use regex::Regex;

use crate::error::ApiError;
use crate::json;

/// Where a parameter comes from, as the first element of its `loc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Path,
    Query,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Query => "query",
        }
    }
}

/// The validation errors of one request, in the order FastAPI reports them.
#[derive(Debug, Default)]
pub struct Errors {
    entries: Vec<Value>,
}

impl Errors {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add one entry.
    pub fn push(&mut self, kind: &str, loc: Vec<Value>, msg: &str, input: Value, ctx: Option<Dict>) {
        let mut entry = Dict::new();
        entry.insert("type", Value::Str(kind.to_owned()));
        entry.insert("loc", Value::List(loc));
        entry.insert("msg", Value::Str(msg.to_owned()));
        entry.insert("input", input);
        if let Some(ctx) = ctx {
            entry.insert("ctx", Value::Dict(ctx));
        }
        self.entries.push(Value::Dict(entry));
    }

    /// Add pydantic's entries from `fav_core::validate`, under `prefix`.
    fn extend_from(&mut self, prefix: &[Value], errors: Vec<validate::ErrorItem>) {
        for error in errors {
            let mut loc = prefix.to_vec();
            loc.extend(error.loc.iter().map(|item| match item {
                LocItem::Key(name) => Value::Str(name.clone()),
                LocItem::Index(position) => Value::Int(i128::try_from(*position).unwrap_or(i128::MAX)),
            }));
            self.push(error.kind, loc, &error.msg, error.input, error.ctx);
        }
    }

    /// `Ok(())` when nothing failed, else the 422.
    pub fn finish(self) -> Result<(), ApiError> {
        if self.entries.is_empty() {
            Ok(())
        } else {
            Err(ApiError::validation(self.entries))
        }
    }
}

fn loc(source: Source, name: &str) -> Vec<Value> {
    vec![Value::Str(source.name().to_owned()), Value::Str(name.to_owned())]
}

fn ctx_one(key: &str, value: Value) -> Dict {
    let mut ctx = Dict::new();
    ctx.insert(key, value);
    ctx
}

fn missing(errors: &mut Errors, loc: Vec<Value>) {
    errors.push("missing", loc, "Field required", Value::Null, None);
}

/// Bounds of an integer parameter (`Query(ge=1, le=200)`, `Path(gt=0)`).
#[derive(Debug, Clone, Copy, Default)]
pub struct IntBounds {
    pub gt: Option<i64>,
    pub ge: Option<i64>,
    pub le: Option<i64>,
}

/// A parsed integer that may not fit 64 bits; Python's integers are unbounded, so such a
/// value is still checked against the bounds rather than refused for its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BigInt {
    Fits(i64),
    AbovePositive,
    BelowNegative,
}

/// An integer parameter. `None` when it failed (the error is recorded) or is absent
/// without a default.
pub fn int_param(
    errors: &mut Errors,
    source: Source,
    name: &str,
    raw: Option<&str>,
    default: Option<i64>,
    bounds: IntBounds,
) -> Option<BigInt> {
    let Some(raw) = raw else {
        if default.is_none() {
            missing(errors, loc(source, name));
        }
        return default.map(BigInt::Fits);
    };
    let input = Value::Str(raw.to_owned());
    let parsed = match validate::int(&input, &[]) {
        Ok(value) => BigInt::Fits(value),
        Err(failures) if failures.iter().all(|failure| failure.kind == "int_parsing_size") => {
            if fav_core::pytext::py_strip(raw).starts_with('-') {
                BigInt::BelowNegative
            } else {
                BigInt::AbovePositive
            }
        }
        Err(failures) => {
            errors.extend_from(&loc(source, name), failures);
            return None;
        }
    };
    let failed = |errors: &mut Errors, kind: &str, msg: String, key: &str, bound: i64| {
        errors.push(kind, loc(source, name), &msg, input.clone(), Some(ctx_one(key, Value::from(bound))));
    };
    if let Some(gt) = bounds.gt
        && !matches!(parsed, BigInt::AbovePositive)
        && (matches!(parsed, BigInt::BelowNegative) || matches!(parsed, BigInt::Fits(value) if value <= gt))
    {
        failed(errors, "greater_than", format!("Input should be greater than {gt}"), "gt", gt);
        return None;
    }
    if let Some(ge) = bounds.ge
        && !matches!(parsed, BigInt::AbovePositive)
        && (matches!(parsed, BigInt::BelowNegative) || matches!(parsed, BigInt::Fits(value) if value < ge))
    {
        failed(
            errors,
            "greater_than_equal",
            format!("Input should be greater than or equal to {ge}"),
            "ge",
            ge,
        );
        return None;
    }
    if let Some(le) = bounds.le
        && !matches!(parsed, BigInt::BelowNegative)
        && (matches!(parsed, BigInt::AbovePositive) || matches!(parsed, BigInt::Fits(value) if value > le))
    {
        failed(
            errors,
            "less_than_equal",
            format!("Input should be less than or equal to {le}"),
            "le",
            le,
        );
        return None;
    }
    Some(parsed)
}

/// Length and pattern constraints of a string parameter.
#[derive(Debug, Clone, Copy, Default)]
pub struct StrRules<'a> {
    pub min_length: Option<usize>,
    pub max_length: Option<usize>,
    /// The pattern as written in Python, and the compiled form.
    pub pattern: Option<(&'a str, &'a Regex)>,
}

/// A string parameter; `None` when it failed or is absent (`required` records `missing`).
pub fn str_param(
    errors: &mut Errors,
    source: Source,
    name: &str,
    raw: Option<&str>,
    required: bool,
    rules: StrRules<'_>,
) -> Option<String> {
    let Some(raw) = raw else {
        if required {
            missing(errors, loc(source, name));
        }
        return None;
    };
    let input = Value::Str(raw.to_owned());
    let length = raw.chars().count();
    if let Some(min) = rules.min_length
        && length < min
    {
        let plural = if min == 1 { "character" } else { "characters" };
        errors.push(
            "string_too_short",
            loc(source, name),
            &format!("String should have at least {min} {plural}"),
            input,
            Some(ctx_one("min_length", Value::from(i64::try_from(min).unwrap_or(i64::MAX)))),
        );
        return None;
    }
    if let Some(max) = rules.max_length
        && length > max
    {
        let plural = if max == 1 { "character" } else { "characters" };
        errors.push(
            "string_too_long",
            loc(source, name),
            &format!("String should have at most {max} {plural}"),
            input,
            Some(ctx_one("max_length", Value::from(i64::try_from(max).unwrap_or(i64::MAX)))),
        );
        return None;
    }
    if let Some((text, regex)) = rules.pattern
        && !regex.is_match(raw)
    {
        errors.push(
            "string_pattern_mismatch",
            loc(source, name),
            &format!("String should match pattern '{text}'"),
            input,
            Some(ctx_one("pattern", Value::Str(text.to_owned()))),
        );
        return None;
    }
    Some(raw.to_owned())
}

/// pydantic's `enum` error text: `'a', 'b' or 'c'`.
pub fn expected_text(values: &[&str]) -> String {
    let quoted: Vec<String> = values.iter().map(|value| format!("'{value}'")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
    }
}

/// One value of a `StrEnum`; an error at `loc` when it is not one of `values`.
pub fn enum_value(errors: &mut Errors, loc: Vec<Value>, input: &Value, values: &[&str]) -> Option<String> {
    if let Value::Str(text) = input
        && values.contains(&text.as_str())
    {
        return Some(text.clone());
    }
    let expected = expected_text(values);
    errors.push(
        "enum",
        loc,
        &format!("Input should be {expected}"),
        input.clone(),
        Some(ctx_one("expected", Value::Str(expected.clone()))),
    );
    None
}

/// A repeated query parameter of enum values (`list[JobRequestStatus] | None`); `None` when
/// absent.
pub fn enum_list_param(errors: &mut Errors, name: &str, raw: &[&str], values: &[&str]) -> Option<Vec<String>> {
    if raw.is_empty() {
        return None;
    }
    let mut parsed = Vec::new();
    let mut failed = false;
    for (position, item) in raw.iter().enumerate() {
        let mut item_loc = loc(Source::Query, name);
        item_loc.push(Value::Int(i128::try_from(position).unwrap_or(i128::MAX)));
        match enum_value(errors, item_loc, &Value::Str((*item).to_owned()), values) {
            Some(value) => parsed.push(value),
            None => failed = true,
        }
    }
    (!failed).then_some(parsed)
}

// ---------- the body ----------

/// What FastAPI hands pydantic as the body.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// No bytes at all.
    Absent,
    /// Parsed JSON (the content type said JSON).
    Json(Value),
    /// The raw bytes, when no JSON content type was sent; pydantic reports them as text.
    Raw(String),
}

/// Read the body the way FastAPI's request handler does, before any dependency runs: a
/// JSON content type (`application/json`, `application/*+json`) is parsed and a decode
/// error is a 422 straight away; any other content type, or none, leaves the bytes as they
/// are. Bytes that are not UTF-8 are a 400, as Python's decode error is.
pub fn read_body(bytes: &[u8], content_type: Option<&str>) -> Result<Body, ApiError> {
    if bytes.is_empty() {
        return Ok(Body::Absent);
    }
    let is_json = content_type.is_some_and(|value| {
        let (maintype, subtype) = content_type_parts(value);
        maintype == "application" && (subtype == "json" || subtype.ends_with("+json"))
    });
    if !is_json {
        return Ok(Body::Raw(String::from_utf8_lossy(bytes).into_owned()));
    }
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Err(ApiError::new(400, "http_error", "There was an error parsing the body"));
    };
    pyjson::loads(text).map(Body::Json).map_err(|error| {
        let (message, position) = load_error_parts(&error);
        let mut entry = Errors::new();
        entry.push(
            "json_invalid",
            vec![
                Value::Str("body".to_owned()),
                Value::from(i64::try_from(position).unwrap_or(i64::MAX)),
            ],
            "JSON decode error",
            Value::Dict(Dict::new()),
            Some(ctx_one("error", Value::Str(message.to_owned()))),
        );
        ApiError::validation(entry.entries)
    })
}

/// `email.message.Message`'s main type and subtype, lowercased; a value without a `/` is
/// `text/plain`, as the email package treats it.
fn content_type_parts(value: &str) -> (String, String) {
    let essence = value.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    match essence.split_once('/') {
        Some((main, sub)) if !main.is_empty() => (main.to_owned(), sub.to_owned()),
        _ => ("text".to_owned(), "plain".to_owned()),
    }
}

/// `JSONDecodeError.msg` and `.pos`.
fn load_error_parts(error: &LoadError) -> (&'static str, usize) {
    match *error {
        LoadError::ExpectingValue(at) => ("Expecting value", at),
        LoadError::ExtraData(at) => ("Extra data", at),
        LoadError::UnterminatedString(at) => ("Unterminated string starting at", at),
        LoadError::ControlCharacter(at) => ("Invalid control character at", at),
        LoadError::InvalidEscape(at) => ("Invalid \\escape", at),
        LoadError::ExpectingPropertyName(at) => ("Expecting property name enclosed in double quotes", at),
        LoadError::ExpectingColon(at) => ("Expecting ':' delimiter", at),
        LoadError::ExpectingComma(at) => ("Expecting ',' delimiter", at),
        LoadError::LoneSurrogate(at) => ("Lone surrogate escape", at),
        LoadError::IntegerTooLarge(at) => ("Integer out of range", at),
    }
}

fn body_loc() -> Vec<Value> {
    vec![Value::Str("body".to_owned())]
}

fn body_input(body: &Body) -> Value {
    match body {
        Body::Absent => Value::Null,
        Body::Json(value) => value.clone(),
        Body::Raw(text) => Value::Str(text.clone()),
    }
}

/// A required `dict[str, Any]` body (`PUT /api/v2/settings/{section}`).
pub fn dict_body(errors: &mut Errors, body: &Body) -> Option<Dict> {
    match body {
        // FastAPI treats a JSON `null` body as no body.
        Body::Absent | Body::Json(Value::Null) => {
            missing(errors, body_loc());
            None
        }
        Body::Json(Value::Dict(dict)) => Some(dict.clone()),
        other => {
            errors.push(
                "dict_type",
                body_loc(),
                "Input should be a valid dictionary",
                body_input(other),
                None,
            );
            None
        }
    }
}

/// A required body model with `extra='forbid'`: its dictionary, or the errors pydantic
/// reports before looking at the fields.
pub fn model_body<'a>(errors: &mut Errors, body: &'a Body) -> Option<&'a Dict> {
    match body {
        // FastAPI treats a JSON `null` body as no body.
        Body::Absent | Body::Json(Value::Null) => {
            missing(errors, body_loc());
            None
        }
        Body::Json(Value::Dict(dict)) => Some(dict),
        other => {
            errors.push(
                "model_attributes_type",
                body_loc(),
                "Input should be a valid dictionary or object to extract fields from",
                body_input(other),
                None,
            );
            None
        }
    }
}

/// A required field of a body model; records `missing` with the whole body as input.
pub fn body_field<'a>(errors: &mut Errors, dict: &'a Dict, name: &str) -> Option<&'a Value> {
    let value = dict.get(name);
    if value.is_none() {
        let mut field_loc = body_loc();
        field_loc.push(Value::Str(name.to_owned()));
        errors.push("missing", field_loc, "Field required", Value::Dict(dict.clone()), None);
    }
    value
}

/// `extra='forbid'`: one error per unexpected key, in the body's order.
pub fn forbid_extra(errors: &mut Errors, dict: &Dict, fields: &[&str]) {
    for (key, value) in dict.iter() {
        let name = fav_runtime::pystr::str_key(key);
        if !fields.contains(&name.as_str()) {
            let mut field_loc = body_loc();
            field_loc.push(Value::Str(name));
            errors.push("extra_forbidden", field_loc, "Extra inputs are not permitted", value.clone(), None);
        }
    }
}

/// `loc` for a body field.
pub fn body_field_loc(name: &str) -> Vec<Value> {
    let mut field_loc = body_loc();
    field_loc.push(Value::Str(name.to_owned()));
    field_loc
}

/// The value as a JSON string list (for error details that list allowed values).
pub fn allowed_values(values: &[&str]) -> Value {
    json::dict([("allowed_values", json::str_list(values))])
}

#[cfg(test)]
mod tests {
    use super::{Body, expected_text, read_body};

    #[test]
    fn expected_values_read_like_pydantic() {
        assert_eq!(expected_text(&["a"]), "'a'");
        assert_eq!(expected_text(&["a", "b"]), "'a' or 'b'");
        assert_eq!(expected_text(&["a", "b", "c"]), "'a', 'b' or 'c'");
    }

    #[test]
    fn bodies_follow_the_content_type() {
        assert_eq!(read_body(b"", Some("application/json")).unwrap(), Body::Absent);
        assert_eq!(read_body(b"{}", None).unwrap(), Body::Raw("{}".to_owned()));
        assert_eq!(read_body(b"{}", Some("text/plain")).unwrap(), Body::Raw("{}".to_owned()));
        assert_eq!(read_body(b"{}", Some("json")).unwrap(), Body::Raw("{}".to_owned()));
        assert!(matches!(
            read_body(b"{}", Some("Application/JSON; charset=utf-8")).unwrap(),
            Body::Json(_)
        ));
        assert!(matches!(read_body(b"{}", Some("application/problem+json")).unwrap(), Body::Json(_)));
        assert_eq!(read_body(b"\xff", Some("application/json")).unwrap_err().status, 400);
        assert_eq!(read_body(b"{bad", Some("application/json")).unwrap_err().status, 422);
    }
}
