//! The seam between the Telegram source and its `MTProto` client.
//!
//! `src/web/telegram.py` calls a handful of Telethon methods: `connect`,
//! `is_user_authorized`, `get_entity(PeerChannel(id))`, `iter_messages`, `get_messages`,
//! `download_media`, `disconnect` and the `disconnected` future, plus two event handlers.
//! [`TgClient`] is what those reduce to at the request level (Telethon's own paging and
//! naming live in `history.rs` and `files.rs`), and [`Connector`] builds one per account.
//! `live.rs` implements both over grammers; tests use fakes.

use std::fmt;
use std::path::{Path, PathBuf};

use fav_runtime::BoxFuture;
use tokio::sync::mpsc;

use super::files::Location;
use super::message::TgMessage;
use crate::pydecode::PyError;

/// A channel as `get_entity(PeerChannel(id))` returns it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Channel {
    pub id: i64,
    pub access_hash: i64,
    pub username: Option<String>,
    pub title: Option<String>,
}

impl Channel {
    /// `getattr(channel, 'username', None) or getattr(channel, 'title', str(id)) or str(id)`,
    /// sanitized as a file name.
    pub fn name(&self) -> String {
        let name = self
            .username
            .as_deref()
            .filter(|name| !name.is_empty())
            .or(self.title.as_deref().filter(|title| !title.is_empty()))
            .map_or_else(|| self.id.to_string(), str::to_owned);
        fav_core::filename::sanitize(&name, 200)
    }
}

/// One `messages.getHistory` request, as Telethon's `_MessagesIter` fills it in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryRequest {
    pub offset_id: i64,
    pub offset_date: i64,
    pub add_offset: i64,
    pub limit: i64,
    pub max_id: i64,
    pub min_id: i64,
}

/// One entry of a history page: a message, or a `MessageEmpty` (which only has an id).
#[allow(clippy::large_enum_variant)] // reason: a page holds at most 100; boxing buys nothing
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryItem {
    Empty(i64),
    Message(TgMessage),
}

impl HistoryItem {
    pub fn id(&self) -> i64 {
        match self {
            Self::Empty(id) => *id,
            Self::Message(message) => message.id,
        }
    }
}

/// A `messages.getHistory` answer: its messages, newest first, and whether it is a
/// `messages.Messages` (the whole history at once, not a slice).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryPage {
    pub messages: Vec<HistoryItem>,
    pub complete: bool,
}

/// How a client failure is handled, as the Python `except` clauses tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// `FloodWaitError` and its `seconds`.
    FloodWait(u32),
    /// `MessageIdInvalidError`.
    MessageIdInvalid,
    /// Any other `RPCError`.
    Rpc,
    /// `OSError` (a connection lost, a file that cannot be written).
    Os,
    /// `RuntimeError`.
    Runtime,
    /// Any other exception.
    Other,
}

/// A failure of the client or of the work around it, as the Python exception would read:
/// its class and its `str()`.
#[derive(Clone, PartialEq, Eq)]
pub struct TgError {
    pub class: String,
    pub message: String,
    pub kind: ErrorKind,
}

impl fmt::Debug for TgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TgError({}: {})", self.class, self.message)
    }
}

impl fmt::Display for TgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TgError {}

/// The `OSError` subclasses: what `except OSError` catches.
const OS_ERRORS: [&str; 17] = [
    "OSError",
    "IOError",
    "ConnectionError",
    "ConnectionResetError",
    "ConnectionAbortedError",
    "ConnectionRefusedError",
    "BrokenPipeError",
    "TimeoutError",
    "FileNotFoundError",
    "FileExistsError",
    "PermissionError",
    "IsADirectoryError",
    "NotADirectoryError",
    "InterruptedError",
    "BlockingIOError",
    "ChildProcessError",
    "ProcessLookupError",
];

/// What Telethon's `rpc_message_to_error` makes of an answer, without the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raised {
    pub class: &'static str,
    /// The text before ` (caused by ...)`.
    pub text: String,
    /// The number a pattern error carries (`FloodWaitError.seconds`, the DC of a migrate).
    pub capture: Option<u64>,
    /// `_call` sends the request again two seconds later.
    pub server: bool,
}

/// The message Telegram sent, from grammers' parts: grammers takes the first run of
/// digits out of the name together with the `_` before it (`INTERDC_2_CALL_ERROR` is
/// `INTERDC_CALL_ERROR` and 2), and keeps a name whose first digits follow anything else
/// (`MD5_CHECKSUM_INVALID`, `2FA_CONFIRM_WAIT_30`) whole.
pub fn telegram_message(name: &str, value: Option<u32>) -> String {
    let Some(value) = value else {
        return name.to_owned();
    };
    if let Some(start) = name.find(|ch: char| ch.is_ascii_digit())
        && (start == 0 || name.as_bytes()[start - 1] != b'_')
    {
        return name.to_owned();
    }
    for (prefix, suffix, ..) in super::rpc_errors::PATTERNS {
        if prefix.strip_suffix('_').is_some_and(|head| name.strip_prefix(head) == Some(suffix)) {
            return format!("{prefix}{value}{suffix}");
        }
    }
    format!("{name}_{value}")
}

