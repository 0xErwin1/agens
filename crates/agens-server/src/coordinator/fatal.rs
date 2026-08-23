//! What the coordinator does when the service core is no longer usable.
//!
//! A panic taken while the core is held poisons its lock for the rest of the
//! process. Everything that reaches the core then fails in the same way and
//! forever: admission read the poison as "a tick that did nothing" and slept
//! its backoff, the timer wheel read it as "nothing was due" and kept ticking
//! against a core it would never take again, and the facade went on answering
//! `Status::internal` to every client. The daemon stayed up, held the machine's
//! slot, and did nothing at all.
//!
//! There is no recovery. The invariants the control plane rests on were left
//! half-established by code that did not finish, and no later transition can
//! reason about a state nothing described. So this is terminal by design: it is
//! written down twice and the daemon is asked to stop, which is the one action
//! that returns the machine to a process supervisor able to start a clean one.
//!
//! Written down twice because neither record is sufficient alone. The journal
//! is the control plane's own account and is where an operator looks afterwards
//! — but it needs a store the poisoned core cannot lend, and if the control
//! plane is what broke, the write fails too. The diagnostics log is a file a
//! supervisor is already tailing, and it is the record that survives a control
//! plane that cannot be written to.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agens_core::HeadlessTurnCancellation;
use agens_diagnostics::best_effort;
use agens_store::{ControlPlaneStore, EventClass, EventRow};

use crate::diagnostics::CoordinatorDiagnostics;

/// The service core was left unusable and the daemon is stopping.
pub const CORE_POISONED_EVENT: &str = "core_poisoned";

/// The one place a loop reports that it can no longer take the core.
///
/// Shared by every loop, and idempotent: several loops discover the same poison
/// within a heartbeat of each other, and a supervisor reading a hundred
/// identical lines learns nothing the first one did not say.
pub(super) struct FatalCore {
    data_directory: PathBuf,
    /// The coordinator's own stop, so its loops come down.
    stopping: Arc<AtomicBool>,
    /// The daemon's stop, so the process comes down with them. Without this the
    /// loops end and the facade keeps serving a core no request can take.
    shutdown: HeadlessTurnCancellation,
    diagnostics: CoordinatorDiagnostics,
    reported: AtomicBool,
}

impl FatalCore {
    pub(super) fn new(
        data_directory: &Path,
        stopping: &Arc<AtomicBool>,
        shutdown: &HeadlessTurnCancellation,
        diagnostics: CoordinatorDiagnostics,
    ) -> Self {
        Self {
            data_directory: data_directory.to_path_buf(),
            stopping: Arc::clone(stopping),
            shutdown: shutdown.clone(),
            diagnostics,
            reported: AtomicBool::new(false),
        }
    }

    /// Records that `component` found the core poisoned and stops the daemon.
    ///
    /// `component` names the loop that discovered it, not the code that
    /// poisoned it: the panic happened in whatever was holding the core, and by
    /// the time anything reads the lock that caller is gone.
    pub(super) fn poisoned(&self, component: &str) {
        if self.reported.swap(true, Ordering::AcqRel) {
            return;
        }

        self.diagnostics.core_poisoned(component);
        self.journal(component);

        self.stopping.store(true, Ordering::Release);
        self.shutdown.cancel();
    }

    /// Whether a loop found the core poisoned.
    ///
    /// Read on the way out, because the flag every loop was watching is the
    /// daemon's ordinary stop and says nothing about why it was raised. A
    /// caller that reported success for this would leave a process supervisor
    /// with a machine that has no daemon and no reason to start one.
    pub(super) fn reported(&self) -> bool {
        self.reported.load(Ordering::Acquire)
    }

    /// Appends the one entry that says why this daemon stopped.
    ///
    /// Through a store opened here rather than the machines', because the
    /// machines are behind the lock this is reporting about. It is a second
    /// writer of the control-plane tables for exactly one row, at the moment
    /// the single writer no longer exists.
    fn journal(&self, component: &str) {
        let Ok(mut store) = ControlPlaneStore::open(&self.data_directory) else {
            return;
        };

        best_effort(store.append_event(&EventRow {
            id: None,
            // No run: the core being unusable is a fact about the daemon, and
            // attributing it to whichever run happened to be executing would
            // read as that run having done something.
            run_id: None,
            event_type: CORE_POISONED_EVENT.to_owned(),
            class: EventClass::Infra,
            payload: serde_json::json!({ "component": component }).to_string(),
            ts: super::now(),
        }));
    }
}
