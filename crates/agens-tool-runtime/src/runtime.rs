//! Production native-tool runtime construction: opens the confined native
//! tool catalog, wires the task and skill tools, and assembles the
//! read-only/dangerous child-process tool sets used by subagent turns.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agens_config::{SubagentSettings, ToolLimitSettings};
use agens_core::ask_user::{AskUserPort, UnavailableAskUserPort};
use agens_providers::OpenAiFunctionTool;
use agens_tools::{
    AskUserTool, NativeToolCatalog, NativeTools, SessionWorktrees, SkillCatalog, SkillResourceTool,
    TaskControlTool, TaskExecutionRegistry, TaskMessageSource, TaskMessageTool, TaskRunner,
    ToolDispatcher, WorkingDirectory,
};

use crate::mcp::{
    ProductionMcpRuntime, load_configured_mcp_registry, mcp_model_tool_name,
    native_model_tool_name, remote_function_tool,
};
use crate::runner::ProductionTaskRunner;
use crate::task::{TaskParentSelection, register_production_task_tool};
use agens_bootstrap::Bootstrap;
use agens_bootstrap::discover_skill_catalog;
use agens_dispatch::RegisteredNativeTool;
use agens_error::CliError;
use agens_permissions::SharedToolDispatcher;
use agens_session::provider::{bootstrap_authentication, resolve_provider_for_model};

