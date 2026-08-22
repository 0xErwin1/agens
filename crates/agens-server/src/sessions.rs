//! The daemon's registry of live sessions.
//!
//! A session here is a peer, not a child: it holds its own provider client, its
//! own budget and its own cancellation, and nothing it does reaches another one.
//! That is the whole point of the registry — the existing parent/child shape
//! (`agens-tools`' task registry) cancels a child by cancelling the turn that
//! spawned it, which is exactly what N independent sessions must not do.
//!
//! Sessions are keyed by their durable session id, the same one
//! `agens direct --session <id>` writes against and the one a turn publishes in
//! its `turn_started` event. A registry keyed by anything else would leave a
//! live session addressable by the daemon and by nobody else.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use agens_core::HeadlessTurnCancellation;
use tokio::{
    runtime::Handle,
    task::JoinHandle,
    time::{Instant, timeout},
};

use crate::blocking::BlockingBoundary;

/// How many sessions one daemon keeps alive at once.
///
/// A ceiling rather than a queue: the scheduler that decides what waits and in
/// which order is a separate piece, and admitting without bound would let a
/// caller exhaust the machine before that piece exists.
const DEFAULT_MAX_LIVE_SESSIONS: usize = 8;

/// How many finished sessions the registry keeps so a supervisor can still read
/// how they ended.
///
/// Bounded rather than unlimited: a finished session is only useful until
/// somebody has read its outcome, and a daemon that runs for weeks would
/// otherwise hold every session it ever ran.
const DEFAULT_RETAINED_FINISHED_SESSIONS: usize = 32;

/// How long shutdown waits for the sessions it just cancelled.
///
/// A session can be inside work that does not observe cancellation — a child
/// process with no timeout of its own, a provider call mid-flight — and the
/// daemon still has to exit. The wait is generous enough that an ordinary
/// session ends inside it and short enough that a stuck one does not hold the
/// process open forever.
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// The durable identity of a session, as stored by `agens-store`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(i64);

impl SessionId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// The provider client one session speaks through.
///
/// A port rather than a concrete client: this crate depends on `agens-core`
/// alone, and what the daemon has to guarantee is not which provider a session
/// picked but that no two sessions share one. Ownership says it — the registry
/// hands the client to the session that was admitted with it and keeps no copy.
pub trait SessionProvider: Send {
    /// The model this client speaks to, for the registry's own listings.
    fn model(&self) -> &str;
}

/// What one session may spend before it has to be admitted again.
///
/// Per session and never shared: two sessions running the same model still
/// exhaust their own allowance, so one worker cannot spend a peer's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionBudget {
    max_turns: Option<u64>,
    spent_turns: u64,
    max_iterations_per_turn: Option<usize>,
}

impl SessionBudget {
    pub const fn unlimited() -> Self {
        Self {
            max_turns: None,
            spent_turns: 0,
            max_iterations_per_turn: None,
        }
    }

    pub const fn with_max_turns(max_turns: u64) -> Self {
        Self {
            max_turns: Some(max_turns),
            spent_turns: 0,
            max_iterations_per_turn: None,
        }
    }

    pub const fn with_max_iterations_per_turn(mut self, max_iterations: usize) -> Self {
        self.max_iterations_per_turn = Some(max_iterations);
        self
    }

    /// The per-turn iteration cap this session runs its turns under.
    pub const fn max_iterations_per_turn(&self) -> Option<usize> {
        self.max_iterations_per_turn
    }

    pub const fn spent_turns(&self) -> u64 {
        self.spent_turns
    }

    /// `None` when the session is unbounded.
    pub const fn remaining_turns(&self) -> Option<u64> {
        match self.max_turns {
            Some(max_turns) => Some(max_turns.saturating_sub(self.spent_turns)),
            None => None,
        }
    }

    /// Takes one turn from the allowance, reporting whether the session may run
    /// it. A refused turn spends nothing, so an exhausted session stays exactly
    /// as exhausted as it was.
    pub fn consume_turn(&mut self) -> bool {
        if self.remaining_turns() == Some(0) {
            return false;
        }

        self.spent_turns = self.spent_turns.saturating_add(1);
        true
    }
}

