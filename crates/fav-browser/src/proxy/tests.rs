use fav_core::golden;
use fav_core::pyjson::Value;

use super::{ProxySettings, build_proxy_settings};

fn settings(server: &str, username: Option<&str>, password: Option<&str>) -> ProxySettings {
    ProxySettings {
        server: server.to_owned(),
        username: username.map(str::to_owned),
        password: password.map(str::to_owned),
    }
}

// py: tests/test_rednote.py::test_proxy_credentials_are_split_out_of_the_url
#[test]
fn proxy_credentials_are_split_out_of_the_url() {
    // Chromium ignores userinfo in --proxy-server.
    assert_eq!(
        build_proxy_settings("http://user:pw@home.example:3128"),
        Ok(Some(settings("http://home.example:3128", Some("user"), Some("pw"))))
    );
    assert_eq!(
        build_proxy_settings("home.example:3128"),
        Ok(Some(settings("http://home.example:3128", None, None)))
    );
    assert_eq!(build_proxy_settings("  "), Ok(None));
}

// py: tests/test_rednote.py::test_an_authenticated_socks_proxy_is_refused_rather_than_silently_anonymous
#[test]
fn an_authenticated_socks_proxy_is_refused_rather_than_silently_anonymous() {
    let error = build_proxy_settings("socks5://user:pw@home.example:1080").expect_err("refused");
    // ProxyConfigurationError is a ValueError in Python.
    assert_eq!(error.class, "ProxyConfigurationError");
    assert!(error.message.contains("SOCKS"), "{error}");

    // Without credentials SOCKS is fine.
    assert_eq!(
        build_proxy_settings("socks5://home.example:1080"),
        Ok(Some(settings("socks5://home.example:1080", None, None)))
    );
}

#[test]
fn proxy_settings_match_python() {
    let document = golden::load("browser_login/proxy_settings.json");
    for case in golden::cases(&document) {
        let proxy = golden::string(golden::get(case, "proxy"));
        match (build_proxy_settings(&proxy), case.get("result"), case.get("error")) {
            (Ok(None), Some(Value::Null), None) => {}
            (Ok(Some(found)), Some(expected), None) => {
                assert_eq!(found.server, golden::string(golden::get(expected, "server")), "{proxy:?}");
                assert_eq!(found.username, expected.get("username").map(golden::string), "{proxy:?}");
                assert_eq!(found.password, expected.get("password").map(golden::string), "{proxy:?}");
            }
            (Err(error), None, Some(raised)) => {
                assert_eq!(error.class, golden::string(golden::get(raised, "class")), "{proxy:?}");
                assert_eq!(error.message, golden::string(golden::get(raised, "message")), "{proxy:?}");
            }
            (built, result, error) => panic!("{proxy:?}: built {built:?}, Python {result:?} / {error:?}"),
        }
    }
}

#[test]
fn credentials_never_reach_debug_output() {
    let found = build_proxy_settings("http://alice:s3cr3t@proxy.example:3128")
        .expect("valid")
        .expect("a proxy");
    let printed = format!("{found:?}");
    assert!(!printed.contains("s3cr3t") && !printed.contains("alice"), "{printed}");
}
