//! grammers covering what `src/web/telegram.py` does with Telethon.

use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use grammers_client::media::Media;
use grammers_client::message::Message;
use grammers_client::sender::{ConnectionParams, RetryContext, RetryPolicy, UpdatesConfiguration};
use grammers_client::session::Session;
use grammers_client::session::SessionData;
use grammers_client::session::storages::MemorySession;
use grammers_client::session::types::{DcOption, PeerId, PeerRef};
use grammers_client::update::Update;
use grammers_client::{Client, InvocationError, SenderPool, tl};

/// The values a Telethon `.session` SQLite file stores in its `sessions` row.
#[derive(Debug, Clone)]
pub struct TelethonSessionRow {
    pub dc_id: i32,
    pub server_address: std::net::Ipv4Addr,
    pub port: u16,
    pub auth_key: [u8; 256],
}

/// Telethon's `flood_sleep_threshold`: sleep through a `FLOOD_WAIT` up to the
/// threshold, surface anything longer to the caller.
#[derive(Debug)]
pub struct FloodSleepThreshold {
    pub threshold: Duration,
}

impl RetryPolicy for FloodSleepThreshold {
    fn should_retry(&self, ctx: &RetryContext) -> ControlFlow<(), Duration> {
        match &ctx.error {
            InvocationError::Rpc(rpc) if rpc.name == "FLOOD_WAIT" && ctx.fail_count == NonZeroU32::MIN => {
                let wait = Duration::from_secs(u64::from(rpc.value.unwrap_or(0)));
                if wait <= self.threshold {
                    ControlFlow::Continue(wait)
                } else {
                    ControlFlow::Break(())
                }
            }
            _ => ControlFlow::Break(()),
        }
    }
}

/// Seconds to cool down for, as Telethon's `FloodWaitError.seconds`.
pub fn flood_wait_seconds(error: &InvocationError) -> Option<u32> {
    match error {
        InvocationError::Rpc(rpc) if rpc.name == "FLOOD_WAIT" => rpc.value,
        _ => None,
    }
}

/// Import an existing Telethon login so the account is not re-authorised.
pub fn session_from_telethon(row: &TelethonSessionRow) -> SessionData {
    let mut data = SessionData {
        home_dc: row.dc_id,
        ..SessionData::default()
    };
    let known = data.dc_options.get(&row.dc_id).cloned();
    let option = DcOption {
        id: row.dc_id,
        ipv4: std::net::SocketAddrV4::new(row.server_address, row.port),
        ipv6: known.map_or_else(
            || std::net::SocketAddrV6::new(row.server_address.to_ipv6_mapped(), row.port, 0, 0),
            |known| known.ipv6,
        ),
        auth_key: Some(row.auth_key),
    };
    data.dc_options.insert(row.dc_id, option);
    data
}

/// `TelegramClient(session, api_id, api_hash, flood_sleep_threshold=..., receive_updates=True, catch_up=True)`
/// followed by `connect()` and `is_user_authorized()`.
pub async fn connect(session: Arc<MemorySession>, api_id: i32, flood_sleep_threshold: Duration) -> Result<(Client, bool), InvocationError> {
    let params = ConnectionParams {
        retry_policy: Box::new(FloodSleepThreshold {
            threshold: flood_sleep_threshold,
        }),
        ..Default::default()
    };
    let pool = SenderPool::with_configuration(Arc::clone(&session), api_id, params);
    let client = Client::new(pool.handle);
    // The runner drives all I/O; when it returns the connection is gone, which
    // is what Telethon's `client.disconnected` future reports.
    tokio::spawn(pool.runner.run());
    let authorized = client.is_authorized().await?;
    let _updates = client.stream_updates(pool.updates, UpdatesConfiguration { catch_up: true }).await;
    Ok((client, authorized))
}

/// `iter_messages(channel, limit=..., min_id=..., reverse=True)`. Telethon does not send
/// `min_id`: in reverse it pages upward from `offset_id = min_id + 1` with a negative
/// `add_offset` and drops anything `<= min_id` on the client. grammers' reverse iterator
/// does the same from `offset_id(min_id)` (it sets the request's `min_id` to the offset,
/// which is exclusive), so every message newer than `min_id` arrives, oldest first, page
/// after page, however large the backlog.
pub async fn history_after(client: &Client, channel: PeerRef, min_id: i32, limit: Option<usize>) -> Result<Vec<Message>, InvocationError> {
    let mut iter = client.iter_messages(channel).reverse(true).offset_id(min_id);
    if let Some(limit) = limit {
        iter = iter.limit(limit);
    }
    let mut out = Vec::new();
    while let Some(message) = iter.next().await? {
        out.push(message);
    }
    Ok(out)
}