/// A session's own budget, readable by the registry while the session spends it.
#[derive(Clone, Debug)]
pub struct SessionBudgetHandle {
    budget: Arc<Mutex<SessionBudget>>,
}

impl SessionBudgetHandle {
    fn new(budget: SessionBudget) -> Self {
        Self {
            budget: Arc::new(Mutex::new(budget)),
        }
    }

    pub fn snapshot(&self) -> SessionBudget {
        *self.locked()
    }

    /// See [`SessionBudget::consume_turn`].
    pub fn consume_turn(&self) -> bool {
        self.locked().consume_turn()
    }

    /// A poisoned budget lock is recovered rather than propagated: the daemon
    /// serves other sessions through this same registry, and a panic under one
    /// session's lock must not make every later budget read fail.
    fn locked(&self) -> std::sync::MutexGuard<'_, SessionBudget> {
        self.budget
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Everything a session needs before the daemon will admit it.
pub struct SessionAdmission {
    session: SessionId,
    provider: Box<dyn SessionProvider>,
    budget: SessionBudget,
}

impl SessionAdmission {
    pub fn new(
        session: SessionId,
        provider: Box<dyn SessionProvider>,
        budget: SessionBudget,
    ) -> Self {
        Self {
            session,
            provider,
            budget,
        }
    }

    pub const fn session(&self) -> SessionId {
        self.session
    }
}

/// What an admitted session runs with.
///
/// Handed to the work once, by value: the provider client belongs to this
/// session for as long as it lives, and drops with it.
pub struct SessionRuntime {
    session: SessionId,
    provider: Box<dyn SessionProvider>,
    budget: SessionBudgetHandle,
    cancellation: HeadlessTurnCancellation,
}

impl SessionRuntime {
    pub const fn session(&self) -> SessionId {
        self.session
    }

    pub fn provider(&self) -> &dyn SessionProvider {
        self.provider.as_ref()
    }

    pub const fn budget(&self) -> &SessionBudgetHandle {
        &self.budget
    }

    /// This session's own cancellation, and no peer's.
    pub const fn cancellation(&self) -> &HeadlessTurnCancellation {
        &self.cancellation
    }
}

/// How a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl SessionOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Where a registered session is in its life.
///
/// `Cancelling` is its own state rather than a flag on `Running`: between the
/// request and the session noticing it, a supervisor asking "is this still
/// working" and "did anyone ask it to stop" would otherwise get answers that
/// contradict each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Running,
    Cancelling,
    Finished(SessionOutcome),
}

impl SessionState {
    pub const fn terminal(self) -> Option<SessionOutcome> {
        match self {
            Self::Finished(outcome) => Some(outcome),
            Self::Running | Self::Cancelling => None,
        }
    }
}

/// One registered session, as an observer sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStatus {
    pub session: SessionId,
    pub model: String,
    pub state: SessionState,
    pub cancellation_requested: bool,
    pub budget: SessionBudget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRegistryError {
    /// A session with this durable id is already live. Its own variant because
    /// the caller must reach the running one rather than start a second copy of
    /// it — two runtimes over one session row would interleave their writes.
    AlreadyLive,
    AtCapacity,
    Unknown,
    /// The session already ended, so there is nothing left to act on.
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionLimits {
    pub max_live_sessions: usize,
    /// How many finished sessions stay listed once they have ended.
    pub retained_finished_sessions: usize,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_live_sessions: DEFAULT_MAX_LIVE_SESSIONS,
            retained_finished_sessions: DEFAULT_RETAINED_FINISHED_SESSIONS,
        }
    }
}

/// What [`SessionRegistry::record_end`] found when it went to record an end.
enum SessionEndRecord {
    Recorded,
    AlreadyEnded,
    Unknown,
}

struct RegisteredSession {
    model: String,
    budget: SessionBudgetHandle,
    cancellation: HeadlessTurnCancellation,
    state: SessionState,
    /// When this session ended, as a registry-wide sequence number.
    ///
    /// A sequence rather than a clock: pruning only has to know which finished
    /// session is the oldest, and session ids are durable rather than ordered by
    /// when the daemon happened to run them.
    finished_at: Option<u64>,
}

impl RegisteredSession {
    fn status(&self, session: SessionId) -> SessionStatus {
        SessionStatus {
            session,
            model: self.model.clone(),
            state: self.state,
            cancellation_requested: self.cancellation.is_cancelled(),
            budget: self.budget.snapshot(),
        }
    }
}

