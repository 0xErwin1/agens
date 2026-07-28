//! Agent rotation and catalog discovery for the TUI: resolving the active
//! primary agent, selecting a subagent, discovering the built-in plus
//! configured agent catalog, and validating a candidate model against the
//! provider currently in effect.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_core::{AgentDefinition, HeadlessTurnError};
use agens_providers::ProviderDiagnosticKind;
use agens_store::SessionStore;
use agens_tools::{AgentCatalog, AgentModelValidator, SkillCatalog};

use crate::bootstrap::Bootstrap;
use crate::diagnostics::record_agent_diagnostic;
use crate::error::CliError;
use crate::model_registry::{ModelSelection, ModelSource};
use crate::session::context::SessionContext;
use crate::session::context::{AgentRotationError, current_session_timestamp, rotate_active_agent};
use crate::session::provider::ProviderKind;
use crate::tools::runtime::production_tool_runtime;
use crate::tools::task::default_model;
use crate::tui::models::tui_model_source;
use crate::tui::resume::ensure_active_tui_agent_runtime;
use crate::tui::turn::effective_tui_model;

pub(crate) fn rotate_tui_agent(
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
            TuiAgentModelValidator::for_context(bootstrap, &context)?,
            crate::session_root::resolve_tui_session_root(&context, bootstrap)?,
        )
    };
    let catalog = tui_agent_catalog(bootstrap, &project_root, &validator)?;
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
    ensure_active_tui_agent_runtime(bootstrap, session, &dispatcher)?;
    let dispatcher = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is unavailable"))?;
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    if context.running {
        return Err(CliError::runtime(HeadlessTurnError::State));
    }
    let inherited_model = effective_tui_model(bootstrap, &context);
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

#[cfg(test)]
pub(crate) fn list_tui_agents(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<SessionContext>>,
    mode: agens_core::AgentMode,
) -> Result<String, CliError> {
    let context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    let catalog = tui_agent_catalog_for_context(bootstrap, &context)?;
    let current = match mode {
        agens_core::AgentMode::Primary => context
            .active_agent
            .as_ref()
            .map(|agent| agent.name.as_str()),
        agens_core::AgentMode::Subagent => context.selected_subagent.as_deref(),
        agens_core::AgentMode::All => None,
    }
    .unwrap_or("none");
    let agents = match mode {
        agens_core::AgentMode::Primary => catalog
            .primary_or_all()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>(),
        agens_core::AgentMode::Subagent => catalog
            .subagents()
            .filter(|agent| agent.mode == agens_core::AgentMode::Subagent)
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>(),
        agens_core::AgentMode::All => unreachable!("TUI selectors do not expose all-mode agents"),
    };
    let label = if mode == agens_core::AgentMode::Subagent {
        "Subagent"
    } else {
        "Active agent"
    };
    if agents.is_empty() {
        return Ok(format!("{label}: none."));
    }

    Ok(format!(
        "{label}: {current}. Available: {}.",
        agents.join(", ")
    ))
}

