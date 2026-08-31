//! A daemon whose data directory was removed underneath it stops itself.
//!
//! The orphans this exists for were real: `agens serve --foreground` processes
//! alive for a day against `/tmp` directories that no longer existed, holding a
//! tokio runtime, a bound socket and an open WAL against deleted paths, serving
//! nobody. Nothing could reach them — the socket they were bound to had no name
//! left — and nothing brought them down, because the only stop the daemon had
//! was one an operator reaches through the very files that were gone.
//!
//! So the daemon watches the socket it bound. Not for existence alone: a socket
//! file at the same path with a different inode is a different daemon's world,
//! and this one's is still gone.

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::{fs, thread};

use agens_core::HeadlessTurnCancellation;
use agens_server::Daemon;

/// The daemon checks on a slow cadence and needs several consecutive losses, so
/// a test waits out that budget with room rather than a fixed sleep.
const PATIENCE: Duration = Duration::from_secs(30);

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "agens-server-vanished-{}-{suffix}",
        std::process::id()
    ))
}

#[test]
fn a_daemon_whose_data_directory_was_removed_stops_itself() {
    let directory = data_directory();
    let daemon = Daemon::start(&directory).unwrap();
    let socket = daemon.socket_path().to_path_buf();
    assert!(socket.exists(), "the daemon bound its socket");

    let shutdown = HeadlessTurnCancellation::new();
    let watched = shutdown.clone();
    let running = thread::spawn(move || daemon.run_until_shutdown(&watched));

    fs::remove_dir_all(&directory).expect("remove the daemon's world");

    let deadline = Instant::now() + PATIENCE;
    while !running.is_finished() {
        assert!(
            Instant::now() < deadline,
            "the daemon outlived the data directory it was serving"
        );
        thread::sleep(Duration::from_millis(50));
    }

    running.join().expect("the daemon stopped cleanly");
}

/// The dangerous inverse: a healthy daemon must not be stopped by a check that
/// happened to look while the socket was momentarily unreadable. The socket
/// here never changes identity, so however many checks run, none of them may
/// add up to a shutdown.
#[test]
fn a_daemon_whose_socket_is_unchanged_keeps_serving() {
    let directory = data_directory();
    let daemon = Daemon::start(&directory).unwrap();
    let socket = daemon.socket_path().to_path_buf();
    let bound = fs::symlink_metadata(&socket).expect("the socket is there");

    let shutdown = HeadlessTurnCancellation::new();
    let watched = shutdown.clone();
    let running = thread::spawn(move || daemon.run_until_shutdown(&watched));

    // Long enough for several checks on the daemon's cadence to have run.
    thread::sleep(Duration::from_secs(8));

    assert!(
        !running.is_finished(),
        "a daemon whose world is intact went on serving"
    );
    let still = fs::symlink_metadata(&socket).expect("the socket is still there");
    assert_eq!((bound.dev(), bound.ino()), (still.dev(), still.ino()));

    shutdown.cancel();
    running.join().expect("the daemon stopped cleanly");
    let _ = fs::remove_dir_all(&directory);
}
