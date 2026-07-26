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

#[cfg(test)]
mod tests {
    use agens_core::{
        PermissionMode, PermissionPattern, PermissionPolicy, PermissionRule, PermissionSession,
    };
    use agens_store::PermissionGrantStore;
    use agens_tools::{SkillCatalog, ToolDispatcher};
    use agens_tui::{
        BridgeTx, TuiExecutionEvent, TuiPermissionReply, TuiProviderOutcome, TuiRuntimeEvent,
        TuiSubagentEvent,
    };

    use super::*;
    use crate::CliError;
    use crate::permissions::{
        PermissionPromptAnswer, ProductionPermissionGate, ProductionPermissionResolver,
        ProductionPromptAuthorization, production_tui_permission_bridge,
    };
    use crate::test_support::{RecordingPrompt, tui_session_bootstrap, tui_session_directory};
    use crate::tools::runner::{ProductionTaskRunner, TuiTaskControls};
    use crate::tools::task::production_tui_task_runtime_with_runner;
    use crate::tui::agents::select_tui_subagent;
    use crate::tui::resume::ensure_active_tui_agent_runtime;
    use crate::tui::session::TuiSessionContext;
    use std::path::Path;

    #[test]
    fn u15_authorization_model_and_tui_launch_share_one_native_task_path() {
        struct RecordingTaskTool(Arc<std::sync::atomic::AtomicUsize>);

        impl DispatchTool for RecordingTaskTool {
            fn permission_target(
                &self,
                arguments: &serde_json::Value,
            ) -> Result<String, agens_core::Error> {
                arguments
                    .get("agent")
                    .and_then(serde_json::Value::as_str)
                    .filter(|agent| !agent.is_empty())
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| agens_core::Error::Tool("missing agent".into()))
            }

            fn execute(
                &mut self,
                _: &ToolExecutionContext,
                _: serde_json::Value,
            ) -> Result<ToolOutput, agens_core::Error> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ToolOutput::success("executed"))
            }
        }

        fn authorized_native_task_runtime<P: PermissionPrompter>(
            directory: &Path,
            policy: PermissionPolicy,
            dispatcher: SharedToolDispatcher,
            prompt: P,
        ) -> AuthorizedNativeTaskRuntime<P> {
            let grants = Arc::new(Mutex::new(Vec::new()));
            let allowed = Arc::new(Mutex::new(BTreeMap::new()));
            let prompts = Arc::new(Mutex::new(BTreeMap::new()));
            let gate = ProductionPermissionGate::new(
                policy.clone(),
                Arc::clone(&grants),
                PermissionSession::new(),
                "project".into(),
                Arc::clone(&dispatcher),
                Arc::clone(&allowed),
                Arc::clone(&prompts),
            );
            let resolver = ProductionPermissionResolver::new(
                prompt,
                PermissionGrantStore::open(directory).unwrap(),
                grants,
                prompts,
                ProductionPromptAuthorization {
                    policy,
                    session: PermissionSession::new(),
                    project: "project".into(),
                    dispatcher: Arc::clone(&dispatcher),
                    allowed: Arc::clone(&allowed),
                },
            );

            AuthorizedNativeTaskRuntime {
                gate,
                resolver,
                dispatcher: ProductionToolDispatcher::new(dispatcher, allowed),
                next_call_id: 0,
            }
        }

        fn launch_request<'a>(
            agent: &'a str,
            description: &'a str,
            background: bool,
        ) -> TaskLaunchRequest<'a> {
            TaskLaunchRequest {
                agent,
                description,
                background,
            }
        }

        let directory =
            std::env::temp_dir().join(format!("agens-u15-authorization-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
        dispatcher
            .lock()
            .unwrap()
            .register_native(
                "native::task",
                agens_core::ToolAccess::Write,
                RecordingTaskTool(Arc::clone(&executions)),
            )
            .unwrap();

        let ask_policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Ask,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let mut model = authorized_native_task_runtime(
            &directory,
            PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    PermissionDecision::Allow,
                    PermissionPattern::Exact("native::task".into()),
                    PermissionPattern::Any,
                )],
            ),
            Arc::clone(&dispatcher),
            RecordingPrompt {
                answers: vec![PermissionPromptAnswer::AllowOnce],
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );
        let mut tui = authorized_native_task_runtime(
            &directory,
            ask_policy,
            Arc::clone(&dispatcher),
            RecordingPrompt {
                answers: vec![PermissionPromptAnswer::AllowOnce],
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );
        let cancellation = HeadlessTurnCancellation::new();

        assert_eq!(
            model.launch(
                launch_request("reviewer", "model task", false),
                &cancellation
            ),
            Ok(TaskLaunchOutcome::Dispatched(HeadlessToolOutput::success(
                "executed"
            )))
        );
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            tui.launch(launch_request("reviewer", "TUI task", true), &cancellation),
            Ok(TaskLaunchOutcome::Dispatched(HeadlessToolOutput::success(
                "executed"
            )))
        );
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 2);

        let mut denied = authorized_native_task_runtime(
            &directory,
            PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    PermissionDecision::Deny,
                    PermissionPattern::Exact("native::task".into()),
                    PermissionPattern::Any,
                )],
            ),
            Arc::clone(&dispatcher),
            RecordingPrompt {
                answers: Vec::new(),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        );
        assert_eq!(
            denied.launch(launch_request("reviewer", "denied", false), &cancellation),
            Ok(TaskLaunchOutcome::Denied)
        );
        assert_eq!(
            tui.launch(launch_request("", "invalid", false), &cancellation),
            Ok(TaskLaunchOutcome::RejectedEmptyInput)
        );
        assert_eq!(
            tui.launch(launch_request("reviewer", "", false), &cancellation),
            Ok(TaskLaunchOutcome::RejectedEmptyInput)
        );
        cancellation.cancel();
        assert_eq!(
            tui.launch(
                launch_request("reviewer", "cancelled", false),
                &cancellation
            ),
            Ok(TaskLaunchOutcome::RejectedCancelled)
        );
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 2);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_runtime_scheduled_turn_never_consumes_the_armed_subagent() {
        let temporary = tui_session_directory("auto-turn-armed-subagent");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext::fresh()));
        assert_eq!(
            select_tui_subagent(&bootstrap, "reviewer", &session),
            Ok("Subagent: reviewer.".to_owned())
        );

        assert!(origin_launches_selected_subagent(TuiSubmitOrigin::User));
        assert!(origin_launches_selected_subagent(
            TuiSubmitOrigin::Background
        ));
        assert!(!origin_launches_selected_subagent(
            TuiSubmitOrigin::SubagentCompletion
        ));
        assert_eq!(
            session.lock().unwrap().selected_subagent.as_deref(),
            Some("reviewer")
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn u15_c1c_backgrounded_success_skips_the_parent_provider_and_history_path() {
        let temporary = tui_session_directory("selected-background-launch");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (events, receiver) = BridgeTx::bounded(8);
        let controls = TuiTaskControls::default();
        let lifecycle_bridge = TuiTaskLifecycleBridge::new(events, controls.clone());
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &SkillCatalog::default(),
            production_tui_permission_bridge().0,
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                bootstrap.project_root().unwrap().to_path_buf(),
                Arc::clone(&probe),
            )
            .with_lifecycle_bridge(lifecycle_bridge.clone()),
        )
        .unwrap();
        runtime.authorized.gate.policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let session = Arc::new(Mutex::new(TuiSessionContext {
            selected_subagent: Some("reviewer".into()),
            ..TuiSessionContext::fresh()
        }));
        ensure_active_tui_agent_runtime(&bootstrap, &session, &runtime.dispatcher).unwrap();
        let cancellation = HeadlessTurnCancellation::new();
        let parent_runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let next_event = |timeout| receiver.recv_timeout(timeout).unwrap().into_parts().1;
        let worker = std::thread::spawn({
            let session = Arc::clone(&session);
            let cancellation = cancellation.clone();
            let lifecycle_bridge = lifecycle_bridge.clone();
            let parent_runs = Arc::clone(&parent_runs);
            move || {
                let skips_parent = selected_tui_task_skips_parent(
                    launch_selected_tui_task(
                        &mut runtime,
                        &session,
                        "review task",
                        false,
                        &cancellation,
                    ),
                    &lifecycle_bridge,
                )?;
                if skips_parent {
                    Ok(TuiProviderOutcome::Backgrounded)
                } else {
                    parent_runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err(CliError::runtime(HeadlessTurnError::Provider))
                }
            }
        });
        assert_eq!(
            next_event(std::time::Duration::from_secs(1)),
            TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: TuiExecutionEvent::ForegroundStarted { id: 1 },
            }
        );
        assert_eq!(
            next_event(std::time::Duration::from_secs(1)),
            TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::started(
                1,
                "reviewer",
                "review task",
                agens_tui::TuiExecutionState::ForegroundRunning,
            ))
        );
        assert!(controls.transition_to_background(1));
        assert_eq!(
            next_event(std::time::Duration::from_secs(1)),
            TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: TuiExecutionEvent::Backgrounded { id: 1 },
            }
        );
        assert_eq!(worker.join().unwrap(), Ok(TuiProviderOutcome::Backgrounded));
        assert_eq!(
            next_event(std::time::Duration::from_secs(1)),
            TuiRuntimeEvent::TaskExecution {
                agent: "reviewer".into(),
                event: TuiExecutionEvent::Completed { id: 1 },
            }
        );
        let probe = probe.lock().unwrap();
        assert_eq!(probe.len(), 1);
        assert_eq!(parent_runs.load(std::sync::atomic::Ordering::SeqCst), 0);
        let session = session.lock().unwrap();
        assert!(session.messages.is_empty());
        assert_eq!(
            session
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn u15_a1b2_permission_cardinality_is_exact_for_allow_ask_and_deny() {
        fn policy(decision: PermissionDecision) -> PermissionPolicy {
            PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    decision,
                    PermissionPattern::Exact("native::task".into()),
                    PermissionPattern::Any,
                )],
            )
        }

        let temporary = tui_session_directory("selected-task-cardinality");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (bridge, requests) = production_tui_permission_bridge();
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &SkillCatalog::default(),
            bridge.clone(),
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                bootstrap.project_root().unwrap().to_path_buf(),
                Arc::clone(&probe),
            ),
        )
        .unwrap();
        let cancellation = HeadlessTurnCancellation::new();
        let selected = || {
            Arc::new(Mutex::new(TuiSessionContext {
                selected_subagent: Some("reviewer".into()),
                ..TuiSessionContext::fresh()
            }))
        };

        runtime.authorized.gate.policy = policy(PermissionDecision::Allow);
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "allow", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Dispatched)
        );
        assert_eq!(probe.lock().unwrap().len(), 1);
        assert!(requests.try_recv().is_err());

        let ask = policy(PermissionDecision::Ask);
        runtime.authorized.gate.policy = ask.clone();
        runtime.authorized.resolver.authorization.policy = ask;
        let reply_bridge = bridge.clone();
        let reply = std::thread::spawn(move || {
            let request = requests
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("ask should prompt once");
            reply_bridge.reply(request.id(), TuiPermissionReply::AllowOnce)
        });
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "ask", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Dispatched)
        );
        assert!(reply.join().unwrap());
        assert_eq!(probe.lock().unwrap().len(), 2);

        runtime.authorized.gate.policy = policy(PermissionDecision::Deny);
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "deny", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Rejected(TaskLaunchOutcome::Denied))
        );
        assert_eq!(probe.lock().unwrap().len(), 2);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn u15_a1b2_rejections_leave_the_concrete_runner_and_grants_unchanged() {
        let temporary = tui_session_directory("selected-task-rejections");
        let bootstrap = tui_session_bootstrap(
            &temporary,
            &[(
                "reviewer",
                "---\nname: reviewer\ndescription: reviewer\nmode: subagent\npermissions: []\n---\nReview work.\n",
            )],
        );
        let probe = Arc::new(Mutex::new(Vec::new()));
        let (bridge, requests) = production_tui_permission_bridge();
        let mut runtime = production_tui_task_runtime_with_runner(
            &bootstrap,
            &SkillCatalog::default(),
            bridge.clone(),
            ProductionTaskRunner::with_probe(
                bootstrap.clone(),
                bootstrap.project_root().unwrap().to_path_buf(),
                Arc::clone(&probe),
            ),
        )
        .unwrap();
        let selected = || {
            Arc::new(Mutex::new(TuiSessionContext {
                selected_subagent: Some("reviewer".into()),
                ..TuiSessionContext::fresh()
            }))
        };
        let cancellation = HeadlessTurnCancellation::new();

        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Rejected(
                TaskLaunchOutcome::RejectedEmptyInput
            ))
        );
        cancellation.cancel();
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "cancelled", false, &cancellation),
            Ok(TuiSelectedTaskLaunch::Rejected(
                TaskLaunchOutcome::RejectedCancelled
            ))
        );
        assert_eq!(probe.lock().unwrap().len(), 0);
        assert!(requests.try_recv().is_err());
        assert!(runtime.authorized.gate.grants.lock().unwrap().is_empty());

        runtime.authorized.gate.policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let unavailable = Arc::new(Mutex::new(TuiSessionContext {
            selected_subagent: Some("missing".into()),
            ..TuiSessionContext::fresh()
        }));
        assert_eq!(
            launch_selected_tui_task(
                &mut runtime,
                &unavailable,
                "missing",
                false,
                &HeadlessTurnCancellation::new(),
            ),
            Err(CliError::runtime(HeadlessTurnError::Tool))
        );
        assert_eq!(probe.lock().unwrap().len(), 0);

        let expired = HeadlessTurnCancellation::with_deadline(std::time::Duration::ZERO);
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "expired", false, &expired),
            Err(CliError::runtime(HeadlessTurnError::TimedOut))
        );
        assert_eq!(probe.lock().unwrap().len(), 0);

        let ask = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Ask,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        runtime.authorized.gate.policy = ask.clone();
        runtime.authorized.resolver.authorization.policy = ask;
        let active = HeadlessTurnCancellation::new();
        let reply_bridge = bridge.clone();
        let reply = std::thread::spawn(move || {
            [TuiPermissionReply::DenyOnce, TuiPermissionReply::Cancelled]
                .into_iter()
                .map(|answer| {
                    let request = requests
                        .recv_timeout(std::time::Duration::from_secs(1))
                        .expect("asked rejection should prompt once");
                    reply_bridge.reply(request.id(), answer)
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "deny once", false, &active),
            Ok(TuiSelectedTaskLaunch::Rejected(TaskLaunchOutcome::Denied))
        );
        assert_eq!(
            launch_selected_tui_task(&mut runtime, &selected(), "cancel ask", false, &active),
            Err(CliError::runtime(HeadlessTurnError::Cancelled))
        );
        assert!(reply.join().unwrap().into_iter().all(|replied| replied));
        assert_eq!(probe.lock().unwrap().len(), 0);
        assert!(runtime.authorized.gate.grants.lock().unwrap().is_empty());

        std::fs::remove_dir_all(temporary).unwrap();
    }
}
