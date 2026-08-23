//! What a daemon does when its service core is left unusable.
//!
//! Driven by a real effects port that panics while the core is held, because
//! that is the only way the state under test arises: nothing marks a `Mutex`
//! poisoned on purpose, and a test that reached in to set a flag would be
//! asserting about its own fixture rather than about the daemon.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agens_core::HeadlessTurnCancellation;
use agens_server::{
    CORE_POISONED_EVENT, Coordinator, CoordinatorSettings, LaunchError, RunLaunch, RunSession,
    RunWorkerFactory, SessionSupervisor,
};
use agens_store::{ControlPlaneStore, EventRow, RunRow, RunState, WorktreeStatus};

const REPO: &str = "a1b2c3d4e5f60718";
const PROVIDER: &str = "scripted";

/// How long an assertion waits for a loop that ticks on a heartbeat.
const PATIENCE: Duration = Duration::from_secs(10);

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn scratch_directory(kind: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-server-poison-{kind}-{}-{suffix}",
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

/// A queued run with the worktree `CreateRun` provisions: admission reads that
/// column, and a run whose directory is not `active` is never offered a slot,
/// so the launcher this test needs to reach would never be called.
fn queued_run(directory: &std::path::Path) -> RunRow {
    let worktree = directory.join("worktrees").join(REPO).join("agn-186");
    fs::create_dir_all(&worktree).unwrap();

    RunRow {
        id: None,
        repo_id: REPO.to_owned(),
        repo_root: "/home/dev/agens".to_owned(),
        remote_url: None,
        external_ref: Some("agens/AGN-186".to_owned()),
        parent_run_id: None,
        task: "give admission something to try to launch".to_owned(),
        scope: "crates/agens-server/src/coordinator".to_owned(),
        dod: "a poisoned core stops the daemon".to_owned(),
        genesis_paths: None,
        state: RunState::Queued,
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

/// A port that panics while the core is held.
///
/// The worker factory is a real effects port, called by the scheduler from
/// inside the tick that is holding the core, so a panic here leaves exactly the
/// state this test is about: an `ApiCore` nothing can ever take again.
fn panicking_worker() -> RunWorkerFactory {
    std::sync::Arc::new(
        |_launch: &RunLaunch<'_>| -> Result<RunSession, LaunchError> {
            panic!("an effects port gave up while holding the core")
        },
    ) as RunWorkerFactory
}

fn journalled_poisonings(directory: &Path) -> Vec<EventRow> {
    ControlPlaneStore::open(directory)
        .expect("reopen the control plane")
        .events_after(0, 512)
        .expect("read the journal")
        .into_iter()
        .filter(|event| event.event_type == CORE_POISONED_EVENT)
        .collect()
}

fn recorded_diagnostics(directory: &Path) -> String {
    let Ok(entries) = fs::read_dir(directory.join("diagnostics")) else {
        return String::new();
    };

    entries
        .flatten()
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .collect()
}

#[test]
fn a_poisoned_core_stops_the_daemon_instead_of_being_slept_through() {
    let directory = scratch_directory("fatal");

    ControlPlaneStore::open(&directory)
        .expect("open the control plane")
        .insert_run(&queued_run(&directory))
        .expect("insert the run");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let supervisor = SessionSupervisor::new(runtime.handle().clone());
    let shutdown = HeadlessTurnCancellation::new();

    let settings = CoordinatorSettings {
        diagnostics: true,
        ..CoordinatorSettings::default()
    };

    // The panic is deliberate, and its default report would read as a failing
    // test to anyone looking at the output.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let coordinator = Coordinator::start(
        &directory,
        &settings,
        supervisor,
        panicking_worker(),
        &shutdown,
    )
    .expect("the coordinator composes over the data directory");

    let deadline = Instant::now() + PATIENCE;
    while !shutdown.is_cancelled() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    std::panic::set_hook(previous);

    coordinator.stop();
    runtime.shutdown_timeout(Duration::ZERO);

    assert!(
        shutdown.is_cancelled(),
        "the daemon is asked to stop, because a poisoned core has no recovery"
    );

    let journalled = journalled_poisonings(&directory);
    assert_eq!(
        journalled.len(),
        1,
        "one poisoning is one entry, whichever loops noticed it: {journalled:?}"
    );
    assert_eq!(
        journalled[0].run_id, None,
        "the core being unusable is a fact about the daemon, not about a run"
    );

    let recorded = recorded_diagnostics(&directory);
    assert!(
        recorded.contains(r#""event":"core_poisoned""#),
        "the record that survives a control plane nothing can write to: {recorded}"
    );

    fs::remove_dir_all(directory).unwrap();
}
