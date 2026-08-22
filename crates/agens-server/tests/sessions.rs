//! N sessions living in one daemon as peers.

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use agens_core::HeadlessTurnCancellation;
use agens_server::{
    Daemon, SessionAdmission, SessionBudget, SessionId, SessionOutcome, SessionProvider,
    SessionRuntime, SessionShutdown, SessionState, SessionSupervisor,
};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "agens-server-sessions-{}-{suffix}",
        std::process::id()
    ))
}

/// Counts how many provider clients are alive at once, which is what says the
/// sessions each hold their own instead of sharing one.
struct CountingProvider {
    model: String,
    live: Arc<AtomicUsize>,
}

impl CountingProvider {
    fn new(model: &str, live: &Arc<AtomicUsize>) -> Self {
        live.fetch_add(1, Ordering::Release);
        Self {
            model: model.to_owned(),
            live: Arc::clone(live),
        }
    }
}

impl SessionProvider for CountingProvider {
    fn model(&self) -> &str {
        &self.model
    }
}

impl Drop for CountingProvider {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::Release);
    }
}

fn admission(session: i64, model: &str, live: &Arc<AtomicUsize>) -> SessionAdmission {
    SessionAdmission::new(
        SessionId::new(session),
        Box::new(CountingProvider::new(model, live)),
        SessionBudget::unlimited(),
    )
}

/// A session that runs until its own cancellation fires, reporting the id it was
/// handed so the test can see which session the work belongs to.
fn until_cancelled(
    started: mpsc::Sender<i64>,
    barrier: Arc<Barrier>,
) -> impl FnOnce(SessionRuntime) -> SessionOutcome + Send + 'static {
    move |runtime| {
        let _ = started.send(runtime.session().value());
        // Both sessions have to reach this point for either to pass it, so the
        // test only proceeds once they are genuinely in flight at the same time.
        barrier.wait();

        while !runtime.cancellation().is_cancelled() {
            thread::sleep(Duration::from_millis(5));
        }

        SessionOutcome::Cancelled
    }
}

fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);

    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

struct RunningDaemon {
    sessions: SessionSupervisor,
    shutdown: HeadlessTurnCancellation,
    daemon: Option<thread::JoinHandle<SessionShutdown>>,
    directory: PathBuf,
}

impl RunningDaemon {
    fn start() -> Self {
        let directory = data_directory();
        let daemon = Daemon::start(&directory).unwrap();
        let sessions = daemon.sessions().clone();
        let shutdown = HeadlessTurnCancellation::new();
        let daemon_shutdown = shutdown.clone();
        let handle = thread::spawn(move || daemon.run_until_shutdown(&daemon_shutdown));

        Self {
            sessions,
            shutdown,
            daemon: Some(handle),
            directory,
        }
    }

