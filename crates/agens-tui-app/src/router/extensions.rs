//! Skills, commands and the palette they populate.
//!
//! A resumed session discovers these against its own root, so they are rebuilt
//! when the session slot changes rather than at startup only.

use std::sync::Arc;

use agens_tools::{CommandCatalog, SkillCatalog};
use agens_tui::PaletteEntry;

use crate::extensions::{discover_tui_command_catalog, resolved_tui_palette};
use crate::files::tui_picker_file_candidates;
use agens_agents::subagent_catalog;
use agens_bootstrap::Bootstrap;
use agens_bootstrap::discover_skill_catalog;
use agens_error::CliError;
use agens_session::context::SessionContext;

use super::TuiRuntimeRouter;

impl TuiRuntimeRouter {
    pub fn skills(&self) -> Result<Arc<SkillCatalog>, CliError> {
        self.extensions
            .lock()
            .map(|extensions| Arc::clone(&extensions.skills))
            .map_err(|_| CliError::storage("TUI extension catalogs are unavailable"))
    }

    pub(super) fn commands(&self) -> Result<Arc<CommandCatalog>, CliError> {
        self.extensions
            .lock()
            .map(|extensions| Arc::clone(&extensions.commands))
            .map_err(|_| CliError::storage("TUI extension catalogs are unavailable"))
    }

    pub fn palette_entries(&self) -> Result<Vec<PaletteEntry>, CliError> {
        self.extensions
            .lock()
            .map(|extensions| extensions.palette.clone())
            .map_err(|_| CliError::storage("TUI extension catalogs are unavailable"))
    }

    /// Re-discovers commands, skills, and the derived palette from the session's OWN recorded
    /// root, after a post-startup resume may have changed that root out from under this router.
    ///
    /// Best-effort: on any discovery failure the previously held catalogs are left in place,
    /// mirroring the same graceful degradation startup already applies (`tui.add_info` there,
    /// nothing to report to here since a resume commit has no `Tui` handle). This must run for
    /// every resume that lands in the live session slot, or the catalogs captured once at startup
    /// keep feeding a DIFFERENT root's skill bodies and command templates into every later turn as
    /// model-facing instruction text.
    pub(super) fn refresh_session_extensions(
        &self,
        bootstrap: &Bootstrap,
        context: &SessionContext,
    ) {
        let Ok(project_root) = agens_session::root::resolve_tui_session_root(context, bootstrap)
        else {
            return;
        };
        let Ok(commands_discovery) = discover_tui_command_catalog(bootstrap, &project_root) else {
            return;
        };
        let Ok(skills_discovery) = discover_skill_catalog(bootstrap, &project_root) else {
            return;
        };
        let commands = Arc::new(commands_discovery.catalog().clone());
        let skills = Arc::new(skills_discovery.catalog().clone());
        let has_subagents =
            subagent_catalog(bootstrap, context).is_ok_and(|mut agents| agents.next().is_some());
        let palette = resolved_tui_palette(&commands, &skills, has_subagents);
        if let Ok(mut extensions) = self.extensions.lock() {
            extensions.commands = commands;
            extensions.skills = skills;
            extensions.palette = palette;
        }
    }

    /// Called by [`commit_tui_session_resume`] exactly once a resume has actually won its commit
    /// race (never on a rejected, stale, or cancelled attempt), with the context that is about to
    /// become the live session. Refreshes every session-scoped derived surface — the command and
    /// skill catalogs, the `@` picker candidate list, and the composer's rendered palette — from
    /// that context's own root, and returns the picker candidates and palette entries for the
    /// `SessionResumed` outcome to apply to the `Tui`.
    pub(super) fn on_session_resume_committed(
        &self,
        bootstrap: &Bootstrap,
        context: &SessionContext,
    ) -> (Vec<String>, Vec<PaletteEntry>) {
        self.refresh_session_extensions(bootstrap, context);
        let file_candidates = tui_picker_file_candidates(context, bootstrap).unwrap_or_default();
        let palette_entries = self.palette_entries().unwrap_or_default();
        (file_candidates, palette_entries)
    }
}
