//! Leaving no daemon behind, whoever started it.
//!
//! A test fixture owns a data directory, not a process. The daemon serving that
//! directory may have been spawned by the fixture, or re-executed by the binary
//! under test when a bare launch auto-started one, or left half-composed by a
//! command that failed — and only the first of those is something the fixture
//! holds a handle to. So teardown reaps whatever daemon exists FOR THAT
//! DIRECTORY, read off the runtime files the daemon publishes there.
//!
//! Three things this deliberately does not do:
//!
//! - It does not run `agens serve stop`. That resolves the operator's
//!   configuration first, and a test that broke the configuration on purpose is
//!   exactly the one whose daemon then cannot be stopped through it. `SIGTERM`
//!   to the published pid is the same signal `serve stop` sends, minus the
//!   dependency on a file the test was allowed to corrupt.
//! - It does not look for processes by name. The invariant is one daemon per
//!   data directory, and a daemon serving a live directory of its own is
//!   somebody else's and legitimate.
//! - It does not trust an absent pid file. A daemon that holds the slot without
//!   having published a pid is composing itself, and abandoning it there is how
//!   a suite leaks a daemon whose data directory is deleted a millisecond
//!   later. The slot lock answers for it, and the verdict has to hold twice.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a reap waits for a daemon to end before killing it outright. Past
/// this it is a leak, not a process whose report anybody will read.
const PATIENCE: Duration = Duration::from_secs(20);

/// How often a wait looks again.
const POLL: Duration = Duration::from_millis(25);

/// How deep under a fixture root a data directory is looked for. Deep enough
/// for `xdg-data/agens` and for the per-command roots the CLI suite builds, and
/// shallow enough that teardown never walks a repository checkout.
const SEARCH_DEPTH: usize = 5;

/// Stops every daemon serving a data directory anywhere under `root`.
///
/// Called before the root is removed. A daemon whose data directory is deleted
/// out from under it now reaps itself, but that takes seconds it should never
/// have to spend — and this is the record of which test owed it.
pub(crate) fn reap_under(root: &Path) {
    for data_directory in data_directories_under(root, SEARCH_DEPTH) {
        reap(&data_directory);
    }
}

/// Stops the daemon serving one data directory, if there is one.
///
/// Waits on the PROCESS, not on the files it publishes. The pid file and the
/// socket are removed while the daemon is still unwinding, so a reap that
/// stopped there would return to a test that then asserts on a process which is
/// alive for a few more milliseconds.
pub(crate) fn reap(data_directory: &Path) {
    let pid_path = data_directory.join("serve.pid");
    let deadline = Instant::now() + PATIENCE;

    // The daemon this reap is responsible for, remembered past the removal of
    // the file that named it.
    let mut daemon = None;
    let mut settled = false;

    loop {
        let published = published_pid(&pid_path);
        if published.is_some() {
            daemon = published;
        }

        match daemon.filter(|pid| alive(*pid)) {
            Some(pid) => {
                settled = false;

                if Instant::now() >= deadline {
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    return;
                }

                // The same signal `serve stop` sends, and only while the daemon
                // still claims the directory: once its pid file is gone it is
                // already on its way out and there is nothing left to ask.
                if published.is_some() {
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                }
            }
            // Nothing running that this reap knows of, but a held slot is a
            // daemon that has not published a pid yet, so it is still this
            // fixture's to stop.
            None if agens_server::slot_is_held(data_directory) => {
                settled = false;

                if Instant::now() >= deadline {
                    return;
                }
            }
            // Nothing serving. Confirmed once more before believing it, so a
            // daemon caught between taking its slot and opening its lock file
            // is not read as an absent one.
            None => {
                if settled {
                    return;
                }

                settled = true;
            }
        }

        std::thread::sleep(POLL);
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn published_pid(pid_path: &Path) -> Option<i32> {
    std::fs::read_to_string(pid_path)
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0 && alive(*pid))
}

/// Every directory under `root` that a daemon has served or is serving, found
/// by the runtime files it publishes there.
fn data_directories_under(root: &Path, depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();

    if root.join("serve.lock").exists() || root.join("serve.pid").exists() {
        found.push(root.to_path_buf());
    }

    if depth == 0 {
        return found;
    }

    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };

    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            found.extend(data_directories_under(&entry.path(), depth - 1));
        }
    }

    found
}
