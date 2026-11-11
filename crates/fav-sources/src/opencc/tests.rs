use std::fmt::Write as _;

use fav_core::golden;
use fav_core::pyjson::Value;
use sha2::{Digest, Sha256};

use super::{T2S, convert, dictionary_files, to_simplified_chinese};

#[test]
fn dictionaries_are_the_package_files() {
    let document = golden::load("hanime1/t2s_dictionaries.json");
    let cases = golden::cases(&document);
    assert_eq!(cases.len(), 2);
    for ((case, (name, text)), dictionary) in cases.iter().zip(dictionary_files()).zip(T2S.iter()) {
        assert_eq!(golden::string(golden::get(case, "file")), name);
        let digest = Sha256::digest(text.as_bytes());
        let hex = digest.iter().fold(String::new(), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("writing to a String");
            hex
        });
        assert_eq!(golden::string(golden::get(case, "sha256")), hex, "{name}");
        assert_eq!(
            golden::int(golden::get(case, "max_len")),
            i128::try_from(dictionary.max_len).unwrap(),
            "{name}"
        );
        assert_eq!(
            golden::int(golden::get(case, "min_len")),
            i128::try_from(dictionary.min_len).unwrap(),
            "{name}"
        );
        assert_eq!(
            golden::int(golden::get(case, "entries")),
            i128::try_from(dictionary.entries.len()).unwrap(),
            "{name}"
        );
        let multi = dictionary.entries.values().filter(|value| value.contains(' ')).count();
        assert_eq!(
            golden::int(golden::get(case, "multi_valued")),
            i128::try_from(multi).unwrap(),
            "{name}"
        );
    }
}

#[test]
fn conversion_matches_the_package() {
    let document = golden::load("hanime1/t2s.json");
    let cases = golden::cases(&document);
    assert!(cases.len() > 5000, "{}", cases.len());
    let mut mismatches = Vec::new();
    for case in cases {
        let input = golden::string(golden::get(case, "input"));
        let expected = golden::string(golden::get(case, "convert"));
        let simplified = match golden::get(case, "simplified") {
            Value::Null => None,
            value => Some(golden::string(value)),
        };
        let actual = convert(&input);
        if actual != expected || to_simplified_chinese(Some(&input)) != simplified {
            mismatches.push((input, expected, actual));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} mismatches, first: {:?}",
        mismatches.len(),
        mismatches.first()
    );
}

#[test]
fn to_simplified_chinese_treats_none_and_empty_as_none() {
    assert_eq!(to_simplified_chinese(None), None);
    assert_eq!(to_simplified_chinese(Some("")), None);
    assert_eq!(to_simplified_chinese(Some(" \u{3000} ")), None);
    assert_eq!(to_simplified_chinese(Some(" 屈辱 ")).as_deref(), Some("屈辱"));
    assert_eq!(
        to_simplified_chinese(Some("戀愛始於辣妹的命運 2")).as_deref(),
        Some("恋爱始于辣妹的命运 2")
    );
}
