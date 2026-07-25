//! Native-tool dispatch-table wiring: the registered-tool adapters that bridge native and
//! MCP tools into the shared dispatcher, the production `HeadlessToolDispatcher`, and the
//! authorized subagent-task launch path that ties the permission gate, resolver, and
//! dispatcher together for a single TUI-selected task.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agens_core::{
    HeadlessPermissionGate, HeadlessPermissionResolver, HeadlessToolCall, HeadlessToolDispatcher,
    HeadlessToolOutput, HeadlessTurnCancellation, HeadlessTurnError, HeadlessTurnPortError,
    PermissionDecision,
};
use agens_tools::{
    DispatchTool, McpRegistry, NativeToolCatalog, TaskLaunchMode, ToolExecutionContext, ToolOutput,
};
use agens_tui::TuiSubmitOrigin;

use crate::error::{CliError, ExitStatus};
use crate::permissions::{
    AllowedNativeCall, NativePermissionTarget, PermissionPrompter, ProductionPermissionGate,
    ProductionPermissionResolver, SharedToolDispatcher,
};
use crate::tools::runner::TuiTaskLifecycleBridge;
use crate::tools::task::ProductionTuiTaskRuntime;
use crate::tui::session::TuiSessionContext;

pub(crate) struct RegisteredNativeTool {
    pub(crate) name: String,
    pub(crate) catalog: Arc<Mutex<NativeToolCatalog>>,
}

impl DispatchTool for RegisteredNativeTool {
    fn permission_target(
        &self,
        arguments: &serde_json::Value,
    ) -> Result<String, agens_core::Error> {
        NativePermissionTarget::parse(&self.name, arguments)
            .map(NativePermissionTarget::into_value)
            .map_err(|error| agens_core::Error::Tool(error.to_string()))
    }

    fn execute(
        &mut self,
        context: &ToolExecutionContext,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, agens_core::Error> {
        self.catalog
            .lock()
            .map_err(|_| agens_core::Error::Tool("native tool catalog is unavailable".into()))?
            .execute(&self.name, arguments, context)
    }
}

pub(crate) struct RegisteredMcpTool {
    pub(crate) name: String,
    pub(crate) registry: Arc<Mutex<McpRegistry>>,
}

impl DispatchTool for RegisteredMcpTool {
    fn permission_target(&self, _: &serde_json::Value) -> Result<String, agens_core::Error> {
        Ok(self.name.clone())
    }

