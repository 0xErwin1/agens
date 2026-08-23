//! The Agens daemon process.
//!
//! One daemon per machine serves N projects (AGN-80), so nothing here resolves a
//! project: a project only enters through a run. The crate exists apart from the
//! CLI on purpose — the daemon owns the coordinator, its state machines, the
//! scheduler and the timers, and none of that belongs to a command surface.

mod api;
mod blocking;
mod cache;
mod chat;
mod coordinator;
mod diagnostics;
mod fsm;
mod gates;
pub mod grpc;
mod ingest;
mod instance;
mod introspection;
mod policy;
mod ports;
mod scheduler;
mod sessions;
mod timers;

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_core::HeadlessTurnCancellation;

/// What this crate is, as one list.
///
/// Everything below is re-exported out of a private module, so this block is
/// the whole of the daemon's surface and the modules stay free to move behind
/// it. Three kinds of name are in it, and they are not interchangeable:
///
/// - **The composition root.** [`serve_until_shutdown`], [`Daemon`],
///   [`Coordinator`] and [`CoordinatorSettings`]: what a command surface needs
///   to run a daemon, and nothing about how one is built.
/// - **The worker's contract.** [`ApiCore`]'s named operations, [`RunLaunch`],
///   [`RunIntrospection`], [`FactSender`] and the types they carry. A harness
///   executing a run reaches the control plane through exactly these.
/// - **The tables and their vocabulary.** [`RUN_TRANSITIONS`] and the guards,
///   triggers and effects it is written in. Public because a machine written as
///   data is a machine something else can read, which is what the tests that
///   assert the tables do.
///
/// A name reachable from none of the three is not surface: it is an internal
/// that a flat re-export happened to carry, and it belongs behind the module it
/// came from.
pub use api::{
    AdmissionControl, AdmissionState, AnswerQuestion, AnsweredQuestion, ApiCore, ApiError,
    ApprovePlan, AuthorizeMerge, CleaningAction, CleaningDisposition, CreateRun, CreatedRun,
    Delivery, DeliveryGrain, DeliveryPayload, DeliveryQueue, DetailQuestionRefusal, EventFeed,
    EventFilter, HookPolicy, InboxItem, InboxView, MergeAuthorization, MergeAuthorized,
    OPERATION_AUTHORIZATION, Operation, OperationAuthorization, PortError, Ports, PreparedRun,
    ProvisionedWorktree, RepositoryIdentity, RetryRequest, RunRef, RunSummary, RunView,
    SessionControl, StopRequest, StopScope, Subscription, TURN_FAILED_EVENT, TakeoverHandle,
    TreeSnapshot, TurnFailure, WorktreeDerivation, WorktreeGate, WorktreeRequest,
    praetor_may_answer,
};
pub use blocking::{BlockingBoundary, BlockingError};
pub use chat::{
    ChatError, ChatEvent, ChatSession, ChatSessionFactory, ChatSessionRequest, ChatSessions,
    ChatTurnOutcome, ChatTurns, OpenChatSummary,
};
pub use coordinator::{
    ADMISSION_FAILED_EVENT, BootReconciliation, CORE_POISONED_EVENT, Coordinator, CoordinatorError,
    CoordinatorSettings, MissingWorktree, OrphanWorktree, RUN_DEFERRED_EVENT, RunLaunch,
    RunWorkerFactory, WORKTREE_MISSING_EVENT, WORKTREE_ORPHANED_EVENT,
};
pub use diagnostics::CoordinatorDiagnostics;
pub use fsm::{
    AppliedQuestionTransition, AppliedRunTransition, AppliedTransition, AppliedWorktreeTransition,
    MergeSettlement, Principal, QUESTION_TRANSITIONS, QuestionEffect, QuestionFacts, QuestionGuard,
    QuestionTransition, QuestionTrigger, RUN_TRANSITIONS, RunEffect, RunFacts, RunGuard,
    RunTransition, RunTrigger, SettledMerge, StateMachines, TransitionOutcome, TransitionRejection,
    WORKTREE_HOLDING_RUN_STATES, WORKTREE_TRANSITIONS, WorktreeEffect, WorktreeFacts,
    WorktreeGuard, WorktreeTransition, WorktreeTrigger,
};
pub use gates::{
    DisposeVerdict, GateError, GateRefusal, Gates, MergePath, PreMergeRequest, PreMergeVerdict,
    Receipt, ReclaimRequest, ReclaimVerdict, SubAgentKind, SubAgentRequest, freeze_receipt,
};
pub use grpc::{ChatFacade, CoreHandle, FacadeBinding, FacadeError, FeedFacade, TeamFacade};
pub use ingest::{
    AcceptedFact, Attribution, BACKLOGGED_EVENT, BacklogNotice, CheckpointClaim,
    CheckpointStanding, DrainedFact, FactReceiver, FactSender, HealthSignal, HealthThresholds,
    Ingest, IngestFact, IngestRejection, LostReason, RefusedReport, ReportedCheckpoint,
    ReportedFact, backlogged_event, channel as ingest_channel, channel_with_backlog,
};
pub use instance::{
    ServeInstance, ServeInstanceError, log_path, pid_path, slot_is_held, socket_path,
};
pub use introspection::{AttemptResolver, CheckpointReporting, Clock, RunIntrospection};
pub use policy::{
    HookTrust, PendingHookTrust, PolicyError, PolicySettings, PolicyStore, RepositoryPolicy,
    TrustReadFailure, TrustedRepository, trust_repository,
};
pub use ports::GitWorktreeGate;
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
    CHECKPOINT_EVENT, CHECKPOINT_OVERDUE_EVENT, DEFAULT_CHECKPOINT_GRACE_PERCENT,
    DEFAULT_FIRST_CHECKPOINT_SECONDS, DEFAULT_QUOTA_WINDOW_SECONDS, ExpiredQuestion,
    ManualTimerClock, OverdueCheckpoint, QuotaReset, RejectedStage, TIMER_STAGE_REJECTED_EVENT,
    TimerSettings, TimerStage, TimerTick, TimerWheel,
};

