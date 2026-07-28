//! Production native-tool runtime construction: opens the confined native
//! tool catalog, wires the task and skill tools, and assembles the
//! read-only/dangerous child-process tool sets used by subagent turns.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_config::{SubagentSettings, ToolLimitSettings};
use agens_providers::OpenAiFunctionTool;
use agens_tools::{
    NativeToolCatalog, NativeTools, SkillCatalog, SkillResourceTool, TaskControlTool,
    TaskExecutionRegistry, TaskMessageSource, TaskMessageTool, TaskRunner, ToolDispatcher,
};

use crate::Bootstrap;
use crate::mcp::{
    ProductionMcpRuntime, load_configured_mcp_registry, mcp_model_tool_name,
    native_model_tool_name, remote_function_tool,
};
use crate::tools::runner::ProductionTaskRunner;
use crate::tools::task::{TaskParentSelection, register_production_task_tool};
use agens_bootstrap::discover_skill_catalog;
use agens_dispatch::RegisteredNativeTool;
use agens_error::CliError;
use agens_models::default_model;
use agens_permissions::SharedToolDispatcher;

/// Converts configured tool bounds into the runtime shape the tools crate owns.
pub(crate) fn native_tool_limits(settings: ToolLimitSettings) -> agens_tools::NativeToolLimits {
    agens_tools::NativeToolLimits {
        max_list_entries: settings.max_list_entries,
        max_search_entries: settings.max_search_entries,
        max_search_results: settings.max_search_results,
        max_search_depth: settings.max_search_depth,
        operation_timeout: std::time::Duration::from_millis(settings.operation_timeout_ms),
        bash_timeout: std::time::Duration::from_millis(settings.bash_timeout_ms),
    }
}

/// Converts configured subagent bounds into the runtime shape the task tool
/// owns. The `[subagents]` table names the user-facing concept; the registry
/// names the mechanism that enforces it.
pub(crate) fn task_execution_limits(
    settings: SubagentSettings,
) -> agens_tools::TaskExecutionLimits {
    agens_tools::TaskExecutionLimits {
        max_iterations: settings.max_iterations,
        max_concurrency: settings.max_concurrency,
        max_output_chars: settings.max_output_chars,
    }
}

pub(crate) fn open_native_tools(
    project_root: &Path,
    settings: ToolLimitSettings,
) -> Result<NativeTools, CliError> {
    NativeTools::open_with_limits(project_root, native_tool_limits(settings))
        .map_err(|_| CliError::configuration("native tools are unavailable"))
}

pub(crate) fn production_tool_runtime(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    production_tool_runtime_with_task_runner(
        bootstrap,
        project_root,
        skills,
        ProductionTaskRunner::new(bootstrap.clone(), project_root.to_path_buf()),
    )
}

pub(crate) fn production_tool_runtime_for_parent(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    parent_model: String,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    production_tool_runtime_with_parent_task_runner(
        bootstrap,
        project_root,
        skills,
        parent_model,
        parent_request_config,
        model_resolution_reference,
        ProductionTaskRunner::new(bootstrap.clone(), project_root.to_path_buf()),
    )
}

pub(crate) fn production_tool_runtime_with_task_runner<R: TaskRunner>(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    task_runner: R,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    let parent_model = bootstrap
        .model()
        .unwrap_or_else(|| default_model(bootstrap.provider_type()))
        .to_owned();
    production_tool_runtime_with_parent_task_runner(
        bootstrap,
        project_root,
        skills,
        parent_model,
        agens_core::RequestConfig::default(),
        None,
        task_runner,
    )
}