/// The raw call stays available for anything the iterator does not expose.
pub async fn raw_history(
    client: &Client,
    channel: &PeerRef,
    offset_id: i32,
    add_offset: i32,
    limit: i32,
) -> Result<tl::enums::messages::Messages, InvocationError> {
    client
        .invoke(&tl::functions::messages::GetHistory {
            peer: channel.into(),
            offset_id,
            offset_date: 0,
            add_offset,
            limit,
            max_id: 0,
            min_id: 0,
            hash: 0,
        })
        .await
}

/// `iter_messages(channel, limit=..., reverse=True)` through the high-level iterator.
pub async fn newest_window(client: &Client, channel: PeerRef, limit: usize) -> Result<Vec<Message>, InvocationError> {
    let mut iter = client.iter_messages(channel).limit(limit);
    let mut out = Vec::new();
    while let Some(message) = iter.next().await? {
        out.push(message);
    }
    Ok(out)
}

/// `get_entity(PeerChannel(id))` and `get_messages(channel, ids=[...])`.
pub async fn messages_by_id(
    client: &Client,
    session: &MemorySession,
    channel_id: i64,
    ids: &[i32],
) -> Result<Vec<Option<Message>>, InvocationError> {
    let peer = PeerId::channel(channel_id).ok_or_else(|| InvocationError::Io(std::io::Error::other("bad channel id")))?;
    let peer_ref = session.peer_ref(peer).await.ok().flatten().unwrap_or_else(|| peer.to_ambient_ref());
    let _resolved = client.resolve_peer(peer_ref).await?;
    client.get_messages_by_id(peer_ref, ids).await
}

/// What `_media_type`/`_is_video`/`_is_sticker` read: photo vs document, mime type,
/// `DocumentAttributeVideo`, sticker and custom-emoji attributes, and `grouped_id`.
pub fn classify(message: &Message) -> (Option<i64>, &'static str) {
    let kind = match message.media() {
        Some(Media::Photo(_)) => "photo",
        Some(Media::Sticker(_)) => "sticker",
        Some(Media::Document(document)) => {
            let raw = &document.raw;
            let video = matches!(&raw.document, Some(tl::enums::Document::Document(doc))
                if doc.attributes.iter().any(|attr| matches!(attr, tl::enums::DocumentAttribute::Video(_))));
            let custom_emoji = matches!(&raw.document, Some(tl::enums::Document::Document(doc))
                if doc.attributes.iter().any(|attr| matches!(attr, tl::enums::DocumentAttribute::CustomEmoji(_))));
            if video {
                "video"
            } else if custom_emoji {
                "sticker"
            } else if document.mime_type().is_some_and(|mime| mime.starts_with("image/")) {
                "image"
            } else {
                "document"
            }
        }
        _ => "other",
    };
    (message.grouped_id(), kind)
}

/// `msg.download_media(file=..., progress_callback=cb)`: chunked download with progress.
pub async fn download_with_progress(
    client: &Client,
    media: &Media,
    dst: &Path,
    mut progress: impl FnMut(u64, Option<usize>),
) -> Result<(), InvocationError> {
    use tokio::io::AsyncWriteExt;
    let total = match media {
        Media::Photo(photo) => photo.size(),
        Media::Document(document) => document.size(),
        _ => None,
    };
    let mut file = tokio::fs::File::create(dst).await?;
    let mut download = client.iter_download(media);
    let mut done = 0_u64;
    while let Some(chunk) = download.next().await? {
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        progress(done, total);
    }
    file.flush().await?;
    Ok(())
}

/// The realtime listener: `events.NewMessage()` and `events.Album()`. grammers
/// has no album event, so albums are grouped by `grouped_id` in our code.
pub fn new_message(update: &Update) -> Option<&Message> {
    match update {
        Update::NewMessage(message) => Some(message),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telethon_login_imports_into_a_grammers_session() {
        let row = TelethonSessionRow {
            dc_id: 4,
            server_address: std::net::Ipv4Addr::new(149, 154, 167, 91),
            port: 443,
            auth_key: [7; 256],
        };
        let session = MemorySession::from(session_from_telethon(&row));
        assert_eq!(session.home_dc_id().unwrap(), 4);
        let option = session.dc_option(4).unwrap().unwrap();
        assert_eq!(option.auth_key, Some([7; 256]));
        assert_eq!(option.ipv4.port(), 443);
    }

    #[test]
    fn flood_wait_is_slept_only_under_the_threshold() {
        let policy = FloodSleepThreshold {
            threshold: Duration::from_mins(1),
        };
        let ctx = |seconds| RetryContext {
            fail_count: NonZeroU32::MIN,
            slept_so_far: Duration::ZERO,
            error: InvocationError::Rpc(grammers_client::sender::RpcError {
                code: 420,
                name: "FLOOD_WAIT".into(),
                value: Some(seconds),
                caused_by: None,
            }),
        };
        assert_eq!(policy.should_retry(&ctx(30)), ControlFlow::Continue(Duration::from_secs(30)));
        assert_eq!(policy.should_retry(&ctx(61)), ControlFlow::Break(()));
        assert_eq!(flood_wait_seconds(&ctx(61).error), Some(61));
    }
}
