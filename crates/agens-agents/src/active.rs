//! The agent a session is actually running.
//!
//! A persisted agent can stop being available between one run and the next, so
//! resolution has to decide what a resumed session falls back to and record why,
//! rather than failing the resume.

use std::sync::{Arc, Mutex};

use agens_bootstrap::Bootstrap;
use agens_core::{AgentDefinition, ReasoningEffort, RequestConfig};
use agens_diagnostics::record_agent_diagnostic;
use agens_error::CliError;
use agens_permissions::SharedToolDispatcher;
use agens_providers::ProviderDiagnosticKind;
use agens_session::context::ActiveAgentRuntime;
use agens_session::context::SessionContext;
use agens_session::context::current_session_timestamp;
use agens_session::model::{effective_model, model_source};
use agens_store::SessionStore;
use agens_tools::{AgentCatalog, AgentModelValidator};

use crate::catalog::{agent_catalog, agent_rotation_error, task_agent_catalog};
use crate::models::AgentModelCompatibility;
use crate::resolver::{AgentProfileResolver, ProfileOrigin};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedAgentResolution {
    agent: AgentDefinition,
    fallback_from: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistedAgentResolutionError {
    Model,
    Agent,
    Primary,
}

pub fn resolve_persisted_active_agent(
    name: &str,
    catalog: &AgentCatalog,
    unvalidated_catalog: &AgentCatalog,
    validator: &dyn AgentModelValidator,
) -> Result<PersistedAgentResolution, PersistedAgentResolutionError> {
    let unvalidated = unvalidated_catalog.agent(name);
    if unvalidated
        .and_then(|agent| agent.model.as_deref())
        .is_some_and(|model| validator.validate_model(model).is_err())
    {
        return Err(PersistedAgentResolutionError::Model);
    }
    if let Some(agent) = catalog
        .agent(name)
        .filter(|agent| agent.mode != agens_core::AgentMode::Subagent)
    {
        return Ok(PersistedAgentResolution {
            agent: agent.clone(),
            fallback_from: None,
        });
    }
    if name == "primary" {
        return Err(PersistedAgentResolutionError::Primary);
    }
    if unvalidated.is_some() {
        return Err(PersistedAgentResolutionError::Agent);
    }

    let unvalidated_primary = unvalidated_catalog
        .agent("primary")
        .ok_or(PersistedAgentResolutionError::Primary)?;
    if unvalidated_primary.mode == agens_core::AgentMode::Subagent {
        return Err(PersistedAgentResolutionError::Primary);
    }
    if unvalidated_primary
        .model
        .as_deref()
        .is_some_and(|model| validator.validate_model(model).is_err())
    {
        return Err(PersistedAgentResolutionError::Model);
    }
    let primary = catalog
        .agent("primary")
        .filter(|agent| agent.mode != agens_core::AgentMode::Subagent)
        .ok_or(PersistedAgentResolutionError::Primary)?;
    Ok(PersistedAgentResolution {
        agent: primary.clone(),
        fallback_from: Some(name.to_owned()),
    })
}

pub fn persisted_agent_resolution_error(error: PersistedAgentResolutionError) -> CliError {
    match error {
        PersistedAgentResolutionError::Model => {
            CliError::configuration("agent model is unavailable")
        }
        PersistedAgentResolutionError::Agent => {
            CliError::configuration("active agent is unavailable")
        }
        PersistedAgentResolutionError::Primary => {
            CliError::configuration("primary agent is unavailable")
        }
    }
}

/// A resumed session keeps the agent it was persisted with; a fresh one starts
/// from the configured default. An unresolvable name is not fatal here: it
/// falls through the same recovery path a stale persisted agent takes.
pub fn initial_active_agent_name(context: &SessionContext, bootstrap: &Bootstrap) -> String {
    context
        .metadata
        .as_ref()
        .map(|metadata| metadata.active_agent.clone())
        .or_else(|| bootstrap.default_agent().map(ToOwned::to_owned))
        .unwrap_or_else(|| "primary".into())
}

pub fn reconcile_persisted_active_agent(
    bootstrap: &Bootstrap,
    context: &mut SessionContext,
) -> Result<AgentDefinition, CliError> {
    let name = initial_active_agent_name(context, bootstrap);
    let validator = AgentModelCompatibility::for_context(bootstrap, context)?;
    let project_root = agens_session::root::resolve_tui_session_root(context, bootstrap)?;
    let catalog = agent_catalog(bootstrap, &project_root, &validator)?;
    let unvalidated_catalog = task_agent_catalog(bootstrap, &project_root)?;
    let resolution =
        resolve_persisted_active_agent(&name, &catalog, &unvalidated_catalog, &validator).map_err(
            |error| {
                record_agent_diagnostic(bootstrap, ProviderDiagnosticKind::AgentUnavailable);
                persisted_agent_resolution_error(error)
            },
        )?;

    if let Some(stale_name) = resolution.fallback_from.as_deref() {
        if let Some(metadata) = context.metadata.as_mut() {
            metadata.active_agent = "primary".into();
            context.agent_correction_pending = true;
        }
        context.resume_notice = Some(format!(
            "Agent '{stale_name}' is unavailable; resumed with primary."
        ));
        record_agent_diagnostic(bootstrap, ProviderDiagnosticKind::AgentFallback);
    }

    apply_configured_agent_profile(
        bootstrap,
        context,
        &project_root,
        resolution.agent,
        &validator,
    )
}

fn apply_configured_agent_profile(
    bootstrap: &Bootstrap,
    context: &mut SessionContext,
    project_root: &std::path::Path,
    mut agent: AgentDefinition,
    validator: &dyn AgentModelValidator,
) -> Result<AgentDefinition, CliError> {
    let session_model = effective_model(bootstrap, context);
    let session_effort = match context.selection.as_ref() {
        Some(selection) => selection.reasoning_effort_value(),
        None => context
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.reasoning_effort)
            .or_else(|| {
                bootstrap
                    .reasoning_effort()
                    .and_then(parse_reasoning_effort)
            }),
    };
    let session_root =
        agens_bootstrap::session_root::SessionRoot::confined_to(project_root.to_path_buf());
    let session_config =
        agens_bootstrap::session_config::SessionConfig::resolve(&session_root, bootstrap)?;
    let profile = AgentProfileResolver::new(session_config.agent_profiles()).resolve(
        &agent.name,
        agent.model.as_deref(),
        agent.reasoning_effort,
        &session_model,
        session_effort,
    );
    if profile.model.origin == ProfileOrigin::SessionInherited
        && profile.effort.origin == ProfileOrigin::SessionInherited
    {
        return Ok(agent);
    }
    if profile.model.origin != ProfileOrigin::SessionInherited {
        validator
            .validate_model(&profile.model.value)
            .map_err(|_| CliError::configuration("agent model is unavailable"))?;
    }

    let mut selection =
        agens_models::ModelSelection::for_source(&session_model, model_source(bootstrap, context));
    if selection.apply_model(&profile.model.value).is_err() {
        if profile.model.origin == ProfileOrigin::SessionInherited {
            selection
                .apply_unverified_model(&profile.model.value)
                .map_err(|_| CliError::configuration("agent model is unavailable"))?;
        } else {
            return Err(CliError::configuration("agent model is unavailable"));
        }
    }
    if let Some(effort) = profile.effort.value {
        selection
            .apply_reasoning_effort(effort.as_str())
            .map_err(|_| CliError::configuration("agent reasoning effort is unavailable"))?;
    }

    agent.model = Some(profile.model.value);
    agent.reasoning_effort = profile.effort.value;
    context.selection = Some(selection);
    Ok(agent)
}

