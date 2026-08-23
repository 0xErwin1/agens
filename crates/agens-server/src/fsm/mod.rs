//! The coordinator's three state machines: runs, worktrees and questions.
//!
//! Each machine is a table of transitions held as data. A row pairs a source
//! state and a trigger with the state it leads to, the guard that has to hold,
//! the effects it declares, and the domain event it journals. Nothing outside
//! those tables can move a row: a caller names a trigger, never a destination,
//! so a caller cannot force a state the table has no path to.
//!
//! Two invariants hold across all three:
//!
//! - **The guard runs before anything is written.** A refused transition
//!   returns [`TransitionRejection`] with the store untouched. Behind the guard
//!   the write is still conditional on the state that was read, so a row that
//!   moved in between is refused rather than overwritten.
//! - **No transition is silent.** Every applied transition journals the generic
//!   `run_state_changed` event plus its own domain event, in that order and in
//!   the same transaction as the state change. The generic event is what a
//!   subscriber can follow without knowing every domain event by name; the
//!   worktree and question machines emit it too, because a subscriber watching
//!   one run should not have to learn three vocabularies to see that something
//!   moved.
//!
//! Effects are declared as data next to the transition, and they divide in two.
//! A few are state this store owns, and the machine writes them inside the same
//! transaction. The rest name work outside it — admitting to a slot, launching
//! or suspending a session, queueing a safe-point delivery, cancelling — and
//! are returned to the caller to perform. The scheduler, the timer wheel, the
//! gates and the API core are the callers, and they read the effects rather
//! than re-deriving what a transition implied.
//!
//! Time always comes from the caller. The machines read no clock, so a
//! coordinator reconciling after a restart decides what "now" means.

mod questions;
mod runs;
mod worktrees;

use agens_store::{
    CheckpointWrite, ControlPlaneError, ControlPlaneStore, EventClass, EventRow, FindingRow,
    QuestionRow, RunRow,
};

pub use questions::{
    AppliedQuestionTransition, QUESTION_TRANSITIONS, QuestionEffect, QuestionFacts, QuestionGuard,
    QuestionTransition, QuestionTrigger,
};
pub use runs::{
    AppliedRunTransition, RUN_TRANSITIONS, RunEffect, RunFacts, RunGuard, RunTransition, RunTrigger,
};
pub use worktrees::{
    AppliedWorktreeTransition, WORKTREE_TRANSITIONS, WorktreeEffect, WorktreeFacts, WorktreeGuard,
    WorktreeTransition, WorktreeTrigger,
};

/// Who is asking for a transition.
///
/// Only the two guards that need it read this: approving an execution is the
/// user's alone, and the facts a run's own lifecycle turns on are reported by
/// the coordinator's ingest rather than claimed by a client. The full
/// per-principal authorization surface belongs to the API core.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Principal {
    User,
    Praetor,
    /// The coordinator acting on its own facts: ingest, the timer wheel, boot
    /// reconciliation.
    #[default]
    Coordinator,
}

impl Principal {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Praetor => "praetor",
            Self::Coordinator => "coordinator",
        }
    }
}

/// Why a transition was refused. Nothing was written in any of these cases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransitionRejection {
    /// The row the transition was asked for does not exist.
    NoSuchRow {
        table: &'static str,
        id: i64,
    },
    /// No row of the transition table pairs this state with this trigger. This
    /// is where a caller trying to force a state lands.
    NoSuchTransition {
        machine: &'static str,
        from: &'static str,
        trigger: &'static str,
    },
    /// The transition exists, but its guard did not hold.
    GuardFailed {
        machine: &'static str,
        guard: &'static str,
        detail: String,
    },
    /// The row moved between the read the guard ran against and the write.
    Conflict(String),
    Storage(String),
}

