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
use std::sync::mpsc::{Sender, channel};

use agens_store::EventRow;

use crate::api::{EventFeed, EventFilter, PortError, Subscription};

/// One subscriber: where to send, and what it asked for.
struct Subscriber {
    filter: EventFilter,
    outbound: Sender<EventRow>,
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
pub struct JournalFeed {
    subscribers: Mutex<Vec<Subscriber>>,
}

impl JournalFeed {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Hands one journal entry to every subscriber that asked for it, dropping
    /// the ones whose receiver is gone.
    ///
    /// A client that disconnected is not an error and is not retried: its end
    /// of the channel closed, which is the only signal a fan-out gets and the
    /// only one it needs.
    pub fn publish(&self, event: &EventRow, repo_id: Option<&str>) {
        let Ok(mut subscribers) = self.subscribers.lock() else {
            return;
        };

        subscribers.retain(|subscriber| {
            if !subscriber.wants(event, repo_id) {
                return true;
            }

            subscriber.outbound.send(event.clone()).is_ok()
        });
    }

    /// How many subscribers are still listening. The publisher reads it to skip
    /// the journal entirely while nobody is watching.
    #[must_use]
    pub fn subscribers(&self) -> usize {
        self.subscribers
            .lock()
            .map_or(0, |subscribers| subscribers.len())
    }
}

impl EventFeed for JournalFeed {
    fn subscribe(&self, filter: &EventFilter) -> Result<Subscription, PortError> {
        let (outbound, inbound) = channel();

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
