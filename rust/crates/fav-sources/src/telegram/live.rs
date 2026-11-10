//! The real client: grammers over Telethon's session file.
//!
//! Requests go out as the raw calls Telethon makes (`channels.getChannels`,
//! `messages.getHistory`, `channels.getMessages`, `upload.getFile`), and the answers are
//! read into the model of `message.rs`. The retry policy is Telethon's `_call`: up to six
//! attempts, two seconds after a Telegram-side failure, and flood waits slept through up to
//! `flood_sleep_threshold` (D150). This module is never run against Telegram in tests
//! (WP15 is verified under mocks only); its conversions are.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use fav_runtime::BoxFuture;
use grammers_client::media::Downloadable;
use grammers_client::sender::{ConnectionParams, RetryContext, RetryPolicy, RpcError, UpdatesConfiguration};
use grammers_client::session::Session as _;
use grammers_client::session::types::{PeerAuth, PeerId, PeerInfo};
use grammers_client::{Client, InvocationError, SenderPool, tl};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{mpsc, oneshot};

use super::client::{Channel, ClientConfig, Connection, Connector, ErrorKind, HistoryItem, HistoryPage, HistoryRequest, TgClient, TgError};
use super::client::{FLOOD_CLASSES, rpc_message_to_error, telegram_message};
use super::files::Location;
use super::message::{Document, DocumentAttribute, Media, Photo, PhotoSize, TgMessage, WebPage};
use super::session::TelethonSession;

/// `request_retries=5`: the first try and five more.
const MAX_ATTEMPTS: u32 = 6;
/// How often the update state is written to the session (Telethon saves it every minute).
const STATE_SAVE_INTERVAL: Duration = Duration::from_mins(1);

/// Telethon's `_call` retry rules as a grammers [`RetryPolicy`], with its memory of flood
/// waits (`_flood_waited_requests`), shared by the policy and the client.
#[derive(Debug, Clone)]
pub struct TelethonRetries {
    pub flood_sleep_threshold: Duration,
    /// `not receive_updates`: Telethon wraps each request in `InvokeWithoutUpdatesRequest`.
    pub no_updates: bool,
    flood_waited: Arc<Mutex<HashMap<u32, f64>>>,
}

/// `time.time()`.
fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// Telethon's request class name for a grammers constructor id: `channels.getChannels`
/// is `GetChannelsRequest`.
pub fn request_name(constructor: u32) -> String {
    let name = tl::name_for_id(constructor);
    let short = name.rsplit('.').next().unwrap_or(name);
    let mut chars = short.chars();
    chars
        .next()
        .map_or_else(String::new, |first| format!("{}{}Request", first.to_uppercase(), chars.as_str()))
}

/// The Telethon error of a failed invocation on a client that receives updates.
pub fn invocation_error(error: &InvocationError) -> TgError {
    invocation_error_for(error, false)
}

fn invocation_error_for(error: &InvocationError, no_updates: bool) -> TgError {
    match error {
        InvocationError::Rpc(rpc) => {
            let request = rpc.caused_by.map(|constructor| {
                let name = request_name(constructor);
                if no_updates {
                    format!("InvokeWithoutUpdatesRequest({name})")
                } else {
                    name
                }
            });
            TgError::rpc(rpc.code, &rpc.name, rpc.value, request.as_deref())
        }
        InvocationError::Deserialize(error) => TgError::new("TypeNotFoundError", error.to_string(), ErrorKind::Other),
        InvocationError::Session(error) => TgError::new("OperationalError", error.to_string(), ErrorKind::Other),
        other => TgError::connection(other.to_string()),
    }
}

impl TelethonRetries {
    pub fn new(flood_sleep_threshold: Duration, no_updates: bool) -> Self {
        Self {
            flood_sleep_threshold,
            no_updates,
            flood_waited: Arc::default(),
        }
    }

    fn flood_waited(&self) -> std::sync::MutexGuard<'_, HashMap<u32, f64>> {
        self.flood_waited.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How long `_call` sleeps before sending the request again after this error, or
    /// `None` when it raises the error at once.
    fn rpc_wait(&self, rpc: &RpcError) -> Option<Duration> {
        let raised = rpc_message_to_error(rpc.code, &telegram_message(&rpc.name, rpc.value));
        if raised.server {
            return Some(Duration::from_secs(2));
        }
        if FLOOD_CLASSES.contains(&raised.class) {
            // A wait of 0 is slept as 1 second.
            let wait = Duration::from_secs(raised.capture.unwrap_or(0).max(1));
            return (wait <= self.flood_sleep_threshold).then_some(wait);
        }
        None
    }

