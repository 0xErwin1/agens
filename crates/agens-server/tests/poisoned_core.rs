//! What a daemon does when its service core is left unusable.
//!
//! Driven by a real effects port that panics while the core is held, because
//! that is the only way the state under test arises: nothing marks a `Mutex`
//! poisoned on purpose, and a test that reached in to set a flag would be
//! asserting about its own fixture rather than about the daemon.

mod common;

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use agens_core::HeadlessTurnCancellation;
use agens_server::grpc::proto::{self, feed_client::FeedClient};
use agens_server::{
    CORE_POISONED_EVENT, Coordinator, CoordinatorSettings, LaunchError, RunLaunch, RunSession,
    RunWorkerFactory, SessionSupervisor,
};
use agens_store::{ControlPlaneStore, EventRow, RunRow, RunState};
use tonic::transport::{Endpoint, Uri};

use common::{REPO, run_in, scratch_directory, worktree_in};

/// How long an assertion waits for a loop that ticks on a heartbeat.
const PATIENCE: Duration = Duration::from_secs(10);

/// A queued run with the worktree `CreateRun` provisions: admission reads that
/// column, and a run whose directory is not `active` is never offered a slot,
/// so the launcher this test needs to reach would never be called.
fn queued_run(directory: &Path) -> RunRow {
    let worktree = worktree_in(directory, "agn-186");

    RunRow {
        external_ref: Some("agens/AGN-186".to_owned()),
        task: "give admission something to try to launch".to_owned(),
        dod: "a poisoned core stops the daemon".to_owned(),
        ..run_in(RunState::Queued, &worktree)
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

/// A port that refuses every launch until it is armed, and panics after that.
///
/// Refusing keeps the run queued, so the tick that panics is one the test
/// chose: an admission loop offers the same queued run a slot on every
/// heartbeat, and a launch that succeeded would leave nothing to launch again.
fn worker_that_panics_once_armed(armed: Arc<AtomicBool>) -> RunWorkerFactory {
    std::sync::Arc::new(
        move |_launch: &RunLaunch<'_>| -> Result<RunSession, LaunchError> {
            assert!(
                !armed.load(Ordering::Acquire),
                "an effects port gave up while holding the core"
            );

            Err(LaunchError(
                "this launch is not the one under test".to_owned(),
            ))
        },
    ) as RunWorkerFactory
}

/// Waits until the facade answers one request, which is the moment the daemon
/// is serving rather than composing.
fn answer_one_client(socket: &Path) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a client runtime");

    runtime.block_on(async {
        let deadline = Instant::now() + PATIENCE;

        while Instant::now() < deadline {
            if let Ok(channel) = connect(socket).await
                && FeedClient::new(channel)
                    .tree(proto::TreeRequest {
                        repo_id: REPO.to_owned(),
                    })
                    .await
                    .is_ok()
            {
                return;
            }

            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        panic!("the facade never answered a client");
    });
}

async fn connect(socket: &Path) -> Result<tonic::transport::Channel, tonic::transport::Error> {
    let path = socket.to_path_buf();

    Endpoint::try_from("http://localhost")
        .expect("a well-formed authority")
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = path.clone();

            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;

                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
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

/// A daemon that came down on a poisoned core did not stop cleanly, and a
/// process supervisor is the only party that can put a working one back.
///
/// The exit is what reaches it: `Restart=on-failure` reads a status, not a
/// journal, so a fatal stop that returned the same success a clean shutdown
/// returns leaves the machine with no daemon and nothing restarting it. The
/// socket is still released on the way out, because the next daemon has to be
/// able to bind it.
#[test]
fn a_daemon_that_stopped_on_a_poisoned_core_says_so_to_whoever_started_it() {
    let directory = scratch_directory("poison", "serve");

    ControlPlaneStore::open(&directory)
        .expect("open the control plane")
        .insert_run(&queued_run(&directory))
        .expect("insert the run");

    let shutdown = HeadlessTurnCancellation::new();
    let poison = Arc::new(AtomicBool::new(false));

    // Held back until the facade has answered a client, so the poisoning is
    // the one this test is about — a core taken from under a serving daemon —
    // rather than one taken during composition, which is a refusal to start.
    let arming = std::thread::spawn({
        let socket = agens_server::socket_path(&directory);
        let poison = Arc::clone(&poison);

        move || {
            answer_one_client(&socket);
            poison.store(true, Ordering::Release);
        }
    });

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let refused = agens_server::serve_until_shutdown(
        &directory,
        &CoordinatorSettings {
            heartbeat: Duration::from_millis(25),
            ..CoordinatorSettings::default()
        },
        worker_that_panics_once_armed(poison),
        common::refusing_chat(),
        common::refusing_chat_history(),
        &shutdown,
    )
    .expect_err("a daemon whose core was poisoned did not stop cleanly");

    std::panic::set_hook(previous);
    arming.join().expect("the client thread finishes");

    let cause = match &refused {
        agens_server::ServerError::Unavailable(cause) => cause.clone(),
        other => panic!("{other:?}"),
    };

    assert!(
        cause.contains("poisoned"),
        "what stopped the daemon travels to its supervisor: {cause}"
    );
    assert!(
        !agens_server::socket_path(&directory).exists(),
        "the socket is released, so the daemon a supervisor starts next can bind it"
    );
    assert_eq!(
        journalled_poisonings(&directory).len(),
        1,
        "one poisoning is one entry"
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_poisoned_core_stops_the_daemon_instead_of_being_slept_through() {
    let directory = scratch_directory("poison", "fatal");

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
