//! `goldens/rednote/parsers.json`: the source's parsers against Python's answers.

use super::*;

fn int_value(int: i128) -> Value {
    Value::Int(int)
}

fn text(value: &str) -> Value {
    Value::from(value)
}

fn arg<'a>(case: &'a Value, name: &str) -> Option<&'a Value> {
    case.as_dict().and_then(|case| case.get(name))
}

fn arg_str(case: &Value, name: &str) -> String {
    golden::string(golden::get(case, name))
}

fn to_serde(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(flag) => serde_json::Value::Bool(*flag),
        Value::Int(int) => i64::try_from(*int).map_or(serde_json::Value::Null, serde_json::Value::from),
        Value::Float(float) => serde_json::Value::from(*float),
        Value::Str(text) => serde_json::Value::String(text.clone()),
        Value::List(items) => serde_json::Value::Array(items.iter().map(to_serde).collect()),
        Value::Dict(dict) => serde_json::Value::Object(dict.iter().map(|(key, item)| (key_text(key), to_serde(item))).collect()),
    }
}

fn key_text(key: &fav_core::pyjson::Key) -> String {
    match key {
        fav_core::pyjson::Key::Str(text) => text.clone(),
        other => panic!("a string key, not {other:?}"),
    }
}

fn media_dict(media: &RedNoteMedia) -> Value {
    let mut dict = Dict::new();
    dict.insert("note_id", text(&media.note_id));
    dict.insert("media_index", int_value(i128::from(media.media_index)));
    dict.insert("media_type", text(&media.media_type));
    dict.insert("media_url", text(&media.media_url));
    dict.insert("title", text(&media.title));
    dict.insert("author", text(&media.author));
    dict.insert("author_id", text(&media.author_id));
    dict.insert("note_type", text(&media.note_type));
    dict.insert("published_at", text(&media.published_at));
    dict.insert("xsec_token", text(&media.xsec_token));
    dict.insert("last_error", text(&media.last_error));
    Value::Dict(dict)
}

fn media_from(case: &Value) -> RedNoteMedia {
    let media = golden::get(case, "media");
    let field = |name: &str| arg(media, name).map(golden::string).unwrap_or_default();
    RedNoteMedia {
        note_id: field("note_id"),
        media_index: i64::try_from(golden::int(golden::get(media, "media_index"))).expect("an index"),
        media_type: field("media_type"),
        media_url: field("media_url"),
        title: field("title"),
        author: field("author"),
        author_id: field("author_id"),
        note_type: field("note_type"),
        published_at: field("published_at"),
        xsec_token: field("xsec_token"),
        last_error: field("last_error"),
    }
}

/// A raised error as the golden records it.
fn raised(class: &str, message: &str, dedupe_key: Option<&str>) -> Value {
    let mut error = Dict::new();
    error.insert("class", text(class));
    error.insert("message", text(message));
    if let Some(key) = dedupe_key {
        error.insert("dedupe_key", text(key));
    }
    Value::Dict(error)
}

fn py(result: Result<Value, PyError>) -> Result<Value, Value> {
    result.map_err(|error| raised(&error.class, &error.message, None))
}

fn job(result: Result<Value, JobError>) -> Result<Value, Value> {
    result.map_err(|error| raised(error.class_name(), error.message(), error.notification_dedupe_key()))
}

fn answer(case: &Value) -> Result<Value, Value> {
    let function = arg_str(case, "function");
    match function.as_str() {
        "to_int" => Ok(to_int(arg(case, "value")).map_or(Value::Null, int_value)),
        "format_published_at" => py(format_published_at(arg(case, "value")).map(Value::Str)),
        "infer_image_extension" => py(infer_image_extension(&arg_str(case, "url")).map(Value::Str)),
        "normalize_media_url" => py(normalize_media_url(&arg_str(case, "url")).map(Value::Str)),
        "pick_image_url" => py(pick_image_url(arg(case, "entry")).map(Value::Str)),
        "best_video_stream" => {
            py(best_video_stream(arg(case, "stream")).map(|(url, edge)| Value::List(vec![Value::Str(url), int_value(edge)])))
        }
        "note_source_long_edge" => Ok(int_value(note_source_long_edge(arg(case, "block")))),
        "build_note_url" => Ok(Value::Str(build_note_url(&arg_str(case, "note_id"), &arg_str(case, "token")))),
        "build_media_filename" => py(build_media_filename(&media_from(case), &arg_str(case, "ext")).map(Value::Str)),
        "parse_api_envelope" => job(parse_api_envelope(arg(case, "payload")).map(Value::Dict)),
        "extract_like_page" => {
            let data = arg(case, "data").and_then(Value::as_dict).cloned().unwrap_or_default();
            let (notes, cursor, has_more) = extract_like_page(&data);
            let notes = notes
                .iter()
                .map(|note| {
                    let mut dict = Dict::new();
                    dict.insert("note_id", text(&note.note_id));
                    dict.insert("xsec_token", text(&note.xsec_token));
                    Value::Dict(dict)
                })
                .collect();
            Ok(Value::List(vec![Value::List(notes), Value::Str(cursor), Value::Bool(has_more)]))
        }
        "extract_note_media" => {
            let card = arg(case, "card").and_then(Value::as_dict).cloned().unwrap_or_default();
            py(extract_note_media(&card, "n1", "T").map(|media| Value::List(media.iter().map(media_dict).collect())))
        }
        "decide_login_state" => {
            let serde_json::Value::Object(probe) = to_serde(golden::get(case, "probe")) else {
                panic!("a probe")
            };
            let cookies: HashMap<String, String> = golden::get(case, "cookies")
                .as_dict()
                .map(|cookies| {
                    cookies
                        .iter()
                        .map(|(name, value)| (key_text(name), golden::string(value)))
                        .collect()
                })
                .unwrap_or_default();
            Ok(text(decide_login_state(&probe, &cookies)))
        }
        other => panic!("no parser named {other}"),
    }
}

#[test]
fn the_parsers_answer_as_python_does() {
    let document = golden::load("rednote/parsers.json");
    let cases = golden::cases(&document);
    assert!(cases.len() > 100, "{}", cases.len());
    for case in cases {
        let expected = match arg(case, "error") {
            Some(error) => Err(error.clone()),
            None => Ok(golden::get(case, "result").clone()),
        };
        assert_eq!(answer(case), expected, "{case:?}");
    }
}
