use super::*;
use crate::golden;
use crate::pyjson::Key;

fn pairs(value: &Value) -> Vec<(String, String)> {
    value
        .as_dict()
        .unwrap_or_else(|| panic!("expected an object, got {value:?}"))
        .iter()
        .map(|(name, value)| match name {
            Key::Str(name) => (name.clone(), golden::string(value)),
            other => panic!("expected a string key, got {other:?}"),
        })
        .collect()
}

/// The parts of an error entry the env goldens record.
fn error_summary(error: &ErrorItem) -> Value {
    let full = error.to_value();
    let mut dict = Dict::new();
    for name in ["type", "loc", "msg", "input"] {
        dict.insert(name, golden::get(&full, name).clone());
    }
    Value::Dict(dict)
}

fn check_case(case: &Value, result: Result<Env, ValidationError>) {
    match (result, case.get("errors")) {
        (Ok(env), None) => {
            assert_eq!(env.dump(), *golden::get(case, "value"), "{case:?}");
            let origins: Vec<Value> = env.cors_origins().into_iter().map(Value::Str).collect();
            assert_eq!(Value::List(origins), *golden::get(case, "cors_origins"), "{case:?}");
        }
        (Err(error), Some(expected)) => {
            assert_eq!(error.title, "Env");
            let errors: Vec<Value> = error.errors.iter().map(error_summary).collect();
            assert_eq!(Value::List(errors), *expected, "{case:?}");
        }
        (Ok(env), Some(_)) => panic!("expected errors for {case:?}, got {env:?}"),
        (Err(error), None) => panic!("expected a value for {case:?}, got {error}"),
    }
}

#[test]
fn environ_matches_pydantic_settings() {
    let document = golden::load("env/environ.json");
    for case in golden::cases(&document) {
        check_case(case, Env::from_sources(pairs(golden::get(case, "environ")), None));
    }
    assert_eq!(golden::cases(&document).len(), 31);
}

#[test]
fn dotenv_matches_pydantic_settings() {
    let document = golden::load("env/dotenv.json");
    for case in golden::cases(&document) {
        let text = golden::string(golden::get(case, "dotenv"));
        let environ = case.get("environ").map(pairs).unwrap_or_default();
        check_case(case, Env::from_sources(environ, Some(&text)));
    }
    assert_eq!(golden::cases(&document).len(), 17);
}

#[test]
fn dotenv_expands_variables_like_python_dotenv() {
    let environ = vec![("HOME".to_owned(), "/home/fav".to_owned())];
    let text = "A=${HOME}/x\nB=${A}-${MISSING:-fallback}\nC='${HOME}'\nD=${bad:name}\nE\n";
    let parsed = parse_dotenv(text, &environ);
    let expected = vec![
        ("A".to_owned(), Some("/home/fav/x".to_owned())),
        ("B".to_owned(), Some("/home/fav/x-fallback".to_owned())),
        ("C".to_owned(), Some("/home/fav".to_owned())),
        ("D".to_owned(), Some("${bad:name}".to_owned())),
        ("E".to_owned(), None),
    ];
    assert_eq!(parsed, expected);
}

#[test]
fn a_bad_statement_is_skipped_from_where_reading_stopped() {
    // python-dotenv's `_rest_of_line` resumes after the closing quote, two lines down.
    let parsed = parse_dotenv("A=\"x\nAPI_TOKEN=evil\" junk\nB=1\n", &[]);
    assert_eq!(parsed, [("B".to_owned(), Some("1".to_owned()))]);
}

#[test]
fn debug_output_redacts_secrets() {
    let env = Env::from_sources(base([]), None).expect("valid");
    let text = format!("{env:?}");
    assert!(!text.contains("abc123") && !text.contains("db.local"), "{text}");
    // A missing variable's error input is every variable found, secrets included.
    let error = Env::from_sources(vars([("API_TOKEN", "abc123")]), None).expect_err("missing DSN");
    let text = format!("{error:?} {error}");
    assert!(!text.contains("abc123"), "{text}");
}

fn base<const N: usize>(extra: [(&str, &str); N]) -> Vec<(String, String)> {
    [("POSTGRES_DSN", "postgresql://db.local/fav"), ("API_TOKEN", "abc123")]
        .into_iter()
        .chain(extra)
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn vars<const N: usize>(items: [(&str, &str); N]) -> Vec<(String, String)> {
    items.into_iter().map(|(name, value)| (name.to_owned(), value.to_owned())).collect()
}

// py: tests/test_config_env.py::test_env_reads_required_bootstrap_values
#[test]
fn env_reads_required_bootstrap_values() {
    let env = Env::from_sources(
        vars([("POSTGRES_DSN", "postgresql://user:pass@db.local/fav"), ("API_TOKEN", "abc123")]),
        None,
    )
    .expect("valid");
    assert_eq!(env.postgres_dsn, "postgresql://user:pass@db.local/fav");
    assert_eq!(env.api_token, "abc123");
    assert_eq!(env.api_bind, "0.0.0.0");
    assert_eq!(env.api_port, 8091);
}

// py: tests/test_config_env.py::test_env_overrides_bind_and_port
#[test]
fn env_overrides_bind_and_port() {
    let env = Env::from_sources(base([("API_BIND", "127.0.0.1"), ("API_PORT", "9000")]), None).expect("valid");
    assert_eq!(env.api_bind, "127.0.0.1");
    assert_eq!(env.api_port, 9000);
}

// py: tests/test_config_env.py::test_env_rejects_missing_postgres_dsn
#[test]
fn env_rejects_missing_postgres_dsn() {
    assert!(Env::from_sources(vars([("API_TOKEN", "abc123")]), None).is_err());
}

// py: tests/test_config_env.py::test_env_rejects_empty_api_token
#[test]
fn env_rejects_empty_api_token() {
    // An empty token would leave the settings API world-writable on a fresh deployment.
    assert!(Env::from_sources(vars([("POSTGRES_DSN", "postgresql://db.local/fav"), ("API_TOKEN", "   ")]), None).is_err());
}

// py: tests/test_config_env.py::test_env_rejects_out_of_range_port
#[test]
fn env_rejects_out_of_range_port() {
    assert!(Env::from_sources(base([("API_PORT", "70000")]), None).is_err());
}

// py: tests/test_config_env.py::test_cors_origins_are_split_trimmed_and_deduped
#[test]
fn cors_origins_are_split_trimmed_and_deduped() {
    let env = Env::from_sources(
        base([("API_CORS_ORIGINS", " https://a.example/, https://b.example ,, https://a.example ")]),
        None,
    )
    .expect("valid");
    assert_eq!(env.cors_origins(), ["https://a.example", "https://b.example"]);
}

// py: tests/test_config_env.py::test_cors_origins_default_to_empty
#[test]
fn cors_origins_default_to_empty() {
    let env = Env::from_sources(base([]), None).expect("valid");
    assert!(env.cors_origins().is_empty());
}