    /// `self._flood_waited_requests[request.CONSTRUCTOR_ID] = time.time() + e.seconds` for
    /// every flood wait but slow mode. With no updates `request` is the wrapping
    /// `InvokeWithoutUpdatesRequest`, which the check before a request never looks up, so
    /// nothing is remembered.
    fn remember(&self, rpc: &RpcError) {
        let raised = rpc_message_to_error(rpc.code, &telegram_message(&rpc.name, rpc.value));
        if self.no_updates || !FLOOD_CLASSES.contains(&raised.class) || raised.class == "SlowModeWaitError" {
            return;
        }
        if let Some(constructor) = rpc.caused_by {
            #[allow(clippy::cast_precision_loss)] // reason: a wait in seconds
            let due = now() + raised.capture.unwrap_or(0) as f64;
            self.flood_waited().insert(constructor, due);
        }
    }

    /// The check `_call` makes before sending: a request still in a flood wait sleeps
    /// through it up to the threshold (a wait of 3 seconds or less is forgotten), and
    /// raises `FloodWaitError` past it without being sent.
    pub async fn before(&self, constructor: u32) -> Result<(), TgError> {
        let Some(due) = self.flood_waited().get(&constructor).copied() else {
            return Ok(());
        };
        let diff = (due - now()).round_ties_even();
        if diff <= 3.0 {
            self.flood_waited().remove(&constructor);
            return Ok(());
        }
        if diff <= self.flood_sleep_threshold.as_secs_f64() {
            tokio::time::sleep(Duration::from_secs_f64(diff)).await;
            self.flood_waited().remove(&constructor);
            return Ok(());
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // reason: a wait over 3 seconds
        let seconds = diff as u64;
        let raised = rpc_message_to_error(420, &format!("FLOOD_WAIT_{seconds}"));
        Err(TgError::raised(&raised, Some(&request_name(constructor))))
    }

    /// The error `_call` raises once a request has failed for good: an error it would
    /// have retried means all six attempts failed, and Telethon then raises
    /// `ValueError('Request was unsuccessful 6 time(s)')` rather than the last error.
    pub fn final_error(&self, error: &InvocationError) -> TgError {
        if let InvocationError::Rpc(rpc) = error
            && self.rpc_wait(rpc).is_some()
        {
            return TgError::new(
                "ValueError",
                format!("Request was unsuccessful {MAX_ATTEMPTS} time(s)"),
                ErrorKind::Other,
            );
        }
        invocation_error_for(error, self.no_updates)
    }
}

impl RetryPolicy for TelethonRetries {
    fn should_retry(&self, ctx: &RetryContext) -> ControlFlow<(), Duration> {
        if let InvocationError::Rpc(rpc) = &ctx.error {
            self.remember(rpc);
        }
        if ctx.fail_count.get() >= MAX_ATTEMPTS {
            return ControlFlow::Break(());
        }
        match &ctx.error {
            InvocationError::Rpc(rpc) => self.rpc_wait(rpc).map_or(ControlFlow::Break(()), ControlFlow::Continue),
            // Telethon reconnects and sends the request again, a second apart.
            InvocationError::Io(_) | InvocationError::Dropped(_) | InvocationError::Transport(_) => {
                ControlFlow::Continue(Duration::from_secs(1))
            }
            _ => ControlFlow::Break(()),
        }
    }
}

// ---------- tl to the model ----------

fn photo_sizes(sizes: &[tl::enums::PhotoSize]) -> Vec<PhotoSize> {
    sizes
        .iter()
        .map(|size| match size {
            tl::enums::PhotoSize::Empty(size) => PhotoSize::Empty { kind: size.r#type.clone() },
            tl::enums::PhotoSize::Size(size) => PhotoSize::Size {
                kind: size.r#type.clone(),
                size: i64::from(size.size),
            },
            tl::enums::PhotoSize::PhotoCachedSize(size) => PhotoSize::Cached {
                kind: size.r#type.clone(),
                bytes: size.bytes.clone(),
            },
            tl::enums::PhotoSize::PhotoStrippedSize(size) => PhotoSize::Stripped {
                kind: size.r#type.clone(),
                bytes: size.bytes.clone(),
            },
            tl::enums::PhotoSize::Progressive(size) => PhotoSize::Progressive {
                kind: size.r#type.clone(),
                sizes: size.sizes.iter().map(|size| i64::from(*size)).collect(),
            },
            tl::enums::PhotoSize::PhotoPathSize(size) => PhotoSize::Path { kind: size.r#type.clone() },
        })
        .collect()
}

fn video_sizes(sizes: &[tl::enums::VideoSize]) -> Vec<PhotoSize> {
    sizes
        .iter()
        .map(|size| match size {
            tl::enums::VideoSize::Size(size) => PhotoSize::Video {
                kind: size.r#type.clone(),
                size: i64::from(size.size),
            },
            // Telethon's `_get_thumb` sorts these first and never picks them over a size.
            tl::enums::VideoSize::EmojiMarkup(_) | tl::enums::VideoSize::StickerMarkup(_) => PhotoSize::Empty { kind: String::new() },
        })
        .collect()
}

/// A `types.Photo`, `None` for `PhotoEmpty`.
pub fn photo(photo: &tl::enums::Photo) -> Option<Photo> {
    let tl::enums::Photo::Photo(photo) = photo else { return None };
    Some(Photo {
        id: photo.id,
        access_hash: photo.access_hash,
        file_reference: photo.file_reference.clone(),
        dc_id: photo.dc_id,
        sizes: photo_sizes(&photo.sizes),
        video_sizes: video_sizes(photo.video_sizes.as_deref().unwrap_or_default()),
    })
}

/// A `types.Document`, `None` for `DocumentEmpty`.
pub fn document(document: &tl::enums::Document) -> Option<Document> {
    let tl::enums::Document::Document(document) = document else {
        return None;
    };
    let attributes = document
        .attributes
        .iter()
        .map(|attr| match attr {
            tl::enums::DocumentAttribute::Video(video) => DocumentAttribute::Video {
                round_message: video.round_message,
            },
            tl::enums::DocumentAttribute::Sticker(_) => DocumentAttribute::Sticker,
            tl::enums::DocumentAttribute::CustomEmoji(_) => DocumentAttribute::CustomEmoji,
            tl::enums::DocumentAttribute::Filename(name) => DocumentAttribute::Filename(name.file_name.clone()),
            tl::enums::DocumentAttribute::Audio(audio) => DocumentAttribute::Audio {
                voice: audio.voice,
                performer: audio.performer.clone(),
                title: audio.title.clone(),
            },
            _ => DocumentAttribute::Other,
        })
        .collect();
    Some(Document {
        id: document.id,
        access_hash: document.access_hash,
        file_reference: document.file_reference.clone(),
        dc_id: document.dc_id,
        size: document.size,
        mime_type: document.mime_type.clone(),
        attributes,
    })
}

/// The Telethon class name of a media without a model of its own.
fn media_class(media: &tl::enums::MessageMedia) -> &'static str {
    use tl::enums::MessageMedia as M;
    match media {
        M::Geo(_) => "MessageMediaGeo",
        M::Contact(_) => "MessageMediaContact",
        M::Unsupported => "MessageMediaUnsupported",
        M::Venue(_) => "MessageMediaVenue",
        M::Game(_) => "MessageMediaGame",
        M::Invoice(_) => "MessageMediaInvoice",
        M::GeoLive(_) => "MessageMediaGeoLive",
        M::Poll(_) => "MessageMediaPoll",
        M::Dice(_) => "MessageMediaDice",
        M::Story(_) => "MessageMediaStory",
        M::Giveaway(_) => "MessageMediaGiveaway",
        M::GiveawayResults(_) => "MessageMediaGiveawayResults",
        M::PaidMedia(_) => "MessageMediaPaidMedia",
        M::ToDo(_) => "MessageMediaToDo",
        M::VideoStream(_) => "MessageMediaVideoStream",
        M::Widget(_) => "MessageMediaWidget",
        M::Empty | M::Photo(_) | M::Document(_) | M::WebPage(_) => "MessageMedia",
    }
}

/// A message's media as Telethon sets `msg.media` (`MessageMediaEmpty` is none).
pub fn media(media: &tl::enums::MessageMedia) -> Option<Media> {
    use tl::enums::MessageMedia as M;
    Some(match media {
        M::Empty => return None,
        M::Photo(media) => Media::Photo(media.photo.as_ref().and_then(photo)),
        M::Document(media) => Media::Document(media.document.as_ref().and_then(document)),
        M::WebPage(media) => Media::WebPage(match &media.webpage {
            tl::enums::WebPage::Page(page) => WebPage {
                url: page.url.clone(),
                loaded: true,
                photo: page.photo.as_ref().and_then(photo),
                document: page.document.as_ref().and_then(document),
            },
            tl::enums::WebPage::Empty(page) => WebPage {
                url: page.url.clone().unwrap_or_default(),
                ..WebPage::default()
            },
            tl::enums::WebPage::Pending(page) => WebPage {
                url: page.url.clone().unwrap_or_default(),
                ..WebPage::default()
            },
            tl::enums::WebPage::NotModified(_) => WebPage::default(),
        }),
        other => Media::Other(media_class(other).to_owned()),
    })
}

fn channel_of(peer: &tl::enums::Peer) -> Option<i64> {
    match peer {
        tl::enums::Peer::Channel(channel) => Some(channel.channel_id),
        _ => None,
    }
}

/// A `Message` or `MessageService`; `None` for `MessageEmpty`.
pub fn message(message: &tl::enums::Message) -> Option<TgMessage> {
    match message {
        tl::enums::Message::Empty(_) => None,
        tl::enums::Message::Message(message) => Some(TgMessage {
            id: i64::from(message.id),
            text: message.message.clone(),
            grouped_id: message.grouped_id,
            channel_id: channel_of(&message.peer_id),
            action: false,
            media: message.media.as_ref().and_then(media),
            date: i64::from(message.date),
        }),
        tl::enums::Message::Service(message) => Some(TgMessage {
            id: i64::from(message.id),
            text: String::new(),
            grouped_id: None,
            channel_id: channel_of(&message.peer_id),
            action: true,
            media: None,
            date: i64::from(message.date),
        }),
    }
}

fn history_item(raw: &tl::enums::Message) -> HistoryItem {
    match raw {
        tl::enums::Message::Empty(empty) => HistoryItem::Empty(i64::from(empty.id)),
        other => message(other).map_or(HistoryItem::Empty(0), HistoryItem::Message),
    }
}

/// A channel of a `chats` list.
pub fn channel(chat: &tl::enums::Chat) -> Option<Channel> {
    match chat {
        tl::enums::Chat::Channel(channel) => Some(Channel {
            id: channel.id,
            access_hash: channel.access_hash.unwrap_or(0),
            username: channel.username.clone(),
            title: Some(channel.title.clone()),
        }),
        tl::enums::Chat::ChannelForbidden(channel) => Some(Channel {
            id: channel.id,
            access_hash: channel.access_hash,
            username: None,
            title: Some(channel.title.clone()),
        }),
        _ => None,
    }
}

// ---------- the client ----------

struct FileLocation {
    location: tl::enums::InputFileLocation,
    size: Option<usize>,
    dc_id: i32,
}

impl Downloadable for FileLocation {
    fn to_raw_input_location(&self) -> Option<tl::enums::InputFileLocation> {
        Some(self.location.clone())
    }

    fn size(&self) -> Option<usize> {
        self.size
    }

    fn dc_id(&self) -> Option<i32> {
        Some(self.dc_id)
    }
}

/// One connected account.
pub struct GrammersClient {
    client: Client,
    session: Arc<TelethonSession>,
    handle: grammers_client::sender::SenderPoolFatHandle,
    retries: TelethonRetries,
}

impl std::fmt::Debug for GrammersClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrammersClient").finish_non_exhaustive()
    }
}

