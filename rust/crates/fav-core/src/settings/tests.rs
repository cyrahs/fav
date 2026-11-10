use super::*;
use crate::golden;
use crate::pyjson::{DumpOptions, Key, dumps, loads};

/// Build a JSON object from `key => value` pairs.
macro_rules! obj {
    ($($key:literal => $value:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut dict = Dict::new();
        $(dict.insert($key, Value::from($value));)*
        Value::Dict(dict)
    }};
}

fn list(items: Vec<Value>) -> Value {
    Value::List(items)
}

/// Validate any model the goldens name, returning its dump, its runnable list, or its errors.
fn validate_named(name: &str, input: &Value) -> Result<(Value, Option<Vec<String>>), ValidationError> {
    fn run<M: Model>(input: &Value, runnable: impl Fn(&M) -> Option<Vec<String>>) -> Result<(Value, Option<Vec<String>>), ValidationError> {
        M::validate(input).map(|model| (model.dump(), runnable(&model)))
    }
    match name {
        "ScheduleJob" => run::<ScheduleJob>(input, |model| Some(model.validate_runnable())),
        "BilibiliFavorite" => run::<BilibiliFavorite>(input, |_| None),
        "BilibiliAccount" => run::<BilibiliAccount>(input, |model| Some(model.validate_runnable())),
        "TelegramChannel" => run::<TelegramChannel>(input, |_| None),
        "TelegramAccount" => run::<TelegramAccount>(input, |model| Some(model.validate_runnable())),
        "Hanime1RankingDeepScan" => run::<Hanime1RankingDeepScan>(input, |_| None),
        "Hanime1Ranking" => run::<Hanime1Ranking>(input, |_| None),
        "KemonoCreator" => run::<KemonoCreator>(input, |_| None),
        "Settings" => run::<Settings>(input, |_| None),
        section => match SectionValue::validate(section, input) {
            Ok(model) => {
                let runnable = if matches!(model, SectionValue::TikHub(_)) {
                    None
                } else {
                    Some(model.validate_runnable())
                };
                Ok((model.dump(), runnable))
            }
            Err(SectionError::Invalid(error)) => Err(error),
            Err(SectionError::Unknown(error)) => panic!("{error}"),
        },
    }
}

fn strings(value: &Value) -> Vec<String> {
    golden::list(value).iter().map(golden::string).collect()
}

/// Inputs whose integers do not fit 64 bits: Python keeps them, the port refuses them
/// (DIVERGENCES.md D5). Each is checked for the Rust behaviour instead.
fn beyond_i64(input: &str) -> bool {
    input.contains("9223372036854775808\"") || input.contains("99999999999999999999")
}

#[test]
fn validation_matches_pydantic() {
    let document = golden::load("settings/validation.json");
    let mut checked = 0;
    for case in golden::cases(&document) {
        let model = golden::string(golden::get(case, "model"));
        let text = golden::string(golden::get(case, "input"));
        let input = loads(&text).unwrap();
        let result = validate_named(&model, &input);
        if beyond_i64(&text) && model == "web.jandan" {
            let error = result.unwrap_err();
            assert_eq!(error.errors.len(), 1, "{text}");
            assert_eq!(error.errors[0].kind, "int_parsing_size", "{text}");
            continue;
        }
        match (case.get("value"), case.get("errors"), result) {
            (Some(expected), None, Ok((value, runnable))) => {
                assert_eq!(&value, expected, "{model} {text}");
                match case.get("runnable") {
                    Some(expected) => assert_eq!(runnable, Some(strings(expected)), "{model} {text}"),
                    None => assert_eq!(runnable, None, "{model} {text}"),
                }
            }
            (None, Some(expected), Err(error)) => assert_eq!(&error.to_value(), expected, "{model} {text}"),
            (_, _, result) => panic!("{model} {text}: golden and port disagree on success: {result:?}"),
        }
        checked += 1;
    }
    assert!(checked > 600, "only {checked} cases checked");
}

#[test]
fn build_settings_matches_python() {
    let document = golden::load("settings/build_settings.json");
    for case in golden::cases(&document) {
        let text = golden::string(golden::get(case, "rows"));
        let rows = loads(&text).unwrap();
        let rows: Vec<(&str, &Value)> = rows
            .as_dict()
            .unwrap()
            .iter()
            .map(|(key, value)| match key {
                Key::Str(name) => (name.as_str(), value),
                other => panic!("{other:?}"),
            })
            .collect();
        match (case.get("value"), case.get("errors"), build_settings(rows)) {
            (Some(expected), None, Ok(settings)) => assert_eq!(&settings.dump(), expected, "{text}"),
            (None, Some(expected), Err(error)) => assert_eq!(&error.to_value(), expected, "{text}"),
            (_, _, result) => panic!("{text}: {result:?}"),
        }
    }
}

