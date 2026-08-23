//! The coordinator's service core: one API, principal-checked, facade-free.
//!
//! Praetor's `team_*` tools and the clients' gRPC surface call this same core.
//! Only two things differ between them: the transport, and the [`Principal`]
//! the request arrives as. Everything that decides whether a request is allowed
//! lives here, so a new transport is a new caller and never a new authority.
//!
//! Two planes:
//!
//! - **Team** moves the control plane. Every operation goes through the
//!   authorization table, then a state machine, and only then through the ports
//!   the transition's effects name.
//! - **Feed** reads it. Nothing on this plane writes, so it is a projection of
//!   what the machines already wrote.
//!
//! The core never invokes a model and never decides what is worth doing. It
//! checks who is asking, asks the machine to move, and performs what the
//! machine said the move implied.

mod authorization;
mod feed;
mod ports;
mod runs;
mod team;

use std::sync::Arc;

use agens_store::{EventClass, EventRow};

use agens_store::RunState;

use crate::fsm::{
    Principal, RunEffect, RunFacts, RunTrigger, StateMachines, TransitionOutcome,
    TransitionRejection,
};
use crate::ingest::{RefusedReport, backlogged_event};
use crate::policy::RepositoryPolicy;
use crate::scheduler::{QueueReport, RunLauncher, Scheduler, SchedulerError, SchedulerLoad};
use crate::timers::{TimerTick, TimerWheel};

pub use authorization::{
    DetailQuestionRefusal, OPERATION_AUTHORIZATION, Operation, OperationAuthorization,
    praetor_may_answer,
};
pub use feed::{InboxItem, InboxView, RunSummary, RunView, TreeSnapshot};
pub use ports::{
    AdmissionControl, Delivery, DeliveryGrain, DeliveryPayload, DeliveryQueue, EventFeed,
    EventFilter, HookPolicy, PortError, ProvisionedWorktree, RepositoryIdentity, SessionControl,
    StopScope, Subscription, TakeoverHandle, WorktreeDerivation, WorktreeGate, WorktreeRequest,
};
pub use runs::{CreateRun, CreatedRun, PreparedRun};
pub use team::{
    AdmissionState, AnswerQuestion, AnsweredQuestion, ApprovePlan, AuthorizeMerge, CleaningAction,
    CleaningDisposition, MergeAuthorization, MergeAuthorized, RetryRequest, RunRef, StopRequest,
};

/// The journal entry a turn that ended without a completion is recorded as.
///
/// Its payload names the failure's class, what it said, and the state the run
/// was in when the turn ended: a provider that failed after the run had already
/// left `running` is a different fact from one that failed the attempt.
pub const TURN_FAILED_EVENT: &str = "turn_failed";

/// How much of a failure's own words the journal keeps. A cause is a sentence;
/// what arrives is whatever a provider, a tool or a transport produced.
const TURN_FAILURE_DETAIL_MAX_CHARS: usize = 512;

/// What a worker knows about a turn that ended without a completion.
#[derive(Clone, Copy, Debug)]
pub struct TurnFailure<'a> {
    /// The failure's class, as the error carries it.
    pub category: &'a str,
    pub detail: &'a str,
    /// Where the run's own row was when the turn ended. `None` when the run is
    /// no longer readable at all.
    pub state: Option<RunState>,
    pub now: i64,
}