impl GrammersClient {
    fn invoke_error(&self, error: &InvocationError) -> TgError {
        self.retries.final_error(error)
    }

    /// `await self(request)`: `_call`'s flood-wait check, then the request with its retries.
    async fn call<R: tl::RemoteCall + tl::Identifiable>(&self, request: &R) -> Result<R::Return, TgError> {
        self.retries.before(R::CONSTRUCTOR_ID).await?;
        self.client.invoke(request).await.map_err(|error| self.invoke_error(&error))
    }

    /// Remember the channels of an answer, as Telethon's `process_entities` does.
    async fn cache_chats(&self, chats: &[tl::enums::Chat]) {
        for chat in chats {
            if let tl::enums::Chat::Channel(channel) = chat
                && !channel.min
                && let Some(hash) = channel.access_hash
            {
                let _ = self
                    .session
                    .cache_peer(PeerInfo::Channel {
                        id: channel.id,
                        auth: Some(PeerAuth::from_hash(hash)),
                        kind: None,
                    })
                    .await;
            }
        }
    }

    /// `get_input_entity(PeerChannel(id))`: the cached hash, else `channels.getChannels`
    /// with hash 0.
    async fn input_channel(&self, channel_id: i64) -> Result<tl::types::InputChannel, TgError> {
        let cached = PeerId::channel(channel_id).and_then(|peer| {
            futures_util::FutureExt::now_or_never(self.session.peer_ref(peer))
                .and_then(Result::ok)
                .flatten()
        });
        if let Some(peer) = cached {
            return Ok(tl::types::InputChannel {
                channel_id,
                access_hash: peer.auth.hash(),
            });
        }
        // A channel Telegram does not know with hash 0 is Telethon's `ValueError`; any other
        // failure is raised as it is.
        let not_found = || {
            TgError::new(
                "ValueError",
                format!(
                    "Could not find the input entity for PeerChannel(channel_id={channel_id}) (PeerChannel). Please read \
                     https://docs.telethon.dev/en/stable/concepts/entities.html to find out more details."
                ),
                ErrorKind::Other,
            )
        };
        let channels = match self.get_channels(channel_id, 0).await {
            Err(error) if error.class == "ChannelInvalidError" => return Err(not_found()),
            other => other?,
        };
        // Telethon takes the first chat of the answer, whatever its id.
        let channel = channels.into_iter().next().ok_or_else(not_found)?;
        Ok(tl::types::InputChannel {
            channel_id: channel.id,
            access_hash: channel.access_hash,
        })
    }

