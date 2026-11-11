use super::{Attribute, HtmlHandler, entities, feed, unescape};
use crate::golden;
use crate::pyjson::Value;

#[test]
fn the_entity_table_is_pythons() {
    let document = golden::load("pyweb/entities.json");
    let cases = golden::cases(&document);
    assert_eq!(cases.len(), entities::ENTITIES.len());
    for (case, (name, value)) in cases.iter().zip(entities::ENTITIES.iter()) {
        let pair = golden::list(case);
        assert_eq!(golden::string(&pair[0]), *name);
        assert_eq!(golden::string(&pair[1]), *value, "{name}");
    }
}

#[test]
fn unescape_matches_python() {
    let document = golden::load("pyweb/unescape.json");
    for case in golden::cases(&document) {
        let text = golden::string(golden::get(case, "text"));
        assert_eq!(unescape(&text), golden::string(golden::get(case, "result")), "{text:?}");
    }
}

#[derive(Default)]
struct Recorder {
    events: Vec<Value>,
}

fn attrs_value(attrs: &[Attribute]) -> Value {
    Value::List(
        attrs
            .iter()
            .map(|(name, value)| Value::List(vec![Value::from(name.as_str()), value.as_deref().map_or(Value::Null, Value::from)]))
            .collect(),
    )
}

impl HtmlHandler for Recorder {
    fn start_tag(&mut self, tag: &str, attrs: &[Attribute]) {
        self.events
            .push(Value::List(vec![Value::from("start"), Value::from(tag), attrs_value(attrs)]));
    }

    fn end_tag(&mut self, tag: &str) {
        self.events.push(Value::List(vec![Value::from("end"), Value::from(tag)]));
    }

    fn data(&mut self, data: &str) {
        self.events.push(Value::List(vec![Value::from("data"), Value::from(data)]));
    }
}

#[test]
fn the_tokenizer_reports_what_htmlparser_reports() {
    let document = golden::load("pyweb/html_events.json");
    for case in golden::cases(&document) {
        let html = golden::string(golden::get(case, "html"));
        let mut recorder = Recorder::default();
        feed(&html, &mut recorder);
        assert_eq!(&Value::List(recorder.events), golden::get(case, "events"), "{html:?}");
    }
}

#[test]
fn text_without_references_is_borrowed() {
    assert!(matches!(unescape("plain"), std::borrow::Cow::Borrowed("plain")));
}
