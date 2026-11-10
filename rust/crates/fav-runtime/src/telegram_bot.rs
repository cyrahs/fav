//! Direct notification delivery through the Telegram Bot API.
//!
//! Python origin: `src/tool/telegram_bot.py`. Every send uses `MarkdownV2`; the outbox
//! delivery prefers a local image, then the remote image URL, then plain text, and keeps
//! track of the one message a notification row has pinned.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fav_core::logging::{self, Logger};
use fav_core::mimetypes::MimeTypes;
use fav_core::pytext::py_strip;
use fav_core::settings::Settings;
use fav_net::reqwest;
use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt as _;

use crate::notifications::NotificationRecord;

pub const API_BASE_URL: &str = "https://api.telegram.org";
pub const MAX_IMAGE_BYTES: usize = 9_500_000;
pub const MAX_CAPTION_LENGTH: usize = 1024;
pub const MAX_MESSAGE_LENGTH: usize = 4096;
const MARKDOWN_V2_SPECIAL_CHARS: &str = "\\_*[]()~`>#+-=|{}.!";
/// Every message opens with this, so one glance in a busy chat says which app is talking.
pub const APP_LABEL: &str = "FAV";

pub const CONNECT_TIMEOUT_SECONDS: u64 = 5;
pub const READ_TIMEOUT_SECONDS: u64 = 90;

const RETRYABLE_STATUS_CODES: [i64; 4] = [408, 409, 425, 429];
const SERVER_ERROR_MIN: i64 = 500;

fn log() -> Logger {
    logging::get("telegram-bot")
}

fn is_special(ch: char) -> bool {
    MARKDOWN_V2_SPECIAL_CHARS.contains(ch)
}

/// Escape text for the parse mode every send here uses.
///
/// Lives beside the senders because forgetting it is not a formatting bug: Telegram rejects
/// the whole message with a 400, so a single unescaped `.` loses whatever was being sent.
pub fn escape_markdown_v2(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if is_special(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// The `FAV · <source>` line every message starts with, already escaped.
pub fn format_header_line(header: &str) -> String {
    escape_markdown_v2(&format!("{APP_LABEL} · {header}"))
}

fn with_header_line(header: &str, markdown: &str) -> String {
    let header = py_strip(header);
    if header.is_empty() {
        return markdown.to_owned();
    }
    let header_line = format_header_line(header);
    if markdown.is_empty() {
        header_line
    } else {
        format!("{header_line}\n{markdown}")
    }
}

/// The bot and the chat (and topic) notifications go to.
#[derive(Clone, PartialEq, Eq)]
pub struct TelegramBotConfig {
    pub bot_token: String,
    pub chat_id: String,
    pub message_thread_id: Option<i64>,
}

impl std::fmt::Debug for TelegramBotConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramBotConfig")
            .field("bot_token", &"<redacted>")
            .field("chat_id", &self.chat_id)
            .field("message_thread_id", &self.message_thread_id)
            .finish()
    }
}

/// How one outbox delivery went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramDeliveryResult {
    pub message_id: Option<i64>,
    /// `none`, `local` or `remote`: which image went out, if any.
    pub media_status: &'static str,
    pub warnings: Vec<String>,
    /// The message this delivery left pinned for its notification, if any. The outbox
    /// stores it so the next delivery on the same row knows what to unpin.
    pub pinned_message_id: Option<i64>,
}

/// A failed Bot API call. The message never carries the bot token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct TelegramDeliveryError {
    pub message: String,
    pub retryable: bool,
    pub retry_after_seconds: Option<i64>,
}

impl TelegramDeliveryError {
    fn new(message: String, retryable: bool) -> Self {
        Self {
            message,
            retryable,
            retry_after_seconds: None,
        }
    }

    /// `TelegramNotConfiguredError`.
    pub fn not_configured() -> Self {
        Self::new("Telegram bot_token and chat_id are required".to_owned(), false)
    }

