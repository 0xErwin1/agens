//! Resolving which root a live session's tools are confined to.
//!
//! The `SessionRoot` type itself lives in `agens-bootstrap`; this is the part
//! that needs a session to resolve from, which is why it stayed behind.

use std::path::PathBuf;

use agens_bootstrap::{Bootstrap, session_root::SessionRoot};
use agens_error::CliError;

use crate::context::SessionContext;

/// Resolves the root a resumed or in-progress TUI session's tools must be confined to: the
/// value recorded on the session when it was loaded, or the process's own discovered root for a
/// session that has not been created yet.
pub fn resolve_tui_session_root(
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

/// Resolves where a session's tools should reopen: the directory a tool moved
/// them to, or the confinement root when nothing has moved them.
///
/// A directory that has since gone away is not resolved here. The tools refuse
/// to reopen it and stay at the root, which is the same answer this would
/// give and is reached without a second filesystem check.
pub fn resolve_tui_working_directory(
    context: &SessionContext,
    bootstrap: &Bootstrap,
) -> Result<PathBuf, CliError> {
    match context.working_directory.clone() {
        Some(directory) => Ok(directory),
        None => resolve_tui_session_root(context, bootstrap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agens_fixtures::{session_bootstrap, session_directory};

    #[test]
    fn a_session_that_has_not_moved_reopens_at_its_confinement_root() {
        let temporary = session_directory("session-working-directory-root");
        let bootstrap = session_bootstrap(&temporary, &[]);
        let mut context = SessionContext::fresh();
        context.confinement_root = Some(temporary.join("project"));

        assert_eq!(
            resolve_tui_working_directory(&context, &bootstrap).unwrap(),
            temporary.join("project")
        );
    }

    #[test]
    fn a_session_that_moved_reopens_where_it_was_left() {
        let temporary = session_directory("session-working-directory-moved");
        let bootstrap = session_bootstrap(&temporary, &[]);
        let mut context = SessionContext::fresh();
        context.confinement_root = Some(temporary.join("project"));
        context.working_directory = Some(temporary.join("project/nested"));

        assert_eq!(
            resolve_tui_working_directory(&context, &bootstrap).unwrap(),
            temporary.join("project/nested")
        );
    }
}
