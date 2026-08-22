//! The Agens daemon process.
//!
//! One daemon per machine serves N projects (AGN-80), so nothing here resolves a
//! project: a project only enters through a run. The crate exists apart from the
//! CLI on purpose — the daemon owns the coordinator, its state machines, the
//! scheduler and the timers, and none of that belongs to a command surface.

mod api;
mod blocking;
mod fsm;
mod gates;
mod ingest;
mod instance;
mod introspection;
mod scheduler;
mod sessions;
mod timers;

use std::os::unix::net::UnixListener;
use std::path::Path;

use agens_core::HeadlessTurnCancellation;

pub use api::{
    AdmissionState, AnswerQuestion, AnsweredQuestion, ApiCore, ApiError, ApprovePlan,
    AuthorizeMerge, CleaningAction, CleaningDisposition, Delivery, DeliveryGrain, DeliveryPayload,
    DeliveryQueue, DetailQuestionRefusal, EventFeed, EventFilter, InboxItem, InboxView,
    OPERATION_AUTHORIZATION, Operation, OperationAuthorization, PortError, Ports, RetryRequest,
    RunRef, RunSummary, RunView, SchedulerPort, SessionControl, StopRequest, StopScope,
    Subscription, TakeoverHandle, TreeSnapshot, WorktreeDerivation, WorktreeGate,
    praetor_may_answer,
};
pub use blocking::{BlockingBoundary, BlockingError};
pub use fsm::{
    AppliedQuestionTransition, AppliedRunTransition, AppliedTransition, AppliedWorktreeTransition,
    Principal, QUESTION_TRANSITIONS, QuestionEffect, QuestionFacts, QuestionGuard,
    QuestionTransition, QuestionTrigger, RUN_TRANSITIONS, RunEffect, RunFacts, RunGuard,
    RunTransition, RunTrigger, StateMachines, TransitionOutcome, TransitionRejection,
    WORKTREE_TRANSITIONS, WorktreeEffect, WorktreeFacts, WorktreeGuard, WorktreeTransition,
    WorktreeTrigger,
};
pub use gates::{
    GateError, GateRefusal, Gates, MergePath, PreMergeRequest, PreMergeVerdict, Receipt,
    ReclaimRequest, ReclaimVerdict, SubAgentKind, SubAgentRequest, freeze_receipt,
};
pub use ingest::{
    AcceptedFact, CheckpointClaim, CheckpointStanding, DrainedFact, FactReceiver, FactSender,
    HealthSignal, HealthThresholds, Ingest, IngestFact, IngestRejection, LostReason,
    ReportedCheckpoint, ReportedFact, channel as ingest_channel, detect_worker_lost,
};
pub use instance::{ServeInstance, ServeInstanceError};
pub use introspection::{CHECKPOINT_EVENT, Clock, RunIntrospection};
pub use scheduler::{
    Admission, AdmissionFailure, Candidate, Deferral, Ineligible, LaunchError, LaunchedSession,
    PendingRun, Queue, QueueReport, RunLauncher, RunSession, Scheduler, SchedulerError,
    SchedulerLimits, SchedulerLoad, SupervisorLauncher,
};
pub use sessions::{
    SessionAdmission, SessionBudget, SessionBudgetHandle, SessionId, SessionLimits, SessionOutcome,
    SessionProvider, SessionRegistry, SessionRegistryError, SessionRuntime, SessionShutdown,
    SessionState, SessionStatus, SessionSupervisor,
};
pub use timers::{
    CHECKPOINT_EVENT, CHECKPOINT_OVERDUE_EVENT, DEFAULT_CHECKPOINT_GRACE_PERCENT, ExpiredQuestion,
    ManualTimerClock, OverdueCheckpoint, QuotaReset, TimerSettings, TimerTick, TimerWheel,
};