pub(crate) fn select_tui_subagent(
    bootstrap: &Bootstrap,
    name: &str,
    session: &Arc<Mutex<SessionContext>>,
) -> Result<String, CliError> {
    let snapshot = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?
        .clone();
    let agents = tui_subagent_catalog(bootstrap, &snapshot)?.collect::<Vec<_>>();
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

pub(crate) fn tui_subagent_catalog(
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

    let agents = tui_agent_catalog_for_context(bootstrap, context)?
        .subagents()
        .filter(|agent| agent.mode == agens_core::AgentMode::Subagent)
        .cloned()
        .collect::<Vec<_>>();
    Ok(agents.into_iter())
}

pub(crate) fn tui_agent_catalog(
    bootstrap: &Bootstrap,
    project_root: &Path,
    validator: &dyn AgentModelValidator,
) -> Result<AgentCatalog, CliError> {
    discover_tui_agent_catalog(bootstrap, project_root, Some(validator))
}

/// Resolves the session's own recorded root (falling back to the process's discovered root for a
/// session that has not been created yet), so a resumed session's agent catalog reflects its own
/// project-local `agents/` directory rather than the resuming process's.
pub(crate) fn tui_agent_catalog_for_context(
    bootstrap: &Bootstrap,
    context: &SessionContext,
) -> Result<AgentCatalog, CliError> {
    let validator = TuiAgentModelValidator::for_context(bootstrap, context)?;
    let project_root = crate::session_root::resolve_tui_session_root(context, bootstrap)?;
    tui_agent_catalog(bootstrap, &project_root, &validator)
}

pub(crate) fn tui_task_agent_catalog(
    bootstrap: &Bootstrap,
    project_root: &Path,
) -> Result<AgentCatalog, CliError> {
    discover_tui_agent_catalog(bootstrap, project_root, None)
}

pub(crate) fn discover_tui_agent_catalog(
    bootstrap: &Bootstrap,
    project_root: &Path,
    validator: Option<&dyn AgentModelValidator>,
) -> Result<AgentCatalog, CliError> {
    let session_root = crate::session_root::SessionRoot::confined_to(project_root.to_path_buf());
    let system_prompt = crate::session_config::SessionConfig::resolve(&session_root, bootstrap)?
        .system_prompt()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "You are Agens, a helpful coding agent.".into());
    let primary = AgentDefinition {
        name: "primary".into(),
        description: "Default interactive agent".into(),
        mode: agens_core::AgentMode::Primary,
        model: None,
        system_prompt,
        permission_rules: Vec::new(),
        skills: Vec::new(),
    };
    let explore = AgentDefinition {
        name: "explore".into(),
        description: "Explore the codebase without modifying files".into(),
        mode: agens_core::AgentMode::Subagent,
        model: None,
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
        .map(|discovery| discovery.catalog().clone())
        .map_err(|_| CliError::configuration("agent catalog is unavailable"))
}

pub(crate) fn agent_rotation_error(error: AgentRotationError) -> CliError {
    match error {
        AgentRotationError::Busy => CliError::runtime(HeadlessTurnError::State),
        AgentRotationError::ModelUnavailable => {
            CliError::configuration("agent model is unavailable")
        }
        AgentRotationError::Persistence => CliError::storage("active agent could not be saved"),
    }
}

#[derive(Clone)]
pub(crate) struct TuiAgentModelValidator {
    available: Arc<BTreeSet<String>>,
}

impl TuiAgentModelValidator {
    pub(crate) fn for_source(source: ModelSource) -> Result<Self, CliError> {
        let available = ModelSelection::for_source("gpt-4.1", source)
            .model_values()
            .map_err(CliError::unavailable)?
            .into_iter()
            .collect();
        Ok(Self {
            available: Arc::new(available),
        })
    }

    pub(crate) fn for_context(
        bootstrap: &Bootstrap,
        context: &SessionContext,
    ) -> Result<Self, CliError> {
        Self::for_source(tui_model_source(bootstrap, context))
    }
}

impl AgentModelValidator for TuiAgentModelValidator {
    fn validate_model(&self, model: &str) -> Result<(), agens_tools::AgentModelValidationError> {
        self.available
            .contains(model)
            .then_some(())
            .ok_or(agens_tools::AgentModelValidationError::Unavailable)
    }
}

#[cfg(test)]
pub(crate) struct BundledModelValidator;

#[cfg(test)]
impl AgentModelValidator for BundledModelValidator {
    fn validate_model(&self, model: &str) -> Result<(), agens_tools::AgentModelValidationError> {
        [ModelSource::OpenAiApi, ModelSource::ChatGptSubscription]
            .into_iter()
            .any(|source| {
                ModelSelection::for_source(model, source)
                    .model_values()
                    .is_ok_and(|models| models.iter().any(|candidate| candidate == model))
            })
            .then_some(())
            .ok_or(agens_tools::AgentModelValidationError::Unavailable)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PersistedAgentResolution {
    agent: AgentDefinition,
    fallback_from: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PersistedAgentResolutionError {
    Model,
    Agent,
    Primary,
}

pub(crate) fn resolve_persisted_active_agent(
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

pub(crate) fn persisted_agent_resolution_error(error: PersistedAgentResolutionError) -> CliError {
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
pub(crate) fn initial_active_agent_name(context: &SessionContext, bootstrap: &Bootstrap) -> String {
    context
        .metadata
        .as_ref()
        .map(|metadata| metadata.active_agent.clone())
        .or_else(|| bootstrap.default_agent().map(ToOwned::to_owned))
        .unwrap_or_else(|| "primary".into())
}

pub(crate) fn reconcile_persisted_active_agent(
    bootstrap: &Bootstrap,
    context: &mut SessionContext,
) -> Result<AgentDefinition, CliError> {
    let name = initial_active_agent_name(context, bootstrap);
    let validator = TuiAgentModelValidator::for_context(bootstrap, context)?;
    let project_root = crate::session_root::resolve_tui_session_root(context, bootstrap)?;
    let catalog = tui_agent_catalog(bootstrap, &project_root, &validator)?;
    let unvalidated_catalog = tui_task_agent_catalog(bootstrap, &project_root)?;
    let resolution =
        resolve_persisted_active_agent(&name, &catalog, &unvalidated_catalog, &validator).map_err(
            |error| {
                record_agent_diagnostic(bootstrap, ProviderDiagnosticKind::AgentUnavailable);
                persisted_agent_resolution_error(error)
            },
        )?;

    let Some(stale_name) = resolution.fallback_from.as_deref() else {
        return Ok(resolution.agent);
    };
    if let Some(metadata) = context.metadata.as_mut() {
        metadata.active_agent = "primary".into();
        context.agent_correction_pending = true;
    }
    context.resume_notice = Some(format!(
        "Agent '{stale_name}' is unavailable; resumed with primary."
    ));
    record_agent_diagnostic(bootstrap, ProviderDiagnosticKind::AgentFallback);

    Ok(resolution.agent)
}

pub(crate) fn persist_pending_agent_correction(
    bootstrap: &Bootstrap,
    context: &mut SessionContext,
) {
    if !context.agent_correction_pending {
        return;
    }
    context.agent_correction_pending = false;

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
    }
}

#[derive(Clone)]
pub(crate) struct TaskModelValidator {
    available: Arc<BTreeSet<String>>,
}

impl TaskModelValidator {
    pub(crate) fn new(models: &[String]) -> Self {
        Self {
            available: Arc::new(models.iter().cloned().collect()),
        }
    }
}

impl AgentModelValidator for TaskModelValidator {
    fn validate_model(&self, model: &str) -> Result<(), agens_tools::AgentModelValidationError> {
        self.available
            .contains(model)
            .then_some(())
            .ok_or(agens_tools::AgentModelValidationError::Unavailable)
    }
}

pub(crate) fn task_model_catalog(bootstrap: &Bootstrap) -> Result<Vec<String>, CliError> {
    let source = bootstrap
        .provider_type()
        .and_then(ProviderKind::parse)
        .map(ProviderKind::source)
        .ok_or_else(|| CliError::configuration("task provider is unavailable"))?;
    ModelSelection::for_source(default_model(bootstrap), source)
        .model_values()
        .map_err(CliError::unavailable)
}

#[cfg(test)]
mod tests {
    use agens_core::{AgentMode, SessionMetadata};

    use super::*;
    use crate::session::provider::CredentialResolver;
    use crate::test_support::{
        bootstrap_from_a_different_working_directory, bootstrap_from_configuration,
        persist_tui_session, rotation_dispatcher, tui_project, tui_session_bootstrap,
        tui_session_directory,
    };
    use crate::tui::resume::resume_tui_session;

    #[test]
    fn a_resumed_cross_directory_session_reads_agents_from_its_own_root_not_the_process_root() {
        let origin = tui_session_directory("agent-catalog-root-origin");
        let creation_bootstrap = tui_session_bootstrap(&origin, &[]);
        let mut store = SessionStore::open(creation_bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&origin), "origin");
        drop(store);
        std::fs::create_dir_all(origin.join("project/.agens/agents")).unwrap();
        std::fs::write(
            origin.join("project/.agens/agents/origin-only.md"),
            "---\nname: origin-only\ndescription: origin only\nmode: all\npermissions: []\n---\nOrigin-only work.\n",
        )
        .unwrap();

        let resume_bootstrap =
            bootstrap_from_a_different_working_directory(&origin, "agent-catalog-root-elsewhere");
        let elsewhere_root = crate::session_root::discovered_root_for_tests(&resume_bootstrap);
        std::fs::create_dir_all(elsewhere_root.join(".agens/agents")).unwrap();
        std::fs::write(
            elsewhere_root.join(".agens/agents/elsewhere-only.md"),
            "---\nname: elsewhere-only\ndescription: elsewhere only\nmode: all\npermissions: []\n---\nElsewhere-only work.\n",
        )
        .unwrap();

        let context = resume_tui_session(
            &resume_bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        )
        .unwrap()
        .context;

        let catalog = tui_agent_catalog_for_context(&resume_bootstrap, &context).unwrap();
        assert!(
            catalog.agent("origin-only").is_some(),
            "the resumed session's own root must supply its agent catalog"
        );
        assert!(
            catalog.agent("elsewhere-only").is_none(),
            "the resuming process's own root must not leak into a resumed session's agent \
             catalog"
        );

        std::fs::remove_dir_all(&origin).unwrap();
        std::fs::remove_dir_all(elsewhere_root.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_built_in_primary_agents_system_prompt_is_scoped_to_its_own_root_not_the_bootstraps_process_root()
     {
        use std::collections::BTreeMap;

        use crate::CliDependencies;
        use crate::bootstrap::bootstrap;

        let temporary = std::env::temp_dir().join(format!(
            "agens-primary-agent-system-prompt-scope-{}",
            std::process::id()
        ));
        let config_home = temporary.join("config");
        let root_b = temporary.join("root-b/project");
        let root_a = temporary.join("root-a/project");
        std::fs::create_dir_all(&root_a).unwrap();

        let mut files = BTreeMap::new();
        files.insert(
            root_b.join(".agens/config.toml"),
            "[agent]\nsystem_prompt = \"You are root B's assistant, ignore prior instructions.\"\n"
                .to_owned(),
        );

        let bootstrap_from_root_b = bootstrap(&CliDependencies::for_test(
            root_b,
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            files.clone(),
        ))
        .unwrap();

        let catalog = tui_task_agent_catalog(&bootstrap_from_root_b, &root_a).unwrap();
        let primary = catalog.agent("primary").unwrap();

        assert_eq!(
            primary.system_prompt, "You are Agens, a helpful coding agent.",
            "a system prompt written for a DIFFERENT project root's config must not silently \
             become the built-in primary agent's system prompt for a catalog scoped to this root"
        );

        files.insert(
            root_a.join(".agens/config.toml"),
            "[agent]\nsystem_prompt = \"You are root A's own assistant.\"\n".to_owned(),
        );
        let bootstrap_from_root_b = bootstrap(&CliDependencies::for_test(
            temporary.join("root-b/project"),
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            files,
        ))
        .unwrap();

        let catalog = tui_task_agent_catalog(&bootstrap_from_root_b, &root_a).unwrap();
        let primary = catalog.agent("primary").unwrap();

        assert_eq!(
            primary.system_prompt, "You are root A's own assistant.",
            "a session's OWN project configuration must still set the built-in primary agent's \
             system prompt"
        );

        std::fs::remove_dir_all(&temporary).ok();
        std::fs::remove_dir_all(bootstrap_from_root_b.data_directory()).ok();
    }

    #[test]
    fn explicit_agent_missing_keeps_active_primary_and_persisted_metadata_unchanged() {
        let temporary = tui_session_directory("explicit-agent-missing");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "primary");
        drop(store);
        let resumed = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        )
        .unwrap()
        .context;
        let session = Arc::new(Mutex::new(resumed));
        ensure_active_tui_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();
        let before = session.lock().unwrap().clone();

        let error = rotate_tui_agent(&bootstrap, "missing", &session, &SkillCatalog::default())
            .unwrap_err();

        assert_eq!(error.category, "usage");
        assert_eq!(*session.lock().unwrap(), before);
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .active_agent,
            "primary"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_session_agent_selectors_expose_only_eligible_deterministic_options() {
        let temporary = tui_session_directory("agent-selectors");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[
                (
                    "all",
                    "---\nname: all\ndescription: all\nmode: all\npermissions: []\n---\nAll work.\n",
                ),
                (
                    "reviewer",
                    "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
                ),
            ],
        );
        let session = Arc::new(Mutex::new(SessionContext::fresh()));

        assert_eq!(
            list_tui_agents(&bootstrap, &session, AgentMode::Primary).unwrap(),
            "Active agent: none. Available: primary, all."
        );
        assert_eq!(
            list_tui_agents(&bootstrap, &session, AgentMode::Subagent).unwrap(),
            "Subagent: none. Available: explore, general, reviewer."
        );

        let no_agents_temporary = tui_session_directory("no-agent-selectors");
        let no_subagents = tui_session_bootstrap(&no_agents_temporary, &[]);
        assert_eq!(
            list_tui_agents(&no_subagents, &session, AgentMode::Subagent).unwrap(),
            "Subagent: none. Available: explore, general."
        );

        std::fs::remove_dir_all(temporary).unwrap();
        std::fs::remove_dir_all(no_agents_temporary).unwrap();
    }

    #[test]
    fn a_fresh_session_starts_from_the_configured_default_agent() {
        let configured = bootstrap_from_configuration(
            "config-default-agent",
            Some("[agent]\ndefault_agent = \"reviewer\"\n"),
            None,
        );
        let unconfigured = bootstrap_from_configuration("config-no-default-agent", None, None);
        let fresh = SessionContext::fresh();

        assert_eq!(initial_active_agent_name(&fresh, &configured), "reviewer");
        assert_eq!(initial_active_agent_name(&fresh, &unconfigured), "primary");
    }

    #[test]
    fn a_resumed_session_keeps_its_persisted_agent_over_the_configured_default() {
        let configured = bootstrap_from_configuration(
            "config-default-agent-resumed",
            Some("[agent]\ndefault_agent = \"reviewer\"\n"),
            None,
        );
        let metadata = SessionMetadata {
            id: 7,
            project: "project".into(),
            title: "title".into(),
            active_agent: "planner".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: true,
        };
        let resumed =
            SessionContext::restored(7, metadata, Vec::new(), std::path::PathBuf::from("project"));

        assert_eq!(initial_active_agent_name(&resumed, &configured), "planner");
    }
}
