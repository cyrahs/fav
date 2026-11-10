//! Masking of the secrets stored in `app_settings`.
//!
//! Python origin: `src/api/settings_masking.py`. Reads replace a secret with a short
//! masked preview; writes treat a masked (or absent) value as "keep what is stored". The
//! per-account Telegram secrets are matched by account name, so reordering accounts in the
//! UI cannot move a secret between them. `rust/goldens/api/masking.json` holds Python's
//! results.

use fav_core::pyjson::{Dict, Value};

pub const MASK_SUFFIX: &str = "••••";
const VISIBLE_PREFIX_CHARS: usize = 4;

/// `mask_value`: the first four characters and the mask; four or fewer show only the mask.
pub fn mask_value(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    if value.chars().count() <= VISIBLE_PREFIX_CHARS {
        return MASK_SUFFIX.to_owned();
    }
    let prefix: String = value.chars().take(VISIBLE_PREFIX_CHARS).collect();
    format!("{prefix}{MASK_SUFFIX}")
}

/// `is_masked`.
pub fn is_masked(value: &Value) -> bool {
    matches!(value, Value::Str(text) if text.ends_with(MASK_SUFFIX))
}

/// `keep_secret`: an absent, null or masked value is replaced by the stored one.
fn keep_secret(payload: &mut Dict, key: &str, stored_value: String) {
    let keep = match payload.get(key) {
        None | Some(Value::Null) => true,
        Some(value) => is_masked(value),
    };
    if keep {
        payload.insert(key, Value::Str(stored_value));
    }
}

fn mask_scalar(payload: &mut Dict, key: &str) {
    if let Some(Value::Str(text)) = payload.get(key) {
        let masked = mask_value(text);
        payload.insert(key, Value::Str(masked));
    }
}

/// `_accounts`: the dictionaries in `accounts`, copied; anything else reads as none.
fn accounts(payload: &Dict) -> Vec<Dict> {
    match payload.get("accounts") {
        Some(Value::List(items)) => items.iter().filter_map(|item| item.as_dict().cloned()).collect(),
        _ => Vec::new(),
    }
}

/// `str(value or '')`.
fn str_or_empty(value: Option<&Value>) -> String {
    match value {
        Some(value) if value.is_truthy() => fav_runtime::pystr::str_value(value),
        _ => String::new(),
    }
}

/// `mask_section`.
pub fn mask_section(section: &str, payload: &Dict) -> Dict {
    let mut masked = payload.clone();
    match section {
        "notifications.telegram" => mask_scalar(&mut masked, "bot_token"),
        "web.wechat" | "services.tikhub" => mask_scalar(&mut masked, "token"),
        "web.telegram" => {
            let accounts = accounts(&masked)
                .into_iter()
                .map(|mut account| {
                    mask_scalar(&mut account, "api_hash");
                    Value::Dict(account)
                })
                .collect();
            masked.insert("accounts", Value::List(accounts));
        }
        _ => {}
    }
    masked
}

/// `unmask_section`: restore secrets the client sent back masked or left out.
pub fn unmask_section(section: &str, payload: &Dict, stored: &Dict) -> Dict {
    let mut merged = payload.clone();
    match section {
        "notifications.telegram" => keep_secret(&mut merged, "bot_token", str_or_empty(stored.get("bot_token"))),
        "web.wechat" | "services.tikhub" => keep_secret(&mut merged, "token", str_or_empty(stored.get("token"))),
        "web.telegram" => {
            // Later accounts of the same name win, as a dict comprehension keeps the last.
            let mut stored_hashes: Vec<(String, String)> = Vec::new();
            for account in accounts(stored) {
                let name = str_or_empty(account.get("name"));
                let hash = str_or_empty(account.get("api_hash"));
                if let Some(slot) = stored_hashes.iter_mut().find(|(existing, _)| *existing == name) {
                    slot.1 = hash;
                } else {
                    stored_hashes.push((name, hash));
                }
            }
            let accounts = accounts(&merged)
                .into_iter()
                .map(|mut account| {
                    let name = str_or_empty(account.get("name"));
                    let stored_hash = stored_hashes
                        .iter()
                        .find(|(existing, _)| *existing == name)
                        .map(|(_, hash)| hash.clone())
                        .unwrap_or_default();
                    keep_secret(&mut account, "api_hash", stored_hash);
                    Value::Dict(account)
                })
                .collect();
            merged.insert("accounts", Value::List(accounts));
        }
        _ => {}
    }
    merged
}

