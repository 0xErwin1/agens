//! Switching the active agent.
//!
//! Rotation lives here rather than in `agens-agents` because installing an agent
//! means handing it the tool runtime it will call through, and that runtime is
//! assembled by this crate. Everything else about agents — the catalog, the
//! validators, resolving a persisted choice — is in `agens-agents`.

use std::sync::{Arc, Mutex};

use agens_core::HeadlessTurnError;
use agens_store::SessionStore;
use agens_tools::SkillCatalog;

use crate::runtime::production_tool_runtime;
use agens_agents::{
    AgentModelCompatibility, agent_catalog, agent_rotation_error, ensure_active_agent_runtime,
};
use agens_bootstrap::Bootstrap;
use agens_error::CliError;
use agens_session::context::SessionContext;
use agens_session::context::rotate_active_agent;
use agens_session::model::effective_model;
pub fn rotate_agent(
    bootstrap: &Bootstrap,
    name: &str,
    session: &Arc<Mutex<SessionContext>>,
    skills: &SkillCatalog,
) -> Result<String, CliError> {
    let (validator, project_root) = {
        let context = session
            .lock()
            .map_err(|_| CliError::storage("TUI session is unavailable"))?;
        (
            AgentModelCompatibility::for_context(bootstrap, &context)?,
            agens_session::root::resolve_tui_session_root(&context, bootstrap)?,
        )
    };
    let catalog = agent_catalog(bootstrap, &project_root, &validator)?;
    if session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?
        .running
    {
        return Err(CliError::runtime(HeadlessTurnError::State));
    }
    let agent = catalog
        .agent(name.trim())
        .filter(|agent| agent.mode != agens_core::AgentMode::Subagent)
        .ok_or_else(|| CliError::usage("/agent requires an available primary agent"))?
        .clone();
    let (_, dispatcher) = production_tool_runtime(bootstrap, &project_root, Some(skills))?;
    ensure_active_agent_runtime(bootstrap, session, &dispatcher)?;
    let dispatcher = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is unavailable"))?;
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    if context.running {
        return Err(CliError::runtime(HeadlessTurnError::State));
    }
    let inherited_model = effective_model(bootstrap, &context);
    let mut store = context
        .metadata
        .is_some()
        .then(|| SessionStore::open(bootstrap.data_directory()))
        .transpose()
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    rotate_active_agent(
        &mut context,
        &agent,
        Some(&inherited_model),
        &project_root.display().to_string(),
        &dispatcher,
        &validator,
        store.as_mut(),
    )
    .map_err(agent_rotation_error)?;
    Ok(format!("Active agent: {}.", agent.name))
}