    async fn get_channels(&self, channel_id: i64, access_hash: i64) -> Result<Vec<Channel>, TgError> {
        let request = tl::functions::channels::GetChannels {
            id: vec![tl::enums::InputChannel::Channel(tl::types::InputChannel {
                channel_id,
                access_hash,
            })],
        };
        let chats = self.call(&request).await?.chats();
        self.cache_chats(&chats).await;
        Ok(chats.iter().filter_map(channel).collect())
    }
}

impl TgClient for GrammersClient {
    fn is_authorized(&self) -> BoxFuture<'_, Result<bool, TgError>> {
        // `is_user_authorized`: any `RPCError` from `updates.getState` means no.
        Box::pin(async move {
            match self.call(&tl::functions::updates::GetState {}).await {
                Ok(_) => Ok(true),
                Err(error) if matches!(error.kind, ErrorKind::Rpc | ErrorKind::FloodWait(_) | ErrorKind::MessageIdInvalid) => Ok(false),
                Err(error) => Err(error),
            }
        })
    }

    fn get_channel(&self, channel_id: i64) -> BoxFuture<'_, Result<Channel, TgError>> {
        Box::pin(async move {
            let input = self.input_channel(channel_id).await?;
            self.get_channels(input.channel_id, input.access_hash)
                .await?
                .into_iter()
                .find(|channel| channel.id == input.channel_id)
                // `id_entity[...]` in `get_entity`.
                .ok_or_else(|| TgError::new("KeyError", input.channel_id.to_string(), ErrorKind::Other))
        })
    }

    fn get_history<'a>(&'a self, channel: &'a Channel, request: HistoryRequest) -> BoxFuture<'a, Result<HistoryPage, TgError>> {
        Box::pin(async move {
            let clamp = |value: i64| i32::try_from(value.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).unwrap_or(0);
            let call = tl::functions::messages::GetHistory {
                peer: tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                    channel_id: channel.id,
                    access_hash: channel.access_hash,
                }),
                offset_id: clamp(request.offset_id),
                offset_date: clamp(request.offset_date),
                add_offset: clamp(request.add_offset),
                limit: clamp(request.limit),
                max_id: clamp(request.max_id),
                min_id: clamp(request.min_id),
                hash: 0,
            };
            let answer = self.call(&call).await?;
            let (messages, chats, complete) = match answer {
                tl::enums::messages::Messages::Messages(all) => (all.messages, all.chats, true),
                tl::enums::messages::Messages::Slice(slice) => (slice.messages, slice.chats, false),
                tl::enums::messages::Messages::ChannelMessages(page) => (page.messages, page.chats, false),
                tl::enums::messages::Messages::NotModified(_) => (Vec::new(), Vec::new(), false),
            };
            self.cache_chats(&chats).await;
            Ok(HistoryPage {
                messages: messages.iter().map(history_item).collect(),
                complete,
            })
        })
    }

    fn get_messages<'a>(&'a self, channel_id: i64, ids: &'a [i64]) -> BoxFuture<'a, Result<Vec<Option<TgMessage>>, TgError>> {
        Box::pin(async move {
            let input = self.input_channel(channel_id).await?;
            let call = tl::functions::channels::GetMessages {
                channel: tl::enums::InputChannel::Channel(input),
                id: ids
                    .iter()
                    .map(|id| {
                        tl::enums::InputMessage::Id(tl::types::InputMessageId {
                            id: i32::try_from(*id).unwrap_or(0),
                        })
                    })
                    .collect(),
            };
            let answer = self.call(&call).await?;
            let (messages, chats) = match answer {
                tl::enums::messages::Messages::Messages(all) => (all.messages, all.chats),
                tl::enums::messages::Messages::Slice(slice) => (slice.messages, slice.chats),
                tl::enums::messages::Messages::ChannelMessages(page) => (page.messages, page.chats),
                tl::enums::messages::Messages::NotModified(_) => (Vec::new(), Vec::new()),
            };
            self.cache_chats(&chats).await;
            // `_IDsIter`: an empty message, or one of another channel, is `None`.
            let mut out: Vec<Option<TgMessage>> = messages
                .iter()
                .map(|raw| message(raw).filter(|message| message.channel_id == Some(channel_id)))
                .collect();
            if out.is_empty() {
                out = vec![None; ids.len()];
            }
            Ok(out)
        })
    }

    fn download<'a>(&'a self, location: &'a Location, path: &'a Path, size: Option<i64>) -> BoxFuture<'a, Result<(), TgError>> {
        Box::pin(async move {
            let (location, dc_id) = match location {
                Location::Photo { photo, thumb_size } => (
                    tl::enums::InputFileLocation::InputPhotoFileLocation(tl::types::InputPhotoFileLocation {
                        id: photo.id,
                        access_hash: photo.access_hash,
                        file_reference: photo.file_reference.clone(),
                        thumb_size: thumb_size.clone(),
                    }),
                    photo.dc_id,
                ),
                Location::Document { document } => (
                    tl::enums::InputFileLocation::InputDocumentFileLocation(tl::types::InputDocumentFileLocation {
                        id: document.id,
                        access_hash: document.access_hash,
                        file_reference: document.file_reference.clone(),
                        thumb_size: String::new(),
                    }),
                    document.dc_id,
                ),
                Location::Inline(bytes) => {
                    return tokio::fs::write(path, bytes).await.map_err(|error| TgError::io(&error));
                }
            };
            let target = FileLocation {
                location,
                size: size.and_then(|size| usize::try_from(size).ok()),
                dc_id,
            };
            let mut file = tokio::fs::File::create(path).await.map_err(|error| TgError::io(&error))?;
            let mut download = self.client.iter_download(&target);
            loop {
                self.retries
                    .before(<tl::functions::upload::GetFile as tl::Identifiable>::CONSTRUCTOR_ID)
                    .await?;
                let Some(chunk) = download.next().await.map_err(|error| self.invoke_error(&error))? else {
                    break;
                };
                file.write_all(&chunk).await.map_err(|error| TgError::io(&error))?;
            }
            file.flush().await.map_err(|error| TgError::io(&error))?;
            Ok(())
        })
    }

    fn disconnect(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.handle.quit();
        })
    }
}