impl std::fmt::Display for TransitionRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchRow { table, id } => write!(formatter, "no {table} with id {id}"),
            Self::NoSuchTransition {
                machine,
                from,
                trigger,
            } => write!(
                formatter,
                "the {machine} machine has no transition out of {from} on {trigger}"
            ),
            Self::GuardFailed {
                machine,
                guard,
                detail,
            } => write!(formatter, "{machine} guard {guard} did not hold: {detail}"),
            Self::Conflict(detail) => write!(formatter, "state moved under the caller: {detail}"),
            Self::Storage(detail) => write!(formatter, "{detail}"),
        }
    }
}

impl std::error::Error for TransitionRejection {}

impl From<ControlPlaneError> for TransitionRejection {
    fn from(error: ControlPlaneError) -> Self {
        if error.is_conflict() {
            Self::Conflict(error.to_string())
        } else {
            Self::Storage(error.to_string())
        }
    }
}

/// One applied transition: where the row was, where it is, what the caller
/// still has to do, and the pair of journal entries that announced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedTransition<S, E: 'static> {
    pub from: S,
    pub to: S,
    /// Declared by the transition table. The ones this machine persists are
    /// already written; the rest are the caller's to perform.
    pub effects: &'static [E],
    pub domain_event: &'static str,
    pub state_changed_event_id: i64,
    pub domain_event_id: i64,
    /// The question the transition opened. Only the run machine's `ask` ever
    /// opens one; the other two machines leave it `None`.
    pub opened_question_id: Option<i64>,
}

/// The result of asking for a transition that may already have happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransitionOutcome<S, E: 'static> {
    Applied(AppliedTransition<S, E>),
    /// The row is already in the state the trigger leads to, so there is
    /// nothing to move and nothing to journal. Cancellation is idempotent and
    /// is the only trigger that reaches this.
    AlreadySettled,
}

impl<S, E> TransitionOutcome<S, E> {
    /// The applied transition, or `None` when the request was a no-op.
    #[must_use]
    pub const fn applied(&self) -> Option<&AppliedTransition<S, E>> {
        match self {
            Self::Applied(transition) => Some(transition),
            Self::AlreadySettled => None,
        }
    }
}

/// The three state machines over one control-plane store.
///
/// It owns the store because the machines are the only writer of the state they
/// govern: handing out a second handle to the same tables would put a caller in
/// a position to move a row without a guard, an event pair, or both.
pub struct StateMachines {
    store: ControlPlaneStore,
}

impl StateMachines {
    #[must_use]
    pub const fn new(store: ControlPlaneStore) -> Self {
        Self { store }
    }

    /// Read access for callers that project the control plane rather than move
    /// it: the scheduler's eligibility query, the read models, the gates.
    #[must_use]
    pub const fn store(&self) -> &ControlPlaneStore {
        &self.store
    }

    /// Opens a new run row and returns its id.
    ///
    /// Not a transition: there is no row to move yet, and no guard to run
    /// against a state that does not exist. It goes through the machines all
    /// the same, because they are the single writer of the control-plane
    /// tables — a second handle used only for inserts would be a second writer
    /// no matter how narrow its use.
    pub fn open_run(&mut self, run: &RunRow) -> Result<i64, agens_store::ControlPlaneError> {
        self.store.insert_run(run)
    }

    /// Opens a question against a run that no transition asked for.
    ///
    /// Not a transition, for the same reason [`Self::open_run`] is not: there
    /// is no question row yet, so there is no state to guard. The run machine's
    /// `ask` remains the only way a *running* run parks on one — this exists
    /// for the question a run is created with, where nothing is parked and
    /// nothing moves.
    pub fn open_question(
        &mut self,
        question: &QuestionRow,
    ) -> Result<i64, agens_store::ControlPlaneError> {
        self.store.insert_question(question)
    }

