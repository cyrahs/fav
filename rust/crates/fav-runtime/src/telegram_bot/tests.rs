use chrono::{TimeZone as _, Utc};
use fav_db::settings_store::default_settings;
use serde_json::{Value, json};

use super::*;
use crate::notifications::NotificationRecord;
use crate::testutil::{Answer, FakeBotApi, golden_json};

fn config() -> TelegramBotConfig {
    TelegramBotConfig {
        bot_token: "123:secret-token".to_owned(),
        chat_id: "-100123".to_owned(),
        message_thread_id: Some(42),
    }
}

/// `_notification()` of the Python tests.
fn notification() -> NotificationRecord {
    let now = Utc.with_ymd_and_hms(2026, 3, 2, 0, 0, 0).unwrap();
    NotificationRecord {
        notification_id: 7,
        kind: "job_failed".to_owned(),
        source: "worker".to_owned(),
        header: String::new(),
        title: "Job failed".to_owned(),
        body: "boom".to_owned(),
        link_url: String::new(),
        image_url: String::new(),
        payload: "{}".to_owned(),
        dedupe_key: "job_failed:test:boom".to_owned(),
        status: "unread".to_owned(),
        markdown: "*Job failed*\nboom".to_owned(),
        disable_web_page_preview: true,
        disable_notification: false,
        pin: false,
        webhook_action: "upsert".to_owned(),
        occurrence_count: 1,
        event_version: 3,
        delivery_status: "sending".to_owned(),
        attempt_count: 0,
        next_attempt_at: now,
        created_at: now,
        read_at: None,
        delivered_at: None,
        last_error: String::new(),
        scope: "item".to_owned(),
        telegram_message_id: None,
        telegram_pinned_message_id: None,
    }
}

async fn deliver(api: &FakeBotApi, notification: &NotificationRecord) -> Result<TelegramDeliveryResult, TelegramDeliveryError> {
    api.client().deliver(notification, &config()).await
}

fn error_403() -> Answer {
    Answer::json(200, json!({"ok": false, "error_code": 403, "description": "not enough rights"}))
}

// py: tests/test_telegram_bot.py::test_load_config_requires_enabled_for_worker_but_not_test
#[test]
fn load_config_requires_enabled_for_worker_but_not_test() {
    let mut settings = default_settings();
    settings.notifications.telegram.bot_token = "123:token".to_owned();
    settings.notifications.telegram.chat_id = "-100123".to_owned();

    assert_eq!(load_config(&settings, true), None);
    assert_eq!(
        load_config(&settings, false),
        Some(TelegramBotConfig {
            bot_token: "123:token".to_owned(),
            chat_id: "-100123".to_owned(),
            message_thread_id: None,
        })
    );
    settings.notifications.telegram.enabled = true;
    assert!(load_config(&settings, true).is_some());
    // Not configured is None either way.
    settings.notifications.telegram.chat_id = String::new();
    assert_eq!(load_config(&settings, false), None);
}

// py: tests/test_telegram_bot.py::test_deliver_sends_markdown_to_configured_topic
#[tokio::test]
async fn deliver_sends_markdown_to_configured_topic() {
    let api = FakeBotApi::start().await;
    let result = deliver(&api, &notification()).await.expect("delivered");
    assert_eq!(result.message_id, Some(99));
    let requests: Vec<Value> = api.received().iter().map(super::super::testutil::Received::json).collect();
    assert_eq!(
        requests,
        vec![json!({
            "chat_id": "-100123",
            "message_thread_id": 42,
            "text": "*Job failed*\nboom",
            "parse_mode": "MarkdownV2",
            "disable_notification": false,
            "link_preview_options": {"is_disabled": true},
        })]
    );
}

