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

use crate::dispatch::{AuthorizedNativeTaskRuntime, ProductionToolDispatcher};
use crate::session::agents::{TaskModelValidator, task_agent_catalog, task_model_catalog};
use crate::tools::runner::{ProductionTaskRunner, TuiTaskLifecycleBridge};
use crate::tools::runtime::production_tool_runtime_with_parent_task_runner;
use crate::{Bootstrap, next_diagnostic_reference, record_subagent_terminal};
use agens_error::CliError;
use agens_models::default_model;
use agens_permissions::PermissionPrompter;
use agens_permissions::{
    ProductionPermissionGate, ProductionPermissionResolver, ProductionPromptAuthorization,
    SharedToolDispatcher, permission_policy,
};

pub(crate) struct ProductionTuiTaskRuntime {
    pub(crate) provider_tools: Vec<OpenAiFunctionTool>,
    pub(crate) dispatcher: SharedToolDispatcher,
    pub(crate) task_registry: TaskExecutionRegistry,
    #[allow(dead_code)]
    pub(crate) authorized: AuthorizedNativeTaskRuntime<Box<dyn PermissionPrompter>>,
    /// The session root this runtime's dispatcher, permission policy, and grant scope were built
    /// against. The headless turn body reuses this value instead of re-deriving a root, so the
    /// parent turn and this runtime never disagree about which project's grants apply.
    pub(crate) project_root: std::path::PathBuf,
}

pub(crate) struct TaskParentSelection {
    pub(crate) model: String,
    pub(crate) request_config: agens_core::RequestConfig,
    pub(crate) diagnostic_reference: Option<String>,
}