#[derive(Debug)]
pub enum ServerError {
    /// Another daemon owns this machine's slot. Its own variant because the
    /// caller must attach rather than start a second process.
    AlreadyRunning,
    /// Something the daemon is built from could not be opened, carrying what
    /// said so.
    ///
    /// The cause travels rather than being replaced by a fixed phrase: the
    /// component that failed and the reason it gave are the whole of what an
    /// operator can act on, and a daemon that refuses to start is exactly the
    /// moment there is no journal, no facade and no diagnostics file to read it
    /// from instead.
    Unavailable(String),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning => {
                formatter.write_str("a daemon already holds this machine's slot")
            }
            Self::Unavailable(cause) => formatter.write_str(cause),
        }
    }
}

impl std::error::Error for ServerError {}

/// The machine's daemon: its slot, its socket, its runtime, and the sessions
/// living in it.
///
/// Field order is drop order: the runtime stops the sessions' work, the socket
/// closes, and only then does the instance release the slot and remove the
/// socket file a client could still be looking at.
///
/// The scheduler's admission tick is what admits a session into this daemon,
/// through the supervisor the daemon owns rather than one of its own. Every
/// session it starts is given its OWN per-session state: its own provider
/// client, which [`SessionAdmission`] enforces by ownership, and its own MCP
/// connections, which the worker takes from `Bootstrap::for_new_session`.
/// Handing a peer a bootstrap CLONE instead would put every session's MCP
/// servers behind one lock and let one session's close reach another's.
pub struct Daemon {
    runtime: tokio::runtime::Runtime,
    sessions: SessionSupervisor,
    /// The daemon's address for the life of the process. Clients are accepted
    /// on it by [`Daemon::serve_until_shutdown`]; [`Daemon::run_until_shutdown`]
    /// holds it bound without accepting.
    listener: UnixListener,
    instance: ServeInstance,
}