#[derive(Default)]
struct SessionRegistryState {
    sessions: BTreeMap<SessionId, RegisteredSession>,
    limits: SessionLimits,
    ended: u64,
}

/// The queryable record of every session the daemon holds.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<Mutex<SessionRegistryState>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(limits: SessionLimits) -> Self {
        let registry = Self::default();
        registry.locked().limits = limits;
        registry
    }

    pub fn limits(&self) -> SessionLimits {
        self.locked().limits
    }

    /// Registers a session and hands back what it runs with.
    ///
    /// The provider client leaves the admission here and never enters the
    /// registry's own state, so there is no path by which a second session
    /// could reach it.
    pub fn admit(
        &self,
        admission: SessionAdmission,
    ) -> Result<SessionRuntime, SessionRegistryError> {
        let SessionAdmission {
            session,
            provider,
            budget,
        } = admission;

        let mut state = self.locked();

        if state
            .sessions
            .get(&session)
            .is_some_and(|registered| registered.state.terminal().is_none())
        {
            return Err(SessionRegistryError::AlreadyLive);
        }

        let live = state
            .sessions
            .values()
            .filter(|registered| registered.state.terminal().is_none())
            .count();
        if live >= state.limits.max_live_sessions {
            return Err(SessionRegistryError::AtCapacity);
        }

        let budget = SessionBudgetHandle::new(budget);
        let cancellation = HeadlessTurnCancellation::new();
        state.sessions.insert(
            session,
            RegisteredSession {
                model: provider.model().to_owned(),
                budget: budget.clone(),
                cancellation: cancellation.clone(),
                state: SessionState::Running,
                finished_at: None,
            },
        );

        Ok(SessionRuntime {
            session,
            provider,
            budget,
            cancellation,
        })
    }

    pub fn status(&self, session: SessionId) -> Option<SessionStatus> {
        self.locked()
            .sessions
            .get(&session)
            .map(|registered| registered.status(session))
    }

    /// Every registered session in id order, live and finished alike. A finished
    /// one stays listed until it is released: a supervisor that only ever polls
    /// would otherwise never learn how a session it started ended.
    pub fn list(&self) -> Vec<SessionStatus> {
        self.locked()
            .sessions
            .iter()
            .map(|(session, registered)| registered.status(*session))
            .collect()
    }

    /// Asks one session to stop, reaching no other.
    pub fn cancel(&self, session: SessionId) -> Result<(), SessionRegistryError> {
        let mut state = self.locked();
        let registered = state
            .sessions
            .get_mut(&session)
            .ok_or(SessionRegistryError::Unknown)?;
        if registered.state.terminal().is_some() {
            return Err(SessionRegistryError::Terminal);
        }

        registered.cancellation.cancel();
        registered.state = SessionState::Cancelling;
        Ok(())
    }

    /// Asks every live session to stop. Used at shutdown, where the daemon owes
    /// each session the same stop signal it would get individually.
    pub fn cancel_all(&self) -> Vec<SessionId> {
        let mut state = self.locked();
        state
            .sessions
            .iter_mut()
            .filter(|(_, registered)| registered.state.terminal().is_none())
            .map(|(session, registered)| {
                registered.cancellation.cancel();
                registered.state = SessionState::Cancelling;
                *session
            })
            .collect()
    }

    /// Records how a session ended. The first outcome wins: a session that was
    /// cancelled and then reported completing is still the one that was stopped.
    pub fn finish(
        &self,
        session: SessionId,
        outcome: SessionOutcome,
    ) -> Result<(), SessionRegistryError> {
        match self.record_end(session, outcome) {
            SessionEndRecord::Recorded => Ok(()),
            SessionEndRecord::AlreadyEnded => Err(SessionRegistryError::Terminal),
            SessionEndRecord::Unknown => Err(SessionRegistryError::Unknown),
        }
    }

    /// The recording itself, kept infallible for the supervisor: the task that
    /// ran the session is the one authority on how it ended, so there is no
    /// caller above it left to hand a failure to.
    fn record_end(&self, session: SessionId, outcome: SessionOutcome) -> SessionEndRecord {
        let mut state = self.locked();
        let ended = state.ended.saturating_add(1);
        let Some(registered) = state.sessions.get_mut(&session) else {
            return SessionEndRecord::Unknown;
        };
        if registered.state.terminal().is_some() {
            return SessionEndRecord::AlreadyEnded;
        }

        registered.state = SessionState::Finished(outcome);
        registered.finished_at = Some(ended);
        state.ended = ended;
        SessionEndRecord::Recorded
    }

    /// Drops a finished session from the registry.
    pub fn release(&self, session: SessionId) -> Result<(), SessionRegistryError> {
        let mut state = self.locked();
        match state.sessions.get(&session) {
            None => Err(SessionRegistryError::Unknown),
            Some(registered) if registered.state.terminal().is_none() => {
                Err(SessionRegistryError::Terminal)
            }
            Some(_) => {
                state.sessions.remove(&session);
                Ok(())
            }
        }
    }

    /// Drops the finished sessions past the retention bound, oldest first, and
    /// reports which ones went.
    ///
    /// The counterpart to [`Self::release`] for the sessions nobody ever asks
    /// about: `release` is how a supervisor says it has read an outcome, and
    /// this is what keeps the registry bounded when no supervisor ever does.
    /// Live sessions are never touched, and the most recent outcomes are the
    /// ones kept, so a supervisor polling at any sane interval still sees how
    /// the sessions it started ended.
    pub fn prune_finished(&self) -> Vec<SessionId> {
        let mut state = self.locked();
        let retained = state.limits.retained_finished_sessions;

        let mut finished: Vec<(u64, SessionId)> = state
            .sessions
            .iter()
            .filter_map(|(session, registered)| {
                registered.finished_at.map(|ended| (ended, *session))
            })
            .collect();
        if finished.len() <= retained {
            return Vec::new();
        }

        finished.sort_unstable();
        finished.truncate(finished.len() - retained);

        let pruned: Vec<SessionId> = finished.into_iter().map(|(_, session)| session).collect();
        for session in &pruned {
            state.sessions.remove(session);
        }

        pruned
    }

    /// See [`SessionBudgetHandle::locked`] for why poisoning is recovered.
    fn locked(&self) -> std::sync::MutexGuard<'_, SessionRegistryState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// What shutdown found when it stopped waiting for the sessions it cancelled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionShutdown {
    /// The sessions that ended within the deadline.
    pub stopped: Vec<SessionId>,
    /// The sessions still running when the deadline passed.
    ///
    /// Named rather than counted: the work is on the blocking pool and cannot
    /// be interrupted from here, so all shutdown can do for the operator is say
    /// exactly which session outlived the daemon that started it.
    pub abandoned: Vec<SessionId>,
}