/// Why a request did not go through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// The principal does not reach this operation, or does not reach it for
    /// this particular subject. Nothing was moved.
    Unauthorized {
        operation: Operation,
        principal: Principal,
        reason: String,
        /// Whether the refusal reached the journal. A refusal that could not be
        /// recorded is still a refusal, and saying so is what keeps the audit
        /// trail honest about its own gaps.
        journaled: bool,
    },
    /// The principal was allowed, but the state machine refused the move.
    Rejected(TransitionRejection),
    /// The transition applied and a port could not carry out what it implied.
    Port(PortError),
    NotFound {
        subject: &'static str,
        id: i64,
    },
    Storage(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized {
                operation,
                principal,
                reason,
                ..
            } => write!(
                formatter,
                "{} may not {}: {reason}",
                principal.as_str(),
                operation.as_str()
            ),
            Self::Rejected(rejection) => write!(formatter, "{rejection}"),
            Self::Port(error) => write!(formatter, "{error}"),
            Self::NotFound { subject, id } => write!(formatter, "no {subject} with id {id}"),
            Self::Storage(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for ApiError {}

impl From<TransitionRejection> for ApiError {
    fn from(rejection: TransitionRejection) -> Self {
        Self::Rejected(rejection)
    }
}

impl From<PortError> for ApiError {
    fn from(error: PortError) -> Self {
        Self::Port(error)
    }
}

impl From<agens_store::ControlPlaneError> for ApiError {
    fn from(error: agens_store::ControlPlaneError) -> Self {
        Self::Storage(error.to_string())
    }
}

/// The components the core performs a transition's effects through.
///
/// They are owned as trait objects rather than generics because the daemon
/// holds exactly one core and swaps implementations at composition time, not at
/// call sites.
#[derive(Clone)]
pub struct Ports {
    pub scheduler: Arc<dyn AdmissionControl>,
    pub worktrees: Arc<dyn WorktreeGate>,
    pub delivery: Arc<dyn DeliveryQueue>,
    pub sessions: Arc<dyn SessionControl>,
    pub feed: Arc<dyn EventFeed>,
}

/// The service core.
///
/// It owns the state machines for the same reason the machines own the store:
/// a second path to a transition is a path around a guard. A facade holds a
/// principal and this core, and has nothing else to hold.
pub struct ApiCore {
    machines: StateMachines,
    ports: Ports,
    /// What the operator decided about the repositories this daemon serves.
    ///
    /// Held beside the ports rather than among them because it performs no
    /// effect: it is the data an authorization decision is made from, and the
    /// decision itself stays here.
    policy: Arc<dyn RepositoryPolicy>,
}

impl ApiCore {
    #[must_use]
    pub fn new(machines: StateMachines, ports: Ports, policy: Arc<dyn RepositoryPolicy>) -> Self {
        Self {
            machines,
            ports,
            policy,
        }
    }

    #[must_use]
    pub fn policy(&self) -> &Arc<dyn RepositoryPolicy> {
        &self.policy
    }

    /// The state machines, for the coordinator's own components. Facades reach
    /// the control plane through the operations, never through this.
    #[must_use]
    pub const fn machines(&self) -> &StateMachines {
        &self.machines
    }

    /// The state machines, for the coordinator's own writers inside this
    /// crate: boot reconciliation, the gates and the introspection surface. The
    /// core is their single owner, so each of them borrows through here rather
    /// than holding a second handle on the same tables.
    ///
    /// Crate-private on purpose. A caller outside the daemon reaching the
    /// machines directly would be a second path to a transition, which is a
    /// path around the authorization table this core exists to run. What such a
    /// caller needs instead is a named operation below.
    #[must_use]
    pub(crate) const fn machines_mut(&mut self) -> &mut StateMachines {
        &mut self.machines
    }

    /// One scheduler tick: offers every eligible queued run a slot and moves
    /// the ones that were launched.
    ///
    /// A named operation rather than a borrow of the machines because the
    /// admission loop is a writer of the control plane, and every writer the
    /// core has is one the core can name.
    pub fn admit_queued_runs(
        &mut self,
        scheduler: &Scheduler,
        launcher: &dyn RunLauncher,
        load: &SchedulerLoad,
    ) -> Result<QueueReport, SchedulerError> {
        scheduler.tick(&mut self.machines, launcher, load)
    }

    /// One turn of the timer wheel: every deadline that came due, applied and
    /// journaled.
    ///
    /// The wheel raises signals and reports to nobody, so what it found is
    /// returned for the caller to carry. Infallible by construction: the
    /// wheel's three stages are refused independently, and a refusal comes back
    /// journaled in [`TimerTick::rejections`] rather than as an error that
    /// would speak for the stages that ran.
    pub fn advance_timers(&mut self, wheel: &TimerWheel) -> TimerTick {
        wheel.tick(&mut self.machines)
    }

    /// One of a run's own lifecycle transitions, as reported by the harness
    /// executing it.
    ///
    /// The principal is pinned to the coordinator here and read from nothing:
    /// this is what the run machine's `reported_by_harness` guard admits, and a
    /// caller that could name its own principal would be able to claim a run's
    /// lifecycle for a party that is not executing it.
    pub fn report_run_lifecycle(
        &mut self,
        run_id: i64,
        trigger: RunTrigger,
        facts: &RunFacts,
    ) -> Result<TransitionOutcome<RunState, RunEffect>, TransitionRejection> {
        self.machines.apply_run(
            run_id,
            trigger,
            &RunFacts {
                principal: Principal::Coordinator,
                ..facts.clone()
            },
        )
    }

    /// Records why a turn ended without a completion.
    ///
    /// Not a transition: what the run does about a failed turn is decided by
    /// the trigger the harness reports next, and a turn that failed after
    /// something else had already moved the run moves nothing at all. What
    /// this writes is the cause, which the transition does not carry and which
    /// is otherwise only in the error the worker discards.
    pub fn journal_turn_failure(
        &mut self,
        run_id: i64,
        failure: &TurnFailure<'_>,
    ) -> Result<(), TransitionRejection> {
        let detail: String = failure
            .detail
            .chars()
            .take(TURN_FAILURE_DETAIL_MAX_CHARS)
            .collect();

        self.machines.journal(&[EventRow {
            id: None,
            run_id: Some(run_id),
            event_type: TURN_FAILED_EVENT.to_owned(),
            class: EventClass::Infra,
            payload: serde_json::json!({
                "category": failure.category,
                "detail": detail,
                "state": failure.state.map(RunState::as_str),
            })
            .to_string(),
            ts: failure.now,
        }])?;

        Ok(())
    }

    /// Records that a reported fact never reached ingest.
    ///
    /// The bounded queue turned "a fact is never lost" into "a fact is lost in
    /// silence": every reporter dropped the refusal, so a run whose evidence
    /// stopped arriving looked exactly like a run that had nothing to say. The
    /// entry is the run's own record that a fact existed and was not folded.
    ///
    /// Deduplication belongs to the caller, which is the party that knows
    /// whether the backlog it just met is the same one it already reported.
    pub fn journal_backlogged_fact(
        &mut self,
        reporter: &str,
        refused: &RefusedReport,
    ) -> Result<(), TransitionRejection> {
        self.machines
            .journal(&[backlogged_event(reporter, refused)])?;

        Ok(())
    }

    /// Names the physical execution one of a run's attempts is running as.
    ///
    /// Not a transition — the attempt stays where it is — but still a write of
    /// the control plane, so the harness reaches it by name rather than by
    /// borrowing the machines.
    pub fn correlate_attempt(
        &mut self,
        attempt_id: i64,
        session_attempt_id: i64,
    ) -> Result<(), TransitionRejection> {
        self.machines
            .correlate_attempt(attempt_id, session_attempt_id)
    }

    #[must_use]
    pub const fn ports(&self) -> &Ports {
        &self.ports
    }

    /// The table check every operation starts with.
    ///
    /// A refusal is journaled before it is returned. A rejection that is only
    /// returned to its caller cannot be diagnosed afterwards, and the caller is
    /// exactly the party with a reason not to mention it.
    fn authorize(
        &mut self,
        operation: Operation,
        principal: Principal,
        run_id: Option<i64>,
        now: i64,
    ) -> Result<(), ApiError> {
        if operation.admits(principal) {
            return Ok(());
        }

        Err(self.refuse(
            operation,
            principal,
            run_id,
            now,
            format!(
                "the operation admits {}",
                principals_of(operation).join(" and ")
            ),
        ))
    }

    /// Records a refusal and shapes it into the error the caller receives.
    fn refuse(
        &mut self,
        operation: Operation,
        principal: Principal,
        run_id: Option<i64>,
        now: i64,
        reason: String,
    ) -> ApiError {
        let payload = serde_json::json!({
            "operation": operation.as_str(),
            "principal": principal.as_str(),
            "reason": reason,
        });

        let journaled = self
            .machines
            .journal(&[EventRow {
                id: None,
                run_id,
                event_type: "authorization_denied".to_owned(),
                class: EventClass::Infra,
                payload: payload.to_string(),
                ts: now,
            }])
            .is_ok();

        ApiError::Unauthorized {
            operation,
            principal,
            reason,
            journaled,
        }
    }
}

fn principals_of(operation: Operation) -> Vec<&'static str> {
    operation
        .principals()
        .iter()
        .map(|principal| principal.as_str())
        .collect()
}