#[test]
fn stored_defaults_are_byte_identical() {
    let document = golden::load("settings/stored_defaults.json");
    let mut sections = Vec::new();
    for case in golden::cases(&document) {
        let section = golden::string(golden::get(case, "section"));
        let model = SectionValue::validate(&section, &obj! {}).unwrap();
        assert_eq!(
            dumps(&model.dump(), &DumpOptions::plain()).unwrap(),
            golden::string(golden::get(case, "stored")),
            "{section}"
        );
        sections.push(section);
    }
    assert_eq!(sections, SECTIONS);
}

#[test]
fn section_registry_matches_python() {
    let document = golden::load("settings/sections.json");
    let cases = golden::cases(&document);
    assert_eq!(cases.len(), SECTIONS.len());
    for (case, section) in cases.iter().zip(SECTIONS) {
        assert_eq!(golden::string(golden::get(case, "section")), section);
        let model = Settings::default().section(section).unwrap();
        assert_eq!(golden::string(golden::get(case, "model")), model.class_name());
        assert_eq!(strings(golden::get(case, "sensitive_fields")), sensitive_fields(section));
    }
}

#[test]
fn default_tree_matches_python() {
    let document = golden::load("settings/default_tree.json");
    assert_eq!(&Settings::default().dump(), golden::get(&golden::cases(&document)[0], "settings"));
}

#[test]
fn unknown_sections_are_refused() {
    assert_eq!(
        SectionValue::validate("cookiecloud", &obj! {}).unwrap_err().to_string(),
        "Unknown settings section: cookiecloud"
    );
    assert!(Settings::default().section("web").is_err());
}

// ---------- tests/test_settings_store.py ----------

// py: tests/test_settings_store.py::test_a_row_left_by_the_retired_cookiecloud_section_is_not_read
#[test]
fn a_row_left_by_the_retired_cookiecloud_section_is_not_read() {
    let cookiecloud =
        obj! {"configs" => list(vec![obj! {"name" => "main", "server_url" => "https://cc", "uuid" => "u", "password" => "p"}])};
    let wechat = obj! {"token" => "t", "nga_cookiecloud" => "main"};
    let loaded = build_settings([("cookiecloud", &cookiecloud), ("web.wechat", &wechat)]).unwrap();

    assert!(loaded.dump().get("cookiecloud").is_none());
    assert!(!loaded.web.wechat.dump().as_dict().unwrap().contains_key("nga_cookiecloud"));
    assert_eq!(loaded.web.wechat.token, "t");
}

// ---------- tests/test_schedule_config.py ----------

const TELEGRAM_DEFAULT_SCAN_LIMIT: i64 = 50;
const TELEGRAM_DEFAULT_DOWNLOAD_LIMIT_PER_CHANNEL: i64 = 2;
const TELEGRAM_DEFAULT_DOWNLOAD_DELAY_SECONDS: f64 = 60.0;
const TELEGRAM_DEFAULT_CHANNEL_COOLDOWN_SECONDS: f64 = 1800.0;
const TELEGRAM_DEFAULT_HISTORY_WAIT_SECONDS: f64 = 1.0;
const TELEGRAM_DEFAULT_FLOOD_SLEEP_THRESHOLD_SECONDS: i64 = 300;
const DEEP_SCAN_DEFAULT_QUOTA: f64 = 0.25;
const DEEP_SCAN_DEFAULT_MAX_EXTRA_PAGES: i64 = 5;
const DEEP_SCAN_FRACTIONAL_QUOTA: f64 = 0.5;

fn channel(id: i64, path: &str) -> Value {
    obj! {"id" => id, "path" => path}
}

fn account(name: &str, channels: Vec<Value>, api_id: i64, api_hash: &str, session_path: &str) -> Value {
    obj! {"name" => name, "channels" => list(channels), "api_id" => api_id, "api_hash" => api_hash, "session_path" => session_path}
}

fn main_account() -> Value {
    account("main", vec![channel(1, "./collection/telegram/main")], 123, "hash-1", "./data/main")
}

fn with(base: &Value, key: &str, value: Value) -> Value {
    let mut dict = base.as_dict().unwrap().clone();
    dict.insert(key, value);
    Value::Dict(dict)
}

fn messages(error: &ValidationError) -> Vec<String> {
    error.errors.iter().map(|item| item.msg.clone()).collect()
}

