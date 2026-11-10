use super::{unquote, urljoin, urlsplit};
use crate::golden;

#[test]
fn unquote_matches_python() {
    let document = golden::load("pyweb/unquote.json");
    for case in golden::cases(&document) {
        let text = golden::string(golden::get(case, "text"));
        assert_eq!(unquote(&text), golden::string(golden::get(case, "result")), "{text:?}");
    }
}

#[test]
fn urljoin_matches_python() {
    let document = golden::load("pyweb/urljoin.json");
    for case in golden::cases(&document) {
        let base = golden::string(golden::get(case, "base"));
        let url = golden::string(golden::get(case, "url"));
        let outcome = urljoin(&base, &url);
        match (case.get("result"), case.get("error")) {
            (Some(result), None) => assert_eq!(outcome.as_deref(), Ok(golden::string(result).as_str()), "{base:?} {url:?}"),
            (None, Some(error)) => assert_eq!(outcome.map_err(|error| error.0), Err(golden::string(error)), "{base:?} {url:?}"),
            _ => panic!("malformed case {case:?}"),
        }
    }
}

#[test]
fn urlsplit_matches_python() {
    let document = golden::load("pyweb/urlsplit.json");
    for case in golden::cases(&document) {
        let url = golden::string(golden::get(case, "url"));
        let parts = urlsplit(&url, "").expect("splits");
        let expected: Vec<String> = golden::list(golden::get(case, "parts")).iter().map(golden::string).collect();
        assert_eq!(
            vec![parts.scheme, parts.netloc, parts.path, parts.query, parts.fragment],
            expected,
            "{url:?}"
        );
    }
}