    fn execute(
        &mut self,
        context: &ToolExecutionContext,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, agens_core::Error> {
        self.registry
            .lock()
            .map_err(|_| agens_core::Error::Tool("MCP tool registry is unavailable".into()))?
            .call_tool(&self.name, arguments, context)
    }
}

pub(crate) struct ProductionToolDispatcher {
    dispatcher: SharedToolDispatcher,
    allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
}

impl ProductionToolDispatcher {
    pub(crate) fn new(
        dispatcher: SharedToolDispatcher,
        allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
    ) -> Self {
        Self {
            dispatcher,
            allowed,
        }
    }
}

impl HeadlessToolDispatcher for ProductionToolDispatcher {
    fn dispatch(
        &mut self,
        call: HeadlessToolCall,
        cancellation: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<HeadlessToolOutput, HeadlessTurnPortError>> + Send
    {
        let allowed = self
            .allowed
            .lock()
            .map_err(|_| HeadlessTurnPortError::Tool)
            .and_then(|mut allowed| {
                let allowed_call = allowed.get(&call.id).ok_or(HeadlessTurnPortError::Tool)?;

                if allowed_call.name != call.name || allowed_call.input != call.input {
                    return Err(HeadlessTurnPortError::Tool);
                }

                allowed.remove(&call.id).ok_or(HeadlessTurnPortError::Tool)
            });
        let output = allowed
            .and_then(|allowed| {
                self.dispatcher
                    .lock()
                    .map_err(|_| HeadlessTurnPortError::Tool)?
                    .execute(
                        allowed.handle,
                        &ToolExecutionContext::from_headless_adapter(cancellation.adapter_view()),
                    )
                    .map_err(headless_tool_error)
            })
            .and_then(|output| {
                if let Some(terminal) = output.terminal() {
                    return Err(HeadlessTurnPortError::TaskTerminal(terminal));
                }
                let content = if output.is_error {
                    sanitized_native_tool_failure(&output.content)
                } else {
                    output.content
                };
                Ok(HeadlessToolOutput {
                    content,
                    is_error: output.is_error,
                })
            });
        std::future::ready(output)
    }
}

pub(crate) fn sanitized_native_tool_failure(content: &str) -> String {
    let Some((tool, reason)) = content.split_once(": ") else {
        return "tool execution failed".to_owned();
    };
    if !matches!(
        tool,
        "read"
            | "list"
            | "search"
            | "glob"
            | "grep"
            | "write"
            | "edit"
            | "bash"
            | "webfetch"
            | "file picker"
    ) {
        return "tool execution failed".to_owned();
    }

    let safe_reason = matches!(
        reason,
        "operation timed out" | "cancelled" | "invalid regex" | "invalid glob pattern"
    ) || [
        ("entry limit of ", " exceeded"),
        ("result limit of ", " exceeded"),
        ("traversal depth limit of ", " exceeded"),
    ]
    .into_iter()
    .any(|(prefix, suffix)| {
        reason
            .strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(suffix))
            .is_some_and(|value| {
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
            })
    });
    if safe_reason {
        format!("{tool}: {reason}")
    } else if reason.contains("outside project root")
        || reason.contains("traversal is not allowed")
        || reason.contains("must be a non-empty relative path")
    {
        format!("{tool}: path validation failed")
    } else {
        "tool execution failed".to_owned()
    }
}

pub(crate) struct TaskLaunchRequest<'a> {
    pub(crate) agent: &'a str,
    pub(crate) description: &'a str,
    pub(crate) background: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TuiSelectedTaskLaunch {
    NotSelected,
    Dispatched,
    Rejected(TaskLaunchOutcome),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TaskLaunchOutcome {
    Dispatched(HeadlessToolOutput),
    RejectedEmptyInput,
    RejectedCancelled,
    Denied,
}

pub(crate) struct AuthorizedNativeTaskRuntime<P> {
    pub(crate) gate: ProductionPermissionGate,
    pub(crate) resolver: ProductionPermissionResolver<P>,
    pub(crate) dispatcher: ProductionToolDispatcher,
    pub(crate) next_call_id: u64,
}

impl<P: PermissionPrompter> AuthorizedNativeTaskRuntime<P> {
    pub(crate) fn launch(
        &mut self,
        request: TaskLaunchRequest<'_>,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<TaskLaunchOutcome, HeadlessTurnPortError> {
        if request.agent.trim().is_empty() || request.description.trim().is_empty() {
            return Ok(TaskLaunchOutcome::RejectedEmptyInput);
        }
        if cancellation.is_cancelled() {
            return Ok(TaskLaunchOutcome::RejectedCancelled);
        }
        if cancellation.is_expired() {
            return Err(HeadlessTurnPortError::TimedOut);
        }

        self.next_call_id += 1;
        let call = HeadlessToolCall {
            id: format!("tui-task-{}", self.next_call_id),
            name: "native::task".into(),
            input: serde_json::json!({
                "agent": request.agent,
                "description": request.description,
                "background": request.background,
            })
            .to_string(),
        };
        let decision = poll_permission_port(self.gate.evaluate(&call, cancellation))?;
        let decision = if decision == PermissionDecision::Ask {
            poll_permission_port(self.resolver.resolve(&call, cancellation))?
        } else {
            decision
        };

        if decision == PermissionDecision::Deny {
            return Ok(TaskLaunchOutcome::Denied);
        }

        poll_permission_port(self.dispatcher.dispatch(call, cancellation))
            .map(TaskLaunchOutcome::Dispatched)
    }
}

pub(crate) fn launch_selected_tui_task(
    runtime: &mut ProductionTuiTaskRuntime,
    session: &Arc<Mutex<TuiSessionContext>>,
    description: &str,
    background: bool,
    cancellation: &HeadlessTurnCancellation,
) -> Result<TuiSelectedTaskLaunch, CliError> {
    let agent = session
        .lock()
        .map_err(|_| CliError::new(ExitStatus::Failure, "ui", "TUI session is unavailable"))?
        .selected_subagent
        .take();
    let Some(agent) = agent else {
        return Ok(TuiSelectedTaskLaunch::NotSelected);
    };

    match runtime.authorized.launch(
        TaskLaunchRequest {
            agent: &agent,
            description,
            background,
        },
        cancellation,
    ) {
        Ok(TaskLaunchOutcome::Dispatched(output)) if !output.is_error => {
            Ok(TuiSelectedTaskLaunch::Dispatched)
        }
        Ok(TaskLaunchOutcome::Dispatched(_)) if cancellation.is_cancelled() => {
            Err(CliError::runtime(HeadlessTurnError::Cancelled))
        }
        Ok(TaskLaunchOutcome::Dispatched(_)) if cancellation.is_expired() => {
            Err(CliError::runtime(HeadlessTurnError::TimedOut))
        }
        Ok(outcome) => Ok(TuiSelectedTaskLaunch::Rejected(outcome)),
        Err(HeadlessTurnPortError::Cancelled) => {
            Err(CliError::runtime(HeadlessTurnError::Cancelled))
        }
        Err(HeadlessTurnPortError::TimedOut) => Err(CliError::runtime(HeadlessTurnError::TimedOut)),
        Err(_) => Err(CliError::runtime(HeadlessTurnError::Tool)),
    }
}

/// A subagent armed for the user's next prompt must survive a turn the user never submitted, so a
/// runtime-scheduled turn leaves the arming in place and runs the main agent instead.
pub(crate) fn origin_launches_selected_subagent(origin: TuiSubmitOrigin) -> bool {
    match origin {
        TuiSubmitOrigin::User | TuiSubmitOrigin::Background => true,
        TuiSubmitOrigin::SubagentCompletion => false,
    }
}

pub(crate) fn selected_tui_task_skips_parent(
    launch: Result<TuiSelectedTaskLaunch, CliError>,
    lifecycle: &TuiTaskLifecycleBridge,
) -> Result<bool, CliError> {
    match launch? {
        TuiSelectedTaskLaunch::NotSelected => Ok(false),
        TuiSelectedTaskLaunch::Dispatched => {
            Ok(lifecycle.mode() == Some(TaskLaunchMode::Background))
        }
        TuiSelectedTaskLaunch::Rejected(outcome) => Err(selected_task_launch_error(outcome)),
    }
}

fn selected_task_launch_error(outcome: TaskLaunchOutcome) -> CliError {
    match outcome {
        TaskLaunchOutcome::RejectedEmptyInput => CliError::usage("subagent task is empty"),
        TaskLaunchOutcome::RejectedCancelled => CliError::runtime(HeadlessTurnError::Cancelled),
        TaskLaunchOutcome::Denied => CliError::runtime(HeadlessTurnError::Permission),
        TaskLaunchOutcome::Dispatched(_) => CliError::runtime(HeadlessTurnError::Tool),
    }
}

#[allow(dead_code)]
pub(crate) fn poll_permission_port<T>(
    future: impl std::future::Future<Output = Result<T, HeadlessTurnPortError>>,
) -> Result<T, HeadlessTurnPortError> {
    let mut future = std::pin::pin!(future);
    let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

    match future.as_mut().poll(context) {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => Err(HeadlessTurnPortError::Permission),
    }
}

fn headless_tool_error(error: agens_core::Error) -> HeadlessTurnPortError {
    match error {
        agens_core::Error::Cancelled => HeadlessTurnPortError::Cancelled,
        agens_core::Error::Tool(message) if message == "mcp operation timed out" => {
            HeadlessTurnPortError::TimedOut
        }
        agens_core::Error::Tool(_) | agens_core::Error::Extension(_) => HeadlessTurnPortError::Tool,
        _ => HeadlessTurnPortError::Tool,
    }
}
