//! What the admission loop writes down about a queue that is not moving.
//!
//! Driven through a composed coordinator rather than the scheduler on its own,
//! because the thing under test is what the loop does with the report — the
//! scheduler already returned the same report before any of this was journaled.

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use agens_core::HeadlessTurnCancellation;
use agens_server::{
    ADMISSION_FAILED_EVENT, Coordinator, CoordinatorSettings, RUN_DEFERRED_EVENT, SchedulerLimits,
    SessionSupervisor,
};
use agens_store::{ControlPlaneStore, EventRow, RunRow, RunState};

use common::{REFUSAL, refusing_worker, run_in, scratch_directory, worktree_in};

/// How long an assertion waits for a loop that ticks on a heartbeat.
const PATIENCE: Duration = Duration::from_secs(10);

/// A queued run with the worktree `CreateRun` provisions.
///
/// Admission reads that column: a run whose directory is not `active` is held
/// as ineligible rather than offered a slot, and this test writes the row
/// directly instead of going through the call that would have provisioned it.
fn queued_run(directory: &Path) -> RunRow {
    let worktree = worktree_in(directory, "agn-186");

    RunRow {
        external_ref: Some("agens/AGN-186".to_owned()),
        task: "make a queue that is not moving visible".to_owned(),
        dod: "the reason a run stayed queued is in the journal".to_owned(),
        ..run_in(RunState::Queued, &worktree)
    }
}

fn entries_of_type(directory: &Path, run_id: i64, event_type: &str) -> Vec<EventRow> {
    ControlPlaneStore::open(directory)
        .expect("open the control plane")
        .events_of_type_for_run(run_id, event_type)
        .expect("read the journal")
}

/// Waits for at least one entry of this type, then keeps watching long enough
/// for a loop that journaled per tick to write a second one.
fn settled_entries(directory: &Path, run_id: i64, event_type: &str) -> Vec<EventRow> {
    let deadline = Instant::now() + PATIENCE;

    while entries_of_type(directory, run_id, event_type).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    std::thread::sleep(Duration::from_millis(500));

    entries_of_type(directory, run_id, event_type)
}

fn coordinator_over(
    directory: &Path,
    settings: &CoordinatorSettings,
) -> (
    Coordinator,
    tokio::runtime::Runtime,
    HeadlessTurnCancellation,
) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let supervisor = SessionSupervisor::new(runtime.handle().clone());

    let shutdown = HeadlessTurnCancellation::new();
    let coordinator = Coordinator::start(
        directory,
        settings,
        supervisor,
        refusing_worker(),
        &shutdown,
    )
    .expect("the coordinator composes over the data directory");

    (coordinator, runtime, shutdown)
}

#[test]
fn a_run_held_by_a_ceiling_says_so_once_rather_than_on_every_tick() {
    let directory = scratch_directory("queue", "deferred");

    let run_id = ControlPlaneStore::open(&directory)
        .expect("open the control plane")
        .insert_run(&queued_run(&directory))
        .expect("insert the run");

    let settings = CoordinatorSettings {
        scheduler: SchedulerLimits {
            max_concurrent: 0,
            ..CoordinatorSettings::default().scheduler
        },
        ..CoordinatorSettings::default()
    };
    let (coordinator, runtime, _shutdown) = coordinator_over(&directory, &settings);

    let deferred = settled_entries(&directory, run_id, RUN_DEFERRED_EVENT);

    coordinator.stop();
    runtime.shutdown_timeout(Duration::ZERO);

    assert_eq!(
        deferred.len(),
        1,
        "one standing condition is one entry, not one per heartbeat: {deferred:?}"
    );
    let payload: serde_json::Value = serde_json::from_str(&deferred[0].payload).unwrap();
    assert_eq!(payload["reason"], "max_concurrent");
    assert_eq!(payload["limit"], 0);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_launch_that_keeps_failing_says_why_and_says_it_once() {
    let directory = scratch_directory("queue", "failed");

    let run_id = ControlPlaneStore::open(&directory)
        .expect("open the control plane")
        .insert_run(&queued_run(&directory))
        .expect("insert the run");

    let (coordinator, runtime, _shutdown) =
        coordinator_over(&directory, &CoordinatorSettings::default());

    let failures = settled_entries(&directory, run_id, ADMISSION_FAILED_EVENT);

    coordinator.stop();
    runtime.shutdown_timeout(Duration::ZERO);

    assert_eq!(failures.len(), 1, "{failures:?}");
    let payload: serde_json::Value = serde_json::from_str(&failures[0].payload).unwrap();
    assert_eq!(payload["reason"], "launch");
    assert_eq!(payload["detail"], REFUSAL);

    assert_eq!(
        entries_of_type(&directory, run_id, RUN_DEFERRED_EVENT).len(),
        0,
        "a run that was offered a slot was not deferred"
    );

    fs::remove_dir_all(directory).unwrap();
}
