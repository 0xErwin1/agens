//! Boot reconciliation, driven the way a restart drives it.
//!
//! The data directory is prepared to look like what a killed daemon leaves
//! behind — a run recorded as `running`, the attempt it was executing still
//! open, and a worktree directory nothing claims — and a coordinator is then
//! composed over it. Everything asserted afterwards is read back from the
//! control plane, because a reconciliation that only holds inside the pass that
//! ran it has reconciled nothing.

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use agens_server::{Coordinator, CoordinatorSettings, SessionSupervisor, WORKTREE_ORPHANED_EVENT};
use agens_store::{AttemptOutcome, AttemptRow, ControlPlaneStore, RunRow, RunState};

use common::{now, refusing_worker, run_in, scratch_directory, worktree_in};

/// How long an assertion waits for a loop that ticks on a heartbeat.
const PATIENCE: Duration = Duration::from_secs(10);

/// A run the last process was executing when it died.
fn interrupted_run(worktree: &Path) -> RunRow {
    RunRow {
        external_ref: Some("agens/AGN-62".to_owned()),
        task: "reconcile the state a restart left behind".to_owned(),
        dod: "a running row with no session goes back to the queue".to_owned(),
        ..run_in(RunState::Running, worktree)
    }
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
    let directory = scratch_directory("reconcile", "running-row");
    let orphan = worktree_in(&directory, "agn-99");
    let worktree = worktree_in(&directory, "agn-62");

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

    let shutdown = agens_core::HeadlessTurnCancellation::new();
    let coordinator = Coordinator::start(
        &directory,
        &CoordinatorSettings::default(),
        supervisor,
        refusing_worker(),
        &shutdown,
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

/// Cancellation moves the run and never touches its directory, so a cancelled
/// run still claims one.
///
/// Boot reconciliation used to leave `cancelled` out of the states it walked,
/// and so reported that directory as work nobody claimed while the scheduler's
/// ceiling went on counting it against admission. Both now read one list.
#[test]
fn a_cancelled_run_still_claims_its_worktree() {
    let directory = scratch_directory("reconcile", "cancelled-claim");
    let worktree = worktree_in(&directory, "agn-191");

    {
        let mut store = ControlPlaneStore::open(&directory).expect("open the control plane");
        store
            .insert_run(&RunRow {
                state: RunState::Cancelled,
                ..interrupted_run(&worktree)
            })
            .expect("insert the run");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let supervisor = SessionSupervisor::new(runtime.handle().clone());

    let shutdown = agens_core::HeadlessTurnCancellation::new();
    let coordinator = Coordinator::start(
        &directory,
        &CoordinatorSettings::default(),
        supervisor,
        refusing_worker(),
        &shutdown,
    )
    .expect("the coordinator composes over the data directory");

    let reconciliation = coordinator.reconciliation().clone();

    coordinator.stop();
    runtime.shutdown_timeout(Duration::ZERO);

    assert!(
        reconciliation.orphan_worktrees.is_empty(),
        "a cancelled run's directory is claimed, not orphaned: {:?}",
        reconciliation.orphan_worktrees
    );
    assert!(reconciliation.missing_worktrees.is_empty());

    fs::remove_dir_all(directory).unwrap();
}
