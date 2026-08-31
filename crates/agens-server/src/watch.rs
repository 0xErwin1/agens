//! The daemon noticing that the world it serves is gone.
//!
//! A daemon is a slot, a socket and a data directory, and it has exactly one
//! stop: a flag something outside raises, which `serve stop` reaches by
//! signalling the pid published under that data directory. Remove the directory
//! and there is no pid to read, no socket to connect to, and no way left to ask
//! the process to stop — so it does not. Four such processes were found alive
//! for a day, each holding a tokio runtime, a bound socket and an open WAL
//! against paths that no longer existed, serving nobody.
//!
//! Existence is the wrong question to ask about the socket. The daemon bound
//! one specific socket, and a file that appears at the same path afterwards
//! belongs to something else: it is the identity of the file — its device and
//! inode, recorded when it was bound — that says whether this daemon's world is
//! still there. Only this daemon may create that path while it holds the slot,
//! so a change of identity is as final as a removal.
//!
//! The cost of being wrong is a healthy daemon killed by a filesystem that
//! stuttered, so the check is deliberately slow and deliberately repeated: a
//! single failure decides nothing, and the loss has to hold across several
//! checks spaced seconds apart before the daemon stops.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agens_core::HeadlessTurnCancellation;

use crate::diagnostics::CoordinatorDiagnostics;

/// How long between two looks at the socket this daemon bound.
///
/// Seconds rather than milliseconds: nothing acts on the answer quickly, and
/// the loop must not be a reason to wake a daemon that is otherwise idle.
const CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// How many consecutive losses stand for a world that is actually gone.
///
/// A daemon that stops itself takes its sessions with it, so one unlucky read
/// may not be enough to do it. Three checks two seconds apart is six seconds of
/// a socket that is continuously missing or continuously somebody else's, which
/// no transient failure survives and no real removal fails.
const CONSECUTIVE_LOSSES: usize = 3;

/// What one look at the socket found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    /// The socket this daemon bound is still the file at that path.
    Present,
    /// It is not, for the named reason. The reason is one of a closed set: it
    /// goes into the diagnostics file, which carries no path.
    Lost(&'static str),
}

/// The identity of the socket a daemon bound, taken once while it was bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl SocketIdentity {
    fn of(socket_path: &Path) -> Option<Self> {
        let metadata = std::fs::symlink_metadata(socket_path).ok()?;

        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

/// The consecutive-loss rule, apart from the clock and the filesystem so it can
/// be stated and tested as what it is.
#[derive(Debug, Default)]
struct LossRun {
    consecutive: usize,
}

impl LossRun {
    /// Folds one check in, and names the reason once the run is long enough to
    /// act on. Reports at most once: the caller stops the daemon on it.
    fn observe(&mut self, check: Check) -> Option<&'static str> {
        match check {
            Check::Present => {
                self.consecutive = 0;
                None
            }
            Check::Lost(reason) => {
                self.consecutive += 1;

                (self.consecutive == CONSECUTIVE_LOSSES).then_some(reason)
            }
        }
    }
}

/// The daemon watching its own socket, and stopping the daemon when it is gone.
///
/// The stop is the ordinary one — the same flag `serve stop` raises through a
/// signal — so the sessions get the teardown they would have got from an
/// operator rather than a process that disappears under them.
pub(crate) struct RuntimeWatch {
    socket_path: PathBuf,
    /// Taken while the socket was bound. `None` when it could not be read at
    /// all, which turns the watch off rather than letting it compare against a
    /// nothing every later check would differ from.
    bound: Option<SocketIdentity>,
    diagnostics: Option<CoordinatorDiagnostics>,
}

impl RuntimeWatch {
    /// Records what the daemon just bound, so every later check has something
    /// to be the same as.
    pub(crate) fn of_bound_socket(socket_path: &Path) -> Self {
        Self {
            socket_path: socket_path.to_path_buf(),
            bound: SocketIdentity::of(socket_path),
            diagnostics: None,
        }
    }

    /// Gives the watch the daemon's diagnostics handle, so the reason it
    /// stopped is written where a supervisor already reads.
    pub(crate) fn reporting_to(mut self, diagnostics: CoordinatorDiagnostics) -> Self {
        self.diagnostics = Some(diagnostics);
        self
    }

    fn check(&self) -> Check {
        let Some(bound) = self.bound else {
            return Check::Present;
        };

        match SocketIdentity::of(&self.socket_path) {
            None => Check::Lost("socket_missing"),
            Some(found) if found == bound => Check::Present,
            Some(_) => Check::Lost("socket_replaced"),
        }
    }

    /// Watches until the daemon is stopping for any reason, stopping it itself
    /// if the socket it bound stays gone.
    pub(crate) async fn run(self, shutdown: HeadlessTurnCancellation) {
        let mut run = LossRun::default();

        while !shutdown.is_cancelled() {
            tokio::time::sleep(CHECK_INTERVAL).await;

            if shutdown.is_cancelled() {
                return;
            }

            if let Some(reason) = run.observe(self.check()) {
                if let Some(diagnostics) = &self.diagnostics {
                    diagnostics.runtime_lost(reason, CONSECUTIVE_LOSSES);
                }

                shutdown.cancel();

                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_that_is_still_the_bound_one_is_present() {
        let directory = std::env::temp_dir().join(format!("agens-watch-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("serve.sock");
        std::fs::write(&socket, "bound").unwrap();

        let watch = RuntimeWatch::of_bound_socket(&socket);

        assert_eq!(watch.check(), Check::Present);

        std::fs::remove_file(&socket).unwrap();
        assert_eq!(watch.check(), Check::Lost("socket_missing"));

        // A file at the same path is a different file, and this daemon's world
        // is still gone.
        std::fs::write(&socket, "somebody else").unwrap();
        assert_eq!(watch.check(), Check::Lost("socket_replaced"));

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn one_loss_that_recovers_stops_nothing() {
        let mut run = LossRun::default();

        assert_eq!(run.observe(Check::Lost("socket_missing")), None);
        assert_eq!(run.observe(Check::Present), None);

        // The run started over, so the next two losses are not the third one.
        assert_eq!(run.observe(Check::Lost("socket_missing")), None);
        assert_eq!(run.observe(Check::Lost("socket_missing")), None);
    }

    #[test]
    fn a_loss_that_holds_across_the_whole_run_reports_its_reason() {
        let mut run = LossRun::default();

        for _ in 1..CONSECUTIVE_LOSSES {
            assert_eq!(run.observe(Check::Lost("socket_missing")), None);
        }

        assert_eq!(
            run.observe(Check::Lost("socket_missing")),
            Some("socket_missing")
        );
    }
}