// py: tests/test_telegram_bot.py::test_deliver_falls_back_to_text_when_remote_photo_is_rejected
#[tokio::test]
async fn deliver_falls_back_to_text_when_remote_photo_is_rejected() {
    let api = FakeBotApi::start().await;
    api.script(
        "sendPhoto",
        vec![Answer::json(200, json!({"ok": false, "error_code": 400, "description": "wrong file identifier"}))],
    );
    api.script("sendMessage", vec![Answer::json(200, json!({"ok": true, "result": {"message_id": 101}}))]);
    let mut record = notification();
    record.image_url = "https://images.example/test.png".to_owned();
    let result = deliver(&api, &record).await.expect("delivered");
    assert_eq!(api.methods(), ["sendPhoto", "sendMessage"]);
    assert_eq!(result.message_id, Some(101));
    assert_eq!(result.media_status, "none");
}

// py: tests/test_telegram_bot.py::test_deliver_sends_a_local_gif_as_an_animation
#[tokio::test]
async fn deliver_sends_a_local_gif_as_an_animation() {
    let directory = tempfile::tempdir().expect("tempdir");
    let gif = directory.path().join("sticker.gif");
    std::fs::write(&gif, b"GIF89a-frames").expect("write gif");
    let api = FakeBotApi::start().await;
    api.script("sendAnimation", vec![Answer::json(200, json!({"ok": true, "result": {"message_id": 102}}))]);
    let mut record = notification();
    record.payload = json!({"image_path": gif.to_str().expect("utf-8 path")}).to_string();
    let result = deliver(&api, &record).await.expect("delivered");

    let [request] = api.received().try_into().expect("one request");
    assert!(request.path.ends_with("/sendAnimation"), "{}", request.path);
    let body = String::from_utf8_lossy(&request.body);
    assert!(body.contains("name=\"animation\"; filename=\"sticker.gif\""), "{body}");
    assert_eq!(result.media_status, "local");
    assert_eq!(result.message_id, Some(102));
}

// py: tests/test_telegram_bot.py::test_deliver_exposes_retry_after_without_leaking_token
#[tokio::test]
async fn deliver_exposes_retry_after_without_leaking_token() {
    let token = config().bot_token;
    let api = FakeBotApi::start().await;
    api.script(
        "sendMessage",
        vec![Answer::json(
            429,
            json!({"ok": false, "error_code": 429, "description": format!("retry for {token}"), "parameters": {"retry_after": 17}}),
        )],
    );
    let error = deliver(&api, &notification()).await.expect_err("rate limited");
    assert!(error.retryable);
    assert_eq!(error.retry_after_seconds, Some(17));
    assert!(!error.to_string().contains(&token), "{error}");
}

// py: tests/test_telegram_bot.py::test_pin_failure_is_best_effort
#[tokio::test]
async fn pin_failure_is_best_effort() {
    let api = FakeBotApi::start().await;
    api.script("pinChatMessage", vec![error_403()]);
    let mut record = notification();
    record.pin = true;
    let result = deliver(&api, &record).await.expect("delivered");
    assert_eq!(api.methods(), ["sendMessage", "pinChatMessage"]);
    assert_eq!(result.message_id, Some(99));
    assert_eq!(
        result.warnings,
        ["Pinning failed: Telegram Bot API responded with error 403: not enough rights"]
    );
}

// py: tests/test_telegram_bot.py::test_long_markdown_uses_truncated_plain_text
#[tokio::test]
async fn long_markdown_uses_truncated_plain_text() {
    let api = FakeBotApi::start().await;
    let mut record = notification();
    record.markdown = format!("*Title*\n{}", "x".repeat(MAX_MESSAGE_LENGTH));
    deliver(&api, &record).await.expect("delivered");
    let payload = api.received()[0].json();
    assert_eq!(payload["text"].as_str().expect("text").chars().count(), MAX_MESSAGE_LENGTH);
    assert!(payload.get("parse_mode").is_none());
}

