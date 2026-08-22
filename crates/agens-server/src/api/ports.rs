//! The seams the API core reaches the rest of the coordinator through.
//!
//! The core owns authorization and the transitions; it owns none of the work a
//! transition implies. Admission, git re-derivation, the safe-point queue, the
//! sessions and the event fan-out each belong to a component of their own, and
//! each arrives here as a trait so the core can be built and tested before they
//! exist.
//!
//! Every port is fallible and none of them is silently skipped: a transition
//! whose effects could not be performed is reported, not swallowed.

use std::fmt;
use std::sync::mpsc::Receiver;

use agens_store::{EventClass, EventRow, RunRow};

/// Why a port could not do what it was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortError {
    port: &'static str,
    detail: String,
}

impl PortError {
    #[must_use]
    pub fn new(port: &'static str, detail: impl Into<String>) -> Self {
        Self {
            port,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub const fn port(&self) -> &'static str {
        self.port
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for PortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "the {} port refused: {}", self.port, self.detail)
    }
}

impl std::error::Error for PortError {}

/// Whether queued runs are admitted at all, and when the queue moved.
///
/// Not [`crate::Scheduler`], and not a narrower view of it. That one decides
/// which queued runs fit right now and starts them, reading everything it needs
/// from the store on each tick. This one carries the two things the core has
/// that a tick cannot derive: the operator's toggle, which says whether a tick
/// should run, and the fact that an approval, an answer or a retry just put a
/// run in the queue, which is what gives a tick its occasion.
pub trait SchedulerPort: Send + Sync {
    fn admissions_paused(&self) -> bool;

    /// Sets the toggle and reports what it was before, so a caller can tell a
    /// change from a repeat.
    fn set_admissions_paused(&self, paused: bool) -> Result<bool, PortError>;

    /// A run entered the queue.
    fn queue_changed(&self, run_id: i64);
}

/// What a worktree's live topology says right now.
///
/// Every field is re-derived from git at the moment of the call. Nothing here
/// is read from a stored column: a merge that a flag claims and git does not is
/// exactly the state the derivation exists to refuse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeDerivation {
    pub branch_merged: bool,
    /// Nothing uncommitted left to lose.
    pub worktree_clean: bool,
    /// `HEAD^{tree}` as it stands, for comparison against an approval's frozen
    /// receipt.
    pub tree_hash: String,
    /// The digest of the paths the worktree touched, the other half of that
    /// receipt.
    pub paths_digest: String,
}

/// Git derivation and disposal of a run's worktree.
///
/// [`crate::Gates`] does not satisfy this and is not meant to: `reclaim` is a
/// complete operation that derives, journals its verdict and applies the
/// worktree transition itself, it needs a target ref this port has no reason to
/// carry, and it has no manual-disposition path at all. Which of the two owns
/// the reclaim sweep once the daemon is wired is a composition question, and it
/// comes with the one below.
///
/// Both [`crate::Gates`] and the API core take [`crate::StateMachines`] by
/// value, because each is the sole writer of the tables it moves. A daemon can
/// build one of them, not both, so wiring them together settles that ownership
/// first.
pub trait WorktreeGate: Send + Sync {
    fn derive(&self, run: &RunRow) -> Result<WorktreeDerivation, PortError>;

    /// Removes the worktree directory. Called only behind a transition that
    /// already moved the row to `cleaned`.
    fn remove(&self, run: &RunRow) -> Result<(), PortError>;
}

/// Which edge of the worker's execution a delivery waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryGrain {
    /// The worker keeps doing the same thing, better informed. Waiting for the
    /// end of the turn would delay the correction without buying anything.
    ToolCall,
    /// The worker has to replan, so it needs a point where its plan is closed.
    Turn,
}

/// What is being handed to the worker at a safe point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryPayload {
    Answer {
        question_id: i64,
        text: String,
    },
    /// Guidance that moves what the run is doing.
    Directive(String),
    /// The nudge a parked or interrupted run comes back on.
    Continue,
}

impl DeliveryPayload {
    /// The grain this payload is delivered at.
    ///
    /// The cut is whether the payload moves the approved scope. An answer to a
    /// detail question does not, so it lands at the nearest tool-call edge. A
    /// directive and a resume both change what the run is doing, and the
    /// divergence detector measures a turn's touched paths as one unit, so they
    /// wait for the turn to close.
    #[must_use]
    pub const fn grain(&self) -> DeliveryGrain {
        match self {
            Self::Answer { .. } => DeliveryGrain::ToolCall,
            Self::Directive(_) | Self::Continue => DeliveryGrain::Turn,
        }
    }
}

/// One queued safe-point delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub run_id: i64,
    pub payload: DeliveryPayload,
    pub grain: DeliveryGrain,
}

impl Delivery {
    #[must_use]
    pub fn new(run_id: i64, payload: DeliveryPayload) -> Self {
        let grain = payload.grain();

        Self {
            run_id,
            payload,
            grain,
        }
    }
}

/// The durable safe-point queue a worker drains at a defined edge.
pub trait DeliveryQueue: Send + Sync {
    fn enqueue(&self, delivery: &Delivery) -> Result<(), PortError>;
}

/// A live handle on the session of a run the user took over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TakeoverHandle {
    pub run_id: i64,
    pub session_id: i64,
}

/// How far a stop reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopScope {
    Run(i64),
    Repo(String),
    Machine,
}

/// The worker sessions a run executes in.
pub trait SessionControl: Send + Sync {
    /// The one signal that waits for no edge.
    fn cancel(&self, run_id: i64) -> Result<(), PortError>;

    fn take_over(&self, run_id: i64) -> Result<TakeoverHandle, PortError>;

    fn stop(&self, scope: &StopScope) -> Result<(), PortError>;
}

/// What a subscriber wants to see.
///
/// The repository is part of the filter because one daemon serves N projects,
/// so an unfiltered stream would carry another repository's runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventFilter {
    pub repo_id: Option<String>,
    pub run_id: Option<i64>,
    /// Empty means every class.
    pub classes: Vec<EventClass>,
}

/// A subscriber's end of the journal fan-out.
pub type Subscription = Receiver<EventRow>;

/// The live fan-out of the coordinator's journal.
pub trait EventFeed: Send + Sync {
    fn subscribe(&self, filter: &EventFilter) -> Result<Subscription, PortError>;
}