pub(crate) fn production_tool_runtime_with_parent_task_runner<R: TaskRunner>(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    parent_model: String,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
    task_runner: R,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    #[cfg(test)]
    crate::test_counters::note_production_tool_runtime();

    let native_catalog = Arc::new(Mutex::new(NativeToolCatalog::new(open_native_tools(
        project_root,
        bootstrap.tool_limits(),
    )?)));
    let mcp_registry = Arc::new(Mutex::new(load_configured_mcp_registry(
        bootstrap,
        project_root,
    )));
    let mut dispatcher = ToolDispatcher::new();
    let mut provider_tools = BTreeMap::new();
    let discovered_skills;
    let skills = match skills {
        Some(skills) => skills,
        None => {
            discovered_skills = discover_skill_catalog(bootstrap, project_root)?
                .catalog()
                .clone();
            &discovered_skills
        }
    };

    for metadata in NativeToolCatalog::metadata() {
        let model_name = native_model_tool_name(&metadata.qualified_name)?;
        provider_tools.insert(
            model_name.clone(),
            OpenAiFunctionTool::new(model_name, metadata.description, metadata.input_schema)
                .map_err(|_| CliError::configuration("native tools are unavailable"))?,
        );
        dispatcher
            .register_native(
                metadata.qualified_name.clone(),
                metadata.access,
                RegisteredNativeTool {
                    name: metadata.qualified_name,
                    catalog: Arc::clone(&native_catalog),
                },
            )
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    }

    provider_tools.insert(
        "skill".into(),
        OpenAiFunctionTool::new(
            "skill",
            "Load selected skill instructions or a declared reference, script, or asset as text",
            SkillResourceTool::input_schema(),
        )
        .map_err(|_| CliError::configuration("skill tool is unavailable"))?,
    );
    dispatcher
        .register_native(
            "native::skill",
            agens_core::ToolAccess::ReadOnly,
            SkillResourceTool::new(skills.clone()),
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;

    register_production_task_tool(
        bootstrap,
        project_root,
        skills,
        &mut dispatcher,
        &mut provider_tools,
        TaskParentSelection {
            model: parent_model,
            request_config: parent_request_config,
            diagnostic_reference: model_resolution_reference,
        },
        task_runner,
    )?;

    let mut runtime = ProductionMcpRuntime {
        registry: mcp_registry,
        dispatcher: Arc::new(Mutex::new(dispatcher)),
    };
    let remote_tools = runtime.discover_configured_tools()?;

    for metadata in remote_tools {
        let model_name = mcp_model_tool_name(&metadata);
        provider_tools.insert(
            model_name.clone(),
            remote_function_tool(&metadata, model_name)?,
        );
    }

    Ok((provider_tools.into_values().collect(), runtime.dispatcher))
}

fn production_read_only_tool_runtime(
    project_root: &Path,
    tool_limits: ToolLimitSettings,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    let catalog = Arc::new(Mutex::new(NativeToolCatalog::new(open_native_tools(
        project_root,
        tool_limits,
    )?)));
    let metadata = NativeToolCatalog::metadata()
        .into_iter()
        .find(|metadata| metadata.qualified_name == "native::read")
        .ok_or_else(|| CliError::configuration("native read tool is unavailable"))?;
    let name = native_model_tool_name(&metadata.qualified_name)?;
    let tool = OpenAiFunctionTool::new(name.clone(), metadata.description, metadata.input_schema)
        .map_err(|_| CliError::configuration("native tools are unavailable"))?;
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(
            "native::read",
            metadata.access,
            RegisteredNativeTool {
                name: "native::read".into(),
                catalog,
            },
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;

    Ok((vec![tool], Arc::new(Mutex::new(dispatcher))))
}

pub(crate) fn production_dangerous_child_tool_runtime(
    project_root: &Path,
    tool_limits: ToolLimitSettings,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    let catalog = Arc::new(Mutex::new(NativeToolCatalog::new(open_native_tools(
        project_root,
        tool_limits,
    )?)));
    let metadata = NativeToolCatalog::metadata();
    let mut provider_tools =
        Vec::with_capacity(agens_permissions::DANGEROUS_CHILD_NATIVE_TOOLS.len());
    let mut dispatcher = ToolDispatcher::new();

    for name in agens_permissions::DANGEROUS_CHILD_NATIVE_TOOLS {
        let metadata = metadata
            .iter()
            .find(|metadata| metadata.qualified_name == name)
            .ok_or_else(|| CliError::configuration("dangerous child native tool is unavailable"))?;
        let model_name = native_model_tool_name(&metadata.qualified_name)?;
        provider_tools.push(
            OpenAiFunctionTool::new(
                model_name,
                metadata.description.clone(),
                metadata.input_schema.clone(),
            )
            .map_err(|_| CliError::configuration("native tools are unavailable"))?,
        );
        dispatcher
            .register_native(
                metadata.qualified_name.clone(),
                metadata.access,
                RegisteredNativeTool {
                    name: metadata.qualified_name.clone(),
                    catalog: Arc::clone(&catalog),
                },
            )
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    }

    Ok((provider_tools, Arc::new(Mutex::new(dispatcher))))
}

pub(crate) fn production_child_tool_runtime(
    project_root: &Path,
    tool_limits: ToolLimitSettings,
    dangerous_mode: bool,
    task_registry: TaskExecutionRegistry,
    execution_id: agens_tools::TaskExecutionId,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    let (mut provider_tools, dispatcher) = if dangerous_mode {
        production_dangerous_child_tool_runtime(project_root, tool_limits)
    } else {
        production_read_only_tool_runtime(project_root, tool_limits)
    }?;
    provider_tools.push(
        OpenAiFunctionTool::new(
            "task_control",
            "Inspect, background, or cancel this subagent execution",
            TaskControlTool::input_schema(),
        )
        .map_err(|_| CliError::configuration("task control tool is unavailable"))?,
    );
    provider_tools.push(
        OpenAiFunctionTool::new(
            "task_message",
            "Queue a bounded coordination message for the main agent",
            TaskMessageTool::input_schema(),
        )
        .map_err(|_| CliError::configuration("task message tool is unavailable"))?,
    );
    let mut dispatcher_guard = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is unavailable"))?;
    dispatcher_guard
        .register_native(
            "native::task_control",
            agens_core::ToolAccess::Write,
            TaskControlTool::new(
                task_registry.clone(),
                TaskMessageSource::Execution(execution_id),
            ),
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    dispatcher_guard
        .register_native(
            "native::task_message",
            agens_core::ToolAccess::Write,
            TaskMessageTool::new(task_registry, TaskMessageSource::Execution(execution_id)),
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    drop(dispatcher_guard);

    Ok((provider_tools, dispatcher))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use agens_core::{
        HeadlessTurnCancellation, PermissionDecision, PermissionMode, PermissionPattern,
        PermissionPolicy, PermissionRule, PermissionSession,
    };
    use agens_tools::{
        TaskExecutionRegistry, TaskLaunchMode, TaskRunContext, TaskRunner, TaskRunnerError,
        TaskTurnRequest, TaskTurnResult, ToolDispatchRequest, ToolEvaluationOutcome,
        ToolExecutionContext,
    };

    use super::*;
    use crate::test_support::{
        bootstrap_from_configuration, tui_session_bootstrap, tui_session_bootstrap_for_provider,
        tui_session_directory,
    };
    use agens_agents::task_model_catalog;

    #[test]
    fn configured_tool_limits_reach_the_native_tool_runtime() {
        let bootstrap = bootstrap_from_configuration(
            "config-tool-limits",
            Some(
                "[tools]\nmax_list_entries = 5\nmax_search_entries = 6\nmax_search_results = 7\nmax_search_depth = 8\noperation_timeout_ms = 900\nbash_timeout_ms = 1500\n",
            ),
            None,
        );

        let limits = native_tool_limits(bootstrap.tool_limits());

        assert_eq!(limits.max_list_entries, 5);
        assert_eq!(limits.max_search_entries, 6);
        assert_eq!(limits.max_search_results, 7);
        assert_eq!(limits.max_search_depth, 8);
        assert_eq!(
            limits.operation_timeout,
            std::time::Duration::from_millis(900)
        );
        assert_eq!(limits.bash_timeout, std::time::Duration::from_millis(1_500));
    }

    #[test]
    fn default_configuration_keeps_the_runtime_tool_limits_unchanged() {
        let bootstrap = bootstrap_from_configuration("config-tool-defaults", None, None);

        assert_eq!(
            native_tool_limits(bootstrap.tool_limits()),
            agens_tools::NativeToolLimits::default()
        );
    }

    #[test]
    fn configured_subagent_limits_bound_the_task_registry() {
        let bootstrap = bootstrap_from_configuration(
            "config-subagent-limits",
            Some("[subagents]\nmax_iterations = 3\nmax_concurrency = 1\nmax_output_chars = 2048\n"),
            None,
        );

        let registry =
            TaskExecutionRegistry::with_limits(task_execution_limits(bootstrap.subagent_limits()));

        assert_eq!(registry.limits().max_iterations, 3);
        assert_eq!(registry.limits().max_output_chars, 2_048);
        assert!(registry.admit(TaskLaunchMode::Background).is_some());
        assert!(registry.admit(TaskLaunchMode::Background).is_none());
    }

    #[test]
    fn default_configuration_keeps_the_runtime_subagent_limits_unchanged() {
        let bootstrap = bootstrap_from_configuration("config-subagent-defaults", None, None);

        assert_eq!(
            task_execution_limits(bootstrap.subagent_limits()),
            agens_tools::TaskExecutionLimits::default()
        );
    }

    #[test]
    fn dangerous_child_catalog_is_exact_and_never_recursive() {
        let temporary = tui_session_directory("dangerous-child-catalog");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let (provider_tools, dispatcher) =
            production_dangerous_child_tool_runtime(&project_root, ToolLimitSettings::default())
                .unwrap();
        let provider_names = provider_tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        assert_eq!(
            provider_names,
            [
                "read", "git_read", "list", "search", "glob", "grep", "write", "edit", "bash",
                "webfetch",
            ]
        );

        let dispatcher = dispatcher.lock().unwrap();
        for name in [
            "read", "list", "search", "glob", "grep", "write", "edit", "bash", "webfetch",
        ] {
            assert_eq!(
                dispatcher.canonical_identity(name),
                dispatcher.canonical_identity(&format!("native::{name}")),
                "{name} must have one native dispatcher identity"
            );
        }
        for rejected in [
            "task",
            "native::task",
            "skill",
            "native::skill",
            "files::first",
            "native::unregistered",
        ] {
            assert_eq!(
                dispatcher.canonical_identity(rejected),
                None,
                "{rejected} must be rejected before execution"
            );
            assert!(
                dispatcher
                    .evaluate(
                        &PermissionPolicy::new(
                            PermissionMode::Edit,
                            vec![PermissionRule::global(
                                PermissionDecision::Allow,
                                PermissionPattern::Any,
                                PermissionPattern::Any,
                            )],
                        ),
                        &[],
                        &PermissionSession::new(),
                        ToolDispatchRequest::new("project", rejected, serde_json::json!({})),
                    )
                    .is_err(),
                "{rejected} must fail dispatcher evaluation before execution"
            );
        }
        drop(dispatcher);

        let task_registry = TaskExecutionRegistry::new();
        let execution_id = task_registry.admit(TaskLaunchMode::Foreground).unwrap();
        let (mode_off_tools, mode_off_dispatcher) = production_child_tool_runtime(
            &project_root,
            ToolLimitSettings::default(),
            false,
            task_registry,
            execution_id,
        )
        .unwrap();
        assert_eq!(
            mode_off_tools
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>(),
            ["read", "task_control", "task_message"]
        );
        assert!(
            mode_off_dispatcher
                .lock()
                .unwrap()
                .canonical_identity("native::read")
                .is_some()
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn production_task_catalog_includes_built_ins_and_dispatches_requested_call() {
        struct RecordingTaskRunner(Arc<Mutex<Vec<(String, TaskLaunchMode)>>>);

        impl TaskRunner for RecordingTaskRunner {
            fn run(
                &self,
                request: TaskTurnRequest,
                context: &TaskRunContext,
            ) -> Result<TaskTurnResult, TaskRunnerError> {
                self.0.lock().unwrap().push((
                    request.agent_name().to_owned(),
                    context.execution().unwrap().mode(),
                ));
                Ok(TaskTurnResult {
                    output: request.description().to_owned(),
                    iterations: 1,
                })
            }
        }

        let temporary = tui_session_directory("conditional-task-catalog");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[
                (
                    "alpha",
                    "---\nname: alpha\ndescription: default\nmode: subagent\npermissions: []\n---\nDefault work.\n",
                ),
                (
                    "reviewer",
                    "---\nname: reviewer\ndescription: review\nmode: subagent\npermissions: []\n---\nReview work.\n",
                ),
            ],
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (provider_tools, dispatcher) = production_tool_runtime_with_task_runner(
            &bootstrap,
            &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
            Some(&SkillCatalog::default()),
            RecordingTaskRunner(Arc::clone(&calls)),
        )
        .unwrap();
        let task = provider_tools
            .iter()
            .find(|tool| tool.name() == "task")
            .expect("eligible catalog should expose task");
        assert_eq!(
            task.description(),
            "Dispatch an isolated eligible subagent task in the foreground or background"
        );
        assert_eq!(
            task.parameters()["properties"]["agent"]["enum"],
            serde_json::json!(["alpha", "explore", "general", "reviewer"])
        );
        assert_eq!(
            task.parameters()["properties"]["model"]["enum"],
            serde_json::json!(task_model_catalog(&bootstrap).unwrap())
        );
        let task_schema = task.parameters().to_string();
        assert!(task_schema.contains("Explore the codebase without modifying files"));
        assert!(task_schema.contains("Handle a general delegated coding task"));
        assert!(!task_schema.contains("You are the read-only exploration subagent"));

        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let cancellation = HeadlessTurnCancellation::new();
        let context = ToolExecutionContext::from_headless_adapter(cancellation.adapter_view());
        let mut dispatcher = dispatcher.lock().unwrap();
        for arguments in [
            serde_json::json!({"agent":"reviewer","background":true,"description":"selected"}),
            serde_json::json!({"description":"default"}),
        ] {
            let ToolEvaluationOutcome::Authorized(handle) = dispatcher
                .evaluate(
                    &policy,
                    &[],
                    &PermissionSession::new(),
                    ToolDispatchRequest::new("project", "task", arguments),
                )
                .unwrap()
            else {
                panic!("provider task call should authorize");
            };
            dispatcher.execute(handle, &context).unwrap();
        }
        drop(dispatcher);

        // A background dispatch reaches the runner on its own thread while the
        // foreground one reaches it inline, so which lands first is not a
        // guarantee this design makes. Wait for both, then compare without
        // ordering.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while calls.lock().unwrap().len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "both task dispatches should reach the runner, saw {:?}",
                *calls.lock().unwrap()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut observed = calls.lock().unwrap().clone();
        observed.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            observed,
            vec![
                ("alpha".to_owned(), TaskLaunchMode::Foreground),
                ("reviewer".to_owned(), TaskLaunchMode::Background),
            ]
        );

        for provider in ["openai-api", "openai-chatgpt"] {
            let provider_temporary = tui_session_directory(provider);
            let bootstrap =
                tui_session_bootstrap_for_provider(&provider_temporary, &[], provider, "gpt-4.1");
            let (provider_tools, dispatcher) = production_tool_runtime_with_task_runner(
                &bootstrap,
                &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
                Some(&SkillCatalog::default()),
                RecordingTaskRunner(Arc::new(Mutex::new(Vec::new()))),
            )
            .unwrap();

            let task = provider_tools
                .iter()
                .find(|tool| tool.name() == "task")
                .expect("built-in subagents should expose task for both providers");
            assert_eq!(
                task.parameters()["properties"]["agent"]["enum"],
                serde_json::json!(["explore", "general"])
            );
            assert_eq!(
                task.parameters()["properties"]["model"]["enum"],
                serde_json::json!(task_model_catalog(&bootstrap).unwrap())
            );
            assert!(
                task.parameters()["properties"]["model"]["enum"]
                    .as_array()
                    .is_some_and(|models| !models.is_empty() && models.len() <= 256)
            );
            assert!(
                dispatcher
                    .lock()
                    .unwrap()
                    .canonical_identity("native::task")
                    .is_some()
            );

            std::fs::remove_dir_all(provider_temporary).unwrap();
        }

        let override_temporary = tui_session_directory("overridden-built-ins");
        let bootstrap = tui_session_bootstrap(
            &override_temporary,
            &[
                (
                    "explore",
                    "---\nname: explore\ndescription: primary override\nmode: primary\npermissions: []\n---\nPrimary work.\n",
                ),
                (
                    "general",
                    "---\nname: general\ndescription: all override\nmode: all\npermissions: []\n---\nAll work.\n",
                ),
            ],
        );
        let (provider_tools, dispatcher) = production_tool_runtime_with_task_runner(
            &bootstrap,
            &agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap),
            Some(&SkillCatalog::default()),
            RecordingTaskRunner(Arc::new(Mutex::new(Vec::new()))),
        )
        .unwrap();

        assert!(provider_tools.iter().all(|tool| tool.name() != "task"));
        let dispatcher = dispatcher.lock().unwrap();
        assert_eq!(dispatcher.canonical_identity("task"), None);
        assert_eq!(dispatcher.canonical_identity("native::task"), None);
        drop(dispatcher);
        std::fs::remove_dir_all(override_temporary).unwrap();

        std::fs::remove_dir_all(temporary).unwrap();
    }
}
