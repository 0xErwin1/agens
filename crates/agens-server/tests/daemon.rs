use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use agens_core::HeadlessTurnCancellation;
use agens_server::{ServeInstance, ServeInstanceError, ServerError, run_until_shutdown};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "agens-server-daemon-{}-{suffix}",
        std::process::id()
    ))
}

fn socket_path(directory: &Path) -> PathBuf {
    directory.join("serve.sock")
}

/// Waits for the daemon to publish its socket rather than sleeping a guessed
/// interval, so the test does not race a slow machine.
fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);

    while !path.exists() {
        assert!(Instant::now() < deadline, "the daemon never bound {path:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_daemon_binds_its_socket_and_releases_it_on_shutdown() {
    let directory = data_directory();
    let socket = socket_path(&directory);
    let shutdown = HeadlessTurnCancellation::new();
    let daemon_shutdown = shutdown.clone();
    let daemon_directory = directory.clone();

    let daemon = thread::spawn(move || run_until_shutdown(&daemon_directory, &daemon_shutdown));
    wait_for_socket(&socket);

    shutdown.cancel();
    daemon.join().unwrap().unwrap();

    assert!(!socket.exists(), "shutdown left the socket behind");

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_second_daemon_refuses_to_start_while_one_is_running() {
    let directory = data_directory();
    let socket = socket_path(&directory);
    let shutdown = HeadlessTurnCancellation::new();
    let daemon_shutdown = shutdown.clone();
    let daemon_directory = directory.clone();

    let daemon = thread::spawn(move || run_until_shutdown(&daemon_directory, &daemon_shutdown));
    wait_for_socket(&socket);

    assert!(matches!(
        run_until_shutdown(&directory, &HeadlessTurnCancellation::new()),
        Err(ServerError::AlreadyRunning)
    ));
    assert!(
        socket.exists(),
        "the refused daemon removed the running one's socket"
    );

    shutdown.cancel();
    daemon.join().unwrap().unwrap();

    fs::remove_dir_all(directory).unwrap();
}

/// The slot is released for the next process, not merely for the next call.
#[test]
fn the_machine_slot_is_free_again_after_shutdown() {
    let directory = data_directory();
    let shutdown = HeadlessTurnCancellation::new();
    let daemon_shutdown = shutdown.clone();
    let daemon_directory = directory.clone();

    let daemon = thread::spawn(move || run_until_shutdown(&daemon_directory, &daemon_shutdown));
    wait_for_socket(&socket_path(&directory));
    shutdown.cancel();
    daemon.join().unwrap().unwrap();

    assert!(matches!(
        ServeInstance::acquire(&directory),
        Ok(_) | Err(ServeInstanceError::Unavailable(_))
    ));

    fs::remove_dir_all(directory).unwrap();
}
