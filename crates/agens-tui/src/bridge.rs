use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

const RETRY_QUANTUM: Duration = Duration::from_millis(5);

#[derive(Clone, Debug, Default)]
pub struct BridgeCancel(Arc<AtomicBool>);

impl BridgeCancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiEnvelope<T> {
    ordinal: u64,
    event: T,
}

impl<T> UiEnvelope<T> {
    pub fn into_parts(self) -> (u64, T) {
        (self.ordinal, self.event)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    Published { ordinal: u64 },
    Cancelled,
    DeadlineExpired,
    Disconnected,
    Closed,
}

struct BridgeState {
    closed: AtomicBool,
    next_ordinal: AtomicU64,
    publication: Mutex<()>,
    wake_lock: Mutex<()>,
    wake: Condvar,
}

pub struct BridgeTx<T> {
    sender: SyncSender<UiEnvelope<T>>,
    state: Arc<BridgeState>,
}

impl<T> Clone for BridgeTx<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            state: Arc::clone(&self.state),
        }
    }
}

impl<T> BridgeTx<T> {
    pub fn bounded(capacity: usize) -> (Self, Receiver<UiEnvelope<T>>) {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let state = Arc::new(BridgeState {
            closed: AtomicBool::new(false),
            next_ordinal: AtomicU64::new(0),
            publication: Mutex::new(()),
            wake_lock: Mutex::new(()),
            wake: Condvar::new(),
        });

        (Self { sender, state }, receiver)
    }

    pub fn close(&self) {
        self.state.closed.store(true, Ordering::Release);
        self.state.wake.notify_all();
    }

    pub fn publish(
        &self,
        event: T,
        cancellation: &BridgeCancel,
        deadline: Option<Instant>,
    ) -> PublishOutcome {
        let Some(_publication) = self.wait_for_source_order(cancellation, deadline) else {
            return self.unavailable_outcome(cancellation, deadline);
        };

        let ordinal = self.state.next_ordinal.fetch_add(1, Ordering::Relaxed);
        let mut envelope = UiEnvelope { ordinal, event };

        loop {
            if let Some(outcome) = self.unavailable() {
                return outcome;
            }
            if cancellation.is_cancelled() {
                return PublishOutcome::Cancelled;
            }
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                return PublishOutcome::DeadlineExpired;
            }

            match self.sender.try_send(envelope) {
                Ok(()) => return PublishOutcome::Published { ordinal },
                Err(TrySendError::Full(unsent)) => {
                    envelope = unsent;
                    self.wait(deadline);
                }
                Err(TrySendError::Disconnected(_)) => return PublishOutcome::Disconnected,
            }
        }
    }

    fn wait_for_source_order(
        &self,
        cancellation: &BridgeCancel,
        deadline: Option<Instant>,
    ) -> Option<std::sync::MutexGuard<'_, ()>> {
        loop {
            if self.unavailable().is_some() || cancellation.is_cancelled() {
                return None;
            }
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                return None;
            }

            match self.state.publication.try_lock() {
                Ok(guard) => return Some(guard),
                Err(std::sync::TryLockError::Poisoned(error)) => return Some(error.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) => self.wait(deadline),
            }
        }
    }

    fn unavailable_outcome(
        &self,
        cancellation: &BridgeCancel,
        deadline: Option<Instant>,
    ) -> PublishOutcome {
        self.unavailable().unwrap_or_else(|| {
            if cancellation.is_cancelled() {
                PublishOutcome::Cancelled
            } else if deadline.is_some_and(|limit| Instant::now() >= limit) {
                PublishOutcome::DeadlineExpired
            } else {
                PublishOutcome::Closed
            }
        })
    }

    fn unavailable(&self) -> Option<PublishOutcome> {
        self.state
            .closed
            .load(Ordering::Acquire)
            .then_some(PublishOutcome::Closed)
    }

    fn wait(&self, deadline: Option<Instant>) {
        let guard = self
            .state
            .wake_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let timeout = deadline.map_or(RETRY_QUANTUM, |limit| {
            RETRY_QUANTUM.min(limit.saturating_duration_since(Instant::now()))
        });
        let _ = self.state.wake.wait_timeout(guard, timeout);
    }
}
