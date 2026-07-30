//! Which agents a session may use.
//!
//! The catalog is the built-in primary agent plus whatever the project and the
//! global configuration define, resolved against the session's own root rather
//! than the root of the process that happens to be running.

use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_bootstrap::Bootstrap;
use agens_core::{AgentDefinition, HeadlessTurnError};
use agens_error::CliError;
use agens_session::context::AgentRotationError;
use agens_session::context::SessionContext;
use agens_session::provider::ProviderKind;
use agens_tools::{AgentCatalog, AgentModelValidator};

pub fn select_subagent(
    bootstrap: &Bootstrap,
    name: &str,
    session: &Arc<Mutex<SessionContext>>,
) -> Result<String, CliError> {
    let snapshot = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?
        .clone();
    let agents = subagent_catalog(bootstrap, &snapshot)?.collect::<Vec<_>>();
    if agents.is_empty() {
        return Err(CliError::usage("No eligible subagents are available."));
    }
    let agent = agents
        .into_iter()
        .find(|agent| agent.name == name.trim())
        .ok_or_else(|| CliError::usage("/subagent requires an available subagent"))?;
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    if context.running {
        return Err(CliError::runtime(HeadlessTurnError::State));
    }
    context.selected_subagent = Some(agent.name.clone());
    Ok(format!("Subagent: {}.", agent.name))
}

pub fn subagent_catalog(
    bootstrap: &Bootstrap,
    context: &SessionContext,
) -> Result<impl Iterator<Item = AgentDefinition>, CliError> {
    if bootstrap
        .provider_type()
        .and_then(ProviderKind::parse)
        .is_none()
    {
        return Ok(Vec::new().into_iter());
    }

    let agents = agent_catalog_for_context(bootstrap, context)?
        .subagents()
        .filter(|agent| agent.mode == agens_core::AgentMode::Subagent)
        .cloned()
        .collect::<Vec<_>>();
    Ok(agents.into_iter())
}

pub fn agent_catalog(
    bootstrap: &Bootstrap,
    project_root: &Path,
    validator: &dyn AgentModelValidator,
) -> Result<AgentCatalog, CliError> {
    discover_agent_catalog(bootstrap, project_root, Some(validator))
}

/// Resolves the session's own recorded root (falling back to the process's discovered root for a
/// session that has not been created yet), so a resumed session's agent catalog reflects its own
/// project-local `agents/` directory rather than the resuming process's.
pub fn agent_catalog_for_context(
    bootstrap: &Bootstrap,
    context: &SessionContext,
) -> Result<AgentCatalog, CliError> {
    let project_root = agens_session::root::resolve_tui_session_root(context, bootstrap)?;
    discover_agent_catalog(bootstrap, &project_root, None)
}

pub fn task_agent_catalog(
    bootstrap: &Bootstrap,
    project_root: &Path,
) -> Result<AgentCatalog, CliError> {
    discover_agent_catalog(bootstrap, project_root, None)
}

pub fn discover_agent_catalog(
    bootstrap: &Bootstrap,
    project_root: &Path,
    validator: Option<&dyn AgentModelValidator>,
) -> Result<AgentCatalog, CliError> {
    let session_root =
        agens_bootstrap::session_root::SessionRoot::confined_to(project_root.to_path_buf());
    let system_prompt =
        agens_bootstrap::session_config::SessionConfig::resolve(&session_root, bootstrap)?
            .system_prompt()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "You are Agens, a helpful coding agent.".into());
    let instructions =
        agens_bootstrap::session_config::SessionInstructions::resolve(&session_root, bootstrap);
    let primary = AgentDefinition {
        name: "primary".into(),
        description: "Default interactive agent".into(),
        mode: agens_core::AgentMode::Primary,
        model: None,
        reasoning_effort: None,
        system_prompt,
        permission_rules: Vec::new(),
        skills: Vec::new(),
    };
    let explore = AgentDefinition {
        name: "explore".into(),
        description: "Explore the codebase without modifying files".into(),
        mode: agens_core::AgentMode::Subagent,
        model: None,
        reasoning_effort: None,
        system_prompt: "You are the read-only exploration subagent. Inspect the codebase without modifying files and return concise, grounded findings."
            .into(),
        permission_rules: Vec::new(),
        skills: Vec::new(),
    };
    let general = AgentDefinition {
        name: "general".into(),
        description: "Handle a general delegated coding task".into(),
        mode: agens_core::AgentMode::Subagent,
        model: None,
        reasoning_effort: None,
        system_prompt: "You are the general-purpose subagent. Complete the delegated task with the available native tools and return a concise result."
            .into(),
        permission_rules: Vec::new(),
        skills: Vec::new(),
    };
    let global = bootstrap.paths.global_config.with_file_name("agents");
    let project = project_root.join(".agens/agents");
    let built_ins = [primary, explore, general];
    let discovery = match validator {
        Some(validator) => {
            AgentCatalog::discover_with_model_validator(&built_ins, &global, &project, validator)
        }
        None => AgentCatalog::discover(&built_ins, &global, &project),
    };
    discovery
        .map(|discovery| {
            discovery
                .catalog()
                .clone()
                .with_appended_instructions(instructions.text().unwrap_or(""))
        })
        .map_err(|_| CliError::configuration("agent catalog is unavailable"))
}

pub fn agent_rotation_error(error: AgentRotationError) -> CliError {
    match error {
        AgentRotationError::Busy => CliError::runtime(HeadlessTurnError::State),
        AgentRotationError::ModelUnavailable => {
            CliError::configuration("agent model is unavailable")
        }
        AgentRotationError::Persistence => CliError::storage("active agent could not be saved"),
    }
}
