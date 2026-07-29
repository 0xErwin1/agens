//! Production task-tool wiring: assembles the isolated task runtime used by
//! the TUI (permission gate, resolver, and dispatcher) and registers the
//! `task`/`task_control`/`task_message` tools with the parent dispatcher.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_core::{PermissionMode, PermissionSession};
use agens_providers::{OpenAiFunctionTool, ProviderDiagnosticClass};
use agens_store::PermissionGrantStore;
use agens_tools::{
    SkillCatalog, TaskControlTool, TaskExecutionRegistry, TaskMessageSource, TaskMessageTool,
    TaskModelResolutionError, TaskRunner, TaskTool, ToolDispatcher,
};

use crate::runner::{ProductionTaskRunner, TuiTaskLifecycleBridge};
use crate::runtime::production_tool_runtime_with_parent_task_runner;
use agens_agents::{TaskModelValidator, task_agent_catalog, task_model_catalog};
use agens_bootstrap::Bootstrap;
use agens_diagnostics::{next_diagnostic_reference, record_subagent_terminal};
use agens_dispatch::{AuthorizedNativeTaskRuntime, ProductionToolDispatcher};
use agens_error::CliError;
use agens_models::{default_model, unknown_provider_message};
use agens_permissions::PermissionPrompter;
use agens_permissions::{
    ProductionPermissionGate, ProductionPermissionResolver, ProductionPromptAuthorization,
    SharedToolDispatcher, permission_policy,
};

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

pub fn production_tui_task_runtime(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    prompter: Box<dyn PermissionPrompter>,
    lifecycle_bridge: TuiTaskLifecycleBridge,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: String,
) -> Result<ProductionTuiTaskRuntime, CliError> {
    production_tui_task_runtime_with_runner_and_parent_config(
        bootstrap,
        project_root,
        skills,
        prompter,
        ProductionTaskRunner::new(bootstrap.clone(), project_root.to_path_buf())
            .with_lifecycle_bridge(lifecycle_bridge),
        parent_request_config,
        Some(model_resolution_reference),
    )
}

pub fn production_tui_task_runtime_with_runner(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    prompter: Box<dyn PermissionPrompter>,
    task_runner: ProductionTaskRunner,
) -> Result<ProductionTuiTaskRuntime, CliError> {
    production_tui_task_runtime_with_runner_and_parent_config(
        bootstrap,
        project_root,
        skills,
        prompter,
        task_runner,
        agens_core::RequestConfig::default(),
        None,
    )
}

pub fn production_tui_task_runtime_with_runner_and_parent_config(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    prompter: Box<dyn PermissionPrompter>,
    task_runner: ProductionTaskRunner,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
) -> Result<ProductionTuiTaskRuntime, CliError> {
    let task_registry = task_runner.execution_registry().unwrap_or_default();
    let parent_model = match bootstrap.model() {
        Some(model) => model.to_owned(),
        None => default_model(bootstrap.provider_type())
            .ok_or_else(|| {
                CliError::configuration(unknown_provider_message(bootstrap.provider_type()))
            })?
            .to_owned(),
    };
    let (provider_tools, dispatcher) = production_tool_runtime_with_parent_task_runner(
        bootstrap,
        project_root,
        Some(skills),
        parent_model,
        parent_request_config,
        model_resolution_reference,
        task_runner,
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
    let session = PermissionSession::new();
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
            session: PermissionSession::new(),
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
    let available_models = task_model_catalog(bootstrap)?;
    let validator = TaskModelValidator::new(&available_models);
    let agents = task_agent_catalog(bootstrap, project_root)?;
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
        available_models,
        validator,
        task_runner,
    )
    .with_model_resolution_diagnostics(move |error| match error {
        TaskModelResolutionError::ModelUnavailable => {
            let reference = parent
                .diagnostic_reference
                .clone()
                .unwrap_or_else(next_diagnostic_reference);
            record_subagent_terminal(
                &diagnostic_bootstrap,
                &reference,
                ProviderDiagnosticClass::ModelUnavailable,
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
            "Dispatch an isolated eligible subagent task in the foreground or background",
            input_schema,
        )
        .map_err(|_| CliError::configuration("task tool is unavailable"))?,
    );
    dispatcher
        .register_native("native::task", agens_core::ToolAccess::Write, task)
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;

    register_task_coordination_tools(
        dispatcher,
        provider_tools,
        task_registry,
        TaskMessageSource::Main,
    )
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
            "Inspect, background, or cancel a live subagent execution",
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
