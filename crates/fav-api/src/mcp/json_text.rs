//! JSON text as `pydantic_core` writes it.
//!
//! Python origin: `pydantic_core.to_json` (the text of a tool result, `indent=2`) and
//! `BaseModel.model_dump_json` (every JSON-RPC message the transport sends). Both write
//! non-ASCII characters as they are and escape only `"`, `\` and control characters below
//! U+0020, print floats in their shortest round-trip form (`1e20`, `2000000000.0`),
//! integers exactly, and dictionary keys as strings. They differ in non-finite floats:
//! `to_json` writes `NaN`/`Infinity`, a model dump `null` (`ser_json_inf_nan='null'`).

use fav_core::pyjson::{Key, Value};

#[derive(Clone, Copy)]
enum NonFinite {
    Constants,
    Null,
}

/// `pydantic_core.to_json(value, indent=2)`: a tool result's text.
pub(crate) fn pretty(value: &Value) -> String {
    let mut out = String::new();
    write(&mut out, value, Some(0), NonFinite::Constants);
    out
}

/// `model_dump_json()`: a JSON-RPC message on the wire.
pub(crate) fn compact(value: &Value) -> String {
    let mut out = String::new();
    write(&mut out, value, None, NonFinite::Null);
    out
}

fn string(out: &mut String, text: &str) {
    // serde_json escapes exactly what pydantic_core (also serde_json) does.
    out.push_str(&serde_json::Value::String(text.to_owned()).to_string());
}

/// The shortest round-trip form, with the exponent as Rust's `ryu` writes it (`1e20`,
/// `1.5e-7`), which is pydantic-core's; `serde_json` adds a `+` to positive exponents.
fn float_text(number: f64) -> String {
    serde_json::Value::from(number).to_string().replace("e+", "e")
}

fn float(out: &mut String, number: f64, mode: NonFinite) {
    if number.is_finite() {
        out.push_str(&float_text(number));
        return;
    }
    out.push_str(match mode {
        NonFinite::Null => "null",
        NonFinite::Constants if number.is_nan() => "NaN",
        NonFinite::Constants if number > 0.0 => "Infinity",
        NonFinite::Constants => "-Infinity",
    });
}

fn key_text(key: &Key) -> String {
    match key {
        Key::Str(text) => text.clone(),
        Key::Int(int) => int.to_string(),
        Key::Float(number) => float_text(*number),
        Key::Bool(true) => "true".to_owned(),
        Key::Bool(false) => "false".to_owned(),
        Key::Null => "None".to_owned(),
    }
}

fn newline(out: &mut String, depth: usize) {
    out.push('\n');
    out.extend(std::iter::repeat_n(' ', depth * 2));
}

fn write(out: &mut String, value: &Value, depth: Option<usize>, mode: NonFinite) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(int) => out.push_str(&int.to_string()),
        Value::Float(number) => float(out, *number, mode),
        Value::Str(text) => string(out, text),
        Value::List(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                if let Some(depth) = depth {
                    newline(out, depth + 1);
                }
                write(out, item, depth.map(|depth| depth + 1), mode);
            }
            if let (Some(depth), false) = (depth, items.is_empty()) {
                newline(out, depth);
            }
            out.push(']');
        }
        Value::Dict(dict) => {
            out.push('{');
            for (index, (key, item)) in dict.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                if let Some(depth) = depth {
                    newline(out, depth + 1);
                }
                string(out, &key_text(key));
                out.push_str(if depth.is_some() { ": " } else { ":" });
                write(out, item, depth.map(|depth| depth + 1), mode);
            }
            if let (Some(depth), false) = (depth, dict.is_empty()) {
                newline(out, depth);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use fav_core::pyjson::loads;

    use super::{compact, pretty};

    #[test]
    fn writes_like_pydantic_core() {
        // pydantic_core.to_json(..., indent=2), checked in Python.
        let value = loads(r#"{"a": [], "b": {}, "c": [1, {"x": null}]}"#).unwrap();
        assert_eq!(
            pretty(&value),
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    {\n      \"x\": null\n    }\n  ]\n}"
        );
        let value = loads("[1e20, 2e9, 1.0, -0.0, 1000000000000000000000000000000, \"\\u001f\\u007f\\u2028\\\"\\\\é\", 1.5e-7]").unwrap();
        assert_eq!(
            compact(&value),
            "[1e20,2000000000.0,1.0,-0.0,1000000000000000000000000000000,\"\\u001f\u{7f}\u{2028}\\\"\\\\é\",1.5e-7]"
        );
        let value = loads("[NaN, Infinity, -Infinity]").unwrap();
        assert_eq!(pretty(&value), "[\n  NaN,\n  Infinity,\n  -Infinity\n]");
        assert_eq!(compact(&value), "[null,null,null]");
    }
}
