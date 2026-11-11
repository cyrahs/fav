//! `events.NewMessage()` and `events.Album()` over the new messages a connection receives.
//!
//! Python origin: Telethon 1.43's `events/album.py`. The parts of an album arrive as
//! separate messages; Telethon keeps one `AlbumHack` per `grouped_id` and per client, and
//! dispatches the album 0.5 s after its last part arrived. An album of a single message
//! is never dispatched (`Album.filter` wants two or more), and the hack is never dropped,
//! so a part that arrives after its album was dispatched is lost to the event (the
//! channel's reconciliation still finds it). Service messages are neither event.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

use super::message::TgMessage;

/// `_HACK_DELAY`.
pub const HACK_DELAY: Duration = Duration::from_millis(500);

/// What a handler receives.
#[allow(clippy::large_enum_variant)] // reason: a handful live at a time; boxing buys nothing
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    NewMessage(TgMessage),
    Album(Vec<TgMessage>),
}

#[derive(Debug)]
struct Pending {
    messages: Vec<TgMessage>,
    due: Instant,
}

/// The albums one connection is assembling.
#[derive(Debug, Default)]
pub struct Albums {
    pending: HashMap<i64, Pending>,
    /// Albums already dispatched; their late parts go nowhere, as in Telethon.
    dispatched: HashSet<i64>,
}

impl Albums {
    /// A new message: the `NewMessage` event it is, if any. A grouped message joins its
    /// album instead.
    ///
    /// `src/web/telegram.py`'s `NewMessage` handler ignores a message with a (truthy)
    /// `grouped_id`, and `Album` takes any message whose `grouped_id` is not `None`.
    pub fn push(&mut self, message: TgMessage, now: Instant) -> Option<Event> {
        if message.action {
            return None;
        }
        let single = message.album().is_none().then(|| Event::NewMessage(message.clone()));
        if let Some(group) = message.grouped_id {
            if self.dispatched.contains(&group) {
                return single;
            }
            let pending = self.pending.entry(group).or_insert_with(|| Pending {
                messages: Vec::new(),
                due: now,
            });
            pending.messages.push(message);
            pending.due = now + HACK_DELAY;
        }
        single
    }

    /// When the next album is due.
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.values().map(|pending| pending.due).min()
    }

    /// The albums due at `now`, in the order they became due. One of a single message is
    /// dropped (`Album.filter`).
    pub fn take_due(&mut self, now: Instant) -> Vec<Event> {
        let mut due: Vec<(Instant, i64)> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.due <= now)
            .map(|(group, pending)| (pending.due, *group))
            .collect();
        due.sort_unstable();
        let mut events = Vec::new();
        for (_, group) in due {
            if let Some(pending) = self.pending.remove(&group) {
                self.dispatched.insert(group);
                if pending.messages.len() > 1 {
                    events.push(Event::Album(pending.messages));
                }
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::{Albums, Event, HACK_DELAY, Instant};
    use crate::telegram::message::TgMessage;

    fn grouped(id: i64, group: i64) -> TgMessage {
        TgMessage {
            grouped_id: Some(group),
            ..TgMessage::new(id, 123)
        }
    }

    #[test]
    fn albums_are_dispatched_half_a_second_after_their_last_part() {
        let start = Instant::now();
        let mut albums = Albums::default();
        assert_eq!(
            albums.push(TgMessage::new(1, 123), start),
            Some(Event::NewMessage(TgMessage::new(1, 123)))
        );
        assert_eq!(albums.push(grouped(2, 99), start), None);
        assert_eq!(albums.push(grouped(3, 99), start + HACK_DELAY / 2), None);
        assert_eq!(albums.next_due(), Some(start + HACK_DELAY / 2 + HACK_DELAY));
        assert!(albums.take_due(start + HACK_DELAY).is_empty());
        let events = albums.take_due(start + HACK_DELAY * 2);
        assert_eq!(events, vec![Event::Album(vec![grouped(2, 99), grouped(3, 99)])]);
        // A part after its album went out is lost to the event, as in Telethon.
        assert_eq!(albums.push(grouped(4, 99), start + HACK_DELAY * 3), None);
        assert!(albums.take_due(start + HACK_DELAY * 5).is_empty());
        // An album of one message is never an event; a service message is neither.
        albums.push(grouped(5, 7), start);
        assert!(albums.take_due(start + HACK_DELAY).is_empty());
        let service = TgMessage {
            action: true,
            ..TgMessage::new(6, 123)
        };
        assert_eq!(albums.push(service, start), None);
        assert_eq!(albums.next_due(), None);
    }
}
