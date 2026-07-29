//! Launching the terminal surface.
//!
//! The surface itself is `agens-tui-app`. What stays here is the one decision
//! the binary owns: resolving the run's configuration and handing it to the
//! launcher the composition root installed.

use crate::CliDependencies;
use std::path::PathBuf;
use std::sync::Arc;

use crate::deps::bootstrap;
use crate::profile_store::{
    AgentProfileStore as FileProfileStore, ProfileScope as FileProfileScope,
};
use agens_config::AgentProfilePatch;
use agens_error::CliError;
use agens_tui_app::profiles::{AgentProfileStore, ProfileScope};

struct TuiProfileStore(FileProfileStore);

impl AgentProfileStore for TuiProfileStore {
    fn save(
        &self,
        scope: ProfileScope,
        agent: &str,
        patch: &AgentProfilePatch,
    ) -> Result<(), String> {
        let scope = match scope {
            ProfileScope::Global => FileProfileScope::Global,
            ProfileScope::Project => FileProfileScope::Project,
        };
        let snapshot = self.0.read(scope).map_err(|error| error.to_string())?;
        self.0
            .save(scope, &snapshot, agent, patch)
            .map_err(|error| error.to_string())
    }
}

pub(crate) fn run_production_tui(
    bootstrap: &agens_bootstrap::Bootstrap,
    resume: Option<i64>,
) -> Result<String, CliError> {
    let project_root = bootstrap
        .project_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    let store = TuiProfileStore(FileProfileStore::new(
        bootstrap.paths.global_config.clone(),
        project_root.join(".agens/config.toml"),
    ));
    agens_tui_app::engine::run_production_tui_with_profile_store(
        bootstrap,
        resume,
        Some(Arc::new(store)),
    )
}

pub(crate) fn run_tui(
    dependencies: &CliDependencies,
    resume: Option<i64>,
) -> Result<String, CliError> {
    let bootstrap = bootstrap(dependencies)?;
    let output = (dependencies.tui_launcher)(&bootstrap, resume)?;
    Ok(format!("{output}\n"))
}
