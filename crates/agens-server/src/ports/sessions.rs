//! Cancelling, taking over and stopping the sessions a run executes in.
//!
//! The mapping from a run to the session executing it is not held in memory: it
//! is the run's live attempt, which the admission transition wrote and which
//! survives a restart. A registry consulted instead would disagree with it the
//! moment the daemon came back up.
//!
//! The store here is a second handle, opened by the composition root and read
//! from only. The state machines stay the single writer of these tables, and a
//! port that took the core's lock to read them would deadlock the daemon: the
//! core is already holding it while it performs the effect.

use std::sync::Mutex;

use agens_store::{ControlPlaneStore, RunState};

use crate::api::{PortError, SessionControl, StopScope, TakeoverHandle};
use crate::sessions::{SessionId, SessionRegistryError, SessionSupervisor};

/// The daemon's sessions, addressed by run.
pub(crate) struct SupervisedSessions {
    supervisor: SessionSupervisor,
    store: Mutex<ControlPlaneStore>,
}

impl SupervisedSessions {
    #[must_use]
    pub(crate) const fn new(supervisor: SessionSupervisor, store: ControlPlaneStore) -> Self {
        Self {
            supervisor,
            store: Mutex::new(store),
        }
    }

    /// The session a run is executing in right now, or `None` when it is not
    /// executing.
    ///
    /// The last attempt is the live one, and an attempt that already ended
    /// names no session to reach: a run that is not running is not cancelled by
    /// signalling whichever session ran it last.
    fn live_session(&self, run_id: i64) -> Result<Option<SessionId>, PortError> {
        let store = self.locked()?;

        let Some(run) = store.load_run(run_id).map_err(storage)? else {
            return Err(PortError::new(
                "sessions",
                format!("no run with id {run_id}"),
            ));
        };

        if run.state != RunState::Running {
            return Ok(None);
        }

        Ok(store
            .attempts_for_run(run_id)
            .map_err(storage)?
            .last()
            .filter(|attempt| attempt.ended_at.is_none())
            .and_then(|attempt| attempt.session_id)
            .map(SessionId::new))
    }

    /// The session a run was last executing in, whatever state the run is in
    /// now.
    ///
    /// Read without the liveness filter [`Self::live_session`] applies, because
    /// a suspension is performed after the transition that parked the run: by
    /// then the run has left `running` and the attempt it ran under is closed,
    /// and the session that is still burning a provider is precisely the one
    /// that attempt names.
    fn last_session(&self, run_id: i64) -> Result<Option<SessionId>, PortError> {
        let store = self.locked()?;

        Ok(store
            .attempts_for_run(run_id)
            .map_err(storage)?
            .last()
            .and_then(|attempt| attempt.session_id)
            .map(SessionId::new))
    }

    /// Every run of one repository that is executing right now.
    fn live_sessions_in(&self, repo_id: &str) -> Result<Vec<SessionId>, PortError> {
        let runs = self
            .locked()?
            .runs_for_repo(repo_id)
            .map_err(storage)?
            .into_iter()
            .filter(|run| run.state == RunState::Running)
            .filter_map(|run| run.id)
            .collect::<Vec<_>>();

        let mut sessions = Vec::new();
        for run_id in runs {
            if let Some(session) = self.live_session(run_id)? {
                sessions.push(session);
            }
        }

        Ok(sessions)
    }

    /// Cancels one session, treating a session that is already gone as the
    /// state the caller asked for rather than as a failure.
    fn cancel_session(&self, session: SessionId) -> Result<(), PortError> {
        match self.supervisor.cancel(session) {
            Ok(()) | Err(SessionRegistryError::Unknown | SessionRegistryError::Terminal) => Ok(()),
            Err(error) => Err(PortError::new("sessions", describe(error))),
        }
    }

    fn locked(&self) -> Result<std::sync::MutexGuard<'_, ControlPlaneStore>, PortError> {
        self.store.lock().map_err(|_| {
            PortError::new(
                "sessions",
                "the control plane is unusable after a failed read",
            )
        })
    }
}

impl SessionControl for SupervisedSessions {
    /// Cancels the run's session immediately. A run with none is already as
    /// stopped as cancelling it would make it, so nothing is reported.
    fn cancel(&self, run_id: i64) -> Result<(), PortError> {
        match self.live_session(run_id)? {
            Some(session) => self.cancel_session(session),
            None => Ok(()),
        }
    }

    /// Stops the session a parked run left behind. A run whose session already
    /// ended is already as suspended as this would make it.
    fn suspend(&self, run_id: i64) -> Result<(), PortError> {
        match self.last_session(run_id)? {
            Some(session) => self.cancel_session(session),
            None => Ok(()),
        }
    }

    fn take_over(&self, run_id: i64) -> Result<TakeoverHandle, PortError> {
        let session = self.live_session(run_id)?.ok_or_else(|| {
            PortError::new(
                "sessions",
                format!("run {run_id} is not executing, so there is no session to take over"),
            )
        })?;

        Ok(TakeoverHandle {
            run_id,
            session_id: session.value(),
        })
    }

    /// Stops what the scope reaches.
    ///
    /// The machine scope goes through the registry rather than through the
    /// control plane: stopping the daemon has to reach every session it holds,
    /// including one whose run the store has not caught up with.
    fn stop(&self, scope: &StopScope) -> Result<(), PortError> {
        match scope {
            StopScope::Run(run_id) => self.cancel(*run_id),
            StopScope::Repo(repo_id) => {
                for session in self.live_sessions_in(repo_id)? {
                    self.cancel_session(session)?;
                }

                Ok(())
            }
            StopScope::Machine => {
                self.supervisor.registry().cancel_all();

                Ok(())
            }
        }
    }
}

fn storage(error: agens_store::ControlPlaneError) -> PortError {
    PortError::new("sessions", error.to_string())
}

const fn describe(error: SessionRegistryError) -> &'static str {
    match error {
        SessionRegistryError::AlreadyLive => "a session with this id is already live",
        SessionRegistryError::AtCapacity => "the daemon holds as many sessions as it admits",
        SessionRegistryError::Unknown => "no such session",
        SessionRegistryError::Terminal => "the session already ended",
    }
}
