//! The text of a pydantic `ValidationError`, as `str(exc)` renders it.
//!
//! Python origin: pydantic-core 2.33 (`ValidationError.__str__`, through pydantic 2.11),
//! which FastMCP puts into tool errors (`Error executing tool get_run: 1 validation error
//! for get_runArguments ...`) and the streamable-HTTP transport into its `Validation
//! error: ...` answer. Each entry is the location, the message, the error type, the
//! input's `repr` (cut to its first 25 and last 24 bytes when longer than 50) and the
//! input's type name, then the documentation link.

use std::fmt::Write as _;

use fav_core::pyjson::Value;
use fav_core::validate::{ErrorItem, PYDANTIC_DOCS_VERSION};
use fav_runtime::pystr::repr_value;

/// `type(value).__qualname__` of a value `json.loads` produced.
pub(crate) fn input_type(value: &Value) -> &'static str {
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

/// `truncate_input_value`: a repr over 50 bytes keeps its first 25 and last 24 bytes,
/// each cut back to a character boundary.
fn truncated(repr: &str) -> String {
    if repr.len() <= 50 {
        return repr.to_owned();
    }
    let head = repr.floor_char_boundary(25);
    let tail = repr.ceil_char_boundary(repr.len() - 24);
    format!("{}...{}", &repr[..head], &repr[tail..])
}

/// `str(ValidationError)` for a model titled `title`.
pub(crate) fn error_text(title: &str, errors: &[ErrorItem]) -> String {
    let count = errors.len();
    let mut out = format!("{count} validation error{} for {title}", if count == 1 { "" } else { "s" });
    for error in errors {
        let loc: Vec<String> = error.loc.iter().map(ToString::to_string).collect();
        let _ = write!(
            out,
            "\n{}\n  {} [type={}, input_value={}, input_type={}]\n    For further information visit https://errors.pydantic.dev/{PYDANTIC_DOCS_VERSION}/v/{}",
            loc.join("."),
            error.msg,
            error.kind,
            truncated(&repr_value(&error.input)),
            input_type(&error.input),
            error.kind,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::truncated;

    #[test]
    fn long_reprs_are_cut_at_character_boundaries() {
        // pydantic: 'aaaaaaaaaaaaaaaaaaaaaaa...ééééééééééé' for 'a' * 23 + 'é' * 30.
        let repr = format!("'{}{}'", "a".repeat(23), "é".repeat(30));
        assert_eq!(truncated(&repr), format!("'{}...{}'", "a".repeat(23), "é".repeat(11)));
        let repr = format!("'{}'", "日本".repeat(20));
        assert_eq!(truncated(&repr), "'日本日本日本日本...本日本日本日本'");
        assert_eq!(truncated(&"x".repeat(50)), "x".repeat(50));
    }
}