#[derive(Debug)]
pub enum ServerError {
    /// Another daemon owns this machine's slot. Its own variant because the
    /// caller must attach rather than start a second process.
    AlreadyRunning,
    Unavailable(&'static str),
}

/// The machine's daemon: its slot, its socket, its runtime, and the sessions
/// living in it.
///
/// Field order is drop order: the runtime stops the sessions' work, the socket
/// closes, and only then does the instance release the slot and remove the
/// socket file a client could still be looking at.
///
/// Nothing admits a session into this daemon yet. The wire facade lands in
/// AGN-63/64, and when it does, the one place it belongs is between accepting a
/// client and [`SessionSupervisor::start`]: a session admitted there must be
/// given its OWN per-session state — its own provider client, which
/// [`SessionAdmission`] already enforces by ownership, and its own MCP
/// connections, which `agens-bootstrap` exposes as `Bootstrap::for_new_session`.
/// Handing a peer a bootstrap CLONE instead would put every session's MCP
/// servers behind one lock and let one session's close reach another's.
pub struct Daemon {
    runtime: tokio::runtime::Runtime,
    sessions: SessionSupervisor,
    /// Held for its binding, not for reading: the daemon owns its address for
    /// the life of the process, and nothing accepts on it until the client
    /// protocol lands.
    #[allow(dead_code)]
    listener: UnixListener,
    instance: ServeInstance,
}

impl Daemon {
    /// Takes the machine's daemon slot and binds its socket, leaving the process
    /// ready to hold sessions.
    pub fn start(data_directory: &Path) -> Result<Self, ServerError> {
        let instance = ServeInstance::acquire(data_directory).map_err(|error| match error {
            ServeInstanceError::AlreadyRunning => ServerError::AlreadyRunning,
            ServeInstanceError::Unavailable(_) => {
                ServerError::Unavailable("runtime is unavailable")
            }
        })?;

        let listener = UnixListener::bind(instance.socket_path())
            .map_err(|_| ServerError::Unavailable("socket is unavailable"))?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .build()
            .map_err(|_| ServerError::Unavailable("runtime is unavailable"))?;
        let sessions = SessionSupervisor::new(runtime.handle().clone());

        Ok(Self {
            runtime,
            sessions,
            listener,
            instance,
        })
    }

    /// Where a client attaches to this daemon.
    pub fn socket_path(&self) -> &Path {
        self.instance.socket_path()
    }

    /// The daemon's sessions. Cloneable, so a client surface holds the same
    /// registry the daemon runs against rather than a copy of its contents.
    pub fn sessions(&self) -> &SessionSupervisor {
        &self.sessions
    }

    /// Parks until asked to stop, then stops every session before releasing the
    /// slot and the socket, reporting any session that outlived the wait.
    ///
    /// Takes the daemon by value so the runtime is shut down explicitly rather
    /// than dropped: dropping a runtime waits for its blocking tasks, and a
    /// session that already ignored its cancellation would put the unbounded
    /// wait straight back at the end of a shutdown that just bounded it.
    pub fn run_until_shutdown(self, shutdown: &HeadlessTurnCancellation) -> SessionShutdown {
        let Self {
            runtime,
            sessions,
            listener,
            instance,
        } = self;

        let report = runtime.block_on(async {
            park_until_shutdown(shutdown).await;
            sessions.cancel_all_and_join().await
        });

        // Explicit, in the order the field declarations describe: the sessions'
        // work stops first, the socket closes next, and the slot is the last
        // thing released so no client can find a socket with no owner behind it.
        runtime.shutdown_timeout(std::time::Duration::ZERO);
        drop(sessions);
        drop(listener);
        drop(instance);

        report
    }
}

/// Takes the machine's daemon slot, binds its socket and parks until asked to
/// stop, releasing both on the way out.
pub fn run_until_shutdown(
    data_directory: &Path,
    shutdown: &HeadlessTurnCancellation,
) -> Result<SessionShutdown, ServerError> {
    Ok(Daemon::start(data_directory)?.run_until_shutdown(shutdown))
}

/// The daemon has no admission surface of its own yet, so it parks on the shared
/// cancellation rather than inventing a second stop path.
async fn park_until_shutdown(shutdown: &HeadlessTurnCancellation) {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

    while !shutdown.is_cancelled() {
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
