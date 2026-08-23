//! Production task-tool wiring: assembles the isolated task runtime used by
//! the TUI (permission gate, resolver, and dispatcher) and registers the
//! `task`/`task_control`/`task_message` tools with the parent dispatcher.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_core::ask_user::AskUserPort;
use agens_core::{AgentModelSource, PermissionMode, PermissionSession};
use agens_providers::OpenAiFunctionTool;
use agens_store::PermissionGrantStore;
use agens_tools::{
    SkillCatalog, TaskControlTool, TaskExecutionRegistry, TaskMessageSource, TaskMessageTool,
    TaskModelResolutionError, TaskRunner, TaskTool, ToolDispatcher,
};

use crate::runner::{ProductionTaskRunner, TuiTaskLifecycleBridge};
use crate::runtime::production_tool_runtime_with_discovery_cancellation;
use agens_agents::{ProfileOrigin, TaskModelValidator, task_agent_catalog, task_model_catalog};
use agens_bootstrap::Bootstrap;
use agens_diagnostics::{next_diagnostic_reference, record_subagent_model_unavailable};
use agens_dispatch::{AuthorizedNativeTaskRuntime, ProductionToolDispatcher};
use agens_error::CliError;
use agens_permissions::PermissionPrompter;
use agens_permissions::{
    ProductionPermissionGate, ProductionPermissionResolver, ProductionPromptAuthorization,
    SharedToolDispatcher, permission_policy,
};
use agens_session::provider::{bootstrap_authentication, resolve_provider_for_model};

pub struct ProductionTuiTaskRuntime {
    pub provider_tools: Vec<OpenAiFunctionTool>,
    pub dispatcher: SharedToolDispatcher,
    pub task_registry: TaskExecutionRegistry,
    #[allow(dead_code)]
    pub authorized: AuthorizedNativeTaskRuntime<Box<dyn PermissionPrompter>>,
    /// The session root this runtime's dispatcher, permission policy, and grant scope were built
    /// against. The headless turn body reuses this value instead of re-deriving a root, so the
    /// parent turn and this runtime never disagree about which project's grants apply.
    pub project_root: std::path::PathBuf,
}

pub struct TaskParentSelection {
    pub model: String,
    pub request_config: agens_core::RequestConfig,
    pub diagnostic_reference: Option<String>,
}