// py: tests/test_telegram_bot.py::test_send_text_now_opens_with_the_same_header_line_as_the_outbox
#[tokio::test]
async fn send_text_now_opens_with_the_same_header_line_as_the_outbox() {
    let mut settings = default_settings();
    settings.notifications.telegram.bot_token = "123:token".to_owned();
    settings.notifications.telegram.chat_id = "-100123".to_owned();
    settings.notifications.telegram.enabled = true;
    let api = FakeBotApi::start().await;
    api.script("sendMessage", vec![Answer::json(200, json!({"ok": true, "result": {"message_id": 5}}))]);
    let message_id = api.client().send_text_now(&settings, "RedNote", "Signed in.", true).await.expect("sent");
    assert_eq!(message_id, Some(5));
    assert_eq!(api.received()[0].json()["text"], "FAV · RedNote\nSigned in\\.");
}

#[tokio::test]
async fn immediate_sends_need_a_configured_bot() {
    let api = FakeBotApi::start().await;
    let mut settings = default_settings();
    let client = api.client();
    let error = client.send_text_now(&settings, "", "hi", true).await.expect_err("not configured");
    assert_eq!(error, TelegramDeliveryError::not_configured());
    assert!(!error.retryable);
    // The settings page's test works while delivery is switched off.
    settings.notifications.telegram.bot_token = "123:token".to_owned();
    settings.notifications.telegram.chat_id = "-100123".to_owned();
    assert!(client.send_text_now(&settings, "", "hi", true).await.is_err());
    let result = client.send_test_notification(&settings).await.expect("test sent");
    assert_eq!(result.message_id, Some(99));
    assert_eq!(api.received()[0].json()["text"], "FAV · Telegram notifications\nNotification test");
    let photo = PhotoFile {
        filename: "qr.png".to_owned(),
        bytes: vec![1, 2, 3],
        content_type: "image/png".to_owned(),
    };
    client.send_photo_now(&settings, photo, "X", "Scan me.", false).await.expect("photo sent");
    let (fields, files) = api.received()[1].multipart();
    assert_eq!(fields["caption"], "FAV · X\nScan me\\.");
    assert_eq!(fields["disable_notification"], "false");
    assert_eq!(files, [json!({"field": "photo", "filename": "qr.png", "content_type": "image/png", "size": 3})]);
}

// py: tests/test_telegram_bot.py::test_resolve_delivery_unpins_the_previous_failure_message
#[tokio::test]
async fn resolve_delivery_unpins_the_previous_failure_message() {
    let api = FakeBotApi::start().await;
    let mut record = notification();
    record.webhook_action = "resolve".to_owned();
    record.telegram_pinned_message_id = Some(55);
    let result = deliver(&api, &record).await.expect("delivered");
    assert_eq!(api.methods(), ["sendMessage", "unpinChatMessage"]);
    assert_eq!(api.received()[1].json(), json!({"chat_id": "-100123", "message_thread_id": 42, "message_id": 55}));
    assert_eq!(result.message_id, Some(99));
    assert_eq!(result.pinned_message_id, None);
    assert!(result.warnings.is_empty());
}

// py: tests/test_telegram_bot.py::test_pinned_repeat_replaces_the_previous_pin
#[tokio::test]
async fn pinned_repeat_replaces_the_previous_pin() {
    let api = FakeBotApi::start().await;
    let mut record = notification();
    record.pin = true;
    record.telegram_pinned_message_id = Some(55);
    let result = deliver(&api, &record).await.expect("delivered");
    assert_eq!(api.methods(), ["sendMessage", "pinChatMessage", "unpinChatMessage"]);
    let received = api.received();
    assert_eq!(received[1].json()["message_id"], 99);
    assert_eq!(received[2].json()["message_id"], 55);
    assert_eq!(result.pinned_message_id, Some(99));
}

// py: tests/test_telegram_bot.py::test_delivery_without_a_previous_pin_does_not_unpin
#[tokio::test]
async fn delivery_without_a_previous_pin_does_not_unpin() {
    let api = FakeBotApi::start().await;
    let mut record = notification();
    record.pin = true;
    let result = deliver(&api, &record).await.expect("delivered");
    assert_eq!(api.methods(), ["sendMessage", "pinChatMessage"]);
    assert_eq!(result.pinned_message_id, Some(99));
}

