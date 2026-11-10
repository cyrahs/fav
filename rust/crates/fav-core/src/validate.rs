//! Input validation with pydantic's lax-mode rules and error entries.
//!
//! The settings API answers a rejected payload with pydantic's error list
//! (`json.loads(exc.json())`: `type`, `loc`, `msg`, `input`, `ctx`, `url`), and accepts
//! what pydantic's lax mode accepts (`"12"` for an int, `"yes"` for a bool, `1.0` for an
//! int). The converters here reproduce the subset of pydantic 2.11 that fav's models use;
//! `rust/goldens/settings/validation.json` pins every rule against the real pydantic.

use std::fmt;

use crate::pyjson::{Dict, Value, float_repr};
use crate::pytext::py_strip;

/// The pydantic version whose documentation URLs the error entries carry.
pub const PYDANTIC_DOCS_VERSION: &str = "2.11";

/// One step of an error location: a field name or a list index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocItem {
    Key(String),
    Index(usize),
}

impl fmt::Display for LocItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key(key) => f.write_str(key),
            Self::Index(index) => write!(f, "{index}"),
        }
    }
}

/// Where a value sits in the validated input.
pub type Loc = Vec<LocItem>;

fn child(loc: &[LocItem], item: LocItem) -> Loc {
    let mut loc = loc.to_vec();
    loc.push(item);
    loc
}

/// `loc` extended by a field name.
pub fn key(loc: &[LocItem], name: &str) -> Loc {
    child(loc, LocItem::Key(name.to_owned()))
}

/// `loc` extended by a list index.
pub fn index(loc: &[LocItem], position: usize) -> Loc {
    child(loc, LocItem::Index(position))
}

/// One entry of a pydantic error list.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorItem {
    /// pydantic's error type, such as `int_parsing` or `value_error`.
    pub kind: &'static str,
    pub loc: Loc,
    pub msg: String,
    /// The input as it was given at `loc`, before any coercion.
    pub input: Value,
    pub ctx: Option<Dict>,
}

impl ErrorItem {
    fn new(kind: &'static str, msg: impl Into<String>, loc: &[LocItem], input: &Value) -> Self {
        Self {
            kind,
            loc: loc.to_vec(),
            msg: msg.into(),
            input: input.clone(),
            ctx: None,
        }
    }

    /// A `ValueError` raised by a validator: pydantic prefixes the message and keeps it in `ctx`.
    pub fn value_error(message: &str, loc: &[LocItem], input: &Value) -> Self {
        let mut ctx = Dict::new();
        ctx.insert("error", Value::Str(message.to_owned()));
        Self {
            kind: "value_error",
            loc: loc.to_vec(),
            msg: format!("Value error, {message}"),
            input: input.clone(),
            ctx: Some(ctx),
        }
    }

    /// The entry as `json.loads(exc.json())` holds it.
    pub fn to_value(&self) -> Value {
        let mut entry = Dict::new();
        entry.insert("type", Value::Str(self.kind.to_owned()));
        entry.insert(
            "loc",
            Value::List(
                self.loc
                    .iter()
                    .map(|item| match item {
                        LocItem::Key(name) => Value::Str(name.clone()),
                        LocItem::Index(position) => Value::Int(i128::try_from(*position).unwrap_or(i128::MAX)),
                    })
                    .collect(),
            ),
        );
        entry.insert("msg", Value::Str(self.msg.clone()));
        entry.insert("input", self.input.clone());
        if let Some(ctx) = &self.ctx {
            entry.insert("ctx", Value::Dict(ctx.clone()));
        }
        entry.insert(
            "url",
            Value::Str(format!("https://errors.pydantic.dev/{PYDANTIC_DOCS_VERSION}/v/{}", self.kind)),
        );
        Value::Dict(entry)
    }
}

/// The errors one validation produced, in pydantic's order.
pub type Errors = Vec<ErrorItem>;

/// A failed validation of a whole model, like pydantic's `ValidationError`.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    /// The model's class name, as pydantic titles the error.
    pub title: &'static str,
    pub errors: Errors,
}

impl ValidationError {
    /// `json.loads(exc.json())`: what the settings API returns as error details.
    pub fn to_value(&self) -> Value {
        Value::List(self.errors.iter().map(ErrorItem::to_value).collect())
    }
}