    fn can_fallback_from_photo(&self) -> bool {
        !self.retryable
            && ["error 400", "error 413", "error 415"]
                .iter()
                .any(|code| self.message.contains(code))
    }
}

/// `load_config(require_enabled=...)`: the bot to deliver through, or `None` when it is
/// switched off (and `require_enabled`) or not configured.
pub fn load_config(settings: &Settings, require_enabled: bool) -> Option<TelegramBotConfig> {
    let telegram = &settings.notifications.telegram;
    if require_enabled && !telegram.enabled {
        return None;
    }
    if !telegram.configured() {
        return None;
    }
    Some(TelegramBotConfig {
        bot_token: telegram.bot_token.clone(),
        chat_id: telegram.chat_id.clone(),
        message_thread_id: telegram.message_thread_id,
    })
}

/// A local image ready to upload: `(filename, bytes, content type)` in Python.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoFile {
    pub filename: String,
    pub bytes: Vec<u8>,
    pub content_type: String,
}

/// What `_send_photo` sends: a URL Telegram fetches itself, or a file uploaded here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Photo {
    Url(String),
    File(PhotoFile),
}

/// `isinstance(value, int)`: a JSON integer, or a boolean (a Python `bool` is an `int`).
fn py_int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

fn retryable_status(status_code: i64) -> bool {
    RETRYABLE_STATUS_CODES.contains(&status_code) || status_code >= SERVER_ERROR_MIN
}

fn response_error(status: u16, payload: Option<&Value>, token: &str) -> TelegramDeliveryError {
    let mut error_code = i64::from(status);
    let mut description = String::new();
    let mut retry_after = None;
    if let Some(Value::Object(payload)) = payload {
        if let Some(code) = py_int(payload.get("error_code")) {
            error_code = code;
        }
        if let Some(Value::String(raw)) = payload.get("description") {
            description = if token.is_empty() {
                raw.clone()
            } else {
                raw.replace(token, "[redacted]")
            };
        }
        if let Some(Value::Object(parameters)) = payload.get("parameters") {
            retry_after = py_int(parameters.get("retry_after"));
        }
    }
    let detail = if description.is_empty() {
        String::new()
    } else {
        format!(": {}", description.chars().take(200).collect::<String>())
    };
    TelegramDeliveryError {
        message: format!("Telegram Bot API responded with error {error_code}{detail}"),
        retryable: retryable_status(error_code),
        retry_after_seconds: retry_after,
    }
}

fn message_id(result: &Map<String, Value>) -> Option<i64> {
    py_int(result.get("message_id"))
}

fn request_failed(error: &reqwest::Error) -> TelegramDeliveryError {
    TelegramDeliveryError::new(
        format!("Telegram Bot API request failed: {}", fav_net::httpx_error_class(error)),
        true,
    )
}

/// The Bot API client (`build_client()` plus the senders that take it).
#[derive(Debug, Clone)]
pub struct BotClient {
    http: reqwest::Client,
    base_url: String,
    mime_types: Arc<MimeTypes>,
}

/// The body of one Bot API call.
enum Body {
    Json(Value),
    Multipart {
        fields: Vec<(&'static str, String)>,
        field: &'static str,
        file: PhotoFile,
    },
}

impl BotClient {
    /// A client for the real Bot API.
    pub fn new() -> Result<Self, reqwest::Error> {
        Self::with_base_url(API_BASE_URL)
    }