/// `rpc_message_to_error(RpcError(code, message), request)`: the named error by its exact
/// name (any case), then by pattern, then the base class of the code.
pub fn rpc_message_to_error(code: i32, message: &str) -> Raised {
    let upper = message.to_uppercase();
    if let Some((_, class, text, server)) = super::rpc_errors::EXACT.iter().find(|(name, ..)| *name == upper) {
        return Raised {
            class,
            text: (*text).to_owned(),
            capture: None,
            server: *server,
        };
    }
    for (prefix, suffix, class, text, server) in super::rpc_errors::PATTERNS {
        let Some(rest) = message.strip_prefix(prefix) else { continue };
        let digits = rest.len() - rest.trim_start_matches(|ch: char| ch.is_ascii_digit()).len();
        if digits > 0 && rest[digits..].starts_with(suffix) {
            let capture = rest[..digits].parse::<u64>().unwrap_or(u64::MAX);
            return Raised {
                class,
                text: text.replacen("{}", &capture.to_string(), 1),
                capture: Some(capture),
                server: *server,
            };
        }
    }
    let (class, server) = super::rpc_errors::BASE
        .iter()
        .find(|(base, ..)| *base == code.saturating_abs())
        .map_or(("RPCError", false), |(_, class, server)| (*class, *server));
    // `code or self.code`: a code of 0 falls back to the class's own.
    let shown = match code {
        0 => super::rpc_errors::BASE
            .iter()
            .find(|(_, base_class, _)| *base_class == class)
            .map_or_else(|| "None".to_owned(), |(base, ..)| base.to_string()),
        code => code.to_string(),
    };
    Raised {
        class,
        text: format!("RPCError {shown}: {message}"),
        capture: None,
        server,
    }
}

/// The flood waits `_call` sleeps through up to `flood_sleep_threshold`.
pub const FLOOD_CLASSES: [&str; 4] = [
    "FloodWaitError",
    "FloodPremiumWaitError",
    "SlowModeWaitError",
    "FloodTestPhoneWaitError",
];

impl TgError {
    pub fn new(class: impl Into<String>, message: impl Into<String>, kind: ErrorKind) -> Self {
        Self {
            class: class.into(),
            message: message.into(),
            kind,
        }
    }

    /// `RuntimeError(message)`.
    pub fn runtime(message: impl Into<String>) -> Self {
        Self::new("RuntimeError", message, ErrorKind::Runtime)
    }

    /// An `OSError` from the file system, with Python's text.
    pub fn io(error: &std::io::Error) -> Self {
        Self::new(crate::kit::io_class_name(error), crate::kit::py_os_error_text(error), ErrorKind::Os)
    }

    /// A connection failure (`ConnectionError`).
    pub fn connection(message: impl Into<String>) -> Self {
        Self::new("ConnectionError", message, ErrorKind::Os)
    }

    /// An exception Python code around the client raised (a database or HTTP failure).
    pub fn py(error: &PyError) -> Self {
        let kind = if OS_ERRORS.contains(&error.class.as_str()) {
            ErrorKind::Os
        } else if error.class == "RuntimeError" {
            ErrorKind::Runtime
        } else {
            ErrorKind::Other
        };
        Self::new(error.class.clone(), error.message.clone(), kind)
    }

    /// The `RPCError` Telethon raises for an answer grammers read as `code`, `name` and
    /// `value`. `request` is what `_fmt_request` names (`GetHistoryRequest`, or
    /// `InvokeWithoutUpdatesRequest(GetHistoryRequest)` on a client that receives no
    /// updates), `None` when there was no request.
    pub fn rpc(code: i32, name: &str, value: Option<u32>, request: Option<&str>) -> Self {
        let raised = rpc_message_to_error(code, &telegram_message(name, value));
        Self::raised(&raised, request)
    }

    /// A [`Raised`] error with its ` (caused by ...)`.
    pub fn raised(raised: &Raised, request: Option<&str>) -> Self {
        // `type(request).__name__`, which is `NoneType` without one.
        let message = format!("{} (caused by {})", raised.text, request.unwrap_or("NoneType"));
        let kind = match raised.class {
            // `_call` makes a wait of 0 one second before the source reads `seconds`.
            "FloodWaitError" => ErrorKind::FloodWait(u32::try_from(raised.capture.unwrap_or(0)).unwrap_or(u32::MAX).max(1)),
            "MessageIdInvalidError" => ErrorKind::MessageIdInvalid,
            _ => ErrorKind::Rpc,
        };
        Self::new(raised.class, message, kind)
    }

    /// `f'{exc.__class__.__name__}: {exc}'`.
    pub fn formatted(&self) -> String {
        format!("{}: {}", self.class, self.message)
    }
}