fn parse_reasoning_effort(value: &str) -> Option<ReasoningEffort> {
    RequestConfig::with_reasoning_effort(value)
        .ok()
        .and_then(|config| config.reasoning_effort())
}

pub fn persist_pending_agent_correction(bootstrap: &Bootstrap, context: &mut SessionContext) {
    if !context.agent_correction_pending {
        return;
    }

    let Some(metadata) = context.metadata.as_mut() else {
        return;
    };
    let mut corrected = metadata.clone();
    corrected.updated_at = current_session_timestamp();
    if SessionStore::open(bootstrap.data_directory())
        .and_then(|mut store| store.update_session(&corrected))
        .is_ok()
    {
        *metadata = corrected;
        context.agent_correction_pending = false;
    }
}

pub fn ensure_active_agent_runtime(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<SessionContext>>,
    dispatcher: &SharedToolDispatcher,
) -> Result<(), CliError> {
    let dispatcher = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is unavailable"))?;
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    if context.active_agent.is_some() {
        return Ok(());
    }
    let project_root = agens_session::root::resolve_tui_session_root(&context, bootstrap)?;
    let agent = reconcile_persisted_active_agent(bootstrap, &mut context)?;
    let validator = AgentModelCompatibility::for_context(bootstrap, &context)?;
    let inherited_model = effective_model(bootstrap, &context);
    let active_agent = ActiveAgentRuntime::build(
        &agent,
        Some(&inherited_model),
        &project_root.display().to_string(),
        &dispatcher,
        &validator,
    )
    .map_err(agent_rotation_error)?;
    persist_pending_agent_correction(bootstrap, &mut context);
    context.active_agent = Some(active_agent);
    Ok(())
}
