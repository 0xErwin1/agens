//! What every streaming method on this facade shares: a ceiling on how many
//! streams the daemon forwards at once, and how one entry is handed to a client
//! that may have stopped reading.
//!
//! Both planes that stream — the journal's `Subscribe` and a hosted chat's —
//! take their entries off a synchronous channel on a thread of their own, and
//! both face the same client. Keeping the bound and the hand-off in one place
//! is what stops the two from disagreeing about how long a stalled client is
//! waited for.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// How many entries a slow client may fall behind before the forwarder starts
/// waiting for it.
pub(super) const SUBSCRIPTION_BUFFER: usize = 256;

/// How long a forwarder waits on a client that has stopped reading before it
/// ends the stream.
///
/// The wait is bounded rather than indefinite: a forwarder blocked forever on
/// one client holds the entries behind it and the thread carrying them. A
/// client that has not taken a single entry in this long is not reading, and
/// the stream it is not reading is the one thing that has to end for the daemon
/// to reclaim both.
pub(super) const FORWARD_PATIENCE: Duration = Duration::from_secs(30);

/// How often a forwarder looks again at a client whose buffer is full.
const FORWARD_POLL: Duration = Duration::from_millis(10);

/// How many streams the daemon forwards at once.
///
/// One forwarder is one operating-system thread, so an unbounded number of
/// streams is an unbounded number of threads: a client that opens them in a
/// loop costs the daemon a stack apiece until the process cannot spawn another,
/// and what fails then is whatever asked for a thread next, not the client that
/// took them. Well past what any number of attached clients wants, and far
/// short of what a machine will not give.
pub(super) const LIVE_SUBSCRIPTIONS: usize = 64;

/// The ceiling on live forwarders, and the slots taken against it.
///
/// A slot is held by the forwarder rather than by the subscription: what costs
/// the thread is the forwarding, and it is over exactly when the thread ends,
/// however it ends.
#[derive(Debug)]
pub(super) struct SubscriptionSlots {
    live: AtomicUsize,
    ceiling: usize,
}

impl SubscriptionSlots {
    pub(super) const fn new(ceiling: usize) -> Self {
        Self {
            live: AtomicUsize::new(0),
            ceiling,
        }
    }

    /// Takes one slot, or `None` when the ceiling is already reached.
    pub(super) fn take(self: &Arc<Self>) -> Option<SubscriptionSlot> {
        let mut live = self.live.load(Ordering::Acquire);

        loop {
            if live >= self.ceiling {
                return None;
            }

            match self.live.compare_exchange_weak(
                live,
                live + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(SubscriptionSlot {
                        slots: Arc::clone(self),
                    });
                }
                Err(current) => live = current,
            }
        }
    }
}

/// One live forwarder's claim on a slot, released when it ends.
#[derive(Debug)]
pub(super) struct SubscriptionSlot {
    slots: Arc<SubscriptionSlots>,
}

impl Drop for SubscriptionSlot {
    fn drop(&mut self) {
        self.slots.live.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Hands one entry to the client, waiting at most `patience` for room.
///
/// Written against the synchronous half of the channel rather than awaiting
/// `send_timeout`, because this runs on a thread of its own with no runtime
/// under it: entering one to wait would tie the forwarder's timeout to a
/// runtime it does not belong to.
///
/// `false` means the stream is over — the client hung up, or it has not taken a
/// single entry for longer than the daemon waits.
pub(super) fn forward<T>(
    sender: &tokio::sync::mpsc::Sender<T>,
    entry: T,
    patience: Duration,
) -> bool {
    use tokio::sync::mpsc::error::TrySendError;

    let deadline = Instant::now() + patience;
    let mut entry = entry;

    loop {
        match sender.try_send(entry) {
            Ok(()) => return true,
            Err(TrySendError::Closed(_)) => return false,
            Err(TrySendError::Full(_)) if Instant::now() >= deadline => return false,
            Err(TrySendError::Full(returned)) => {
                entry = returned;
                std::thread::sleep(FORWARD_POLL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daemon_forwards_for_as_many_subscriptions_as_it_has_threads_for() {
        let slots = Arc::new(SubscriptionSlots::new(2));

        let first = slots.take().expect("the first subscription fits");
        let second = slots.take().expect("the second subscription fits");

        assert!(
            slots.take().is_none(),
            "a subscription past the ceiling is refused rather than given a thread"
        );

        drop(first);

        assert!(
            slots.take().is_some(),
            "a stream that ended gave its thread back"
        );

        drop(second);
    }

    #[test]
    fn a_refused_subscription_takes_no_slot_with_it() {
        let slots = Arc::new(SubscriptionSlots::new(1));
        let held = slots.take().expect("the first subscription fits");

        assert!(slots.take().is_none());
        assert!(slots.take().is_none(), "a refusal is not a reservation");

        drop(held);

        assert!(slots.take().is_some());
    }

    #[test]
    fn a_client_that_takes_nothing_ends_its_own_stream() {
        const PATIENCE: Duration = Duration::from_millis(50);

        let (sender, _receiver) = tokio::sync::mpsc::channel(1);

        assert!(forward(&sender, 1, PATIENCE), "the first entry fits");

        let started = Instant::now();

        assert!(
            !forward(&sender, 2, PATIENCE),
            "the entry that does not fit ends the stream rather than waiting forever"
        );
        assert!(started.elapsed() >= PATIENCE, "it waited before giving up");
    }

    #[test]
    fn a_client_that_catches_up_within_the_wait_keeps_its_stream() {
        const PATIENCE: Duration = Duration::from_secs(10);

        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);

        assert!(forward(&sender, 1, PATIENCE));

        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let taken = receiver.blocking_recv();

            (receiver, taken)
        });

        assert!(
            forward(&sender, 2, PATIENCE),
            "room that appears inside the wait is used rather than missed"
        );

        let (_receiver, taken) = reader.join().unwrap();
        assert!(taken.is_some());
    }

    #[test]
    fn a_client_that_hung_up_ends_the_forwarder_at_once() {
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        drop(receiver);

        let started = Instant::now();

        assert!(!forward(&sender, 1, Duration::from_secs(60)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a closed stream is not something to wait out"
        );
    }
}
