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

use crate::error::CliError;
use crate::mcp::{
    ProductionMcpRuntime, load_configured_mcp_registry, mcp_model_tool_name,
    native_model_tool_name, remote_function_tool,
};
use crate::tools::runner::ProductionTaskRunner;
use crate::tools::task::{TaskParentSelection, default_model, register_production_task_tool};
use crate::{Bootstrap, RegisteredNativeTool, SharedToolDispatcher, discover_skill_catalog};

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
        .unwrap_or_else(|| default_model(bootstrap))
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
    crate::test_support::note_production_tool_runtime();

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
            discovered_skills = discover_skill_catalog(bootstrap)?.catalog().clone();
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

const DANGEROUS_CHILD_NATIVE_TOOLS: [&str; 9] = [
    "native::read",
    "native::list",
    "native::search",
    "native::glob",
    "native::grep",
    "native::write",
    "native::edit",
    "native::bash",
    "native::webfetch",
];

pub(crate) fn production_dangerous_child_tool_runtime(
    project_root: &Path,
    tool_limits: ToolLimitSettings,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    let catalog = Arc::new(Mutex::new(NativeToolCatalog::new(open_native_tools(
        project_root,
        tool_limits,
    )?)));
    let metadata = NativeToolCatalog::metadata();
    let mut provider_tools = Vec::with_capacity(DANGEROUS_CHILD_NATIVE_TOOLS.len());
    let mut dispatcher = ToolDispatcher::new();

    for name in DANGEROUS_CHILD_NATIVE_TOOLS {
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

pub(crate) fn is_dangerous_child_native_tool(name: &str) -> bool {
    DANGEROUS_CHILD_NATIVE_TOOLS.iter().any(|registered| {
        name == *registered || name == registered.strip_prefix("native::").unwrap_or_default()
    })
}