impl fmt::Display for ValidationError {
    /// A short form of pydantic's text: the count, the model, then `loc: msg` per error.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.errors.len();
        write!(
            f,
            "{count} validation error{} for {}",
            if count == 1 { "" } else { "s" },
            self.title
        )?;
        for error in &self.errors {
            let loc: Vec<String> = error.loc.iter().map(ToString::to_string).collect();
            write!(f, "\n{}\n  {} [type={}]", loc.join("."), error.msg, error.kind)?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationError {}

/// A value that validates from a JSON-shaped input and dumps back to one.
pub trait Model: Sized {
    /// The Python class name, used in `model_type` errors.
    const CLASS_NAME: &'static str;

    /// Validate `value` found at `loc`.
    fn validate_at(value: &Value, loc: &[LocItem]) -> Result<Self, Errors>;

    /// `json.loads(model.model_dump_json())`.
    fn dump(&self) -> Value;

    /// `Model.model_validate(value)`.
    fn validate(value: &Value) -> Result<Self, ValidationError> {
        Self::validate_at(value, &[]).map_err(|errors| ValidationError {
            title: Self::CLASS_NAME,
            errors,
        })
    }
}

/// The fields of one model being validated, collecting every field's errors.
#[derive(Debug)]
pub struct Fields<'a> {
    whole: &'a Value,
    dict: &'a Dict,
    loc: Loc,
    errors: Errors,
}

impl<'a> Fields<'a> {
    /// Start validating a model: the input must be a dictionary.
    pub fn begin(value: &'a Value, loc: &[LocItem], class_name: &str) -> Result<Self, Errors> {
        match value {
            Value::Dict(dict) => Ok(Self {
                whole: value,
                dict,
                loc: loc.to_vec(),
                errors: Vec::new(),
            }),
            other => {
                let mut error = ErrorItem::new(
                    "model_type",
                    format!("Input should be a valid dictionary or instance of {class_name}"),
                    loc,
                    other,
                );
                let mut ctx = Dict::new();
                ctx.insert("class_name", Value::Str(class_name.to_owned()));
                error.ctx = Some(ctx);
                Err(vec![error])
            }
        }
    }

    /// A field without a default: absent means a `missing` error.
    pub fn required<T>(&mut self, name: &str, convert: impl FnOnce(&Value, &[LocItem]) -> Result<T, Errors>) -> Option<T> {
        let loc = key(&self.loc, name);
        let Some(value) = self.dict.get(name) else {
            self.errors.push(ErrorItem::new("missing", "Field required", &loc, self.whole));
            return None;
        };
        convert(value, &loc).map_err(|errors| self.errors.extend(errors)).ok()
    }

    /// A field with a default, which pydantic does not validate.
    pub fn optional<T>(
        &mut self,
        name: &str,
        default: impl FnOnce() -> T,
        convert: impl FnOnce(&Value, &[LocItem]) -> Result<T, Errors>,
    ) -> Option<T> {
        let loc = key(&self.loc, name);
        match self.dict.get(name) {
            None => Some(default()),
            Some(value) => convert(value, &loc).map_err(|errors| self.errors.extend(errors)).ok(),
        }
    }