// py: tests/test_schedule_config.py::test_schedule_job_accepts_five_field_cron
#[test]
fn schedule_job_accepts_five_field_cron() {
    let job = ScheduleJob::validate(&obj! {"cron" => "*/5 * * * *"}).unwrap();

    assert_eq!(job.schedule.cron, "*/5 * * * *");
    // Defaults are off so an unconfigured deployment boots idle.
    assert!(!job.schedule.enabled);
    // Notifications are the other way around: a source that runs reports by default.
    assert!(job.schedule.notify);
}

// py: tests/test_schedule_config.py::test_schedule_job_rejects_non_five_field_cron
#[test]
fn schedule_job_rejects_non_five_field_cron() {
    assert!(ScheduleJob::validate(&obj! {"cron" => "*/5 * * *"}).is_err());
}

// py: tests/test_schedule_config.py::test_telegram_notification_requires_credentials_to_be_configured
#[test]
fn telegram_notification_requires_credentials_to_be_configured() {
    let cfg = TelegramNotification::validate(
        &obj! {"enabled" => true, "bot_token" => " 123:token ", "chat_id" => " -100123 ", "message_thread_id" => 42_i64},
    )
    .unwrap();

    assert!(cfg.configured());
    assert_eq!(cfg.bot_token, "123:token");
    assert_eq!(cfg.chat_id, "-100123");
}

// py: tests/test_schedule_config.py::test_telegram_notification_rejects_invalid_message_thread_id
#[test]
fn telegram_notification_rejects_invalid_message_thread_id() {
    assert!(TelegramNotification::validate(&obj! {"message_thread_id" => 0_i64}).is_err());
}

// py: tests/test_schedule_config.py::test_telegram_notification_reports_missing_credentials
#[test]
fn telegram_notification_reports_missing_credentials() {
    let cfg = TelegramNotification::validate(&obj! {"enabled" => true}).unwrap();
    assert_eq!(cfg.validate_runnable(), ["bot_token", "chat_id"]);
}

// py: tests/test_schedule_config.py::test_hanime1_ranking_dedupes_periods
#[test]
fn hanime1_ranking_dedupes_periods() {
    let ranking =
        Hanime1Ranking::validate(&obj! {"enabled" => true, "periods" => vec!["weekly", "weekly", "monthly"], "pages" => 1_i64}).unwrap();

    assert!(ranking.enabled);
    assert_eq!(ranking.periods, [RankingPeriod::Weekly, RankingPeriod::Monthly]);
    assert_eq!(ranking.pages, 1);
}

// py: tests/test_schedule_config.py::test_hanime1_ranking_rejects_empty_periods
#[test]
fn hanime1_ranking_rejects_empty_periods() {
    assert!(Hanime1Ranking::validate(&obj! {"periods" => list(vec![])}).is_err());
}

// py: tests/test_schedule_config.py::test_hanime1_ranking_rejects_non_positive_pages
#[test]
fn hanime1_ranking_rejects_non_positive_pages() {
    assert!(Hanime1Ranking::validate(&obj! {"pages" => 0_i64}).is_err());
}

// py: tests/test_schedule_config.py::test_hanime1_ranking_deep_scan_defaults_off
#[test]
fn hanime1_ranking_deep_scan_defaults_off() {
    let ranking = Hanime1Ranking::validate(&obj! {}).unwrap();

    assert!(!ranking.deep_scan.enabled);
    assert!((ranking.deep_scan.quota - DEEP_SCAN_DEFAULT_QUOTA).abs() < f64::EPSILON);
    assert_eq!(ranking.deep_scan.max_extra_pages, DEEP_SCAN_DEFAULT_MAX_EXTRA_PAGES);
}

// py: tests/test_schedule_config.py::test_hanime1_ranking_deep_scan_accepts_fractional_quota
#[test]
fn hanime1_ranking_deep_scan_accepts_fractional_quota() {
    let deep_scan =
        Hanime1RankingDeepScan::validate(&obj! {"enabled" => true, "quota" => DEEP_SCAN_FRACTIONAL_QUOTA, "max_extra_pages" => 3_i64})
            .unwrap();

    assert!((deep_scan.quota - DEEP_SCAN_FRACTIONAL_QUOTA).abs() < f64::EPSILON);
}

// py: tests/test_schedule_config.py::test_hanime1_ranking_deep_scan_rejects_invalid_quota
#[test]
fn hanime1_ranking_deep_scan_rejects_invalid_quota() {
    for quota in [0.0, -1.0, f64::INFINITY, f64::NAN] {
        assert!(Hanime1RankingDeepScan::validate(&obj! {"quota" => quota}).is_err(), "{quota}");
    }
}