    fn state(&self, session: i64) -> SessionState {
        self.sessions
            .status(SessionId::new(session))
            .unwrap_or_else(|| panic!("session {session} is not registered"))
            .state
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(daemon) = self.daemon.take() {
            let shutdown = daemon.join().unwrap();
            assert!(
                shutdown.is_clean(),
                "the daemon left sessions behind: {:?}",
                shutdown.abandoned
            );
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn two_sessions_run_at_once_and_cancelling_one_leaves_the_other_running() {
    let daemon = RunningDaemon::start();
    let live_providers = Arc::new(AtomicUsize::new(0));
    let (started, starts) = mpsc::channel();
    let barrier = Arc::new(Barrier::new(2));

    daemon
        .sessions
        .start(
            admission(7, "model-a", &live_providers),
            until_cancelled(started.clone(), Arc::clone(&barrier)),
        )
        .unwrap();
    daemon
        .sessions
        .start(
            admission(9, "model-b", &live_providers),
            until_cancelled(started, Arc::clone(&barrier)),
        )
        .unwrap();

    let mut in_flight = vec![
        starts.recv_timeout(Duration::from_secs(5)).unwrap(),
        starts.recv_timeout(Duration::from_secs(5)).unwrap(),
    ];
    in_flight.sort_unstable();
    assert_eq!(in_flight, vec![7, 9]);
    assert_eq!(
        live_providers.load(Ordering::Acquire),
        2,
        "the sessions share a provider client instead of holding their own"
    );

    daemon.sessions.cancel(SessionId::new(7)).unwrap();
    wait_until(
        || daemon.state(7) == SessionState::Finished(SessionOutcome::Cancelled),
        "the cancelled session to end",
    );

    let survivor = daemon.sessions.status(SessionId::new(9)).unwrap();
    assert_eq!(survivor.state, SessionState::Running);
    assert!(
        !survivor.cancellation_requested,
        "cancelling one session reached the other"
    );

    daemon.sessions.cancel(SessionId::new(9)).unwrap();
    wait_until(
        || daemon.state(9) == SessionState::Finished(SessionOutcome::Cancelled),
        "the second session to end",
    );
}

/// The registry key is the durable session id, which is what `agens direct
/// --session <id>` addresses and what a turn publishes in `turn_started`. A
/// registry keyed by anything else would leave a live session unreachable.
#[test]
fn a_live_session_is_listed_and_reachable_by_its_durable_session_id() {
    let daemon = RunningDaemon::start();
    let live_providers = Arc::new(AtomicUsize::new(0));
    let (started, starts) = mpsc::channel();
    let barrier = Arc::new(Barrier::new(1));

    daemon
        .sessions
        .start(
            admission(41, "model-a", &live_providers),
            until_cancelled(started, barrier),
        )
        .unwrap();

    assert_eq!(starts.recv_timeout(Duration::from_secs(5)).unwrap(), 41);

    let listed = daemon.sessions.list();
    assert_eq!(listed.len(), 1);
    let status = listed.first().unwrap();
    assert_eq!(status.session, SessionId::new(41));
    assert_eq!(status.model, "model-a");
    assert_eq!(status.state, SessionState::Running);

    daemon.sessions.cancel(SessionId::new(41)).unwrap();
    wait_until(
        || daemon.state(41) == SessionState::Finished(SessionOutcome::Cancelled),
        "the session to end",
    );
}

/// Shutting the daemon down is not a way to lose track of live work: every
/// session is cancelled and joined before the process lets go of its slot.
#[test]
fn shutdown_cancels_every_live_session() {
    let directory = data_directory();
    let daemon = Daemon::start(&directory).unwrap();
    let sessions = daemon.sessions().clone();
    let shutdown = HeadlessTurnCancellation::new();
    let daemon_shutdown = shutdown.clone();
    let live_providers = Arc::new(AtomicUsize::new(0));
    let (started, starts) = mpsc::channel();
    let barrier = Arc::new(Barrier::new(2));

    sessions
        .start(
            admission(1, "model-a", &live_providers),
            until_cancelled(started.clone(), Arc::clone(&barrier)),
        )
        .unwrap();
    sessions
        .start(
            admission(2, "model-b", &live_providers),
            until_cancelled(started, barrier),
        )
        .unwrap();
    starts.recv_timeout(Duration::from_secs(5)).unwrap();
    starts.recv_timeout(Duration::from_secs(5)).unwrap();

    let running = thread::spawn(move || daemon.run_until_shutdown(&daemon_shutdown));
    shutdown.cancel();
    let report = running.join().unwrap();

    assert!(report.is_clean(), "a session outlived the daemon's wait");
    assert_eq!(report.stopped, vec![SessionId::new(1), SessionId::new(2)]);

    for session in [1, 2] {
        assert_eq!(
            sessions.status(SessionId::new(session)).unwrap().state,
            SessionState::Finished(SessionOutcome::Cancelled)
        );
    }
    assert_eq!(
        live_providers.load(Ordering::Acquire),
        0,
        "a provider client outlived the session that owned it"
    );

    fs::remove_dir_all(directory).unwrap();
}

/// A session that gives up must not take the daemon or its peers with it.
#[test]
fn a_session_that_panics_is_recorded_as_failed_and_its_peer_keeps_running() {
    let daemon = RunningDaemon::start();
    let live_providers = Arc::new(AtomicUsize::new(0));
    let (started, starts) = mpsc::channel();
    let barrier = Arc::new(Barrier::new(1));

    daemon
        .sessions
        .start(
            admission(3, "model-a", &live_providers),
            until_cancelled(started, barrier),
        )
        .unwrap();
    assert_eq!(starts.recv_timeout(Duration::from_secs(5)).unwrap(), 3);

    daemon
        .sessions
        .start(admission(4, "model-b", &live_providers), |_| {
            panic!("a session gave up")
        })
        .unwrap();

    wait_until(
        || daemon.state(4) == SessionState::Finished(SessionOutcome::Failed),
        "the panicking session to be recorded as failed",
    );
    assert_eq!(daemon.state(3), SessionState::Running);

    daemon.sessions.cancel(SessionId::new(3)).unwrap();
    wait_until(
        || daemon.state(3) == SessionState::Finished(SessionOutcome::Cancelled),
        "the surviving session to end",
    );
}

/// `release` had no production caller, which is how the registry came to grow
/// for the life of the daemon. A finished session has to leave it.
#[test]
fn a_session_that_ran_to_completion_is_released_from_the_registry() {
    let daemon = RunningDaemon::start();
    let live_providers = Arc::new(AtomicUsize::new(0));

    daemon
        .sessions
        .start(admission(21, "model-a", &live_providers), |_| {
            SessionOutcome::Completed
        })
        .unwrap();
    wait_until(
        || daemon.state(21) == SessionState::Finished(SessionOutcome::Completed),
        "the session to end",
    );

    daemon
        .sessions
        .registry()
        .release(SessionId::new(21))
        .unwrap();

    assert!(daemon.sessions.status(SessionId::new(21)).is_none());
    assert!(daemon.sessions.list().is_empty());
    assert_eq!(
        live_providers.load(Ordering::Acquire),
        0,
        "a provider client outlived the session that owned it"
    );
}
