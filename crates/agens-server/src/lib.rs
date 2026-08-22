//! The Agens daemon process.
//!
//! One daemon per machine serves N projects (AGN-80), so nothing here resolves a
//! project: a project only enters through a run. The crate exists apart from the
//! CLI on purpose — the daemon owns the coordinator, its state machines, the
//! scheduler and the timers, and none of that belongs to a command surface.

mod api;
mod blocking;
mod coordinator;
mod fsm;
mod gates;
pub mod grpc;
mod ingest;
mod instance;
mod introspection;
mod ports;
mod scheduler;
mod sessions;
mod timers;

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_core::HeadlessTurnCancellation;

pub use api::{
    AdmissionState, AnswerQuestion, AnsweredQuestion, ApiCore, ApiError, ApprovePlan,
    AuthorizeMerge, CleaningAction, CleaningDisposition, CreateRun, CreatedRun, Delivery,
    DeliveryGrain, DeliveryPayload, DeliveryQueue, DetailQuestionRefusal, EventFeed, EventFilter,
    InboxItem, InboxView, OPERATION_AUTHORIZATION, Operation, OperationAuthorization, PortError,
    Ports, ProvisionedWorktree, RepositoryIdentity, RetryRequest, RunRef, RunSummary, RunView,
    SchedulerPort, SessionControl, StopRequest, StopScope, Subscription, TakeoverHandle,
    TreeSnapshot, WorktreeDerivation, WorktreeGate, WorktreeRequest, praetor_may_answer,
};
pub use blocking::{BlockingBoundary, BlockingError};
pub use coordinator::{
    Coordinator, CoordinatorError, CoordinatorSettings, RunLaunch, RunWorkerFactory,
};
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
pub use grpc::{CoreHandle, FacadeBinding, FacadeError, FeedFacade, TeamFacade};
pub use ingest::{
    AcceptedFact, CheckpointClaim, CheckpointStanding, DrainedFact, FactReceiver, FactSender,
    HealthSignal, HealthThresholds, Ingest, IngestFact, IngestRejection, LostReason,
    ReportedCheckpoint, ReportedFact, channel as ingest_channel, detect_worker_lost,
};
pub use instance::{ServeInstance, ServeInstanceError, socket_path};
pub use introspection::{Clock, RunIntrospection};
pub use ports::{
    Admissions, GitWorktreeGate, JournalFeed, RunDeliveries, SupervisedSessions, run_mailbox,
};
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
/// Nothing admits a session into this daemon yet. When something does, the one
/// place it belongs is between accepting a client and
/// [`SessionSupervisor::start`]: a session admitted there must be given its OWN
/// per-session state — its own provider client, which [`SessionAdmission`]
/// already enforces by ownership, and its own MCP connections, which
/// `agens-bootstrap` exposes as `Bootstrap::for_new_session`. Handing a peer a
/// bootstrap CLONE instead would put every session's MCP servers behind one
/// lock and let one session's close reach another's.
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
            ServeInstanceError::Unavailable(_) => {
                ServerError::Unavailable("runtime is unavailable")
            }
        })?;

        let listener = UnixListener::bind(instance.socket_path())
            .map_err(|_| ServerError::Unavailable("socket is unavailable"))?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            // The facade accepts on this runtime, and a socket without the IO
            // driver does not fail to bind — it panics on the first accept.
            .enable_io()
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

    /// Serves the clients' facade until asked to stop, then stops every session
    /// before releasing the slot and the socket.
    ///
    /// The facade always accepts on the daemon's unix socket, and on loopback
    /// as well when a port is named. Both are local by construction: the facade
    /// authenticates nobody, so remote access is an SSH tunnel rather than a
    /// listener anything else can route to.
    ///
    /// The core arrives from the composition root rather than being built here.
    /// It is the coordinator's one core — the scheduler, the gates, the
    /// safe-point queue and the event fan-out all reach it — and a daemon that
    /// built its own would be a second one.
    pub fn serve_until_shutdown(
        self,
        core: Arc<Mutex<ApiCore>>,
        localhost_port: Option<u16>,
        shutdown: &HeadlessTurnCancellation,
    ) -> Result<SessionShutdown, ServerError> {
        let Self {
            runtime,
            sessions,
            listener,
            instance,
        } = self;

        let mut binding = FacadeBinding::none().on_unix_socket(listener);

        if let Some(port) = localhost_port {
            binding = binding
                .bind_localhost(port)
                .map_err(|_| ServerError::Unavailable("loopback is unavailable"))?;
        }

        let blocking = BlockingBoundary::new(runtime.handle().clone());

        let report = runtime.block_on(async {
            let served = grpc::serve_until_shutdown(core, blocking, binding, shutdown).await;
            let report = sessions.cancel_all_and_join().await;

            (served, report)
        });

        runtime.shutdown_timeout(std::time::Duration::ZERO);
        drop(sessions);
        drop(instance);

        let (served, report) = report;
        served.map_err(|_| ServerError::Unavailable("the facade is unavailable"))?;

        Ok(report)
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
    localhost_port: Option<u16>,
    shutdown: &HeadlessTurnCancellation,
) -> Result<SessionShutdown, ServerError> {
    let daemon = Daemon::start(data_directory)?;

    // The supervisor is the daemon's, so the sessions the scheduler starts are
    // the ones the daemon stops on its way out.
    let coordinator =
        Coordinator::start(data_directory, settings, daemon.sessions().clone(), worker)
            .map_err(|_| ServerError::Unavailable("the coordinator is unavailable"))?;

    let report = daemon.serve_until_shutdown(coordinator.core(), localhost_port, shutdown);

    // After the facade has stopped: nothing is admitting, ticking or publishing
    // against a core the sessions behind it have already been stopped.
    coordinator.stop();

    report
}

/// The daemon has no admission surface of its own yet, so it parks on the shared
/// cancellation rather than inventing a second stop path.
async fn park_until_shutdown(shutdown: &HeadlessTurnCancellation) {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

    while !shutdown.is_cancelled() {
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