/// Builds the task runtime for a TUI-launched subagent (both the parent turn's own `task` tool
/// runtime and the selected-subagent-launch runtime `crates/agens-tui-app/src/engine.rs` builds
/// ahead of the parent turn). `bypass` must be the session's current bypass state at the time this
/// runtime is built — see `PermissionSession::with_temporary_bypass` below and the module docs on
/// subagent scope for why `child.rs` never receives it.
#[allow(clippy::too_many_arguments)]
pub fn production_tui_task_runtime(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    prompter: Box<dyn PermissionPrompter>,
    lifecycle_bridge: TuiTaskLifecycleBridge,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: String,
    bypass: bool,
    ask_user: Box<dyn AskUserPort>,
) -> Result<ProductionTuiTaskRuntime, CliError> {
    production_tui_task_runtime_with_cancellation(
        bootstrap,
        project_root,
        skills,
        prompter,
        lifecycle_bridge,
        parent_request_config,
        model_resolution_reference,
        bypass,
        ask_user,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn production_tui_task_runtime_with_cancellation(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    prompter: Box<dyn PermissionPrompter>,
    lifecycle_bridge: TuiTaskLifecycleBridge,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: String,
    bypass: bool,
    ask_user: Box<dyn AskUserPort>,
    discovery_cancellation: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<ProductionTuiTaskRuntime, CliError> {
    production_tui_task_runtime_with_runner_parent_config_and_cancellation(
        bootstrap,
        project_root,
        skills,
        prompter,
        ProductionTaskRunner::new(bootstrap.clone(), project_root.to_path_buf())
            .with_lifecycle_bridge(lifecycle_bridge)
            .with_bypass(bypass),
        parent_request_config,
        Some(model_resolution_reference),
        ask_user,
        None,
        discovery_cancellation,
    )
}

pub fn production_tui_task_runtime_with_runner(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    prompter: Box<dyn PermissionPrompter>,
    task_runner: ProductionTaskRunner,
    ask_user: Box<dyn AskUserPort>,
) -> Result<ProductionTuiTaskRuntime, CliError> {
    production_tui_task_runtime_with_runner_and_parent_config(
        bootstrap,
        project_root,
        skills,
        prompter,
        task_runner,
        agens_core::RequestConfig::default(),
        None,
        ask_user,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn production_tui_task_runtime_with_runner_and_parent_config(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    prompter: Box<dyn PermissionPrompter>,
    task_runner: ProductionTaskRunner,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
    ask_user: Box<dyn AskUserPort>,
) -> Result<ProductionTuiTaskRuntime, CliError> {
    production_tui_task_runtime_with_runner_parent_config_and_cancellation(
        bootstrap,
        project_root,
        skills,
        prompter,
        task_runner,
        parent_request_config,
        model_resolution_reference,
        ask_user,
        None,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn production_tui_task_runtime_with_runner_parent_config_and_cancellation(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    prompter: Box<dyn PermissionPrompter>,
    task_runner: ProductionTaskRunner,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
    ask_user: Box<dyn AskUserPort>,
    working_directory: Option<agens_tools::WorkingDirectory>,
    discovery_cancellation: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<ProductionTuiTaskRuntime, CliError> {
    let bypass = task_runner.bypass();
    let task_registry = task_runner.execution_registry().unwrap_or_default();
    let parent_model = match bootstrap.model() {
        Some(model) => model.to_owned(),
        None => {
            resolve_provider_for_model(None, &bootstrap_authentication(bootstrap))
                .map_err(|error| CliError::configuration(error.message()))?
                .model
        }
    };
    let (provider_tools, dispatcher) = production_tool_runtime_with_discovery_cancellation(
        bootstrap,
        project_root,
        Some(skills),
        parent_model,
        parent_request_config,
        model_resolution_reference,
        task_runner,
        ask_user,
        working_directory,
        discovery_cancellation,
    )?;
    let project = project_root.display().to_string();
    let session_root =
        agens_bootstrap::session_root::SessionRoot::confined_to(project_root.to_path_buf());
    let session_config =
        agens_bootstrap::session_config::SessionConfig::resolve(&session_root, bootstrap)?;
    let policy = permission_policy(
        session_config.permission_rules(),
        &project,
        PermissionMode::Edit,
        &dispatcher,
        None,
    )?;
    let grant_store = PermissionGrantStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("permission grants are unavailable"))?;
    let grants = grant_store
        .grants_for_project(&project)
        .map_err(|_| CliError::storage("permission grants are unavailable"))?;
    let grants = Arc::new(Mutex::new(grants));
    let session = if bypass {
        PermissionSession::with_temporary_bypass()
    } else {
        PermissionSession::new()
    };
    let pending = Arc::new(Mutex::new(BTreeMap::new()));
    let prompts = Arc::new(Mutex::new(BTreeMap::new()));
    let gate = ProductionPermissionGate::new(
        policy.clone(),
        Arc::clone(&grants),
        session,
        project.clone(),
        Arc::clone(&dispatcher),
        Arc::clone(&pending),
        Arc::clone(&prompts),
    );
    let resolver = ProductionPermissionResolver::new(
        prompter,
        grant_store,
        grants,
        prompts,
        ProductionPromptAuthorization {
            policy,
            session: if bypass {
                PermissionSession::with_temporary_bypass()
            } else {
                PermissionSession::new()
            },
            project,
            dispatcher: Arc::clone(&dispatcher),
            allowed: Arc::clone(&pending),
        },
    );

    Ok(ProductionTuiTaskRuntime {
        provider_tools,
        dispatcher: Arc::clone(&dispatcher),
        task_registry,
        authorized: AuthorizedNativeTaskRuntime {
            gate,
            resolver,
            dispatcher: ProductionToolDispatcher::new(dispatcher, pending),
            next_call_id: 0,
        },
        project_root: project_root.to_path_buf(),
    })
}

pub fn register_production_task_tool<R: TaskRunner>(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    dispatcher: &mut ToolDispatcher,
    provider_tools: &mut BTreeMap<String, OpenAiFunctionTool>,
    parent: TaskParentSelection,
    task_runner: R,
) -> Result<(), CliError> {
    register_task_tool(
        bootstrap,
        project_root,
        skills,
        dispatcher,
        provider_tools,
        parent,
        task_runner,
        Some(TaskMessageSource::Main),
    )
}

/// `coordination` names the scope the `task_control`/`task_message` pair
/// should target, or `None` where the caller already holds its own pair.
#[allow(clippy::too_many_arguments)]
fn register_task_tool<R: TaskRunner>(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    dispatcher: &mut ToolDispatcher,
    provider_tools: &mut BTreeMap<String, OpenAiFunctionTool>,
    parent: TaskParentSelection,
    task_runner: R,
    coordination: Option<TaskMessageSource>,
) -> Result<(), CliError> {
    let available_models = task_model_catalog(bootstrap)?;
    let validator = TaskModelValidator::new(&available_models);
    let agents = resolved_task_agents(bootstrap, project_root, &parent)?;
    if !agents
        .subagents()
        .any(|agent| agent.mode == agens_core::AgentMode::Subagent)
    {
        return Ok(());
    }

    let diagnostic_bootstrap = bootstrap.clone();
    let task = TaskTool::from_catalogs_with_parent_config(
        agents,
        skills.clone(),
        parent.model,
        parent.request_config,
        validator,
        task_runner,
    )
    .with_model_resolution_diagnostics(move |error| match error {
        TaskModelResolutionError::ModelUnavailable {
            agent,
            requested_model,
            fallback_model,
        } => {
            let reference = parent
                .diagnostic_reference
                .clone()
                .unwrap_or_else(next_diagnostic_reference);
            record_subagent_model_unavailable(
                &diagnostic_bootstrap,
                &reference,
                &agent,
                &requested_model,
                &fallback_model,
            );
            Some(reference)
        }
    });
    let input_schema = task.catalog_input_schema();
    let task_registry = task.execution_registry().clone();

    provider_tools.insert(
        "task".into(),
        OpenAiFunctionTool::new(
            "task",
            "Dispatch an isolated eligible subagent task. After background delegation, end the turn; its completion notice arrives on the next turn.",
            input_schema,
        )
        .map_err(|_| CliError::configuration("task tool is unavailable"))?,
    );
    dispatcher
        .register_native("native::task", agens_core::ToolAccess::Write, task)
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;

    let Some(source) = coordination else {
        return Ok(());
    };
    register_task_coordination_tools(dispatcher, provider_tools, task_registry, source)
}

/// Registers `task` for a delegated runtime that already holds its own
/// coordination tools.
///
/// A child's `task_control` is bound to its own execution; the pair
/// [`register_production_task_tool`] adds is bound to `Main`. Registering both
/// replaces the first with the second, so the child would be able to control
/// the main thread and no longer able to control itself — the opposite of what
/// either tool is for.
pub fn register_delegating_task_tool<R: TaskRunner>(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    dispatcher: &mut ToolDispatcher,
    provider_tools: &mut BTreeMap<String, OpenAiFunctionTool>,
    parent: TaskParentSelection,
    task_runner: R,
) -> Result<(), CliError> {
    register_task_tool(
        bootstrap,
        project_root,
        skills,
        dispatcher,
        provider_tools,
        parent,
        task_runner,
        None,
    )
}

fn resolved_task_agents(
    bootstrap: &Bootstrap,
    project_root: &Path,
    parent: &TaskParentSelection,
) -> Result<agens_tools::AgentCatalog, CliError> {
    let session_root = agens_bootstrap::session_root::SessionRoot::confined_to(project_root.into());
    let session_config =
        agens_bootstrap::session_config::SessionConfig::resolve(&session_root, bootstrap)?;
    let resolver = agens_agents::AgentProfileResolver::new(session_config.agent_profiles());
    let agents = task_agent_catalog(bootstrap, project_root)?;

    Ok(agents.map_agents(|agent| {
        let profile = resolver.resolve(
            &agent.name,
            agent.model.as_deref(),
            agent.reasoning_effort,
            &parent.model,
            parent.request_config.reasoning_effort(),
        );
        let mut agent = agent.clone();
        agent.model = Some(profile.model.value);
        agent.model_source = Some(model_source(profile.model.origin));
        agent.reasoning_effort = profile.effort.value;
        agent
    }))
}

const fn model_source(origin: ProfileOrigin) -> AgentModelSource {
    match origin {
        ProfileOrigin::ProjectProfile | ProfileOrigin::GlobalProfile => {
            AgentModelSource::ConfiguredProfile
        }
        ProfileOrigin::Frontmatter => AgentModelSource::AgentDefinition,
        ProfileOrigin::SessionInherited => AgentModelSource::SessionInherited,
    }
}

fn register_task_coordination_tools(
    dispatcher: &mut ToolDispatcher,
    provider_tools: &mut BTreeMap<String, OpenAiFunctionTool>,
    registry: TaskExecutionRegistry,
    source: TaskMessageSource,
) -> Result<(), CliError> {
    provider_tools.insert(
        "task_control".into(),
        OpenAiFunctionTool::new(
            "task_control",
            "Inspect, background, cancel, or wait for a live subagent execution",
            TaskControlTool::input_schema(),
        )
        .map_err(|_| CliError::configuration("task control tool is unavailable"))?,
    );
    provider_tools.insert(
        "task_message".into(),
        OpenAiFunctionTool::new(
            "task_message",
            "Queue a bounded coordination message for a live subagent or the main agent",
            TaskMessageTool::input_schema(),
        )
        .map_err(|_| CliError::configuration("task message tool is unavailable"))?,
    );
    dispatcher
        .register_native(
            "native::task_control",
            agens_core::ToolAccess::Write,
            TaskControlTool::new(registry.clone(), source),
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    dispatcher
        .register_native(
            "native::task_message",
            agens_core::ToolAccess::Write,
            TaskMessageTool::new(registry, source),
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))
}

#[cfg(test)]
mod tests {
    use agens_agents::ProfileOrigin;
    use agens_core::{AgentModelSource, ReasoningEffort, RequestConfig};
    use agens_fixtures::bootstrap_from_configuration;

    use super::{TaskParentSelection, model_source, resolved_task_agents};

    #[test]
    fn every_profile_origin_maps_to_the_model_source_the_task_tool_reads() {
        assert_eq!(
            model_source(ProfileOrigin::ProjectProfile),
            AgentModelSource::ConfiguredProfile
        );
        assert_eq!(
            model_source(ProfileOrigin::GlobalProfile),
            AgentModelSource::ConfiguredProfile
        );
        assert_eq!(
            model_source(ProfileOrigin::Frontmatter),
            AgentModelSource::AgentDefinition
        );
        assert_eq!(
            model_source(ProfileOrigin::SessionInherited),
            AgentModelSource::SessionInherited
        );
    }

    #[test]
    fn task_agents_resolve_profiles_for_the_shared_tui_and_headless_builder() {
        let bootstrap = bootstrap_from_configuration(
            "task-agent-profiles",
            Some(
                "[provider]\nmodel = \"openai-api/gpt-4.1\"\n\n[agents.explore]\nmodel = \"global-model\"\n\n[agents.general]\neffort = \"low\"\n",
            ),
            Some("[agents.explore]\nmodel = \"project-model\"\neffort = \"max\"\n"),
        );
        let project_root = bootstrap
            .paths
            .project_config
            .parent()
            .and_then(|path| path.parent())
            .unwrap()
            .to_path_buf();
        let parent = TaskParentSelection {
            model: "session-model".into(),
            request_config: RequestConfig::with_reasoning_effort_value(ReasoningEffort::High),
            diagnostic_reference: None,
        };

        let agents = resolved_task_agents(&bootstrap, &project_root, &parent).unwrap();
        let explore = agents.agent("explore").unwrap();
        assert_eq!(explore.model.as_deref(), Some("project-model"));
        assert_eq!(explore.reasoning_effort, Some(ReasoningEffort::Max));
        assert_eq!(
            explore.model_source,
            Some(AgentModelSource::ConfiguredProfile)
        );

        let general = agents.agent("general").unwrap();
        assert_eq!(general.model.as_deref(), Some("session-model"));
        assert_eq!(general.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(
            general.model_source,
            Some(AgentModelSource::SessionInherited)
        );
    }
}