    /// A client for another Bot API server (a local one, or a fake in tests).
    pub fn with_base_url(base_url: &str) -> Result<Self, reqwest::Error> {
        let http = fav_net::client_builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS))
            .read_timeout(Duration::from_secs(READ_TIMEOUT_SECONDS))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            mime_types: Arc::new(MimeTypes::system().clone()),
        })
    }

    /// Use `mime_types` to tell an image file from another (the system table otherwise).
    #[must_use]
    pub fn with_mime_types(mut self, mime_types: MimeTypes) -> Self {
        self.mime_types = Arc::new(mime_types);
        self
    }

    async fn call_api(&self, config: &TelegramBotConfig, method: &str, body: Body) -> Result<Map<String, Value>, TelegramDeliveryError> {
        let url = format!("{}/bot{}/{method}", self.base_url, config.bot_token);
        let request = self.http.post(url);
        let request = match body {
            Body::Json(payload) => request.json(&payload),
            Body::Multipart { fields, field, file } => {
                let mut form = reqwest::multipart::Form::new();
                for (name, value) in fields {
                    form = form.text(name, value);
                }
                let part = reqwest::multipart::Part::bytes(file.bytes)
                    .file_name(file.filename)
                    .mime_str(&file.content_type)
                    .map_err(|error| request_failed(&error))?;
                request.multipart(form.part(field, part))
            }
        };
        let response = request.send().await.map_err(|error| request_failed(&error))?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|error| request_failed(&error))?;
        let payload: Option<Value> = serde_json::from_slice(&bytes).ok();
        if status.is_success()
            && let Some(Value::Object(payload)) = &payload
            && payload.get("ok") == Some(&Value::Bool(true))
        {
            return Ok(match payload.get("result") {
                Some(Value::Object(result)) => result.clone(),
                _ => Map::new(),
            });
        }
        Err(response_error(status.as_u16(), payload.as_ref(), &config.bot_token))
    }

    /// `_send_message`. A text longer than Telegram allows goes out as truncated plain text.
    pub async fn send_message(
        &self,
        config: &TelegramBotConfig,
        markdown: &str,
        disable_notification: bool,
        disable_web_page_preview: bool,
    ) -> Result<Option<i64>, TelegramDeliveryError> {
        let mut text = py_strip(markdown).to_owned();
        if text.is_empty() {
            "Empty notification".clone_into(&mut text);
        }
        let mut parse_mode = Some("MarkdownV2");
        if text.chars().count() > MAX_MESSAGE_LENGTH {
            let chars: Vec<char> = text.chars().collect();
            let mut plain = String::with_capacity(text.len());
            let mut index = 0;
            while index < chars.len() {
                if chars[index] == '\\' && index + 1 < chars.len() && is_special(chars[index + 1]) {
                    index += 1;
                } else if chars[index] == '*' {
                    index += 1;
                    continue;
                }
                plain.push(chars[index]);
                index += 1;
            }
            text = plain.chars().take(MAX_MESSAGE_LENGTH - 1).collect();
            text.push('…');
            parse_mode = None;
        }
        let mut payload = common_payload(config);
        payload.insert("text".to_owned(), Value::String(text));
        payload.insert("disable_notification".to_owned(), Value::Bool(disable_notification));
        payload.insert("link_preview_options".to_owned(), json!({"is_disabled": disable_web_page_preview}));
        if let Some(mode) = parse_mode {
            payload.insert("parse_mode".to_owned(), Value::from(mode));
        }
        let result = self.call_api(config, "sendMessage", Body::Json(Value::Object(payload))).await?;
        Ok(message_id(&result))
    }

    /// `_send_photo`: the caption goes along only when it fits.
    pub async fn send_photo(
        &self,
        config: &TelegramBotConfig,
        photo: Photo,
        caption: &str,
        disable_notification: bool,
    ) -> Result<Option<i64>, TelegramDeliveryError> {
        let include_caption = !caption.is_empty() && caption.chars().count() <= MAX_CAPTION_LENGTH;
        let result = match photo {
            Photo::Url(url) => {
                let mut payload = common_payload(config);
                payload.insert("photo".to_owned(), Value::String(url));
                payload.insert("disable_notification".to_owned(), Value::Bool(disable_notification));
                if include_caption {
                    payload.insert("caption".to_owned(), Value::from(caption));
                    payload.insert("parse_mode".to_owned(), Value::from("MarkdownV2"));
                }
                self.call_api(config, "sendPhoto", Body::Json(Value::Object(payload))).await?
            }
            Photo::File(file) => {
                let mut fields = vec![("chat_id", config.chat_id.clone())];
                if let Some(thread) = config.message_thread_id {
                    fields.push(("message_thread_id", thread.to_string()));
                }
                fields.push(("disable_notification", disable_notification.to_string()));
                if include_caption {
                    fields.push(("caption", caption.to_owned()));
                    fields.push(("parse_mode", "MarkdownV2".to_owned()));
                }
                // sendPhoto keeps only a GIF's first frame; sendAnimation plays it.
                let (method, field) = if file.content_type == "image/gif" {
                    ("sendAnimation", "animation")
                } else {
                    ("sendPhoto", "photo")
                };
                self.call_api(config, method, Body::Multipart { fields, field, file }).await?
            }
        };
        Ok(message_id(&result))
    }

    async fn pin_message(&self, config: &TelegramBotConfig, message_id: i64) -> Result<(), TelegramDeliveryError> {
        let mut payload = common_payload(config);
        payload.insert("message_id".to_owned(), Value::from(message_id));
        payload.insert("disable_notification".to_owned(), Value::Bool(true));
        self.call_api(config, "pinChatMessage", Body::Json(Value::Object(payload))).await?;
        Ok(())
    }

    async fn unpin_message(&self, config: &TelegramBotConfig, message_id: i64) -> Result<(), TelegramDeliveryError> {
        let mut payload = common_payload(config);
        payload.insert("message_id".to_owned(), Value::from(message_id));
        match self.call_api(config, "unpinChatMessage", Body::Json(Value::Object(payload))).await {
            Ok(_) => Ok(()),
            // A 400 here means the message is gone or was unpinned by hand already; either
            // way there is nothing left to unpin, so it is not worth a retry.
            Err(error) if !error.retryable && error.message.contains("error 400") => {
                log().info(format!("Telegram message {message_id} was already unpinned: {error}"));
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// `_settle_pins`: pin the new message if asked, unpin the one this row had pinned
    /// before. Returns the message that is pinned once the dust settles: the new one, or the
    /// previous one if unpinning it failed and nothing replaced it, so a later delivery gets
    /// another go at clearing it.
    async fn settle_pins(
        &self,
        config: &TelegramBotConfig,
        notification: &NotificationRecord,
        message_id: Option<i64>,
        warnings: &mut Vec<String>,
    ) -> Option<i64> {
        let mut pinned_message_id = None;
        if notification.pin
            && let Some(message_id) = message_id
        {
            match self.pin_message(config, message_id).await {
                Ok(()) => pinned_message_id = Some(message_id),
                Err(error) => {
                    let warning = format!("Pinning failed: {error}");
                    log().warning(format!(
                        "Telegram notification {} was delivered but {warning}",
                        notification.notification_id
                    ));
                    warnings.push(warning);
                }
            }
        }
        let Some(previous) = notification.telegram_pinned_message_id else {
            return pinned_message_id;
        };
        if Some(previous) == pinned_message_id {
            return pinned_message_id;
        }
        if let Err(error) = self.unpin_message(config, previous).await {
            let warning = format!("Unpinning message {previous} failed: {error}");
            log().warning(format!(
                "Telegram notification {} was delivered but {warning}",
                notification.notification_id
            ));
            warnings.push(warning);
            if pinned_message_id.is_none() {
                return Some(previous);
            }
        }
        pinned_message_id
    }

    /// `deliver`: one outbox record, preferring a local image and then its remote URL.
    pub async fn deliver(
        &self,
        notification: &NotificationRecord,
        config: &TelegramBotConfig,
    ) -> Result<TelegramDeliveryResult, TelegramDeliveryError> {
        let mut media_status = "none";
        let mut message_id = None;
        let caption_fits = notification.markdown.chars().count() <= MAX_CAPTION_LENGTH;
        let mut candidates: Vec<(&'static str, Photo)> = Vec::new();
        if let Some(path) = notification.local_image_path()
            && let Some(file) = read_bounded_image(&path, &self.mime_types).await
        {
            candidates.push(("local", Photo::File(file)));
        }
        let image_url = py_strip(&notification.image_url);
        if !image_url.is_empty() {
            candidates.push(("remote", Photo::Url(image_url.to_owned())));
        }
        for (status, photo) in candidates {
            match self
                .send_photo(config, photo, &notification.markdown, notification.disable_notification)
                .await
            {
                Ok(id) => {
                    message_id = id;
                    media_status = status;
                    break;
                }
                Err(error) if error.can_fallback_from_photo() => {
                    log().warning(format!(
                        "Telegram rejected {status} image for notification {}; trying fallback",
                        notification.notification_id
                    ));
                }
                Err(error) => return Err(error),
            }
        }
        if media_status == "none" || !caption_fits {
            message_id = self
                .send_message(
                    config,
                    &notification.markdown,
                    notification.disable_notification,
                    notification.disable_web_page_preview,
                )
                .await?;
        }
        let mut warnings = Vec::new();
        let pinned_message_id = self.settle_pins(config, notification, message_id, &mut warnings).await;
        Ok(TelegramDeliveryResult {
            message_id,
            media_status,
            warnings,
            pinned_message_id,
        })
    }

    /// `send_text_now`: one message right away, outside the outbox.
    ///
    /// The outbox is the right place for anything that can wait. This is for the one thing
    /// that cannot: a login prompt that has to reach the user *during* the run that is
    /// blocked on it. Queued notifications are only flushed after the run returns, which for
    /// a login wait is far too late to be useful.
    pub async fn send_text_now(
        &self,
        settings: &Settings,
        header: &str,
        text: &str,
        require_enabled: bool,
    ) -> Result<Option<i64>, TelegramDeliveryError> {
        let config = load_config(settings, require_enabled).ok_or_else(TelegramDeliveryError::not_configured)?;
        self.send_message(&config, &with_header_line(header, &escape_markdown_v2(text)), false, true)
            .await
    }

    /// `send_photo_now`: one image right away, outside the outbox. See [`Self::send_text_now`].
    pub async fn send_photo_now(
        &self,
        settings: &Settings,
        photo: PhotoFile,
        header: &str,
        caption: &str,
        require_enabled: bool,
    ) -> Result<Option<i64>, TelegramDeliveryError> {
        let config = load_config(settings, require_enabled).ok_or_else(TelegramDeliveryError::not_configured)?;
        self.send_photo(
            &config,
            Photo::File(photo),
            &with_header_line(header, &escape_markdown_v2(caption)),
            false,
        )
        .await
    }

    /// `send_test_notification`: the settings page's test button; works while delivery is off.
    pub async fn send_test_notification(&self, settings: &Settings) -> Result<TelegramDeliveryResult, TelegramDeliveryError> {
        let config = load_config(settings, false).ok_or_else(TelegramDeliveryError::not_configured)?;
        let message_id = self
            .send_message(
                &config,
                &with_header_line("Telegram notifications", &escape_markdown_v2("Notification test")),
                false,
                true,
            )
            .await?;
        Ok(TelegramDeliveryResult {
            message_id,
            media_status: "none",
            warnings: Vec::new(),
            pinned_message_id: None,
        })
    }
}

fn common_payload(config: &TelegramBotConfig) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("chat_id".to_owned(), Value::String(config.chat_id.clone()));
    if let Some(thread) = config.message_thread_id {
        payload.insert("message_thread_id".to_owned(), Value::from(thread));
    }
    payload
}

/// `_read_bounded_image`: the file when its name says image and it is small enough.
async fn read_bounded_image(path: &Path, mime_types: &MimeTypes) -> Option<PhotoFile> {
    let filename = path.file_name()?.to_string_lossy().into_owned();
    let content_type = mime_types.guess_type(&filename).filter(|kind| kind.starts_with("image/"))?;
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES as u64 + 1).read_to_end(&mut bytes).await.ok()?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    Some(PhotoFile {
        filename,
        bytes,
        content_type,
    })
}

#[cfg(test)]
mod tests;
