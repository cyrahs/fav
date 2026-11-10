//! `client.iter_messages(channel, ...)` and `client.get_messages(channel, ids=...)` as
//! Telethon 1.43 pages them (`_MessagesIter`, `_IDsIter`, `RequestIter`).
//!
//! Telethon never sends `min_id` or `max_id`: going forward (`reverse=True`) it starts at
//! `offset_id = min_id + 1` with a negative `add_offset`; going back from the newest it
//! stops at the first id at or below `min_id`. Pages are at most 100 messages, and with a
//! `wait_time` it sleeps between pages so that loads are at least that far apart.

use std::time::Duration;

use tokio::time::Instant;

use super::client::{Channel, HistoryItem, HistoryRequest, TgClient, TgError};
use super::message::TgMessage;

/// `_MAX_CHUNK_SIZE`.
const MAX_CHUNK_SIZE: i64 = 100;

/// The arguments `src/web/telegram.py` passes to `iter_messages`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scan {
    pub min_id: i64,
    /// `None` is no limit.
    pub limit: Option<i64>,
    /// Oldest first (`reverse=True`).
    pub reverse: bool,
    /// `None` lets Telethon choose: 1 s beyond 3000 messages, else no wait.
    pub wait_time: Option<f64>,
}

/// Whether a message ends the iteration (`_message_in_range`).
fn in_range(id: i64, reverse: bool, last_id: Option<i64>, min_id: i64) -> bool {
    if reverse {
        // `max_id` is infinite here: nothing is above it.
        last_id.is_none_or(|last| id > last)
    } else {
        last_id.is_none_or(|last| id < last) && id > min_id
    }
}

/// Every message `iter_messages(channel, limit=..., min_id=..., reverse=..., wait_time=...)`
/// yields, in its order.
pub async fn iter_messages(client: &dyn TgClient, channel: &Channel, scan: Scan) -> Result<Vec<TgMessage>, TgError> {
    // `self.limit = max(inf if limit is None else limit, 0)`; `None` is infinite.
    let limit = scan.limit.map(|limit| limit.max(0));
    let mut request = HistoryRequest {
        limit: 1,
        ..HistoryRequest::default()
    };
    if scan.reverse {
        let offset_id = scan.min_id.max(0);
        request.offset_id = if offset_id != 0 { offset_id + 1 } else { 1 };
    }
    if limit == Some(0) {
        // Telethon still asks once, for the total count, and yields nothing.
        client.get_history(channel, request).await?;
        return Ok(Vec::new());
    }
    let wait_time = scan
        .wait_time
        .unwrap_or(if limit.is_none_or(|limit| limit > 3000) { 1.0 } else { 0.0 });
    if scan.reverse {
        request.add_offset -= MAX_CHUNK_SIZE;
    }
    let mut left = limit;
    // `last_id` starts at 0 going forward and at infinity going back.
    let mut last_id: Option<i64> = if scan.reverse { Some(0) } else { None };
    let mut last_load: Option<Instant> = None;
    let mut out = Vec::new();
    loop {
        if left.is_some_and(|left| left <= 0) {
            break;
        }
        if wait_time > 0.0 {
            if let Some(last) = last_load {
                let elapsed = last.elapsed().as_secs_f64();
                if wait_time > elapsed {
                    tokio::time::sleep(Duration::from_secs_f64(wait_time - elapsed)).await;
                }
            }
            last_load = Some(Instant::now());
        }
        request.limit = left.map_or(MAX_CHUNK_SIZE, |left| left.min(MAX_CHUNK_SIZE));
        if scan.reverse && request.limit != MAX_CHUNK_SIZE {
            request.add_offset = -request.limit;
        }
        let page = client.get_history(channel, request).await?;
        let mut buffer: Vec<TgMessage> = Vec::new();
        let mut done = false;
        let ordered: Box<dyn Iterator<Item = &HistoryItem>> = if scan.reverse {
            Box::new(page.messages.iter().rev())
        } else {
            Box::new(page.messages.iter())
        };
        for item in ordered {
            let HistoryItem::Message(message) = item else { continue };
            if !in_range(message.id, scan.reverse, last_id, scan.min_id) {
                done = true;
                break;
            }
            last_id = Some(message.id);
            buffer.push(message.clone());
        }
        if !done {
            let first_id = page.messages.first().map(HistoryItem::id);
            if page.complete || first_id.is_none_or(|id| !scan.reverse && id <= request.limit) {
                done = true;
            } else if let Some(last) = buffer.last() {
                request.offset_id = last.id + i64::from(scan.reverse);
                request.offset_date = last.date;
            } else {
                done = true;
            }
        }
        if buffer.is_empty() {
            break;
        }
        let take = if done {
            buffer.len()
        } else {
            left.map_or(buffer.len(), |left| usize::try_from(left).unwrap_or(usize::MAX).min(buffer.len()))
        };
        if let Some(left) = left.as_mut() {
            *left -= i64::try_from(take).unwrap_or(i64::MAX);
        }
        out.extend(buffer.into_iter().take(take));
        if done {
            break;
        }
    }
    Ok(out)
}

/// `get_messages(channel, ids=[...])`: one entry per id, `None` for one that is gone,
/// fetched 100 ids at a time.
pub async fn get_messages(client: &dyn TgClient, channel_id: i64, ids: &[i64]) -> Result<Vec<Option<TgMessage>>, TgError> {
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(usize::try_from(MAX_CHUNK_SIZE).unwrap_or(100)) {
        out.extend(client.get_messages(channel_id, chunk).await?);
    }
    Ok(out)
}
