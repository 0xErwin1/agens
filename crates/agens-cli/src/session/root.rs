//! Resolving which root a live session's tools are confined to.
//!
//! The `SessionRoot` type itself lives in `agens-bootstrap`; this is the part
//! that needs a session to resolve from, which is why it stayed behind.

use std::path::PathBuf;

use agens_bootstrap::{Bootstrap, session_root::SessionRoot};
use agens_error::CliError;

use crate::session::context::SessionContext;

/// Resolves the root a resumed or in-progress TUI session's tools must be confined to: the
/// value recorded on the session when it was loaded, or the process's own discovered root for a
/// session that has not been created yet.
pub(crate) fn resolve_tui_session_root(
    context: &SessionContext,
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
