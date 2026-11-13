//! What a source does with a queued item it could not save, the same in Telegram and in
//! WeChat: how many tries the item gets, how long it waits when the service that looks it
//! up is down, and the one notification such an outage has.
//!
//! No Python origin: there each source gave an item up after five tries, whatever had
//! failed (DIVERGENCES.md D185).
//!
//! The sources keep their own queues and their own pace: Telegram tries a link again after
//! 30 s, doubling up to half an hour, and WeChat works its queue once per run. What they
//! share is [`next`], which says what becomes of the item after a failed try, and
//! [`Outages`], through which an outage is reported once, by whichever source runs into it
//! first, and closed when the last item of a source that waited on it is saved or given up.

use fav_core::logging::Logger;
use fav_runtime::notifications::{NewNotification, Resolution};

use crate::notifier::Notifier;

/// The tries an item that cannot be saved gets.
pub const MAX_ATTEMPTS: i64 = 5;

/// A service an item is looked up through, whose outages the item waits out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// The Zhihu browser: answers, articles and pins linked from Telegram or WeChat (D187).
    Zhihu,
    /// TikHub's 视频号 API: the videos forwarded in WeChat.
    TikHubChannels,
}

impl Service {
    /// The dedupe key of the one notification its outage has, whichever source reports it.
    /// The services fail apart from each other, so each has a notification of its own.
    pub fn outage_key(self) -> &'static str {
        match self {
            Self::Zhihu => "zhihu:outage",
            Self::TikHubChannels => "tikhub:outage:channels",
        }
    }

    /// What it looks up, as the notifications call one of them.
    fn item(self) -> &'static str {
        match self {
            Self::Zhihu => "link",
            Self::TikHubChannels => "video",
        }
    }
}

/// What becomes of an item after a failed try.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// It waits for a setting only the user can fix (a login, an API key), and is not
    /// given up while it does.
    WaitForSetting,
    /// It is tried again.
    Retry,
    /// Past the tries any item gets, it waits for the service to come back.
    WaitOut(Service),
    /// It is given up and reported.
    GiveUp,
}

/// What becomes of an item whose try just failed. `attempts` counts the tries made, this
/// one included; `outage` is the service that failed, when it is the service and not the
/// item; `outage_attempts` is how many tries the source's pace fits into about a day.
pub fn next(waits_for_setting: bool, outage: Option<Service>, attempts: i64, outage_attempts: i64) -> Next {
    if waits_for_setting {
        return Next::WaitForSetting;
    }
    if attempts < MAX_ATTEMPTS {
        return Next::Retry;
    }
    match outage {
        // The item may be fine: it is not given up with the tries an unreadable one gets.
        Some(service) if attempts < outage_attempts => Next::WaitOut(service),
        _ => Next::GiveUp,
    }
}

/// Whether an item waited on an outage before the try that `attempts` counts as its last:
/// only [`Next::WaitOut`] lets an item past the tries any item gets.
pub fn waited(attempts: i64) -> bool {
    attempts > MAX_ATTEMPTS
}

/// How an item that waited on an outage was done with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Saved,
    GivenUp,
}

/// The outage notifications of one source: its name on them, and where its warnings go.
pub struct Outages<'a> {
    pub notifier: &'a dyn Notifier,
    /// The source's job key, which its notification toggle goes by.
    pub source: &'a str,
    /// The source's name in the message header.
    pub header: &'a str,
    pub log: Logger,
}

impl std::fmt::Debug for Outages<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outages")
            .field("source", &self.source)
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

