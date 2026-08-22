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
};

use agens_core::HeadlessTurnCancellation;
use tokio::{runtime::Handle, task::JoinHandle};

use crate::blocking::BlockingBoundary;

/// How many sessions one daemon keeps alive at once.
///
/// A ceiling rather than a queue: the scheduler that decides what waits and in
/// which order is a separate piece, and admitting without bound would let a
/// caller exhaust the machine before that piece exists.
const DEFAULT_MAX_LIVE_SESSIONS: usize = 8;

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
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_live_sessions: DEFAULT_MAX_LIVE_SESSIONS,
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
        let Some(registered) = state.sessions.get_mut(&session) else {
            return SessionEndRecord::Unknown;
        };
        if registered.state.terminal().is_some() {
            return SessionEndRecord::AlreadyEnded;
        }

        registered.state = SessionState::Finished(outcome);
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

    /// See [`SessionBudgetHandle::locked`] for why poisoning is recovered.
    fn locked(&self) -> std::sync::MutexGuard<'_, SessionRegistryState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        }
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
        // until shutdown.
        workers.retain(|_, worker| !worker.is_finished());
        workers.insert(session, worker);

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

    /// Cancels every live session and waits for each to end.
    ///
    /// Awaited rather than aborted: a session owns a provider client and a
    /// turn's worth of work, and dropping the task mid-flight would leave both
    /// to be cleaned up by nobody.
    pub async fn cancel_all_and_join(&self) {
        self.registry.cancel_all();

        let workers = std::mem::take(&mut *self.locked_workers());
        for (_, worker) in workers {
            // A join error means the task itself ended abnormally, and by then
            // the session's outcome is already recorded; waiting is all this
            // owes the caller.
            let _ = worker.await;
        }
    }

    fn locked_workers(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, JoinHandle<()>>> {
        self.workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
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
}
