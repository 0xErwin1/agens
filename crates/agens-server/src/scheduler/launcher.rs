//! Starting an admitted run through the daemon's session supervisor.
//!
//! This is the whole of the scheduler's contact with execution. What a session
//! is built from — its own provider client, its confinement root, its own MCP
//! connections — stays with the caller that supplies [`RunSession`], because
//! those are the per-session pieces the daemon must never share between peers
//! and the scheduler has no business deciding.

use crate::sessions::{
    SessionAdmission, SessionId, SessionOutcome, SessionRegistryError, SessionRuntime,
    SessionSupervisor,
};

use super::{LaunchError, LaunchedSession, PendingRun, RunLauncher};

/// What starting one run's session takes.
pub struct RunSession {
    pub admission: SessionAdmission,
    /// The work the session runs, handed its runtime by value.
    pub work: Box<dyn FnOnce(SessionRuntime) -> SessionOutcome + Send + 'static>,
    /// The physical execution row this attempt is recorded against, when the
    /// caller has already opened one.
    pub session_attempt_id: Option<i64>,
}

/// A [`RunLauncher`] over the daemon's supervisor.
///
/// Takes a factory rather than building sessions itself. Everything a session
/// needs that only the composition root can resolve — the bootstrap, the
/// provider client, the worktree the run executes in — is decided there, and
/// keeping it outside admission is what makes adding a wire surface a change to
/// who asks rather than to who decides what runs.
pub struct SupervisorLauncher<F> {
    supervisor: SessionSupervisor,
    session_for: F,
}

impl<F> SupervisorLauncher<F>
where
    F: Fn(&PendingRun<'_>) -> Result<RunSession, LaunchError>,
{
    pub const fn new(supervisor: SessionSupervisor, session_for: F) -> Self {
        Self {
            supervisor,
            session_for,
        }
    }
}

impl<F> RunLauncher for SupervisorLauncher<F>
where
    F: Fn(&PendingRun<'_>) -> Result<RunSession, LaunchError>,
{
    fn launch(&self, pending: &PendingRun<'_>) -> Result<LaunchedSession, LaunchError> {
        let RunSession {
            admission,
            work,
            session_attempt_id,
        } = (self.session_for)(pending)?;

        let session = self
            .supervisor
            .start(admission, work)
            .map_err(|error| LaunchError(describe(error).to_owned()))?;

        Ok(LaunchedSession {
            session,
            session_attempt_id,
        })
    }

    /// Cancels the session. A cancellation that finds nothing to cancel is the
    /// state this wanted anyway, so it is not reported back as a second
    /// failure.
    fn abandon(&self, session: SessionId) {
        let _ = self.supervisor.cancel(session);
    }
}

const fn describe(error: SessionRegistryError) -> &'static str {
    match error {
        SessionRegistryError::AlreadyLive => "a session with this id is already live",
        SessionRegistryError::AtCapacity => "the daemon holds as many sessions as it admits",
        SessionRegistryError::Unknown => "no such session",
        SessionRegistryError::Terminal => "the session already ended",
    }
}