impl Outages<'_> {
    /// Whether the outage has its notification already. One that cannot be asked about
    /// counts as unreported.
    async fn reported(&self, service: Service, item: &str) -> bool {
        match self.notifier.is_open(service.outage_key().to_owned()).await {
            Ok(open) => open,
            Err(error) => {
                self.log
                    .warning(format_args!("Failed to look up the outage notification for {item}: {error}"));
                false
            }
        }
    }

    /// Report the outage `item` waits on, unless it is reported already: every enqueue is a
    /// message of its own, and an outage stops every item of its kind at once. Not tied to
    /// the item's fifth try either, which a restart or a cooldown can skip.
    pub async fn report(&self, service: Service, item: &str, title: &str, error: &str) {
        if self.reported(service, item).await {
            return;
        }
        let notification = NewNotification {
            kind: "download_failed".to_owned(),
            source: self.source.to_owned(),
            header: self.header.to_owned(),
            title: title.to_owned(),
            body: format!(
                "Waiting out an outage: {}s it stops are tried again for a day. {error}",
                service.item()
            ),
            dedupe_key: service.outage_key().to_owned(),
            ..NewNotification::default()
        };
        if let Err(error) = self.notifier.enqueue(notification).await {
            self.log
                .warning(format_args!("Failed to enqueue the outage notification for {item}: {error}"));
        }
    }

    /// An item that waited on the outage is done with: close the outage notification, if
    /// one is open, unless `waiting` more items of the source still wait on it. The next
    /// outage then starts a notification of its own.
    ///
    /// Closed by the last waiting item and not by any save: with a service that fails for
    /// some items only, or on and off, each save in between would close the notification
    /// and the next failed try reopen it, a pair of messages every time. And closed by an
    /// item given up as well, or a notification nothing waits on any more would stand for
    /// good and keep the next outage from being reported.
    pub async fn settle(&self, service: Service, item: &str, title: &str, end: End, waiting: i64) {
        if waiting > 0 || !self.reported(service, item).await {
            return;
        }
        let (kind, body) = match end {
            End::Saved => (
                "job_recovered",
                format!(
                    "The outage is over: this {} was saved, and none is waiting any more.",
                    service.item()
                ),
            ),
            End::GivenUp => (
                "download_failed",
                format!("No {} waits on the outage any more: the last was given up.", service.item()),
            ),
        };
        let resolution = Resolution {
            kind: kind.to_owned(),
            source: self.source.to_owned(),
            header: self.header.to_owned(),
            title: title.to_owned(),
            body,
            ..Resolution::default()
        };
        if let Err(error) = self.notifier.resolve(service.outage_key().to_owned(), resolution).await {
            self.log
                .warning(format_args!("Failed to resolve the outage notification for {item}: {error}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::testkit::RecordingNotifier;

    const DAY: i64 = 48;

    #[test]
    fn an_item_is_retried_through_its_fifth_try_and_then_given_up() {
        for attempts in 1..MAX_ATTEMPTS {
            assert_eq!(next(false, None, attempts, DAY), Next::Retry, "try {attempts}");
        }
        assert_eq!(next(false, None, MAX_ATTEMPTS, DAY), Next::GiveUp);
        assert_eq!(next(false, None, DAY + 9, DAY), Next::GiveUp);
    }

    #[test]
    fn an_outage_is_waited_out_from_the_fifth_try_until_the_day_is_over() {
        let outage = Some(Service::Zhihu);
        // The first tries are any item's.
        assert_eq!(next(false, outage, MAX_ATTEMPTS - 1, DAY), Next::Retry);
        assert_eq!(next(false, outage, MAX_ATTEMPTS, DAY), Next::WaitOut(Service::Zhihu));
        assert_eq!(next(false, outage, DAY - 1, DAY), Next::WaitOut(Service::Zhihu));
        assert_eq!(next(false, outage, DAY, DAY), Next::GiveUp);
        // Past its fifth try an item is given up by the first failure that is its own.
        assert_eq!(next(false, None, MAX_ATTEMPTS + 3, DAY), Next::GiveUp);
    }

    #[test]
    fn a_missing_setting_is_waited_for_whatever_else_is_true() {
        assert_eq!(next(true, None, 1, DAY), Next::WaitForSetting);
        assert_eq!(next(true, Some(Service::TikHubChannels), DAY + 1, DAY), Next::WaitForSetting);
    }

    fn outages<'a>(notifier: &'a RecordingNotifier, source: &'a str, header: &'a str) -> Outages<'a> {
        Outages {
            notifier,
            source,
            header,
            log: fav_core::logging::get(source),
        }
    }

    #[tokio::test]
    async fn an_outage_is_reported_once_across_sources_and_closed_by_either() {
        let notifier = Arc::new(RecordingNotifier::default());
        let telegram = outages(&notifier, "telegram", "Telegram");
        let wechat = outages(&notifier, "wechat", "WeChat");

        telegram.report(Service::Zhihu, "message 1", "A", "Zhihu is failing").await;
        wechat.report(Service::Zhihu, "article 2", "B", "Zhihu is failing").await;
        telegram.report(Service::Zhihu, "message 3", "C", "Zhihu is failing").await;

        let sent = notifier.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            (
                sent[0].kind.as_str(),
                sent[0].source.as_str(),
                sent[0].header.as_str(),
                sent[0].title.as_str()
            ),
            ("download_failed", "telegram", "Telegram", "A")
        );
        assert_eq!(sent[0].dedupe_key, "zhihu:outage");
        assert_eq!(
            sent[0].body,
            "Waiting out an outage: links it stops are tried again for a day. Zhihu is failing"
        );

        wechat.settle(Service::Zhihu, "article 2", "B", End::Saved, 0).await;
        wechat.settle(Service::Zhihu, "article 4", "D", End::Saved, 0).await;

        let resolved = notifier.resolved();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "zhihu:outage");
        assert_eq!(
            (
                resolved[0].1.kind.as_str(),
                resolved[0].1.header.as_str(),
                resolved[0].1.title.as_str()
            ),
            ("job_recovered", "WeChat", "B")
        );
        assert_eq!(
            resolved[0].1.body,
            "The outage is over: this link was saved, and none is waiting any more."
        );

        // The next outage starts a notification of its own.
        wechat.report(Service::Zhihu, "article 5", "E", "Zhihu is failing").await;
        assert_eq!(notifier.sent().len(), 2);
    }

    #[tokio::test]
    async fn each_service_has_an_outage_of_its_own() {
        let notifier = Arc::new(RecordingNotifier::default());
        let wechat = outages(&notifier, "wechat", "WeChat");

        wechat.report(Service::Zhihu, "article 1", "A", "down").await;
        wechat.report(Service::TikHubChannels, "channel_video 2", "B", "down").await;
        // A video saved says nothing about the Zhihu API.
        wechat.settle(Service::TikHubChannels, "channel_video 3", "C", End::Saved, 0).await;

        let sent = notifier.sent();
        assert_eq!(
            sent.iter().map(|sent| sent.dedupe_key.as_str()).collect::<Vec<_>>(),
            ["zhihu:outage", "tikhub:outage:channels"]
        );
        assert_eq!(
            sent[1].body,
            "Waiting out an outage: videos it stops are tried again for a day. down"
        );
        let resolved = notifier.resolved();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "tikhub:outage:channels");
        assert_eq!(
            resolved[0].1.body,
            "The outage is over: this video was saved, and none is waiting any more."
        );
    }

    #[tokio::test]
    async fn an_outage_stays_open_while_an_item_still_waits_on_it() {
        let notifier = Arc::new(RecordingNotifier::default());
        let telegram = outages(&notifier, "telegram", "Telegram");
        telegram.report(Service::Zhihu, "message 1", "A", "down").await;

        // A service that fails on and off: a save in between closes nothing, so the next
        // failed try has nothing to report again.
        telegram.settle(Service::Zhihu, "message 2", "B", End::Saved, 2).await;
        telegram.report(Service::Zhihu, "message 1", "A", "down").await;
        telegram.settle(Service::Zhihu, "message 3", "C", End::GivenUp, 1).await;
        assert!(notifier.resolved().is_empty());
        assert_eq!(notifier.sent().len(), 1);

        telegram.settle(Service::Zhihu, "message 1", "A", End::Saved, 0).await;
        assert_eq!(notifier.resolved().len(), 1);
    }

    #[tokio::test]
    async fn the_last_waiting_item_given_up_closes_the_outage_too() {
        let notifier = Arc::new(RecordingNotifier::default());
        let wechat = outages(&notifier, "wechat", "WeChat");
        wechat.report(Service::TikHubChannels, "channel_video 1", "A", "down").await;

        wechat
            .settle(Service::TikHubChannels, "channel_video 1", "A", End::GivenUp, 0)
            .await;

        let resolved = notifier.resolved();
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            (resolved[0].1.kind.as_str(), resolved[0].1.body.as_str()),
            ("download_failed", "No video waits on the outage any more: the last was given up.")
        );
        // Nothing stands in the way of the next outage's report.
        wechat.report(Service::TikHubChannels, "channel_video 2", "B", "down").await;
        assert_eq!(notifier.sent().len(), 2);
    }

    #[test]
    fn an_item_waited_once_it_is_past_the_tries_any_item_gets() {
        assert!(!waited(MAX_ATTEMPTS));
        assert!(waited(MAX_ATTEMPTS + 1));
    }

    #[tokio::test]
    async fn a_notifier_that_fails_is_only_logged() {
        let failing = RecordingNotifier::failing();
        outages(&failing, "wechat", "WeChat")
            .report(Service::Zhihu, "article 1", "A", "down")
            .await;
        assert!(failing.sent().is_empty());

        let failing = RecordingNotifier::failing_resolve();
        let wechat = outages(&failing, "wechat", "WeChat");
        wechat.report(Service::Zhihu, "article 1", "A", "down").await;
        wechat.settle(Service::Zhihu, "article 2", "B", End::Saved, 0).await;
        assert!(failing.resolved().is_empty());
    }
}