// py: tests/test_telegram_bot.py::test_unpin_tolerates_a_message_that_is_already_gone
#[tokio::test]
async fn unpin_tolerates_a_message_that_is_already_gone() {
    let api = FakeBotApi::start().await;
    api.script(
        "unpinChatMessage",
        vec![Answer::json(400, json!({"ok": false, "error_code": 400, "description": "message to unpin not found"}))],
    );
    let mut record = notification();
    record.webhook_action = "resolve".to_owned();
    record.telegram_pinned_message_id = Some(55);
    let result = deliver(&api, &record).await.expect("delivered");
    assert_eq!(api.methods(), ["sendMessage", "unpinChatMessage"]);
    assert_eq!(result.pinned_message_id, None);
    assert!(result.warnings.is_empty());
}

// py: tests/test_telegram_bot.py::test_unpin_failure_keeps_the_previous_pin_for_a_later_attempt
#[tokio::test]
async fn unpin_failure_keeps_the_previous_pin_for_a_later_attempt() {
    let api = FakeBotApi::start().await;
    api.script("unpinChatMessage", vec![error_403()]);
    let mut record = notification();
    record.webhook_action = "resolve".to_owned();
    record.telegram_pinned_message_id = Some(55);
    let result = deliver(&api, &record).await.expect("delivered");
    assert_eq!(api.methods(), ["sendMessage", "unpinChatMessage"]);
    assert_eq!(result.pinned_message_id, Some(55));
    assert_eq!(
        result.warnings,
        ["Unpinning message 55 failed: Telegram Bot API responded with error 403: not enough rights"]
    );
}

// py: tests/test_telegram_bot.py::test_unpin_failure_does_not_override_a_new_pin
#[tokio::test]
async fn unpin_failure_does_not_override_a_new_pin() {
    let api = FakeBotApi::start().await;
    api.script("unpinChatMessage", vec![error_403()]);
    let mut record = notification();
    record.pin = true;
    record.telegram_pinned_message_id = Some(55);
    let result = deliver(&api, &record).await.expect("delivered");
    assert_eq!(api.methods(), ["sendMessage", "pinChatMessage", "unpinChatMessage"]);
    assert_eq!(result.pinned_message_id, Some(99));
    assert_eq!(result.warnings.len(), 1);
}

#[tokio::test]
async fn a_transport_failure_is_retryable_and_names_the_httpx_class() {
    // Nothing listens on port 1.
    let client = BotClient::with_base_url("http://127.0.0.1:1").expect("client");
    let error = client.deliver(&notification(), &config()).await.expect_err("refused");
    assert_eq!(error.message, "Telegram Bot API request failed: ConnectError");
    assert!(error.retryable);
    assert_eq!(error.retry_after_seconds, None);
}

#[test]
fn the_config_debug_hides_the_token() {
    let text = format!("{:?}", config());
    assert!(!text.contains("secret"), "{text}");
}

/// The notification a `deliver.json` scenario describes.
fn scenario_notification(overrides: &Value, image_path: Option<&str>) -> NotificationRecord {
    let mut record = notification();
    "FAV · Test\n*Job failed*\nboom".clone_into(&mut record.markdown);
    if let Some(path) = image_path {
        record.payload = json!({"image_path": path}).to_string();
    }
    for (key, value) in overrides.as_object().expect("overrides") {
        match key.as_str() {
            "image_url" => record.image_url = value.as_str().expect("str").to_owned(),
            "markdown" => record.markdown = value.as_str().expect("str").to_owned(),
            "webhook_action" => record.webhook_action = value.as_str().expect("str").to_owned(),
            "pin" => record.pin = value.as_bool().expect("bool"),
            "disable_web_page_preview" => record.disable_web_page_preview = value.as_bool().expect("bool"),
            "telegram_pinned_message_id" => record.telegram_pinned_message_id = value.as_i64(),
            other => panic!("unhandled override {other}"),
        }
    }
    record
}