/// Telethon's `InitConnection` device values: `PC 64bit` on x86-64, the kernel release
/// without its suffix.
fn telethon_params(retries: TelethonRetries) -> ConnectionParams {
    let machine = std::env::consts::ARCH;
    let device_model = match machine {
        "x86_64" => "PC 64bit".to_owned(),
        "x86" => "PC 32bit".to_owned(),
        other => other.to_owned(),
    };
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let release = fav_core::pytext::py_strip(&release);
    let system_version = release.split('-').next().unwrap_or(release).to_owned();
    ConnectionParams {
        device_model,
        system_version: if system_version.is_empty() {
            "1.0".to_owned()
        } else {
            system_version
        },
        app_version: "1.43.2".to_owned(),
        system_lang_code: "en".to_owned(),
        lang_code: "en".to_owned(),
        retry_policy: Box::new(retries),
        ..ConnectionParams::default()
    }
}

/// Builds a [`GrammersClient`] per connection.
#[derive(Debug, Default, Clone, Copy)]
pub struct GrammersConnector;

impl Connector for GrammersConnector {
    fn connect(&self, config: ClientConfig) -> BoxFuture<'_, Result<Connection, TgError>> {
        Box::pin(async move {
            let session = Arc::new(
                TelethonSession::open(&config.session_path)
                    .await
                    .map_err(|error| TgError::new("OperationalError", error.to_string(), ErrorKind::Other))?,
            );
            let api_id =
                i32::try_from(config.api_id).map_err(|_| TgError::new("ValueError", "api_id is out of range", ErrorKind::Other))?;
            let threshold = Duration::from_secs(u64::try_from(config.flood_sleep_threshold).unwrap_or(0));
            let retries = TelethonRetries::new(threshold, !config.receive_updates);
            let pool = SenderPool::with_configuration(Arc::clone(&session), api_id, telethon_params(retries.clone()));
            let handle = pool.handle.clone();
            let client = Client::new(pool.handle);
            let (done_tx, done_rx) = oneshot::channel::<Result<(), TgError>>();
            let runner = tokio::spawn(pool.runner.run());
            let mut updates = pool.updates;
            let messages = if config.receive_updates {
                let stream = client
                    .stream_updates(updates, UpdatesConfiguration { catch_up: config.catch_up })
                    .await
                    .map_err(|error| TgError::connection(error.to_string()))?;
                let (tx, rx) = mpsc::channel(256);
                tokio::spawn(pump_updates(stream, tx, done_tx));
                Some(rx)
            } else {
                // Telethon asks for no updates at all; here they are read and dropped.
                tokio::spawn(async move { while updates.recv().await.is_some() {} });
                drop(done_tx);
                None
            };
            let session_on_close = Arc::clone(&session);
            let disconnected: BoxFuture<'static, Result<(), TgError>> = Box::pin(async move {
                let ended = tokio::select! {
                    outcome = done_rx => outcome.unwrap_or(Ok(())),
                    _ = runner => Ok(()),
                };
                session_on_close.close().await;
                ended
            });
            let client: Arc<dyn TgClient> = Arc::new(GrammersClient {
                client,
                session,
                handle,
                retries,
            });
            Ok(Connection {
                client,
                messages: if config.receive_updates { messages } else { None },
                disconnected: if config.receive_updates {
                    disconnected
                } else {
                    Box::pin(std::future::pending())
                },
            })
        })
    }
}

