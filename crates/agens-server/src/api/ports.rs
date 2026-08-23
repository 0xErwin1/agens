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
use std::path::{Path, PathBuf};
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
///
/// Named for what it controls rather than for what implements it: a port called
/// after the scheduler reads as a handle on the scheduler, which is the one
/// thing it is not.
pub trait AdmissionControl: Send + Sync {
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
/// **This is a seam, not a second gate.** [`crate::Gates`] owns the pre-merge
/// and reclaim sweeps: it derives, journals its verdict, spends the
/// authorization and applies the worktree transition itself. This port owns
/// none of that. It answers what git says and disposes of a directory once the
/// core has already moved the row, which is what the core's own operations —
/// `cleaning`, and freezing an approval's receipt — need and all they need.
///
/// The ownership question the two used to share is settled: the API core owns
/// [`crate::StateMachines`], and the gates borrow them from it for the span of
/// one sweep through `ApiCore::machines_mut`. There is one owner and one
/// writer, so the daemon runs both without building a second control plane.
/// Both reach git through the same `SessionWorktrees` derivation, so the
/// receipt this port freezes and the one the gate re-derives cannot disagree
/// about what a digest of one worktree is.
pub trait WorktreeGate: Send + Sync {
    fn derive(&self, run: &RunRow) -> Result<WorktreeDerivation, PortError>;

    /// Removes the worktree directory. Called only behind a transition that
    /// already moved the row to `cleaned`.
    fn remove(&self, run: &RunRow) -> Result<(), PortError>;

    /// The identity of the repository a run is being created against.
    ///
    /// Derived rather than taken from the request: a caller that named its own
    /// would decide which repository's runs, events and questions its work is
    /// grouped with, and the grouping is the coordinator's.
    fn identify(&self, repository: &Path) -> Result<RepositoryIdentity, PortError>;

    /// Creates the worktree a new run works in and applies the repository's own
    /// provisioning contract to it.
    ///
    /// The two halves are one port call because a worktree git created and the
    /// contract did not reach is not a worktree a run can start in, and undoing
    /// the first half is this port's business rather than its caller's.
    fn provision(&self, request: &WorktreeRequest<'_>) -> Result<ProvisionedWorktree, PortError>;
}

/// One repository, as the control plane groups by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryIdentity {
    /// The fingerprint every worktree of this repository shares. Never joined
    /// with a session's confinement root, which is per worktree on purpose.
    pub repo_id: String,
    /// Persisted beside the fingerprint so a changed origin is diagnosable
    /// rather than only orphaning rows.
    pub remote_url: Option<String>,
}

/// Whether this run's provisioning hooks may run, as the core decided it.
///
/// The decision is the core's and travels with the request, because the port
/// knows how to execute a hook and nothing about who asked for it. There is no
/// variant that means "decide for yourself".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookPolicy {
    /// The operator has authorized this repository's hooks.
    Allow,
    /// They do not run, and the core opens the question that would authorize
    /// them for the next run.
    Ask,
    /// They do not run, and nothing is asked.
    Deny,
}

/// The worktree one new run is asking for.
#[derive(Clone, Copy, Debug)]
pub struct WorktreeRequest<'a> {
    /// The checkout it is created from, and the only source files are copied
    /// out of.
    pub repository: &'a Path,
    pub repo_id: &'a str,
    /// The directory name under this repository's worktrees, and the segment a
    /// person reads when they `cd` into it.
    pub name: &'a str,
    pub branch: &'a str,
    /// The commit the branch starts from.
    pub start_point: &'a str,
    pub hooks: HookPolicy,
}

/// A worktree that exists on disk and has had its contract applied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProvisionedWorktree {
    pub path: PathBuf,
    /// Every provisioning hook that did not succeed and was continued past.
    /// A non-empty list means the run starts in an environment that is not what
    /// the repository declared, so the worker has to be told rather than left
    /// to discover it.
    pub hook_failures: Vec<String>,
    /// The names of the hooks the repository declared, whether or not they ran.
    /// Empty for a repository that declares none, which is what separates
    /// "nothing to authorize" from "not authorized".
    pub declared_hooks: Vec<String>,
    /// Whether those hooks were executed.
    pub hooks_ran: bool,
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