    /// End field validation. With no field errors, run the model-level `after` validator,
    /// whose `ValueError` pydantic reports at the model's own location with the whole input.
    pub fn finish<T>(self, build: impl FnOnce() -> Option<T>, model_check: impl FnOnce(&T) -> Result<(), String>) -> Result<T, Errors> {
        if !self.errors.is_empty() {
            return Err(self.errors);
        }
        let Some(model) = build() else {
            return Err(self.errors);
        };
        model_check(&model).map_err(|message| vec![ErrorItem::value_error(&message, &self.loc, self.whole)])?;
        Ok(model)
    }
}

/// Apply an `after` field validator to a converted value; its `ValueError` becomes a
/// `value_error` at the field, carrying the field's original input.
pub fn after<T>(
    convert: impl FnOnce(&Value, &[LocItem]) -> Result<T, Errors>,
    check: impl FnOnce(T) -> Result<T, String>,
) -> impl FnOnce(&Value, &[LocItem]) -> Result<T, Errors> {
    move |value, loc| {
        let converted = convert(value, loc)?;
        check(converted).map_err(|message| vec![ErrorItem::value_error(&message, loc, value)])
    }
}

// ---------- scalar converters ----------

/// `str`: strict, only a string.
pub fn string(value: &Value, loc: &[LocItem]) -> Result<String, Errors> {
    match value {
        Value::Str(text) => Ok(text.clone()),
        other => Err(vec![ErrorItem::new("string_type", "Input should be a valid string", loc, other)]),
    }
}

const INT_PARSING: &str = "Input should be a valid integer, unable to parse string as an integer";
const INT_SIZE: &str = "Unable to parse input string as an integer, exceeded maximum size";

/// `int` in lax mode: ints, bools, integral finite floats and integer strings.
///
/// Python's integers are unbounded; these are 64-bit, and anything larger is refused
/// with `int_parsing_size` (see `DIVERGENCES.md`).
pub fn int(value: &Value, loc: &[LocItem]) -> Result<i64, Errors> {
    let fail = |kind: &'static str, msg: &str| Err(vec![ErrorItem::new(kind, msg, loc, value)]);
    match value {
        Value::Bool(flag) => Ok(i64::from(*flag)),
        Value::Int(int) => i64::try_from(*int).or_else(|_| fail("int_parsing_size", INT_SIZE)),
        Value::Float(float) => match float_to_int(*float) {
            FloatInt::Int(int) => Ok(int),
            FloatInt::NotFinite => fail("finite_number", "Input should be a finite number"),
            FloatInt::Fraction => fail(
                "int_from_float",
                "Input should be a valid integer, got a number with a fractional part",
            ),
            FloatInt::TooLarge => fail("int_parsing_size", INT_SIZE),
        },
        Value::Str(text) => match parse_int_str(text) {
            Some(Ok(int)) => Ok(int),
            Some(Err(())) => fail("int_parsing_size", INT_SIZE),
            None => fail("int_parsing", INT_PARSING),
        },
        _ => fail("int_type", "Input should be a valid integer"),
    }
}

enum FloatInt {
    Int(i64),
    NotFinite,
    Fraction,
    TooLarge,
}

// i64 bounds as f64; the upper one rounds up to 2**63, which is itself out of range.
const I64_MIN_F: f64 = -9_223_372_036_854_775_808.0;
const I64_MAX_F: f64 = 9_223_372_036_854_775_808.0;

#[allow(clippy::cast_possible_truncation)] // Range and integrality are checked first.
fn float_to_int(value: f64) -> FloatInt {
    if !value.is_finite() {
        FloatInt::NotFinite
    } else if value.fract() != 0.0 {
        FloatInt::Fraction
    } else if !(I64_MIN_F..I64_MAX_F).contains(&value) {
        FloatInt::TooLarge
    } else {
        FloatInt::Int(value as i64)
    }
}

/// Digits with single underscores between them, Python's `int()` grammar after the sign.
fn underscored_digits(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    for (position, &byte) in bytes.iter().enumerate() {
        match byte {
            b'0'..=b'9' => out.push(char::from(byte)),
            b'_' if position > 0 && bytes[position - 1].is_ascii_digit() && bytes.get(position + 1).is_some_and(u8::is_ascii_digit) => {}
            _ => return None,
        }
    }
    Some(out)
}

/// pydantic's string-to-int: trim, drop a `.000` tail, then Python `int()` digits.
/// `None` is unparseable; `Some(Err(()))` parsed but does not fit 64 bits.
fn parse_int_str(text: &str) -> Option<Result<i64, ()>> {
    let mut text = text.trim();
    if let Some((whole, fraction)) = text.split_once('.') {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte == b'0') {
            return None;
        }
        text = whole;
    }
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let digits = underscored_digits(digits)?;
    let magnitude: i128 = match digits.parse() {
        Ok(value) => value,
        Err(_) => return Some(Err(())),
    };
    let signed = if negative { -magnitude } else { magnitude };
    Some(i64::try_from(signed).map_err(|_| ()))
}

