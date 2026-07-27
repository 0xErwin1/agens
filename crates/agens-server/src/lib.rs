//! The Agens daemon process.
//!
//! One daemon per machine serves N projects (AGN-80), so nothing here resolves a
//! project: a project only enters through a run. The crate exists apart from the
//! CLI on purpose — the daemon owns the coordinator, its state machines, the
//! scheduler and the timers, and none of that belongs to a command surface.

mod instance;

use std::os::unix::net::UnixListener;

use agens_core::HeadlessTurnCancellation;

pub use instance::{ServeInstance, ServeInstanceError};

#[derive(Debug)]
pub enum ServerError {
    /// Another daemon owns this machine's slot. Its own variant because the
    /// caller must attach rather than start a second process.
    AlreadyRunning,
    Unavailable(&'static str),
}

/// Takes the machine's daemon slot, binds its socket and parks until asked to
/// stop, releasing both on the way out.
pub fn run_until_shutdown(
    data_directory: &std::path::Path,
    shutdown: &HeadlessTurnCancellation,
) -> Result<(), ServerError> {
    let instance = ServeInstance::acquire(data_directory).map_err(|error| match error {
        ServeInstanceError::AlreadyRunning => ServerError::AlreadyRunning,
        ServeInstanceError::Unavailable(_) => ServerError::Unavailable("runtime is unavailable"),
    })?;

    let listener = UnixListener::bind(instance.socket_path())
        .map_err(|_| ServerError::Unavailable("socket is unavailable"))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .map_err(|_| ServerError::Unavailable("runtime is unavailable"))?;

    runtime.block_on(park_until_shutdown(shutdown));

    drop(listener);
    drop(instance);

    Ok(())
}

/// The daemon has no work of its own yet, so it parks on the shared cancellation
/// rather than inventing a second stop path.
async fn park_until_shutdown(shutdown: &HeadlessTurnCancellation) {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

    while !shutdown.is_cancelled() {
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
