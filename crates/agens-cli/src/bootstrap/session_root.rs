//! The confinement root a session's tools are opened against.
//!
//! `Bootstrap` only ever knows the process's own discovered project root — the directory
//! containing `.git`, found by walking up from the current working directory at process start.
//! That value is correct for a session that does not exist yet, but it is the wrong source of
//! truth once a session can be resumed: a resumed session must stay confined to the root it was
//! created under, even when the resuming process's current working directory differs from it.
//!
//! [`Bootstrap::discovered_root`](crate::bootstrap::Bootstrap) is visible only to this module, so
//! every other call site that needs a filesystem root for tool confinement must go through one of
//! the constructors here instead. That makes it a compile error to reach the process-wide
//! discovered root from a session code path by accident.

use std::path::{Path, PathBuf};

use crate::bootstrap::Bootstrap;
use crate::error::CliError;
use crate::tui::session::TuiSessionContext;

/// The literal filesystem root a session's native tools must be confined to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionRoot(PathBuf);

impl SessionRoot {
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn into_path_buf(self) -> PathBuf {
        self.0
    }

    /// The root for a session that has not been created yet: the process's own discovered
    /// project root. This is the only legitimate source for a brand-new session, since no
    /// recorded root can exist before the session's first persisted attempt.
    pub(crate) fn discover_for_new_session(bootstrap: &Bootstrap) -> Option<Self> {
        bootstrap
            .discovered_root()
            .map(|root| Self(root.to_path_buf()))
    }
}

/// Resolves the root a resumed or in-progress TUI session's tools must be confined to: the
/// value recorded on the session when it was loaded, or the process's own discovered root for a
/// session that has not been created yet.
pub(crate) fn resolve_tui_session_root(
    context: &TuiSessionContext,
    bootstrap: &Bootstrap,
) -> Result<PathBuf, CliError> {
    context
        .confinement_root
        .clone()
        .or_else(|| {
            SessionRoot::discover_for_new_session(bootstrap).map(SessionRoot::into_path_buf)
        })
        .ok_or_else(|| CliError::configuration("native tools require a project root"))
}

/// Test-only escape hatch for fixtures that only need "some root matching the process's own
/// discovered project" and have no session to resolve one from.
#[cfg(test)]
pub(crate) fn discovered_root_for_tests(bootstrap: &Bootstrap) -> PathBuf {
    bootstrap
        .discovered_root()
        .expect("test bootstrap fixtures always discover a project root")
        .to_path_buf()
}