impl Daemon {
    /// Takes the machine's daemon slot and binds its socket, leaving the process
    /// ready to hold sessions.
    pub fn start(data_directory: &Path) -> Result<Self, ServerError> {
        let instance = ServeInstance::acquire(data_directory).map_err(|error| match error {
            ServeInstanceError::AlreadyRunning => ServerError::AlreadyRunning,
            ServeInstanceError::Unavailable(cause) => {
                ServerError::Unavailable(format!("the daemon's slot is unavailable: {cause}"))
            }
        })?;

        let listener = UnixListener::bind(instance.socket_path()).map_err(|error| {
            ServerError::Unavailable(format!("the socket is unavailable: {error}"))
        })?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            // The facade accepts on this runtime, and a socket without the IO
            // driver does not fail to bind — it panics on the first accept.
            .enable_io()
            .build()
            .map_err(|error| {
                ServerError::Unavailable(format!("the runtime is unavailable: {error}"))
            })?;
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

    /// Serves the clients' facade until asked to stop, then stops every session
    /// before releasing the slot and the socket.
    ///
    /// The facade accepts on the daemon's unix socket and nowhere else: it
    /// authenticates nobody, so the only address it may carry the user's
    /// authority on is one whose reach something outside it already decides.
    /// Remote access is an SSH tunnel rather than a second listener.
    ///
    /// The core arrives from the composition root rather than being built here.
    /// It is the coordinator's one core — the scheduler, the gates, the
    /// safe-point queue and the event fan-out all reach it — and a daemon that
    /// built its own would be a second one.
    pub fn serve_until_shutdown(
        self,
        core: Arc<Mutex<ApiCore>>,
        chats: Arc<ChatSessions>,
        shutdown: &HeadlessTurnCancellation,
    ) -> Result<SessionShutdown, ServerError> {
        let Self {
            runtime,
            sessions,
            listener,
            instance,
        } = self;

        // Everything this daemon serves with is built by now — the core arrived
        // composed — so this is the first moment the pid is true. A start
        // waiting on it gets back a daemon whose control plane is already open,
        // rather than a socket that answers `connect` because it is bound.
        publish_pid(&instance)?;

        let binding = FacadeBinding::none().on_unix_socket(listener);
        let blocking = BlockingBoundary::new(runtime.handle().clone());

        let report = runtime.block_on(async {
            let served = grpc::serve_until_shutdown(core, chats, blocking, binding, shutdown).await;
            let report = sessions.cancel_all_and_join().await;

            (served, report)
        });

        runtime.shutdown_timeout(std::time::Duration::ZERO);
        drop(sessions);
        drop(instance);

        let (served, report) = report;
        served.map_err(|error| ServerError::Unavailable(format!("the facade stopped: {error}")))?;

        Ok(report)
    }

    /// Parks until asked to stop, then stops every session before releasing the
    /// slot and the socket, reporting any session that outlived the wait.
    ///
    /// It publishes no pid, because it answers nothing: it holds the slot and
    /// the socket without accepting on either. The pid file says a daemon is
    /// serving, and a process that never serves must not be found under it.
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

/// Takes the machine's daemon slot, composes the coordinator over its data
/// directory and serves the clients' facade until asked to stop.
///
/// This is the whole of what a command surface has to do to run a daemon: the
/// pieces it is made of, and the order they are given each other in, belong to
/// the composition root rather than to the caller.
pub fn serve_until_shutdown(
    data_directory: &Path,
    settings: &CoordinatorSettings,
    worker: RunWorkerFactory,
    chat: ChatSessionFactory,
    shutdown: &HeadlessTurnCancellation,
) -> Result<SessionShutdown, ServerError> {
    let daemon = Daemon::start(data_directory)?;

    // The supervisor is the daemon's, so the sessions the scheduler starts are
    // the ones the daemon stops on its way out.
    let coordinator = Coordinator::start(
        data_directory,
        settings,
        daemon.sessions().clone(),
        worker,
        shutdown,
    )
    .map_err(|error| ServerError::Unavailable(error.to_string()))?;

    // The same supervisor the scheduler launches into, so a hosted chat is a
    // peer of the runs rather than a session the daemon does not know it has:
    // one capacity, one shutdown, one drain.
    let chats = Arc::new(ChatSessions::new(daemon.sessions().clone(), chat));

    let report = daemon.serve_until_shutdown(coordinator.core(), chats, shutdown);

    // After the facade has stopped: nothing is admitting, ticking or publishing
    // against a core the sessions behind it have already been stopped.
    let poisoned = coordinator.stop();

    // A daemon that came down on a poisoned core did not stop cleanly, and the
    // exit status is the only part of that a process supervisor reads. The
    // socket and the machine's slot are already released by the line above, so
    // what this refuses is the report, not the shutdown.
    if poisoned {
        return Err(ServerError::Unavailable(
            "the service core was left poisoned and the daemon stopped".to_owned(),
        ));
    }

    report
}

/// Says the daemon is serving, in the one place that is true.
fn publish_pid(instance: &ServeInstance) -> Result<(), ServerError> {
    instance.publish_pid().map_err(|error| match error {
        ServeInstanceError::AlreadyRunning => ServerError::AlreadyRunning,
        ServeInstanceError::Unavailable(cause) => {
            ServerError::Unavailable(format!("the daemon's pid is unavailable: {cause}"))
        }
    })
}

/// Parks on the shared cancellation rather than inventing a second stop path:
/// the daemon is stopped from outside, by the same flag every other loop in the
/// process is watching.
async fn park_until_shutdown(shutdown: &HeadlessTurnCancellation) {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

    while !shutdown.is_cancelled() {
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
