//! What the coordinator's loops owe a shutdown.
//!
//! Both tests run against a composed coordinator rather than against a loop
//! body, because what they assert is a property of the running daemon: how long
//! `stop` takes, and what is still folded after it was asked to stop.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agens_server::{
    Coordinator, CoordinatorSettings, IngestFact, LaunchError, ReportedFact, RunLaunch,
    RunWorkerFactory, SessionSupervisor,
};
use agens_store::{AttemptRow, ControlPlaneStore, RunRow, RunState, WorktreeStatus};

const REPO: &str = "a1b2c3d4e5f60718";
const PROVIDER: &str = "scripted";

/// The physical execution the reported fact is attributed to.
const SESSION_ATTEMPT: i64 = 1;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn scratch_directory(kind: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-server-loops-{kind}-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();

    directory
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

fn run_in(state: RunState, worktree: &Path) -> RunRow {
    RunRow {
        id: None,
        repo_id: REPO.to_owned(),
        repo_root: "/home/dev/agens".to_owned(),
        remote_url: None,
        external_ref: Some("agens/AGN-185".to_owned()),
        parent_run_id: None,
        task: "the loops answer to the stop flag".to_owned(),
        scope: "crates/agens-server/src/coordinator".to_owned(),
        dod: "a shutdown waits for no backoff and loses no fact".to_owned(),
        genesis_paths: None,
        state,
        priority: 5,
        dep_run_id: None,
        provider: PROVIDER.to_owned(),
        budget_tokens: None,
        worktree_path: Some(worktree.display().to_string()),
        worktree_status: Some(WorktreeStatus::Active),
        created_at: now(),
        result: None,
    }
}

fn worktree_in(directory: &Path) -> PathBuf {
    let worktree = directory.join("worktrees").join(REPO).join("agn-185");
    fs::create_dir_all(&worktree).unwrap();

    worktree
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// A worker that refuses every launch and counts how often it was asked.
fn counting_refusals() -> (RunWorkerFactory, Arc<AtomicUsize>) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);

    (
        Arc::new(move |_launch: &RunLaunch<'_>| {
            counter.fetch_add(1, Ordering::Release);

            Err(LaunchError("this test starts no sessions".to_owned()))
        }) as RunWorkerFactory,
        attempts,
    )
}

fn await_launch_attempt(attempts: &AtomicUsize) {
    let deadline = Instant::now() + Duration::from_secs(10);

    while attempts.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        attempts.load(Ordering::Acquire) > 0,
        "the admission loop never reached the launcher"
    );
}

/// A launch that did not work pauses admission for twenty heartbeats. Slept
/// through in one call, that pause also became the shutdown's: `stop` sets the
/// flag and joins, and the loop was not looking at the flag until the pause was
/// over.
#[test]
fn a_shutdown_does_not_wait_out_the_failed_launch_backoff() {
    let directory = scratch_directory("backoff");
    let worktree = worktree_in(&directory);

    {
        let mut store = ControlPlaneStore::open(&directory).unwrap();
        store
            .insert_run(&run_in(RunState::Queued, &worktree))
            .unwrap();
    }

    let runtime = runtime();
    let (worker, attempts) = counting_refusals();
    let shutdown = agens_core::HeadlessTurnCancellation::new();
    let coordinator = Coordinator::start(
        &directory,
        &CoordinatorSettings::default(),
        SessionSupervisor::new(runtime.handle().clone()),
        worker,
        &shutdown,
    )
    .expect("the coordinator composes over the data directory");

    await_launch_attempt(&attempts);

    // Long enough for the loop to be inside the pause the refusal earned it.
    std::thread::sleep(Duration::from_millis(300));

    let asked_to_stop = Instant::now();
    coordinator.stop();
    let took = asked_to_stop.elapsed();

    runtime.shutdown_timeout(Duration::ZERO);

    // The backoff is twenty heartbeats of 250ms. Anything near it means the
    // shutdown waited for the pause rather than interrupting it.
    assert!(
        took < Duration::from_secs(2),
        "the shutdown waited {took:?} for the backoff"
    );

    fs::remove_dir_all(directory).unwrap();
}

/// The facts reported in the last window before a stop are folded on the way
/// out. Left in the channel, a worker's evidence is journaled and its run's
/// health describes a window the daemon had the facts for and never read.
#[test]
fn the_facts_of_the_last_window_are_ingested_on_the_way_out() {
    let directory = scratch_directory("drain");
    let worktree = worktree_in(&directory);

    let run_id = {
        let mut store = ControlPlaneStore::open(&directory).unwrap();
        let run_id = store
            .insert_run(&run_in(RunState::Running, &worktree))
            .unwrap();

        seed_session_attempt(&directory);

        store
            .insert_attempt(&AttemptRow {
                id: None,
                run_id,
                n: 1,
                session_id: Some(SESSION_ATTEMPT),
                session_attempt_id: Some(SESSION_ATTEMPT),
                started_at: now(),
                ended_at: None,
                outcome: None,
                retry_trigger: None,
                tokens: None,
                cost_micros: None,
            })
            .unwrap();

        run_id
    };

    // A heartbeat long enough that the loop is asleep for the whole of this
    // test: the only pass that can fold the fact is the one after the stop.
    let settings = CoordinatorSettings {
        heartbeat: Duration::from_secs(2),
        ..CoordinatorSettings::default()
    };

    let runtime = runtime();
    let (worker, _) = counting_refusals();
    let shutdown = agens_core::HeadlessTurnCancellation::new();
    let coordinator = Coordinator::start(
        &directory,
        &settings,
        SessionSupervisor::new(runtime.handle().clone()),
        worker,
        &shutdown,
    )
    .expect("the coordinator composes over the data directory");

    std::thread::sleep(Duration::from_millis(400));

    coordinator
        .facts()
        .report(ReportedFact {
            run_id,
            attempt_id: SESSION_ATTEMPT,
            turn: 1,
            now: now(),
            fact: IngestFact::TurnStarted,
        })
        .expect("the ingest channel has a reader");

    coordinator.stop();
    runtime.shutdown_timeout(Duration::ZERO);

    let store = ControlPlaneStore::open(&directory).unwrap();
    let journalled: Vec<String> = store
        .events_for_run(run_id)
        .unwrap()
        .into_iter()
        .map(|event| event.event_type)
        .collect();

    assert!(
        journalled.iter().any(|event| event == "turn_started"),
        "the last window's fact was dropped: {journalled:?}"
    );
    assert!(
        store.load_run_health(run_id).unwrap().is_some(),
        "the fact was folded into the run's health"
    );

    fs::remove_dir_all(directory).unwrap();
}

/// The session rows the evidence ledger's foreign keys require, which no store
/// API exposes because nothing but a live session creates them.
fn seed_session_attempt(directory: &Path) {
    let connection = rusqlite::Connection::open(directory.join("agens.db")).unwrap();

    connection
        .execute(
            "INSERT OR IGNORE INTO sessions (
                 id, project, title, active_agent, created_at, updated_at
             ) VALUES (?1, 'project', 'title', 'build', 0, 0)",
            [SESSION_ATTEMPT],
        )
        .unwrap();
    connection
        .execute(
            "INSERT OR IGNORE INTO session_attempts (
                 id, session_id, sequence, status, retry_prompt, started_at
             ) VALUES (?1, ?1, 1, 'running', 'retry', 0)",
            [SESSION_ATTEMPT],
        )
        .unwrap();
}