/// What `TelegramClient(...)` is built with.
#[derive(Clone)]
pub struct ClientConfig {
    pub session_path: PathBuf,
    pub api_id: i64,
    pub api_hash: String,
    pub flood_sleep_threshold: i64,
    pub receive_updates: bool,
    pub catch_up: bool,
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("session_path", &self.session_path)
            .field("api_id", &self.api_id)
            .field("api_hash", &"<redacted>")
            .field("flood_sleep_threshold", &self.flood_sleep_threshold)
            .field("receive_updates", &self.receive_updates)
            .field("catch_up", &self.catch_up)
            .finish()
    }
}

/// One connected account.
pub trait TgClient: Send + Sync {
    /// `is_user_authorized()`.
    fn is_authorized(&self) -> BoxFuture<'_, Result<bool, TgError>>;

    /// `get_entity(PeerChannel(channel_id))`.
    fn get_channel(&self, channel_id: i64) -> BoxFuture<'_, Result<Channel, TgError>>;

    /// One `messages.getHistory` call on `channel`.
    fn get_history<'a>(&'a self, channel: &'a Channel, request: HistoryRequest) -> BoxFuture<'a, Result<HistoryPage, TgError>>;

    /// One `channels.getMessages` call for at most 100 ids, an entry per id: `None` for a
    /// message that is empty, deleted or of another channel.
    fn get_messages<'a>(&'a self, channel_id: i64, ids: &'a [i64]) -> BoxFuture<'a, Result<Vec<Option<TgMessage>>, TgError>>;

    /// Download `location` (of `size` bytes, if known) into `path`. `msg_data` is the
    /// `(channel id, message id)` the media came from, to refresh an expired document.
    fn download<'a>(
        &'a self,
        location: &'a Location,
        path: &'a Path,
        size: Option<i64>,
        msg_data: (i64, i64),
    ) -> BoxFuture<'a, Result<(), TgError>>;

    /// `disconnect()`: never fails.
    fn disconnect(&self) -> BoxFuture<'_, ()>;
}

/// A connected client, the new channel messages it receives (when it receives updates),
/// and a future that resolves when the connection is gone (`client.disconnected`).
pub struct Connection {
    pub client: std::sync::Arc<dyn TgClient>,
    pub messages: Option<mpsc::Receiver<TgMessage>>,
    pub disconnected: BoxFuture<'static, Result<(), TgError>>,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection").finish_non_exhaustive()
    }
}

/// `TelegramClient(...)` followed by `connect()`.
pub trait Connector: Send + Sync {
    fn connect(&self, config: ClientConfig) -> BoxFuture<'_, Result<Connection, TgError>>;
}

#[cfg(test)]
mod tests {
    use super::{ErrorKind, TgError, telegram_message};

    #[test]
    fn rpc_errors_read_as_telethon_raises_them() {
        let flood = TgError::rpc(420, "FLOOD_WAIT", Some(12), Some("GetHistoryRequest"));
        assert_eq!(flood.kind, ErrorKind::FloodWait(12));
        assert_eq!(
            flood.formatted(),
            "FloodWaitError: A wait of 12 seconds is required (caused by GetHistoryRequest)"
        );
        assert_eq!(TgError::rpc(420, "FLOOD_WAIT", Some(0), None).kind, ErrorKind::FloodWait(1));
        let private = TgError::rpc(400, "CHANNEL_PRIVATE", None, Some("GetChannelsRequest"));
        assert_eq!(private.kind, ErrorKind::Rpc);
        assert_eq!(
            private.formatted(),
            "ChannelPrivateError: The channel specified is private and you lack permission to access it. Another reason may \
             be that you were banned from it (caused by GetChannelsRequest)"
        );
        assert_eq!(
            TgError::rpc(400, "MESSAGE_ID_INVALID", None, None).kind,
            ErrorKind::MessageIdInvalid
        );
        assert_eq!(TgError::rpc(420, "FLOOD_PREMIUM_WAIT", Some(5), None).kind, ErrorKind::Rpc);
        assert_eq!(
            TgError::rpc(0, "NEVER_SEEN", None, None).message,
            "RPCError None: NEVER_SEEN (caused by NoneType)"
        );
    }

    #[test]
    fn grammers_names_are_read_back_into_telegrams_message() {
        assert_eq!(telegram_message("FLOOD_WAIT", Some(30)), "FLOOD_WAIT_30");
        assert_eq!(telegram_message("INTERDC_CALL_ERROR", Some(2)), "INTERDC_2_CALL_ERROR");
        assert_eq!(
            telegram_message("PREVIOUS_CHAT_IMPORT_ACTIVE_WAITMIN", Some(5)),
            "PREVIOUS_CHAT_IMPORT_ACTIVE_WAIT_5MIN"
        );
        assert_eq!(telegram_message("MD5_CHECKSUM_INVALID", Some(5)), "MD5_CHECKSUM_INVALID");
        assert_eq!(telegram_message("CHANNEL_PRIVATE", None), "CHANNEL_PRIVATE");
        assert_eq!(telegram_message("SOMETHING_NEW", Some(42)), "SOMETHING_NEW_42");
    }
}