/// Forward new messages until the stream fails, saving the update state every minute and
/// at the end; the failure ends the connection (`client.disconnected`).
async fn pump_updates(
    mut stream: grammers_client::client::UpdateStream,
    tx: mpsc::Sender<TgMessage>,
    done: oneshot::Sender<Result<(), TgError>>,
) {
    let mut save = tokio::time::interval(STATE_SAVE_INTERVAL);
    save.tick().await;
    let outcome = loop {
        tokio::select! {
            next = stream.next_raw() => match next {
                Ok((update, _state, _peers)) => {
                    let raw = match &update {
                        tl::enums::Update::NewChannelMessage(update) => Some(&update.message),
                        tl::enums::Update::NewMessage(update) => Some(&update.message),
                        _ => None,
                    };
                    if let Some(message) = raw.and_then(message)
                        && tx.send(message).await.is_err()
                    {
                        break Ok(());
                    }
                }
                Err(error) => break Err(invocation_error(&error)),
            },
            _ = save.tick() => {
                let _ = stream.sync_update_state().await;
            }
        }
    };
    let _ = stream.sync_update_state().await;
    let _ = done.send(outcome);
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::ops::ControlFlow;
    use std::time::Duration;

    use grammers_client::InvocationError;
    use grammers_client::sender::{RetryContext, RetryPolicy, RpcError};

    use super::super::client::ErrorKind;
    use super::{TelethonRetries, request_name};

    fn rpc(code: i32, name: &str, value: Option<u32>, fail_count: u32) -> RetryContext {
        RetryContext {
            fail_count: NonZeroU32::new(fail_count).expect("non-zero"),
            slept_so_far: Duration::ZERO,
            error: InvocationError::Rpc(RpcError {
                code,
                name: name.to_owned(),
                value,
                caused_by: None,
            }),
        }
    }

    #[test]
    fn retries_follow_telethons_call() {
        let policy = TelethonRetries::new(Duration::from_mins(5), false);
        assert_eq!(
            policy.should_retry(&rpc(420, "FLOOD_WAIT", Some(30), 1)),
            ControlFlow::Continue(Duration::from_secs(30))
        );
        assert_eq!(
            policy.should_retry(&rpc(420, "FLOOD_WAIT", Some(0), 1)),
            ControlFlow::Continue(Duration::from_secs(1))
        );
        assert_eq!(policy.should_retry(&rpc(420, "FLOOD_WAIT", Some(301), 1)), ControlFlow::Break(()));
        assert_eq!(
            policy.should_retry(&rpc(420, "FLOOD_PREMIUM_WAIT", Some(3), 1)),
            ControlFlow::Continue(Duration::from_secs(3))
        );
        assert_eq!(
            policy.should_retry(&rpc(500, "WORKER_BUSY_TOO_LONG_RETRY", None, 1)),
            ControlFlow::Continue(Duration::from_secs(2))
        );
        assert_eq!(
            policy.should_retry(&rpc(400, "RPC_CALL_FAIL", None, 5)),
            ControlFlow::Continue(Duration::from_secs(2))
        );
        // Six attempts in all.
        assert_eq!(
            policy.should_retry(&rpc(500, "INTERDC_CALL_ERROR", Some(2), 6)),
            ControlFlow::Break(())
        );
        assert_eq!(policy.should_retry(&rpc(400, "CHANNEL_PRIVATE", None, 1)), ControlFlow::Break(()));
    }

    #[test]
    fn exhausted_retries_raise_telethons_value_error() {
        let policy = TelethonRetries::new(Duration::from_mins(1), false);
        let exhausted = policy.final_error(&rpc(500, "WORKER_BUSY_TOO_LONG_RETRY", None, 6).error);
        assert_eq!(exhausted.class, "ValueError");
        assert_eq!(exhausted.message, "Request was unsuccessful 6 time(s)");
        assert_eq!(exhausted.kind, ErrorKind::Other);
        let flood = policy.final_error(&rpc(420, "FLOOD_WAIT", Some(30), 6).error);
        assert_eq!(flood.class, "ValueError");
        // Waits over the threshold and errors `_call` never retries are raised as they are.
        let long = policy.final_error(&rpc(420, "FLOOD_WAIT", Some(61), 1).error);
        assert_eq!(long.class, "FloodWaitError");
        assert_eq!(long.kind, ErrorKind::FloodWait(61));
        let private = policy.final_error(&rpc(400, "CHANNEL_PRIVATE", None, 1).error);
        assert_eq!(private.class, "ChannelPrivateError");
    }

    fn caused(code: i32, name: &str, value: Option<u32>, constructor: u32) -> RetryContext {
        let mut ctx = rpc(code, name, value, 1);
        if let InvocationError::Rpc(error) = &mut ctx.error {
            error.caused_by = Some(constructor);
        }
        ctx
    }

    #[tokio::test]
    async fn a_request_in_a_flood_wait_is_not_sent_again() {
        use grammers_client::tl::Identifiable as _;
        let history = grammers_client::tl::functions::messages::GetHistory::CONSTRUCTOR_ID;
        let channels = grammers_client::tl::functions::channels::GetChannels::CONSTRUCTOR_ID;
        let policy = TelethonRetries::new(Duration::from_mins(1), false);
        assert_eq!(
            policy.should_retry(&caused(420, "FLOOD_WAIT", Some(300), history)),
            ControlFlow::Break(())
        );
        let early = policy.before(history).await.expect_err("still waiting");
        assert_eq!(early.class, "FloodWaitError");
        assert!(matches!(early.kind, ErrorKind::FloodWait(299 | 300)), "{early:?}");
        assert!(
            early.message.ends_with(" seconds is required (caused by GetHistoryRequest)"),
            "{}",
            early.message
        );
        // Other requests are sent; slow mode and short waits are not remembered.
        policy.before(channels).await.expect("not waiting");
        assert_eq!(
            policy.should_retry(&caused(420, "SLOWMODE_WAIT", Some(300), channels)),
            ControlFlow::Break(())
        );
        policy.before(channels).await.expect("slow mode is per chat");
        let _ = policy.should_retry(&caused(420, "FLOOD_WAIT", Some(2), channels));
        policy.before(channels).await.expect("3 seconds or less is forgotten");
        // With no updates Telethon keys the wait by the wrapping request, never looked up.
        let quiet = TelethonRetries::new(Duration::from_mins(1), true);
        let _ = quiet.should_retry(&caused(420, "FLOOD_WAIT", Some(300), history));
        quiet.before(history).await.expect("not remembered");
        let wrapped = quiet.final_error(&caused(403, "CHAT_WRITE_FORBIDDEN", None, history).error);
        assert_eq!(
            wrapped.message,
            "You can't write in this chat (caused by InvokeWithoutUpdatesRequest(GetHistoryRequest))"
        );
    }

    #[test]
    fn requests_are_named_as_telethon_names_them() {
        use grammers_client::tl::Identifiable as _;
        assert_eq!(
            request_name(grammers_client::tl::functions::channels::GetChannels::CONSTRUCTOR_ID),
            "GetChannelsRequest"
        );
        assert_eq!(
            request_name(grammers_client::tl::functions::messages::GetHistory::CONSTRUCTOR_ID),
            "GetHistoryRequest"
        );
    }
}
