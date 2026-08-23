//! The live fan-out of the coordinator's journal.
//!
//! Every subscriber gets its own channel and its own filter, and the journal is
//! read once for all of them: `events.id` is assigned in commit order by a
//! single writer, so one watermark over the whole table is the position every
//! subscriber shares. Polling rather than a hook inside the writer, because a
//! transaction that fanned out before it committed would show a subscriber a
//! state the store may still roll back.
//!
//! A subscription starts at the journal's head. It is a live stream, not a
//! replay: a client that wants what already happened asks for the run's detail,
//! which is the projection built for exactly that.

use std::sync::Mutex;
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

use agens_store::EventRow;

use crate::api::{EventFeed, EventFilter, PortError, Subscription};

/// How many journal entries one subscriber may have waiting before the fan-out
/// gives up on it.
///
/// The channel is bounded rather than unbounded because the publisher never
/// waits: a subscriber that stops draining would otherwise grow this queue for
/// as long as the daemon runs, and the memory it grows into belongs to the
/// daemon rather than to the client that stopped reading.
const SUBSCRIBER_BACKLOG: usize = 1024;

/// One subscriber: where to send, and what it asked for.
struct Subscriber {
    filter: EventFilter,
    outbound: SyncSender<EventRow>,
}

impl Subscriber {
    /// Whether this entry is one this subscriber asked for.
    ///
    /// An unset repository or run means "not filtered by that", and an empty
    /// class list means every class. The repository is compared through the run
    /// the entry belongs to, which the publisher resolves once per entry rather
    /// than once per subscriber.
    fn wants(&self, event: &EventRow, repo_id: Option<&str>) -> bool {
        if let Some(wanted) = &self.filter.repo_id
            && repo_id != Some(wanted.as_str())
        {
            return false;
        }

        if let Some(wanted) = self.filter.run_id
            && event.run_id != Some(wanted)
        {
            return false;
        }

        self.filter.classes.is_empty() || self.filter.classes.contains(&event.class)
    }
}

/// The fan-out every client's `Subscribe` reaches.
#[derive(Default)]
pub(crate) struct JournalFeed {
    subscribers: Mutex<Vec<Subscriber>>,
}

impl JournalFeed {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Hands one journal entry to every subscriber that asked for it, dropping
    /// the ones whose receiver is gone and the ones that fell too far behind.
    ///
    /// A client that disconnected is not an error and is not retried: its end
    /// of the channel closed, which is the only signal a fan-out gets and the
    /// only one it needs.
    ///
    /// A backlog that is full ends the subscription rather than blocking or
    /// skipping the entry. Blocking would let one client that stopped reading
    /// stall the publisher for every other subscriber, and skipping would leave
    /// a hole in an append-only sequence with nothing in the stream to say so.
    /// Ending it is the one outcome the client can act on: the stream closes,
    /// and a subscription that starts again is live from the head with the
    /// run's detail available for whatever it missed.
    pub(crate) fn publish(&self, event: &EventRow, repo_id: Option<&str>) {
        let Ok(mut subscribers) = self.subscribers.lock() else {
            return;
        };

        subscribers.retain(|subscriber| {
            if !subscriber.wants(event, repo_id) {
                return true;
            }

            match subscriber.outbound.try_send(event.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
            }
        });
    }

    /// How many subscribers are still listening. The publisher reads it to skip
    /// the journal entirely while nobody is watching.
    #[must_use]
    pub(crate) fn subscribers(&self) -> usize {
        self.subscribers
            .lock()
            .map_or(0, |subscribers| subscribers.len())
    }
}

impl EventFeed for JournalFeed {
    fn subscribe(&self, filter: &EventFilter) -> Result<Subscription, PortError> {
        let (outbound, inbound) = sync_channel(SUBSCRIBER_BACKLOG);

        self.subscribers
            .lock()
            .map_err(|_| PortError::new("feed", "the fan-out is unusable after a failed send"))?
            .push(Subscriber {
                filter: filter.clone(),
                outbound,
            });

        Ok(inbound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::TryRecvError;

    use agens_store::EventClass;

    fn entry(id: i64) -> EventRow {
        EventRow {
            id: Some(id),
            run_id: Some(7),
            event_type: "checkpoint".to_owned(),
            class: EventClass::Agent,
            payload: r#"{"claim":"the parser handles escapes"}"#.to_owned(),
            ts: 1_700_000_000 + id,
        }
    }

    #[test]
    fn a_subscriber_that_stops_reading_is_dropped_once_its_backlog_is_full() {
        let feed = JournalFeed::new();
        let subscription = feed
            .subscribe(&EventFilter::default())
            .expect("the fan-out registers the subscriber");

        for id in 1..=i64::try_from(SUBSCRIBER_BACKLOG).unwrap() {
            feed.publish(&entry(id), None);
        }

        assert_eq!(
            feed.subscribers(),
            1,
            "a backlog that is exactly full is still being served"
        );

        feed.publish(&entry(i64::try_from(SUBSCRIBER_BACKLOG).unwrap() + 1), None);

        assert_eq!(
            feed.subscribers(),
            0,
            "the entry that did not fit ends the subscription"
        );

        let mut delivered = 0;
        loop {
            match subscription.try_recv() {
                Ok(_) => delivered += 1,
                Err(TryRecvError::Empty) => {
                    panic!("the sending end is gone, so this cannot be empty")
                }
                Err(TryRecvError::Disconnected) => break,
            }
        }

        assert_eq!(
            delivered, SUBSCRIBER_BACKLOG,
            "what the subscriber queued is bounded by the backlog and nothing more"
        );
    }

    #[test]
    fn a_subscriber_that_keeps_reading_is_never_dropped() {
        let feed = JournalFeed::new();
        let subscription = feed
            .subscribe(&EventFilter::default())
            .expect("the fan-out registers the subscriber");

        for id in 1..=i64::try_from(SUBSCRIBER_BACKLOG).unwrap() * 3 {
            feed.publish(&entry(id), None);
            assert_eq!(subscription.recv().map(|event| event.id), Ok(Some(id)));
        }

        assert_eq!(feed.subscribers(), 1);
    }
}