#[tokio::test]
async fn deliver_matches_python_for_every_scripted_scenario() {
    let golden = golden_json("notifications/deliver.json");
    for case in golden["cases"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let directory = tempfile::tempdir().expect("tempdir");
        let image_path = if let Some(image) = case["image"].as_array() {
            let name = image[0].as_str().expect("name");
            let size = usize::try_from(image[1].as_u64().expect("size")).expect("size");
            let bytes = if name.ends_with(".gif") {
                let mut bytes = b"GIF89a".to_vec();
                bytes.resize(size, 0);
                bytes
            } else {
                vec![0x89; size]
            };
            let path = directory.path().join(name);
            std::fs::write(&path, bytes).expect("write image");
            Some(path)
        } else {
            case["missing_image"].as_str().map(|name| directory.path().join(name))
        };
        let api = FakeBotApi::start().await;
        for (method, answers) in case["responses"].as_object().expect("responses") {
            api.script(method, answers.as_array().expect("answers").iter().map(Answer::from_golden).collect());
        }
        let record = scenario_notification(&case["notification"], image_path.as_deref().map(|path| path.to_str().expect("utf-8")));
        let mut bot = config();
        bot.message_thread_id = case["thread"].as_i64();
        let outcome = api.client().deliver(&record, &bot).await;

        let requests: Vec<Value> = api.received().iter().map(super::super::testutil::Received::recorded).collect();
        assert_eq!(Value::Array(requests), case["requests"], "{label}: requests");
        match outcome {
            Ok(result) => {
                let expected = &case["result"];
                assert_eq!(result.message_id, expected["message_id"].as_i64(), "{label}");
                assert_eq!(result.media_status, expected["media_status"], "{label}");
                assert_eq!(json!(result.warnings), expected["warnings"], "{label}");
                assert_eq!(result.pinned_message_id, expected["pinned_message_id"].as_i64(), "{label}");
            }
            Err(error) => {
                let expected = &case["error"];
                assert!(expected.is_object(), "{label}: unexpected error {error:?}");
                assert_eq!(error.message, expected["message"], "{label}");
                assert_eq!(error.retryable, expected["retryable"], "{label}");
                assert_eq!(error.retry_after_seconds, expected["retry_after_seconds"].as_i64(), "{label}");
            }
        }
    }
}

#[tokio::test]
async fn send_message_matches_python() {
    let golden = golden_json("notifications/send_message.json");
    for case in golden["cases"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let api = FakeBotApi::start().await;
        let mut bot = config();
        bot.message_thread_id = case["message_thread_id"].as_i64();
        api.client()
            .send_message(
                &bot,
                case["markdown"].as_str().expect("markdown"),
                case["disable_notification"].as_bool().expect("bool"),
                case["disable_web_page_preview"].as_bool().expect("bool"),
            )
            .await
            .expect("sent");
        assert_eq!(api.received()[0].json(), case["payload"], "{label}");
    }
}

#[test]
fn escaping_and_image_types_match_python() {
    let golden = golden_json("notifications/text.json");
    let table = MimeTypes::defaults();
    let mut checked = 0;
    for case in golden["cases"].as_array().expect("cases") {
        match case["function"].as_str().expect("function") {
            "escape" => {
                let input = case["input"].as_str().expect("input");
                assert_eq!(escape_markdown_v2(input), case["escape_markdown_v2"], "{input:?}");
                assert_eq!(format_header_line(input), case["format_header_line"], "{input:?}");
                checked += 1;
            }
            "guess_type" => {
                let name = case["name"].as_str().expect("name");
                let ours = table.guess_type(name);
                // The table carries only the image types; that is all `deliver` asks about.
                match case["result"].as_str() {
                    Some(expected) if expected.starts_with("image/") => assert_eq!(ours.as_deref(), Some(expected), "{name:?}"),
                    _ => assert!(!ours.is_some_and(|kind| kind.starts_with("image/")), "{name:?}"),
                }
                checked += 1;
            }
            _ => {}
        }
    }
    assert!(checked > 30, "{checked}");
}