pub(crate) fn production_tui_task_runtime(
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

#[cfg(test)]
pub(crate) fn production_tui_task_runtime_with_runner(
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

pub(crate) fn production_tui_task_runtime_with_runner_and_parent_config(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: &SkillCatalog,
    prompter: Box<dyn PermissionPrompter>,
    task_runner: ProductionTaskRunner,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
) -> Result<ProductionTuiTaskRuntime, CliError> {
    let task_registry = task_runner.execution_registry().unwrap_or_default();
    let parent_model = bootstrap
        .model()
        .unwrap_or_else(|| default_model(bootstrap.provider_type()))
        .to_owned();
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

pub(crate) fn register_production_task_tool<R: TaskRunner>(
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agens_core::{
        HeadlessTurnCancellation, PermissionDecision, PermissionPattern, PermissionPolicy,
        PermissionRule,
    };
    use agens_tools::{
        TaskLaunchMode, ToolDispatchRequest, ToolEvaluationOutcome, ToolExecutionContext,
    };

    use super::*;
    use crate::permission_prompt::{TuiPermissionPrompter, production_tui_permission_bridge};
    use crate::test_support::{tui_session_bootstrap, tui_session_directory};

    #[test]
    fn a_task_runtimes_permission_policy_is_scoped_to_its_own_root_not_the_bootstraps_process_root()
    {
        use std::collections::BTreeMap;
        use std::path::Path;

        use agens_core::{PermissionDecision, PermissionRequest, PermissionSession, ToolAccess};

        use crate::CliDependencies;
        use crate::deps::bootstrap;

        let temporary = std::env::temp_dir().join(format!(
            "agens-task-runtime-permission-scope-{}",
            std::process::id()
        ));
        let config_home = temporary.join("config");
        let root_b = temporary.join("root-b/project");
        let root_a = temporary.join("root-a/project");

        // `config_reader` in this fixture answers for ANY path present in this map, mirroring
        // the production `read_file` capability, which can re-read a different root's document
        // on demand rather than only the one path `bootstrap()` itself resolved.
        let mut files = BTreeMap::new();
        files.insert(
            config_home.join("config.toml"),
            "[provider]\ntype = \"openai-api\"\nmodel = \"gpt-4.1\"\n".to_owned(),
        );
        files.insert(
            root_b.join(".agens/config.toml"),
            "[permissions]\nallow = [\"write\"]\n".to_owned(),
        );
        files.insert(
            root_a.join(".agens/config.toml"),
            "[permissions]\nallow = [\"write\"]\n".to_owned(),
        );

        let bootstrap_from_root_b = bootstrap(&CliDependencies::for_test(
            root_b.clone(),
            Some(temporary.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                config_home.display().to_string(),
            )]),
            files,
        ))
        .unwrap();

        let evaluate_write_decision = |root: &Path| {
            std::fs::create_dir_all(root).unwrap();
            let runtime = production_tui_task_runtime_with_runner_and_parent_config(
                &bootstrap_from_root_b,
                root,
                &SkillCatalog::default(),
                Box::new(TuiPermissionPrompter(production_tui_permission_bridge().0)),
                ProductionTaskRunner::new(bootstrap_from_root_b.clone(), root.to_path_buf()),
                agens_core::RequestConfig::default(),
                None,
            )
            .unwrap();
            let write_identity = runtime
                .dispatcher
                .lock()
                .unwrap()
                .canonical_identity("native::write")
                .unwrap()
                .as_str()
                .to_owned();
            runtime.authorized.gate.policy.evaluate(
                &PermissionRequest::new(
                    root.display().to_string(),
                    write_identity,
                    "notes.md",
                    ToolAccess::Write,
                ),
                &[],
                &PermissionSession::new(),
            )
        };

        // `bootstrap_from_root_b` discovered its process-scoped configuration from root B, which
        // grants `write`. Building a runtime for root A — a DIFFERENT recorded root, which also
        // happens to grant `write` in ITS OWN config — must authorize from root A's own document,
        // never from the bootstrap's process-captured one.
        assert_eq!(
            evaluate_write_decision(&root_a),
            PermissionDecision::Allow,
            "a permission rule written for THIS root's own project config must still authorize"
        );

        // Removing root A's own grant must remove the authorization too, even though the
        // bootstrap's process-captured rules (from root B) still grant `write` unconditionally.
        // If the runtime were still reading `bootstrap.permission_rules()`, this would stay
        // `Allow`.
        let root_a_without_its_own_grant = temporary.join("root-a-bare/project");
        assert_eq!(
            evaluate_write_decision(&root_a_without_its_own_grant),
            PermissionDecision::Ask,
            "a permission rule written for a DIFFERENT project root's config must not silently \
             auto-authorize a tool call in this root's task runtime"
        );

        std::fs::remove_dir_all(&temporary).ok();
        std::fs::remove_dir_all(bootstrap_from_root_b.data_directory()).ok();
    }

    #[test]
    fn u15_a1b1_production_task_runtime_assembles_current_turn_registration() {
        let temporary = tui_session_directory("production-task-runtime");
        let mut bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        bootstrap.model = Some("gpt-5.6-sol".into());
        let probe = Arc::new(Mutex::new(Vec::new()));
        let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);
        let runtime = production_tui_task_runtime_with_runner_and_parent_config(
            &bootstrap,
            &project_root,
            &SkillCatalog::default(),
            Box::new(TuiPermissionPrompter(production_tui_permission_bridge().0)),
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                project_root.clone(),
                Arc::clone(&probe),
            ),
            agens_core::RequestConfig::with_reasoning_effort("high").unwrap(),
            None,
        )
        .unwrap();

        assert!(
            runtime
                .provider_tools
                .iter()
                .any(|tool| tool.name() == "task")
        );
        let mut dispatcher = runtime.dispatcher.lock().unwrap();
        assert_eq!(
            dispatcher.canonical_identity("task"),
            dispatcher.canonical_identity("native::task")
        );
        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let ToolEvaluationOutcome::Authorized(handle) = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new(
                    "project",
                    "native::task",
                    serde_json::json!({"agent":"reviewer","description":"probe"}),
                ),
            )
            .unwrap()
        else {
            panic!("registered task should authorize");
        };
        let cancellation = HeadlessTurnCancellation::new();
        let output = dispatcher
            .execute(
                handle,
                &ToolExecutionContext::from_headless_adapter(cancellation.adapter_view()),
            )
            .unwrap();
        assert_eq!(output.content, "probe");
        let probe = probe.lock().unwrap();
        assert_eq!(probe.len(), 1);
        assert_eq!(probe[0].1, TaskLaunchMode::Foreground);
        assert_eq!(probe[0].2, "gpt-5.6-sol");
        assert_eq!(probe[0].3, Some(agens_core::ReasoningEffort::High));

        std::fs::remove_dir_all(temporary).unwrap();
    }
}
