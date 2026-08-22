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

use crate::fsm::{Principal, StateMachines, TransitionRejection};

pub use authorization::{
    DetailQuestionRefusal, OPERATION_AUTHORIZATION, Operation, OperationAuthorization,
    praetor_may_answer,
};
pub use feed::{InboxItem, InboxView, RunSummary, RunView, TreeSnapshot};
pub use ports::{
    Delivery, DeliveryGrain, DeliveryPayload, DeliveryQueue, EventFeed, EventFilter, PortError,
    ProvisionedWorktree, RepositoryIdentity, SchedulerPort, SessionControl, StopScope,
    Subscription, TakeoverHandle, WorktreeDerivation, WorktreeGate, WorktreeRequest,
};
pub use runs::{CreateRun, CreatedRun};
pub use team::{
    AdmissionState, AnswerQuestion, AnsweredQuestion, ApprovePlan, AuthorizeMerge, CleaningAction,
    CleaningDisposition, RetryRequest, RunRef, StopRequest,
};

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
    pub scheduler: Arc<dyn SchedulerPort>,
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
}

impl ApiCore {
    #[must_use]
    pub const fn new(machines: StateMachines, ports: Ports) -> Self {
        Self { machines, ports }
    }

    /// The state machines, for the coordinator's own components. Facades reach
    /// the control plane through the operations, never through this.
    #[must_use]
    pub const fn machines(&self) -> &StateMachines {
        &self.machines
    }

    /// The state machines, for the coordinator's own writers: the scheduler's
    /// tick, the timer wheel, the gates and the worker's introspection. The
    /// core is their single owner, so each of them borrows through here rather
    /// than holding a second handle on the same tables.
    #[must_use]
    pub const fn machines_mut(&mut self) -> &mut StateMachines {
        &mut self.machines
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