impl SessionShutdown {
    /// Whether every cancelled session stopped before the deadline.
    pub fn is_clean(&self) -> bool {
        self.abandoned.is_empty()
    }
}

/// Runs registered sessions on the daemon's runtime.
///
/// Every session crosses into synchronous code through [`BlockingBoundary`],
/// which is also what turns a session that gives up into a recorded failure
/// instead of a lost task: the boundary reports the panic, the registry stores
/// it, and the daemon keeps serving its peers.
#[derive(Clone)]
pub struct SessionSupervisor {
    registry: SessionRegistry,
    boundary: BlockingBoundary,
    handle: Handle,
    workers: Arc<Mutex<BTreeMap<SessionId, JoinHandle<()>>>>,
    shutdown_timeout: Duration,
}

impl SessionSupervisor {
    pub fn new(handle: Handle) -> Self {
        Self::with_registry(SessionRegistry::new(), handle)
    }

    pub fn with_registry(registry: SessionRegistry, handle: Handle) -> Self {
        Self {
            registry,
            boundary: BlockingBoundary::new(handle.clone()),
            handle,
            workers: Arc::new(Mutex::new(BTreeMap::new())),
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }

    /// How long [`Self::cancel_all_and_join`] waits before giving up on the
    /// sessions that have not stopped.
    #[must_use]
    pub const fn with_shutdown_timeout(mut self, shutdown_timeout: Duration) -> Self {
        self.shutdown_timeout = shutdown_timeout;
        self
    }

    pub fn registry(&self) -> &SessionRegistry {
        &self.registry
    }

    /// Admits a session and starts its work, concurrently with every peer.
    pub fn start<F>(
        &self,
        admission: SessionAdmission,
        work: F,
    ) -> Result<SessionId, SessionRegistryError>
    where
        F: FnOnce(SessionRuntime) -> SessionOutcome + Send + 'static,
    {
        let session = admission.session();
        let runtime = self.registry.admit(admission)?;

        let registry = self.registry.clone();
        let boundary = self.boundary.clone();
        let worker = self.handle.spawn(async move {
            let outcome = boundary
                .run(move || work(runtime))
                .await
                .unwrap_or(SessionOutcome::Failed);
            registry.record_end(session, outcome);
        });

        let mut workers = self.locked_workers();
        // Sessions come and go for the life of the daemon, so the handles of the
        // ones that already ended are dropped here rather than accumulating
        // until shutdown. The registry entries they left behind are bounded on
        // the same beat: pruning only where sessions are admitted means the two
        // never drift apart, and a daemon that admits nothing grows nothing.
        workers.retain(|_, worker| !worker.is_finished());
        workers.insert(session, worker);
        drop(workers);
        self.registry.prune_finished();

        Ok(session)
    }

    pub fn status(&self, session: SessionId) -> Option<SessionStatus> {
        self.registry.status(session)
    }

    pub fn list(&self) -> Vec<SessionStatus> {
        self.registry.list()
    }

    pub fn cancel(&self, session: SessionId) -> Result<(), SessionRegistryError> {
        self.registry.cancel(session)
    }

    /// Cancels every live session and waits for each to end, up to this
    /// supervisor's shutdown timeout.
    pub async fn cancel_all_and_join(&self) -> SessionShutdown {
        self.cancel_all_and_join_within(self.shutdown_timeout).await
    }

    /// Cancels every live session and waits up to `timeout` in total for them
    /// to end, reporting which ones did not.
    ///
    /// Awaited rather than aborted: a session owns a provider client and a
    /// turn's worth of work, and dropping the task mid-flight would leave both
    /// to be cleaned up by nobody. Bounded all the same, because cancellation
    /// is cooperative: a session inside work that never looks at it would
    /// otherwise hold the whole process open, and a daemon that cannot be
    /// stopped is worse than one that names what it left behind.
    pub async fn cancel_all_and_join_within(&self, timeout_after: Duration) -> SessionShutdown {
        self.registry.cancel_all();

        let workers = std::mem::take(&mut *self.locked_workers());
        let deadline = Instant::now() + timeout_after;
        let mut shutdown = SessionShutdown::default();

        for (session, mut worker) in workers {
            let remaining = deadline.saturating_duration_since(Instant::now());
            // A join error means the task itself ended abnormally, and by then
            // the session's outcome is already recorded; it stopped either way.
            match timeout(remaining, &mut worker).await {
                Ok(_) => shutdown.stopped.push(session),
                Err(_) => shutdown.abandoned.push(session),
            }
        }

        shutdown
    }

    fn locked_workers(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, JoinHandle<()>>> {
        self.workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    struct StubProvider(&'static str);

    impl SessionProvider for StubProvider {
        fn model(&self) -> &str {
            self.0
        }
    }

    fn admission(session: i64, model: &'static str) -> SessionAdmission {
        SessionAdmission::new(
            SessionId::new(session),
            Box::new(StubProvider(model)),
            SessionBudget::unlimited(),
        )
    }

    #[test]
    fn an_admitted_session_is_listed_under_its_durable_id() {
        let registry = SessionRegistry::new();

        let runtime = registry.admit(admission(12, "model-a")).unwrap();

        assert_eq!(runtime.session(), SessionId::new(12));
        assert_eq!(runtime.provider().model(), "model-a");
        let status = registry.status(SessionId::new(12)).unwrap();
        assert_eq!(status.model, "model-a");
        assert_eq!(status.state, SessionState::Running);
        assert!(!status.cancellation_requested);
    }

    #[test]
    fn sessions_hold_their_own_cancellation() {
        let registry = SessionRegistry::new();
        let first = registry.admit(admission(1, "model-a")).unwrap();
        let second = registry.admit(admission(2, "model-b")).unwrap();

        registry.cancel(SessionId::new(1)).unwrap();

        assert!(first.cancellation().is_cancelled());
        assert!(!second.cancellation().is_cancelled());
        assert_eq!(
            registry.status(SessionId::new(2)).unwrap().state,
            SessionState::Running
        );
    }

    #[test]
    fn a_cancelled_session_reports_cancelling_until_it_ends() {
        let registry = SessionRegistry::new();
        registry.admit(admission(1, "model-a")).unwrap();

        registry.cancel(SessionId::new(1)).unwrap();
        let cancelling = registry.status(SessionId::new(1)).unwrap();
        assert_eq!(cancelling.state, SessionState::Cancelling);
        assert!(cancelling.cancellation_requested);

        registry
            .finish(SessionId::new(1), SessionOutcome::Cancelled)
            .unwrap();
        assert_eq!(
            registry.status(SessionId::new(1)).unwrap().state,
            SessionState::Finished(SessionOutcome::Cancelled)
        );
    }

    #[test]
    fn a_live_session_is_never_registered_twice() {
        let registry = SessionRegistry::new();
        registry.admit(admission(5, "model-a")).unwrap();

        assert_eq!(
            registry.admit(admission(5, "model-a")).err(),
            Some(SessionRegistryError::AlreadyLive)
        );
    }

    /// The id is durable, so the same session coming back after it ended is the
    /// ordinary case of resuming it rather than a collision.
    #[test]
    fn a_finished_session_can_be_admitted_again() {
        let registry = SessionRegistry::new();
        registry.admit(admission(5, "model-a")).unwrap();
        registry
            .finish(SessionId::new(5), SessionOutcome::Completed)
            .unwrap();

        assert!(registry.admit(admission(5, "model-b")).is_ok());
        assert_eq!(registry.status(SessionId::new(5)).unwrap().model, "model-b");
    }

    #[test]
    fn admission_stops_at_the_configured_ceiling_and_frees_up_as_sessions_end() {
        let registry = SessionRegistry::with_limits(SessionLimits {
            max_live_sessions: 2,
            ..SessionLimits::default()
        });
        registry.admit(admission(1, "model-a")).unwrap();
        registry.admit(admission(2, "model-a")).unwrap();

        assert_eq!(
            registry.admit(admission(3, "model-a")).err(),
            Some(SessionRegistryError::AtCapacity)
        );

        registry
            .finish(SessionId::new(1), SessionOutcome::Completed)
            .unwrap();
        assert!(registry.admit(admission(3, "model-a")).is_ok());
    }

    #[test]
    fn acting_on_a_session_the_registry_does_not_hold_is_refused() {
        let registry = SessionRegistry::new();

        assert_eq!(
            registry.cancel(SessionId::new(404)).err(),
            Some(SessionRegistryError::Unknown)
        );
        assert_eq!(
            registry.finish(SessionId::new(404), SessionOutcome::Failed),
            Err(SessionRegistryError::Unknown)
        );
        assert_eq!(
            registry.release(SessionId::new(404)).err(),
            Some(SessionRegistryError::Unknown)
        );
        assert!(registry.status(SessionId::new(404)).is_none());
    }

    #[test]
    fn a_finished_session_stays_listed_until_it_is_released() {
        let registry = SessionRegistry::new();
        registry.admit(admission(1, "model-a")).unwrap();
        registry
            .finish(SessionId::new(1), SessionOutcome::Completed)
            .unwrap();

        assert_eq!(
            registry.list().first().map(|status| status.state),
            Some(SessionState::Finished(SessionOutcome::Completed))
        );
        assert_eq!(
            registry.cancel(SessionId::new(1)).err(),
            Some(SessionRegistryError::Terminal)
        );

        registry.release(SessionId::new(1)).unwrap();
        assert!(registry.list().is_empty());
    }

    #[test]
    fn a_live_session_is_not_released() {
        let registry = SessionRegistry::new();
        registry.admit(admission(1, "model-a")).unwrap();

        assert_eq!(
            registry.release(SessionId::new(1)).err(),
            Some(SessionRegistryError::Terminal)
        );
        assert!(registry.status(SessionId::new(1)).is_some());
    }

    #[test]
    fn how_a_session_ended_is_recorded_once() {
        let registry = SessionRegistry::new();
        registry.admit(admission(1, "model-a")).unwrap();
        registry
            .finish(SessionId::new(1), SessionOutcome::Cancelled)
            .unwrap();

        assert_eq!(
            registry.finish(SessionId::new(1), SessionOutcome::Completed),
            Err(SessionRegistryError::Terminal)
        );
        assert_eq!(
            registry.status(SessionId::new(1)).unwrap().state,
            SessionState::Finished(SessionOutcome::Cancelled)
        );
    }

    #[test]
    fn cancelling_everything_reaches_only_the_live_sessions() {
        let registry = SessionRegistry::new();
        registry.admit(admission(1, "model-a")).unwrap();
        registry.admit(admission(2, "model-a")).unwrap();
        registry
            .finish(SessionId::new(2), SessionOutcome::Completed)
            .unwrap();

        assert_eq!(registry.cancel_all(), vec![SessionId::new(1)]);
        assert_eq!(
            registry.status(SessionId::new(2)).unwrap().state,
            SessionState::Finished(SessionOutcome::Completed)
        );
    }

    #[test]
    fn a_session_spends_only_its_own_budget() {
        let registry = SessionRegistry::new();
        let first = registry
            .admit(SessionAdmission::new(
                SessionId::new(1),
                Box::new(StubProvider("model-a")),
                SessionBudget::with_max_turns(2),
            ))
            .unwrap();
        let second = registry
            .admit(SessionAdmission::new(
                SessionId::new(2),
                Box::new(StubProvider("model-a")),
                SessionBudget::with_max_turns(2),
            ))
            .unwrap();

        assert!(first.budget().consume_turn());
        assert!(first.budget().consume_turn());

        assert!(!first.budget().consume_turn());
        assert_eq!(
            registry
                .status(SessionId::new(1))
                .unwrap()
                .budget
                .spent_turns(),
            2
        );
        assert_eq!(
            registry
                .status(SessionId::new(2))
                .unwrap()
                .budget
                .remaining_turns(),
            Some(2)
        );
        assert!(second.budget().consume_turn());
    }

    #[test]
    fn an_unlimited_budget_never_refuses_a_turn() {
        let budget = SessionBudgetHandle::new(SessionBudget::unlimited());

        assert!(budget.consume_turn());
        assert_eq!(budget.snapshot().remaining_turns(), None);
        assert_eq!(budget.snapshot().spent_turns(), 1);
    }

    #[test]
    fn a_per_turn_iteration_cap_travels_with_the_session() {
        let registry = SessionRegistry::new();
        let runtime = registry
            .admit(SessionAdmission::new(
                SessionId::new(1),
                Box::new(StubProvider("model-a")),
                SessionBudget::with_max_turns(1).with_max_iterations_per_turn(12),
            ))
            .unwrap();

        assert_eq!(
            runtime.budget().snapshot().max_iterations_per_turn(),
            Some(12)
        );
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    fn finish_all(registry: &SessionRegistry, sessions: impl IntoIterator<Item = i64>) {
        for session in sessions {
            registry
                .finish(SessionId::new(session), SessionOutcome::Completed)
                .unwrap();
        }
    }

    fn listed(registry: &SessionRegistry) -> Vec<i64> {
        registry
            .list()
            .into_iter()
            .map(|status| status.session.value())
            .collect()
    }

    #[test]
    fn pruning_keeps_the_most_recent_outcomes_and_drops_the_rest() {
        let registry = SessionRegistry::with_limits(SessionLimits {
            retained_finished_sessions: 2,
            ..SessionLimits::default()
        });
        for session in [1, 2, 3, 4] {
            registry.admit(admission(session, "model-a")).unwrap();
        }
        // Ended out of id order on purpose: what pruning keeps is the sessions
        // that ended last, not the ones with the highest ids.
        finish_all(&registry, [3, 1, 4]);

        assert_eq!(registry.prune_finished(), vec![SessionId::new(3)]);
        assert_eq!(listed(&registry), vec![1, 2, 4]);
    }

    #[test]
    fn pruning_never_touches_a_live_session() {
        let registry = SessionRegistry::with_limits(SessionLimits {
            retained_finished_sessions: 0,
            ..SessionLimits::default()
        });
        registry.admit(admission(1, "model-a")).unwrap();
        registry.admit(admission(2, "model-a")).unwrap();
        finish_all(&registry, [2]);

        assert_eq!(registry.prune_finished(), vec![SessionId::new(2)]);
        assert_eq!(listed(&registry), vec![1]);
        assert_eq!(
            registry.status(SessionId::new(1)).unwrap().state,
            SessionState::Running
        );
    }

    #[test]
    fn pruning_below_the_retention_bound_drops_nothing() {
        let registry = SessionRegistry::with_limits(SessionLimits {
            retained_finished_sessions: 4,
            ..SessionLimits::default()
        });
        registry.admit(admission(1, "model-a")).unwrap();
        finish_all(&registry, [1]);

        assert!(registry.prune_finished().is_empty());
        assert_eq!(listed(&registry), vec![1]);
    }

    /// The registry used to grow for the life of the daemon: `release` had no
    /// production caller, so every session a daemon ever ran stayed listed.
    #[test]
    fn admitting_a_session_prunes_the_outcomes_nobody_came_back_for() {
        let runtime = runtime();
        let registry = SessionRegistry::with_limits(SessionLimits {
            retained_finished_sessions: 1,
            ..SessionLimits::default()
        });
        let supervisor = SessionSupervisor::with_registry(registry, runtime.handle().clone());

        for session in [1, 2, 3] {
            supervisor
                .start(admission(session, "model-a"), |_| SessionOutcome::Completed)
                .unwrap();
            wait_for(&supervisor, session);
        }

        // Held open so the session whose admission triggers the pruning is
        // still running while it happens, which is what makes the listing below
        // the same on a loaded machine as on an idle one.
        let (release, released) = mpsc::channel::<()>();
        supervisor
            .start(admission(4, "model-a"), move |_| {
                let _ = released.recv();
                SessionOutcome::Completed
            })
            .unwrap();

        assert_eq!(listed(supervisor.registry()), vec![3, 4]);

        drop(release);
        wait_for(&supervisor, 4);
    }

    fn wait_for(supervisor: &SessionSupervisor, session: i64) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while supervisor
            .status(SessionId::new(session))
            .and_then(|status| status.state.terminal())
            .is_none()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for session {session} to end"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Cancellation is cooperative, so shutdown cannot assume every session
    /// observes it. Unbounded, one session inside uncancellable work held the
    /// whole process open.
    #[test]
    fn shutdown_stops_waiting_for_a_session_that_ignores_its_cancellation() {
        let runtime = runtime();
        let supervisor = SessionSupervisor::new(runtime.handle().clone());
        let (release, released) = mpsc::channel::<()>();

        supervisor
            .start(admission(1, "model-a"), move |_| {
                // Deaf to its cancellation on purpose: a child process with no
                // timeout of its own behaves exactly like this.
                let _ = released.recv();
                SessionOutcome::Completed
            })
            .unwrap();
        supervisor
            .start(admission(2, "model-b"), |_| SessionOutcome::Completed)
            .unwrap();
        wait_for(&supervisor, 2);

        let shutdown =
            runtime.block_on(supervisor.cancel_all_and_join_within(Duration::from_millis(50)));

        assert_eq!(shutdown.abandoned, vec![SessionId::new(1)]);
        assert_eq!(shutdown.stopped, vec![SessionId::new(2)]);
        assert!(!shutdown.is_clean());

        drop(release);
        runtime.shutdown_timeout(Duration::from_secs(5));
    }

    #[test]
    fn shutdown_reports_clean_when_every_session_stops_in_time() {
        let runtime = runtime();
        let supervisor = SessionSupervisor::new(runtime.handle().clone());

        supervisor
            .start(admission(1, "model-a"), |runtime| {
                while !runtime.cancellation().is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                SessionOutcome::Cancelled
            })
            .unwrap();

        let shutdown =
            runtime.block_on(supervisor.cancel_all_and_join_within(Duration::from_secs(5)));

        assert!(shutdown.is_clean());
        assert_eq!(shutdown.stopped, vec![SessionId::new(1)]);
        assert_eq!(
            supervisor.status(SessionId::new(1)).unwrap().state,
            SessionState::Finished(SessionOutcome::Cancelled)
        );
    }
}