/// `float` in lax mode: numbers, bools and numeric strings (including `nan` and `inf`).
#[allow(clippy::cast_precision_loss)] // Python's float(int) rounds the same way.
pub fn float(value: &Value, loc: &[LocItem]) -> Result<f64, Errors> {
    match value {
        Value::Bool(flag) => Ok(if *flag { 1.0 } else { 0.0 }),
        Value::Int(int) => Ok(*int as f64),
        Value::Float(float) => Ok(*float),
        Value::Str(text) => parse_float_str(text).ok_or_else(|| {
            vec![ErrorItem::new(
                "float_parsing",
                "Input should be a valid number, unable to parse string as a number",
                loc,
                value,
            )]
        }),
        other => Err(vec![ErrorItem::new("float_type", "Input should be a valid number", loc, other)]),
    }
}

/// pydantic's string-to-float: trim, allow underscores between digits, then Rust's float
/// grammar (which, like pydantic-core's, takes `inf`, `infinity` and `nan` in any case).
fn parse_float_str(text: &str) -> Option<f64> {
    let text = text.trim();
    let bytes = text.as_bytes();
    let mut cleaned = String::with_capacity(text.len());
    for (position, &byte) in bytes.iter().enumerate() {
        if byte == b'_' {
            let between_digits =
                position > 0 && bytes[position - 1].is_ascii_digit() && bytes.get(position + 1).is_some_and(u8::is_ascii_digit);
            if !between_digits {
                return None;
            }
        } else {
            cleaned.push(char::from(byte));
        }
    }
    if !text.is_ascii() {
        return None;
    }
    cleaned.parse().ok()
}

const BOOL_PARSING: &str = "Input should be a valid boolean, unable to interpret input";

/// `bool` in lax mode: bools, 0 and 1 (also as integral floats), and the usual words.
pub fn boolean(value: &Value, loc: &[LocItem]) -> Result<bool, Errors> {
    let from_int = |int: i128| match int {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(vec![ErrorItem::new("bool_parsing", BOOL_PARSING, loc, value)]),
    };
    match value {
        Value::Bool(flag) => Ok(*flag),
        Value::Int(int) => from_int(*int),
        Value::Float(float) => match float_to_int(*float) {
            FloatInt::Int(int) => from_int(i128::from(int)),
            _ => Err(vec![ErrorItem::new("bool_type", "Input should be a valid boolean", loc, value)]),
        },
        Value::Str(text) => match text.to_ascii_lowercase().as_str() {
            "0" | "off" | "f" | "false" | "n" | "no" => Ok(false),
            "1" | "on" | "t" | "true" | "y" | "yes" => Ok(true),
            _ => Err(vec![ErrorItem::new("bool_parsing", BOOL_PARSING, loc, value)]),
        },
        other => Err(vec![ErrorItem::new("bool_type", "Input should be a valid boolean", loc, other)]),
    }
}

/// A `pathlib.Path` as pydantic builds it from a string: only strings are accepted, and the
/// result is `str(PurePosixPath(text))`.
pub fn path(value: &Value, loc: &[LocItem]) -> Result<PyPath, Errors> {
    match value {
        Value::Str(text) => Ok(PyPath::new(text)),
        other => Err(vec![ErrorItem::new(
            "path_type",
            "Input is not a valid path for <class 'pathlib.Path'>",
            loc,
            other,
        )]),
    }
}

/// `X | None`: `None` passes, anything else goes to `convert`.
pub fn nullable<T>(
    convert: impl FnOnce(&Value, &[LocItem]) -> Result<T, Errors>,
) -> impl FnOnce(&Value, &[LocItem]) -> Result<Option<T>, Errors> {
    move |value, loc| match value {
        Value::Null => Ok(None),
        other => convert(other, loc).map(Some),
    }
}

/// `list[T]`: a list whose items each convert; every item's errors are kept.
pub fn list_of<T>(convert: impl Fn(&Value, &[LocItem]) -> Result<T, Errors>) -> impl FnOnce(&Value, &[LocItem]) -> Result<Vec<T>, Errors> {
    move |value, loc| {
        let Value::List(items) = value else {
            return Err(vec![ErrorItem::new("list_type", "Input should be a valid list", loc, value)]);
        };
        let mut converted = Vec::with_capacity(items.len());
        let mut errors = Vec::new();
        for (position, item) in items.iter().enumerate() {
            match convert(item, &index(loc, position)) {
                Ok(item) => converted.push(item),
                Err(item_errors) => errors.extend(item_errors),
            }
        }
        if errors.is_empty() { Ok(converted) } else { Err(errors) }
    }
}