/// Converts configured tool bounds into the runtime shape the tools crate owns.
pub fn native_tool_limits(settings: ToolLimitSettings) -> agens_tools::NativeToolLimits {
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
pub fn task_execution_limits(settings: SubagentSettings) -> agens_tools::TaskExecutionLimits {
    agens_tools::TaskExecutionLimits {
        check_interval: settings.check_interval,
        max_concurrency: settings.max_concurrency,
        max_output_chars: settings.max_output_chars,
    }
}

pub fn open_native_tools(
    project_root: &Path,
    settings: ToolLimitSettings,
) -> Result<NativeTools, CliError> {
    NativeTools::open_with_limits(project_root, native_tool_limits(settings))
        .map_err(|_| CliError::configuration("native tools are unavailable"))
}

/// Opens the session's own native tools: confined to `project_root`, able to
/// create worktrees for it, and reopened wherever the session was left.
///
/// A tool runtime is built again for every turn, so the directory the session
/// moved to has to be re-entered here, or the session walks back to its root
/// between one prompt and the next.
fn open_session_native_tools(
    bootstrap: &Bootstrap,
    project_root: &Path,
    settings: ToolLimitSettings,
    working_directory: Option<WorkingDirectory>,
) -> Result<NativeTools, CliError> {
    let mut tools = open_native_tools(project_root, settings)?
        .with_worktrees(
            SessionWorktrees::new(bootstrap.data_directory()),
            session_worktree_repository_id(project_root),
        )
        .map_err(|_| CliError::configuration("session worktrees are unavailable"))?;

    if let Some(working_directory) = working_directory {
        let resumed = working_directory.current();
        if resumed != project_root {
            // A directory that is gone, or no longer reachable, leaves the
            // session at its root rather than failing the turn it was about
            // to take. The handle is corrected to match, so nothing reading it
            // afterwards reports a place the session is not.
            let reopened = tools
                .change_directory(&resumed)
                .is_ok_and(|outcome| !outcome.is_error);
            if !reopened {
                working_directory.moved_to(project_root);
            }
        }
        tools = tools.with_published_directory(working_directory);
    }

    Ok(tools)
}

/// A stable, file-name-safe name for one project's session worktrees, so two
/// projects never share a worktree directory and the same project always
/// finds its own.
fn session_worktree_repository_id(project_root: &Path) -> String {
    use sha2::{Digest, Sha256};

    Sha256::digest(project_root.display().to_string().as_bytes())
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn production_tool_runtime(
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

pub fn production_tool_runtime_for_parent(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    parent_model: String,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    production_tool_runtime_for_parent_with_cancellation(
        bootstrap,
        project_root,
        skills,
        parent_model,
        parent_request_config,
        model_resolution_reference,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
}

pub fn production_tool_runtime_for_parent_with_cancellation(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    parent_model: String,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
    discovery_cancellation: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    production_tool_runtime_with_discovery_cancellation(
        bootstrap,
        project_root,
        skills,
        parent_model,
        parent_request_config,
        model_resolution_reference,
        ProductionTaskRunner::new(bootstrap.clone(), project_root.to_path_buf()),
        Box::new(UnavailableAskUserPort),
        None,
        discovery_cancellation,
    )
}

pub fn production_tool_runtime_with_task_runner<R: TaskRunner>(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    task_runner: R,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    // The provider comes from the model, so an absent model has to be resolved
    // the same way a turn resolves one: through what this run can authenticate.
    let parent_model = match bootstrap.model() {
        Some(model) => model.to_owned(),
        None => {
            resolve_provider_for_model(None, &bootstrap_authentication(bootstrap))
                .map_err(|error| CliError::configuration(error.message()))?
                .model
        }
    };
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

pub fn production_tool_runtime_with_parent_task_runner<R: TaskRunner>(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    parent_model: String,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
    task_runner: R,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    production_tool_runtime_with_parent_task_runner_and_ask_user(
        bootstrap,
        project_root,
        skills,
        parent_model,
        parent_request_config,
        model_resolution_reference,
        task_runner,
        Box::new(UnavailableAskUserPort),
    )
}

/// Same as [`production_tool_runtime_with_parent_task_runner_and_ask_user`], but
/// MCP discovery observes `discovery_cancellation` instead of a dead flag.
#[allow(clippy::too_many_arguments)]
pub fn production_tool_runtime_with_discovery_cancellation<R: TaskRunner>(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    parent_model: String,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
    task_runner: R,
    ask_user: Box<dyn AskUserPort>,
    working_directory: Option<WorkingDirectory>,
    discovery_cancellation: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    production_tool_runtime_inner(
        bootstrap,
        project_root,
        skills,
        parent_model,
        parent_request_config,
        model_resolution_reference,
        task_runner,
        ask_user,
        working_directory,
        discovery_cancellation,
    )
}

/// Same as [`production_tool_runtime_with_parent_task_runner`], but lets the caller supply the
/// `ask_user` port instead of always defaulting to [`UnavailableAskUserPort`]. Kept as a
/// separate function rather than adding a parameter to the existing one, so every one of that
/// function's ~20 existing callers keeps compiling unchanged.
#[allow(clippy::too_many_arguments)]
pub fn production_tool_runtime_with_parent_task_runner_and_ask_user<R: TaskRunner>(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    parent_model: String,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
    task_runner: R,
    ask_user: Box<dyn AskUserPort>,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    production_tool_runtime_inner(
        bootstrap,
        project_root,
        skills,
        parent_model,
        parent_request_config,
        model_resolution_reference,
        task_runner,
        ask_user,
        None,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
}

#[allow(clippy::too_many_arguments)]
fn production_tool_runtime_inner<R: TaskRunner>(
    bootstrap: &Bootstrap,
    project_root: &Path,
    skills: Option<&SkillCatalog>,
    parent_model: String,
    parent_request_config: agens_core::RequestConfig,
    model_resolution_reference: Option<String>,
    mut task_runner: R,
    ask_user: Box<dyn AskUserPort>,
    working_directory: Option<WorkingDirectory>,
    discovery_cancellation: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    agens_callcount::note_tool_runtime_build();

    let native_catalog = Arc::new(Mutex::new(NativeToolCatalog::new(
        open_session_native_tools(
            bootstrap,
            project_root,
            bootstrap.tool_limits(),
            working_directory,
        )?,
    )));
    let mcp_registry = Arc::new(Mutex::new(load_configured_mcp_registry(
        bootstrap,
        project_root,
    )));
    mcp_registry
        .lock()
        .map_err(|_| CliError::configuration("MCP tools are unavailable"))?
        .set_discovery_cancellation(discovery_cancellation);
    // Handed over before the task tool is built, so every child this runner
    // launches dispatches through these connections rather than its own.
    task_runner.share_mcp_registry(Arc::clone(&mcp_registry));
    let mut dispatcher = ToolDispatcher::new();
    dispatcher.declare_mcp_servers(
        bootstrap
            .mcp_servers
            .iter()
            .map(|server| server.name.clone()),
    );
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
            SkillResourceTool::new(skills.clone(), project_root),
        )
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;

    provider_tools.insert(
        "ask_user".into(),
        OpenAiFunctionTool::new(
            "ask_user",
            "Ask the person at the terminal one or more bounded structured questions",
            AskUserTool::input_schema(),
        )
        .map_err(|_| CliError::configuration("ask_user tool is unavailable"))?,
    );
    dispatcher
        .register_native(
            "native::ask_user",
            agens_core::ToolAccess::ReadOnly,
            AskUserTool::new(ask_user),
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
    // Per-server discovery reports are surfaced through the shared
    // `McpStatusHandle` that `runtime.registry` already writes to; the caller
    // here only needs the merged tool metadata to build the provider's tool
    // list.
    let (remote_tools, _reports) = runtime.discover_configured_tools()?;

    for metadata in remote_tools {
        let model_name = mcp_model_tool_name(&metadata);
        provider_tools.insert(
            model_name.clone(),
            remote_function_tool(&metadata, model_name)?,
        );
    }

    Ok((provider_tools.into_values().collect(), runtime.dispatcher))
}

pub fn production_dangerous_child_tool_runtime(
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

/// `skills` is the catalog the child's `skill` tool reads from. A child that
/// is handed none holds the tool over an empty catalog rather than not holding
/// it: "this agent has no skills installed" and "this agent cannot load the
/// skills its own instructions name" are different failures, and only the
/// first one is a configuration.
/// `mcp_registry` is the parent's own, shared rather than rebuilt: the child
/// runs on a thread of this process, and the registry's discovery is
/// idempotent, so sharing it registers the servers' tools on this dispatcher
/// without a second connection to any of them. Which of those tools the child
/// is actually offered is `surface.remote_tools`, already narrowed by the same
/// declarations that narrowed the natives.
/// How far delegation may go: the parent turn is depth 0, so its child is 1
/// and that child's child is 2. A runtime at [`MAX_DELEGATION_DEPTH`] holds no
/// `task` tool, which is what stops the chain — an agent that cannot see the
/// tool cannot call it, so the limit is enforced by absence rather than by a
/// refusal the model would be free to retry.
pub const MAX_DELEGATION_DEPTH: usize = 2;

/// What a child runtime needs to be able to delegate one level further.
///
/// `None` where a runtime must not delegate at all, which is every caller that
/// is not a real subagent execution.
pub struct ChildDelegation<'a> {
    pub bootstrap: &'a Bootstrap,
    pub depth: usize,
    /// The child's own model and request config, which its grandchildren
    /// inherit the same way this child inherited the parent's.
    pub parent: TaskParentSelection,
    pub permission_prompter: Option<crate::runner::PrompterFactory>,
    pub ask_user_port: Option<crate::runner::AskUserPortFactory>,
    /// Names this execution on every prompt it raises.
    pub origin: crate::runner::PromptOrigin,
}

#[allow(clippy::too_many_arguments)]
pub fn production_child_tool_runtime(
    project_root: &Path,
    tool_limits: ToolLimitSettings,
    surface: &crate::child_catalog::ChildToolSurface,
    task_registry: TaskExecutionRegistry,
    execution_id: agens_tools::TaskExecutionId,
    skills: Option<&SkillCatalog>,
    mcp_registry: Option<Arc<Mutex<agens_tools::McpRegistry>>>,
    delegation: Option<ChildDelegation<'_>>,
) -> Result<(Vec<OpenAiFunctionTool>, SharedToolDispatcher), CliError> {
    let catalog = Arc::new(Mutex::new(NativeToolCatalog::new(open_native_tools(
        project_root,
        tool_limits,
    )?)));
    let mut provider_tools = Vec::with_capacity(surface.tools.len());
    let mut dispatcher = ToolDispatcher::new();

    for metadata in &surface.tools {
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

    let holds = |tool: &str| surface.coordination_tools.contains(&tool);

    if holds("native::ask_user") {
        // Always registered, never conditional on there being a surface — the
        // parent does the same. A delegation with no port gets
        // `UnavailableAskUserPort`, so a headless child asking gets the
        // domain's own "no interactive surface" answer instead of the tool
        // silently not existing for it.
        let port = delegation
            .as_ref()
            .and_then(|delegation| {
                delegation
                    .ask_user_port
                    .as_ref()
                    .map(|build| build(delegation.origin.clone()))
            })
            .unwrap_or_else(|| Box::new(UnavailableAskUserPort) as Box<dyn AskUserPort>);

        provider_tools.push(
            OpenAiFunctionTool::new(
                "ask_user",
                "Ask the person at the terminal one or more bounded structured questions",
                AskUserTool::input_schema(),
            )
            .map_err(|_| CliError::configuration("ask_user tool is unavailable"))?,
        );
        dispatcher
            .register_native(
                "native::ask_user",
                agens_core::ToolAccess::ReadOnly,
                AskUserTool::new(port),
            )
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    }

    if holds("native::skill") {
        provider_tools.push(
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
                SkillResourceTool::new(skills.cloned().unwrap_or_default(), project_root),
            )
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    }

    if holds("native::task_control") {
        provider_tools.push(
            OpenAiFunctionTool::new(
                "task_control",
                "Inspect, background, or cancel this subagent execution",
                TaskControlTool::input_schema(),
            )
            .map_err(|_| CliError::configuration("task control tool is unavailable"))?,
        );
        dispatcher
            .register_native(
                "native::task_control",
                agens_core::ToolAccess::Write,
                TaskControlTool::new(
                    task_registry.clone(),
                    TaskMessageSource::Execution(execution_id),
                ),
            )
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    }

    if holds("native::task_message") {
        provider_tools.push(
            OpenAiFunctionTool::new(
                "task_message",
                "Queue a bounded coordination message for the main agent",
                TaskMessageTool::input_schema(),
            )
            .map_err(|_| CliError::configuration("task message tool is unavailable"))?,
        );
        dispatcher
            .register_native(
                "native::task_message",
                agens_core::ToolAccess::Write,
                TaskMessageTool::new(
                    task_registry.clone(),
                    TaskMessageSource::Execution(execution_id),
                ),
            )
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;
    }

    if let Some(delegation) = delegation
        && delegation.depth < MAX_DELEGATION_DEPTH
    {
        let mut runner =
            ProductionTaskRunner::new(delegation.bootstrap.clone(), project_root.to_path_buf())
                .with_depth(delegation.depth)
                .with_task_registry(task_registry.clone());
        if let Some(prompter) = delegation.permission_prompter {
            runner = runner.with_permission_prompter(prompter);
        }
        if let Some(registry) = mcp_registry.as_ref() {
            runner.share_mcp_registry(Arc::clone(registry));
        }

        // `register_production_task_tool` writes into the parent path's
        // keyed map; this path collects a list, so the entries are moved
        // across once it has decided whether there is a tool at all — it
        // registers nothing when no subagent is eligible.
        let mut nested = BTreeMap::new();
        crate::task::register_delegating_task_tool(
            delegation.bootstrap,
            project_root,
            skills.unwrap_or(&SkillCatalog::default()),
            &mut dispatcher,
            &mut nested,
            delegation.parent,
            runner,
        )?;
        provider_tools.extend(nested.into_values());
    }

    let Some(mcp_registry) = mcp_registry else {
        return Ok((provider_tools, Arc::new(Mutex::new(dispatcher))));
    };

    let declared = mcp_registry
        .lock()
        .map_err(|_| CliError::configuration("MCP tools are unavailable"))?
        .configured_server_names();
    dispatcher.declare_mcp_servers(declared);

    let mut runtime = ProductionMcpRuntime {
        registry: mcp_registry,
        dispatcher: Arc::new(Mutex::new(dispatcher)),
    };
    // Idempotent against the shared registry: the parent already connected
    // these servers, so this synchronizes their tools onto this dispatcher
    // without a second connection. A server that failed for the parent is
    // already failed here and simply contributes nothing; surfacing its report
    // again is the parent's job, not this child's.
    let (remote_tools, _reports) = runtime.discover_configured_tools()?;

    for metadata in remote_tools {
        if !surface.remote_tools.contains(&metadata.qualified_name) {
            continue;
        }
        let model_name = mcp_model_tool_name(&metadata);
        provider_tools.push(remote_function_tool(&metadata, model_name)?);
    }

    Ok((provider_tools, runtime.dispatcher))
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
    use agens_fixtures::{
        bootstrap_from_configuration, session_bootstrap as tui_session_bootstrap,
        session_bootstrap_for_provider as tui_session_bootstrap_for_provider,
        session_directory as tui_session_directory,
    };

    /// A turn's runtime is built from scratch, so where the session was left
    /// has to be re-entered when the next one opens. Without this the model
    /// would have to move again after every prompt.
    #[test]
    fn a_session_reopens_in_the_directory_its_last_turn_left_it_in() {
        let temporary = tui_session_directory("session-working-directory");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project_root = temporary.join("project");
        let nested = project_root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("inner.txt"), "inner file").unwrap();

        let directory = agens_tools::WorkingDirectory::new(&nested);
        let tools = open_session_native_tools(
            &bootstrap,
            &project_root,
            bootstrap.tool_limits(),
            Some(directory),
        )
        .expect("the session's tools must open");

        assert_eq!(
            tools.working_directory(),
            std::fs::canonicalize(&nested).unwrap()
        );
    }

    /// A directory the session can no longer reach is not a reason to fail the
    /// turn: the session opens at its root and the model can look again.
    #[test]
    fn a_directory_that_went_away_leaves_the_session_at_its_root() {
        let temporary = tui_session_directory("session-working-directory-gone");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let directory = agens_tools::WorkingDirectory::new(project_root.join("removed"));
        let tools = open_session_native_tools(
            &bootstrap,
            &project_root,
            bootstrap.tool_limits(),
            Some(directory),
        )
        .expect("the session's tools must open");

        assert_eq!(
            tools.working_directory(),
            std::fs::canonicalize(&project_root).unwrap()
        );
    }

    #[test]
    fn two_projects_never_share_a_session_worktree_directory() {
        let one = session_worktree_repository_id(Path::new("/projects/agens"));
        let another = session_worktree_repository_id(Path::new("/projects/other"));

        assert_ne!(one, another);
        assert_eq!(
            one,
            session_worktree_repository_id(Path::new("/projects/agens"))
        );
        assert!(one.chars().all(|character| character.is_ascii_hexdigit()));
    }

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
            Some("[subagents]\ncheck_interval = 3\nmax_concurrency = 1\nmax_output_chars = 2048\n"),
            None,
        );

        let registry =
            TaskExecutionRegistry::with_limits(task_execution_limits(bootstrap.subagent_limits()));

        assert_eq!(registry.limits().check_interval, 3);
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

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// The chain stops by absence, not by refusal: a great-grandchild is not
    /// offered `task` at all, so there is no call for it to make and retry.
    /// Both ends are pinned — a limit that also removed the tool from the
    /// child would read as "enforced" while quietly banning delegation
    /// outright.
    #[test]
    fn delegation_reaches_a_grandchild_and_stops_there() {
        let temporary = tui_session_directory("delegation-depth");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "worker",
                "---\nname: worker\ndescription: work\nmode: subagent\npermissions: []\n---\nWork.\n",
            )],
        );
        let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);

        let offers_task = |depth: usize| {
            let surface = crate::child_catalog::resolve_child_surface(&[], &[], &[]).unwrap();
            let task_registry = TaskExecutionRegistry::new();
            let execution_id = task_registry.admit(TaskLaunchMode::Foreground).unwrap();
            let (tools, dispatcher) = production_child_tool_runtime(
                &project_root,
                ToolLimitSettings::default(),
                &surface,
                task_registry,
                execution_id,
                Some(&SkillCatalog::default()),
                None,
                Some(ChildDelegation {
                    bootstrap: &bootstrap,
                    depth,
                    parent: TaskParentSelection {
                        model: "gpt-5.5".into(),
                        request_config: agens_core::RequestConfig::default(),
                        diagnostic_reference: None,
                    },
                    permission_prompter: None,
                    ask_user_port: None,
                    origin: crate::runner::PromptOrigin {
                        execution: 1,
                        agent: "worker".into(),
                    },
                }),
            )
            .expect("the child runtime must build");

            let offered = tools.iter().any(|tool| tool.name() == "task");
            let dispatches = dispatcher
                .lock()
                .unwrap()
                .canonical_identity("native::task")
                .is_some();
            assert_eq!(
                offered, dispatches,
                "a tool offered to the model and one the dispatcher resolves must be the same set"
            );
            offered
        };

        // Literal depths, never `MAX_DELEGATION_DEPTH`: the requirement is
        // about concrete levels — parent, child, grandchild — so asserting
        // against the constant would only prove the code agrees with itself,
        // and would keep passing if the limit moved.
        assert!(offers_task(1), "a child may delegate to a grandchild");
        assert!(
            !offers_task(2),
            "a grandchild is where the chain ends: no great-grandchild"
        );

        std::fs::remove_dir_all(temporary).ok();
    }

    /// A scripted MCP server that answers exactly one connection's worth of
    /// handshake, so a second connection attempt runs out of responses and
    /// fails loudly instead of quietly working.
    struct ScriptedTransport {
        responses: std::cell::RefCell<std::collections::VecDeque<agens_tools::McpResponse>>,
    }

    impl agens_tools::McpTransport for ScriptedTransport {
        fn execute(
            &mut self,
            _: agens_tools::McpRequest,
            _: &agens_tools::McpOperationContext,
        ) -> Result<agens_tools::McpResponse, agens_tools::McpTransportError> {
            self.responses
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| agens_tools::McpTransportError::Protocol("exhausted".into()))
        }

        fn notify(
            &mut self,
            _: agens_tools::McpRequest,
            _: &agens_tools::McpOperationContext,
        ) -> Result<(), agens_tools::McpTransportError> {
            Ok(())
        }

        fn close(
            &mut self,
            _: &agens_tools::McpOperationContext,
        ) -> Result<(), agens_tools::McpTransportError> {
            Ok(())
        }
    }

    /// The whole reason a child shares the parent's registry rather than
    /// loading one from the same configuration: a loaded-again registry has
    /// attempted nothing, so every subagent would connect to every server
    /// afresh. The connection counter is the assertion — the tool showing up
    /// only proves the child can see it, not that seeing it was free.
    #[test]
    fn a_child_reaches_the_parents_mcp_tools_without_connecting_again() {
        let temporary = tui_session_directory("child-mcp-shared");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_connections = Arc::clone(&connections);
        let mut registry = agens_tools::McpRegistry::new();
        registry
            .configure_server(
                "engram",
                move || {
                    factory_connections.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    Ok(Box::new(ScriptedTransport {
                        responses: std::cell::RefCell::new(
                            [
                                agens_tools::McpResponse::Initialized(
                                    agens_tools::McpInitializeResult::new(
                                        agens_tools::MCP_PROTOCOL_VERSION,
                                        serde_json::json!({"tools": {}}),
                                    ),
                                ),
                                agens_tools::McpResponse::ToolsListed(
                                    agens_tools::McpToolsPage::new(
                                        vec![agens_tools::McpToolDefinition {
                                            name: "mem_save".into(),
                                            description: Some("save".into()),
                                            input_schema: serde_json::json!({"type": "object"}),
                                            annotations: agens_tools::McpToolAnnotations {
                                                read_only_hint: Some(false),
                                            },
                                        }],
                                        None,
                                    ),
                                ),
                            ]
                            .into(),
                        ),
                    }) as Box<dyn agens_tools::McpTransport>)
                },
                agens_tools::McpTimeouts::new(
                    std::time::Duration::from_millis(50),
                    std::time::Duration::from_millis(50),
                    std::time::Duration::from_millis(50),
                )
                .unwrap(),
                agens_tools::McpLimits::new(8, 16).unwrap(),
            )
            .expect("the probe server must configure");

        // The parent's own discovery, which is what a real session does at
        // startup.
        assert!(!registry.discover_server("engram").is_failed());
        assert_eq!(
            connections.load(std::sync::atomic::Ordering::Acquire),
            1,
            "the parent connects once"
        );

        let shared = Arc::new(Mutex::new(registry));
        let remote = shared
            .lock()
            .unwrap()
            .tools()
            .into_iter()
            .map(|tool| tool.qualified_name.clone())
            .collect::<Vec<_>>();
        assert_eq!(remote, vec!["engram::mem_save".to_owned()]);

        let surface = crate::child_catalog::resolve_child_surface(&[], &[], &remote).unwrap();
        let task_registry = TaskExecutionRegistry::new();
        let execution_id = task_registry.admit(TaskLaunchMode::Foreground).unwrap();
        let (tools, dispatcher) = production_child_tool_runtime(
            &project_root,
            ToolLimitSettings::default(),
            &surface,
            task_registry,
            execution_id,
            None,
            Some(Arc::clone(&shared)),
            None,
        )
        .unwrap();

        assert!(
            tools.iter().any(|tool| tool.name().contains("mem_save")),
            "the child must be offered the parent's MCP tool: {:?}",
            tools.iter().map(|tool| tool.name()).collect::<Vec<_>>()
        );
        assert_eq!(
            connections.load(std::sync::atomic::Ordering::Acquire),
            1,
            "the child must reuse the parent's connection, not open a second one"
        );
        assert!(
            dispatcher
                .lock()
                .unwrap()
                .canonical_identity("engram::mem_save")
                .is_some(),
            "the child's dispatcher must resolve the remote tool it was offered"
        );

        drop(dispatcher);
        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// Holding `skill` is only half of it: the tool has to read the same
    /// installation the parent reads, or an agent told to open its own skill
    /// still cannot. This drives the catalog in through the runtime rather
    /// than asserting the tool name is present, because the name was never the
    /// part that was missing.
    #[test]
    fn a_child_can_load_a_skill_from_the_catalog_it_was_given() {
        let temporary = tui_session_directory("child-skill-load");
        let project_root = temporary.join("project");
        let skills_root = temporary.join("skills");
        std::fs::create_dir_all(project_root.join(agens_tools::PROJECT_SKILLS_DIRECTORY)).unwrap();
        std::fs::create_dir_all(skills_root.join("sdd-apply")).unwrap();
        std::fs::write(
            skills_root.join("sdd-apply").join("SKILL.md"),
            "---\nname: sdd-apply\ndescription: apply phase\n---\n\nRED then GREEN.\n",
        )
        .unwrap();

        let skills = agens_tools::SkillCatalog::discover(
            skills_root,
            project_root.join(agens_tools::PROJECT_SKILLS_DIRECTORY),
        )
        .expect("the skill catalog must discover")
        .catalog()
        .clone();

        let surface = crate::child_catalog::resolve_child_surface(&[], &[], &[]).unwrap();
        let task_registry = TaskExecutionRegistry::new();
        let execution_id = task_registry.admit(TaskLaunchMode::Foreground).unwrap();
        let (tools, dispatcher) = production_child_tool_runtime(
            &project_root,
            ToolLimitSettings::default(),
            &surface,
            task_registry,
            execution_id,
            Some(&skills),
            None,
            None,
        )
        .unwrap();

        assert!(
            tools.iter().any(|tool| tool.name() == "skill"),
            "the child must be offered the skill tool"
        );

        // Authorized through the child's own policy and an ordinary session,
        // so this also pins that the surface authorizes `skill` rather than
        // leaving it registered and undecided.
        let policy = PermissionPolicy::new(PermissionMode::Edit, surface.rules.clone());
        let cancellation = HeadlessTurnCancellation::new();
        let context = ToolExecutionContext::from_headless_adapter(cancellation.adapter_view());
        let mut dispatcher = dispatcher.lock().expect("dispatcher must be available");
        let ToolEvaluationOutcome::Authorized(handle) = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new(
                    "project",
                    "native::skill",
                    serde_json::json!({"skill": "sdd-apply"}),
                ),
            )
            .expect("the child's skill call must evaluate")
        else {
            panic!("a delegated child must be authorized to load its own skill");
        };

        let output = dispatcher
            .execute(handle, &context)
            .expect("the child's skill tool must execute");

        assert!(
            output.content.contains("RED then GREEN"),
            "the child must read the same skill body the parent would: {}",
            output.content
        );

        drop(dispatcher);
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn child_catalog_inherits_the_parents_surface_by_default() {
        let temporary = tui_session_directory("child-catalog-default");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let surface = crate::child_catalog::resolve_child_surface(&[], &[], &[]).unwrap();
        let task_registry = TaskExecutionRegistry::new();
        let execution_id = task_registry.admit(TaskLaunchMode::Foreground).unwrap();
        let (tools, dispatcher) = production_child_tool_runtime(
            &project_root,
            ToolLimitSettings::default(),
            &surface,
            task_registry,
            execution_id,
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            tools.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            [
                "read",
                "write",
                "edit",
                "list",
                "search",
                "grep",
                "glob",
                "bash",
                "git_read",
                "webfetch",
                "ask_user",
                "skill",
                "task_control",
                "task_message",
            ]
        );
        let dispatcher = dispatcher.lock().unwrap();
        for name in ["native::read", "native::write", "native::bash"] {
            assert!(
                dispatcher.canonical_identity(name).is_some(),
                "{name} must be reachable when nothing narrows the inherited surface"
            );
        }
        drop(dispatcher);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn child_catalog_omits_a_declared_deny() {
        let temporary = tui_session_directory("child-catalog-narrowed");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let surface = crate::child_catalog::resolve_child_surface(
            &[],
            &[
                PermissionRule::global(
                    PermissionDecision::Deny,
                    PermissionPattern::glob("write").unwrap(),
                    PermissionPattern::Any,
                ),
                PermissionRule::global(
                    PermissionDecision::Deny,
                    PermissionPattern::glob("edit").unwrap(),
                    PermissionPattern::Any,
                ),
                PermissionRule::global(
                    PermissionDecision::Deny,
                    PermissionPattern::glob("bash").unwrap(),
                    PermissionPattern::Any,
                ),
                PermissionRule::global(
                    PermissionDecision::Deny,
                    PermissionPattern::glob("webfetch").unwrap(),
                    PermissionPattern::Any,
                ),
            ],
            &[],
        )
        .unwrap();
        let task_registry = TaskExecutionRegistry::new();
        let execution_id = task_registry.admit(TaskLaunchMode::Foreground).unwrap();
        let (tools, dispatcher) = production_child_tool_runtime(
            &project_root,
            ToolLimitSettings::default(),
            &surface,
            task_registry,
            execution_id,
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            tools.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            [
                "read",
                "list",
                "search",
                "grep",
                "glob",
                "git_read",
                "ask_user",
                "skill",
                "task_control",
                "task_message",
            ]
        );
        let dispatcher = dispatcher.lock().unwrap();
        for name in [
            "native::write",
            "native::edit",
            "native::bash",
            "native::webfetch",
        ] {
            assert!(
                dispatcher.canonical_identity(name).is_none(),
                "{name} must be absent from a narrowed child catalog"
            );
        }
        drop(dispatcher);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// A coordination tool is registered by this function rather than read out
    /// of the catalog, which is exactly why a declaration naming one used to
    /// decide nothing. `deny` has to remove it as it removes any other tool.
    #[test]
    fn child_catalog_omits_a_declared_deny_on_a_coordination_tool() {
        let temporary = tui_session_directory("child-catalog-coordination");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let surface = crate::child_catalog::resolve_child_surface(
            &[],
            &[PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::glob("task_control").unwrap(),
                PermissionPattern::Any,
            )],
            &[],
        )
        .unwrap();
        let task_registry = TaskExecutionRegistry::new();
        let execution_id = task_registry.admit(TaskLaunchMode::Foreground).unwrap();
        let (tools, dispatcher) = production_child_tool_runtime(
            &project_root,
            ToolLimitSettings::default(),
            &surface,
            task_registry,
            execution_id,
            None,
            None,
            None,
        )
        .unwrap();

        assert!(tools.iter().all(|tool| tool.name() != "task_control"));
        assert!(tools.iter().any(|tool| tool.name() == "task_message"));
        let dispatcher = dispatcher.lock().unwrap();
        assert_eq!(dispatcher.canonical_identity("native::task_control"), None);
        assert!(
            dispatcher
                .canonical_identity("native::task_message")
                .is_some(),
            "denying one coordination tool must leave the other reachable"
        );
        drop(dispatcher);

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
        assert!(
            task.parameters()["properties"]["model"]["enum"].is_null(),
            "the bundled model catalog is a snapshot, so it must not be published as a closed set"
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
        // The unnamed dispatch lands on `general`, not on `alpha`: an omitted
        // agent means the general-purpose one, never whichever name sorts first.
        assert_eq!(
            observed,
            vec![
                ("general".to_owned(), TaskLaunchMode::Foreground),
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
            assert!(task.parameters()["properties"]["model"]["enum"].is_null());
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

    #[test]
    fn ask_user_has_exactly_one_provider_definition_named_ask_user_not_native_prefixed() {
        let temporary = tui_session_directory("ask-user-provider-definition");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);

        let (provider_tools, dispatcher) = production_tool_runtime_for_parent(
            &bootstrap,
            &project_root,
            Some(&SkillCatalog::default()),
            "gpt-4.1".to_owned(),
            agens_core::RequestConfig::default(),
            None,
        )
        .unwrap();

        let matches: Vec<_> = provider_tools
            .iter()
            .filter(|tool| tool.name() == "ask_user")
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "exactly one provider tool must be named ask_user, saw {:?}",
            provider_tools
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>()
        );
        assert!(
            provider_tools
                .iter()
                .all(|tool| tool.name() != "native::ask_user"),
            "the provider-visible name must have the native:: prefix stripped"
        );
        assert!(
            dispatcher
                .lock()
                .unwrap()
                .canonical_identity("native::ask_user")
                .is_some(),
            "the dispatch identity native::ask_user must be registered"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// `hard_safety_allows` denies a `ToolAccess::Write` tool outright in `PermissionMode::Chat`
    /// regardless of any matching rule. Proving a matching Allow rule still authorizes
    /// `native::ask_user` under Chat mode is therefore proof the tool is registered
    /// `ToolAccess::ReadOnly`, since a `Write` registration would be hard-denied here even with
    /// the rule present.
    #[test]
    fn ask_user_is_registered_read_only_and_survives_chat_mode_hard_safety() {
        let temporary = tui_session_directory("ask-user-read-only-classification");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);

        let (_, dispatcher) = production_tool_runtime_for_parent(
            &bootstrap,
            &project_root,
            Some(&SkillCatalog::default()),
            "gpt-4.1".to_owned(),
            agens_core::RequestConfig::default(),
            None,
        )
        .unwrap();

        let policy = PermissionPolicy::new(
            PermissionMode::Chat,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::ask_user".into()),
                PermissionPattern::Any,
            )],
        );
        let outcome = dispatcher
            .lock()
            .unwrap()
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new("project", "native::ask_user", serde_json::json!({})),
            )
            .unwrap();

        assert!(
            matches!(outcome, ToolEvaluationOutcome::Authorized(_)),
            "a ReadOnly tool with a matching Allow rule must not be hard-denied in Chat mode, saw {outcome:?}"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn ask_user_default_wiring_yields_unavailable_without_blocking() {
        let temporary = tui_session_directory("ask-user-default-unavailable");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);

        let (_, dispatcher) = production_tool_runtime_for_parent(
            &bootstrap,
            &project_root,
            Some(&SkillCatalog::default()),
            "gpt-4.1".to_owned(),
            agens_core::RequestConfig::default(),
            None,
        )
        .unwrap();

        let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
        let cancellation = HeadlessTurnCancellation::new();
        let context = ToolExecutionContext::from_headless_adapter(cancellation.adapter_view());
        let mut dispatcher = dispatcher.lock().unwrap();
        let ToolEvaluationOutcome::Authorized(handle) = dispatcher
            .evaluate(
                &policy,
                &[],
                &PermissionSession::with_temporary_bypass(),
                ToolDispatchRequest::new(
                    "project",
                    "native::ask_user",
                    serde_json::json!({
                        "questions": [{
                            "id": "q",
                            "prompt": "p",
                            "mode": "single",
                            "options": [{"id": "a", "label": "A"}]
                        }]
                    }),
                ),
            )
            .unwrap()
        else {
            panic!("ask_user should authorize under a bypassed session");
        };

        let output = dispatcher.execute(handle, &context).unwrap();
        assert!(!output.is_error);
        assert_eq!(
            output.content,
            "{\"status\":\"unavailable\",\"reason\":\"no interactive surface\"}"
        );
    }

    #[test]
    fn ask_user_is_absent_from_every_child_runtime() {
        let temporary = tui_session_directory("ask-user-absent-from-child-runtimes");
        let project_root = temporary.join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let (dangerous_tools, dangerous_dispatcher) =
            production_dangerous_child_tool_runtime(&project_root, ToolLimitSettings::default())
                .unwrap();
        assert!(dangerous_tools.iter().all(|tool| tool.name() != "ask_user"));
        assert!(
            dangerous_dispatcher
                .lock()
                .unwrap()
                .canonical_identity("native::ask_user")
                .is_none()
        );

        // A child holds `ask_user` only where a port was handed down. The
        // dangerous runtime above never gets one — it has no surface at all —
        // and a delegation with no port is a headless one.
        let surface = crate::child_catalog::resolve_child_surface(&[], &[], &[]).unwrap();
        let task_registry = TaskExecutionRegistry::new();
        let execution_id = task_registry.admit(TaskLaunchMode::Foreground).unwrap();
        let (child_tools, child_dispatcher) = production_child_tool_runtime(
            &project_root,
            ToolLimitSettings::default(),
            &surface,
            task_registry,
            execution_id,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(child_tools.iter().any(|tool| tool.name() == "ask_user"));
        assert!(
            child_dispatcher
                .lock()
                .unwrap()
                .canonical_identity("native::ask_user")
                .is_some(),
            "the tool is always registered; the port decides whether a person is reachable"
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    /// A subagent that hits a real fork in the work can ask, and the question
    /// says who is asking. Both halves matter: an anonymous question put in
    /// front of someone running three subagents is one they cannot answer
    /// responsibly, which is why the port and the origin arrive together.
    #[test]
    fn a_child_given_a_port_can_ask_and_the_question_names_it() {
        let temporary = tui_session_directory("child-ask-user-attributed");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "worker",
                "---\nname: worker\ndescription: work\nmode: subagent\npermissions: []\n---\nWork.\n",
            )],
        );
        let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);

        struct SilentPort;

        impl AskUserPort for SilentPort {
            fn ask(
                &self,
                _: &agens_core::ask_user::AskUserRequest,
                _: &agens_core::HeadlessTurnCancellation,
            ) -> agens_core::ask_user::AskUserReply {
                agens_core::ask_user::AskUserReply::Cancelled
            }
        }

        let seen: Arc<Mutex<Vec<crate::runner::PromptOrigin>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let surface = crate::child_catalog::resolve_child_surface(&[], &[], &[]).unwrap();
        let task_registry = TaskExecutionRegistry::new();
        let execution_id = task_registry.admit(TaskLaunchMode::Foreground).unwrap();

        let (tools, dispatcher) = production_child_tool_runtime(
            &project_root,
            ToolLimitSettings::default(),
            &surface,
            task_registry,
            execution_id,
            Some(&SkillCatalog::default()),
            None,
            Some(ChildDelegation {
                bootstrap: &bootstrap,
                depth: 1,
                parent: TaskParentSelection {
                    model: "gpt-5.5".into(),
                    request_config: agens_core::RequestConfig::default(),
                    diagnostic_reference: None,
                },
                permission_prompter: None,
                ask_user_port: Some(Arc::new(move |origin: crate::runner::PromptOrigin| {
                    recorder
                        .lock()
                        .expect("the record must be available")
                        .push(origin);
                    Box::new(SilentPort) as Box<dyn AskUserPort>
                })),
                origin: crate::runner::PromptOrigin {
                    execution: 7,
                    agent: "reviewer".into(),
                },
            }),
        )
        .expect("the child runtime must build");

        assert!(
            tools.iter().any(|tool| tool.name() == "ask_user"),
            "the child must be offered ask_user"
        );
        assert!(
            dispatcher
                .lock()
                .unwrap()
                .canonical_identity("native::ask_user")
                .is_some()
        );

        // The port is built once, for this execution, and told whose it is —
        // which is what lets the surface render "reviewer is asking" instead of
        // an unattributed question.
        let seen = seen.lock().expect("the record must be available");
        assert_eq!(seen.len(), 1, "one port per delegated execution");
        assert_eq!(seen[0].execution, 7);
        assert_eq!(seen[0].agent, "reviewer");

        drop(seen);
        std::fs::remove_dir_all(temporary).ok();
    }
}