    /// Journals facts that no transition carries, in one transaction and in the
    /// order given.
    ///
    /// A gate's verdict is the case this exists for: it is a fact about a run
    /// whether or not anything moved, and a refused gate moves nothing at all.
    /// Routing it through here rather than a second store handle keeps this the
    /// only writer of the control-plane tables, which is the property the
    /// machines, the scheduler and the timers are built on.
    pub fn journal(&mut self, events: &[EventRow]) -> Result<Vec<i64>, TransitionRejection> {
        let outcome = self.store.apply_transition(&agens_store::TransitionWrite {
            run_id: events
                .first()
                .and_then(|event| event.run_id)
                .unwrap_or_default(),
            run_state: None,
            worktree_status: None,
            question: None,
            new_question: None,
            attempt: None,
            close_attempt: None,
            provider: None,
            events,
        })?;

        Ok(outcome.event_ids)
    }

    /// Records one checkpoint: its journal entry and a finding per claim, in
    /// one write.
    ///
    /// Separate from [`Self::journal`] because a checkpoint is not only facts:
    /// its findings have to land with the entry they are attributed to, and the
    /// entry's own id is what attributes them. It is still not a transition —
    /// a checkpoint moves no row and runs no guard, because what it writes is
    /// the journal and the evidence, which is exactly what a run being measured
    /// is allowed to add to.
    pub fn record_checkpoint(
        &mut self,
        checkpoint: &EventRow,
        findings: &[FindingRow],
    ) -> Result<CheckpointWrite, TransitionRejection> {
        Ok(self.store.record_checkpoint(checkpoint, findings)?)
    }

    fn load_run(&self, run_id: i64) -> Result<RunRow, TransitionRejection> {
        self.store
            .load_run(run_id)?
            .ok_or(TransitionRejection::NoSuchRow {
                table: "run",
                id: run_id,
            })
    }

    fn load_question(&self, question_id: i64) -> Result<QuestionRow, TransitionRejection> {
        self.store
            .load_question(question_id)?
            .ok_or(TransitionRejection::NoSuchRow {
                table: "question",
                id: question_id,
            })
    }
}

/// One transition as the journal describes it, independent of which machine it
/// came from.
struct JournaledMove {
    run_id: i64,
    ts: i64,
    class: EventClass,
    machine: &'static str,
    from: &'static str,
    to: &'static str,
    trigger: &'static str,
    domain_event: &'static str,
}

/// Builds the pair of journal entries every applied transition writes: the
/// generic one first so a subscriber sees the move before whatever the domain
/// event says about it, then the domain event with the same move plus the
/// detail that only its own name explains.
fn transition_events(moved: &JournaledMove, domain_detail: &serde_json::Value) -> [EventRow; 2] {
    let JournaledMove {
        run_id,
        ts,
        class,
        machine,
        from,
        to,
        trigger,
        domain_event,
    } = *moved;

    let move_description = serde_json::json!({
        "machine": machine,
        "from": from,
        "to": to,
        "trigger": trigger,
    });

    let mut domain_payload = move_description.clone();
    if let (Some(target), Some(detail)) =
        (domain_payload.as_object_mut(), domain_detail.as_object())
    {
        for (key, value) in detail {
            target.insert(key.clone(), value.clone());
        }
    }

    [
        EventRow {
            id: None,
            run_id: Some(run_id),
            event_type: "run_state_changed".to_owned(),
            class,
            payload: move_description.to_string(),
            ts,
        },
        EventRow {
            id: None,
            run_id: Some(run_id),
            event_type: domain_event.to_owned(),
            class,
            payload: domain_payload.to_string(),
            ts,
        },
    ]
}

/// Splits the two journal ids an applied transition wrote, in the order
/// [`transition_events`] produced them.
fn event_pair(ids: &[i64]) -> Result<(i64, i64), TransitionRejection> {
    match ids {
        [state_changed, domain] => Ok((*state_changed, *domain)),
        _ => Err(TransitionRejection::Storage(
            "an applied transition must journal exactly the generic event and its domain event"
                .to_owned(),
        )),
    }
}