/// A nested model.
pub fn model<M: Model>(value: &Value, loc: &[LocItem]) -> Result<M, Errors> {
    M::validate_at(value, loc)
}

/// A string enumeration, like `Literal['a', 'b']`.
pub trait LiteralStr: Sized + Copy + 'static {
    /// Every value with its Python spelling, in declaration order.
    const VARIANTS: &'static [(Self, &'static str)];

    fn as_str(self) -> &'static str;
}

/// `Literal[...]` of strings: an exact match, otherwise `literal_error`.
pub fn literal<L: LiteralStr>(value: &Value, loc: &[LocItem]) -> Result<L, Errors> {
    if let Value::Str(text) = value
        && let Some((variant, _)) = L::VARIANTS.iter().find(|(_, spelling)| spelling == text)
    {
        return Ok(*variant);
    }
    let quoted: Vec<String> = L::VARIANTS.iter().map(|(_, spelling)| format!("'{spelling}'")).collect();
    let expected = match quoted.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        Some((last, _)) => last.clone(),
        None => String::new(),
    };
    let mut error = ErrorItem::new("literal_error", format!("Input should be {expected}"), loc, value);
    let mut ctx = Dict::new();
    ctx.insert("expected", Value::Str(expected));
    error.ctx = Some(ctx);
    Err(vec![error])
}

// ---------- dumping ----------

/// A float as `model_dump_json` writes it: non-finite values become `null`.
pub fn dump_float(value: f64) -> Value {
    if value.is_finite() { Value::Float(value) } else { Value::Null }
}

/// Python's `str(int)`, used where messages interpolate an int.
pub fn int_text(value: i64) -> String {
    value.to_string()
}

/// Python's `str(float)`.
pub fn float_text(value: f64) -> String {
    float_repr(value)
}

// ---------- paths ----------

/// A `pathlib.PurePosixPath`, kept in its normalised string form: repeated slashes and
/// `.` parts dropped, a trailing slash dropped, `..` kept, a leading `//` (exactly two)
/// kept, and the empty path written as `.`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PyPath(String);

impl PyPath {
    pub fn new(text: &str) -> Self {
        let root = if text.starts_with("//") && !text.starts_with("///") {
            "//"
        } else if text.starts_with('/') {
            "/"
        } else {
            ""
        };
        let parts: Vec<&str> = text.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
        let joined = format!("{root}{}", parts.join("/"));
        Self(if joined.is_empty() { ".".to_owned() } else { joined })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &std::path::Path {
        std::path::Path::new(&self.0)
    }
}

impl fmt::Display for PyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&PyPath> for Value {
    fn from(value: &PyPath) -> Self {
        Value::Str(value.0.clone())
    }
}

/// Python `value.strip()` returning an owned string, for validators.
pub fn stripped(value: &str) -> String {
    py_strip(value).to_owned()
}

#[cfg(test)]
mod tests {
    use super::{PyPath, parse_float_str, parse_int_str};

    #[test]
    fn paths_normalise_like_pure_posix_path() {
        for (raw, expected) in [
            ("./a/b/", "a/b"),
            ("a//b", "a/b"),
            ("", "."),
            (".", "."),
            ("./", "."),
            ("//x", "//x"),
            ("///x", "/x"),
            ("/", "/"),
            ("/a/./b/../c", "/a/b/../c"),
            ("a/..", "a/.."),
        ] {
            assert_eq!(PyPath::new(raw).as_str(), expected, "{raw:?}");
        }
    }

    #[test]
    fn int_strings_follow_pydantic() {
        assert_eq!(parse_int_str(" 1_000 "), Some(Ok(1000)));
        assert_eq!(parse_int_str("-1.00"), Some(Ok(-1)));
        assert_eq!(parse_int_str("1."), None);
        assert_eq!(parse_int_str("1__0"), None);
        assert_eq!(parse_int_str("9223372036854775808"), Some(Err(())));
    }

    #[test]
    fn float_strings_follow_pydantic() {
        assert_eq!(parse_float_str("1_0.5"), Some(10.5));
        assert_eq!(parse_float_str("+.5"), Some(0.5));
        assert!(parse_float_str("-nan").is_some_and(f64::is_nan));
        assert_eq!(parse_float_str("infinit"), None);
        assert_eq!(parse_float_str("\u{661}"), None);
    }
}
