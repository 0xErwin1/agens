//! Admission's toggle, and the occasion a tick runs on.
//!
//! The scheduler holds no queue: every tick rebuilds it from the store. What it
//! cannot derive is when to look, and whether it is allowed to. Both live here,
//! so the core can pause admission and announce that a run entered the queue
//! without knowing anything about the loop that acts on it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::api::{AdmissionControl, PortError};

/// The operator's toggle and the scheduler loop's doorbell.
///
/// A run entering the queue only sets a flag: the loop reads the queue from the
/// store, so several announcements between two ticks are one occasion to look,
/// and the run id they named is not carried anywhere.
#[derive(Debug)]
pub(crate) struct Admissions {
    paused: AtomicBool,
    /// Whether something happened that a tick has not looked at yet.
    pending: Mutex<bool>,
    doorbell: Condvar,
}

impl Default for Admissions {
    fn default() -> Self {
        Self::new()
    }
}

impl Admissions {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            pending: Mutex::new(false),
            doorbell: Condvar::new(),
        }
    }

    /// Waits until a run enters the queue or `timeout` elapses, and reports
    /// whether admission is open.
    ///
    /// The periodic wake-up is not redundant with the doorbell: a slot frees
    /// when a session ends, which nothing announces, and a tick that only ran
    /// on new arrivals would leave the queue standing behind a ceiling that has
    /// already lifted.
    pub(crate) fn wait_for_occasion(&self, timeout: Duration) -> bool {
        let Ok(pending) = self.pending.lock() else {
            return false;
        };

        let outcome = self
            .doorbell
            .wait_timeout_while(pending, timeout, |pending| !*pending);

        if let Ok((mut pending, _)) = outcome {
            *pending = false;
        }

        !self.paused.load(Ordering::Acquire)
    }

    /// Wakes the loop without anything having entered the queue, so a daemon
    /// that is shutting down does not wait out the poll interval first.
    pub(crate) fn wake(&self) {
        self.announce();
    }

    fn announce(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = true;
        }

        self.doorbell.notify_all();
    }
}

impl AdmissionControl for Admissions {
    fn admissions_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Sets the toggle and reports what it was, so `stop` can tell a team it
    /// just paused from one that was already paused.
    fn set_admissions_paused(&self, paused: bool) -> Result<bool, PortError> {
        let previous = self.paused.swap(paused, Ordering::AcqRel);

        // Resuming has to wake the loop: the runs it has to look at entered the
        // queue while it was refusing to admit them, and nothing announces them
        // a second time.
        if previous && !paused {
            self.announce();
        }

        Ok(previous)
    }

    fn queue_changed(&self, _run_id: i64) {
        self.announce();
    }
}
