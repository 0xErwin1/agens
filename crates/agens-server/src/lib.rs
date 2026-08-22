//! The Agens daemon process.
//!
//! One daemon per machine serves N projects (AGN-80), so nothing here resolves a
//! project: a project only enters through a run. The crate exists apart from the
//! CLI on purpose — the daemon owns the coordinator, its state machines, the
//! scheduler and the timers, and none of that belongs to a command surface.

mod blocking;
mod instance;
mod sessions;

use std::os::unix::net::UnixListener;
use std::path::Path;

use agens_core::HeadlessTurnCancellation;

pub use blocking::{BlockingBoundary, BlockingError};
pub use instance::{ServeInstance, ServeInstanceError};
pub use sessions::{
    SessionAdmission, SessionBudget, SessionBudgetHandle, SessionId, SessionLimits, SessionOutcome,
    SessionProvider, SessionRegistry, SessionRegistryError, SessionRuntime, SessionState,
    SessionStatus, SessionSupervisor,
};

#[derive(Debug)]
pub enum ServerError {
    /// Another daemon owns this machine's slot. Its own variant because the
    /// caller must attach rather than start a second process.
    AlreadyRunning,
    Unavailable(&'static str),
}

/// The machine's daemon: its slot, its socket, its runtime, and the sessions
/// living in it.
///
/// Field order is drop order: the runtime stops the sessions' work, the socket
/// closes, and only then does the instance release the slot and remove the
/// socket file a client could still be looking at.
pub struct Daemon {
    runtime: tokio::runtime::Runtime,
    sessions: SessionSupervisor,
    /// Held for its binding, not for reading: the daemon owns its address for
    /// the life of the process, and nothing accepts on it until the client
    /// protocol lands.
    #[allow(dead_code)]
    listener: UnixListener,
    instance: ServeInstance,
}

impl Daemon {
    /// Takes the machine's daemon slot and binds its socket, leaving the process
    /// ready to hold sessions.
    pub fn start(data_directory: &Path) -> Result<Self, ServerError> {
        let instance = ServeInstance::acquire(data_directory).map_err(|error| match error {
            ServeInstanceError::AlreadyRunning => ServerError::AlreadyRunning,
            ServeInstanceError::Unavailable(_) => {
                ServerError::Unavailable("runtime is unavailable")
            }
        })?;

        let listener = UnixListener::bind(instance.socket_path())
            .map_err(|_| ServerError::Unavailable("socket is unavailable"))?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_time()
            .build()
            .map_err(|_| ServerError::Unavailable("runtime is unavailable"))?;
        let sessions = SessionSupervisor::new(runtime.handle().clone());

        Ok(Self {
            runtime,
            sessions,
            listener,
            instance,
        })
    }

    /// Where a client attaches to this daemon.
    pub fn socket_path(&self) -> &Path {
        self.instance.socket_path()
    }

    /// The daemon's sessions. Cloneable, so a client surface holds the same
    /// registry the daemon runs against rather than a copy of its contents.
    pub fn sessions(&self) -> &SessionSupervisor {
        &self.sessions
    }

    /// Parks until asked to stop, then stops every session before releasing the
    /// slot and the socket.
    pub fn run_until_shutdown(&self, shutdown: &HeadlessTurnCancellation) {
        self.runtime.block_on(async {
            park_until_shutdown(shutdown).await;
            self.sessions.cancel_all_and_join().await;
        });
    }
}

/// Takes the machine's daemon slot, binds its socket and parks until asked to
/// stop, releasing both on the way out.
pub fn run_until_shutdown(
    data_directory: &Path,
    shutdown: &HeadlessTurnCancellation,
) -> Result<(), ServerError> {
    Daemon::start(data_directory)?.run_until_shutdown(shutdown);

    Ok(())
}

/// The daemon has no admission surface of its own yet, so it parks on the shared
/// cancellation rather than inventing a second stop path.
async fn park_until_shutdown(shutdown: &HeadlessTurnCancellation) {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

    while !shutdown.is_cancelled() {
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