// py: tests/test_schedule_config.py::test_hanime1_ranking_deep_scan_rejects_non_positive_max_extra_pages
#[test]
fn hanime1_ranking_deep_scan_rejects_non_positive_max_extra_pages() {
    assert!(Hanime1RankingDeepScan::validate(&obj! {"max_extra_pages" => 0_i64}).is_err());
}

// py: tests/test_schedule_config.py::test_telegram_reports_empty_accounts_as_not_runnable
#[test]
fn telegram_reports_empty_accounts_as_not_runnable() {
    // The settings form must be able to hold a half-filled section, so an empty
    // account list validates but is reported as not runnable.
    let cfg = Telegram::validate(&obj! {"accounts" => list(vec![])}).unwrap();

    assert_eq!(cfg.validate_runnable(), ["accounts"]);
}

// py: tests/test_schedule_config.py::test_telegram_accounts_config_resolves_named_accounts
#[test]
fn telegram_accounts_config_resolves_named_accounts() {
    let cfg = Telegram::validate(&obj! {"accounts" => list(vec![
        account("main", vec![channel(1, "./collection/telegram/main")], 123, "hash-1", "./data/main"),
        account("alt", vec![channel(2, "./collection/telegram/alt")], 456, "hash-2", "./data/alt"),
    ])})
    .unwrap();

    let accounts = cfg.resolved_accounts();

    assert_eq!(
        accounts.iter().map(|account| account.name.as_str()).collect::<Vec<_>>(),
        ["main", "alt"]
    );
    assert_eq!(accounts.iter().map(|account| account.channels[0].id).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(
        accounts.iter().map(|account| account.channels[0].path.clone()).collect::<Vec<_>>(),
        [PyPath::new("./collection/telegram/main"), PyPath::new("./collection/telegram/alt")]
    );
}

// py: tests/test_schedule_config.py::test_telegram_channel_defaults_to_video_media_type
#[test]
fn telegram_channel_defaults_to_video_media_type() {
    let channel = TelegramChannel::validate(&channel(1, "./collection/telegram/main")).unwrap();

    assert_eq!(channel.media_types, [TelegramMediaType::Video]);
}

// py: tests/test_schedule_config.py::test_telegram_channel_accepts_image_media_type_and_dedupes
#[test]
fn telegram_channel_accepts_image_media_type_and_dedupes() {
    let channel = TelegramChannel::validate(
        &obj! {"id" => 1_i64, "path" => "./collection/telegram/main", "media_types" => vec!["video", "image", "image"]},
    )
    .unwrap();

    assert_eq!(channel.media_types, [TelegramMediaType::Video, TelegramMediaType::Image]);
}

// py: tests/test_schedule_config.py::test_telegram_channel_rejects_empty_media_types
#[test]
fn telegram_channel_rejects_empty_media_types() {
    assert!(
        TelegramChannel::validate(&obj! {"id" => 1_i64, "path" => "./collection/telegram/main", "media_types" => list(vec![])}).is_err()
    );
}

// py: tests/test_schedule_config.py::test_telegram_channel_rejects_unknown_media_type
#[test]
fn telegram_channel_rejects_unknown_media_type() {
    assert!(
        TelegramChannel::validate(&obj! {"id" => 1_i64, "path" => "./collection/telegram/main", "media_types" => vec!["photo"]}).is_err()
    );
}

// py: tests/test_schedule_config.py::test_telegram_download_safety_defaults
#[test]
fn telegram_download_safety_defaults() {
    let cfg = Telegram::validate(&obj! {"accounts" => list(vec![main_account()])}).unwrap();

    assert_eq!(cfg.scan_limit, TELEGRAM_DEFAULT_SCAN_LIMIT);
    assert_eq!(cfg.download_limit_per_channel, TELEGRAM_DEFAULT_DOWNLOAD_LIMIT_PER_CHANNEL);
    assert!((cfg.download_delay_seconds - TELEGRAM_DEFAULT_DOWNLOAD_DELAY_SECONDS).abs() < f64::EPSILON);
    assert!((cfg.channel_cooldown_seconds - TELEGRAM_DEFAULT_CHANNEL_COOLDOWN_SECONDS).abs() < f64::EPSILON);
    assert!((cfg.history_wait_seconds - TELEGRAM_DEFAULT_HISTORY_WAIT_SECONDS).abs() < f64::EPSILON);
    assert_eq!(cfg.flood_sleep_threshold_seconds, TELEGRAM_DEFAULT_FLOOD_SLEEP_THRESHOLD_SECONDS);
}

// py: tests/test_schedule_config.py::test_telegram_rejects_invalid_download_safety_values
#[test]
fn telegram_rejects_invalid_download_safety_values() {
    let base = obj! {"accounts" => list(vec![main_account()])};
    for (key, value) in [
        ("scan_limit", Value::from(0_i64)),
        ("download_limit_per_channel", Value::from(0_i64)),
        ("download_delay_seconds", Value::from(-1_i64)),
        ("download_delay_seconds", Value::from(f64::NAN)),
        ("channel_cooldown_seconds", Value::from(f64::INFINITY)),
        ("channel_cooldown_seconds", Value::from(-1_i64)),
        ("history_wait_seconds", Value::from(-1_i64)),
        ("flood_sleep_threshold_seconds", Value::from(-1_i64)),
    ] {
        assert!(Telegram::validate(&with(&base, key, value.clone())).is_err(), "{key}={value:?}");
    }
}

// py: tests/test_schedule_config.py::test_telegram_rejects_duplicate_account_names
#[test]
fn telegram_rejects_duplicate_account_names() {
    let result = Telegram::validate(&obj! {"accounts" => list(vec![
        account("main", vec![channel(1, "./collection/telegram/main")], 123, "hash-1", "./data/main"),
        account("MAIN", vec![channel(2, "./collection/telegram/alt")], 456, "hash-2", "./data/alt"),
    ])});

    assert!(result.is_err());
}

// py: tests/test_schedule_config.py::test_telegram_rejects_unsafe_account_name
#[test]
fn telegram_rejects_unsafe_account_name() {
    assert!(
        TelegramAccount::validate(&account(
            "..",
            vec![channel(1, "./collection/telegram/main")],
            123,
            "hash",
            "./data/main"
        ))
        .is_err()
    );
}

// py: tests/test_schedule_config.py::test_telegram_account_supports_split_media_routes
#[test]
fn telegram_account_supports_split_media_routes() {
    let account = TelegramAccount::validate(&account(
        "main",
        vec![
            obj! {"id" => 3_942_401_424_i64, "path" => "./collection/image", "media_types" => vec!["image"]},
            obj! {"id" => 3_942_401_424_i64, "path" => "./collection/video", "media_types" => vec!["video"]},
        ],
        123,
        "hash",
        "./data/main",
    ))
    .unwrap();

    assert_eq!(
        account
            .channels
            .iter()
            .map(|channel| channel.media_types.clone())
            .collect::<Vec<_>>(),
        [vec![TelegramMediaType::Image], vec![TelegramMediaType::Video]]
    );
    let routes = account.channel_routes();
    assert_eq!(routes.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [3_942_401_424]);
    let media = &routes[0].1;
    let route = |media_type| {
        media
            .iter()
            .find(|(kind, _)| *kind == media_type)
            .map(|(_, channel)| channel.path.clone())
    };
    assert_eq!(route(TelegramMediaType::Image), Some(PyPath::new("./collection/image")));
    assert_eq!(route(TelegramMediaType::Video), Some(PyPath::new("./collection/video")));
}

// py: tests/test_schedule_config.py::test_telegram_account_rejects_duplicate_media_route
#[test]
fn telegram_account_rejects_duplicate_media_route() {
    let error = TelegramAccount::validate(&account(
        "main",
        vec![
            obj! {"id" => 1_i64, "path" => "./collection/one", "media_types" => vec!["image"]},
            obj! {"id" => 1_i64, "path" => "./collection/two", "media_types" => vec!["image"]},
        ],
        123,
        "hash",
        "./data/main",
    ))
    .unwrap_err();

    assert!(
        messages(&error)
            .iter()
            .any(|msg| msg.contains("duplicate telegram media route for channel 1: image")),
        "{error}"
    );
}

// py: tests/test_schedule_config.py::test_telegram_account_checks_routes_after_bot_api_id_normalization
#[test]
fn telegram_account_checks_routes_after_bot_api_id_normalization() {
    let error = TelegramAccount::validate(&account(
        "main",
        vec![
            channel(-1_002_522_897_097, "./collection/one"),
            channel(2_522_897_097, "./collection/two"),
        ],
        123,
        "hash",
        "./data/main",
    ))
    .unwrap_err();

    assert!(
        messages(&error)
            .iter()
            .any(|msg| msg.contains("duplicate telegram media route for channel 2522897097: video")),
        "{error}"
    );
}

#[test]
fn notify_enabled_reads_the_source_toggle_of_a_snapshot() {
    let mut settings = Settings::default();
    assert!(notify_enabled(&settings, "bilibili"));
    settings.job_schedule_mut("bilibili").unwrap().notify = false;
    assert!(!notify_enabled(&settings, " Bilibili "));
    assert!(notify_enabled(&settings, "worker"));
}
