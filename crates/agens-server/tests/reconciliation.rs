//! Boot reconciliation, driven the way a restart drives it.
//!
//! The data directory is prepared to look like what a killed daemon leaves
//! behind — a run recorded as `running`, the attempt it was executing still
//! open, and a worktree directory nothing claims — and a coordinator is then
//! composed over it. Everything asserted afterwards is read back from the
//! control plane, because a reconciliation that only holds inside the pass that
//! ran it has reconciled nothing.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agens_server::{
    Coordinator, CoordinatorSettings, LaunchError, RunLaunch, RunWorkerFactory, SessionSupervisor,
    WORKTREE_ORPHANED_EVENT,
};
use agens_store::{
    AttemptOutcome, AttemptRow, ControlPlaneStore, RunRow, RunState, WorktreeStatus,
};

const REPO: &str = "a1b2c3d4e5f60718";
const PROVIDER: &str = "scripted";

/// How long an assertion waits for a loop that ticks on a heartbeat.
const PATIENCE: Duration = Duration::from_secs(10);

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn scratch_directory(kind: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-server-reconcile-{kind}-{}-{suffix}",
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

/// A run the last process was executing when it died.
fn interrupted_run(worktree: &Path) -> RunRow {
    RunRow {
        id: None,
        repo_id: REPO.to_owned(),
        repo_root: "/home/dev/agens".to_owned(),
        remote_url: None,
        external_ref: Some("agens/AGN-62".to_owned()),
        parent_run_id: None,
        task: "reconcile the state a restart left behind".to_owned(),
        scope: "crates/agens-server/src/coordinator".to_owned(),
        dod: "a running row with no session goes back to the queue".to_owned(),
        genesis_paths: None,
        state: RunState::Running,
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

/// A worker that never produces a session.
///
/// The run this test follows is the one reconciliation put back in the queue,
/// and a launch that refuses leaves it queued where the assertion can read it
/// instead of racing the admission loop for the row.
fn refusing_worker() -> RunWorkerFactory {
    std::sync::Arc::new(|_launch: &RunLaunch<'_>| {
        Err(LaunchError("this test starts no sessions".to_owned()))
    }) as RunWorkerFactory
}

/// The run's state, read straight from the control plane.
fn state_of(directory: &Path, run_id: i64) -> Option<RunState> {
    ControlPlaneStore::open(directory)
        .ok()?
        .load_run(run_id)
        .ok()
        .flatten()
        .map(|run| run.state)
}

fn await_state(directory: &Path, run_id: i64, wanted: RunState) -> Option<RunState> {
    let deadline = Instant::now() + PATIENCE;

    loop {
        let state = state_of(directory, run_id);

        if state == Some(wanted) || Instant::now() >= deadline {
            return state;
        }

        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_coordinator_started_over_a_running_row_puts_it_back_in_the_queue() {
    let directory = scratch_directory("running-row");
    let orphan = directory.join("worktrees").join(REPO).join("agn-99");
    fs::create_dir_all(&orphan).unwrap();
    let worktree = directory.join("worktrees").join(REPO).join("agn-62");
    fs::create_dir_all(&worktree).unwrap();

    let run_id = {
        let mut store = ControlPlaneStore::open(&directory).expect("open the control plane");
        let run_id = store
            .insert_run(&interrupted_run(&worktree))
            .expect("insert the run");

        // The attempt the lost turn was executing, still open, which is what a
        // process that was killed mid-turn leaves behind.
        store
            .insert_attempt(&AttemptRow {
                id: None,
                run_id,
                n: 1,
                session_id: None,
                session_attempt_id: None,
                started_at: now(),
                ended_at: None,
                outcome: None,
                retry_trigger: None,
                tokens: None,
                cost_micros: None,
            })
            .expect("open the attempt");

        run_id
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let supervisor = SessionSupervisor::new(runtime.handle().clone());

    let coordinator = Coordinator::start(
        &directory,
        &CoordinatorSettings::default(),
        supervisor,
        refusing_worker(),
    )
    .expect("the coordinator composes over the data directory");

    let reconciliation = coordinator.reconciliation().clone();

    assert_eq!(reconciliation.interrupted, vec![run_id]);
    assert_eq!(reconciliation.resumed, vec![run_id]);
    assert_eq!(
        reconciliation
            .orphan_worktrees
            .iter()
            .map(|orphan| orphan.path.clone())
            .collect::<Vec<_>>(),
        vec![orphan.clone()],
        "the directory no run claims is the only orphan"
    );
    assert!(reconciliation.missing_worktrees.is_empty());

    assert_eq!(
        await_state(&directory, run_id, RunState::Queued),
        Some(RunState::Queued),
        "a running row with no session behind it goes back to the queue"
    );

    coordinator.stop();
    runtime.shutdown_timeout(Duration::ZERO);

    let store = ControlPlaneStore::open(&directory).expect("reopen the control plane");

    // The lost turn is not the worker's failure, so the attempt it ran as is
    // closed as interrupted and the retry budget is untouched.
    let attempts = store.attempts_for_run(run_id).expect("read the attempts");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, Some(AttemptOutcome::Interrupted));
    assert!(attempts[0].ended_at.is_some());

    let journalled: Vec<String> = store
        .events_for_run(run_id)
        .expect("read the journal")
        .into_iter()
        .map(|event| event.event_type)
        .collect();
    assert!(
        journalled.contains(&"run_interrupted".to_owned())
            && journalled.contains(&"run_resumed".to_owned()),
        "{journalled:?}"
    );

    // The orphan is a request for the cleaning flow, not a removal: the
    // directory is still there for a pass that re-derives merge state first.
    assert!(orphan.is_dir());
    assert_eq!(
        store
            .events_after(0, 256)
            .expect("read the journal")
            .into_iter()
            .filter(|event| event.event_type == WORKTREE_ORPHANED_EVENT)
            .count(),
        1
    );

    fs::remove_dir_all(directory).unwrap();
}
