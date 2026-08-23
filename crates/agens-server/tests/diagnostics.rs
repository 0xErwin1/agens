//! What a supervisor reading the diagnostics log learns about the daemon.
//!
//! Driven through a composed coordinator and asserted by reading the file back,
//! because the whole point of these lines is that they are readable by
//! something that never attached a client.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agens_core::HeadlessTurnCancellation;
use agens_server::{Coordinator, CoordinatorSettings, SessionSupervisor};
use agens_store::{AttemptRow, ControlPlaneStore, RunRow, RunState};

use common::{
    REFUSAL, REPO_ROOT, SCOPE, now, refusing_worker, run_in, scratch_directory, worktree_in,
};

/// How long an assertion waits for a loop that ticks on a heartbeat.
const PATIENCE: Duration = Duration::from_secs(10);

/// A run the last process was executing when it died, so boot reconciliation
/// has something to interrupt and resume through the state machines.
fn interrupted_run(worktree: &Path) -> RunRow {
    RunRow {
        external_ref: Some("agens/AGN-186".to_owned()),
        task: "give a supervisor something to read".to_owned(),
        dod: "the coordinator's own events reach the diagnostics log".to_owned(),
        ..run_in(RunState::Running, worktree)
    }
}

/// Seeds a directory with a run a killed daemon left behind and returns it.
fn seeded_directory(kind: &str) -> PathBuf {
    let directory = scratch_directory("diagnostics", kind);
    let worktree = worktree_in(&directory, "agn-186");

    let mut store = ControlPlaneStore::open(&directory).expect("open the control plane");
    let run_id = store
        .insert_run(&interrupted_run(&worktree))
        .expect("insert the run");
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

    directory
}

/// Every diagnostics line this daemon has written so far.
fn recorded(directory: &Path) -> Vec<serde_json::Value> {
    let Ok(entries) = fs::read_dir(directory.join("diagnostics")) else {
        return Vec::new();
    };

    let mut lines = Vec::new();

    for entry in entries.flatten() {
        let Ok(contents) = fs::read_to_string(entry.path()) else {
            continue;
        };

        lines.extend(
            contents
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()),
        );
    }

    lines
}

fn await_event(directory: &Path, event: &str) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + PATIENCE;

    loop {
        let lines: Vec<serde_json::Value> = recorded(directory)
            .into_iter()
            .filter(|line| line["event"] == event)
            .collect();

        if !lines.is_empty() || Instant::now() >= deadline {
            return lines;
        }

        std::thread::sleep(Duration::from_millis(20));
    }
}

fn coordinator_over(
    directory: &Path,
    diagnostics: bool,
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

    let settings = CoordinatorSettings {
        diagnostics,
        ..CoordinatorSettings::default()
    };
    let shutdown = HeadlessTurnCancellation::new();
    let coordinator = Coordinator::start(
        directory,
        &settings,
        supervisor,
        refusing_worker(),
        &shutdown,
    )
    .expect("the coordinator composes over the data directory");

    (coordinator, runtime, shutdown)
}

#[test]
fn the_coordinator_records_what_it_moved_and_what_it_could_not_start() {
    let directory = seeded_directory("recording");
    let (coordinator, runtime, _shutdown) = coordinator_over(&directory, true);

    let moved = await_event(&directory, "run_state_changed");
    let failed = await_event(&directory, "admission_failed");

    coordinator.stop();
    runtime.shutdown_timeout(Duration::ZERO);

    assert!(
        !moved.is_empty(),
        "a run the daemon moved is a run a supervisor can follow"
    );
    let move_line = &moved[0];
    assert_eq!(move_line["component"], "coordinator");
    assert_eq!(move_line["scope"], "parent");
    assert_eq!(move_line["machine"], "run");
    assert!(move_line["run"].is_i64());
    assert!(move_line["reference"].as_str().is_some_and(|reference| {
        reference.len() == 8 && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
    }));

    assert!(!failed.is_empty(), "a launch that did not work is recorded");
    assert_eq!(failed[0]["component"], "coordinator");
    assert_eq!(failed[0]["reason"], "launch");

    // One daemon, one reference: a supervisor correlating this file must not
    // see the coordinator as several unrelated components.
    let references: std::collections::BTreeSet<String> = recorded(&directory)
        .iter()
        .filter(|line| line["component"] == "coordinator")
        .filter_map(|line| line["reference"].as_str().map(ToOwned::to_owned))
        .collect();
    assert_eq!(references.len(), 1, "{references:?}");

    // Nothing a person wrote travels: the run's repository, its scope and the
    // worker's own refusal stay in the journal, which is not the file the audit
    // overlay reads.
    let whole = fs::read_dir(directory.join("diagnostics"))
        .unwrap()
        .flatten()
        .map(|entry| fs::read_to_string(entry.path()).unwrap())
        .collect::<String>();
    assert!(!whole.contains(REPO_ROOT), "{whole}");
    assert!(!whole.contains(SCOPE), "{whole}");
    assert!(!whole.contains(REFUSAL), "{whole}");

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_daemon_that_did_not_ask_for_capture_writes_no_file() {
    let directory = seeded_directory("silent");
    let (coordinator, runtime, _shutdown) = coordinator_over(&directory, false);

    // Long enough for every loop to have ticked several times.
    std::thread::sleep(Duration::from_millis(1_500));

    coordinator.stop();
    runtime.shutdown_timeout(Duration::ZERO);

    assert!(
        recorded(&directory).is_empty(),
        "capture is off, so nothing about this daemon is on disk"
    );
    assert!(!directory.join("diagnostics").exists());

    fs::remove_dir_all(directory).unwrap();
}
