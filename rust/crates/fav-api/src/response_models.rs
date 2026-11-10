//! pydantic's check of what a route returns.
//!
//! Python origin: the `response_model` of a route and the `model_validate` calls in
//! `src/api/service.py` (`model_bd2_character_summary` and the like). A payload the
//! library built is validated in pydantic's lax mode and dumped in the model's field order
//! (`model_dump(mode='json')`); a payload that fails is an exception, which the API answers
//! with its generic 500. The converters here only decide pass or fail and build the dumped
//! value; the first failure is enough, as any failure is the same 500.

use fav_core::pyjson::{Dict, Value};
use fav_core::validate;

/// A dumped value, or why the model refused it.
pub(crate) type Checked = Result<Value, String>;

/// One model being validated (`extra='forbid'`, as every `ApiSchema`).
#[derive(Debug)]
pub(crate) struct Model<'a> {
    name: &'static str,
    dict: &'a Dict,
    fields: Vec<&'static str>,
    out: Dict,
}

impl<'a> Model<'a> {
    /// Start validating `value` as `name`: it must be a dictionary.
    pub(crate) fn begin(name: &'static str, value: &'a Value) -> Result<Self, String> {
        match value {
            Value::Dict(dict) => Ok(Self {
                name,
                dict,
                fields: Vec::new(),
                out: Dict::new(),
            }),
            _ => Err(format!("{name}: input should be a valid dictionary")),
        }
    }

    /// A field without a default.
    pub(crate) fn required(&mut self, field: &'static str, convert: impl FnOnce(&Value) -> Checked) -> Result<(), String> {
        self.fields.push(field);
        let Some(value) = self.dict.get(field) else {
            return Err(format!("{}.{field}: field required", self.name));
        };
        let converted = convert(value).map_err(|error| format!("{}.{field}: {error}", self.name))?;
        self.out.insert(field, converted);
        Ok(())
    }

    /// A field with a default, which is dumped as it is when the field is absent.
    pub(crate) fn optional(&mut self, field: &'static str, default: Value, convert: impl FnOnce(&Value) -> Checked) -> Result<(), String> {
        self.fields.push(field);
        let converted = match self.dict.get(field) {
            None => default,
            Some(value) => convert(value).map_err(|error| format!("{}.{field}: {error}", self.name))?,
        };
        self.out.insert(field, converted);
        Ok(())
    }

    /// `extra='forbid'`, then the dumped model.
    pub(crate) fn finish(self) -> Checked {
        for (key, _) in self.dict.iter() {
            let name = fav_runtime::pystr::str_key(key);
            if !self.fields.contains(&name.as_str()) {
                return Err(format!("{}.{name}: extra inputs are not permitted", self.name));
            }
        }
        Ok(Value::Dict(self.out))
    }
}

fn first_error(errors: &[validate::ErrorItem]) -> String {
    errors.first().map_or_else(String::new, |error| error.msg.clone())
}

/// `str`.
pub(crate) fn string(value: &Value) -> Checked {
    match value {
        Value::Str(_) => Ok(value.clone()),
        _ => Err("input should be a valid string".to_owned()),
    }
}

/// `int`, lax.
pub(crate) fn int(value: &Value) -> Checked {
    match value {
        // Python's integers are unbounded; a stored one keeps its value.
        Value::Int(_) => Ok(value.clone()),
        other => validate::int(other, &[])
            .map(|int| Value::Int(i128::from(int)))
            .map_err(|errors| first_error(&errors)),
    }
}

/// `float`, lax.
pub(crate) fn float(value: &Value) -> Checked {
    validate::float(value, &[]).map(Value::Float).map_err(|errors| first_error(&errors))
}

/// `bool`, lax.
pub(crate) fn boolean(value: &Value) -> Checked {
    validate::boolean(value, &[])
        .map(Value::Bool)
        .map_err(|errors| first_error(&errors))
}

/// `dict[str, Any]`.
pub(crate) fn free_dict(value: &Value) -> Checked {
    match value {
        Value::Dict(_) => Ok(value.clone()),
        _ => Err("input should be a valid dictionary".to_owned()),
    }
}

/// `X | None`.
pub(crate) fn nullable(convert: impl FnOnce(&Value) -> Checked) -> impl FnOnce(&Value) -> Checked {
    move |value| match value {
        Value::Null => Ok(Value::Null),
        other => convert(other),
    }
}

/// `list[X]`.
pub(crate) fn list_of(convert: impl Fn(&Value) -> Checked) -> impl FnOnce(&Value) -> Checked {
    move |value| match value {
        Value::List(items) => items
            .iter()
            .enumerate()
            .map(|(index, item)| convert(item).map_err(|error| format!("[{index}] {error}")))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::List),
        _ => Err("input should be a valid list".to_owned()),
    }
}

/// `dict[str, X]`.
pub(crate) fn dict_of(convert: impl Fn(&Value) -> Checked) -> impl FnOnce(&Value) -> Checked {
    move |value| match value {
        Value::Dict(dict) => {
            let mut out = Dict::new();
            for (key, item) in dict.iter() {
                let name = fav_runtime::pystr::str_key(key);
                let converted = convert(item).map_err(|error| format!("[{name}] {error}"))?;
                out.insert(key.clone(), converted);
            }
            Ok(Value::Dict(out))
        }
        _ => Err("input should be a valid dictionary".to_owned()),
    }
}

/// `int | str | None` in pydantic's smart mode: an exact type wins, then lax `int`
/// (a bool, an integral float); `str` takes nothing but strings either way.
pub(crate) fn int_or_str_or_none(value: &Value) -> Checked {
    match value {
        Value::Null | Value::Int(_) | Value::Str(_) => Ok(value.clone()),
        other => int(other).map_err(|_| "input should be a valid integer or string".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use fav_core::pyjson::Value;

    use super::{Model, int, int_or_str_or_none};

    #[test]
    fn smart_unions_prefer_the_exact_type() {
        assert_eq!(int_or_str_or_none(&Value::Str("12".to_owned())), Ok(Value::Str("12".to_owned())));
        assert_eq!(int_or_str_or_none(&Value::Float(999.0)), Ok(Value::Int(999)));
        assert_eq!(int_or_str_or_none(&Value::Bool(true)), Ok(Value::Int(1)));
        assert!(int_or_str_or_none(&Value::Float(1.5)).is_err());
        assert!(int_or_str_or_none(&Value::List(Vec::new())).is_err());
        assert_eq!(int(&Value::Str(" 7 ".to_owned())), Ok(Value::Int(7)));
    }

    #[test]
    fn models_forbid_extra_keys_and_fill_defaults() {
        let value = fav_core::pyjson::loads(r#"{"b": 1, "a": "x"}"#).unwrap();
        let mut model = Model::begin("M", &value).unwrap();
        model.required("a", super::string).unwrap();
        model.optional("c", Value::Int(0), int).unwrap();
        assert!(model.finish().is_err());
        let mut model = Model::begin("M", &value).unwrap();
        model.required("a", super::string).unwrap();
        model.required("b", int).unwrap();
        model.optional("c", Value::Int(0), int).unwrap();
        assert_eq!(
            fav_core::pyjson::dumps(&model.finish().unwrap(), &fav_core::pyjson::DumpOptions::compact()).unwrap(),
            r#"{"a":"x","b":1,"c":0}"#
        );
    }
}