#[cfg(test)]
mod tests {
    use fav_core::golden;
    use fav_core::pyjson::{self, Dict, Value};

    use super::{MASK_SUFFIX, mask_section, mask_value, unmask_section};

    fn dict(value: &Value) -> Dict {
        value.as_dict().expect("a dictionary").clone()
    }

    fn parse(text: &str) -> Dict {
        dict(&pyjson::loads(text).expect("valid JSON"))
    }

    fn get<'a>(dict: &'a Dict, key: &str) -> &'a Value {
        dict.get(key).expect("present")
    }

    #[test]
    fn mask_value_keeps_a_short_prefix() {
        assert_eq!(mask_value(""), "");
        assert_eq!(mask_value("abcd"), "••••");
        assert_eq!(mask_value("abcde"), "abcd••••");
        assert_eq!(mask_value("😀😀😀😀😀"), "😀😀😀😀••••");
    }

    #[test]
    fn masking_matches_python() {
        let document = golden::load("api/masking.json");
        for case in golden::cases(&document) {
            let section = golden::string(golden::get(case, "section"));
            let payload = dict(golden::get(case, "payload"));
            let result = match golden::string(golden::get(case, "op")).as_str() {
                "mask" => mask_section(&section, &payload),
                _ => unmask_section(&section, &payload, &dict(golden::get(case, "stored"))),
            };
            assert_eq!(Value::Dict(result), *golden::get(case, "result"), "{case:?}");
        }
    }

    // py: tests/test_settings_masking.py::test_a_nonsecret_field_is_left_alone
    #[test]
    fn a_nonsecret_field_is_left_alone() {
        let masked = mask_section("web.wechat", &parse(r#"{"api_url": "http://favinbox", "token": "secret-token"}"#));
        assert_eq!(get(&masked, "api_url"), &Value::from("http://favinbox"));
    }

    // py: tests/test_settings_masking.py::test_telegram_bot_token_is_masked
    #[test]
    fn telegram_bot_token_is_masked() {
        let masked = mask_section(
            "notifications.telegram",
            &parse(r#"{"enabled": true, "bot_token": "123456:secret", "chat_id": "-100123", "message_thread_id": null}"#),
        );
        assert_eq!(get(&masked, "bot_token"), &Value::Str(format!("1234{MASK_SUFFIX}")));
        assert_eq!(get(&masked, "chat_id"), &Value::from("-100123"));
    }

    // py: tests/test_settings_masking.py::test_masked_telegram_bot_token_is_preserved_on_save
    #[test]
    fn masked_telegram_bot_token_is_preserved_on_save() {
        let stored = parse(r#"{"enabled": false, "bot_token": "123456:secret", "chat_id": "-100123", "message_thread_id": null}"#);
        let payload = parse(&format!(
            r#"{{"enabled": true, "bot_token": "1234{MASK_SUFFIX}", "chat_id": "-100456", "message_thread_id": 42}}"#
        ));
        let merged = unmask_section("notifications.telegram", &payload, &stored);
        assert_eq!(
            merged,
            parse(r#"{"enabled": true, "bot_token": "123456:secret", "chat_id": "-100456", "message_thread_id": 42}"#)
        );
    }

    // py: tests/test_settings_masking.py::test_short_secret_reveals_no_prefix
    #[test]
    fn short_secret_reveals_no_prefix() {
        let masked = mask_section("notifications.telegram", &parse(r#"{"bot_token": "abc"}"#));
        assert_eq!(get(&masked, "bot_token"), &Value::from(MASK_SUFFIX));
    }

    // py: tests/test_settings_masking.py::test_empty_secret_stays_empty_rather_than_looking_configured
    #[test]
    fn empty_secret_stays_empty_rather_than_looking_configured() {
        let masked = mask_section("notifications.telegram", &parse(r#"{"bot_token": ""}"#));
        assert_eq!(get(&masked, "bot_token"), &Value::from(""));
    }

    // py: tests/test_settings_masking.py::test_masking_does_not_mutate_the_input_payload
    #[test]
    fn masking_does_not_mutate_the_input_payload() {
        let payload = parse(r#"{"accounts": [{"name": "a", "api_hash": "hash-A"}]}"#);
        let before = payload.clone();
        let masked = mask_section("web.telegram", &payload);
        assert_eq!(payload, before);
        assert_ne!(masked, before);
    }

    // py: tests/test_settings_masking.py::test_writing_back_a_masked_secret_keeps_the_stored_value
    #[test]
    fn writing_back_a_masked_secret_keeps_the_stored_value() {
        let merged = unmask_section(
            "notifications.telegram",
            &parse(&format!(r#"{{"bot_token": "real{MASK_SUFFIX}"}}"#)),
            &parse(r#"{"bot_token": "real-token"}"#),
        );
        assert_eq!(get(&merged, "bot_token"), &Value::from("real-token"));
    }

    // py: tests/test_settings_masking.py::test_omitting_a_secret_keeps_the_stored_value
    #[test]
    fn omitting_a_secret_keeps_the_stored_value() {
        let merged = unmask_section(
            "notifications.telegram",
            &parse(r#"{"chat_id": "-100"}"#),
            &parse(r#"{"bot_token": "real-token"}"#),
        );
        assert_eq!(get(&merged, "bot_token"), &Value::from("real-token"));
    }

    // py: tests/test_settings_masking.py::test_a_new_secret_replaces_the_stored_value
    #[test]
    fn a_new_secret_replaces_the_stored_value() {
        let merged = unmask_section(
            "notifications.telegram",
            &parse(r#"{"bot_token": "brand-new"}"#),
            &parse(r#"{"bot_token": "real-token"}"#),
        );
        assert_eq!(get(&merged, "bot_token"), &Value::from("brand-new"));
    }

    // py: tests/test_settings_masking.py::test_telegram_secrets_follow_the_account_name_across_a_reorder
    #[test]
    fn telegram_secrets_follow_the_account_name_across_a_reorder() {
        let stored = parse(r#"{"accounts": [{"name": "alice", "api_hash": "hash-ALICE"}, {"name": "bob", "api_hash": "hash-BOB"}]}"#);
        let masked = mask_section("web.telegram", &stored);
        // The UI reorders the accounts and sends the masked values straight back.
        let mut accounts = get(&masked, "accounts").as_list().expect("a list").to_vec();
        accounts.reverse();
        let mut reordered = Dict::new();
        reordered.insert("accounts", Value::List(accounts));
        let merged = unmask_section("web.telegram", &reordered, &stored);
        let by_name: Vec<(String, String)> = get(&merged, "accounts")
            .as_list()
            .expect("a list")
            .iter()
            .map(|account| {
                (
                    golden::string(golden::get(account, "name")),
                    golden::string(golden::get(account, "api_hash")),
                )
            })
            .collect();
        assert_eq!(
            by_name,
            [
                ("bob".to_owned(), "hash-BOB".to_owned()),
                ("alice".to_owned(), "hash-ALICE".to_owned())
            ]
        );
    }

    // py: tests/test_settings_masking.py::test_the_rednote_proxy_is_shown_and_saved_in_plaintext
    #[test]
    fn the_rednote_proxy_is_shown_and_saved_in_plaintext() {
        // Deliberate: proxies are configuration, not credentials, in this single-user
        // deployment, so the UI shows and edits them as ordinary text.
        let stored = parse(r#"{"user_id": "", "proxy": "http://user:pw@home.example:3128"}"#);
        let mut edited = mask_section("web.rednote", &stored);
        assert_eq!(get(&edited, "proxy"), &Value::from("http://user:pw@home.example:3128"));
        edited.insert("proxy", Value::from("http://other.example:3128"));
        let merged = unmask_section("web.rednote", &edited, &stored);
        assert_eq!(get(&merged, "proxy"), &Value::from("http://other.example:3128"));
    }

    // py: tests/test_settings_masking.py::test_the_retired_cookiecloud_section_is_passed_through_untouched
    #[test]
    fn the_retired_cookiecloud_section_is_passed_through_untouched() {
        let payload = parse(r#"{"configs": [{"name": "main", "password": "p"}]}"#);
        assert_eq!(mask_section("cookiecloud", &payload), payload);
    }
}
